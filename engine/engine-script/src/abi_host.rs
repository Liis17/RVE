use crate::abi_string::read_abi_str;
use crate::actions::ActionMutation;
use crate::lite::{
    CommandGeneration, EventQueue, LuaCommands, MAX_LOG_MESSAGE_BYTES, RegisteredCommandCallback,
    RegisteredLuaCommand,
};
#[cfg(test)]
use crate::transaction::ScriptTransactionInit;
use crate::transaction::{
    MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_TARGET_ID_BYTES, MessageValueBudget, QueueMessageError,
    ScriptTransaction,
};
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptTime, ScriptTransform};
use engine_console::CommandSpec;
use engine_files::VirtualPath;
use engine_physics::Ray;
use engine_project::InputActionKind;
use engine_scene::{
    AntiAliasing, AudioGroup, AudioRequest, CameraColorSettings, CameraRenderSettings,
    CameraSettings, ClipParams, ObjectHandle, ParamValue, ProjectionSettings, Tonemapper, VoiceId,
};
use glam::Vec3;
use rve_sdk::PhysicalInput;
use rve_sdk::abi::{
    RVE_ACTION_AXIS, RVE_ACTION_BUTTON, RVE_ACTION_VECTOR2, RVE_ANTI_ALIASING_DLSS,
    RVE_ANTI_ALIASING_FSR, RVE_ANTI_ALIASING_FXAA, RVE_ANTI_ALIASING_NONE, RVE_ANTI_ALIASING_TAA,
    RVE_INPUT_DOWN, RVE_INPUT_MAX_BINDINGS, RVE_INPUT_PRESSED, RVE_INPUT_RELEASED, RVE_LOG_ERROR,
    RVE_LOG_INFO, RVE_LOG_WARN, RVE_TONEMAPPER_ACES, RVE_TONEMAPPER_NEUTRAL, RVE_TONEMAPPER_NONE,
    RVE_VALUE_ARRAY, RVE_VALUE_BOOL, RVE_VALUE_FLOAT, RVE_VALUE_INT, RVE_VALUE_STR, RveArray,
    RveCameraSettings, RveCharacterMoveResult, RveCollisionEvent, RveCursorState, RveHost,
    RveParamEntry, RveParams, RveRaycastHit, RveScriptDesc, RveStatus, RveStr, RveTime,
    RveTransform, RveValue, RveValueData,
};
use rve_sdk::abi::{
    RVE_AUDIO_GROUP_AMBIENT, RVE_AUDIO_GROUP_EFFECTS, RVE_AUDIO_GROUP_MASTER,
    RVE_AUDIO_GROUP_MUSIC, RveAudioParams,
};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) struct AbiCallContext<'a> {
    self_object: u64,
    transaction: &'a mut ScriptTransaction,
    events: EventQueue,
    source: &'a str,
    commands: LuaCommands,
    commands_generation: CommandGeneration,
    command_owner: usize,
    commands_allowed: bool,
    error: Option<String>,
    action_binding: String,
    poisoned: bool,
}

pub(crate) struct AbiScriptCall<'a> {
    pub(crate) self_object: u64,
    pub(crate) transaction: &'a mut ScriptTransaction,
    pub(crate) events: &'a EventQueue,
    pub(crate) source: &'a str,
    pub(crate) commands: &'a LuaCommands,
    pub(crate) commands_generation: &'a CommandGeneration,
    pub(crate) command_owner: usize,
    pub(crate) commands_allowed: bool,
}

pub(crate) struct AbiCommandAccess<'a> {
    commands: &'a LuaCommands,
    generation: &'a CommandGeneration,
    owner: usize,
    allowed: bool,
}

impl<'a> AbiCallContext<'a> {
    pub(crate) fn new(
        self_object: u64,
        transaction: &'a mut ScriptTransaction,
        events: &EventQueue,
        source: &'a str,
    ) -> Self {
        Self {
            self_object,
            transaction,
            events: events.clone(),
            source,
            commands: std::rc::Rc::new(std::cell::RefCell::new(BTreeMap::new())),
            commands_generation: std::rc::Rc::new(std::cell::Cell::new(0)),
            command_owner: 0,
            commands_allowed: false,
            error: None,
            action_binding: String::new(),
            poisoned: false,
        }
    }

    pub(crate) fn with_commands(
        self_object: u64,
        transaction: &'a mut ScriptTransaction,
        events: &EventQueue,
        source: &'a str,
        command_access: AbiCommandAccess<'a>,
    ) -> Self {
        Self {
            self_object,
            transaction,
            events: events.clone(),
            source,
            commands: command_access.commands.clone(),
            commands_generation: command_access.generation.clone(),
            command_owner: command_access.owner,
            commands_allowed: command_access.allowed,
            error: None,
            action_binding: String::new(),
            poisoned: false,
        }
    }

    pub(crate) fn host(&mut self, time: ScriptTime) -> RveHost {
        RveHost {
            ctx: (self as *mut Self).cast(),
            time: RveTime {
                elapsed: time.elapsed,
                frame: time.frame,
            },
            self_object: self.self_object,
            set_error: host_set_error,
            log: host_log,
            find_object: host_find_object,
            object_id: host_object_id,
            object_name: host_object_name,
            get_local: host_get_local,
            set_local: host_set_local,
            world_matrix: host_world_matrix,
            send: host_send,
            broadcast: host_broadcast,
            key_state: host_key_state,
            mouse_button_state: host_mouse_button_state,
            cursor_position: host_cursor_position,
            mouse_delta: host_mouse_delta,
            wheel_delta: host_wheel_delta,
            get_camera_settings: Some(host_get_camera_settings),
            set_camera_settings: Some(host_set_camera_settings),
            active_camera: Some(host_active_camera),
            set_active_camera: Some(host_set_active_camera),
            register_command: self.commands_allowed.then_some(host_register_command),
            unregister_command: self.commands_allowed.then_some(host_unregister_command),
            raycast: self
                .transaction
                .physics_attached()
                .then_some(host_physics_raycast),
            get_velocity: self
                .transaction
                .physics_attached()
                .then_some(host_physics_get_velocity),
            set_velocity: self
                .transaction
                .physics_attached()
                .then_some(host_physics_set_velocity),
            add_impulse: self
                .transaction
                .physics_attached()
                .then_some(host_physics_add_impulse),
            add_force: self
                .transaction
                .physics_attached()
                .then_some(host_physics_add_force),
            move_character: self
                .transaction
                .physics_attached()
                .then_some(host_physics_move_character),
            is_grounded: self
                .transaction
                .physics_attached()
                .then_some(host_physics_is_grounded),
            set_collider_enabled: self
                .transaction
                .physics_attached()
                .then_some(host_physics_set_collider_enabled),
            is_collider_enabled: self
                .transaction
                .physics_attached()
                .then_some(host_physics_is_collider_enabled),
            action_state: Some(host_action_state),
            action_value: Some(host_action_value),
            input_context: Some(host_input_context),
            set_input_context: Some(host_set_input_context),
            rebind_action: Some(host_rebind_action),
            action_binding: Some(host_action_binding),
            reset_bindings: Some(host_reset_bindings),
            audio_play_source: Some(host_audio_play_source),
            audio_stop_source: Some(host_audio_stop_source),
            audio_set_source_volume: Some(host_audio_set_source_volume),
            audio_play_clip: Some(host_audio_play_clip),
            audio_stop_voice: Some(host_audio_stop_voice),
            audio_set_group_volume: Some(host_audio_set_group_volume),
            cursor_state: Some(host_cursor_state),
            set_cursor_visible: Some(host_set_cursor_visible),
            set_cursor_locked: Some(host_set_cursor_locked),
        }
    }

    pub(crate) fn callback_result(&mut self, status: RveStatus) -> Result<(), String> {
        if self.poisoned {
            return Err("host callback panicked; ABI call is poisoned".to_owned());
        }
        if status != RveStatus::Ok {
            return Err(self.error.take().unwrap_or_else(|| match status {
                RveStatus::Error => "ABI script callback returned Error".to_owned(),
                RveStatus::Panicked => "ABI script callback panicked".to_owned(),
                RveStatus::Ok => unreachable!(),
            }));
        }
        Ok(())
    }

    fn command_result(&mut self, status: RveStatus) -> Result<(), (String, bool)> {
        if self.poisoned {
            return Err((
                "host callback panicked; ABI call is poisoned".to_owned(),
                true,
            ));
        }
        if status != RveStatus::Ok {
            return Err((
                self.error.take().unwrap_or_else(|| match status {
                    RveStatus::Error => "ABI command callback returned Error".to_owned(),
                    RveStatus::Panicked => "ABI command callback panicked".to_owned(),
                    RveStatus::Ok => unreachable!(),
                }),
                status == RveStatus::Panicked,
            ));
        }
        Ok(())
    }
}

pub(crate) struct AbiScriptInstance {
    instance: *mut c_void,
    descriptor: &'static RveScriptDesc,
    events: EventQueue,
    source: String,
}

impl AbiScriptInstance {
    pub(crate) fn create(
        descriptor: &'static RveScriptDesc,
        params: &BTreeMap<String, ParamValue>,
        call: AbiScriptCall<'_>,
    ) -> Result<Self, String> {
        let params = OwnedAbiParams::new(params);
        let mut context =
            AbiCallContext::new(call.self_object, call.transaction, call.events, call.source);
        let mut instance = std::ptr::null_mut();
        let status = {
            let host = context.host(ScriptTime::default());
            // SAFETY: callback context and borrowed parameters remain live through create.
            unsafe { (descriptor.create)(&host, &params.abi, &mut instance) }
        };
        let poisoned = context.poisoned;
        let error = context.error.take();
        drop(context);
        if poisoned {
            if status == RveStatus::Ok && !instance.is_null() {
                drop(Self {
                    instance,
                    descriptor,
                    events: call.events.clone(),
                    source: call.source.to_owned(),
                });
            }
            return Err("host callback panicked during create".to_owned());
        }
        if status != RveStatus::Ok {
            return Err(error.unwrap_or_else(|| match status {
                RveStatus::Error => "ABI create returned Error".to_owned(),
                RveStatus::Panicked => "ABI create panicked".to_owned(),
                RveStatus::Ok => unreachable!(),
            }));
        }
        if instance.is_null() {
            return Err("ABI create returned Ok with a null instance".to_owned());
        }
        Ok(Self {
            instance,
            descriptor,
            events: call.events.clone(),
            source: call.source.to_owned(),
        })
    }

    pub(crate) fn invoke(
        &mut self,
        phase: AbiPhase,
        time: ScriptTime,
        call: AbiScriptCall<'_>,
    ) -> Result<(), String> {
        let command_access = AbiCommandAccess {
            commands: call.commands,
            generation: call.commands_generation,
            owner: call.command_owner,
            allowed: call.commands_allowed && self.descriptor.command.is_some(),
        };
        let mut context = AbiCallContext::with_commands(
            call.self_object,
            call.transaction,
            call.events,
            call.source,
            command_access,
        );
        let status = {
            let host = context.host(time);
            // SAFETY: this instance came from its matching descriptor's successful create call.
            unsafe {
                match phase {
                    AbiPhase::Start => (self.descriptor.start)(self.instance, &host),
                    AbiPhase::Update(dt) => (self.descriptor.update)(self.instance, &host, dt),
                    AbiPhase::Destroy => (self.descriptor.destroy)(self.instance, &host),
                }
            }
        };
        context.callback_result(status)
    }

    pub(crate) fn invoke_message(
        &mut self,
        time: ScriptTime,
        sender: &str,
        name: &str,
        value: &ParamValue,
        call: AbiScriptCall<'_>,
    ) -> Result<(), String> {
        let value = OwnedAbiValue::new(value);
        let sender = RveStr::from_str_ref(sender);
        let name = RveStr::from_str_ref(name);
        let command_access = AbiCommandAccess {
            commands: call.commands,
            generation: call.commands_generation,
            owner: call.command_owner,
            allowed: call.commands_allowed && self.descriptor.command.is_some(),
        };
        let mut context = AbiCallContext::with_commands(
            call.self_object,
            call.transaction,
            call.events,
            call.source,
            command_access,
        );
        let status = {
            let host = context.host(time);
            // SAFETY: message data, instance, and host remain live throughout the callback.
            unsafe { (self.descriptor.message)(self.instance, &host, sender, name, &value.value) }
        };
        context.callback_result(status)
    }

    pub(crate) fn invoke_physics_event(
        &mut self,
        time: ScriptTime,
        kind: u32,
        other: u64,
        point: [f32; 3],
        normal: [f32; 3],
        call: AbiScriptCall<'_>,
    ) -> Result<(), String> {
        let Some(callback) = self.descriptor.collision else {
            return Ok(());
        };
        let command_access = AbiCommandAccess {
            commands: call.commands,
            generation: call.commands_generation,
            owner: call.command_owner,
            allowed: call.commands_allowed && self.descriptor.command.is_some(),
        };
        let mut context = AbiCallContext::with_commands(
            call.self_object,
            call.transaction,
            call.events,
            call.source,
            command_access,
        );
        let event = RveCollisionEvent {
            kind,
            other,
            point,
            normal,
        };
        let status = {
            let host = context.host(time);
            // SAFETY: the event, instance, and host remain live throughout the callback.
            unsafe { callback(self.instance, &host, event) }
        };
        context.callback_result(status)
    }

    pub(crate) fn invoke_command(
        &mut self,
        time: ScriptTime,
        name: &str,
        args: &[String],
        call: AbiScriptCall<'_>,
    ) -> Result<(), (String, bool)> {
        let Some(callback) = self.descriptor.command else {
            return Err(("ABI script has no command callback".to_owned(), false));
        };
        let args: Vec<_> = args.iter().map(|arg| RveStr::from_str_ref(arg)).collect();
        let command_access = AbiCommandAccess {
            commands: call.commands,
            generation: call.commands_generation,
            owner: call.command_owner,
            allowed: true,
        };
        let mut context = AbiCallContext::with_commands(
            call.self_object,
            call.transaction,
            call.events,
            call.source,
            command_access,
        );
        let status = {
            let host = context.host(time);
            // SAFETY: name and arguments are borrowed for the duration of the synchronous callback.
            unsafe {
                callback(
                    self.instance,
                    &host,
                    RveStr::from_str_ref(name),
                    args.as_ptr(),
                    args.len(),
                )
            }
        };
        context.command_result(status)
    }
}

impl Drop for AbiScriptInstance {
    fn drop(&mut self) {
        // SAFETY: this instance owns a successful create result and Drop runs exactly once.
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            (self.descriptor.drop)(self.instance)
        }));
        let message = match result {
            Ok(RveStatus::Ok) => return,
            Ok(RveStatus::Error) => "ABI script drop returned Error".to_owned(),
            Ok(RveStatus::Panicked) => "ABI script drop panicked".to_owned(),
            Err(_) => "ABI script drop unwound across its callback".to_owned(),
        };
        if let Ok(mut events) = self.events.try_borrow_mut() {
            events.push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: self.source.clone(),
                message,
            });
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AbiPhase {
    Start,
    Update(f32),
    Destroy,
}

fn guarded<T: Copy>(
    ctx: *mut c_void,
    fallback: T,
    callback: impl FnOnce(&mut AbiCallContext<'_>) -> T,
) -> T {
    if ctx.is_null() {
        return fallback;
    }
    // SAFETY: the ABI host passes a live call context for the duration of each host callback.
    let result = catch_unwind(AssertUnwindSafe(|| {
        callback(unsafe { &mut *ctx.cast::<AbiCallContext<'_>>() })
    }));
    match result {
        Ok(value) => value,
        Err(_) => {
            // SAFETY: catch_unwind has ended the temporary mutable borrow above.
            unsafe { (*ctx.cast::<AbiCallContext<'_>>()).poisoned = true };
            fallback
        }
    }
}

unsafe extern "C" fn host_set_error(ctx: *mut c_void, message: RveStr) {
    guarded(ctx, (), |context| {
        context.error = Some(
            unsafe { read_abi_str(message) }
                .map(str::to_owned)
                .unwrap_or_else(|_| "guest supplied an invalid error message".to_owned()),
        );
    });
}

unsafe extern "C" fn host_log(ctx: *mut c_void, level: u32, message: RveStr) {
    guarded(ctx, (), |context| {
        if !context.events.borrow().has_log_capacity() {
            context.events.borrow_mut().record_dropped_log();
            return;
        }
        let message = unsafe { read_abi_log_message(message) };
        #[cfg(test)]
        if message == "__rve_test_host_log_panic__" {
            panic!("test host log panic");
        }
        let level = match level {
            RVE_LOG_INFO => ScriptEventLevel::Info,
            RVE_LOG_WARN => ScriptEventLevel::Warn,
            RVE_LOG_ERROR => ScriptEventLevel::Error,
            _ => ScriptEventLevel::Error,
        };
        context.events.borrow_mut().push_log(ScriptEvent {
            level,
            source: context.source.to_owned(),
            message,
        });
    });
}

/// Reads an ABI log message with bounded copying; oversized messages are truncated with a marker.
///
/// # Safety
/// For nonzero lengths, `value.ptr` must refer to readable storage for `value.len` bytes.
unsafe fn read_abi_log_message(value: RveStr) -> String {
    if value.len <= MAX_LOG_MESSAGE_BYTES {
        return unsafe { read_abi_str(value) }
            .map(str::to_owned)
            .unwrap_or_else(|_| "guest supplied an invalid log message".to_owned());
    }
    if value.ptr.is_null() {
        return "guest supplied an invalid log message".to_owned();
    }

    let content_limit = MAX_LOG_MESSAGE_BYTES - "…".len();
    // SAFETY: the caller guarantees readable storage for value.len and content_limit is smaller.
    let prefix = unsafe { std::slice::from_raw_parts(value.ptr.cast::<u8>(), content_limit) };
    match std::str::from_utf8(prefix) {
        Ok(message) => format!("{message}…"),
        Err(error) if error.error_len().is_none() => {
            let valid_prefix = std::str::from_utf8(&prefix[..error.valid_up_to()])
                .expect("valid_up_to is the valid UTF-8 prefix");
            format!("{valid_prefix}…")
        }
        Err(_) => "guest supplied an invalid log message".to_owned(),
    }
}

unsafe extern "C" fn host_find_object(ctx: *mut c_void, id: RveStr) -> u64 {
    guarded(ctx, 0, |context| {
        let Ok(id) = (unsafe { read_abi_str(id) }) else {
            return 0;
        };
        context
            .transaction
            .handle_by_id(id)
            .and_then(|handle| context.transaction.abi_handle(handle))
            .unwrap_or(0)
    })
}

unsafe extern "C" fn host_object_id(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(id) = context.transaction.object_id(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(id) };
        true
    })
}

unsafe extern "C" fn host_object_name(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(name) = context.transaction.object_name(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(name) };
        true
    })
}

unsafe extern "C" fn host_get_local(ctx: *mut c_void, object: u64, out: *mut RveTransform) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(transform) = context.transaction.local_transform(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe {
            *out = RveTransform {
                position: transform.position,
                rotation_deg: transform.rotation,
                scale: transform.scale,
            }
        };
        true
    })
}

unsafe extern "C" fn host_set_local(
    ctx: *mut c_void,
    object: u64,
    transform: *const RveTransform,
) -> bool {
    guarded(ctx, false, |context| {
        if transform.is_null() {
            return false;
        }
        // SAFETY: transform was checked non-null and is valid for this callback.
        let transform = unsafe { *transform };
        let transform = ScriptTransform {
            position: transform.position,
            rotation: transform.rotation_deg,
            scale: transform.scale,
        };
        context.transaction.set_local_from_abi(object, transform)
    })
}

unsafe extern "C" fn host_world_matrix(ctx: *mut c_void, object: u64, out: *mut [f32; 16]) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(matrix) = context.transaction.world_matrix(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = matrix };
        true
    })
}

unsafe extern "C" fn host_get_camera_settings(
    ctx: *mut c_void,
    object: u64,
    out: *mut RveCameraSettings,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(settings) = context.transaction.camera_settings(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = camera_settings_to_abi(settings) };
        true
    })
}

unsafe extern "C" fn host_set_camera_settings(
    ctx: *mut c_void,
    object: u64,
    settings: *const RveCameraSettings,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() || settings.is_null() {
            return false;
        }
        // SAFETY: settings was checked non-null and is valid for this callback.
        let Some(settings) = camera_settings_from_abi(unsafe { *settings }) else {
            return false;
        };
        context
            .transaction
            .set_camera_settings_from_abi(object, settings)
    })
}

unsafe extern "C" fn host_active_camera(ctx: *mut c_void) -> u64 {
    guarded(ctx, 0, |context| {
        context
            .transaction
            .active_camera()
            .and_then(|handle| context.transaction.abi_handle(handle))
            .unwrap_or(0)
    })
}

unsafe extern "C" fn host_set_active_camera(ctx: *mut c_void, object: u64) -> bool {
    guarded(ctx, false, |context| {
        context.transaction.set_active_camera_from_abi(object)
    })
}

unsafe extern "C" fn host_physics_raycast(
    ctx: *mut c_void,
    origin: *const [f32; 3],
    direction: *const [f32; 3],
    max_distance: f32,
    include_triggers: bool,
    ignore: u64,
    out: *mut RveRaycastHit,
) -> bool {
    guarded(ctx, false, |context| {
        if origin.is_null() || direction.is_null() || out.is_null() {
            context.error = Some("raycast received a null vector or output pointer".to_owned());
            return false;
        }
        // SAFETY: non-null input pointers must be readable for this synchronous callback.
        let origin = Vec3::from_array(unsafe { *origin });
        // SAFETY: non-null input pointers must be readable for this synchronous callback.
        let direction = Vec3::from_array(unsafe { *direction });
        let ray = match Ray::new(origin, direction, max_distance) {
            Ok(ray) => ray,
            Err(error) => {
                context.error = Some(error.to_string());
                return false;
            }
        };
        let ignore = if ignore == 0 {
            None
        } else {
            let Some(handle) = context.transaction.object_from_abi_handle(ignore) else {
                context.error = Some("raycast ignore handle is unknown".to_owned());
                return false;
            };
            Some(handle)
        };
        let hit = match context
            .transaction
            .physics_raycast(ray, include_triggers, ignore)
        {
            Ok(hit) => hit,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        let mut result = RveRaycastHit::default();
        if let Some(hit) = hit {
            let Some(object) = context.transaction.physics_object(hit.body) else {
                context.error = Some("raycast hit has no scene object".to_owned());
                return false;
            };
            let Some(object) = context.transaction.abi_handle(object) else {
                context.error =
                    Some("raycast object handle cannot be represented by the ABI".to_owned());
                return false;
            };
            result = RveRaycastHit {
                object,
                point: hit.point.to_array(),
                normal: hit.normal.to_array(),
                distance: hit.distance,
            };
        }
        // SAFETY: out was checked non-null and is writable for this synchronous callback.
        unsafe { *out = result };
        true
    })
}

unsafe extern "C" fn host_physics_get_velocity(
    ctx: *mut c_void,
    object: u64,
    value: *mut [f32; 3],
) -> bool {
    guarded(ctx, false, |context| {
        if value.is_null() {
            context.error = Some("get_velocity received a null output pointer".to_owned());
            return false;
        }
        let Some(object) = context.transaction.object_from_abi_handle(object) else {
            context.error = Some("get_velocity received an unknown object handle".to_owned());
            return false;
        };
        let velocity = match context.transaction.body_velocity(object) {
            Ok(velocity) => velocity,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        // SAFETY: value was checked non-null and is writable for this synchronous callback.
        unsafe { *value = velocity.to_array() };
        true
    })
}

unsafe extern "C" fn host_physics_set_velocity(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    host_physics_vector_command(ctx, object, value, |transaction, object, value| {
        transaction.set_body_velocity(object, value)
    })
}

unsafe extern "C" fn host_physics_add_impulse(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    host_physics_vector_command(ctx, object, value, |transaction, object, value| {
        transaction.add_body_impulse(object, value)
    })
}

unsafe extern "C" fn host_physics_add_force(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    host_physics_vector_command(ctx, object, value, |transaction, object, value| {
        transaction.add_body_force(object, value)
    })
}

fn host_physics_vector_command(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
    command: impl FnOnce(&mut ScriptTransaction, engine_scene::ObjectHandle, Vec3) -> Result<(), String>,
) -> bool {
    guarded(ctx, false, |context| {
        if value.is_null() {
            context.error = Some("physics command received a null vector pointer".to_owned());
            return false;
        }
        // SAFETY: value must point to three readable floats for this synchronous callback.
        let value = Vec3::from_array(unsafe { *value });
        let Some(object) = context.transaction.object_from_abi_handle(object) else {
            context.error = Some("physics command received an unknown object handle".to_owned());
            return false;
        };
        match command(context.transaction, object, value) {
            Ok(()) => true,
            Err(error) => {
                context.error = Some(error);
                false
            }
        }
    })
}

unsafe extern "C" fn host_physics_move_character(
    ctx: *mut c_void,
    object: u64,
    displacement: *const [f32; 3],
    out: *mut RveCharacterMoveResult,
) -> bool {
    guarded(ctx, false, |context| {
        if displacement.is_null() || out.is_null() {
            context.error =
                Some("move_character received a null vector or output pointer".to_owned());
            return false;
        }
        // SAFETY: displacement is readable for this synchronous callback.
        let displacement = Vec3::from_array(unsafe { *displacement });
        let Some(object) = context.transaction.object_from_abi_handle(object) else {
            context.error = Some("move_character received an unknown object handle".to_owned());
            return false;
        };
        let movement = match context.transaction.move_character(object, displacement) {
            Ok(movement) => movement,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        // SAFETY: out was checked non-null and is writable for this synchronous callback.
        unsafe {
            *out = RveCharacterMoveResult {
                translation: movement.translation.to_array(),
                grounded: movement.grounded,
                collided: !movement.hits.is_empty(),
            }
        };
        true
    })
}

unsafe extern "C" fn host_physics_is_grounded(
    ctx: *mut c_void,
    object: u64,
    value: *mut bool,
) -> bool {
    host_physics_get_bool(ctx, object, value, |transaction, object| {
        transaction.is_grounded(object)
    })
}

unsafe extern "C" fn host_physics_is_collider_enabled(
    ctx: *mut c_void,
    object: u64,
    value: *mut bool,
) -> bool {
    host_physics_get_bool(ctx, object, value, |transaction, object| {
        transaction.collider_enabled(object)
    })
}

fn host_physics_get_bool(
    ctx: *mut c_void,
    object: u64,
    value: *mut bool,
    get: impl FnOnce(&ScriptTransaction, engine_scene::ObjectHandle) -> Result<bool, String>,
) -> bool {
    guarded(ctx, false, |context| {
        if value.is_null() {
            context.error = Some("physics getter received a null output pointer".to_owned());
            return false;
        }
        let Some(object) = context.transaction.object_from_abi_handle(object) else {
            context.error = Some("physics getter received an unknown object handle".to_owned());
            return false;
        };
        let result = match get(context.transaction, object) {
            Ok(result) => result,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        // SAFETY: value was checked non-null and is writable for this synchronous callback.
        unsafe { *value = result };
        true
    })
}

unsafe extern "C" fn host_physics_set_collider_enabled(
    ctx: *mut c_void,
    object: u64,
    value: bool,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(object) = context.transaction.object_from_abi_handle(object) else {
            context.error =
                Some("set_collider_enabled received an unknown object handle".to_owned());
            return false;
        };
        match context.transaction.set_collider_enabled(object, value) {
            Ok(()) => true,
            Err(error) => {
                context.error = Some(error);
                false
            }
        }
    })
}

/// Converts an audio callback result to `RveStatus`. The reason is also logged at once, because a
/// guest that ignores the status would otherwise lose it.
fn audio_result(context: &mut AbiCallContext<'_>, result: Result<(), String>) -> u32 {
    match result {
        Ok(()) => RveStatus::Ok as u32,
        Err(message) => {
            context.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: context.source.to_owned(),
                message: message.clone(),
            });
            context.error = Some(message);
            RveStatus::Error as u32
        }
    }
}

/// Resolves an object for `play_sound`-style calls; the object must have an `audio_source`.
fn audio_source_object(
    context: &AbiCallContext<'_>,
    object: u64,
    name: &str,
) -> Result<ObjectHandle, String> {
    let handle = context
        .transaction
        .object_from_abi_handle(object)
        .ok_or_else(|| format!("{name} received an unknown object handle"))?;
    if !context.transaction.has_audio_source(handle) {
        return Err(format!("{name} requires an audio_source component"));
    }
    Ok(handle)
}

fn audio_volume(volume: f32) -> Result<f32, String> {
    if volume.is_finite() && (0.0..=engine_scene::MAX_AUDIO_VOLUME).contains(&volume) {
        Ok(volume)
    } else {
        Err(format!(
            "audio volume must be finite and from 0 to {}",
            engine_scene::MAX_AUDIO_VOLUME
        ))
    }
}

fn audio_group(code: u32) -> Result<AudioGroup, String> {
    match code {
        RVE_AUDIO_GROUP_MASTER => Ok(AudioGroup::Master),
        RVE_AUDIO_GROUP_MUSIC => Ok(AudioGroup::Music),
        RVE_AUDIO_GROUP_EFFECTS => Ok(AudioGroup::Effects),
        RVE_AUDIO_GROUP_AMBIENT => Ok(AudioGroup::Ambient),
        other => Err(format!("unknown audio group code {other}")),
    }
}

fn clip_params(params: &RveAudioParams) -> Result<ClipParams, String> {
    let group = audio_group(params.group)?;
    if group == AudioGroup::Master {
        return Err("clips cannot use the master audio group".to_owned());
    }
    let position = if params.has_position {
        if !params.position.iter().all(|axis| axis.is_finite()) {
            return Err("clip position must be finite".to_owned());
        }
        Some(params.position)
    } else {
        None
    };
    Ok(ClipParams {
        group,
        volume: audio_volume(params.volume)?,
        looping: params.looping,
        position,
    })
}

unsafe extern "C" fn host_audio_play_source(ctx: *mut c_void, object: u64) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = audio_source_object(context, object, "play_sound").and_then(|handle| {
            context.transaction.stage_audio(AudioRequest::PlaySource {
                object: handle,
                voice: VoiceId::allocate(),
            })
        });
        audio_result(context, result)
    })
}

unsafe extern "C" fn host_audio_stop_source(ctx: *mut c_void, object: u64) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = audio_source_object(context, object, "stop_sound").and_then(|handle| {
            context
                .transaction
                .stage_audio(AudioRequest::StopSource { object: handle })
        });
        audio_result(context, result)
    })
}

unsafe extern "C" fn host_audio_set_source_volume(
    ctx: *mut c_void,
    object: u64,
    volume: f32,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = audio_source_object(context, object, "set_sound_volume").and_then(|handle| {
            let volume = audio_volume(volume)?;
            context
                .transaction
                .stage_audio(AudioRequest::SetSourceVolume {
                    object: handle,
                    volume,
                })
        });
        audio_result(context, result)
    })
}

unsafe extern "C" fn host_audio_play_clip(
    ctx: *mut c_void,
    path: RveStr,
    params: *const RveAudioParams,
    out_voice: *mut u64,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = stage_play_clip(context, path, params, out_voice);
        audio_result(context, result)
    })
}

fn stage_play_clip(
    context: &mut AbiCallContext<'_>,
    path: RveStr,
    params: *const RveAudioParams,
    out_voice: *mut u64,
) -> Result<(), String> {
    if params.is_null() || out_voice.is_null() {
        return Err("play_clip received a null pointer".to_owned());
    }
    // SAFETY: the guest passes a valid params pointer for this synchronous callback.
    let params = clip_params(unsafe { &*params })?;
    // SAFETY: the guest keeps the path string alive for this synchronous callback.
    let path = unsafe { read_abi_str(path) }
        .map_err(|_| "play_clip received an invalid path".to_owned())?;
    let path = VirtualPath::parse(path).map_err(|error| format!("play_clip: {error}"))?;
    let voice = VoiceId::allocate();
    context.transaction.stage_audio(AudioRequest::PlayClip {
        voice,
        path,
        params,
    })?;
    // SAFETY: out_voice was checked non-null and is writable for this callback.
    unsafe { *out_voice = voice.get() };
    Ok(())
}

unsafe extern "C" fn host_audio_stop_voice(ctx: *mut c_void, voice: u64) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = VoiceId::from_raw(voice)
            .ok_or_else(|| "stop_voice expects a nonzero voice id".to_owned())
            .and_then(|voice| {
                context
                    .transaction
                    .stage_audio(AudioRequest::StopVoice { voice })
            });
        audio_result(context, result)
    })
}

unsafe extern "C" fn host_audio_set_group_volume(ctx: *mut c_void, group: u32, volume: f32) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = audio_group(group).and_then(|group| {
            let volume = audio_volume(volume)?;
            context
                .transaction
                .stage_audio(AudioRequest::SetGroupVolume { group, volume })
        });
        audio_result(context, result)
    })
}

unsafe extern "C" fn host_register_command(
    ctx: *mut c_void,
    name: RveStr,
    description: RveStr,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        if !context.commands_allowed {
            context.error = Some("console commands are unavailable in this callback".to_owned());
            return RveStatus::Error as u32;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name,
            Err(error) => {
                context.error = Some(format!("invalid console command name: {error:?}"));
                return RveStatus::Error as u32;
            }
        };
        let description = match unsafe { read_abi_str(description) } {
            Ok(description) => description,
            Err(error) => {
                context.error = Some(format!("invalid console command description: {error:?}"));
                return RveStatus::Error as u32;
            }
        };
        let spec = match CommandSpec::new(name, description) {
            Ok(spec) => spec,
            Err(error) => {
                context.error = Some(error.to_string());
                return RveStatus::Error as u32;
            }
        };
        if context.commands.borrow().contains_key(&spec.name) {
            context.error = Some(format!(
                "console command {:?} is already registered",
                spec.name
            ));
            return RveStatus::Error as u32;
        }
        context.commands.borrow_mut().insert(
            spec.name.clone(),
            RegisteredLuaCommand {
                owner: context.command_owner,
                spec,
                callback: RegisteredCommandCallback::Abi,
            },
        );
        context
            .commands_generation
            .set(context.commands_generation.get().wrapping_add(1));
        RveStatus::Ok as u32
    })
}

unsafe extern "C" fn host_unregister_command(ctx: *mut c_void, name: RveStr) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        if !context.commands_allowed {
            context.error = Some("console commands are unavailable in this callback".to_owned());
            return RveStatus::Error as u32;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name,
            Err(error) => {
                context.error = Some(format!("invalid console command name: {error:?}"));
                return RveStatus::Error as u32;
            }
        };
        {
            let mut commands = context.commands.borrow_mut();
            let Some(command) = commands.get(name) else {
                context.error = Some(format!("unknown console command {name:?}"));
                return RveStatus::Error as u32;
            };
            if command.owner != context.command_owner {
                context.error = Some(format!(
                    "script cannot unregister command {name:?} owned by another instance"
                ));
                return RveStatus::Error as u32;
            }
            if matches!(&command.callback, RegisteredCommandCallback::Lua(_)) {
                context.error = Some(format!(
                    "script cannot unregister command {name:?} owned by another script type"
                ));
                return RveStatus::Error as u32;
            }
            commands.remove(name);
        }
        context
            .commands_generation
            .set(context.commands_generation.get().wrapping_add(1));
        RveStatus::Ok as u32
    })
}

fn camera_settings_to_abi(settings: CameraSettings) -> RveCameraSettings {
    RveCameraSettings {
        fov_deg: settings.projection.fov_deg,
        near: settings.projection.near,
        far: settings.projection.far,
        render_scale: settings.render.render_scale,
        clear_color: settings.render.clear_color,
        exposure_ev: settings.color.exposure_ev,
        anti_aliasing: match settings.render.anti_aliasing {
            AntiAliasing::Fxaa => RVE_ANTI_ALIASING_FXAA,
            AntiAliasing::None => RVE_ANTI_ALIASING_NONE,
            AntiAliasing::Taa => RVE_ANTI_ALIASING_TAA,
            AntiAliasing::Fsr => RVE_ANTI_ALIASING_FSR,
            AntiAliasing::Dlss => RVE_ANTI_ALIASING_DLSS,
        },
        tonemapper: match settings.color.tonemapper {
            Tonemapper::Aces => RVE_TONEMAPPER_ACES,
            Tonemapper::Neutral => RVE_TONEMAPPER_NEUTRAL,
            Tonemapper::None => RVE_TONEMAPPER_NONE,
        },
    }
}

fn camera_settings_from_abi(value: RveCameraSettings) -> Option<CameraSettings> {
    let anti_aliasing = match value.anti_aliasing {
        RVE_ANTI_ALIASING_FXAA => AntiAliasing::Fxaa,
        RVE_ANTI_ALIASING_NONE => AntiAliasing::None,
        RVE_ANTI_ALIASING_TAA => AntiAliasing::Taa,
        RVE_ANTI_ALIASING_FSR => AntiAliasing::Fsr,
        RVE_ANTI_ALIASING_DLSS => AntiAliasing::Dlss,
        _ => return None,
    };
    let tonemapper = match value.tonemapper {
        RVE_TONEMAPPER_ACES => Tonemapper::Aces,
        RVE_TONEMAPPER_NEUTRAL => Tonemapper::Neutral,
        RVE_TONEMAPPER_NONE => Tonemapper::None,
        _ => return None,
    };
    let settings = CameraSettings {
        projection: ProjectionSettings {
            fov_deg: value.fov_deg,
            near: value.near,
            far: value.far,
        },
        render: CameraRenderSettings {
            render_scale: value.render_scale,
            clear_color: value.clear_color,
            anti_aliasing,
        },
        color: CameraColorSettings {
            exposure_ev: value.exposure_ev,
            tonemapper,
        },
    };
    settings.validate().ok()?;
    Some(settings)
}

unsafe extern "C" fn host_send(
    ctx: *mut c_void,
    target_id: RveStr,
    name: RveStr,
    value: *const RveValue,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() {
            return false;
        }
        if target_id.len > MAX_MESSAGE_TARGET_ID_BYTES {
            return false;
        }
        let target_id = match unsafe { read_abi_str(target_id) } {
            Ok(target_id) => target_id,
            Err(error) => {
                context.error = Some(format!("invalid message target id: {error:?}"));
                return false;
            }
        };
        if name.len > MAX_MESSAGE_NAME_BYTES {
            context.error = Some(format!(
                "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
            ));
            return false;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name,
            Err(error) => {
                context.error = Some(format!("invalid message name: {error:?}"));
                return false;
            }
        };
        // SAFETY: `value` is provided by the guest for the duration of this ABI callback.
        let (value, value_size) = match unsafe { read_message_value(value) } {
            Ok(value) => value,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        match context.transaction.send(target_id, name, value, value_size) {
            Ok(()) => true,
            Err(QueueMessageError::UnknownTarget | QueueMessageError::ReadOnly) => false,
            Err(QueueMessageError::InvalidName) => {
                context.error = Some(format!(
                    "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
                ));
                false
            }
            Err(QueueMessageError::InvalidValue) => {
                context.error =
                    Some("message value is invalid or exceeds the message limits".to_owned());
                false
            }
        }
    })
}

unsafe extern "C" fn host_broadcast(
    ctx: *mut c_void,
    name: RveStr,
    value: *const RveValue,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() {
            return false;
        }
        if name.len > MAX_MESSAGE_NAME_BYTES {
            context.error = Some(format!(
                "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
            ));
            return false;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name,
            Err(error) => {
                context.error = Some(format!("invalid message name: {error:?}"));
                return false;
            }
        };
        // SAFETY: `value` is provided by the guest for the duration of this ABI callback.
        let (value, value_size) = match unsafe { read_message_value(value) } {
            Ok(value) => value,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        match context.transaction.broadcast(name, value, value_size) {
            Ok(()) => true,
            Err(QueueMessageError::ReadOnly) => false,
            Err(QueueMessageError::UnknownTarget) => unreachable!(),
            Err(QueueMessageError::InvalidName) => {
                context.error = Some(format!(
                    "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
                ));
                false
            }
            Err(QueueMessageError::InvalidValue) => {
                context.error =
                    Some("message value is invalid or exceeds the message limits".to_owned());
                false
            }
        }
    })
}

unsafe extern "C" fn host_key_state(ctx: *mut c_void, key: u32) -> u32 {
    guarded(ctx, 0, |context| context.transaction.input().key_state(key))
}

unsafe extern "C" fn host_mouse_button_state(ctx: *mut c_void, button: u32) -> u32 {
    guarded(ctx, 0, |context| {
        context.transaction.input().mouse_button_state(button)
    })
}

unsafe extern "C" fn host_cursor_position(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    host_input_vector(ctx, out, |input| input.cursor())
}

unsafe extern "C" fn host_mouse_delta(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    host_input_vector(ctx, out, |input| input.mouse_delta())
}

unsafe extern "C" fn host_wheel_delta(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    host_input_vector(ctx, out, |input| input.wheel())
}

unsafe extern "C" fn host_action_state(ctx: *mut c_void, name: RveStr, kind: u32) -> u32 {
    guarded(ctx, 0, |context| {
        let Ok(name) = (unsafe { read_abi_str(name) }) else {
            return 0;
        };
        if kind != RVE_ACTION_BUTTON {
            return 0;
        }
        let Ok(value) = context
            .transaction
            .action_state(name, InputActionKind::Button)
        else {
            return 0;
        };
        (u32::from(value.down) * RVE_INPUT_DOWN)
            | (u32::from(value.pressed) * RVE_INPUT_PRESSED)
            | (u32::from(value.released) * RVE_INPUT_RELEASED)
    })
}

unsafe extern "C" fn host_action_value(
    ctx: *mut c_void,
    name: RveStr,
    kind: u32,
    out: *mut [f32; 2],
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Ok(name) = (unsafe { read_abi_str(name) }) else {
            return false;
        };
        let kind = match kind {
            RVE_ACTION_AXIS => InputActionKind::Axis,
            RVE_ACTION_VECTOR2 => InputActionKind::Vector2,
            _ => return false,
        };
        let Ok(value) = context.transaction.action_state(name, kind) else {
            return false;
        };
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = value.value };
        true
    })
}

unsafe extern "C" fn host_input_context(ctx: *mut c_void, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(name) = context.transaction.action_context() else {
            return false;
        };
        // SAFETY: output was checked and the frame owns the context name for this call.
        unsafe { *out = RveStr::from_str_ref(name) };
        true
    })
}

unsafe extern "C" fn host_set_input_context(ctx: *mut c_void, name: RveStr) -> bool {
    guarded(ctx, false, |context| {
        let Ok(name) = (unsafe { read_abi_str(name) }) else {
            return false;
        };
        context
            .transaction
            .stage_action_mutation(ActionMutation::SetContext(name.to_owned()))
            .is_ok()
    })
}

unsafe extern "C" fn host_rebind_action(
    ctx: *mut c_void,
    context_name: RveStr,
    action: RveStr,
    slot: RveStr,
    inputs: *const RveStr,
    input_count: usize,
) -> bool {
    guarded(ctx, false, |context| {
        if input_count > RVE_INPUT_MAX_BINDINGS || (input_count > 0 && inputs.is_null()) {
            return false;
        }
        let (Ok(context_name), Ok(action), Ok(slot)) = (
            unsafe { read_abi_str(context_name) },
            unsafe { read_abi_str(action) },
            unsafe { read_abi_str(slot) },
        ) else {
            return false;
        };
        // SAFETY: the guest promises `input_count` readable RveStr entries when nonzero.
        let input_names = if input_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(inputs, input_count) }
        };
        let mut physical_inputs = Vec::with_capacity(input_count);
        for input in input_names {
            let Ok(input) = (unsafe { read_abi_str(*input) }) else {
                return false;
            };
            let Some(input) = PhysicalInput::from_name(input) else {
                return false;
            };
            physical_inputs.push(input);
        }
        context
            .transaction
            .stage_action_mutation(ActionMutation::Rebind {
                context: context_name.to_owned(),
                action: action.to_owned(),
                slot: slot.to_owned(),
                inputs: physical_inputs,
            })
            .is_ok()
    })
}

unsafe extern "C" fn host_action_binding(
    ctx: *mut c_void,
    context_name: RveStr,
    action: RveStr,
    slot: RveStr,
    index: usize,
    out: *mut RveStr,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let (Ok(context_name), Ok(action), Ok(slot)) = (
            unsafe { read_abi_str(context_name) },
            unsafe { read_abi_str(action) },
            unsafe { read_abi_str(slot) },
        ) else {
            return false;
        };
        let Ok(bindings) = context
            .transaction
            .action_bindings(context_name, action, slot)
        else {
            return false;
        };
        if bindings.is_empty() && index == 0 {
            // SAFETY: output was checked; empty is the ABI v5 valid-empty-slot sentinel.
            unsafe { *out = RveStr::empty() };
            return true;
        }
        let Some(binding) = bindings.get(index) else {
            return false;
        };
        context.action_binding.clone_from(binding);
        // SAFETY: output was checked and context owns the string until the next host callback.
        unsafe { *out = RveStr::from_str_ref(&context.action_binding) };
        true
    })
}

unsafe extern "C" fn host_reset_bindings(ctx: *mut c_void) -> bool {
    guarded(ctx, false, |context| {
        context
            .transaction
            .stage_action_mutation(ActionMutation::ResetBindings)
            .is_ok()
    })
}

unsafe extern "C" fn host_cursor_state(ctx: *mut c_void, out: *mut RveCursorState) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let cursor = context.transaction.cursor();
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe {
            *out = RveCursorState {
                visible: cursor.visible,
                locked: cursor.locked,
            };
        }
        true
    })
}

unsafe extern "C" fn host_set_cursor_visible(ctx: *mut c_void, visible: bool) -> bool {
    guarded(ctx, false, |context| {
        context.transaction.set_cursor_visible(visible).is_ok()
    })
}

unsafe extern "C" fn host_set_cursor_locked(ctx: *mut c_void, locked: bool) -> bool {
    guarded(ctx, false, |context| {
        context.transaction.set_cursor_locked(locked).is_ok()
    })
}

fn host_input_vector(
    ctx: *mut c_void,
    out: *mut [f32; 2],
    value: impl FnOnce(&crate::types::ScriptInput) -> [f32; 2],
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = value(context.transaction.input()) };
        true
    })
}

pub(crate) unsafe fn read_message_value(
    value: *const RveValue,
) -> Result<(ParamValue, usize), String> {
    if value.is_null() {
        return Err("null message value".to_owned());
    }
    // SAFETY: the guest passes the value pointer for the duration of this host callback.
    let mut budget = MessageValueBudget::new();
    let value = unsafe { read_message_value_inner(*value, 0, &mut budget) }?;
    Ok((value, budget.estimated_size()))
}

unsafe fn read_message_value_inner(
    value: RveValue,
    depth: usize,
    budget: &mut MessageValueBudget,
) -> Result<ParamValue, String> {
    budget.enter(depth).map_err(str::to_owned)?;
    match value.tag {
        RVE_VALUE_BOOL => Ok(ParamValue::Bool(unsafe { value.data.boolean })),
        RVE_VALUE_INT => Ok(ParamValue::Int(unsafe { value.data.integer })),
        RVE_VALUE_FLOAT => {
            let value = unsafe { value.data.float };
            if value.is_finite() {
                Ok(ParamValue::Float(value))
            } else {
                Err("message numbers must be finite".to_owned())
            }
        }
        RVE_VALUE_STR => {
            let string = unsafe { value.data.string };
            budget.add_string(string.len).map_err(str::to_owned)?;
            let string = unsafe { read_abi_str(string) }
                .map_err(|error| format!("message string is invalid: {error:?}"))?;
            Ok(ParamValue::String(string.to_owned()))
        }
        RVE_VALUE_ARRAY => {
            let array = unsafe { value.data.array };
            if array.len > budget.remaining_elements() {
                return Err("message value contains more than 1024 elements".to_owned());
            }
            if array.len == 0 {
                return Ok(ParamValue::Array(Vec::new()));
            }
            if array.ptr.is_null() {
                return Err("null array values".to_owned());
            }
            let values = unsafe { std::slice::from_raw_parts(array.ptr, array.len) };
            let mut owned = Vec::with_capacity(array.len);
            for value in values.iter().copied() {
                owned.push(unsafe { read_message_value_inner(value, depth + 1, budget) }?);
            }
            Ok(ParamValue::Array(owned))
        }
        tag => Err(format!("unknown value tag {tag}")),
    }
}

struct OwnedAbiValue {
    value: RveValue,
    _string: Option<String>,
    _children: Vec<OwnedAbiValue>,
    _array: Vec<RveValue>,
}

impl OwnedAbiValue {
    fn new(value: &ParamValue) -> Self {
        match value {
            ParamValue::Bool(value) => Self::leaf(RveValue {
                tag: rve_sdk::abi::RVE_VALUE_BOOL,
                data: RveValueData { boolean: *value },
            }),
            ParamValue::Int(value) => Self::leaf(RveValue {
                tag: rve_sdk::abi::RVE_VALUE_INT,
                data: RveValueData { integer: *value },
            }),
            ParamValue::Float(value) => Self::leaf(RveValue {
                tag: rve_sdk::abi::RVE_VALUE_FLOAT,
                data: RveValueData { float: *value },
            }),
            ParamValue::String(value) => {
                let value = value.clone();
                let abi_value = RveValue {
                    tag: rve_sdk::abi::RVE_VALUE_STR,
                    data: RveValueData {
                        string: RveStr::from_str_ref(&value),
                    },
                };
                Self {
                    value: abi_value,
                    _string: Some(value),
                    _children: Vec::new(),
                    _array: Vec::new(),
                }
            }
            ParamValue::Array(values) => {
                let children = values.iter().map(Self::new).collect::<Vec<_>>();
                let array = children.iter().map(|child| child.value).collect::<Vec<_>>();
                let value = RveValue {
                    tag: rve_sdk::abi::RVE_VALUE_ARRAY,
                    data: RveValueData {
                        array: RveArray {
                            ptr: array.as_ptr(),
                            len: array.len(),
                        },
                    },
                };
                Self {
                    value,
                    _string: None,
                    _children: children,
                    _array: array,
                }
            }
        }
    }

    fn leaf(value: RveValue) -> Self {
        Self {
            value,
            _string: None,
            _children: Vec::new(),
            _array: Vec::new(),
        }
    }
}

struct OwnedAbiParams {
    _keys: Vec<String>,
    _values: Vec<OwnedAbiValue>,
    _entries: Vec<RveParamEntry>,
    abi: RveParams,
}

impl OwnedAbiParams {
    fn new(params: &BTreeMap<String, ParamValue>) -> Self {
        let keys = params.keys().cloned().collect::<Vec<_>>();
        let values = params.values().map(OwnedAbiValue::new).collect::<Vec<_>>();
        let entries = keys
            .iter()
            .zip(&values)
            .map(|(key, value)| RveParamEntry {
                key: RveStr::from_str_ref(key),
                value: value.value,
            })
            .collect::<Vec<_>>();
        let abi = RveParams {
            entries: entries.as_ptr(),
            len: entries.len(),
        };
        Self {
            _keys: keys,
            _values: values,
            _entries: entries,
            abi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ScriptCursor, ScriptInput};
    use engine_scene::{SceneDocument, World};
    use rve_sdk::abi::RveCameraSettings;

    #[test]
    fn warning_logs_keep_the_warn_level() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/object/Probe");
        let host = context.host(ScriptTime::default());
        // SAFETY: the host context and strings remain live for these callbacks.
        unsafe {
            (host.log)(host.ctx, RVE_LOG_INFO, RveStr::from_static("info"));
            (host.log)(host.ctx, RVE_LOG_WARN, RveStr::from_static("warn"));
            (host.log)(host.ctx, RVE_LOG_ERROR, RveStr::from_static("error"));
            (host.log)(host.ctx, u32::MAX, RveStr::from_static("unknown"));
        }
        let events = events.borrow();
        assert_eq!(events[0].level, ScriptEventLevel::Info);
        assert_eq!(events[1].level, ScriptEventLevel::Warn);
        assert_eq!(events[2].level, ScriptEventLevel::Error);
        assert_eq!(events[3].level, ScriptEventLevel::Error);
    }

    #[test]
    fn abi_logs_share_the_pending_log_limit() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/object/Probe");
        let host = context.host(ScriptTime::default());
        // SAFETY: the host context and static string remain live through each callback.
        unsafe {
            for _ in 0..2_000 {
                (host.log)(host.ctx, RVE_LOG_INFO, RveStr::from_static("abi log"));
            }
        }

        let drained = events.borrow_mut().drain();
        assert_eq!(drained.len(), 1_025);
        assert!(
            drained[..1_024]
                .iter()
                .all(|event| event.level == ScriptEventLevel::Info)
        );
        assert_eq!(drained[1_024].level, ScriptEventLevel::Warn);
        assert_eq!(drained[1_024].message, "976 script log messages dropped");
    }

    #[test]
    fn oversized_abi_logs_are_truncated_without_copying_the_tail() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/object/Probe");
        let host = context.host(ScriptTime::default());
        let message = "é".repeat(3_000);
        // SAFETY: the host context and borrowed message remain live through this callback.
        unsafe {
            (host.log)(host.ctx, RVE_LOG_INFO, RveStr::from_str_ref(&message));
        }

        let drained = events.borrow_mut().drain();
        assert_eq!(drained.len(), 1);
        assert!(drained[0].message.len() <= MAX_LOG_MESSAGE_BYTES);
        assert!(drained[0].message.ends_with('…'));
    }

    #[test]
    fn unknown_input_codes_return_zero_and_vectors_are_readable_during_create() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene");
        let host = context.host(ScriptTime::default());
        let mut cursor = [1.0; 2];
        let mut mouse_delta = [1.0; 2];
        let mut wheel = [1.0; 2];

        // SAFETY: the host context and output slots remain live for these callbacks.
        unsafe {
            assert_eq!((host.key_state)(host.ctx, u32::MAX), 0);
            assert_eq!((host.mouse_button_state)(host.ctx, u32::MAX), 0);
            assert!((host.cursor_position)(host.ctx, &mut cursor));
            assert!((host.mouse_delta)(host.ctx, &mut mouse_delta));
            assert!((host.wheel_delta)(host.ctx, &mut wheel));
        }
        assert_eq!(cursor, [0.0; 2]);
        assert_eq!(mouse_delta, [0.0; 2]);
        assert_eq!(wheel, [0.0; 2]);
    }

    #[test]
    fn input_mutation_callbacks_reject_script_creation() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene");
        let host = context.host(ScriptTime::default());

        // SAFETY: callback context and borrowed names remain live for each call.
        unsafe {
            assert!(!(host.set_input_context.unwrap())(
                host.ctx,
                RveStr::from_static("menu")
            ));
            assert!(!(host.reset_bindings.unwrap())(host.ctx));
            assert!(!(host.rebind_action.unwrap())(
                host.ctx,
                RveStr::from_static("game"),
                RveStr::from_static("Jump"),
                RveStr::from_static("inputs"),
                std::ptr::null(),
                0,
            ));
        }
    }

    #[test]
    fn camera_callbacks_validate_handles_settings_and_creation_writes() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let camera = world.active_camera();
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        transaction.begin(ScriptTransactionInit {
            world: &world,
            self_object: camera,
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            cursor: ScriptCursor::default(),
        });
        {
            let mut context =
                AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
            let host = context.host(ScriptTime::default());
            let mut actual = RveCameraSettings::default();
            // SAFETY: the host context and output slots remain live through the calls.
            unsafe {
                assert!((host.get_camera_settings.unwrap())(
                    host.ctx,
                    1,
                    &mut actual
                ));
                assert_eq!(host.active_camera.unwrap()(host.ctx), 1);
            }
            assert_eq!(actual, RveCameraSettings::default());

            let updated = RveCameraSettings {
                exposure_ev: 2.0,
                ..RveCameraSettings::default()
            };
            // SAFETY: the host context and settings pointer remain live through the call.
            unsafe {
                assert!((host.set_camera_settings.unwrap())(host.ctx, 1, &updated));
                actual = RveCameraSettings::default();
                assert!((host.get_camera_settings.unwrap())(
                    host.ctx,
                    1,
                    &mut actual
                ));
            }
            assert_eq!(actual.exposure_ev, 2.0);

            let invalid_enum = RveCameraSettings {
                tonemapper: u32::MAX,
                ..updated
            };
            let invalid_data = RveCameraSettings {
                exposure_ev: 17.0,
                ..updated
            };
            // SAFETY: pointers are valid for each synchronous callback.
            unsafe {
                assert!(!(host.set_camera_settings.unwrap())(
                    host.ctx,
                    1,
                    &invalid_enum
                ));
                assert!(!(host.set_camera_settings.unwrap())(
                    host.ctx,
                    1,
                    &invalid_data
                ));
                assert!(!(host.set_camera_settings.unwrap())(
                    host.ctx,
                    1,
                    std::ptr::null()
                ));
                assert!(!(host.get_camera_settings.unwrap())(
                    host.ctx,
                    1,
                    std::ptr::null_mut()
                ));
                assert!(!(host.set_active_camera.unwrap())(host.ctx, 0));
                assert!(!(host.set_active_camera.unwrap())(host.ctx, 2));
            }
            assert!(transaction.writable());
        }

        transaction.begin(ScriptTransactionInit {
            world: &world,
            self_object: camera,
            source: "scene/camera/Probe",
            writable: false,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            cursor: ScriptCursor::default(),
        });
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene");
        let host = context.host(ScriptTime::default());
        let settings = RveCameraSettings::default();
        // SAFETY: the host context and settings pointer remain live through the call.
        unsafe {
            assert!(!(host.set_camera_settings.unwrap())(host.ctx, 1, &settings));
            assert!(!(host.set_active_camera.unwrap())(host.ctx, 1));
        }
    }

    #[test]
    fn cursor_callbacks_stage_writes_and_read_pending_state_until_commit() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        transaction.begin(ScriptTransactionInit {
            world: &world,
            self_object: world.active_camera(),
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            cursor: ScriptCursor::default(),
        });
        {
            let mut context =
                AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
            let host = context.host(ScriptTime::default());
            let mut state = RveCursorState {
                visible: false,
                locked: true,
            };
            // SAFETY: the host context and output slot remain live through each call.
            unsafe {
                assert!((host.cursor_state.unwrap())(host.ctx, &mut state));
                assert!(state.visible && !state.locked);
                assert!((host.set_cursor_visible.unwrap())(host.ctx, false));
                assert!((host.set_cursor_locked.unwrap())(host.ctx, true));
                assert!((host.cursor_state.unwrap())(host.ctx, &mut state));
                assert!(!state.visible && state.locked);
            }
        }
        let mut queued = Vec::new();
        let mut queued_bytes = 0;
        let mut audio = Vec::new();
        transaction
            .commit(
                &mut world,
                &mut queued,
                &mut queued_bytes,
                &events,
                &mut audio,
            )
            .unwrap();
        assert_eq!(
            transaction.cursor(),
            ScriptCursor {
                visible: false,
                locked: true,
            }
        );
    }

    #[test]
    fn cursor_write_callbacks_fail_during_script_creation() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/Probe");
        let host = context.host(ScriptTime::default());
        // SAFETY: the host context remains live for the callbacks; no transaction was begun.
        unsafe {
            assert!(!(host.set_cursor_visible.unwrap())(host.ctx, false));
            assert!(!(host.set_cursor_locked.unwrap())(host.ctx, true));
        }
    }
}
