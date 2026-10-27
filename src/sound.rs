// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Play the app's sounds through Windows' MCI: the QSO-logged ding (assets/ding.mp3) and the
//! wanted-park pop (assets/pop.mp3). Both defaults are compiled into the exe, so the exe runs
//! on its own.

use std::path::PathBuf;

use windows_sys::Win32::Media::Multimedia::mciSendStringW;

#[derive(Clone, Copy)]
pub enum Kind {
    Ding,
    Pop,
}

impl Kind {
    fn alias(self) -> &'static str {
        match self {
            Kind::Ding => "pota_ding",
            Kind::Pop => "pota_pop",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Kind::Ding => "ding",
            Kind::Pop => "pop",
        }
    }
    fn bytes(self) -> &'static [u8] {
        match self {
            Kind::Ding => include_bytes!("../assets/ding.mp3"),
            Kind::Pop => include_bytes!("../assets/pop.mp3"),
        }
    }
}

fn mci(cmd: &str) -> u32 {
    let w: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
    unsafe { mciSendStringW(w.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut()) }
}

/// MCI opens files only, so the built-in sound is written to the temp folder (once per run
/// unless the bytes are already there).
fn builtin(kind: Kind) -> Option<PathBuf> {
    let bytes = kind.bytes();
    let p = std::env::temp_dir().join(format!("pota-auto-digimationator-{}-{}.mp3", kind.name(), bytes.len()));
    if std::fs::read(&p).map(|b| b == bytes).unwrap_or(false) || std::fs::write(&p, bytes).is_ok() {
        Some(p)
    } else {
        None
    }
}

/// Start `path` playing (the built-in sound of `kind` when `path` is empty or missing) and
/// return at once. Quietly does nothing if nothing can be played.
pub fn play(kind: Kind, path: &str) -> bool {
    let alias = kind.alias();
    let p = match PathBuf::from(path) {
        p if !path.is_empty() && p.is_file() => p,
        _ => match builtin(kind) {
            Some(p) => p,
            None => return false,
        },
    };
    mci(&format!("close {alias}")); // restart if it is still playing
    if mci(&format!("open \"{}\" alias {alias}", p.display())) != 0 {
        return false;
    }
    mci(&format!("play {alias}")) == 0
}
