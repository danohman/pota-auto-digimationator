// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Play the QSO-logged sound through Windows' MCI. The default ding (assets/ding.mp3) is
//! compiled into the exe, so the exe runs on its own.

use std::path::PathBuf;

use windows_sys::Win32::Media::Multimedia::mciSendStringW;

const ALIAS: &str = "pota_ding";
const DING: &[u8] = include_bytes!("../assets/ding.mp3");

fn mci(cmd: &str) -> u32 {
    let w: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
    unsafe { mciSendStringW(w.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut()) }
}

/// MCI opens files only, so the built-in ding is written to the temp folder (once per run
/// unless the bytes are already there).
fn builtin() -> Option<PathBuf> {
    let p = std::env::temp_dir().join(format!("pota-auto-digimationator-ding-{}.mp3", DING.len()));
    if std::fs::read(&p).map(|b| b == DING).unwrap_or(false) || std::fs::write(&p, DING).is_ok() {
        Some(p)
    } else {
        None
    }
}

/// Start `path` playing (the built-in ding when `path` is empty or missing) and return at
/// once. Quietly does nothing if nothing can be played.
pub fn play(path: &str) -> bool {
    let p = match PathBuf::from(path) {
        p if !path.is_empty() && p.is_file() => p,
        _ => match builtin() {
            Some(p) => p,
            None => return false,
        },
    };
    mci(&format!("close {ALIAS}")); // restart if it is still playing
    if mci(&format!("open \"{}\" alias {ALIAS}", p.display())) != 0 {
        return false;
    }
    mci(&format!("play {ALIAS}")) == 0
}
