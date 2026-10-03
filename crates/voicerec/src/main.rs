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
        cli::Command::Daemon => commands::daemon::run(cli.config.as_deref()),
        cli::Command::Status => commands::status::run(cli.config.as_deref()),
        cli::Command::Reindex => commands::reindex::run(cli.config.as_deref()),
        cli::Command::DebugRecord {
            seconds,
            output_dir,
        } => commands::debug_record::run(cli.config.as_deref(), seconds, &output_dir),
    }
}
