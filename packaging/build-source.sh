#!/usr/bin/env bash
# Build Ubuntu source packages for a Launchpad PPA, in dist/source/.
#   packaging/build-source.sh [SERIES...]     default: resolute
#   UNSIGNED=1 packaging/build-source.sh      skip GPG signing (local test)
# The orig tarball is `git archive HEAD` (minus debian/) plus the crates
# vendored with `cargo vendor`, so the package builds without network.
# Upload: dput ppa:hpn777/mdglass dist/source/mdglass_<ver>~<series>1_source.changes
set -euo pipefail
cd "$(dirname "$0")/.."

SERIES=("${@:-resolute}")
UPSTREAM="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
DEBVER="$(dpkg-parsechangelog -S Version)"
OUT="$PWD/dist/source"
ORIG="$OUT/mdglass_${UPSTREAM}.orig.tar.xz"
mkdir -p "$OUT"

if [ ! -f "$ORIG" ]; then
    [ -z "$(git status --porcelain -- . ':!debian')" ] || { echo "build-source: commit your changes first" >&2; exit 1; }
    tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
    git archive --prefix="mdglass-${UPSTREAM}/" HEAD -- . ':!debian' | tar -x -C "$tmp"
    (cd "$tmp/mdglass-${UPSTREAM}" && cargo vendor --locked vendor >/dev/null)
    tar -C "$tmp" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$(git log -1 --format=%ct)" \
        -cJf "$ORIG" "mdglass-${UPSTREAM}"
    echo "orig: $ORIG"
fi

for series in "${SERIES[@]}"; do
    work="$(mktemp -d)"
    tar -xJf "$ORIG" -C "$work"
    src="$work/mdglass-${UPSTREAM}"
    cp -a debian "$src/"
    cp "$ORIG" "$work/"
    # one upload per series: 0.1.0-3 -> 0.1.0-3~resolute1, distribution resolute
    sed -i "1s/^mdglass ([^)]*) [a-z-]*;/mdglass (${DEBVER}~${series}1) ${series};/" "$src/debian/changelog"
    sign=(); [ "${UNSIGNED:-}" = 1 ] && sign=(-us -uc)
    (cd "$src" && dpkg-buildpackage -S -sa -d "${sign[@]}" >/dev/null)
    cp "$work"/mdglass_*~"${series}"1* "$OUT/"
    rm -rf "$work"
    echo "source: $OUT/mdglass_${DEBVER}~${series}1_source.changes"
done
