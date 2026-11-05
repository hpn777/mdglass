BIN     := mdglass
PREFIX  ?= /usr/local/bin
SUDO    ?= sudo

TARGET_DIR = $(shell cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)

.PHONY: build release test check deb install uninstall clean

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

# unit tests, end-to-end pty checks of the release binary, the GUI's C ABI
# audit, and a scripted GUI session on a headless Broadway display
check: test release
	python3 tests/pty_smoke.py $(TARGET_DIR)/release/$(BIN)
	python3 tests/gui_abi_audit.py src/gui.rs
	python3 tests/gui_smoke.py $(TARGET_DIR)/release/$(BIN)

# Ubuntu/Debian package in dist/
deb:
	packaging/build-deb.sh

install: release
	$(SUDO) install -Dm755 $(TARGET_DIR)/release/$(BIN) $(PREFIX)/$(BIN)

uninstall:
	$(SUDO) rm -f $(PREFIX)/$(BIN)

clean:
	cargo clean
