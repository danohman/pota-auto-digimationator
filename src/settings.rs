// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! settings.json: keys and defaults, unknown keys are kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Layout {
    pub right_w: Option<i64>,
    pub split: Option<Vec<f64>>,
    /// section order of the left and right columns (card names)
    pub cols: Option<Vec<Vec<String>>>,
    /// relative heights of the sections that share their column
    pub weights: Option<BTreeMap<String, f64>>,
}

/// Where a section sits when it is popped out into its own window (points on screen).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PopGeom {
    pub popped: bool,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub udp_group: String,
    pub udp_port: u16,
    pub dry_run: bool,
    pub max_attempts: u32,
    pub attempts_by_mode: BTreeMap<String, u32>,
    /// null = no limit
    pub min_snr: Option<i32>,
    pub move_tx: bool,
    pub auto_log: bool,
    pub cq_text: String,
    pub ding: bool,
    /// custom sound file; "" = assets/ding.mp3 in the app folder
    pub ding_path: String,
    /// sound when a candidate at a wanted (NEW) park is ready to call
    pub pop: bool,
    /// custom file for it; "" = the built-in pop
    pub pop_path: String,
    /// keep ATT/NB/DNR off on the Yaesu, via ACLog's CAT passthrough
    pub rig_clean: bool,
    /// on close: "on", "off" or "leave" (the rig can't be read back)
    pub rig_restore_att: String,
    pub rig_restore_nb: String,
    pub rig_restore_dnr: String,
    pub aclog_enabled: bool,
    pub aclog_host: String,
    pub aclog_port: u16,
    /// "" = detect: WSJT-X and WSJT-X Improved log to different folders
    pub adif_path: String,
    pub pounce_signoffs: bool,
    /// only auto-hunt parks pota.app says you never hunted (NEW)
    pub new_parks_only: bool,
    /// pota.app account, for the hunt-count chips
    pub pota_user: String,
    pub pota_password: String,
    /// "system" (follow Windows), "light" or "dark"
    pub theme: String,
    /// the UI font's size in points (the other fonts follow); 8 to 16
    pub font_size: u32,
    /// respot the activator to pota.app after each logged QSO
    pub respot: bool,
    /// respot comment template; "" = no comment (default "{MODE}: S:{RST_SENT} / R:{RST_RCVD}") (see respot::PLACEHOLDERS)
    pub respot_message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// popped-out sections by name (see app::Card)
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub popouts: BTreeMap<String, PopGeom>,
    /// Tk geometry, "WxH+X+Y"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// keys this version doesn't know, kept as they are
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            udp_group: "224.0.0.2".into(),
            udp_port: 2237,
            dry_run: true,
            max_attempts: 4,
            attempts_by_mode: [("FT8", 5), ("FT4", 10), ("FT2", 20)].iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            min_snr: Some(-20),
            move_tx: false,
            auto_log: true,
            cq_text: "CQ".into(),
            ding: true,
            ding_path: String::new(),
            rig_clean: true,
            rig_restore_att: "on".into(),
            rig_restore_nb: "on".into(),
            rig_restore_dnr: "on".into(),
            aclog_enabled: false,
            aclog_host: "127.0.0.1".into(),
            aclog_port: 1100,
            adif_path: String::new(),
            pop: true,
            pop_path: String::new(),
            pounce_signoffs: true,
            new_parks_only: false,
            pota_user: String::new(),
            pota_password: String::new(),
            theme: "system".into(),
            font_size: 10,
            layout: None,
            popouts: BTreeMap::new(),
            window: None,
            extra: Map::new(),
            respot: true,
            respot_message: "{MODE}: S:{RST_SENT} / R:{RST_RCVD}".into(),
        }
    }
}

/// Where WSJT-X, or WSJT-X Improved ("WS"), keeps its log.
pub fn adif_candidates() -> Vec<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    ["WS", "WSJT-X"].iter().map(|f| Path::new(&local).join(f).join("wsjtx_log.adi")).collect()
}

/// The most recently written log among the candidates (the last if none exist).
pub fn find_adif() -> PathBuf {
    let c = adif_candidates();
    c.iter()
        .filter_map(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()).map(|t| (t, p.clone())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
        .unwrap_or_else(|| c.last().unwrap().clone())
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// The folder for the files the app writes (settings, state, spot cache), `data` under
/// the app folder. Created if missing, and files left in the app folder by older
/// versions are moved in.
pub fn data_dir(app_dir: &Path) -> PathBuf {
    let data = app_dir.join("data");
    let _ = std::fs::create_dir_all(&data);
    for name in ["settings.json", "state.json", "spots_cache.json"] {
        let (old, new) = (app_dir.join(name), data.join(name));
        if old.is_file() && !new.exists() {
            let _ = std::fs::rename(&old, &new);
        }
    }
    data
}

pub fn load(path: &Path) -> Settings {
    // A value of the wrong type falls back to the default for that key only.
    let mut s = Settings::default();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
            let mut base = serde_json::to_value(&s).unwrap();
            for (k, v) in map {
                let mut trial = base.clone();
                trial[k.as_str()] = v.clone();
                if serde_json::from_value::<Settings>(trial).is_ok() {
                    base[k.as_str()] = v;
                }
            }
            s = serde_json::from_value(base).unwrap_or_default();
        }
    }
    s.font_size = s.font_size.clamp(8, 16);
    // Older versions saved the stock WSJT-X log path as if it were a choice; detect.
    let old_default = adif_candidates().last().unwrap().display().to_string();
    if !s.adif_path.is_empty() && norm(&s.adif_path) == norm(&old_default) {
        s.adif_path.clear();
    }
    s
}

pub fn save(path: &Path, s: &Settings) -> std::io::Result<()> {
    let text = serde_json::to_string(s).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}
