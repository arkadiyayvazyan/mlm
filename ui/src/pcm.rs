//! Pure PCM helpers. Tracks are kept as interleaved little-endian integer PCM (`bps` = 2, 3 or 4 bytes
//! per sample) and converted to interleaved stereo f32 only on their way into the worklet's ring.

pub const BINS: usize = 1000;

pub fn sample(s: &[u8]) -> f32 {
    match s.len() {
        2 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
        3 => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8388608.0,
        _ => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2147483648.0,
    }
}

/// Whole frames of `bytes` appended to `out` as interleaved stereo: mono is doubled, channels past 2 dropped.
pub fn to_stereo(bytes: &[u8], bps: usize, nch: usize, out: &mut Vec<f32>) {
    for f in bytes.chunks_exact(bps * nch) {
        out.push(sample(&f[..bps]));
        out.push(sample(&f[if nch > 1 { bps } else { 0 }..][..bps]));
    }
}

/// Fold frames [from, to) of `bytes` (which holds the track from frame `base`) into `peaks` (bin = frame * BINS / frames,
/// max |sample| over channels).
/// Only ~256 evenly spaced frames per bin are looked at: a 7-min track is 18 M frames, and doing all of them on
/// the main thread while three tracks stream in made scrolling stutter on phones. The drawn waveform is the same.
pub fn update_peaks(peaks: &mut [f32], bytes: &[u8], bps: usize, nch: usize, base: usize, from: usize, to: usize, frames: usize) {
    let stride = (frames / (peaks.len() * 256)).max(1);
    for f in (from.next_multiple_of(stride)..to).step_by(stride) {
        let bin = (f as u64 * peaks.len() as u64 / frames as u64) as usize; // u64: f * 1000 overflows wasm32's usize past 97 s
        for s in bytes[(f - base) * bps * nch..(f - base + 1) * bps * nch].chunks_exact(bps) {
            peaks[bin] = peaks[bin].max(sample(s).abs());
        }
    }
}

/// 44-byte header of a PCM WAV holding `frames` frames (for sharing a loaded track as a file).
pub fn wav_header(rate: u32, nch: usize, bps: usize, frames: usize) -> [u8; 44] {
    let (block, data) = ((nch * bps) as u16, (frames * nch * bps) as u32);
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // integer PCM
    h[22..24].copy_from_slice(&(nch as u16).to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * block as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&((bps * 8) as u16).to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

#[test]
fn wav_header_fields() {
    let h = wav_header(44100, 2, 3, 1000);
    let u32_at = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
    let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
    assert_eq!((&h[0..4], &h[8..12], &h[36..40]), (&b"RIFF"[..], &b"WAVE"[..], &b"data"[..]));
    assert_eq!((u16_at(22), u32_at(24), u16_at(32), u16_at(34)), (2, 44100, 6, 24)); // channels, rate, block align, bits
    assert_eq!((u32_at(28), u32_at(40), u32_at(4)), (44100 * 6, 6000, 6036)); // byte rate, data size, riff size
}

#[test]
fn stereo_16_bit_and_peaks() {
    let samples: [i16; 6] = [1000, -2000, 32767, -32768, 0, 16384]; // 3 frames of L/R
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut out = vec![];
    to_stereo(&bytes[..11], 2, 2, &mut out); // trailing partial frame is left for the next call
    assert_eq!(out, [1000.0 / 32768.0, -2000.0 / 32768.0, 32767.0 / 32768.0, -1.0]);
    let mut peaks = vec![0.0; BINS];
    update_peaks(&mut peaks, &bytes, 2, 2, 0, 0, 3, 3);
    // bins: frame f -> f*1000/3 = 0, 333, 666
    assert_eq!((peaks[0], peaks[333], peaks[666]), (2000.0 / 32768.0, 1.0, 0.5));
    assert_eq!(peaks.iter().filter(|p| **p != 0.0).count(), 3);
    // the same bytes held from frame 1 of a 4-frame track: the first frame in them is track frame 1
    let mut peaks = vec![0.0; BINS];
    update_peaks(&mut peaks, &bytes, 2, 2, 1, 1, 4, 4);
    assert_eq!((peaks[250], peaks[500], peaks[750]), (2000.0 / 32768.0, 1.0, 0.5));
}

#[test]
fn peaks_of_a_long_track_are_sampled_but_still_found() {
    let frames = BINS * 1000; // stride 3
    let mut bytes = vec![0u8; frames * 2];
    bytes[2 * 999_999..][..2].copy_from_slice(&i16::MAX.to_le_bytes()); // one loud sample in the last bin, on the stride
    let mut peaks = vec![0.0; BINS];
    for (a, b) in [(0, 400_000), (400_000, frames)] { // arrives in two chunks, split off-stride
        update_peaks(&mut peaks, &bytes, 2, 1, 0, a, b, frames);
    }
    assert_eq!(peaks[BINS - 1], i16::MAX as f32 / 32768.0);
    assert_eq!(peaks.iter().filter(|p| **p > 0.0).count(), 1);
}

#[test]
fn mono_24_bit_sign_extends_and_doubles() {
    let mut out = vec![];
    to_stereo(&[0xff, 0xff, 0xff, 0x00, 0x00, 0x80], 3, 1, &mut out);
    assert_eq!(out, [-1.0 / 8388608.0, -1.0 / 8388608.0, -1.0, -1.0]);
}
