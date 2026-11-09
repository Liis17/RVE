//! ABI-тесты компонентных callbacks: вызовы идут через таблицу `RveHost` той же транзакцией, что и
//! скрипт, поэтому проверяется состояние, которое увидит следующий вызов до и после commit.

use super::*;
use crate::actions::ActionFrame;
use crate::lifecycle::Lifecycle;
use crate::physics::PhysicsScriptState;
use crate::types::{ScriptCursor, ScriptInput};
use engine_scene::{SceneDocument, World};
use rve_sdk::abi::{RveEnvironmentState, RveProceduralSky};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

const SKY_SCENE: &str = r#"
[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "sky"
[objects.environment.procedural]
zenith_color = [0.1, 0.2, 0.8]
horizon_color = [0.6, 0.6, 0.6]
ground_color = [0.1, 0.1, 0.1]
sun_size_deg = 1.0
sun_intensity = 3.0
"#;

fn world(source: &str) -> World {
    World::from_document(&SceneDocument::parse("scene.rvescene", source).unwrap()).unwrap()
}

fn writable_transaction(world: &World) -> ScriptTransaction {
    let mut transaction = ScriptTransaction::default();
    transaction.begin(ScriptTransactionInit {
        world,
        self_object: Some(world.active_camera()),
        source: "scene/camera/Probe",
        writable: true,
        actions: Arc::new(ActionFrame::empty(ScriptInput::default())),
        queued_messages: (0, 0),
        queued_audio: 0,
        queued_animation: 0,
        queued_rumble: 0,
        net: Default::default(),
        queued_net: 0,
        physics: Rc::new(RefCell::new(PhysicsScriptState::default())),
        physics_phase_dt: None,
        lifecycle: Rc::new(RefCell::new(Lifecycle::default())),
        lifecycle_writes: true,
        instance_serial: 0,
        instance_enabled: true,
        cursor: ScriptCursor::default(),
    });
    transaction
}

#[test]
fn environment_set_with_an_invalid_sky_stages_nothing_even_with_a_valid_intensity() {
    let world = world(SKY_SCENE);
    let sky = world.handle("sky").unwrap().to_abi().unwrap();
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let get = host.environment_get.unwrap();
    let set = host.environment_set.unwrap();
    // SAFETY: state pointers are valid for each synchronous callback.
    unsafe {
        let mut before = RveEnvironmentState::default();
        assert!(get(host.ctx, sky, &mut before));
        let invalid = RveEnvironmentState {
            intensity: 2.0,
            has_sky: true,
            sky: RveProceduralSky {
                sun_size_deg: 0.0,
                ..before.sky
            },
        };
        assert!(!set(host.ctx, sky, &invalid));
        let mut after = RveEnvironmentState::default();
        assert!(get(host.ctx, sky, &mut after));
        assert_eq!(after, before, "отказ не оставляет частичной записи");
    }
}

#[test]
fn environment_set_with_a_valid_sky_is_visible_before_commit() {
    let world = world(SKY_SCENE);
    let sky = world.handle("sky").unwrap().to_abi().unwrap();
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let get = host.environment_get.unwrap();
    let set = host.environment_set.unwrap();
    // SAFETY: state pointers are valid for each synchronous callback.
    unsafe {
        let mut before = RveEnvironmentState::default();
        assert!(get(host.ctx, sky, &mut before));
        let staged = RveEnvironmentState {
            intensity: 2.5,
            has_sky: true,
            sky: RveProceduralSky {
                sun_intensity: 4.0,
                ..before.sky
            },
        };
        assert!(set(host.ctx, sky, &staged));
        let mut after = RveEnvironmentState::default();
        assert!(get(host.ctx, sky, &mut after));
        assert_eq!(after.intensity, 2.5);
        assert_eq!(after.sky.sun_intensity, 4.0);
    }
}
