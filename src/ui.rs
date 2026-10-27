// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Drawing primitives that reproduce classic Win32 controls: text placed by
//! GDI metrics, rounded boxes and pills (`Rnd`), tooltips, and slim scrollbars.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use eframe::egui::{self, Align2, Color32, CornerRadius, CursorIcon, Id, Pos2, Rect, Response, Sense, Ui, Vec2};
use eframe::epaint::Galley;

use crate::theme::{Font, Palette};

pub const CAP_HEIGHT: f32 = 0.70; // cap height / em for Segoe UI and most UI fonts

thread_local! {
    static BASELINES: RefCell<HashMap<(u32, String), f32>> = RefCell::new(HashMap::new());
    /// (face, px, weight) -> memory DC with that font selected (None: GDI substituted it)
    static DCS: RefCell<HashMap<(&'static str, i32, i32), Option<isize>>> = RefCell::new(HashMap::new());
    /// (face, px, weight, text) -> x of each char, then the total width
    static POSITIONS: RefCell<HashMap<(&'static str, i32, i32, String), Rc<Vec<f32>>>> = RefCell::new(HashMap::new());
}

fn family_face(f: &Font) -> (&'static str, i32) {
    match &f.id.family {
        egui::FontFamily::Name(n) => match n.as_ref() {
            "ui" => ("Segoe UI Variable Text", 400),
            "ui-bold" => ("Segoe UI Variable Text", 700),
            "display-bold" => ("Segoe UI Variable Display", 700),
            "mono" => ("Cascadia Mono", 400),
            "mono-bold" => ("Cascadia Mono", 700),
            "icons" => ("Segoe Fluent Icons", 400),
            "tip" => ("Segoe UI", 400),
            _ => ("Segoe UI", 400),
        },
        _ => ("Segoe UI", 400),
    }
}

/// A memory DC with the GDI font Tk would use (cached for the app's lifetime).
fn gdi_dc(face: &'static str, px: i32, weight: i32) -> Option<isize> {
    use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, CreateFontW, GetTextFaceW, SelectObject};
    if let Some(dc) = DCS.with(|m| m.borrow().get(&(face, px, weight)).copied()) {
        return dc;
    }
    let wface: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
    let dc = unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let font = CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, wface.as_ptr());
        SelectObject(dc, font as _);
        let mut name = [0u16; 64];
        let n = GetTextFaceW(dc, 64, name.as_mut_ptr());
        let got = String::from_utf16_lossy(&name[..(n.max(1) - 1) as usize]);
        got.eq_ignore_ascii_case(face).then_some(dc as isize)
    };
    DCS.with(|m| m.borrow_mut().insert((face, px, weight), dc));
    dc
}

/// GDI advance of each UTF-16 unit of `w` (0 where the font has no glyph).
fn gdi_advances(dc: isize, w: &[u16]) -> Option<Vec<f32>> {
    use windows_sys::Win32::Foundation::SIZE;
    use windows_sys::Win32::Graphics::Gdi::{GetGlyphIndicesW, GetTextExtentExPointW};
    let mut dx = vec![0i32; w.len()];
    let mut size = SIZE { cx: 0, cy: 0 };
    let mut glyphs = vec![0u16; w.len()];
    unsafe {
        if GetTextExtentExPointW(dc as _, w.as_ptr(), w.len() as i32, 0, std::ptr::null_mut(), dx.as_mut_ptr(), &mut size) == 0 {
            return None;
        }
        GetGlyphIndicesW(dc as _, w.as_ptr(), w.len() as i32, glyphs.as_mut_ptr(), 1); // GGI_MARK_NONEXISTING_GLYPHS
    }
    let mut prev = 0;
    Some(dx.iter().zip(&glyphs).map(|(&d, &g)| {
        let a = d - prev;
        prev = d;
        if g == 0xFFFF { -1.0 } else { a as f32 }
    }).collect())
}

/// Where each character of `text` starts, as Tk lays it out (GDI advance widths; a
/// character the font lacks is measured in Segoe UI Symbol, like Tk's fallback), and
/// the total width last.
pub fn positions(f: &Font, text: &str) -> Rc<Vec<f32>> {
    let (face, weight) = family_face(f);
    let px = f.size() as i32;
    let key = (face, px, weight, text.to_string());
    if let Some(v) = POSITIONS.with(|m| m.borrow().get(&key).cloned()) {
        return v;
    }
    let mut out = Vec::with_capacity(text.len() + 1);
    let w: Vec<u16> = text.encode_utf16().collect();
    let adv = gdi_dc(face, px, weight).and_then(|dc| gdi_advances(dc, &w));
    let mut x = 0.0f32;
    match adv {
        Some(adv) => {
            let mut i = 0;
            for c in text.chars() {
                out.push(x);
                let n = c.len_utf16();
                let mut a: f32 = adv[i..i + n].iter().sum();
                if adv[i..i + n].iter().any(|v| *v < 0.0) {
                    // the font has no such glyph: Tk draws it from a fallback font
                    a = f.size() * 0.6;
                    for fb in ["Lucida Sans Unicode", "Segoe UI Symbol"] {
                        let got = gdi_dc(fb, px, weight).and_then(|dc| gdi_advances(dc, &w[i..i + n]));
                        if let Some(v) = got.filter(|v| v.iter().all(|x| *x >= 0.0)) {
                            a = v.iter().sum();
                            break;
                        }
                    }
                }
                x += a;
                i += n;
            }
        }
        None => {
            for _ in text.chars() {
                out.push(x);
                x += (f.size() * 0.55).round();
            }
        }
    }
    out.push(x);
    let v = Rc::new(out);
    POSITIONS.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 50_000 {
            m.clear();
        }
        m.insert(key, v.clone());
    });
    v
}

/// Draw `lines` with GDI (hinted, ClearType) onto an opaque `bg` box of `size` physical
/// pixels, each line at `origin` + i × `line_h`. egui's unhinted grayscale glyphs look
/// smeared at tooltip sizes (user noticed); this is how Windows and Tk draw text. Only
/// for opaque backgrounds: ClearType needs to know the colour behind it.
pub fn gdi_text_image(f: &Font, scale: f32, lines: &[&str], fg: Color32, bg: Color32, size: [usize; 2], origin: Vec2, line_h: f32) -> Option<egui::ColorImage> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, GdiFlush, SelectObject, SetBkMode, SetTextColor,
        TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, TRANSPARENT,
    };
    let (face, weight) = family_face(f);
    let [w, h] = size;
    if w == 0 || h == 0 {
        return None;
    }
    let wface: Vec<u16> = face.encode_utf16().chain(Some(0)).collect();
    let rgb = |c: Color32| c.r() as u32 | (c.g() as u32) << 8 | (c.b() as u32) << 16;
    unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bmp.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, w * h);
        let bgr = (bg.b() as u32) | (bg.g() as u32) << 8 | (bg.r() as u32) << 16;
        px.fill(bgr);
        let old_bmp = SelectObject(dc, bmp as _);
        // quality 5 = CLEARTYPE_QUALITY, charset 1 = DEFAULT_CHARSET
        let font = CreateFontW(-(f.size() * scale).round() as i32, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, wface.as_ptr());
        let old_font = SelectObject(dc, font as _);
        SetBkMode(dc, TRANSPARENT as _);
        SetTextColor(dc, rgb(fg));
        for (i, l) in lines.iter().enumerate() {
            let wl: Vec<u16> = l.encode_utf16().collect();
            let x = (origin.x * scale).round() as i32;
            let y = ((origin.y + i as f32 * line_h) * scale).round() as i32;
            TextOutW(dc, x, y, wl.as_ptr(), wl.len() as i32);
        }
        GdiFlush();
        let rgba: Vec<Color32> = px.iter().map(|&p| Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8)).collect();
        SelectObject(dc, old_font);
        SelectObject(dc, old_bmp);
        DeleteObject(font as _);
        DeleteObject(bmp as _);
        DeleteDC(dc);
        Some(egui::ColorImage { size, pixels: rgba, source_size: Vec2::new(w as f32, h as f32) })
    }
}

/// A run of text drawn by GDI: its texture, the offset of the texture's corner from the
/// run's top-left (the margin), its size in points, and the pass it was last drawn in.
struct RunTex {
    tex: egui::TextureHandle,
    offset: Vec2,
    size: Vec2,
    last: u64,
}

/// (text, face, physical px, weight, fg, bg)
type RunKey = (String, &'static str, i32, i32, Color32, Color32);

thread_local! {
    /// GDI-drawn runs; `None` marks a run the font can't draw (missing glyphs), which
    /// falls back to egui's glyphs
    static RUNS: RefCell<HashMap<RunKey, Option<RunTex>>> = RefCell::new(HashMap::new());
}

/// Room around a run for glyphs that overhang their advance and ClearType fringes;
/// these pixels are transparent unless ink lands there.
const RUN_MARGIN: i32 = 3;

/// Draw `s` with GDI (hinted, ClearType) onto `bg`, as Windows and Tk draw text. Pixels
/// left at exactly `bg` come out transparent, so a run never paints a box over its
/// neighbours. `None` when the font lacks a glyph of `s`.
fn render_run(f: &Font, s: &str, scale: f32, fg: Color32, bg: Color32) -> Option<egui::ColorImage> {
    use windows_sys::Win32::Graphics::Gdi::{
        CreateDIBSection, DeleteObject, GdiFlush, SelectObject, SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, TRANSPARENT,
    };
    let (face, weight) = family_face(f);
    let px = (f.size() * scale).round() as i32;
    let dc = gdi_dc(face, px, weight)?; // its font is CLEARTYPE_QUALITY
    let w: Vec<u16> = s.encode_utf16().collect();
    let adv = gdi_advances(dc, &w)?;
    if adv.iter().any(|a| *a < 0.0) {
        return None;
    }
    let width = adv.iter().sum::<f32>() as i32;
    let bw = (width + 2 * RUN_MARGIN) as usize;
    let bh = ((f.m.linespace * scale).ceil() as i32 + 2 * RUN_MARGIN) as usize;
    let bgr = |c: Color32| (c.b() as u32) | (c.g() as u32) << 8 | (c.r() as u32) << 16;
    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw as i32,
            biHeight: -(bh as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bmp = CreateDIBSection(dc as _, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if bmp.is_null() || bits.is_null() {
            return None;
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, bw * bh);
        let back = bgr(bg);
        px.fill(back);
        let old = SelectObject(dc as _, bmp as _);
        SetBkMode(dc as _, TRANSPARENT as _);
        // COLORREF is 0x00BBGGRR
        SetTextColor(dc as _, fg.r() as u32 | (fg.g() as u32) << 8 | (fg.b() as u32) << 16);
        TextOutW(dc as _, RUN_MARGIN, RUN_MARGIN, w.as_ptr(), w.len() as i32);
        GdiFlush();
        let pixels: Vec<Color32> = px
            .iter()
            .map(|&p| if p & 0xFF_FFFF == back { Color32::TRANSPARENT } else { Color32::from_rgb((p >> 16) as u8, (p >> 8) as u8, p as u8) })
            .collect();
        SelectObject(dc as _, old);
        DeleteObject(bmp as _);
        Some(egui::ColorImage { size: [bw, bh], pixels, source_size: Vec2::new(bw as f32, bh as f32) })
    }
}

/// Like `text`, but drawn by GDI with ClearType onto `bg`, the solid colour behind the
/// text (it must be what is really there: ClearType blends into it). Widths and
/// positions are the same as `text`'s. Each (text, font, colours) run is drawn once and
/// kept as a texture; runs not drawn for a while are dropped.
pub fn text_on(ui: &Ui, painter: &egui::Painter, top_left: Pos2, s: &str, f: &Font, color: Color32, bg: Color32) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    let width = measure(f, s);
    let clip = painter.clip_rect();
    if top_left.x > clip.right() || top_left.x + width < clip.left() || top_left.y > clip.bottom() || top_left.y + f.m.linespace < clip.top() {
        return width;
    }
    let ctx = ui.ctx();
    let scale = ctx.pixels_per_point();
    let pass = ctx.cumulative_pass_nr();
    let (face, weight) = family_face(f);
    let key: RunKey = (s.to_string(), face, (f.size() * scale).round() as i32, weight, color, bg);
    let found = RUNS.with(|m| {
        let mut m = m.borrow_mut();
        if !m.contains_key(&key) {
            if m.len() > 3000 {
                // drop runs not drawn in the last ~600 passes (scrolled away, old glow colours)
                m.retain(|_, r| r.as_ref().is_some_and(|r| r.last + 600 > pass));
                if m.len() > 3000 {
                    m.clear();
                }
            }
            let run = render_run(f, s, scale, color, bg).map(|img| {
                let size = Vec2::new(img.size[0] as f32, img.size[1] as f32) / scale;
                RunTex { tex: ctx.load_texture("gdi-run", img, egui::TextureOptions::NEAREST), offset: Vec2::splat(-RUN_MARGIN as f32 / scale), size, last: pass }
            });
            m.insert(key.clone(), run);
        }
        m.get_mut(&key).unwrap().as_mut().map(|r| {
            r.last = pass;
            (r.tex.id(), r.offset, r.size)
        })
    });
    match found {
        Some((id, offset, size)) => {
            // whole physical pixels, so GDI's pixels land 1:1 on the screen
            let at = ((top_left.to_vec2() * scale).round() / scale).to_pos2() + offset;
            painter.image(id, Rect::from_min_size(at, size), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            crate::textsel::record(ui, painter, top_left, s, f);
            width
        }
        None => text(ui, painter, top_left, s, f, color),
    }
}

/// Text width in pixels as Tk would measure it.
pub fn measure(f: &Font, text: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    *positions(f, text).last().unwrap()
}

pub fn galley(ui: &Ui, f: &Font, text: &str, color: Color32) -> Arc<Galley> {
    ui.painter().layout_no_wrap(text.to_string(), f.id.clone(), color)
}

/// Distance from the top of an egui galley to its first baseline.
pub fn egui_baseline(ui: &Ui, f: &Font) -> f32 {
    let key = (f.size().to_bits(), format!("{:?}", f.id.family));
    if let Some(b) = BASELINES.with(|m| m.borrow().get(&key).copied()) {
        return b;
    }
    let g = galley(ui, f, "Hg", Color32::WHITE);
    let b = g
        .rows
        .first()
        .and_then(|r| r.row.glyphs.first().map(|gl| r.pos.y + gl.pos.y))
        .unwrap_or(f.m.ascent);
    BASELINES.with(|m| m.borrow_mut().insert(key, b));
    b
}

/// Draw `text` whose Tk line box (ascent + descent) starts at `top_left`, each character
/// where Tk would put it. Returns its width.
pub fn text(ui: &Ui, painter: &egui::Painter, top_left: Pos2, s: &str, f: &Font, color: Color32) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    crate::textsel::record(ui, painter, top_left, s, f);
    let pos = positions(f, s);
    let baseline = top_left.y + f.m.ascent;
    let x0 = top_left.x.round();
    let clip = painter.clip_rect();
    let mut buf = [0u8; 4];
    for (i, c) in s.chars().enumerate() {
        if c == ' ' {
            continue;
        }
        let x = x0 + pos[i];
        if x > clip.right() || x + f.size() * 2.0 < clip.left() {
            continue;
        }
        let g = ui.painter().layout_no_wrap(c.encode_utf8(&mut buf).to_string(), f.id.clone(), color);
        // each glyph's own baseline (a fallback font's differs from the main font's)
        let b = g.rows.first().and_then(|r| r.row.glyphs.first().map(|gl| r.pos.y + gl.pos.y)).unwrap_or(f.m.ascent);
        painter.galley(Pos2::new(x, (baseline - b).round()), g, color);
    }
    *pos.last().unwrap()
}

/// A wrapped paragraph like a Tk Label with wraplength; returns its height.
pub fn paragraph(ui: &Ui, painter: Option<&egui::Painter>, top_left: Pos2, s: &str, f: &Font, color: Color32, wrap: f32) -> f32 {
    let lines = wrap_lines(ui, f, s, wrap);
    if let Some(p) = painter {
        for (i, line) in lines.iter().enumerate() {
            text(ui, p, top_left + Vec2::new(0.0, i as f32 * f.m.linespace), line, f, color);
        }
    }
    lines.len().max(1) as f32 * f.m.linespace
}

/// Break `s` into lines no wider than `wrap` (on spaces, like Tk's wraplength).
pub fn wrap_lines(_ui: &Ui, f: &Font, s: &str, wrap: f32) -> Vec<String> {
    wrap_lines_measure(f, s, wrap)
}

pub fn wrap_lines_measure(f: &Font, s: &str, wrap: f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let trial = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && measure(f, &trial) > wrap {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = trial;
            }
        }
        out.push(line);
    }
    out
}

pub fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

/// A rounded box with optically centred text. With a click
/// handler it is a button.
#[derive(Clone)]
pub struct Rnd<'a> {
    pub text: &'a str,
    pub font: &'a Font,
    pub fill: Color32,
    pub fg: Color32,
    pub hover_fill: Option<Color32>,
    pub padx: f32,
    pub pady: f32,
    /// None makes a pill
    pub radius: Option<f32>,
    /// a status light before the text
    pub dot: Option<Color32>,
    /// false centres the line box instead of the capitals (icon glyphs)
    pub cap: bool,
    /// the narrowest the box gets
    pub size_text: &'a str,
}

impl<'a> Rnd<'a> {
    pub fn new(text: &'a str, font: &'a Font) -> Self {
        Rnd {
            text,
            font,
            fill: Color32::from_gray(0x44),
            fg: Color32::WHITE,
            hover_fill: None,
            padx: 10.0,
            pady: 4.0,
            radius: Some(8.0),
            dot: None,
            cap: true,
            size_text: "",
        }
    }

    pub fn pill(mut self) -> Self {
        self.radius = None;
        self
    }

    pub fn pad(mut self, x: f32, y: f32) -> Self {
        self.padx = x;
        self.pady = y;
        self
    }

    pub fn colors(mut self, fill: Color32, fg: Color32) -> Self {
        self.fill = fill;
        self.fg = fg;
        self
    }

    pub fn hover(mut self, h: Color32) -> Self {
        self.hover_fill = Some(h);
        self
    }

    pub fn dot(mut self, d: Option<Color32>) -> Self {
        self.dot = d;
        self
    }

    pub fn icon(mut self) -> Self {
        self.cap = false;
        self
    }

    pub fn min_text(mut self, s: &'a str) -> Self {
        self.size_text = s;
        self
    }

    fn dot_space(&self) -> (f32, f32) {
        if self.dot.is_none() {
            return (0.0, 0.0);
        }
        let d = (self.font.m.linespace * 0.42).round();
        (d, d + 6f32.max((d / 2.0).floor())) // diameter, space it takes before the text
    }

    pub fn size(&self, _ui: &Ui) -> Vec2 {
        self.size_static()
    }

    pub fn size_static(&self) -> Vec2 {
        let (_, dot_w) = self.dot_space();
        let widest = measure(self.font, self.text).max(measure(self.font, self.size_text));
        Vec2::new(widest + dot_w + 2.0 * self.padx, self.font.m.linespace + 2.0 * self.pady)
    }

    pub fn paint(&self, ui: &Ui, painter: &egui::Painter, rect: Rect, hovered: bool) {
        let (w, h) = (rect.width(), rect.height());
        let r = match self.radius {
            None => h / 2.0,
            Some(r) => r,
        }
        .min(h / 2.0)
        .min(w / 2.0);
        let fill = if hovered { self.hover_fill.unwrap_or(self.fill) } else { self.fill };
        painter.rect_filled(rect, radius(r), fill);
        let (d, dot_w) = self.dot_space();
        let top = if self.cap {
            let cap = self.font.size() * CAP_HEIGHT;
            (h / 2.0 - (self.font.m.ascent - cap / 2.0)).round()
        } else {
            ((h - self.font.m.linespace) / 2.0).round()
        };
        let text_w = measure(self.font, self.text);
        let left = (w - text_w - dot_w) / 2.0;
        let x = (left + dot_w + text_w / 2.0).round() - (text_w / 2.0).floor();
        let at = rect.min + Vec2::new(x, top);
        if fill.a() == 255 {
            text_on(ui, painter, at, self.text, self.font, self.fg, fill);
        } else {
            text(ui, painter, at, self.text, self.font, self.fg);
        }
        if let Some(c) = self.dot {
            let c0 = rect.min + Vec2::new(left.round() + d / 2.0, ((h - d) / 2.0).round() + d / 2.0);
            painter.circle_filled(c0, d / 2.0, c);
        }
    }

    /// Paint at `rect`; clickable when `click`. Returns the response.
    pub fn show(&self, ui: &mut Ui, id: Id, rect: Rect, click: bool) -> Response {
        let resp = ui.interact(rect, id, if click { Sense::click() } else { Sense::hover() });
        let hovered = click && resp.hovered();
        if hovered {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        self.paint(ui, &ui.painter().clone(), rect, hovered);
        resp
    }
}

// --- tooltips ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum TipAt {
    /// under the widget, right-aligned with it
    Below(Rect),
    /// at the pointer, offset (the Table hover hints)
    Pointer,
}

struct TipReq {
    id: Id,
    text: String,
    delay: f64,
    at: TipAt,
}

/// One hover hint at a time, shown after a delay, hidden by a click.
#[derive(Default)]
pub struct Tips {
    hover: Option<(Id, f64, Pos2)>,
    suppressed: Option<Id>,
    req: Option<TipReq>,
    /// the GDI-drawn text of the tooltip showing: (text, fg, bg, scale) and its texture
    tex: Option<((String, Color32, Color32, u32), egui::TextureHandle)>,
}

impl Tips {
    /// Ask for `text` to show while `resp` is hovered.
    pub fn tip(&mut self, resp: &Response, text: &str, delay_ms: u32, at: TipAt) {
        if text.is_empty() {
            return;
        }
        if resp.clicked() || resp.secondary_clicked() {
            self.suppressed = Some(resp.id);
        }
        if resp.hovered() {
            self.req = Some(TipReq { id: resp.id, text: text.to_string(), delay: delay_ms as f64 / 1000.0, at });
        }
    }

    /// Same as `tip`, for an area that isn't a widget (hovered decided by the caller).
    pub fn tip_area(&mut self, id: Id, hovered: bool, text: &str, delay_ms: u32, at: TipAt) {
        if hovered && !text.is_empty() {
            self.req = Some(TipReq { id, text: text.to_string(), delay: delay_ms as f64 / 1000.0, at });
        }
    }

    /// Call once at the end of each frame.
    pub fn show(&mut self, ctx: &egui::Context, ui: &Ui, p: &Palette, font: &Font) {
        let now = ctx.input(|i| i.time);
        let pointer = ctx.input(|i| i.pointer.hover_pos()).unwrap_or_default();
        let Some(req) = self.req.take() else {
            self.hover = None;
            self.suppressed = None;
            return;
        };
        if self.suppressed.is_some_and(|s| s != req.id) {
            self.suppressed = None;
        }
        if self.suppressed == Some(req.id) {
            return;
        }
        let since = match self.hover {
            Some((id, t, _)) if id == req.id => t,
            _ => {
                self.hover = Some((req.id, now, pointer));
                now
            }
        };
        let left = req.delay - (now - since);
        if left > 0.0 {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(left + 0.01));
            return;
        }
        let lines: Vec<&str> = req.text.split('\n').collect();
        let w = lines.iter().map(|l| measure(font, l)).fold(0.0, f32::max) + 16.0;
        let h = lines.len() as f32 * font.m.linespace + 8.0;
        let screen = ctx.content_rect();
        let mut pos = match req.at {
            TipAt::Below(r) => Pos2::new((r.right() - w).max(r.left()), r.bottom() + 6.0),
            TipAt::Pointer => {
                let p0 = self.hover.map(|h| h.2).unwrap_or(pointer);
                Pos2::new(p0.x + 12.0, p0.y + 18.0)
            }
        };
        pos.x = pos.x.min(screen.right() - w).max(screen.left());
        pos.y = pos.y.min(screen.bottom() - h).max(screen.top());
        // whole physical pixels, so GDI's pixels land 1:1 on the screen
        let scale = ctx.pixels_per_point();
        pos = (pos.to_vec2() * scale).round().to_pos2() / scale;
        let rect = Rect::from_min_size(pos, Vec2::new(w, h));
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::unique("app-tooltip")));
        painter.rect_filled(rect, CornerRadius::ZERO, p.text);
        let key = (req.text.clone(), p.surface, p.text, scale.to_bits());
        if self.tex.as_ref().is_none_or(|(k, _)| *k != key) {
            let size = [(w * scale).round() as usize, (h * scale).round() as usize];
            self.tex = gdi_text_image(font, scale, &lines, p.surface, p.text, size, Vec2::new(8.0, 4.0), font.m.linespace)
                .map(|img| (key, ctx.load_texture("app-tooltip-text", img, egui::TextureOptions::NEAREST)));
        }
        match &self.tex {
            Some((_, tex)) => {
                painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                for (i, l) in lines.iter().enumerate() {
                    text(ui, &painter, rect.min + Vec2::new(8.0, 4.0 + i as f32 * font.m.linespace), l, font, p.surface);
                }
            }
        }
    }
}

// --- slim scrollbar ------------------------------------------------------------

/// A Text widget's scroll state and the slim ttk scrollbar beside it.
#[derive(Default, Clone)]
pub struct Scroll {
    pub offset: f32,
    drag: Option<(f32, f32)>,
    /// keep showing the end as content is added (Tk's `see end` when at the bottom)
    pub at_bottom: bool,
}

impl Scroll {
    pub fn max(content: f32, view: f32) -> f32 {
        (content - view).max(0.0)
    }

    pub fn clamp(&mut self, content: f32, view: f32) {
        self.offset = self.offset.clamp(0.0, Self::max(content, view));
    }

    pub fn is_at_bottom(&self, content: f32, view: f32) -> bool {
        self.offset >= Self::max(content, view) - 1.0
    }

    /// Mouse wheel over `area`: one notch scrolls `line_h * 4` like Tk's Text.
    pub fn wheel(&mut self, ui: &Ui, area: Rect, line_h: f32, content: f32, view: f32) {
        let over = ui.ctx().pointer_hover_pos().is_some_and(|p| area.contains(p))
            && ui.ctx().layer_id_at(ui.ctx().pointer_hover_pos().unwrap()).is_none_or(|l| l.order == egui::Order::Background);
        if !over {
            return;
        }
        let mut dy = 0.0;
        ui.ctx().input(|i| {
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    dy += match unit {
                        egui::MouseWheelUnit::Line => delta.y * line_h * 4.0,
                        egui::MouseWheelUnit::Page => delta.y * view,
                        egui::MouseWheelUnit::Point => delta.y,
                    };
                }
            }
        });
        if dy != 0.0 {
            self.offset -= dy;
            self.clamp(content, view);
        }
    }

    /// The scrollbar (10 px trough, 8 px thumb) in `bar`.
    pub fn bar(&mut self, ui: &mut Ui, id: Id, bar: Rect, content: f32, view: f32, p: &Palette) {
        let painter = ui.painter().clone();
        let track = bar.shrink(1.0);
        let (a, b) = if content <= view || content <= 0.0 {
            (0.0, 1.0)
        } else {
            (self.offset / content, ((self.offset + view) / content).min(1.0))
        };
        let thumb = Rect::from_min_max(
            Pos2::new(track.left(), track.top() + a * track.height()),
            Pos2::new(track.right(), (track.top() + b * track.height()).max(track.top() + a * track.height() + 8.0)),
        );
        let resp = ui.interact(bar, id, Sense::click_and_drag());
        let active = resp.hovered() || resp.dragged();
        painter.rect_filled(thumb, CornerRadius::ZERO, if active { p.faint } else { p.border });
        if resp.drag_started() {
            if let Some(pos) = resp.interact_pointer_pos() {
                if thumb.contains(pos) {
                    self.drag = Some((pos.y, self.offset));
                } else {
                    // a click on the trough pages towards it
                    let page = view.max(1.0);
                    self.offset += if pos.y < thumb.top() { -page } else { page };
                    self.clamp(content, view);
                }
            }
        }
        if resp.dragged() {
            if let (Some((y0, off0)), Some(pos)) = (self.drag, resp.interact_pointer_pos()) {
                self.offset = off0 + (pos.y - y0) / track.height().max(1.0) * content;
                self.clamp(content, view);
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
    }
}

/// Is the pointer over `rect` with nothing (popup, panel) above the main window?
pub fn hover_in(ui: &Ui, rect: Rect) -> bool {
    match ui.ctx().pointer_hover_pos() {
        Some(p) => rect.contains(p) && ui.ctx().layer_id_at(p).is_none_or(|l| l.order == egui::Order::Background),
        None => false,
    }
}

pub fn anchor_text(ui: &Ui, painter: &egui::Painter, pos: Pos2, align: Align2, s: &str, f: &Font, color: Color32) -> Rect {
    let w = measure(f, s);
    let rect = align.anchor_size(pos, Vec2::new(w, f.m.linespace));
    text(ui, painter, rect.min, s, f, color);
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cost of drawing runs with GDI, as a pulsing row does every frame (a new colour
    /// each time). `cargo test --release -- --ignored run_render_cost --nocapture`
    #[test]
    #[ignore]
    fn run_render_cost() {
        let (f, _) = crate::theme::Fonts::load();
        let cells = ["10:56", "0m", "N8UC", "7074.0", "US-1511 Kal-Haven Trail State Park"];
        let t = std::time::Instant::now();
        let n = 200;
        for i in 0..n {
            let bg = Color32::from_rgb(40, (i % 255) as u8, 60);
            for c in cells {
                assert!(render_run(&f.ui, c, 1.0, Color32::WHITE, bg).is_some());
            }
        }
        let per_row = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("one five-cell row: {per_row:.3} ms");
        let t = std::time::Instant::now();
        let line = "Loaded 2 QSO(s) from today (UTC) out of C:/Users/dan/AppData/Local/WS/wsjtx_log.adi";
        for _ in 0..n {
            render_run(&f.mono, line, 1.0, Color32::WHITE, Color32::BLACK);
        }
        println!("one long log line: {:.3} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }
}
