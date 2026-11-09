//! Столкновения и физическая симуляция игрового мира.

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod broad;
pub mod bvh;
pub mod character;
mod error;
pub mod hull;
mod material;
pub mod narrow;
pub mod query;
pub mod ray;
pub mod scene;
pub mod shape;
mod solver;
pub mod thread;
pub mod world;

pub use broad::{
    BroadPhaseError, CandidatePair, MAX_BROADPHASE_COMPARISONS, MAX_BROADPHASE_ITEMS,
    MAX_BROADPHASE_PAIRS, sweep_and_prune,
};
pub use bvh::TriMesh;
pub use character::{
    CHARACTER_MOVE_ITERATIONS, CharacterController, CharacterError, CharacterHit,
    CharacterMoveResult, MAX_PUSH_SPEED,
};
pub use error::GeometryError;
pub use hull::{Hull, MAX_HULL_INPUT_POINTS, MAX_HULL_ITERATIONS, MAX_HULL_VERTICES, Plane};
pub use material::{CombineMode, CombinedMaterial, PhysicsMaterial, combine_materials};
pub use narrow::{
    ContactManifold, ContactPoint, DistanceResult, EPA_MAX_FACES, EPA_MAX_ITERATIONS,
    GJK_MAX_ITERATIONS, MAX_MANIFOLD_POINTS, MAX_MESH_CANDIDATES, ManifoldCache, NarrowphaseError,
    PairKey, collide, distance,
};
pub use query::{
    MAX_QUERY_CANDIDATES, MAX_SHAPE_CAST_ITERATIONS, QueryBodyState, QueryError, QueryFilter,
    QueryHit, QueryOverrides, QuerySnapshot,
};
pub use ray::{Ray, RayHit, raycast};
pub use scene::{
    ModelGeometry, ModelMesh, ObjectWorldTransform, PhysicsSceneBinding, PhysicsSceneBody,
    PhysicsSceneDesc, PhysicsSceneError, PhysicsSceneSync, PhysicsSceneSyncError, ShapeCache,
    character_capsule_shape, character_controller_scaled, object_world_transform,
    primitive_collider_shape,
};
pub use shape::{Aabb, MassProperties, Pose, Shape};
pub use thread::{
    PhysicsClient, PhysicsClientError, PhysicsInitialization, PhysicsReceive, PhysicsReply,
    PhysicsRequest, PhysicsServer, PhysicsServerError, PhysicsStepResult,
};
pub use world::{
    BodyChange, BodyDesc, BodyHandle, BodyKind, ColliderDesc, PHYSICS_LAYER_COUNT, PhysicsCommand,
    PhysicsEvent, PhysicsWorld, PhysicsWorldError, PrimitiveShape, StepDriver, StepResult,
};
