//! Single-room RLBREG2 registry daemon.

mod persist;
mod protocol;
mod server;

pub use server::{RegistryService, ServerConfig};
