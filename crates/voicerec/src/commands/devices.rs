use std::path::Path;

use anyhow::Context;
use recorder_core::capture::pipewire::{snapshot_graph, DEFAULT_SNAPSHOT_TIMEOUT};
use recorder_core::device::{select_targets, NodeInfo, StreamKind};
use recorder_core::Config;

pub fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::load_default()?,
    };

    let graph = snapshot_graph(DEFAULT_SNAPSHOT_TIMEOUT)
        .context("could not enumerate PipeWire audio devices")?;

    print_section(
        "Microphones (sources)",
        graph.mics(),
        graph.default_source.as_deref(),
    );
    println!();
    print_section(
        "Outputs (sinks, recorded via monitor)",
        graph.sinks(),
        graph.default_sink.as_deref(),
    );
    println!();

    match select_targets(&graph, &config.capture) {
        Ok(targets) => {
            println!(
                "With the current config ({:?} mode) these would be recorded:",
                config.capture.mode
            );
            for t in targets {
                let kind = match t.kind {
                    StreamKind::Mic => "mic    ",
                    StreamKind::Monitor => "monitor",
                };
                println!("  [{kind}] {}  (stored as {})", t.node_name, t.slug);
            }
        }
        Err(err) => println!("Current config selects no recordable devices: {err}"),
    }

    Ok(())
}

fn print_section<'a>(
    title: &str,
    nodes: impl Iterator<Item = &'a NodeInfo>,
    default_name: Option<&str>,
) {
    println!("{title}:");
    let mut any = false;
    for node in nodes {
        any = true;
        let marker = if Some(node.name.as_str()) == default_name {
            "* "
        } else {
            "  "
        };
        let desc = node.description.as_deref().unwrap_or("");
        println!("{marker}{}  —  {desc}", node.name);
    }
    if !any {
        println!("  (none found)");
    }
}
