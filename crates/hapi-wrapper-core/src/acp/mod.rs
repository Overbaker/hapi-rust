//! Generic ACP (Agent Client Protocol) layer, reusable by any ACP agent
//! (TS `cli/src/agent/backends/acp`). agy is the first user.
pub mod backend;
pub mod filters;
pub mod generation_clock;
pub mod message_handler;
pub mod permission;
pub mod tool_lifetimes;
pub mod tool_names;
pub mod transport;
pub mod types;
pub use types::*;
