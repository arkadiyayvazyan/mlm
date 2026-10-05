#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod pcm;
mod tags;
#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod media;
#[cfg(target_arch = "wasm32")]
mod offline;
#[cfg(target_arch = "wasm32")]
mod player;

#[derive(serde::Deserialize, Clone)]
pub struct Track {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub rate: u32,
    pub bpm: u32,
    pub ext: String,
    pub rel: String,
    pub added: u64, // unix seconds
}

pub fn fmt(s: f64) -> String {
    format!("{}:{:02}", (s / 60.0) as u64, (s % 60.0) as u64)
}

/// The search box's little language: terms joined by `&&` must all be in `hay`, `||` separates alternatives
/// (so `a && b || c` is "a and b, or c"). Both arguments lowercase. A term still being typed (empty) doesn't count,
/// and a query with no terms at all matches everything.
pub fn matches(query: &str, hay: &str) -> bool {
    let mut any = false;
    for alt in query.split("||") {
        let mut terms = alt.split("&&").map(str::trim).filter(|t| !t.is_empty()).peekable();
        if terms.peek().is_some() {
            any = true;
            if terms.all(|t| hay.contains(t)) {
                return true;
            }
        }
    }
    !any
}

#[cfg(test)]
#[test]
fn search_and_or() {
    let hay = "ytdl/gwen mccrae - keep the fire burning.mp3 easy disco";
    for q in ["", "  ", "easy", "fire burning", "easy || house", "house || disco", "easy && disco", " easy  &&  disco ", "easy && house || disco",
              "house || easy && disco", "easy ||", "easy || ", "|| easy", "easy &&", "&&", "||"] {
        assert!(matches(q, hay), "{q:?} should match");
    }
    for q in ["house", "easy && house", "house || techno", "house && easy || techno", "house ||", "easy && disco && house", "easy&disco", "easy | disco"] {
        assert!(!matches(q, hay), "{q:?} should not match");
    }
    assert!(matches("r&b || soul", "some r&b tune")); // a single & or | is just text
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;
    let doc = web_sys::window().unwrap().document().unwrap();
    let canvas: web_sys::HtmlCanvasElement = doc.get_element_by_id("mlm").unwrap().unchecked_into();
    wasm_bindgen_futures::spawn_local(async move {
        let app: eframe::AppCreator = Box::new(|cc| Ok(Box::new(app::App::new(cc))));
        eframe::WebRunner::new().start(canvas.clone(), Default::default(), app).await.expect("start eframe");
        let _ = canvas.focus(); // keys go to the canvas: take them without a first click
        // ctrl+d / ctrl+a are the app's download / analyze: stop the browser's bookmark / select-all
        let keys = wasm_bindgen::closure::Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
            if (e.ctrl_key() || e.meta_key()) && matches!(e.key().as_str(), "d" | "a") {
                e.prevent_default();
            }
        });
        // capture phase: eframe stops key events from bubbling past the canvas
        let _ = web_sys::window().unwrap().add_event_listener_with_callback_and_bool("keydown", keys.as_ref().unchecked_ref(), true);
        keys.forget();
    });
}

/// Natively the crate only exists for `cargo test -p mlm-ui`.
#[cfg(not(target_arch = "wasm32"))]
fn main() {}
