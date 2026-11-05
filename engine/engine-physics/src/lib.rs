//! Столкновения и физическая симуляция игрового мира.

pub mod broad;
pub mod bvh;
mod error;
pub mod hull;
pub mod narrow;
pub mod ray;
pub mod shape;

pub use broad::{
    BroadPhaseError, CandidatePair, MAX_BROADPHASE_COMPARISONS, MAX_BROADPHASE_ITEMS,
    MAX_BROADPHASE_PAIRS, sweep_and_prune,
};
pub use bvh::TriMesh;
pub use error::GeometryError;
pub use hull::{Hull, MAX_HULL_INPUT_POINTS, MAX_HULL_ITERATIONS, MAX_HULL_VERTICES, Plane};
pub use narrow::{
    ContactManifold, ContactPoint, DistanceResult, EPA_MAX_FACES, EPA_MAX_ITERATIONS,
    GJK_MAX_ITERATIONS, MAX_MANIFOLD_POINTS, MAX_MESH_CANDIDATES, ManifoldCache, NarrowphaseError,
    PairKey, collide, distance,
};
pub use ray::{Ray, RayHit, raycast};
pub use shape::{Aabb, MassProperties, Pose, Shape};

use std::sync::mpsc::Receiver;

#[derive(Default)]
pub struct PhysicsEngine;

impl PhysicsEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn run(self, stop: Receiver<()>) {
        let _ = stop.recv();
    }
}

#[cfg(test)]
mod tests {
    use super::PhysicsEngine;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn run_returns_after_stop_signal() {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            PhysicsEngine::new().run(stop_rx);
            done_tx.send(()).unwrap();
        });

        stop_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn run_returns_when_stop_sender_is_dropped() {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            PhysicsEngine::new().run(stop_rx);
            done_tx.send(()).unwrap();
        });

        drop(stop_tx);
        done_rx.recv().unwrap();
        worker.join().unwrap();
    }
}
