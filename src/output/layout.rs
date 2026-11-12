//! Point-based, renderer-independent print layout. No terminal-width truncation.
use std::collections::HashMap;
use std::sync::Arc;

use super::native::Native;
use super::{FitMode, Settings};
use crate::doc::{self, Cfg, Line, Role, Span, Style, TableRow};

const MAX_PAGES: usize = 4096;
const MAX_TILES: usize = 1024;
const PAD: f64 = 5.0;

#[derive(Clone, Debug)]
pub struct PagePlan {
    pub pages: Vec<Page>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub items: Vec<Item>,
}
#[derive(Clone, Debug)]
pub enum Item {
    Text {
        x: f64,
        y: f64,
        spans: Vec<Span>,
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    },
    Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        fill: Option<(f64, f64, f64)>,
        stroke: Option<(f64, f64, f64)>,
    },
    Svg {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        svg: Arc<str>,
        clip: Option<(f64, f64, f64, f64)>,
    },
}

#[derive(Clone, Debug)]
struct Run {
    start: usize,
    end: usize,
    height: f64,
}
struct Measure {
    width: f64,
    lines: Vec<Run>,
}
trait Metrics {
    fn text(
        &self,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    ) -> Result<Measure, String>;
    fn svg_size(&self, svg: &str) -> Result<(f64, f64), String>;
}
impl Metrics for Native {
    fn text(
        &self,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    ) -> Result<Measure, String> {
        let m = Native::text(self, spans, size, bold, mono, width)?;
        if !m.height.is_finite() || m.height <= 0.0 {
            return Err("Invalid native text height".into());
        }
        Ok(Measure {
            width: m.width,
            lines: m
                .lines
                .into_iter()
                .map(|l| Run {
                    start: l.start,
                    end: l.end,
                    height: l.height,
                })
                .collect(),
        })
    }
    fn svg_size(&self, svg: &str) -> Result<(f64, f64), String> {
        Native::svg_size(self, svg)
    }
}

fn plain(text: impl Into<String>) -> Vec<Span> {
    vec![Span {
        text: text.into(),
        style: Style::empty(),
        role: Role::None,
        url: None,
    }]
}

fn expand_tabs(line: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = 4 - column % 4;
            out.extend(std::iter::repeat(' ').take(spaces));
            column += spaces;
        } else {
            out.push(ch);
            column += crate::ansi::char_width(ch);
        }
    }
    out
}

/// Paginate using native font metrics, without rasterizing text or diagrams.
pub fn paginate(src: &str, settings: &Settings, native: &Native) -> Result<PagePlan, String> {
    paginate_with(src, settings, native)
}
fn paginate_with(
    src: &str,
    settings: &Settings,
    metrics: &dyn Metrics,
) -> Result<PagePlan, String> {
    settings.validate()?;
    let (w, h) = settings.page_size();
    let mut p = Layout {
        settings,
        metrics,
        plan: PagePlan {
            pages: Vec::new(),
            warnings: Vec::new(),
        },
        w,
        h,
        y: settings.margin,
        diagrams: HashMap::new(),
        pending_heading: None,
    };
    p.new_page()?;
    p.blocks(
        &doc::parse(src, &Cfg::new(false, 1_000_000, settings.urls)),
        0.0,
    )?;
    Ok(p.plan)
}

type Diagram = Result<(Arc<str>, f64, f64), String>;
struct Layout<'a> {
    settings: &'a Settings,
    metrics: &'a dyn Metrics,
    plan: PagePlan,
    w: f64,
    h: f64,
    y: f64,
    diagrams: HashMap<(String, String), Diagram>,
    // Last heading's item index and top on the current page, including any
    // following blank space in self.y. Only single-page headings are movable.
    pending_heading: Option<(usize, f64)>,
}
impl Layout<'_> {
    fn width(&self) -> f64 {
        self.w - 2.0 * self.settings.margin
    }
    fn height(&self) -> f64 {
        self.h - 2.0 * self.settings.margin
    }
    fn bottom(&self) -> f64 {
        self.h - self.settings.margin
    }
    fn new_page(&mut self) -> Result<(), String> {
        self.settings.check_cancelled()?;
        if self.plan.pages.len() >= MAX_PAGES {
            return Err(format!("Output exceeds {MAX_PAGES} pages"));
        }
        self.plan.pages.push(Page {
            width: self.w,
            height: self.h,
            items: Vec::new(),
        });
        self.y = self.settings.margin;
        Ok(())
    }
    fn item(&mut self, item: Item) {
        self.plan.pages.last_mut().unwrap().items.push(item);
    }
    fn fresh(&mut self) -> Result<(), String> {
        if !self.plan.pages.last().unwrap().items.is_empty() {
            self.new_page()?;
        }
        Ok(())
    }
    fn room(&mut self, height: f64) -> Result<(), String> {
        self.settings.check_cancelled()?;
        if height > self.height() + 0.01 {
            return Err(
                "A text line is taller than the printable page; reduce font size or margins".into(),
            );
        }
        if self.y + height > self.bottom() + 0.01 {
            self.new_page()?;
        }
        Ok(())
    }
    fn measured(
        &self,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        width: Option<f64>,
    ) -> Result<Measure, String> {
        self.settings.check_cancelled()?;
        let m = self.metrics.text(spans, size, bold, mono, width)?;
        self.settings.check_cancelled()?;
        let text = doc::spans_plain(spans);
        if !m.width.is_finite() || m.width < 0.0 {
            return Err("Invalid native text metrics".into());
        }
        // Pango omits paragraph separators, but must not omit actual content or
        // return overlapping byte ranges. Validate before slicing any UTF-8.
        let separators = |s: &str| {
            s.chars()
                .all(|c| matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}'))
        };
        let mut previous = 0;
        for l in &m.lines {
            if !l.height.is_finite()
                || l.height <= 0.0
                || l.start < previous
                || l.start > l.end
                || l.end > text.len()
                || !text.is_char_boundary(l.start)
                || !text.is_char_boundary(l.end)
                || !separators(&text[previous..l.start])
            {
                return Err("Invalid native text metrics".into());
            }
            previous = l.end;
        }
        if !separators(&text[previous..]) {
            return Err("Native text metrics omitted content".into());
        }
        Ok(m)
    }
    fn object_room(&mut self, minimum: f64) -> Result<(), String> {
        self.settings.check_cancelled()?;
        if self.y + minimum <= self.bottom() + 0.01 {
            return Ok(());
        }
        if let Some((start, top)) = self.pending_heading {
            let occupied = self.y - top;
            if occupied + minimum > self.height() + 0.01 {
                return Err(
                    "Heading and following object cannot fit together; reduce font size or margins"
                        .into(),
                );
            }
            let mut items = self.plan.pages.last_mut().unwrap().items.split_off(start);
            if self.plan.pages.last().unwrap().items.is_empty() {
                // Leading blank space alone must not leave an empty page.
                self.y = self.settings.margin;
            } else {
                self.new_page()?;
            }
            let delta = self.settings.margin - top;
            for item in &mut items {
                match item {
                    Item::Text { y, .. } | Item::Rect { y, .. } => *y += delta,
                    Item::Svg { y, clip, .. } => {
                        *y += delta;
                        if let Some((_, cy, _, _)) = clip {
                            *cy += delta;
                        }
                    }
                }
            }
            self.plan.pages.last_mut().unwrap().items.extend(items);
            self.y += occupied;
            self.pending_heading = Some((0, self.settings.margin));
        } else {
            self.room(minimum)?;
        }
        Ok(())
    }
    fn text_item(&mut self, x: f64, y: f64, spans: Vec<Span>, size: f64, bold: bool, mono: bool) {
        self.item(Item::Text {
            x,
            y,
            spans,
            size,
            bold,
            mono,
            width: None,
        });
    }
    fn fitted_fragment(
        &self,
        mut spans: Vec<Span>,
        size: f64,
        bold: bool,
        mono: bool,
        width: f64,
    ) -> Result<(Vec<Span>, Measure), String> {
        let mut m = self.measured(&spans, size, bold, mono, None)?;
        if m.width > width + 0.01 {
            // Pango suppresses trailing break spaces when measuring a wrapped
            // line. A standalone slice gives those spaces advances again.
            // Remove only that invisible suffix, never words, code symbols,
            // leading indentation, or non-breaking spaces.
            while let Some(last) = spans.last_mut() {
                last.text.truncate(last.text.trim_end_matches([' ', '\t']).len());
                if !last.text.is_empty() { break; }
                spans.pop();
            }
            m = self.measured(&spans, size, bold, mono, None)?;
        }
        if m.width > width + 0.01 {
            return Err("A wrapped text line exceeds the printable area".into());
        }
        Ok((spans, m))
    }
    fn paragraph(
        &mut self,
        spans: &[Span],
        size: f64,
        bold: bool,
        mono: bool,
        indent: f64,
        hanging: f64,
        following: f64,
    ) -> Result<(), String> {
        let width = self.width() - indent - hanging;
        if width < size {
            return Err("Indent leaves insufficient printable width".into());
        }
        let m = self.measured(spans, size, bold, mono, Some(width))?;
        if m.width > width + 0.01 {
            return Err(
                "A text glyph is wider than the printable area; reduce font size or margins".into(),
            );
        }
        let total: f64 = m.lines.iter().map(|l| l.height).sum();
        let first = m.lines.first().map_or(size * 1.3, |l| l.height);
        let reserve = if following > 0.0 {
            total + size * 0.4 + following
        } else if m.lines.len() > 1 {
            first + m.lines[1].height
        } else {
            first
        };
        if reserve <= self.height() && self.y + reserve > self.bottom() {
            self.new_page()?;
        }
        for (i, line) in m.lines.iter().enumerate() {
            // Keep the final pair together where a page can accommodate both.
            if i + 2 == m.lines.len() {
                let pair = line.height + m.lines[i + 1].height;
                if pair <= self.height() && self.y + pair > self.bottom() {
                    self.new_page()?;
                }
            }
            self.room(line.height)?;
            let (fragment, _) = self.fitted_fragment(
                doc::spans_slice(spans, line.start, line.end), size, bold, mono, width,
            )?;
            self.text_item(
                self.settings.margin + indent + if i == 0 { 0.0 } else { hanging },
                self.y,
                fragment,
                size,
                bold,
                mono,
            );
            self.y += line.height;
        }
        self.y += size * 0.4;
        Ok(())
    }
    fn blocks(&mut self, blocks: &[Line], indent: f64) -> Result<(), String> {
        if !indent.is_finite() || indent < 0.0 || self.width() - indent < 2.0 {
            return Err("Indent leaves insufficient printable width".into());
        }
        let size = self.settings.font_size;
        for (index, block) in blocks.iter().enumerate() {
            self.settings.check_cancelled()?;
            if !matches!(block, Line::Blank | Line::Table(_) | Line::Code { .. }) {
                self.pending_heading = None;
            }
            match block {
                Line::Para {
                    spans,
                    indent: i,
                    hanging,
                } => self.paragraph(
                    spans,
                    size,
                    false,
                    false,
                    indent + *i as f64 * size * 0.55,
                    *hanging as f64 * size * 0.55,
                    0.0,
                )?,
                Line::Heading { lvl, spans } => {
                    let page = self.plan.pages.len();
                    let start = self.plan.pages.last().unwrap().items.len();
                    let following = self.following_height(&blocks[index + 1..], indent)?;
                    self.paragraph(
                        spans,
                        size * match lvl {
                            1 => 2.0,
                            2 => 1.65,
                            3 => 1.35,
                            _ => 1.15,
                        },
                        true,
                        false,
                        indent,
                        0.0,
                        following,
                    )?;
                    let same_page = page == self.plan.pages.len();
                    let split = !same_page && self.plan.pages[page - 1].items.len() > start;
                    if !split {
                        let start = if same_page { start } else { 0 };
                        if let Some(Item::Text { y, .. }) =
                            self.plan.pages.last().unwrap().items.get(start)
                        {
                            self.pending_heading = Some((start, *y));
                        }
                    } else if blocks[index + 1..]
                        .iter()
                        .find(|b| !matches!(b, Line::Blank))
                        .is_some_and(|b| matches!(b, Line::Table(_) | Line::Code { .. }))
                    {
                        return Err(
                            "A multi-page heading cannot be kept with its following object".into(),
                        );
                    }
                }
                Line::Code { lang, lines } => {
                    if !self.diagram(lang, lines, indent)? {
                        for line in lines {
                            self.paragraph(
                                &plain(expand_tabs(line)),
                                size * 0.9,
                                false,
                                true,
                                indent + PAD,
                                0.0,
                                0.0,
                            )?;
                        }
                    }
                    self.pending_heading = None;
                }
                Line::Table(t) => {
                    self.table(t, indent)?;
                    self.pending_heading = None;
                }
                Line::Quote { inner } => {
                    // A semantic quote marker survives page breaks, unlike one giant rectangle.
                    let page = self.plan.pages.len() - 1;
                    let start = self.y;
                    self.blocks(inner, indent + size * 1.3)?;
                    let last = self.plan.pages.len() - 1;
                    for n in page..=last {
                        let top = if n == page {
                            start
                        } else {
                            self.settings.margin
                        };
                        let bottom = if n == last {
                            self.y.min(self.bottom())
                        } else {
                            self.bottom()
                        };
                        if bottom > top {
                            self.plan.pages[n].items.insert(
                                0,
                                Item::Rect {
                                    x: self.settings.margin + indent,
                                    y: top,
                                    width: 2.0,
                                    height: bottom - top,
                                    fill: Some((0.65, 0.65, 0.65)),
                                    stroke: None,
                                },
                            );
                        }
                    }
                    self.pending_heading = None;
                }
                Line::Rule => {
                    self.room(size)?;
                    self.item(Item::Rect {
                        x: self.settings.margin + indent,
                        y: self.y + size * 0.4,
                        width: (self.width() - indent).max(1.0),
                        height: 0.6,
                        fill: Some((0.7, 0.7, 0.7)),
                        stroke: None,
                    });
                    self.y += size;
                }
                Line::Blank => self.y = (self.y + size * 0.5).min(self.bottom()),
            }
        }
        Ok(())
    }

    /// Include intervening blank space and the next text line in a heading's
    /// keep-with-next budget, rather than assuming the next line's font size.
    fn following_height(&self, blocks: &[Line], indent: f64) -> Result<f64, String> {
        let base = self.settings.font_size;
        let mut gap = 0.0;
        for block in blocks {
            self.settings.check_cancelled()?;
            let (spans, size, bold, mono, inset) = match block {
                Line::Blank => {
                    gap += base * 0.5;
                    continue;
                }
                Line::Para {
                    spans,
                    indent: i,
                    hanging,
                } => (
                    spans.clone(),
                    base,
                    false,
                    false,
                    (*i + *hanging) as f64 * base * 0.55,
                ),
                Line::Heading { lvl, spans } => (
                    spans.clone(),
                    base * match lvl {
                        1 => 2.0,
                        2 => 1.65,
                        3 => 1.35,
                        _ => 1.15,
                    },
                    true,
                    false,
                    0.0,
                ),
                Line::Code { lines, .. } => (
                    plain(lines.first().map_or("", String::as_str)),
                    base * 0.9,
                    false,
                    true,
                    PAD,
                ),
                Line::Quote { inner } => {
                    return Ok(gap + self.following_height(inner, indent + base * 1.3)?)
                }
                _ => return Ok(gap + base * 1.3),
            };
            let m = self.measured(
                &spans,
                size,
                bold,
                mono,
                Some((self.width() - indent - inset).max(1.0)),
            )?;
            // Paragraph pagination reserves its opening pair, so reserve the
            // same pair here or it could move away from the heading again.
            return Ok(gap
                + if m.lines.is_empty() {
                    size * 1.3
                } else {
                    m.lines.iter().take(2).map(|l| l.height).sum()
                });
        }
        Ok(0.0)
    }

    fn diagram(&mut self, lang: &str, lines: &[String], indent: f64) -> Result<bool, String> {
        if !self.settings.diagrams
            || (!crate::vector::is_mermaid(lang) && !crate::vector::art_lang(lang))
        {
            return Ok(false);
        }
        self.settings.check_cancelled()?;
        if !crate::vector::is_mermaid(lang) {
            let ascii: Vec<String> = lines.iter().map(|l| crate::vector::to_ascii(l)).collect();
            if !crate::vector::looks_like_art(&ascii) {
                return Ok(false);
            }
        }
        let source = lines.join("\n");
        let key = (lang.to_string(), source.clone());
        if !self.diagrams.contains_key(&key) {
            let result = crate::vector::engine_svg(lang, &source, false)
                .and_then(|s| crate::vector::apply_theme(lang, &s, false))
                .ok_or_else(|| "Diagram renderer did not produce SVG".to_string())
                .and_then(|s| {
                    let (w, h) = self.metrics.svg_size(&s)?;
                    let (w, h) = (w * 0.75, h * 0.75);
                    if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
                        return Err("Invalid diagram dimensions".into());
                    }
                    Ok((Arc::<str>::from(s), w, h))
                });
            if let Err(e) = &result {
                if crate::vector::is_mermaid(lang) || crate::vector::art_lang(lang) {
                    self.plan
                        .warnings
                        .push(format!("{lang}: {e}; printing source instead"));
                }
            }
            self.diagrams.insert(key.clone(), result);
        }
        let Ok((svg, w, h)) = self.diagrams[&key].clone() else {
            return Ok(false);
        };
        let aw = self.width() - indent;
        let ah = self.height();
        if aw <= 1.0 || ah <= 1.0 {
            return Err("No printable area for diagram".into());
        }
        let mut fit = (aw / w).min(ah / h).min(1.0);
        let tile = self.settings.diagram_mode == FitMode::Tile
            || (self.settings.diagram_mode == FitMode::Auto && fit < 0.65);
        if !tile {
            if let Some((_, top)) = self.pending_heading {
                // Size the diagram for the space below its heading on a fresh
                // page, then move the pair together only if necessary.
                let available = self.height() - (self.y - top);
                if available <= 0.0 {
                    return Err("Heading leaves no printable area for diagram".into());
                }
                fit = fit.min(available / h);
            }
            if w * fit <= 0.0 || h * fit <= 0.0 {
                return Err("Diagram aspect ratio exceeds printable geometry".into());
            }
            if self.pending_heading.is_some() {
                self.object_room(h * fit)?;
            } else {
                self.room(h * fit)?;
            }
            self.item(Item::Svg {
                x: self.settings.margin + indent,
                y: self.y,
                width: w * fit,
                height: h * fit,
                svg,
                clip: None,
            });
            self.y += h * fit + self.settings.font_size * 0.5;
            return Ok(true);
        }
        // Measure a worst-case tile caption before choosing the vertical grid.
        // Captions must wrap too, particularly on narrow printer pages.
        let caption_size = (self.settings.font_size * 0.8)
            .min(10.0)
            .min(aw / 16.0)
            .min(ah / 8.0);
        let caption = self.measured(
            &plain(format!(
                "Diagram tile {MAX_TILES}, {MAX_TILES} of {MAX_TILES} × {MAX_TILES}"
            )),
            caption_size,
            false,
            false,
            Some(aw),
        )?;
        let caption_height =
            caption.lines.iter().map(|l| l.height).sum::<f64>() + caption_size * 0.5;
        if self.pending_heading.is_some() {
            self.object_room(caption_height + h.min(24.0))?;
        } else {
            self.fresh()?;
        }
        // A heading shares the first tile. Keep one uniform viewport size for
        // the entire grid, so later rows never skip any part of the SVG.
        let ah = self.bottom() - self.y - caption_height;
        let xs = tile_offsets(w, aw, self.settings.overlap)?;
        let ys = tile_offsets(h, ah, self.settings.overlap)?;
        if xs
            .len()
            .checked_mul(ys.len())
            .filter(|&n| n <= MAX_TILES)
            .is_none()
        {
            return Err("Diagram exceeds tile limit".into());
        }
        for (iy, &dy) in ys.iter().enumerate() {
            for (ix, &dx) in xs.iter().enumerate() {
                self.settings.check_cancelled()?;
                if iy != 0 || ix != 0 {
                    self.fresh()?;
                }
                let top = self.y;
                self.paragraph(
                    &plain(format!(
                        "Diagram tile {}, {} of {} × {}",
                        ix + 1,
                        iy + 1,
                        xs.len(),
                        ys.len()
                    )),
                    caption_size,
                    false,
                    false,
                    indent,
                    0.0,
                    0.0,
                )?;
                let y = top + caption_height;
                self.item(Item::Svg {
                    x: self.settings.margin + indent - dx,
                    y: y - dy,
                    width: w,
                    height: h,
                    svg: svg.clone(),
                    clip: Some((self.settings.margin + indent, y, aw, ah)),
                });
                self.y = self.bottom();
            }
        }
        Ok(true)
    }

    fn table(&mut self, table: &doc::Table, indent: f64) -> Result<(), String> {
        let rows: Vec<_> = table
            .rows
            .iter()
            .filter_map(|r| match r {
                TableRow::Cells { header, cells } => Some((*header, cells)),
                _ => None,
            })
            .collect();
        let n = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
        if n == 0 {
            return Ok(());
        }
        if n > 4096 {
            return Err("Table exceeds column limit".into());
        }
        if rows
            .len()
            .checked_mul(n)
            .filter(|&cells| cells <= 1_000_000)
            .is_none()
        {
            return Err("Table exceeds cell limit".into());
        }
        let aw = self.width() - indent;
        let size = self.settings.font_size;
        if aw < size * 3.0 + PAD * 2.0 {
            return Err("No printable area for table".into());
        }
        let mut widths = vec![size * 3.0 + PAD * 2.0; n];
        for (header, cells) in &rows {
            self.settings.check_cancelled()?;
            for (i, cell) in cells.iter().enumerate() {
                widths[i] = widths[i]
                    .max(self.measured(cell, size, *header, false, None)?.width + 2.0 * PAD);
            }
        }
        // A single column must always fit; never cut it into horizontal pixel strips.
        let cap = if n > 1 && self.settings.repeat_first_column {
            aw * 0.5
        } else {
            aw
        };
        for w in &mut widths {
            *w = w.min(cap);
        }
        if self.settings.table_mode == FitMode::Fit {
            let sum: f64 = widths.iter().sum();
            if sum > aw {
                if aw / n as f64 <= 2.0 * PAD + size * 0.5 {
                    return Err("Too many columns to fit; use table tile mode".into());
                }
                for w in &mut widths {
                    *w = aw / n as f64;
                }
            }
        } else if self.settings.table_mode == FitMode::Auto {
            // Prefer wrapping a modest number of readable columns to making
            // horizontal bands merely because their natural widths are large.
            fit_readable_columns(&mut widths, aw, size * 6.0 + 2.0 * PAD);
        }
        let bands = column_bands(&widths, aw, self.settings.repeat_first_column)?;
        let mut wrapped = Vec::new();
        let mut row_slots = Vec::new();
        let mut slot = size * 1.3;
        for (header, cells) in &rows {
            self.settings.check_cancelled()?;
            let mut row = Vec::new();
            for i in 0..n {
                let spans = cells.get(i).cloned().unwrap_or_default();
                let m = self.measured(
                    &spans,
                    size,
                    *header,
                    false,
                    Some((widths[i] - 2.0 * PAD).max(1.0)),
                )?;
                if m.width > widths[i] - 2.0 * PAD + 0.01 {
                    return Err(
                        "A table glyph is wider than its column; widen columns or reduce font size"
                            .into(),
                    );
                }
                for l in &m.lines {
                    slot = slot.max(l.height);
                }
                row.push((spans, m.lines));
            }
            row_slots.push(row.iter().map(|(_, ls)| ls.len()).max().unwrap_or(1).max(1));
            wrapped.push(row);
        }
        let mut header = rows.iter().position(|(h, _)| *h);
        let mut hh = header.map_or(0.0, |i| row_slots[i] as f64 * slot + 2.0 * PAD);
        if hh > self.height() || (rows.len() > 1 && hh + slot + 2.0 * PAD > self.height()) {
            // A giant header is content, not disposable decoration. Split it
            // exactly like a body row rather than rejecting or truncating it.
            header = None;
            hh = 0.0;
            self.plan
                .warnings
                .push("Table header is too tall to repeat; printing it once in fragments".into());
        }
        if slot + 2.0 * PAD > self.height() - hh && rows.len() > usize::from(header.is_some()) {
            return Err("Table has no room for a row".into());
        }
        let has_body = rows.len() > usize::from(header.is_some());
        let first_row = row_slots.iter().enumerate().find(|(r, _)| Some(*r) != header)
            .map(|(_, &count)| hh + count as f64 * slot + 2.0 * PAD);
        if let Some(first_height) = first_row.filter(|h| *h <= self.height() + 0.01) {
            if self.pending_heading.is_some_and(|(_, top)|
                self.y - top + first_height > self.height() + 0.01) {
                // Prefer an intact ordinary row to keep-with-next when both
                // cannot be satisfied. Oversized rows still split below.
                self.pending_heading = None;
            }
            self.object_room(first_height)?;
        } else {
            self.object_room(hh + if has_body { slot + 2.0 * PAD } else { 0.0 })?;
        }
        // Compute vertical ranges once, then reuse them for every column band.
        let mut chunks: Vec<Vec<(usize, usize, usize)>> = Vec::new();
        let mut chunk = Vec::new();
        let mut remaining = self.bottom() - self.y - hh;
        for (r, &count) in row_slots.iter().enumerate() {
            self.settings.check_cancelled()?;
            if Some(r) == header {
                continue;
            }
            // Ordinary rows stay intact; only genuinely oversized rows need
            // synchronized cell-line fragments on multiple pages.
            let row_height = count as f64 * slot + 2.0 * PAD;
            if !chunk.is_empty()
                && row_height <= self.height() - hh + 0.01
                && row_height > remaining + 0.01
            {
                chunks.push(std::mem::take(&mut chunk));
                remaining = self.height() - hh;
            }
            let mut from = 0;
            while from < count {
                self.settings.check_cancelled()?;
                if remaining + 0.01 < slot + 2.0 * PAD {
                    chunks.push(std::mem::take(&mut chunk));
                    remaining = self.height() - hh;
                }
                let capacity = ((remaining - 2.0 * PAD + 0.01) / slot).floor() as usize;
                let take = (count - from).min(capacity);
                if take == 0 {
                    return Err("Table has no room for a row".into());
                }
                chunk.push((r, from, from + take));
                from += take;
                remaining -= take as f64 * slot + 2.0 * PAD;
                if chunks.len() > MAX_PAGES {
                    return Err("Table exceeds page limit".into());
                }
            }
        }
        if !chunk.is_empty() || chunks.is_empty() {
            chunks.push(chunk);
        }
        if chunks
            .len()
            .checked_mul(bands.len())
            .filter(|&v| v <= MAX_PAGES)
            .is_none()
        {
            return Err("Table exceeds page limit".into());
        }
        let mut first = true;
        for chunk in chunks {
            for band in &bands {
                self.settings.check_cancelled()?;
                if !first {
                    self.fresh()?;
                }
                first = false;
                if let Some(r) = header {
                    self.table_row(
                        &wrapped[r],
                        band,
                        &widths,
                        0,
                        row_slots[r],
                        slot,
                        size,
                        true,
                        indent,
                        &table.aligns,
                    )?;
                }
                for &(r, from, to) in &chunk {
                    self.table_row(
                        &wrapped[r],
                        band,
                        &widths,
                        from,
                        to,
                        slot,
                        size,
                        rows[r].0,
                        indent,
                        &table.aligns,
                    )?;
                }
            }
        }
        self.y += size * 0.5;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn table_row(
        &mut self,
        cells: &[(Vec<Span>, Vec<Run>)],
        band: &[usize],
        widths: &[f64],
        from: usize,
        to: usize,
        slot: f64,
        size: f64,
        header: bool,
        indent: f64,
        aligns: &[doc::Align],
    ) -> Result<(), String> {
        let height = (to - from) as f64 * slot + 2.0 * PAD;
        if self.y + height > self.bottom() + 0.01 {
            return Err("Table fragment exceeded printable area".into());
        }
        let mut x = self.settings.margin + indent;
        for &i in band {
            self.settings.check_cancelled()?;
            self.item(Item::Rect {
                x,
                y: self.y,
                width: widths[i],
                height,
                fill: Some(if header {
                    (0.93, 0.94, 0.95)
                } else {
                    (1.0, 1.0, 1.0)
                }),
                stroke: Some((0.7, 0.7, 0.7)),
            });
            let (spans, lines) = &cells[i];
            for k in from..to {
                if let Some(l) = lines.get(k) {
                    let (fragment, measured) = self.fitted_fragment(
                        doc::spans_slice(spans, l.start, l.end), size, header, false,
                        widths[i] - 2.0 * PAD,
                    )?;
                    let spare = widths[i] - 2.0 * PAD - measured.width;
                    if spare < -0.01 {
                        return Err("A wrapped table line exceeds its column".into());
                    }
                    let offset = match aligns.get(i).unwrap_or(&doc::Align::Left) {
                        doc::Align::Left => 0.0,
                        doc::Align::Center => spare.max(0.0) / 2.0,
                        doc::Align::Right => spare.max(0.0),
                    };
                    self.text_item(
                        x + PAD + offset,
                        self.y + PAD + (k - from) as f64 * slot,
                        fragment,
                        size,
                        header,
                        false,
                    );
                }
            }
            x += widths[i];
        }
        self.y += height;
        Ok(())
    }
}

fn fit_readable_columns(widths: &mut [f64], available: f64, readable: f64) {
    let preferred: f64 = widths.iter().sum();
    if preferred <= available {
        return;
    }
    let minimum: f64 = widths.iter().map(|w| w.min(readable)).sum();
    if minimum > available {
        return;
    }
    let ratio = (available - minimum) / (preferred - minimum);
    for width in widths {
        let min = width.min(readable);
        *width = min + (*width - min) * ratio;
    }
}

fn column_bands(widths: &[f64], available: f64, repeat: bool) -> Result<Vec<Vec<usize>>, String> {
    let mut out = Vec::new();
    let mut next = 0;
    while next < widths.len() {
        let mut band = Vec::new();
        let mut used = 0.0;
        if repeat && next > 0 {
            band.push(0);
            used = widths[0];
        }
        let start = next;
        while next < widths.len() && used + widths[next] <= available + 0.001 {
            used += widths[next];
            band.push(next);
            next += 1;
        }
        if next == start {
            return Err("A table column cannot fit in its column band".into());
        }
        out.push(band);
    }
    Ok(out)
}
fn tile_offsets(extent: f64, viewport: f64, overlap: f64) -> Result<Vec<f64>, String> {
    if !extent.is_finite()
        || !viewport.is_finite()
        || viewport <= 0.0
        || extent <= 0.0
        || !overlap.is_finite()
    {
        return Err("Invalid tile geometry".into());
    }
    let step = viewport - overlap.max(0.0).min(viewport * 0.25);
    let count = ((extent - viewport).max(0.0) / step).ceil() + 1.0;
    if !count.is_finite() || count > MAX_TILES as f64 {
        return Err("Diagram exceeds tile limit".into());
    }
    let count = count as usize;
    Ok((0..count)
        .map(|i| (i as f64 * step).min((extent - viewport).max(0.0)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake;
    impl Metrics for Fake {
        fn text(
            &self,
            spans: &[Span],
            size: f64,
            _: bool,
            _: bool,
            width: Option<f64>,
        ) -> Result<Measure, String> {
            let text = doc::spans_plain(spans);
            let columns =
                width.map_or(usize::MAX, |w| (w / (size * 0.5)).floor().max(1.0) as usize);
            let mut lines = Vec::new();
            let mut start = 0;
            let mut count = 0;
            for (i, c) in text.char_indices() {
                if count == columns || c == '\n' {
                    lines.push(Run {
                        start,
                        end: i,
                        height: size * 1.3,
                    });
                    start = i;
                    count = 0;
                }
                count += 1;
            }
            if start < text.len() {
                lines.push(Run {
                    start,
                    end: text.len(),
                    height: size * 1.3,
                });
            }
            Ok(Measure {
                width: (text.chars().count().min(columns)) as f64 * size * 0.5,
                lines,
            })
        }
        fn svg_size(&self, _: &str) -> Result<(f64, f64), String> {
            Err("test has no SVG library".into())
        }
    }
    fn small() -> Settings {
        Settings {
            page_size_override: Some((240.0, 200.0)),
            margin: 15.0,
            overlap: 5.0,
            ..Settings::default()
        }
    }
    fn text(plan: &PagePlan) -> String {
        plan.pages
            .iter()
            .flat_map(|p| p.items.iter())
            .filter_map(|i| match i {
                Item::Text { spans, .. } => Some(doc::spans_plain(spans)),
                _ => None,
            })
            .collect()
    }
    fn cached_diagram(settings: &Settings, w: f64, h: f64) -> Result<PagePlan, String> {
        cached_diagram_with_heading(settings, w, h, false)
    }
    fn cached_diagram_with_heading(
        settings: &Settings,
        w: f64,
        h: f64,
        heading: bool,
    ) -> Result<PagePlan, String> {
        let (pw, ph) = settings.page_size();
        let mut layout = Layout {
            settings,
            metrics: &Fake,
            plan: PagePlan {
                pages: Vec::new(),
                warnings: Vec::new(),
            },
            w: pw,
            h: ph,
            y: settings.margin,
            diagrams: HashMap::new(),
            pending_heading: None,
        };
        layout.diagrams.insert(
            ("mermaid".into(), "test".into()),
            Ok((Arc::from("<svg/>"), w, h)),
        );
        layout.new_page()?;
        if heading {
            layout.blocks(
                &doc::parse(
                    "lead\n\n# Heading\n\n",
                    &Cfg::new(false, 1_000_000, settings.urls),
                ),
                0.0,
            )?;
        }
        assert!(layout.diagram("mermaid", &["test".into()], 0.0)?);
        Ok(layout.plan)
    }
    #[test]
    fn ordinary_untyped_code_does_not_warn_about_diagrams() {
        let p = paginate_with("```\nordinary source code\n```", &small(), &Fake).unwrap();
        assert_eq!(text(&p), "ordinary source code");
        assert!(p.warnings.is_empty());
    }
    #[test]
    fn auto_wraps_readable_columns_before_making_bands() {
        let mut widths = vec![260.0; 3];
        fit_readable_columns(&mut widths, 523.0, 70.0);
        assert!((widths.iter().sum::<f64>() - 523.0).abs() < 0.001);
        assert_eq!(column_bands(&widths, 523.0, true).unwrap().len(), 1);
        let mut widths = vec![100.0; 13];
        fit_readable_columns(&mut widths, 523.0, 70.0);
        assert_eq!(widths, vec![100.0; 13]);
        assert!(column_bands(&widths, 523.0, true).unwrap().len() > 1);
    }
    #[test]
    fn normal_rows_move_intact_instead_of_splitting_to_fill_leftover_space() {
        let source = format!(
            "| Key |\n|---|\n| {} |\n| {} |\n| {} |",
            "A".repeat(125),
            "B".repeat(125),
            "C".repeat(125)
        );
        let p = paginate_with(&source, &small(), &Fake).unwrap();
        assert!(p.pages.len() > 1);
        for letter in ['A', 'B', 'C'] {
            let mut total = 0;
            for page in &p.pages {
                let count = page
                    .items
                    .iter()
                    .filter_map(|i| match i {
                        Item::Text { spans, .. } => Some(doc::spans_plain(spans)),
                        _ => None,
                    })
                    .flat_map(|s| s.chars().collect::<Vec<_>>())
                    .filter(|&c| c == letter)
                    .count();
                assert!(
                    count == 0 || count == 125,
                    "ordinary row split: {letter}: {count}"
                );
                total += count;
            }
            assert_eq!(total, 125);
        }
    }
    #[test]
    fn url_setting_controls_visible_link_destinations() {
        let source = "[label](https://example.org)";
        let p = paginate_with(source, &small(), &Fake).unwrap();
        assert!(text(&p).contains("https://example.org"));
        let p = paginate_with(
            source,
            &Settings {
                urls: false,
                ..small()
            },
            &Fake,
        )
        .unwrap();
        assert_eq!(text(&p), "label");
    }
    #[test]
    fn short_table_shares_heading_page_and_relocates_heading_when_needed() {
        for height in [200.0, 130.0] {
            let settings = Settings {
                page_size_override: Some((240.0, height)),
                ..small()
            };
            let p = paginate_with(
                "lead\n\n# Heading\n\n| A |\n|---|\n| body |",
                &settings,
                &Fake,
            )
            .unwrap();
            let page = p
                .pages
                .iter()
                .find(|p| {
                    p.items.iter().any(|i|
                matches!(i, Item::Text { spans, .. } if doc::spans_plain(spans) == "Heading"))
                })
                .unwrap();
            assert!(page.items.iter().any(
                |i| matches!(i, Item::Text { spans, .. } if doc::spans_plain(spans) == "body")
            ));
            assert_eq!(p.pages.len(), if height == 200.0 { 1 } else { 2 });
        }
    }
    #[test]
    fn tiled_diagram_shares_heading_page_without_changing_row_coverage() {
        let settings = Settings {
            diagram_mode: FitMode::Tile,
            ..small()
        };
        let p = cached_diagram_with_heading(&settings, 480.0, 410.0, true).unwrap();
        assert!(p.pages[0].items.iter().any(
            |i| matches!(i, Item::Text { spans, .. } if doc::spans_plain(spans) == "Heading")
        ));
        let mut intervals = Vec::new();
        for page in &p.pages {
            let tile = page
                .items
                .iter()
                .find_map(|i| match i {
                    Item::Svg {
                        y,
                        clip: Some((_, cy, _, ch)),
                        ..
                    } => {
                        assert!(cy + ch <= page.height - settings.margin + 0.01);
                        Some((cy - y, *ch))
                    }
                    _ => None,
                })
                .expect("no heading-only tile page");
            intervals.push(tile);
        }
        assert!(intervals
            .iter()
            .all(|(_, h)| (*h - intervals[0].1).abs() < 0.01));
        intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut end: f64 = 0.0;
        for (start, h) in intervals {
            assert!(start <= end + 0.01);
            end = end.max(start + h);
        }
        assert!(end >= 410.0);
    }
    #[test]
    fn cancellation_is_checked_after_a_metric_operation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct CancelMetrics(Arc<AtomicBool>);
        impl Metrics for CancelMetrics {
            fn text(
                &self,
                spans: &[Span],
                size: f64,
                bold: bool,
                mono: bool,
                width: Option<f64>,
            ) -> Result<Measure, String> {
                let result = Fake.text(spans, size, bold, mono, width);
                self.0.store(true, Ordering::Relaxed);
                result
            }
            fn svg_size(&self, _: &str) -> Result<(f64, f64), String> {
                unreachable!()
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        let settings = Settings {
            cancel: Some(flag.clone()),
            ..small()
        };
        assert!(paginate_with("text", &settings, &CancelMetrics(flag))
            .unwrap_err()
            .contains("cancelled"));
    }
    #[test]
    fn disabled_diagrams_print_source_without_warning() {
        let settings = Settings {
            diagrams: false,
            ..small()
        };
        let p = paginate_with("```mermaid\nnot valid mermaid\n```", &settings, &Fake).unwrap();
        assert_eq!(text(&p), "not valid mermaid");
        assert!(p.warnings.is_empty());
        assert!(p
            .pages
            .iter()
            .flat_map(|p| &p.items)
            .all(|i| !matches!(i, Item::Svg { .. })));
    }
    #[test]
    fn table_alignment_is_applied_per_wrapped_line() {
        let p = paginate_with(
            "| left | center | right |\n|:---|:---:|---:|\n| a | b | c |",
            &small(),
            &Fake,
        )
        .unwrap();
        let mut body = Vec::new();
        for page in &p.pages {
            let mut cell = None;
            for item in &page.items {
                match item {
                    Item::Rect { x, width, .. } => cell = Some((*x, *width)),
                    Item::Text {
                        x,
                        spans,
                        bold: false,
                        ..
                    } => {
                        let (cx, cw) = cell.unwrap();
                        let s = doc::spans_plain(spans);
                        let expected = match s.as_str() {
                            "a" => cx + PAD,
                            "b" => cx + (cw - 5.0) / 2.0,
                            "c" => cx + cw - PAD - 5.0,
                            _ => panic!("unexpected body"),
                        };
                        assert!((x - expected).abs() < 0.01);
                        body.push(s);
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(body, ["a", "b", "c"]);
    }
    #[test]
    fn tile_rectangles_cover_both_axes_without_rescaling_svg() {
        let settings = Settings {
            diagram_mode: FitMode::Tile,
            ..small()
        };
        let p = cached_diagram(&settings, 480.0, 410.0).unwrap();
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for page in &p.pages {
            let tile = page
                .items
                .iter()
                .find_map(|i| match i {
                    Item::Svg {
                        x,
                        y,
                        width,
                        height,
                        clip: Some((cx, cy, cw, ch)),
                        ..
                    } => {
                        assert_eq!((*width, *height), (480.0, 410.0));
                        assert!(*cx >= 15.0 && *cy >= 15.0);
                        assert!(cx + cw <= 225.01 && cy + ch <= 185.01);
                        // Every viewport intersects the full, translated SVG.
                        assert!(*x < cx + cw && x + width > *cx);
                        assert!(*y < cy + ch && y + height > *cy);
                        Some((cx - x, cy - y, *cw, *ch))
                    }
                    _ => None,
                })
                .unwrap();
            xs.push((tile.0, tile.2));
            ys.push((tile.1, tile.3));
        }
        for (mut intervals, extent) in [(xs, 480.0), (ys, 410.0)] {
            intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut end: f64 = 0.0;
            for (start, len) in intervals {
                assert!(start <= end + 0.01);
                end = end.max(start + len);
            }
            assert!(end >= extent);
        }
        assert!(cached_diagram(&settings, 1e100, 410.0)
            .unwrap_err()
            .contains("tile limit"));
    }
    #[test]
    fn minimum_content_geometry_is_safe_even_at_maximum_font() {
        let settings = Settings {
            page_size_override: Some((72.0, 72.0)),
            margin: 12.0,
            overlap: 0.0,
            font_size: 72.0,
            diagram_mode: FitMode::Fit,
            ..Settings::default()
        };
        assert!(paginate_with("A", &settings, &Fake).is_err());
        assert!(paginate_with("| A |\n|---|\n| B |", &settings, &Fake).is_err());
        let p = cached_diagram(&settings, 200.0, 200.0).unwrap();
        assert!(matches!(&p.pages[0].items[0], Item::Svg {width, height, ..}
            if (*width - 48.0).abs() < 0.01 && (*height - 48.0).abs() < 0.01));
        let settings = Settings {
            diagram_mode: FitMode::Tile,
            ..settings
        };
        assert!(cached_diagram(&settings, 80.0, 80.0).is_ok());
    }
    #[test]
    fn heading_stays_with_following_opening_pair_on_short_pages() {
        let settings = Settings {
            page_size_override: Some((180.0, 100.0)),
            margin: 10.0,
            overlap: 0.0,
            ..Settings::default()
        };
        let p = paginate_with(
            &format!("lead\n\n# Heading\n\n{}", "body ".repeat(8)),
            &settings,
            &Fake,
        )
        .unwrap();
        let heading_page = p
            .pages
            .iter()
            .position(|p| {
                p.items
                    .iter()
                    .any(|i| matches!(i, Item::Text { bold: true, .. }))
            })
            .unwrap();
        assert!(heading_page > 0);
        assert!(p.pages[heading_page]
            .items
            .iter()
            .any(|i| matches!(i, Item::Text {bold: false, spans, ..}
                if doc::spans_plain(spans).starts_with("body"))));
    }
    #[test]
    fn metrics_cannot_silently_skip_or_overlap_utf8_content() {
        struct Bad(u8);
        impl Metrics for Bad {
            fn text(
                &self,
                _: &[Span],
                _: f64,
                _: bool,
                _: bool,
                _: Option<f64>,
            ) -> Result<Measure, String> {
                let ranges = match self.0 {
                    0 => vec![(1, 5)],         // Middle of é.
                    1 => vec![(0, 2), (0, 5)], // Overlapping byte ranges.
                    2 => vec![(2, 5)],         // Missing é.
                    3 => vec![(0, 2)],         // Missing 猫.
                    _ => vec![],               // Missing all content.
                };
                Ok(Measure {
                    width: 10.0,
                    lines: ranges
                        .into_iter()
                        .map(|(start, end)| Run {
                            start,
                            end,
                            height: 13.0,
                        })
                        .collect(),
                })
            }
            fn svg_size(&self, _: &str) -> Result<(f64, f64), String> {
                unreachable!()
            }
        }
        for kind in 0..5 {
            assert!(paginate_with("é猫", &small(), &Bad(kind)).is_err());
        }
    }
    #[test]
    fn header_only_table_does_not_reserve_an_absent_body() {
        let settings = Settings {
            page_size_override: Some((240.0, 76.0)),
            margin: 10.0,
            overlap: 0.0,
            ..Settings::default()
        };
        let p = paginate_with(
            "| header header header header header header header header |\n|---|",
            &settings,
            &Fake,
        )
        .unwrap();
        assert!(p.warnings.is_empty());
        assert_eq!(
            text(&p),
            "header header header header header header header header"
        );
    }
    #[test]
    fn paragraphs_paginate_without_losing_unicode() {
        let source = "é猫 ".repeat(800);
        let p = paginate_with(&source, &small(), &Fake).unwrap();
        assert!(p.pages.len() > 2);
        assert_eq!(text(&p), source.trim_end());
        for page in &p.pages {
            for item in &page.items {
                if let Item::Text { x, y, size, .. } = item {
                    assert!(*x >= 15.0 && *y >= 15.0 && *y + size * 1.3 <= 185.01);
                }
            }
        }
    }
    #[test]
    fn code_wraps_instead_of_clipping() {
        let source = format!("```rust\n{}\n```", "x".repeat(1000));
        let p = paginate_with(&source, &small(), &Fake).unwrap();
        assert_eq!(text(&p), "x".repeat(1000));
        assert!(p.pages.len() > 1);
        assert!(p
            .pages
            .iter()
            .flat_map(|p| &p.items)
            .all(|i| !matches!(i, Item::Text { mono: false, .. })));
    }
    #[test]
    fn table_headers_repeat_and_giant_rows_split() {
        let source = format!(
            "| Identifier | Content |\n|---|---|\n| id | {} |",
            "word ".repeat(500)
        );
        let p = paginate_with(&source, &small(), &Fake).unwrap();
        assert!(p.pages.len() > 2);
        for page in &p.pages {
            assert!(page.items.iter().any(|i|matches!(i,Item::Text {spans,bold:true,..} if doc::spans_plain(spans)=="Identifier")));
            for i in &page.items {
                if let Item::Rect { y, height, .. } = i {
                    assert!(*y + height <= 185.01);
                }
            }
        }
        assert_eq!(text(&p).matches("word").count(), 500);
    }
    #[test]
    fn giant_header_splits_without_loss() {
        let source = format!("| {} |\n|---|\n| body |", "header ".repeat(1000));
        let p = paginate_with(&source, &small(), &Fake).unwrap();
        assert_eq!(
            text(&p),
            format!("{}body", "header ".repeat(1000).trim_end())
        );
        assert_eq!(p.warnings.len(), 1);
    }
    #[test]
    fn column_bands_share_row_fragments() {
        let source=format!("# Heading\n\n| ID | First wide heading | Second wide heading | Third wide heading |\n|---|---|---|---|\n| key | {} | {} | {} |", "one ".repeat(80),"two ".repeat(80),"six ".repeat(80));
        let p = paginate_with(&source, &small(), &Fake).unwrap();
        assert!(p.pages.len() >= 6);
        let heights: Vec<Vec<f64>> = p
            .pages
            .iter()
            .map(|p| {
                p.items
                    .iter()
                    .filter_map(|i| match i {
                        Item::Rect { height, .. } => Some(*height),
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        for group in heights.chunks(3) {
            assert_eq!(group.len(), 3);
            assert_eq!(group[0], group[1]);
            assert_eq!(group[1], group[2]);
        }
        let body: String = p
            .pages
            .iter()
            .flat_map(|p| &p.items)
            .filter_map(|i| match i {
                Item::Text {
                    spans, bold: false, ..
                } => Some(doc::spans_plain(spans)),
                _ => None,
            })
            .collect();
        // A word may itself wrap across a vertical fragment boundary.
        for letter in ['n', 'w', 'x'] {
            assert_eq!(body.chars().filter(|&c| c == letter).count(), 80);
        }
    }
    #[test]
    fn native_pagination_when_available() {
        let Ok(native) = Native::load() else { return };
        let source=format!("# Heading\n\n- Unicode é猫 and **bold** text\n\n```rust\n{}\n```\n\n| ID | Text |\n|---|---|\n| a | {} |", "long_identifier_".repeat(100),"body ".repeat(200));
        let p = paginate(&source, &small(), &native).unwrap();
        assert!(p.pages.len() > 2);
        assert!(text(&p).contains("é猫"));
    }
    #[test]
    fn native_wrapped_break_spaces_do_not_overflow_fragments() {
        let Ok(native) = Native::load() else { return };
        let paragraph = "Paragraph with **bold** and _italic_ words. ".repeat(100);
        let code = format!("```mermaid\n{}\n```", "node --> other_node ".repeat(100));
        let table = format!("| Text |\n|---|\n| {} |", paragraph);
        let settings = Settings { diagrams: false, ..small() };
        for source in [&paragraph, &code, &table] {
            let p = paginate(source, &settings, &native).unwrap();
            let mut words = String::new();
            for page in &p.pages {
                for item in &page.items {
                    if let Item::Text { x, y, spans, size, bold, mono, .. } = item {
                        let m = native.text(spans, *size, *bold, *mono, None).unwrap();
                        assert!(x + m.width <= page.width - settings.margin + 0.01);
                        assert!(y + m.height <= page.height - settings.margin + 0.01);
                        assert!(m.lines.len() <= 1, "fragment introduced another line");
                        words.push_str(&doc::spans_plain(spans));
                    }
                }
            }
            if source == &code {
                assert_eq!(words.matches("node --> other_node").count(), 100);
            } else {
                assert_eq!(words.matches("Paragraph").count(), 100);
                assert_eq!(words.matches("words.").count(), 100);
            }
        }
    }
    #[test]
    fn fitted_tall_diagram_reserves_heading_space_before_scaling() {
        let settings = Settings { diagram_mode: FitMode::Fit, ..small() };
        let p = cached_diagram_with_heading(&settings, 100.0, 10_000.0, true).unwrap();
        let page = p.pages.iter().find(|page| page.items.iter().any(|item|
            matches!(item, Item::Svg { .. }))).unwrap();
        assert!(page.items.iter().any(|item|
            matches!(item, Item::Text { spans, .. } if doc::spans_plain(spans) == "Heading")));
        for item in &page.items {
            if let Item::Svg { y, height, .. } = item {
                assert!(height.is_finite() && *height > 0.0);
                assert!(y + height <= page.height - settings.margin + 0.01);
            }
        }
    }
    #[test]
    fn native_first_ordinary_row_moves_with_pending_heading_intact() {
        let Ok(native) = Native::load() else { return };
        let settings = Settings { paper: super::super::Paper::Letter, margin: 200.0,
            ..Settings::default() };
        let source = format!("lead\n\n# Heading\n\n| Key |\n|---|\n| {} |", "WORD ".repeat(135));
        let p = paginate(&source, &settings, &native).unwrap();
        let pages_with_words: Vec<_> = p.pages.iter().filter(|page| page.items.iter().any(|item|
            matches!(item, Item::Text { spans, .. } if doc::spans_plain(spans).contains("WORD")))).collect();
        assert_eq!(pages_with_words.len(), 1, "ordinary row was split");
        let page = pages_with_words[0];
        assert!(page.items.iter().any(|item|
            matches!(item, Item::Text { spans, .. } if doc::spans_plain(spans) == "Heading")));
        let words: String = page.items.iter().filter_map(|item| match item {
            Item::Text { spans, .. } => Some(doc::spans_plain(spans)), _ => None,
        }).collect();
        assert_eq!(words.matches("WORD").count(), 135);
    }
    #[test]
    fn bands_never_cut_columns() {
        assert_eq!(
            column_bands(&[30.0, 40.0, 50.0, 30.0], 80.0, true).unwrap(),
            vec![vec![0, 1], vec![0, 2], vec![0, 3]]
        );
        assert!(column_bands(&[90.0], 80.0, false).is_err());
    }
    #[test]
    fn tiles_cover_and_cap_overlap() {
        let offsets = tile_offsets(250.0, 100.0, 99.0).unwrap();
        assert_eq!(offsets, vec![0.0, 75.0, 150.0]);
        assert!(tile_offsets(1e100, 100.0, 0.0).is_err());
        assert!(tile_offsets(f64::NAN, 100.0, 0.0).is_err());
    }
    #[test]
    fn unicode_styled_slices_preserve_links() {
        let mut spans = plain("é猫abc");
        spans[0].url = Some("https://example.org".into());
        let sliced = doc::spans_slice(&spans, 2, 5);
        assert_eq!(sliced[0].text, "猫");
        assert_eq!(sliced[0].url, spans[0].url);
    }
}
