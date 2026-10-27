// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! WSJT-X UDP protocol (NetworkMessage.hpp) codec and multicast listener.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};

use crate::events::{Event, Post};

pub const MAGIC: u32 = 0xADBC_CBDA;
pub const SCHEMA: u32 = 2;

pub const HEARTBEAT: u32 = 0;
pub const STATUS: u32 = 1;
pub const DECODE: u32 = 2;
pub const REPLY: u32 = 4;
pub const QSO_LOGGED: u32 = 5;
pub const CLOSE: u32 = 6;
pub const HALT_TX: u32 = 8;
pub const FREE_TEXT: u32 = 9;

/// Reply modifiers: keys "held" during the double-click. Ctrl moves Tx to the
/// decode's frequency too, even with Hold Tx Freq checked.
pub const MOD_NONE: u8 = 0x00;
pub const MOD_CTRL: u8 = 0x04;

const NULL_LEN: u32 = 0xFFFF_FFFF;
/// Julian day number of 0001-01-01 relative to the proleptic ordinal 1.
const JD_OFFSET: i64 = 1_721_425;

#[derive(Debug)]
pub struct Eof;

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Eof> {
        if self.pos + n > self.data.len() {
            return Err(Eof);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, Eof> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, Eof> {
        Ok(self.u8()? != 0)
    }

    pub fn u32(&mut self) -> Result<u32, Eof> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i32(&mut self) -> Result<i32, Eof> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, Eof> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, Eof> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn double(&mut self) -> Result<f64, Eof> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn utf8(&mut self) -> Result<String, Eof> {
        let n = self.u32()?;
        if n == NULL_LEN {
            return Ok(String::new());
        }
        let bytes = self.take(n as usize)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// QTime -> milliseconds since midnight (None if invalid).
    pub fn qtime(&mut self) -> Result<Option<u32>, Eof> {
        let ms = self.u32()?;
        Ok(if ms == NULL_LEN { None } else { Some(ms) })
    }

    /// QDateTime -> UTC datetime (None if invalid).
    pub fn qdatetime(&mut self) -> Result<Option<DateTime<Utc>>, Eof> {
        let jd = self.i64()?;
        let ms = self.u32()?;
        let spec = self.u8()?;
        let offset = if spec == 2 { self.i32()? } else { 0 };
        if jd <= 0 || ms == NULL_LEN {
            return Ok(None);
        }
        let Some(date) = NaiveDate::from_num_days_from_ce_opt((jd - JD_OFFSET) as i32) else {
            return Ok(None);
        };
        let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
        Ok(Some(
            midnight + chrono::Duration::milliseconds(ms as i64)
                - chrono::Duration::seconds(offset as i64),
        ))
    }
}

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }

    pub fn bool(self, v: bool) -> Self {
        self.u8(v as u8)
    }

    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i32(mut self, v: i32) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i64(mut self, v: i64) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn double(mut self, v: f64) -> Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn utf8(self, s: Option<&str>) -> Self {
        match s {
            None => self.u32(NULL_LEN),
            Some(s) => {
                let mut w = self.u32(s.len() as u32);
                w.buf.extend_from_slice(s.as_bytes());
                w
            }
        }
    }

    pub fn str(self, s: &str) -> Self {
        self.utf8(Some(s))
    }

    pub fn qtime(self, ms: Option<u32>) -> Self {
        self.u32(ms.unwrap_or(NULL_LEN))
    }

    pub fn qdatetime(self, dt: DateTime<Utc>) -> Self {
        let jd = dt.date_naive().num_days_from_ce() as i64 + JD_OFFSET;
        let t = dt.time();
        use chrono::Timelike;
        let ms = ((t.hour() * 60 + t.minute()) * 60 + t.second()) * 1000 + t.nanosecond() / 1_000_000;
        self.i64(jd).u32(ms).u8(1) // Qt::UTC
    }

    pub fn bytes(self) -> Vec<u8> {
        self.buf
    }
}

use chrono::Datelike;

pub fn header(msg_type: u32, client_id: &str) -> Writer {
    Writer::default().u32(MAGIC).u32(SCHEMA).u32(msg_type).str(client_id)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub id: String,
    pub dial_freq: u64,
    pub mode: String,
    pub dx_call: String,
    pub report: String,
    pub tx_mode: String,
    pub tx_enabled: bool,
    pub transmitting: bool,
    pub decoding: bool,
    pub rx_df: u32,
    pub tx_df: u32,
    pub de_call: String,
    pub de_grid: String,
    pub dx_grid: String,
    pub tx_watchdog: bool,
    pub sub_mode: String,
    pub fast_mode: bool,
    pub special_op_mode: u8,
    pub freq_tolerance: u32,
    pub tr_period: u32,
    pub config_name: String,
    pub tx_message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decode {
    pub id: String,
    pub new: bool,
    pub time_ms: Option<u32>,
    pub snr: i32,
    pub dt: f64,
    pub df: u32,
    pub mode: String,
    pub message: String,
    pub low_confidence: bool,
    pub off_air: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct QsoLogged {
    pub id: String,
    pub time_off: Option<DateTime<Utc>>,
    pub dx_call: String,
    pub dx_grid: String,
    pub tx_freq: u64,
    pub mode: String,
    pub report_sent: String,
    pub report_rcvd: String,
    pub tx_power: String,
    pub comments: String,
    pub name: String,
    pub time_on: Option<DateTime<Utc>>,
    pub operator_call: String,
    pub my_call: String,
    pub my_grid: String,
    pub exchange_sent: String,
    pub exchange_rcvd: String,
    pub prop_mode: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Heartbeat { id: String },
    Status(Status),
    Decode(Decode),
    QsoLogged(QsoLogged),
    Close { id: String },
    Other { id: String, kind: u32 },
}

/// Reads optional trailing fields; older WSJT-X versions omit some. Each line
/// stops the parse quietly at the end of the data.
macro_rules! fill {
    ($r:ident, $obj:ident, $($field:ident => $read:ident),+ $(,)?) => {
        'fill: {
            $(
                match $r.$read() {
                    Ok(v) => $obj.$field = v,
                    Err(_) => break 'fill,
                }
            )+
        }
    };
}

/// Parse one datagram. Returns a message, or None if it isn't WSJT-X.
pub fn parse(data: &[u8]) -> Option<Message> {
    let mut r = Reader::new(data);
    if r.u32().ok()? != MAGIC {
        return None;
    }
    r.u32().ok()?; // schema
    let msg_type = r.u32().ok()?;
    let id = r.utf8().ok()?;
    match msg_type {
        HEARTBEAT => Some(Message::Heartbeat { id }),
        STATUS => {
            let mut s = Status { id, ..Default::default() };
            fill!(r, s,
                dial_freq => u64, mode => utf8, dx_call => utf8, report => utf8,
                tx_mode => utf8, tx_enabled => bool, transmitting => bool, decoding => bool,
                rx_df => u32, tx_df => u32, de_call => utf8, de_grid => utf8, dx_grid => utf8,
                tx_watchdog => bool, sub_mode => utf8, fast_mode => bool,
                special_op_mode => u8, freq_tolerance => u32, tr_period => u32,
                config_name => utf8, tx_message => utf8);
            Some(Message::Status(s))
        }
        DECODE => {
            let mut d = Decode {
                id,
                new: r.bool().ok()?,
                time_ms: r.qtime().ok()?,
                snr: r.i32().ok()?,
                dt: r.double().ok()?,
                df: r.u32().ok()?,
                mode: r.utf8().ok()?,
                message: r.utf8().ok()?,
                low_confidence: false,
                off_air: false,
            };
            fill!(r, d, low_confidence => bool, off_air => bool);
            Some(Message::Decode(d))
        }
        QSO_LOGGED => {
            let mut q = QsoLogged { id, ..Default::default() };
            fill!(r, q,
                time_off => qdatetime, dx_call => utf8, dx_grid => utf8, tx_freq => u64,
                mode => utf8, report_sent => utf8, report_rcvd => utf8, tx_power => utf8,
                comments => utf8, name => utf8, time_on => qdatetime, operator_call => utf8,
                my_call => utf8, my_grid => utf8, exchange_sent => utf8, exchange_rcvd => utf8,
                prop_mode => utf8);
            Some(Message::QsoLogged(q))
        }
        CLOSE => Some(Message::Close { id }),
        kind => Some(Message::Other { id, kind }),
    }
}

/// Reply(4): makes WSJT-X act as if the operator double-clicked `decode`.
pub fn build_reply(d: &Decode, modifiers: u8) -> Vec<u8> {
    header(REPLY, &d.id)
        .qtime(d.time_ms)
        .i32(d.snr)
        .double(d.dt)
        .u32(d.df)
        .str(&d.mode)
        .str(&d.message)
        .bool(d.low_confidence)
        .u8(modifiers)
        .bytes()
}

pub fn build_halt_tx(client_id: &str, auto_only: bool) -> Vec<u8> {
    header(HALT_TX, client_id).bool(auto_only).bytes()
}

/// FreeText(9): puts `text` in WSJT-X's free-text (Tx5) box; with `send` it also
/// enables Tx. Kept for reference: it neither enables Tx nor reliably fills Tx5.
#[allow(dead_code)]
pub fn build_free_text(client_id: &str, text: &str, send: bool) -> Vec<u8> {
    header(FREE_TEXT, client_id).str(text).bool(send).bytes()
}

/// What the listener thread shares with the app: where WSJT-X sends from, and
/// the socket to answer it on.
pub struct ListenerShared {
    pub group: String,
    pub port: u16,
    sock: Mutex<Option<UdpSocket>>,
    pub wsjtx_addr: Mutex<Option<SocketAddr>>,
    stop: AtomicBool,
}

impl ListenerShared {
    /// Send a datagram to WSJT-X (unicast back to the address it sends from).
    pub fn send(&self, payload: &[u8]) -> Result<(), String> {
        let addr = *self.wsjtx_addr.lock().unwrap();
        let sock = self.sock.lock().unwrap();
        match (sock.as_ref(), addr) {
            (Some(s), Some(a)) => s.send_to(payload, a).map(|_| ()).map_err(|e| e.to_string()),
            _ => Err("WSJT-X address not known yet (no packets received)".into()),
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// A listener that never listens (the setup wizard runs before anything is started);
/// `App::begin_operation` swaps in a real one.
pub fn idle_listener(group: &str, port: u16) -> Arc<ListenerShared> {
    Arc::new(ListenerShared {
        group: group.to_string(),
        port,
        sock: Mutex::new(None),
        wsjtx_addr: Mutex::new(None),
        stop: AtomicBool::new(true),
    })
}

/// Receives WSJT-X multicast datagrams and posts `Event::Udp` messages. WSJT-X sends
/// each datagram on every configured interface, so the same packet can arrive
/// twice; exact duplicates within 2 s are dropped.
pub fn start_listener(post: Post, group: &str, port: u16) -> Arc<ListenerShared> {
    let shared = Arc::new(ListenerShared {
        group: group.to_string(),
        port,
        sock: Mutex::new(None),
        wsjtx_addr: Mutex::new(None),
        stop: AtomicBool::new(false),
    });
    let sh = shared.clone();
    std::thread::Builder::new()
        .name("wsjtx-udp".into())
        .spawn(move || run(sh, post))
        .expect("spawn wsjtx-udp");
    shared
}

fn open(group: &str, port: u16) -> std::io::Result<(UdpSocket, Vec<&'static str>)> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    let mut joined = Vec::new();
    let first: Option<u8> = group.split('.').next().and_then(|x| x.parse().ok());
    if let (Some(first), Ok(g)) = (first, group.parse::<Ipv4Addr>()) {
        if (224..=239).contains(&first) {
            for (name, iface) in [("127.0.0.1", Ipv4Addr::LOCALHOST), ("0.0.0.0", Ipv4Addr::UNSPECIFIED)] {
                if s.join_multicast_v4(&g, &iface).is_ok() {
                    joined.push(name);
                }
            }
            if joined.is_empty() {
                return Err(std::io::Error::other(format!("could not join multicast group {group}")));
            }
        }
    }
    s.set_read_timeout(Some(Duration::from_secs(1)))?;
    Ok((s.into(), joined))
}

fn run(sh: Arc<ListenerShared>, post: Post) {
    let (sock, joined) = match open(&sh.group, sh.port) {
        Ok(x) => x,
        Err(e) => {
            post(Event::UdpError(format!("UDP socket error: {e}")));
            return;
        }
    };
    let via = if joined.is_empty() { "unicast".to_string() } else { joined.join(", ") };
    post(Event::UdpInfo(format!("Listening on {}:{} (joined via {via})", sh.group, sh.port)));
    match sock.try_clone() {
        Ok(c) => *sh.sock.lock().unwrap() = Some(c),
        Err(e) => post(Event::UdpError(format!("UDP socket error: {e}"))),
    }
    let mut recent: HashMap<u64, Instant> = HashMap::new();
    let mut buf = vec![0u8; 65535];
    while !sh.stop.load(Ordering::SeqCst) {
        let (n, addr) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => {
                if sh.stop.load(Ordering::SeqCst) {
                    break;
                }
                // Windows reports an ICMP "port unreachable" from an earlier send as an error here.
                if e.raw_os_error() == Some(10054) {
                    continue;
                }
                post(Event::UdpError(format!("UDP receive error: {e}")));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let data = &buf[..n];
        if is_duplicate(&mut recent, data) {
            continue;
        }
        let Some(msg) = parse(data) else { continue };
        {
            // Prefer the loopback source when WSJT-X is seen on several interfaces.
            let mut a = sh.wsjtx_addr.lock().unwrap();
            let loopback = matches!(addr, SocketAddr::V4(v) if *v.ip() == Ipv4Addr::LOCALHOST);
            if a.is_none() || loopback || a.unwrap().port() != addr.port() {
                *a = Some(addr);
            }
        }
        post(Event::Udp(msg));
    }
    *sh.sock.lock().unwrap() = None;
}

fn is_duplicate(recent: &mut HashMap<u64, Instant>, data: &[u8]) -> bool {
    let now = Instant::now();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut h);
    data.len().hash(&mut h);
    let key = h.finish();
    let seen = recent.insert(key, now);
    if recent.len() > 500 {
        recent.retain(|_, t| now.duration_since(*t) < Duration::from_secs(2));
    }
    matches!(seen, Some(t) if now.duration_since(t) < Duration::from_secs(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_packet(message: &str, snr: i32) -> Vec<u8> {
        header(DECODE, "WSJT-X")
            .bool(true)
            .qtime(Some(10_515_000))
            .i32(snr)
            .double(0.1)
            .u32(1500)
            .str("~")
            .str(message)
            .bool(false)
            .bool(false)
            .bytes()
    }

    #[test]
    fn decode_round_trip() {
        let Some(Message::Decode(d)) = parse(&decode_packet("CQ POTA K0UWU EM29", 11)) else {
            panic!("not a decode")
        };
        assert_eq!((d.id.as_str(), d.snr, d.df, d.mode.as_str(), d.message.as_str()),
                   ("WSJT-X", 11, 1500, "~", "CQ POTA K0UWU EM29"));
        assert!(d.new);
    }

    #[test]
    fn reply_layout() {
        let Some(Message::Decode(d)) = parse(&decode_packet("CQ K1ABC FN42", -5)) else { panic!() };
        let data = build_reply(&d, MOD_NONE);
        let mut r = Reader::new(&data);
        assert_eq!((r.u32().unwrap(), r.u32().unwrap(), r.u32().unwrap(), r.utf8().unwrap()),
                   (MAGIC, SCHEMA, REPLY, "WSJT-X".to_string()));
        assert_eq!(r.qtime().unwrap(), d.time_ms);
        assert_eq!(r.i32().unwrap(), d.snr);
        assert!((r.double().unwrap() - d.dt).abs() < 1e-12);
        assert_eq!(r.u32().unwrap(), d.df);
        assert_eq!((r.utf8().unwrap(), r.utf8().unwrap()), ("~".into(), "CQ K1ABC FN42".into()));
        assert_eq!((r.bool().unwrap(), r.u8().unwrap()), (false, 0));
        assert_eq!(r.pos, data.len());
    }

    #[test]
    fn reply_ctrl_modifier() {
        let Some(Message::Decode(d)) = parse(&decode_packet("CQ POTA K1ABC FN42", -5)) else { panic!() };
        let (plain, ctrl) = (build_reply(&d, MOD_NONE), build_reply(&d, MOD_CTRL));
        assert_eq!(plain[..plain.len() - 1], ctrl[..ctrl.len() - 1]);
        assert_eq!((plain[plain.len() - 1], ctrl[ctrl.len() - 1]), (0x00, 0x04));
    }

    #[test]
    fn halt_tx() {
        let data = build_halt_tx("WSJT-X", false);
        let mut r = Reader::new(&data);
        assert_eq!((r.u32().unwrap(), r.u32().unwrap(), r.u32().unwrap()), (MAGIC, SCHEMA, HALT_TX));
        assert_eq!(*data.last().unwrap(), 0);
    }

    #[test]
    fn qso_logged() {
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 17, 5, 30).unwrap();
        let on = off - chrono::Duration::minutes(2);
        let w = header(QSO_LOGGED, "WSJT-X").qdatetime(off).str("K1ABC/P").str("FN42")
            .u64(14_075_500).str("FT8").str("-10").str("-12").str("100").str("")
            .str("").qdatetime(on).str("").str("WD4DAN").str("EM80").str("").str("").str("");
        let Some(Message::QsoLogged(q)) = parse(&w.bytes()) else { panic!() };
        assert_eq!((q.dx_call.as_str(), q.tx_freq, q.mode.as_str(), q.report_sent.as_str()),
                   ("K1ABC/P", 14_075_500, "FT8", "-10"));
        assert_eq!((q.time_off, q.time_on), (Some(off), Some(on)));
    }

    #[test]
    fn status_with_null_strings() {
        let w = header(STATUS, "WSJT-X").u64(7_074_000).str("FT8").utf8(None).str("")
            .str("FT8").bool(true).bool(false).bool(false).u32(1000).u32(1200).str("WD4DAN");
        let Some(Message::Status(s)) = parse(&w.bytes()) else { panic!() };
        assert_eq!((s.dial_freq, s.mode.as_str(), s.dx_call.as_str(), s.tx_enabled, s.de_call.as_str()),
                   (7_074_000, "FT8", "", true, "WD4DAN"));
    }

    #[test]
    fn not_wsjtx() {
        assert!(parse(b"hello world, not wsjt-x").is_none());
    }
}
