import { LitElement, html, css } from "lit";
import { customElement, state } from "lit/decorators.js";
import { repeat } from "lit/directives/repeat.js";

type Track = {
  id: number; title: string; artist: string; album: string;
  track_no: number; duration_ms: number; ext: string;
};

const url = (t: Track) => `/api/tracks/${t.id}/stream`;
const fmt = (s: number) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;

/** Double-buffered player: `cur` plays, `nxt` has the following track preloaded. */
@customElement("mlm-player")
export class Player extends LitElement {
  static styles = css`
    :host { display: grid; grid-template-columns: auto 1fr auto; gap: 12px; align-items: center;
            padding: 10px 16px; border-top: 1px solid color-mix(in srgb, currentColor 20%, transparent);
            background: Canvas; }
    .meta { overflow: hidden; white-space: nowrap; text-overflow: ellipsis; }
    input[type=range] { width: 100%; }
    button { font: inherit; padding: 4px 10px; }
  `;
  @state() queue: Track[] = [];
  @state() i = -1;
  @state() playing = false;
  @state() t = 0;
  @state() dur = 0;

  private cur = new Audio();
  private nxt = new Audio();

  constructor() {
    super();
    for (const a of [this.cur, this.nxt]) a.preload = "auto";
    this.hook(this.cur);
  }

  private hook(a: HTMLAudioElement) {
    a.ontimeupdate = () => { if (a === this.cur) { this.t = a.currentTime; this.dur = a.duration || 0; } };
    a.onended = () => { if (a === this.cur) this.next(); };
    a.onplay = () => { if (a === this.cur) this.playing = true; };
    a.onpause = () => { if (a === this.cur) this.playing = false; };
  }

  get track() { return this.queue[this.i]; }

  play(queue: Track[], i: number) {
    this.queue = queue;
    this.load(i);
  }

  private load(i: number) {
    if (i < 0 || i >= this.queue.length) return;
    this.i = i;
    const t = this.queue[i];
    // if the preloaded element already holds this track, just swap
    if (this.nxt.src.endsWith(url(t))) {
      this.cur.pause();
      [this.cur, this.nxt] = [this.nxt, this.cur];
    } else {
      this.cur.src = url(t);
    }
    this.hook(this.cur); this.hook(this.nxt);
    this.cur.currentTime = 0;
    this.cur.play();
    const n = this.queue[i + 1];
    if (n) this.nxt.src = url(n); else this.nxt.removeAttribute("src");
    this.dispatchEvent(new CustomEvent("track-change", { detail: t.id }));
    // ponytail: ~50 ms gap between tracks; move to Web Audio scheduling if gapless matters
  }

  next() { this.load(this.i + 1); }
  prev() { this.cur.currentTime > 3 ? (this.cur.currentTime = 0) : this.load(this.i - 1); }
  toggle() { this.cur.paused ? this.cur.play() : this.cur.pause(); }
  seek(e: Event) { this.cur.currentTime = +(e.target as HTMLInputElement).value; }

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
        <input type="range" min="0" max=${this.dur} step="0.1" .value=${String(this.t)} @input=${this.seek}>
      </div>
      <div>${fmt(this.t)} / ${fmt(this.dur)}</div>`;
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
