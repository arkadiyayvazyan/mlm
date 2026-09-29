//! Non-AIFF formats (MP3, M4A, FLAC, WAV, ...) decoded to a streamed WAV, so the egui player gets one
//! input format and starts playing on the first chunk. Decoding runs on a blocking thread behind a small
//! channel: it keeps pace with the client and stops when the client hangs up. No ranges, always from 0.

use std::io;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::Stream;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error;
use tokio::sync::mpsc::{channel, Sender};

use crate::aiff;

pub fn stream(path: PathBuf, est_ms: u64) -> impl Stream<Item = io::Result<Bytes>> {
    let (tx, rx) = channel(8);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = decode(&path, est_ms, &tx) {
            let _ = tx.blocking_send(Err(e));
        }
    });
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) })
}

fn decode(path: &Path, est_ms: u64, tx: &Sender<io::Result<Bytes>>) -> io::Result<()> {
    let send = |b: Vec<u8>| tx.blocking_send(Ok(b.into())).map_err(|_| io::Error::other("client gone"));
    let mut out = Vec::new();
    let mut head = false;
    each_packet(path, |p, rate, channels, samples| {
        let bits: u16 = if p.bits_per_sample.unwrap_or(16) > 16 { 24 } else { 16 };
        if !head {
            head = true;
            // unknown length (VBR MP3 without a Xing header): overestimate from the tag duration, the player trims to what arrives
            let frames = p.n_frames.unwrap_or((est_ms + 1000) * rate as u64 / 1000);
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

/// Decode `path` packet by packet: `each(codec params, rate, channels, interleaved i32 samples)` returns
/// whether to go on. Rate and channels come from the decoded buffer: AAC in MP4 only knows its channel
/// count after decoding.
pub fn each_packet(
    path: &Path,
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
        if !each(p, rate, channels, sb.samples())? {
            return Ok(());
        }
    }
}
