use std::fs;
use std::path::PathBuf;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use super::error::{CliError, Result};

/// Validate that an agent name is safe for use as a directory name.
/// Allows ASCII alphanumeric characters, hyphens, and underscores only.
pub fn validate_agent_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(CliError::Other("agent name cannot be empty".into()));
    }
    if name.len() > 64 {
        return Err(CliError::Other("agent name must be 64 characters or fewer".into()));
    }
    if name == "." || name == ".." {
        return Err(CliError::Other("invalid agent name".into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CliError::Other(
            "agent name may only contain letters, digits, hyphens, and underscores".into(),
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub relays: Vec<String>,
    pub secret_key: String,
    pub payment: PaymentSection,
    #[serde(default)]
    pub llm: Option<LlmSection>,
    /// Deprecated: ignored on read, not written. Kept for backwards compat with old configs.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    pub customer_llm: Option<LlmSection>,
    #[serde(default)]
    pub encryption: Option<super::crypto::EncryptionSection>,
    #[serde(default)]
    pub recovery: RecoverySection,
    #[serde(default)]
    pub social: SocialSection,
}

// Custom Debug impl to avoid leaking secret keys and API keys in logs.
impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("capabilities", &self.capabilities)
            .field("relays", &self.relays)
            .field("secret_key", &"[REDACTED]")
            .field("payment", &self.payment)
            .field("llm", &self.llm)
            .field("customer_llm", &"[deprecated]")
            .field("encryption", &self.encryption.is_some())
            .finish()
    }
}

impl AgentConfig {
    /// Returns true if secrets are encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encryption.is_some()
    }

    /// Bundle all secret fields for encryption.
    pub fn secrets_bundle(&self) -> super::crypto::SecretsBundle {
        super::crypto::SecretsBundle {
            nostr_secret_key: self.secret_key.clone(),
            solana_secret_key: self.payment.solana_secret_key.clone(),
            llm_api_key: self.llm.as_ref().map(|l| l.api_key.clone()).unwrap_or_default(),
            customer_llm_api_key: None,
        }
    }

    /// Encrypt secrets with password and clear plaintext fields in-place.
    pub fn encrypt_secrets(&mut self, password: &str) -> Result<()> {
        let bundle = self.secrets_bundle();
        let section = super::crypto::encrypt_secrets(&bundle, password)?;
        self.encryption = Some(section);
        self.secret_key = String::new();
        self.payment.solana_secret_key = String::new();
        if let Some(ref mut llm) = self.llm {
            llm.api_key = String::new();
        }
        Ok(())
    }

    /// Decrypt secrets with password and populate plaintext fields in-place.
    pub fn decrypt_secrets(&mut self, password: &str) -> Result<()> {
        let section = self.encryption.as_ref()
            .ok_or_else(|| CliError::Other("config is not encrypted".into()))?;
        let bundle = super::crypto::decrypt_secrets(section, password)?;
        self.secret_key = bundle.nostr_secret_key;
        self.payment.solana_secret_key = bundle.solana_secret_key;
        if let Some(ref mut llm) = self.llm {
            llm.api_key = bundle.llm_api_key;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LlmSection {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

// Custom Debug impl to avoid leaking API keys in logs.
impl std::fmt::Debug for LlmSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmSection")
            .field("provider", &self.provider)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_job_price() -> u64 {
    10_000_000 // 0.01 SOL
}

fn default_payment_timeout_secs() -> u32 {
    120
}


#[derive(Clone, Serialize, Deserialize)]
pub struct PaymentSection {
    pub chain: String,
    pub network: String,
    #[serde(default)]
    pub rpc_url: Option<String>,
    #[serde(default = "default_job_price")]
    pub job_price: u64,
    #[serde(default = "default_payment_timeout_secs")]
    pub payment_timeout_secs: u32,
    pub solana_secret_key: String,
}

// Custom Debug impl to avoid leaking Solana secret key in logs.
impl std::fmt::Debug for PaymentSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentSection")
            .field("chain", &self.chain)
            .field("network", &self.network)
            .field("rpc_url", &self.rpc_url)
            .field("job_price", &self.job_price)
            .field("payment_timeout_secs", &self.payment_timeout_secs)
            .field("solana_secret_key", &"[REDACTED]")
            .finish()
    }
}

impl Default for PaymentSection {
    fn default() -> Self {
        Self {
            chain: "solana".to_string(),
            network: "devnet".to_string(),
            rpc_url: None,
            job_price: 10_000_000, // 0.01 SOL in lamports
            payment_timeout_secs: 120,
            solana_secret_key: String::new(),
        }
    }
}

impl PaymentSection {
    /// Derive the Solana public address from the stored secret key for display.
    pub fn solana_address(&self) -> Option<String> {
        let bytes = bs58::decode(&self.solana_secret_key).into_vec().ok()?;
        let keypair = solana_sdk::signature::Keypair::try_from(bytes.as_slice()).ok()?;
        Some(solana_sdk::signer::Signer::pubkey(&keypair).to_string())
    }
}

fn default_delivery_retries() -> u32 {
    3
}

fn default_recovery_max_retries() -> u32 {
    5
}

fn default_recovery_interval_secs() -> u64 {
    60
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoverySection {
    #[serde(default = "default_delivery_retries")]
    pub delivery_retries: u32,
    #[serde(default = "default_recovery_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_recovery_interval_secs")]
    pub interval_secs: u64,
}

impl Default for RecoverySection {
    fn default() -> Self {
        Self {
            delivery_retries: 3,
            max_retries: 5,
            interval_secs: 60,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SocialSection {
    /// Automatically like and repost elisymlabs posts.
    #[serde(default)]
    pub auto_engage: bool,
    /// Publish a Nostr note after each completed job.
    #[serde(default)]
    pub publish_notes: bool,
}

impl Default for SocialSection {
    fn default() -> Self {
        Self {
            auto_engage: false,
            publish_notes: false,
        }
    }
}

/// Root directory: ~/.elisym/agents/
fn agents_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| CliError::Other("cannot find home directory".into()))?;
    Ok(home.join(".elisym").join("agents"))
}

/// Directory for a specific agent: ~/.elisym/agents/<name>/
pub fn agent_dir(name: &str) -> Result<PathBuf> {
    validate_agent_name(name)?;
    Ok(agents_root()?.join(name))
}

/// Path to config.toml for a specific agent
pub fn config_path(name: &str) -> Result<PathBuf> {
    Ok(agent_dir(name)?.join("config.toml"))
}

/// Save agent config to disk, creating directories as needed
pub fn save_config(config: &AgentConfig) -> Result<()> {
    let dir = agent_dir(&config.name)?;
    fs::create_dir_all(&dir)?;

    let toml_str = toml::to_string_pretty(config)?;
    let path = config_path(&config.name)?;
    fs::write(&path, toml_str)?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Load agent config from disk
pub fn load_config(name: &str) -> Result<AgentConfig> {
    let path = config_path(name)?;
    let contents = fs::read_to_string(&path).map_err(|e| {
        CliError::Other(format!("agent '{}' not found ({})", name, e))
    })?;
    let config: AgentConfig = toml::from_str(&contents)?;
    Ok(config)
}

/// Load agent config from disk, stripping all secret material.
/// Use this for read-only commands (status, list) that don't need secrets.
pub fn load_config_public(name: &str) -> Result<AgentConfig> {
    let mut config = load_config(name)?;
    config.secret_key.zeroize();
    config.payment.solana_secret_key.zeroize();
    if let Some(ref mut llm) = config.llm {
        llm.api_key.zeroize();
    }
    Ok(config)
}

/// List all configured agent names
pub fn list_agents() -> Result<Vec<String>> {
    let root = agents_root()?;
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                let cfg = entry.path().join("config.toml");
                if cfg.exists() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Delete an agent directory entirely
pub fn delete_agent(name: &str) -> Result<()> {
    let dir = agent_dir(name)?;
    if !dir.exists() {
        return Err(CliError::Other(format!("agent '{}' not found", name)));
    }
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;

    // ── validate_agent_name ──────────────────────────────────────────

    #[test]
    fn validate_agent_name_empty_returns_err() {
        assert!(validate_agent_name("").is_err());
    }

    #[test]
    fn validate_agent_name_dot_returns_err() {
        assert!(validate_agent_name(".").is_err());
    }

    #[test]
    fn validate_agent_name_dotdot_returns_err() {
        assert!(validate_agent_name("..").is_err());
    }

    #[test]
    fn validate_agent_name_too_long_returns_err() {
        let long = "a".repeat(65);
        assert!(validate_agent_name(&long).is_err());
    }

    #[test]
    fn validate_agent_name_valid_returns_ok() {
        assert!(validate_agent_name("my-agent_1").is_ok());
    }

    #[test]
    fn validate_agent_name_spaces_returns_err() {
        assert!(validate_agent_name("my agent").is_err());
    }

    #[test]
    fn validate_agent_name_slashes_returns_err() {
        assert!(validate_agent_name("my/agent").is_err());
    }

    #[test]
    fn validate_agent_name_unicode_returns_err() {
        assert!(validate_agent_name("агент").is_err());
    }

    // ── PaymentSection::solana_address ───────────────────────────────

    #[test]
    fn solana_address_empty_key_returns_none() {
        let ps = PaymentSection::default();
        assert!(ps.solana_address().is_none());
    }

    #[test]
    fn solana_address_invalid_bs58_returns_none() {
        let ps = PaymentSection {
            solana_secret_key: "not-valid-bs58!!!".to_string(),
            ..Default::default()
        };
        assert!(ps.solana_address().is_none());
    }

    #[test]
    fn solana_address_valid_keypair_returns_pubkey() {
        let kp = Keypair::new();
        let encoded = bs58::encode(kp.to_bytes()).into_string();
        let ps = PaymentSection {
            solana_secret_key: encoded,
            ..Default::default()
        };
        let addr = ps.solana_address().expect("should derive address");
        assert_eq!(addr, kp.pubkey().to_string());
    }

    // ── AgentConfig TOML roundtrip ───────────────────────────────────

    #[test]
    fn agent_config_toml_roundtrip() {
        let config = AgentConfig {
            name: "test-agent".to_string(),
            description: "A test agent".to_string(),
            capabilities: vec!["cap-a".to_string(), "cap-b".to_string()],
            relays: vec!["wss://relay.damus.io".to_string()],
            secret_key: "nsec_placeholder".to_string(),
            payment: PaymentSection::default(),
            llm: Some(LlmSection {
                provider: "anthropic".to_string(),
                api_key: "sk-test-key".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                max_tokens: 8192,
            }),
            customer_llm: None,
            encryption: None,
            recovery: RecoverySection::default(),
            social: SocialSection::default(),
        };

        let toml_str = toml::to_string_pretty(&config).expect("serialize");
        let deserialized: AgentConfig = toml::from_str(&toml_str).expect("deserialize");

        assert_eq!(deserialized.name, config.name);
        assert_eq!(deserialized.description, config.description);
        assert_eq!(deserialized.capabilities, config.capabilities);
        assert_eq!(deserialized.relays, config.relays);
        assert_eq!(deserialized.secret_key, config.secret_key);
        assert_eq!(deserialized.payment.chain, config.payment.chain);
        assert_eq!(deserialized.payment.network, config.payment.network);
        assert_eq!(deserialized.payment.job_price, config.payment.job_price);

        let llm = deserialized.llm.expect("llm section present");
        assert_eq!(llm.provider, "anthropic");
        assert_eq!(llm.model, "claude-sonnet-4-20250514");
        assert_eq!(llm.max_tokens, 8192);
    }

    // ── AgentConfig::secrets_bundle ──────────────────────────────────

    #[test]
    fn secrets_bundle_fields_populated() {
        let config = AgentConfig {
            name: "bundle-test".to_string(),
            description: String::new(),
            capabilities: vec![],
            relays: vec![],
            secret_key: "nostr-secret".to_string(),
            payment: PaymentSection {
                solana_secret_key: "solana-secret".to_string(),
                ..Default::default()
            },
            llm: Some(LlmSection {
                provider: "anthropic".to_string(),
                api_key: "llm-api-key".to_string(),
                model: "claude-sonnet-4-20250514".to_string(),
                max_tokens: 4096,
            }),
            customer_llm: None,
            encryption: None,
            recovery: RecoverySection::default(),
            social: SocialSection::default(),
        };

        let bundle = config.secrets_bundle();
        assert_eq!(bundle.nostr_secret_key, "nostr-secret");
        assert_eq!(bundle.solana_secret_key, "solana-secret");
        assert_eq!(bundle.llm_api_key, "llm-api-key");
    }

    #[test]
    fn secrets_bundle_without_llm_section() {
        let config = AgentConfig {
            name: "no-llm".to_string(),
            description: String::new(),
            capabilities: vec![],
            relays: vec![],
            secret_key: "nostr-key".to_string(),
            payment: PaymentSection::default(),
            llm: None,
            customer_llm: None,
            encryption: None,
            recovery: RecoverySection::default(),
            social: SocialSection::default(),
        };

        let bundle = config.secrets_bundle();
        assert_eq!(bundle.nostr_secret_key, "nostr-key");
        assert!(bundle.llm_api_key.is_empty());
    }
}
