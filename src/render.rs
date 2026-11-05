// Markdown -> ANSI renderer. A fast, line-based renderer for a practical
// CommonMark/GFM subset: headings (ATX + setext), emphasis, code (inline and
// fenced with generic highlighting), blockquotes (nested), lists (nested,
// ordered, task lists), links (with OSC 8 hyperlinks), images, tables,
// horizontal rules, entities, and front-matter.

use crate::ansi::{pad_to, restyled, sg, truncate_to, visible_width, wrap, RESET};

pub struct Cfg {
    pub color: bool,
    pub width: usize,
    pub urls: bool,
}

impl Cfg {
    fn narrow(&self, by: usize) -> Cfg {
        Cfg { color: self.color, width: self.width.saturating_sub(by).max(20), urls: self.urls }
    }
}

fn st(cfg: &Cfg, code: &str, text: &str) -> String {
    if cfg.color && !text.is_empty() {
        format!("{}{text}{RESET}", sg(code))
    } else {
        text.to_string()
    }
}

// ---------------------------------------------------------------- inline ---

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

const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"), ("lt", "<"), ("gt", ">"), ("quot", "\""), ("apos", "'"), ("nbsp", "\u{a0}"),
    ("mdash", "—"), ("ndash", "–"), ("hellip", "…"), ("copy", "©"), ("reg", "®"),
    ("trade", "™"), ("deg", "°"), ("plusmn", "±"), ("times", "×"), ("middot", "·"),
    ("rarr", "→"), ("larr", "←"), ("darr", "↓"), ("uarr", "↑"), ("harr", "↔"),
    ("bull", "•"), ("sect", "§"), ("para", "¶"), ("dagger", "†"), ("euro", "€"),
    ("pound", "£"), ("yen", "¥"), ("copy2", "©"),
];

fn run_len(ch: &[char], i: usize, c: char) -> usize {
    let mut n = 0;
    while i + n < ch.len() && ch[i + n] == c {
        n += 1;
    }
    n
}

fn closer_at(ch: &[char], j: usize, tok: &[char]) -> bool {
    if j + tok.len() > ch.len() || ch[j..j + tok.len()] != *tok {
        return false;
    }
    if j == 0 || ch[j - 1] == ' ' || ch[j - 1] == '\t' {
        return false;
    }
    if tok[0] == '_' {
        if let Some(&c) = ch.get(j + tok.len()) {
            if c.is_alphanumeric() {
                return false; // keep snake_case intact
            }
        }
    }
    true
}

fn find_closer(ch: &[char], content_start: usize, tok: &[char]) -> Option<usize> {
    let mut j = content_start + 1;
    while j < ch.len() {
        if ch[j] == tok[0] {
            if closer_at(ch, j, tok) {
                return Some(j);
            }
            j += run_len(ch, j, tok[0]);
        } else {
            j += 1;
        }
    }
    None
}

fn try_entity(ch: &[char], i: usize) -> Option<(&'static str, usize)> {
    let mut j = i + 1;
    let mut name = String::new();
    while j < ch.len() && j - i <= 12 {
        let c = ch[j];
        if c == ';' {
            let mapped = ENTITIES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, r)| *r)
                .or_else(|| match name.as_str() {
                    "#39" => Some("'"),
                    "#38" => Some("&"),
                    "#60" => Some("<"),
                    "#62" => Some(">"),
                    "#34" => Some("\""),
                    "#160" => Some("\u{a0}"),
                    "#8212" => Some("—"),
                    "#8211" => Some("–"),
                    "#8230" => Some("…"),
                    _ => None,
                });
            return mapped.map(|r| (r, j + 1));
        }
        if !(c.is_ascii_alphanumeric() || c == '#') {
            return None;
        }
        name.push(c);
        j += 1;
    }
    None
}

fn parse_bracket(ch: &[char], i: usize, cfg: &Cfg, out: &mut String) -> Option<usize> {
    let img = ch[i] == '!';
    let o = if img { i + 1 } else { i };
    if ch.get(o) != Some(&'[') {
        return None;
    }
    let mut depth = 0usize;
    let mut j = o;
    while j < ch.len() {
        match ch[j] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            '\\' => j += 1,
            _ => {}
        }
        j += 1;
    }
    if j >= ch.len() || ch.get(j + 1) != Some(&'(') {
        return None;
    }
    let mut pd = 1usize;
    let mut k = j + 2;
    while k < ch.len() {
        match ch[k] {
            '(' => pd += 1,
            ')' => {
                pd -= 1;
                if pd == 0 {
                    break;
                }
            }
            '\\' => k += 1,
            _ => {}
        }
        k += 1;
    }
    if pd != 0 {
        return None;
    }
    let target: String = ch[j + 2..k].iter().collect();
    let target = target.split_whitespace().next().unwrap_or("").trim_matches('<');
    let label: String = ch[o + 1..j].iter().collect();
    if img {
        out.push_str(&st(cfg, "38;5;175", "◈ "));
        let alt = if label.trim().is_empty() { target } else { &label };
        out.push_str(&st(cfg, "2;3", alt));
        if cfg.urls && cfg.color && !label.trim().is_empty() && !target.is_empty() {
            out.push_str(&st(cfg, "2", &format!(" ({target})")));
        }
    } else {
        let text = inline(&label, cfg);
        if cfg.color {
            let clean: String =
                target.chars().filter(|c| *c != '\u{7}' && *c != '\u{1b}').collect();
            out.push_str(&format!("\u{1b}]8;;{clean}\u{1b}\\{}", sg("4;38;5;39")));
            out.push_str(&text);
            out.push_str(RESET);
            out.push_str("\u{1b}]8;;\u{1b}\\");
        } else {
            out.push_str(&text);
        }
        if cfg.urls && !target.is_empty() && target != label.trim() {
            out.push_str(&st(cfg, "2", &format!(" ({target})")));
        }
    }
    Some(k + 1)
}

/// Render inline markdown (emphasis, code, links, images, entities, escapes,
/// autolinks) to a styled string (single logical line).
pub fn inline(src: &str, cfg: &Cfg) -> String {
    let ch: Vec<char> = src.trim_end().chars().collect();
    let n = ch.len();
    let mut out = String::new();
    let mut i = 0usize;
    while i < n {
        let c = ch[i];
        if c == '\u{1b}' {
            // pass through pre-existing escapes verbatim
            crate::ansi::copy_escape(&ch, &mut i, &mut out);
            continue;
        }
        match c {
            '\\' if i + 1 < n && ch[i + 1].is_ascii_punctuation() => {
                out.push(ch[i + 1]);
                i += 2;
            }
            '`' => {
                let run = run_len(&ch, i, '`');
                let mut j = i + run;
                let mut close = None;
                while j < n {
                    if ch[j] == '`' {
                        let r2 = run_len(&ch, j, '`');
                        if r2 == run {
                            close = Some(j);
                            break;
                        }
                        j += r2;
                    } else {
                        j += 1;
                    }
                }
                if let Some(cj) = close {
                    let mut inner: String = ch[i + run..cj].iter().collect();
                    if inner.len() > 2 && inner.starts_with(' ') && inner.ends_with(' ') {
                        inner = inner[1..inner.len() - 1].to_string();
                    }
                    out.push_str(&st(cfg, "38;5;203;48;5;236", &inner.replace('\u{1b}', "")));
                    i = cj + run;
                } else {
                    out.push('`');
                    i += 1;
                }
            }
            '*' | '_' => {
                let run = run_len(&ch, i, c);
                let opener_ok = c == '*' || i == 0 || !ch[i - 1].is_alphanumeric();
                let after = i + run;
                let flanking = after < n && ch[after] != ' ' && ch[after] != '\t';
                let mut done = false;
                if opener_ok && flanking {
                    for (need, code) in [(3, "1;3"), (2, "1"), (1, "3")] {
                        if run < need {
                            continue;
                        }
                        let tok: Vec<char> = vec![c; need];
                        if let Some(j) = find_closer(&ch, after, &tok) {
                            let inner: String = ch[after..j].iter().collect();
                            let body = inline(&inner, cfg);
                            let shown = if cfg.color { restyled(&body, &sg(code)) } else { body };
                            out.push_str(&shown);
                            i = j + need;
                            done = true;
                            break;
                        }
                    }
                }
                if !done {
                    out.push(c);
                    i += 1;
                }
            }
            '~' if run_len(&ch, i, '~') >= 2 => {
                let tok = ['~', '~'];
                if let Some(j) = find_closer(&ch, i + 2, &tok) {
                    let inner: String = ch[i + 2..j].iter().collect();
                    let body = inline(&inner, cfg);
                    let shown = if cfg.color { restyled(&body, &sg("9")) } else { body };
                    out.push_str(&shown);
                    i = j + 2;
                } else {
                    out.push('~');
                    i += 1;
                }
            }
            '!' | '[' => {
                if let Some(next) = parse_bracket(&ch, i, cfg, &mut out) {
                    i = next;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            '<' => {
                if let Some(end) = find_angle(&ch, i) {
                    let body: String = ch[i + 1..end].iter().collect();
                    let is_link = body.starts_with("http://")
                        || body.starts_with("https://")
                        || body.starts_with("mailto:");
                    if is_link {
                        let url = body.trim_end_matches('>').trim();
                        if cfg.color {
                            out.push_str(&format!(
                                "\u{1b}]8;;{url}\u{1b}\\{}{url}{RESET}\u{1b}]8;;\u{1b}\\",
                                sg("4;38;5;39")
                            ));
                        } else {
                            out.push_str(url);
                        }
                    }
                    // other HTML tags are dropped
                    i = end + 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            '&' => {
                if let Some((rep, next)) = try_entity(&ch, i) {
                    out.push_str(rep);
                    i = next;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Find the end of an HTML tag or autolink starting at `<`, if it looks like one.
fn find_angle(ch: &[char], i: usize) -> Option<usize> {
    let n = ch.len();
    let first = *ch.get(i + 1)?;
    if !(first.is_ascii_alphabetic() || first == '/' || first == '!') {
        return None;
    }
    let mut j = i + 1;
    while j < n && j - i <= 300 {
        let c = ch[j];
        if c == '>' {
            return Some(j);
        }
        if !(c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '=' | '"' | '\'' | '/' | '-' | ':' | '.' | '_' | ',' | '#' | '?'))
        {
            return None;
        }
        j += 1;
    }
    None
}

// -------------------------------------------------------------- blocks ---

pub fn render(src: &str, cfg: &Cfg) -> Vec<String> {
    let normalized = src.replace('\t', "    ").replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;

    // YAML/TOML front matter
    if lines.first().map(|l| l.trim()) == Some("---") || lines.first().map(|l| l.trim()) == Some("+++") {
        let fence = lines[0].trim();
        if let Some(end) = (1..lines.len()).find(|&k| lines[k].trim() == fence) {
            i = end + 1;
        }
    }

    let push_blank = |out: &mut Vec<String>| {
        if !out.is_empty() && !out.last().map(|l| l.is_empty()).unwrap_or(false) {
            out.push(String::new());
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();

        // fenced code
        if t.starts_with("```") || t.starts_with("~~~") {
            let fc = t.chars().next().unwrap();
            let fence: String = t.chars().take_while(|&c| c == fc).collect();
            let lang = t[fence.len()..].trim().to_string();
            out.push(fence_top(cfg, &lang));
            let mut in_block = false;
            i += 1;
            while i < lines.len() {
                let cl = lines[i].trim();
                if cl.starts_with(&fence) && cl.chars().all(|c| c == fc || c == ' ') {
                    i += 1;
                    break;
                }
                out.push(code_line(cfg, &lines[i].replace('\t', "    "), &mut in_block));
                i += 1;
            }
            out.push(fence_bottom(cfg));
            continue;
        }

        if t.is_empty() {
            push_blank(&mut out);
            i += 1;
            continue;
        }

        // ATX heading
        if t.starts_with('#') {
            let lvl = t.chars().take_while(|&c| c == '#').count();
            if lvl <= 6 && (t.len() == lvl || t.as_bytes()[lvl] == b' ') {
                push_blank(&mut out);
                out.push(heading(cfg, lvl, t[lvl..].trim()));
                push_blank(&mut out);
                i += 1;
                continue;
            }
        }

        // setext heading
        if !is_block_start(line) {
            if let Some(next) = lines.get(i + 1) {
                let nt = next.trim();
                if !nt.is_empty() && nt.chars().all(|c| c == '=') {
                    push_blank(&mut out);
                    out.push(heading(cfg, 1, t));
                    push_blank(&mut out);
                    i += 2;
                    continue;
                }
                if nt.len() >= 2 && nt.chars().all(|c| c == '-') {
                    push_blank(&mut out);
                    out.push(heading(cfg, 2, t));
                    push_blank(&mut out);
                    i += 2;
                    continue;
                }
            }
        }

        if hr_detect(t) {
            push_blank(&mut out);
            out.push(if cfg.color {
                st(cfg, "2", &"─".repeat(cfg.width.max(1)))
            } else {
                "-".repeat(cfg.width.max(1))
            });
            push_blank(&mut out);
            i += 1;
            continue;
        }

        // table
        if line.contains('|') && lines.get(i + 1).map(|l| is_sep_row(l.trim())).unwrap_or(false) {
            let (rows, next_i) = render_table(&lines, i, cfg);
            out.extend(rows);
            push_blank(&mut out);
            i = next_i;
            continue;
        }

        // blockquote
        if t.starts_with('>') {
            let mut inner: Vec<String> = Vec::new();
            let lazy_ok = |l: &str| {
                !l.trim().is_empty() && !is_block_start(l) && !hr_detect(l.trim())
            };
            while i < lines.len() {
                let lt = lines[i].trim_start();
                if let Some(rest) = lt.strip_prefix('>') {
                    inner.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    i += 1;
                } else if lazy_ok(lines[i]) && !inner.last().map(|x| x.trim().is_empty()).unwrap_or(true) {
                    inner.push(lines[i].to_string());
                    i += 1;
                } else {
                    break;
                }
            }
            let quoted = render(&inner.join("\n"), &cfg.narrow(2));
            let bar = st(cfg, "38;5;33", "┃");
            for q in quoted {
                let joined = format!("{bar} {q}");
                for row in wrap(&joined, cfg.width.max(3)) {
                    out.push(row);
                }
            }
            continue;
        }

        // list item
        if let Some((indent, marker, rest)) = list_item(line) {
            let depth = (indent / 2).min(6);
            let pad = "  ".repeat(depth);
            let (content, task) = strip_task(&rest);
            let mut bullet = String::new();
            match marker {
                Marker::Bullet => {
                    let sym = ["•", "◦", "▪"][depth % 3];
                    bullet.push_str(&st(cfg, "38;5;215", sym));
                }                Marker::Ord(num) => bullet.push_str(&st(cfg, "1;38;5;215", &num)),
            }
            bullet.push(' ');
            match task {
                Some(true) => bullet.push_str(&st(cfg, "1;38;5;41", "☑ ")),
                Some(false) => bullet.push_str(&st(cfg, "2", "☐ ")),
                None => {}
            }
            out.push(format!("{pad}{bullet}{}", inline(content, cfg)));
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                if l.trim().is_empty() || list_item(l).is_some() || is_block_start(l) {
                    break;
                }
                out.push(format!("{pad}  {}", inline(l.trim_start(), cfg)));
                i += 1;
            }
            continue;
        }

        out.push(inline(line.trim_end(), cfg));
        i += 1;
    }

    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

fn is_block_start(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with('#')
        || t.starts_with('>')
        || t.starts_with("```")
        || t.starts_with("~~~")
        || t.starts_with('|')
        || list_item(l).is_some()
}

fn hr_detect(t: &str) -> bool {
    if t.len() < 3 {
        return false;
    }
    let c = match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    t.chars().all(|ch| ch == c || ch == ' ') && t.chars().filter(|&ch| ch == c).count() >= 3
}

fn heading(cfg: &Cfg, lvl: usize, text: &str) -> String {
    if !cfg.color {
        return format!("{} {text}", "#".repeat(lvl));
    }
    let code = match lvl {
        1 => "1;4;38;5;203",
        2 => "1;4;38;5;208",
        3 => "1;38;5;221",
        4 => "1;38;5;148",
        5 => "1;38;5;81",
        _ => "1;2",
    };
    let body = inline(text, cfg);
    format!("{} {body}{RESET}", sg(code))
}

fn fence_top(cfg: &Cfg, lang: &str) -> String {
    if !cfg.color {
        return format!("```{lang}");
    }
    let w = cfg.width;
    if w < 6 {
        return st(cfg, "2", &"─".repeat(w));
    }
    let label = if lang.is_empty() { String::new() } else { format!(" {lang} ") };
    let lw = visible_width(&label);
    let dashes = w.saturating_sub(2 + lw);
    let (a, b) = (dashes / 2, dashes - dashes / 2);
    format!("{}╭{a}{label}{b}╮{RESET}", sg("2"), a = "─".repeat(a), b = "─".repeat(b))
}

fn fence_bottom(cfg: &Cfg) -> String {
    if !cfg.color {
        return "```".to_string();
    }
    st(cfg, "2", &format!("╰{}╯", "─".repeat(cfg.width.saturating_sub(2).max(1))))
}

fn code_line(cfg: &Cfg, s: &str, in_block: &mut bool) -> String {
    if !cfg.color {
        return s.to_string();
    }
    let base = sg("38;5;252;48;5;236");
    let body = highlight(s, in_block);
    let body = body.replace(RESET, &format!("{RESET}{base}"));
    let pad = " ".repeat(cfg.width.saturating_sub(visible_width(&body)));
    format!("{base}{body}{pad}{RESET}")
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

// ---------------------------------------------------------------- tables ---

fn is_sep_row(t: &str) -> bool {
    let t = t.trim();
    if t.len() < 3 || !t.contains('|') {
        return false;
    }
    let body = t.trim_start_matches('|').trim_end_matches('|');
    !body.is_empty()
        && body.split('|').all(|c| {
            let c = c.trim().trim_start_matches(':').trim_end_matches(':');
            !c.is_empty() && c.chars().all(|ch| ch == '-')
        })
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

fn render_table(lines: &[&str], start: usize, cfg: &Cfg) -> (Vec<String>, usize) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut i = start;
    let mut aligns: Vec<Align> = Vec::new();
    while i < lines.len() {
        let t = lines[i].trim().trim_matches('|');
        if !lines[i].contains('|') {
            break;
        }
        let cells: Vec<String> = split_cells(t).into_iter().map(|c| c.trim().to_string()).collect();
        if i == start + 1 {
            // alignment row (always the second line of the block)
            aligns = cells
                .iter()
                .map(|c| {
                    let l = c.starts_with(':');
                    let r = c.ends_with(':');
                    match (l, r) {
                        (true, true) => Align::Center,
                        (false, true) => Align::Right,
                        _ => Align::Left,
                    }
                })
                .collect();
            i += 1;
            continue;
        }
        rows.push(cells);
        i += 1;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    rows.iter_mut().for_each(|r| r.resize(cols, String::new()));
    aligns.resize(cols, Align::Left);

    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|c| inline(c, cfg)).collect())
        .collect();
    let mut widths: Vec<usize> = Vec::new();
    for c in 0..cols {
        let mut wmax = 3usize;
        for (ri, _) in rows.iter().enumerate() {
            wmax = wmax.max(visible_width(&rendered[ri][c]));
        }
        widths.push(wmax.min(60));
    }
    let total = |ws: &[usize]| ws.iter().sum::<usize>() + 3 * ws.len() + 1;
    while total(&widths) > cfg.width && widths.iter().any(|w| *w > 3) {
        if let Some(mx) = widths.iter().position(|w| *w == *widths.iter().max().unwrap()) {
            widths[mx] -= 1;
        }
    }
    for w in widths.iter_mut() {
        *w = (*w).min(cfg.width.saturating_sub(3).max(3));
    }

    let mut out: Vec<String> = Vec::new();
    if rows.len() >= 2 {
        let border = |l: char, m: char, r: char| -> String {
            let segs: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            st(cfg, "2", &format!("{l}{}{r}", segs.join(&m.to_string())))
        };
        out.push(border('╭', '┬', '╮'));
        let row_line = |cells: &[String], align_row: bool| -> String {
            let mut s = st(cfg, "2", "│");
            for c in 0..cols {
                let raw = cells.get(c).cloned().unwrap_or_default();
                let cell = truncate_to(&raw, widths[c]);
                let cell = if align_row && cfg.color { restyled(&cell, &sg("1")) } else { cell };
                let a = aligns.get(c).copied().unwrap_or(Align::Left);
                let extra = widths[c].saturating_sub(visible_width(&cell));
                let text = match a {
                    Align::Right => format!("{}{cell}", " ".repeat(extra)),
                    Align::Center => format!(
                        "{}{cell}{}",
                        " ".repeat(extra / 2),
                        " ".repeat(extra - extra / 2)
                    ),
                    Align::Left => pad_to(&cell, widths[c]),
                };
                s.push(' ');
                s.push_str(&text);
                s.push(' ');
                s.push_str(&st(cfg, "2", "│"));
            }
            s
        };
        out.push(row_line(&rendered[0], true));
        out.push(border('┝', '┿', '┥'));
        for row in rendered.iter().skip(1) {
            out.push(row_line(row, false));
        }
        out.push(border('╰', '┴', '╯'));
    }
    (out, i)
}

fn split_cells(t: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for c in t.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
            cur.push(c);
        } else if c == '|' {
            cells.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    cells.push(cur);
    cells
}

// ---------------------------------------------------------------- lists ---

enum Marker {
    Bullet,
    Ord(String),
}

fn list_item(l: &str) -> Option<(usize, Marker, String)> {
    let indent = l.chars().take_while(|&c| c == ' ').count();
    let rest = &l[indent..];
    let mut it = rest.chars();
    let first = it.next()?;
    if matches!(first, '-' | '*' | '+') && matches!(it.next(), Some(' ') | None) && rest.len() < 1000 {
        // not a horizontal rule
        if hr_detect(rest.trim()) {
            return None;
        }
        return Some((indent, Marker::Bullet, rest[1..].trim_start().to_string()));
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).take(9).collect();
    if !digits.is_empty() {
        let after = &rest[digits.len()..];
        if after.starts_with(". ") || after.starts_with(") ") || after == "." || after == ")" {
            return Some((
                indent,
                Marker::Ord(format!("{}.", digits)),
                after[1..].trim_start().to_string(),
            ));
        }
    }
    None
}

fn strip_task(s: &str) -> (&str, Option<bool>) {
    if let Some(rest) = s.strip_prefix("[ ] ") {
        (rest, Some(false))
    } else if let Some(rest) = s.strip_prefix("[x] ").or_else(|| s.strip_prefix("[X] ")) {
        (rest, Some(true))
    } else {
        (s, None)
    }
}

#[cfg(test)]
mod tests;
