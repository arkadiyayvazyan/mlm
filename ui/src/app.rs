use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use eframe::egui::{
    self, pos2, text::LayoutJob, vec2, Align, Align2, Button, Color32, CornerRadius, FontId, Key, Label, Layout,
    Modifiers, Painter, Rect, RichText, ScrollArea, Sense, TextEdit, TextFormat, Ui, Vec2,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{RequestInit, Response};

use crate::player::Player;
use crate::tags::Tags;
use crate::{fmt, Track};

const ROW: f32 = 26.0;
const NARROW: f32 = 600.0; // below this width (phones): two-line rows, touch-sized controls

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
}

async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Option<T> {
    let res: Response = JsFuture::from(web_sys::window()?.fetch_with_str(url)).await.ok()?.dyn_into().ok()?;
    serde_json::from_str(&JsFuture::from(res.text().ok()?).await.ok()?.as_string()?).ok()
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        let inbox = Rc::new(RefCell::new(None));
        let (ib, ctx) = (inbox.clone(), cc.egui_ctx.clone());
        spawn_local(async move {
            let (t, g) = (get_json("/api/tracks").await, get_json("/api/tags").await);
            *ib.borrow_mut() = Some((t.unwrap_or_default(), g.unwrap_or_default()));
            ctx.request_repaint();
        });
        // the scheduler runs off a timer, not the frame loop: egui doesn't paint while the tab is hidden
        let player = Rc::new(RefCell::new(Player::new()));
        let p = player.clone();
        let tick = Closure::<dyn FnMut()>::new(move || p.borrow_mut().tick());
        web_sys::window()
            .unwrap()
            .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 50)
            .unwrap();
        tick.forget();
        Self {
            tracks: vec![], tags: Tags::default(), inbox, player, q: String::new(), new_name: String::new(),
            new_key: String::new(), err: String::new(), help: false, shown_id: None, view: (0.0, 0.0), sort: None,
            analyzed: Rc::default(), status: None,
        }
    }

    fn save_tags(&self) {
        let init = RequestInit::new();
        init.set_method("PUT");
        init.set_body(&serde_json::to_string(&self.tags).unwrap().into());
        let _ = web_sys::window().unwrap().fetch_with_str_and_init("/api/tags", &init);
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
                _ => {
                    drop(p);
                    let name = self.tags.keys.iter().find(|(_, v)| **v == k).map(|(n, _)| n.clone());
                    if let (Some(name), Some(rel)) = (name, self.now_rel()) {
                        self.tags.toggle(&rel, &name);
                        self.save_tags();
                    }
                }
            }
        }
    }

    fn tags_panel(&mut self, ui: &mut Ui) {
        let now = self.now_rel();
        ui.horizontal_wrapped(|ui| {
            for (n, k) in self.tags.keys.clone() {
                let on = now.as_ref().is_some_and(|r| self.tags.has(r, &n));
                // always full pastel; a bright outline marks tags on the playing track
                let stroke = if on { egui::Stroke::new(2.0, ui.visuals().strong_text_color()) } else { egui::Stroke::NONE };
                let b = ui.add(Button::new(RichText::new(format!("{n}  {k}")).color(INK)).fill(tag_color(&self.tags, &n)).stroke(stroke));
                if b.on_hover_text(format!("press {k} to toggle on the playing track")).clicked() && now.is_some() {
                    self.tags.toggle(now.as_ref().unwrap(), &n);
                    self.save_tags();
                }
                let confirm = |m: &str| web_sys::window().unwrap().confirm_with_message(m).unwrap_or(false);
                if ui.button("🗙").on_hover_text("delete tag").clicked() && confirm(&format!("Delete \"{n}\" from all tracks?")) {
                    self.tags.remove(&n);
                    self.save_tags();
                }
            }
            let a = ui.add(TextEdit::singleline(&mut self.new_name).hint_text("new tag").desired_width(100.0));
            let b = ui.add(TextEdit::singleline(&mut self.new_key).hint_text("key").char_limit(1).desired_width(30.0));
            if a.changed() || b.changed() {
                self.err.clear();
            }
            let enter = (a.lost_focus() || b.lost_focus()) && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.button("add").clicked() || enter {
                match self.tags.add(&self.new_name, &self.new_key) {
                    Ok(()) => {
                        self.tags.color_missing(js_sys::Math::random);
                        self.save_tags();
                        self.new_name.clear();
                        self.new_key.clear();
                    }
                    Err(e) => self.err = e,
                }
            }
            if !self.err.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &self.err);
            }
        });
    }

    fn player_bar(&mut self, ui: &mut Ui) {
        let mut p = self.player.borrow_mut();
        if !p.supported() {
            ui.colored_label(ui.visuals().error_fg_color, "playback needs a cross-origin isolated page: open over HTTPS or localhost");
            return;
        }
        let time = format!("{} / {}", fmt(p.pos()), fmt(p.dur()));
        if ui.available_width() < NARROW {
            now_playing(ui, &mut p);
            ui.horizontal(|ui| {
                controls(ui, &mut p, 22.0, vec2(56.0, 40.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(time));
            });
            return;
        }
        ui.horizontal(|ui| {
            controls(ui, &mut p, 14.0, Vec2::ZERO);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(time);
                ui.vertical(|ui| now_playing(ui, &mut p));
            });
        });
    }

    fn list(&mut self, ui: &mut Ui) {
        let q = self.q.trim().to_lowercase();
        // ponytail: filter re-run every frame; cache on (q, tags) if 20k+ tracks lag
        let mut list: Vec<&Track> = self.tracks.iter()
            .filter(|t| q.is_empty() || format!("{} {} {} {} {}", t.rel, t.title, t.artist, t.album, self.tags.of(&t.rel).join(" ")).to_lowercase().contains(&q))
            .collect();
        if let Some((c, desc)) = self.sort {
            match c {
                0 => list.sort_by_cached_key(|t| filename(t).to_lowercase()),
                1 => list.sort_by_cached_key(|t| self.tags.of(&t.rel).join(" ")),
                2 => list.sort_by_key(|t| t.duration_ms),
                3 => list.sort_by_key(|t| t.bpm),
                _ => list.sort_by_key(|t| t.added),
            }
            if desc {
                list.reverse();
            }
        }
        let (weak, text) = (ui.visuals().text_color(), ui.visuals().strong_text_color()); // one step brighter than egui defaults: easier to read
        let narrow = ui.available_width() < NARROW;
        let (row_h, hdr_h) = if narrow { (52.0, 36.0) } else { (ROW, ROW) };
        // header: click a column to sort by it, again to reverse; on phones just four equal buttons
        let (hdr, _) = ui.allocate_exact_size(vec2(ui.available_width(), hdr_h), Sense::hover());
        let slots = if narrow { [0.0, 1.0, 2.0, 3.0, 4.0].map(|k| (16.0 + k * (hdr.width() - 32.0) / 5.0, (hdr.width() - 32.0) / 5.0)) } else { cols(hdr.width()) };
        for (i, (x, cw)) in slots.into_iter().enumerate() {
            let c = Rect::from_min_size(pos2(hdr.left() + x, hdr.top()), vec2(cw, hdr_h));
            let r = ui.interact(c, ui.id().with(("sort", i)), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            let arrow = match self.sort { Some((j, d)) if j == i => if d { " ⬇" } else { " ⬆" }, _ => "" };
            let label = format!("{}{arrow}", ["filename", "tags", "duration", "bpm", "added"][i]);
            let (at, align) = if i < 2 || narrow { (c.left_center(), Align2::LEFT_CENTER) } else { (c.right_center(), Align2::RIGHT_CENTER) };
            let color = if r.hovered() || arrow != "" { text } else { weak };
            ui.painter().with_clip_rect(c).text(at, align, label, FontId::proportional(13.0), color);
            if r.clicked() {
                self.sort = match self.sort { Some((j, d)) if j == i => Some((i, !d)), _ => Some((i, false)) };
            }
        }
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
                let clip = |c: Rect| ui.painter().with_clip_rect(c.intersect(ui.clip_rect()));
                let font = FontId::proportional(14.0);
                let dur = fmt(t.duration_ms as f64 / 1000.0);
                let tags = self.tags.of(&t.rel);
                if narrow {
                    // filename on top; tags left, "duration · bpm" right underneath
                    let pad = row.shrink2(vec2(16.0, 6.0));
                    let (l1, l2) = pad.split_top_bottom_at_fraction(0.5);
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
        if let Some(k) = clicked {
            self.player.borrow_mut().play(list.into_iter().cloned().collect(), k);
        }
    }
}

/// Column (x, width) in a row `w` wide: filename 2fr, tags 1fr, duration 4em, bpm 3em, added 64px; 16px padding, 8px gaps.
fn cols(w: f32) -> [(f32, f32); 5] {
    let fr = ((w - 32.0 - 32.0 - 56.0 - 42.0 - 64.0) / 3.0).max(0.0);
    [(16.0, 2.0 * fr), (24.0 + 2.0 * fr, fr), (32.0 + 3.0 * fr, 56.0), (96.0 + 3.0 * fr, 42.0), (146.0 + 3.0 * fr, 64.0)]
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

fn controls(ui: &mut Ui, p: &mut Player, size: f32, min: Vec2) {
    let b = |s: &str| Button::new(RichText::new(s).size(size)).min_size(min);
    if ui.add(b("⏮")).clicked() { p.prev() }
    if ui.add(b(if p.playing() { "⏸" } else { "▶" })).clicked() { p.toggle() }
    if ui.add(b("⏭")).clicked() { p.next() }
}

/// Title — artist, and the waveform seek bar under it.
fn now_playing(ui: &mut Ui, p: &mut Player) {
    let mut job = LayoutJob::default();
    let font = FontId::proportional(14.0);
    match p.track() {
        Some(t) => {
            job.append(&t.title, 0.0, TextFormat::simple(font.clone(), ui.visuals().strong_text_color()));
            job.append(&format!(" — {}", t.artist), 0.0, TextFormat::simple(font, ui.visuals().text_color()));
        }
        None => job.append("nothing playing", 0.0, TextFormat::simple(font, ui.visuals().weak_text_color())),
    }
    ui.add(Label::new(job).truncate());
    let (pos, dur) = (p.pos(), p.dur());
    let seek = {
        let l = p.cur();
        let loaded = l.as_ref().map_or(0.0, |l| if l.frames > 0 { l.loaded as f32 / l.frames as f32 } else { 0.0 });
        wave(ui, l.as_ref().map(|l| &l.peaks[..]), loaded, if dur > 0.0 { (pos / dur) as f32 } else { 0.0 })
    };
    if let Some(f) = seek {
        p.seek(f as f64 * dur);
    }
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
        let bw = rect.width() / p.len() as f32;
        let painter = ui.painter_at(rect);
        for (b, &v) in p.iter().enumerate() {
            let f = b as f32 / p.len() as f32;
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
        if let Some((t, g)) = self.inbox.borrow_mut().take() {
            (self.tracks, self.tags) = (t, g);
            if self.tags.color_missing(js_sys::Math::random) {
                self.save_tags(); // tags from before colors existed (or made in the old UI) get theirs once
            }
        }
        ui.ctx().request_repaint_after(Duration::from_millis(250)); // clock + waveform progress
        let now = ui.input(|i| i.time);
        if let Some((id, r)) = self.analyzed.borrow_mut().take() {
            let name = self.tracks.iter().find(|t| t.id == id).map_or(String::new(), |t| filename(t).to_owned());
            self.status = Some((
                match r {
                    Ok(bpm) => {
                        self.tracks.iter_mut().filter(|t| t.id == id).for_each(|t| t.bpm = bpm);
                        format!("{bpm} BPM written to {name}")
                    }
                    Err(e) => format!("analyze failed: {e}"),
                },
                now + 5.0,
            ));
        }
        if self.status.as_ref().is_some_and(|(_, until)| now > *until) {
            self.status = None;
        }
        if let Some((msg, _)) = &self.status {
            egui::Area::new("status".into()).anchor(Align2::CENTER_TOP, vec2(0.0, 8.0)).show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| ui.add(Label::new(msg.as_str()).wrap_mode(egui::TextWrapMode::Extend)));
            });
        }
        self.keys(&ui.ctx().clone());
        egui::Panel::top("search").show(ui, |ui| {
            ui.add_space(8.0);
            let hint = format!("search {} tracks", self.tracks.len());
            let h = if ui.available_width() < NARROW { 36.0 } else { 0.0 }; // thumb-sized on phones
            ui.add(TextEdit::singleline(&mut self.q).hint_text(hint).desired_width(f32::INFINITY).min_size(vec2(0.0, h)));
            ui.collapsing("tags", |ui| self.tags_panel(ui));
        });
        egui::Panel::bottom("player").show(ui, |ui| self.player_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.list(ui));
        if self.help {
            let m = egui::Modal::new("help".into()).show(ui.ctx(), |ui| {
                ui.label(RichText::new("Keys").strong());
                for l in ["space — play / pause", "j / k — next / previous track", "h / l — back / forward 1 min",
                          "ctrl+d — download the playing track", "ctrl+a — detect the playing track's BPM and write it into the file", "tag keys — toggle that tag on the playing track (see tags panel)",
                          "? — this help"] {
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
