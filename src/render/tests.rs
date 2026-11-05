// Unit tests for the renderer and ANSI helpers.

use super::*;
use crate::ansi::{strip_ansi, wrap};

fn cfg_plain() -> Cfg {
    Cfg { color: false, width: 80, urls: true }
}

fn cfg_color() -> Cfg {
    Cfg { color: true, width: 80, urls: true }
}

#[test]
fn headings_render_in_both_modes() {
    let doc = "# Title\n\ntext";
    let plain = render(doc, &cfg_plain());
    assert_eq!(plain[0], "# Title");
    let color = render(doc, &cfg_color());
    assert!(color[0].contains("\u{1b}[1;4;38;5;203m"));
    assert_eq!(strip_ansi(&color[0]), " Title");
}

#[test]
fn inline_emphasis_and_code() {
    let s = inline("**bold** and *em* and `code`", &cfg_color());
    let plain = strip_ansi(&s);
    assert_eq!(plain, "bold and em and code");
    assert!(s.contains("\u{1b}[1m")); // bold
    assert!(s.contains("\u{1b}[3m")); // italic
    assert!(s.contains("48;5;236")); // inline code background

    // plain mode emits no escapes
    assert!(!inline("**x** `y`", &cfg_plain()).contains('\u{1b}'));
}

#[test]
fn snake_case_survives() {
    let s = inline("my_var_name and _real em_", &cfg_plain());
    assert_eq!(s, "my_var_name and real em");
}

#[test]
fn unmatched_delimiters_stay_literal() {
    assert_eq!(inline("2 * 3 * 4", &cfg_plain()), "2 * 3 * 4");
    assert_eq!(inline("a ` b", &cfg_plain()), "a ` b");
}

#[test]
fn link_with_url_and_osc8() {
    let s = inline("[site](https://example.com)", &cfg_color());
    assert!(s.contains("\u{1b}]8;;https://example.com\u{1b}\\"));
    let plain = strip_ansi(&s);
    assert!(plain.starts_with("site"));
    assert!(plain.contains("(https://example.com)"));
}

#[test]
fn lists_and_tasks() {
    let doc = "- one\n  - nested\n- [x] done\n- [ ] todo\n\n1. first\n2. second";
    let joined = render(doc, &cfg_plain()).join("\n");
    assert!(joined.contains("• one"));
    assert!(joined.contains("  ◦ nested"));
    assert!(joined.contains("☑ done"));
    assert!(joined.contains("☐ todo"));
    assert!(joined.contains("1. first"));
    assert!(joined.contains("2. second"));
}

#[test]
fn fenced_code_with_language_and_generic_highlight() {
    let doc = "```rust\nfn main() {\n    let x = 1; // hi\n}\n```\nafter";
    let lines = render(doc, &cfg_color());
    let joined = strip_ansi(&lines.join("\n"));
    assert!(joined.contains("rust"));
    assert!(joined.contains("fn main"));
    assert!(joined.contains("// hi"));
    assert!(joined.ends_with("after"));
    // keyword got colored
    assert!(lines[1].contains("38;5;141"));
}

#[test]
fn blockquote_nesting() {
    let doc = "> outer\n> > inner";
    let lines = render(doc, &cfg_plain());
    assert!(lines[0].starts_with('┃'));
    assert!(lines.iter().any(|l| l.contains("inner")));
    assert!(lines.iter().all(|l| visible_width(l) <= 80));
}

#[test]
fn table_alignment_and_width() {
    let doc = "| name | n |\n|:-----|--:|\n| alpha | 1 |\n| beta | 22 |";
    let lines = render(doc, &cfg_plain());
    assert!(lines[0].starts_with('╭'));
    assert!(lines.last().unwrap().starts_with('╰'));
    assert!(lines.iter().all(|l| visible_width(l) <= 80));
    let body = &lines[3];
    assert!(body.contains("alpha"));
    // right-aligned column: digits hug the right border
    assert!(lines[4].contains("22 │"));
}

#[test]
fn hr_and_entities_and_escapes() {
    let lines = render("---\n\na &amp; b \\*lit\\*", &cfg_plain());
    assert!(lines.iter().any(|l| l.starts_with('-')));
    assert!(lines.iter().any(|l| l == "a & b *lit*"));
}

#[test]
fn front_matter_hidden() {
    let doc = "---\ntitle: hidden\n---\n\n# Real";
    let lines = render(doc, &cfg_plain());
    assert!(!lines.iter().any(|l| l.contains("hidden")));
    assert!(lines.iter().any(|l| l.contains("Real")));
}

#[test]
fn wrap_never_breaks_escapes_or_width() {
    let long = format!("{}{}", "\u{1b}[1;38;5;203m".to_string(), "x".repeat(100));
    let rows = wrap(&long, 40);
    assert!(rows.len() >= 3);
    for r in &rows {
        assert!(visible_width(r) <= 40);
        // every row is valid: escape count is balanced, no lone ESC mid-row oddity
        assert_eq!(strip_ansi(r).len(), visible_width(r));
    }
    // continuation rows reopen the bold style
    assert!(rows[1].starts_with("\u{1b}["));
}

#[test]
fn wide_cjk_width() {
    assert_eq!(visible_width("日本語"), 6);
    assert_eq!(visible_width("a\u{301}"), 1);
}

use crate::ansi::visible_width;
