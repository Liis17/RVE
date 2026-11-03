//! Выбирает платформенную интеграцию при сборке для целевой ОС.

mod cache;

pub use cache::{
    load_pipeline_cache, pipeline_cache_path, read_pipeline_cache, safe_cache_app_name,
    save_pipeline_cache, write_pipeline_cache,
};

#[cfg(target_os = "windows")]
pub use engine_platform_windows::*;

#[cfg(target_os = "macos")]
pub use engine_platform_macos::*;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
compile_error!("RVE поддерживает платформенные адаптеры только для Windows и macOS");
