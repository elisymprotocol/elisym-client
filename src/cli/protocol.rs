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
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub nonce: String,
}

#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn random_nonce() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("failed to generate random bytes");
    bs58::encode(&buf).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_constructor() {
        let msg = HeartbeatMessage::ping("abc123".into());
        assert_eq!(msg.msg_type, "elisym_ping");
        assert_eq!(msg.nonce, "abc123");
    }

    #[test]
    fn pong_constructor() {
        let msg = HeartbeatMessage::pong("xyz789".into());
        assert_eq!(msg.msg_type, "elisym_pong");
        assert_eq!(msg.nonce, "xyz789");
    }

    #[test]
    fn is_ping_is_pong_predicates() {
        let ping = HeartbeatMessage::ping("n".into());
        assert!(ping.is_ping());
        assert!(!ping.is_pong());

        let pong = HeartbeatMessage::pong("n".into());
        assert!(pong.is_pong());
        assert!(!pong.is_ping());
    }

    #[test]
    fn json_serde_roundtrip() {
        let original = HeartbeatMessage::ping("roundtrip_nonce".into());
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: HeartbeatMessage = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized.msg_type, original.msg_type);
        assert_eq!(deserialized.nonce, original.nonce);

        // Verify the "type" rename in JSON
        let value: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(value.get("type").is_some());
        assert!(value.get("msg_type").is_none());
    }

    #[test]
    fn random_nonce_non_empty_and_unique() {
        let a = random_nonce();
        let b = random_nonce();
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        assert_ne!(a, b);
    }
}
