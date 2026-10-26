//! T/R period lengths for the modes WSJT-X supports (WSJT-X 2.7 user guide).
//!
//! Fixed-period modes come from a table. FST4, FST4W and Q65 let the operator pick
//! the length; WSJT-X reports it in Status.tr_period, but WSJT-Z sends 4294967295
//! (unset), so it's only used when sane, and otherwise the length is measured from
//! how far apart decodes start (`PeriodDetector`).

fn fixed(mode: &str) -> Option<f64> {
    Some(match mode {
        "FT8" => 15.0,
        "FT4" => 7.5,
        "FT2" => 3.75, // measured from live decode starts: 3.76 s apart
        "JT65" | "JT9" | "JT4" => 60.0,
        "WSPR" => 120.0,
        "MSK144" => 15.0, // the guide allows 5, 10, 15 or 30 s; 15 is the default
        _ => return None,
    })
}

/// Operator-selectable modes: allowed lengths, and the length to assume until known.
fn variable(mode: &str) -> Option<(&'static [u32], f64)> {
    Some(match mode {
        "FST4" => (&[15, 30, 60, 120, 300, 900, 1800], 60.0),
        "FST4W" => (&[120, 300, 900, 1800], 120.0),
        "Q65" => (&[15, 30, 60, 120, 300], 60.0),
        _ => return None,
    })
}

pub const DEFAULT: f64 = 15.0;

/// Length in seconds of the T/R period for `mode`.
pub fn cycle_seconds(mode: &str, tr_period: u32, detected: Option<u32>) -> f64 {
    let mode = mode.to_uppercase();
    if let Some(s) = fixed(&mode) {
        return s;
    }
    if let Some((allowed, default)) = variable(&mode) {
        if allowed.contains(&tr_period) {
            return tr_period as f64;
        }
        if let Some(d) = detected.filter(|d| allowed.contains(d)) {
            return d as f64;
        }
        return default;
    }
    if (1..=1800).contains(&tr_period) {
        tr_period as f64
    } else {
        DEFAULT
    }
}

/// The allowed length nearest `interval` seconds (within 12%), else None.
pub fn snap(interval: f64, allowed: &[u32]) -> Option<u32> {
    let best = *allowed
        .iter()
        .min_by(|a, b| (**a as f64 - interval).abs().total_cmp(&(**b as f64 - interval).abs()))?;
    ((best as f64 - interval).abs() <= 0.12 * best as f64).then_some(best)
}

/// Learns a variable mode's period from when decoding begins, cycle after cycle.
#[derive(Default)]
pub struct PeriodDetector {
    mode: Option<String>,
    edges: Vec<f64>,
    pub period: Option<u32>,
}

impl PeriodDetector {
    /// Call at each switch to decoding; returns the detected period or None.
    pub fn decoding_started(&mut self, mode: &str, now: f64) -> Option<u32> {
        let mode = mode.to_uppercase();
        if self.mode.as_deref() != Some(mode.as_str()) {
            self.mode = Some(mode.clone());
            self.edges.clear();
            self.period = None;
        }
        let (allowed, _) = variable(&mode)?;
        self.edges.push(now);
        if self.edges.len() > 4 {
            self.edges.remove(0);
        }
        let snapped: Vec<Option<u32>> =
            self.edges.windows(2).map(|w| snap(w[1] - w[0], allowed)).collect();
        let n = snapped.len();
        if n >= 2 && snapped[n - 1].is_some() && snapped[n - 1] == snapped[n - 2] {
            self.period = snapped[n - 1];
        }
        self.period
    }
}

/// Fill fraction of WSJT-X's own T/R progress bar at time `now` (epoch seconds).
///
/// WSJT-X refreshes the bar once per UTC second with value = int(fmod(second, period))
/// and a maximum of int(period), so it steps each second and, for a period like FT2's
/// 3.75 s, wraps every 4 s with the last step full. Matching it keeps the two bars in
/// lockstep (user request).
pub fn wsjtx_progress(now: f64, period: f64) -> f64 {
    let top = (period as i64).max(1) as f64;
    let value = ((now.trunc()) % period).trunc();
    (value / top).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_modes_ignore_a_bogus_tr_period() {
        assert_eq!(cycle_seconds("FT8", 4294967295, None), 15.0);
        assert_eq!(cycle_seconds("FT4", 7, None), 7.5);
        assert_eq!(cycle_seconds("FT2", 4294967295, None), 3.75);
        assert_eq!(cycle_seconds("JT65", 0, None), 60.0);
        assert_eq!(cycle_seconds("WSPR", 0, None), 120.0);
    }

    #[test]
    fn variable_modes() {
        assert_eq!(cycle_seconds("Q65", 30, None), 30.0);
        assert_eq!(cycle_seconds("Q65", 4294967295, None), 60.0);
        assert_eq!(cycle_seconds("Q65", 4294967295, Some(120)), 120.0);
        assert_eq!(cycle_seconds("FST4", 7, None), 60.0);
        assert_eq!(cycle_seconds("FST4W", 0, None), 120.0);
    }

    #[test]
    fn unknown_mode() {
        assert_eq!(cycle_seconds("NEWMODE", 20, None), 20.0);
        assert_eq!(cycle_seconds("NEWMODE", 4294967295, None), 15.0);
    }

    #[test]
    fn snapping() {
        assert_eq!(snap(61.2, &[15, 30, 60, 120]), Some(60));
        assert_eq!(snap(45.0, &[15, 30, 60, 120]), None);
    }

    #[test]
    fn detector_needs_two_matching_intervals() {
        let mut d = PeriodDetector::default();
        assert_eq!(d.decoding_started("Q65", 100.0), None);
        assert_eq!(d.decoding_started("Q65", 130.2), None);
        assert_eq!(d.decoding_started("Q65", 160.1), Some(30));
        assert_eq!(d.decoding_started("FST4", 200.0), None);
        assert_eq!(d.decoding_started("FT8", 215.0), None);
    }

    #[test]
    fn progress_steps_like_wsjtxs_bar() {
        let steps: Vec<f64> = (0..8)
            .map(|s| (wsjtx_progress(s as f64 + 0.6, 3.75) * 100.0).round() / 100.0)
            .collect();
        assert_eq!(steps, [0.0, 0.33, 0.67, 1.0, 0.0, 0.33, 0.67, 1.0]);
        assert_eq!(wsjtx_progress(60.0 * 5.0 + 14.9, 15.0), 14.0 / 15.0);
        assert_eq!(wsjtx_progress(60.0 * 5.0 + 15.0, 15.0), 0.0);
    }
}
