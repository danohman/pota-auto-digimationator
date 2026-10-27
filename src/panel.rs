// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The Settings popover under the gear button: tabs of switches, steppers and segmented
//! choices, plus the WSJT-X setup, ACLog and pota.app account sub-pages.

use chrono::Datelike;
use eframe::egui::{self, Color32, CursorIcon, Id, Key, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::app::{App, ATTEMPT_MODES};
use crate::respot;
use crate::sound::Kind;
use crate::theme::{self, Font, Fonts, Palette};
use crate::ui::{self, measure, Rnd, TipAt};

pub const NO_SNR_LIMIT: i32 = -999;
/// what our automatic CQ says before call and grid
pub const CQ_CHOICES: [&str; 3] = ["CQ", "CQ POTA", "CQ DX"];
pub const WIDTH: f32 = 440.0;
/// first year of the copyright notice on the About tab
const FIRST_YEAR: i32 = 2026;
const GPL_URL: &str = "https://www.gnu.org/licenses/gpl-3.0.html";

/// Text styles of the About tab's selectable lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AFont {
    Ui,
    UiBold,
    Small,
    SmallMuted,
    SmallLink,
}

impl AFont {
    fn resolve<'a>(self, f: &'a Fonts, p: &Palette) -> (&'a Font, Color32) {
        match self {
            AFont::Ui => (&f.ui, p.text),
            AFont::UiBold => (&f.ui_bold, p.text),
            AFont::Small => (&f.small, p.text),
            AFont::SmallMuted => (&f.small, p.muted),
            AFont::SmallLink => (&f.small, p.link),
        }
    }
}

/// One wrapped line of the About tab as drawn last frame, for selecting and clicking.
struct AboutLine {
    x: f32,
    y: f32,
    text: String,
    font: AFont,
    /// lines of one paragraph share this (copying rejoins them with a space)
    para: usize,
    url: Option<&'static str>,
}
const BD: f32 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Tab {
    #[default]
    Hunting,
    Radio,
    Logging,
    Connect,
    Appearance,
    About,
}

pub const TABS: [(Tab, &str); 6] = [
    (Tab::Hunting, "Hunting"),
    (Tab::Radio, "Radio"),
    (Tab::Logging, "Logging"),
    (Tab::Connect, "Connections"),
    (Tab::Appearance, "Appearance"),
    (Tab::About, "About"),
];

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Page {
    #[default]
    Main,
    Aclog,
    Wsjtx,
    Pota,
    Respot,
}

/// Loggers a QSO can also be sent to: (key, name, one-line description, implemented).
pub const LOGGERS: [(&str, &str, &str, bool); 2] = [
    ("aclog", "ACLog", "N3FJP ACLog over its TCP API", true),
    ("ham2k", "Ham2K Power Logger", "Not available yet", false),
];

#[derive(Default)]
pub struct Panel {
    open: bool,
    pub page: Page,
    pub tab: Tab,
    closed_at: f64,
    opened_at: f64,
    // edit buffers for the sub-pages
    group: String,
    port: String,
    host: String,
    aport: String,
    user: String,
    pass: String,
    respot_msg: String,
    error: String,
    focus_first: bool,
    /// the About tab's lines as last drawn, and the selection (line, char) anchor and focus
    about_lines: Vec<AboutLine>,
    about_sel: Option<((usize, usize), (usize, usize))>,
    /// opened by the POTA_DEMO test hook: don't close when the window isn't focused
    demo: bool,
}

/// What a click in the panel asks for.
enum Do {
    Tab(Tab),
    Page(Page),
    Close,
    Back,
    Changed,
    Theme(&'static str),
    FontSize(u32),
    Test(Kind),
    Choose(Kind),
    ResetSound(Kind),
    ApplyUdp,
    ResetUdp,
    SaveAclog,
    SavePota,
    SaveRespot,
}

/// A vertical stack of rows, drawn or only measured.
struct L<'a> {
    ui: &'a mut Ui,
    paint: bool,
    left: f32,
    right: f32,
    y: f32,
    p: &'a Palette,
    f: &'a Fonts,
    sections: usize,
    /// the widest row asked for (for the panel's width)
    need_w: f32,
    act: Vec<Do>,
}

impl<'a> L<'a> {
    fn painter(&self) -> egui::Painter {
        self.ui.painter().clone()
    }

    fn text(&self, x: f32, y: f32, s: &str, f: &Font, c: Color32) -> f32 {
        if self.paint {
            ui::text(self.ui, &self.ui.painter().clone(), Pos2::new(x, y), s, f, c)
        } else {
            measure(f, s)
        }
    }

    fn need(&mut self, w: f32) {
        self.need_w = self.need_w.max(w);
    }

    /// A Rounded button at `rect`; true when clicked.
    fn button(&mut self, r: &Rnd, rect: Rect, id: Id, click: bool) -> bool {
        if !self.paint {
            return false;
        }
        r.show(self.ui, id, rect, click).clicked()
    }

    fn icon_button(&mut self, x: f32, y: f32, icon: &str, fallback: &str, id: &str) -> (Rect, bool) {
        let (p, f) = (self.p, self.f);
        let (txt, font) = if f.has_icons { (icon, &f.icon_12) } else { (fallback, &f.ui_bold) };
        let r = Rnd::new(txt, font).pill().pad(8.0, 6.0).colors(p.surface, p.muted).hover(p.neutral).icon();
        let sz = r.size(self.ui);
        let rect = Rect::from_min_size(Pos2::new(x, y), sz);
        let clicked = self.button(&r, rect, Id::unique(("panel-icon", id)), true);
        (rect, clicked)
    }

    /// Title row with a close button; sub-pages get a back arrow before the title.
    fn header(&mut self, title: &str, back: bool) {
        self.y += 16.0;
        let h = 28.0f32.max(self.f.heading.m.linespace + 2.0);
        let mut x = self.left;
        if back {
            let (r, clicked) = self.icon_button(x, self.y + ((h - 28.0) / 2.0).floor(), theme::ICON_BACK, "<", "back");
            if clicked {
                self.act.push(Do::Back);
            }
            x = r.right() + 6.0;
        }
        let th = self.f.heading.m.linespace + 2.0;
        let w = self.text(x, self.y + ((h - th) / 2.0).floor() + 1.0, title, &self.f.heading, self.p.text);
        let (_, clicked) = self.icon_button(self.right - 28.0, self.y + ((h - 28.0) / 2.0).floor(), theme::ICON_CLOSE, "×", "close");
        if clicked {
            self.act.push(Do::Close);
        }
        self.need(x - self.left + w + 28.0 + 40.0);
        self.y += h + 6.0;
    }

    fn tab_strip(&mut self, current: Tab) {
        let (p, f) = (self.p, self.f);
        let mut x = self.left;
        let cell_h = f.ui.m.linespace + 12.0;
        for (tab, name) in TABS {
            let on = tab == current;
            let font = if on { &f.ui_bold } else { &f.ui };
            let w = measure(font, name) + 16.0;
            let rect = Rect::from_min_size(Pos2::new(x, self.y), Vec2::new(w, cell_h + 2.0));
            if self.paint {
                let resp = self.ui.interact(rect, Id::unique(("tab", name)), Sense::click());
                if resp.hovered() {
                    self.ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                let fg = if on || resp.hovered() { p.text } else { p.muted };
                self.text(x + 8.0, self.y + 6.0, name, font, fg);
                if on {
                    self.painter().rect_filled(Rect::from_min_size(Pos2::new(x, self.y + cell_h), Vec2::new(w, 2.0)), 0.0, p.accent);
                }
                if resp.clicked() {
                    self.act.push(Do::Tab(tab));
                }
            }
            x += w + 4.0;
        }
        self.need(x - self.left + 40.0);
        self.y += cell_h + 2.0;
        self.rule();
    }

    fn rule(&mut self) {
        if self.paint {
            self.painter().rect_filled(Rect::from_min_size(Pos2::new(self.left, self.y), Vec2::new(self.right - self.left, 1.0)), 0.0, self.p.border);
        }
        self.y += 1.0;
    }

    fn section(&mut self, title: &str) {
        if self.sections > 0 {
            // a rule above every section but the first
            self.y += 10.0;
            self.rule();
        }
        self.sections += 1;
        self.y += 12.0;
        self.text(self.left, self.y + 1.0, &title.to_uppercase(), &self.f.small_bold, self.p.muted);
        self.y += self.f.small_bold.m.linespace + 2.0 + 2.0;
    }

    /// A row with a title and a muted subtitle on the left; `right_w`/`right_h` is the
    /// space the controls on the right need. Returns the row rect.
    fn row(&mut self, title: &str, subtitle: &str, right_w: f32, right_h: f32, title_fg: Color32) -> Rect {
        let f = self.f;
        self.y += 8.0;
        let th = f.ui.m.linespace + 2.0 + 2.0 + f.small.m.linespace + 2.0;
        let h = th.max(right_h);
        let rect = Rect::from_min_size(Pos2::new(self.left, self.y), Vec2::new(self.right - self.left, h));
        let ty = self.y + ((h - th) / 2.0).floor();
        let w1 = self.text(self.left, ty + 1.0, title, &f.ui, title_fg);
        let w2 = self.text(self.left, ty + f.ui.m.linespace + 2.0 + 2.0 + 1.0, subtitle, &f.small, self.p.muted);
        self.need(w1.max(w2) + right_w + 40.0);
        self.y += h + 8.0;
        rect
    }

    fn switch(&mut self, rect: Rect, id: &str, on: bool, enabled: bool) -> Rect {
        let (p, ui) = (self.p, &self.ui);
        let r = Rect::from_min_size(Pos2::new(rect.right() - 40.0, rect.top() + ((rect.height() - 22.0) / 2.0).floor()), Vec2::new(40.0, 22.0));
        if self.paint {
            let pos = ui.ctx().animate_bool_with_time(Id::unique(("switch", id)), on, 0.064);
            let (mut on_c, mut off_c, mut knob) = (p.accent, p.faint, Color32::WHITE);
            if !enabled {
                on_c = theme::mix(p.surface, p.faint, 0.35);
                off_c = on_c; // a faded, inert switch
                knob = theme::mix(p.surface, Color32::WHITE, 0.6);
            }
            let painter = ui.painter();
            painter.rect_filled(r, ui::radius(11.0), theme::mix(off_c, on_c, pos));
            let d = 22.0 - 6.0;
            let x = (3.0 + pos * (40.0 - 6.0 - d)).round();
            painter.circle_filled(Pos2::new(r.left() + x + d / 2.0, r.top() + 3.0 + d / 2.0), d / 2.0, knob);
        }
        r
    }

    /// A switch row: the whole row toggles. Returns true when toggled.
    fn switch_row(&mut self, title: &str, subtitle: &str, value: &mut bool, enabled: bool, extra_w: f32) -> (Rect, bool) {
        let fg = if enabled { self.p.text } else { self.p.muted };
        let rect = self.row(title, subtitle, 40.0 + 16.0 + extra_w, 22.0, fg);
        let id = format!("{title}{subtitle}");
        self.switch(rect, &id, *value, enabled);
        let mut toggled = false;
        if self.paint && enabled {
            let resp = self.ui.interact(rect, Id::unique(("switch-row", title)), Sense::click());
            if resp.hovered() {
                self.ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                *value = !*value;
                toggled = true;
                self.act.push(Do::Changed);
            }
        }
        (rect, toggled)
    }

    fn nav_row(&mut self, title: &str, subtitle: &str, page: Page) {
        let (p, f) = (self.p, self.f);
        let (glyph, font) = if f.has_icons { (theme::ICON_CHEVRON, &f.icon_12) } else { (">", &f.ui) };
        let lw = measure(font, glyph) + 2.0 * (1.0 + BD);
        let lh = f.small.m.linespace + 2.0 * (1.0 + BD);
        let rect = self.row(title, subtitle, lw + 20.0, lh, p.text);
        if self.paint {
            let x = rect.right() - 4.0 - lw + 1.0 + BD;
            let y = rect.top() + ((rect.height() - lh) / 2.0).floor() + 1.0 + BD;
            self.text(x, y, glyph, font, p.muted);
            let resp = self.ui.interact(rect, Id::unique(("nav", title)), Sense::click());
            if resp.hovered() {
                self.ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                self.act.push(Do::Page(page));
            }
        }
    }

    fn stepper_width(&self, labels: &[String]) -> f32 {
        let f = self.f;
        let width = measure(&f.ui, "No limit") + 24.0;
        let inner = labels.iter().map(|l| measure(&f.ui, l) + 16.0).fold(width, f32::max);
        let btn = Rnd::new("+", &f.ui).pill().pad(9.0, 2.0).min_text("+").size(self.ui).x;
        2.0 * btn + inner + 2.0 * BD
    }

    /// [−] value [+] over `labels`; returns the new index when stepped.
    fn stepper_row(&mut self, title: &str, subtitle: &str, labels: &[String], idx: usize) -> Option<usize> {
        let (p, f) = (self.p, self.f);
        let total = self.stepper_width(labels);
        let lh = f.ui.m.linespace + 2.0 * (1.0 + BD);
        let rect = self.row(title, subtitle, total + 16.0, lh, p.text);
        if !self.paint {
            return None;
        }
        let x0 = rect.right() - total;
        let top = rect.top() + ((rect.height() - lh) / 2.0).floor();
        let mut out = None;
        let width = measure(&f.ui, "No limit") + 24.0;
        let inner = labels.iter().map(|l| measure(&f.ui, l) + 16.0).fold(width, f32::max);
        let mk = |t: &'static str, ok: bool| {
            let r = Rnd::new(t, &f.ui).pill().pad(9.0, 2.0).min_text("+");
            if ok { r.colors(p.neutral, p.neutral_text).hover(p.neutral_hover) } else { r.colors(p.surface2, p.faint) }
        };
        let minus = mk("−", idx > 0);
        let sz = minus.size(self.ui);
        let mr = Rect::from_min_size(Pos2::new(x0, top + ((lh - sz.y) / 2.0).floor()), sz);
        if self.button(&minus, mr, Id::unique(("step-", title)), idx > 0) {
            out = Some(idx - 1);
        }
        let text = &labels[idx];
        let tw = measure(&f.ui, text);
        let spare = ((inner - tw) / 2.0).floor().max(0.0);
        self.text(mr.right() + BD + spare, top + BD + 1.0, text, &f.ui, p.text);
        let px = mr.right() + 2.0 * BD + 2.0 * spare + tw;
        let plus = mk("+", idx + 1 < labels.len());
        let pr = Rect::from_min_size(Pos2::new(px, mr.top()), sz);
        if self.button(&plus, pr, Id::unique(("step+", title)), idx + 1 < labels.len()) {
            out = Some(idx + 1);
        }
        out
    }

    fn segmented_size(&self, choices: &[(&str, &str)]) -> Vec2 {
        let f = self.f;
        let w: f32 = choices.iter().map(|(_, t)| Rnd::new(t, &f.ui).pill().pad(12.0, 4.0).size(self.ui).x + 6.0).sum();
        Vec2::new(w, f.ui.m.linespace + 8.0)
    }

    /// Pill choices starting at `x`; returns the value picked.
    fn segmented(&mut self, x: f32, y: f32, id: &str, choices: &[(&'static str, &str)], current: &str) -> Option<&'static str> {
        let (p, f) = (self.p, self.f);
        let mut x = x;
        let mut out = None;
        for (value, label) in choices {
            let on = *value == current;
            let r = Rnd::new(label, &f.ui).pill().pad(12.0, 4.0);
            let r = if on { r.colors(p.accent, p.accent_text).hover(p.accent_hover) } else { r.colors(p.neutral, p.neutral_text).hover(p.neutral_hover) };
            let sz = r.size(self.ui);
            let rect = Rect::from_min_size(Pos2::new(x, y), sz);
            if self.button(&r, rect, Id::unique(("seg", id, *value)), true) {
                out = Some(*value);
            }
            x += sz.x + 6.0;
        }
        out
    }

    fn segmented_row(&mut self, title: &str, subtitle: &str, id: &str, choices: &[(&'static str, &str)], current: &str) -> Option<&'static str> {
        let sz = self.segmented_size(choices);
        let rect = self.row(title, subtitle, sz.x, sz.y, self.p.text);
        let y = rect.top() + ((rect.height() - sz.y) / 2.0).floor();
        let x = rect.right() - sz.x;
        if self.paint {
            self.segmented(x, y, id, choices, current)
        } else {
            None
        }
    }

    fn small_button(&self, text: &'a str) -> Rnd<'a> {
        let p = self.p;
        Rnd::new(text, &self.f.small_bold).pill().pad(12.0, 3.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover)
    }

    /// A ttk.Entry look-alike: 29 px tall, 1 px border, accent when focused.
    fn entry(&mut self, rect: Rect, id: &str, buf: &mut String, password: bool, focus: bool) -> bool {
        if !self.paint {
            return false;
        }
        let (p, f) = (self.p, self.f);
        let eid = Id::unique(("entry", id));
        let focused = self.ui.ctx().memory(|m| m.has_focus(eid));
        let painter = self.painter();
        painter.rect_filled(rect, 0.0, p.surface2);
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, if focused { p.accent } else { p.border }), StrokeKind::Inside);
        let shift = f.ui.m.ascent - ui::egui_baseline(self.ui, &f.ui);
        let inner = Rect::from_min_size(Pos2::new(rect.left() + 8.0, rect.top() + 6.0 + shift), Vec2::new(rect.width() - 16.0, f.ui.m.linespace));
        let te = egui::TextEdit::singleline(buf)
            .id(eid)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .font(f.ui.id.clone())
            .text_color(p.text)
            .password(password)
            .desired_width(inner.width());
        let resp = self.ui.put(inner, te);
        if focus {
            resp.request_focus();
        }
        resp.changed()
    }

    /// Label + entry form rows (grid: labels, then entries with 16 px gap).
    fn form(&mut self, rows: &mut [(&str, &mut String, f32, bool)], pady: f32, focus_first: bool) {
        let f = self.f;
        let lw = rows.iter().map(|r| measure(&f.ui, r.0) + 2.0).fold(0.0, f32::max);
        let eh = f.ui.m.linespace + 12.0;
        let n = rows.len();
        for (i, (label, buf, chars, password)) in rows.iter_mut().enumerate() {
            self.y += pady;
            let ly = self.y + ((eh - (f.ui.m.linespace + 2.0)) / 2.0).floor() + 1.0;
            self.text(self.left + 1.0, ly, label, &f.ui, self.p.text);
            let natural = *chars * measure(&f.ui, "0") + 16.0;
            let x = self.left + lw + 16.0;
            let w = if i == 0 || n == 2 && *chars > 10.0 { (self.right - x).max(natural) } else { natural };
            self.need(lw + 16.0 + natural + 40.0);
            let rect = Rect::from_min_size(Pos2::new(x, self.y), Vec2::new(w, eh));
            let id = format!("{label}");
            self.entry(rect, &id, buf, *password, focus_first && i == 0);
            self.y += eh + pady;
        }
    }

    fn paragraph(&mut self, s: &str, f: &Font, c: Color32, wrap: f32, padtop: f32, padbottom: f32) {
        self.y += padtop;
        let painter = self.painter();
        let h = ui::paragraph(self.ui, if self.paint { Some(&painter) } else { None }, Pos2::new(self.left + 1.0, self.y + 1.0), s, f, c, wrap);
        self.y += h + 2.0 + padbottom;
    }

    /// Save / Cancel style buttons packed from the right.
    fn buttons_right(&mut self, buttons: &[(&str, bool, &str)], padtop: f32) -> Option<String> {
        let (p, f) = (self.p, self.f);
        self.y += padtop;
        let mut x = self.right;
        let mut h: f32 = 0.0;
        let mut out = None;
        for (label, primary, size_text) in buttons {
            let r = Rnd::new(label, &f.ui_bold).pill().pad(16.0, 6.0).min_text(size_text);
            let r = if *primary { r.colors(p.accent, p.accent_text).hover(p.accent_hover) } else { r.colors(p.neutral, p.neutral_text).hover(p.neutral_hover) };
            let sz = r.size(self.ui);
            let rect = Rect::from_min_size(Pos2::new(x - sz.x, self.y), sz);
            if self.button(&r, rect, Id::unique(("pbtn", *label)), true) {
                out = Some(label.to_string());
            }
            x = rect.left() - 8.0;
            h = h.max(sz.y);
        }
        self.y += h;
        out
    }
}

fn stepper_labels_attempts(current: u32) -> (Vec<u32>, Vec<String>) {
    let mut v: Vec<u32> = (1..=30).collect();
    if !v.contains(&current) {
        v.push(current);
        v.sort();
    }
    let labels = v.iter().map(|n| format!("{n} call{}", if *n != 1 { "s" } else { "" })).collect();
    (v, labels)
}

/// "Leave as is" (0) then 5 to 100 W in 1 W steps.
fn stepper_labels_power() -> (Vec<u32>, Vec<String>) {
    let v: Vec<u32> = std::iter::once(0).chain(5..=100).collect();
    let labels = v.iter().map(|w| if *w == 0 { "Leave".to_string() } else { format!("{w} W") }).collect();
    (v, labels)
}

fn stepper_labels_snr(current: i32) -> (Vec<i32>, Vec<String>) {
    let mut v: Vec<i32> = (-24..=-6).step_by(2).collect();
    if current != NO_SNR_LIMIT && !v.contains(&current) {
        v.push(current);
    }
    v.sort();
    v.insert(0, NO_SNR_LIMIT);
    let labels = v.iter().map(|db| if *db == NO_SNR_LIMIT { "No limit".to_string() } else { format!("{db:+} dB") }).collect();
    (v, labels)
}

impl Panel {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self, now: f64) {
        if self.open {
            self.close(now);
        } else if now - self.closed_at > 0.25 {
            // (a click that just closed it doesn't reopen it)
            self.open = true;
            self.page = Page::Main;
            self.opened_at = now;
        }
    }

    /// Drop the About tab's text selection.
    pub fn clear_selection(&mut self) {
        self.about_sel = None;
    }

    pub fn close(&mut self, now: f64) {
        if self.open {
            self.open = false;
            self.demo = false;
            self.page = Page::Main;
            self.about_sel = None;
            self.closed_at = now;
        }
    }

    /// Open on a tab or sub-page, as a link from the main window does.
    pub fn open_at(&mut self, app: &App, now: f64, tab: Tab, page: Page) {
        self.open = true;
        self.opened_at = now;
        self.tab = tab;
        self.show_page(app, page);
    }

    /// Open on a tab or sub-page (the POTA_DEMO test hook).
    pub fn open_on(&mut self, app: &App, now: f64, tab: Tab, page: Page) {
        self.open = true;
        self.demo = true;
        self.opened_at = now;
        self.tab = tab;
        self.show_page(app, page);
    }

    fn show_page(&mut self, app: &App, page: Page) {
        self.page = page;
        self.error.clear();
        self.focus_first = true;
        match page {
            Page::Wsjtx => {
                self.group = app.s.udp_group.clone();
                self.port = app.s.udp_port.to_string();
            }
            Page::Aclog => {
                self.host = app.s.aclog_host.clone();
                self.aport = app.s.aclog_port.to_string();
            }
            Page::Pota => {
                self.user = app.s.pota_user.clone();
                self.pass = app.s.pota_password.clone();
            }
            Page::Respot => self.respot_msg = app.s.respot_message.clone(),
            Page::Main => {}
        }
    }

    /// Lay out (and with `paint`, draw) the current page; returns the stack.
    fn build<'a>(&mut self, app: &mut App, ui: &'a mut Ui, rect: Rect, paint: bool, tab: Tab) -> (f32, f32, Vec<Do>) {
        let (p, f) = (app.p.clone(), app.f.clone());
        let mut l = L { ui, paint, left: rect.left() + 20.0, right: rect.right() - 20.0, y: rect.top(), p: &p, f: &f, sections: 0, need_w: WIDTH, act: vec![] };
        match self.page {
            Page::Main => {
                l.header("Settings", false);
                l.tab_strip(tab);
                match tab {
                    Tab::Hunting => self.hunting_tab(app, &mut l),
                    Tab::Radio => self.radio_tab(app, &mut l),
                    Tab::Logging => self.logging_tab(app, &mut l),
                    Tab::Connect => self.connect_tab(app, &mut l),
                    Tab::Appearance => {
                        l.section("Theme");
                        l.y += 2.0;
                        let choices = [("system", "Match Windows"), ("light", "Light"), ("dark", "Dark")];
                        let sz = l.segmented_size(&choices);
                        l.need(sz.x + 40.0);
                        if paint {
                            if let Some(v) = l.segmented(l.left, l.y, "theme", &choices, &app.s.theme.clone()) {
                                l.act.push(Do::Theme(v));
                            }
                        }
                        l.y += sz.y + 8.0;
                        l.section("Text size");
                        l.y += 2.0;
                        let sizes: Vec<u32> = (8..=16).collect();
                        let labels: Vec<String> = sizes.iter().map(|n| format!("{n} pt")).collect();
                        let idx = sizes.iter().position(|n| *n == app.s.font_size).unwrap_or(2);
                        if let Some(i) = l.stepper_row("Font size", "Changes everywhere at once; 10 pt is the default", &labels, idx) {
                            l.act.push(Do::FontSize(sizes[i]));
                        }
                    }
                    Tab::About => self.about_tab(app, &mut l),
                }
            }
            Page::Wsjtx => self.wsjtx_page(app, &mut l),
            Page::Aclog => self.aclog_page(&mut l),
            Page::Pota => self.pota_page(app, &mut l),
            Page::Respot => self.respot_page(&mut l),
        }
        (l.need_w, l.y - rect.top(), l.act)
    }

    fn about_tab(&mut self, app: &mut App, l: &mut L) {
        let wrap = WIDTH - 40.0;
        // last frame's lines drive the mouse (the layout doesn't change between frames)
        let prev = if l.paint { std::mem::take(&mut self.about_lines) } else { Vec::new() };
        if l.paint {
            self.about_input(app, l, &prev);
        }
        l.section("About");
        l.y += 6.0;
        self.about_text(l, "POTA Auto-Digimationator", AFont::UiBold, wrap, 0.0, 0.0, None);
        self.about_text(l, &format!("Version {}", env!("CARGO_PKG_VERSION")), AFont::SmallMuted, wrap, 0.0, 0.0, None);
        l.section("Copyright");
        let year = chrono::Local::now().year();
        let years = if year > FIRST_YEAR { format!("{FIRST_YEAR}-{year}") } else { FIRST_YEAR.to_string() };
        self.about_text(l, &format!("Copyright \u{a9} {years} Dan Muntz (WD4DAN)"), AFont::Ui, wrap, 4.0, 4.0, None);
        l.section("License");
        self.about_text(l, "GNU General Public License, version 3 or (at your option) any later version.", AFont::UiBold, wrap, 4.0, 4.0, None);
        self.about_text(
            l,
            "This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.",
            AFont::Small, wrap, 0.0, 4.0, None,
        );
        self.about_text(
            l,
            "This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.",
            AFont::Small, wrap, 0.0, 4.0, None,
        );
        self.about_text(l, "The full text is in the LICENSE file, and online at:", AFont::SmallMuted, wrap, 0.0, 0.0, None);
        self.about_text(l, GPL_URL, AFont::SmallLink, wrap, 0.0, 6.0, Some(GPL_URL));
    }

    /// One selectable paragraph of the About tab: wrapped, recorded for the mouse
    /// handling and drawn with the selection behind it.
    #[allow(clippy::too_many_arguments)]
    fn about_text(&mut self, l: &mut L, s: &str, font: AFont, wrap: f32, padtop: f32, padbottom: f32, url: Option<&'static str>) {
        let (p, f) = (l.p, l.f);
        let (fr, color) = font.resolve(f, p);
        l.y += padtop;
        let para = self.about_lines.last().map_or(0, |x| x.para + 1);
        for line in ui::wrap_lines(l.ui, fr, s, wrap) {
            if l.paint {
                let idx = self.about_lines.len();
                let (a, b) = self.about_sel_in(idx, line.chars().count());
                let at = Pos2::new(l.left + 1.0, l.y + 1.0);
                let painter = l.painter();
                if a < b {
                    let pp = ui::positions(fr, &line);
                    painter.rect_filled(Rect::from_min_max(Pos2::new(at.x + pp[a], at.y), Pos2::new(at.x + pp[b], at.y + fr.m.linespace)), 0.0, p.select);
                }
                ui::text(l.ui, &painter, at, &line, fr, color);
                self.about_lines.push(AboutLine { x: at.x, y: at.y, text: line, font, para, url });
            }
            l.y += fr.m.linespace;
        }
        l.y += 2.0 + padbottom;
    }

    /// The selected character range of recorded line `idx` (of `n` characters).
    fn about_sel_in(&self, idx: usize, n: usize) -> (usize, usize) {
        let Some((a, b)) = self.about_sel else { return (0, 0) };
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if idx < a.0 || idx > b.0 {
            return (0, 0);
        }
        let s = if idx == a.0 { a.1 } else { 0 };
        let e = if idx == b.0 { b.1 } else { n };
        (s.min(n), e.min(n).max(s.min(n)))
    }

    /// Mouse and keyboard for the About text: drag to select, double-click a word, Ctrl+C
    /// to copy, click the link to open it.
    fn about_input(&mut self, app: &mut App, l: &mut L, lines: &[AboutLine]) {
        let (Some(first), Some(last)) = (lines.first(), lines.last()) else { return };
        let (p, f) = (l.p, l.f);
        let lh = |ln: &AboutLine| ln.font.resolve(f, p).0.m.linespace;
        let area = Rect::from_min_max(Pos2::new(l.left, first.y - 2.0), Pos2::new(l.right, last.y + lh(last) + 2.0));
        let hit = |pos: Pos2| -> (usize, usize) {
            let i = lines.iter().position(|ln| pos.y < ln.y + lh(ln)).unwrap_or(lines.len() - 1);
            let fr = lines[i].font.resolve(f, p).0;
            let pp = ui::positions(fr, &lines[i].text);
            let n = lines[i].text.chars().count().min(pp.len().saturating_sub(1));
            let rel = pos.x - lines[i].x;
            let k = (0..=n).min_by(|&a, &b| (pp[a] - rel).abs().total_cmp(&(pp[b] - rel).abs())).unwrap_or(0);
            (i, k)
        };
        // the link's own text, not the empty space after it
        let link_at = |pos: Pos2| -> Option<&'static str> {
            let ln = &lines[hit(pos).0];
            let fr = ln.font.resolve(f, p).0;
            (pos.x >= ln.x && pos.x <= ln.x + measure(fr, &ln.text) && pos.y >= ln.y && pos.y < ln.y + fr.m.linespace).then_some(ln.url).flatten()
        };
        let resp = l.ui.interact(area, Id::new("about-select"), Sense::click_and_drag());
        if resp.hovered() {
            if let Some(pos) = l.ui.ctx().pointer_hover_pos() {
                l.ui.ctx().set_cursor_icon(if link_at(pos).is_some() { CursorIcon::PointingHand } else { CursorIcon::Text });
            }
        }
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let a = hit(pos);
                self.about_sel = Some((a, a));
                app.clear_text_selections();
            }
        } else if resp.dragged() {
            if let (Some(pos), Some((a, _))) = (resp.interact_pointer_pos(), self.about_sel) {
                self.about_sel = Some((a, hit(pos)));
            }
        } else if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let (i, k) = hit(pos);
                let chars: Vec<char> = lines[i].text.chars().collect();
                let k = k.min(chars.len());
                let (mut a, mut b) = (k, k);
                while a > 0 && chars[a - 1] != ' ' {
                    a -= 1;
                }
                while b < chars.len() && chars[b] != ' ' {
                    b += 1;
                }
                self.about_sel = if a < b { Some(((i, a), (i, b))) } else { None };
                app.clear_text_selections();
            }
        } else if resp.clicked() {
            self.about_sel = None;
            if let Some(url) = resp.interact_pointer_pos().and_then(link_at) {
                l.ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
        if let Some((a, b)) = self.about_sel {
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            if b.0 < lines.len() && l.ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                let mut out = String::new();
                for i in a.0..=b.0 {
                    let chars: Vec<char> = lines[i].text.chars().collect();
                    let s = if i == a.0 { a.1.min(chars.len()) } else { 0 };
                    let e = if i == b.0 { b.1.min(chars.len()) } else { chars.len() };
                    if i > a.0 {
                        // wrapped lines of one paragraph rejoin with a space
                        out.push(if lines[i].para == lines[i - 1].para { ' ' } else { '\n' });
                    }
                    out.extend(&chars[s.min(e)..e]);
                }
                l.ui.ctx().copy_text(out);
            }
        }
    }

    fn hunting_tab(&mut self, app: &mut App, l: &mut L) {
        l.section("Hunting");
        let mut dry = app.s.dry_run;
        l.switch_row("Dry run", "Log what it would call, never transmit", &mut dry, true, 0.0);
        app.s.dry_run = dry;
        for mode in ATTEMPT_MODES {
            let cur = app.s.attempts_by_mode.get(mode).copied().unwrap_or(app.s.max_attempts);
            let (vals, labels) = stepper_labels_attempts(cur);
            let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
            if let Some(i) = l.stepper_row(&format!("Give up after ({mode})"), "Unanswered calls before moving on", &labels, idx) {
                app.s.attempts_by_mode.insert(mode.to_string(), vals[i]);
                l.act.push(Do::Changed);
            }
        }
        let cur = app.s.min_snr.unwrap_or(NO_SNR_LIMIT);
        let (vals, labels) = stepper_labels_snr(cur);
        let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
        if let Some(i) = l.stepper_row("Skip stations weaker than", "Auto-calls only; Answer still works", &labels, idx) {
            app.s.min_snr = if vals[i] == NO_SNR_LIMIT { None } else { Some(vals[i]) };
            l.act.push(Do::Changed);
        }
        let mut v = app.s.new_parks_only;
        l.switch_row("Only hunt new parks", "Skip parks you've hunted before (needs your pota.app login)", &mut v, true, 0.0);
        app.s.new_parks_only = v;
        let mut v = app.s.pounce_signoffs;
        l.switch_row("Call stations after their 73", "Needs WSJT-X to show every decode, not just CQs", &mut v, true, 0.0);
        app.s.pounce_signoffs = v;
        let mut v = app.s.move_tx;
        l.switch_row("Move Tx to their frequency", "Off: only Rx moves, Tx stays where you put it", &mut v, true, 0.0);
        app.s.move_tx = v;

        l.section("Auto CQ/Answer");
        let choices: Vec<(&'static str, &str)> = CQ_CHOICES.iter().map(|c| (*c, *c)).collect();
        if let Some(v) = l.segmented_row("CQ message", "Sent with your call and grid, e.g. CQ POTA WD4DAN EM80", "cq", &choices, &app.s.cq_text.clone()) {
            app.s.cq_text = v.to_string();
            l.act.push(Do::Changed);
        }
    }

    fn radio_tab(&mut self, app: &mut App, l: &mut L) {
        l.section("Radio receive (Yaesu, via ACLog)");
        let mut v = app.s.rig_clean;
        l.switch_row("Keep ATT, NB and DNR off", "Sent at start-up and after each band or mode change", &mut v, true, 0.0);
        app.s.rig_clean = v;
        let choices = [("leave", "Leave"), ("off", "Off"), ("on", "On")];
        for (key, name) in [("att", "ATT"), ("nb", "NB"), ("dnr", "DNR")] {
            let cur = match key {
                "att" => app.s.rig_restore_att.clone(),
                "nb" => app.s.rig_restore_nb.clone(),
                _ => app.s.rig_restore_dnr.clone(),
            };
            if let Some(v) = l.segmented_row(&format!("{name} when closing"), "ACLog can't read the radio, so choose the state to leave it in", key, &choices, &cur) {
                match key {
                    "att" => app.s.rig_restore_att = v.into(),
                    "nb" => app.s.rig_restore_nb = v.into(),
                    _ => app.s.rig_restore_dnr = v.into(),
                }
                l.act.push(Do::Changed);
            }
        }
        l.section("Radio Tx power (Yaesu, via ACLog)");
        let (vals, labels) = stepper_labels_power();
        let cur = app.s.rig_power.unwrap_or(0);
        let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
        if let Some(i) = l.stepper_row("Tx power at start-up and QSY", "Set at start-up and after each band or mode change, 5 to 100 W", &labels, idx) {
            app.s.rig_power = (vals[i] != 0).then_some(vals[i]);
            l.act.push(Do::Changed);
        }
        let cur = app.s.rig_power_close.unwrap_or(0);
        let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
        if let Some(i) = l.stepper_row("Tx power when closing", "Set when the app closes, 5 to 100 W", &labels, idx) {
            app.s.rig_power_close = (vals[i] != 0).then_some(vals[i]);
            l.act.push(Do::Changed);
        }
    }

    fn connect_tab(&mut self, app: &mut App, l: &mut L) {
        l.section("WSJT-X");
        let (_, ok) = app.wsjtx_link();
        let (g, port) = app.listen_addr();
        l.nav_row("WSJT-X setup", &format!("{g}:{port}  ·  {}", if ok { "connected" } else { "not receiving" }), Page::Wsjtx);
        l.section("pota.app");
        l.nav_row("pota.app account", &app.pota_summary(), Page::Pota);
        let mut v = app.s.respot;
        l.switch_row("Respot to pota.app after each QSO", "Posts the activator's spot after a logged QSO (needs the login above and a known park)", &mut v, true, 0.0);
        app.s.respot = v;
        let msg = &app.s.respot_message;
        l.nav_row("Respot message", if msg.is_empty() { "None" } else { msg }, Page::Respot);
    }

    /// "Sound file" row with Test / Choose… / Reset buttons for one of the app's sounds.
    fn sound_file_row(l: &mut L, p: &Palette, kind: Kind, custom: &str) {
        let sub = if custom.is_empty() {
            "Default (built in)".to_string()
        } else {
            std::path::Path::new(custom).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(custom.to_string())
        };
        let mut buttons = vec![("Test", Do::Test(kind)), ("Choose…", Do::Choose(kind))];
        if !custom.is_empty() {
            buttons.push(("Reset", Do::ResetSound(kind)));
        }
        let widths: Vec<Vec2> = buttons.iter().map(|(t, _)| l.small_button(t).size(l.ui)).collect();
        let total: f32 = widths.iter().map(|s| s.x + 6.0).sum();
        let bh = widths.iter().map(|s| s.y).fold(0.0, f32::max);
        let rect = l.row("Sound file", &sub, total, bh, p.text);
        if l.paint {
            let mut x = rect.right() - total + 6.0;
            for ((t, d), sz) in buttons.into_iter().zip(widths) {
                let r = Rect::from_min_size(Pos2::new(x, rect.top() + ((rect.height() - sz.y) / 2.0).floor()), sz);
                if l.button(&l.small_button(t), r, Id::unique(("sound", kind.name(), t)), true) {
                    l.act.push(d);
                }
                x += sz.x + 6.0;
            }
        }
    }

    fn logging_tab(&mut self, app: &mut App, l: &mut L) {
        let (p, f) = (l.p, l.f);
        l.section("WSJT-X");
        let mut v = app.s.auto_log;
        l.switch_row("Press OK on WSJT-X's Log QSO window", "Whenever it opens, for any QSO", &mut v, true, 0.0);
        app.s.auto_log = v;
        let mut v = app.s.ding;
        l.switch_row("Play a sound when a QSO is logged", "Plays when WSJT-X logs any QSO", &mut v, true, 0.0);
        app.s.ding = v;
        let custom = app.s.ding_path.clone();
        Self::sound_file_row(l, p, Kind::Ding, &custom);

        let mut v = app.s.pop;
        l.switch_row("Play a sound for a wanted park", "Plays when a wanted park is ready to call (only NEW/ATNO parks when hunting new parks only)", &mut v, true, 0.0);
        app.s.pop = v;
        let custom = app.s.pop_path.clone();
        Self::sound_file_row(l, p, Kind::Pop, &custom);

        l.section("Also log each QSO to");
        for (key, name, blurb, ready) in LOGGERS {
            if key == "aclog" {
                let host = format!("{}:{}", app.s.aclog_host, app.s.aclog_port);
                let edit = l.small_button("Edit");
                let esz = edit.size(l.ui);
                let mut v = app.s.aclog_enabled;
                let (rect, _) = l.switch_row(name, &format!("{blurb}  ·  {host}"), &mut v, true, esz.x + 12.0);
                app.s.aclog_enabled = v;
                if l.paint {
                    let r = Rect::from_min_size(Pos2::new(rect.right() - 40.0 - 16.0 - 12.0 - esz.x, rect.top() + ((rect.height() - esz.y) / 2.0).floor()), esz);
                    if l.button(&edit, r, Id::unique("aclog-edit"), true) {
                        l.act.push(Do::Page(Page::Aclog));
                    }
                }
            } else {
                let mut v = false;
                let soon = Rnd::new("Coming soon", &f.small_bold).pill().pad(10.0, 2.0).colors(p.surface2, p.muted);
                let ssz = soon.size(l.ui);
                let (rect, _) = l.switch_row(name, blurb, &mut v, ready, ssz.x + 12.0);
                if l.paint && !ready {
                    let r = Rect::from_min_size(Pos2::new(rect.right() - 40.0 - 16.0 - 12.0 - ssz.x, rect.top() + ((rect.height() - ssz.y) / 2.0).floor()), ssz);
                    soon.paint(l.ui, &l.painter(), r, false);
                }
            }
        }
    }

    /// Where to listen for WSJT-X, and what to set up in WSJT-X so it works.
    fn wsjtx_page(&mut self, app: &mut App, l: &mut L) {
        let (p, f) = (l.p, l.f);
        l.header("WSJT-X setup", true);
        l.section("Connection");
        let (text, ok) = app.wsjtx_link();
        let chip = Rnd::new(&text, &f.ui).pill().pad(12.0, 5.0).colors(p.surface2, p.text).dot(Some(if ok { p.ok } else { p.bad }));
        let sz = chip.size(l.ui);
        l.y += 2.0;
        if l.paint {
            chip.paint(l.ui, &l.painter(), Rect::from_min_size(Pos2::new(l.left, l.y), sz), false);
        }
        l.need(sz.x + 40.0);
        l.y += sz.y + 6.0;
        self.focus_first = false; // (this page doesn't focus its first box)
        let (mut g, mut port) = (self.group.clone(), self.port.clone());
        l.form(&mut [("Address", &mut g, 24.0, false), ("Port", &mut port, 8.0, false)], 5.0, false);
        self.group = g;
        self.port = port;
        if !self.error.is_empty() || !l.paint {
            let e = self.error.clone();
            let h = f.small.m.linespace + 2.0;
            l.text(l.left + 1.0, l.y + 1.0, &e, &f.small, p.bad);
            l.y += h;
        } else {
            l.y += f.small.m.linespace + 2.0;
        }
        match l.buttons_right(&[("Apply", true, ""), ("Reset to default", false, "")], 4.0).as_deref() {
            Some("Apply") => l.act.push(Do::ApplyUdp),
            Some("Reset to default") => l.act.push(Do::ResetUdp),
            _ => {}
        }
        let (g, port) = app.listen_addr();
        l.section("Set this up in WSJT-X");
        let steps = [
            format!("File > Settings > Reporting > UDP Server: set the address to {g} and the port to {port}. These must match the values above."),
            "Tick Accept UDP requests in the same place. Without it WSJT-X ignores every call this program sends.".into(),
            "Under Outgoing interfaces, tick the loopback adapter (and your network adapter if WSJT-X runs on another PC).".into(),
            "WSJT-X Improved: untick CQ only and Hide FT8 dupes in the Band Activity window. WSJT-X only acts on a call to a line it is showing there, so hidden lines are ignored. With CQ only left on, turn off Call stations after their 73.".into(),
            "Recommended on the main window: Hold Tx Freq (keeps your transmit frequency where you set it) and Auto Seq. In Settings > General, tick Disable Tx after sending 73.".into(),
            "Leave WSJT-X (or WSJT-X Improved) running and monitoring the band. You must be at the station: you stay the control operator.".into(),
        ];
        for (n, s) in steps.iter().enumerate() {
            l.y += 3.0;
            let num_w = 2.0 * measure(&f.small_bold, "0") + 2.0;
            let num_h = f.small_bold.m.linespace + 2.0;
            let wrap = WIDTH - 40.0 - 36.0;
            let painter = l.painter();
            let text_h = ui::paragraph(l.ui, if l.paint { Some(&painter) } else { None }, Pos2::new(l.left + num_w + 10.0, l.y + 1.0), s, &f.small, p.text, wrap) + 2.0;
            if l.paint {
                let r = Rect::from_min_size(Pos2::new(l.left, l.y), Vec2::new(num_w, num_h));
                painter.rect_filled(r, 0.0, p.surface2);
                let label = (n + 1).to_string();
                let tw = measure(&f.small_bold, &label);
                l.text(r.center().x - tw / 2.0, r.top() + 1.0, &label, &f.small_bold, p.muted);
            }
            l.y += text_h.max(num_h) + 3.0;
        }
    }

    /// ACLog's host and port, edited in place.
    fn aclog_page(&mut self, l: &mut L) {
        let (p, f) = (l.p, l.f);
        l.header("ACLog connection", true);
        l.y += 8.0;
        let focus = self.focus_first && l.paint;
        if l.paint {
            self.focus_first = false;
        }
        let (mut h, mut port) = (self.host.clone(), self.aport.clone());
        l.form(&mut [("Host", &mut h, 24.0, false), ("Port", &mut port, 8.0, false)], 6.0, focus);
        self.host = h;
        self.aport = port;
        l.paragraph("In ACLog, turn on Settings > Application Program Interface > TCP API and use the port shown there (usually 1100).",
                    &f.small, p.muted, WIDTH - 40.0, 8.0, 4.0);
        let e = self.error.clone();
        l.text(l.left + 1.0, l.y + 1.0, &e, &f.small, p.bad);
        l.y += f.small.m.linespace + 2.0;
        match l.buttons_right(&[("Save", true, "Cancel"), ("Cancel", false, "Cancel")], 10.0).as_deref() {
            Some("Save") => l.act.push(Do::SaveAclog),
            Some("Cancel") => l.act.push(Do::Back),
            _ => {}
        }
    }

    /// pota.app username and password, for the hunt counts.
    fn pota_page(&mut self, app: &mut App, l: &mut L) {
        let (p, f) = (l.p, l.f);
        l.header("pota.app account", true);
        l.y += 8.0;
        let focus = self.focus_first && l.paint;
        if l.paint {
            self.focus_first = false;
        }
        let (mut u, mut pw) = (self.user.clone(), self.pass.clone());
        l.form(&mut [("Username", &mut u, 26.0, false), ("Password", &mut pw, 26.0, true)], 6.0, focus);
        self.user = u;
        self.pass = pw;
        l.paragraph("The username or email you sign in with at pota.app. Used only to read your hunt counts, so spots show \"new to me\" parks and how often you've worked each activator. The password is stored in settings.json on this computer.",
                    &f.small, p.muted, WIDTH - 40.0, 8.0, 4.0);
        let status = app.pota_status.clone();
        let c = if status.starts_with("Sign-in failed") { p.bad } else { p.muted };
        l.paragraph(&status, &f.small, c, WIDTH - 40.0, 0.0, 0.0);
        match l.buttons_right(&[("Save", true, "Cancel"), ("Cancel", false, "Cancel")], 10.0).as_deref() {
            Some("Save") => l.act.push(Do::SavePota),
            Some("Cancel") => l.act.push(Do::Back),
            _ => {}
        }
    }

    /// The optional comment sent with each respot.
    fn respot_page(&mut self, l: &mut L) {
        let (p, f) = (l.p, l.f);
        l.header("Respot message", true);
        l.y += 8.0;
        let focus = self.focus_first && l.paint;
        if l.paint {
            self.focus_first = false;
        }
        let mut m = self.respot_msg.clone();
        l.form(&mut [("Message", &mut m, 26.0, false)], 6.0, focus);
        self.respot_msg = m;
        let help = format!(
            "Optional comment sent with each respot (blank = none). Placeholders, filled in from the logged QSO: {}.",
            respot::PLACEHOLDERS.iter().map(|(k, v)| format!("{k} = {v}")).collect::<Vec<_>>().join("; "));
        l.paragraph(&help, &f.small, p.muted, WIDTH - 40.0, 8.0, 4.0);
        match l.buttons_right(&[("Save", true, "Cancel"), ("Cancel", false, "Cancel")], 10.0).as_deref() {
            Some("Save") => l.act.push(Do::SaveRespot),
            Some("Cancel") => l.act.push(Do::Back),
            _ => {}
        }
    }

    fn footer_h(f: &Fonts) -> f32 {
        1.0 + f.small.m.linespace + 2.0 * (8.0 + BD)
    }

    pub fn show(&mut self, app: &mut App, ctx: &egui::Context, ui: &mut Ui) {
        if !self.open {
            return;
        }
        let now = app.now();
        let f = app.f.clone();
        let p = app.p.clone();
        // measure: the main page is sized for its largest tab so switching tabs never
        // resizes (and so never moves) the panel
        let probe = Rect::from_min_size(Pos2::ZERO, Vec2::new(WIDTH, 2000.0));
        let (mut w, mut h) = (WIDTH, 0.0f32);
        let tabs: Vec<Tab> = if self.page == Page::Main { TABS.iter().map(|t| t.0).collect() } else { vec![self.tab] };
        for t in tabs {
            let (nw, nh, _) = self.build(app, ui, probe, false, t);
            w = w.max(nw);
            h = h.max(nh);
        }
        let total = Vec2::new(w, h + Self::footer_h(&f));
        let gear = app.gear_rect;
        let screen = ctx.content_rect();
        let x = (gear.right() - total.x).min(screen.right() - total.x).max(screen.left());
        let y = (gear.bottom() + 8.0).min(screen.bottom() - total.y).max(screen.top());
        let rect = Rect::from_min_size(Pos2::new(x, y), total);
        let alpha = (((now - self.opened_at) / 0.075) as f32).clamp(0.2, 1.0);
        if alpha < 1.0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(15));
        }
        let mut acts = vec![];
        egui::Area::new(Id::unique("settings-panel"))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .constrain(false)
            .show(ctx, |aui| {
                aui.multiply_opacity(alpha);
                let _ = aui.allocate_exact_size(total, Sense::click());
                let painter = aui.painter().clone();
                painter.rect_filled(rect, ui::radius(8.0), p.surface);
                let content = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.bottom() - Self::footer_h(&f)));
                let tab = self.tab;
                let (_, _, a) = self.build(app, aui, content, true, tab);
                acts = a;
                // footer
                let fy = rect.bottom() - Self::footer_h(&f);
                painter.rect_filled(Rect::from_min_size(Pos2::new(rect.left(), fy), Vec2::new(total.x, 1.0)), 0.0, p.border);
                let fr = Rect::from_min_max(Pos2::new(rect.left(), fy + 1.0), rect.max);
                painter.rect_filled(fr, egui::CornerRadius { nw: 0, ne: 0, sw: 8, se: 8 }, p.surface2);
                ui::text(aui, &painter, Pos2::new(rect.left() + 20.0 + BD, fy + 1.0 + BD + 8.0),
                         "Changes are saved automatically.  Esc closes this panel.", &f.small, p.muted);
                painter.rect_stroke(rect, ui::radius(8.0), Stroke::new(1.0, p.border), StrokeKind::Inside);
            });
        // keyboard: Esc steps out of a sub-page first; Enter saves a sub-page
        let (esc, enter) = ctx.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
        if esc {
            acts.push(Do::Back);
        }
        if enter {
            match self.page {
                Page::Aclog => acts.push(Do::SaveAclog),
                Page::Pota => acts.push(Do::SavePota),
                Page::Respot => acts.push(Do::SaveRespot),
                Page::Wsjtx => acts.push(Do::ApplyUdp),
                Page::Main => {}
            }
        }
        // a click elsewhere (or another app taking focus) closes it, like losing focus
        let pressed_out = ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|q| !rect.contains(q)));
        let on_gear = ctx.input(|i| i.pointer.interact_pos().is_some_and(|q| gear.contains(q)));
        if (pressed_out && !on_gear && now - self.opened_at > 0.05) || (!ctx.input(|i| i.focused) && !self.demo) {
            self.close(now);
            return;
        }
        for a in acts {
            match a {
                Do::Tab(t) => {
                    self.about_sel = None;
                    self.tab = t;
                    self.page = Page::Main;
                }
                Do::Page(pg) => self.show_page(app, pg),
                Do::Close => self.close(now),
                Do::Back => match self.page {
                    Page::Main => self.close(now),
                    Page::Aclog => self.show_page(app, Page::Main),
                    _ => {
                        self.tab = Tab::Connect;
                        self.show_page(app, Page::Main);
                    }
                },
                Do::Changed => app.settings_changed(),
                Do::Theme(mode) => app.set_theme(ctx, mode),
                Do::FontSize(pt) => app.set_font_size(ctx, pt),
                Do::Test(k) => {
                    let path = if matches!(k, Kind::Ding) { &app.s.ding_path } else { &app.s.pop_path };
                    crate::sound::play(k, path);
                }
                Do::Choose(k) => {
                    if let Some(path) = choose_sound_file(k) {
                        match k {
                            Kind::Ding => app.s.ding_path = path.clone(),
                            Kind::Pop => app.s.pop_path = path.clone(),
                        }
                        app.settings_changed();
                        crate::sound::play(k, &path);
                    }
                }
                Do::ResetSound(k) => {
                    match k {
                        Kind::Ding => app.s.ding_path.clear(),
                        Kind::Pop => app.s.pop_path.clear(),
                    }
                    app.settings_changed();
                }
                Do::ResetUdp => {
                    self.group = "224.0.0.2".into();
                    self.port = "2237".into();
                }
                Do::ApplyUdp => {
                    let addr = self.group.trim().to_string();
                    let port = match self.port.trim().parse::<u16>() {
                        Ok(n) if n >= 1 => n,
                        _ => {
                            self.error = "Port must be a number from 1 to 65535".into();
                            continue;
                        }
                    };
                    if addr.parse::<std::net::Ipv4Addr>().is_err() || addr.matches('.').count() != 3 {
                        self.error = "Address must look like 224.0.0.2 (or 127.0.0.1)".into();
                        continue;
                    }
                    app.apply_udp(&addr, port);
                    self.show_page(app, Page::Wsjtx);
                }
                Do::SaveAclog => {
                    let port = match self.aport.trim().parse::<u16>() {
                        Ok(n) if n >= 1 => n,
                        _ => {
                            self.error = "Port must be a number from 1 to 65535".into();
                            continue;
                        }
                    };
                    let host = self.host.trim();
                    app.s.aclog_host = if host.is_empty() { "127.0.0.1".into() } else { host.to_string() };
                    app.s.aclog_port = port;
                    app.settings_changed();
                    self.tab = Tab::Logging;
                    self.show_page(app, Page::Main);
                }
                Do::SavePota => {
                    let (u, pw) = (self.user.clone(), self.pass.clone());
                    app.apply_pota_login(&u, &pw, true);
                    self.tab = Tab::Connect;
                    self.show_page(app, Page::Main);
                }
                Do::SaveRespot => {
                    app.s.respot_message = self.respot_msg.trim().to_string();
                    app.settings_changed();
                    self.tab = Tab::Connect;
                    self.show_page(app, Page::Main);
                }
            }
        }
        let _ = TipAt::Pointer;
    }
}

/// The sound file picker (a standard Windows Open dialog).
fn choose_sound_file(kind: Kind) -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW};
    let filter: Vec<u16> = "Sound files\0*.mp3;*.wav;*.wma;*.m4a\0All files\0*.*\0\0".encode_utf16().collect();
    let what = if matches!(kind, Kind::Ding) { "when a QSO is logged" } else { "for a wanted park" };
    let title: Vec<u16> = format!("Sound to play {what}\0").encode_utf16().collect();
    let mut buf = vec![0u16; 1024];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = crate::theme::find_own_window(crate::app::TITLE) as _;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..n]))
}
