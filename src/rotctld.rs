// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Client for Hamlib's rotctld rotator server (plain-text TCP, default port 4533).
//! `p` reads the position (two lines: azimuth, elevation); `P az el` turns the rotator and
//! answers `RPRT 0` (a negative number is a Hamlib error code).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);

fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("cannot resolve {host}"))?;
    let s = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(|e| format!("cannot reach rotctld at {host}:{port} ({e})"))?;
    let _ = s.set_read_timeout(Some(TIMEOUT));
    let _ = s.set_write_timeout(Some(TIMEOUT));
    Ok(s)
}

fn line(r: &mut impl BufRead) -> Result<String, String> {
    let mut s = String::new();
    match r.read_line(&mut s) {
        Ok(0) => Err("rotctld closed the connection".into()),
        Ok(_) => Ok(s.trim().to_string()),
        Err(e) => Err(format!("rotctld did not answer ({e})")),
    }
}

fn report(code: &str) -> Result<(), String> {
    match code.strip_prefix("RPRT ").and_then(|n| n.trim().parse::<i32>().ok()) {
        Some(0) => Ok(()),
        Some(n) => Err(format!("rotctld error {n}")),
        None => Err(format!("unexpected answer from rotctld: {code}")),
    }
}

/// The rotator's (azimuth, elevation) in degrees.
pub fn position(host: &str, port: u16) -> Result<(f64, f64), String> {
    let mut s = connect(host, port)?;
    s.write_all(b"p\n").map_err(|e| e.to_string())?;
    let mut r = BufReader::new(s);
    let az = line(&mut r)?;
    if az.starts_with("RPRT") {
        return report(&az).map(|_| (0.0, 0.0));
    }
    let el = line(&mut r)?;
    match (az.parse::<f64>(), el.parse::<f64>()) {
        (Ok(a), Ok(e)) => Ok((a, e)),
        _ => Err(format!("unexpected answer from rotctld: {az} {el}")),
    }
}

/// Turn the rotator to `az` degrees (elevation 0).
pub fn set_azimuth(host: &str, port: u16, az: f64) -> Result<(), String> {
    let mut s = connect(host, port)?;
    s.write_all(format!("P {:.1} 0\n", az.rem_euclid(360.0)).as_bytes()).map_err(|e| e.to_string())?;
    report(&line(&mut BufReader::new(s))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    /// A one-shot fake rotctld that answers with `reply` and returns what it was sent.
    fn fake(reply: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = c.read(&mut buf).unwrap();
            c.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (port, h)
    }

    #[test]
    fn reads_position() {
        let (port, h) = fake("123.5\n0.0\n");
        assert_eq!(position("127.0.0.1", port), Ok((123.5, 0.0)));
        assert_eq!(h.join().unwrap(), "p\n");
    }

    #[test]
    fn sets_azimuth_and_reports_errors() {
        let (port, h) = fake("RPRT 0\n");
        assert_eq!(set_azimuth("127.0.0.1", port, 370.0), Ok(()));
        assert_eq!(h.join().unwrap(), "P 10.0 0\n");
        let (port, _h) = fake("RPRT -1\n");
        assert!(set_azimuth("127.0.0.1", port, 90.0).is_err());
    }
}
