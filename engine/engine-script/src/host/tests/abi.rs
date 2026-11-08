use super::world;
use crate::ScriptTime;
use crate::host::{ScriptHost, ScriptLoadError};
use crate::lifecycle::{PrefabCatalog, PrefabKind, PrefabRequest, PrefabSpec};
use crate::transaction::{MAX_MESSAGE_NAME_BYTES, MAX_MESSAGES_PER_FRAME};
use crate::types::ScriptEventLevel;
use engine_files::VirtualPath;
use engine_project::{InputAction, InputSettings};
use engine_scene::{
    AudioRequest, MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_STRING_BYTES,
    PrefabTemplate, World,
};
use rve_sdk::abi::{
    RVE_LOG_INFO, RVE_SCRIPT_ABI_MIN_VERSION, RVE_SCRIPT_ABI_VERSION, RVE_SPAWN_HAS_ID,
    RVE_SPAWN_HAS_PARENT, RveCreateFn, RveHeader, RveHost, RveModule, RveParams, RveScriptDesc,
    RveScriptDescV1, RveScriptDescV3, RveScriptDescV4, RveSpawnOptions, RveStatus, RveStr,
    RveTransform, RveUpdateFn, RveValue,
};
use rve_sdk::{
    AntiAliasing, CameraSettings, CollisionContact, ConsoleLevel, Key, MouseButton, Params,
    PhysicalInput, Script, Tonemapper, Value,
};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static PROBE_DROPS: AtomicUsize = AtomicUsize::new(0);
static CREATE_MESSAGE_DELIVERIES: AtomicUsize = AtomicUsize::new(0);
static ERROR_MESSAGE_DELIVERIES: AtomicUsize = AtomicUsize::new(0);
static BULK_MESSAGE_DELIVERIES: AtomicUsize = AtomicUsize::new(0);

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Probe {
    label: String,
    panic_update: bool,
    nan_update: bool,
    mutate_other: bool,
    panic_host_log: bool,
    panic_destroy: bool,
    panic_drop: bool,
}

struct InputProbe;
struct PhysicsProbe;
struct InputActionsProbe(u8);

struct CameraControl {
    panic_update: bool,
}

struct AbiConsoleCommand {
    panic_update: bool,
}

struct AbiConsoleForeign;

fn camera_control_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(CameraControl {
        panic_update: matches!(params.get("panic_update"), Some(Value::Bool(true))),
    }))
}

impl Script for CameraControl {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let camera = ctx.active_camera().expect("active camera exists");
        let mut settings = camera.camera_settings(ctx).expect("camera settings exist");
        assert_eq!(settings, CameraSettings::default());
        settings.fov_deg = 75.0;
        settings.render_scale = 0.5;
        settings.clear_color = [0.2, 0.3, 0.4];
        settings.exposure_ev = 2.0;
        settings.anti_aliasing = AntiAliasing::None;
        settings.tonemapper = Tonemapper::Neutral;
        assert!(camera.set_camera_settings(ctx, settings));
        assert_eq!(camera.camera_settings(ctx), Some(settings));

        let second = ctx.find("second").expect("second camera exists");
        assert!(second.activate_camera(ctx));
        assert_eq!(ctx.active_camera(), Some(second));
        if self.panic_update {
            panic!("camera update panic");
        }
    }
}

fn abi_console_command_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(AbiConsoleCommand {
        panic_update: matches!(params.get("panic_update"), Some(Value::Bool(true))),
    }))
}

fn abi_console_foreign_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(AbiConsoleForeign))
}

impl Script for AbiConsoleCommand {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        ctx.console.write("ABI info", ConsoleLevel::Log);
        ctx.console.write("ABI warning", ConsoleLevel::Warning);
        ctx.console.write("ABI error", ConsoleLevel::Error);
        ctx.console
            .register("abi.echo", Some("echo arguments"))
            .unwrap();
        assert!(ctx.console.register("abi.echo", None).is_err());
        assert!(ctx.console.register("help", None).is_err());
        assert!(ctx.console.register("Bad", None).is_err());
    }

    fn on_update(&mut self, _: &mut rve_sdk::Context<'_>, _: f32) {
        if self.panic_update {
            panic!("ABI update failure");
        }
    }

    fn on_command(&mut self, ctx: &mut rve_sdk::Context<'_>, name: &str, args: &[&str]) {
        assert_eq!(name, "abi.echo");
        if args == ["panic"] {
            ctx.transform.position[0] = 99.0;
            panic!("ABI console command panic");
        }
        if args == ["unregister"] {
            ctx.console.unregister(name).unwrap();
            return;
        }
        ctx.transform.position[0] += args.len() as f32;
        ctx.console
            .write(format!("echo:{}", args.join(" ")), ConsoleLevel::Log);
    }
}

impl Script for AbiConsoleForeign {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        assert!(ctx.console.unregister("abi.echo").is_err());
    }
}

fn input_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(InputProbe))
}

fn physics_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(PhysicsProbe))
}

impl Script for InputProbe {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let input = ctx.input();
        assert!(input.key_down(Key::W));
        assert!(input.key_pressed(Key::W));
        assert!(input.key_released(Key::A));
        assert!(input.mouse_down(MouseButton::Left));
        assert!(input.mouse_pressed(MouseButton::Left));
        assert!(input.mouse_released(MouseButton::Right));
        assert_eq!(input.cursor(), [40.0, 60.0]);
        assert_eq!(input.mouse_delta(), [2.5, -3.0]);
        assert_eq!(input.wheel(), [1.0, 0.5]);
        ctx.log_info("ABI input snapshot");
    }
}

impl Script for PhysicsProbe {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let actor = ctx.find("actor").expect("actor exists");
        let crate_object = ctx.find("crate").expect("crate exists");
        let hit = ctx
            .physics()
            .raycast([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], 10.0)
            .expect("raycast succeeds")
            .expect("crate hit");
        assert_eq!(hit.object.id(ctx).as_deref(), Some("crate"));
        assert!(hit.distance > 4.0);
        assert_eq!(crate_object.get_velocity(ctx).unwrap(), [0.0; 3]);
        crate_object.set_velocity(ctx, [1.0, 0.0, 0.0]).unwrap();
        crate_object.add_impulse(ctx, [0.0, 1.0, 0.0]).unwrap();
        crate_object.add_force(ctx, [0.0, 0.0, 2.0]).unwrap();
        let movement = actor.move_character(ctx, [0.0, 0.0, 0.0]).unwrap();
        assert_eq!(movement.translation, [0.0; 3]);
        assert!(!movement.grounded);
        assert!(!actor.is_grounded(ctx).unwrap());
        crate_object.set_collider_enabled(ctx, false).unwrap();
        assert!(!crate_object.is_collider_enabled(ctx).unwrap());
        assert!(
            ctx.physics()
                .raycast([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], 10.0)
                .unwrap()
                .is_none()
        );
        ctx.log_info("ABI physics API committed");
    }
}

struct AudioProbe;

struct AudioRejected;

fn audio_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(AudioProbe))
}

fn audio_rejected_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(AudioRejected))
}

impl Script for AudioProbe {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let actor = ctx.find("actor").expect("actor exists");
        actor.play_sound(ctx).unwrap();
        actor.set_sound_volume(ctx, 0.5).unwrap();
        let options = rve_sdk::ClipOptions {
            position: Some([1.0, 0.0, 0.0]),
            ..rve_sdk::ClipOptions::default()
        };
        let voice = ctx.play_clip("/audio/hum.wav", options).unwrap();
        ctx.stop_voice(voice).unwrap();
        ctx.set_group_volume(rve_sdk::AudioGroup::Music, 0.5)
            .unwrap();
        ctx.log_info("ABI audio API staged");
    }
}

impl Script for AudioRejected {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let actor = ctx.find("actor").expect("actor exists");
        actor.play_sound(ctx).unwrap();
        // The guest ignores the status; the host must still report the reason.
        assert_eq!(
            ctx.play_clip("../escape.wav", rve_sdk::ClipOptions::default()),
            Err(rve_sdk::AudioError::Rejected)
        );
    }
}

fn input_actions_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(InputActionsProbe(0)))
}

impl Script for InputActionsProbe {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        match self.0 {
            0 => {
                {
                    let input = ctx.input();
                    assert_eq!(input.context().as_deref(), Some("game"));
                    assert!(input.action_down("Jump"));
                    assert!(input.action_pressed("Jump"));
                    assert!(!input.action_released("Jump"));
                    assert_eq!(input.axis("MoveX"), Some(1.5));
                    let [x, y] = input.vector("Move").unwrap();
                    assert!((x - 1.7071068).abs() < 0.0001);
                    assert!((y - 0.20710677).abs() < 0.0001);
                    assert_eq!(input.axis("Jump"), None);
                    assert_eq!(input.vector("Missing"), None);
                    assert!(!input.action_down("Confirm"));
                    assert_eq!(
                        input.bindings("game", "Jump", "inputs"),
                        Some(vec!["Space".to_owned(), "W".to_owned()])
                    );
                    assert_eq!(input.bindings("game", "Empty", "inputs"), Some(Vec::new()));
                    assert_eq!(input.bindings("missing", "Jump", "inputs"), None);
                    let presses = input.pressed_inputs();
                    assert!(presses.contains(&"Space".to_owned()));
                    assert!(presses.contains(&"W".to_owned()));
                    assert!(presses.contains(&"D".to_owned()));
                    assert!(presses.contains(&"Mouse.Left".to_owned()));
                }
                assert!(ctx.set_input_context("menu"));
                assert!(ctx.rebind("game", "Jump", "inputs", &[PhysicalInput::Key(Key::Enter)],));
                ctx.log_info("ABI v5 action snapshot");
                self.0 = 1;
            }
            1 => {
                {
                    let input = ctx.input();
                    assert_eq!(input.context().as_deref(), Some("menu"));
                    assert!(!input.action_down("Jump"));
                    assert!(input.action_released("Jump"));
                    assert_eq!(
                        input.bindings("game", "Jump", "inputs"),
                        Some(vec!["Enter".to_owned()])
                    );
                    assert!(input.pressed_inputs().is_empty());
                }
                assert!(ctx.reset_bindings());
                ctx.log_info("ABI v5 action mutations");
                self.0 = 2;
            }
            _ => {
                let input = ctx.input();
                assert_eq!(input.context().as_deref(), Some("menu"));
                assert_eq!(
                    input.bindings("game", "Jump", "inputs"),
                    Some(vec!["Space".to_owned(), "W".to_owned()])
                );
                assert!(!input.action_released("Jump"));
                ctx.log_info("ABI v5 action reset");
            }
        }
    }
}

impl Script for Probe {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        let object_id = ctx.object_id().unwrap();
        assert!(matches!(object_id.as_str(), "camera" | "second"));
        if object_id == "camera" {
            assert!(matches!(
                ctx.object_name().as_deref(),
                Some("Camera object" | "camera")
            ));
        }
        assert_eq!(ctx.time().elapsed, 3.5);
        assert_eq!(ctx.time().frame, 17);
        ctx.log_info(format!("start:{}", self.label));
        if self.label == "from-params" {
            ctx.transform.position[0] += 10.0;
        }
    }

    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, dt: f32) {
        if self.mutate_other {
            let target = ctx.find("target").expect("target exists");
            let mut transform = target.transform(ctx).expect("target transform exists");
            transform.position[0] = 42.0;
            assert!(target.set_transform(ctx, transform));
            assert!(ctx.broadcast("rollback", &Value::Bool(true)));
        }
        if self.panic_update {
            ctx.transform.position[0] += 100.0;
            panic!("update panic");
        }
        if self.nan_update {
            ctx.transform.position[0] = f32::NAN;
            return;
        }
        ctx.transform.position[0] += dt;
        if self.panic_host_log {
            ctx.log_info("__rve_test_host_log_panic__");
        } else {
            ctx.log_info(format!("update:{}", self.label));
        }
    }

    fn on_destroy(&mut self, _: &mut rve_sdk::Context<'_>) {
        if self.panic_destroy {
            panic!("destroy panic");
        }
    }

    fn on_collision_enter(
        &mut self,
        ctx: &mut rve_sdk::Context<'_>,
        other: rve_sdk::Object,
        contact: CollisionContact,
    ) {
        let id = other.id(ctx).expect("physics event object is readable");
        ctx.log_info(format!(
            "abi collision:{id}:{}:{}",
            contact.point[0], contact.normal[0]
        ));
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        PROBE_DROPS.fetch_add(1, Ordering::SeqCst);
        if self.panic_drop {
            panic!("drop panic");
        }
    }
}

fn probe_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    let label = match params.get("label") {
        Some(Value::String(label)) => label.clone(),
        _ => "default".to_owned(),
    };
    Ok(Box::new(Probe {
        label,
        panic_update: matches!(params.get("panic_update"), Some(Value::Bool(true))),
        nan_update: matches!(params.get("nan_update"), Some(Value::Bool(true))),
        mutate_other: matches!(params.get("mutate_other"), Some(Value::Bool(true))),
        panic_host_log: matches!(params.get("panic_host_log"), Some(Value::Bool(true))),
        panic_destroy: matches!(params.get("panic_destroy"), Some(Value::Bool(true))),
        panic_drop: matches!(params.get("panic_drop"), Some(Value::Bool(true))),
    }))
}

fn error_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Err("factory rejected parameters".to_owned())
}

fn panic_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    panic!("factory panic")
}

struct DoubleWrite {
    change_field: bool,
}

fn double_write_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(DoubleWrite {
        change_field: matches!(params.get("change_field"), Some(Value::Bool(true))),
    }))
}

impl Script for DoubleWrite {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let object_id = ctx.object_id().unwrap();
        let own = ctx.find(&object_id).unwrap();
        assert_eq!(own.id(ctx).as_deref(), Some(object_id.as_str()));
        assert_eq!(own.name(ctx).as_deref(), Some(object_id.as_str()));
        let initial_x = if object_id == "field-wins" { 1.0 } else { 2.0 };
        assert_eq!(own.world_matrix(ctx).unwrap()[12], initial_x);
        let mut transform = own.transform(ctx).unwrap();
        transform.position[0] = 9.0;
        assert!(own.set_transform(ctx, transform));
        assert_eq!(own.world_matrix(ctx).unwrap()[12], initial_x);
        if self.change_field {
            ctx.transform.position[1] = 5.0;
        }
    }
}

struct MessageScript {
    role: String,
}

fn message_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    let Some(Value::String(role)) = params.get("role") else {
        return Err("missing role".to_owned());
    };
    Ok(Box::new(MessageScript { role: role.clone() }))
}

impl Script for MessageScript {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        if self.role == "sender" {
            let value = Value::Array(vec![Value::Int(2), Value::String("abi".to_owned())]);
            assert!(ctx.send("receiver", "ping", &value));
            assert!(!ctx.send("missing", "unknown", &Value::Bool(false)));
            assert!(ctx.broadcast("fan", &Value::String("start".to_owned())));
        } else if self.role == "bulk_sender" {
            for _ in 0..(MAX_MESSAGES_PER_FRAME + 4) {
                assert!(ctx.send("receiver", "bulk", &Value::Bool(true)));
            }
        } else if self.role == "boundary_sender" {
            let mut depth = Value::Bool(true);
            for _ in 0..MAX_MESSAGE_DEPTH {
                depth = Value::Array(vec![depth]);
            }
            let elements = Value::Array(vec![Value::Bool(true); MAX_MESSAGE_ELEMENTS - 1]);
            let strings = Value::Array(vec![
                Value::String("a".repeat(MAX_MESSAGE_STRING_BYTES / 2)),
                Value::String("b".repeat(MAX_MESSAGE_STRING_BYTES / 2)),
            ]);
            let max_name = "n".repeat(MAX_MESSAGE_NAME_BYTES);
            assert!(ctx.send("receiver", "depth", &depth));
            assert!(ctx.send("receiver", "elements", &elements));
            assert!(ctx.send("receiver", "strings", &strings));
            assert!(ctx.send("receiver", &max_name, &Value::Bool(true)));
        }
    }

    fn on_message(
        &mut self,
        ctx: &mut rve_sdk::Context<'_>,
        sender: &str,
        name: &str,
        value: &Value,
    ) {
        if self.role == "receiver" && sender == "boundary_sender" {
            let accepted = match name {
                "depth" => {
                    let mut expected = Value::Bool(true);
                    for _ in 0..MAX_MESSAGE_DEPTH {
                        expected = Value::Array(vec![expected]);
                    }
                    value == &expected
                }
                "elements" => matches!(
                    value,
                    Value::Array(values) if values.len() == MAX_MESSAGE_ELEMENTS - 1
                ),
                "strings" => matches!(
                    value,
                    Value::Array(values)
                        if values.len() == 2
                            && matches!(&values[0], Value::String(value) if value.len() == MAX_MESSAGE_STRING_BYTES / 2)
                            && matches!(&values[1], Value::String(value) if value.len() == MAX_MESSAGE_STRING_BYTES / 2)
                ),
                name if name.len() == MAX_MESSAGE_NAME_BYTES => value == &Value::Bool(true),
                _ => false,
            };
            assert!(accepted, "unexpected boundary message {name:?}");
        }
        if self.role == "receiver" && name == "ping" {
            assert_eq!(
                value,
                &Value::Array(vec![Value::Int(2), Value::String("abi".to_owned())])
            );
            assert!(ctx.send("sender", "reply", &Value::Bool(true)));
            ctx.transform.position[2] = 1.0;
        }
        if self.role == "receiver" && name == "bulk" {
            BULK_MESSAGE_DELIVERIES.fetch_add(1, Ordering::SeqCst);
        }
        ctx.log_info(format!("{}:{name}:{sender}", self.role));
    }

    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        if self.role == "disabled" && ctx.time().frame == 1 {
            panic!("disable broadcast recipient");
        }
        if self.role == "broadcast_sender" && ctx.time().frame == 1 {
            assert!(ctx.broadcast("enabled_only", &Value::Bool(true)));
        }
        ctx.log_info(format!("{}:update", self.role));
    }
}

struct LimitMessage;

fn limit_message_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(LimitMessage))
}

impl Script for LimitMessage {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        let mut too_deep = Value::Bool(true);
        for _ in 0..=MAX_MESSAGE_DEPTH {
            too_deep = Value::Array(vec![too_deep]);
        }
        let too_many = Value::Array(vec![Value::Bool(true); MAX_MESSAGE_ELEMENTS]);
        let too_many_bytes = Value::Array(vec![
            Value::String("a".repeat(MAX_MESSAGE_STRING_BYTES / 2)),
            Value::String("b".repeat(MAX_MESSAGE_STRING_BYTES / 2 + 1)),
        ]);
        for value in [&too_deep, &too_many, &too_many_bytes] {
            assert!(!ctx.send("receiver", "invalid", value));
            assert!(!ctx.broadcast("invalid", value));
        }

        let too_long_name = "n".repeat(MAX_MESSAGE_NAME_BYTES + 1);
        assert!(!ctx.send("receiver", &too_long_name, &Value::Bool(true)));
        assert!(!ctx.broadcast(&too_long_name, &Value::Bool(true)));
        ctx.log_info("message limits rejected");
    }
}

/// Spawns one bolt from `on_update` and then drives its handle one step per update.
#[derive(Default)]
struct SpawnDriver {
    step: u32,
    bolt: Option<rve_sdk::Object>,
}

fn spawn_driver_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(SpawnDriver::default()))
}

impl Script for SpawnDriver {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        let step = self.step;
        self.step += 1;
        match step {
            0 => {
                let options = rve_sdk::SpawnOptions::new()
                    .position([1.0, 0.0, 0.0])
                    .id("bolt")
                    .name("Bolt");
                let bolt = ctx.spawn("prefabs/bolt.rveprefab", &options);
                self.bolt = bolt;
                ctx.log_info(format!("spawned={}", bolt.is_some()));
                if let Some(bolt) = bolt {
                    let pending = bolt.is_pending(ctx);
                    let alive = bolt.is_alive(ctx);
                    ctx.log_info(format!("pending={pending} alive={alive}"));
                }
                // The target is still pending, so only the transaction's pending ids can resolve it.
                let sent = ctx.send("bolt", "ping", &rve_sdk::Value::Bool(true));
                ctx.log_info(format!("sent={sent}"));
            }
            1 => {
                self.log_state(ctx);
                let destroyed = self.bolt_handle().destroy(ctx, 1.0);
                ctx.log_info(format!("destroy={destroyed}"));
            }
            2 | 3 => self.log_state(ctx),
            4 => {
                self.log_state(ctx);
                let first = self.bolt_handle().destroy(ctx, 0.0);
                let second = self.bolt_handle().destroy(ctx, 0.0);
                ctx.log_info(format!("dead_destroy={first}"));
                ctx.log_info(format!("dead_destroy={second}"));
            }
            _ => {}
        }
    }
}

impl SpawnDriver {
    fn bolt_handle(&self) -> rve_sdk::Object {
        self.bolt.expect("bolt was spawned in the first update")
    }

    fn log_state(&self, ctx: &mut rve_sdk::Context<'_>) {
        let bolt = self.bolt_handle();
        let alive = bolt.is_alive(ctx);
        let pending = bolt.is_pending(ctx);
        ctx.log_info(format!("alive={alive} pending={pending}"));
    }
}

/// Destroys `victim` on its first update, then calls every handle-taking object API on the dead
/// object. The guest must see no-op results and no error, and keep running.
#[derive(Default)]
struct DeadHandleProbe {
    step: u32,
    victim: Option<rve_sdk::Object>,
}

fn dead_handle_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(DeadHandleProbe::default()))
}

impl Script for DeadHandleProbe {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        self.step += 1;
        if self.step == 1 {
            let victim = ctx.find("victim").expect("victim exists");
            self.victim = Some(victim);
            let destroyed = victim.destroy(ctx, 0.0);
            ctx.log_info(format!("destroy={destroyed}"));
            return;
        }
        let victim = self.victim.expect("victim was found in the first update");
        let alive = victim.is_alive(ctx);
        let velocity = victim.get_velocity(ctx).err();
        let set_velocity = victim.set_velocity(ctx, [1.0, 0.0, 0.0]).err();
        let movement = victim.move_character(ctx, [0.0; 3]).err();
        let matrix_none = victim.world_matrix(ctx).is_none();
        let transform = rve_sdk::Transform {
            position: [0.0; 3],
            rotation_deg: [0.0; 3],
            scale: [1.0; 3],
        };
        let set_transform = victim.set_transform(ctx, transform);
        ctx.log_info(format!("alive={alive}"));
        ctx.log_info(format!("velocity_err={velocity:?}"));
        ctx.log_info(format!("set_velocity_err={set_velocity:?}"));
        ctx.log_info(format!("move_err={movement:?}"));
        ctx.log_info(format!("matrix_none={matrix_none}"));
        ctx.log_info(format!("set_transform={set_transform}"));
        ctx.log_info("still running");
    }
}

/// Destroys `victim` on its first update, then reports each collision exit it receives and keeps
/// logging its update number.
#[derive(Default)]
struct ExitWatcher {
    step: u32,
}

fn exit_watcher_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(ExitWatcher::default()))
}

impl Script for ExitWatcher {
    fn on_update(&mut self, ctx: &mut rve_sdk::Context<'_>, _: f32) {
        self.step += 1;
        if self.step == 1 {
            let victim = ctx.find("victim").expect("victim exists");
            let destroyed = victim.destroy(ctx, 0.0);
            ctx.log_info(format!("destroy={destroyed}"));
        }
        ctx.log_info(format!("update {}", self.step));
    }

    fn on_collision_exit(&mut self, ctx: &mut rve_sdk::Context<'_>, other: rve_sdk::Object) {
        ctx.log_info(format!("exit alive={}", other.is_alive(ctx)));
    }
}

/// Heavy script of the spawned bolt: reports its start, the message sent to its id, and its end.
struct BoltProbe;

fn bolt_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(BoltProbe))
}

impl Script for BoltProbe {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        let id = ctx.object_id().unwrap_or_default();
        ctx.log_info(format!("start:{id}"));
    }

    fn on_message(&mut self, ctx: &mut rve_sdk::Context<'_>, _: &str, name: &str, _: &Value) {
        ctx.log_info(format!("hit:{name}"));
    }

    fn on_destroy(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        ctx.log_info("bolt destroyed");
    }
}

/// Reads prefab states and requests from `on_start`.
struct PrefabProbe;

fn prefab_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(PrefabProbe))
}

impl Script for PrefabProbe {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        let prefabs = ctx.prefabs();
        let resident = prefabs.state("prefabs/bolt.rveprefab");
        let unknown = prefabs.state("prefabs/missing.rveprefab");
        let heavy = prefabs.state("prefabs/heavy.rveprefab");
        let loaded = prefabs.load("prefabs/heavy.rveprefab");
        let rejected = prefabs.load("prefabs/missing.rveprefab");
        ctx.log_info(format!(
            "resident={resident:?} unknown={unknown:?} heavy={heavy:?} load={loaded} rejected={rejected}"
        ));
    }
}

fn sdk_module() -> &'static RveModule {
    rve_sdk::script_module! {
        "Probe" => probe_factory,
        "Error" => error_factory,
        "Panic" => panic_factory,
        "DoubleWrite" => double_write_factory,
        "Message" => message_factory,
        "Limits" => limit_message_factory,
        "InputProbe" => input_probe_factory,
        "PhysicsProbe" => physics_probe_factory,
        "AudioProbe" => audio_probe_factory,
        "AudioRejected" => audio_rejected_factory,
        "InputActionsProbe" => input_actions_probe_factory,
        "SpawnDriver" => spawn_driver_factory,
        "BoltProbe" => bolt_probe_factory,
        "PrefabProbe" => prefab_probe_factory,
        "DeadHandleProbe" => dead_handle_probe_factory,
        "ExitWatcher" => exit_watcher_factory,
        "CameraControl" => camera_control_factory,
        "AbiConsoleCommand" => abi_console_command_factory,
        "AbiConsoleForeign" => abi_console_foreign_factory,
    }
}

fn library() -> crate::ScriptLibrary {
    // SAFETY: the SDK macro returns an immutable process-lifetime table.
    match unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(sdk_module())) } {
        Ok(library) => library,
        Err(error) => panic!("invalid test ABI module: {error}"),
    }
}

fn input_action_settings() -> InputSettings {
    InputSettings {
        default_context: Some("game".to_owned()),
        contexts: BTreeMap::from([
            (
                "game".to_owned(),
                BTreeMap::from([
                    (
                        "Jump".to_owned(),
                        InputAction::Button {
                            inputs: vec!["Space".to_owned(), "W".to_owned()],
                        },
                    ),
                    (
                        "MoveX".to_owned(),
                        InputAction::Axis {
                            negative: vec!["A".to_owned()],
                            positive: vec!["D".to_owned()],
                            analog: Some("MouseX".to_owned()),
                            scale: 0.25,
                        },
                    ),
                    (
                        "Move".to_owned(),
                        InputAction::Vector2 {
                            up: vec!["W".to_owned()],
                            down: vec!["S".to_owned()],
                            left: vec!["A".to_owned()],
                            right: vec!["D".to_owned()],
                            analog: Some("MouseDelta".to_owned()),
                            scale: 0.5,
                        },
                    ),
                    (
                        "Empty".to_owned(),
                        InputAction::Button { inputs: Vec::new() },
                    ),
                ]),
            ),
            (
                "menu".to_owned(),
                BTreeMap::from([(
                    "Confirm".to_owned(),
                    InputAction::Button {
                        inputs: vec!["Enter".to_owned()],
                    },
                )]),
            ),
        ]),
    }
}

fn scripted_world(script_name: &str, params: &str) -> World {
    world(&format!(
        r#"
[[objects]]
id = "camera"
name = "Camera object"
[objects.camera]
[[objects.heavy_scripts]]
name = {script_name:?}
params = {params}
"#
    ))
}

fn empty_sources() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn load(world: &World, library: &crate::ScriptLibrary) -> Result<ScriptHost, ScriptLoadError> {
    ScriptHost::load(world, "scene", &empty_sources(), Some(library))
}

fn load_host(world: &World, library: &crate::ScriptLibrary) -> ScriptHost {
    match load(world, library) {
        Ok(host) => host,
        Err(error) => panic!("unexpected script load error: {error}"),
    }
}

fn message_list(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect()
}

#[test]
fn version_mismatch_does_not_read_the_module_table() {
    for version in [0, RVE_SCRIPT_ABI_VERSION + 1] {
        let module = RveModule {
            header: RveHeader {
                abi_version: version,
                header_size: std::mem::size_of::<RveHeader>() as u32,
            },
            scripts: std::ptr::null(),
            script_count: 1,
        };
        // SAFETY: the header is valid; unsupported versions are rejected without table reads.
        let result = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(&module)) };
        assert!(matches!(
            result,
            Err(crate::ScriptLibraryError::AbiVersion {
                min: RVE_SCRIPT_ABI_MIN_VERSION,
                max: RVE_SCRIPT_ABI_VERSION,
                found,
            }) if found == version
        ));
    }
}

#[test]
fn abi_v3_descriptor_loads_without_reading_a_command_tail() {
    let module = v3_probe_module();
    // SAFETY: the v3 module and its exact-sized descriptor table are leaked for the test process.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let descriptor = library.script("Probe").unwrap();
    assert!(descriptor.command.is_none());
}

#[test]
fn abi_v4_descriptor_loads_without_reading_the_v6_collision_tail() {
    let module = command_probe_module(4);
    // SAFETY: the v4 module and exact-sized descriptor table are leaked for the test process.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let descriptor = library.script("Probe").unwrap();
    assert!(descriptor.command.is_some());
    assert!(descriptor.collision.is_none());
}

#[test]
fn abi_v5_descriptor_uses_the_command_layout_without_collision() {
    let module = command_probe_module(5);
    // SAFETY: the v5 module and its two exact-sized descriptors are leaked for the test process.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    // The second entry checks that the loader strides by the v4/v5 descriptor size.
    for name in ["Probe", "ProbeSecond"] {
        let descriptor = library.script(name).unwrap();
        assert!(descriptor.command.is_some());
        assert!(descriptor.collision.is_none());
    }
}

#[test]
fn abi_v3_v4_and_v5_normalized_descriptors_run_without_appended_callbacks() {
    let _lock = test_lock();
    for module in [
        v3_probe_module(),
        command_probe_module(4),
        command_probe_module(5),
    ] {
        // SAFETY: each module and its version-specific descriptor table live for this test.
        let library =
            unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
        let mut world = scripted_world("Probe", r#"{ label = "legacy" }"#);
        let mut host = load_host(&world, &library);
        host.start(
            &mut world,
            ScriptTime {
                elapsed: 3.5,
                frame: 17,
            },
        );
        assert!(
            message_list(&mut host)
                .iter()
                .any(|message| message == "start:legacy")
        );
    }
}

/// Builds a module with the `RveScriptDescV4` layout (no collision tail) for ABI 4 or 5.
fn command_probe_module(abi_version: u32) -> &'static RveModule {
    let current = sdk_module();
    // SAFETY: `sdk_module` returns an immutable process-lifetime descriptor table.
    let probe = unsafe { std::slice::from_raw_parts(current.scripts, current.script_count) }
        .iter()
        .find(|script| unsafe { script.name.to_str() }.ok() == Some("Probe"))
        .expect("Probe descriptor exists");
    let entry = |name| RveScriptDescV4 {
        name,
        create: probe.create,
        start: probe.start,
        update: probe.update,
        message: probe.message,
        destroy: probe.destroy,
        drop: probe.drop,
        params: probe.params,
        param_count: probe.param_count,
        command: probe.command,
    };
    let scripts = Box::leak(Box::new([
        entry(probe.name),
        entry(RveStr::from_static("ProbeSecond")),
    ]));
    Box::leak(Box::new(RveModule {
        header: RveHeader {
            abi_version,
            header_size: std::mem::size_of::<RveHeader>() as u32,
        },
        scripts: scripts.as_ptr().cast(),
        script_count: scripts.len(),
    }))
}

#[test]
fn abi_v6_collision_callback_receives_other_and_contact() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
name = "Camera object"
[objects.camera]
[[objects.heavy_scripts]]
name = "Probe"
[[objects]]
id = "second"
"#,
    );
    let camera = world.handle("camera").unwrap();
    let second = world.handle("second").unwrap();
    let physics = engine_physics::PhysicsWorld::new(glam::Vec3::ZERO).unwrap();
    let bindings = [
        engine_physics::PhysicsSceneBinding {
            object: camera,
            body: engine_physics::BodyHandle(101),
            kind: engine_physics::BodyKind::Static,
            character_controller: None,
        },
        engine_physics::PhysicsSceneBinding {
            object: second,
            body: engine_physics::BodyHandle(102),
            kind: engine_physics::BodyKind::Static,
            character_controller: None,
        },
    ];
    let mut host = load_host(&world, &library());
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.queue_physics_events([engine_physics::PhysicsEvent::CollisionEnter {
        first: engine_physics::BodyHandle(101),
        second: engine_physics::BodyHandle(102),
        point: glam::Vec3::X,
        normal: glam::Vec3::X,
    }]);

    host.update(
        &mut world,
        1.0 / 60.0,
        ScriptTime {
            elapsed: 3.5,
            frame: 17,
        },
    );

    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message == "abi collision:second:1:1")
    );
}

#[test]
fn abi_v6_physics_host_callbacks_query_and_stage_commands() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[objects]]
id = "actor"
[[objects.heavy_scripts]]
name = "PhysicsProbe"
[[objects]]
id = "crate"
"#,
    );
    let actor = world.handle("actor").unwrap();
    let crate_object = world.handle("crate").unwrap();
    let controller =
        engine_physics::CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
    let mut physics = engine_physics::PhysicsWorld::new(glam::Vec3::ZERO).unwrap();
    let actor_body = physics
        .insert_body({
            let mut desc = engine_physics::BodyDesc::new(
                engine_physics::BodyKind::Kinematic,
                engine_physics::Pose::IDENTITY,
                engine_physics::ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let crate_body = physics
        .insert_body(engine_physics::BodyDesc::dynamic(
            engine_physics::Shape::sphere(0.5).unwrap(),
            engine_physics::Pose::new(glam::Vec3::X * 5.0, glam::Quat::IDENTITY).unwrap(),
            1.0,
        ))
        .unwrap();
    let bindings = [
        engine_physics::PhysicsSceneBinding {
            object: actor,
            body: actor_body,
            kind: engine_physics::BodyKind::Kinematic,
            character_controller: Some(controller),
        },
        engine_physics::PhysicsSceneBinding {
            object: crate_object,
            body: crate_body,
            kind: engine_physics::BodyKind::Dynamic,
            character_controller: None,
        },
    ];
    let mut host = load_host(&world, &library());
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let commands = host.drain_physics_commands();
    assert_eq!(commands.len(), 4);
    assert!(
        matches!(commands[0], engine_physics::PhysicsCommand::SetVelocity { body, velocity }
        if body == crate_body && velocity == glam::Vec3::X)
    );
    assert!(
        matches!(commands[1], engine_physics::PhysicsCommand::AddImpulse { body, impulse, .. }
        if body == crate_body && impulse == glam::Vec3::Y)
    );
    assert!(
        matches!(commands[2], engine_physics::PhysicsCommand::AddForce { body, force, .. }
        if body == crate_body && force == glam::Vec3::Z * 2.0)
    );
    assert!(
        matches!(commands[3], engine_physics::PhysicsCommand::SetColliderEnabled { body, enabled: false }
        if body == crate_body)
    );
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message == "ABI physics API committed")
    );
}

#[test]
fn abi_dead_object_handles_are_noops_with_one_warning() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[objects]]
id = "probe"
[[objects.heavy_scripts]]
name = "DeadHandleProbe"
[[objects]]
id = "victim"
"#,
    );
    let mut host = load_host(&world, &library());
    // Physics callbacks are offered only while physics is attached, so attach an empty world.
    let physics = engine_physics::PhysicsWorld::new(glam::Vec3::ZERO).unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &[]);
    host.update(&mut world, 1.0 / 60.0, at(0.1, 1));
    assert!(world.handle("victim").is_none());
    assert_eq!(info_messages(&mut host), ["destroy=true"]);

    host.update(&mut world, 1.0 / 60.0, at(0.2, 2));
    let events = host.drain_events();
    let infos: Vec<_> = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Info)
        .map(|event| event.message.as_str())
        .collect();
    assert_eq!(
        infos,
        [
            "alive=false",
            "velocity_err=Some(Rejected)",
            "set_velocity_err=Some(Rejected)",
            "move_err=Some(Rejected)",
            "matrix_none=true",
            "set_transform=false",
            "still running",
        ],
        "every handle-taking call on the dead object is a no-op"
    );
    let mut warnings = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Warn)
        .count();

    host.update(&mut world, 1.0 / 60.0, at(0.3, 3));
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .all(|event| event.level != ScriptEventLevel::Error),
        "no callback error"
    );
    warnings += events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Warn)
        .count();
    assert_eq!(warnings, 1, "one warning per script and dead object");
}

#[test]
fn abi_exit_with_a_destroyed_other_is_delivered_only_inside_the_retired_window() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[objects]]
id = "sensor"
[[objects.heavy_scripts]]
name = "ExitWatcher"
[[objects]]
id = "victim"
"#,
    );
    let sensor = world.handle("sensor").unwrap();
    let victim = world.handle("victim").unwrap();
    let mut physics = engine_physics::PhysicsWorld::new(glam::Vec3::ZERO).unwrap();
    let sensor_body = physics
        .insert_body(engine_physics::BodyDesc::static_collider(
            engine_physics::Shape::sphere(0.5).unwrap(),
            engine_physics::Pose::IDENTITY,
        ))
        .unwrap();
    let victim_body = physics
        .insert_body(engine_physics::BodyDesc::static_collider(
            engine_physics::Shape::sphere(0.5).unwrap(),
            engine_physics::Pose::new(glam::Vec3::X * 2.0, glam::Quat::IDENTITY).unwrap(),
        ))
        .unwrap();
    let binding = |object, body| engine_physics::PhysicsSceneBinding {
        object,
        body,
        kind: engine_physics::BodyKind::Static,
        character_controller: None,
    };
    let both = [binding(sensor, sensor_body), binding(victim, victim_body)];
    let sensor_only = [binding(sensor, sensor_body)];
    let snapshot = physics.query_snapshot().unwrap();
    let exit = engine_physics::PhysicsEvent::CollisionExit {
        first: sensor_body,
        second: victim_body,
    };
    let mut host = load_host(&world, &library());

    host.set_physics_snapshot(snapshot.clone(), &both);
    host.update(&mut world, 1.0 / 60.0, at(0.1, 1));
    assert!(world.handle("victim").is_none());
    host.drain_events();

    host.set_physics_snapshot(snapshot.clone(), &sensor_only);
    host.queue_physics_events([exit.clone()]);
    host.update(&mut world, 1.0 / 60.0, at(0.2, 2));
    assert!(
        info_messages(&mut host).contains(&"exit alive=false".to_owned()),
        "the survivor receives the exit inside the window"
    );

    // The window closes with the second result; the stale binding must not reach the guest.
    host.set_physics_snapshot(snapshot, &both);
    host.queue_physics_events([exit]);
    host.update(&mut world, 1.0 / 60.0, at(0.3, 3));
    let after = info_messages(&mut host);
    assert!(
        !after.iter().any(|message| message.starts_with("exit ")),
        "{after:?}"
    );
    assert!(after.contains(&"update 3".to_owned()), "{after:?}");
    assert!(error_messages(&mut host).is_empty());
}

#[test]
fn abi_v7_audio_callbacks_stage_requests_in_guest_order() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[assets]]
path = "/audio/hum.wav"
[[objects]]
id = "actor"
[objects.audio_source]
clip = "/audio/hum.wav"
[[objects.heavy_scripts]]
name = "AudioProbe"
"#,
    );
    let mut host = load_host(&world, &library());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let requests = host.drain_audio_requests();
    assert_eq!(requests.len(), 5, "{requests:?}");
    assert!(matches!(requests[0], AudioRequest::PlaySource { .. }));
    assert!(matches!(
        requests[1],
        AudioRequest::SetSourceVolume { volume, .. } if volume == 0.5
    ));
    let played = match &requests[2] {
        AudioRequest::PlayClip {
            voice,
            path,
            params,
        } => {
            assert_eq!(path.as_str(), "audio/hum.wav");
            assert_eq!(params.position, Some([1.0, 0.0, 0.0]));
            *voice
        }
        other => panic!("ожидался PlayClip: {other:?}"),
    };
    assert!(matches!(
        requests[3],
        AudioRequest::StopVoice { voice } if voice == played
    ));
    assert!(matches!(
        requests[4],
        AudioRequest::SetGroupVolume {
            group: engine_scene::AudioGroup::Music,
            volume
        } if volume == 0.5
    ));
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message == "ABI audio API staged")
    );
}

#[test]
fn abi_v7_rejected_audio_reason_is_reported_even_when_the_guest_ignores_the_status() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[assets]]
path = "/audio/hum.wav"
[[objects]]
id = "actor"
[objects.audio_source]
clip = "/audio/hum.wav"
[[objects.heavy_scripts]]
name = "AudioRejected"
"#,
    );
    let mut host = load_host(&world, &library());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error
                && event.message.starts_with("play_clip:")),
        "{events:?}"
    );
    // The call itself succeeded, so its earlier play_sound is still committed.
    assert_eq!(host.drain_audio_requests().len(), 1);
}

#[test]
fn abi_v6_to_v8_modules_load_on_the_v9_host() {
    let _lock = test_lock();
    let current = sdk_module();
    for abi_version in [6, 7, 8] {
        let module = Box::leak(Box::new(RveModule {
            header: RveHeader {
                abi_version,
                header_size: std::mem::size_of::<RveHeader>() as u32,
            },
            scripts: current.scripts,
            script_count: current.script_count,
        }));
        // SAFETY: the module and its descriptor table live for the whole test process.
        let library =
            unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
        let mut world = scripted_world("Probe", r#"{ label = "v6" }"#);
        let mut host = load_host(&world, &library);
        host.start(
            &mut world,
            ScriptTime {
                elapsed: 3.5,
                frame: 17,
            },
        );
        assert!(
            message_list(&mut host)
                .iter()
                .any(|message| message == "start:v6"),
            "guest ABI v{abi_version} runs on the v9 host"
        );
    }
}

fn v3_probe_module() -> &'static RveModule {
    let current = sdk_module();
    // SAFETY: `sdk_module` returns an immutable process-lifetime descriptor table.
    let probe = unsafe { std::slice::from_raw_parts(current.scripts, current.script_count) }
        .iter()
        .find(|script| unsafe { script.name.to_str() }.ok() == Some("Probe"))
        .expect("Probe descriptor exists");
    let scripts = Box::leak(Box::new([RveScriptDescV3 {
        name: probe.name,
        create: probe.create,
        start: probe.start,
        update: probe.update,
        message: probe.message,
        destroy: probe.destroy,
        drop: probe.drop,
        params: probe.params,
        param_count: probe.param_count,
    }]));
    Box::leak(Box::new(RveModule {
        header: RveHeader {
            abi_version: 3,
            header_size: std::mem::size_of::<RveHeader>() as u32,
        },
        scripts: scripts.as_ptr().cast(),
        script_count: scripts.len(),
    }))
}

/// Presents the `AbiConsoleCommand` descriptor as an ABI 4 table. The v6 table cannot be reused
/// directly because its stride is larger than the v4 descriptor layout.
fn v4_sdk_module() -> &'static RveModule {
    let current = sdk_module();
    // SAFETY: `sdk_module` returns an immutable process-lifetime descriptor table.
    let command = unsafe { std::slice::from_raw_parts(current.scripts, current.script_count) }
        .iter()
        .find(|script| unsafe { script.name.to_str() }.ok() == Some("AbiConsoleCommand"))
        .expect("AbiConsoleCommand descriptor exists");
    let scripts = Box::leak(Box::new([RveScriptDescV4 {
        name: command.name,
        create: command.create,
        start: command.start,
        update: command.update,
        message: command.message,
        destroy: command.destroy,
        drop: command.drop,
        params: command.params,
        param_count: command.param_count,
        command: command.command,
    }]));
    Box::leak(Box::new(RveModule {
        header: RveHeader {
            abi_version: 4,
            header_size: std::mem::size_of::<RveHeader>() as u32,
        },
        scripts: scripts.as_ptr().cast(),
        script_count: scripts.len(),
    }))
}

#[test]
fn legacy_abi_v1_and_v2_descriptors_are_rejected() {
    for version in [1, 2] {
        let module = legacy_probe_module(version);
        // SAFETY: the leaked module is immutable; rejection reads only its header.
        assert!(matches!(
            unsafe { crate::ScriptLibrary::from_module(module) },
            Err(crate::ScriptLibraryError::AbiVersion { found, .. }) if found == version
        ));
    }
}
fn legacy_probe_module(version: u32) -> &'static RveModule {
    let current = sdk_module();
    // SAFETY: `sdk_module` returns an immutable process-lifetime descriptor table.
    let probe = unsafe { std::slice::from_raw_parts(current.scripts, current.script_count) }
        .iter()
        .find(|script| unsafe { script.name.to_str() }.ok() == Some("Probe"))
        .expect("Probe descriptor exists");
    let scripts = Box::leak(Box::new([RveScriptDescV1 {
        name: probe.name,
        create: probe.create,
        start: probe.start,
        update: probe.update,
        message: probe.message,
        destroy: probe.destroy,
        drop: probe.drop,
    }]));
    Box::leak(Box::new(RveModule {
        header: RveHeader {
            abi_version: version,
            header_size: std::mem::size_of::<RveHeader>() as u32,
        },
        scripts: scripts.as_ptr().cast(),
        script_count: scripts.len(),
    }))
}

#[test]
fn abi_console_commands_execute_with_levels_and_owner_checks() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[objects]]
id = "owner"
[[objects.heavy_scripts]]
name = "AbiConsoleCommand"
[[objects]]
id = "foreign"
[[objects.heavy_scripts]]
name = "AbiConsoleForeign"
"#,
    );
    let owner = world.handle("owner").unwrap();
    let mut host = load_host(&world, &library());
    host.start(&mut world, ScriptTime::default());

    assert!(
        host.commands()
            .iter()
            .any(|command| command.name == "abi.echo")
    );
    assert_eq!(host.commands_generation(), 1);
    let start_events = host.drain_events();
    assert_eq!(
        start_events
            .iter()
            .map(|event| event.level)
            .collect::<Vec<_>>(),
        [
            ScriptEventLevel::Info,
            ScriptEventLevel::Warn,
            ScriptEventLevel::Error
        ]
    );

    let command = engine_console::parse_command_line("abi.echo first second").unwrap();
    host.execute_command(&mut world, &command).unwrap();
    assert_eq!(world.local_transform(owner).unwrap().position[0], 2.0);
    let command_events = host.drain_events();
    assert!(command_events.iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "echo:first second"
    }));

    let unregister = engine_console::parse_command_line("abi.echo unregister").unwrap();
    host.execute_command(&mut world, &unregister).unwrap();
    assert_eq!(host.commands_generation(), 2);
    assert!(
        !host
            .commands()
            .iter()
            .any(|command| command.name == "abi.echo")
    );
    assert!(host.execute_command(&mut world, &unregister).is_err());
}

#[test]
fn abi_v4_guest_commands_still_load_and_execute() {
    let _lock = test_lock();
    let mut world = scripted_world("AbiConsoleCommand", "{}");
    // SAFETY: the test module and its SDK descriptor table are process-lifetime allocations.
    let library =
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(v4_sdk_module())) }.unwrap();
    let owner = world.handle("camera").unwrap();
    let mut host = load_host(&world, &library);
    host.start(&mut world, ScriptTime::default());
    assert!(
        host.commands()
            .iter()
            .any(|command| command.name == "abi.echo")
    );

    let command = engine_console::parse_command_line("abi.echo v4").unwrap();
    host.execute_command(&mut world, &command).unwrap();
    assert_eq!(world.local_transform(owner).unwrap().position[0], 1.0);
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message == "echo:v4")
    );
}

#[test]
fn abi_console_command_panic_rolls_back_and_removes_owner_commands() {
    let _lock = test_lock();
    let mut world = scripted_world("AbiConsoleCommand", "{}");
    let owner = world.handle("camera").unwrap();
    let mut host = load_host(&world, &library());
    host.start(&mut world, ScriptTime::default());
    let command = engine_console::parse_command_line("abi.echo panic").unwrap();

    assert!(host.execute_command(&mut world, &command).is_err());
    assert_eq!(world.local_transform(owner).unwrap().position[0], 0.0);
    assert_eq!(host.commands_generation(), 2);
    assert!(
        !host
            .commands()
            .iter()
            .any(|command| command.name == "abi.echo")
    );
    assert!(host.execute_command(&mut world, &command).is_err());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event.message.contains("ABI console command panic")
    }));
}

#[test]
fn abi_console_commands_are_removed_when_an_instance_is_disabled() {
    let _lock = test_lock();
    let mut world = scripted_world("AbiConsoleCommand", "{ panic_update = true }");
    let mut host = load_host(&world, &library());
    host.start(&mut world, ScriptTime::default());
    assert_eq!(host.commands_generation(), 1);

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert_eq!(host.commands_generation(), 2);
    assert!(
        !host
            .commands()
            .iter()
            .any(|command| command.name == "abi.echo")
    );
}

#[test]
fn abi_camera_sdk_reads_writes_settings_and_switches_the_active_camera() {
    let _lock = test_lock();
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects.heavy_scripts]]
name = "CameraControl"
[[objects]]
id = "second"
[objects.camera]
"#,
    );
    let camera = world.handle("camera").unwrap();
    let second = world.handle("second").unwrap();
    let mut host = load_host(&world, &library);
    host.update(&mut world, 0.1, ScriptTime::default());

    assert_eq!(world.active_camera(), second);
    assert_eq!(
        world.camera_settings(camera).unwrap(),
        &engine_scene::CameraSettings {
            projection: engine_scene::ProjectionSettings {
                fov_deg: 75.0,
                ..Default::default()
            },
            render: engine_scene::CameraRenderSettings {
                render_scale: 0.5,
                clear_color: [0.2, 0.3, 0.4],
                anti_aliasing: engine_scene::AntiAliasing::None,
            },
            color: engine_scene::CameraColorSettings {
                exposure_ev: 2.0,
                tonemapper: engine_scene::Tonemapper::Neutral,
            },
        }
    );
    assert!(host.drain_events().is_empty());
}

#[test]
fn abi_camera_settings_and_active_camera_writes_roll_back_on_guest_panic() {
    let _lock = test_lock();
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects.heavy_scripts]]
name = "CameraControl"
params = { panic_update = true }
[[objects]]
id = "second"
[objects.camera]
"#,
    );
    let camera = world.handle("camera").unwrap();
    let mut host = load_host(&world, &library);
    host.update(&mut world, 0.1, ScriptTime::default());

    assert_eq!(world.active_camera(), camera);
    assert_eq!(
        world.camera_settings(camera).unwrap(),
        &engine_scene::CameraSettings::default()
    );
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message.contains("camera update panic"))
    );
}

#[test]
fn matching_version_rejects_a_null_nonempty_script_table() {
    static MODULE: RveModule = RveModule {
        header: RveHeader {
            abi_version: RVE_SCRIPT_ABI_VERSION,
            header_size: std::mem::size_of::<RveHeader>() as u32,
        },
        scripts: std::ptr::null(),
        script_count: 1,
    };
    // SAFETY: the module header is valid, and the null table is explicitly rejected.
    assert!(matches!(
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(&MODULE)) },
        Err(crate::ScriptLibraryError::NullScripts)
    ));
}

#[test]
fn matching_version_rejects_an_invalid_header_size_before_reading_the_table() {
    static MODULE: RveModule = RveModule {
        header: RveHeader {
            abi_version: RVE_SCRIPT_ABI_VERSION,
            header_size: 0,
        },
        scripts: std::ptr::null(),
        script_count: usize::MAX,
    };
    // SAFETY: the header is valid; an incorrect size is rejected before table fields are read.
    assert!(matches!(
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(&MODULE)) },
        Err(crate::ScriptLibraryError::InvalidHeaderSize {
            expected: size,
            found: 0,
        }) if size == std::mem::size_of::<RveHeader>()
    ));
}

#[test]
fn abi_guest_reads_the_script_input_snapshot_from_a_module() {
    let _lock = test_lock();
    let mut world = scripted_world("InputProbe", "{}");
    let library = library();
    let mut host = load_host(&world, &library);
    let mut input = crate::ScriptInput::default();
    input.set_key(Key::W, true);
    input.set_key(Key::A, true);
    input.set_key(Key::A, false);
    input.set_mouse_button(MouseButton::Left, true);
    input.set_mouse_button(MouseButton::Right, true);
    input.set_mouse_button(MouseButton::Right, false);
    input.set_cursor(40.0, 60.0);
    input.add_mouse_delta(2.5, -3.0);
    input.add_wheel(1.0, 0.5);
    assert_eq!(input.key_state(u32::MAX), 0);
    host.set_input(input);
    host.update(
        &mut world,
        1.0 / 60.0,
        ScriptTime {
            elapsed: 1.0,
            frame: 1,
        },
    );

    assert_eq!(message_list(&mut host), ["ABI input snapshot"]);
}

#[test]
fn abi_v5_input_actions_read_snapshots_and_stage_mutations() {
    let _lock = test_lock();
    let mut world = scripted_world("InputActionsProbe", "{}");
    let library = library();
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &empty_sources(),
        Some(&library),
        crate::ScriptLoadOptions {
            input: input_action_settings(),
            ..crate::ScriptLoadOptions::default()
        },
    )
    .unwrap();
    let mut input = crate::ScriptInput::default();
    input.set_key(Key::Space, true);
    input.set_key(Key::W, true);
    input.set_key(Key::D, true);
    input.set_mouse_button(MouseButton::Left, true);
    input.add_mouse_delta(2.0, -1.0);
    host.set_input(input.take_frame());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert_eq!(message_list(&mut host), ["ABI v5 action snapshot"]);

    host.set_input(input.take_frame());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert_eq!(message_list(&mut host), ["ABI v5 action mutations"]);

    host.set_input(input.take_frame());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert_eq!(message_list(&mut host), ["ABI v5 action reset"]);
}

fn invalid_name_module() -> &'static RveModule {
    rve_sdk::script_module! { "bad-name" => probe_factory }
}

fn duplicate_name_module() -> &'static RveModule {
    rve_sdk::script_module! {
        "Same" => probe_factory,
        "Same" => probe_factory,
    }
}

#[test]
fn library_validation_rejects_invalid_and_duplicate_names() {
    // SAFETY: both tables and all descriptor names are static SDK macro output.
    let invalid =
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(invalid_name_module())) };
    assert!(matches!(
        invalid,
        Err(crate::ScriptLibraryError::InvalidScriptName { .. })
    ));
    // SAFETY: the descriptor table and names are static SDK macro output.
    let duplicate =
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(duplicate_name_module())) };
    assert!(matches!(
        duplicate,
        Err(crate::ScriptLibraryError::DuplicateScriptName(name)) if name == "Same"
    ));

    // SAFETY: these test modules have valid static tables with intentionally malformed names.
    assert!(matches!(
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(null_name_module())) },
        Err(crate::ScriptLibraryError::NullScriptName { index: 0 })
    ));
    assert!(matches!(
        unsafe {
            crate::ScriptLibrary::from_module(std::ptr::from_ref(invalid_utf8_name_module()))
        },
        Err(crate::ScriptLibraryError::InvalidUtf8ScriptName { index: 0 })
    ));
}

unsafe extern "C" fn null_create(
    _: *const RveHost,
    _: *const RveParams,
    out: *mut *mut c_void,
) -> RveStatus {
    if !out.is_null() {
        // SAFETY: caller supplies a live output slot.
        unsafe { *out = std::ptr::null_mut() };
    }
    RveStatus::Ok
}

unsafe extern "C" fn no_op(_: *mut c_void, _: *const RveHost) -> RveStatus {
    RveStatus::Ok
}

unsafe extern "C" fn no_op_update(_: *mut c_void, _: *const RveHost, _: f32) -> RveStatus {
    RveStatus::Ok
}

unsafe extern "C" fn no_op_message(
    _: *mut c_void,
    _: *const RveHost,
    _: RveStr,
    _: RveStr,
    _: *const rve_sdk::abi::RveValue,
) -> RveStatus {
    RveStatus::Ok
}

unsafe extern "C" fn count_create_message(
    _: *mut c_void,
    _: *const RveHost,
    _: RveStr,
    _: RveStr,
    _: *const RveValue,
) -> RveStatus {
    CREATE_MESSAGE_DELIVERIES.fetch_add(1, Ordering::SeqCst);
    RveStatus::Ok
}

unsafe extern "C" fn no_op_drop(_: *mut c_void) -> RveStatus {
    RveStatus::Ok
}

fn null_module() -> &'static RveModule {
    static SCRIPTS: [RveScriptDesc; 1] = [RveScriptDesc {
        name: RveStr::from_static("Null"),
        create: null_create,
        start: no_op,
        update: no_op_update,
        message: no_op_message,
        destroy: no_op,
        drop: no_op_drop,
        params: std::ptr::null(),
        param_count: 0,
        command: None,
        collision: None,
    }];
    static MODULE: RveModule = RveModule::new(&SCRIPTS);
    &MODULE
}

fn null_name_module() -> &'static RveModule {
    static SCRIPTS: [RveScriptDesc; 1] = [RveScriptDesc {
        name: RveStr {
            ptr: std::ptr::null(),
            len: 1,
        },
        create: null_create,
        start: no_op,
        update: no_op_update,
        message: no_op_message,
        destroy: no_op,
        drop: no_op_drop,
        params: std::ptr::null(),
        param_count: 0,
        command: None,
        collision: None,
    }];
    static MODULE: RveModule = RveModule::new(&SCRIPTS);
    &MODULE
}

fn invalid_utf8_name_module() -> &'static RveModule {
    static NAME: [u8; 1] = [0xff];
    static SCRIPTS: [RveScriptDesc; 1] = [RveScriptDesc {
        name: RveStr {
            ptr: NAME.as_ptr(),
            len: NAME.len(),
        },
        create: null_create,
        start: no_op,
        update: no_op_update,
        message: no_op_message,
        destroy: no_op,
        drop: no_op_drop,
        params: std::ptr::null(),
        param_count: 0,
        command: None,
        collision: None,
    }];
    static MODULE: RveModule = RveModule::new(&SCRIPTS);
    &MODULE
}

#[test]
fn create_error_panic_and_null_are_heavy_load_errors() {
    let library = library();
    for (name, expected) in [
        ("Error", "factory rejected parameters"),
        ("Panic", "factory panic"),
    ] {
        let world = scripted_world(name, "{}");
        let error = load(&world, &library).err().unwrap();
        assert!(matches!(
            error,
            ScriptLoadError::Heavy { message, .. } if message.contains(expected)
        ));
    }

    // SAFETY: this manual table contains process-lifetime static data and callbacks.
    let null_library =
        unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(null_module())) }.unwrap();
    let world = scripted_world("Null", "{}");
    assert!(matches!(
        load(&world, &null_library),
        Err(ScriptLoadError::Heavy { message, .. })
            if message.contains("null instance")
    ));
}

#[test]
fn partial_load_failure_drops_instances_already_created() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let world = world(
        r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects.heavy_scripts]]
name = "Probe"
params = { label = "first" }
[[objects]]
id = "second"
[[objects.heavy_scripts]]
name = "Error"
"#,
    );
    assert!(load(&world, &library).is_err());
    assert_eq!(PROBE_DROPS.load(Ordering::SeqCst), 1);
}

#[test]
fn transform_time_params_and_log_cross_the_abi() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let mut world = scripted_world("Probe", r#"{ label = "from-params" }"#);
    let handle = world.handle("camera").unwrap();
    let mut host = load_host(&world, &library);
    host.start(
        &mut world,
        ScriptTime {
            elapsed: 3.5,
            frame: 17,
        },
    );
    assert_eq!(world.local_transform(handle).unwrap().position.x, 10.0);
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, "start:from-params");
    assert_eq!(events[0].source, "scene/camera/Probe");
    drop(host);
    assert_eq!(PROBE_DROPS.load(Ordering::SeqCst), 1);
}

#[test]
fn update_panic_disables_only_its_instance_and_drop_runs_after_destroy_panic() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects.heavy_scripts]]
name = "Probe"
params = { label = "bad", panic_update = true }
[[objects]]
id = "second"
[[objects.heavy_scripts]]
name = "Probe"
params = { label = "good" }
"#,
    );
    let first = world.handle("camera").unwrap();
    let second = world.handle("second").unwrap();
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 3.5,
        frame: 17,
    };
    host.start(&mut world, time);
    host.drain_events();
    host.update(&mut world, 0.5, time);
    assert_eq!(world.local_transform(first).unwrap().position.x, 0.0);
    assert_eq!(world.local_transform(second).unwrap().position.x, 0.5);
    let events = message_list(&mut host);
    assert!(
        events
            .iter()
            .any(|message| message.contains("update panic"))
    );
    assert!(events.iter().any(|message| message == "update:good"));
    host.update(&mut world, 0.5, time);
    assert_eq!(world.local_transform(first).unwrap().position.x, 0.0);
    assert_eq!(world.local_transform(second).unwrap().position.x, 1.0);
    drop(host);
    assert_eq!(PROBE_DROPS.load(Ordering::SeqCst), 2);

    PROBE_DROPS.store(0, Ordering::SeqCst);
    let mut world = scripted_world("Probe", r#"{ label = "destroy", panic_destroy = true }"#);
    let mut host = load_host(&world, &library);
    host.start(&mut world, time);
    host.destroy(&mut world, time);
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message.contains("destroy panic"))
    );
    drop(host);
    assert_eq!(PROBE_DROPS.load(Ordering::SeqCst), 1);
}

#[test]
fn panic_in_host_function_poisoned_call_rolls_back_transform() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
[[objects.heavy_scripts]]
name = "Probe"
params = { label = "poison", panic_host_log = true, mutate_other = true }
[[objects]]
id = "target"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "target" }
"#,
    );
    let handle = world.handle("camera").unwrap();
    let target = world.handle("target").unwrap();
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 3.5,
        frame: 17,
    };
    host.start(&mut world, time);
    host.drain_events();
    let before = world.local_transform(handle).unwrap().position;
    let target_before = world.local_transform(target).unwrap().position;
    host.update(&mut world, 0.5, time);
    assert_eq!(world.local_transform(handle).unwrap().position, before);
    assert_eq!(
        world.local_transform(target).unwrap().position,
        target_before
    );
    host.update(&mut world, 0.5, time);
    let events = message_list(&mut host);
    assert!(events.iter().any(|message| message.contains("poisoned")));
    assert!(events.iter().all(|message| !message.contains("rollback")));
    drop(host);
}

#[test]
fn panic_rolls_back_own_and_cross_object_transform_writes() {
    let _lock = test_lock();
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
[[objects.heavy_scripts]]
name = "Probe"
params = { label = "bad", panic_update = true, mutate_other = true }
[[objects]]
id = "target"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "target" }
"#,
    );
    let camera = world.handle("camera").unwrap();
    let target = world.handle("target").unwrap();
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 3.5,
        frame: 17,
    };
    host.start(&mut world, time);
    host.drain_events();
    host.update(&mut world, 0.5, time);

    assert_eq!(
        world.local_transform(camera).unwrap().position.to_array(),
        [0.0; 3]
    );
    assert_eq!(
        world.local_transform(target).unwrap().position.to_array(),
        [0.0; 3]
    );
    host.update(&mut world, 0.5, time);
    let events = message_list(&mut host);
    assert!(
        events
            .iter()
            .any(|message| message.contains("update panic"))
    );
    assert!(events.iter().all(|message| !message.contains("rollback")));
}

#[test]
fn non_finite_abi_transform_is_rolled_back_and_disables_the_instance() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "camera"
[[objects.heavy_scripts]]
name = "Probe"
params = { label = "nan", nan_update = true, mutate_other = true }
[[objects]]
id = "target"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "target" }
"#,
    );
    let handle = world.handle("camera").unwrap();
    let target = world.handle("target").unwrap();
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 3.5,
        frame: 17,
    };
    host.start(&mut world, time);
    host.drain_events();
    host.update(&mut world, 0.5, time);
    assert_eq!(world.local_transform(handle).unwrap().position.x, 0.0);
    assert_eq!(world.local_transform(target).unwrap().position.x, 0.0);
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message.contains("non-finite transform"))
    );
    host.update(&mut world, 0.5, time);
    assert!(
        message_list(&mut host)
            .iter()
            .all(|message| !message.contains("rollback"))
    );
    assert!(host.drain_events().is_empty());
    assert_eq!(world.local_transform(handle).unwrap().position.x, 0.0);
    drop(host);
    assert_eq!(PROBE_DROPS.load(Ordering::SeqCst), 1);
}

static POISON_CREATE_DROPS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn set_local_during_create(
    host: *const RveHost,
    _: *const RveParams,
    out: *mut *mut c_void,
) -> RveStatus {
    if host.is_null() || out.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the host and output pointer are supplied for the duration of this callback.
    let host = unsafe { &*host };
    let transform = RveTransform {
        position: [99.0, 0.0, 0.0],
        ..RveTransform::default()
    };
    // SAFETY: both pointers are valid for this create callback.
    if unsafe { (host.set_local)(host.ctx, host.self_object, &transform) } {
        return RveStatus::Error;
    }
    let message = RveValue::boolean(true);
    // SAFETY: the message strings and value live through the host callbacks.
    if unsafe {
        (host.send)(
            host.ctx,
            RveStr::from_static("camera"),
            RveStr::from_static("early"),
            &message,
        ) || (host.broadcast)(host.ctx, RveStr::from_static("early"), &message)
    } {
        return RveStatus::Error;
    }
    // SAFETY: the non-null allocation is released by `free_byte` after successful creation.
    unsafe { *out = Box::into_raw(Box::new(0_u8)).cast() };
    RveStatus::Ok
}

unsafe extern "C" fn poisoned_create(
    host: *const RveHost,
    _: *const RveParams,
    out: *mut *mut c_void,
) -> RveStatus {
    if host.is_null() || out.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the host and output pointer are supplied for the duration of this callback.
    let host = unsafe { &*host };
    // SAFETY: the ABI host log callback is valid for this create call.
    unsafe {
        (host.log)(
            host.ctx,
            RVE_LOG_INFO,
            RveStr::from_str_ref("__rve_test_host_log_panic__"),
        )
    };
    // SAFETY: the non-null allocation is released by `poisoned_drop` after the host is poisoned.
    unsafe { *out = Box::into_raw(Box::new(0_u8)).cast() };
    RveStatus::Ok
}

unsafe extern "C" fn free_byte(instance: *mut c_void) -> RveStatus {
    if instance.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the instance was allocated as a Box<u8> by the corresponding test create callback.
    drop(unsafe { Box::from_raw(instance.cast::<u8>()) });
    RveStatus::Ok
}

unsafe extern "C" fn create_byte_instance(
    _: *const RveHost,
    _: *const RveParams,
    out: *mut *mut c_void,
) -> RveStatus {
    if out.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the caller supplies a live output slot for this create callback.
    unsafe { *out = Box::into_raw(Box::new(0_u8)).cast() };
    RveStatus::Ok
}

unsafe extern "C" fn broadcast_then_error(
    _: *mut c_void,
    host: *const RveHost,
    _: f32,
) -> RveStatus {
    if host.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the host and message are valid for this callback.
    let host = unsafe { &*host };
    let message = RveValue::boolean(true);
    // SAFETY: the strings and value live through this host call.
    if !unsafe { (host.broadcast)(host.ctx, RveStr::from_static("failed"), &message) } {
        return RveStatus::Error;
    }
    // SAFETY: the host callback and static error string are valid for this call.
    unsafe { (host.set_error)(host.ctx, RveStr::from_static("injected callback error")) };
    RveStatus::Error
}

unsafe extern "C" fn count_error_message(
    _: *mut c_void,
    _: *const RveHost,
    _: RveStr,
    _: RveStr,
    _: *const RveValue,
) -> RveStatus {
    ERROR_MESSAGE_DELIVERIES.fetch_add(1, Ordering::SeqCst);
    RveStatus::Ok
}

fn error_message_module() -> &'static RveModule {
    static SCRIPTS: [RveScriptDesc; 2] = [
        RveScriptDesc {
            name: RveStr::from_static("ErrorSender"),
            create: create_byte_instance,
            start: no_op,
            update: broadcast_then_error,
            message: no_op_message,
            destroy: no_op,
            drop: free_byte,
            params: std::ptr::null(),
            param_count: 0,
            command: None,
            collision: None,
        },
        RveScriptDesc {
            name: RveStr::from_static("ErrorObserver"),
            create: create_byte_instance,
            start: no_op,
            update: no_op_update,
            message: count_error_message,
            destroy: no_op,
            drop: free_byte,
            params: std::ptr::null(),
            param_count: 0,
            command: None,
            collision: None,
        },
    ];
    static MODULE: RveModule = RveModule::new(&SCRIPTS);
    &MODULE
}

unsafe extern "C" fn poisoned_drop(instance: *mut c_void) -> RveStatus {
    if instance.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the instance was allocated as a Box<u8> by `poisoned_create`.
    drop(unsafe { Box::from_raw(instance.cast::<u8>()) });
    POISON_CREATE_DROPS.fetch_add(1, Ordering::SeqCst);
    RveStatus::Panicked
}

fn set_local_during_create_module() -> &'static RveModule {
    static SCRIPTS: [RveScriptDesc; 1] = [RveScriptDesc {
        name: RveStr::from_static("CreateMutation"),
        create: set_local_during_create,
        start: no_op,
        update: no_op_update,
        message: count_create_message,
        destroy: no_op,
        drop: free_byte,
        params: std::ptr::null(),
        param_count: 0,
        command: None,
        collision: None,
    }];
    static MODULE: RveModule = RveModule::new(&SCRIPTS);
    &MODULE
}

fn poisoned_create_module() -> &'static RveModule {
    static SCRIPTS: [RveScriptDesc; 1] = [RveScriptDesc {
        name: RveStr::from_static("PoisonCreate"),
        create: poisoned_create,
        start: no_op,
        update: no_op_update,
        message: no_op_message,
        destroy: no_op,
        drop: poisoned_drop,
        params: std::ptr::null(),
        param_count: 0,
        command: None,
        collision: None,
    }];
    static MODULE: RveModule = RveModule::new(&SCRIPTS);
    &MODULE
}

#[test]
fn create_rejects_writes_and_messages_without_mutating_or_queueing_them() {
    CREATE_MESSAGE_DELIVERIES.store(0, Ordering::SeqCst);
    let module = set_local_during_create_module();
    // SAFETY: this test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let mut world = scripted_world("CreateMutation", "{}");
    let handle = world.handle("camera").unwrap();
    let before = world.local_transform(handle).unwrap().position;
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 0.0,
        frame: 1,
    };
    host.start(&mut world, time);
    host.update(&mut world, 0.1, time);
    assert_eq!(world.local_transform(handle).unwrap().position, before);
    assert_eq!(CREATE_MESSAGE_DELIVERIES.load(Ordering::SeqCst), 0);
    drop(host);
    assert_eq!(world.local_transform(handle).unwrap().position, before);
}

#[test]
fn rust_field_transform_overrides_own_handle_write_only_when_changed() {
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "field-wins"
name = "field-wins"
[objects.transform]
position = [1.0, 0.0, 0.0]
[[objects.heavy_scripts]]
name = "DoubleWrite"
params = { change_field = true }
[[objects]]
id = "handle-wins"
name = "handle-wins"
[objects.transform]
position = [2.0, 0.0, 0.0]
[[objects.heavy_scripts]]
name = "DoubleWrite"
params = { change_field = false }
"#,
    );
    let field_wins = world.handle("field-wins").unwrap();
    let handle_wins = world.handle("handle-wins").unwrap();
    let mut host = load_host(&world, &library);
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );

    assert_eq!(
        world
            .local_transform(field_wins)
            .unwrap()
            .position
            .to_array(),
        [1.0, 5.0, 0.0]
    );
    assert_eq!(
        world
            .local_transform(handle_wins)
            .unwrap()
            .position
            .to_array(),
        [9.0, 0.0, 0.0]
    );
    assert!(host.drain_events().is_empty());
}

#[test]
fn abi_messages_from_start_and_on_message_use_next_update_in_order() {
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "sender"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "receiver" }
[[objects]]
id = "observer"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "observer" }
"#,
    );
    let mut host = load_host(&world, &library);

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert_eq!(
        message_list(&mut host),
        [
            "receiver:ping:sender",
            "sender:fan:sender",
            "receiver:fan:sender",
            "observer:fan:sender",
            "sender:update",
            "receiver:update",
            "observer:update",
        ]
    );
    assert_eq!(
        world
            .local_transform(world.handle("receiver").unwrap())
            .unwrap()
            .position
            .z,
        1.0
    );

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert_eq!(
        message_list(&mut host),
        [
            "sender:reply:receiver",
            "sender:update",
            "receiver:update",
            "observer:update",
        ]
    );
}

#[test]
fn abi_send_and_broadcast_reject_message_limits_before_staging() {
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "sender"
[[objects.heavy_scripts]]
name = "Limits"
[[objects]]
id = "receiver"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "receiver" }
"#,
    );
    let mut host = load_host(&world, &library);
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.message == "message limits rejected")
    );
    assert!(
        events
            .iter()
            .all(|event| event.level != crate::ScriptEventLevel::Error)
    );
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains(":invalid:"))
    );
}

#[test]
fn abi_message_values_accept_exact_limits_and_reach_the_receiver() {
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "sender"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "boundary_sender" }
[[objects]]
id = "receiver"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "receiver" }
"#,
    );
    let mut host = load_host(&world, &library);
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );

    let events = host.drain_events();
    let max_name = "n".repeat(MAX_MESSAGE_NAME_BYTES);
    assert!(
        events
            .iter()
            .all(|event| event.level != crate::ScriptEventLevel::Error)
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "receiver:depth:sender")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "receiver:elements:sender")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "receiver:strings:sender")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == format!("receiver:{max_name}:sender"))
    );
}

#[test]
fn abi_message_queue_limit_is_shared_and_drops_excess_messages() {
    let _lock = test_lock();
    BULK_MESSAGE_DELIVERIES.store(0, Ordering::SeqCst);
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "sender"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "bulk_sender" }
[[objects]]
id = "receiver"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "receiver" }
"#,
    );
    let mut host = load_host(&world, &library);
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );

    let events = host.drain_events();
    assert_eq!(
        BULK_MESSAGE_DELIVERIES.load(Ordering::SeqCst),
        MAX_MESSAGES_PER_FRAME
    );
    assert!(events.iter().any(|event| {
        event.level == crate::ScriptEventLevel::Error
            && event.message.contains("dropped 4 message(s)")
    }));
    assert!(events.iter().any(|event| {
        event.level == crate::ScriptEventLevel::Warn
            && event.message == "3074 script log messages dropped"
    }));
}

#[test]
fn abi_broadcast_skips_disabled_instances() {
    let library = library();
    let mut world = world(
        r#"
[[objects]]
id = "disabled"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "disabled" }
[[objects]]
id = "sender"
[[objects.heavy_scripts]]
name = "Message"
params = { role = "broadcast_sender" }
"#,
    );
    let mut host = load_host(&world, &library);
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    host.drain_events();
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );

    let events = message_list(&mut host);
    assert!(
        events
            .iter()
            .any(|message| message == "broadcast_sender:enabled_only:sender")
    );
    assert!(
        events
            .iter()
            .all(|message| message != "disabled:enabled_only:sender")
    );
}

#[test]
fn abi_error_status_rolls_back_messages_without_panicking() {
    ERROR_MESSAGE_DELIVERIES.store(0, Ordering::SeqCst);
    let module = error_message_module();
    // SAFETY: this test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let mut world = world(
        r#"
[[objects]]
id = "sender"
[[objects.heavy_scripts]]
name = "ErrorSender"
[[objects]]
id = "observer"
[[objects.heavy_scripts]]
name = "ErrorObserver"
"#,
    );
    let mut host = load_host(&world, &library);
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == crate::ScriptEventLevel::Error && event.message == "injected callback error"
    }));
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains("panicked"))
    );

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert_eq!(ERROR_MESSAGE_DELIVERIES.load(Ordering::SeqCst), 0);
}

#[test]
fn poisoned_create_drops_its_non_null_instance_once() {
    POISON_CREATE_DROPS.store(0, Ordering::SeqCst);
    let module = poisoned_create_module();
    // SAFETY: this test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let world = scripted_world("PoisonCreate", "{}");
    let result = load(&world, &library);
    assert!(matches!(
        result,
        Err(ScriptLoadError::Heavy { message, .. }) if message.contains("panicked during create")
    ));
    assert_eq!(POISON_CREATE_DROPS.load(Ordering::SeqCst), 1);
}

#[test]
fn panic_in_guest_drop_emits_an_error_event() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let world = scripted_world("Probe", r#"{ label = "drop", panic_drop = true }"#);
    let host = load_host(&world, &library);
    let events = host.events.clone();
    drop(host);
    assert!(events.borrow().iter().any(|event| {
        event.level == crate::ScriptEventLevel::Error && event.message == "ABI script drop panicked"
    }));
    assert_eq!(PROBE_DROPS.load(Ordering::SeqCst), 1);
}

struct EchoParams(Params);

impl Script for EchoParams {
    fn on_start(&mut self, ctx: &mut rve_sdk::Context<'_>) {
        let mut entries: Vec<_> = self
            .0
            .iter()
            .map(|(key, value)| format!("{key}={value:?}"))
            .collect();
        entries.sort();
        ctx.log_info(entries.join(";"));
    }
}

fn echo_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(EchoParams(params.clone())))
}

fn typed_module() -> &'static RveModule {
    rve_sdk::script_module! {
        "Typed" => echo_factory {
            speed: f32 = 1.5 (0.0..=10.0) "Speed",
            enabled: bool = true,
        },
        "Untyped" => echo_factory,
    }
}

fn typed_library() -> crate::ScriptLibrary {
    // SAFETY: the SDK macro returns an immutable process-lifetime table.
    unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(typed_module())) }.unwrap()
}

#[test]
fn library_exposes_script_names_and_declared_schemas() {
    let library = typed_library();
    assert_eq!(library.script_names(), ["Typed", "Untyped"]);
    let schema = library.schema("Typed").unwrap();
    let names: Vec<_> = schema
        .specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["speed", "enabled"]);
    let speed = schema.get("speed").unwrap();
    assert_eq!(speed.kind, engine_scene::ParamKind::Float);
    assert_eq!((speed.min, speed.max), (Some(0.0), Some(10.0)));
    assert_eq!(speed.label.as_deref(), Some("Speed"));
    assert!(library.schema("Untyped").is_none());
    assert!(library.schema("Missing").is_none());
}

#[test]
fn heavy_defaults_are_filled_and_scene_values_coerced_before_create() {
    let _lock = test_lock();
    let library = typed_library();
    let mut world = scripted_world("Typed", "{ speed = 3 }");
    let mut host = load_host(&world, &library);
    host.start(&mut world, ScriptTime::default());
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message == "enabled=Bool(true);speed=Float(3.0)")
    );

    let mut world = scripted_world("Untyped", "{ anything = 3 }");
    let mut host = load_host(&world, &library);
    host.start(&mut world, ScriptTime::default());
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message == "anything=Int(3)")
    );
}

#[test]
fn heavy_params_that_violate_the_schema_fail_loading() {
    let library = typed_library();
    for params in ["{ speed = 11.0 }", "{ enabled = 1 }", "{ unknown = 1 }"] {
        let world = scripted_world("Typed", params);
        let error = load(&world, &library).err().unwrap();
        assert!(
            matches!(&error, ScriptLoadError::Heavy { object_id, name, .. }
                if object_id == "camera" && name == "Typed"),
            "{params}: {error}"
        );
    }
}

fn module_with_params(params: &'static [rve_sdk::abi::RveParamDesc]) -> RveModule {
    // The leaked descriptor table lives for the test process, as the ABI requires.
    let scripts: &'static [RveScriptDesc] = Box::leak(Box::new([RveScriptDesc {
        name: RveStr::from_static("Broken"),
        create: null_create,
        start: no_op,
        update: no_op_update,
        message: no_op_message,
        destroy: no_op,
        drop: no_op_drop,
        params: params.as_ptr(),
        param_count: params.len(),
        command: None,
        collision: None,
    }]));
    RveModule::new(scripts)
}

#[test]
fn malformed_parameter_tables_are_rejected() {
    use rve_sdk::abi::{
        RVE_PARAM_BOOL, RVE_PARAM_FLOAT, RVE_PARAM_HAS_MIN, RveParamDesc, RveValue,
    };
    fn desc(name: RveStr, kind: u32, default: RveValue, flags: u32) -> RveParamDesc {
        RveParamDesc {
            name,
            kind,
            default,
            min: 5.0,
            max: 0.0,
            flags,
            label: RveStr::empty(),
        }
    }
    static BAD_UTF8: [u8; 1] = [0xff];
    let bad_utf8 = RveStr {
        ptr: BAD_UTF8.as_ptr(),
        len: 1,
    };
    let speed = RveStr::from_static("speed");
    let cases: Vec<Vec<RveParamDesc>> = vec![
        vec![desc(speed, RVE_PARAM_FLOAT, RveValue::boolean(true), 0)],
        vec![
            desc(speed, RVE_PARAM_FLOAT, RveValue::float(1.0), 0),
            desc(speed, RVE_PARAM_FLOAT, RveValue::float(2.0), 0),
        ],
        vec![desc(bad_utf8, RVE_PARAM_BOOL, RveValue::boolean(true), 0)],
        vec![desc(speed, 99, RveValue::float(1.0), 0)],
        vec![desc(speed, RVE_PARAM_FLOAT, RveValue::float(1.0), 0x80)],
        vec![desc(
            speed,
            RVE_PARAM_FLOAT,
            RveValue::float(1.0),
            RVE_PARAM_HAS_MIN,
        )],
        vec![desc(
            RveStr::from_static("1bad"),
            RVE_PARAM_FLOAT,
            RveValue::float(1.0),
            0,
        )],
    ];
    for case in cases {
        let params: &'static [RveParamDesc] = Box::leak(case.into_boxed_slice());
        let module: &'static RveModule = Box::leak(Box::new(module_with_params(params)));
        // SAFETY: the leaked tables live for the process and their strings are static.
        let result = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) };
        assert!(
            matches!(
                &result,
                Err(crate::ScriptLibraryError::InvalidParams { script, .. }) if script == "Broken"
            ),
            "{:?}",
            result.err()
        );
    }

    let null_table: &'static RveModule = Box::leak(Box::new({
        let mut module = module_with_params(&[]);
        let scripts: &'static mut [RveScriptDesc] = Box::leak(Box::new([RveScriptDesc {
            name: RveStr::from_static("Broken"),
            create: null_create,
            start: no_op,
            update: no_op_update,
            message: no_op_message,
            destroy: no_op,
            drop: no_op_drop,
            params: std::ptr::null(),
            param_count: 1,
            command: None,
            collision: None,
        }]));
        module.scripts = scripts.as_ptr();
        module
    }));
    // SAFETY: the null table with a nonzero count is rejected before it is read.
    let result = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(null_table)) };
    assert!(matches!(
        result,
        Err(crate::ScriptLibraryError::InvalidParams { .. })
    ));
}

fn at(elapsed: f64, frame: u64) -> ScriptTime {
    ScriptTime { elapsed, frame }
}

fn info_messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Info)
        .map(|event| event.message)
        .collect()
}

fn error_messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Error)
        .map(|event| event.message)
        .collect()
}

const BOLT_PREFAB: &str =
    "[[objects]]\nid = \"root\"\n[[objects.heavy_scripts]]\nname = \"BoltProbe\"\n";
/// A prefab with no heavy script, for tests whose guest module does not define the bolt script.
const RAW_PREFAB: &str = "[[objects]]\nid = \"root\"\n";
const SPAWNER_SCENE: &str =
    "[[objects]]\nid = \"spawner\"\n[[objects.heavy_scripts]]\nname = \"SpawnDriver\"\n";

/// Loads a scene with the bolt prefab registered as a fast prefab from `template_text` and
/// `prefabs/heavy.rveprefab` in the project list.
fn spawn_host(world: &World, library: &crate::ScriptLibrary, template_text: &str) -> ScriptHost {
    let mut prefabs = PrefabCatalog::default();
    prefabs.insert(
        "prefabs/bolt.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    prefabs.insert(
        "prefabs/heavy.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Heavy,
            max_instances: None,
        },
    );
    let mut host = ScriptHost::load_with_options(
        world,
        "scene",
        &empty_sources(),
        Some(library),
        crate::ScriptLoadOptions {
            prefabs,
            ..crate::ScriptLoadOptions::default()
        },
    )
    .unwrap();
    let path = VirtualPath::parse("prefabs/bolt.rveprefab").unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(template_text.to_owned()) };
    let template = Arc::new(PrefabTemplate::load(&path, &mut read).unwrap());
    host.register_prefab(world, "prefabs/bolt.rveprefab", template, &empty_sources())
        .unwrap();
    host
}

#[test]
fn abi_spawn_is_pending_then_applied_and_starts_before_messages() {
    let _lock = test_lock();
    let mut world = world(SPAWNER_SCENE);
    let mut host = spawn_host(&world, &library(), BOLT_PREFAB);

    host.update(&mut world, 0.1, at(0.1, 1));
    assert!(
        world.handle("bolt").is_some(),
        "applied at the end of the phase"
    );
    assert_eq!(
        info_messages(&mut host),
        ["spawned=true", "pending=true alive=false", "sent=true"],
        "the pending handle reads as pending and the pending id accepts the message"
    );

    host.update(&mut world, 0.1, at(0.2, 2));
    assert_eq!(
        info_messages(&mut host),
        [
            "start:bolt",
            "hit:ping",
            "alive=true pending=false",
            "destroy=true"
        ],
        "on_start runs before the queued message; the spawned object is alive"
    );
}

#[test]
fn abi_destroy_with_delay_keeps_the_object_until_due_and_dead_handles_warn_once() {
    let _lock = test_lock();
    let mut world = world(SPAWNER_SCENE);
    let mut host = spawn_host(&world, &library(), BOLT_PREFAB);
    host.update(&mut world, 0.1, at(0.1, 1));
    host.update(&mut world, 0.1, at(0.2, 2));
    assert!(info_messages(&mut host).contains(&"destroy=true".to_owned()));
    let _ = host.drain_world_changes();

    // The delay is 1.0 seconds from elapsed 0.2, so the object is still in the world at 0.5.
    host.update(&mut world, 0.1, at(0.5, 3));
    assert!(world.handle("bolt").is_some());
    assert_eq!(info_messages(&mut host), ["alive=true pending=false"]);

    // Removal happens at the end of the phase, so the driver still sees it alive in this update.
    host.update(&mut world, 0.1, at(1.3, 4));
    assert_eq!(
        info_messages(&mut host),
        ["alive=true pending=false", "bolt destroyed"]
    );
    assert!(world.handle("bolt").is_none());
    let changes = host.drain_world_changes();
    assert_eq!(changes.despawned.len(), 1);

    host.update(&mut world, 0.1, at(1.4, 5));
    let events = host.drain_events();
    let infos: Vec<_> = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Info)
        .map(|event| event.message.as_str())
        .collect();
    assert_eq!(
        infos,
        [
            "alive=false pending=false",
            "dead_destroy=false",
            "dead_destroy=false"
        ],
        "a dead handle reads as dead and destroy refuses it"
    );
    let warnings = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Warn)
        .count();
    assert_eq!(warnings, 1, "one warning per script and dead object");
}

#[test]
fn abi_prefab_api_reports_states_and_queues_heavy_loads() {
    let _lock = test_lock();
    let mut world = world(
        r#"
[[objects]]
id = "probe"
[[objects.heavy_scripts]]
name = "PrefabProbe"
"#,
    );
    let mut host = spawn_host(&world, &library(), BOLT_PREFAB);
    host.update(&mut world, 0.1, at(0.1, 1));

    let events = host.drain_events();
    let infos: Vec<_> = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Info)
        .map(|event| event.message.as_str())
        .collect();
    assert_eq!(
        infos,
        ["resident=Resident unknown=Unknown heavy=Unloaded load=true rejected=false"]
    );
    let errors: Vec<_> = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Error)
        .map(|event| event.message.clone())
        .collect();
    assert!(
        errors
            .iter()
            .any(|message| message.contains("prefabs/missing.rveprefab")),
        "{errors:?}"
    );
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Load("prefabs/heavy.rveprefab".to_owned())]
    );
}

unsafe fn spawn_options_from_host(host: *const RveHost, flags: u32, id: RveStr) -> u64 {
    // SAFETY: callers pass the live host of a running guest callback.
    let host = unsafe { &*host };
    let Some(spawn) = host.spawn else {
        return 0;
    };
    let options = RveSpawnOptions {
        flags,
        position: [0.0; 3],
        rotation_deg: [0.0; 3],
        scale: [1.0; 3],
        velocity: [0.0; 3],
        parent: 0,
        name: RveStr::empty(),
        id,
    };
    // SAFETY: the prefab path and options stay valid for this host call.
    unsafe {
        spawn(
            host.ctx,
            RveStr::from_static("prefabs/bolt.rveprefab"),
            &options,
        )
    }
}

/// Builds a one-script module whose callbacks are supplied by the test.
fn raw_module(create: RveCreateFn, update: RveUpdateFn) -> &'static RveModule {
    let scripts = Box::leak(Box::new([RveScriptDesc {
        name: RveStr::from_static("RawProbe"),
        create,
        start: no_op,
        update,
        message: no_op_message,
        destroy: no_op,
        drop: free_byte,
        params: std::ptr::null(),
        param_count: 0,
        command: None,
        collision: None,
    }]));
    Box::leak(Box::new(RveModule::new(scripts)))
}

unsafe extern "C" fn spawn_during_create(
    host: *const RveHost,
    _: *const RveParams,
    out: *mut *mut c_void,
) -> RveStatus {
    if host.is_null() || out.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the host is valid for this create callback.
    if unsafe { spawn_options_from_host(host, 0, RveStr::empty()) } != 0 {
        return RveStatus::Error;
    }
    // The refusal is reported as a script event; the instance itself is created normally.
    // SAFETY: the caller supplies a live output slot; `free_byte` releases the allocation.
    unsafe { *out = Box::into_raw(Box::new(0_u8)).cast() };
    RveStatus::Ok
}

#[test]
fn abi_spawn_during_create_is_refused_without_queueing() {
    let _lock = test_lock();
    let module = raw_module(spawn_during_create, no_op_update);
    // SAFETY: the test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let mut world =
        world("[[objects]]\nid = \"raw\"\n[[objects.heavy_scripts]]\nname = \"RawProbe\"\n");
    let mut host = spawn_host(&world, &library, RAW_PREFAB);
    assert!(
        error_messages(&mut host)
            .iter()
            .any(|message| message == "spawn is unavailable during script creation"),
    );
    host.update(&mut world, 0.1, at(0.1, 1));
    assert!(host.drain_world_changes().spawned.is_empty());
    assert!(world.handle("bolt").is_none());
}

unsafe extern "C" fn spawn_then_error(_: *mut c_void, host: *const RveHost, _: f32) -> RveStatus {
    if host.is_null() {
        return RveStatus::Error;
    }
    // SAFETY: the host is valid for this callback.
    let host_ref = unsafe { &*host };
    // SAFETY: the host is valid for this callback.
    let handle =
        unsafe { spawn_options_from_host(host, RVE_SPAWN_HAS_ID, RveStr::from_static("rolled")) };
    if handle == 0 {
        return RveStatus::Error;
    }
    // SAFETY: the host callback copies the static message during this call.
    unsafe { (host_ref.set_error)(host_ref.ctx, RveStr::from_static("injected spawn error")) };
    RveStatus::Error
}

#[test]
fn abi_spawn_is_rolled_back_when_the_callback_returns_error() {
    let _lock = test_lock();
    let module = raw_module(create_byte_instance, spawn_then_error);
    // SAFETY: the test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let mut world =
        world("[[objects]]\nid = \"raw\"\n[[objects.heavy_scripts]]\nname = \"RawProbe\"\n");
    let mut host = spawn_host(&world, &library, RAW_PREFAB);
    host.update(&mut world, 0.1, at(0.1, 1));

    let errors = error_messages(&mut host);
    assert!(
        errors
            .iter()
            .any(|message| message == "injected spawn error"),
        "{errors:?}"
    );
    assert!(world.handle("rolled").is_none());
    assert!(host.drain_world_changes().spawned.is_empty());
}

unsafe extern "C" fn spawn_invalid_options(
    _: *mut c_void,
    host: *const RveHost,
    _: f32,
) -> RveStatus {
    if host.is_null() {
        return RveStatus::Error;
    }
    // Unknown presence bit 7 and a parent handle that names no slot are both refused.
    // SAFETY: the host is valid for this callback.
    let unknown = unsafe { spawn_options_from_host(host, 1 << 7, RveStr::empty()) };
    // SAFETY: the host is valid for this callback.
    let no_parent = unsafe { spawn_options_from_host(host, RVE_SPAWN_HAS_PARENT, RveStr::empty()) };
    if unknown != 0 || no_parent != 0 {
        return RveStatus::Error;
    }
    RveStatus::Ok
}

#[test]
fn abi_spawn_refuses_unknown_option_flags_and_bad_parents() {
    let _lock = test_lock();
    let module = raw_module(create_byte_instance, spawn_invalid_options);
    // SAFETY: the test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let mut world =
        world("[[objects]]\nid = \"raw\"\n[[objects.heavy_scripts]]\nname = \"RawProbe\"\n");
    let mut host = spawn_host(&world, &library, RAW_PREFAB);
    host.update(&mut world, 0.1, at(0.1, 1));
    let errors = error_messages(&mut host);
    assert!(
        errors
            .iter()
            .any(|message| message == "spawn received unknown option flags 0x80"),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|message| message == "spawn parent is not an object handle"),
        "{errors:?}"
    );
    assert!(host.drain_world_changes().spawned.is_empty());
}
