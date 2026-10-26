//! Play the QSO-logged sound (assets/ding.mp3) through Windows' MCI.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Media::Multimedia::mciSendStringW;

const ALIAS: &str = "pota_ding";

fn mci(cmd: &str) -> u32 {
    let w: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
    unsafe { mciSendStringW(w.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut()) }
}

/// Start `path` (default: assets/ding.mp3 in the app folder, also used when `path` is
/// missing) playing and return at once. Quietly does nothing if there is no file.
pub fn play(path: &str, default: &Path) -> bool {
    let mut p = PathBuf::from(path);
    if path.is_empty() || !p.is_file() {
        p = default.to_path_buf();
    }
    if !p.is_file() {
        return false;
    }
    mci(&format!("close {ALIAS}")); // restart if it is still playing
    if mci(&format!("open \"{}\" alias {ALIAS}", p.display())) != 0 {
        return false;
    }
    mci(&format!("play {ALIAS}")) == 0
}
