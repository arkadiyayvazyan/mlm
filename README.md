# mlm

Self-hosted music player. Rust (axum) backend streams AIFF as WAV on the fly (byte swap, no decoding,
byte-exact seeking); other formats are served as-is. Lit frontend with a double-buffered player so
the next track is already loaded when you skip.

## Dev

    make run                       # bun build + cargo run, MLM_DIR defaults to .
    MLM_DIR=~/Music make run

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
