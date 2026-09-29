mod aiff;
mod bpm;
mod decode;
mod library;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use library::Track;

const COOP: header::HeaderName = header::HeaderName::from_static("cross-origin-opener-policy");
const COEP: header::HeaderName = header::HeaderName::from_static("cross-origin-embedder-policy");

#[derive(Clone)]
struct App {
    tracks: Arc<RwLock<Arc<Vec<Track>>>>,
    dir: PathBuf,
    cache: PathBuf,
    tags: PathBuf,
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
        tags: PathBuf::from(env("MLM_TAGS", "mlm-tags.json")),
        tracks: Default::default(),
    };
    *app.tracks.write().unwrap() = Arc::new(library::load_cache(&app.dir, &app.cache));
    app.rescan();

    let router = Router::new()
        .route("/", get(index))
        .route("/egui", get(index)) // where the UI lived before it replaced the Lit one; installed PWAs may still open it
        .route("/mlm-ui.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_bytes!("../static/mlm-ui.js").as_slice()) }))
        .route("/mlm-ui_bg.wasm", get(|| async { ([(header::CONTENT_TYPE, "application/wasm")], include_bytes!("../static/mlm-ui_bg.wasm").as_slice()) }))
        .route("/manifest.json", get(|| async { ([(header::CONTENT_TYPE, "application/manifest+json")], include_bytes!("../ui/manifest.json").as_slice()) }))
        .route("/icon-192.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../ui/icon-192.png").as_slice()) }))
        .route("/icon-512.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../ui/icon-512.png").as_slice()) }))
        .route("/worklet.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_bytes!("../ui/worklet.js").as_slice()) }))
        .route("/api/tracks", get(tracks))
        .route("/api/tracks/{id}/file", get(file))
        .route("/api/tracks/{id}/pcm", get(pcm))
        .route("/api/tracks/{id}/analyze", post(analyze))
        .route("/api/tags", get(tags_get).put(tags_put))
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

/// User tags: `{ keys: { name: key }, tracks: { rel_path: [name] } }`. The client owns the document;
/// the server only validates the shape and stores it.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct Tags {
    keys: BTreeMap<String, String>,
    tracks: BTreeMap<String, Vec<String>>,
}

async fn tags_get(State(app): State<App>) -> Response {
    let body = std::fs::read(&app.tags).unwrap_or_else(|_| br#"{"keys":{},"tracks":{}}"#.to_vec());
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ponytail: whole-doc PUT, last write wins across devices; per-track POST if two clients tag at once
async fn tags_put(State(app): State<App>, body: Bytes) -> StatusCode {
    if serde_json::from_slice::<Tags>(&body).is_err() {
        return StatusCode::BAD_REQUEST;
    }
    let tmp = app.tags.with_extension("tmp"); // write + rename: a crash mid-write can't truncate user data
    match std::fs::write(&tmp, &body).and_then(|_| std::fs::rename(&tmp, &app.tags)) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The UI page. SharedArrayBuffer (the player's audio ring) needs a cross-origin isolated page.
async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html"), (COOP, "same-origin"), (COEP, "require-corp")], include_bytes!("../ui/index.html").as_slice())
}

/// Any format as a streamed WAV (the egui player's input): AIFF byte-swapped, the rest decoded.
async fn pcm(State(app): State<App>, Path(id): Path<u64>, req: Request) -> Response {
    let tracks = app.tracks();
    let Some(t) = tracks.iter().find(|t| t.id == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if t.is_aiff() {
        return match stream_aiff(&t.path, req.headers()).await {
            Ok(r) => r,
            Err(e) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, e.to_string()).into_response(),
        };
    }
    let body = Body::from_stream(decode::stream(t.path.clone(), t.duration_ms));
    ([(header::CONTENT_TYPE, "audio/wav")], body).into_response()
}

/// Detect the tempo, write it to the file's BPM tag, re-index. `{"bpm": 124}`, or 422 when no tempo is found.
async fn analyze(State(app): State<App>, Path(id): Path<u64>) -> Response {
    let Some(path) = app.tracks().iter().find(|t| t.id == id).map(|t| t.path.clone()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let r = tokio::task::spawn_blocking(move || -> std::io::Result<Option<u32>> {
        let Some(bpm) = bpm::detect(&path)? else { return Ok(None) };
        let bpm = bpm.round() as u32;
        bpm::write(&path, bpm)?;
        Ok(Some(bpm))
    })
    .await
    .unwrap();
    match r {
        Ok(Some(bpm)) => {
            app.rescan(); // only this file's mtime changed: one re-tag
            ([(header::CONTENT_TYPE, "application/json")], format!(r#"{{"bpm":{bpm}}}"#)).into_response()
        }
        Ok(None) => (StatusCode::UNPROCESSABLE_ENTITY, "no tempo found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// The original file, untouched (ctrl+d in the UI); the browser names it via the anchor's `download`.
async fn file(State(app): State<App>, Path(id): Path<u64>, req: Request) -> Response {
    match app.tracks().iter().find(|t| t.id == id) {
        Some(t) => ServeFile::new(&t.path).oneshot(req).await.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
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
