use crate::AppState;
use simple_dns::{Packet, ResourceRecord, CLASS, TYPE, QTYPE};
use simple_dns::rdata::{A, RData};
use tokio::net::UdpSocket;
use std::sync::Arc;
use tracing::{info, warn, error, debug};
use std::net::Ipv4Addr;

pub async fn start_dns_server(state: AppState) {
    // Hardcoded gateway for now, matching default CNI config
    let gateway_ip = "172.16.0.1";
    let addr = format!("{}:53", gateway_ip);
    
    info!("Starting DNS Server on {}", addr);

    tokio::spawn(async move {
        // Initial delay to allow bridge to be ready (ADR-029 fix for WSL2 race condition)
        // The user reported DNS binding fails because it tries before bridge IP is ready
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        info!("DNS: Initial delay complete, attempting to bind...");
        
        let mut retries = 0;
        // Retry loop for binding
        let socket = loop {
            match UdpSocket::bind(&addr).await {
                Ok(s) => {
                    info!("DNS Server successfully bound to {}", addr);
                    break s;
                },
                Err(e) => {
                    retries += 1;
                    if retries <= 5 {
                        warn!("DNS bind failed (interface might not be ready): {}. Retrying in 2s...", e);
                    } else if retries <= 30 {
                        debug!("DNS bind failed (retry {}/30): {}", retries, e);
                    } else {
                        info!("DNS server could not bind after 30 attempts; continuing without DNS");
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        };

        let mut buf = [0u8; 512];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    match handle_conn(&buf[..len], &state).await {
                        Ok(response) => {
                            if !response.is_empty() {
                                let _ = socket.send_to(&response, src).await;
                            }
                        },
                        Err(e) => {
                            debug!("DNS Handle error: {}", e);
                        }
                    }
                },
                Err(e) => {
                    error!("DNS Receive Error: {}", e);
                }
            }
        }
    });
}

async fn handle_conn(buf: &[u8], state: &AppState) -> anyhow::Result<Vec<u8>> {
    let packet = Packet::parse(buf)?;
    
    // We only answer queries
    if packet.has_flags(simple_dns::PacketFlag::RESPONSE) {
        return Ok(vec![]);
    }

    let mut reply = Packet::new_reply(packet.id());
    
    // Process Questions
    for question in packet.questions {
        let name_str = question.qname.to_string();
        
        // Only handle A records for .vyoma domain or simple hostnames
        if question.qtype == QTYPE::TYPE(TYPE::A) && (name_str.ends_with(".vyoma") || !name_str.contains('.')) {
             
             let search_name = name_str.trim_end_matches(".vyoma").trim_end_matches('.').to_string();
             
             // 1. Get Candidates (Sync)
             let candidates = {
                 let vms = state.vms.lock().await;
                 if let Some(vm_arc) = vms.get(&search_name) {
                     vec![vm_arc.clone()]
                 } else {
                     vms.values().cloned().collect()
                 }
             };
             
             // 2. Filter (Async)
             let mut ips = Vec::new();
             for vm_arc in candidates {
                 let vm = vm_arc.lock().await;
                 if vm.id == search_name || vm.hostname.as_deref() == Some(search_name.as_str()) {
                      let ip_str = &vm.ip_address;
                      let ip_clean = ip_str.split('/').next().unwrap_or(ip_str);
                      if let Ok(ipv4) = ip_clean.parse::<Ipv4Addr>() {
                          ips.push(ipv4);
                      }
                 }
             }

             // Round-Robin / All IPs
             for ipv4 in ips {
                 let rdata = RData::A(A { address: ipv4.into() });
                 let rr = ResourceRecord::new(question.qname.clone(), CLASS::IN, 10, rdata);
                 reply.answers.push(rr);
             }
        }
    }
    
    if reply.answers.is_empty() {
         return forward_query(buf).await;
    }

    Ok(reply.build_bytes_vec().map_err(|e| anyhow::anyhow!("Build failed: {:?}", e))?)
}

async fn forward_query(buf: &[u8]) -> anyhow::Result<Vec<u8>> {
    // Simple forwarder to 1.1.1.1
    let upstream = "1.1.1.1:53";
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    
    socket.send_to(buf, upstream).await?;
    
    let mut resp_buf = [0u8; 512];
    let (len, _) = tokio::time::timeout(std::time::Duration::from_millis(500), socket.recv_from(&mut resp_buf))
        .await
        .map_err(|_| anyhow::anyhow!("Upstream timeout"))??;
        
    Ok(resp_buf[..len].to_vec())
}
