//! Offline use. Downloaded tracks are their `/pcm` stream (what the player reads anyway) plus cover art in the
//! "tracks" Cache Storage, which `sw.js` serves when asked; the rest of the app is cached by `sw.js` itself.
//! Tag edits are `Op`s queued in localStorage and sent whenever the Pi answers.
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use eframe::egui;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{Cache, RequestInit, Response};

use crate::tags::{Op, Tags};
use crate::Track;

const PENDING: &str = "mlm-pending"; // localStorage key of the unsent tag edits

pub struct Offline {
    pub have: Rc<RefCell<HashSet<u64>>>, // downloaded track ids
    pub busy: Rc<RefCell<HashSet<u64>>>, // downloading
    pub unreachable: Rc<Cell<bool>>,     // the last request to the Pi failed (or sw.js answered from its cache)
    pub pending: Vec<Op>,
    sending: Rc<Cell<bool>>,
    synced: Rc<RefCell<Option<(usize, Tags)>>>, // a flush landed: how many ops it carried, and the server's doc
    ctx: egui::Context,
}

async fn tracks_cache() -> Result<Cache, JsValue> {
    JsFuture::from(web_sys::window().unwrap().caches()?.open("tracks")).await?.dyn_into()
}

fn pcm(id: u64) -> String {
    format!("/api/tracks/{id}/pcm")
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

impl Offline {
    pub fn new(ctx: &egui::Context) -> Self {
        let pending = storage().and_then(|s| s.get_item(PENDING).ok()?).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        let o = Self {
            have: Rc::default(), busy: Rc::default(), unreachable: Rc::default(), pending,
            sending: Rc::default(), synced: Rc::default(), ctx: ctx.clone(),
        };
        let (have, ctx) = (o.have.clone(), ctx.clone());
        spawn_local(async move {
            let Ok(c) = tracks_cache().await else { return };
            let Ok(keys) = JsFuture::from(c.keys()).await else { return };
            for r in js_sys::Array::from(&keys) {
                let url = r.unchecked_into::<web_sys::Request>().url();
                let id: Option<u64> = url.strip_suffix("/pcm").and_then(|u| u.rsplit('/').next()).and_then(|s| s.parse().ok());
                have.borrow_mut().extend(id);
            }
            ctx.request_repaint();
        });
        o
    }

    /// Download the track for offline use, or drop the offline copy (after asking).
    pub fn toggle(&self, t: &Track, note: Rc<RefCell<Option<String>>>) {
        let id = t.id;
        if self.busy.borrow().contains(&id) {
            return;
        }
        let name = t.rel.rsplit('/').next().unwrap_or(&t.rel).to_owned();
        let drop = self.have.borrow().contains(&id);
        if drop && !web_sys::window().unwrap().confirm_with_message(&format!("Remove the offline copy of {name}?")).unwrap_or(false) {
            return;
        }
        let (have, busy, ctx) = (self.have.clone(), self.busy.clone(), self.ctx.clone());
        busy.borrow_mut().insert(id);
        spawn_local(async move {
            let r: Result<(), JsValue> = async {
                let c = tracks_cache().await?;
                if drop {
                    JsFuture::from(c.delete_with_str(&pcm(id))).await?;
                    JsFuture::from(c.delete_with_str(&format!("/api/tracks/{id}/art"))).await?;
                    have.borrow_mut().remove(&id);
                } else {
                    JsFuture::from(c.add_with_str(&pcm(id))).await?; // fetch + store; sw.js serves it from now on
                    JsFuture::from(c.add_with_str(&format!("/api/tracks/{id}/art"))).await?; // lock-screen art offline
                    have.borrow_mut().insert(id);
                    // ask the browser not to evict downloads when the phone runs low on space
                    if let Ok(p) = web_sys::window().unwrap().navigator().storage().persist() {
                        let _ = JsFuture::from(p).await;
                    }
                }
                Ok(())
            }
            .await;
            busy.borrow_mut().remove(&id);
            *note.borrow_mut() = Some(match (r, drop) {
                (Ok(()), true) => format!("removed {name} from this device"),
                (Ok(()), false) => format!("{name} is available offline"),
                (Err(_), _) => format!("offline copy of {name}: failed (is the Pi reachable?)"),
            });
            ctx.request_repaint();
        });
    }

    /// Remember an edit already applied to the local doc; `flush` sends it.
    pub fn queue(&mut self, op: Op) {
        self.pending.push(op);
        self.save();
    }

    fn save(&self) {
        if let Some(s) = storage() {
            let _ = s.set_item(PENDING, &serde_json::to_string(&self.pending).unwrap());
        }
    }

    /// Send the queued edits (one request at a time); the answer lands in `take_synced`.
    pub fn flush(&self) {
        if self.pending.is_empty() || self.sending.replace(true) {
            return;
        }
        let (n, body) = (self.pending.len(), serde_json::to_string(&self.pending).unwrap());
        let (sending, synced, unreachable, ctx) = (self.sending.clone(), self.synced.clone(), self.unreachable.clone(), self.ctx.clone());
        spawn_local(async move {
            let r: Result<Tags, JsValue> = async {
                let init = RequestInit::new();
                init.set_method("POST");
                init.set_body(&body.into());
                let res: Response = JsFuture::from(web_sys::window().unwrap().fetch_with_str_and_init("/api/tags/ops", &init)).await?.dyn_into()?;
                if !res.ok() {
                    return Err("server".into());
                }
                let text = JsFuture::from(res.text()?).await?.as_string().unwrap_or_default();
                serde_json::from_str(&text).map_err(|e| e.to_string().into())
            }
            .await;
            unreachable.set(r.is_err());
            if let Ok(doc) = r {
                *synced.borrow_mut() = Some((n, doc));
            }
            sending.set(false);
            ctx.request_repaint();
        });
    }

    /// The server's doc after a flush, with edits made since then re-applied on top.
    pub fn take_synced(&mut self) -> Option<Tags> {
        let (n, mut doc) = self.synced.borrow_mut().take()?;
        self.pending.drain(..n);
        self.save();
        self.pending.iter().for_each(|op| doc.apply(op));
        Some(doc)
    }
}
