use std::path::Path;

use anyhow::Context;
use recorder_core::Config;

use crate::cli::ConfigCommand;

pub fn run(command: ConfigCommand, config_path: Option<&Path>) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Init => {
            let path = Config::init_default_file().context("could not create config file")?;
            println!("wrote default config to {}", path.display());
            Ok(())
        }
        ConfigCommand::Path => {
            let path = match config_path {
                Some(p) => p.to_owned(),
                None => Config::default_path()?,
            };
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::Check => {
            let (config, source) = if let Some(p) = config_path {
                (Config::load(p)?, p.display().to_string())
            } else {
                let default = Config::default_path()?;
                if default.exists() {
                    (Config::load(&default)?, default.display().to_string())
                } else {
                    (
                        Config::default(),
                        "built-in defaults (no config file)".into(),
                    )
                }
            };
            println!("config OK ({source})");
            println!(
                "  storage.root        = {}",
                config.storage.root_path().display()
            );
            println!(
                "  storage.retention   = {}",
                humantime::format_duration(config.storage.retention)
            );
            println!(
                "  segment_duration    = {}",
                humantime::format_duration(config.storage.segment_duration)
            );
            println!("  capture.mode        = {:?}", config.capture.mode);
            println!("  capture.mic         = {}", config.capture.mic);
            println!("  capture.output      = {}", config.capture.output);
            println!("  fanout.enabled      = {}", config.fanout.enabled);
            Ok(())
        }
    }
}
