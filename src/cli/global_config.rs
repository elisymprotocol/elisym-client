use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::error::{CliError, Result};

/// Global elisym settings at ~/.elisym/config.toml
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Last-used agent name, persisted by `switch_agent`.
    /// Not written by default — only set after an explicit switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(default)]
    pub tui: TuiSection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TuiSection {
    /// Play system sound on job completed.
    #[serde(default = "default_true")]
    pub sound_enabled: bool,
    /// Sound volume 0.0–1.0 (macOS afplay -v).
    #[serde(default = "default_volume")]
    pub sound_volume: f32,
}

impl Default for TuiSection {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.15,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_volume() -> f32 {
    0.15
}

fn global_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| CliError::Other("cannot find home directory".into()))?;
    Ok(home.join(".elisym").join("config.toml"))
}

pub fn load_global_config() -> GlobalConfig {
    let path = match global_config_path() {
        Ok(p) => p,
        Err(_) => return GlobalConfig::default(),
    };
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return GlobalConfig::default(),
    };
    toml::from_str(&contents).unwrap_or_default()
}

pub fn save_global_config(config: &GlobalConfig) -> Result<()> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml_str = toml::to_string_pretty(config)?;
    fs::write(&path, toml_str)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_config_toml_roundtrip() {
        let gc = GlobalConfig {
            default_agent: Some("my-agent".into()),
            tui: TuiSection {
                sound_enabled: false,
                sound_volume: 0.5,
            },
        };
        let toml_str = toml::to_string_pretty(&gc).unwrap();
        let parsed: GlobalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.default_agent.as_deref(), Some("my-agent"));
        assert!(!parsed.tui.sound_enabled);
        assert!((parsed.tui.sound_volume - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_tui_section_uses_defaults() {
        let toml_str = r#"default_agent = "foo""#;
        let parsed: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_agent.as_deref(), Some("foo"));
        // tui section should have defaults
        assert!(parsed.tui.sound_enabled);
        assert!((parsed.tui.sound_volume - 0.15).abs() < f32::EPSILON);
    }
}
