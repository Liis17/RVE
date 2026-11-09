//! Lua API компонентов физики и запросов ABI v17: коллайдер, rigid body, character controller,
//! перекрытия, касты и `raycast_all`. Каждая функция использует тот же метод транзакции, что и
//! ABI-хост, поэтому обе ветки дают одинаковый результат.

use super::{
    ActiveCall, method_argument, object_alive, object_table, read_vector, vector_table,
    with_active_call,
};
use crate::physics::ColliderValues;
use crate::transaction::{ScriptQueryFilter, ScriptTransaction};
use engine_physics::{Pose, QueryHit, Ray, Shape};
use engine_scene::{
    CharacterControllerComponent, ColliderShape, ObjectHandle, RigidBodyComponent, RigidBodyKind,
};
use glam::{Quat, Vec3};
use mlua::{Error as LuaError, Lua, MultiValue, Table, Value};

/// Скин по умолчанию для касов; совпадает с `skin` контроллера по умолчанию.
const DEFAULT_CAST_SKIN: f32 = 0.02;

/// Методы компонентов объекта: `get_*`/`set_*`, `simple_move`, `jump`, `vertical_velocity` и
/// `get_overlaps`.
pub(super) fn add_component_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    let get_collider = active_call.clone();
    object.set(
        "get_collider",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_collider, handle)? {
                return Ok(Value::Nil);
            }
            let values = with_active_call(&get_collider, |transaction| {
                Ok(transaction
                    .collider_component(handle)
                    .ok()
                    .map(|component| ColliderValues::from_component(&component)))
            })?;
            match values {
                Some(values) => Ok(Value::Table(collider_table(lua, values)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_collider = active_call.clone();
    object.set(
        "set_collider",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_collider")?;
            if !object_alive(&set_collider, handle)? {
                return Ok(false);
            }
            with_active_call(&set_collider, |transaction| {
                let current = transaction
                    .collider_component(handle)
                    .map_err(LuaError::runtime)?;
                let values = collider_from_table(&table, ColliderValues::from_component(&current))?;
                transaction
                    .set_collider_component(handle, values.apply_to(&current))
                    .map_err(LuaError::runtime)
            })?;
            Ok(true)
        })?,
    )?;

    let get_body = active_call.clone();
    object.set(
        "get_rigid_body",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_body, handle)? {
                return Ok(Value::Nil);
            }
            let body = with_active_call(&get_body, |transaction| {
                Ok(transaction.rigid_body_component(handle).ok())
            })?;
            match body {
                Some(body) => Ok(Value::Table(rigid_body_table(lua, body)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_body = active_call.clone();
    object.set(
        "set_rigid_body",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_rigid_body")?;
            if !object_alive(&set_body, handle)? {
                return Ok(false);
            }
            with_active_call(&set_body, |transaction| {
                let current = transaction
                    .rigid_body_component(handle)
                    .map_err(LuaError::runtime)?;
                let body = rigid_body_from_table(&table, current)?;
                transaction
                    .set_rigid_body_component(handle, body)
                    .map_err(LuaError::runtime)
            })?;
            Ok(true)
        })?,
    )?;

    let get_controller = active_call.clone();
    object.set(
        "get_controller",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_controller, handle)? {
                return Ok(Value::Nil);
            }
            let controller = with_active_call(&get_controller, |transaction| {
                Ok(transaction.controller_component(handle).ok())
            })?;
            match controller {
                Some(controller) => Ok(Value::Table(controller_table(lua, controller)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_controller = active_call.clone();
    object.set(
        "set_controller",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_controller")?;
            if !object_alive(&set_controller, handle)? {
                return Ok(false);
            }
            with_active_call(&set_controller, |transaction| {
                let current = transaction
                    .controller_component(handle)
                    .map_err(LuaError::runtime)?;
                let controller = controller_from_table(&table, current)?;
                transaction
                    .set_controller_component(handle, controller)
                    .map_err(LuaError::runtime)
            })?;
            Ok(true)
        })?,
    )?;

    let simple_move = active_call.clone();
    object.set(
        "simple_move",
        lua.create_function(move |lua, arguments: MultiValue| {
            let velocity = table_argument(arguments, "object.simple_move")?;
            let velocity = Vec3::from_array(read_vector(&velocity, "velocity")?);
            if !object_alive(&simple_move, handle)? {
                return Ok(Value::Nil);
            }
            let movement = with_active_call(&simple_move, |transaction| {
                transaction
                    .simple_move(handle, velocity)
                    .map_err(LuaError::runtime)
            })?;
            let result = lua.create_table()?;
            result.set("grounded", movement.grounded)?;
            result.set("collided", !movement.hits.is_empty())?;
            Ok(Value::Table(result))
        })?,
    )?;

    let jump = active_call.clone();
    object.set(
        "jump",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&jump, handle)? {
                return Ok(false);
            }
            with_active_call(&jump, |transaction| {
                transaction.jump(handle).map_err(LuaError::runtime)
            })
        })?,
    )?;

    let vertical = active_call.clone();
    object.set(
        "vertical_velocity",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&vertical, handle)? {
                return Ok(0.0);
            }
            with_active_call(&vertical, |transaction| {
                transaction
                    .vertical_velocity(handle)
                    .map(f64::from)
                    .map_err(LuaError::runtime)
            })
        })?,
    )?;

    let overlaps = active_call.clone();
    object.set(
        "get_overlaps",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&overlaps, handle)? {
                return lua.create_table();
            }
            let handles = with_active_call(&overlaps, |transaction| {
                transaction.overlaps(handle).map_err(LuaError::runtime)
            })?;
            objects_table(lua, &overlaps, handles)
        })?,
    )?;
    Ok(())
}

/// Функции `rve.physics` для перекрытий, кастов и `raycast_all`.
pub(super) fn add_query_functions(
    lua: &Lua,
    physics: &Table,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    let sphere = active_call.clone();
    physics.set(
        "overlap_sphere",
        lua.create_function(
            move |lua, (center, radius, options): (Table, f64, Option<Table>)| {
                let shape = Shape::sphere(finite_f32(radius, "radius")?)
                    .map_err(|error| LuaError::runtime(error.to_string()))?;
                overlap_objects(lua, &sphere, &shape, center, options)
            },
        )?,
    )?;

    let box_active = active_call.clone();
    physics.set(
        "overlap_box",
        lua.create_function(
            move |lua, (center, size, options): (Table, Table, Option<Table>)| {
                let shape = box_shape(&size)?;
                overlap_objects(lua, &box_active, &shape, center, options)
            },
        )?,
    )?;

    let capsule_active = active_call.clone();
    physics.set(
        "overlap_capsule",
        lua.create_function(
            move |lua, (center, radius, height, options): (Table, f64, f64, Option<Table>)| {
                let shape = capsule_shape(radius, height)?;
                overlap_objects(lua, &capsule_active, &shape, center, options)
            },
        )?,
    )?;

    let sphere_cast = active_call.clone();
    physics.set(
        "sphere_cast",
        lua.create_function(
            move |lua, (center, radius, displacement, options): (Table, f64, Table, Option<Table>)| {
                let shape = Shape::sphere(finite_f32(radius, "radius")?)
                    .map_err(|error| LuaError::runtime(error.to_string()))?;
                cast_hit(lua, &sphere_cast, &shape, center, displacement, options)
            },
        )?,
    )?;

    let box_cast_active = active_call.clone();
    physics.set(
        "box_cast",
        lua.create_function(
            move |lua, (center, size, displacement, options): (Table, Table, Table, Option<Table>)| {
                let shape = box_shape(&size)?;
                cast_hit(lua, &box_cast_active, &shape, center, displacement, options)
            },
        )?,
    )?;

    let capsule_cast = active_call.clone();
    physics.set(
        "capsule_cast",
        lua.create_function(
            move |lua,
                  (center, radius, height, displacement, options): (
                Table,
                f64,
                f64,
                Table,
                Option<Table>,
            )| {
                let shape = capsule_shape(radius, height)?;
                cast_hit(lua, &capsule_cast, &shape, center, displacement, options)
            },
        )?,
    )?;

    let raycast_all = active_call;
    physics.set(
        "raycast_all",
        lua.create_function(
            move |lua,
                  (origin, direction, max_distance, options): (
                Table,
                Table,
                f64,
                Option<Table>,
            )| {
                let max_distance = finite_f32(max_distance, "max_distance")?;
                if max_distance < 0.0 {
                    return Err(LuaError::runtime(
                        "raycast_all max_distance must be nonnegative",
                    ));
                }
                let ray = Ray::new(
                    Vec3::from_array(read_vector(&origin, "origin")?),
                    Vec3::from_array(read_vector(&direction, "direction")?),
                    max_distance,
                )
                .map_err(|error| LuaError::runtime(error.to_string()))?;
                let options = query_options(options)?;
                let hits = with_active_call(&raycast_all, |transaction| {
                    let filter = resolve_filter(transaction, &options);
                    transaction
                        .raycast_all(ray, &filter)
                        .map_err(LuaError::runtime)
                })?;
                let result = lua.create_table()?;
                for hit in &hits {
                    if let Some(table) = hit_table(lua, &raycast_all, hit)? {
                        result.push(table)?;
                    }
                }
                Ok(result)
            },
        )?,
    )?;
    Ok(())
}

fn overlap_objects(
    lua: &Lua,
    active_call: &ActiveCall,
    shape: &Shape,
    center: Table,
    options: Option<Table>,
) -> mlua::Result<Table> {
    let pose = center_pose(&center)?;
    let options = query_options(options)?;
    let handles = with_active_call(active_call, |transaction| {
        let filter = resolve_filter(transaction, &options);
        transaction
            .overlap_shape(shape, pose, &filter)
            .map_err(LuaError::runtime)
    })?;
    objects_table(lua, active_call, handles)
}

fn cast_hit(
    lua: &Lua,
    active_call: &ActiveCall,
    shape: &Shape,
    center: Table,
    displacement: Table,
    options: Option<Table>,
) -> mlua::Result<Option<Table>> {
    let pose = center_pose(&center)?;
    let displacement = Vec3::from_array(read_vector(&displacement, "displacement")?);
    let skin = cast_skin(options.as_ref())?;
    let options = query_options(options)?;
    let hit = with_active_call(active_call, |transaction| {
        let filter = resolve_filter(transaction, &options);
        transaction
            .shape_cast(shape, pose, displacement, skin, &filter)
            .map_err(LuaError::runtime)
    })?;
    match hit {
        Some(hit) => hit_table(lua, active_call, &hit),
        None => Ok(None),
    }
}

fn box_shape(size: &Table) -> mlua::Result<Shape> {
    let size = Vec3::from_array(read_vector(size, "size")?);
    Shape::cuboid(size * 0.5).map_err(|error| LuaError::runtime(error.to_string()))
}

fn capsule_shape(radius: f64, height: f64) -> mlua::Result<Shape> {
    Shape::capsule(finite_f32(radius, "radius")?, finite_f32(height, "height")?)
        .map_err(|error| LuaError::runtime(error.to_string()))
}

fn center_pose(center: &Table) -> mlua::Result<Pose> {
    Pose::new(
        Vec3::from_array(read_vector(center, "center")?),
        Quat::IDENTITY,
    )
    .map_err(|error| LuaError::runtime(error.to_string()))
}

fn finite_f32(value: f64, name: &str) -> mlua::Result<f32> {
    let converted = value as f32;
    if value.is_finite() && converted.is_finite() {
        Ok(converted)
    } else {
        Err(LuaError::runtime(format!("{name} must be finite")))
    }
}

/// Опции запроса из Lua: `layer_mask`, `include_triggers`, `ignore` и `skin` для кастов.
struct LuaQueryOptions {
    layer_mask: u32,
    include_triggers: bool,
    ignore: Vec<String>,
}

fn query_options(options: Option<Table>) -> mlua::Result<LuaQueryOptions> {
    let Some(options) = options else {
        return Ok(LuaQueryOptions {
            layer_mask: u32::MAX,
            include_triggers: false,
            ignore: Vec::new(),
        });
    };
    reject_unknown_fields(
        &options,
        &["layer_mask", "include_triggers", "ignore", "skin"],
        "options",
    )?;
    let layer_mask = match options.get::<Value>("layer_mask")? {
        Value::Nil => u32::MAX,
        Value::Integer(value) => u32::try_from(value).map_err(|_| {
            LuaError::runtime("options.layer_mask must be between 0 and 4294967295")
        })?,
        _ => return Err(LuaError::runtime("options.layer_mask must be an integer")),
    };
    let include_triggers = match options.get::<Value>("include_triggers")? {
        Value::Nil => false,
        Value::Boolean(value) => value,
        _ => {
            return Err(LuaError::runtime(
                "options.include_triggers must be a boolean",
            ));
        }
    };
    let ignore = match options.get::<Value>("ignore")? {
        Value::Nil => Vec::new(),
        Value::Table(table) => match table.get::<Value>("id")? {
            Value::Nil => {
                let mut ids = Vec::new();
                for object in table.sequence_values::<Table>() {
                    ids.push(object_id(&object?)?);
                }
                ids
            }
            _ => vec![object_id(&table)?],
        },
        _ => {
            return Err(LuaError::runtime(
                "options.ignore must be an object or a list of objects",
            ));
        }
    };
    Ok(LuaQueryOptions {
        layer_mask,
        include_triggers,
        ignore,
    })
}

fn cast_skin(options: Option<&Table>) -> mlua::Result<f32> {
    let Some(options) = options else {
        return Ok(DEFAULT_CAST_SKIN);
    };
    match options.get::<Value>("skin")? {
        Value::Nil => Ok(DEFAULT_CAST_SKIN),
        Value::Integer(value) => finite_nonnegative(value as f64, "options.skin"),
        Value::Number(value) => finite_nonnegative(value, "options.skin"),
        _ => Err(LuaError::runtime("options.skin must be a number")),
    }
}

fn finite_nonnegative(value: f64, name: &str) -> mlua::Result<f32> {
    let value = finite_f32(value, name)?;
    if value < 0.0 {
        return Err(LuaError::runtime(format!("{name} must be nonnegative")));
    }
    Ok(value)
}

fn object_id(object: &Table) -> mlua::Result<String> {
    object
        .get::<String>("id")
        .map_err(|_| LuaError::runtime("options.ignore must be an object returned by rve.find"))
}

fn resolve_filter(transaction: &ScriptTransaction, options: &LuaQueryOptions) -> ScriptQueryFilter {
    ScriptQueryFilter {
        layer_mask: options.layer_mask,
        include_triggers: options.include_triggers,
        ignore: options
            .ignore
            .iter()
            .filter_map(|id| transaction.handle_by_id(id))
            .collect(),
    }
}

fn hit_table(lua: &Lua, active_call: &ActiveCall, hit: &QueryHit) -> mlua::Result<Option<Table>> {
    let identity = with_active_call(active_call, |transaction| {
        let Some(handle) = transaction.physics_object(hit.body) else {
            return Ok(None);
        };
        Ok(transaction
            .object_identity(handle)
            .map(|(id, name)| (handle, id, name)))
    })?;
    let Some((handle, id, name)) = identity else {
        return Ok(None);
    };
    let result = lua.create_table()?;
    result.set(
        "object",
        object_table(lua, handle, id, name, active_call.clone())?,
    )?;
    result.set("point", vector_table(lua, hit.point.to_array())?)?;
    result.set("normal", vector_table(lua, hit.normal.to_array())?)?;
    result.set("distance", f64::from(hit.distance))?;
    Ok(Some(result))
}

fn objects_table(
    lua: &Lua,
    active_call: &ActiveCall,
    handles: Vec<ObjectHandle>,
) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    for handle in handles {
        let identity = with_active_call(active_call, |transaction| {
            Ok(transaction.object_identity(handle))
        })?;
        if let Some((id, name)) = identity {
            result.push(object_table(lua, handle, id, name, active_call.clone())?)?;
        }
    }
    Ok(result)
}

/// Аргумент метода-таблицы; `self` отбрасывается так же, как в остальных методах объекта.
pub(super) fn table_argument(arguments: MultiValue, name: &str) -> mlua::Result<Table> {
    match method_argument(arguments, name)? {
        Value::Table(table) => Ok(table),
        _ => Err(LuaError::runtime(format!("{name} expects a table"))),
    }
}

pub(super) fn reject_unknown_fields(table: &Table, known: &[&str], what: &str) -> mlua::Result<()> {
    for pair in table.clone().pairs::<String, Value>() {
        let (key, _) = pair?;
        if !known.contains(&key.as_str()) {
            return Err(LuaError::runtime(format!(
                "{what} has unknown field {key:?}"
            )));
        }
    }
    Ok(())
}

pub(super) fn number_field(table: &Table, key: &str) -> mlua::Result<Option<f32>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Integer(value) => finite_f32(value as f64, key).map(Some),
        Value::Number(value) => finite_f32(value, key).map(Some),
        _ => Err(LuaError::runtime(format!("{key} must be a finite number"))),
    }
}

pub(super) fn bool_field(table: &Table, key: &str) -> mlua::Result<Option<bool>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Boolean(value) => Ok(Some(value)),
        _ => Err(LuaError::runtime(format!("{key} must be a boolean"))),
    }
}

pub(super) fn vector_field(table: &Table, key: &str) -> mlua::Result<Option<[f32; 3]>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::Table(vector) => read_vector(&vector, key).map(Some),
        _ => Err(LuaError::runtime(format!("{key} must be a vector table"))),
    }
}

pub(super) fn string_field(table: &Table, key: &str) -> mlua::Result<Option<String>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        Value::String(value) => Ok(Some(value.to_str()?.to_owned())),
        _ => Err(LuaError::runtime(format!("{key} must be a string"))),
    }
}

fn collider_from_table(table: &Table, current: ColliderValues) -> mlua::Result<ColliderValues> {
    reject_unknown_fields(
        table,
        &["shape", "size", "radius", "height", "offset", "trigger"],
        "collider",
    )?;
    let mut values = current;
    if let Some(shape) = string_field(table, "shape")? {
        values.shape = match shape.as_str() {
            "box" => ColliderShape::Box,
            "sphere" => ColliderShape::Sphere,
            "capsule" => ColliderShape::Capsule,
            "convex_mesh" | "triangle_mesh" => {
                return Err(LuaError::runtime(
                    "collider mesh shapes are read-only; use box, sphere or capsule",
                ));
            }
            _ => {
                return Err(LuaError::runtime(
                    "collider shape must be box, sphere or capsule",
                ));
            }
        };
    }
    if let Some(size) = vector_field(table, "size")? {
        values.size = size;
    }
    if let Some(radius) = number_field(table, "radius")? {
        values.radius = radius;
    }
    if let Some(height) = number_field(table, "height")? {
        values.height = height;
    }
    if let Some(offset) = vector_field(table, "offset")? {
        values.offset = offset;
    }
    if let Some(trigger) = bool_field(table, "trigger")? {
        values.trigger = trigger;
    }
    Ok(values)
}

fn collider_table(lua: &Lua, values: ColliderValues) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "shape",
        match values.shape {
            ColliderShape::Box => "box",
            ColliderShape::Sphere => "sphere",
            ColliderShape::Capsule => "capsule",
            ColliderShape::ConvexMesh => "convex_mesh",
            ColliderShape::TriangleMesh => "triangle_mesh",
        },
    )?;
    table.set("size", vector_table(lua, values.size)?)?;
    table.set("radius", f64::from(values.radius))?;
    table.set("height", f64::from(values.height))?;
    table.set("offset", vector_table(lua, values.offset)?)?;
    table.set("trigger", values.trigger)?;
    Ok(table)
}

fn rigid_body_from_table(
    table: &Table,
    current: RigidBodyComponent,
) -> mlua::Result<RigidBodyComponent> {
    reject_unknown_fields(
        table,
        &[
            "kind",
            "mass",
            "linear_damping",
            "angular_damping",
            "gravity_scale",
            "lock_rotation",
        ],
        "rigid_body",
    )?;
    let mut body = current;
    if let Some(kind) = string_field(table, "kind")? {
        body.kind = match kind.as_str() {
            "static" => RigidBodyKind::Static,
            "kinematic" => RigidBodyKind::Kinematic,
            "dynamic" => RigidBodyKind::Dynamic,
            _ => {
                return Err(LuaError::runtime(
                    "rigid_body kind must be static, kinematic or dynamic",
                ));
            }
        };
    }
    if let Some(mass) = number_field(table, "mass")? {
        body.mass = mass;
    }
    if let Some(linear_damping) = number_field(table, "linear_damping")? {
        body.linear_damping = linear_damping;
    }
    if let Some(angular_damping) = number_field(table, "angular_damping")? {
        body.angular_damping = angular_damping;
    }
    if let Some(gravity_scale) = number_field(table, "gravity_scale")? {
        body.gravity_scale = gravity_scale;
    }
    if let Some(lock_rotation) = bool_field(table, "lock_rotation")? {
        body.lock_rotation = lock_rotation;
    }
    Ok(body)
}

fn rigid_body_table(lua: &Lua, body: RigidBodyComponent) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "kind",
        match body.kind {
            RigidBodyKind::Static => "static",
            RigidBodyKind::Kinematic => "kinematic",
            RigidBodyKind::Dynamic => "dynamic",
        },
    )?;
    table.set("mass", f64::from(body.mass))?;
    table.set("linear_damping", f64::from(body.linear_damping))?;
    table.set("angular_damping", f64::from(body.angular_damping))?;
    table.set("gravity_scale", f64::from(body.gravity_scale))?;
    table.set("lock_rotation", body.lock_rotation)?;
    Ok(table)
}

fn controller_from_table(
    table: &Table,
    current: CharacterControllerComponent,
) -> mlua::Result<CharacterControllerComponent> {
    reject_unknown_fields(
        table,
        &[
            "radius",
            "height",
            "step_height",
            "max_slope_deg",
            "snap_distance",
            "skin",
            "push_strength",
            "jump_height",
            "gravity_scale",
            "use_gravity",
        ],
        "controller",
    )?;
    let mut controller = current;
    if let Some(radius) = number_field(table, "radius")? {
        controller.radius = radius;
    }
    if let Some(height) = number_field(table, "height")? {
        controller.height = height;
    }
    if let Some(step_height) = number_field(table, "step_height")? {
        controller.step_height = step_height;
    }
    if let Some(max_slope_deg) = number_field(table, "max_slope_deg")? {
        controller.max_slope_deg = max_slope_deg;
    }
    if let Some(snap_distance) = number_field(table, "snap_distance")? {
        controller.snap_distance = snap_distance;
    }
    if let Some(skin) = number_field(table, "skin")? {
        controller.skin = skin;
    }
    if let Some(push_strength) = number_field(table, "push_strength")? {
        controller.push_strength = push_strength;
    }
    if let Some(jump_height) = number_field(table, "jump_height")? {
        controller.jump_height = jump_height;
    }
    if let Some(gravity_scale) = number_field(table, "gravity_scale")? {
        controller.gravity_scale = gravity_scale;
    }
    if let Some(use_gravity) = bool_field(table, "use_gravity")? {
        controller.use_gravity = use_gravity;
    }
    Ok(controller)
}

fn controller_table(lua: &Lua, controller: CharacterControllerComponent) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("radius", f64::from(controller.radius))?;
    table.set("height", f64::from(controller.height))?;
    table.set("step_height", f64::from(controller.step_height))?;
    table.set("max_slope_deg", f64::from(controller.max_slope_deg))?;
    table.set("snap_distance", f64::from(controller.snap_distance))?;
    table.set("skin", f64::from(controller.skin))?;
    table.set("push_strength", f64::from(controller.push_strength))?;
    table.set("jump_height", f64::from(controller.jump_height))?;
    table.set("gravity_scale", f64::from(controller.gravity_scale))?;
    table.set("use_gravity", controller.use_gravity)?;
    Ok(table)
}
