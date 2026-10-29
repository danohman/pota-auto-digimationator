// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The setup wizard: runs by itself on the first start (no settings.json yet, before
//! anything is listening or polling) and again from Settings > Appearance > Run setup
//! wizard. One page per group of settings, each setting explained, Back and Next, then a
//! "Setup complete" box with a Continue button.

use eframe::egui::{self, Color32, Id, Key, Order, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2};

use super::{choose_sound_file, stepper_labels_attempts, stepper_labels_power, stepper_labels_snr, Do, Panel, CQ_CHOICES, L, NO_SNR_LIMIT};
use crate::app::{App, ATTEMPT_MODES, PRESENCE_SECONDS};
use crate::cat;
use crate::respot;
use crate::settings::Settings;
use crate::sound::{self, Kind};
use crate::theme::{Fonts, Palette};
use crate::ui::{self, measure, Rnd, Scroll};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Look,
    Link,
    WsjtxSetup,
    Safety,
    Hunting,
    Cq,
    Pota,
    Logging,
    Sounds,
    RadioConn,
    Radio,
    Review,
}

/// (page, title, one-line summary under the title)
const STEPS: [(Step, &str, &str); 13] = [
    (Step::Welcome, "Welcome", "What this program does and how it keeps you in control"),
    (Step::Look, "Appearance", "Light or dark, and how big the text is"),
    (Step::Link, "Connect to WSJT-X", "The address and port this program listens on"),
    (Step::WsjtxSetup, "Set up WSJT-X", "Options to switch on inside WSJT-X itself"),
    (Step::Safety, "Hunting: safety limits", "Dry run, how persistent to be, and how weak is too weak"),
    (Step::Hunting, "Hunting: behavior", "Which stations to call and where your transmit frequency goes"),
    (Step::Cq, "Auto CQ/Answer", "The CQ message used when you let the program call CQ"),
    (Step::Pota, "pota.app", "Your pota.app login and respotting"),
    (Step::Logging, "Logging", "WSJT-X's Log QSO window and other loggers"),
    (Step::Sounds, "Sounds", "What you hear when a QSO is logged or a wanted park appears"),
    (Step::RadioConn, "Radio connection", "Which radio you have and the serial port it is on"),
    (Step::Radio, "Radio", "Yaesu, Kenwood, Elecraft, Icom or Xiegu receiver and power settings sent over the COM port"),
    (Step::Review, "All set?", "Check your choices, then finish"),
];

const CARD_W: f32 = 660.0;
const SIDE: f32 = 28.0;

enum Nav {
    Back,
    Next,
    Cancel,
}

pub struct Wizard {
    step: usize,
    /// the "Setup complete" box is showing
    complete: bool,
    /// started from Settings rather than on the first run
    rerun: bool,
    /// the settings as they were when a re-run started (Cancel puts them back)
    orig: Option<Settings>,
    // edit buffers, copied into the settings when their page is left
    group: String,
    port: String,
    /// the logger picked in the Logging page drop-down, and every logger.s host and port text (see Panel::logger_picker)
    lg_sel: usize,
    lg_bufs: Vec<(String, String)>,
    user: String,
    pass: String,
    msg: String,
    error: String,
    scroll: Scroll,
    /// content height of each page, for (card width, font size, dry run)
    sizes: Option<((i32, u32, bool), Vec<f32>)>,
    /// the unfolded drop-down on the radio connection page, and the COM ports it lists
    dd: Option<&'static str>,
    ports: Vec<String>,
}

fn help(l: &mut L, s: &str) {
    let (f, p) = (l.f, l.p);
    let w = l.right - l.left - 2.0;
    l.paragraph(s, &f.small, p.muted, w, 0.0, 8.0);
}

fn lead(l: &mut L, s: &str) {
    let (f, p) = (l.f, l.p);
    let w = l.right - l.left - 2.0;
    l.paragraph(s, &f.ui, p.text, w, 8.0, 4.0);
}

fn bullet(l: &mut L, s: &str) {
    let (f, p) = (l.f, l.p);
    let y = l.y + 3.0;
    l.text(l.left + 4.0, y + 1.0, "\u{2022}", &f.small, p.muted);
    l.left += 18.0;
    let w = l.right - l.left - 2.0;
    l.paragraph(s, &f.small, p.text, w, 3.0, 0.0);
    l.left -= 18.0;
}

fn switch(l: &mut L, title: &str, sub: &str, v: &mut bool, text: &str) {
    l.switch_row(title, sub, v, true, 0.0);
    help(l, text);
}

fn error_line(l: &mut L, e: &str) {
    let (f, p) = (l.f, l.p);
    l.text(l.left + 1.0, l.y + 1.0, e, &f.small, p.bad);
    l.y += f.small.m.linespace + 2.0;
}

impl Wizard {
    pub fn new(s: &Settings, rerun: bool) -> Self {
        Wizard {
            step: 0,
            complete: false,
            rerun,
            orig: rerun.then(|| s.clone()),
            group: s.udp_group.clone(),
            port: s.udp_port.to_string(),
            lg_sel: 0,
            lg_bufs: vec![],
            user: s.pota_user.clone(),
            pass: s.pota_password.clone(),
            msg: s.respot_message.clone(),
            error: String::new(),
            scroll: Scroll::default(),
            sizes: None,
            dd: None,
            ports: vec![],
        }
    }

    pub fn is_rerun(&self) -> bool {
        self.rerun
    }

    /// POTA_DEMO "wizard:N" (page from 1) or "wizard:done": jump there (testing).
    pub fn demo(&mut self, arg: &str) {
        if arg == "done" {
            self.step = STEPS.len() - 1;
            self.complete = true;
        } else if let Ok(n) = arg.parse::<usize>() {
            self.step = n.clamp(1, STEPS.len()) - 1;
        }
    }

    /// Copy the page's edit buffers into the settings; the error says what to fix.
    fn commit(&mut self, app: &mut App, step: Step) -> Result<(), String> {
        match step {
            Step::Link => {
                let addr = self.group.trim().to_string();
                let port = match self.port.trim().parse::<u16>() {
                    Ok(n) if n >= 1 => n,
                    _ => return Err("Port must be a number from 1 to 65535".into()),
                };
                if addr.parse::<std::net::Ipv4Addr>().is_err() || addr.matches('.').count() != 3 {
                    return Err("Address must look like 224.0.0.1 (or 127.0.0.1)".into());
                }
                app.s.udp_group = addr;
                app.s.udp_port = port;
            }
            Step::Pota => {
                app.s.pota_user = self.user.trim().to_string();
                app.s.pota_password = self.pass.clone();
                app.s.respot_message = self.msg.trim().to_string();
            }
            _ => {}
        }
        Ok(())
    }

    /// Content height of every page at this width (cached until the width, the font
    /// size or the dry-run line of the review changes).
    fn heights(&mut self, app: &mut App, ui: &mut Ui, width: f32, p: &Palette, f: &Fonts) -> Vec<f32> {
        let key = (width as i32, app.s.font_size, app.s.dry_run);
        if let Some((k, v)) = &self.sizes {
            if *k == key {
                return v.clone();
            }
        }
        let v: Vec<f32> = (0..STEPS.len())
            .map(|i| {
                let mut l = L { ui: &mut *ui, paint: false, left: SIDE, right: width - SIDE, y: 0.0, p, f, sections: 0, need_w: 0.0, act: vec![], anchor: Rect::NOTHING };
                self.body(app, &mut l, i);
                l.y + 8.0 + 20.0
            })
            .collect();
        self.sizes = Some((key, v.clone()));
        v
    }

    /// Draw the wizard over the whole window; true when it is finished (Continue, or
    /// Cancel on a re-run).
    pub fn show(&mut self, app: &mut App, ctx: &egui::Context, ui: &mut Ui) -> bool {
        let (p, f) = (app.p.clone(), app.f.clone());
        let screen = ctx.content_rect();
        ui.painter().rect_filled(screen, 0.0, p.bg);
        // covers the window behind it (a re-run draws on top of the running app)
        ui.interact(screen, Id::unique("wiz-cover"), Sense::click()); // not drag: that would stop text selection

        let cw = (screen.width() - 32.0).clamp(300.0, CARD_W);
        let btn_h = Rnd::new("Next", &f.ui_bold).pill().pad(16.0, 6.0).size(ui).y;
        let foot_h = 14.0 + btn_h + 14.0;
        let head_h = 14.0 + f.title.m.linespace + 4.0 + f.small.m.linespace + 14.0 + 4.0;
        let heights = self.heights(app, ui, cw, &p, &f);
        let max_h = heights.iter().cloned().fold(0.0, f32::max);
        let card_h = (head_h + max_h + foot_h).min(screen.height() - 32.0).max(head_h + foot_h + 120.0);
        let card = Rect::from_center_size(screen.center(), Vec2::new(cw, card_h));
        let card = Rect::from_min_size(card.min.round(), card.size());
        let painter = ui.painter().clone();
        let round = ui::radius(8.0);
        painter.rect_filled(card, round, p.surface);

        // header band: title, step counter, progress
        let band = Rect::from_min_size(card.min, Vec2::new(cw, head_h));
        let top = egui::CornerRadius { nw: 7, ne: 7, sw: 0, se: 0 };
        painter.rect_filled(band.shrink2(Vec2::new(1.0, 1.0)).with_max_y(band.bottom()), top, p.head);
        let (_, title, blurb) = STEPS[self.step];
        ui::text(ui, &painter, Pos2::new(card.left() + SIDE, band.top() + 14.0 + 1.0), title, &f.title, p.text);
        let counter = format!("Step {} of {}", self.step + 1, STEPS.len());
        let cwid = measure(&f.small, &counter);
        ui::text(ui, &painter, Pos2::new(card.right() - SIDE - cwid, band.top() + 14.0 + (f.title.m.linespace - f.small.m.linespace) / 2.0 + 1.0), &counter, &f.small, p.muted);
        ui::text(ui, &painter, Pos2::new(card.left() + SIDE, band.top() + 14.0 + f.title.m.linespace + 4.0 + 1.0), blurb, &f.small, p.muted);
        let strip = Rect::from_min_size(Pos2::new(card.left() + 1.0, band.bottom() - 4.0), Vec2::new(cw - 2.0, 4.0));
        painter.rect_filled(strip, 0.0, p.border);
        let done_w = strip.width() * (self.step + 1) as f32 / STEPS.len() as f32;
        painter.rect_filled(Rect::from_min_size(strip.min, Vec2::new(done_w, 4.0)), 0.0, p.accent);

        // body, scrolled when the window is too short for the page
        let body = Rect::from_min_max(Pos2::new(card.left(), band.bottom()), Pos2::new(card.right(), card.bottom() - foot_h));
        let content = heights[self.step];
        let view = body.height();
        self.scroll.clamp(content, view);
        if !self.complete {
            self.scroll.wheel(ui, body, f.ui.m.linespace, content, view);
        }
        let mut child = ui.new_child(UiBuilder::new().max_rect(body));
        child.set_clip_rect(body.intersect(ui.clip_rect()));
        let acts = {
            let mut l = L { ui: &mut child, paint: true, left: card.left() + SIDE, right: card.right() - SIDE, y: body.top() + 8.0 - self.scroll.offset, p: &p, f: &f, sections: 0, need_w: 0.0, act: vec![], anchor: Rect::NOTHING };
            self.body(app, &mut l, self.step);
            std::mem::take(&mut l.act)
        };
        if content > view {
            let bar = Rect::from_min_max(Pos2::new(body.right() - 12.0, body.top() + 4.0), Pos2::new(body.right() - 2.0, body.bottom() - 4.0));
            self.scroll.bar(ui, Id::unique("wiz-scroll"), bar, content, view, &p);
        }

        // footer: Back, (Cancel), progress dots, Next / Finish
        let fy = card.bottom() - foot_h;
        painter.rect_filled(Rect::from_min_size(Pos2::new(card.left(), fy), Vec2::new(cw, 1.0)), 0.0, p.border);
        painter.rect_filled(Rect::from_min_max(Pos2::new(card.left() + 1.0, fy + 1.0), card.max - Vec2::new(1.0, 1.0)), egui::CornerRadius { nw: 0, ne: 0, sw: 7, se: 7 }, p.surface2);
        let last = self.step + 1 == STEPS.len();
        let mk = |label: &'static str, primary: bool, enabled: bool| {
            let r = Rnd::new(label, &f.ui_bold).pill().pad(16.0, 6.0).min_text("Finish");
            if !enabled {
                r.colors(p.surface2, p.faint)
            } else if primary {
                r.colors(p.accent, p.accent_text).hover(p.accent_hover)
            } else {
                r.colors(p.neutral, p.neutral_text).hover(p.neutral_hover)
            }
        };
        let live = !self.complete;
        let mut nav = None;
        let by = fy + 1.0 + 13.0;
        let back = mk("Back", false, self.step > 0);
        let sz = back.size(ui);
        let mut x = card.left() + SIDE;
        if back.show(ui, Id::unique("wiz-back"), Rect::from_min_size(Pos2::new(x, by), sz), live && self.step > 0).clicked() && live && self.step > 0 {
            nav = Some(Nav::Back);
        }
        x += sz.x + 8.0;
        if self.rerun {
            let cancel = mk("Cancel", false, true);
            let csz = cancel.size(ui);
            if cancel.show(ui, Id::unique("wiz-cancel"), Rect::from_min_size(Pos2::new(x, by), csz), live).clicked() && live {
                nav = Some(Nav::Cancel);
            }
            x += csz.x + 8.0;
        }
        let next = mk(if last { "Finish" } else { "Next" }, true, true);
        let nsz = next.size(ui);
        let nx = card.right() - SIDE - nsz.x;
        if next.show(ui, Id::unique("wiz-next"), Rect::from_min_size(Pos2::new(nx, by), nsz), live).clicked() && live {
            nav = Some(Nav::Next);
        }
        let dots_w = STEPS.len() as f32 * 14.0 - 6.0;
        if nx - x > dots_w + 24.0 {
            let mut dx = ((x + nx) / 2.0 - dots_w / 2.0).round();
            for i in 0..STEPS.len() {
                let c = if i == self.step { p.accent } else if i < self.step { p.muted } else { p.border };
                painter.circle_filled(Pos2::new(dx + 4.0, fy + 1.0 + foot_h / 2.0), 4.0, c);
                dx += 14.0;
            }
        }
        painter.rect_stroke(card, round, Stroke::new(1.0, p.border), StrokeKind::Inside);

        // actions from the page (theme, text size, sounds)
        for a in acts {
            match a {
                Do::Theme(m) => app.set_theme(ctx, m),
                Do::FontSize(pt) => app.set_font_size(ctx, pt),
                Do::Test(k) => {
                    let path = if matches!(k, Kind::Ding) { app.s.ding_path.clone() } else { app.s.pop_path.clone() };
                    sound::play(k, &path);
                }
                Do::Choose(k) => {
                    if let Some(path) = choose_sound_file(k) {
                        match k {
                            Kind::Ding => app.s.ding_path = path.clone(),
                            Kind::Pop => app.s.pop_path = path.clone(),
                        }
                        sound::play(k, &path);
                    }
                }
                Do::ResetSound(k) => match k {
                    Kind::Ding => app.s.ding_path.clear(),
                    Kind::Pop => app.s.pop_path.clear(),
                },
                Do::TestCat => app.start_cat_test(),
                Do::CheckPota => {
                    let (u, pw) = (self.user.clone(), self.pass.clone());
                    app.check_pota(&u, &pw);
                }
                Do::Page(_) | Do::Close | Do::Back | Do::Changed | Do::Tab(_) => {}
                _ => {}
            }
        }
        if let Some(n) = nav {
            let step = STEPS[self.step].0;
            match n {
                Nav::Back => {
                    let _ = self.commit(app, step);
                    self.step -= 1;
                    self.dd = None;
                    self.error.clear();
                    self.scroll.offset = 0.0;
                }
                Nav::Cancel => {
                    self.cancel(app, ctx);
                    return true;
                }
                Nav::Next => match self.commit(app, step) {
                    Err(e) => self.error = e,
                    Ok(()) if last => match app.wizard_apply() {
                        Ok(()) => {
                            self.complete = true;
                            self.error.clear();
                        }
                        Err(e) => self.error = e,
                    },
                    Ok(()) => {
                        self.step += 1;
                        self.dd = None;
                        self.error.clear();
                        self.scroll.offset = 0.0;
                    }
                },
            }
        }
        if self.complete {
            return self.modal(app, ctx, screen, &p, &f);
        }
        false
    }

    /// Put the settings back as they were when the re-run started.
    fn cancel(&mut self, app: &mut App, ctx: &egui::Context) {
        let Some(o) = self.orig.take() else { return };
        let pt = o.font_size;
        let shown = app.s.font_size;
        app.s = o;
        app.s.font_size = shown;
        app.set_font_size(ctx, pt);
        app.apply_theme(ctx, None);
    }

    /// The "Setup complete" box over a dimmed wizard; true when Continue is pressed.
    fn modal(&mut self, app: &App, ctx: &egui::Context, screen: Rect, p: &Palette, f: &Fonts) -> bool {
        let scrim = Color32::from_rgba_unmultiplied(p.bg.r(), p.bg.g(), p.bg.b(), 170);
        egui::Area::new(Id::unique("wiz-scrim")).order(Order::Foreground).fixed_pos(Pos2::ZERO).show(ctx, |ui| {
            ui.painter().rect_filled(screen, 0.0, scrim);
            ui.allocate_response(screen.size(), Sense::click_and_drag());
        });
        let w = (screen.width() - 32.0).clamp(280.0, 460.0);
        let inner = w - 64.0;
        let first = if self.rerun {
            "Your changes have been saved and are in effect."
        } else {
            "Your settings have been saved. When you press Continue, the program starts listening for WSJT-X and fetching the current POTA spots."
        };
        let dry = if app.s.dry_run {
            "Dry run is on: the program only shows what it would call and never transmits. Switch it off in Settings > Hunting when you are ready."
        } else {
            "Dry run is off: the program will transmit once you enable Auto-Hunt or Auto CQ/Answer."
        };
        let last = "Auto-Hunt and Auto CQ/Answer start switched off. Every setting can be changed later with the gear button (or Ctrl+,), and you can run this wizard again from Settings > Appearance.";
        let paras: Vec<(Vec<String>, &crate::theme::Font, Color32)> = vec![
            (ui::wrap_lines(&f.ui, first, inner), &f.ui, p.text),
            (ui::wrap_lines(&f.small, dry, inner), &f.small, if app.s.dry_run { p.muted } else { p.warn }),
            (ui::wrap_lines(&f.small, last, inner), &f.small, p.muted),
        ];
        let btn = Rnd::new("Continue", &f.ui_bold).pill().pad(24.0, 7.0).colors(p.accent, p.accent_text).hover(p.accent_hover);
        let bsz = btn.size_static();
        let text_h: f32 = paras.iter().map(|(l, font, _)| l.len() as f32 * font.m.linespace + 8.0).sum();
        let h = 28.0 + 56.0 + 14.0 + f.title.m.linespace + 10.0 + text_h + 12.0 + bsz.y + 28.0;
        let rect = Rect::from_center_size(screen.center(), Vec2::new(w, h));
        let rect = Rect::from_min_size(rect.min.round(), rect.size());
        let mut pressed = false;
        egui::Area::new(Id::unique("wiz-done")).order(Order::Tooltip).fixed_pos(rect.min).show(ctx, |ui| {
            let painter = ui.painter().clone();
            painter.rect_filled(rect, ui::radius(8.0), p.surface);
            let cx = rect.center().x;
            let mut y = rect.top() + 28.0;
            painter.circle_filled(Pos2::new(cx, y + 28.0), 28.0, p.ok);
            let c = Pos2::new(cx, y + 28.0);
            painter.add(Shape::line(vec![c + Vec2::new(-12.0, 1.0), c + Vec2::new(-4.0, 9.0), c + Vec2::new(13.0, -9.0)], Stroke::new(4.0, Color32::WHITE)));
            y += 56.0 + 14.0;
            let t = "Setup complete";
            ui::text(ui, &painter, Pos2::new((cx - measure(&f.title, t) / 2.0).round(), y + 1.0), t, &f.title, p.text);
            y += f.title.m.linespace + 10.0;
            for (lines, font, color) in &paras {
                for line in lines {
                    ui::text(ui, &painter, Pos2::new((cx - measure(font, line) / 2.0).round(), y + 1.0), line, font, *color);
                    y += font.m.linespace;
                }
                y += 8.0;
            }
            y += 12.0;
            let r = Rect::from_min_size(Pos2::new((cx - bsz.x / 2.0).round(), y), bsz);
            if btn.show(ui, Id::unique("wiz-continue"), r, true).clicked() {
                pressed = true;
            }
            painter.rect_stroke(rect, ui::radius(8.0), Stroke::new(1.0, p.border), StrokeKind::Inside);
        });
        pressed || ctx.input(|i| i.key_pressed(Key::Enter))
    }

    // --- pages -----------------------------------------------------------------

    fn body(&mut self, app: &mut App, l: &mut L, step: usize) {
        let (p, f) = (l.p, l.f);
        match STEPS[step].0 {
            Step::Welcome => {
                lead(l, "This program listens to WSJT-X and, when it hears a Parks on the Air (POTA) activator calling, makes WSJT-X call them for you. It also logs the contact, can respot the activator on pota.app, and remembers who you have already worked today.");
                lead(l, "This setup goes through every setting, explains what it does and suggests a value. Nothing is transmitted and nothing is saved until you press Finish on the last page. Use Back whenever you want to change an earlier answer. Every choice can be changed later with the gear button (or Ctrl+,).");
                l.section("Before you start");
                bullet(l, "WSJT-X or WSJT-X Improved installed. It does not have to be running yet, but you need it to finish the connection.");
                bullet(l, "Optional: your pota.app login, so the program can show which parks are new to you.");
                bullet(l, "Optional: N3FJP ACLog, Log4OM, DXKeeper, HRD Logbook, N1MM Logger+ or Logger32, if you want every QSO logged there too.");
                bullet(l, "Optional: a Yaesu FTDX101D, Kenwood TS-590S/SG/TS-890S Elecraft, Icom or Xiegu radio controlled over its COM port, for the receiver and power settings.");
                l.y += 4.0;
                l.section("You stay in control");
                lead(l, &format!("Amateur radio rules require a control operator at the station, so fully automatic, unattended operation is not allowed. To keep it that way the program starts with Auto-Hunt and Auto CQ/Answer switched off, asks you to confirm every time you turn one on, and asks \"Are you still there?\" every {} minutes while either is on. If you do not answer, it switches itself off.", (PRESENCE_SECONDS / 60.0) as u32));
            }
            Step::Look => {
                l.section("Theme");
                help(l, "Light and dark are both fully supported. \"Match Windows\" follows the Windows light or dark setting and changes by itself when you change it in Windows; Light and Dark stay as you set them. The change is applied at once so you can see it.");
                let choices = [("system", "Match Windows"), ("light", "Light"), ("dark", "Dark")];
                let sz = l.segmented_size(&choices);
                if l.paint {
                    if let Some(v) = l.segmented(l.left, l.y, "wiz-theme", &choices, &app.s.theme.clone()) {
                        l.act.push(Do::Theme(v));
                    }
                }
                l.y += sz.y + 12.0;
                l.section("Text size");
                help(l, "The size of the text in points, from 8 to 16. Everything scales together: the lists, the activity log and the settings. 10 pt is the default; try 11 or 12 on a high-resolution screen if the text looks small.");
                let sizes: Vec<u32> = (8..=16).collect();
                let labels: Vec<String> = sizes.iter().map(|n| format!("{n} pt")).collect();
                let idx = sizes.iter().position(|n| *n == app.s.font_size).unwrap_or(2);
                if let Some(i) = l.stepper_row("Font size", "Applied at once", &labels, idx) {
                    l.act.push(Do::FontSize(sizes[i]));
                }
            }
            Step::Link => {
                lead(l, "WSJT-X sends every decode and status update to one UDP address and port. This program listens there, and answers through the same connection when it wants WSJT-X to call a station. Both sides must use the same address and port.");
                l.section("Where to listen");
                let (mut g, mut port) = (self.group.clone(), self.port.clone());
                l.form(&mut [("UDP address", &mut g, 24.0, false), ("UDP port", &mut port, 8.0, false)], 5.0, false);
                self.group = g;
                self.port = port;
                error_line(l, &self.error.clone());
                if let Some("Reset to default") = l.buttons_right(&[("Reset to default", false, "")], 0.0).as_deref() {
                    self.group = "224.0.0.1".into();
                    self.port = "2237".into();
                    self.error.clear();
                }
                l.y += 8.0;
                help(l, "Address: the default, 224.0.0.1, is a multicast address. WSJT-X uses it out of the box, and it lets several programs (GridTracker, JTAlert and this one) listen at the same time. A plain address such as 127.0.0.1 also works, but then only one program can receive.");
                help(l, "Port: WSJT-X's default is 2237. Change it only if you changed it in WSJT-X (File > Settings > Reporting > UDP Server), or if another program already uses it.");
                help(l, "The connection is made when setup finishes. Until WSJT-X is heard, the main window shows a \"Waiting for WSJT-X\" box that goes away by itself.");
            }
            Step::WsjtxSetup => {
                let (g, port) = (app.s.udp_group.clone(), app.s.udp_port);
                lead(l, "Now set these options in WSJT-X itself. Without them this program cannot hear WSJT-X, or make it call anybody. You can do it now or after setup; nothing here changes how you answer this wizard.");
                let steps = [
                    format!("File > Settings > Reporting > UDP Server: set the address to {g} and the port to {port}. These must match the values on the previous page."),
                    "Tick Accept UDP requests in the same place. Without it WSJT-X ignores every call this program sends.".to_string(),
                    "Under Outgoing interfaces, tick the loopback adapter (and your network adapter if WSJT-X runs on another computer).".to_string(),
                    "WSJT-X Improved only: untick CQ only and Hide FT8 dupes in the Band Activity window. WSJT-X acts on a call only if it is showing that decode line, so hidden lines are ignored. If you leave CQ only on, turn off Call stations after their 73 on the hunting page.".to_string(),
                    "On WSJT-X's main window tick Hold Tx Freq (keeps your transmit frequency where you set it) and Auto Seq. In File > Settings > General tick Disable Tx after sending 73. In File > Settings > Radio set the Poll Interval to 1 to 3 seconds, so WSJT-X notices band changes the app makes on the radio.".to_string(),
                    "Leave WSJT-X running and monitoring the band you want to hunt. You must be at the station: you remain the control operator.".to_string(),
                ];
                l.section("In WSJT-X");
                for (n, s) in steps.iter().enumerate() {
                    l.y += 3.0;
                    let num_w = 2.0 * measure(&f.small_bold, "0") + 2.0;
                    let num_h = f.small_bold.m.linespace + 2.0;
                    let wrap = l.right - l.left - num_w - 12.0;
                    let painter = l.ui.painter().clone();
                    let text_h = ui::paragraph(l.ui, if l.paint { Some(&painter) } else { None }, Pos2::new(l.left + num_w + 10.0, l.y + 1.0), s, &f.small, p.text, wrap) + 2.0;
                    if l.paint {
                        let r = Rect::from_min_size(Pos2::new(l.left, l.y), Vec2::new(num_w, num_h));
                        painter.rect_filled(r, 0.0, p.surface2);
                        let label = (n + 1).to_string();
                        l.text(r.center().x - measure(&f.small_bold, &label) / 2.0, r.top() + 1.0, &label, &f.small_bold, p.muted);
                    }
                    l.y += text_h.max(num_h) + 3.0;
                }
            }
            Step::Safety => {
                lead(l, "These settings limit what the program does on its own. The defaults are a good start.");
                l.section("Dry run");
                switch(l, "Dry run", "Show what it would call, never transmit", &mut app.s.dry_run,
                       "While this is on the program does everything except transmit: it lists the stations it would call (\"DRY RUN\" lines) and what it would respot. It applies to Auto-Hunt and to Auto CQ/Answer. Keep it on for your first sessions to see how the program behaves, then switch it off in Settings > Hunting.");
                l.section("Give up after");
                for mode in ATTEMPT_MODES {
                    let cur = app.s.attempts_by_mode.get(mode).copied().unwrap_or(app.s.max_attempts);
                    let (vals, labels) = stepper_labels_attempts(cur);
                    let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
                    if let Some(i) = l.stepper_row(&format!("Give up after ({mode})"), "Unanswered calls before moving on", &labels, idx) {
                        app.s.attempts_by_mode.insert(mode.to_string(), vals[i]);
                    }
                }
                help(l, "How many times to call a station without an answer before giving up on it for ten minutes. Slower modes need fewer calls because each call takes longer: 5 for FT8, 10 for FT4 and 20 for the short FT2 periods are the defaults. Any other mode uses 4.");
                l.section("Weak stations");
                let cur = app.s.min_snr.unwrap_or(NO_SNR_LIMIT);
                let (vals, labels) = stepper_labels_snr(cur);
                let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
                if let Some(i) = l.stepper_row("Skip stations weaker than", "Automatic calls only", &labels, idx) {
                    app.s.min_snr = if vals[i] == NO_SNR_LIMIT { None } else { Some(vals[i]) };
                }
                help(l, "Stations decoded weaker than this signal-to-noise ratio are listed as skipped and not called automatically, because a very weak station rarely hears you back. -20 dB is the default; \"No limit\" calls everything. This never stops you from answering a station yourself.");
            }
            Step::Hunting => {
                lead(l, "These choices decide which stations Auto-Hunt calls and how it calls them.");
                l.section("Which stations");
                switch(l, "Only hunt new parks", "Skip parks you have hunted before", &mut app.s.new_parks_only,
                       "When on, a station is called only if its park is new to you, according to your pota.app hunt counts. This needs your pota.app login (a later page); without it nothing is called while this is on. Off calls every spotted activator you have not worked today.");
                switch(l, "Call stations after their 73", "Also reply to a sign-off", &mut app.s.pounce_signoffs,
                       "When an activator ends one QSO with 73 or RR73, the program can call them right away, which is often the quickest way in during a pile-up. It needs WSJT-X to show every decode, not only CQs (see the WSJT-X page). Turn it off if you keep WSJT-X's CQ only filter on.");
                l.section("Transmit frequency");
                switch(l, "Move Tx to their frequency", "Off: only your receive frequency moves", &mut app.s.move_tx,
                       "Off (recommended): when the program calls a station, only WSJT-X's receive frequency moves to theirs and your transmit frequency stays where you put it. On: your transmit frequency is moved onto the station's as well. Auto CQ/Answer never moves your transmit frequency, whatever you choose here.");
            }
            Step::Cq => {
                lead(l, "Besides hunting, the program can call CQ for you when you are idle and answer the stations that call back. This is the second switch in the main window, Auto CQ/Answer. It starts off, and Auto-Hunt always takes priority over it: a station worth hunting is called over your CQ.");
                l.section("CQ message");
                let choices: Vec<(&'static str, &str)> = CQ_CHOICES.iter().map(|c| (*c, *c)).collect();
                if let Some(v) = l.segmented_row("CQ message", "Sent with your call and grid", "wiz-cq", &choices, &app.s.cq_text.clone()) {
                    app.s.cq_text = v.to_string();
                }
                help(l, "The message sent is this text, your call and your four-character grid, for example \"CQ POTA WD4DAN EM80\". CQ is a general call. CQ POTA tells hunters you are a POTA activator, and is what to use when you are in a park. CQ DX asks for distant stations only.");
                help(l, "It uses WSJT-X's own CQ, so WSJT-X's Auto Seq may answer a caller before the program does; the program follows along and logs it either way.");
            }
            Step::Pota => {
                lead(l, "pota.app shows the current spots to anyone, so the program works without a login. Signing in adds your personal hunt counts: which parks are new to you, how often you have hunted each park, and how often you have worked each activator. It also lets the program respot.");
                l.section("Sign in (optional)");
                let (mut u, mut pw) = (self.user.clone(), self.pass.clone());
                l.form(&mut [("Username", &mut u, 26.0, false), ("Password", &mut pw, 26.0, true)], 6.0, false);
                self.user = u;
                self.pass = pw;
                Panel::check_creds_row(l, app);
                help(l, "The username or email you sign in with at pota.app. Leave both blank to skip. The password is saved as plain text in settings.json on this computer, so use it only on a computer you trust. It is sent only to pota.app, to read your hunt counts.");
                l.section("Respot");
                switch(l, "Respot after each QSO", "Post the activator's spot after a logged QSO", &mut app.s.respot,
                       "After a QSO is logged, the program posts a spot for the activator to pota.app with the frequency and mode, so other hunters can find them. It needs the login above and a confirmed park, and in Dry run it only says what it would have posted.");
                let mut m = self.msg.clone();
                l.form(&mut [("Comment", &mut m, 26.0, false)], 6.0, false);
                self.msg = m;
                let list = respot::PLACEHOLDERS.iter().map(|(k, v)| format!("{k} = {v}")).collect::<Vec<_>>().join("; ");
                help(l, &format!("The comment sent with each respot (blank sends none). Placeholders are filled in from the QSO: {list}."));
            }
            Step::Logging => {
                lead(l, "WSJT-X keeps its own log. These settings control how the contacts are accepted and whether they are also sent to another logger.");
                l.section("WSJT-X's Log QSO window");
                switch(l, "Press OK for me", "Whenever the Log QSO window opens", &mut app.s.auto_log,
                       "WSJT-X records a QSO (and tells this program about it) only after you click OK in its Log QSO window. With this on, the program clicks OK for you a moment after the window opens, for any QSO, so contacts are never left unlogged. Turn it off if you want to check each QSO yourself.");
                l.section("Also log each QSO to");
                Panel::logger_picker(l, app, &mut self.dd, &mut self.lg_sel, &mut self.lg_bufs);
            }
            Step::Sounds => {
                lead(l, "Two short sounds tell you what is happening while you are looking at another window. Each can be switched off or replaced with your own file (mp3, wav, wma or m4a).");
                l.section("QSO logged");
                switch(l, "Play a sound when a QSO is logged", "Any QSO WSJT-X logs", &mut app.s.ding,
                       "A ding plays whenever WSJT-X logs a contact, whether the program started it or you did. Test plays the current sound, Choose picks your own file, Reset goes back to the built-in one.");
                let custom = app.s.ding_path.clone();
                Panel::sound_file_row(l, p, Kind::Ding, &custom);
                l.section("Wanted park");
                switch(l, "Play a sound for a wanted park", "A candidate is ready to call", &mut app.s.pop,
                       "A pop plays when the program has a station it is ready to call. With \"Only hunt new parks\" on, it plays only for parks that are new to you, so it works as an alert for the ones you want. It plays whether or not Auto-Hunt is on.");
                let custom = app.s.pop_path.clone();
                Panel::sound_file_row(l, p, Kind::Pop, &custom);
            }
            Step::RadioConn => {
                lead(l, "If your radio has a serial (COM or USB) CAT port, the program can set its receiver and transmit power for you on the next page. This is optional: with no port chosen, nothing is ever sent to the radio. The Test CAT Control button at the bottom of this page checks that the radio answers; everything here can be changed later in Settings > Radio.");
                l.section("Radio");
                if l.dropdown_row("Radio make", "Which CAT command set to use", "wiz-make", &app.s.rig_type, &cat::MAKES) {
                    self.dd = if self.dd == Some("make") { None } else { Some("make") };
                }
                if self.dd == Some("make") {
                    let labels: Vec<String> = cat::MAKES.iter().map(|m| m.to_string()).collect();
                    let cur = cat::MAKES.iter().position(|m| *m == app.s.rig_type);
                    match l.dropdown_list("wiz-make", &labels, cur) {
                        Some(Some(i)) => {
                            // a baud rate still at the old radio's default follows the new radio
                            let old = cat::resolve(&app.s.rig_type, &app.s.rig_model, &app.s.rig_civ);
                            app.s.rig_type = cat::MAKES[i].to_string();
                            app.s.rig_model = cat::model_names(cat::MAKES[i])[0].to_string();
                            let new = cat::resolve(&app.s.rig_type, &app.s.rig_model, &app.s.rig_civ);
                            if app.s.rig_baud == old.default_baud() {
                                app.s.rig_baud = new.default_baud();
                            }
                            self.dd = None;
                        }
                        Some(None) => self.dd = None,
                        None => {}
                    }
                }
                let models = cat::model_names(&app.s.rig_type);
                if l.dropdown_row("Radio model", "Most are untested; only the Yaesu FTDX101D has been tried on a real radio", "wiz-model", &app.s.rig_model, &models) {
                    self.dd = if self.dd == Some("model") { None } else { Some("model") };
                }
                if self.dd == Some("model") {
                    let labels: Vec<String> = models.iter().map(|m| m.to_string()).collect();
                    let cur = models.iter().position(|m| *m == app.s.rig_model);
                    match l.dropdown_list("wiz-model", &labels, cur) {
                        Some(Some(i)) => {
                            app.s.rig_model = models[i].to_string();
                            self.dd = None;
                        }
                        Some(None) => self.dd = None,
                        None => {}
                    }
                }
                if app.s.rig_type == "Icom" && app.s.rig_model == cat::ICOM_OTHER {
                    let mut buf = app.s.rig_civ.clone();
                    l.form(&mut [("CI-V address (hex)", &mut buf, 4.0, false)], 4.0, false);
                    app.s.rig_civ = buf;
                    help(l, "The radio's CI-V address in hex (94 is an IC-7300). Power is then assumed to be 100 W full scale and the attenuator 20 dB.");
                }
                help(l, "Pick the make, then the model. Models with the same make share one command set; the model sets the maximum transmit power and, for Icom and Xiegu, the CI-V address.");
                l.section("Serial port");
                let com = if app.s.rig_com.is_empty() { "Choose…".to_string() } else { app.s.rig_com.clone() };
                if l.dropdown_row("COM port", "The radio's CAT serial port", "wiz-com", &com, &["COM20", "Choose…"]) {
                    if self.dd == Some("com") {
                        self.dd = None;
                    } else {
                        self.ports = crate::comports::list();
                        self.dd = Some("com");
                    }
                }
                if self.dd == Some("com") {
                    let labels: Vec<String> = if self.ports.is_empty() { vec!["No COM ports found".to_string()] } else { self.ports.clone() };
                    let cur = self.ports.iter().position(|p| *p == app.s.rig_com);
                    match l.dropdown_list("wiz-com", &labels, cur) {
                        Some(Some(i)) => {
                            if let Some(port) = self.ports.get(i) {
                                app.s.rig_com = port.clone();
                            }
                            self.dd = None;
                        }
                        Some(None) => self.dd = None,
                        None => {}
                    }
                }
                const BAUDS: [u32; 6] = [4800, 9600, 19200, 38400, 57600, 115200];
                let labels: Vec<String> = BAUDS.iter().map(|b| b.to_string()).collect();
                let wide: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                if l.dropdown_row("Baud rate", "Must match the radio's CAT speed", "wiz-baud", &app.s.rig_baud.to_string(), &wide) {
                    self.dd = if self.dd == Some("baud") { None } else { Some("baud") };
                }
                if self.dd == Some("baud") {
                    let cur = BAUDS.iter().position(|b| *b == app.s.rig_baud);
                    match l.dropdown_list("wiz-baud", &labels, cur) {
                        Some(Some(i)) => {
                            app.s.rig_baud = BAUDS[i];
                            self.dd = None;
                        }
                        Some(None) => self.dd = None,
                        None => {}
                    }
                }
                let bits = app.s.rig_data_bits.to_string();
                if let Some(v) = l.segmented_row("Data bits", "", "wiz-bits", &[("7", "7"), ("8", "8")], &bits) {
                    app.s.rig_data_bits = v.parse().unwrap_or(8);
                }
                let stop = app.s.rig_stop_bits.to_string();
                if let Some(v) = l.segmented_row("Stop bits", "", "wiz-stop", &[("1", "1"), ("2", "2")], &stop) {
                    app.s.rig_stop_bits = v.parse().unwrap_or(1);
                }
                let hs = app.s.rig_handshake.clone();
                if let Some(v) = l.segmented_row("Handshake", "", "wiz-handshake", &[("none", "None"), ("xonxoff", "XON/XOFF"), ("hardware", "Hardware")], &hs) {
                    app.s.rig_handshake = v.to_string();
                }
                let testing = app.cat_testing;
                let b = l.small_button(if testing { "Testing…" } else { "Test CAT Control" });
                let sz = b.size(l.ui);
                l.y += 12.0;
                if l.paint {
                    let r = Rect::from_min_size(Pos2::new(l.left, l.y), sz);
                    if l.button(&b, r, Id::unique("wiz-test-cat"), !testing && !app.s.rig_com.is_empty()) {
                        l.act.push(Do::TestCat);
                    }
                }
                l.y += sz.y + 4.0;
                help(l, "Test CAT Control briefly changes NB, NR, ATT and power, checks that the radio confirms each, puts them back and shows the result. It needs a COM port above and uses the choices on this page. It is refused while WSJT-X is transmitting.");
                help(l, "The baud rate must match the radio's own CAT setting (Yaesu FTDX101D over USB: 38400; Kenwood menu default: 9600; Icom: 19200 or Auto). Eight data bits, one stop bit and no handshake suit almost every radio; change them only if your radio's manual says so. A radio on a USB cable appears as a COM port once its driver is installed.");
            }
            Step::Radio => {
                lead(l, "These settings go to the radio chosen on the previous page: the program sends the CAT commands straight to its COM port and reads them back to confirm. Until a port is chosen nothing is sent. If you have no CAT-controlled radio, switch the first one off and leave the power settings on Leave.");
                l.section("Receiver");
                switch(l, "Keep ATT, NB and NR off", "Sent at start-up and after each band change", &mut app.s.rig_clean,
                       "Attenuator, noise blanker and noise reduction hurt weak-signal digital decoding, and the radio may remember them for each band. When on, the program turns all three off for the main receiver two seconds after it starts and again after every band or mode change.");
                l.section("Transmit power");
                let (vals, labels) = stepper_labels_power();
                let cur = app.s.rig_power.unwrap_or(0);
                let idx = vals.iter().position(|v| *v == cur).unwrap_or(0);
                if let Some(i) = l.stepper_row("Tx power at start-up and QSY", "5 to 100 W, or Leave", &labels, idx) {
                    app.s.rig_power = (vals[i] != 0).then_some(vals[i]);
                }
                help(l, "Digital modes should run at modest power, and a hunter does not need much. The power is set when the program starts and after each band or mode change. Leave sends nothing.");
                l.section("Restore");
                switch(l, "Restore the radio's settings", "Put back as they were before each QSY and on close", &mut app.s.rig_restore,
                       "When on, NB, NR, ATT and Tx power are restored to the settings the radio had before the program changed them: before the program QSYs to another band and when it closes.");
            }
            Step::Review => {
                lead(l, "This is what will be saved. Go Back to change anything, or press Finish.");
                l.section("Your choices");
                let on = |b: bool| if b { "on" } else { "off" };
                let s = &app.s;
                let theme = match s.theme.as_str() {
                    "light" => "Light",
                    "dark" => "Dark",
                    _ => "Match Windows",
                };
                let snr = s.min_snr.map_or("no limit".to_string(), |d| format!("{d:+} dB"));
                let give = ATTEMPT_MODES
                    .iter()
                    .map(|m| format!("{m} {}", s.attempts_by_mode.get(*m).copied().unwrap_or(s.max_attempts)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let rows: Vec<(&str, String)> = vec![
                    ("Appearance", format!("{theme}, {} pt text", s.font_size)),
                    ("WSJT-X", format!("listening on {}:{}", s.udp_group, s.udp_port)),
                    ("Dry run", on(s.dry_run).to_string()),
                    ("Give up after", give),
                    ("Skip weaker than", snr),
                    ("Hunting", format!("new parks only {}, after 73 {}, move Tx {}", on(s.new_parks_only), on(s.pounce_signoffs), on(s.move_tx))),
                    ("Auto CQ message", s.cq_text.clone()),
                    ("pota.app", if s.pota_user.is_empty() { "no login".to_string() } else { format!("{}, respot {}", s.pota_user, on(s.respot)) }),
                    ("Logging", {
                        let mut others: Vec<String> = Vec::new();
                        if s.aclog_enabled {
                            others.push(format!("ACLog ({}:{})", s.aclog_host, s.aclog_port));
                        }
                        for k in crate::loggers::KINDS {
                            let c = s.logger(k);
                            if c.enabled {
                                others.push(format!("{} ({}:{})", k.name(), c.host, c.port));
                            }
                        }
                        let to = if others.is_empty() { "no other logger".to_string() } else { others.join(", ") };
                        format!("press OK {}, {to}", on(s.auto_log))
                    }),
                    ("Sounds", format!("QSO logged {}, wanted park {}", on(s.ding), on(s.pop))),
                    ("Radio connection", if s.rig_com.is_empty() { format!("{} {}, no port chosen (nothing will be sent)", s.rig_type, s.rig_model) } else { format!("{} {} on {}, {} baud", s.rig_type, s.rig_model, s.rig_com, s.rig_baud) }),
                    ("Radio", format!("ATT/NB/NR off {}, restore {}, Tx power {}", on(s.rig_clean), on(s.rig_restore), s.rig_power.map_or("left alone".to_string(), |w| format!("{w} W")))),
                ];
                for (title, value) in rows {
                    l.row(title, &value, 0.0, 0.0, p.text);
                }
                let dry = app.s.dry_run;
                l.y += 4.0;
                if dry {
                    help(l, "Dry run is on, so nothing will be transmitted until you switch it off in Settings > Hunting.");
                } else {
                    l.paragraph("Dry run is off: once you enable Auto-Hunt or Auto CQ/Answer the program will transmit. Check your settings again if that is not what you want.", &f.small, p.warn, l.right - l.left - 2.0, 0.0, 8.0);
                }
                error_line(l, &self.error.clone());
            }
        }
    }
}
