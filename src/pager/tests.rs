// Pager tests. Input goes through `parse_bytes` exactly as terminal bytes
// would, so key-decoding mismatches (e.g. Enter arriving as CR) are caught.

use super::*;

fn pager(docs: &[&str], height: usize) -> Pager {
    let files: Vec<(String, String)> =
        docs.iter().enumerate().map(|(i, d)| (format!("f{i}.md"), d.to_string())).collect();
    Pager::new(&files, true, true, 60, height)
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
