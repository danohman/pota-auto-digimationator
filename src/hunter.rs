// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Decision logic: which decodes to call, dupe tracking, call attempts.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use serde_json::{json, Value};

use crate::cycles;
use crate::pota_spots::{is_digital, Spot, SpotTable};
use crate::wsjtx_udp::{Decode, QsoLogged, Status};

pub const BANDS: [(&str, u64, u64); 12] = [
    ("160m", 1_800_000, 2_000_000),
    ("80m", 3_500_000, 4_000_000),
    ("60m", 5_250_000, 5_450_000),
    ("40m", 7_000_000, 7_300_000),
    ("30m", 10_100_000, 10_150_000),
    ("20m", 14_000_000, 14_350_000),
    ("17m", 18_068_000, 18_168_000),
    ("15m", 21_000_000, 21_450_000),
    ("12m", 24_890_000, 24_990_000),
    ("10m", 28_000_000, 29_700_000),
    ("6m", 50_000_000, 54_000_000),
    ("2m", 144_000_000, 148_000_000),
];

pub fn band_for(freq_hz: u64) -> Option<&'static str> {
    BANDS.iter().find(|(_, lo, hi)| (*lo..=*hi).contains(&freq_hz)).map(|b| b.0)
}

pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

fn has_digit(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit())
}

fn has_upper(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_uppercase())
}

/// Core callsign: "K1ABC/P", "VE3/K1ABC", "<K1ABC>" -> "K1ABC".
pub fn base_call(call: &str) -> String {
    let c = call.trim().trim_matches(|ch| ch == '<' || ch == '>').to_uppercase();
    let parts: Vec<&str> = c.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return c;
    }
    let cores: Vec<&str> = parts.iter().copied().filter(|p| has_digit(p) && has_upper(p)).collect();
    if cores.is_empty() {
        return parts[0].to_string();
    }
    // max() keeps the first of equal lengths
    let mut best = cores[0];
    for p in &cores[1..] {
        if p.chars().count() > best.chars().count() {
            best = p;
        }
    }
    best.to_string()
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

static GRID_RE: OnceLock<Regex> = OnceLock::new(); // also matches RR73
static MODIFIER_RE: OnceLock<Regex> = OnceLock::new(); // CQ POTA, CQ DX, CQ NA, CQ 290
static TAG_RE: OnceLock<Regex> = OnceLock::new(); // WSJT-X AP / low-confidence tags
static CALL_RE: OnceLock<Regex> = OnceLock::new();

fn grid_re() -> &'static Regex {
    re(&GRID_RE, r"^[A-R]{2}\d{2}$")
}

fn modifier_re() -> &'static Regex {
    re(&MODIFIER_RE, r"^(?:[A-Z]{1,4}|\d{3})$")
}

fn tag_re() -> &'static Regex {
    re(&TAG_RE, r"\s+(a\d+|\?)\s*$")
}

/// The message without trailing whitespace and AP tag.
fn untagged(message: &str) -> String {
    tag_re().replace(message.trim_end(), "").into_owned()
}

/// A decoded message for display: WSJT-X pads messages with spaces before the AP
/// tag ("WD4DAN AK7W DM42          a2"); collapse runs of whitespace. Replies must
/// still use the raw message.
pub fn shown(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn looks_like_call(tok: &str) -> bool {
    if tok.is_empty() || tok.starts_with('<') || grid_re().is_match(tok) {
        return false;
    }
    if !re(&CALL_RE, r"^[A-Z0-9/]{3,15}$").is_match(tok) {
        return false;
    }
    let core = base_call(tok);
    has_digit(&core) && has_upper(&core)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cq,
    /// RR73 / 73
    End,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    pub kind: Kind,
    /// the station that sent the message
    pub call: String,
    /// CQ modifier, e.g. POTA
    pub modifier: String,
    /// addressee for non-CQ messages
    pub to: String,
}

pub fn parse_message(message: &str) -> Option<Parsed> {
    let text = untagged(message);
    let upper = text.to_uppercase();
    let toks: Vec<&str> = upper.split_whitespace().collect();
    if toks.len() < 2 {
        return None;
    }
    let p = |kind, call: &str, modifier: &str, to: &str| Parsed {
        kind,
        call: call.to_string(),
        modifier: modifier.to_string(),
        to: to.to_string(),
    };
    if toks[0] == "CQ" {
        if toks.len() >= 3 && modifier_re().is_match(toks[1]) && looks_like_call(toks[2]) {
            return Some(p(Kind::Cq, toks[2], toks[1], ""));
        }
        if looks_like_call(toks[1]) {
            return Some(p(Kind::Cq, toks[1], "", ""));
        }
        return None;
    }
    if !looks_like_call(toks[1]) {
        return None;
    }
    let last = toks.get(2).copied().unwrap_or("");
    let kind = if last == "RR73" || last == "73" { Kind::End } else { Kind::Other };
    Some(p(kind, toks[1], "", toks[0]))
}

/// [WorkedEntry] for QSOs in a WSJT-X ADIF log dated `today` (UTC).
pub fn read_adif_today(path: &std::path::Path, today: NaiveDate) -> Vec<WorkedEntry> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    static EOH: OnceLock<Regex> = OnceLock::new();
    static EOR: OnceLock<Regex> = OnceLock::new();
    static FIELD: OnceLock<Regex> = OnceLock::new();
    if let Some(m) = re(&EOH, r"(?i)<eoh>").find(&text) {
        text = text[m.end()..].to_string();
    }
    let want = today.format("%Y%m%d").to_string();
    let mut out = Vec::new();
    for rec in re(&EOR, r"(?i)<eor>").split(&text) {
        let mut fields: HashMap<String, String> = HashMap::new();
        for c in re(&FIELD, r"<(\w+):(\d+)(?::\w)?>").captures_iter(rec) {
            let end = c.get(0).unwrap().end();
            let n: usize = c[2].parse().unwrap_or(0);
            let value: String = rec[end..].chars().take(n).collect();
            fields.insert(c[1].to_uppercase(), value);
        }
        let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
        if fields.get("QSO_DATE") != Some(&want) || get("CALL").is_empty() {
            continue;
        }
        let mut mode = get("MODE").to_uppercase();
        if mode == "MFSK" && !get("SUBMODE").is_empty() {
            mode = get("SUBMODE").to_uppercase();
        }
        let mut band = get("BAND").to_lowercase();
        if band.is_empty() && !get("FREQ").is_empty() {
            band = match get("FREQ").trim().parse::<f64>() {
                Ok(mhz) => band_for((mhz * 1_000_000.0) as i64 as u64).unwrap_or("").to_string(),
                Err(_) => String::new(),
            };
        }
        let hhmmss: String = format!("{}00", get("TIME_ON")).chars().take(6).collect();
        let t = NaiveDateTime::parse_from_str(&format!("{want}{hhmmss}"), "%Y%m%d%H%M%S")
            .ok()
            .map(|n| Utc.from_utc_datetime(&n));
        let r = if get("SIG").to_uppercase() == "POTA" { get("SIG_INFO") } else { String::new() };
        out.push(WorkedEntry::new(&get("CALL").to_uppercase(), &band, &mode, t, &r, ""));
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub id: u64,
    pub decode: Decode,
    pub call: String,
    pub reason: String,
    pub ref_: String,
    pub park: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    pub call: String,
    pub band: String,
    pub mode: String,
    pub started: f64,
    pub attempts: u32,
    pub answered: bool,
    /// we answered a station that called us (Answer click / late reply)
    pub replying: bool,
    /// ...and it has called us again since, so it is still there
    pub heard: bool,
    /// WSJT-X has Tx on for this target (after seen_dx)
    pub saw_tx: bool,
    /// WSJT-X's DX call has switched to this target
    pub seen_dx: bool,
    pub completed: bool,
    /// park the station was spotted at when we called ("" = unknown)
    pub ref_: String,
    pub park: String,
    /// we replied to its 73/RR73 line rather than a CQ
    pub signoff: bool,
    /// when Tx went off after a one-shot sign-off reply (0 = not idle)
    pub idle_since: f64,
    /// the line we replied to, re-sent for each further call
    pub decode: Option<Decode>,
    /// when we last sent the Reply
    pub replied_at: f64,
    /// when WSJT-X last began transmitting to them
    pub tx_started: f64,
    /// the operator started this call (Call now): it keeps calling with Auto-Hunt disabled
    pub manual: bool,
}

impl Target {
    pub fn new(call: &str, band: &str, mode: &str, started: f64) -> Self {
        Target {
            call: call.to_string(),
            band: band.to_string(),
            mode: mode.to_string(),
            started,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkedEntry {
    pub call: String,
    pub band: String,
    pub mode: String,
    pub time: Option<DateTime<Utc>>,
    pub ref_: String,
    pub park: String,
    /// false: QSO completed on air, WSJT-X hasn't logged it
    pub logged: bool,
    /// park filled in later from spots, for display only
    pub ref_guessed: bool,
}

impl WorkedEntry {
    pub fn new(call: &str, band: &str, mode: &str, time: Option<DateTime<Utc>>, ref_: &str, park: &str) -> Self {
        WorkedEntry {
            call: call.to_string(),
            band: band.to_string(),
            mode: mode.to_string(),
            time,
            ref_: ref_.to_string(),
            park: park.to_string(),
            logged: true,
            ref_guessed: false,
        }
    }

    /// The park this QSO is known to be for, or "" when unknown.
    pub fn dupe_ref(&self) -> &str {
        if self.ref_guessed {
            ""
        } else {
            &self.ref_
        }
    }
}

#[derive(Clone, Debug)]
pub struct AwaitingLog {
    pub call: String,
    pub deadline: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tag {
    Info,
    Cand,
    Call,
    Worked,
    Dupe,
    Summary,
    Skip,
    Error,
    Hunt,
}

/// What a log-line button does when clicked.
#[derive(Clone, Debug)]
pub enum ActionKind {
    Skip { call: String, ref_: String },
    HaltTx,
    CallNow { cand: Candidate, received: f64 },
    Answer { decode: Decode, call: String, received: f64 },
    /// the give-up count restarts at 1 for another round of calls
    KeepCalling { call: String },
}

impl ActionKind {
    /// The station a button is about (None for Halt Tx).
    pub fn call(&self) -> Option<&str> {
        match self {
            ActionKind::Skip { call, .. } | ActionKind::Answer { call, .. } | ActionKind::KeepCalling { call } => Some(call),
            ActionKind::CallNow { cand, .. } => Some(&cand.call),
            ActionKind::HaltTx => None,
        }
    }
}

/// While a button glows red: its QSO is running, or our CQ is out.
#[derive(Clone, Debug)]
pub enum Active {
    QsoRunning(String),
    CqStarted,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub label: &'static str,
    pub kind: ActionKind,
    pub active: Option<Active>,
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub text: String,
    pub tag: Tag,
    pub actions: Vec<Action>,
    /// starts with an orange NEW chip (a park never hunted)
    pub new: bool,
}

/// What the hunter does to the outside world (WSJT-X, the Log QSO window).
pub trait HunterIo {
    /// `keep_tx`: never move Tx (answering a caller to our own CQ); `cq_on`: Auto CQ/Answer is on.
    fn send_reply(&mut self, d: &Decode, keep_tx: bool, cq_on: bool) -> Result<(), String>;
    fn send_halt(&mut self, client_id: &str) -> Result<(), String>;
    /// Can `send_text` start our CQ at all?
    fn can_send_text(&self) -> bool;
    /// WSJT-X starts calling `text` (our CQ).
    fn send_text(&mut self, client_id: &str, text: &str) -> Result<(), String>;
    /// true when OK was pressed on a Log QSO window
    fn press_log_ok(&mut self) -> bool;
    /// Is WSJT-X's Log QSO window open? None: only a QSO we are waiting on counts.
    fn log_window_open(&mut self) -> Option<bool>;
}

pub type SkipKey = (String, String, String, String); // (base call, band, mode, park ref)

fn sign_off(message: &str) -> String {
    let t = untagged(message).to_uppercase();
    let toks: Vec<&str> = t.split_whitespace().collect();
    match toks.get(2) {
        Some(&x) if x == "73" || x == "RR73" => x.to_string(),
        _ => String::new(),
    }
}

/// True for "<to> <from> RR73|RRR|73" (tags stripped).
fn is_final(message: &str, to_call: &str, from_call: &str) -> bool {
    let t = untagged(message).to_uppercase();
    let toks: Vec<&str> = t.split_whitespace().collect();
    toks.len() >= 3
        && ["RR73", "RRR", "73"].contains(&toks[2])
        && base_call(toks[0]) == to_call
        && base_call(toks[1]) == from_call
}

pub struct Hunter {
    target_: Option<Target>,
    pub io: Box<dyn HunterIo>,
    pub clock: Box<dyn Fn() -> f64>,
    pub utcnow: Box<dyn Fn() -> DateTime<Utc>>,
    /// the app: never hunted this park (per pota.app)?
    pub park_is_new: Box<dyn Fn(&str) -> bool>,
    /// log lines waiting for the app to show them
    pub logs: Vec<LogLine>,
    /// set when a wanted candidate is ready to call: any candidate, or only a NEW park when hunting new parks only (the app plays the pop sound)
    pub wanted_ready: bool,
    /// set when the worked list or skips changed (the app saves and redraws)
    pub worked_changed: bool,
    /// set when the operator halted our CQ and the mode switched itself off
    pub cq_stopped: bool,

    log_seen_at: Option<f64>,
    log_presses: u32,
    log_last_try: f64,
    /// Auto CQ/Answer: call CQ when idle, answer whoever calls
    pub cq_enabled: bool,
    /// before our call and grid, e.g. "CQ POTA"
    pub cq_text: String,
    /// our CQ is queued or on the air, nobody engaged yet
    pub cq_started: bool,
    /// Tx was seen on since we sent it
    pub cq_seen_on: bool,
    pub cq_last: f64,
    /// WSJT-X's DX call when our CQ went out
    pub cq_dx0: String,
    /// base call -> clock time of its last message to us
    pub called_us: HashMap<String, f64>,
    pub last_target_call: String,
    /// (decode, call, received) answering us this cycle
    pub callers: Vec<(Decode, String, f64)>,

    pub enabled: bool,
    pub dry_run: bool,
    /// mode -> unanswered calls before giving up
    pub attempts_by_mode: HashMap<String, u32>,
    /// for modes not in attempts_by_mode
    pub default_attempts: u32,
    /// don't auto-call decodes weaker than this (None = no limit)
    pub min_snr: Option<i32>,
    pub auto_log: bool,
    /// auto-hunt only parks pota.app says we never hunted (NEW)
    pub new_parks_only: bool,
    /// also call a station that has just sent 73/RR73 to someone
    pub pounce_signoffs: bool,

    /// a hunter QSO WSJT-X hasn't logged
    pub awaiting_log: Option<AwaitingLog>,
    pub status: Option<Status>,
    pub spots: Option<Arc<SpotTable>>,
    /// today's QSOs (several per call if parks differ)
    pub worked: Vec<WorkedEntry>,
    pub utc_date: NaiveDate,
    /// (base_call, band)
    pub pota_cq_seen: HashSet<(String, String)>,
    /// base_call -> (ref, park)
    pub known_ref: HashMap<String, (String, String)>,
    /// base_call -> clock deadline
    pub cooloff: HashMap<String, f64>,
    /// base_call -> clock deadline: the operator halted Tx on this station; a fresh CQ/73 from it
    /// is offered (Call now / Skip) but never called automatically
    pub halted: HashMap<String, f64>,
    /// the operator skipped these today
    pub skipped: HashSet<SkipKey>,
    /// base_call -> (target we let go of, deadline); see set_target
    pub recent: HashMap<String, (Target, f64)>,
    /// base_call -> clock deadline for answering a late reply
    pub gave_up: HashMap<String, f64>,
    dupe_logged: HashMap<(String, String, String, Vec<String>), f64>,
    pub pending: Vec<Candidate>,
    pub last_decode_at: f64,
    /// QTime of the T/R cycle being decoded
    cycle_time: Option<Option<u32>>,
    /// distinct senders decoded this cycle
    cycle_calls: HashSet<String>,
    /// decodes arrived since the last settle
    pub cycle_dirty: bool,
    cycle_reported: bool,
    next_cand_id: u64,
}

impl Hunter {
    /// wait this long after the last decode of a cycle
    pub const SETTLE_SECONDS: f64 = 1.5;
    /// don't retry a station that didn't answer
    pub const COOLOFF_SECONDS: f64 = 600.0;
    pub const DRY_RUN_COOLOFF: f64 = 120.0;
    /// how often to repeat "already worked" messages
    pub const DUPE_LOG_SECONDS: f64 = 600.0;
    /// WSJT-X must enable Tx this soon after a Reply
    pub const START_TIMEOUT: f64 = 20.0;
    /// after the last one-shot call to a 73, wait this long for an answer
    pub const SIGNOFF_WAIT: f64 = 45.0;
    /// least seconds between re-sent Replies
    pub const RESEND_GAP: f64 = 3.0;
    /// how long to look for the Log QSO window after a QSO
    pub const LOG_WAIT_SECONDS: f64 = 90.0;
    pub const LOG_PRESS_RETRY: f64 = 2.0;
    /// the Log QSO window must have been open this long before OK is pressed
    pub const LOG_OPEN_DELAY: f64 = 1.5;
    pub const LOG_MAX_PRESSES: u32 = 3;
    /// an Answer click on an older caller line is refused
    pub const ANSWER_MAX_AGE: f64 = 300.0;
    /// WSJT-X must start our CQ this soon (a period is up to 15 s)
    pub const CQ_START_TIMEOUT: f64 = 20.0;
    /// least seconds between our CQs, and after a QSO ends
    pub const CQ_GAP: f64 = 3.0;

    pub fn new(io: Box<dyn HunterIo>, clock: Box<dyn Fn() -> f64>, utcnow: Box<dyn Fn() -> DateTime<Utc>>) -> Self {
        let today = utcnow().date_naive();
        Hunter {
            target_: None,
            io,
            clock,
            utcnow,
            park_is_new: Box::new(|_| false),
            logs: Vec::new(),
            wanted_ready: false,
            worked_changed: false,
            cq_stopped: false,
            log_seen_at: None,
            log_presses: 0,
            log_last_try: -1e9,
            cq_enabled: false,
            cq_text: "CQ".into(),
            cq_started: false,
            cq_seen_on: false,
            cq_last: -1e9,
            cq_dx0: String::new(),
            called_us: HashMap::new(),
            last_target_call: String::new(),
            callers: Vec::new(),
            enabled: false,
            dry_run: true,
            attempts_by_mode: HashMap::new(),
            default_attempts: 4,
            min_snr: Some(-20),
            auto_log: true,
            new_parks_only: false,
            pounce_signoffs: true,
            awaiting_log: None,
            status: None,
            spots: None,
            worked: Vec::new(),
            utc_date: today,
            pota_cq_seen: HashSet::new(),
            known_ref: HashMap::new(),
            cooloff: HashMap::new(),
            halted: HashMap::new(),
            skipped: HashSet::new(),
            recent: HashMap::new(),
            gave_up: HashMap::new(),
            dupe_logged: HashMap::new(),
            pending: Vec::new(),
            last_decode_at: 0.0,
            cycle_time: None,
            cycle_calls: HashSet::new(),
            cycle_dirty: false,
            cycle_reported: false,
            next_cand_id: 0,
        }
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    fn log(&mut self, text: impl Into<String>, tag: Tag) {
        self.log_with(text, tag, Vec::new(), false);
    }

    fn log_with(&mut self, text: impl Into<String>, tag: Tag, actions: Vec<Action>, new: bool) {
        self.logs.push(LogLine { text: text.into(), tag, actions, new });
    }

    fn worked_did_change(&mut self) {
        self.worked_changed = true;
    }

    pub fn band(&self) -> Option<&'static str> {
        self.status.as_ref().and_then(|s| band_for(s.dial_freq))
    }

    fn band_s(&self) -> String {
        self.band().unwrap_or("").to_string()
    }

    pub fn mode(&self) -> String {
        self.status.as_ref().map(|s| s.mode.to_uppercase()).unwrap_or_default()
    }

    pub fn my_call(&self) -> String {
        self.status.as_ref().map(|s| base_call(&s.de_call)).unwrap_or_default()
    }

    pub fn target(&self) -> Option<&Target> {
        self.target_.as_ref()
    }

    pub fn target_mut(&mut self) -> Option<&mut Target> {
        self.target_.as_mut()
    }

    /// A target we let go of before the QSO finished is remembered for a while: a
    /// slow station may still answer and WSJT-X's Auto Seq carry on without us.
    pub fn set_target(&mut self, value: Option<Target>) {
        if let Some(old) = self.target_.take() {
            self.last_target_call = old.call.clone();
            if !old.completed {
                let until = self.now() + Self::COOLOFF_SECONDS;
                self.recent.insert(old.call.clone(), (old, until));
            }
        }
        if value.is_some() {
            self.cq_started = false; // a QSO replaces our CQ
        }
        self.target_ = value;
    }

    fn log_window_open(&mut self) -> bool {
        match self.io.log_window_open() {
            Some(open) => open,
            None => self.awaiting_log.is_some(),
        }
    }

    fn reply(&mut self, d: &Decode, keep_tx: bool) -> Result<(), String> {
        let cq_on = self.cq_enabled;
        self.io.send_reply(d, keep_tx, cq_on)
    }

    fn halt(&mut self, id: &str) -> Result<(), String> {
        self.io.send_halt(id)
    }

    /// Tx is in use or a QSO is in progress. Our own CQ with nobody engaged doesn't
    /// count: Auto-Hunt has priority over it.
    pub fn busy(&self) -> bool {
        if self.target_.is_some() {
            return true;
        }
        match &self.status {
            Some(s) => (s.tx_enabled || s.transmitting) && !self.cq_idle(),
            None => false,
        }
    }

    /// Parks spotted on other bands/modes than the current one that we have never
    /// hunted (pota.app counts) and haven't worked today, ignoring skipped spots, in
    /// band order, each with the spot_time of the newest spot at one of those new
    /// parks: [(band, mode, n_parks, newest_spot_time)]. Digital modes only. Empty
    /// until the hunt counts have loaded.
    pub fn new_parks_elsewhere(&self) -> Vec<(String, String, usize, String)> {
        let Some(spots) = &self.spots else { return Vec::new() };
        let worked: HashSet<&str> =
            self.worked.iter().map(|e| e.dupe_ref()).filter(|r| !r.is_empty()).collect();
        let mut found: HashMap<(&str, String), HashSet<&str>> = HashMap::new();
        let mut newest: HashMap<(&str, String), String> = HashMap::new();
        let here_band = self.band();
        let here_mode = self.mode();
        for sp in spots.all() {
            let r = sp.reference.as_str();
            let Some(band) = sp.band else { continue };
            if r.is_empty()
                || !is_digital(&sp.mode)
                || (Some(band) == here_band && sp.mode == here_mode)
                || worked.contains(r)
                || !(self.park_is_new)(r)
                || self.is_skipped(&base_call(&sp.activator), band, &sp.mode, &[r.to_string()])
            {
                continue;
            }
            let key = (band, sp.mode.clone());
            found.entry(key.clone()).or_default().insert(r);
            let n = newest.entry(key).or_default();
            if sp.spot_time > *n {
                *n = sp.spot_time.clone();
            }
        }
        let order = |b: &str| BANDS.iter().position(|x| x.0 == b).unwrap_or(99);
        let mut out: Vec<_> = found
            .into_iter()
            .map(|((b, m), refs)| {
                let t = newest.get(&(b, m.clone())).cloned().unwrap_or_default();
                (b.to_string(), m, refs.len(), t)
            })
            .collect();
        out.sort_by(|a, b| b.3.cmp(&a.3).then_with(|| (order(&a.0), &a.1).cmp(&(order(&b.0), &b.1))));
        out
    }

    /// The parks behind one `new_parks_elsewhere` line, newest spot first, one entry per
    /// park (its newest unskipped spot): the spot itself and whether the park is an ATNO.
    pub fn new_park_spots(&self, band: &str, mode: &str) -> Vec<(Spot, bool)> {
        let Some(spots) = &self.spots else { return Vec::new() };
        let worked: HashSet<&str> =
            self.worked.iter().map(|e| e.dupe_ref()).filter(|r| !r.is_empty()).collect();
        let mut best: HashMap<&str, &Spot> = HashMap::new();
        for sp in spots.all() {
            let r = sp.reference.as_str();
            if r.is_empty()
                || sp.band != Some(band)
                || sp.mode != mode
                || worked.contains(r)
                || !(self.park_is_new)(r)
                || self.is_skipped(&base_call(&sp.activator), band, &sp.mode, &[r.to_string()])
            {
                continue;
            }
            let e = best.entry(r).or_insert(sp);
            if sp.spot_time > e.spot_time {
                *e = sp;
            }
        }
        let mut out: Vec<(Spot, bool)> = best.into_values().map(|s| (s.clone(), s.qualifying == Some(false))).collect();
        out.sort_by(|a, b| b.0.spot_time.cmp(&a.0.spot_time).then_with(|| a.0.reference.cmp(&b.0.reference)));
        out
    }

    /// Why the band or mode must not be changed right now, or None when it is safe.
    pub fn retune_blocker(&mut self) -> Option<String> {
        let Some(s) = self.status.clone() else {
            return Some("WSJT-X is not connected".into());
        };
        if let Some(t) = &self.target_ {
            return Some(format!("a QSO with {} is in progress", t.call));
        }
        if s.tx_enabled || s.transmitting {
            return Some("Tx is on".into());
        }
        if self.cq_started || self.cq_enabled {
            return Some("Auto CQ/Answer is on".into());
        }
        if self.log_window_open() {
            return Some("the Log QSO window is open".into());
        }
        None
    }

    /// Our automatic CQ is what WSJT-X is sending, and nobody has answered it.
    pub fn cq_idle(&self) -> bool {
        let word = self.cq_text.to_uppercase().split_whitespace().next().unwrap_or("").to_string();
        match &self.status {
            Some(s) => {
                self.cq_started
                    && self.target_.is_none()
                    && s.tx_enabled
                    && (s.tx_message.is_empty() || s.tx_message.to_uppercase().starts_with(&word))
            }
            None => false,
        }
    }

    // --- worked list -------------------------------------------------------

    /// Add WorkedEntry items to today's worked list.
    pub fn seed_worked(&mut self, entries: Vec<WorkedEntry>) {
        for e in entries {
            self.add_worked(e);
        }
        self.worked_did_change();
    }

    /// Today's worked list and skips as plain data, for the app's own state file.
    pub fn export_state(&self) -> Value {
        let mut skipped: Vec<Vec<String>> = self
            .skipped
            .iter()
            .map(|(a, b, c, d)| vec![a.clone(), b.clone(), c.clone(), d.clone()])
            .collect();
        skipped.sort();
        let mut cq: Vec<Vec<String>> =
            self.pota_cq_seen.iter().map(|(a, b)| vec![a.clone(), b.clone()]).collect();
        cq.sort();
        json!({
            "date": self.utc_date.format("%Y-%m-%d").to_string(),
            "worked": self.worked.iter().map(|e| json!({
                "call": e.call, "band": e.band, "mode": e.mode,
                "time": e.time.map(iso).unwrap_or_default(),
                "ref": e.ref_, "park": e.park, "logged": e.logged, "ref_guessed": e.ref_guessed,
            })).collect::<Vec<_>>(),
            "skipped": skipped,
            "pota_cq": cq,
        })
    }

    /// Merge a saved state (see export_state) into today's lists. WSJT-X's own log is
    /// the base and misses QSOs that finished on air but weren't logged, and the park
    /// each one was worked at; this fills both in. Ignored if from another day.
    pub fn import_state(&mut self, state: &Value) {
        if state.get("date").and_then(Value::as_str) != Some(&self.utc_date.format("%Y-%m-%d").to_string()) {
            return;
        }
        let empty = Vec::new();
        for d in state.get("worked").and_then(Value::as_array).unwrap_or(&empty) {
            let s = |k: &str| d.get(k).and_then(Value::as_str);
            let (Some(call), Some(band), Some(mode)) = (s("call"), s("band"), s("mode")) else { continue };
            let when = match s("time") {
                Some(t) if !t.is_empty() => match DateTime::parse_from_rfc3339(t) {
                    Ok(t) => Some(t.with_timezone(&Utc)),
                    Err(_) => continue,
                },
                _ => None,
            };
            let mut saved = WorkedEntry::new(call, band, mode, when, s("ref").unwrap_or(""), s("park").unwrap_or(""));
            saved.logged = truthy(d.get("logged"), true);
            saved.ref_guessed = truthy(d.get("ref_guessed"), false);
            let same = self.worked_entries(&saved.call, &saved.band, &saved.mode);
            let sref = saved.dupe_ref().to_string();
            let mut found = same.iter().copied().find(|&i| self.worked[i].dupe_ref() == sref && !sref.is_empty());
            if found.is_none() && !sref.is_empty() {
                // The log has the QSO but not its park: give it the park we confirmed.
                found = same.iter().copied().find(|&i| self.worked[i].dupe_ref().is_empty());
            }
            match found {
                None if same.is_empty() => self.worked.push(saved),
                Some(i) if !sref.is_empty() && self.worked[i].dupe_ref().is_empty() => {
                    let m = &mut self.worked[i];
                    m.ref_ = saved.ref_;
                    m.park = saved.park;
                    m.ref_guessed = false;
                }
                _ => {}
            }
        }
        for k in state.get("pota_cq").and_then(Value::as_array).unwrap_or(&empty) {
            if let Some([a, b]) = strings(k).as_deref() {
                self.pota_cq_seen.insert((a.clone(), b.clone()));
            }
        }
        for k in state.get("skipped").and_then(Value::as_array).unwrap_or(&empty) {
            if let Some([a, b, c, d]) = strings(k).as_deref() {
                self.skipped.insert((a.clone(), b.clone(), c.clone(), d.clone()));
            }
        }
        self.worked_did_change();
    }

    /// The Park cell of a worked entry: the park, else "CQ POTA (Not spotted)" when we
    /// heard them send CQ POTA on that band today, else "Unknown".
    pub fn park_text(&self, e: &WorkedEntry) -> String {
        if !e.ref_.is_empty() || !e.park.is_empty() {
            return format!("{} {}", e.ref_, e.park).trim().to_string();
        }
        if self.pota_cq_seen.contains(&(base_call(&e.call), e.band.clone())) {
            "CQ POTA (Not spotted)".into()
        } else {
            "Unknown".into()
        }
    }

    /// Add a QSO unless the same call/band/mode/park is already listed. Returns its
    /// index and whether it was added.
    fn add_worked(&mut self, entry: WorkedEntry) -> (usize, bool) {
        for i in self.worked_entries(&entry.call, &entry.band, &entry.mode) {
            if self.worked[i].dupe_ref() == entry.dupe_ref() {
                return (i, false);
            }
        }
        self.worked.push(entry);
        (self.worked.len() - 1, true)
    }

    /// New local spot copy; also fills in parks for worked entries that lack one
    /// (shown in the list only; a guessed park doesn't count for dupes).
    pub fn set_spots(&mut self, table: Arc<SpotTable>) {
        let mut changed = false;
        for e in self.worked.iter_mut() {
            if e.ref_.is_empty() {
                let mut spots = table.spots_for(&e.call, Some(&e.band));
                if spots.is_empty() {
                    spots = table.spots_for(&e.call, None);
                }
                if let Some(s) = spots.first() {
                    e.ref_ = s.reference.clone();
                    e.park = s.park_name.clone();
                    e.ref_guessed = true;
                    changed = true;
                }
            }
        }
        self.spots = Some(table);
        if changed {
            self.worked_did_change();
        }
    }

    /// Indexes of today's QSOs with call on band/mode.
    pub fn worked_entries(&self, call: &str, band: &str, mode: &str) -> Vec<usize> {
        let bc = base_call(call);
        self.worked
            .iter()
            .enumerate()
            .filter(|(_, e)| base_call(&e.call) == bc && e.band == band && e.mode == mode)
            .map(|(i, _)| i)
            .collect()
    }

    /// Most recent QSO with call on band/mode today.
    pub fn worked_entry(&self, call: &str, band: &str, mode: &str) -> Option<&WorkedEntry> {
        self.worked_entries(call, band, mode).last().map(|&i| &self.worked[i])
    }

    /// Already worked today on this band and mode, at one of `refs`? A QSO for an
    /// unknown park, or a station whose current park is unknown (`refs` empty),
    /// counts as worked: don't call again until 0000Z.
    pub fn is_worked(&self, call: &str, band: &str, mode: &str, refs: &[String]) -> bool {
        let entries = self.worked_entries(call, band, mode);
        if entries.is_empty() {
            return false;
        }
        if refs.is_empty() {
            return true;
        }
        entries.iter().any(|&i| {
            let r = self.worked[i].dupe_ref();
            r.is_empty() || refs.iter().any(|x| x == r)
        })
    }

    /// Skipped by the operator today on this band and mode, at one of `refs`? A skip
    /// with no park covers every park; with `refs` empty (park unknown) any skip of
    /// the station on this band and mode counts.
    pub fn is_skipped(&self, call: &str, band: &str, mode: &str, refs: &[String]) -> bool {
        let keys: Vec<&SkipKey> =
            self.skipped.iter().filter(|k| k.0 == call && k.1 == band && k.2 == mode).collect();
        if keys.is_empty() {
            return false;
        }
        refs.is_empty() || keys.iter().any(|k| k.3.is_empty() || refs.contains(&k.3))
    }

    /// The operator skipped `call`: never work it again today on this band, mode and
    /// park, and stop transmitting if it is who we are calling. `ref_` None: the
    /// park it is spotted at now.
    pub fn skip(&mut self, call: &str, ref_: Option<&str>) {
        let call = base_call(call);
        let (band, mode) = (self.band_s(), self.mode());
        let ref_ = match ref_ {
            Some(r) => r.to_string(),
            None => self.park_now(&call, Some(&band)).0,
        };
        self.skipped.insert((call.clone(), band.clone(), mode.clone(), ref_.clone()));
        self.pending.retain(|c| c.call != call);
        self.cooloff.remove(&call);
        let st = self.status.clone();
        let calling = self.target_.as_ref().is_some_and(|t| t.call == call);
        let manual = st.as_ref().is_some_and(|s| {
            !s.dx_call.is_empty() && base_call(&s.dx_call) == call && (s.tx_enabled || s.transmitting)
        });
        let mut halted = String::new();
        if calling || manual {
            if calling {
                self.set_target(None);
            }
            let id = st.map(|s| s.id).unwrap_or_default();
            halted = match self.halt(&id) {
                Ok(()) => "; Tx halted".into(),
                Err(e) => format!("; could not halt Tx: {e}"),
            };
        }
        let at = if ref_.is_empty() { String::new() } else { format!(" at {ref_}") };
        self.log(format!("Skipped {call} on {band} {mode}{at} for the rest of the UTC day{halted}"), Tag::Skip);
        self.worked_did_change();
    }

    /// Take back a skip (the station is fair game again today).
    pub fn unskip(&mut self, call: &str, ref_: &str) {
        let call = base_call(call);
        let (band, mode) = (self.band_s(), self.mode());
        let before = self.skipped.len();
        self.skipped.retain(|k| {
            !(k.0 == call && k.1 == band && k.2 == mode && (ref_.is_empty() || k.3.is_empty() || k.3 == ref_))
        });
        if self.skipped.len() != before {
            self.log(format!("No longer skipping {call} on {band} {mode}"), Tag::Info);
            self.worked_did_change();
        }
    }

    fn skip_button(call: &str, ref_: &str) -> Action {
        Action { label: "Skip", kind: ActionKind::Skip { call: call.into(), ref_: ref_.into() }, active: None }
    }

    /// Buttons for a line about a QSO in progress: Halt Tx (stop transmitting, no skip)
    /// and Skip, which both glow red for as long as the QSO is running.
    fn qso_buttons(call: &str, ref_: &str, skip: bool) -> Vec<Action> {
        let running = Some(Active::QsoRunning(call.to_string()));
        let mut b = vec![Action { label: "Halt Tx", kind: ActionKind::HaltTx, active: running.clone() }];
        if skip {
            b.push(Action {
                label: "Skip",
                kind: ActionKind::Skip { call: call.into(), ref_: ref_.into() },
                active: running,
            });
        }
        b
    }

    /// Is a glowing button's QSO (or our CQ) still running?
    pub fn is_active(&self, a: &Active) -> bool {
        match a {
            Active::QsoRunning(call) => self.target_.as_ref().is_some_and(|t| &t.call == call && !t.completed),
            Active::CqStarted => self.cq_started,
        }
    }

    /// Has the station of a QSO-running button already answered us?
    pub fn has_answered(&self, a: &Active) -> bool {
        match a {
            Active::QsoRunning(call) => self.target_.as_ref().is_some_and(|t| &t.call == call && t.answered),
            Active::CqStarted => false,
        }
    }

    /// A log-line button was clicked.
    pub fn perform(&mut self, kind: &ActionKind) {
        match kind {
            ActionKind::Skip { call, ref_ } => self.skip(call, Some(ref_)),
            ActionKind::HaltTx => self.halt_tx(),
            ActionKind::CallNow { cand, received } => self.call_candidate(cand, *received),
            ActionKind::KeepCalling { call } => self.keep_calling(call),
            ActionKind::Answer { decode, call, received } => {
                self.answer_caller(decode, call, *received, "you clicked Answer")
            }
        }
    }

    /// The operator clicked Keep calling: start another round of calls (count back to 1).
    fn keep_calling(&mut self, call: &str) {
        let max = self.max_attempts();
        let Some(t) = self.target_.as_mut().filter(|t| t.call == call && !t.completed) else { return };
        t.attempts = 1;
        t.idle_since = 0.0;
        let call = t.call.clone();
        self.log(format!("Calling {call} again: another round of up to {max} calls"), Tag::Info);
    }

    /// Stop WSJT-X transmitting without skipping the station.
    pub fn halt_tx(&mut self) {
        let Some(id) = self.status.as_ref().map(|s| s.id.clone()) else { return };
        if let Err(e) = self.halt(&id) {
            self.log(format!("Could not halt Tx: {e}"), Tag::Error);
            return;
        }
        // remember who we were calling: its next CQ or 73 is offered again, not ignored
        let call = self.target_.as_ref().map(|t| t.call.clone()).or_else(|| {
            self.status.as_ref().filter(|s| !s.dx_call.is_empty()).map(|s| base_call(&s.dx_call))
        });
        if let Some(call) = call {
            let until = self.now() + Self::COOLOFF_SECONDS;
            self.halted.insert(call, until);
        }
        // A call we started ourselves is dropped here: left as the target, the sign-off
        // resend would turn Enable Tx straight back on once the next Status shows Tx off.
        if self.target_.as_ref().is_some_and(|t| !t.answered) {
            let call = self.target_.as_ref().map(|t| t.call.clone()).unwrap_or_default();
            self.cooloff.insert(call, self.now() + Self::COOLOFF_SECONDS);
            self.set_target(None);
        }
        self.log("Tx halted", Tag::Error);
    }

    /// (ref, park) of the first current spot of call on band (default: ours).
    fn park_now(&self, call: &str, band: Option<&str>) -> (String, String) {
        let band = band.filter(|b| !b.is_empty()).map(str::to_string).or_else(|| self.band().map(str::to_string));
        if let (Some(spots), Some(band)) = (&self.spots, band) {
            if let Some(s) = spots.spots_for(call, Some(&band)).first() {
                return (s.reference.clone(), s.park_name.clone());
            }
        }
        (String::new(), String::new())
    }

    /// Parks `call` is spotted at on `band` right now (local spot copy).
    pub fn current_parks(&self, call: &str, band: Option<&str>) -> Vec<String> {
        match (&self.spots, band) {
            (Some(spots), Some(b)) => spots
                .spots_for(call, Some(b))
                .iter()
                .filter(|s| !s.reference.is_empty())
                .map(|s| s.reference.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn check_new_day(&mut self) {
        let today = (self.utcnow)().date_naive();
        if today != self.utc_date {
            self.utc_date = today;
            self.worked.clear();
            self.skipped.clear();
            self.recent.clear();
            self.pota_cq_seen.clear();
            self.cooloff.clear();
            self.halted.clear();
            self.gave_up.clear();
            self.dupe_logged.clear();
            self.log(format!("0000Z: new UTC day {}, worked list cleared", today.format("%Y-%m-%d")), Tag::Info);
            self.worked_did_change();
        }
    }

    // --- WSJT-X messages ---------------------------------------------------

    pub fn handle_status(&mut self, st: Status) {
        let prev = self.status.replace(st.clone());
        let changed = match &prev {
            None => true,
            Some(p) => band_for(p.dial_freq) != self.band() || p.mode != st.mode,
        };
        if changed {
            self.pending.clear();
            // the app shows the station/band in the Activity log's static area
        }

        if self.cq_enabled && self.target_.is_none() && self.engaged(&st, None) {
            self.adopt(&base_call(&st.dx_call));
        }
        if self.cq_started {
            if st.tx_enabled || st.transmitting {
                self.cq_seen_on = true;
            } else if self.cq_seen_on && self.target_.is_none() {
                // Tx went off during our CQ with nobody engaged: the operator halted it.
                self.cq_started = false;
                if self.cq_enabled {
                    self.cq_enabled = false;
                    self.log("Tx stopped during the CQ; Auto CQ/Answer turned off", Tag::Hunt);
                    self.cq_stopped = true;
                }
            }
        }
        let Some(mut t) = self.target_.take() else { return };
        // Until WSJT-X has switched its DX call to our target, its Status still
        // describes whatever it was doing before (e.g. the end of your own CQ QSO
        // when you click Answer), so don't judge the target by it yet.
        if !st.dx_call.is_empty() && base_call(&st.dx_call) == t.call {
            t.seen_dx = true;
        }
        if t.seen_dx && (st.tx_enabled || st.transmitting) {
            t.saw_tx = true;
        }
        if !t.saw_tx {
            if self.now() - t.started > Self::START_TIMEOUT {
                if t.signoff {
                    // WSJT-X acts on a Reply only for a line it still shows in Band Activity,
                    // and filters like "CQ only" or "Hide dupes" hide sign-off lines.
                    self.log(format!("WSJT-X ignored the call to {}: its 73/RR73 line is probably \
                        hidden by a WSJT-X display filter (CQ only, Hide dupes). Turn the \
                        filter off, or turn off 'Call stations after their 73'.", t.call), Tag::Error);
                } else {
                    self.log(format!("WSJT-X never started calling {}; is 'Accept UDP requests' on?", t.call), Tag::Error);
                }
                self.cooloff.insert(t.call.clone(), self.now() + Self::COOLOFF_SECONDS);
                self.target_ = Some(t);
                self.set_target(None);
                return;
            }
            self.target_ = Some(t);
            return;
        }
        if !st.dx_call.is_empty() && base_call(&st.dx_call) != t.call {
            self.log(format!("Operator changed DX call to {}; letting go of {}", st.dx_call, t.call), Tag::Info);
            self.target_ = Some(t);
            self.set_target(None);
            return;
        }
        // WSJT-X can report "transmitting" before it updates tx_message, so the RR73/73
        // may only appear in a Status after Tx has ended. tx_message is what was last
        // sent, and seen_dx rules out an earlier QSO's message.
        if is_final(&st.tx_message, &t.call, &self.my_call()) {
            self.qso_complete(&mut t);
        }
        let waiting = !t.answered || (t.replying && !t.heard);
        let max = self.max_attempts();
        if st.transmitting && !prev.as_ref().is_some_and(|p| p.transmitting) {
            if waiting && t.attempts >= max {
                self.log(format!("No answer from {} after {} calls; halting Tx", t.call, t.attempts), Tag::Skip);
                let until = self.now() + Self::COOLOFF_SECONDS;
                self.cooloff.insert(t.call.clone(), until);
                self.gave_up.insert(t.call.clone(), until);
                self.target_ = Some(t);
                self.set_target(None);
                if let Err(e) = self.halt(&st.id) {
                    self.log(format!("Could not send Halt Tx: {e}"), Tag::Error);
                }
                return;
            }
            t.attempts += 1;
            t.tx_started = self.now();
            if waiting {
                let verb = if t.replying { "Answering" } else { "Calling" };
                let mut buttons = Self::qso_buttons(&t.call, &t.ref_, true);
                if t.attempts >= max {
                    // the last call of the round: offer another round
                    buttons.push(Action {
                        label: "Keep calling",
                        kind: ActionKind::KeepCalling { call: t.call.clone() },
                        active: Some(Active::QsoRunning(t.call.clone())),
                    });
                }
                self.log_with(format!("{verb} {} ({}/{max})", t.call, t.attempts), Tag::Call, buttons, false);
            }
        }
        if !st.tx_enabled && !st.transmitting {
            if t.signoff && !t.answered {
                // Replying to a station's 73 makes WSJT-X send its call once and then
                // switch Tx off by itself; Auto Seq turns it back on when they answer.
                // So repeat the Reply for each further call, up to max_attempts, then wait
                // a little for an answer.
                let now = self.now();
                let period = cycles::cycle_seconds(&st.mode, st.tr_period, None);
                if t.idle_since == 0.0 && (t.tx_started == 0.0 || now - t.tx_started < 0.6 * period) {
                    // WSJT-X ends its own Tx at the end of the period; one cut short means
                    // the operator halted it, so don't turn it back on.
                    self.cooloff.insert(t.call.clone(), now + Self::COOLOFF_SECONDS);
                    self.halted.insert(t.call.clone(), now + Self::COOLOFF_SECONDS);
                    self.target_ = Some(t);
                    self.set_target(None);
                    return;
                }
                if (self.enabled || t.manual) && t.attempts < max && t.decode.is_some() && now - t.replied_at >= Self::RESEND_GAP {
                    let d = t.decode.clone().unwrap();
                    if let Err(e) = self.reply(&d, false) {
                        self.log(format!("Could not send Reply to WSJT-X: {e}"), Tag::Error);
                        self.target_ = Some(t);
                        self.set_target(None);
                        return;
                    }
                    t.replied_at = now;
                    self.target_ = Some(t);
                    return;
                }
                if t.attempts < max {
                    self.target_ = Some(t); // a Reply was just re-sent; let it take effect
                    return;
                }
                if t.idle_since == 0.0 {
                    t.idle_since = now;
                    let s = if t.attempts != 1 { "s" } else { "" };
                    self.log(format!("Called {} {} time{s}; waiting for an answer", t.call, t.attempts), Tag::Info);
                } else if now - t.idle_since > Self::SIGNOFF_WAIT {
                    self.log(format!("No answer from {} after {} calls", t.call, t.attempts), Tag::Skip);
                    self.gave_up.insert(t.call.clone(), now + Self::COOLOFF_SECONDS);
                    self.cooloff.insert(t.call.clone(), self.now() + Self::COOLOFF_SECONDS);
                    self.target_ = Some(t);
                    self.set_target(None);
                    return;
                }
                self.target_ = Some(t);
                return;
            }
            if !t.answered {
                self.log(format!("Tx disabled; stopped calling {}", t.call), Tag::Info);
            } else if !t.completed {
                self.log(format!("Tx stopped before the QSO with {} completed; it is not marked worked", t.call), Tag::Skip);
            }
            self.target_ = Some(t);
            self.set_target(None);
            return;
        }
        self.target_ = Some(t);
    }

    pub fn handle_decode(&mut self, d: &Decode) {
        if !d.new || self.status.is_none() {
            return;
        }
        self.check_new_day();
        let p = parse_message(&d.message);
        let Some(band) = self.band() else { return };
        if self.cycle_time != Some(d.time_ms) {
            self.cycle_time = Some(d.time_ms);
            self.cycle_calls.clear();
            self.cycle_reported = false;
            self.pending.clear();
            self.callers.clear();
        }
        self.cycle_dirty = true;
        self.last_decode_at = self.now();
        let Some(p) = p else { return };
        let call = base_call(&p.call);
        let me = self.my_call();
        if call != me {
            self.cycle_calls.insert(call.clone());
        }
        if !p.to.is_empty() && base_call(&p.to) == me {
            self.called_us.insert(call.clone(), self.now());
            if self.target_.as_ref().map(|t| t.call != call).unwrap_or(true) {
                self.revive(&call, d);
            }
            if self.target_.as_ref().is_some_and(|t| t.call == call) {
                let mut t = self.target_.take().unwrap();
                if !t.answered {
                    t.answered = true;
                    self.log(format!("{} answered: {}", p.call, shown(&d.message)), Tag::Worked);
                } else if t.replying && !t.heard {
                    t.heard = true;
                    self.log(format!("{} replied: {}", p.call, shown(&d.message)), Tag::Worked);
                }
                if is_final(&d.message, &me, &call) {
                    self.qso_complete(&mut t);
                }
                self.target_ = Some(t);
            } else if self.gave_up.get(&call).copied().unwrap_or(0.0) > self.now()
                || (self.target_.is_none() && self.recent.get(&call).map(|r| r.1).unwrap_or(0.0) > self.now())
            {
                self.answer_late(d, &call);
            } else if self.recent.get(&call).map(|r| r.1).unwrap_or(0.0) > self.now() {
                // We called them, then moved on to someone else (Call now, Skip...).
                let now_call = self.target_.as_ref().map(|t| t.call.clone()).unwrap_or_default();
                let btn = self.answer_button(d, &call);
                self.log_with(format!("{} is answering your earlier call, but you are now working \
                    {now_call}; ignoring  [{}]", p.call, shown(&d.message)), Tag::Skip, btn, false);
            } else if self.cq_enabled && self.engaged(&self.status.clone().unwrap(), Some(&call)) {
                self.adopt(&call); // WSJT-X's Auto Seq already answered them
            } else if !self.wsjtx_handling(&call) {
                let so = sign_off(&d.message);
                if !so.is_empty() {
                    self.log(format!("{} sent you {so}  [{}]", p.call, shown(&d.message)), Tag::Skip);
                } else if self.cq_enabled {
                    let now = self.now();
                    self.callers.push((d.clone(), call.clone(), now)); // chosen when the cycle settles
                } else {
                    let btn = self.answer_button(d, &call);
                    self.log_with(format!("{} is calling you, but you never called them; ignoring  [{}]",
                        p.call, shown(&d.message)), Tag::Skip, btn, false);
                }
            }
            return;
        }
        if call == me {
            return;
        }

        let is_pota_cq = p.kind == Kind::Cq && p.modifier == "POTA";
        if is_pota_cq {
            self.pota_cq_seen.insert((call.clone(), band.to_string()));
        }
        if !matches!(p.kind, Kind::Cq | Kind::End) || d.off_air || d.low_confidence {
            return;
        }
        if p.kind == Kind::End && !self.pounce_signoffs {
            return;
        }

        let spots: Vec<(String, String)> = match &self.spots {
            Some(s) => s.spots_for(&call, Some(band)).iter().map(|s| (s.reference.clone(), s.park_name.clone())).collect(),
            None => Vec::new(),
        };
        if let Some(first) = spots.first() {
            self.known_ref.insert(call.clone(), first.clone());
        }
        let refs: Vec<String> = spots.iter().filter(|s| !s.0.is_empty()).map(|s| s.0.clone()).collect();
        let mut reason = if is_pota_cq {
            "CQ POTA".to_string()
        } else if !spots.is_empty() {
            "spotted on pota.app".to_string()
        } else {
            return;
        };
        // Only a current spot says which park they're at now; known_ref may be an
        // earlier park and is used for display only.
        let (ref_, park) = spots.first().cloned().unwrap_or_default();
        let (shown_ref, shown_park) = if !ref_.is_empty() {
            (ref_.clone(), park.clone())
        } else {
            self.known_ref.get(&call).cloned().unwrap_or_default()
        };
        let now = self.now();
        if self.new_parks_only && !(!ref_.is_empty() && (self.park_is_new)(&ref_)) {
            return; // not a NEW park (or park/hunt counts unknown): not a candidate
        }
        let mode = self.mode();
        if self.is_skipped(&call, band, &mode, &refs) {
            return;
        }
        if self.is_worked(&call, band, &mode, &refs) {
            let key = (call.clone(), band.to_string(), mode.clone(), refs.clone());
            if now - self.dupe_logged.get(&key).copied().unwrap_or(-1e9) > Self::DUPE_LOG_SECONDS {
                self.dupe_logged.insert(key, now);
                let at = if refs.is_empty() { String::new() } else { format!(" at {}", refs.join(", ")) };
                let why = if refs.is_empty() { ", park unknown" } else { "" };
                self.log(format!("{} skipped (worked today on {band} {mode}{at}{why})", p.call), Tag::Dupe);
            }
            return;
        }
        let earlier: Vec<String> = self
            .worked_entries(&call, band, &mode)
            .iter()
            .map(|&i| self.worked[i].dupe_ref().to_string())
            .collect();
        if !earlier.is_empty() {
            reason += &format!(", new park (worked at {} earlier today)", earlier.join(", "));
        }
        // After the operator halted Tx on a station, its next CQ or 73 is offered, not called
        let offer_only = self.halted.get(&call).copied().unwrap_or(0.0) > now && self.target_.as_ref().is_none_or(|t| t.call != call);
        if !offer_only && (self.cooloff.get(&call).copied().unwrap_or(0.0) > now || self.target_.as_ref().is_some_and(|t| t.call == call)) {
            if self.enabled {
                self.call_again(d, &call, &p, &ref_, &park);
            }
            return;
        }
        if self.pending.iter().any(|c| c.call == call) {
            return;
        }
        let where_ = if shown_ref.is_empty() {
            String::new()
        } else {
            format!(" {shown_ref} {shown_park}").trim_end().to_string()
        };
        let new = !shown_ref.is_empty() && (self.park_is_new)(&shown_ref);
        // "KD5QJQ (RR73) (-7 dB) at US-1091 (Lake Dardanelle State Park)"; (RR73) only for
        // sign-offs; only reasons other than a plain spot ("CQ POTA", "new park ...")
        // are spelled out.
        let extra = reason
            .strip_prefix("spotted on pota.app")
            .unwrap_or(&reason)
            .to_string();
        let extra = extra.strip_prefix("CQ POTA").unwrap_or(&extra).trim_start_matches([',', ' ']).to_string();
        let mut at = if !shown_ref.is_empty() && !shown_park.is_empty() {
            format!(" at {shown_ref} ({shown_park})")
        } else if !where_.is_empty() {
            format!(" at {}", where_.trim())
        } else {
            String::new()
        };
        let mut sign = if p.kind == Kind::End {
            format!(" ({})", shown(&d.message).split(' ').next_back().unwrap_or("").to_uppercase())
        } else {
            " (CQ)".to_string()
        };
        if is_pota_cq && shown_ref.is_empty() {
            sign = " (CQ POTA)".into();
            at = " [not spotted]".into();
        }
        let extra = if extra.is_empty() { String::new() } else { format!(", {extra}") };
        let line = format!("{}{sign} ({:+} dB){at}{extra}", p.call, d.snr);
        if let Some(min) = self.min_snr {
            if d.snr < min {
                self.log_with(format!("{line} [{:+} dB is below the {:+} dB threshold; not calling]", d.snr, min), Tag::Skip, Vec::new(), new);
                return;
            }
        }
        self.next_cand_id += 1;
        let cand = Candidate {
            id: self.next_cand_id,
            decode: d.clone(),
            call: call.clone(),
            reason: reason.clone(),
            ref_: ref_.clone(),
            park: park.clone(),
        };
        if !offer_only {
            self.pending.push(cand.clone());
            if new || !self.new_parks_only {
                self.wanted_ready = true;
            }
        }
        // The operator can always call a candidate right now, switching from any target.
        let received = self.now();
        let mut actions = vec![Action { label: "Call now", kind: ActionKind::CallNow { cand, received }, active: None }];
        actions.push(Self::skip_button(&call, &ref_));
        self.log_with(line, Tag::Cand, actions, new);
    }

    /// True when WSJT-X itself will deal with `call` calling us: the operator is
    /// already working them, or Auto Seq answers callers to our own CQ.
    fn wsjtx_handling(&self, call: &str) -> bool {
        let Some(s) = &self.status else { return false };
        if !s.tx_enabled {
            return false;
        }
        if !s.dx_call.is_empty() && base_call(&s.dx_call) == call {
            return true;
        }
        if self.cq_idle() {
            return false; // our own CQ: WSJT-X won't answer for us
        }
        s.tx_message.trim().to_uppercase().starts_with("CQ")
    }

    /// An Answer button for a log line about a caller we're not answering, or none for a
    /// sign-off (73 / RR73), which needs no reply.
    fn answer_button(&self, d: &Decode, call: &str) -> Vec<Action> {
        if !sign_off(&d.message).is_empty() {
            return Vec::new();
        }
        vec![Action {
            label: "Answer",
            kind: ActionKind::Answer { decode: d.clone(), call: call.to_string(), received: self.now() },
            active: None,
        }]
    }

    /// The operator clicked Answer on a station calling us. Reply exactly as a
    /// double-click in WSJT-X would, then handle the QSO like a hunter call.
    pub fn answer_caller(&mut self, d: &Decode, call: &str, received: f64, how: &str) {
        let label = shown(&d.message);
        if self.now() - received > Self::ANSWER_MAX_AGE {
            self.log(format!("That call from {call} is too old to answer now  [{label}]"), Tag::Skip);
            return;
        }
        if self.target_.as_ref().is_some_and(|t| t.call == call) {
            return;
        }
        if let Some(t) = &self.target_ {
            let from = t.call.clone();
            self.log(format!("Switching from {from} to {call}"), Tag::Info);
        }
        // they are answering our CQ: Tx stays where it is
        let keep = self.cq_enabled;
        if let Err(e) = self.reply(d, keep) {
            self.log(format!("Could not send Reply to WSJT-X: {e}"), Tag::Error);
            return;
        }
        self.gave_up.remove(call);
        self.cooloff.remove(call);
        let (r, park) = self.park_now(call, None);
        let mut t = Target::new(call, &self.band_s(), &self.mode(), self.now());
        t.answered = true;
        t.replying = true;
        t.ref_ = r.clone();
        t.park = park;
        self.set_target(Some(t));
        self.log_with(format!("Answering {call} ({how})"), Tag::Call, Self::qso_buttons(call, &r, false), false);
    }

    /// The operator clicked Call now on a candidate. Reply like an auto-call, so
    /// attempts count and give up after max_attempts.
    pub fn call_candidate(&mut self, cand: &Candidate, received: f64) {
        let (d, call) = (&cand.decode, cand.call.as_str());
        let label = shown(&d.message);
        if self.now() - received > Self::ANSWER_MAX_AGE {
            self.log(format!("That CQ from {call} is too old to answer now  [{label}]"), Tag::Skip);
            return;
        }
        if self.target_.as_ref().is_some_and(|t| t.call == call) {
            return;
        }
        if let Some(t) = &self.target_ {
            let from = t.call.clone();
            self.log(format!("Switching from {from} to {call}"), Tag::Info);
        }
        if let Err(e) = self.reply(d, false) {
            self.log(format!("Could not send Reply to WSJT-X: {e}"), Tag::Error);
            return;
        }
        self.pending.retain(|c| c.id != cand.id);
        self.cooloff.remove(call);
        self.halted.remove(call);
        let mut t = Target::new(call, &self.band_s(), &self.mode(), self.now());
        t.ref_ = cand.ref_.clone();
        t.park = cand.park.clone();
        t.decode = Some(d.clone());
        t.replied_at = self.now();
        t.signoff = !sign_off(&d.message).is_empty();
        t.manual = true;
        self.set_target(Some(t));
        self.log_with(format!("CALLING {call} (you clicked Call now)  [{label}]"), Tag::Call,
                      Self::qso_buttons(call, &cand.ref_, true), false);
    }

    /// A station we are waiting on ("Called X n times; waiting for an answer") or gave up
    /// on has sent a fresh CQ or 73/RR73: a new chance, so start a new round of calls
    /// (count back to 0) with a Reply to this line. Not after the operator halted Tx
    /// (that only sets the cool-off, not `gave_up`).
    fn call_again(&mut self, d: &Decode, call: &str, p: &Parsed, ref_: &str, park: &str) {
        if self.min_snr.is_some_and(|min| d.snr < min) {
            return;
        }
        let now = self.now();
        let waiting = self.target_.as_ref().is_some_and(|t| t.call == call && !t.completed && t.idle_since != 0.0);
        let gave_up = self.target_.is_none() && self.gave_up.get(call).copied().unwrap_or(0.0) > now && !self.busy();
        if !waiting && !gave_up {
            return;
        }
        let label = shown(&d.message);
        let again = if p.kind == Kind::Cq { "CQ" } else { "73/RR73" };
        if let Err(e) = self.reply(d, false) {
            self.log(format!("Could not send Reply to WSJT-X: {e}"), Tag::Error);
            return;
        }
        let signoff = !sign_off(&d.message).is_empty();
        if waiting {
            let t = self.target_.as_mut().unwrap();
            t.attempts = 0;
            t.idle_since = 0.0;
            t.decode = Some(d.clone());
            t.replied_at = now;
            t.signoff = signoff;
        } else {
            self.gave_up.remove(call);
            self.cooloff.remove(call);
            let mut t = Target::new(call, &self.band_s(), &self.mode(), now);
            t.ref_ = ref_.to_string();
            t.park = park.to_string();
            t.decode = Some(d.clone());
            t.replied_at = now;
            t.signoff = signoff;
            self.set_target(Some(t));
        }
        self.log_with(format!("CALLING {call} again: sent {again} again  [{label}]"), Tag::Call,
                      Self::qso_buttons(call, ref_, true), false);
    }

    /// A station we let go of is answering us after all (it was just slow) while WSJT-X
    /// is working it, or has finished the QSO: take it back as the target so the QSO is
    /// tracked, marked worked and the Log QSO window gets its OK.
    fn revive(&mut self, call: &str, d: &Decode) {
        let Some(rec) = self.recent.get(call) else { return };
        if rec.1 <= self.now() {
            return;
        }
        let engaged = self.status.as_ref().is_some_and(|st| {
            st.tx_enabled && !st.dx_call.is_empty() && base_call(&st.dx_call) == call
        });
        if !(engaged || is_final(&d.message, &self.my_call(), call)) {
            return;
        }
        let (mut t, _) = self.recent.remove(call).unwrap();
        t.answered = false; // let the normal "answered" handling run
        t.seen_dx = true;
        t.saw_tx = true;
        t.idle_since = 0.0;
        self.gave_up.remove(call);
        self.cooloff.remove(call);
        self.set_target(Some(t));
    }

    /// A station we gave up on has now called us: answer it.
    fn answer_late(&mut self, d: &Decode, call: &str) {
        let label = shown(&d.message);
        let answer = self.answer_button(d, call);
        if !self.enabled {
            self.log_with(format!("{call} answered late, but hunting is stopped; ignoring  [{label}]"), Tag::Skip, answer, false);
            return;
        }
        let (band, mode) = (self.band_s(), self.mode());
        let parks = self.current_parks(call, self.band());
        if self.is_skipped(call, &band, &mode, &parks) {
            return;
        }
        if self.is_worked(call, &band, &mode, &parks) {
            self.log_with(format!("{call} is calling you, already worked on {band} {mode} today; \
                ignoring  [{label}]"), Tag::Skip, answer, false);
            return;
        }
        if self.busy() {
            self.log_with(format!("{call} answered late, but WSJT-X is busy; not replying  [{label}]"), Tag::Skip, answer, false);
            return;
        }
        if self.dry_run {
            self.log(format!("DRY RUN: would answer late reply from {call}  [{label}]"), Tag::Call);
            return;
        }
        if let Err(e) = self.reply(d, false) {
            self.log(format!("Could not send Reply to WSJT-X: {e}"), Tag::Error);
            return;
        }
        self.gave_up.remove(call);
        self.cooloff.remove(call);
        let (r, park) = self.park_now(call, None);
        let mut t = Target::new(call, &band, &mode, self.now());
        t.answered = true;
        t.replying = true;
        t.ref_ = r;
        t.park = park;
        self.set_target(Some(t));
        self.log(format!("LATE ANSWER from {call}, replying  [{label}]"), Tag::Call);
    }

    /// RR73/RRR/73 exchanged with the hunter's target: count it as worked now, whether
    /// or not the operator (or we) confirm WSJT-X's Log QSO window.
    fn qso_complete(&mut self, t: &mut Target) {
        if t.completed {
            return;
        }
        t.completed = true;
        t.answered = true;
        t.heard = true;
        let mut entry = WorkedEntry::new(&t.call, &t.band, &t.mode, Some((self.utcnow)()), &t.ref_, &t.park);
        entry.logged = false;
        if self.add_worked(entry).1 {
            self.worked_did_change();
        }
        let at = if t.ref_.is_empty() { " (park unknown)".to_string() } else { format!(" at {}", t.ref_) };
        self.log(format!("QSO complete with {} on {} {}{at}", t.call, t.band, t.mode), Tag::Worked);
        self.awaiting_log = Some(AwaitingLog { call: t.call.clone(), deadline: self.now() + Self::LOG_WAIT_SECONDS });
    }

    /// Auto-OK: whenever WSJT-X's Log QSO window is open (any QSO, ours or manual), press
    /// OK once it has been up for LOG_OPEN_DELAY. Gives up on a window that won't close
    /// after LOG_MAX_PRESSES presses, until it closes.
    fn tick_log_window(&mut self) {
        let now = self.now();
        if let Some(a) = self.awaiting_log.clone() {
            if now > a.deadline {
                self.awaiting_log = None;
                if self.auto_log {
                    self.log(format!("{} still not logged by WSJT-X; click Log QSO in WSJT-X \
                        to log it (it already counts as worked here)", a.call), Tag::Error);
                }
            }
        }
        if !self.auto_log {
            return;
        }
        if !self.log_window_open() {
            self.log_seen_at = None;
            self.log_presses = 0;
            return;
        }
        let seen = *self.log_seen_at.get_or_insert(now);
        if now - seen < Self::LOG_OPEN_DELAY
            || now - self.log_last_try < Self::LOG_PRESS_RETRY
            || self.log_presses > Self::LOG_MAX_PRESSES
        {
            return;
        }
        self.log_last_try = now;
        if !self.io.press_log_ok() {
            return;
        }
        self.log_presses += 1;
        let who = self.awaiting_log.as_ref().map(|a| format!(" for {}", a.call)).unwrap_or_default();
        if self.log_presses > Self::LOG_MAX_PRESSES {
            self.log(format!("Log QSO window won't close{who}; please click OK yourself"), Tag::Error);
        }
    }

    /// Record a logged QSO. Returns the WorkedEntry (with park info).
    pub fn handle_qso_logged(&mut self, q: &QsoLogged) -> WorkedEntry {
        self.check_new_day();
        let call = base_call(&q.dx_call);
        let band = band_for(q.tx_freq).map(str::to_string).unwrap_or_else(|| self.band_s());
        let mode = if q.mode.is_empty() { self.mode() } else { q.mode.to_uppercase() };
        let when = q.time_off.unwrap_or_else(|| (self.utcnow)());
        // The QSO the hunter just completed (still marked not logged)?
        let found = self
            .worked_entries(&call, &band, &mode)
            .into_iter()
            .rev()
            .find(|&i| !self.worked[i].logged);
        let i = match found {
            Some(i) => i,
            None => {
                let t = self.target_.as_ref().filter(|t| t.call == call);
                let (r, park, guessed) = match t {
                    Some(t) if !t.ref_.is_empty() => (t.ref_.clone(), t.park.clone(), false),
                    _ => {
                        let now = self.park_now(&call, Some(&band));
                        if !now.0.is_empty() {
                            (now.0, now.1, false)
                        } else {
                            let (r, p) = self.known_ref.get(&call).cloned().unwrap_or_default();
                            let g = !r.is_empty();
                            (r, p, g)
                        }
                    }
                };
                let mut e = WorkedEntry::new(&q.dx_call.to_uppercase(), &band, &mode, Some(when), &r, &park);
                e.ref_guessed = guessed;
                self.add_worked(e).0
            }
        };
        self.worked[i].logged = true;
        self.worked[i].time = Some(when);
        let entry = self.worked[i].clone();
        self.gave_up.remove(&call);
        if self.awaiting_log.as_ref().is_some_and(|a| a.call == call) {
            self.awaiting_log = None;
        }
        let park = if entry.ref_.is_empty() { String::new() } else { format!("  {} {}", entry.ref_, entry.park) };
        self.log(format!("Logged {} on {band} {mode}{park}", entry.call), Tag::Worked);
        if self.target_.as_ref().is_some_and(|t| t.call == call) {
            self.set_target(None);
        }
        self.worked_did_change();
        entry
    }

    // --- periodic ----------------------------------------------------------

    /// Unanswered calls before giving up, for the mode WSJT-X is on.
    pub fn max_attempts(&self) -> u32 {
        self.attempts_by_mode.get(&self.mode()).copied().unwrap_or(self.default_attempts)
    }

    /// One number for every mode (clears the per-mode choices).
    pub fn set_max_attempts(&mut self, n: u32) {
        self.attempts_by_mode.clear();
        self.default_attempts = n;
    }

    pub fn tick(&mut self) {
        self.check_new_day();
        self.tick_log_window();
        if self.cycle_dirty && self.now() - self.last_decode_at >= Self::SETTLE_SECONDS {
            self.cycle_dirty = false;
            let callers = std::mem::take(&mut self.callers);
            let mut started = false;
            if !self.pending.is_empty() {
                let cands = std::mem::take(&mut self.pending);
                self.cycle_reported = true;
                started = self.call_best(cands);
            } else if !callers.is_empty() {
                self.cycle_reported = true;
            } else if !self.cycle_reported {
                // Late decode passes of the same cycle don't repeat this line.
                self.cycle_reported = true;
                let n = self.cycle_calls.len();
                let s = if n != 1 { "s" } else { "" };
                self.log(format!("{n} call{s} decoded, no needed POTA"), Tag::Summary);
            }
            if !callers.is_empty() && !started {
                self.answer_best(callers); // Auto-Hunt has priority over answering callers
            }
        }
        self.maybe_cq();
    }

    /// Call the strongest candidate. True when a call went out (or would have, in Dry
    /// run), so lower-priority work waits.
    fn call_best(&mut self, cands: Vec<Candidate>) -> bool {
        if !self.enabled {
            return false;
        }
        // max() keeps the first of equal values
        let mut best = &cands[0];
        for c in &cands[1..] {
            if c.decode.snr > best.decode.snr {
                best = c;
            }
        }
        let best = best.clone();
        let label = format!("{}  ({:+} dB at {} Hz, {})", best.call, best.decode.snr, best.decode.df, best.reason);
        if self.busy() {
            self.log(format!("Busy (in QSO); not calling {}", best.call), Tag::Skip);
            return false;
        }
        if self.dry_run {
            self.log_with(format!("DRY RUN: would call {label}"), Tag::Call, vec![Self::skip_button(&best.call, &best.ref_)], false);
            self.cooloff.insert(best.call.clone(), self.now() + Self::DRY_RUN_COOLOFF);
            return true;
        }
        if let Err(e) = self.reply(&best.decode, false) {
            self.log(format!("Could not send Reply to WSJT-X: {e}"), Tag::Error);
            return false;
        }
        let mut t = Target::new(&best.call, &self.band_s(), &self.mode(), self.now());
        t.ref_ = best.ref_.clone();
        t.park = best.park.clone();
        t.decode = Some(best.decode.clone());
        t.replied_at = self.now();
        t.signoff = !sign_off(&best.decode.message).is_empty();
        self.set_target(Some(t));
        self.log_with(format!("CALLING {label}"), Tag::Call, Self::qso_buttons(&best.call, &best.ref_, true), false);
        true
    }

    // --- Auto CQ/Answer ----------------------------------------------------

    fn cq_message(&self) -> String {
        let s = self.status.as_ref().unwrap();
        let grid: String = s.de_grid.chars().take(4).collect();
        [self.cq_text.trim(), s.de_call.as_str(), grid.as_str()]
            .iter()
            .filter(|x| !x.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase()
    }

    /// Answer the strongest eligible station that called us this cycle.
    fn answer_best(&mut self, mut callers: Vec<(Decode, String, f64)>) {
        // sorted(reverse=True) is stable: equal SNRs keep their order
        callers.sort_by(|a, b| b.0.snr.cmp(&a.0.snr));
        let (band, mode) = (self.band_s(), self.mode());
        for (d, call, received) in callers {
            let label = shown(&d.message);
            let parks = self.current_parks(&call, self.band());
            if self.is_skipped(&call, &band, &mode, &parks) {
                continue;
            }
            if self.is_worked(&call, &band, &mode, &parks) {
                let b = self.answer_button(&d, &call);
                self.log_with(format!("{call} is calling you, already worked on {band} {mode} today; \
                    ignoring  [{label}]"), Tag::Skip, b, false);
                continue;
            }
            if self.busy() {
                let b = self.answer_button(&d, &call);
                self.log_with(format!("{call} is calling you, but WSJT-X is busy; not answering  [{label}]"), Tag::Skip, b, false);
                return;
            }
            if self.dry_run {
                let b = self.answer_button(&d, &call);
                self.log_with(format!("DRY RUN: would answer {call}  [{label}]"), Tag::Call, b, false);
                return;
            }
            self.answer_caller(&d, &call, received, "Auto CQ/Answer");
            return;
        }
    }

    /// WSJT-X's Auto Seq is working a station that called us (`call`, or anyone when
    /// None) and we are not tracking it. That includes the next caller WSJT-X moves on
    /// to by itself when a QSO ends, with Tx staying on. A DX call left over from an
    /// earlier QSO doesn't count, and neither does a station the operator picked who
    /// never called us.
    fn engaged(&self, st: &Status, call: Option<&str>) -> bool {
        if !(st.tx_enabled && !st.dx_call.is_empty()) {
            return false;
        }
        let c = base_call(&st.dx_call);
        if call.is_some_and(|x| x != c) {
            return false;
        }
        if self.cq_started && st.dx_call == self.cq_dx0 {
            return false; // stale: still the previous QSO's call
        }
        if c == self.last_target_call {
            return false;
        }
        self.called_us.get(&c).copied().unwrap_or(-1e9) > self.now() - 300.0
    }

    /// A station answered our CQ and WSJT-X is already working it. Track the QSO as
    /// ours without sending a Reply, which would restart the exchange.
    fn adopt(&mut self, call: &str) {
        if self.target_.is_some() {
            return;
        }
        let (r, park) = self.park_now(call, None);
        let mut t = Target::new(call, &self.band_s(), &self.mode(), self.now());
        t.answered = true;
        t.replying = true;
        t.heard = true;
        t.seen_dx = true;
        t.saw_tx = true;
        t.ref_ = r.clone();
        t.park = park;
        self.set_target(Some(t));
        self.log_with(format!("Answering {call} (WSJT-X answered your CQ)"), Tag::Call, Self::qso_buttons(call, &r, true), false);
    }

    /// Auto CQ/Answer was turned off: halt our CQ if it is what is on the air. A QSO in
    /// progress (a target) is left alone.
    pub fn stop_cq(&mut self) -> bool {
        let was_out = self.cq_started;
        let idle = self.cq_idle();
        self.cq_started = false;
        let Some(s) = self.status.clone() else { return false };
        if self.target_.is_none() && (was_out || idle) && (s.tx_enabled || s.transmitting) {
            if let Err(e) = self.halt(&s.id) {
                self.log(format!("Could not halt Tx: {e}"), Tag::Error);
                return false;
            }
            return true;
        }
        false
    }

    /// Auto CQ/Answer: when nothing else is going on, send our CQ.
    fn maybe_cq(&mut self) {
        let Some(s) = self.status.clone() else { return };
        if !(self.cq_enabled && self.io.can_send_text() && !s.de_call.is_empty() && self.band().is_some()) {
            return;
        }
        let now = self.now();
        if self.cq_started && !self.cq_seen_on && now - self.cq_last >= Self::CQ_START_TIMEOUT {
            self.cq_started = false; // WSJT-X never started it: back off
            self.cq_last = now + 40.0;
            self.log("WSJT-X did not start the CQ; will try again shortly", Tag::Error);
            return;
        }
        if self.cq_started && !self.cq_seen_on {
            return; // still waiting for Tx to come on
        }
        if self.target_.is_some()
            || s.tx_enabled
            || s.transmitting
            || self.awaiting_log.is_some()
            || self.cycle_dirty
            || !self.pending.is_empty()
            || now - self.cq_last < Self::CQ_GAP
        {
            return;
        }
        let msg = self.cq_message();
        if self.dry_run {
            self.cq_last = now + 45.0; // in Dry run say so once in a while
            self.log(format!("DRY RUN: would call {msg}"), Tag::Call);
            return;
        }
        if let Err(e) = self.io.send_text(&s.id, &msg) {
            self.log(format!("Could not send the CQ to WSJT-X: {e}"), Tag::Error);
            self.cq_last = now + 30.0;
            return;
        }
        self.cq_last = now;
        self.cq_started = true;
        self.cq_seen_on = false;
        self.cq_dx0 = s.dx_call.clone();
        let halt = Action { label: "Halt Tx", kind: ActionKind::HaltTx, active: Some(Active::CqStarted) };
        self.log_with(format!("CALLING {msg}"), Tag::Call, vec![halt], false);
    }
}

/// ISO 8601 form of an aware UTC datetime: "2026-10-24T23:40:00+00:00".
pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false)
}

fn truthy(v: Option<&Value>, default: bool) -> bool {
    match v {
        None => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) => false,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn strings(v: &Value) -> Option<Vec<String>> {
    v.as_array()?.iter().map(|x| x.as_str().map(str::to_string)).collect()
}

#[cfg(test)]
#[path = "hunter_tests.rs"]
mod tests;
