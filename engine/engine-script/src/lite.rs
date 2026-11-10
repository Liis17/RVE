mod component_api;
mod physics_api;

use crate::actions::{ActionMutation, ActionValue};
use crate::lifecycle::{ObjectState, PrefabRequest, SpawnArgs};
use crate::script_refs::normalize_script_key;
use crate::transaction::ScreenShaderState;
use crate::transaction::{
    MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_TARGET_ID_BYTES, MessageValueBudget, QueueMessageError,
    ScriptObjectRef, ScriptTransaction,
};
use crate::types::ScriptFrameTime;
use crate::types::{
    RumbleRequest, ScriptEvent, ScriptEventLevel, ScriptInput, ScriptParams, ScriptTransform,
};
use crate::ui::UiDelivery;
use crate::ui_lua;
use engine_console::CommandSpec;
use engine_files::VirtualPath;
use engine_physics::Ray;
use engine_project::InputActionKind;
use engine_scene::{
    AmbientOcclusionQuality, AnimationRequest, AntiAliasing, AssetKind, AudioGroup, AudioRequest,
    CameraSettings, ClipParams, ComponentKind, ExposureMode, MAX_SCREEN_SHADER_PARAMS, NetEvent,
    NetMode, NetTarget, ObjectEdit, ObjectHandle, ParamKind, ParamSchema, ParamSpec, ParamValue,
    PeerId, PostProcessSettings, Tonemapper, VoiceId,
};
use glam::Vec3;
use mlua::chunk::ChunkMode;
use mlua::thread::{ThreadEvent, ThreadStatus, ThreadTriggers};
use mlua::{
    Error as LuaError, Function, HookTriggers, Lua, LuaOptions, LuaString, MultiValue, RegistryKey,
    StdLib, Table, Thread, UserData, UserDataFields, Value, VmState,
};
use rve_sdk::abi::{RVE_GAMEPAD_SLOTS, RVE_INPUT_MAX_BINDINGS, RveGamepadState};
use rve_sdk::{GamepadAxis, GamepadButton, Key, MouseButton, PhysicalInput};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const LUA_HOOK_INSTRUCTION_INTERVAL: u32 = 1_000;
const LUA_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;
const MAX_PROPERTY_ARRAY_DEPTH: usize = 4;

pub(crate) const MAX_PENDING_LOG_EVENTS: usize = 1_024;
pub(crate) const MAX_LOG_MESSAGE_BYTES: usize = 4_096;
const LOG_TRUNCATION_MARKER: &str = "…";

pub(crate) type EventQueue = Rc<RefCell<ScriptEventQueue>>;
pub(crate) type ActiveSource = Rc<RefCell<String>>;
pub(crate) type ActiveCall = Rc<Cell<*mut ScriptTransaction>>;
pub(crate) type ActiveOwner = Rc<Cell<Option<usize>>>;
pub(crate) type CommandGeneration = Rc<Cell<u64>>;

pub(crate) enum RegisteredCommandCallback {
    Lua(RegistryKey),
    Abi,
}

pub(crate) struct RegisteredLuaCommand {
    pub(crate) owner: usize,
    pub(crate) spec: CommandSpec,
    pub(crate) callback: RegisteredCommandCallback,
}

pub(crate) type LuaCommands = Rc<RefCell<std::collections::BTreeMap<String, RegisteredLuaCommand>>>;

#[derive(Default)]
pub(crate) struct ScriptEventQueue {
    events: Vec<ScriptEvent>,
    pending_log_events: usize,
    dropped_log_events: usize,
}

impl ScriptEventQueue {
    pub(crate) fn push(&mut self, event: ScriptEvent) {
        self.events.push(event);
    }

    pub(crate) fn push_log(&mut self, mut event: ScriptEvent) {
        if !self.has_log_capacity() {
            self.record_dropped_log();
            return;
        }
        truncate_log_message(&mut event.message);
        self.pending_log_events += 1;
        self.events.push(event);
    }

    pub(crate) fn has_log_capacity(&self) -> bool {
        self.pending_log_events < MAX_PENDING_LOG_EVENTS
    }

    pub(crate) fn record_dropped_log(&mut self) {
        self.dropped_log_events = self.dropped_log_events.saturating_add(1);
    }

    pub(crate) fn drain(&mut self) -> Vec<ScriptEvent> {
        let mut events = std::mem::take(&mut self.events);
        if self.dropped_log_events > 0 {
            events.push(ScriptEvent {
                level: ScriptEventLevel::Warn,
                source: "engine-script".to_owned(),
                message: format!("{} script log messages dropped", self.dropped_log_events),
            });
        }
        self.pending_log_events = 0;
        self.dropped_log_events = 0;
        events
    }
}

impl std::ops::Deref for ScriptEventQueue {
    type Target = [ScriptEvent];

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

fn truncate_log_message(message: &mut String) {
    if message.len() > MAX_LOG_MESSAGE_BYTES {
        truncate_to_boundary(message, MAX_LOG_MESSAGE_BYTES - LOG_TRUNCATION_MARKER.len());
        message.push_str(LOG_TRUNCATION_MARKER);
    }
}

fn truncate_to_boundary(message: &mut String, max_bytes: usize) {
    let mut end = max_bytes.min(message.len());
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
}

pub(crate) fn bounded_lua_log_message(bytes: &[u8]) -> String {
    let prefix_len = bytes.len().min(MAX_LOG_MESSAGE_BYTES);
    let mut message = String::from_utf8_lossy(&bytes[..prefix_len]).into_owned();
    if bytes.len() > prefix_len || message.len() > MAX_LOG_MESSAGE_BYTES {
        truncate_to_boundary(
            &mut message,
            MAX_LOG_MESSAGE_BYTES - LOG_TRUNCATION_MARKER.len(),
        );
        message.push_str(LOG_TRUNCATION_MARKER);
    }
    message
}

pub(crate) struct LuaState {
    pub(crate) lua: Lua,
    pub(crate) base_environment: Table,
    pub(crate) time: Table,
    pub(crate) call_deadline: CallDeadline,
}

#[derive(Clone)]
pub(crate) struct CallDeadline {
    deadline: Rc<Cell<Option<Instant>>>,
    expired: Rc<Cell<bool>>,
    call_time_limit: Option<Duration>,
    interrupted: Arc<AtomicBool>,
}

pub(crate) struct ActiveCallDeadline<'lua> {
    lua: &'lua Lua,
    deadline: CallDeadline,
    owns_deadline: bool,
    thread: Option<Thread>,
}

pub(crate) enum CoroutineYield {
    NextFrame,
    Frames(u64),
    Seconds(f64),
}

pub(crate) enum CoroutineResume {
    Finished,
    Yielded(CoroutineYield),
}

impl CallDeadline {
    pub(crate) fn is_expired(&self) -> bool {
        self.expired.get()
    }

    pub(crate) fn new(call_time_limit: Option<Duration>, interrupted: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Rc::new(Cell::new(None)),
            expired: Rc::new(Cell::new(false)),
            call_time_limit,
            interrupted,
        }
    }

    pub(crate) fn enter<'lua>(&self, lua: &'lua Lua) -> ActiveCallDeadline<'lua> {
        let owns_deadline = if let Some(limit) = self.call_time_limit {
            if self.deadline.get().is_none() {
                let now = Instant::now();
                self.deadline
                    .set(Some(now.checked_add(limit).unwrap_or(now)));
                self.expired.set(false);
                true
            } else {
                false
            }
        } else {
            false
        };
        ActiveCallDeadline {
            lua,
            deadline: self.clone(),
            owns_deadline,
            thread: None,
        }
    }

    pub(crate) fn enter_thread<'lua>(
        &self,
        lua: &'lua Lua,
        thread: &Thread,
    ) -> mlua::Result<ActiveCallDeadline<'lua>> {
        let owns_deadline = if let Some(limit) = self.call_time_limit {
            if self.deadline.get().is_none() {
                let now = Instant::now();
                self.deadline
                    .set(Some(now.checked_add(limit).unwrap_or(now)));
                self.expired.set(false);
                true
            } else {
                false
            }
        } else {
            false
        };
        if let Err(error) = self.install_sparse_thread_hook(thread) {
            if owns_deadline {
                self.deadline.set(None);
                self.expired.set(false);
            }
            return Err(error);
        }
        Ok(ActiveCallDeadline {
            lua,
            deadline: self.clone(),
            owns_deadline,
            thread: Some(thread.clone()),
        })
    }

    fn install_sparse_hook(&self, lua: &Lua) -> mlua::Result<()> {
        let deadline = self.clone();
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(LUA_HOOK_INSTRUCTION_INTERVAL),
            move |lua, _| {
                if deadline.interrupted.load(Ordering::Acquire) {
                    deadline.install_dense_hook(lua)?;
                    return Err(LuaError::runtime("Lua execution interrupted"));
                }
                if deadline
                    .deadline
                    .get()
                    .is_some_and(|expires_at| Instant::now() >= expires_at)
                {
                    deadline.expired.set(true);
                    deadline.install_dense_hook(lua)?;
                    return Err(LuaError::runtime(deadline.timeout_message()));
                }
                Ok(VmState::Continue)
            },
        )
    }

    fn install_sparse_thread_hook(&self, thread: &Thread) -> mlua::Result<()> {
        let deadline = self.clone();
        let hooked_thread = thread.clone();
        thread.set_hook(
            HookTriggers::new().every_nth_instruction(LUA_HOOK_INSTRUCTION_INTERVAL),
            move |_, _| {
                if deadline.interrupted.load(Ordering::Acquire) {
                    deadline.install_dense_thread_hook(&hooked_thread)?;
                    return Err(LuaError::runtime("Lua execution interrupted"));
                }
                if deadline
                    .deadline
                    .get()
                    .is_some_and(|expires_at| Instant::now() >= expires_at)
                {
                    deadline.expired.set(true);
                    deadline.install_dense_thread_hook(&hooked_thread)?;
                    return Err(LuaError::runtime(deadline.timeout_message()));
                }
                Ok(VmState::Continue)
            },
        )
    }

    fn install_dense_hook(&self, lua: &Lua) -> mlua::Result<()> {
        let interrupted = self.interrupted.clone();
        let expired = self.expired.clone();
        let timeout_message = self.timeout_message();
        lua.set_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
            if interrupted.load(Ordering::Acquire) {
                Err(LuaError::runtime("Lua execution interrupted"))
            } else if expired.get() {
                Err(LuaError::runtime(timeout_message.clone()))
            } else {
                Ok(VmState::Continue)
            }
        })
    }

    fn install_dense_thread_hook(&self, thread: &Thread) -> mlua::Result<()> {
        let interrupted = self.interrupted.clone();
        let expired = self.expired.clone();
        let timeout_message = self.timeout_message();
        thread.set_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
            if interrupted.load(Ordering::Acquire) {
                Err(LuaError::runtime("Lua execution interrupted"))
            } else if expired.get() {
                Err(LuaError::runtime(timeout_message.clone()))
            } else {
                Ok(VmState::Continue)
            }
        })
    }

    pub(crate) fn timeout_message(&self) -> String {
        format!(
            "Lua call exceeded time limit of {:?}",
            self.call_time_limit.unwrap_or_default()
        )
    }
}

impl Drop for ActiveCallDeadline<'_> {
    fn drop(&mut self) {
        if self.owns_deadline {
            let expired = self.deadline.expired.get();
            self.deadline.deadline.set(None);
            if expired && !self.deadline.interrupted.load(Ordering::Acquire) {
                let _ = self.deadline.install_sparse_hook(self.lua);
            }
        }
        if let Some(thread) = &self.thread {
            let _ = self.deadline.install_sparse_thread_hook(thread);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_lua(
    events: EventQueue,
    active_source: ActiveSource,
    active_call: ActiveCall,
    active_owner: ActiveOwner,
    commands: LuaCommands,
    commands_generation: CommandGeneration,
    call_deadline: CallDeadline,
    time_scale: Rc<Cell<f32>>,
) -> mlua::Result<LuaState> {
    let lua = Lua::new_with(
        // mlua always opens the Lua base library; these flags select the remaining libraries.
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(LUA_MEMORY_LIMIT_BYTES)?;
    call_deadline.install_sparse_hook(&lua)?;
    let thread_deadline = call_deadline.clone();
    lua.set_thread_event_callback(ThreadTriggers::ON_CREATE, move |_, event| {
        if let ThreadEvent::Create(thread) = event {
            thread_deadline.install_sparse_thread_hook(&thread)?;
        }
        Ok(())
    });

    create_object_handles(&lua)?;
    ui_lua::create_registry(&lua)?;
    let globals = lua.globals();
    globals.set("dofile", Value::Nil)?;
    globals.set("loadfile", Value::Nil)?;
    globals.set("require", Value::Nil)?;

    let tostring = globals.get::<Function>("tostring")?;
    let print = log_function(
        &lua,
        tostring.clone(),
        events.clone(),
        active_source.clone(),
        ScriptEventLevel::Info,
    )?;
    globals.set("print", print)?;
    globals.set("collectgarbage", restricted_gc_function(&lua)?)?;

    let time_values = lua.create_table()?;
    time_values.set("dt", 0.0)?;
    time_values.set("unscaled_dt", 0.0)?;
    time_values.set("elapsed", 0.0)?;
    time_values.set("unscaled_elapsed", 0.0)?;
    time_values.set("fixed_dt", 1.0 / 60.0)?;
    time_values.set("frame", 0_u64)?;
    let indexed_time_values = time_values.clone();
    let time_metatable = lua.create_table()?;
    time_metatable.set(
        "__index",
        lua.create_function({
            let active_call = active_call.clone();
            let time_scale = time_scale.clone();
            move |_, (_table, key): (Table, Value)| {
                if is_time_scale_key(&key) {
                    let scale = if active_call.get().is_null() {
                        time_scale.get()
                    } else {
                        with_active_call(&active_call, |transaction| Ok(transaction.time_scale()))?
                    };
                    return Ok(Value::Number(f64::from(scale)));
                }
                indexed_time_values.raw_get(key)
            }
        })?,
    )?;
    time_metatable.set(
        "__newindex",
        lua.create_function({
            let active_call = active_call.clone();
            move |_, (_table, key, value): (Table, Value, Value)| {
                if !is_time_scale_key(&key) {
                    return Ok(());
                }
                let scale = match value {
                    Value::Number(scale) => scale as f32,
                    Value::Integer(scale) => scale as f32,
                    _ => return Err(LuaError::runtime("time scale must be a number")),
                };
                with_active_call(&active_call, |transaction| {
                    transaction
                        .stage_time_scale(scale)
                        .map_err(LuaError::runtime)
                })
            }
        })?,
    )?;
    time_values.set_metatable(Some(time_metatable))?;
    let rve = lua.create_table()?;
    rve.set("time", time_values.clone())?;
    crate::math_lua::install(&lua, &rve)?;
    rve.set("asset", asset_function(&lua)?)?;
    rve.set("save", save_table(&lua, active_call.clone())?)?;
    let timer = lua.create_table()?;
    timer.set("after", timer_after_function(&lua, active_call.clone())?)?;
    timer.set("every", timer_every_function(&lua, active_call.clone())?)?;
    timer.set("cancel", timer_cancel_function(&lua, active_call.clone())?)?;
    rve.set("timer", timer)?;
    rve.set(
        "start_coroutine",
        lua.create_function({
            let active_call = active_call.clone();
            move |lua, callback: Function| {
                let thread = lua.create_thread(callback)?;
                with_active_call(&active_call, |transaction| {
                    transaction
                        .start_coroutine(thread)
                        .map_err(LuaError::runtime)
                })
            }
        })?,
    )?;
    rve.set(
        "log",
        log_function(
            &lua,
            tostring.clone(),
            events.clone(),
            active_source.clone(),
            ScriptEventLevel::Info,
        )?,
    )?;
    rve.set(
        "log_info",
        log_function(
            &lua,
            tostring.clone(),
            events.clone(),
            active_source.clone(),
            ScriptEventLevel::Info,
        )?,
    )?;
    rve.set(
        "log_warn",
        log_function(
            &lua,
            tostring.clone(),
            events.clone(),
            active_source.clone(),
            ScriptEventLevel::Warn,
        )?,
    )?;
    rve.set(
        "log_error",
        log_function(
            &lua,
            tostring.clone(),
            events.clone(),
            active_source.clone(),
            ScriptEventLevel::Error,
        )?,
    )?;
    let console = lua.create_table()?;
    console.set(
        "write",
        lua.create_function({
            let events = events.clone();
            let active_source = active_source.clone();
            move |_, (text, level): (LuaString, Option<String>)| {
                let level = match level.as_deref().unwrap_or("info") {
                    "log" | "info" => ScriptEventLevel::Info,
                    "warning" | "warn" => ScriptEventLevel::Warn,
                    "error" => ScriptEventLevel::Error,
                    other => {
                        return Err(LuaError::runtime(format!(
                            "unknown console level {other:?}"
                        )));
                    }
                };
                if !events.borrow().has_log_capacity() {
                    events.borrow_mut().record_dropped_log();
                    return Ok(());
                }
                let message = bounded_lua_log_message(&text.as_bytes());
                events.borrow_mut().push_log(ScriptEvent {
                    level,
                    source: active_source.borrow().clone(),
                    message,
                });
                Ok(())
            }
        })?,
    )?;
    console.set(
        "register",
        register_command_function(
            &lua,
            active_owner.clone(),
            commands.clone(),
            commands_generation.clone(),
        )?,
    )?;
    console.set(
        "unregister",
        unregister_command_function(&lua, active_owner, commands, commands_generation)?,
    )?;
    rve.set("console", console)?;
    rve.set("find", find_function(&lua, active_call.clone())?)?;
    for (name, function) in object_finders(&lua, active_call.clone())? {
        rve.set(name, function)?;
    }
    let physics = lua.create_table()?;
    physics.set(
        "raycast",
        physics_raycast_function(&lua, active_call.clone())?,
    )?;
    physics_api::add_query_functions(&lua, &physics, active_call.clone())?;
    rve.set("physics", physics)?;
    rve.set(
        "active_camera",
        active_camera_function(&lua, active_call.clone())?,
    )?;
    rve.set("spawn", spawn_function(&lua, active_call.clone())?)?;
    rve.set("destroy", destroy_function(&lua, active_call.clone())?)?;
    rve.set("scene", scene_table(&lua, active_call.clone())?)?;
    rve.set("app", app_table(&lua, active_call.clone())?)?;
    rve.set("prefabs", prefabs_table(&lua, active_call.clone())?)?;
    rve.set("send", send_function(&lua, active_call.clone())?)?;
    rve.set("broadcast", broadcast_function(&lua, active_call.clone())?)?;
    rve.set("audio", audio_table(&lua, active_call.clone())?)?;
    rve.set("net", net_table(&lua, active_call.clone())?)?;
    rve.set("ui", ui_lua::ui_table(&lua, active_call.clone())?)?;
    rve.set("input", input_table(&lua, active_call)?)?;
    globals.set("rve", rve)?;
    lua.load(
        r#"
        rve.wait = function(seconds)
            assert(type(seconds) == "number" and seconds >= 0 and seconds < math.huge,
                "wait seconds must be finite and nonnegative")
            return coroutine.yield("seconds", seconds)
        end
        rve.wait_frames = function(frames)
            assert(type(frames) == "number" and frames >= 0 and frames < math.huge
                and frames % 1 == 0, "wait_frames requires a nonnegative integer")
            return coroutine.yield("frames", frames)
        end
        rve.wait_until = function(predicate)
            assert(type(predicate) == "function", "wait_until requires a function")
            while not predicate() do
                coroutine.yield("frames", 1)
            end
        end
        "#,
    )
    .exec()?;
    Ok(LuaState {
        lua,
        base_environment: globals,
        time: time_values,
        call_deadline,
    })
}

pub(crate) fn compile_class(
    lua: &Lua,
    base_environment: &Table,
    path: &str,
    source: &str,
) -> Result<(Table, Option<ParamSchema>), String> {
    let environment =
        script_environment(lua, base_environment).map_err(|error| error.to_string())?;
    let value = lua
        .load(source)
        .set_name(path)
        .set_mode(ChunkMode::Text)
        .set_environment(environment)
        .eval::<Value>()
        .map_err(|error| error.to_string())?;
    let class = match value {
        Value::Table(class) => class,
        _ => return Err("script chunk must return a class table".to_owned()),
    };
    for callback in [
        "on_start",
        "on_enable",
        "on_disable",
        "on_update",
        "on_fixed_update",
        "on_late_update",
        "on_message",
        "on_destroy",
        "on_collision_enter",
        "on_collision_exit",
        "on_trigger_enter",
        "on_trigger_exit",
        "on_trigger_stay",
    ] {
        match class
            .get::<Value>(callback)
            .map_err(|error| error.to_string())?
        {
            Value::Nil | Value::Function(_) => {}
            _ => return Err(format!("class field {callback:?} must be a function")),
        }
    }
    let schema = class_properties(&class)?;
    Ok((class, schema))
}

/// Схема из поля класса `properties`: таблица `name = { type = ... }` (поля по имени)
/// или массив `{ { name = ..., type = ... }, ... }` (порядок массива).
fn class_properties(class: &Table) -> Result<Option<ParamSchema>, String> {
    let properties = match class
        .get::<Value>("properties")
        .map_err(|error| error.to_string())?
    {
        Value::Nil => return Ok(None),
        Value::Table(properties) => properties,
        _ => return Err("class field \"properties\" must be a table".to_owned()),
    };
    let mut named = Vec::new();
    let mut listed = Vec::new();
    properties
        .for_each::<Value, Value>(|key, value| {
            let Value::Table(entry) = value else {
                return Err(LuaError::runtime("each property must be a table"));
            };
            match key {
                Value::String(name) => {
                    let name = name
                        .to_str()
                        .map_err(|_| LuaError::runtime("property names must be UTF-8"))?
                        .to_owned();
                    named.push((name, entry));
                }
                key => match sequence_index(key) {
                    Some(index) => listed.push((index, entry)),
                    None => {
                        return Err(LuaError::runtime(
                            "properties keys must be names or array indices",
                        ));
                    }
                },
            }
            Ok(())
        })
        .map_err(|error| format!("properties: {error}"))?;
    if !named.is_empty() && !listed.is_empty() {
        return Err("properties must be either a name table or an array, not both".to_owned());
    }
    let mut specs = Vec::with_capacity(named.len() + listed.len());
    if listed.is_empty() {
        named.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (name, entry) in named {
            if !matches!(entry.get::<Value>("name"), Ok(Value::Nil)) {
                return Err(format!(
                    "properties.{name}: field \"name\" is only used in the array form"
                ));
            }
            specs.push(property_spec(name, &entry)?);
        }
    } else {
        listed.sort_by_key(|(index, _)| *index);
        if listed
            .iter()
            .enumerate()
            .any(|(offset, (index, _))| *index != offset + 1)
        {
            return Err("properties array must have consecutive indices 1..n".to_owned());
        }
        for (index, entry) in listed {
            let name = match entry.get::<Value>("name") {
                Ok(Value::String(name)) => name
                    .to_str()
                    .map_err(|_| format!("properties[{index}]: name must be UTF-8"))?
                    .to_owned(),
                _ => return Err(format!("properties[{index}]: name must be a string")),
            };
            specs.push(property_spec(name, &entry)?);
        }
    }
    ParamSchema::new(specs)
        .map(Some)
        .map_err(|error| format!("properties: {error}"))
}

fn property_spec(name: String, entry: &Table) -> Result<ParamSpec, String> {
    let context = |message: &str| format!("properties.{name}: {message}");
    entry
        .for_each::<Value, Value>(|key, _| match key {
            Value::String(key)
                if matches!(
                    key.as_bytes().as_ref(),
                    b"name"
                        | b"type"
                        | b"default"
                        | b"min"
                        | b"max"
                        | b"label"
                        | b"component"
                        | b"script"
                        | b"asset"
                        | b"values"
                        | b"of"
                ) =>
            {
                Ok(())
            }
            _ => Err(LuaError::runtime("unknown field in script parameter")),
        })
        .map_err(|error| context(&error.to_string()))?;
    let kind = property_kind(entry, 0).map_err(|message| context(&message))?;
    let default = match entry.get::<Value>("default") {
        Ok(Value::Nil) => kind.zero(),
        Ok(value) => lua_param_value(value).map_err(|error| context(&error.to_string()))?,
        Err(error) => return Err(context(&error.to_string())),
    };
    let bound = |field: &str| match entry.get::<Value>(field) {
        Ok(Value::Nil) => Ok(None),
        Ok(Value::Integer(value)) => Ok(Some(value as f64)),
        Ok(Value::Number(value)) => Ok(Some(value)),
        _ => Err(context(&format!("{field} must be a number"))),
    };
    let label = match entry.get::<Value>("label") {
        Ok(Value::Nil) => None,
        Ok(Value::String(label)) => Some(
            label
                .to_str()
                .map_err(|_| context("label must be UTF-8"))?
                .to_owned(),
        ),
        _ => return Err(context("label must be a string")),
    };
    Ok(ParamSpec {
        min: bound("min")?,
        max: bound("max")?,
        name,
        kind,
        default,
        label,
    })
}

fn property_kind(entry: &Table, array_depth: usize) -> Result<ParamKind, String> {
    let kind = match entry.get::<Value>("type") {
        Ok(Value::String(kind)) => kind
            .to_str()
            .map_err(|_| "type must be UTF-8".to_owned())?
            .to_owned(),
        _ => return Err("type must be a string".to_owned()),
    };
    let allowed = match kind.as_str() {
        "component" => &["component", "script"][..],
        "asset" => &["asset"][..],
        "enum" => &["values"][..],
        "array" => &["of"][..],
        _ => &[][..],
    };
    entry
        .for_each::<Value, Value>(|key, _| match key {
            Value::String(key)
                if matches!(
                    key.as_bytes().as_ref(),
                    b"name" | b"type" | b"default" | b"min" | b"max" | b"label"
                ) || allowed
                    .iter()
                    .any(|field| key.as_bytes() == field.as_bytes()) =>
            {
                Ok(())
            }
            _ => Err(LuaError::runtime("field does not match parameter type")),
        })
        .map_err(|error| error.to_string())?;
    Ok(match kind.as_str() {
        "component" => {
            let component = entry.get::<Value>("component").ok();
            let script = entry.get::<Value>("script").ok();
            match (component, script) {
                (Some(Value::String(component)), None | Some(Value::Nil)) => {
                    let component = component
                        .to_str()
                        .map_err(|_| "component must be UTF-8".to_owned())?;
                    ComponentKind::parse(&component)
                        .map(ParamKind::Component)
                        .ok_or_else(|| format!("unknown component {component:?}"))?
                }
                (None | Some(Value::Nil), Some(Value::String(script))) => {
                    let script = script
                        .to_str()
                        .map_err(|_| "script must be UTF-8".to_owned())?;
                    if script.is_empty() {
                        return Err("script component reference must not be empty".to_owned());
                    }
                    ParamKind::Component(ComponentKind::Script(script.to_owned()))
                }
                _ => return Err("component requires exactly one of component or script".to_owned()),
            }
        }
        "asset" => match entry.get::<Value>("asset") {
            Ok(Value::Nil) => ParamKind::Asset(None),
            Ok(Value::String(asset)) => {
                let asset = asset
                    .to_str()
                    .map_err(|_| "asset kind must be UTF-8".to_owned())?;
                ParamKind::Asset(Some(
                    AssetKind::parse(&asset)
                        .ok_or_else(|| format!("unknown asset kind {asset:?}"))?,
                ))
            }
            _ => return Err("asset must be a string".to_owned()),
        },
        "enum" => {
            let values = match entry.get::<Value>("values") {
                Ok(Value::Table(values)) => values,
                _ => return Err("enum requires a values array".to_owned()),
            };
            let mut items = Vec::new();
            values
                .for_each::<Value, Value>(|key, value| {
                    let Some(index) = sequence_index(key) else {
                        return Err(LuaError::runtime("enum values must be a sequence"));
                    };
                    let Value::String(value) = value else {
                        return Err(LuaError::runtime("enum values must be strings"));
                    };
                    let value = value
                        .to_str()
                        .map_err(|_| LuaError::runtime("enum values must be UTF-8"))?
                        .to_owned();
                    items.push((index, value));
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
            items.sort_by_key(|(index, _)| *index);
            if items
                .iter()
                .enumerate()
                .any(|(index, (key, _))| *key != index + 1)
            {
                return Err("enum values must have consecutive indices 1..n".to_owned());
            }
            ParamKind::Enum(items.into_iter().map(|(_, value)| value).collect())
        }
        "array" => {
            if array_depth >= MAX_PROPERTY_ARRAY_DEPTH {
                return Err(format!(
                    "array property nesting exceeds maximum depth of {MAX_PROPERTY_ARRAY_DEPTH}"
                ));
            }
            let of = match entry.get::<Value>("of") {
                Ok(Value::Table(of)) => of,
                _ => return Err("array requires an of table".to_owned()),
            };
            ParamKind::Array(Box::new(property_kind(&of, array_depth + 1)?))
        }
        primitive => ParamKind::parse(primitive)
            .ok_or_else(|| format!("unknown parameter type {primitive:?}"))?,
    })
}

/// Схема параметров lite-скрипта без создания экземпляров: chunk выполняется в отдельном
/// Lua с теми же ограничениями, что и при загрузке сцены. API движка (`rve.find` и др.)
/// на верхнем уровне chunk недоступен, как и при загрузке.
pub(crate) fn properties(
    label: &str,
    source: &str,
    interrupted: Arc<AtomicBool>,
) -> Result<Option<ParamSchema>, String> {
    let state = create_lua(
        Rc::new(RefCell::new(ScriptEventQueue::default())),
        Rc::new(RefCell::new(label.to_owned())),
        Rc::new(Cell::new(std::ptr::null_mut())),
        Rc::new(Cell::new(None)),
        Rc::new(RefCell::new(std::collections::BTreeMap::new())),
        Rc::new(Cell::new(0)),
        CallDeadline::new(None, interrupted),
        Rc::new(Cell::new(1.0)),
    )
    .map_err(|error| error.to_string())?;
    compile_class(&state.lua, &state.base_environment, label, source).map(|(_, schema)| schema)
}

pub(crate) struct LuaInstance {
    class: Table,
    instance: Table,
    transform: mlua::AnyUserData,
    active_call: ActiveCall,
    property_names: Vec<String>,
    property_values: ScriptParams,
    object_refs: std::collections::HashMap<String, ScriptObjectRef>,
}

pub(crate) struct LuaInstanceInit<'a> {
    pub(crate) object_id: &'a str,
    pub(crate) object_name: &'a str,
    pub(crate) params: &'a ScriptParams,
    pub(crate) property_names: Vec<String>,
    pub(crate) transform: ScriptTransform,
    pub(crate) object_refs: &'a std::collections::HashMap<String, ScriptObjectRef>,
    pub(crate) object_handle: ObjectHandle,
}

pub(crate) struct LitePhysicsEvent {
    pub(crate) callback: &'static str,
    pub(crate) other: ObjectHandle,
    pub(crate) point: Vec3,
    pub(crate) normal: Vec3,
    pub(crate) has_contact: bool,
}

impl LuaInstance {
    pub(crate) fn new(
        lua: &Lua,
        class: Table,
        init: LuaInstanceInit<'_>,
        active_call: ActiveCall,
    ) -> mlua::Result<Self> {
        let params = params_table(lua, init.params, init.object_refs, active_call.clone())?;
        let object = object_table(
            lua,
            init.object_handle,
            Some(init.object_id.to_owned()),
            init.object_name.to_owned(),
            active_call.clone(),
        )?;
        let transform = lua.create_userdata(TransformUserData {
            value: init.transform,
        })?;

        let instance = lua.create_table()?;
        instance.set("params", params)?;
        instance.set("object", object)?;
        instance.set("script", script_table(lua, active_call.clone())?)?;
        instance.set("transform", transform.clone())?;
        let metatable = lua.create_table()?;
        metatable.set("__index", class.clone())?;
        instance.set_metatable(Some(metatable))?;

        Ok(Self {
            class,
            instance,
            transform,
            active_call,
            property_values: init
                .property_names
                .iter()
                .filter_map(|name| {
                    init.params
                        .get(name)
                        .map(|value| (name.clone(), value.clone()))
                })
                .collect(),
            property_names: init.property_names,
            object_refs: init.object_refs.clone(),
        })
    }

    pub(crate) fn set_transform(&self, value: ScriptTransform) -> mlua::Result<()> {
        self.transform.borrow_mut::<TransformUserData>()?.value = value;
        Ok(())
    }

    pub(crate) fn transform(&self) -> mlua::Result<ScriptTransform> {
        Ok(self.transform.borrow::<TransformUserData>()?.value)
    }

    pub(crate) fn invoke_start(&self) -> mlua::Result<()> {
        self.invoke("on_start", (self.instance.clone(),))
    }

    pub(crate) fn invoke_update(&self, dt: f32) -> mlua::Result<()> {
        self.invoke("on_update", (self.instance.clone(), dt))
    }

    pub(crate) fn invoke_fixed_update(&self, dt: f32) -> mlua::Result<()> {
        self.invoke("on_fixed_update", (self.instance.clone(), dt))
    }

    pub(crate) fn invoke_late_update(&self, dt: f32) -> mlua::Result<()> {
        self.invoke("on_late_update", (self.instance.clone(), dt))
    }

    pub(crate) fn declared_properties(&self, lua: &Lua) -> mlua::Result<ScriptParams> {
        let params: Table = self.instance.get("params")?;
        self.property_names
            .iter()
            .map(|name| {
                let value: Value = params.get(name.as_str())?;
                let value = match value {
                    Value::Nil => ParamValue::Object(String::new()),
                    value => lua_message_param_value_with_size(lua, &self.active_call, value)?.0,
                };
                Ok((name.clone(), value))
            })
            .collect()
    }

    pub(crate) fn restore_params(&self, lua: &Lua) -> mlua::Result<()> {
        let params: Table = self.instance.get("params")?;
        for (name, value) in &self.property_values {
            params.set(
                name.as_str(),
                param_value(lua, value, &self.active_call, Some(&self.object_refs))?,
            )?;
        }
        Ok(())
    }

    pub(crate) fn commit_properties(&mut self, properties: ScriptParams) {
        self.property_values = properties;
    }

    pub(crate) fn invoke_timer(&self, callback: &Function, id: u64) -> mlua::Result<()> {
        callback.call(id)
    }

    pub(crate) fn resume_coroutine(&self, thread: &Thread) -> mlua::Result<CoroutineResume> {
        let yielded = thread.resume::<MultiValue>(())?;
        if thread.status() == ThreadStatus::Finished {
            return Ok(CoroutineResume::Finished);
        }
        let mut yielded = yielded.into_iter();
        let Some(Value::String(kind)) = yielded.next() else {
            return Ok(CoroutineResume::Yielded(CoroutineYield::NextFrame));
        };
        match kind.to_str()?.as_ref() {
            "frames" => {
                let frames = match yielded.next() {
                    Some(Value::Integer(frames)) if frames >= 0 => frames as u64,
                    Some(Value::Number(frames))
                        if frames.is_finite()
                            && frames >= 0.0
                            && frames.fract() == 0.0
                            && frames <= u64::MAX as f64 =>
                    {
                        frames as u64
                    }
                    _ => {
                        return Err(LuaError::runtime(
                            "wait_frames yielded an invalid frame count",
                        ));
                    }
                };
                Ok(CoroutineResume::Yielded(CoroutineYield::Frames(frames)))
            }
            "seconds" => {
                let seconds = match yielded.next() {
                    Some(Value::Number(seconds)) => seconds,
                    Some(Value::Integer(seconds)) => seconds as f64,
                    _ => return Err(LuaError::runtime("wait yielded an invalid duration")),
                };
                if !seconds.is_finite() || seconds < 0.0 {
                    return Err(LuaError::runtime("wait yielded an invalid duration"));
                }
                Ok(CoroutineResume::Yielded(CoroutineYield::Seconds(seconds)))
            }
            _ => Ok(CoroutineResume::Yielded(CoroutineYield::NextFrame)),
        }
    }

    pub(crate) fn invoke_enable(&self) -> mlua::Result<()> {
        self.invoke("on_enable", (self.instance.clone(),))
    }

    pub(crate) fn invoke_disable(&self) -> mlua::Result<()> {
        self.invoke("on_disable", (self.instance.clone(),))
    }

    pub(crate) fn invoke_destroy(&self) -> mlua::Result<()> {
        self.invoke("on_destroy", (self.instance.clone(),))
    }

    pub(crate) fn has_message_callback(&self) -> mlua::Result<bool> {
        self.class
            .get::<Value>("on_message")
            .map(|value| !matches!(value, Value::Nil))
    }

    pub(crate) fn invoke_message(
        &self,
        lua: &Lua,
        sender: &str,
        name: &str,
        value: &ParamValue,
    ) -> mlua::Result<()> {
        self.invoke(
            "on_message",
            (
                self.instance.clone(),
                sender,
                name,
                param_value(lua, value, &self.active_call, None)?,
            ),
        )
    }

    /// Есть ли у класса функция `name`.
    pub(crate) fn has_callback(&self, name: &str) -> mlua::Result<bool> {
        self.class
            .get::<Value>(name)
            .map(|value| !matches!(value, Value::Nil))
    }

    pub(crate) fn invoke_net_event(&self, lua: &Lua, event: &NetEvent) -> mlua::Result<()> {
        match event {
            NetEvent::Message {
                peer, name, value, ..
            } => self.invoke(
                "on_net_message",
                (
                    self.instance.clone(),
                    *peer,
                    name.as_str(),
                    param_value(lua, value, &self.active_call, None)?,
                ),
            ),
            NetEvent::PeerConnected { peer } => {
                self.invoke("on_peer_connected", (self.instance.clone(), *peer))
            }
            NetEvent::PeerDisconnected { peer, reason } => self.invoke(
                "on_peer_disconnected",
                (self.instance.clone(), *peer, reason.as_str()),
            ),
        }
    }

    pub(crate) fn invoke_ui_event(
        &self,
        lua: &Lua,
        active_call: &ActiveCall,
        event: &UiDelivery,
    ) -> mlua::Result<()> {
        let table = ui_lua::event_table(lua, event, active_call.clone())?;
        self.invoke("on_ui_event", (self.instance.clone(), table))
    }

    pub(crate) fn invoke_physics_event(
        &self,
        lua: &Lua,
        active_call: &ActiveCall,
        event: LitePhysicsEvent,
    ) -> mlua::Result<()> {
        let other_data = with_active_call(active_call, |transaction| {
            Ok(transaction.object_identity(event.other))
        })?;
        // Без идентичности `other` нельзя передать скрипту; выживший объект не должен отключаться
        // из-за того, что привязка тела пережила окно удаления.
        let Some((id, name)) = other_data else {
            return Ok(());
        };
        let other = object_table(lua, event.other, id, name, active_call.clone())?;
        if event.has_contact {
            let contact = lua.create_table()?;
            contact.set("point", vector_table(lua, event.point.to_array())?)?;
            contact.set("normal", vector_table(lua, event.normal.to_array())?)?;
            self.invoke(event.callback, (self.instance.clone(), other, contact))
        } else {
            self.invoke(event.callback, (self.instance.clone(), other))
        }
    }

    fn invoke<A>(&self, name: &str, args: A) -> mlua::Result<()>
    where
        A: mlua::IntoLuaMulti,
    {
        match self.class.get::<Value>(name)? {
            Value::Nil => Ok(()),
            Value::Function(function) => function.call(args),
            _ => Err(LuaError::runtime(format!(
                "class field {name:?} is no longer a function"
            ))),
        }
    }
}

fn timer_after_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, (delay, callback): (f64, Function)| {
        with_active_call(&active_call, |transaction| {
            transaction
                .schedule_lua_timer(delay, None, callback)
                .map_err(LuaError::runtime)
        })
    })
}

fn timer_every_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, (interval, callback): (f64, Function)| {
        with_active_call(&active_call, |transaction| {
            transaction
                .schedule_lua_timer(interval, Some(interval), callback)
                .map_err(LuaError::runtime)
        })
    })
}

fn timer_cancel_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, id: u64| {
        with_active_call(&active_call, |transaction| {
            transaction.cancel_timer(id).map_err(LuaError::runtime)
        })
    })
}

fn is_time_scale_key(key: &Value) -> bool {
    matches!(key, Value::String(key) if key.to_str().is_ok_and(|key| key.as_ref() == "scale"))
}

pub(crate) fn set_time(time_table: &Table, time: ScriptFrameTime) -> mlua::Result<()> {
    time_table.raw_set("dt", time.dt)?;
    time_table.raw_set("unscaled_dt", time.unscaled_dt)?;
    time_table.raw_set("elapsed", time.elapsed)?;
    time_table.raw_set("unscaled_elapsed", time.unscaled_elapsed)?;
    time_table.raw_set("fixed_dt", time.fixed_dt)?;
    // Preserve integer frames where possible; Lua's native integer is signed.
    if let Ok(frame) = i64::try_from(time.frame) {
        time_table.raw_set("frame", frame)?;
    } else {
        time_table.raw_set("frame", time.frame as f64)?;
    }
    Ok(())
}

fn log_function(
    lua: &Lua,
    tostring: Function,
    events: EventQueue,
    active_source: ActiveSource,
    level: ScriptEventLevel,
) -> mlua::Result<Function> {
    lua.create_function(move |_, arguments: MultiValue| {
        if !events.borrow().has_log_capacity() {
            events.borrow_mut().record_dropped_log();
            return Ok(());
        }
        let mut message = String::new();
        let mut truncated = false;
        for (index, argument) in arguments.into_iter().enumerate() {
            let string = tostring.call::<LuaString>(argument)?;
            if truncated {
                continue;
            }
            let part = bounded_lua_log_message(&string.as_bytes());
            truncated = append_log_piece(&mut message, &part, index != 0);
        }
        events.borrow_mut().push_log(ScriptEvent {
            level,
            source: active_source.borrow().clone(),
            message,
        });
        Ok(())
    })
}

fn append_log_piece(message: &mut String, part: &str, separator: bool) -> bool {
    let joined_len = message
        .len()
        .saturating_add(usize::from(separator))
        .saturating_add(part.len());
    if joined_len <= MAX_LOG_MESSAGE_BYTES {
        if separator {
            message.push(' ');
        }
        message.push_str(part);
        return false;
    }

    let content_limit = MAX_LOG_MESSAGE_BYTES - LOG_TRUNCATION_MARKER.len();
    truncate_to_boundary(message, content_limit);
    if separator && message.len() < content_limit {
        message.push(' ');
    }
    let available = content_limit - message.len();
    let mut end = part.len().min(available);
    while !part.is_char_boundary(end) {
        end -= 1;
    }
    message.push_str(&part[..end]);
    message.push_str(LOG_TRUNCATION_MARKER);
    true
}

fn register_command_function(
    lua: &Lua,
    active_owner: ActiveOwner,
    commands: LuaCommands,
    generation: CommandGeneration,
) -> mlua::Result<Function> {
    lua.create_function(
        move |lua, (name, callback, description): (String, Function, Option<String>)| {
            let owner = active_owner.get().ok_or_else(|| {
                LuaError::runtime("rve.console.register is only available during script callbacks")
            })?;
            let spec = CommandSpec::new(name, description.unwrap_or_default())
                .map_err(|error| LuaError::runtime(error.to_string()))?;
            let duplicate = commands.borrow().contains_key(&spec.name);
            if duplicate {
                return Err(LuaError::runtime(format!(
                    "console command {:?} is already registered",
                    spec.name
                )));
            }
            let callback = lua.create_registry_value(callback)?;
            commands.borrow_mut().insert(
                spec.name.clone(),
                RegisteredLuaCommand {
                    owner,
                    spec,
                    callback: RegisteredCommandCallback::Lua(callback),
                },
            );
            generation.set(generation.get().wrapping_add(1));
            Ok(())
        },
    )
}

fn unregister_command_function(
    lua: &Lua,
    active_owner: ActiveOwner,
    commands: LuaCommands,
    generation: CommandGeneration,
) -> mlua::Result<Function> {
    lua.create_function(move |lua, name: String| {
        let owner = active_owner.get().ok_or_else(|| {
            LuaError::runtime("rve.console.unregister is only available during script callbacks")
        })?;
        let command = {
            let mut commands = commands.borrow_mut();
            let command = commands
                .get(&name)
                .ok_or_else(|| LuaError::runtime(format!("unknown console command {name:?}")))?;
            if command.owner != owner {
                return Err(LuaError::runtime(format!(
                    "script cannot unregister command {name:?} owned by another instance"
                )));
            }
            commands.remove(&name).expect("command exists after lookup")
        };
        generation.set(generation.get().wrapping_add(1));
        if let RegisteredCommandCallback::Lua(callback) = command.callback {
            lua.remove_registry_value(callback)?;
        }
        Ok(())
    })
}

pub(crate) fn with_active_call<T>(
    active_call: &ActiveCall,
    callback: impl FnOnce(&mut ScriptTransaction) -> mlua::Result<T>,
) -> mlua::Result<T> {
    let transaction = active_call.get();
    if transaction.is_null() {
        return Err(LuaError::runtime(
            "RVE host API is only available during script calls",
        ));
    }
    // SAFETY: ScriptHost sets this pointer immediately before entering Lua and clears it as soon
    // as the callback returns. Lua runs synchronously on the owning scripts thread.
    callback(unsafe { &mut *transaction })
}

/// Ключ реестра Lua с таблицей «объект → хендл»; ключи слабые, поэтому записи уходят вместе с объектами.
const OBJECT_HANDLES_KEY: &str = "rve.object_handles";

fn create_object_handles(lua: &Lua) -> mlua::Result<()> {
    let handles = lua.create_table()?;
    let mode = lua.create_table()?;
    mode.set("__mode", "k")?;
    handles.set_metatable(Some(mode))?;
    lua.set_named_registry_value(OBJECT_HANDLES_KEY, handles)
}

fn register_object_handle(lua: &Lua, object: &Table, handle: ObjectHandle) -> mlua::Result<()> {
    let value = handle
        .to_abi()
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| LuaError::runtime("object handle cannot be represented in Lua"))?;
    let handles: Table = lua.named_registry_value(OBJECT_HANDLES_KEY)?;
    handles.raw_set(object.clone(), value)
}

/// Хендл объекта, который вернул `rve.find`, `rve.spawn` или методы объекта.
pub(crate) fn object_handle_of(lua: &Lua, object: &Table) -> mlua::Result<Option<ObjectHandle>> {
    let handles: Table = lua.named_registry_value(OBJECT_HANDLES_KEY)?;
    Ok(match handles.raw_get::<Value>(object.clone())? {
        Value::Integer(value) => u64::try_from(value).ok().and_then(ObjectHandle::from_abi),
        _ => None,
    })
}

/// Живой ли объект для метода. Мёртвый объект пишет Warn один раз на пару «экземпляр + объект»;
/// вызывающий отвечает `nil` для чтения и `false` для записи.
fn object_alive(active_call: &ActiveCall, handle: ObjectHandle) -> mlua::Result<bool> {
    with_active_call(active_call, |transaction| {
        Ok(transaction.object_access(handle) == ObjectState::Alive)
    })
}

fn find_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |lua, id: String| {
        let found = with_active_call(&active_call, |transaction| {
            Ok(transaction.handle_by_id(&id).and_then(|handle| {
                Some((
                    handle,
                    transaction.object_id(handle)?.to_owned(),
                    transaction.object_name(handle)?.to_owned(),
                ))
            }))
        })?;
        let Some((handle, id, name)) = found else {
            return Ok(Option::<Table>::None);
        };
        Ok(Some(object_table(
            lua,
            handle,
            Some(id),
            name,
            active_call.clone(),
        )?))
    })
}

fn active_camera_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |lua, _arguments: MultiValue| {
        let active = with_active_call(&active_call, |transaction| {
            Ok(transaction.active_camera().and_then(|handle| {
                Some((
                    handle,
                    transaction.object_id(handle)?.to_owned(),
                    transaction.object_name(handle)?.to_owned(),
                ))
            }))
        })?;
        let Some((handle, id, name)) = active else {
            return Ok(Option::<Table>::None);
        };
        Ok(Some(object_table(
            lua,
            handle,
            Some(id),
            name,
            active_call.clone(),
        )?))
    })
}

/// Таблица объекта. `id` равен `None`, если у объекта без явного id ещё нет имени в мире
/// (pending spawn). Методы захватывают хендл, поэтому устаревшая таблица не попадает на чужой объект.
fn object_table(
    lua: &Lua,
    handle: ObjectHandle,
    id: Option<String>,
    name: String,
    active_call: ActiveCall,
) -> mlua::Result<Table> {
    let object = lua.create_table()?;
    object.set("id", id)?;
    object.set("name", name)?;
    register_object_handle(lua, &object, handle)?;

    let get_active = active_call.clone();
    object.set(
        "get_transform",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_active, handle)? {
                return Ok(Value::Nil);
            }
            let transform = with_active_call(&get_active, |transaction| {
                Ok(transaction.local_transform(handle))
            })?;
            match transform {
                Some(value) => Ok(Value::UserData(
                    lua.create_userdata(TransformUserData { value })?,
                )),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_active = active_call.clone();
    object.set(
        "set_transform",
        lua.create_function(move |_, arguments: MultiValue| {
            let mut arguments = arguments.into_iter();
            let first = arguments.next();
            let value = match first {
                Some(Value::Table(_)) => arguments.next(),
                value => value,
            };
            let Some(Value::UserData(value)) = value else {
                return Err(LuaError::runtime(
                    "object.set_transform expects a transform from get_transform()",
                ));
            };
            let transform = value.borrow::<TransformUserData>()?.value;
            let (writable, state, accepted) = with_active_call(&set_active, |transaction| {
                let state = transaction.object_access(handle);
                let accepted = state == ObjectState::Alive
                    && transaction.set_local_transform(handle, transform);
                Ok((transaction.writable(), state, accepted))
            })?;
            if accepted {
                Ok(true)
            } else if state == ObjectState::Alive && !writable {
                Err(LuaError::runtime(
                    "object.set_transform is unavailable during script creation",
                ))
            } else {
                Ok(false)
            }
        })?,
    )?;

    let camera_settings_active = active_call.clone();
    object.set(
        "get_camera_settings",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&camera_settings_active, handle)? {
                return Ok(Value::Nil);
            }
            let settings = with_active_call(&camera_settings_active, |transaction| {
                Ok(transaction
                    .camera_settings(handle)
                    .zip(transaction.post_process(handle)))
            })?;
            match settings {
                Some((settings, post)) => {
                    Ok(Value::Table(camera_settings_table(lua, settings, post)?))
                }
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_camera_settings_active = active_call.clone();
    object.set(
        "set_camera_settings",
        lua.create_function(move |_, arguments: MultiValue| {
            let mut arguments = arguments.into_iter();
            let settings_table = match arguments.next() {
                Some(Value::Table(_)) => arguments.next(),
                value => value,
            };
            let Some(Value::Table(settings_table)) = settings_table else {
                return Err(LuaError::runtime(
                    "object.set_camera_settings expects a partial settings table",
                ));
            };
            if arguments.next().is_some() {
                return Err(LuaError::runtime(
                    "object.set_camera_settings expects one settings table",
                ));
            }
            if !object_alive(&set_camera_settings_active, handle)? {
                return Ok(false);
            }
            let (writable, mut settings) =
                with_active_call(&set_camera_settings_active, |transaction| {
                    Ok((transaction.writable(), transaction.camera_settings(handle)))
                })?;
            let Some(mut settings) = settings.take() else {
                return Err(LuaError::runtime(
                    "object.set_camera_settings target object has no camera",
                ));
            };
            if !writable {
                return Err(LuaError::runtime(
                    "object.set_camera_settings is unavailable during script creation",
                ));
            }
            apply_camera_settings_table(&settings_table, &mut settings)?;
            let accepted = with_active_call(&set_camera_settings_active, |transaction| {
                Ok(transaction.set_camera_settings(handle, settings))
            })?;
            Ok(accepted)
        })?,
    )?;

    let screen_count_active = active_call.clone();
    object.set(
        "screen_shader_count",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&screen_count_active, handle)? {
                return Ok(Value::Nil);
            }
            let count = with_active_call(&screen_count_active, |transaction| {
                Ok(transaction.screen_shader_count(handle))
            })?;
            Ok(count.map_or(Value::Nil, |count| Value::Integer(count as i64)))
        })?,
    )?;

    let screen_get_active = active_call.clone();
    object.set(
        "screen_shader",
        lua.create_function(move |lua, arguments: MultiValue| {
            let (slot, _) = screen_call_arguments(arguments, "object.screen_shader")?;
            if !object_alive(&screen_get_active, handle)? {
                return Ok(Value::Nil);
            }
            let state = with_active_call(&screen_get_active, |transaction| {
                Ok(transaction.screen_shader(handle, slot))
            })?;
            match state {
                Some(state) => Ok(Value::Table(screen_shader_table(lua, &state)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let screen_set_active = active_call.clone();
    object.set(
        "set_screen_shader",
        lua.create_function(move |_, arguments: MultiValue| {
            let (slot, table) = screen_call_arguments(arguments, "object.set_screen_shader")?;
            let Some(table) = table else {
                return Err(LuaError::runtime(
                    "object.set_screen_shader expects a table with enabled and params",
                ));
            };
            if !object_alive(&screen_set_active, handle)? {
                return Ok(false);
            }
            let (writable, current) = with_active_call(&screen_set_active, |transaction| {
                Ok((
                    transaction.writable(),
                    transaction.screen_shader(handle, slot),
                ))
            })?;
            if !writable {
                return Err(LuaError::runtime(
                    "object.set_screen_shader is unavailable during script creation",
                ));
            }
            let Some(mut state) = current else {
                return Err(LuaError::runtime(
                    "object.set_screen_shader target has no such screen shader slot",
                ));
            };
            apply_screen_shader_table(&table, &mut state)?;
            with_active_call(&screen_set_active, |transaction| {
                Ok(transaction.set_screen_shader(handle, slot, state))
            })
        })?,
    )?;

    let get_post_process_active = active_call.clone();
    object.set(
        "get_post_process",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_post_process_active, handle)? {
                return Ok(Value::Nil);
            }
            let settings = with_active_call(&get_post_process_active, |transaction| {
                Ok(transaction.post_process(handle))
            })?;
            match settings {
                Some(settings) => Ok(Value::Table(post_process_table(lua, settings)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_post_process_active = active_call.clone();
    object.set(
        "set_post_process",
        lua.create_function(move |_, arguments: MultiValue| {
            let mut arguments = arguments.into_iter();
            let settings_table = match arguments.next() {
                Some(Value::Table(_)) => arguments.next(),
                value => value,
            };
            let Some(Value::Table(settings_table)) = settings_table else {
                return Err(LuaError::runtime(
                    "object.set_post_process expects a partial post process table",
                ));
            };
            if arguments.next().is_some() {
                return Err(LuaError::runtime(
                    "object.set_post_process expects one post process table",
                ));
            }
            if !object_alive(&set_post_process_active, handle)? {
                return Ok(false);
            }
            let (writable, current) = with_active_call(&set_post_process_active, |transaction| {
                Ok((transaction.writable(), transaction.post_process(handle)))
            })?;
            let Some(mut settings) = current else {
                return Err(LuaError::runtime(
                    "object.set_post_process target object has no camera",
                ));
            };
            if !writable {
                return Err(LuaError::runtime(
                    "object.set_post_process is unavailable during script creation",
                ));
            }
            apply_post_process_table(&settings_table, &mut settings)?;
            settings
                .validate()
                .map_err(|error| LuaError::runtime(error.to_string()))?;
            let accepted = with_active_call(&set_post_process_active, |transaction| {
                Ok(transaction.set_post_process(handle, settings))
            })?;
            Ok(accepted)
        })?,
    )?;

    let activate_camera_active = active_call.clone();
    object.set(
        "activate_camera",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&activate_camera_active, handle)? {
                return Ok(false);
            }
            let (writable, accepted) = with_active_call(&activate_camera_active, |transaction| {
                Ok((
                    transaction.writable(),
                    transaction.set_active_camera(handle),
                ))
            })?;
            if accepted {
                Ok(true)
            } else if writable {
                Err(LuaError::runtime(
                    "object.activate_camera target object has no camera",
                ))
            } else {
                Err(LuaError::runtime(
                    "object.activate_camera is unavailable during script creation",
                ))
            }
        })?,
    )?;

    let matrix_active = active_call.clone();
    object.set(
        "world_matrix",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&matrix_active, handle)? {
                return Ok(Value::Nil);
            }
            let matrix = with_active_call(&matrix_active, |transaction| {
                Ok(transaction.world_matrix(handle))
            })?;
            match matrix {
                Some(matrix) => Ok(Value::Table(matrix_table(lua, matrix)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let destroy_active = active_call.clone();
    object.set(
        "destroy",
        lua.create_function(move |_, arguments: MultiValue| {
            let delay = delay_argument(arguments, "object.destroy")?;
            with_active_call(&destroy_active, |transaction| {
                transaction
                    .destroy(handle, delay)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let alive_active = active_call.clone();
    object.set(
        "is_alive",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&alive_active, |transaction| {
                Ok(transaction.object_state(handle) == ObjectState::Alive)
            })
        })?,
    )?;

    let pending_active = active_call.clone();
    object.set(
        "is_pending",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&pending_active, |transaction| {
                Ok(transaction.object_state(handle) == ObjectState::Pending)
            })
        })?,
    )?;
    add_hierarchy_object_api(lua, &object, handle, active_call.clone())?;
    add_script_ref_api(lua, &object, handle, active_call.clone())?;
    add_audio_object_api(lua, &object, handle, active_call.clone())?;
    component_api::add_component_methods(lua, &object, handle, active_call.clone())?;
    add_animation_object_api(lua, &object, handle, active_call.clone())?;
    add_net_object_api(lua, &object, handle, active_call.clone())?;
    add_physics_object_api(lua, &object, handle, active_call)?;
    Ok(object)
}

/// Задержка `destroy`: необязательный `self` и необязательное число; по умолчанию 0.
fn delay_argument(arguments: MultiValue, name: &str) -> mlua::Result<f64> {
    let mut arguments = arguments.into_iter();
    let mut value = arguments.next();
    if matches!(value, Some(Value::Table(_))) {
        value = arguments.next();
    }
    match value {
        None | Some(Value::Nil) => Ok(0.0),
        Some(Value::Integer(delay)) => Ok(delay as f64),
        Some(Value::Number(delay)) => Ok(delay),
        Some(_) => Err(LuaError::runtime(format!("{name} delay must be a number"))),
    }
}

/// Методы сети объекта: `net_id`, `net_owner`, `is_owner`, `has_authority`, `set_net_owner` и
/// `net_send`. Для мёртвого объекта чтения дают `nil` или `false`, записи ничего не делают.
fn add_net_object_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    let id_active = active_call.clone();
    object.set(
        "net_id",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&id_active, handle)? {
                return Ok(Option::<i64>::None);
            }
            with_active_call(&id_active, |transaction| {
                Ok(transaction
                    .net_object(handle)
                    .map(|state| i64::from(state.net_id.get())))
            })
        })?,
    )?;

    let owner_active = active_call.clone();
    object.set(
        "net_owner",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&owner_active, handle)? {
                return Ok(Option::<i64>::None);
            }
            with_active_call(&owner_active, |transaction| {
                Ok(transaction
                    .net_object(handle)
                    .and_then(|state| state.owner)
                    .map(i64::from))
            })
        })?,
    )?;

    let is_owner_active = active_call.clone();
    object.set(
        "is_owner",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&is_owner_active, handle)? {
                return Ok(false);
            }
            with_active_call(&is_owner_active, |transaction| {
                Ok(transaction.net_is_owner(handle))
            })
        })?,
    )?;

    let authority_active = active_call.clone();
    object.set(
        "has_authority",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&authority_active, handle)? {
                return Ok(false);
            }
            with_active_call(&authority_active, |transaction| {
                Ok(transaction.net_has_authority(handle))
            })
        })?,
    )?;

    let owner_set_active = active_call.clone();
    object.set(
        "set_net_owner",
        lua.create_function(move |_, arguments: MultiValue| {
            let peer = net_method_arguments(arguments, "object.set_net_owner", 1, 1)?
                .pop()
                .unwrap_or(Value::Nil);
            let peer = lua_peer(lua_integer(peer, "object.set_net_owner peer")?)?;
            if !object_alive(&owner_set_active, handle)? {
                return Ok(());
            }
            with_active_call(&owner_set_active, |transaction| {
                transaction
                    .set_net_owner(handle, peer)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let send_active = active_call;
    object.set(
        "net_send",
        lua.create_function(move |lua, arguments: MultiValue| {
            let mut values = net_method_arguments(arguments, "object.net_send", 3, 4)?;
            let options = if values.len() == 4 {
                values.pop()
            } else {
                None
            };
            let [name, value, target]: [Value; 3] = values.try_into().map_err(|_| {
                LuaError::runtime("object.net_send expects a name, value and target")
            })?;
            let name = lua_name_value(name, "object.net_send")?;
            let target = lua_net_target(target)?;
            let reliable = lua_reliable(lua_options(options)?.as_ref(), "object.net_send")?;
            let (value, _) = lua_message_param_value_with_size(lua, &send_active, value)?;
            if !object_alive(&send_active, handle)? {
                return Ok(());
            }
            with_active_call(&send_active, |transaction| {
                transaction
                    .send_net(Some(handle), &name, value.clone(), target, reliable)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    Ok(())
}

/// Аргументы метода без `self`: Lua передаёт таблицу объекта первой, поэтому её пропускаем.
fn net_method_arguments(
    arguments: MultiValue,
    function: &str,
    min: usize,
    max: usize,
) -> mlua::Result<Vec<Value>> {
    let mut values: Vec<Value> = arguments.into_iter().collect();
    if matches!(values.first(), Some(Value::Table(_))) {
        values.remove(0);
    }
    if !(min..=max).contains(&values.len()) {
        return Err(LuaError::runtime(format!(
            "{function} expects {min} to {max} arguments"
        )));
    }
    Ok(values)
}

fn lua_name_value(value: Value, function: &str) -> mlua::Result<String> {
    match value {
        Value::String(name) => lua_message_name(name),
        other => Err(LuaError::runtime(format!(
            "{function} expects the message name as a string, got {}",
            other.type_name()
        ))),
    }
}

pub(crate) fn lua_integer(value: Value, what: &str) -> mlua::Result<i64> {
    match value {
        Value::Integer(value) => Ok(value),
        Value::Number(value) if value.is_finite() && value.fract() == 0.0 => Ok(value as i64),
        other => Err(LuaError::runtime(format!(
            "{what} must be an integer, got {}",
            other.type_name()
        ))),
    }
}

fn lua_options(value: Option<Value>) -> mlua::Result<Option<Table>> {
    match value {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Table(options)) => Ok(Some(options)),
        Some(other) => Err(LuaError::runtime(format!(
            "network options must be a table, got {}",
            other.type_name()
        ))),
    }
}

/// Таблица `rve.net`: режим, участники и отправка. Ошибки записи (клиент, лимиты) — ошибки Lua.
fn net_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let net = lua.create_table()?;

    let mode_active = active_call.clone();
    net.set(
        "mode",
        lua.create_function(move |_, _arguments: MultiValue| {
            let mode = with_active_call(&mode_active, |transaction| Ok(transaction.net_mode()))?;
            Ok(net_mode_name(mode))
        })?,
    )?;

    let server_active = active_call.clone();
    net.set(
        "is_server",
        lua.create_function(move |_, _arguments: MultiValue| {
            let mode = with_active_call(&server_active, |transaction| Ok(transaction.net_mode()))?;
            Ok(matches!(
                mode,
                NetMode::Offline | NetMode::Host | NetMode::Server
            ))
        })?,
    )?;

    let client_active = active_call.clone();
    net.set(
        "is_client",
        lua.create_function(move |_, _arguments: MultiValue| {
            let mode = with_active_call(&client_active, |transaction| Ok(transaction.net_mode()))?;
            Ok(matches!(mode, NetMode::Host | NetMode::Client))
        })?,
    )?;

    let local_active = active_call.clone();
    net.set(
        "local_peer",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&local_active, |transaction| {
                Ok(i64::from(transaction.net_local_peer()))
            })
        })?,
    )?;

    let peers_active = active_call.clone();
    net.set(
        "peers",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let peers = with_active_call(&peers_active, |transaction| Ok(transaction.net_peers()))?;
            lua.create_sequence_from(peers.into_iter().map(i64::from))
        })?,
    )?;

    net.set(
        "send",
        lua.create_function(
            move |lua, (target, name, value, options): (Value, LuaString, Value, Option<Table>)| {
                let target = lua_net_target(target)?;
                let reliable = lua_reliable(options.as_ref(), "rve.net.send")?;
                let name = lua_message_name(name)?;
                let (value, _) = lua_message_param_value_with_size(lua, &active_call, value)?;
                with_active_call(&active_call, |transaction| {
                    transaction
                        .send_net(None, &name, value.clone(), target, reliable)
                        .map_err(LuaError::runtime)
                })
            },
        )?,
    )?;
    Ok(net)
}

fn net_mode_name(mode: NetMode) -> &'static str {
    match mode {
        NetMode::Offline => "offline",
        NetMode::Host => "host",
        NetMode::Server => "server",
        NetMode::Client => "client",
    }
}

/// Адресат `"server"`, `"all"` или номер участника.
fn lua_net_target(value: Value) -> mlua::Result<NetTarget> {
    match value {
        Value::String(text) => match text.to_str()?.to_owned().as_str() {
            "server" => Ok(NetTarget::Server),
            "all" => Ok(NetTarget::All),
            other => Err(LuaError::runtime(format!(
                "сетевой адресат {other:?} должен быть \"server\", \"all\" или номером участника"
            ))),
        },
        value @ (Value::Integer(_) | Value::Number(_)) => Ok(NetTarget::Peer(lua_peer(
            lua_integer(value, "сетевой адресат")?,
        )?)),
        _ => Err(LuaError::runtime(
            "сетевой адресат должен быть строкой \"server\" или \"all\" либо номером участника",
        )),
    }
}

fn lua_peer(peer: i64) -> mlua::Result<PeerId> {
    PeerId::try_from(peer).map_err(|_| {
        LuaError::runtime(format!(
            "номер участника {peer} вне диапазона 0..={}",
            PeerId::MAX
        ))
    })
}

/// Необязательный `reliable` в таблице опций; по умолчанию `true`.
fn lua_reliable(options: Option<&Table>, function: &str) -> mlua::Result<bool> {
    let Some(options) = options else {
        return Ok(true);
    };
    match options.get::<Value>("reliable")? {
        Value::Nil => Ok(true),
        value => lua_boolean(value, &format!("{function} option reliable")),
    }
}

fn add_audio_object_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    let play_active = active_call.clone();
    object.set(
        "play_sound",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&play_active, handle)? {
                return Ok(false);
            }
            with_active_call(&play_active, |transaction| {
                require_audio_source(transaction, handle, "object.play_sound")?;
                let voice = VoiceId::allocate();
                transaction
                    .stage_audio(AudioRequest::PlaySource {
                        object: handle,
                        voice,
                    })
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;

    let stop_active = active_call.clone();
    object.set(
        "stop_sound",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&stop_active, handle)? {
                return Ok(false);
            }
            with_active_call(&stop_active, |transaction| {
                require_audio_source(transaction, handle, "object.stop_sound")?;
                transaction
                    .stage_audio(AudioRequest::StopSource { object: handle })
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;

    object.set(
        "set_sound_volume",
        lua.create_function(move |_, arguments: MultiValue| {
            let value = method_argument(arguments, "object.set_sound_volume")?;
            let volume = audio_volume(&value, "object.set_sound_volume")?;
            if !object_alive(&active_call, handle)? {
                return Ok(false);
            }
            with_active_call(&active_call, |transaction| {
                require_audio_source(transaction, handle, "object.set_sound_volume")?;
                transaction
                    .stage_audio(AudioRequest::SetSourceVolume {
                        object: handle,
                        volume,
                    })
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

/// Методы аниматора объекта: `play_animation`, `set_animation_weight`, `set_animation_parameter`,
/// `set_animation_speed` и `stop_animation`. Без `animator` каждый метод возвращает ошибку.
fn add_animation_object_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    let play_active = active_call.clone();
    object.set(
        "play_animation",
        lua.create_function(move |_, arguments: MultiValue| {
            let (motion, options) = animation_motion_arguments(arguments, "object.play_animation")?;
            with_active_call(&play_active, |transaction| {
                require_animator(transaction, handle, "object.play_animation")?;
                transaction
                    .stage_animation(AnimationRequest::Play {
                        object: handle,
                        motion: motion.clone(),
                        fade: options.fade,
                        looping: options.looping,
                        then: options.then.clone(),
                    })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let weight_active = active_call.clone();
    object.set(
        "set_animation_weight",
        lua.create_function(move |_, arguments: MultiValue| {
            let values = animation_values(arguments, "object.set_animation_weight")?;
            let motion = animation_name(values.first(), "object.set_animation_weight")?;
            let weight = animation_number(values.get(1), "object.set_animation_weight", "weight")?;
            let fade = optional_fade(values.get(2), "object.set_animation_weight")?;
            with_active_call(&weight_active, |transaction| {
                require_animator(transaction, handle, "object.set_animation_weight")?;
                transaction
                    .stage_animation(AnimationRequest::SetWeight {
                        object: handle,
                        motion,
                        weight,
                        fade,
                    })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let parameter_active = active_call.clone();
    object.set(
        "set_animation_parameter",
        lua.create_function(move |_, arguments: MultiValue| {
            let values = animation_values(arguments, "object.set_animation_parameter")?;
            let name = animation_name(values.first(), "object.set_animation_parameter")?;
            let value = animation_number(values.get(1), "object.set_animation_parameter", "value")?;
            with_active_call(&parameter_active, |transaction| {
                require_animator(transaction, handle, "object.set_animation_parameter")?;
                transaction
                    .stage_animation(AnimationRequest::SetParameter {
                        object: handle,
                        name,
                        value,
                    })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let speed_active = active_call.clone();
    object.set(
        "set_animation_speed",
        lua.create_function(move |_, arguments: MultiValue| {
            let values = animation_values(arguments, "object.set_animation_speed")?;
            let speed = animation_number(values.first(), "object.set_animation_speed", "speed")?;
            if speed < 0.0 {
                return Err(LuaError::runtime(
                    "object.set_animation_speed speed must not be negative",
                ));
            }
            with_active_call(&speed_active, |transaction| {
                require_animator(transaction, handle, "object.set_animation_speed")?;
                transaction
                    .stage_animation(AnimationRequest::SetSpeed {
                        object: handle,
                        speed,
                    })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    object.set(
        "stop_animation",
        lua.create_function(move |_, arguments: MultiValue| {
            animation_values(arguments, "object.stop_animation")?;
            with_active_call(&active_call, |transaction| {
                require_animator(transaction, handle, "object.stop_animation")?;
                transaction
                    .stage_animation(AnimationRequest::Stop { object: handle })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    Ok(())
}

/// Аргументы без `self`: `obj:method(...)` передаёт таблицу объекта первой. Ни один первый
/// параметр методов аниматора не является таблицей, поэтому ведущая таблица всегда считается `self`.
fn animation_values(arguments: MultiValue, name: &str) -> mlua::Result<Vec<Value>> {
    let mut values: Vec<Value> = arguments.into_iter().collect();
    if matches!(values.first(), Some(Value::Table(_))) {
        values.remove(0);
    }
    if values.len() > 4 {
        return Err(LuaError::runtime(format!(
            "{name} received too many arguments"
        )));
    }
    Ok(values)
}

fn animation_name(value: Option<&Value>, name: &str) -> mlua::Result<String> {
    match value {
        Some(Value::String(text)) => {
            let text = text.to_str()?.to_owned();
            if text.is_empty() {
                Err(LuaError::runtime(format!("{name} name must not be empty")))
            } else {
                Ok(text)
            }
        }
        _ => Err(LuaError::runtime(format!(
            "{name} expects a motion name string"
        ))),
    }
}
/// `rve.spawn(path, opts?)`: резервирует объект и возвращает его таблицу (pending до применения).
fn spawn_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |lua, (path, options): (String, Option<Table>)| {
        let args = spawn_args(lua, options.as_ref(), &active_call)?;
        let handle = with_active_call(&active_call, |transaction| {
            transaction.spawn(&path, args).map_err(LuaError::runtime)
        })?;
        let (id, name) = with_active_call(&active_call, |transaction| {
            Ok(transaction
                .object_identity(handle)
                .unwrap_or((None, String::new())))
        })?;
        object_table(lua, handle, id, name, active_call.clone())
    })
}

fn spawn_args(
    lua: &Lua,
    options: Option<&Table>,
    active_call: &ActiveCall,
) -> mlua::Result<SpawnArgs> {
    let mut args = SpawnArgs::default();
    let Some(options) = options else {
        return Ok(args);
    };
    args.options.position = optional_vector(options, "position")?;
    args.options.rotation_deg = optional_vector(options, "rotation")?;
    args.options.scale = optional_vector(options, "scale")?;
    args.velocity = optional_vector(options, "velocity")?;
    args.options.name = optional_string(options, "name")?;
    args.options.id = optional_string(options, "id")?;
    args.options.parent = match options.get::<Value>("parent")? {
        Value::Nil => None,
        Value::Table(object) => Some(object_handle_of(lua, &object)?.ok_or_else(|| {
            LuaError::runtime("spawn parent must be an object returned by rve.find or rve.spawn")
        })?),
        Value::String(id) => {
            let id = id.to_str()?.to_owned();
            let handle =
                with_active_call(active_call, |transaction| Ok(transaction.handle_by_id(&id)))?;
            Some(
                handle.ok_or_else(|| {
                    LuaError::runtime(format!("spawn parent {id:?} does not exist"))
                })?,
            )
        }
        _ => return Err(LuaError::runtime("spawn parent must be an object or an id")),
    };
    Ok(args)
}

fn optional_vector(table: &Table, key: &str) -> mlua::Result<Option<Vec3>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Table(vector) => Ok(Some(Vec3::from_array(read_vector(&vector, key)?))),
        _ => Err(LuaError::runtime(format!(
            "spawn {key} must be a vector table"
        ))),
    }
}

fn animation_number(value: Option<&Value>, name: &str, field: &str) -> mlua::Result<f32> {
    let number = match value {
        Some(Value::Integer(value)) => *value as f64,
        Some(Value::Number(value)) => *value,
        _ => {
            return Err(LuaError::runtime(format!(
                "{name} expects a numeric {field}"
            )));
        }
    };
    let converted = number as f32;
    if !number.is_finite() || !converted.is_finite() {
        return Err(LuaError::runtime(format!("{name} {field} must be finite")));
    }
    Ok(converted)
}

fn optional_fade(value: Option<&Value>, name: &str) -> mlua::Result<f32> {
    match value {
        None | Some(Value::Nil) => Ok(0.0),
        other => {
            let fade = animation_number(other, name, "fade")?;
            if fade < 0.0 {
                return Err(LuaError::runtime(format!(
                    "{name} fade must not be negative"
                )));
            }
            Ok(fade)
        }
    }
}

/// Разобранные параметры `play_animation`.
struct LuaPlayOptions {
    fade: f32,
    looping: Option<bool>,
    then: Option<String>,
}

/// Разбирает `motion` и необязательную таблицу `{fade=, loop=, ["then"]=}`.
fn animation_motion_arguments(
    arguments: MultiValue,
    name: &str,
) -> mlua::Result<(String, LuaPlayOptions)> {
    let values = animation_values(arguments, name)?;
    let motion = animation_name(values.first(), name)?;
    let table = match values.get(1) {
        None | Some(Value::Nil) => {
            return Ok((
                motion,
                LuaPlayOptions {
                    fade: 0.0,
                    looping: None,
                    then: None,
                },
            ));
        }
        Some(Value::Table(table)) => table.clone(),
        Some(_) => {
            return Err(LuaError::runtime(format!("{name} options must be a table")));
        }
    };
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let known = match &key {
            Value::String(key) => matches!(key.to_str()?.as_ref(), "fade" | "loop" | "then"),
            _ => false,
        };
        if !known {
            return Err(LuaError::runtime(format!(
                "{name} received an unknown option"
            )));
        }
    }
    let fade_value = table.get::<Value>("fade")?;
    let fade = optional_fade(Some(&fade_value), name)?;
    let looping = match table.get::<Value>("loop")? {
        Value::Nil => None,
        Value::Boolean(value) => Some(value),
        _ => {
            return Err(LuaError::runtime(format!("{name} loop must be a boolean")));
        }
    };
    let then = match table.get::<Value>("then")? {
        Value::Nil => None,
        Value::String(text) => Some(text.to_str()?.to_owned()),
        _ => {
            return Err(LuaError::runtime(format!(
                "{name} then must be a motion name string"
            )));
        }
    };
    Ok((
        motion,
        LuaPlayOptions {
            fade,
            looping,
            then,
        },
    ))
}

fn require_animator(
    transaction: &ScriptTransaction,
    handle: ObjectHandle,
    name: &str,
) -> mlua::Result<()> {
    if transaction.has_animator(handle) {
        Ok(())
    } else {
        Err(LuaError::runtime(format!(
            "{name} requires an animator component"
        )))
    }
}
fn optional_string(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(value.to_str()?.to_owned())),
        _ => Err(LuaError::runtime(format!("spawn {key} must be a string"))),
    }
}

/// `rve.destroy(obj_or_id, delay?)`: удаление поддерева. Неизвестный id даёт `false`.
fn destroy_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |lua, (target, delay): (Value, Option<f64>)| {
        let handle = match target {
            Value::Table(object) => object_handle_of(lua, &object)?,
            Value::String(id) => {
                let id = id.to_str()?.to_owned();
                with_active_call(&active_call, |transaction| {
                    Ok(transaction.handle_by_any_id(&id))
                })?
            }
            _ => {
                return Err(LuaError::runtime(
                    "rve.destroy expects an object or an object id",
                ));
            }
        };
        let Some(handle) = handle else {
            return Ok(false);
        };
        let delay = delay.unwrap_or(0.0);
        with_active_call(&active_call, |transaction| {
            transaction
                .destroy(handle, delay)
                .map_err(LuaError::runtime)
        })
    })
}

/// `rve.prefabs.load/unload/state`: загрузка тяжёлых префабов и их состояние.
fn prefabs_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let prefabs = lua.create_table()?;
    for (name, load) in [("load", true), ("unload", false)] {
        let active = active_call.clone();
        prefabs.set(
            name,
            lua.create_function(move |_, path: String| {
                let request = if load {
                    PrefabRequest::Load(path)
                } else {
                    PrefabRequest::Unload(path)
                };
                with_active_call(&active, |transaction| {
                    transaction
                        .prefab_request(request)
                        .map_err(LuaError::runtime)
                })
            })?,
        )?;
    }
    prefabs.set(
        "state",
        lua.create_function(move |_, path: String| {
            with_active_call(&active_call, |transaction| {
                Ok(transaction
                    .prefab_status(&path)
                    .map(|status| status.as_str().to_owned()))
            })
        })?,
    )?;
    Ok(prefabs)
}

fn require_audio_source(
    transaction: &ScriptTransaction,
    handle: ObjectHandle,
    name: &str,
) -> mlua::Result<()> {
    if transaction.has_audio_source(handle) {
        Ok(())
    } else {
        Err(LuaError::runtime(format!(
            "{name} requires an audio_source component"
        )))
    }
}

/// Громкость из Lua: конечное число не меньше нуля.
fn audio_volume(value: &Value, name: &str) -> mlua::Result<f32> {
    let volume = match value {
        Value::Integer(value) => *value as f64,
        Value::Number(value) => *value,
        _ => {
            return Err(LuaError::runtime(format!(
                "{name} expects a numeric volume"
            )));
        }
    };
    let converted = volume as f32;
    let max = f64::from(engine_scene::MAX_AUDIO_VOLUME);
    if !volume.is_finite() || !converted.is_finite() || !(0.0..=max).contains(&volume) {
        return Err(LuaError::runtime(format!(
            "{name} volume must be a finite number from 0 to {max}"
        )));
    }
    Ok(converted)
}

fn audio_group(name: &str, allow_master: bool) -> mlua::Result<AudioGroup> {
    match name {
        "master" if allow_master => Ok(AudioGroup::Master),
        "music" => Ok(AudioGroup::Music),
        "effects" => Ok(AudioGroup::Effects),
        "ambient" => Ok(AudioGroup::Ambient),
        other => Err(LuaError::runtime(format!(
            "unknown audio group {other:?}: expected {}",
            if allow_master {
                "master, music, effects or ambient"
            } else {
                "music, effects or ambient"
            }
        ))),
    }
}

/// `rve.audio`: клипы без объекта, остановка по id и громкость групп.
fn audio_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let audio = lua.create_table()?;
    let active_for_position = active_call.clone();

    let play_active = active_call.clone();
    audio.set(
        "play",
        lua.create_function(move |lua, (path, options): (String, Option<Table>)| {
            let path = VirtualPath::parse(&path)
                .map_err(|error| LuaError::runtime(format!("rve.audio.play: {error}")))?;
            let mut params = ClipParams::default();
            if let Some(options) = options {
                component_api::read_clip_spatial_options(lua, &options, &mut params)?;
                if let Some(group) = options.get::<Option<String>>("group")? {
                    params.group = audio_group(&group, false)?;
                }
                if let Some(volume) = options.get::<Option<Value>>("volume")? {
                    params.volume = audio_volume(&volume, "rve.audio.play")?;
                }
                if let Some(looping) = options.get::<Option<bool>>("looping")? {
                    params.looping = looping;
                }
                if let Some(position) = options.get::<Option<Table>>("position")? {
                    params.position = Some(read_vector(&position, "rve.audio.play position")?);
                }
            }
            with_active_call(&play_active, |transaction| {
                let voice = VoiceId::allocate();
                transaction
                    .stage_audio(AudioRequest::PlayClip {
                        voice,
                        path: path.clone(),
                        params,
                    })
                    .map_err(LuaError::runtime)?;
                Ok(voice.get() as i64)
            })
        })?,
    )?;

    let stop_active = active_call.clone();
    audio.set(
        "stop",
        lua.create_function(move |_, id: i64| {
            let voice = u64::try_from(id)
                .ok()
                .and_then(VoiceId::from_raw)
                .ok_or_else(|| LuaError::runtime("rve.audio.stop expects a voice id"))?;
            with_active_call(&stop_active, |transaction| {
                transaction
                    .stage_audio(AudioRequest::StopVoice { voice })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    audio.set(
        "set_group_volume",
        lua.create_function(move |_, (group, volume): (String, Value)| {
            let group = audio_group(&group, true)?;
            let volume = audio_volume(&volume, "rve.audio.set_group_volume")?;
            with_active_call(&active_call, |transaction| {
                transaction
                    .stage_audio(AudioRequest::SetGroupVolume { group, volume })
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    audio.set(
        "set_voice_position",
        component_api::audio_set_voice_position(lua, &active_for_position)?,
    )?;
    Ok(audio)
}

fn add_physics_object_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    physics_api::add_component_methods(lua, object, handle, active_call.clone())?;
    let get_active = active_call.clone();
    object.set(
        "get_velocity",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_active, handle)? {
                return Ok(Value::Nil);
            }
            let velocity = with_active_call(&get_active, |transaction| {
                transaction.body_velocity(handle).map_err(LuaError::runtime)
            })?;
            Ok(Value::Table(vector_table(lua, velocity.to_array())?))
        })?,
    )?;

    for (name, setter) in [
        ("set_velocity", true),
        ("add_impulse", false),
        ("add_force", false),
    ] {
        let set_active = active_call.clone();
        object.set(
            name,
            lua.create_function(move |_, arguments: MultiValue| {
                let vector = method_argument(arguments, name)?;
                let Value::Table(vector) = vector else {
                    return Err(LuaError::runtime(format!(
                        "object.{name} expects a vector table"
                    )));
                };
                let vector = Vec3::from_array(read_vector(&vector, name)?);
                if !object_alive(&set_active, handle)? {
                    return Ok(false);
                }
                with_active_call(&set_active, |transaction| {
                    let result = if setter {
                        transaction.set_body_velocity(handle, vector)
                    } else if name == "add_impulse" {
                        transaction.add_body_impulse(handle, vector)
                    } else {
                        transaction.add_body_force(handle, vector)
                    };
                    result.map_err(LuaError::runtime)?;
                    Ok(true)
                })
            })?,
        )?;
    }

    let move_active = active_call.clone();
    object.set(
        "move",
        lua.create_function(move |lua, arguments: MultiValue| {
            let displacement = method_argument(arguments, "object.move")?;
            let Value::Table(displacement) = displacement else {
                return Err(LuaError::runtime(
                    "object.move expects a displacement vector",
                ));
            };
            let displacement = Vec3::from_array(read_vector(&displacement, "displacement")?);
            if !object_alive(&move_active, handle)? {
                return Ok(Value::Nil);
            }
            let result = with_active_call(&move_active, |transaction| {
                transaction
                    .move_character(handle, displacement)
                    .map_err(LuaError::runtime)
            })?;
            let result_table = lua.create_table()?;
            result_table.set("grounded", result.grounded)?;
            result_table.set("collided", !result.hits.is_empty())?;
            Ok(Value::Table(result_table))
        })?,
    )?;

    let grounded_active = active_call.clone();
    object.set(
        "is_grounded",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&grounded_active, handle)? {
                return Ok(false);
            }
            with_active_call(&grounded_active, |transaction| {
                transaction.is_grounded(handle).map_err(LuaError::runtime)
            })
        })?,
    )?;

    let enabled_active = active_call.clone();
    object.set(
        "set_collider_enabled",
        lua.create_function(move |_, arguments: MultiValue| {
            let enabled = method_argument(arguments, "object.set_collider_enabled")?;
            let Value::Boolean(enabled) = enabled else {
                return Err(LuaError::runtime(
                    "object.set_collider_enabled expects a boolean",
                ));
            };
            if !object_alive(&enabled_active, handle)? {
                return Ok(false);
            }
            with_active_call(&enabled_active, |transaction| {
                transaction
                    .set_collider_enabled(handle, enabled)
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;

    let is_enabled_active = active_call;
    object.set(
        "is_collider_enabled",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&is_enabled_active, handle)? {
                return Ok(Value::Nil);
            }
            let enabled = with_active_call(&is_enabled_active, |transaction| {
                transaction
                    .collider_enabled(handle)
                    .map_err(LuaError::runtime)
            })?;
            Ok(Value::Boolean(enabled))
        })?,
    )?;
    Ok(())
}

/// Аргументы метода без необязательного `self`: первая таблица отбрасывается, только если это
/// сам объект. Так работают и `obj:set_parent(p)`, и `obj.set_parent(p)` с другим объектом первым.
fn method_arguments(
    lua: &Lua,
    arguments: MultiValue,
    handle: ObjectHandle,
) -> mlua::Result<Vec<Value>> {
    let mut values = arguments.into_iter().collect::<Vec<_>>();
    if let Some(Value::Table(table)) = values.first()
        && object_handle_of(lua, table)? == Some(handle)
    {
        values.remove(0);
    }
    Ok(values)
}

/// Таблица живого объекта по хендлу; `None`, если объект уже мёртв.
fn live_object_table(
    lua: &Lua,
    active_call: &ActiveCall,
    handle: ObjectHandle,
) -> mlua::Result<Option<Table>> {
    let identity = with_active_call(active_call, |transaction| {
        Ok(transaction.object_identity(handle))
    })?;
    let Some((id, name)) = identity else {
        return Ok(None);
    };
    Ok(Some(object_table(
        lua,
        handle,
        id,
        name,
        active_call.clone(),
    )?))
}

/// Список объектов из хендлов в порядке, который вернул движок.
fn object_list(
    lua: &Lua,
    active_call: &ActiveCall,
    handles: Vec<ObjectHandle>,
) -> mlua::Result<Table> {
    let list = lua.create_table()?;
    for handle in handles {
        if let Some(table) = live_object_table(lua, active_call, handle)? {
            list.push(table)?;
        }
    }
    Ok(list)
}

/// Иерархия, активность, теги, слои и компоненты объекта. Чтения видят заявки текущего вызова,
/// где движок их поддерживает; мировые позиции и поиск читают последний коммит мира.
fn add_hierarchy_object_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    let parent_call = active_call.clone();
    object.set(
        "parent",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let parent = with_active_call(&parent_call, |transaction| {
                Ok(transaction.parent(handle).flatten())
            })?;
            match parent {
                Some(parent) => match live_object_table(lua, &parent_call, parent)? {
                    Some(table) => Ok(Value::Table(table)),
                    None => Ok(Value::Nil),
                },
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let children_call = active_call.clone();
    object.set(
        "children",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let children = with_active_call(&children_call, |transaction| {
                Ok(transaction.children(handle).unwrap_or_default())
            })?;
            object_list(lua, &children_call, children)
        })?,
    )?;

    let set_parent_call = active_call.clone();
    object.set(
        "set_parent",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let parent = match values.first() {
                None | Some(Value::Nil) => None,
                Some(Value::Table(table)) => {
                    Some(object_handle_of(lua, table)?.ok_or_else(|| {
                        LuaError::runtime("object.set_parent expects an object or nil")
                    })?)
                }
                Some(_) => {
                    return Err(LuaError::runtime(
                        "object.set_parent expects an object or nil",
                    ));
                }
            };
            let keep_world = match values.get(1) {
                None => true,
                Some(Value::Boolean(keep_world)) => *keep_world,
                Some(_) => {
                    return Err(LuaError::runtime(
                        "object.set_parent expects keep_world to be a boolean",
                    ));
                }
            };
            with_active_call(&set_parent_call, |transaction| {
                let parent_alive = parent
                    .is_none_or(|parent| transaction.object_state(parent) == ObjectState::Alive);
                let staged = parent_alive
                    && transaction.stage_edit(ObjectEdit::SetParent {
                        object: handle,
                        parent,
                        keep_world,
                    });
                Ok(staged)
            })
        })?,
    )?;

    let active_query_call = active_call.clone();
    object.set(
        "is_active",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&active_query_call, |transaction| {
                Ok(transaction.is_active(handle).unwrap_or(false))
            })
        })?,
    )?;

    let hierarchy_call = active_call.clone();
    object.set(
        "active_in_hierarchy",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&hierarchy_call, |transaction| {
                Ok(transaction.active_in_hierarchy(handle).unwrap_or(false))
            })
        })?,
    )?;

    let keep_on_load_call = active_call.clone();
    object.set(
        "keep_on_load",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&keep_on_load_call, |transaction| {
                Ok(transaction.keep_on_load(handle).unwrap_or(false))
            })
        })?,
    )?;

    let set_keep_on_load_call = active_call.clone();
    object.set(
        "set_keep_on_load",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let keep = lua_boolean(
                values.into_iter().next().unwrap_or(Value::Nil),
                "object.set_keep_on_load",
            )?;
            with_active_call(&set_keep_on_load_call, |transaction| {
                Ok(transaction.set_keep_on_load(handle, keep))
            })
        })?,
    )?;

    let set_active_call = active_call.clone();
    object.set(
        "set_active",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let active = lua_boolean(
                values.into_iter().next().unwrap_or(Value::Nil),
                "object.set_active",
            )?;
            with_active_call(&set_active_call, |transaction| {
                Ok(transaction.stage_edit(ObjectEdit::SetActive {
                    object: handle,
                    active,
                }))
            })
        })?,
    )?;

    let position_call = active_call.clone();
    object.set(
        "get_world_position",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let position = with_active_call(&position_call, |transaction| {
                Ok(transaction.world_position(handle))
            })?;
            match position {
                Some(position) => Ok(Value::Table(vector_table(lua, position.to_array())?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_position_call = active_call.clone();
    object.set(
        "set_world_position",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let Some(Value::Table(table)) = values.into_iter().next() else {
                return Err(LuaError::runtime(
                    "object.set_world_position expects a vector table",
                ));
            };
            let position = read_vector(&table, "set_world_position")?;
            with_active_call(&set_position_call, |transaction| {
                Ok(transaction.stage_edit(ObjectEdit::SetWorldPosition {
                    object: handle,
                    position: Vec3::from_array(position),
                }))
            })
        })?,
    )?;

    let rotation_call = active_call.clone();
    object.set(
        "get_world_rotation",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let rotation = with_active_call(&rotation_call, |transaction| {
                Ok(transaction.world_rotation_deg(handle))
            })?;
            match rotation {
                Some(rotation) => Ok(Value::Table(vector_table(lua, rotation.to_array())?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_rotation_call = active_call.clone();
    object.set(
        "set_world_rotation",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let Some(Value::Table(table)) = values.into_iter().next() else {
                return Err(LuaError::runtime(
                    "object.set_world_rotation expects a vector table in degrees",
                ));
            };
            let rotation = read_vector(&table, "set_world_rotation")?;
            with_active_call(&set_rotation_call, |transaction| {
                Ok(transaction.stage_edit(ObjectEdit::SetWorldRotation {
                    object: handle,
                    rotation_deg: Vec3::from_array(rotation),
                }))
            })
        })?,
    )?;

    let tag_call = active_call.clone();
    object.set(
        "tag",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&tag_call, |transaction| {
                Ok(transaction.tag(handle).map(str::to_owned))
            })
        })?,
    )?;

    let set_tag_call = active_call.clone();
    object.set(
        "set_tag",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let tag = match values.into_iter().next() {
                Some(Value::String(tag)) => tag.to_str()?.to_owned(),
                _ => return Err(LuaError::runtime("object.set_tag expects a string")),
            };
            with_active_call(&set_tag_call, |transaction| {
                Ok(transaction.stage_edit(ObjectEdit::SetTag {
                    object: handle,
                    tag,
                }))
            })
        })?,
    )?;

    let layer_call = active_call.clone();
    object.set(
        "layer",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(&layer_call, |transaction| {
                Ok(transaction.layer(handle).map(i64::from))
            })
        })?,
    )?;

    let set_layer_call = active_call.clone();
    object.set(
        "set_layer",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let layer = lua_integer(
                values.into_iter().next().unwrap_or(Value::Nil),
                "object.set_layer",
            )?;
            let Ok(layer) = u8::try_from(layer) else {
                return Ok(false);
            };
            with_active_call(&set_layer_call, |transaction| {
                Ok(transaction.stage_edit(ObjectEdit::SetLayer {
                    object: handle,
                    layer,
                }))
            })
        })?,
    )?;

    let component_call = active_call.clone();
    object.set(
        "has_component",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let name = match values.into_iter().next() {
                Some(Value::String(name)) => name.to_str()?.to_owned(),
                _ => return Err(LuaError::runtime("object.has_component expects a name")),
            };
            has_component_by_name(&component_call, handle, &name)
        })?,
    )?;

    let components_call = active_call.clone();
    object.set(
        "components",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let list = lua.create_table()?;
            for name in rve_sdk::abi::RVE_COMPONENT_NAMES {
                if has_component_by_name(&components_call, handle, name)? {
                    list.push(name)?;
                }
            }
            Ok(list)
        })?,
    )?;
    Ok(())
}

/// Проверяет компонент по имени из `RVE_COMPONENT_NAMES` или `script:<путь>`; неизвестное имя — `false`.
fn has_component_by_name(
    active_call: &ActiveCall,
    handle: ObjectHandle,
    name: &str,
) -> mlua::Result<bool> {
    let Some(kind) = ComponentKind::parse(name) else {
        return Ok(false);
    };
    with_active_call(active_call, |transaction| {
        Ok(transaction.has_component(handle, &kind).unwrap_or(false))
    })
}

/// Поиск объектов по имени и тегу: `rve.find_by_name`, `rve.find_all`, `rve.find_with_tag`, `rve.find_all_with_tag`.
fn object_finders(
    lua: &Lua,
    active_call: ActiveCall,
) -> mlua::Result<Vec<(&'static str, Function)>> {
    let mut finders = Vec::new();
    let by_name_call = active_call.clone();
    finders.push((
        "find_by_name",
        lua.create_function(move |lua, name: String| {
            let found = with_active_call(&by_name_call, |transaction| {
                Ok(transaction.find_by_name(&name))
            })?;
            match found {
                Some(handle) => match live_object_table(lua, &by_name_call, handle)? {
                    Some(table) => Ok(Value::Table(table)),
                    None => Ok(Value::Nil),
                },
                None => Ok(Value::Nil),
            }
        })?,
    ));
    let all_name_call = active_call.clone();
    finders.push((
        "find_all",
        lua.create_function(move |lua, name: String| {
            let found = with_active_call(&all_name_call, |transaction| {
                Ok(transaction.find_all_by_name(&name))
            })?;
            object_list(lua, &all_name_call, found)
        })?,
    ));
    let tag_call = active_call.clone();
    finders.push((
        "find_with_tag",
        lua.create_function(move |lua, tag: String| {
            let found =
                with_active_call(&tag_call, |transaction| Ok(transaction.find_with_tag(&tag)))?;
            match found {
                Some(handle) => match live_object_table(lua, &tag_call, handle)? {
                    Some(table) => Ok(Value::Table(table)),
                    None => Ok(Value::Nil),
                },
                None => Ok(Value::Nil),
            }
        })?,
    ));
    let all_tag_call = active_call;
    finders.push((
        "find_all_with_tag",
        lua.create_function(move |lua, tag: String| {
            let found = with_active_call(&all_tag_call, |transaction| {
                Ok(transaction.find_all_with_tag(&tag))
            })?;
            object_list(lua, &all_tag_call, found)
        })?,
    ));
    Ok(finders)
}

/// Таблица `self.script` экземпляра: `set_enabled(enabled)` и `enabled()`. Записи коммитятся
/// вместе с вызовом; сам экземпляр не может менять чужой флаг.
fn script_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let script = lua.create_table()?;
    let set_call = active_call.clone();
    script.set(
        "set_enabled",
        lua.create_function(move |_, arguments: MultiValue| {
            let mut values = arguments.into_iter();
            let first = values.next();
            let value = match first {
                Some(Value::Table(_)) => values.next(),
                value => value,
            };
            let enabled = lua_boolean(value.unwrap_or(Value::Nil), "script.set_enabled")?;
            with_active_call(&set_call, |transaction| {
                Ok(transaction.set_script_enabled(enabled))
            })
        })?,
    )?;
    let enabled_call = active_call;
    script.set(
        "enabled",
        lua.create_function(move |_, _arguments: MultiValue| {
            with_active_call(
                &enabled_call,
                |transaction| Ok(transaction.script_enabled()),
            )
        })?,
    )?;
    Ok(script)
}

fn method_argument(mut arguments: MultiValue, name: &str) -> mlua::Result<Value> {
    let Some(first) = arguments.pop_front() else {
        return Err(LuaError::runtime(format!("{name} is missing its argument")));
    };
    let value = if matches!(first, Value::Table(_)) {
        arguments.pop_front().unwrap_or(first)
    } else {
        first
    };
    if arguments.pop_front().is_some() {
        return Err(LuaError::runtime(format!("{name} expects one argument")));
    }
    Ok(value)
}

fn physics_raycast_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(
        move |lua,
              (origin, direction, max_distance, options): (
                  Table,
                  Table,
                  f64,
                  Option<Table>,
              )| {
            let origin = Vec3::from_array(read_vector(&origin, "origin")?);
            let direction = Vec3::from_array(read_vector(&direction, "direction")?);
            let max_distance = max_distance as f32;
            if !max_distance.is_finite() || max_distance < 0.0 {
                return Err(LuaError::runtime("raycast max_distance must be finite and nonnegative"));
            }
            let (include_triggers, ignore) = if let Some(options) = options {
                let include_triggers = match options.get::<Value>("include_triggers")? {
                    Value::Nil => false,
                    Value::Boolean(value) => value,
                    _ => return Err(LuaError::runtime("opts.include_triggers must be a boolean")),
                };
                let ignore = match options.get::<Value>("ignore")? {
                    Value::Nil => None,
                    Value::Table(object) => {
                        let id = object.get::<String>("id").map_err(|_| {
                            LuaError::runtime("opts.ignore must be an object returned by rve.find")
                        })?;
                        Some(id)
                    }
                    _ => return Err(LuaError::runtime("opts.ignore must be an object")),
                };
                (include_triggers, ignore)
            } else {
                (false, None)
            };
            let ray = Ray::new(origin, direction, max_distance)
                .map_err(|error| LuaError::runtime(error.to_string()))?;
            let hit = with_active_call(&active_call, |transaction| {
                let ignore = ignore
                    .as_deref()
                    .and_then(|id| transaction.handle_by_id(id));
                transaction
                    .physics_raycast(ray, include_triggers, ignore)
                    .map_err(LuaError::runtime)
            })?;
            let Some(hit) = hit else {
                return Ok(Option::<Table>::None);
            };
            let result = lua.create_table()?;
            let object = with_active_call(&active_call, |transaction| {
                let Some(handle) = transaction.physics_object(hit.body) else {
                    return Ok(None);
                };
                let Some((id, name)) = transaction.object_identity(handle) else {
                    return Ok(None);
                };
                object_table(lua, handle, id, name, active_call.clone()).map(Some)
            })?;
            let Some(object) = object else {
                return Ok(Option::<Table>::None);
            };
            result.set("object", object)?;
            result.set("point", vector_table(lua, hit.point.to_array())?)?;
            result.set("normal", vector_table(lua, hit.normal.to_array())?)?;
            result.set("distance", hit.distance as f64)?;
            Ok(Some(result))
        },
    )
}

/// Таблица постобработки для `object:get_post_process()`; секции вложены по эффектам.
fn post_process_table(lua: &Lua, settings: PostProcessSettings) -> mlua::Result<Table> {
    let result = lua.create_table()?;

    let exposure = lua.create_table()?;
    exposure.set("enabled", settings.exposure.enabled)?;
    exposure.set(
        "mode",
        match settings.exposure.mode {
            ExposureMode::Manual => "manual",
            ExposureMode::Auto => "auto",
        },
    )?;
    exposure.set("ev", settings.exposure.ev as f64)?;
    exposure.set("min_ev", settings.exposure.min_ev as f64)?;
    exposure.set("max_ev", settings.exposure.max_ev as f64)?;
    exposure.set("speed_up", settings.exposure.speed_up as f64)?;
    exposure.set("speed_down", settings.exposure.speed_down as f64)?;
    exposure.set("low_percent", settings.exposure.low_percent as f64)?;
    exposure.set("high_percent", settings.exposure.high_percent as f64)?;
    result.set("exposure", exposure)?;

    let tonemapping = lua.create_table()?;
    tonemapping.set("enabled", settings.tonemapping.enabled)?;
    tonemapping.set(
        "tonemapper",
        tonemapper_name(settings.tonemapping.tonemapper),
    )?;
    result.set("tonemapping", tonemapping)?;

    let bloom = lua.create_table()?;
    bloom.set("enabled", settings.bloom.enabled)?;
    bloom.set("intensity", settings.bloom.intensity as f64)?;
    bloom.set("threshold", settings.bloom.threshold as f64)?;
    bloom.set("knee", settings.bloom.knee as f64)?;
    bloom.set("scatter", settings.bloom.scatter as f64)?;
    bloom.set("tint", number_array3_table(lua, settings.bloom.tint)?)?;
    result.set("bloom", bloom)?;

    let ao = lua.create_table()?;
    ao.set("enabled", settings.ambient_occlusion.enabled)?;
    ao.set("radius", settings.ambient_occlusion.radius as f64)?;
    ao.set("intensity", settings.ambient_occlusion.intensity as f64)?;
    ao.set("falloff", settings.ambient_occlusion.falloff as f64)?;
    ao.set(
        "quality",
        match settings.ambient_occlusion.quality {
            AmbientOcclusionQuality::Low => "low",
            AmbientOcclusionQuality::Medium => "medium",
            AmbientOcclusionQuality::High => "high",
        },
    )?;
    result.set("ambient_occlusion", ao)?;

    let grading = lua.create_table()?;
    grading.set("enabled", settings.color_grading.enabled)?;
    grading.set("temperature", settings.color_grading.temperature as f64)?;
    grading.set("tint", settings.color_grading.tint as f64)?;
    grading.set("contrast", settings.color_grading.contrast as f64)?;
    grading.set("saturation", settings.color_grading.saturation as f64)?;
    grading.set(
        "lift",
        number_array3_table(lua, settings.color_grading.lift)?,
    )?;
    grading.set(
        "gamma",
        number_array3_table(lua, settings.color_grading.gamma)?,
    )?;
    grading.set(
        "gain",
        number_array3_table(lua, settings.color_grading.gain)?,
    )?;
    result.set("color_grading", grading)?;

    let vignette = lua.create_table()?;
    vignette.set("enabled", settings.vignette.enabled)?;
    vignette.set("intensity", settings.vignette.intensity as f64)?;
    vignette.set("smoothness", settings.vignette.smoothness as f64)?;
    vignette.set("roundness", settings.vignette.roundness as f64)?;
    vignette.set("color", number_array3_table(lua, settings.vignette.color)?)?;
    result.set("vignette", vignette)?;

    let aberration = lua.create_table()?;
    aberration.set("enabled", settings.chromatic_aberration.enabled)?;
    aberration.set("intensity", settings.chromatic_aberration.intensity as f64)?;
    result.set("chromatic_aberration", aberration)?;

    let grain = lua.create_table()?;
    grain.set("enabled", settings.film_grain.enabled)?;
    grain.set("intensity", settings.film_grain.intensity as f64)?;
    grain.set("response", settings.film_grain.response as f64)?;
    result.set("film_grain", grain)?;

    let dof = lua.create_table()?;
    dof.set("enabled", settings.depth_of_field.enabled)?;
    dof.set(
        "focus_distance",
        settings.depth_of_field.focus_distance as f64,
    )?;
    dof.set("aperture", settings.depth_of_field.aperture as f64)?;
    dof.set(
        "focal_length_mm",
        settings.depth_of_field.focal_length_mm as f64,
    )?;
    dof.set("max_blur_px", settings.depth_of_field.max_blur_px as f64)?;
    result.set("depth_of_field", dof)?;

    let motion = lua.create_table()?;
    motion.set("enabled", settings.motion_blur.enabled)?;
    motion.set(
        "shutter_angle_deg",
        settings.motion_blur.shutter_angle_deg as f64,
    )?;
    motion.set("samples", settings.motion_blur.samples)?;
    motion.set("max_blur_px", settings.motion_blur.max_blur_px as f64)?;
    result.set("motion_blur", motion)?;
    Ok(result)
}

fn tonemapper_name(tonemapper: Tonemapper) -> &'static str {
    match tonemapper {
        Tonemapper::Aces => "aces",
        Tonemapper::Neutral => "neutral",
        Tonemapper::None => "none",
    }
}

fn number_array3_table(lua: &Lua, values: [f32; 3]) -> mlua::Result<Table> {
    let table = lua.create_table_with_capacity(3, 0)?;
    for (index, value) in values.into_iter().enumerate() {
        table.raw_set(index + 1, value as f64)?;
    }
    Ok(table)
}

/// Применяет частичную таблицу постобработки. Неизвестные поля и секции, а также неверные
/// типы дают ошибку до записи; диапазоны проверяет `validate` при установке.
fn apply_post_process_table(table: &Table, settings: &mut PostProcessSettings) -> mlua::Result<()> {
    ensure_known_fields(
        table,
        &[
            "exposure",
            "tonemapping",
            "bloom",
            "ambient_occlusion",
            "color_grading",
            "vignette",
            "chromatic_aberration",
            "film_grain",
            "depth_of_field",
            "motion_blur",
        ],
        "post process",
    )?;
    if let Some(section) = post_process_section(
        table,
        "exposure",
        &[
            "enabled",
            "mode",
            "ev",
            "min_ev",
            "max_ev",
            "speed_up",
            "speed_down",
            "low_percent",
            "high_percent",
        ],
    )? {
        optional_bool(&section, "enabled", &mut settings.exposure.enabled)?;
        if let Some(mode) = optional_text(&section, "mode")? {
            settings.exposure.mode = match mode.as_str() {
                "manual" => ExposureMode::Manual,
                "auto" => ExposureMode::Auto,
                _ => {
                    return Err(LuaError::runtime(
                        "exposure.mode must be \"manual\" or \"auto\"",
                    ));
                }
            };
        }
        set_optional_number(&section, "ev", &mut settings.exposure.ev)?;
        set_optional_number(&section, "min_ev", &mut settings.exposure.min_ev)?;
        set_optional_number(&section, "max_ev", &mut settings.exposure.max_ev)?;
        set_optional_number(&section, "speed_up", &mut settings.exposure.speed_up)?;
        set_optional_number(&section, "speed_down", &mut settings.exposure.speed_down)?;
        set_optional_number(&section, "low_percent", &mut settings.exposure.low_percent)?;
        set_optional_number(
            &section,
            "high_percent",
            &mut settings.exposure.high_percent,
        )?;
    }
    if let Some(section) = post_process_section(table, "tonemapping", &["enabled", "tonemapper"])? {
        optional_bool(&section, "enabled", &mut settings.tonemapping.enabled)?;
        if let Some(name) = optional_text(&section, "tonemapper")? {
            settings.tonemapping.tonemapper = match name.as_str() {
                "aces" => Tonemapper::Aces,
                "neutral" => Tonemapper::Neutral,
                "none" => Tonemapper::None,
                _ => {
                    return Err(LuaError::runtime(
                        "tonemapping.tonemapper must be \"aces\", \"neutral\", or \"none\"",
                    ));
                }
            };
        }
    }
    if let Some(section) = post_process_section(
        table,
        "bloom",
        &[
            "enabled",
            "intensity",
            "threshold",
            "knee",
            "scatter",
            "tint",
        ],
    )? {
        optional_bool(&section, "enabled", &mut settings.bloom.enabled)?;
        set_optional_number(&section, "intensity", &mut settings.bloom.intensity)?;
        set_optional_number(&section, "threshold", &mut settings.bloom.threshold)?;
        set_optional_number(&section, "knee", &mut settings.bloom.knee)?;
        set_optional_number(&section, "scatter", &mut settings.bloom.scatter)?;
        optional_array3(&section, "tint", &mut settings.bloom.tint)?;
    }
    if let Some(section) = post_process_section(
        table,
        "ambient_occlusion",
        &["enabled", "radius", "intensity", "falloff", "quality"],
    )? {
        optional_bool(&section, "enabled", &mut settings.ambient_occlusion.enabled)?;
        set_optional_number(&section, "radius", &mut settings.ambient_occlusion.radius)?;
        set_optional_number(
            &section,
            "intensity",
            &mut settings.ambient_occlusion.intensity,
        )?;
        set_optional_number(&section, "falloff", &mut settings.ambient_occlusion.falloff)?;
        if let Some(quality) = optional_text(&section, "quality")? {
            settings.ambient_occlusion.quality = match quality.as_str() {
                "low" => AmbientOcclusionQuality::Low,
                "medium" => AmbientOcclusionQuality::Medium,
                "high" => AmbientOcclusionQuality::High,
                _ => {
                    return Err(LuaError::runtime(
                        "ambient_occlusion.quality must be \"low\", \"medium\", or \"high\"",
                    ));
                }
            };
        }
    }
    if let Some(section) = post_process_section(
        table,
        "color_grading",
        &[
            "enabled",
            "temperature",
            "tint",
            "contrast",
            "saturation",
            "lift",
            "gamma",
            "gain",
        ],
    )? {
        optional_bool(&section, "enabled", &mut settings.color_grading.enabled)?;
        set_optional_number(
            &section,
            "temperature",
            &mut settings.color_grading.temperature,
        )?;
        set_optional_number(&section, "tint", &mut settings.color_grading.tint)?;
        set_optional_number(&section, "contrast", &mut settings.color_grading.contrast)?;
        set_optional_number(
            &section,
            "saturation",
            &mut settings.color_grading.saturation,
        )?;
        optional_array3(&section, "lift", &mut settings.color_grading.lift)?;
        optional_array3(&section, "gamma", &mut settings.color_grading.gamma)?;
        optional_array3(&section, "gain", &mut settings.color_grading.gain)?;
    }
    if let Some(section) = post_process_section(
        table,
        "vignette",
        &["enabled", "intensity", "smoothness", "roundness", "color"],
    )? {
        optional_bool(&section, "enabled", &mut settings.vignette.enabled)?;
        set_optional_number(&section, "intensity", &mut settings.vignette.intensity)?;
        set_optional_number(&section, "smoothness", &mut settings.vignette.smoothness)?;
        set_optional_number(&section, "roundness", &mut settings.vignette.roundness)?;
        optional_array3(&section, "color", &mut settings.vignette.color)?;
    }
    if let Some(section) =
        post_process_section(table, "chromatic_aberration", &["enabled", "intensity"])?
    {
        optional_bool(
            &section,
            "enabled",
            &mut settings.chromatic_aberration.enabled,
        )?;
        set_optional_number(
            &section,
            "intensity",
            &mut settings.chromatic_aberration.intensity,
        )?;
    }
    if let Some(section) =
        post_process_section(table, "film_grain", &["enabled", "intensity", "response"])?
    {
        optional_bool(&section, "enabled", &mut settings.film_grain.enabled)?;
        set_optional_number(&section, "intensity", &mut settings.film_grain.intensity)?;
        set_optional_number(&section, "response", &mut settings.film_grain.response)?;
    }
    if let Some(section) = post_process_section(
        table,
        "depth_of_field",
        &[
            "enabled",
            "focus_distance",
            "aperture",
            "focal_length_mm",
            "max_blur_px",
        ],
    )? {
        optional_bool(&section, "enabled", &mut settings.depth_of_field.enabled)?;
        set_optional_number(
            &section,
            "focus_distance",
            &mut settings.depth_of_field.focus_distance,
        )?;
        set_optional_number(&section, "aperture", &mut settings.depth_of_field.aperture)?;
        set_optional_number(
            &section,
            "focal_length_mm",
            &mut settings.depth_of_field.focal_length_mm,
        )?;
        set_optional_number(
            &section,
            "max_blur_px",
            &mut settings.depth_of_field.max_blur_px,
        )?;
    }
    if let Some(section) = post_process_section(
        table,
        "motion_blur",
        &["enabled", "shutter_angle_deg", "samples", "max_blur_px"],
    )? {
        optional_bool(&section, "enabled", &mut settings.motion_blur.enabled)?;
        set_optional_number(
            &section,
            "shutter_angle_deg",
            &mut settings.motion_blur.shutter_angle_deg,
        )?;
        optional_samples(&section, "samples", &mut settings.motion_blur.samples)?;
        set_optional_number(
            &section,
            "max_blur_px",
            &mut settings.motion_blur.max_blur_px,
        )?;
    }
    Ok(())
}

/// Возвращает вложенную секцию с проверкой полей; `nil` означает «не менять».
fn post_process_section(
    table: &Table,
    name: &str,
    allowed: &[&str],
) -> mlua::Result<Option<Table>> {
    match table.get::<Value>(name)? {
        Value::Nil => Ok(None),
        Value::Table(section) => {
            ensure_known_fields(&section, allowed, name)?;
            Ok(Some(section))
        }
        _ => Err(LuaError::runtime(format!(
            "post process.{name} must be a table"
        ))),
    }
}

/// Аргументы `obj:screen_shader(i)` или `obj:set_screen_shader(i, t)`: слот (1-based в Lua) и,
/// для записи, таблица. Ведущий объект-таблица пропускается, если за ним идёт номер слота.
fn screen_call_arguments(
    arguments: MultiValue,
    name: &str,
) -> mlua::Result<(usize, Option<Table>)> {
    let mut values: Vec<Value> = arguments.into_iter().collect();
    if matches!(values.first(), Some(Value::Table(_))) && values.len() > 1 {
        values.remove(0);
    }
    let slot = match values.first() {
        Some(Value::Integer(index)) if *index >= 1 => (*index - 1) as usize,
        Some(Value::Number(index)) if index.fract() == 0.0 && *index >= 1.0 => {
            (*index as usize) - 1
        }
        _ => {
            return Err(LuaError::runtime(format!(
                "{name} expects a 1-based screen shader index"
            )));
        }
    };
    let table = match values.get(1) {
        Some(Value::Table(table)) => Some(table.clone()),
        _ => None,
    };
    Ok((slot, table))
}

/// Таблица состояния слота: `enabled` (boolean) и `params` (массив из шестнадцати чисел).
fn screen_shader_table(lua: &Lua, state: &ScreenShaderState) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set("enabled", state.enabled)?;
    let params = lua.create_table()?;
    for (index, value) in state.params.iter().enumerate() {
        params.set(index + 1, f64::from(*value))?;
    }
    result.set("params", params)?;
    Ok(result)
}

/// Читает `enabled` и `params` из таблицы; отсутствующие поля сохраняют текущие значения.
/// `params` заменяет все шестнадцать значений, недостающие дополняются нулями.
fn apply_screen_shader_table(table: &Table, state: &mut ScreenShaderState) -> mlua::Result<()> {
    ensure_known_fields(table, &["enabled", "params"], "screen shader")?;
    optional_bool(table, "enabled", &mut state.enabled)?;
    match table.get::<Value>("params")? {
        Value::Nil => {}
        Value::Table(list) => {
            if list.raw_len() > MAX_SCREEN_SHADER_PARAMS {
                return Err(LuaError::runtime(format!(
                    "screen shader params accept at most {MAX_SCREEN_SHADER_PARAMS} numbers"
                )));
            }
            let mut params = [0.0_f32; MAX_SCREEN_SHADER_PARAMS];
            for (index, target) in params.iter_mut().enumerate() {
                *target = match list.get::<Value>(index + 1)? {
                    Value::Nil => 0.0,
                    Value::Integer(value) => value as f32,
                    Value::Number(value) if value.is_finite() => value as f32,
                    _ => {
                        return Err(LuaError::runtime(
                            "screen shader params must be finite numbers",
                        ));
                    }
                };
            }
            state.params = params;
        }
        _ => return Err(LuaError::runtime("params must be an array of numbers")),
    }
    Ok(())
}

fn optional_bool(table: &Table, key: &str, destination: &mut bool) -> mlua::Result<()> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(()),
        Value::Boolean(value) => {
            *destination = value;
            Ok(())
        }
        _ => Err(LuaError::runtime(format!("{key} must be a boolean"))),
    }
}

fn optional_text(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(value.to_str()?.to_owned())),
        _ => Err(LuaError::runtime(format!("{key} must be a string"))),
    }
}

fn optional_array3(table: &Table, key: &str, destination: &mut [f32; 3]) -> mlua::Result<()> {
    if matches!(table.get::<Value>(key)?, Value::Nil) {
        return Ok(());
    }
    *destination = read_number_array3(table, key)?;
    Ok(())
}

fn optional_samples(table: &Table, key: &str, destination: &mut u32) -> mlua::Result<()> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(()),
        Value::Integer(value) => {
            *destination = u32::try_from(value)
                .map_err(|_| LuaError::runtime(format!("{key} must be a non-negative integer")))?;
            Ok(())
        }
        _ => Err(LuaError::runtime(format!("{key} must be an integer"))),
    }
}

/// Таблица камеры для `object:get_camera_settings()`. Блок `color` — устаревший прокси: exposure и
/// tonemapper читаются из эффективной постобработки камеры.
fn camera_settings_table(
    lua: &Lua,
    settings: CameraSettings,
    post: PostProcessSettings,
) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    let projection = lua.create_table()?;
    projection.set("fov_deg", settings.projection.fov_deg as f64)?;
    projection.set("near", settings.projection.near as f64)?;
    projection.set("far", settings.projection.far as f64)?;
    result.set("projection", projection)?;

    let render = lua.create_table()?;
    render.set("render_scale", settings.render.render_scale as f64)?;
    let clear_color = lua.create_table_with_capacity(3, 0)?;
    for (index, value) in settings.render.clear_color.into_iter().enumerate() {
        clear_color.raw_set(index + 1, value as f64)?;
    }
    render.set("clear_color", clear_color)?;
    render.set(
        "anti_aliasing",
        match settings.render.anti_aliasing {
            AntiAliasing::Fxaa => "fxaa",
            AntiAliasing::None => "none",
            AntiAliasing::Taa => "taa",
            AntiAliasing::Fsr => "fsr",
            AntiAliasing::Dlss => "dlss",
        },
    )?;
    result.set("render", render)?;

    let color = lua.create_table()?;
    color.set("exposure_ev", post.exposure.ev as f64)?;
    color.set(
        "tonemapper",
        match post.tonemapping.tonemapper {
            Tonemapper::Aces => "aces",
            Tonemapper::Neutral => "neutral",
            Tonemapper::None => "none",
        },
    )?;
    result.set("color", color)?;
    Ok(result)
}

fn apply_camera_settings_table(table: &Table, settings: &mut CameraSettings) -> mlua::Result<()> {
    ensure_known_fields(table, &["projection", "render", "color"], "camera settings")?;
    if let Value::Table(projection) = table.get::<Value>("projection")? {
        ensure_known_fields(&projection, &["fov_deg", "near", "far"], "projection")?;
        set_optional_number(&projection, "fov_deg", &mut settings.projection.fov_deg)?;
        set_optional_number(&projection, "near", &mut settings.projection.near)?;
        set_optional_number(&projection, "far", &mut settings.projection.far)?;
    } else if !matches!(table.get::<Value>("projection")?, Value::Nil) {
        return Err(LuaError::runtime(
            "camera settings.projection must be a table",
        ));
    }
    if let Value::Table(render) = table.get::<Value>("render")? {
        ensure_known_fields(
            &render,
            &["render_scale", "clear_color", "anti_aliasing"],
            "render",
        )?;
        set_optional_number(&render, "render_scale", &mut settings.render.render_scale)?;
        if let Value::Table(clear_color) = render.get::<Value>("clear_color")? {
            settings.render.clear_color = read_number_array3(&clear_color, "render.clear_color")?;
        } else if !matches!(render.get::<Value>("clear_color")?, Value::Nil) {
            return Err(LuaError::runtime(
                "render.clear_color must be an array of three numbers",
            ));
        }
        if let Value::String(value) = render.get::<Value>("anti_aliasing")? {
            settings.render.anti_aliasing = match value.to_str()?.as_ref() {
                "fxaa" => AntiAliasing::Fxaa,
                "none" => AntiAliasing::None,
                "taa" => AntiAliasing::Taa,
                "fsr" => AntiAliasing::Fsr,
                "dlss" => AntiAliasing::Dlss,
                _ => {
                    return Err(LuaError::runtime(
                        "render.anti_aliasing must be \"fxaa\", \"none\", \"taa\", \"fsr\", or \"dlss\"",
                    ));
                }
            };
        } else if !matches!(render.get::<Value>("anti_aliasing")?, Value::Nil) {
            return Err(LuaError::runtime("render.anti_aliasing must be a string"));
        }
    } else if !matches!(table.get::<Value>("render")?, Value::Nil) {
        return Err(LuaError::runtime("camera settings.render must be a table"));
    }
    if let Value::Table(color) = table.get::<Value>("color")? {
        ensure_known_fields(&color, &["exposure_ev", "tonemapper"], "color")?;
        let mut color_settings = settings.color.unwrap_or_default();
        set_optional_number(&color, "exposure_ev", &mut color_settings.exposure_ev)?;
        if let Value::String(value) = color.get::<Value>("tonemapper")? {
            color_settings.tonemapper = match value.to_str()?.as_ref() {
                "aces" => Tonemapper::Aces,
                "neutral" => Tonemapper::Neutral,
                "none" => Tonemapper::None,
                _ => {
                    return Err(LuaError::runtime(
                        "color.tonemapper must be \"aces\", \"neutral\", or \"none\"",
                    ));
                }
            };
        } else if !matches!(color.get::<Value>("tonemapper")?, Value::Nil) {
            return Err(LuaError::runtime("color.tonemapper must be a string"));
        }
        settings.color = Some(color_settings);
    } else if !matches!(table.get::<Value>("color")?, Value::Nil) {
        return Err(LuaError::runtime("camera settings.color must be a table"));
    }
    Ok(())
}

fn ensure_known_fields(table: &Table, allowed: &[&str], context: &str) -> mlua::Result<()> {
    for pair in table.clone().pairs::<String, Value>() {
        let (key, _) =
            pair.map_err(|_| LuaError::runtime(format!("{context} keys must be strings")))?;
        if !allowed.contains(&key.as_str()) {
            return Err(LuaError::runtime(format!(
                "unknown {context} field {key:?}"
            )));
        }
    }
    Ok(())
}

fn set_optional_number(table: &Table, key: &str, destination: &mut f32) -> mlua::Result<()> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(()),
        Value::Integer(value) => {
            *destination = value as f32;
            Ok(())
        }
        Value::Number(value) => {
            *destination = value as f32;
            Ok(())
        }
        _ => Err(LuaError::runtime(format!("{key} must be a number"))),
    }
}

fn read_number_array3(table: &Table, field: &str) -> mlua::Result<[f32; 3]> {
    let mut values = [None; 3];
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::Integer(index) = key else {
            return Err(LuaError::runtime(format!(
                "{field} must use indices 1, 2, and 3"
            )));
        };
        if !(1..=3).contains(&index) {
            return Err(LuaError::runtime(format!(
                "{field} must use indices 1, 2, and 3"
            )));
        }
        let index = (index - 1) as usize;
        if values[index].is_some() {
            return Err(LuaError::runtime(format!(
                "{field} contains duplicate indices"
            )));
        }
        let value = match value {
            Value::Integer(value) => value as f64,
            Value::Number(value) => value,
            _ => {
                return Err(LuaError::runtime(format!(
                    "{field} entries must be numbers"
                )));
            }
        };
        values[index] = Some(value as f32);
    }
    let [Some(x), Some(y), Some(z)] = values else {
        return Err(LuaError::runtime(format!(
            "{field} must have exactly three numbers"
        )));
    };
    Ok([x, y, z])
}

fn matrix_table(lua: &Lua, matrix: [f32; 16]) -> mlua::Result<Table> {
    let table = lua.create_table_with_capacity(16, 0)?;
    for (index, value) in matrix.into_iter().enumerate() {
        table.raw_set(index + 1, value as f64)?;
    }
    Ok(table)
}

fn send_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(
        move |lua, (target, name, value): (LuaString, LuaString, Value)| {
            let target = lua_message_target(target)?;
            let name = lua_message_name(name)?;
            let (value, value_size) = lua_message_param_value_with_size(lua, &active_call, value)?;
            with_active_call(&active_call, |transaction| {
                transaction
                    .send(&target, &name, value, value_size)
                    .map_err(|error| queue_message_error(error, &target))?;
                Ok(())
            })
        },
    )
}

fn broadcast_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |lua, (name, value): (LuaString, Value)| {
        let name = lua_message_name(name)?;
        let (value, value_size) = lua_message_param_value_with_size(lua, &active_call, value)?;
        with_active_call(&active_call, |transaction| {
            transaction
                .broadcast(&name, value, value_size)
                .map_err(|error| queue_message_error(error, ""))?;
            Ok(())
        })
    })
}

fn input_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let input = lua.create_table()?;
    input.set(
        "key_down",
        key_input_function(lua, active_call.clone(), ScriptInput::key_down)?,
    )?;
    input.set(
        "key_pressed",
        key_input_function(lua, active_call.clone(), ScriptInput::key_pressed)?,
    )?;
    input.set(
        "key_released",
        key_input_function(lua, active_call.clone(), ScriptInput::key_released)?,
    )?;
    input.set(
        "mouse_down",
        mouse_input_function(lua, active_call.clone(), ScriptInput::mouse_down)?,
    )?;
    input.set(
        "mouse_pressed",
        mouse_input_function(lua, active_call.clone(), ScriptInput::mouse_pressed)?,
    )?;
    input.set(
        "mouse_released",
        mouse_input_function(lua, active_call.clone(), ScriptInput::mouse_released)?,
    )?;
    input.set(
        "cursor",
        input_vector_function(lua, active_call.clone(), ScriptInput::cursor)?,
    )?;
    input.set(
        "mouse_delta",
        input_vector_function(lua, active_call.clone(), ScriptInput::mouse_delta)?,
    )?;
    input.set(
        "wheel",
        input_vector_function(lua, active_call.clone(), ScriptInput::wheel)?,
    )?;
    input.set(
        "action_down",
        action_button_function(lua, active_call.clone(), |value| value.down)?,
    )?;
    input.set(
        "action_pressed",
        action_button_function(lua, active_call.clone(), |value| value.pressed)?,
    )?;
    input.set(
        "action_released",
        action_button_function(lua, active_call.clone(), |value| value.released)?,
    )?;
    input.set("axis", action_axis_function(lua, active_call.clone())?)?;
    input.set("vector", action_vector_function(lua, active_call.clone())?)?;
    input.set(
        "context",
        action_context_function(lua, active_call.clone())?,
    )?;
    input.set(
        "set_context",
        set_context_function(lua, active_call.clone())?,
    )?;
    input.set("rebind", rebind_function(lua, active_call.clone())?)?;
    input.set("bindings", bindings_function(lua, active_call.clone())?)?;
    input.set(
        "reset_bindings",
        reset_bindings_function(lua, active_call.clone())?,
    )?;
    input.set(
        "pressed_inputs",
        pressed_inputs_function(lua, active_call.clone())?,
    )?;
    input.set(
        "set_cursor_visible",
        set_cursor_visible_function(lua, active_call.clone())?,
    )?;
    input.set(
        "set_cursor_locked",
        set_cursor_locked_function(lua, active_call.clone())?,
    )?;
    input.set(
        "cursor_visible",
        cursor_visible_function(lua, active_call.clone())?,
    )?;
    input.set(
        "cursor_locked",
        cursor_locked_function(lua, active_call.clone())?,
    )?;
    input.set(
        "gamepad_down",
        gamepad_button_function(
            lua,
            active_call.clone(),
            "rve.input.gamepad_down",
            |state| state.down,
        )?,
    )?;
    input.set(
        "gamepad_pressed",
        gamepad_button_function(
            lua,
            active_call.clone(),
            "rve.input.gamepad_pressed",
            |state| state.pressed,
        )?,
    )?;
    input.set(
        "gamepad_released",
        gamepad_button_function(
            lua,
            active_call.clone(),
            "rve.input.gamepad_released",
            |state| state.released,
        )?,
    )?;
    input.set(
        "gamepad_axis",
        gamepad_axis_function(lua, active_call.clone())?,
    )?;
    input.set(
        "gamepad_stick",
        gamepad_stick_function(lua, active_call.clone())?,
    )?;
    input.set(
        "gamepad_connected",
        gamepad_connected_function(lua, active_call.clone())?,
    )?;
    input.set("gamepad_rumble", gamepad_rumble_function(lua, active_call)?)?;
    Ok(input)
}

/// Reads an optional Lua pad argument: nil or 0 is every slot, 1..=4 is one slot.
fn gamepad_pad_arg(pad: Option<i64>, function: &str) -> mlua::Result<u32> {
    match pad {
        None => Ok(0),
        Some(pad) if (0..=4).contains(&pad) => Ok(pad as u32),
        Some(_) => Err(LuaError::runtime(format!(
            "{function} pad must be an integer from 0 to 4"
        ))),
    }
}

fn gamepad_button_function(
    lua: &Lua,
    active_call: ActiveCall,
    function: &'static str,
    field: fn(&RveGamepadState) -> u32,
) -> mlua::Result<Function> {
    lua.create_function(move |_, (name, pad): (LuaString, Option<i64>)| {
        let name = name.to_str()?;
        let button = GamepadButton::from_name(name.as_ref())
            .ok_or_else(|| LuaError::runtime(format!("unknown gamepad button {name:?}")))?;
        let pad = gamepad_pad_arg(pad, function)?;
        with_active_call(&active_call, |transaction| {
            let state = transaction.input().gamepad_state(pad).unwrap_or_default();
            Ok(field(&state) & (1 << button as u32) != 0)
        })
    })
}

fn gamepad_axis_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, (name, pad): (LuaString, Option<i64>)| {
        let name = name.to_str()?;
        let axis = GamepadAxis::from_name(name.as_ref())
            .ok_or_else(|| LuaError::runtime(format!("unknown gamepad axis {name:?}")))?;
        let pad = gamepad_pad_arg(pad, "rve.input.gamepad_axis")?;
        with_active_call(&active_call, |transaction| {
            let state = transaction.input().gamepad_state(pad).unwrap_or_default();
            Ok(state.axes[axis as usize])
        })
    })
}

fn gamepad_stick_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, (side, pad): (LuaString, Option<i64>)| {
        let side = side.to_str()?;
        let (x, y) = match side.as_ref() {
            "Left" => (GamepadAxis::LeftX, GamepadAxis::LeftY),
            "Right" => (GamepadAxis::RightX, GamepadAxis::RightY),
            _ => {
                return Err(LuaError::runtime(format!(
                    "unknown gamepad stick {side:?}, expected \"Left\" or \"Right\""
                )));
            }
        };
        let pad = gamepad_pad_arg(pad, "rve.input.gamepad_stick")?;
        with_active_call(&active_call, |transaction| {
            let state = transaction.input().gamepad_state(pad).unwrap_or_default();
            Ok((state.axes[x as usize], state.axes[y as usize]))
        })
    })
}

fn gamepad_connected_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, pad: Option<i64>| {
        let pad = gamepad_pad_arg(pad, "rve.input.gamepad_connected")?;
        with_active_call(&active_call, |transaction| {
            let state = transaction.input().gamepad_state(pad).unwrap_or_default();
            Ok(state.connected != 0)
        })
    })
}

fn gamepad_rumble_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(
        move |_, (strong, weak, seconds, pad): (f32, f32, f32, Option<i64>)| {
            let pad = gamepad_pad_arg(pad, "rve.input.gamepad_rumble")?;
            let request = RumbleRequest::new(pad, strong, weak, seconds)
                .ok_or_else(|| LuaError::runtime("rve.input.gamepad_rumble pad is invalid"))?;
            with_active_call(&active_call, |transaction| {
                transaction.stage_rumble(request).map_err(LuaError::runtime)
            })
        },
    )
}

fn set_cursor_visible_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, value: Value| {
        let visible = lua_boolean(value, "rve.input.set_cursor_visible")?;
        with_active_call(&active_call, |transaction| {
            transaction
                .set_cursor_visible(visible)
                .map_err(LuaError::runtime)
        })
    })
}

fn set_cursor_locked_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, value: Value| {
        let locked = lua_boolean(value, "rve.input.set_cursor_locked")?;
        with_active_call(&active_call, |transaction| {
            transaction
                .set_cursor_locked(locked)
                .map_err(LuaError::runtime)
        })
    })
}

fn cursor_visible_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        with_active_call(&active_call, |transaction| Ok(transaction.cursor().visible))
    })
}

fn cursor_locked_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        with_active_call(&active_call, |transaction| Ok(transaction.cursor().locked))
    })
}

/// mlua приводит любое значение к bool, поэтому API принимает только настоящий boolean.
pub(crate) fn lua_boolean(value: Value, function: &str) -> mlua::Result<bool> {
    match value {
        Value::Boolean(value) => Ok(value),
        other => Err(LuaError::runtime(format!(
            "{function} expects a boolean, got {}",
            other.type_name()
        ))),
    }
}

struct AssetUserData(VirtualPath);

impl UserData for AssetUserData {}

fn asset_function(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|lua, path: LuaString| {
        let path = path
            .to_str()
            .map_err(|_| LuaError::runtime("asset paths must be valid UTF-8"))?;
        let path = VirtualPath::parse(path.as_ref())
            .map_err(|error| LuaError::runtime(error.to_string()))?;
        lua.create_userdata(AssetUserData(path))
    })
}

fn save_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let save = lua.create_table()?;
    save.set(
        "set",
        lua.create_function({
            let active_call = active_call.clone();
            move |lua, (key, value): (LuaString, Value)| {
                let key = key
                    .to_str()
                    .map_err(|_| LuaError::runtime("save keys must be valid UTF-8"))?
                    .to_owned();
                with_active_call(&active_call, |transaction| {
                    let mut budget = MessageValueBudget::new();
                    let value = lua_param_value_inner(
                        value,
                        0,
                        &mut budget,
                        Some(lua),
                        Some(&active_call),
                    )?;
                    transaction.save_set(&key, value).map_err(LuaError::runtime)
                })
            }
        })?,
    )?;
    save.set(
        "get",
        lua.create_function({
            let active_call = active_call.clone();
            move |lua, key: LuaString| {
                let key = key
                    .to_str()
                    .map_err(|_| LuaError::runtime("save keys must be valid UTF-8"))?
                    .to_owned();
                let value =
                    with_active_call(&active_call, |transaction| Ok(transaction.save_get(&key)))?;
                match value {
                    Some(ParamValue::Asset(path)) => {
                        Ok(Value::UserData(lua.create_userdata(AssetUserData(path))?))
                    }
                    Some(value) => param_value(lua, &value, &active_call, None),
                    None => Ok(Value::Nil),
                }
            }
        })?,
    )?;
    let delete_active = active_call.clone();
    save.set(
        "delete",
        lua.create_function(move |_, key: LuaString| {
            let key = key
                .to_str()
                .map_err(|_| LuaError::runtime("save keys must be valid UTF-8"))?
                .to_owned();
            with_active_call(&delete_active, |transaction| {
                transaction.save_delete(&key).map_err(LuaError::runtime)
            })
        })?,
    )?;
    let has_active = active_call.clone();
    save.set(
        "has",
        lua.create_function(move |_, key: LuaString| {
            let key = key
                .to_str()
                .map_err(|_| LuaError::runtime("save keys must be valid UTF-8"))?
                .to_owned();
            with_active_call(&has_active, |transaction| {
                transaction.save_has(&key).map_err(LuaError::runtime)
            })
        })?,
    )?;
    let keys_active = active_call.clone();
    save.set(
        "keys",
        lua.create_function(move |lua, ()| {
            let keys = with_active_call(&keys_active, |transaction| Ok(transaction.save_keys()))?;
            let table = lua.create_table_with_capacity(keys.len(), 0)?;
            for (index, key) in keys.into_iter().enumerate() {
                table.raw_set(index + 1, key)?;
            }
            Ok(table)
        })?,
    )?;
    let flush_active = active_call;
    save.set(
        "flush",
        lua.create_function(move |_, ()| {
            with_active_call(&flush_active, |transaction| {
                transaction.save_flush().map_err(LuaError::runtime)
            })?;
            Ok(true)
        })?,
    )?;
    Ok(save)
}

fn action_button_function(
    lua: &Lua,
    active_call: ActiveCall,
    state: fn(ActionValue) -> bool,
) -> mlua::Result<Function> {
    lua.create_function(move |_, name: LuaString| {
        let name = name.to_str()?;
        with_active_call(&active_call, |transaction| {
            let value = transaction
                .action_state(name.as_ref(), InputActionKind::Button)
                .map_err(LuaError::runtime)?;
            Ok(state(value))
        })
    })
}

fn action_axis_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, name: LuaString| {
        let name = name.to_str()?;
        with_active_call(&active_call, |transaction| {
            let value = transaction
                .action_state(name.as_ref(), InputActionKind::Axis)
                .map_err(LuaError::runtime)?;
            Ok(value.value[0])
        })
    })
}

fn action_vector_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, name: LuaString| {
        let name = name.to_str()?;
        with_active_call(&active_call, |transaction| {
            let value = transaction
                .action_state(name.as_ref(), InputActionKind::Vector2)
                .map_err(LuaError::runtime)?;
            Ok((value.value[0], value.value[1]))
        })
    })
}

fn action_context_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        with_active_call(&active_call, |transaction| {
            Ok(transaction.action_context().map(str::to_owned))
        })
    })
}

fn set_context_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, name: LuaString| {
        let name = name.to_str()?.to_owned();
        with_active_call(&active_call, |transaction| {
            transaction
                .stage_action_mutation(ActionMutation::SetContext(name))
                .map_err(LuaError::runtime)
        })
    })
}

fn rebind_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(
        move |_, (context, action, slot, inputs): (LuaString, LuaString, LuaString, Table)| {
            let context = context.to_str()?.to_owned();
            let action = action.to_str()?.to_owned();
            let slot = slot.to_str()?.to_owned();
            let inputs = read_input_array(&inputs)?;
            with_active_call(&active_call, |transaction| {
                transaction
                    .stage_action_mutation(ActionMutation::Rebind {
                        context,
                        action,
                        slot,
                        inputs,
                    })
                    .map_err(LuaError::runtime)
            })
        },
    )
}

fn bindings_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(
        move |lua, (context, action, slot): (LuaString, LuaString, LuaString)| {
            let context = context.to_str()?.to_owned();
            let action = action.to_str()?.to_owned();
            let slot = slot.to_str()?.to_owned();
            with_active_call(&active_call, |transaction| {
                let bindings = transaction
                    .action_bindings(&context, &action, &slot)
                    .map_err(LuaError::runtime)?;
                let result = lua.create_table_with_capacity(bindings.len(), 0)?;
                for (index, binding) in bindings.iter().enumerate() {
                    result.raw_set(index + 1, binding.as_str())?;
                }
                Ok(result)
            })
        },
    )
}

fn reset_bindings_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        with_active_call(&active_call, |transaction| {
            transaction
                .stage_action_mutation(ActionMutation::ResetBindings)
                .map_err(LuaError::runtime)
        })
    })
}

fn pressed_inputs_function(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Function> {
    lua.create_function(move |lua, ()| {
        with_active_call(&active_call, |transaction| {
            let mut inputs = Vec::new();
            for code in 0..65 {
                let Some(key) = Key::from_code(code) else {
                    continue;
                };
                if transaction.input().key_pressed(key) {
                    inputs.push(format!("{key:?}"));
                }
            }
            for code in 0..5 {
                let Some(button) = MouseButton::from_code(code) else {
                    continue;
                };
                if transaction.input().mouse_pressed(button) {
                    inputs.push(format!("Mouse.{button:?}"));
                }
            }
            for pad in 1..=RVE_GAMEPAD_SLOTS as u32 {
                let Some(state) = transaction.input().gamepad_state(pad) else {
                    continue;
                };
                for button in GamepadButton::ALL {
                    if state.pressed & (1 << button as u32) != 0 {
                        inputs.push(format!("Gamepad{pad}.{}", button.name()));
                    }
                }
            }
            let result = lua.create_table_with_capacity(inputs.len(), 0)?;
            for (index, input) in inputs.iter().enumerate() {
                result.raw_set(index + 1, input.as_str())?;
            }
            Ok(result)
        })
    })
}

fn read_input_array(table: &Table) -> mlua::Result<Vec<PhysicalInput>> {
    let mut inputs = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::Integer(index) = key else {
            return Err(LuaError::runtime("input bindings must be an array"));
        };
        if index < 1 || index > RVE_INPUT_MAX_BINDINGS as i64 {
            return Err(LuaError::runtime(
                "input bindings must contain at most 16 entries",
            ));
        }
        let Value::String(value) = value else {
            return Err(LuaError::runtime("input binding names must be strings"));
        };
        let value = value.to_str()?;
        let input = PhysicalInput::from_name(value.as_ref()).ok_or_else(|| {
            LuaError::runtime(format!("unknown physical input {:?}", value.as_ref()))
        })?;
        inputs.push((index, input));
    }
    inputs.sort_by_key(|(index, _)| *index);
    if inputs
        .iter()
        .enumerate()
        .any(|(offset, (index, _))| *index as usize != offset + 1)
    {
        return Err(LuaError::runtime("input bindings must be a dense array"));
    }
    Ok(inputs.into_iter().map(|(_, input)| input).collect())
}

fn key_input_function(
    lua: &Lua,
    active_call: ActiveCall,
    state: fn(&ScriptInput, Key) -> bool,
) -> mlua::Result<Function> {
    lua.create_function(move |_, name: LuaString| {
        let name = name.to_str()?;
        let key = Key::from_name(name.as_ref())
            .ok_or_else(|| LuaError::runtime(format!("unknown input key {name:?}")))?;
        with_active_call(&active_call, |transaction| {
            Ok(state(transaction.input(), key))
        })
    })
}

fn mouse_input_function(
    lua: &Lua,
    active_call: ActiveCall,
    state: fn(&ScriptInput, MouseButton) -> bool,
) -> mlua::Result<Function> {
    lua.create_function(move |_, name: LuaString| {
        let name = name.to_str()?;
        let button = MouseButton::from_name(name.as_ref())
            .ok_or_else(|| LuaError::runtime(format!("unknown mouse button {name:?}")))?;
        with_active_call(&active_call, |transaction| {
            Ok(state(transaction.input(), button))
        })
    })
}

fn input_vector_function(
    lua: &Lua,
    active_call: ActiveCall,
    value: fn(&ScriptInput) -> [f32; 2],
) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        with_active_call(&active_call, |transaction| {
            let [x, y] = value(transaction.input());
            Ok((x, y))
        })
    })
}

fn queue_message_error(error: QueueMessageError, target: &str) -> LuaError {
    match error {
        QueueMessageError::ReadOnly => {
            LuaError::runtime("rve.send and rve.broadcast are unavailable during script creation")
        }
        QueueMessageError::UnknownTarget => LuaError::runtime(format!(
            "rve.send target {:?} does not exist",
            shortened_target(target)
        )),
        QueueMessageError::InvalidName => LuaError::runtime(format!(
            "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
        )),
        QueueMessageError::InvalidValue => {
            LuaError::runtime("message value is invalid or exceeds the message limits")
        }
    }
}

fn lua_message_target(value: LuaString) -> mlua::Result<String> {
    if value.as_bytes().len() > MAX_MESSAGE_TARGET_ID_BYTES {
        return Err(LuaError::runtime(format!(
            "message target id exceeds {MAX_MESSAGE_TARGET_ID_BYTES} bytes"
        )));
    }
    value
        .to_str()
        .map(|value| value.to_owned())
        .map_err(|_| LuaError::runtime("message target ids must contain valid UTF-8"))
}

fn shortened_target(target: &str) -> String {
    let mut chars = target.chars();
    let prefix = chars.by_ref().take(64).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn lua_message_name(value: LuaString) -> mlua::Result<String> {
    if value.as_bytes().len() > MAX_MESSAGE_NAME_BYTES {
        return Err(LuaError::runtime(format!(
            "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
        )));
    }
    value
        .to_str()
        .map(|value| value.to_owned())
        .map_err(|_| LuaError::runtime("message names must contain valid UTF-8"))
}

fn lua_param_value(value: Value) -> mlua::Result<ParamValue> {
    lua_param_value_with_size(value).map(|(value, _)| value)
}

fn lua_param_value_with_size(value: Value) -> mlua::Result<(ParamValue, usize)> {
    let mut budget = MessageValueBudget::new();
    let value = lua_param_value_inner(value, 0, &mut budget, None, None)?;
    Ok((value, budget.estimated_size()))
}

fn lua_message_param_value_with_size(
    lua: &Lua,
    active_call: &ActiveCall,
    value: Value,
) -> mlua::Result<(ParamValue, usize)> {
    let mut budget = MessageValueBudget::new();
    let value = lua_param_value_inner(value, 0, &mut budget, Some(lua), Some(active_call))?;
    Ok((value, budget.estimated_size()))
}

fn lua_param_value_inner(
    value: Value,
    depth: usize,
    budget: &mut MessageValueBudget,
    lua: Option<&Lua>,
    active_call: Option<&ActiveCall>,
) -> mlua::Result<ParamValue> {
    budget.enter(depth).map_err(LuaError::runtime)?;

    match value {
        Value::Boolean(value) => Ok(ParamValue::Bool(value)),
        Value::Integer(value) => Ok(ParamValue::Int(value)),
        Value::Number(value) if value.is_finite() => Ok(ParamValue::Float(value)),
        Value::Number(_) => Err(LuaError::runtime("message numbers must be finite")),
        Value::String(value) => {
            budget
                .add_string(value.as_bytes().len())
                .map_err(LuaError::runtime)?;
            value
                .to_str()
                .map(|value| ParamValue::String(value.to_owned()))
                .map_err(|_| LuaError::runtime("message strings must contain valid UTF-8"))
        }
        Value::UserData(value) => {
            let asset = value
                .borrow::<AssetUserData>()
                .map_err(|_| LuaError::runtime("save userdata values must be assets"))?;
            budget
                .add_string(asset.0.as_str().len())
                .map_err(LuaError::runtime)?;
            Ok(ParamValue::Asset(asset.0.clone()))
        }
        Value::Table(table) => {
            if let (Some(lua), Some(active_call)) = (lua, active_call)
                && let Some(handle) = object_handle_of(lua, &table)?
            {
                let id = with_active_call(active_call, |transaction| {
                    Ok(transaction.object_id(handle).map(str::to_owned))
                })?
                .ok_or_else(|| LuaError::runtime("message object reference is no longer valid"))?;
                budget.add_string(id.len()).map_err(LuaError::runtime)?;
                return Ok(ParamValue::Object(id));
            }
            let mut entries = Vec::new();
            table.for_each::<Value, Value>(|key, value| {
                let Some(index) = sequence_index(key) else {
                    return Err(LuaError::runtime(
                        "message tables must be sequences with integer keys 1..n",
                    ));
                };
                if entries.len() >= budget.remaining_elements() {
                    return Err(LuaError::runtime(
                        "message value contains more than 1024 elements",
                    ));
                }
                entries.push((index, value));
                Ok(())
            })?;
            entries.sort_unstable_by_key(|(index, _)| *index);
            if entries
                .iter()
                .enumerate()
                .any(|(offset, (index, _))| *index != offset + 1)
            {
                return Err(LuaError::runtime(
                    "message tables must have consecutive integer keys 1..n",
                ));
            }
            entries
                .into_iter()
                .map(|(_, value)| lua_param_value_inner(value, depth + 1, budget, lua, active_call))
                .collect::<mlua::Result<Vec<_>>>()
                .map(ParamValue::Array)
        }
        _ => Err(LuaError::runtime(
            "message values must be booleans, integers, finite numbers, UTF-8 strings, object references, or sequence tables",
        )),
    }
}

fn sequence_index(key: Value) -> Option<usize> {
    match key {
        Value::Integer(index) => usize::try_from(index).ok().filter(|index| *index > 0),
        Value::Number(index)
            if index.is_finite()
                && index.fract() == 0.0
                && index >= 1.0
                && index <= usize::MAX as f64 =>
        {
            Some(index as usize)
        }
        _ => None,
    }
}

fn text_load_function(lua: &Lua, default_environment: Table) -> mlua::Result<Function> {
    lua.create_function(
        move |lua,
              (source, chunk_name, mode, environment): (
            LuaString,
            Option<String>,
            Option<String>,
            Option<Table>,
        )| {
            if mode.as_deref().is_some_and(|mode| mode != "t") {
                return Ok((
                    Option::<Function>::None,
                    Some("only text chunks are allowed (mode must be \"t\")".to_owned()),
                ));
            }
            let source_bytes = source.as_bytes();
            let mut chunk = lua
                .load(source_bytes.as_ref())
                .set_name(chunk_name.unwrap_or_else(|| "=(load)".to_owned()))
                .set_mode(ChunkMode::Text);
            chunk =
                chunk.set_environment(environment.unwrap_or_else(|| default_environment.clone()));
            match chunk.into_function() {
                Ok(function) => Ok((Some(function), Option::<String>::None)),
                Err(error) => Ok((Option::<Function>::None, Some(error.to_string()))),
            }
        },
    )
}

fn script_environment(lua: &Lua, base_environment: &Table) -> mlua::Result<Table> {
    let environment = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.set("__index", base_environment.clone())?;
    metatable.set("__metatable", false)?;
    environment.set_metatable(Some(metatable))?;
    environment.set("_G", environment.clone())?;
    environment.set("load", text_load_function(lua, environment.clone())?)?;
    Ok(environment)
}

fn restricted_gc_function(lua: &Lua) -> mlua::Result<Function> {
    let original: Function = lua.globals().get("collectgarbage")?;
    lua.create_function(move |_, (mode, argument): (Option<String>, Option<i32>)| {
        let mode = mode.unwrap_or_else(|| "collect".to_owned());
        if !matches!(mode.as_str(), "collect" | "count" | "step") {
            return Err(LuaError::runtime(format!(
                "collectgarbage mode {mode:?} is not allowed"
            )));
        }
        match argument {
            Some(argument) => original.call::<MultiValue>((mode, argument)),
            None => original.call::<MultiValue>(mode),
        }
    })
}

fn params_table(
    lua: &Lua,
    params: &ScriptParams,
    object_refs: &std::collections::HashMap<String, ScriptObjectRef>,
    active_call: ActiveCall,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (name, value) in params {
        table.set(
            name.as_str(),
            param_value(lua, value, &active_call, Some(object_refs))?,
        )?;
    }
    Ok(table)
}

fn param_value(
    lua: &Lua,
    value: &ParamValue,
    active_call: &ActiveCall,
    object_refs: Option<&std::collections::HashMap<String, ScriptObjectRef>>,
) -> mlua::Result<Value> {
    match value {
        ParamValue::Bool(value) => Ok(Value::Boolean(*value)),
        ParamValue::Int(value) => Ok(Value::Integer(*value)),
        ParamValue::Float(value) => Ok(Value::Number(*value)),
        ParamValue::String(value) => Ok(Value::String(lua.create_string(value)?)),
        ParamValue::Asset(value) => Ok(Value::String(lua.create_string(value.as_str())?)),
        ParamValue::Object(id) if id.is_empty() => Ok(Value::Nil),
        ParamValue::Object(id) => {
            let reference = if let Some(reference) = object_refs.and_then(|refs| refs.get(id)) {
                Some((reference.handle, Some(id.clone()), reference.name.clone()))
            } else {
                with_active_call(active_call, |transaction| {
                    Ok(transaction.handle_by_id(id).and_then(|handle| {
                        transaction
                            .object_identity(handle)
                            .map(|(object_id, name)| (handle, object_id, name))
                    }))
                })?
            };
            let Some((handle, object_id, name)) = reference else {
                return Ok(Value::Nil);
            };
            Ok(Value::Table(object_table(
                lua,
                handle,
                object_id,
                name,
                active_call.clone(),
            )?))
        }
        ParamValue::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.set(
                    index + 1,
                    param_value(lua, value, active_call, object_refs)?,
                )?;
            }
            Ok(Value::Table(table))
        }
    }
}

struct TransformUserData {
    value: ScriptTransform,
}

impl UserData for TransformUserData {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("position", |lua, this| {
            vector_table(lua, this.value.position)
        });
        fields.add_field_method_set("position", |_, this, table: Table| {
            this.value.position = read_vector(&table, "position")?;
            Ok(())
        });
        fields.add_field_method_get("rotation", |lua, this| {
            vector_table(lua, this.value.rotation)
        });
        fields.add_field_method_set("rotation", |_, this, table: Table| {
            this.value.rotation = read_vector(&table, "rotation")?;
            Ok(())
        });
        fields.add_field_method_get("scale", |lua, this| vector_table(lua, this.value.scale));
        fields.add_field_method_set("scale", |_, this, table: Table| {
            this.value.scale = read_vector(&table, "scale")?;
            Ok(())
        });
    }
}

fn vector_table(lua: &Lua, value: [f32; 3]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", value[0] as f64)?;
    table.set("y", value[1] as f64)?;
    table.set("z", value[2] as f64)?;
    Ok(table)
}

fn read_vector(table: &Table, property: &str) -> mlua::Result<[f32; 3]> {
    let mut result = [0.0; 3];
    for (index, axis) in ["x", "y", "z"].into_iter().enumerate() {
        let value = table.get::<f64>(axis).map_err(|error| {
            LuaError::runtime(format!(
                "transform.{property}.{axis} must be a finite number: {error}"
            ))
        })?;
        let converted = value as f32;
        if !value.is_finite() || !converted.is_finite() {
            return Err(LuaError::runtime(format!(
                "transform.{property}.{axis} must be a finite f32 number"
            )));
        }
        result[index] = converted;
    }
    Ok(result)
}

/// Ссылка на экземпляр скрипта, полученная через `obj:script(key)`. Серийный номер не повторяется,
/// поэтому после удаления объекта или смены сцены ссылка даёт ошибку, а не чужой экземпляр.
struct ScriptRefUserData {
    serial: u64,
    active_call: ActiveCall,
}

impl mlua::UserData for ScriptRefUserData {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, name: LuaString| {
            let name = lua_text_key(Value::String(name), "script.get")?;
            let value = with_active_call(&this.active_call, |transaction| {
                transaction
                    .script_get(this.serial, &name)
                    .map_err(|error| LuaError::runtime(error.message()))
            })?;
            param_value(lua, &value, &this.active_call, None)
        });
        methods.add_method("set", |lua, this, (name, value): (LuaString, Value)| {
            let name = lua_text_key(Value::String(name), "script.set")?;
            let (value, _) = lua_message_param_value_with_size(lua, &this.active_call, value)?;
            with_active_call(&this.active_call, |transaction| {
                transaction
                    .script_set(this.serial, &name, &value)
                    .map_err(|error| LuaError::runtime(error.message()))
            })
        });
        methods.add_method("send", |lua, this, (name, value): (LuaString, Value)| {
            let name = lua_message_name(name)?;
            let (value, value_size) =
                lua_message_param_value_with_size(lua, &this.active_call, value)?;
            with_active_call(&this.active_call, |transaction| {
                transaction
                    .script_send(this.serial, &name, value, value_size)
                    .map_err(|error| LuaError::runtime(format!("script.send: {error:?}")))
            })
        });
        methods.add_method("enabled", |_, this, ()| {
            with_active_call(&this.active_call, |transaction| {
                transaction
                    .script_enabled_of(this.serial)
                    .map_err(|error| LuaError::runtime(error.message()))
            })
        });
        methods.add_method("set_enabled", |_, this, value: Value| {
            let enabled = lua_boolean(value, "script.set_enabled")?;
            with_active_call(&this.active_call, |transaction| {
                transaction
                    .set_script_enabled_of(this.serial, enabled)
                    .map_err(|error| LuaError::runtime(error.message()))
            })
        });
    }
}

/// `obj:script(key)`: ссылка на экземпляр объекта по виртуальному пути lite-скрипта или имени
/// heavy-скрипта; `nil`, если такого экземпляра нет.
fn add_script_ref_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    object.set(
        "script",
        lua.create_function(move |lua, arguments: MultiValue| {
            let values = method_arguments(lua, arguments, handle)?;
            let key = lua_text_key(
                values.into_iter().next().unwrap_or(Value::Nil),
                "object.script",
            )?;
            let key = normalize_script_key(&key);
            let serial = with_active_call(&active_call, |transaction| {
                Ok(transaction.script_ref(handle, &key))
            })?;
            match serial {
                Some(serial) => Ok(Value::UserData(lua.create_userdata(ScriptRefUserData {
                    serial,
                    active_call: active_call.clone(),
                })?)),
                None => Ok(Value::Nil),
            }
        })?,
    )
}

impl LuaInstance {
    /// Записывает объявленные свойства в Lua-таблицу `params`. Вызывается перед вызовом экземпляра,
    /// если другой скрипт изменил его свойства.
    pub(crate) fn write_params(
        &mut self,
        lua: &Lua,
        properties: &std::collections::BTreeMap<String, ParamValue>,
    ) -> mlua::Result<()> {
        self.stage_params(lua, properties)?;
        for (name, value) in properties {
            self.property_values.insert(name.clone(), value.clone());
        }
        Ok(())
    }

    /// Writes canonical callback values into `self.params` without committing their rollback base.
    pub(crate) fn stage_params(
        &mut self,
        lua: &Lua,
        properties: &std::collections::BTreeMap<String, ParamValue>,
    ) -> mlua::Result<()> {
        let params: Table = self.instance.get("params")?;
        self.cache_object_refs(properties);
        for (name, value) in properties {
            params.set(
                name.as_str(),
                param_value(lua, value, &self.active_call, Some(&self.object_refs))?,
            )?;
        }
        Ok(())
    }

    fn cache_object_refs(&mut self, properties: &ScriptParams) {
        fn collect<'a>(value: &'a ParamValue, ids: &mut Vec<&'a str>) {
            match value {
                ParamValue::Object(id) if !id.is_empty() => ids.push(id),
                ParamValue::Array(values) => {
                    for value in values {
                        collect(value, ids);
                    }
                }
                _ => {}
            }
        }

        let mut ids = Vec::new();
        for value in properties.values() {
            collect(value, &mut ids);
        }
        for id in ids {
            if self.object_refs.contains_key(id) {
                continue;
            }
            let resolved = with_active_call(&self.active_call, |transaction| {
                Ok(transaction.handle_by_id(id).and_then(|handle| {
                    transaction
                        .object_identity(handle)
                        .map(|(_, name)| (handle, name.to_owned()))
                }))
            });
            if let Ok(Some((handle, name))) = resolved {
                self.object_refs
                    .insert(id.to_owned(), ScriptObjectRef { handle, name });
            }
        }
    }
}

/// Текст ключа свойства или скрипта: любая UTF-8 строка, без ограничения длины имени сообщения.
fn lua_text_key(value: Value, function: &str) -> mlua::Result<String> {
    match value {
        Value::String(text) => text
            .to_str()
            .map(|text| text.to_owned())
            .map_err(|_| LuaError::runtime(format!("{function} expects valid UTF-8 text"))),
        other => Err(LuaError::runtime(format!(
            "{function} expects a string, got {}",
            other.type_name()
        ))),
    }
}

/// `rve.scene`: `load(path)` ставит запрос загрузки сцены проекта, `current()` возвращает имя
/// текущей сцены. Запрос выполняется после конца кадра, если сцена объявлена и включена.
fn scene_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let scene = lua.create_table()?;
    let load_call = active_call.clone();
    scene.set(
        "load",
        lua.create_function(move |_, path: LuaString| {
            let path = lua_text_key(Value::String(path), "rve.scene.load")?;
            with_active_call(&load_call, |transaction| {
                transaction
                    .request_scene_load(&path)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    scene.set(
        "current",
        lua.create_function(move |lua, ()| {
            let current =
                with_active_call(&active_call, |transaction| Ok(transaction.current_scene()))?;
            Ok(Value::String(lua.create_string(&current)?))
        })?,
    )?;
    Ok(scene)
}

/// `rve.app`: `quit()` запрашивает штатное завершение после успешного callback.
fn app_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let app = lua.create_table()?;
    app.set(
        "quit",
        lua.create_function(move |_, ()| {
            with_active_call(&active_call, |transaction| {
                transaction.request_quit().map_err(LuaError::runtime)
            })
        })?,
    )?;
    Ok(app)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::MAX_MESSAGE_TARGET_ID_BYTES;
    use engine_scene::{MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_STRING_BYTES};

    fn convert(source: &str) -> mlua::Result<ParamValue> {
        let lua = Lua::new();
        lua_param_value(lua.load(source).eval::<Value>()?)
    }

    #[test]
    fn lua_properties_parse_reference_asset_enum_color_and_array_kinds() {
        let source = r#"return {
            properties = {
                target = { type = "component", component = "rigid_body" },
                script = { type = "component", script = "scripts/actor.lua" },
                model = { type = "asset", asset = "model" },
                mode = { type = "enum", values = { "idle", "run" }, default = "run" },
                tint = { type = "color", default = { 0.1, 0.2, 0.3 } },
                weights = { type = "array", of = { type = "float" }, default = { 1, 2.5 } },
            },
        }"#;
        let schema = properties("refs.lua", source, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .unwrap();
        assert_eq!(
            schema
                .specs()
                .iter()
                .map(|spec| spec.kind.clone())
                .collect::<Vec<_>>(),
            [
                ParamKind::Enum(vec!["idle".into(), "run".into()]),
                ParamKind::Asset(Some(AssetKind::Model)),
                ParamKind::Component(ComponentKind::Script("scripts/actor.lua".into())),
                ParamKind::Component(ComponentKind::RigidBody),
                ParamKind::Color,
                ParamKind::Array(Box::new(ParamKind::Float)),
            ]
        );
        assert_eq!(
            schema.resolve(&Default::default()).unwrap()["mode"],
            ParamValue::String("run".into())
        );
    }

    #[test]
    fn lua_properties_reject_bad_reference_and_collection_schemas() {
        for property in [
            r#"{ type = "component", component = "missing" }"#,
            r#"{ type = "asset", asset = "missing" }"#,
            r#"{ type = "enum", values = {} }"#,
            r#"{ type = "array", of = { type = "unknown" } }"#,
        ] {
            let source = format!("return {{ properties = {{ value = {property} }} }}");
            assert!(
                properties("bad.lua", &source, Arc::new(AtomicBool::new(false))).is_err(),
                "accepted {property}"
            );
        }
    }

    #[test]
    fn lua_message_values_accept_scalars_and_dense_sequences() {
        assert_eq!(convert("true").unwrap(), ParamValue::Bool(true));
        assert_eq!(convert("17").unwrap(), ParamValue::Int(17));
        assert_eq!(convert("1.25").unwrap(), ParamValue::Float(1.25));
        assert_eq!(
            convert("{ 'hello', 2, false, { 3.5 } }").unwrap(),
            ParamValue::Array(vec![
                ParamValue::String("hello".to_owned()),
                ParamValue::Int(2),
                ParamValue::Bool(false),
                ParamValue::Array(vec![ParamValue::Float(3.5)]),
            ])
        );
        assert_eq!(convert("{}").unwrap(), ParamValue::Array(Vec::new()));
    }

    #[test]
    fn lua_message_values_accept_exact_limits() {
        let lua = Lua::new();

        let mut nested = Value::Integer(1);
        for _ in 0..MAX_MESSAGE_DEPTH {
            let table = lua.create_table().unwrap();
            table.set(1, nested).unwrap();
            nested = Value::Table(table);
        }
        assert!(lua_param_value(nested).is_ok());

        let table = lua.create_table().unwrap();
        for index in 1..MAX_MESSAGE_ELEMENTS {
            table.set(index, true).unwrap();
        }
        assert_eq!(
            match lua_param_value(Value::Table(table)).unwrap() {
                ParamValue::Array(values) => values.len(),
                _ => panic!("sequence table must become an array"),
            },
            MAX_MESSAGE_ELEMENTS - 1
        );

        let first = lua
            .create_string(vec![b'a'; MAX_MESSAGE_STRING_BYTES / 2])
            .unwrap();
        let second = lua
            .create_string(vec![b'b'; MAX_MESSAGE_STRING_BYTES / 2])
            .unwrap();
        let value = lua.create_table().unwrap();
        value.set(1, first).unwrap();
        value.set(2, second).unwrap();
        let ParamValue::Array(values) = lua_param_value(Value::Table(value)).unwrap() else {
            panic!("sequence table must become an array");
        };
        let string_bytes = values
            .iter()
            .map(|value| match value {
                ParamValue::String(value) => value.len(),
                _ => panic!("Lua strings must become string values"),
            })
            .sum::<usize>();
        assert_eq!(string_bytes, MAX_MESSAGE_STRING_BYTES);
    }

    #[test]
    fn lua_message_values_reject_non_sequences_and_non_finite_numbers() {
        for source in [
            "{ [1] = true, [3] = false }",
            "{ [1] = true, label = 2 }",
            "0 / 0",
        ] {
            assert!(convert(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    fn lua_message_values_reject_invalid_utf8_depth_size_and_cycles() {
        let lua = Lua::new();
        let invalid_utf8 = lua.create_string([0xff]).unwrap();
        assert!(lua_param_value(Value::String(invalid_utf8)).is_err());

        let mut nested = Value::Integer(1);
        for _ in 0..(MAX_MESSAGE_DEPTH + 2) {
            let table = lua.create_table().unwrap();
            table.set(1, nested).unwrap();
            nested = Value::Table(table);
        }
        assert!(lua_param_value(nested).is_err());

        let table = lua.create_table().unwrap();
        for index in 1..=MAX_MESSAGE_ELEMENTS {
            table.set(index, index as i64).unwrap();
        }
        assert!(lua_param_value(Value::Table(table)).is_err());

        let cycle = lua.create_table().unwrap();
        cycle.set(1, cycle.clone()).unwrap();
        let error = lua_param_value(Value::Table(cycle)).unwrap_err();
        assert!(error.to_string().contains("nesting exceeds"));
    }

    #[test]
    fn lua_message_values_reject_too_many_string_bytes_before_copying() {
        let lua = Lua::new();
        let value = lua
            .create_string(vec![b'x'; MAX_MESSAGE_STRING_BYTES + 1])
            .unwrap();
        let error = lua_param_value(Value::String(value)).unwrap_err();
        assert!(error.to_string().contains("65536 bytes"));

        let first = lua
            .create_string(vec![b'a'; MAX_MESSAGE_STRING_BYTES / 2])
            .unwrap();
        let second = lua
            .create_string(vec![b'b'; MAX_MESSAGE_STRING_BYTES / 2 + 1])
            .unwrap();
        let nested = lua.create_table().unwrap();
        nested.set(1, second).unwrap();
        let root = lua.create_table().unwrap();
        root.set(1, first).unwrap();
        root.set(2, nested).unwrap();
        let error = lua_param_value(Value::Table(root)).unwrap_err();
        assert!(error.to_string().contains("65536 bytes"));
    }

    #[test]
    fn lua_message_names_are_limited_by_utf8_bytes() {
        let lua = Lua::new();
        let max = lua.create_string("n".repeat(256)).unwrap();
        assert_eq!(lua_message_name(max).unwrap().len(), 256);

        let too_long = lua.create_string("n".repeat(257)).unwrap();
        let error = lua_message_name(too_long).unwrap_err();
        assert!(error.to_string().contains("256 bytes"));
    }

    #[test]
    fn lua_message_targets_are_limited_before_copying_and_errors_are_shortened() {
        let lua = Lua::new();
        let max = lua
            .create_string(vec![b't'; MAX_MESSAGE_TARGET_ID_BYTES])
            .unwrap();
        assert_eq!(
            lua_message_target(max).unwrap().len(),
            MAX_MESSAGE_TARGET_ID_BYTES
        );

        let too_long = lua
            .create_string(vec![b't'; MAX_MESSAGE_TARGET_ID_BYTES + 1])
            .unwrap();
        let error = lua_message_target(too_long).unwrap_err();
        assert!(error.to_string().contains("256 bytes"));

        let invalid_utf8 = lua.create_string([0xff]).unwrap();
        let error = lua_message_target(invalid_utf8).unwrap_err();
        assert!(error.to_string().contains("valid UTF-8"));

        let target = "x".repeat(80);
        let error = queue_message_error(QueueMessageError::UnknownTarget, &target).to_string();
        assert!(error.contains(&format!("{}…", "x".repeat(64))));
        assert!(!error.contains(&"x".repeat(65)));
    }
}
