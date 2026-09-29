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

.PHONY: ui run pi caddy
