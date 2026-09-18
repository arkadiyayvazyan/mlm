import { expect, test } from "bun:test";
import { BINS, pcmToFloat, updatePeaks } from "./pcm";

test("16-bit stereo frames straddling a chunk boundary", () => {
  // 3 frames, 2 channels, 16-bit LE: L/R pairs
  const samples = [1000, -2000, 32767, -32768, 0, 16384];
  const bytes = new Uint8Array(12);
  const dv = new DataView(bytes.buffer);
  samples.forEach((s, i) => dv.setInt16(i * 2, s, true));

  const out = [new Float32Array(3), new Float32Array(3)];
  const peaks = new Float32Array(BINS);
  let loaded = 0, carry = new Uint8Array(0);
  for (const chunk of [bytes.subarray(0, 5), bytes.subarray(5)]) {  // odd split: frame 2 straddles
    const buf = new Uint8Array(carry.length + chunk.length);
    buf.set(carry); buf.set(chunk, carry.length);
    const n = pcmToFloat(buf, 16, 2, out, loaded);
    carry = buf.subarray(n * 4).slice();
    updatePeaks(peaks, out, loaded, loaded + n, 3);
    loaded += n;
  }
  expect(loaded).toBe(3);
  expect(carry.length).toBe(0);
  expect(Array.from(out[0])).toEqual([1000 / 32768, 32767 / 32768, 0].map(Math.fround));
  expect(Array.from(out[1])).toEqual([-2000 / 32768, -1, 16384 / 32768].map(Math.fround));
  // bins: frame f -> floor(f*1000/3) = 0, 333, 666
  expect(peaks[0]).toBeCloseTo(2000 / 32768, 6);
  expect(peaks[333]).toBe(1);
  expect(peaks[666]).toBeCloseTo(0.5, 6);
  expect(peaks.reduce((a, b) => a + b, 0)).toBeCloseTo(2000 / 32768 + 1 + 0.5, 6);
});

test("24-bit sign extension", () => {
  const out = [new Float32Array(2)];
  expect(pcmToFloat(new Uint8Array([0xff, 0xff, 0xff, 0x00, 0x00, 0x80]), 24, 1, out, 0)).toBe(2);
  expect(out[0][0]).toBeCloseTo(-1 / 8388608, 9);
  expect(out[0][1]).toBe(-1);
});
