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
    pub ext: String,
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

fn read_tags(path: &Path) -> Option<CachedTrack> {
    let f = lofty::read_from_path(path).ok()?;
    let dur = f.properties().duration().as_millis() as u64;
    let tag = f.primary_tag().or_else(|| f.first_tag());
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let get = |k: ItemKey| tag.and_then(|t| t.get_string(&k)).map(str::to_owned);
    Some(CachedTrack {
        title: get(ItemKey::TrackTitle).unwrap_or(stem),
        artist: get(ItemKey::TrackArtist).or_else(|| get(ItemKey::AlbumArtist)).unwrap_or_default(),
        album: get(ItemKey::AlbumTitle).unwrap_or_default(),
        track_no: tag.and_then(|t| t.track()).unwrap_or(0),
        duration_ms: dur,
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
            Some((m, t)) if *m == mt => t.clone(),
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
    to_tracks(new)
}

/// Load the cache only (fast startup path); `scan` refreshes it afterwards.
pub fn load_cache(cache_path: &Path) -> Vec<Track> {
    std::fs::read(cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Cache>(&b).ok())
        .map(to_tracks)
        .unwrap_or_default()
}

fn to_tracks(c: Cache) -> Vec<Track> {
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
            path,
        })
        .collect();
    v.sort_by(|a, b| (&a.artist, &a.album, a.track_no, &a.title).cmp(&(&b.artist, &b.album, b.track_no, &b.title)));
    v
}
