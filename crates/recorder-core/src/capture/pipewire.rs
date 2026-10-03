//! PipeWire backend.
//!
//! pipewire-rs types are not `Send`; everything that touches the main loop
//! stays on the calling thread. [`snapshot_graph`] runs a short-lived main
//! loop to enumerate nodes and default devices, then tears it down.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::registry::RegistryRc;
use pw::types::ObjectType;

use crate::device::{AudioGraph, NodeInfo, MEDIA_CLASS_SINK, MEDIA_CLASS_SOURCE};
use crate::error::CaptureError;

/// How long [`snapshot_graph`] waits for PipeWire before giving up.
pub const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to PipeWire, enumerate audio nodes and default devices, and
/// return a point-in-time snapshot of the graph.
pub fn snapshot_graph(timeout: Duration) -> Result<AudioGraph, CaptureError> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let graph = Rc::new(RefCell::new(AudioGraph::default()));
    // Bound metadata proxies and their listeners must stay alive until the
    // loop finishes, otherwise their property events never arrive.
    let metadata_guards: Rc<RefCell<Vec<(Metadata, MetadataListener)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let timed_out = Rc::new(RefCell::new(false));

    let registry_weak = registry.downgrade();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let graph = Rc::clone(&graph);
            let metadata_guards = Rc::clone(&metadata_guards);
            move |global| match global.type_ {
                ObjectType::Node => {
                    if let Some(node) = node_info(global.props) {
                        graph.borrow_mut().nodes.push(node);
                    }
                }
                ObjectType::Metadata => {
                    let name = global
                        .props
                        .and_then(|p| p.get("metadata.name"))
                        .unwrap_or_default();
                    if name == "default" {
                        let Some(registry) = registry_weak.upgrade() else {
                            return;
                        };
                        match watch_default_metadata(&registry, global, &graph) {
                            Ok(guard) => metadata_guards.borrow_mut().push(guard),
                            Err(err) => {
                                tracing::warn!("failed to bind default metadata: {err}");
                            }
                        }
                    }
                }
                _ => {}
            }
        })
        .register();

    // Two sync round trips: the first flushes registry globals (during which
    // we bind the "default" metadata), the second flushes the metadata
    // property events produced by that bind.
    let pending = Rc::new(RefCell::new(core.sync(0)?));
    let rounds_left = Rc::new(RefCell::new(1_u32));
    let core_weak = core.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            let pending = Rc::clone(&pending);
            let rounds_left = Rc::clone(&rounds_left);
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == *pending.borrow() {
                    let mut rounds = rounds_left.borrow_mut();
                    if *rounds == 0 {
                        mainloop.quit();
                        return;
                    }
                    *rounds -= 1;
                    match core_weak.upgrade().map(|core| core.sync(0)) {
                        Some(Ok(next)) => *pending.borrow_mut() = next,
                        _ => mainloop.quit(),
                    }
                }
            }
        })
        .register();

    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        let timed_out = Rc::clone(&timed_out);
        move |_| {
            *timed_out.borrow_mut() = true;
            mainloop.quit();
        }
    });
    timer
        .update_timer(Some(timeout), None)
        .into_result()
        .map_err(|err| CaptureError::Internal(format!("failed to arm timeout timer: {err:?}")))?;

    mainloop.run();

    if *timed_out.borrow() {
        return Err(CaptureError::Timeout);
    }
    let snapshot = graph.borrow().clone();
    Ok(snapshot)
}

fn node_info(props: Option<&pw::spa::utils::dict::DictRef>) -> Option<NodeInfo> {
    let props = props?;
    let media_class = props.get("media.class")?;
    if media_class != MEDIA_CLASS_SOURCE && media_class != MEDIA_CLASS_SINK {
        return None;
    }
    Some(NodeInfo {
        name: props.get("node.name")?.to_owned(),
        description: props.get("node.description").map(ToOwned::to_owned),
        media_class: media_class.to_owned(),
    })
}

fn watch_default_metadata(
    registry: &RegistryRc,
    global: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>,
    graph: &Rc<RefCell<AudioGraph>>,
) -> Result<(Metadata, MetadataListener), pw::Error> {
    let metadata: Metadata = registry.bind(global)?;
    let listener = metadata
        .add_listener_local()
        .property({
            let graph = Rc::clone(graph);
            move |_subject, key, _type, value| {
                let name = value.and_then(parse_default_name);
                match key {
                    Some("default.audio.source") => graph.borrow_mut().default_source = name,
                    Some("default.audio.sink") => graph.borrow_mut().default_sink = name,
                    _ => {}
                }
                0
            }
        })
        .register();
    Ok((metadata, listener))
}

/// Metadata values look like `{"name":"alsa_input...."}`.
fn parse_default_name(value: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Named {
        name: String,
    }
    serde_json::from_str::<Named>(value).ok().map(|n| n.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_metadata_value() {
        assert_eq!(
            parse_default_name(r#"{"name":"alsa_input.usb-foo.analog-stereo"}"#),
            Some("alsa_input.usb-foo.analog-stereo".to_owned())
        );
        assert_eq!(parse_default_name("not json"), None);
        assert_eq!(parse_default_name("{}"), None);
    }

    /// Live test against the real PipeWire daemon; run with
    /// `VOICEREC_LIVE_TESTS=1 cargo test -- --ignored`.
    #[test]
    #[ignore = "requires a running PipeWire daemon"]
    fn live_snapshot_sees_audio_nodes() {
        if std::env::var("VOICEREC_LIVE_TESTS").is_err() {
            eprintln!("VOICEREC_LIVE_TESTS not set; skipping");
            return;
        }
        let graph = snapshot_graph(DEFAULT_SNAPSHOT_TIMEOUT).expect("snapshot");
        assert!(graph.mics().count() > 0, "no mics found: {graph:?}");
        assert!(graph.sinks().count() > 0, "no sinks found: {graph:?}");
    }
}
