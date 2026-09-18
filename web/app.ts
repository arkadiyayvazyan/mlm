import { LitElement, html, css } from "lit";
import { customElement, property, state } from "lit/decorators.js";
import { repeat } from "lit/directives/repeat.js";
import { load, type Loaded } from "./pcm";

type Track = {
  id: number; title: string; artist: string; album: string;
  track_no: number; duration_ms: number; rate: number; ext: string;
};

const url = (t: Track) => `/api/tracks/${t.id}/stream`;
const fmt = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

type Slot = { id: number; l: Loaded; abort: AbortController; scheduled: number };

const LOOKAHEAD = 3;                     // seconds of audio kept scheduled ahead of the clock
const CHUNK = 1;                         // seconds per AudioBufferSourceNode
const dur = (l: Loaded) => l.frames && l.rate ? l.frames / l.rate : 0;   // 0 = header not in yet

/**
 * Gapless player: decoded PCM (`cur` playing, `nxt` preloaded) is cut into 1 s AudioBuffers and
 * started on the AudioContext clock. `t0` is the ctx time of `cur` frame 0; `nxt` starts exactly
 * at `t0 + cur.frames / cur.rate`, so the transition is sample-accurate.
 */
// ponytail: Float32 in memory ≈ 10 MB/min stereo; cur+nxt of 10-min tracks ≈ 200 MB. Keep Int16 and convert on schedule if a phone chokes
@customElement("mlm-player")
export class Player extends LitElement {
  static styles = css`
    :host { display: grid; grid-template-columns: auto 1fr auto; gap: 12px; align-items: center;
            padding: 10px 16px; border-top: 1px solid color-mix(in srgb, currentColor 20%, transparent);
            background: Canvas; }
    .meta { overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
    button { font: inherit; padding: 4px 10px; }
  `;
  @state() queue: Track[] = [];
  @state() i = -1;
  @state() playing = false;
  @state() t = 0;
  @state() dur = 0;
  @state() loaded = 0;   // fraction of cur decoded

  private ctx?: AudioContext;
  private cur?: Slot;
  private nxt?: Slot;
  private t0 = 0;
  private nodes: AudioBufferSourceNode[] = [];
  private timer = 0;
  private raf = 0;

  connectedCallback() {
    super.connectedCallback();
    this.timer = window.setInterval(() => this.tick(), 250);
    window.addEventListener("keydown", this.key);
  }
  disconnectedCallback() {
    super.disconnectedCallback();
    clearInterval(this.timer); cancelAnimationFrame(this.raf);
    window.removeEventListener("keydown", this.key);
  }

  // vim-style: j/k/space = play/pause, h/l = -/+ 1 min, ? = help. Ignored while typing in an input.
  private key = (e: KeyboardEvent) => {
    if ((e.target as HTMLElement).tagName === "INPUT" || e.ctrlKey || e.metaKey || e.altKey) return;
    const help = this.renderRoot.querySelector("dialog")!;
    const act: Record<string, () => void> = {
      j: () => this.toggle(), k: () => this.toggle(), " ": () => this.toggle(),
      h: () => this.skip(-60), l: () => this.skip(60),
      "?": () => help.open ? help.close() : help.showModal(),
    };
    if (act[e.key]) { e.preventDefault(); act[e.key](); }
  };

  get track() { return this.queue[this.i]; }
  get pos() { return this.ctx && this.cur ? Math.min(Math.max(this.ctx.currentTime - this.t0, 0), this.dur) : 0; }

  play(queue: Track[], i: number) {
    this.queue = queue;
    this.start(i);
  }
  next() { this.start(this.i + 1); }
  prev() { this.pos > 3 ? this.start(this.i, 0) : this.start(this.i - 1); }
  seek(sec: number) { if (this.cur) this.start(this.i, sec); }   // ponytail: no range re-fetch on seek; whole AIFF lands in ~2 s on LAN
  toggle() { const c = this.ctx; if (c) c.state === "running" ? c.suspend() : c.resume(); }
  skip(s: number) { if (this.cur) this.seek(Math.min(Math.max(this.pos + s, 0), this.dur)); }

  private open(t: Track): Slot {
    const abort = new AbortController();
    const s: Slot = { id: t.id, l: load(url(t), t.ext, this.ctx!, abort.signal), abort, scheduled: 0 };
    s.l.onProgress = () => this.tick();
    s.l.done.catch(() => {});   // abort / network error: nothing to play, nothing to do
    return s;
  }

  /** Rebuild the timeline so queue[i] plays from `offset` seconds. Reuses cur (seek/restart) and nxt (next). */
  private start(i: number, offset = 0) {
    const t = this.queue[i];
    if (!t) return;
    // ctx runs at the track's rate so chunk boundaries never go through the per-node resampler (clicks)
    if (this.ctx && t.rate && this.ctx.sampleRate !== t.rate) { this.ctx.close(); this.ctx = undefined; }
    const ctx = this.ctx ??= Object.assign(new AudioContext(t.rate ? { sampleRate: t.rate } : {}), { onstatechange: () => this.tick() });
    if (ctx.state === "suspended") ctx.resume();
    for (const n of this.nodes) { n.stop(); n.disconnect(); }
    this.nodes = [];
    const following = this.queue[i + 1];
    let cur = this.cur, nxt = this.nxt;
    if (cur?.id !== t.id) {
      cur?.abort.abort();   // ponytail: prev() refetches the track we just left
      cur = nxt?.id === t.id ? nxt : this.open(t);
      if (nxt === cur) nxt = undefined;
      this.dispatchEvent(new CustomEvent("track-change", { detail: t.id }));
    }
    if (nxt && nxt.id !== following?.id) { nxt.abort.abort(); nxt = undefined; }
    if (!nxt && following) nxt = this.open(following);
    this.cur = cur; this.nxt = nxt; this.i = i;
    this.t0 = ctx.currentTime - offset;
    cur.scheduled = Math.min(Math.round(offset * cur.l.rate), cur.l.frames);
    if (nxt) nxt.scheduled = 0;
    this.tick();
  }

  /** Scheduler: promote nxt when cur has run out, then keep LOOKAHEAD seconds of both queued on the ctx clock. */
  private tick() {
    const ctx = this.ctx;
    if (!ctx || !this.cur) return;
    const now = ctx.currentTime;
    let end = this.t0 + dur(this.cur.l);
    while (dur(this.cur.l) && now >= end && this.nxt) {   // loop: nxt may be shorter than one tick
      this.cur = this.nxt; this.t0 = end; this.i++;
      const n = this.queue[this.i + 1];
      this.nxt = n ? this.open(n) : undefined;
      this.dispatchEvent(new CustomEvent("track-change", { detail: this.cur.id }));
      end = this.t0 + dur(this.cur.l);
    }
    const c = this.cur;
    // nothing sounding and the next chunk is already due (cold start, seek past loaded): re-anchor instead of skipping
    if (!this.nodes.length && c.l.loaded > c.scheduled && this.t0 + c.scheduled / c.l.rate < now) {
      this.t0 = now - c.scheduled / c.l.rate;
      end = this.t0 + dur(c.l);
    }
    this.schedule(c, this.t0);
    if (this.nxt && dur(c.l)) this.schedule(this.nxt, end);
    this.dur = dur(c.l);
    this.t = this.pos;
    this.loaded = c.l.frames ? c.l.loaded / c.l.frames : 0;
    const ended = !this.nxt && this.dur > 0 && now >= end;
    this.playing = ctx.state === "running" && !ended;
    if (this.playing && !this.raf) this.raf = requestAnimationFrame(this.frame);
  }

  private frame = () => {
    this.t = this.pos;
    this.raf = this.playing ? requestAnimationFrame(this.frame) : 0;
  };

  /** Queue up to CHUNK-second buffers of slot `s` (whose frame 0 plays at ctx time `base`) within LOOKAHEAD. */
  private schedule(s: Slot, base: number) {
    const ctx = this.ctx!, l = s.l, now = ctx.currentTime;
    while (s.scheduled < l.loaded) {
      const at = base + s.scheduled / l.rate;
      if (at >= now + LOOKAHEAD) break;
      const n = Math.min(Math.round(CHUNK * l.rate), l.loaded - s.scheduled);
      if (at + n / l.rate <= now) { s.scheduled += n; continue; }   // entirely in the past (data came late): skip
      const buf = ctx.createBuffer(l.channels.length, n, l.rate);
      l.channels.forEach((ch, c) => buf.copyToChannel(ch.subarray(s.scheduled, s.scheduled + n) as Float32Array<ArrayBuffer>, c));
      const node = ctx.createBufferSource();
      node.buffer = buf;
      node.connect(ctx.destination);
      at < now ? node.start(now, now - at) : node.start(at);   // late chunk: start now, skipping what was missed
      node.onended = () => { const k = this.nodes.indexOf(node); if (k >= 0) this.nodes.splice(k, 1); };
      this.nodes.push(node);
      s.scheduled += n;
    }
  }

  render() {
    const t = this.track;
    return html`
      <div>
        <button @click=${this.prev}>⏮</button>
        <button @click=${this.toggle}>${this.playing ? "⏸" : "▶"}</button>
        <button @click=${this.next}>⏭</button>
      </div>
      <div class="meta">
        ${t ? html`<b>${t.title}</b> — ${t.artist}` : "nothing playing"}
        <mlm-wave .peaks=${this.cur?.l.peaks} .loadedFrac=${this.loaded} .progress=${this.dur ? this.t / this.dur : 0}
                  @seek=${(e: CustomEvent<number>) => this.seek(e.detail * this.dur)}></mlm-wave>
      </div>
      <div>${fmt(this.t)} / ${fmt(this.dur)}</div>
      <dialog><b>Keys</b><br>j / k / space — play / pause<br>h / l — back / forward 1 min<br>? — this help<br><small>Esc closes</small></dialog>`;
  }
}

/** Waveform seek bar: one mirrored bar per peak bin; played bars in the accent color, undecoded bars dimmed. */
@customElement("mlm-wave")
export class Wave extends LitElement {
  static styles = css`
    :host { display: block; height: 48px; cursor: pointer; touch-action: none; }
    canvas { display: block; width: 100%; height: 100%; color: AccentColor; }
    @supports not (color: AccentColor) { canvas { color: color-mix(in srgb, currentColor 80%, transparent); } }
  `;
  @property({ attribute: false }) peaks?: Float32Array;
  @property({ type: Number }) loadedFrac = 0;
  @property({ type: Number }) progress = 0;

  private ro = new ResizeObserver(() => this.draw());

  firstUpdated() { this.ro.observe(this); }
  disconnectedCallback() { super.disconnectedCallback(); this.ro.disconnect(); }
  updated() { this.draw(); }

  private draw() {
    const c = this.renderRoot.querySelector("canvas");
    if (!c) return;
    const dpr = devicePixelRatio || 1, w = this.clientWidth, h = this.clientHeight;
    if (!w || !h) return;
    if (c.width !== w * dpr || c.height !== h * dpr) { c.width = w * dpr; c.height = h * dpr; }
    const g = c.getContext("2d")!;
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, h);
    const p = this.peaks;
    if (!p) return;
    const fg = getComputedStyle(this).color, accent = getComputedStyle(c).color, bw = w / p.length;
    for (let b = 0; b < p.length; b++) {
      const f = b / p.length, played = f < this.progress;
      g.fillStyle = played ? accent : fg;
      g.globalAlpha = played ? 1 : f < this.loadedFrac ? 0.5 : 0.2;
      const bh = Math.max(1, p[b] * h);
      g.fillRect(b * bw, (h - bh) / 2, bw, bh);
    }
  }

  private seekAt(e: PointerEvent) {
    const r = this.getBoundingClientRect();
    this.dispatchEvent(new CustomEvent("seek", { detail: Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)) }));
  }

  render() {
    return html`<canvas
      @pointerdown=${(e: PointerEvent) => { (e.target as Element).setPointerCapture(e.pointerId); this.seekAt(e); }}
      @pointermove=${(e: PointerEvent) => { if (e.buttons) this.seekAt(e); }}></canvas>`;
  }
}

@customElement("mlm-app")
export class App extends LitElement {
  static styles = css`
    :host { display: grid; grid-template-rows: auto 1fr auto; height: 100vh; }
    input[type=search] { font: inherit; width: 100%; box-sizing: border-box; padding: 8px 12px; border: 0;
                         border-bottom: 1px solid color-mix(in srgb, currentColor 20%, transparent); }
    .list { overflow-y: auto; }
    .row { display: grid; grid-template-columns: 1fr 1fr 1fr 4em; gap: 8px; padding: 6px 16px; cursor: pointer; }
    .row:hover, .row.on { background: color-mix(in srgb, currentColor 10%, transparent); }
    .row > * { overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
    .row > :last-child { text-align: right; opacity: .6; }
  `;
  @state() tracks: Track[] = [];
  @state() q = "";
  @state() nowId = -1;

  async connectedCallback() {
    super.connectedCallback();
    this.tracks = await (await fetch("/api/tracks")).json();
  }

  get filtered() {
    const q = this.q.trim().toLowerCase();
    if (!q) return this.tracks;
    return this.tracks.filter(t => `${t.title} ${t.artist} ${t.album}`.toLowerCase().includes(q));
  }

  play(list: Track[], i: number) {
    this.nowId = list[i].id;
    (this.renderRoot.querySelector("mlm-player") as Player).play(list, i);
  }

  render() {
    const list = this.filtered;
    // ponytail: plain repeat; virtualize only if >20k rows lags
    return html`
      <input type="search" placeholder="search ${this.tracks.length} tracks" @input=${(e: Event) => this.q = (e.target as HTMLInputElement).value}>
      <div class="list">
        ${repeat(list, t => t.id, (t, i) => html`
          <div class="row ${t.id === this.nowId ? "on" : ""}" @click=${() => this.play(list, i)}>
            <span>${t.title}</span><span>${t.artist}</span><span>${t.album}</span>
            <span>${fmt(t.duration_ms / 1000)}</span>
          </div>`)}
      </div>
      <mlm-player @track-change=${(e: CustomEvent) => this.nowId = e.detail}></mlm-player>`;
  }
}
