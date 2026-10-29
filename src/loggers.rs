// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Sends each logged QSO to other logging programs as an ADIF record: Log4OM, DXKeeper,
//! HRD Logbook, N1MM Logger+ and Logger32 (see logger32.rs). The wire formats follow GridTracker (BSD-3-Clause), which
//! logs to the same programs; only protocol facts were taken, no code.

use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::events::{Event, Post};
use crate::hunter::{Tag, WorkedEntry};
use crate::wsjtx_udp::QsoLogged;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Log4om,
    Dxkeeper,
    Hrd,
    N1mm,
    Logger32,
}

pub const KINDS: [Kind; 5] = [Kind::Log4om, Kind::Dxkeeper, Kind::Hrd, Kind::N1mm, Kind::Logger32];

impl Kind {
    /// settings key and Settings > Logging row key
    pub fn key(self) -> &'static str {
        match self {
            Kind::Log4om => "log4om",
            Kind::Dxkeeper => "dxkeeper",
            Kind::Hrd => "hrd",
            Kind::N1mm => "n1mm",
            Kind::Logger32 => "logger32",
        }
    }

    /// False for Logger32, which is reached through Windows messages, not a host and port.
    pub fn uses_network(self) -> bool {
        self != Kind::Logger32
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Log4om => "Log4OM",
            Kind::Dxkeeper => "DXKeeper",
            Kind::Hrd => "HRD Logbook",
            Kind::N1mm => "N1MM Logger+",
            Kind::Logger32 => "Logger32",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Kind::Log4om => 2236,
            Kind::Dxkeeper => 52000,
            Kind::Hrd => 7826,
            Kind::N1mm => 2333,
            Kind::Logger32 => 0,
        }
    }

    /// What to switch on in the other program.
    pub fn help(self) -> &'static str {
        match self {
            Kind::Log4om => "In Log4OM 2, open Settings > Program Configuration > Software Integration > UDP Inbound (ADIF) and enable it, using the same port (default 2236).",
            Kind::Dxkeeper => "In DXKeeper, open Config > Defaults/Network Service and tick the network service, then enter its Base Port here (default 52000). DXKeeper listens on the base port plus 1, which is where QSOs are sent.",
            Kind::Hrd => "HRD Logbook must be running with its network (TCP) interface on, default port 7826. A QSO is sent as a db add command.",
            Kind::N1mm => "In N1MM Logger+, open Config > Configure Ports, Telnet/Packet Ports, Other Ports and Settings and tick Broadcast Data on the ADIF line (default port 2333).",
            Kind::Logger32 => "Logger32 must be running. It has no network port: QSOs go in through its External Interface (Windows messages), which needs no setup but allows only five outside programs at once, so close one if this reports that all slots are in use. Each QSO fills Logger32's entry window and logs it, so don't be typing in that window.",
        }
    }
}

/// Turns a field list into an ADIF record ending in <EOR>. Lengths are in bytes.
pub fn adif_line(fields: &[(&str, String)]) -> String {
    let mut s = String::new();
    for (k, v) in fields {
        s.push_str(&format!("<{k}:{}>{v} ", v.len()));
    }
    s.push_str("<EOR>");
    s
}

/// When the QSO started and ended.
pub fn qso_times(q: &QsoLogged, entry: &WorkedEntry) -> (DateTime<Utc>, DateTime<Utc>) {
    let t_on: DateTime<Utc> = q.time_on.or(q.time_off).or(entry.time).unwrap_or_else(Utc::now);
    (t_on, q.time_off.unwrap_or(t_on))
}

/// The station's call as WSJT-X logged it (/M and all), upper case.
pub fn qso_call(q: &QsoLogged, entry: &WorkedEntry) -> String {
    if q.dx_call.is_empty() { &entry.call } else { &q.dx_call }.to_uppercase()
}

/// WSJT-X's comment with " POTA <ref> <park name>" added when the park is confirmed.
pub fn qso_comment(q: &QsoLogged, entry: &WorkedEntry) -> String {
    if entry.ref_.is_empty() {
        return q.comments.clone();
    }
    let pota = format!("POTA {} {}", entry.ref_, entry.park).trim().to_string();
    format!("{} {pota}", q.comments).trim().to_string()
}

/// The Tx frequency in MHz with six decimals, empty when WSJT-X gave none.
pub fn qso_freq_mhz(q: &QsoLogged) -> String {
    if q.tx_freq != 0 { format!("{:.6}", q.tx_freq as f64 / 1e6) } else { String::new() }
}

/// The QSO as ADIF fields; empty values are left out.
pub fn adif_fields(q: &QsoLogged, entry: &WorkedEntry) -> Vec<(&'static str, String)> {
    let (t_on, t_off) = qso_times(q, entry);
    let (call, comment) = (qso_call(q, entry), qso_comment(q, entry));
    // ADIF has FT4 as a submode of MFSK
    let (mode, submode) = match entry.mode.as_str() {
        "FT4" => ("MFSK", "FT4"),
        m => (m, ""),
    };
    let freq = qso_freq_mhz(q);
    let mut f: Vec<(&'static str, String)> = vec![
        ("CALL", call),
        ("QSO_DATE", t_on.format("%Y%m%d").to_string()),
        ("TIME_ON", t_on.format("%H%M%S").to_string()),
        ("QSO_DATE_OFF", t_off.format("%Y%m%d").to_string()),
        ("TIME_OFF", t_off.format("%H%M%S").to_string()),
        ("BAND", entry.band.clone()),
        ("FREQ", freq),
        ("MODE", mode.to_string()),
        ("SUBMODE", submode.to_string()),
        ("RST_SENT", q.report_sent.clone()),
        ("RST_RCVD", q.report_rcvd.clone()),
        ("GRIDSQUARE", q.dx_grid.clone()),
        ("NAME", q.name.clone()),
        ("TX_PWR", q.tx_power.clone()),
        ("STATION_CALLSIGN", q.my_call.clone()),
        ("OPERATOR", q.operator_call.clone()),
        ("MY_GRIDSQUARE", q.my_grid.clone()),
        ("COMMENT", comment),
    ];
    if !entry.ref_.is_empty() {
        f.push(("SIG", "POTA".into()));
        f.push(("SIG_INFO", entry.ref_.clone()));
    }
    f.retain(|(_, v)| !v.trim().is_empty());
    for (_, v) in f.iter_mut() {
        *v = v.replace(['\r', '\n'], " ");
    }
    f
}

/// The bytes to send, and the port to send them to (the setting's port, or +1 for DXKeeper).
pub fn payload(kind: Kind, fields: &[(&str, String)], port: u16) -> (Vec<u8>, u16) {
    let adif = adif_line(fields);
    match kind {
        Kind::N1mm | Kind::Logger32 => (adif.into_bytes(), port), // (Logger32 doesn't use this)
        Kind::Log4om => (format!("ADD {adif}").into_bytes(), port),
        Kind::Dxkeeper => {
            let cmd = format!("<command:3>log <parameters:{}>{adif}\r\n", adif.len());
            (cmd.into_bytes(), port.saturating_add(1))
        }
        Kind::Hrd => {
            let mut cmd = String::from("ver\rdb add {");
            for (k, v) in fields {
                let v = if *k == "FREQ" { v.replace('.', "") } else { v.replace('"', "'") };
                cmd.push_str(&format!("{k}=\"{v}\" "));
            }
            cmd.push_str("}\rexit\r");
            (cmd.into_bytes(), port)
        }
    }
}

fn deliver(kind: Kind, host: &str, bytes: &[u8], port: u16) -> std::io::Result<()> {
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| std::io::Error::other("no address"))?;
    match kind {
        Kind::Logger32 => return Err(std::io::Error::other("Logger32 has no network port")),
        Kind::Log4om | Kind::N1mm => {
            let s = UdpSocket::bind("0.0.0.0:0")?;
            s.send_to(bytes, addr)?;
        }
        Kind::Dxkeeper | Kind::Hrd => {
            let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
            s.set_write_timeout(Some(Duration::from_secs(5)))?;
            s.write_all(bytes)?;
            s.flush()?;
        }
    }
    Ok(())
}

/// Sends one QSO to `kind` on a worker thread; the result is posted as an `Event::Aclog` line.
pub fn log_qso(post: Post, kind: Kind, host: String, port: u16, q: QsoLogged, entry: WorkedEntry) {
    std::thread::Builder::new()
        .name(kind.key().into())
        .spawn(move || {
            if kind == Kind::Logger32 {
                match crate::logger32::send(&crate::logger32::records(&q, &entry)) {
                    Ok(()) => post(Event::Aclog(format!("Logger32: sent {} {} {}", entry.call, entry.band, entry.mode), Tag::Worked)),
                    Err(e) => post(Event::Aclog(format!("Logger32: {e}"), Tag::Error)),
                }
                return;
            }
            let (bytes, to_port) = payload(kind, &adif_fields(&q, &entry), port);
            match deliver(kind, &host, &bytes, to_port) {
                Ok(()) => post(Event::Aclog(format!("{}: sent {} {} {}", kind.name(), entry.call, entry.band, entry.mode), Tag::Worked)),
                Err(e) => post(Event::Aclog(format!("{} not reachable at {host}:{to_port}: {e}", kind.name()), Tag::Error)),
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> (QsoLogged, WorkedEntry) {
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 17, 5, 30).unwrap();
        let q = QsoLogged {
            id: "WSJT-X".into(), time_off: Some(off), dx_call: "k1abc/p".into(), dx_grid: "FN42".into(),
            tx_freq: 14_075_500, mode: "FT8".into(), report_sent: "-10".into(), report_rcvd: "-12".into(),
            time_on: Some(off - chrono::Duration::minutes(2)), my_call: "WD4DAN".into(), ..Default::default()
        };
        (q, WorkedEntry::new("K1ABC/P", "20m", "FT8", Some(off), "US-1234", "Test Park"))
    }

    #[test]
    fn adif_record() {
        let (q, e) = sample();
        let a = adif_line(&adif_fields(&q, &e));
        for part in ["<CALL:7>K1ABC/P", "<QSO_DATE:8>20261024", "<TIME_ON:6>170330", "<TIME_OFF:6>170530",
                     "<BAND:3>20m", "<FREQ:9>14.075500", "<MODE:3>FT8", "<SIG:4>POTA", "<SIG_INFO:7>US-1234",
                     "<COMMENT:", "<STATION_CALLSIGN:6>WD4DAN"] {
            assert!(a.contains(part), "{part} missing from {a}");
        }
        assert!(a.ends_with("<EOR>"));
        assert!(!a.contains("<NAME"));
    }

    #[test]
    fn ft4_is_mfsk() {
        let (q, mut e) = sample();
        e.mode = "FT4".into();
        let a = adif_line(&adif_fields(&q, &e));
        assert!(a.contains("<MODE:4>MFSK") && a.contains("<SUBMODE:3>FT4"));
    }

    #[test]
    fn wire_formats() {
        let (q, e) = sample();
        let f = adif_fields(&q, &e);
        let adif = adif_line(&f);
        assert_eq!(payload(Kind::N1mm, &f, 2333), (adif.clone().into_bytes(), 2333));
        assert_eq!(payload(Kind::Log4om, &f, 2236).0, format!("ADD {adif}").into_bytes());
        let (dx, port) = payload(Kind::Dxkeeper, &f, 52000);
        assert_eq!(port, 52001);
        assert!(String::from_utf8(dx).unwrap().starts_with(&format!("<command:3>log <parameters:{}><CALL:7>", adif.len())));
        let (hrd, port) = payload(Kind::Hrd, &f, 7826);
        let hrd = String::from_utf8(hrd).unwrap();
        assert_eq!(port, 7826);
        assert!(hrd.starts_with("ver\rdb add {CALL=\"K1ABC/P\" ") && hrd.ends_with("}\rexit\r"));
        assert!(hrd.contains("FREQ=\"14075500\""));
    }
}
