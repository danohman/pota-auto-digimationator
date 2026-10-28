// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! The COM ports Windows has registered (HKLM\HARDWARE\DEVICEMAP\SERIALCOMM), the same
//! list .NET's `SerialPort.GetPortNames()` gives, including virtual ports like VSPE's.

use windows_sys::Win32::System::Registry::{RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ};

/// Port names such as "COM3", sorted by number.
pub fn list() -> Vec<String> {
    let key: Vec<u16> = "HARDWARE\\DEVICEMAP\\SERIALCOMM\0".encode_utf16().collect();
    let mut hkey = std::ptr::null_mut();
    let mut out = vec![];
    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, key.as_ptr(), 0, KEY_READ, &mut hkey) != 0 {
            return out;
        }
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut nlen = name.len() as u32;
            let mut data = [0u8; 256];
            let mut dlen = data.len() as u32;
            let mut kind = 0u32;
            let r = RegEnumValueW(hkey, i, name.as_mut_ptr(), &mut nlen, std::ptr::null_mut(), &mut kind, data.as_mut_ptr(), &mut dlen);
            if r == 259 {
                break; // ERROR_NO_MORE_ITEMS
            }
            if r != 0 || kind != REG_SZ {
                continue;
            }
            let wide: Vec<u16> = data[..dlen as usize].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
            let port = String::from_utf16_lossy(&wide);
            if !port.is_empty() {
                out.push(port);
            }
        }
        RegCloseKey(hkey);
    }
    out.sort_by_key(|p| (p.trim_start_matches(|c: char| !c.is_ascii_digit()).parse::<u32>().unwrap_or(u32::MAX), p.clone()));
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn lists_without_crashing() {
        let l = super::list();
        assert!(l.iter().all(|p| p.starts_with("COM") || !p.is_empty()));
    }
}
