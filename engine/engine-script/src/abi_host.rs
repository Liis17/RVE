use crate::abi_string::read_abi_str;
use crate::actions::ActionMutation;
use crate::lifecycle::{ObjectState, PrefabRequest, PrefabStatus, SpawnArgs};
use crate::lite::{
    CommandGeneration, EventQueue, LuaCommands, MAX_LOG_MESSAGE_BYTES, RegisteredCommandCallback,
    RegisteredLuaCommand,
};
#[cfg(test)]
use crate::transaction::ScriptTransactionInit;
use crate::transaction::{
    AbiObject, MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_TARGET_ID_BYTES, MessageValueBudget,
    QueueMessageError, ScriptObjectRef, ScriptTransaction,
};
use crate::types::{RumbleRequest, ScriptEvent, ScriptEventLevel, ScriptTime, ScriptTransform};
use engine_console::CommandSpec;
use engine_files::VirtualPath;
use engine_physics::Ray;
use engine_project::InputActionKind;
use engine_scene::{
    AmbientOcclusionQuality, AnimationRequest, AntiAliasing, AudioGroup, AudioRequest,
    CameraColorSettings, CameraRenderSettings, CameraSettings, ClipParams, ComponentKind,
    ExposureMode, NetEvent, NetMode, NetTarget, ObjectEdit, ObjectHandle, ParamValue, PeerId,
    PostProcessSettings, ProjectionSettings, Tonemapper, VoiceId,
};
use glam::Vec3;
use rve_sdk::PhysicalInput;
use rve_sdk::abi::{
    RVE_ACTION_AXIS, RVE_ACTION_BUTTON, RVE_ACTION_VECTOR2, RVE_ANTI_ALIASING_DLSS,
    RVE_ANTI_ALIASING_FSR, RVE_ANTI_ALIASING_FXAA, RVE_ANTI_ALIASING_NONE, RVE_ANTI_ALIASING_TAA,
    RVE_INPUT_DOWN, RVE_INPUT_MAX_BINDINGS, RVE_INPUT_PRESSED, RVE_INPUT_RELEASED, RVE_LOG_ERROR,
    RVE_LOG_INFO, RVE_LOG_WARN, RVE_NET_EVENT_MESSAGE, RVE_NET_EVENT_PEER_CONNECTED,
    RVE_NET_EVENT_PEER_DISCONNECTED, RVE_NET_INVALID_INPUT, RVE_NET_MODE_CLIENT, RVE_NET_MODE_HOST,
    RVE_NET_MODE_OFFLINE, RVE_NET_MODE_SERVER, RVE_NET_OK, RVE_NET_REJECTED, RVE_NET_TARGET_ALL,
    RVE_NET_TARGET_PEER, RVE_NET_TARGET_SERVER, RVE_TONEMAPPER_ACES, RVE_TONEMAPPER_NEUTRAL,
    RVE_TONEMAPPER_NONE, RVE_VALUE_ARRAY, RVE_VALUE_ASSET, RVE_VALUE_BOOL, RVE_VALUE_FLOAT,
    RVE_VALUE_INT, RVE_VALUE_OBJECT, RVE_VALUE_STR, RveArray, RveCameraSettings,
    RveCharacterMoveResult, RveCollisionEvent, RveCursorState, RveGamepadState, RveHost,
    RveNetEvent, RveParamEntry, RveParams, RveRaycastHit, RveScriptDesc, RveStatus, RveStr,
    RveTime, RveTransform, RveValue, RveValueData,
};
use rve_sdk::abi::{
    RVE_AO_QUALITY_HIGH, RVE_AO_QUALITY_LOW, RVE_AO_QUALITY_MEDIUM, RVE_AUDIO_GROUP_AMBIENT,
    RVE_AUDIO_GROUP_EFFECTS, RVE_AUDIO_GROUP_MASTER, RVE_AUDIO_GROUP_MUSIC, RVE_EXPOSURE_MODE_AUTO,
    RVE_EXPOSURE_MODE_MANUAL, RveAudioParams, RvePlayAnimationParams, RvePostProcessSettings,
    RveScreenShaderState,
};
use rve_sdk::abi::{
    RVE_OBJECT_ALIVE, RVE_OBJECT_DEAD, RVE_OBJECT_PENDING, RVE_PREFAB_FAILED, RVE_PREFAB_LOADED,
    RVE_PREFAB_LOADING, RVE_PREFAB_RESIDENT, RVE_PREFAB_UNKNOWN, RVE_PREFAB_UNLOADED,
    RVE_PREFAB_UNLOADING, RVE_SPAWN_HAS_ID, RVE_SPAWN_HAS_NAME, RVE_SPAWN_HAS_PARENT,
    RVE_SPAWN_HAS_POSITION, RVE_SPAWN_HAS_ROTATION, RVE_SPAWN_HAS_SCALE, RVE_SPAWN_HAS_VELOCITY,
    RVE_SPAWN_KNOWN_FLAGS, RveSpawnOptions,
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
            spawn: Some(host_spawn),
            destroy: Some(host_destroy),
            object_state: Some(host_object_state),
            prefab_load: Some(host_prefab_load),
            prefab_unload: Some(host_prefab_unload),
            prefab_state: Some(host_prefab_state),
            animation_play: Some(host_animation_play),
            animation_set_weight: Some(host_animation_set_weight),
            animation_set_parameter: Some(host_animation_set_parameter),
            animation_set_speed: Some(host_animation_set_speed),
            animation_stop: Some(host_animation_stop),
            gamepad_state: Some(host_gamepad_state),
            gamepad_rumble: Some(host_gamepad_rumble),
            get_post_process: Some(host_get_post_process),
            set_post_process: Some(host_set_post_process),
            screen_shader_count: Some(host_screen_shader_count),
            get_screen_shader: Some(host_get_screen_shader),
            set_screen_shader: Some(host_set_screen_shader),
            net_mode: Some(host_net_mode),
            net_local_peer: Some(host_net_local_peer),
            net_peers: Some(host_net_peers),
            net_send: Some(host_net_send),
            net_object_id: Some(host_net_object_id),
            net_object_owner: Some(host_net_object_owner),
            net_object_is_owner: Some(host_net_object_is_owner),
            net_object_has_authority: Some(host_net_object_has_authority),
            net_object_set_owner: Some(host_net_object_set_owner),
            net_object_send: Some(host_net_object_send),
            object_set_active: Some(host_object_set_active),
            object_is_active: Some(host_object_is_active),
            object_active_in_hierarchy: Some(host_object_active_in_hierarchy),
            object_parent: Some(host_object_parent),
            object_set_parent: Some(host_object_set_parent),
            object_children: Some(host_object_children),
            object_world_position: Some(host_object_world_position),
            object_set_world_position: Some(host_object_set_world_position),
            object_world_rotation: Some(host_object_world_rotation),
            object_set_world_rotation: Some(host_object_set_world_rotation),
            object_tag: Some(host_object_tag),
            object_set_tag: Some(host_object_set_tag),
            object_layer: Some(host_object_layer),
            object_set_layer: Some(host_object_set_layer),
            object_has_component: Some(host_object_has_component),
            find_objects_by_name: Some(host_find_objects_by_name),
            find_objects_by_tag: Some(host_find_objects_by_tag),
            script_set_enabled: Some(host_script_set_enabled),
            script_enabled: Some(host_script_enabled),
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
        object_refs: &std::collections::HashMap<String, ScriptObjectRef>,
        call: AbiScriptCall<'_>,
    ) -> Result<Self, String> {
        let params = OwnedAbiParams::new(params, object_refs);
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
                    // Descriptors without these callbacks have nothing to run on the transition.
                    AbiPhase::Enable => match self.descriptor.on_enable {
                        Some(callback) => callback(self.instance, &host),
                        None => RveStatus::Ok,
                    },
                    AbiPhase::Disable => match self.descriptor.on_disable {
                        Some(callback) => callback(self.instance, &host),
                        None => RveStatus::Ok,
                    },
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
        let object_refs = message_object_refs(value, call.transaction);
        let value = OwnedAbiValue::new(value, &object_refs);
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

    /// Есть ли у дескриптора callback сетевых событий (ABI v13).
    pub(crate) fn has_net_event(&self) -> bool {
        self.descriptor.net_event.is_some()
    }

    /// Вызывает сетевой callback гостя. Без него (дескриптор до v13) событие пропускается без ошибки.
    pub(crate) fn invoke_net_event(
        &mut self,
        time: ScriptTime,
        event: &NetEvent,
        call: AbiScriptCall<'_>,
    ) -> Result<(), String> {
        let Some(callback) = self.descriptor.net_event else {
            return Ok(());
        };
        let (kind, peer, name, reason, value) = match event {
            NetEvent::Message {
                peer, name, value, ..
            } => (
                RVE_NET_EVENT_MESSAGE,
                *peer,
                RveStr::from_str_ref(name),
                RveStr::empty(),
                Some(OwnedAbiValue::new(
                    value,
                    &message_object_refs(value, call.transaction),
                )),
            ),
            NetEvent::PeerConnected { peer } => (
                RVE_NET_EVENT_PEER_CONNECTED,
                *peer,
                RveStr::empty(),
                RveStr::empty(),
                None,
            ),
            NetEvent::PeerDisconnected { peer, reason } => (
                RVE_NET_EVENT_PEER_DISCONNECTED,
                *peer,
                RveStr::empty(),
                RveStr::from_str_ref(reason),
                None,
            ),
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
        let status = {
            let host = context.host(time);
            let value = value
                .as_ref()
                .map_or(std::ptr::null(), |value| &value.value as *const RveValue);
            let event = RveNetEvent {
                kind,
                peer,
                name,
                value,
                reason,
            };
            // SAFETY: the event, its borrowed strings and value, the instance, and the host remain
            // live throughout the callback.
            unsafe { callback(self.instance, &host, event) }
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
    Enable,
    Disable,
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
        let Some(post) = context.transaction.post_process(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = camera_settings_to_abi(settings, post) };
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

unsafe extern "C" fn host_get_post_process(
    ctx: *mut c_void,
    object: u64,
    out: *mut RvePostProcessSettings,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(settings) = context.transaction.post_process(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = post_process_to_abi(settings) };
        true
    })
}

unsafe extern "C" fn host_set_post_process(
    ctx: *mut c_void,
    object: u64,
    settings: *const RvePostProcessSettings,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() || settings.is_null() {
            return false;
        }
        // SAFETY: settings was checked non-null and is valid for this callback.
        let Some(settings) = post_process_from_abi(unsafe { *settings }) else {
            return false;
        };
        context
            .transaction
            .set_post_process_from_abi(object, settings)
    })
}

unsafe extern "C" fn host_screen_shader_count(
    ctx: *mut c_void,
    object: u64,
    out: *mut u32,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(count) = context.transaction.screen_shader_count(handle) else {
            return false;
        };
        let Ok(count) = u32::try_from(count) else {
            return false;
        };
        // SAFETY: output was checked and points to the guest's writable slot.
        unsafe { *out = count };
        true
    })
}

unsafe extern "C" fn host_get_screen_shader(
    ctx: *mut c_void,
    object: u64,
    slot: u32,
    out: *mut RveScreenShaderState,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(state) = context.transaction.screen_shader(handle, slot as usize) else {
            return false;
        };
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe {
            *out = RveScreenShaderState {
                enabled: u32::from(state.enabled),
                params: state.params,
            };
        }
        true
    })
}

unsafe extern "C" fn host_set_screen_shader(
    ctx: *mut c_void,
    object: u64,
    slot: u32,
    state: *const RveScreenShaderState,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() || state.is_null() {
            return false;
        }
        // SAFETY: state was checked non-null and is valid for this callback.
        let state = unsafe { *state };
        context.transaction.set_screen_shader_from_abi(
            object,
            slot as usize,
            crate::transaction::ScreenShaderState {
                enabled: state.enabled != 0,
                params: state.params,
            },
        )
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
        // A dead or pending object has no body in the query, so it has nothing to ignore.
        let ignore = if ignore == 0 {
            None
        } else {
            match context.transaction.abi_object(ignore) {
                AbiObject::Alive(handle) => Some(handle),
                AbiObject::Unavailable => None,
                AbiObject::Malformed => {
                    context.error = Some("raycast ignore handle is unknown".to_owned());
                    return false;
                }
            }
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
        let Some(object) = live_object(context, object, "get_velocity") else {
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

/// Объект для вызова, который сообщает об ошибке. Мёртвый и pending-объект дают `None` без ошибки
/// (Warn пишет транзакция); значение, которое не декодируется как хендл, — ошибку в `context.error`.
fn live_object(context: &mut AbiCallContext<'_>, object: u64, name: &str) -> Option<ObjectHandle> {
    match context.transaction.abi_object(object) {
        AbiObject::Alive(handle) => Some(handle),
        AbiObject::Unavailable => None,
        AbiObject::Malformed => {
            context.error = Some(format!("{name} received an unknown object handle"));
            None
        }
    }
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
        let Some(object) = live_object(context, object, "physics command") else {
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
        let Some(object) = live_object(context, object, "move_character") else {
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
        let Some(object) = live_object(context, object, "physics getter") else {
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
        let Some(object) = live_object(context, object, "set_collider_enabled") else {
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
/// `Ok(None)` means the object is dead or pending: the call does nothing.
fn audio_source_object(
    context: &mut AbiCallContext<'_>,
    object: u64,
    name: &str,
) -> Result<Option<ObjectHandle>, String> {
    let handle = match context.transaction.abi_object(object) {
        AbiObject::Alive(handle) => handle,
        AbiObject::Unavailable => return Ok(None),
        AbiObject::Malformed => return Err(format!("{name} received an unknown object handle")),
    };
    if !context.transaction.has_audio_source(handle) {
        return Err(format!("{name} requires an audio_source component"));
    }
    Ok(Some(handle))
}

/// Runs a staged audio call on a resolved source. A dead source returns `Error` without an error
/// event: the dead-object warning has already been recorded.
fn audio_source_call(
    context: &mut AbiCallContext<'_>,
    object: u64,
    name: &str,
    stage: impl FnOnce(&mut ScriptTransaction, ObjectHandle) -> Result<(), String>,
) -> u32 {
    match audio_source_object(context, object, name) {
        Ok(Some(handle)) => {
            let result = stage(&mut *context.transaction, handle);
            audio_result(context, result)
        }
        Ok(None) => RveStatus::Error as u32,
        Err(message) => audio_result(context, Err(message)),
    }
}

/// Объект с аниматором: без компонента вызов отклоняется, как у аудио.
fn animator_object(
    context: &mut AbiCallContext<'_>,
    object: u64,
    name: &str,
) -> Result<ObjectHandle, String> {
    let handle = context
        .transaction
        .object_from_abi_handle(object)
        .ok_or_else(|| format!("{name} received an unknown object handle"))?;
    if !context.transaction.has_animator(handle) {
        return Err(format!("{name} requires an animator component"));
    }
    Ok(handle)
}

fn animation_finite(value: f32, name: &str) -> Result<f32, String> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{name} must be finite"))
    }
}

fn animation_non_negative(value: f32, name: &str) -> Result<f32, String> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(format!("{name} must be finite and not negative"))
    }
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
        audio_source_call(context, object, "play_sound", |transaction, handle| {
            transaction.stage_audio(AudioRequest::PlaySource {
                object: handle,
                voice: VoiceId::allocate(),
            })
        })
    })
}

unsafe extern "C" fn host_audio_stop_source(ctx: *mut c_void, object: u64) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        audio_source_call(context, object, "stop_sound", |transaction, handle| {
            transaction.stage_audio(AudioRequest::StopSource { object: handle })
        })
    })
}

unsafe extern "C" fn host_audio_set_source_volume(
    ctx: *mut c_void,
    object: u64,
    volume: f32,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        audio_source_call(
            context,
            object,
            "set_sound_volume",
            |transaction, handle| {
                let volume = audio_volume(volume)?;
                transaction.stage_audio(AudioRequest::SetSourceVolume {
                    object: handle,
                    volume,
                })
            },
        )
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

fn animation_result(context: &mut AbiCallContext<'_>, result: Result<(), String>) -> u32 {
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

unsafe extern "C" fn host_animation_play(
    ctx: *mut c_void,
    object: u64,
    motion: RveStr,
    params: *const RvePlayAnimationParams,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = stage_animation_play(context, object, motion, params);
        animation_result(context, result)
    })
}

fn stage_animation_play(
    context: &mut AbiCallContext<'_>,
    object: u64,
    motion: RveStr,
    params: *const RvePlayAnimationParams,
) -> Result<(), String> {
    if params.is_null() {
        return Err("play_animation received a null pointer".to_owned());
    }
    // SAFETY: the guest passes a valid params pointer for this synchronous callback.
    let params = unsafe { *params };
    let handle = animator_object(context, object, "play_animation")?;
    // SAFETY: the guest keeps the motion string alive for this synchronous callback.
    let motion = unsafe { read_abi_str(motion) }
        .map_err(|_| "play_animation received an invalid motion name".to_owned())?;
    if motion.is_empty() {
        return Err("play_animation motion name must not be empty".to_owned());
    }
    let fade = animation_non_negative(params.fade, "play_animation fade")?;
    let looping = match params.looping {
        -1 => None,
        0 => Some(false),
        1 => Some(true),
        _ => return Err("play_animation loop must be -1, 0 or 1".to_owned()),
    };
    // SAFETY: the guest keeps the optional `then` string alive for this synchronous callback.
    let then = unsafe { read_abi_str(params.then) }
        .map_err(|_| "play_animation received an invalid then name".to_owned())?;
    let then = (!then.is_empty()).then(|| then.to_owned());
    context.transaction.stage_animation(AnimationRequest::Play {
        object: handle,
        motion: motion.to_owned(),
        fade,
        looping,
        then,
    })
}

unsafe extern "C" fn host_animation_set_weight(
    ctx: *mut c_void,
    object: u64,
    motion: RveStr,
    weight: f32,
    fade: f32,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = (|| {
            let handle = animator_object(context, object, "set_animation_weight")?;
            // SAFETY: the guest keeps the motion string alive for this synchronous callback.
            let motion = unsafe { read_abi_str(motion) }
                .map_err(|_| "set_animation_weight received an invalid motion name".to_owned())?;
            let weight = animation_finite(weight, "set_animation_weight weight")?;
            let fade = animation_non_negative(fade, "set_animation_weight fade")?;
            context
                .transaction
                .stage_animation(AnimationRequest::SetWeight {
                    object: handle,
                    motion: motion.to_owned(),
                    weight,
                    fade,
                })
        })();
        animation_result(context, result)
    })
}

unsafe extern "C" fn host_animation_set_parameter(
    ctx: *mut c_void,
    object: u64,
    name: RveStr,
    value: f32,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = (|| {
            let handle = animator_object(context, object, "set_animation_parameter")?;
            // SAFETY: the guest keeps the parameter name alive for this synchronous callback.
            let name = unsafe { read_abi_str(name) }
                .map_err(|_| "set_animation_parameter received an invalid name".to_owned())?;
            let value = animation_finite(value, "set_animation_parameter value")?;
            context
                .transaction
                .stage_animation(AnimationRequest::SetParameter {
                    object: handle,
                    name: name.to_owned(),
                    value,
                })
        })();
        animation_result(context, result)
    })
}

unsafe extern "C" fn host_animation_set_speed(ctx: *mut c_void, object: u64, speed: f32) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = (|| {
            let handle = animator_object(context, object, "set_animation_speed")?;
            let speed = animation_non_negative(speed, "set_animation_speed speed")?;
            context
                .transaction
                .stage_animation(AnimationRequest::SetSpeed {
                    object: handle,
                    speed,
                })
        })();
        animation_result(context, result)
    })
}

unsafe extern "C" fn host_animation_stop(ctx: *mut c_void, object: u64) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        let result = animator_object(context, object, "stop_animation").and_then(|handle| {
            context
                .transaction
                .stage_animation(AnimationRequest::Stop { object: handle })
        });
        animation_result(context, result)
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

/// Records a refused lifecycle request as a script error and returns the callback's fallback.
/// The reason is also kept as the call's error, so a guest that returns `Error` without a message
/// of its own still reports it.
fn lifecycle_refusal<T>(context: &mut AbiCallContext<'_>, message: String, fallback: T) -> T {
    context.events.borrow_mut().push(ScriptEvent {
        level: ScriptEventLevel::Error,
        source: context.source.to_owned(),
        message: message.clone(),
    });
    context.error = Some(message);
    fallback
}

/// Reads the optional fields of a spawn request. Unknown presence bits are refused before any
/// field is read, so a newer guest cannot silently lose data.
///
/// # Safety
/// `options` must be null or point to a readable `RveSpawnOptions`, and its strings must stay
/// valid for the duration of the call.
unsafe fn spawn_args(options: *const RveSpawnOptions) -> Result<SpawnArgs, String> {
    let mut args = SpawnArgs::default();
    if options.is_null() {
        return Ok(args);
    }
    // SAFETY: the caller guarantees a readable options struct; it is copied before use.
    let options = unsafe { *options };
    let unknown = options.flags & !RVE_SPAWN_KNOWN_FLAGS;
    if unknown != 0 {
        return Err(format!("spawn received unknown option flags {unknown:#x}"));
    }
    let has = |bit: u32| options.flags & bit != 0;
    if has(RVE_SPAWN_HAS_POSITION) {
        args.options.position = Some(Vec3::from_array(options.position));
    }
    if has(RVE_SPAWN_HAS_ROTATION) {
        args.options.rotation_deg = Some(Vec3::from_array(options.rotation_deg));
    }
    if has(RVE_SPAWN_HAS_SCALE) {
        args.options.scale = Some(Vec3::from_array(options.scale));
    }
    if has(RVE_SPAWN_HAS_VELOCITY) {
        args.velocity = Some(Vec3::from_array(options.velocity));
    }
    if has(RVE_SPAWN_HAS_PARENT) {
        // The transaction checks that the parent is a live object.
        let parent = ObjectHandle::from_abi(options.parent)
            .ok_or_else(|| "spawn parent is not an object handle".to_owned())?;
        args.options.parent = Some(parent);
    }
    if has(RVE_SPAWN_HAS_NAME) {
        // SAFETY: the caller keeps the borrowed bytes readable for this call.
        let name = unsafe { read_abi_str(options.name) }
            .map_err(|_| "spawn name must be valid UTF-8".to_owned())?;
        args.options.name = Some(name.to_owned());
    }
    if has(RVE_SPAWN_HAS_ID) {
        // SAFETY: the caller keeps the borrowed bytes readable for this call.
        let id = unsafe { read_abi_str(options.id) }
            .map_err(|_| "spawn id must be valid UTF-8".to_owned())?;
        args.options.id = Some(id.to_owned());
    }
    Ok(args)
}

unsafe extern "C" fn host_spawn(
    ctx: *mut c_void,
    prefab: RveStr,
    options: *const RveSpawnOptions,
) -> u64 {
    guarded(ctx, 0, |context| {
        // SAFETY: the guest passes a valid prefab string and options pointer for this call.
        let result = unsafe { read_abi_str(prefab) }
            .map_err(|_| "spawn received an invalid prefab path".to_owned())
            .and_then(|prefab| {
                // SAFETY: the options pointer is valid for this call, as checked above.
                let args = unsafe { spawn_args(options) }?;
                context.transaction.spawn(prefab, args)
            });
        match result {
            // `to_abi` fails only for slot index u32::MAX; zero is never a valid handle.
            Ok(handle) => handle.to_abi().unwrap_or(0),
            Err(message) => lifecycle_refusal(context, message, 0),
        }
    })
}

unsafe extern "C" fn host_destroy(ctx: *mut c_void, object: u64, delay: f32) -> bool {
    guarded(ctx, false, |context| {
        // Pending handles are accepted: the transaction checks their generation and state.
        let Some(handle) = ObjectHandle::from_abi(object) else {
            return false;
        };
        match context.transaction.destroy(handle, f64::from(delay)) {
            Ok(accepted) => accepted,
            Err(message) => lifecycle_refusal(context, message, false),
        }
    })
}

unsafe extern "C" fn host_object_state(ctx: *mut c_void, object: u64) -> u32 {
    guarded(ctx, RVE_OBJECT_DEAD, |context| {
        // Unlike `object_from_abi_handle`, this also reports a pending handle.
        let Some(handle) = ObjectHandle::from_abi(object) else {
            return RVE_OBJECT_DEAD;
        };
        match context.transaction.object_state(handle) {
            ObjectState::Alive => RVE_OBJECT_ALIVE,
            ObjectState::Pending => RVE_OBJECT_PENDING,
            ObjectState::Dead => RVE_OBJECT_DEAD,
        }
    })
}

unsafe extern "C" fn host_object_set_active(ctx: *mut c_void, object: u64, active: bool) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        context.transaction.stage_edit(ObjectEdit::SetActive {
            object: handle,
            active,
        })
    })
}

/// Записывает флаг в `out`, если объект жив; общий путь для `is_active` и иерархической активности.
fn object_bool(
    ctx: *mut c_void,
    object: u64,
    out: *mut bool,
    query: fn(&ScriptTransaction, ObjectHandle) -> Option<bool>,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(value) = query(context.transaction, handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = value };
        true
    })
}

unsafe extern "C" fn host_object_is_active(ctx: *mut c_void, object: u64, out: *mut bool) -> bool {
    object_bool(ctx, object, out, ScriptTransaction::is_active)
}

unsafe extern "C" fn host_object_active_in_hierarchy(
    ctx: *mut c_void,
    object: u64,
    out: *mut bool,
) -> bool {
    object_bool(ctx, object, out, ScriptTransaction::active_in_hierarchy)
}

unsafe extern "C" fn host_object_parent(ctx: *mut c_void, object: u64, out: *mut u64) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(parent) = context.transaction.parent(handle) else {
            return false;
        };
        let value = match parent {
            Some(parent) => match context.transaction.abi_handle(parent) {
                Some(value) => value,
                None => return false,
            },
            None => 0,
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = value };
        true
    })
}

unsafe extern "C" fn host_object_set_parent(
    ctx: *mut c_void,
    object: u64,
    parent: u64,
    keep_world: bool,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let parent = if parent == 0 {
            None
        } else {
            match context.transaction.object_from_abi_handle(parent) {
                Some(parent) => Some(parent),
                None => return false,
            }
        };
        context.transaction.stage_edit(ObjectEdit::SetParent {
            object: handle,
            parent,
            keep_world,
        })
    })
}

/// Копирует список хендлов в буфер гостя: пишет общее число и не больше `capacity` значений.
fn write_handles(out: *mut u64, capacity: usize, count: *mut usize, handles: &[u64]) -> bool {
    if count.is_null() || (capacity > 0 && out.is_null()) {
        return false;
    }
    // SAFETY: the guest passes a writable count slot, checked above.
    unsafe { *count = handles.len() };
    for (index, handle) in handles.iter().take(capacity).enumerate() {
        // SAFETY: `index` is below `capacity`, and the guest allocated `capacity` entries.
        unsafe { *out.add(index) = *handle };
    }
    true
}

unsafe extern "C" fn host_object_children(
    ctx: *mut c_void,
    object: u64,
    out: *mut u64,
    capacity: usize,
    count: *mut usize,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(children) = context.transaction.children(handle) else {
            return false;
        };
        let values = children
            .into_iter()
            .filter_map(|child| context.transaction.abi_handle(child))
            .collect::<Vec<_>>();
        write_handles(out, capacity, count, &values)
    })
}

/// Записывает вектор в `out`, если объект жив; общий путь для мировых позиции и поворота.
fn object_vec3(
    ctx: *mut c_void,
    object: u64,
    out: *mut [f32; 3],
    query: fn(&ScriptTransaction, ObjectHandle) -> Option<Vec3>,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(value) = query(context.transaction, handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = value.to_array() };
        true
    })
}

unsafe extern "C" fn host_object_world_position(
    ctx: *mut c_void,
    object: u64,
    out: *mut [f32; 3],
) -> bool {
    object_vec3(ctx, object, out, ScriptTransaction::world_position)
}

unsafe extern "C" fn host_object_world_rotation(
    ctx: *mut c_void,
    object: u64,
    out: *mut [f32; 3],
) -> bool {
    object_vec3(ctx, object, out, ScriptTransaction::world_rotation_deg)
}

/// Reads the vector passed by the guest; `None` for a null pointer.
fn read_vec3(value: *const [f32; 3]) -> Option<Vec3> {
    // SAFETY: the guest passes a readable vector for the duration of this callback.
    (!value.is_null()).then(|| Vec3::from_array(unsafe { *value }))
}

unsafe extern "C" fn host_object_set_world_position(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    guarded(ctx, false, |context| {
        let (Some(position), Some(handle)) = (
            read_vec3(value),
            context.transaction.object_from_abi_handle(object),
        ) else {
            return false;
        };
        context
            .transaction
            .stage_edit(ObjectEdit::SetWorldPosition {
                object: handle,
                position,
            })
    })
}

unsafe extern "C" fn host_object_set_world_rotation(
    ctx: *mut c_void,
    object: u64,
    value: *const [f32; 3],
) -> bool {
    guarded(ctx, false, |context| {
        let (Some(rotation_deg), Some(handle)) = (
            read_vec3(value),
            context.transaction.object_from_abi_handle(object),
        ) else {
            return false;
        };
        context
            .transaction
            .stage_edit(ObjectEdit::SetWorldRotation {
                object: handle,
                rotation_deg,
            })
    })
}

unsafe extern "C" fn host_object_tag(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(tag) = context.transaction.tag(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(tag) };
        true
    })
}

unsafe extern "C" fn host_object_set_tag(ctx: *mut c_void, object: u64, value: RveStr) -> bool {
    guarded(ctx, false, |context| {
        // SAFETY: the guest passes a string valid for the duration of this callback.
        let Ok(tag) = (unsafe { read_abi_str(value) }) else {
            return false;
        };
        let tag = tag.to_owned();
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        context.transaction.stage_edit(ObjectEdit::SetTag {
            object: handle,
            tag,
        })
    })
}

unsafe extern "C" fn host_object_layer(ctx: *mut c_void, object: u64, out: *mut u32) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(layer) = context.transaction.layer(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = u32::from(layer) };
        true
    })
}

unsafe extern "C" fn host_object_set_layer(ctx: *mut c_void, object: u64, layer: u32) -> bool {
    guarded(ctx, false, |context| {
        let Ok(layer) = u8::try_from(layer) else {
            return false;
        };
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        context.transaction.stage_edit(ObjectEdit::SetLayer {
            object: handle,
            layer,
        })
    })
}

unsafe extern "C" fn host_object_has_component(
    ctx: *mut c_void,
    object: u64,
    name: RveStr,
    out: *mut bool,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        // SAFETY: the guest passes a string valid for the duration of this callback.
        let Ok(name) = (unsafe { read_abi_str(name) }) else {
            return false;
        };
        let Some(kind) = ComponentKind::parse(name) else {
            return false;
        };
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(present) = context.transaction.has_component(handle, &kind) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = present };
        true
    })
}

/// Ищет живые объекты по имени или тегу; общий путь для `find_objects_by_name` и `..._by_tag`.
fn find_objects(
    ctx: *mut c_void,
    value: RveStr,
    out: *mut u64,
    capacity: usize,
    count: *mut usize,
    find: fn(&ScriptTransaction, &str) -> Vec<ObjectHandle>,
) -> bool {
    guarded(ctx, false, |context| {
        if count.is_null() || (capacity > 0 && out.is_null()) {
            return false;
        }
        // SAFETY: the guest passes a string valid for the duration of this callback.
        let Ok(text) = (unsafe { read_abi_str(value) }) else {
            return false;
        };
        let handles = find(context.transaction, text)
            .into_iter()
            .filter_map(|handle| context.transaction.abi_handle(handle))
            .collect::<Vec<_>>();
        write_handles(out, capacity, count, &handles)
    })
}

unsafe extern "C" fn host_find_objects_by_name(
    ctx: *mut c_void,
    name: RveStr,
    out: *mut u64,
    capacity: usize,
    count: *mut usize,
) -> bool {
    find_objects(ctx, name, out, capacity, count, |transaction, name| {
        transaction.find_all_by_name(name)
    })
}

unsafe extern "C" fn host_find_objects_by_tag(
    ctx: *mut c_void,
    tag: RveStr,
    out: *mut u64,
    capacity: usize,
    count: *mut usize,
) -> bool {
    find_objects(ctx, tag, out, capacity, count, |transaction, tag| {
        transaction.find_all_with_tag(tag)
    })
}

unsafe extern "C" fn host_script_set_enabled(ctx: *mut c_void, enabled: bool) -> bool {
    guarded(ctx, false, |context| {
        context.transaction.set_script_enabled(enabled)
    })
}

unsafe extern "C" fn host_script_enabled(ctx: *mut c_void, out: *mut bool) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = context.transaction.script_enabled() };
        true
    })
}

fn request_prefab(ctx: *mut c_void, path: RveStr, request: fn(String) -> PrefabRequest) -> bool {
    guarded(ctx, false, |context| {
        // SAFETY: the guest passes a valid path string for this callback.
        let result = match unsafe { read_abi_str(path) } {
            Ok(path) => context.transaction.prefab_request(request(path.to_owned())),
            Err(_) => Err("prefab path must be valid UTF-8".to_owned()),
        };
        match result {
            Ok(()) => true,
            Err(message) => lifecycle_refusal(context, message, false),
        }
    })
}

unsafe extern "C" fn host_prefab_load(ctx: *mut c_void, path: RveStr) -> bool {
    request_prefab(ctx, path, PrefabRequest::Load)
}

unsafe extern "C" fn host_prefab_unload(ctx: *mut c_void, path: RveStr) -> bool {
    request_prefab(ctx, path, PrefabRequest::Unload)
}

unsafe extern "C" fn host_prefab_state(ctx: *mut c_void, path: RveStr) -> u32 {
    guarded(ctx, RVE_PREFAB_UNKNOWN, |context| {
        // SAFETY: the guest passes a valid path string for this callback.
        let Ok(path) = (unsafe { read_abi_str(path) }) else {
            return RVE_PREFAB_UNKNOWN;
        };
        context
            .transaction
            .prefab_status(path)
            .map_or(RVE_PREFAB_UNKNOWN, prefab_state_code)
    })
}

fn prefab_state_code(status: PrefabStatus) -> u32 {
    match status {
        PrefabStatus::Resident => RVE_PREFAB_RESIDENT,
        PrefabStatus::Unloaded => RVE_PREFAB_UNLOADED,
        PrefabStatus::Loading => RVE_PREFAB_LOADING,
        PrefabStatus::Loaded => RVE_PREFAB_LOADED,
        PrefabStatus::Unloading => RVE_PREFAB_UNLOADING,
        PrefabStatus::Failed => RVE_PREFAB_FAILED,
    }
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

/// Прокси устаревшего `[color]` для `get_camera_settings`: exposure и tonemapper берутся из
/// эффективной постобработки камеры, а не из блока `[color]` файла `.camerasettings`.
fn camera_settings_to_abi(
    settings: CameraSettings,
    post: PostProcessSettings,
) -> RveCameraSettings {
    RveCameraSettings {
        fov_deg: settings.projection.fov_deg,
        near: settings.projection.near,
        far: settings.projection.far,
        render_scale: settings.render.render_scale,
        clear_color: settings.render.clear_color,
        exposure_ev: post.exposure.ev,
        anti_aliasing: match settings.render.anti_aliasing {
            AntiAliasing::Fxaa => RVE_ANTI_ALIASING_FXAA,
            AntiAliasing::None => RVE_ANTI_ALIASING_NONE,
            AntiAliasing::Taa => RVE_ANTI_ALIASING_TAA,
            AntiAliasing::Fsr => RVE_ANTI_ALIASING_FSR,
            AntiAliasing::Dlss => RVE_ANTI_ALIASING_DLSS,
        },
        tonemapper: match post.tonemapping.tonemapper {
            Tonemapper::Aces => RVE_TONEMAPPER_ACES,
            Tonemapper::Neutral => RVE_TONEMAPPER_NEUTRAL,
            Tonemapper::None => RVE_TONEMAPPER_NONE,
        },
    }
}

/// Переводит постобработку в плоскую запись ABI v12; булевы поля кодируются 0 и 1.
fn post_process_to_abi(settings: PostProcessSettings) -> RvePostProcessSettings {
    let exposure = settings.exposure;
    let tonemapping = settings.tonemapping;
    let bloom = settings.bloom;
    let ao = settings.ambient_occlusion;
    let grading = settings.color_grading;
    let vignette = settings.vignette;
    let aberration = settings.chromatic_aberration;
    let grain = settings.film_grain;
    let dof = settings.depth_of_field;
    let motion = settings.motion_blur;
    RvePostProcessSettings {
        exposure_enabled: u32::from(exposure.enabled),
        exposure_mode: match exposure.mode {
            ExposureMode::Manual => RVE_EXPOSURE_MODE_MANUAL,
            ExposureMode::Auto => RVE_EXPOSURE_MODE_AUTO,
        },
        exposure_ev: exposure.ev,
        exposure_min_ev: exposure.min_ev,
        exposure_max_ev: exposure.max_ev,
        exposure_speed_up: exposure.speed_up,
        exposure_speed_down: exposure.speed_down,
        exposure_low_percent: exposure.low_percent,
        exposure_high_percent: exposure.high_percent,
        tonemapping_enabled: u32::from(tonemapping.enabled),
        tonemapper: match tonemapping.tonemapper {
            Tonemapper::Aces => RVE_TONEMAPPER_ACES,
            Tonemapper::Neutral => RVE_TONEMAPPER_NEUTRAL,
            Tonemapper::None => RVE_TONEMAPPER_NONE,
        },
        bloom_enabled: u32::from(bloom.enabled),
        bloom_intensity: bloom.intensity,
        bloom_threshold: bloom.threshold,
        bloom_knee: bloom.knee,
        bloom_scatter: bloom.scatter,
        bloom_tint: bloom.tint,
        ao_enabled: u32::from(ao.enabled),
        ao_radius: ao.radius,
        ao_intensity: ao.intensity,
        ao_falloff: ao.falloff,
        ao_quality: match ao.quality {
            AmbientOcclusionQuality::Low => RVE_AO_QUALITY_LOW,
            AmbientOcclusionQuality::Medium => RVE_AO_QUALITY_MEDIUM,
            AmbientOcclusionQuality::High => RVE_AO_QUALITY_HIGH,
        },
        grading_enabled: u32::from(grading.enabled),
        grading_temperature: grading.temperature,
        grading_tint: grading.tint,
        grading_contrast: grading.contrast,
        grading_saturation: grading.saturation,
        grading_lift: grading.lift,
        grading_gamma: grading.gamma,
        grading_gain: grading.gain,
        vignette_enabled: u32::from(vignette.enabled),
        vignette_intensity: vignette.intensity,
        vignette_smoothness: vignette.smoothness,
        vignette_roundness: vignette.roundness,
        vignette_color: vignette.color,
        aberration_enabled: u32::from(aberration.enabled),
        aberration_intensity: aberration.intensity,
        grain_enabled: u32::from(grain.enabled),
        grain_intensity: grain.intensity,
        grain_response: grain.response,
        dof_enabled: u32::from(dof.enabled),
        dof_focus_distance: dof.focus_distance,
        dof_aperture: dof.aperture,
        dof_focal_length_mm: dof.focal_length_mm,
        dof_max_blur_px: dof.max_blur_px,
        motion_enabled: u32::from(motion.enabled),
        motion_shutter_angle_deg: motion.shutter_angle_deg,
        motion_samples: motion.samples,
        motion_max_blur_px: motion.max_blur_px,
    }
}

/// Разбирает плоскую запись ABI v12. Коды и булевы значения проверяются строго; диапазоны —
/// той же функцией `validate`, что и в `.postprocess`.
fn post_process_from_abi(value: RvePostProcessSettings) -> Option<PostProcessSettings> {
    fn flag(value: u32) -> Option<bool> {
        match value {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    let mut settings = PostProcessSettings::default();
    settings.exposure.enabled = flag(value.exposure_enabled)?;
    settings.exposure.mode = match value.exposure_mode {
        RVE_EXPOSURE_MODE_MANUAL => ExposureMode::Manual,
        RVE_EXPOSURE_MODE_AUTO => ExposureMode::Auto,
        _ => return None,
    };
    settings.exposure.ev = value.exposure_ev;
    settings.exposure.min_ev = value.exposure_min_ev;
    settings.exposure.max_ev = value.exposure_max_ev;
    settings.exposure.speed_up = value.exposure_speed_up;
    settings.exposure.speed_down = value.exposure_speed_down;
    settings.exposure.low_percent = value.exposure_low_percent;
    settings.exposure.high_percent = value.exposure_high_percent;
    settings.tonemapping.enabled = flag(value.tonemapping_enabled)?;
    settings.tonemapping.tonemapper = match value.tonemapper {
        RVE_TONEMAPPER_ACES => Tonemapper::Aces,
        RVE_TONEMAPPER_NEUTRAL => Tonemapper::Neutral,
        RVE_TONEMAPPER_NONE => Tonemapper::None,
        _ => return None,
    };
    settings.bloom.enabled = flag(value.bloom_enabled)?;
    settings.bloom.intensity = value.bloom_intensity;
    settings.bloom.threshold = value.bloom_threshold;
    settings.bloom.knee = value.bloom_knee;
    settings.bloom.scatter = value.bloom_scatter;
    settings.bloom.tint = value.bloom_tint;
    settings.ambient_occlusion.enabled = flag(value.ao_enabled)?;
    settings.ambient_occlusion.radius = value.ao_radius;
    settings.ambient_occlusion.intensity = value.ao_intensity;
    settings.ambient_occlusion.falloff = value.ao_falloff;
    settings.ambient_occlusion.quality = match value.ao_quality {
        RVE_AO_QUALITY_LOW => AmbientOcclusionQuality::Low,
        RVE_AO_QUALITY_MEDIUM => AmbientOcclusionQuality::Medium,
        RVE_AO_QUALITY_HIGH => AmbientOcclusionQuality::High,
        _ => return None,
    };
    settings.color_grading.enabled = flag(value.grading_enabled)?;
    settings.color_grading.temperature = value.grading_temperature;
    settings.color_grading.tint = value.grading_tint;
    settings.color_grading.contrast = value.grading_contrast;
    settings.color_grading.saturation = value.grading_saturation;
    settings.color_grading.lift = value.grading_lift;
    settings.color_grading.gamma = value.grading_gamma;
    settings.color_grading.gain = value.grading_gain;
    settings.vignette.enabled = flag(value.vignette_enabled)?;
    settings.vignette.intensity = value.vignette_intensity;
    settings.vignette.smoothness = value.vignette_smoothness;
    settings.vignette.roundness = value.vignette_roundness;
    settings.vignette.color = value.vignette_color;
    settings.chromatic_aberration.enabled = flag(value.aberration_enabled)?;
    settings.chromatic_aberration.intensity = value.aberration_intensity;
    settings.film_grain.enabled = flag(value.grain_enabled)?;
    settings.film_grain.intensity = value.grain_intensity;
    settings.film_grain.response = value.grain_response;
    settings.depth_of_field.enabled = flag(value.dof_enabled)?;
    settings.depth_of_field.focus_distance = value.dof_focus_distance;
    settings.depth_of_field.aperture = value.dof_aperture;
    settings.depth_of_field.focal_length_mm = value.dof_focal_length_mm;
    settings.depth_of_field.max_blur_px = value.dof_max_blur_px;
    settings.motion_blur.enabled = flag(value.motion_enabled)?;
    settings.motion_blur.shutter_angle_deg = value.motion_shutter_angle_deg;
    settings.motion_blur.samples = value.motion_samples;
    settings.motion_blur.max_blur_px = value.motion_max_blur_px;
    settings.validate().ok()?;
    Some(settings)
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
        color: Some(CameraColorSettings {
            exposure_ev: value.exposure_ev,
            tonemapper,
        }),
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
        let (value, value_size) = match unsafe {
            read_message_value_with_objects(value, |handle| {
                ObjectHandle::from_abi(handle)
                    .and_then(|handle| context.transaction.object_id(handle).map(str::to_owned))
            })
        } {
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
        let (value, value_size) = match unsafe {
            read_message_value_with_objects(value, |handle| {
                ObjectHandle::from_abi(handle)
                    .and_then(|handle| context.transaction.object_id(handle).map(str::to_owned))
            })
        } {
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

fn net_mode_code(mode: NetMode) -> u32 {
    match mode {
        NetMode::Offline => RVE_NET_MODE_OFFLINE,
        NetMode::Host => RVE_NET_MODE_HOST,
        NetMode::Server => RVE_NET_MODE_SERVER,
        NetMode::Client => RVE_NET_MODE_CLIENT,
    }
}

/// Адресат из кода `RVE_NET_TARGET_*` и номера участника.
fn net_target(kind: u32, peer: u32) -> Result<NetTarget, String> {
    match kind {
        RVE_NET_TARGET_SERVER => Ok(NetTarget::Server),
        RVE_NET_TARGET_ALL => Ok(NetTarget::All),
        RVE_NET_TARGET_PEER => Ok(NetTarget::Peer(peer as PeerId)),
        other => Err(format!("unknown network target code {other}")),
    }
}

/// Переводит результат сетевой операции в код ABI. Ошибка пишется в журнал скрипта и в ответ гостю.
fn net_result(context: &mut AbiCallContext<'_>, result: Result<(), String>, invalid: bool) -> u32 {
    match result {
        Ok(()) => RVE_NET_OK,
        Err(message) => {
            context.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: context.source.to_owned(),
                message: message.clone(),
            });
            context.error = Some(message);
            if invalid {
                RVE_NET_INVALID_INPUT
            } else {
                RVE_NET_REJECTED
            }
        }
    }
}

/// Живой объект сетевого вызова. Мёртвый или pending объект даёт `None` без ошибки.
fn net_object(context: &mut AbiCallContext<'_>, object: u64) -> Option<ObjectHandle> {
    match context.transaction.abi_object(object) {
        AbiObject::Alive(handle) => Some(handle),
        AbiObject::Unavailable => None,
        AbiObject::Malformed => {
            context.error = Some("network call received an unknown object handle".to_owned());
            None
        }
    }
}

unsafe extern "C" fn host_net_mode(ctx: *mut c_void) -> u32 {
    guarded(ctx, RVE_NET_MODE_OFFLINE, |context| {
        net_mode_code(context.transaction.net_mode())
    })
}

unsafe extern "C" fn host_net_local_peer(ctx: *mut c_void) -> u32 {
    guarded(ctx, 0, |context| context.transaction.net_local_peer())
}

unsafe extern "C" fn host_net_peers(
    ctx: *mut c_void,
    out: *mut u32,
    capacity: usize,
    count: *mut usize,
) -> bool {
    guarded(ctx, false, |context| {
        if count.is_null() || (capacity > 0 && out.is_null()) {
            return false;
        }
        let peers = context.transaction.net_peers();
        // SAFETY: the guest passes a writable count slot, checked above.
        unsafe { *count = peers.len() };
        for (index, peer) in peers.iter().take(capacity).enumerate() {
            // SAFETY: `index` is below `capacity`, and the guest allocated `capacity` entries.
            unsafe { out.add(index).write(*peer) };
        }
        true
    })
}

/// Общая часть `send` и `net_send`: имя и значение проверяются по тем же лимитам, что у `rve.send`.
fn net_send_request(
    context: &mut AbiCallContext<'_>,
    object: Option<ObjectHandle>,
    target: (u32, u32),
    name: RveStr,
    value: *const RveValue,
    reliable: bool,
) -> u32 {
    if name.len > MAX_MESSAGE_NAME_BYTES {
        return net_result(
            context,
            Err(format!(
                "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
            )),
            true,
        );
    }
    let name = match unsafe { read_abi_str(name) } {
        Ok(name) => name,
        Err(error) => {
            return net_result(
                context,
                Err(format!("invalid message name: {error:?}")),
                true,
            );
        }
    };
    // SAFETY: `value` is provided by the guest for the duration of this ABI callback.
    let (value, _) = match unsafe {
        read_message_value_with_objects(value, |handle| {
            ObjectHandle::from_abi(handle)
                .and_then(|handle| context.transaction.object_id(handle).map(str::to_owned))
        })
    } {
        Ok(value) => value,
        Err(error) => return net_result(context, Err(error), true),
    };
    let target = match net_target(target.0, target.1) {
        Ok(target) => target,
        Err(error) => return net_result(context, Err(error), true),
    };
    let result = context
        .transaction
        .send_net(object, name, value, target, reliable);
    net_result(context, result, false)
}

unsafe extern "C" fn host_net_send(
    ctx: *mut c_void,
    target_kind: u32,
    peer: u32,
    name: RveStr,
    value: *const RveValue,
    reliable: bool,
) -> u32 {
    guarded(ctx, RVE_NET_REJECTED, |context| {
        net_send_request(context, None, (target_kind, peer), name, value, reliable)
    })
}

unsafe extern "C" fn host_net_object_id(ctx: *mut c_void, object: u64, out: *mut u32) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = net_object(context, object) else {
            return false;
        };
        let Some(state) = context.transaction.net_object(handle) else {
            return false;
        };
        if out.is_null() {
            return false;
        }
        // SAFETY: the guest owns the output slot for this call; null was rejected above.
        unsafe { *out = state.net_id.get() };
        true
    })
}

unsafe extern "C" fn host_net_object_owner(ctx: *mut c_void, object: u64, out: *mut u32) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = net_object(context, object) else {
            return false;
        };
        let Some(owner) = context
            .transaction
            .net_object(handle)
            .and_then(|state| state.owner)
        else {
            return false;
        };
        if out.is_null() {
            return false;
        }
        // SAFETY: the guest owns the output slot for this call; null was rejected above.
        unsafe { *out = owner };
        true
    })
}

unsafe extern "C" fn host_net_object_is_owner(
    ctx: *mut c_void,
    object: u64,
    out: *mut bool,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = net_object(context, object) else {
            return false;
        };
        if out.is_null() {
            return false;
        }
        // SAFETY: the guest owns the output slot for this call; null was rejected above.
        unsafe { *out = context.transaction.net_is_owner(handle) };
        true
    })
}

unsafe extern "C" fn host_net_object_has_authority(
    ctx: *mut c_void,
    object: u64,
    out: *mut bool,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = net_object(context, object) else {
            return false;
        };
        if out.is_null() {
            return false;
        }
        // SAFETY: the guest owns the output slot for this call; null was rejected above.
        unsafe { *out = context.transaction.net_has_authority(handle) };
        true
    })
}

unsafe extern "C" fn host_net_object_set_owner(ctx: *mut c_void, object: u64, peer: u32) -> u32 {
    guarded(ctx, RVE_NET_REJECTED, |context| {
        let Some(handle) = net_object(context, object) else {
            return RVE_NET_OK;
        };
        let result = context.transaction.set_net_owner(handle, peer as PeerId);
        net_result(context, result, false)
    })
}

unsafe extern "C" fn host_net_object_send(
    ctx: *mut c_void,
    object: u64,
    target_kind: u32,
    peer: u32,
    name: RveStr,
    value: *const RveValue,
    reliable: bool,
) -> u32 {
    guarded(ctx, RVE_NET_REJECTED, |context| {
        let Some(handle) = net_object(context, object) else {
            return RVE_NET_OK;
        };
        net_send_request(
            context,
            Some(handle),
            (target_kind, peer),
            name,
            value,
            reliable,
        )
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

unsafe extern "C" fn host_gamepad_state(
    ctx: *mut c_void,
    pad: u32,
    out: *mut RveGamepadState,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let state = context.transaction.input().gamepad_state(pad);
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = state.unwrap_or_default() };
        state.is_some()
    })
}

unsafe extern "C" fn host_gamepad_rumble(
    ctx: *mut c_void,
    pad: u32,
    strong: f32,
    weak: f32,
    seconds: f32,
) -> bool {
    guarded(ctx, false, |context| {
        let result = RumbleRequest::new(pad, strong, weak, seconds)
            .ok_or_else(|| format!("gamepad slot must be 0 or from 1 to 4, got {pad}"))
            .and_then(|request| context.transaction.stage_rumble(request));
        audio_result(context, result) == RveStatus::Ok as u32
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
    // Messages without a world context can carry only ordinary scalar and array values.
    unsafe { read_message_value_with_objects(value, |_| None) }
}

pub(crate) unsafe fn read_message_value_with_objects(
    value: *const RveValue,
    mut object_id: impl FnMut(u64) -> Option<String>,
) -> Result<(ParamValue, usize), String> {
    if value.is_null() {
        return Err("null message value".to_owned());
    }
    // SAFETY: the guest passes the value pointer for the duration of this host callback.
    let mut budget = MessageValueBudget::new();
    let value = unsafe { read_message_value_inner(*value, 0, &mut budget, &mut object_id) }?;
    Ok((value, budget.estimated_size()))
}

unsafe fn read_message_value_inner(
    value: RveValue,
    depth: usize,
    budget: &mut MessageValueBudget,
    object_id: &mut impl FnMut(u64) -> Option<String>,
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
        RVE_VALUE_STR | RVE_VALUE_ASSET => {
            let string = unsafe { value.data.string };
            budget.add_string(string.len).map_err(str::to_owned)?;
            let string = unsafe { read_abi_str(string) }
                .map_err(|error| format!("message string is invalid: {error:?}"))?;
            if value.tag == RVE_VALUE_ASSET {
                if string.is_empty() {
                    return Ok(ParamValue::String(String::new()));
                }
                VirtualPath::parse(string)
                    .map(ParamValue::Asset)
                    .map_err(|error| format!("message asset path is invalid: {error}"))
            } else {
                Ok(ParamValue::String(string.to_owned()))
            }
        }
        RVE_VALUE_OBJECT => {
            let handle = unsafe { value.data.object };
            object_id(handle)
                .map(ParamValue::Object)
                .ok_or_else(|| format!("message object handle {handle} is not alive"))
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
                owned.push(unsafe {
                    read_message_value_inner(value, depth + 1, budget, object_id)
                }?);
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

fn message_object_refs(
    value: &ParamValue,
    transaction: &ScriptTransaction,
) -> std::collections::HashMap<String, ScriptObjectRef> {
    fn visit(
        value: &ParamValue,
        transaction: &ScriptTransaction,
        references: &mut std::collections::HashMap<String, ScriptObjectRef>,
    ) {
        match value {
            ParamValue::Object(id) if !id.is_empty() => {
                if let Some(handle) = transaction.handle_by_id(id)
                    && let Some(name) = transaction.object_name(handle)
                {
                    references.insert(
                        id.clone(),
                        ScriptObjectRef {
                            handle,
                            name: name.to_owned(),
                        },
                    );
                }
            }
            ParamValue::Array(values) => {
                for value in values {
                    visit(value, transaction, references);
                }
            }
            _ => {}
        }
    }

    let mut references = std::collections::HashMap::new();
    visit(value, transaction, &mut references);
    references
}

impl OwnedAbiValue {
    fn new(
        value: &ParamValue,
        object_refs: &std::collections::HashMap<String, ScriptObjectRef>,
    ) -> Self {
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
            ParamValue::Asset(path) => {
                let value = path.as_str().to_owned();
                let abi_value = RveValue {
                    tag: RVE_VALUE_ASSET,
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
            ParamValue::Object(id) => {
                let handle = object_refs
                    .get(id)
                    .and_then(|reference| reference.handle.to_abi())
                    .unwrap_or_default();
                Self::leaf(RveValue {
                    tag: RVE_VALUE_OBJECT,
                    data: RveValueData { object: handle },
                })
            }
            ParamValue::Array(values) => {
                let children = values
                    .iter()
                    .map(|value| Self::new(value, object_refs))
                    .collect::<Vec<_>>();
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
    fn new(
        params: &BTreeMap<String, ParamValue>,
        object_refs: &std::collections::HashMap<String, ScriptObjectRef>,
    ) -> Self {
        let keys = params.keys().cloned().collect::<Vec<_>>();
        let values = params
            .values()
            .map(|value| OwnedAbiValue::new(value, object_refs))
            .collect::<Vec<_>>();
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
    use crate::types::{ScriptCursor, ScriptEventLevel, ScriptInput};
    use engine_scene::{SceneDocument, World};
    use rve_sdk::abi::RveCameraSettings;

    #[test]
    fn gamepad_state_refuses_null_output_and_unknown_slots_with_zeroed_output() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene");
        let host = context.host(ScriptTime::default());
        let get_state = host.gamepad_state.unwrap();
        let mut state = RveGamepadState {
            connected: 9,
            axes: [1.0; 6],
            ..RveGamepadState::default()
        };

        // SAFETY: the host context and the output slot remain live for these calls.
        unsafe {
            assert!(!get_state(host.ctx, 0, std::ptr::null_mut()));
            assert!(!get_state(host.ctx, 5, &mut state));
        }
        assert_eq!(state, RveGamepadState::default());
    }

    #[test]
    fn gamepad_rumble_is_refused_for_bad_slots_and_during_script_creation() {
        let events = EventQueue::default();
        // A default transaction is not writable, as during script creation.
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene");
        let host = context.host(ScriptTime::default());
        let rumble = host.gamepad_rumble.unwrap();

        // SAFETY: the host context remains live for these calls.
        unsafe {
            assert!(!rumble(host.ctx, 5, 1.0, 1.0, 1.0));
            assert!(!rumble(host.ctx, 1, 1.0, 1.0, 1.0));
        }
        let events = events.borrow();
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| event.level == ScriptEventLevel::Error)
        );
        assert!(events[0].message.contains("got 5"), "{}", events[0].message);
        assert!(
            events[1]
                .message
                .contains("rumble is unavailable during script creation"),
            "{}",
            events[1].message
        );
    }

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
    fn screen_shader_callbacks_read_and_queue_slots_and_reject_invalid_input() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[assets]]\npath = \"/fx/vhs.glsl\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects.camera.screen_shaders]]\nshader = \"/fx/vhs.glsl\"\nstage = \"after_post\"\nparams = [0.5]\n[[objects]]\nid = \"plain\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        transaction.begin(ScriptTransactionInit {
            world: &world,
            self_object: Some(world.active_camera()),
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            queued_animation: 0,
            queued_rumble: 0,
            net: Default::default(),
            queued_net: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            lifecycle: std::rc::Rc::new(std::cell::RefCell::new(
                crate::lifecycle::Lifecycle::default(),
            )),
            lifecycle_writes: true,
            instance_serial: 0,
            instance_enabled: true,
            cursor: ScriptCursor::default(),
        });
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
        let host = context.host(ScriptTime::default());
        let count = host.screen_shader_count.unwrap();
        let get = host.get_screen_shader.unwrap();
        let set = host.set_screen_shader.unwrap();
        let camera = 1;
        let plain = 2;
        let mut slots = 0_u32;
        let mut state = rve_sdk::abi::RveScreenShaderState::default();
        // SAFETY: output pointers and the state pointer are valid for each synchronous callback.
        unsafe {
            assert!(count(host.ctx, camera, &mut slots));
            assert_eq!(slots, 1);
            assert!(!count(host.ctx, plain, &mut slots));
            assert!(get(host.ctx, camera, 0, &mut state));
            assert_eq!(state.enabled, 1, "enabled по умолчанию true");
            assert_eq!(state.params[0], 0.5);
            assert_eq!(state.params[1], 0.0);
            assert!(!get(host.ctx, camera, 1, &mut state), "слота 1 нет");
            assert!(!get(host.ctx, plain, 0, &mut state));
            assert!(!get(host.ctx, camera, 0, std::ptr::null_mut()));

            let staged = rve_sdk::abi::RveScreenShaderState {
                enabled: 0,
                params: [0.25; 16],
            };
            assert!(set(host.ctx, camera, 0, &staged));
            assert!(get(host.ctx, camera, 0, &mut state));
            assert_eq!(state, staged, "очередь видна до commit");

            let mut not_finite = staged;
            not_finite.params[3] = f32::NAN;
            assert!(!set(host.ctx, camera, 0, &not_finite));
            assert!(!set(host.ctx, camera, 1, &staged));
            assert!(!set(host.ctx, plain, 0, &staged));
            assert!(!set(host.ctx, camera, 0, std::ptr::null()));
        }
    }

    #[test]
    fn post_process_callbacks_round_trip_and_reject_invalid_values_and_non_cameras() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"plain\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        transaction.begin(ScriptTransactionInit {
            world: &world,
            self_object: Some(world.active_camera()),
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            queued_animation: 0,
            queued_rumble: 0,
            net: Default::default(),
            queued_net: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            lifecycle: std::rc::Rc::new(std::cell::RefCell::new(
                crate::lifecycle::Lifecycle::default(),
            )),
            lifecycle_writes: true,
            instance_serial: 0,
            instance_enabled: true,
            cursor: ScriptCursor::default(),
        });
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
        let host = context.host(ScriptTime::default());
        let get = host.get_post_process.unwrap();
        let set = host.set_post_process.unwrap();
        let camera = 1;
        let plain = 2;
        let mut actual = rve_sdk::abi::RvePostProcessSettings {
            bloom_intensity: -5.0,
            ..rve_sdk::abi::RvePostProcessSettings::default()
        };
        // SAFETY: output and settings pointers are valid for each synchronous callback.
        unsafe {
            assert!(get(host.ctx, camera, &mut actual));
            assert_eq!(actual, rve_sdk::abi::RvePostProcessSettings::default());
            assert!(!get(host.ctx, plain, &mut actual));
            assert!(!get(host.ctx, camera, std::ptr::null_mut()));
        }

        let updated = rve_sdk::abi::RvePostProcessSettings {
            bloom_enabled: 1,
            bloom_intensity: 2.0,
            exposure_mode: rve_sdk::abi::RVE_EXPOSURE_MODE_AUTO,
            ..rve_sdk::abi::RvePostProcessSettings::default()
        };
        let invalid = [
            rve_sdk::abi::RvePostProcessSettings {
                bloom_intensity: -1.0,
                ..updated
            },
            rve_sdk::abi::RvePostProcessSettings {
                bloom_enabled: 2,
                ..updated
            },
            rve_sdk::abi::RvePostProcessSettings {
                exposure_mode: 7,
                ..updated
            },
            rve_sdk::abi::RvePostProcessSettings {
                ao_quality: 9,
                ..updated
            },
            rve_sdk::abi::RvePostProcessSettings {
                tonemapper: u32::MAX,
                ..updated
            },
        ];
        // SAFETY: pointers are valid for each synchronous callback.
        unsafe {
            assert!(set(host.ctx, camera, &updated));
            assert!(get(host.ctx, camera, &mut actual));
            assert_eq!(actual, updated);
            for settings in &invalid {
                assert!(!set(host.ctx, camera, settings), "{settings:?}");
            }
            assert!(!set(host.ctx, plain, &updated));
            assert!(!set(host.ctx, camera, std::ptr::null()));
        }
        drop(context);
        assert!(transaction.post_process(world.active_camera()).is_some());
    }

    #[test]
    fn legacy_color_proxies_read_and_write_the_effective_post_process_in_call_order() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let camera = world.active_camera();
        let mut file_post = PostProcessSettings::default();
        file_post.exposure.ev = 2.0;
        world.set_post_process(camera, file_post).unwrap();
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        transaction.begin(ScriptTransactionInit {
            world: &world,
            self_object: Some(camera),
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            queued_animation: 0,
            queued_rumble: 0,
            net: Default::default(),
            queued_net: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            lifecycle: std::rc::Rc::new(std::cell::RefCell::new(
                crate::lifecycle::Lifecycle::default(),
            )),
            lifecycle_writes: true,
            instance_serial: 0,
            instance_enabled: true,
            cursor: ScriptCursor::default(),
        });
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/camera/Probe");
        let host = context.host(ScriptTime::default());
        let get = host.get_camera_settings.unwrap();
        let set_camera = host.set_camera_settings.unwrap();
        let set_post = host.set_post_process.unwrap();
        let camera_handle = 1;
        let read_exposure = || {
            let mut actual = RveCameraSettings::default();
            // SAFETY: the host context and output slot remain live through the call.
            unsafe { assert!(get(host.ctx, camera_handle, &mut actual)) };
            actual.exposure_ev
        };
        // Legacy get берёт EV из .postprocess, а не из пустого [color].
        assert_eq!(read_exposure(), 2.0);

        // Legacy set меняет эффективную экспозицию, а не только устаревший блок.
        let legacy_zero = RveCameraSettings {
            exposure_ev: 0.0,
            ..RveCameraSettings::default()
        };
        // SAFETY: the settings pointer remains live through the call.
        unsafe { assert!(set_camera(host.ctx, camera_handle, &legacy_zero)) };
        assert_eq!(read_exposure(), 0.0);

        // Порядок: set_post_process после legacy set побеждает, поскольку пишет ту же копию.
        let mut later = post_process_to_abi(PostProcessSettings::default());
        later.exposure_ev = 3.0;
        // SAFETY: the post process pointer remains live through the call.
        unsafe { assert!(set_post(host.ctx, camera_handle, &later)) };
        assert_eq!(read_exposure(), 3.0);

        // Порядок: legacy set после set_post_process побеждает.
        // SAFETY: the settings pointer remains live through the call.
        let legacy_one = RveCameraSettings {
            exposure_ev: 1.0,
            ..RveCameraSettings::default()
        };
        unsafe { assert!(set_camera(host.ctx, camera_handle, &legacy_one)) };
        assert_eq!(read_exposure(), 1.0);
        drop(context);
        assert_eq!(transaction.post_process(camera).unwrap().exposure.ev, 1.0);
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
            self_object: Some(camera),
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            queued_rumble: 0,
            queued_animation: 0,
            net: Default::default(),
            queued_net: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            lifecycle: std::rc::Rc::new(std::cell::RefCell::new(
                crate::lifecycle::Lifecycle::default(),
            )),
            lifecycle_writes: true,
            instance_serial: 0,
            instance_enabled: true,
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
            self_object: Some(camera),
            source: "scene/camera/Probe",
            writable: false,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            queued_rumble: 0,
            queued_animation: 0,
            net: Default::default(),
            queued_net: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            lifecycle: std::rc::Rc::new(std::cell::RefCell::new(
                crate::lifecycle::Lifecycle::default(),
            )),
            lifecycle_writes: true,
            instance_serial: 0,
            instance_enabled: true,
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
            self_object: Some(world.active_camera()),
            source: "scene/camera/Probe",
            writable: true,
            actions: std::sync::Arc::new(
                crate::actions::ActionFrame::empty(ScriptInput::default()),
            ),
            queued_messages: (0, 0),
            queued_audio: 0,
            queued_rumble: 0,
            queued_animation: 0,
            net: Default::default(),
            queued_net: 0,
            physics: std::rc::Rc::new(std::cell::RefCell::new(
                crate::physics::PhysicsScriptState::default(),
            )),
            physics_phase_dt: None,
            lifecycle: std::rc::Rc::new(std::cell::RefCell::new(
                crate::lifecycle::Lifecycle::default(),
            )),
            lifecycle_writes: true,
            instance_serial: 0,
            instance_enabled: true,
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
                &mut Vec::new(),
                &mut Vec::new(),
                &mut Vec::new(),
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
