//! Live audio fan-out: stream the capturing audio to external listeners (a
//! wake-word detector, a transcriber, …) over a Unix socket, without ever
//! blocking or corrupting recording. See [`protocol`] for the wire format and
//! [`server`] for the endpoint.

pub mod protocol;
pub mod server;

pub use server::serve;
