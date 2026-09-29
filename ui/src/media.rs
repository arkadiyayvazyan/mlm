//! Lock-screen / notification player via the Media Session API. Chrome on Android only shows it (and iOS only keeps
//! audio running with the screen locked) while a media element plays, so a looping silent <audio> plays alongside
//! the Web Audio output. `sync` runs from the player's interval, which keeps going while the page is hidden.
use std::cell::RefCell;
use std::rc::Rc;

use js_sys::Reflect;
use wasm_bindgen::prelude::*;
use web_sys::{
    HtmlAudioElement, MediaImage, MediaMetadata, MediaMetadataInit, MediaPositionState, MediaSession, MediaSessionAction,
    MediaSessionActionDetails, MediaSessionPlaybackState,
};

use crate::player::Player;

const SEEK_STEP: f64 = 30.0;

pub struct Media {
    el: HtmlAudioElement,
    session: MediaSession,
    id: Option<u64>,
    playing: bool,
    pos: (f64, f64), // last position handed to the session, and the performance.now() (ms) it was at
}

impl Media {
    pub fn new(player: &Rc<RefCell<Player>>) -> Self {
        let el = HtmlAudioElement::new_with_src("/silence.wav").unwrap();
        el.set_loop(true);
        let nav = web_sys::window().unwrap().navigator();
        // iOS 17.5+: a "playback" session keeps Web Audio running with the screen locked
        if let Ok(s) = Reflect::get(&nav, &"audioSession".into()) {
            if !s.is_undefined() {
                let _ = Reflect::set(&s, &"type".into(), &"playback".into());
            }
        }
        let session = nav.media_session();
        let on = |action: MediaSessionAction, f: fn(&mut Player, &MediaSessionActionDetails)| {
            let p = player.clone();
            let h = Closure::<dyn Fn(MediaSessionActionDetails)>::new(move |d: MediaSessionActionDetails| f(&mut p.borrow_mut(), &d));
            session.set_action_handler(action, Some(h.as_ref().unchecked_ref()));
            h.forget();
        };
        on(MediaSessionAction::Play, |p, _| p.set_playing(true));
        on(MediaSessionAction::Pause, |p, _| p.set_playing(false));
        on(MediaSessionAction::Stop, |p, _| p.set_playing(false));
        on(MediaSessionAction::Previoustrack, |p, _| p.prev());
        on(MediaSessionAction::Nexttrack, |p, _| p.next());
        on(MediaSessionAction::Seekbackward, |p, d| p.skip(-d.get_seek_offset().unwrap_or(SEEK_STEP)));
        on(MediaSessionAction::Seekforward, |p, d| p.skip(d.get_seek_offset().unwrap_or(SEEK_STEP)));
        on(MediaSessionAction::Seekto, |p, d| {
            if let Some(t) = d.get_seek_time() {
                p.seek(t)
            }
        });
        // iOS only lets a media element play() from a timer once a user gesture has played it: do that on the first
        // tap / key (capture phase, before egui reacts), then leave it paused unless the player started meanwhile
        let (el2, p) = (el.clone(), player.clone());
        let once = Rc::new(std::cell::Cell::new(false));
        let unlock = Closure::<dyn Fn()>::new(move || {
            if once.replace(true) {
                return;
            }
            let (el, p) = (el2.clone(), p.clone());
            if let Ok(pr) = el.play() {
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = wasm_bindgen_futures::JsFuture::from(pr).await;
                    if !p.borrow().playing() {
                        let _ = el.pause();
                    }
                });
            }
        });
        let w = web_sys::window().unwrap();
        for ev in ["pointerup", "touchend", "keydown"] {
            let _ = w.add_event_listener_with_callback_and_bool(ev, unlock.as_ref().unchecked_ref(), true);
        }
        unlock.forget();
        Self { el, session, id: None, playing: false, pos: (0.0, 0.0) }
    }

    pub fn sync(&mut self, p: &Player) {
        let id = p.track().map(|t| t.id);
        let changed = id != self.id;
        if changed {
            self.id = id;
            self.session.set_metadata(p.track().map(|t| {
                let m = MediaMetadataInit::new();
                m.set_title(&t.title);
                m.set_artist(&t.artist);
                m.set_album(&t.album);
                let art = MediaImage::new(&format!("/api/tracks/{}/art", t.id));
                art.set_sizes("512x512");
                m.set_artwork(&[art]);
                MediaMetadata::new_with_init(&m).unwrap()
            }).as_ref());
        }
        let playing = p.playing();
        let toggled = playing != self.playing;
        if toggled {
            self.playing = playing;
            let _ = if playing { self.el.play().map(drop) } else { self.el.pause() };
            self.session.set_playback_state(if playing { MediaSessionPlaybackState::Playing } else { MediaSessionPlaybackState::Paused });
        }
        // position: on any change above, or when it drifts from what the OS extrapolates (a seek)
        let (dur, pos, now) = (p.dur(), p.pos(), web_sys::window().unwrap().performance().unwrap().now());
        let expected = self.pos.0 + if playing { (now - self.pos.1) / 1000.0 } else { 0.0 };
        if dur > 0.0 && (changed || toggled || (pos - expected).abs() > 1.0 || now - self.pos.1 > 10_000.0) {
            let s = MediaPositionState::new();
            s.set_duration(dur);
            s.set_position(pos.min(dur));
            s.set_playback_rate(1.0);
            let _ = self.session.set_position_state_with_state(&s);
            self.pos = (pos, now);
        }
    }
}
