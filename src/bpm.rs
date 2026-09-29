//! Tempo detection and the BPM tag write (ctrl+a in the egui UI).
//!
//! Onset envelope: every HOP samples, the rise in log energy of the bass (two one-pole low-passes, ~150 Hz)
//! plus the rise in full-band log energy. The beat period is the lag where that envelope best matches
//! itself shifted by 1..4 beats (autocorrelation, with a mild preference for ~125 BPM against octave errors),
//! then refined to 0.01 BPM by lining up 16 beats. On 40 tagged library tracks: 35 matched the existing tag
//! within ±1, 3 were half/double, 2 (slow, swung) off by 4/3.

use std::io;
use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{ItemKey, Tag};

const HOP: usize = 128;
const MAX_SECS: u64 = 240; // ponytail: first 4 min only (a 96-min mix would take a minute to decode on the Pi)

/// Streaming onset envelope of a mono signal.
struct Onsets {
    k: f32, // low-pass coefficient
    lp: [f32; 2],
    acc: [f32; 2], // bass, full energy in the current hop
    n: usize,
    smooth: [f32; 2],
    prev: [f32; 2],
    env: Vec<f32>,
}

impl Onsets {
    fn new(rate: u32) -> Self {
        let k = 1.0 - (-2.0 * std::f32::consts::PI * 150.0 / rate as f32).exp();
        Self { k, lp: [0.0; 2], acc: [0.0; 2], n: 0, smooth: [0.0; 2], prev: [0.0; 2], env: vec![] }
    }
    fn push(&mut self, x: f32) {
        self.lp[0] += self.k * (x - self.lp[0]);
        self.lp[1] += self.k * (self.lp[0] - self.lp[1]);
        self.acc[0] += self.lp[1] * self.lp[1];
        self.acc[1] += x * x;
        self.n += 1;
        if self.n == HOP {
            let mut flux = 0.0;
            for b in 0..2 {
                self.smooth[b] += 0.35 * (self.acc[b] - self.smooth[b]); // ~8 ms window: no ripple from the bass itself
                let v = (self.smooth[b] + 1e-6).ln();
                flux += (v - self.prev[b]).max(0.0);
                self.prev[b] = v;
            }
            self.env.push(flux);
            (self.acc, self.n) = ([0.0; 2], 0);
        }
    }
}

/// BPM of an onset envelope sampled at `fps` hops per second, searched in 60..200.
fn tempo(mut env: Vec<f32>, fps: f64) -> Option<f64> {
    let mean = env.iter().sum::<f32>() / env.len().max(1) as f32;
    env.iter_mut().for_each(|v| *v -= mean);
    let lag = |bpm: f64| 60.0 * fps / bpm;
    let max_lag = (lag(60.0) * 16.0) as usize + 2;
    if env.len() < max_lag * 2 {
        return None; // shorter than ~32 s at 60 BPM: not enough beats to trust
    }
    let r: Vec<f64> = (0..=max_lag).map(|l| env.iter().zip(&env[l..]).map(|(a, b)| (a * b) as f64).sum()).collect();
    let at = |l: f64| { let (i, f) = (l as usize, l.fract()); r[i] * (1.0 - f) + r[i + 1] * f };
    let score = |bpm: f64, beats: usize| (1..=beats).map(|k| at(k as f64 * lag(bpm))).sum::<f64>();
    let prior = |bpm: f64| (-0.5 * ((bpm / 125.0).log2() / 0.9).powi(2)).exp();
    let coarse = (600..=2000).map(|b| b as f64 / 10.0).max_by(|a, b| (score(*a, 4) * prior(*a)).total_cmp(&(score(*b, 4) * prior(*b))))?;
    (-150..=150).map(|d| coarse + d as f64 / 100.0).max_by(|a, b| score(*a, 16).total_cmp(&score(*b, 16)))
}

/// Decode (up to MAX_SECS) and detect the tempo.
pub fn detect(path: &Path) -> io::Result<Option<f64>> {
    let mut o: Option<(Onsets, u32)> = None;
    crate::decode::each_packet(path, |_, rate, ch, samples| {
        let (on, _) = o.get_or_insert_with(|| (Onsets::new(rate), rate));
        for f in samples.chunks_exact(ch) {
            on.push(f.iter().map(|&s| s as f32 / 2147483648.0).sum::<f32>() / ch as f32);
        }
        Ok(((on.env.len() * HOP) as u64 / rate as u64) < MAX_SECS)
    })?;
    Ok(o.and_then(|(on, rate)| tempo(on.env, rate as f64 / HOP as f64)))
}

/// Write `bpm` into the file's tag. The file is edited in place (keeping its birth time, i.e. "date added");
/// a copy is kept until the write succeeds and restored if it fails.
pub fn write(path: &Path, bpm: u32) -> io::Result<()> {
    let bak = path.with_file_name(format!(".{}.mlm-bak", path.file_name().unwrap_or_default().to_string_lossy()));
    std::fs::copy(path, &bak)?;
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let r = match ext.as_str() {
        "mp3" | "aif" | "aiff" | "aifc" | "wav" => write_id3(path, bpm),
        _ => write_lofty(path, bpm),
    };
    match r {
        Ok(()) => std::fs::remove_file(&bak),
        Err(e) => {
            std::fs::copy(&bak, path)?; // copy, not rename: same inode, birth time kept
            std::fs::remove_file(&bak)?;
            Err(e)
        }
    }
}

/// ID3 (MP3/AIFF/WAV) via the id3 crate: it sets TBPM and writes every other frame back byte for byte,
/// including ones it doesn't model (DJ software's GEOB cue/beatgrid blobs, OWNE) and the tag version.
/// lofty re-encodes the whole tag and refuses frames like OWNE.
fn write_id3(path: &Path, bpm: u32) -> io::Result<()> {
    use id3::{Tag, TagLike, Version};
    let mut tag = match Tag::read_from_path(path) {
        Ok(t) => t,
        Err(e) if matches!(e.kind, id3::ErrorKind::NoTag) => Tag::with_version(Version::Id3v24),
        Err(e) => return Err(io::Error::other(e)),
    };
    tag.set_text("TBPM", bpm.to_string());
    let v = tag.version();
    tag.write_to_path(path, v).map_err(io::Error::other)
}

/// Everything else (M4A `tmpo`, FLAC/Ogg comments) via lofty.
fn write_lofty(path: &Path, bpm: u32) -> io::Result<()> {
    let mut f = lofty::read_from_path(path).map_err(io::Error::other)?;
    if f.primary_tag().is_none() {
        f.insert_tag(Tag::new(f.primary_tag_type()));
    }
    let tag = f.primary_tag_mut().unwrap();
    tag.remove_key(&ItemKey::Bpm);
    tag.insert_text(ItemKey::IntegerBpm, bpm.to_string());
    f.save_to_path(path, WriteOptions::default()).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_128_bpm_kicks_and_hats() {
        let rate = 44100;
        let beat = 60.0 / 128.0;
        let mut o = Onsets::new(rate);
        for i in 0..rate as usize * 60 {
            let t = i as f64 / rate as f64;
            let (b, h) = (t % beat, (t + beat / 2.0) % beat); // kick on the beat, hat on the offbeat
            let kick = (2.0 * std::f64::consts::PI * 55.0 * b).sin() * (-b * 30.0).exp();
            let hat = ((i * 7919 % 1000) as f64 / 500.0 - 1.0) * (-h * 200.0).exp() * 0.3;
            o.push((kick + hat) as f32 * 0.5);
        }
        let bpm = tempo(o.env, rate as f64 / HOP as f64).unwrap();
        assert!((bpm - 128.0).abs() < 0.1, "{bpm}");
    }
}

