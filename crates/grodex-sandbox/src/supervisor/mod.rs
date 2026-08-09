//! macOS-only external Sandbox Supervisor submodules.

#[cfg(target_os = "macos")]
pub mod protocol;
#[cfg(target_os = "macos")]
pub mod child;
#[cfg(target_os = "macos")]
pub mod client;
