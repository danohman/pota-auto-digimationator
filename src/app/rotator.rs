// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Rotator (rotctld): the settings tab's test and turning toward the station being called.

use super::*;

/// Smallest angle between two headings, 0..=180 degrees.
pub fn heading_diff(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

impl App {
    pub fn start_rotator_test(&mut self) {
        if self.rot_testing || !self.s.rotctld_enabled {
            return;
        }
        self.rot_testing = true;
        self.rot_note = Some(("Asking rotctld…".into(), false));
        let (host, port, post) = (self.s.rotctld_host.trim().to_string(), self.s.rotctld_port, self.post.clone());
        std::thread::spawn(move || {
            post(Event::RotTest(crate::rotctld::position(&host, port).map(|(az, el)| format!("Connected: the rotator points at {az:.0}° (elevation {el:.0}°)"))));
        });
    }

    pub fn rotator_test_done(&mut self, r: Result<String, String>) {
        self.rot_testing = false;
        self.rot_note = Some(match r {
            Ok(t) => (t, false),
            Err(e) => (format!("Failed: {e}"), true),
        });
    }

    /// The rotator may move on the current band (Settings > Rotator > Bands). An unknown band counts as allowed.
    pub fn rot_band_ok(&self) -> bool {
        self.h.band().is_none_or(|b| self.s.rotctld_bands.iter().any(|x| x == b))
    }

    /// Turn the antenna toward the park of the station being called, once per target and
    /// only when it is not already pointing within the dead band.
    pub(super) fn aim_rotator(&mut self) {
        let Some(call) = self.h.target().map(|t| t.call.clone()) else {
            self.rot_aimed.clear();
            return;
        };
        if !self.s.rotctld_enabled || !self.s.rotctld_auto || self.rot_aimed == call || !self.rot_band_ok() {
            return;
        }
        let Some(me) = self.h.status.as_ref().and_then(|s| crate::geo::grid_center(&s.de_grid)) else { return };
        let base = base_call(&call);
        let Some(pos) = self.h.spots.as_ref().and_then(|t| t.all().find(|s| base_call(&s.activator) == base && s.pos.is_some()).and_then(|s| s.pos)) else { return };
        self.rot_aimed = call.clone();
        let want = crate::geo::bearing(me, pos);
        let (host, port, dead, post, state) = (self.s.rotctld_host.trim().to_string(), self.s.rotctld_port, self.s.rotctld_deadband as f64, self.post.clone(), self.rot.clone());
        std::thread::spawn(move || {
            let say = |t: String, tag: Tag| post(Event::Aclog(t, tag));
            let cur = match crate::rotctld::position(&host, port) {
                Ok((az, _)) => az,
                Err(e) => return say(format!("Rotator: {e}"), Tag::Error),
            };
            if dead > 0.0 && heading_diff(cur, want) <= dead {
                return say(format!("Rotator: {call} is at {want:.0}°, the antenna is at {cur:.0}° (within ±{dead:.0}°), not turning"), Tag::Info);
            }
            match crate::rotctld::set_azimuth(&host, port, want) {
                Ok(()) => {
                    {
                        let mut s = state.lock().unwrap();
                        (s.target, s.target_at, s.moving) = (Some(want), epoch_now(), true);
                    }
                    say(format!("Rotator: turning to {want:.0}° for {call} (was {cur:.0}°)"), Tag::Info)
                }
                Err(e) => say(format!("Rotator: could not turn toward {call}: {e}"), Tag::Error),
            }
        });
    }
}


/// What the status row's rotator chip shows; written by the poller thread and the turn.
#[derive(Default, Clone)]
pub struct RotState {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// the last heading read, None until read or while rotctld cannot be reached
    pub az: Option<f64>,
    /// the heading a turn we ordered is heading for, until it gets there
    pub target: Option<f64>,
    /// when `target` was set (epoch seconds), so a stuck turn gives up
    pub target_at: f64,
    /// the heading changed since the last read, or a turn is under way
    pub moving: bool,
}

impl RotState {
    pub fn from_settings(s: &Settings) -> Self {
        RotState { enabled: s.rotctld_enabled, host: s.rotctld_host.trim().to_string(), port: s.rotctld_port, ..Default::default() }
    }
}

/// Reads the heading once a second while a rotator is enabled.
pub fn start_poller(state: Arc<Mutex<RotState>>) {
    std::thread::Builder::new()
        .name("rotator".into())
        .spawn(move || {
            let mut last: Option<f64> = None;
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                let (enabled, host, port) = {
                    let s = state.lock().unwrap();
                    (s.enabled, s.host.clone(), s.port)
                };
                if !enabled {
                    last = None;
                    let mut s = state.lock().unwrap();
                    (s.az, s.target, s.moving) = (None, None, false);
                    continue;
                }
                let read = crate::rotctld::position(&host, port).ok().map(|(az, _)| az);
                let mut s = state.lock().unwrap();
                s.az = read;
                let changed = matches!((read, last), (Some(a), Some(b)) if heading_diff(a, b) >= 2.0);
                if let (Some(t), Some(a)) = (s.target, read) {
                    if heading_diff(a, t) <= 2.0 || epoch_now() - s.target_at > 180.0 {
                        s.target = None;
                    }
                }
                if read.is_none() {
                    s.target = None;
                }
                s.moving = changed || s.target.is_some();
                last = read;
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::heading_diff;

    #[test]
    fn diff_wraps_around_north() {
        assert_eq!(heading_diff(350.0, 10.0), 20.0);
        assert_eq!(heading_diff(10.0, 350.0), 20.0);
        assert_eq!(heading_diff(90.0, 270.0), 180.0);
        assert_eq!(heading_diff(45.0, 45.0), 0.0);
    }
}
