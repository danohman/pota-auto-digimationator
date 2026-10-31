// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The debug window (a click on the app icon in the header): version, Windows, CPU and
//! memory (the machine's and this process's), the graphics backend, frame rate, caches and
//! the state of the connections and the hunter, refreshed every second, with a button that
//! copies it all as text for a bug report. A real window like the dialogs, blocking the
//! main window until closed. A child module of `app` so it can reach the App's private state.

use std::time::{Duration, Instant};

use eframe::egui::{self, Id, Key, Pos2, Rect, Sense, Ui, UiBuilder, Vec2, ViewportBuilder, ViewportId};

use super::{refresh_frame, App};
use crate::hunter::Tag;
use crate::sysinfo::{self, CpuSampler};
use crate::theme;
use crate::ui::{self, Rnd};

const TITLE: &str = "Debug info";
const SIZE: Vec2 = Vec2::new(700.0, 720.0);
const LABEL_W: f32 = 230.0;

#[derive(Clone)]
enum Row {
    Section(String),
    Item(String, String),
}

pub(super) struct DebugWin {
    pos: Option<Pos2>,
    size: Vec2,
    hwnd: isize,
    titled: Option<bool>,
    rows: Vec<Row>,
    next: Instant,
    sampler: CpuSampler,
    /// the OS and CPU names never change, so they are read once
    os: String,
    cpu: String,
    opened: Instant,
    /// the process's private bytes when the window opened, to show growth
    start_private: Option<u64>,
    /// (egui pass number, when) at the last refresh, for the frame rate
    last_pass: Option<(u64, Instant)>,
    fps: f64,
    copied_until: Option<Instant>,
}

impl DebugWin {
    pub fn new() -> Self {
        DebugWin {
            pos: None,
            size: SIZE,
            hwnd: 0,
            titled: None,
            rows: Vec::new(),
            next: Instant::now(),
            sampler: CpuSampler::new(),
            os: sysinfo::os_version(),
            cpu: sysinfo::cpu_name(),
            opened: Instant::now(),
            start_private: sysinfo::proc_mem().map(|m| m.private),
            last_pass: None,
            fps: 0.0,
            copied_until: None,
        }
    }
}

/// What the app was built from and on, for the Graphics lines (read once at start-up).
pub(super) fn graphics_rows(rs: &eframe::egui_wgpu::RenderState, setting: &str) -> Vec<(String, String)> {
    let i = rs.adapter.get_info();
    let mut v = vec![
        ("Adapter".to_string(), i.name.clone()),
        ("Backend".to_string(), format!("{:?}", i.backend)),
        ("Device type".to_string(), format!("{:?}", i.device_type)),
        ("Driver".to_string(), format!("{} {}", i.driver, i.driver_info).trim().to_string()),
        ("Vendor / device id".to_string(), format!("{:#06x} / {:#06x}", i.vendor, i.device)),
        ("Backend setting".to_string(), format!("{} (POTA_GPU: {})", if setting.is_empty() { "automatic" } else { setting }, std::env::var("POTA_GPU").unwrap_or_else(|_| "not set".into()))),
    ];
    let others: Vec<String> = rs.available_adapters.iter().map(|a| { let i = a.get_info(); format!("{} ({:?})", i.name, i.backend) }).collect();
    v.push(("Adapters found".to_string(), others.join(", ")));
    v
}

fn item(v: &mut Vec<Row>, k: &str, val: impl Into<String>) {
    v.push(Row::Item(k.to_string(), val.into()));
}

fn section(v: &mut Vec<Row>, name: &str) {
    v.push(Row::Section(name.to_string()));
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn text_of(rows: &[Row]) -> String {
    let mut out = String::new();
    for r in rows {
        match r {
            Row::Section(s) => out.push_str(&format!("\n== {s} ==\n")),
            Row::Item(k, v) => out.push_str(&format!("{k}: {v}\n")),
        }
    }
    out.trim_start().to_string()
}

impl App {
    /// Collect every line of the debug window.
    fn debug_rows(&self, ctx: &egui::Context, st: &mut DebugWin) -> Vec<Row> {
        st.sampler.sample();
        let mut v = Vec::new();
        let now = self.now();
        let s = &self.s;

        section(&mut v, "Program");
        item(&mut v, "Version", format!("{} ({})", env!("CARGO_PKG_VERSION"), if cfg!(debug_assertions) { "debug build" } else { "release build" }));
        item(&mut v, "Running for", sysinfo::duration(self.t0.elapsed().as_secs_f64()));
        item(&mut v, "Executable", std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default());
        item(&mut v, "Data folder", self.dir.display().to_string());
        item(&mut v, "Update", match (&self.update, &self.update_note) {
            (Some(r), _) => format!("version {} is available", r.version),
            (None, Some((n, _))) => n.clone(),
            _ => if self.update_ok { "up to date".to_string() } else { "not checked yet".to_string() },
        });

        section(&mut v, "Windows");
        item(&mut v, "Version", st.os.clone());
        item(&mut v, "Processor", st.cpu.clone());
        item(&mut v, "Windows uptime", sysinfo::duration(sysinfo::os_uptime()));
        item(&mut v, "CPU in use (all programs)", format!("{:.1}%", st.sampler.system));
        if let Some(m) = sysinfo::sys_mem() {
            item(&mut v, "Memory in use", format!("{}% ({} free of {})", m.load, sysinfo::mb(m.avail), sysinfo::mb(m.total)));
            item(&mut v, "Commit charge", format!("{} of {}", sysinfo::mb(m.commit_used), sysinfo::mb(m.commit_limit)));
        }

        section(&mut v, "This program");
        item(&mut v, "CPU", format!("{:.1}% of all {} logical processors", st.sampler.app, sysinfo::logical_cpus()));
        if let Some(m) = sysinfo::proc_mem() {
            item(&mut v, "Memory (working set)", format!("{} (peak {})", sysinfo::mb(m.working_set), sysinfo::mb(m.peak_working_set)));
            let grew = st.start_private.map(|p0| format!(", {:+.1} MB since this window opened {} ago", (m.private as f64 - p0 as f64) / 1_048_576.0, sysinfo::duration(st.opened.elapsed().as_secs_f64()))).unwrap_or_default();
            item(&mut v, "Memory (private bytes)", format!("{} (peak {}){grew}", sysinfo::mb(m.private), sysinfo::mb(m.peak_commit)));
            item(&mut v, "Page faults", m.page_faults.to_string());
        }
        let (handles, gdi, user, threads) = sysinfo::proc_objects();
        item(&mut v, "Handles / GDI / USER objects", format!("{handles} / {gdi} / {user}"));
        item(&mut v, "Threads", threads.to_string());

        section(&mut v, "Graphics");
        for (k, val) in &self.gfx {
            item(&mut v, k, val.clone());
        }
        let (tex_n, tex_bytes, runs_n) = {
            let tm = ctx.tex_manager();
            let tm = tm.read();
            let (mut n, mut bytes, mut runs) = (0usize, 0usize, 0usize);
            for (_, m) in tm.allocated() {
                n += 1;
                bytes += m.size[0] * m.size[1] * m.bytes_per_pixel;
                if m.name == "gdi-run" {
                    runs += 1;
                }
            }
            (n, bytes, runs)
        };
        item(&mut v, "Frames per second", format!("{:.1}", st.fps));
        item(&mut v, "Display scale / window", {
            let r = ctx.content_rect();
            format!("{:.2}x, {:.0} x {:.0} points", ctx.pixels_per_point(), r.width(), r.height())
        });
        item(&mut v, "Textures", format!("{tex_n} ({} MB), {runs_n} of them text runs", format_args!("{:.1}", tex_bytes as f64 / 1_048_576.0)));
        let (runs, positions) = ui::cache_sizes();
        item(&mut v, "Text caches", format!("{runs} runs, {positions} measured strings"));
        item(&mut v, "Theme / text size", format!("{} / {} pt", self.theme_name, s.font_size));

        section(&mut v, "WSJT-X");
        item(&mut v, "Connected", format!("{} (last message {})", yes(self.connected()), self.last_wsjtx.map(|t| format!("{:.0} s ago", t.elapsed().as_secs_f64())).unwrap_or_else(|| "never".into())));
        item(&mut v, "Listening on", format!("{}:{}", s.udp_group, s.udp_port));
        if let Some(st) = &self.h.status {
            item(&mut v, "Instance / config", format!("{} / {}", st.id, st.config_name));
            item(&mut v, "Dial / mode", format!("{:.3} MHz, {} ({} s periods)", st.dial_freq as f64 / 1e6, st.mode, st.tr_period));
            item(&mut v, "Tx enabled / transmitting / decoding", format!("{} / {} / {}", yes(st.tx_enabled), yes(st.transmitting), yes(st.decoding)));
            item(&mut v, "Station", format!("{} {}", st.de_call, st.de_grid));
        }

        section(&mut v, "pota.app and logging");
        item(&mut v, "Spots", format!("{} (last fetch {})", self.spots_text, match &self.h.spots {
            Some(t) => format!("{} ago{}", sysinfo::duration(crate::pota_spots::epoch_now() - t.fetched_at), if self.spots_failed { ", the latest try failed" } else { "" }),
            None => "none yet".into(),
        }));
        item(&mut v, "Hunt counts", format!("{}{}", self.pota_status, if self.pota_failed { " (failed)" } else { "" }));
        item(&mut v, "ACLog", if s.aclog_enabled { format!("on, {}:{}", s.aclog_host, s.aclog_port) } else { "off".into() });
        item(&mut v, "Radio (CAT)", format!("{} {} on {} at {} baud; clean {}, restore {}", s.rig_type, s.rig_model, if s.rig_com.is_empty() { "no port" } else { &s.rig_com }, s.rig_baud, yes(s.rig_clean), yes(s.rig_restore)));

        section(&mut v, "Hunter");
        item(&mut v, "Auto-Hunt / Auto CQ / dry run", format!("{} / {} / {}", yes(self.h.enabled), yes(self.h.cq_enabled), yes(self.h.dry_run)));
        item(&mut v, "Target", self.h.target().map(|t| format!("{} on {} {}, attempt {}, {:.0} s", t.call, t.band, t.mode, t.attempts, now - t.started)).unwrap_or_else(|| "none".into()));
        item(&mut v, "Candidates waiting", self.h.pending.len().to_string());
        item(&mut v, "Remembered", format!("{} called us, {} cooling off, {} gave up, {} halted, {} skipped, {} recent targets", self.h.called_us.len(), self.h.cooloff.len(), self.h.gave_up.len(), self.h.halted.len(), self.h.skipped.len(), self.h.recent.len()));
        item(&mut v, "Hidden programs", s.hidden_programs.len().to_string());
        item(&mut v, "Activity log lines / timers", format!("{} / {}", self.log.lines.len(), self.timers.len()));

        let errors: Vec<&crate::logview::Line> = self.log.lines.iter().filter(|l| l.tag == Tag::Error).collect();
        section(&mut v, "Recent errors (newest last)");
        if errors.is_empty() {
            item(&mut v, "None", "");
        }
        for l in errors.iter().skip(errors.len().saturating_sub(8)) {
            item(&mut v, &l.ts, l.text.clone());
        }
        v
    }

    /// Draw the debug window, when open.
    pub(super) fn draw_debug(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.debug.take() else { return };
        if Instant::now() >= st.next {
            let pass = ctx.cumulative_pass_nr();
            if let Some((p0, t0)) = st.last_pass {
                st.fps = (pass - p0) as f64 / t0.elapsed().as_secs_f64().max(0.001);
            }
            st.last_pass = Some((pass, Instant::now()));
            st.rows = self.debug_rows(ctx, &mut st);
            st.next = Instant::now() + Duration::from_secs(1);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        if st.pos.is_none() {
            // wide enough for the longest path, as far as the monitor allows
            let paths = st.rows.iter().filter_map(|r| match r {
                Row::Item(k, v) if k == "Executable" || k == "Data folder" => Some(ui::measure(&self.f.ui, v)),
                _ => None,
            });
            let need = LABEL_W + paths.fold(0.0, f32::max) + 2.0 * 20.0 + 2.0 + 12.0 + 28.0;
            let monitor = ctx.input(|i| i.viewport().monitor_size).map_or(1600.0, |m| m.x);
            st.size = Vec2::new(need.max(SIZE.x).min((monitor - 40.0).max(SIZE.x)), SIZE.y);
            let main = ctx.input(|i| i.viewport().inner_rect).unwrap_or(Rect::from_min_size(Pos2::ZERO, Vec2::new(1600.0, 800.0)));
            st.pos = Some(Pos2::new((main.left() + (main.width() - st.size.x) / 2.0).max(0.0).round(), (main.top() + (main.height() - st.size.y) / 3.0).max(0.0).round()));
        }
        let builder = ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(st.size)
            .with_min_inner_size(Vec2::new(480.0, 360.0))
            .with_position(st.pos.unwrap())
            .with_minimize_button(false)
            .with_maximize_button(false);
        let (p, f) = (self.p.clone(), self.f.clone());
        let dark = self.theme_name == "dark";
        let (mut close, mut copy) = (false, false);
        let copied = st.copied_until.is_some_and(|t| Instant::now() < t);
        let rows = st.rows.clone();
        ctx.show_viewport_immediate(ViewportId::from_hash_of("debug-info"), builder, |ui: &mut Ui, _class| {
            let full = ui.ctx().content_rect();
            let painter = ui.painter().clone();
            painter.rect_filled(full, 0.0, p.surface);
            let x0 = 20.0;
            let right = full.right() - 20.0;
            ui::text(ui, &painter, Pos2::new(x0, 16.0), TITLE, &f.heading, p.text);
            let sub_y = 16.0 + f.heading.m.linespace + 4.0;
            ui::text(ui, &painter, Pos2::new(x0, sub_y), "Updates every second. Copy it into a bug report.", &f.small, p.muted);
            let top = sub_y + f.small.m.linespace + 12.0;
            let bottom_h = 56.0;
            let list = Rect::from_min_max(Pos2::new(x0, top), Pos2::new(right, full.bottom() - bottom_h));
            painter.rect_filled(list, ui::radius(6.0), p.surface2);
            painter.rect_stroke(list, ui::radius(6.0), egui::Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
            ui.scope_builder(UiBuilder::new().max_rect(list.shrink(1.0)), |ui| {
                egui::ScrollArea::vertical().id_salt("debug-rows").auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    let value_w = (ui.available_width() - LABEL_W - 28.0).max(120.0);
                    ui.add_space(6.0);
                    for r in &rows {
                        match r {
                            Row::Section(name) => {
                                ui.add_space(8.0);
                                let (rr, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), f.ui_bold.m.linespace + 4.0), Sense::hover());
                                ui::text(ui, ui.painter(), Pos2::new(rr.left() + 12.0, rr.top() + 2.0), name, &f.ui_bold, p.accent);
                            }
                            Row::Item(k, val) => {
                                let lines = ui::wrap_lines(&f.ui, val, value_w);
                                let h = lines.len().max(1) as f32 * f.ui.m.linespace + 3.0;
                                let (rr, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
                                ui::text(ui, ui.painter(), Pos2::new(rr.left() + 12.0, rr.top() + 1.0), k, &f.ui, p.muted);
                                for (i, l) in lines.iter().enumerate() {
                                    ui::text(ui, ui.painter(), Pos2::new(rr.left() + 12.0 + LABEL_W, rr.top() + 1.0 + i as f32 * f.ui.m.linespace), l, &f.ui, p.text);
                                }
                            }
                        }
                    }
                    ui.add_space(8.0);
                });
            });
            let mid = full.bottom() - bottom_h / 2.0;
            let label = if copied { "Copied!" } else { "Copy to clipboard" };
            let cb = Rnd::new(label, &f.ui_bold).pill().pad(18.0, 7.0).colors(p.accent, p.accent_text).hover(p.accent_hover);
            let cs = cb.size(ui);
            if cb.show(ui, Id::unique("dbg-copy"), Rect::from_min_size(Pos2::new(x0, mid - cs.y / 2.0), cs), true).clicked() {
                copy = true;
            }
            let xb = Rnd::new("Close", &f.ui_bold).pill().pad(22.0, 7.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover);
            let xs = xb.size(ui);
            if xb.show(ui, Id::unique("dbg-close"), Rect::from_min_size(Pos2::new(right - xs.x, mid - xs.y / 2.0), xs), true).clicked() {
                close = true;
            }
            if ui.input(|i| i.key_pressed(Key::Escape) || i.viewport().close_requested()) {
                close = true;
            }
        });
        if copy {
            ctx.copy_text(text_of(&st.rows));
            st.copied_until = Some(Instant::now() + Duration::from_secs(2));
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
            self.debug = Some(st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_text_has_sections_and_items() {
        let rows = vec![Row::Section("Program".into()), Row::Item("Version".into(), "1.0".into()), Row::Section("Windows".into()), Row::Item("Build".into(), "1".into())];
        assert_eq!(text_of(&rows), "== Program ==\nVersion: 1.0\n\n== Windows ==\nBuild: 1\n");
    }
}
