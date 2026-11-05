#!/usr/bin/env bash
# mdv installer for Ubuntu. Run as your normal user; sudo is used only for
# the copy steps that need it.
#   ./install.sh                 build + install mdv to /usr/local/bin
#   ./install.sh --with-desktop  also add mdv to the file manager's "Open With"
#   ./install.sh --uninstall     remove the binary and desktop entry
#   PREFIX=~/.local/bin ./install.sh   install without sudo
set -euo pipefail

BIN_NAME=mdv
PREFIX="${PREFIX:-/usr/local/bin}"
ACTION=install
DESKTOP=0
for arg in "$@"; do
    case "$arg" in
        --with-desktop) DESKTOP=1 ;;
        --uninstall) ACTION=uninstall ;;
        -h|--help) sed -n '2,7s/^# \{0,1\}//p' "$0"; exit 0 ;;
        *) echo "mdv: unknown option $arg (try --help)" >&2; exit 2 ;;
    esac
done
cd "$(dirname "$0")"

if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
    echo "mdv: run ./install.sh without sudo; it asks for sudo only when installing." >&2
    exit 1
fi

# Prints "sudo" when writing into directory $1 needs it.
sudo_for() {
    local dir="$1"
    while [ ! -d "$dir" ]; do dir="$(dirname "$dir")"; done
    if [ -w "$dir" ]; then
        echo ""
    elif command -v sudo >/dev/null 2>&1; then
        echo "sudo"
    else
        echo "mdv: $1 is not writable and sudo is unavailable; try PREFIX=\$HOME/.local/bin" >&2
        exit 1
    fi
}

# The menu entry goes next to the binary's tree: system-wide for system
# prefixes, per-user when installing under $HOME.
case "$PREFIX" in
    "$HOME"/*) APPS_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications" ;;
    *) APPS_DIR=/usr/share/applications ;;
esac
DESKTOP_FILE="$APPS_DIR/mdv.desktop"

if [ "$ACTION" = uninstall ]; then
    $(sudo_for "$PREFIX") rm -f "$PREFIX/$BIN_NAME"
    [ -e "$DESKTOP_FILE" ] && $(sudo_for "$APPS_DIR") rm -f "$DESKTOP_FILE"
    echo ">> mdv uninstalled."
    exit 0
fi

# 1. Rust toolchain (installs rustup with the minimal profile if cargo is absent)
if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo ">> Rust toolchain not found - installing rustup (minimal profile)..."
    if ! command -v curl >/dev/null 2>&1; then
        sudo apt-get update -q && sudo apt-get install -y curl
    fi
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
    # shellcheck disable=SC1091
    . "${CARGO_HOME:-$HOME/.cargo}/env"
fi
if ! command -v cc >/dev/null 2>&1; then
    echo ">> C linker not found - installing build-essential..."
    sudo apt-get update -q && sudo apt-get install -y build-essential
fi

# 2. Build release binary
echo ">> Building mdv (release)..."
cargo build --release --locked
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)"
BIN="$TARGET_DIR/release/$BIN_NAME"
[ -f "$BIN" ] || { echo "mdv: build produced no binary at $BIN" >&2; exit 1; }

# 3. Install
$(sudo_for "$PREFIX") install -Dm755 "$BIN" "$PREFIX/$BIN_NAME"
echo ">> Installed $PREFIX/$BIN_NAME ($("$PREFIX/$BIN_NAME" --version))"
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) echo ">> Note: $PREFIX is not on your PATH" ;;
esac

# 4. Optional "Open With" entry: launches mdv in the default terminal
if [ "$DESKTOP" = 1 ]; then
    entry="[Desktop Entry]
Type=Application
Name=mdv
Comment=Terminal markdown viewer
Exec=x-terminal-emulator -e $PREFIX/$BIN_NAME %f
Terminal=false
MimeType=text/markdown;text/x-markdown;
NoDisplay=true"
    printf '%s\n' "$entry" | $(sudo_for "$APPS_DIR") install -Dm644 /dev/stdin "$DESKTOP_FILE"
    command -v update-desktop-database >/dev/null 2>&1 && $(sudo_for "$APPS_DIR") update-desktop-database "$APPS_DIR" >/dev/null 2>&1 || true
    echo ">> Added desktop entry $DESKTOP_FILE"
fi

echo ">> Done. Try: mdv README.md"
