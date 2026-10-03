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

/// Errors from the PipeWire capture backend.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("PipeWire error")]
    PipeWire(#[from] pipewire::Error),

    #[error("timed out talking to PipeWire — is the PipeWire daemon running?")]
    Timeout,

    #[error("{0}")]
    Internal(String),
}

/// Errors writing or reading Ogg/Opus segments.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("opus codec error")]
    Opus(#[from] opus::Error),

    #[error("I/O error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{0} is not an Ogg/Opus file")]
    NotOpus(PathBuf),

    #[error("unsupported channel count {0} (only 1 or 2 supported)")]
    BadChannels(u8),

    #[error("invalid Opus bitrate {0}")]
    BadBitrate(u32),
}

/// Errors from the recorder service (daemon).
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error(transparent)]
    Encode(#[from] EncodeError),

    #[error(transparent)]
    Index(#[from] IndexError),

    #[error(transparent)]
    Selection(#[from] crate::device::SelectionError),

    #[error("no devices selected for recording")]
    NothingToRecord,

    #[error("I/O error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Errors from exporting a time range.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Index(#[from] IndexError),

    #[error("no audio in the requested time range")]
    NoAudio,

    #[error("could not run ffmpeg (is it installed and on PATH?)")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("ffmpeg failed:\n{stderr}")]
    Ffmpeg { stderr: String },
}

/// Errors from the SQLite segment index.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("database error")]
    Sqlite(#[from] rusqlite::Error),

    #[error("I/O error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
