#!/usr/bin/env bash
# Build an Ubuntu/Debian package: dist/mdv_<version>-<rev>_<arch>.deb
# Needs only dpkg-dev tools (dpkg-deb, dpkg-shlibdeps) and cargo.
#   MAINTAINER="Name <email>"  override the Maintainer field
#   DEB_REVISION=2             package revision (default 1)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
REVISION="${DEB_REVISION:-1}"
ARCH="$(dpkg --print-architecture)"
MAINTAINER="${MAINTAINER:-hypnos <hypnos@localhost>}"
PKG="mdv_${VERSION}-${REVISION}_${ARCH}"

cargo build --release --locked
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)"
BIN="$TARGET_DIR/release/mdv"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ROOT="$WORK/$PKG"

install -Dm755 "$BIN" "$ROOT/usr/bin/mdv"
install -Dm644 packaging/mdv.1 "$ROOT/usr/share/man/man1/mdv.1"
gzip -9n "$ROOT/usr/share/man/man1/mdv.1"
install -Dm644 packaging/copyright "$ROOT/usr/share/doc/mdv/copyright"
install -Dm644 packaging/changelog "$ROOT/usr/share/doc/mdv/changelog.Debian"
gzip -9n "$ROOT/usr/share/doc/mdv/changelog.Debian"
install -Dm644 README.md "$ROOT/usr/share/doc/mdv/README.md"
install -Dm644 packaging/mdv.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/mdv.svg"
# GNOME preview is the visible "Open With" app; the terminal pager stays as a
# hidden alternative. desktop-file-utils triggers refresh the cache on install.
install -Dm644 /dev/stdin "$ROOT/usr/share/applications/mdv.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=mdv
GenericName=Markdown Viewer
Comment=View Markdown documents with a native GNOME preview
Exec=mdv --gui %f
Icon=mdv
Terminal=false
Categories=Utility;TextEditor;
Keywords=markdown;viewer;preview;
MimeType=text/markdown;text/x-markdown;
StartupWMClass=org.mdv.Viewer
EOF
install -Dm644 /dev/stdin "$ROOT/usr/share/applications/mdv-terminal.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=mdv (terminal)
Comment=Terminal markdown viewer with pager
Exec=x-terminal-emulator -e mdv %f
Icon=mdv
Terminal=false
NoDisplay=true
MimeType=text/markdown;text/x-markdown;
EOF

# Shared-library dependencies, computed from the binary
mkdir -p "$WORK/shlibs/debian"
printf 'Source: mdv\n\nPackage: mdv\nArchitecture: any\n' > "$WORK/shlibs/debian/control"
DEPENDS="$(cd "$WORK/shlibs" && dpkg-shlibdeps -O "$ROOT/usr/bin/mdv" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
[ -n "$DEPENDS" ] || { echo "build-deb: dpkg-shlibdeps found no dependencies" >&2; exit 1; }

mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<EOF
Package: mdv
Version: ${VERSION}-${REVISION}
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Installed-Size: $(du -sk --exclude=DEBIAN "$ROOT" | cut -f1)
Depends: ${DEPENDS}
Section: text
Priority: optional
Description: super fast markdown viewer: terminal pager + GNOME preview
 mdv renders markdown in the terminal with colors, tables, syntax-highlighted
 code blocks and clickable links, shown in a built-in less-style pager with
 search; piped output is plain, word-wrapped text. mdv --gui opens a native
 GNOME (GTK4/libadwaita) preview window with find, zoom and live reload.
EOF
(cd "$ROOT" && find usr -type f -exec md5sum {} + | sort -k2) > "$ROOT/DEBIAN/md5sums"
chmod 644 "$ROOT/DEBIAN/control" "$ROOT/DEBIAN/md5sums"

mkdir -p dist
dpkg-deb --root-owner-group -Zxz --build "$ROOT" "dist/$PKG.deb" >/dev/null
echo "dist/$PKG.deb"
