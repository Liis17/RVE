//! Формат сцен `.rvescene`, runtime-граф `World` и математика камеры, трансформаций и света.

pub mod camera;
pub mod document;
pub mod light;
pub mod transform;
pub mod world;

pub use document::{
    CameraComponent, DirectionalLightComponent, HeavyScript, LiteScript, MeshRenderer, ParamValue,
    SceneAsset, SceneDocument, SceneError, SceneObject, SceneTransform,
};
pub use world::{
    LightState, LocalTransform, MeshInstance, ObjectHandle, SceneFrame, SceneWarning, World,
    WorldObject,
};
