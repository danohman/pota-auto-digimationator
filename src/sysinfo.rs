// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Windows facts and live numbers for the debug window: OS version, CPU, memory, this
//! process's CPU, memory and handle counts. Everything is best effort (`None` or "?").

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn reg_string(key: &str, name: &str) -> Option<String> {
    let (k, n) = (wide(key), wide(name));
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE as HKEY, k.as_ptr(), n.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as _, &mut len) };
    if rc != 0 {
        return None;
    }
    let n = (len as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

fn reg_dword(key: &str, name: &str) -> Option<u32> {
    let (k, n) = (wide(key), wide(name));
    let mut v = 0u32;
    let mut len = 4u32;
    let rc = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE as HKEY, k.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut v as *mut u32 as _, &mut len) };
    (rc == 0).then_some(v)
}

/// "Windows 10 Pro 22H2 (build 19045.5247)"; Windows 11 still says "Windows 10" in the
/// registry's ProductName, so the build number decides.
pub fn os_version() -> String {
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let name = reg_string(KEY, "ProductName").unwrap_or_else(|| "Windows".into());
    let build = reg_string(KEY, "CurrentBuild").unwrap_or_default();
    let name = if build.parse::<u32>().is_ok_and(|b| b >= 22000) { name.replace("Windows 10", "Windows 11") } else { name };
    let display = reg_string(KEY, "DisplayVersion").or_else(|| reg_string(KEY, "ReleaseId")).unwrap_or_default();
    let ubr = reg_dword(KEY, "UBR").map(|u| format!(".{u}")).unwrap_or_default();
    format!("{name} {display} (build {build}{ubr})").replace("  ", " ")
}

/// The processor's marketing name and its logical processor count.
pub fn cpu_name() -> String {
    let name = reg_string(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", "ProcessorNameString").unwrap_or_else(|| "?".into());
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    format!("{} ({n} logical)", name.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn logical_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Windows uptime in seconds.
pub fn os_uptime() -> f64 {
    unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() as f64 / 1000.0 }
}

/// Physical memory: (total, available, load percent), and the commit limit and charge, in bytes.
pub struct SysMem {
    pub total: u64,
    pub avail: u64,
    pub load: u32,
    pub commit_limit: u64,
    pub commit_used: u64,
}

pub fn sys_mem() -> Option<SysMem> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then(|| SysMem {
        total: m.ullTotalPhys,
        avail: m.ullAvailPhys,
        load: m.dwMemoryLoad,
        commit_limit: m.ullTotalPageFile,
        commit_used: m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile),
    })
}

/// This process's memory in bytes.
pub struct ProcMem {
    pub working_set: u64,
    pub peak_working_set: u64,
    pub private: u64,
    pub peak_commit: u64,
    pub page_faults: u32,
}

pub fn proc_mem() -> Option<ProcMem> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    (unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as _, c.cb) } != 0).then(|| ProcMem {
        working_set: c.WorkingSetSize as u64,
        peak_working_set: c.PeakWorkingSetSize as u64,
        private: c.PrivateUsage as u64,
        peak_commit: c.PeakPagefileUsage as u64,
        page_faults: c.PageFaultCount,
    })
}

/// Kernel objects this process holds: (handles, GDI objects, USER objects, threads).
pub fn proc_objects() -> (u32, u32, u32, usize) {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetGuiResources, GetProcessHandleCount};
    let (mut handles, gdi, user);
    unsafe {
        handles = 0u32;
        GetProcessHandleCount(GetCurrentProcess(), &mut handles);
        gdi = GetGuiResources(GetCurrentProcess(), 0);
        user = GetGuiResources(GetCurrentProcess(), 1);
    }
    (handles, gdi, user, threads(unsafe { GetCurrentProcessId() }))
}

fn threads(pid: u32) -> usize {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32};
    let mut n = 0;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &mut e) != 0;
        while ok {
            if e.th32OwnerProcessID == pid {
                n += 1;
            }
            ok = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    n
}

fn ticks(f: &FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// CPU use between two calls to `sample`: this process and the whole machine, as a
/// percent of all logical processors (what Task Manager shows).
pub struct CpuSampler {
    last: Option<(u64, u64, u64)>, // process busy, system busy, system total (100 ns ticks)
    pub app: f64,
    pub system: f64,
}

impl CpuSampler {
    pub fn new() -> Self {
        CpuSampler { last: None, app: 0.0, system: 0.0 }
    }

    pub fn sample(&mut self) {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes};
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut create, mut exit, mut pk, mut pu) = (z, z, z, z);
        let (mut idle, mut sk, mut su) = (z, z, z);
        let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut create, &mut exit, &mut pk, &mut pu) != 0 && GetSystemTimes(&mut idle, &mut sk, &mut su) != 0 };
        if !ok {
            return;
        }
        let proc = ticks(&pk) + ticks(&pu);
        let total = ticks(&sk) + ticks(&su); // kernel time includes idle
        let busy = total.saturating_sub(ticks(&idle));
        if let Some((p0, b0, t0)) = self.last {
            let dt = total.saturating_sub(t0) as f64;
            if dt > 0.0 {
                self.app = proc.saturating_sub(p0) as f64 / dt * 100.0;
                self.system = busy.saturating_sub(b0) as f64 / dt * 100.0;
            }
        }
        self.last = Some((proc, busy, total));
    }
}

pub fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

pub fn duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (d, h, m, s) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else {
        format!("{m}m {s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_something_on_windows() {
        assert!(os_version().starts_with("Windows"));
        assert!(sys_mem().is_some_and(|m| m.total > 0));
        let p = proc_mem().unwrap();
        assert!(p.working_set > 0 && p.private > 0);
        let (h, _, _, t) = proc_objects();
        assert!(h > 0 && t > 0);
        let mut c = CpuSampler::new();
        c.sample();
        c.sample();
    }

    #[test]
    fn durations() {
        assert_eq!(duration(65.0), "1m 5s");
        assert_eq!(duration(3700.0), "1h 1m 40s");
        assert_eq!(duration(90000.0), "1d 1h 0m");
    }
}
