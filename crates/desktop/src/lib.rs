//! Native desktop services for Buzz, free of any UI toolkit.
//!
//! The Tauri app and the GPUI port both call these. Each module takes the
//! paths, handles and signer it needs from its caller and reports progress
//! through callbacks, so nothing here depends on an app runtime.

pub mod event;
pub mod media;
pub mod process;
pub mod relay;
#[doc(hidden)]
pub mod test_executable;
#[cfg(test)]
mod test_support;
pub mod upload;

pub use event::{EventTemplate, Signer};
