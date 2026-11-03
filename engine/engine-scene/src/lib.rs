//! Формат сцен `.rvescene`, runtime-граф `World` и математика камеры, трансформаций и света.

pub mod camera;
pub mod camera_settings;
pub mod document;
pub mod light;
pub mod transform;
pub mod world;

pub use camera_settings::{
    AntiAliasing, CameraColorSettings, CameraRenderSettings, CameraSettings, CameraSettingsError,
    ProjectionSettings, Tonemapper,
};
pub use document::{
    CameraComponent, DirectionalLightComponent, HeavyScript, LiteScript, LodGroup, LodLevel,
    MAX_LOD_LEVELS, MaterialOverride, MeshRenderer, ParamValue, SceneAsset, SceneDocument,
    SceneError, SceneObject, SceneTransform,
};
pub use world::{
    LightState, LocalTransform, MeshInstance, ObjectHandle, SceneFrame, SceneWarning, World,
    WorldObject,
};
