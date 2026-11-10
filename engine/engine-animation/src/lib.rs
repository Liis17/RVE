//! Скелетная анимация: позы узлов, клипы FBX, смешивание и состояние проигрывания.
//!
//! Крейт не зависит от GPU: скелет приходит из `engine-assets`, а палитры матриц для
//! skinning выдаются renderer'у. [`AnimationBridge`] связывает аниматоры объектов
//! `engine-scene` с этими данными и не читает файлы сам.

mod animator;
mod blend;
mod bridge;
mod clip;
mod pose;
mod skeleton;

pub use animator::{
    Animator, AnimatorError, MAX_TREE_POINTS, Motion, MotionEvent, MotionSource, PlayOptions,
    TreePoint,
};
pub use blend::{blend, line_weights};
pub use bridge::{AnimationBridge, AnimationEvent};
pub use clip::{Clip, Key, Track};
pub use pose::{Pose, Transform};
pub use skeleton::{Joint, Skeleton, SkeletonNode};

/// Версия крейта (SemVer).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
