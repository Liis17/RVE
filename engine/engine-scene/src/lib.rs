//! Формат сцен `.rvescene`, runtime-граф `World` и математика камеры, трансформаций и света.

pub mod camera;
pub mod camera_settings;
pub mod document;
pub mod light;
pub mod params;
pub mod prefab;
pub mod transform;
pub mod world;

pub use camera_settings::{
    AntiAliasing, CameraColorSettings, CameraRenderSettings, CameraSettings, CameraSettingsError,
    ProjectionSettings, Tonemapper,
};
pub use document::{
    CameraComponent, DirectionalLightComponent, HeavyScript, LiteScript, LodGroup, LodLevel,
    MAX_LOD_LEVELS, MaterialAlphaMode, MaterialOverride, MeshRenderer, ParamValue, SceneAsset,
    SceneDocument, SceneError, SceneObject, SceneTransform,
};
pub use params::{ParamKind, ParamSchema, ParamSpec};
pub use prefab::{
    Expansion, HeavyParamsOverride, LiteParamsOverride, MeshRendererOverride, ObjectOrigin,
    PrefabInstance, PrefabOverride, TransformOverride, apply_override, expand,
    expand_without_overrides,
};
pub use world::{
    LightState, LocalTransform, MeshInstance, ObjectHandle, SceneFrame, SceneWarning, World,
    WorldObject,
};
