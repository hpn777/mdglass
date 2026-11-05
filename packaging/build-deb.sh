#!/usr/bin/env bash
# Build an Ubuntu/Debian package: dist/mdglass_<version>-<rev>_<arch>.deb
# Needs only dpkg-dev tools (dpkg-deb, dpkg-shlibdeps) and cargo.
#   MAINTAINER="Name <email>"  override the Maintainer field
#   DEB_REVISION=N             package revision (default: from debian/changelog)
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
REVISION="${DEB_REVISION:-$(dpkg-parsechangelog -S Version | sed 's/.*-//')}"
ARCH="$(dpkg --print-architecture)"
MAINTAINER="${MAINTAINER:-Rafal Okninski <hpn777@gmail.com>}"
PKG="mdglass_${VERSION}-${REVISION}_${ARCH}"

cargo build --release --locked
TARGET_DIR="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -1)"
BIN="$TARGET_DIR/release/mdglass"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ROOT="$WORK/$PKG"

install -Dm755 "$BIN" "$ROOT/usr/bin/mdglass"
install -Dm644 packaging/mdglass.1 "$ROOT/usr/share/man/man1/mdglass.1"
gzip -9n "$ROOT/usr/share/man/man1/mdglass.1"
install -Dm644 debian/copyright "$ROOT/usr/share/doc/mdglass/copyright"
install -Dm644 debian/changelog "$ROOT/usr/share/doc/mdglass/changelog.Debian"
gzip -9n "$ROOT/usr/share/doc/mdglass/changelog.Debian"
install -Dm644 README.md "$ROOT/usr/share/doc/mdglass/README.md"
install -Dm644 packaging/io.github.hpn777.MdGlass.metainfo.xml "$ROOT/usr/share/metainfo/io.github.hpn777.MdGlass.metainfo.xml"
install -Dm644 packaging/io.github.hpn777.MdGlass.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/io.github.hpn777.MdGlass.svg"
# GNOME preview is the visible "Open With" app; the terminal pager stays as a
# hidden alternative. desktop-file-utils triggers refresh the cache on install.
install -Dm644 packaging/io.github.hpn777.MdGlass.desktop "$ROOT/usr/share/applications/io.github.hpn777.MdGlass.desktop"
install -Dm644 packaging/mdglass-terminal.desktop "$ROOT/usr/share/applications/mdglass-terminal.desktop"

# Shared-library dependencies, computed from the binary
mkdir -p "$WORK/shlibs/debian"
printf 'Source: mdglass\n\nPackage: mdglass\nArchitecture: any\n' > "$WORK/shlibs/debian/control"
DEPENDS="$(cd "$WORK/shlibs" && dpkg-shlibdeps -O "$ROOT/usr/bin/mdglass" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
[ -n "$DEPENDS" ] || { echo "build-deb: dpkg-shlibdeps found no dependencies" >&2; exit 1; }

mkdir -p "$ROOT/DEBIAN"
# Recommends: the GUI loads GTK4/libadwaita at runtime (dlopen), so
# dpkg-shlibdeps cannot see them
cat > "$ROOT/DEBIAN/control" <<EOF
Package: mdglass
Version: ${VERSION}-${REVISION}
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Installed-Size: $(du -sk --exclude=DEBIAN "$ROOT" | cut -f1)
Depends: ${DEPENDS}
Recommends: libgtk-4-1, libadwaita-1-0
Section: text
Priority: optional
Description: super fast markdown viewer: terminal pager + GNOME preview
 mdglass renders markdown in the terminal with colors, tables, highlighted
 code blocks and clickable links, in a built-in less-style pager with search;
 piped output is plain, word-wrapped text. mdglass --gui opens a native GNOME
 (GTK4/libadwaita) preview window with find, zoom and live reload.
EOF
(cd "$ROOT" && find usr -type f -exec md5sum {} + | sort -k2) > "$ROOT/DEBIAN/md5sums"
chmod 644 "$ROOT/DEBIAN/control" "$ROOT/DEBIAN/md5sums"

mkdir -p dist
dpkg-deb --root-owner-group -Zxz --build "$ROOT" "dist/$PKG.deb" >/dev/null
echo "dist/$PKG.deb"
