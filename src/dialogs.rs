// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The modal windows: the enable confirmation and the "Are you still there?" pair.
//! Each is a real window (an egui viewport) with a title bar, centred over the app.

use eframe::egui::{self, Color32, Id, Key, Pos2, Rect, Stroke, Ui, Vec2, ViewportBuilder, ViewportId};

use crate::app::App;
use crate::theme::{self, Font, Fonts, Palette};
use crate::ui::{self, measure, Rnd};

#[derive(Clone, PartialEq, Debug)]
pub enum DialogKind {
    ConfirmHunt,
    ConfirmCq,
    PresenceAsk,
    /// which modes were switched off, e.g. "Auto-Hunt and Auto CQ/Answer"
    PresenceExpired(String),
    /// the Radio tab's CAT test: passed or not, and one line per item
    CatResult(bool, Vec<String>),
    /// Settings > About > Check now (or the header version): already on the latest release
    UpToDate(String),
    /// a newer release exists (its version); offers Update now / Cancel
    UpdateAvail(String),
    /// the update check failed (the reason)
    CheckFailed(String),
}

impl DialogKind {
    pub fn is_confirm(&self) -> bool {
        matches!(self, DialogKind::ConfirmHunt | DialogKind::ConfirmCq)
    }

    pub fn is_presence(&self) -> bool {
        matches!(self, DialogKind::PresenceAsk | DialogKind::PresenceExpired(_))
    }
}

pub struct Dialog {
    pub kind: DialogKind,
    pos: Option<Pos2>,
    hwnd: isize,
    titled: Option<bool>,
    /// characters typed into this window this frame (for the presence bypass, which
    /// Tk's bind_all saw in every window)
    pub typed: Vec<char>,
}

impl Dialog {
    pub fn new(kind: DialogKind) -> Self {
        Dialog { kind, pos: None, hwnd: 0, titled: None, typed: Vec::new() }
    }
}

pub enum Outcome {
    Open,
    Closed,
    Enable,
    StillHere,
    Expired,
}

const BD: f32 = 3.0; // a tk.Label's border (2) plus padding (1)
const WRAP: f32 = 440.0;

struct Para {
    text: String,
    font: Font,
    color: Color32,
    pad: f32,
}

fn label_size(f: &Font, s: &str, wrap: f32) -> Vec2 {
    let lines = ui::wrap_lines_measure(f, s, wrap);
    let w = lines.iter().map(|l| measure(f, l)).fold(0.0, f32::max);
    Vec2::new(w + 2.0 * BD, lines.len() as f32 * f.m.linespace + 2.0 * BD)
}

fn content(kind: &DialogKind, p: &Palette, f: &Fonts, dry: bool) -> (String, String, Color32, Vec<Para>) {
    let para = |t: &str, font: &Font, color: Color32, pad: f32| Para { text: t.to_string(), font: font.clone(), color, pad };
    match kind {
        DialogKind::ConfirmHunt | DialogKind::ConfirmCq => {
            let what = if *kind == DialogKind::ConfirmHunt { "Auto-Hunt" } else { "Auto CQ/Answer" };
            let last = if dry { "Dry run is on, so nothing will be transmitted yet.".to_string() } else { format!("Dry run is off: {what} will transmit.") };
            (format!("Enable {what}"), "You must be at your station".into(), p.bad, vec![
                para("As the control operator, you are required to be present and in control of your station whenever it is transmitting.", &f.ui, p.text, 10.0),
                para(&format!("Fully automated QSOs are prohibited. {what} only helps you call stations; it does not replace you. Stay at the radio, watch what it does, and turn it off if anything looks wrong."), &f.ui, p.text, 8.0),
                para(&last, &f.small, p.muted, 12.0),
            ])
        }
        DialogKind::PresenceAsk => ("Are you still there?".into(), "Are you still there?".into(), p.warn, vec![
            para("As the control operator you must be present and in control of your station whenever it is transmitting. Fully automated QSOs are prohibited, so the Auto modes check from time to time that somebody is really operating.", &f.ui, p.text, 8.0),
            para("Confirm below, or Auto-Hunt and Auto CQ/Answer will be switched off when the countdown ends.", &f.ui, p.text, 8.0),
        ]),
        DialogKind::CatResult(ok, lines) => {
            let mut v = vec![];
            for l in lines {
                let bad = l.contains("FAILED") || l.contains("NOT CONFIRMED") || l.starts_with("Could not") || l.starts_with("No radio") || l.starts_with("WSJT-X is");
                v.push(para(l, &f.ui, if bad { p.bad } else { p.text }, 6.0));
            }
            let (title, head, color) = if *ok {
                ("CAT test passed", "CAT control works", p.on)
            } else {
                ("CAT test failed", "CAT control test failed", p.bad)
            };
            if *ok {
                v.push(para("Every setting was changed, confirmed by the radio, and put back as it was.", &f.small, p.muted, 10.0));
            } else {
                v.push(para("Anything the test changed was put back where it could be; check the radio's ATT, NB, NR and power.", &f.small, p.muted, 10.0));
            }
            (title.into(), head.into(), color, v)
        }
        DialogKind::UpToDate(v) => ("Up to date".into(), "You're up to date".into(), p.on, vec![
            para(&format!("Version {v} is the latest release."), &f.ui, p.text, 8.0),
        ]),
        DialogKind::UpdateAvail(v) => ("Update available".into(), format!("Version {v} is available"), p.blue, vec![
            para(&format!("You are running version {}.", env!("CARGO_PKG_VERSION")), &f.ui, p.text, 8.0),
            para("Update now downloads the new version, swaps it in and restarts the app. The app closes normally first. It won't update while a QSO or an Auto mode is active.", &f.ui, p.text, 8.0),
        ]),
        DialogKind::CheckFailed(e) => ("Update check failed".into(), "Couldn't check for updates".into(), p.bad, vec![
            para(e, &f.ui, p.text, 8.0),
        ]),
        DialogKind::PresenceExpired(names) => ("Auto modes disabled".into(), format!("{names} switched off"), p.bad, vec![
            para("You did not confirm that you were at the station in time. Fully automated QSOs are prohibited, so the Auto modes are switched off whenever nobody answers.", &f.ui, p.text, 8.0),
            para("Calls already in progress in WSJT-X are left alone. You can switch the Auto modes back on yourself; that starts a fresh timer.", &f.ui, p.text, 8.0),
        ]),
    }
}

/// Draw the open dialog in its own window.
pub fn show(d: &mut Dialog, app: &mut App, ctx: &egui::Context) -> Outcome {
    let (p, f) = (app.p.clone(), app.f.clone());
    let (title, heading, heading_color, paras) = content(&d.kind, &p, &f, app.h.dry_run);
    // size, as Tk would request it
    let head = label_size(&f.heading, &heading, 10000.0);
    let mut w = head.x;
    let mut h = head.y;
    for pa in &paras {
        let s = label_size(&pa.font, &pa.text, WRAP);
        w = w.max(s.x);
        h += pa.pad + s.y;
    }
    let ui_bold = &f.ui_bold;
    let (b1, b2, extra_h) = match &d.kind {
        DialogKind::ConfirmHunt | DialogKind::ConfirmCq => (
            Some(Rnd::new("I'm at my station – Enable", ui_bold).pill().pad(18.0, 7.0).colors(p.on, Color32::WHITE).hover(p.on_hover)),
            Some(Rnd::new("Cancel", ui_bold).pill().pad(18.0, 7.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover)),
            0.0,
        ),
        DialogKind::UpdateAvail(_) => (
            Some(Rnd::new("Update now", ui_bold).pill().pad(18.0, 7.0).colors(p.blue, p.blue_text).hover(p.blue_hover)),
            Some(Rnd::new("Cancel", ui_bold).pill().pad(18.0, 7.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover)),
            0.0,
        ),
        DialogKind::PresenceAsk => (
            Some(Rnd::new("Yes, I'm still here", ui_bold).pill().pad(22.0, 8.0).colors(p.on, Color32::WHITE).hover(p.on_hover)),
            None,
            16.0 + 120.0,
        ),
        DialogKind::PresenceExpired(_) | DialogKind::CatResult(..) | DialogKind::UpToDate(_) | DialogKind::CheckFailed(_) => (
            Some(Rnd::new("OK", ui_bold).pill().pad(28.0, 8.0).colors(p.accent, p.accent_text).hover(p.accent_hover)),
            None,
            0.0,
        ),
    };
    let btn_h = b1.as_ref().map(|b| b.size_static().y).unwrap_or(0.0);
    let btn_gap = if d.kind.is_confirm() { 20.0 } else { 16.0 };
    h += extra_h + btn_gap + btn_h;
    let size = Vec2::new(w + 48.0, h + 40.0);
    if d.pos.is_none() {
        // centre it over the app window (a third of the way down)
        let main = ctx.input(|i| i.viewport().inner_rect).unwrap_or(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 800.0)));
        d.pos = Some(Pos2::new((main.left() + (main.width() - size.x) / 2.0).max(0.0).round(),
                               (main.top() + (main.height() - size.y) / 3.0).max(0.0).round()));
    }
    let mut builder = ViewportBuilder::default()
        .with_title(title.clone())
        .with_inner_size(size)
        .with_position(d.pos.unwrap())
        .with_resizable(false)
        .with_minimize_button(false)
        .with_maximize_button(false);
    if d.kind.is_presence() {
        builder = builder.with_always_on_top().with_close_button(false); // the operator may be looking at another window
    }
    let id = ViewportId::from_hash_of(("dialog", title.as_str()));
    let dark = app.theme_name == "dark";
    let remaining = app.presence.remaining().unwrap_or(0.0);
    let warn = app.presence.warn;
    let expired = d.kind == DialogKind::PresenceAsk && app.presence.phase() == Some(crate::presence::Phase::Expired);
    if expired {
        return Outcome::Expired;
    }
    let kind = d.kind.clone();
    let mut outcome = Outcome::Open;
    let mut typed = Vec::new();
    ctx.show_viewport_immediate(id, builder, |ui: &mut Ui, _class| {
        ui.input(|i| {
            for e in &i.events {
                if let egui::Event::Text(t) = e {
                    typed.extend(t.chars());
                }
            }
        });
        let painter = ui.painter().clone();
        let full = ui.ctx().content_rect();
        painter.rect_filled(full, 0.0, p.surface);
        let mut y = 20.0;
        let x = 24.0;
        ui::text(ui, &painter, Pos2::new(x + BD, y + BD), &heading, &f.heading, heading_color);
        y += head.y;
        for pa in &paras {
            y += pa.pad;
            let s = label_size(&pa.font, &pa.text, WRAP);
            ui::paragraph(ui, Some(&painter), Pos2::new(x + BD, y + BD), &pa.text, &pa.font, pa.color, WRAP);
            y += s.y;
        }
        let right = full.right() - 24.0;
        match kind {
            DialogKind::ConfirmHunt | DialogKind::ConfirmCq | DialogKind::UpdateAvail(_) => {
                y += 20.0;
                let (e, c) = (b1.as_ref().unwrap(), b2.as_ref().unwrap());
                let es = e.size(ui);
                let er = Rect::from_min_size(Pos2::new(right - es.x, y), es);
                if e.show(ui, Id::unique("dlg-enable"), er, true).clicked() {
                    outcome = Outcome::Enable;
                }
                let cs = c.size(ui);
                let cr = Rect::from_min_size(Pos2::new(er.left() - 8.0 - cs.x, y), cs);
                if c.show(ui, Id::unique("dlg-cancel"), cr, true).clicked() {
                    outcome = Outcome::Closed;
                }
                // Esc cancels; deliberately no Enter shortcut: enabling must be deliberate
                if ui.input(|i| i.key_pressed(Key::Escape) || i.viewport().close_requested()) {
                    outcome = Outcome::Closed;
                }
            }
            DialogKind::PresenceAsk => {
                y += 16.0;
                let c = Pos2::new(full.center().x, y + 60.0);
                let frac = (remaining / warn).clamp(0.0, 1.0) as f32;
                let color = if remaining <= 15.0 { p.bad } else { p.warn };
                donut(&painter, c, 60.0, 12.0, frac, color, p.border);
                let t = format!("{}", (remaining + 0.999) as i64);
                let tw = measure(&f.heading, &t);
                ui::text(ui, &painter, Pos2::new(c.x - tw / 2.0, c.y - f.heading.m.linespace / 2.0), &t, &f.heading, p.text);
                y += 120.0 + 16.0;
                let b = b1.as_ref().unwrap();
                let bs = b.size(ui);
                let br = Rect::from_min_size(Pos2::new((full.center().x - bs.x / 2.0).round(), y), bs);
                if b.show(ui, Id::unique("dlg-here"), br, true).clicked() {
                    outcome = Outcome::StillHere;
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                if ui.input(|i| i.viewport().close_requested()) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
            }
            DialogKind::PresenceExpired(_) | DialogKind::CatResult(..) | DialogKind::UpToDate(_) | DialogKind::CheckFailed(_) => {
                y += 16.0;
                let b = b1.as_ref().unwrap();
                let bs = b.size(ui);
                let br = Rect::from_min_size(Pos2::new(right - bs.x, y), bs);
                if b.show(ui, Id::unique("dlg-ok"), br, true).clicked() {
                    outcome = Outcome::Closed;
                }
                let easy_close = !matches!(kind, DialogKind::PresenceExpired(_));
                if easy_close && ui.input(|i| i.key_pressed(Key::Escape) || i.key_pressed(Key::Enter) || i.viewport().close_requested()) {
                    outcome = Outcome::Closed;
                } else if ui.input(|i| i.viewport().close_requested()) {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
            }
        }
    });
    d.typed = typed;
    if d.titled != Some(dark) {
        if d.hwnd == 0 {
            d.hwnd = theme::find_own_window(&title);
        }
        if d.hwnd != 0 {
            theme::set_title_bar(d.hwnd, dark);
            crate::app::refresh_frame(d.hwnd);
            d.titled = Some(dark);
        }
    }
    outcome
}

/// A progress ring: a track with an arc filling clockwise from 12 o'clock.
fn donut(painter: &egui::Painter, c: Pos2, r_out: f32, thickness: f32, frac: f32, color: Color32, track: Color32) {
    let r = r_out - 1.0 - thickness / 2.0;
    painter.circle_stroke(c, r, Stroke::new(thickness, track));
    if frac <= 0.0 {
        return;
    }
    let n = (frac * 120.0).ceil().max(2.0) as usize;
    let sweep = frac * std::f32::consts::TAU;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = sweep * i as f32 / n as f32;
            Pos2::new(c.x + r * a.sin(), c.y - r * a.cos())
        })
        .collect();
    painter.add(egui::Shape::line(pts, Stroke::new(thickness, color)));
}
