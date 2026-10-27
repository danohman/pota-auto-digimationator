// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! N3FJP ACLog TCP API client (ACLog: Settings > Application Program Interface).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use regex::Regex;

use crate::events::{Event, Post};
use crate::hunter::{Tag, WorkedEntry};
use crate::wsjtx_udp::QsoLogged;

pub const SIG_INFO_TITLE: &str = "SIG_INFO";

/// ACLog parses tags, so a stray < or > in a value would break the command.
fn clean(value: &str) -> String {
    value.replace(['<', '>'], "").trim().to_string()
}

/// "20m" -> "20" (ACLog's band format).
pub fn aclog_band(band: &str) -> &str {
    band.strip_suffix('m').unwrap_or(band)
}

/// ADDDIRECT command for a WSJT-X QsoLogged message plus park info.
pub fn build_add_command(q: &QsoLogged, entry: &WorkedEntry, park_field: Option<&str>) -> String {
    let t_on: DateTime<Utc> = q.time_on.or(q.time_off).or(entry.time).unwrap_or_else(Utc::now);
    let t_off = q.time_off.unwrap_or(t_on);
    let mut comments = q.comments.clone();
    if !entry.ref_.is_empty() {
        let pota = format!("POTA {} {}", entry.ref_, entry.park).trim().to_string();
        comments = format!("{comments} {pota}").trim().to_string();
    }
    let call = if q.dx_call.is_empty() { &entry.call } else { &q.dx_call }.to_uppercase();
    let freq = if q.tx_freq != 0 { format!("{:.6}", q.tx_freq as f64 / 1e6) } else { String::new() };
    let mut fields: Vec<(&str, String)> = vec![
        ("fldCall", call), // as WSJT-X logged it, /M and all
        ("fldDateStr", t_on.format("%Y/%m/%d").to_string()),
        ("fldTimeOnStr", t_on.format("%H:%M:%S").to_string()),
        ("fldTimeOffStr", t_off.format("%H:%M:%S").to_string()),
        ("fldBand", aclog_band(&entry.band).to_string()),
        ("fldMode", entry.mode.clone()),
        ("fldModeContest", "DIG".into()),
        ("fldFrequency", freq),
        ("fldRstS", q.report_sent.clone()),
        ("fldRstR", q.report_rcvd.clone()),
        ("fldGridR", q.dx_grid.clone()),
        ("fldPower", q.tx_power.clone()),
        ("fldNameR", q.name.clone()),
        ("fldComments", comments),
    ];
    if let Some(f) = park_field.filter(|f| !f.is_empty()) {
        if !entry.ref_.is_empty() {
            fields.push((f, entry.ref_.clone()));
        }
    }
    let body: String = fields
        .iter()
        .map(|(k, v)| (k, clean(v)))
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("<{k}>{v}</{k}>"))
        .collect();
    format!("<CMD><ADDDIRECT><EXCLUDEDUPES>TRUE</EXCLUDEDUPES><STAYOPEN>FALSE</STAYOPEN>{body}</CMD>")
}

/// "txtEntryOther1: SIG_INFO" lines -> {"SIG_INFO": "fldOther1", ...}.
pub fn parse_other_titles(response: &str) -> std::collections::HashMap<String, String> {
    let re = Regex::new(r"(?i)txtEntryOther(\d)\s*:\s*([^\r\n<]*)").unwrap();
    re.captures_iter(response)
        .map(|c| (c[2].trim().to_uppercase(), format!("fldOther{}", &c[1])))
        .collect()
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

/// Sends QSOs to ACLog on worker threads; results are posted as `Event::Aclog`.
#[derive(Clone)]
pub struct AclogClient {
    post: Post,
    addr: Arc<Mutex<(String, u16)>>,
    park_field: Arc<Mutex<Option<String>>>,
    lock: Arc<Mutex<()>>,
}

impl AclogClient {
    pub fn new(post: Post, host: &str, port: u16) -> Self {
        AclogClient {
            post,
            addr: Arc::new(Mutex::new((host.to_string(), port))),
            park_field: Arc::new(Mutex::new(None)),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn set_addr(&self, host: &str, port: u16) {
        *self.addr.lock().unwrap() = (host.to_string(), port);
    }

    fn where_(&self) -> String {
        let (h, p) = self.addr.lock().unwrap().clone();
        format!("{h}:{p}")
    }

    fn send(&self, command: &str, want_reply: bool) -> std::io::Result<String> {
        let (host, port) = self.addr.lock().unwrap().clone();
        let addr = (host.as_str(), port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address"))?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(format!("{command}\r\n").as_bytes())?;
        if !want_reply {
            return Ok(String::new());
        }
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        loop {
            if String::from_utf8_lossy(&buf).trim_end().ends_with("</CMD>") {
                break;
            }
            match s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break, // timeout
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn find_park_field(&self) -> std::io::Result<String> {
        if let Some(f) = self.park_field.lock().unwrap().clone() {
            return Ok(f);
        }
        let reply = self.send("<CMD><GETOTHERFIELDTITLES></CMD>", true)?;
        let f = parse_other_titles(&reply).get(SIG_INFO_TITLE).cloned().unwrap_or_default();
        *self.park_field.lock().unwrap() = Some(f.clone());
        Ok(f)
    }

    /// Send CAT commands through ACLog's SENDRIGCOMMAND on a worker thread (or right
    /// here with wait). ACLog can't return the rig's answer, so state can't be read.
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
            std::thread::Builder::new()
                .name("aclog-rig".into())
                .spawn(move || me.do_set_rig(&cmds, &why, quiet))
                .ok();
        }
    }

    fn do_set_rig(&self, cmds: &[String], why: &str, quiet: bool) {
        let _g = self.lock.lock().unwrap();
        let result = (|| -> std::io::Result<()> {
            // ACLog passes a stack of CAT commands in one SENDRIGCOMMAND (live-tested 2026-10-27)
            self.send(&format!("<CMD><SENDRIGCOMMAND><VALUE>{}</VALUE></CMD>", cmds.concat()), false)?;
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
            (self.post)(Event::Aclog(format!("CAT:Yaesu: {}", items.join(" / ")), Tag::Info)); // Current Info line once sent
            Ok(())
        })();
        match result {
            Ok(()) if !quiet => (self.post)(Event::Aclog(why.to_string(), Tag::Info)),
            Ok(()) => {}
            Err(e) => (self.post)(Event::Aclog(format!("ACLog not reachable at {}: {e}", self.where_()), Tag::Error)),
        }
    }

    pub fn log_qso(&self, q: QsoLogged, entry: WorkedEntry) {
        let me = self.clone();
        std::thread::Builder::new().name("aclog".into()).spawn(move || me.do_log_qso(&q, &entry)).ok();
    }

    fn do_log_qso(&self, q: &QsoLogged, entry: &WorkedEntry) {
        let _g = self.lock.lock().unwrap();
        let result = (|| -> std::io::Result<String> {
            let park_field = self.find_park_field()?;
            self.send(&build_add_command(q, entry, Some(&park_field)), false)?;
            self.send("<CMD><CHECKLOG></CMD>", false)?;
            Ok(park_field)
        })();
        match result {
            Ok(field) => {
                let where_ = if entry.ref_.is_empty() {
                    String::new()
                } else {
                    let f = if field.is_empty() { "comments" } else { &field };
                    format!(" ({} in {f})", entry.ref_)
                };
                (self.post)(Event::Aclog(format!("ACLog: sent {} {} {}{where_}", entry.call, entry.band, entry.mode), Tag::Worked));
            }
            Err(e) => (self.post)(Event::Aclog(format!("ACLog not reachable at {}: {e}", self.where_()), Tag::Error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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
    }

    #[test]
    fn add_command() {
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 17, 5, 30).unwrap();
        let q = QsoLogged {
            id: "WSJT-X".into(), time_off: Some(off), dx_call: "K1ABC/P".into(), dx_grid: "FN42".into(),
            tx_freq: 14_075_500, mode: "FT8".into(), report_sent: "-10".into(), report_rcvd: "-12".into(),
            tx_power: "100".into(), time_on: Some(off - chrono::Duration::minutes(2)), ..Default::default()
        };
        let e = WorkedEntry::new("K1ABC/P", "20m", "FT8", Some(off), "US-1234", "Test <State> Park");
        let cmd = build_add_command(&q, &e, Some("fldOther1"));
        assert!(cmd.starts_with("<CMD><ADDDIRECT><EXCLUDEDUPES>TRUE</EXCLUDEDUPES>"));
        for part in ["<fldCall>K1ABC/P</fldCall>", "<fldDateStr>2026/10/24</fldDateStr>",
                     "<fldTimeOnStr>17:03:30</fldTimeOnStr>", "<fldTimeOffStr>17:05:30</fldTimeOffStr>",
                     "<fldBand>20</fldBand>", "<fldMode>FT8</fldMode>",
                     "<fldFrequency>14.075500</fldFrequency>", "<fldGridR>FN42</fldGridR>",
                     "<fldOther1>US-1234</fldOther1>",
                     "<fldComments>POTA US-1234 Test State Park</fldComments>"] {
            assert!(cmd.contains(part), "{part} missing from {cmd}");
        }
        assert!(!cmd.contains("fldNameR"));
    }

    #[test]
    fn call_keeps_the_suffix_wsjtx_logged() {
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 17, 5, 30).unwrap();
        let q = QsoLogged { id: "WSJT-X".into(), time_off: Some(off), dx_call: "N1KM/M".into(),
                            tx_freq: 7_074_000, mode: "FT8".into(), ..Default::default() };
        let e = WorkedEntry::new("N1KM", "40m", "FT8", Some(off), "", "");
        assert!(build_add_command(&q, &e, None).contains("<fldCall>N1KM/M</fldCall>"));
    }

    #[test]
    fn other_titles() {
        let reply = "<CMD><OTHERFIELDTITLESRESPONSE><VALUE>\ntxtEntryOther1: SIG_INFO\n\
                     txtEntryOther2: MY_SIG_INFO\ntxtEntryOther4: Other4\n</VALUE></CMD>";
        let t = parse_other_titles(reply);
        assert_eq!(t["SIG_INFO"], "fldOther1");
        assert_eq!(t["MY_SIG_INFO"], "fldOther2");
    }
}
