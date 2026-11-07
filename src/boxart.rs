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
use mermaid_rs_renderer::{
    parse_mermaid_strict, DiagramKind, EdgeDecoration, LayoutConfig, NodeShape, Theme,
};

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
    match raster(
        text,
        &layout,
        width_cols(&layout),
        height_rows(&layout),
        plain,
        max_cols,
    ) {
        Ok(art) => Outcome::Art(art),
        // boxes/routes collided at cell scale: give dagre more room once
        Err(Fail::Retry) if tried_default => match pick(V_ROOMY) {
            Pick::Draw(l) => {
                match raster(text, &l, width_cols(&l), height_rows(&l), plain, max_cols) {
                    Ok(art) => Outcome::Art(art),
                    Err(_) => Outcome::Source,
                }
            }
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
    let extra = if layout.kind == DiagramKind::Er {
        layout.edges.len() + 1
    } else {
        0
    };
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
    if built.is_none() {
        gate("the upstream parser or layout refused first", false);
    }
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
            let stale: Vec<String> = c
                .iter()
                .filter(|(_, e)| e.gen != gen)
                .map(|(k, _)| k.clone())
                .collect();
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
        && l.nodes
            .values()
            .all(|n| ok(n.x) && ok(n.y) && ok(n.width) && ok(n.height))
        && l.edges.iter().all(|e| {
            e.points.iter().all(|p| {
                p.0.is_finite()
                    && p.1.is_finite()
                    && p.0.abs() <= 100_000.0
                    && p.1.abs() <= 100_000.0
            })
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
const EDGE: u8 = 2; // edge line cell
const NODE: u8 = 3; // box border or erased interior
const TEXT: u8 = 4; // any glyph that is content: node/edge label, arrowhead

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
        let parallel = if horiz {
            w[0].0 != w[1].0
        } else {
            w[0].1 != w[1].1
        };
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

fn raster(
    src: &str,
    layout: &Layout,
    cols: usize,
    rows: usize,
    plain: bool,
    win: usize,
) -> Result<Vec<String>, Fail> {
    // every kind enters through this one door: probe, next_warm and
    // terminal_ascii walk the same ladder for all of them, so no kind can
    // lie to the pager's cache probe about what a real run would draw
    match layout.kind {
        DiagramKind::Flowchart | DiagramKind::State | DiagramKind::Er => {
            graph_raster(src, layout, cols, rows, plain, win)
        }
        DiagramKind::Sequence => sequence_raster(src, layout, cols, rows, plain),
        DiagramKind::Pie => pie_list(src, layout, cols.min(win)),
        _ => Err(gate("kind has no terminal art", false)),
    }
}

/// the node id at the head of an arrow-statement segment: an optional
/// `|link text|` first, then the bare word before any label bracket
fn graph_seg_id(seg: &str) -> Option<&str> {
    let mut t = seg.trim_start();
    if let Some(r) = t.strip_prefix('|') {
        t = r.split_once('|').map(|(_, r)| r).unwrap_or("");
    }
    let t = t.trim_start();
    let end = t
        .find([' ', '\t', '[', '(', '{', '"', '`', ':'])
        .unwrap_or(t.len());
    let id = t[..end].trim();
    (!id.is_empty()).then_some(id)
}

/// Every flowchart link form mermaid knows, longest match first at each
/// offset (review #3e, 1); the `x`/`o` ends live on flow_stmt's refuse
/// list because flat art cannot show the cross or circle marker.
const FLOW_LINKS: [&str; 20] = [
    "-.->", "<==>", "<-->", "==>", "-->", "---", "-..-", "-.-", "~~~", "===", "<--", "<-.", ".->",
    "o--", "x--", "--x", "--o", "==", "--", "-.",
];

fn flow_link_at(s: &str) -> Option<(&'static str, &str)> {
    FLOW_LINKS
        .iter()
        .find_map(|l| s.strip_prefix(l).map(|r| (*l, r)))
}

fn flow_link_ahead(s: &str) -> Option<(&'static str, &str)> {
    (0..s.len())
        .filter(|i| s.is_char_boundary(*i))
        .find_map(|i| flow_link_at(&s[i..]))
}

/// the id at a statement position: a bare word or a `"quoted name"`
fn flow_id(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('"') {
        let e = r.find('"')?;
        return Some((&r[..e], &r[e + 1..]));
    }
    let e = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(s.len());
    if e == 0 {
        return None;
    }
    Some((&s[..e], &s[e..]))
}

fn flow_group_end(s: &str, close: char) -> Option<&str> {
    let mut quoted = false;
    for (idx, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c == close && !quoted => return Some(&s[idx..]),
            _ => {}
        }
    }
    None
}

/// shape and class suffixes after an id: `[x] ((x)) {x} >x] @{..} :::cls`
fn flow_shape_tail(s: &str) -> Option<(&str, Option<&str>)> {
    let mut s = s;
    let mut label = None;
    loop {
        s = s.trim_start();
        if let Some(r) = s.strip_prefix(":::") {
            let e = r
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(r.len());
            s = &r[e..];
        } else if s.starts_with('@') {
            // node metadata `@{ label: .. shape: .. }`: the crate prints it
            // as a separate box or ignores it, never as the label/shape
            // mermaid gives the node (review #3f, 2)
            return None;
        } else {
            let (open_c, close_c) = match s.chars().next() {
                Some('[') => ('[', ']'),
                Some('(') => ('(', ')'),
                Some('{') => ('{', '}'),
                Some('>') => ('>', ']'),
                _ => return Some((s.trim_start(), label)),
            };
            let r = &s[open_c.len_utf8()..];
            let dbl = open_c != '>' && r.starts_with(open_c);
            let r = if dbl { &r[open_c.len_utf8()..] } else { r };
            let closed = flow_group_end(r, close_c)?;
            label = Some(&r[..r.len() - closed.len()]);
            if dbl && open_c == '(' && !r[..r.len() - closed.len()].trim().is_empty() {
                // a `((label))` circle rasterizes to a bare dot: the
                // label's written words would vanish from the picture
                return None;
            }
            let closed = closed.strip_prefix(close_c)?;
            s = if dbl {
                closed.strip_prefix(close_c).unwrap_or(closed)
            } else {
                closed
            };
        }
        s = s.trim_start();
    }
}

/// words of a keyword line, keeping `"quoted strings"` and `(call args)`
/// whole; None on an unterminated quote or paren
fn flow_words(s: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let (mut quoted, mut depth) = (false, 0i32);
        while i < b.len() && (quoted || depth > 0 || !b[i].is_ascii_whitespace()) {
            match b[i] {
                b'"' => quoted = !quoted,
                b'(' if !quoted => depth += 1,
                b')' if !quoted => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        if quoted || depth != 0 {
            return None;
        }
        out.push(&s[start..i]);
    }
    Some(out)
}

/// a keyword line mermaid would accept with nothing left over (review
/// #3i, 3): the crate SKIPS these lines whole, so any word mermaid would
/// reject — `click A --> X`, `class A foo X` — vanishes from the art
fn flow_keyword_line_ok(head: &str, line: &str) -> bool {
    let mut unquoted: String = line.split('"').step_by(2).collect();
    if head == "subgraph" {
        // a `[title]` is free text: `subgraph S [Has --> arrow]` is valid
        if let (Some(a), Some(b)) = (unquoted.find('['), unquoted.rfind(']')) {
            if a < b {
                unquoted.replace_range(a..=b, "");
            }
        }
    }
    let linky = ["--", "==", "-.", "~~~"]
        .iter()
        .any(|t| unquoted.contains(t));
    let Some(w) = flow_words(line) else {
        return false;
    };
    let target = |t: &str| matches!(t, "_self" | "_blank" | "_parent" | "_top");
    let quoted = |t: &str| t.len() >= 2 && t.starts_with('"') && t.ends_with('"');
    match head {
        "flowchart" | "graph" => true,
        "end" => w.len() == 1,
        "direction" => w.len() == 2 && matches!(w[1], "TB" | "TD" | "BT" | "RL" | "LR"),
        // `class A&B foo` is valid, but a shape, quote or `:::` in it is
        // a parse error whose words the art would drop (review #3j, 2)
        "class" => {
            !linky
                && w.len() == 3
                && !line.contains(":::")
                && !line.contains(['[', ']', '(', ')', '{', '}', '<', '>', '|', '"', '@'])
        }
        "click" => {
            if linky || w.len() < 3 {
                return false;
            }
            // click ID (href URL | URL | call f(..) | callback) ["tip"] [target]
            let rest = &w[2..];
            let (rest, url_like) = match rest {
                ["href", url, r @ ..] if quoted(url) => (r, true),
                ["call", f, r @ ..] if f.contains('(') => (r, false),
                [url, r @ ..] if quoted(url) => (r, true),
                [cb, r @ ..] if !cb.starts_with('"') => (r, false),
                _ => return false,
            };
            match rest {
                [] => true,
                [tip] => quoted(tip) || (url_like && target(tip)),
                [tip, t] => quoted(tip) && url_like && target(t),
                _ => false,
            }
        }
        // a style tail takes only `key:value,..` tokens (`#`, `%`, units,
        // spaces): a shape, quote, `&`, `@`, `:::` or `-` word is a parse
        // error, and the crate would drop `C[Box]` silently (review #3j, 2)
        "classDef" | "style" | "linkStyle" => {
            !linky
                && !line.contains(":::")
                && !line.contains(['[', ']', '(', ')', '{', '}', '<', '>', '|', '&', '"', '@'])
                && !w.iter().any(|t| t.starts_with('-'))
        }
        // a subgraph line's `[title]` and quoted id carry free text
        _ => !linky,
    }
}

/// state diagrams draw only from an ALLOWLIST of exact line forms
/// (review #3n): the crate skips or reinterprets everything else — note
/// blocks become states, `Moving:::cls` an edge label `::cls`, a bare
/// `Foo` or `end` line a state it then drops, `A : goes --> nowhere` two
/// boxes and an edge. Anything outside these forms refuses:
///   `stateDiagram[-v2]`, `direction XX`, `hide empty description`,
///   `ID --> ID[ : label]` (no arrow-like text in the label), `state ID
///   {` (never empty), `state "Desc" as ID[ {]` (once per ID), a bare
///   `state ID`, `}`, `classDef`/`class` lines within the flowchart
///   keyword grammar; no line holds a `\`.
/// An ID is `[*]` or word characters, never a keyword; an aliased or bare
/// `state` ID must also take part in a transition, and an aliased box
/// must read exactly as written. `ID : description` refuses: mermaid
/// shows the description WITHOUT the id the crate prints above it.
fn state_grammar(src: &str, layout: &Layout) -> Result<(), Fail> {
    const KEYWORDS: [&str; 10] = [
        "state",
        "end",
        "direction",
        "hide",
        "note",
        "class",
        "classDef",
        "click",
        "style",
        "as",
    ];
    let id_ok = |t: &str| {
        t == "[*]"
            || (!t.is_empty()
                && t.chars().all(|c| c.is_alphanumeric() || c == '_')
                && !KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(t))
                && !t.to_ascii_lowercase().starts_with("statediagram"))
    };
    let refuse = || gate("a state line outside the drawable grammar", false);
    let ls: Vec<&str> = src.lines().collect();
    // a closed frontmatter fence holds YAML (the title gate read it)
    let fm_end = if ls.first().is_some_and(|l| l.trim() == "---") {
        ls[1..]
            .iter()
            .position(|l| matches!(l.trim(), "---" | "..."))
            .map(|e| e + 1)
    } else {
        None
    };
    let mut in_transition: Vec<&str> = Vec::new();
    // per open `state X {` block: has it any statement yet? an empty one
    // is a mermaid render error the art would silently drop (#3o, 5)
    let mut filled: Vec<bool> = Vec::new();
    let mut aliased: Vec<(&str, &str)> = Vec::new();
    let mut declared: Vec<&str> = Vec::new();
    for (no, raw) in ls.iter().enumerate() {
        if fm_end.is_some_and(|e| no <= e) {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if line == "}" {
            if filled.pop() != Some(true) {
                return Err(refuse());
            }
            continue;
        }
        if matches!(
            line,
            "stateDiagram" | "stateDiagram-v2" | "hide empty description"
        ) {
            continue;
        }
        if let Some(f) = filled.last_mut() {
            *f = true;
        }
        // the crate turns a literal `\n` into a line break mermaid never
        // draws (review #3o, 2)
        if line.contains('\\') {
            return Err(refuse());
        }
        let w: Vec<&str> = line.split_whitespace().collect();
        match w[0] {
            "direction" => {
                if !(w.len() == 2 && matches!(w[1], "TB" | "TD" | "BT" | "LR" | "RL")) {
                    return Err(refuse());
                }
                continue;
            }
            "classDef" | "class" => {
                if line.contains(":::") || !flow_keyword_line_ok(w[0], line) {
                    return Err(refuse());
                }
                continue;
            }
            "state" => {
                let rest = line["state".len()..].trim();
                let (body, block) = match rest.strip_suffix('{') {
                    Some(b) => (b.trim(), true),
                    None => (rest, false),
                };
                if let Some(q) = body.strip_prefix('"') {
                    // state "Desc" as ID
                    let Some((desc, tail)) = q.split_once('"') else {
                        return Err(refuse());
                    };
                    let t: Vec<&str> = tail.split_whitespace().collect();
                    if desc.trim().is_empty() || t.len() != 2 || t[0] != "as" || !id_ok(t[1]) {
                        return Err(refuse());
                    }
                    if aliased.iter().any(|(i, _)| *i == t[1]) {
                        return Err(refuse());
                    }
                    aliased.push((t[1], desc));
                } else if !(id_ok(body) && body != "[*]") {
                    return Err(refuse());
                } else if !block {
                    // a bare `state Foo` draws nothing in mermaid unless a
                    // transition names Foo too (review #3n, 9)
                    declared.push(body);
                }
                if block {
                    filled.push(false);
                }
                continue;
            }
            _ => {}
        }
        if line.contains(":::") {
            return Err(refuse());
        }
        if let Some((l, r)) = line.split_once("-->") {
            let (to, label) = match r.split_once(':') {
                Some((t, lab)) => (t.trim(), Some(lab.trim())),
                None => (r.trim(), None),
            };
            let (from, to) = (l.trim(), to);
            // the crate tries its arrow tokens in LIST order, not by
            // position: `A --> B : done <-- ok` split at `<--` into two
            // invented boxes (review #3o, 1). No arrow-like text in labels
            let arrowish = |l: &str| {
                ["--", "<-", "->", "==", "~~", "-."]
                    .iter()
                    .any(|t| l.contains(t))
            };
            if !id_ok(from) || !id_ok(to) || label.is_some_and(|l| l.is_empty() || arrowish(l)) {
                return Err(refuse());
            }
            in_transition.push(from);
            in_transition.push(to);
            continue;
        }
        // `ID : description` refuses: mermaid shows the description
        // ALONE where the crate titles it with the id, and on a composite
        // the crate drops it (review #3o, 3/4)
        return Err(refuse());
    }
    if !filled.is_empty() {
        return Err(refuse());
    }
    for id in aliased.iter().map(|(i, _)| i).chain(declared.iter()) {
        if !in_transition.contains(id) {
            return Err(refuse());
        }
    }
    // an aliased box shows the alias exactly as written
    for (id, name) in &aliased {
        let want = [name.trim()];
        let drawn: Option<Vec<&str>> = layout
            .nodes
            .get(*id)
            .map(|n| n.label.lines.iter().map(|l| l.trim()).collect());
        if drawn.as_deref() != Some(&want[..]) {
            return Err(gate("a state label the source never wrote", true));
        }
    }
    Ok(())
}

/// the id a `subgraph` line declares: `subgraph s1 [Title]` -> `s1`, and
/// `subgraph "s 2"` -> `s 2` (mermaid unquotes it, so a bare `s2` line
/// names it, review #3h, 1)
fn flow_subgraph_id(line: &str) -> &str {
    let t = line["subgraph".len()..].trim_start();
    if let Some(r) = t.strip_prefix('"') {
        return r.split('"').next().unwrap_or("");
    }
    t.split(|c: char| c.is_whitespace() || c == '[')
        .next()
        .unwrap_or("")
}

/// ids named by a statement, the links it writes as endpoint pairs, and
/// the label text written inside each id's shape
type FlowStmt<'a> = (
    Vec<&'a str>,
    Vec<(&'a str, &'a str)>,
    Vec<(&'a str, &'a str)>,
);

/// the title a `subgraph` line shows: `[Title]` after the id, the quoted
/// id itself, the bare id, or (`subgraph a b`) the whole text
fn flow_subgraph_title(line: &str) -> &str {
    let t = line["subgraph".len()..].trim();
    if t.starts_with('"') {
        return t;
    }
    let id = flow_subgraph_id(line);
    let rest = t[id.len()..].trim();
    match rest.strip_prefix('[') {
        Some(r) => r.strip_suffix(']').unwrap_or(r),
        None if rest.is_empty() => id,
        None => t,
    }
}

/// One flowchart statement, fully tokenized (review #3e, 1/4): id members
/// (shapes, `@{}`, `:::` classes) separated by link tokens, `&` fans and
/// the text forms `-- t -->`, `-. t .->`, `== t ==>`. Any word the
/// tokenizer cannot place — `A --> B C --> D`, `A[a] B[b]`, `A x--> "b c"`
/// — or an `x`/`o` arrow end returns None, and the diagram refuses: a
/// statement mermaid rejects may not draw as confident art that drops
/// written words.
fn flow_stmt(line: &str) -> Option<FlowStmt<'_>> {
    let mut ids: Vec<&str> = Vec::new();
    let mut labels: Vec<(&str, &str)> = Vec::new();
    // ids between links form groups; every adjacent group pair is linked
    // crosswise, which is exactly what `A & B --> C & D` means
    let mut groups: Vec<Vec<&str>> = vec![Vec::new()];
    // groups whose incoming link is invisible
    let mut hidden: Vec<usize> = Vec::new();
    let mut rest = line;
    let mut need_id = true;
    loop {
        let t = rest.trim_start();
        if need_id {
            let (id, r) = flow_id(t)?;
            ids.push(id);
            groups.last_mut().unwrap().push(id);
            let (r, label) = flow_shape_tail(r)?;
            if let Some(l) = label {
                labels.push((id, l));
            }
            rest = r;
            need_id = false;
            continue;
        }
        if t.is_empty() {
            let mut pairs: Vec<(&str, &str)> = Vec::new();
            for (k, w) in groups.windows(2).enumerate() {
                if hidden.contains(&(k + 1)) {
                    continue;
                }
                for a in &w[0] {
                    for b in &w[1] {
                        pairs.push((*a, *b));
                    }
                }
            }
            return Some((ids, pairs, labels));
        }
        if let Some(r) = t.strip_prefix('&') {
            rest = r;
            need_id = true;
            continue;
        }
        // an edge id may stand before the arrow: `A e1@--> B` (#3e note,
        // mermaid 11 syntax); it labels nothing, so nothing is demanded
        let t2 = match t.find('@') {
            Some(at)
                if at > 0
                    && t[..at]
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == '.') =>
            {
                &t[at + 1..]
            }
            _ => t,
        };
        let (tok, r) = flow_link_at(t2)?;
        if matches!(tok, "o--" | "x--" | "--x" | "--o") {
            return None; // an arrow end the flat art would flatten away
        }
        // an invisible `~~~` link draws nothing in mermaid either: its
        // ends must still be nodes, but it demands no edge (#3f minor)
        if tok == "~~~" {
            hidden.push(groups.len());
        }
        groups.push(Vec::new()); // the next ids open the link's target group
                                 // `---->` is one arrow written with extra dashes
        let mut r = r.trim_start_matches(['-', '>', '.', '=']).trim_start();
        if matches!(tok, "--" | "-." | "==") {
            // a further link token turns this open form into the left
            // half of one TEXT link: `-- hi -->` is a single edge
            if let Some((term, after)) = flow_link_ahead(r) {
                let ok = matches!(
                    (tok, term),
                    ("--", "-->" | "---")
                        | ("-.", "-.->" | ".->" | "-.-" | "-..-")
                        | ("==", "==>" | "===")
                );
                if !ok {
                    return None;
                }
                // the group opened by the base IS this link's target
                // group; the terminator merely extends the same link
                r = after.trim_start_matches(['-', '>', '.', '=']).trim_start();
                if r.starts_with('|') {
                    // `A -- t -->|u| B`: mermaid rejects it, the crate keeps
                    // `u` and drops `t` (review #3f, 4)
                    return None;
                }
            }
        }
        if let Some(rr) = r.strip_prefix('|') {
            // the label may quote pipes: -->|"a|b"|
            let mut quoted = false;
            let mut done = None;
            for (idx, c) in rr.char_indices() {
                match c {
                    '"' => quoted = !quoted,
                    '|' if !quoted => {
                        done = Some(idx);
                        break;
                    }
                    _ => {}
                }
            }
            r = &rr[done? + 1..];
        }
        rest = r;
        need_id = true;
    }
}

/// decode YAML-style `\xHH` / `\uHHHH` escapes the way a frontmatter
/// parser resolves them before matching a key (review #3e, 2)
fn decode_escapes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let n = match it.peek() {
            Some('x') => 2,
            Some('u') => 4,
            Some('U') => 8,
            _ => {
                out.push(c);
                continue;
            }
        };
        it.next();
        let hex: String = it.by_ref().take(n).collect();
        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
            Some(d) => out.push(d),
            None => {
                out.push('\\');
                out.push_str(&hex);
            }
        }
    }
    out
}

/// the line's first word after escape decoding and after any leading
/// quote, brace, question mark or other punctuation — frontmatter can
/// spell a title `"title":`, `{title:`, `? title` or `"ti\\x74le":`
fn head_word(line: &str) -> String {
    let d = decode_escapes(line);
    let t = d.trim_start();
    let rest = t.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_');
    let e = rest
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    rest[..e].to_ascii_lowercase()
}

fn graph_raster(
    src: &str,
    layout: &Layout,
    cols: usize,
    rows: usize,
    plain: bool,
    win: usize,
) -> Result<Vec<String>, Fail> {
    let kind = layout.kind;
    // statement honesty for the graph kinds (review #3d, 4/6): mermaid
    // splits on `;` and comments from any mid-line `%%`, the crate does
    // neither, so `A[hi %% x]` truncates and `A -->|a;b| B` splits a
    // label into two nodes; and a frontmatter `title:` names a caption
    // the graph art has no room to draw, so it must not silently vanish
    for raw in src.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if line.contains(';') {
            return Err(gate(
                "a semicolon splits statements the parser merges",
                false,
            ));
        }
        if line.contains("%%") {
            return Err(gate(
                "a mid-line comment truncates the label upstream",
                false,
            ));
        }
        if matches!(head_word(line).as_str(), "title" | "acctitle" | "accdescr") {
            return Err(gate("a title the art has no room for", false));
        }
    }
    // a leading frontmatter fence can hide a title in quoting styles,
    // braces, a YAML complex key or escapes (`"ti\x74le":` decodes to
    // `title` before a YAML parser matches the key); every caption word
    // inside a CLOSED block refuses, the art has no room to draw one
    // (review #3e, 2)
    {
        let ls: Vec<&str> = src.lines().collect();
        if ls.first().is_some_and(|l| l.trim() == "---") {
            if let Some(end) = ls[1..]
                .iter()
                .position(|l| matches!(l.trim(), "---" | "..."))
            {
                // escapes (`\x74`, `\U00000074`) decode before YAML
                // matches a key, so words are read decoded; and since a
                // `\`-escaped line break folds a quoted key across lines
                // (`? "ti\` / `tle"`), a fence holding ANY backslash is
                // read as ONE run of letters instead — an over-refusal,
                // never a hidden caption (review #3f, 5). Without one, a
                // config key like `titleTopMargin` is no caption.
                let text: String = ls[1..=end]
                    .iter()
                    .map(|l| decode_escapes(l) + "\n")
                    .collect::<String>()
                    .to_lowercase();
                let hit = if ls[1..=end].iter().any(|l| l.contains('\\')) {
                    let run: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
                    run.contains("title") || run.contains("accdescr")
                } else {
                    text.split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|w| matches!(w, "title" | "acctitle" | "accdescr"))
                };
                if hit {
                    return Err(gate("a title the art has no room for", false));
                }
            }
        }
    }
    // lost-piece audit for flowchart (review #3d, 5; #3e, 1/4): every
    // statement must tokenize under the FULL link grammar — open `---`,
    // dotted `-.-`, thick `===`, text forms `-- t -->`, `&` fans — and
    // any word the tokenizer cannot place refuses rather than trusting
    // a parser that swallows statement tails; every named id must then
    // survive in the layout and the edge count must cover every arrow
    if kind == DiagramKind::Flowchart {
        // mermaid's keywords match in THIS spelling only: `Click --> C`
        // is a node named Click (review #3h, 2)
        const SKIP: [&str; 10] = [
            "flowchart",
            "graph",
            "subgraph",
            "end",
            "direction",
            "class",
            "classDef",
            "style",
            "click",
            "linkStyle",
        ];
        let mut ids: Vec<&str> = Vec::new();
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        let mut labels: Vec<(&str, &str)> = Vec::new();
        let mut open: Vec<&str> = Vec::new();
        let mut nested: Vec<&str> = Vec::new();
        let mut styled: Vec<&str> = Vec::new();
        // a closed frontmatter block holds YAML keys, not statements —
        // `config: { theme: dark }` must keep drawing (#3e honest set)
        let ls: Vec<&str> = src.lines().collect();
        let fm_end = if ls.first().is_some_and(|l| l.trim() == "---") {
            ls[1..]
                .iter()
                .position(|l| matches!(l.trim(), "---" | "..."))
                .map(|e| e + 1)
        } else {
            None
        };
        // statement lines only: frontmatter and comments declare nothing
        let stmt = |no: usize, raw: &str| {
            let line = raw.trim();
            !(fm_end.is_some_and(|e| no <= e)
                || line.is_empty()
                || line.starts_with("%%")
                || line.starts_with("---"))
        };
        let declared: Vec<&str> = ls
            .iter()
            .enumerate()
            .filter(|(no, raw)| stmt(*no, raw))
            .map(|(_, raw)| raw.trim())
            .filter(|l| l.split_whitespace().next() == Some("subgraph"))
            .map(flow_subgraph_id)
            .collect();
        // mermaid MERGES two `subgraph s1` blocks into one cluster (first
        // title wins); the crate draws two frames (review #3i, 1)
        if declared
            .iter()
            .enumerate()
            .any(|(i, d)| declared[..i].contains(d))
        {
            return Err(gate("a subgraph declared twice", false));
        }
        // per open subgraph: has it any member yet? mermaid draws an EMPTY
        // subgraph as a box, the crate drops it (review #3i, 2)
        let mut filled: Vec<bool> = Vec::new();
        // each node's subgraph (by declaration number): mermaid gives a node
        // named in two subgraphs to the one that CLOSES first, which can
        // empty the other into a titled box the crate drops (review #3j, 1)
        let mut member_of: Vec<(&str, usize)> = Vec::new();
        let mut open_no: Vec<usize> = Vec::new();
        let mut opened = 0usize;
        // per subgraph (declaration number): its title and its parent's
        let mut sg_info: Vec<(&str, Option<usize>)> = Vec::new();
        for (no, raw) in ls.iter().enumerate() {
            if !stmt(no, raw) {
                continue;
            }
            let line = raw.trim();
            let head = line.split_whitespace().next().unwrap_or("");
            // a keyword in the wrong case is an ID to mermaid but a
            // keyword to the crate, which then drops it: refuse
            if !SKIP.contains(&head)
                && SKIP.iter().any(|k| {
                    k.eq_ignore_ascii_case(
                        head.split(['[', '(', '{', '@', ':']).next().unwrap_or(""),
                    )
                })
            {
                return Err(gate("a keyword spelled as an id", false));
            }
            if SKIP.contains(&head) && !flow_keyword_line_ok(head, line) {
                return Err(gate("a keyword line with words mermaid rejects", false));
            }
            // subgraph nesting: an edge landing on a subgraph INSIDE another
            // subgraph routes to strokes no element owns (review #3f, 6)
            if head == "subgraph" {
                let id = flow_subgraph_id(line);
                if !open.is_empty() {
                    nested.push(id);
                }
                if let Some(f) = filled.last_mut() {
                    *f = true;
                }
                open.push(id);
                filled.push(false);
                sg_info.push((flow_subgraph_title(line), open_no.last().copied()));
                open_no.push(opened);
                opened += 1;
                continue;
            }
            if head == "end" {
                open.pop();
                open_no.pop();
                if filled.pop() == Some(false) {
                    return Err(gate("an empty subgraph mermaid draws as a box", false));
                }
                continue;
            }
            if head == "style" {
                // mermaid draws a box for a `style` target nobody wrote
                // (and one literally named `A,B` for a list); the crate
                // draws neither (review #3g, 2)
                let target = line.split_whitespace().nth(1).unwrap_or("");
                if target.contains(',') {
                    return Err(gate("a style line mermaid draws as a box", false));
                }
                styled.push(target);
                continue;
            }
            if SKIP.contains(&head) {
                continue;
            }
            let Some((mut line_ids, line_pairs, line_labels)) = flow_stmt(line) else {
                return Err(gate("a flowchart statement the parser may misread", false));
            };
            // a subgraph id named inside another subgraph NESTS it there
            // in mermaid; the crate draws it beside its parent instead
            // (review #3g, 1)
            if !open.is_empty() && line_ids.iter().any(|i| declared.contains(i)) {
                return Err(gate("a subgraph nested by membership", true));
            }
            if let Some(f) = filled.last_mut() {
                *f = true;
            }
            if let Some(&k) = open_no.last() {
                for &i in &line_ids {
                    match member_of.iter().find(|(m, _)| *m == i) {
                        Some(&(_, j)) if j != k => {
                            return Err(gate("a node listed in two subgraphs", false));
                        }
                        Some(_) => {}
                        None => member_of.push((i, k)),
                    }
                }
            }
            ids.append(&mut line_ids);
            pairs.extend(line_pairs);
            labels.extend(line_labels);
        }
        // every written link with its TRUE endpoints must stand in the
        // layout: a self-loop counted but never drawn, or a dropped tail
        // relabelled as one longer edge, fails the multiset check
        // (review #3e, fuzz6)
        if layout.edges.len() != pairs.len() {
            return Err(gate("an arrow the renderer lost or invented", true));
        }
        let mut expect: Vec<(String, String)> = pairs
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let mut actual: Vec<(String, String)> = layout
            .edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        expect.sort();
        actual.sort();
        if expect != actual {
            return Err(gate("an edge the renderer moved", true));
        }
        if styled
            .iter()
            .any(|t| !ids.contains(t) && !declared.contains(t))
        {
            return Err(gate("a style line mermaid draws as a box", false));
        }
        if expect
            .iter()
            .any(|(a, b)| nested.contains(&a.as_str()) || nested.contains(&b.as_str()))
        {
            return Err(gate("an edge into a nested subgraph", true));
        }
        for id in &ids {
            if !layout.nodes.contains_key(*id) {
                return Err(gate("a node the renderer lost", true));
            }
        }
        // and the other way round: every box the art would draw must be
        // an id the source wrote — `click A call cb(arg)` makes the crate
        // invent a round node `arg` (review #3f, 3); a hidden node only
        // anchors a subgraph and draws no box
        if layout
            .nodes
            .values()
            .any(|n| !n.hidden && !ids.contains(&n.id.as_str()))
        {
            return Err(gate("a node the source never wrote", true));
        }
        // each box must say what the source wrote for it: the text inside
        // one of its shapes, or its bare id when it has none. The parser
        // swallows the rest of a line into a label (`Al[[Al]] ~~~ Al[Al]`
        // drew one box reading `[Al]] ~~~ Al[Al`, review #3f fuzz6b);
        // spacing, quotes, `<br>` and shape brackets compare loosely
        let norm = |t: &str| -> String {
            let t = t.replace("<br/>", "").replace("<br>", "");
            let t = t.trim_matches(|c: char| "[](){}/\\\"`>".contains(c) || c.is_whitespace());
            t.chars()
                .filter(|c| !c.is_whitespace() && *c != '"')
                .collect()
        };
        for n in layout.nodes.values().filter(|n| !n.hidden) {
            let drawn = norm(&n.label.lines.join(" "));
            // mermaid keeps the LAST label written for an id
            let written = labels
                .iter()
                .rev()
                .find(|(i, _)| *i == n.id.as_str())
                .map(|(_, l)| norm(l));
            let ok = match written {
                None => drawn == norm(&n.id),
                Some(w) => drawn == w,
            };
            if !ok {
                return Err(gate("a node label the source never wrote", true));
            }
        }
        // and each box must be DRAWN in the frames the source put it in:
        // inside exactly as many frames as its subgraph is deep, the
        // innermost titled as that subgraph. A node named only by a `~~~`
        // link inside a subgraph is placed outside it (review #3k, 1)
        let drawn: Vec<&mermaid_rs_renderer::layout::SubgraphLayout> = layout
            .subgraphs
            .iter()
            .filter(|sg| frame_rect(sg).is_some())
            .collect();
        for n in layout.nodes.values().filter(|n| !n.hidden) {
            let (mx, my) = (n.x + n.width / 2.0, n.y + n.height / 2.0);
            let mut around: Vec<&&mermaid_rs_renderer::layout::SubgraphLayout> = drawn
                .iter()
                .filter(|sg| {
                    sg.x < mx && mx < sg.x + sg.width && sg.y < my && my < sg.y + sg.height
                })
                .collect();
            around.sort_by(|a, b| (a.width * a.height).total_cmp(&(b.width * b.height)));
            // the titles of every subgraph around it, innermost first
            let mut chain: Vec<&str> = Vec::new();
            let mut at = member_of
                .iter()
                .find(|(m, _)| *m == n.id.as_str())
                .map(|&(_, k)| k);
            while let Some(k) = at {
                let Some(&(title, parent)) = sg_info.get(k) else {
                    break;
                };
                chain.push(title);
                at = parent;
            }
            // titles compare STRICTLY (one pair of quotes off, spacing
            // collapsed): a loose match let `[(cyl)]`, which mermaid
            // rejects, draw as a frame titled `cyl` (review #3l, 2)
            let strict = |t: &str| -> String {
                let t = t.trim();
                let t = t
                    .strip_prefix('"')
                    .and_then(|r| r.strip_suffix('"'))
                    .unwrap_or(t);
                t.split_whitespace().collect::<Vec<_>>().join(" ")
            };
            let ok = around.len() == chain.len()
                && around
                    .iter()
                    .zip(&chain)
                    .all(|(sg, t)| strict(&sg.label) == strict(t));
            if !ok {
                return Err(gate(
                    "a box drawn outside the subgraph it was written in",
                    true,
                ));
            }
        }
    }
    // lost-piece audit for state (review #3d, 5): the state grammar has
    // no open/dotted/text link forms, so the canonical arrow tokens are
    // enough to demand every named id and arrow survive the layout
    if kind == DiagramKind::State {
        state_grammar(src, layout)?;
        const CANON: [&str; 4] = ["-.->", "==>", "-->", "~~~"];
        let mut arrows = 0usize;
        let mut ids: Vec<&str> = Vec::new();
        for raw in src.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("%%") {
                continue;
            }
            // split on the canonical arrow tokens, non-overlapping
            let mut segs: Vec<&str> = Vec::new();
            let mut rest = line;
            'scan: while !rest.is_empty() {
                let mut best: Option<(usize, &str)> = None;
                for t in CANON {
                    if let Some(p) = rest.find(t) {
                        let better = match best {
                            None => true,
                            Some((bp, bt)) => p < bp || (p == bp && t.len() > bt.len()),
                        };
                        if better {
                            best = Some((p, t));
                        }
                    }
                }
                let Some((p, t)) = best else { break };
                segs.push(&rest[..p]);
                rest = &rest[p + t.len()..];
                continue 'scan;
            }
            let count = segs.len(); // arrows == splits
            if count == 0 {
                continue;
            }
            segs.push(rest);
            arrows += count;
            // ids live at segment heads; a fan-out `A & B --> C` puts
            // them elsewhere, so only the endpoints are safe to demand
            let id_of = graph_seg_id;
            if !line.contains('&') {
                if let Some(id) = id_of(segs[0]) {
                    ids.push(id);
                }
                for seg in segs[1..].iter().copied() {
                    if let Some(id) = id_of(seg) {
                        ids.push(id);
                    }
                }
            }
        }
        if layout.edges.len() < arrows {
            return Err(gate("an arrow the renderer lost", true));
        }
        for id in ids {
            if id == "[*]" {
                continue; // the start/end pseudo-state draws as a dot
            }
            if !layout.nodes.contains_key(id) {
                return Err(gate("a node the renderer lost", true));
            }
        }
    }
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
        return Err(gate(
            "er relations need crow's-foot ends, no self-links",
            false,
        ));
    }
    if kind == DiagramKind::Er {
        // the Relationships: list is one line per relation: an empty list
        // would draw boxes the source never related, and a name carrying
        // the line's own separators (space, colon) cannot be re-read
        if layout.edges.is_empty() {
            return Err(gate("erDiagram with no relations draws phantoms", false));
        }
        if layout.nodes.values().filter(|n| !n.hidden).any(|n| {
            n.label
                .lines
                .first()
                .map(|l| l.split_whitespace().count() > 1 || l.contains(':'))
                .unwrap_or(true)
        }) {
            return Err(gate("an entity name the list cannot show", false));
        }
        // the list prints the crate's decorations, which cannot hold a
        // malformed end: `|--o{` printed as `||--o{` ("exactly one"). Each
        // relation, in order, must print as the source wrote it (review
        // #3m, 4); word forms (`one or more`) refuse — over-refusal only
        const LEFT: [&str; 4] = ["|o", "||", "}o", "}|"];
        const RIGHT: [&str; 4] = ["o|", "||", "o{", "|{"];
        let mut rels: Vec<(&str, &str, &str, &str)> = Vec::new();
        let mut depth = 0i32;
        let mut fence = false;
        for raw in src.lines() {
            let line = raw.trim();
            if line == "---" {
                fence = !fence;
                continue;
            }
            if fence || line.is_empty() || line.starts_with("%%") {
                continue;
            }
            if depth > 0 {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                continue;
            }
            let bare: String = line.split('"').step_by(2).collect();
            if !(bare.contains("--") || bare.contains("..")) {
                depth += bare.matches('{').count() as i32 - bare.matches('}').count() as i32;
                continue;
            }
            // the token sits around the first `--`/`..`, spaced or not
            // (`A ||--o{B:v` is valid): 2 end chars, separator, 2 end chars
            let sep_at = match (line.find("--"), line.find("..")) {
                (Some(a), Some(b)) => a.min(b),
                (a, b) => a.or(b).unwrap_or(0),
            };
            let rel = (sep_at >= 2 && line.is_char_boundary(sep_at - 2))
                .then(|| line.get(sep_at - 2..sep_at + 4))
                .flatten()
                .filter(|t| {
                    LEFT.contains(&&t[..2])
                        && matches!(&t[2..4], "--" | "..")
                        && RIGHT.contains(&&t[4..])
                });
            let Some(tok) = rel else {
                return Err(gate("an ER relation the list may misprint", false));
            };
            let from = line[..sep_at - 2].trim();
            let (to, label) = line[sep_at + 4..].split_once(':').unwrap_or(("", ""));
            let (to, label) = (to.trim(), label.trim());
            // the label is ONE word or ONE quoted string: in `: places
            // order` mermaid labels `places` and draws an entity `order`
            // the list would drop (review #3n, 7)
            // (the list prints a quoted label verbatim, quotes and all)
            let ok = match label.strip_prefix('"') {
                Some(q) => q.strip_suffix('"').is_some_and(|t| !t.contains('"')),
                None => !label.is_empty() && !label.contains(char::is_whitespace),
            };
            let Some(label) = ok.then_some(label) else {
                return Err(gate("an ER relation the list may misprint", false));
            };
            if from.is_empty()
                || to.is_empty()
                || from.contains(char::is_whitespace)
                || to.contains(char::is_whitespace)
            {
                return Err(gate("an ER relation the list may misprint", false));
            }
            rels.push((from, tok, to, label));
        }
        if rels.len() != layout.edges.len() {
            return Err(gate("an ER relation the list may misprint", false));
        }
        for (e, (a, tok, b, label)) in layout.edges.iter().zip(&rels) {
            let sep = if matches!(e.style, mermaid_rs_renderer::ir::EdgeStyle::Dotted) {
                ".."
            } else {
                "--"
            };
            let printed = format!(
                "{}{}{}",
                er_card(e.start_decoration, true),
                sep,
                er_card(e.end_decoration, false)
            );
            let verb = e
                .label
                .as_ref()
                .map(|l| l.lines.join(" "))
                .unwrap_or_default();
            if e.from != *a || e.to != *b || printed != *tok || verb.trim() != label.trim() {
                return Err(gate("an ER relation the list may misprint", false));
            }
        }
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
            (
                n.id.as_str(),
                if cell {
                    Rect {
                        x0: (rect.x0 + rect.x1) / 2,
                        y0: (rect.y0 + rect.y1) / 2,
                        x1: (rect.x0 + rect.x1) / 2,
                        y1: (rect.y0 + rect.y1) / 2,
                    }
                } else {
                    rect
                },
            )
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
        // if the dot's px centre sits inside a composite, the dot belongs
        // to that composite: sliding it out of the frame would move the
        // membership the frame states (review #3, B7)
        // the dot's home is the innermost frame around it, not the first
        // listed: nested composites state membership through the small one
        let home_frame = frame_lines
            .iter()
            .filter(|f| f.has(r.x0, r.y0))
            .min_by_key(|f| (f.x1 - f.x0) * (f.y1 - f.y0));
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
                    || ((cand.x0 == f.x0 || cand.x0 == f.x1) && (f.y0..=f.y1).contains(&cand.y0))
            });
            if on_frame {
                continue;
            }
            if home_frame.is_some_and(|f| !f.has(cand.x0, cand.y0)) {
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
        // two frames whose walls share cells read as ONE frame: nested
        // subgraphs given the same rect drew one frame where the source
        // wrote two (review #3m, 1)
        let on_wall = |r: &Rect, x: i32, y: i32| {
            ((x == r.x0 || x == r.x1) && (r.y0..=r.y1).contains(&y))
                || ((y == r.y0 || y == r.y1) && (r.x0..=r.x1).contains(&x))
        };
        let shares = |prev: &Rect| {
            (f.x0..=f.x1).any(|x| on_wall(prev, x, f.y0) || on_wall(prev, x, f.y1))
                || (f.y0..=f.y1).any(|y| on_wall(prev, f.x0, y) || on_wall(prev, f.x1, y))
        };
        if frames.iter().any(shares) {
            return Err(gate("two frames drawn on one wall", true));
        }
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
            let on_h = xover && ((r.y0..=r.y1).contains(&f.y0) || (r.y0..=r.y1).contains(&f.y1));
            let on_v = yover && ((r.x0..=r.x1).contains(&f.x0) || (r.x0..=r.x1).contains(&f.x1));
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
    // the wall ring of a box is one cell thick: a staircase rounding may
    // graze it (and the invisible cell behind it, NODE out-ranks EDGE), but
    // a route cell inside the ring is a spur the source never drew
    let end_strict = |t: usize, x: i32, y: i32| -> bool {
        if t < nodes.len() {
            nodes[t].1.x0 < x && x < nodes[t].1.x1 && nodes[t].1.y0 < y && y < nodes[t].1.y1
        } else {
            let f = &frames[anchors[t - nodes.len()].1];
            f.x0 < x && x < f.x1 && f.y0 < y && y < f.y1
        }
    };
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
            nodes.iter().position(|(n, _)| *n == nid).or_else(|| {
                anchors
                    .iter()
                    .position(|(a, _)| *a == nid)
                    .map(|p| nodes.len() + p)
            })
        };
        let (Some(from), Some(to)) = (edge_node(&e.from), edge_node(&e.to)) else {
            // an edge to a node this art does not model at all: drawing
            // half the connection would misrepresent which boxes are linked

            return Err(gate("edge touches an unmodelled node", false));
        };
        // start/end labels are edge syntax we would silently drop
        for lb in [e.start_label.as_ref(), e.end_label.as_ref()]
            .into_iter()
            .flatten()
        {
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
        // a route may leave its source box once and enter its target box
        // once: the staircase clipping back through either endpoint draws
        // a spur that exists in no source (review #3, B9)
        let mut left_end = false;
        for &(x, y) in &cells {
            let mine = end_has(from, x, y) || end_has(to, x, y);
            if !mine {
                left_end = true;
            } else if left_end && end_strict(from, x, y) && from != to {
                return Err(gate("route bounces back into its source box", true));
            }
        }
        let mut reentries = 0;
        let mut prev_in_to = false;
        if from != to {
            // (a self route starts and ends on one box: there it is normal)
            for &(x, y) in &cells {
                let in_to = end_strict(to, x, y);
                if in_to && !prev_in_to {
                    reentries += 1;
                }
                prev_in_to = in_to;
            }
            if reentries > 1 {
                return Err(gate("route bounces back into its target box", true));
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
            // two edges between the SAME pair of boxes must each show a
            // stroke of their own: a route that lies wholly on the other's
            // cells (outside the boxes) draws two edges as one arrow, or
            // leaves a floating head with no line (review #3f, 1)
            if (prev.from == from && prev.to == to) || (prev.from == to && prev.to == from) {
                let off_ends = |c: &(i32, i32)| !(end_has(from, c.0, c.1) || end_has(to, c.0, c.1));
                let mine_own = cells.iter().any(|c| off_ends(c) && !prev.cells.contains(c));
                let theirs_own = prev.cells.iter().any(|c| off_ends(c) && !cells.contains(c));
                if !mine_own || !theirs_own {
                    return Err(gate("two parallel edges drawn as one", true));
                }
            }
            // shared role, not just a shared node: A-->B and B-->A converge
            // at both ends yet read as two arrows, so only same-role pairs
            // (both leaving A / both entering B) may merge freely
            let shares = prev.from == from || prev.to == to;
            let shared: Vec<(i32, i32)> = cells
                .iter()
                .filter(|c| prev.cells.contains(c))
                .copied()
                .collect();
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
                    (straight_ns(mine) && straight_ew(theirs))
                        || (straight_ew(mine) && straight_ns(theirs))
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
                let same_role = if start {
                    prev.from == from
                } else {
                    prev.to == to
                };
                if prev.cells.contains(&(hx, hy)) && !same_role {
                    return Err(gate("arrowhead stamped on a foreign line", true));
                }
                // two edges between the same boxes showing ONE head read
                // as one edge (review #3f fuzz6b)
                let same_pair =
                    (prev.from == from && prev.to == to) || (prev.from == to && prev.to == from);
                if same_pair && prev.heads.contains_key(&(hx, hy)) {
                    return Err(gate("two arrowheads land on one cell", true));
                }
            }
            let dec = if start {
                &e.start_decoration
            } else {
                &e.end_decoration
            };
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
            *drawn.last_mut().unwrap().heads.entry((hx, hy)).or_default() |=
                if start { 1 } else { 2 };
        }
    }

    // 3. nodes: opaque boxes, centered multi-line labels (never ellipsized)
    for (id, r) in &nodes {
        let n = layout
            .nodes
            .get(*id)
            .ok_or_else(|| gate("node vanished mid-raster", false))?;
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
                NodeShape::Circle => {
                    if plain {
                        'o'
                    } else {
                        '\u{25cf}'
                    }
                }
                _ => {
                    if plain {
                        '@'
                    } else {
                        '\u{25c9}'
                    }
                }
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
    for (ei, e) in layout.edges.iter().enumerate() {
        let lb = match &e.label {
            Some(lb) if lb.lines.iter().any(|l| !l.trim().is_empty()) => lb,
            _ => continue,
        };
        if lb.lines.iter().any(|l| !printable(l)) {
            return Err(gate(
                "edge label has control or zero-width characters",
                false,
            ));
        }
        let (ax, ay) = e
            .label_anchor
            .ok_or_else(|| gate("edge label without an anchor", false))?;
        let own = ei as i16;
        let w = lb.lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
        // a candidate window is free when nothing drawn is covered and
        // the border columns keep their air: two labels touching read as
        // one word ("pausedispatch" is not two labels). Frame walls are
        // never a backdrop: a label eating a wall cell breaks the wall
        // open like a door the source never drew (B8a); ASCII words may
        // not touch either way (B8b), CJK contact is fine as a fallback
        let clear_at = |x: i32, y: i32, require_gap: bool| -> bool {
            if !(g.inside(x, y) && g.inside(x + w as i32 - 1, y + lb.lines.len() as i32 - 1)) {
                return false;
            }
            for r in 0..lb.lines.len() as i32 {
                for xx in x - 1..=x + w as i32 {
                    let Some(c) = g.get(xx, y + r) else {
                        return false;
                    };
                    if xx == x - 1 || xx == x + w as i32 {
                        if c.owner == TEXT {
                            let ours = if xx == x - 1 {
                                lb.lines[r as usize].trim_start().chars().next()
                            } else {
                                lb.lines[r as usize].trim_end().chars().next_back()
                            };
                            let fuses = c.ch.is_ascii_alphanumeric()
                                && ours.is_some_and(|o| o.is_ascii_alphanumeric());
                            if require_gap || fuses {
                                return false;
                            }
                        }
                        continue;
                    }
                    // the interior admits bare air and the label's own
                    // route (the SVG label punches its line); walls,
                    // boxes, other routes and other words may not (B8a)
                    if !(c.owner == EMPTY || (c.owner == EDGE && c.edge == own)) {
                        return false;
                    }
                }
            }
            true
        };
        // attribution gate (review #3b, 8): a floated label must stay
        // the nearest thing to its OWN route - no foreign route cell
        // or box/dot rect may sit closer - and some own route cell
        // must reach the window without crossing a frame wall, so a
        // label cannot wander across a boundary and caption the
        // wrong arrow (or an unlabelled dot beside it)
        let h = lb.lines.len() as i32;
        let dwin = |x: i32, y: i32, px: i32, py: i32| -> i32 {
            (x - px).max(0).max(px - (x + w as i32 - 1)) + (y - py).max(0).max(py - (y + h - 1))
        };
        let foreign: Vec<(i32, i32)> = drawn
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != ei)
            .flat_map(|(_, d)| d.cells.iter().copied())
            .collect();
        let foreign_rects: Vec<(i32, i32, i32, i32)> = nodes
            .iter()
            .filter(|(id, _)| **id != e.from && **id != e.to)
            .map(|(_, r)| (r.x0, r.y0, r.x1, r.y1))
            .collect();
        let honest = |x: i32, y: i32| -> bool {
            let drect = |x: i32, y: i32, rx0: i32, ry0: i32, rx1: i32, ry1: i32| -> i32 {
                (x - rx1).max(0).max(rx0 - (x + w as i32 - 1))
                    + (y - ry1).max(0).max(ry0 - (y + h - 1))
            };
            let d_own = drawn[ei]
                .cells
                .iter()
                .map(|&(rx, ry)| dwin(x, y, rx, ry))
                .min()
                .unwrap_or(i32::MAX);
            let d_foreign = foreign
                .iter()
                .map(|&(fx, fy)| dwin(x, y, fx, fy))
                .chain(
                    foreign_rects
                        .iter()
                        .map(|r| drect(x, y, r.0, r.1, r.2, r.3)),
                )
                .min()
                .unwrap_or(i32::MAX);
            // strictly nearer to its own route than to anything foreign:
            // a tie reads as belonging to whichever thing the eye meets
            // first, and the eye does not consult our ledger (B8 tie)
            if d_foreign <= d_own {
                return false;
            }
            drawn[ei].cells.iter().any(|&(rx, ry)| {
                // no wall row or column may lie strictly between this
                // own route cell and the window: a label that crosses a
                // boundary could caption what lives in the next box
                // (review #3b, 8)
                let (wy1, wx1) = (y + h - 1, x + w as i32 - 1);
                let between = |q: i32, lo: i32, hi: i32| q > lo && q < hi;
                frames.iter().all(|f| {
                    let row_shut = (ry < y && [f.y0, f.y1].iter().any(|&q| between(q, ry, y)))
                        || (ry > wy1 && [f.y0, f.y1].iter().any(|&q| between(q, wy1, ry)));
                    let col_shut = (rx < x && [f.x0, f.x1].iter().any(|&q| between(q, rx, x)))
                        || (rx > wx1 && [f.x0, f.x1].iter().any(|&q| between(q, wx1, rx)));
                    !row_shut && !col_shut
                })
            })
        };
        let mut spot = None;
        // the SVG label hangs loosely near its line's anchor: a few rows
        // up or down all still read as belonging to it; more than that
        // would name a different line, so the search stops at two. A gap
        // from neighbouring labels comes first; sharing a border with a
        // word is ugly, refusing the diagram is worse
        'anchor: for (require_gap, dy) in [true, false]
            .into_iter()
            .flat_map(|gap| [0i32, -1, 1, -2, 2].map(move |dy| (gap, dy)))
        {
            // SVG hangs labels beside their line, not dead centre; when
            // the centre sits on a foreign vertical, drift sideways
            for dx in [0i32, -1, 1, -2, 2, -3, 3, -4, 4] {
                let y = cy(ay) + dy - (lb.lines.len() as i32 - 1) / 2;
                let x = cx(ax) + dx - w as i32 / 2;
                if clear_at(x, y, require_gap) && honest(x, y) {
                    spot = Some((x, y));
                    break 'anchor;
                }
            }
        }
        if spot.is_none() {
            // last resort: float beside the drawn route. The SVG anchor
            // can round inside a composite's boxes while the line runs
            // free outside; one step off the polyline still names this
            // route and no other, while a refusal drops the whole diagram
            let (axc, ayc) = (cx(ax), cy(ay));
            let mut spots: Vec<(i32, i32, i32)> = Vec::new();
            for &(rx, ry) in &drawn[ei].cells {
                for dy in [-1i32, 0, 1] {
                    // window by its left edge: a label may end beside the
                    // route, not only centre on it (own route cells are
                    // legal backdrop, the SVG label punches its own line)
                    for dx in -(w as i32)..=3 {
                        let (x, y) = (rx + dx, ry + dy);
                        let d = (x + w as i32 / 2 - axc).abs() + (y - ayc).abs();
                        spots.push((d, x, y));
                    }
                }
            }
            spots.sort_unstable();
            spots.dedup_by(|a, b| a.1 == b.1 && a.2 == b.2);
            'route: for require_gap in [true, false] {
                for &(_, x, y) in &spots {
                    if clear_at(x, y, require_gap) && honest(x, y) {
                        spot = Some((x, y));
                        break 'route;
                    }
                }
            }
        }
        if let Some((x, y)) = spot {
            for (i, line) in lb.lines.iter().enumerate() {
                let pad = (w - visible_width(line)) as i32 / 2;
                g.text(x + pad, y + i as i32, line, TEXT);
                if !line.trim().is_empty() {
                    placed.push((x + pad, y + i as i32, line));
                }
            }
        } else {
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
            let sep = if matches!(e.style, mermaid_rs_renderer::ir::EdgeStyle::Dotted) {
                ".."
            } else {
                "--"
            };
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
                if verb.is_empty() {
                    String::new()
                } else {
                    format!("   {verb}")
                }
            );
            // the list hangs under the picture, not inside it: the
            // terminal (win) is the wall it must clear, not the grid
            if visible_width(&line) > win {
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
        Some(EdgeDecoration::CrowsFootZeroOne) => {
            if start {
                "|o"
            } else {
                "o|"
            }
        }
        Some(EdgeDecoration::CrowsFootMany) => {
            if start {
                "}|"
            } else {
                "|{"
            }
        }
        // the start side mirrors the end side: reverse and swap brace dirs
        Some(EdgeDecoration::CrowsFootZeroMany) => {
            if start {
                "}o"
            } else {
                "o{"
            }
        }
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
        NodeShape::RoundRect
            | NodeShape::Stadium
            | NodeShape::Circle
            | NodeShape::DoubleCircle
            | NodeShape::Cylinder
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
        return Err(gate(
            "node without a label: fine in SVG, pointless in art",
            false,
        ));
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
/// A line's row: unclaimed, free of boxes, and positioned so it stays in
/// the same order as every already-claimed line - a slide that crossed
/// another event would retell the story out of turn (review #3, B1).
/// Slide order prefers DOWN (sequence flows top-down); an equal px means
/// the crate fused two events at one height, which no honest ASCII
/// reading can un-fuse, so no row is offered at all.
fn claim_row(
    want: i32,
    px: f32,
    used: &mut HashSet<i32>,
    rows: usize,
    boxes: &[Rect],
    lines_px: &[(f32, i32)],
) -> Option<i32> {
    for off in [0, 1, 2, -1, -2] {
        let r = want + off;
        if r < 0 || r >= rows as i32 {
            continue;
        }
        if used.contains(&r) || boxes.iter().any(|b| (b.y0..=b.y1).contains(&r)) {
            continue;
        }
        let ordered = lines_px
            .iter()
            .all(|&(p, rr)| (px < p && r <= rr) || (px > p && r >= rr));
        if ordered {
            used.insert(r);
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

/// the crate's sequence parser answers with the first arrow token from a
/// fixed list that appears anywhere on a line, swaps the ends of `<-`
/// arrows, and silently drops statements and messages it cannot tokenise:
/// an open arrow (`->`) gains a filled head, `A->xB` mangles its target,
/// `A<->B` invents a participant named ">B". To refuse rather than draw
/// that, reread the source with the parser's own scan and keep only
/// filled, one-way messages; anything else either draws nothing here or
/// refuses whole (review #3, B4)
/// what the source says a sequence diagram holds: the messages (in
/// order), activation events (who, after how many messages, and whether
/// the arrow itself lies inside the span), the declared names, and how
/// many frames and else-branches were written. The layout is audited
/// against all of it (review #3b, 3/6/7): the crate drops frames without
/// messages, hangs activation bars on the wrong arrow, and deactivates
/// the receiver of a `-` arrow where mermaid deactivates the sender
struct SeqSrc<'a> {
    msgs: Vec<(&'a str, &'a str)>,
    /// declared participant ids, in the order the source declared them
    decls: Vec<&'a str>,
    /// participants in first-mention order over messages AND notes:
    /// mermaid gives a lane to a named note actor where it is written
    /// (review #3d, fuzz5), so the lane order follows this chain
    appear: Vec<&'a str>,
    events: Vec<SeqEvt<'a>>,
    frames: Vec<FrameSrc>,
    sections: usize,
    notes: usize,
    /// per note, every frame written around it (closing-order index) and
    /// the section of that frame it sits in (0 = above the first `else`)
    note_in: Vec<Vec<(usize, usize)>>,
}

/// one written alt/opt/loop: the message indexes (source order) and
/// notes it holds and its else-divider count; the raster must find a
/// layout frame holding exactly that, or the crate has moved the frame
/// over messages the source placed outside it (review #3c, 1)
struct FrameSrc {
    lo: usize,
    hi: usize,
    divs: usize,
    notes: usize,
}

/// the last VISIBLE statement a source line followed: activation bars
/// start and stop at that statement's drawn row, because mermaid anchors
/// them at the current position — after any note, frame top, divider or
/// floor the source wrote in between — never at the previous message
/// alone (review #3c, 4)
#[derive(Clone, Copy)]
enum Last {
    None,
    Msg(usize),
    Note(usize),
    /// a frame's top wall, by index in CLOSING order
    Top(usize),
    /// its k-th divider
    Div(usize, usize),
    /// its floor
    Floor(usize),
}

#[derive(Clone, Copy)]
enum SeqEvt<'a> {
    Start {
        p: &'a str,
        last: Last,
        incl: Option<usize>,
    },
    End {
        p: &'a str,
        last: Last,
        incl: Option<usize>,
    },
}

fn seq_source_messages(src: &str) -> Result<SeqSrc<'_>, Fail> {
    // the parser's exact token list, in its exact order
    const TOKENS: [&str; 16] = [
        "-->>+", "->>+", "-->+", "->+", "-->>-", "->>-", "-->-", "->-", "<--+", "<-+", "<--", "<-",
        "-->>", "->>", "-->", "->",
    ];
    // statements that leave nothing for the art to misstate: lanes,
    // frames and notes they create are checked by the gates in the
    // raster. Statements that draw nothing at all (title, link, rect,
    // autonumber) are deliberately not on this list: dropping written
    // words silently is the one thing the terminal pager may not do
    const STATEMENTS: [&str; 11] = [
        "sequencediagram",
        "participant",
        "actor",
        "end",
        "alt",
        "else",
        "opt",
        "loop",
        "note",
        "activate",
        "deactivate",
    ];
    let mut out = SeqSrc {
        msgs: Vec::new(),
        events: Vec::new(),
        decls: Vec::new(),
        appear: Vec::new(),
        frames: Vec::new(),
        sections: 0,
        notes: 0,
        note_in: Vec::new(),
    };
    // open frames as (open index, message mark, note mark, divs so far),
    // LIFO; Top/Div events carry the OPEN index and are rewritten to the
    // closing-order frame identity once the scan knows it
    let mut stack: Vec<(usize, usize, usize, usize)> = Vec::new();
    // the keyword that opened each stacked frame: only `alt` takes `else`
    let mut kinds: Vec<String> = Vec::new();
    let mut open2close: Vec<Option<usize>> = Vec::new();
    let mut last = Last::None;
    for raw in src.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        // mermaid splits statements on `;` and reads `%%` as a comment
        // only at the line start; the crate does neither, so `A->>B: hi;
        // B->>A: yo` would draw one event where the source wrote two and
        // `hi %% x` would lose its tail (review #3c, 2/8)
        if line.contains(';') {
            return Err(gate(
                "a semicolon splits statements the parser merges",
                false,
            ));
        }
        if line.contains("%%") {
            return Err(gate(
                "a mid-line comment truncates the label upstream",
                false,
            ));
        }
        let head = line
            .split(|c: char| c == ':' || c.is_whitespace())
            .next()
            .unwrap_or(line)
            .to_ascii_lowercase();
        if STATEMENTS.contains(&head.as_str()) {
            match head.as_str() {
                "alt" | "opt" | "loop" => {
                    let oi = open2close.len();
                    open2close.push(None);
                    stack.push((oi, out.msgs.len(), out.notes, 0));
                    kinds.push(head.clone());
                    last = Last::Top(oi);
                }
                "else" => {
                    let Some(top) = stack.last_mut() else {
                        return Err(gate("an else outside any frame", false));
                    };
                    // `loop`/`opt` take no `else` in mermaid (a parse
                    // error); the crate splits the frame (review #3l, 1)
                    if kinds.last().map(String::as_str) != Some("alt") {
                        return Err(gate("an else in a frame that has no branches", false));
                    }
                    last = Last::Div(top.0, top.3);
                    top.3 += 1;
                    out.sections += 1;
                }
                "end" => {
                    // the crate auto-closes frames and swallows stray
                    // ends; mermaid rejects both, and a drawn closure the
                    // source never wrote is an invented fact
                    let Some((oi, lo, note_mark, divs)) = stack.pop() else {
                        return Err(gate("an end that closes no frame", false));
                    };
                    kinds.pop();
                    let close = out.frames.len();
                    open2close[oi] = Some(close);
                    last = Last::Floor(close);
                    out.frames.push(FrameSrc {
                        lo,
                        hi: out.msgs.len(),
                        divs,
                        notes: out.notes - note_mark,
                    });
                }
                "note" => {
                    last = Last::Note(out.notes);
                    out.notes += 1;
                    // open indexes for now, settled to closing order below
                    out.note_in.push(stack.iter().map(|t| (t.0, t.3)).collect());
                    // `Note over A,B:` / `Note left of A:` name lanes at
                    // this line in mermaid; collect plain ids only
                    let rest = line[head.len()..].split(':').next().unwrap_or("");
                    for tok in rest.split(',').flat_map(|t| t.split_whitespace()) {
                        if tok.starts_with('"') {
                            // mermaid gives a QUOTED name its own lane:
                            // `Note over "C"` next to a real C is TWO
                            // lanes there, one here — the crate merges
                            // them, so the honest answer is to refuse
                            // (review #3e, 3)
                            return Err(gate(
                                "a quoted note actor the layout merges into another lane",
                                false,
                            ));
                        }
                        let id = tok.trim_matches('"');
                        if !id.is_empty()
                            && !matches!(
                                id.to_ascii_lowercase().as_str(),
                                "over" | "left" | "right" | "of"
                            )
                            && id
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                        {
                            out.appear.push(id);
                        }
                    }
                }
                "participant" | "actor" => {
                    // the id is the FIRST word (`participant B as Bob`
                    // declares B); the lane order gate needs declarations
                    let id = line[head.len()..].split_whitespace().next().unwrap_or("");
                    if !id.is_empty() {
                        out.decls.push(id);
                    }
                }
                "sequencediagram" => {}
                "activate" | "deactivate" => {
                    // ids only: mermaid resolves activations by participant
                    // id and errors on a bare display name (review #3c, 3);
                    // whether the id owns a lane is settled at draw time
                    let who = line[head.len()..]
                        .trim()
                        .split(':')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if who.is_empty() {
                        return Err(gate("an activation naming no participant", false));
                    }
                    out.events.push(if head == "activate" {
                        SeqEvt::Start {
                            p: who,
                            last,
                            incl: None,
                        }
                    } else {
                        SeqEvt::End {
                            p: who,
                            last,
                            incl: None,
                        }
                    });
                }
                _ => {}
            }
            continue;
        }
        // mirror the parser's scan: first token (in its order) with a
        // non-empty left and right wins, exactly as there
        let mut found: Option<(&str, usize)> = None;
        for t in TOKENS {
            if let Some(pos) = line.find(t) {
                if !line[..pos].trim().is_empty() && !line[pos + t.len()..].trim().is_empty() {
                    found = Some((t, pos));
                    break;
                }
            }
        }
        let Some((token, pos)) = found else {
            return Err(gate(
                "a statement this raster cannot draw refuses first",
                false,
            ));
        };
        if token.starts_with('<') {
            return Err(gate("an arrowhead at the source end is not drawn", false));
        }
        // only a filled head pointing away from the sender: the parser
        // marks every token it accepts as directed, open arrows included,
        // so `->` and `-->` would arrive here wearing a head the source
        // never wrote
        if !token.trim_end_matches(['+', '-']).ends_with(">>") {
            return Err(gate("an open or crossed arrowhead is not drawn", false));
        }
        let left = line[..pos].trim();
        let right = line[pos + token.len()..]
            .trim()
            .split(':')
            .next()
            .unwrap_or("")
            .trim();
        let plain_id = |p: &str| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        };
        if !plain_id(left) || !plain_id(right) {
            return Err(gate("a participant the message parse cannot name", false));
        }
        let i = out.msgs.len();
        out.msgs.push((left, right));
        out.appear.push(left);
        out.appear.push(right);
        last = Last::Msg(i);
        // `->>+` wakes the receiver from this arrow on; `->>-` retires
        // the SENDER after it (mermaid semantics; the crate applies `-`
        // to the receiver, which is not what the source said)
        if token.ends_with('+') {
            out.events.push(SeqEvt::Start {
                p: right,
                last,
                incl: Some(i),
            });
        } else if token.ends_with('-') {
            out.events.push(SeqEvt::End {
                p: left,
                last,
                incl: Some(i),
            });
        }
    }
    if !stack.is_empty() {
        return Err(gate("a frame that never closes", false));
    }
    // settle Top/Div onto the closing-order frame identity
    for ev in &mut out.events {
        let l = match ev {
            SeqEvt::Start { last, .. } | SeqEvt::End { last, .. } => last,
        };
        *l = match *l {
            Last::Top(oi) => Last::Top(open2close[oi].unwrap_or(usize::MAX)),
            Last::Div(oi, k) => Last::Div(open2close[oi].unwrap_or(usize::MAX), k),
            other => other,
        };
    }
    for chain in &mut out.note_in {
        for (f, _) in chain.iter_mut() {
            *f = open2close[*f].unwrap_or(usize::MAX);
        }
    }
    Ok(out)
}

/// sequence diagrams speak in lanes and rows, not routed boxes: lifeline x
/// maps to a column, message y to a row. Source order IS the topology, so
/// messages are never merged or reordered. A claim ledger keeps them apart:
/// full-width lines (walls, dividers, arrows) own entire rows, boxes and
/// notes own rectangles, and frame wall and divider cells are sacred, so no
/// label or box may break a section open like a door the source never
/// drew. Rounding crowds events into single rows at cell scale; slides are
/// bounded, and after drawing one final gate re-reads every line against
/// the px height it stands for and refuses unless the rows kept the events
/// in the order the source told them (review #3, B1)
fn sequence_raster(
    src: &str,
    layout: &Layout,
    cols: usize,
    rows: usize,
    plain: bool,
) -> Result<Vec<String>, Fail> {
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
        if !matches!(
            n.position,
            mermaid_rs_renderer::ir::SequenceNotePosition::Over
        ) {
            return Err(gate("leftOf/rightOf notes are not drawn", false));
        }
    }
    if layout
        .nodes
        .values()
        .any(|n| !matches!(n.shape, NodeShape::ActorBox))
    {
        return Err(gate("stick-figure actors are not drawn", false));
    }
    if seq.lifelines.len() < 2 {
        return Err(gate("sequence needs at least two lanes", false));
    }
    // the drawn messages must be exactly the written ones: same count,
    // same order, same participants (review #3, B4)
    let aud = seq_source_messages(src)?;
    if aud.msgs.len() != layout.edges.len()
        || aud
            .msgs
            .iter()
            .zip(layout.edges.iter())
            .any(|((f, t), e)| *f != e.from || *t != e.to)
    {
        return Err(gate("drawn messages differ from the written ones", false));
    }
    // a frame without a single message is deleted by the renderer
    // (layout/sequence.rs start_idx >= end_idx) together with its label
    // and sections: notes inside it would then draw outside any block
    if aud.frames.len() != seq.frames.len()
        || seq.frames.iter().map(|f| f.dividers.len()).sum::<usize>() != aud.sections
        || aud.notes != seq.notes.len()
    {
        return Err(gate(
            "a frame the renderer drops would orphan its words",
            false,
        ));
    }
    // frame membership (review #3c, 1): the crate counts a note written
    // just before a frame as part of it and lifts the top wall over the
    // messages that came before, so messages the source placed OUTSIDE
    // the loop would draw inside it. Each frame's px band must hold
    // exactly the messages, notes and dividers the source wrote within
    // its lines; pairing the frames also anchors activation bars
    let head_y: Vec<f32> = layout.edges.iter().map(|e| e.points[0].1).collect();
    let note_mid: Vec<f32> = seq.notes.iter().map(|n| n.y + n.height / 2.0).collect();
    type FKey = (usize, usize, usize, usize);
    let mut cand: HashMap<FKey, Vec<usize>> = HashMap::new();
    for (fi, f) in seq.frames.iter().enumerate() {
        let inside: Vec<usize> = (0..head_y.len())
            .filter(|i| head_y[*i] > f.y && head_y[*i] < f.y + f.height)
            .collect();
        let notes = note_mid
            .iter()
            .filter(|y| **y > f.y && **y < f.y + f.height)
            .count();
        let (lo, hi) = match (inside.first(), inside.last()) {
            (Some(a), Some(b)) => (*a, b + 1),
            _ => (0, 0),
        };
        if inside.len() != hi - lo {
            return Err(gate("a frame holds a gapped set of messages", true));
        }
        cand.entry((lo, hi, notes, f.dividers.len()))
            .or_default()
            .push(fi);
    }
    // identical keys happen for nested frames holding the same rows:
    // the taller band is always the outer frame, which also closes later
    for v in cand.values_mut() {
        v.sort_by(|a, b| seq.frames[*b].height.total_cmp(&seq.frames[*a].height));
    }
    let src_fi: Vec<usize> = {
        let mut order: Vec<usize> = (0..aud.frames.len()).collect();
        order.sort_by_key(|si| {
            let f = &aud.frames[*si];
            (f.lo, f.hi, f.notes, f.divs, usize::MAX - si)
        });
        let mut back = vec![usize::MAX; aud.frames.len()];
        for si in order {
            let f = &aud.frames[si];
            let key = (f.lo, f.hi, f.notes, f.divs);
            let Some(fi) = cand.get_mut(&key).and_then(|v| v.pop()) else {
                return Err(gate(
                    "a frame would hold messages the source wrote outside it",
                    true,
                ));
            };
            back[si] = fi;
        }
        back
    };
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

    // lane order must follow mermaid: declared participants, then first
    // appearance in the messages. An `activate` line names a participant
    // but creates no lane there — the crate registers one at its line
    // and slides that lane left of the speakers (review #3d, 2). Note
    // ghosts add lanes in both renderers, so only lanes that speak or
    // declare must line up in order
    let mut want: Vec<&str> = Vec::new();
    for d in aud.decls.iter().chain(aud.appear.iter()) {
        if !want.contains(d) {
            want.push(d);
        }
    }
    let lined_up: Vec<&str> = seq
        .lifelines
        .iter()
        .map(|l| l.id.as_str())
        .filter(|id| want.contains(id))
        .collect();
    if lined_up != want {
        return Err(gate("a line the renderer moved to another lane", true));
    }

    // the claim ledger: rows belong to full-width lines (walls, dividers,
    // arrows and labels); boxes and notes own rectangles and may share a
    // row with each other only where their columns do not touch; wall and
    // divider cells are sacred to the section structure (review #3, B3)
    let mut used: HashSet<i32> = HashSet::new();
    let mut boxes: Vec<Rect> = Vec::new();
    let mut sacred: HashSet<(i32, i32)> = HashSet::new();
    // every full-width line with the px it stands for, for the order gate
    let mut lines_px: Vec<(f32, i32)> = Vec::new();

    // lifelines first, under everything that speaks
    for l in &seq.lifelines {
        let c = lane[l.id.as_str()];
        for y in cy(l.y1) + 1..cy(l.y2) {
            g.put(c, y, vb, FRAME);
        }
    }

    // 1. frames: two blocks that overlap where neither strictly contains
    // the other have no honest reading at all: a message between their
    // walls would belong to two sections at once, and the crate produces
    // exactly that overlap for consecutive blocks on real fixtures
    // (review #3, B2). Nested frames must nest strictly, walls included
    let frects: Vec<Rect> = seq
        .frames
        .iter()
        .map(|f| Rect {
            x0: cx(f.x),
            y0: cy(f.y),
            x1: cx(f.x + f.width),
            y1: cy(f.y + f.height),
        })
        .collect();
    for i in 0..frects.len() {
        for j in i + 1..frects.len() {
            let (a, b) = (&frects[i], &frects[j]);
            if a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1 {
                // frames spanning the same lanes nest at the SAME x —
                // mermaid draws an opt inside an alt across the full
                // width — so containment is relaxed on x but strict on
                // y: sharing a wall ROW stays the B2 merge artifact it
                // is, and a real cross-overlap stays refused
                let a_in_b = b.x0 <= a.x0 && a.x1 <= b.x1 && b.y0 < a.y0 && a.y1 < b.y1;
                let b_in_a = a.x0 <= b.x0 && b.x1 <= a.x1 && a.y0 < b.y0 && b.y1 < a.y1;
                if !(a_in_b || b_in_a) {
                    return Err(gate(
                        "two frames overlap where neither holds the other",
                        true,
                    ));
                }
            }
        }
    }

    // walls claim their rows, so arrows inside can never crowd a wall out;
    // every wall and divider cell is sacred from then on
    struct FrameRows {
        x0: i32,
        x1: i32,
        y0: i32,
        y1: i32,
        lines: Vec<i32>,
    }
    let mut frame_rows: Vec<(usize, FrameRows)> = Vec::new();
    // a nested frame's wall meeting an already-drawn wall must join it,
    // not punch a hole the source never drew: the outer wall keeps its
    // stroke and the joint reads as a T or a cross (review #3d, nested
    // art; frames arrive outer-first, so inner frames land on the join)
    let lane_cols: Vec<i32> = lane.values().copied().collect();
    let put_join = |g: &mut Grid, x: i32, y: i32, want: char| {
        let cur = g.get(x, y).map(|c| c.ch).unwrap_or(' ');
        // a `│` not standing on a lane column is an outer frame's wall
        let outer_wall = matches!(cur, '\u{2502}' | '\u{2551}') && !lane_cols.contains(&x);
        let ch = if outer_wall {
            match want {
                w if w == tl || w == bl => t_r,
                w if w == tr || w == br => t_l,
                w if w == hb => cross,
                _ => return, // side on side: the same stroke already stands
            }
        } else {
            want // empty air, or a lane the wall erases as it always has
        };
        g.put(x, y, ch, FRAME);
    };
    for (fi, f) in seq.frames.iter().enumerate() {
        let (x0, x1) = (cx(f.x), cx(f.x + f.width));
        if x1 - x0 < 3 {
            return Err(gate("a frame this narrow cannot hold its walls", true));
        }
        let want_h = cy(f.y + f.height) - cy(f.y);
        if want_h < 3 {
            return Err(gate("a frame this short cannot hold its sections", true));
        }
        let y0 = claim_row(cy(f.y), f.y, &mut used, rows, &boxes, &lines_px)
            .ok_or_else(|| gate("a frame wall fuses into a used row", true))?;
        lines_px.push((f.y, y0));
        let y1 = claim_row(
            cy(f.y + f.height),
            f.y + f.height,
            &mut used,
            rows,
            &boxes,
            &lines_px,
        )
        .ok_or_else(|| gate("a frame wall fuses into a used row", true))?;
        lines_px.push((f.y + f.height, y1));
        if y1 - y0 < 3 {
            return Err(gate("a frame this short cannot hold its sections", true));
        }
        for x in x0 + 1..x1 {
            put_join(&mut g, x, y0, hb);
            put_join(&mut g, x, y1, hb);
        }
        for y in y0 + 1..y1 {
            // side walls: a column an outer wall already holds is `│`
            // either way; only a cross on a drawn horizontal matters
            put_join(&mut g, x0, y, vb);
            put_join(&mut g, x1, y, vb);
        }
        put_join(&mut g, x0, y0, tl);
        put_join(&mut g, x1, y0, tr);
        put_join(&mut g, x0, y1, bl);
        put_join(&mut g, x1, y1, br);
        for x in x0..=x1 {
            sacred.insert((x, y0));
            sacred.insert((x, y1));
        }
        for y in y0..=y1 {
            sacred.insert((x0, y));
            sacred.insert((x1, y));
        }
        frame_rows.push((
            fi,
            FrameRows {
                x0,
                x1,
                y0,
                y1,
                lines: Vec::new(),
            },
        ));
    }
    // labels and dividers after the walls exist (labels ride walls; the
    // SVG label pentagon punches its own wall, TEXT over FRAME is the
    // same word, but never a corner and never another section's line)
    let mut last_label_end: HashMap<(usize, i32), i32> = HashMap::new();
    for (fi, fr) in &mut frame_rows {
        let f = &seq.frames[*fi];
        // dividers claim rows FIRST so labels can snap to where the line
        // actually ended up after any slide
        for d in &f.dividers {
            let dy = claim_row(cy(*d), *d, &mut used, rows, &boxes, &lines_px)
                .ok_or_else(|| gate("a frame divider fuses into a used row", true))?;
            lines_px.push((*d, dy));
            if dy <= fr.y0 || dy >= fr.y1 {
                return Err(gate("a frame divider escapes its frame", true));
            }
            for x in fr.x0 + 1..fr.x1 {
                g.put(x, dy, hb, FRAME);
            }
            g.put(fr.x0, dy, t_r, FRAME);
            g.put(fr.x1, dy, t_l, FRAME);
            for x in fr.x0..=fr.x1 {
                sacred.insert((x, dy));
            }
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
                let x = last_label_end
                    .get(&(*fi, y))
                    .map_or(x, |&end| x.max(end + 1));
                // the pentagon may punch its own wall, but running off the
                // wall end would erase the frame corner: a wider label
                // needs a wider frame, and clipping a section name is a
                // word dropped (review #3, B2)
                if x + w > fr.x1 {
                    return Err(gate("a frame label runs over its corner", true));
                }
                for xx in x..x + w {
                    let own_line = xx > fr.x0 && xx < fr.x1;
                    let clear = matches!(g.get(xx, y), Some(c) if c.owner <= FRAME)
                        && (!sacred.contains(&(xx, y)) || own_line);
                    if !clear {
                        return Err(gate("a frame label lands on drawn art", true));
                    }
                }
                g.text(x, y, line, TEXT);
                placed.push((x, y, line));
                last_label_end.insert((*fi, y), x + w);
            }
        }
    }

    // 2. actor boxes at head and foot (NODE level over frame walls): a
    // box needs cells of its own: no line row, no other box, nothing
    // drawn underneath, and no sacred wall cell inside it
    let mut head_bot = 0i32;
    let mut foot_top = rows as i32 - 1;
    let boxed = layout
        .nodes
        .values()
        .map(|n| (true, n))
        .chain(seq.footboxes.iter().map(|n| (false, n)));
    for (head, n) in boxed {
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
            let free = (r.y0..=r.y1).all(|y| !used.contains(&y))
                && !boxes
                    .iter()
                    .any(|b| b.x0 <= r.x1 && r.x0 <= b.x1 && b.y0 <= r.y1 && r.y0 <= b.y1)
                && !(r.y0..=r.y1).any(|y| (r.x0..=r.x1).any(|x| sacred.contains(&(x, y))))
                && band_clear(&g, r);
            if free {
                band = Some(r);
                break;
            }
        }
        let r = band.ok_or_else(|| gate("an actor box lands on drawn art", true))?;
        if r.x1 - r.x0 < 4 || r.y1 - r.y0 < 2 {
            return Err(gate("an actor box this thin cannot hold its name", true));
        }
        put_label_box(
            &mut g,
            r,
            &n.label.lines,
            (tl, tr, bl, br, hb, vb),
            &mut placed,
            false,
        )?;
        boxes.push(r);
        if head {
            head_bot = head_bot.max(r.y1);
        } else {
            foot_top = foot_top.min(r.y0);
        }
    }

    // 4. messages: one row each, in source order, never merged
    let mut msg_rows: Vec<i32> = Vec::new();
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
        let mr = claim_row(
            cy(e.points[0].1),
            e.points[0].1,
            &mut used,
            rows,
            &boxes,
            &lines_px,
        )
        .ok_or_else(|| gate("two messages fuse into one row", true))?;
        lines_px.push((e.points[0].1, mr));
        msg_rows.push(mr);
        if has_label {
            let line = e.label.as_ref().unwrap().lines.first().unwrap();
            let w = visible_width(line) as i32;
            let ax = e.label_anchor.map(|(x, _)| cx(x)).unwrap_or((ca + cb) / 2);
            let lx = (ax - w / 2).max(1).min((cols as i32 - 1 - w).max(1));
            // the SVG label field punches through lane lines - lanes may
            // sit under it, anything already spoken for may not; a label
            // may rise or sink one row (SVG hangs it loosely), the arrow
            // it names never moves, and no claimed line may lie between
            // the two: a label floating over a wall would file its
            // message into the section above or below (review #3, B1)
            let mut ly = None;
            // only above, never below (review #3b, 2): a label under its
            // arrow reads as the caption of the NEXT arrow, and an
            // unlabelled arrow makes that swap silent
            for r in [mr - 1, mr - 2] {
                if r <= 0 || r >= rows as i32 || used.contains(&r) {
                    continue;
                }
                if (mr.min(r) + 1..mr.max(r)).any(|q| used.contains(&q)) {
                    continue;
                }
                if (lx..lx + w).all(|x| match g.get(x, r) {
                    None => false,
                    Some(c) => c.owner <= FRAME && !sacred.contains(&(x, r)),
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
            // self message: the 4-point stub loop right of the lane. The
            // route may cross a frame wall (a real crossing, the SVG
            // draws the same) but can never share a row with one: the
            // claim ledger made those rows unavailable above
            if e.points.len() < 4 {
                return Err(gate("self message route not understood", false));
            }
            let r1 = claim_row(
                cy(e.points[3].1),
                e.points[3].1,
                &mut used,
                rows,
                &boxes,
                &lines_px,
            )
            .ok_or_else(|| gate("self message return fuses into another row", true))?;
            if r1 <= mr {
                return Err(gate("self message return rides above its start", true));
            }
            lines_px.push((e.points[3].1, r1));
            let s = cx(e.points[1].0).max(ca + 2).min(cols as i32 - 2);
            if s <= ca + 1 {
                return Err(gate("no room beside the lane for a self loop", true));
            }
            // the stub's box must not contain a wall or divider: a loop
            // that starts under one section line and returns under the
            // next lived in two branches at once (review #3b, 4)
            for y in mr + 1..r1 {
                for x in ca + 1..=s {
                    if sacred.contains(&(x, y)) {
                        return Err(gate("a self loop straddles a section line", true));
                    }
                }
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

    let mut note_rects: Vec<Rect> = Vec::new();
    // 5. notes over: a boxed label across its participants, needing cells
    // as free as an actor box's (messages are drawn: band_clear would
    // refuse them, so a note slides or refuses, never overwrites)
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
            let free = (r.y0..=r.y1).all(|y| !used.contains(&y))
                && !boxes
                    .iter()
                    .any(|b| b.x0 <= r.x1 && r.x0 <= b.x1 && b.y0 <= r.y1 && r.y0 <= b.y1)
                && !(r.y0..=r.y1).any(|y| (r.x0..=r.x1).any(|x| sacred.contains(&(x, y))))
                && band_clear(&g, r);
            if free {
                band = Some(r);
                break;
            }
        }
        let r = band.ok_or_else(|| gate("a note lands on drawn art", true))?;
        if r.x1 - r.x0 < 4 || r.y1 - r.y0 < 2 {
            return Err(gate("a note this thin cannot hold its text", true));
        }
        put_label_box(
            &mut g,
            r,
            &nt.label.lines,
            (tl, tr, bl, br, hb, vb),
            &mut placed,
            false,
        )?;
        boxes.push(r);
        note_rects.push(r);
    }
    // each note must stand strictly inside every frame written around it,
    // in the very section it was written in, and clear of every other
    // frame: the crate drops a note that ends an `alt` branch below the
    // `else` divider, and leaves a frame narrower than a note's lane
    // (review #3k, 2)
    for (ni, r) in note_rects.iter().enumerate() {
        let chain = aud.note_in.get(ni).map(Vec::as_slice).unwrap_or(&[]);
        for (fi, fr) in &frame_rows {
            let want = chain
                .iter()
                .find(|(si, _)| src_fi.get(*si) == Some(fi))
                .map(|&(_, k)| k);
            let inside = fr.x0 < r.x0 && r.x1 < fr.x1 && fr.y0 < r.y0 && r.y1 < fr.y1;
            let apart = r.x1 < fr.x0 || fr.x1 < r.x0 || r.y1 < fr.y0 || fr.y1 < r.y0;
            match want {
                None if apart => {}
                Some(k) if inside => {
                    let top = if k == 0 {
                        Some(fr.y0)
                    } else {
                        fr.lines.get(k - 1).copied()
                    };
                    let bot = fr.lines.get(k).copied().unwrap_or(fr.y1);
                    if !top.is_some_and(|t| t < r.y0 && r.y1 < bot) {
                        return Err(gate("a note drawn in another branch", true));
                    }
                }
                _ => return Err(gate("a note drawn across a frame it is not in", true)),
            }
        }
    }

    // 5b. activation bars from the SOURCE and the ledger (review #3b
    // 6/7, #3c 3/4/5/6): the crate hangs every bar on the wrong arrow,
    // deactivates the receiver of a `-` arrow, and anchors bars to
    // messages alone, ignoring the notes and frame lines written in
    // between. Each event carries the last VISIBLE statement its line
    // followed; a start begins one row below that statement's drawn row,
    // an end closes on it; `+`/`-` suffixes ride their own arrow row; a
    // never-closed bar runs to the foot box; a bar no statement spans
    // takes one free row strictly BEFORE the next statement, or refuses;
    // and a column that would sit on a frame wall refuses rather than
    // vanish into it. Arrows, boxes and words out-rank the bar; a wall
    // row it crosses joins as a junction, both lines truly continuing
    let anchor = |last: Last| -> Option<i32> {
        match last {
            Last::None => None,
            Last::Msg(i) => Some(msg_rows[i]),
            Last::Note(n) => note_rects.get(n).map(|r| r.y1),
            other => {
                let (si, want) = match other {
                    Last::Top(si) => (si, 0usize),
                    Last::Div(si, k) => (si, k + 1),
                    Last::Floor(si) => (si, usize::MAX),
                    _ => return None,
                };
                if si == usize::MAX {
                    return None;
                }
                let fi = *src_fi.get(si)?;
                let f = &frame_rows.get(fi)?.1;
                match want {
                    0 => Some(f.y0),
                    usize::MAX => Some(f.y1),
                    k => f.lines.get(k - 1).copied(),
                }
            }
        }
    };
    let speaks = |p: &str| aud.msgs.iter().any(|(f, t)| *f == p || *t == p);
    let mut open: HashMap<&str, Vec<(i32, usize)>> = HashMap::new();
    let mut bars: Vec<(i32, i32, i32)> = Vec::new();
    for ev in &aud.events {
        let (p, is_start, last, incl) = match ev {
            SeqEvt::Start { p, last, incl } => (*p, true, *last, *incl),
            SeqEvt::End { p, last, incl } => (*p, false, *last, *incl),
        };
        let lane_c = *lane
            .get(p)
            .ok_or_else(|| gate("activation for an unknown participant", false))?;
        // a bar claims a participant lived through these rows: a name
        // that never speaks has no lane in mermaid (display names are
        // an upstream TypeError), so it has no bar here (review #3c, 3)
        if !speaks(p) {
            return Err(gate("an activation for a silent participant", false));
        }
        if is_start {
            let top = match incl {
                Some(i) => msg_rows[i],
                None => anchor(last).map(|r| r + 1).unwrap_or(head_bot + 1),
            };
            let stack = open.entry(p).or_default();
            let depth = stack.len();
            if depth > 2 {
                return Err(gate("activation stack too deep to keep apart", false));
            }
            stack.push((top, depth));
        } else {
            let bot = match incl {
                Some(i) => msg_rows[i],
                None => anchor(last).unwrap_or(head_bot),
            };
            let (top, depth) = open
                .get_mut(p)
                .and_then(|st| st.pop())
                .ok_or_else(|| gate("a deactivation with nothing active", false))?;
            bars.push((lane_c + 1 + depth as i32, top, bot));
        }
    }
    for (p, stack) in &open {
        let c = lane[*p];
        for (top, depth) in stack {
            // never deactivated: mermaid runs the bar down to the foot
            // box — and past any note written after the last message
            bars.push((c + 1 + *depth as i32, *top, foot_top - 1));
        }
    }
    // statement rows bound every gap: a momentary bar must fit strictly
    // before whatever the source wrote next, never after it (#3c, 5)
    let mut bounds: Vec<i32> = msg_rows.clone();
    bounds.extend(note_rects.iter().map(|r| r.y0));
    // walls, floors and dividers close a gap too: a bar opened before an
    // `else` belongs to the branch above it, never below the divider
    // (review #3d, 3)
    for (_, f) in &frame_rows {
        bounds.push(f.y0);
        bounds.push(f.y1);
        bounds.extend(f.lines.iter().copied());
    }
    bounds.push(foot_top);
    for (col, top, bot) in bars {
        if lane.values().any(|&l| l == col) || col >= cols as i32 - 1 {
            return Err(gate(
                "no column free beside the lane for an activation",
                true,
            ));
        }
        for (fi, f) in &frame_rows {
            let _ = fi;
            if (f.x0 == col || f.x1 == col) && f.y0 <= bot && top <= f.y1 {
                return Err(gate("an activation bar would sit on a frame wall", true));
            }
        }
        if top > bot {
            let a = top - 1;
            let bound = bounds
                .iter()
                .copied()
                .filter(|&r| r > a)
                .min()
                .unwrap_or(rows as i32);
            let mut placed = false;
            for y in top..bound {
                if !used.contains(&y)
                    && !sacred.contains(&(col, y))
                    && g.get(col, y).is_some_and(|c| c.owner <= FRAME)
                {
                    g.put(col, y, bar, FRAME);
                    placed = true;
                    break;
                }
            }
            if !placed {
                return Err(gate("no row free for a momentary activation", true));
            }
            continue;
        }
        let mut seen = 0;
        for y in top..=bot {
            if sacred.contains(&(col, y)) {
                let ch = g.get(col, y).map(|c| c.ch).unwrap_or(' ');
                // horizontal walls join with the bar; a vertical wall,
                // a corner or a title character already reads as it is
                if !matches!(
                    ch,
                    '\u{2502}'
                        | '\u{2551}'
                        | '!'
                        | ' '
                        | '\u{250c}'
                        | '\u{2510}'
                        | '\u{2514}'
                        | '\u{2518}'
                ) && !ch.is_alphanumeric()
                {
                    // the junction joins the wall honestly, but it is
                    // not a bar STROKE: only `║` cells count towards
                    // "a reader can see this activation" (review #3d)
                    g.put(col, y, cross, FRAME);
                } else if ch == '\u{2551}' {
                    seen += 1;
                }
            } else {
                g.put(col, y, bar, FRAME);
                if g.get(col, y).is_some_and(|c| c.ch == bar) {
                    seen += 1;
                }
            }
        }
        // a bar every note covered would draw an activation the reader
        // cannot see, while mermaid keeps a stub in view: refusing beats
        // a picture whose answer to "is B active?" is wrong (review #3d, 1)
        if seen == 0 {
            return Err(gate("an activation no reader could see", true));
        }
    }

    // 6. order honesty: every line stands at the height the source put
    // it. A slide that inverted two events, or split one px height into
    // two rows, draws a sequence nobody wrote (review #3, B1/B2). The
    // ledger spans MESSAGE lines: frame walls and dividers keep their
    // own monotone claim (claim_row refuses equal-px collisions there),
    // and activation bars only span already-claimed rows, so nothing
    // outside this list can slip past the gate
    lines_px.sort_by(|a, b| a.0.total_cmp(&b.0));
    for pair in lines_px.windows(2) {
        let ((pa, ra), (pb, rb)) = (pair[0], pair[1]);
        if pa == pb && ra != rb {
            return Err(gate("one event height split across two rows", true));
        }
        if pa < pb && ra >= rb {
            return Err(gate("a slide inverted the order of events", true));
        }
    }

    check_survival(&g, &placed)?;
    emit(g, plain)
}

/// a pie is two numbers per slice and a name: the honest text form is the
/// list itself (title, label, share). Percentages show alongside the raw
/// value so rounding that does not add to 100 cannot mislead.
fn pie_list(src: &str, layout: &Layout, cols: usize) -> Result<Vec<String>, Fail> {
    let DiagramData::Pie(pie) = &layout.diagram else {
        return Err(gate("pie without pie data", false));
    };
    if pie.slices.is_empty() {
        return Err(gate("pie data shape not understood", false));
    }
    // the renderer drops slices too thin to see (1 in 100000 vanishes):
    // every written slice must reach the list or the chart silently lies
    // with a rounded-away category (review #3, B5)
    let mut written: Vec<(&str, f32, &str)> = Vec::new();
    for raw in src.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        // headers first: mermaid reads `pie`/`showData`/`title` lines as
        // chart furniture, so `pie title Sales: Q1` must not be mistaken
        // for a slice line (over-refusal, review #3c note)
        let head = line.split_whitespace().next().unwrap_or("");
        if head == "pie" {
            // the header carries only `showData` and `title ..`: a slice
            // written on it (`pie "A" : 5`) is dropped by the crate and
            // every share recomputed (review #3n, 8)
            let rest = line["pie".len()..].trim_start();
            let rest = rest
                .strip_prefix("showData")
                .map(str::trim_start)
                .unwrap_or(rest);
            let titled = rest
                .strip_prefix("title")
                .is_some_and(|t| t.is_empty() || t.starts_with(char::is_whitespace));
            if !(rest.is_empty() || titled) {
                return Err(gate("a pie line the parser may not understand", false));
            }
            continue;
        }
        if line == "showData" || head == "title" {
            continue;
        }
        // any other line is a slice: mermaid rejects one without `:`, and
        // skipping it recomputed every share without it (review #3m, 3)
        let Some((label, value)) = line.rsplit_once(':') else {
            return Err(gate("a pie line the parser may not understand", false));
        };
        let vtext = value.trim();
        let Ok(value) = vtext.parse::<f32>() else {
            return Err(gate("a pie line the parser may not understand", false));
        };
        if !value.is_finite() || value < 0.0 {
            return Err(gate("a pie line the parser may not understand", false));
        }
        let label = label.trim().trim_matches('"');
        if label.is_empty() {
            return Err(gate("a pie line the parser may not understand", false));
        }
        if written.iter().any(|(l, _, _)| *l == label) {
            return Err(gate(
                "a pie label the chart would merge into one slice",
                false,
            ));
        }
        written.push((label, value, vtext));
    }
    if written.len() != pie.slices.len() {
        return Err(gate("the pie lost a written slice", false));
    }
    let mut drawn: Vec<(String, f32)> = pie
        .slices
        .iter()
        .map(|sl| (sl.label.lines.join(" ").trim().to_string(), sl.value))
        .collect();
    written.sort_by(|a, b| a.0.cmp(b.0).then(a.1.total_cmp(&b.1)));
    let mut pool = written.clone();
    drawn.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    if written.iter().zip(&drawn).any(|((lw, vw, _), (ld, vd))| {
        *lw != ld.as_str() || vw.total_cmp(vd) != std::cmp::Ordering::Equal
    }) {
        return Err(gate("the pie renamed or resized a slice", false));
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
    // one row per slice, read from the slice itself: the crate sorts the
    // legend independently, so zipping slices against legend entries pairs
    // wrong names with wrong values (review #3, B5). The raw value rides
    // along so rounded shares stay honest, exactly as `showData` shows it
    for sl in &pie.slices {
        if sl.value as f64 / total * 100.0 > 100.5 || sl.label.lines.len() > 1 {
            return Err(gate("pie entry not drawable as text", false));
        }
        let label = sl.label.lines.join(" ");
        let label = label.trim();
        if !printable(label) || label.is_empty() {
            return Err(gate("pie label is not drawable", false));
        }
        let pct = sl.value as f64 / total * 100.0;
        // a non-zero slice must never display as zero
        let share = if pct > 0.0 && pct < 0.05 {
            "<0.1%".to_string()
        } else {
            format!("{pct:.1}%")
        };
        // print the number exactly as the source wrote it: parity was
        // proven per (label, value), so popping a matching entry can
        // only return this slice's own text (B5, review #3b)
        let key = (label.to_string(), sl.value);
        let Some(at) = pool.iter().position(|(l, v, _)| {
            *l == key.0.as_str() && v.total_cmp(&key.1) == std::cmp::Ordering::Equal
        }) else {
            return Err(gate("the pie renamed or resized a slice", false));
        };
        let vtext = pool.swap_remove(at).2;
        let line = format!("  {label}  [{vtext}]  {share}");
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
