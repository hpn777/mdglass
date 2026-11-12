#!/usr/bin/env python3
"""Regenerate Flatpak's offline registry sources after changing Cargo.lock."""
import json
import sys
from pathlib import Path
import tomllib

root = Path(__file__).resolve().parents[2]
lock = tomllib.loads((root / "Cargo.lock").read_text())
sources = []
for package in lock["package"]:
    if "source" not in package:
        continue
    if package["source"] != "registry+https://github.com/rust-lang/crates.io-index":
        raise ValueError(f"Unsupported dependency source: {package['source']}")
    name, version, checksum = (package[key] for key in ("name", "version", "checksum"))
    dest = f"cargo/vendor/{name}-{version}"
    sources.extend([
        {"type": "archive", "archive-type": "tar-gzip",
         "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
         "sha256": checksum, "dest": dest},
        {"type": "inline", "contents": json.dumps({"package": checksum, "files": {}}),
         "dest": dest, "dest-filename": ".cargo-checksum.json"},
    ])
sources.append({
    "type": "inline",
    "contents": '[source.vendored-sources]\ndirectory = "cargo/vendor"\n\n'
                '[source.crates-io]\nreplace-with = "vendored-sources"\n',
    "dest": "cargo", "dest-filename": "config",
})
destination = Path(__file__).with_name("cargo-sources.json")
generated = json.dumps(sources, indent=4) + "\n"
if sys.argv[1:] == ["--check"]:
    if destination.read_text() != generated:
        sys.exit("Stale Flatpak dependencies: run packaging/flatpak/update-cargo-sources.py")
elif sys.argv[1:]:
    sys.exit("Usage: update-cargo-sources.py [--check]")
else:
    destination.write_text(generated)
