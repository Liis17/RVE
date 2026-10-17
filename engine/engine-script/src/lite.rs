use crate::transaction::{
    MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_TARGET_ID_BYTES, MessageValueBudget, QueueMessageError,
    ScriptTransaction,
};
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptParams, ScriptTime, ScriptTransform};
use engine_scene::{ObjectHandle, ParamValue};
use mlua::chunk::ChunkMode;
use mlua::{
    Error as LuaError, Function, HookTriggers, Lua, LuaOptions, LuaString, MultiValue, StdLib,
    Table, UserData, UserDataFields, Value, VmState,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const LUA_HOOK_INSTRUCTION_INTERVAL: u32 = 1_000;
const LUA_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

pub(crate) type EventQueue = Rc<RefCell<Vec<ScriptEvent>>>;
pub(crate) type ActiveSource = Rc<RefCell<String>>;
pub(crate) type ActiveCall = Rc<Cell<*mut ScriptTransaction>>;

pub(crate) struct LuaState {
    pub(crate) lua: Lua,
    pub(crate) base_environment: Table,
    pub(crate) time: Table,
}

pub(crate) fn create_lua(
    events: EventQueue,
    active_source: ActiveSource,
    active_call: ActiveCall,
    interrupted: Arc<AtomicBool>,
) -> mlua::Result<LuaState> {
    let lua = Lua::new_with(
        // mlua always opens the Lua base library; these flags select the remaining libraries.
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(LUA_MEMORY_LIMIT_BYTES)?;
    let dense_hook_enabled = Arc::new(AtomicBool::new(false));
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(LUA_HOOK_INSTRUCTION_INTERVAL),
        {
            let interrupted = interrupted.clone();
            let dense_hook_enabled = dense_hook_enabled.clone();
            move |lua, _| {
                if !interrupted.load(Ordering::Acquire) {
                    return Ok(VmState::Continue);
                }
                if !dense_hook_enabled.load(Ordering::Acquire) {
                    let interrupted = interrupted.clone();
                    lua.set_hook(HookTriggers::new().every_nth_instruction(1), move |_, _| {
                        if interrupted.load(Ordering::Acquire) {
                            Err(LuaError::runtime("Lua execution interrupted"))
                        } else {
                            Ok(VmState::Continue)
                        }
                    })?;
                    dense_hook_enabled.store(true, Ordering::Release);
                }
                Err(LuaError::runtime("Lua execution interrupted"))
            }
        },
    )?;

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
            tostring,
            events,
            active_source,
            ScriptEventLevel::Error,
        )?,
    )?;
    rve.set("find", find_function(&lua, active_call.clone())?)?;
    rve.set("send", send_function(&lua, active_call.clone())?)?;
    rve.set("broadcast", broadcast_function(&lua, active_call)?)?;
    globals.set("rve", rve)?;
    Ok(LuaState {
        lua,
        base_environment: globals,
        time,
    })
}

pub(crate) fn compile_class(
    lua: &Lua,
    base_environment: &Table,
    path: &str,
    source: &str,
) -> Result<Table, String> {
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
    for callback in ["on_start", "on_update", "on_message", "on_destroy"] {
        match class
            .get::<Value>(callback)
            .map_err(|error| error.to_string())?
        {
            Value::Nil | Value::Function(_) => {}
            _ => return Err(format!("class field {callback:?} must be a function")),
        }
    }
    Ok(class)
}

pub(crate) struct LuaInstance {
    class: Table,
    instance: Table,
    transform: mlua::AnyUserData,
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

    pub(crate) fn has_message_callback(&self) -> bool {
        !matches!(self.class.get::<Value>("on_message"), Ok(Value::Nil))
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
        let mut parts = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let string = tostring.call::<LuaString>(argument)?;
            parts.push(string.to_string_lossy());
        }
        events.borrow_mut().push(ScriptEvent {
            level,
            source: active_source.borrow().clone(),
            message: parts.join(" "),
        });
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
            id,
            name,
            active_call.clone(),
        )?))
    })
}

fn object_table(
    lua: &Lua,
    handle: ObjectHandle,
    id: String,
    name: String,
    active_call: ActiveCall,
) -> mlua::Result<Table> {
    let object = lua.create_table()?;
    object.set("id", id)?;
    object.set("name", name)?;

    let get_active = active_call.clone();
    object.set(
        "get_transform",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let transform = with_active_call(&get_active, |transaction| {
                Ok(transaction.local_transform(handle))
            })?;
            transform
                .map(|value| lua.create_userdata(TransformUserData { value }))
                .transpose()
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
            let (writable, accepted) = with_active_call(&set_active, |transaction| {
                Ok((
                    transaction.writable(),
                    transaction.set_local_transform(handle, transform),
                ))
            })?;
            if accepted {
                Ok(())
            } else if writable {
                Err(LuaError::runtime(
                    "object.set_transform target object no longer exists",
                ))
            } else {
                Err(LuaError::runtime(
                    "object.set_transform is unavailable during script creation",
                ))
            }
        })?,
    )?;

    let matrix_active = active_call;
    object.set(
        "world_matrix",
        lua.create_function(move |lua, _arguments: MultiValue| {
            let matrix = with_active_call(&matrix_active, |transaction| {
                Ok(transaction.world_matrix(handle))
            })?;
            matrix.map(|matrix| matrix_table(lua, matrix)).transpose()
        })?,
    )?;
    Ok(object)
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

#[cfg(test)]
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
