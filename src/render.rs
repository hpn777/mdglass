// ANSI terminal back end: turns the parsed document (`crate::doc`) into styled
// terminal rows. A fast, line-based renderer for a practical CommonMark/GFM
// subset: headings (ATX + setext), emphasis, code (inline and fenced with
// generic highlighting), blockquotes (nested), lists (nested, ordered, task
// lists), links (with OSC 8 hyperlinks), images, tables, horizontal rules,
// entities, and front-matter. Parsing lives in `doc.rs` and is shared with the
// native GUI back end (`gui.rs`).

use crate::ansi::{sg, truncate_to, visible_width, wrap, wrap_indent, RESET};

pub use crate::doc::{parse, spans_plain, table_text, Align, Cfg, Line, Role, Span, Style, Table, TableRow};

// ------------------------------------------------------------ inline ANSI ---

const KEYWORDS: &[&str] = &[
    "abstract", "and", "as", "async", "await", "begin", "break", "case", "catch", "class",
    "const", "continue", "def", "default", "del", "do", "dyn", "elif", "else", "end", "enum",
    "except", "export", "extends", "false", "final", "finally", "fn", "for", "from", "func",
    "global", "go", "goto", "if", "impl", "import", "in", "inline", "interface", "is", "lambda",
    "let", "local", "loop", "macro", "match", "mod", "move", "mut", "namespace", "new", "nil",
    "None", "not", "null", "nullptr", "or", "override", "package", "pass", "pub", "pure", "ref",
    "return", "self", "static", "struct", "super", "switch", "template", "then", "this", "throw",
    "throws", "trait", "true", "try", "type", "typedef", "typeof", "union", "unsafe", "use",
    "using", "val", "var", "virtual", "when", "while", "with", "yield",
];

/// The SGR colors that express a role in the terminal.
fn role_code(role: Role) -> Option<&'static str> {
    match role {
        Role::None => None,
        Role::Bullet => Some("38;5;215"),
        Role::Code => Some("38;5;203;48;5;236"),
        Role::Link => Some("4;38;5;39"),
        Role::Image => Some("38;5;175"),
        Role::Faint => Some("2"),
        Role::Done => Some("38;5;41"),
        Role::Todo => Some("2"),
    }
}

fn flag_code(style: Style, role: Role) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if style.contains(Style::BOLD) {
        parts.push("1");
    }
    if style.contains(Style::ITALIC) {
        parts.push("3");
    }
    if style.contains(Style::STRIKE) {
        parts.push("9");
    }
    if style.contains(Style::UNDER) && role != Role::Link {
        parts.push("4"); // the link role already underlines
    }
    parts.join(";")
}

/// One styled span to ANSI: SGR colors, and an OSC 8 hyperlink for links.
fn span_ansi(s: &Span, cfg: &Cfg, base: &str) -> String {
    if !cfg.color || s.text.is_empty() {
        return s.text.clone();
    }
    let flags = flag_code(s.style, s.role);
    let role = role_code(s.role).unwrap_or("");
    let mut code = String::new();
    for part in [base, flags.as_str(), role] {
        if !part.is_empty() {
            if !code.is_empty() {
                code.push(';');
            }
            code.push_str(part);
        }
    }
    if code.is_empty() {
        return s.text.clone();
    }
    match &s.url {
        Some(url) => {
            let clean: String = url.chars().filter(|c| *c != '\u{7}' && *c != '\u{1b}').collect();
            format!("\u{1b}]8;;{clean}\u{1b}\\{}{}{RESET}\u{1b}]8;;\u{1b}\\", sg(&code), s.text)
        }
        None => format!("{}{}{RESET}", sg(&code), s.text),
    }
}

fn spans_ansi(spans: &[Span], cfg: &Cfg, base: &str) -> String {
    spans.iter().map(|s| span_ansi(s, cfg, base)).collect()
}

/// Render inline markdown to a styled string (single logical line).
#[cfg(test)]
pub fn inline(src: &str, cfg: &Cfg) -> String {
    spans_ansi(&crate::doc::inline_spans(src, cfg), cfg, "")
}

// ----------------------------------------------------------- block ANSI ---

pub fn render(src: &str, cfg: &Cfg) -> Vec<String> {
    render_lines(&parse(src, cfg), cfg)
}

/// Turn parsed blocks into styled terminal rows.
pub fn render_lines(lines: &[Line], cfg: &Cfg) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        match line {
            Line::Blank => out.push(String::new()),
            Line::Rule => {
                if cfg.color {
                    out.push(format!("{}{}{RESET}", sg("2"), "─".repeat(cfg.width.max(1))));
                } else {
                    out.push("-".repeat(cfg.width.max(1)));
                }
            }
            Line::Heading { lvl, spans } => out.extend(heading(cfg, *lvl, spans)),
            Line::Para { spans, indent, hanging } => {
                let pre = " ".repeat(*indent);
                let hang = " ".repeat(indent + hanging);
                let body = spans_ansi(spans, cfg, "");
                out.extend(wrap_indent(&format!("{pre}{body}"), cfg.width, &hang));
            }
            Line::Code { lang, lines } => {
                out.push(fence_top(cfg, lang));
                let mut in_block = false;
                for l in lines {
                    out.extend(code_lines(cfg, l, &mut in_block));
                }
                out.push(fence_bottom(cfg));
            }
            Line::Table(t) => {
                if !cfg.color {
                    out.extend(table_text(t));
                } else {
                    out.extend(table_ansi(t, cfg));
                }
            }
            Line::Quote { inner } => {
                let inner_cfg = Cfg { width: cfg.width.saturating_sub(2).max(20), ..cfg.clone() };
                let mut rows = render_lines(inner, &inner_cfg);
                while rows.last().map(|r| r.is_empty()).unwrap_or(false) {
                    rows.pop();
                }
                let bar = if cfg.color { format!("{}┃{RESET}", sg("38;5;33")) } else { "┃".into() };
                out.extend(rows.into_iter().map(|r| {
                    if r.is_empty() {
                        bar.clone()
                    } else {
                        format!("{bar} {r}")
                    }
                }));
            }
        }
    }
    out
}

fn heading(cfg: &Cfg, lvl: usize, spans: &[Span]) -> Vec<String> {    if !cfg.color {
        let marks = "#".repeat(lvl);
        let text = spans_plain(spans);
        return wrap_indent(&format!("{marks} {text}"), cfg.width, &" ".repeat(lvl + 1));
    }
    let code = match lvl {
        1 => "1;4;38;5;203",
        2 => "1;4;38;5;208",
        3 => "1;38;5;221",
        4 => "1;38;5;148",
        5 => "1;38;5;81",
        _ => "1;2",
    };
    let body = spans_ansi(spans, cfg, code);
    wrap_indent(&format!(" {body}"), cfg.width, " ")
}

/// Styled table rows: faint frame, bold header, inline-styled cells.
fn table_ansi(t: &Table, cfg: &Cfg) -> Vec<String> {
    let cols = t.widths.len();
    let border = |l: char, m: char, r: char| -> String {
        let segs: Vec<String> = t.widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("{}{l}{}{r}{RESET}", sg("2"), segs.join(&m.to_string()))
    };
    let pipe = || format!("{}│{RESET}", sg("2"));
    let row_line = |cells: &[Vec<Span>], header: bool| -> String {
        let mut s = pipe();
        for c in 0..cols {
            let raw = spans_ansi(cells.get(c).cloned().unwrap_or_default().as_slice(), cfg, if header { "1" } else { "" });
            let cell = truncate_to(&raw, t.widths[c]);
            let a = t.aligns.get(c).copied().unwrap_or(Align::Left);
            let extra = t.widths[c].saturating_sub(visible_width(&cell));
            let text = match a {
                Align::Right => format!("{}{cell}", " ".repeat(extra)),
                Align::Center => {
                    format!("{}{cell}{}", " ".repeat(extra / 2), " ".repeat(extra - extra / 2))
                }
                Align::Left => format!("{cell}{}", " ".repeat(extra)),
            };
            s.push(' ');
            s.push_str(&text);
            s.push(' ');
            s.push_str(&pipe());
        }
        s
    };
    t.rows
        .iter()
        .map(|row| match row {
            TableRow::Top => border('╭', '┬', '╮'),
            TableRow::Sep => border('├', '┼', '┤'),
            TableRow::Bottom => border('╰', '┴', '╯'),
            TableRow::Cells { header, cells } => row_line(cells, *header),
        })
        .collect()
}

fn fence_top(cfg: &Cfg, lang: &str) -> String {
    if !cfg.color {
        return truncate_to(&format!("```{lang}"), cfg.width);
    }
    let w = cfg.width;
    if w < 6 {
        return format!("{}{}{RESET}", sg("2"), "─".repeat(w));
    }
    let label = if lang.is_empty() { String::new() } else { truncate_to(&format!(" {lang} "), w - 4) };
    let lw = visible_width(&label);
    let dashes = w.saturating_sub(2 + lw);
    let (a, b) = (dashes / 2, dashes - dashes / 2);
    format!("{}╭{a}{label}{b}╮{RESET}", sg("2"), a = "─".repeat(a), b = "─".repeat(b))
}

fn fence_bottom(cfg: &Cfg) -> String {
    if !cfg.color {
        return "```".to_string();
    }
    format!("{}╰{}╯{RESET}", sg("2"), "─".repeat(cfg.width.saturating_sub(2).max(1)))
}

fn code_lines(cfg: &Cfg, s: &str, in_block: &mut bool) -> Vec<String> {
    if !cfg.color {
        return wrap(s, cfg.width);
    }
    let base = sg("38;5;252;48;5;236");
    let body = highlight(s, in_block).replace(RESET, &format!("{RESET}{base}"));
    wrap(&format!("{base}{body}"), cfg.width)
        .into_iter()
        .map(|r| {
            let pad = " ".repeat(cfg.width.saturating_sub(visible_width(&r)));
            format!("{r}{base}{pad}{RESET}")
        })
        .collect()
}

// ------------------------------------------------------ code highlighting ---

fn tok(code: &str, slice: &[char]) -> String {
    format!("{}{}{RESET}", sg(code), slice.iter().collect::<String>())
}

fn highlight(s: &str, in_block: &mut bool) -> String {
    let ch: Vec<char> = s.chars().collect();
    let n = ch.len();
    let mut out = String::new();
    let mut i = 0usize;
    while i < n {
        if *in_block {
            let mut j = i;
            let end = loop {
                if j >= n {
                    break n;
                }
                if ch[j] == '*' && ch.get(j + 1) == Some(&'/') {
                    *in_block = false;
                    break j + 2;
                }
                j += 1;
            };
            out.push_str(&tok("2;3", &ch[i..end]));
            i = end;
            continue;
        }
        let c = ch[i];
        if c == '/' && ch.get(i + 1) == Some(&'/') {
            out.push_str(&tok("2;3", &ch[i..]));
            break;
        }
        if c == '#' && (i == 0 || !ch[i - 1].is_alphanumeric()) {
            out.push_str(&tok("2;3", &ch[i..]));
            break;
        }
        if c == '/' && ch.get(i + 1) == Some(&'*') {
            *in_block = true;
            out.push_str(&tok("2;3", &ch[i..i + 2]));
            i += 2;
            continue;
        }
        if matches!(c, '"' | '\'' | '`') {
            let mut j = i + 1;
            while j < n {
                if ch[j] == '\\' {
                    j += 2;
                    continue;
                }
                if ch[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            out.push_str(&tok("38;5;179", &ch[i..j.min(n)]));
            i = j.min(n);
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n && (ch[j].is_ascii_alphanumeric() || matches!(ch[j], '.' | '_')) {
                j += 1;
            }
            out.push_str(&tok("38;5;175", &ch[i..j]));
            i = j;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < n && (ch[j].is_alphanumeric() || ch[j] == '_') {
                j += 1;
            }
            let word: String = ch[i..j].iter().collect();
            let lower = word.to_lowercase();
            let next_nonspace = ch[j..].iter().find(|c| !c.is_whitespace());
            let code = if KEYWORDS.contains(&lower.as_str()) {
                Some("1;38;5;141")
            } else if next_nonspace == Some(&'(') {
                Some("38;5;222")
            } else if word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                Some("38;5;186")
            } else {
                None
            };
            match code {
                Some(c) => out.push_str(&tok(c, &ch[i..j])),
                None => out.push_str(&word),
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests;
