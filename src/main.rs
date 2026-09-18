mod aiff;
mod library;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use library::Track;

#[derive(Clone)]
struct App {
    tracks: Arc<RwLock<Arc<Vec<Track>>>>,
    dir: PathBuf,
    cache: PathBuf,
}

impl App {
    fn tracks(&self) -> Arc<Vec<Track>> {
        self.tracks.read().unwrap().clone()
    }
    fn rescan(&self) {
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let t = library::scan(&app.dir, &app.cache);
            *app.tracks.write().unwrap() = Arc::new(t);
        });
    }
}

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let app = App {
        dir: PathBuf::from(env("MLM_DIR", ".")),
        cache: PathBuf::from(env("MLM_CACHE", "mlm-index.json")),
        tracks: Default::default(),
    };
    *app.tracks.write().unwrap() = Arc::new(library::load_cache(&app.cache));
    app.rescan();

    let router = Router::new()
        .route("/", get(|| async { ([(header::CONTENT_TYPE, "text/html")], include_bytes!("../web/index.html").as_slice()) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_bytes!("../static/app.js").as_slice()) }))
        .route("/api/tracks", get(tracks))
        .route("/api/tracks/{id}/stream", get(stream))
        .route("/api/rescan", post(|State(app): State<App>| async move { app.rescan(); StatusCode::ACCEPTED }))
        .with_state(app);

    let addr = env("MLM_ADDR", "0.0.0.0:8080");
    eprintln!("listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, router).await.unwrap();
}

async fn tracks(State(app): State<App>) -> Response {
    let body = serde_json::to_vec(&*app.tracks()).unwrap();
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn stream(State(app): State<App>, Path(id): Path<u64>, req: Request) -> Response {
    let tracks = app.tracks();
    let Some(t) = tracks.iter().find(|t| t.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !t.is_aiff() {
        return ServeFile::new(&t.path).oneshot(req).await.into_response();
    }
    match stream_aiff(&t.path, req.headers()).await {
        Ok(r) => r,
        Err(e) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, e.to_string()).into_response(),
    }
}

async fn stream_aiff(path: &std::path::Path, headers: &HeaderMap) -> std::io::Result<Response> {
    let mut f = std::fs::File::open(path)?;
    let info = aiff::parse(&mut f)?;
    let total = info.wav_len();
    let (start, end, status) = match parse_range(headers, total) {
        Some((a, b)) => (a, b, StatusCode::PARTIAL_CONTENT),
        None => (0, total - 1, StatusCode::OK),
    };
    if start > end || end >= total {
        return Ok((StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{total}"))]).into_response());
    }
    let file = tokio::fs::File::from_std(f);
    let body = Body::from_stream(aiff::stream(file, info, start, end));
    let mut res = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, end - start + 1);
    if status == StatusCode::PARTIAL_CONTENT {
        res = res.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    Ok(res.body(body).unwrap())
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` -> inclusive (start, end). Single range only.
fn parse_range(headers: &HeaderMap, total: u64) -> Option<(u64, u64)> {
    let s = headers.get(header::RANGE)?.to_str().ok()?.strip_prefix("bytes=")?;
    let (a, b) = s.split_once('-')?;
    Some(match (a.parse::<u64>().ok(), b.parse::<u64>().ok()) {
        (Some(a), Some(b)) => (a, b.min(total - 1)),
        (Some(a), None) => (a, total - 1),
        (None, Some(n)) => (total.saturating_sub(n), total - 1),
        _ => return None,
    })
}
