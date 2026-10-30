// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Light and dark palettes, fonts (the same Windows fonts Tk picks) and the Win32
//! window touches (dark title bar, rounded corners, monitor work areas).

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui::{Color32, FontData, FontDefinitions, FontFamily, FontId};
use eframe::epaint::text::{FontTweak, VariationCoords};
use skrifa::raw::TableProvider;
use skrifa::MetadataProvider;

pub fn hex(s: &str) -> Color32 {
    let v = u32::from_str_radix(&s[1..], 16).unwrap_or(0);
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub fn mix(a: Color32, b: Color32, k: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogTag {
    Info,
    Cand,
    Call,
    Worked,
    Dupe,
    Summary,
    Skip,
    Error,
    Hunt,
    Ts,
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub surface: Color32,
    pub surface2: Color32,
    /// card header band
    pub head: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_text: Color32,
    pub on: Color32,
    pub on_hover: Color32,
    pub off: Color32,
    pub off_hover: Color32,
    pub neutral: Color32,
    pub neutral_hover: Color32,
    pub neutral_text: Color32,
    pub link: Color32,
    pub blue: Color32,
    pub blue_hover: Color32,
    pub blue_text: Color32,
    pub select: Color32,
    pub dim_row: Color32,
    pub dim_text: Color32,
    pub glow: (Color32, Color32),
    pub heard: (Color32, Color32),
    /// the same pulse for a heard spot at a NEW park (orange) or an ATNO park (purple)
    pub heard_new: (Color32, Color32),
    pub heard_atno: (Color32, Color32),
    /// pulse of a new-parks line (a park new to us appeared, or its newest spot got newer); apart from the orange `new` text
    pub notice: (Color32, Color32),
    /// the same pulse for a new-parks line with an ATNO park (purple)
    pub notice_atno: (Color32, Color32),
    pub ok: Color32,
    pub bad: Color32,
    pub warn: Color32,
    pub new: Color32,
    pub new_text: Color32,
    pub atno: Color32,
    pub atno_text: Color32,
    log: [Color32; 10],
    pub disabled_btn: (Color32, Color32),
}

impl Palette {
    pub fn log(&self, t: LogTag) -> Color32 {
        self.log[t as usize]
    }

    pub fn light() -> Self {
        Palette {
            dark: false,
            bg: hex("#eef1f5"),
            surface: hex("#ffffff"),
            surface2: hex("#f5f7fa"),
            head: hex("#d3e6da"),
            border: hex("#dde2e9"),
            text: hex("#1c2330"),
            muted: hex("#6a7484"),
            faint: hex("#a3abb8"),
            accent: hex("#0d8a5c"),
            accent_hover: hex("#0b9e68"),
            accent_text: hex("#ffffff"),
            on: hex("#14945f"),
            on_hover: hex("#18a96d"),
            off: hex("#d23c3c"),
            off_hover: hex("#e05252"),
            neutral: hex("#e7ebf0"),
            neutral_hover: hex("#dbe0e7"),
            neutral_text: hex("#2a3342"),
            link: hex("#1a5fd0"),
            blue: hex("#1a5fd0"),
            blue_hover: hex("#2b73e6"),
            blue_text: hex("#ffffff"),
            select: hex("#cfe3ff"),
            dim_row: hex("#eef0f3"),
            dim_text: hex("#8b94a3"),
            glow: (Color32::from_rgb(252, 214, 214), Color32::from_rgb(238, 140, 140)),
            heard: (Color32::from_rgb(214, 242, 222), Color32::from_rgb(120, 214, 150)),
            heard_new: (Color32::from_rgb(255, 240, 214), Color32::from_rgb(250, 190, 110)),
            heard_atno: (Color32::from_rgb(240, 228, 255), Color32::from_rgb(196, 160, 244)),
            notice: (Color32::from_rgb(255, 240, 214), Color32::from_rgb(252, 214, 160)),
            notice_atno: (Color32::from_rgb(244, 235, 255), Color32::from_rgb(216, 192, 248)),
            ok: hex("#14945f"),
            bad: hex("#d23c3c"),
            warn: hex("#c77700"),
            new: hex("#e8710a"),
            new_text: hex("#ffffff"),
            atno: hex("#7e3fc4"),
            atno_text: hex("#ffffff"),
            log: [
                hex("#3d4757"), hex("#1f5fbf"), hex("#c25a00"), hex("#13804a"), hex("#8b94a3"),
                hex("#8b94a3"), hex("#7a3fb0"), hex("#c62828"), hex("#c62828"), hex("#a3abb8"),
            ],
            disabled_btn: (hex("#e7ebf0"), hex("#a3abb8")),
        }
    }

    pub fn dark() -> Self {
        Palette {
            dark: true,
            bg: hex("#14171c"),
            surface: hex("#1b1f26"),
            surface2: hex("#222731"),
            head: hex("#264434"),
            border: hex("#2c323d"),
            text: hex("#e4e8ee"),
            muted: hex("#8a94a5"),
            faint: hex("#5d6676"),
            accent: hex("#2fbf7f"),
            accent_hover: hex("#45d192"),
            accent_text: hex("#0d1512"),
            on: hex("#1f9d63"),
            on_hover: hex("#27b371"),
            off: hex("#c03a3a"),
            off_hover: hex("#d44c4c"),
            neutral: hex("#2a303b"),
            neutral_hover: hex("#343b48"),
            neutral_text: hex("#dfe4eb"),
            link: hex("#6cb0ff"),
            blue: hex("#2f6fdb"),
            blue_hover: hex("#4585ee"),
            blue_text: hex("#ffffff"),
            select: hex("#2c4a73"),
            dim_row: hex("#191c22"),
            dim_text: hex("#6a7382"),
            glow: (Color32::from_rgb(72, 28, 28), Color32::from_rgb(150, 48, 48)),
            heard: (Color32::from_rgb(24, 62, 38), Color32::from_rgb(40, 130, 70)),
            heard_new: (Color32::from_rgb(62, 40, 18), Color32::from_rgb(140, 84, 20)),
            heard_atno: (Color32::from_rgb(44, 30, 66), Color32::from_rgb(100, 60, 160)),
            notice: (Color32::from_rgb(62, 40, 18), Color32::from_rgb(112, 66, 20)),
            notice_atno: (Color32::from_rgb(44, 30, 66), Color32::from_rgb(82, 52, 128)),
            ok: hex("#3ccf8a"),
            bad: hex("#ff6b6b"),
            warn: hex("#f0a840"),
            new: hex("#ff9a3c"),
            new_text: hex("#1a1208"),
            atno: hex("#b48cf0"),
            atno_text: hex("#140c20"),
            log: [
                hex("#c4cad4"), hex("#6cb0ff"), hex("#ffa94d"), hex("#4fd394"), hex("#6a7382"),
                hex("#6a7382"), hex("#c39bff"), hex("#ff6b6b"), hex("#ff6b6b"), hex("#5d6676"),
            ],
            disabled_btn: (hex("#262b34"), hex("#5d6676")),
        }
    }

    pub fn for_name(name: &str) -> Self {
        if name == "dark" {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

// Segoe Fluent Icons / MDL2 Assets
pub const ICON_SETTINGS: &str = "\u{e713}";
pub const ICON_SUN: &str = "\u{e706}";
pub const ICON_MOON: &str = "\u{e708}";
pub const ICON_CLOSE: &str = "\u{e711}";
pub const ICON_POP: &str = "\u{e8a7}";
pub const ICON_DOCK: &str = "\u{e73f}";
pub const ICON_BACK: &str = "\u{e72b}";
pub const ICON_CHEVRON: &str = "\u{e76c}";
pub const ICON_OK: &str = "\u{e73e}";
pub const ICON_BAD: &str = "\u{e711}";

/// "light" or "dark" from Windows' app mode setting.
pub fn system_mode() -> &'static str {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
    let name: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();
    let mut value: u32 = 1;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(),
                     &mut value as *mut u32 as *mut _, &mut size)
    };
    if r != 0 || value != 0 {
        "light"
    } else {
        "dark"
    }
}

pub fn resolve(mode: &str) -> &'static str {
    match mode {
        "system" => system_mode(),
        "dark" => "dark",
        _ => "light",
    }
}

/// Ask Windows for a dark (or light) title bar on a window.
pub fn set_title_bar(hwnd: isize, dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    if hwnd == 0 {
        return;
    }
    let value: i32 = dark as i32;
    for attr in [20u32, 19] {
        // DWMWA_USE_IMMERSIVE_DARK_MODE (19 before Win10 20H1)
        let r = unsafe { DwmSetWindowAttribute(hwnd as _, attr, &value as *const i32 as *const _, 4) };
        if r == 0 {
            break;
        }
    }
}

/// The virtual desktop: (left, top, width, height).
pub fn virtual_desktop() -> (i32, i32, i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
    unsafe { (GetSystemMetrics(76), GetSystemMetrics(77), GetSystemMetrics(78), GetSystemMetrics(79)) }
}

/// The primary screen's size in pixels.
pub fn screen_size() -> (i32, i32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
    unsafe { (GetSystemMetrics(0), GetSystemMetrics(1)) }
}

/// Our window with this exact title (this process only), as an HWND number.
pub fn find_own_window(title: &str) -> isize {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};
    struct Find {
        title: String,
        pid: u32,
        found: isize,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> i32 { unsafe {
        let f = &mut *(lp as *mut Find);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == f.pid && internal_title(hwnd) == f.title {
            f.found = hwnd as isize;
            return 0;
        }
        1
    }}
    let mut f = Find { title: title.to_string(), pid: unsafe { GetCurrentProcessId() }, found: 0 };
    unsafe { EnumWindows(Some(cb), &mut f as *mut Find as LPARAM) };
    f.found
}

/// A window's title without sending it a message (safe for our own windows while the
/// event loop is busy).
fn internal_title(hwnd: windows_sys::Win32::Foundation::HWND) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::InternalGetWindowText;
    let mut buf = [0u16; 256];
    let n = unsafe { InternalGetWindowText(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn bell() {
    use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
    unsafe { MessageBeep(0) };
}

// --- fonts -------------------------------------------------------------------

/// Font file bytes for a face as GDI resolves it (the way Tk picks fonts), and the
/// face GDI actually gave us.
fn gdi_font(face: &str, weight: i32) -> Option<(Vec<u8>, String)> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, GetFontData, GetTextFaceW, SelectObject,
    };
    let wface: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let font = CreateFontW(-13, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 0, 0, wface.as_ptr());
        let old = SelectObject(dc, font as _);
        let mut name = [0u16; 64];
        let n = GetTextFaceW(dc, 64, name.as_mut_ptr());
        let got = String::from_utf16_lossy(&name[..n.max(1) as usize - 1]);
        let mut data = None;
        for table in [0x6663_7474u32 /* 'ttcf' */, 0] {
            let size = GetFontData(dc, table, 0, std::ptr::null_mut(), 0);
            if size != u32::MAX && size > 0 {
                let mut buf = vec![0u8; size as usize];
                if GetFontData(dc, table, 0, buf.as_mut_ptr() as *mut _, size) == size {
                    data = Some(buf);
                    break;
                }
            }
        }
        SelectObject(dc, old);
        DeleteObject(font as _);
        DeleteDC(dc);
        data.map(|d| (d, got))
    }
}

/// Coordinates of a variable font's named instance, e.g. "Text Bold".
fn instance_coords(bytes: &[u8], wanted: &str) -> Option<Vec<(String, f32)>> {
    let font = skrifa::FontRef::from_index(bytes, 0).ok()?;
    let axes: Vec<String> = font.axes().iter().map(|a| a.tag().to_string()).collect();
    for inst in font.named_instances().iter() {
        let name: String = font.localized_strings(inst.subfamily_name_id()).english_or_first()?.chars().collect();
        if name.eq_ignore_ascii_case(wanted) {
            return Some(axes.iter().cloned().zip(inst.user_coords()).collect());
        }
    }
    None
}

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub ascent: f32,
    pub descent: f32,
    pub linespace: f32,
}

/// A font as the app uses it: an egui font plus Tk-style (GDI) metrics.
#[derive(Clone, Debug)]
pub struct Font {
    pub id: FontId,
    pub m: Metrics,
}

impl Font {
    pub fn size(&self) -> f32 {
        self.id.size
    }
}

/// Named fonts: Segoe UI / Cascadia Mono / Segoe Fluent Icons, with the
/// usual fallbacks.
#[derive(Clone, Debug)]
pub struct Fonts {
    pub ui: Font,
    pub ui_bold: Font,
    pub small: Font,
    pub small_bold: Font,
    pub heading: Font,
    pub title: Font,
    pub toggle: Font,
    pub mono: Font,
    pub icon: Font,
    pub icon_small: Font,
    /// icon glyphs at the small UI size (Tk drew them in the small font's fallback)
    pub icon_12: Font,
    /// a table heading: one point smaller than the UI font, bold
    pub table_heading: Font,
    /// hover hints: Segoe UI 9 pt, like Windows' own tooltips
    pub tip: Font,
    pub has_icons: bool,
    /// the faces' metrics, kept to rebuild the fonts at another size
    metrics: Arc<HashMap<String, (u16, i16, i16)>>,
}

/// Tk's pixel size for a point size at 96 dpi (tk scaling 1.333).
pub fn px(pt: f32) -> f32 {
    (pt * 96.0 / 72.0).round()
}

struct Loaded {
    defs: FontDefinitions,
    metrics: HashMap<String, (u16, i16, i16)>,
}

fn load_face(l: &mut Loaded, family: &str, faces: &[&str], weight: i32, instance: Option<&str>) -> bool {
    for face in faces {
        let Some((bytes, got)) = gdi_font(face, weight) else { continue };
        if !got.eq_ignore_ascii_case(face) && !face.starts_with(got.as_str()) {
            continue; // GDI substituted another font
        }
        let mut tweak = FontTweak::default();
        let variable = skrifa::FontRef::from_index(&bytes, 0).map(|f| !f.axes().is_empty()).unwrap_or(false);
        if variable {
            let wanted = match instance {
                Some(i) => i.to_string(),
                None => if weight >= 700 { "Bold".into() } else { "Regular".into() },
            };
            let mut coords = VariationCoords::default();
            match instance_coords(&bytes, &wanted) {
                Some(c) => {
                    for (tag, v) in c {
                        coords.push(tag.as_str(), v);
                    }
                }
                None => coords.push(b"wght", weight as f32),
            }
            tweak.coords = coords;
        }
        let (upem, asc, desc) = skrifa::FontRef::from_index(&bytes, 0)
            .ok()
            .and_then(|f| {
                let upem = f.head().ok()?.units_per_em();
                let os2 = f.os2().ok()?;
                Some((upem, os2.us_win_ascent() as i16, os2.us_win_descent() as i16))
            })
            .unwrap_or((2048, 2210, 514));
        l.metrics.insert(family.to_string(), (upem, asc, desc));
        l.defs.font_data.insert(family.to_string(), Arc::new(FontData::from_owned(bytes).tweak(tweak)));
        let fam = FontFamily::Name(family.into());
        l.defs.families.insert(fam, vec![family.to_string(), "lucida-fallback".into(), "sym-fallback".into(), "emoji-fallback".into()]);
        return true;
    }
    false
}

impl Fonts {
    /// Load the fonts (for `egui::Context::set_fonts`) and their metrics.
    pub fn load(base_pt: f32) -> (Fonts, FontDefinitions) {
        let mut l = Loaded { defs: FontDefinitions::default(), metrics: HashMap::new() };
        // Glyphs the main fonts lack (e.g. ▾ ✓ ✗) come from the fonts Tk falls back to.
        for (name, face) in [("lucida-fallback", "Lucida Sans Unicode"), ("sym-fallback", "Segoe UI Symbol"), ("emoji-fallback", "Segoe UI Emoji")] {
            if let Some((bytes, _)) = gdi_font(face, 400) {
                l.defs.font_data.insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            }
        }
        let ui_faces = ["Segoe UI"];
        load_face(&mut l, "ui", &ui_faces, 400, Some("Regular"));
        load_face(&mut l, "ui-bold", &ui_faces, 700, Some("Bold Text"));
        load_face(&mut l, "display-bold", &ui_faces, 700, Some("Bold Text"));
        let mono_faces = ["Cascadia Mono", "Consolas"];
        load_face(&mut l, "mono", &mono_faces, 400, None);
        load_face(&mut l, "mono-bold", &mono_faces, 700, None);
        let has_icons = load_face(&mut l, "icons", &["Segoe Fluent Icons", "Segoe MDL2 Assets"], 400, None);
        // Hover hints: classic Segoe UI, as Windows' own tooltips (user request)
        load_face(&mut l, "tip", &["Segoe UI", "Segoe UI Variable Text"], 400, Some("Regular"));
        // Anything that failed to load falls back to egui's own fonts.
        for fam in ["ui", "ui-bold", "display-bold", "mono", "mono-bold", "icons", "tip"] {
            let key = FontFamily::Name(fam.into());
            if !l.defs.families.contains_key(&key) {
                let base = if fam.starts_with("mono") { FontFamily::Monospace } else { FontFamily::Proportional };
                let list = l.defs.families.get(&base).cloned().unwrap_or_default();
                l.defs.families.insert(key, list);
            }
        }
        // egui's own widgets (text boxes) use the UI font too.
        let ui_first = ["ui".to_string()];
        for base in [FontFamily::Proportional] {
            let mut list: Vec<String> = ui_first.iter().filter(|f| l.defs.font_data.contains_key(*f)).cloned().collect();
            list.extend(l.defs.families.get(&base).cloned().unwrap_or_default());
            l.defs.families.insert(base, list);
        }
        let fonts = Fonts::build(Arc::new(l.metrics.clone()), has_icons, base_pt);
        (fonts, l.defs)
    }

    /// The same fonts at another base size (the UI font's point size; the others follow).
    pub fn sized(&self, base_pt: f32) -> Fonts {
        Fonts::build(self.metrics.clone(), self.has_icons, base_pt)
    }

    fn build(metrics: Arc<HashMap<String, (u16, i16, i16)>>, has_icons: bool, base_pt: f32) -> Fonts {
        let d = base_pt - 10.0;
        let m2 = metrics.clone();
        let make = move |fam: &str, pt: f32| {
            let size = px(pt + d);
            let (upem, asc, desc) = metrics.get(fam).copied().unwrap_or((2048, 2210, 514));
            let ascent = (asc as f32 * size / upem as f32).round();
            let descent = (desc as f32 * size / upem as f32).round();
            let mut font = Font { id: FontId::new(size, FontFamily::Name(fam.into())), m: Metrics { ascent, descent, linespace: ascent + descent } };
            if let Some((a, d)) = crate::ui::gdi_ascent_descent(&font) {
                font.m = Metrics { ascent: a, descent: d, linespace: a + d };
            }
            font
        };
        let fonts = Fonts {
            ui: make("ui", 10.0),
            ui_bold: make("ui-bold", 10.0),
            small: make("ui", 9.0),
            small_bold: make("ui-bold", 9.0),
            heading: make("ui-bold", 11.0),
            title: make("display-bold", 15.0),
            toggle: make("ui-bold", 11.0),
            mono: make("mono", 10.0),
            icon: make("icons", 11.0),
            icon_small: make("icons", 13.0),
            // Tk drew these glyphs with the small UI font's metrics (its fallback font)
            icon_12: Font { m: make("ui", 9.0).m, ..make("icons", 9.0) },
            table_heading: make("ui-bold", 9.0),
            tip: make("tip", 9.0),
            has_icons,
            metrics: m2,
        };
        fonts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn show_font_instances() {
        for face in ["Segoe UI Variable Text", "Cascadia Mono", "Segoe Fluent Icons"] {
            let (bytes, got) = gdi_font(face, 400).unwrap();
            let font = skrifa::FontRef::from_index(&bytes, 0).unwrap();
            println!("{face} -> {got}, {} bytes, axes {:?}", bytes.len(),
                     font.axes().iter().map(|a| (a.tag().to_string(), a.min_value(), a.default_value(), a.max_value())).collect::<Vec<_>>());
            for inst in font.named_instances().iter() {
                let name: String = font.localized_strings(inst.subfamily_name_id()).english_or_first().unwrap().chars().collect();
                println!("  {name}: {:?}", inst.user_coords().collect::<Vec<_>>());
            }
        }
    }
}
