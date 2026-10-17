use super::world;
use crate::ScriptTime;
use crate::host::{ScriptHost, ScriptLoadError};
use crate::transaction::{
    MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_STRING_BYTES,
    MAX_MESSAGES_PER_FRAME,
};
use engine_scene::World;
use rve_sdk::abi::{
    RVE_LOG_INFO, RVE_SCRIPT_ABI_VERSION, RveHeader, RveHost, RveModule, RveParams, RveScriptDesc,
    RveStatus, RveStr, RveTransform, RveValue,
};
use rve_sdk::{Params, Script, Value};
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
fov_deg = 60.0
near = 0.1
far = 100.0
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
    static MODULE: RveModule = RveModule {
        header: RveHeader {
            abi_version: RVE_SCRIPT_ABI_VERSION + 1,
            header_size: std::mem::size_of::<RveHeader>() as u32,
        },
        scripts: std::ptr::null(),
        script_count: 1,
    };
    // SAFETY: the header is valid; on a version mismatch the remaining fields are not read.
    let result = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(&MODULE)) };
    assert!(matches!(
        result,
        Err(crate::ScriptLibraryError::AbiVersion {
            expected: RVE_SCRIPT_ABI_VERSION,
            found: value,
        }) if value == RVE_SCRIPT_ABI_VERSION + 1
    ));
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
fov_deg = 60.0
near = 0.1
far = 100.0
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
fov_deg = 60.0
near = 0.1
far = 100.0
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
        },
        RveScriptDesc {
            name: RveStr::from_static("ErrorObserver"),
            create: create_byte_instance,
            start: no_op,
            update: no_op_update,
            message: count_error_message,
            destroy: no_op,
            drop: free_byte,
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
