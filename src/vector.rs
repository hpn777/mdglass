//! Diagram recognition and SVG production for the GUI (docs/diagram-engine-options.md).
//!
//! Pure logic, no GTK: which code blocks become pictures, and how their SVG
//! is produced. Every entry point fails soft (`None`) so the caller keeps the
//! plain code block, and both engines are `catch_unwind`-wrapped: a parser
//! panic must degrade to text, never close the window.

use std::panic::catch_unwind;

/// The Mermaid info string that opts a fenced block into diagram rendering.
pub fn is_mermaid(lang: &str) -> bool {
    lang.trim().eq_ignore_ascii_case("mermaid")
}

/// Info strings that mean "untyped text", where box art may legitimately
/// appear. A `rust`/`bash`/... block stays code even if it draws boxes.
pub fn art_lang(lang: &str) -> bool {
    matches!(lang.trim().to_ascii_lowercase().as_str(), "" | "text" | "txt" | "diagram" | "art")
}

/// Unicode box drawing and arrows mapped onto the ASCII dialect the aasvg
/// recognizer understands (`+` corners, `-`/`|` rules, `>`/`<` arrow heads).
/// Everything else passes through, one column stays one column.
pub fn to_ascii(art: &str) -> String {
    art.chars()
        .map(|c| match c {
            '│' | '┃' | '╎' | '╏' | '┆' | '┇' | '┊' | '┋' | '║' => '|',
            '─' | '━' | '═' | '╌' | '╍' | '┄' | '┅' | '┈' | '┉' => '-',
            '┌' | '┍' | '┎' | '┏' | '╒' | '╓' | '╔' | '╭' => '+',
            '┐' | '┑' | '┒' | '┓' | '╕' | '╖' | '╗' | '╮' => '+',
            '└' | '┕' | '┖' | '┗' | '╘' | '╙' | '╚' | '╰' => '+',
            '┘' | '┙' | '┚' | '┛' | '╛' | '╜' | '╝' | '╯' => '+',
            c if ('\u{251C}'..='\u{254C}').contains(&c) => '+', // ├..┬ row/block junctions
            c if ('\u{2550}'..='\u{2573}').contains(&c) => '+', // ═..╬ double lines
            '→' | '⇒' | '▶' => '>',
            '←' | '⇐' | '◀' => '<',
            c => c,
        })
        .collect()
}

/// Does this block look like box art aasvg can draw? Deliberately strict:
/// a false positive turns ordinary code into a picture, a false negative
/// just keeps today's text block. Rules: bounded size, ASCII-only after the
/// box translation (double-width glyphs would desync aasvg's column math),
/// art glyphs on most lines, and either a closed `+---+` box or an `-->`
/// arrow.
pub fn looks_like_art(lines: &[String]) -> bool {
    if lines.len() < 2 || lines.len() > 48 || lines.iter().any(|l| l.len() > 140 || l.contains('\t')) {
        return false;
    }
    let all = lines.join("\n");
    if all.chars().any(|c| crate::ansi::char_width(c) > 1) {
        return false;
    }
    let flat = to_ascii(&all);
    let boxy = flat.lines().any(|l| {
        let b = l.as_bytes();
        b.iter().enumerate().any(|(i, c)| {
            *c == b'+'
                && {
                    let mut dashes = 0;
                    let mut j = i + 1;
                    while j < b.len() && (b[j] == b'-' || b[j] == b'=') {
                        dashes += 1;
                        j += 1;
                    }
                    dashes >= 2 && j < b.len() && b[j] == b'+'
                }
        })
    });
    let arrow = ["-->", "<--", "==>", "<=="].iter().any(|a| flat.contains(a));
    // a line is "art" when line/arrow glyphs make up a third of its
    // non-space content: true of box rules and arrows, false of code that
    // merely contains `=` or a `-->` in a comment
    let art_lines = lines
        .iter()
        .filter(|l| {
            let art = l.chars().filter(|c| matches!(c, '+' | '-' | '|' | '=' | '>' | '<')).count();
            let solid = l.chars().filter(|c| !c.is_whitespace()).count().max(1);
            art >= 2 && art * 3 >= solid
        })
        .count();
    (boxy || arrow) && art_lines * 2 >= lines.len()
}

/// Pin aasvg's palette to the current theme before rasterizing. librsvg
/// resolves `var()` only from stylesheets, never from presentation
/// attributes (verified against 2.61: the var() strokes rasterize to
/// nothing), so every `var(--aasvg-*)` reference becomes the literal color
/// and the CSS block is dropped.
pub fn svg_theme(svg: &str, dark: bool) -> String {
    let (stroke, bg, text) = if dark { ("#9a999d", "#242424", "#dfdfdf") } else { ("#5e5c64", "#ffffff", "#222222") };
    let mut out = svg.to_string();
    if let (Some(a), Some(rel)) = (out.find("<style>"), out.find("</style>")) {
        out.replace_range(a..rel + "</style>".len(), "");
    }
    for (var, color) in [
        ("var(--aasvg-stroke)", stroke),
        ("var(--aasvg-fill)", stroke),
        ("var(--aasvg-bg)", bg),
        ("var(--aasvg-text)", text),
    ] {
        out = out.replace(var, color);
    }
    out
}

/// SVG for a code block, or None to keep it as text. `dark` themes the art;
/// Mermaid SVGs carry their own (light) palette, like embedded images do.
pub fn diagram_svg(lang: &str, text: &str, dark: bool) -> Option<String> {
    if is_mermaid(lang) {
        let text = text.to_string();
        return catch_unwind(move || mermaid_rs_renderer::render(&text)).ok()?.ok();
    }
    if !art_lang(lang) {
        return None;
    }
    let ascii = to_ascii(text);
    let lines: Vec<String> = ascii.lines().map(str::to_string).collect();
    if !looks_like_art(&lines) {
        return None;
    }
    let svg = catch_unwind(move || aasvg::render(&ascii)).ok()?;
    Some(svg_theme(&svg, dark))
}

/// Second-chance Mermaid through an installed `mmdr` CLI, when the built-in
/// renderer (0.3.x, still growing) cannot parse the diagram.
pub fn mermaid_via_cli(text: &str) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("mmdr")
        .args(["-e", "svg"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(text.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if out.status.success() && out.stdout.windows(5).any(|w| w == b"<svg ") {
        String::from_utf8(out.stdout).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    #[test]
    fn gate_accepts_art_rejects_code() {
        assert!(looks_like_art(&lines("+------+\n| db   |----> out\n+------+\n")));
        assert!(looks_like_art(&lines("a --> b\nb --> c\n"))); // arrows only, 2/2 art lines
        let code = lines("fn main() {\n    x --> y(); // a rust body, mostly code\n    let v = vec![1];\n}");
        assert!(!looks_like_art(&code));
        assert!(!looks_like_art(&lines("a --> b"))); // single line
        assert!(!looks_like_art(&lines("+------+\n| 数据库 |\n+------+\n"))); // CJK: width-2
        assert!(!looks_like_art(&lines("just prose\nnothing else")));
    }

    #[test]
    fn unicode_maps_to_dialect() {
        assert_eq!(to_ascii("\u{250c}\u{2500}\u{2510}\u{2502}\u{2514}\u{2500}\u{2518}\u{251c}\u{252c}\u{2524}\u{2534}\u{253c}\u{2550}\u{2551}"), "+-+|+-++++++-|"); // ═ maps to the - rule
        assert_eq!(to_ascii("a → b ← c ⇒ d ▶ e ◀ f"), "a > b < c > d > e < f");
        assert_eq!(to_ascii("plain: text │ ok"), "plain: text | ok");
        // the round trip the docs promise: unicode boxes vectorize after mapping
        let uni = "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\n\u{2502} db \u{2502}\n\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}";
        assert!(looks_like_art(&lines(&to_ascii(uni))));
    }

    #[test]
    fn aasvg_vectors_the_art_and_theme_pins_colors() {
        let art = "+------+\n| db   |----> out\n+------+\n";
        let svg = diagram_svg("", art, true).expect("art renders");
        assert!(svg.contains("<path"), "expected vector paths");
        assert!(svg.contains("stroke=\"#9a999d\""), "dark stroke pinned");
        assert!(svg.contains("fill=\"#dfdfdf\""), "label color pinned");
        assert!(!svg.contains("var(--"), "no unresolved vars left");
        assert!(!svg.contains("<style>"), "CSS block dropped");
        assert!(diagram_svg("", art, false).unwrap().contains("stroke=\"#5e5c64\""));
        // typed blocks stay code even when they draw boxes
        assert!(diagram_svg("rust", art, true).is_none());
        assert!(diagram_svg("text", art, true).is_some());
    }

    #[test]
    fn mermaid_renders_and_garbage_fails_soft() {
        let svg = diagram_svg("mermaid", "flowchart LR; A-->B", false).expect("tiny flowchart");
        assert!(svg.contains("<svg"));
        // unparsable input must be None, not a panic and not a picture
        // mmdr is forgiving, but rejects a document with no diagram header
        assert!(diagram_svg("mermaid", "this is not mermaid at all", false).is_none());
    }

    #[test]
    fn mermaid_lang_match() {
        assert!(is_mermaid("mermaid"));
        assert!(is_mermaid(" Mermaid "));
        assert!(!is_mermaid("text"));
        assert!(!is_mermaid(""));
        assert!(art_lang(""));
        assert!(art_lang("TEXT"));
        assert!(!art_lang("mermaid"));
        assert!(!art_lang("python"));
    }
}

