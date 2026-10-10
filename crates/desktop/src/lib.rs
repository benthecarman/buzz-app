//! Native desktop services for Buzz, free of any UI toolkit.
//!
//! The Tauri app and the GPUI port both call these. Each module takes the
//! paths, handles and signer it needs from its caller and reports progress
//! through callbacks, so nothing here depends on an app runtime.

pub mod agent_models;
pub mod archive;
pub mod browser;
pub mod event;
pub mod goose_models;
pub mod media;
pub mod oauth_callback;
pub mod pi_models;
pub mod process;
pub mod relay;
pub mod terminal;
#[doc(hidden)]
pub mod test_executable;
#[cfg(test)]
mod test_support;
pub mod upload;
pub mod window_state;

pub use event::{EventTemplate, Signer};
