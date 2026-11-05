BIN     := mdv
PREFIX  ?= /usr/local/bin
SUDO    ?= sudo

TARGET_DIR = $(shell cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)

.PHONY: build release test check install uninstall clean

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

# unit tests plus end-to-end checks of the release binary in a real pty
check: test release
	python3 tests/pty_smoke.py $(TARGET_DIR)/release/$(BIN)

install: release
	$(SUDO) install -Dm755 $(TARGET_DIR)/release/$(BIN) $(PREFIX)/$(BIN)

uninstall:
	$(SUDO) rm -f $(PREFIX)/$(BIN)

clean:
	cargo clean
