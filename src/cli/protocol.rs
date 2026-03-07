use serde::{Deserialize, Serialize};

/// Heartbeat message for ping/pong liveness checks over NIP-17 encrypted DMs.
///
/// Before submitting a job, the customer pings candidate providers to verify
/// they are online. A provider that responds with a matching pong (same nonce)
/// is considered live and eligible for job selection.
///
/// Wire format (JSON inside NIP-17 envelope):
/// ```json
/// { "type": "elisym_ping", "nonce": "<random_bs58>" }
/// { "type": "elisym_pong", "nonce": "<echo_back_same_nonce>" }
/// ```
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

/// Generate a cryptographically random nonce using OS entropy.
pub fn random_nonce() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("failed to generate random bytes");
    bs58::encode(&buf).into_string()
}
