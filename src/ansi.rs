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

/// Soft-wrap a styled line into rows of at most `width` columns.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if visible_width(s) <= width {
        return vec![s.to_string()];
    }
    let ch: Vec<char> = s.chars().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut active: Vec<String> = Vec::new(); // SGR params still in effect
    let mut col = 0usize;
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i] == '\u{1b}' {
            if let Some(params) = copy_escape(&ch, &mut i, &mut cur) {
                for p in params.split(';').filter(|p| !p.is_empty() && *p != "0") {
                    if p.starts_with("3") && !p.starts_with("38") {
                        active.retain(|a| !(a.starts_with('3') && !a.starts_with("38")));
                    } else if p == "4" || p == "24" {
                        active.retain(|a| a != "4");
                    } else {
                        active.retain(|a| a != p);
                    }
                    if p != "0" {
                        active.push(p.to_string());
                    }
                }
                if params.split(';').any(|p| p == "0") || params.is_empty() {
                    active.clear();
                    cur.push_str(RESET);
                }
            }
            continue;
        }
        let cw = char_width(ch[i]);
        if col + cw > width {
            // style state carried into the new row
            let reopen: String = active.iter().map(|p| sg(p)).collect();
            rows.push(std::mem::take(&mut cur));
            cur = reopen;
            col = 0;
            if ch[i] == ' ' {
                i += 1; // don't start a row with a space
                continue;
            }
        }
        cur.push(ch[i]);
        col += cw;
        i += 1;
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
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
