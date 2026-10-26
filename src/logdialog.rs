//! Find WSJT-X's (or WSJT-X Improved's) Log QSO window and press its default (OK) button.
//!
//! WSJT-X keeps the window around hidden ("WSJT-X   v2.7.0-rc7    - Log QSO") and shows
//! it after a QSO. There is no UDP command to accept it, so we post an Enter keypress
//! straight to that window; Qt routes it to the dialog's focus widget and the default
//! button fires, without stealing focus from other apps.

use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    WM_KEYDOWN, WM_KEYUP,
};

pub const TITLE_SUFFIX: &str = "- Log QSO";
/// WSJT-X is wsjtx.exe; WSJT-X Improved ("WS") is ws.exe, with the same Log QSO window.
pub const PROCESS_NAMES: [&str; 2] = ["wsjtx.exe", "ws.exe"];

const VK_RETURN: usize = 0x0D;
const VK_ESCAPE: usize = 0x1B;

fn exe_name(hwnd: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
    }
}

pub fn window_title(hwnd: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, buf.as_mut_ptr(), n + 1);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> i32 {
    let found = &mut *(lparam as *mut Vec<usize>);
    if IsWindowVisible(hwnd) != 0
        && window_title(hwnd).trim_end().ends_with(TITLE_SUFFIX)
        && PROCESS_NAMES.contains(&exe_name(hwnd).as_str())
    {
        found.push(hwnd as usize);
    }
    1
}

/// Visible WSJT-X Log QSO windows.
pub fn find_log_windows() -> Vec<usize> {
    let mut found: Vec<usize> = Vec::new();
    unsafe {
        EnumWindows(Some(collect), &mut found as *mut _ as LPARAM);
    }
    found
}

fn post_key(hwnd: usize, vk: usize) {
    let scan: isize = if vk == VK_RETURN { 0x1C } else { 0x01 } << 16;
    unsafe {
        PostMessageW(hwnd as HWND, WM_KEYDOWN, vk, scan | 0x1);
        PostMessageW(hwnd as HWND, WM_KEYUP, vk, scan | 0xC000_0001u32 as i32 as isize);
    }
}

/// Press OK (or Cancel, for testing) on the Log QSO window. true when one was showing.
pub fn press(ok: bool) -> bool {
    let windows = find_log_windows();
    for &w in &windows {
        post_key(w, if ok { VK_RETURN } else { VK_ESCAPE });
    }
    !windows.is_empty()
}
