# mlm

Self-hosted music player. Rust (axum) backend; the UI in `ui/` is egui compiled to wasm, served at `/`.

Playback runs on an AudioWorklet (`ui/worklet.js`) draining a lock-free SharedArrayBuffer ring that the
main thread fills, so tracks play back to back sample-exactly and UI jank can't cause dropouts.
Every format streams: `GET /api/tracks/{id}/pcm` is AIFF byte-swapped to WAV on the fly (no decoding), or for
MP3/M4A/FLAC/WAV a server-side decode (symphonia) streamed as WAV, so playback starts on the first chunk.

Ctrl+A detects the playing track's BPM (`POST /api/tracks/{id}/analyze`, `src/bpm.rs`) and writes it into the
file's tag: TBPM via the `id3` crate for MP3/AIFF/WAV (every other frame, e.g. DJ software's GEOB cue data, is
written back untouched), `tmpo` via lofty for M4A. The file is edited in place so its birth time ("added") stays;
a `.name.mlm-bak` copy exists only during the write. On 40 library tracks it matched the existing tag in 35.
Ctrl+D downloads the playing track; `?` lists all keys.

## HTTPS

SharedArrayBuffer and AudioWorklet need a secure, cross-origin isolated page: the server sends the
COOP/COEP headers, but the origin must be `localhost` or HTTPS: on the Pi, Caddy (`Caddyfile`,
`make caddy`) serves `https://mlm.lan` with a cert from its local CA. Trust that CA once per device:

    ssh pi.lan sudo cat /var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt > caddy-root.crt
    # Windows: certutil -addstore -user Root caddy-root.crt   (or double-click → Trusted Root Certification Authorities)
    # Firefox (own cert store): Settings → Privacy & Security → View Certificates → Authorities → Import,
    #   tick "Trust this CA to identify websites"
    # Android: Settings → Security → Encryption & credentials → Install a certificate → CA certificate
    # iOS: open the .crt, install the profile, then enable it in General → About → Certificate Trust Settings

It's a PWA (`ui/manifest.json`): on a phone open `https://mlm.lan` and use "Install app" / "Add to
Home Screen". Below 600 pt wide the list switches to two-line rows and the player to touch-sized controls.

## Dev

    rustup target add wasm32-unknown-unknown
    cargo install wasm-bindgen-cli --version 0.2.129   # must match the wasm-bindgen pin in ui/Cargo.toml
    make run                       # wasm build + cargo run, MLM_DIR defaults to .
    MLM_DIR=~/Music make run
    cargo test && cargo test -p mlm-ui

## Deploy to the Pi

    make pi PI=pi@raspberrypi.local   # static aarch64 binary, scp'd to ~/mlm
    # on the Pi:
    sudo cp mlm.service /etc/systemd/system/ && sudo systemctl enable --now mlm

Env: `MLM_DIR` (music root), `MLM_ADDR` (default 0.0.0.0:8080), `MLM_CACHE` (index JSON path),
`MLM_TAGS` (user tags JSON path, default `mlm-tags.json`).
`POST /api/rescan` re-indexes; only files with a changed mtime are re-tagged.

## Tags

Open the **tags** panel under the search box to create a tag with a one-key shortcut. Pressing that
key (or clicking the tag button) toggles the tag on the playing track; typing a tag name in search
filters by it. Tags are stored in `MLM_TAGS` keyed by path relative to `MLM_DIR`, so they survive
rescans and moving the library root.
