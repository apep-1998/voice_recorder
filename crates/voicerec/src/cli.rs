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
