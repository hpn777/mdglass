//! Terminal box-art rendering of mermaid flowcharts (docs/terminal-mermaid-plan.md).
//!
//! The layout engine behind the SVG path is reused: `parse_mermaid_strict` +
//! `compute_layout` give real dagre geometry (ranks, lanes, subgraph frames),
//! and this module splats that geometry into a character grid. Output stays
//! text: find still finds node names, selection still copies them.
//!
//! Failure is always `None` (keep the plain code block), never a panic and
//! never mangled art: gates reject any layout whose edges ride box borders or
//! frame lines, whose routes stack or kink into each other, whose arrowheads
//! would be dropped, or whose labels would not survive the finished grid.
//! Gates that spacing might fix trigger exactly one roomier re-layout;
//! everything else refuses outright. Geometry boxes are never moved: where
//! cell rounding fuses what px kept apart, the honest answer is source.
//!
//! The engine runs under `catch_unwind` behind a once-installed filtering
//! panic hook: a panic message printed into the alternate screen would
//! corrupt the display even when caught. Known lossy trade: edge styles
//! (thick/dotted) all draw solid; direction and connectivity stay truthful.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::panic;
use std::rc::Rc;
use std::sync::Once;

use mermaid_rs_renderer::layout::{compute_layout, DiagramData, Layout};
use mermaid_rs_renderer::{parse_mermaid_strict, DiagramKind, EdgeDecoration, LayoutConfig, NodeShape, Theme};

use crate::ansi::{char_width, visible_width};

/// px per character cell; ~1:2 cell aspect. With `fast_text_metrics` a label
/// of n ASCII chars measures >= 0.56*font_size*n px, and at font_size 16 a
/// char is 8.96 px >= SX, so a label that fits in px always fits in cells.
const SX: f32 = 8.0;
const SY: f32 = 16.0;

/// Below this even the compact layout stops being readable; smaller
/// terminals keep the source.
const MIN_COLS: usize = 48;
/// A 600-row diagram is a scroll wall, not a picture.
const MAX_ROWS: usize = 600;
/// Beyond these the engine measurement hit ~0.8 s per variant and the art
/// stops being legible anyway; refuse before paying for layout.
const MAX_NODES: usize = 40;
const MAX_EDGES: usize = 60;
/// Two different edges may share a few cells (parallel routes visually merge,
/// which is honest); beyond that the drawing is spaghetti and the reader
/// deserves the source.
const MAX_SHARED_CELLS: usize = 6;
/// Layouts (and refusals) memoized per source text; see `cached_layout`.
const CACHE_MAX: usize = 512;

/// layout configs tried: default, then at most one retry per axis
const V_DEFAULT: usize = 0;
const V_COMPACT: usize = 1; // too wide for the window
const V_ROOMY: usize = 2; // geometry too tight to draw honestly at cell scale
const V_COUNT: usize = 3;

/// the pager hands layouts to a worker thread and back; if upstream ever
/// puts interior mutability in these types that must break here, not there
const _: fn() = || {
    fn require_send<T: Send>() {}
    require_send::<Layout>();
};

/// one engine call per (source, variant) slot; a render consults at most
/// two slots (default, plus compact or roomy; a fit check is free, the
/// engine is not)
struct Entry {
    tried: [bool; V_COUNT],
    got: [Option<Rc<Layout>>; V_COUNT],
    /// generation of the last use; overflow evicts stale entries only, so
    /// a full cache can never cycle: dropping an entry the current view
    /// still draws would re-arm its fence and evict the replacement in turn
    gen: u64,
}

impl Default for Entry {
    fn default() -> Self {
        Entry {
            tried: [false; V_COUNT],
            got: [None, None, None],
            gen: 0,
        }
    }
}

thread_local! {
    /// source -> layouts. Layout is width-independent, so resize
    /// re-rasterizes without re-running the engine (measured up to ~0.8 s for
    /// a 40-edge graph), and the 15 ms resize poll would otherwise re-pay it.
    /// Refusals are memoized in the slot too, so unparseable blocks cost
    /// nothing per frame.
    static LAYOUTS: RefCell<HashMap<String, Entry>> = RefCell::new(HashMap::new());
    /// set while the engine runs: the global panic hook keeps quiet
    static QUIET: Cell<bool> = const { Cell::new(false) };
    /// bumped by the pager on every rerender; see Entry.gen
    static GEN: Cell<u64> = const { Cell::new(1) };
}

/// ASCII/Unicode box art for a mermaid flowchart, or `None` to keep the code
/// block. `plain` forces pure ASCII (+---, >) for `--color`-less output.
pub fn terminal_ascii(text: &str, max_cols: usize, plain: bool) -> Option<Vec<String>> {
    match decide(text, max_cols, plain, &mut |v| Some(cached_layout(text, v))) {
        Outcome::Art(art) => Some(art),
        _ => None,
    }
}

/// What `terminal_ascii` would return, without ever running the engine:
/// the pager draws from `Art`/`Source` immediately and re-arms `Unknown`
/// fences into an idle-time warm queue (the engine cannot be aborted
/// mid-run, so it must never start inside a frame the user is waiting on).
#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    Art(Vec<String>),
    Source,
    Unknown,
}

pub(crate) fn probe(text: &str, max_cols: usize, plain: bool) -> Outcome {
    decide(text, max_cols, plain, &mut |v| peek_layout(text, v))
}

/// the variant a `terminal_ascii` run would have to compute next for this
/// fence (None when the cache already decides it): the probe ladder records
/// the first variant its getter could not answer, so dispatch follows the
/// exact same path the real run takes - never a re-derived copy of it
pub(crate) fn next_warm(text: &str, max_cols: usize, plain: bool) -> Option<usize> {
    let mut need: Option<usize> = None;
    let outcome = decide(text, max_cols, plain, &mut |v| {
        let hit = peek_layout(text, v);
        if hit.is_none() {
            need.get_or_insert(v);
        }
        hit
    });
    match outcome {
        Outcome::Unknown => {
            debug_assert!(need.is_some(), "Unknown must name a variant");
            need
        }
        _ => None,
    }
}

/// The spacing ladder shared by the full path and the probe, so the probe's
/// answer can never drift from what a real run would decide. `layout_of`
/// answers per variant: Some(layout) decided (None = memoized refusal), or
/// None = never tried, which only the full path may resolve.
fn decide(
    text: &str,
    max_cols: usize,
    plain: bool,
    layout_of: &mut dyn FnMut(usize) -> Option<Option<Rc<Layout>>>,
) -> Outcome {
    if !crate::vector::fits(text) || max_cols < MIN_COLS {
        return Outcome::Source;
    }
    // uniform scale only (never squish either axis alone: that cuts through
    // labels); the spacing config is the only knob, turned at most once
    let mut pick = |variant: usize| -> Pick {
        match layout_of(variant) {
            None => Pick::Unknown,
            Some(None) => Pick::Source,
            Some(Some(l)) if fits_window(&l, max_cols) => Pick::Draw(l),
            Some(Some(_)) => Pick::Source, // decided; just too wide for this window
        }
    };
    // roomy is strictly wider than default, so it never follows a width
    // failure — only a raster retry
    let (layout, tried_default) = match pick(V_DEFAULT) {
        Pick::Draw(l) => (l, true),
        Pick::Source => match pick(V_COMPACT) {
            Pick::Draw(l) => (l, false),
            Pick::Source => return Outcome::Source,
            Pick::Unknown => return Outcome::Unknown,
        },
        Pick::Unknown => return Outcome::Unknown,
    };
    match raster(&layout, width_cols(&layout), height_rows(&layout), plain) {
        Ok(art) => Outcome::Art(art),
        // boxes/routes collided at cell scale: give dagre more room once
        Err(Fail::Retry) if tried_default => match pick(V_ROOMY) {
            Pick::Draw(l) => match raster(&l, width_cols(&l), height_rows(&l), plain) {
                Ok(art) => Outcome::Art(art),
                Err(_) => Outcome::Source,
            },
            Pick::Source => Outcome::Source,
            Pick::Unknown => Outcome::Unknown,
        },
        Err(_) => Outcome::Source,
    }
}

enum Pick {
    Draw(Rc<Layout>),
    Source,
    Unknown,
}

/// cache-only layout lookup: None = this variant was never tried
fn peek_layout(text: &str, variant: usize) -> Option<Option<Rc<Layout>>> {
    LAYOUTS.with(|c| {
        let mut c = c.borrow_mut();
        let hit = c
            .get_mut(text)
            .filter(|e| e.tried[variant])
            .map(|e| (e.got[variant].clone(), e.gen));
        if let Some((got, _)) = hit {
            if let Some(e) = c.get_mut(text) {
                e.gen = GEN.with(|g| g.get()); // probing is using it
            }
            return Some(got);
        }
        None
    })
}

#[cfg(test)]
pub(crate) fn clear_cache() {
    LAYOUTS.with(|c| c.borrow_mut().clear());
}

fn fits_window(layout: &Layout, max_cols: usize) -> bool {
    if layout.kind == DiagramKind::Pie {
        // a legend list never uses the pie's 510 px canvas: its rows are
        // gated by label width at draw time
        return height_rows(layout) <= MAX_ROWS;
    }
    // ER appends a relationship list under the grid: count its rows now,
    // not after the engine paid for the layout
    let extra = if layout.kind == DiagramKind::Er { layout.edges.len() + 1 } else { 0 };
    width_cols(layout) <= max_cols && height_rows(layout) + extra <= MAX_ROWS
}

fn width_cols(layout: &Layout) -> usize {
    (layout.width / SX).ceil() as usize + 2 // one cell of air each side
}

fn height_rows(layout: &Layout) -> usize {
    (layout.height / SY).ceil() as usize + 2
}

fn cached_layout(text: &str, variant: usize) -> Option<Rc<Layout>> {
    // Option<Option<..>>: a memoized refusal must not look like a miss, or
    // an over-cap or insane graph re-runs the engine on every frame
    if let Some(decided) = peek_layout(text, variant) {
        return decided;
    }
    let built = engine(text, variant);
    insert_layout(text, variant, built);
    LAYOUTS.with(|c| c.borrow().get(text).and_then(|e| e.got[variant].clone()))
}

/// record a decided layout (worker results arrive here on the UI thread);
/// the layout is Send plain data, wrapped in Rc only after the move
pub(crate) fn insert_layout(text: &str, variant: usize, built: Option<Layout>) {
    let gen = GEN.with(|g| g.get());
    LAYOUTS.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > CACHE_MAX {
            let stale: Vec<String> =
                c.iter().filter(|(_, e)| e.gen != gen).map(|(k, _)| k.clone()).collect();
            for k in stale {
                c.remove(&k);
            }
            if c.len() > CACHE_MAX {
                // soft cap: never evict what the live view draws. Dropping
                // a decided-but-shown fence would re-arm it, and the next
                // insert would evict its replacement - the cycle the
                // generation exists to prevent. Memory instead grows with
                // what the session opened (~100 KB per layout).
            }
        }
        let e = c.entry(text.to_string()).or_default();
        e.tried[variant] = true;
        e.got[variant] = built.map(Rc::new);
        e.gen = gen;
    });
}

/// the fence set changed: everything used from now on stamps this generation
pub(crate) fn new_generation() {
    GEN.with(|g| g.set(g.get() + 1));
}

/// keep an entry the current view still draws alive through eviction
pub(crate) fn keep(text: &str) {
    LAYOUTS.with(|c| {
        if let Some(e) = c.borrow_mut().get_mut(text) {
            e.gen = GEN.with(|g| g.get());
        }
    });
}

/// parse + layout under the panic discipline. Only flowcharts that come back
/// as graph data are eligible; everything else (sequence, pie, ...) stays
/// source until P3. `pub(crate)` for the pager's worker thread; it touches
/// no cache, so the two threads cannot race it.
pub(crate) fn engine(text: &str, variant: usize) -> Option<Layout> {
    with_quiet_panic(|| {
        let parsed = parse_mermaid_strict(text).ok()?; // preflight validator inside
        // P3: state and ER are graph data like the flowchart, sequence
        // carries head boxes + messages through the same channels, pie is
        // a legend list; every other kind has its own data shape and stays
        // source
        if !matches!(
            parsed.graph.kind,
            DiagramKind::Flowchart
                | DiagramKind::State
                | DiagramKind::Er
                | DiagramKind::Sequence
                | DiagramKind::Pie
        ) {
            return None;
        }
        // refuse before paying for layout: big graphs are slow AND illegible
        if parsed.graph.nodes.len() > MAX_NODES || parsed.graph.edges.len() > MAX_EDGES {
            return None;
        }
        let mut theme = Theme::modern();
        theme.font_size = 16.0;
        let cfg = LayoutConfig {
            fast_text_metrics: true, // no fontdb scan, no ~/.cache writes
            node_padding_x: match variant {
                V_COMPACT => 12.0,
                V_ROOMY => 36.0,
                _ => 30.0,
            },
            node_spacing: match variant {
                V_COMPACT => 24.0,
                V_ROOMY => 80.0,
                _ => 50.0,
            },
            rank_spacing: match variant {
                V_COMPACT => 30.0,
                V_ROOMY => 80.0,
                _ => 50.0,
            },
            ..LayoutConfig::default()
        };
        let layout = compute_layout(&parsed.graph, &theme, &cfg);
        // graph kinds get the geometry sanity pass; sequence and pie carry
        // their own data shapes and are admitted (or refused) inside their
        // rasters, where the gates understand lanes and legend rows
        let sane = match &layout.diagram {
            DiagramData::Graph { .. } => sane_geometry(&layout),
            DiagramData::Sequence(_) | DiagramData::Pie(_) => true,
            _ => false,
        };
        if sane {
            Some(layout)
        } else {
            None
        }
    })
    .flatten()
}

/// the catch_unwind is cheap insurance, but the default hook's print would
/// scribble over the alternate screen; one filtering hook for the process
/// avoids racing other threads over the global (a per-call swap could leave
/// the silent hook installed if another thread panics inside the window)
fn with_quiet_panic<T, F: FnOnce() -> T + panic::UnwindSafe>(f: F) -> Option<T> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if QUIET.with(|q| q.get()) {
                return; // engine panic: catch_unwind will turn it into text
            }
            default_hook(info);
        }));
    });
    QUIET.with(|q| q.set(true));
    let r = panic::catch_unwind(f);
    QUIET.with(|q| q.set(false));
    r.ok()
}

/// hostile/absurd geometry must not reach the raster (a NaN width becomes a
/// huge `as usize`, and one 1e9 px point makes the walk allocate forever)
fn sane_geometry(l: &Layout) -> bool {
    let ok = |v: f32| v.is_finite() && (0.0..=100_000.0).contains(&v);
    ok(l.width)
        && ok(l.height)
        && l.nodes.values().all(|n| ok(n.x) && ok(n.y) && ok(n.width) && ok(n.height))
        && l.edges.iter().all(|e| {
            e.points
                .iter()
                .all(|p| p.0.is_finite() && p.1.is_finite() && p.0.abs() <= 100_000.0 && p.1.abs() <= 100_000.0)
        })
}

/// labels must survive the grid intact: control characters and zero-width
/// codepoints shift every border after them off the column grid
fn printable(s: &str) -> bool {
    s.chars().all(|c| !c.is_control() && char_width(c) > 0)
}

// ---------------------------------------------------------------- failure ---

/// why the raster gave up: Retry means a roomier layout might fix it
#[derive(PartialEq)]
enum Fail {
    Retry,
    Fatal,
}

/// uniform refusal path; MDGLASS_BOXART_DEBUG=1 names the gate that fired
/// (tuning the gates without gdb requires exactly this)
fn gate(reason: &'static str, retry: bool) -> Fail {
    if std::env::var_os("MDGLASS_BOXART_DEBUG").is_some() {
        eprintln!("boxart: refuse ({reason})");
    }
    if retry {
        Fail::Retry
    } else {
        Fail::Fatal
    }
}

// --------------------------------------------------------------------- grid ---

const EMPTY: u8 = 0;
const FRAME: u8 = 1; // subgraph border
const EDGE: u8 = 2;  // edge line cell
const NODE: u8 = 3;  // box border or erased interior
const TEXT: u8 = 4;  // any glyph that is content: node/edge label, arrowhead

const M_N: u8 = 1;
const M_E: u8 = 2;
const M_S: u8 = 4;
const M_W: u8 = 8;
// combo constants: in pattern position `|` is or-pattern syntax, so bit-or
// masks must be named before they can be matched
const M_NS: u8 = M_N | M_S;
const M_EW: u8 = M_E | M_W;
const M_NE: u8 = M_N | M_E;
const M_NW: u8 = M_N | M_W;
const M_SE: u8 = M_S | M_E;
const M_SW: u8 = M_S | M_W;
const M_NES: u8 = M_N | M_E | M_S;
const M_NSW: u8 = M_N | M_S | M_W;
const M_NEW: u8 = M_N | M_E | M_W;
const M_ESW: u8 = M_E | M_S | M_W;
const M_ALL: u8 = M_N | M_E | M_S | M_W;

/// continuation cell of a double-width glyph: renders as nothing
const SKIP: char = '\u{0}';

#[derive(Clone, Copy)]
struct CellG {
    ch: char,
    mask: u8,
    owner: u8,
    /// which edge drew this line cell (-1: nobody), so an edge label may only
    /// blank its own line, never someone else's
    edge: i16,
}

impl Default for CellG {
    fn default() -> Self {
        CellG {
            ch: ' ',
            mask: 0,
            owner: EMPTY,
            edge: -1,
        }
    }
}

struct Grid {
    w: usize,
    h: usize,
    cells: Vec<CellG>,
}

impl Grid {
    fn new(w: usize, h: usize) -> Grid {
        Grid {
            w,
            h,
            cells: vec![CellG::default(); w * h],
        }
    }
    #[inline]
    fn inside(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h
    }
    fn get(&self, x: i32, y: i32) -> Option<CellG> {
        if self.inside(x, y) {
            Some(self.cells[y as usize * self.w + x as usize])
        } else {
            None
        }
    }
    /// higher owner wins: edges erase frames, nodes erase edges, text wins all
    fn put(&mut self, x: i32, y: i32, ch: char, owner: u8) {
        if !self.inside(x, y) {
            return;
        }
        let c = &mut self.cells[y as usize * self.w + x as usize];
        if owner >= c.owner {
            *c = CellG {
                ch,
                mask: 0,
                owner,
                edge: -1,
            };
        }
    }
    /// draw/merge a line cell: two edges meeting here keep both directions
    /// (a legitimate crossroads); the edge id records the later owner
    fn put_edge(&mut self, x: i32, y: i32, mask: u8, id: i16) {
        if !self.inside(x, y) {
            return;
        }
        let c = &mut self.cells[y as usize * self.w + x as usize];
        if c.owner < EDGE {
            *c = CellG {
                ch: ' ',
                mask,
                owner: EDGE,
                edge: id,
            };
        } else if c.owner == EDGE {
            c.mask |= mask;
            // a cell two routes share belongs to neither label
            if c.edge != id {
                c.edge = -2;
            }
        }
    }
    /// draw a string, consuming an extra placeholder cell after wide glyphs
    fn text(&mut self, x: i32, y: i32, s: &str, owner: u8) {
        let mut x = x;
        for ch in s.chars() {
            self.put(x, y, ch, owner);
            x += 1;
            if char_width(ch) == 2 {
                self.put(x, y, SKIP, owner);
                x += 1;
            }
        }
    }
}

// ------------------------------------------------------------------- raster ---

/// node rect in cell coordinates, inclusive on all four sides
#[derive(Clone, Copy, Debug)]
struct Rect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Rect {
    fn has(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// subgraph frames round INWARD (ceil/floor): outward rounding can put the
/// frame line on the same row a member box or a corridor edge occupies,
/// fusing frame and box into one line and faking membership
fn frame_rect(sg: &mermaid_rs_renderer::layout::SubgraphLayout) -> Option<Rect> {
    let r = Rect {
        x0: (sg.x / SX).ceil() as i32,
        y0: (sg.y / SY).ceil() as i32,
        x1: ((sg.x + sg.width) / SX).floor() as i32,
        y1: ((sg.y + sg.height) / SY).floor() as i32,
    };
    (r.x1 - r.x0 >= 4 && r.y1 - r.y0 >= 2).then_some(r)
}

/// longest contiguous run of steps along one grid line (horizontal line at
/// `fixed` y over x in [lo,hi], or vertical at `fixed` x over y in [lo,hi]).
/// >2 means the route RODE the line; a staircase crossing has runs of 1
fn run_along(cells: &[(i32, i32)], fixed: i32, lo: i32, hi: i32, horiz: bool) -> usize {
    let on = |c: &(i32, i32)| {
        if horiz {
            c.1 == fixed && c.0 >= lo && c.0 <= hi
        } else {
            c.0 == fixed && c.1 >= lo && c.1 <= hi
        }
    };
    let mut best = 0;
    let mut cur = 0;
    for w in cells.windows(2) {
        let parallel = if horiz { w[0].0 != w[1].0 } else { w[0].1 != w[1].1 };
        if parallel && on(&w[0]) && on(&w[1]) {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// total riding over all four frame lines (each line allows a 2-step
/// perpendicular approach to graze it)
fn frame_riders(cells: &[(i32, i32)], frames: &[Rect]) -> usize {
    frames
        .iter()
        .map(|f| {
            run_along(cells, f.y0, f.x0, f.x1, true).saturating_sub(2)
                + run_along(cells, f.y1, f.x0, f.x1, true).saturating_sub(2)
                + run_along(cells, f.x0, f.y0, f.y1, false).saturating_sub(2)
                + run_along(cells, f.x1, f.y0, f.y1, false).saturating_sub(2)
        })
        .sum()
}

/// what later edges must know about an already-drawn one
struct EdgeRec {
    cells: HashSet<(i32, i32)>,
    dirs: HashMap<(i32, i32), u8>, // this route's own direction bits at each cell
    /// arrowhead cells with the head's role (bit 1 = start, 2 = end):
    /// erasing one hides an endpoint
    heads: HashMap<(i32, i32), u8>,
    from: usize,
    to: usize,
}

fn raster(layout: &Layout, cols: usize, rows: usize, plain: bool) -> Result<Vec<String>, Fail> {
    // every kind enters through this one door: probe, next_warm and
    // terminal_ascii walk the same ladder for all of them, so no kind can
    // lie to the pager's cache probe about what a real run would draw
    match layout.kind {
        DiagramKind::Flowchart | DiagramKind::State | DiagramKind::Er => {
            graph_raster(layout, cols, rows, plain)
        }
        DiagramKind::Sequence => sequence_raster(layout, cols, rows, plain),
        DiagramKind::Pie => pie_list(layout, cols),
        _ => Err(gate("kind has no terminal art", false)),
    }
}

fn graph_raster(layout: &Layout, cols: usize, rows: usize, plain: bool) -> Result<Vec<String>, Fail> {
    let kind = layout.kind;
    // per-kind admission (docs/terminal-mermaid-plan.md): anything this
    // renderer would misstate refuses before touching the grid
    if let DiagramData::Graph { state_notes } = &layout.diagram {
        if kind == DiagramKind::State && !state_notes.is_empty() {
            return Err(gate("state notes have no drawer", false));
        }
    }
    if kind == DiagramKind::State
        && layout
            .nodes
            .values()
            .any(|n| matches!(n.shape, NodeShape::ForkJoin | NodeShape::Text))
    {
        return Err(gate("choice/fork pseudo-states are not drawn", false));
    }
    if kind == DiagramKind::Er
        && layout.edges.iter().any(|e| {
            e.from == e.to
                || [e.start_decoration, e.end_decoration]
                    .into_iter()
                    .flatten()
                    .any(|d| {
                        !matches!(
                            d,
                            EdgeDecoration::CrowsFootOne
                                | EdgeDecoration::CrowsFootZeroOne
                                | EdgeDecoration::CrowsFootMany
                                | EdgeDecoration::CrowsFootZeroMany
                        )
                    })
        })
    {
        return Err(gate("er relations need crow's-foot ends, no self-links", false));
    }
    let mut g = Grid::new(cols, rows);
    let cx = |x: f32| (x / SX).round() as i32;
    let cy = |y: f32| (y / SY).round() as i32;

    // visible (non-anchor) nodes, in stable id order. State dots ([*] start
    // and end markers) draw as a single glyph, so their footprint is that
    // one cell, not the 16 px circle's rounded rect - and the cell slides
    // a step clear of any box it rounded onto (a dot is a marker; moving
    // it half a cell is rounding's cost, eating a box wall is not)
    let mut nodes: Vec<(&str, Rect)> = layout
        .nodes
        .values()
        .filter(|n| !n.hidden)
        .map(|n| {
            let rect = Rect {
                x0: cx(n.x),
                y0: cy(n.y),
                x1: cx(n.x + n.width),
                y1: cy(n.y + n.height),
            };
            let cell = matches!(n.shape, NodeShape::Circle | NodeShape::DoubleCircle);
            (n.id.as_str(), if cell { Rect { x0: (rect.x0 + rect.x1) / 2, y0: (rect.y0 + rect.y1) / 2, x1: (rect.x0 + rect.x1) / 2, y1: (rect.y0 + rect.y1) / 2 } } else { rect })
        })
        .collect();
    let boxes: Vec<Rect> = layout
        .nodes
        .values()
        .filter(|n| !n.hidden && !matches!(n.shape, NodeShape::Circle | NodeShape::DoubleCircle))
        .map(|n| Rect {
            x0: cx(n.x),
            y0: cy(n.y),
            x1: cx(n.x + n.width),
            y1: cy(n.y + n.height),
        })
        .collect();
    for (id, r) in &mut nodes {
        if !matches!(
            layout.nodes[*id].shape,
            NodeShape::Circle | NodeShape::DoubleCircle
        ) {
            continue;
        }
        // search a small spiral for the dot's cell: it must sit off every
        // box and every frame line, with one row of clearance from boxes
        // (an arrow into a dot that touches a wall has nowhere to put its
        // head); below comes first, matching how the SVG hangs end states
        let frame_lines: Vec<Rect> = layout.subgraphs.iter().filter_map(frame_rect).collect();
        let mut cands: Vec<(i32, i32, i32)> = Vec::new();
        for dx in -3i32..=3 {
            for dy in -3i32..=3 {
                let score = dx * dx * 4 + dy * dy * 4 + (4 - dy).min(4) + dx.abs();
                cands.push((score, dx, dy));
            }
        }
        cands.sort_unstable();
        let mut moved = false;
        for (_, dx, dy) in cands {
            let cand = Rect {
                x0: r.x0 + dx,
                y0: r.y0 + dy,
                x1: r.x1 + dx,
                y1: r.y1 + dy,
            };
            if cand.x0 < 1 || cand.y0 < 0 || cand.x0 >= cols as i32 - 1 || cand.y0 >= rows as i32 {
                continue;
            }
            if boxes.iter().any(|b| b.has(cand.x0, cand.y0)) {
                continue;
            }
            if [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(nx, ny)| boxes.iter().any(|b| b.has(cand.x0 + nx, cand.y0 + ny)))
            {
                continue;
            }
            let on_frame = frame_lines.iter().any(|f| {
                ((cand.y0 == f.y0 || cand.y0 == f.y1) && (f.x0..=f.x1).contains(&cand.x0))
                    || ((cand.x0 == f.x0 || cand.x0 == f.x1)
                        && (f.y0..=f.y1).contains(&cand.y0))
            });
            if on_frame {
                continue;
            }
            *r = cand;
            moved = true;
            break;
        }
        if !moved {
            return Err(gate("no free cell for a state dot", true));
        }
    }
    if nodes.is_empty() || nodes.len() > MAX_NODES {
        return Err(gate("no visible nodes or too many of them", false));
    }
    // cell rounding fusing two boxes means px space was never there: refuse,
    // do not move geometry (moving it can fake subgraph membership)
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            let (a, b) = (nodes[i].1, nodes[j].1);
            if a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1 {

                return Err(gate("boxes share cells", true));
            }
        }
    }
    let node_at = |x: i32, y: i32| nodes.iter().position(|(_, r)| r.has(x, y));

    // everything the reader must still find after every later write: checked
    // one final time before emit, because a label eaten in passing is a lie
    let mut placed: Vec<(i32, i32, &str)> = Vec::new();

    // 1. subgraph frames under everything, rounded inward
    let mut frames: Vec<Rect> = Vec::new();
    for sg in &layout.subgraphs {
        let title = sg.label.trim();
        let Some(f) = frame_rect(sg) else {
            // skipping a small frame silently drops its title and membership
            return Err(gate("subgraph frame too small to draw", true));
        };
        if !title.is_empty() && !printable(title) {
            return Err(gate("subgraph title has control characters", false));
        }
        // a box border landing on the frame line fuses them: the reader
        // cannot tell membership apart from adjacency
        for (_, r) in &nodes {
            let xover = r.x0 <= f.x1 && f.x0 <= r.x1;
            let yover = r.y0 <= f.y1 && f.y0 <= r.y1;
            // equality OR span: a box whose rows straddle a frame line fuses
            // with it just as surely as one that merely touches it
            let on_h = xover
                && ((r.y0..=r.y1).contains(&f.y0) || (r.y0..=r.y1).contains(&f.y1));
            let on_v = yover
                && ((r.x0..=r.x1).contains(&f.x0) || (r.x0..=r.x1).contains(&f.x1));
            if on_h || on_v {

                return Err(gate("box border fuses with frame line", true));
            }
        }
        let (tl, tr, bl, br, h, v) = corners(plain);
        for x in f.x0 + 1..f.x1 {
            g.put(x, f.y0, h, FRAME);
            g.put(x, f.y1, h, FRAME);
        }
        for y in f.y0 + 1..f.y1 {
            g.put(f.x0, y, v, FRAME);
            g.put(f.x1, y, v, FRAME);
        }
        g.put(f.x0, f.y0, tl, FRAME);
        g.put(f.x1, f.y0, tr, FRAME);
        g.put(f.x0, f.y1, bl, FRAME);
        g.put(f.x1, f.y1, br, FRAME);
        if !title.is_empty() {
            // titles are find-targets: they get text priority (edges and
            // node boxes must not run over them) + the survival check
            if visible_width(title) + 4 > (f.x1 - f.x0) as usize {
                return Err(gate("subgraph title does not fit", true));
            }
            g.text(f.x0 + 2, f.y0, title, TEXT);
            placed.push((f.x0 + 2, f.y0, title));
        }
        frames.push(f);
    }

    // state composites own a hidden anchor node inside their subgraph
    // frame, and transitions attach to it. Half-drawing the connection
    // would misstate which boxes link; the honest endpoint is the FRAME -
    // the members live inside it, and an arrowhead at a phantom dot would
    // be worse than one at the wall
    let mut anchors: Vec<(&str, usize)> = Vec::new(); // hidden member id -> frame
    for (fi, sg) in layout.subgraphs.iter().enumerate() {
        for m in &sg.nodes {
            if layout.nodes.get(m).is_some_and(|n| n.hidden) {
                anchors.push((m.as_str(), fi));
            }
        }
    }
    // a state composite's own pseudo-node points at its frame but is not
    // listed among the frame's members: it anchors edges (Running ->
    // Working) at the frame, exactly like a member would
    for n in layout.nodes.values() {
        if n.hidden
            && n.anchor_subgraph.is_some()
            && !anchors.iter().any(|(m, _)| *m == n.id.as_str())
        {
            if let Some(fi) = n.anchor_subgraph {
                anchors.push((n.id.as_str(), fi));
            }
        }
    }

    // an endpoint is a visible box (index < nodes) or an anchor's frame;
    // both answer "does this cell belong to the endpoint". For plain boxes
    // this is the exact old index comparison: cell rounding never fuses
    // two boxes (the overlap gate refuses first), so flowchart art is
    // unchanged
    // which cells the state dots actually draw in (their 1-cell rects):
    // the engine's polyline still aims at the px circle centre, so edge
    // endpoints must be re-aimed at the cell the reader will find
    let dot_at: Vec<Option<(i32, i32)>> = nodes
        .iter()
        .map(|(id, r)| {
            matches!(
                layout.nodes[*id].shape,
                NodeShape::Circle | NodeShape::DoubleCircle
            )
            .then_some((r.x0, r.y0))
        })
        .collect();
    let end_has = |t: usize, x: i32, y: i32| -> bool {
        if t < nodes.len() {
            nodes[t].1.has(x, y)
        } else {
            frames[anchors[t - nodes.len()].1].has(x, y)
        }
    };

    // 2. edges: polyline -> 4-connected cell walk, gated, then drawn
    let mut drawn: Vec<EdgeRec> = Vec::new();
    for (ei, e) in layout.edges.iter().enumerate() {
        let id = ei as i16;
        let edge_node = |nid: &str| -> Option<usize> {
            nodes
                .iter()
                .position(|(n, _)| *n == nid)
                .or_else(|| anchors.iter().position(|(a, _)| *a == nid).map(|p| nodes.len() + p))
        };
        let (Some(from), Some(to)) = (edge_node(&e.from), edge_node(&e.to)) else {
            // an edge to a node this art does not model at all: drawing
            // half the connection would misrepresent which boxes are linked

            return Err(gate("edge touches an unmodelled node", false));
        };
        // start/end labels are edge syntax we would silently drop
        for lb in [e.start_label.as_ref(), e.end_label.as_ref()].into_iter().flatten() {
            if lb.lines.iter().any(|l| !l.trim().is_empty()) {
                return Err(gate("start/end edge labels are dropped syntax", false));
            }
        }
        if e.points.len() < 2 {
            return Err(gate("edge without a route", false));
        }
        let mut pts: Vec<(f32, f32)> = e.points.clone();
        if let Some((x, y)) = dot_at.get(from).copied().flatten() {
            *pts.first_mut().unwrap() = (x as f32 * SX, y as f32 * SY);
        }
        if let Some((x, y)) = dot_at.get(to).copied().flatten() {
            *pts.last_mut().unwrap() = (x as f32 * SX, y as f32 * SY);
        }
        let cells = walk(&pts);
        if cells.len() < 2 {
            return Err(gate("edge route collapses to one cell", true));
        }
        // direction bits of this route at each of its cells
        let mut dirs: HashMap<(i32, i32), u8> = HashMap::new();
        for i in 0..cells.len() {
            let mut m = 0;
            if i > 0 {
                m |= towards(cells[i], cells[i - 1]);
            }
            if i + 1 < cells.len() {
                m |= towards(cells[i], cells[i + 1]);
            }
            *dirs.entry(cells[i]).or_insert(0) |= m;
        }
        for &(x, y) in &cells {
            // a cell inside a box that is not one of this edge's endpoints
            // (endpoint cells survive anyway: nodes paint over them)
            if node_at(x, y).is_some() && !end_has(from, x, y) && !end_has(to, x, y) {
                return Err(gate("edge routed through a foreign box", true));
            }
        }
        // frame riders: the route runs along a frame line (rounding fused
        // parallel lanes onto it). Perpendicular and staircase crossings have
        // no run longer than 2 steps on any one line; riding a corridor one
        // cell off a BOX border is faithful (the SVG shows the same) and not
        // gated — on a FRAME line it erases the membership information
        let riders = frame_riders(&cells, &frames);
        if riders > 0 {
            return Err(gate("edge rides a subgraph frame", true));
        }
        // cross-edge contact: sharing a cell is honest when the routes share
        // an endpoint (they converge at it anyway) or cross as two straight
        // lines (┼ reads correctly); anything else merges two connections
        // into one shape that exists in no source file
        for prev in &drawn {
            // shared role, not just a shared node: A-->B and B-->A converge
            // at both ends yet read as two arrows, so only same-role pairs
            // (both leaving A / both entering B) may merge freely
            let shares = prev.from == from || prev.to == to;
            let shared: Vec<(i32, i32)> = cells.iter().filter(|c| prev.cells.contains(c)).copied().collect();
            if shared.is_empty() {
                continue;
            }
            // a later route running through an earlier route's arrowhead
            // erases where that arrow ends, unless both share the head's role
            let hits_head = shared.iter().any(|c| match prev.heads.get(c) {
                Some(roles) => {
                    (roles & 1 != 0 && prev.from != from) || (roles & 2 != 0 && prev.to != to)
                }
                None => false,
            });
            if hits_head {
                return Err(gate("route runs through a foreign arrowhead", true));
            }
            if !shares {
                let cross = shared.iter().all(|c| {
                    let mine = dirs.get(c).copied().unwrap_or(0);
                    let theirs = prev.dirs.get(c).copied().unwrap_or(0);
                    let straight_ns = |m: u8| m != 0 && m & !M_NS == 0;
                    let straight_ew = |m: u8| m != 0 && m & !M_EW == 0;
                    (straight_ns(mine) && straight_ew(theirs)) || (straight_ew(mine) && straight_ns(theirs))
                });
                if !cross {
                    return Err(gate("two routes merge into a shape no source draws", true));
                }
            }
            // stacked (parallel, same direction) beyond a few cells: spaghetti
            let inside_ends = |c: &(i32, i32)| {
                end_has(from, c.0, c.1)
                    || end_has(to, c.0, c.1)
                    || end_has(prev.from, c.0, c.1)
                    || end_has(prev.to, c.0, c.1)
            };
            if shared.iter().filter(|c| !inside_ends(c)).count() > MAX_SHARED_CELLS {
                return Err(gate("two routes stacked on each other", true));
            }
        }

        // draw
        for i in 0..cells.len() {
            g.put_edge(cells[i].0, cells[i].1, dirs[&cells[i]], id);
        }
        drawn.push(EdgeRec {
            cells: cells.iter().copied().collect(),
            dirs,
            heads: HashMap::new(),
            from,
            to,
        });

        // arrowheads: the last/first cell outside both endpoint boxes, and it
        // must actually touch the box it points at (no dangling arrows)
        let outside = |i: usize| {
            let (x, y) = cells[i];
            !end_has(from, x, y) && !end_has(to, x, y)
        };
        let mut heads: Vec<(usize, bool)> = Vec::new(); // (cell, is_start)
        if e.arrow_end {
            match (0..cells.len()).rev().find(|&i| outside(i)) {
                Some(i) => heads.push((i, false)),
                None => {

                    return Err(gate("no room for the end arrowhead", true));
                }
            }
        }
        if e.arrow_start {
            match (0..cells.len()).find(|&i| outside(i)) {
                Some(i) => heads.push((i, true)),
                None => return Err(gate("no room for the start arrowhead", true)),
            }
        }
        for (i, start) in heads {
            let (hx, hy) = cells[i];
            // aim at the endpoint box itself, not at where the line came
            // from: on a staircase the arrival step can be sideways while
            // the box sits straight below, and the glyph must point at the
            // connection the reader will trace
            let target = if start { from } else { to };
            let aim = [(hx + 1, hy), (hx - 1, hy), (hx, hy + 1), (hx, hy - 1)]
                .into_iter()
                .find(|(x, y)| end_has(target, *x, *y));
            let Some(aim) = aim else {
                return Err(gate("arrowhead dangles free of its box", true));
            };
            // a head stamped on another route's line invents an endpoint
            // there unless that route shares this head's role
            for prev in &drawn {
                let same_role = if start { prev.from == from } else { prev.to == to };
                if prev.cells.contains(&(hx, hy)) && !same_role {
                    return Err(gate("arrowhead stamped on a foreign line", true));
                }
            }
            let dec = if start { &e.start_decoration } else { &e.end_decoration };
            let glyph = match dec {
                Some(EdgeDecoration::Circle) => 'o',
                Some(EdgeDecoration::Cross) => 'x',
                Some(EdgeDecoration::Diamond) => diamond(plain),
                Some(EdgeDecoration::DiamondFilled) => diamond_filled(plain),
                Some(_) => return Err(gate("crow's foot etc. is not flowchart art", false)),
                // direction from the drawn cells, not px: the glyph points
                // where the reader can see the line going — into the box
                None => head_between((hx, hy), aim, plain),
            };
            g.put(hx, hy, glyph, TEXT);
            *drawn.last_mut().unwrap().heads.entry((hx, hy)).or_default() |= if start { 1 } else { 2 };
        }
    }

    // 3. nodes: opaque boxes, centered multi-line labels (never ellipsized)
    for (id, r) in &nodes {
        let n = layout.nodes.get(*id).ok_or_else(|| gate("node vanished mid-raster", false))?;
        // defensive: any TEXT cell under this box (a title, a stray head)
        // would be erased here, and no gate promised it survives
        let cuts_text = (r.y0..=r.y1)
            .flat_map(|y| (r.x0..=r.x1).map(move |x| (x, y)))
            .any(|(x, y)| g.get(x, y).is_some_and(|c| c.owner == TEXT));
        if cuts_text {
            return Err(gate("node box would erase existing text", true));
        }
        let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
        if matches!(n.shape, NodeShape::Circle | NodeShape::DoubleCircle) {
            // state start/end pseudo-states: one dot, no box (a frame
            // around a dot at cell scale reads as a state box, which is a
            // lie); the dot carries the meaning and no text to preserve
            let glyph = match n.shape {
                NodeShape::Circle => if plain { 'o' } else { '\u{25cf}' },
                _ => if plain { '@' } else { '\u{25c9}' },
            };
            g.put((r.x0 + r.x1) / 2, (r.y0 + r.y1) / 2, glyph, NODE);
            continue;
        }
        if w < 4 || h < 2 {
            return Err(gate("a box this thin cannot hold its label honestly", true));
        }
        let corners = shape_corners(n.shape, plain);
        put_label_box(
            &mut g,
            *r,
            &n.label.lines,
            corners,
            &mut placed,
            matches!(layout.kind, DiagramKind::Er),
        )?;
    }

    // 4. edge labels: centered on the engine's anchor; the window may cover
    // empty cells or this edge's own line, nothing else (a label sitting on
    // a stranger's line belongs to it visually); one row up/down is allowed
    for e in &layout.edges {
        let lb = match &e.label {
            Some(lb) if lb.lines.iter().any(|l| !l.trim().is_empty()) => lb,
            _ => continue,
        };
        if lb.lines.iter().any(|l| !printable(l)) {
            return Err(gate("edge label has control or zero-width characters", false));
        }
        let (ax, ay) = e.label_anchor.ok_or_else(|| gate("edge label without an anchor", false))?;
        let own = layout
            .edges
            .iter()
            .position(|x| std::ptr::eq(x, e))
            .unwrap_or(0) as i16;
        let w = lb.lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
        let mut placed_ok = false;
        // the SVG label hangs loosely near its line's anchor: a few rows up
        // or down all still read as belonging to it; more than that would
        // name a different line, so the search stops at two. A gap from
        // neighbouring labels comes first; sharing a border with a word
        // is ugly, refusing the diagram is worse
        'row: for (require_gap, dy) in [true, false]
            .into_iter()
            .flat_map(|gap| [0i32, -1, 1, -2, 2].map(move |dy| (gap, dy)))
        {
            // SVG hangs labels beside their line, not dead centre; when
            // the centre sits on a foreign vertical, drift sideways
            'cell: for dx in [0i32, -1, 1, -2, 2, -3, 3, -4, 4] {
            let y = cy(ay) + dy - (lb.lines.len() as i32 - 1) / 2;
            let x = cx(ax) + dx - w as i32 / 2;
            if !(g.inside(x, y) && g.inside(x + w as i32 - 1, y + lb.lines.len() as i32 - 1)) {
                continue 'cell;
            }
            for r in 0..lb.lines.len() as i32 {
                for xx in x - 1..=x + w as i32 {
                    // one column of air on each side: two labels touching
                    // read as one word ("pausedispatch" is not two labels)
                    let Some(c) = g.get(xx, y + r) else {
                        continue 'cell;
                    };
                    if xx == x - 1 || xx == x + w as i32 {
                        if c.owner == TEXT && require_gap {
                            continue 'cell;
                        }
                        continue;
                    }
                    if c.owner > EDGE || (c.owner == EDGE && c.edge != own) {
                        continue 'cell;
                    }
                }
            }
            for (i, line) in lb.lines.iter().enumerate() {
                let pad = (w - visible_width(line)) as i32 / 2;
                g.text(x + pad, y + i as i32, line, TEXT);
                if !line.trim().is_empty() {
                    placed.push((x + pad, y + i as i32, line));
                }
            }
            placed_ok = true;
            break 'row;
            }
        }
        if !placed_ok {

            return Err(gate("no free row for an edge label", true));
        }
    }

    // 5. survival check: every label and title must still be intact
    check_survival(&g, &placed)?;

    let mut out = emit(g, plain)?;

    // 6. ER: cardinality lives in decorations the SVG paints as endpoint
    // markers; placing glyphs at cell scale on arbitrary routes would
    // misstate their meaning, so the pairs are spelled out as a list under
    // the art (loss-free: notation + both entities + verb)
    if kind == DiagramKind::Er {
        out.push(String::new());
        out.push("Relationships:".to_string());
        for e in &layout.edges {
            let name = |id: &str| -> String {
                layout
                    .nodes
                    .get(id)
                    .and_then(|n| n.label.lines.first())
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty() && l != "---")
                    .unwrap_or_else(|| id.to_string())
            };
            let sep = if matches!(e.style, mermaid_rs_renderer::ir::EdgeStyle::Dotted) { ".." } else { "--" };
            let verb = e
                .label
                .as_ref()
                .map(|l| l.lines.join(" "))
                .unwrap_or_default()
                .trim()
                .to_string();
            let line = format!(
                "  {}  {}{}{}  {}{}",
                name(&e.from),
                er_card(e.start_decoration, true),
                sep,
                er_card(e.end_decoration, false),
                name(&e.to),
                if verb.is_empty() { String::new() } else { format!("   {verb}") }
            );
            if visible_width(&line) > cols {
                return Err(gate("a relationship line is too wide", false));
            }
            out.push(line);
        }
    }
    Ok(out)
}

/// crow's foot notation in ASCII, mirrored for the start side
fn er_card(dec: Option<EdgeDecoration>, start: bool) -> &'static str {
    match dec {
        Some(EdgeDecoration::CrowsFootOne) => "||",
        Some(EdgeDecoration::CrowsFootZeroOne) => if start { "|o" } else { "o|" },
        Some(EdgeDecoration::CrowsFootMany) => if start { "}|" } else { "|{" },
        Some(EdgeDecoration::CrowsFootZeroMany) => if start { "{o" } else { "o{" },
        _ => "-", // undecorated link: the line alone carries it
    }
}

/// walk a polyline of px points as a 4-connected staircase of cells
fn walk(pts: &[(f32, f32)]) -> Vec<(i32, i32)> {
    let c = |p: (f32, f32)| ((p.0 / SX).round() as i32, (p.1 / SY).round() as i32);
    let mut out = vec![c(pts[0])];
    for seg in pts.windows(2) {
        let (mut x, mut y) = *out.last().unwrap();
        let (tx, ty) = c(seg[1]);
        let (dx, dy) = (tx - x, ty - y);
        let (adx, ady) = (dx.abs(), dy.abs());
        let (sx, sy) = (dx.signum(), dy.signum());
        // Bresenham, 4-connected: when the error term fires, step onto the
        // intermediate cell first or the path cuts a diagonal the terminal
        // cannot draw and the junction glyphs lose their neighbor
        if adx >= ady {
            let mut err = 0i32;
            for _ in 0..adx {
                x += sx;
                err += ady;
                if 2 * err >= adx {
                    out.push((x, y));
                    y += sy;
                    err -= adx;
                }
                out.push((x, y));
            }
        } else {
            let mut err = 0i32;
            for _ in 0..ady {
                y += sy;
                err += adx;
                if 2 * err >= ady {
                    out.push((x, y));
                    x += sx;
                    err -= ady;
                }
                out.push((x, y));
            }
        }
    }
    // consecutive duplicates (rounded equal points) are noise
    out.dedup();
    out
}

fn towards(from: (i32, i32), to: (i32, i32)) -> u8 {
    match (to.0 - from.0, to.1 - from.1) {
        (1, _) => M_E,
        (-1, _) => M_W,
        (_, 1) => M_S,
        _ => M_N,
    }
}

/// glyph for a line cell from its N/E/S/W mask; arrowheads are text cells and
/// connect via their neighbor's mask, so masks need no diagonal cases
fn glyph(mask: u8, plain: bool) -> char {
    // single-bit masks point at an arrowhead or one-cell stub, not a line end
    if plain {
        return match mask {
            M_N | M_S | M_NS => '|',
            M_E | M_W | M_EW => '-',
            _ => '+',
        };
    }
    match mask {
        M_N | M_S | M_NS => '│',
        M_E | M_W | M_EW => '─',
        M_NE => '└',
        M_NW => '┘',
        M_SE => '┌',
        M_SW => '┐',
        M_NES => '├',
        M_NSW => '┤',
        M_NEW => '┴',
        M_ESW => '┬',
        M_ALL => '┼',
        _ => '─',
    }
}

/// the head glyph from the two cells the reader can actually see: the head
/// points where the line came from says it is going
fn head_between(from: (i32, i32), to: (i32, i32), plain: bool) -> char {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    if dx.abs() >= dy.abs() {
        if dx >= 0 {
            if plain {
                '>'
            } else {
                '►'
            }
        } else if plain {
            '<'
        } else {
            '◄'
        }
    } else if dy >= 0 {
        if plain {
            'v'
        } else {
            '▼'
        }
    } else if plain {
        '^'
    } else {
        '▲'
    }
}

fn corners(plain: bool) -> (char, char, char, char, char, char) {
    if plain {
        ('+', '+', '+', '+', '-', '|')
    } else {
        ('┌', '┐', '└', '┘', '─', '│')
    }
}

fn shape_corners(shape: NodeShape, plain: bool) -> (char, char, char, char, char, char) {
    let round = matches!(
        shape,
        NodeShape::RoundRect | NodeShape::Stadium | NodeShape::Circle | NodeShape::DoubleCircle | NodeShape::Cylinder
    );
    if round && !plain {
        ('╭', '╮', '╰', '╯', '─', '│')
    } else {
        corners(plain) // diamonds etc. draw as plain boxes; the label is the point
    }
}

fn diamond(plain: bool) -> char {
    if plain {
        '>'
    } else {
        '◇'
    }
}

fn diamond_filled(plain: bool) -> char {
    if plain {
        '>'
    } else {
        '◆'
    }
}

#[cfg(test)]
mod tests;

/// opaque box with a centered multi-line label: erase interior, frame, text;
/// refuses if the label would be eaten and registers it for the survival
/// check. Shared by graph nodes and sequence actors/notes.
fn put_label_box<'a>(
    g: &mut Grid,
    r: Rect,
    label: &'a [String],
    corners: (char, char, char, char, char, char),
    placed: &mut Vec<(i32, i32, &'a str)>,
    rule: bool,
) -> Result<(), Fail> {
    let (tl, tr, bl, br, hb, vb) = corners;
    for y in r.y0 + 1..r.y1 {
        for x in r.x0 + 1..r.x1 {
            g.put(x, y, ' ', NODE); // erase edges routed under the box
        }
    }
    for x in r.x0 + 1..r.x1 {
        g.put(x, r.y0, hb, NODE);
        g.put(x, r.y1, hb, NODE);
    }
    for y in r.y0 + 1..r.y1 {
        g.put(r.x0, y, vb, NODE);
        g.put(r.x1, y, vb, NODE);
    }
    g.put(r.x0, r.y0, tl, NODE);
    g.put(r.x1, r.y0, tr, NODE);
    g.put(r.x0, r.y1, bl, NODE);
    g.put(r.x1, r.y1, br, NODE);

    let inner_w = (r.x1 - r.x0 - 1) as usize; // cells strictly inside borders
    let inner_h = (r.y1 - r.y0 - 1) as usize;
    if label.is_empty() {
        return Err(gate("node without a label: fine in SVG, pointless in art", false));
    }
    if label.iter().any(|l| !printable(l)) {
        return Err(gate("label has control or zero-width characters", false));
    }
    if label.len() > inner_h || label.iter().any(|l| visible_width(l) > inner_w) {
        return Err(gate("label would be eaten", true));
    }
    let y0 = r.y0 + 1 + (inner_h - label.len()) as i32 / 2;
    for (i, line) in label.iter().enumerate() {
        // an ER box separates its name from the attributes with "---":
        // draw that as a rule across the box, not as three hyphens
        if rule && line.trim() == "---" {
            for xx in r.x0 + 1..r.x1 {
                g.put(xx, y0 + i as i32, hb, NODE);
            }
            continue;
        }
        let pad = (inner_w - visible_width(line)) as i32 / 2;
        let lx = r.x0 + 1 + pad;
        let ly = y0 + i as i32;
        g.text(lx, ly, line, TEXT);
        if !line.trim().is_empty() {
            placed.push((lx, ly, line));
        }
    }
    Ok(())
}

/// every label and title must still read exactly as drawn, where it was
/// drawn: a later write that ate a glyph in passing is a lie. Shared by
/// the graph and sequence rasters.
fn check_survival(g: &Grid, placed: &[(i32, i32, &str)]) -> Result<(), Fail> {
    for (x, y, s) in placed {
        let mut xx = *x;
        for ch in s.chars() {
            match g.get(xx, *y) {
                Some(c) if c.ch == ch => xx += 1,
                _ => return Err(gate("a label did not survive the drawing", true)),
            }
            if char_width(ch) == 2 {
                match g.get(xx, *y) {
                    Some(c) if c.ch == SKIP => xx += 1,
                    _ => return Err(gate("a label did not survive the drawing", true)),
                }
            }
        }
    }
    Ok(())
}

/// line masks -> glyphs, then rows: leading/trailing blank rows cut,
/// trailing spaces trimmed (render.rs re-pads for color)
fn emit(mut g: Grid, plain: bool) -> Result<Vec<String>, Fail> {
    // masks -> glyphs for bare line cells (junctions pick the right corner)
    for y in 0..g.h {
        for x in 0..g.w {
            let c = g.cells[y * g.w + x];
            // routed lines carry a direction mask; the sequence raster draws
            // its lanes and arrows as finished glyphs (mask 0) - glyph()
            // must never touch those
            if c.owner == EDGE && c.mask != 0 {
                g.cells[y * g.w + x].ch = glyph(c.mask, plain);
            }
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(g.h);
    for y in 0..g.h {
        let mut s = String::with_capacity(g.w);
        for x in 0..g.w {
            let ch = g.cells[y * g.w + x].ch;
            if ch != SKIP {
                s.push(ch);
            }
        }
        while s.ends_with(' ') {
            s.pop();
        }
        out.push(s);
    }
    while out.first().is_some_and(|s| s.is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|s| s.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        return Err(gate("nothing drew", false));
    }
    Ok(out)
}


/// rounding px to grid rows is ours to resolve: claim the wanted row band,
/// sliding up or down by at most two rows, and only where every row in the
/// band is still unclaimed. Two events sharing one row would read as one
/// event; sliding is honest, silent merging is not.
fn claim_rows(want: i32, n: i32, used: &mut HashSet<i32>, rows: usize) -> Option<i32> {
    // slide order prefers DOWN: sequence flows top-down, so moving a
    // crowder's row downward keeps events in their source order, while an
    // upward slide can invert containment (a frame top above the previous
    // frame's bottom)
    for off in [0, 1, 2, -1] {
        let r = want + off;
        if r < 0 || r + n > rows as i32 {
            continue;
        }
        if (r..r + n).all(|y| !used.contains(&y)) {
            for y in r..r + n {
                used.insert(y);
            }
            return Some(r);
        }
    }
    None
}

/// can a box occupy this whole rectangle? only empty or FRAME-level cells
/// (lifelines, frame walls) may be covered - an edge or a label underneath
/// would be erased, and an erased message is a lie. Boxes slide by up to
/// two rows to find such a band; that is rounding's cost, not the reader's
fn band_clear(g: &Grid, r: Rect) -> bool {
    (r.y0..=r.y1).all(|y| {
        (r.x0..=r.x1).all(|x| match g.get(x, y) {
            Some(c) => c.owner <= FRAME,
            None => false,
        })
    })
}

/// sequence diagrams speak in lanes and rows, not routed boxes: lifeline x
/// maps to a column, message y to a row. Source order IS the topology, so
/// messages are never merged or reordered. Rows are claimed top-down as
/// frames, boxes, arrows and notes draw; when px rounding crowds two
/// distinct events into one row, the later one slides by at most one row
/// (never reorders, never moves px geometry - rounding is ours to resolve),
/// and only a row that no shift can free refuses.
fn sequence_raster(layout: &Layout, cols: usize, rows: usize, plain: bool) -> Result<Vec<String>, Fail> {
    let DiagramData::Sequence(seq) = &layout.diagram else {
        return Err(gate("sequence without sequence data", false));
    };
    let cx = |x: f32| (x / SX).round() as i32;
    let cy = |y: f32| (y / SY).round() as i32;
    // admission: unsupported syntax refuses whole, never partially
    if !seq.numbers.is_empty() || !seq.boxes.is_empty() {
        return Err(gate("autonumber or participant boxes are not drawn", false));
    }
    for f in &seq.frames {
        if !matches!(
            f.kind,
            mermaid_rs_renderer::ir::SequenceFrameKind::Alt
                | mermaid_rs_renderer::ir::SequenceFrameKind::Opt
                | mermaid_rs_renderer::ir::SequenceFrameKind::Loop
        ) {
            return Err(gate("only alt/opt/loop frames are drawn", false));
        }
    }
    for n in &seq.notes {
        if !matches!(n.position, mermaid_rs_renderer::ir::SequenceNotePosition::Over) {
            return Err(gate("leftOf/rightOf notes are not drawn", false));
        }
    }
    if layout.nodes.values().any(|n| !matches!(n.shape, NodeShape::ActorBox)) {
        return Err(gate("stick-figure actors are not drawn", false));
    }
    if seq.lifelines.len() < 2 {
        return Err(gate("sequence needs at least two lanes", false));
    }
    let mut g = Grid::new(cols, rows);
    let mut placed: Vec<(i32, i32, &str)> = Vec::new();
    let (tl, tr, bl, br, hb, vb) = corners(plain);

    let cross = if plain { '+' } else { '\u{253c}' };
    let t_r = if plain { '+' } else { '\u{251c}' };
    let t_l = if plain { '+' } else { '\u{2524}' };
    let head_r = if plain { '>' } else { '\u{25ba}' };
    let head_l = if plain { '<' } else { '\u{25c4}' };
    let bar = if plain { '!' } else { '\u{2551}' };
    let dash = |dotted: bool, plain: bool| -> char {
        if plain {
            '-'
        } else if dotted {
            '\u{254c}'
        } else {
            '\u{2500}'
        }
    };

    // lane column per participant: two lanes fused into one column are not
    // two participants any more; an activation bar lives right of its lane
    let mut lane: HashMap<&str, i32> = HashMap::new();
    for l in &seq.lifelines {
        let c = cx(l.x).clamp(1, cols as i32 - 2);
        if lane.values().any(|&o| o == c) {
            return Err(gate("two lanes fuse into one column", true));
        }
        lane.insert(l.id.as_str(), c);
    }
    // rows already spoken for; claim_rows slides by at most two
    let mut used: HashSet<i32> = HashSet::new();

    // lifelines first, under everything that speaks
    for l in &seq.lifelines {
        let c = lane[l.id.as_str()];
        for y in cy(l.y1) + 1..cy(l.y2) {
            g.put(c, y, vb, FRAME);
        }
    }

    // activation bars: one bar right of the lane it activates (a bar drawn
    // ON the lane would overwrite the lifeline, and a 10 px box has no
    // honest two-column reading at cell scale). Drawn at FRAME level so
    // later arrows may cross it, exactly as the SVG draws them across
    for a in &seq.activations {
        let c = *lane
            .get(a.participant.as_str())
            .ok_or_else(|| gate("activation for an unknown participant", false))?;
        let col = c + 1 + a.depth.min(2) as i32;
        if lane.values().any(|&l| l == col) || col >= cols as i32 - 1 {
            return Err(gate("no column free beside the lane for an activation", true));
        }
        for y in cy(a.y)..=cy(a.y + a.height) {
            if g.get(col, y).is_some_and(|cell| cell.ch == bar) {
                return Err(gate("two activation bars fuse into one column", true));
            }
            g.put(col, y, bar, FRAME);
        }
    }



    // 1. frames first (FRAME level: actors and messages win every cell):
    // walls claim their rows, so arrows inside can never crowd a wall out
    struct FrameRows {
        x0: i32,
        x1: i32,
        y0: i32,
        y1: i32,
        lines: Vec<i32>,
    }
    let mut frame_rows: Vec<(usize, FrameRows)> = Vec::new();
    for (fi, f) in seq.frames.iter().enumerate() {
        let (x0, x1) = (cx(f.x), cx(f.x + f.width));
        if x1 - x0 < 3 {
            return Err(gate("a frame this narrow cannot hold its walls", true));
        }
        let want_h = cy(f.y + f.height) - cy(f.y);
        if want_h < 3 {
            return Err(gate("a frame this short cannot hold its sections", true));
        }
        let y0 = claim_rows(cy(f.y), 1, &mut used, rows)
            .ok_or_else(|| gate("a frame wall fuses into a used row", true))?;
        let y1 = claim_rows(cy(f.y + f.height), 1, &mut used, rows)
            .ok_or_else(|| gate("a frame wall fuses into a used row", true))?;
        if y1 - y0 < 3 {
            return Err(gate("a frame this short cannot hold its sections", true));
        }
        for x in x0 + 1..x1 {
            g.put(x, y0, hb, FRAME);
            g.put(x, y1, hb, FRAME);
        }
        for y in y0 + 1..y1 {
            g.put(x0, y, vb, FRAME);
            g.put(x1, y, vb, FRAME);
        }
        g.put(x0, y0, tl, FRAME);
        g.put(x1, y0, tr, FRAME);
        g.put(x0, y1, bl, FRAME);
        g.put(x1, y1, br, FRAME);
        frame_rows.push((fi, FrameRows { x0, x1, y0, y1, lines: Vec::new() }));
    }
    // labels and dividers after the walls exist (labels ride walls; the
    // SVG label pentagon punches the wall, TEXT over FRAME is the same word)
    let mut last_label_end: HashMap<(usize, i32), i32> = HashMap::new();
    for (fi, fr) in &mut frame_rows {
        let f = &seq.frames[*fi];
        // dividers claim rows FIRST so labels can snap to where the line
        // actually ended up after any slide
        for d in &f.dividers {
            let dy = claim_rows(cy(*d), 1, &mut used, rows)
                .ok_or_else(|| gate("a frame divider fuses into a used row", true))?;
            if dy <= fr.y0 || dy >= fr.y1 {
                return Err(gate("a frame divider escapes its frame", true));
            }
            for x in fr.x0 + 1..fr.x1 {
                g.put(x, dy, hb, FRAME);
            }
            g.put(fr.x0, dy, t_r, FRAME);
            g.put(fr.x1, dy, t_l, FRAME);
            fr.lines.push(dy);
        }
        let keyword = std::iter::once(&f.label).chain(f.section_labels.iter());
        for sl in keyword {
            if sl.text.lines.len() > 1 {
                return Err(gate("multi-line frame label is not drawn", false));
            }
            if let Some(line) = sl.text.lines.first().filter(|l| !l.trim().is_empty()) {
                if !printable(line) {
                    return Err(gate("frame label has control characters", false));
                }
                let w = visible_width(line) as i32;
                let x = cx(sl.x).max(fr.x0 + 1).min(cols as i32 - 2);
                // the text rides the top wall, the bottom wall or a divider;
                // SVG hangs it a few px off the line, so snap within one row
                let want = cy(sl.y);
                let wall = std::iter::once(fr.y0)
                    .chain(std::iter::once(fr.y1))
                    .chain(fr.lines.iter().copied())
                    .filter(|&r| (r - want).abs() <= 1)
                    .min_by_key(|&r| (r - want).abs());
                let Some(y) = wall else {
                    return Err(gate("frame label is not on a wall or divider", false));
                };
                // the keyword and its section label share one pentagon in
                // the SVG ("opt [debug]"): a later label on the same line
                // continues where the last one ended, never overwrites it
                let x = last_label_end.get(&(*fi, y)).map_or(x, |&end| x.max(end + 1));
                g.text(x, y, line, TEXT);
                placed.push((x, y, line));
                last_label_end.insert((*fi, y), x + w);
            }
        }
    }

    // 2. actor boxes at head and foot (NODE level over frame walls)
    for n in layout.nodes.values().chain(seq.footboxes.iter()) {
        let n_rows = (cy(n.y + n.height) - cy(n.y) + 1).max(2);
        let mut band = None;
        for off in [0, -1, 1, -2, 2] {
            let y0 = cy(n.y) + off;
            if y0 < 0 || y0 + n_rows > rows as i32 {
                continue;
            }
            let r = Rect {
                x0: cx(n.x),
                y0,
                x1: cx(n.x + n.width),
                y1: y0 + n_rows - 1,
            };
            if band_clear(&g, r) {
                band = Some(r);
                break;
            }
        }
        let r = band.ok_or_else(|| gate("an actor box lands on drawn art", true))?;
        if r.x1 - r.x0 < 4 || r.y1 - r.y0 < 2 {
            return Err(gate("an actor box this thin cannot hold its name", true));
        }
        put_label_box(&mut g, r, &n.label.lines, (tl, tr, bl, br, hb, vb), &mut placed, false)?;
    }

    // 4. messages: one row each, in source order, never merged
    for e in &layout.edges {
        let (Some(ca), Some(cb)) = (
            lane.get(e.from.as_str()).copied(),
            lane.get(e.to.as_str()).copied(),
        ) else {
            return Err(gate("message to an unknown participant", false));
        };
        if e.points.len() < 2 {
            return Err(gate("message without endpoints", false));
        }
        if e.arrow_start {
            return Err(gate("an arrowhead at the source end is not drawn", false));
        }
        let dotted = matches!(e.style, mermaid_rs_renderer::ir::EdgeStyle::Dotted)
            || e.override_style.dasharray.is_some();
        let has_label = e
            .label
            .as_ref()
            .and_then(|l| l.lines.first())
            .is_some_and(|l| !l.trim().is_empty());
        if let Some(lb) = &e.label {
            if lb.lines.iter().any(|l| !printable(l)) {
                return Err(gate("message label has control characters", false));
            }
            if lb.lines.len() > 1 {
                return Err(gate("multi-line message label is not drawn", false));
            }
        }
        // the arrow keeps (or slides to) one exact row; its label then
        // floats above it, SVG-style, choosing any clear row nearby
        let mr = claim_rows(cy(e.points[0].1), 1, &mut used, rows)
            .ok_or_else(|| gate("two messages fuse into one row", true))?;
        if has_label {
            let line = e.label.as_ref().unwrap().lines.first().unwrap();
            let w = visible_width(line) as i32;
            let ax = e.label_anchor.map(|(x, _)| cx(x)).unwrap_or((ca + cb) / 2);
            let lx = (ax - w / 2).max(1).min((cols as i32 - 1 - w).max(1));
            // the SVG label field punches through lane lines - lanes may
            // sit under it, anything already spoken for may not; a label
            // may rise or sink one row (SVG hangs it loosely), the arrow
            // it names never moves
            let mut ly = None;
            for r in [mr - 1, mr - 2, mr + 1] {
                if r <= 0 || r >= rows as i32 || used.contains(&r) {
                    continue;
                }
                if (lx..lx + w).all(|x| match g.get(x, r) {
                    None => false,
                    Some(c) => c.owner <= FRAME,
                }) {
                    ly = Some(r);
                    break;
                }
            }
            let ly = ly.ok_or_else(|| gate("a message label lands on drawn art", true))?;
            used.insert(ly);
            g.text(lx, ly, line, TEXT);
            placed.push((lx, ly, line));
        }
        if ca == cb {
            // self message: the 4-point stub loop right of the lane
            if e.points.len() < 4 {
                return Err(gate("self message route not understood", false));
            }
            let r1 = claim_rows(cy(e.points[3].1), 1, &mut used, rows)
                .ok_or_else(|| gate("self message return fuses into another row", true))?;
            if r1 <= mr {
                return Err(gate("self message return rides above its start", true));
            }
            let s = cx(e.points[1].0).max(ca + 2).min(cols as i32 - 2);
            if s <= ca + 1 {
                return Err(gate("no room beside the lane for a self loop", true));
            }

            g.put(ca, mr, t_r, EDGE);
            for x in ca + 1..s {
                g.put(x, mr, dash(dotted, plain), EDGE);
                g.put(x, r1, dash(dotted, plain), EDGE);
            }
            g.put(s, mr, tr, EDGE);
            g.put(s, r1, br, EDGE);
            for y in mr + 1..r1 {
                g.put(s, y, vb, EDGE);
            }
            if e.arrow_end {
                g.put(ca, r1, head_l, TEXT);
            } else {
                g.put(ca, r1, t_l, EDGE);
            }
        } else {
            let dir = if cb > ca { 1 } else { -1 };
            g.put(ca, mr, if dir > 0 { t_r } else { t_l }, EDGE);
            let mut x = ca + dir;
            while x != cb {
                if lane.values().any(|&l| l == x) {
                    g.put(x, mr, cross, EDGE); // crossing a foreign lane: both lines survive
                } else {
                    g.put(x, mr, dash(dotted, plain), EDGE);
                }
                x += dir;
            }
            if e.arrow_end {
                g.put(cb, mr, if dir > 0 { head_r } else { head_l }, TEXT);
            } else {
                g.put(cb, mr, if dir > 0 { t_l } else { t_r }, EDGE);
            }
        }
    }

    // 5. notes over: a boxed label across its participants
    for nt in &seq.notes {
        let n_rows = (cy(nt.y + nt.height) - cy(nt.y) + 1).max(2);
        let mut band = None;
        for off in [0, -1, 1, -2, 2] {
            let y0 = cy(nt.y) + off;
            if y0 < 0 || y0 + n_rows > rows as i32 {
                continue;
            }
            let r = Rect {
                x0: cx(nt.x),
                y0,
                x1: cx(nt.x + nt.width),
                y1: y0 + n_rows - 1,
            };
            if band_clear(&g, r) {
                band = Some(r);
                break;
            }
        }
        let r = band.ok_or_else(|| gate("a note lands on drawn art", true))?;
        if r.x1 - r.x0 < 4 || r.y1 - r.y0 < 2 {
            return Err(gate("a note this thin cannot hold its text", true));
        }
        put_label_box(&mut g, r, &nt.label.lines, (tl, tr, bl, br, hb, vb), &mut placed, false)?;
    }

    check_survival(&g, &placed)?;
    emit(g, plain)
}

/// a pie is two numbers per slice and a name: the honest text form is the
/// list itself (title, label, share). Percentages show alongside the raw
/// value so rounding that does not add to 100 cannot mislead.
fn pie_list(layout: &Layout, cols: usize) -> Result<Vec<String>, Fail> {
    let DiagramData::Pie(pie) = &layout.diagram else {
        return Err(gate("pie without pie data", false));
    };
    if pie.slices.is_empty() || pie.slices.len() != pie.legend.len() {
        return Err(gate("pie data shape not understood", false));
    }
    let total: f64 = pie.slices.iter().map(|v| v.value as f64).sum();
    if !total.is_finite() || total <= 0.0 {
        return Err(gate("pie total is zero", false));
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(t) = &pie.title {
        if t.text.lines.len() > 1 {
            return Err(gate("multi-line pie title is not drawn", false));
        }
        if let Some(title) = t.text.lines.first().map(|l| l.trim()) {
            if !title.is_empty() {
                if !printable(title) || visible_width(title) > cols {
                    return Err(gate("pie title is not drawable", false));
                }
                out.push(title.to_string());
                out.push(String::new());
            }
        }
    }
    for (sl, lg) in pie.slices.iter().zip(&pie.legend) {
        // the legend label is what mermaid shows (including [value] with
        // showData); the raw value column keeps rounded shares honest
        if sl.value as f64 / total * 100.0 > 100.5 || lg.label.lines.len() > 1 {
            return Err(gate("pie entry not drawable as text", false));
        }
        let label = lg.label.lines.join(" ");
        let label = label.trim();
        if !printable(label) || label.is_empty() {
            return Err(gate("pie label is not drawable", false));
        }
        let pct = sl.value as f64 / total * 100.0;
        let line = format!("  {label}  {:.1}%", pct);
        if visible_width(&line) > cols {
            return Err(gate("a pie label is too wide", false));
        }
        out.push(line);
    }
    if out.len() > MAX_ROWS {
        return Err(gate("pie legend longer than any window", false));
    }
    Ok(out)
}
