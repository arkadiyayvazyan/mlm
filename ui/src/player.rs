//! Gapless player. Every track streams in as the server's on-the-fly WAV (`/pcm`: AIFF byte-swapped, the
//! rest decoded server-side) and is kept as integer PCM. `tick` converts the next frames to stereo f32 and pushes them into a lock-free
//! SharedArrayBuffer ring that an AudioWorklet (worklet.js) drains on the audio thread. Tracks are
//! written back to back, so transitions are sample-exact and main-thread jank can't cause dropouts.
//! Seek/skip bumps the ring's flush seq and rewrites from the new spot.
use std::cell::{Ref, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use js_sys::{Atomics, Float32Array, Int32Array, Object, Reflect, SharedArrayBuffer, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    AbortController, AbortSignal, AudioContext, AudioContextOptions, AudioContextState, AudioWorkletNode,
    AudioWorkletNodeOptions, HtmlAnchorElement, ReadableStreamDefaultReader, RequestInit, Response,
};

use crate::pcm::{self, BINS};
use crate::Track;

const CAP: usize = 1 << 20; // ring frames, ~24 s at 44.1 kHz: rides out background-tab timer throttling (1 tick/s) and app-switch stalls
const CHUNK: usize = 8192; // frames converted per ring write
// ponytail: PCM kept whole in memory (~10 MB/min at 16-bit/44.1k stereo); window of cur + 2. Range-fetch on seek if phones choke
const PRELOAD: usize = 2; // tracks fetched ahead of the playing one

#[derive(Default)]
pub struct Loaded {
    pub rate: u32, // Hz, 0 = header not in yet
    nch: usize,
    bps: usize, // bytes per sample
    bytes: Vec<u8>, // interleaved LE integer PCM
    pub frames: usize, // total, known from the header before data arrives
    pub loaded: usize, // frames in `bytes`
    pub peaks: Vec<f32>, // BINS entries 0..1
    done: bool, // fetch ended (ok or not); frames == loaded from then on
}

impl Loaded {
    pub fn dur(&self) -> f64 {
        if self.rate > 0 { self.frames as f64 / self.rate as f64 } else { 0.0 }
    }
    /// The whole track as a WAV file once fully loaded (to share an AIFF, which Android's share sheet won't take).
    pub fn wav(&self) -> Option<Vec<u8>> {
        let len = self.loaded * self.nch * self.bps;
        (self.done && self.rate > 0).then(|| [&pcm::wav_header(self.rate, self.nch, self.bps, self.loaded)[..], &self.bytes[..len]].concat())
    }
    fn append(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        self.grew();
    }
    /// A network chunk, copied straight from JS into `bytes`: the one copy of a track's ~70 MB this makes.
    fn append_js(&mut self, chunk: &Uint8Array) {
        let (len, n) = (self.bytes.len(), chunk.length() as usize);
        self.bytes.reserve(n);
        chunk.copy_to_uninit(&mut self.bytes.spare_capacity_mut()[..n]);
        // SAFETY: copy_to_uninit initialized exactly these n bytes
        unsafe { self.bytes.set_len(len + n) };
        self.grew();
    }
    fn grew(&mut self) {
        let from = self.loaded;
        self.loaded = (self.bytes.len() / (self.bps * self.nch)).min(self.frames);
        pcm::update_peaks(&mut self.peaks, &self.bytes, self.bps, self.nch, from, self.loaded, self.frames);
    }
}

struct Slot {
    q: usize, // queue index
    id: u64,
    l: Rc<RefCell<Loaded>>,
    abort: AbortController,
}

/// A run of one track in the ring: queue index, ring frame it starts at, first track frame written there.
struct Seg {
    q: usize,
    at: i32,
    from: usize,
}

/// Main-thread end of the ring. Header layout is documented in worklet.js.
struct Ring {
    sab: SharedArrayBuffer,
    h: Int32Array,
    d: Float32Array,
    w: i32,
}

impl Ring {
    fn read(&self) -> i32 {
        Atomics::load(&self.h, 1).unwrap()
    }
    fn write(&mut self, stereo: &[f32]) {
        let n = stereo.len() / 2;
        let at = self.w as u32 as usize & (CAP - 1);
        let first = n.min(CAP - at);
        self.d.subarray((at * 2) as u32, ((at + first) * 2) as u32).copy_from(&stereo[..first * 2]);
        self.d.subarray(0, ((n - first) * 2) as u32).copy_from(&stereo[first * 2..]);
        self.w = self.w.wrapping_add(n as i32);
        Atomics::store(&self.h, 0, self.w).unwrap();
    }
    /// Make the worklet drop everything queued; data written from now on plays next.
    fn flush(&self) {
        Atomics::store(&self.h, 2, self.w).unwrap();
        Atomics::add(&self.h, 3, 1).unwrap();
    }
}

pub struct Player {
    pub queue: Vec<Track>,
    ring: Option<Ring>, // None: page isn't cross-origin isolated, no SharedArrayBuffer
    ctx: Option<AudioContext>,
    slots: Vec<Slot>, // the playing track and PRELOAD after it
    segs: VecDeque<Seg>, // what's in the ring, oldest first; segs[0] is playing
    fill: (usize, usize), // producer position: queue index, next track frame to write
}

impl Player {
    pub fn new() -> Self {
        let isolated = Reflect::get(&js_sys::global(), &"crossOriginIsolated".into()).is_ok_and(|v| v.is_truthy());
        let ring = isolated.then(|| {
            let sab = SharedArrayBuffer::new((16 + CAP * 8) as u32);
            let h = Int32Array::new_with_byte_offset_and_length(&sab, 0, 4);
            let d = Float32Array::new_with_byte_offset(&sab, 16);
            Ring { sab, h, d, w: 0 }
        });
        Self { queue: vec![], ring, ctx: None, slots: vec![], segs: VecDeque::new(), fill: (0, 0) }
    }

    pub fn supported(&self) -> bool {
        self.ring.is_some()
    }
    pub fn i(&self) -> Option<usize> {
        self.segs.front().map(|s| s.q)
    }
    pub fn track(&self) -> Option<&Track> {
        self.queue.get(self.i()?)
    }
    fn slot(&self, q: usize) -> Option<&Slot> {
        self.slots.iter().find(|s| s.q == q)
    }
    pub fn cur(&self) -> Option<Ref<'_, Loaded>> {
        Some(self.slot(self.i()?)?.l.borrow())
    }
    pub fn dur(&self) -> f64 {
        self.cur().map_or(0.0, |l| l.dur())
    }
    pub fn pos(&self) -> f64 {
        let (Some(s), Some(ring), Some(l)) = (self.segs.front(), &self.ring, self.cur()) else { return 0.0 };
        if l.rate == 0 {
            return 0.0;
        }
        let played = ring.read().wrapping_sub(s.at).max(0) as usize; // negative until the worklet sees a flush
        ((s.from + played) as f64 / l.rate as f64).min(l.dur())
    }
    fn ended(&self) -> bool {
        let (Some(ring), Some(s)) = (&self.ring, self.slot(self.fill.0)) else { return true };
        let l = s.l.borrow();
        ring.read() == ring.w && l.done && self.fill.1 >= l.loaded && self.fill.0 + 1 >= self.queue.len()
    }
    pub fn playing(&self) -> bool {
        self.ctx.as_ref().is_some_and(|c| c.state() == AudioContextState::Running) && !self.ended()
    }

    pub fn play(&mut self, queue: Vec<Track>, i: usize) {
        self.queue = queue;
        self.start(i, 0.0);
    }
    pub fn next(&mut self) {
        self.start(self.i().map_or(0, |i| i + 1), 0.0);
    }
    pub fn prev(&mut self) {
        match self.i() {
            Some(i) if self.pos() > 3.0 => self.start(i, 0.0),
            Some(i) if i > 0 => self.start(i - 1, 0.0),
            _ => {}
        }
    }
    pub fn seek(&mut self, sec: f64) {
        if let Some(i) = self.i() {
            self.start(i, sec);
        }
    }
    pub fn skip(&mut self, s: f64) {
        self.seek((self.pos() + s).clamp(0.0, self.dur()));
    }
    pub fn toggle(&self) {
        if let Some(c) = &self.ctx {
            let _ = if c.state() == AudioContextState::Running { c.suspend() } else { c.resume() };
        }
    }
    pub fn set_playing(&self, on: bool) {
        if let Some(c) = &self.ctx {
            let _ = if on { c.resume() } else { c.suspend() };
        }
    }
    pub fn download(&self) {
        let Some(t) = self.track() else { return };
        let doc = web_sys::window().unwrap().document().unwrap();
        let a: HtmlAnchorElement = doc.create_element("a").unwrap().unchecked_into();
        a.set_href(&format!("/api/tracks/{}/file", t.id));
        a.set_download(t.rel.rsplit('/').next().unwrap_or(&t.rel));
        a.click();
    }

    /// Play queue[i] from `offset` seconds. Reuses any slot already loading for the new window (seek, next, prev).
    fn start(&mut self, i: usize, offset: f64) {
        let Some(t) = self.queue.get(i) else { return };
        let Some(sab) = self.ring.as_ref().map(|r| r.sab.clone()) else { return };
        let mut old = std::mem::take(&mut self.slots);
        let known = old.iter().find(|s| s.id == t.id).map(|s| s.l.borrow().rate).filter(|r| *r > 0);
        let rate = known.unwrap_or(t.rate);
        // the ctx runs at the track's rate: the worklet copies samples 1:1, nothing resamples
        if let Some(c) = self.ctx.take_if(|c| rate > 0 && c.sample_rate() as u32 != rate) {
            let _ = c.close();
        }
        let ctx = self.ctx.get_or_insert_with(|| new_ctx(rate, &sab)).clone();
        if ctx.state() == AudioContextState::Suspended {
            let _ = ctx.resume();
        }
        for q in i..(i + 1 + PRELOAD).min(self.queue.len()) {
            let tr = &self.queue[q];
            let mut s = match old.iter().position(|s| s.id == tr.id) {
                Some(k) => old.swap_remove(k),
                None if q == i => open(tr),
                None => continue, // preloads start once this track is in: see tick
            };
            s.q = q;
            self.slots.push(s);
        }
        for s in old {
            s.abort.abort(); // ponytail: prev() refetches the track we just left
        }
        let ring = self.ring.as_ref().unwrap();
        ring.flush();
        let from = (offset * self.slot(i).unwrap().l.borrow().rate as f64).round() as usize;
        self.segs = [Seg { q: i, at: ring.w, from }].into();
        self.fill = (i, from);
        self.tick();
    }

    /// ponytail: temporary dropout probe, reports to the Pi's journal via /api/log; delete once the app-switch gap is understood
    pub fn probe(&self, last: &mut Option<(f64, i32, i32, bool)>) {
        let (Some(ring), Some(ctx)) = (&self.ring, &self.ctx) else { return };
        let (now, r, rate) = (js_sys::Date::now(), ring.read(), ctx.sample_rate() as f64);
        let buf = ring.w.wrapping_sub(r);
        let hidden = web_sys::window().unwrap().document().unwrap().hidden();
        let log = |m: String| web_sys::window().unwrap().navigator().send_beacon_with_opt_str("/api/log", Some(&m));
        if let Some((t, r0, b0, h0)) = *last {
            let (dt, played) = ((now - t) / 1000.0, r.wrapping_sub(r0) as f64 / rate);
            if hidden != h0 {
                let _ = log(format!("hidden {hidden}, buffered {:.2}s, ctx {:?}", buf as f64 / rate, ctx.state()));
            }
            if self.playing() && dt - played > 0.15 {
                let _ = log(format!("stall {:.2}s of {dt:.2}s, buffered {:.2}s -> {:.2}s, hidden {hidden}, ctx {:?}",
                    dt - played, b0 as f64 / rate, buf as f64 / rate, ctx.state()));
            }
        }
        *last = Some((now, r, buf, hidden));
    }

    /// Called every 50 ms: top the ring up, track what the worklet has played, keep the preload window.
    pub fn tick(&mut self) {
        let (Some(ring), Some(ctx)) = (&mut self.ring, self.ctx.clone()) else { return };
        // at most ~1 s of audio converted per 50 ms tick: filling the whole ring at once stalls a phone's UI
        let mut free = (CAP - ring.w.wrapping_sub(ring.read()) as usize).min(CHUNK * 6);
        let mut out = Vec::with_capacity(CHUNK * 2);
        let mut rate_change = false;
        while free > 0 {
            let (q, f) = self.fill;
            let Some(s) = self.slots.iter().find(|s| s.q == q) else { break };
            let l = s.l.borrow();
            if f < l.loaded {
                let (n, fb) = ((l.loaded - f).min(free).min(CHUNK), l.bps * l.nch);
                out.clear();
                pcm::to_stereo(&l.bytes[f * fb..(f + n) * fb], l.bps, l.nch, &mut out);
                ring.write(&out);
                self.fill.1 += n;
                free -= n;
                continue;
            }
            if !l.done {
                break; // waiting for the network
            }
            let Some(next) = self.slots.iter().find(|s| s.q == q + 1) else { break };
            let nl = next.l.borrow();
            if nl.rate == 0 && !nl.done {
                break; // header not in yet
            }
            if nl.rate != 0 && nl.rate != ctx.sample_rate() as u32 {
                rate_change = true; // let the ring drain, then restart at the new rate below
                break;
            }
            self.segs.push_back(Seg { q: q + 1, at: ring.w, from: 0 }); // a failed fetch is a 0-frame seg: skipped
            self.fill = (q + 1, 0);
        }
        let (r, w) = (ring.read(), ring.w);
        if rate_change && r == w {
            return self.start(self.fill.0 + 1, 0.0);
        }
        while self.segs.len() > 1 && r.wrapping_sub(self.segs[1].at) >= 0 {
            self.segs.pop_front();
        }
        let Some(q) = self.segs.front().map(|s| s.q) else { return };
        self.slots.retain(|s| s.q >= q);
        // the playing track streams alone until it's all in or LEAD s past the ring's write point (full bandwidth
        // for its start, and on a phone the main thread isn't copying three tracks at once); waiting for all of it
        // left next/skip with nothing preloaded for the first ~half minute of every track
        const LEAD: usize = 30;
        let fill = self.fill.1;
        if !self.slot(q).is_some_and(|s| { let l = s.l.borrow(); l.done || l.loaded >= fill + LEAD * l.rate as usize }) {
            return;
        }
        for k in q..(q + 1 + PRELOAD).min(self.queue.len()) {
            if self.slot(k).is_none() {
                let s = open(&self.queue[k]);
                self.slots.push(Slot { q: k, ..s });
            }
        }
    }
}

fn new_ctx(rate: u32, sab: &SharedArrayBuffer) -> AudioContext {
    let o = AudioContextOptions::new();
    if rate > 0 {
        o.set_sample_rate(rate as f32);
    }
    let ctx = AudioContext::new_with_context_options(&o).unwrap();
    let (c, sab) = (ctx.clone(), sab.clone());
    spawn_local(async move {
        let r: Result<(), JsValue> = async {
            JsFuture::from(c.audio_worklet()?.add_module("/worklet.js")?).await?;
            let o = AudioWorkletNodeOptions::new();
            let p = Object::new();
            Reflect::set(&p, &"sab".into(), &sab)?;
            o.set_processor_options(Some(&p));
            o.set_output_channel_count(&js_sys::Array::of1(&2.into()));
            AudioWorkletNode::new_with_options(&c, "ring", &o)?.connect_with_audio_node(&c.destination())?;
            Ok(())
        }
        .await;
        if let Err(e) = r {
            web_sys::console::error_2(&"audio worklet:".into(), &e);
        }
    });
    ctx
}

fn open(t: &Track) -> Slot {
    let abort = AbortController::new().unwrap();
    let l = Rc::new(RefCell::new(Loaded { peaks: vec![0.0; BINS], ..Default::default() }));
    let (l2, signal) = (l.clone(), abort.signal());
    let url = format!("/api/tracks/{}/pcm", t.id);
    spawn_local(async move {
        let _ = stream_wav(&url, &signal, &l2).await;
        let mut l = l2.borrow_mut();
        l.frames = l.loaded; // truncated / failed / aborted: play what arrived
        l.done = true;
    });
    Slot { q: 0, id: t.id, l, abort }
}

async fn fetch(url: &str, signal: &AbortSignal) -> Result<Response, JsValue> {
    let init = RequestInit::new();
    init.set_signal(Some(signal));
    let res: Response = JsFuture::from(web_sys::window().unwrap().fetch_with_str_and_init(url, &init)).await?.dyn_into()?;
    if res.ok() { Ok(res) } else { Err(format!("fetch {url}: {}", res.status()).into()) }
}

/// The server's WAV: fixed 44-byte header, then PCM appended as it arrives.
async fn stream_wav(url: &str, signal: &AbortSignal, l: &RefCell<Loaded>) -> Result<(), JsValue> {
    let body = fetch(url, signal).await?.body().ok_or("no body")?;
    let reader: ReadableStreamDefaultReader = body.get_reader().dyn_into()?;
    let mut head = vec![];
    loop {
        let r = JsFuture::from(reader.read()).await?;
        if signal.aborted() {
            return Err("aborted".into());
        }
        if Reflect::get(&r, &"done".into())?.is_truthy() {
            return Ok(());
        }
        // already a Uint8Array: `Uint8Array::new` on it would copy the whole chunk, `to_vec` a second time
        let chunk: Uint8Array = Reflect::get(&r, &"value".into())?.unchecked_into();
        let mut l = l.borrow_mut();
        if l.rate > 0 {
            l.append_js(&chunk);
            continue;
        }
        head.extend(chunk.to_vec());
        if head.len() < 44 {
            continue;
        }
        let u16_at = |i: usize| u16::from_le_bytes([head[i], head[i + 1]]) as usize;
        let u32_at = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
        let (nch, bps) = (u16_at(22), u16_at(34) / 8);
        if nch == 0 || !(2..=4).contains(&bps) {
            return Err("bad wav header".into());
        }
        let frames = u32_at(40) as usize / (bps * nch);
        (l.nch, l.bps, l.rate, l.frames) = (nch, bps, u32_at(24), frames);
        l.bytes.reserve_exact(frames * bps * nch);
        l.append(&head[44..]);
    }
}
