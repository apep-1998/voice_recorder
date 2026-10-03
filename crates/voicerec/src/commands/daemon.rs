use std::path::Path;

use anyhow::Context;
use recorder_core::Config;
use tokio::sync::watch;

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::load_default()?,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start async runtime")?;

    runtime.block_on(async move {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        tokio::spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
                _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            }
            let _ = shutdown_tx.send(true);
        });

        recorder_core::recorder::run(&config, shutdown_rx)
            .await
            .context("recorder failed")
    })
}
