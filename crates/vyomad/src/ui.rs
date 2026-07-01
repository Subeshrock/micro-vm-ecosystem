use rust_embed::RustEmbed;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    http::{header, Uri, StatusCode},
    body::Body,
};
use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "../../ui/dist"]
pub struct Assets;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn inject_meta(html: String, token: Option<&String>, version: &str) -> String {
    let mut tags = format!(
        r#"<meta name="vyoma-daemon-version" content="{}">"#,
        version
    );
    if let Some(t) = token {
        tags.push_str(&format!(
            r#"<meta name="vyoma-api-token" content="{}">"#,
            t
        ));
    }
    html.replace("</head>", &format!("{}</head>", tags))
}

pub async fn ui_handler(
    State(state): State<AppState>,
    uri: Uri,
) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    let is_html = path.ends_with(".html") || path == "index.html";

    match Assets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            
            let body = if is_html {
                let html = String::from_utf8_lossy(&content.data.to_vec()).into_owned();
                let modified = inject_meta(html, state.api_token.as_ref(), VERSION);
                Body::from(modified)
            } else {
                Body::from(content.data.to_vec())
            };
            
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(body)
                .unwrap()
        }
        None => {
            // If explicit file extension request failed, 404.
            if path.contains('.') {
                return (StatusCode::NOT_FOUND, "404 Not Found").into_response();
            }
            // Otherwise fallback to index.html (SPA)
             match Assets::get("index.html") {
                Some(content) => {
                    let html = String::from_utf8_lossy(&content.data.to_vec()).into_owned();
                    let modified = inject_meta(html, state.api_token.as_ref(), VERSION);
                    let body = Body::from(modified);
                    
                    Response::builder()
                        .header(header::CONTENT_TYPE, "text/html")
                        .body(body)
                        .unwrap()
                },
                None => (StatusCode::NOT_FOUND, "index.html missing").into_response(),
            }
        }
    }
}