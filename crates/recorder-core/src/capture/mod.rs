//! Audio capture: the PipeWire backend, the capture engine, and the frame
//! bus that fans captured audio out to subscribers.

pub mod bus;
pub mod engine;
pub mod pipewire;
