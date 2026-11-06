//! Столкновения и физическая симуляция игрового мира.

pub mod broad;
pub mod bvh;
pub mod character;
mod error;
pub mod hull;
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
    ModelGeometry, ModelMesh, PhysicsSceneBinding, PhysicsSceneBody, PhysicsSceneDesc,
    PhysicsSceneError, PhysicsSceneSync, PhysicsSceneSyncError,
};
pub use shape::{Aabb, MassProperties, Pose, Shape};
pub use thread::{
    PhysicsClient, PhysicsClientError, PhysicsInitialization, PhysicsReceive, PhysicsReply,
    PhysicsRequest, PhysicsServer, PhysicsServerError, PhysicsStepResult,
};
pub use world::{
    BodyDesc, BodyHandle, BodyKind, ColliderDesc, PhysicsCommand, PhysicsEvent, PhysicsWorld,
    PhysicsWorldError, StepDriver, StepResult,
};
