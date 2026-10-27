// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The heading + rows list:
//! per-cell hyperlinks, check/cross icons, chips, "(n)" suffixes, row backgrounds,
//! hover hints and a right-click hook.

use eframe::egui::{self, Color32, CursorIcon, Id, Pos2, Rect, Sense, Ui, Vec2};

use crate::theme::{Fonts, Palette, ICON_BAD, ICON_OK};
use crate::ui::{self, measure, Rnd, Scroll, TipAt, Tips};

/// inner left margin of heading and rows
pub const PADX: f32 = 12.0;
/// space after each column's widest text
pub const COL_GAP: f32 = 18.0;

pub struct Col {
    pub name: &'static str,
    pub heading: &'static str,
    pub sample: String,
    /// hover hint over the heading
    pub tip: Option<&'static str>,
}

impl Col {
    pub fn new(name: &'static str, heading: &'static str, sample: &str) -> Self {
        Col { name, heading, sample: sample.to_string(), tip: None }
    }

    pub fn tip(mut self, tip: &'static str) -> Self {
        self.tip = Some(tip);
        self
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Ok,
    Bad,
}

#[derive(Clone)]
pub struct Chip {
    pub text: String,
    pub fill: Color32,
    pub fg: Color32,
    pub tip: Option<String>,
}

/// Before a cell's text: a chip (fill set) or plain bold muted text like a suffix.
#[derive(Clone)]
pub struct Lead {
    pub text: String,
    pub chip: Option<(Color32, Color32)>,
    pub tip: Option<String>,
}

#[derive(Clone, Default)]
pub struct Cell {
    pub text: String,
    pub link: Option<String>,
    /// hover hint over the link text
    pub link_tip: Option<String>,
    pub icon: Option<Icon>,
    pub chip: Option<Chip>,
    /// after the text, outside the link, with an optional hover hint
    pub suffix: Option<(String, Option<String>)>,
    pub lead: Option<Lead>,
    /// a faint red text colour (old spots' Ago)
    pub stale: bool,
}

impl Cell {
    pub fn text(s: impl Into<String>) -> Self {
        Cell { text: s.into(), ..Default::default() }
    }
}

#[derive(Clone, Default)]
pub struct RowTags {
    pub worked: bool,
    pub target: bool,
    pub pending: bool,
    pub heard: bool,
    pub unlogged: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RowData {
    pub call: String,
    pub ref_: String,
    pub status: String,
    pub park: String,
    pub skipped: bool,
}

pub struct Row {
    pub cells: Vec<Cell>,
    pub tags: RowTags,
    pub data: Option<RowData>,
}

/// Colours that change every frame (pulsing rows).
pub struct RowColors {
    pub target: Color32,
    pub heard: Color32,
}

pub enum TableEvent {
    Context(RowData, Pos2),
}

pub struct Table {
    pub id: &'static str,
    pub columns: Vec<Col>,
    pub hidden: Vec<&'static str>,
    pub widths: Vec<f32>,
    need: Vec<f32>,
    pub rows: Vec<Row>,
    pub scroll: Scroll,
    /// rows shown before scrolling when the window first opens
    pub height_rows: usize,
}

struct Chunk {
    asc: f32,
    desc: f32,
}

impl Table {
    pub fn new(id: &'static str, columns: Vec<Col>, height_rows: usize) -> Self {
        let n = columns.len();
        Table { id, columns, hidden: vec![], widths: vec![0.0; n], need: vec![0.0; n], rows: vec![], scroll: Scroll::default(), height_rows }
    }

    /// Size the columns to heading and sample text (before any rows are added).
    pub fn size_from_samples(&mut self, f: &Fonts) {
        self.widths = self
            .columns
            .iter()
            .map(|c| measure(&f.table_heading, &c.heading.to_uppercase()).max(measure(&f.ui, &c.sample)) + COL_GAP)
            .collect();
    }

    pub fn fit_width(&self) -> f32 {
        self.widths.iter().sum::<f32>() + 2.0 * PADX
    }

    fn visible(&self) -> Vec<usize> {
        (0..self.columns.len()).filter(|&i| !self.hidden.contains(&self.columns[i].name)).collect()
    }

    /// x of each visible column (the first at 0).
    fn stops(&self) -> Vec<(usize, f32)> {
        let mut x = 0.0;
        self.visible()
            .into_iter()
            .map(|i| {
                let s = (i, x);
                x += self.widths[i];
                s
            })
            .collect()
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.need = vec![0.0; self.columns.len()];
    }

    pub fn add(&mut self, f: &Fonts, row: Row) {
        let space = measure(&f.ui, " ");
        for (i, c) in row.cells.iter().enumerate() {
            if i >= self.need.len() {
                break;
            }
            let mut px = measure(&f.ui, &c.text);
            if let Some(chip) = &c.chip {
                px = measure(&f.table_heading, &chip.text) + 16.0;
            } else if c.icon.is_some() {
                px = 24.0;
            }
            if let Some((s, _)) = &c.suffix {
                px += space + measure(&f.table_heading, s);
            }
            if let Some(l) = &c.lead {
                px += measure(&f.table_heading, &l.text) + if l.chip.is_some() { 14.0 } else { 0.0 } + space;
            }
            self.need[i] = self.need[i].max(px);
        }
        self.rows.push(row);
    }

    /// Size the columns to what the rows actually hold; call after adding rows.
    pub fn fit_columns(&mut self, f: &Fonts) {
        if !self.need.iter().any(|&n| n > 0.0) {
            return;
        }
        self.widths = self
            .columns
            .iter()
            .zip(&self.need)
            .map(|(c, &need)| measure(&f.table_heading, &c.heading.to_uppercase()).max(need) + COL_GAP)
            .collect();
    }

    fn nudge(f: &Fonts) -> f32 {
        2f32.max((f.ui.m.linespace * 0.15).round())
    }

    fn chip_h(f: &Fonts) -> f32 {
        f.table_heading.m.linespace + Self::nudge(f)
    }

    /// Line height and the text's top offset for a row (Tk centres the text when an
    /// embedded chip makes the line taller).
    fn line_metrics(&self, f: &Fonts, row: &Row) -> (f32, f32, f32) {
        let mut chunks = vec![Chunk { asc: f.ui.m.ascent, desc: f.ui.m.descent }];
        let mut win_h: f32 = 0.0;
        for i in self.visible() {
            let Some(c) = row.cells.get(i) else { continue };
            if c.icon.is_some() {
                // offset -2 lowers the glyph two pixels
                chunks.push(Chunk { asc: f.icon_small.m.ascent - 2.0, desc: f.icon_small.m.descent + 2.0 });
            }
            if c.suffix.is_some() || c.lead.as_ref().is_some_and(|l| l.chip.is_none()) {
                chunks.push(Chunk { asc: f.table_heading.m.ascent, desc: f.table_heading.m.descent });
            }
            if c.chip.is_some() || c.lead.as_ref().is_some_and(|l| l.chip.is_some()) {
                win_h = win_h.max(Self::chip_h(f));
            }
        }
        let asc = chunks.iter().map(|c| c.asc).fold(0.0, f32::max);
        let desc = chunks.iter().map(|c| c.desc).fold(0.0, f32::max);
        let h = (asc + desc).max(win_h);
        let shift = ((h - (asc + desc)) / 2.0).floor();
        (h, asc, shift)
    }

    fn row_height(&self, f: &Fonts, row: &Row) -> f32 {
        3.0 + self.line_metrics(f, row).0 + 3.0
    }

    /// Height the table wants for `n` empty-looking rows (window sizing).
    pub fn rows_height(f: &Fonts, n: usize) -> f32 {
        n as f32 * (f.ui.m.linespace + 6.0) + 4.0
    }

    pub fn heading_height(f: &Fonts) -> f32 {
        f.table_heading.m.linespace + 12.0
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(&mut self, ui: &mut Ui, rect: Rect, p: &Palette, f: &Fonts, colors: &RowColors, tips: &mut Tips, events: &mut Vec<TableEvent>) {
        let painter = ui.painter().clone();
        let text_w = rect.width() - 12.0;
        let stops = self.stops();
        // heading
        let head_h = Self::heading_height(f);
        for &(i, x) in &stops {
            let head = self.columns[i].heading.to_uppercase();
            let pos = Pos2::new(rect.left() + PADX + x, rect.top() + 6.0);
            ui::text_on(ui, &painter, pos, &head, &f.table_heading, p.muted, p.surface);
            if let Some(tip) = self.columns[i].tip {
                let hr = Rect::from_min_size(Pos2::new(pos.x, rect.top()), Vec2::new(measure(&f.table_heading, &head), head_h));
                let resp = ui.interact(hr, Id::unique((self.id, "head", i)), Sense::hover());
                tips.tip(&resp, tip, 300, TipAt::Below(hr));
            }
        }
        let rule_y = rect.top() + head_h;
        painter.rect_filled(Rect::from_min_size(Pos2::new(rect.left(), rule_y), Vec2::new(text_w, 1.0)), 0.0, p.border);
        // body
        let body = Rect::from_min_max(Pos2::new(rect.left(), rule_y + 1.0), Pos2::new(rect.left() + text_w, rect.bottom()));
        let heights: Vec<f32> = self.rows.iter().map(|r| self.row_height(f, r)).collect();
        let content = 4.0 + heights.iter().sum::<f32>();
        let view = body.height();
        self.scroll.wheel(ui, body, f.ui.m.linespace + 6.0, content, view);
        self.scroll.clamp(content, view);
        let bar = Rect::from_min_max(Pos2::new(rect.right() - 12.0, rect.top() + 2.0), Pos2::new(rect.right() - 2.0, rect.bottom() - 2.0));
        self.scroll.bar(ui, Id::unique((self.id, "bar")), bar, content, view, p);

        let clip = painter.with_clip_rect(body.intersect(painter.clip_rect()));
        let pointer = ui.ctx().pointer_hover_pos();
        let hover_ok = ui::hover_in(ui, body);
        let mut y = body.top() + 2.0 - self.scroll.offset;
        let mut hand = false;
        let space = measure(&f.ui, " ");
        for (ri, row) in self.rows.iter().enumerate() {
            let rh = heights[ri];
            let row_rect = Rect::from_min_size(Pos2::new(body.left(), y), Vec2::new(text_w, rh));
            y += rh;
            if row_rect.bottom() < body.top() || row_rect.top() > body.bottom() {
                continue;
            }
            let t = &row.tags;
            let bg = if t.heard {
                Some(colors.heard)
            } else if t.worked {
                Some(p.dim_row)
            } else if t.pending {
                Some(p.select)
            } else if t.target {
                Some(colors.target)
            } else {
                None
            };
            if let Some(bg) = bg {
                clip.rect_filled(row_rect, 3.0, bg);
            }
            // what the text really sits on, for ClearType
            let text_bg = bg.unwrap_or(p.surface);
            let base_fg = if t.unlogged {
                p.bad
            } else if t.worked {
                p.dim_text
            } else {
                p.text
            };
            let row_bg = if t.worked { p.dim_row } else { p.surface };
            let (line_h, asc, shift) = self.line_metrics(f, row);
            let line_top = row_rect.top() + 3.0;
            let baseline = line_top + shift + asc;
            let text_top = |font: &crate::theme::Font| baseline - font.m.ascent;
            let win_top = line_top + ((line_h - Self::chip_h(f)) / 2.0).floor() + Self::nudge(f);
            let row_hover = hover_ok && pointer.is_some_and(|pp| row_rect.contains(pp));
            if row_hover {
                if let Some(d) = &row.data {
                    let resp = ui.interact(row_rect.intersect(body), Id::unique((self.id, "row", ri)), Sense::click());
                    if resp.secondary_clicked() {
                        events.push(TableEvent::Context(d.clone(), pointer.unwrap()));
                    }
                }
            }
            for &(i, x0) in &stops {
                let Some(c) = row.cells.get(i) else { continue };
                let mut x = body.left() + PADX + x0;
                if let Some(l) = &c.lead {
                    let w = measure(&f.table_heading, &l.text);
                    match l.chip {
                        Some((fill, fg)) => {
                            let r = Rnd::new(&l.text, &f.table_heading).pill().pad(7.0, 0.0).colors(fill, fg);
                            let sz = r.size(ui);
                            let rr = Rect::from_min_size(Pos2::new(x, win_top), sz);
                            r.paint(ui, &clip, rr, false);
                            if let Some(tip) = &l.tip {
                                let resp = ui.interact(rr.intersect(body), Id::unique((self.id, "lead", ri, i)), Sense::hover());
                                tips.tip(&resp, tip, 300, TipAt::Below(rr));
                            }
                            x += sz.x;
                        }
                        None => {
                            let rr = Rect::from_min_size(Pos2::new(x, text_top(&f.table_heading)), Vec2::new(w, f.table_heading.m.linespace));
                            ui::text_on(ui, &clip, rr.min, &l.text, &f.table_heading, p.muted, text_bg);
                            if let Some(tip) = &l.tip {
                                let hovered = hover_ok && pointer.is_some_and(|pp| rr.contains(pp));
                                tips.tip_area(Id::unique((self.id, "leadtip", ri, i)), hovered, tip, 300, TipAt::Pointer);
                            }
                            x += w;
                        }
                    }
                    x += space;
                }
                if let Some(chip) = &c.chip {
                    let r = Rnd::new(&chip.text, &f.table_heading).pill().pad(8.0, 0.0).colors(chip.fill, chip.fg);
                    let sz = r.size(ui);
                    let rr = Rect::from_min_size(Pos2::new(x, win_top), sz);
                    r.paint(ui, &clip, rr, false);
                    if let Some(tip) = &chip.tip {
                        let resp = ui.interact(rr.intersect(body), Id::unique((self.id, "chip", ri, i)), Sense::hover());
                        tips.tip(&resp, tip, 300, TipAt::Below(rr));
                    }
                    x += sz.x;
                } else if let Some(icon) = c.icon {
                    let (glyph, color) = match (icon, f.has_icons) {
                        (Icon::Ok, true) => (ICON_OK, p.ok),
                        (Icon::Bad, true) => (ICON_BAD, p.bad),
                        (Icon::Ok, false) => ("✓", p.ok),
                        (Icon::Bad, false) => ("✗", p.bad),
                    };
                    let font = if f.has_icons { &f.icon_small } else { &f.ui };
                    let top = baseline + 2.0 - font.m.ascent;
                    x += ui::text_on(ui, &clip, Pos2::new(x, top), glyph, font, color, text_bg);
                } else if let (Some(url), false) = (&c.link, c.text.is_empty()) {
                    let w = ui::text_on(ui, &clip, Pos2::new(x, text_top(&f.ui)), &c.text, &f.ui, p.link, text_bg);
                    let rr = Rect::from_min_size(Pos2::new(x, line_top), Vec2::new(w, line_h));
                    if hover_ok && pointer.is_some_and(|pp| rr.contains(pp)) {
                        hand = true;
                        let resp = ui.interact(rr.intersect(body), Id::unique((self.id, "link", ri, i)), Sense::click());
                        if resp.clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        }
                        if let Some(tip) = &c.link_tip {
                            tips.tip(&resp, tip, 300, TipAt::Below(rr));
                        }
                    }
                    x += w;
                } else {
                    let fg = if c.stale { stale_color(p, row_bg) } else { base_fg };
                    x += ui::text_on(ui, &clip, Pos2::new(x, text_top(&f.ui)), &c.text, &f.ui, fg, text_bg);
                }
                if let Some((s, tip)) = &c.suffix {
                    x += space;
                    let w = ui::text_on(ui, &clip, Pos2::new(x, text_top(&f.table_heading)), s, &f.table_heading, p.muted, text_bg);
                    if let Some(tip) = tip {
                        let rr = Rect::from_min_size(Pos2::new(x, line_top), Vec2::new(w, line_h));
                        let hovered = hover_ok && pointer.is_some_and(|pp| rr.contains(pp));
                        tips.tip_area(Id::unique((self.id, "suffix", ri, i)), hovered, tip, 300, TipAt::Pointer);
                    }
                }
            }
        }
        if hand {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }
}

/// Spots older than STALE_MINUTES: the Ago text takes a faint red (Tk has no alpha, so
/// the red is blended 40% into the row's background).
pub fn stale_color(p: &Palette, base: Color32) -> Color32 {
    crate::theme::mix(base, p.bad, 0.4)
}
