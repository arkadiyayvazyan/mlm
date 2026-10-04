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

## Phones: lock screen, background, share

The Media Session API (`ui/src/media.rs`) drives the Android notification / lock-screen player and headphone,
Bluetooth, car and watch controls: title, artist, album, cover art (`GET /api/tracks/{id}/art`, the app icon when
a file has none), play/pause, previous/next, ±30 s, and a draggable progress bar. Chrome only shows it (and iOS
only keeps audio running when locked) while a media element plays, so a looping silent `/silence.wav` plays along
with the Web Audio output; on iOS 17.5+ `navigator.audioSession.type = "playback"` keeps Web Audio alive too.
On phones the player bar has ⬇ / bpm / share buttons. Share sends the original file for MP3/M4A/WAV/FLAC/Ogg;
Android's share sheet refuses AIFF, so AIFF goes as a lossless WAV built from the already-loaded audio (no tags).
The manifest has a monochrome icon for Android 13+ themed icons; when Chrome offers installation, an
"install app" button appears under the search box.

## Offline

Hold a track (right-click on desktop) to keep it on the device; again to remove it. A 💾 in the first column marks
downloaded tracks. The copy is the track's `/pcm` stream plus cover art in Cache Storage, and `ui/sw.js` (a service
worker) plays it from there; the app itself, the track list and the tags are cached by `sw.js` too (network first,
4 s, else the last copy), so the installed app opens and plays downloaded tracks anywhere. Away from the Pi the
other tracks are faded and the queue only holds downloaded ones.

Tag edits are ops (`tags::Op`, `ui/src/tags.rs`, shared with the server): applied at once, queued in localStorage,
and sent to `POST /api/tags/ops` whenever the Pi answers (retried every 15 s). The server applies them to its doc,
so offline phones and stale tabs never overwrite each other's edits.

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

## YouTube

Paste a YouTube or YouTube Music link into the search box (or type it and press enter): the server downloads it
(`POST /api/ytdl`, body = the link) as a 320 kbps MP3 with title / artist tags and square cover art into
`MLM_DIR/ytdl/`, named `Artist - Track.mp3` (or the video title), re-indexes, and the search shows the new track,
ready to play, analyze and tag. It needs `yt-dlp`, `ffmpeg` and `bun` on the service's PATH: `make ytdl-deps`
installs yt-dlp and links bun into `/usr/local/bin` on the Pi; rerun it to update yt-dlp when YouTube breaks it.

## Tags

Open the **tags** panel under the search box to create a tag with a one-key shortcut. Pressing that
key (or clicking the tag button) toggles the tag on the playing track; typing a tag name in search
filters by it. Tags are stored in `MLM_TAGS` keyed by path relative to `MLM_DIR`, so they survive
rescans and moving the library root.
