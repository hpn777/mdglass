BIN     := mdv
PREFIX  ?= /usr/local/bin

.PHONY: build release test install uninstall clean

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

TARGET_DIR := $(shell cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)

install: release
	sudo install -Dm755 $(TARGET_DIR)/release/$(BIN) $(PREFIX)/$(BIN)

uninstall:
	sudo rm -f $(PREFIX)/$(BIN)

clean:
	cargo clean
