//! Выбирает платформенную интеграцию при сборке для целевой ОС.

#[cfg(target_os = "windows")]
pub use engine_platform_windows::*;

#[cfg(target_os = "macos")]
pub use engine_platform_macos::*;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
compile_error!("RVE поддерживает платформенные адаптеры только для Windows и macOS");
