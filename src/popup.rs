// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! A themed popup menu (right-click on a spot, the band and mode drop-downs).

use eframe::egui::{self, CursorIcon, Id, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::theme::{Fonts, Palette};
use crate::ui::{self, measure};

#[derive(Clone, Debug)]
pub enum MenuCmd {
    Skip { call: String, ref_: String },
    Unskip { call: String, ref_: String },
    Band(String),
    Mode(String),
}

#[derive(Clone)]
pub enum Item {
    /// a small muted heading
    Heading(String),
    Row { label: String, cmd: Option<MenuCmd>, subtitle: String },
}

pub struct PopupMenu {
    pub items: Vec<Item>,
    pub pos: Pos2,
    opened_at: f64,
}

const BD: f32 = 2.0; // a tk.Label's default border

impl PopupMenu {
    pub fn new(items: Vec<Item>, pos: Pos2, now: f64) -> Self {
        PopupMenu { items, pos, opened_at: now }
    }

    fn row_h(f: &Fonts, it: &Item) -> f32 {
        match it {
            Item::Heading(_) => f.small_bold.m.linespace + 2.0 * (6.0 + BD),
            Item::Row { subtitle, .. } if !subtitle.is_empty() => {
                7.0 + f.ui.m.linespace + 2.0 * BD + f.small.m.linespace + 2.0 * BD + 7.0
            }
            Item::Row { .. } => 7.0 + f.ui.m.linespace + 2.0 * BD + 7.0,
        }
    }

    fn size(&self, f: &Fonts) -> Vec2 {
        let pad = 2.0 * (14.0 + BD);
        let w = self
            .items
            .iter()
            .map(|it| match it {
                Item::Heading(t) => measure(&f.small_bold, t) + pad,
                Item::Row { label, subtitle, .. } => measure(&f.ui, label).max(measure(&f.small, subtitle)) + pad,
            })
            .fold(0.0, f32::max);
        let h: f32 = self.items.iter().map(|it| Self::row_h(f, it)).sum();
        Vec2::new(w + 2.0, h + 2.0)
    }

    /// Draw; Some(Some(cmd)) when a row was picked, Some(None) when it should close.
    pub fn show(&self, ctx: &egui::Context, ui_ref: &egui::Ui, p: &Palette, f: &Fonts) -> Option<Option<MenuCmd>> {
        let size = self.size(f);
        let screen = ctx.content_rect();
        let x = self.pos.x.min(screen.right() - size.x).max(screen.left());
        let y = self.pos.y.min(screen.bottom() - size.y).max(screen.top());
        let rect = Rect::from_min_size(Pos2::new(x, y), size);
        let mut result = None;
        egui::Area::new(Id::unique("popup-menu"))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .constrain(false)
            .show(ctx, |ui| {
                let _ = ui.allocate_exact_size(size, Sense::click());
                let painter = ui.painter();
                painter.rect_filled(rect, ui::radius(8.0), p.surface);
                painter.rect_stroke(rect, ui::radius(8.0), Stroke::new(1.0, p.border), StrokeKind::Inside);
                let mut yy = rect.top() + 1.0;
                for (i, it) in self.items.iter().enumerate() {
                    let h = Self::row_h(f, it);
                    let r = Rect::from_min_size(Pos2::new(rect.left() + 1.0, yy), Vec2::new(size.x - 2.0, h));
                    yy += h;
                    match it {
                        Item::Heading(t) => {
                            ui::text(ui_ref, painter, Pos2::new(r.left() + 14.0 + BD, r.top() + 6.0 + BD), t, &f.small_bold, p.muted);
                        }
                        Item::Row { label, cmd, subtitle } => {
                            let resp = ui.interact(r, Id::unique(("popup-row", i)), Sense::click());
                            let enabled = cmd.is_some();
                            if enabled && resp.hovered() {
                                painter.rect_filled(r, 0.0, p.select);
                                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                            }
                            let fg = if enabled { p.text } else { p.faint };
                            let ty = r.top() + 7.0 + BD;
                            ui::text(ui_ref, painter, Pos2::new(r.left() + 14.0 + BD, ty), label, &f.ui, fg);
                            if !subtitle.is_empty() {
                                let sy = ty + f.ui.m.linespace + BD + BD;
                                ui::text(ui_ref, painter, Pos2::new(r.left() + 14.0 + BD, sy), subtitle, &f.small, p.muted);
                            }
                            if enabled && resp.clicked() {
                                result = Some(cmd.clone());
                            }
                        }
                    }
                }
            });
        if result.is_some() {
            return result;
        }
        // Esc, or a click anywhere else, closes it (like losing focus)
        let now = ctx.input(|i| i.time);
        let (esc, clicked_at) = ctx.input(|i| {
            (i.key_pressed(egui::Key::Escape),
             if i.pointer.any_pressed() { i.pointer.interact_pos() } else { None })
        });
        if esc || (now - self.opened_at > 0.05 && clicked_at.is_some_and(|c| !rect.contains(c))) || (!ctx.input(|i| i.focused) && std::env::var_os("POTA_DEMO").is_none()) {
            return Some(None);
        }
        None
    }
}
