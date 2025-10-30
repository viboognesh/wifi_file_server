use axum::{
    Router,
    body::Body,
    extract::State,
    http::{
        HeaderValue, StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::Response,
    routing::get,
};
use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio_util::io::ReaderStream;

const SERVER_PORT: u16 = 3000;

#[derive(Clone)]
struct AppState {
    file_path: PathBuf,
}

async fn download_handler(State(state): State<AppState>) -> Result<Response, StatusCode> {
    let path = Path::new(&state.file_path);
    let file = tokio::fs::File::open(path).await.map_err(|_| {
        eprintln!(
            "Error: File not found or failed to open: {:?}",
            state.file_path
        );
        StatusCode::NOT_FOUND
    })?;

    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    let stream = ReaderStream::new(file);

    let mut res = Response::builder();

    let headers = res.headers_mut().unwrap();

    let body = Body::from_stream(stream);

    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );

    let content_disposition = format!("attachment; filename=\"{}\"", file_name);
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::try_from(content_disposition)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );

    Ok(res.status(StatusCode::OK).body(body).unwrap())
}

fn get_local_ip() -> Result<String, Box<dyn std::error::Error>> {
    let ip = local_ip_address::local_ip()?;
    Ok(ip.to_string())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: cargo run -- <file_path>");
        std::process::exit(1);
    }
    let file_path = PathBuf::from(&args[1]);

    let file_name = file_path.file_name().unwrap().to_string_lossy().to_string();

    let local_ip = get_local_ip().unwrap_or_else(|e| {
        eprintln!(
            "Warning: Could not determine local IP. Using 127.0.0.1. Error: {}",
            e
        );
        "127.0.0.1".to_string()
    });

    println!("--- File Download Server Started ---");
    println!("File to serve: {:?}", file_path);
    println!("Server running on: http://{}:{}", local_ip, SERVER_PORT);
    println!(
        "-> DOWNLOAD URL: http://{}:{}/download",
        local_ip, SERVER_PORT
    );
    println!(
        "curl -o {} http://{}:{}/download",
        file_name, local_ip, SERVER_PORT
    );
    println!("------------------------------------");

    let app_state = AppState { file_path };
    let app = Router::new()
        .route("/download", get(download_handler))
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], SERVER_PORT)); // 0.0.0.0 binds to all interfaces

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
