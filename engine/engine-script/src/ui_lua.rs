//! Lua API `rve.ui`: открытие и закрытие канвасов, поиск элементов, их свойства и событие `on_ui_event`.
//!
//! Канвас и элемент — таблицы с методами. Хендл канваса хранится в реестре Lua (слабые ключи), хендл
//! элемента замкнут в методах своей таблицы. Все операции идут через активную транзакцию вызова.

use crate::lite::{ActiveCall, lua_boolean, lua_integer, object_handle_of, with_active_call};
use crate::transaction::ScriptTransaction;
use crate::ui::UiDelivery;
use engine_files::VirtualPath;
use engine_gui::document::RectSpec;
use engine_gui::events::{UiEventKind, UiValue};
use engine_gui::system::{UiCanvasHandle, UiElementHandle};
use mlua::{Error as LuaError, Lua, Table, Value};

/// Ключ реестра Lua с таблицей «канвас → код хендла»; ключи слабые, как у объектов.
const CANVAS_HANDLES_KEY: &str = "rve.ui_canvas_handles";

pub(crate) fn create_registry(lua: &Lua) -> mlua::Result<()> {
    let handles = lua.create_table()?;
    let mode = lua.create_table()?;
    mode.set("__mode", "k")?;
    handles.set_metatable(Some(mode))?;
    lua.set_named_registry_value(CANVAS_HANDLES_KEY, handles)
}

/// Код хендла канваса: индекс слота (+1) и поколение в старших 32 битах, как у объектов.
fn canvas_code(canvas: UiCanvasHandle) -> i64 {
    ((u64::from(canvas.generation()) << 32) | (u64::from(canvas.index()) + 1)) as i64
}

fn canvas_from_code(code: i64) -> Option<UiCanvasHandle> {
    let value = code as u64;
    let slot = (value & u64::from(u32::MAX)) as u32;
    (slot != 0).then(|| UiCanvasHandle::from_raw(slot - 1, (value >> 32) as u32))
}

fn canvas_of(lua: &Lua, value: &Value) -> mlua::Result<Option<UiCanvasHandle>> {
    let Value::Table(table) = value else {
        return Ok(None);
    };
    let handles: Table = lua.named_registry_value(CANVAS_HANDLES_KEY)?;
    Ok(match handles.raw_get::<Value>(table.clone())? {
        Value::Integer(code) => canvas_from_code(code),
        _ => None,
    })
}

pub(crate) fn ui_table(lua: &Lua, active_call: ActiveCall) -> mlua::Result<Table> {
    let ui = lua.create_table()?;

    let open_active = active_call.clone();
    ui.set(
        "open",
        lua.create_function(move |lua, path: String| {
            let path = VirtualPath::parse(&path)
                .map_err(|error| LuaError::runtime(format!("rve.ui.open: {error}")))?;
            let canvas = with_active_call(&open_active, |transaction| {
                transaction
                    .ui_open(path.as_str())
                    .map_err(LuaError::runtime)
            })?;
            canvas_table(lua, canvas, open_active.clone())
        })?,
    )?;

    let close_active = active_call.clone();
    ui.set(
        "close",
        lua.create_function(move |lua, canvas: Value| {
            let canvas = canvas_of(lua, &canvas)?
                .ok_or_else(|| LuaError::runtime("rve.ui.close ожидает канвас из rve.ui"))?;
            with_active_call(&close_active, |transaction| {
                transaction.ui_close(canvas).map_err(LuaError::runtime)
            })
        })?,
    )?;

    let canvas_active = active_call.clone();
    ui.set(
        "canvas",
        lua.create_function(move |lua, object: Table| {
            let Some(handle) = object_handle_of(lua, &object)? else {
                return Ok(Value::Nil);
            };
            let canvas = with_active_call(&canvas_active, |transaction| {
                Ok(transaction.ui_component_canvas(handle))
            })?;
            match canvas {
                Some(canvas) => Ok(Value::Table(canvas_table(
                    lua,
                    canvas,
                    canvas_active.clone(),
                )?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let focused_active = active_call.clone();
    ui.set(
        "focused",
        lua.create_function(move |lua, ()| {
            let element =
                with_active_call(&focused_active, |transaction| Ok(transaction.ui_focused()))?;
            match element {
                Some(element) => Ok(Value::Table(element_table(
                    lua,
                    element,
                    focused_active.clone(),
                )?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let pointer_active = active_call;
    ui.set(
        "pointer_over",
        lua.create_function(move |_, ()| {
            with_active_call(&pointer_active, |transaction| {
                Ok(transaction.ui_pointer_over())
            })
        })?,
    )?;
    Ok(ui)
}

fn canvas_table(lua: &Lua, canvas: UiCanvasHandle, active_call: ActiveCall) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let handles: Table = lua.named_registry_value(CANVAS_HANDLES_KEY)?;
    handles.raw_set(table.clone(), canvas_code(canvas))?;

    let find_active = active_call.clone();
    table.set(
        "find",
        lua.create_function(move |lua, (_self, id): (Value, String)| {
            let element = with_active_call(&find_active, |transaction| {
                transaction.ui_find(canvas, &id).map_err(LuaError::runtime)
            })?;
            match element {
                Some(element) => Ok(Value::Table(element_table(
                    lua,
                    element,
                    find_active.clone(),
                )?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let visible_active = active_call.clone();
    table.set(
        "visible",
        lua.create_function(move |_, _self: Value| {
            with_active_call(&visible_active, |transaction| {
                transaction
                    .ui_canvas_visible(canvas)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let set_visible_active = active_call.clone();
    table.set(
        "set_visible",
        lua.create_function(move |_, (_self, visible): (Value, Value)| {
            let visible = lua_boolean(visible, "canvas:set_visible")?;
            with_active_call(&set_visible_active, |transaction| {
                transaction
                    .ui_set_canvas_visible(canvas, visible)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let layer_active = active_call.clone();
    table.set(
        "layer",
        lua.create_function(move |_, _self: Value| {
            with_active_call(&layer_active, |transaction| {
                transaction
                    .ui_canvas_layer(canvas)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    table.set(
        "set_layer",
        lua.create_function(move |_, (_self, layer): (Value, Value)| {
            let layer = lua_integer(layer, "canvas:set_layer")?;
            let layer = i32::try_from(layer)
                .map_err(|_| LuaError::runtime("canvas:set_layer: слой вне диапазона i32"))?;
            with_active_call(&active_call, |transaction| {
                transaction
                    .ui_set_canvas_layer(canvas, layer)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;
    Ok(table)
}

/// Таблица элемента: `id` и методы, которые работают с этим элементом через активную транзакцию.
fn element_table(
    lua: &Lua,
    element: UiElementHandle,
    active_call: ActiveCall,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    let id = with_active_call(&active_call, |transaction| {
        transaction
            .ui_element_id(element)
            .map_err(LuaError::runtime)
    })?;
    table.set("id", id)?;

    method(
        lua,
        &active_call,
        &table,
        "get_text",
        element,
        |_, tx, element, _| tx.ui_text(element).map_err(LuaError::runtime),
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_text",
        element,
        |_, tx, element, arg| {
            let text = lua_string(arg, "element:set_text")?;
            tx.ui_set_text(element, &text).map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "get_value",
        element,
        |lua, tx, element, _| {
            let value = tx.ui_value(element).map_err(LuaError::runtime)?;
            ui_value_to_lua(lua, value)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_value",
        element,
        |_, tx, element, arg| {
            let current = tx.ui_value(element).map_err(LuaError::runtime)?;
            let value = lua_ui_value(current, arg)?;
            tx.ui_set_value(element, value).map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "get_visible",
        element,
        |_, tx, element, _| tx.ui_visible(element).map_err(LuaError::runtime),
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_visible",
        element,
        |_, tx, element, arg| {
            let visible = lua_boolean(arg, "element:set_visible")?;
            tx.ui_set_visible(element, visible)
                .map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "get_interactable",
        element,
        |_, tx, element, _| tx.ui_interactable(element).map_err(LuaError::runtime),
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_interactable",
        element,
        |_, tx, element, arg| {
            let interactable = lua_boolean(arg, "element:set_interactable")?;
            tx.ui_set_interactable(element, interactable)
                .map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "get_color",
        element,
        |lua, tx, element, _| {
            let color = tx.ui_color(element).map_err(LuaError::runtime)?;
            color_table(lua, color)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_color",
        element,
        |_, tx, element, arg| {
            let color = lua_color(arg)?;
            tx.ui_set_color(element, color).map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_image",
        element,
        |_, tx, element, arg| {
            let path = lua_string(arg, "element:set_image")?;
            tx.ui_set_image(element, &path).map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "get_rect",
        element,
        |lua, tx, element, _| {
            let rect = tx.ui_rect(element).map_err(LuaError::runtime)?;
            rect_table(lua, rect)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_rect",
        element,
        |_, tx, element, arg| {
            let Value::Table(fields) = arg else {
                return Err(LuaError::runtime("element:set_rect ожидает таблицу"));
            };
            let mut rect = tx.ui_rect(element).map_err(LuaError::runtime)?;
            apply_rect_fields(&mut rect, &fields)?;
            tx.ui_set_rect(element, rect).map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "set_options",
        element,
        |_, tx, element, arg| {
            let options = lua_strings(arg, "element:set_options")?;
            tx.ui_set_options(element, options)
                .map_err(LuaError::runtime)
        },
    )?;
    method(
        lua,
        &active_call,
        &table,
        "focus",
        element,
        |_, tx, element, _| tx.ui_focus(element).map_err(LuaError::runtime),
    )?;
    Ok(table)
}

/// Регистрирует метод элемента. Первый аргумент Lua — сам элемент (синтаксис `:`), второй — значение.
fn method<R: mlua::IntoLuaMulti + 'static>(
    lua: &Lua,
    active_call: &ActiveCall,
    table: &Table,
    name: &str,
    element: UiElementHandle,
    body: fn(&Lua, &mut ScriptTransaction, UiElementHandle, Value) -> mlua::Result<R>,
) -> mlua::Result<()> {
    let active_call = active_call.clone();
    table.set(
        name,
        lua.create_function(move |lua, (_self, arg): (Value, Value)| {
            with_active_call(&active_call, |transaction| {
                body(lua, transaction, element, arg)
            })
        })?,
    )
}

/// Событие `on_ui_event`: канвас, элемент, его id, вид и значение (`nil`, если значения нет).
pub(crate) fn event_table(
    lua: &Lua,
    event: &UiDelivery,
    active_call: ActiveCall,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "canvas",
        canvas_table(lua, event.canvas, active_call.clone())?,
    )?;
    table.set("element", element_table(lua, event.element, active_call)?)?;
    table.set("id", event.element_id.as_str())?;
    table.set("kind", kind_name(event.kind))?;
    table.set("value", ui_value_to_lua(lua, event.value.clone())?)?;
    Ok(table)
}

fn kind_name(kind: UiEventKind) -> &'static str {
    match kind {
        UiEventKind::Click => "click",
        UiEventKind::ValueChanged => "value_changed",
        UiEventKind::Submit => "submit",
        UiEventKind::Cancel => "cancel",
        UiEventKind::Focus => "focus",
        UiEventKind::Blur => "blur",
        UiEventKind::HoverEnter => "hover_enter",
        UiEventKind::HoverExit => "hover_exit",
    }
}

/// Значение элемента в Lua: число, boolean, индекс выпадающего списка (с единицы) или строка.
fn ui_value_to_lua(lua: &Lua, value: Option<UiValue>) -> mlua::Result<Value> {
    Ok(match value {
        None => Value::Nil,
        Some(UiValue::Number(number)) => Value::Number(f64::from(number)),
        Some(UiValue::Bool(flag)) => Value::Boolean(flag),
        Some(UiValue::Index(index)) => Value::Integer(index as i64 + 1),
        Some(UiValue::Text(text)) => Value::String(lua.create_string(&text)?),
    })
}

/// Значение для записи: тип определяется текущим значением элемента. У dropdown без выбора текущего
/// значения принимается индекс.
fn lua_ui_value(current: Option<UiValue>, value: Value) -> mlua::Result<UiValue> {
    match current {
        Some(UiValue::Number(_)) => Ok(UiValue::Number(lua_finite(&value, "element:set_value")?)),
        Some(UiValue::Bool(_)) => Ok(UiValue::Bool(lua_boolean(value, "element:set_value")?)),
        Some(UiValue::Text(_)) => Ok(UiValue::Text(lua_string(value, "element:set_value")?)),
        Some(UiValue::Index(_)) | None => {
            let index = lua_index(&value)?;
            Ok(UiValue::Index(index))
        }
    }
}

fn lua_index(value: &Value) -> mlua::Result<usize> {
    let number = match value {
        Value::Integer(index) => *index,
        Value::Number(number) if number.fract() == 0.0 => *number as i64,
        _ => {
            return Err(LuaError::runtime(
                "element:set_value: индекс выпадающего списка должен быть целым числом",
            ));
        }
    };
    usize::try_from(number - 1)
        .map_err(|_| LuaError::runtime("element:set_value: индекс начинается с 1"))
}

fn lua_string(value: Value, what: &str) -> mlua::Result<String> {
    match value {
        Value::String(text) => Ok(text.to_str()?.to_owned()),
        _ => Err(LuaError::runtime(format!("{what} ожидает строку"))),
    }
}

fn lua_strings(value: Value, what: &str) -> mlua::Result<Vec<String>> {
    let Value::Table(table) = value else {
        return Err(LuaError::runtime(format!("{what} ожидает список строк")));
    };
    let mut options = Vec::new();
    for index in 1..=table.raw_len() {
        let item: Value = table.raw_get(index)?;
        options.push(lua_string(item, what)?);
    }
    Ok(options)
}

fn lua_finite(value: &Value, what: &str) -> mlua::Result<f32> {
    let number = match value {
        Value::Number(number) => *number,
        Value::Integer(number) => *number as f64,
        _ => return Err(LuaError::runtime(format!("{what} ожидает число"))),
    };
    if !number.is_finite() {
        return Err(LuaError::runtime(format!("{what} ожидает конечное число")));
    }
    Ok(number as f32)
}

/// Цвет `{r, g, b, a}` с компонентами 0..=255.
fn lua_color(value: Value) -> mlua::Result<[u8; 4]> {
    let Value::Table(table) = value else {
        return Err(LuaError::runtime(
            "element:set_color ожидает таблицу {r, g, b, a}",
        ));
    };
    let mut color = [0_u8; 4];
    for (slot, key) in color.iter_mut().zip(["r", "g", "b", "a"]) {
        let component: i64 = table.get(key)?;
        *slot = u8::try_from(component)
            .map_err(|_| LuaError::runtime("element:set_color: компонента вне 0..255"))?;
    }
    Ok(color)
}

fn color_table(lua: &Lua, color: [u8; 4]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (key, value) in ["r", "g", "b", "a"].into_iter().zip(color) {
        table.set(key, i64::from(value))?;
    }
    Ok(table)
}

fn vec2_table(lua: &Lua, value: [f32; 2]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("x", f64::from(value[0]))?;
    table.set("y", f64::from(value[1]))?;
    Ok(table)
}

fn rect_table(lua: &Lua, rect: RectSpec) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("anchor_min", vec2_table(lua, rect.anchor_min)?)?;
    table.set("anchor_max", vec2_table(lua, rect.anchor_max)?)?;
    table.set("pivot", vec2_table(lua, rect.pivot)?)?;
    table.set("offset_min", vec2_table(lua, rect.offset_min)?)?;
    table.set("offset_max", vec2_table(lua, rect.offset_max)?)?;
    Ok(table)
}

/// Частичное обновление: поля, которых нет в таблице, сохраняют прежнее значение.
fn apply_rect_fields(rect: &mut RectSpec, fields: &Table) -> mlua::Result<()> {
    let targets: [(&str, &mut [f32; 2]); 5] = [
        ("anchor_min", &mut rect.anchor_min),
        ("anchor_max", &mut rect.anchor_max),
        ("pivot", &mut rect.pivot),
        ("offset_min", &mut rect.offset_min),
        ("offset_max", &mut rect.offset_max),
    ];
    for (key, target) in targets {
        let value: Value = fields.get(key)?;
        if value.is_nil() {
            continue;
        }
        let Value::Table(vector) = value else {
            return Err(LuaError::runtime(format!(
                "element:set_rect: поле {key} ожидает таблицу {{x, y}}"
            )));
        };
        target[0] = lua_finite(&vector.get::<Value>("x")?, "element:set_rect")?;
        target[1] = lua_finite(&vector.get::<Value>("y")?, "element:set_rect")?;
    }
    Ok(())
}
