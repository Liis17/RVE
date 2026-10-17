use super::world;
use crate::host::{ScriptHost, ScriptLoadError};
use crate::{HeavyRegistry, ScriptInterruptHandle, ScriptTime};
use engine_scene::World;
use rve_sdk::abi::{
    RVE_LOG_INFO, RVE_SCRIPT_ABI_VERSION, RveHeader, RveHost, RveModule, RveParams, RveScriptDesc,
    RveStatus, RveStr, RveTransform,
};
use rve_sdk::{Params, Script, Value};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static PROBE_DROPS: AtomicUsize = AtomicUsize::new(0);

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Probe {
    label: String,
    panic_update: bool,
    nan_update: bool,
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

fn sdk_module() -> &'static RveModule {
    rve_sdk::script_module! {
        "Probe" => probe_factory,
        "Error" => error_factory,
        "Panic" => panic_factory,
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
    ScriptHost::load_with_library(
        world,
        "scene",
        &empty_sources(),
        &HeavyRegistry::default(),
        Some(library),
        ScriptInterruptHandle::new(),
    )
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
    let mut world = scripted_world("Probe", r#"{ label = "poison", panic_host_log = true }"#);
    let handle = world.handle("camera").unwrap();
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 3.5,
        frame: 17,
    };
    host.start(&mut world, time);
    host.drain_events();
    let before = world.local_transform(handle).unwrap().position;
    host.update(&mut world, 0.5, time);
    assert_eq!(world.local_transform(handle).unwrap().position, before);
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message.contains("poisoned"))
    );
    drop(host);
}

#[test]
fn non_finite_abi_transform_is_rolled_back_and_disables_the_instance() {
    let _lock = test_lock();
    PROBE_DROPS.store(0, Ordering::SeqCst);
    let library = library();
    let mut world = scripted_world("Probe", r#"{ label = "nan", nan_update = true }"#);
    let handle = world.handle("camera").unwrap();
    let mut host = load_host(&world, &library);
    let time = ScriptTime {
        elapsed: 3.5,
        frame: 17,
    };
    host.start(&mut world, time);
    host.drain_events();
    host.update(&mut world, 0.5, time);
    assert_eq!(world.local_transform(handle).unwrap().position.x, 0.0);
    assert!(
        message_list(&mut host)
            .iter()
            .any(|message| message.contains("non-finite transform"))
    );
    host.update(&mut world, 0.5, time);
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
        message: no_op_message,
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
fn create_rejects_set_local_without_mutating_the_world() {
    let module = set_local_during_create_module();
    // SAFETY: this test module and its callbacks have process lifetime.
    let library = unsafe { crate::ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap();
    let world = scripted_world("CreateMutation", "{}");
    let handle = world.handle("camera").unwrap();
    let before = world.local_transform(handle).unwrap().position;
    let host = load_host(&world, &library);
    assert_eq!(world.local_transform(handle).unwrap().position, before);
    drop(host);
    assert_eq!(world.local_transform(handle).unwrap().position, before);
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

struct Legacy;

impl crate::HeavyScript for Legacy {}

fn legacy_factory(_: &crate::ScriptParams) -> Result<Box<dyn crate::HeavyScript>, String> {
    Ok(Box::new(Legacy))
}

#[test]
fn abi_and_static_registry_name_collision_is_a_load_error() {
    let library = library();
    let mut registry = HeavyRegistry::new();
    registry.register("Probe", legacy_factory).unwrap();
    let world = scripted_world("Probe", "{}");
    let result = ScriptHost::load_with_library(
        &world,
        "scene",
        &empty_sources(),
        &registry,
        Some(&library),
        ScriptInterruptHandle::new(),
    );
    assert!(matches!(
        result,
        Err(ScriptLoadError::Heavy { message, .. }) if message.contains("both")
    ));
}
