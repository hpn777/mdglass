# Mermaid diagrams in the terminal (plan v2)

v2 — revised after the Opus 5.5 review of v1 (verdict: approve-with-changes).
Load-bearing corrections: layout cost is NOT a few ms (measured), labels are
multi-line `TextBlock`s, edges are not "near-orthogonal", the flag must be
plumbed through the pager (not defaulted in `Cfg::new`), and piping must keep
source. All adopted below.

Goal: the pager draws fenced `mermaid` blocks as ASCII/Unicode box art, so a
diagram reads in any terminal — no graphics protocol, no new dependency.
**Size claim verified by review: single binary, `mod gui`/`mod vector`
unconditional, mermaid-rs-renderer an unconditional Cargo dependency already
linked under LTO — the ASCII engine adds only its own drawing code.**
Non-goals: faithful typography, sixel/kitty passthrough (deferred).

Principle carried over: terminal output stays text. Box-art keeps node and
edge labels as real characters — find (`/`) and copy work on what the diagram
*says*. The mermaid source stops being searchable by keywords; that is the
trade, covered by a pager toggle key (P2) and `--no-diagrams` — not by a flag
users must know before first run.

## Evidence base

- `mmdr --dumpLayout`: nodes as `{x, y, width, height}` + `label: TextBlock`
  (x,y = top-left), edges as point polylines + `label_anchor` + arrow flags,
  subgraph boxes; layout is deterministic across runs (BTreeMap, verified
  identical md5 over 6 runs).
- **Cost, measured (`mmdr --fastText`, release):** 3-node LR 0.01 s; 6-node
  TD + subgraph 0.12 s; 40 edges 0.27 s; cold system-font scan ≈ +0.11 s and
  a ~760 KB copy under `$XDG_CACHE_HOME/mmdr/font-cache/`. The pager re-renders
  every file on every width change seen by its 15 ms resize poll
  (`pager.rs:367-373`), so layout MUST be memoized (Design → Cache).
- Diagonals are real: a TD fan-out produced a 51×103 px diagonal edge (≈13
  staircase rows), and two edges ran shared segments *along a node's border*
  (`b.mmd` probe). The quality gate (below) is therefore part of the design,
  not an aspiration; P0 must test fan-in/fan-out, a back edge, a self-loop,
  a multi-edge, a subgraph, and an edge into a subgraph — not just the
  3-node example.
- `fast_text_metrics: true` makes ASCII label widths exactly
  `chars × 0.56 × font_size` px (`text.rs:69-71` guard): at `font_size = 16`
  (8.96 px/char) and 8 px/cols, an ASCII label *always* fits its box with no
  compression — and fontdb is never touched for ASCII (no scan, no cache
  write, no host-font dependence in tests). Non-ASCII labels still reach
  fontdb: accepted, documented.

## Approach

**Chosen: layout-native ASCII.** `parse_mermaid_strict` + `compute_layout`
in-process (the same validator-gated parse the GUI runs, so the terminal never
draws input the GUI would reject), then draw the geometry into a char grid.

Rejected: rasterize-then-braille (labels mush, kills find/copy), sixel/kitty
(deferred). Review dropped v1's "rank-based connector" fallback: a second
engine for a view the GUI already draws faithfully — if the quality gate
fails, return `None` and show the source, full stop.

Fallback ladder: gate reject / engine failure / gate fail → today's code
block. Never worse than now.

## Design

### Engine: new `src/boxart.rs` (pure, no GTK); `vector.rs` keeps SVG duties

- `pub fn terminal_ascii(text: &str, max_cols: usize, plain: bool) -> Option<Vec<String>>`
- Input capped by `vector::fits()`; engine runs `catch_unwind` behind a
  once-installed *filtering* panic hook (≈244 unwrap/index sites in upstream
  parser + layout; the default hook's panic text corrupts the pager's
  alternate screen even when caught; a per-call global swap would race other
  threads, so one `Once` hook consults a thread-local quiet flag).
- Gate: `kind == DiagramKind::Flowchart` and `DiagramData::Graph` (direction
  comes from `Graph.direction`, and is *not* a gate — the enum has exactly
  the four values). `DiagramData::Error` and every other kind → None.
- `LayoutConfig { fast_text_metrics: true, .. }` + fixed `Theme` font_size 16.
- **Uniform scale only** (8 px/col, 16 px/row). Too wide → one retry with a
  compact `LayoutConfig` (smaller `node_padding_x`/`node_spacing`/`rank_spacing`)
  — geometry stays consistent because the engine, not the canvas, moves the
  boxes. Still too wide → None. v1's per-column x-compression is dead: it
  would have to remap every polyline and label anchor — a label-eating,
  edge-collision factory.
- Multi-line labels: `TextBlock.lines` drawn as centered rows; measure columns
  with `ansi::char_width` (CJK = exactly 2 cols at these metrics — keep the
  fit check). Never ellipsize (see gate).

### Layout cache (P1 requirement, not optimization)

The layout does not depend on width; only rasterization does. Memoize
`Option<Layout>` (failures included — they are the hot path for garbage) in a
`thread_local!` keyed by the block text. A drag-resize then re-rasterizes at
~0 cost instead of 100–300 ms per diagram per poll.

### Canvas and drawing order (z bottom-up)

1. Subgraph frames (`┌ ─ ┐ │ └ ┘` + label); inner content wins overlaps.
2. Edges: polylines rasterized to cells; diagonal stretches become staircases;
   glyph per cell from its in/out bit mask (`─ │ ┌ ┐ └ ┘ ├ ┤ ┬ ┴ ┼`); arrowheads
   by final segment direction, `► ◄ ▲ ▼` (color) / `> v < ^` (plain) —
   one column under `char_width` (ambiguous-width precedent: fences/tables
   already ship `─ ╭`). Circle/cross endpoints come from
   `start_decoration`/`end_decoration` (`EdgeDecoration::{Circle,Cross}`);
   `arrow_*_kind` is `OpenTriangle`/`ClassDependency` only. Edge endpoints
   land ON node borders — the arrow cell goes one cell outside, the border
   keeps its glyph (write-mask rule, not last-writer-wins).
3. Node boxes: erase interior, draw border, center `TextBlock.lines`. Shapes
   map to conventions, not curves: RoundRect/Stadium → `(`…`)`, Diamond →
   plain box (or `<`…`>` if room), Cylinder → `|‾‾‾|`/`|_|`, else box.
   Skip `NodeLayout.hidden`.
4. Edge labels: blank a window at `label_anchor`; overlap keeps the first
   (deterministic).

### Quality gate (concrete reject list — any hit → None → source)

- any label that does not fit its box at full width (refuse, never ellipsize:
  eaten text breaks find),
- uniform scale below ~6 px/col, or still wider than `max_cols` after the
  one compact re-layout,
- an edge cell landing on any node border/interior except its own endpoint
  cells,
- an edge running along a border (the `b.mmd` pattern), or two edges sharing
  more than a small number of cells,
- an edge label with no free window,
- two visible node boxes overlapping after rounding to cells,
- non-flowchart kind or `DiagramData::Error`.

### Phase-1 implementation notes (locked by review #1; phase 2 builds on this)

- `parse_mermaid_strict` (crate root; preflight validator), flowchart +
  `DiagramData::Graph` gate, pre-layout caps 40 nodes / 60 edges.
- Spacing variants: default; **compact** only when the default is too wide;
  **roomy** (spacing 80) only as the retry a `Fail::Retry` gate asks for —
  roomy never follows a width failure (it is wider than default). Cache:
  `(source -> [tried/got; 3])`, values `Rc<Layout>`, refusals memoized too.
- Geometry is never moved: box rects are the rounded layout rects; ranks
  that fuse at cell scale refuse (the tried settle-nudge pass produced a
  membership lie and was deleted).
- Frames round **inward** (ceil/floor). A node rect touching or spanning a
  frame line refuses. Riding **on** a frame line refuses (`frame_riders`:
  parallel step runs > 2 per line, perpendicular/staircase crossings touch
  ≤ 2 and pass); riding a corridor one cell off a box or frame is faithful
  (SVG shows the same) and is deliberately not gated.
- Arrowheads: last/first route cell outside the endpoint rects, glyph aims
  at the endpoint box (4-neighbour inside it), must share a role if stamped
  on another line; a route through a foreign head refuses.
- Cross-edge contacts: same-role endpoints converge freely; unconnected
  routes may only cross as two straight lines (┼); stacking > 6 cells
  outside all endpoint rects refuses. Cells shared by two routes take edge
  id -2 and no edge label may blank them.
- Every node label, edge label and subgraph title is re-read from the grid
  before emit; anything eaten refuses. Labels with control/zero-width chars
  refuse. `ok`, `x`, `◇`, `◆` decorations draw; crow's-foot refuses.
- Lossy trade, documented: thick/dotted edges all draw solid. Hidden-anchor
  edges refuse (P1.5). `%%{init}%%` theming is not followed (source parity
  is the artifact).

### Terminal integration (`render.rs`, `pager.rs`, `main.rs`)

- Art renders when it will be *read*: interactive pager, or `-n` on a TTY /
  `--color` forced. **Piped without `--color` keeps the source** — plain mode
  reproduces markdown (`fence_top` emits ```` ```mermaid ````); art inside
  that fence would be invalid mermaid and would break
  `mdglass -n doc.md | grep -- '-->'`.
- Flag `--no-diagrams` (matches `--no-urls`/`--no-pager`, generalizes to
  sequence/pie later). Explicitly plumbed: `main.rs` → `Pager::new`/`pager::run`
  AND `print_plain` — pager.rs:113 builds its own `Cfg::new`, so a default-
  true field in `Cfg::new` would silently ignore the flag and change every
  fixture in `render/tests.rs` unnoticed.
- Hook: the `Line::Code` arm of `render_lines` (quotes recurse at width−2 →
  art narrows automatically; GUI never calls `render_lines`). Caption: reuse
  the existing `fence_top(cfg, "mermaid")`/`fence_bottom` chrome — no third
  frame style; it also keeps one searchable `mermaid` keyword on screen.
- Pager toggle key (P2): flip diagrams on/off for the session and re-render
  (art ⇄ source); test it in the pty smoke.
- Resize: nothing to add — `Pager::resize` re-renders; the cache makes it cheap.

### Phase-2 implementation notes (built as specced; deltas only)

- `Cfg { diagrams }` defaults **false**; `Cfg::new` keeps 3 args. `main.rs`
  gates `diagrams && (color || stdout_tty)` then plumbs the value explicitly
  to `pager::run` and `print_plain` — never re-derived downstream.
- `render.rs` art rows bypass `code_lines`/`wrap` entirely (a sheared box is
  a lie): color rows get the same `38;5;252;48;5;236` shading + padding as
  code rows, plain rows pass through untouched (boxart guarantees width and
  prints no control chars, so art rows carry no ESC anywhere).
- `Pager` gained the field plus an extracted `rerender()` (resize and `D`
  both call it); it rebuilds `plain` so search matches art rows, and clamps
  `top` because art is shorter than source.
- Engine cost measured (release, fast_text_metrics): 3-node 0.01 s, ~15
  nodes ~0.3 s; at the 40-node/60-edge caps one variant run costs 1.7–4.7 s
  and a fence consults at most two variants (default + compact or roomy).
  The engine cannot be aborted mid-run, so a per-call deadline buys
  nothing — not even on idle ticks (review #2 second blocker: a warm tick
  once stalled quit for up to ~9 s). The engine must run on another thread.
- Pager render path is probe-only: cache answers (`Art`/`Source`); fences
  the cache has not decided draw as source and queue as `(src, cols)` —
  `cols` is part of the key (quotes narrow by two, mirrored from
  `render.rs`) so every dispatch answers what its probe asked. A single
  session worker thread (64 MB stack: the recursive-descent parser needs
  the headroom a 2 MB worker default lacks) runs `boxart::engine` per
  `(src, variant)` job and posts the plain-data `Layout` back through mpsc
  (compile-time `Send` assert); the UI thread inserts `Rc`s into the
  thread-local cache, re-probes, and dispatches the next variant;
  `next_warm` walks the exact `decide` ladder so dispatch never re-derives
  it. Exactly one job in flight (priority is re-decided after every
  arrival, harvest+dispatch share a tick), queued current-file first, then
  nearest the viewport. Quit never joins: process exit reaps the worker.
  `CACHE_MAX` 512 is a soft cap with generation eviction: a rerender
  stamps what it draws, overflow drops only un-stamped entries, and live
  entries are never evicted (dropping one re-arms it — the exact cycle the
  generation prevents), so memory grows with what the session opened. If
  the thread cannot spawn, art simply never warms and fences honestly
  stay source.
  `print_plain` stays synchronous (batch output may wait);
  `--no-diagrams` is the opt-out.
- `rerender()` preserves the reading place (nearest-copy anchor of the
  top visible row), resets `last_match` (an index into dead rows), and
  rebuilds the warm queue; plain-mode art fences as ```` ```text mermaid ````
  so copied art is never invalid mermaid. `--color` pipes are the one
  case where piped output is art, and the docs now say so.
- Upstream quirk inherited from mermaid-rs (identical in the shipped GUI,
  so terminal and window agree): chained edges with inline node definitions
  on one line (`A -->|l| B[x] --> C[y]`) mis-tokenize; mermaid.js would
  parse them differently. Fixing it belongs upstream or in P1.5.

### Phase-3 implementation notes (built as specced; deltas and honest limits)

- One `raster()` dispatcher routes every admitted kind: Flowchart/State/Er
  share `graph_raster` (kind gates inside), Sequence gets `sequence_raster`,
  Pie becomes `pie_list` — title plus `label  [value]  pct%` rows, read
  from each slice itself; the value prints as the SOURCE wrote it (the
  first version reprinted the parsed f32 with `{:.2}`, so `123456789`
  arrived as `123456792.00` and `0.001` as `0.00` — review #3b, 5), the
  share is computed alongside, and the list matches `showData` output for
  every pie. Rows follow slice order (sorted by value, like the chart),
  not source order.
- State: `[*]` start/end pseudo-states draw as one dot (● start, ◉ end;
  plain `o`/`@`) — a 16 px circle has no honest box reading at cell scale.
  Their footprint is the single cell, which slides a small spiral outward
  until it is off every box, off every frame line, and a cell clear of any
  box so the arrow into it has a head; if the dot's px centre sits inside
  a composite the candidates stay inside that composite — sliding out
  would move the membership the frame states (review #3 B7). The engine
  polyline endpoints are re-aimed at the cell actually drawn. Composite anchors (`Running -->
  Working`) were hidden nodes *not* listed in their own subgraph's members;
  they now enter `anchors` via `anchor_subgraph`, so such edges terminate
  at the frame like the SVG.
- Sequence: lanes map to columns, events claim rows top-down
  Every drawn line claims through the px-aware `claim_row`: the row must
  be free (no earlier line, no box row) *and* keep the same order against
  every already-claimed line (a slide that crossed another event would
  retell the sequence out of turn, review #3 B1); no row can un-fuse two
  events that share a px height, so those refuse. A final gate re-reads
  every drawn line against its px height and refuses unless rows kept the
  source order — the belt to the claim's braces. Boxes and notes claim
  whole rectangles (three head boxes may share a band when their columns
  don't touch, but never a line row, another box, or a drawn cell). Wall
  and divider cells are sacred: boxes, notes and floating labels may not
  break a section open like a door the source never drew (B3/B8), though a
  message row crossing a wall is a real crossing and stays. Frame labels
  snap to the *claimed* wall/divider row, may punch their own wall between
  the corners but never run over one — a wide section name needs a wide
  frame (B2); keyword and section share one pentagon ("opt [debug]").
  Activation bars draw one column right of the lane, but they come from
  the SOURCE and the ledger's claimed rows, not from the crate's
  activation boxes: those take the NEXT message's index (so every bar
  covered the wrong messages) and apply a `->>-` arrow's deactivation to
  the receiver where mermaid retires the SENDER (review #3b, 6/7). A
  `->>+` arrow wakes the receiver under its own arrow row; a never-closed
  bar runs to the foot box; nested activations step one further right
  (depth > 2 refuses); deactivating what nothing activated refuses. Each
  explicit activate/deactivate anchors on the LAST VISIBLE statement its
  line followed — message, note, frame top, divider or floor, whatever
  the source wrote in between (review #3c, 4): a start begins one row
  below that statement's drawn row and an end closes on it, so a bar
  neither ignores a note it should span nor starts beside a note written
  before it. Activations name participant IDS only — display aliases map
  bars to the wrong lane and a bare display name is a TypeError in
  mermaid, so a name that never speaks refuses (review #3c, 3). Lane
  order belongs to the SOURCE — declarations, then first mention in
  messages and notes alike; an `activate` names a participant but
  creates no lane, yet the crate registers a lifeline at its line and
  would slide that participant ahead of the first speaker, so the
  layout's lanes must line up with the source's or the diagram refuses
  (#3d, 2). A column
  that would coincide with a frame's wall over the bar's rows refuses
  rather than vanish into the wall or open a false junction (#3c, 6); a
  bar no statement spans takes one free row strictly BEFORE the next
  statement — walls, floors and dividers close that gap too, so a stub
  opened before an `else` never drops below its divider (#3c, 5; #3d,
  3) — or refuses. A bar that survives no row as an actual `║` stroke
  (every cell hidden behind a box, or reduced to wall junctions)
  refuses too: the reader must SEE the activation mermaid shows (#3d,
  1). The bar hides behind arrows, boxes and words (they out-rank it)
  and joins a crossed wall as a junction, because both lines truly
  continue. A message label may
  only sit ABOVE its arrow: below, an unlabelled neighbour makes the
  caption swap silent and the fuzz proved readers take the bait (review
  #3b, 2), so a label with no honest row above refuses the diagram. A
  self-message's stub may not contain a wall or divider cell — a loop
  starting under one section line and returning under the next lived in
  two branches at once (review #3b, 4). Alt/else/opt/loop
  frames draw; two frames that overlap where neither strictly contains
  the other (the crate stacks consecutive blocks' walls into one row)
  refuse whole (B2). Containment reads the y band strictly but the x
  band loosely: frames spanning the same lanes NEST at the same x, and
  mermaid draws an opt inside an alt across the full width — the inner
  frame opens on the outer wall with a T-junction (`├──opt──┤`), never
  a corner that cuts a door in the outer stroke, while sharing a wall
  ROW stays the merge artifact B2 refuses (#3d note). The renderer deletes every frame that holds no
  message (empty or notes-only), taking its keyword, condition and
  else-branches with it, so the written frame and divider counts must
  match the layout's or the diagram refuses (review #3b, 3). Counts are
  not enough though: the crate also counts a note written just before a
  frame as part of it and lifts the top wall over the messages before —
  a membership audit requires each frame's px band to hold EXACTLY the
  messages, notes and dividers the source wrote inside its lines, pairing
  source frames to layout frames by that membership (taller band = outer
  frame) (#3c, 1). Frames that never close and stray `end`s refuse: the
  crate auto-closes, mermaid errors, and a drawn closure the source never
  wrote is invented (#3c note). The scanner also refuses any line holding
  a `;` (mermaid splits statements there, the crate merges them into one
  label and drops an event, #3c, 2) or a mid-line `%%` (a comment there
  truncates the label upstream, #3c, 8) — and that scan guards EVERY
  kind, not just sequence: `A[hi %% x]` truncates a flowchart node and
  `A -->|a;b| B` splits a label into two nodes exactly the same way
  (#3d, 4). A title is harder to catch than a first word: frontmatter
  can spell it `"title":`, `{title:`, `? title` or even `"ti\x74le":`
  (escapes decode before YAML matches the key), so a CLOSED `---` fence
  is word-scanned and any graph line whose decoded head word is
  `title`/`accTitle`/`accDescr` refuses (#3d, 6; #3e, 2). Flowchart
  statements are additionally TOKENIZED under mermaid's full link
  grammar (#3e, 1/4): open `---`, dotted `-.-`, thick `===`,
  bidirectional `<-->`, edge ids (`e1@-->`), quoted-pipe edge labels,
  node shapes, `&` fans and the text forms `-- t -->`, `-. t .->`,
  `== t ==>`; a `((label))` circle (it rasterizes to a bare dot, so
  the label's words would vanish) and `x`/`o` arrow ends (the flat art
  would flatten the marker away) refuse on sight. A line the tokenizer
  cannot place — `A --> B C --> D`, `A[a] B[b]`, `A x--> "b c"`, the
  chain `A --- B --- C` the parser merges into one labeled edge —
  refuses without consulting the parser at all, and every id it does
  accept must exist in the layout — moreover the parsed links, with
  their true endpoints (`&` fans expanded crosswise), must match the
  layout's edge multiset EXACTLY: a self-loop counted but never drawn
  and a tail relabelled as one longer edge both fail (#3d, 5; #3e, 1
  + fuzz6). The check runs BOTH ways: every box the art would draw
  must be an id the source wrote, since `click A call cb(arg)` makes
  the crate invent a node `arg` (#3f, 3); node metadata `@{ label: ..
  }` refuses because its label or shape never reaches the art (#3f,
  2); a finished text link may not take a `|label|` as well (#3f, 4);
  an edge into a subgraph nested inside another routes to strokes no
  element owns and refuses (#3f, 6), and naming a subgraph's id inside
  another subgraph — mermaid's other way to nest one — refuses too,
  since the crate draws it beside its parent (#3g, 1); a `style` line
  on an id no statement wrote, or on a comma list, refuses because
  mermaid draws a box for it (`class`/`click` create none, #3g, 2); and two edges between the same
  pair of boxes must each keep cells of their own outside the boxes,
  or the raster has drawn two edges as one arrow (#3f, 1) — nor may
  they share one arrowhead. Every visible box must also SAY what the
  source wrote for it: the text of the last shape written for its id,
  or the bare id when none was (compared ignoring spacing, quotes,
  `<br>` and shape brackets) — the parser swallows line tails into
  labels (`Al[[Al]] ~~~ Al[Al]` drew one box reading `[Al]] ~~~
  Al[Al`, #3f fuzz). Invisible
  `~~~` links demand their nodes but no edge — mermaid draws no line
  for them either. Escapes include 8-digit `\U`, and a frontmatter
  fence holding any backslash is read as one run of letters, since a
  `\`-escaped break folds a quoted key across lines (#3f, 5). State
  keeps the canonical arrow audit: its grammar has no open or text
  link forms.
  Only `Note over` draws — and only where a free band exists;
  autonumber, participant boxes, par/critical/break frames, leftOf/rightOf
  notes, quoted note actors (mermaid gives `Note over "C"` a lane of its
  own beside a real C, the crate merges them, #3e, 3), source-end
  arrowheads and stick figures refuse whole.
- `emit` now glyph-converts only cells with a routing mask: the sequence
  raster draws finished glyphs (mask 0) and was silently flattening `┐ ┘ │
  ╌` to `─` — flowchart art is untouched (masked cells only, dumps verified
  byte-identical before and after).
- ER: attribute separator `---` inside a box label draws as a full-width
  rule (flowchart and state nodes never take it — the rule flag is gated
  on the ER kind); relations also print as a `Relationships:` list under
  the diagram using the exact crow's-foot notation (`||--o{` etc.),
  mirrored per end — the start side reads `}o`/`}|`, the reverse-and-swap
  of the end side (review #3 B6) — because the notation cannot fit one
  cell per glyph. The list hangs under the picture, so it is width-gated
  against the terminal, not the picture's own bounding box (the first
  gate compared every relation to the art width and refused even a single
  short relation at any window size, review #3b minor). Self-relations, empty relation lists (would draw boxes
  the source never related) and entity names carrying the list's own
  separators refuse.
- Edge label placement learned to float: rows ±2 and columns ±4 from the
  anchor (SVG hangs labels loosely), preferring one column of air from
  neighbouring labels — falling back to touching rather than refusing.
  The window interior admits bare air and the label's own route only:
  walls (the first B8a guard waved FRAME straight through its own
  condition — a no-op, review #3b, 1), boxes, foreign routes and foreign
  words all block. The touching fallback may not fuse two ASCII words
  ("pausedispatch" is not two labels; CJK contact stays legal, B8b).
  When every anchor window is spoken for, the label floats beside the
  drawn route instead. Both passes share one attribution gate (review
  #3b, 8): the window must sit STRICTLY nearer its own route than to any
  foreign route cell, box or entry dot — a tie reads as belonging to
  whichever mark the eye meets first — and some own route cell must
  reach it without crossing a frame wall, so a label can no longer float
  across a boundary and caption the wrong arrow (or an unlabelled
  `[*]`-dot beside it). Flowchart art is byte-identical under all of
  this (dumps re-verified; labels fused into walls in the reviewer's
  repros now move or refuse, and the shipped demos keep every wall).
- Costs, measured on fixtures that actually DRAW (the earlier scratch set
  all refused and only ever measured the refusal path; rerun with
  `cargo test -- --ignored measure_kind_caps`, which grows each kind until
  it stops drawing): state LR-tree 37 nodes in 40 ms, flow chain 13 nodes
  at 120 cols in 189 ms / 23 at 220 cols in 431 ms, ER star 9 entities at
  120 cols in 148 ms, sequence 41 messages in 14 ms, pie 25 slices in
  0.4 ms. The largest drawable sizes sit below the 40n/60e caps because
  long chains lose stubs or window width first; the one-over-cap flow
  refusal costs 5 ms. The flowchart budget (1.7–4.7 s per variant at the
  caps, worker-thread only) stays the binding limit; no per-kind caps
  added.
- Sequence and pie carry a source-preflight gate because the parser
  itself lies: the sequence parser hands EVERY accepted arrow token a
  filled head (`A->B` would draw `►`), swaps the ends of `<-` tokens
  (`A<->B` invents a participant named ">B") and silently drops whole
  messages (`A->>B->>C`). `seq_source_messages` reruns the parser's exact
  token scan on the source and refuses unless the drawn edges are exactly
  the written filled one-way messages, in order and count; statement
  types that draw nothing here (title, link, rect, autonumber) refuse
  rather than vanish. Pie has the same shape of problem: slices sort by
  value while the legend keeps source order (zipping pairs wrong names
  with wrong numbers — each row reads its own slice's label/value), and
  the renderer DROPS slices below ~1% share — so every written slice must
  appear in the layout, by label and value, or the pie refuses (
  repeated labels refuse too — mermaid keeps the first value and merges
  the slice, so a list printing every row would invent a slice (#3c, 7);
  and `pie`/`showData`/`title` leading lines are chart furniture, not
  slices, so `pie title Sales: Q1` draws (#3c note))a
  1-in-100000 pie draws no art, review #3 B5).
- Honest known limits: `sequenceDiagram` layouts where the crate itself
  overlaps elements (consecutive frames sharing a wall row, self-loop
  stubs fused into notes or foot boxes — `seq_dense` fixture) refuse
  exactly where no claim survives; 26+-slice pies refuse (their thin
  slices drop below the renderer's floor and the parity gate catches it);
  33-state graphs with back-jumps refuse at `two routes merge` like
  equally dense flowcharts do. All fall back to the code block, GUI
  unaffected.
- Upstream flake found while testing: bare `A<->B: z` without participant
  declarations parses into a phantom participant in *some* processes
  (crate tokenizer + HashMap state). The source preflight makes this moot
  for the art — `<-`-family tokens and odd tokenisations refuse before
  the layout is trusted, whatever the parser made of them.

## Phases and gates

- **P0 feasibility**: prototype raster with the full test set: 3-node LR,
  TD fan-out/fan-in, back edge, self-loop, multi-edge, subgraph frame, edge
  into a subgraph, `b.mmd` border-runner. Gate = readable, gate rejects
  exactly the dirty cases. Kill criterion: if clean flowcharts also fall to
  the gate more than rarely, stop and keep SVG the only diagram path.
- **P1 engine**: `boxart.rs` + cache + strict parse + panic hook + gate;
  unit tests assert *invariants* (width ≤ max_cols, every non-wrapped label
  substring present, determinism, gate rejects the dirty set, plain mode has
  no box glyphs, `fast_text_metrics` leaves no font-cache dir), never exact
  art. Pin the crate version; no golden snapshots.
- **P2 integration**: `--no-diagrams` plumbed everywhere, pager toggle key,
  pty smoke: box glyphs for the diagram doc, find hits a node label, broken
  mermaid stays code, piped output stays source, toggle key shows source.
  Update the expectations in `examples/diagrams.md`, README and metainfo that
  today say "terminal output stays text [as code]" — still text, now drawn.
  changelog +0 size re-verified.
- **P3 (decide after P2)**: sequence via `DiagramData::Sequence` (its own
  drawing path — separate risk bucket); state/ER if they present as
  nodes+edges; dim-border color pass; pie as an aligned percentage list.

## Questions — resolved by review

1. Default-on in the pager/TTY, source when piped; plus a session toggle key
   (better answer to the find/copy worry than a CLI flag).
2. No rank-based fallback; gate fail → source.
3. Sequence stays P3.
4. Reuse fence chrome, not a new frame.
5. `--no-diagrams`.
6. Refuse instead of degrade: the concrete gate list above.
