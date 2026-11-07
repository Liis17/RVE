//! Формат сцен `.rvescene`, runtime-граф `World` и математика камеры, трансформаций и света.

pub mod camera;
pub mod camera_settings;
pub mod document;
pub mod light;
pub mod params;
pub mod prefab;
pub mod transform;
pub mod world;

/// Версия крейта из `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use light::ShadowSettings;

pub use camera_settings::{
    AntiAliasing, CameraColorSettings, CameraRenderSettings, CameraSettings, CameraSettingsError,
    ProjectionSettings, Tonemapper,
};
pub use document::{
    CameraComponent, CameraCursor, CharacterControllerComponent, ColliderComponent, ColliderShape,
    DirectionalLightComponent, EnvironmentComponent, HeavyScript, LiteScript, LodGroup, LodLevel,
    MAX_LOD_LEVELS, MaterialAlphaMode, MaterialOverride, MeshRenderer, ParamValue,
    PointLightComponent, ProceduralSkyComponent, RigidBodyComponent, RigidBodyKind, SceneAsset,
    SceneDocument, SceneError, SceneObject, SceneTransform, SpotLightComponent,
};
pub use params::{ParamKind, ParamSchema, ParamSpec};
pub use prefab::{
    Expansion, HeavyParamsOverride, LiteParamsOverride, MeshRendererOverride, ObjectOrigin,
    PrefabInstance, PrefabOverride, TransformOverride, apply_override, expand,
    expand_without_overrides,
};
pub use world::{
    EnvironmentState, LightState, LocalLightState, LocalTransform, MeshInstance, ObjectHandle,
    SceneFrame, SceneWarning, World, WorldObject,
};
