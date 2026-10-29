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

use chrono::{DateTime, Utc};
use eframe::egui::{self, Color32, CursorIcon, Id, Key, Order, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2};

use crate::aclog::AclogClient;
use crate::cat::RigClient;
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
use crate::table::{Cell, Chip, Col, Icon, Lead, Row, RowColors, RowData, RowTags, SortKey, Table, TableEvent};
use crate::theme::{self, Fonts, Palette};
use crate::ui::{self, measure, Rnd, TipAt, Tips};
use crate::wsjtx_gui;
use crate::wsjtx_udp::{self, Decode, ListenerShared, Message};
use crate::{logdialog, respot, sound};

pub const TITLE: &str = "POTA Auto-Digimationator";
pub const PAD: f32 = 12.0;
/// corner radius of the cards
const CARD_RADIUS: f32 = 6.0;
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
    /// label, pulse period (s), dark and bright color
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
    UpdateCheck,
}

/// how often the app looks for a newer release
const UPDATE_EVERY: f64 = 6.0 * 3600.0;

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
        // auto_only=true unticks Enable Tx (the plain halt leaves it ticked while receiving);
        // auto_only=false stops a transmission in progress
        let l = self.listener.lock().unwrap();
        l.send(&wsjtx_udp::build_halt_tx(id, true))?;
        l.send(&wsjtx_udp::build_halt_tx(id, false))
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
        // tick polls this 10x a second and EnumWindows walks every window: reuse a result under 0.4 s old
        static LAST: std::sync::Mutex<Option<(Instant, bool)>> = std::sync::Mutex::new(None);
        let mut last = LAST.lock().unwrap();
        if let Some((t, open)) = *last {
            if t.elapsed() < Duration::from_millis(400) {
                return Some(open);
            }
        }
        let open = !logdialog::find_log_windows().is_empty();
        *last = Some((Instant::now(), open));
        Some(open)
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
    /// width of the left column
    left: f32,
    placed: bool,
    last: Vec2,
    /// the sections of the left and right columns in order (popped-out ones keep their place)
    cols: [Vec<Card>; 2],
    /// relative heights of the sections that share the rest of their column
    weights: HashMap<Card, f32>,
    /// a header being dragged to another place
    grab: Option<Card>,
}

impl Panes {
    fn default_cols() -> [Vec<Card>; 2] {
        [vec![Card::Info, Card::Log], vec![Card::Notices, Card::Spots, Card::Done, Card::Worked]]
    }

    fn new() -> Self {
        let weights = [(Card::Log, 1.0), (Card::Spots, 2.0), (Card::Done, 1.0), (Card::Worked, 1.0)].into_iter().collect();
        Panes { left: 0.0, placed: false, last: Vec2::ZERO, cols: Self::default_cols(), weights, grab: None }
    }
}

/// One line of the Activity card's static area.
#[derive(Clone)]
struct StaticLine {
    /// HH:MM:SS shown before the text; empty for a continuation line
    ts: String,
    text: String,
    /// drawn as a link to the pota.app account settings
    link: bool,
    /// drawn in the error color (a failed sign-in or fetch)
    bad: bool,
}

impl StaticLine {
    fn new(ts: String, text: String) -> Self {
        StaticLine { ts, text, link: false, bad: false }
    }
}

/// Word-wrap the static lines to `avail` px. Continuation rows have no stamp.
fn wrap_static(info: Vec<StaticLine>, font: &crate::theme::Font, avail: f32) -> Vec<StaticLine> {
    let mut out = Vec::new();
    for l in info {
        for (i, text) in ui::wrap_lines(font, &l.text, avail).into_iter().enumerate() {
            out.push(StaticLine { ts: if i == 0 { l.ts.clone() } else { String::new() }, text, link: l.link, bad: l.bad });
        }
    }
    out
}

pub struct App {
    dir: PathBuf,
    pub s: Settings,
    rx: Receiver<Event>,
    post: Post,
    listener: Arc<Mutex<Arc<ListenerShared>>>,
    poller: Option<Arc<SpotPoller>>,
    stats: Option<Arc<StatsPoller>>,
    aclog: AclogClient,
    rig: RigClient,
    move_tx: Arc<AtomicBool>,
    pub h: Hunter,
    counts: Rc<RefCell<Option<Arc<HuntCounts>>>>,
    pub pota_status: String,
    /// the last sign-in or hunt-count fetch failed (the Current Info line is red)
    pota_failed: bool,
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
    notices: Vec<(String, String, usize, String)>,
    info_cache: Option<((u64, u32), Vec<StaticLine>)>,
    heard: HashMap<String, f64>,
    /// newest spot time seen per new-parks line (band, mode), and until when each pulses
    notice_seen: HashMap<(String, String), String>,
    notice_pulse: HashMap<(String, String), f64>,
    /// (base call, band) -> `now()` of the last decode sent by that station
    last_heard: HashMap<(String, String), f64>,
    /// last band WSJT-X reported; kept while the dial is between bands (mid-retune)
    heard_band: std::cell::Cell<Option<&'static str>>,
    decode_until: f64,
    radio: Option<Radio>,
    periods: PeriodDetector,
    pub presence: PresenceTimer,
    presence_off: bool,
    typed: Vec<(f64, char)>,
    state_ready: bool,
    rig_key: Option<(String, String)>,
    rig_applied: bool,
    /// the Radio tab's CAT test is running
    pub cat_testing: bool,
    /// "Check credentials" is running / its last result (ok, text)
    pub cred_checking: bool,
    pub cred_result: Option<(bool, String)>,
    /// a newer GitHub release than this build, shown as a chip in Current Info
    update: Option<crate::update::Release>,
    /// the update check or the download is running
    update_busy: bool,
    /// a check has worked, so "up to date" is true
    update_ok: bool,
    /// the last update problem (red line in Current Info) or "up to date" after a manual check: (text, bad)
    update_note: Option<(String, bool)>,
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
    /// the last pota.app spot fetch failed: the chip's dot is red until one works
    spots_failed: bool,
    /// "ATT/NB/NR off": the last rig change, shown as "[CAT: ...]" after the WSJT-X line in the log's static area.
    static_rig: String,
    layout_due: Option<f64>,
    last_outer: Option<Rect>,
    last_geometry: Option<Rect>,
    pub gear_rect: Rect,
    logo: Option<egui::TextureHandle>,
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
    /// a decode has arrived since WSJT-X connected (the spinner stays until one does)
    wsjtx_decoded: bool,
    /// the setup wizard, while it is showing (first run, or re-run from Settings)
    pub wizard: Option<crate::panel::Wizard>,
    /// first run: settings.json does not exist yet, nothing is written until the wizard finishes
    unsaved: bool,
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

/// A band name like "20m" or "70cm" in metres, for sorting (unknown = 0).
fn band_meters(band: &str) -> f64 {
    let n: f64 = band.trim_end_matches(|c: char| c.is_alphabetic()).parse().unwrap_or(0.0);
    if band.ends_with("cm") { n / 100.0 } else if band.ends_with("mm") { n / 1000.0 } else { n }
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
        // First run: no settings.json yet. The setup wizard runs before anything is started
        // (no listener, no pota.app polling); `begin_operation` starts them afterwards.
        let first_run = !dir.join("data").join("settings.json").exists();
        let listener = Arc::new(Mutex::new(if first_run {
            wsjtx_udp::idle_listener(&s.udp_group, s.udp_port)
        } else {
            wsjtx_udp::start_listener(post.clone(), &s.udp_group, s.udp_port)
        }));
        let poller = (!first_run).then(|| pota_spots::start_poller(post.clone(), dir.join("data").join("spots_cache.json")));
        let stats = (!first_run).then(|| pota_auth::start_stats_poller(post.clone()));
        let aclog = AclogClient::new(post.clone(), &s.aclog_host, s.aclog_port);
        let rig = RigClient::new(post.clone());
        rig.set_cfg((!s.rig_com.is_empty()).then(|| crate::cat::Cfg {
            rig: crate::cat::resolve(&s.rig_type, &s.rig_model, &s.rig_civ),
            port: s.rig_com.clone(),
            baud: s.rig_baud,
            data_bits: s.rig_data_bits,
            stop_bits: s.rig_stop_bits,
            handshake: s.rig_handshake.clone(),
        }));
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
        let sorts = s.sorts.clone();
        let saved_sort = |mut t: Table| {
            if let Some((c, asc)) = sorts.get(t.id) {
                t.set_sort(c, *asc);
            }
            t
        };
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
            rig,
            move_tx,
            h,
            counts,
            pota_status: String::new(),
            pota_failed: false,
            last_wsjtx: None,
            t0,
            theme_name,
            p,
            f,
            log: LogView::new(),
            spots_tree: saved_sort(Table::new("spots", spots_cols).sorted_by("ago", true)),
            done_tree: saved_sort(Table::new("done", done_cols).sorted_by("ago", true)),
            worked_tree: saved_sort(Table::new("worked", worked_cols).sorted_by("time", false)),
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
            notices: vec![],
            info_cache: None,
            heard: HashMap::new(),
            notice_seen: HashMap::new(),
            notice_pulse: HashMap::new(),
            last_heard: HashMap::new(),
            heard_band: std::cell::Cell::new(None),
            decode_until: 0.0,
            radio: None,
            periods: PeriodDetector::default(),
            presence: PresenceTimer::new(PRESENCE_SECONDS, PRESENCE_WARN, Box::new(mono_clock(t0))),
            presence_off: false,
            typed: vec![],
            state_ready: false,
            rig_key: None,
            rig_applied: false,
            cat_testing: false,
            cred_checking: false,
            cred_result: None,
            update: None,
            update_busy: false,
            update_ok: false,
            update_note: None,
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
            panes: Panes::new(),
            next_tick: 0.0,
            next_status: 1.0,
            first_status_done: false,
            hwnd: 0,
            titled_dark: None,
            day_left: String::new(),
            spots_text: "Spots (pota.app): waiting for first fetch".into(),
            spots_failed: false,
            static_rig: String::new(),
            layout_due: None,
            last_outer: None,
            last_geometry: None,
            gear_rect: Rect::NOTHING,
            logo: None,
            band_rect: Rect::NOTHING,
            mode_rect: Rect::NOTHING,
            ppp: 1.0,
            fit_right_w,
            closing: false,
            demo: std::env::var("POTA_DEMO").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default(),
            demo_tip: false,
            wsjtx_seen: false,
            wsjtx_decoded: false,
            wizard: None,
            unsaved: first_run,
        };
        // demo screenshots skip the WSJT-X popup unless they ask for it ("wait")
        app.wsjtx_seen = !app.demo.is_empty() && !app.demo.iter().any(|s| s == "wait");
        for t in [&mut app.spots_tree, &mut app.done_tree, &mut app.worked_tree] {
            t.size_from_samples(&app.f);
        }
        if first_run {
            app.wizard = Some(crate::panel::Wizard::new(&app.s, false));
            app.demo_wizard();
        } else {
            app.start_data();
        }
        app
    }

    /// Today's worked list, the pota.app login and the first rig setting (everything that
    /// needs the settings to be final).
    fn start_data(&mut self) {
        self.seed_worked();
        let (u, pw) = (self.s.pota_user.clone(), self.s.pota_password.clone());
        self.apply_pota_login(&u, &pw, false);
        self.after(2.0, Timer::RigClean(false));
        if self.demo.is_empty() {
            self.after(10.0, Timer::UpdateCheck);
        }
        self.render_worked();
    }

    /// The GitHub token for reading a private repository's releases (settings.json
    /// `github_token`, no UI), or None.
    fn github_token(&self) -> Option<String> {
        self.s.extra.get("github_token").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
    }

    /// Look for a newer release on a thread; `manual` = the user asked (say so when up to date).
    pub fn check_update(&mut self, manual: bool) {
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        let (post, token) = (self.post.clone(), self.github_token());
        std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || post(Event::UpdateCheck(crate::update::check(token.as_deref()), manual)))
            .ok();
    }

    /// Download the new exe, swap it in and (the app closing) start it. Refused while a QSO is going on.
    fn start_update(&mut self) {
        let Some(rel) = self.update.clone() else { return };
        if self.update_busy {
            return;
        }
        if let Some(why) = self.h.retune_blocker() {
            self.update_note = Some((format!("Can't update now: {why}"), true));
            return;
        }
        self.update_busy = true;
        self.update_note = Some((format!("Downloading version {}...", rel.version), false));
        let (post, token) = (self.post.clone(), self.github_token());
        std::thread::Builder::new()
            .name("update-install".into())
            .spawn(move || post(Event::UpdateDone(crate::update::install(&rel, token.as_deref()))))
            .ok();
    }

    /// The setup wizard is done (first run): start the WSJT-X listener and the pota.app
    /// pollers with the chosen settings and begin operating normally.
    fn begin_operation(&mut self) {
        let new = wsjtx_udp::start_listener(self.post.clone(), &self.s.udp_group, self.s.udp_port);
        let old = std::mem::replace(&mut *self.listener.lock().unwrap(), new);
        old.stop();
        self.poller = Some(pota_spots::start_poller(self.post.clone(), self.dir.join("data").join("spots_cache.json")));
        self.stats = Some(pota_auth::start_stats_poller(self.post.clone()));
        self.settings_changed();
        self.start_data();
    }

    fn wizard_frame(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        let Some(mut w) = self.wizard.take() else { return };
        if w.show(self, ctx, ui) {
            if !w.is_rerun() {
                self.begin_operation();
            }
        } else {
            self.wizard = Some(w);
        }
    }

    /// POTA_DEMO "wizard:N" (page number from 1) or "wizard:done": show that wizard page.
    fn demo_wizard(&mut self) {
        let steps: Vec<String> = self.demo.iter().filter_map(|s| s.strip_prefix("wizard:")).map(str::to_string).collect();
        if let Some(w) = self.wizard.as_mut() {
            for s in steps {
                w.demo(&s);
            }
        }
    }

    /// Open the setup wizard again from Settings, starting from the current settings.
    pub fn start_wizard(&mut self) {
        self.wizard = Some(crate::panel::Wizard::new(&self.s, true));
    }

    /// The wizard's Finish: write settings.json (first run) or apply the changes to the
    /// running app (re-run).
    pub fn wizard_apply(&mut self) -> Result<(), String> {
        if self.unsaved {
            settings::save(&self.dir.join("data").join("settings.json"), &self.s).map_err(|e| format!("Could not save settings: {e}"))?;
            self.unsaved = false;
            return Ok(());
        }
        let (g, p) = (self.s.udp_group.clone(), self.s.udp_port);
        self.apply_udp(&g, p);
        let (u, pw) = (self.s.pota_user.clone(), self.s.pota_password.clone());
        self.apply_pota_login(&u, &pw, false);
        self.settings_changed();
        Ok(())
    }

    pub fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    fn after(&mut self, secs: f64, t: Timer) {
        let at = self.now() + secs;
        self.timers.push((at, t));
    }

    /// Settings > Radio > Test CAT Control: change and restore NB, NR, ATT and power over
    /// the COM port and show the outcome in a dialog.
    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// The open dialog, if any, as a modal over the main window (or the wizard).
    fn draw_dialog(&mut self, ctx: &egui::Context, screen: Rect) {
        if let Some(mut d) = self.dialog.take() {
            // the main window can't be used while a dialog is up
            egui::Area::new(Id::unique("modal-blocker"))
                .order(Order::Middle)
                .fixed_pos(Pos2::ZERO)
                .show(ctx, |ui| {
                    ui.allocate_response(screen.size(), Sense::click_and_drag());
                });
            let result = dialogs::show(&mut d, self, ctx);
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
                    DialogKind::UpdateAvail(_) => self.start_update(),
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
    }

    /// Give the rig client the radio settings as they are now.
    fn push_rig_cfg(&mut self) {
        self.rig.set_cfg((!self.s.rig_com.is_empty()).then(|| crate::cat::Cfg {
            rig: crate::cat::resolve(&self.s.rig_type, &self.s.rig_model, &self.s.rig_civ),
            port: self.s.rig_com.clone(),
            baud: self.s.rig_baud,
            data_bits: self.s.rig_data_bits,
            stop_bits: self.s.rig_stop_bits,
            handshake: self.s.rig_handshake.clone(),
        }));
    }

    /// Sign in to pota.app with the given login (not saved) to see whether it works.
    pub fn check_pota(&mut self, user: &str, pass: &str) {
        if self.cred_checking {
            return;
        }
        let (user, pass) = (user.trim().to_string(), pass.to_string());
        if user.is_empty() || pass.is_empty() {
            self.cred_result = Some((false, "Enter a username and a password first".into()));
            return;
        }
        self.cred_checking = true;
        self.cred_result = None;
        let post = self.post.clone();
        std::thread::Builder::new()
            .name("pota-check".into())
            .spawn(move || post(Event::PotaCheck(crate::pota_auth::login(&user, &pass).map(|_| ()).map_err(|e| e.to_string()))))
            .ok();
    }

    pub fn start_cat_test(&mut self) {
        if self.cat_testing {
            return;
        }
        if self.h.status.as_ref().is_some_and(|s| s.transmitting) {
            self.dialog = Some(Dialog::new(DialogKind::CatResult(false, vec!["WSJT-X is transmitting; the test would change the transmit power. Try again between transmissions.".into()])));
            return;
        }
        self.push_rig_cfg(); // the wizard edits the settings before they are applied
        self.cat_testing = true;
        self.rig.test();
    }

    // --- Rig receive settings (Yaesu over the COM port) ----------------------------------

    /// ATT, NB, NR off and the Tx power set on the main receiver (the radio may
    /// remember them per band). The first pass after a start or QSY also reads what the
    /// radio had, so it can be put back; the quiet second pass only repeats the settings.
    fn rig_clean(&mut self, quiet: bool) {
        let wants = crate::cat::Wants { clean: self.s.rig_clean, power: self.s.rig_power };
        if !(wants.clean || wants.power.is_some()) {
            return;
        }
        self.rig_applied = true;
        let mut parts = vec![];
        if wants.clean {
            parts.push("ATT/NB/NR off".to_string());
        }
        if let Some(w) = wants.power {
            parts.push(format!("PWR {w} W"));
        }
        let fresh = !quiet && self.s.rig_restore;
        self.rig.apply(wants, fresh, false, &parts.join(", "), quiet);
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
        if !first && (self.s.rig_clean || self.s.rig_power.is_some()) {
            // Let the radio finish changing band before sending, and once more in case
            // it restores its per-band memory late.
            self.after(1.5, Timer::RigClean(false));
            self.after(5.0, Timer::RigClean(true));
        }
    }

    /// On close, put the radio back as it was before we changed it.
    fn rig_restore(&self, why: &str) {
        if self.rig_applied && self.s.rig_restore {
            self.rig.restore(true, why);
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
        self.spots_key = None; // rebuild the lists with the new row colors
        self.refresh_spots_list();
        self.render_worked();
    }

    pub fn set_theme(&mut self, ctx: &egui::Context, mode: &str) {
        self.apply_theme(ctx, Some(mode));
        self.save_settings();
    }

    /// Change the UI font size (points; the other fonts follow) and redraw everything with it.
    pub fn set_font_size(&mut self, ctx: &egui::Context, pt: u32) {
        let pt = pt.clamp(8, 16);
        if pt == self.s.font_size {
            return;
        }
        self.s.font_size = pt;
        self.f = self.f.sized(pt as f32);
        self.log.relayout();
        self.static_sel = None;
        self.style_egui(ctx);
        self.spots_key = None; // the lists measure their columns again
        self.refresh_spots_list();
        self.render_worked();
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
        self.push_rig_cfg();
        self.save_settings();
    }

    pub fn save_settings(&mut self) {
        if self.unsaved {
            return; // first run: settings.json is written when the wizard finishes
        }
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
        if std::mem::take(&mut self.h.wanted_ready) && self.s.pop {
            sound::play(sound::Kind::Pop, &self.s.pop_path);
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
        self.pota_failed = false;
        self.pota_status = if !user.is_empty() && !password.is_empty() { format!("Signing in as {user}") } else { String::new() };
        if let Some(s) = &self.stats {
            s.configure(&user, password);
        }
        if save {
            self.save_settings();
        }
    }

    /// Respot the activator to pota.app after a logged QSO (setting `respot`). Needs the
    /// pota.app login and a park confirmed for the QSO; the post runs on its own thread.
    fn respot(&mut self, q: &wsjtx_udp::QsoLogged, entry: &WorkedEntry) {
        if !self.s.respot {
            return;
        }
        if entry.ref_.is_empty() || entry.ref_guessed {
            self.log(&format!("Not respotting {}: no park confirmed for this QSO", entry.call), Tag::Info);
            return;
        }
        let my_call = self.h.status.as_ref().map(|s| s.de_call.clone()).unwrap_or_default();
        let r = respot::Respot::new(q, &my_call, &entry.ref_, &entry.band);
        if r.spotter.is_empty() {
            self.log("Not respotting: your call isn't known yet", Tag::Info);
            return;
        }
        let body = r.body(&self.s.respot_message);
        if self.s.dry_run {
            self.log(&format!("Dry run: would respot {} at {} on pota.app", r.activator, r.reference), Tag::Info);
            return;
        }
        let (Some(stats), post) = (self.stats.clone(), self.post.clone()) else { return };
        let what = format!("{} at {}", r.activator, r.reference);
        std::thread::spawn(move || {
            match stats.token().and_then(|t| respot::post(&t, &body)) {
                Ok(()) => post(Event::Aclog(format!("Respotted {what} on pota.app"), Tag::Worked)),
                Err(e) => post(Event::Aclog(format!("Could not respot {what} on pota.app ({e})"), Tag::Error)),
            }
        });
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
                        logdialog::refresh_app_name(&st.id);
                        self.h.handle_status(st);
                        self.update_wsjtx_status(); // show Tx changes immediately
                        self.rig_watch();
                    }
                    Message::Decode(d) => {
                        self.wsjtx_decoded = true;
                        self.h.handle_decode(&d);
                        self.record_heard(&d);
                        self.flash_heard(&d);
                    }
                    Message::QsoLogged(q) => {
                        let entry = self.h.handle_qso_logged(&q);
                        if self.s.ding {
                            sound::play(sound::Kind::Ding, &self.s.ding_path);
                        }
                        self.respot(&q, &entry);
                        for kind in crate::loggers::KINDS {
                            let cfg = self.s.logger(kind);
                            if cfg.enabled {
                                crate::loggers::log_qso(self.post.clone(), kind, cfg.host, cfg.port, q.clone(), entry.clone());
                            }
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
            Event::SpotsOk(ok) => self.spots_failed = !ok,
            Event::PotaStats(c) => {
                self.pota_failed = false;
                self.pota_status = format!("Signed in as {}: {} parks, {} activators hunted",
                                           self.s.pota_user, c.parks.len(), c.activators.len());
                *self.counts.borrow_mut() = Some(c);
                self.spots_key = None;
            }
            Event::PotaStatsError(t) => {
                self.pota_failed = true;
                self.pota_status = match t.split_once("sign-in failed") {
                    Some((_, rest)) => format!("Sign-in failed{rest}"),
                    None => t.clone(),
                };
                self.log(&t, Tag::Error);
            }
            Event::Aclog(t, Tag::Info) if t.starts_with("CAT:") => self.static_rig = t[4..].to_string(),
            Event::Aclog(t, Tag::Info) if t.starts_with("ATT/NB/NR off") || t.starts_with("PWR ") => {}
            Event::Aclog(t, tag) => self.log(&t, tag),
            Event::Retune(what, value, result) => {
                if result != "ok" {
                    self.log(&format!("Could not switch WSJT-X's {what} to {value} ({result}); change it in WSJT-X"), Tag::Error);
                }
            }
            Event::PotaCheck(r) => {
                self.cred_checking = false;
                self.cred_result = Some(match r {
                    Ok(()) => (true, "Signed in to pota.app: these credentials work".into()),
                    Err(e) => (false, format!("Sign-in failed: {e}")),
                });
            }
            Event::UpdateCheck(r, manual) => {
                self.update_busy = false;
                match r {
                    Ok(Some(rel)) => {
                        self.update_ok = true;
                        self.update_note = None;
                        if manual && self.dialog.is_none() {
                            self.dialog = Some(Dialog::new(DialogKind::UpdateAvail(rel.version.clone())));
                        }
                        self.update = Some(rel);
                    }
                    Ok(None) => {
                        self.update_ok = true;
                        self.update = None;
                        self.update_note = None;
                        if manual && self.dialog.is_none() {
                            self.dialog = Some(Dialog::new(DialogKind::UpToDate(env!("CARGO_PKG_VERSION").to_string())));
                        }
                    }
                    // a failed background check stays quiet; the user asking sees why
                    Err(e) => {
                        if manual {
                            if self.dialog.is_none() {
                                self.dialog = Some(Dialog::new(DialogKind::CheckFailed(e.to_string())));
                            } else {
                                self.update_note = Some((format!("Update check failed: {e}"), true));
                            }
                        }
                    }
                }
            }
            Event::UpdateDone(r) => {
                self.update_busy = false;
                match r {
                    Ok(()) => {
                        self.update_note = Some(("Update installed, restarting...".into(), false));
                        crate::update::request_relaunch();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => self.update_note = Some((format!("Update failed: {e}"), true)),
                }
            }
            Event::CatTest(ok, lines) => {
                self.cat_testing = false;
                self.log(&format!("CAT test {}", if ok { "passed" } else { "FAILED" }), if ok { Tag::Info } else { Tag::Error });
                self.dialog = Some(Dialog::new(DialogKind::CatResult(ok, lines)));
            }
            Event::CqTx(r) => {
                if r == "notfound" {
                    self.log("Could not find WSJT-X's Enable Tx checkbox; turn Tx on yourself", Tag::Error);
                } else if r.starts_with("error") {
                    self.log(&format!("Could not switch Tx on in WSJT-X ({r})"), Tag::Error);
                }
            }
        }
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
        let Some(band) = self.heard_band() else { return };
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

    /// The current band, or the last one seen while the dial is between bands.
    fn heard_band(&self) -> Option<&'static str> {
        if let Some(b) = self.h.band() {
            self.heard_band.set(Some(b));
        }
        self.heard_band.get()
    }

    /// Seconds since we last heard `call` on this band, for sorting the Heard column.
    fn heard_key(&self, call: &str) -> SortKey {
        let Some(band) = self.heard_band() else { return SortKey::None };
        match self.last_heard.get(&(base_call(call), band.to_string())) {
            Some(t) => SortKey::Num((self.now() - t).max(0.0).floor()),
            None => SortKey::None,
        }
    }

    /// "12s" under a minute, else whole minutes ("3m"); "-" if never heard on this band.
    fn heard_text(&self, call: &str) -> String {
        let Some(band) = self.heard_band() else { return "-".into() };
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
        let keys: Vec<Vec<SortKey>> = [&self.spots_tree, &self.done_tree]
            .iter()
            .map(|t| t.rows.iter().map(|r| r.data.as_ref().map_or(SortKey::None, |d| self.heard_key(&d.call))).collect())
            .collect();
        let mut changed = false;
        for ((t, texts), keys) in [&mut self.spots_tree, &mut self.done_tree].into_iter().zip(texts).zip(keys) {
            for ((r, text), key) in t.rows.iter_mut().zip(texts).zip(keys) {
                if let Some(k) = r.keys.get_mut(1) {
                    *k = key;
                }
                if let Some(c) = r.cells.get_mut(1) {
                    if c.text != text {
                        c.text = text;
                        changed = true;
                    }
                }
            }
        }
        for t in [&mut self.spots_tree, &mut self.done_tree] {
            if t.sort.0 == 1 {
                t.sort_rows(); // sorted by Heard: keep the order current as the times tick
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
        if call == self.h.my_call() || spots.spots_for_mode(&call, band, &self.h.mode()).is_empty() {
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
        // a newer request replaces any pending check, or it would blame a mode the user has already left
        self.timers.retain(|(_, t)| !matches!(t, Timer::CheckMode(_)));
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
        // second each); first put the radio back as it was before we changed it.
        let post = self.post.clone();
        let band_now = self.h.band().unwrap_or("").to_string();
        let rig = (restore && self.rig_applied && self.s.rig_restore).then(|| self.rig.clone());
        // Fallbacks for WSJT-X builds without the band/mode buttons (WSJT-Z): the mode goes
        // by UDP Configure, the band by tuning the rig to the mode's usual frequency.
        let tune_rig = self.rig.clone();
        let listener = self.listener.clone();
        let client = self.h.status.as_ref().map(|s| s.id.clone()).unwrap_or_default();
        let tune_mode = steps.iter().find(|s| s.0 == "mode").map_or_else(|| self.h.mode(), |s| s.1.clone());
        std::thread::Builder::new()
            .name("retune".into())
            .spawn(move || {
                if let Some(rig) = rig {
                    rig.restore(true, &format!("QSY, putting {band_now} NB/NR/ATT/power back as they were"));
                    std::thread::sleep(Duration::from_millis(500));
                }
                for (i, (what, value)) in steps.into_iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(Duration::from_millis(1500)); // let WSJT-X finish the first change
                    }
                    let mut r = if what == "mode" { wsjtx_gui::set_mode(&value) } else { wsjtx_gui::set_band(&value) };
                    if r.as_deref() == Ok("notfound") {
                        r = if what == "mode" {
                            let sent = listener.lock().unwrap().send(&wsjtx_udp::build_configure(&client, &value));
                            sent.map(|_| "ok".to_string())
                        } else {
                            match wsjtx_gui::dial_hz(&value, &tune_mode) {
                                Some(hz) => tune_rig.tune(hz).map(|_| "ok".to_string()),
                                None => Err(format!("no standard {tune_mode} frequency known for {value}")),
                            }
                        };
                    }
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
            (text, p.off, p.off_hover)
        } else {
            (HUNT_OFF_TEXT.into(), p.on, p.on_hover)
        }
    }

    /// A pulsing orange halo around an enabled Auto button: a few translucent rings of the
    /// palette orange (`new`), fading outward, so it reads on both the light and dark header.
    fn auto_glow(&self, ui: &Ui, r: Rect) {
        let k = 0.5 + 0.5 * pulse(self.now(), 1.6) as f32;
        let h = r.height();
        for i in 1..=6 {
            let grow = i as f32 * 1.5;
            let a = k * 0.85 * (1.0 - (i as f32 - 1.0) / 6.0).powi(2);
            ui.painter().rect_filled(r.expand(grow), (h + 2.0 * grow) / 2.0, self.p.new.gamma_multiply(a));
        }
    }

    fn cq_button(&self) -> (String, Color32, Color32) {
        let p = &self.p;
        if self.h.cq_enabled {
            let text = format!("{CQ_ON_TEXT}{}", if self.h.dry_run { " (Dry run)" } else { "" });
            (text, p.off, p.off_hover)
        } else {
            (CQ_OFF_TEXT.into(), p.on, p.on_hover)
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
        // a line pulses when it is new, or its newest spot got newer (a lower age)
        let until = self.now() + HEARD_SECONDS;
        for (band, mode, _, newest) in &self.notices {
            let key = (band.clone(), mode.clone());
            if self.notice_seen.get(&key).is_none_or(|old| newest > old) {
                self.notice_seen.insert(key.clone(), newest.clone());
                self.notice_pulse.insert(key, until);
            }
        }
        self.spots_tree.clear();
        self.done_tree.clear();
        let (Some(spots), Some(band)) = (self.h.spots.clone(), self.h.band()) else {
            self.spots_count = "0".into();
            self.done_count = "0".into();
            self.spots_title = SPOTS_TITLE.into();
            self.done_title = DONE_TITLE.into();
            // waiting for WSJT-X has its own popup (the big spinner in the Activity card); the list only waits
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
            let mut keys = vec![
                age_minutes(&s.spot_time, now).map_or(SortKey::None, |m| SortKey::Num(m as f64)),
                self.heard_key(&s.activator),
                SortKey::Text(s.activator.to_lowercase()),
                SortKey::Num(s.frequency_khz),
            ];
            if dim {
                keys.push(SortKey::Text(worked.to_string()));
            }
            keys.push(SortKey::Text(format!("{} {}", s.reference, s.park_name).trim().to_lowercase()));
            let table = if dim { &mut self.done_tree } else { &mut self.spots_tree };
            table.add(&self.f, Row { cells, tags, data: Some(data), keys });
        }
        self.spots_tree.sort_rows();
        self.done_tree.sort_rows();
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
            let keys = vec![
                e.time.map_or(SortKey::None, |t| SortKey::Num(t.timestamp() as f64)),
                SortKey::Text(e.call.to_lowercase()),
                SortKey::Num(band_meters(&e.band)),
                SortKey::Text(e.mode.to_lowercase()),
                SortKey::Num(if e.logged { 1.0 } else { 0.0 }),
                SortKey::Text(self.h.park_text(e).to_lowercase()),
            ];
            self.worked_tree.add(&self.f, Row { cells, tags: RowTags { unlogged: !e.logged, ..Default::default() }, data: None, keys });
        }
        self.worked_tree.sort_rows();
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
            let wait = self.poller.as_ref().map_or(0, |p| (p.next_fetch() - epoch_now()) as i64);
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
                Timer::UpdateCheck => {
                    if self.s.update_check && self.update.is_none() {
                        self.check_update(false);
                    }
                    self.after(UPDATE_EVERY, Timer::UpdateCheck);
                }
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
            if (b.label == "Halt Tx" || b.label == "Keep calling") && !on {
                remove.push(b.id); // no QSO in progress: no Halt Tx / Keep calling button
                continue;
            }
            if b.label == "Keep calling" {
                continue; // stays green, never glows
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
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(Key::Comma)) && self.dialog.is_none() && self.wizard.is_none() {
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
        let sx = |v: i32| format!("+{v}");
        Some(format!("{w}x{h}{}{}", sx(x), sx(y)))
    }

    fn save_layout(&mut self, ctx: &egui::Context) {
        let minimized = ctx.input(|i| i.viewport().minimized) == Some(true);
        let total = self.panes.last;
        if !self.panes.placed || minimized || total.y < 300.0 || total.x < 600.0 {
            return; // not laid out (or minimized): the numbers would be junk
        }
        let right_w = (total.x - self.panes.left).round();
        if !(200.0 < right_w && right_w < total.x - 200.0) {
            return;
        }
        let cols = self.panes.cols.iter().map(|c| c.iter().map(|k| k.key().to_string()).collect()).collect();
        let weights = self.panes.weights.iter().map(|(c, w)| (c.key().to_string(), (*w as f64 * 10000.0).round() / 10000.0)).collect();
        self.s.layout = Some(settings::Layout { right_w: Some(right_w as i64), split: None, cols: Some(cols), weights: Some(weights) });
    }

    fn current_sorts(&self) -> std::collections::BTreeMap<String, (String, bool)> {
        [&self.spots_tree, &self.done_tree, &self.worked_tree].iter().map(|t| (t.id.to_string(), t.sort_cfg())).collect()
    }

    fn persist_layout(&mut self, ctx: &egui::Context) {
        self.save_layout(ctx);
        self.save_popouts();
        self.s.sorts = self.current_sorts();
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
        // logo: the app icon
        let lx = PAD + 4.0;
        let ly = center(36.0);
        let logo = self.logo.get_or_insert_with(|| {
            let img = egui::ColorImage::from_rgba_unmultiplied([64, 64], include_bytes!("../assets/icon64.rgba"));
            ui.ctx().load_texture("logo", img, egui::TextureOptions::LINEAR)
        });
        let logo_rect = Rect::from_min_size(Pos2::new(lx, ly), Vec2::splat(36.0));
        painter.image(logo.id(), logo_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
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
        if self.h.enabled {
            self.auto_glow(ui, r);
        }
        let resp = hb.show(ui, Id::unique("hunt-btn"), r, true);
        self.tips.tip(&resp, "Click to turn Auto-Hunt on or off", 500, TipAt::Below(r));
        if resp.clicked() && self.wsjtx_seen {
            self.toggle_hunt();
        }
        right = r.left() - 10.0;
        let cb = Rnd::new(&cq_text, &f.toggle).pill().pad(10.0, 9.0).colors(cfill, Color32::WHITE).hover(chover).min_text(&cq_min);
        let sz = cb.size(ui);
        let r = Rect::from_min_size(Pos2::new(right - sz.x, center(sz.y)), sz);
        if self.h.cq_enabled {
            self.auto_glow(ui, r);
        }
        let resp = cb.show(ui, Id::unique("cq-btn"), r, true);
        self.tips.tip(&resp, "Click to turn Auto CQ/Answer on or off.\nCalls CQ when idle and answers stations that call you;\nAuto-Hunt has priority.", 500, TipAt::Below(r));
        if resp.clicked() && self.wsjtx_seen {
            self.toggle_cq();
        }
        // titles: only what fits left of the buttons
        let th = f.title.m.linespace + 4.0 + f.small.m.linespace + 4.0;
        let ty = center(th);
        let room = r.left() - 12.0 - (tx + 2.0);
        if measure(&f.title, TITLE) <= room {
            ui::text(ui, &painter, Pos2::new(tx + 2.0, ty + 2.0), TITLE, &f.title, p.text);
            // the version and update state, small and in parens after the title
            let ver = env!("CARGO_PKG_VERSION");
            let vx = tx + 2.0 + measure(&f.title, TITLE) + 8.0;
            let vy = ty + 2.0 + f.title.m.linespace - f.small.m.linespace - 1.0;
            let at = Pos2::new(vx, vy);
            let new = self.update.as_ref().map(|u| u.version.clone());
            let status = match (&new, self.update_busy, self.update_ok) {
                (Some(_), true, _) => Some("downloading..."),
                (None, true, _) => Some("checking..."),
                _ => None,
            };
            match (new.filter(|_| !self.update_busy), status) {
                (Some(v), _) => {
                    let head = format!("(v{ver} ");
                    let hw = measure(&f.small, &head);
                    let label = format!("Update now to v{v}");
                    let chip = Rnd::new(&label, &f.small_bold).pill().pad(8.0, 1.0).colors(p.blue, p.blue_text).hover(p.blue_hover);
                    let sz = chip.size(ui);
                    let tail_w = measure(&f.small, ")");
                    if vx + hw + sz.x + 2.0 + tail_w <= room + tx + 2.0 {
                        ui::text(ui, &painter, at, &head, &f.small, p.muted);
                        let r = Rect::from_min_size(Pos2::new(vx + hw, vy + ((f.small.m.linespace - sz.y) / 2.0).floor()), sz);
                        let resp = chip.show(ui, Id::unique("update-chip"), r, true);
                        self.tips.tip(&resp, "Download the new version, install it and restart the app", 300, TipAt::Below(r));
                        ui::text(ui, &painter, Pos2::new(r.right() + 2.0, vy), ")", &f.small, p.muted);
                        if resp.clicked() {
                            self.start_update();
                        }
                    }
                }
                (None, s) => {
                    let text = match s {
                        Some(s) => format!("(v{ver} {s})"),
                        None => format!("(v{ver})"),
                    };
                    let tw = measure(&f.small, &text);
                    if vx + tw <= room + tx + 2.0 {
                        ui::text(ui, &painter, at, &text, &f.small, p.muted);
                        if !self.update_busy {
                            // clicking it checks for a new version right now
                            let r = Rect::from_min_size(at, Vec2::new(tw, f.small.m.linespace));
                            let resp = ui.interact(r, Id::unique("update-status"), egui::Sense::click());
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                            }
                            self.tips.tip(&resp, "Click to check for a new version now", 300, TipAt::Below(r));
                            if resp.clicked() {
                                self.check_update(true);
                            }
                        }
                    }
                }
            }
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
        let spots_text = self.spots_text.clone();
        let spots = self.chip(&spots_text, &f.ui).dot(Some(if self.spots_failed { p.bad } else { p.accent }));
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
        // kinds: 0 connection, 1 station, 2 band, 3 mode, 4 radio badge, 6 spots
        let mut items: Vec<(u8, Vec2, f32)> = vec![(0, conn.size(ui), 8.0)];
        if connected {
            items.push((1, station.size(ui), 6.0));
            items.push((2, band_dd.size(ui), 6.0));
            items.push((3, mode_dd.size(ui), 8.0));
        }
        if radio.is_some() {
            items.push((4, tx.size(ui), 8.0));
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
        let y0 = top + 10.0;
        let mut row_y = vec![y0; rows];
        for r in 1..rows {
            row_y[r] = row_y[r - 1] + row0 + 6.0;
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
            let resp = ui.interact(r, Id::unique("radio-badge"), Sense::hover());
            // the chip is its own progress bar: the pulsing color fills left to right
            // through the current T/R period over a darker track
            let period = self.cycle_period();
            let k = cycles::wsjtx_progress(epoch_now(), period) as f32;
            let split = (r.left() + (r.width() * k.clamp(0.0, 1.0)).round()).min(r.right());
            let track = Rnd { fill: theme::mix(tx.fill, Color32::BLACK, 0.5), ..tx.clone() };
            let painter = ui.painter().clone();
            track.paint(ui, &painter.with_clip_rect(Rect::from_min_max(Pos2::new(split, r.top()), r.right_bottom())), r, false);
            if split > r.left() {
                tx.paint(ui, &painter.with_clip_rect(Rect::from_min_max(r.left_top(), Pos2::new(split, r.bottom()))), r, false);
            }
            let mode = st.as_ref().map(|s| s.mode.clone()).unwrap_or_default();
            self.tips.tip(&resp, &format!("{mode} cycle: {} s", period_text(period)), 500, TipAt::Below(r));
        }
        let r = place(spots_sz, spot(6).unwrap());
        let resp = spots.show(ui, Id::unique("spots-chip"), r, false);
        self.tips.tip(&resp, "pota.app spots, fetched at most once a minute", 500, TipAt::Below(r));
        row_y[rows - 1] + row0 + 6.0
    }

    fn card(&self, ui: &Ui, rect: Rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, ui::radius(CARD_RADIUS), self.p.surface);
        painter.rect_stroke(rect, ui::radius(CARD_RADIUS), Stroke::new(1.0, self.p.border), StrokeKind::Inside);
    }

    /// A card with a header band in its own color behind the title row (height `hc`);
    /// returns the row's top.
    fn card_head(&mut self, ui: &mut Ui, card: Card, rect: Rect, hc: f32) -> f32 {
        self.card(ui, rect);
        let inner = rect.shrink(1.0);
        let hy = inner.top() + 7.0;
        let band = Rect::from_min_max(inner.min, Pos2::new(inner.right(), hy + hc + 7.0));
        // dragging the band moves the section (not in its own window); the header's
        // buttons and boxes are drawn after this, so they take their own clicks
        if self.in_pop.is_none() {
            let resp = ui.interact(band, Id::unique(("head-drag", card.key())), Sense::drag());
            if resp.drag_started() {
                self.panes.grab = Some(card);
            }
            if resp.hovered() && self.panes.grab.is_none() {
                ui.ctx().set_cursor_icon(CursorIcon::Grab);
            }
        }
        let painter = ui.painter();
        let top = egui::CornerRadius { nw: CARD_RADIUS as u8 - 1, ne: CARD_RADIUS as u8 - 1, sw: 0, se: 0 };
        painter.rect_filled(band, top, self.p.head);
        painter.rect_filled(Rect::from_min_size(Pos2::new(band.left(), band.bottom()), Vec2::new(band.width(), 1.0)), 0.0, self.p.border);
        hy
    }

    /// A card's title (ttk label padding 2) at the head's content origin; returns its right edge.
    fn card_title(&self, ui: &Ui, x: f32, y: f32, h: f32, title: &str) -> f32 {
        let f = &self.f;
        // center the capitals (not the line box, whose descent is deeper than its ascent)
        let cap = f.heading.size() * ui::CAP_HEIGHT;
        let top = y + (h / 2.0 - (f.heading.m.ascent - cap / 2.0)).round();
        let w = ui::text(ui, ui.painter(), Pos2::new(x + 2.0, top), title, &f.heading, self.p.text);
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
        let hy0 = self.card_head(ui, Card::Log, rect, head_content);
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
        let y = hy + head_content + 9.0;
        let body = Rect::from_min_max(Pos2::new(inner.left(), y), inner.max);
        let mut tips = std::mem::take(&mut self.tips);
        let click = self.log.show(ui, body, &p, &f, &mut tips);
        self.tips = tips;
        if self.waiting_for_wsjtx() {
            self.list_overlay(ui, rect, "Waiting for a WSJT-X decode...", true);
        }
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
    /// `build_info`, built once per frame and width (the column layout and the card both ask).
    fn info_lines(&mut self, ui: &Ui, card_w: f32) -> Vec<StaticLine> {
        let key = (ui.ctx().cumulative_pass_nr(), card_w.to_bits());
        if let Some((k, lines)) = &self.info_cache {
            if *k == key {
                return lines.clone();
            }
        }
        let lines = self.build_info(card_w);
        self.info_cache = Some((key, lines.clone()));
        lines
    }


    /// The lines of the Current Info card, oldest first, wrapped for a card `card_w` wide.
    fn build_info(&mut self, card_w: f32) -> Vec<StaticLine> {
        let f = self.f.clone();
        let mut info: Vec<StaticLine> = vec![];
        if let Some(st) = &self.h.status {
            let band = self.h.band().unwrap_or("?");
            let text = format!("{}: {} on {} {} ({:.3} MHz)", logdialog::app_name(), st.de_call, band, st.mode, st.dial_freq as f64 / 1e6);
            info.push(StaticLine::new(self.stamp("wsjtx", &text), text));
        }
        if !self.static_rig.is_empty() {
            let text = format!("CAT: {}", self.static_rig);
            info.push(StaticLine::new(self.stamp("cat", &text), text));
        }
        let logged_in = !self.s.pota_user.is_empty() && !self.s.pota_password.is_empty();
        let counts = self.counts.borrow().clone();
        if !logged_in {
            let text = "pota.app: not signed in. Your login is needed to show which parks are new to you, how many times you've hunted each park, and how many times you've worked each activator.";
            info.push(StaticLine::new(self.stamp("pota-notice", text), text.to_string()));
            info.push(StaticLine { link: true, ..StaticLine::new(String::new(), "Add your pota.app login in Settings".into()) });
        } else if let Some(c) = counts.as_ref() {
            let now = epoch_now();
            let next = match self.stats.as_ref().and_then(|s| s.next_fetch()) {
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
            info.push(StaticLine { bad: self.pota_failed, ..StaticLine::new(ts, format!("pota.app: {text}")) });
            if self.pota_status.starts_with("Sign-in failed") {
                info.push(StaticLine { link: true, ..StaticLine::new(String::new(), "Check your pota.app login in Settings".into()) });
            }
        }
        let avail = (card_w - 2.0 - PAD - (PAD + 2.0 + 10.0 * measure(&f.mono, "0"))).max(60.0);
        if let Some((text, bad)) = self.update_note.clone() {
            info.push(StaticLine { bad, ..StaticLine::new(self.stamp("update-note", &text), text) });
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
        wrap_static(info, &f.ui, avail)
    }

    fn info_row_h(&self) -> f32 {
        self.f.ui.m.linespace + 2.0
    }

    /// Height of the Current Info card: title row, the lines, a little room below.
    fn info_height(&self, info: &[StaticLine]) -> f32 {
        let hc = self.f.heading.m.linespace + 4.0;
        1.0 + 7.0 + hc + 9.0 + self.info_row_h() * info.len() as f32 + 8.0
    }

    /// Its own card above the Activity log: current facts, timestamped like log lines,
    /// updated in place, as tall as its lines need.
    fn draw_info_card(&mut self, ui: &mut Ui, rect: Rect) {
        let f = self.f.clone();
        let inner = rect.shrink(1.0);
        let hc = f.heading.m.linespace + 4.0;
        let hy0 = self.card_head(ui, Card::Info, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        self.card_title(ui, hx, hy, hc, "Current Info");
        self.pop_button(ui, Card::Info, inner, hy, hc);
        let info = self.info_lines(ui, rect.width());
        let (link_clicked, _) = self.static_selection(ui, &info, inner, hy + hc + 9.0);
        if link_clicked {
            use crate::panel::{Page, Tab};
            let now = self.now();
            let mut panel = std::mem::take(&mut self.panel);
            panel.open_at(self, now, Tab::Connect, Page::Pota);
            self.panel = panel;
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
    fn static_selection(&mut self, ui: &mut Ui, info: &[StaticLine], inner: Rect, y0: f32) -> (bool, f32) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let x0 = inner.left() + PAD + 2.0;
        if info.is_empty() {
            self.static_sel = None;
            return (false, 0.0);
        }
        let cw = measure(&f.mono, "0");
        let xt = x0 + 10.0 * cw; // text starts after "HH:MM:SS  "
        let lh = self.info_row_h();
        let area = Rect::from_min_max(Pos2::new(inner.left(), y0), Pos2::new(inner.right(), y0 + lh * info.len() as f32));
        let hit = |pos: Pos2| -> (usize, usize) {
            let i = (((pos.y - y0) / lh).floor().max(0.0) as usize).min(info.len() - 1);
            let pp = ui::positions(&f.ui, &info[i].text);
            let n = info[i].text.chars().count().min(pp.len().saturating_sub(1));
            let rel = pos.x - xt;
            // the character under the pointer (the selection takes in every character it touches)
            let k = (0..n).rev().find(|&c| pp[c] <= rel).unwrap_or(0);
            (i, k)
        };
        let hover_ok = ui::hover_in(ui, area);
        let resp = ui.interact(area, Id::unique("static-select"), egui::Sense::click_and_drag());
        let mut link_clicked = false;
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
                let (a, b) = ui::word_span(&info[i].text, k);
                self.static_sel = if a < b { Some(((i, a), (i, b - 1))) } else { None };
                self.log.clear_selection();
            }
        } else if resp.clicked() {
            self.static_sel = None;
            if let Some(pos) = resp.interact_pointer_pos() {
                let i = hit(pos).0;
                if info[i].link && pos.x < xt + measure(&f.ui, &info[i].text) {
                    link_clicked = true;
                }
            }
        } else if ui.input(|i| i.pointer.any_pressed()) && !hover_ok {
            self.static_sel = None;
        }
        // normalised (start, end); dropped if the lines changed under it
        let range = self.static_sel.and_then(|(a, b)| {
            let (a, b) = if a <= b { (a, (b.0, b.1 + 1)) } else { (b, (a.0, a.1 + 1)) }; // end is exclusive
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
            // timestamp, in the log's color; highlighted when the selection takes in the line start
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
            let color = if line.bad { p.bad } else if line.link { p.link } else { p.text };
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
        }
        (link_clicked, lh * info.len() as f32)
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
        let mut h = 7.0 + f.heading.m.linespace + 4.0 + 9.0 + 4.0; // header, rows, bottom
        let mut wraps = vec![];
        for (band, mode, n, newest) in &self.notices {
            let w = measure(&f.ui, &self.notice_label(*n, band, mode, newest));
            let bw = Self::qsy_button(f, &self.p, "QSY").size(ui).x;
            let wrap = lx + bw + 8.0 + w + 2.0 > width - 1.0 - PAD;
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
        let hy = self.card_head(ui, Card::Notices, rect, hc);
        let hx = inner.left() + PAD + 2.0;
        let total = self.notices.iter().map(|n| n.2).sum::<usize>().to_string();
        let px = self.pill(ui, hx, hy, hc, &total, "new-count");
        self.card_title(ui, px + 6.0, hy, hc, "POTA Spots (New to me) [on other band/mode]");
        self.pop_button(ui, Card::Notices, inner, hy, hc);
        let (wraps, _) = self.notices_layout(ui, rect.width());
        let rowh = f.ui_bold.m.linespace + 2.0;
        let lx = inner.left() + PAD + 2.0;
        let mut top = hy + hc + 9.0;
        let mut retune = None;
        let now = self.now();
        for (i, (band, mode, n, newest)) in self.notices.clone().into_iter().enumerate() {
            let ry = top + 1.0;
            let label = self.notice_label(n, &band, &mode, &newest);
            if self.notice_pulse.get(&(band.clone(), mode.clone())).is_some_and(|u| *u > now) {
                let h = rowh + 2.0 + if wraps.get(i).copied().unwrap_or(false) { rowh + 2.0 } else { 0.0 };
                let fill = theme::mix(p.notice.0, p.notice.1, pulse(now, HEARD_PERIOD) as f32);
                ui.painter().rect_filled(Rect::from_min_size(Pos2::new(inner.left() + 2.0, top), Vec2::new(inner.width() - 4.0, h)), 3.0, fill);
            }
            let b = Self::qsy_button(&f, &p, "QSY");
            let sz = b.size(ui);
            let wrap = wraps.get(i).copied().unwrap_or(false);
            // the button leads the line; on a narrow card the label drops under it
            let r = Rect::from_min_size(Pos2::new(lx, ry + ((rowh - sz.y) / 2.0).floor()), sz);
            let (lab_x, lab_y) = if wrap {
                top += rowh + 2.0;
                (lx, ry + rowh + 2.0)
            } else {
                (lx + sz.x + 8.0, ry)
            };
            let w = ui::text(ui, ui.painter(), Pos2::new(lab_x + 1.0, lab_y + 1.0), &label, &f.ui, p.new);
            let label_r = Rect::from_min_size(Pos2::new(lab_x, lab_y), Vec2::new(w + 2.0, rowh));
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
        let hy0 = self.card_head(ui, Card::Spots, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = self.spots_title.clone();
        let count = self.spots_count.clone();
        let px = self.pill(ui, hx, hy, hc, &count, "spots-count");
        self.card_title(ui, px + 6.0, hy, hc, &title);
        self.pop_button(ui, Card::Spots, inner, hy, hc);
        let top = hy + hc + 9.0;
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

    /// The centered "waiting" / "No spots found" overlay over a spot list.
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
        let start = ((self.now() / 0.03) as f32 * -12.0).rem_euclid(360.0).to_radians();
        let pts: Vec<Pos2> = (0..=40)
            .map(|i| {
                let a = start + (100f32.to_radians()) * i as f32 / 40.0;
                Pos2::new(center.x + r * a.cos(), center.y - r * a.sin())
            })
            .collect();
        painter.add(egui::Shape::line(pts, Stroke::new(7.0, self.p.accent)));
    }

    /// True while no WSJT-X is talking to us: from start-up until its first Status, and
    /// again after it closes (a Close message) or goes quiet for 35 s (user request
    /// 2026-10-29). Informational only: the big spinner sits inside the Activity card and
    /// blocks nothing; the Auto toggles are refused until WSJT-X has been heard once.
    fn waiting_for_wsjtx(&self) -> bool {
        let demo_skip = !self.demo.is_empty() && !self.demo.iter().any(|s| s == "wait");
        !demo_skip && !(self.connected() && self.wsjtx_decoded)
    }

    fn wsjtx_waiting(&mut self, ctx: &egui::Context) {
        if self.h.band().is_some() {
            self.wsjtx_seen = true;
        }
        if !self.last_wsjtx.is_some_and(|t| t.elapsed().as_secs_f64() < 35.0) {
            self.wsjtx_decoded = false; // closed or silent: wait for a decode again
        }
        if self.waiting_for_wsjtx() {
            ctx.request_repaint_after(Duration::from_millis(40));
        }
    }

    fn draw_done_card(&mut self, ui: &mut Ui, rect: Rect) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let inner = rect.shrink(1.0);
        let hc = f.heading.m.linespace + 4.0;
        let hy0 = self.card_head(ui, Card::Done, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = self.done_title.clone();
        let count = self.done_count.clone();
        let px = self.pill(ui, hx, hy, hc, &count, "done-count");
        self.card_title(ui, px + 6.0, hy, hc, &title);
        self.pop_button(ui, Card::Done, inner, hy, hc);
        let table_rect = Rect::from_min_max(Pos2::new(inner.left(), hy + hc + 9.0), inner.max);
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
        let hy0 = self.card_head(ui, Card::Worked, rect, hc);
        let (hx, hy) = (inner.left() + PAD + 2.0, hy0);
        let title = format!("Worked today ({})", Utc::now().format("%Y-%m-%d"));
        let count = self.worked_count.clone();
        let px = self.pill(ui, hx, hy, hc, &count, "worked-count");
        let x = self.card_title(ui, px + 6.0, hy, hc, &title);
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
        let table_rect = Rect::from_min_max(Pos2::new(inner.left(), hy + hc + 9.0), inner.max);
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
            // only the choice that applies: Skip, or Unskip when already skipped
            let row = if d.skipped {
                Item::Row {
                    label: format!("Unskip {name}"),
                    cmd: Some(MenuCmd::Unskip { call: d.call.clone(), ref_: d.ref_.clone() }),
                    subtitle: "Allow calls to them again today".into(),
                }
            } else {
                Item::Row {
                    label: format!("Skip {name}"),
                    cmd: Some(MenuCmd::Skip { call: d.call.clone(), ref_: d.ref_.clone() }),
                    subtitle: "Not today on this band, mode and park; stops Tx if calling them".into(),
                }
            };
            let items = vec![Item::Heading(head), row];
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
    /// The saved section order and heights, or the defaults when they are missing or wrong.
    fn load_arrangement(&mut self, ) {
        let layout = self.s.layout.clone();
        let cols = layout.as_ref().and_then(|l| l.cols.as_ref()).and_then(|cols| {
            let cols: Vec<Vec<Card>> = cols.iter().map(|c| c.iter().filter_map(|k| Card::from_key(k)).collect()).collect();
            let mut all: Vec<Card> = cols.iter().flatten().copied().collect();
            all.sort();
            let ok = cols.len() == 2 && all == Card::ALL.to_vec();
            ok.then(|| [cols[0].clone(), cols[1].clone()])
        });
        if let Some(c) = cols {
            self.panes.cols = c;
        }
        if let Some(w) = layout.as_ref().and_then(|l| l.weights.as_ref()) {
            for (k, v) in w {
                if let (Some(c), true) = (Card::from_key(k), *v > 0.0) {
                    self.panes.weights.insert(c, *v as f32);
                }
            }
        } else if let Some(s) = layout.and_then(|l| l.split).filter(|s| s.len() == 2 && 0.1 < s[0] && s[0] < s[1] && s[1] < 0.95) {
            // the old layout: two divider positions as fractions of the right column
            self.panes.weights.insert(Card::Spots, s[0] as f32);
            self.panes.weights.insert(Card::Done, (s[1] - s[0]) as f32);
            self.panes.weights.insert(Card::Worked, (1.0 - s[1]) as f32);
        }
    }

    /// Stack a column's visible sections: Current Info and the new-parks card take the
    /// height their lines need, the others share the rest by their weights. Returns the
    /// rectangles and the dividers between neighboring shared sections (card above, card
    /// below, divider rectangle, the column's spare height, the sum of the weights).
    #[allow(clippy::type_complexity)]
    fn stack_column(&mut self, ui: &Ui, cards: &[Card], x: f32, w: f32, top: f32, bottom: f32) -> (Vec<(Card, Rect)>, Vec<(Card, Card, Rect, f32, f32)>) {
        let col_h = bottom - top;
        let auto: Vec<Option<f32>> = cards
            .iter()
            .map(|c| match c {
                Card::Info if w > 120.0 => {
                    let info = self.info_lines(ui, w);
                    Some(self.info_height(&info).min(col_h * 0.55))
                }
                Card::Notices => Some(self.notices_layout(ui, w).1.min(col_h * 0.4)),
                _ => None,
            })
            .collect();
        let weight = |s: &Self, c: Card| s.panes.weights.get(&c).copied().unwrap_or(1.0).max(0.05);
        let sum_w: f32 = cards.iter().zip(&auto).filter(|(_, a)| a.is_none()).map(|(c, _)| weight(self, *c)).sum();
        let used: f32 = auto.iter().flatten().sum::<f32>() + 10.0 * cards.len().saturating_sub(1) as f32;
        let spare = (col_h - used).max(0.0);
        let mut y = top;
        let mut rects: Vec<(Card, Rect)> = vec![];
        for (c, a) in cards.iter().zip(&auto) {
            let h = a.unwrap_or_else(|| (spare * weight(self, *c) / sum_w).floor());
            rects.push((*c, Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))));
            y += h + 10.0;
        }
        let mut sashes = vec![];
        for (i, pair) in rects.windows(2).enumerate() {
            if auto[i].is_none() && auto[i + 1].is_none() {
                sashes.push((pair[0].0, pair[1].0, Rect::from_min_size(Pos2::new(x, pair[0].1.bottom()), Vec2::new(w, 10.0)), spare, sum_w));
            }
        }
        (rects, sashes)
    }

    fn draw_panes(&mut self, ui: &mut Ui, area: Rect) {
        const DROP_W: f32 = 140.0;
        let total = area.size();
        if !self.panes.placed && total.x >= 300.0 && total.y >= 300.0 {
            let saved_w = self.s.layout.as_ref().and_then(|l| l.right_w).map(|v| v as f32);
            let right_w = match saved_w {
                Some(v) if 200.0 < v && v < total.x - 200.0 => v,
                _ => self.fit_right_w + 5.0,
            };
            // neither side may start out collapsed, however narrow the window is
            let min_pane = (total.x * 0.3).min(200.0);
            self.panes.left = (total.x - right_w).clamp(min_pane, total.x - min_pane);
            self.load_arrangement();
            self.panes.placed = true;
            self.panes.last = total;
        } else if self.panes.placed && total != self.panes.last {
            // a resize: the columns share the change by their weights (left 3 : right 2)
            let d = total - self.panes.last;
            let min_pane = (total.x * 0.3).min(200.0);
            self.panes.left = (self.panes.left + d.x * 3.0 / 5.0).clamp(min_pane, total.x - min_pane);
            self.panes.last = total;
        }
        // the sections in the main window, per column (popped-out ones and an empty
        // new-parks card are left out)
        let shown = |s: &Self, c: Card| !s.is_popped(c) && !(c == Card::Notices && s.notices.is_empty());
        let vis: [Vec<Card>; 2] = [0, 1].map(|i| self.panes.cols[i].iter().copied().filter(|c| shown(self, *c)).collect());
        let grabbing = self.panes.grab.is_some();
        let (e0, e1) = (vis[0].is_empty(), vis[1].is_empty());
        // while a header is being dragged an empty column shows a strip to drop it on
        let (left_w, rx, rw) = match (e0, e1) {
            (true, true) => (0.0, area.left(), 0.0),
            (true, false) if grabbing => (DROP_W, area.left() + DROP_W + 10.0, total.x - DROP_W - 10.0),
            (true, false) => (0.0, area.left(), total.x),
            (false, true) if grabbing => (total.x - DROP_W - 10.0, area.right() - DROP_W, DROP_W),
            (false, true) => (total.x, area.right() + 10.0, 0.0),
            _ => (self.panes.left.round(), area.left() + self.panes.left.round() + 10.0, total.x - self.panes.left.round() - 10.0),
        };
        let mut placed: Vec<(Card, Rect)> = vec![];
        let mut sashes: Vec<(Card, Card, Rect, f32, f32)> = vec![];
        for (i, (x, w)) in [(area.left(), left_w), (rx, rw)].into_iter().enumerate() {
            if vis[i].is_empty() || w <= 2.0 {
                continue;
            }
            let (r, s) = self.stack_column(ui, &vis[i], x, w, area.top(), area.bottom());
            placed.extend(r);
            sashes.extend(s);
        }
        // dividers: between the columns, and between neighboring shared sections
        if !e0 && !e1 {
            let r = Rect::from_min_size(Pos2::new(area.left() + left_w, area.top()), Vec2::new(10.0, total.y));
            let resp = ui.interact(r, Id::unique("sash-cols"), Sense::drag());
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            }
            if resp.dragged() {
                self.panes.left = (self.panes.left + resp.drag_delta().x).clamp(0.0, total.x - 10.0);
            }
            if resp.drag_stopped() {
                self.layout_due = Some(self.now() + 0.5);
            }
        }
        for (i, (above, below, r, spare, sum_w)) in sashes.into_iter().enumerate() {
            let resp = ui.interact(r, Id::unique(("sash-rows", i)), Sense::drag());
            if resp.hovered() || resp.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
            }
            if resp.dragged() && spare > 1.0 {
                let dw = resp.drag_delta().y * sum_w / spare;
                let (wa, wb) = (self.panes.weights.get(&above).copied().unwrap_or(1.0), self.panes.weights.get(&below).copied().unwrap_or(1.0));
                let min_w = 0.05 * sum_w;
                if wa + dw >= min_w && wb - dw >= min_w {
                    self.panes.weights.insert(above, wa + dw);
                    self.panes.weights.insert(below, wb - dw);
                }
            }
            if resp.drag_stopped() {
                self.layout_due = Some(self.now() + 0.5);
            }
        }
        // each card draws inside its own rectangle (nothing spills out in a narrow window)
        let outer_clip = ui.clip_rect();
        for (card, rect) in &placed {
            if rect.width() > 2.0 && rect.height() > 2.0 {
                self.card_rects.insert(*card, *rect);
                ui.set_clip_rect(outer_clip.intersect(*rect));
                self.draw_card(ui, *card, *rect);
            }
        }
        ui.set_clip_rect(outer_clip);
        if let Some(card) = self.panes.grab {
            self.move_card(ui, card, &vis, &placed, area, rx, [(area.left(), left_w), (rx, rw)]);
        }
    }

    /// A header is being dragged: show where it would land and move it on release.
    #[allow(clippy::too_many_arguments)]
    fn move_card(&mut self, ui: &mut Ui, card: Card, vis: &[Vec<Card>; 2], placed: &[(Card, Rect)], area: Rect, rx: f32, cols_x: [(f32, f32); 2]) {
        let (p, f) = (self.p.clone(), self.f.clone());
        let down = ui.input(|i| i.pointer.primary_down());
        let Some(pos) = ui.input(|i| i.pointer.interact_pos()) else {
            self.panes.grab = None;
            return;
        };
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        let col = if pos.x < rx - 5.0 { 0 } else { 1 };
        let rect_of = |c: Card| placed.iter().find(|(k, _)| *k == c).map(|(_, r)| *r);
        let others: Vec<Card> = vis[col].iter().copied().filter(|c| *c != card).collect();
        let idx = others.iter().filter(|c| rect_of(**c).is_some_and(|r| r.center().y < pos.y)).count();
        // the marker: a line before the card the dragged one would land above, or after the last
        let (cx, cw) = cols_x[col];
        let (mx, mw) = if cw > 2.0 { (cx, cw) } else { (area.left(), 140.0) };
        let y = match (others.get(idx).and_then(|c| rect_of(*c)), others.last().and_then(|c| rect_of(*c))) {
            (Some(r), _) => r.top() - 5.0,
            (None, Some(r)) => r.bottom() + 5.0,
            (None, None) => area.top() + 2.0,
        };
        let painter = ui.ctx().layer_painter(egui::LayerId::new(Order::Tooltip, Id::unique("card-drag")));
        if others.is_empty() {
            painter.rect_stroke(Rect::from_min_size(Pos2::new(mx, area.top()), Vec2::new(mw, area.height())), 0.0, Stroke::new(2.0, p.accent), StrokeKind::Inside);
        } else {
            painter.rect_filled(Rect::from_min_size(Pos2::new(mx, y - 2.0), Vec2::new(mw, 4.0)), 2.0, p.accent);
        }
        let label = card.title();
        let w = measure(&f.heading, label) + 20.0;
        let r = Rect::from_min_size(pos + Vec2::new(12.0, 12.0), Vec2::new(w, f.heading.m.linespace + 12.0));
        painter.rect_filled(r, 4.0, p.surface);
        painter.rect_stroke(r, 4.0, Stroke::new(1.0, p.accent), StrokeKind::Inside);
        ui::text(ui, &painter, r.min + Vec2::new(10.0, 6.0), label, &f.heading, p.text);
        ui.ctx().request_repaint();
        let esc = ui.input(|i| i.key_pressed(Key::Escape));
        if down && !esc {
            return;
        }
        self.panes.grab = None;
        if esc {
            return;
        }
        // drop: take it out of its column and put it in before `others[idx]` (or after the last)
        for c in self.panes.cols.iter_mut() {
            c.retain(|k| *k != card);
        }
        let list = &mut self.panes.cols[col];
        let at = match others.get(idx) {
            Some(next) => list.iter().position(|k| k == next).unwrap_or(list.len()),
            None => others.last().and_then(|last| list.iter().position(|k| k == last)).map(|i| i + 1).unwrap_or(list.len()),
        };
        list.insert(at, card);
        self.layout_due = Some(self.now() + 0.5);
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
                "update" => {
                    let a = |n: &str| crate::update::Asset { name: n.into(), api_url: String::new(), browser_url: String::new() };
                    self.update = Some(crate::update::Release { version: "9.9.9".into(), page: String::new(), exe: a("pota-auto-digimationator.exe"), exe_sha: None });
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
                "cat" => self.dialog = Some(Dialog::new(DialogKind::CatResult(true, vec!["Radio on COM20 (38400 baud, 8 data bits, 1 stop, none handshake)".into(), "NB: OK, was off, set on confirmed, restored off confirmed".into(), "PWR: OK, was 9 W, set 10 W confirmed, restored 9 W confirmed".into()]))),
                "catfail" => self.dialog = Some(Dialog::new(DialogKind::CatResult(false, vec!["NB: OK, was off, set on confirmed, restored off confirmed".into(), "ATT: FAILED, the radio did not answer".into()]))),
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
        if self.wizard.as_ref().is_some_and(|w| !w.is_rerun()) {
            // first run: only the wizard, nothing else has been started
            let now = self.now();
            if now >= self.next_status {
                self.next_status = now + 1.0;
                if self.s.theme == "system" && theme::system_mode() != self.theme_name {
                    self.apply_theme(&ctx, None);
                }
            }
            if !self.demo.is_empty() && now > 3.0 {
                self.run_demo(&ctx);
            }
            // the wizard's Test CAT Control posts its result; nothing else is running yet
            while let Ok(e) = self.rx.try_recv() {
                if let Event::CatTest(..) | Event::PotaCheck(..) = e {
                    self.dispatch(e, &ctx);
                }
            }
            self.wizard_frame(&ctx, ui);
            self.draw_dialog(&ctx, ctx.content_rect());
            // drag over the wizard's text to select it, Ctrl+C copies it
            let select = self.p.select;
            self.tsel.end_frame(&ctx, Color32::from_rgba_unmultiplied(select.r(), select.g(), select.b(), 120));
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
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
        self.wsjtx_waiting(&ctx);
        // the settings panel, popups and dialogs
        let mut panel = std::mem::take(&mut self.panel);
        panel.show(self, &ctx, ui);
        if !self.panel.is_open() {
            self.panel = panel; // (if it was toggled during show, e.g. Ctrl+, keep the newer state)
        }
        if let Some(menu) = if self.popup_in.is_none() { self.popup.take() } else { None } {
            match menu.show(&ctx, ui, &self.p, &self.f) {
                None => self.popup = Some(menu),
                Some(Some(cmd)) => self.menu_cmd(cmd),
                Some(None) => {}
            }
        }
        self.wizard_frame(&ctx, ui); // (a re-run from Settings covers the window)
        self.draw_dialog(&ctx, screen);
        if self.demo_tip {
            let at = Rect::from_min_size(Pos2::new(200.0, 200.0), Vec2::new(300.0, 20.0));
            self.tips.tip_area(Id::unique("demo-tip"), true, "Click to turn Auto CQ/Answer on or off.\nCalls CQ when idle and answers stations that call you;\nYou've hunted K4SWL 2 times", 0, TipAt::Below(at));
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
        if self.layout_due.is_none() && !self.unsaved && self.current_sorts() != self.s.sorts {
            self.layout_due = Some(now + 0.5);
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
            || self.notice_pulse.values().any(|u| *u > now)
            || (!self.waiting.0.is_empty() && self.waiting.1) || self.dialog.is_some()
            || self.log.buttons_mut().any(|b| b.glowing)
            || self.h.enabled || self.h.cq_enabled;
        ctx.request_repaint_after(Duration::from_millis(if busy { 40 } else { 100 }));
    }

    fn on_exit(&mut self) {
        self.closing = true;
        self.listener.lock().unwrap().stop();
        if let Some(p) = &self.poller {
            p.stop();
        }
        if let Some(s) = &self.stats {
            s.stop();
        }
        self.settings_changed();
        self.rig_restore("Closing, putting NB/NR/ATT/power back as they were");
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

