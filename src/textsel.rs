//! App-wide text selection: drag across any text in the main window to highlight it,
//! Ctrl+C copies it (handy for debugging). Every string `ui::text` / `ui::text_on`
//! draws is recorded as a run (its position, characters and clip) while the mouse button
//! is down or a selection is showing; at the end of the frame a drag that no widget
//! claimed (the log, Current Info and the About tab have their own selection, scroll bars
//! and dividers are dragged by their widgets) selects every character between the
//! press and the pointer in reading order, paints a translucent highlight over it and,
//! on Ctrl+C, copies it.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use eframe::egui::{self, Color32, Context, CursorIcon, Key, LayerId, Pos2, Rect, Ui, Vec2, ViewportId};

use crate::theme::Font;
use crate::ui::positions;

/// Vertical slack when deciding whether two points are on the same line of text.
const LINE_TOL: f32 = 9.0;

struct Run {
    x: f32,
    y: f32,
    h: f32,
    text: String,
    xs: Rc<Vec<f32>>,
    clip: Rect,
    layer: LayerId,
}

thread_local! {
    static RUNS: RefCell<Vec<Run>> = const { RefCell::new(Vec::new()) };
    /// recording costs a little per string, so only while a selection could be happening
    static ON: Cell<bool> = const { Cell::new(false) };
}

/// Note that `s` was drawn at `top_left` (called by the text primitives).
pub fn record(ui: &Ui, painter: &egui::Painter, top_left: Pos2, s: &str, f: &Font) {
    if s.is_empty() || !ON.with(|c| c.get()) || ui.ctx().viewport_id() != ViewportId::ROOT {
        return;
    }
    let xs = positions(f, s);
    let r = Rect::from_min_size(top_left, Vec2::new(xs.last().copied().unwrap_or(0.0), f.m.linespace));
    let clip = painter.clip_rect();
    if !clip.intersects(r) {
        return;
    }
    RUNS.with(|runs| runs.borrow_mut().push(Run { x: top_left.x, y: top_left.y, h: f.m.linespace, text: s.to_string(), xs, clip, layer: painter.layer_id() }));
}

/// `p` comes before `q` in reading order (rows first, within a row left to right).
fn before(p: Pos2, q: Pos2) -> bool {
    p.y < q.y - LINE_TOL || ((p.y - q.y).abs() <= LINE_TOL && p.x < q.x)
}

#[derive(Default)]
pub struct TextSelect {
    /// where the mouse button went down, and the layer under it
    press: Option<(Pos2, LayerId)>,
    /// anchor, focus and layer of the selection
    sel: Option<(Pos2, Pos2, LayerId)>,
    /// the card the drag started in: other cards' text on the same rows isn't selected
    region: Option<Rect>,
}

/// The card (a clip narrower than the window) holding `anchor`: the widest such clip, so
/// a table's body clip inside the card doesn't shut out the card's heading.
fn card_at(runs: &[Run], anchor: Pos2, layer: LayerId, window: Rect) -> Option<Rect> {
    runs.iter()
        .filter(|r| r.layer == layer && r.clip.contains(anchor) && r.clip.width() < window.width() * 0.9)
        .map(|r| r.clip)
        .max_by(|a, b| a.area().total_cmp(&b.area()))
}

impl TextSelect {
    pub fn clear(&mut self) {
        self.sel = None;
    }

    /// Run once at the end of every frame. Returns true when a selection has just
    /// started (the caller drops the other selections so Ctrl+C copies this one).
    pub fn end_frame(&mut self, ctx: &Context, color: Color32) -> bool {
        let runs = RUNS.with(|r| std::mem::take(&mut *r.borrow_mut()));
        let (pressed, down, pos, dragging, copy) = ctx.input(|i| {
            (i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.interact_pos(), i.pointer.is_decidedly_dragging(), i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
        });
        if pressed {
            self.sel = None;
            self.press = pos.map(|p| (p, ctx.layer_id_at(p).unwrap_or_else(LayerId::background)));
        }
        if !down {
            self.press = None;
        }
        let mut started = false;
        if down && dragging {
            if let (Some((a, layer)), Some(p)) = (self.press, pos) {
                if self.sel.is_some() || !ctx.egui_is_using_pointer() {
                    if self.sel.is_none() {
                        started = true;
                        self.region = card_at(&runs, a, layer, ctx.content_rect());
                    }
                    self.sel = Some((a, p, layer));
                }
            }
        }
        if self.sel.is_some() && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.sel = None;
        }
        if let Some((a, b, layer)) = self.sel {
            if down {
                ctx.set_cursor_icon(CursorIcon::Text);
            }
            let (a, b) = if before(b, a) { (b, a) } else { (a, b) };
            let mut seen = HashSet::new();
            let mut parts: Vec<(f32, f32, f32, String)> = Vec::new(); // centre y, x, width, text
            let region = self.region.map(|r| r.expand(1.0));
            for run in runs.iter().filter(|r| r.layer == layer && region.is_none_or(|g| g.contains_rect(r.clip))) {
                if !seen.insert((run.x.round() as i32, run.y.round() as i32, run.text.as_str())) {
                    continue;
                }
                let chars: Vec<char> = run.text.chars().collect();
                let cy = run.y + run.h / 2.0;
                let picked = |i: usize| {
                    let c = Pos2::new(run.x + (run.xs[i] + run.xs[i + 1]) / 2.0, cy);
                    !before(c, a) && !before(b, c)
                };
                let mut i = 0;
                while i < chars.len() {
                    if !picked(i) {
                        i += 1;
                        continue;
                    }
                    let s = i;
                    while i < chars.len() && picked(i) {
                        i += 1;
                    }
                    let (x0, x1) = (run.x + run.xs[s], run.x + run.xs[i]);
                    ctx.layer_painter(layer).with_clip_rect(run.clip).rect_filled(Rect::from_min_max(Pos2::new(x0, run.y), Pos2::new(x1, run.y + run.h)), 0.0, color);
                    parts.push((cy, x0, x1 - x0, chars[s..i].iter().collect()));
                }
            }
            if copy && !parts.is_empty() {
                ctx.copy_text(join_parts(parts));
            }
        }
        // record during the next frame only while it can matter
        let on = down || self.sel.is_some();
        ON.with(|c| c.set(on));
        started
    }
}

/// The selected pieces as text: lines top to bottom, pieces of one line left to right.
fn join_parts(mut parts: Vec<(f32, f32, f32, String)>) -> String {
    parts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut lines: Vec<Vec<(f32, f32, String)>> = Vec::new();
    let mut line_y = f32::MIN;
    for (cy, x, w, t) in parts {
        if lines.is_empty() || (cy - line_y).abs() > LINE_TOL * 0.7 {
            lines.push(Vec::new());
            line_y = cy;
        }
        lines.last_mut().unwrap().push((x, w, t));
    }
    let mut out = Vec::new();
    for mut line in lines {
        line.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut s = String::new();
        let mut end = f32::MIN;
        for (x, w, t) in line {
            if !s.is_empty() && x - end > 3.0 {
                s.push(' ');
            }
            s.push_str(&t);
            end = x + w;
        }
        out.push(s);
    }
    out.join("\n")
}
