//! Project files and startup configuration shared by engine tools and applications.

mod bootstrap;
mod input_settings;
mod project_settings;

pub use bootstrap::{BootstrapConfig, BootstrapPresentMode};
pub use input_settings::{InputAction, InputActionKind, InputSettings};
pub use project_settings::{
    PhysicsSettings, ProjectRequirements, ProjectScene, ProjectSettings, RuntimeSettings,
};
