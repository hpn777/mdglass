#!/usr/bin/env bash
# mdv installer for Ubuntu
#   ./install.sh                 build + install mdv to /usr/local/bin
#   ./install.sh --with-desktop  also register "Open Markdown with mdv" in the file manager
#   ./install.sh --uninstall     remove binary (and desktop entry if present)
set -euo pipefail

BIN_NAME=mdv
PREFIX="${PREFIX:-/usr/local/bin}"
DESKTOP_FILE=/usr/share/applications/mdv.desktop
cd "$(dirname "$0")"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    mkdir -p "$PREFIX" 2>/dev/null || true
    if [ ! -w "$PREFIX" ]; then
        command -v sudo >/dev/null 2>&1 && SUDO="sudo" || { echo "mdv: $PREFIX is not writable and sudo is unavailable; try PREFIX=\$HOME/.local/bin" >&2; exit 1; }
    fi
fi

if [ "${1:-}" = "--uninstall" ]; then
    $SUDO rm -f "$PREFIX/$BIN_NAME" "$DESKTOP_FILE"
    echo ">> mdv uninstalled."
    exit 0
fi

# 1. Rust toolchain (installs rustup with the minimal profile if cargo is absent)
if ! command -v cargo >/dev/null 2>&1; then
    echo ">> Rust toolchain not found - installing rustup (minimal profile)..."
    if command -v curl >/dev/null 2>&1 || { $SUDO apt-get update -q && $SUDO apt-get install -y curl; }; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
    else
        echo "mdv: need curl (or an existing cargo) - run: sudo apt-get install curl" >&2
        exit 1
    fi
    # shellcheck disable=SC1091
    . "${CARGO_HOME:-$HOME/.cargo}/env"
fi

# 2. Build release binary
echo ">> Building mdv (release)..."
cargo build --release --locked 2>/dev/null || cargo build --release
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)"
BIN="$TARGET_DIR/release/$BIN_NAME"
[ -f "$BIN" ] || { echo "mdv: build produced no binary at $BIN" >&2; exit 1; }

# 3. Install
$SUDO install -Dm755 "$BIN" "$PREFIX/$BIN_NAME"
echo ">> Installed $PREFIX/$BIN_NAME ($("$PREFIX/$BIN_NAME" --version))"

# 4. Optional desktop entry so file managers can open .md files in a terminal
if [ "${1:-}" = "--with-desktop" ]; then
    $SUDO tee "$DESKTOP_FILE" >/dev/null <<EOF
[Desktop Entry]
Type=Application
Name=mdv
Comment=Terminal markdown viewer
Exec=x-terminal-emulator -e mdv %f
Terminal=false
MimeType=text/markdown;
NoDisplay=false
EOF
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database >/dev/null 2>&1 || true
    echo ">> Registered desktop entry ($DESKTOP_FILE)"
fi

echo ">> Done. Try: mdv README.md"
