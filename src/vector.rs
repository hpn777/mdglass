//! Diagram recognition and SVG production for the GUI (docs/diagram-engine-options.md).
//!
//! Pure logic, no GTK: which code blocks become pictures, and how their SVG
//! is produced. Every entry point fails soft (`None`) so the caller keeps the
//! plain code block, and both engines are `catch_unwind`-wrapped: a parser
//! panic must degrade to text, never close the window. `catch_unwind` cannot
//! stop a stack overflow or an abort, so engine input is size-capped too.

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

/// Engines are forgiving but not infinite: layout time and parser recursion
/// grow with the input, run on the UI thread, and a deep enough recursion is
/// a SIGSEGV that catch_unwind cannot catch. Oversized blocks stay text.
fn fits(text: &str) -> bool {
    text.len() <= 200_000 && text.lines().count() <= 2000
}

/// Unicode box drawing and arrows mapped onto the ASCII dialect the aasvg
/// recognizer understands (`+` corners, `-`/`|` rules, `>`/`<`/`v`/`^` arrow
/// heads). Everything else passes through; every mapping is one column.
pub fn to_ascii(art: &str) -> String {
    art.chars()
        .map(|c| match c {
            '│' | '┃' | '╎' | '╏' | '┆' | '┇' | '┊' | '┋' | '║' => '|',
            '─' | '━' | '═' | '╌' | '╍' | '┄' | '┅' | '┈' | '┉' => '-',
            '╱' => '/',
            '╲' => '\\',
            '╳' => 'X',
            '┌' | '┍' | '┎' | '┏' | '╒' | '╓' | '╔' | '╭' => '+',
            '┐' | '┑' | '┒' | '┓' | '╕' | '╖' | '╗' | '╮' => '+',
            '└' | '┕' | '┖' | '┗' | '╘' | '╙' | '╚' | '╰' => '+',
            '┘' | '┙' | '┚' | '┛' | '╛' | '╜' | '╝' | '╯' => '+',
            c if ('\u{251C}'..='\u{254C}').contains(&c) => '+', // ├..┬ row/block junctions
            c if ('\u{2552}'..='\u{2570}').contains(&c) => '+', // double junctions and rounded corners
            '→' | '⇒' | '▶' | '►' | '▸' | '▹' => '>',
            '←' | '⇐' | '◀' | '◄' | '◂' | '◃' => '<',
            '↓' | '▼' | '▾' => 'v',
            '↑' | '▲' | '▴' => '^',
            c => c,
        })
        .collect()
}

/// `[=====>    ] 50%`: a progress bar, not a diagram.
fn progress_bar(line: &str) -> bool {
    let t = line.trim_start();
    let Some(body) = t.strip_prefix('[') else { return false };
    if !t.contains('%') {
        return false;
    }
    // the run is ASCII by construction, so byte-slicing after it is safe
    let run = body.chars().take_while(|c| matches!(c, '=' | '-' | '#' | '+' | '>')).count();
    run >= 3 && body[run..].trim_start().starts_with(']')
}

/// Does this block look like box art aasvg can draw? Deliberately strict:
/// a false positive turns ordinary code into a picture (and its text into
/// pixels find cannot see), a false negative just keeps today's text block.
/// `lines` arrive already `to_ascii`-translated (the render path does that).
pub fn looks_like_art(lines: &[String]) -> bool {
    if lines.len() < 2 || lines.len() > 48 || lines.iter().any(|l| l.len() > 140 || l.contains('\t'))
    {
        return false;
    }
    let all = lines.join("\n");
    // aasvg's column grid needs exactly one cell per char: no double-width
    // glyphs and no zero-width marks shifting the grid
    if all.chars().any(|c| c != '\n' && crate::ansi::char_width(c) != 1) {
        return false;
    }
    // markup is never box art, and vectorizing it would hide HTML people
    // came to this file to read (unclosed tags turn into label mush)
    if all.contains("</") || all.contains("/>") || all.contains("<!--") || all.contains("<!") {
        return false;
    }
    if lines.iter().any(|l| progress_bar(l)) {
        return false;
    }
    // psql/mysql/RST grid tables: `| a | b |` rows. They ARE boxes, but
    // their content is data, and a picture of data cannot be searched or
    // copied, so text wins whenever multi-column rows carry the block
    let rows = lines.iter().filter(|l| l.matches('|').count() >= 3).count();
    if rows >= 3 && rows * 2 >= lines.len() {
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
    // a line is "art" when line glyphs and true arrow heads outweigh its
    // printables: `<`/`>` count only as arrow heads, i.e. touching a RUN of
    // at least two rule chars (`-->` yes, `a <= b` no), so comparison
    // operators never qualify
    let art_lines = lines
        .iter()
        .filter(|l| {
            let b: Vec<char> = l.chars().collect();
            let rule = |c: char| c == '-' || c == '=';
            let run_after = |i: usize| b[i..].iter().take_while(|c| rule(**c)).count();
            let run_before = |i: usize| b[..i].iter().rev().take_while(|c| rule(**c)).count();
            let art = b
                .iter()
                .enumerate()
                .filter(|(i, c)| match **c {
                    '+' | '-' | '|' | '=' => true,
                    '<' => run_after(i + 1) >= 2,
                    '>' => run_before(*i) >= 2,
                    _ => false,
                })
                .count();
            let solid = b.iter().filter(|c| !c.is_whitespace()).count().max(1);
            art >= 2 && art * 3 > solid
        })
        .count();
    (boxy || arrow) && art_lines * 2 >= lines.len()
}

/// Pin aasvg's palette to the current theme before rasterizing. librsvg
/// resolves `var()` only from stylesheets, never from presentation
/// attributes (verified against 2.61: the var() strokes rasterize to
/// nothing), so every reference becomes the literal color and the CSS block
/// is dropped. If the input is not the shape aasvg emits today, `None`:
/// an unthemed picture on the wrong background is worse than code text.
pub fn svg_theme(svg: &str, dark: bool) -> Option<String> {
    let (stroke, bg, text) =
        if dark { ("#9a999d", "#242424", "#dfdfdf") } else { ("#5e5c64", "#ffffff", "#222222") };
    let a = svg.find("<style>")?;
    let rel = svg[a..].find("</style>")?; // searched after the opening tag
    let mut out = format!("{}{}", &svg[..a], &svg[a + rel + "</style>".len()..]);
    for (var, color) in
        [("var(--aasvg-stroke)", stroke), ("var(--aasvg-fill)", stroke), ("var(--aasvg-bg)", bg), ("var(--aasvg-text)", text)]
    {
        out = out.replace(var, color);
    }
    if out.contains("var(") {
        return None; // a variable we do not know would render invisible
    }
    Some(out)
}

/// Raw SVG from the engine named by `lang`, or None to keep the block as
/// text. Mermaid bakes its palette into the SVG (a dark preset exists and
/// the theme affects layout-relevant colors), so its output is keyed by
/// theme; aasvg emits `var()` colors and stays theme-independent until
/// `apply_theme`. None is cached too, so a broken block never re-runs an
/// engine on every frame.
pub fn engine_svg(lang: &str, text: &str, dark: bool) -> Option<String> {
    if !fits(text) {
        return None;
    }
    if is_mermaid(lang) {
        let text = text.to_string();
        let built = catch_unwind(move || {
            // modern (light) is the crate default look; Theme::dark() paints
            // the #333333 canvas mermaid users expect in dark mode
            let theme =
                if dark { mermaid_rs_renderer::Theme::dark() } else { mermaid_rs_renderer::Theme::modern() };
            let opts = mermaid_rs_renderer::RenderOptions { theme, ..Default::default() };
            mermaid_rs_renderer::render_with_options(&text, opts)
        })
        .ok()?;
        return built.ok();
    }
    if !art_lang(lang) {
        return None;
    }
    let ascii = to_ascii(text);
    let lines: Vec<String> = ascii.lines().map(str::to_string).collect();
    if !looks_like_art(&lines) {
        return None;
    }
    catch_unwind(move || aasvg::render(&ascii)).ok()
}

/// Engine output themed for the current style; mermaid SVGs already carry
/// the palette rendered for that style and pass through, like images do.
pub fn apply_theme(lang: &str, raw: &str, dark: bool) -> Option<String> {
    if is_mermaid(lang) {
        return Some(raw.to_string());
    }
    svg_theme(raw, dark)
}

/// Second-chance Mermaid through an installed `mmdr` CLI, when the built-in
/// renderer (0.3.x, still growing) cannot parse the diagram. The `-t` preset
/// keeps the fallback's palette in step with the built-in one.
pub fn mermaid_via_cli(text: &str, dark: bool) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if !fits(text) {
        return None;
    }
    let mut child = Command::new("mmdr")
        .args(["-e", "svg", "-t", if dark { "dark" } else { "modern" }])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // close stdin even when the write fails, and always reap the child:
    // an early return here used to leak a zombie next to a blocked mmdr
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(text.as_bytes());
    }
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
    fn gate_rejects_html_bars_tables_comparisons() {
        // every one of these used to vectorize into a picture
        assert!(!looks_like_art(&lines("<ul>\n  <li>one</li>\n  <!-- more -->\n</ul>")));
        assert!(!looks_like_art(&lines("<!-- header -->\n<p>hi</p>")));
        assert!(!looks_like_art(&lines("<App>\n  <Header />\n  <Main>{x}</Main>\n  {/* <!-- --> */}\n</App>")));
        assert!(!looks_like_art(&lines("[=====>     ] 50%\n[==========>] 100%")));
        assert!(!looks_like_art(&lines("a <= b\nb >= c\nc == d --> e")));
        // psql/mysql/RST grid tables keep their searchable text
        assert!(!looks_like_art(&lines(
            "+----+-------+\n| id | name  |\n+----+-------+\n| 1  | alice |\n| 2  | bob   |\n+----+-------+"
        )));
        // ...but a real flow with a couple of boxes is still art
        assert!(looks_like_art(&lines(
            "+--------+      +--------+\n| Client |----->| Server |\n+--------+      +--------+"
        )));
    }

    #[test]
    fn gate_rejects_zero_width_shifters() {
        // VS16/ZWSP would slide aasvg's column grid out of sync
        assert!(!looks_like_art(&lines("+------\u{200b}+\n| a b\u{fe0f} |\n+------+")));
    }

    #[test]
    fn unicode_maps_to_dialect() {
        assert_eq!(to_ascii("\u{250c}\u{2500}\u{2510}\u{2502}\u{2514}\u{2500}\u{2518}\u{251c}\u{252c}\u{2524}\u{2534}\u{253c}\u{2550}\u{2551}"), "+-+|+-++++++-|"); // ═ maps to the - rule
        assert_eq!(to_ascii("a → b ← c ⇒ d ▶ e ◀ f"), "a > b < c > d > e < f");
        assert_eq!(to_ascii("↓ ▲ ╱ ╲ ╳"), "v ^ / \\ X");
        assert_eq!(to_ascii("plain: text │ ok"), "plain: text | ok");
        // the round trip the docs promise: unicode boxes vectorize after mapping
        let uni = "\u{250c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2510}\n\u{2502} db \u{2502}\n\u{2514}\u{2500}\u{2500}\u{2500}\u{2500}\u{2518}";
        assert!(looks_like_art(&lines(&to_ascii(uni))));
    }

    #[test]
    fn aasvg_vectors_the_art_and_theme_pins_colors() {
        let art = "+------+\n| db   |----> out\n+------+\n";
        let raw = engine_svg("", art, false).expect("art renders");
        assert!(raw.contains("<style>"), "engine emits the themed shape");
        // aasvg raw output carries var() colors, so it does not depend on
        // the theme; only mermaid's baked palette does
        assert_eq!(engine_svg("", art, true), Some(raw.clone()), "aasvg raw is theme-independent");
        let svg = apply_theme("", &raw, true).expect("theme applies");
        assert!(svg.contains("<path"), "expected vector paths");
        assert!(svg.contains("stroke=\"#9a999d\""), "dark stroke pinned");
        assert!(svg.contains("fill=\"#dfdfdf\""), "label color pinned");
        assert!(!svg.contains("var(--"), "no unresolved vars left");
        assert!(!svg.contains("<style>"), "CSS block dropped");
        assert!(apply_theme("", &raw, false).unwrap().contains("stroke=\"#5e5c64\""));
        // a differently shaped engine output must fail closed
        assert_eq!(apply_theme("", "<svg>changed shape</svg>", true), None);
        // typed blocks stay code even when they draw boxes
        assert!(engine_svg("rust", art, false).is_none());
        assert!(engine_svg("text", art, false).is_some());
    }

    #[test]
    fn mermaid_renders_and_garbage_fails_soft() {
        let svg = engine_svg("mermaid", "flowchart LR; A-->B", false).expect("tiny flowchart");
        assert!(svg.contains("<svg"));
        assert!(svg.contains("#FFFFFF"), "light style gets the light canvas");
        let dark = engine_svg("mermaid", "flowchart LR; A-->B", true).expect("dark renders");
        assert!(dark.contains("#333333"), "dark style gets the #333333 canvas");
        assert!(!dark.contains("#FFFFFF"), "no light canvas leaks into dark mode");
        assert_ne!(svg, dark, "the palette follows the style");
        assert_eq!(apply_theme("mermaid", &dark, true).unwrap(), dark); // own palette
        // mmdr is forgiving, but rejects a document with no diagram header
        assert!(engine_svg("mermaid", "this is not mermaid at all", false).is_none());
        // oversized input stays text instead of hanging the UI thread
        assert!(engine_svg("mermaid", &"flowchart LR\n".repeat(30_000), false).is_none());
        assert!(mermaid_via_cli(&"x".repeat(300_000), false).is_none());
    }

    #[test]
    fn shipped_example_renders_as_designed() {
        // examples/diagrams.md is the demo people run and screenshot; if the
        // gate or an engine changes shape, the file must fail this, not rot
        let doc = include_str!("../examples/diagrams.md");
        let mut fences: Vec<(String, String)> = Vec::new();
        let mut open: Option<(String, String)> = None;
        for line in doc.lines() {
            if line.starts_with("```") {
                match open.take() {
                    None => open = Some((line[3..].trim().to_string(), String::new())),
                    Some(f) => fences.push(f),
                }
            } else if let Some((_, body)) = open.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
        assert!(open.is_none(), "balanced fences");
        assert_eq!(fences.len(), 6, "six fenced blocks in the example");
        for (i, lang) in [("", ""), ("text", "text")].iter().take(0) {
            let _ = (i, lang); // (kept simple below)
        }
        // box art and unicode art vectorize, themed for either style
        for i in 0..2 {
            assert_eq!(fences[i].0, ["", "text"][i], "block {i} info string");
            let raw = engine_svg(&fences[i].0, &fences[i].1, false).unwrap_or_else(|| panic!("art block {i} must render"));
            assert!(raw.contains("<path"), "block {i} has vector paths");
            for dark in [true, false] {
                assert!(apply_theme(&fences[i].0, &raw, dark).is_some(), "block {i} themes");
            }
        }
        // the three mermaid diagrams parse with the built-in engine, in
        // both palettes
        for i in 2..5 {
            assert_eq!(fences[i].0, "mermaid", "block {i} info string");
            for dark in [true, false] {
                assert!(engine_svg("mermaid", &fences[i].1, dark).is_some(), "mermaid block {i} must render (dark={dark})");
            }
        }
        // the grid table is the deliberate rejection: it stays searchable text
        assert_eq!(fences[5].0, "");
        assert!(engine_svg("", &fences[5].1, false).is_none(), "the data table must stay text");
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
