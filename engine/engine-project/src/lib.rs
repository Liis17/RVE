//! Project files and startup configuration shared by engine tools and applications.

mod bootstrap;
mod project_settings;

pub use bootstrap::BootstrapConfig;
pub use project_settings::{ProjectRequirements, ProjectScene, ProjectSettings, RuntimeSettings};
