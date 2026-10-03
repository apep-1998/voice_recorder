//! Error types for the core library.

use std::path::PathBuf;

/// Errors arising from loading, parsing, or validating the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write config file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config file {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },

    #[error("config file already exists at {path}")]
    AlreadyExists { path: PathBuf },

    #[error("could not determine the user config directory")]
    NoConfigDir,

    #[error("invalid config: {0}")]
    Invalid(String),
}
