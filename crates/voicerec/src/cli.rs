use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Continuous background audio recorder for Linux.
#[derive(Debug, Parser)]
#[command(name = "voicerec", version, about)]
pub struct Cli {
    /// Path to the config file (default: `~/.config/voice_recorder/config.toml`).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect and manage the configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// List audio devices and show which ones would be recorded.
    Devices,
    /// Run the recorder daemon: capture the configured devices continuously
    /// into the segment store (what the systemd service runs).
    Daemon,
    /// List recorded audio in a time range, with per-device coverage and gaps.
    List {
        /// Window ending now, e.g. "2h", "90m", "1h30m".
        #[arg(long, value_name = "DURATION")]
        last: Option<String>,
        /// Range start, e.g. "8h ago", "yesterday 15:50", "2026-10-03 09:00".
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Range end (defaults to now).
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Only devices whose slug contains this substring.
        #[arg(long, value_name = "SUBSTR")]
        device: Option<String>,
    },
    /// Export a time range of one device to a single audio file.
    Export {
        /// Window ending now, e.g. "2h", "90m".
        #[arg(long, value_name = "DURATION")]
        last: Option<String>,
        /// Range start, e.g. "8h ago", "yesterday 15:50".
        #[arg(long, value_name = "TIME")]
        from: Option<String>,
        /// Range end (defaults to now).
        #[arg(long, value_name = "TIME")]
        to: Option<String>,
        /// Device slug substring (required if more than one device exists).
        #[arg(long, value_name = "SUBSTR")]
        device: Option<String>,
        /// Output file; the extension sets the format (.opus/.wav/.flac/.mp3).
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Drop recording gaps instead of filling them with silence (loses
        /// wall-clock alignment).
        #[arg(long)]
        compact: bool,
    },
    /// Show what is stored: segment counts, time span, disk usage.
    Status,
    /// Rebuild the segment index from the files on disk.
    Reindex,
    /// Record the configured devices to raw WAV files for a few seconds
    /// (debugging aid to verify capture works).
    DebugRecord {
        /// How long to record.
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Directory to write one WAV file per device into.
        #[arg(long, value_name = "DIR")]
        output_dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum ConfigCommand {
    /// Write the default config file to `~/.config/voice_recorder/config.toml`.
    Init,
    /// Validate the config file and print the resolved settings.
    Check,
    /// Print the path of the config file in use.
    Path,
}
