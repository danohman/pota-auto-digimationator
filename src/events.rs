// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dan Muntz (WD4DAN)
//! What worker threads post to the UI thread.

use std::sync::Arc;

use crate::hunter::Tag;
use crate::pota_auth::HuntCounts;
use crate::pota_spots::SpotTable;
use crate::wsjtx_udp::Message;

pub enum Event {
    Udp(Message),
    UdpInfo(String),
    UdpError(String),
    Spots(Arc<SpotTable>),
    SpotsError(String),
    /// whether the last pota.app spot fetch worked (colours the status dot)
    SpotsOk(bool),
    PotaStats(Arc<HuntCounts>),
    PotaStatsError(String),
    Aclog(String, Tag),
    /// (what, value, result) of pressing a WSJT-X band/mode button
    Retune(String, String, String),
    /// result of starting our CQ through UI Automation
    CqTx(String),
    /// result of the Radio tab's "Test CAT Control": (all passed, lines)
    CatTest(bool, Vec<String>),
}

/// Hands an event to the UI thread (and wakes it up).
pub type Post = Arc<dyn Fn(Event) + Send + Sync>;
