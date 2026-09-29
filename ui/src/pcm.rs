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

/// Fold frames [from, to) of `bytes` into `peaks` (bin = frame * BINS / frames, max |sample| over channels).
pub fn update_peaks(peaks: &mut [f32], bytes: &[u8], bps: usize, nch: usize, from: usize, to: usize, frames: usize) {
    for f in from..to {
        let bin = (f as u64 * peaks.len() as u64 / frames as u64) as usize; // u64: f * 1000 overflows wasm32's usize past 97 s
        for s in bytes[f * bps * nch..(f + 1) * bps * nch].chunks_exact(bps) {
            peaks[bin] = peaks[bin].max(sample(s).abs());
        }
    }
}

#[test]
fn stereo_16_bit_and_peaks() {
    let samples: [i16; 6] = [1000, -2000, 32767, -32768, 0, 16384]; // 3 frames of L/R
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut out = vec![];
    to_stereo(&bytes[..11], 2, 2, &mut out); // trailing partial frame is left for the next call
    assert_eq!(out, [1000.0 / 32768.0, -2000.0 / 32768.0, 32767.0 / 32768.0, -1.0]);
    let mut peaks = vec![0.0; BINS];
    update_peaks(&mut peaks, &bytes, 2, 2, 0, 3, 3);
    // bins: frame f -> f*1000/3 = 0, 333, 666
    assert_eq!((peaks[0], peaks[333], peaks[666]), (2000.0 / 32768.0, 1.0, 0.5));
    assert_eq!(peaks.iter().filter(|p| **p != 0.0).count(), 3);
}

#[test]
fn mono_24_bit_sign_extends_and_doubles() {
    let mut out = vec![];
    to_stereo(&[0xff, 0xff, 0xff, 0x00, 0x00, 0x80], 3, 1, &mut out);
    assert_eq!(out, [-1.0 / 8388608.0, -1.0 / 8388608.0, -1.0, -1.0]);
}
