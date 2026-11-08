//! Игровой UI: документы `.rveui`, layout и состояние виджетов без зависимости от renderer и окна.

pub mod capture;
pub mod document;
pub mod events;
pub mod input;
pub mod layout;
pub mod system;

/// Версия крейта для проверки совместимости документов и ABI вызывающей стороны.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
