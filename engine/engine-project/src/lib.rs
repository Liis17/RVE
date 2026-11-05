//! Project files and startup configuration shared by engine tools and applications.

mod bootstrap;
mod project_settings;

pub use bootstrap::{BootstrapConfig, BootstrapPresentMode};
pub use project_settings::{
    PhysicsSettings, ProjectRequirements, ProjectScene, ProjectSettings, RuntimeSettings,
};
