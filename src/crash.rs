// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Crash reporting. The app has no console, so a panic or a native crash just makes the
//! window vanish. This leaves evidence in the data folder:
//!
//! - `crash.log`: a panic (message, thread, source line, backtrace) or an unhandled Windows
//!   exception (code, address, module and offset), each with the time and version.
//! - `crash.dmp`: a minidump written on an unhandled Windows exception (the latest one only).
//! - `running.flag`: written at start, removed on a clean exit. If it is still there at the
//!   next start (and that process is gone), the last run ended some way no handler could
//!   catch (killed, a Rust abort, an out-of-memory failure), and a message box says so.

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpNormal, MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MessageBoxW};

static DATA: OnceLock<PathBuf> = OnceLock::new();

const LOG_CAP: u64 = 512 * 1024;

fn log_path() -> Option<PathBuf> {
    DATA.get().map(|d| d.join("crash.log"))
}

fn flag_path() -> Option<PathBuf> {
    DATA.get().map(|d| d.join("running.flag"))
}

/// Appends one report to crash.log (starting a fresh file once it passes 512 KB).
fn append(report: &str) {
    let Some(p) = log_path() else { return };
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > LOG_CAP) {
        let _ = std::fs::rename(&p, p.with_extension("old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(report.as_bytes());
    }
}

fn header(kind: &str) -> String {
    format!(
        "==== {kind} at {} (v{}) ====\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        env!("CARGO_PKG_VERSION")
    )
}

/// Installs the handlers and checks for a crash on the last run. Call first thing in main,
/// with the data folder.
pub fn install(data: &Path) {
    let _ = DATA.set(data.to_path_buf());
    report_previous_run();
    if let Some(f) = flag_path() {
        let _ = std::fs::write(f, unsafe { GetCurrentProcessId() }.to_string());
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let mut r = header("PANIC");
        r.push_str(&format!("thread: {}\n", thread.name().unwrap_or("<unnamed>")));
        r.push_str(&format!("{info}\n"));
        r.push_str(&format!("backtrace:\n{}\n\n", std::backtrace::Backtrace::force_capture()));
        append(&r);
        prev(info);
    }));
    unsafe { SetUnhandledExceptionFilter(Some(exception_filter)) };
}

/// Call after the window has closed normally.
pub fn clean_exit() {
    if let Some(f) = flag_path() {
        let _ = std::fs::remove_file(f);
    }
}

fn report_previous_run() {
    let Some(flag) = flag_path() else { return };
    let Ok(pid) = std::fs::read_to_string(&flag) else { return };
    let _ = std::fs::remove_file(&flag);
    if pid.trim().parse::<u32>().is_ok_and(process_alive) {
        return; // another copy is still running; its flag, not a crash
    }
    append(&format!(
        "{}The last run ended without a clean shutdown and left no panic or exception record: it was killed, aborted, or ran out of memory.\n\n",
        header("UNEXPECTED EXIT")
    ));
    let log = log_path().map(|p| p.display().to_string()).unwrap_or_default();
    let text = format!("The last run of POTA Auto-Digimationator ended unexpectedly.\n\nDetails (if any were recorded) are in:\n{log}");
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    unsafe { MessageBoxW(std::ptr::null_mut(), wide(&text).as_ptr(), wide("POTA Auto-Digimationator").as_ptr(), MB_OK | MB_ICONWARNING) };
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        CloseHandle(h);
        ok
    }
}

/// "module.dll+0x1234" for an address, or just the address.
unsafe fn module_of(addr: *const core::ffi::c_void) -> String {
    unsafe {
        let mut m = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(flags, addr as *const u16, &mut m) == 0 {
            return format!("{addr:p}");
        }
        let mut buf = [0u16; 520];
        let n = GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) as usize;
        let name = String::from_utf16_lossy(&buf[..n]);
        format!("{name}+{:#x}", addr as usize - m as usize)
    }
}

unsafe extern "system" fn exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    unsafe {
        let mut r = header("UNHANDLED EXCEPTION");
        if let Some(rec) = info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) {
            r.push_str(&format!("code: {:#010x}{}\n", rec.ExceptionCode as u32, describe(rec.ExceptionCode as u32)));
            r.push_str(&format!("address: {}\n", module_of(rec.ExceptionAddress)));
            if rec.ExceptionCode as u32 == 0xC000_0005 && rec.NumberParameters >= 2 {
                let kind = match rec.ExceptionInformation[0] {
                    0 => "reading",
                    1 => "writing",
                    _ => "executing",
                };
                r.push_str(&format!("access violation {kind} {:#x}\n", rec.ExceptionInformation[1]));
            }
        }
        r.push_str(&format!("thread: {}\n", std::thread::current().name().unwrap_or("<unnamed>")));
        if let Some(dir) = DATA.get() {
            let dmp = dir.join("crash.dmp");
            if let Ok(f) = std::fs::File::create(&dmp) {
                let ex = MINIDUMP_EXCEPTION_INFORMATION { ThreadId: GetCurrentThreadId(), ExceptionPointers: info as *mut _, ClientPointers: 0 };
                let ok = MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), f.as_raw_handle(), MiniDumpNormal, &ex, std::ptr::null(), std::ptr::null());
                r.push_str(&format!("minidump: {} ({})\n", dmp.display(), if ok != 0 { "written" } else { "failed" }));
            }
        }
        r.push('\n');
        append(&r);
    }
    0 // EXCEPTION_CONTINUE_SEARCH: let Windows end the process as usual
}

fn describe(code: u32) -> &'static str {
    match code {
        0xC000_0005 => " (access violation)",
        0xC000_00FD => " (stack overflow)",
        0xC000_001D => " (illegal instruction)",
        0xC000_0094 => " (integer divide by zero)",
        0xC000_0374 => " (heap corruption)",
        0xC000_0409 => " (stack buffer overrun / fast fail)",
        0xC000_0006 => " (in-page I/O error)",
        0xC000_0017 => " (no memory)",
        _ => "",
    }
}
