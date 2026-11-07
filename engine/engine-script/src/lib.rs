//! Lightweight Lua and ABI-loaded Rust scripts for scene objects.

mod abi_host;
mod abi_string;
mod actions;
mod host;
mod library;
mod lite;
mod physics;
mod transaction;
mod types;

pub use engine_scene::{ParamKind, ParamSchema, ParamSpec, ParamValue};
pub use host::{
    CommandError, ScriptHost, ScriptInterruptHandle, ScriptLoadError, ScriptLoadOptions,
    lite_properties,
};
pub use library::{ScriptLibrary, ScriptLibraryError};
pub use rve_sdk::{Key, MouseButton};
pub use types::{
    ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptInput, ScriptParams, ScriptTime,
    ScriptTransform,
};

/// Версия крейта из `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
