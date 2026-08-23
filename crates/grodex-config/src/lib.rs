//! Grodex Config — layered configuration with values/requirements separation.
//!
//! Config follows a two-plane model:
//!   - **Values Plane**: what the user chose (model, UI, MCP servers, etc.)
//!   - **Requirements Plane**: what enterprise IT mandates (cannot be overridden)

pub mod error;
pub mod fs_backend;
pub mod generation;
pub mod layer;
pub mod loader;
pub mod merge;
pub mod migration;
pub mod requirements;
pub mod resolver;
pub mod trust;
pub mod values;
pub mod watcher;

// Re-export key types.
pub use error::ConfigError;
pub use fs_backend::{ConfigPublish, ConfigValidator, FsConfigBackend, FsWatchSource};
pub use generation::{ConfigGeneration, LoadedConfig};
pub use layer::{ConfigLayer, ConfigLayerSource, MergeTrace};
pub use loader::{expand_user_path, ConfigPaths};
pub use migration::{migrate, MigrationResult, CURRENT_SCHEMA_VERSION};
pub use resolver::ConfigResolver;
pub use trust::WorkspaceTrustBinding;
pub use values::EffectiveConfig;
pub use watcher::{
    BreakerState, ConfigDomain, ConfigWatcher, LastKnownGood, PublishBreakerConfig, WatchOutcome,
};
