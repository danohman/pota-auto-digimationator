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
    PotaStats(Arc<HuntCounts>),
    PotaStatsError(String),
    Aclog(String, Tag),
    /// (what, value, result) of pressing a WSJT-X band/mode button
    Retune(String, String, String),
    /// result of starting our CQ through UI Automation
    CqTx(String),
}

/// Hands an event to the UI thread (and wakes it up).
pub type Post = Arc<dyn Fn(Event) + Send + Sync>;
