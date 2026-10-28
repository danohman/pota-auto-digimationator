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

#[derive(Clone, Debug, PartialEq)]
pub struct Cfg {
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

/// The query that reads back a set command: `NB00;` -> `NB0;`, `PC009;` -> `PC;`.
pub fn query_for(set: &str) -> String {
    if set.starts_with("PC") { "PC;".into() } else { format!("{};", &set[..set.len().min(3)]) }
}

/// True when `reply` holds the state `set` asked for.
pub fn confirms(reply: &str, set: &str) -> bool {
    reply.contains(set)
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

/// Read until `want` shows up in what has arrived or `wait` passes.
fn read_until(p: &mut dyn serialport::SerialPort, want: &str, wait: Duration) -> String {
    let end = Instant::now() + wait;
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while Instant::now() < end {
        match p.read(&mut buf) {
            Ok(n) if n > 0 => {
                got.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&got).contains(want) {
                    break;
                }
            }
            _ => {}
        }
    }
    String::from_utf8_lossy(&got).into_owned()
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
            p.write_all(c.as_bytes()).map_err(|e| format!("{}: {e}", cfg.port))?;
            std::thread::sleep(Duration::from_millis(150));
            let _ = p.clear(serialport::ClearBuffer::Input);
            p.write_all(query_for(c).as_bytes()).map_err(|e| format!("{}: {e}", cfg.port))?;
            let reply = read_until(p.as_mut(), c, ANSWER_WAIT);
            if confirms(&reply, c) {
                ok = true;
                break;
            }
        }
        out.push((c.clone(), ok));
    }
    Ok(out)
}



/// Ask for one setting and parse the digits after `key` (`NB0` -> `NB00;` gives 0,
/// `PC` -> `PC009;` gives 9), skipping the rig's other chatter.
fn read_value(p: &mut dyn serialport::SerialPort, key: &str, digits: usize) -> Option<u32> {
    let re = regex::Regex::new(&format!(r"{key}(\d{{{digits}}});")).ok()?;
    let query = if key == "PC" { "PC;".to_string() } else { format!("{key};") };
    for _ in 0..TRIES {
        let _ = p.clear(serialport::ClearBuffer::Input);
        p.write_all(query.as_bytes()).ok()?;
        let end = Instant::now() + ANSWER_WAIT;
        let mut got = String::new();
        let mut buf = [0u8; 256];
        while Instant::now() < end {
            if let Ok(n) = p.read(&mut buf) {
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(c) = re.captures_iter(&got).last() {
                    return c[1].parse().ok();
                }
            }
        }
    }
    None
}

/// Set one value, give the rig a moment, read it back.
fn set_and_read(p: &mut dyn serialport::SerialPort, set: &str, key: &str, digits: usize) -> Result<Option<u32>, String> {
    p.write_all(set.as_bytes()).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(200));
    Ok(read_value(p, key, digits))
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
    // (name, key, digits)
    for (name, key, digits) in [("NB", "NB0", 1usize), ("NR (DNR)", "NR0", 1), ("ATT", "RA0", 1), ("PWR", "PC", 3)] {
        let show = |v: u32| if key == "PC" { format!("{v} W") } else if v == 0 { "off".into() } else { "on".into() };
        let cmd = |v: u32| if key == "PC" { format!("PC{v:03};") } else { format!("{key}{v};") };
        let Some(orig) = read_value(p.as_mut(), key, digits) else {
            lines.push(format!("{name}: FAILED, the radio did not answer"));
            all_ok = false;
            continue;
        };
        let target = if key == "PC" { if orig < 100 { orig + 1 } else { orig - 1 } } else { 1 - orig.min(1) };
        let changed = set_and_read(p.as_mut(), &cmd(target), key, digits);
        let restored = set_and_read(p.as_mut(), &cmd(orig), key, digits); // always put it back
        let (ok, text) = match (changed, restored) {
            (Err(e), _) | (_, Err(e)) => (false, format!("FAILED, port error: {e}")),
            (Ok(c), Ok(r)) => {
                let c_ok = c == Some(target);
                let r_ok = r == Some(orig);
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

/// Yaesu FTDX101D CAT strings for the main receiver (RA = attenuator, NB, NR = DNR);
/// each argument is Some(true) (on), Some(false) (off) or None (leave alone).
/// `power` is the transmit power in watts (PC, three digits, 5 to 100 on the FTDX101D).
pub fn rig_commands(att: Option<bool>, nb: Option<bool>, dnr: Option<bool>, power: Option<u32>) -> Vec<String> {
    let mut v: Vec<String> = [("RA0", att), ("NB0", nb), ("NR0", dnr)]
        .iter()
        .filter_map(|(p, s)| s.map(|on| format!("{p}{};", on as u8)))
        .collect();
    if let Some(w) = power {
        v.push(format!("PC{:03};", w.clamp(5, 100)));
    }
    v
}

/// "ATT off", "PWR 9w"... for the Current Info line.
fn describe(cmds: &[String]) -> String {
    let items: Vec<String> = cmds
        .iter()
        .map(|c| {
            let onoff = if c[2..].starts_with("01") { "on" } else { "off" };
            match &c[..2] {
                "RA" => format!("ATT {onoff}"),
                "NB" => format!("NB {onoff}"),
                "NR" => format!("DNR {onoff}"),
                _ => format!("PWR {}w", c[2..].trim_end_matches(';').trim_start_matches('0')),
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
        let cmds = rig_commands(att, nb, dnr, power);
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
                    say(format!("CAT:Yaesu via {}, confirmed: {}", cfg.port, describe(&ok)), Tag::Info);
                }
                if bad.is_empty() {
                    if !quiet {
                        say(format!("{why} (confirmed by the radio on {})", cfg.port), Tag::Info);
                    }
                } else {
                    say(format!("The radio on {} did not confirm: {}", cfg.port, describe(&bad)), Tag::Error);
                }
            }
            Err(e) => say(format!("Radio serial port not usable, no CAT sent: {e}"), Tag::Error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_off() {
        assert_eq!(rig_commands(Some(false), Some(false), Some(false), None), ["RA00;", "NB00;", "NR00;"]);
    }

    #[test]
    fn mixed_and_leave() {
        assert_eq!(rig_commands(Some(true), None, Some(false), None), ["RA01;", "NR00;"]);
        assert!(rig_commands(None, None, None, None).is_empty());
    }

    #[test]
    fn power() {
        assert_eq!(rig_commands(None, None, None, Some(5)), ["PC005;"]);
        assert_eq!(rig_commands(None, None, None, Some(100)), ["PC100;"]);
        assert_eq!(rig_commands(None, None, None, Some(250)), ["PC100;"]);
        assert_eq!(describe(&["PC009;".to_string()]), "PWR 9w");
    }

    #[test]
    fn queries() {
        assert_eq!(query_for("NB00;"), "NB0;");
        assert_eq!(query_for("RA01;"), "RA0;");
        assert_eq!(query_for("NR00;"), "NR0;");
        assert_eq!(query_for("PC009;"), "PC;");
    }

    #[test]
    fn confirmation_scans_the_auto_info_noise() {
        assert!(confirms("VS0;NB01;IF503007074000+000000C00000;FA007074000;", "NB01;"));
        assert!(!confirms("NB00;VS0;", "NB01;"));
        assert!(!confirms("", "RA00;"));
    }
}
