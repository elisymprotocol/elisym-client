use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CliError, Result};

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub relays: Vec<String>,
    pub secret_key: String,
    pub payment: PaymentSection,
    #[serde(default)]
    pub llm: Option<LlmSection>,
    #[serde(default)]
    pub customer_llm: Option<LlmSection>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LlmSection {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    4096
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentSection {
    pub chain: String,
    pub network: String,
    #[serde(default)]
    pub rpc_url: Option<String>,
    pub token: String,
    pub job_price: u64,
    pub payment_timeout_secs: u32,
    pub solana_secret_key: String,
}

impl Default for PaymentSection {
    fn default() -> Self {
        Self {
            chain: "solana".to_string(),
            network: "devnet".to_string(),
            rpc_url: None,
            token: "sol".to_string(),
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

/// Root directory: ~/.elisym/agents/
fn agents_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| CliError::Other("cannot find home directory".into()))?;
    Ok(home.join(".elisym").join("agents"))
}

/// Directory for a specific agent: ~/.elisym/agents/<name>/
pub fn agent_dir(name: &str) -> Result<PathBuf> {
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
    fs::write(config_path(&config.name)?, toml_str)?;
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
