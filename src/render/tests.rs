// Unit tests for the renderer and ANSI helpers.

use super::*;
use crate::ansi::{strip_ansi, visible_width, wrap, wrap_indent};

fn cfg_plain() -> Cfg {
    Cfg::new(false, 80, true)
}

fn cfg_color() -> Cfg {
    Cfg::new(true, 80, true)
}

const MMD: &str = "```mermaid\ngraph LR\n A[alpha] --> B[omega]\n```\n";

#[test]
fn mermaid_fences_become_box_art_when_diagrams_on() {
    // color: Unicode boxes inside a shaded fence, labels searchable, source gone
    let cfg = Cfg { diagrams: true, ..cfg_color() };
    let out = render(MMD, &cfg);
    let all = out.join("\n");
    let plain = strip_ansi(&all);
    assert!(plain.contains('►') && plain.contains("alpha"), "no art: {plain}");
    assert!(!plain.contains("graph LR"), "source beside art");
    assert!(plain.contains("mermaid"), "fence label lost");
    assert!(out.iter().all(|l| visible_width(l) <= 80), "art overflowed width");
    // plain mode: pure ASCII, still fenced
    let cfg = Cfg { diagrams: true, ..cfg_plain() };
    let all = render(MMD, &cfg).join("\n");
    assert!(all.contains('+') && all.contains('-') && all.contains("alpha"), "no ASCII art");
    assert!(all.contains(">") && !all.contains('\u{25ba}'), "non-ASCII in plain art");
    assert!(all.contains("```"), "plain fence lost");
}

#[test]
fn probe_only_mode_waits_for_warm_then_draws() {
    crate::boxart::clear_cache();
    let cfg = Cfg { diagrams: true, art_probe_only: true, ..cfg_color() };
    // cold cache: the pager frame must not wait on the engine -> source
    let all = strip_ansi(&render(MMD, &cfg).join("\n"));
    assert!(all.contains("graph"), "probe mode blocked on the engine");
    assert!(!all.contains('\u{25ba}'), "probe mode ran the engine");
    // the idle warm tick fills the cache; the next probe-only frame has art
    crate::boxart::terminal_ascii("graph LR\n A[alpha] --> B[omega]", 80, false);
    let all = strip_ansi(&render(MMD, &cfg).join("\n"));
    assert!(all.contains('\u{25ba}') && all.contains("alpha"), "warm art did not appear");
    // plain art is fenced as text, not as (invalid) mermaid
    let cfg = Cfg { diagrams: true, ..cfg_plain() };
    let all = render(MMD, &cfg).join("\n");
    assert!(all.contains("```text mermaid"), "plain art must not masquerade as mermaid");
    assert!(!all.contains("```mermaid\n+"), "plain art fenced as mermaid");
}

#[test]
fn diagrams_on_falls_back_and_ignores_strangers() {
    let cfg = Cfg { diagrams: true, ..cfg_color() };
    // broken mermaid keeps its source rows (boxart refuses -> None)
    let out = render("```mermaid\nthis is not a diagram\n```\n", &cfg).join("\n");
    assert!(strip_ansi(&out).contains("this is not a diagram"));
    // other languages never enter the art path
    let out = render("```rust\nfn a() {}\n```\n", &cfg).join("\n");
    assert!(strip_ansi(&out).contains("fn a()"));
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

#[test]
fn wrap_breaks_at_words() {
    let s = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima";
    let rows = wrap(s, 30);
    for r in &rows {
        assert!(visible_width(r) <= 30);
        assert!(!r.starts_with(' '));
    }
    let words: Vec<&str> = rows.iter().flat_map(|r| r.split(' ')).collect();
    assert_eq!(words, s.split(' ').collect::<Vec<_>>()); // no word was split
}

#[test]
fn wrap_hard_splits_overlong_words() {
    let rows = wrap(&"x".repeat(25), 10);
    assert_eq!(rows.iter().map(|r| r.len()).collect::<Vec<_>>(), vec![10, 10, 5]);
}

#[test]
fn wrap_reopens_256_colors_without_blink() {
    let s = format!("\u{1b}[38;5;252;48;5;236m{}\u{1b}[0m", "word ".repeat(20));
    let rows = wrap(&s, 30);
    assert!(rows.len() > 1);
    for r in &rows[1..] {
        assert!(r.starts_with("\u{1b}[38;5;252;48;5;236m"), "row lost its style: {r:?}");
        assert!(!r.contains("\u{1b}[5m"), "blink leaked into: {r:?}");
    }
    // a color index of 0 is not a reset
    let s = format!("\u{1b}[38;5;0m{}", "word ".repeat(10));
    assert!(wrap(&s, 20)[1].starts_with("\u{1b}[38;5;0m"));
}

#[test]
fn wrap_closes_and_reopens_hyperlinks() {
    let open = "\u{1b}]8;;https://x.io\u{1b}\\";
    let s = format!("{open}{}\u{1b}]8;;\u{1b}\\", "link ".repeat(10));
    let rows = wrap(&s, 20);
    assert!(rows[0].contains("\u{1b}]8;;\u{1b}\\"));
    assert!(rows[1].starts_with(open));
}

#[test]
fn wrap_indent_hangs_continuations() {
    let rows = wrap_indent("• one two three four five six", 12, "  ");
    assert!(rows.len() > 1);
    assert!(rows[1..].iter().all(|r| r.starts_with("  ") && !r.starts_with("   ")));
}

#[test]
fn rendered_rows_fit_width() {
    let doc = format!(
        "# {h}\n\n{p}\n\n- {p}\n\n> {p}\n\n```\n{c}\n```\n",
        h = "heading ".repeat(15),
        p = "lorem ipsum ".repeat(20),
        c = "let x = 1; ".repeat(15)
    );
    for cfg in [Cfg::new(true, 50, true), Cfg::new(false, 50, true)] {
        for r in render(&doc, &cfg) {
            assert!(visible_width(&r) <= 50, "{} cols: {r:?}", visible_width(&r));
        }
    }
}

#[test]
fn list_continuation_rows_are_indented() {
    let rows = render(&format!("- {}", "word ".repeat(30)), &Cfg::new(false, 40, true));
    assert!(rows[0].starts_with("• "));
    assert!(rows[1..].iter().all(|r| r.starts_with("  ")));
}

#[test]
fn blockquote_bar_on_every_row() {
    let rows = render(&format!("> {}", "word ".repeat(30)), &Cfg::new(false, 40, true));
    assert!(rows.len() > 1);
    assert!(rows.iter().all(|r| r.starts_with('┃')));
}

#[test]
fn leading_hr_is_not_front_matter() {
    let doc = "---\n# Visible Title\n\nimportant paragraph\n\n---\n\nend";
    let joined = render(doc, &cfg_plain()).join("\n");
    assert!(joined.contains("Visible Title"));
    assert!(joined.contains("important paragraph"));
}

#[test]
fn toml_front_matter_hidden() {
    let doc = "+++\ntitle = \"x\"\n[extra]\nk = 1\n+++\nbody";
    assert_eq!(render(doc, &cfg_plain()), vec!["body"]);
}

#[test]
fn soft_breaks_join_and_hard_breaks_split() {
    let rows = render("one\ntwo *em\nphasis*  \nthree\\\nfour", &cfg_plain());
    assert_eq!(rows, vec!["one two em phasis", "three", "four"]);
}

#[test]
fn paragraph_line_then_dashes_is_setext() {
    let rows = render("intro\nTitle\n---\nbody", &cfg_plain());
    assert_eq!(rows, vec!["intro", "", "## Title", "", "body"]);
}

#[test]
fn reference_links_resolve() {
    let doc = "[full][r] [collapsed][] [r] [nope]\n\n[r]: https://r.io\n[collapsed]: <https://c.io> \"title\"";
    let rows = render(doc, &cfg_plain());
    assert_eq!(rows, vec!["full (https://r.io) collapsed (https://c.io) r (https://r.io) [nope]"]);
}

#[test]
fn indented_code_block() {
    let rows = render("text\n\n    let x = 1;\n\n    let y = 2;\n\nafter", &cfg_plain());
    assert_eq!(rows, vec!["text", "", "```", "let x = 1;", "", "let y = 2;", "```", "", "after"]);
}

#[test]
fn indented_text_after_list_is_not_code() {
    let rows = render("- item\n\n    more about item\n\nafter", &cfg_plain());
    assert!(!rows.iter().any(|r| r == "```"));
    assert!(rows.iter().any(|r| r.trim() == "more about item"));
}

#[test]
fn plain_truncated_table_has_no_escapes() {
    let doc = format!("| a | b |\n|---|---|\n| {} | {} |", "word ".repeat(30), "x".repeat(90));
    let rows = render(&doc, &cfg_plain());
    assert!(rows.iter().all(|r| !r.contains('\u{1b}')), "{rows:?}");
    assert!(rows.iter().all(|r| visible_width(r) <= 80));
    assert!(rows.iter().any(|r| r.contains('…')));
}

#[test]
fn emoji_and_cjk_widths() {
    use crate::ansi::char_width;
    for c in ['❌', '✅', '⚡', '🚀', '🧪', '☰', '日', 'Ｗ'] {
        assert_eq!(char_width(c), 2, "{c} U+{:04X}", c as u32);
    }
    for c in ['a', 'é', '→', '│', '✔', '🌡', '🛠', '©'] {
        assert_eq!(char_width(c), 1, "{c} U+{:04X}", c as u32);
    }
    for c in ['\u{301}', '\u{200d}', '\u{fe0f}', '\u{64e}'] {
        assert_eq!(char_width(c), 0, "U+{:04X}", c as u32);
    }
}

#[test]
fn inline_triple_backticks_are_not_a_fence() {
    let rows = render("```pip install torch --index-url https://x```\n\nafter", &cfg_plain());
    assert_eq!(rows, vec!["pip install torch --index-url https://x", "", "after"]);
}

#[test]
fn fence_language_is_first_word_of_info() {
    let rows = render("```python title=\"x.py\"\nprint(1)\n```", &cfg_plain());
    assert_eq!(rows, vec!["```python", "print(1)", "```"]);
}

#[test]
fn multiline_html_comment_hidden() {
    let rows = render("a\n\n<!--\nsecret\n-->\n\nb", &cfg_plain());
    assert!(!rows.iter().any(|r| r.contains("secret")));
    assert!(rows.contains(&"b".to_string()));
}
