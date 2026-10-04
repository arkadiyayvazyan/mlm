PI ?= pi.lan
TARGET = aarch64-unknown-linux-musl

ui:
	cargo build -p mlm-ui --release --target wasm32-unknown-unknown
	wasm-bindgen --target web --no-typescript --out-dir static target/wasm32-unknown-unknown/release/mlm-ui.wasm

run: ui
	cargo run

pi: ui
	cargo zigbuild --release --target $(TARGET)
	ssh $(PI) "sudo systemctl stop mlm 2>/dev/null; true"
	scp target/$(TARGET)/release/mlm $(PI):~/mlm
	ssh $(PI) "sudo systemctl start mlm"

caddy:
	scp Caddyfile $(PI):/tmp/Caddyfile
	ssh $(PI) "sudo mv /tmp/Caddyfile /etc/caddy/Caddyfile && sudo systemctl reload caddy"

# what pasted YouTube links need, where the service's PATH finds it: yt-dlp, and bun for it to run YouTube's
# player JS with (ffmpeg comes from apt). Rerun to update yt-dlp when YouTube breaks it.
ytdl-deps:
	ssh $(PI) "sudo curl -fL -o /usr/local/bin/yt-dlp https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux_aarch64 \
		&& sudo chmod +x /usr/local/bin/yt-dlp && sudo ln -sf ~/.bun/bin/bun /usr/local/bin/bun && yt-dlp --version"

.PHONY: ui run pi caddy ytdl-deps
