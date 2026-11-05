# Desktop rendering quality: grids (tables) and diagrams (ASCII art)

Status: proposal. Scope: `--gui` back end (`src/gui.rs`), with small shared
changes in `src/doc.rs`. The terminal back end is the fidelity reference.

## Current state

The GUI renders the whole document into one `GtkTextView` (word wrap on,
horizontal scrollbar off). Tables are laid out by `doc::table_text()` into
box-drawn **plain text** rows inserted with the `mono` tag; code blocks
(including diagrams) are inserted as plain monospace lines. Everything that
does not fit the window is soft-wrapped by GTK.

## Measured defects (1024x768 window, Ubuntu, DejaVu Sans Mono as Monospace)

Evidence gathered from a real Broadway screenshot of `tests`-style documents
plus the code.

1. **Wide tables wrap and break.** A 23-column table (~1155 px natural) has
   its border lines cut at the viewport edge (x=991) and wrapped mid-row:
   `╭───...╮` continues on the next visual line and every `│` loses its
   column. Causes: fixed layout width `Cfg::new(false, 120, true)`
   (`gui.rs:687`) is unrelated to the real viewport, horizontal policy is
   `POLICY_NEVER` (`gui.rs:976`) and wrap is `WRAP_WORD_CHAR` (`gui.rs:979`).
   Zooming in has the same effect on tables that fit at 100 %.

2. **CJK (and any non-covered script) drifts columns.** With one CJK cell,
   the table's right border moves from x=788 to x=768 (4 columns visible
   drift). `fc-match "monospace:lang=zh"` resolves to DejaVu Sans Mono, which
   has no CJK glyphs, so Pango falls back to a proportional font whose advance
   is not 2x the mono advance. `doc`'s `char_width` (2 columns) and the actual
   pixel advance disagree; space padding cannot re-align the border.

3. **Long lines in code/diagrams wrap and destroy the drawing.** Diagrams are
   inserted like any code (`gui.rs:647-653`); a line wider than the viewport
   wraps inside the box-drawing art. No way to see it intact.

4. **GUI drops all table styling that the model carries.** `table_text()`
   flattens cells with `spans_plain` (`doc.rs:1012-1024`): no bold header
   (the `TableRow::Cells { header }` flag is unused in the GUI), no inline
   code background, no link color; the frame is full-contrast foreground and
   the header separator `┝━┿━┥` is visually loud. The terminal back end has
   all of this (`table_ansi`, `render.rs:175-212`: faint frame, bold header).

5. **Quote bars are missing next to code blocks and tables inside quotes.**
   `emit()` inserts the `┃ ` lead only through `emit_spans()` (paragraphs,
   headings); the `Line::Code` and `Line::Table` arms only add a left margin
   (`gui.rs:647-663`), so a diagram inside a quote floats with an empty
   gutter. The terminal prefixes every inner row with the bar (`render.rs:143`).

6. **Cell text is truncated even when there is room.** Column width caps at
   60 columns (`doc.rs:986`) and cells are cut with `…` (`doc.rs:1025`) —
   information loss that is unnecessary on a wide window.

7. **Font assumptions are fragile.** `MONO_ADVANCE = 0.6` assumes DejaVu; the
   `lead` tag pins "DejaVu Sans Mono" (`gui.rs:451`) but `mono`, `role-code`
   and code-block tags use generic `Monospace` (`gui.rs:430-455`), which on
   other systems (Flatpak, Fedora) may resolve elsewhere and lose box-drawing
   coverage or a different advance. Zoom only changes CSS font size; the text
   layout (table budgets, wrap) is never recomputed.

## Goal

Grids and diagrams in the GUI look exactly as intended at any window size and
zoom level: borders connected, columns aligned, no wrapping through art,
styling (bold headers, inline code, links, faint frame) at least on par with
the terminal. Everything stays selectable and findable buffer text.

Non-goals (keep the "no WebKit, one small binary" promise): Mermaid/SVG
rendering, embedded raster images, syntax highlighting.

## Plan

### Phase 1 — fidelity fixes inside the current architecture

Small, low-risk, each independently shippable.

1. **Quote bars for all blocks.** In `emit()`, prepend the `┃ ` lead run
   (`lead` + `role-bar` tags, as `emit_spans` does) to every code line and
   every table row when `depth > 0`. (`gui.rs`)

2. **Styled table emission.** Add `doc::table_cells(t) -> iterator yielding
   per-row pieces` (frame pieces vs cell pieces with their spans and the
   `header` flag); have the GUI insert each piece with its own tags:
   - bold header row (reuse `h` weight via the `bold` tag),
   - new `role-frame` tag: faint frame color, ~55 % of foreground,
   - inline spans inside cells keep `role-code` / `role-link`,
   - lighter header separator `├─┼─┤` (keep `┝` variant only if the header is
     not bold).
   Keep `table_text()` as the plain fallback for `--plain`/no-color paths.

3. **Consistent mono font.** Pin `"DejaVu Sans Mono, Noto Sans Mono, Monospace"`
   in the `mono`, `role-code` and code-block margin tags (same as `lead`).
   Replace `MONO_ADVANCE` with a measured advance: render a reference string
   with a PangoLayout from the view (`pango_layout_new`,
   `pango_layout_set_text`, `pango_layout_get_pixel_size` — three more dlopen
   syms) once per zoom level and use it in `col_px()`.

4. **Viewport-aware table width.** Derive the layout budget from the real
   view: `cols = (view_width - margins) / col_px()` instead of the fixed 120,
   re-`load_current()` on window size change (connect `notify::default-width`
   or a size-allocate tick, debounced ~150 ms) and after zoom actions. Tables
   that still exceed even the minimum width (many narrow columns) keep today's
   wrap until Phase 2; a table that fits never wraps again.

5. **Theme-aware tag colors.** Query `adw_style_manager_get_dark()` (one more
   sym) and pick light/dark variants for `role-frame`, `role-faint`, link and
   backgrounds instead of hardcoded hex/rgba, so grids look right in dark mode.

### Phase 2 — no-wrap islands: diagrams and wide tables done right

GTK cannot mix wrapped paragraphs and a horizontal scrollbar sanely (one long
non-wrapping line raises the view's minimum width and un-wraps everything).
The robust design is what the terminal already does: **wrap in Rust, let GTK
scroll.**

1. Implement span-aware wrapping in `doc.rs`/`ansi.rs`: `wrap_spans(spans,
   width, indent) -> Vec<Vec<Span>>`, word-boundary, `char_width`-based — the
   pure-Rust twin of `ansi::wrap_indent`.
2. GUI: set the view to `WRAP_NONE`, pre-wrap every paragraph/heading/list
   item to the viewport column budget (from Phase 1.4), insert hard breaks.
   Set the scrolled window's horizontal policy to `AUTOMATIC`.
   Code blocks and tables are emitted unwrapped: a diagram or wide grid wider
   than the window now scrolls horizontally intact; when nothing is wide, no
   scrollbar appears.
3. Re-render (debounced) on resize and zoom so paragraph wrap width follows
   the viewport. Documents are small; a full re-render is sub-millisecond for
   anything realistic.
4. Consequence to accept and document: copy/paste and find see the wrap as
   hard breaks — same semantics as `mdglass -n` output and the pager.

### Phase 3 — pixel-exact columns for mixed scripts

Fixes defect 2 for good (CJK, emoji, future fallbacks):

- Option A (recommended, keeps text semantics): fit each emitted line to
  exactly `n x col_px` pixels — measure the line with a PangoLayout and
  distribute the delta via `PANGO_ATTR_LETTER_SPACING` on that line's
  attributes. Borders then align regardless of fallback fonts. Needs a small
  `pango_layout_*` + `pango_attr_list_*` FFI addition and a per-line tag.
- Option B (rejected, documented for the record): embed real `GtkGrid`
  widgets at child anchors. Pixel-perfect but loses text selection, find
  hits, and zoom plumbing — against the grain of a text viewer.

## Test plan

- Unit: `wrap_spans` widths (including wide chars, hard breaks);
  `table_cells` piece decomposition; budget calc from fake view widths.
- `tests/gui_smoke.py` fixture additions: a 23-column table, a box-drawing
  diagram, a table with a CJK cell, a diagram inside a blockquote.
- New numeric, image-free assertions on the existing screenshot hook (no
  vision model needed): for the fitting table, every text row's right border
  x is identical; the diagram's left border column has no gaps between its
  top and bottom border rows; quote bar pixels present at the expected column
  beside code lines inside quotes.
- `make check` stays green; `gui_abi_audit.py` covers the new dlopen'd
  symbols automatically.

## Suggested order and effort

| Step | Files | Risk | Est. |
|------|-------|------|------|
| 1. Quote bars for code/tables | gui.rs | low | S |
| 2. Styled table emission | doc.rs, gui.rs | low | M |
| 3. Mono pinning + measured advance | gui.rs | low | S |
| 4. Viewport-aware width + re-render | gui.rs | med | M |
| 5. Theme-aware colors | gui.rs | low | S |
| 6. Pre-wrap + no-wrap view (Phase 2) | doc.rs, ansi.rs, gui.rs | med-high | L |
| 7. Letter-spacing fit (Phase 3) | gui.rs | med | M |
| Tests throughout | tests/ | — | M |

Steps 1-5 make normal documents look clearly better with no architectural
risk; step 6 is the one that definitively kills diagram/grid corruption;
step 7 is only needed if tables with CJK/emoji matter.
