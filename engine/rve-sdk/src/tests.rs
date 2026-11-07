use crate::abi::{
    RVE_INPUT_DOWN, RVE_INPUT_PRESSED, RVE_INPUT_RELEASED, RVE_LOG_ERROR, RVE_LOG_INFO,
    RVE_LOG_WARN, RVE_SCRIPT_ABI_VERSION, RVE_VALUE_ARRAY, RVE_VALUE_STR, RveArray, RveAudioParams,
    RveCameraSettings, RveCharacterMoveResult, RveHeader, RveHost, RveModule, RveParamEntry,
    RveParams, RveRaycastHit, RveScriptDesc, RveStatus, RveStr, RveTime, RveTransform, RveValue,
    RveValueData,
};
use crate::{
    AntiAliasing, CameraSettings, ConsoleLevel, Context, Key, MouseButton, Params, Script,
    Tonemapper, Value,
};
use std::ffi::c_void;

struct Probe {
    panic_start: bool,
    panic_update: bool,
    panic_message: bool,
    panic_destroy: bool,
    panic_drop: bool,
    last_message: String,
}

impl Script for Probe {
    fn on_start(&mut self, ctx: &mut Context<'_>) {
        assert_eq!(ctx.time().elapsed, 12.5);
        assert_eq!(ctx.object_id().as_deref(), Some("camera"));
        assert_eq!(ctx.object_name().as_deref(), Some("Camera"));
        let camera = ctx.find("camera").expect("camera exists");
        assert_eq!(camera.id(ctx).as_deref(), Some("camera"));
        assert_eq!(camera.name(ctx).as_deref(), Some("Camera"));
        assert_eq!(camera.transform(ctx).unwrap().position, [0.0; 3]);
        assert_eq!(camera.world_matrix(ctx), Some([2.0; 16]));
        ctx.log_info("guest log");
        ctx.transform.position[0] = 7.0;
        assert!(self.panic_start.then(|| panic!("start panic")).is_none());
    }

    fn on_update(&mut self, ctx: &mut Context<'_>, dt: f32) {
        if self.panic_update {
            panic!("update panic");
        }
        ctx.transform.position[1] += dt;
        if self.last_message.is_empty() {
            ctx.log_info("guest update");
        } else {
            ctx.log_info(format!("message:{}", self.last_message));
        }
    }

    fn on_message(&mut self, ctx: &mut Context<'_>, sender: &str, name: &str, _: &Value) {
        if self.panic_message {
            panic!("message panic");
        }
        self.last_message = format!("{sender}:{name}");
        let nested = Value::Array(vec![Value::Int(9), Value::String("sdk".to_owned())]);
        assert!(ctx.send("camera", "reply", &nested));
        assert!(ctx.broadcast("notice", &Value::Bool(true)));
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        if self.panic_destroy {
            panic!("destroy panic");
        }
        ctx.log_info("guest destroy");
    }
}

struct ConsoleProbe;

impl Script for ConsoleProbe {
    fn on_start(&mut self, ctx: &mut Context<'_>) {
        ctx.console.write("sdk log", ConsoleLevel::Log);
        ctx.console.write("sdk warning", ConsoleLevel::Warning);
        ctx.console.write("sdk error", ConsoleLevel::Error);
        ctx.console
            .register("sdk.cmd", Some("SDK console command"))
            .unwrap();
    }

    fn on_command(&mut self, ctx: &mut Context<'_>, name: &str, args: &[&str]) {
        assert_eq!(name, "sdk.cmd");
        assert_eq!(args, ["first", "second"]);
        ctx.console.write("command warning", ConsoleLevel::Warning);
        ctx.console.unregister(name).unwrap();
    }
}

struct PanicCommand;

impl Script for PanicCommand {
    fn on_command(&mut self, _: &mut Context<'_>, _: &str, _: &[&str]) {
        panic!("command panic");
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.panic_drop {
            panic!("drop panic");
        }
    }
}

fn probe_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(Probe {
        panic_start: matches!(params.get("panic_start"), Some(Value::Bool(true))),
        panic_update: matches!(params.get("panic_update"), Some(Value::Bool(true))),
        panic_message: matches!(params.get("panic_message"), Some(Value::Bool(true))),
        panic_destroy: matches!(params.get("panic_destroy"), Some(Value::Bool(true))),
        panic_drop: matches!(params.get("panic_drop"), Some(Value::Bool(true))),
        last_message: String::new(),
    }))
}

fn error_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Err("factory error".to_owned())
}

fn panic_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    panic!("factory panic")
}

fn console_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(ConsoleProbe))
}

fn panic_command_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(PanicCommand))
}

fn scripts() -> &'static RveModule {
    crate::script_module! {
        "Probe" => probe_factory,
        "Error" => error_factory,
        "Panic" => panic_factory,
    }
}

fn command_scripts() -> &'static RveModule {
    crate::script_module! {
        "ConsoleProbe" => console_factory,
        "PanicCommand" => panic_command_factory,
    }
}

mod factories_named_like_callbacks {
    use crate::abi::RveModule;
    use crate::{Params, Script};

    struct Named;

    impl Script for Named {}

    fn create(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Named))
    }

    fn start(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Named))
    }

    pub(super) fn module() -> &'static RveModule {
        crate::script_module! {
            "CreateFactory" => create,
            "StartFactory" => start,
        }
    }
}

mod exported {
    use crate::{Params, Script};

    struct Exported;

    impl Script for Exported {}

    fn factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Exported))
    }

    crate::export_scripts! { "Exported" => factory }
}

#[derive(Default)]
struct HostState {
    error: Option<String>,
    transform: RveTransform,
    logs: Vec<String>,
    log_levels: Vec<u32>,
    registered_commands: Vec<(String, String)>,
    unregistered_commands: Vec<String>,
    matrix: [f32; 16],
    sent: Vec<(String, String, Value)>,
    broadcasts: Vec<(String, Value)>,
    key_flags: u32,
    mouse_flags: u32,
    cursor: [f32; 2],
    mouse_delta: [f32; 2],
    wheel: [f32; 2],
    camera_settings: RveCameraSettings,
    active_camera: u64,
    physics_velocity: [f32; 3],
    physics_enabled: bool,
    physics_vectors: Vec<(u64, [f32; 3])>,
    audio_calls: Vec<String>,
    /// Status returned by every audio callback; zero is `RveStatus::Ok`.
    audio_status: u32,
    next_voice: u64,
    /// Spawn, destroy, object-state and prefab calls in guest order.
    lifecycle_calls: Vec<String>,
    /// Handle returned by every spawn callback; zero is a refusal.
    spawn_handle: u64,
    /// Result of every destroy and prefab request callback.
    lifecycle_accepts: bool,
    /// Code returned by the object-state callback.
    object_state_code: u32,
    /// Code returned by the prefab-state callback.
    prefab_state_code: u32,
}

unsafe extern "C" fn set_error(ctx: *mut c_void, message: RveStr) {
    // SAFETY: test host points to a live HostState and guest strings are valid during the call.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: message references a live guest string for this callback.
    state.error = Some(unsafe { message.to_str() }.unwrap().to_owned());
}

unsafe extern "C" fn log(ctx: *mut c_void, level: u32, message: RveStr) {
    // SAFETY: test host points to a live HostState and guest strings are valid during the call.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: message references a live guest string for this callback.
    state
        .logs
        .push(unsafe { message.to_str() }.unwrap().to_owned());
    state.log_levels.push(level);
}

unsafe extern "C" fn register_command(ctx: *mut c_void, name: RveStr, description: RveStr) -> u32 {
    // SAFETY: the host context and string slices remain live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    let name = unsafe { name.to_str() }.unwrap().to_owned();
    let description = unsafe { description.to_str() }.unwrap().to_owned();
    state.registered_commands.push((name, description));
    RveStatus::Ok as u32
}

unsafe extern "C" fn unregister_command(ctx: *mut c_void, name: RveStr) -> u32 {
    // SAFETY: the host context and string slice remain live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state
        .unregistered_commands
        .push(unsafe { name.to_str() }.unwrap().to_owned());
    RveStatus::Ok as u32
}

unsafe extern "C" fn find_object(_: *mut c_void, id: RveStr) -> u64 {
    // SAFETY: SDK tests pass a valid id string during the callback.
    if unsafe { id.to_str() }.unwrap() == "camera" {
        1
    } else {
        0
    }
}

unsafe extern "C" fn object_id(_: *mut c_void, _: u64, out: *mut RveStr) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: camera_id has process lifetime and out is supplied by the ABI caller.
    unsafe { *out = RveStr::from_static("camera") };
    true
}

unsafe extern "C" fn object_name(_: *mut c_void, _: u64, out: *mut RveStr) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: camera_name has process lifetime and out is supplied by the ABI caller.
    unsafe { *out = RveStr::from_static("Camera") };
    true
}

unsafe extern "C" fn get_local(ctx: *mut c_void, _: u64, out: *mut RveTransform) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and out is supplied by the ABI caller.
    unsafe { *out = (&*ctx.cast::<HostState>()).transform };
    true
}

unsafe extern "C" fn world_matrix(ctx: *mut c_void, _: u64, out: *mut [f32; 16]) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and out is supplied by the ABI caller.
    unsafe { *out = (&*ctx.cast::<HostState>()).matrix };
    true
}

unsafe extern "C" fn send(
    ctx: *mut c_void,
    target: RveStr,
    name: RveStr,
    value: *const RveValue,
) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: test host and message pointers remain live through this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    let target = unsafe { target.to_str() }.unwrap().to_owned();
    let name = unsafe { name.to_str() }.unwrap().to_owned();
    let value = unsafe { (*value).into_owned() }.unwrap();
    state.sent.push((target, name, value));
    true
}

unsafe extern "C" fn broadcast(ctx: *mut c_void, name: RveStr, value: *const RveValue) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: test host and message pointers remain live through this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    let name = unsafe { name.to_str() }.unwrap().to_owned();
    let value = unsafe { (*value).into_owned() }.unwrap();
    state.broadcasts.push((name, value));
    true
}

unsafe extern "C" fn set_local(ctx: *mut c_void, _: u64, transform: *const RveTransform) -> bool {
    if transform.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and transform lives through this call.
    unsafe { (&mut *ctx.cast::<HostState>()).transform = *transform };
    true
}

unsafe extern "C" fn key_state(ctx: *mut c_void, key: u32) -> u32 {
    // SAFETY: test host points to a live HostState for the guest call.
    let state = unsafe { &*ctx.cast::<HostState>() };
    if key == Key::W as u32 {
        state.key_flags
    } else {
        0
    }
}

unsafe extern "C" fn mouse_button_state(ctx: *mut c_void, button: u32) -> u32 {
    // SAFETY: test host points to a live HostState for the guest call.
    let state = unsafe { &*ctx.cast::<HostState>() };
    if button == MouseButton::Left as u32 {
        state.mouse_flags
    } else {
        0
    }
}

unsafe fn input_vector(
    ctx: *mut c_void,
    out: *mut [f32; 2],
    select: fn(&HostState) -> [f32; 2],
) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and out is supplied by the ABI caller.
    unsafe { *out = select(&*ctx.cast::<HostState>()) };
    true
}

unsafe extern "C" fn cursor_position(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    // SAFETY: pointers are forwarded to the checked helper.
    unsafe { input_vector(ctx, out, |state| state.cursor) }
}

unsafe extern "C" fn mouse_delta(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    // SAFETY: pointers are forwarded to the checked helper.
    unsafe { input_vector(ctx, out, |state| state.mouse_delta) }
}

unsafe extern "C" fn wheel_delta(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    // SAFETY: pointers are forwarded to the checked helper.
    unsafe { input_vector(ctx, out, |state| state.wheel) }
}

unsafe extern "C" fn get_camera_settings(
    ctx: *mut c_void,
    object: u64,
    out: *mut RveCameraSettings,
) -> bool {
    if object == 0 || out.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and out is supplied by the ABI caller.
    unsafe { *out = (&*ctx.cast::<HostState>()).camera_settings };
    true
}

unsafe extern "C" fn set_camera_settings(
    ctx: *mut c_void,
    object: u64,
    settings: *const RveCameraSettings,
) -> bool {
    if object == 0 || settings.is_null() {
        return false;
    }
    // SAFETY: test host and settings pointer remain live through this callback.
    unsafe { (&mut *ctx.cast::<HostState>()).camera_settings = *settings };
    true
}

unsafe extern "C" fn active_camera(ctx: *mut c_void) -> u64 {
    // SAFETY: test host points to a live HostState during the guest call.
    unsafe { (&*ctx.cast::<HostState>()).active_camera }
}

unsafe extern "C" fn set_active_camera(ctx: *mut c_void, object: u64) -> bool {
    if object == 0 {
        return false;
    }
    // SAFETY: test host points to a live HostState during the guest call.
    unsafe { (&mut *ctx.cast::<HostState>()).active_camera = object };
    true
}

fn host(state: &mut HostState) -> RveHost {
    state.matrix = [2.0; 16];
    RveHost {
        ctx: (state as *mut HostState).cast(),
        time: RveTime {
            elapsed: 12.5,
            frame: 8,
        },
        self_object: 1,
        set_error,
        log,
        find_object,
        object_id,
        object_name,
        get_local,
        set_local,
        world_matrix,
        send,
        broadcast,
        key_state,
        mouse_button_state,
        cursor_position,
        mouse_delta,
        wheel_delta,
        get_camera_settings: Some(get_camera_settings),
        set_camera_settings: Some(set_camera_settings),
        active_camera: Some(active_camera),
        set_active_camera: Some(set_active_camera),
        register_command: Some(register_command),
        unregister_command: Some(unregister_command),
        action_state: None,
        action_value: None,
        input_context: None,
        set_input_context: None,
        rebind_action: None,
        action_binding: None,
        reset_bindings: None,
        raycast: None,
        get_velocity: None,
        set_velocity: None,
        add_impulse: None,
        add_force: None,
        move_character: None,
        is_grounded: None,
        set_collider_enabled: None,
        is_collider_enabled: None,
        audio_play_source: None,
        audio_stop_source: None,
        audio_set_source_volume: None,
        audio_play_clip: None,
        audio_stop_voice: None,
        audio_set_group_volume: None,
        spawn: None,
        destroy: None,
        object_state: None,
        prefab_load: None,
        prefab_unload: None,
        prefab_state: None,
    }
}

unsafe extern "C" fn physics_raycast(
    _: *mut c_void,
    origin: *const [f32; 3],
    direction: *const [f32; 3],
    max_distance: f32,
    _: bool,
    _: u64,
    out: *mut RveRaycastHit,
) -> bool {
    if origin.is_null() || direction.is_null() || out.is_null() || max_distance <= 0.0 {
        return false;
    }
    // SAFETY: test inputs and output are valid for the callback.
    unsafe {
        *out = RveRaycastHit {
            object: 2,
            point: [1.0, 2.0, 3.0],
            normal: [0.0, 1.0, 0.0],
            distance: 4.5,
        }
    };
    true
}

unsafe extern "C" fn physics_get_velocity(ctx: *mut c_void, _: u64, out: *mut [f32; 3]) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: the context and output slot are valid for the callback.
    unsafe { *out = (&*ctx.cast::<HostState>()).physics_velocity };
    true
}

unsafe extern "C" fn physics_set_velocity(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: the context and vector pointer are valid for the callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state.physics_velocity = unsafe { *value };
    state.physics_vectors.push((object, state.physics_velocity));
    true
}

unsafe extern "C" fn physics_stage_vector(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    if value.is_null() {
        return false;
    }
    // SAFETY: the context and vector pointer are valid for the callback.
    unsafe { &mut *ctx.cast::<HostState>() }
        .physics_vectors
        .push((object, unsafe { *value }));
    true
}

unsafe extern "C" fn physics_move_character(
    _: *mut c_void,
    _: u64,
    displacement: *const [f32; 3],
    out: *mut RveCharacterMoveResult,
) -> bool {
    if displacement.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: the callback receives valid input and output storage.
    unsafe {
        *out = RveCharacterMoveResult {
            translation: *displacement,
            grounded: true,
            collided: false,
        }
    };
    true
}

unsafe extern "C" fn physics_get_grounded(_: *mut c_void, _: u64, out: *mut bool) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: out is writable for this callback.
    unsafe { *out = true };
    true
}

unsafe extern "C" fn physics_set_enabled(ctx: *mut c_void, _: u64, enabled: bool) -> bool {
    // SAFETY: the context is live for this callback.
    unsafe { (&mut *ctx.cast::<HostState>()).physics_enabled = enabled };
    true
}

unsafe extern "C" fn physics_get_enabled(ctx: *mut c_void, _: u64, out: *mut bool) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: the context and output pointer are valid for the callback.
    unsafe { *out = (&*ctx.cast::<HostState>()).physics_enabled };
    true
}

fn physics_host(state: &mut HostState) -> RveHost {
    let mut host = host(state);
    host.raycast = Some(physics_raycast);
    host.get_velocity = Some(physics_get_velocity);
    host.set_velocity = Some(physics_set_velocity);
    host.add_impulse = Some(physics_stage_vector);
    host.add_force = Some(physics_stage_vector);
    host.move_character = Some(physics_move_character);
    host.is_grounded = Some(physics_get_grounded);
    host.set_collider_enabled = Some(physics_set_enabled);
    host.is_collider_enabled = Some(physics_get_enabled);
    host
}

fn descriptor(module: &RveModule, name: &str) -> &'static RveScriptDesc {
    // SAFETY: the macro produces a static immutable table.
    unsafe { std::slice::from_raw_parts(module.scripts, module.script_count) }
        .iter()
        .find(|script| {
            // SAFETY: descriptor names are static UTF-8 from this test macro.
            unsafe { script.name.to_str() }.unwrap() == name
        })
        .unwrap()
}

#[test]
fn script_module_macro_builds_abi_table_and_guest_callbacks() {
    let module = scripts();
    assert_eq!(module.header.abi_version, RVE_SCRIPT_ABI_VERSION);
    assert_eq!(
        module.header.header_size as usize,
        std::mem::size_of::<RveHeader>()
    );
    assert_eq!(module.script_count, 3);

    let script = descriptor(module, "Probe");
    let mut state = HostState {
        transform: RveTransform::default(),
        ..HostState::default()
    };
    let host = host(&mut state);
    let params = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: pointers are valid for the callback and the static descriptor is from this SDK.
    assert_eq!(
        unsafe { (script.create)(&host, &params, &mut instance) },
        RveStatus::Ok
    );
    assert!(!instance.is_null());
    // SAFETY: this callback uses the live host and instance created above.
    assert_eq!(unsafe { (script.start)(instance, &host) }, RveStatus::Ok);
    assert_eq!(state.transform.position, [7.0, 0.0, 0.0]);
    assert_eq!(state.logs, ["guest log"]);
    // SAFETY: event and callback pointers are valid during this guest call.
    assert_eq!(
        unsafe {
            (script.collision.unwrap())(
                instance,
                &host,
                crate::abi::RveCollisionEvent {
                    kind: crate::abi::RVE_COLLISION_ENTER,
                    other: 1,
                    point: [1.0, 0.0, 0.0],
                    normal: [1.0, 0.0, 0.0],
                },
            )
        },
        RveStatus::Ok
    );
    // SAFETY: drop is called exactly once for this successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);
}

#[test]
fn console_api_logs_registers_commands_and_catches_command_panics() {
    let module = command_scripts();
    let mut state = HostState::default();
    let host = host(&mut state);
    let params = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut console_instance = std::ptr::null_mut();
    let console_script = descriptor(module, "ConsoleProbe");
    // SAFETY: descriptor and host are live for each synchronous callback.
    unsafe {
        assert_eq!(
            (console_script.create)(&host, &params, &mut console_instance),
            RveStatus::Ok
        );
        assert_eq!(
            (console_script.start)(console_instance, &host),
            RveStatus::Ok
        );
    }
    assert_eq!(
        state.log_levels,
        [RVE_LOG_INFO, RVE_LOG_WARN, RVE_LOG_ERROR]
    );
    assert_eq!(
        state.registered_commands,
        [("sdk.cmd".to_owned(), "SDK console command".to_owned())]
    );

    let arguments = [RveStr::from_static("first"), RveStr::from_static("second")];
    // SAFETY: command strings and instance remain live for the callback.
    unsafe {
        assert_eq!(
            (console_script.command.unwrap())(
                console_instance,
                &host,
                RveStr::from_static("sdk.cmd"),
                arguments.as_ptr(),
                arguments.len(),
            ),
            RveStatus::Ok
        );
        assert_eq!((console_script.drop)(console_instance), RveStatus::Ok);
    }
    assert_eq!(state.unregistered_commands, ["sdk.cmd"]);
    assert_eq!(state.log_levels.last(), Some(&RVE_LOG_WARN));

    let mut panic_instance = std::ptr::null_mut();
    let panic_script = descriptor(module, "PanicCommand");
    // SAFETY: descriptor and host are live for each synchronous callback.
    unsafe {
        assert_eq!(
            (panic_script.create)(&host, &params, &mut panic_instance),
            RveStatus::Ok
        );
        assert_eq!(
            (panic_script.command.unwrap())(
                panic_instance,
                &host,
                RveStr::from_static("panic"),
                std::ptr::null(),
                0,
            ),
            RveStatus::Panicked
        );
        assert_eq!((panic_script.drop)(panic_instance), RveStatus::Ok);
    }
    assert!(state.error.as_deref().unwrap().contains("command panic"));
}

#[test]
fn console_command_registration_reports_unavailable_optional_host_callbacks() {
    let mut state = HostState::default();
    let mut host = host(&mut state);
    host.register_command = None;
    host.unregister_command = None;
    let context = Context::new(&host).unwrap();
    assert!(context.console.register("sdk.cmd", None).is_err());
    assert!(context.console.unregister("sdk.cmd").is_err());
}

#[test]
fn sdk_physics_api_wraps_queries_commands_and_character_state() {
    let mut state = HostState {
        physics_velocity: [1.0, 2.0, 3.0],
        physics_enabled: true,
        ..HostState::default()
    };
    let host = physics_host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let object = context.find("camera").unwrap();

    let hit = context
        .physics()
        .raycast_with_options([0.0; 3], [1.0, 0.0, 0.0], 10.0, true, Some(object))
        .unwrap()
        .unwrap();
    assert_eq!(hit.object.id(&mut context).as_deref(), Some("camera"));
    assert_eq!(hit.point, [1.0, 2.0, 3.0]);
    assert_eq!(hit.normal, [0.0, 1.0, 0.0]);
    assert_eq!(hit.distance, 4.5);
    assert_eq!(object.get_velocity(&context).unwrap(), [1.0, 2.0, 3.0]);
    object.set_velocity(&mut context, [4.0, 5.0, 6.0]).unwrap();
    object.add_impulse(&mut context, [0.0, 1.0, 0.0]).unwrap();
    object.add_force(&mut context, [0.0, 0.0, 2.0]).unwrap();
    let movement = object
        .move_character(&mut context, [0.5, 0.0, 0.0])
        .unwrap();
    assert_eq!(movement.translation, [0.5, 0.0, 0.0]);
    assert!(movement.grounded);
    assert!(!movement.collided);
    assert!(object.is_grounded(&context).unwrap());
    object.set_collider_enabled(&mut context, false).unwrap();
    assert!(!object.is_collider_enabled(&context).unwrap());
    context.commit().unwrap();
    assert_eq!(state.physics_velocity, [4.0, 5.0, 6.0]);
    assert_eq!(state.physics_vectors.len(), 3);
}

#[test]
fn sdk_physics_api_reports_absent_optional_callbacks() {
    let mut state = HostState::default();
    let host = host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let object = context.find("camera").unwrap();
    assert_eq!(
        context.physics().raycast([0.0; 3], [1.0, 0.0, 0.0], 1.0),
        Err(crate::PhysicsError::Unavailable)
    );
    assert_eq!(
        object.set_velocity(&mut context, [0.0; 3]),
        Err(crate::PhysicsError::Unavailable)
    );
}

unsafe extern "C" fn audio_object(ctx: *mut c_void, _: u64) -> u32 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state.audio_calls.push("object".to_owned());
    state.audio_status
}

unsafe extern "C" fn audio_object_volume(ctx: *mut c_void, _: u64, volume: f32) -> u32 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state.audio_calls.push(format!("volume:{volume}"));
    state.audio_status
}

unsafe extern "C" fn audio_play_clip(
    ctx: *mut c_void,
    path: RveStr,
    params: *const RveAudioParams,
    out_voice: *mut u64,
) -> u32 {
    // SAFETY: the context, guest path and params are live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    let path = unsafe { path.to_str() }.unwrap();
    let params = unsafe { *params };
    state.audio_calls.push(format!(
        "clip:{path}:group={}:volume={}:looping={}:position={:?}",
        params.group,
        params.volume,
        params.looping,
        params.has_position.then_some(params.position)
    ));
    if state.audio_status == 0 {
        // SAFETY: out_voice is writable for this callback.
        unsafe { *out_voice = state.next_voice };
    }
    state.audio_status
}

unsafe extern "C" fn audio_stop_voice(ctx: *mut c_void, voice: u64) -> u32 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state.audio_calls.push(format!("stop:{voice}"));
    state.audio_status
}

unsafe extern "C" fn audio_group_volume(ctx: *mut c_void, group: u32, volume: f32) -> u32 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state.audio_calls.push(format!("group:{group}:{volume}"));
    state.audio_status
}

fn audio_host(state: &mut HostState) -> RveHost {
    let mut host = host(state);
    host.audio_play_source = Some(audio_object);
    host.audio_stop_source = Some(audio_object);
    host.audio_set_source_volume = Some(audio_object_volume);
    host.audio_play_clip = Some(audio_play_clip);
    host.audio_stop_voice = Some(audio_stop_voice);
    host.audio_set_group_volume = Some(audio_group_volume);
    host
}

#[test]
fn sdk_audio_api_forwards_options_and_returns_the_voice_id() {
    let mut state = HostState {
        next_voice: 42,
        ..HostState::default()
    };
    let host = audio_host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let object = context.find("camera").unwrap();
    object.play_sound(&mut context).unwrap();
    object.set_sound_volume(&mut context, 0.5).unwrap();
    let voice = context
        .play_clip(
            "/audio/hum.wav",
            crate::ClipOptions {
                group: crate::AudioGroup::Music,
                volume: 0.25,
                looping: true,
                position: Some([1.0, 2.0, 3.0]),
            },
        )
        .unwrap();
    assert_eq!(voice, 42);
    context.stop_voice(voice).unwrap();
    context
        .set_group_volume(crate::AudioGroup::Master, 0.75)
        .unwrap();
    assert_eq!(
        state.audio_calls,
        [
            "object",
            "volume:0.5",
            "clip:/audio/hum.wav:group=1:volume=0.25:looping=true:position=Some([1.0, 2.0, 3.0])",
            "stop:42",
            "group:0:0.75",
        ]
    );
}

#[test]
fn sdk_audio_api_reports_rejections_missing_callbacks_and_invalid_input() {
    let mut state = HostState {
        audio_status: 1,
        ..HostState::default()
    };
    let host = audio_host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let object = context.find("camera").unwrap();
    assert_eq!(
        object.play_sound(&mut context),
        Err(crate::AudioError::Rejected)
    );
    assert_eq!(
        context.play_clip("/audio/hum.wav", crate::ClipOptions::default()),
        Err(crate::AudioError::Rejected)
    );
    assert_eq!(
        object.set_sound_volume(&mut context, -1.0),
        Err(crate::AudioError::InvalidInput)
    );
    assert_eq!(
        object.set_sound_volume(&mut context, crate::MAX_AUDIO_VOLUME + 0.5),
        Err(crate::AudioError::InvalidInput)
    );
    assert_eq!(
        context.set_group_volume(crate::AudioGroup::Music, 10.0),
        Err(crate::AudioError::InvalidInput)
    );
    let nan_position = crate::ClipOptions {
        position: Some([f32::NAN, 0.0, 0.0]),
        ..crate::ClipOptions::default()
    };
    assert_eq!(
        context.play_clip("/audio/hum.wav", nan_position),
        Err(crate::AudioError::InvalidInput)
    );
    assert_eq!(
        state.audio_calls.len(),
        2,
        "invalid input never reaches the host"
    );

    let mut plain = HostState::default();
    let plain_host = self::host(&mut plain);
    let mut context = Context::new(&plain_host).unwrap();
    let object = context.find("camera").unwrap();
    assert_eq!(
        object.stop_sound(&mut context),
        Err(crate::AudioError::Unavailable)
    );
    assert_eq!(
        context.set_group_volume(crate::AudioGroup::Music, 1.0),
        Err(crate::AudioError::Unavailable)
    );
}

#[test]
fn audio_callbacks_and_params_keep_the_v7_layout() {
    use std::mem::{offset_of, size_of};
    assert_eq!(size_of::<crate::abi::RveAudioParams>(), 24);
    assert_eq!(offset_of!(crate::abi::RveAudioParams, position), 12);
    assert_eq!(
        offset_of!(RveHost, audio_play_source),
        offset_of!(RveHost, is_collider_enabled)
            + size_of::<Option<crate::abi::RvePhysicsBoolFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, audio_set_group_volume),
        offset_of!(RveHost, audio_stop_voice) + size_of::<Option<crate::abi::RveAudioVoiceFn>>()
    );
}

fn present<T>(flags: u32, bit: u32, value: T) -> Option<T> {
    (flags & bit != 0).then_some(value)
}

/// Renders the spawn options as the guest-visible fields, so tests can compare the whole request.
fn describe_spawn(prefab: &str, options: Option<crate::abi::RveSpawnOptions>) -> String {
    let Some(options) = options else {
        return format!("spawn:{prefab}:null");
    };
    let flags = options.flags;
    // SAFETY: the test host receives strings that the guest keeps alive for this call.
    let name = present(flags, crate::abi::RVE_SPAWN_HAS_NAME, options.name)
        .map(|value| unsafe { value.to_str() }.unwrap().to_owned());
    let id = present(flags, crate::abi::RVE_SPAWN_HAS_ID, options.id)
        .map(|value| unsafe { value.to_str() }.unwrap().to_owned());
    format!(
        "spawn:{prefab}:flags={flags:#x}:position={:?}:rotation={:?}:scale={:?}:velocity={:?}:parent={:?}:name={name:?}:id={id:?}",
        present(flags, crate::abi::RVE_SPAWN_HAS_POSITION, options.position),
        present(
            flags,
            crate::abi::RVE_SPAWN_HAS_ROTATION,
            options.rotation_deg
        ),
        present(flags, crate::abi::RVE_SPAWN_HAS_SCALE, options.scale),
        present(flags, crate::abi::RVE_SPAWN_HAS_VELOCITY, options.velocity),
        present(flags, crate::abi::RVE_SPAWN_HAS_PARENT, options.parent),
    )
}

unsafe extern "C" fn spawn_object(
    ctx: *mut c_void,
    prefab: RveStr,
    options: *const crate::abi::RveSpawnOptions,
) -> u64 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: the guest passes a valid prefab string for this callback.
    let prefab = unsafe { prefab.to_str() }.unwrap();
    // SAFETY: the guest passes either null or a readable options struct.
    let options = unsafe { options.as_ref() }.copied();
    state.lifecycle_calls.push(describe_spawn(prefab, options));
    state.spawn_handle
}

unsafe extern "C" fn destroy_object(ctx: *mut c_void, object: u64, delay: f32) -> bool {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state
        .lifecycle_calls
        .push(format!("destroy:{object}:{delay}"));
    state.lifecycle_accepts
}

unsafe extern "C" fn object_state(ctx: *mut c_void, object: u64) -> u32 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    state.lifecycle_calls.push(format!("state:{object}"));
    state.object_state_code
}

unsafe extern "C" fn prefab_load(ctx: *mut c_void, path: RveStr) -> bool {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: the guest passes a valid path string for this callback.
    let path = unsafe { path.to_str() }.unwrap();
    state.lifecycle_calls.push(format!("load:{path}"));
    state.lifecycle_accepts
}

unsafe extern "C" fn prefab_unload(ctx: *mut c_void, path: RveStr) -> bool {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: the guest passes a valid path string for this callback.
    let path = unsafe { path.to_str() }.unwrap();
    state.lifecycle_calls.push(format!("unload:{path}"));
    state.lifecycle_accepts
}

unsafe extern "C" fn prefab_state(ctx: *mut c_void, path: RveStr) -> u32 {
    // SAFETY: the context is live for this callback.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: the guest passes a valid path string for this callback.
    let path = unsafe { path.to_str() }.unwrap();
    state.lifecycle_calls.push(format!("prefab_state:{path}"));
    state.prefab_state_code
}

fn lifecycle_host(state: &mut HostState) -> RveHost {
    let mut host = host(state);
    host.spawn = Some(spawn_object);
    host.destroy = Some(destroy_object);
    host.object_state = Some(object_state);
    host.prefab_load = Some(prefab_load);
    host.prefab_unload = Some(prefab_unload);
    host.prefab_state = Some(prefab_state);
    host
}

#[test]
fn sdk_spawn_api_forwards_present_fields_and_returns_the_pending_object() {
    let mut state = HostState {
        spawn_handle: 42,
        ..HostState::default()
    };
    let host = lifecycle_host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let camera = context.find("camera").unwrap();
    let options = crate::SpawnOptions::new()
        .position([1.0, 2.0, 3.0])
        .rotation_deg([0.0, 90.0, 0.0])
        .scale([2.0; 3])
        .velocity([0.0, 5.0, 0.0])
        .parent(camera)
        .name("Bolt")
        .id("bolt_1");
    let bolt = context
        .spawn("prefabs/bolt.rveprefab", &options)
        .expect("host returned a pending handle");
    assert_eq!(bolt.handle, 42);
    assert_eq!(
        state.lifecycle_calls.last().unwrap(),
        &format!(
            "spawn:prefabs/bolt.rveprefab:flags=0x7f:position=Some([1.0, 2.0, 3.0]):rotation=Some([0.0, 90.0, 0.0]):scale=Some([2.0, 2.0, 2.0]):velocity=Some([0.0, 5.0, 0.0]):parent=Some({}):name=Some(\"Bolt\"):id=Some(\"bolt_1\")",
            camera.handle
        )
    );

    // Unset fields are not presented to the host; the request is a no-op for the prefab's values.
    context.spawn("prefabs/bolt.rveprefab", &crate::SpawnOptions::new());
    assert_eq!(
        state.lifecycle_calls.last().unwrap(),
        "spawn:prefabs/bolt.rveprefab:flags=0x0:position=None:rotation=None:scale=None:velocity=None:parent=None:name=None:id=None"
    );
}

#[test]
fn sdk_spawn_api_returns_none_for_refusals_and_non_finite_options() {
    let mut state = HostState::default();
    let host = lifecycle_host(&mut state);
    let mut context = Context::new(&host).unwrap();
    assert!(
        context
            .spawn("prefabs/bolt.rveprefab", &crate::SpawnOptions::new())
            .is_none(),
        "a zero handle is a refusal"
    );
    let calls = state.lifecycle_calls.len();
    let non_finite = crate::SpawnOptions::new().position([f32::NAN, 0.0, 0.0]);
    assert!(
        context
            .spawn("prefabs/bolt.rveprefab", &non_finite)
            .is_none()
    );
    let infinite = crate::SpawnOptions::new().velocity([0.0, f32::INFINITY, 0.0]);
    assert!(context.spawn("prefabs/bolt.rveprefab", &infinite).is_none());
    assert_eq!(
        state.lifecycle_calls.len(),
        calls,
        "non-finite options never reach the host"
    );
}

#[test]
fn sdk_object_lifecycle_api_forwards_destroy_and_state_codes() {
    let mut state = HostState {
        lifecycle_accepts: true,
        object_state_code: crate::abi::RVE_OBJECT_ALIVE,
        ..HostState::default()
    };
    let host = lifecycle_host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let object = context.find("camera").unwrap();
    assert!(object.destroy(&mut context, 1.5));
    assert!(object.is_alive(&context));
    assert!(!object.is_pending(&context));
    for delay in [f32::NAN, f32::INFINITY, -0.5] {
        assert!(!object.destroy(&mut context, delay), "delay {delay}");
    }
    assert_eq!(
        state.lifecycle_calls,
        [
            format!("destroy:{}:1.5", object.handle),
            format!("state:{}", object.handle),
            format!("state:{}", object.handle),
        ],
        "invalid delays never reach the host"
    );

    state.object_state_code = crate::abi::RVE_OBJECT_PENDING;
    assert!(object.is_pending(&context));
    assert!(!object.is_alive(&context));
    state.object_state_code = crate::abi::RVE_OBJECT_DEAD;
    assert!(!object.is_pending(&context));
    assert!(!object.is_alive(&context));

    state.lifecycle_accepts = false;
    assert!(!object.destroy(&mut context, 0.0), "the host refused it");
}

#[test]
fn sdk_prefab_api_forwards_requests_and_maps_every_state_code() {
    let mut state = HostState {
        lifecycle_accepts: true,
        ..HostState::default()
    };
    let host = lifecycle_host(&mut state);
    let context = Context::new(&host).unwrap();
    let prefabs = context.prefabs();
    assert!(prefabs.load("prefabs/heavy.rveprefab"));
    assert!(prefabs.unload("prefabs/heavy.rveprefab"));

    let expected = [
        (crate::abi::RVE_PREFAB_UNKNOWN, crate::PrefabState::Unknown),
        (
            crate::abi::RVE_PREFAB_RESIDENT,
            crate::PrefabState::Resident,
        ),
        (
            crate::abi::RVE_PREFAB_UNLOADED,
            crate::PrefabState::Unloaded,
        ),
        (crate::abi::RVE_PREFAB_LOADING, crate::PrefabState::Loading),
        (crate::abi::RVE_PREFAB_LOADED, crate::PrefabState::Loaded),
        (
            crate::abi::RVE_PREFAB_UNLOADING,
            crate::PrefabState::Unloading,
        ),
        (crate::abi::RVE_PREFAB_FAILED, crate::PrefabState::Failed),
        (99, crate::PrefabState::Unknown),
    ];
    for (code, state_value) in expected {
        // The host state lives behind a raw pointer; updating it between calls is the test's job.
        let host_state = unsafe { &mut *host.ctx.cast::<HostState>() };
        host_state.prefab_state_code = code;
        assert_eq!(
            prefabs.state("prefabs/heavy.rveprefab"),
            state_value,
            "code {code}"
        );
    }
    assert_eq!(
        state.lifecycle_calls.last().unwrap(),
        "prefab_state:prefabs/heavy.rveprefab"
    );
    assert_eq!(state.lifecycle_calls[0], "load:prefabs/heavy.rveprefab");
    assert_eq!(state.lifecycle_calls[1], "unload:prefabs/heavy.rveprefab");

    state.lifecycle_accepts = false;
    assert!(!prefabs.load("prefabs/heavy.rveprefab"));
}

#[test]
fn sdk_lifecycle_api_reports_missing_callbacks_without_calling_the_host() {
    let mut state = HostState::default();
    let host = self::host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let object = context.find("camera").unwrap();
    assert!(
        context
            .spawn("prefabs/bolt.rveprefab", &crate::SpawnOptions::new())
            .is_none()
    );
    assert!(!object.destroy(&mut context, 0.0));
    assert!(!object.is_alive(&context));
    assert!(!object.is_pending(&context));
    let prefabs = context.prefabs();
    assert!(!prefabs.load("prefabs/heavy.rveprefab"));
    assert!(!prefabs.unload("prefabs/heavy.rveprefab"));
    assert_eq!(
        prefabs.state("prefabs/heavy.rveprefab"),
        crate::PrefabState::Unavailable
    );
    assert!(state.lifecycle_calls.is_empty());
}

#[test]
fn lifecycle_callbacks_and_spawn_options_keep_the_v8_layout() {
    use crate::abi::RveSpawnOptions;
    use std::mem::{align_of, offset_of, size_of};
    assert_eq!(size_of::<RveSpawnOptions>(), 96);
    assert_eq!(align_of::<RveSpawnOptions>(), 8);
    assert_eq!(offset_of!(RveSpawnOptions, flags), 0);
    assert_eq!(offset_of!(RveSpawnOptions, position), 4);
    assert_eq!(offset_of!(RveSpawnOptions, rotation_deg), 16);
    assert_eq!(offset_of!(RveSpawnOptions, scale), 28);
    assert_eq!(offset_of!(RveSpawnOptions, velocity), 40);
    assert_eq!(offset_of!(RveSpawnOptions, parent), 56);
    assert_eq!(offset_of!(RveSpawnOptions, name), 64);
    assert_eq!(offset_of!(RveSpawnOptions, id), 80);

    assert_eq!(
        offset_of!(RveHost, spawn),
        offset_of!(RveHost, audio_set_group_volume)
            + size_of::<Option<crate::abi::RveAudioGroupVolumeFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, destroy),
        offset_of!(RveHost, spawn) + size_of::<Option<crate::abi::RveSpawnFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, object_state),
        offset_of!(RveHost, destroy) + size_of::<Option<crate::abi::RveDestroyObjectFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, prefab_load),
        offset_of!(RveHost, object_state) + size_of::<Option<crate::abi::RveObjectStateFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, prefab_unload),
        offset_of!(RveHost, prefab_load) + size_of::<Option<crate::abi::RvePrefabRequestFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, prefab_state),
        offset_of!(RveHost, prefab_unload) + size_of::<Option<crate::abi::RvePrefabRequestFn>>()
    );

    use crate::abi::{
        RVE_OBJECT_ALIVE, RVE_OBJECT_DEAD, RVE_OBJECT_PENDING, RVE_PREFAB_FAILED,
        RVE_PREFAB_LOADED, RVE_PREFAB_LOADING, RVE_PREFAB_RESIDENT, RVE_PREFAB_UNKNOWN,
        RVE_PREFAB_UNLOADED, RVE_PREFAB_UNLOADING, RVE_SPAWN_HAS_ID, RVE_SPAWN_HAS_NAME,
        RVE_SPAWN_HAS_PARENT, RVE_SPAWN_HAS_POSITION, RVE_SPAWN_HAS_ROTATION, RVE_SPAWN_HAS_SCALE,
        RVE_SPAWN_HAS_VELOCITY, RVE_SPAWN_KNOWN_FLAGS,
    };
    assert_eq!(
        [RVE_OBJECT_DEAD, RVE_OBJECT_PENDING, RVE_OBJECT_ALIVE],
        [0, 1, 2]
    );
    assert_eq!(
        [
            RVE_PREFAB_UNKNOWN,
            RVE_PREFAB_RESIDENT,
            RVE_PREFAB_UNLOADED,
            RVE_PREFAB_LOADING,
            RVE_PREFAB_LOADED,
            RVE_PREFAB_UNLOADING,
            RVE_PREFAB_FAILED,
        ],
        [0, 1, 2, 3, 4, 5, 6]
    );
    let flags = [
        RVE_SPAWN_HAS_POSITION,
        RVE_SPAWN_HAS_ROTATION,
        RVE_SPAWN_HAS_SCALE,
        RVE_SPAWN_HAS_VELOCITY,
        RVE_SPAWN_HAS_PARENT,
        RVE_SPAWN_HAS_NAME,
        RVE_SPAWN_HAS_ID,
    ];
    for (index, flag) in flags.into_iter().enumerate() {
        assert_eq!(flag, 1 << index);
    }
    assert_eq!(RVE_SPAWN_KNOWN_FLAGS, 0x7f);
}

#[test]
fn script_module_allows_factories_named_create_and_start() {
    let module = factories_named_like_callbacks::module();
    assert_eq!(module.script_count, 2);
    assert_eq!(
        unsafe { descriptor(module, "CreateFactory").name.to_str() }.unwrap(),
        "CreateFactory"
    );
    assert_eq!(
        unsafe { descriptor(module, "StartFactory").name.to_str() }.unwrap(),
        "StartFactory"
    );
}

#[test]
fn export_scripts_macro_exposes_a_static_module_pointer() {
    let module = exported::rve_script_module();
    assert!(!module.is_null());
    // SAFETY: the macro exports process-lifetime static module data.
    let module = unsafe { &*module };
    assert_eq!(module.header.abi_version, RVE_SCRIPT_ABI_VERSION);
    assert_eq!(module.script_count, 1);
}

#[test]
fn guest_input_wrapper_reads_states_and_pointer_vectors() {
    let mut state = HostState {
        key_flags: RVE_INPUT_DOWN | RVE_INPUT_PRESSED,
        mouse_flags: RVE_INPUT_RELEASED,
        cursor: [12.0, 34.0],
        mouse_delta: [1.5, -2.0],
        wheel: [0.0, 0.75],
        ..HostState::default()
    };
    let host = host(&mut state);
    let context = Context::new(&host).unwrap();
    let input = context.input();

    assert!(input.key_down(Key::W));
    assert!(input.key_pressed(Key::W));
    assert!(!input.key_released(Key::W));
    assert!(!input.key_down(Key::A));
    assert!(input.mouse_released(MouseButton::Left));
    assert!(!input.mouse_down(MouseButton::Left));
    assert_eq!(input.cursor(), [12.0, 34.0]);
    assert_eq!(input.mouse_delta(), [1.5, -2.0]);
    assert_eq!(input.wheel(), [0.0, 0.75]);
}

#[test]
fn guest_camera_wrappers_read_write_and_switch_cameras() {
    let mut state = HostState {
        active_camera: 1,
        ..HostState::default()
    };
    let host = host(&mut state);
    let mut context = Context::new(&host).unwrap();
    let camera = context.find("camera").unwrap();
    assert_eq!(context.active_camera(), Some(camera));
    assert_eq!(
        camera.camera_settings(&context),
        Some(CameraSettings::default())
    );

    let settings = CameraSettings {
        fov_deg: 72.0,
        near: 0.25,
        far: 250.0,
        render_scale: 0.75,
        clear_color: [0.2, 0.3, 0.4],
        exposure_ev: 2.0,
        anti_aliasing: AntiAliasing::None,
        tonemapper: Tonemapper::Neutral,
    };
    assert!(camera.set_camera_settings(&mut context, settings));
    assert_eq!(camera.camera_settings(&context), Some(settings));

    let secondary = crate::Object { handle: 2 };
    assert!(secondary.activate_camera(&mut context));
    assert_eq!(context.active_camera(), Some(secondary));
}

#[test]
fn guest_camera_wrappers_report_when_callbacks_are_unavailable() {
    let mut state = HostState {
        active_camera: 1,
        ..HostState::default()
    };
    let mut host = host(&mut state);
    host.get_camera_settings = None;
    host.set_camera_settings = None;
    host.active_camera = None;
    host.set_active_camera = None;
    let mut context = Context::new(&host).unwrap();
    let camera = context.find("camera").unwrap();

    assert_eq!(context.active_camera(), None);
    assert_eq!(camera.camera_settings(&context), None);
    assert!(!camera.set_camera_settings(&mut context, CameraSettings::default()));
    assert!(!camera.activate_camera(&mut context));
}

#[test]
fn camera_sdk_enum_codes_and_raw_settings_validation_are_stable() {
    assert_eq!(AntiAliasing::Fxaa.code(), 0);
    assert_eq!(AntiAliasing::None.code(), 1);
    assert_eq!(AntiAliasing::Taa.code(), 2);
    assert_eq!(AntiAliasing::Fsr.code(), 3);
    assert_eq!(AntiAliasing::Dlss.code(), 4);
    assert_eq!(AntiAliasing::from_code(0), Some(AntiAliasing::Fxaa));
    assert_eq!(AntiAliasing::from_code(1), Some(AntiAliasing::None));
    assert_eq!(AntiAliasing::from_code(2), Some(AntiAliasing::Taa));
    assert_eq!(AntiAliasing::from_code(3), Some(AntiAliasing::Fsr));
    assert_eq!(AntiAliasing::from_code(4), Some(AntiAliasing::Dlss));
    assert_eq!(AntiAliasing::from_code(5), None);
    assert_eq!(Tonemapper::Aces.code(), 0);
    assert_eq!(Tonemapper::Neutral.code(), 1);
    assert_eq!(Tonemapper::None.code(), 2);
    assert_eq!(Tonemapper::from_code(3), None);

    let invalid_code = RveCameraSettings {
        anti_aliasing: 99,
        ..RveCameraSettings::default()
    };
    assert!(CameraSettings::try_from(invalid_code).is_err());
    let invalid_range = RveCameraSettings {
        exposure_ev: 16.1,
        ..RveCameraSettings::default()
    };
    assert!(CameraSettings::try_from(invalid_range).is_err());
}

#[test]
fn guest_wrappers_report_factory_and_callback_panics_and_drop_panics() {
    let module = scripts();
    let mut state = HostState::default();
    let host = host(&mut state);
    let empty = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: callback pointers are valid and the host remains live.
    assert_eq!(
        unsafe { (descriptor(module, "Panic").create)(&host, &empty, &mut instance) },
        RveStatus::Panicked
    );
    assert_eq!(state.error.as_deref(), Some("factory panic"));
    assert!(instance.is_null());

    state.error = None;
    let params = [RveParamEntry {
        key: RveStr::from_static("panic_start"),
        value: RveValue::boolean(true),
    }];
    let params = RveParams {
        entries: params.as_ptr(),
        len: params.len(),
    };
    let script = descriptor(module, "Probe");
    // SAFETY: pointers are valid for the callback and params remain live.
    assert_eq!(
        unsafe { (script.create)(&host, &params, &mut instance) },
        RveStatus::Ok
    );
    // SAFETY: callback pointers are valid and this instance was created above.
    assert_eq!(
        unsafe { (script.start)(instance, &host) },
        RveStatus::Panicked
    );
    assert_eq!(state.error.as_deref(), Some("start panic"));
    // SAFETY: drop is called exactly once after successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);

    state.error = None;
    let params = [RveParamEntry {
        key: RveStr::from_static("panic_drop"),
        value: RveValue::boolean(true),
    }];
    let params = RveParams {
        entries: params.as_ptr(),
        len: params.len(),
    };
    // SAFETY: pointers are valid for the callback and params remain live.
    assert_eq!(
        unsafe { (script.create)(&host, &params, &mut instance) },
        RveStatus::Ok
    );
    // SAFETY: drop is called exactly once after successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Panicked);
    assert_eq!(state.error, None);
}

#[test]
fn guest_wrappers_invoke_update_message_and_destroy_and_report_their_panics() {
    let script = descriptor(scripts(), "Probe");
    let mut state = HostState::default();
    let host = host(&mut state);
    let empty = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: callback pointers are valid and the host remains live.
    assert_eq!(
        unsafe { (script.create)(&host, &empty, &mut instance) },
        RveStatus::Ok
    );
    // SAFETY: the instance was created above and pointers remain live.
    assert_eq!(
        unsafe { (script.update)(instance, &host, 1.5) },
        RveStatus::Ok
    );
    assert_eq!(state.transform.position[1], 1.5);
    let value = RveValue::string("hello");
    // SAFETY: message data and host remain live for the callback.
    assert_eq!(
        unsafe {
            (script.message)(
                instance,
                &host,
                RveStr::from_static("sender"),
                RveStr::from_static("greet"),
                &value,
            )
        },
        RveStatus::Ok
    );
    // SAFETY: the instance and host remain live for the callback.
    assert_eq!(
        unsafe { (script.update)(instance, &host, 0.5) },
        RveStatus::Ok
    );
    assert_eq!(state.transform.position[1], 2.0);
    assert_eq!(state.logs, ["guest update", "message:sender:greet"]);
    assert_eq!(
        state.sent,
        [(
            "camera".to_owned(),
            "reply".to_owned(),
            Value::Array(vec![Value::Int(9), Value::String("sdk".to_owned())])
        )]
    );
    assert_eq!(state.broadcasts, [("notice".to_owned(), Value::Bool(true))]);
    // SAFETY: the instance and host remain live for the callback.
    assert_eq!(unsafe { (script.destroy)(instance, &host) }, RveStatus::Ok);
    assert_eq!(
        state.logs,
        ["guest update", "message:sender:greet", "guest destroy"]
    );
    // SAFETY: drop is called exactly once after successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);

    for (parameter, callback, expected) in [
        ("panic_update", 0, "update panic"),
        ("panic_message", 1, "message panic"),
        ("panic_destroy", 2, "destroy panic"),
    ] {
        state.error = None;
        let entry = [RveParamEntry {
            key: RveStr::from_static(parameter),
            value: RveValue::boolean(true),
        }];
        let params = RveParams {
            entries: entry.as_ptr(),
            len: entry.len(),
        };
        // SAFETY: params and host remain live through create.
        assert_eq!(
            unsafe { (script.create)(&host, &params, &mut instance) },
            RveStatus::Ok
        );
        let status = match callback {
            0 => {
                // SAFETY: the instance and host remain live for this callback.
                unsafe { (script.update)(instance, &host, 0.25) }
            }
            1 => {
                // SAFETY: message data and host remain live for this callback.
                unsafe {
                    (script.message)(
                        instance,
                        &host,
                        RveStr::from_static("sender"),
                        RveStr::from_static("greet"),
                        &RveValue::integer(1),
                    )
                }
            }
            _ => {
                // SAFETY: the instance and host remain live for this callback.
                unsafe { (script.destroy)(instance, &host) }
            }
        };
        assert_eq!(status, RveStatus::Panicked);
        assert_eq!(state.error.as_deref(), Some(expected));
        // SAFETY: drop is called exactly once after each successful create.
        assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);
    }
}

#[test]
fn borrowed_abi_values_and_params_become_owned_values() {
    let children = [RveValue::integer(-9), RveValue::string("value")];
    let nested = [RveValue {
        tag: RVE_VALUE_ARRAY,
        data: RveValueData {
            array: crate::abi::RveArray {
                ptr: children.as_ptr(),
                len: children.len(),
            },
        },
    }];
    let entries = [
        RveParamEntry {
            key: RveStr::from_static("enabled"),
            value: RveValue::boolean(true),
        },
        RveParamEntry {
            key: RveStr::from_static("ratio"),
            value: RveValue::float(1.25),
        },
        RveParamEntry {
            key: RveStr::from_static("nested"),
            value: RveValue::array(&nested),
        },
    ];
    let params = RveParams {
        entries: entries.as_ptr(),
        len: entries.len(),
    };
    // SAFETY: all pointers refer to the live arrays and static string keys above.
    let owned = unsafe { params.into_owned() }.unwrap();
    assert_eq!(owned["enabled"], Value::Bool(true));
    assert_eq!(owned["ratio"], Value::Float(1.25));
    assert_eq!(
        owned["nested"],
        Value::Array(vec![Value::Array(vec![
            Value::Int(-9),
            Value::String("value".to_owned())
        ])])
    );
}

#[test]
fn malformed_borrowed_values_and_duplicate_keys_are_rejected() {
    let invalid_utf8 = [0xff];
    let invalid_string = RveValue {
        tag: RVE_VALUE_STR,
        data: RveValueData {
            string: RveStr {
                ptr: invalid_utf8.as_ptr(),
                len: invalid_utf8.len(),
            },
        },
    };
    // SAFETY: the byte array is valid storage; its contents are intentionally invalid UTF-8.
    assert!(
        unsafe { invalid_string.into_owned() }
            .unwrap_err()
            .0
            .contains("not UTF-8")
    );

    let null_array = RveValue {
        tag: RVE_VALUE_ARRAY,
        data: RveValueData {
            array: RveArray {
                ptr: std::ptr::null(),
                len: 1,
            },
        },
    };
    // SAFETY: the converter detects the null pointer before constructing a slice.
    assert_eq!(
        unsafe { null_array.into_owned() }.unwrap_err().0,
        "null array values"
    );

    let unknown_tag = RveValue {
        tag: u32::MAX,
        data: RveValueData { integer: 0 },
    };
    // SAFETY: unknown tags are rejected before any union field is read.
    assert_eq!(
        unsafe { unknown_tag.into_owned() }.unwrap_err().0,
        "unknown value tag 4294967295"
    );

    let duplicates = [
        RveParamEntry {
            key: RveStr::from_static("same"),
            value: RveValue::integer(1),
        },
        RveParamEntry {
            key: RveStr::from_static("same"),
            value: RveValue::integer(2),
        },
    ];
    let params = RveParams {
        entries: duplicates.as_ptr(),
        len: duplicates.len(),
    };
    // SAFETY: entries and all referenced strings remain live for conversion.
    assert_eq!(
        unsafe { params.into_owned() }.unwrap_err().0,
        "duplicate parameter key \"same\""
    );
}

mod with_params {
    use crate::{Params, Script};

    struct Empty;
    impl Script for Empty {}

    fn factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Empty))
    }

    pub(super) fn module() -> &'static crate::abi::RveModule {
        crate::script_module! {
            "Typed" => factory {
                enabled: bool = true "Enabled",
                count: i64 = 3 (0..=10),
                amplitude: f32 = 0.15,
                speed: f64 = -1.5 (-10.0..=10.0) "Speed",
                title: &str = "hello",
                axis: [f32; 3] = [0.0, 1, -0.5],
            },
            "Plain" => factory,
        }
    }
}

#[test]
fn script_module_macro_exports_typed_parameter_tables() {
    use crate::abi::{
        RVE_PARAM_BOOL, RVE_PARAM_FLOAT, RVE_PARAM_HAS_MAX, RVE_PARAM_HAS_MIN, RVE_PARAM_INT,
        RVE_PARAM_STRING, RVE_PARAM_VEC3, RveParamDesc,
    };
    let module = with_params::module();
    let plain = descriptor(module, "Plain");
    assert_eq!(plain.param_count, 0);

    let typed = descriptor(module, "Typed");
    // SAFETY: the macro builds a process-lifetime table of `param_count` descriptors.
    let params: &[RveParamDesc] =
        unsafe { std::slice::from_raw_parts(typed.params, typed.param_count) };
    let text = |value: RveStr| unsafe { value.to_str() }.unwrap().to_owned();
    let summary: Vec<_> = params
        .iter()
        .map(|param| (text(param.name), param.kind, param.flags, text(param.label)))
        .collect();
    assert_eq!(
        summary,
        [
            ("enabled".into(), RVE_PARAM_BOOL, 0, "Enabled".into()),
            (
                "count".into(),
                RVE_PARAM_INT,
                RVE_PARAM_HAS_MIN | RVE_PARAM_HAS_MAX,
                String::new()
            ),
            ("amplitude".into(), RVE_PARAM_FLOAT, 0, String::new()),
            (
                "speed".into(),
                RVE_PARAM_FLOAT,
                RVE_PARAM_HAS_MIN | RVE_PARAM_HAS_MAX,
                "Speed".into()
            ),
            ("title".into(), RVE_PARAM_STRING, 0, String::new()),
            ("axis".into(), RVE_PARAM_VEC3, 0, String::new()),
        ]
    );
    // SAFETY: defaults are static values built by the macro.
    let defaults: Vec<_> = params
        .iter()
        .map(|param| unsafe { param.default.into_owned() }.unwrap())
        .collect();
    assert_eq!(
        defaults,
        [
            Value::Bool(true),
            Value::Int(3),
            Value::Float(0.15),
            Value::Float(-1.5),
            Value::String("hello".into()),
            Value::Array(vec![
                Value::Float(0.0),
                Value::Float(1.0),
                Value::Float(-0.5)
            ]),
        ]
    );
    assert_eq!((params[1].min, params[1].max), (0.0, 10.0));
    assert_eq!((params[3].min, params[3].max), (-10.0, 10.0));
}

#[test]
fn param_descriptor_layout_is_c_compatible() {
    use crate::abi::{RVE_SCRIPT_ABI_MIN_VERSION, RveParamDesc, RveScriptDescV3, RveScriptDescV4};
    use std::mem::{align_of, offset_of, size_of};
    let pointer = size_of::<usize>();
    assert_eq!(offset_of!(RveParamDesc, name), 0);
    assert_eq!(offset_of!(RveParamDesc, kind), 2 * pointer);
    assert_eq!(
        offset_of!(RveParamDesc, default) % align_of::<RveValue>(),
        0
    );
    assert_eq!(
        align_of::<RveParamDesc>(),
        align_of::<RveValue>().max(align_of::<f64>())
    );
    assert_eq!(size_of::<RveParamDesc>() % align_of::<RveParamDesc>(), 0);
    assert_eq!(
        offset_of!(RveScriptDesc, param_count),
        offset_of!(RveScriptDesc, params) + pointer
    );
    assert_eq!(
        offset_of!(RveScriptDesc, command),
        size_of::<RveScriptDescV3>()
    );
    assert_eq!(
        offset_of!(RveScriptDescV4, command),
        size_of::<RveScriptDescV3>()
    );
    assert_eq!(
        offset_of!(RveScriptDesc, collision),
        size_of::<RveScriptDescV4>()
    );
    assert_eq!(RVE_SCRIPT_ABI_VERSION, 8);
    assert_eq!(RVE_SCRIPT_ABI_MIN_VERSION, 3);
    assert_eq!(crate::abi::RVE_INPUT_MAX_BINDINGS, 16);
    assert_eq!(
        offset_of!(RveHost, register_command),
        offset_of!(RveHost, set_active_camera)
            + size_of::<Option<crate::abi::RveSetActiveCameraFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, action_state),
        offset_of!(RveHost, unregister_command)
            + size_of::<Option<crate::abi::RveUnregisterCommandFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, action_value),
        offset_of!(RveHost, action_state) + size_of::<Option<crate::abi::RveActionStateFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, input_context),
        offset_of!(RveHost, action_value) + size_of::<Option<crate::abi::RveActionValueFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, set_input_context),
        offset_of!(RveHost, input_context) + size_of::<Option<crate::abi::RveInputContextFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, rebind_action),
        offset_of!(RveHost, set_input_context)
            + size_of::<Option<crate::abi::RveSetInputContextFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, action_binding),
        offset_of!(RveHost, rebind_action) + size_of::<Option<crate::abi::RveRebindActionFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, reset_bindings),
        offset_of!(RveHost, action_binding) + size_of::<Option<crate::abi::RveActionBindingFn>>()
    );
    assert_eq!(
        offset_of!(RveHost, raycast),
        offset_of!(RveHost, reset_bindings) + size_of::<Option<crate::abi::RveResetBindingsFn>>()
    );
}
