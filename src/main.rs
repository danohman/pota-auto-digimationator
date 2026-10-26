//! POTA Auto-Digimationator: calls POTA activators in WSJT-X automatically.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod aclog;
mod app;
mod cycles;
mod dialogs;
mod events;
mod hunter;
mod logdialog;
mod logview;
mod panel;
mod popup;
mod pota_auth;
mod pota_spots;
mod presence;
mod settings;
mod sound;
mod table;
mod textsel;
mod theme;
mod ui;
mod wsjtx_gui;
mod wsjtx_udp;

use std::path::PathBuf;

use eframe::egui;

use crate::app::{App, CQ_ON_TEXT, HUNT_ON_TEXT, LOG_COLUMNS, PAD, PARK_SAMPLE, TITLE};
use crate::theme::Fonts;
use crate::ui::measure;

/// The app folder, holding assets/ding.mp3 and the data folder (settings.json, state.json, spots_cache.json): the exe's folder, or the
/// first folder above it holding assets/ding.mp3 (so a build in target\release finds
/// the project root).
fn app_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("POTA_APP_DIR") {
        return PathBuf::from(d); // a separate folder, e.g. for testing
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let start = exe.parent().map(PathBuf::from).unwrap_or_default();
    let mut d = Some(start.as_path());
    while let Some(dir) = d {
        if dir.join("assets").join("ding.mp3").is_file() {
            return dir.to_path_buf();
        }
        d = dir.parent();
    }
    start
}

/// A small green tree as the window icon.
fn icon() -> egui::IconData {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32, y as f32);
            let cx = fx.clamp(7.0, size as f32 - 8.0);
            let cy = fy.clamp(7.0, size as f32 - 8.0);
            let inside = (fx - cx).powi(2) + (fy - cy).powi(2) <= 49.0;
            let tree = ((5.0..=16.0).contains(&fy) && (fx - 15.5).abs() <= (fy - 4.0) * 0.75)
                || ((11.0..=24.0).contains(&fy) && (fx - 15.5).abs() <= (fy - 9.0) * 0.75)
                || (fy > 24.0 && fy <= 28.0 && (14.0..=17.0).contains(&fx));
            let i = (y * size + x) * 4;
            let c: [u8; 4] = if inside && tree {
                [255, 255, 255, 255]
            } else if inside {
                [0x0d, 0x8a, 0x5c, 255]
            } else {
                [0, 0, 0, 0]
            };
            rgba[i..i + 4].copy_from_slice(&c);
        }
    }
    egui::IconData { rgba, width: size as u32, height: size as u32 }
}

fn table_fit(f: &Fonts, cols: &[(&str, &str)]) -> f32 {
    cols.iter()
        .map(|(h, sample)| measure(&f.table_heading, &h.to_uppercase()).max(measure(&f.ui, sample)) + table::COL_GAP)
        .sum::<f32>()
        + 2.0 * table::PADX
}

/// The window geometry remembered from last time ("WxH+X+Y"), or None when it is missing
/// or would put the window off every monitor (one was unplugged).
fn saved_geometry(s: &str) -> Option<(i32, i32, i32, i32)> {
    let re = regex::Regex::new(r"^(\d+)x(\d+)([+-]-?\d+)([+-]-?\d+)$").unwrap();
    let c = re.captures(s)?;
    let num = |g: &str| g.replace('+', "").parse::<i32>().ok();
    let (w, h, x, y) = (num(&c[1])?, num(&c[2])?, num(&c[3])?, num(&c[4])?);
    let (left, top, vw, vh) = theme::virtual_desktop();
    // The title bar must be reachable: its middle inside the desktop.
    if !(left <= x + w / 2 && x + w / 2 < left + vw && top <= y && y < top + vh - 40) {
        return None;
    }
    Some((w, h, x, y))
}

fn main() -> eframe::Result {
    let dir = app_dir();
    let (fonts, defs) = Fonts::load();
    let s = settings::load(&settings::data_dir(&dir).join("settings.json"));
    let scale = unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() } as f32 / 96.0;
    let scale = if scale > 0.0 { scale } else { 1.0 };

    // Open wide enough that nothing is clipped or wrapped (within the screen).
    let f = &fonts;
    let scrollbar = 14.0;
    let log_w = measure(&f.mono, &"0".repeat(LOG_COLUMNS)) + 2.0 * PAD + scrollbar + 2.0;
    let spot = [("Heard", "00:00"), ("Ago", "99m"), ("Call", "AE4XO/P (999)"), ("kHz", "28074.0")];
    let spots_fit = table_fit(f, &[&spot[..], &[("Park", PARK_SAMPLE)]].concat());
    let done_fit = table_fit(f, &[&spot[..], &[("W/S", "W/S"), ("Park", PARK_SAMPLE)]].concat());
    let worked_fit = table_fit(f, &[("Time", "00:00"), ("Call", "JN1JYD/P2"), ("Band", "160m"), ("Mode", "FT8"), ("Log", "Log"), ("Park", PARK_SAMPLE)]);
    let right_w = spots_fit.max(done_fit).max(worked_fit) + scrollbar + 2.0;
    let titles = measure(&f.title, TITLE).max(measure(&f.small, "Calls Parks on the Air activators through WSJT-X")) + 4.0;
    let toggle = |t: &str| measure(&f.toggle, t) + 20.0;
    let icon_btn = measure(&f.icon, theme::ICON_SETTINGS) + 22.0;
    let header = 32.0 + 36.0 + 12.0 + titles + icon_btn + icon_btn + 8.0
        + toggle(&format!("{HUNT_ON_TEXT} (Dry run)")) + 16.0 + toggle(&format!("{CQ_ON_TEXT} (Dry run)")) + 10.0;
    let dot = (f.ui.m.linespace * 0.42).round();
    let dot_w = dot + 6f32.max((dot / 2.0).floor());
    let chip = |t: &str| measure(&f.ui, t) + 24.0 + dot_w;
    let strip = 2.0 * PAD + chip("WSJT-X: waiting for packets...") + 8.0 + chip("Spots (pota.app): waiting for first fetch");
    let bars = header.max(strip);
    // The narrowest the window can be: logo, the compact header buttons, and a little
    // room for the two panes (the chips wrap, the title hides).
    let min_width = 32.0 + 36.0 + 12.0 + icon_btn + icon_btn + 8.0
        + toggle("Auto CQ/Answer Enabled") + 10.0 + toggle("Auto-Hunt Disabled") + 16.0 + 10.0 + 2.0 * PAD;
    let (sw, sh) = theme::screen_size();
    let (sw, sh) = (sw as f32 / scale, sh as f32 / scale);
    let width = bars.max(log_w + right_w + 2.0 * PAD + 10.0).min(sw - 40.0);
    let height = 760f32.min(sh - 80.0);
    let mut vp = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_icon(icon())
        .with_inner_size([width, height])
        .with_min_inner_size([min_width.min(width), 480.0]);
    if let Some((w, h, x, y)) = s.window.as_deref().and_then(saved_geometry) {
        vp = vp.with_inner_size([(w as f32 / scale).max(min_width), (h as f32 / scale).max(480.0)]).with_position([x as f32 / scale, y as f32 / scale]);
    }
    let options = eframe::NativeOptions { viewport: vp, centered: s.window.is_none(), ..Default::default() };
    let channel = std::sync::mpsc::channel();
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, dir, (fonts, defs), channel, right_w)))),
    )
}
