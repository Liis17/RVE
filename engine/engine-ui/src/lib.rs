//! Игровой UI на GPU и общий egui-мост RVE для инструментов и runtime.

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod game;

#[cfg(feature = "egui")]
pub mod console;
#[cfg(feature = "egui")]
pub mod profiler;
#[cfg(feature = "egui")]
pub mod render;
