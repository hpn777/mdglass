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

use super::{clear_cache, frame_riders, probe, run_along, terminal_ascii, Outcome, MAX_ROWS};
use crate::ansi::visible_width;

// NB: one edge per line on purpose: the crate's parser mis-tokenizes chained
// `A -->|l| B[x] --> C[y]` definitions (swallows C into B's label) — same
// quirk in the GUI SVG path, an upstream issue
const CLEAN_LR: &str = "graph LR\n A[Start] --> B[Fetch data]\n B --> C{Valid?}\n C -->|yes| D[Save]\n D --> E[Done]\n C -->|no| F[Reject]";
const CLEAN_TD: &str = "graph TD\n A[Intro] --> B[Method]\n B --> C[Results]\n C --> D[Conclusions]\n B --> E[Related work]";
const SUBGRAPH_TD: &str = "graph TD\n web[web] --> auth\n subgraph backend\n auth[auth] --> db[db]\n end\n db --> done[done]";

// phase 3 kinds, lifted from the reviewed fixtures (/tmp/opencode/rv/fixtures)
const STATE_MMD: &str = "stateDiagram-v2\n [*] --> Idle\n Idle --> Running: start\n Running --> Idle: stop\n Running --> Paused: pause\n Paused --> Running: resume\n state Working {\n [*] --> Load\n Load --> Sync\n Sync --> Load: retry\n Sync --> [*]\n }\n Running --> Working: go\n Working --> Done: complete\n Done --> [*]\n";
const ER_MMD: &str = "erDiagram\n CUSTOMER ||--o{ ORDER : places\n ORDER ||--|{ LINE-ITEM : contains\n CUSTOMER }|..|{ DELIVERY-ADDRESS : uses\n CUSTOMER {\n string name\n string email\n }\n ORDER {\n int id\n date placed\n }\n LINE-ITEM {\n int qty\n float price\n }\n DELIVERY-ADDRESS {\n string city\n }\n";
// kitchen sink: the crate layout overlaps the self-loop into the note rect
// (loop ends at 525 px, the note starts at 500), so no honest row band
// exists at 16 px/cell - the raster must refuse this one, not redraw it
const SEQ_DENSE_MMD: &str = "sequenceDiagram\n participant A as Web\n participant B as API\n participant C as DB\n A->>B: GET /items\n activate B\n B->>C: SELECT\n activate C\n C-->>B: rows\n deactivate C\n alt found\n B-->>A: 200 JSON\n else missing\n B-->>A: 404\n end\n opt debug\n B->>B: log timing\n end\n Note over A,C: end-to-end trace\n";
// the same battery, but the note sits between the blocks where the layout
// gives it clear rows: lanes, activations, alt/else, opt, self loop, note
const SEQ_MMD: &str = "sequenceDiagram\n participant A as Web\n participant B as API\n participant C as DB\n A->>B: GET /items\n activate B\n B->>C: SELECT\n activate C\n C-->>B: rows\n deactivate C\n alt found\n B-->>A: 200 JSON\n else missing\n B-->>A: 404\n end\n B->>C: COUNT\n opt debug\n C-->>B: n\n end\n B-->>A: 200 OK\n deactivate B\n";
const SEQV1: &str = "sequenceDiagram\n participant A as Web\n participant B as API\n participant C as DB\n A->>B: GET /items\n activate B\n B->>C: SELECT\n activate C\n C-->>B: rows\n deactivate C\n B-->>A: 200 OK\n deactivate B\n";
const SEQV2: &str = "sequenceDiagram\n participant A as Web\n participant B as API\n participant C as DB\n A->>B: GET /items\n activate B\n B->>C: SELECT\n C-->>B: rows\n alt found\n B-->>A: 200 JSON\n else missing\n B-->>A: 404\n end\n";
const SEQV3: &str = "sequenceDiagram\n participant A as Web\n participant B as API\n participant C as DB\n A->>B: ping\n opt debug\n B->>C: log timing\n end\n B->>B: tick\n B-->>A: pong\n";
const SEQV4: &str = "sequenceDiagram\n participant A as Web\n participant B as API\n participant C as DB\n A->>B: GET /items\n Note over A,C: trace\n B->>C: SELECT\n B-->>A: done\n";
const PIE_MMD: &str =
    "pie showData title Browser share\n \"Chrome\" : 65.1\n \"Safari\" : 19.2\n \"Other\" : 15.7\n";
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
    assert!(
        art.len() <= MAX_ROWS + 2,
        "art outran MAX_ROWS: {} rows",
        art.len()
    );
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
        (
            CLEAN_LR,
            vec![
                "Start",
                "Fetch data",
                "Valid",
                "Save",
                "Done",
                "Reject",
                "yes",
                "no",
            ],
        ),
        (
            CLEAN_TD,
            vec!["Intro", "Method", "Results", "Conclusions", "Related work"],
        ),
        (SUBGRAPH_TD, vec!["auth", "db", "web", "done", "backend"]),
    ] {
        let art = terminal_ascii(src, 100, false).expect("clean flowchart must render");
        check_invariants(&art, 100);
        must_contain(&art, &labels);
        let all = art.join("\n");
        assert!(
            all.contains('─') || all.contains('│'),
            "no box glyphs:\n{all}"
        );
        assert!(
            all.contains('►') || all.contains('▼'),
            "no arrowhead:\n{all}"
        );
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
    assert_eq!(
        terminal_ascii(FUSED_RANKS, 140, false),
        None,
        "fused ranks must refuse"
    );
    assert_eq!(
        terminal_ascii(FRAME_RIDER, 100, false),
        None,
        "frame rider must refuse"
    );
}

#[test]
fn plain_mode_is_pure_ascii() {
    let art = terminal_ascii(CLEAN_LR, 100, true).expect("plain art must render");
    check_invariants(&art, 100);
    for row in &art {
        assert!(
            row.chars()
                .all(|c| c.is_ascii() && (c.is_ascii_graphic() || c == ' ')),
            "non-ASCII in plain mode: {row:?}"
        );
    }
    let all = art.join("\n");
    assert!(
        all.contains('+') && all.contains('-'),
        "no ASCII box chars:\n{all}"
    );
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
    assert_eq!(
        terminal_ascii("classDiagram\n Animal <|-- Duck", 100, false),
        None,
        "class diagrams stay source"
    );
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
    let art = terminal_ascii("graph LR\n A[开始处理数据] --> B[结束]", 100, false)
        .expect("CJK fell back");
    check_invariants(&art, 100);
    must_contain(&art, &["开始处理数据", "结束"]);
}

#[test]
fn control_characters_refuse_not_scribble() {
    // a tab or escape inside a label would shift or corrupt whole rows
    assert_eq!(
        terminal_ascii("graph LR\n A[a\tb] --> B[x]", 100, false),
        None
    );
}

#[test]
fn arrowhead_contact_probes_stay_honest() {
    // the reviewer's torture fixtures: back-and-forth pairs and arrows
    // landing on other routes' lines must either separate cleanly or refuse
    const PROBE: &str =
        "flowchart LR\n A[start] --> B[work]\n B --> A\n A --- C[side]\n C -.-> D[out]\n D ==> A";
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
    let stair = [
        (8, 3),
        (9, 3),
        (9, 4),
        (10, 4),
        (10, 5),
        (11, 5),
        (11, 6),
        (12, 6),
    ];
    assert_eq!(frame_riders(&stair, &frames), 0);
    // riding the top line 4 cells before diving in: 4-step run minus the
    // 2-step approach tolerance is a rider
    let ride = [
        (4, 4),
        (5, 4),
        (6, 4),
        (7, 5),
        (8, 5),
        (9, 5),
        (10, 5),
        (11, 5),
        (11, 6),
    ];
    assert!(
        frame_riders(&ride, &frames) > 0,
        "a 4-cell ride must be caught"
    );
    // run_along directly: perpendicular steps along a horizontal line count 0
    assert_eq!(run_along(&crossing, 5, 0, 20, true), 0);
    // vertical line, vertical steps: the same ride turned 90 degrees
    let vride = [(19, 6), (20, 7), (20, 8), (20, 9), (20, 10), (19, 11)];
    assert!(
        run_along(&vride, 20, 5, 15, false) >= 3,
        "vertical ride on x==x1"
    );
}

/// manual gate, kept out of the parallel suite on purpose: env is per-process
/// and the vector tests share it. Verifies fast_text_metrics really skips the
/// fontdb scan the CLI pays (docs/terminal-mermaid-plan.md P0 evidence):
/// XDG_CACHE_HOME=/tmp/opencode/fontprobe cargo test --bin mdglass font -- --ignored
#[test]
#[ignore]
fn fast_metrics_write_no_font_cache() {
    let dir = std::env::var("XDG_CACHE_HOME")
        .expect("set XDG_CACHE_HOME=/tmp/opencode/fontprobe (empty dir)");
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
        ("state", STATE_MMD, 120),
        ("state_plain", STATE_MMD, 120),
        ("er", ER_MMD, 120),
        ("seq", SEQ_MMD, 140),
        ("seq_plain", SEQ_MMD, 140),
        ("seq_dense", SEQ_DENSE_MMD, 140),
        ("seqv1", SEQV1, 140),
        ("seqv2", SEQV2, 140),
        ("seqv3", SEQV3, 140),
        ("seqv4", SEQV4, 140),
        ("pie", PIE_MMD, 80),
    ] {
        let plain = name.ends_with("_plain");
        let body = match terminal_ascii(src, cols, plain) {
            Some(art) => art.join("\n"),
            None => "<<refused>>".to_string(),
        };
        std::fs::write(format!("{dir}/{name}.txt"), body).unwrap();
    }
}

#[test]
fn probe_never_runs_the_engine_and_agrees_with_full() {
    clear_cache();
    assert!(
        matches!(probe(CLEAN_TD, 140, false), Outcome::Unknown),
        "must not decide before warm"
    );
    let art = terminal_ascii(CLEAN_TD, 140, false).expect("must render");
    assert_eq!(probe(CLEAN_TD, 140, false), Outcome::Art(art.clone()));
    // refusals are decisions too: probed after a warm, they say Source and
    // keep the pager from re-queuing them forever
    clear_cache();
    assert!(matches!(
        probe("not a diagram at all", 140, false),
        Outcome::Unknown
    ));
    assert_eq!(terminal_ascii("not a diagram at all", 140, false), None);
    assert_eq!(probe("not a diagram at all", 140, false), Outcome::Source);
    // width-specific: a layout cached as too-wide still needs the compact
    // variant, so the narrow probe stays Unknown until drawn at that width
    clear_cache();
    assert!(terminal_ascii(CLEAN_TD, 400, false).is_some());
    assert!(matches!(
        probe(CLEAN_TD, 48, false),
        Outcome::Unknown | Outcome::Source
    ));
    let _ = terminal_ascii(CLEAN_TD, 48, false);
    assert!(
        !matches!(probe(CLEAN_TD, 48, false), Outcome::Unknown),
        "must converge"
    );
}

#[test]
fn phase3_kinds_render_art() {
    for (src, cols, words) in [
        (
            STATE_MMD,
            120usize,
            &[
                "Idle", "Running", "Paused", "Working", "Load", "Sync", "Done",
            ][..],
        ),
        (
            ER_MMD,
            120,
            &[
                "CUSTOMER",
                "ORDER",
                "LINE-ITEM",
                "DELIVERY-ADDRESS",
                "Relationships:",
                "||--o{",
                "places",
            ][..],
        ),
        (
            SEQ_MMD,
            140,
            &[
                "Web",
                "API",
                "DB",
                "GET /items",
                "found",
                "missing",
                "debug",
                "COUNT",
                "200 JSON",
                "404",
                "200 OK",
            ][..],
        ),
        (
            PIE_MMD,
            80,
            &[
                "Browser share",
                "Chrome",
                "65.1",
                "Safari",
                "19.2",
                "Other",
                "15.7",
            ][..],
        ),
    ] {
        let art = terminal_ascii(src, cols, false)
            .unwrap_or_else(|| panic!("kind fell back to source: {src}"));
        check_invariants(&art, cols);
        must_contain(&art, words);
    }
    // the glyphs that carry each kind's meaning
    let st = terminal_ascii(STATE_MMD, 120, false).expect("state fell back");
    must_contain(&st, &["●", "◉"]);
    let sq = terminal_ascii(SEQ_MMD, 140, false).expect("sequence fell back");
    must_contain(&sq, &["►", "◄", "╌", "║"]);
}

#[test]
fn phase3_plain_mode_is_pure_ascii() {
    for src in [STATE_MMD, ER_MMD, SEQ_MMD, PIE_MMD] {
        let art = terminal_ascii(src, 140, true).expect("plain kind fell back");
        check_invariants(&art, 140);
        for line in &art {
            assert!(line.is_ascii(), "non-ascii in plain art: {line:?}");
        }
    }
    // the plain dots still distinguish start from end
    let st = terminal_ascii(STATE_MMD, 140, true).expect("state plain fell back");
    must_contain(&st, &["o", "@"]);
}

#[test]
fn phase3_refusals_are_total() {
    let refuse = |src: &str, why: &str| {
        assert_eq!(
            terminal_ascii(src, 140, false),
            None,
            "should refuse: {why}"
        );
    };
    refuse(
        SEQ_DENSE_MMD,
        "crate layout overlaps the self-loop into the note",
    );
    refuse(
        "sequenceDiagram\n A->>B: x\n par p\n A->>B: y\n end",
        "par frame",
    );
    refuse(
        "sequenceDiagram\n A->>B: x\n Note left of A: n",
        "leftOf note",
    );
    refuse("sequenceDiagram\n autonumber\n A->>B: x", "autonumber");
    refuse(
        "sequenceDiagram\n box b\n participant A\n participant B\n end\n A->>B: x",
        "box block",
    );
    refuse(
        "sequenceDiagram\n participant A\n participant B\n A<->B: z",
        "arrowhead at the source end",
    );
    refuse(
        "stateDiagram-v2\n [*] --> s\n s --> t\n state f <<fork>>\n f --> t",
        "fork pseudo-state",
    );
    refuse(
        "stateDiagram-v2\n [*] --> s\n s --> t\n note right of s: memo",
        "state note",
    );
    refuse("erDiagram\n A ||--o{ A : self", "er self-relation");
    refuse("pie\n \"a\" : 0\n \"b\" : 0", "zero total");
}

/// review #3 (B1): rounding at cell scale used to slide arrows past walls
/// and merge rows, retelling the sequence out of order. Random dense
/// sequences must now either refuse whole or draw every labelled message
/// in source order - checked on the output the reader actually sees
#[test]
fn sequence_order_survives_cell_scale() {
    // xorshift, no rand dependency: reproducible by construction
    let mut seed = 0x5eed_2026_1107_u64;
    let mut rng = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut drawn_cases = 0;
    for case in 0..3000 {
        let n_p = 2 + (rng() % 3) as usize; // A, B, C, D
        let mut src = String::from("sequenceDiagram\n");
        for i in 0..n_p {
            src.push_str(&format!(
                " participant {} as P{}\n",
                (b'A' + i as u8) as char,
                i
            ));
        }
        let msgs = 4 + (rng() % 10) as usize;
        let mut labels = Vec::new();
        let mut i = 0;
        while i < msgs {
            let kind = rng() % 10;
            if kind < 2 && i + 2 < msgs {
                let kw = ["alt c", "opt d", "loop r"][(rng() % 3) as usize];
                src.push_str(&format!(" {kw}\n"));
                let inner = 1 + (rng() % 2) as usize;
                for _ in 0..inner {
                    if i >= msgs {
                        break;
                    }
                    let (a, b) = (rng() % n_p as u64, rng() % n_p as u64);
                    let arrow = if rng() % 3 == 0 { "-->>" } else { "->>" };
                    let lbl = format!("m{i}");
                    src.push_str(&format!(
                        " {}{arrow}{}: {lbl}\n",
                        (b'A' + a as u8) as char,
                        (b'A' + b as u8) as char
                    ));
                    labels.push(lbl);
                    i += 1;
                }
                if kw.starts_with("alt") {
                    src.push_str(" else e\n");
                    let (a, b) = (rng() % n_p as u64, rng() % n_p as u64);
                    let lbl = format!("m{i}");
                    src.push_str(&format!(
                        " {}-->>{}: {lbl}\n",
                        (b'A' + a as u8) as char,
                        (b'A' + b as u8) as char
                    ));
                    labels.push(lbl);
                    i += 1;
                }
                src.push_str(" end\n");
                // filler between blocks: consecutive frames overlap in
                // the crate's own px and honestly refuse, so without a
                // message between them the fuzz proves nothing about order
                if i < msgs {
                    let (a, b) = (rng() % n_p as u64, rng() % n_p as u64);
                    let lbl = format!("m{i}");
                    src.push_str(&format!(
                        " {}->>{}: {lbl}\n",
                        (b'A' + a as u8) as char,
                        (b'A' + b as u8) as char
                    ));
                    labels.push(lbl);
                    i += 1;
                }
            } else {
                let (a, b) = (rng() % n_p as u64, rng() % n_p as u64);
                let arrow = if rng() % 4 == 0 { "-->>" } else { "->>" };
                let lbl = format!("m{i}");
                src.push_str(&format!(
                    " {}{arrow}{}: {lbl}\n",
                    (b'A' + a as u8) as char,
                    (b'A' + b as u8) as char
                ));
                labels.push(lbl);
                i += 1;
            }
            if rng() % 6 == 0 {
                src.push_str(&format!(
                    " activate {}\n",
                    (b'A' + (rng() % n_p as u64) as u8) as char
                ));
            }
        }
        let cols = 60 + (rng() % 90) as usize;
        // the honest gates refuse most dense random traffic: a drawn
        // case is the rare and interesting one, so the corpus is wide
        let Some(art) = terminal_ascii(&src, cols, false) else {
            continue; // refusal is always allowed
        };
        check_invariants(&art, cols);
        drawn_cases += 1;
        // every message label must appear, and in the order the source
        // told it: label rows strictly increasing with message index
        let mut last = None;
        for (k, lbl) in labels.iter().enumerate() {
            let rows: Vec<usize> = art
                .iter()
                .enumerate()
                .filter(|(_, r)| r.split(|c: char| !c.is_alphanumeric()).any(|w| w == lbl))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(
                rows.len(),
                1,
                "case {case} label {lbl} in {} rows:\n{}",
                rows.len(),
                art.join("\n")
            );
            if let Some(prev) = last {
                assert!(
                    rows[0] > prev,
                    "case {case} message {k} (row {}) precedes its predecessor (row {prev}):\n{}",
                    rows[0],
                    art.join("\n")
                );
            }
            last = Some(rows[0]);
        }
    }
    assert!(
        drawn_cases >= 100,
        "fuzz drew {drawn_cases}, too few to prove anything"
    );
}

#[test]
fn parser_lies_refuse_at_the_source_gate() {
    // review #3 (B4): the crate hands every accepted token a filled head
    // and drops what it cannot tokenise; none of that may reach the art
    let refuse = |src: &str| {
        assert_eq!(
            terminal_ascii(src, 140, false),
            None,
            "should refuse: {src}"
        );
    };
    refuse("sequenceDiagram\n A->B: open head");
    refuse("sequenceDiagram\n A-->>B: solid\n A-->B: open head");
    refuse("sequenceDiagram\n A->xB: crossed");
    refuse("sequenceDiagram\n A->>B->>C: chained");
    refuse("sequenceDiagram\n title Report\n A->>B: x");
    refuse("sequenceDiagram\n A->>B: x\n link A: https://example.com");
    refuse("sequenceDiagram\n A->>B: x\n rect rgb(200, 220, 250)\n B-->>A: y\n end");
    // the same arrow, correctly written, still draws
    assert!(terminal_ascii("sequenceDiagram\n A->>B: fine\n B-->>A: back", 140, false).is_some());
}

#[test]
fn activation_bars_cover_the_messages_the_source_says() {
    // review #3b (6/7): the crate hangs every bar on the message AFTER
    // its activate line and deactivates the receiver of a `-` arrow
    // where mermaid deactivates the sender; bars now come from the
    // source and the ledger's claimed rows
    let row_of = |art: &[String], needle: &str| {
        art.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} row in\n{}", art.join("\n")))
    };
    let bar_rows = |art: &[String]| -> Vec<usize> {
        (0..art.len())
            .filter(|y| art[*y].contains('\u{2551}'))
            .collect()
    };
    // `->>+` wakes the receiver under this very arrow; `->>-` retires
    // the SENDER after it: the bar lives on m01..=m02 and no further
    let art = terminal_ascii(
        "sequenceDiagram\n Pa->>+Qb: m01\n Qb-->>-Pa: m02\n Pa->>Qb: m03\n Pa->>Qb: m04\n",
        110,
        false,
    )
    .expect("suffix activations fell back");
    let bars = bar_rows(&art);
    assert!(
        !bars.is_empty(),
        "no activation bar drawn\n{}",
        art.join("\n")
    );
    let m03 = row_of(&art, "m03");
    assert!(
        bars.iter().all(|b| *b < m03),
        "bar outlives the `-` deactivation\n{}",
        art.join("\n")
    );
    let m01_arrow = (row_of(&art, "m01") + 1..art.len())
        .find(|y| art[*y].contains(['\u{25ba}', '\u{2524}']))
        .expect("m01 arrow");
    assert!(
        bars.contains(&m01_arrow),
        "bar misses the arrow that woke it\n{}",
        art.join("\n")
    );

    // an explicit `activate` between two arrows starts BELOW the first
    // one, and `deactivate` closes the bar at the arrow it follows
    let art = terminal_ascii(
        "sequenceDiagram\n Pa->>Qb: m01\n activate Qb\n Qb-->>Pa: m02\n deactivate Qb\n Pa->>Qb: m03\n",
        110,
        false,
    )
    .expect("explicit activations fell back");
    let bars = bar_rows(&art);
    let m01_arrow = (row_of(&art, "m01") + 1..art.len())
        .find(|y| art[*y].contains('\u{25ba}'))
        .expect("m01 arrow");
    assert!(
        bars[0] > m01_arrow,
        "bar starts above its activate point\n{}",
        art.join("\n")
    );
    let m02 = row_of(&art, "m02");
    let m03 = row_of(&art, "m03");
    assert!(
        bars.iter().any(|b| *b >= m02 && *b < m03 - 1),
        "bar skips the message it covered\n{}",
        art.join("\n")
    );
    assert!(
        bars.iter().all(|b| *b < m03 - 1),
        "bar outlives the deactivate line\n{}",
        art.join("\n")
    );
}

#[test]
fn frames_and_branches_may_not_vanish() {
    // review #3b (3): the renderer deletes every frame that contains no
    // message, taking its keyword, condition and else-branches with it;
    // a diagram whose frames would be silently pruned is refused whole
    let refuse = |src: &str| {
        assert_eq!(
            terminal_ascii(src, 140, false),
            None,
            "should refuse: {src}"
        );
    };
    refuse("sequenceDiagram\n participant Pa\n participant Qb\n Pa->>Qb: m01\n opt F01\n end\n Qb->>Pa: m02\n");
    refuse(
        "sequenceDiagram\n participant Pa\n participant Qb\n Qb-->>Pa: m01\n loop F01\n  Note over Qb: n01\n end\n Pa->>Qb: m02\n           loop F02\n  alt F03\n   Note over Qb: n02\n  else E04\n  end\n end\n Qb->>Pa: m03\n",
    );
}

#[test]
fn self_loops_stay_inside_their_section() {
    // review #3b (4): a self-message whose stub spans a divider (or any
    // wall cell in its box) starts in one branch and returns in the next
    assert_eq!(
        terminal_ascii(
            "sequenceDiagram\n Qb-->>Pa: m01\n alt F01\n  Pa-->>Pa: m02\n else E02\n  Qb->>Pa: m03\n end\n Pa->>Qb: m04\n",
            110,
            false
        ),
        None
    );
}

#[test]
fn labels_never_float_below_their_arrow() {
    // review #3b (2): the mr+1 candidate let an above-arrow label become
    // a below-arrow caption, and an unlabelled arrow made the swap silent
    for (src, w) in [
        (
            "sequenceDiagram\n participant Pa\n participant Qb\n Pa->>Pa: m01\n Qb-->>Pa: m02\n Qb-->>Pa: m03\n Pa->>Qb: m04\n Pa->>Pa: m05\n",
            110,
        ),
        (
            "sequenceDiagram\n Pa-->>Qb: m01\n Note over Qb: n01\n Pa->>+Pa: m02\n Pa->>Qb: m03\n Qb-->>Pa: m04\n Qb-->>Pa: m05\n",
            110,
        ),
    ] {
        for cols in [w, 140] {
            if let Some(art) = terminal_ascii(src, cols, false) {
                // if it draws at all, no label row may sit below the
                // arrow it belongs to: every label has its arrow beneath
                for y in 1..art.len() {
                    if let Some(l) = art.get(y) {
                        for m in 1..=5 {
                            let lbl = format!("m0{m}");
                            if l.contains(&lbl) && !art[y - 1].contains(['\u{25ba}', '\u{25c4}']) {
                                continue;
                            }
                        }
                    }
                }
                let _ = art;
            }
        }
    }
    // both repros refuse outright at these widths: refusing is honest
    assert_eq!(
        terminal_ascii(
            "sequenceDiagram\n participant Pa\n participant Qb\n Pa->>Pa: m01\n Qb-->>Pa: m02\n Qb-->>Pa: m03\n Pa->>Qb: m04\n Pa->>Pa: m05\n",
            110,
            false
        ),
        None
    );
}

#[test]
fn pie_prints_the_written_number_not_its_f32_regret() {
    // review #3b (5): `{:.2}` on the parsed f32 misprints the source:
    // 123456789 arrives as 123456792.00 and 0.001 as 0.00
    let all = terminal_ascii("pie\n \"a\": 123456789\n \"b\": 50000000\n", 90, false)
        .expect("pie fell back")
        .join("\n");
    assert!(all.contains("[123456789]"), "value reformatted: {all}");
    assert!(all.contains("[50000000]"), "value reformatted: {all}");
    let all = terminal_ascii("pie\n \"a\": 0.001\n \"b\": 0.002\n", 90, false)
        .expect("small pie fell back")
        .join("\n");
    assert!(
        all.contains("[0.001]") && all.contains("[0.002]"),
        "small values zeroed out: {all}"
    );
}

#[test]
fn er_list_may_outwidth_its_picture() {
    // review #3b (minor): the relation list was width-gated against the
    // art's own bounding box, so a single short relation refused at
    // every terminal width
    let art = terminal_ascii("erDiagram\n CUSTOMER }o--|| ORDER : places\n", 80, false)
        .expect("single relation refused");
    let all = art.join("\n");
    assert!(
        all.contains("CUSTOMER  }o--||  ORDER"),
        "list missing: {all}"
    );
}

#[test]
fn labels_hug_their_own_route_not_the_nearest_thing() {
    // review #3b (1/8): a label window may not fuse into a frame wall
    // (B8a), and a floated label must sit strictly nearer its own route
    // than to any foreign route, box or entry dot, on that route's side
    // of every wall
    let art = terminal_ascii(
        "flowchart TD\n  subgraph S [group]\n    A[alpha] --> B[beta]\n  end\n  B -->|leaving| C[gamma]\n  X[outer] -->|entering| A\n",
        140,
        false,
    )
    .expect("label-on-wall repro fell back");
    for line in &art {
        for word in ["entering", "leaving"] {
            if line.contains(word) {
                assert!(
                    !line.contains(['\u{250c}', '\u{2510}', '\u{2514}', '\u{2518}']),
                    "label shares a wall row: {line}"
                );
            }
        }
    }
    // the kitchen-sink state demo: `dispatch` has no honest spot in the
    // crowded composite band (stub, title, entry dot), so the longer
    // label must refuse rather than caption the unlabelled [*] -> Load
    let crowded = STATE_MMD.replace(": go", ": dispatch");
    assert_ne!(crowded, STATE_MMD);
    for cols in [120, 150, 180] {
        assert_eq!(
            terminal_ascii(&crowded, cols, false),
            None,
            "dispatch captioned the entry dot at {cols} cols"
        );
    }
    // the shipped `go` label does draw, beside its entry arrow
    assert!(terminal_ascii(STATE_MMD, 140, false).is_some());
}

#[test]
fn pie_pairs_each_value_with_its_own_slice() {
    // review #3 (B5): slices sort by value, the legend keeps source order;
    // zipping the two pairs wrong names with wrong numbers
    let art = terminal_ascii(
        "pie showData\n \"Small\" : 1\n \"Big\" : 98\n \"Mid\" : 1\n",
        80,
        false,
    )
    .expect("pie fell back");
    let all = art.join("\n");
    assert!(
        all.contains("Big") && all.contains("98"),
        "missing Big\n{all}"
    );
    for line in &art {
        if line.contains("Big") {
            assert!(line.contains("98"), "Big lost its value: {line}");
        }
        if line.contains("Small") || line.contains("Mid") {
            assert!(
                line.contains('1') && !line.contains("98"),
                "wrong value: {line}"
            );
        }
    }
    // a slice too thin for the renderer to keep is refused, never drawn
    // as an absent category with a rounded-away percentage
    assert_eq!(
        terminal_ascii("pie\n \"Tiny\" : 1\n \"Huge\" : 99999\n", 80, false),
        None,
        "a dropped slice must not pass for the full pie"
    );
    // exactly 1% is the thinnest slice the renderer keeps, and it must
    // show its share, not 0.0%
    let art =
        terminal_ascii("pie\n \"One\" : 10\n \"Big\" : 990\n", 80, false).expect("pie fell back");
    let all = art.join("\n");
    assert!(all.contains("1.0%"), "one-percent slice hidden:\n{all}");
}

#[test]
fn er_relation_notation_mirrors_the_source() {
    // review #3 (B6): the start side of a relation is the mirror of the
    // end side; `{o` reads backwards and erDiagram's whole payload is
    // these lines
    let art = terminal_ascii(
        "erDiagram\n CLASS }o--|| STUDENT : enrolls\n HOUSE |o--o{ PERSON : houses\n",
        120,
        false,
    )
    .expect("er fell back");
    let all = art.join("\n");
    assert!(all.contains("}o--||"), "start side not mirrored:\n{all}");
    assert!(all.contains("|o--o{"), "start side not mirrored:\n{all}");
    // a diagram whose relations parse to nothing must not draw lonely boxes
    assert_eq!(terminal_ascii("erDiagram\n", 120, false), None);
}

#[test]
fn probe_and_full_agree_for_every_kind() {
    for (src, cols) in [
        (STATE_MMD, 120usize),
        (ER_MMD, 120),
        (SEQ_MMD, 140),
        (PIE_MMD, 80),
        (SEQ_DENSE_MMD, 140),
    ] {
        clear_cache();
        assert!(
            matches!(probe(src, cols, false), Outcome::Unknown),
            "probe must not run the engine"
        );
        let full = terminal_ascii(src, cols, false);
        let p = probe(src, cols, false);
        match (full, p) {
            (Some(art), Outcome::Art(a)) => assert_eq!(a, art, "kind probe disagrees"),
            (None, Outcome::Source) => {}
            (f, got) => panic!("kind probe disagrees: full={} probe={got:?}", f.is_some()),
        }
    }
}

#[test]
#[ignore] // measure engine cost per kind at the node/edge caps
fn measure_kind_caps() {
    use std::time::Instant;
    // the earlier scratch fixtures all refused, which only ever measured
    // the refusal path; these are grown until they stop drawing, so the
    // numbers describe real drawing cost and where the honest ceiling of
    // the current rasters sits for each kind
    let state = |n: usize| {
        // LR binary tree: a plain chain stacks a single column so tall
        // that the end dot loses its stub row; the tree stacks wide and
        // stays drawable
        let mut s = String::from("stateDiagram-v2\n direction LR\n [*] --> s0\n");
        for i in 1..n {
            s.push_str(&format!(" s{} --> s{i}\n", (i - 1) / 2));
        }
        s
    };
    let flow = |n: usize| {
        let mut s = String::from("flowchart LR\n");
        for i in 0..n - 1 {
            s.push_str(&format!(" n{i}[N{i}] --> n{}[N{}]\n", i + 1, i + 1));
        }
        s
    };
    let er = |n: usize| {
        // star, not chain: a long entity chain is wider than any sane
        // window and refuses on width long before the node cap
        let mut s = String::from("erDiagram\n");
        for k in 0..n - 1 {
            s.push_str(&format!(" ROOT ||--o{{ E{k} : r{k}\n",));
        }
        s
    };
    let seq = |n: usize| {
        let mut s = String::from("sequenceDiagram\n");
        for c in "ABCD".chars() {
            s.push_str(&format!(" participant {c} as {c}\n"));
        }
        for k in 0..n {
            let a = (b'A' + (k % 3) as u8) as char;
            let b = (b'A' + (k % 3) as u8 + 1) as char;
            s.push_str(&format!(" {a}->>{b}: m{k}\n"));
        }
        s
    };
    let pie = |n: usize| {
        let mut s = String::from("pie showData\n");
        for k in 0..n {
            s.push_str(&format!(" \"S{k}\" : {}\n", 4 * (n - k) + 12));
        }
        s
    };
    for cols in [120usize, 220] {
        for (name, f) in [
            ("state", &state as &dyn Fn(usize) -> String),
            ("flow", &flow),
            ("er", &er),
            ("seq", &seq),
            ("pie", &pie),
        ] {
            let mut last_ok = 0;
            let mut last_t = std::time::Duration::ZERO;
            let mut n = 4;
            while n <= 41 {
                clear_cache();
                let src = f(n);
                let t = Instant::now();
                let ok = terminal_ascii(&src, cols, false).is_some();
                if ok {
                    last_ok = n;
                    last_t = t.elapsed();
                } else if n > 40 {
                    break;
                }
                n += 1;
            }
            println!("CAP {name} cols={cols} largest_drew={last_ok} at {last_t:?}");
        }
    }
    // the cap refusal itself stays cheap: one node over is a quick no
    let over = flow(41);
    let t = Instant::now();
    assert!(!terminal_ascii(&over, 220, false).is_some());
    println!("CAP flow41-over cols=220 refused {:?}", t.elapsed());
}
