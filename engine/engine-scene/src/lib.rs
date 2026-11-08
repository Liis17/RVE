//! Формат сцен `.rvescene`, runtime-граф `World` и математика камеры, трансформаций и света.

pub mod animation;
pub mod audio;
pub mod camera;
pub mod camera_settings;
pub mod document;
pub mod light;
pub mod network;
pub mod params;
pub mod particles;
pub mod prefab;
pub mod transform;
pub mod world;

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use animation::{
    AnimationRequest, AnimatorComponent, BlendTree, BlendTreePoint, MAX_BLEND_TREE_POINTS,
};
pub use audio::{
    AudioGroup, AudioRequest, AudioSourceComponent, ClipParams, MAX_AUDIO_VOLUME, VoiceId,
};
pub use light::ShadowSettings;
pub use network::{
    MAX_INTERPOLATION_DELAY_MS, MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_STRING_BYTES,
    MAX_SEND_RATE_HZ, NetAuthority, NetEvent, NetId, NetMode, NetRequest, NetTarget,
    NetworkIdentityComponent, NetworkTransformComponent, PeerId, SERVER_PEER_ID, scene_net_id,
};
pub use particles::{
    MAX_ADDITIVE_PARTICLE_CAPACITY, MAX_ALPHA_PARTICLE_CAPACITY, ParticleBlendMode,
    ParticleBurstComponent, ParticleCollisionComponent, ParticleColorKeyComponent,
    ParticleEmitterComponent, ParticleFlipbookComponent, ParticleShapeComponent,
    ParticleSizeKeyComponent,
};

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
    PrefabInstance, PrefabOverride, PrefabTemplate, TransformOverride, apply_override, expand,
    expand_without_overrides,
};
pub use world::{
    EnvironmentState, LightState, LocalLightState, LocalTransform, MAX_GENERATION,
    MAX_LIVE_OBJECTS, MeshInstance, ObjectHandle, ParticleEmitterInstance, SceneFrame,
    SceneWarning, SpawnOptions, World, WorldObject,
};
