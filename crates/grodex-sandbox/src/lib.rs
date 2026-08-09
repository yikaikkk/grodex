//! Grodex Sandbox — sandbox runtime.

pub mod manager;
pub mod platform;
pub mod profile;
pub mod profile_layers;
pub mod runtime;
pub mod validator;

#[cfg(target_os = "macos")]
pub mod supervisor;

pub use manager::SandboxManager;
pub use profile::ProfileStore;
pub use profile_layers::{
    AccessLevel, ExtraProfileHints, IntersectionResult, LayeredProfileInput, ProfileLayer,
};
pub use runtime::{PreparedOperation, SandboxRuntimeClient, SandboxRuntimeResponse};
pub use validator::PathValidator;
