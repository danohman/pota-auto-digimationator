// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The main window: owns the hunter, the worker threads, the
//! settings and state files, and draws everything.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Timelike, Utc};
use eframe::egui::{self, Color32, CursorIcon, Id, Key, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::aclog::AclogClient;
use crate::cycles::{self, PeriodDetector};
use crate::dialogs::{self, Dialog, DialogKind};
use crate::events::{Event, Post};
use crate::hunter::{base_call, parse_message, read_adif_today, Hunter, HunterIo, Tag, WorkedEntry, BANDS};
use crate::logview::{LogClick, LogView};
use crate::panel::Panel;
use crate::popup::{Item, MenuCmd, PopupMenu};
use crate::pota_auth::{self, HuntCounts, StatsPoller};
use crate::pota_spots::{self, epoch_now, SpotPoller};
use crate::presence::{Phase, PresenceTimer};
use crate::settings::{self, Settings};
use crate::table::{Cell, Chip, Col, Icon, Lead, Row, RowColors, RowData, RowTags, Table, TableEvent};
use crate::theme::{self, Fonts, Palette};
use crate::ui::{self, measure, Rnd, TipAt, Tips};
use crate::wsjtx_gui;
use crate::wsjtx_udp::{self, Decode, ListenerShared, Message};
use crate::{logdialog, sound};

pub const TITLE: &str = "POTA Auto-Digimationator";
pub const PAD: f32 = 12.0;
pub const HEARD_SECONDS: f64 = 3.0;
pub const HEARD_PERIOD: f64 = 0.6;
pub const GLOW_PERIOD: f64 = 1.6;
pub const ATTEMPT_MODES: [&str; 3] = ["FT8", "FT4", "FT2"];
/// "Are you still there?" every this long (10 minutes)
pub const PRESENCE_SECONDS: f64 = 600.0;
/// the question is asked this long before the deadline
pub const PRESENCE_WARN: f64 = 60.0;
/// older spots get a faint red Ago cell
pub const STALE_MINUTES: i64 = 10;
/// log characters visible without wrapping at startup
pub const LOG_COLUMNS: usize = 125;
pub const HUNT_ON_TEXT: &str = "Auto-Hunt Enabled";
pub const HUNT_OFF_TEXT: &str = "Auto-Hunt Disabled";
pub const CQ_ON_TEXT: &str = "Auto CQ/Answer Enabled";
pub const CQ_OFF_TEXT: &str = "Auto CQ/Answer Disabled";
pub const SPOTS_TITLE: &str = "Current POTA Spots (Not worked today)";
pub const DONE_TITLE: &str = "Current POTA Spots (Worked/Skipped today)";
/// a long park name
pub const PARK_SAMPLE: &str = "JP-2105 Aichi Children's World Prefectural Park";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Radio {
    Tx,
    Decode,
    Rx,
}

impl Radio {
    /// label, pulse period (s), dark and bright colour
    fn spec(self) -> (&'static str, f64, (u8, u8, u8), (u8, u8, u8)) {
        match self {
            Radio::Tx => ("TRANSMITTING", 1.2, (0x8a, 0x10, 0x10), (0xF0, 0x3a, 0x3a)),
            Radio::Decode => ("DECODING", 0.6, (0x0a, 0x6e, 0x3a), (0x2f, 0xc9, 0x6e)),
            Radio::Rx => ("RECEIVING", 2.4, (0x0b, 0x5a, 0x34), (0x17, 0x93, 0x58)),
        }
    }
}

enum Timer {
    RigClean(bool),
    CheckMode(String),
}

/// What the hunter does to WSJT-X and its Log QSO window.
struct AppIo {
    listener: Arc<Mutex<Arc<ListenerShared>>>,
    move_tx: Arc<AtomicBool>,
    post: Post,
}

impl HunterIo for AppIo {
    fn send_reply(&mut self, d: &Decode, keep_tx: bool, cq_on: bool) -> Result<(), String> {
        // Without Ctrl, WSJT-X moves only Rx when Hold Tx Freq is checked. With Auto
        // CQ/Answer on, Tx never moves, for any Reply (hunting included).
        let keep = keep_tx || cq_on;
        let mods = if self.move_tx.load(Ordering::SeqCst) && !keep { wsjtx_udp::MOD_CTRL } else { wsjtx_udp::MOD_NONE };
        self.listener.lock().unwrap().send(&wsjtx_udp::build_reply(d, mods))
    }

    fn send_halt(&mut self, id: &str) -> Result<(), String> {
        self.listener.lock().unwrap().send(&wsjtx_udp::build_halt_tx(id, false))
    }

    fn can_send_text(&self) -> bool {
        true
    }

    /// Start WSJT-X calling `text` (our CQ) through UI Automation on a worker thread.
    fn send_text(&mut self, _id: &str, text: &str) -> Result<(), String> {
        let post = self.post.clone();
        let text = text.to_string();
        std::thread::Builder::new()
            .name("start-cq".into())
            .spawn(move || {
                let r = wsjtx_gui::start_cq(&text).unwrap_or_else(|e| format!("error: {e}"));
                post(Event::CqTx(r));
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn press_log_ok(&mut self) -> bool {
        logdialog::press(true)
    }

    fn log_window_open(&mut self) -> Option<bool> {
        Some(!logdialog::find_log_windows().is_empty())
    }
}

#[derive(PartialEq, Clone)]
struct SpotsKey(Option<u64>, Option<&'static str>, String, usize, usize, Option<String>, bool, Option<u64>, i64);

/// The sections that can be popped out into their own windows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Card {
    Info,
    Log,
    Notices,
    Spots,
    Done,
    Worked,
}

impl Card {
    const ALL: [Card; 6] = [Card::Info, Card::Log, Card::Notices, Card::Spots, Card::Done, Card::Worked];

    /// the name in settings.json
    fn key(self) -> &'static str {
        match self {
            Card::Info => "info",
            Card::Log => "activity",
            Card::Notices => "new_parks",
            Card::Spots => "spots",
            Card::Done => "worked_skipped",
            Card::Worked => "worked_today",
        }
    }

    fn from_key(k: &str) -> Option<Card> {
        Card::ALL.into_iter().find(|c| c.key() == k)
    }

    /// window title part
    fn title(self) -> &'static str {
        match self {
            Card::Info => "Current Info",
            Card::Log => "Activity",
            Card::Notices => "POTA Spots (New to me)",
            Card::Spots => "Current POTA Spots (Not worked today)",
            Card::Done => "Current POTA Spots (Worked/Skipped today)",
            Card::Worked => "Worked today",
        }
    }
}

/// A section's own window: where it opens and where it is now (points on screen).
struct Pop {
    popped: bool,
    init: Rect,
    cur: Rect,
    /// the geometry has been checked against the monitors
    placed: bool,
    /// hover hints of this window (the main window has its own)
    tips: Tips,
    hwnd: isize,
    titled: Option<bool>,
}

impl Pop {
    fn new(popped: bool, r: Rect) -> Self {
        Pop { popped, init: r, cur: r, placed: false, tips: Tips::default(), hwnd: 0, titled: None }
    }
}

/// Pane dividers, in pixels from the panes' origin.
struct Panes {
    /// width of the left (log) pane
    left: f32,
    /// heights of the spots and done cards in the right column
    a: f32,
    b: f32,
    placed: bool,
    last: Vec2,
    drag: Option<u8>,
    /// height of the new-parks card above the spots card plus its gap (0 when there is none)
    nh: f32,
}

/// One line of the Activity card's static area.
struct StaticLine {
    /// HH:MM:SS shown before the text; empty for a continuation line
    ts: String,
    text: String,
    /// drawn as a link to the pota.app account settings
    link: bool,
    /// a button after the text: (label, enabled)
    button: Option<(&'static str, bool)>,
}

impl StaticLine {
    fn new(ts: String, text: String) -> Self {
        StaticLine { ts, text, link: false, button: None }
    }
}

/// Word-wrap the static lines to `avail` px. Continuation rows have no stamp; the
/// button goes after the last row, or on a row of its own when it doesn't fit there
/// (`btn_w` is its width plus the gap).
fn wrap_static(info: Vec<StaticLine>, font: &crate::theme::Font, avail: f32, btn_w: f32) -> Vec<StaticLine> {
    let mut out = Vec::new();
    for l in info {
        let mut rows: Vec<String> = Vec::new();
        let mut cur = String::new();
        for word in l.text.split(' ') {
            let try_ = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if !cur.is_empty() && measure(font, &try_) > avail {
                rows.push(std::mem::take(&mut cur));
                cur = word.to_string();
            } else {
                cur = try_;
            }
        }
        rows.push(cur);
        let n = rows.len();
        for (i, text) in rows.into_iter().enumerate() {
            let last = i + 1 == n;
            let mut row = StaticLine { ts: if i == 0 { l.ts.clone() } else { String::new() }, text, link: l.link, button: None };
            if last {
                match l.button {
                    Some(b) if measure(font, &row.text) + btn_w <= avail => row.button = Some(b),
                    Some(b) => {
                        out.push(row);
                        row = StaticLine { ts: String::new(), text: String::new(), link: false, button: Some(b) };
                    }
                    None => {}
                }
            }
            out.push(row);
        }
    }
    out
}

enum StaticClick {
    Link,
    Refresh,
}

pub struct App {
    dir: PathBuf,
    pub s: Settings,
    rx: Receiver<Event>,
    post: Post,
    listener: Arc<Mutex<Arc<ListenerShared>>>,
    poller: Arc<SpotPoller>,
    stats: Arc<StatsPoller>,
    aclog: AclogClient,
    move_tx: Arc<AtomicBool>,
    pub h: Hunter,
    counts: Rc<RefCell<Option<Arc<HuntCounts>>>>,
    pub pota_status: String,
    last_wsjtx: Option<Instant>,
    t0: Instant,
    pub theme_name: &'static str,
    pub p: Palette,
    pub f: Fonts,
    log: LogView,
    spots_tree: Table,
    done_tree: Table,
    worked_tree: Table,
    worked_filter: String,
    spots_key: Option<SpotsKey>,
    spots_title: String,
    done_title: String,
    spots_count: String,
    /// mouse selection in the static lines of the Activity card: (line, char) anchor and focus
    static_sel: Option<((usize, usize), (usize, usize))>,
    /// app-wide drag-to-select for any other text
    tsel: crate::textsel::TextSelect,
    /// (text, HH:MM:SS) per static line slot: when that line's text last changed
    static_stamps: HashMap<&'static str, (String, String)>,
    done_count: String,
    worked_count: String,
    waiting: (String, bool),
    spinner_angle: f32,
    notices: Vec<(String, String, usize, String)>,
    heard: HashMap<String, f64>,
    /// (base call, band) -> `now()` of the last decode sent by that station
    last_heard: HashMap<(String, String), f64>,
    decode_until: f64,
    radio: Option<Radio>,
    periods: PeriodDetector,
    pub presence: PresenceTimer,
    presence_off: bool,
    typed: Vec<(f64, char)>,
    state_ready: bool,
    rig_key: Option<(String, String)>,
    rig_applied: bool,
    timers: Vec<(f64, Timer)>,
    pub panel: Panel,
    popup: Option<PopupMenu>,
    /// the section window the open popup belongs to (None: the main window)
    popup_in: Option<Card>,
    pops: std::collections::BTreeMap<Card, Pop>,
    /// where each section was last drawn in the main window (to size its pop-out window)
    card_rects: HashMap<Card, Rect>,
    /// pop-out (true) or dock-back (false) requests from the header buttons
    pop_req: Vec<(Card, bool)>,
    /// the section whose own window is being drawn
    in_pop: Option<Card>,
    dialog: Option<Dialog>,
    tips: Tips,
    panes: Panes,
    next_tick: f64,
    next_status: f64,
    first_status_done: bool,
    hwnd: isize,
    titled_dark: Option<bool>,
    day_left: String,
    spots_text: String,
    /// "ATT/NB/DNR off": the last rig change, shown as "[CAT: ...]" after the WSJT-X line in the log's static area.
    static_rig: String,
    layout_due: Option<f64>,
    last_outer: Option<Rect>,
    last_geometry: Option<Rect>,
    pub gear_rect: Rect,
    band_rect: Rect,
    mode_rect: Rect,
    ppp: f32,
    fit_right_w: f32,
    closing: bool,
    /// POTA_DEMO: steps that put the window in a given state for screenshots (testing)
    demo: Vec<String>,
    /// POTA_DEMO "tip": keep a sample tooltip showing
    demo_tip: bool,
    /// WSJT-X has reported a band since start-up: the waiting popup is gone for good
    wsjtx_seen: bool,
}

pub fn mono_clock(t0: Instant) -> impl Fn() -> f64 {
    move || t0.elapsed().as_secs_f64()
}

fn hhmmss(t: DateTime<Utc>) -> String {
    t.format("%H:%M:%S").to_string()
}

/// Whole minutes since a pota.app spot time (UTC), None if it can't be read.
pub fn age_minutes(spot_time: &str, now: DateTime<Utc>) -> Option<i64> {
    let t = chrono::NaiveDateTime::parse_from_str(spot_time, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(spot_time, "%Y-%m-%dT%H:%M:%S"))
        .ok()?
        .and_utc();
    Some(((now - t).num_seconds().max(0)) / 60)
}

/// Minutes since a pota.app spot time (UTC), e.g. "3m"; "1h05m" past an hour.
pub fn ago(spot_time: &str, now: DateTime<Utc>) -> String {
    match age_minutes(spot_time, now) {
        None => String::new(),
        Some(m) if m < 60 => format!("{m}m"),
        Some(m) => format!("{}h{:02}m", m / 60, m % 60),
    }
}

/// e.g. "7h 12m 5s"; units that are zero are left out ("2h 5s", "45s").
pub fn duration(seconds: i64) -> String {
    let parts: Vec<String> = [(seconds / 3600, "h"), (seconds % 3600 / 60, "m"), (seconds % 60, "s")]
        .iter()
        .filter(|(n, _)| *n != 0)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

fn period_text(p: f64) -> String {
    format!("{p}")
}

fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), k: f64) -> Color32 {
    let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * k).round() as u8;
    Color32::from_rgb(f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

fn pulse(now: f64, period: f64) -> f64 {
    (1.0 - (2.0 * std::f64::consts::PI * (now % period) / period).cos()) / 2.0
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, dir: PathBuf, fonts: (Fonts, egui::FontDefinitions), (tx, rx): (Sender<Event>, Receiver<Event>), fit_right_w: f32) -> App {
        let ctx = cc.egui_ctx.clone();
        ctx.set_fonts(fonts.1);
        let f = fonts.0;
        let s = settings::load(&dir.join("data").join("settings.json"));
        let repaint = ctx.clone();
        let post: Post = Arc::new(move |e| {
            let _ = tx.send(e);
            repaint.request_repaint();
        });
        let listener = Arc::new(Mutex::new(wsjtx_udp::start_listener(post.clone(), &s.udp_group, s.udp_port)));
        let poller = pota_spots::start_poller(post.clone(), dir.join("data").join("spots_cache.json"));
        let stats = pota_auth::start_stats_poller(post.clone());
        let aclog = AclogClient::new(post.clone(), &s.aclog_host, s.aclog_port);
        let move_tx = Arc::new(AtomicBool::new(s.move_tx));
        let t0 = Instant::now();
        let io = AppIo { listener: listener.clone(), move_tx: move_tx.clone(), post: post.clone() };
        let mut h = Hunter::new(Box::new(io), Box::new(mono_clock(t0)), Box::new(Utc::now));
        h.cq_text = s.cq_text.clone();
        h.dry_run = s.dry_run;
        h.auto_log = s.auto_log;
        h.pounce_signoffs = s.pounce_signoffs;
        h.new_parks_only = s.new_parks_only;
        h.default_attempts = s.max_attempts;
        h.attempts_by_mode = s.attempts_by_mode.iter().map(|(k, v)| (k.clone(), *v)).collect();
        h.min_snr = s.min_snr;
        let counts: Rc<RefCell<Option<Arc<HuntCounts>>>> = Rc::new(RefCell::new(None));
        let c2 = counts.clone();
        h.park_is_new = Box::new(move |r| c2.borrow().as_ref().is_some_and(|c| c.park(r) == 0));
        let theme_name = theme::resolve(&s.theme);
        let p = Palette::for_name(theme_name);
        let spot_cols = || {
            vec![Col::new("ago", "Spot", "99m").tip("Age of the most recent spot on pota.app"),
                 Col::new("heard", "Heard", "99m").tip("Age of the most recent WSJT-X decode"),
                 Col::new("call", "Call", "AE4XO/P (999)").tip("Callsign of the activator"),
                 Col::new("freq", "kHz", "28074.0").tip("Frequency of the activator")]
        };
        let park_col = || Col::new("park", "Park", PARK_SAMPLE).tip("Park reference and name of the activator");
        let mut spots_cols = spot_cols();
        spots_cols.push(park_col());
        let mut done_cols = spot_cols();
        done_cols.push(Col::new("ws", "W/S", "W/S").tip("\"W\"orked or \"S\"kipped spot"));
        done_cols.push(park_col());
        let worked_cols = vec![
            Col::new("time", "Time", "00:00").tip("Logged QSO time"),
            Col::new("call", "Call", "JN1JYD/P2").tip("Logged callsign"),
            Col::new("band", "Band", "160m").tip("Logged band"),
            Col::new("mode", "Mode", "FT8").tip("Logged mode"),
            Col::new("logged", "Log", "Log").tip("Logged successful or not"),
            Col::new("park", "Park", PARK_SAMPLE).tip("Logged park reference and name"),
        ];
        let pops = s
            .popouts
            .iter()
            .filter_map(|(k, g)| {
                let r = Rect::from_min_size(Pos2::new(g.x as f32, g.y as f32), Vec2::new(g.w as f32, g.h as f32));
                Some((Card::from_key(k)?, Pop::new(g.popped, r)))
            })
            .collect();
        let mut app = App {
            dir: dir.clone(),
            s,
            rx,
            post,
            listener,
            poller,
            stats,
            aclog,
            move_tx,
            h,
            counts,
            pota_status: String::new(),
            last_wsjtx: None,
            t0,
            theme_name,
            p,
            f,
            log: LogView::new(),
            spots_tree: Table::new("spots", spots_cols, 8),
            done_tree: Table::new("done", done_cols, 4),
            worked_tree: Table::new("worked", worked_cols, 8),
            worked_filter: String::new(),
            spots_key: None,
            spots_title: SPOTS_TITLE.into(),
            done_title: DONE_TITLE.into(),
            spots_count: "0".into(),
            static_sel: None,
            tsel: Default::default(),
            static_stamps: HashMap::new(),
            done_count: "0".into(),
            worked_count: "0".into(),
            waiting: (String::new(), true),
            spinner_angle: 0.0,
            notices: vec![],
            heard: HashMap::new(),
            last_heard: HashMap::new(),
            decode_until: 0.0,
            radio: None,
            periods: PeriodDetector::default(),
            presence: PresenceTimer::new(PRESENCE_SECONDS, PRESENCE_WARN, Box::new(mono_clock(t0))),
            presence_off: false,
            typed: vec![],
            state_ready: false,
            rig_key: None,
            rig_applied: false,
            timers: vec![],
            panel: Panel::default(),
            popup: None,
            popup_in: None,
            pops,
            card_rects: HashMap::new(),
            pop_req: vec![],
            in_pop: None,
            dialog: None,
            tips: Tips::default(),
            panes: Panes { left: 0.0, a: 0.0, b: 0.0, placed: false, last: Vec2::ZERO, drag: None, nh: 0.0 },
            next_tick: 0.0,
            next_status: 1.0,
            first_status_done: false,
            hwnd: 0,
            titled_dark: None,
            day_left: String::new(),
            spots_text: "Spots (pota.app): waiting for first fetch".into(),
            static_rig: String::new(),
            layout_due: None,
            last_outer: None,
            last_geometry: None,
            gear_rect: Rect::NOTHING,
            band_rect: Rect::NOTHING,
            mode_rect: Rect::NOTHING,
            ppp: 1.0,
            fit_right_w,
            closing: false,
            demo: std::env::var("POTA_DEMO").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default(),
            demo_tip: false,
            wsjtx_seen: false,
        };
        // demo screenshots skip the WSJT-X popup unless they ask for it ("wait")
        app.wsjtx_seen = !app.demo.is_empty() && !app.demo.iter().any(|s| s == "wait");
        for t in [&mut app.spots_tree, &mut app.done_tree, &mut app.worked_tree] {
            t.size_from_samples(&app.f);
        }
        app.seed_worked();
        let (u, pw) = (app.s.pota_user.clone(), app.s.pota_password.clone());
        app.apply_pota_login(&u, &pw, false);
        app.after(2.0, Timer::RigClean(false));
        app.render_worked();
        app
    }

    pub fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    fn after(&mut self, secs: f64, t: Timer) {
        let at = self.now() + secs;
        self.timers.push((at, t));
    }

    // --- Rig receive settings (Yaesu via ACLog) ----------------------------------

    /// ATT, NB and DNR off on the main receiver (the radio may remember them per band).
    fn rig_clean(&mut self, quiet: bool) {
        if self.s.rig_clean {
            self.rig_applied = true;
            self.aclog.set_rig(Some(false), Some(false), Some(false), false, "ATT/NB/DNR off", quiet);
        }
    }

    fn rig_watch(&mut self) {
        let (Some(band), mode) = (self.h.band(), self.h.mode()) else { return };
        if mode.is_empty() {
            return;
        }
        let key = (band.to_string(), mode);
        if self.rig_key.as_ref() == Some(&key) {
            return;
        }
        let first = self.rig_key.is_none();
        self.rig_key = Some(key.clone());
        if !first {
            self.log.clear(); // new band/mode: start the Activity log afresh
            self.static_rig.clear(); // until the rig has been set on the new band
        }
        if !first && self.s.rig_clean {
            // Let the radio finish changing band before sending, and once more in case
            // it restores its per-band memory late.
            self.after(1.5, Timer::RigClean(false));
            self.after(5.0, Timer::RigClean(true));
        }
    }

    fn restore_wants(&self) -> (Option<bool>, Option<bool>, Option<bool>) {
        let w = |v: &str| match v {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        };
        (w(&self.s.rig_restore_att), w(&self.s.rig_restore_nb), w(&self.s.rig_restore_dnr))
    }

    /// On close, put back what the operator chose (ACLog can't read the rig's old state).
    fn rig_restore(&self, why: &str) {
        let (a, n, d) = self.restore_wants();
        if self.rig_applied && (a.is_some() || n.is_some() || d.is_some()) {
            self.aclog.set_rig(a, n, d, true, why, false);
        }
    }

    // --- theme ------------------------------------------------------------------

    pub fn apply_theme(&mut self, ctx: &egui::Context, mode: Option<&str>) {
        if let Some(m) = mode {
            self.s.theme = m.to_string();
        }
        self.theme_name = theme::resolve(&self.s.theme);
        self.p = Palette::for_name(self.theme_name);
        self.style_egui(ctx);
        self.titled_dark = None;
        self.spots_key = None; // rebuild the lists with the new row colours
        self.refresh_spots_list();
        self.render_worked();
    }

    pub fn set_theme(&mut self, ctx: &egui::Context, mode: &str) {
        self.apply_theme(ctx, Some(mode));
        self.save_settings();
    }

    fn style_egui(&self, ctx: &egui::Context) {
        let p = &self.p;
        ctx.all_styles_mut(|s| {
            let v = &mut s.visuals;
            v.dark_mode = p.dark;
            // light text on dark needs a heavier coverage curve than dark on light
            v.text_options.color_transfer_function = if p.dark {
                eframe::epaint::FontColorTransferFunction::DARK_MODE_DEFAULT
            } else {
                eframe::epaint::FontColorTransferFunction::LIGHT_MODE_DEFAULT
            };
            v.selection.bg_fill = p.select;
            v.selection.stroke = Stroke::new(1.0, p.text);
            v.text_cursor.stroke = Stroke::new(1.0, p.text);
            v.override_text_color = Some(p.text);
            v.extreme_bg_color = p.surface2;
            v.panel_fill = p.bg;
            v.window_fill = p.surface;
            s.spacing.item_spacing = Vec2::ZERO;
            s.interaction.selectable_labels = false;
        });
    }

    // --- settings -----------------------------------------------------------------

    /// Push the settings into the hunter and save them.
    pub fn settings_changed(&mut self) {
        let h = &mut self.h;
        h.dry_run = self.s.dry_run;
        h.auto_log = self.s.auto_log;
        h.pounce_signoffs = self.s.pounce_signoffs;
        if h.new_parks_only != self.s.new_parks_only && h.enabled {
            let msg = if self.s.new_parks_only { "Auto-Hunt now only hunts NEW parks" } else { "Auto-Hunt now hunts all parks" };
            self.log(msg, Tag::Hunt);
        }
        let h = &mut self.h;
        h.new_parks_only = self.s.new_parks_only;
        h.cq_text = self.s.cq_text.clone();
        h.attempts_by_mode = self.s.attempts_by_mode.iter().map(|(k, v)| (k.clone(), (*v).max(1))).collect();
        h.min_snr = self.s.min_snr;
        if self.s.aclog_host.trim().is_empty() {
            self.s.aclog_host = "127.0.0.1".into();
        }
        self.move_tx.store(self.s.move_tx, Ordering::SeqCst);
        self.aclog.set_addr(self.s.aclog_host.trim(), self.s.aclog_port);
        self.save_settings();
    }

    pub fn save_settings(&mut self) {
        if let Err(e) = settings::save(&self.dir.join("data").join("settings.json"), &self.s) {
            self.log(&format!("Could not save settings: {e}"), Tag::Error);
        }
    }

    // --- log ------------------------------------------------------------------------

    pub fn log(&mut self, text: &str, tag: Tag) {
        self.log.add(hhmmss(Utc::now()), text.to_string(), tag, vec![], false);
    }

    /// Move the hunter's new log lines (and flags) into the window.
    fn drain_hunter(&mut self) {
        for l in std::mem::take(&mut self.h.logs) {
            self.log.add(hhmmss(Utc::now()), l.text, l.tag, l.actions, l.new);
        }
        if std::mem::take(&mut self.h.worked_changed) {
            self.save_state();
            self.render_worked();
        }
        self.h.cq_stopped = false; // the Auto CQ/Answer button reads the hunter directly
    }

    // --- state.json ---------------------------------------------------------------

    fn save_state(&mut self) {
        if !self.state_ready {
            return;
        }
        let path = self.dir.join("data").join("state.json");
        let tmp = self.dir.join("data").join("state.json.tmp");
        let body = serde_json::to_string(&self.h.export_state()).unwrap_or_default();
        let result = (|| -> std::io::Result<()> {
            std::fs::write(&tmp, body)?;
            for attempt in 0..5 {
                // a scanner can hold the file for a moment
                match std::fs::rename(&tmp, &path) {
                    Ok(()) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 4 => {
                        std::thread::sleep(Duration::from_millis(200))
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.log(&format!("Could not save {}: {e}", path.display()), Tag::Error);
        }
    }

    fn load_state(&mut self) {
        if let Ok(text) = std::fs::read_to_string(self.dir.join("data").join("state.json")) {
            if let Ok(v) = serde_json::from_str(&text) {
                self.h.import_state(&v);
            }
        }
        self.state_ready = true;
        self.h.worked_changed = false;
        self.save_state();
    }

    fn seed_worked(&mut self) {
        let path = if self.s.adif_path.is_empty() { settings::find_adif() } else { PathBuf::from(&self.s.adif_path) };
        let qsos = read_adif_today(&path, Utc::now().date_naive());
        let n = qsos.len();
        self.h.seed_worked(qsos);
        self.h.worked_changed = false;
        if path.exists() {
            self.log(&format!("Loaded {n} QSO(s) from today (UTC) out of {}", path.display()), Tag::Info);
        } else {
            self.log(&format!("WSJT-X log not found at {}; starting with an empty worked list", path.display()), Tag::Error);
        }
        self.load_state();
    }

    // --- pota.app -------------------------------------------------------------------

    pub fn pota_summary(&self) -> String {
        if self.s.pota_user.is_empty() {
            return "Not signed in; add your login for hunt counts".into();
        }
        if self.pota_status.is_empty() {
            format!("{}  ·  signing in", self.s.pota_user)
        } else {
            self.pota_status.clone()
        }
    }

    pub fn apply_pota_login(&mut self, user: &str, password: &str, save: bool) {
        let user = user.trim().to_string();
        self.s.pota_user = user.clone();
        self.s.pota_password = password.to_string();
        *self.counts.borrow_mut() = None;
        self.spots_key = None;
        self.pota_status = if !user.is_empty() && !password.is_empty() { format!("Signing in as {user}") } else { String::new() };
        self.stats.configure(&user, password);
        if save {
            self.save_settings();
        }
    }

    // --- WSJT-X ---------------------------------------------------------------------

    pub fn wsjtx_link(&self) -> (String, bool) {
        let l = self.listener.lock().unwrap().clone();
        let addr = *l.wsjtx_addr.lock().unwrap();
        if self.connected() {
            if let Some(a) = addr {
                let id = self.h.status.as_ref().map(|s| s.id.clone()).unwrap_or_default();
                return (format!("Receiving from {id} at {}:{}", a.ip(), a.port()), true);
            }
        }
        if self.last_wsjtx.is_some() && addr.is_some() {
            return ("WSJT-X went quiet".into(), false);
        }
        (format!("No packets yet on {}:{}", l.group, l.port), false)
    }

    pub fn listen_addr(&self) -> (String, u16) {
        let l = self.listener.lock().unwrap();
        (l.group.clone(), l.port)
    }

    /// Listen on a new address and port: replace the listener thread.
    pub fn apply_udp(&mut self, group: &str, port: u16) {
        if (group.to_string(), port) == self.listen_addr() {
            return;
        }
        let new = wsjtx_udp::start_listener(self.post.clone(), group, port);
        let old = std::mem::replace(&mut *self.listener.lock().unwrap(), new);
        old.stop();
        self.last_wsjtx = None;
        self.s.udp_group = group.to_string();
        self.s.udp_port = port;
        self.save_settings();
        self.log(&format!("Now listening to WSJT-X on {group}:{port}"), Tag::Info);
    }

    fn connected(&self) -> bool {
        self.last_wsjtx.is_some_and(|t| t.elapsed().as_secs_f64() < 35.0) && self.h.status.is_some()
    }

    fn dispatch(&mut self, e: Event, ctx: &egui::Context) {
        match e {
            Event::Udp(msg) => {
                self.last_wsjtx = Some(Instant::now());
                match msg {
                    Message::Status(st) => {
                        self.h.handle_status(st);
                        self.update_wsjtx_status(); // show Tx changes immediately
                        self.rig_watch();
                    }
                    Message::Decode(d) => {
                        self.h.handle_decode(&d);
                        self.record_heard(&d);
                        self.flash_heard(&d);
                    }
                    Message::QsoLogged(q) => {
                        let entry = self.h.handle_qso_logged(&q);
                        if self.s.ding {
                            sound::play(&self.s.ding_path);
                        }
                        if self.s.aclog_enabled {
                            self.aclog.log_qso(q, entry);
                        }
                    }
                    Message::Close { .. } => {
                        self.log("WSJT-X closed", Tag::Error);
                        self.last_wsjtx = None;
                    }
                    _ => {}
                }
            }
            Event::UdpInfo(t) => self.log(&t, Tag::Info),
            Event::UdpError(t) => self.log(&t, Tag::Error),
            Event::Spots(t) => self.h.set_spots(t),
            Event::SpotsError(t) => self.log(&t, Tag::Error),
            Event::PotaStats(c) => {
                self.pota_status = format!("Signed in as {}: {} parks, {} activators hunted",
                                           self.s.pota_user, c.parks.len(), c.activators.len());
                *self.counts.borrow_mut() = Some(c);
                self.spots_key = None;
            }
            Event::PotaStatsError(t) => {
                self.pota_status = match t.split_once("sign-in failed") {
                    Some((_, rest)) => format!("Sign-in failed{rest}"),
                    None => t.clone(),
                };
                self.log(&t, Tag::Error);
            }
            Event::Aclog(t, Tag::Info) if t.ends_with("ATT/NB/DNR off") => self.static_rig = t,
            Event::Aclog(t, tag) => self.log(&t, tag),
            Event::Retune(what, value, result) => {
                if result != "ok" {
                    self.log(&format!("Could not switch WSJT-X's {what} to {value} ({result}); change it in WSJT-X"), Tag::Error);
                }
            }
            Event::CqTx(r) => {
                if r == "notfound" {
                    self.log("Could not find WSJT-X's Enable Tx checkbox; turn Tx on yourself", Tag::Error);
                } else if r.starts_with("error") {
                    self.log(&format!("Could not switch Tx on in WSJT-X ({r})"), Tag::Error);
                }
            }
        }
        let _ = ctx;
    }

    fn update_wsjtx_status(&mut self) {
        let mut state = None;
        if self.connected() {
            let st = self.h.status.clone().unwrap();
            state = Some(if st.transmitting { Radio::Tx } else if st.decoding { Radio::Decode } else { Radio::Rx });
            if state == Some(Radio::Decode) {
                // WSJT-X's decoding flag drops before the period ends; keep showing DECODING
                // until the bar recycles, for a decode that began late in the period.
                let (period, now) = (self.cycle_period(), epoch_now());
                if now % period > period / 2.0 {
                    self.decode_until = ((now / period).floor() + 1.0) * period;
                }
            } else if state == Some(Radio::Rx) && epoch_now() < self.decode_until {
                state = Some(Radio::Decode);
            }
        }
        self.set_radio_state(state);
    }

    fn set_radio_state(&mut self, state: Option<Radio>) {
        if state == self.radio {
            return;
        }
        if state == Some(Radio::Decode) {
            let mode = self.h.status.as_ref().map(|s| s.mode.clone()).unwrap_or_default();
            self.periods.decoding_started(&mode, epoch_now());
        }
        self.radio = state;
    }

    /// Seconds in the current T/R period (see cycles.rs).
    fn cycle_period(&self) -> f64 {
        let st = self.h.status.as_ref();
        cycles::cycle_seconds(st.map(|s| s.mode.as_str()).unwrap_or(""), st.map(|s| s.tr_period).unwrap_or(0), self.periods.period)
    }

    /// Any new decode sent by a station restarts its "Heard" age in the spot lists.
    fn record_heard(&mut self, d: &Decode) {
        let Some(band) = self.h.band() else { return };
        if !d.new {
            return;
        }
        let Some(p) = parse_message(&d.message) else { return };
        let call = base_call(&p.call);
        if call == self.h.my_call() {
            return;
        }
        self.last_heard.insert((call, band.to_string()), self.now());
        self.refresh_heard_cells();
    }

    /// "12s" under a minute, else whole minutes ("3m"); "-" if never heard on this band.
    fn heard_text(&self, call: &str) -> String {
        let Some(band) = self.h.band() else { return "-".into() };
        match self.last_heard.get(&(base_call(call), band.to_string())) {
            None => "-".into(),
            Some(t) => {
                let s = (self.now() - t).max(0.0) as u64;
                if s < 60 { format!("{s}s") } else { format!("{}m", s / 60) }
            }
        }
    }

    /// Update the Heard cells in place (called each second and on every decode).
    fn refresh_heard_cells(&mut self) {
        let texts: Vec<Vec<String>> = [&self.spots_tree, &self.done_tree]
            .iter()
            .map(|t| t.rows.iter().map(|r| r.data.as_ref().map(|d| self.heard_text(&d.call)).unwrap_or_default()).collect())
            .collect();
        let mut changed = false;
        for (t, texts) in [&mut self.spots_tree, &mut self.done_tree].into_iter().zip(texts) {
            for (r, text) in t.rows.iter_mut().zip(texts) {
                if let Some(c) = r.cells.get_mut(1) {
                    if c.text != text {
                        c.text = text;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.spots_tree.fit_columns(&self.f);
            self.done_tree.fit_columns(&self.f);
        }
    }

    /// Any decode sent by a station that is spotted on pota.app on this band makes its
    /// spot row pulse green for HEARD_SECONDS, whatever the message says.
    fn flash_heard(&mut self, d: &Decode) {
        let (Some(band), Some(spots)) = (self.h.band(), self.h.spots.clone()) else { return };
        if !d.new {
            return;
        }
        let Some(p) = parse_message(&d.message) else { return };
        let call = base_call(&p.call);
        if call == self.h.my_call() || spots.spots_for(&call, Some(band)).is_empty() {
            return;
        }
        self.heard.insert(call.clone(), self.now() + HEARD_SECONDS);
        // (only the Spots list pulses; the Worked/Skipped list stays still)
        for t in [&mut self.spots_tree] {
            for r in t.rows.iter_mut() {
                if r.data.as_ref().is_some_and(|d| base_call(&d.call) == call) {
                    r.tags.heard = true;
                }
            }
        }
    }

    pub fn retune_blocker(&mut self) -> Option<String> {
        self.h.retune_blocker()
    }

    fn retune_menu(&mut self, which: &str) {
        let rect = if which == "band" { self.band_rect } else { self.mode_rect };
        let anchor = Pos2::new(rect.left(), rect.bottom() + 2.0);
        let now = self.now();
        if let Some(why) = self.h.retune_blocker() {
            self.popup = Some(PopupMenu::new(vec![Item::Heading(format!("Can't change {which}: {why}"))], anchor, now));
            self.popup_in = None;
            return;
        }
        let (names, current): (Vec<String>, String) = if which == "band" {
            (BANDS.iter().map(|b| b.0.to_string()).collect(), self.h.band().unwrap_or("").to_string())
        } else {
            (wsjtx_gui::MODES.iter().map(|m| m.to_string()).collect(), self.h.mode())
        };
        let items = names
            .into_iter()
            .map(|n| Item::Row {
                label: format!("{}{n}", if n == current { "✓ " } else { "" }),
                cmd: Some(if which == "band" { MenuCmd::Band(n) } else { MenuCmd::Mode(n) }),
                subtitle: String::new(),
            })
            .collect();
        self.popup = Some(PopupMenu::new(items, anchor, now));
        self.popup_in = None;
    }

    pub fn retune(&mut self, band: Option<String>, mode: Option<String>) {
        if let Some(why) = self.h.retune_blocker() {
            self.log(&format!("Not changing band or mode: {why}"), Tag::Error);
            return;
        }
        let mut steps: Vec<(String, String)> = vec![]; // mode first: it retunes on the current band
        if let Some(m) = mode.filter(|m| *m != self.h.mode()) {
            self.log(&format!("Switching WSJT-X to {m}"), Tag::Info);
            steps.push(("mode".into(), m.clone()));
            self.after(8.0, Timer::CheckMode(m));
        }
        let mut restore = false;
        if let Some(b) = band.filter(|b| Some(b.as_str()) != self.h.band()) {
            self.log(&format!("Switching WSJT-X to {b}"), Tag::Info);
            steps.push(("band".into(), b));
            restore = true;
        }
        if steps.is_empty() {
            return;
        }
        // Press WSJT-X's band/mode buttons in order on a worker thread (PowerShell takes a
        // second each); first leave the band with ATT/NB/DNR at their defaults.
        let post = self.post.clone();
        let band_now = self.h.band().unwrap_or("").to_string();
        let (a, n, d) = self.restore_wants();
        let rig = (restore && self.rig_applied && (a.is_some() || n.is_some() || d.is_some())).then(|| self.aclog.clone());
        std::thread::Builder::new()
            .name("retune".into())
            .spawn(move || {
                if let Some(aclog) = rig {
                    aclog.set_rig(a, n, d, true, &format!("QSY, setting {band_now} ATT/NB/DNR back to normal"), false);
                    std::thread::sleep(Duration::from_millis(500));
                }
                for (i, (what, value)) in steps.into_iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(Duration::from_millis(1500)); // let WSJT-X finish the first change
                    }
                    let r = if what == "mode" { wsjtx_gui::set_mode(&value) } else { wsjtx_gui::set_band(&value) };
                    post(Event::Retune(what, value, r.unwrap_or_else(|e| format!("error: {e}"))));
                }
            })
            .ok();
    }

    // --- Auto modes -------------------------------------------------------------------

    fn toggle_hunt(&mut self) {
        if self.h.enabled {
            self.set_hunting(false);
        } else {
            self.confirm_enable(DialogKind::ConfirmHunt);
        }
    }

    fn toggle_cq(&mut self) {
        if self.h.cq_enabled {
            self.set_cq(false);
        } else {
            self.confirm_enable(DialogKind::ConfirmCq);
        }
    }

    fn confirm_enable(&mut self, kind: DialogKind) {
        if self.dialog.as_ref().is_some_and(|d| d.kind.is_confirm()) {
            return;
        }
        if self.dialog.is_none() {
            self.dialog = Some(Dialog::new(kind));
        }
    }

    pub fn set_hunting(&mut self, on: bool) {
        self.h.enabled = on;
        self.h.pending.clear();
        if on {
            let mode = if self.h.dry_run { "dry run, nothing will be transmitted" } else { "will call stations" };
            let only = if self.h.new_parks_only { "; only hunting NEW parks" } else { "" };
            self.log(&format!("Auto-Hunt enabled ({mode}{only})"), Tag::Hunt);
        } else {
            self.log("Auto-Hunt disabled. Calls already in progress in WSJT-X are left alone.", Tag::Hunt);
        }
        self.presence_sync(on);
    }

    pub fn set_cq(&mut self, on: bool) {
        self.h.cq_enabled = on;
        self.h.callers.clear();
        if on {
            let mode = if self.h.dry_run { "dry run, nothing will be transmitted" } else { "will call CQ and answer callers" };
            self.log(&format!("Auto CQ/Answer enabled ({mode})"), Tag::Hunt);
        } else {
            let halted = self.h.stop_cq();
            self.drain_hunter();
            self.log(&format!("Auto CQ/Answer disabled{}", if halted { "; CQ halted" } else { "." }), Tag::Hunt);
        }
        self.presence_sync(on);
    }

    fn hunt_button(&self) -> (String, Color32, Color32) {
        let p = &self.p;
        if self.h.enabled {
            let mut notes = vec![];
            if self.h.new_parks_only {
                notes.push("New parks only");
            }
            if self.h.dry_run {
                notes.push("Dry run");
            }
            let text = if notes.is_empty() { HUNT_ON_TEXT.to_string() } else { format!("{HUNT_ON_TEXT} ({})", notes.join(", ")) };
            (text, p.on, p.on_hover)
        } else {
            (HUNT_OFF_TEXT.into(), p.off, p.off_hover)
        }
    }

    fn cq_button(&self) -> (String, Color32, Color32) {
        let p = &self.p;
        if self.h.cq_enabled {
            let text = format!("{CQ_ON_TEXT}{}", if self.h.dry_run { " (Dry run)" } else { "" });
            (text, p.on, p.on_hover)
        } else {
            (CQ_OFF_TEXT.into(), p.off, p.off_hover)
        }
    }

    // --- "Are you still there?" ------------------------------------------------------

    /// Keep the timer in step with the Auto modes: enabling one starts a fresh countdown;
    /// with both off it stops.
    fn presence_sync(&mut self, enabling: bool) {
        if self.h.enabled || self.h.cq_enabled {
            if enabling {
                self.presence.arm();
            }
        } else {
            self.presence.disarm();
            if self.dialog.as_ref().is_some_and(|d| d.kind.is_presence()) {
                self.dialog = None;
            }
        }
    }

    /// Restart the presence timer (only while it isn't already asking).
    fn activity(&mut self) {
        if self.presence.phase() == Some(Phase::Ok) {
            self.presence.arm();
        }
    }

    fn bypass_keys(&mut self, c: char) {
        let now = self.now();
        self.typed.push((now, c.to_ascii_lowercase()));
        if self.typed.len() > 6 {
            self.typed.remove(0);
        }
        // only a hash of the word is in the source (FNV-1a 64 of the 6 lowercase letters)
        let hash = self.typed.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, t| {
            (h ^ t.1 as u64).wrapping_mul(0x100_0000_01b3)
        });
        if self.typed.len() == 6 && hash == 0x24ac_adea_4102_5e28 && now - self.typed[0].0 <= 1.0 {
            self.typed.clear(); // a repeat needs the whole word again
            if self.presence_off {
                self.log("Konami! Presence checking is already disabled until the application is restarted.", Tag::Hunt);
                return;
            }
            self.presence_off = true;
            if self.dialog.as_ref().is_some_and(|d| d.kind == DialogKind::PresenceAsk) {
                self.dialog = None;
                self.presence.arm();
            }
            self.log("Konami! Presence checking has been disabled until the application is restarted.", Tag::Hunt);
        }
    }

    fn presence_tick(&mut self) {
        let phase = self.presence.phase();
        if self.presence_off {
            if matches!(phase, Some(Phase::Ask) | Some(Phase::Expired)) {
                self.log("Konami! Presence detection is disabled, you would have been caught!", Tag::Hunt);
                self.presence.arm();
            }
            return;
        }
        let presence_open = self.dialog.as_ref().is_some_and(|d| d.kind.is_presence());
        if phase == Some(Phase::Expired) && !presence_open {
            self.presence_expired(); // e.g. the PC slept through the question
        } else if phase == Some(Phase::Ask) && !presence_open {
            self.dialog = Some(Dialog::new(DialogKind::PresenceAsk));
            theme::bell();
        }
    }

    /// Nobody answered: switch every Auto mode off now and say why.
    pub fn presence_expired(&mut self) {
        let mut off = vec![];
        if self.h.enabled {
            self.set_hunting(false);
            off.push("Auto-Hunt");
        }
        if self.h.cq_enabled {
            self.set_cq(false);
            off.push("Auto CQ/Answer");
        }
        self.presence.disarm();
        if self.dialog.as_ref().is_some_and(|d| d.kind.is_presence()) {
            self.dialog = None;
        }
        if off.is_empty() {
            return;
        }
        let names = off.join(" and ");
        self.log(&format!("No answer to \"Are you still there?\": {names} switched off."), Tag::Error);
        self.dialog = Some(Dialog::new(DialogKind::PresenceExpired(names)));
        theme::bell();
    }

    // --- spot lists --------------------------------------------------------------------

    /// Rebuild the spot lists only when the spots, band/mode or worked list change.
    fn refresh_spots_list(&mut self) {
        let h = &self.h;
        let target = h.target().map(|t| t.call.clone());
        // Pulse only while WSJT-X is really calling them: not before it has started, and
        // not once all our calls are out and we are just waiting for an answer.
        let calling = h.target().is_some_and(|t| t.saw_tx && t.idle_since == 0.0);
        let counts = self.counts.borrow().clone();
        let key = SpotsKey(
            h.spots.as_ref().map(|s| s.fetched_at.to_bits()),
            h.band(),
            h.mode(),
            h.worked.len(),
            h.skipped.len(),
            target.clone(),
            calling,
            counts.as_ref().map(|c| c.fetched_at.to_bits()),
            (epoch_now() / 60.0) as i64,
        );
        if self.spots_key.as_ref() == Some(&key) {
            return;
        }
        self.spots_key = Some(key);
        self.notices = if self.h.band().is_some() { self.h.new_parks_elsewhere() } else { vec![] };
        self.spots_tree.clear();
        self.done_tree.clear();
        let (Some(spots), Some(band)) = (self.h.spots.clone(), self.h.band()) else {
            self.spots_count = "0".into();
            self.done_count = "0".into();
            self.spots_title = SPOTS_TITLE.into();
            self.done_title = DONE_TITLE.into();
            // waiting for WSJT-X has its own popup (draw_wsjtx_wait); the list only waits
            // for pota.app
            self.waiting = (if self.h.spots.is_none() { "Waiting for pota.app...".into() } else { String::new() }, true);
            return;
        };
        let mode = self.h.mode();
        let list = spots.on(band, &mode);
        self.waiting = if list.is_empty() { (format!("No spots found for {band} {mode}"), false) } else { (String::new(), true) };
        let now = Utc::now();
        let mut rows: Vec<(&pota_spots::Spot, &'static str)> = list
            .iter()
            .map(|s| {
                let refs: Vec<String> = if s.reference.is_empty() { vec![] } else { vec![s.reference.clone()] };
                let st = if self.h.is_worked(&s.activator, band, &mode, &refs) {
                    "Worked"
                } else if self.h.is_skipped(&base_call(&s.activator), band, &mode, &refs) {
                    "Skipped"
                } else {
                    ""
                };
                (*s, st)
            })
            .collect();
        rows.sort_by(|a, b| b.0.spot_time.cmp(&a.0.spot_time)); // newest first (stable)
        let p = self.p.clone();
        let n_done = rows.iter().filter(|r| !r.1.is_empty()).count();
        self.done_count = n_done.to_string();
        let mono = self.now();
        for (s, worked) in rows {
            let mut tags = RowTags { worked: !worked.is_empty(), ..Default::default() };
            if target.as_deref() == Some(base_call(&s.activator).as_str()) {
                if calling {
                    tags.target = true;
                } else {
                    tags.pending = true;
                }
            }
            if worked.is_empty() && self.heard.get(&base_call(&s.activator)).copied().unwrap_or(0.0) > mono {
                tags.heard = true;
            }
            let dim = !worked.is_empty();
            let park_col = if dim { 5 } else { 4 };
            let (suffix, lead) = self.hunt_badges(s, counts.as_deref());
            let refs = if s.reference.is_empty() { vec![] } else { vec![s.reference.clone()] };
            let skipped = self.h.is_skipped(&base_call(&s.activator), band, &mode, &refs);
            let mut cells = vec![
                Cell { text: ago(&s.spot_time, now), stale: age_minutes(&s.spot_time, now).unwrap_or(0) > STALE_MINUTES, ..Default::default() },
                Cell::text(self.heard_text(&s.activator)),
                Cell { text: s.activator.clone(), link: Some(format!("https://pota.app/profile/{}", base_call(&s.activator))), link_tip: Some("Click to open on pota.app".into()), suffix, ..Default::default() },
                Cell::text(format!("{:.1}", s.frequency_khz)),
            ];
            if dim {
                let (text, fill, tip) = if worked == "Worked" {
                    ("W", p.ok, "Worked today on this band and mode,\nso it won't be called again")
                } else {
                    ("S", p.warn, "You skipped this station today;\nright-click the row and choose Unskip to undo")
                };
                cells.push(Cell { chip: Some(Chip { text: text.into(), fill, fg: p.accent_text, tip: Some(tip.into()) }), ..Default::default() });
            }
            let mut park = Cell::text(format!("{} {}", s.reference, s.park_name).trim().to_string());
            if !s.reference.is_empty() {
                park.link = Some(format!("https://pota.app/park/{}", s.reference));
                park.link_tip = Some(if s.location.is_empty() {
                    "Click to open on pota.app".to_string()
                } else {
                    format!("Park Location: {}\nClick to open on pota.app", s.location)
                });
            }
            park.lead = lead;
            cells.push(park);
            debug_assert_eq!(cells.len() - 1, park_col);
            let data = RowData { call: s.activator.clone(), ref_: s.reference.clone(), status: worked.to_string(), park: s.park_name.clone(), skipped };
            let table = if dim { &mut self.done_tree } else { &mut self.spots_tree };
            table.add(&self.f, Row { cells, tags, data: Some(data) });
        }
        self.spots_tree.fit_columns(&self.f);
        self.done_tree.fit_columns(&self.f);
        self.spots_count = (list.len() - n_done).to_string(); // the rest are in the other card
        self.spots_title = format!("{SPOTS_TITLE} ({band} {mode})");
        self.done_title = format!("{DONE_TITLE} ({band} {mode})");
    }

    /// (suffix, lead): "(n)" after a call you've hunted before; before the park an
    /// orange NEW chip if you never have, else the count as "(n)". None until pota.app
    /// has been read.
    fn hunt_badges(&self, s: &pota_spots::Spot, c: Option<&HuntCounts>) -> (Option<(String, Option<String>)>, Option<Lead>) {
        let Some(c) = c else { return (None, None) };
        let p = &self.p;
        let plural = |n: i64| if n != 1 { "s" } else { "" };
        let n = c.activator(&s.activator, &base_call(&s.activator));
        let suffix = (n != 0).then(|| (format!("({n})"), Some(format!("You've hunted {} {n} time{}", s.activator, plural(n)))));
        let lead = if s.reference.is_empty() {
            None
        } else {
            let n = c.park(&s.reference);
            Some(if n != 0 {
                Lead { text: format!("({n})"), chip: None, tip: Some(format!("You've hunted {} {n} time{}", s.reference, plural(n))) }
            } else {
                if s.qualifying == Some(false) {
                    Lead { text: "ATNO".into(), chip: Some((p.atno, p.atno_text)), tip: Some(format!("All-time new one: {} has never had a valid activation, and you've never hunted it", s.reference)) }
                } else {
                    Lead { text: "NEW".into(), chip: Some((p.new, p.new_text)), tip: Some(format!("You've never hunted {}", s.reference)) }
                }
            })
        };
        (suffix, lead)
    }

    pub fn render_worked(&mut self) {
        let mut entries: Vec<WorkedEntry> = self.h.worked.clone();
        entries.sort_by(|a, b| b.time.cmp(&a.time)); // newest first, None last (stable)
        let words: Vec<String> = self.worked_filter.to_lowercase().split_whitespace().map(str::to_string).collect();
        self.worked_tree.clear();
        let mut shown = 0;
        for e in &entries {
            let t = e.time.map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
            let park = self.h.park_text(e);
            let hay = [t.as_str(), &e.call, &e.band, &e.mode, &park, if e.logged { "logged" } else { "not logged" }].join(" ").to_lowercase();
            if !words.iter().all(|w| hay.contains(w.as_str())) {
                continue;
            }
            shown += 1;
            let mut park_cell = Cell::text(park);
            if !e.ref_.is_empty() {
                park_cell.link = Some(format!("https://pota.app/park/{}", e.ref_));
                let location = self.h.spots.as_ref().and_then(|t| t.all().find(|s| s.reference == e.ref_ && !s.location.is_empty()).map(|s| s.location.clone()));
                park_cell.link_tip = Some(match location {
                    Some(l) => format!("Park Location: {l}\nClick to open on pota.app"),
                    None => "Click to open on pota.app".to_string(),
                });
            }
            let cells = vec![
                Cell::text(t),
                Cell { text: e.call.clone(), link: Some(format!("https://pota.app/profile/{}", base_call(&e.call))), link_tip: Some("Click to open on pota.app".into()), ..Default::default() },
                Cell::text(e.band.clone()),
                Cell::text(e.mode.clone()),
                Cell { icon: Some(if e.logged { Icon::Ok } else { Icon::Bad }), ..Default::default() },
                park_cell,
            ];
            self.worked_tree.add(&self.f, Row { cells, tags: RowTags { unlogged: !e.logged, ..Default::default() }, data: None });
        }
        self.worked_tree.fit_columns(&self.f);
        self.worked_count = if shown == entries.len() { entries.len().to_string() } else { format!("{shown}/{}", entries.len()) };
    }

    // --- periodic ----------------------------------------------------------------------

    fn update_status(&mut self, ctx: &egui::Context) {
        self.first_status_done = true;
        self.update_wsjtx_status();
        let now = Utc::now();
        let midnight = (now + chrono::Duration::days(1)).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc();
        let ms = (midnight - now).num_milliseconds();
        let left = (ms + 999) / 1000; // round up: 00:00:00 is the reset
        self.day_left = format!("({} left in UTC day)", duration(left));
        if let Some(spots) = &self.h.spots {
            let here = match (self.h.band(), self.h.mode()) {
                (Some(b), m) if !m.is_empty() => format!(" • {} ({b} {m})", spots.count(b, &m)),
                _ => String::new(),
            };
            let wait = (self.poller.next_fetch() - epoch_now()) as i64;
            let refresh = if wait <= 0 {
                "refreshing...".to_string()
            } else if wait < 120 {
                format!("refresh in {wait}s")
            } else {
                format!("refresh in {} min", wait / 60)
            };
            self.spots_text = format!("Spots (pota.app): {} (total) • {} (dig){here} • {refresh}", spots.len(), spots.digital_count());
        }
        // Follow Windows' light/dark setting while on "Match Windows".
        if self.s.theme == "system" && theme::system_mode() != self.theme_name {
            self.apply_theme(ctx, None);
        }
        self.refresh_spots_list();
        self.refresh_heard_cells();
        self.presence_tick();
        let _ = now.second();
    }

    fn run_timers(&mut self) {
        let now = self.now();
        let due: Vec<Timer> = {
            let (due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.timers).into_iter().partition(|(at, _)| *at <= now);
            self.timers = keep;
            due.into_iter().map(|(_, t)| t).collect()
        };
        for t in due {
            match t {
                Timer::RigClean(quiet) => self.rig_clean(quiet),
                Timer::CheckMode(mode) => {
                    if let Some(s) = &self.h.status {
                        if s.mode != mode {
                            let cur = s.mode.clone();
                            self.log(&format!("WSJT-X is still on {cur}; it did not switch to {mode}"), Tag::Error);
                        }
                    }
                }
            }
        }
    }

    /// Restart, glow and remove the log buttons that belong to a running QSO.
    fn glow_buttons(&mut self, k: f64) {
        // the same pulse as the spot row of the station we're calling
        let fill = theme::mix(self.p.glow.0, self.p.glow.1, k as f32);
        let h = &self.h;
        let newest = {
            let mut found = None;
            for l in self.log.lines.iter().rev() {
                for b in l.buttons.iter().rev() {
                    if b.active.as_ref().is_some_and(|a| h.is_active(a)) && !b.used {
                        found = Some(b.id);
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found
        };
        let mut remove = vec![];
        for b in self.log.buttons_mut() {
            let Some(a) = &b.active else { continue };
            let on = h.is_active(a);
            if b.label == "Halt Tx" && !on {
                remove.push(b.id); // no QSO in progress: no Halt Tx button
                continue;
            }
            if b.label == "Skip" && h.has_answered(a) {
                remove.push(b.id); // they answered us: no point skipping them any more
                continue;
            }
            if b.used {
                continue;
            }
            if on && (b.label == "Halt Tx" || Some(b.id) == newest || newest.is_none()) {
                b.glowing = true;
                b.glow_fill = fill;
            } else {
                b.glowing = false;
            }
        }
        for id in remove {
            self.log.remove_button(id);
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        let mut active = false;
        for e in &events {
            match e {
                egui::Event::PointerButton { pressed: true, .. } | egui::Event::MouseWheel { .. } => active = true,
                egui::Event::Key { pressed: true, .. } => active = true,
                egui::Event::Text(t) => {
                    for c in t.chars() {
                        self.bypass_keys(c);
                    }
                }
                _ => {}
            }
        }
        // moving or resizing the window counts too
        let outer = ctx.input(|i| i.viewport().outer_rect);
        if let Some(o) = outer {
            if self.last_geometry.is_some_and(|g| g != o) {
                active = true;
            }
            self.last_geometry = Some(o);
        }
        if active {
            self.activity();
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(Key::Comma)) && self.dialog.is_none() && self.wsjtx_seen {
            self.panel.toggle(self.now());
        }
    }

    // --- layout persistence --------------------------------------------------------------

    fn geometry_string(&self, ctx: &egui::Context) -> Option<String> {
        let (inner, outer, minimized, maximized) =
            ctx.input(|i| (i.viewport().inner_rect, i.viewport().outer_rect, i.viewport().minimized, i.viewport().maximized));
        if minimized == Some(true) || maximized == Some(true) {
            return None;
        }
        let (inner, outer) = (inner?, outer?);
        let k = self.ppp;
        let (w, h) = ((inner.width() * k).round() as i32, (inner.height() * k).round() as i32);
        let (x, y) = ((outer.left() * k).round() as i32, (outer.top() * k).round() as i32);
        let sx = |v: i32| if v < 0 { format!("+{v}") } else { format!("+{v}") };
        Some(format!("{w}x{h}{}{}", sx(x), sx(y)))
    }

    fn save_layout(&mut self, ctx: &egui::Context) {
        let minimized = ctx.input(|i| i.viewport().minimized) == Some(true);
        let total = self.panes.last;
        let right_h = total.y - self.panes.nh;
        if !self.panes.placed || minimized || right_h < 300.0 || total.x < 600.0 {
            return; // not laid out (or minimised): the numbers would be junk
        }
        let right_w = (total.x - self.panes.left).round();
        let split = [((self.panes.a / right_h) as f64 * 10000.0).round() / 10000.0,
                     (((self.panes.b) / right_h) as f64 * 10000.0).round() / 10000.0];
        if !(200.0 < right_w && right_w < total.x - 200.0 && 0.05 < split[0] && split[0] < split[1] && split[1] < 0.97) {
            return;
        }
        self.s.layout = Some(settings::Layout { right_w: Some(right_w as i64), split: Some(split.to_vec()) });
    }

    fn persist_layout(&mut self, ctx: &egui::Context) {
        self.save_layout(ctx);
        self.save_popouts();
        if let Some(g) = self.geometry_string(ctx) {
            self.s.window = Some(g);
        }
        self.save_settings();
    }

    // --- drawing ---------------------------------------------------------------------------

    fn header_height(&self) -> f32 {
        let f = &self.f;
        let titles = f.title.m.linespace + 4.0 + f.small.m.linespace + 4.0;
        let btn = (f.icon.m.linespace + 18.0).max(f.toggle.m.linespace + 18.0);
        titles.max(36.0).max(btn) + 20.0
    }

    fn draw_header(&mut self, ui: &mut Ui, w: f32) -> f32 {
        let (p, f) = (self.p.clone(), self.f.clone());
        let hh = self.header_height();
        let painter = ui.painter().clone();
        painter.rect_filled(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, hh)), 0.0, p.surface);
        painter.rect_filled(Rect::from_min_size(Pos2::new(0.0, hh), Vec2::new(w, 1.0)), 0.0, p.border);
        let inner_top = 10.0;
        let inner_h = hh - 20.0;
        let center = |hgt: f32| inner_top + ((inner_h - hgt) / 2.0).floor();
        // logo: a white tree on a green rounded square
        let lx = PAD + 4.0;
        let ly = center(36.0);
        painter.rect_filled(Rect::from_min_size(Pos2::new(lx, ly), Vec2::splat(36.0)), ui::radius(9.0), p.accent);
        let pt = |x: f32, y: f32| Pos2::new(lx + x, ly + y);
        let fg = p.accent_text;
        painter.add(egui::Shape::convex_polygon(vec![pt(18.0, 6.0), pt(26.0, 17.0), pt(10.0, 17.0)], fg, Stroke::new(1.0, fg)));
        painter.add(egui::Shape::convex_polygon(vec![pt(18.0, 11.0), pt(28.0, 25.0), pt(8.0, 25.0)], fg, Stroke::new(1.0, fg)));
        painter.rect_filled(Rect::from_min_max(pt(15.5, 24.5), pt(20.5, 31.5)), 0.0, fg);
        let tx = lx + 36.0 + 12.0;
        // right side: gear, theme, Auto-Hunt, Auto CQ/Answer (right to left)
        let icon_font = if f.has_icons { &f.icon } else { &f.ui_bold };
        let gear_txt = if f.has_icons { theme::ICON_SETTINGS } else { "Settings" };
        let gear = Rnd::new(gear_txt, icon_font).pad(11.0, 9.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover).icon();
        let dark = self.theme_name == "dark";
        let theme_txt = if f.has_icons { if dark { theme::ICON_SUN } else { theme::ICON_MOON } } else if dark { "Light" } else { "Dark" };
        let tb = Rnd::new(theme_txt, icon_font).pad(11.0, 9.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover).icon();
        let (hunt_full, hfill, hhover) = self.hunt_button();
        let (cq_full, cfill, chover) = self.cq_button();
        let hunt_size_text = format!("{HUNT_ON_TEXT} (Dry run)");
        let cq_size_text = format!("{CQ_ON_TEXT} (Dry run)");
        let full_w = |text: &str, min: &str| Rnd::new(text, &f.toggle).pill().pad(10.0, 9.0).min_text(min).size(ui).x;
        let right_edge = w - PAD - 4.0;
        let need_full = gear.size(ui).x + 8.0 + tb.size(ui).x + 16.0 + full_w(&hunt_full, &hunt_size_text) + 10.0 + full_w(&cq_full, &cq_size_text);
        // a narrow window drops the "(Dry run)" style suffixes so the buttons still fit
        let compact = right_edge - need_full < tx + 8.0;
        let short = |s: &str| s.split(" (").next().unwrap_or(s).to_string();
        let (hunt_text, hunt_min) = if compact { (short(&hunt_full), short(&hunt_size_text)) } else { (hunt_full.clone(), hunt_size_text.clone()) };
        let (cq_text, cq_min) = if compact { (short(&cq_full), short(&cq_size_text)) } else { (cq_full.clone(), cq_size_text.clone()) };
        let mut right = right_edge;
        let sz = gear.size(ui);
        let r = Rect::from_min_size(Pos2::new(right - sz.x, center(sz.y)), sz);
        self.gear_rect = r;
        let resp = gear.show(ui, Id::unique("gear"), r, true);
        self.tips.tip(&resp, "Settings  (Ctrl+,)", 500, TipAt::Below(r));
        if resp.clicked() {
            self.panel.toggle(self.now());
        }
        right = r.left() - 8.0;
        let sz = tb.size(ui);
        let r = Rect::from_min_size(Pos2::new(right - sz.x, center(sz.y)), sz);
        let resp = tb.show(ui, Id::unique("theme-btn"), r, true);
        self.tips.tip(&resp, if dark { "Switch to light mode" } else { "Switch to dark mode" }, 500, TipAt::Below(r));
        if resp.clicked() {
            let next = if dark { "light" } else { "dark" };
            self.set_theme(ui.ctx(), next);
        }
        right = r.left() - 16.0;
        let hb = Rnd::new(&hunt_text, &f.toggle).pill().pad(10.0, 9.0).colors(hfill, Color32::WHITE).hover(hhover).min_text(&hunt_min);
        let sz = hb.size(ui);
        let r = Rect::from_min_size(Pos2::new(right - sz.x, center(sz.y)), sz);
        let resp = hb.show(ui, Id::unique("hunt-btn"), r, true);
        self.tips.tip(&resp, "Click to turn Auto-Hunt on or off", 500, TipAt::Below(r));
        if resp.clicked() {
            self.toggle_hunt();
        }
        right = r.left() - 10.0;
        let cb = Rnd::new(&cq_text, &f.toggle).pill().pad(10.0, 9.0).colors(cfill, Color32::WHITE).hover(chover).min_text(&cq_min);
        let sz = cb.size(ui);
        let r = Rect::from_min_size(Pos2::new(right - sz.x, center(sz.y)), sz);
        let resp = cb.show(ui, Id::unique("cq-btn"), r, true);
        self.tips.tip(&resp, "Click to turn Auto CQ/Answer on or off.\nCalls CQ when idle and answers stations that call you;\nAuto-Hunt has priority.", 500, TipAt::Below(r));
        if resp.clicked() {
            self.toggle_cq();
        }
        // titles: only what fits left of the buttons
        let th = f.title.m.linespace + 4.0 + f.small.m.linespace + 4.0;
        let ty = center(th);
        let room = r.left() - 12.0 - (tx + 2.0);
        if measure(&f.title, TITLE) <= room {
            ui::text(ui, &painter, Pos2::new(tx + 2.0, ty + 2.0), TITLE, &f.title, p.text);
            let sub = "Calls Parks on the Air activators through WSJT-X";
            if measure(&f.small, sub) <= room {
                ui::text(ui, &painter, Pos2::new(tx + 2.0, ty + f.title.m.linespace + 4.0 + 2.0), sub, &f.small, p.muted);
            }
        }
        hh + 1.0
    }

    fn chip<'a>(&self, text: &'a str, font: &'a theme::Font) -> Rnd<'a> {
        Rnd::new(text, font).pill().pad(12.0, 5.0).colors(self.p.surface, self.p.text)
    }

    /// The status row; returns its bottom.
    fn draw_strip(&mut self, ui: &mut Ui, top: f32, w: f32) -> f32 {
        let (p, f) = (self.p.clone(), self.f.clone());
        let connected = self.connected();
        let st = self.h.status.clone();
        let (conn_text, conn_dot) = if !self.first_status_done {
            ("WSJT-X: waiting for packets...".to_string(), theme::hex("#999999"))
        } else if connected {
            ("WSJT-X connected".to_string(), p.ok)
        } else {
            ("WSJT-X not connected".to_string(), p.bad)
        };
        let conn = self.chip(&conn_text, &f.ui).dot(Some(conn_dot));
        let station_text = st.as_ref().map(|s| format!("{}   {:.3} MHz", s.de_call, s.dial_freq as f64 / 1e6)).unwrap_or_default();
        let band_text = format!("{} ▾", self.h.band().unwrap_or("?"));
        let mode_text = format!("{} ▾", st.as_ref().map(|s| s.mode.clone()).unwrap_or_default());
        let station = self.chip(&station_text, &f.ui);
        let band_dd = self.chip(&band_text, &f.ui).min_text("160m ▾").hover(p.neutral_hover);
        let mode_dd = self.chip(&mode_text, &f.ui).min_text("FT8 ▾").hover(p.neutral_hover);
        let mut parts = vec![];
        if let Some(s) = &st {
            if s.tx_enabled && !s.transmitting {
                parts.push("Tx enabled".to_string());
            }
            if let Some(t) = self.h.target() {
                parts.push(format!("working {}", t.call));
            } else if !s.dx_call.is_empty() && (s.tx_enabled || s.transmitting) {
                // WSJT-X keeps the DX call after a QSO ends, so only show it while Tx is live.
                parts.push(format!("DX {}", s.dx_call));
            }
        }
        let dx_text = parts.join("  ·  ");
        let dx = self.chip(&dx_text, &f.ui);
        let spots_text = self.spots_text.clone();
        let spots = self.chip(&spots_text, &f.ui).dot(Some(p.accent));
        let radio = if connected { self.radio } else { None };
        let radio_label = radio.map(|r| r.spec().0).unwrap_or("");
        let now = self.now();
        let radio_fill = radio.map(|r| {
            let (_, period, lo, hi) = r.spec();
            lerp_rgb(lo, hi, pulse(now, period))
        });
        let tx = Rnd::new(radio_label, &f.small_bold).pill().pad(12.0, 6.0).min_text("TRANSMITTING")
            .colors(radio_fill.unwrap_or(p.bg), Color32::WHITE);

        // row height: the tallest visible item
        let mut row0 = conn.size(ui).y.max(spots.size(ui).y);
        if connected {
            row0 = row0.max(station.size(ui).y);
        }
        if radio.is_some() {
            row0 = row0.max(tx.size(ui).y);
        }
        // Flow the chips left to right, wrapping to a new row when the window is too
        // narrow; the pota.app chip goes at the right of the last row (or on a row of its own).
        // kinds: 0 connection, 1 station, 2 band, 3 mode, 4 radio badge, 5 DX/target, 6 spots
        let mut items: Vec<(u8, Vec2, f32)> = vec![(0, conn.size(ui), 8.0)];
        if connected {
            items.push((1, station.size(ui), 6.0));
            items.push((2, band_dd.size(ui), 6.0));
            items.push((3, mode_dd.size(ui), 8.0));
        }
        if radio.is_some() {
            items.push((4, tx.size(ui), 8.0));
        }
        if connected && !dx_text.is_empty() {
            items.push((5, dx.size(ui), 8.0));
        }
        let limit = w - PAD;
        let mut placed: Vec<(u8, usize, f32)> = vec![]; // (kind, row, x)
        let (mut row, mut x) = (0usize, PAD);
        for &(kind, sz, gap) in &items {
            if x + sz.x > limit && x > PAD {
                row += 1;
                x = PAD;
            }
            placed.push((kind, row, x));
            x += sz.x + gap;
        }
        let spots_sz = spots.size(ui);
        if x + spots_sz.x > limit && x > PAD {
            row += 1;
        }
        placed.push((6, row, (limit - spots_sz.x).max(PAD)));
        let rows = row + 1;
        let row_has_radio = |r: usize| placed.iter().any(|&(k, rr, _)| k == 4 && rr == r);
        let y0 = top + 10.0;
        let mut row_y = vec![y0; rows];
        for r in 1..rows {
            row_y[r] = row_y[r - 1] + row0 + 6.0 + if row_has_radio(r - 1) { 7.0 } else { 0.0 };
        }
        let spot = |kind: u8| placed.iter().find(|&&(k, _, _)| k == kind).map(|&(_, r, x)| (x, row_y[r]));
        let place = |sz: Vec2, at: (f32, f32)| Rect::from_min_size(Pos2::new(at.0, at.1 + ((row0 - sz.y) / 2.0).floor()), sz);
        let r = place(conn.size(ui), spot(0).unwrap());
        conn.show(ui, Id::unique("conn-chip"), r, false);
        if connected {
            let r = place(station.size(ui), spot(1).unwrap());
            station.show(ui, Id::unique("station-chip"), r, false);
            let r = place(band_dd.size(ui), spot(2).unwrap());
            self.band_rect = r;
            let resp = band_dd.show(ui, Id::unique("band-dd"), r, true);
            self.tips.tip(&resp, "WSJT-X's band; click to change it", 500, TipAt::Below(r));
            if resp.clicked() {
                self.retune_menu("band");
            }
            let r = place(mode_dd.size(ui), spot(3).unwrap());
            self.mode_rect = r;
            let resp = mode_dd.show(ui, Id::unique("mode-dd"), r, true);
            self.tips.tip(&resp, "WSJT-X's mode; click to change it", 500, TipAt::Below(r));
            if resp.clicked() {
                self.retune_menu("mode");
            }
        }
        if let Some((bx, by)) = spot(4) {
            let sz = tx.size(ui);
            let r = place(sz, (bx, by));
            tx.show(ui, Id::unique("radio-badge"), r, false);
            // the slim progress bar under it: how far through the current T/R period
            let bar = Rect::from_min_max(Pos2::new(bx + 2.0, by + row0 + 3.0), Pos2::new(bx + sz.x + 8.0 - 10.0, by + row0 + 7.0));
            let period = self.cycle_period();
            let k = cycles::wsjtx_progress(epoch_now(), period) as f32;
            let painter = ui.painter();
            painter.rect_filled(bar, ui::radius(2.0), p.border);
            let filled = (bar.width() * k.clamp(0.0, 1.0)).round().max(4.0);
            let fill = if radio == Some(Radio::Tx) { p.bad } else { p.accent };
            painter.rect_filled(Rect::from_min_size(bar.min, Vec2::new(filled, 4.0)), ui::radius(2.0), fill);
            let resp = ui.interact(bar.expand2(Vec2::new(0.0, 2.0)), Id::unique("cycle-bar"), Sense::hover());
            let mode = st.as_ref().map(|s| s.mode.clone()).unwrap_or_default();
            self.tips.tip(&resp, &format!("{mode} cycle: {} s", period_text(period)), 500, TipAt::Below(bar));
        }
        if let Some(at) = spot(5) {
            let r = place(dx.size(ui), at);
            dx.show(ui, Id::unique("dx-chip"), r, false);
        }
        let r = place(spots_sz, spot(6).unwrap());
        let resp = spots.show(ui, Id::unique("spots-chip"), r, false);
        self.tips.tip(&resp, "pota.app spots, fetched at most once a minute", 500, TipAt::Below(r));
        row_y[rows - 1] + row0 + if row_has_radio(rows - 1) { 7.0 } else { 0.0 } + 6.0
    }

    fn card(&self, ui: &Ui, rect: Rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, self.p.surface);
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, self.p.border), StrokeKind::Inside);
    }

    /// A card with a header band in its own colour behind the title row (height `hc`);
    /// returns the row's top.
    fn card_head(&self, ui: &Ui, rect: Rect, hc: f32) -> f32 {
        self.card(ui, rect);
        let inner = rect.shrink(1.0);
        let hy = inner.top() + 10.0;
        let band = Rect::from_min_max(inner.min, Pos2::new(inner.right(), hy + hc + 4.0));
        let painter = ui.painter();
        painter.rect_filled(band, 0.0, self.p.head);
        painter.rect_filled(Rect::from_min_size(Pos2::new(band.left(), band.bottom()), Vec2::new(band.width(), 1.0)), 0.0, self.p.border);
        hy
    }

    /// A card's title (ttk label padding 2) at the head's content origin; returns its right edge.
    fn card_title(&self, ui: &Ui, x: f32, y: f32, h: f32, title: &str) -> f32 {
        let f = &self.f;
        let lh = f.heading.m.linespace + 4.0;
        let top = y + ((h - lh) / 2.0).floor();
        let w = ui::text(ui, ui.painter(), Pos2::new(x + 2.0, top + 2.0), title, &f.heading, self.p.text);
        x + w + 4.0
    }

    fn pill(&self, ui: &mut Ui, x: f32, y: f32, h: f32, text: &str, id: &str) -> f32 {
        let r = Rnd::new(text, &self.f.small_bold).pill().pad(8.0, 1.0).min_text("00").colors(self.p.neutral, self.p.neutral_text);
        let sz = r.size(ui);
        let rect = Rect::from_min_size(Pos2::new(x, y + ((h - sz.y) / 2.0).floor()), sz);
        if rect.right() <= ui.clip_rect().right() - 6.0 {
            r.show(ui, Id::unique(id), rect, false); // (left out when the card is too narrow for it)
        }
        rect.right()
    }

    /// The search box at the right of a card header; returns (left edge, changed).
    fn search_box(&mut self, ui: &mut Ui, right: f32, min_left: f32, y: f32, h: f32, hint: &str, which: u8) -> (f32, bool) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let text = if which == 0 { self.log.filter.clone() } else { self.worked_filter.clone() };
        let mut entry_w = 22.0 * measure(&f.small, "0") + 2.0;
        let clear = Rnd::new("×", &f.small_bold).pill().pad(6.0, 0.0).min_text("×").colors(p.neutral, p.neutral_text).hover(p.neutral_hover);
        let clear_w = if text.is_empty() { 0.0 } else { clear.size(ui).x + 4.0 };
        // in a narrow card the box shrinks to the room left of the title, or goes
        let room = right - min_left - (2.0 + 6.0 + 2.0 + clear_w);
        if room < 50.0 {
            return (right, false);
        }
        entry_w = entry_w.min(room);
        let bw = 2.0 + 6.0 + entry_w + 2.0 + clear_w;
        let bh = 2.0 + 6.0 + f.small.m.linespace + 2.0;
        let rect = Rect::from_min_size(Pos2::new(right - bw, y + ((h - bh) / 2.0).floor()), Vec2::new(bw, bh));
        let id = Id::unique(("search", which));
        let focused = ui.ctx().memory(|m| m.has_focus(id));
        let painter = ui.painter().clone();
        painter.rect_filled(rect, 0.0, p.surface2);
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, if focused { p.accent } else { p.border }), StrokeKind::Inside);
        let text_top = rect.top() + 1.0 + 3.0 + 1.0;
        let erect = Rect::from_min_size(Pos2::new(rect.left() + 1.0 + 6.0 + 1.0, text_top), Vec2::new(entry_w - 2.0, f.small.m.linespace));
        if text.is_empty() {
            ui::text(ui, &painter.with_clip_rect(erect.expand2(Vec2::new(0.0, 4.0))), Pos2::new(rect.left() + 8.0, text_top), hint, &f.small, p.faint);
        }
        let mut buf = text.clone();
        let shift = f.small.m.ascent - ui::egui_baseline(ui, &f.small);
        let te = egui::TextEdit::singleline(&mut buf)
            .id(id)
            .frame(egui::Frame::NONE)
            .margin(egui::Margin::ZERO)
            .font(f.small.id.clone())
            .text_color(p.text)
            .desired_width(erect.width());
        let resp = ui.put(erect.translate(Vec2::new(0.0, shift)), te);
        if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            buf.clear();
        }
        if !text.is_empty() {
            let sz = clear.size(ui);
            let r = Rect::from_min_size(Pos2::new(rect.right() - 1.0 - 4.0 - sz.x, rect.top() + ((bh - sz.y) / 2.0).floor()), sz);
            let resp = clear.show(ui, Id::unique(("search-clear", which)), r, true);
            self.tips.tip(&resp, "Clear the search", 300, TipAt::Below(r));
            if resp.clicked() {
                buf.clear();
            }
        }
        let changed = buf != text;
        if changed {
            if which == 0 {
                self.log.filter = buf;
                self.log.jump_end = true;
            } else {
                self.worked_filter = buf;
            }
        }
        (rect.left(), changed)
    }

    fn row_colors(&self) -> RowColors {
        let now = self.now();
        let k = pulse(now, GLOW_PERIOD) as f32;
        let kh = pulse(now, HEARD_PERIOD) as f32;
        RowColors { target: theme::mix(self.p.glow.0, self.p.glow.1, k), heard: theme::mix(self.p.heard.0, self.p.heard.1, kh) }
    }

    fn draw_log_card(&mut self, ui: &mut Ui, rect: Rect) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let inner = rect.shrink(1.0);
        let head_content = (f.heading.m.linespace + 4.0).max(2.0 + 6.0 + f.small.m.linespace + 2.0);
        let hy0 = self.card_head(ui, rect, head_content);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = match self.h.band() {
            Some(b) if !self.h.mode().is_empty() => format!("Activity ({b} {})", self.h.mode()),
            _ => "Activity".to_string(),
        };
        let title_right = self.card_title(ui, hx, hy, head_content, &title);
        let pop_left = self.pop_button(ui, Card::Log, inner, hy, head_content);
        let clear = Rnd::new("Clear", &f.small_bold).pill().pad(10.0, 2.0).colors(p.blue, p.blue_text).hover(p.blue_hover);
        let sz = clear.size(ui);
        let r = Rect::from_min_size(Pos2::new(pop_left - sz.x, hy + ((head_content - sz.y) / 2.0).floor()), sz);
        let resp = clear.show(ui, Id::unique("log-clear"), r, true);
        self.tips.tip(&resp, "Clear the activity log", 500, TipAt::Below(r));
        if resp.clicked() {
            self.log.clear();
        }
        self.search_box(ui, r.left() - 10.0, title_right + 10.0, hy, head_content, "Search activity", 0);
        let y = hy + head_content + 6.0;
        let body = Rect::from_min_max(Pos2::new(inner.left(), y), inner.max);
        let mut tips = std::mem::take(&mut self.tips);
        let click = self.log.show(ui, body, &p, &f, &mut tips);
        self.tips = tips;
        if self.log.has_selection() {
            self.static_sel = None; // one selection at a time
        }
        if let Some(LogClick::Button(id)) = click {
            let kind = self.log.button_mut(id).map(|b| {
                b.used = true;
                b.glowing = false;
                b.kind.clone()
            });
            if let Some(kind) = kind {
                self.h.perform(&kind);
                self.drain_hunter();
            }
        }
    }

    /// The lines of the Current Info card, oldest first, wrapped for a card `card_w` wide.
    fn build_info(&mut self, ui: &Ui, card_w: f32) -> Vec<StaticLine> {
        let f = self.f.clone();
        let mut info: Vec<StaticLine> = vec![];
        if let Some(st) = &self.h.status {
            let band = self.h.band().unwrap_or("?");
            let cat = if self.static_rig.is_empty() { String::new() } else { format!(" [CAT: {}]", self.static_rig.trim_end_matches(" off")) };
            let text = format!("WSJT-X: {} on {} {} ({:.3} MHz){cat}", st.de_call, band, st.mode, st.dial_freq as f64 / 1e6);
            info.push(StaticLine::new(self.stamp("wsjtx", &text), text));
        }
        let logged_in = !self.s.pota_user.is_empty() && !self.s.pota_password.is_empty();
        let counts = self.counts.borrow().clone();
        if !logged_in {
            let text = "pota.app: not signed in. Your login is needed to show which parks are new to you, how many times you've hunted each park, and how many times you've worked each activator.";
            info.push(StaticLine::new(self.stamp("pota-notice", text), text.to_string()));
            info.push(StaticLine { link: true, ..StaticLine::new(String::new(), "Add your pota.app login in Settings".into()) });
        } else if let Some(c) = counts.as_ref() {
            let now = epoch_now();
            let next = match self.stats.next_fetch() {
                Some(n) if n > now => {
                    let s = (n - now) as i64;
                    format!("refresh in {}m {:02}s", s / 60, s % 60)
                }
                Some(_) => "refreshing...".to_string(),
                None => "no refresh scheduled".to_string(),
            };
            info.push(StaticLine::new(
                hhmmss(DateTime::from_timestamp(c.fetched_at as i64, 0).unwrap_or_default()),
                format!("POTA: {} parks hunted, {} activators worked ({next})", c.parks.len(), c.activators.len()),
            ));
        } else {
            let text = self.pota_summary();
            let ts = self.stamp("pota-status", &text);
            info.push(StaticLine::new(ts, format!("pota.app: {text}")));
            if self.pota_status.starts_with("Sign-in failed") {
                info.push(StaticLine { link: true, ..StaticLine::new(String::new(), "Check your pota.app login in Settings".into()) });
            }
        }
        // oldest first, like the log; a line with no stamp belongs to the stamped one above it
        let mut groups: Vec<Vec<StaticLine>> = vec![];
        for l in info {
            match groups.last_mut() {
                Some(g) if l.ts.is_empty() => g.push(l),
                _ => groups.push(vec![l]),
            }
        }
        groups.sort_by(|a, b| a[0].ts.cmp(&b[0].ts));
        let info: Vec<StaticLine> = groups.into_iter().flatten().collect();
        // wrap to the card's width (a narrow window): the text starts after the 10-column stamp
        let avail = (card_w - 2.0 - PAD - (PAD + 2.0 + 10.0 * measure(&f.mono, "0"))).max(60.0);
        let btn_w = Rnd::new("Refresh now", &f.small_bold).pill().pad(9.0, 1.0).size(ui).x + 10.0;
                wrap_static(info, &f.ui, avail, btn_w)
    }

    fn info_row_h(&self, ui: &Ui, info: &[StaticLine]) -> f32 {
        let f = &self.f;
        let mut lh = f.ui.m.linespace + 2.0;
        if info.iter().any(|l| l.button.is_some()) {
            lh = lh.max(Rnd::new("Refresh now", &f.small_bold).pill().pad(9.0, 1.0).size(ui).y + 2.0);
        }
        lh
    }

    /// Height of the Current Info card: title row, the lines, a little room below.
    fn info_height(&self, ui: &Ui, info: &[StaticLine]) -> f32 {
        let hc = self.f.heading.m.linespace + 4.0;
        1.0 + 10.0 + hc + 6.0 + self.info_row_h(ui, info) * info.len() as f32 + 8.0
    }

    /// Its own card above the Activity log: current facts, timestamped like log lines,
    /// updated in place, as tall as its lines need.
    fn draw_info_card(&mut self, ui: &mut Ui, rect: Rect) {
        let f = self.f.clone();
        let inner = rect.shrink(1.0);
        let hc = f.heading.m.linespace + 4.0;
        let hy0 = self.card_head(ui, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        self.card_title(ui, hx, hy, hc, "Current Info");
        self.pop_button(ui, Card::Info, inner, hy, hc);
        let info = self.build_info(ui, rect.width());
        let (click, _) = self.static_selection(ui, &info, inner, hy + hc + 6.0);
        match click {
            Some(StaticClick::Link) => {
                use crate::panel::{Page, Tab};
                let now = self.now();
                let mut panel = std::mem::take(&mut self.panel);
                panel.open_at(self, now, Tab::Connect, Page::Pota);
                self.panel = panel;
            }
            Some(StaticClick::Refresh) => self.stats.refresh(),
            None => {}
        }
    }

    /// When `text` last changed in static-line `slot`, as HH:MM:SS (UTC).
    fn stamp(&mut self, slot: &'static str, text: &str) -> String {
        match self.static_stamps.get(slot) {
            Some((t, ts)) if t == text => ts.clone(),
            _ => {
                let ts = hhmmss(Utc::now());
                self.static_stamps.insert(slot, (text.to_string(), ts.clone()));
                ts
            }
        }
    }

    /// Drop the log's and Current Info's text selections (another text is being selected,
    /// so Ctrl+C must copy that one).
    pub fn clear_text_selections(&mut self) {
        self.log.clear_selection();
        self.static_sel = None;
    }

    /// Draw the static lines of the Activity card, selectable with the mouse like the
    /// log below (drag, double-click a word, Ctrl+C). `y0` is the top of the first
    /// line. Returns what was clicked, if anything, and the height used.
    fn static_selection(&mut self, ui: &mut Ui, info: &[StaticLine], inner: Rect, y0: f32) -> (Option<StaticClick>, f32) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let x0 = inner.left() + PAD + 2.0;
        if info.is_empty() {
            self.static_sel = None;
            return (None, 0.0);
        }
        let cw = measure(&f.mono, "0");
        let xt = x0 + 10.0 * cw; // text starts after "HH:MM:SS  "
        let btn_of = |label: &'static str| Rnd::new(label, &f.small_bold).pill().pad(9.0, 1.0).colors(p.blue, p.blue_text).hover(p.blue_hover);
        let lh = self.info_row_h(ui, info);
        let area = Rect::from_min_max(Pos2::new(inner.left(), y0), Pos2::new(inner.right(), y0 + lh * info.len() as f32));
        let hit = |pos: Pos2| -> (usize, usize) {
            let i = (((pos.y - y0) / lh).floor().max(0.0) as usize).min(info.len() - 1);
            let pp = ui::positions(&f.ui, &info[i].text);
            let n = info[i].text.chars().count().min(pp.len().saturating_sub(1));
            let rel = pos.x - xt;
            let k = (0..=n).min_by(|&a, &b| (pp[a] - rel).abs().total_cmp(&(pp[b] - rel).abs())).unwrap_or(0);
            (i, k)
        };
        let hover_ok = ui::hover_in(ui, area);
        let resp = ui.interact(area, Id::new("static-select"), egui::Sense::click_and_drag());
        let mut click = None;
        if resp.hovered() && hover_ok {
            if let Some(pos) = ui.ctx().pointer_hover_pos() {
                let i = hit(pos).0;
                if info[i].link && pos.x < xt + measure(&f.ui, &info[i].text) {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
            }
        }
        if resp.drag_started() && hover_ok {
            if let Some(pos) = resp.interact_pointer_pos() {
                let a = hit(pos);
                self.static_sel = Some((a, a));
                self.log.clear_selection();
            }
        } else if resp.dragged() {
            if let (Some(pos), Some((a, _))) = (resp.interact_pointer_pos(), self.static_sel) {
                self.static_sel = Some((a, hit(pos)));
            }
        } else if resp.double_clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let (i, k) = hit(pos);
                let chars: Vec<char> = info[i].text.chars().collect();
                let k = k.min(chars.len());
                let (mut a, mut b) = (k, k);
                while a > 0 && chars[a - 1] != ' ' {
                    a -= 1;
                }
                while b < chars.len() && chars[b] != ' ' {
                    b += 1;
                }
                self.static_sel = if a < b { Some(((i, a), (i, b))) } else { None };
                self.log.clear_selection();
            }
        } else if resp.clicked() {
            self.static_sel = None;
            if let Some(pos) = resp.interact_pointer_pos() {
                let i = hit(pos).0;
                if info[i].link && pos.x < xt + measure(&f.ui, &info[i].text) {
                    click = Some(StaticClick::Link);
                }
            }
        } else if ui.input(|i| i.pointer.any_pressed()) && !hover_ok {
            self.static_sel = None;
        }
        // normalised (start, end); dropped if the lines changed under it
        let range = self.static_sel.and_then(|(a, b)| {
            let (a, b) = if a <= b { (a, b) } else { (b, a) };
            (b.0 < info.len()).then_some((a, b))
        });
        if range.is_none() {
            self.static_sel = None;
        }
        let len = |i: usize| info[i].text.chars().count();
        if let Some(((si, sk), (ei, ek))) = range {
            if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                let mut out = Vec::new();
                for i in si..=ei {
                    let chars: Vec<char> = info[i].text.chars().collect();
                    let s = if i == si { sk.min(chars.len()) } else { 0 };
                    let e = if i == ei { ek.min(chars.len()) } else { chars.len() };
                    let body: String = chars[s.min(e)..e].iter().collect();
                    out.push(if s == 0 && !info[i].ts.is_empty() { format!("{}  {body}", info[i].ts) } else { body });
                }
                ui.ctx().copy_text(out.join("\n"));
            }
        }
        for (i, line) in info.iter().enumerate() {
            let y = y0 + lh * i as f32;
            let ty = y + ((lh - f.ui.m.linespace) / 2.0).floor();
            let (s, e) = match range {
                Some(((si, sk), (ei, ek))) if i >= si && i <= ei => (if i == si { sk } else { 0 }, if i == ei { ek } else { len(i) }),
                _ => (0, 0),
            };
            let n = len(i);
            let (s, e) = (s.min(n), e.min(n).max(s.min(n)));
            // timestamp, in the log's colour; highlighted when the selection takes in the line start
            if !line.ts.is_empty() {
                let in_range = matches!(range, Some(((si, _), (ei, _))) if i >= si && i <= ei);
                let ts_sel = in_range && s == 0;
                let bg = if ts_sel { p.select } else { p.surface };
                let my = y + ((lh - f.mono.m.linespace) / 2.0).floor();
                if ts_sel {
                    ui.painter().rect_filled(Rect::from_min_size(Pos2::new(x0, my), Vec2::new(line.ts.chars().count() as f32 * cw, f.mono.m.linespace)), 0.0, bg);
                }
                ui::text_on(ui, ui.painter(), Pos2::new(x0, my), &line.ts, &f.mono, p.log(theme::LogTag::Ts), bg);
            }
            let color = if line.link { p.link } else { p.text };
            let pp = ui::positions(&f.ui, &line.text);
            let chars: Vec<char> = line.text.chars().collect();
            for (from, to, selected) in [(0, s, false), (s, e, true), (e, n, false)] {
                if from >= to {
                    continue;
                }
                let px = xt + pp[from];
                let bg = if selected { p.select } else { p.surface };
                if selected {
                    let r = Rect::from_min_max(Pos2::new(px, ty), Pos2::new(xt + pp[to], ty + f.ui.m.linespace));
                    ui.painter().rect_filled(r, 0.0, bg);
                }
                let seg: String = chars[from..to].iter().collect();
                ui::text_on(ui, ui.painter(), Pos2::new(px, ty), &seg, &f.ui, color, bg);
            }
            if let Some((label, enabled)) = line.button {
                let mut b = btn_of(label);
                if !enabled {
                    b = Rnd::new(label, &f.small_bold).pill().pad(9.0, 1.0).colors(p.disabled_btn.0, p.disabled_btn.1);
                }
                let sz = b.size(ui);
                let after = if line.text.is_empty() { 0.0 } else { measure(&f.ui, &line.text) + 10.0 };
                let r = Rect::from_min_size(Pos2::new(xt + after, y + ((lh - sz.y) / 2.0).floor()), sz);
                let resp = b.show(ui, Id::new("static-refresh"), r, enabled);
                let tip = if enabled { "Fetch your hunt counts from pota.app again now" } else { "Counts were fetched less than a minute ago" };
                self.tips.tip(&resp, tip, 300, TipAt::Below(r));
                if enabled && resp.clicked() {
                    click = Some(StaticClick::Refresh);
                }
            }
        }
        (click, lh * info.len() as f32)
    }

    fn notice_label(&self, n: usize, band: &str, mode: &str, newest: &str) -> String {
        let newest_text = age_minutes(newest, Utc::now()).map(|a| format!(" (newest {a}m ago)")).unwrap_or_default();
        format!("{n} new park{} on {band} {mode} right now{newest_text}", if n != 1 { "s" } else { "" })
    }

    fn qsy_button<'a>(f: &'a Fonts, p: &Palette, text: &'a str) -> Rnd<'a> {
        Rnd::new(text, &f.ui_bold).pill().pad(7.0, 0.0).colors(p.blue, p.blue_text).hover(p.blue_hover)
    }

    /// For each new-parks line whether its QSY button drops under the label (a narrow
    /// card), and the card's height; (empty, 0) when there are none.
    fn notices_layout(&self, ui: &Ui, width: f32) -> (Vec<bool>, f32) {
        if self.notices.is_empty() {
            return (vec![], 0.0);
        }
        let f = &self.f;
        let rowh = f.ui_bold.m.linespace + 2.0;
        let lx = 1.0 + PAD + 2.0;
        let mut h = 10.0 + f.heading.m.linespace + 4.0 + 6.0 + 4.0; // header, rows, bottom
        let mut wraps = vec![];
        for (band, mode, n, newest) in &self.notices {
            let w = measure(&f.ui_bold, &self.notice_label(*n, band, mode, newest));
            let bw = Self::qsy_button(f, &self.p, &format!("QSY to {band} {mode}")).size(ui).x;
            let wrap = lx + w + 12.0 + bw > width - 1.0 - PAD;
            wraps.push(wrap);
            h += rowh + 2.0 + if wrap { rowh + 2.0 } else { 0.0 };
        }
        (wraps, h)
    }

    /// Its own card above the Spots card: one line per other band/mode with parks we've
    /// never hunted, as tall as its lines need.
    fn draw_notices_card(&mut self, ui: &mut Ui, rect: Rect) {
        let f = self.f.clone();
        let p = self.p.clone();
        let inner = rect.shrink(1.0);
        let hc = f.heading.m.linespace + 4.0;
        let hy = self.card_head(ui, rect, hc);
        let hx = inner.left() + PAD + 2.0;
        let x = self.card_title(ui, hx, hy, hc, "POTA Spots (New to me) [on other band/mode]");
        let total = self.notices.iter().map(|n| n.2).sum::<usize>().to_string();
        self.pill(ui, x + 8.0, hy, hc, &total, "new-count");
        self.pop_button(ui, Card::Notices, inner, hy, hc);
        let (wraps, _) = self.notices_layout(ui, rect.width());
        let rowh = f.ui_bold.m.linespace + 2.0;
        let lx = inner.left() + PAD + 2.0;
        let mut top = hy + hc + 6.0;
        let mut retune = None;
        for (i, (band, mode, n, newest)) in self.notices.clone().into_iter().enumerate() {
            let ry = top + 1.0;
            let label = self.notice_label(n, &band, &mode, &newest);
            let w = ui::text(ui, ui.painter(), Pos2::new(lx + 1.0, ry + 1.0), &label, &f.ui_bold, p.new);
            let label_r = Rect::from_min_size(Pos2::new(lx, ry), Vec2::new(w + 2.0, rowh));
            let label_resp = ui.interact(label_r, Id::unique(("notice", i)), Sense::hover());
            if label_resp.hovered() {
                let now = Utc::now();
                let lines: Vec<String> = self
                    .h
                    .new_park_spots(&band, &mode)
                    .iter()
                    .map(|(s, atno)| {
                        format!("{} \u{2022} {} ago \u{2022} {} \u{2022} {} ({})", if *atno { "ATNO" } else { "NEW" }, ago(&s.spot_time, now), s.activator, s.reference, s.park_name)
                    })
                    .collect();
                if !lines.is_empty() {
                    self.tips.tip(&label_resp, &lines.join("\n"), 300, TipAt::Below(label_r));
                }
            }
            let qsy_text = format!("QSY to {band} {mode}");
            let b = Self::qsy_button(&f, &p, &qsy_text);
            let sz = b.size(ui);
            let wrap = wraps.get(i).copied().unwrap_or(false);
            let r = if wrap {
                top += rowh + 2.0; // the button goes on a row of its own under the label
                Rect::from_min_size(Pos2::new(lx, ry + rowh + 2.0 + ((rowh - sz.y) / 2.0).floor()), sz)
            } else {
                Rect::from_min_size(Pos2::new(lx + w + 2.0 + 10.0, ry + ((rowh - sz.y) / 2.0).floor()), sz)
            };
            let resp = b.show(ui, Id::unique(("qsy", i)), r, true);
            self.tips.tip(&resp, &format!("Switch WSJT-X to {band} {mode}\n(not while a QSO or Tx is active)"), 300, TipAt::Below(r));
            if resp.clicked() {
                retune = Some((band.clone(), mode.clone()));
            }
            top += rowh + 2.0;
        }
        if let Some((b, m)) = retune {
            self.retune(Some(b), Some(m));
        }
    }

    fn draw_spots_card(&mut self, ui: &mut Ui, rect: Rect) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let inner = rect.shrink(1.0);
        let hc = f.heading.m.linespace + 4.0;
        let hy0 = self.card_head(ui, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = self.spots_title.clone();
        let x = self.card_title(ui, hx, hy, hc, &title);
        let count = self.spots_count.clone();
        self.pill(ui, x + 8.0, hy, hc, &count, "spots-count");
        self.pop_button(ui, Card::Spots, inner, hy, hc);
        let top = hy + hc + 6.0;
        let table_rect = Rect::from_min_max(Pos2::new(inner.left(), top), inner.max);
        let colors = self.row_colors();
        let mut events = vec![];
        let mut tips = std::mem::take(&mut self.tips);
        self.spots_tree.show(ui, table_rect, &p, &f, &colors, &mut tips, &mut events);
        self.tips = tips;
        self.table_events(events);
        let (text, spin) = self.waiting.clone();
        if !text.is_empty() {
            self.list_overlay(ui, rect, &text, spin);
        }
    }

    /// The centred "waiting" / "No spots found" overlay over a spot list.
    fn list_overlay(&mut self, ui: &mut Ui, rect: Rect, text: &str, spin: bool) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let lw = measure(&f.heading, text) + 2.0;
        let lh = f.heading.m.linespace + 2.0;
        let bw = lw.max(if spin { 72.0 } else { 0.0 });
        let bh = if spin { 72.0 + 14.0 + lh } else { lh };
        let c = Pos2::new(rect.left() + rect.width() * 0.5, rect.top() + rect.height() * 0.55);
        let b = Rect::from_center_size(c, Vec2::new(bw, bh));
        let b = Rect::from_min_size(b.min.round(), b.size());
        let painter = ui.painter();
        painter.rect_filled(b, 0.0, p.surface);
        if spin {
            self.spinner(painter, Pos2::new(b.center().x, b.top() + 36.0));
        }
        ui::text(ui, painter, Pos2::new(b.center().x - lw / 2.0 + 1.0, b.bottom() - lh + 1.0), text, &f.heading, p.muted);
    }

    /// The big "waiting" spinner (72 px, 7 px line).
    fn spinner(&mut self, painter: &egui::Painter, center: Pos2) {
        let p = &self.p;
        let r = 36.0 - 7.0 / 2.0 - 1.0;
        painter.circle_stroke(center, r, Stroke::new(7.0, p.border));
        self.spinner_angle = ((self.now() / 0.03) as f32 * -12.0).rem_euclid(360.0);
        let start = self.spinner_angle.to_radians();
        let pts: Vec<Pos2> = (0..=40)
            .map(|i| {
                let a = start + (100f32.to_radians()) * i as f32 / 40.0;
                Pos2::new(center.x + r * a.cos(), center.y - r * a.sin())
            })
            .collect();
        painter.add(egui::Shape::line(pts, Stroke::new(7.0, self.p.accent)));
    }

    /// The "Waiting for WSJT-X" popup: a card centred in the window, shown from start-up
    /// until WSJT-X's first Status, then never again. It can't be dismissed and blocks the
    /// rest of the app (user request). Returns true while it is showing.
    fn draw_wsjtx_wait(&mut self, ctx: &egui::Context, screen: Rect) -> bool {
        if self.wsjtx_seen {
            return false;
        }
        if self.h.band().is_some() {
            self.wsjtx_seen = true;
            return false;
        }
        let (p, f) = (self.p.clone(), self.f.clone());
        egui::Area::new(Id::new("wsjtx-wait-blocker"))
            .order(Order::Middle)
            .fixed_pos(Pos2::ZERO)
            .show(ctx, |ui| {
                ui.allocate_response(screen.size(), Sense::click_and_drag());
            });
        let heading = "Waiting for WSJT-X...";
        let hint = format!("Start WSJT-X, or check that its UDP Server is {} port {} with Accept UDP requests ticked. \
                            This closes by itself once WSJT-X is heard.", self.s.udp_group, self.s.udp_port);
        let lines = ui::wrap_lines_measure(&f.ui, &hint, 380.0);
        // sized like the other cards: padding 32 x 24, spinner, 14, heading, 8, hint
        let hw = measure(&f.heading, heading) + 2.0;
        let lw = lines.iter().map(|l| measure(&f.ui, l)).fold(0.0, f32::max) + 2.0;
        let inner = Vec2::new(hw.max(lw).max(72.0), 72.0 + 14.0 + f.heading.m.linespace + 2.0 + 8.0 + lines.len() as f32 * f.ui.m.linespace + 2.0);
        let size = inner + Vec2::new(64.0 + 2.0, 48.0 + 2.0);
        let rect = Rect::from_center_size(screen.center(), size);
        let rect = Rect::from_min_size(rect.min.round(), size);
        egui::Area::new(Id::new("wsjtx-wait"))
            .order(Order::Foreground)
            .fixed_pos(rect.min)
            .show(ctx, |ui| {
                let painter = ui.painter().clone();
                painter.rect_filled(rect, 0.0, p.border);
                painter.rect_filled(rect.shrink(1.0), 0.0, p.surface);
                let cx = rect.center().x;
                let mut y = rect.top() + 1.0 + 24.0;
                self.spinner(&painter, Pos2::new(cx, y + 36.0));
                y += 72.0 + 14.0;
                ui::text(ui, &painter, Pos2::new((cx - hw / 2.0).round() + 1.0, y + 1.0), heading, &f.heading, p.text);
                y += f.heading.m.linespace + 2.0 + 8.0;
                for (i, l) in lines.iter().enumerate() {
                    let w = measure(&f.ui, l);
                    ui::text(ui, &painter, Pos2::new((cx - w / 2.0).round(), y + 1.0 + i as f32 * f.ui.m.linespace), l, &f.ui, p.muted);
                }
                ui.allocate_rect(rect, Sense::click_and_drag());
            });
        ctx.request_repaint_after(Duration::from_millis(40));
        true
    }

    fn draw_done_card(&mut self, ui: &mut Ui, rect: Rect) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let inner = rect.shrink(1.0);
        let hc = f.heading.m.linespace + 4.0;
        let hy0 = self.card_head(ui, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = self.done_title.clone();
        let x = self.card_title(ui, hx, hy, hc, &title);
        let count = self.done_count.clone();
        self.pill(ui, x + 8.0, hy, hc, &count, "done-count");
        self.pop_button(ui, Card::Done, inner, hy, hc);
        let table_rect = Rect::from_min_max(Pos2::new(inner.left(), hy + hc + 6.0), inner.max);
        let colors = self.row_colors();
        let mut events = vec![];
        let mut tips = std::mem::take(&mut self.tips);
        self.done_tree.show(ui, table_rect, &p, &f, &colors, &mut tips, &mut events);
        self.tips = tips;
        self.table_events(events);
        let (text, spin) = self.waiting.clone();
        if !spin && !text.is_empty() {
            self.list_overlay(ui, rect, &text, false); // "No spots found for <band> <mode>"
        }
    }

    fn draw_worked_card(&mut self, ui: &mut Ui, rect: Rect) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let inner = rect.shrink(1.0);
        let hc = (f.heading.m.linespace + 4.0).max(2.0 + 6.0 + f.small.m.linespace + 2.0);
        let hy0 = self.card_head(ui, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = format!("Worked today ({})", Utc::now().format("%Y-%m-%d"));
        let x = self.card_title(ui, hx, hy, hc, &title);
        let count = self.worked_count.clone();
        let x = self.pill(ui, x + 8.0, hy, hc, &count, "worked-count");
        let lh = f.small.m.linespace + 4.0;
        let day_left = self.day_left.clone();
        let day_w = measure(&f.small, &day_left);
        let mut left_end = x;
        if x + 8.0 + day_w <= inner.right() - PAD {
            ui::text(ui, ui.painter(), Pos2::new(x + 6.0 + 2.0, hy + ((hc - lh) / 2.0).floor() + 2.0), &day_left, &f.small, p.muted);
            left_end = x + 8.0 + day_w;
        }
        let pop_left = self.pop_button(ui, Card::Worked, inner, hy, hc);
        let (_, changed) = self.search_box(ui, pop_left, left_end + 10.0, hy, hc, "Search worked today", 1);
        if changed {
            self.render_worked();
        }
        let table_rect = Rect::from_min_max(Pos2::new(inner.left(), hy + hc + 6.0), inner.max);
        let colors = self.row_colors();
        let mut events = vec![];
        let mut tips = std::mem::take(&mut self.tips);
        self.worked_tree.show(ui, table_rect, &p, &f, &colors, &mut tips, &mut events);
        self.tips = tips;
    }

    fn table_events(&mut self, events: Vec<TableEvent>) {
        for e in events {
            let TableEvent::Context(d, pos) = e;
            let where_ = format!("{} {}", d.ref_, d.park).trim().to_string();
            let where_ = if where_.is_empty() { "park unknown".to_string() } else { where_ };
            let name = base_call(&d.call);
            let status = match d.status.as_str() {
                "Worked" => "Worked today",
                "Skipped" => "Skipped today",
                _ => "",
            };
            let head = format!("{}  \u{b7}  {where_}{}", d.call, if status.is_empty() { String::new() } else { format!("  \u{b7}  {status}") });
            let items = vec![
                Item::Heading(head),
                Item::Row {
                    label: format!("Skip {name}"),
                    cmd: (!d.skipped).then(|| MenuCmd::Skip { call: d.call.clone(), ref_: d.ref_.clone() }),
                    subtitle: if d.skipped { "Already skipped".into() } else { "Not today on this band, mode and park; stops Tx if calling them".into() },
                },
                Item::Row {
                    label: format!("Unskip {name}"),
                    cmd: d.skipped.then(|| MenuCmd::Unskip { call: d.call.clone(), ref_: d.ref_.clone() }),
                    subtitle: if d.skipped { "Allow calls to them again today".into() } else { "Not skipped".into() },
                },
            ];
            self.popup = Some(PopupMenu::new(items, pos, self.now()));
            self.popup_in = self.in_pop;
        }
    }

    fn menu_cmd(&mut self, cmd: MenuCmd) {
        match cmd {
            MenuCmd::Skip { call, ref_ } => self.h.skip(&call, Some(&ref_)),
            MenuCmd::Unskip { call, ref_ } => self.h.unskip(&call, &ref_),
            MenuCmd::Band(b) => self.retune(Some(b), None),
            MenuCmd::Mode(m) => self.retune(None, Some(m)),
        }
        self.drain_hunter();
    }

    /// The panes (log | spots / done / worked) with their draggable dividers.
    fn draw_panes(&mut self, ui: &mut Ui, area: Rect) {
        let total = area.size();
        if !self.panes.placed && total.x >= 300.0 && total.y >= 300.0 {
            let layout = self.s.layout.clone();
            let saved_w = layout.as_ref().and_then(|l| l.right_w).map(|v| v as f32);
            let right_w = match saved_w {
                Some(v) if 200.0 < v && v < total.x - 200.0 => v,
                _ => self.fit_right_w + 5.0,
            };
            // neither side may start out collapsed, however narrow the window is
            let min_pane = (total.x * 0.3).min(200.0);
            self.panes.left = (total.x - right_w).clamp(min_pane, total.x - min_pane);
            let split = layout.and_then(|l| l.split).filter(|s| s.len() == 2 && 0.1 < s[0] && s[0] < s[1] && s[1] < 0.95);
            let (a, b) = match split {
                Some(s) => (s[0] as f32, s[1] as f32),
                None => (0.5, 0.75), // spots get half, the other two a quarter each
            };
            self.panes.a = (total.y * a).floor();
            self.panes.b = (total.y * b).floor();
            self.panes.placed = true;
            self.panes.last = total;
        } else if self.panes.placed && total != self.panes.last {
            // a resize: spread the change by the panes' weights (log 3 : right 2; 2 : 1 : 1)
            let d = total - self.panes.last;
            let min_pane = (total.x * 0.3).min(200.0);
            self.panes.left = (self.panes.left + d.x * 3.0 / 5.0).clamp(min_pane, total.x - min_pane);
            let a = self.panes.a + d.y * 2.0 / 4.0;
            let b = self.panes.b + d.y * 3.0 / 4.0;
            self.panes.a = a.max(0.0);
            self.panes.b = b.max(self.panes.a + 10.0);
            self.panes.last = total;
        }
        // popped-out sections leave the main window; a column with nothing left goes away
        let popped = |c: Card| self.is_popped(c);
        let (info_out, log_out, notices_out) = (popped(Card::Info), popped(Card::Log), popped(Card::Notices));
        let vis = [!popped(Card::Spots), !popped(Card::Done), !popped(Card::Worked)];
        let left_gone = info_out && log_out;
        let right_gone = !vis.iter().any(|v| *v) && (notices_out || self.notices.is_empty());
        let left_w = if left_gone { 0.0 } else if right_gone { total.x } else { self.panes.left.round() };
        // the new-parks card takes its height from the Spots card, so the others keep theirs
        let rw0 = total.x - left_w - 10.0;
        let nh = if rw0 > 150.0 && !self.notices.is_empty() && !notices_out { (self.notices_layout(ui, rw0).1).min(total.y * 0.4) + 10.0 } else { 0.0 };
        if (nh - self.panes.nh).abs() > 0.5 {
            let d = nh - self.panes.nh;
            self.panes.a = (self.panes.a - d).max(0.0);
            self.panes.b = (self.panes.b - d).max(self.panes.a + 10.0);
            self.panes.nh = nh;
        }
        let pz = &self.panes;
        let (a, b) = (pz.a.round(), pz.b.round());
        let left = left_w;
        let col = area.top() + nh;
        // the Current Info card sits above the Activity log and takes its height from it
        let info_h = if left > 120.0 && !info_out {
            let info = self.build_info(ui, left);
            self.info_height(ui, &info).min(total.y * 0.55)
        } else {
            0.0
        };
        let info_rect = Rect::from_min_size(area.min, Vec2::new(left, info_h));
        let log_top = if info_h > 0.0 { area.top() + info_h + 10.0 } else { area.top() };
        let log_rect = if log_out {
            Rect::NOTHING
        } else {
            Rect::from_min_max(Pos2::new(area.left(), log_top), Pos2::new(area.left() + left, area.bottom()))
        };
        let rx = if left_gone { area.left() } else { area.left() + left + 10.0 };
        let rw = if right_gone { 0.0 } else if left_gone { total.x } else { total.x - left - 10.0 };
        let notices_rect = if notices_out { Rect::NOTHING } else { Rect::from_min_size(Pos2::new(rx, area.top()), Vec2::new(rw, (nh - 10.0).max(0.0))) };
        // the three lists share the rest of the column: the dividers' sizes when all are
        // there, else their sizes scaled up to fill it
        let (spots_rect, done_rect, worked_rect);
        if vis.iter().all(|v| *v) {
            spots_rect = Rect::from_min_size(Pos2::new(rx, col), Vec2::new(rw, a));
            done_rect = Rect::from_min_max(Pos2::new(rx, col + a + 10.0), Pos2::new(rx + rw, col + b));
            worked_rect = Rect::from_min_max(Pos2::new(rx, col + b + 10.0), Pos2::new(rx + rw, area.bottom()));
        } else {
            let column_h = area.bottom() - col;
            let weights = [a.max(20.0), (b - a - 10.0).max(20.0), (column_h - b - 10.0).max(20.0)];
            let n = vis.iter().filter(|v| **v).count();
            let sum: f32 = (0..3).filter(|i| vis[*i]).map(|i| weights[i]).sum();
            let avail = (column_h - 10.0 * n.saturating_sub(1) as f32).max(0.0);
            let mut y = col;
            let mut rects = [Rect::NOTHING; 3];
            for i in 0..3 {
                if vis[i] {
                    let h = (avail * weights[i] / sum).floor();
                    rects[i] = Rect::from_min_size(Pos2::new(rx, y), Vec2::new(rw, h));
                    y += h + 10.0;
                }
            }
            (spots_rect, done_rect, worked_rect) = (rects[0], rects[1], rects[2]);
        }
        // dividers
        let mut sashes = vec![];
        if !left_gone && !right_gone {
            sashes.push((0u8, Rect::from_min_size(Pos2::new(area.left() + left, area.top()), Vec2::new(10.0, total.y)), CursorIcon::ResizeHorizontal));
        }
        if vis.iter().all(|v| *v) {
            sashes.push((1u8, Rect::from_min_size(Pos2::new(rx, col + a), Vec2::new(rw, 10.0)), CursorIcon::ResizeVertical));
            sashes.push((2u8, Rect::from_min_size(Pos2::new(rx, col + b), Vec2::new(rw, 10.0)), CursorIcon::ResizeVertical));
        }
        for (i, r, cursor) in sashes {
            let resp = ui.interact(r, Id::unique(("sash", i)), Sense::drag());
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(cursor);
            }
            if resp.dragged() {
                let d = resp.drag_delta();
                match i {
                    0 => self.panes.left = (self.panes.left + d.x).clamp(0.0, total.x - 10.0),
                    1 => self.panes.a = (self.panes.a + d.y).clamp(0.0, self.panes.b - 10.0),
                    _ => self.panes.b = (self.panes.b + d.y).clamp(self.panes.a + 10.0, total.y - nh - 10.0),
                }
            }
            if resp.drag_stopped() {
                self.layout_due = Some(self.now() + 0.5);
            }
        }
        // each card draws inside its own rectangle (nothing spills out in a narrow window)
        let outer_clip = ui.clip_rect();
        let cards = [
            (Card::Info, info_rect, left > 120.0 && info_h > 2.0),
            (Card::Log, log_rect, log_rect.width() > 2.0 && log_rect.height() > 2.0),
            (Card::Notices, notices_rect, notices_rect.height() > 2.0 && rw > 2.0),
            (Card::Spots, spots_rect, spots_rect.height() > 2.0 && rw > 2.0),
            (Card::Done, done_rect, done_rect.height() > 2.0 && rw > 2.0),
            (Card::Worked, worked_rect, worked_rect.height() > 2.0 && rw > 2.0),
        ];
        for (card, rect, show) in cards {
            if show {
                self.card_rects.insert(card, rect);
                ui.set_clip_rect(outer_clip.intersect(rect));
                self.draw_card(ui, card, rect);
            }
        }
        ui.set_clip_rect(outer_clip);
    }

    // --- sections in their own windows ---------------------------------------------------

    fn is_popped(&self, card: Card) -> bool {
        self.pops.get(&card).is_some_and(|p| p.popped)
    }

    fn draw_card(&mut self, ui: &mut Ui, card: Card, rect: Rect) {
        match card {
            Card::Info => self.draw_info_card(ui, rect),
            Card::Log => self.draw_log_card(ui, rect),
            Card::Notices => self.draw_notices_card(ui, rect),
            Card::Spots => self.draw_spots_card(ui, rect),
            Card::Done => self.draw_done_card(ui, rect),
            Card::Worked => self.draw_worked_card(ui, rect),
        }
    }

    /// The pop-out / dock-back button at the right end of a card's header row; returns
    /// the x of its left edge, so the header's other right-aligned items stop short of it.
    fn pop_button(&mut self, ui: &mut Ui, card: Card, inner: Rect, hy: f32, hc: f32) -> f32 {
        let (p, f) = (self.p.clone(), self.f.clone());
        let in_window = self.in_pop == Some(card);
        let (icon, text, tip) = if in_window {
            (theme::ICON_DOCK, "Dock", "Put this section back in the main window")
        } else {
            (theme::ICON_POP, "Pop out", "Move this section into its own window")
        };
        let (txt, font) = if f.has_icons { (icon, &f.icon_12) } else { (text, &f.small_bold) };
        let b = Rnd::new(txt, font).pill().pad(if f.has_icons { 8.0 } else { 10.0 }, 2.0).colors(p.neutral, p.neutral_text).hover(p.neutral_hover).icon();
        let sz = b.size(ui);
        let r = Rect::from_min_size(Pos2::new(inner.right() - PAD - sz.x, hy + ((hc - sz.y) / 2.0).floor()), sz);
        let resp = b.show(ui, Id::unique(("pop-btn", card.key())), r, true);
        self.tips.tip(&resp, tip, 500, TipAt::Below(r));
        if resp.clicked() {
            self.pop_req.push((card, !in_window));
        }
        r.left() - 10.0
    }

    /// Carry out the pop-out / dock-back requests of this frame.
    fn apply_pop_requests(&mut self, ctx: &egui::Context) {
        for (card, out) in std::mem::take(&mut self.pop_req) {
            if out {
                let main = ctx.input(|i| i.viewport().inner_rect).unwrap_or(Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(1000.0, 700.0)));
                let r = self.card_rects.get(&card).copied().unwrap_or(Rect::from_min_size(Pos2::new(40.0, 40.0), Vec2::new(500.0, 300.0)));
                let size = Vec2::new(r.width().max(260.0), r.height().max(160.0));
                let fresh = Rect::from_min_size(main.min + r.min.to_vec2() + Vec2::splat(30.0), size);
                // a window that was popped out before reopens where it was
                let pop = self.pops.entry(card).or_insert_with(|| Pop::new(true, fresh));
                pop.popped = true;
                pop.placed = false;
                pop.titled = None;
                pop.hwnd = 0;
            } else if let Some(pop) = self.pops.get_mut(&card) {
                pop.popped = false;
                pop.init = pop.cur;
            }
            self.layout_due = Some(self.now() + 0.5);
        }
    }

    /// Every popped-out section in its own window.
    fn draw_popouts(&mut self, ctx: &egui::Context) {
        use egui::{ViewportBuilder, ViewportId};
        self.apply_pop_requests(ctx);
        let dark = self.theme_name == "dark";
        let cards: Vec<Card> = self.pops.iter().filter(|(_, p)| p.popped).map(|(c, _)| *c).collect();
        for card in cards {
            if !self.pops[&card].placed {
                self.place_pop(card, ctx);
            }
            let title = format!("{TITLE} - {}", card.title());
            let init = self.pops[&card].init;
            let builder = ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size(init.size())
                .with_position(init.min)
                .with_min_inner_size(Vec2::new(220.0, 120.0));
            let id = ViewportId::from_hash_of(("pop", card.key()));
            // this window's own hover hints while it is drawn
            let mut tips = std::mem::take(&mut self.pops.get_mut(&card).unwrap().tips);
            std::mem::swap(&mut self.tips, &mut tips);
            self.in_pop = Some(card);
            let (mut dock, mut active, mut cur) = (false, false, None);
            let mut typed: Vec<char> = Vec::new();
            ctx.show_viewport_immediate(id, builder, |ui: &mut Ui, _class| {
                let (p, f) = (self.p.clone(), self.f.clone());
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
                ui.painter().rect_filled(full, 0.0, p.bg);
                let rect = full.shrink(PAD / 2.0);
                let outer = ui.clip_rect();
                ui.set_clip_rect(outer.intersect(rect));
                self.draw_card(ui, card, rect);
                ui.set_clip_rect(outer);
                if self.popup_in == Some(card) {
                    if let Some(menu) = self.popup.take() {
                        match menu.show(ui.ctx(), ui, &p, &f) {
                            None => self.popup = Some(menu),
                            Some(Some(cmd)) => {
                                self.popup_in = None;
                                self.menu_cmd(cmd);
                            }
                            Some(None) => self.popup_in = None,
                        }
                    } else {
                        self.popup_in = None;
                    }
                }
                let tf = f.tip.clone();
                self.tips.show(ui.ctx(), ui, &p, &tf);
                if ui.input(|i| i.viewport().close_requested()) {
                    dock = true;
                }
                let (o, i) = ui.ctx().input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
                if let (Some(o), Some(i)) = (o, i) {
                    cur = Some(Rect::from_min_size(o.min, i.size()));
                }
            });
            self.in_pop = None;
            std::mem::swap(&mut self.tips, &mut tips);
            let now = self.now();
            let mut moved = false;
            if let Some(pop) = self.pops.get_mut(&card) {
                pop.tips = tips;
                if let Some(c) = cur {
                    if c != pop.cur {
                        pop.cur = c;
                        moved = true;
                    }
                }
                if dock {
                    pop.popped = false;
                    pop.init = pop.cur;
                    moved = true;
                }
                if pop.titled != Some(dark) {
                    if pop.hwnd == 0 {
                        pop.hwnd = theme::find_own_window(&title);
                    }
                    if pop.hwnd != 0 {
                        theme::set_title_bar(pop.hwnd, dark);
                        refresh_frame(pop.hwnd);
                        pop.titled = Some(dark);
                    }
                }
            }
            if moved {
                self.layout_due = Some(now + 0.5);
            }
            if active {
                self.activity();
            }
            for c in typed {
                self.bypass_keys(c);
            }
        }
    }

    /// Make sure a popped-out window opens on a monitor and isn't tiny.
    fn place_pop(&mut self, card: Card, ctx: &egui::Context) {
        let ppp = self.ppp.max(0.5);
        let main = ctx.input(|i| i.viewport().inner_rect).unwrap_or(Rect::from_min_size(Pos2::new(100.0, 100.0), Vec2::new(1000.0, 700.0)));
        let (vl, vt, vw, vh) = theme::virtual_desktop();
        let Some(pop) = self.pops.get_mut(&card) else { return };
        let r = pop.init;
        let (px, py) = (r.left() * ppp + 40.0, r.top() * ppp + 10.0);
        let on_screen = px >= vl as f32 && px < (vl + vw) as f32 && py >= vt as f32 && py < (vt + vh) as f32;
        let size = Vec2::new(r.width().max(220.0), r.height().max(120.0));
        pop.init = if on_screen { Rect::from_min_size(r.min, size) } else { Rect::from_min_size(main.min + Vec2::splat(60.0), size) };
        pop.cur = pop.init;
        pop.placed = true;
    }

    fn save_popouts(&mut self) {
        self.s.popouts = self
            .pops
            .iter()
            .map(|(c, p)| {
                let r = if p.popped { p.cur } else { p.init };
                (c.key().to_string(), settings::PopGeom { popped: p.popped, x: r.left() as f64, y: r.top() as f64, w: r.width() as f64, h: r.height() as f64 })
            })
            .collect();
    }

    fn title_bar(&mut self) {
        let dark = self.theme_name == "dark";
        if self.titled_dark == Some(dark) {
            return;
        }
        if self.hwnd == 0 {
            self.hwnd = theme::find_own_window(TITLE);
        }
        if self.hwnd != 0 {
            theme::set_title_bar(self.hwnd, dark);
            refresh_frame(self.hwnd);
            self.titled_dark = Some(dark);
        }
    }

    /// Test aid: POTA_DEMO="counts,dark,panel:radio,..." puts the window in a state for
    /// screenshots without clicking (each step runs once, three seconds after start).
    fn run_demo(&mut self, ctx: &egui::Context) {
        use crate::panel::{Page, Tab};
        for step in std::mem::take(&mut self.demo) {
            let now = self.now();
            match step.as_str() {
                "dark" | "light" => self.apply_theme(ctx, Some(&step)),
                "counts" => {
                    let c = HuntCounts::new(&serde_json::json!({"US-1234": 3, "US-1574": 1}),
                                            &serde_json::json!({"K4SWL": 2, "KB3EOF": 7}));
                    *self.counts.borrow_mut() = Some(Arc::new(c));
                    self.spots_key = None;
                    self.refresh_spots_list();
                }
                "hunt" => self.set_hunting(true),
                "cq" => self.set_cq(true),
                "skip" => {
                    self.h.skip("N5XYZ", Some("US-1234"));
                    self.drain_hunter();
                    self.spots_key = None;
                    self.refresh_spots_list();
                }
                "confirm" => self.dialog = Some(Dialog::new(DialogKind::ConfirmHunt)),
                "presence" => self.dialog = Some(Dialog::new(DialogKind::PresenceAsk)),
                "expired" => self.dialog = Some(Dialog::new(DialogKind::PresenceExpired("Auto-Hunt".into()))),
                "band" => self.retune_menu("band"),
                "tip" => self.demo_tip = true,
                "popup" => {
                    if let Some(d) = self.spots_tree.rows.first().and_then(|r| r.data.clone()) {
                        self.table_events(vec![TableEvent::Context(d, Pos2::new(1200.0, 300.0))]);
                    }
                }
                s if s.starts_with("panel") => {
                    let (tab, page) = match s.split(':').nth(1).unwrap_or("") {
                        "radio" => (Tab::Radio, Page::Main),
                        "logging" => (Tab::Logging, Page::Main),
                        "connect" => (Tab::Connect, Page::Main),
                        "appearance" => (Tab::Appearance, Page::Main),
                        "about" => (Tab::About, Page::Main),
                        "aclog" => (Tab::Logging, Page::Aclog),
                        "wsjtx" => (Tab::Connect, Page::Wsjtx),
                        "pota" => (Tab::Connect, Page::Pota),
                        _ => (Tab::Hunting, Page::Main),
                    };
                    let mut panel = std::mem::take(&mut self.panel);
                    panel.open_on(self, now, tab, page);
                    self.panel = panel;
                }
                _ => {}
            }
        }
    }
}

/// Repaint a window's frame now (DWM only redraws it on activation otherwise).
pub fn refresh_frame(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER};
    unsafe {
        SetWindowPos(hwnd as _, std::ptr::null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.p.bg.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.hwnd == 0 {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = frame.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    self.hwnd = w.hwnd.get();
                }
            }
        }
        if self.closing {
            return;
        }
        self.ppp = ctx.pixels_per_point();
        self.style_egui(&ctx);
        self.title_bar();
        while let Ok(e) = self.rx.try_recv() {
            self.dispatch(e, &ctx);
        }
        self.drain_hunter();
        let now = self.now();
        if now >= self.next_tick {
            self.next_tick = now + 0.1;
            self.h.tick();
            self.drain_hunter();
        }
        if now >= self.next_status {
            self.next_status = now + 1.0;
            self.update_status(&ctx);
        }
        if self.radio == Some(Radio::Decode) && epoch_now() >= self.decode_until && !self.h.status.as_ref().is_some_and(|s| s.decoding) {
            self.update_wsjtx_status(); // the held DECODING ends with the period
        }
        self.run_timers();
        self.handle_input(&ctx);
        // heard rows stop pulsing once every station's time is up
        if !self.heard.is_empty() {
            self.heard.retain(|_, u| *u > now);
            if self.heard.is_empty() {
                for t in [&mut self.spots_tree, &mut self.done_tree] {
                    for r in t.rows.iter_mut() {
                        r.tags.heard = false;
                    }
                }
            }
        }
        self.glow_buttons(pulse(now, GLOW_PERIOD));
        self.drain_hunter();
        if !self.demo.is_empty() && now > 3.0 {
            self.run_demo(&ctx);
        }
        let screen = ctx.content_rect();
        ui.painter().rect_filled(screen, 0.0, self.p.bg);
        let w = screen.width();
        let top = self.draw_header(ui, w);
        let strip_bottom = self.draw_strip(ui, top, w);
        let area = Rect::from_min_max(Pos2::new(PAD, strip_bottom), Pos2::new(w - PAD, screen.height() - PAD));
        if area.width() > 20.0 && area.height() > 20.0 {
            self.draw_panes(ui, area);
        }
        // the settings panel, popups and dialogs
        let mut panel = std::mem::take(&mut self.panel);
        panel.show(self, &ctx, ui);
        if !self.panel.is_open() && !panel.is_open() {
            self.panel = panel;
        } else if self.panel.is_open() {
            // the panel was toggled during show (e.g. Ctrl+,) — keep the newer state
        } else {
            self.panel = panel;
        }
        if let Some(menu) = if self.popup_in.is_none() { self.popup.take() } else { None } {
            match menu.show(&ctx, ui, &self.p, &self.f) {
                None => self.popup = Some(menu),
                Some(Some(cmd)) => self.menu_cmd(cmd),
                Some(None) => {}
            }
        }
        self.draw_wsjtx_wait(&ctx, screen);
        if let Some(mut d) = self.dialog.take() {
            // the main window can't be used while a dialog is up
            egui::Area::new(Id::unique("modal-blocker"))
                .order(Order::Middle)
                .fixed_pos(Pos2::ZERO)
                .show(&ctx, |ui| {
                    ui.allocate_response(screen.size(), Sense::click_and_drag());
                });
            let result = dialogs::show(&mut d, self, &ctx);
            let typed = std::mem::take(&mut d.typed);
            match result {
                dialogs::Outcome::Open => {
                    if self.dialog.is_none() {
                        self.dialog = Some(d);
                    }
                }
                dialogs::Outcome::Closed => {}
                dialogs::Outcome::Enable => match d.kind {
                    DialogKind::ConfirmHunt => self.set_hunting(true),
                    DialogKind::ConfirmCq => self.set_cq(true),
                    _ => {}
                },
                dialogs::Outcome::StillHere => {
                    self.presence.arm();
                    self.log("Presence confirmed; Auto modes stay on.", Tag::Hunt);
                }
                dialogs::Outcome::Expired => self.presence_expired(),
            }
            // keys typed in a dialog window count for the presence bypass too
            for c in typed {
                self.bypass_keys(c);
            }
        }
        if self.demo_tip {
            let at = Rect::from_min_size(Pos2::new(200.0, 200.0), Vec2::new(300.0, 20.0));
            self.tips.tip_area(Id::new("demo-tip"), true, "Click to turn Auto CQ/Answer on or off.\nCalls CQ when idle and answers stations that call you;\nYou've hunted K4SWL 2 times", 0, TipAt::Below(at));
        }
        self.draw_popouts(&ctx);
        let tips_font = self.f.tip.clone();
        let p = self.p.clone();
        self.tips.show(&ctx, ui, &p, &tips_font);

        // save the window and dividers half a second after they change
        let outer = ctx.input(|i| i.viewport().outer_rect);
        if outer.is_some() && outer != self.last_outer {
            if self.last_outer.is_some() {
                self.layout_due = Some(now + 0.5);
            }
            self.last_outer = outer;
        }
        if self.layout_due.is_some_and(|t| now >= t) && now > 1.5 {
            self.layout_due = None;
            self.persist_layout(&ctx);
        }

        // one text selection at a time: the app-wide one starts only on text no other
        // selection owns, and any other selection starting drops it (a new press does)
        let select = self.p.select;
        if self.tsel.end_frame(&ctx, Color32::from_rgba_unmultiplied(select.r(), select.g(), select.b(), 120)) {
            self.clear_text_selections();
            self.panel.clear_selection();
        }
        // keep animations and timers running
        let busy = self.radio.is_some() || self.h.target().is_some() || !self.heard.is_empty()
            || (!self.waiting.0.is_empty() && self.waiting.1) || self.dialog.is_some()
            || self.log.buttons_mut().any(|b| b.glowing);
        ctx.request_repaint_after(Duration::from_millis(if busy { 40 } else { 100 }));
        let _ = Stroke::NONE;
    }

    fn on_exit(&mut self) {
        self.closing = true;
        self.listener.lock().unwrap().stop();
        self.poller.stop();
        self.stats.stop();
        self.settings_changed();
        self.rig_restore("Closing, setting ATT/NB/DNR back to normal");
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // save the layout and window on close, before anything that could fail or hang
        if raw.viewports.get(&egui::ViewportId::ROOT).is_some_and(|v| v.close_requested()) {
            self.save_layout(ctx);
            if let Some(g) = self.geometry_string(ctx) {
                self.s.window = Some(g);
            }
        }
    }
}
