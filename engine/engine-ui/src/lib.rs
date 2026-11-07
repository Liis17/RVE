//! Shared egui components and renderer bridge for RVE tools and runtime.

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod console;
pub mod profiler;
pub mod render;
