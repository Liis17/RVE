//! Lua API `rve.fs`, `rve.window` и `rve.dialog`.
//!
//! `rve.fs` работает с диском сразу и без ограничений путей, как `io` стандартного Lua: ошибки
//! ввода-вывода возвращаются вторым значением (`nil, message`), а не поднимаются, и отказ вызова
//! не откатывает уже записанные файлы. Относительные пути отсчитываются от рабочего каталога
//! процесса. `rve.window` и `rve.dialog` ставят запросы в транзакцию вызова, их выполняет
//! вызывающий код после кадра.

use crate::lite::{ActiveCall, lua_boolean, lua_integer, with_active_call};
use crate::platform::{FileDialogFilter, FileDialogKind, FileDialogRequest};
use mlua::{Error as LuaError, Lua, Table, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

/// Таблица `rve.fs`.
pub(crate) fn fs_table(lua: &Lua) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "read",
        lua.create_function(|lua, path: Value| {
            let path = lua_path(path, "rve.fs.read")?;
            io_result(lua, fs::read(&path), |lua, bytes| {
                Ok(Value::String(lua.create_string(bytes)?))
            })
        })?,
    )?;
    table.set(
        "write",
        lua.create_function(|lua, (path, data): (Value, Value)| {
            let path = lua_path(path, "rve.fs.write")?;
            let data = lua_bytes(data, "rve.fs.write")?;
            io_result(lua, fs::write(&path, data), |_, ()| {
                Ok(Value::Boolean(true))
            })
        })?,
    )?;
    table.set(
        "append",
        lua.create_function(|lua, (path, data): (Value, Value)| {
            let path = lua_path(path, "rve.fs.append")?;
            let data = lua_bytes(data, "rve.fs.append")?;
            let written = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(&data));
            io_result(lua, written, |_, ()| Ok(Value::Boolean(true)))
        })?,
    )?;
    table.set(
        "exists",
        lua.create_function(|_, path: Value| Ok(lua_path(path, "rve.fs.exists")?.exists()))?,
    )?;
    table.set(
        "is_dir",
        lua.create_function(|_, path: Value| Ok(lua_path(path, "rve.fs.is_dir")?.is_dir()))?,
    )?;
    table.set(
        "list",
        lua.create_function(|lua, path: Value| {
            let path = lua_path(path, "rve.fs.list")?;
            io_result(lua, list_directory(&path), |lua, names| {
                Ok(Value::Table(lua.create_sequence_from(names)?))
            })
        })?,
    )?;
    table.set(
        "create_dir",
        lua.create_function(|lua, path: Value| {
            let path = lua_path(path, "rve.fs.create_dir")?;
            io_result(lua, fs::create_dir_all(&path), |_, ()| {
                Ok(Value::Boolean(true))
            })
        })?,
    )?;
    Ok(table)
}

/// Таблица `rve.window`.
pub(crate) fn window_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let read_call = active_call.clone();
    table.set(
        "fullscreen",
        lua.create_function(move |_, ()| {
            with_active_call(
                &read_call,
                |transaction| Ok(transaction.window_fullscreen()),
            )
        })?,
    )?;
    let set_call = active_call.clone();
    table.set(
        "set_fullscreen",
        lua.create_function(move |_, value: Value| {
            let fullscreen = lua_boolean(value, "rve.window.set_fullscreen")?;
            with_active_call(&set_call, |transaction| {
                transaction
                    .request_fullscreen(fullscreen)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    table.set(
        "screenshot",
        lua.create_function(move |_, path: Option<Value>| {
            let path = match path {
                None | Some(Value::Nil) => None,
                Some(path) => Some(lua_path(path, "rve.window.screenshot")?),
            };
            with_active_call(&active_call, |transaction| {
                transaction
                    .request_screenshot(path)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    Ok(table)
}

/// Таблица `rve.dialog`.
pub(crate) fn dialog_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (name, kind) in [
        ("open_file", FileDialogKind::OpenFile),
        ("open_files", FileDialogKind::OpenFiles),
        ("save_file", FileDialogKind::SaveFile),
        ("pick_folder", FileDialogKind::PickFolder),
    ] {
        let call = active_call.clone();
        let function = format!("rve.dialog.{name}");
        table.set(
            name,
            lua.create_function(move |_, options: Option<Value>| {
                let request = dialog_request(kind, options, &function)?;
                with_active_call(&call, |transaction| {
                    transaction
                        .open_file_dialog(request)
                        .map(|id| id as i64)
                        .map_err(LuaError::runtime)
                })
            })?,
        )?;
    }
    table.set(
        "poll",
        lua.create_function(move |lua, id: Value| {
            let id = lua_integer(id, "rve.dialog.poll id")?;
            let id = u64::try_from(id)
                .map_err(|_| LuaError::runtime(format!("unknown file dialog id {id}")))?;
            let result = with_active_call(&active_call, |transaction| {
                transaction.poll_file_dialog(id).map_err(LuaError::runtime)
            })?;
            match result {
                None => Ok(Value::Nil),
                Some(paths) => Ok(Value::Table(lua.create_sequence_from(
                    paths.iter().map(|path| path.to_string_lossy().into_owned()),
                )?)),
            }
        })?,
    )?;
    Ok(table)
}

fn dialog_request(
    kind: FileDialogKind,
    options: Option<Value>,
    function: &str,
) -> mlua::Result<FileDialogRequest> {
    let mut request = FileDialogRequest {
        kind,
        title: None,
        directory: None,
        file_name: None,
        filters: Vec::new(),
    };
    let options = match options {
        None | Some(Value::Nil) => return Ok(request),
        Some(Value::Table(options)) => options,
        Some(other) => {
            return Err(LuaError::runtime(format!(
                "{function} options must be a table, got {}",
                other.type_name()
            )));
        }
    };
    request.title = optional_text(&options, "title", function)?;
    request.directory = optional_text(&options, "directory", function)?.map(PathBuf::from);
    request.file_name = optional_text(&options, "file_name", function)?;
    match options.get::<Value>("filters")? {
        Value::Nil => {}
        Value::Table(filters) => {
            for filter in filters.sequence_values::<Table>() {
                let filter = filter
                    .map_err(|_| LuaError::runtime(format!("{function} filters must be tables")))?;
                let name = text(filter.get("name")?, &format!("{function} filter name"))?;
                let extensions = match filter.get::<Value>("extensions")? {
                    Value::Table(extensions) => extensions
                        .sequence_values::<Value>()
                        .map(|extension| text(extension?, &format!("{function} filter extension")))
                        .collect::<mlua::Result<Vec<_>>>()?,
                    other => {
                        return Err(LuaError::runtime(format!(
                            "{function} filter extensions must be a table, got {}",
                            other.type_name()
                        )));
                    }
                };
                request.filters.push(FileDialogFilter { name, extensions });
            }
        }
        other => {
            return Err(LuaError::runtime(format!(
                "{function} filters must be a table, got {}",
                other.type_name()
            )));
        }
    }
    Ok(request)
}

fn optional_text(options: &Table, field: &str, function: &str) -> mlua::Result<Option<String>> {
    match options.get::<Value>(field)? {
        Value::Nil => Ok(None),
        value => text(value, &format!("{function} {field}")).map(Some),
    }
}

fn text(value: Value, what: &str) -> mlua::Result<String> {
    match value {
        Value::String(text) => text
            .to_str()
            .map(|text| text.to_owned())
            .map_err(|_| LuaError::runtime(format!("{what} must be valid UTF-8 text"))),
        other => Err(LuaError::runtime(format!(
            "{what} must be a string, got {}",
            other.type_name()
        ))),
    }
}

fn lua_path(value: Value, function: &str) -> mlua::Result<PathBuf> {
    let path = text(value, &format!("{function} path"))?;
    if path.is_empty() {
        return Err(LuaError::runtime(format!(
            "{function} path must not be empty"
        )));
    }
    Ok(PathBuf::from(path))
}

fn lua_bytes(value: Value, function: &str) -> mlua::Result<Vec<u8>> {
    match value {
        Value::String(data) => Ok(data.as_bytes().to_vec()),
        other => Err(LuaError::runtime(format!(
            "{function} data must be a string, got {}",
            other.type_name()
        ))),
    }
}

fn list_directory(path: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut names = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// Успех — одно значение, ошибка ввода-вывода — `nil, message`.
fn io_result<T>(
    lua: &Lua,
    result: std::io::Result<T>,
    success: impl FnOnce(&Lua, T) -> mlua::Result<Value>,
) -> mlua::Result<(Value, Value)> {
    match result {
        Ok(value) => Ok((success(lua, value)?, Value::Nil)),
        Err(error) => Ok((
            Value::Nil,
            Value::String(lua.create_string(error.to_string())?),
        )),
    }
}
