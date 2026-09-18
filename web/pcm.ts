// STUB — replaced by the real decoder (branch `pcm`) at merge. Contract must not change.
export type Loaded = {
  rate: number;              // Hz
  channels: Float32Array[];  // full-length, filled progressively (AIFF) or at once (others)
  frames: number;            // total frames, known from header before data arrives
  loaded: number;            // frames filled so far; == frames when done
  peaks: Float32Array;       // BINS entries 0..1, max |sample| over all channels, updated as data arrives
  onProgress?: () => void;   // called after each chunk/decode; you set this
  done: Promise<void>;       // resolves when loaded == frames; rejects on abort/error
};
export const BINS = 1000;
export function load(url: string, ext: string, ctx: AudioContext, signal: AbortSignal): Loaded {
  throw new Error("stub");
}
