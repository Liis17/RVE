use crate::types::{ScriptEvent, ScriptEventLevel, ScriptParams, ScriptTime, ScriptTransform};
use engine_scene::ParamValue;
use mlua::chunk::ChunkMode;
use mlua::{
    Error as LuaError, Function, HookTriggers, Lua, LuaOptions, LuaString, MultiValue, StdLib,
    Table, UserData, UserDataFields, Value, VmState,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const LUA_HOOK_INSTRUCTION_INTERVAL: u32 = 1_000;
const LUA_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

pub(crate) type EventQueue = Rc<RefCell<Vec<ScriptEvent>>>;
pub(crate) type ActiveSource = Rc<RefCell<String>>;

pub(crate) struct LuaState {
    pub(crate) lua: Lua,
    pub(crate) base_environment: Table,
    pub(crate) time: Table,
}

pub(crate) fn create_lua(
    events: EventQueue,
    active_source: ActiveSource,
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
    )?;
    globals.set("print", print)?;
    globals.set("collectgarbage", restricted_gc_function(&lua)?)?;

    let time = lua.create_table()?;
    time.set("elapsed", 0.0)?;
    time.set("frame", 0_u64)?;
    let rve = lua.create_table()?;
    rve.set("time", time.clone())?;
    rve.set("log", log_function(&lua, tostring, events, active_source)?)?;
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
    for callback in ["on_start", "on_update", "on_destroy"] {
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
) -> mlua::Result<Function> {
    lua.create_function(move |_, arguments: MultiValue| {
        let mut parts = Vec::with_capacity(arguments.len());
        for argument in arguments {
            let string = tostring.call::<LuaString>(argument)?;
            parts.push(string.to_string_lossy());
        }
        events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Info,
            source: active_source.borrow().clone(),
            message: parts.join(" "),
        });
        Ok(())
    })
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
