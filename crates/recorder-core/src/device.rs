//! Device identity and selection.
//!
//! Pure logic: given a snapshot of the PipeWire graph (nodes plus default
//! device names), resolve the configured capture selection into concrete
//! capture targets. Enumerating the live graph lives in
//! [`crate::capture::pipewire`].

use serde::{Deserialize, Serialize};

use crate::config::{CaptureConfig, CaptureMode, MicSelector, OutputSelector};

/// PipeWire media classes we care about.
pub const MEDIA_CLASS_SOURCE: &str = "Audio/Source";
pub const MEDIA_CLASS_SINK: &str = "Audio/Sink";

/// What kind of audio a capture target records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// A microphone (an `Audio/Source` node).
    Mic,
    /// The playback of an output device (an `Audio/Sink` node, captured via
    /// its monitor).
    Monitor,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::Monitor => "monitor",
        }
    }
}

impl std::str::FromStr for StreamKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mic" => Ok(Self::Mic),
            "monitor" => Ok(Self::Monitor),
            other => Err(format!("unknown stream kind {other:?}")),
        }
    }
}

/// One audio node from the PipeWire graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// Stable PipeWire `node.name` (bus-path based, survives reboots).
    pub name: String,
    /// Human-readable `node.description`.
    pub description: Option<String>,
    /// PipeWire `media.class`, e.g. `Audio/Source` or `Audio/Sink`.
    pub media_class: String,
}

impl NodeInfo {
    pub fn is_mic(&self) -> bool {
        self.media_class == MEDIA_CLASS_SOURCE
    }

    pub fn is_sink(&self) -> bool {
        self.media_class == MEDIA_CLASS_SINK
    }
}

/// A snapshot of the audio graph: all nodes plus the current defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioGraph {
    pub nodes: Vec<NodeInfo>,
    /// `node.name` of the default source (microphone), if any.
    pub default_source: Option<String>,
    /// `node.name` of the default sink (output), if any.
    pub default_sink: Option<String>,
}

impl AudioGraph {
    pub fn mics(&self) -> impl Iterator<Item = &NodeInfo> {
        self.nodes.iter().filter(|n| n.is_mic())
    }

    pub fn sinks(&self) -> impl Iterator<Item = &NodeInfo> {
        self.nodes.iter().filter(|n| n.is_sink())
    }
}

/// A concrete device to record.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CaptureTarget {
    pub kind: StreamKind,
    /// The PipeWire node to capture: the source node for a mic, the sink
    /// node for a monitor (captured with `stream.capture.sink`).
    pub node_name: String,
    /// Filesystem-safe identity used for the storage layout. Monitors get a
    /// `.monitor` suffix so they never collide with a mic slug.
    pub slug: String,
}

impl CaptureTarget {
    fn mic(node_name: &str) -> Self {
        Self {
            kind: StreamKind::Mic,
            node_name: node_name.to_owned(),
            slug: slugify(node_name),
        }
    }

    fn monitor(sink_name: &str) -> Self {
        Self {
            kind: StreamKind::Monitor,
            node_name: sink_name.to_owned(),
            slug: format!("{}.monitor", slugify(sink_name)),
        }
    }
}

/// Errors resolving the configured selection against the live graph.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectionError {
    #[error("no microphone found in the audio graph")]
    NoMicAvailable,
    #[error("no output device found in the audio graph")]
    NoOutputAvailable,
    #[error("configured microphone {0:?} not found (see `voicerec devices`)")]
    MicNotFound(String),
    #[error("configured output {0:?} not found or not an output device (see `voicerec devices`)")]
    OutputNotFound(String),
}

/// Turn a PipeWire node name into a filesystem-safe slug.
pub fn slugify(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('.').to_owned();
    if trimmed.is_empty() {
        "_".to_owned()
    } else {
        trimmed
    }
}

/// Resolve the configured capture selection against a graph snapshot.
///
/// In `all` mode every mic and every sink monitor is selected. In `selected`
/// mode the configured mic/output are resolved; `default` falls back to the
/// first available device when PipeWire reports no default.
pub fn select_targets(
    graph: &AudioGraph,
    capture: &CaptureConfig,
) -> Result<Vec<CaptureTarget>, SelectionError> {
    match capture.mode {
        CaptureMode::All => {
            let mut targets: Vec<CaptureTarget> =
                graph.mics().map(|n| CaptureTarget::mic(&n.name)).collect();
            targets.extend(graph.sinks().map(|n| CaptureTarget::monitor(&n.name)));
            Ok(targets)
        }
        CaptureMode::Selected => {
            let mut targets = Vec::with_capacity(2);

            let mic = match &capture.mic {
                MicSelector::Default => graph
                    .default_source
                    .as_deref()
                    .filter(|name| graph.mics().any(|n| n.name == *name))
                    .or_else(|| graph.mics().next().map(|n| n.name.as_str()))
                    .ok_or(SelectionError::NoMicAvailable)?,
                MicSelector::Node(name) => graph
                    .mics()
                    .find(|n| n.name == *name)
                    .map(|n| n.name.as_str())
                    .ok_or_else(|| SelectionError::MicNotFound(name.clone()))?,
            };
            targets.push(CaptureTarget::mic(mic));

            match &capture.output {
                OutputSelector::None => {}
                OutputSelector::Default => {
                    let sink = graph
                        .default_sink
                        .as_deref()
                        .filter(|name| graph.sinks().any(|n| n.name == *name))
                        .or_else(|| graph.sinks().next().map(|n| n.name.as_str()))
                        .ok_or(SelectionError::NoOutputAvailable)?;
                    targets.push(CaptureTarget::monitor(sink));
                }
                OutputSelector::Node(name) => {
                    let sink = graph
                        .sinks()
                        .find(|n| n.name == *name)
                        .map(|n| n.name.as_str())
                        .ok_or_else(|| SelectionError::OutputNotFound(name.clone()))?;
                    targets.push(CaptureTarget::monitor(sink));
                }
            }

            Ok(targets)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, class: &str) -> NodeInfo {
        NodeInfo {
            name: name.to_owned(),
            description: Some(format!("desc of {name}")),
            media_class: class.to_owned(),
        }
    }

    /// Fixture mirroring the real graph of the development machine.
    fn real_graph() -> AudioGraph {
        AudioGraph {
            nodes: vec![
                node(
                    "alsa_input.usb-DJI_Technology_Co.__Ltd._Wireless_Microphone_RX_XSP12345678B-01.analog-stereo",
                    MEDIA_CLASS_SOURCE,
                ),
                node("alsa_input.pci-0000_00_1f.3.analog-stereo", MEDIA_CLASS_SOURCE),
                node(
                    "alsa_input.usb-DisplayLink_ThinkPad_Hybrid_USB-C_with_USB-A_Dock_10634641-02.iec958-stereo",
                    MEDIA_CLASS_SOURCE,
                ),
                node(
                    "alsa_output.usb-DisplayLink_ThinkPad_Hybrid_USB-C_with_USB-A_Dock_10634641-02.analog-stereo",
                    MEDIA_CLASS_SINK,
                ),
                node("alsa_output.pci-0000_03_00.1.hdmi-stereo-extra2", MEDIA_CLASS_SINK),
                node("alsa_output.pci-0000_00_1f.3.iec958-stereo", MEDIA_CLASS_SINK),
                // Non-audio nodes must be ignored.
                node("v4l2_input.pci-some-camera", "Video/Source"),
            ],
            default_source: Some(
                "alsa_input.usb-DJI_Technology_Co.__Ltd._Wireless_Microphone_RX_XSP12345678B-01.analog-stereo"
                    .to_owned(),
            ),
            default_sink: Some(
                "alsa_output.usb-DisplayLink_ThinkPad_Hybrid_USB-C_with_USB-A_Dock_10634641-02.analog-stereo"
                    .to_owned(),
            ),
        }
    }

    #[test]
    fn selected_mode_picks_defaults() {
        let targets = select_targets(&real_graph(), &CaptureConfig::default()).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].kind, StreamKind::Mic);
        assert!(targets[0].node_name.contains("DJI"));
        assert_eq!(targets[1].kind, StreamKind::Monitor);
        assert!(targets[1].node_name.contains("DisplayLink"));
        assert!(targets[1].slug.ends_with(".monitor"));
    }

    #[test]
    fn selected_mode_honours_explicit_nodes() {
        let capture = CaptureConfig {
            mode: CaptureMode::Selected,
            mic: MicSelector::Node("alsa_input.pci-0000_00_1f.3.analog-stereo".into()),
            output: OutputSelector::Node("alsa_output.pci-0000_00_1f.3.iec958-stereo".into()),
        };
        let targets = select_targets(&real_graph(), &capture).unwrap();
        assert_eq!(
            targets[0].node_name,
            "alsa_input.pci-0000_00_1f.3.analog-stereo"
        );
        assert_eq!(
            targets[1].node_name,
            "alsa_output.pci-0000_00_1f.3.iec958-stereo"
        );
    }

    #[test]
    fn output_none_records_mic_only() {
        let capture = CaptureConfig {
            output: OutputSelector::None,
            ..CaptureConfig::default()
        };
        let targets = select_targets(&real_graph(), &capture).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, StreamKind::Mic);
    }

    #[test]
    fn all_mode_selects_every_mic_and_sink() {
        let capture = CaptureConfig {
            mode: CaptureMode::All,
            ..CaptureConfig::default()
        };
        let targets = select_targets(&real_graph(), &capture).unwrap();
        let mics = targets.iter().filter(|t| t.kind == StreamKind::Mic).count();
        let monitors = targets
            .iter()
            .filter(|t| t.kind == StreamKind::Monitor)
            .count();
        assert_eq!((mics, monitors), (3, 3));
        // Slugs must be unique — they define storage directories.
        let mut slugs: Vec<_> = targets.iter().map(|t| t.slug.clone()).collect();
        slugs.sort();
        slugs.dedup();
        assert_eq!(slugs.len(), targets.len());
    }

    #[test]
    fn missing_default_falls_back_to_first_device() {
        let mut graph = real_graph();
        graph.default_source = None;
        graph.default_sink = Some("not-a-real-sink".into());
        let targets = select_targets(&graph, &CaptureConfig::default()).unwrap();
        assert_eq!(targets[0].node_name, graph.nodes[0].name);
        assert!(targets[1].node_name.contains("DisplayLink"));
    }

    #[test]
    fn empty_graph_errors() {
        let graph = AudioGraph::default();
        assert_eq!(
            select_targets(&graph, &CaptureConfig::default()),
            Err(SelectionError::NoMicAvailable)
        );
    }

    #[test]
    fn unknown_explicit_mic_errors() {
        let capture = CaptureConfig {
            mic: MicSelector::Node("nope".into()),
            ..CaptureConfig::default()
        };
        assert_eq!(
            select_targets(&real_graph(), &capture),
            Err(SelectionError::MicNotFound("nope".into()))
        );
    }

    #[test]
    fn sink_name_as_mic_errors() {
        // A sink node name is not a valid mic even though it exists.
        let capture = CaptureConfig {
            mic: MicSelector::Node("alsa_output.pci-0000_00_1f.3.iec958-stereo".into()),
            ..CaptureConfig::default()
        };
        assert!(matches!(
            select_targets(&real_graph(), &capture),
            Err(SelectionError::MicNotFound(_))
        ));
    }

    #[test]
    fn slugify_sanitizes() {
        assert_eq!(
            slugify("alsa_input.pci-0000_00_1f.3.analog-stereo"),
            "alsa_input.pci-0000_00_1f.3.analog-stereo"
        );
        assert_eq!(slugify("weird name/with:chars"), "weird_name_with_chars");
        assert_eq!(slugify("...."), "_");
        assert_eq!(slugify(".hidden."), "hidden");
    }
}
