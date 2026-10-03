mod cli;
mod commands;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "voicerec=info,recorder_core=info".into()),
        )
        .init();

    let cli = cli::Cli::parse();
    match cli.command {
        cli::Command::Config { command } => commands::config::run(command, cli.config.as_deref()),
        cli::Command::Devices => commands::devices::run(cli.config.as_deref()),
    }
}
