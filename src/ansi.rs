// ANSI helpers: escaping, stripping, display widths, and terminal-width wrapping.
// Wrapping is escape-aware: it never splits a sequence and it re-opens the style
// state on each continuation line so colors survive the wrap.

pub const RESET: &str = "\x1b[0m";

pub fn sg(code: &str) -> String {
    format!("\x1b[{code}m")
}

/// Zero-width code points: combining marks, joiners, variation selectors.
const ZERO: &[(u32, u32)] = &[
    (0x0300, 0x036F), (0x0483, 0x0489), (0x0591, 0x05BD), (0x05BF, 0x05BF), (0x05C1, 0x05C2),
    (0x05C4, 0x05C5), (0x05C7, 0x05C7), (0x0610, 0x061A), (0x064B, 0x065F), (0x0670, 0x0670),
    (0x06D6, 0x06DC), (0x06DF, 0x06E4), (0x06E7, 0x06E8), (0x06EA, 0x06ED), (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A), (0x0E47, 0x0E4E), (0x1AB0, 0x1AFF), (0x1DC0, 0x1DFF), (0x200B, 0x200F),
    (0x2060, 0x2064), (0x20D0, 0x20FF), (0xFE00, 0xFE0F), (0xFE20, 0xFE2F), (0xFEFF, 0xFEFF),
    (0xE0000, 0xE007F), (0xE0100, 0xE01EF),
];

/// Double-width code points (Unicode 16 East Asian Width "W"/"F"): CJK, fullwidth
/// forms, and emoji with default emoji presentation. Generated from Python's
/// unicodedata; ranges are merged across unassigned gaps.
const WIDE: &[(u32, u32)] = &[
    (0x1100, 0x115F), (0x231A, 0x231B), (0x2329, 0x232A), (0x23E9, 0x23EC), (0x23F0, 0x23F0),
    (0x23F3, 0x23F3), (0x25FD, 0x25FE), (0x2614, 0x2615), (0x2630, 0x2637), (0x2648, 0x2653),
    (0x267F, 0x267F), (0x268A, 0x268F), (0x2693, 0x2693), (0x26A1, 0x26A1), (0x26AA, 0x26AB),
    (0x26BD, 0x26BE), (0x26C4, 0x26C5), (0x26CE, 0x26CE), (0x26D4, 0x26D4), (0x26EA, 0x26EA),
    (0x26F2, 0x26F3), (0x26F5, 0x26F5), (0x26FA, 0x26FA), (0x26FD, 0x26FD), (0x2705, 0x2705),
    (0x270A, 0x270B), (0x2728, 0x2728), (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755),
    (0x2757, 0x2757), (0x2795, 0x2797), (0x27B0, 0x27B0), (0x27BF, 0x27BF), (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x2E80, 0x303E), (0x3041, 0x3247), (0x3250, 0xA4C6),
    (0xA960, 0xA97C), (0xAC00, 0xD7A3), (0xF900, 0xFAD9), (0xFE10, 0xFE19), (0xFE30, 0xFE6B),
    (0xFF01, 0xFF60), (0xFFE0, 0xFFE6), (0x16FE0, 0x1B2FB), (0x1D300, 0x1D376), (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF), (0x1F18E, 0x1F18E), (0x1F191, 0x1F19A), (0x1F200, 0x1F320),
    (0x1F32D, 0x1F335), (0x1F337, 0x1F37C), (0x1F37E, 0x1F393), (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3), (0x1F3E0, 0x1F3F0), (0x1F3F4, 0x1F3F4), (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440), (0x1F442, 0x1F4FC), (0x1F4FF, 0x1F53D), (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567), (0x1F57A, 0x1F57A), (0x1F595, 0x1F596), (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F), (0x1F680, 0x1F6C5), (0x1F6CC, 0x1F6CC), (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6DF), (0x1F6EB, 0x1F6EC), (0x1F6F4, 0x1F6FC), (0x1F7E0, 0x1F7F0),
    (0x1F90C, 0x1F93A), (0x1F93C, 0x1F945), (0x1F947, 0x1F9FF), (0x1FA70, 0x1FAF8),
    (0x20000, 0x3FFFD),
];

fn in_table(table: &[(u32, u32)], u: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < u {
                std::cmp::Ordering::Less
            } else if lo > u {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Terminal display width of one character: 0, 1 or 2 columns.
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x300 {
        return (u != 0) as usize; // fast path for ASCII and Latin-1
    }
    if in_table(ZERO, u) {
        0
    } else if in_table(WIDE, u) {
        2
    } else {
        1
    }
}

/// Byte ranges of maximal runs of double-width characters (CJK, emoji):
/// these runs are the ones whose pixel advance cannot be trusted, because
/// a fallback font rarely hits exactly 2x the mono advance.
#[allow(dead_code)] // consumed by the Phase 3 per-run letter-spacing fit
pub fn wide_runs(s: &str) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    for (b, c) in s.char_indices() {
        if char_width(c) == 2 {
            match &mut run {
                Some(r) => r.1 = b + c.len_utf8(),
                None => run = Some((b, b + c.len_utf8())),
            }
        } else if let Some(r) = run.take() {
            out.push(r); // any other char (even a zero-width one) breaks a run
        }
    }
    out.extend(run);
    out
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
    if s.contains('\u{1b}') {
        out.push_str(RESET);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_runs_are_byte_ranges() {
        assert_eq!(wide_runs(""), Vec::new());
        assert_eq!(wide_runs("ab漢c"), vec![(2, 5)]);
        // 👍 is a 4-byte char; 漢字 merge into one run
        assert_eq!(wide_runs("👍x漢字"), vec![(0, 4), (5, 11)]);
    }

    #[test]
    fn wide_runs_break_on_narrow_and_zero_width() {
        assert_eq!(wide_runs("жд"), Vec::new()); // Cyrillic is single-width
        assert_eq!(wide_runs("漢\u{301}漢"), vec![(0, 3), (5, 8)]); // combining mark splits
    }
}
