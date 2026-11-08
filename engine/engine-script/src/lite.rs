use crate::actions::{ActionMutation, ActionValue};
use crate::lifecycle::{ObjectState, PrefabRequest, SpawnArgs};
use crate::transaction::{
    MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_TARGET_ID_BYTES, MessageValueBudget, QueueMessageError,
    ScriptTransaction,
};
use crate::types::{
    RumbleRequest, ScriptEvent, ScriptEventLevel, ScriptInput, ScriptParams, ScriptTime,
    ScriptTransform,
};
use engine_console::CommandSpec;
use engine_files::VirtualPath;
use engine_physics::Ray;
use engine_project::InputActionKind;
use engine_scene::{
    AnimationRequest, AntiAliasing, AudioGroup, AudioRequest, CameraSettings, ClipParams,
    ObjectHandle, ParamKind, ParamSchema, ParamSpec, ParamValue, Tonemapper, VoiceId,
};
use glam::Vec3;
use mlua::chunk::ChunkMode;
use mlua::{
    Error as LuaError, Function, HookTriggers, Lua, LuaOptions, LuaString, MultiValue, RegistryKey,
    StdLib, Table, UserData, UserDataFields, Value, VmState,
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
        }
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

    fn timeout_message(&self) -> String {
        format!(
            "Lua call exceeded time limit of {:?}",
            self.call_time_limit.unwrap_or_default()
        )
    }
}

impl Drop for ActiveCallDeadline<'_> {
    fn drop(&mut self) {
        if !self.owns_deadline {
            return;
        }
        let expired = self.deadline.expired.get();
        self.deadline.deadline.set(None);
        self.deadline.expired.set(false);
        if expired && !self.deadline.interrupted.load(Ordering::Acquire) {
            let _ = self.deadline.install_sparse_hook(self.lua);
        }
    }
}

pub(crate) fn create_lua(
    events: EventQueue,
    active_source: ActiveSource,
    active_call: ActiveCall,
    active_owner: ActiveOwner,
    commands: LuaCommands,
    commands_generation: CommandGeneration,
    call_deadline: CallDeadline,
) -> mlua::Result<LuaState> {
    let lua = Lua::new_with(
        // mlua always opens the Lua base library; these flags select the remaining libraries.
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(LUA_MEMORY_LIMIT_BYTES)?;
    call_deadline.install_sparse_hook(&lua)?;

    create_object_handles(&lua)?;
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

    let time = lua.create_table()?;
    time.set("elapsed", 0.0)?;
    time.set("frame", 0_u64)?;
    let rve = lua.create_table()?;
    rve.set("time", time.clone())?;
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
    let physics = lua.create_table()?;
    physics.set(
        "raycast",
        physics_raycast_function(&lua, active_call.clone())?,
    )?;
    rve.set("physics", physics)?;
    rve.set(
        "active_camera",
        active_camera_function(&lua, active_call.clone())?,
    )?;
    rve.set("spawn", spawn_function(&lua, active_call.clone())?)?;
    rve.set("destroy", destroy_function(&lua, active_call.clone())?)?;
    rve.set("prefabs", prefabs_table(&lua, active_call.clone())?)?;
    rve.set("send", send_function(&lua, active_call.clone())?)?;
    rve.set("broadcast", broadcast_function(&lua, active_call.clone())?)?;
    rve.set("audio", audio_table(&lua, active_call.clone())?)?;
    rve.set("input", input_table(&lua, active_call)?)?;
    globals.set("rve", rve)?;
    Ok(LuaState {
        lua,
        base_environment: globals,
        time,
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
        "on_update",
        "on_message",
        "on_destroy",
        "on_collision_enter",
        "on_collision_exit",
        "on_trigger_enter",
        "on_trigger_exit",
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
                    b"name" | b"type" | b"default" | b"min" | b"max" | b"label"
                ) =>
            {
                Ok(())
            }
            _ => Err(LuaError::runtime(
                "unknown field; expected type, default, min, max or label",
            )),
        })
        .map_err(|error| context(&error.to_string()))?;
    let kind = match entry.get::<Value>("type") {
        Ok(Value::String(kind)) => kind
            .to_str()
            .ok()
            .and_then(|kind| ParamKind::parse(&kind))
            .ok_or_else(|| context("type must be bool, int, float, string or vec3"))?,
        _ => return Err(context("type must be a string")),
    };
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
    )
    .map_err(|error| error.to_string())?;
    compile_class(&state.lua, &state.base_environment, label, source).map(|(_, schema)| schema)
}

pub(crate) struct LuaInstance {
    class: Table,
    instance: Table,
    transform: mlua::AnyUserData,
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
        object_id: &str,
        object_name: &str,
        params: &ScriptParams,
        transform: ScriptTransform,
    ) -> mlua::Result<Self> {
        let params = params_table(lua, params)?;
        let object = lua.create_table()?;
        object.set("id", object_id)?;
        object.set("name", object_name)?;
        let transform = lua.create_userdata(TransformUserData { value: transform })?;

        let instance = lua.create_table()?;
        instance.set("params", params)?;
        instance.set("object", object)?;
        instance.set("transform", transform.clone())?;
        let metatable = lua.create_table()?;
        metatable.set("__index", class.clone())?;
        instance.set_metatable(Some(metatable))?;

        Ok(Self {
            class,
            instance,
            transform,
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
                param_value(lua, value)?,
            ),
        )
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

pub(crate) fn set_time(time_table: &Table, time: ScriptTime) -> mlua::Result<()> {
    time_table.raw_set("elapsed", time.elapsed)?;
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

fn with_active_call<T>(
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
fn object_handle_of(lua: &Lua, object: &Table) -> mlua::Result<Option<ObjectHandle>> {
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
                Ok(transaction.camera_settings(handle))
            })?;
            match settings {
                Some(settings) => Ok(Value::Table(camera_settings_table(lua, settings)?)),
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
    add_audio_object_api(lua, &object, handle, active_call.clone())?;
    add_animation_object_api(lua, &object, handle, active_call.clone())?;
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

    let play_active = active_call.clone();
    audio.set(
        "play",
        lua.create_function(move |_, (path, options): (String, Option<Table>)| {
            let path = VirtualPath::parse(&path)
                .map_err(|error| LuaError::runtime(format!("rve.audio.play: {error}")))?;
            let mut params = ClipParams::default();
            if let Some(options) = options {
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
    Ok(audio)
}

fn add_physics_object_api(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
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

fn camera_settings_table(lua: &Lua, settings: CameraSettings) -> mlua::Result<Table> {
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
    color.set("exposure_ev", settings.color.exposure_ev as f64)?;
    color.set(
        "tonemapper",
        match settings.color.tonemapper {
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
        set_optional_number(&color, "exposure_ev", &mut settings.color.exposure_ev)?;
        if let Value::String(value) = color.get::<Value>("tonemapper")? {
            settings.color.tonemapper = match value.to_str()?.as_ref() {
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
        move |_, (target, name, value): (LuaString, LuaString, Value)| {
            let target = lua_message_target(target)?;
            let name = lua_message_name(name)?;
            let (value, value_size) = lua_param_value_with_size(value)?;
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
    lua.create_function(move |_, (name, value): (LuaString, Value)| {
        let name = lua_message_name(name)?;
        let (value, value_size) = lua_param_value_with_size(value)?;
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
fn lua_boolean(value: Value, function: &str) -> mlua::Result<bool> {
    match value {
        Value::Boolean(value) => Ok(value),
        other => Err(LuaError::runtime(format!(
            "{function} expects a boolean, got {}",
            other.type_name()
        ))),
    }
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
    let value = lua_param_value_inner(value, 0, &mut budget)?;
    Ok((value, budget.estimated_size()))
}

fn lua_param_value_inner(
    value: Value,
    depth: usize,
    budget: &mut MessageValueBudget,
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
        Value::Table(table) => {
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
                .map(|(_, value)| lua_param_value_inner(value, depth + 1, budget))
                .collect::<mlua::Result<Vec<_>>>()
                .map(ParamValue::Array)
        }
        _ => Err(LuaError::runtime(
            "message values must be booleans, integers, finite numbers, UTF-8 strings, or sequence tables",
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

fn params_table(lua: &Lua, params: &ScriptParams) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (name, value) in params {
        table.set(name.as_str(), param_value(lua, value)?)?;
    }
    Ok(table)
}

fn param_value(lua: &Lua, value: &ParamValue) -> mlua::Result<Value> {
    match value {
        ParamValue::Bool(value) => Ok(Value::Boolean(*value)),
        ParamValue::Int(value) => Ok(Value::Integer(*value)),
        ParamValue::Float(value) => Ok(Value::Number(*value)),
        ParamValue::String(value) => Ok(Value::String(lua.create_string(value)?)),
        ParamValue::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.set(index + 1, param_value(lua, value)?)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::{
        MAX_MESSAGE_DEPTH, MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_STRING_BYTES,
        MAX_MESSAGE_TARGET_ID_BYTES,
    };

    fn convert(source: &str) -> mlua::Result<ParamValue> {
        let lua = Lua::new();
        lua_param_value(lua.load(source).eval::<Value>()?)
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
