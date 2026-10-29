// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Update check against the latest GitHub release, and the download / swap / relaunch.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::pota_auth::USER_AGENT;

const REPO: &str = "danohman/pota-auto-digimationator";
/// the stable name of the bare exe the release workflow publishes
const EXE_ASSET: &str = "pota-auto-digimationator.exe";
/// refuse a download bigger than this (the exe is a few MB)
const MAX_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    /// API url (needs `Accept: application/octet-stream`; the only one that works while private)
    pub api_url: String,
    pub browser_url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// "1.2.3", without the leading v
    pub version: String,
    pub page: String,
    pub exe: Asset,
    pub exe_sha: Option<Asset>,
}

/// The numeric parts of "v1.2.3" and its pre-release tag ("beta" of "1.2.3-beta", "" for a
/// final release; build metadata after '+' is ignored), or None.
fn parse_version(v: &str) -> Option<(Vec<u64>, String)> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split('+').next()?;
    let (nums, pre) = v.split_once('-').unwrap_or((v, ""));
    let parts: Option<Vec<u64>> = nums.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty()).map(|p| (p, pre.to_string()))
}

/// Is `latest` a higher version than `current`? A pre-release ranks below its own final
/// release (1.0.2-beta < 1.0.2).
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some((mut a, pa)), Some((mut b, pb))) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            if a != b {
                return a > b;
            }
            match (pa.is_empty(), pb.is_empty()) {
                (true, false) => true,
                (false, true) => false,
                _ => pa > pb,
            }
        }
        _ => false,
    }
}

fn asset_of(v: &Value) -> Option<Asset> {
    Some(Asset {
        name: v.get("name")?.as_str()?.to_string(),
        api_url: v.get("url")?.as_str()?.to_string(),
        browser_url: v.get("browser_download_url").and_then(Value::as_str).unwrap_or_default().to_string(),
    })
}

/// Pick the release's version, page and exe assets out of the API's JSON.
pub fn parse_release(json: &str) -> Result<Release, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unreadable release data: {e}"))?;
    let tag = v.get("tag_name").and_then(Value::as_str).ok_or("the release has no tag")?;
    let assets: Vec<Asset> = v.get("assets").and_then(Value::as_array).map(|a| a.iter().filter_map(asset_of).collect()).unwrap_or_default();
    let exe = assets.iter().find(|a| a.name == EXE_ASSET).cloned().ok_or_else(|| format!("release {tag} has no {EXE_ASSET}"))?;
    let exe_sha = assets.iter().find(|a| a.name == format!("{EXE_ASSET}.sha256")).cloned();
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        page: v.get("html_url").and_then(Value::as_str).unwrap_or_default().to_string(),
        exe,
        exe_sha,
    })
}

fn get(url: &str, token: Option<&str>, accept: &str, redirects: u32) -> Result<ureq::Response, String> {
    let agent = ureq::AgentBuilder::new().redirects(redirects).timeout(Duration::from_secs(60)).build();
    let mut req = agent.get(url).set("User-Agent", USER_AGENT).set("Accept", accept);
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(r) => Ok(r),
        // a redirect we asked not to follow comes back as a normal response
        Err(ureq::Error::Status(code, r)) if (300..400).contains(&code) => Ok(r),
        Err(ureq::Error::Status(404, _)) if token.is_none_or(str::is_empty) => {
            Err("GitHub says not found (the repository is private; set github_token in settings.json, or wait until it is public)".into())
        }
        Err(ureq::Error::Status(code, r)) => Err(format!("GitHub answered {code} {}", r.status_text())),
        Err(e) => Err(e.to_string()),
    }
}

/// Is there a release newer than this build? Ok(None) = up to date.
pub fn check(token: Option<&str>) -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = get(&url, token, "application/vnd.github+json", 5)?.into_string().map_err(|e| e.to_string())?;
    let rel = parse_release(&body)?;
    Ok(is_newer(&rel.version, env!("CARGO_PKG_VERSION")).then_some(rel))
}

/// The bytes of a release asset. With a token the API url is used (the only one a private
/// repository serves), and its redirect to the storage host is followed without the token.
fn fetch(a: &Asset, token: Option<&str>) -> Result<Vec<u8>, String> {
    let resp = if token.is_some_and(|t| !t.is_empty()) {
        let r = get(&a.api_url, token, "application/octet-stream", 0)?;
        match r.header("Location") {
            Some(loc) if (300..400).contains(&r.status()) => get(loc, None, "application/octet-stream", 5)?,
            _ => r,
        }
    } else {
        get(&a.browser_url, None, "application/octet-stream", 5)?
    };
    let mut buf = Vec::new();
    resp.into_reader().take(MAX_BYTES + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_BYTES {
        return Err("the download is far bigger than expected".into());
    }
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check the downloaded exe: it must look like a Windows program and, when the release
/// has a .sha256 file, match it.
pub fn verify(exe: &[u8], sha_file: Option<&str>) -> Result<(), String> {
    if exe.len() < 100_000 || &exe[..2] != b"MZ" {
        return Err("the download is not a Windows program".into());
    }
    if let Some(text) = sha_file {
        let want = text.split_whitespace().next().unwrap_or("").to_lowercase();
        let got = hex(&Sha256::digest(exe));
        if want != got {
            return Err("the download does not match its SHA-256, not installing it".into());
        }
    }
    Ok(())
}

fn with_suffix(exe: &Path, suffix: &str) -> PathBuf {
    let mut s = exe.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Where to start the new exe after we exit (the path our exe had before the swap).
static RELAUNCH: Mutex<Option<PathBuf>> = Mutex::new(None);
static RELAUNCH_NOW: AtomicBool = AtomicBool::new(false);

/// Download the release's exe, verify it and swap it in for the running one (Windows lets
/// a running exe be renamed). On success the app should close; `relaunch` then starts it.
pub fn install(rel: &Release, token: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bytes = fetch(&rel.exe, token)?;
    let sha = match &rel.exe_sha {
        Some(a) => Some(String::from_utf8_lossy(&fetch(a, token)?).into_owned()),
        None => None,
    };
    verify(&bytes, sha.as_deref())?;
    let (new, old) = (with_suffix(&exe, ".new"), with_suffix(&exe, ".old"));
    std::fs::write(&new, &bytes).map_err(|e| format!("could not write {}: {e}", new.display()))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(format!("could not move the running exe aside: {e}"));
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe); // put the old one back
        let _ = std::fs::remove_file(&new);
        return Err(format!("could not put the new exe in place: {e}"));
    }
    *RELAUNCH.lock().unwrap() = Some(exe);
    Ok(())
}

/// The app is closing after a successful `install`: ask for the new exe to be started once we have exited.
pub fn request_relaunch() {
    RELAUNCH_NOW.store(true, Ordering::SeqCst);
}

/// Called by main after the window has closed and the app has shut down (rig restored,
/// settings saved): start the new exe, so two copies never run together.
pub fn relaunch_if_requested() {
    if !RELAUNCH_NOW.load(Ordering::SeqCst) {
        return;
    }
    if let Some(exe) = RELAUNCH.lock().unwrap().take() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// Remove the leftovers of the last update (the old exe, an unfinished download).
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(with_suffix(&exe, ".old"));
        let _ = std::fs::remove_file(with_suffix(&exe, ".new"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions() {
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(is_newer("1.0.10", "1.0.9"));
        assert!(is_newer("2.0", "1.9.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("nightly", "1.0.0"));
        assert!(is_newer("1.1.0-beta", "1.0.0"));
        assert!(is_newer("1.0.2", "1.0.2-beta"));
        assert!(!is_newer("1.0.2-beta", "1.0.2"));
        assert!(!is_newer("1.0.2-beta", "1.0.2-beta"));
        assert!(is_newer("1.0.2-beta2", "1.0.2-beta1"));
    }

    const SAMPLE: &str = r#"{"tag_name":"v1.2.0","html_url":"https://github.com/o/r/releases/tag/v1.2.0","assets":[
        {"name":"pota-auto-digimationator-v1.2.0-windows-x64.zip","url":"https://api.github.com/a/1","browser_download_url":"https://github.com/z"},
        {"name":"pota-auto-digimationator.exe","url":"https://api.github.com/a/2","browser_download_url":"https://github.com/e"},
        {"name":"pota-auto-digimationator.exe.sha256","url":"https://api.github.com/a/3","browser_download_url":"https://github.com/s"}]}"#;

    #[test]
    fn parses_a_release() {
        let r = parse_release(SAMPLE).unwrap();
        assert_eq!(r.version, "1.2.0");
        assert_eq!(r.exe.api_url, "https://api.github.com/a/2");
        assert_eq!(r.exe_sha.unwrap().browser_url, "https://github.com/s");
        assert!(parse_release(r#"{"tag_name":"v1","assets":[]}"#).is_err());
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn verifies_downloads() {
        let mut exe = vec![0u8; 200_000];
        exe[0] = b'M';
        exe[1] = b'Z';
        let good = format!("{}  pota-auto-digimationator.exe\n", hex(&Sha256::digest(&exe)));
        assert!(verify(&exe, Some(&good)).is_ok());
        assert!(verify(&exe, None).is_ok());
        assert!(verify(&exe, Some("00ff  x")).is_err());
        assert!(verify(b"<html>not found</html>", None).is_err());
    }
}
