mod aiff;
mod bpm;
mod decode;
mod library;
#[path = "../ui/src/tags.rs"]
#[allow(dead_code)] // shared with the UI; the server only needs Tags + Op::apply
mod tags;

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
    tags_lock: Arc<std::sync::Mutex<()>>, // serializes read-apply-write of the tags file
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
        tags_lock: Default::default(),
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
        .route("/icon-mono.png", get(|| async { ([(header::CONTENT_TYPE, "image/png")], include_bytes!("../ui/icon-mono.png").as_slice()) }))
        .route("/silence.wav", get(silence))
        .route("/sw.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")], include_bytes!("../ui/sw.js").as_slice()) }))
        .route("/worklet.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_bytes!("../ui/worklet.js").as_slice()) }))
        .route("/api/tracks", get(tracks))
        .route("/api/tracks/{id}/file", get(file))
        .route("/api/tracks/{id}/pcm", get(pcm))
        .route("/api/tracks/{id}/analyze", post(analyze))
        .route("/api/tracks/{id}/art", get(art))
        .route("/api/tags", get(tags_get))
        .route("/api/tags/ops", post(tags_ops))
        .route("/api/ytdl", post(ytdl))
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

async fn tags_get(State(app): State<App>) -> Response {
    let body = std::fs::read(&app.tags).unwrap_or_else(|_| br#"{"keys":{},"tracks":{}}"#.to_vec());
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Apply a batch of tag edits (`[{"op": "tag", ...}]`, see `tags::Op`) to the stored doc and return the result.
/// Clients send per-change ops, queued while offline, instead of whole documents: nobody overwrites anybody.
async fn tags_ops(State(app): State<App>, body: Bytes) -> Response {
    let Ok(ops) = serde_json::from_slice::<Vec<tags::Op>>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let _g = app.tags_lock.lock().unwrap();
    let mut doc: tags::Tags = std::fs::read(&app.tags).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    ops.iter().for_each(|op| doc.apply(op));
    let body = serde_json::to_vec(&doc).unwrap();
    let tmp = app.tags.with_extension("tmp"); // write + rename: a crash mid-write can't truncate user data
    match std::fs::write(&tmp, &body).and_then(|_| std::fs::rename(&tmp, &app.tags)) {
        Ok(()) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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

/// 10 s of silence the UI loops in an <audio> element while playing: Chrome on Android only shows the lock-screen
/// player (and iOS only keeps audio alive when locked) while a media element plays; Web Audio alone doesn't count.
async fn silence() -> impl IntoResponse {
    let data_len = 8000 * 2 * 10;
    let mut b = aiff::Info { channels: 1, bits: 16, rate: 8000, data_off: 0, data_len, little: true }.wav_header().to_vec();
    b.resize(b.len() + data_len as usize, 0);
    ([(header::CONTENT_TYPE, "audio/wav")], b)
}

/// Embedded cover art (front cover, else the first picture) for the lock screen; the app icon when there is none,
/// so the artwork URL the UI hands to the Media Session is always a valid image.
async fn art(State(app): State<App>, Path(id): Path<u64>) -> Response {
    let Some(path) = app.tracks().iter().find(|t| t.id == id).map(|t| t.path.clone()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let pic = tokio::task::spawn_blocking(move || {
        use lofty::file::TaggedFileExt;
        let f = lofty::read_from_path(&path).ok()?;
        let tag = f.primary_tag().or_else(|| f.first_tag())?;
        let pics = tag.pictures();
        let p = pics.iter().find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront).or(pics.first())?;
        Some((p.data().to_vec(), p.mime_type().map_or("image/jpeg", |m| m.as_str()).to_owned()))
    })
    .await
    .unwrap();
    let (body, mime) = pic.unwrap_or_else(|| (include_bytes!("../ui/icon-512.png").to_vec(), "image/png".into()));
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "max-age=86400".into())], body).into_response()
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

/// Only YouTube / YouTube Music links reach yt-dlp, which would fetch from a thousand other sites too.
fn yt_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !u.contains(char::is_whitespace) && matches!(host, "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be")
}

/// Download a YouTube link (the request body) as a 320 kbps MP3 with square cover art into `MLM_DIR/ytdl/` and
/// index it; answers with the new track's `rel`. Needs yt-dlp, ffmpeg and bun on PATH (`make ytdl-deps`).
async fn ytdl(State(app): State<App>, url: String) -> Response {
    let url = url.trim().to_owned();
    if !yt_url(&url) {
        return (StatusCode::BAD_REQUEST, "not a YouTube link").into_response();
    }
    // download + scan in one blocking task: it finishes (and the track is indexed) even if the phone hangs up
    let r = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let out = std::process::Command::new("yt-dlp")
            .args(["--js-runtimes", "bun", "--no-playlist", "--playlist-items", "1"])
            .arg("--no-mtime") // "added" is the download, not the upload
            .args(["-x", "--audio-format", "mp3", "--audio-quality", "320K"])
            .args(["--embed-metadata", "--embed-thumbnail", "--convert-thumbnails", "jpg"])
            .args(["--ppa", r#"ThumbnailsConvertor+ffmpeg_o:-c:v mjpeg -vf crop="'if(gt(ih,iw),iw,ih)':'if(gt(iw,ih),ih,iw)'""#])
            // intermediate files stay out of the library: a concurrent scan would index a half-written .m4a
            .arg("-P").arg(format!("home:{}", app.dir.join("ytdl").display()))
            .arg("-P").arg(format!("temp:{}", std::env::temp_dir().join("mlm-ytdl").display()))
            // "Artist - Track" when YouTube knows them, else the video title; never the video id.
            // ponytail: two videos with the same artist + title share a filename, the second counts as already
            // downloaded; add %(id)s back (or a counter) if that ever bites
            .args(["-o", "%(artist&{} - |)s%(track,title)s.%(ext)s"])
            .args(["--print", "after_move:filepath", "--", &url])
            .output()
            .map_err(|e| format!("yt-dlp: {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed").to_owned());
        }
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().trim());
        let name = path.file_name().ok_or("yt-dlp printed no file")?.to_string_lossy().into_owned();
        let t = library::scan(&app.dir, &app.cache);
        *app.tracks.write().unwrap() = Arc::new(t);
        Ok(format!("ytdl/{name}"))
    })
    .await
    .unwrap();
    match r {
        Ok(rel) => rel.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
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

#[cfg(test)]
mod tests {
    use super::yt_url;

    #[test]
    fn only_youtube_links_pass() {
        for ok in ["https://www.youtube.com/watch?v=abc", "https://youtube.com/watch?v=abc", "https://m.youtube.com/watch?v=abc",
                   "https://music.youtube.com/watch?v=abc&si=x", "https://youtu.be/abc?t=1"] {
            assert!(yt_url(ok), "{ok}");
        }
        for bad in ["http://www.youtube.com/watch?v=abc", "https://youtube.com.evil.com/watch?v=abc", "https://youtu.be@evil.com/x",
                    "https://evil.com/?u=https://youtu.be/abc", "--exec rm", "https://youtu.be/abc --exec rm", "youtu.be/abc", ""] {
            assert!(!yt_url(bad), "{bad}");
        }
    }
}
