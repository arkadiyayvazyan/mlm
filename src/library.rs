use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey};
use serde::{Deserialize, Serialize};

const EXTS: &[&str] = &["aif", "aiff", "aifc", "mp3", "flac", "m4a", "ogg", "wav"];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Track {
    pub id: u64,
    #[serde(skip_serializing)]
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_no: u32,
    pub duration_ms: u64,
    pub rate: u32,
    pub bpm: u32,
    pub ext: String,
    /// Path relative to the music root: the stable key for tags (`id` changes across Rust releases).
    pub rel: String,
    /// "Date added", unix seconds: see `added`.
    pub added: u64,
}

impl Track {
    pub fn is_aiff(&self) -> bool {
        matches!(self.ext.as_str(), "aif" | "aiff" | "aifc")
    }
}

/// On-disk cache: path -> (mtime, track). Kept separate from the serving struct so `path`
/// stays in the cache but out of the API JSON.
#[derive(Serialize, Deserialize, Default)]
struct Cache(HashMap<PathBuf, (u64, CachedTrack)>);

#[derive(Serialize, Deserialize, Clone)]
struct CachedTrack {
    title: String,
    artist: String,
    album: String,
    track_no: u32,
    duration_ms: u64,
    #[serde(default)]
    rate: u32,
    /// None = cache entry predates this field (re-tag once); Some(0) = untagged.
    #[serde(default)]
    bpm: Option<u32>,
}

fn id_of(path: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    h.finish() >> 11 // 53 bits: exact as a JS number
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// When the file came into the library: the earlier of its birth time (copied onto this disk) and mtime
/// (the download time, when a copy preserved it). mtime alone is useless here: tag writers bump it.
fn added(path: &Path) -> u64 {
    birth(path).map_or(mtime(path), |b| b.min(mtime(path)))
}

/// statx birth time. std's `Metadata::created` isn't implemented on musl (the Pi build), nor is
/// `libc::statx`, so this is the raw syscall; the kernel's struct statx layout is fixed.
fn birth(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    const STATX_BTIME: u32 = 0x800;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf = [0u64; 32]; // struct statx is 256 bytes: stx_mask is the first u32, stx_btime.tv_sec is at byte 80
    // SAFETY: buf is a writable, 8-aligned 256-byte buffer, the size of struct statx; c is NUL-terminated
    let r = unsafe { libc::syscall(libc::SYS_statx, libc::AT_FDCWD, c.as_ptr(), 0, STATX_BTIME, buf.as_mut_ptr()) };
    (r == 0 && buf[0] as u32 & STATX_BTIME != 0 && buf[10] > 0).then_some(buf[10])
}

fn read_tags(path: &Path) -> Option<CachedTrack> {
    let f = lofty::read_from_path(path).ok()?;
    let dur = f.properties().duration().as_millis() as u64;
    let rate = f.properties().sample_rate().unwrap_or(0);
    let tag = f.primary_tag().or_else(|| f.first_tag());
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let get = |k: ItemKey| tag.and_then(|t| t.get_string(&k)).map(str::to_owned);
    Some(CachedTrack {
        title: get(ItemKey::TrackTitle).unwrap_or(stem),
        artist: get(ItemKey::TrackArtist).or_else(|| get(ItemKey::AlbumArtist)).unwrap_or_default(),
        album: get(ItemKey::AlbumTitle).unwrap_or_default(),
        track_no: tag.and_then(|t| t.track()).unwrap_or(0),
        duration_ms: dur,
        rate,
        bpm: Some(get(ItemKey::IntegerBpm).or_else(|| get(ItemKey::Bpm)).and_then(|s| s.trim().parse::<f64>().ok()).map(|b| b.round() as u32).unwrap_or(0)),
    })
}

/// Walk `dir`, reuse cache entries with unchanged mtime, tag the rest, write cache, return tracks.
pub fn scan(dir: &Path, cache_path: &Path) -> Vec<Track> {
    let old: Cache = std::fs::read(cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut new = Cache::default();
    let mut n = 0usize;
    for e in walkdir::WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        let p = e.path();
        // macOS AppleDouble sidecars (._foo.aiff) are 4 KB resource forks, not audio
        if e.file_name().to_string_lossy().starts_with("._") {
            continue;
        }
        match p.extension().and_then(|x| x.to_str()) {
            Some(x) if EXTS.contains(&x.to_ascii_lowercase().as_str()) => {}
            _ => continue,
        }
        let mt = mtime(p);
        let t = match old.0.get(p) {
            Some((m, t)) if *m == mt && t.rate != 0 && t.bpm.is_some() => t.clone(), // rate==0 / bpm None: older cache entry, re-tag
            _ => match read_tags(p) {
                Some(t) => { n += 1; t }
                None => continue,
            },
        };
        new.0.insert(p.to_path_buf(), (mt, t));
    }
    if let Ok(b) = serde_json::to_vec(&new) {
        let _ = std::fs::write(cache_path, b);
    }
    eprintln!("scan: {} tracks ({} newly tagged)", new.0.len(), n);
    to_tracks(dir, new)
}

/// Load the cache only (fast startup path); `scan` refreshes it afterwards.
pub fn load_cache(dir: &Path, cache_path: &Path) -> Vec<Track> {
    std::fs::read(cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .map(|c| to_tracks(dir, c))
        .unwrap_or_default()
}

fn to_tracks(dir: &Path, c: Cache) -> Vec<Track> {
    let mut v: Vec<Track> = c
        .0
        .into_iter()
        .map(|(path, (_, t))| Track {
            id: id_of(&path),
            ext: path.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default(),
            title: t.title,
            artist: t.artist,
            album: t.album,
            track_no: t.track_no,
            duration_ms: t.duration_ms,
            rate: t.rate,
            bpm: t.bpm.unwrap_or(0),
            rel: path.strip_prefix(dir).unwrap_or(&path).to_string_lossy().into_owned(),
            added: added(&path),
            path,
        })
        .collect();
    v.sort_by(|a, b| (&a.artist, &a.album, a.track_no, &a.title).cmp(&(&b.artist, &b.album, b.track_no, &b.title)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::config::WriteOptions;
    use lofty::tag::{Tag, TagType};

    #[test]
    fn bpm_from_tag_and_cache_refresh() {
        let dir = std::env::temp_dir().join(format!("mlm-bpm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("t.wav");
        // minimal 44-byte PCM header + 100 silent 16-bit mono samples at 8 kHz
        let mut b = Vec::new();
        b.extend(b"RIFF"); b.extend(236u32.to_le_bytes()); b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes()); b.extend(1u16.to_le_bytes()); b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes()); b.extend(16000u32.to_le_bytes()); b.extend(2u16.to_le_bytes()); b.extend(16u16.to_le_bytes());
        b.extend(b"data"); b.extend(200u32.to_le_bytes()); b.extend([0u8; 200]);
        std::fs::write(&wav, b).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.insert_text(ItemKey::IntegerBpm, "128".into());
        lofty::tag::TagExt::save_to_path(&tag, &wav, WriteOptions::default()).unwrap();

        let cache = dir.join("idx.json");
        // pre-bpm cache entry (no `bpm` field) must be re-tagged, not trusted
        std::fs::write(&cache, format!(
            r#"{{"{}":[{},{{"title":"stale","artist":"","album":"","track_no":0,"duration_ms":0,"rate":8000}}]}}"#,
            wav.display(), mtime(&wav))).unwrap();
        let t = &scan(&dir, &cache)[0];
        assert_eq!((t.bpm, t.title.as_str()), (128, "t"));
        assert_eq!(scan(&dir, &cache)[0].bpm, 128); // second scan served from refreshed cache
        let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert!((now - 60..=now).contains(&added(&wav)), "added = just now: {}", added(&wav));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
