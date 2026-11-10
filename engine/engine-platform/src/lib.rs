//! Выбирает платформенную интеграцию при сборке для целевой ОС.

/// Platform crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod cache;

pub use cache::{
    load_pipeline_cache, pipeline_cache_path, read_pipeline_cache, safe_cache_app_name,
    save_data_path, save_pipeline_cache, screenshots_dir, write_pipeline_cache,
    write_user_data_atomically,
};

#[cfg(target_os = "windows")]
pub use engine_platform_windows::*;

#[cfg(target_os = "macos")]
pub use engine_platform_macos::*;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
compile_error!("RVE поддерживает платформенные адаптеры только для Windows и macOS");
