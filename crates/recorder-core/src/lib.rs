//! Core library for `voice_recorder`: a continuous background audio recorder
//! for Linux.
//!
//! This crate contains everything that is independent of a particular user
//! interface: configuration, device selection, audio capture, segment
//! encoding and storage, retention, time-range export, and the listener
//! fan-out protocol. The `voicerec` binary is a thin CLI/daemon wrapper
//! around this crate.

pub mod capture;
pub mod config;
pub mod device;
pub mod error;

pub use config::Config;
pub use error::{CaptureError, ConfigError};
