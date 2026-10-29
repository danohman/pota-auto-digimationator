// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The Activity log: a word-wrapped monospace list of lines, each with a timestamp, any
//! one-shot buttons (Call now, Skip, Answer, Halt Tx) and an optional NEW chip.

use std::collections::VecDeque;

use eframe::egui::{Color32, Event, Id, Pos2, Rect, Sense, Ui, Vec2};

use crate::hunter::{Action, ActionKind, Active, Tag};
use crate::theme::{Fonts, LogTag, Palette};
use crate::ui::{self, measure, Rnd, Scroll, TipAt, Tips};

pub const MAX_LOG_LINES: usize = 3000;

/// Hover hints for the buttons in log lines
pub fn action_tip(label: &str) -> &'static str {
    match label {
        "Call now" => "Reply to this CQ right away, switching from\nany station you're calling now",
        "Skip" => "Never call this station again today (this band,\nmode and park). Stops transmitting if it's\nthe one being called",
        "Answer" => "Reply to this station that is calling you",
        "Keep calling" => "Start another round of calls to this station
(the count goes back to 1)",
        "Halt Tx" => "Stop transmitting now. The station is not skipped,\nso it can be called again",
        _ => "",
    }
}

pub struct LogButton {
    pub id: u64,
    pub label: &'static str,
    pub kind: ActionKind,
    pub active: Option<Active>,
    pub used: bool,
    pub glowing: bool,
    /// fill while glowing (set by the app's pulse)
    pub glow_fill: Color32,
}

pub struct Line {
    uid: u64,
    pub ts: String,
    pub text: String,
    pub tag: Tag,
    pub buttons: Vec<LogButton>,
    pub new: bool,
    pub summary: bool,
    /// (avail width, wrapped rows as char ranges, first row height)
    laid: Option<(f32, Vec<(usize, usize)>, f32)>,
}

impl Line {
    fn haystack(&self) -> String {
        let mut s = format!("{}  ", self.ts);
        for _ in &self.buttons {
            s.push(' ');
        }
        if self.new {
            s.push(' ');
        }
        s.push_str(&self.text);
        s.to_lowercase()
    }
}

pub fn log_tag(t: Tag) -> LogTag {
    match t {
        Tag::Info => LogTag::Info,
        Tag::Cand => LogTag::Cand,
        Tag::Call => LogTag::Call,
        Tag::Worked => LogTag::Worked,
        Tag::Dupe => LogTag::Dupe,
        Tag::Summary => LogTag::Summary,
        Tag::Skip => LogTag::Skip,
        Tag::Error => LogTag::Error,
        Tag::Hunt => LogTag::Hunt,
    }
}

#[derive(Default)]
pub struct LogView {
    pub lines: VecDeque<Line>,
    pub scroll: Scroll,
    pub filter: String,
    next_id: u64,
    next_uid: u64,
    /// mouse selection: (anchor, focus), each (line uid, char index in the line's text)
    sel: Option<((u64, usize), (u64, usize))>,
    /// for (anchor, focus): was the pointer over the timestamp (not the gap or the text)?
    /// Only then is a line's timestamp part of the selection.
    sel_ts: (bool, bool),
    stick: bool,
    /// filter changed: show the end (Tk's `see end`)
    pub jump_end: bool,
}

pub enum LogClick {
    Button(u64),
}

impl LogView {
    pub fn new() -> Self {
        LogView { stick: true, ..Default::default() }
    }

    /// Append a line; `actions` become buttons right after the timestamp.
    pub fn add(&mut self, ts: String, text: String, tag: Tag, actions: Vec<Action>, new: bool) {
        let text = crate::logdialog::localize(&text);
        if tag == Tag::Summary {
            // Replace the previous summary line instead of piling them up.
            self.lines.retain(|l| !l.summary);
        }
        let mut buttons = Vec::new();
        for a in actions {
            if a.label == "Halt Tx" {
                // only the latest line offers it
                for l in self.lines.iter_mut() {
                    l.buttons.retain(|b| b.label != "Halt Tx");
                }
            }
            // only the newest line about a station keeps its buttons
            if let Some(call) = a.kind.call().map(crate::hunter::base_call) {
                for l in self.lines.iter_mut() {
                    let before = l.buttons.len();
                    l.buttons.retain(|b| b.kind.call().map(crate::hunter::base_call).as_deref() != Some(call.as_str()));
                    if l.buttons.len() != before {
                        l.laid = None;
                    }
                }
            }
            self.next_id += 1;
            buttons.push(LogButton {
                id: self.next_id,
                label: a.label,
                kind: a.kind,
                active: a.active,
                used: false,
                glowing: false,
                glow_fill: Color32::TRANSPARENT,
            });
        }
        self.next_uid += 1;
        self.lines.push_back(Line { uid: self.next_uid, ts, text, tag, buttons, new, summary: tag == Tag::Summary, laid: None });
        while self.lines.len() > MAX_LOG_LINES {
            self.lines.pop_front();
        }
    }

    /// The fonts changed size: every line is wrapped again at the next draw.
    pub fn relayout(&mut self) {
        for l in self.lines.iter_mut() {
            l.laid = None;
        }
        self.sel = None;
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.sel = None;
        self.scroll.offset = 0.0;
    }

    pub fn button_mut(&mut self, id: u64) -> Option<&mut LogButton> {
        self.lines.iter_mut().flat_map(|l| l.buttons.iter_mut()).find(|b| b.id == id)
    }

    pub fn buttons_mut(&mut self) -> impl Iterator<Item = &mut LogButton> {
        self.lines.iter_mut().flat_map(|l| l.buttons.iter_mut())
    }

    /// Take a button out of its line (and the space after it).
    pub fn remove_button(&mut self, id: u64) {
        for l in self.lines.iter_mut() {
            let before = l.buttons.len();
            l.buttons.retain(|b| b.id != id);
            if l.buttons.len() != before {
                l.laid = None;
            }
        }
    }

    fn matches(words: &[String], l: &Line) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = l.haystack();
        words.iter().all(|w| hay.contains(w.as_str()))
    }

    fn prefix_width(f: &Fonts, l: &Line, cw: f32) -> f32 {
        let mut w = 10.0 * cw; // "HH:MM:SS  "
        for b in &l.buttons {
            if !is_trailing(b) {
                w += measure_btn(f, b.label) + cw;
            }
        }
        if l.new {
            w += measure_btn(f, "NEW") + cw;
        }
        w
    }

    /// First row height (embedded buttons and the NEW chip are taller than the text).
    fn first_row_h(f: &Fonts, l: &Line) -> f32 {
        let mut h = f.mono.m.linespace;
        if !l.buttons.is_empty() {
            h = h.max(btn_h(f));
        }
        if l.new {
            h = h.max(btn_h(f) + 4.0);
        }
        h
    }

    /// Word-wrap like Tk's Text (wrap="word"): break after spaces, a word longer than a
    /// line is broken by character. Rows are char ranges of `l.text`.
    fn wrap(l: &Line, avail: f32, prefix: f32, cw: f32) -> Vec<(usize, usize)> {
        let chars: Vec<char> = l.text.chars().collect();
        let mut rows = Vec::new();
        let mut start = 0usize;
        let mut room = ((avail - prefix) / cw).floor().max(1.0) as usize;
        let full = (avail / cw).floor().max(1.0) as usize;
        while start < chars.len() {
            if chars.len() - start <= room {
                rows.push((start, chars.len()));
                break;
            }
            // last space within room (the space may hang past the edge)
            let limit = start + room;
            let mut brk = None;
            let mut i = limit;
            while i > start {
                if chars[i] == ' ' {
                    brk = Some(i + 1);
                    break;
                }
                i -= 1;
            }
            let end = match brk {
                Some(e) if e > start => {
                    // swallow any further spaces at the break
                    let mut e = e;
                    while e < chars.len() && chars[e] == ' ' {
                        e += 1;
                    }
                    e
                }
                _ => {
                    if rows.is_empty() && prefix > 0.0 && room < full {
                        // nothing fits after the prefix: the text starts on the next row
                        rows.push((start, start));
                        room = full;
                        continue;
                    }
                    limit
                }
            };
            rows.push((start, end));
            start = end;
            room = full;
        }
        if rows.is_empty() {
            rows.push((0, 0));
        }
        rows
    }

    fn layout(&mut self, f: &Fonts, avail: f32, cw: f32) -> Vec<f32> {
        let mut heights = Vec::with_capacity(self.lines.len());
        let filter_words = !self.filter.trim().is_empty();
        let shown: Vec<bool> = if filter_words {
            let words: Vec<String> = self.filter.to_lowercase().split_whitespace().map(str::to_string).collect();
            self.lines.iter().map(|l| Self::matches(&words, l)).collect()
        } else {
            vec![true; self.lines.len()]
        };
        for (i, l) in self.lines.iter_mut().enumerate() {
            if !shown[i] {
                heights.push(0.0);
                continue;
            }
            let fresh = matches!(&l.laid, Some((w, _, _)) if *w == avail);
            if !fresh {
                let prefix = Self::prefix_width(f, l, cw);
                let rows = Self::wrap(l, avail, prefix, cw);
                l.laid = Some((avail, rows, Self::first_row_h(f, l)));
            }
            let (_, rows, h1) = l.laid.as_ref().unwrap();
            heights.push(2.0 + h1 + (rows.len() as f32 - 1.0) * f.mono.m.linespace + 2.0);
        }
        heights
    }

    pub fn clear_selection(&mut self) {
        self.sel = None;
    }

    pub fn has_selection(&self) -> bool {
        self.sel.is_some()
    }

    /// The (line index, char index in its text) under `pos`; `None` with no visible lines.
    fn hit(&self, pos: Pos2, text_rect: Rect, heights: &[f32], f: &Fonts, cw: f32) -> Option<(usize, usize)> {
        let left = text_rect.left() + 12.0;
        let mut y = text_rect.top() + 4.0 - self.scroll.offset;
        let mut last = None;
        for (i, l) in self.lines.iter().enumerate() {
            let h = heights[i];
            if h == 0.0 {
                continue;
            }
            let top = y;
            y += h;
            last = Some(i);
            if pos.y < y {
                let (_, rows, h1) = l.laid.as_ref()?;
                let rel = pos.y - (top + 2.0);
                let ri = if rel < *h1 { 0 } else { (1 + ((rel - h1) / f.mono.m.linespace).floor() as usize).min(rows.len() - 1) };
                let (a, b) = rows[ri];
                let origin = if ri == 0 { left + Self::prefix_width(f, l, cw) } else { left };
                // the character under the pointer (the selection takes in every character it touches)
                let col = (((pos.x - origin) / cw).floor().max(0.0) as usize).min((b - a).saturating_sub(1));
                return Some((i, a + col));
            }
        }
        let i = last?;
        Some((i, self.lines[i].text.chars().count().saturating_sub(1)))
    }

    /// The selection as (start, end) line indices and char indices, start first.
    /// The selection in reading order: (line, char, pointer was over the timestamp) twice.
    fn sel_range(&self) -> Option<((usize, usize, bool), (usize, usize, bool))> {
        let ((ua, ka), (ub, kb)) = self.sel?;
        let ia = self.lines.iter().position(|l| l.uid == ua)?;
        let ib = self.lines.iter().position(|l| l.uid == ub)?;
        let (a, b) = ((ia, ka, self.sel_ts.0), (ib, kb, self.sel_ts.1));
        // the end is exclusive and takes in the character under it
        let end = |p: (usize, usize, bool)| (p.0, (p.1 + 1).min(self.lines[p.0].text.chars().count()), p.2);
        if (a.0, a.1) <= (b.0, b.1) { Some((a, end(b))) } else { Some((b, end(a))) }
    }

    /// Is the pointer over a line's timestamp (the first 8 columns), not the gap after it?
    fn over_ts(pos: Pos2, text_rect: Rect, cw: f32) -> bool {
        pos.x < text_rect.left() + 12.0 + 8.0 * cw
    }

    /// Is line `i`'s timestamp part of the selection? Only when the pointer touched it: the
    /// first line when the drag started on it, the last when it ended on it or reached its
    /// text, and every line in between.
    fn ts_selected(i: usize, r: ((usize, usize, bool), (usize, usize, bool))) -> bool {
        let ((si, sk, sts), (ei, ek, ets)) = r;
        if i < si || i > ei {
            false
        } else if si == ei {
            sk == 0 && sts
        } else if i == si {
            sk == 0 && sts
        } else if i == ei {
            ek > 0 || ets
        } else {
            true
        }
    }

    /// The selected text, one line per row of the log (timestamp included when the
    /// selection takes in the start of the line).
    fn selected_text(&self, heights: &[f32]) -> String {
        let Some(r) = self.sel_range() else { return String::new() };
        let ((si, sk, _), (ei, ek, _)) = r;
        let mut out = Vec::new();
        for i in si..=ei {
            if heights[i] == 0.0 {
                continue;
            }
            let l = &self.lines[i];
            let chars: Vec<char> = l.text.chars().collect();
            let s = if i == si { sk } else { 0 };
            let e = if i == ei { ek } else { chars.len() };
            let body: String = chars[s.min(e)..e].iter().collect();
            out.push(if Self::ts_selected(i, r) { format!("{}  {}", l.ts, body) } else { body });
        }
        out.join("\n")
    }

    /// Draw in `rect` (the log card's body). Returns the button clicked, if any.
    pub fn show(&mut self, ui: &mut Ui, rect: Rect, p: &Palette, f: &Fonts, tips: &mut Tips) -> Option<LogClick> {
        let cw = measure(&f.mono, "0");
        let text_rect = Rect::from_min_max(rect.min, Pos2::new(rect.right() - 12.0, rect.bottom()));
        let avail = text_rect.width() - 24.0;
        let heights = self.layout(f, avail, cw);
        let content = 8.0 + heights.iter().sum::<f32>();
        let view = text_rect.height();
        let was_bottom = self.stick;
        self.scroll.wheel(ui, text_rect, f.mono.m.linespace, content, view);
        let bar = Rect::from_min_max(Pos2::new(rect.right() - 12.0, rect.top() + 2.0), Pos2::new(rect.right() - 2.0, rect.bottom() - 2.0));
        let before = self.scroll.offset;
        self.scroll.bar(ui, Id::unique("log-bar"), bar, content, view, p);
        if was_bottom && self.scroll.offset == before || self.jump_end {
            self.scroll.offset = Scroll::max(content, view);
            self.jump_end = false;
        }
        self.scroll.clamp(content, view);
        self.stick = self.scroll.is_at_bottom(content, view);

        let painter = ui.painter().with_clip_rect(text_rect.intersect(ui.clip_rect()));
        let hover_ok = ui::hover_in(ui, text_rect);
        let mut clicked = None;

        // Mouse selection: drag to select, double-click for a word, Ctrl+C copies.
        let resp = ui.interact(text_rect, Id::unique("log-select"), Sense::click_and_drag());
        if resp.drag_started() && hover_ok {
            if let Some((i, k)) = resp.interact_pointer_pos().and_then(|pos| self.hit(pos, text_rect, &heights, f, cw)) {
                let at = (self.lines[i].uid, k);
                self.sel = Some((at, at));
                let ts = resp.interact_pointer_pos().is_some_and(|pos| Self::over_ts(pos, text_rect, cw));
                self.sel_ts = (ts, ts);
            }
        } else if resp.dragged() {
            if let (Some(pos), Some((anchor, _))) = (resp.interact_pointer_pos(), self.sel) {
                if let Some((i, k)) = self.hit(pos, text_rect, &heights, f, cw) {
                    self.sel = Some((anchor, (self.lines[i].uid, k)));
                    self.sel_ts.1 = Self::over_ts(pos, text_rect, cw);
                }
                // dragging past the edge scrolls
                let step = if pos.y < text_rect.top() { -f.mono.m.linespace } else if pos.y > text_rect.bottom() { f.mono.m.linespace } else { 0.0 };
                if step != 0.0 {
                    self.scroll.offset += step;
                    self.stick = false;
                    ui.ctx().request_repaint();
                }
            }
        } else if resp.double_clicked() {
            if let Some((i, k)) = resp.interact_pointer_pos().and_then(|pos| self.hit(pos, text_rect, &heights, f, cw)) {
                let (a, b) = ui::word_span(&self.lines[i].text, k);
                let uid = self.lines[i].uid;
                self.sel = if a < b { Some(((uid, a), (uid, b - 1))) } else { None };
                self.sel_ts = (false, false);
            }
        } else if resp.clicked() {
            self.sel = None;
        }
        let range = self.sel_range();
        if self.sel.is_some() && range.is_none() {
            self.sel = None; // a selected line has scrolled out of the log's memory
        }
        if range.is_some() && ui.input(|i| i.events.iter().any(|e| matches!(e, Event::Copy))) {
            let text = self.selected_text(&heights);
            ui.ctx().copy_text(text);
        }

        let mut y = text_rect.top() + 4.0 - self.scroll.offset;
        for (i, l) in self.lines.iter().enumerate() {
            let h = heights[i];
            let top = y;
            y += h;
            if h == 0.0 || y < text_rect.top() || top > text_rect.bottom() {
                continue;
            }
            let (_, rows, h1) = l.laid.as_ref().unwrap();
            let line_top = top + 2.0;
            let font = if l.tag == Tag::Call { &f.mono_bold } else { &f.mono };
            let color = p.log(log_tag(l.tag));
            let text_top = line_top + ((h1 - f.mono.m.linespace) / 2.0).floor();
            let left = text_rect.left() + 12.0;
            // the selected span of this line's text (char range), and whether the
            // timestamp is in it
            let (sel_s, sel_e, ts_sel) = match range {
                Some(r @ ((si, sk, _), (ei, ek, _))) if i >= si && i <= ei => {
                    let len = l.text.chars().count();
                    let s = if i == si { sk } else { 0 };
                    (s, if i == ei { ek } else { len }, Self::ts_selected(i, r))
                }
                _ => (0, 0, false),
            };
            let ts_bg = if ts_sel { p.select } else { p.surface };
            if ts_sel {
                painter.rect_filled(Rect::from_min_size(Pos2::new(left, text_top), Vec2::new(l.ts.chars().count() as f32 * cw, f.mono.m.linespace)), 0.0, ts_bg);
            }
            ui::text_on(ui, &painter, Pos2::new(left, text_top), &l.ts, &f.mono, p.log(LogTag::Ts), ts_bg);
            let mut x = left + 10.0 * cw;
            for b in l.buttons.iter().filter(|b| !is_trailing(b)) {
                let w = measure_btn(f, b.label);
                let r = Rect::from_min_size(Pos2::new(x, line_top + ((h1 - btn_h(f)) / 2.0).floor()), Vec2::new(w, btn_h(f)));
                paint_button(ui, &painter, f, p, tips, hover_ok, text_rect, b, r, &mut clicked);
                x += w + cw;
            }
            if l.new {
                let w = measure_btn(f, "NEW");
                let r = Rect::from_min_size(Pos2::new(x, line_top + ((h1 - btn_h(f)) / 2.0).floor()), Vec2::new(w, btn_h(f)));
                Rnd::new("NEW", &f.ui_bold).pill().pad(9.0, 1.0).colors(p.new, p.new_text).paint(ui, &painter, r, false);
                if hover_ok {
                    let resp = ui.interact(r.intersect(text_rect), Id::unique(("lognew", i, l.ts.as_str())), eframe::egui::Sense::hover());
                    tips.tip(&resp, "You've never hunted this park", 300, TipAt::Below(r));
                }
                x += w + cw;
            }
            let chars: Vec<char> = l.text.chars().collect();
            for (ri, &(a, b)) in rows.iter().enumerate() {
                let (rx, ry) = if ri == 0 { (x, text_top) } else { (left, line_top + h1 + (ri as f32 - 1.0) * f.mono.m.linespace) };
                // unselected / selected / unselected pieces of the row
                let m1 = sel_s.clamp(a, b);
                let m2 = sel_e.clamp(a, b).max(m1);
                for (from, to, selected) in [(a, m1, false), (m1, m2, true), (m2, b, false)] {
                    if from >= to {
                        continue;
                    }
                    let s: String = chars[from..to].iter().collect();
                    let px = rx + (from - a) as f32 * cw;
                    let bg = if selected { p.select } else { p.surface };
                    if selected {
                        painter.rect_filled(Rect::from_min_size(Pos2::new(px, ry), Vec2::new((to - from) as f32 * cw, f.mono.m.linespace)), 0.0, bg);
                    }
                    let trimmed = s.trim_end_matches(' ');
                    if !trimmed.is_empty() {
                        ui::text_on(ui, &painter, Pos2::new(px, ry), trimmed, font, color, bg);
                    }
                }
            }
            // buttons that follow the text (Keep calling), after the end of the last row
            if l.buttons.iter().any(is_trailing) {
                let n = rows.len().saturating_sub(1);
                let (a, b) = rows.get(n).copied().unwrap_or((0, 0));
                let (rx, ry) = if n == 0 { (x, text_top) } else { (left, line_top + h1 + (n as f32 - 1.0) * f.mono.m.linespace) };
                let shown: String = chars[a..b].iter().collect();
                let mut tx = rx + shown.trim_end_matches(' ').chars().count() as f32 * cw + cw;
                let by = if n == 0 { line_top + ((h1 - btn_h(f)) / 2.0).floor() } else { ry - 1.0 };
                for b in l.buttons.iter().filter(|b| is_trailing(b)) {
                    let w = measure_btn(f, b.label);
                    let r = Rect::from_min_size(Pos2::new(tx, by), Vec2::new(w, btn_h(f)));
                    paint_button(ui, &painter, f, p, tips, hover_ok, text_rect, b, r, &mut clicked);
                    tx += w + cw;
                }
            }
        }
        clicked
    }
}

/// Buttons drawn after the line's text instead of before it.
fn is_trailing(b: &LogButton) -> bool {
    b.label == "Keep calling"
}

#[allow(clippy::too_many_arguments)]
fn paint_button(ui: &Ui, painter: &eframe::egui::Painter, f: &Fonts, p: &Palette, tips: &mut Tips, hover_ok: bool, text_rect: Rect, b: &LogButton, r: Rect, clicked: &mut Option<LogClick>) {
    let (fill, fg, hover) = if b.used {
        (p.disabled_btn.0, p.disabled_btn.1, None)
    } else if b.glowing {
        (b.glow_fill, p.text, Some(b.glow_fill))
    } else if b.label == "Keep calling" {
        (p.on, Color32::WHITE, Some(p.on_hover))
    } else {
        (p.accent, p.accent_text, Some(p.accent_hover))
    };
    let mut rnd = Rnd::new(b.label, &f.ui_bold).pill().pad(9.0, 1.0).colors(fill, fg);
    if let Some(h) = hover {
        rnd = rnd.hover(h);
    }
    let visible = r.intersect(text_rect);
    if visible.height() > 0.0 && hover_ok {
        let resp = ui.interact(visible, Id::unique(("logbtn", b.id)), eframe::egui::Sense::click());
        let hovered = !b.used && resp.hovered();
        if hovered {
            ui.ctx().set_cursor_icon(eframe::egui::CursorIcon::PointingHand);
        }
        rnd.paint(ui, painter, r, hovered);
        tips.tip(&resp, action_tip(b.label), 300, TipAt::Below(r));
        if resp.clicked() && !b.used {
            *clicked = Some(LogClick::Button(b.id));
        }
    } else {
        rnd.paint(ui, painter, r, false);
    }
}

pub fn btn_h(f: &Fonts) -> f32 {
    f.ui_bold.m.linespace + 2.0
}

pub fn measure_btn(f: &Fonts, label: &str) -> f32 {
    measure(&f.ui_bold, label) + 18.0
}
