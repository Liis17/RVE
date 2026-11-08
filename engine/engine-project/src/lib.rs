//! Project files and startup configuration shared by engine tools and applications.

mod bootstrap;
mod input_settings;
mod prefab_catalog;
mod project_settings;

pub use bootstrap::{BootstrapConfig, BootstrapPresentMode, BootstrapReflexMode};
pub use input_settings::{InputAction, InputActionKind, InputSettings};
pub use prefab_catalog::{
    PREFAB_CATALOG_FILE_NAME, PREFAB_CATALOG_RELEASE_PATH, PrefabCatalogEntry, PrefabCatalogFile,
};
pub use project_settings::{
    PhysicsSettings, ProjectRequirements, ProjectScene, ProjectSettings, RuntimeSettings,
};

/// Версия крейта `engine-project`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
