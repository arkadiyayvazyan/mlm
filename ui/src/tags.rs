//! User tags: name -> shortcut key, and track (relative path) -> tag names. Shared by the UI and the server
//! (`src/main.rs` includes this file): edits travel as `Op`s that both sides apply, so offline phones and stale
//! tabs never overwrite each other's changes.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const RESERVED: &str = " jkhl?"; // keys already taken by the player

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct Tags {
    pub keys: BTreeMap<String, String>,
    pub tracks: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub hues: BTreeMap<String, u16>, // tag name -> pastel hue in degrees
}

/// One tag edit. Idempotent: `Tag` says on or off rather than "toggle", so replaying a queued op is harmless.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    Tag { rel: String, name: String, on: bool },
    Define { name: String, key: String, hue: u16 }, // create a tag, or set its key / hue
    Remove { name: String },
    Rename { from: String, to: String }, // keeps the key, hue and tracks
}

impl Tags {
    pub fn apply(&mut self, op: &Op) {
        match op {
            // a tag deleted elsewhere meanwhile isn't resurrected by a queued "on"
            Op::Tag { rel, name, on } if *on != self.has(rel, name) && (!on || self.keys.contains_key(name)) => self.toggle(rel, name),
            Op::Tag { .. } => {}
            Op::Define { name, key, hue } => {
                self.keys.insert(name.clone(), key.clone());
                self.hues.insert(name.clone(), *hue);
            }
            Op::Remove { name } => self.remove(name),
            Op::Rename { from, to } => self.rename(from, to),
        }
    }

    pub fn of(&self, rel: &str) -> &[String] {
        self.tracks.get(rel).map_or(&[], |v| v)
    }

    pub fn has(&self, rel: &str, name: &str) -> bool {
        self.of(rel).iter().any(|n| n == name)
    }

    /// Add `name` to the track if missing, else remove it. Tracks with no tags are dropped from the doc.
    pub fn toggle(&mut self, rel: &str, name: &str) {
        let v = self.tracks.entry(rel.to_string()).or_default();
        match v.iter().position(|n| n == name) {
            Some(i) => { v.remove(i); }
            None => v.push(name.to_string()),
        }
        if v.is_empty() {
            self.tracks.remove(rel);
        }
    }

    /// New tag definition, or an error message the form can show.
    pub fn add(&mut self, name: &str, key: &str) -> Result<(), String> {
        self.check(name, key, None)?;
        self.keys.insert(name.trim().to_string(), key.to_string());
        Ok(())
    }

    /// A tag's new name and key as the ops that get there (none when nothing changed), or an error message.
    pub fn edit(&self, old: &str, name: &str, key: &str) -> Result<Vec<Op>, String> {
        self.check(name, key, Some(old))?;
        let (name, mut ops) = (name.trim(), vec![]);
        if name != old {
            ops.push(Op::Rename { from: old.into(), to: name.into() });
        }
        if self.keys.get(old).is_some_and(|k| k != key) {
            ops.push(Op::Define { name: name.into(), key: key.into(), hue: self.hues.get(old).copied().unwrap_or(0) });
        }
        Ok(ops)
    }

    /// Name and key valid for a new tag, or for the existing tag `except` (which may keep its own).
    fn check(&self, name: &str, key: &str, except: Option<&str>) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("name required".into());
        }
        if self.keys.contains_key(name) && except != Some(name) {
            return Err(format!("\"{name}\" exists"));
        }
        if key.chars().count() != 1 {
            return Err("one key".into());
        }
        if RESERVED.contains(key) {
            return Err(format!("{} is a player key", if key == " " { "space" } else { key }));
        }
        if let Some((used, _)) = self.keys.iter().find(|(n, k)| *k == key && except != Some(n.as_str())) {
            return Err(format!("{key} is \"{used}\""));
        }
        Ok(())
    }

    /// Rename a tag everywhere; a no-op when `from` is gone or `to` already exists (edited elsewhere meanwhile).
    pub fn rename(&mut self, from: &str, to: &str) {
        if from == to || !self.keys.contains_key(from) || self.keys.contains_key(to) {
            return;
        }
        let (key, hue) = (self.keys.remove(from).unwrap(), self.hues.remove(from));
        self.keys.insert(to.to_string(), key);
        if let Some(h) = hue {
            self.hues.insert(to.to_string(), h);
        }
        for v in self.tracks.values_mut() {
            v.iter_mut().filter(|n| *n == from).for_each(|n| *n = to.to_string());
        }
    }

    /// Give each tag without a hue a random one, as far as possible from the hues already taken
    /// (best of 24 random candidates). Returns whether anything changed, i.e. the doc needs saving.
    pub fn color_missing(&mut self, mut rand: impl FnMut() -> f64) -> bool {
        let missing: Vec<String> = self.keys.keys().filter(|n| !self.hues.contains_key(*n)).cloned().collect();
        for n in &missing {
            let gap = |h: u16| self.hues.values().map(|&o| { let d = h.abs_diff(o); d.min(360 - d) }).min().unwrap_or(180);
            let h = (0..24).map(|_| (rand() * 360.0) as u16 % 360).max_by_key(|&h| gap(h)).unwrap();
            self.hues.insert(n.clone(), h);
        }
        !missing.is_empty()
    }

    /// Delete the definition and strip the name from every track.
    pub fn remove(&mut self, name: &str) {
        self.keys.remove(name);
        self.hues.remove(name);
        self.tracks.retain(|_, v| {
            v.retain(|n| n != name);
            !v.is_empty()
        });
    }
}

#[test]
fn add_toggle_remove() {
    let mut t = Tags::default();
    t.add("fav", "f").unwrap();
    t.add("chill", "c").unwrap();
    assert_eq!(t.add(" ", "x").unwrap_err(), "name required");
    assert_eq!(t.add("fav", "x").unwrap_err(), "\"fav\" exists");
    assert!(t.add("loud", "j").unwrap_err().contains("player key"));
    assert_eq!(t.add("loud", " ").unwrap_err(), "space is a player key");
    assert_eq!(t.add("loud", "f").unwrap_err(), "f is \"fav\"");
    assert_eq!(t.add("loud", "ab").unwrap_err(), "one key");

    t.toggle("a/1.aiff", "fav");
    t.toggle("a/1.aiff", "chill");
    t.toggle("b/2.mp3", "fav");
    assert_eq!(t.of("a/1.aiff"), ["fav", "chill"]);
    assert_eq!(t.of("b/2.mp3"), ["fav"]);
    t.toggle("b/2.mp3", "fav");
    assert!(!t.has("b/2.mp3", "fav"));
    assert!(!t.tracks.contains_key("b/2.mp3")); // empty entries dropped

    t.remove("fav");
    assert_eq!(t.keys.keys().collect::<Vec<_>>(), ["chill"]);
    assert_eq!(t.tracks.len(), 1);
    assert_eq!(t.of("a/1.aiff"), ["chill"]);
    t.remove("chill");
    assert!(t.tracks.is_empty());
}

#[test]
fn ops_replay_and_merge() {
    let mut a = Tags::default();
    let ops = [
        Op::Define { name: "fav".into(), key: "f".into(), hue: 10 },
        Op::Tag { rel: "x.aiff".into(), name: "fav".into(), on: true },
        Op::Tag { rel: "x.aiff".into(), name: "fav".into(), on: true }, // replayed: still on once
    ];
    ops.iter().for_each(|o| a.apply(o));
    assert_eq!(a.of("x.aiff"), ["fav"]);
    let mut b = a.clone(); // another device removed the tag while this one was offline...
    b.apply(&Op::Remove { name: "fav".into() });
    b.apply(&Op::Tag { rel: "y.aiff".into(), name: "fav".into(), on: true }); // ...then this phone's queued op lands
    assert!(b.tracks.is_empty() && b.keys.is_empty());
    let json = serde_json::to_string(&ops[1]).unwrap();
    assert_eq!(json, r#"{"op":"tag","rel":"x.aiff","name":"fav","on":true}"#);
}

#[test]
fn edit_and_rename() {
    let mut t = Tags::default();
    t.add("fav", "f").unwrap();
    t.add("chill", "c").unwrap();
    t.toggle("a/1.aiff", "fav");
    assert_eq!(t.edit("fav", "fav", "f").unwrap(), []); // unchanged: nothing to send
    assert_eq!(t.edit("fav", "chill", "f").unwrap_err(), "\"chill\" exists");
    assert_eq!(t.edit("fav", "fav", "c").unwrap_err(), "c is \"chill\"");
    let ops = t.edit("fav", "best", "b").unwrap();
    assert_eq!(ops.len(), 2);
    ops.iter().for_each(|o| t.apply(o));
    assert_eq!(t.keys.get("best").unwrap(), "b");
    assert!(!t.keys.contains_key("fav"));
    assert_eq!(t.of("a/1.aiff"), ["best"]);
    t.apply(&Op::Rename { from: "best".into(), to: "chill".into() }); // taken meanwhile: ignored
    assert!(t.keys.contains_key("best"));
}

#[test]
fn hues_are_spread_out() {
    let mut t = Tags::default();
    for (n, k) in [("fav", "f"), ("chill", "c"), ("peak", "p"), ("warmup", "w"), ("vocal", "v")] {
        t.add(n, k).unwrap();
    }
    let mut seed = 1u64; // LCG so the test is deterministic
    let mut rand = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 11) as f64 / (1u64 << 53) as f64 };
    assert!(t.color_missing(&mut rand));
    assert!(!t.color_missing(&mut rand)); // all colored: nothing to save
    let h: Vec<u16> = t.hues.values().copied().collect();
    let min = h.iter().enumerate().flat_map(|(i, a)| h[i + 1..].iter().map(move |b| { let d = a.abs_diff(*b); d.min(360 - d) })).min().unwrap();
    assert!(min >= 30, "hues too close: {h:?}");
    t.remove("fav");
    assert!(!t.hues.contains_key("fav"));
}
