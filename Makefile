PI ?= pi.lan
TARGET = aarch64-unknown-linux-musl

web:
	NODE_ENV=production bun build web/app.ts --outfile static/app.js --minify

run: web
	cargo run

pi: web
	cargo zigbuild --release --target $(TARGET)
	ssh $(PI) "sudo systemctl stop mlm 2>/dev/null; true"
	scp target/$(TARGET)/release/mlm $(PI):~/mlm
	ssh $(PI) "sudo systemctl start mlm"

.PHONY: web run pi
