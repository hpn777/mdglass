// Built-in pager: less-style key bindings, search with match highlighting,
// multi-file navigation, and resize-aware redraw on the alternate screen.
// `Pager` holds all state and turns keys into frames; `run` only shuttles
// bytes between it and the terminal, so the logic is testable without a TTY.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::thread::sleep;
use std::time::Duration;

use termion::event::{parse_event, Event, Key};
use termion::raw::IntoRawMode;
use termion::{async_stdin, AsyncReader};

use crate::ansi::{map_visible, pad_to, sg, strip_ansi, truncate_to, visible_width, wrap, RESET};
use crate::render::{render, Cfg};

/// Decode raw terminal input. termion reports a bare ESC as a parse error, so
/// an ESC with nothing (or another ESC) after it is mapped to `Key::Esc` here.
pub fn parse_bytes(bytes: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut iter = bytes.iter().copied().map(Ok::<u8, io::Error>).peekable();
    while let Some(Ok(b)) = iter.next() {
        if b == 0x1b && matches!(iter.peek(), None | Some(Ok(0x1b))) {
            events.push(Event::Key(Key::Esc));
            continue;
        }
        if let Ok(e) = parse_event(b, &mut iter) {
            events.push(e);
        }
    }
    events
}

/// Non-blocking key pump over the controlling TTY (works even when stdin is piped).
struct Input {
    reader: AsyncReader,
    queue: VecDeque<Event>,
}

impl Input {
    fn poll(&mut self) -> Option<Event> {
        while self.queue.is_empty() {
            let mut buf = [0u8; 256];
            match self.reader.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => self.queue.extend(parse_bytes(&buf[..n])),
            }
        }
        self.queue.pop_front()
    }
}

struct FileDoc {
    name: String,
    content: String,
    lines: Vec<String>, // styled rows, already fitted to the terminal width
    plain: Vec<String>, // case-folded, escape-free text of each row, for search
    top: usize,
}

/// Lowercase one char to one char, so folded text keeps the same char indices.
fn fold(s: &str) -> String {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

pub struct Pager {
    docs: Vec<FileDoc>,
    file: usize,
    color: bool,
    urls: bool,
    diagrams: bool,
    width: usize,
    height: usize,
    needle: String,
    search: Option<(String, bool)>, // prompt text, searching backward
    last_match: Option<usize>,
    msg: Option<String>,
    pending_g: bool,
}

impl Pager {
    pub fn new(
        files: &[(String, String)],
        color: bool,
        urls: bool,
        diagrams: bool,
        width: usize,
        height: usize,
    ) -> Pager {
        let docs = files
            .iter()
            .map(|(name, content)| FileDoc {
                name: name.clone(),
                content: content.clone(),
                lines: Vec::new(),
                plain: Vec::new(),
                top: 0,
            })
            .collect();
        let mut p = Pager {
            docs,
            file: 0,
            color,
            urls,
            diagrams,
            width: 0,
            height: 0,
            needle: String::new(),
            search: None,
            last_match: None,
            msg: None,
            pending_g: false,
        };
        p.resize(width, height);
        p
    }

    /// Re-render every document at the current width and flags. Called on
    /// resize and on the D toggle; `plain` must be rebuilt too, or search
    /// would keep matching rows that are no longer on screen.
    fn rerender(&mut self) {
        let cfg = Cfg { diagrams: self.diagrams, ..Cfg::new(self.color, self.width, self.urls) };
        for d in self.docs.iter_mut() {
            // render() wraps to `width`; wrap() again only guards rows that cannot shrink
            d.lines = render(&d.content, &cfg).iter().flat_map(|l| wrap(l, self.width)).collect();
            d.plain = d.lines.iter().map(|l| fold(&strip_ansi(l))).collect();
        }
        // art is shorter than source: a top deep in the source must not
        // leave the screen blank until the next keypress
        let vh = self.vh();
        for d in self.docs.iter_mut() {
            d.top = d.top.min(d.lines.len().saturating_sub(vh));
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        self.height = height.max(2);
        if width != self.width {
            self.width = width;
            self.rerender();
        }
        let vh = self.vh();
        for d in self.docs.iter_mut() {
            d.top = d.top.min(d.lines.len().saturating_sub(vh));
        }
    }

    fn vh(&self) -> usize {
        self.height - 1
    }

    fn max_top(&self) -> usize {
        self.docs[self.file].lines.len().saturating_sub(self.vh())
    }

    fn scroll(&mut self, delta: isize) {
        let max = self.max_top() as isize;
        let d = &mut self.docs[self.file];
        d.top = (d.top as isize + delta).clamp(0, max) as usize;
    }

    fn switch(&mut self, file: usize) {
        self.file = file;
        self.docs[file].top = 0;
        self.last_match = None;
    }

    /// Jump to the next (or previous) match. Continues from the last match
    /// while it is on screen, otherwise from the top of the screen.
    fn jump(&mut self, back: bool) {
        let vh = self.vh();
        let d = &self.docs[self.file];
        let n = d.plain.len();
        if n == 0 || self.needle.is_empty() {
            return;
        }
        let from = match self.last_match {
            Some(m) if m >= d.top && m < d.top + vh => {
                if back {
                    m + n - 1
                } else {
                    m + 1
                }
            }
            _ => {
                if back {
                    d.top + n - 1
                } else {
                    d.top
                }
            }
        } % n;
        let hit = (0..n)
            .map(|k| if back { (from + n - k) % n } else { (from + k) % n })
            .find(|&idx| d.plain[idx].contains(&self.needle));
        match hit {
            None => self.msg = Some(format!("pattern not found: {}", self.needle)),
            Some(idx) => {
                if back && idx > from {
                    self.msg = Some("search hit top, continuing at bottom".into());
                } else if !back && idx < from {
                    self.msg = Some("search hit bottom, continuing at top".into());
                }
                self.last_match = Some(idx);
                let top = idx.saturating_sub(vh / 3).min(self.max_top());
                self.docs[self.file].top = top;
            }
        }
    }

    /// Handle one key. Returns false when the pager should exit.
    pub fn key(&mut self, k: Key) -> bool {
        if let Some((buf, back)) = self.search.as_mut() {
            match k {
                Key::Char('\n') => {
                    let pat = fold(buf.trim());
                    let back = *back;
                    self.search = None;
                    if !pat.is_empty() {
                        self.needle = pat;
                        self.last_match = None;
                        self.jump(back);
                    }
                }
                Key::Char(c) if !c.is_control() => buf.push(c),
                Key::Backspace | Key::Ctrl('h') => {
                    if buf.pop().is_none() {
                        self.search = None;
                    }
                }
                Key::Ctrl('u') => buf.clear(),
                Key::Esc | Key::Ctrl('c') => self.search = None,
                _ => {}
            }
            return true;
        }

        self.msg = None;
        let g = std::mem::take(&mut self.pending_g);
        let vh = self.vh() as isize;
        let half = (vh / 2).max(1);
        match k {
            Key::Char('q') | Key::Char('Q') | Key::Esc | Key::Ctrl('c') => {
                if self.file + 1 < self.docs.len() {
                    self.switch(self.file + 1);
                } else {
                    return false;
                }
            }
            Key::Char('j') | Key::Char('\n') | Key::Down | Key::Ctrl('e') | Key::Ctrl('n') => self.scroll(1),
            Key::Char('k') | Key::Up | Key::Ctrl('y') | Key::Ctrl('p') => self.scroll(-1),
            Key::Char('d') | Key::Ctrl('d') => self.scroll(half),
            Key::Char('u') | Key::Ctrl('u') => self.scroll(-half),
            Key::Char('f') | Key::Char(' ') | Key::Ctrl('f') | Key::PageDown => self.scroll(vh),
            Key::Char('b') | Key::Ctrl('b') | Key::PageUp => self.scroll(-vh),
            Key::Char('g') if g => self.docs[self.file].top = 0,
            Key::Char('g') => self.pending_g = true,
            Key::Home | Key::Char('<') => self.docs[self.file].top = 0,
            Key::Char('G') | Key::End | Key::Char('>') => self.docs[self.file].top = self.max_top(),
            Key::Char('/') => self.search = Some((String::new(), false)),
            Key::Char('?') => self.search = Some((String::new(), true)),
            Key::Char('n') => self.jump(false),
            Key::Char('N') => self.jump(true),
            Key::Char(']') => self.switch((self.file + 1) % self.docs.len()),
            Key::Char('[') => self.switch((self.file + self.docs.len() - 1) % self.docs.len()),
            Key::Char('D') => {
                self.diagrams = !self.diagrams;
                self.rerender();
                self.msg = Some(if self.diagrams {
                    "mermaid: box art".into()
                } else {
                    "mermaid: source".into()
                });
            }
            _ => {}
        }
        true
    }

    /// The full screen: content rows plus the status line, drawn from home.
    pub fn frame(&self) -> String {
        let vh = self.vh();
        let d = &self.docs[self.file];
        let mut buf = String::with_capacity(vh * (self.width + 16));
        buf.push_str("\u{1b}[H");
        for row in 0..vh {
            // clear first: EL after a full-width row would erase its last column
            buf.push_str("\u{1b}[K");
            let li = d.top + row;
            if let Some(line) = d.lines.get(li) {
                if self.needle.is_empty() {
                    buf.push_str(line);
                } else {
                    highlight_line(line, &d.plain[li], &self.needle, &mut buf);
                }
            }
            buf.push_str(RESET);
            buf.push_str("\r\n");
        }
        buf.push_str("\u{1b}[K");
        buf.push_str(&self.status_line());
        buf
    }

    fn status_line(&self) -> String {
        let d = &self.docs[self.file];
        let left = if let Some((pat, back)) = &self.search {
            format!(" {}{pat}\u{2588}", if *back { '?' } else { '/' })
        } else if let Some(msg) = &self.msg {
            format!(" {}⚠ {msg}{RESET}{}", sg("1;38;5;221;48;5;236"), sg("38;5;252;48;5;236"))
        } else {
            let total = d.lines.len();
            let vh = self.vh();
            let pct = if total == 0 || d.top + vh >= total { 100 } else { (d.top + vh) * 100 / total };
            let multi = if self.docs.len() > 1 {
                format!(" [{}/{}]", self.file + 1, self.docs.len())
            } else {
                String::new()
            };
            format!(" {}{multi}  {pct:>3}%  {}/{} lines", d.name, (d.top + 1).min(total.max(1)), total)
        };
        let help = if self.docs.len() > 1 {
            "q quit · / ? search · n N match · [ ] file · D art "
        } else {
            "q quit · / ? search · n N match · D art "
        };
        // one column short of the edge so the terminal never scrolls
        let width = self.width.saturating_sub(1);
        let left = truncate_to(&left, width);
        let gap = width.saturating_sub(visible_width(&left) + visible_width(help));
        let line = if gap >= 2 { format!("{left}{}{help}", " ".repeat(gap)) } else { left };
        format!("{}{}{RESET}", sg("38;5;252;48;5;236"), pad_to(&line, width))
    }
}

/// Reverse-video every occurrence of `needle`, matched against the parallel
/// folded plain-text row. Each matched char gets its own SGR pair so styles
/// reset inside the match cannot cancel the highlight.
fn highlight_line(styled: &str, plain: &str, needle: &str, out: &mut String) {
    let p: Vec<char> = plain.chars().collect();
    let nd: Vec<char> = needle.chars().collect();
    let mut mark = vec![false; p.len()];
    let mut i = 0;
    while !nd.is_empty() && i + nd.len() <= p.len() {
        if p[i..i + nd.len()] == nd[..] {
            mark[i..i + nd.len()].fill(true);
            i += nd.len();
        } else {
            i += 1;
        }
    }
    out.push_str(&map_visible(styled, |c, o, idx| {
        if mark.get(idx) == Some(&true) {
            o.push_str("\u{1b}[7m");
            o.push(c);
            o.push_str("\u{1b}[27m");
        } else {
            o.push(c);
        }
    }));
}

/// Leaves the alternate screen and shows the cursor on every exit path; it is
/// declared after the raw-mode guard so it runs before raw mode is undone.
struct AltScreen;

impl Drop for AltScreen {
    fn drop(&mut self) {
        let mut o = io::stdout();
        let _ = o.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l");
        let _ = o.flush();
    }
}

pub fn run(files: &[(String, String)], color: bool, urls: bool, diagrams: bool) -> io::Result<()> {
    let (w, h) = termion::terminal_size().unwrap_or((80, 24));
    let mut pager = Pager::new(files, color, urls, diagrams, w as usize, h as usize);
    let mut out = io::stdout().into_raw_mode()?;
    out.write_all(b"\x1b[?1049h\x1b[?25l")?;
    let _screen = AltScreen;
    let mut input = Input { reader: async_stdin(), queue: VecDeque::new() };
    let mut size = (w, h);
    let mut dirty = true;
    loop {
        if dirty {
            out.write_all(pager.frame().as_bytes())?;
            out.flush()?;
            dirty = false;
        }
        match input.poll() {
            Some(Event::Key(k)) => {
                if !pager.key(k) {
                    return Ok(());
                }
                dirty = true;
            }
            Some(_) => {}
            None => {
                if let Ok(now) = termion::terminal_size() {
                    if now != size {
                        size = now;
                        pager.resize(now.0 as usize, now.1 as usize);
                        dirty = true;
                    }
                }
                sleep(Duration::from_millis(15));
            }
        }
    }
}

#[cfg(test)]
mod tests;
