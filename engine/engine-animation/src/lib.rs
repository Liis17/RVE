//! Скелетная анимация: позы узлов, клипы FBX, смешивание и состояние проигрывания.
//!
//! Крейт не зависит от GPU и сцены: он получает данные скелета из `engine-assets`
//! и выдаёт палитры матриц для skinning. Соединение с `World` и событиями объектов
//! выполняется выше по стеку.

mod animator;
mod blend;
mod clip;
mod pose;
mod skeleton;

pub use animator::{
    Animator, AnimatorError, MAX_TREE_POINTS, Motion, MotionEvent, MotionSource, PlayOptions,
    TreePoint,
};
pub use blend::{blend, line_weights};
pub use clip::{Clip, Key, Track};
pub use pose::{Pose, Transform};
pub use skeleton::{Joint, Skeleton, SkeletonNode};

/// Версия крейта (SemVer).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
