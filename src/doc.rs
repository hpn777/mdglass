// Markdown document model: one parser, two back ends (ANSI terminal, native GUI).
// Parsing yields styled spans inside typed blocks; each back end decides how a
// heading, a code block or a link is actually drawn.

use std::collections::HashMap;
use std::ops::BitOr;

use crate::ansi::{char_width, copy_escape};

#[derive(Clone, Default)]
pub struct Cfg {
    pub color: bool,
    pub width: usize,
    pub urls: bool,
    /// Reference-link definitions (`[label]: url`), keyed by normalized label.
    pub refs: HashMap<String, String>,
}

impl Cfg {
    pub fn new(color: bool, width: usize, urls: bool) -> Cfg {
        Cfg { color, width, urls, refs: HashMap::new() }
    }

    fn narrow(&self, by: usize) -> Cfg {
        Cfg { width: self.width.saturating_sub(by).max(20), ..self.clone() }
    }
}

/// What a piece of text means, so each back end can pick its own colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    None,
    Bullet,
    Code,
    Link,
    Image,
    Faint,
    Done,
    Todo,
}

/// Character attributes that combine across nesting (`**_x_**`, links in bold, ...).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style(u8);

impl Style {
    pub const BOLD: Style = Style(1);
    pub const ITALIC: Style = Style(2);
    pub const STRIKE: Style = Style(4);
    pub const UNDER: Style = Style(8);

    pub const fn empty() -> Style {
        Style(0)
    }

    pub const fn contains(self, other: Style) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Style {
    type Output = Style;
    fn bitor(self, o: Style) -> Style {
        Style(self.0 | o.0)
    }
}

impl std::ops::BitOrAssign for Style {
    fn bitor_assign(&mut self, o: Style) {
        self.0 |= o.0;
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub role: Role,
    /// Hyperlink target for `Role::Link`.
    pub url: Option<String>,
}

impl Span {
    fn new(text: impl Into<String>) -> Span {
        Span { text: text.into(), style: Style::empty(), role: Role::None, url: None }
    }

    fn with(text: impl Into<String>, style: Style, role: Role) -> Span {
        Span { text: text.into(), style, role, url: None }
    }
}

pub fn spans_plain(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

pub fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.chars().map(char_width).sum::<usize>()).sum()
}

/// Truncate styled spans to `w` visible columns; when cutting, append one
/// `…` span carrying the cut span's style/role (like `ansi::truncate_to`,
/// whose result this mirrors for escape-free text). A wide char that would
/// overrun the budget is dropped whole: half a glyph is worse than an
/// early ellipsis.
pub fn truncate_spans(spans: &[Span], w: usize) -> Vec<Span> {
    if spans_width(spans) <= w {
        return spans.to_vec();
    }
    let budget = w.saturating_sub(1); // room for the ellipsis itself
    let mut out: Vec<Span> = Vec::new();
    let mut col = 0usize;
    for s in spans {
        let mut keep = String::new();
        for c in s.text.chars() {
            let cw = char_width(c);
            if col + cw > budget {
                if !keep.is_empty() {
                    out.push(Span { text: keep, style: s.style, role: s.role, url: s.url.clone() });
                }
                out.push(Span::with("…", s.style, s.role));
                return out;
            }
            keep.push(c);
            col += cw;
        }
        if !keep.is_empty() {
            out.push(Span { text: keep, style: s.style, role: s.role, url: s.url.clone() });
        }
    }
    out // unreachable: total > w guarantees a cut, but keep it total
}

/// The byte range `[from, to)` of the concatenated span texts as styled
/// spans (pieces keep style/role/url, empty pieces dropped) — used to cut
/// a pre-wrapped line out of a paragraph at Pango break offsets. Offsets
/// must fall on char boundaries; ranges beyond the text clamp to it.
pub fn spans_slice(spans: &[Span], from: usize, to: usize) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut base = 0usize;
    for s in spans {
        let (a, b) = (from.max(base), to.min(base + s.text.len()));
        if a < b {
            out.push(Span {
                text: s.text[a - base..b - base].to_string(),
                style: s.style,
                role: s.role,
                url: s.url.clone(),
            });
        }
        base += s.text.len();
    }
    out
}

/// Apply an outer style to inner spans (bold outside, code inside, ...).
fn restyle(spans: &mut [Span], style: Style, role: Role) {
    for s in spans.iter_mut() {
        s.style |= style;
        if s.role == Role::None {
            s.role = role;
        }
    }
}

/// One block of the parsed document.
pub enum Line {
    /// A paragraph segment (soft-wrapped lines joined, hard breaks split),
    /// already carrying its list bullet if it is a list item.
    Para {
        spans: Vec<Span>,
        /// Left indent in terminal columns (continuation lines of list items).
        indent: usize,
        /// Width of the bullet marker, for hanging indents.
        hanging: usize,
    },
    /// Heading text; the back end supplies the marker or the font size.
    Heading { lvl: usize, spans: Vec<Span> },
    /// A fenced or indented code block: plain lines plus an info string.
    Code { lang: String, lines: Vec<String> },
    /// A parsed pipe table; see `TableRow` / `table_text`.
    Table(Table),
    /// Blockquote: one bar, inner blocks rendered at a narrower width.
    Quote { inner: Vec<Line> },
    Rule,
    Blank,
}

// ---------------------------------------------------------------- parsing ---

const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"), ("lt", "<"), ("gt", ">"), ("quot", "\""), ("apos", "'"), ("nbsp", "\u{a0}"),
    ("mdash", "—"), ("ndash", "–"), ("hellip", "…"), ("copy", "©"), ("reg", "®"),
    ("trade", "™"), ("deg", "°"), ("plusmn", "±"), ("times", "×"), ("middot", "·"),
    ("rarr", "→"), ("larr", "←"), ("darr", "↓"), ("uarr", "↑"), ("harr", "↔"),
    ("bull", "•"), ("sect", "§"), ("para", "¶"), ("dagger", "†"), ("euro", "€"),
    ("pound", "£"), ("yen", "¥"),
];

fn ref_key(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Parse a reference definition line: `[label]: url "optional title"`.
fn ref_def(l: &str) -> Option<(String, String)> {
    if l.len() - l.trim_start().len() > 3 {
        return None;
    }
    let rest = l.trim_start().strip_prefix('[')?;
    let (label, after) = rest.split_once("]:")?;
    if label.trim().is_empty() || label.starts_with('^') {
        return None;
    }
    let url = after.split_whitespace().next()?.trim_start_matches('<').trim_end_matches('>');
    Some((ref_key(label), url.to_string()))
}

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

fn emit_link(label: &str, target: &str, img: bool, cfg: &Cfg, out: &mut Vec<Span>) {
    if img {
        out.push(Span::with("◈ ", Style::empty(), Role::Image));
        let alt = if label.trim().is_empty() { target } else { label };
        out.push(Span::with(alt, Style::ITALIC, Role::Image));
        if cfg.urls && cfg.color && !label.trim().is_empty() && !target.is_empty() {
            out.push(Span::with(format!(" ({target})"), Style::empty(), Role::Faint));
        }
    } else {
        let mut text = inline_spans(label, cfg);
        restyle(&mut text, Style::empty(), Role::Link);
        for s in text.iter_mut() {
            if s.url.is_none() {
                s.url = Some(target.to_string());
            }
        }
        out.extend(text);
        if cfg.urls && !target.is_empty() && target != label.trim() {
            out.push(Span::with(format!(" ({target})"), Style::empty(), Role::Faint));
        }
    }
}

fn parse_bracket(ch: &[char], i: usize, cfg: &Cfg, out: &mut Vec<Span>) -> Option<usize> {
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
    if j >= ch.len() {
        return None;
    }
    let label: String = ch[o + 1..j].iter().collect();
    if ch.get(j + 1) != Some(&'(') {
        // reference forms: [text][ref], [text][], [ref]
        let (key, next) = if ch.get(j + 1) == Some(&'[') {
            let close = (j + 2..ch.len()).find(|&k| ch[k] == ']')?;
            let r: String = ch[j + 2..close].iter().collect();
            (if r.trim().is_empty() { label.clone() } else { r }, close + 1)
        } else {
            (label.clone(), j + 1)
        };
        let target = cfg.refs.get(&ref_key(&key))?.clone();
        emit_link(&label, &target, img, cfg, out);
        return Some(next);
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
    let target = target.split_whitespace().next().unwrap_or("").trim_matches(['<', '>']);
    emit_link(&label, target, img, cfg, out);
    Some(k + 1)
}

/// Inline markdown (emphasis, code, links, images, entities, escapes,
/// autolinks) as styled spans, for one logical line.
pub fn inline_spans(src: &str, cfg: &Cfg) -> Vec<Span> {
    let ch: Vec<char> = src.trim_end().chars().collect();
    let n = ch.len();
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0usize;
    // Plain text accumulates here and is flushed at every style boundary.
    let mut text = String::new();
    macro_rules! flush {
        () => {
            if !text.is_empty() {
                out.push(Span::with(std::mem::take(&mut text), Style::empty(), Role::None));
            }
        };
    }
    while i < n {
        let c = ch[i];
        if c == '\u{1b}' {
            // pre-existing escapes carry no meaning here: drop them
            let mut sink = String::new();
            copy_escape(&ch, &mut i, &mut sink);
            continue;
        }
        match c {
            '\\' if i + 1 < n && ch[i + 1].is_ascii_punctuation() => {
                text.push(ch[i + 1]);
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
                    inner.retain(|c| c != '\u{1b}');
                    if inner.len() > 2 && inner.starts_with(' ') && inner.ends_with(' ') {
                        inner = inner[1..inner.len() - 1].to_string();
                    }
                    flush!();
                    out.push(Span::with(inner, Style::empty(), Role::Code));
                    i = cj + run;
                } else {
                    text.push('`');
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
                    for (need, style) in [
                        (3, Style::BOLD | Style::ITALIC),
                        (2, Style::BOLD),
                        (1, Style::ITALIC),
                    ] {
                        if run < need {
                            continue;
                        }
                        let tok: Vec<char> = vec![c; need];
                        if let Some(j) = find_closer(&ch, after, &tok) {
                            let inner: String = ch[after..j].iter().collect();
                            flush!();
                            let mut body = inline_spans(&inner, cfg);
                            restyle(&mut body, style, Role::None);
                            out.extend(body);
                            i = j + need;
                            done = true;
                            break;
                        }
                    }
                }
                if !done {
                    text.push(c);
                    i += 1;
                }
            }
            '~' if run_len(&ch, i, '~') >= 2 => {
                let tok = ['~', '~'];
                if let Some(j) = find_closer(&ch, i + 2, &tok) {
                    let inner: String = ch[i + 2..j].iter().collect();
                    flush!();
                    let mut body = inline_spans(&inner, cfg);
                    restyle(&mut body, Style::STRIKE, Role::None);
                    out.extend(body);
                    i = j + 2;
                } else {
                    text.push('~');
                    i += 1;
                }
            }
            '!' | '[' => {
                flush!();
                if let Some(next) = parse_bracket(&ch, i, cfg, &mut out) {
                    i = next;
                } else {
                    text.push(c);
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
                        flush!();
                        let url = body.trim_end_matches('>').trim().to_string();
                        let mut s = Span::with(url.clone(), Style::empty(), Role::Link);
                        s.url = Some(url);
                        out.push(s);
                    }
                    // other HTML tags are dropped
                    i = end + 1;
                } else {
                    text.push(c);
                    i += 1;
                }
            }
            '&' => {
                if let Some((rep, next)) = try_entity(&ch, i) {
                    text.push_str(rep);
                    i = next;
                } else {
                    text.push(c);
                    i += 1;
                }
            }
            _ => {
                text.push(c);
                i += 1;
            }
        }
    }
    flush!();
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

// --------------------------------------------------------------- blocks ---

pub fn parse(src: &str, cfg: &Cfg) -> Vec<Line> {
    let normalized = src.replace('\t', "    ").replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let body = &lines[front_matter_end(&lines).map(|e| e + 1).unwrap_or(0)..];
    let mut cfg = cfg.clone();
    for l in body {
        if let Some((k, v)) = ref_def(l) {
            cfg.refs.entry(k).or_insert(v);
        }
    }
    let mut out = blocks(body, &cfg);
    while matches!(out.last(), Some(Line::Blank)) {
        out.pop();
    }
    out
}

/// YAML (`---`) or TOML (`+++`) front matter: only when every line between the
/// fences looks like a key/value pair, a list item, or a continuation, so a
/// document that merely opens with a horizontal rule is left alone.
fn front_matter_end(lines: &[&str]) -> Option<usize> {
    let fence = lines.first()?.trim();
    let sep = match fence {
        "---" => ':',
        "+++" => '=',
        _ => return None,
    };
    let end = (1..lines.len()).find(|&k| lines[k].trim() == fence)?;
    let keyed = |l: &str| {
        let key: String =
            l.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect();
        !key.is_empty() && l[key.len()..].trim_start().starts_with(sep)
    };
    let body = &lines[1..end];
    let ok = body.iter().any(|l| keyed(l))
        && body.iter().all(|l| {
            l.trim().is_empty()
                || keyed(l)
                || l.starts_with(' ')
                || l.starts_with("- ")
                || (sep == '=' && l.starts_with('['))
        });
    ok.then_some(end)
}

fn push_blank(out: &mut Vec<Line>) {
    if !out.is_empty() && !matches!(out.last(), Some(Line::Blank)) {
        out.push(Line::Blank);
    }
}

enum Marker {
    Bullet,
    Ord(String),
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

fn list_item(l: &str) -> Option<(usize, Marker, String)> {
    let indent = l.chars().take_while(|&c| c == ' ').count();
    let rest = &l[indent..];
    let mut it = rest.chars();
    let first = it.next()?;
    if matches!(first, '-' | '*' | '+') && matches!(it.next(), Some(' ') | None) {
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

fn blocks(lines: &[&str], cfg: &Cfg) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut i = 0usize;
    let mut in_list = false;

    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        if !t.is_empty() && !line.starts_with(' ') && list_item(line).is_none() {
            in_list = false;
        }

        // fenced code
        if let Some((fence, lang)) = fence_open(t) {
            let fc = fence.chars().next().unwrap();
            let mut body: Vec<String> = Vec::new();
            i += 1;
            while i < lines.len() {
                let cl = lines[i].trim();
                if cl.starts_with(&fence) && cl.chars().all(|c| c == fc || c == ' ') {
                    i += 1;
                    break;
                }
                body.push(lines[i].to_string());
                i += 1;
            }
            while body.last().map(|l| l.is_empty()).unwrap_or(false) {
                body.pop();
            }
            out.push(Line::Code { lang, lines: body });
            continue;
        }

        if t.is_empty() {
            push_blank(&mut out);
            i += 1;
            continue;
        }

        // HTML comments, possibly spanning lines
        if t.starts_with("<!--") {
            while i < lines.len() && !lines[i].contains("-->") {
                i += 1;
            }
            i += 1;
            continue;
        }

        if ref_def(line).is_some() {
            i += 1;
            continue;
        }

        // ATX heading
        if t.starts_with('#') {
            let lvl = t.chars().take_while(|&c| c == '#').count();
            if lvl <= 6 && (t.len() == lvl || t.as_bytes()[lvl] == b' ') {
                let text = t[lvl..].trim().trim_end_matches('#').trim_end();
                push_blank(&mut out);
                out.push(Line::Heading { lvl, spans: inline_spans(text, cfg) });
                push_blank(&mut out);
                i += 1;
                continue;
            }
        }

        // setext heading
        if !is_block_start(line) {
            if let Some(lvl) = lines.get(i + 1).and_then(|n| setext_level(n)) {
                push_blank(&mut out);
                out.push(Line::Heading { lvl, spans: inline_spans(t, cfg) });
                push_blank(&mut out);
                i += 2;
                continue;
            }
        }

        if hr_detect(t) {
            push_blank(&mut out);
            out.push(Line::Rule);
            push_blank(&mut out);
            i += 1;
            continue;
        }

        // table
        if is_table_start(lines, i) {
            let (table, used) = table_layout(lines, i, cfg);
            out.push(Line::Table(table));
            push_blank(&mut out);
            i += used;
            continue;
        }

        // blockquote (one bar at this level; `> >` nests through recursion)
        if t.starts_with('>') {
            let mut inner: Vec<String> = Vec::new();
            while i < lines.len() {
                let lt = lines[i].trim_start();
                if let Some(rest) = lt.strip_prefix('>') {
                    inner.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    i += 1;
                } else if !inner.last().map(|x| x.trim().is_empty()).unwrap_or(true)
                    && !para_ends(lines, i)
                {
                    inner.push(lines[i].to_string()); // lazy continuation
                    i += 1;
                } else {
                    break;
                }
            }
            let inner: Vec<&str> = inner.iter().map(String::as_str).collect();
            let mut quoted = blocks(&inner, &cfg.narrow(2));
            while matches!(quoted.last(), Some(Line::Blank)) {
                quoted.pop();
            }
            out.push(Line::Quote { inner: quoted });
            continue;
        }

        // list item
        if let Some((indent, marker, rest)) = list_item(line) {
            in_list = true;
            let depth = (indent / 2).min(6);
            let pad = "  ".repeat(depth);
            let mut text = rest;
            i += 1;
            while i < lines.len() && !para_ends(lines, i) {
                text.push(' ');
                text.push_str(lines[i].trim());
                i += 1;
            }
            let (content, task) = strip_task(&text);
            let mut bullet: Vec<Span> = Vec::new();
            match marker {
                Marker::Bullet => {
                    bullet.push(Span::with(["•", "◦", "▪"][depth % 3], Style::empty(), Role::Bullet));
                }
                Marker::Ord(num) => {
                    bullet.push(Span::with(num, Style::BOLD, Role::Bullet));
                }
            }
            bullet.push(Span::new(" "));
            match task {
                Some(true) => bullet.push(Span::with("☑ ", Style::BOLD, Role::Done)),
                Some(false) => bullet.push(Span::with("☐ ", Style::empty(), Role::Todo)),
                None => {}
            }
            let hanging = spans_width(&bullet);
            let mut spans = bullet;
            spans.extend(inline_spans(content, cfg));
            out.push(Line::Para { spans, indent: pad.chars().count(), hanging });
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        // indented content after a list item belongs to that item
        if in_list && indent > 0 {
            let (rows, next) = paragraph(lines, i, cfg, indent.min(12));
            out.extend(rows);
            i = next;
            continue;
        }

        // indented code block
        if indent >= 4 {
            let mut block: Vec<String> = Vec::new();
            while i < lines.len() && (lines[i].starts_with("    ") || lines[i].trim().is_empty()) {
                block.push(lines[i].get(4..).unwrap_or("").to_string());
                i += 1;
            }
            while block.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                block.pop();
                i -= 1; // trailing blank lines belong to what follows
            }
            out.push(Line::Code { lang: String::new(), lines: block });
            continue;
        }

        let (rows, next) = paragraph(lines, i, cfg, 0);
        out.extend(rows);
        i = next;
    }
    out
}

/// Collect a paragraph starting at `start`, joining soft-wrapped source lines
/// and splitting on hard breaks (two trailing spaces or a trailing backslash).
fn paragraph(lines: &[&str], start: usize, cfg: &Cfg, indent: usize) -> (Vec<Line>, usize) {
    let mut segs: Vec<String> = vec![String::new()];
    let mut j = start;
    while j < lines.len() && (j == start || !para_ends(lines, j)) {
        let l = lines[j];
        let lt = l.trim();
        let hard_bs = lt.ends_with('\\') && !lt.ends_with("\\\\");
        let hard = l.ends_with("  ") || hard_bs;
        let piece = if hard_bs { &lt[..lt.len() - 1] } else { lt };
        let cur = segs.last_mut().unwrap();
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(piece);
        if hard {
            segs.push(String::new());
        }
        j += 1;
        // the last line before a setext underline becomes that heading
        if lines.get(j + 1).and_then(|n| setext_level(n)).is_some() && !para_ends(lines, j) {
            break;
        }
    }
    let mut rows = Vec::new();
    for seg in segs {
        let spans = inline_spans(&seg, cfg);
        if spans_plain(&spans).trim().is_empty() {
            continue; // e.g. a line made only of HTML tags
        }
        rows.push(Line::Para { spans, indent, hanging: 0 });
    }
    (rows, j)
}

/// Whether line `j` cannot continue the paragraph or list item above it.
fn para_ends(lines: &[&str], j: usize) -> bool {
    let l = lines[j];
    let t = l.trim();
    t.is_empty()
        || is_block_start(l)
        || hr_detect(t)
        || t.starts_with("<!--")
        || ref_def(l).is_some()
        || is_table_start(lines, j)
        || setext_level(l).is_some()
}

fn setext_level(l: &str) -> Option<usize> {
    let t = l.trim();
    if !t.is_empty() && t.chars().all(|c| c == '=') {
        Some(1)
    } else if t.len() >= 2 && t.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// An opening code fence (3+ backticks or tildes): returns the fence and the
/// language (first word of the info string). A backtick fence cannot have
/// backticks in its info string; such a line is inline code instead.
fn fence_open(t: &str) -> Option<(String, String)> {
    let fc = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let fence: String = t.chars().take_while(|&c| c == fc).collect();
    let info = &t[fence.len()..];
    if fence.len() < 3 || (fc == '`' && info.contains('`')) {
        return None;
    }
    Some((fence, info.split_whitespace().next().unwrap_or("").to_string()))
}

fn is_table_start(lines: &[&str], i: usize) -> bool {
    lines[i].contains('|') && lines.get(i + 1).map(|l| is_sep_row(l.trim())).unwrap_or(false)
}

fn is_block_start(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with('#')
        || t.starts_with('>')
        || fence_open(t).is_some()
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
pub enum Align {
    Left,
    Center,
    Right,
}

/// One row of a laid-out table: a box-drawing border, or its styled cells.
pub enum TableRow {
    Top,
    Sep,
    Bottom,
    Cells { header: bool, cells: Vec<Vec<Span>> },
}

pub struct Table {
    pub widths: Vec<usize>,
    pub aligns: Vec<Align>,
    pub rows: Vec<TableRow>,
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

/// Parse and measure a pipe table starting at `start`. Returns the layout and
/// how many source lines it occupied. Cell spans carry no styling of their own
/// (colors), only inline structure, so each back end can add its own.
fn table_layout(lines: &[&str], start: usize, cfg: &Cfg) -> (Table, usize) {
    let pcfg = Cfg { color: false, ..cfg.clone() };
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut i = start;
    let mut aligns: Vec<Align> = Vec::new();
    let mut used = 0usize;
    while i < lines.len() {
        if !lines[i].contains('|') {
            break;
        }
        let t = lines[i].trim().trim_matches('|');
        let cells: Vec<String> =
            split_cells(t).into_iter().map(|c| c.trim().to_string()).collect();
        if i == start + 1 {
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
            used += 1;
            continue;
        }
        rows.push(cells);
        i += 1;
        used += 1;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    rows.iter_mut().for_each(|r| r.resize(cols, String::new()));
    aligns.resize(cols, Align::Left);

    let cells: Vec<Vec<Vec<Span>>> = rows
        .iter()
        .map(|r| r.iter().map(|c| inline_spans(c, &pcfg)).collect())
        .collect();
    let plain_width = |s: &Vec<Span>| spans_plain(s).chars().map(char_width).sum::<usize>();
    let mut widths: Vec<usize> = Vec::new();
    for c in 0..cols {
        let mut wmax = 3usize;
        for ri in 0..cells.len() {
            wmax = wmax.max(plain_width(&cells[ri][c]));
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

    let mut out_rows: Vec<TableRow> = Vec::new();
    if cells.len() >= 2 {
        out_rows.push(TableRow::Top);
        out_rows.push(TableRow::Cells { header: true, cells: cells[0].clone() });
        out_rows.push(TableRow::Sep);
        for row in cells.iter().skip(1) {
            out_rows.push(TableRow::Cells { header: false, cells: row.clone() });
        }
        out_rows.push(TableRow::Bottom);
    }
    (Table { widths, aligns, rows: out_rows }, used)
}

/// One drawn fragment of a laid-out table row, so each back end can frame,
/// header and cells with different styling.
#[derive(Debug)]
pub enum TablePiece {
    /// Box-drawing text: a border line, or a `│` column separator.
    Frame(String),
    /// A cell truncated to its column width. `pad_l`/`pad_r` are the
    /// space gutters around it (including the one standard space), split
    /// per column alignment so a back end can paint padding with plain
    /// styling instead of e.g. an inline-code background.
    Cell {
        spans: Vec<Span>,
        pad_l: usize,
        pad_r: usize,
        header: bool,
    },
}

/// Lay a table out row by row as styled pieces; the single pad/truncate
/// math shared by the plain (`table_text`) and styled back ends.
pub fn table_pieces(t: &Table) -> Vec<Vec<TablePiece>> {
    let cols = t.widths.len();
    let border = |l: char, m: char, r: char| -> String {
        let segs: Vec<String> = t.widths.iter().map(|w| "─".repeat(w + 2)).collect();
        format!("{l}{}{r}", segs.join(&m.to_string()))
    };
    let mut out: Vec<Vec<TablePiece>> = Vec::new();
    for row in &t.rows {
        let pieces = match row {
            TableRow::Top => vec![TablePiece::Frame(border('╭', '┬', '╮'))],
            TableRow::Sep => vec![TablePiece::Frame(border('├', '┼', '┤'))],
            TableRow::Bottom => vec![TablePiece::Frame(border('╰', '┴', '╯'))],
            TableRow::Cells { header, cells } => {
                let mut p = vec![TablePiece::Frame("│".to_string())];
                for c in 0..cols {
                    let spans = cells
                        .get(c)
                        .map(|sp| truncate_spans(sp, t.widths[c]))
                        .unwrap_or_default();
                    let extra = t.widths[c].saturating_sub(spans_width(&spans));
                    let (pad_l, pad_r) = match t.aligns.get(c).copied().unwrap_or(Align::Left) {
                        Align::Right => (1 + extra, 1),
                        Align::Center => (1 + extra / 2, 1 + extra - extra / 2),
                        Align::Left => (1, 1 + extra),
                    };
                    p.push(TablePiece::Cell { spans, pad_l, pad_r, header: *header });
                    p.push(TablePiece::Frame("│".to_string()));
                }
                p
            }
        };
        out.push(pieces);
    }
    out
}

/// Lay a table out as plain box-drawn text rows (the terminal's plain
/// path; the GUI consumes `table_pieces` directly). Rendered from
/// `table_pieces` so both paths share one code path and cannot drift apart.
pub fn table_text(t: &Table) -> Vec<String> {
    table_pieces(t)
        .into_iter()
        .map(|row| {
            let mut s = String::new();
            for p in row {
                match p {
                    TablePiece::Frame(f) => s.push_str(&f),
                    TablePiece::Cell { spans, pad_l, pad_r, .. } => {
                        s.push_str(&" ".repeat(pad_l));
                        s.push_str(&spans_plain(&spans));
                        s.push_str(&" ".repeat(pad_r));
                    }
                }
            }
            s
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_spans_fits_untouched() {
        let spans = vec![Span::with("abc", Style::BOLD, Role::Code)];
        let cut = truncate_spans(&spans, 10);
        assert_eq!(spans_plain(&cut), "abc");
        assert_eq!(cut.len(), 1);
        assert_eq!(spans_plain(&cut), spans_plain(&spans)); // cloned, no ellipsis
    }

    #[test]
    fn truncate_spans_ascii_cut_adds_one_ellipsis() {
        let cut = truncate_spans(&[Span::new("hello world")], 5);
        assert_eq!(spans_plain(&cut), "hell…");
        assert_eq!(cut.iter().filter(|s| s.text == "…").count(), 1);
        assert_eq!(spans_width(&cut), 5);
    }

    #[test]
    fn truncate_spans_cjk_cut_never_exceeds_or_splits() {
        // budget 3: 漢 fits (2), the next 字 would hit 4 > 3 and is dropped whole
        let cut = truncate_spans(&[Span::new("漢字漢")], 4);
        assert_eq!(spans_plain(&cut), "漢…");
        assert!(spans_width(&cut) <= 4);
        assert!(spans_plain(&cut).contains('…'));
    }

    #[test]
    fn truncate_spans_keeps_style_of_the_cut_span() {
        let spans = vec![Span::new("ab"), Span::with("cdef", Style::BOLD, Role::Code)];
        let cut = truncate_spans(&spans, 4);
        assert_eq!(spans_plain(&cut), "abc…");
        assert_eq!(spans_width(&cut), 4);
        assert_eq!(cut[0].style, Style::empty()); // kept prefix of span 0
        assert_eq!(cut[1].style, Style::BOLD); // kept prefix of span 1
        assert_eq!(cut[1].role, Role::Code);
        assert_eq!(cut[2].text, "…");
        assert_eq!(cut[2].style, Style::BOLD); // ellipsis carries the cut span's style
        assert_eq!(cut[2].role, Role::Code);
        assert!(cut[2].url.is_none());
    }

    fn demo_table() -> Table {
        Table {
            widths: vec![5, 7],
            aligns: vec![Align::Left, Align::Right],
            rows: vec![
                TableRow::Top,
                TableRow::Cells {
                    header: true,
                    cells: vec![vec![Span::new("ab")], vec![Span::new("cd")]],
                },
                TableRow::Sep,
                TableRow::Cells {
                    header: false,
                    cells: vec![vec![Span::new("hello there")], vec![Span::new("z")]],
                },
                TableRow::Bottom,
            ],
        }
    }

    #[test]
    fn table_pieces_row_shapes() {
        let rows = table_pieces(&demo_table());
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].len(), 1); // Top: a single border frame
        match &rows[0][0] {
            TablePiece::Frame(f) => assert!(f.starts_with('╭') && f.contains('┬')),
            _ => panic!("Top row must be one Frame"),
        }
        // Cells rows: Frame │ (Cell Frame)×cols, and the header flag survives
        for (ri, header) in [(1usize, true), (3usize, false)] {
            for (pi, p) in rows[ri].iter().enumerate() {
                match (pi % 2 == 0, p) {
                    (true, TablePiece::Frame(f)) => assert_eq!(f, "│", "row {ri} piece {pi}"),
                    (false, TablePiece::Cell { header: h, .. }) => assert_eq!(*h, header),
                    _ => panic!("row {ri} piece {pi} has the wrong kind"),
                }
            }
            assert_eq!(rows[ri].len(), 2 * 2 + 1);
        }
    }

    #[test]
    fn table_pieces_pads_per_alignment() {
        let mut t = demo_table();
        t.widths = vec![5, 5, 5];
        t.aligns = vec![Align::Left, Align::Center, Align::Right];
        let x = vec![Span::new("x")];
        t.rows = vec![TableRow::Cells { header: false, cells: vec![x.clone(), x.clone(), x] }];
        let pads: Vec<(usize, usize)> = table_pieces(&t)
            .remove(0)
            .iter()
            .filter_map(|p| match p {
                TablePiece::Cell { spans, pad_l, pad_r, .. } => {
                    // gutters + cell always fill the column plus the two standard spaces
                    assert_eq!(pad_l + pad_r + spans_width(spans), 5 + 2);
                    Some((*pad_l, *pad_r))
                }
                _ => None,
            })
            .collect();
        assert_eq!(pads, vec![(1, 5), (3, 3), (5, 1)]);
    }

    #[test]
    fn table_pieces_truncates_long_cells() {
        let rows = table_pieces(&demo_table());
        match &rows[3][1] {
            TablePiece::Cell { spans, .. } => {
                assert_eq!(spans_plain(spans), "hell…");
                assert!(spans_width(spans) <= 5);
            }
            _ => panic!("piece 1 of a Cells row must be a Cell"),
        }
    }

    #[test]
    fn table_text_rebuilds_from_pieces() {
        let t = demo_table();
        let text = table_text(&t);
        assert_eq!(text[0], format!("╭{}┬{}╮", "─".repeat(7), "─".repeat(9)));
        assert!(text[2].starts_with('├') && text[2].contains('┼'));
        assert!(text[4].starts_with('╰') && text[4].ends_with('╯'));
        // golden cell line: pins the shared pad/truncate math byte for byte
        assert_eq!(text[1], "│ ab    │      cd │");
        // concatenating pieces must reproduce table_text exactly (this is
        // what the Phase 1 GUI emitter will do instead of pushing strings)
        for (pieces, line) in table_pieces(&t).iter().zip(&text) {
            let mut s = String::new();
            for p in pieces {
                match p {
                    TablePiece::Frame(f) => s.push_str(f),
                    TablePiece::Cell { spans, pad_l, pad_r, .. } => {
                        s.push_str(&" ".repeat(*pad_l));
                        s.push_str(&spans_plain(spans));
                        s.push_str(&" ".repeat(*pad_r));
                    }
                }
            }
            assert_eq!(&s, line);
        }
    }

    #[test]
    fn spans_slice_cuts_stay_styled() {
        let spans = vec![
            Span::with("hello", Style::BOLD, Role::None),
            Span::with(" world", Style::empty(), Role::Code),
        ];
        let mid = spans_slice(&spans, 1, 4); // inside one span
        assert_eq!(spans_plain(&mid), "ell");
        assert_eq!(mid[0].style, Style::BOLD);
        let cross = spans_slice(&spans, 3, 8); // across the style boundary
        assert_eq!(spans_plain(&cross), "lo wo");
        assert_eq!(cross.len(), 2);
        assert_eq!(cross[0].style, Style::BOLD);
        assert_eq!(cross[1].role, Role::Code);
    }

    #[test]
    fn spans_slice_clamps_and_empties() {
        let spans = vec![Span::new("abc"), Span::new("def")];
        assert_eq!(spans_plain(&spans_slice(&spans, 0, 100)), "abcdef");
        assert!(spans_slice(&spans, 9, 12).is_empty()); // past the end
        assert!(spans_slice(&spans, 4, 4).is_empty()); // empty range
        assert!(spans_slice(&spans, 4, 2).is_empty()); // inverted range
    }
}
