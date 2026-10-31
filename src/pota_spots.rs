// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! pota.app spot poller. Fetches at most once a minute and keeps a local copy.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::events::{Event, Post};
use crate::hunter::{band_for, base_call};

pub const SPOTS_URL: &str = "https://api.pota.app/v1/spots";
pub const USER_AGENT: &str = "pota-auto-digimationator/1.0";
/// seconds between fetches, never less
pub const POLL_INTERVAL: f64 = 60.0;
/// reuse the cache file on startup if newer than this
pub const CACHE_MAX_AGE: f64 = 300.0;
/// retry delays after consecutive failures
pub const BACKOFF: [f64; 3] = [10.0, 30.0, 60.0];

/// pota.app modes WSJT-X can work (the overall spot count only shows these)
pub const DIGITAL_MODES: [&str; 11] =
    ["FT8", "FT4", "FT2", "JT65", "JT9", "JT4", "MSK144", "Q65", "FST4", "FST4W", "WSPR"];

pub fn is_digital(mode: &str) -> bool {
    DIGITAL_MODES.contains(&mode)
}

pub fn epoch_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spot {
    pub activator: String,
    pub frequency_khz: f64,
    pub band: Option<&'static str>,
    pub mode: String,
    pub reference: String,
    pub park_name: String,
    pub location: String,
    /// `hasQualifyingActivation` (false = the park has never had a valid activation: ATNO)
    pub qualifying: Option<bool>,
    /// UTC, e.g. "2026-10-25T02:45:50"
    pub spot_time: String,
}

impl Spot {
    /// The program of the park: "US" for "US-1234", "JA" for "JA-116273"; "" without a park.
    pub fn program(&self) -> &str {
        program_of(&self.reference)
    }
}

pub fn program_of(reference: &str) -> &str {
    reference.split_once('-').map_or("", |(p, _)| p)
}

/// How a hidden program is stored for one band and mode: "JA 20m FT8".
pub fn hide_key(program: &str, band: &str, mode: &str) -> String {
    format!("{program} {band} {}", mode.to_uppercase())
}

/// Local copy of the spot list, indexed by base callsign (in the order pota.app
/// listed them). Spots of a program hidden on their band and mode (see `set_hidden`) are
/// kept but never returned, so every list, count and the hunt see the same filtered table.
#[derive(Clone)]
pub struct SpotTable {
    pub fetched_at: f64,
    calls: Vec<(String, Vec<Spot>)>,
    index: HashMap<String, usize>,
    hidden: HashSet<String>,
}

fn text(s: &Value, key: &str) -> String {
    match s.get(key) {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The activator's or a hunter's last comment says "QRT": the station is off the air.
fn went_qrt(s: &Value) -> bool {
    ["activatorLastComments", "hunterLastComments"]
        .iter()
        .any(|k| text(s, k).to_uppercase().contains("QRT"))
}

impl SpotTable {
    pub fn new(raw: &[Value], fetched_at: f64) -> Self {
        let mut t = SpotTable { fetched_at, calls: Vec::new(), index: HashMap::new(), hidden: HashSet::new() };
        for s in raw {
            let khz = match s.get("frequency") {
                None | Some(Value::Null) => 0.0,
                Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                Some(Value::String(v)) if v.is_empty() => 0.0,
                Some(Value::String(v)) => match v.trim().parse::<f64>() {
                    Ok(x) => x,
                    Err(_) => continue,
                },
                Some(Value::Bool(b)) => *b as u8 as f64,
                Some(_) => continue,
            };
            let call = text(s, "activator").to_uppercase();
            if call.is_empty() || truthy(s.get("invalid")) || went_qrt(s) {
                continue;
            }
            let spot = Spot {
                activator: call.clone(),
                frequency_khz: khz,
                band: band_for((khz * 1000.0) as i64 as u64),
                mode: text(s, "mode").to_uppercase(),
                reference: text(s, "reference"),
                park_name: text(s, "name"),
                location: text(s, "locationDesc"),
                qualifying: match s.get("hasQualifyingActivation") {
                    Some(Value::Bool(b)) => Some(*b),
                    Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0),
                    _ => None,
                },
                spot_time: text(s, "spotTime"),
            };
            let key = base_call(&call);
            match t.index.get(&key) {
                Some(&i) => t.calls[i].1.push(spot),
                None => {
                    t.index.insert(key.clone(), t.calls.len());
                    t.calls.push((key, vec![spot]));
                }
            }
        }
        t
    }

    /// Hide the spots of these program/band/mode combinations (`hide_key`): "JA" can be
    /// hidden on 20m FT8 and still shown on 40m FT8.
    pub fn set_hidden(&mut self, hidden: HashSet<String>) {
        self.hidden = hidden;
    }

    fn shown(&self, s: &Spot) -> bool {
        match s.band {
            Some(b) if !self.hidden.is_empty() => !self.hidden.contains(&hide_key(s.program(), b, &s.mode)),
            _ => true,
        }
    }

    pub fn len(&self) -> usize {
        self.all().count()
    }

    pub fn all(&self) -> impl Iterator<Item = &Spot> + '_ {
        self.calls.iter().flat_map(|(_, v)| v.iter()).filter(|s| self.shown(s))
    }

    /// Every program spotted on `band` in `mode`, hidden ones included, as (program,
    /// spots), the busiest first: what the program filter offers.
    pub fn programs(&self, band: &str, mode: &str) -> Vec<(String, usize)> {
        let mode = mode.to_uppercase();
        let mut n: HashMap<&str, usize> = HashMap::new();
        for s in self.calls.iter().flat_map(|(_, v)| v.iter()).filter(|s| s.band == Some(band) && s.mode == mode && !s.program().is_empty()) {
            *n.entry(s.program()).or_default() += 1;
        }
        let mut v: Vec<(String, usize)> = n.into_iter().map(|(p, c)| (p.to_string(), c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Every band and mode with digital spots, hidden ones counted too, as (band, mode,
    /// spots): what the Program filters window lists.
    pub fn combos(&self) -> Vec<(&'static str, String, usize)> {
        let mut n: HashMap<(&'static str, &str), usize> = HashMap::new();
        for s in self.calls.iter().flat_map(|(_, v)| v.iter()).filter(|s| is_digital(&s.mode)) {
            if let Some(b) = s.band {
                *n.entry((b, s.mode.as_str())).or_default() += 1;
            }
        }
        n.into_iter().map(|((b, m), c)| (b, m.to_string(), c)).collect()
    }

    /// The spots of `call` on `band` in `mode` that the program filter hides (so the Programs
    /// button can say a filtered station was heard).
    pub fn hidden_spots_for_mode(&self, call: &str, band: &str, mode: &str) -> Vec<&Spot> {
        let Some(&i) = self.index.get(&base_call(call)) else { return Vec::new() };
        self.calls[i].1.iter().filter(|s| s.band == Some(band) && s.mode.eq_ignore_ascii_case(mode) && !self.shown(s)).collect()
    }

    /// Spots in a mode WSJT-X can handle (SSB, CW and FM spots are left out).
    pub fn digital_count(&self) -> usize {
        self.all().filter(|s| is_digital(&s.mode)).count()
    }

    pub fn spots_for(&self, call: &str, band: Option<&str>) -> Vec<&Spot> {
        let Some(&i) = self.index.get(&base_call(call)) else { return Vec::new() };
        self.calls[i]
            .1
            .iter()
            .filter(|s| (band.is_none() || s.band == band) && self.shown(s))
            .collect()
    }

    /// Spots of `call` on `band` in `mode` (upper case, e.g. "FT8").
    pub fn spots_for_mode(&self, call: &str, band: &str, mode: &str) -> Vec<&Spot> {
        self.spots_for(call, Some(band)).into_iter().filter(|s| s.mode.eq_ignore_ascii_case(mode)).collect()
    }

    /// Spots on `band` whose mode is `mode` (e.g. "40m", "FT8"), by frequency.
    pub fn on(&self, band: &str, mode: &str) -> Vec<&Spot> {
        let mode = mode.to_uppercase();
        let mut v: Vec<&Spot> =
            self.all().filter(|s| s.band == Some(band) && s.mode == mode).collect();
        v.sort_by(|a, b| a.frequency_khz.total_cmp(&b.frequency_khz));
        v
    }

    pub fn count(&self, band: &str, mode: &str) -> usize {
        let mode = mode.to_uppercase();
        self.all().filter(|s| s.band == Some(band) && s.mode == mode).count()
    }
}

/// Fetches spots on a worker thread; posts `Event::Spots` or `Event::SpotsError`.
pub struct SpotPoller {
    /// epoch seconds of the next fetch, read by the GUI for its countdown
    next_fetch: AtomicU64,
    stop: AtomicBool,
}

impl SpotPoller {
    pub fn next_fetch(&self) -> f64 {
        f64::from_bits(self.next_fetch.load(Ordering::SeqCst))
    }

    fn set_next(&self, t: f64) {
        self.next_fetch.store(t.to_bits(), Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub fn start_poller(post: Post, cache_path: PathBuf) -> Arc<SpotPoller> {
    let p = Arc::new(SpotPoller { next_fetch: AtomicU64::new(epoch_now().to_bits()), stop: AtomicBool::new(false) });
    let me = p.clone();
    std::thread::Builder::new()
        .name("pota-spots".into())
        .spawn(move || run(me, post, cache_path))
        .expect("spawn pota-spots");
    p
}

fn load_cache(path: &PathBuf) -> Option<(f64, Vec<Value>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let at = v.get("fetched_at")?.as_f64()?;
    let spots = v.get("spots")?.as_array()?.clone();
    Some((at, spots))
}

fn save_cache(path: &PathBuf, fetched_at: f64, spots: &[Value]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::json!({"fetched_at": fetched_at, "spots": spots});
    std::fs::write(&tmp, serde_json::to_string(&body).unwrap_or_default())?;
    std::fs::rename(&tmp, path)
}

fn fetch() -> Result<Vec<Value>, String> {
    let resp = ureq::get(SPOTS_URL)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|e| e.to_string())?;
    let body = resp.into_string().map_err(|e| e.to_string())?;
    match serde_json::from_str::<Value>(&body).map_err(|e| e.to_string())? {
        Value::Array(a) => Ok(a),
        _ => Err("unexpected response format".into()),
    }
}

fn run(me: Arc<SpotPoller>, post: Post, cache_path: PathBuf) {
    me.set_next(epoch_now());
    if let Some((fetched_at, spots)) = load_cache(&cache_path) {
        post(Event::Spots(Arc::new(SpotTable::new(&spots, fetched_at))));
        if epoch_now() - fetched_at < CACHE_MAX_AGE {
            me.set_next(fetched_at + POLL_INTERVAL);
        }
    }
    let mut failures = 0usize;
    while !me.stop.load(Ordering::SeqCst) {
        if me.next_fetch() - epoch_now() > 0.0 {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        match fetch() {
            Ok(spots) => {
                let fetched_at = epoch_now();
                if let Err(e) = save_cache(&cache_path, fetched_at, &spots) {
                    post(Event::SpotsError(format!("Could not write spot cache: {e}")));
                }
                post(Event::Spots(Arc::new(SpotTable::new(&spots, fetched_at))));
                post(Event::SpotsOk(true));
                failures = 0;
                me.set_next(fetched_at + POLL_INTERVAL);
            }
            Err(e) => {
                failures += 1;
                let delay = BACKOFF[failures.min(BACKOFF.len()) - 1];
                post(Event::SpotsError(format!(
                    "pota.app fetch failed ({e}); using local copy, retry in {} s",
                    delay as i64
                )));
                post(Event::SpotsOk(false));
                me.set_next(epoch_now() + delay);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spot_count_by_band_and_mode() {
        let raw = vec![
            json!({"activator": "K1A", "frequency": "7074", "mode": "FT8"}),
            json!({"activator": "K2B", "frequency": "7075.5", "mode": "ft8"}),
            json!({"activator": "K3C", "frequency": "7047.5", "mode": "FT4"}),
            json!({"activator": "K4D", "frequency": "14074", "mode": "FT8"}),
            json!({"activator": "K5E", "frequency": "7030", "mode": "CW"}),
        ];
        let t = SpotTable::new(&raw, 0.0);
        assert_eq!(t.count("40m", "FT8"), 2);
        assert_eq!(t.count("40m", "ft4"), 1);
        assert_eq!(t.count("20m", "FT8"), 1);
        assert_eq!(t.len(), 5);
        assert_eq!(t.digital_count(), 4);
    }

    #[test]
    fn qrt_spots_are_skipped() {
        let raw = vec![
            json!({"activator": "K1A", "frequency": "7074", "mode": "FT8", "activatorLastComments": "Qrt tnx"}),
            json!({"activator": "K2B", "frequency": "7075", "mode": "FT8", "hunterLastComments": "he went QRT"}),
            json!({"activator": "K3C", "frequency": "7076", "mode": "FT8", "activatorLastComments": "CQ POTA"}),
        ];
        let t = SpotTable::new(&raw, 0.0);
        assert_eq!(t.len(), 1);
        assert!(t.spots_for("K1A", None).is_empty());
        assert_eq!(t.spots_for("K3C", None).len(), 1);
    }

    #[test]
    fn hidden_programs_vanish_only_on_their_band_and_mode() {
        let raw = vec![
            json!({"activator": "K1A", "frequency": "7074", "mode": "FT8", "reference": "US-1234"}),
            json!({"activator": "JA1B", "frequency": "7075", "mode": "FT8", "reference": "JA-116273"}),
            json!({"activator": "JA2C", "frequency": "14074", "mode": "FT8", "reference": "JA-0001"}),
            json!({"activator": "K4D", "frequency": "14075", "mode": "FT4", "reference": "US-1"}),
            json!({"activator": "K5E", "frequency": "7030", "mode": "CW", "reference": "CA-9"}),
        ];
        let mut t = SpotTable::new(&raw, 0.0);
        assert_eq!(t.programs("40m", "FT8"), vec![("JA".to_string(), 1), ("US".to_string(), 1)]);
        t.set_hidden([hide_key("JA", "20m", "ft8")].into_iter().collect());
        assert_eq!(t.len(), 4);
        assert_eq!(t.count("20m", "FT8"), 0); // JA2C is hidden on 20m FT8
        assert_eq!(t.count("40m", "FT8"), 2); // JA1B still shows on 40m FT8
        assert!(!t.spots_for("JA1B", None).is_empty());
        assert!(t.spots_for("JA2C", None).is_empty());
        assert_eq!(t.hidden_spots_for_mode("JA2C", "20m", "FT8").len(), 1);
        assert!(t.hidden_spots_for_mode("JA1B", "40m", "FT8").is_empty());
        assert_eq!(t.programs("20m", "FT8"), vec![("JA".to_string(), 1)]); // still offered, so it can be shown again
        t.set_hidden(Default::default());
        assert_eq!(t.len(), 5);
        assert_eq!(program_of("US-1234"), "US");
        assert_eq!(program_of(""), "");
    }
}
