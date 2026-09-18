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

Env: `MLM_DIR` (music root), `MLM_ADDR` (default 0.0.0.0:8080), `MLM_CACHE` (index JSON path).
`POST /api/rescan` re-indexes; only files with a changed mtime are re-tagged.
