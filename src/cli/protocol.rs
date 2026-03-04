use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Heartbeat message for ping/pong liveness checks (NIP-17 encrypted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub nonce: String,
}

impl HeartbeatMessage {
    pub fn ping(nonce: String) -> Self {
        Self {
            msg_type: "elisym_ping".into(),
            nonce,
        }
    }

    pub fn pong(nonce: String) -> Self {
        Self {
            msg_type: "elisym_pong".into(),
            nonce,
        }
    }

    pub fn is_ping(&self) -> bool {
        self.msg_type == "elisym_ping"
    }

    pub fn is_pong(&self) -> bool {
        self.msg_type == "elisym_pong"
    }
}

/// Generate a unique nonce from timestamp + atomic counter (no extra deps).
pub fn random_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}", ts, count)
}
