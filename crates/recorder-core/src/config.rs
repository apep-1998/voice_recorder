//! Configuration: schema, defaults, loading, and validation.
//!
//! The configuration lives at `~/.config/voice_recorder/config.toml` (XDG
//! config dir). [`Config::init_default_file`] writes a commented default
//! template there on first run, and [`Config::load`] / [`Config::load_default`]
//! read and validate it.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

/// Default config template written by `voicerec config init`. Kept as a
/// commented literal (rather than serializing `Config::default()`) so the
/// installed file documents itself; a unit test guarantees it stays in sync
/// with the actual defaults.
pub const DEFAULT_CONFIG_TOML: &str = r#"# voice_recorder configuration
# See https://github.com/apep-1998/voice_recorder for documentation.

[storage]
# Where recorded segments and the index are stored.
root = "~/.local/share/voice_recorder"
# How long to keep recordings. Older segments are deleted automatically.
# Examples: "12h", "5d", "2weeks".
retention = "5d"
# Length of each recorded segment file. Shorter segments lose less audio on
# power failure but create more files.
segment_duration = "60s"
# Optional hard cap on total disk usage; oldest segments are evicted first.
# Accepts "30GB", "500MB", or a plain number of bytes.
# max_disk_bytes = "30GB"

[capture]
# "selected": record the devices chosen below.
# "all": record every microphone and every output monitor to separate files.
mode = "selected"
# Microphone to record: "default" follows the system default microphone,
# or give an exact PipeWire node name (see `voicerec devices`).
mic = "default"
# Output device whose playback is recorded (via its monitor): "default"
# follows the system default output, "none" disables output recording,
# or give an exact PipeWire sink node name.
output = "default"

[encoding]
# Opus bitrates in bits/second and channel counts per stream kind.
mic_bitrate = 32000
monitor_bitrate = 64000
mic_channels = 1
monitor_channels = 2

[fanout]
# Live audio fan-out socket for external listeners (e.g. wake-word daemons).
enabled = false
socket_path = "$XDG_RUNTIME_DIR/voice_recorder/audio.sock"

[power]
# Listen for logind suspend/resume signals to finalize segments cleanly.
logind_integration = true
# Wall-clock vs. sample-clock drift (ms) that forces a new segment; this
# catches suspends and clock steps even without logind.
clock_drift_threshold_ms = 250
"#;

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub storage: StorageConfig,
    pub capture: CaptureConfig,
    pub encoding: EncodingConfig,
    pub fanout: FanoutConfig,
    pub power: PowerConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Root directory for segments and the index. `~` and `$VARS` are
    /// expanded by [`StorageConfig::root_path`].
    pub root: String,
    /// How long recordings are kept.
    #[serde(with = "humantime_serde")]
    pub retention: Duration,
    /// Length of one segment file.
    #[serde(with = "humantime_serde")]
    pub segment_duration: Duration,
    /// Optional hard cap on total stored bytes (oldest evicted first).
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_opt_bytes",
        serialize_with = "serialize_opt_bytes"
    )]
    pub max_disk_bytes: Option<u64>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            root: "~/.local/share/voice_recorder".to_owned(),
            retention: Duration::from_secs(5 * 24 * 60 * 60),
            segment_duration: Duration::from_secs(60),
            max_disk_bytes: None,
        }
    }
}

impl StorageConfig {
    /// The storage root with `~` and environment variables expanded.
    pub fn root_path(&self) -> PathBuf {
        expand_path(&self.root)
    }
}

/// Which devices to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Record the configured `mic` and `output` devices.
    #[default]
    Selected,
    /// Record every microphone and every output monitor, each to its own
    /// stream of segment files.
    All,
}

/// Microphone selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MicSelector {
    /// Follow the system default source.
    #[default]
    Default,
    /// An exact `PipeWire` node name.
    Node(String),
}

/// Output-device selection (recorded via the sink's monitor source).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OutputSelector {
    /// Follow the system default sink.
    #[default]
    Default,
    /// Do not record system output.
    None,
    /// An exact `PipeWire` sink node name.
    Node(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub mode: CaptureMode,
    pub mic: MicSelector,
    pub output: OutputSelector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EncodingConfig {
    pub mic_bitrate: u32,
    pub monitor_bitrate: u32,
    pub mic_channels: u8,
    pub monitor_channels: u8,
}

impl Default for EncodingConfig {
    fn default() -> Self {
        Self {
            mic_bitrate: 32_000,
            monitor_bitrate: 64_000,
            mic_channels: 1,
            monitor_channels: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FanoutConfig {
    pub enabled: bool,
    pub socket_path: String,
}

impl Default for FanoutConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket_path: "$XDG_RUNTIME_DIR/voice_recorder/audio.sock".to_owned(),
        }
    }
}

impl FanoutConfig {
    /// The socket path with `~` and environment variables expanded.
    pub fn socket_path(&self) -> PathBuf {
        expand_path(&self.socket_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PowerConfig {
    pub logind_integration: bool,
    pub clock_drift_threshold_ms: u64,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            logind_integration: true,
            clock_drift_threshold_ms: 250,
        }
    }
}

impl Config {
    /// Default location: `~/.config/voice_recorder/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let dirs = directories::BaseDirs::new().ok_or(ConfigError::NoConfigDir)?;
        Ok(dirs.config_dir().join("voice_recorder").join("config.toml"))
    }

    /// Load and validate the config from an explicit path.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Load from the default path, falling back to built-in defaults if the
    /// file does not exist yet.
    pub fn load_default() -> Result<Self, ConfigError> {
        let path = Self::default_path()?;
        if path.exists() {
            Self::load(&path)
        } else {
            Ok(Self::default())
        }
    }

    /// Write the commented default config template to the default path.
    /// Fails if the file already exists.
    pub fn init_default_file() -> Result<PathBuf, ConfigError> {
        let path = Self::default_path()?;
        if path.exists() {
            return Err(ConfigError::AlreadyExists { path });
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.clone(),
                source,
            })?;
        }
        std::fs::write(&path, DEFAULT_CONFIG_TOML).map_err(|source| ConfigError::Write {
            path: path.clone(),
            source,
        })?;
        Ok(path)
    }

    /// Check invariants that the type system cannot express.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let seg = self.storage.segment_duration;
        if seg < Duration::from_secs(5) || seg > Duration::from_secs(3600) {
            return Err(ConfigError::Invalid(format!(
                "storage.segment_duration must be between 5s and 1h, got {}",
                humantime::format_duration(seg)
            )));
        }
        if self.storage.retention < seg {
            return Err(ConfigError::Invalid(format!(
                "storage.retention ({}) must be at least one segment_duration ({})",
                humantime::format_duration(self.storage.retention),
                humantime::format_duration(seg)
            )));
        }
        if self.storage.root.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "storage.root must not be empty".into(),
            ));
        }
        for (name, bitrate) in [
            ("encoding.mic_bitrate", self.encoding.mic_bitrate),
            ("encoding.monitor_bitrate", self.encoding.monitor_bitrate),
        ] {
            // libopus accepts 500 bit/s .. 512 kbit/s.
            if !(500..=512_000).contains(&bitrate) {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be between 500 and 512000, got {bitrate}"
                )));
            }
        }
        for (name, channels) in [
            ("encoding.mic_channels", self.encoding.mic_channels),
            ("encoding.monitor_channels", self.encoding.monitor_channels),
        ] {
            if !(1..=2).contains(&channels) {
                return Err(ConfigError::Invalid(format!(
                    "{name} must be 1 or 2, got {channels}"
                )));
            }
        }
        if self.fanout.enabled && self.fanout.socket_path.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "fanout.socket_path must not be empty when fanout is enabled".into(),
            ));
        }
        Ok(())
    }
}

/// Expand a leading `~` and `$VAR` / `${VAR}` references in a path. Unknown
/// variables expand to the empty string.
pub fn expand_path(input: &str) -> PathBuf {
    let mut s = input.to_owned();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            s = format!("{}/{rest}", home.to_string_lossy());
        }
    } else if s == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            s = home.to_string_lossy().into_owned();
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let braced = chars.peek() == Some(&'{');
        if braced {
            chars.next();
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' {
                name.push(n);
                chars.next();
            } else {
                break;
            }
        }
        if braced && chars.peek() == Some(&'}') {
            chars.next();
        }
        if name.is_empty() {
            out.push('$');
        } else if let Ok(value) = std::env::var(&name) {
            out.push_str(&value);
        }
    }
    PathBuf::from(out)
}

// --- serde plumbing -------------------------------------------------------

impl Serialize for MicSelector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => serializer.serialize_str("default"),
            Self::Node(name) => serializer.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for MicSelector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "default" => Self::Default,
            _ => Self::Node(s),
        })
    }
}

impl fmt::Display for MicSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Node(name) => f.write_str(name),
        }
    }
}

impl Serialize for OutputSelector {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => serializer.serialize_str("default"),
            Self::None => serializer.serialize_str("none"),
            Self::Node(name) => serializer.serialize_str(name),
        }
    }
}

impl<'de> Deserialize<'de> for OutputSelector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "default" => Self::Default,
            "none" => Self::None,
            _ => Self::Node(s),
        })
    }
}

impl fmt::Display for OutputSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::None => f.write_str("none"),
            Self::Node(name) => f.write_str(name),
        }
    }
}

/// Parse a human byte size: `"30GB"`, `"512 MiB"`, `"1000000"`, or a bare
/// TOML integer. Decimal (kB/MB/GB/TB) and binary (KiB/MiB/GiB/TiB) units.
pub fn parse_byte_size(input: &str) -> Result<u64, String> {
    let s = input.trim();
    let digits_end = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid byte size: {input:?}"))?;
    let multiplier: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" | "k" => 1_000,
        "mb" | "m" => 1_000_000,
        "gb" | "g" => 1_000_000_000,
        "tb" | "t" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        other => return Err(format!("unknown byte-size unit {other:?} in {input:?}")),
    };
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let bytes = (value * multiplier as f64) as u64;
    Ok(bytes)
}

fn deserialize_opt_bytes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u64),
        Text(String),
    }
    match Option::<Raw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Raw::Number(n)) => Ok(Some(n)),
        Some(Raw::Text(s)) => parse_byte_size(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

#[allow(clippy::ref_option)]
fn serialize_opt_bytes<S: Serializer>(
    value: &Option<u64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(n) => serializer.serialize_u64(*n),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_matches_default_config() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("template parses");
        assert_eq!(parsed, Config::default());
        parsed.validate().expect("defaults are valid");
    }

    #[test]
    fn empty_file_gives_defaults() {
        let parsed: Config = toml::from_str("").expect("empty config parses");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [storage]
            root = "/data/rec"
            retention = "12h"
            segment_duration = "30s"
            max_disk_bytes = "30GB"

            [capture]
            mode = "all"
            mic = "alsa_input.usb-DJI.analog-stereo"
            output = "none"

            [encoding]
            mic_bitrate = 48000
            monitor_bitrate = 96000
            mic_channels = 2
            monitor_channels = 1

            [fanout]
            enabled = true
            socket_path = "/run/user/1000/vr.sock"

            [power]
            logind_integration = false
            clock_drift_threshold_ms = 500
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.storage.root, "/data/rec");
        assert_eq!(c.storage.retention, Duration::from_secs(12 * 3600));
        assert_eq!(c.storage.segment_duration, Duration::from_secs(30));
        assert_eq!(c.storage.max_disk_bytes, Some(30_000_000_000));
        assert_eq!(c.capture.mode, CaptureMode::All);
        assert_eq!(
            c.capture.mic,
            MicSelector::Node("alsa_input.usb-DJI.analog-stereo".into())
        );
        assert_eq!(c.capture.output, OutputSelector::None);
        assert_eq!(c.encoding.mic_bitrate, 48_000);
        assert!(c.fanout.enabled);
        assert!(!c.power.logind_integration);
        assert_eq!(c.power.clock_drift_threshold_ms, 500);
        c.validate().unwrap();
    }

    #[test]
    fn max_disk_bytes_accepts_integer() {
        let c: Config =
            toml::from_str("[storage]\nmax_disk_bytes = 123456\n").expect("integer bytes");
        assert_eq!(c.storage.max_disk_bytes, Some(123_456));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("[storage]\nretenshun = \"5d\"\n").unwrap_err();
        assert!(err.to_string().contains("retenshun"), "{err}");
    }

    #[test]
    fn byte_sizes_parse() {
        assert_eq!(parse_byte_size("0"), Ok(0));
        assert_eq!(parse_byte_size("123"), Ok(123));
        assert_eq!(parse_byte_size("1kb"), Ok(1000));
        assert_eq!(parse_byte_size("30GB"), Ok(30_000_000_000));
        assert_eq!(parse_byte_size("1.5 GB"), Ok(1_500_000_000));
        assert_eq!(parse_byte_size("2GiB"), Ok(2 << 30));
        assert!(parse_byte_size("ten").is_err());
        assert!(parse_byte_size("10xb").is_err());
    }

    #[test]
    fn validation_rejects_bad_values() {
        let mut c = Config::default();
        c.storage.segment_duration = Duration::from_secs(1);
        assert!(matches!(c.validate(), Err(ConfigError::Invalid(_))));

        let mut c = Config::default();
        c.storage.retention = Duration::from_secs(10);
        assert!(matches!(c.validate(), Err(ConfigError::Invalid(_))));

        let mut c = Config::default();
        c.encoding.mic_bitrate = 100;
        assert!(matches!(c.validate(), Err(ConfigError::Invalid(_))));

        let mut c = Config::default();
        c.encoding.monitor_channels = 6;
        assert!(matches!(c.validate(), Err(ConfigError::Invalid(_))));

        let mut c = Config::default();
        c.fanout.enabled = true;
        c.fanout.socket_path = "  ".into();
        assert!(matches!(c.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn load_round_trips_through_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, DEFAULT_CONFIG_TOML).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded, Config::default());
    }

    #[test]
    fn load_reports_parse_errors_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[storage\n").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn expand_path_handles_home_and_vars() {
        // These tests rely on HOME being set, which is true in any normal
        // environment including CI.
        let home = std::env::var("HOME").expect("HOME set");
        assert_eq!(expand_path("~/x"), PathBuf::from(format!("{home}/x")));
        assert_eq!(expand_path("~"), PathBuf::from(home.clone()));
        assert_eq!(
            expand_path("$HOME/data"),
            PathBuf::from(format!("{home}/data"))
        );
        assert_eq!(
            expand_path("${HOME}/data"),
            PathBuf::from(format!("{home}/data"))
        );
        assert_eq!(expand_path("/plain/path"), PathBuf::from("/plain/path"));
        assert_eq!(
            expand_path("/x/$VOICEREC_UNSET_VAR_12345/y"),
            PathBuf::from("/x//y")
        );
    }

    #[test]
    fn selectors_round_trip() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            mic: MicSelector,
            out: OutputSelector,
        }
        let w: Wrap = toml::from_str("mic = \"default\"\nout = \"none\"\n").unwrap();
        assert_eq!(w.mic, MicSelector::Default);
        assert_eq!(w.out, OutputSelector::None);
        let s = toml::to_string(&Wrap {
            mic: MicSelector::Node("n1".into()),
            out: OutputSelector::Default,
        })
        .unwrap();
        assert!(s.contains("mic = \"n1\""));
        assert!(s.contains("out = \"default\""));
    }
}
