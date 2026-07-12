import { useState, useEffect, useRef } from 'react';
import * as d3 from 'd3';
import { useVmList } from '../../hooks/queries/useVmList';
import { useNetworks } from '../../hooks/queries/useNetworks';

interface TopologyNode {
  id: string;
  type: 'vm' | 'network';
  label: string;
  x?: number;
  y?: number;
}

interface TopologyLink {
  source: string;
  target: string;
}

export function TopologyPage() {
  const svgRef = useRef<SVGSVGElement>(null);
  const { data: vmsData } = useVmList();
  const { data: netsData } = useNetworks();
  const [nodes, setNodes] = useState<TopologyNode[]>([]);
  const [links, setLinks] = useState<TopologyLink[]>([]);

  useEffect(() => {
    if (!vmsData?.vms || !netsData) return;

    const topoNodes: TopologyNode[] = [
      ...vmsData.vms.map((v) => ({ id: v.id, type: 'vm' as const, label: v.labels['vyoma.service'] || v.id.slice(0, 8) })),
      ...netsData.map((n) => ({ id: n, type: 'network' as const, label: n })),
    ];
    const topoLinks: TopologyLink[] = vmsData.vms.map((v) => ({ source: v.id, target: v.labels['network'] || 'default' }));

    setNodes(topoNodes);
    setLinks(topoLinks);
  }, [vmsData, netsData]);

  useEffect(() => {
    if (!svgRef.current || nodes.length === 0) return;

    const svg = d3.select(svgRef.current);
    svg.selectAll('*').remove();

    const width = svgRef.current.clientWidth || 800;
    const height = svgRef.current.clientHeight || 500;

    const simulation = d3.forceSimulation(nodes as any)
      .force('link', d3.forceLink(links).id((d: any) => d.id).distance(120))
      .force('charge', d3.forceManyBody().strength(-400))
      .force('center', d3.forceCenter(width / 2, height / 2))
      .force('collision', d3.forceCollide().radius(40));

    const link = svg.append('g')
      .selectAll('line')
      .data(links)
      .enter()
      .append('line')
      .attr('stroke', '#334155')
      .attr('stroke-width', 2);

    const node = svg.append('g')
      .selectAll('g')
      .data(nodes)
      .enter()
      .append('g')
      .call(d3.drag<any, any>()
        .on('start', (event, d) => {
          if (!event.active) simulation.alphaTarget(0.3).restart();
          d.fx = d.x;
          d.fy = d.y;
        })
        .on('drag', (event, d) => {
          d.fx = event.x;
          d.fy = event.y;
        })
        .on('end', (event, d) => {
          if (!event.active) simulation.alphaTarget(0);
          delete d.fx;
          delete d.fy;
        })
      );

    node.append('circle')
      .attr('r', 24)
      .attr('fill', (d) => d.type === 'vm' ? '#2fd9f4' : '#f6be39') // Use Vyoma accent/secondary
      .attr('stroke', '#1e293b')
      .attr('stroke-width', 2);

    node.append('text')
      .text((d) => d.label.slice(0, 8))
      .attr('text-anchor', 'middle')
      .attr('dy', 40)
      .attr('fill', '#64748b')
      .attr('font-size', '11px');

    simulation.on('tick', () => {
      link
        .attr('x1', (d: any) => d.source.x)
        .attr('y1', (d: any) => d.source.y)
        .attr('x2', (d: any) => d.target.x)
        .attr('y2', (d: any) => d.target.y);

      node.attr('transform', (d: any) => `translate(${d.x},${d.y})`);
    });
  }, [nodes, links]);

  return (
    <div className="p-8 max-w-6xl mx-auto h-[600px] flex flex-col">
      <header className="mb-4">
        <h2 className="text-2xl font-bold text-foreground mb-1">Network Topology</h2>
        <p className="text-sm text-muted-foreground">Interactive graph of VMs and networks. Drag nodes to reposition.</p>
      </header>
      <div className="flex-1 bg-card rounded-xl border border-border overflow-hidden relative">
        {nodes.length === 0 ? (
          <div className="absolute inset-0 flex items-center justify-center text-muted-foreground">
            No topology data. Run some VMs first.
          </div>
        ) : (
          <svg ref={svgRef} className="w-full h-full" />
        )}
      </div>
    </div>
  );
}
