//! Save schema versions and the migration hook that runs before scripts see save data.

use crate::SaveData;
use crate::abi_host::{OwnedAbiParams, read_message_value_with_objects};
use crate::abi_string::read_abi_str;
use crate::lite::{
    AssetUserData, CallDeadline, LUA_MEMORY_LIMIT_BYTES, SaveObjectUserData, lua_param_value,
};
use crate::transaction::{ScriptObjectRef, validate_save_values};
use engine_scene::{ObjectHandle, ParamValue};
use mlua::chunk::ChunkMode;
use mlua::{Function, Lua, LuaOptions, StdLib, Table, Value};
use rve_sdk::abi::{
    RVE_SAVE_MIGRATION_ABI_VERSION, RveSaveMigrateFn, RveSaveMigration, RveStatus, RveStr, RveValue,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;

type SaveValues = BTreeMap<String, ParamValue>;

/// Game save schema: the version written to save files and the optional Lua migrator.
///
/// A Rust migrator is the `rve_save_migrate` export of the loaded script library.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SaveSchema {
    /// Version this game reads and writes; files without `version` are version 0.
    pub version: u32,
    /// Lua migrator from `[save] migration` in `project.rveproject`.
    pub lua_migration: Option<LuaSaveMigration>,
}

/// Lua chunk that returns a table with `migrate(from_version, values) -> values`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LuaSaveMigration {
    /// Virtual path used as the chunk name in errors.
    pub path: String,
    /// Chunk source.
    pub source: String,
}

/// Brings `data` to `schema.version`, one migration step per version.
pub(crate) fn migrate_save_data(
    data: SaveData,
    schema: &SaveSchema,
    rust_migration: Option<RveSaveMigrateFn>,
    deadline: &CallDeadline,
) -> Result<SaveValues, String> {
    let target = schema.version;
    if schema.lua_migration.is_some() && rust_migration.is_some() {
        return Err(
            "save migration is declared twice: [save] migration and the library's rve_save_migrate"
                .to_owned(),
        );
    }
    if data.version > target {
        return Err(format!(
            "save file version {} is newer than save version {target} supported by this game",
            data.version
        ));
    }
    if data.version == target {
        return Ok(data.values);
    }
    if let Some(migration) = &schema.lua_migration {
        migrate_with_lua(migration, data, target, deadline)
            .map_err(|error| format!("Lua save migration {}: {error}", migration.path))
    } else if let Some(migrate) = rust_migration {
        let mut values = data.values;
        for from in data.version..target {
            // SAFETY: the library that exported `migrate` stays loaded with the host.
            values = unsafe { migrate_with_rust(migrate, from, &values) }
                .and_then(|values| validate_save_values(&values).map(|()| values))
                .map_err(|error| {
                    format!(
                        "rve_save_migrate from version {from} to {}: {error}",
                        from + 1
                    )
                })?;
        }
        Ok(values)
    } else {
        Err(format!(
            "save file version {} is older than save version {target}, but the game declares no save migration",
            data.version
        ))
    }
}

fn migrate_with_lua(
    migration: &LuaSaveMigration,
    data: SaveData,
    target: u32,
    deadline: &CallDeadline,
) -> Result<SaveValues, String> {
    let lua = Lua::new_with(
        StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    let setup = || -> mlua::Result<Function> {
        lua.set_memory_limit(LUA_MEMORY_LIMIT_BYTES)?;
        deadline.install_sparse_hook(&lua)?;
        let globals = lua.globals();
        for name in ["dofile", "loadfile", "require", "print"] {
            globals.set(name, Value::Nil)?;
        }
        let module = lua
            .load(migration.source.as_str())
            .set_name(migration.path.as_str())
            .set_mode(ChunkMode::Text)
            .eval::<Value>()?;
        let Value::Table(module) = module else {
            return Err(mlua::Error::runtime("chunk must return a table"));
        };
        module
            .get::<Function>("migrate")
            .map_err(|_| mlua::Error::runtime("migrate must be a function"))
    };
    // One deadline covers loading the chunk and every step, so a stuck migrator cannot hang
    // startup.
    let _deadline = deadline.enter(&lua);
    let migrate = setup().map_err(|error| error.to_string())?;
    let mut values = data.values;
    for from in data.version..target {
        let step = || -> mlua::Result<SaveValues> {
            let input = lua_values(&lua, &values)?;
            let Value::Table(output) = migrate.call::<Value>((from, input))? else {
                return Err(mlua::Error::runtime("migrate must return a values table"));
            };
            save_values_from_lua(output)
        };
        values = step()
            .map_err(|error| error.to_string())
            .and_then(|values| validate_save_values(&values).map(|()| values))
            .map_err(|error| format!("from version {from} to {}: {error}", from + 1))?;
    }
    Ok(values)
}

fn lua_values(lua: &Lua, values: &SaveValues) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (key, value) in values {
        table.set(key.as_str(), lua_value(lua, value)?)?;
    }
    Ok(table)
}

/// Lossless conversion: assets and objects stay userdata so they keep their type on return.
fn lua_value(lua: &Lua, value: &ParamValue) -> mlua::Result<Value> {
    Ok(match value {
        ParamValue::Bool(value) => Value::Boolean(*value),
        ParamValue::Int(value) => Value::Integer(*value),
        ParamValue::Float(value) => Value::Number(*value),
        ParamValue::String(value) => Value::String(lua.create_string(value)?),
        ParamValue::Asset(path) => {
            Value::UserData(lua.create_userdata(AssetUserData(path.clone()))?)
        }
        ParamValue::Object(id) => {
            Value::UserData(lua.create_userdata(SaveObjectUserData(id.clone()))?)
        }
        ParamValue::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.set(index + 1, lua_value(lua, value)?)?;
            }
            Value::Table(table)
        }
    })
}

fn save_values_from_lua(table: Table) -> mlua::Result<SaveValues> {
    let mut values = SaveValues::new();
    table.for_each::<Value, Value>(|key, value| {
        let Value::String(key) = key else {
            return Err(mlua::Error::runtime("save keys must be strings"));
        };
        let key = key
            .to_str()
            .map_err(|_| mlua::Error::runtime("save keys must be valid UTF-8"))?
            .to_owned();
        values.insert(key, lua_param_value(value)?);
        Ok(())
    })?;
    Ok(values)
}

struct RustMigrationOutput<'a> {
    object_ids: &'a [String],
    values: SaveValues,
    error: Option<String>,
}

/// # Safety
/// `migrate` must obey the `rve_save_migrate` contract.
unsafe fn migrate_with_rust(
    migrate: RveSaveMigrateFn,
    from: u32,
    values: &SaveValues,
) -> Result<SaveValues, String> {
    // Objects are passed as handles local to this call: handle `n` is the `n`th distinct id.
    let mut object_ids = Vec::new();
    for value in values.values() {
        collect_object_ids(value, &mut object_ids);
    }
    let references = object_ids
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let handle = ObjectHandle::from_abi(index as u64 + 1)?;
            Some((
                id.clone(),
                ScriptObjectRef {
                    handle,
                    name: id.clone(),
                },
            ))
        })
        .collect::<HashMap<_, _>>();
    let input = OwnedAbiParams::new(values, &references);
    let mut output = RustMigrationOutput {
        object_ids: &object_ids,
        values: SaveValues::new(),
        error: None,
    };
    let migration = RveSaveMigration {
        abi_version: RVE_SAVE_MIGRATION_ABI_VERSION,
        from_version: from,
        values: input.abi,
        ctx: (&mut output as *mut RustMigrationOutput).cast(),
        set: migration_set,
        error: migration_error,
    };
    // SAFETY: `input` and `output` outlive this synchronous call; see the function contract.
    let status = unsafe { migrate(&migration) };
    if status == RveStatus::Ok as u32 {
        return Ok(output.values);
    }
    Err(output.error.unwrap_or_else(|| {
        if status == RveStatus::Panicked as u32 {
            "the migration panicked".to_owned()
        } else {
            format!("the migration failed with status {status}")
        }
    }))
}

fn collect_object_ids(value: &ParamValue, ids: &mut Vec<String>) {
    match value {
        ParamValue::Object(id) if !id.is_empty() && !ids.contains(id) => ids.push(id.clone()),
        ParamValue::Array(values) => {
            for value in values {
                collect_object_ids(value, ids);
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn migration_set(ctx: *mut c_void, key: RveStr, value: *const RveValue) -> bool {
    // SAFETY: `ctx` is the `RustMigrationOutput` of the active `migrate_with_rust` call.
    let output = unsafe { &mut *ctx.cast::<RustMigrationOutput>() };
    // SAFETY: the guest keeps the key readable during this call.
    let key = match unsafe { read_abi_str(key) } {
        Ok(key) => key.to_owned(),
        Err(error) => {
            output.error = Some(format!("save key is invalid: {error:?}"));
            return false;
        }
    };
    let object_ids = output.object_ids;
    // SAFETY: the guest keeps the value tree readable during this call.
    let decoded = unsafe {
        read_message_value_with_objects(value, |handle| match handle {
            0 => Some(String::new()),
            handle => usize::try_from(handle - 1)
                .ok()
                .and_then(|index| object_ids.get(index))
                .cloned(),
        })
    };
    match decoded {
        Ok((value, _)) => {
            output.values.insert(key, value);
            true
        }
        Err(error) => {
            output.error = Some(format!("save value {key:?} is invalid: {error}"));
            false
        }
    }
}

unsafe extern "C" fn migration_error(ctx: *mut c_void, message: RveStr) {
    // SAFETY: `ctx` is the `RustMigrationOutput` of the active `migrate_with_rust` call.
    let output = unsafe { &mut *ctx.cast::<RustMigrationOutput>() };
    // SAFETY: the guest keeps the message readable during this call.
    output.error = Some(
        unsafe { read_abi_str(message) }
            .map(str::to_owned)
            .unwrap_or_else(|_| "the migration reported an invalid error message".to_owned()),
    );
}

#[cfg(test)]
#[path = "save_migration_tests.rs"]
mod tests;
