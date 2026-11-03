use super::world;
use crate::ScriptTime;
use crate::host::{ScriptHost, ScriptLoadError};
use crate::transaction::{
    MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_STRING_BYTES,
    MAX_MESSAGES_PER_FRAME,
};
use crate::types::ScriptEventLevel;
use engine_scene::World;
use rve_sdk::abi::{
    RVE_LOG_INFO, RVE_SCRIPT_ABI_MIN_VERSION, RVE_SCRIPT_ABI_VERSION, RveHeader, RveHost,
    RveModule, RveParams, RveScriptDesc, RveScriptDescV1, RveScriptDescV3, RveStatus, RveStr,
    RveTransform, RveValue,
};
use rve_sdk::{
    AntiAliasing, CameraSettings, ConsoleLevel, Key, MouseButton, Params, Script, Tonemapper, Value,
};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static PROBE_DROPS: AtomicUsize = AtomicUsize::new(0);
static CREATE_MESSAGE_DELIVERIES: AtomicUsize = AtomicUsize::new(0);
static ERROR_MESSAGE_DELIVERIES: AtomicUsize = AtomicUsize::new(0);

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

fn sdk_module() -> &'static RveModule {
    rve_sdk::script_module! {
        "Probe" => probe_factory,
        "Error" => error_factory,
        "Panic" => panic_factory,
        "DoubleWrite" => double_write_factory,
        "Message" => message_factory,
        "Limits" => limit_message_factory,
        "InputProbe" => input_probe_factory,
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
        events
            .iter()
            .filter(|event| event.message == "receiver:bulk:sender")
            .count(),
        MAX_MESSAGES_PER_FRAME
    );
    assert!(events.iter().any(|event| {
        event.level == crate::ScriptEventLevel::Error
            && event.message.contains("dropped 4 message(s)")
    }));
    assert!(
        events
            .iter()
            .any(|event| event.message == "bulk_sender:update")
    );
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
