//! Gamepad input for RVE: a `gilrs` backend, a pure event translator, and a slot mapper that
//! assigns up to four pads to slots 1..=4 and builds processed snapshots.

mod backend;
mod gate;
mod mapper;
mod translate;

pub use backend::GilrsBackend;
pub use gate::PadGate;
pub use mapper::{GamepadMapper, GamepadSnapshot, GamepadTuning, PadSnapshot, SLOT_COUNT};
pub use translate::{MapperEvent, translate};

/// Версия крейта `engine-input`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
