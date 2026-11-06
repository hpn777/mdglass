//! Invariant tests for the box-art raster (docs/terminal-mermaid-plan.md).
//!
//! Art is layout-engine output; pinning its exact glyphs would make every
//! upstream dagre tweak a test failure. So the tests assert invariants
//! instead: labels are present (find must work), rows fit the width, plain
//! mode is pure ASCII, and refusals are total (None, never half-drawn art).
//! Two fixtures must keep rendering (the fan/back-edge and self/multi-edge
//! shapes — the engine may never regress them into silent fallback); the
//! illegible ones must keep refusing. `MDGLASS_BOXART_DUMP=/dir` writes each
//! fixture's art for eyeballing.

use super::{clear_cache, frame_riders, run_along, terminal_ascii, MAX_ROWS};
use crate::ansi::visible_width;

// NB: one edge per line on purpose: the crate's parser mis-tokenizes chained
// `A -->|l| B[x] --> C[y]` definitions (swallows C into B's label) — same
// quirk in the GUI SVG path, an upstream issue
const CLEAN_LR: &str = "graph LR\n A[Start] --> B[Fetch data]\n B --> C{Valid?}\n C -->|yes| D[Save]\n D --> E[Done]\n C -->|no| F[Reject]";
const CLEAN_TD: &str = "graph TD\n A[Intro] --> B[Method]\n B --> C[Results]\n C --> D[Conclusions]\n B --> E[Related work]";
const SUBGRAPH_TD: &str = "graph TD\n web[web] --> auth\n subgraph backend\n auth[auth] --> db[db]\n end\n db --> done[done]";
const FAN_BACK: &str = "flowchart LR\n A[fetch] --> P[parse]\n B[read] --> P\n C[stdin] --> P\n P --> O1[json]\n P --> O2[table]\n P --> O3[html]\n O3 --> A";
const MULTI_SELF: &str = "flowchart LR\n A[start] ==> B[work]\n A --> B\n A -.-> B\n B --> B";
const SEQUENCE: &str = "sequenceDiagram\n Alice->>Bob: hello\n Bob-->>Alice: hi";
const NOT_MMD: &str = "def main():\n    print('not mermaid')";
/// the frame-runner: dagre routes the return edge along the frame's bottom
/// border row at cell scale — the honest output is source
const FRAME_RIDER: &str = "graph LR\n subgraph backend\n SVC1[auth] --> SVC2[db]\n end\n CLI[cli] --> SVC1\n SVC2 --> CLI";
/// ranks fused at cell scale (px gap < one row): refuse, never fuse boxes
const FUSED_RANKS: &str = "flowchart TD\n A[Ingest] --> B{Valid?}\n B -->|yes| C[Parse]\n B -->|no| E[Reject]\n C --> D[Enrich]\n D --> F[Route]\n subgraph Processing\n F --> G[Transform]\n G -->|retry| G\n G --> H[Validate]\n H --> I[Store]\n end\n H -->|bad| J[Quarantine]\n J --> G\n I --> K[Index]\n K --> L[Notify]\n L --> M{Acked?}\n M -->|no| L\n M -->|yes| N[Archive]\n N --> O[Done]\n I --> O";

fn check_invariants(art: &[String], cols: usize) {
    for row in art {
        assert!(
            visible_width(row) <= cols,
            "row overflows {cols} cols: {row:?}"
        );
        assert!(!row.contains('\u{0}'), "unresolved wide-char placeholder");
        assert!(!row.ends_with(' '), "trailing space: {row:?}");
        assert!(
            row.chars().all(|c| !c.is_control()),
            "control character in output: {row:?}"
        );
    }
    assert!(!art.is_empty());
    assert!(art.len() <= MAX_ROWS + 2, "art outran MAX_ROWS: {} rows", art.len());
}

fn must_contain(art: &[String], words: &[&str]) {
    let all = art.join("\n");
    for w in words {
        assert!(all.contains(w), "label {w:?} eaten from art:\n{all}");
    }
}

#[test]
fn clean_flowcharts_render_art() {
    for (src, labels) in [
        (CLEAN_LR, vec!["Start", "Fetch data", "Valid", "Save", "Done", "Reject", "yes", "no"]),
        (CLEAN_TD, vec!["Intro", "Method", "Results", "Conclusions", "Related work"]),
        (SUBGRAPH_TD, vec!["auth", "db", "web", "done", "backend"]),
    ] {
        let art = terminal_ascii(src, 100, false).expect("clean flowchart must render");
        check_invariants(&art, 100);
        must_contain(&art, &labels);
        let all = art.join("\n");
        assert!(all.contains('─') || all.contains('│'), "no box glyphs:\n{all}");
        assert!(all.contains('►') || all.contains('▼'), "no arrowhead:\n{all}");
    }
}

#[test]
fn common_dirty_shapes_must_render() {
    // fan-in/out with a back edge, and self-loop with multi-edges: the
    // engine drawing these is the feature — a regression into fallback here
    // means some new gate swallowed a common idiom
    let art = terminal_ascii(FAN_BACK, 120, false).expect("fan-in/out + back edge fell back");
    check_invariants(&art, 120);
    must_contain(&art, &["fetch", "parse", "stdin", "json", "table", "html"]);
    let art = terminal_ascii(MULTI_SELF, 100, false).expect("self-loop/multi-edge fell back");
    check_invariants(&art, 100);
    must_contain(&art, &["start", "work"]);
}

#[test]
fn illegible_geometries_refuse_or_stay_honest() {
    // quality gate contract: either honest art with every label intact, or
    // plain None so the caller keeps the source — never mangled output
    for src in [FUSED_RANKS, FRAME_RIDER] {
        for cols in [90, 140] {
            if let Some(art) = terminal_ascii(src, cols, false) {
                check_invariants(&art, cols);
            }
        }
    }
    // the crate version is pinned, so these two geometries are stable:
    // if an upstream change ever lets them render, it must be re-reviewed,
    // not silently absorbed (a green test that hides wrong art is worse
    // than a red one)
    assert_eq!(terminal_ascii(FUSED_RANKS, 140, false), None, "fused ranks must refuse");
    assert_eq!(terminal_ascii(FRAME_RIDER, 100, false), None, "frame rider must refuse");
}

#[test]
fn plain_mode_is_pure_ascii() {
    let art = terminal_ascii(CLEAN_LR, 100, true).expect("plain art must render");
    check_invariants(&art, 100);
    for row in &art {
        assert!(
            row.chars().all(|c| c.is_ascii() && (c.is_ascii_graphic() || c == ' ')),
            "non-ASCII in plain mode: {row:?}"
        );
    }
    let all = art.join("\n");
    assert!(all.contains('+') && all.contains('-'), "no ASCII box chars:\n{all}");
}

#[test]
fn deterministic() {
    // same result twice, including across a cache wipe: art must come from
    // the layout, not from state a previous render left behind
    let a = terminal_ascii(FAN_BACK, 120, false);
    assert!(a.is_some());
    assert_eq!(a, terminal_ascii(FAN_BACK, 120, false));
    clear_cache();
    assert_eq!(a, terminal_ascii(FAN_BACK, 120, false));
}

#[test]
fn refusals_are_total() {
    assert_eq!(terminal_ascii(SEQUENCE, 100, false), None, "sequence is P3");
    assert_eq!(terminal_ascii(NOT_MMD, 100, false), None);
    assert_eq!(terminal_ascii("", 100, false), None);
    assert_eq!(terminal_ascii(CLEAN_LR, 47, false), None, "below MIN_COLS");
    // strict parse: an unbalanced subgraph draws nowhere in real mermaid
    assert_eq!(
        terminal_ascii("graph LR\n subgraph s\n A[one] --> B[two]", 100, false),
        None,
        "unbalanced subgraph must not render"
    );
    // long labels wrap in the layout engine: tall box, never eaten
    let wide = terminal_ascii(
        "graph LR\n A[this label is deliberately far longer than any forty column terminal could ever show] --> B[x]",
        50,
        false,
    )
    .expect("wrapping means no squish refusal");
    must_contain(&wide, &["deliberately"]);
}

#[test]
fn wide_labels_survive_narrow_widths() {
    // CJK labels: one char = two cells; the fit gate must use visible width
    let art = terminal_ascii("graph LR\n A[开始处理数据] --> B[结束]", 100, false).expect("CJK fell back");
    check_invariants(&art, 100);
    must_contain(&art, &["开始处理数据", "结束"]);
}

#[test]
fn control_characters_refuse_not_scribble() {
    // a tab or escape inside a label would shift or corrupt whole rows
    assert_eq!(terminal_ascii("graph LR\n A[a\tb] --> B[x]", 100, false), None);
}

#[test]
fn arrowhead_contact_probes_stay_honest() {
    // the reviewer's torture fixtures: back-and-forth pairs and arrows
    // landing on other routes' lines must either separate cleanly or refuse
    const PROBE: &str = "flowchart LR\n A[start] --> B[work]\n B --> A\n A --- C[side]\n C -.-> D[out]\n D ==> A";
    // the pinned crate routes this roomy-honest today: it must render, and
    // any future refusal is a deliberate re-review, not a silent regression
    let art = terminal_ascii(PROBE, 120, false).expect("probe fell back");
    check_invariants(&art, 120);
}

#[test]
fn frame_rider_detector_measures_runs_not_touches() {
    // extracted so the calibration is pinned: a perpendicular crossing, a
    // staircase entry and a one-step graze all pass; a 3-step ride fails
    let frames = vec![super::Rect {
        x0: 0,
        y0: 5,
        x1: 20,
        y1: 15,
    }];
    // straight vertical crossing of the top line: run along y==5 is 0
    let crossing = [(10, 3), (10, 4), (10, 5), (10, 6), (10, 7)];
    assert_eq!(frame_riders(&crossing, &frames), 0);
    // staircase across the top line: alternating steps, max run 1
    let stair = [(8, 3), (9, 3), (9, 4), (10, 4), (10, 5), (11, 5), (11, 6), (12, 6)];
    assert_eq!(frame_riders(&stair, &frames), 0);
    // riding the top line 4 cells before diving in: 4-step run minus the
    // 2-step approach tolerance is a rider
    let ride = [(4, 4), (5, 4), (6, 4), (7, 5), (8, 5), (9, 5), (10, 5), (11, 5), (11, 6)];
    assert!(frame_riders(&ride, &frames) > 0, "a 4-cell ride must be caught");
    // run_along directly: perpendicular steps along a horizontal line count 0
    assert_eq!(run_along(&crossing, 5, 0, 20, true), 0);
    // vertical line, vertical steps: the same ride turned 90 degrees
    let vride = [(19, 6), (20, 7), (20, 8), (20, 9), (20, 10), (19, 11)];
    assert!(run_along(&vride, 20, 5, 15, false) >= 3, "vertical ride on x==x1");
}

/// manual gate, kept out of the parallel suite on purpose: env is per-process
/// and the vector tests share it. Verifies fast_text_metrics really skips the
/// fontdb scan the CLI pays (docs/terminal-mermaid-plan.md P0 evidence):
/// XDG_CACHE_HOME=/tmp/opencode/fontprobe cargo test --bin mdglass font -- --ignored
#[test]
#[ignore]
fn fast_metrics_write_no_font_cache() {
    let dir = std::env::var("XDG_CACHE_HOME").expect("set XDG_CACHE_HOME=/tmp/opencode/fontprobe (empty dir)");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let art = terminal_ascii(CLEAN_TD, 100, false).expect("must render");
    check_invariants(&art, 100);
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(Result::ok).collect())
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "fast_text_metrics must not touch the font cache: {leftovers:?}",
    );
}

/// phase-0 feasibility eyeball: MDGLASS_BOXART_DUMP=/dir cargo test --bin mdglass dump -- --ignored
#[test]
#[ignore]
fn dump_fixtures_for_eyeballing() {
    let dir = std::env::var("MDGLASS_BOXART_DUMP").expect("set MDGLASS_BOXART_DUMP=/dir");
    for (name, src, cols) in [
        ("clean_lr", CLEAN_LR, 100),
        ("clean_td", CLEAN_TD, 100),
        ("subgraph_td", SUBGRAPH_TD, 100),
        ("fan_back", FAN_BACK, 120),
        ("multi_self", MULTI_SELF, 100),
        ("fused_ranks", FUSED_RANKS, 140),
        ("frame_rider", FRAME_RIDER, 100),
        ("large_plain", FUSED_RANKS, 140),
    ] {
        let plain = name.ends_with("_plain");
        let body = match terminal_ascii(src, cols, plain) {
            Some(art) => art.join("\n"),
            None => "<<refused>>".to_string(),
        };
        std::fs::write(format!("{dir}/{name}.txt"), body).unwrap();
    }
}
