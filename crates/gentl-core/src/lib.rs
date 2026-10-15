//! GenTL framework layer, protocol-independent. This is the main
//! intellectual property of the project; transports plug into it.
//!
//! GenTL types are re-declared here (mirroring `include/GenTL.h`) instead of
//! generated bindings: the ABI surface is small and stability matters more
//! than convenience.

pub mod channel;
pub mod ds;
pub mod error;
pub mod event_queue;
pub mod handles;
pub mod module;
pub mod pixfmt;
pub mod pixfmt_table;
pub mod types;

pub use error::{last_error, set_error, set_last_error, GentlError, GentlResult};
pub use handles::{free_handle, from_handle, to_handle};
pub use module::{Module, ModuleType};
pub use types::*;
