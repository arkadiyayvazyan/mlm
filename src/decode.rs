//! Non-AIFF formats (MP3, M4A, FLAC, WAV, ...) decoded to a streamed WAV, so the egui player gets one
//! input format and starts playing on the first chunk. Decoding runs on a blocking thread behind a small
//! channel: it keeps pace with the client and stops when the client hangs up. `from` (a frame) starts the
//! stream mid-track for a seek past what the client holds: the header then counts the frames left.

use std::io;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::Stream;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error;
use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::units::Time;
use tokio::sync::mpsc::{channel, Sender};

use crate::aiff;

pub fn stream(path: PathBuf, est_ms: u64, from: u64) -> impl Stream<Item = io::Result<Bytes>> {
    let (tx, rx) = channel(8);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = decode(&path, est_ms, from, &tx) {
            let _ = tx.blocking_send(Err(e));
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) })
}

fn decode(path: &Path, est_ms: u64, from: u64, tx: &Sender<io::Result<Bytes>>) -> io::Result<()> {
    let send = |b: Vec<u8>| tx.blocking_send(Ok(b.into())).map_err(|_| io::Error::other("client gone"));
    let mut out = Vec::new();
    let mut head = false;
    each_packet(path, from, |p, rate, channels, samples| {
        let bits: u16 = if p.bits_per_sample.unwrap_or(16) > 16 { 24 } else { 16 };
        if !head {
            head = true;
            // unknown length (VBR MP3 without a Xing header): overestimate from the tag duration, the player trims to what arrives
            let frames = p.n_frames.unwrap_or((est_ms + 1000) * rate as u64 / 1000).saturating_sub(from);
            let data_len = frames * (bits as usize / 8 * channels) as u64;
            send(aiff::Info { channels: channels as u16, bits, rate, data_off: 0, data_len, little: true }.wav_header().to_vec())?;
        }
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes()[4 - bits as usize / 8..]); // top bytes of the i32 sample
        }
        if out.len() >= 64 * 1024 {
            send(std::mem::take(&mut out))?;
        }
        Ok(true)
    })?;
    send(out)
}

/// Decode `path` packet by packet from frame `from`: `each(codec params, rate, channels, interleaved i32
/// samples)` returns whether to go on. Rate and channels come from the decoded buffer: AAC in MP4 only knows
/// its channel count after decoding.
pub fn each_packet(
    path: &Path,
    from: u64,
    mut each: impl FnMut(&CodecParameters, u32, usize, &[i32]) -> io::Result<bool>,
) -> io::Result<()> {
    let bad = |e: Error| io::Error::new(io::ErrorKind::InvalidData, e);
    let mut hint = symphonia::core::probe::Hint::new();
    if let Some(e) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(e);
    }
    let src = symphonia::core::io::MediaSourceStream::new(Box::new(std::fs::File::open(path)?), Default::default());
    let opts = symphonia::core::formats::FormatOptions { enable_gapless: true, ..Default::default() };
    let mut format = symphonia::default::get_probe().format(&hint, src, &opts, &Default::default()).map_err(bad)?.format;
    let track = format.default_track().ok_or_else(|| io::Error::other("no audio track"))?.clone();
    let p = &track.codec_params;
    let mut dec = symphonia::default::get_codecs().make(p, &Default::default()).map_err(bad)?;
    // an accurate seek lands on the packet holding `from`: the frames before it are dropped below
    let mut skip = 0;
    if from > 0 {
        let rate = p.sample_rate.ok_or_else(|| io::Error::other("unknown sample rate"))? as u64;
        let time = Time::new(from / rate, (from % rate) as f64 / rate as f64);
        skip = match format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track.id) }) {
            Ok(to) => {
                let t = p.time_base.map_or(Time::new(0, 0.0), |tb| tb.calc_time(to.required_ts.saturating_sub(to.actual_ts)));
                (t.seconds * rate + (t.frac * rate as f64).round() as u64) as usize
            }
            Err(Error::SeekError(_)) => from as usize, // refused before moving: play from the top and drop up to `from`
            Err(e) => return Err(bad(e)),
        };
        dec.reset();
    }
    let mut sb: Option<SampleBuffer<i32>> = None;
    loop {
        let pkt = match format.next_packet() {
            Ok(pkt) => pkt,
            Err(Error::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(bad(e)),
        };
        if pkt.track_id() != track.id {
            continue;
        }
        let buf = match dec.decode(&pkt) {
            Ok(buf) => buf,
            Err(Error::DecodeError(_)) => continue, // one corrupt frame: skip it
            Err(e) => return Err(bad(e)),
        };
        let (rate, channels) = (buf.spec().rate, buf.spec().channels.count());
        if sb.as_ref().is_none_or(|s| s.capacity() < buf.capacity()) {
            sb = Some(SampleBuffer::new(buf.capacity() as u64, *buf.spec()));
        }
        let sb = sb.as_mut().unwrap();
        sb.copy_interleaved_ref(buf);
        let drop = (skip * channels).min(sb.samples().len());
        skip -= drop / channels;
        if !each(p, rate, channels, &sb.samples()[drop..])? {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decoding from frame `from` yields exactly the tail of decoding from 0.
    #[test]
    fn starts_mid_track_on_the_exact_frame() {
        let frames: u64 = 10_000; // 16-bit mono WAV, a ramp: sample i = i
        let info = aiff::Info { channels: 1, bits: 16, rate: 8000, data_off: 0, data_len: frames * 2, little: true };
        let mut wav = info.wav_header().to_vec();
        wav.extend((0..frames as i16).flat_map(|i| i.to_le_bytes()));
        let path = std::env::temp_dir().join("mlm-decode-test.wav");
        std::fs::write(&path, wav).unwrap();
        let collect = |from: u64| {
            let mut out = vec![];
            each_packet(&path, from, |_, rate, ch, s| { assert_eq!((rate, ch), (8000, 1)); out.extend(s.iter().map(|v| (v >> 16) as i16)); Ok(true) }).unwrap();
            out
        };
        let all = collect(0);
        assert_eq!(all.len(), frames as usize);
        assert_eq!(all[4321], 4321);
        assert_eq!(collect(4321), all[4321..]);
        assert_eq!(collect(frames - 1), all[frames as usize - 1..]);
    }
}
