// Pager tests. Input goes through `parse_bytes` exactly as terminal bytes
// would, so key-decoding mismatches (e.g. Enter arriving as CR) are caught.

use super::*;

fn pager(docs: &[&str], height: usize) -> Pager {
    pager_diag(docs, height, false)
}

/// Escape-free text of the current file, for content assertions.
fn shown(p: &Pager) -> String {
    strip_ansi(&p.docs[0].lines.join("\n"))
}

/// Run the idle warm ticks the real loop performs between keys, so every
/// pending diagram has been drawn (Pager::new deliberately never blocks on
/// the engine: a first frame must appear instantly).
fn warm(p: &mut Pager) {
    while p.warm_one() {}
}

fn pager_diag(docs: &[&str], height: usize, diagrams: bool) -> Pager {
    let files: Vec<(String, String)> =
        docs.iter().enumerate().map(|(i, d)| (format!("f{i}.md"), d.to_string())).collect();
    Pager::new(&files, true, true, diagrams, 60, height)
}

/// Feed raw bytes; returns false once the pager asks to exit.
fn feed(p: &mut Pager, bytes: &[u8]) -> bool {
    for e in parse_bytes(bytes) {
        if let Event::Key(k) = e {
            if !p.key(k) {
                return false;
            }
        }
    }
    true
}

fn top(p: &Pager) -> usize {
    p.docs[p.file].top
}

/// 80 paragraphs with "TARGET" in paragraphs 20, 40 and 60.
fn targets_doc() -> String {
    (0..80)
        .map(|i| if i % 20 == 0 && i > 0 { format!("para {i} TARGET") } else { format!("para {i}") })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn match_text(p: &Pager) -> String {
    p.docs[p.file].plain[p.last_match.expect("no match")].clone()
}

#[test]
fn enter_submits_search() {
    let mut p = pager(&[&targets_doc()], 20);
    assert!(feed(&mut p, b"/TaRgEt\r"));
    assert!(p.search.is_none(), "prompt still open after Enter");
    assert_eq!(match_text(&p), "para 20 target");
}

#[test]
fn n_advances_through_matches_and_wraps() {
    let mut p = pager(&[&targets_doc()], 20);
    feed(&mut p, b"/target\r");
    feed(&mut p, b"n");
    assert_eq!(match_text(&p), "para 40 target");
    feed(&mut p, b"n");
    assert_eq!(match_text(&p), "para 60 target");
    feed(&mut p, b"n");
    assert_eq!(match_text(&p), "para 20 target");
    assert!(p.msg.as_deref().unwrap_or("").contains("continuing at top"));
    feed(&mut p, b"N");
    assert_eq!(match_text(&p), "para 60 target");
}

#[test]
fn backward_search_and_not_found() {
    let mut p = pager(&[&targets_doc()], 20);
    feed(&mut p, b"G?target\r");
    assert_eq!(match_text(&p), "para 60 target");
    feed(&mut p, b"/zzz\r");
    assert!(p.msg.as_deref().unwrap_or("").contains("not found"));
}

#[test]
fn match_is_highlighted_in_frame() {
    let mut p = pager(&[&targets_doc()], 20);
    feed(&mut p, b"/target\r");
    // case-insensitive match, highlighted in the original case
    assert!(p.frame().contains("\u{1b}[7mT\u{1b}[27m\u{1b}[7mA\u{1b}[27m"));
}

#[test]
fn q_types_into_prompt_and_esc_cancels() {
    let mut p = pager(&[&targets_doc()], 20);
    assert!(feed(&mut p, b"/abq"));
    assert_eq!(p.search.as_ref().map(|s| s.0.as_str()), Some("abq"));
    assert!(feed(&mut p, b"\x1b"));
    assert!(p.search.is_none());
    assert!(!feed(&mut p, b"q"));
}

#[test]
fn bare_esc_quits_but_arrow_keys_do_not() {
    let mut p = pager(&[&targets_doc()], 20);
    assert!(feed(&mut p, b"\x1b[B\x1b[B"));
    assert_eq!(top(&p), 2);
    assert!(!feed(&mut p, b"\x1b"));
}

#[test]
fn paging_keys() {
    let mut p = pager(&[&targets_doc()], 11); // 10 content rows
    feed(&mut p, b"\x1b[6~");
    assert_eq!(top(&p), 10, "PageDown");
    feed(&mut p, b"f ");
    assert_eq!(top(&p), 30, "f and Space");
    feed(&mut p, b"b\x1b[5~");
    assert_eq!(top(&p), 10, "b and PageUp");
    feed(&mut p, b"d");
    assert_eq!(top(&p), 15, "d");
    feed(&mut p, b"u\x15");
    assert_eq!(top(&p), 5, "u and Ctrl-U");
    feed(&mut p, b"G");
    assert_eq!(top(&p), p.max_top());
    feed(&mut p, b"g");
    assert_eq!(top(&p), p.max_top(), "single g does nothing");
    feed(&mut p, b"g");
    assert_eq!(top(&p), 0, "gg");
    feed(&mut p, b"\x1b[F");
    assert_eq!(top(&p), p.max_top(), "End");
    feed(&mut p, b"\x1b[H");
    assert_eq!(top(&p), 0, "Home");
}

#[test]
fn frame_draws_from_home_with_one_row_per_line() {
    let p = pager(&[&targets_doc()], 12);
    let f = p.frame();
    assert!(f.starts_with("\u{1b}[H"));
    assert_eq!(f.matches("\r\n").count(), 11); // 11 content rows, then the status line
    assert!(!f.ends_with("\r\n"), "a trailing newline would scroll the screen");
    let status = f.rsplit("\r\n").next().unwrap();
    assert_eq!(visible_width(status), 59, "status leaves the last column empty");
}

#[test]
fn d_toggle_swaps_mermaid_art_and_source() {
    let doc = "```mermaid\ngraph LR\n A[alpha] --> B[omega]\n```\n";
    // off by default (pager()): source stays source even for valid mermaid
    let p = pager(&[doc], 24);
    let all = shown(&p);
    assert!(all.contains("graph") && all.contains("LR"), "source vanished with diagrams off");
    assert!(!all.contains('►'), "art shown while toggled off");
    // on: box art with its labels, source gone (after the idle warm ticks)
    let mut p = pager_diag(&[doc], 24, true);
    assert!(shown(&p).contains("graph"), "first frame blocked on the engine");
    assert!(p.warm_one(), "fence was not queued for warming");
    let all = shown(&p);
    assert!(all.contains('►') && all.contains("alpha") && all.contains("omega"), "no art:\n{all}");
    assert!(!all.contains("graph"), "source leaked beside the art");
    // D: back to source (highlight() splits tokens, so match words apart)
    feed(&mut p, b"D");
    let all = shown(&p);
    assert!(all.contains("graph") && all.contains("-->"), "D did not restore source");
    // D again: art returns (deterministic, cache or not)
    feed(&mut p, b"D");
    assert!(shown(&p).contains('►'), "D did not restore art");
    // deep scroll then toggle: art is shorter than source, and the view
    // must clamp at the new bottom instead of painting blank rows
    feed(&mut p, b"GG");
    p.docs[0].top = p.docs[0].lines.len() - 1;
    feed(&mut p, b"D");
    let vh = p.vh();
    assert!(p.docs[0].top <= p.docs[0].lines.len().saturating_sub(vh), "top outran shortened doc");
    assert!(shown(&p).contains("graph"), "D did not restore source");
    // back to art, then a resize must keep whichever mode the user chose
    feed(&mut p, b"D");
    assert!(shown(&p).contains('►'), "D did not restore art");
    assert!(p.frame().contains("omega"), "frame blanked: top not clamped after shortening");
    p.resize(80, 24);
    assert!(shown(&p).contains('►'), "resize lost the diagram mode");
}

#[test]
fn quoted_fences_warm_at_their_narrower_width() {
    let doc = "> ```mermaid\n> graph LR\n>  A[alpha] --> B[omega]\n> ```\n";
    let mut p = pager_diag(&[doc], 24, true);
    assert_eq!(p.warm_q.len(), 1, "quoted fence not queued");
    // the queue keys on the width render() will use: quotes narrow by two
    assert_eq!(p.warm_q[0].1, 58, "quote width not mirrored");
    let mut spins = 0;
    while p.warm_one() {
        spins += 1;
        assert!(spins < 8, "warm queue does not converge: {:?}", p.warm_q);
    }
    assert!(p.warm_q.is_empty(), "queue left armed after warm: {:?}", p.warm_q);
    assert!(shown(&p).contains('\u{25ba}'), "quoted diagram never drawn");
    // the same thread just drew this source at both widths: a fresh pager
    // needs no warming at all, the cache answers every probe
    let doc2 = "```mermaid\ngraph LR\n A[alpha] --> B[omega]\n```\n\n> ```mermaid\n> graph LR\n>  A[alpha] --> B[omega]\n> ```\n";
    let p = pager_diag(&[doc2], 24, true);
    assert!(p.warm_q.is_empty(), "already-decided fences re-queued: {:?}", p.warm_q);
    assert_eq!(shown(&p).matches('\u{25ba}').count(), 2, "both copies from cache");
}

#[test]
fn long_colored_lines_wrap_without_blink() {
    let doc = format!("```rust\nlet s = \"{}\";\n```", "x".repeat(200));
    let p = pager(&[&doc], 12);
    let f = p.frame();
    assert!(!f.contains("\u{1b}[5m"));
    assert!(p.docs[0].lines.iter().all(|l| visible_width(l) <= 60));
}

#[test]
fn resize_rewraps_and_clamps() {
    let mut p = pager(&[&"word ".repeat(400)], 12);
    let before = p.docs[0].lines.len();
    feed(&mut p, b"G");
    p.resize(120, 12);
    assert!(p.docs[0].lines.len() < before);
    assert!(top(&p) <= p.max_top());
}

#[test]
fn multiple_files() {
    let mut p = pager(&["# one", "# two", "# three"], 10);
    feed(&mut p, b"]");
    assert_eq!(p.file, 1);
    feed(&mut p, b"[[");
    assert_eq!(p.file, 2, "[ wraps around");
    feed(&mut p, b"]");
    assert!(feed(&mut p, b"q"), "q on the first file moves on");
    assert_eq!(p.file, 1);
    assert!(feed(&mut p, b"q"));
    assert!(!feed(&mut p, b"q"), "q on the last file exits");
}
