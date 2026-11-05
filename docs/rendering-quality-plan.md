# Desktop rendering quality: grids (tables) and diagrams (ASCII art)

Status: v2, revised after independent review. Scope: `--gui` back end
(`src/gui.rs`), shared logic in `src/doc.rs` / `src/ansi.rs`, plus
`tests/gui_abi_audit.py` and `tests/gui_smoke.py`. The terminal back end is
the fidelity reference.

## Current state

The GUI renders the whole document into one `GtkTextView` (view-wide
`WRAP_WORD_CHAR`, horizontal policy `POLICY_NEVER`). Tables are laid out by
`doc::table_text()` into box-drawn plain-text rows inserted with the `mono`
tag; code blocks (including diagrams) are inserted as plain monospace lines.
Everything that does not fit the window is soft-wrapped by GTK.

## Verified defects (1024x768 Broadway window, Ubuntu, DejaVu Sans Mono)

1. **Wide tables wrap and break.** A 23-column table bottoms out at the
   column-width floor (3 cols, `doc.rs:982,989-996`) = 139 layout columns
   > the fixed 120-col budget (`gui.rs:687`); its border lines are cut at the
   viewport edge (measured last pixel x=991) and rows wrap mid-cell. H-scroll
   is `POLICY_NEVER` (`gui.rs:976`); zooming in reproduces this for tables
   that fit at 100 %.

2. **Wide/fallback glyphs drift columns.** One CJK cell moved the right
   border from x=788 to x=768 (~2.5 mono columns, 20 px). `fc-match
   "monospace:lang=zh"` = DejaVu Sans Mono (no CJK coverage) → Pango falls
   back to a font whose advance ≠ 2x mono; `char_width` says 2 columns,
   pixels say something else, and space padding cannot fix the sum.

3. **Long lines in code/diagrams wrap** like any text (`gui.rs:647-653`) and
   destroy box-drawing art; no way to view it intact.

4. **GUI drops table styling.** `table_text()` flattens cells with
   `spans_plain` (`doc.rs:1012-1024`): no bold header (`TableRow::Cells
   {header}` unused in GUI), no `role-code`/`role-link` in cells, frame at
   full contrast, loud `┝━┿━┥` separator. Terminal has faint frame + bold
   header (`render.rs:175-212`).

5. **Quote bars missing on several inner block types.** No bar on code lines
   or table rows (only margin, `gui.rs:647-663`); none on blank lines or
   rules inside quotes (`Line::Blank`/`Line::Rule` arms); on GTK-wrapped
   paragraph continuations only the first visual line has a bar. The terminal
   bars every single row (`render.rs:143-150`, pinned by
   `render/tests.rs:211-215`).

6. **Cell text truncated even with room**: 60-col cap (`doc.rs:986`) plus
   `truncate_to` ellipsis (`doc.rs:1025`).

7. **Fragile mono assumptions**: `MONO_ADVANCE=0.6` assumes DejaVu; `lead`
   pins DejaVu (`gui.rs:451`) but `mono`/`role-code`/`cb` use generic
   `Monospace` — different systems may resolve to different advances or lose
   box-drawing coverage. Zoom changes only CSS font size; layout is never
   recomputed.

8. **`mono`/`role-code`/`cb` carry `scale 0.92`** while `col_px()` and all
   margins assume the unscaled advance (`gui.rs:434,455,500,469-471`): an
   8 % systematic mismatch between table pixel widths and every column→pixel
   computation.

9. **`insert_text` silently drops the 7th+ tag name** (fixed 6-name varargs
   padding, `gui.rs:550-567`). Styled table pieces inside quotes can reach
   7-8 tags.

## Goal

Grids and diagrams in the GUI look as intended at any window size and zoom:
borders connected, columns aligned, no wrapping through art, styled at least
on par with the terminal. Everything stays selectable, findable buffer text.

Non-goals (keep "no WebKit, one small binary"): Mermaid/SVG, embedded
images, syntax highlighting.

## Verified API facts (this GTK/Pango, introspected)

- `GtkTextTag` **has** `wrap-mode`/`wrap-mode-set` and
  `letter-spacing`/`letter-spacing-set` (gint, Pango units, 1/1024 px); it
  has **no** `attributes` property in GTK4 — fitting must use
  `letter-spacing` on tags, not PangoAttrLists.
- Mixing per-tag `wrap-mode=NONE` paragraphs with a wrapped view is
  self-defeating anyway: a non-wrapping paragraph inflates the view's minimum
  width, so with an automatic h-scrollbar the whole view allocates to that
  width and body prose stops wrapping at the window. Hence **pre-wrap in
  Rust** (below).
- Body text renders in the proportional UI font, so column-count pre-wrap is
  wrong; wrapping must be **pixel-measured** (Pango does it: `set_width` +
  `WORD_CHAR` wrap, break byte-ranges read from `PangoLayoutLine.offset/
  length`).
- Resize observation: GTK4 has no `size-allocate` signal and
  `notify::default-width` does not fire on user resize; use **`notify::width`
  on the view**, debounce with `g_timeout_add`.
- Tag color/letter-spacing properties are live: updating an existing tag
  re-renders without re-inserting text. Adwaita dark state:
  `adw_style_manager_get_default()` + `get_dark()`, live via
  `notify::dark`.
- `gdk_texture_save_to_png` output must be decoded in tests with stdlib only
  (`zlib` + `struct`).

## Phase 0 — shared pure-Rust plumbing (`doc.rs`, `ansi.rs`) + test audit

No behavior change; everything the later phases need, testable without GTK:

1. `pub fn truncate_spans(spans: &[Span], w: usize) -> Vec<Span>` —
   `char_width`-based, preserves style/role/url, appends a single `…` span
   (keeping the cut span's style) when cutting.
2. `pub enum TablePiece { Frame(String), Cell { spans: Vec<Span>, pad_l:
   usize, pad_r: usize, header: bool } }` and
   `pub fn table_pieces(t: &Table) -> Vec<Vec<TablePiece>>` — row-by-row
   fragments sharing `table_text()`'s pad/truncate math: Frame = border text
   or a `│` separator; Cell = spans truncated to the column width,
   alignment split into `pad_l`/`pad_r` **including** the one-space gutters,
   so back ends can paint padding outside code backgrounds. `table_text()`
   stays for the plain terminal path (or is rewritten on top of this).
3. `pub fn spans_slice(spans: &[Span], from: usize, to: usize) -> Vec<Span>`
   — byte-range extraction over concatenated text, styles kept, empty dropped.
4. `pub fn wide_runs(s: &str) -> Vec<(usize, usize)>` (ansi.rs) — byte ranges
   of maximal runs of `char_width==2` chars (CJK/emoji are the fallback-risk
   runs Phase 3 fits).
5. `tests/gui_abi_audit.py`: `require` and walk **Pango** as well, and index
   **constructors** (`get_n_constructors`/`get_constructor`) as well as
   methods, so new pango/adw symbols are arg-count-checked instead of
   tripping the `unknown → exit 1` path.

Unit tests for 1-4 in `doc.rs`/`ansi.rs` (`#[cfg(test)] mod tests`).

## Phase 1 — fidelity fixes inside the current wrapped-view architecture

1. **Quote bars on every block type** (`gui.rs emit`): prepend the
   `┃ `x`depth` lead run (`lead`+`role-bar` tags) to code lines, table rows,
   blank and rule lines inside quotes; on those lines set the tag to
   `left-margin = 0, indent = -cols(depth*2)` so bar+text line up exactly
   like paragraphs (and like the terminal). Phase 2 extends this to
   per-pre-wrapped-line bars automatically.
2. **Styled table emission** via `table_pieces`: Frame pieces get a new
   `role-frame` faint tag; Cell pieces insert `pad` spaces in the plain
   `mono` tag, then per-span tags (`bold` when `header`, `role-code`,
   `role-link`); lighter `├─┼─┤` header separator (bold header carries the
   visual weight). Raise `insert_text` arity to 10 names with a debug assert.
3. **Consistent, measured mono**: drop `scale 0.92` from `mono`,
   `role-code`, `cb` (terminal fidelity; kills defect 8); pin family
   `"DejaVu Sans Mono, Noto Sans Mono, Monospace"` on `mono`/`role-code`/`cb`
   like `lead`; measure the real advance with `gtk_widget_create_pango_layout`
   + `pango_layout_set_font_description`/`from_string`/`set_absolute_size` +
   `pango_layout_get_size` (Pango units, unrounded; explicit
   `dlopen("libpango-1.0.so.0")`) and feed `col_px()` (replaces
   `MONO_ADVANCE`).
4. **Viewport-aware table width**: budget = `(view_width - margins) /
   col_px()`; connect `notify::width` on the view, re-render debounced
   (~150 ms) **only when the column budget changes**, from a cached source
   string (not re-read from disk); seed from `gtk_window_get_default_size()`
   because the first `load_current()` runs before allocation. Zoom actions
   re-render too (budget + margins + measurements change).
5. **Theme-aware colors**: light/dark variants for `role-frame`, `role-faint`,
   link, backgrounds; query `adw_style_manager_get_dark()` at tag creation
   and update existing tags in place on `notify::dark`.

## Phase 2 — no-wrap islands: pre-wrapped prose, unwrapped grids/diagrams

1. **Pixel-aware pre-wrap**: for each paragraph/heading/list item build the
   plain text, wrap it with a `PangoLayout` (`gtk_widget_create_pango_layout`
   → inherits the widget font incl. zoom CSS size; headings set an absolute
   font size = base x heading scale; `pango_layout_set_width(budget px)` +
   `pango_layout_set_wrap(WORD_CHAR)`); take `PangoLayoutLine.offset/length`
   byte ranges (documented struct offsets 16/20; defended by an end-to-end
   test) and emit `spans_slice` per visual line with the existing hanging-
   indent tags. Bars for quotes are emitted per line (defect 5 done right).
2. **View switches to `WRAP_NONE`**, scrolled window horizontal policy
   `POLICY_AUTOMATIC`: code blocks and tables are inserted unwrapped — wide
   art scrolls horizontally intact; with nothing wide, no scrollbar appears.
   Code/table lines keep a no-wrap guarantee trivially (view-level NONE).
3. **State preservation across re-renders** (resize, zoom, reload): cache the
   source string; restore cursor offset and top line (save offset + first
   visible iter before `set_text`, `scroll_to_iter` after).
4. Accepted consequences (documented in README/man): find matches words but
   not phrases spanning a wrap point; copy/paste contains the wrap's hard
   breaks (same semantics as the terminal back end); re-render on resize
   clears the current text selection; wide blocks always show the
   horizontal scrollbar.

## Phase 3 — pixel-exact columns for fallback fonts

Per-**run** letter-spacing, not per-line (per-line spacing can only align the
outer border; interior `│` must land at exact cumulative columns):

- Table **cells**: target `(w+2) x col_px`; code/diagram lines: each
  `wide_runs` segment gets target `2n x col_px`; quote bars and pure-DejaVu
  text need nothing (they are exact by construction after Phase 1.3).
- Mechanism: measure the run with the same `PangoLayout` conditions as it
  renders (same context, font description incl. family), `delta =
  target - measured`; if `|delta| <= 1px` skip; else spacing =
  `delta*PANGO_SCALE/n` (Pango units — quantization error < 0.1 px per
  column, negligible) applied via a **cached `letter-spacing` tag per
  spacing value** (`fit{sp}`, live props, no PangoAttrList — GTK4 tags have
  no `attributes` property). Spacing shifts the text that follows, so
  cumulative piece positions land on the column grid.
- Test tolerance ±1 px (layout rounding); `pango_layout_get_size` (not
  `get_pixel_size`, which rounds).
- Rejected alternative: real `GtkGrid` child anchors — breaks selection/find.

## Test plan

- Unit: Phase 0 functions (wide chars, style preservation, alignment pads,
  truncation, `wide_runs` byte ranges); heading/body wrap offset extraction.
- `tests/gui_abi_audit.py` upgraded in Phase 0 so every new dlopen'd symbol
  is arg-count checked, not waved through.
- `tests/gui_smoke.py`: stdlib-only PNG decode helper (zlib/struct) +
  fixtures: wide 23-col table (assert it no longer touches the right edge
  wrapped: h-scroll present, border row width == table natural width), a
  box-drawing diagram (vertical border column has no gaps between top and
  bottom border rows), a table with one CJK cell (all rows' right-edge x
  equal within ±1 px; valid whether CJK glyphs or tofu render), a diagram
  inside a blockquote (bar pixels at the lead column on every art line),
  light+dark spot checks. Assertions settle after one debounce interval.
- `make check` stays green end to end.

## Order and effort

| # | Step | Files | Risk | Est. |
|---|------|-------|------|------|
| 0 | pure helpers + abi-audit upgrade | doc.rs, ansi.rs, tests/ | low | M |
| 1.1 | bars on all block types | gui.rs | low | S |
| 1.2 | styled tables + arity | doc.rs, gui.rs | low | M |
| 1.3 | mono pinning, scale drop, measured advance, pango dlopen | gui.rs | med | M |
| 1.4 | viewport budget, notify::width, cached re-render | gui.rs | med | M |
| 1.5 | theme-aware colors, notify::dark | gui.rs | low | S |
| 2 | pre-wrap + WRAP_NONE + hscroll + state restore | doc.rs, gui.rs | med-high | L |
| 3 | per-run letter-spacing fit | gui.rs | med | M |
| T | smoke fixtures + numeric assertions | tests/ | med | M |

Phases are strictly sequential (same files); each lands green on
`cargo test` + `make check` before the next starts.
