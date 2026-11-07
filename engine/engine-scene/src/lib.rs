//! Формат сцен `.rvescene`, runtime-граф `World` и математика камеры, трансформаций и света.

pub mod audio;
pub mod camera;
pub mod camera_settings;
pub mod document;
pub mod light;
pub mod params;
pub mod prefab;
pub mod transform;
pub mod world;

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use audio::{
    AudioGroup, AudioRequest, AudioSourceComponent, ClipParams, MAX_AUDIO_VOLUME, VoiceId,
};
pub use light::ShadowSettings;

pub use camera_settings::{
    AntiAliasing, CameraColorSettings, CameraRenderSettings, CameraSettings, CameraSettingsError,
    ProjectionSettings, Tonemapper,
};
pub use document::{
    CameraComponent, CharacterControllerComponent, ColliderComponent, ColliderShape,
    DirectionalLightComponent, EnvironmentComponent, HeavyScript, LiteScript, LodGroup, LodLevel,
    MAX_LOD_LEVELS, MaterialAlphaMode, MaterialOverride, MeshRenderer, ParamValue,
    PointLightComponent, ProceduralSkyComponent, RigidBodyComponent, RigidBodyKind, SceneAsset,
    SceneDocument, SceneError, SceneObject, SceneTransform, SpotLightComponent,
};
pub use params::{ParamKind, ParamSchema, ParamSpec};
pub use prefab::{
    Expansion, HeavyParamsOverride, LiteParamsOverride, MeshRendererOverride, ObjectOrigin,
    PrefabInstance, PrefabOverride, PrefabTemplate, TransformOverride, apply_override, expand,
    expand_without_overrides,
};
pub use world::{
    EnvironmentState, LightState, LocalLightState, LocalTransform, MAX_GENERATION,
    MAX_LIVE_OBJECTS, MeshInstance, ObjectHandle, SceneFrame, SceneWarning, SpawnOptions, World,
    WorldObject,
};
