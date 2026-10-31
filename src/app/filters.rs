// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The "Program filters" window (Program Filter drop-down > Manage all filters): every band
//! and mode on the left with how many programs are hidden there, and for the one picked the
//! programs spotted on it with a checkbox each (checked = shown), so all the hiding can be
//! done in one place. A real window like the dialogs, blocking the main window until Done.
//! A child module of `app` so it can reach the App's private state.

use std::collections::HashMap;

use eframe::egui::{self, Color32, CursorIcon, Id, Key, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, ViewportBuilder, ViewportId};

use super::{refresh_frame, App};
use crate::hunter::BANDS;
use crate::pota_spots::DIGITAL_MODES;
use crate::theme;
use crate::ui::{self, measure, Rnd};

const TITLE: &str = "Program filters";
const SIZE: Vec2 = Vec2::new(820.0, 560.0);
const ROW: f32 = 30.0;

pub(super) struct Filters {
    /// the band and mode whose programs are listed on the right
    sel: Option<(String, String)>,
    pos: Option<Pos2>,
    hwnd: isize,
    titled: Option<bool>,
}

impl Filters {
    pub fn new(current: Option<(String, String)>) -> Self {
        Filters { sel: current, pos: None, hwnd: 0, titled: None }
    }
}

/// One band and mode in the left list.
struct ComboRow {
    band: String,
    mode: String,
    spots: usize,
    hidden: usize,
}

impl App {
    /// Every band and mode with digital spots, plus any with a hidden program and the one
    /// WSJT-X is on, in band order then mode order.
    fn filter_combos(&self) -> Vec<ComboRow> {
        let mut m: HashMap<(String, String), (usize, usize)> = HashMap::new();
        if let Some(t) = &self.h.spots {
            for (b, mode, n) in t.combos() {
                m.entry((b.to_string(), mode)).or_default().0 = n;
            }
        }
        for k in &self.s.hidden_programs {
            let mut it = k.splitn(3, ' ');
            if let (Some(_), Some(b), Some(mode)) = (it.next(), it.next(), it.next()) {
                m.entry((b.to_string(), mode.to_string())).or_default().1 += 1;
            }
        }
        if let Some(c) = self.combo() {
            m.entry(c).or_default();
        }
        let mut v: Vec<ComboRow> = m.into_iter().map(|((band, mode), (spots, hidden))| ComboRow { band, mode, spots, hidden }).collect();
        let band_i = |b: &str| BANDS.iter().position(|x| x.0 == b).unwrap_or(99);
        let mode_i = |m: &str| DIGITAL_MODES.iter().position(|x| *x == m).unwrap_or(99);
        v.sort_by(|a, b| (band_i(&a.band), mode_i(&a.mode), &a.mode).cmp(&(band_i(&b.band), mode_i(&b.mode), &b.mode)));
        v
    }

    /// The programs spotted on `band` in `mode` plus any hidden there that are not spotted
    /// now, as (program, current spots, shown), the busiest first.
    fn filter_programs(&self, band: &str, mode: &str) -> Vec<(String, usize, bool)> {
        let mut progs: Vec<(String, usize)> = self.h.spots.as_ref().map(|t| t.programs(band, mode)).unwrap_or_default();
        let hidden = self.hidden_at(band, mode);
        for h in &hidden {
            if !progs.iter().any(|p| &p.0 == h) {
                progs.push((h.clone(), 0));
            }
        }
        progs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        progs.into_iter().map(|(p, n)| { let shown = !hidden.contains(&p); (p, n, shown) }).collect()
    }

    /// Draw the Program filters window, when open.
    pub(super) fn draw_filters(&mut self, ctx: &egui::Context, screen: Rect) {
        let Some(mut st) = self.filters.take() else { return };
        // the main window can't be used while it is up
        egui::Area::new(Id::unique("filters-blocker")).order(Order::Middle).fixed_pos(Pos2::ZERO).show(ctx, |ui| {
            ui.allocate_response(screen.size(), Sense::click_and_drag());
        });
        let combos = self.filter_combos();
        let has = |sel: &(String, String)| combos.iter().any(|c| c.band == sel.0 && c.mode == sel.1);
        if !st.sel.as_ref().is_some_and(has) {
            st.sel = self
                .combo()
                .filter(has)
                .or_else(|| combos.iter().find(|c| c.hidden > 0).or(combos.first()).map(|c| (c.band.clone(), c.mode.clone())));
        }
        let cur = self.combo();
        let progs = st.sel.as_ref().map(|(b, m)| self.filter_programs(b, m)).unwrap_or_default();
        if st.pos.is_none() {
            let main = ctx.input(|i| i.viewport().inner_rect).unwrap_or(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 800.0)));
            st.pos = Some(Pos2::new((main.left() + (main.width() - SIZE.x) / 2.0).max(0.0).round(), (main.top() + (main.height() - SIZE.y) / 3.0).max(0.0).round()));
        }
        let builder = ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(SIZE)
            .with_min_inner_size(Vec2::new(620.0, 380.0))
            .with_position(st.pos.unwrap())
            .with_minimize_button(false)
            .with_maximize_button(false);
        let (p, f) = (self.p.clone(), self.f.clone());
        let dark = self.theme_name == "dark";
        let (mut close, mut active, mut typed) = (false, false, Vec::new());
        let mut pick: Option<(String, String)> = None;
        let mut toggle: Option<(String, bool)> = None; // (program, hide it)
        let (mut show_all, mut hide_all, mut clear_all) = (false, false, false);
        let sel = st.sel.clone();
        ctx.show_viewport_immediate(ViewportId::from_hash_of("program-filters"), builder, |ui: &mut Ui, _class| {
            ui.input(|i| {
                for e in &i.events {
                    match e {
                        egui::Event::Text(t) => typed.extend(t.chars()),
                        egui::Event::PointerButton { pressed: true, .. } | egui::Event::MouseWheel { .. } => active = true,
                        egui::Event::Key { pressed: true, .. } => active = true,
                        _ => {}
                    }
                }
            });
            let full = ui.ctx().content_rect();
            let painter = ui.painter().clone();
            painter.rect_filled(full, 0.0, p.surface);
            let x0 = 20.0;
            let right = full.right() - 20.0;
            ui::text(ui, &painter, Pos2::new(x0, 16.0), TITLE, &f.heading, p.text);
            let sub_y = 16.0 + f.heading.m.linespace + 4.0;
            ui::text(ui, &painter, Pos2::new(x0, sub_y), "Check a program to show its spots, uncheck it to hide them. Each band and mode has its own list.", &f.small, p.muted);
            let top = sub_y + f.small.m.linespace + 14.0;
            let bottom_h = 60.0;
            let panes_bottom = full.bottom() - bottom_h;
            let left_w = 250.0f32.min((full.width() - 40.0) * 0.4);
            let left = Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x0 + left_w, panes_bottom));
            let rr = Rect::from_min_max(Pos2::new(left.right() + 12.0, top), Pos2::new(right, panes_bottom));
            for r in [left, rr] {
                painter.rect_filled(r, ui::radius(6.0), p.surface2);
                painter.rect_stroke(r, ui::radius(6.0), Stroke::new(1.0, p.border), StrokeKind::Inside);
            }

            // left: every band and mode, the number hidden on it in a red badge
            ui.scope_builder(UiBuilder::new().max_rect(left.shrink(1.0)), |ui| {
                egui::ScrollArea::vertical().id_salt("flt-combos").auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    for c in &combos {
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW), Sense::click());
                        let selected = sel.as_ref().is_some_and(|s| s.0 == c.band && s.1 == c.mode);
                        let pt = ui.painter().clone();
                        if selected {
                            pt.rect_filled(r, 0.0, p.select);
                            pt.rect_filled(Rect::from_min_size(r.min, Vec2::new(3.0, r.height())), 0.0, p.accent);
                        } else if resp.hovered() {
                            pt.rect_filled(r, 0.0, theme::mix(p.surface2, p.select, 0.5));
                        }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                        }
                        let ty = r.top() + ((ROW - f.ui.m.linespace) / 2.0).round();
                        let name = format!("{} {}", c.band, c.mode);
                        let w = ui::text(ui, &pt, Pos2::new(r.left() + 14.0, ty), &name, if selected { &f.ui_bold } else { &f.ui }, p.text);
                        if cur.as_ref().is_some_and(|k| k.0 == c.band && k.1 == c.mode) {
                            ui::text(ui, &pt, Pos2::new(r.left() + 14.0 + w + 8.0, ty + 1.0), "current", &f.small, p.faint);
                        }
                        if c.hidden > 0 {
                            let txt = format!("{} hidden", c.hidden);
                            let b = Rnd::new(&txt, &f.small_bold).pill().pad(8.0, 1.0).colors(p.bad, Color32::WHITE);
                            let sz = b.size(ui);
                            b.paint(ui, &pt, Rect::from_min_size(Pos2::new(r.right() - 14.0 - sz.x, r.center().y - sz.y / 2.0), sz), false);
                        } else {
                            let txt = format!("{} spots", c.spots);
                            let tw = measure(&f.small, &txt);
                            ui::text(ui, &pt, Pos2::new(r.right() - 14.0 - tw, r.center().y - f.small.m.linespace / 2.0), &txt, &f.small, p.faint);
                        }
                        if resp.clicked() {
                            pick = Some((c.band.clone(), c.mode.clone()));
                        }
                    }
                });
            });

            // right: the programs of the picked band and mode, a checkbox each
            if let Some((band, mode)) = &sel {
                let n_hidden = progs.iter().filter(|p| !p.2).count();
                ui::text(ui, &painter, Pos2::new(rr.left() + 14.0, rr.top() + 10.0), &format!("{band} {mode}"), &f.heading, p.text);
                let sub = if progs.is_empty() { "No programs spotted here right now".to_string() } else { format!("{} programs, {} hidden", progs.len(), n_hidden) };
                ui::text(ui, &painter, Pos2::new(rr.left() + 14.0, rr.top() + 10.0 + f.heading.m.linespace + 2.0), &sub, &f.small, p.muted);
                let mut bx = rr.right() - 12.0;
                for (label, which) in [("Show all", 0), ("Hide all", 1)] {
                    let b = Rnd::new(label, &f.small_bold).pill().pad(10.0, 3.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover);
                    let sz = b.size(ui);
                    let r = Rect::from_min_size(Pos2::new(bx - sz.x, rr.top() + 12.0), sz);
                    if b.show(ui, Id::unique(("flt-bulk", which)), r, true).clicked() {
                        if which == 0 { show_all = true } else { hide_all = true }
                    }
                    bx = r.left() - 8.0;
                }
                let list = Rect::from_min_max(Pos2::new(rr.left() + 1.0, rr.top() + 10.0 + f.heading.m.linespace + f.small.m.linespace + 20.0), rr.max - Vec2::splat(1.0));
                painter.rect_filled(Rect::from_min_size(Pos2::new(list.left(), list.top() - 1.0), Vec2::new(list.width(), 1.0)), 0.0, p.border);
                ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
                    egui::ScrollArea::vertical().id_salt("flt-progs").auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        for (prog, n, shown) in &progs {
                            let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW), Sense::click());
                            let pt = ui.painter().clone();
                            if resp.hovered() {
                                pt.rect_filled(r, 0.0, p.select);
                                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                            }
                            let side = (f.ui.m.linespace * 0.8).round();
                            let b = Rect::from_min_size(Pos2::new(r.left() + 14.0, r.top() + ((ROW - side) / 2.0).round()), Vec2::splat(side));
                            check_box(&pt, &p, b, *shown);
                            let name = crate::programs::name(prog).unwrap_or("(unknown_name)");
                            let ty = r.top() + ((ROW - f.ui.m.linespace) / 2.0).round();
                            ui::text(ui, &pt, Pos2::new(b.right() + 10.0, ty), &format!("{prog} ({name})"), &f.ui, p.text);
                            let spots = format!("[{n} current spot{}]", if *n == 1 { "" } else { "s" });
                            let sw = measure(&f.small, &spots);
                            let sy = r.center().y - f.small.m.linespace / 2.0;
                            ui::text(ui, &pt, Pos2::new(r.right() - 14.0 - sw, sy), &spots, &f.small, p.muted);
                            if !shown {
                                let hw = measure(&f.small_bold, "[hidden]");
                                ui::text(ui, &pt, Pos2::new(r.right() - 14.0 - sw - 10.0 - hw, sy), "[hidden]", &f.small_bold, p.bad);
                            }
                            if resp.clicked() {
                                toggle = Some((prog.clone(), *shown));
                            }
                        }
                    });
                });
            } else {
                ui::text(ui, &painter, Pos2::new(rr.left() + 14.0, rr.top() + 12.0), "Waiting for pota.app spots...", &f.ui, p.muted);
            }

            // bottom: the total, Clear all, and Done
            let total = self.s.hidden_programs.len();
            let by = panes_bottom + 14.0;
            let done = Rnd::new("Done", &f.ui_bold).pill().pad(30.0, 8.0).colors(p.accent, p.accent_text).hover(p.accent_hover);
            let ds = done.size(ui);
            let dr = Rect::from_min_size(Pos2::new(right - ds.x, by), ds);
            if done.show(ui, Id::unique("flt-done"), dr, true).clicked() {
                close = true;
            }
            let mid = dr.center().y;
            if total > 0 {
                let b = Rnd::new("Clear all filters", &f.small_bold).pill().pad(12.0, 4.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover);
                let sz = b.size(ui);
                let r = Rect::from_min_size(Pos2::new(x0, mid - sz.y / 2.0), sz);
                if b.show(ui, Id::unique("flt-clear"), r, true).clicked() {
                    clear_all = true;
                }
                ui::text(ui, &painter, Pos2::new(r.right() + 12.0, mid - f.small.m.linespace / 2.0), &format!("{total} hidden in all"), &f.small, p.bad);
            } else {
                ui::text(ui, &painter, Pos2::new(x0, mid - f.small.m.linespace / 2.0), "Nothing is hidden", &f.small, p.muted);
            }
            if ui.input(|i| i.key_pressed(Key::Escape) || i.key_pressed(Key::Enter) || i.viewport().close_requested()) {
                close = true;
            }
        });

        if let Some(s) = pick {
            st.sel = Some(s);
        }
        if let Some((band, mode)) = st.sel.clone() {
            if let Some((prog, hide)) = toggle {
                self.set_program_hidden_at(&band, &mode, Some((&prog, hide)));
            }
            if show_all {
                self.set_program_hidden_at(&band, &mode, None);
            }
            if hide_all {
                for (prog, _, shown) in &progs {
                    if *shown {
                        self.set_program_hidden_at(&band, &mode, Some((prog, true)));
                    }
                }
            }
        }
        if clear_all {
            self.s.hidden_programs.clear();
            self.settings_changed();
        }
        if active {
            self.activity();
        }
        for c in typed {
            self.bypass_keys(c);
        }
        if st.titled != Some(dark) {
            if st.hwnd == 0 {
                st.hwnd = theme::find_own_window(TITLE);
            }
            if st.hwnd != 0 {
                theme::set_title_bar(st.hwnd, dark);
                refresh_frame(st.hwnd);
                st.titled = Some(dark);
            }
        }
        if !close {
            self.filters = Some(st);
        }
    }
}

/// A checkbox: the accent color with a tick when checked, an outline when not.
fn check_box(painter: &egui::Painter, p: &theme::Palette, b: Rect, checked: bool) {
    if checked {
        painter.rect_filled(b, ui::radius(3.0), p.accent);
        let tick = Stroke::new(2.0, p.accent_text);
        let side = b.width();
        let at = |x: f32, y: f32| b.left_top() + Vec2::new(side * x, side * y);
        painter.line_segment([at(0.22, 0.52), at(0.42, 0.72)], tick);
        painter.line_segment([at(0.42, 0.72), at(0.78, 0.28)], tick);
    } else {
        painter.rect_filled(b, ui::radius(3.0), p.surface);
        painter.rect_stroke(b, ui::radius(3.0), Stroke::new(1.5, p.muted), StrokeKind::Inside);
    }
}

