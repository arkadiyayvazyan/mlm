// PCM loading: streamed WAV for AIFF (server transcodes), whole-file decode for the rest.

export const BINS = 1000;

export type Loaded = {
  rate: number;              // Hz
  channels: Float32Array[];  // full-length, filled progressively (AIFF) or at once (others)
  frames: number;            // total frames, known from header before data arrives
  loaded: number;            // frames filled so far; == frames when done
  peaks: Float32Array;       // BINS entries 0..1, max |sample| over all channels
  onProgress?: () => void;   // called after each chunk/decode
  done: Promise<void>;       // resolves when loaded == frames; rejects on abort/error
};

/** Convert whole interleaved LE PCM frames from `bytes` into `out` starting at frame `at`. Returns frames written. */
export function pcmToFloat(bytes: Uint8Array, bits: number, channels: number, out: Float32Array[], at: number): number {
  const bps = bits / 8;
  const n = Math.min(Math.floor(bytes.length / (bps * channels)), out[0].length - at);
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let p = 0;
  for (let f = 0; f < n; f++) {
    for (let c = 0; c < channels; c++, p += bps) {
      let s: number;
      if (bits === 16) s = dv.getInt16(p, true) / 32768;
      else if (bits === 24) s = (((bytes[p] | (bytes[p + 1] << 8) | (bytes[p + 2] << 16)) << 8) >> 8) / 8388608;
      else s = dv.getInt32(p, true) / 2147483648;
      out[c][at + f] = s;
    }
  }
  return n;
}

/** Fold frames [from, to) of `channels` into `peaks` (bin = floor(frame * BINS / frames)). */
export function updatePeaks(peaks: Float32Array, channels: Float32Array[], from: number, to: number, frames: number): void {
  for (let f = from; f < to; f++) {
    const bin = Math.floor(f * peaks.length / frames);
    for (const ch of channels) {
      const a = Math.abs(ch[f]);
      if (a > peaks[bin]) peaks[bin] = a;
    }
  }
}

export function load(url: string, ext: string, ctx: AudioContext, signal: AbortSignal): Loaded {
  const L: Loaded = { rate: 0, channels: [], frames: 0, loaded: 0, peaks: new Float32Array(BINS), done: Promise.resolve() };
  L.done = /^aif[fc]?$/i.test(ext) ? streamWav(url, signal, L) : decodeAll(url, ctx, signal, L);
  return L;
}

async function streamWav(url: string, signal: AbortSignal, L: Loaded): Promise<void> {
  const res = await fetch(url, { signal });
  if (!res.ok || !res.body) throw new Error(`fetch ${url}: ${res.status}`);
  const reader = res.body.getReader();
  signal.addEventListener("abort", () => reader.cancel().catch(() => {}), { once: true });
  let carry: Uint8Array = new Uint8Array(0);
  let bits = 0, nch = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (signal.aborted) throw signal.reason ?? new DOMException("aborted", "AbortError");
    if (done) break;
    let buf: Uint8Array = carry.length ? concat(carry, value) : value;
    if (!nch) {
      if (buf.length < 44) { carry = buf; continue; }
      const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
      nch = dv.getUint16(22, true);
      L.rate = dv.getUint32(24, true);
      bits = dv.getUint16(34, true);
      L.frames = Math.floor(dv.getUint32(40, true) / (bits / 8 * nch));
      L.channels = Array.from({ length: nch }, () => new Float32Array(L.frames));
      buf = buf.subarray(44);
    }
    const n = pcmToFloat(buf, bits, nch, L.channels, L.loaded);
    const used = n * (bits / 8) * nch;
    carry = buf.subarray(used).slice();
    if (n) {
      updatePeaks(L.peaks, L.channels, L.loaded, L.loaded + n, L.frames);
      L.loaded += n;
      L.onProgress?.();
    }
  }
  if (L.loaded < L.frames) L.frames = L.loaded; // truncated stream: keep loaded == frames invariant
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a); out.set(b, a.length);
  return out;
}

// ponytail: whole-file decode for MP3/M4A; ~0.5 s first-sound on a cold click, fine for 300 small files
async function decodeAll(url: string, ctx: AudioContext, signal: AbortSignal, L: Loaded): Promise<void> {
  const res = await fetch(url, { signal });
  if (!res.ok) throw new Error(`fetch ${url}: ${res.status}`);
  const buf = await ctx.decodeAudioData(await res.arrayBuffer());
  if (signal.aborted) throw signal.reason ?? new DOMException("aborted", "AbortError");
  L.rate = buf.sampleRate;
  L.channels = Array.from({ length: buf.numberOfChannels }, (_, c) => buf.getChannelData(c));
  L.frames = L.loaded = buf.length;
  updatePeaks(L.peaks, L.channels, 0, L.frames, L.frames);
  L.onProgress?.();
}
