//! Lightweight Lua and ABI-loaded Rust scripts for scene objects.

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod abi_host;
mod abi_string;
mod actions;
mod component_writes;
mod host;
mod library;
mod lifecycle;
mod lite;
mod math_lua;
mod net;
mod physics;
mod save_file;
mod save_writer;
mod timeline;
mod transaction;
mod types;
mod ui;
mod ui_lua;

mod ui_assets;
mod ui_input;

pub use engine_gui::{document::UiDocument, events::UiEvent, paint::UiImageData, system::UiSystem};
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
pub use save_file::{decode_save_data, encode_save_data};
pub use types::{
    MAX_RUMBLE_SECONDS, RumbleRequest, ScriptCursor, ScriptEvent, ScriptEventLevel,
    ScriptFrameTime, ScriptInput, ScriptParams, ScriptTime, ScriptTransform,
};
pub use ui_assets::{PreparedUi, UiAssetSource, UiRegistry, is_ui_document, load_ui};
pub use ui_input::{UiInputBridge, UiTick};
