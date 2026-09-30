// Plays interleaved stereo f32 from a SharedArrayBuffer ring filled by the main thread (ui/src/player.rs).
// Header (Int32, frame counts that wrap): [0] write, [1] read, [2] flush target, [3] flush seq.
// Single producer / single consumer: only the main thread stores 0, 2, 3; only this processor stores 1.
class Ring extends AudioWorkletProcessor {
  constructor(o) {
    super();
    const sab = o.processorOptions.sab;
    this.h = new Int32Array(sab, 0, 4);
    this.d = new Float32Array(sab, 16);
    this.mask = this.d.length / 2 - 1;   // capacity in frames is a power of two
    this.seq = Atomics.load(this.h, 3);
    Atomics.store(this.h, 1, Atomics.load(this.h, 2));   // new AudioContext: start at the latest flush
  }
  process(_, [[L, R]]) {
    const h = this.h, seq = Atomics.load(h, 3);
    if (seq !== this.seq) { this.seq = seq; Atomics.store(h, 1, Atomics.load(h, 2)); }   // seek / skip: drop what's queued
    const r = Atomics.load(h, 1), n = Math.min(L.length, (Atomics.load(h, 0) - r) | 0);   // underrun: rest stays silent
    for (let i = 0; i < n; i++) { const k = ((r + i) & this.mask) * 2; L[i] = this.d[k]; R[i] = this.d[k + 1]; }
    Atomics.store(h, 1, (r + n) | 0);
    return true;
  }
}
registerProcessor("ring", Ring);
