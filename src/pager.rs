// Built-in pager: less-style key bindings, search with match highlighting,
// multi-file navigation, and resize-aware redraw on the alternate screen.
// `Pager` holds all state and turns keys into frames; `run` only shuttles
// bytes between it and the terminal, so the logic is testable without a TTY.

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::io::{self, Read, Write};
use std::thread::{self, sleep};
use std::time::Duration;

use termion::event::{parse_event, Event, Key};
use termion::raw::IntoRawMode;
use termion::{async_stdin, AsyncReader};

use crate::ansi::{map_visible, pad_to, sg, strip_ansi, truncate_to, visible_width, wrap, RESET};
use crate::boxart::{self, Outcome};
use mermaid_rs_renderer::layout::Layout;
use crate::render::{parse, render, Cfg, Line};

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

/// Every mermaid fence in draw order, with the width render() will draw it
/// at: blockquotes recurse two columns narrower (mirrors render.rs).
fn mermaid_fences(lines: &[Line], cols: usize, out: &mut Vec<(String, usize)>) {
    for line in lines {
        match line {
            Line::Code { lang, lines } => {
                if crate::vector::is_mermaid(lang) {
                    out.push((lines.join("\n"), cols));
                }
            }
            Line::Quote { inner } => mermaid_fences(inner, cols.saturating_sub(2).max(20), out),
            _ => {}
        }
    }
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
    /// mermaid fences the cache cannot answer yet: (source, the width they
    /// were probed at), current file first, nearest the viewport first.
    /// Dispatch hands one variant per idle tick to `job_tx`; opening a doc
    /// or pressing D never blocks the screen on the layout engine.
    warm_q: VecDeque<(String, usize)>,
    /// worker-thread plumbing: (source, variant) jobs out, decided layouts
    /// back; None when the thread could not be spawned (then art simply
    /// never warms and every fence honestly stays source)
    job_tx: Option<Sender<(String, usize)>>,
    result_rx: Receiver<(String, usize, Option<Layout>)>,
    /// jobs handed to the worker but not returned, so the same layout is
    /// never computed twice while it is in flight
    inflight: HashSet<(String, usize)>,
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
            warm_q: VecDeque::new(),
            job_tx: None,
            result_rx: mpsc::channel().1,
            inflight: HashSet::new(),
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
    /// resize, on the D toggle and after each warm tick; `plain` must be
    /// rebuilt too, or search would keep matching rows that are no longer
    /// on screen. Art comes from cache only (art_probe_only): anything the
    /// engine has not decided yet stays source and lands in `warm_q`.
    fn rerender(&mut self) {
        // a match index into rows that are about to vanish must not survive
        self.last_match = None;
        let cfg = Cfg {
            diagrams: self.diagrams,
            art_probe_only: true,
            ..Cfg::new(self.color, self.width, self.urls)
        };
        for d in self.docs.iter_mut() {
            // keep the reading place across the toggle: anchor on the first
            // findable row of the old view and re-find the nearest copy of
            // it in the new rows (art and source shift everything below)
            // prefer a row whose text is unique in the old view: fence
            // chrome repeats per diagram, and the nearest copy of a repeated
            // anchor could be a neighbouring fence
            let candidate = |i: usize| d.plain[i].trim().len() >= 2;
            let unique = |i: usize| d.plain.iter().filter(|l| **l == d.plain[i]).count() == 1;
            let range = d.top..(d.top + 3).min(d.plain.len());
            let anchor = range
                .clone()
                .find(|&i| candidate(i) && unique(i))
                .or_else(|| range.clone().find(|&i| candidate(i)))
                .map(|i| (d.plain[i].clone(), i, d.top));
            // render() wraps to `width`; wrap() again only guards rows that cannot shrink
            d.lines = render(&d.content, &cfg).iter().flat_map(|l| wrap(l, self.width)).collect();
            d.plain = d.lines.iter().map(|l| fold(&strip_ansi(l))).collect();
            if let Some((text, old_i, old_top)) = anchor {
                let mut best: Option<(usize, usize)> = None; // (distance, index)
                for (j, line) in d.plain.iter().enumerate() {
                    if *line == text {
                        let dist = j.abs_diff(old_i);
                        if dist < best.map_or(usize::MAX, |(bd, _)| bd) {
                            best = Some((dist, j));
                        }
                    }
                }
                if let Some((_, j)) = best {
                    d.top = j.saturating_sub(old_top - old_i);
                }
            }
        }
        self.rebuild_warm_q();
        // art is shorter than source: a top deep in the source must not
        // leave the screen blank until the next keypress
        let vh = self.vh();
        for d in self.docs.iter_mut() {
            d.top = d.top.min(d.lines.len().saturating_sub(vh));
        }
    }

    /// Re-arm every fence a probe could not decide at the width it will be
    /// drawn at. (src, cols) rather than src alone: a fence inside a quote
    /// renders two columns narrower and its decision is width-specific;
    /// warming what this probe asks makes the queue converge.
    fn rebuild_warm_q(&mut self) {
        boxart::new_generation(); // eviction may now drop anything unreferenced
        self.warm_q.clear();
        if !self.diagrams {
            return;
        }
        let plain = !self.color;
        let cfg = Cfg { diagrams: true, ..Cfg::new(self.color, self.width, self.urls) };
        let mut seen: HashSet<(String, usize)> = HashSet::new();
        let mut armed: Vec<(i64, i64, usize, (String, usize))> = Vec::new(); // (file rank, distance, order, key)
        let n = self.docs.len().max(1);
        // the file under read comes first: a multi-second diagram in a file
        // nobody is looking at must not delay the one under the eyes
        for k in 0..n {
            let d = &self.docs[(self.file + k) % n];
            let mut fences = Vec::new();
            mermaid_fences(&parse(&d.content, &cfg), self.width, &mut fences);
            let src_lines = d.content.lines().count().max(1);
            let frac_top = if d.lines.is_empty() { 0.0 } else { d.top as f64 / d.lines.len() as f64 };
            for (i, key) in fences.into_iter().enumerate() {
                boxart::keep(&key.0); // drawn or waiting: still referenced
                if !seen.insert(key.clone()) {
                    continue; // same fence at the same width: decided once
                }
                if !matches!(boxart::probe(&key.0, key.1, plain), Outcome::Unknown) {
                    continue;
                }
                // rough viewport priority: source line order and rendered
                // row order grow together, so source fraction orders like a row
                let pos = d.content.find(&key.0).unwrap_or(0);
                let line = d.content[..pos].bytes().filter(|b| *b == b'\n').count();
                let dist = ((line as f64 / src_lines as f64 - frac_top).abs() * 1000.0) as i64;
                armed.push((k as i64, dist, i, key));
            }
        }
        armed.sort_by_key(|(rank, dist, order, _)| (*rank, *dist, *order));
        self.warm_q.extend(armed.into_iter().map(|(_, _, _, key)| key));
    }

    /// One worker for the whole session: the layout engine runs only on
    /// that thread (a capped diagram measures 1.7-4.7 s and cannot be
    /// aborted), so input is never behind a computation and `q` is always
    /// instant. Quitting does not join: process exit reaps the thread.
    pub(crate) fn spawn_warm_worker(&mut self) {
        let (job_tx, job_rx) = mpsc::channel::<(String, usize)>();
        let (done_tx, done_rx) = mpsc::channel::<(String, usize, Option<Layout>)>();
        // worker threads default to 2 MB stacks against the main thread's
        // 8; the recursive-descent parser needs the headroom - a stack
        // overflow would abort the whole process, not just lose a diagram
        let spawned = thread::Builder::new()
            .name("mdglass-warm".into())
            .stack_size(64 << 20)
            .spawn(move || {
                for (text, variant) in job_rx.iter() {
                    let built = boxart::engine(&text, variant); // catches its own panics
                    if done_tx.send((text, variant, built)).is_err() {
                        break; // pager gone; process exit is imminent
                    }
                }
            })
            .is_ok();
        if spawned {
            self.job_tx = Some(job_tx);
            self.result_rx = done_rx;
        }
    }

    /// One idle tick: finished layouts move into the cache and onto the
    /// screen, then the next needed variant goes to the worker. Returns
    /// true when content changed and the caller should repaint.
    pub(crate) fn warm_tick(&mut self) -> bool {
        let mut got = false;
        while let Ok((src, variant, layout)) = self.result_rx.try_recv() {
            self.inflight.remove(&(src.clone(), variant));
            boxart::insert_layout(&src, variant, layout);
            got = true;
        }
        if got && self.diagrams {
            // re-probes answer further down the ladder now: some fences are
            // decided, others still await a follow-up variant
            self.rerender();
        }
        self.dispatch();
        got
    }

    /// Hand the worker one job: `next_warm` walks the very ladder
    /// `terminal_ascii` walks, so a dispatch always answers what the probe
    /// asked, and every arrival marks one slot tried, shrinking the unknown
    /// set (the queue converges). One job in flight at a time means
    /// priority is re-decided after every arrival instead of frozen at send
    /// time, and the queue walk - with its rasters - costs nothing while
    /// the worker is busy. Harvest + dispatch share one tick, so the
    /// worker idles at most 15 ms between jobs.
    fn dispatch(&mut self) {
        if !self.inflight.is_empty() {
            return;
        }
        let Some(tx) = self.job_tx.clone() else { return };
        let plain = !self.color;
        while let Some((src, cols)) = self.warm_q.pop_front() {
            if let Some(variant) = boxart::next_warm(&src, cols, plain) {
                if tx.send((src.clone(), variant)).is_err() {
                    self.job_tx = None; // worker lost: stay honestly on source
                    self.warm_q.push_front((src, cols));
                    return;
                }
                self.inflight.insert((src, variant));
                return;
            }
            // decided since queueing (arrival for a sibling variant): drop;
            // a still-Unknown fence re-arms at the next rebuild
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
    pager.spawn_warm_worker(); // the only thread the layout engine runs on
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
                // no key pending: receive finished layouts and queue the
                // next job; the worker computes, this loop stays responsive
                // (this also runs on ticks that resized)
                if pager.warm_tick() {
                    dirty = true;
                }
                sleep(Duration::from_millis(15));
            }
        }
    }
}

#[cfg(test)]
mod tests;
