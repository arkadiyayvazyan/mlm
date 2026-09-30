use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use eframe::egui::{
    self, pos2, text::LayoutJob, vec2, Align, Align2, Button, Color32, CornerRadius, FontId, Key, Label, Layout,
    DragValue, Modifiers, Painter, Rect, RichText, ScrollArea, Sense, TextEdit, TextFormat, Ui, Vec2,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{RequestInit, Response};

use crate::player::Player;
use crate::offline::Offline;
use crate::tags::{Op, Tags};
use crate::{fmt, Track};

const ROW: f32 = 26.0;
const NARROW: f32 = 600.0; // below this width (phones): two-line rows, touch-sized controls
const ICON: f32 = 20.0; // the offline-copy column before the filename
const OFFLINE_ICON: &str = "⬇"; // same arrow as the download button

pub struct App {
    tracks: Vec<Track>,
    tags: Tags,
    inbox: Rc<RefCell<Option<(Vec<Track>, Tags)>>>, // initial fetch lands here
    player: Rc<RefCell<Player>>,
    q: String,
    new_name: String,
    new_key: String,
    err: String,
    help: bool,
    shown_id: Option<u64>, // playing track as of last frame: scroll when it changes
    view: (f32, f32),      // list scroll offset and height as of last frame
    sort: Option<(usize, bool)>, // column, descending; None = library order
    analyzed: Rc<RefCell<Option<(u64, Result<u32, String>)>>>, // ctrl+a result lands here
    status: Option<(String, f64)>, // message and the egui time it disappears at
    note: Rc<RefCell<Option<String>>>, // status messages from async work (share)
    can_share: bool,                   // the browser can share files (Android / iOS share sheet)
    shared: Rc<RefCell<Option<(u64, web_sys::File)>>>, // last prepared share, reused by a second tap
    install: Rc<RefCell<Option<JsValue>>>, // stashed beforeinstallprompt event: an "install app" button is offered
    off: Offline,
    last_flush: f64, // egui time of the last retry of queued tag edits
    gen: u64,        // bumped whenever tracks or tags change: the filtered + sorted list is rebuilt only then
    bpm: (u32, u32), // BPM filter: center, ± range; kept while off so the closing row still shows them
    bpm_on: bool,    // hold the bpm column to turn on
    tags_open: bool,
    search_open: bool,
    focus_search: bool, // the search field takes focus once it's drawn
    list_key: (String, Option<(usize, bool)>, Option<(u32, u32)>, u64), // (query, sort, bpm, gen) the cached list was built for
    list_idx: Vec<usize>, // that list, as indices into `tracks`
    fps: Option<(f64, f64)>, // frame-time readout (tap the time): smoothed frame interval and ui() time, ms
}

/// GET a JSON API; `unreachable` is set when sw.js had to answer from its cache (the Pi is out of reach).
async fn get_json<T: serde::de::DeserializeOwned>(url: &str, unreachable: &std::cell::Cell<bool>) -> Option<T> {
    let res: Response = JsFuture::from(web_sys::window()?.fetch_with_str(url)).await.ok()?.dyn_into().ok()?;
    if res.headers().has("x-mlm-offline").unwrap_or(false) {
        unreachable.set(true);
    }
    serde_json::from_str(&JsFuture::from(res.text().ok()?).await.ok()?.as_string()?).ok()
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        cc.egui_ctx.all_styles_mut(|s| s.animation_time = 0.12); // panes slide open fast (egui default 0.2 s)
        let inbox = Rc::new(RefCell::new(None));
        let off = Offline::new(&cc.egui_ctx);
        let (ib, ctx, unreachable) = (inbox.clone(), cc.egui_ctx.clone(), off.unreachable.clone());
        spawn_local(async move {
            let (t, g) = (get_json("/api/tracks", &unreachable).await, get_json("/api/tags", &unreachable).await);
            *ib.borrow_mut() = Some((t.unwrap_or_default(), g.unwrap_or_default()));
            ctx.request_repaint();
        });
        // the scheduler runs off a timer, not the frame loop: egui doesn't paint while the tab is hidden
        let player = Rc::new(RefCell::new(Player::new()));
        let p = player.clone();
        // the browser offers installing: keep its event for our own button instead of its mini-infobar
        let install: Rc<RefCell<Option<JsValue>>> = Rc::default();
        let (i, ctx) = (install.clone(), cc.egui_ctx.clone());
        let offer = Closure::<dyn Fn(web_sys::Event)>::new(move |e: web_sys::Event| {
            e.prevent_default();
            *i.borrow_mut() = (e.type_() == "beforeinstallprompt").then(|| e.into());
            ctx.request_repaint();
        });
        for ev in ["beforeinstallprompt", "appinstalled"] {
            let _ = web_sys::window().unwrap().add_event_listener_with_callback(ev, offer.as_ref().unchecked_ref());
        }
        offer.forget();
        let mut media = crate::media::Media::new(&player);
        let mut probe = None;
        let tick = Closure::<dyn FnMut()>::new(move || {
            let mut p = p.borrow_mut();
            p.tick();
            media.sync(&p);
            p.probe(&mut probe);
        });
        web_sys::window()
            .unwrap()
            .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 50)
            .unwrap();
        tick.forget();
        Self {
            tracks: vec![], tags: Tags::default(), inbox, player, q: String::new(), new_name: String::new(),
            new_key: String::new(), err: String::new(), help: false, shown_id: None, view: (0.0, 0.0), sort: None,
            analyzed: Rc::default(), status: None, note: Rc::default(), can_share: can_share(), shared: Rc::default(), install,
            off, last_flush: 0.0, gen: 0, bpm: (120, 4), bpm_on: false, tags_open: false, search_open: false, focus_search: false, list_key: (String::new(), None, None, u64::MAX), list_idx: vec![], fps: None,
        }
    }

    /// A tag edit: applied here at once, queued, and sent to the Pi (now, or when it's reachable again).
    fn edit(&mut self, op: Op) {
        self.gen += 1;
        self.tags.apply(&op);
        self.off.queue(op);
        self.off.flush();
    }

    /// Tags without a color (new, or from before colors) get one, stored on the server like any edit.
    fn color_new_tags(&mut self) {
        let before: Vec<String> = self.tags.keys.keys().filter(|n| !self.tags.hues.contains_key(*n)).cloned().collect();
        if self.tags.color_missing(js_sys::Math::random) {
            self.gen += 1;
            for name in before {
                let (key, hue) = (self.tags.keys[&name].clone(), self.tags.hues[&name]);
                self.off.queue(Op::Define { name, key, hue });
            }
            self.off.flush();
        }
    }

    fn now_rel(&self) -> Option<String> {
        self.player.borrow().track().map(|t| t.rel.clone())
    }

    /// Detect the playing track's BPM on the server, which writes it into the file's tag.
    fn analyze(&mut self, ctx: &egui::Context) {
        let Some(t) = self.player.borrow().track().cloned() else { return };
        self.status = Some((format!("analyzing {}…", filename(&t)), f64::INFINITY));
        let (inbox, ctx) = (self.analyzed.clone(), ctx.clone());
        spawn_local(async move {
            let r = async {
                let init = RequestInit::new();
                init.set_method("POST");
                let url = format!("/api/tracks/{}/analyze", t.id);
                let res: Response = JsFuture::from(web_sys::window().unwrap().fetch_with_str_and_init(&url, &init)).await?.dyn_into()?;
                let body = JsFuture::from(res.text()?).await?.as_string().unwrap_or_default();
                if !res.ok() {
                    return Err(JsValue::from(body));
                }
                #[derive(serde::Deserialize)]
                struct R { bpm: u32 }
                serde_json::from_str::<R>(&body).map(|r| r.bpm).map_err(|e| e.to_string().into())
            }
            .await;
            *inbox.borrow_mut() = Some((t.id, r.map_err(|e| e.as_string().unwrap_or_else(|| "network error".into()))));
            ctx.request_repaint();
        });
    }

    /// Hand the playing track to the share sheet. Android won't take AIFF, so AIFF goes as a WAV built from the PCM
    /// already in memory (instant, lossless, no tags); other formats are fetched as the original file. A fetch can
    /// outlast the tap's user activation: then the prepared file waits and the next tap shares it.
    fn share(&mut self) {
        let Some(t) = self.player.borrow().track().cloned() else { return };
        if let Some((id, f)) = self.shared.borrow().clone() {
            if id == t.id {
                return share_now(f, t.title.clone(), self.note.clone());
            }
        }
        let name = filename(&t).to_owned();
        if shares_original(&t) {
            self.status = Some(("preparing…".into(), f64::INFINITY));
            let (shared, note) = (self.shared.clone(), self.note.clone());
            spawn_local(async move {
                let r: Result<web_sys::File, JsValue> = async {
                    let res: Response = JsFuture::from(web_sys::window().unwrap().fetch_with_str(&format!("/api/tracks/{}/file", t.id))).await?.dyn_into()?;
                    let blob: web_sys::Blob = JsFuture::from(res.blob()?).await?.dyn_into()?;
                    let o = web_sys::FilePropertyBag::new();
                    o.set_type(&blob.type_());
                    web_sys::File::new_with_blob_sequence_and_options(&js_sys::Array::of1(&blob), &name, &o)
                }
                .await;
                match r {
                    Ok(f) => {
                        *shared.borrow_mut() = Some((t.id, f.clone()));
                        *note.borrow_mut() = Some(String::new()); // clears "preparing…"
                        share_now(f, t.title, note);
                    }
                    Err(_) => *note.borrow_mut() = Some("share: download failed".into()),
                }
            });
            return;
        }
        let Some(wav) = self.player.borrow().cur().and_then(|l| l.wav()) else {
            let pct = self.player.borrow().cur().map_or(0, |l| l.loaded * 100 / l.frames.max(1));
            *self.note.borrow_mut() = Some(format!("still loading ({pct}%): share works once the whole track is in"));
            return;
        };
        let o = web_sys::FilePropertyBag::new();
        o.set_type("audio/wav");
        let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
        let f = web_sys::File::new_with_u8_array_sequence_and_options(&js_sys::Array::of1(&js_sys::Uint8Array::from(&wav[..])), &format!("{stem}.wav"), &o).unwrap();
        *self.shared.borrow_mut() = Some((t.id, f.clone()));
        share_now(f, t.title, self.note.clone());
    }

    /// vim-style: space = play/pause, j/k = next/prev, h/l = -/+ 1 min, ? = help, tag keys toggle tags;
    /// ctrl+d = download, ctrl+a = analyze BPM (the browser's bookmark / select-all are cancelled in main.rs).
    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return; // typing: ctrl+a selects the text instead
        }
        let (dl, an) = ctx.input_mut(|i| (i.consume_key(Modifiers::COMMAND, Key::D), i.consume_key(Modifiers::COMMAND, Key::A)));
        if dl {
            self.player.borrow().download();
        }
        if an {
            self.analyze(ctx);
        }
        let texts: Vec<String> = ctx.input(|i| {
            if i.modifiers.alt || i.modifiers.command { return vec![] }
            i.events.iter().filter_map(|e| if let egui::Event::Text(t) = e { Some(t.clone()) } else { None }).collect()
        });
        for k in texts {
            let mut p = self.player.borrow_mut();
            match k.as_str() {
                " " => {
                    ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Space)); // not also a click on a focused button
                    p.toggle()
                }
                "j" => p.next(),
                "k" => p.prev(),
                "h" => p.skip(-60.0),
                "l" => p.skip(60.0),
                "?" => self.help = !self.help,
                "/" => (self.search_open, self.focus_search) = (true, true),
                _ => {
                    drop(p);
                    let name = self.tags.keys.iter().find(|(_, v)| **v == k).map(|(n, _)| n.clone());
                    if let (Some(name), Some(rel)) = (name, self.now_rel()) {
                        let on = !self.tags.has(&rel, &name);
                        self.edit(Op::Tag { rel, name, on });
                    }
                }
            }
        }
    }

    fn tags_panel(&mut self, ui: &mut Ui) {
        let now = self.now_rel();
        let narrow = ui.available_width() < NARROW;
        let h = if narrow { 36.0 } else { 0.0 }; // thumb-sized tags on phones
        // right-aligned, in thumb reach; right-to-left adds the widgets in reverse: reads tags … new tag, key, ➕
        // sized to its rows: in the full available rect a wrapping layout takes the panel's whole height
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 0.0), Layout::right_to_left(Align::Min).with_main_wrap(true), |ui| {
            if !self.err.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &self.err);
            }
            let add = ui.button(label(narrow, "➕", "add")).clicked();
            let b = ui.add(TextEdit::singleline(&mut self.new_key).hint_text("key").char_limit(1).desired_width(30.0));
            let a = ui.add(TextEdit::singleline(&mut self.new_name).hint_text("new tag").desired_width(100.0));
            if a.changed() || b.changed() {
                self.err.clear();
            }
            let enter = (a.lost_focus() || b.lost_focus()) && ui.input(|i| i.key_pressed(Key::Enter));
            if add || enter {
                match self.tags.add(&self.new_name, &self.new_key) {
                    Ok(()) => {
                        self.color_new_tags(); // queues the new tag's Define (key + hue)
                        self.new_name.clear();
                        self.new_key.clear();
                    }
                    Err(e) => self.err = e,
                }
            }
            for (n, k) in self.tags.keys.clone().into_iter().rev() {
                let on = now.as_ref().is_some_and(|r| self.tags.has(r, &n));
                // always full pastel; a bright outline marks tags on the playing track
                let stroke = if on { egui::Stroke::new(2.0, ui.visuals().strong_text_color()) } else { egui::Stroke::NONE };
                let b = ui.add(Button::new(RichText::new(format!("{n}  {k}")).color(INK)).fill(tag_color(&self.tags, &n)).stroke(stroke).min_size(vec2(0.0, h)));
                let b = b.on_hover_text(format!("press {k} to toggle on the playing track; hold (right-click) to delete"));
                if let (true, Some(rel)) = (b.clicked(), now.clone()) {
                    self.edit(Op::Tag { on: !on, rel, name: n.clone() });
                }
                let confirm = |m: &str| web_sys::window().unwrap().confirm_with_message(m).unwrap_or(false);
                if b.secondary_clicked() && confirm(&format!("Delete \"{n}\" from all tracks?")) {
                    self.edit(Op::Remove { name: n.clone() });
                }
            }
        });
    }

    fn player_bar(&mut self, ui: &mut Ui) {
        let player = self.player.clone();
        let mut p = player.borrow_mut();
        if !p.supported() {
            ui.colored_label(ui.visuals().error_fg_color, "playback needs a cross-origin isolated page: open over HTTPS or localhost");
            return;
        }
        let time = format!("{} / {}", fmt(p.pos()), fmt(p.dur()));
        if ui.available_width() < NARROW {
            // phones: time beside the title, and the Ctrl+ shortcuts as buttons next to the controls
            if now_playing(ui, &mut p, Some(time)) {
                self.fps = if self.fps.is_some() { None } else { Some((0.0, 0.0)) };
            }
            let (mut share, mut bpm) = (false, false);
            // all right-aligned, transport at the edge under the thumb: 📤 💓 ⬇ ⏮ ⏵ ⏭
            ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                controls(ui, &mut p, 22.0, vec2(56.0, 40.0));
                let b = |s: &str| Button::new(RichText::new(s).size(15.0)).min_size(vec2(40.0, 40.0));
                if ui.add(b("⬇")).clicked() {
                    p.download();
                }
                let a = ui.add(b(""));
                bars(ui, &a); // detect BPM: the logo's waveform, three bars
                bpm = a.clicked();
                // dimmed until an AIFF is fully in (its WAV is built from the PCM); a tap then says how far along it is
                let ready = p.track().is_some_and(shares_original) || p.cur().is_some_and(|l| l.frames > 0 && l.loaded >= l.frames);
                let dim = ui.visuals().weak_text_color();
                share = self.can_share && ui.add(if ready { b("⬆") } else { Button::new(RichText::new("⬆").size(15.0).color(dim)).min_size(vec2(40.0, 40.0)) }).clicked();
            }));
            drop(p);
            if bpm {
                self.analyze(&ui.ctx().clone());
            }
            if share {
                self.share();
            }
            return;
        }
        ui.horizontal(|ui| {
            controls(ui, &mut p, 14.0, Vec2::ZERO);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(Label::new(time).sense(Sense::click())).on_hover_text("tap: frame rate").clicked() {
                    self.fps = if self.fps.is_some() { None } else { Some((0.0, 0.0)) };
                }
                ui.vertical(|ui| now_playing(ui, &mut p, None));
            });
        });
    }

    fn list(&mut self, ui: &mut Ui) {
        let narrow = ui.available_width() < NARROW;
        if !narrow {
            self.header(ui, false); // phones: in the bottom panel, under the thumb
        }
        // filter + sort only when the query, sort, BPM filter or data changed: every frame costs too much on a phone
        let key = (self.q.clone(), self.sort, self.bpm_on.then_some(self.bpm), self.gen);
        if key != self.list_key {
            let q = self.q.trim().to_lowercase();
            let mut idx: Vec<usize> = (0..self.tracks.len())
                .filter(|&i| {
                    let t = &self.tracks[i];
                    self.bpm_on.then_some(self.bpm).is_none_or(|(c, r)| t.bpm > 0 && t.bpm.abs_diff(c) <= r)
                        && (q.is_empty() || format!("{} {} {} {} {}", t.rel, t.title, t.artist, t.album, self.tags.of(&t.rel).join(" ")).to_lowercase().contains(&q))
                })
                .collect();
            let tr = &self.tracks;
            if let Some((c, desc)) = self.sort {
                match c {
                    0 => idx.sort_by_cached_key(|&i| filename(&tr[i]).to_lowercase()),
                    1 => idx.sort_by_cached_key(|&i| self.tags.of(&tr[i].rel).join(" ")),
                    2 => idx.sort_by_key(|&i| tr[i].duration_ms),
                    3 => idx.sort_by_key(|&i| tr[i].bpm),
                    _ => idx.sort_by_key(|&i| tr[i].added),
                }
                if desc {
                    idx.reverse();
                }
            }
            (self.list_idx, self.list_key) = (idx, key);
        }
        let list: Vec<&Track> = self.list_idx.iter().map(|&i| &self.tracks[i]).collect();
        let (weak, text) = (ui.visuals().text_color(), ui.visuals().strong_text_color()); // one step brighter than egui defaults: easier to read
        let row_h = if narrow { 52.0 } else { ROW };
        let now_id = self.player.borrow().track().map(|t| t.id);
        let mut sa = ScrollArea::vertical().auto_shrink(false);
        // keep the playing row on screen when it changes (j/k, auto-advance); no-op when already visible
        if now_id != self.shown_id {
            self.shown_id = now_id;
            if let Some(k) = list.iter().position(|t| Some(t.id) == now_id) {
                let (y, (off, h)) = (k as f32 * row_h, self.view);
                if y < off { sa = sa.vertical_scroll_offset(y) } else if y + row_h > off + h { sa = sa.vertical_scroll_offset(y + row_h - h) }
            }
        }
        let hl = ui.visuals().widgets.hovered.weak_bg_fill;
        let (have, busy, offline) = (self.off.have.borrow(), self.off.busy.borrow(), self.off.unreachable.get());
        let mut hold = None;
        let out = sa.show_viewport(ui, |ui, vp| {
            ui.set_height(row_h * list.len() as f32);
            let top = ui.max_rect().min;
            let w = ui.max_rect().width();
            let cols = cols(w);
            let mut clicked = None;
            let (a, b) = ((vp.min.y / row_h) as usize, ((vp.max.y / row_h).ceil() as usize).min(list.len()));
            for (k, t) in list.iter().enumerate().take(b).skip(a) {
                let row = Rect::from_min_size(top + vec2(0.0, k as f32 * row_h), vec2(w, row_h));
                let r = ui.interact(row, ui.id().with(t.id), Sense::click());
                if r.hovered() || Some(t.id) == now_id {
                    ui.painter().rect_filled(row, 0.0, hl);
                }
                if r.clicked() {
                    clicked = Some(k);
                }
                if r.secondary_clicked() {
                    hold = Some(k); // long press on a phone, right click on desktop: offline copy on / off
                }
                // Pi out of reach: tracks without an offline copy can't play, show them faded
                let fade = |c: Color32| if offline && !have.contains(&t.id) { c.gamma_multiply(0.35) } else { c };
                let (text, weak) = (fade(text), fade(weak));
                let icon = if busy.contains(&t.id) { "…" } else if have.contains(&t.id) { OFFLINE_ICON } else { "" };
                let clip = |c: Rect| ui.painter().with_clip_rect(c.intersect(ui.clip_rect()));
                let font = FontId::proportional(14.0);
                let dur = fmt(t.duration_ms as f64 / 1000.0);
                let tags = self.tags.of(&t.rel);
                if narrow {
                    // filename on top; tags left, "duration · bpm" right underneath
                    let pad = row.shrink2(vec2(16.0, 6.0));
                    let (l1, l2) = pad.split_top_bottom_at_fraction(0.5);
                    clip(l1).text(l1.left_center() + vec2(ICON / 2.0 - 2.0, 0.0), Align2::CENTER_CENTER, icon, FontId::proportional(13.0), text);
                    let (l1, l2) = (l1.with_min_x(l1.left() + ICON), l2.with_min_x(l2.left() + ICON));
                    clip(l1).text(l1.left_center(), Align2::LEFT_CENTER, filename(t), font, text);
                    let meta = if t.bpm > 0 { format!("{dur} · {} · {}", t.bpm, ymd(t.added)) } else { format!("{dur} · {}", ymd(t.added)) };
                    let m = clip(l2).text(l2.right_center(), Align2::RIGHT_CENTER, meta, FontId::proportional(12.0), weak);
                    chips(&clip(Rect::from_min_max(l2.min, pos2(m.left() - 8.0, l2.max.y))), &self.tags, tags, l2.left(), l2.center().y);
                    continue;
                }
                let cell = |i: usize| {
                    let (x, cw) = cols[i];
                    let c = Rect::from_min_size(pos2(row.left() + x, row.top()), vec2(cw, row_h));
                    (c, clip(c))
                };
                clip(row).text(pos2(row.left() + 16.0 + ICON / 2.0 - 2.0, row.center().y), Align2::CENTER_CENTER, icon, FontId::proportional(13.0), text);
                let (c, p) = cell(0);
                p.text(c.left_center(), Align2::LEFT_CENTER, filename(t), font.clone(), text);
                let (c, p) = cell(1);
                chips(&p, &self.tags, tags, c.left(), c.center().y);
                let (c, p) = cell(2);
                p.text(c.right_center(), Align2::RIGHT_CENTER, dur, font.clone(), weak);
                if t.bpm > 0 {
                    let (c, p) = cell(3);
                    p.text(c.right_center(), Align2::RIGHT_CENTER, t.bpm.to_string(), font.clone(), weak);
                }
                let (c, p) = cell(4);
                p.text(c.right_center(), Align2::RIGHT_CENTER, ymd(t.added), font, weak);
            }
            (clicked, vp)
        });
        let (clicked, vp) = out.inner;
        self.view = (vp.min.y, vp.height());
        drop(busy); // toggle marks the track busy
        if let Some(k) = hold {
            self.off.toggle(list[k], self.note.clone());
        }
        if let Some(k) = clicked {
            if !offline {
                self.player.borrow_mut().play(list.into_iter().cloned().collect(), k);
            } else if have.contains(&list[k].id) {
                // offline: queue only what's on this device, so next/auto-advance never lands on a silent track
                let queue: Vec<Track> = list.iter().filter(|t| have.contains(&t.id)).map(|t| (*t).clone()).collect();
                let i = queue.iter().position(|t| t.id == list[k].id).unwrap();
                self.player.borrow_mut().play(queue, i);
            } else {
                *self.note.borrow_mut() = Some("not on this device: hold a track (right-click on desktop) at home to download it".into());
            }
        }
    }

    /// Filter row for the BPM filter: center and ± range, − / + for thumbs, drag or tap the number for big jumps.
    fn bpm_panel(&mut self, ui: &mut Ui) {
        let (c, r) = &mut self.bpm;
        let mut off = false;
        let narrow = ui.available_width() < NARROW;
        let h = if narrow { 40.0 } else { 0.0 };
        // right-aligned, in thumb reach; right-to-left adds the widgets in reverse: reads 🗙 − 124 + ± − 4 +
        ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if narrow {
                ui.spacing_mut().interact_size.y = h;
                ui.spacing_mut().item_spacing.x = 4.0; // fits a 360 pt phone
            }
            let b = |s: &str| Button::new(RichText::new(s).size(15.0)).min_size(vec2(h, h));
            if ui.add(b("+")).clicked() { *r = (*r + 1).min(50) }
            ui.add(DragValue::new(r).range(0..=50));
            if ui.add(b("−")).clicked() { *r = r.saturating_sub(1) }
            ui.label("±");
            if ui.add(b("+")).clicked() { *c = (*c + 1).min(300) }
            ui.add(DragValue::new(c).range(1..=300));
            if ui.add(b("−")).clicked() { *c = c.saturating_sub(1).max(1) }
            off = ui.add(b(&label(narrow, "🗙", "🗙 bpm"))).on_hover_text("BPM filter off").clicked();
        }));
        if off {
            self.bpm_on = false;
        }
    }

    /// Column header: click a column to sort by it, again to reverse; hold (right-click) bpm to filter by BPM.
    fn header(&mut self, ui: &mut Ui, narrow: bool) {
        let (weak, text) = (ui.visuals().text_color(), ui.visuals().strong_text_color());
        // desktop: painted over the list's columns; phones: flat buttons, as wide as their text, 40 pt tall
        let hdr = (!narrow).then(|| ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::hover()).0);
        for i in 0..5 {
            let arrow = match self.sort { Some((j, d)) if j == i => if d { " ⬇" } else { " ⬆" }, _ => "" };
            let label = format!("{}{arrow}", ["filename", "tags", "duration", "bpm", "added"][i]);
            let lit = arrow != "" || (i == 3 && self.bpm_on);
            let r = match hdr {
                None => ui.add(Button::new(RichText::new(label).size(13.0).color(if lit { text } else { weak })).frame(false).min_size(vec2(0.0, 40.0))),
                Some(hdr) => {
                    let (x, cw) = cols(hdr.width())[i];
                    let c = Rect::from_min_size(pos2(hdr.left() + x, hdr.top()), vec2(cw, ROW));
                    let r = ui.interact(c, ui.id().with(("sort", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    let (at, align) = if i < 2 { (c.left_center(), Align2::LEFT_CENTER) } else { (c.right_center(), Align2::RIGHT_CENTER) };
                    ui.painter().with_clip_rect(c).text(at, align, label, FontId::proportional(13.0), if lit || r.hovered() { text } else { weak });
                    r
                }
            };
            if r.clicked() {
                self.sort = match self.sort { Some((j, d)) if j == i => Some((i, !d)), _ => Some((i, false)) };
            }
            if i == 3 && r.secondary_clicked() && !self.bpm_on {
                let now = self.player.borrow().track().map_or(0, |t| t.bpm); // start around the playing track
                if now > 0 {
                    self.bpm.0 = now;
                }
                self.bpm_on = true;
            }
        }
    }
}

/// Column (x, width) in a row `w` wide, after the ICON column: filename 2fr, tags 1fr, duration 4em, bpm 3em,
/// added 64px; 16px padding, 8px gaps.
fn cols(w: f32) -> [(f32, f32); 5] {
    let fr = ((w - 32.0 - ICON - 32.0 - 56.0 - 42.0 - 64.0) / 3.0).max(0.0);
    let x = 16.0 + ICON;
    [(x, 2.0 * fr), (x + 8.0 + 2.0 * fr, fr), (x + 16.0 + 3.0 * fr, 56.0), (x + 80.0 + 3.0 * fr, 42.0), (x + 130.0 + 3.0 * fr, 64.0)]
}

/// Button text: just the icon on phones, `text` otherwise.
fn label(narrow: bool, icon: &str, text: &str) -> String {
    (if narrow { icon } else { text }).to_owned()
}

/// Unix seconds as local yy/mm/dd.
fn ymd(secs: u64) -> String {
    let d = js_sys::Date::new(&(secs as f64 * 1000.0).into());
    format!("{:02}/{:02}/{:02}", d.get_full_year() % 100, d.get_month() + 1, d.get_date())
}

const INK: Color32 = Color32::from_gray(40); // text on pastel, readable in light and dark themes

fn pastel(hue_deg: f32) -> Color32 {
    egui::ecolor::Hsva::new(hue_deg / 360.0, 0.55, 1.0, 1.0).into()
}

/// The tag's stored pastel hue (assigned on load / creation by `Tags::color_missing`).
fn tag_color(tags: &Tags, name: &str) -> Color32 {
    pastel(tags.hues.get(name).copied().unwrap_or(0) as f32)
}

/// Tag names as rounded pastel chips, left to right from `x`, vertically centered on `y`.
fn chips(p: &Painter, all: &Tags, tags: &[String], mut x: f32, y: f32) {
    for n in tags {
        let g = p.layout_no_wrap(n.clone(), FontId::proportional(12.5), INK);
        let r = Rect::from_min_size(pos2(x, y - g.size().y / 2.0 - 2.0), g.size() + vec2(14.0, 4.0));
        p.rect_filled(r, CornerRadius::same(8), tag_color(all, n));
        p.galley(r.min + vec2(7.0, 2.0), g, INK);
        x = r.right() + 4.0;
    }
}

/// Three waveform bars (short, tall, medium), like the app icon, centered on a button.
fn bars(ui: &Ui, r: &egui::Response) {
    let c = ui.style().interact(r).fg_stroke.color;
    for (k, h) in [8.0, 16.0, 11.0].into_iter().enumerate() {
        let x = r.rect.center().x + (k as f32 - 1.0) * 5.5;
        ui.painter().rect_filled(Rect::from_center_size(pos2(x, r.rect.center().y), vec2(3.0, h)), 1.5, c);
    }
}

fn controls(ui: &mut Ui, p: &mut Player, size: f32, min: Vec2) {
    let b = |s: &str| Button::new(RichText::new(s).size(size)).min_size(min);
    let mut order = [0, 1, 2];
    if ui.layout().prefer_right_to_left() {
        order.reverse(); // still reads ⏮ ⏵ ⏭
    }
    for k in order {
        match k {
            0 => if ui.add(b("⏮")).clicked() { p.prev() },
            1 => if ui.add(b(if p.playing() { "⏸" } else { "▶" })).clicked() { p.toggle() },
            _ => if ui.add(b("⏭")).clicked() { p.next() },
        }
    }
}

/// Title — artist (with `time` right-aligned beside it, on phones), and the waveform seek bar under it.
/// Returns whether the time was tapped.
fn now_playing(ui: &mut Ui, p: &mut Player, time: Option<String>) -> bool {
    let mut tapped = false;
    let mut job = LayoutJob::default();
    let font = FontId::proportional(14.0);
    match p.track() {
        Some(t) => {
            job.append(&t.title, 0.0, TextFormat::simple(font.clone(), ui.visuals().strong_text_color()));
            job.append(&format!(" — {}", t.artist), 0.0, TextFormat::simple(font, ui.visuals().text_color()));
        }
        None => job.append("nothing playing", 0.0, TextFormat::simple(font, ui.visuals().weak_text_color())),
    }
    let title = Label::new(job).truncate();
    match time {
        Some(time) => {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
            let t = ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, time, FontId::proportional(14.0), ui.visuals().text_color());
            tapped = ui.interact(t.expand(6.0), ui.id().with("time"), Sense::click()).clicked();
            let left = Rect::from_min_max(r.min, pos2(t.left() - 8.0, r.max.y));
            ui.scope_builder(egui::UiBuilder::new().max_rect(left).layout(Layout::left_to_right(Align::Center)), |ui| ui.add(title));
        }
        None => {
            ui.add(title);
        }
    }
    let (pos, dur) = (p.pos(), p.dur());
    let seek = {
        let l = p.cur();
        let loaded = l.as_ref().map_or(0.0, |l| if l.frames > 0 { l.loaded as f32 / l.frames as f32 } else { 0.0 });
        wave(ui, l.as_ref().map(|l| &l.peaks[..]), loaded, if dur > 0.0 { (pos / dur) as f32 } else { 0.0 })
    };
    if let Some(f) = seek {
        p.seek(f as f64 * dur);
    }
    tapped
}

/// Whether the share sheet takes files (canShare is missing on desktop Firefox; calling it there would throw).
fn can_share() -> bool {
    let nav = web_sys::window().unwrap().navigator();
    if !js_sys::Reflect::has(&nav, &"canShare".into()).unwrap_or(false) {
        return false;
    }
    let o = web_sys::FilePropertyBag::new();
    o.set_type("audio/wav");
    let Ok(f) = web_sys::File::new_with_u8_array_sequence_and_options(&js_sys::Array::of1(&js_sys::Uint8Array::new_with_length(44)), "probe.wav", &o) else { return false };
    let d = web_sys::ShareData::new();
    d.set_files(&js_sys::Array::of1(&f));
    nav.can_share_with_data(&d)
}

/// Open the share sheet with `f`. Rejections: user cancelled (fine), or the tap's activation expired while
/// preparing (the file is kept; the next tap shares it).
fn share_now(f: web_sys::File, title: String, note: Rc<RefCell<Option<String>>>) {
    let d = web_sys::ShareData::new();
    d.set_files(&js_sys::Array::of1(&f));
    d.set_title(&title);
    let pr = web_sys::window().unwrap().navigator().share_with_data(&d);
    spawn_local(async move {
        if let Err(e) = JsFuture::from(pr).await {
            let name = js_sys::Reflect::get(&e, &"name".into()).ok().and_then(|n| n.as_string()).unwrap_or_default();
            if name == "NotAllowedError" {
                *note.borrow_mut() = Some("ready — tap share again".into());
            } else if name != "AbortError" {
                *note.borrow_mut() = Some(format!("share failed: {name}"));
            }
        }
    });
}

/// Formats the share sheet takes as they are; anything else (AIFF) goes as a WAV built from the fully loaded PCM.
fn shares_original(t: &Track) -> bool {
    matches!(t.ext.as_str(), "mp3" | "m4a" | "wav" | "flac" | "ogg" | "oga" | "opus")
}

fn filename(t: &Track) -> &str {
    t.rel.rsplit('/').next().unwrap_or(&t.rel)
}

/// Waveform seek bar: one mirrored bar per peak bin in a pastel pink → blue sweep; unplayed bars faded, undecoded bars grey.
/// Returns the clicked / dragged-to fraction.
fn wave(ui: &mut Ui, peaks: Option<&[f32]>, loaded: f32, progress: f32) -> Option<f32> {
    let (rect, r) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::click_and_drag());
    if let Some(p) = peaks {
        let fg = ui.visuals().text_color();
        // at most one bar per 2 pt: a phone is ~360 pt wide, drawing all 1000 bins would stack them
        let n = ((rect.width() / 2.0) as usize).clamp(1, p.len());
        let bw = rect.width() / n as f32;
        let painter = ui.painter_at(rect);
        for b in 0..n {
            let v = p[b * p.len() / n..((b + 1) * p.len() / n).max(b * p.len() / n + 1)].iter().fold(0.0f32, |m, &x| m.max(x));
            let f = b as f32 / n as f32;
            let c = if f < progress { pastel(330.0 - 150.0 * f) } else if f < loaded { pastel(330.0 - 150.0 * f).gamma_multiply(0.35) } else { fg.gamma_multiply(0.2) };
            let h = (v * rect.height()).max(1.0);
            painter.rect_filled(Rect::from_min_size(pos2(rect.left() + b as f32 * bw, rect.center().y - h / 2.0), vec2(bw, h)), 0.0, c);
        }
    }
    let hit = r.is_pointer_button_down_on() && (ui.input(|i| i.pointer.any_pressed()) || r.drag_delta().x != 0.0);
    hit.then(|| r.interact_pointer_pos()).flatten().map(|p| ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        let perf = web_sys::window().unwrap().performance().unwrap();
        let t0 = perf.now();
        let loaded = self.inbox.borrow_mut().take();
        if let Some((t, g)) = loaded {
            (self.tracks, self.tags) = (t, g);
            self.gen += 1;
            let pending = self.off.pending.clone(); // edits not yet on the server: keep showing them
            pending.iter().for_each(|op| self.tags.apply(op));
            self.color_new_tags();
            self.off.flush();
        }
        if let Some(doc) = self.off.take_synced() {
            self.tags = doc; // the server's doc also carries other devices' edits
            self.gen += 1;
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250)); // clock + waveform progress
        let now = ui.input(|i| i.time);
        if !self.off.pending.is_empty() && now - self.last_flush > 15.0 {
            self.last_flush = now; // retry queued tag edits while the Pi is out of reach
            self.off.flush();
        }
        if let Some((id, r)) = self.analyzed.borrow_mut().take() {
            let name = self.tracks.iter().find(|t| t.id == id).map_or(String::new(), |t| filename(t).to_owned());
            self.status = Some((
                match r {
                    Ok(bpm) => {
                        self.tracks.iter_mut().filter(|t| t.id == id).for_each(|t| t.bpm = bpm);
                        self.gen += 1;
                        format!("{bpm} BPM written to {name}")
                    }
                    Err(e) => format!("analyze failed: {e}"),
                },
                now + 5.0,
            ));
        }
        if let Some(msg) = self.note.borrow_mut().take() {
            self.status = (!msg.is_empty()).then_some((msg, now + 5.0));
        }
        if self.status.as_ref().is_some_and(|(_, until)| now > *until) {
            self.status = None;
        }
        self.keys(&ui.ctx().clone());
        egui::Panel::bottom("player").show(ui, |ui| self.player_bar(ui));
        // search, tags, filters and (on phones) sorting sit just above the player: all in thumb reach
        egui::Panel::bottom("search").show(ui, |ui| {
            ui.add_space(4.0);
            if self.off.unreachable.get() {
                let n = self.off.pending.len();
                let queued = if n > 0 { format!(" · {n} tag change{} waiting to sync", if n == 1 { "" } else { "s" }) } else { String::new() };
                ui.weak(format!("offline: playing downloaded tracks{queued}"));
            }
            let narrow = ui.available_width() < NARROW;
            let offer = self.install.borrow().clone();
            if let Some(e) = offer {
                let b = ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.button(label(narrow, "📲", "install app")).clicked()).inner).inner;
                if b {
                    if let Ok(f) = js_sys::Reflect::get(&e, &"prompt".into()).and_then(|f| f.dyn_into::<js_sys::Function>()) {
                        let _ = f.call0(&e); // inside the click: egui runs click logic in the pointer event
                    }
                    *self.install.borrow_mut() = None;
                }
            }
            let h = if narrow { 40.0 } else { 0.0 }; // thumb-sized on phones
            // on phones the sort buttons, then the search and tags toggles at the right edge
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0; // fits a 360 pt phone
                if narrow {
                    self.header(ui, true);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let b = |s: String, on: bool| Button::new(RichText::new(s).size(15.0)).selected(on).min_size(vec2(h, h));
                    if ui.add(b(label(narrow, "🏷", "🏷 tags"), self.tags_open)).clicked() {
                        self.tags_open = !self.tags_open;
                    }
                    // lit while a query filters the list, even with the field hidden
                    if ui.add(b(label(narrow, "🔍", "🔍 search"), self.search_open || !self.q.is_empty())).clicked() {
                        self.search_open = !self.search_open;
                        self.focus_search = self.search_open;
                    }
                });
            });
            ui.add_space(4.0);
        });
        // panes slide out of the controls; `&mut { … }`: a copy, only our own buttons open and close them
        egui::Panel::bottom("bpm_pane").resizable(false).show_collapsible(ui, &mut { self.bpm_on }, |ui| {
            ui.add_space(4.0);
            self.bpm_panel(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("tags_pane").resizable(false).show_collapsible(ui, &mut { self.tags_open }, |ui| {
            ui.add_space(4.0);
            self.tags_panel(ui);
            ui.add_space(4.0);
        });
        // search at the top: at the bottom the phone keyboard would cover it
        egui::Panel::top("search_field").resizable(false).show_collapsible(ui, &mut { self.search_open }, |ui| {
            let h = if ui.available_width() < NARROW { 40.0 } else { 0.0 };
            ui.add_space(4.0);
            ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(Button::new(RichText::new("🗙").size(15.0)).min_size(vec2(h, h))).on_hover_text("clear and close").clicked() {
                    self.q.clear();
                    self.search_open = false;
                }
                let hint = format!("search {} tracks", self.tracks.len());
                let r = ui.add(TextEdit::singleline(&mut self.q).hint_text(hint).desired_width(f32::INFINITY).min_size(vec2(0.0, h)).vertical_align(Align::Center));
                if std::mem::take(&mut self.focus_search) {
                    r.request_focus();
                }
            }));
            ui.add_space(4.0);
        });
        let list = egui::CentralPanel::default().show(ui, |ui| self.list(ui));
        if let Some((msg, _)) = &self.status {
            // just above the bottom controls and panes, where the eyes are after a tap
            let at = pos2(ui.ctx().content_rect().center().x, list.response.rect.bottom() - 8.0);
            egui::Area::new("status".into()).pivot(Align2::CENTER_BOTTOM).fixed_pos(at).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(Label::new(msg.as_str()).wrap_mode(egui::TextWrapMode::Extend)));
            });
        }
        if let Some((dt, cpu)) = &mut self.fps {
            // smoothed over ~20 frames; repaint continuously so the number is the real frame rate
            *dt += (ui.input(|i| i.unstable_dt) as f64 * 1000.0 - *dt) * 0.05;
            *cpu += (perf.now() - t0 - *cpu) * 0.05;
            let msg = format!("{:.0} fps · {:.1} ms frame · {:.1} ms ui", 1000.0 / dt.max(0.001), dt, cpu);
            egui::Area::new("fps".into()).anchor(Align2::RIGHT_TOP, vec2(-8.0, 8.0)).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(Label::new(RichText::new(msg).monospace()).wrap_mode(egui::TextWrapMode::Extend)));
            });
            ui.ctx().request_repaint();
        }
        if self.help {
            let m = egui::Modal::new("help".into()).show(ui.ctx(), |ui| {
                ui.label(RichText::new("Keys").strong());
                for l in ["space — play / pause", "j / k — next / previous track", "h / l — back / forward 1 min",
                          "ctrl+d — download the playing track", "ctrl+a — detect the playing track's BPM and write it into the file", "tag keys — toggle that tag on the playing track (see tags panel)",
                          "? — this help", "/ — search", "hold (right-click) the bpm column — filter by BPM ± range", "tap the time (0:42 / 5:10) — frame-rate readout"] {
                    ui.label(l);
                }
                ui.small("Esc closes");
            });
            if m.should_close() {
                self.help = false;
            }
        }
    }
}
