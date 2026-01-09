// ==========================
// Imports
// ==========================

use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, header::RANGE};
use axum::{
    Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};

use clap::Parser;

use std::{
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tracing::info;

// ==========================
// CLI Input Struct
// ==========================

/// Wifi file server tool
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// root folder path of file server
    #[arg(short, long, default_value_t = env::current_dir().expect("Default current directory is invalid").to_string_lossy().into_owned())]
    folder_path: String,

    /// port number
    #[arg(short, long, default_value_t = 3000)]
    port_number: u16,
}

// ==========================
// Application State
// ==========================

#[derive(Clone)]
struct AppState {
    root: PathBuf,
}

// ==========================
// Application Entry Point
// ==========================

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let args = Args::parse();

    let root = PathBuf::from(&args.folder_path)
        .canonicalize()
        .expect("Invalid path");

    let state = AppState { root };

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/files/", get(root_handler))
        .route("/files/{*path}", get(file_handler))
        .with_state(state);

    let local_ip = local_ip_address::local_ip()
        .map(|i| i.to_string())
        .unwrap_or_else(|e| {
            eprintln!(
                "Warning: Could not determine local IP. Using 127.0.0.1. Error: {}",
                e
            );
            "127.0.0.1".to_string()
        });

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port_number));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "Error: Could not bind to port {}.\nDetails: {}",
                args.port_number, e
            );
            std::process::exit(1);
        }
    };

    println!("Server running at http://{}:{}", local_ip, args.port_number);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

// ==========================
// Route Handlers
// ==========================

async fn root_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    if state.root.is_file() {
        serve_file(&state.root, HeaderMap::new(), addr).await
    } else {
        render_directory(&state.root, "").await.into_response()
    }
}

async fn file_handler(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let safe_path = sanitize_path(&path);
    let full_path = state.root.join(safe_path);

    if !full_path.exists() {
        return StatusCode::NOT_FOUND.into_response();
    }

    if full_path.is_dir() {
        render_directory(&full_path, &path).await.into_response()
    } else {
        serve_file(&full_path, headers, addr).await
    }
}

// ==========================
// Path Utilities
// ==========================

fn sanitize_path(path: &str) -> PathBuf {
    Path::new(path)
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect()
}

// ==========================
// Directory Rendering
// ==========================

async fn render_directory(dir: &Path, base: &str) -> impl IntoResponse {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .expect("Read dir in render_directory threw an unexpected error");

    let mut html = String::new();
    html.push_str("<html><body>");
    html.push_str("<h1>Directory listing</h1><ul>");

    if !base.is_empty() {
        let parent = Path::new(base).parent().unwrap_or(Path::new(""));
        html.push_str(&format!(
            "<li><a href=\"/files/{}\">[..]</a></li>",
            parent.display()
        ));
    }

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, name)
        };

        if entry.path().is_dir() {
            html.push_str(&format!(
                "<li>📁 <a href=\"/files/{}\">{}</a></li>",
                path, name
            ));
        } else {
            html.push_str(&format!(
                "<li>📄 <a href=\"/files/{}\">{}</a></li>",
                path, name
            ));
        }
    }

    html.push_str("</ul></body></html>");
    Html(html)
}

// ==========================
// File Serving
// ==========================

async fn serve_file(path: &Path, headers: HeaderMap, client: SocketAddr) -> Response {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let metadata = match file.metadata().await {
        Ok(m) => m,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let file_size = metadata.len();
    let filename = path.file_name().unwrap().to_string_lossy();
    let mime = mime_guess::from_path(path).first_or_octet_stream();

    let mut response_headers = HeaderMap::new();
    response_headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    response_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref()).unwrap(),
    );
    response_headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"", filename)).unwrap(),
    );

    if let Some(range_header) = headers.get(RANGE) {
        let range_str = range_header.to_str().unwrap_or("");
        if let Some((start, end)) = parse_range(range_str, file_size) {
            info!(
                client = %client.ip(),
                file = %path.display(),
                range = %format!("{}-{}", start, end),
                "File download (partial)"
            );

            let length = end - start + 1;

            if file.seek(SeekFrom::Start(start)).await.is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }

            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);

            response_headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, file_size)).unwrap(),
            );
            response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));

            let mut res = Response::new(body);
            *res.status_mut() = StatusCode::PARTIAL_CONTENT;
            *res.headers_mut() = response_headers;
            return res;
        }
    }

    info!(
        client = %client.ip(),
        file = %path.display(),
        size = file_size,
        "File download (full)"
    );

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    response_headers.insert(header::CONTENT_LENGTH, HeaderValue::from(file_size));

    let mut res = Response::new(body);
    *res.headers_mut() = response_headers;
    res
}

// ==========================
// Range Parsing
// ==========================

fn parse_range(header: &str, size: u64) -> Option<(u64, u64)> {
    if !header.starts_with("bytes=") {
        return None;
    }

    let range = header.trim_start_matches("bytes=");
    let mut parts = range.split('-');

    let start = parts.next()?.parse::<u64>().ok()?;
    let end = match parts.next()?.parse::<u64>() {
        Ok(v) => v,
        Err(_) => size - 1,
    };

    if start <= end && end < size {
        Some((start, end))
    } else {
        None
    }
}
