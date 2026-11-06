# Diagram engines: Mermaid + vectorized ASCII art (research, Nov 2026)

Goal: real vector diagrams in the GNOME preview, small impact on the binary
(current: 647 KB) and on startup. Constraints kept: no WebKit, no Node, GUI
must never abort (release profile is `panic = "abort"`).

All size numbers below were measured by linking each candidate into a hello
world with the same release profile as this repo (lto, strip, panic=abort,
codegen-units=1) and subtracting the 307 KB baseline. All timings on this
machine. Preview of every option: `/tmp/opencode/size/diagram-preview.png`.

## Candidates

### 1. aasvg (crates.io 1.0, BSD-2) — ASCII art → vector SVG
Rust port of Martin Thomson's `aasvg` (the Markdeep diagram algorithm, same
as Go's `goat`). Input: an ASCII-art text block; output: SVG where boxes are
`<path>` lines, arrows are `<polygon>` heads, labels are real text with a
dark/light stylesheet. It is a *recognizer*: it understands `+`/`-`/`|`
boxes and `-->`, `<==` style arrows, NOT prose art.

* Measured cost: **+108 KB**, dependencies none of consequence.
* Measured speed: microseconds; irrelevant next to parsing.
* Limit (verified): Unicode box drawing (`╭─│`) is not recognized — those
  stay text glyphs. A one-line byte translation (`│─┌┐└┘├┤┬┴┼═║` → `|-+++++-|=|`)
  before feeding it makes Unicode art vectorize identically (verified:
  raw = 0 box paths, preprocessed = 6 box paths + arrowhead). CJK labels
  inside boxes still need the column-width care aasvg doesn't do; mis-detected
  art just renders as the current code block, so the failure mode is safe.
* Repo was archived Jun 2026 (absorbed into bearcove/dodeca); the crate and
  the algorithm are stable, and at 108 KB a fork/vendored copy is cheap.

### 2. mermaid-rs-renderer "mmdr" (crates.io 0.3.1, MIT) — Mermaid → SVG
Native Rust implementation of the whole Mermaid pipeline (parser → IR → dagre
layout → SVG). 23 diagram types (flowchart, sequence, class, state, ER, pie,
gantt, timeline, mindmap, gitGraph, C4, …). No browser, no Node.

* Measured cost: **+4.4 MB** (binary 0.31 → 4.68 MB). Its deps are
  regex/serde/fontdb/ttf-parser; PNG export (resvg/usvg) stays off.
* Measured speed: small flowchart 0–2 ms; a 30-node flowchart ~240 ms →
  render once per document/zoom step and cache, never inside the draw
  callback.
* Risks (verify before shipping): young project (0.3.x, README admits visual
  parity with mermaid-cli is still improving), a parse *panic* would abort
  this app under `panic=abort` → would need `panic=unwind` + `catch_unwind`
  around render, or the subprocess variant below.
* Optional `scene` feature (git master only, NOT in 0.3.1): returns
  `Scene { width, height, commands }` of filled paths with *text already
  shaped to outlines* — designed exactly for replaying into a host vector
  painter (we'd dlopen `libcairo-2.so.0`, which is always present: GTK4
  links it). Measured cost over plain mmdr: **+400 KB** (total +4.8 MB);
  scene render 2–19 ms for a small flowchart. Downside: git dependency in a
  `.deb` (pin a rev + `cargo vendor`), and it drags in usvg normalization.

### 3. External-process mode (either engine) — +0 KB
`mmdr` and `aasvg` both ship CLIs (`mmdr -e svg`, `aasvg`). If the binary is
on `PATH` (AUR/brew/cargo-install for mmdr), mdglass shells out, gets SVG on
stdout, displays it; otherwise the block stays code text exactly as today.
Zero size cost, zero new linked deps, no panic exposure (subprocess),
but the feature is opt-in per machine — good first increment while 1–2 mature.

### 4. Ruled out
* mermaid-cli / Puppeteer: needs Node + Chromium (~300 MB, 2 s/diagram).
* Kroki server: network round-trip and documents leave the machine.
* Embedding mermaid-js in a JS engine: needs a DOM shim; effectively
  re-implements a browser.
* `dagre` crate alone (layout only): we would still write the Mermaid parser
  and all 23 renderers; mmdr already is that, MIT.

## Display path inside GtkTextView (shared by all options)
Embed a `GtkDrawingArea` at a `GtkTextChildAnchor`
(`gtk_text_buffer_create_child_anchor` + `gtk_text_view_add_child_at_anchor`,
GTK ≥ 4.0) replacing the code-block lines; block anchors already keep scroll
position across re-renders. Two ways to paint:
* **librsvg via gdk-pixbuf**: `gdk_pixbuf_loader_new_with_type("svg")` →
  `GdkTexture`. +0 KB, uses the system `librsvg2-common` (present on Ubuntu
  desktops; absent → keep code text). Rasterizes at widget size, re-render
  on zoom; crisp enough, sharpness fine at integer-ish zooms.
* **cairo replay of mmdr's `scene`**: dlopen cairo (12 more `gs!` symbols),
  `gtk_snapshot_append_cairo` (GTK ≥ 4.12 — the app already needs 4.12) or
  image-surface → `GdkMemoryTexture` (4.0 fallback). True vector at every
  zoom; only pairs with mmdr git/scene.
Selection/copy/find skip these blocks the same way they skip the soft-wrapped
lines today — worth a README line.

## Sizing summary

| option | binary Δ | runtime dep | scope |
|---|---|---|---|
| aasvg in-process | +108 KB (≈755 KB total) | none | ASCII/box art → vector |
| aasvg + mmdr (SVG) | +4.5 MB (≈5.2 MB) | librsvg (soft) | + all 23 mermaid types |
| aasvg + mmdr (scene, git) | +4.9 MB (≈5.6 MB) | none new | + vector-exact zoom |
| subprocess mmdr/aasvg | +0 KB | user-installed CLIs | opt-in, safest |

## Recommendation
1. **Now:** aasvg in-process with the Unicode→ASCII translation (~1 day of
   work: recognizer gate, one tag, pixbuf display, fallback to today's code
   block). Cheap, visible, matches the existing "fidelity inside text view"
   arc. Gate on "looks like box art" so normal code stays text.
2. **Then:** Mermaid via **subprocess mmdr first** (+0 KB, validates the
   anchor/drawing-area plumbing and the display path under the real test
   suite), and promote to in-process mmdr once it is ≥0.4 with a released
   `scene` feature and a panic-safety story — that also removes the git
   dependency problem for the `.deb`.
3. Both stay behind the same fallback: any parse/render failure keeps the
   current syntax-highlighted code block, so the feature can never make a
   document worse than today.

## As built (all three steps landed)

Shipped shape: `src/vector.rs` (pure recognition/SVG) + the GUI render path
(`picture_for`/`emit_picture` in `src/gui.rs`). Step 2 survives as the CLI
second chance: built-in mmdr renders, and only when it fails an installed
`mmdr` CLI is tried (`mermaid_via_cli`); anything else stays a code block.
Step 3 took the crates.io `scene`-free route (0.3.1, SVG via librsvg); the
git `scene` feature stays unshipped. Release profile switched to
`panic = "unwind"` so both engines run inside `catch_unwind` — a parser
panic degrades the block to text instead of closing the window. Measured
binary: 662 KB to 5.46 MB.

The display path is a GtkPicture at a GtkTextChildAnchor, and four things
about GTK 4.22 (each verified empirically, each a silent or fatal failure
when guessed wrong) shape it:

- `GtkImage` from a pixbuf is deprecated since 4.6 and **silently stays
  empty** on 4.22 (16×16, no pixels, no warning). The live API is
  `GtkPicture` + `GdkMemoryTexture`.
- `GtkTextView` measures a `GtkPicture` child as **0×0** (a GtkLabel at the
  same anchor measures fine), so the picture needs an explicit
  `gtk_widget_set_size_request`.
- The reference from `gtk_text_buffer_create_child_anchor` is **owned by
  the buffer** (the documented contract is transfer-none). Unreferencing it
  finalizes the anchor under the live text and the next re-render `g_error`s
  out of the process (gtktextchild.c widget_segment assertion);
  unreferencing later is a double free (G_IS_OBJECT criticals). Correct
  discipline on 4.22: never unref - the buffer frees the anchor with the
  character, taking the picture widget with it (RSS over 12 live reloads
  stays flat).
- A wrong function name is caught at startup, not runtime: the dlopen
  bootstrap treats missing symbols as fatal, so the picture pipeline's
  optional half (gdk-pixbuf and its introspection helpers -
  `gdk_pixbuf_get_has_alpha` is a header macro on some builds, not an
  exported symbol) loads through a null-tolerant path: no pipeline, no
  pictures, GUI unaffected.
- librsvg (2.61) resolves `var()` **only in stylesheets, never in
  presentation attributes** — aasvg emits `stroke="var(--aasvg-stroke)"`,
  which rasterizes to nothing. `svg_theme` replaces every `var(--aasvg-*)`
  with literal theme colors before loading.

The texture's `GBytes` is built with `g_bytes_new_with_free_func` carrying a
reference to the source pixbuf, so the pixels stay alive exactly as long as
the texture reads them. Caching is two-tier: raw engine SVG (and failures)
per document keyed by (info string, source, dark) and decoded pixbufs per
the same triple at the zoom they were made for, so scrolling, live reload
and resize touch neither engine nor librsvg, while zoom gets a fresh librsvg
render at the target size (crisp vectors, never an upscale). The `dark` bit
is in the SVG key because mermaid bakes its palette in: `Theme::modern()`
paints a white canvas, `Theme::dark()` a `#333333` one, so the diagrams
follow the system light/dark setting; the `mmdr` CLI gets `-t modern|dark`
to match. aasvg output is theme-independent (it carries `var()` colors that
`svg_theme` resolves later) but keys the same way. Both caches prune to the
keys the current emit used, because live reload re-renders with the
cache-retaining flag and would otherwise accumulate every saved revision.
Zoom clamp: one uniform factor across both axes.
Terminal output is unchanged: pictures are a window-only enhancement.
