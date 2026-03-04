use thiserror::Error;

pub type Result<T> = std::result::Result<T, CliError>;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("elisym: {0}")]
    Elisym(#[from] elisym_core::ElisymError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml deserialize: {0}")]
    TomlDe(#[from] toml::de::Error),

    #[error("toml serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),

    #[error("dialoguer: {0}")]
    Dialoguer(#[from] dialoguer::Error),

    #[error("LLM error: {0}")]
    Llm(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{0}")]
    Other(String),
}
