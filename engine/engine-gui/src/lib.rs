//! Игровой UI: документы `.rveui`, layout и состояние виджетов без зависимости от renderer и окна.

pub mod document;
pub mod layout;

/// Версия крейта для проверки совместимости документов и ABI вызывающей стороны.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
