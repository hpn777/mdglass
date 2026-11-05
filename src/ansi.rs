// ANSI helpers: escaping, stripping, display widths, and terminal-width wrapping.
// Wrapping is escape-aware: it never splits a sequence and it re-opens the style
// state on each continuation line so colors survive the wrap.

pub const RESET: &str = "\x1b[0m";

pub fn sg(code: &str) -> String {
    format!("\x1b[{code}m")
}

/// Wrap already-rendered text in an SGR code, restoring base style after resets.
pub fn restyled(inner: &str, base: &str) -> String {
    format!("{base}{}{RESET}", inner.replace(RESET, &format!("{RESET}{base}")))
}

/// Rough single-character display width (zero for combining marks, two for CJK/fullwidth/emoji).
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if u == 0 {
        return 0;
    }
    if (0x0300..=0x036F).contains(&u)
        || (0x1AB0..=0x1AFF).contains(&u)
        || (0x200B..=0x200F).contains(&u)
        || u == 0x200D
        || (0xFE00..=0xFE0F).contains(&u)
    {
        return 0;
    }
    if (0x1100..=0x115F).contains(&u)
        || (0x2E80..=0x303E).contains(&u)
        || (0x3041..=0x33FF).contains(&u)
        || (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xA000..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE30..=0xFE4F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1F64F).contains(&u)
        || (0x1F900..=0x1F9FF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
    {
        return 2;
    }
    1
}

/// Copy one escape sequence starting at `i` into `out`. Returns the SGR params
/// when the sequence is a color/style change (`ESC [ ... m`).
pub fn copy_escape(ch: &[char], i: &mut usize, out: &mut String) -> Option<String> {
    let n = ch.len();
    let mut j = *i + 1; // past ESC
    if j < n && ch[j] == '[' {
        j += 1;
        let start = j;
        while j < n && !(('\u{40}'..='\u{7e}').contains(&ch[j])) {
            j += 1;
        }
        let end = j.min(n);
        let params: String = ch[start..end].iter().collect();
        let is_sgr = end < n && ch[end] == 'm';
        out.extend(std::iter::once(ch[*i])); // ESC
        out.extend(ch[*i + 1..=end.min(n - 1)].iter());
        *i = end + 1;
        if is_sgr { Some(params) } else { None }
    } else if j < n && ch[j] == ']' {
        // OSC: terminated by BEL or ST (ESC backslash)
        j += 1;
        while j < n {
            if ch[j] == '\u{7}' {
                j += 1;
                break;
            }
            if ch[j] == '\u{1b}' && j + 1 < n && ch[j + 1] == '\\' {
                j += 2;
                break;
            }
            j += 1;
        }
        out.extend(ch[*i..j].iter());
        *i = j;
        None
    } else {
        out.extend(ch[*i..(*i + 2).min(n)].iter());
        *i = (*i + 2).min(n);
        None
    }
}

pub fn strip_ansi(s: &str) -> String {
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < ch.len() {
        if ch[i] == '\u{1b}' {
            copy_escape(&ch, &mut i, &mut String::new()); // discard the sequence
        } else {
            out.push(ch[i]);
            i += 1;
        }
    }
    out
}

pub fn visible_width(s: &str) -> usize {
    let ch: Vec<char> = s.chars().collect();
    let mut w = 0;
    let mut i = 0;
    while i < ch.len() {
        if ch[i] == '\u{1b}' {
            copy_escape(&ch, &mut i, &mut String::new());
        } else {
            w += char_width(ch[i]);
            i += 1;
        }
    }
    w
}

/// Truncate a styled string to `w` visible columns, adding an ellipsis when cut.
pub fn truncate_to(s: &str, w: usize) -> String {
    if visible_width(s) <= w {
        return s.to_string();
    }
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut col = 0;
    let mut i = 0;
    let budget = w.saturating_sub(1);
    while i < ch.len() {
        if ch[i] == '\u{1b}' {
            copy_escape(&ch, &mut i, &mut out);
            continue;
        }
        let cw = char_width(ch[i]);
        if col + cw > budget {
            break;
        }
        out.push(ch[i]);
        col += cw;
        i += 1;
    }
    out.push('…');
    out.push_str(RESET);
    out
}

/// Append spaces until the line reaches `w` columns (no-op when already wider).
pub fn pad_to(s: &str, w: usize) -> String {
    let pad = w.saturating_sub(visible_width(s));
    format!("{s}{}", " ".repeat(pad))
}

/// True when an SGR parameter list contains a reset (`0` or empty), skipping
/// over the arguments of extended colors so `38;5;0` is not mistaken for one.
fn sgr_resets(params: &str) -> bool {
    let ps: Vec<&str> = params.split(';').collect();
    let mut k = 0;
    while k < ps.len() {
        match ps[k] {
            "" | "0" => return true,
            "38" | "48" | "58" => k += if ps.get(k + 1) == Some(&"2") { 5 } else { 3 },
            _ => k += 1,
        }
    }
    false
}

#[derive(PartialEq)]
enum Kind {
    Esc,
    Space,
    Glyph,
}

/// Rows under construction. Tracks the SGR sequences applied since the last
/// reset and any open OSC 8 link so each new row can re-open them verbatim.
struct Rows {
    width: usize,
    indent: String,
    indent_w: usize,
    out: Vec<String>,
    cur: String,
    col: usize,
    sgr: Vec<String>,
    link: Option<String>,
}

impl Rows {
    fn emit(&mut self, text: &str, w: usize, kind: &Kind) {
        if *kind == Kind::Esc {
            if let Some(params) = text.strip_prefix("\u{1b}[").and_then(|t| t.strip_suffix('m')) {
                if sgr_resets(params) {
                    self.sgr.clear();
                }
                if !params.split(';').all(|p| p.is_empty() || p == "0") {
                    self.sgr.push(text.to_string());
                }
            } else if let Some(rest) = text.strip_prefix("\u{1b}]8;") {
                let uri = rest.split_once(';').map(|(_, u)| u).unwrap_or("");
                let uri = uri.trim_end_matches(['\u{7}', '\\']).trim_end_matches('\u{1b}');
                self.link = if uri.is_empty() { None } else { Some(text.to_string()) };
            }
        }
        self.cur.push_str(text);
        self.col += w;
    }

    fn newline(&mut self) {
        if self.link.is_some() {
            self.cur.push_str("\u{1b}]8;;\u{1b}\\");
        }
        if !self.sgr.is_empty() || self.link.is_some() {
            self.cur.push_str(RESET);
        }
        self.out.push(std::mem::take(&mut self.cur));
        self.cur = self.indent.clone();
        self.cur.extend(self.sgr.iter().map(String::as_str));
        if let Some(l) = &self.link {
            self.cur.push_str(l);
        }
        self.col = self.indent_w;
    }
}

/// Word-wrap a styled line into rows of at most `width` columns.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    wrap_indent(s, width, "")
}

/// Word-wrap a styled line; continuation rows start with `indent` and re-open
/// the styling and hyperlink active at the break. Words longer than a row are
/// split hard.
pub fn wrap_indent(s: &str, width: usize, indent: &str) -> Vec<String> {
    let width = width.max(1);
    if visible_width(s) <= width {
        return vec![s.to_string()];
    }
    let mut indent_w = visible_width(indent);
    let indent = if indent_w * 2 > width {
        indent_w = 0;
        ""
    } else {
        indent
    };

    // tokens are slices of `s`: (text, display width, kind)
    let ch: Vec<char> = s.chars().collect();
    let mut byte_at: Vec<usize> = s.char_indices().map(|(b, _)| b).collect();
    byte_at.push(s.len());
    let mut toks: Vec<(&str, usize, Kind)> = Vec::with_capacity(ch.len());
    let mut sink = String::new();
    let mut i = 0;
    while i < ch.len() {
        let start = i;
        if ch[i] == '\u{1b}' {
            copy_escape(&ch, &mut i, &mut sink);
            sink.clear();
            toks.push((&s[byte_at[start]..byte_at[i]], 0, Kind::Esc));
        } else {
            let kind = if ch[i] == ' ' { Kind::Space } else { Kind::Glyph };
            i += 1;
            toks.push((&s[byte_at[start]..byte_at[i]], char_width(ch[start]), kind));
        }
    }

    let mut rows = Rows {
        width,
        indent: indent.to_string(),
        indent_w,
        out: Vec::new(),
        cur: String::new(),
        col: 0,
        sgr: Vec::new(),
        link: None,
    };
    let mut k = 0;
    while k < toks.len() {
        let gap = k;
        while k < toks.len() && toks[k].2 != Kind::Glyph {
            k += 1;
        }
        let word = k;
        while k < toks.len() && toks[k].2 != Kind::Space {
            k += 1;
        }
        let gap_w: usize = toks[gap..word].iter().map(|t| t.1).sum();
        let word_w: usize = toks[word..k].iter().map(|t| t.1).sum();

        if rows.col + gap_w + word_w <= rows.width {
            for (text, w, kind) in &toks[gap..k] {
                rows.emit(text, *w, kind);
            }
            continue;
        }
        // Break here: drop the spaces but keep their escapes for style state.
        for (text, _, kind) in &toks[gap..word] {
            if *kind == Kind::Esc {
                rows.emit(text, 0, kind);
            }
        }
        if word_w > 0 && rows.col > rows.indent_w {
            rows.newline();
        }
        for (text, w, kind) in &toks[word..k] {
            if *kind == Kind::Glyph && rows.col + w > rows.width && rows.col > rows.indent_w {
                rows.newline();
            }
            rows.emit(text, *w, kind);
        }
    }
    if !rows.cur.is_empty() || rows.out.is_empty() {
        rows.out.push(rows.cur);
    }
    rows.out
}

/// Push every visible char of `s` through `f` while copying escape sequences
/// verbatim; `f` receives the running index of visible chars.
pub fn map_visible(s: &str, mut f: impl FnMut(char, &mut String, usize)) -> String {
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut p = 0;
    while i < ch.len() {
        if ch[i] == '\u{1b}' {
            copy_escape(&ch, &mut i, &mut out);
        } else {
            f(ch[i], &mut out, p);
            p += 1;
            i += 1;
        }
    }
    out
}
