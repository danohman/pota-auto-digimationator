// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! Respotting an activator to pota.app after a logged QSO.

use std::time::Duration;

use serde_json::{json, Value};

use crate::pota_auth::{FetchError, USER_AGENT};
use crate::wsjtx_udp::QsoLogged;

pub const SPOT_URL: &str = "https://api.pota.app/spot";

/// The placeholders a respot message may contain, with what each stands for.
pub const PLACEHOLDERS: &[(&str, &str)] = &[
    ("{RST_RCVD}", "report you received (from the logged QSO)"),
    ("{RST_SENT}", "report you sent"),
    ("{CALL}", "the activator's call"),
    ("{PARK}", "park reference"),
    ("{FREQ}", "frequency in kHz"),
    ("{BAND}", "band"),
    ("{MODE}", "mode"),
    ("{MY_CALL}", "your call"),
    ("{APP_VERSION}", "this program's version"),
];

pub fn source() -> String {
    format!("POTA Auto-Digimationator {}", env!("CARGO_PKG_VERSION"))
}

/// What goes into one respot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Respot {
    pub activator: String,
    pub spotter: String,
    pub reference: String,
    pub freq_khz: String,
    pub mode: String,
    pub band: String,
    pub rst_sent: String,
    pub rst_rcvd: String,
}

impl Respot {
    pub fn new(q: &QsoLogged, my_call: &str, reference: &str, band: &str) -> Self {
        let khz = q.tx_freq as f64 / 1000.0;
        Respot {
            activator: q.dx_call.to_uppercase(),
            spotter: if q.my_call.is_empty() { my_call } else { &q.my_call }.to_uppercase(),
            reference: reference.to_string(),
            freq_khz: if khz.fract() == 0.0 { format!("{khz:.0}") } else { format!("{khz:.1}") },
            mode: q.mode.to_uppercase(),
            band: band.to_string(),
            rst_sent: q.report_sent.trim().to_string(),
            rst_rcvd: q.report_rcvd.trim().to_string(),
        }
    }

    pub fn comments(&self, template: &str) -> String {
        let subs = [
            ("{RST_RCVD}", &self.rst_rcvd),
            ("{RST_SENT}", &self.rst_sent),
            ("{CALL}", &self.activator),
            ("{PARK}", &self.reference),
            ("{FREQ}", &self.freq_khz),
            ("{BAND}", &self.band),
            ("{MODE}", &self.mode),
            ("{MY_CALL}", &self.spotter),
            ("{APP_VERSION}", &env!("CARGO_PKG_VERSION").to_string()),
        ];
        let mut out = template.to_string();
        for (k, v) in subs {
            out = out.replace(k, v);
        }
        out.trim().to_string()
    }

    pub fn body(&self, template: &str) -> Value {
        json!({
            "activator": self.activator,
            "spotter": self.spotter,
            "frequency": self.freq_khz,
            "reference": self.reference,
            "mode": self.mode,
            "source": source(),
            "comments": self.comments(template),
        })
    }
}

pub fn post(id_token: &str, body: &Value) -> Result<(), FetchError> {
    ureq::post(SPOT_URL)
        .set("Authorization", id_token)
        .set("Content-Type", "application/json")
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(20))
        .send_string(&body.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_and_source() {
        let q = QsoLogged { dx_call: "w1abc".into(), tx_freq: 14_074_000, mode: "FT8".into(),
                            report_sent: "-05".into(), report_rcvd: " -10 ".into(), my_call: "WD4DAN".into(), ..Default::default() };
        let r = Respot::new(&q, "", "US-1234", "20m");
        let b = r.body("Heard you {RST_RCVD}, sent {RST_SENT} on {BAND} {MODE} {FREQ} ({APP_VERSION})");
        assert_eq!(b["activator"], "W1ABC");
        assert_eq!(b["spotter"], "WD4DAN");
        assert_eq!(b["frequency"], "14074");
        assert_eq!(b["reference"], "US-1234");
        assert_eq!(b["source"], format!("POTA Auto-Digimationator {}", env!("CARGO_PKG_VERSION")));
        let c = b["comments"].as_str().unwrap();
        assert!(c.starts_with("Heard you -10, sent -05 on 20m FT8 14074 ("), "{c}");
        assert!(!c.contains('{'));
    }
}
