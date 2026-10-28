// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Yaesu CAT straight to the radio's serial port (Settings > Radio > Radio connection),
//! with a read-back to confirm each command took. The port is normally a VSPE virtual
//! port shared with ACLog (live-tested on COM20, 2026-10-27). The rig's auto-information
//! stream shares the line, so replies are scanned for the expected `XXnn;` anywhere.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serialport::{DataBits, FlowControl, Parity, StopBits};

use crate::events::{Event, Post};
use crate::hunter::Tag;

/// Which CAT command set to speak. Yaesu is live-confirmed (FTDX101D); the Kenwood and
/// Icom strings come from Hamlib and are not yet confirmed on real rigs. Variants carry
/// what differs between models of one family: full-scale transmit watts, or the CI-V address.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rig {
    /// Yaesu "newcat" ASCII set (FTDX101, FT-991, FT-710, FT-891, FT-450, ...); max watts
    Yaesu(u32),
    /// TS-590S/SG, TS-480, TS-2000: ATT `RAnn`, NB `NBn`, NR `NRn`; max watts
    Ts590(u32),
    /// TS-890S: ATT is one digit (0 off, 1 = 6 dB) and NB is `NB1` (blanker 1)
    Ts890,
    /// TS-990S: like the 590 but ATT is read as `RA0n` (main receiver), 200 W
    Ts990,
    /// Icom CI-V binary frames to the radio's address (94h = IC-7300). Commands are kept
    /// as hex strings without the frame (`1622 01` = NB on); `wire` adds `FE FE addr E0 .. FD`.
    Icom(u8),
    /// Xiegu: the Icom CI-V set at its own address; max watts (the 0-255 power scale) and
    /// a 12 dB attenuator
    Xiegu(u8, u32),
    /// Elecraft K3/KX3/KX2/K4: Kenwood-style, but power is `PCnnnx` (x 0 = 0.1 W units,
    /// 1 = 1 W units); max watts
    Elecraft(u32),
}

pub const MAKES: [&str; 5] = ["Yaesu", "Kenwood", "Elecraft", "Icom", "Xiegu"];

/// Yaesu and Kenwood models: (make, model, command set).
const FIXED: [(&str, &str, Rig); 26] = [
    ("Elecraft", "K3/K3S (10 W)", Rig::Elecraft(10)),
    ("Elecraft", "K3/K3S + KPA100 (100 W)", Rig::Elecraft(100)),
    ("Elecraft", "KX3 (15 W)", Rig::Elecraft(15)),
    ("Elecraft", "KX2 (12 W)", Rig::Elecraft(12)),
    ("Elecraft", "K4 (100 W)", Rig::Elecraft(100)),
    ("Xiegu", "G90 (20 W)", Rig::Xiegu(0x88, 20)),
    ("Xiegu", "X6100 (10 W)", Rig::Xiegu(0xA4, 10)),
    ("Xiegu", "X5105 (5 W)", Rig::Xiegu(0x70, 5)),
    ("Yaesu", "FTDX101D", Rig::Yaesu(100)),
    ("Yaesu", "FTDX101MP", Rig::Yaesu(200)),
    ("Yaesu", "FTDX10", Rig::Yaesu(100)),
    ("Yaesu", "FT-710", Rig::Yaesu(100)),
    ("Yaesu", "FT-991/991A", Rig::Yaesu(100)),
    ("Yaesu", "FT-891", Rig::Yaesu(100)),
    ("Yaesu", "FTDX3000", Rig::Yaesu(100)),
    ("Yaesu", "FTDX1200", Rig::Yaesu(100)),
    ("Yaesu", "FTDX5000", Rig::Yaesu(200)),
    ("Yaesu", "FT-2000", Rig::Yaesu(100)),
    ("Yaesu", "FT-2000D", Rig::Yaesu(200)),
    ("Yaesu", "FT-950", Rig::Yaesu(100)),
    ("Yaesu", "FT-450/450D", Rig::Yaesu(100)),
    ("Kenwood", "TS-590S/SG", Rig::Ts590(100)),
    ("Kenwood", "TS-890S", Rig::Ts890),
    ("Kenwood", "TS-990S", Rig::Ts990),
    ("Kenwood", "TS-2000", Rig::Ts590(100)),
    ("Kenwood", "TS-480 (100 W)", Rig::Ts590(100)),
];

/// Icom radios: (CI-V address, model, full-scale watts of the 0-255 power level, ATT dB
/// used for "on"). Addresses and attenuator steps are from Hamlib's Icom backend.
const ICOM: [(u8, &str, u32, u32); 13] = [
    (0x70, "IC-7000", 100, 12),
    (0x88, "IC-7100", 100, 20),
    (0x94, "IC-7300", 100, 20),
    (0xB6, "IC-7300MK2", 100, 20),
    (0x80, "IC-7410", 100, 18),
    (0x7A, "IC-7600", 100, 18),
    (0x98, "IC-7610", 100, 18),
    (0x74, "IC-7700", 200, 18),
    (0x6A, "IC-7800", 200, 18),
    (0x7C, "IC-9100", 100, 20),
    (0xA2, "IC-9700", 100, 10),
    (0xA4, "IC-705", 10, 20),
    (0xAC, "IC-905", 10, 20),
];

/// The model entry for Icoms whose address is not in the list.
pub const ICOM_OTHER: &str = "Other";

/// (full-scale watts, ATT dB for "on") of an Icom address. Unknown addresses assume 100 W
/// and 20 dB (off, 00, is the same everywhere).
fn icom_info(addr: u8) -> (u32, u32) {
    ICOM.iter().find(|m| m.0 == addr).map_or((100, 20), |m| (m.2, m.3))
}

/// An Icom CI-V address typed as hex ("94", "0x94", "94h").
pub fn parse_civ(s: &str) -> Option<u8> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X").trim_end_matches(['h', 'H']);
    u8::from_str_radix(t, 16).ok()
}

/// The models offered for a make, in menu order (Icom ends with "Other").
pub fn model_names(make: &str) -> Vec<&'static str> {
    if make == "Icom" {
        ICOM.iter().map(|m| m.1).chain([ICOM_OTHER]).collect()
    } else {
        FIXED.iter().filter(|m| m.0 == make).map(|m| m.1).collect()
    }
}

/// Put settings saved by older builds ("Kenwood TS-590S/SG", "Icom" with no model) onto
/// make + model, and make sure the model belongs to the make.
pub fn migrate(make: &mut String, model: &mut String, civ: &str) {
    if let Some(m) = make.strip_prefix("Kenwood ") {
        *model = m.to_string();
        *make = "Kenwood".into();
    }
    if !MAKES.contains(&make.as_str()) {
        *make = "Yaesu".into();
    }
    let names = model_names(make);
    if !names.contains(&model.as_str()) {
        let by_addr = parse_civ(civ).and_then(|a| ICOM.iter().find(|m| m.0 == a)).map(|m| m.1);
        *model = if make == "Icom" { by_addr.unwrap_or(ICOM_OTHER) } else { names[0] }.to_string();
    }
}

/// The command set for a make + model (+ the typed CI-V address for an Icom "Other").
pub fn resolve(make: &str, model: &str, civ: &str) -> Rig {
    if make == "Icom" {
        return Rig::Icom(ICOM.iter().find(|m| m.1 == model).map_or_else(|| parse_civ(civ).unwrap_or(0x94), |m| m.0));
    }
    FIXED.iter().find(|m| m.0 == make && m.1 == model).or_else(|| FIXED.iter().find(|m| m.0 == make)).map_or(Rig::Yaesu(100), |m| m.2)
}

impl Rig {
    pub fn brand(self) -> &'static str {
        match self {
            Rig::Yaesu(_) => "Yaesu",
            Rig::Icom(_) => "Icom",
            Rig::Xiegu(..) => "Xiegu",
            Rig::Elecraft(_) => "Elecraft",
            _ => "Kenwood",
        }
    }

    /// (CI-V address, full-scale watts, ATT dB for "on") for the CI-V radios.
    fn civ(self) -> Option<(u8, u32, u32)> {
        match self {
            Rig::Icom(a) => {
                let (w, att) = icom_info(a);
                Some((a, w, att))
            }
            Rig::Xiegu(a, w) => Some((a, w, 12)),
            _ => None,
        }
    }

    /// The baud rate the radio is normally left at (Yaesu USB port 38400, Kenwood menu
    /// default 9600, Icom CI-V 19200).
    pub fn default_baud(self) -> u32 {
        match self {
            Rig::Yaesu(_) | Rig::Elecraft(_) => 38400,
            Rig::Icom(_) | Rig::Xiegu(..) => 19200,
            _ => 9600,
        }
    }

    /// Highest transmit power the radio takes, in watts.
    pub fn max_power(self) -> u32 {
        match self {
            Rig::Yaesu(w) | Rig::Ts590(w) | Rig::Elecraft(w) => w,
            Rig::Ts890 => 100,
            Rig::Ts990 => 200,
            Rig::Icom(_) | Rig::Xiegu(..) => self.civ().map_or(100, |c| c.1),
        }
    }
}

/// The CI-V power level (0-255) for `w` watts of a radio whose full scale is `max` W.
fn icom_level(max: u32, w: u32) -> u32 {
    ((w as f64 * 255.0 / max as f64).round() as u32).min(255)
}

/// Elecraft `PCnnnx` as a number nnnx: x = 0 means nnn is tenths of a watt, else watts.
fn elecraft_watts(n: u32) -> u32 {
    if n % 10 == 0 { (n / 10 + 5) / 10 } else { n / 10 }
}

fn to_watts(rig: Rig, raw: u32) -> u32 {
    match rig.civ() {
        Some((_, max, _)) => (raw as f64 * max as f64 / 255.0).round() as u32,
        None => raw,
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

/// What goes on the wire for a command or query string.
fn wire(rig: Rig, cmd: &str) -> Vec<u8> {
    match rig.civ() {
        Some((a, _, _)) => {
            let mut v = vec![0xFE, 0xFE, a, 0xE0];
            v.extend(hex_bytes(cmd));
            v.push(0xFD);
            v
        }
        None => cmd.as_bytes().to_vec(),
    }
}

/// What arrived, as text the reply regexes can scan. Icom: each frame from the radio to
/// us becomes `:<payload hex>;` (our own echoed frames, broadcasts and ACKs of other
/// controllers are dropped).
fn normalize(rig: Rig, got: &[u8]) -> String {
    let Some((a, _, _)) = rig.civ() else { return String::from_utf8_lossy(got).into_owned() };
    let mut out = String::new();
    for chunk in got.split(|b| *b == 0xFD) {
        let Some(i) = chunk.windows(2).position(|w| w == [0xFE, 0xFE]) else { continue };
        let body: Vec<u8> = chunk[i..].iter().copied().skip_while(|b| *b == 0xFE).collect();
        if body.len() > 2 && body[0] == 0xE0 && body[1] == a {
            out.push(':');
            for b in &body[2..] {
                out.push_str(&format!("{b:02X}"));
            }
            out.push(';');
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Item {
    Att,
    Nb,
    Nr,
    Pwr,
}

impl Item {
    fn of(rig: Rig, cmd: &str) -> Item {
        if rig.civ().is_some() {
            return match cmd {
                c if c.starts_with("1622") => Item::Nb,
                c if c.starts_with("1640") => Item::Nr,
                c if c.starts_with("11") => Item::Att,
                _ => Item::Pwr,
            };
        }
        match &cmd[..2] {
            "RA" => Item::Att,
            "NB" => Item::Nb,
            "NR" => Item::Nr,
            _ => Item::Pwr,
        }
    }
}

/// The set command for `item` = `v` (0/1 for the on/off items, watts for power).
fn make_set(rig: Rig, item: Item, v: u32) -> String {
    match (rig, item) {
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Att) => format!("11{:02}", if v == 0 { 0 } else { r.civ().map_or(20, |c| c.2) }),
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Nb) => format!("1622{v:02}"),
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Nr) => format!("1640{v:02}"),
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Pwr) => format!("140A{:04}", icom_level(r.civ().map_or(100, |c| c.1), v)),
        (Rig::Elecraft(_), Item::Pwr) => {
            if v <= 15 { format!("PC{:03}0;", v * 10) } else { format!("PC{v:03}1;") }
        }
        (_, Item::Pwr) => format!("PC{v:03};"),
        (Rig::Yaesu(_), Item::Att) => format!("RA0{v};"),
        (Rig::Yaesu(_), Item::Nb) => format!("NB0{v};"),
        (Rig::Yaesu(_), Item::Nr) => format!("NR0{v};"),
        (Rig::Ts590(_) | Rig::Ts990 | Rig::Elecraft(_), Item::Att) => format!("RA{v:02};"),
        (Rig::Ts890, Item::Att) => format!("RA{v};"),
        (Rig::Ts890, Item::Nb) => format!("NB1{v};"),
        (_, Item::Nb) => format!("NB{v};"),
        (_, Item::Nr) => format!("NR{v};"),
    }
}

/// The value a set command carries (inverse of `make_set`).
fn set_value(rig: Rig, cmd: &str) -> (Item, u32) {
    let item = Item::of(rig, cmd);
    if rig.civ().is_some() {
        let n = cmd.get(if item == Item::Att { 2 } else { 4 }..).and_then(|s| s.parse().ok()).unwrap_or(0);
        return (item, if item == Item::Pwr { to_watts(rig, n) } else { n });
    }
    let skip = match (rig, item) {
        (_, Item::Pwr) => 2,
        (Rig::Yaesu(_), _) | (Rig::Ts890, Item::Nb) => 3,
        _ => 2,
    };
    let n = cmd.trim_end_matches(';').get(skip..).and_then(|s| s.parse().ok()).unwrap_or(0);
    (item, if item == Item::Pwr && matches!(rig, Rig::Elecraft(_)) { elecraft_watts(n) } else { n })
}

/// (query, reply key, digits pattern): what to ask for `item` and how its answer looks.
/// The Kenwood TS-590 answers ATT with more than two digits, nonzero meaning on.
fn read_spec(rig: Rig, item: Item) -> (&'static str, &'static str, &'static str) {
    match (rig, item) {
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Att) => ("11", "11", r"\d{2}"),
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Nb) => ("1622", "1622", r"\d{2}"),
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Nr) => ("1640", "1640", r"\d{2}"),
        (r @ (Rig::Icom(_) | Rig::Xiegu(..)), Item::Pwr) => ("140A", "140A", r"\d{4}"),
        (Rig::Elecraft(_), Item::Pwr) => ("PC;", "PC", r"\d{3,4}"),
        (_, Item::Pwr) => ("PC;", "PC", r"\d{3}"),
        (Rig::Yaesu(_), Item::Att) => ("RA0;", "RA0", r"\d"),
        (Rig::Yaesu(_), Item::Nb) => ("NB0;", "NB0", r"\d"),
        (Rig::Yaesu(_), Item::Nr) => ("NR0;", "NR0", r"\d"),
        (Rig::Ts590(_) | Rig::Elecraft(_), Item::Att) => ("RA;", "RA", r"\d+"),
        (Rig::Ts990, Item::Att) => ("RA0;", "RA0", r"\d"),
        (Rig::Ts890, Item::Att) => ("RA;", "RA", r"\d"),
        (Rig::Ts890, Item::Nb) => ("NB1;", "NB1", r"\d"),
        (_, Item::Nb) => ("NB;", "NB", r"\d"),
        (_, Item::Nr) => ("NR;", "NR", r"\d"),
    }
}

/// On/off items compare as nonzero; NR level and power compare exactly.
fn norm(item: Item, v: u32) -> u32 {
    if matches!(item, Item::Att | Item::Nb) { (v != 0) as u32 } else { v }
}

fn reply_regex(rig: Rig, item: Item) -> regex::Regex {
    let (_, key, pat) = read_spec(rig, item);
    let lead = if rig.civ().is_some() { ":" } else { "" };
    regex::Regex::new(&format!("{lead}{key}({pat});")).unwrap()
}

/// The last value of `item` that appears in `reply` (CI-V and Elecraft power in watts).
fn value_in(rig: Rig, item: Item, reply: &str) -> Option<u32> {
    let caps = reply_regex(rig, item).captures_iter(reply).last()?;
    let n: u32 = caps[1].parse().ok()?;
    Some(match item {
        Item::Pwr if matches!(rig, Rig::Elecraft(_)) && caps[1].len() == 4 => elecraft_watts(n),
        Item::Pwr => to_watts(rig, n),
        _ => n,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cfg {
    pub rig: Rig,
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    /// "none", "xonxoff" or "hardware"
    pub handshake: String,
}

/// How many times a command is sent before it is reported as not confirmed.
const TRIES: usize = 3;
/// How long to wait for the rig's answer to a query.
const ANSWER_WAIT: Duration = Duration::from_millis(700);

/// The query that reads back a set command: Yaesu `NB00;` -> `NB0;`, `PC009;` -> `PC;`,
/// Kenwood `NB1;` -> `NB;`.
pub fn query_for(rig: Rig, set: &str) -> String {
    read_spec(rig, Item::of(rig, set)).0.to_string()
}

/// True when `reply` holds the state `set` asked for.
pub fn confirms(rig: Rig, reply: &str, set: &str) -> bool {
    let (item, want) = set_value(rig, set);
    value_in(rig, item, reply).is_some_and(|v| norm(item, v) == norm(item, want))
}

fn open(cfg: &Cfg) -> Result<Box<dyn serialport::SerialPort>, String> {
    let flow = match cfg.handshake.as_str() {
        "xonxoff" => FlowControl::Software,
        "hardware" => FlowControl::Hardware,
        _ => FlowControl::None,
    };
    let mut p = serialport::new(&cfg.port, cfg.baud)
        .data_bits(if cfg.data_bits == 7 { DataBits::Seven } else { DataBits::Eight })
        .stop_bits(if cfg.stop_bits == 2 { StopBits::Two } else { StopBits::One })
        .parity(Parity::None)
        .flow_control(flow)
        .timeout(Duration::from_millis(100))
        .open()
        .map_err(|e| format!("{}: {e}", cfg.port))?;
    if flow != FlowControl::Hardware {
        // keep PTT/keying lines quiet
        let _ = p.write_data_terminal_ready(false);
        let _ = p.write_request_to_send(false);
    }
    Ok(p)
}

/// Read until `done` accepts what has arrived or `wait` passes.
fn read_until(p: &mut dyn serialport::SerialPort, rig: Rig, done: impl Fn(&str) -> bool, wait: Duration) -> String {
    let end = Instant::now() + wait;
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while Instant::now() < end {
        match p.read(&mut buf) {
            Ok(n) if n > 0 => {
                got.extend_from_slice(&buf[..n]);
                if done(&normalize(rig, &got)) {
                    break;
                }
            }
            _ => {}
        }
    }
    normalize(rig, &got)
}

/// Send each set command (`NB00;`...) and read it back. Err when the port can't be
/// opened or written; otherwise one (command, confirmed) per command.
pub fn send_verified(cfg: &Cfg, cmds: &[String]) -> Result<Vec<(String, bool)>, String> {
    let mut p = open(cfg)?;
    let mut out = vec![];
    for c in cmds {
        let mut ok = false;
        for _ in 0..TRIES {
            let _ = p.clear(serialport::ClearBuffer::Input);
            p.write_all(&wire(cfg.rig, c)).map_err(|e| format!("{}: {e}", cfg.port))?;
            std::thread::sleep(Duration::from_millis(150));
            let _ = p.clear(serialport::ClearBuffer::Input);
            p.write_all(&wire(cfg.rig, &query_for(cfg.rig, c))).map_err(|e| format!("{}: {e}", cfg.port))?;
            let reply = read_until(p.as_mut(), cfg.rig, |r| confirms(cfg.rig, r, c), ANSWER_WAIT);
            if confirms(cfg.rig, &reply, c) {
                ok = true;
                break;
            }
        }
        out.push((c.clone(), ok));
    }
    Ok(out)
}



/// Ask for one setting and parse its value, skipping the rig's other chatter.
fn read_value(p: &mut dyn serialport::SerialPort, rig: Rig, item: Item) -> Option<u32> {
    let query = read_spec(rig, item).0;
    for _ in 0..TRIES {
        let _ = p.clear(serialport::ClearBuffer::Input);
        p.write_all(&wire(rig, query)).ok()?;
        let got = read_until(p, rig, |r| value_in(rig, item, r).is_some(), ANSWER_WAIT);
        if let Some(v) = value_in(rig, item, &got) {
            return Some(v);
        }
    }
    None
}

/// Set one value, give the rig a moment, read it back.
fn set_and_read(p: &mut dyn serialport::SerialPort, rig: Rig, item: Item, v: u32) -> Result<Option<u32>, String> {
    p.write_all(&wire(rig, &make_set(rig, item, v))).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(200));
    Ok(read_value(p, rig, item))
}

/// Exercise NB, NR, ATT and power: read each, change it, check the radio reports the
/// change, put it back and check that too. Returns (all passed, one line per item).
pub fn run_test(cfg: &Cfg) -> (bool, Vec<String>) {
    let mut p = match open(cfg) {
        Ok(p) => p,
        Err(e) => return (false, vec![format!("Could not open the port: {e}")]),
    };
    let mut all_ok = true;
    let mut lines = vec![];
    let rig = cfg.rig;
    for (name, item) in [("NB", Item::Nb), ("NR", Item::Nr), ("ATT", Item::Att), ("PWR", Item::Pwr)] {
        let show = |v: u32| if item == Item::Pwr { format!("{v} W") } else if v == 0 { "off".into() } else { "on".into() };
        let Some(orig) = read_value(p.as_mut(), rig, item).map(|v| norm(item, v)) else {
            lines.push(format!("{name}: FAILED, the radio did not answer"));
            all_ok = false;
            continue;
        };
        let target = if item == Item::Pwr { if orig < rig.max_power() { orig + 1 } else { orig - 1 } } else { 1 - orig.min(1) };
        let changed = set_and_read(p.as_mut(), rig, item, target);
        let restored = set_and_read(p.as_mut(), rig, item, orig); // always put it back
        let (ok, text) = match (changed, restored) {
            (Err(e), _) | (_, Err(e)) => (false, format!("FAILED, port error: {e}")),
            (Ok(c), Ok(r)) => {
                let c_ok = c.map(|v| norm(item, v)) == Some(target);
                let r_ok = r.map(|v| norm(item, v)) == Some(orig);
                let text = format!(
                    "was {}, set {} {}, restored {} {}",
                    show(orig), show(target), if c_ok { "confirmed" } else { "NOT CONFIRMED" },
                    show(orig), if r_ok { "confirmed" } else { "NOT CONFIRMED" },
                );
                (c_ok && r_ok, text)
            }
        };
        all_ok &= ok;
        lines.push(format!("{name}: {}{text}", if ok { "OK, " } else { "" }));
    }
    (all_ok, lines)
}

/// CAT strings for the main receiver (ATT, NB, NR); each argument is Some(true)
/// (on), Some(false) (off) or None (leave alone). `power` is the transmit power in watts
/// (PC, three digits, 5 to 100 on the FTDX101D, TS-590S/SG and TS-890S). Kenwood "on" is
/// ATT on (TS-890S: 6 dB), NB on and NR1.
pub fn rig_commands(rig: Rig, att: Option<bool>, nb: Option<bool>, dnr: Option<bool>, power: Option<u32>) -> Vec<String> {
    let mut v: Vec<String> = [(Item::Att, att), (Item::Nb, nb), (Item::Nr, dnr)]
        .iter()
        .filter_map(|(item, s)| s.map(|on| make_set(rig, *item, on as u32)))
        .collect();
    if let Some(w) = power {
        v.push(make_set(rig, Item::Pwr, w.clamp(5.min(rig.max_power()), rig.max_power())));
    }
    v
}

/// "ATT off", "PWR 9w"... for the Current Info line.
fn describe(rig: Rig, cmds: &[String]) -> String {
    let items: Vec<String> = cmds
        .iter()
        .map(|c| {
            let (item, v) = set_value(rig, c);
            let onoff = if v == 0 { "off" } else { "on" };
            match item {
                Item::Att => format!("ATT {onoff}"),
                Item::Nb => format!("NB {onoff}"),
                Item::Nr => format!("NR {onoff}"),
                Item::Pwr => format!("PWR {v}w"),
            }
        })
        .collect();
    items.join(" / ")
}

/// Sets the radio over its serial port on worker threads and posts the results as
/// `Event::Aclog` lines (a "CAT:" line becomes the Current Info line). With no port
/// chosen nothing is sent: ACLog is no longer used for CAT.
#[derive(Clone)]
pub struct RigClient {
    post: Post,
    cfg: Arc<Mutex<Option<Cfg>>>,
    lock: Arc<Mutex<()>>,
}

impl RigClient {
    pub fn new(post: Post) -> Self {
        RigClient { post, cfg: Arc::new(Mutex::new(None)), lock: Arc::new(Mutex::new(())) }
    }

    pub fn set_cfg(&self, cfg: Option<Cfg>) {
        *self.cfg.lock().unwrap() = cfg;
    }

    /// Send the commands on a worker thread, or right here with `wait`.
    pub fn set_rig(&self, att: Option<bool>, nb: Option<bool>, dnr: Option<bool>, power: Option<u32>, wait: bool, why: &str, quiet: bool) {
        let rig = self.cfg.lock().unwrap().as_ref().map_or(Rig::Yaesu(100), |c| c.rig);
        let cmds = rig_commands(rig, att, nb, dnr, power);
        if cmds.is_empty() {
            return;
        }
        if wait {
            self.do_set_rig(&cmds, why, quiet);
        } else {
            let me = self.clone();
            let why = why.to_string();
            std::thread::Builder::new().name("rig-cat".into()).spawn(move || me.do_set_rig(&cmds, &why, quiet)).ok();
        }
    }


    /// Run `run_test` on a worker thread (holding the rig lock so nothing else is sent
    /// meanwhile) and post `Event::CatTest` with the result.
    pub fn test(&self) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("rig-test".into())
            .spawn(move || {
                let _g = me.lock.lock().unwrap();
                let cfg = me.cfg.lock().unwrap().clone();
                let (ok, lines) = match cfg {
                    Some(cfg) => {
                        let (ok, mut lines) = run_test(&cfg);
                        lines.insert(0, format!("Radio on {} ({} baud, {} data bits, {} stop, {} handshake)", cfg.port, cfg.baud, cfg.data_bits, cfg.stop_bits, cfg.handshake));
                        (ok, lines)
                    }
                    None => (false, vec!["No radio COM port is chosen.".to_string()]),
                };
                (me.post)(Event::CatTest(ok, lines));
            })
            .ok();
    }

    fn do_set_rig(&self, cmds: &[String], why: &str, quiet: bool) {
        let _g = self.lock.lock().unwrap();
        let say = |t: String, tag: Tag| (self.post)(Event::Aclog(t, tag));
        let Some(cfg) = self.cfg.lock().unwrap().clone() else {
            if !quiet {
                say("No radio COM port chosen (Settings > Radio), so no CAT commands were sent".into(), Tag::Error);
            }
            return;
        };
        match send_verified(&cfg, cmds) {
            Ok(res) => {
                let ok: Vec<String> = res.iter().filter(|(_, ok)| *ok).map(|(c, _)| c.clone()).collect();
                let bad: Vec<String> = res.iter().filter(|(_, ok)| !*ok).map(|(c, _)| c.clone()).collect();
                if !ok.is_empty() {
                    say(format!("CAT:{} via {}, confirmed: {}", cfg.rig.brand(), cfg.port, describe(cfg.rig, &ok)), Tag::Info);
                }
                if bad.is_empty() {
                    if !quiet {
                        say(format!("{why} (confirmed by the radio on {})", cfg.port), Tag::Info);
                    }
                } else {
                    say(format!("The radio on {} did not confirm: {}", cfg.port, describe(cfg.rig, &bad)), Tag::Error);
                }
            }
            Err(e) => say(format!("Radio serial port not usable, no CAT sent: {e}"), Tag::Error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Rig::*;

    #[test]
    fn all_off() {
        assert_eq!(rig_commands(Yaesu(100), Some(false), Some(false), Some(false), None), ["RA00;", "NB00;", "NR00;"]);
        assert_eq!(rig_commands(Ts590(100), Some(false), Some(false), Some(false), None), ["RA00;", "NB0;", "NR0;"]);
        assert_eq!(rig_commands(Ts890, Some(false), Some(false), Some(false), None), ["RA0;", "NB10;", "NR0;"]);
    }

    #[test]
    fn mixed_and_leave() {
        assert_eq!(rig_commands(Yaesu(100), Some(true), None, Some(false), None), ["RA01;", "NR00;"]);
        assert_eq!(rig_commands(Ts590(100), Some(true), Some(true), Some(true), None), ["RA01;", "NB1;", "NR1;"]);
        assert_eq!(rig_commands(Ts890, Some(true), Some(true), None, None), ["RA1;", "NB11;"]);
        assert!(rig_commands(Yaesu(100), None, None, None, None).is_empty());
    }

    #[test]
    fn power() {
        for rig in [Yaesu(100), Ts590(100), Ts890] {
            assert_eq!(rig_commands(rig, None, None, None, Some(5)), ["PC005;"]);
            assert_eq!(rig_commands(rig, None, None, None, Some(100)), ["PC100;"]);
            assert_eq!(rig_commands(rig, None, None, None, Some(250)), ["PC100;"]);
            assert_eq!(describe(rig, &["PC009;".to_string()]), "PWR 9w");
        }
        assert_eq!(describe(Ts890, &["NB10;".to_string(), "RA1;".to_string()]), "NB off / ATT on");
    }

    #[test]
    fn queries() {
        assert_eq!(query_for(Yaesu(100), "NB00;"), "NB0;");
        assert_eq!(query_for(Yaesu(100), "RA01;"), "RA0;");
        assert_eq!(query_for(Yaesu(100), "NR00;"), "NR0;");
        assert_eq!(query_for(Yaesu(100), "PC009;"), "PC;");
        assert_eq!(query_for(Ts590(100), "RA00;"), "RA;");
        assert_eq!(query_for(Ts590(100), "NB1;"), "NB;");
        assert_eq!(query_for(Ts890, "NB11;"), "NB1;");
        assert_eq!(query_for(Ts890, "NR2;"), "NR;");
    }

    #[test]
    fn confirmation_scans_the_auto_info_noise() {
        assert!(confirms(Yaesu(100), "VS0;NB01;IF503007074000+000000C00000;FA007074000;", "NB01;"));
        assert!(!confirms(Yaesu(100), "NB00;VS0;", "NB01;"));
        assert!(!confirms(Yaesu(100), "", "RA00;"));
    }

    #[test]
    fn kenwood_confirmation() {
        assert!(confirms(Ts590(100), "FA00014074000;NB1;", "NB1;"));
        assert!(!confirms(Ts590(100), "NB0;", "NB1;"));
        // the TS-590 may answer ATT with extra digits; nonzero means on
        assert!(confirms(Ts590(100), "RA0100;", "RA01;"));
        assert!(confirms(Ts590(100), "RA0000;", "RA00;"));
        assert!(!confirms(Ts590(100), "RA0000;", "RA01;"));
        assert!(confirms(Ts890, "RA1;", "RA1;"));
        assert!(confirms(Ts890, "NB10;", "NB10;"));
        assert!(!confirms(Ts890, "NB11;", "NB10;"));
        assert!(confirms(Ts590(100), "PC005;", "PC005;"));
        assert!(!confirms(Ts590(100), "PC010;", "PC005;"));
        assert!(confirms(Ts590(100), "NR2;", "NR2;"));
        assert!(!confirms(Ts590(100), "NR2;", "NR1;"));
    }

    #[test]
    fn icom_commands() {
        let ic7300 = Icom(0x94);
        assert_eq!(rig_commands(ic7300, Some(false), Some(false), Some(false), None), ["1100", "162200", "164000"]);
        assert_eq!(rig_commands(ic7300, Some(true), Some(true), Some(true), None), ["1120", "162201", "164001"]);
        assert_eq!(rig_commands(ic7300, None, None, None, Some(100)), ["140A0255"]);
        assert_eq!(rig_commands(ic7300, None, None, None, Some(50)), ["140A0128"]);
        assert_eq!(rig_commands(Icom(0xA4), None, None, None, Some(250)), ["140A0255"]); // IC-705: 10 W
        assert_eq!(rig_commands(Icom(0xA4), None, None, None, Some(5)), ["140A0128"]);
        assert_eq!(rig_commands(Icom(0x98), Some(true), None, None, None), ["1118"]);
        assert_eq!(describe(ic7300, &["1100".to_string(), "162201".to_string(), "140A0128".to_string()]), "ATT off / NB on / PWR 50w");
        assert_eq!(wire(ic7300, "162201"), [0xFE, 0xFE, 0x94, 0xE0, 0x16, 0x22, 0x01, 0xFD]);
        assert_eq!(query_for(ic7300, "162201"), "1622");
        assert_eq!(query_for(ic7300, "1120"), "11");
        assert_eq!(query_for(ic7300, "140A0128"), "140A");
    }

    #[test]
    fn icom_confirmation_ignores_echo_and_broadcasts() {
        let r = Icom(0x94);
        // our echoed query, a transceive broadcast, then the real answer, then an ACK
        let got = [
            0xFE, 0xFE, 0x94, 0xE0, 0x16, 0x22, 0xFD, //
            0xFE, 0xFE, 0x00, 0x94, 0x00, 0x00, 0x74, 0x07, 0x14, 0x00, 0xFD, //
            0xFE, 0xFE, 0xE0, 0x94, 0x16, 0x22, 0x01, 0xFD, //
            0xFE, 0xFE, 0xE0, 0x94, 0xFB, 0xFD,
        ];
        let text = normalize(r, &got);
        assert_eq!(text, ":162201;:FB;");
        assert!(confirms(r, &text, "162201"));
        assert!(!confirms(r, &text, "162200"));
        assert!(confirms(r, ":1100;", "1100"));
        assert!(confirms(r, ":1120;", "1120"));
        assert!(!confirms(r, ":1100;", "1120"));
        assert!(confirms(r, ":140A0128;", "140A0128"));
        assert!(!confirms(r, ":140A0100;", "140A0128"));
        assert!(!confirms(r, ":FA;", "162201"));
        assert_eq!(parse_civ("94"), Some(0x94));
        assert_eq!(parse_civ("0xA4"), Some(0xA4));
        assert_eq!(parse_civ("98h"), Some(0x98));
        assert_eq!(parse_civ("zz"), None);
    }

    #[test]
    fn models_resolve_and_migrate() {
        assert_eq!(resolve("Icom", "IC-7610", "94"), Icom(0x98));
        assert_eq!(resolve("Icom", ICOM_OTHER, "8E"), Icom(0x8E));
        assert_eq!(resolve("Kenwood", "TS-890S", ""), Ts890);
        assert_eq!(resolve("Kenwood", "TS-990S", ""), Ts990);
        assert_eq!(resolve("Yaesu", "FTDX5000", ""), Yaesu(200));
        assert_eq!(resolve("Yaesu", "nonsense", ""), Yaesu(100));
        assert_eq!(resolve("nonsense", "", ""), Yaesu(100));
        // settings saved by the first Kenwood/Icom builds
        let (mut make, mut model) = ("Kenwood TS-590S/SG".to_string(), String::new());
        migrate(&mut make, &mut model, "94");
        assert_eq!((make.as_str(), model.as_str()), ("Kenwood", "TS-590S/SG"));
        let (mut make, mut model) = ("Icom".to_string(), String::new());
        migrate(&mut make, &mut model, "A4");
        assert_eq!((make.as_str(), model.as_str()), ("Icom", "IC-705"));
        migrate(&mut make, &mut model, "ZZ");
        assert_eq!(model, "IC-705");
        let (mut make, mut model) = ("Yaesu".to_string(), String::new());
        migrate(&mut make, &mut model, "94");
        assert_eq!(model, "FTDX101D");
        // every listed model has a name in its make's menu
        for (make, name, _) in FIXED {
            assert!(model_names(make).contains(&name));
        }
        assert_eq!(model_names("Icom").last(), Some(&ICOM_OTHER));
    }

    #[test]
    fn elecraft_commands_and_power_format() {
        let k3 = Elecraft(10);
        assert_eq!(rig_commands(k3, Some(false), Some(false), Some(false), Some(5)), ["RA00;", "NB0;", "NR0;", "PC0500;"]);
        assert_eq!(rig_commands(k3, None, None, None, Some(50)), ["PC1000;"]); // clamped to 10 W, tenths
        assert_eq!(rig_commands(Elecraft(100), None, None, None, Some(15)), ["PC1500;"]);
        assert_eq!(rig_commands(Elecraft(100), None, None, None, Some(50)), ["PC0501;"]); // whole watts
        assert_eq!(describe(k3, &["PC0500;".to_string()]), "PWR 5w");
        assert_eq!(describe(Elecraft(100), &["PC0501;".to_string()]), "PWR 50w");
        assert!(confirms(k3, "FA00014074000;PC0500;", "PC0500;"));
        assert!(!confirms(k3, "PC0300;", "PC0500;"));
        assert!(confirms(Elecraft(100), "PC0501;", "PC0501;"));
        assert!(confirms(k3, "PC005;", "PC0500;")); // older firmware: whole watts, no suffix
        assert_eq!(query_for(k3, "RA01;"), "RA;");
        assert_eq!(resolve("Elecraft", "KX2 (12 W)", ""), Elecraft(12));
    }

    #[test]
    fn xiegu_uses_civ_at_its_own_address_and_power() {
        let g90 = resolve("Xiegu", "G90 (20 W)", "");
        assert_eq!(g90, Xiegu(0x88, 20));
        assert_eq!(rig_commands(g90, Some(true), Some(false), None, Some(20)), ["1112", "162200", "140A0255"]);
        assert_eq!(rig_commands(g90, None, None, None, Some(10)), ["140A0128"]);
        assert_eq!(wire(g90, "162201"), [0xFE, 0xFE, 0x88, 0xE0, 0x16, 0x22, 0x01, 0xFD]);
        assert!(confirms(g90, &normalize(g90, &[0xFE, 0xFE, 0xE0, 0x88, 0x11, 0x12, 0xFD]), "1112"));
        assert_eq!(g90.max_power(), 20);
        // an Icom at the same address keeps its own scale
        assert_eq!(rig_commands(Icom(0x88), None, None, None, Some(100)), ["140A0255"]);
    }

    #[test]
    fn power_limits_follow_the_model() {
        assert_eq!(rig_commands(Yaesu(200), None, None, None, Some(150)), ["PC150;"]);
        assert_eq!(rig_commands(Yaesu(100), None, None, None, Some(150)), ["PC100;"]);
        assert_eq!(rig_commands(Ts990, None, None, None, Some(200)), ["PC200;"]);
        assert_eq!(rig_commands(Icom(0x74), None, None, None, Some(200)), ["140A0255"]); // IC-7700
        assert_eq!(rig_commands(Icom(0x70), Some(true), None, None, None), ["1112"]); // IC-7000 12 dB
        // TS-990S: ATT is read as RA0n (main receiver)
        assert_eq!(query_for(Ts990, "RA01;"), "RA0;");
        assert!(confirms(Ts990, "RA01;", "RA01;"));
        assert!(!confirms(Ts990, "RA00;", "RA01;"));
    }
}
