// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Logs a QSO to Logger32 (K4CY). Logger32 has no network port for outside programs: its
//! "External Interface" (help topic "Using the Logger32 External Interface", Bob Furzer K4CY)
//! is a handshake of registered Windows messages, then ADIF text written into a TextBox
//! window that Logger32 hands out. Only protocol facts were taken from that document.
//!
//! 1. Find the top-level window titled "Logger32".
//! 2. `RegisterWindowMessage("Logger32 N")` for N = 1..5 (one slot per outside program) and
//!    post it with wParam 1, lParam = our window. Logger32 replies to our window with the same
//!    message, wParam 0, lParam 1 (taken) or 0 (slot busy, try the next).
//! 3. Post wParam 2, lParam 0 (we want no unsolicited text). Logger32 answers with wParam 3
//!    and lParam = the hWnd of a TextBox of its own.
//! 4. Write ADIF text to that box (WM_SETTEXT). `<EOR>` alone clears the entry window;
//!    `<APP_LOGQSO:1>Y` logs what the entry window now holds.
//! 5. Post wParam 4 to give the slot back.

use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows, FindWindowW, GetWindowTextW,
    IsWindowVisible, PeekMessageW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SendMessageTimeoutW,
    UnregisterClassW, MSG, PM_REMOVE, SMTO_ABORTIFHUNG, WM_SETTEXT, WNDCLASSW,
};

use crate::hunter::WorkedEntry;
use crate::loggers::{adif_fields, qso_times};
use crate::wsjtx_udp::QsoLogged;

const CLASS: &str = "PotaAutoDigimationatorLogger32";
const SLOTS: u32 = 5;
const REPLY_WAIT: Duration = Duration::from_secs(2);
const BOX_WAIT: Duration = Duration::from_secs(3);

/// What Logger32's replies left for us (the window procedure runs on this thread, inside
/// `pump`). `MSG` is the registered message of the slot being tried.
static MSG: AtomicU32 = AtomicU32::new(0);
static ACCEPT: AtomicIsize = AtomicIsize::new(-1);
static TEXT_BOX: AtomicIsize = AtomicIsize::new(0);
/// One session at a time: the state above is shared.
static SESSION: Mutex<()> = Mutex::new(());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// An OLE automation date (days since 1899-12-30), the "Microsoft time stamp" Logger32 wants.
pub fn ole_date(t: DateTime<Utc>) -> String {
    let days = t.timestamp() as f64 / 86400.0 + 25569.0;
    format!("{days:.10}")
}

/// The text to write into Logger32's box, in order: clear the entry window, then the fields
/// and the log command. Logger32's text boxes are ANSI, so anything else becomes `?`.
pub fn records(q: &QsoLogged, entry: &WorkedEntry) -> Vec<String> {
    let f = adif_fields(q, entry);
    let get = |k: &str| f.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let (on, off) = qso_times(q, entry);
    let mut out: Vec<(&str, String)> = Vec::new();
    for k in ["CALL", "RST_SENT", "RST_RCVD", "NAME", "GRIDSQUARE", "COMMENT"] {
        if let Some(v) = get(k) {
            out.push((k, v));
        }
    }
    out.push(("MODE", entry.mode.clone()));
    if q.tx_freq != 0 {
        out.push(("FREQ", format!("{:.3}", q.tx_freq as f64 / 1000.0)));
    }
    out.push(("APP_TIME_ON", ole_date(on)));
    out.push(("APP_TIME_OFF", ole_date(off)));
    out.push(("APP_LOGQSO", "Y".into()));
    let mut s = String::new();
    for (k, v) in out {
        let v: String = v.chars().map(|c| if c.is_ascii() && !c.is_ascii_control() { c } else { '?' }).collect();
        s.push_str(&format!("<{k}:{}>{v}", v.len()));
    }
    vec!["<EOR>".to_string(), s]
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg != 0 && msg == MSG.load(Ordering::SeqCst) {
        match wp {
            0 => ACCEPT.store(lp, Ordering::SeqCst),
            3 => TEXT_BOX.store(lp, Ordering::SeqCst),
            _ => {} // 100 = rig frequency; not wanted
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Runs this thread's message queue until `done` or `wait` has passed.
fn pump(wait: Duration, done: impl Fn() -> bool) -> bool {
    let end = Instant::now() + wait;
    loop {
        unsafe {
            let mut m: MSG = std::mem::zeroed();
            while PeekMessageW(&mut m, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                DispatchMessageW(&m);
            }
        }
        if done() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> i32 {
    unsafe {
        let mut buf = [0u16; 128];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
        let title = String::from_utf16_lossy(&buf[..n]);
        if IsWindowVisible(hwnd) != 0 && title.starts_with("Logger32") {
            *(lp as *mut isize) = hwnd as isize;
            return 0;
        }
        1
    }
}

/// The Logger32 main window: exactly titled "Logger32" per the documentation, else any
/// visible top-level window whose title starts with it (a log name may be appended).
fn find_logger32() -> HWND {
    unsafe {
        let h = FindWindowW(std::ptr::null(), wide("Logger32").as_ptr());
        if !h.is_null() {
            return h;
        }
        let mut found: isize = 0;
        EnumWindows(Some(collect), &mut found as *mut isize as LPARAM);
        found as HWND
    }
}

/// Writes `texts` into Logger32's entry window, the last one logging the QSO.
pub fn send(texts: &[String]) -> Result<(), String> {
    let _one = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        let l32 = find_logger32();
        if l32.is_null() {
            return Err("Logger32 is not running (no window titled \"Logger32\")".into());
        }
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc); // 0 if it already exists, which is fine
        let me = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), hinst, std::ptr::null());
        if me.is_null() {
            return Err("could not create the helper window".into());
        }
        let r = session(l32, me, texts);
        MSG.store(0, Ordering::SeqCst);
        DestroyWindow(me);
        UnregisterClassW(class.as_ptr(), hinst);
        r
    }
}

unsafe fn session(l32: HWND, me: HWND, texts: &[String]) -> Result<(), String> {
    unsafe {
        // claim one of the five slots
        let mut msg = 0;
        for slot in 1..=SLOTS {
            msg = RegisterWindowMessageW(wide(&format!("Logger32 {slot}")).as_ptr());
            MSG.store(msg, Ordering::SeqCst);
            ACCEPT.store(-1, Ordering::SeqCst);
            TEXT_BOX.store(0, Ordering::SeqCst);
            PostMessageW(l32, msg, 1, me as LPARAM);
            if !pump(REPLY_WAIT, || ACCEPT.load(Ordering::SeqCst) >= 0) {
                return Err("Logger32 did not answer the connection request".into());
            }
            if ACCEPT.load(Ordering::SeqCst) == 1 {
                break;
            }
            msg = 0;
        }
        if msg == 0 {
            return Err("all five of Logger32's external interface slots are in use".into());
        }
        // ask for its text box (no unsolicited text wanted)
        PostMessageW(l32, msg, 2, 0);
        let got = pump(BOX_WAIT, || TEXT_BOX.load(Ordering::SeqCst) != 0);
        let result = if !got {
            Err("Logger32 did not hand out its text box".to_string())
        } else {
            let tb = TEXT_BOX.load(Ordering::SeqCst) as HWND;
            let mut res = Ok(());
            for t in texts {
                let w = wide(t);
                let mut out = 0usize;
                let r = SendMessageTimeoutW(tb, WM_SETTEXT, 0, w.as_ptr() as LPARAM, SMTO_ABORTIFHUNG, 3000, &mut out);
                if r == 0 {
                    res = Err("Logger32 did not accept the QSO text (busy or hung)".to_string());
                    break;
                }
                pump(Duration::from_millis(300), || false);
            }
            res
        };
        PostMessageW(l32, msg, 4, 0); // give the slot back
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ole_dates() {
        assert_eq!(ole_date(Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap()), "25569.0000000000");
        // the example in the documentation (39470.67...) is 2008-01-23 around 16:10
        let d = ole_date(Utc.with_ymd_and_hms(2008, 1, 23, 16, 10, 0).unwrap());
        assert!(d.starts_with("39470.6736"), "{d}");
    }

    #[test]
    fn record_text() {
        let off = Utc.with_ymd_and_hms(2026, 10, 24, 17, 5, 30).unwrap();
        let q = QsoLogged {
            id: "WSJT-X".into(), time_off: Some(off), dx_call: "k1abc".into(), tx_freq: 14_075_500, mode: "FT4".into(),
            report_sent: "-10".into(), report_rcvd: "-12".into(), time_on: Some(off), ..Default::default()
        };
        let e = WorkedEntry::new("K1ABC", "20m", "FT4", Some(off), "US-1234", "Café Park");
        let r = records(&q, &e);
        assert_eq!(r[0], "<EOR>");
        assert!(r[1].starts_with("<CALL:5>K1ABC<RST_SENT:3>-10<RST_RCVD:3>-12"), "{}", r[1]);
        assert!(r[1].contains("<MODE:3>FT4") && r[1].contains("<FREQ:9>14075.500"));
        assert!(r[1].contains("US-1234") && r[1].contains("Caf?"));
        assert!(r[1].ends_with("<APP_LOGQSO:1>Y"));
        assert!(!r[1].contains("SIG"));
    }
}
