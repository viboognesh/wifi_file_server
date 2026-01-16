// ==========================
// Imports
// ==========================
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Path as AxumPath, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{self, RANGE},
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::{
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
};
use tokio_util::io::ReaderStream;
use tracing::info;
use unicase::UniCase;
use uuid::Uuid;

// ==========================
// CLI Input Struct
// ==========================
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Root folder path of file server
    #[arg(short, long, default_value_t = env::current_dir().expect("Invalid current directory").to_string_lossy().into_owned())]
    folder_path: String,

    /// Port number
    #[arg(short, long, default_value_t = 3000)]
    port_number: u16,

    /// Parallel downloads
    #[arg(short = 'n', long, default_value_t = 10)]
    parallel_downloads: u16,
}

// ==========================
// Application State
// ==========================
#[derive(Clone)]
struct AppState {
    host: String,
    parallel_downloads: u16,
    root: PathBuf,
    download_cache: Arc<Mutex<LruCache<String, Vec<String>>>>,
}

#[derive(Deserialize)]
struct SelectionRequest {
    files: Vec<String>,
    dirs: Vec<String>,
}

#[derive(Serialize)]
struct SelectionResponse {
    id: String,
}

// ==========================
// Entry Point
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

    let local_ip = local_ip_address::local_ip()
        .map(|i| i.to_string())
        .unwrap_or_else(|e| {
            eprintln!(
                "Warning: Could not determine local IP. Using 127.0.0.1. Error: {}",
                e
            );
            "127.0.0.1".to_string()
        });

    let host = format!("http://{}:{}", local_ip, args.port_number);

    let state = AppState {
        host,
        root,
        parallel_downloads: args.parallel_downloads,
        download_cache: Arc::new(Mutex::new(LruCache::new(
            std::num::NonZeroUsize::new(100).unwrap(),
        ))),
    };

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/files/", get(root_handler))
        .route("/files/{*path}", get(file_handler))
        .route("/register-selection", post(register_selection))
        .route("/config/{id}", get(config_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port_number));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "Error: Could not bind to port {}. Details: {}",
                args.port_number, e
            );
            std::process::exit(1);
        });

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
async fn register_selection(
    State(state): State<AppState>,
    Json(payload): Json<SelectionRequest>,
) -> impl IntoResponse {
    let SelectionRequest { files, dirs } = payload;

    // Expand directories server-side
    let mut all_files = files;
    all_files.extend(expand_dirs(&state.root, dirs).await);

    all_files.sort();
    all_files.dedup();

    let id = Uuid::new_v4().to_string();
    let mut cache = state.download_cache.lock().unwrap();
    cache.put(id.clone(), all_files);

    Json(SelectionResponse { id })
}

async fn config_handler(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    let files = {
        let mut cache = state.download_cache.lock().unwrap();
        cache.get(&id).cloned()
    };

    if let Some(file_list) = files {
        let mut config = format!(
            "globoff\ncontinue-at = -\nparallel\nparallel-max = {}\nparallel-immediate\nprogress-meter\n",
            state.parallel_downloads
        );

        for path in file_list {
            let escaped = escape_curl_config_value(&path);
            config.push_str(&format!(
                "url = \"{}/files/{}\"\noutput = \"{}\"\n\n",
                state.host, escaped, escaped
            ));
        }

        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain")],
            config,
        )
            .into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

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
    let full_path = state.root.join(sanitize_path(&path));

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
// Utilities
// ==========================
fn sanitize_path(path: &str) -> PathBuf {
    Path::new(path)
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect()
}

pub fn escape_curl_config_value(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

// ==========================
// Directory Rendering
// ==========================
pub async fn render_directory(dir: &Path, base: &str) -> impl IntoResponse {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .expect("Failed to read directory");
    let mut items: Vec<DirItem> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().await.is_ok_and(|ft| ft.is_dir());
        let path = if base.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", base, &name)
        };
        let link_path = format!("/files/{}", path);

        items.push(DirItem {
            name,
            path,
            link_path,
            is_dir,
        });
    }

    items.sort_by(|a, b| UniCase::new(&a.name).cmp(&UniCase::new(&b.name)));

    // Parent directory info
    let parent = if !base.is_empty() {
        Some(
            Path::new(base)
                .parent()
                .unwrap_or(Path::new(""))
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    };

    Html(generate_page_html_template(parent, items))
}

struct DirItem {
    name: String,
    path: String,
    link_path: String,
    is_dir: bool,
}

impl DirItem {
    fn to_html(&self) -> String {
        let icon = if self.is_dir { "📁" } else { "📄" };
        format!(
            r#"<div class="item-row">
                <div class="checkbox-wrapper">
                    <input type="checkbox" class="item-check" data-path="{path}" data-is-dir="{is_dir}">
                </div>
                <span class="icon">{icon}</span>
                <a class="file-link" href="{link_path}">{name}</a>
            </div>"#,
            path = self.path,
            is_dir = self.is_dir,
            icon = icon,
            link_path = self.link_path,
            name = self.name
        )
    }
}

fn generate_page_html_template(parent: Option<String>, items: Vec<DirItem>) -> String {
    let mut content = String::new();

    // Parent directory row
    if let Some(parent_path) = parent {
        content.push_str(&format!(
            r#"<div class="item-row parent-row">
                <a href="/files/{}" style="text-decoration:none; color:#666;">⤴ .. (Parent Directory)</a>
            </div>"#,
            parent_path
        ));
    }

    // File and folder rows
    for item in items {
        content.push_str(&item.to_html());
    }

    // Inject into HTML template
    let html_template = include_str!("../index.html");
    html_template.replace("{{CONTENT}}", &content)
}

// ==========================
// File Serving
// ==========================
async fn serve_file(path: &Path, headers: HeaderMap, client: SocketAddr) -> Response {
    let mut file = match File::open(path).await {
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
        if let Some((start, end)) = parse_range(range_header.to_str().unwrap_or(""), file_size) {
            info!(
                client = %client.ip(),
                file = %path.display(),
                range = %format!("{}-{}", start, end),
                "File download (partial)"
            );

            let length = end - start + 1;
            file.seek(SeekFrom::Start(start)).await.ok();

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
    let mut parts = header.trim_start_matches("bytes=").split('-');
    let start = parts.next()?.parse::<u64>().ok()?;
    let end = parts.next()?.parse::<u64>().unwrap_or(size - 1);
    if start <= end && end < size {
        Some((start, end))
    } else {
        None
    }
}

// ==========================
// Directory Expansion
// ==========================
async fn expand_dirs(root: &Path, dirs: Vec<String>) -> Vec<String> {
    let mut result = Vec::new();
    let mut stack: Vec<PathBuf> = dirs
        .into_iter()
        .map(|d| root.join(sanitize_path(&d)))
        .collect();

    while let Some(path) = stack.pop() {
        let mut rd = match fs::read_dir(&path).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            let p = entry.path();
            if entry.file_type().await.map(|f| f.is_dir()).unwrap_or(false) {
                stack.push(p);
            } else if let Ok(rel) = p.strip_prefix(root) {
                result.push(rel.to_string_lossy().to_string());
            }
        }
    }

    result.sort();
    result
}
