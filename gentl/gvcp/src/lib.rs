//! GVCP (GigE Vision Control Protocol) wire implementation.
//!
//! Contract: notes/gvcp-wirefacts.md (extracted from the GigE Vision spec).
//! Both the producer's control channel (gentl-gige) and the software
//! camera (gentl-simulator) build on this crate so the two sides can never
//! disagree on the wire format.

pub mod client;
pub mod gvsp;
pub mod registers;
pub mod server;
pub mod wire;

pub use client::GvcpClient;
pub use server::{DeviceHandler, GvcpServer};
pub use wire::*;
