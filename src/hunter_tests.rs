// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Tests for the hunter.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use serde_json::json;

#[derive(Default)]
struct Rec {
    replies: Vec<Decode>,
    keep_tx: Vec<bool>,
    halts: Vec<String>,
    texts: Vec<String>,
    can_text: bool,
    /// results the fake Log QSO presser returns, in order (true = "pressed")
    log_windows: VecDeque<bool>,
    presses: u32,
    window_open: Option<bool>,
}

struct FakeIo(Rc<RefCell<Rec>>);

impl HunterIo for FakeIo {
    fn send_reply(&mut self, d: &Decode, keep_tx: bool, _cq_on: bool) -> Result<(), String> {
        let mut r = self.0.borrow_mut();
        r.replies.push(d.clone());
        r.keep_tx.push(keep_tx);
        Ok(())
    }
    fn send_halt(&mut self, id: &str) -> Result<(), String> {
        self.0.borrow_mut().halts.push(id.to_string());
        Ok(())
    }
    fn can_send_text(&self) -> bool {
        self.0.borrow().can_text
    }
    fn send_text(&mut self, _id: &str, text: &str) -> Result<(), String> {
        self.0.borrow_mut().texts.push(text.to_string());
        Ok(())
    }
    fn press_log_ok(&mut self) -> bool {
        let mut r = self.0.borrow_mut();
        r.presses += 1;
        r.log_windows.pop_front().unwrap_or(false)
    }
    fn log_window_open(&mut self) -> Option<bool> {
        self.0.borrow().window_open
    }
}

fn status(freq: u64, tx_enabled: bool, transmitting: bool, dx_call: &str) -> Status {
    Status {
        id: "WSJT-X".into(),
        dial_freq: freq,
        mode: "FT8".into(),
        dx_call: dx_call.into(),
        tx_enabled,
        transmitting,
        de_call: "WD4DAN".into(),
        ..Default::default()
    }
}

fn st() -> Status {
    status(14_074_000, false, false, "")
}

fn st_tx(tx_enabled: bool, transmitting: bool, dx: &str) -> Status {
    status(14_074_000, tx_enabled, transmitting, dx)
}

fn dec(message: &str) -> Decode {
    dec_at(message, -5, 10_515_000)
}

fn dec_at(message: &str, snr: i32, time_ms: u32) -> Decode {
    Decode {
        id: "WSJT-X".into(),
        new: true,
        time_ms: Some(time_ms),
        snr,
        dt: 0.1,
        df: 1500,
        mode: "~".into(),
        message: message.into(),
        low_confidence: false,
        off_air: false,
    }
}

fn table(raw: serde_json::Value, at: f64) -> Arc<SpotTable> {
    Arc::new(SpotTable::new(raw.as_array().unwrap(), at))
}

struct T {
    h: Hunter,
    rec: Rc<RefCell<Rec>>,
    t: Rc<Cell<f64>>,
    now: Rc<Cell<DateTime<Utc>>>,
    logs: Vec<(Tag, String)>,
    /// Call now / Answer buttons offered in log lines
    actions: Vec<Action>,
    skips: Vec<Action>,
    halt_buttons: Vec<Action>,
}

impl T {
    fn bare() -> T {
        let rec = Rc::new(RefCell::new(Rec::default()));
        let t = Rc::new(Cell::new(1000.0));
        let now = Rc::new(Cell::new(Utc.with_ymd_and_hms(2026, 10, 24, 23, 50, 0).unwrap()));
        let (tc, nc) = (t.clone(), now.clone());
        let h = Hunter::new(Box::new(FakeIo(rec.clone())), Box::new(move || tc.get()), Box::new(move || nc.get()));
        T { h, rec, t, now, logs: vec![], actions: vec![], skips: vec![], halt_buttons: vec![] }
    }

    fn new() -> T {
        let mut s = T::bare();
        s.h.enabled = true;
        s.h.dry_run = false;
        s.h.spots = Some(table(json!([
            {"activator": "N5XYZ/P", "frequency": "14074", "mode": "FT8",
             "reference": "US-1234", "name": "Test State Park"},
            {"activator": "K9OTHER", "frequency": "7074", "mode": "FT8",
             "reference": "US-9999", "name": "Elsewhere"},
        ]), s.t.get()));
        s.hs(st());
        s
    }

    fn drain(&mut self) {
        for l in std::mem::take(&mut self.h.logs) {
            self.logs.push((l.tag, l.text));
            for a in l.actions {
                match a.label {
                    "Skip" => self.skips.push(a),
                    "Halt Tx" => self.halt_buttons.push(a),
                    _ => self.actions.push(a),
                }
            }
        }
    }

    fn hs(&mut self, s: Status) {
        self.h.handle_status(s);
        self.drain();
    }

    fn hd(&mut self, d: Decode) {
        self.h.handle_decode(&d);
        self.drain();
    }

    fn tick(&mut self) {
        self.h.tick();
        self.drain();
    }

    fn adv(&mut self, secs: f64) {
        self.t.set(self.t.get() + secs);
    }

    fn cycle(&mut self, decodes: &[Decode]) {
        for d in decodes {
            self.h.handle_decode(d);
        }
        self.adv(2.0);
        self.h.tick();
        self.drain();
    }

    fn click(&mut self, a: &Action) {
        self.h.perform(&a.kind);
        self.drain();
    }

    fn replies(&self) -> Vec<String> {
        self.rec.borrow().replies.iter().map(|d| d.message.clone()).collect()
    }

    fn n_replies(&self) -> usize {
        self.rec.borrow().replies.len()
    }

    fn halts(&self) -> Vec<String> {
        self.rec.borrow().halts.clone()
    }

    fn logged(&self, needle: &str) -> bool {
        self.logs.iter().any(|(_, t)| t.contains(needle))
    }

    fn target_call(&self) -> Option<String> {
        self.h.target().map(|t| t.call.clone())
    }

    fn labels(&self) -> Vec<&str> {
        self.actions.iter().map(|a| a.label).collect()
    }

    fn spot_n5xyz_at(&mut self, r: &str) {
        let at = self.t.get();
        self.h.set_spots(table(json!([{"activator": "N5XYZ/P", "frequency": "14074", "mode": "FT8",
            "reference": r, "name": format!("Park {r}")}]), at));
    }

    fn cq_setup(&mut self) {
        self.rec.borrow_mut().can_text = true;
        self.h.cq_enabled = true;
        let mut s = st();
        s.de_grid = "EM80HU".into();
        self.hs(s);
    }

    fn cq_on_air(&mut self) {
        self.cq_setup();
        self.h.enabled = false;
        self.tick();
        let mut s = st_tx(true, true, "");
        s.tx_message = "CQ WD4DAN EM80".into();
        self.hs(s);
    }

    fn give_up_on_k1abc(&mut self) {
        self.h.set_max_attempts(1);
        self.cycle(&[dec("CQ POTA K1ABC FN42")]);
        self.hs(st_tx(true, true, "K1ABC"));
        self.hs(st_tx(true, false, "K1ABC"));
        self.hs(st_tx(true, true, "K1ABC"));
        assert_eq!(self.halts(), ["WSJT-X"]);
        self.hs(st_tx(false, false, "K1ABC")); // WSJT-X has halted Tx
        self.rec.borrow_mut().replies.clear();
    }

    fn work_k1abc_until_rr73(&mut self) {
        self.cycle(&[dec("CQ POTA K1ABC FN42")]);
        self.hs(st_tx(true, true, "K1ABC"));
        self.hs(st_tx(true, false, "K1ABC"));
        self.cycle(&[dec_at("WD4DAN K1ABC -07", -5, 10_530_000)]);
        self.hs(st_tx(true, true, "K1ABC"));
        self.hs(st_tx(true, false, "K1ABC"));
        self.cycle(&[dec_at("WD4DAN K1ABC RR73", -5, 10_560_000)]);
    }

    fn summaries(&self) -> Vec<String> {
        self.logs.iter().filter(|(_, t)| t.contains("no needed POTA")).map(|(_, t)| t.clone()).collect()
    }

    fn ignore_logs(&self) -> Vec<String> {
        self.logs.iter().filter(|(_, t)| t.contains("ignoring")).map(|(_, t)| t.clone()).collect()
    }
}

fn refs(r: &[&str]) -> Vec<String> {
    r.iter().map(|s| s.to_string()).collect()
}

// --- parsing -------------------------------------------------------------

#[test]
fn messages() {
    let p = parse_message("CQ POTA K0UWU EM29").unwrap();
    assert_eq!((p.kind, p.call.as_str(), p.modifier.as_str()), (Kind::Cq, "K0UWU", "POTA"));
    let p = parse_message("CQ HA1BF JN86                         a7").unwrap();
    assert_eq!((p.kind, p.call.as_str(), p.modifier.as_str()), (Kind::Cq, "HA1BF", ""));
    assert_eq!(parse_message("CQ DX EI2IXB IO63").unwrap().call, "EI2IXB");
    assert_eq!(parse_message("CQ W1AW/9").unwrap().call, "W1AW/9");
    let p = parse_message("F5MXH VA3GPL RR73").unwrap();
    assert_eq!((p.kind, p.call.as_str(), p.to.as_str()), (Kind::End, "VA3GPL", "F5MXH"));
    assert_eq!(parse_message("KI6BCJ KG9OH 73").unwrap().kind, Kind::End);
    assert_eq!(parse_message("K1ABC KI5YNZ RRR").unwrap().kind, Kind::End);
    assert_eq!(parse_message("TI3NEL N9BTC R-21").unwrap().kind, Kind::Other);
    assert!(parse_message("<W1AW/9> <K9ATX> EN53").is_none());
}

#[test]
fn set_spots_fills_missing_parks() {
    let mut s = T::bare();
    s.h.seed_worked(vec![WorkedEntry::new("N5XYZ", "20m", "FT8", None, "", ""),
                         WorkedEntry::new("W1NONE", "20m", "FT8", None, "", "")]);
    s.h.set_spots(table(json!([{"activator": "N5XYZ/P", "frequency": "14074", "mode": "FT8",
        "reference": "US-1234", "name": "Test Park"}]), 0.0));
    assert_eq!(s.h.worked_entry("N5XYZ", "20m", "FT8").unwrap().ref_, "US-1234");
    assert_eq!(s.h.worked_entry("W1NONE", "20m", "FT8").unwrap().ref_, "");
}

#[test]
fn base_call_and_band() {
    assert_eq!(base_call("K1ABC/P"), "K1ABC");
    assert_eq!(base_call("VE3/K1ABC"), "K1ABC");
    assert_eq!(base_call("W1AW/9"), "W1AW");
    assert_eq!(band_for(7_074_000), Some("40m"));
    assert_eq!(band_for(14_075_500), Some("20m"));
    assert_eq!(band_for(12_000_000), None);
}

// --- hunting -------------------------------------------------------------

#[test]
fn new_parks_on_other_bands_and_modes() {
    let mut s = T::new();
    s.h.spots = Some(table(json!([
        {"activator": "N5XYZ/P", "frequency": "14074", "mode": "FT8", "reference": "US-1234"},
        {"activator": "K9OTHER", "frequency": "7074", "mode": "FT8", "reference": "US-9999"},
        {"activator": "W1AAA", "frequency": "7075", "mode": "FT8", "reference": "US-9999"},
        {"activator": "W2BBB", "frequency": "7076", "mode": "FT8", "reference": "US-5555"},
        {"activator": "W3CCC", "frequency": "14080", "mode": "FT4", "reference": "US-7777"},
        {"activator": "W4DDD", "frequency": "7200", "mode": "SSB", "reference": "US-8888"},
        {"activator": "W5EEE", "frequency": "10136", "mode": "FT8", "reference": "US-4444"},
    ]), s.t.get()));
    s.h.new_parks_only = true;
    s.h.park_is_new = Box::new(|r| r != "US-5555");
    let got: Vec<_> = s.h.new_parks_elsewhere().into_iter().map(|r| (r.0, r.1, r.2)).collect();
    assert_eq!(got, [("40m".into(), "FT8".into(), 1), ("30m".into(), "FT8".into(), 1), ("20m".into(), "FT4".into(), 1)]);
    s.h.worked.push(WorkedEntry::new("W3CCC", "20m", "FT4", None, "US-7777", ""));
    let bands: Vec<_> = s.h.new_parks_elsewhere().into_iter().map(|r| r.0).collect();
    assert_eq!(bands, ["40m", "30m"]);
    s.h.skipped.insert(("W5EEE".into(), "30m".into(), "FT8".into(), "US-4444".into()));
    let bands: Vec<_> = s.h.new_parks_elsewhere().into_iter().map(|r| r.0).collect();
    assert_eq!(bands, ["40m"]);
    s.h.park_is_new = Box::new(|_| false);
    assert!(s.h.new_parks_elsewhere().is_empty());
    // not hunting new parks only: every unworked, unskipped park counts, hunted or not
    s.h.new_parks_only = false;
    let got: Vec<_> = s.h.new_parks_elsewhere().into_iter().map(|r| (r.0, r.2)).collect();
    assert_eq!(got, [("40m".into(), 2)]);
}

#[test]
fn new_parks_report_newest_new_park_spot() {
    let mut s = T::new();
    s.h.spots = Some(table(json!([
        {"activator": "K9OTHER", "frequency": "7074", "mode": "FT8", "reference": "US-9999",
         "spotTime": "2026-10-25T02:40:00"},
        {"activator": "W1AAA", "frequency": "7075", "mode": "FT8", "reference": "US-5555",
         "spotTime": "2026-10-25T02:45:00"},
    ]), s.t.get()));
    s.h.new_parks_only = true;
    s.h.park_is_new = Box::new(|r| r != "US-5555");
    assert_eq!(s.h.new_parks_elsewhere(), [("40m".into(), "FT8".into(), 1, "2026-10-25T02:40:00".into())]);
}

#[test]
fn retune_blocker() {
    let mut s = T::new();
    assert!(s.h.retune_blocker().is_none());
    s.rec.borrow_mut().window_open = Some(true);
    assert!(s.h.retune_blocker().unwrap().contains("Log QSO"));
    s.rec.borrow_mut().window_open = Some(false);
    s.hs(st_tx(true, false, ""));
    assert!(s.h.retune_blocker().unwrap().contains("Tx"));
    s.hs(st());
    s.h.cq_enabled = true;
    assert!(s.h.retune_blocker().unwrap().contains("Auto CQ"));
    s.h.cq_enabled = false;
    s.cycle(&[dec("CQ N5XYZ EM00")]);
    assert!(s.h.retune_blocker().unwrap().contains("N5XYZ"));
}

#[test]
fn signoff_candidates_can_be_turned_off() {
    let mut s = T::new();
    s.h.pounce_signoffs = false;
    s.cycle(&[dec("W1AW N5XYZ RR73")]);
    assert_eq!(s.n_replies(), 0);
    s.cycle(&[dec_at("CQ N5XYZ EM00", -5, 10_530_000)]);
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn ignored_signoff_reply_explains_the_filter() {
    let mut s = T::new();
    s.cycle(&[dec("W1AW N5XYZ RR73")]);
    assert_eq!(s.n_replies(), 1);
    s.adv(Hunter::START_TIMEOUT + 1.0);
    s.hs(st()); // WSJT-X never moves its DX call or enables Tx
    assert!(s.h.target().is_none());
    assert!(s.logs.iter().any(|(tag, t)| t.contains("filter") && *tag == Tag::Error));
    assert!(s.h.cooloff["N5XYZ"] > s.t.get());
}

#[test]
fn skip_stops_tx_and_blocks_the_station_for_the_day() {
    let mut s = T::new();
    s.cycle(&[dec("CQ N5XYZ EM00")]);
    assert_eq!(s.n_replies(), 1);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
    assert!(s.logs.iter().any(|(tag, _)| *tag == Tag::Call));
    let skip = s.skips.last().unwrap().clone(); // the button on the CALLING line
    assert_eq!(skip.label, "Skip");
    s.click(&skip);
    assert_eq!(s.halts(), ["WSJT-X"]);
    assert!(s.h.target().is_none());
    assert!(s.h.is_skipped("N5XYZ", "20m", "FT8", &refs(&["US-1234"])));
    assert!(!s.h.is_skipped("N5XYZ", "20m", "FT8", &refs(&["US-0000"])));
    assert!(!s.h.is_skipped("N5XYZ", "40m", "FT8", &refs(&["US-1234"])));
    assert!(!s.h.is_skipped("N5XYZ", "20m", "FT4", &refs(&["US-1234"])));
    s.cycle(&[dec_at("CQ N5XYZ EM00", -5, 10_530_000)]);
    assert_eq!(s.n_replies(), 1);
    s.now.set(s.now.get() + chrono::Duration::minutes(15)); // past 0000Z
    s.tick();
    assert!(!s.h.is_skipped("N5XYZ", "20m", "FT8", &refs(&["US-1234"])));
}

#[test]
fn skip_a_candidate_without_calling_anyone() {
    let mut s = T::new();
    s.h.enabled = false;
    s.cycle(&[dec("CQ N5XYZ EM00")]);
    assert_eq!(s.labels(), ["Call now"]);
    let skip = s.skips.last().unwrap().clone();
    s.click(&skip);
    assert!(s.halts().is_empty());
    assert_eq!(s.n_replies(), 0);
    s.h.enabled = true;
    s.cycle(&[dec_at("CQ N5XYZ EM00", -5, 10_530_000)]);
    assert_eq!(s.n_replies(), 0);
}

#[test]
fn park_text_cq_pota_or_unknown() {
    let mut s = T::new();
    let when = Some(s.now.get());
    let a = WorkedEntry::new("K1ABC", "20m", "FT8", when, "", "");
    let b = WorkedEntry::new("W2DEF", "20m", "FT8", when, "", "");
    let c = WorkedEntry::new("N5XYZ", "20m", "FT8", when, "US-1234", "Test State Park");
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    assert_eq!(s.h.park_text(&a), "CQ POTA (Not spotted)");
    assert_eq!(s.h.park_text(&b), "Unknown");
    assert_eq!(s.h.park_text(&c), "US-1234 Test State Park");
    let mut other = T::bare();
    other.h.utc_date = s.h.utc_date;
    other.h.import_state(&s.h.export_state()); // survives a restart
    assert_eq!(other.h.park_text(&a), "CQ POTA (Not spotted)");
}

// --- Auto CQ/Answer --------------------------------------------------------

#[test]
fn cq_is_sent_when_idle_and_not_when_busy() {
    let mut s = T::new();
    s.cq_setup();
    s.tick();
    assert_eq!(s.rec.borrow().texts, ["CQ WD4DAN EM80"]);
    assert_eq!(s.halt_buttons.len(), 1); // the CQ line has Halt Tx
    let hb = s.halt_buttons[0].clone();
    assert!(s.h.is_active(hb.active.as_ref().unwrap()));
    s.h.cq_text = "CQ POTA".into();
    s.h.cq_started = false;
    s.adv(10.0);
    s.tick();
    assert_eq!(s.rec.borrow().texts.last().unwrap(), "CQ POTA WD4DAN EM80");
    let n = s.rec.borrow().texts.len();
    s.adv(10.0);
    s.hs(st_tx(true, false, "")); // Tx is on: nothing more to send
    s.tick();
    assert_eq!(s.rec.borrow().texts.len(), n);
}

#[test]
fn cq_is_not_resent_while_waiting_for_tx_and_backs_off() {
    let mut s = T::new();
    s.cq_setup();
    s.tick();
    for _ in 0..5 {
        s.adv(3.0);
        s.tick();
    }
    assert_eq!(s.rec.borrow().texts.len(), 1);
    s.adv(20.0);
    s.tick();
    assert!(s.logged("did not start the CQ"));
    s.adv(30.0);
    s.tick();
    assert_eq!(s.rec.borrow().texts.len(), 1);
    s.adv(20.0);
    s.tick();
    assert_eq!(s.rec.borrow().texts.len(), 2);
}

#[test]
fn dry_run_cq_sends_nothing() {
    let mut s = T::new();
    s.cq_setup();
    s.h.dry_run = true;
    s.tick();
    assert!(s.rec.borrow().texts.is_empty());
    assert!(s.logged("DRY RUN: would call CQ"));
}

#[test]
fn caller_is_answered_and_the_strongest_wins() {
    let mut s = T::new();
    s.cq_setup();
    s.h.enabled = false;
    s.tick();
    let mut x = st_tx(true, true, "");
    x.tx_message = "CQ WD4DAN EM80".into();
    s.hs(x);
    s.cycle(&[dec_at("WD4DAN K1ABC FN42", -15, 10_515_000), dec_at("WD4DAN W9XYZ EN52", -3, 10_515_000)]);
    assert_eq!(s.replies(), ["WD4DAN W9XYZ EN52"]);
    assert_eq!(s.target_call().as_deref(), Some("W9XYZ"));
    assert_eq!(s.rec.borrow().keep_tx, [true]);
    assert!(s.h.target().unwrap().answered);
    assert!(!s.h.cq_started);
}

#[test]
fn callers_are_only_logged_when_the_mode_is_off() {
    let mut s = T::new();
    s.cq_setup();
    s.h.cq_enabled = false;
    s.cycle(&[dec("WD4DAN K1ABC FN42")]);
    assert_eq!(s.n_replies(), 0);
    assert_eq!(s.labels(), ["Answer"]);
}

#[test]
fn auto_hunt_has_priority_over_cq_and_callers() {
    let mut s = T::new();
    s.cq_setup();
    s.tick();
    let mut x = st_tx(true, true, "");
    x.tx_message = "CQ WD4DAN EM80".into();
    s.hs(x);
    assert!(!s.h.busy());
    s.cycle(&[dec("WD4DAN K1ABC FN42"), dec("CQ N5XYZ EM00")]);
    assert_eq!(s.replies(), ["CQ N5XYZ EM00"]);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
}

#[test]
fn disabling_halts_our_cq_but_not_a_qso() {
    let mut s = T::new();
    s.cq_setup();
    s.tick();
    let mut x = st_tx(true, true, "");
    x.tx_message = "CQ WD4DAN EM80".into();
    s.hs(x);
    s.h.cq_enabled = false;
    assert!(s.h.stop_cq());
    assert_eq!(s.halts(), ["WSJT-X"]);
    s.cq_setup();
    s.rec.borrow_mut().halts.clear();
    let now = s.t.get();
    s.h.set_target(Some(Target::new("N5XYZ", "20m", "FT8", now)));
    s.hs(st_tx(true, true, ""));
    assert!(!s.h.stop_cq());
    assert!(s.halts().is_empty());
}

#[test]
fn station_taken_by_wsjtx_auto_seq_is_tracked_and_logged() {
    let mut s = T::new();
    s.cq_on_air();
    s.hd(dec("WD4DAN K1ABC FN42"));
    s.hs(st_tx(true, true, "K1ABC"));
    assert_eq!(s.target_call().as_deref(), Some("K1ABC"));
    assert_eq!(s.n_replies(), 0);
    assert!(s.logged("Answering K1ABC"));
    assert_eq!(s.skips.len(), 1);
    s.hd(dec_at("WD4DAN K1ABC R-05", -5, 10_545_000));
    s.hd(dec_at("WD4DAN K1ABC 73", -5, 10_560_000));
    assert!(s.h.target().unwrap().completed);
    s.hs(st_tx(false, false, "K1ABC")); // Tx off after 73: not "Halt Tx"
    assert!(s.h.cq_enabled);
}

#[test]
fn next_caller_wsjtx_moves_on_to_is_tracked_too() {
    let mut s = T::new();
    s.cq_on_air();
    s.hd(dec("WD4DAN K1ABC FN42"));
    s.hd(dec("WD4DAN N5XYZ EM00"));
    s.hs(st_tx(true, true, "K1ABC"));
    assert_eq!(s.target_call().as_deref(), Some("K1ABC"));
    s.hd(dec_at("WD4DAN K1ABC 73", -5, 10_560_000));
    assert!(s.h.target().unwrap().completed);
    s.h.set_target(None); // logged
    s.hs(st_tx(true, true, "N5XYZ"));
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
    assert_eq!(s.n_replies(), 0);
}

#[test]
fn stale_dx_call_or_a_station_that_never_called_is_not_adopted() {
    let mut s = T::new();
    s.cq_on_air();
    s.hs(st_tx(true, true, "W9NOCALL"));
    assert!(s.h.target().is_none());
}

#[test]
fn decode_before_status_also_adopts() {
    let mut s = T::new();
    s.cq_on_air();
    s.hs(st_tx(true, true, "K1ABC"));
    s.h.set_target(None);
    s.h.cq_started = true;
    s.hd(dec("WD4DAN K1ABC FN42"));
    assert_eq!(s.target_call().as_deref(), Some("K1ABC"));
    assert_eq!(s.n_replies(), 0);
}

#[test]
fn answer_from_a_station_we_switched_away_from() {
    let mut s = T::new();
    s.cycle(&[dec("CQ N5XYZ EM00")]);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
    s.cycle(&[dec_at("CQ POTA K9OTHER EM00", -5, 10_530_000)]);
    let a = s.actions.last().unwrap().clone();
    assert_eq!(a.label, "Call now");
    s.click(&a);
    assert_eq!(s.target_call().as_deref(), Some("K9OTHER"));
    s.logs.clear();
    s.hd(dec_at("WD4DAN N5XYZ R+17", -5, 10_545_000));
    assert!(s.logged("answering your earlier call"));
    assert!(!s.logged("you never called them"));
}

#[test]
fn qso_lines_get_halt_tx_and_a_skip_that_glows_while_running() {
    let mut s = T::new();
    s.cycle(&[dec("CQ N5XYZ EM00")]);
    assert_eq!(s.halt_buttons.len(), 1);
    let skip = s.skips.last().unwrap().clone();
    assert!(s.h.is_active(skip.active.as_ref().unwrap()));
    let hb = s.halt_buttons.last().unwrap().clone();
    assert!(s.h.is_active(hb.active.as_ref().unwrap()));
    s.click(&hb);
    assert_eq!(s.halts(), ["WSJT-X"]);
    assert!(!s.h.is_skipped("N5XYZ", "20m", "FT8", &refs(&["US-1234"])));
    s.h.set_target(None);
    assert!(!s.h.is_active(skip.active.as_ref().unwrap()));
}

#[test]
fn halting_the_cq_turns_the_mode_off() {
    let mut s = T::new();
    s.cq_setup();
    s.tick();
    let mut x = st_tx(true, false, "");
    x.tx_message = "CQ WD4DAN EM80".into();
    s.hs(x);
    s.hs(st());
    assert!(!s.h.cq_enabled);
    assert!(s.h.cq_stopped);
}

#[test]
fn call_now_switches_from_a_busy_station() {
    let mut s = T::new();
    s.hs(st_tx(true, false, "W1AW"));
    s.cycle(&[dec("CQ N5XYZ EM00")]);
    assert_eq!(s.n_replies(), 0);
    assert_eq!(s.labels(), ["Call now"]);
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    assert_eq!(s.replies(), ["CQ N5XYZ EM00"]);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
    assert!(s.h.pending.is_empty());
}

#[test]
fn skip_halts_a_manual_call_to_that_station() {
    let mut s = T::new();
    s.hs(st_tx(true, false, "N5XYZ"));
    s.h.skip("N5XYZ", None);
    assert_eq!(s.halts(), ["WSJT-X"]);
}

#[test]
fn one_shot_signoff_reply_is_repeated_up_to_the_attempt_limit() {
    let mut s = T::new();
    s.h.set_max_attempts(3);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    for n in 1..=3 {
        assert_eq!(s.n_replies(), n);
        s.hs(st_tx(true, false, "N5XYZ"));
        s.hs(st_tx(true, true, "N5XYZ"));
        s.adv(13.0);
        s.hs(st_tx(false, false, "N5XYZ"));
    }
    assert_eq!(s.n_replies(), 3);
    assert!(s.replies().iter().all(|m| m == "W1AW N5XYZ 73"));
    assert!(s.logged("waiting for an answer"));
    assert!(s.h.target().is_some());
    s.adv(Hunter::SIGNOFF_WAIT + 1.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    assert!(s.h.gave_up["N5XYZ"] > s.t.get());
}

#[test]
fn slow_answer_after_we_gave_up_still_completes_and_logs() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(Hunter::SIGNOFF_WAIT + 1.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    s.hs(st_tx(true, false, "N5XYZ"));
    s.hd(dec_at("WD4DAN N5XYZ -10", -5, 10_600_000));
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
    s.hd(dec_at("WD4DAN N5XYZ 73", -5, 10_660_000));
    assert!(s.h.worked_entry("N5XYZ", "20m", "FT8").is_some());
    assert!(s.h.awaiting_log.is_some());
}

#[test]
fn final_73_from_a_station_we_let_go_of_completes_the_qso() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(Hunter::SIGNOFF_WAIT + 1.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.hd(dec_at("WD4DAN N5XYZ 73", -5, 10_700_000));
    assert!(s.h.awaiting_log.is_some());
    assert!(!s.logged("sent you 73"));
}

#[test]
fn halting_tx_mid_call_is_not_undone() {
    let mut s = T::new();
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(4.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    assert_eq!(s.n_replies(), 1);
    assert!(s.h.target().is_none());
}

#[test]
fn tx_off_after_the_station_answers_someone_else_says_so() {
    let mut s = T::new();
    s.cycle(&[dec("CQ POTA N5XYZ EM10")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hd(dec_at("K9ABC N5XYZ -11", -5, 10_600_000));
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    assert!(s.logged("N5XYZ answered K9ABC; WSJT-X stopped calling"));
    assert!(!s.logged("Tx disabled"));
}

#[test]
fn no_resend_while_hunting_is_disabled() {
    let mut s = T::new();
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.h.enabled = false;
    s.hs(st_tx(false, false, "N5XYZ"));
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn call_now_keeps_calling_while_hunting_is_disabled() {
    let mut s = T::new();
    s.h.enabled = false;
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    assert_eq!(s.n_replies(), 0);
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    assert_eq!(s.n_replies(), 1);
    for n in 2..=3 {
        s.hs(st_tx(true, false, "N5XYZ"));
        s.hs(st_tx(true, true, "N5XYZ"));
        s.adv(13.0);
        s.hs(st_tx(false, false, "N5XYZ"));
        assert_eq!(s.n_replies(), n);
    }
}

#[test]
fn one_shot_signoff_reply_waits_for_the_answer() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    assert_eq!(s.n_replies(), 1);
    s.hs(st_tx(true, false, "N5XYZ"));
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_some());
    assert!(s.logged("waiting for an answer"));
    s.hd(dec_at("WD4DAN N5XYZ -10", -5, 10_530_000));
    assert!(s.h.target().unwrap().answered);
    s.hs(st_tx(true, false, "N5XYZ"));
    assert!(s.h.target().is_some());
}

#[test]
fn other_candidates_wait_while_a_contact_is_in_progress() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.h.spots = Some(table(json!([
        {"activator": "N5XYZ/P", "frequency": "14074", "mode": "FT8", "reference": "US-1234"},
        {"activator": "W2NEW", "frequency": "14074", "mode": "FT8", "reference": "US-5555"},
    ]), s.t.get()));
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.cycle(&[dec_at("CQ W2NEW EM00", 10, 10_530_000)]);
    assert_eq!(s.replies(), ["W1AW N5XYZ 73"]);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
}

#[test]
fn one_shot_signoff_reply_gives_up_without_an_answer() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(Hunter::SIGNOFF_WAIT + 1.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    assert!(s.h.cooloff["N5XYZ"] > s.t.get());
}

#[test]
fn calling_line_names_the_station_not_the_message() {
    let mut s = T::new();
    s.h.spots = Some(table(json!([{"activator": "KB3EOF", "frequency": "14074", "mode": "FT8",
        "reference": "US-1574"}]), s.t.get()));
    s.cycle(&[dec("K2DM KB3EOF 73")]);
    assert!(s.logs.iter().any(|(_, t)| t.starts_with("CALLING KB3EOF  (")));
}

#[test]
fn new_parks_only_calls_only_parks_never_hunted() {
    let mut s = T::new();
    s.h.new_parks_only = true;
    s.h.spots = Some(table(json!([{"activator": "KB3EOF", "frequency": "14074", "mode": "FT8",
        "reference": "US-1574"}]), s.t.get()));
    s.cycle(&[dec("K2DM KB3EOF 73")]);
    assert!(!s.logs.iter().any(|(_, t)| t.starts_with("CALLING")));
    s.h.park_is_new = Box::new(|r| r == "US-9999");
    s.cycle(&[dec_at("K2DM KB3EOF 73", -5, 10_530_000)]);
    assert!(!s.logs.iter().any(|(_, t)| t.starts_with("CALLING")));
    s.h.park_is_new = Box::new(|r| r == "US-1574");
    s.cycle(&[dec_at("K2DM KB3EOF 73", -5, 10_545_000)]);
    assert!(s.logs.iter().any(|(_, t)| t.starts_with("CALLING KB3EOF")));
}

#[test]
fn state_restores_unlogged_qsos_parks_and_skips() {
    let mut s = T::new();
    let now = s.now.get();
    s.h.seed_worked(vec![WorkedEntry::new("N5XYZ", "20m", "FT8", Some(now), "", "")]);
    let saved = json!({
        "date": "2026-10-24",
        "worked": [
            {"call": "N5XYZ", "band": "20m", "mode": "FT8", "time": "2026-10-24T23:40:00+00:00",
             "ref": "US-1234", "park": "Test State Park", "logged": true},
            {"call": "K1NEW", "band": "20m", "mode": "FT8", "time": "2026-10-24T23:45:00+00:00",
             "ref": "US-7", "park": "New Park", "logged": false},
        ],
        "skipped": [["W9SKIP", "20m", "FT8", "US-5"]],
    });
    let mut other_day = saved.clone();
    other_day["date"] = json!("2026-10-23");
    s.h.import_state(&other_day);
    assert!(s.h.worked_entry("K1NEW", "20m", "FT8").is_none());
    s.h.import_state(&saved);
    assert_eq!(s.h.worked_entry("N5XYZ", "20m", "FT8").unwrap().dupe_ref(), "US-1234");
    assert!(!s.h.worked_entry("K1NEW", "20m", "FT8").unwrap().logged);
    assert!(s.h.is_skipped("W9SKIP", "20m", "FT8", &refs(&["US-5"])));
    assert_eq!(s.h.worked.len(), 2);
    let again = s.h.export_state();
    s.h.import_state(&again);
    assert_eq!(s.h.worked.len(), 2);
}

#[test]
fn state_restores_second_park_of_same_station() {
    let mut s = T::new();
    let now = s.now.get();
    s.h.seed_worked(vec![WorkedEntry::new("VE3RRH", "20m", "FT8", Some(now), "", "")]);
    let saved = json!({
        "date": "2026-10-24",
        "worked": [
            {"call": "VE3RRH", "band": "20m", "mode": "FT8", "time": "2026-10-24T18:37:30+00:00",
             "ref": "CA-5574", "park": "Cave Springs", "logged": true},
            {"call": "VE3RRH", "band": "20m", "mode": "FT8", "time": "2026-10-24T20:20:33+00:00",
             "ref": "CA-5573", "park": "Mountainview", "logged": true},
        ],
    });
    s.h.import_state(&saved);
    assert_eq!(s.h.worked.len(), 2);
    let again = s.h.export_state();
    s.h.import_state(&again);
    assert_eq!(s.h.worked.len(), 2);
}

#[test]
fn calls_strongest_eligible() {
    let mut s = T::new();
    s.cycle(&[dec_at("CQ W1PLAIN FN42", 20, 10_515_000),
              dec_at("CQ POTA K1ABC FN42", -10, 10_515_000),
              dec_at("CQ N5XYZ EM12", 3, 10_515_000),
              dec_at("CQ K9OTHER EN52", 15, 10_515_000)]);
    assert_eq!(s.replies(), ["CQ N5XYZ EM12"]);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
}

#[test]
fn skips_stations_below_snr_threshold() {
    let mut s = T::new();
    assert_eq!(s.h.min_snr, Some(-20));
    s.cycle(&[dec_at("CQ POTA K1WEAK FN42", -21, 10_515_000), dec_at("CQ POTA K2OK FN42", -20, 10_515_000)]);
    assert_eq!(s.replies(), ["CQ POTA K2OK FN42"]);
    assert!(s.logged("-21 dB is below the -20 dB threshold"));
}

#[test]
fn weak_only_cycle_calls_nobody_and_no_limit_calls() {
    let mut s = T::new();
    s.cycle(&[dec_at("CQ POTA K1WEAK FN42", -23, 10_515_000)]);
    assert_eq!(s.n_replies(), 0);
    s.h.min_snr = None;
    s.cycle(&[dec_at("CQ POTA K1WEAK FN42", -23, 10_530_000)]);
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn rr73_from_spotted_station() {
    let mut s = T::new();
    s.cycle(&[dec("W2ABC N5XYZ/P RR73")]);
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn stopped_and_dry_run_send_nothing() {
    let mut s = T::new();
    s.h.enabled = false;
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    s.h.enabled = true;
    s.h.dry_run = true;
    s.cycle(&[dec_at("CQ POTA K2ABC FN42", -5, 10_530_000)]);
    assert_eq!(s.n_replies(), 0);
    assert!(s.logged("DRY RUN"));
}

#[test]
fn busy_when_tx_enabled() {
    let mut s = T::new();
    s.hs(st_tx(true, false, ""));
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    assert_eq!(s.n_replies(), 0);
}

fn logged_msg(call: &str) -> QsoLogged {
    QsoLogged { id: "WSJT-X".into(), dx_call: call.into(), tx_freq: 14_075_000, mode: "FT8".into(), ..Default::default() }
}

#[test]
fn dupe_same_band_mode_only() {
    let mut s = T::new();
    s.h.handle_qso_logged(&logged_msg("K1ABC"));
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    assert_eq!(s.n_replies(), 0);
    s.hs(status(7_074_000, false, false, ""));
    s.cycle(&[dec_at("CQ POTA K1ABC FN42", -5, 10_530_000)]);
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn same_park_is_a_dupe_new_park_is_called_again() {
    let mut s = T::new();
    s.cycle(&[dec("CQ N5XYZ EM12")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    let mut x = st_tx(false, false, "N5XYZ");
    x.tx_message = "N5XYZ WD4DAN RR73".into();
    s.hs(x);
    assert_eq!(s.h.worked_entry("N5XYZ", "20m", "FT8").unwrap().ref_, "US-1234");
    s.h.set_target(None);
    s.cycle(&[dec_at("CQ N5XYZ EM12", -5, 10_560_000)]);
    assert_eq!(s.n_replies(), 1);
    s.spot_n5xyz_at("US-5678");
    s.cycle(&[dec_at("CQ N5XYZ EM12", -5, 10_590_000)]);
    assert_eq!(s.n_replies(), 2);
    assert!(s.logged("new park (worked at US-1234 earlier today)"));
}

#[test]
fn qso_without_park_blocks_until_0000z() {
    let mut s = T::new();
    s.h.seed_worked(vec![WorkedEntry::new("N5XYZ", "20m", "FT8", None, "", "")]);
    s.spot_n5xyz_at("US-5678");
    s.cycle(&[dec("CQ N5XYZ EM12")]);
    assert_eq!(s.n_replies(), 0);
    assert!(s.logged("skipped (worked today on 20m FT8 at US-5678)"));
}

#[test]
fn guessed_park_does_not_unblock() {
    let mut s = T::new();
    s.h.seed_worked(vec![WorkedEntry::new("N5XYZ", "20m", "FT8", None, "", "")]);
    s.spot_n5xyz_at("US-1234");
    assert!(s.h.worked_entry("N5XYZ", "20m", "FT8").unwrap().ref_guessed);
    s.spot_n5xyz_at("US-5678");
    s.cycle(&[dec("CQ N5XYZ EM12")]);
    assert_eq!(s.n_replies(), 0);
}

#[test]
fn unspotted_station_worked_at_a_park_is_not_called() {
    let mut s = T::new();
    s.h.seed_worked(vec![WorkedEntry::new("K1ABC", "20m", "FT8", None, "US-1111", "")]);
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    assert_eq!(s.n_replies(), 0);
    assert!(s.logged("park unknown"));
}

#[test]
fn two_parks_same_day_both_listed() {
    let mut s = T::new();
    s.h.seed_worked(vec![WorkedEntry::new("N5XYZ", "20m", "FT8", None, "US-1234", ""),
                         WorkedEntry::new("N5XYZ", "20m", "FT8", None, "US-5678", ""),
                         WorkedEntry::new("N5XYZ", "20m", "FT8", None, "US-5678", "")]);
    let got: Vec<_> = s.h.worked.iter().map(|e| e.ref_.clone()).collect();
    assert_eq!(got, ["US-1234", "US-5678"]);
    assert!(s.h.is_worked("N5XYZ/P", "20m", "FT8", &refs(&["US-5678"])));
    assert!(!s.h.is_worked("N5XYZ", "20m", "FT8", &refs(&["US-9999"])));
    assert!(!s.h.is_worked("N5XYZ", "40m", "FT8", &refs(&["US-1234"])));
}

#[test]
fn reset_at_0000z() {
    let mut s = T::new();
    s.h.handle_qso_logged(&logged_msg("K1ABC"));
    s.now.set(s.now.get() + chrono::Duration::minutes(15));
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    assert_eq!(s.n_replies(), 1);
    assert_eq!(s.h.worked.len(), 0);
}

#[test]
fn gives_up_after_max_attempts() {
    let mut s = T::new();
    s.h.set_max_attempts(2);
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    for _ in 0..2 {
        s.hs(st_tx(true, true, "K1ABC"));
        s.hs(st_tx(true, false, "K1ABC"));
    }
    assert!(s.halts().is_empty());
    s.hs(st_tx(true, true, "K1ABC"));
    assert_eq!(s.halts(), ["WSJT-X"]);
    assert!(s.h.target().is_none());
    s.cycle(&[dec_at("CQ POTA K1ABC FN42", -5, 10_560_000)]);
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn late_answer_after_give_up() {
    let mut s = T::new();
    s.give_up_on_k1abc();
    s.adv(30.0);
    let late = dec_at("WD4DAN K1ABC -07", -5, 10_590_000);
    s.hd(late.clone());
    assert_eq!(s.rec.borrow().replies, [late]);
    assert_eq!(s.target_call().as_deref(), Some("K1ABC"));
    assert!(s.h.target().unwrap().answered);
    assert!(!s.h.cooloff.contains_key("K1ABC"));
    s.hs(st_tx(true, true, "K1ABC"));
    s.hs(st_tx(true, false, "K1ABC"));
    s.hd(dec_at("WD4DAN K1ABC R-07", -5, 10_620_000));
    s.hs(st_tx(true, true, "K1ABC"));
    assert_eq!(s.halts(), ["WSJT-X"]);
}

#[test]
fn late_answer_not_sent_when_busy_or_expired() {
    let mut s = T::new();
    s.give_up_on_k1abc();
    s.hs(st_tx(true, false, "W9ELSE"));
    s.hd(dec_at("WD4DAN K1ABC -07", -5, 10_590_000));
    assert_eq!(s.n_replies(), 0);
    s.hs(st());
    s.adv(Hunter::COOLOFF_SECONDS + 1.0);
    s.hd(dec_at("WD4DAN K1ABC -07", -5, 10_605_000));
    assert_eq!(s.n_replies(), 0);
}

#[test]
fn no_late_answer_for_stations_never_called_or_stopped() {
    let mut s = T::new();
    s.hd(dec("WD4DAN W9RANDOM -07"));
    assert_eq!(s.n_replies(), 0);
    s.give_up_on_k1abc();
    s.h.enabled = false;
    s.hd(dec_at("WD4DAN K1ABC -07", -5, 10_590_000));
    assert_eq!(s.n_replies(), 0);
}

#[test]
fn completed_qso_counts_as_worked_before_logging() {
    let mut s = T::new();
    s.work_k1abc_until_rr73();
    assert!(!s.h.worked_entry("K1ABC", "20m", "FT8").unwrap().logged);
    assert_eq!(s.h.awaiting_log.as_ref().unwrap().call, "K1ABC");
    s.hs(st());
    s.h.set_target(None);
    s.cycle(&[dec_at("CQ POTA K1ABC FN42", -5, 10_590_000)]);
    assert_eq!(s.n_replies(), 1);
    s.h.handle_qso_logged(&logged_msg("K1ABC"));
    assert!(s.h.worked_entry("K1ABC", "20m", "FT8").unwrap().logged);
    assert!(s.h.awaiting_log.is_none());
}

#[test]
fn completion_from_our_own_73() {
    let mut s = T::new();
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    let mut x = st_tx(true, true, "K1ABC");
    x.tx_message = "K1ABC WD4DAN 73                      ".into();
    s.hs(x);
    assert!(s.h.is_worked("K1ABC", "20m", "FT8", &[]));
}

#[test]
fn presses_ok_on_log_window_once() {
    let mut s = T::new();
    s.work_k1abc_until_rr73();
    s.rec.borrow_mut().log_windows = VecDeque::from([false, true]);
    for _ in 0..10 {
        s.adv(2.1);
        s.tick();
    }
    assert!(s.rec.borrow().presses >= 1);
    assert!(!s.logged("Pressed OK"));
    assert!(s.h.awaiting_log.is_some());
    s.adv(Hunter::LOG_WAIT_SECONDS);
    s.tick();
    assert!(s.h.awaiting_log.is_none());
    assert!(s.logged("still not logged"));
}

#[test]
fn gives_up_if_window_wont_close() {
    let mut s = T::new();
    s.work_k1abc_until_rr73();
    s.rec.borrow_mut().log_windows = VecDeque::from([true; 10]);
    s.rec.borrow_mut().presses = 0;
    for _ in 0..6 {
        s.adv(2.1);
        s.tick();
    }
    assert_eq!(s.rec.borrow().presses, 4);
    assert!(s.logged("won't close"));
}

#[test]
fn auto_log_off_never_presses() {
    let mut s = T::new();
    s.h.auto_log = false;
    s.work_k1abc_until_rr73();
    s.rec.borrow_mut().log_windows = VecDeque::from([true; 10]);
    for _ in 0..5 {
        s.adv(2.1);
        s.tick();
    }
    assert_eq!(s.rec.borrow().presses, 0);
    assert!(s.h.is_worked("K1ABC", "20m", "FT8", &[]));
}

#[test]
fn empty_cycle_summary() {
    let mut s = T::new();
    s.cycle(&[dec("CQ W1PLAIN FN42"), dec("K2AAA W3BBB R-10"), dec_at("CQ W1PLAIN FN42", 1, 10_515_000),
              dec("CQ WD4DAN EM80")]);
    assert_eq!(s.summaries(), ["2 calls decoded, no needed POTA"]);
    s.cycle(&[dec("W4CCC W5DDD 73")]);
    assert_eq!(s.summaries().len(), 1);
    s.cycle(&[dec_at("CQ W6EEE CM87", -5, 10_530_000)]);
    assert_eq!(s.summaries().last().unwrap(), "1 call decoded, no needed POTA");
}

#[test]
fn no_summary_when_there_is_a_candidate() {
    let mut s = T::new();
    s.cycle(&[dec("CQ W1PLAIN FN42"), dec("CQ POTA K1ABC FN42")]);
    assert!(s.summaries().is_empty());
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn shows_uncalled_station_calling_me() {
    let mut s = T::new();
    s.hd(dec("WD4DAN W9RANDOM -07"));
    assert_eq!(s.n_replies(), 0);
    assert_eq!(s.ignore_logs().len(), 1);
    assert!(s.ignore_logs()[0].contains("W9RANDOM is calling you, but you never called them"));
}

#[test]
fn answer_button_replies_and_works_like_a_hunter_call() {
    let mut s = T::new();
    s.h.enabled = false;
    let caller = dec("WD4DAN VA3CKU EN93");
    s.hd(caller.clone());
    let a = s.actions.last().unwrap().clone();
    assert_eq!(a.label, "Answer");
    s.click(&a);
    assert_eq!(s.rec.borrow().replies, [caller]);
    assert_eq!(s.target_call().as_deref(), Some("VA3CKU"));
    assert!(s.h.target().unwrap().answered);
    s.hs(st_tx(true, true, "VA3CKU"));
    s.hs(st_tx(true, false, "VA3CKU"));
    s.hd(dec_at("WD4DAN VA3CKU RR73", -5, 10_530_000));
    assert!(s.h.is_worked("VA3CKU", "20m", "FT8", &[]));
    assert_eq!(s.h.awaiting_log.as_ref().unwrap().call, "VA3CKU");
}

fn st40(dx: &str, tx_on: bool, sending: bool, msg: &str) -> Status {
    let mut s = status(7_074_000, tx_on, sending, dx);
    s.tx_message = msg.into();
    s
}

#[test]
fn answer_during_own_cq_qso_replay() {
    // Replays the real 2026-10-25 04:15Z sequence: Answer clicked on AD5DD while
    // WSJT-X was still finishing a QSO with F5PBG.
    let mut s = T::new();
    s.hs(st40("F5PBG", true, true, "F5PBG WD4DAN RR73"));
    s.hs(st40("F5PBG", true, false, "F5PBG WD4DAN RR73"));
    let caller = dec_at("WD4DAN AD5DD EL29", -5, 15_300_000);
    s.hd(caller.clone());
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    assert_eq!(s.rec.borrow().replies, [caller]);
    s.hs(st40("F5PBG", true, false, "F5PBG WD4DAN RR73"));
    assert_eq!(s.target_call().as_deref(), Some("AD5DD"));
    s.hs(st40("AD5DD", true, false, "AD5DD WD4DAN +05"));
    s.hs(st40("AD5DD", true, true, "AD5DD WD4DAN +05"));
    s.hs(st40("AD5DD", true, false, "AD5DD WD4DAN +05"));
    s.hd(dec_at("WD4DAN AD5DD R+04", -5, 15_330_000));
    s.hs(st40("AD5DD", true, true, "AD5DD WD4DAN RR73"));
    assert!(s.h.is_worked("AD5DD", "40m", "FT8", &[]));
    assert_eq!(s.h.awaiting_log.as_ref().unwrap().call, "AD5DD");
}

#[test]
fn logged_messages_have_no_padding_but_reply_is_raw() {
    let mut s = T::new();
    let raw = "WD4DAN AK7W DM42                      a2";
    s.hd(dec(raw));
    assert!(s.logs.last().unwrap().1.contains("AK7W is calling you"));
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    assert_eq!(s.replies().last().unwrap(), raw);
}

#[test]
fn answer_ak7w_replay_rr73_reported_after_tx_ends() {
    let mut s = T::new();
    s.h.enabled = false;
    s.rec.borrow_mut().log_windows = VecDeque::from([true]);
    let st = |tx_on, sending, msg: &str| {
        let mut x = status(7_074_000, tx_on, sending, "AK7W");
        x.tx_message = msg.into();
        x
    };
    s.hs(status(7_074_000, false, false, "KY1ZIU/R"));
    s.hd(dec_at("WD4DAN AK7W DM42                      a2", -5, 15_630_000));
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    s.hs(st(true, true, "AK7W WD4DAN -15"));
    s.hs(st(true, false, "AK7W WD4DAN -15"));
    s.hd(dec_at("WD4DAN AK7W R-08", -5, 15_660_000));
    s.hs(st(true, true, "AK7W WD4DAN -15"));
    s.hs(st(false, false, "AK7W WD4DAN RR73"));
    assert!(s.h.is_worked("AK7W", "40m", "FT8", &[]));
    s.adv(2.1);
    s.tick();
    s.adv(2.1);
    s.tick();
    assert_eq!(s.rec.borrow().presses, 1);
}

#[test]
fn any_log_window_gets_ok_even_for_a_manual_qso() {
    let mut s = T::new();
    s.rec.borrow_mut().window_open = Some(true);
    s.rec.borrow_mut().log_windows = VecDeque::from([true]);
    s.tick();
    assert_eq!(s.rec.borrow().presses, 0);
    s.adv(2.0);
    s.tick();
    assert_eq!(s.rec.borrow().presses, 1);
    s.h.auto_log = false;
    s.rec.borrow_mut().log_windows = VecDeque::from([true]);
    s.adv(5.0);
    s.tick();
    assert_eq!(s.rec.borrow().presses, 1);
}

#[test]
fn tx_stopping_mid_qso_is_logged() {
    let mut s = T::new();
    s.hd(dec("WD4DAN VA3CKU EN93"));
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    s.hs(st_tx(true, true, "VA3CKU"));
    s.hs(st_tx(false, false, "VA3CKU"));
    assert!(s.h.target().is_none());
    assert!(!s.h.is_worked("VA3CKU", "20m", "FT8", &[]));
    assert!(s.logged("before the QSO with VA3CKU completed"));
}

#[test]
fn no_answer_button_for_sign_offs() {
    let mut s = T::new();
    s.hd(dec("WD4DAN N6NXV 73"));
    s.hd(dec("WD4DAN K7ABC RR73"));
    assert!(s.actions.is_empty());
    let last: Vec<_> = s.logs[s.logs.len() - 2..].iter().map(|(_, t)| t.clone()).collect();
    assert_eq!(last, ["N6NXV sent you 73", "K7ABC sent you RR73"]);
    s.hd(dec("WD4DAN W1RRR RRR"));
    assert_eq!(s.actions.len(), 1);
}

#[test]
fn answer_button_refuses_old_calls() {
    let mut s = T::new();
    s.hd(dec("WD4DAN VA3CKU EN93"));
    let a = s.actions.last().unwrap().clone();
    s.adv(Hunter::ANSWER_MAX_AGE + 1.0);
    s.click(&a);
    assert_eq!(s.n_replies(), 0);
    assert!(s.logged("too old"));
}

#[test]
fn no_ignore_message_when_wsjtx_handles_caller() {
    let mut s = T::new();
    s.hs(st_tx(true, false, "W9RANDOM"));
    s.hd(dec("WD4DAN W9RANDOM -07"));
    let mut x = st_tx(true, false, "");
    x.tx_message = "CQ WD4DAN EM80                    ".into();
    s.hs(x);
    s.hd(dec_at("WD4DAN W8CALLER EN81", -5, 10_530_000));
    assert!(s.ignore_logs().is_empty());
}

#[test]
fn shows_late_answer_ignored_when_stopped() {
    let mut s = T::new();
    s.give_up_on_k1abc();
    s.h.enabled = false;
    s.hd(dec_at("WD4DAN K1ABC -07", -5, 10_590_000));
    assert!(s.ignore_logs().last().unwrap().contains("hunting is stopped"));
}

#[test]
fn answer_prevents_give_up() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    s.hs(st_tx(true, true, "K1ABC"));
    s.hs(st_tx(true, false, "K1ABC"));
    s.hd(dec_at("WD4DAN K1ABC -07", -5, 10_530_000));
    s.hs(st_tx(true, true, "K1ABC"));
    assert!(s.halts().is_empty());
    assert!(s.h.target().unwrap().answered);
}

#[test]
fn answer_click_counts_attempts_and_gives_up() {
    let mut s = T::new();
    s.h.set_max_attempts(2);
    s.hd(dec("WD4DAN VA3CKU EN93"));
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    for _ in 0..2 {
        s.hs(st_tx(true, true, "VA3CKU"));
        s.hs(st_tx(true, false, "VA3CKU"));
    }
    assert!(s.halts().is_empty());
    s.hs(st_tx(true, true, "VA3CKU"));
    assert_eq!(s.halts(), ["WSJT-X"]);
    assert!(s.h.target().is_none());
}

#[test]
fn answer_click_stops_counting_once_they_reply() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.hd(dec("WD4DAN VA3CKU EN93"));
    let a = s.actions.last().unwrap().clone();
    s.click(&a);
    s.hs(st_tx(true, true, "VA3CKU"));
    s.hs(st_tx(true, false, "VA3CKU"));
    s.hd(dec_at("WD4DAN VA3CKU R-07", -5, 10_530_000));
    s.hs(st_tx(true, true, "VA3CKU"));
    assert!(s.halts().is_empty());
}

#[test]
fn candidate_always_gets_call_now_button() {
    let mut s = T::new();
    s.hd(dec("CQ POTA K1ABC FN42"));
    assert_eq!(s.labels(), ["Call now"]);
    s.h.enabled = false;
    let cq2 = dec_at("CQ POTA W9XYZ EN52", -5, 10_530_000);
    s.hd(cq2.clone());
    let a = s.actions.last().unwrap().clone();
    assert_eq!(a.label, "Call now");
    s.click(&a);
    assert_eq!(s.rec.borrow().replies, [cq2]);
    assert_eq!(s.target_call().as_deref(), Some("W9XYZ"));
    assert!(!s.h.target().unwrap().answered);
}

#[test]
fn seen_pota_cq_does_not_make_rr73_eligible() {
    let mut s = T::new();
    s.hs(st_tx(true, false, ""));
    s.cycle(&[dec("CQ POTA K1ABC FN42")]);
    s.hs(st());
    s.cycle(&[dec_at("W3XX K1ABC RR73", -5, 10_530_000)]);
    assert!(s.replies().is_empty());
}

#[test]
fn adif_reads_only_today() {
    let adif = "WSJT-X ADIF Export<eoh>\n\
        <call:5>K1ABC <qso_date:8>20261024 <time_on:6>171530 <band:3>20m <mode:4>MFSK \
        <submode:3>FT4 <sig:4>POTA <sig_info:7>US-1234 <eor>\n\
        <call:5>W2DEF <qso_date:8>20261023 <band:3>40m <mode:3>FT8 <eor>\n\
        <call:7>N5XYZ/P <qso_date:8>20261024 <time_on:4>0912 <freq:9>7.075500 <mode:3>FT8 <eor>\n";
    let path = std::env::temp_dir().join(format!("adif-test-{}.adi", std::process::id()));
    std::fs::write(&path, adif).unwrap();
    let got = read_adif_today(&path, NaiveDate::from_ymd_opt(2026, 10, 24).unwrap());
    let _ = std::fs::remove_file(&path);
    let rows: Vec<_> = got.iter().map(|e| (e.call.as_str(), e.band.as_str(), e.mode.as_str(), e.ref_.as_str())).collect();
    assert_eq!(rows, [("K1ABC", "20m", "FT4", "US-1234"), ("N5XYZ/P", "40m", "FT8", "")]);
    assert_eq!(got[0].time, Some(Utc.with_ymd_and_hms(2026, 10, 24, 17, 15, 30).unwrap()));
    assert_eq!(got[1].time, Some(Utc.with_ymd_and_hms(2026, 10, 24, 9, 12, 0).unwrap()));
}

#[test]
fn fresh_signoff_while_waiting_restarts_the_calls() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.logged("waiting for an answer"));
    s.cycle(&[dec_at("W3ABC N5XYZ RR73", 10, 10_530_000)]);
    assert_eq!(s.n_replies(), 2);
    assert!(s.logged("again"));
    assert_eq!(s.h.target().unwrap().attempts, 0);
}

#[test]
fn fresh_signoff_after_giving_up_calls_again() {
    let mut s = T::new();
    s.h.set_max_attempts(1);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(Hunter::SIGNOFF_WAIT + 1.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    s.cycle(&[dec_at("W3ABC N5XYZ RR73", 10, 10_530_000)]);
    assert_eq!(s.n_replies(), 2);
    assert_eq!(s.target_call().as_deref(), Some("N5XYZ"));
}

#[test]
fn fresh_signoff_after_halt_tx_is_not_called() {
    let mut s = T::new();
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(4.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert!(s.h.target().is_none());
    s.cycle(&[dec_at("W3ABC N5XYZ RR73", 10, 10_530_000)]);
    assert_eq!(s.n_replies(), 1);
}

#[test]
fn fresh_cq_after_halt_tx_is_offered_not_called() {
    let mut s = T::new();
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(4.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.cycle(&[dec_at("W3ABC N5XYZ RR73", 10, 10_530_000)]);
    assert_eq!(s.n_replies(), 1); // not called again by Auto-Hunt
    assert!(s.logged("N5XYZ (RR73)")); // but offered, with Call now
    assert!(s.h.pending.is_empty());
}

#[test]
fn halt_tx_on_a_station_we_called_is_not_resent() {
    let mut s = T::new();
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(4.0);
    s.h.halt_tx();
    let before = s.n_replies();
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(4.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    assert_eq!(s.n_replies(), before); // Enable Tx stays off
    assert!(s.h.target().is_none());
}

#[test]
fn keep_calling_restarts_the_count() {
    let mut s = T::new();
    s.h.set_max_attempts(2);
    s.cycle(&[dec("W1AW N5XYZ 73")]);
    s.hs(st_tx(true, true, "N5XYZ"));
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(4.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.hs(st_tx(true, true, "N5XYZ"));
    assert_eq!(s.h.target().unwrap().attempts, 2);
    s.h.perform(&ActionKind::KeepCalling { call: "N5XYZ".into() });
    assert_eq!(s.h.target().unwrap().attempts, 0);
    // the next call of the new round is 1, not 2
    s.adv(13.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.adv(4.0);
    s.hs(st_tx(false, false, "N5XYZ"));
    s.hs(st_tx(true, true, "N5XYZ"));
    assert_eq!(s.h.target().unwrap().attempts, 1);
}
