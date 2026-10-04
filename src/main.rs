mod aiff;
mod bpm;
mod decode;
mod library;
#[path = "../ui/src/tags.rs"]
#[allow(dead_code)] // shared with the UI; the server only needs Tags + Op::apply
mod tags;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

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

const MAX_JOBS: usize = 5; // downloads + analyses running at once; the rest wait their turn

/// A download or an analysis: queued, then run on a blocking thread, MAX_JOBS at a time. UIs poll `/api/jobs`.
#[derive(Clone, serde::Serialize)]
struct Job {
    id: u64,
    #[serde(skip)]
    key: String, // what it works on (a link, a file): asking again while it's pending adds nothing
    name: String,
    state: &'static str, // queued | running | done | failed
    text: String,        // the stage while running, then the result or the error
    progress: f32,       // 0..1
    #[serde(skip)]
    end: Option<Instant>,
}

/// A running job's handle on its own entry in the list.
struct JobRef {
    jobs: Arc<Mutex<Vec<Job>>>,
    id: u64,
}

impl JobRef {
    fn set(&self, f: impl FnOnce(&mut Job)) {
        if let Some(j) = self.jobs.lock().unwrap().iter_mut().find(|j| j.id == self.id) {
            f(j);
        }
    }
}

#[derive(Clone)]
struct App {
    tracks: Arc<RwLock<Arc<Vec<Track>>>>,
    dir: PathBuf,
    cache: PathBuf,
    tags: PathBuf,
    tags_lock: Arc<Mutex<()>>, // serializes read-apply-write of the tags file
    scan_lock: Arc<Mutex<()>>, // one scan at a time: they all rewrite the cache file
    jobs: Arc<Mutex<Vec<Job>>>,
    next_job: Arc<AtomicU64>,
    slots: Arc<tokio::sync::Semaphore>, // MAX_JOBS permits, handed out in arrival order
}

impl App {
    fn new(dir: PathBuf, cache: PathBuf, tags: PathBuf) -> Self {
        Self {
            dir, cache, tags, tracks: Default::default(), tags_lock: Default::default(), scan_lock: Default::default(),
            jobs: Default::default(), next_job: Default::default(), slots: Arc::new(tokio::sync::Semaphore::new(MAX_JOBS)),
        }
    }
    fn tracks(&self) -> Arc<Vec<Track>> {
        self.tracks.read().unwrap().clone()
    }
    /// Re-index, blocking until the new list is being served.
    fn scan(&self) {
        let _g = self.scan_lock.lock().unwrap();
        let t = library::scan(&self.dir, &self.cache);
        *self.tracks.write().unwrap() = Arc::new(t);
    }
    fn rescan(&self) {
        let app = self.clone();
        tokio::task::spawn_blocking(move || app.scan());
    }

    /// Queue `work` as a job and answer with the job list. `work` reports its stage and progress through the
    /// `JobRef` and returns the text the job ends with. It runs to the end even if the client hangs up.
    fn job(&self, key: String, name: String, work: impl FnOnce(&JobRef) -> Result<String, String> + Send + 'static) -> Response {
        let mut jobs = self.jobs.lock().unwrap();
        if !jobs.iter().any(|j| j.key == key && j.end.is_none()) {
            let id = self.next_job.fetch_add(1, Ordering::Relaxed);
            jobs.push(Job { id, key, name, state: "queued", text: "queued".into(), progress: 0.0, end: None });
            let app = self.clone();
            tokio::spawn(async move {
                let _slot = app.slots.acquire().await.unwrap();
                let (me, end) = (JobRef { jobs: app.jobs.clone(), id }, JobRef { jobs: app.jobs.clone(), id });
                me.set(|j| (j.state, j.text) = ("running", "starting".into()));
                let r = tokio::task::spawn_blocking(move || work(&me)).await.unwrap_or_else(|e| Err(e.to_string()));
                end.set(|j| {
                    (j.state, j.text) = match r {
                        Ok(text) => ("done", text),
                        Err(e) => ("failed", e),
                    };
                    (j.progress, j.end) = (1.0, Some(Instant::now()));
                });
            });
        }
        drop(jobs);
        self.jobs_json()
    }

    /// The job list. Finished jobs stay on it for a few seconds (failures longer) so polling UIs see how they ended.
    fn jobs_json(&self) -> Response {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|j| j.end.is_none_or(|t| t.elapsed().as_secs() < if j.state == "failed" { 30 } else { 5 }));
        ([(header::CONTENT_TYPE, "application/json")], serde_json::to_vec(&*jobs).unwrap()).into_response()
    }
}

#[tokio::main]
async fn main() {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let app = App::new(
        PathBuf::from(env("MLM_DIR", ".")),
        PathBuf::from(env("MLM_CACHE", "mlm-index.json")),
        PathBuf::from(env("MLM_TAGS", "mlm-tags.json")),
    );
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
        .route("/api/jobs", get(|State(app): State<App>| async move { app.jobs_json() }))
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

/// A job: detect the tempo, write it to the file's BPM tag, re-index. Ends as "124 BPM", or fails with "no tempo found".
async fn analyze(State(app): State<App>, Path(id): Path<u64>) -> Response {
    let Some(t) = app.tracks().iter().find(|t| t.id == id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = t.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let a = app.clone();
    app.job(t.path.to_string_lossy().into_owned(), name, move |job| {
        job.set(|j| j.text = "analyzing".into());
        let secs = (t.duration_ms / 1000).clamp(1, bpm::MAX_SECS) as f32; // what detect will decode
        let bpm = bpm::detect(&t.path, |s| job.set(|j| j.progress = (s as f32 / secs).min(1.0) * 0.9)).map_err(|e| e.to_string())?;
        let bpm = bpm.ok_or("no tempo found")?.round() as u32;
        job.set(|j| j.text = "writing the tag".into());
        bpm::write(&t.path, bpm).map_err(|e| e.to_string())?;
        a.scan(); // only this file's mtime changed: one re-tag
        Ok(format!("{bpm} BPM"))
    })
}

/// Only YouTube / YouTube Music links reach yt-dlp, which would fetch from a thousand other sites too.
fn yt_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !u.contains(char::is_whitespace) && matches!(host, "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be")
}

const CONVERTING: &str = "converting to MP3"; // the stage `ytdl` fills in from ffmpeg's report

/// What a line of yt-dlp's output says about a download, as (progress 0..1, the stage to show, the track's title
/// and length in seconds once known). Of the bar, resolving the link is the first fifth, the download runs to the
/// middle, the MP3 conversion (`ffmpeg_secs`) to nine tenths, tagging is the rest. Resolving is a varying number
/// of steps with no size of their own (None): `ytdl` moves each part of the way to the fifth.
fn ytdl_progress(line: &str) -> Option<(Option<f32>, String, Option<(&str, f32)>)> {
    let l = line.trim();
    if let Some(l) = l.strip_prefix("mlm dl ") {
        // tab-separated: length, title, and yt-dlp's own progress text, "24.5% of 2.03MiB at 7.45MiB/s ETA 00:00"
        let mut part = l.split('\t');
        let (secs, title) = (part.next()?.parse().unwrap_or(0.0), part.next()?.trim()); // "NA" seconds: a live stream
        let shown = part.next()?.split_whitespace().collect::<Vec<_>>().join(" ");
        let pct: f32 = shown.split('%').next()?.parse().ok()?; // no percentage when the size isn't known: no news
        return Some((Some(0.2 + 0.3 * pct.clamp(0.0, 100.0) / 100.0), format!("downloading {shown}"), Some((title, secs))));
    }
    if let Some(pp) = l.strip_prefix("mlm pp ") {
        let (p, stage) = match pp {
            "ThumbnailsConvertor started" => (None, "preparing the artwork"),
            "ExtractAudio started" => (Some(0.5), CONVERTING),
            "Metadata started" => (Some(0.9), "adding tags"),
            "EmbedThumbnail started" => (Some(0.94), "adding artwork"),
            "MoveFiles started" => (Some(0.98), "moving into the library"),
            _ => return None,
        };
        return Some((p, stage.into(), None));
    }
    // resolving the link: "[youtube] <id>: Downloading webpage", "[info] <id>: Downloading 1 format(s): 251", ...
    if !["[youtube", "[info]", "[jsc"].iter().any(|tag| l.starts_with(tag)) {
        return None;
    }
    let msg = l.split_once("] ")?.1;
    let msg = msg.split_once(": ").filter(|(id, _)| !id.contains(' ')).map_or(msg, |(_, m)| m); // without the video id
    Some((None, msg.split(':').next()?.trim().trim_end_matches(" to").into(), None)) // without the URL or path after a colon
}

/// How many seconds of audio ffmpeg has converted, from the tail of its `-progress` report.
fn ffmpeg_secs(report: &std::path::Path) -> Option<f32> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(report).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(512))).ok()?; // a block of the report is some 250 bytes
    let mut tail = String::new();
    f.read_to_string(&mut tail).ok()?;
    tail.lines().rev().find_map(|l| l.strip_prefix("out_time_us=")?.parse::<f32>().ok()).map(|us| us / 1e6)
}

/// A job: download a YouTube link (the request body) as a 320 kbps MP3 with square cover art into `MLM_DIR/ytdl/`
/// and index it. Needs yt-dlp, ffmpeg and bun on PATH (`make ytdl-deps`).
async fn ytdl(State(app): State<App>, url: String) -> Response {
    use std::io::{BufRead, Read};
    use std::process::Stdio;
    let url = url.trim().to_owned();
    if !yt_url(&url) {
        return (StatusCode::BAD_REQUEST, "not a YouTube link").into_response();
    }
    let a = app.clone();
    app.job(url.clone(), url.clone(), move |job| {
        let app = a;
        let tmp = std::env::temp_dir().join("mlm-ytdl");
        let report = tmp.join(format!("{}.progress", job.id)); // ffmpeg's, while it converts
        let run = || -> Result<(), String> {
            let mut child = std::process::Command::new("yt-dlp")
                .args(["--js-runtimes", "bun", "--no-playlist", "--playlist-items", "1"])
                .arg("--no-mtime") // "added" is the download, not the upload
                .args(["-x", "--audio-format", "mp3", "--audio-quality", "320K"])
                .args(["--embed-metadata", "--embed-thumbnail", "--convert-thumbnails", "jpg"])
                .args(["--ppa", r#"ThumbnailsConvertor+ffmpeg_o:-c:v mjpeg -vf crop="'if(gt(ih,iw),iw,ih)':'if(gt(iw,ih),ih,iw)'""#])
                // intermediate files stay out of the library: a concurrent scan would index a half-written .m4a
                .arg("-P").arg(format!("home:{}", app.dir.join("ytdl").display()))
                .arg("-P").arg(format!("temp:{}", tmp.display()))
                // yt-dlp shows nothing while ffmpeg converts, the slow part: have ffmpeg report how far it is, 10x a second
                .arg("--ppa").arg(format!("ExtractAudio+ffmpeg:-progress '{}' -stats_period 0.1", report.display()))
                // "Artist - Track" when YouTube knows them, else the video title; never the video id.
                // ponytail: two videos with the same artist + title share a filename, the second counts as already
                // downloaded; add %(id)s back (or a counter) if that ever bites
                .args(["-o", "%(artist&{} - |)s%(track,title)s.%(ext)s"])
                // what yt-dlp shows, a line at a time, for `ytdl_progress`; the download and post-processing lines in a
                // shape that's easy to tell apart and carries the title
                .args(["--newline", "--progress-template", "download:mlm dl %(info.duration)s\t%(info.artist&{} - |)s%(info.track,info.title)s\t%(progress._default_template)s"])
                .args(["--progress-template", "postprocess:mlm pp %(progress.postprocessor)s %(progress.status)s"])
                .args(["--", &url])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("yt-dlp: {e}"))?;
            let mut stderr = child.stderr.take().unwrap();
            let err = std::thread::spawn(move || {
                let mut s = String::new(); // drained on the side: a full stderr pipe would stall yt-dlp
                let _ = stderr.read_to_string(&mut s);
                s
            });
            let (over, secs) = (std::sync::atomic::AtomicBool::new(false), std::sync::atomic::AtomicU32::new(0)); // the track's length, f32 bits
            std::thread::scope(|s| {
                // the conversion, from ffmpeg's report: the only stage yt-dlp's own lines don't cover
                s.spawn(|| {
                    while !over.load(Ordering::Relaxed) {
                        let len = f32::from_bits(secs.load(Ordering::Relaxed));
                        if let Some(at) = ffmpeg_secs(&report).filter(|_| len > 0.0) {
                            let part = (at / len).min(1.0);
                            job.set(|j| {
                                if j.text.starts_with(CONVERTING) && 0.5 + 0.4 * part > j.progress {
                                    (j.progress, j.text) = (0.5 + 0.4 * part, format!("{CONVERTING} {:.0}%", part * 100.0));
                                }
                            });
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                });
                for line in std::io::BufReader::new(child.stdout.take().unwrap()).split(b'\n').map_while(Result::ok) {
                    if let Some((p, stage, track)) = ytdl_progress(&String::from_utf8_lossy(&line)) {
                        job.set(|j| {
                            // a step in resolving the link: part of the way to where the download starts
                            j.progress = j.progress.max(p.unwrap_or(j.progress + (0.2 - j.progress) * 0.15));
                            j.text = stage;
                            if let Some((title, len)) = track {
                                j.name = title.into();
                                secs.store(len.to_bits(), Ordering::Relaxed);
                            }
                        });
                    }
                }
                over.store(true, Ordering::Relaxed);
            });
            let _ = std::fs::remove_file(&report);
            let (ok, err) = (child.wait().is_ok_and(|s| s.success()), err.join().unwrap_or_default());
            if !ok {
                return Err(err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed").to_owned());
            }
            Ok(())
        };
        // now and then YouTube refuses the audio (403) of a link it has just resolved, and yt-dlp gives up at once;
        // the next try usually gets it
        let mut r = run();
        for _ in 0..2 {
            if !r.as_ref().is_err_and(|e| e.contains("HTTP Error 403")) {
                break;
            }
            job.set(|j| (j.progress, j.text) = (0.0, "YouTube refused the download, trying again".into()));
            r = run();
        }
        r?;
        app.scan();
        Ok("downloaded".into())
    })
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
    use super::*;

    #[test]
    fn reads_ytdl_progress_lines() {
        let at = |line| ytdl_progress(line).map(|(p, stage, track)| (p, stage, track));
        let step = |text: &str| Some((None, text.to_string(), None));
        // resolving the link: yt-dlp's own words, without the video id, the URL, the path
        assert_eq!(at("[youtube] Extracting URL: https://music.youtube.com/watch?v=abc"), step("Extracting URL"));
        assert_eq!(at("[youtube] abc: Downloading webpage"), step("Downloading webpage"));
        assert_eq!(at("[youtube] abc: Downloading visionos player API JSON"), step("Downloading visionos player API JSON"));
        assert_eq!(at("[info] abc: Downloading 1 format(s): 251"), step("Downloading 1 format(s)"));
        assert_eq!(at("[info] Writing video thumbnail 41 to: /tmp/mlm-ytdl/A - B.webp"), step("Writing video thumbnail 41"));
        assert_eq!(at("mlm pp ThumbnailsConvertor started"), step("preparing the artwork"));
        // the download: yt-dlp's progress text as it shows it, the percentage spread over 20%..50% of the bar
        assert_eq!(
            at("mlm dl 125\tA - B\t 24.5% of    2.03MiB at    7.45MiB/s ETA 00:00"),
            Some((Some(0.2 + 0.3 * 24.5 / 100.0), "downloading 24.5% of 2.03MiB at 7.45MiB/s ETA 00:00".into(), Some(("A - B", 125.0))))
        );
        assert_eq!(
            at("mlm dl NA\tLive set\t100% of    2.03MiB in 00:00:00 at 8.48MiB/s\r"),
            Some((Some(0.5), "downloading 100% of 2.03MiB in 00:00:00 at 8.48MiB/s".into(), Some(("Live set", 0.0))))
        );
        assert_eq!(at("mlm dl 125\tA - B\t   1.20MiB at  500.00KiB/s"), None); // size unknown: no percentage
        // then the post-processing steps
        assert_eq!(at("mlm pp ExtractAudio started"), Some((Some(0.5), "converting to MP3".into(), None)));
        assert_eq!(at("mlm pp Metadata started"), Some((Some(0.9), "adding tags".into(), None)));
        assert_eq!(at("mlm pp EmbedThumbnail started"), Some((Some(0.94), "adding artwork".into(), None)));
        assert_eq!(at("mlm pp MoveFiles started"), Some((Some(0.98), "moving into the library".into(), None)));
        for other in ["mlm pp ExtractAudio finished", "[download] Destination: /tmp/mlm dl 5% x.webm", "[ExtractAudio] Destination: /tmp/x.mp3",
                      "Deleting original file /tmp/x.webm (pass -k to keep)", ""] {
            assert_eq!(at(other), None, "{other}");
        }
    }

    #[test]
    fn reads_ffmpeg_report_tail() {
        let f = std::env::temp_dir().join(format!("mlm-ffmpeg-report-{}", std::process::id()));
        assert_eq!(ffmpeg_secs(&f), None); // not started
        let block = |us: &str, end: &str| format!("bitrate= 320.0kbits/s\ntotal_size=49964\nout_time_us={us}\nout_time_ms={us}\nout_time=00:00:01.231979\nspeed=12.3x\nprogress={end}\n");
        std::fs::write(&f, block("N/A", "continue")).unwrap();
        assert_eq!(ffmpeg_secs(&f), None);
        std::fs::write(&f, block("535979", "continue").repeat(40) + &block("125007000", "end")).unwrap();
        assert_eq!(ffmpeg_secs(&f), Some(125.007));
        std::fs::remove_file(&f).unwrap();
    }

    #[tokio::test]
    async fn jobs_run_five_at_a_time_in_order_and_all_finish() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let app = App::new(PathBuf::new(), PathBuf::new(), PathBuf::new());
        let (now, max) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        for i in 0..8 {
            let (now, max) = (now.clone(), max.clone());
            app.job(format!("k{i}"), format!("job {i}"), move |job| {
                max.fetch_max(now.fetch_add(1, SeqCst) + 1, SeqCst);
                job.set(|j| j.progress = 0.5);
                std::thread::sleep(std::time::Duration::from_millis(60));
                now.fetch_sub(1, SeqCst);
                if i == 7 { Err("boom".into()) } else { Ok(format!("ok {i}")) }
            });
        }
        app.job("k0".into(), "again".into(), |_| Ok("dup".into())); // k0 is pending: not queued a second time
        assert_eq!(app.jobs.lock().unwrap().len(), 8);
        let wait = || tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(30)));
        wait().await.unwrap();
        let states: Vec<_> = app.jobs.lock().unwrap().iter().map(|j| j.state).collect();
        assert_eq!(states, ["running", "running", "running", "running", "running", "queued", "queued", "queued"]);
        while app.jobs.lock().unwrap().iter().any(|j| j.end.is_none()) {
            wait().await.unwrap();
        }
        assert_eq!(max.load(SeqCst), MAX_JOBS);
        let jobs = app.jobs.lock().unwrap().clone();
        assert!(jobs[..7].iter().enumerate().all(|(i, j)| j.state == "done" && j.text == format!("ok {i}") && j.progress == 1.0));
        assert_eq!((jobs[7].state, jobs[7].text.as_str()), ("failed", "boom"));
    }

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
