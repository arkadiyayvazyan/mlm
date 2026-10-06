//! AIFF/AIFC -> WAV on the fly. AIFF is big-endian PCM; WAV is little-endian PCM with a 44-byte
//! header. Conversion is a byte swap, so any byte range of the virtual WAV maps linearly onto
//! the source file. No decoding, no buffering of whole tracks.

use std::io::{self, Read, Seek, SeekFrom};

use bytes::Bytes;
use futures_util::Stream;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub const WAV_HEADER: u64 = 44;
const CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Info {
    pub channels: u16,
    pub bits: u16,
    pub rate: u32,
    pub data_off: u64,
    pub data_len: u64,
    /// true when samples are already little-endian (AIFC "sowt")
    pub little: bool,
}

impl Info {
    pub fn wav_len(&self) -> u64 {
        WAV_HEADER + self.data_len
    }
    pub fn frame(&self) -> usize {
        (self.bits as usize / 8) * self.channels as usize
    }
    /// The same sound minus its first `n` frames: a virtual WAV of the rest.
    pub fn from_frame(self, n: u64) -> Info {
        let cut = (n * self.frame() as u64).min(self.data_len);
        Info { data_off: self.data_off + cut, data_len: self.data_len - cut, ..self }
    }
    pub fn wav_header(&self) -> [u8; 44] {
        let mut h = [0u8; 44];
        let bps = self.bits / 8;
        let block = bps * self.channels;
        h[0..4].copy_from_slice(b"RIFF");
        h[4..8].copy_from_slice(&((36 + self.data_len) as u32).to_le_bytes());
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        h[20..22].copy_from_slice(&1u16.to_le_bytes());
        h[22..24].copy_from_slice(&self.channels.to_le_bytes());
        h[24..28].copy_from_slice(&self.rate.to_le_bytes());
        h[28..32].copy_from_slice(&(self.rate * block as u32).to_le_bytes());
        h[32..34].copy_from_slice(&block.to_le_bytes());
        h[34..36].copy_from_slice(&self.bits.to_le_bytes());
        h[36..40].copy_from_slice(b"data");
        h[40..44].copy_from_slice(&(self.data_len as u32).to_le_bytes());
        h
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Walk the IFF chunks and find COMM + SSND.
pub fn parse<R: Read + Seek>(r: &mut R) -> io::Result<Info> {
    let mut hdr = [0u8; 12];
    r.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"FORM" {
        return Err(bad("not an IFF file"));
    }
    let aifc = match &hdr[8..12] {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return Err(bad("not AIFF/AIFC")),
    };
    let mut comm: Option<(u16, u16, u32, bool)> = None;
    let mut ssnd: Option<(u64, u64)> = None;
    let mut ck = [0u8; 8];
    loop {
        if r.read_exact(&mut ck).is_err() {
            break;
        }
        let id = &ck[0..4];
        let len = u32::from_be_bytes([ck[4], ck[5], ck[6], ck[7]]) as u64;
        let start = r.stream_position()?;
        match id {
            b"COMM" => {
                let mut b = vec![0u8; len.min(64) as usize];
                r.read_exact(&mut b)?;
                if b.len() < 18 {
                    return Err(bad("short COMM"));
                }
                let channels = u16::from_be_bytes([b[0], b[1]]);
                let bits = u16::from_be_bytes([b[6], b[7]]);
                let rate = ext80_to_u32(&b[8..18]);
                let little = aifc && b.len() >= 22 && &b[18..22] == b"sowt";
                if aifc && b.len() >= 22 && &b[18..22] != b"NONE" && !little {
                    return Err(bad("compressed AIFC unsupported"));
                }
                comm = Some((channels, bits, rate, little));
            }
            b"SSND" => {
                let mut b = [0u8; 8];
                r.read_exact(&mut b)?;
                let offset = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
                ssnd = Some((start + 8 + offset, len.saturating_sub(8 + offset)));
            }
            _ => {}
        }
        // chunks are padded to even length
        r.seek(SeekFrom::Start(start + len + (len & 1)))?;
        if comm.is_some() && ssnd.is_some() {
            break;
        }
    }
    let (channels, bits, rate, little) = comm.ok_or_else(|| bad("no COMM"))?;
    let (data_off, data_len) = ssnd.ok_or_else(|| bad("no SSND"))?;
    if !matches!(bits, 16 | 24 | 32) || channels == 0 {
        // ponytail: 8-bit AIFF (signed) unsupported, add sign flip if a file ever shows up
        return Err(bad("unsupported bit depth"));
    }
    let info = Info { channels, bits, rate, data_off, data_len, little };
    // trim to whole frames so range math never splits a frame at EOF
    let data_len = data_len - data_len % info.frame() as u64;
    Ok(Info { data_len, ..info })
}

/// 80-bit IEEE 754 extended -> u32 (sample rates are small integers, so no precision loss).
fn ext80_to_u32(b: &[u8]) -> u32 {
    let exp = (u16::from_be_bytes([b[0], b[1]]) & 0x7fff) as i32 - 16383;
    let mant = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if exp < 0 || exp > 63 {
        return 0;
    }
    (mant >> (63 - exp)) as u32
}

/// Reverse each sample's bytes in place (big-endian -> little-endian).
pub fn swap(buf: &mut [u8], bps: usize) {
    if bps == 1 {
        return;
    }
    for s in buf.chunks_exact_mut(bps) {
        s.reverse();
    }
}

/// Stream bytes [start, end] (inclusive) of the virtual WAV file.
pub fn stream(
    file: tokio::fs::File,
    info: Info,
    start: u64,
    end: u64,
) -> impl Stream<Item = io::Result<Bytes>> {
    let bps = info.bits as usize / 8;
    futures_util::stream::try_unfold((file, start), move |(mut file, pos)| async move {
        if pos > end {
            return Ok(None);
        }
        if pos < WAV_HEADER {
            let h = info.wav_header();
            let stop = end.min(WAV_HEADER - 1) as usize;
            return Ok(Some((Bytes::copy_from_slice(&h[pos as usize..=stop]), (file, stop as u64 + 1))));
        }
        // align the read down to a sample boundary so swapping never straddles a chunk
        let data_pos = pos - WAV_HEADER;
        let lead = (data_pos % bps as u64) as usize;
        let want = ((end - pos + 1) as usize + lead).min(CHUNK);
        let want = want.div_ceil(bps) * bps;
        let mut buf = vec![0u8; want];
        file.seek(SeekFrom::Start(info.data_off + data_pos - lead as u64)).await?;
        file.read_exact(&mut buf).await?;
        if !info.little {
            swap(&mut buf, bps);
        }
        let take = (want - lead).min((end - pos + 1) as usize);
        let out = Bytes::from(buf).slice(lead..lead + take);
        Ok(Some((out, (file, pos + take as u64))))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    /// 2 frames, 16-bit stereo, 44100 Hz. Samples (BE): 0x0102 0x0304 | 0x0506 0x0708
    fn sample_aiff() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"FORM\0\0\0\0AIFF");
        v.extend_from_slice(b"COMM\0\0\0\x12");
        v.extend_from_slice(&2u16.to_be_bytes()); // channels
        v.extend_from_slice(&2u32.to_be_bytes()); // frames
        v.extend_from_slice(&16u16.to_be_bytes()); // bits
        v.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]); // 44100.0 as ext80
        v.extend_from_slice(b"SSND\0\0\0\x10");
        v.extend_from_slice(&[0u8; 8]); // offset, blocksize
        v.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        v
    }

    async fn collect(info: Info, path: &std::path::Path, a: u64, b: u64) -> Vec<u8> {
        let f = tokio::fs::File::open(path).await.unwrap();
        let mut out = Vec::new();
        let mut s = std::pin::pin!(stream(f, info, a, b));
        while let Some(c) = s.next().await {
            out.extend_from_slice(&c.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn converts_and_ranges() {
        let bytes = sample_aiff();
        let info = parse(&mut io::Cursor::new(&bytes)).unwrap();
        assert_eq!((info.channels, info.bits, info.rate, info.data_len), (2, 16, 44100, 8));
        assert_eq!(info.frame(), 4);

        let path = std::env::temp_dir().join("mlm-test.aiff");
        std::fs::write(&path, &bytes).unwrap();

        let full = collect(info, &path, 0, info.wav_len() - 1).await;
        assert_eq!(full.len(), 52);
        assert_eq!(&full[0..4], b"RIFF");
        assert_eq!(&full[44..], &[2, 1, 4, 3, 6, 5, 8, 7]);

        // range starting mid-sample and ending mid-sample
        assert_eq!(collect(info, &path, 45, 47).await, vec![1, 4, 3]);
        // range spanning header/data boundary
        assert_eq!(collect(info, &path, 42, 45).await, vec![0, 0, 2, 1]);
        // from the second frame: a one-frame WAV of it
        let rest = info.from_frame(1);
        assert_eq!((rest.data_len, rest.wav_len()), (4, 48));
        assert_eq!(collect(rest, &path, 40, 47).await, vec![4, 0, 0, 0, 6, 5, 8, 7]);
        assert_eq!(info.from_frame(9).data_len, 0);
    }
}
