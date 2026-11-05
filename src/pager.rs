// Built-in pager: less-style key bindings, incremental search with match
// highlighting, multi-file navigation, and resize-aware redraw.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::thread::sleep;
use std::time::Duration;

use termion::event::{parse_event, Event, Key};
use termion::raw::IntoRawMode;
use termion::{async_stdin, AsyncReader};

use crate::ansi::{map_visible, pad_to, sg, strip_ansi, truncate_to, visible_width, wrap, RESET};
use crate::render::{render, Cfg};

/// Non-blocking key pump over the controlling TTY (works even when stdin is piped).
struct Input {
    reader: AsyncReader,
    queue: VecDeque<Event>,
}

impl Input {
    fn new() -> Input {
        Input { reader: async_stdin(), queue: VecDeque::new() }
    }

    fn poll(&mut self) -> Option<Event> {
        while self.queue.is_empty() {
            let mut buf = [0u8; 256];
            match self.reader.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => {
                    let mut iter = buf[..n].iter().copied().map(Ok::<u8, io::Error>);
                    while let Some(Ok(b)) = iter.next() {
                        match parse_event(b, &mut iter) {
                            Ok(e) => self.queue.push_back(e),
                            Err(_) => break,
                        }
                    }
                }
            }
        }
        self.queue.pop_front()
    }
}


pub struct FileDoc {
    name: String,
    content: String,
    lines: Vec<String>, // styled rows, already wrapped to terminal width
    plain: Vec<String>, // lowercase, escape-free text (one entry per line) for search
    top: usize,
}

fn fit(doc: &mut FileDoc, width: usize) {
    let cfg = Cfg { color: true, width, urls: true };
    let logical = render(&doc.content, &cfg);
    doc.lines = logical.iter().flat_map(|l| wrap(l, width.max(1))).collect();
    doc.plain = doc.lines.iter().map(|l| strip_ansi(l).to_lowercase()).collect();
    doc.top = doc.top.min(doc.lines.len().saturating_sub(1));
}

pub fn run(files: &[(String, String)]) -> io::Result<()> {
    let stdout = io::stdout();
    let raw = stdout.into_raw_mode()?;
    let mut out = raw.lock();
    let mut input = Input::new();

    let (mut term_w, mut term_h) = termion::terminal_size().unwrap_or((80, 24));
    write!(out, "\u{1b}[s\u{1b}[?25l")?;

    let mut docs: Vec<FileDoc> = files
        .iter()
        .map(|(name, content)| FileDoc {
            name: name.clone(),
            content: content.clone(),
            lines: Vec::new(),
            plain: Vec::new(),
            top: 0,
        })
        .collect();
    for d in docs.iter_mut() {
        fit(d, term_w as usize);
    }

    let mut file = 0usize;
    let mut needle = String::new();
    let mut search: Option<String> = None;
    let mut msg: Option<String> = None;
    let mut pending_g = false;
    let mut dirty = true;
    let mut quit = false;

    while !quit {
        let vh = (term_h as usize).saturating_sub(1).max(1);
        if dirty {
            draw(&mut out, &docs[file], file, docs.len(), vh, term_w as usize, &needle, &search, msg.as_deref())?;
            dirty = false;
        }
        match input.poll() {
            None => {
                if let Ok((w, h)) = termion::terminal_size() {
                    if w != term_w || h != term_h {
                        term_w = w;
                        term_h = h;
                        for d in docs.iter_mut() {
                            fit(d, w as usize);
                        }
                        dirty = true;
                    }
                }
                sleep(Duration::from_millis(15));
                continue;
            }
            Some(Event::Key(k)) => {
                if let Some(buf) = search.as_mut() {
                    match k {
                        Key::Char(c) if !c.is_control() => {
                            buf.push(c);
                            dirty = true;
                        }
                        Key::Backspace | Key::Ctrl('u') => {
                            buf.pop();
                            dirty = true;
                        }
                        Key::Esc => {
                            search = None;
                            dirty = true;
                        }
                        Key::Char('\r') => {
                            let pat = buf.trim().to_lowercase();
                            search = None;
                            if !pat.is_empty() {
                                needle = pat.clone();
                                let d = &mut docs[file];
                                let found = find_match(d, &needle, d.top + 1, vh, true);
                                if !found {
                                    msg = Some(format!("pattern not found: {pat}"));
                                }
                            }
                            dirty = true;
                        }
                        _ => {}
                    }
                    continue;
                }
                msg = None;
                let d = &mut docs[file];
                let max_top = d.lines.len().saturating_sub(vh);
                let scroll = |d: &mut FileDoc, delta: isize| {
                    let next = (d.top as isize + delta).clamp(0, max_top as isize) as usize;
                    d.top = next;
                };
                if !matches!(k, Key::Char('g')) {
                    pending_g = false;
                }
                match k {
                    Key::Char('q') | Key::Char('Q') | Key::Esc | Key::Ctrl('c') | Key::Ctrl('z') => {
                        if file + 1 < docs.len() {
                            file += 1;
                            needle.clear();
                            docs[file].top = 0;
                        } else {
                            quit = true;
                        }
                    }
                    Key::Char('j') | Key::Char('l') | Key::Down | Key::Right | Key::Char(' ') => {
                        scroll(d, if matches!(k, Key::Char(' ')) { vh as isize } else { 1 });
                        dirty = true;
                    }
                    Key::Char('k') | Key::Char('h') | Key::Up | Key::Left => {
                        scroll(d, -1);
                        dirty = true;
                    }
                    Key::Ctrl('d') => {
                        scroll(d, (vh / 2).max(1) as isize);
                        dirty = true;
                    }
                    Key::Ctrl('u') => {
                        scroll(d, -((vh / 2).max(1) as isize));
                        dirty = true;
                    }
                    Key::Ctrl('f') => {
                        scroll(d, vh as isize);
                        dirty = true;
                    }
                    Key::Ctrl('b') => {
                        scroll(d, -(vh as isize));
                        dirty = true;
                    }
                    Key::Char('g') => {
                        if pending_g {
                            d.top = 0;
                            pending_g = false;
                            dirty = true;
                        } else {
                            pending_g = true;
                        }
                    }
                    Key::Char('G') | Key::End => {
                        d.top = max_top;
                        dirty = true;
                    }
                    Key::Home => {
                        d.top = 0;
                        dirty = true;
                    }
                    Key::Char('/') | Key::Char('?') => {
                        search = Some(String::new());
                        dirty = true;
                    }
                    Key::Char('n') if !needle.is_empty() => {
                        if !find_match(d, &needle, d.top + 1, vh, true) {
                            msg = Some(format!("no more matches: {needle}"));
                        }
                        dirty = true;
                    }
                    Key::Char('N') if !needle.is_empty() => {
                        if !find_match(d, &needle, d.top, vh, false) {
                            msg = Some(format!("no more matches: {needle}"));
                        }
                        dirty = true;
                    }
                    Key::Char(']') => {
                        file = (file + 1) % docs.len();
                        docs[file].top = 0;
                        dirty = true;
                    }
                    Key::Char('[') => {
                        file = (file + docs.len() - 1) % docs.len();
                        docs[file].top = 0;
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Some(_) => {}
        }
    }

    write!(out, "{RESET}\u{1b}[?25h\u{1b}[u")?;
    out.flush()?;
    Ok(())
}

/// Wrap-around search. `forward` scans down from `from`; otherwise up from it.
fn find_match(d: &mut FileDoc, needle: &str, from: usize, vh: usize, forward: bool) -> bool {
    let n = d.plain.len();
    if n == 0 || needle.is_empty() {
        return false;
    }
    for k in 0..n {
        let idx = if forward {
            (from + k) % n
        } else {
            (from + n - k.max(1)) % n
        };
        if d.plain[idx].contains(needle) {
            d.top = idx.saturating_sub(vh / 3).min(n.saturating_sub(1));
            return true;
        }
    }
    false
}

fn draw(
    out: &mut impl Write,
    doc: &FileDoc,
    index: usize,
    nfiles: usize,
    vh: usize,
    width: usize,
    needle: &str,
    search: &Option<String>,
    msg: Option<&str>,
) -> io::Result<()> {
    let max_top = doc.lines.len().saturating_sub(vh);
    let top = doc.top.min(max_top);
    let mut buf = String::with_capacity(vh * (width + 8));
    buf.push_str("\u{1b}[u"); // home position saved at startup
    for row in 0..vh {
        let li = top + row;
        if let Some(line) = doc.lines.get(li) {
            if needle.is_empty() {
                buf.push_str(line);
            } else {
                highlight_line(line, &doc.plain[li], needle, &mut buf);
            }
        }
        buf.push_str("\u{1b}[0m\u{1b}[K");
        if row + 1 < vh {
            buf.push_str("\r\n");
        }
    }
    buf.push_str("\r\n");
    buf.push_str(&status_line(doc, index, nfiles, top, vh, width, needle, search, msg));
    buf.push_str("\u{1b}[0m\u{1b}[J");
    out.write_all(buf.as_bytes())?;
    out.flush()
}

fn status_line(
    doc: &FileDoc,
    index: usize,
    nfiles: usize,
    top: usize,
    vh: usize,
    width: usize,
    needle: &str,
    search: &Option<String>,
    msg: Option<&str>,
) -> String {
    let _ = needle;
    let base = sg("38;5;252;48;5;236");
    let left = if let Some(msg) = msg {
        format!("{}⚠ {msg}{RESET}", sg("1;38;5;221;48;5;236"))
    } else if let Some(pat) = search {
        format!(" /{pat}\u{2588}")
    } else {
        let total = doc.lines.len();
        let pct = if total == 0 || top + vh >= total { 100 } else { (top + vh) * 100 / total };
        let multi = if nfiles > 1 { format!(" [{}/{nfiles}]", index + 1) } else { String::new() };
        format!(
            " {}{multi}  {pct:>3}%  {}/{} lines",
            doc.name,
            top.min(total.saturating_sub(1)) + 1,
            total
        )
    };
    let help = "q quit · / search · n next · gg/G jump · [ ] files";
    let left = truncate_to(&left, width);
    let gap = width.saturating_sub(visible_width(&left) + visible_width(help));
    let line = if gap >= 2 {
        format!("{left}{}{help}", " ".repeat(gap))
    } else {
        left
    };
    format!("{}{}{RESET}", base, pad_to(&line, width))
}

/// Overlay reverse-video on every occurrence of `needle` (matched against the
/// parallel plain-text line so ANSI codes never shift offsets).
fn highlight_line(styled: &str, plain: &str, needle: &str, out: &mut String) {
    let chars: Vec<char> = plain.chars().collect();
    let mut starts = vec![false; chars.len()];
    let mut ends = vec![false; chars.len()];
    let mut from = 0usize;
    while let Some(pos) = plain[from..].find(needle) {
        let byte_start = from + pos;
        let cstart = plain[..byte_start].chars().count();
        let clen = needle.chars().count();
        for k in 0..clen {
            if let Some(i) = cstart.checked_add(k) {
                if i < starts.len() {
                    starts[i] |= k == 0;
                    ends[i] |= k + 1 == clen;
                }
            }
        }
        from = byte_start + needle.len();
    }
    out.push_str(&map_visible(styled, |c, o, p| {
        if p < starts.len() && starts[p] {
            o.push_str("\u{1b}[7m");
        }
        o.push(c);
        if p < ends.len() && ends[p] {
            o.push_str("\u{1b}[27m");
        }
    }));
}
