//! ABI-тесты компонентных callbacks: вызовы идут через таблицу `RveHost` той же транзакцией, что и
//! скрипт, поэтому проверяется состояние, которое увидит следующий вызов до и после commit.

use super::*;
use crate::actions::ActionFrame;
use crate::lifecycle::Lifecycle;
use crate::physics::PhysicsScriptState;
use crate::types::{ScriptCursor, ScriptInput};
use engine_scene::{SceneDocument, World};
use rve_sdk::abi::{
    RVE_AUDIO_GROUP_EFFECTS, RVE_LIGHT_POINT, RVE_LIGHT_SPOT, RVE_MAX_PARTICLE_BURST,
    RVE_PARTICLE_BURST, RVE_PARTICLE_RESTART, RveAudioPlayExParams, RveAudioSourceState,
    RveEnvironmentState, RveLightState, RveMeshMaterialParams, RveProceduralSky, RveStatus, RveStr,
};
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

const MATERIAL_SCENE: &str = r#"
[[assets]]
path = "/models/crate.fbx"
[[assets]]
path = "/materials/gold.rvemat"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "crate"
[objects.mesh_renderer]
model = "/models/crate.fbx"
"#;

#[test]
fn mesh_set_material_path_accepts_a_declared_rvemat_and_reports_refusals() {
    let world = world(MATERIAL_SCENE);
    let crate_abi = world.handle("crate").unwrap().to_abi().unwrap();
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let set = host.mesh_set_material_path.unwrap();
    // SAFETY: the path strings are valid for each synchronous callback.
    unsafe {
        assert!(!set(
            host.ctx,
            crate_abi,
            RveStr::from_static("/materials/silver.rvemat")
        ));
        let message = context.error.clone().unwrap_or_default();
        assert!(message.contains("не объявлен"), "{message}");
        assert!(set(
            host.ctx,
            crate_abi,
            RveStr::from_static("/materials/gold.rvemat")
        ));
        assert!(!set(
            host.ctx,
            0,
            RveStr::from_static("/materials/gold.rvemat")
        ));
    }
}

const COMPONENT_SCENE: &str = r#"
[[assets]]
path = "/models/crate.fbx"
[[assets]]
path = "/audio/hum.wav"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "lamp"
[objects.point_light]
intensity = 2.0
[[objects]]
id = "crate"
[objects.mesh_renderer]
model = "/models/crate.fbx"
[[objects]]
id = "fire"
[objects.particle_emitter]
[[objects]]
id = "tower"
[objects.mesh_renderer]
model = "/models/crate.fbx"
[objects.lod_group]
cull_distance = 50.0
[[objects.lod_group.levels]]
model = "/models/crate.fbx"
distance = 10.0
[[objects.lod_group.levels]]
model = "/models/crate.fbx"
distance = 20.0
[[objects]]
id = "speaker"
[objects.audio_source]
clip = "/audio/hum.wav"
"#;

fn abi_handle(world: &World, id: &str) -> u64 {
    world.handle(id).unwrap().to_abi().unwrap()
}

#[test]
fn light_get_and_set_round_trip_and_refuse_bad_values_without_writing() {
    let world = world(COMPONENT_SCENE);
    let lamp = abi_handle(&world, "lamp");
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let get = host.light_get.unwrap();
    let set = host.light_set.unwrap();
    // SAFETY: state pointers are valid for each synchronous callback.
    unsafe {
        let mut before = RveLightState::default();
        assert!(get(host.ctx, lamp, RVE_LIGHT_POINT, &mut before));
        assert_eq!(before.intensity, 2.0);
        let refused = RveLightState {
            intensity: -1.0,
            ..before
        };
        assert!(!set(host.ctx, lamp, RVE_LIGHT_POINT, &refused));
        assert!(context.error.is_some());
        let mut after = RveLightState::default();
        assert!(get(host.ctx, lamp, RVE_LIGHT_POINT, &mut after));
        assert_eq!(after, before, "отказ не меняет свет");

        let valid = RveLightState {
            intensity: 3.0,
            ..before
        };
        assert!(set(host.ctx, lamp, RVE_LIGHT_POINT, &valid));
        assert!(get(host.ctx, lamp, RVE_LIGHT_POINT, &mut after));
        assert_eq!(after.intensity, 3.0, "запись видна до commit");

        assert!(!get(host.ctx, lamp, RVE_LIGHT_POINT, std::ptr::null_mut()));
        assert!(!set(host.ctx, lamp, RVE_LIGHT_POINT, std::ptr::null()));
        assert!(!get(host.ctx, lamp, RVE_LIGHT_SPOT, &mut after));
    }
}

#[test]
fn mesh_visibility_material_and_particle_state_read_back_pending_writes() {
    let world = world(COMPONENT_SCENE);
    let crate_handle = abi_handle(&world, "crate");
    let camera = abi_handle(&world, "camera");
    let fire = abi_handle(&world, "fire");
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let get_visible = host.mesh_get_visible.unwrap();
    let set_visible = host.mesh_set_visible.unwrap();
    let get_playing = host.particles_get_playing.unwrap();
    let set_playing = host.particles_set_playing.unwrap();
    let command = host.particles_command.unwrap();
    let set_params = host.mesh_set_material_params.unwrap();
    // SAFETY: output and input pointers are valid for each synchronous callback.
    unsafe {
        let mut visible = false;
        assert!(get_visible(host.ctx, crate_handle, &mut visible));
        assert!(visible);
        assert!(!get_visible(host.ctx, camera, &mut visible));
        assert!(!get_visible(host.ctx, crate_handle, std::ptr::null_mut()));
        assert!(set_visible(host.ctx, crate_handle, false));
        assert!(get_visible(host.ctx, crate_handle, &mut visible));
        assert!(!visible, "скрытие видно до commit");
        assert!(!set_visible(host.ctx, camera, false));

        let mut playing = false;
        assert!(get_playing(host.ctx, fire, &mut playing));
        assert!(playing);
        assert!(set_playing(host.ctx, fire, false));
        assert!(get_playing(host.ctx, fire, &mut playing));
        assert!(!playing);
        assert!(!set_playing(host.ctx, camera, true));
        assert!(!get_playing(host.ctx, fire, std::ptr::null_mut()));

        assert!(!command(host.ctx, fire, RVE_PARTICLE_BURST, 0));
        assert!(!command(
            host.ctx,
            fire,
            RVE_PARTICLE_BURST,
            RVE_MAX_PARTICLE_BURST + 1
        ));
        assert!(!command(host.ctx, fire, 9, 1));
        assert!(command(host.ctx, fire, RVE_PARTICLE_RESTART, 0));
        assert!(command(host.ctx, fire, RVE_PARTICLE_BURST, 4));

        let params = RveMeshMaterialParams {
            has_roughness: true,
            roughness: 1.5,
            ..Default::default()
        };
        assert!(!set_params(host.ctx, crate_handle, &params));
        assert!(!set_params(host.ctx, crate_handle, std::ptr::null()));
    }
}

#[test]
fn lod_get_reports_every_level_and_writes_only_the_capacity() {
    let world = world(COMPONENT_SCENE);
    let tower = abi_handle(&world, "tower");
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let get = host.lod_get.unwrap();
    let set = host.lod_set.unwrap();
    // SAFETY: buffers and out-pointers are valid for each synchronous callback.
    unsafe {
        let mut count = 0_usize;
        let mut has_cull = false;
        let mut cull = 0.0_f32;
        let mut buffer = [-1.0_f32; 2];
        assert!(get(
            host.ctx,
            tower,
            buffer.as_mut_ptr(),
            1,
            &mut count,
            &mut has_cull,
            &mut cull
        ));
        assert_eq!(count, 2, "счётчик всегда полный");
        assert_eq!(buffer, [10.0, -1.0], "записан только capacity");
        assert!(has_cull);
        assert_eq!(cull, 50.0);

        assert!(get(
            host.ctx,
            tower,
            std::ptr::null_mut(),
            0,
            &mut count,
            &mut has_cull,
            &mut cull
        ));
        assert_eq!(count, 2);
        assert!(!get(
            host.ctx,
            tower,
            buffer.as_mut_ptr(),
            2,
            std::ptr::null_mut(),
            &mut has_cull,
            &mut cull
        ));

        let decreasing = [30.0_f32, 20.0];
        assert!(!set(host.ctx, tower, decreasing.as_ptr(), 2, true, 50.0));
        assert!(!set(host.ctx, tower, std::ptr::null(), 1, false, 0.0));
        assert!(get(
            host.ctx,
            tower,
            buffer.as_mut_ptr(),
            2,
            &mut count,
            &mut has_cull,
            &mut cull
        ));
        assert_eq!(
            (count, buffer),
            (2, [10.0, 20.0]),
            "отказ не меняет дистанции"
        );

        // Число уровней сохраняется: новые дистанции должны быть столько же и возрастать.
        let replaced = [5.0_f32, 15.0];
        assert!(set(host.ctx, tower, replaced.as_ptr(), 2, false, 0.0));
        assert!(get(
            host.ctx,
            tower,
            buffer.as_mut_ptr(),
            2,
            &mut count,
            &mut has_cull,
            &mut cull
        ));
        assert_eq!((count, buffer), (2, [5.0, 15.0]));
        assert!(!has_cull, "без cull_distance флаг сбрасывается");
    }
}

#[test]
fn audio_source_round_trips_and_refuses_invalid_settings_without_writing() {
    let world = world(COMPONENT_SCENE);
    let speaker = abi_handle(&world, "speaker");
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let get = host.audio_get_source.unwrap();
    let set = host.audio_set_source.unwrap();
    // SAFETY: state pointers are valid for each synchronous callback.
    unsafe {
        let mut before = RveAudioSourceState::default();
        assert!(get(host.ctx, speaker, &mut before));
        let refused = RveAudioSourceState {
            pitch: 0.0,
            ..before
        };
        assert!(!set(host.ctx, speaker, &refused));
        let mut after = RveAudioSourceState::default();
        assert!(get(host.ctx, speaker, &mut after));
        assert_eq!(after, before, "отказ не меняет источник");

        let valid = RveAudioSourceState {
            volume: 0.5,
            ..before
        };
        assert!(set(host.ctx, speaker, &valid));
        assert!(get(host.ctx, speaker, &mut after));
        assert_eq!(after.volume, 0.5);
        assert!(!get(host.ctx, speaker, std::ptr::null_mut()));
        assert!(!set(host.ctx, speaker, std::ptr::null()));
    }
}

#[test]
fn play_clip_ex_and_voice_position_refuse_contradictory_or_flat_voices() {
    let world = world(COMPONENT_SCENE);
    let crate_abi = abi_handle(&world, "crate");
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    let play = host.audio_play_clip_ex.unwrap();
    let set_position = host.audio_set_voice_position.unwrap();
    let path = RveStr::from_static("/audio/hum.wav");
    let base = RveAudioPlayExParams {
        group: RVE_AUDIO_GROUP_EFFECTS,
        volume: 1.0,
        spatial_blend: 1.0,
        min_distance: 1.0,
        max_distance: 30.0,
        ..Default::default()
    };
    let position = [1.0_f32, 0.0, 0.0];
    // SAFETY: params and output pointers are valid for each synchronous callback.
    unsafe {
        let mut voice = 0_u64;
        let both = RveAudioPlayExParams {
            has_position: true,
            position,
            follow: crate_abi,
            ..base
        };
        assert_eq!(
            play(host.ctx, path, &both, &mut voice),
            RveStatus::Error as u32
        );
        assert!(
            context
                .error
                .clone()
                .unwrap_or_default()
                .contains("нельзя задать одновременно")
        );
        assert_eq!(
            play(host.ctx, path, &base, std::ptr::null_mut()),
            RveStatus::Error as u32
        );

        assert_eq!(
            play(host.ctx, path, &base, &mut voice),
            RveStatus::Ok as u32
        );
        assert_eq!(
            set_position(host.ctx, voice, &position),
            RveStatus::Error as u32,
            "2D-голос не принимает позицию"
        );

        let spatial = RveAudioPlayExParams {
            has_position: true,
            position,
            ..base
        };
        let mut spatial_voice = 0_u64;
        assert_eq!(
            play(host.ctx, path, &spatial, &mut spatial_voice),
            RveStatus::Ok as u32
        );
        let nan = [f32::NAN, 0.0, 0.0];
        assert_eq!(
            set_position(host.ctx, spatial_voice, &nan),
            RveStatus::Error as u32
        );
        assert_eq!(
            set_position(host.ctx, spatial_voice, std::ptr::null()),
            RveStatus::Error as u32
        );
        assert_eq!(
            set_position(host.ctx, spatial_voice, &position),
            RveStatus::Ok as u32
        );
    }
}

#[test]
fn environment_null_pointers_are_refused() {
    let world = world(SKY_SCENE);
    let sky = abi_handle(&world, "sky");
    let events = EventQueue::default();
    let mut transaction = writable_transaction(&world);
    let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
    let host = context.host(ScriptTime::default());
    // SAFETY: null pointers are passed on purpose; the host must refuse them without reading.
    unsafe {
        assert!(!(host.environment_get.unwrap())(
            host.ctx,
            sky,
            std::ptr::null_mut()
        ));
        assert!(!(host.environment_set.unwrap())(
            host.ctx,
            sky,
            std::ptr::null()
        ));
    }
}
