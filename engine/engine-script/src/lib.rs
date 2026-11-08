//! Lightweight Lua and ABI-loaded Rust scripts for scene objects.

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod abi_host;
mod abi_string;
mod actions;
mod host;
mod library;
mod lifecycle;
mod lite;
mod net;
mod physics;
mod transaction;
mod types;

pub use engine_scene::{ParamKind, ParamSchema, ParamSpec, ParamValue};
pub use host::{
    CommandError, ScriptHost, ScriptInterruptHandle, ScriptLoadError, ScriptLoadOptions,
    ScriptUpdateProfile, lite_properties,
};
pub use library::{ScriptLibrary, ScriptLibraryError};
pub use lifecycle::{
    PrefabCatalog, PrefabKind, PrefabRequest, PrefabSpec, PrefabStatus, SpawnedObject, WorldChanges,
};
pub use rve_sdk::{Key, MouseButton};
pub use types::{
    MAX_RUMBLE_SECONDS, RumbleRequest, ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptInput,
    ScriptParams, ScriptTime, ScriptTransform,
};
