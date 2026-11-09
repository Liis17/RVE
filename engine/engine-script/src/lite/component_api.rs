//! Lua API компонентов, которые скрипт меняет во время игры: свет, видимость и материал
//! меш-рендерера, эмиттеры частиц, окружение, LOD, звуковой источник и слежение звука.
//! Каждый метод использует тот же метод транзакции, что и ABI v18, поэтому результат совпадает.

use super::physics_api::{
    bool_field, number_field, reject_unknown_fields, table_argument, vector_field,
};
use super::{
    ActiveCall, method_argument, object_alive, object_handle_of, read_vector, vector_table,
    with_active_call,
};
use crate::transaction::ScriptTransaction;
use engine_scene::audio::{AudioRolloff, AudioSourceComponent};
use engine_scene::world::{MAX_PARTICLE_BURST, MeshMaterialParams, ParticleCommand};
use engine_scene::{
    AudioGroup, DirectionalLightComponent, ObjectHandle, PointLightComponent,
    ProceduralSkyComponent, SpotLightComponent,
};
use mlua::{Error as LuaError, Lua, MultiValue, Table, Value};

/// Свет объекта: первый найденный из направленного, точечного и конического.
#[derive(Clone, Copy)]
enum LightValue {
    Directional(DirectionalLightComponent),
    Point(PointLightComponent),
    Spot(SpotLightComponent),
}

/// Методы компонентов объекта: свет, mesh, частицы, окружение, LOD и звуковой источник.
pub(super) fn add_component_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: ActiveCall,
) -> mlua::Result<()> {
    add_light_methods(lua, object, handle, &active_call)?;
    add_mesh_methods(lua, object, handle, &active_call)?;
    add_particle_methods(lua, object, handle, &active_call)?;
    add_environment_methods(lua, object, handle, &active_call)?;
    add_lod_methods(lua, object, handle, &active_call)?;
    add_audio_source_methods(lua, object, handle, &active_call)?;
    Ok(())
}

fn add_light_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: &ActiveCall,
) -> mlua::Result<()> {
    let get_active = active_call.clone();
    object.set(
        "get_light",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_active, handle)? {
                return Ok(Value::Nil);
            }
            let current = with_active_call(&get_active, |transaction| {
                Ok(current_light(transaction, handle))
            })?;
            match current {
                Some((value, enabled)) => Ok(Value::Table(light_table(lua, value, enabled)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_active = active_call.clone();
    object.set(
        "set_light",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_light")?;
            if !object_alive(&set_active, handle)? {
                return Ok(false);
            }
            with_active_call(&set_active, |transaction| {
                let (current, _) = current_light(transaction, handle)
                    .ok_or_else(|| LuaError::runtime("object.set_light requires a light"))?;
                // Все поля разбираются до первой записи: ошибка в `enabled` не оставляет свет
                // частично изменённым.
                let updated = light_from_table(&table, current)?;
                let enabled = bool_field(&table, "enabled")?;
                match updated {
                    LightValue::Directional(light) => {
                        transaction.stage_directional_light(handle, light)
                    }
                    LightValue::Point(light) => transaction.stage_point_light(handle, light),
                    LightValue::Spot(light) => transaction.stage_spot_light(handle, light),
                }
                .map_err(LuaError::runtime)?;
                if let Some(enabled) = enabled {
                    transaction
                        .stage_light_enabled(handle, enabled)
                        .map_err(LuaError::runtime)?;
                }
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

fn add_mesh_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: &ActiveCall,
) -> mlua::Result<()> {
    let visible_active = active_call.clone();
    object.set(
        "set_visible",
        lua.create_function(move |_, arguments: MultiValue| {
            let value = method_argument(arguments, "object.set_visible")?;
            let Value::Boolean(visible) = value else {
                return Err(LuaError::runtime("object.set_visible expects a boolean"));
            };
            if !object_alive(&visible_active, handle)? {
                return Ok(false);
            }
            with_active_call(&visible_active, |transaction| {
                transaction
                    .stage_mesh_visible(handle, visible)
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;

    let is_visible_active = active_call.clone();
    object.set(
        "is_visible",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&is_visible_active, handle)? {
                return Ok(Value::Nil);
            }
            let visible = with_active_call(&is_visible_active, |transaction| {
                Ok(transaction.mesh_visible(handle))
            })?;
            Ok(visible.map_or(Value::Nil, Value::Boolean))
        })?,
    )?;

    let material_active = active_call.clone();
    object.set(
        "set_material_params",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_material_params")?;
            reject_unknown_fields(
                &table,
                &["base_color", "emissive", "metallic", "roughness"],
                "material params",
            )?;
            let params = MeshMaterialParams {
                base_color: vector_field(&table, "base_color")?,
                emissive: vector_field(&table, "emissive")?,
                metallic: number_field(&table, "metallic")?,
                roughness: number_field(&table, "roughness")?,
            };
            if !object_alive(&material_active, handle)? {
                return Ok(false);
            }
            with_active_call(&material_active, |transaction| {
                transaction
                    .stage_mesh_material(handle, params)
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

fn add_particle_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: &ActiveCall,
) -> mlua::Result<()> {
    for (name, playing) in [("particles_play", true), ("particles_stop", false)] {
        let playing_active = active_call.clone();
        object.set(
            name,
            lua.create_function(move |_, _arguments: MultiValue| {
                if !object_alive(&playing_active, handle)? {
                    return Ok(false);
                }
                with_active_call(&playing_active, |transaction| {
                    transaction
                        .stage_particles_playing(handle, playing)
                        .map_err(LuaError::runtime)?;
                    Ok(true)
                })
            })?,
        )?;
    }

    let is_playing_active = active_call.clone();
    object.set(
        "particles_is_playing",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&is_playing_active, handle)? {
                return Ok(Value::Nil);
            }
            let playing = with_active_call(&is_playing_active, |transaction| {
                Ok(transaction.particles_playing(handle))
            })?;
            Ok(playing.map_or(Value::Nil, Value::Boolean))
        })?,
    )?;

    let restart_active = active_call.clone();
    object.set(
        "particles_restart",
        lua.create_function(move |_, _arguments: MultiValue| {
            if !object_alive(&restart_active, handle)? {
                return Ok(false);
            }
            with_active_call(&restart_active, |transaction| {
                transaction
                    .stage_particle_command(ParticleCommand::Restart { object: handle })
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;

    let emit_active = active_call.clone();
    object.set(
        "particles_emit",
        lua.create_function(move |_, arguments: MultiValue| {
            let count = match method_argument(arguments, "object.particles_emit")? {
                Value::Integer(value) => value,
                _ => {
                    return Err(LuaError::runtime(
                        "object.particles_emit expects an integer count",
                    ));
                }
            };
            let count = u32::try_from(count)
                .ok()
                .filter(|count| (1..=MAX_PARTICLE_BURST).contains(count))
                .ok_or_else(|| {
                    LuaError::runtime(format!(
                        "object.particles_emit count must be in 1..={MAX_PARTICLE_BURST}"
                    ))
                })?;
            if !object_alive(&emit_active, handle)? {
                return Ok(false);
            }
            with_active_call(&emit_active, |transaction| {
                transaction
                    .stage_particle_command(ParticleCommand::Burst {
                        object: handle,
                        count,
                    })
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

fn add_environment_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: &ActiveCall,
) -> mlua::Result<()> {
    let get_active = active_call.clone();
    object.set(
        "get_environment",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_active, handle)? {
                return Ok(Value::Nil);
            }
            let state = with_active_call(&get_active, |transaction| {
                Ok(transaction.environment_state(handle))
            })?;
            let Some((intensity, sky)) = state else {
                return Ok(Value::Nil);
            };
            let table = lua.create_table()?;
            table.set("intensity", intensity)?;
            if let Some(sky) = sky {
                table.set("sky", sky_table(lua, sky)?)?;
            }
            Ok(Value::Table(table))
        })?,
    )?;

    let set_active = active_call.clone();
    object.set(
        "set_environment",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_environment")?;
            reject_unknown_fields(&table, &["intensity", "sky"], "environment")?;
            let intensity = number_field(&table, "intensity")?;
            let sky = match table.get::<Value>("sky")? {
                Value::Nil => None,
                Value::Table(sky) => Some(sky),
                _ => return Err(LuaError::runtime("environment.sky must be a table")),
            };
            if !object_alive(&set_active, handle)? {
                return Ok(false);
            }
            with_active_call(&set_active, |transaction| {
                let sky = match sky {
                    Some(sky_table) => {
                        let current = transaction
                            .environment_state(handle)
                            .and_then(|(_, sky)| sky)
                            .ok_or_else(|| {
                                LuaError::runtime("environment is not a procedural sky")
                            })?;
                        Some(sky_from_table(&sky_table, current)?)
                    }
                    None => None,
                };
                transaction
                    .stage_environment(handle, intensity, sky)
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

fn add_lod_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: &ActiveCall,
) -> mlua::Result<()> {
    let get_active = active_call.clone();
    object.set(
        "get_lod",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_active, handle)? {
                return Ok(Value::Nil);
            }
            let lod = with_active_call(&get_active, |transaction| {
                Ok(transaction.lod_distances(handle))
            })?;
            let Some((distances, cull)) = lod else {
                return Ok(Value::Nil);
            };
            let table = lua.create_table()?;
            table.set("distances", lua.create_sequence_from(distances)?)?;
            table.set("cull_distance", cull)?;
            Ok(Value::Table(table))
        })?,
    )?;

    let set_active = active_call.clone();
    object.set(
        "set_lod_distances",
        lua.create_function(move |_, arguments: MultiValue| {
            // Вызов через `:` передаёт объект первым аргументом.
            let mut arguments = arguments.into_iter().skip(1);
            let Some(Value::Table(distances)) = arguments.next() else {
                return Err(LuaError::runtime(
                    "object.set_lod_distances expects a distance table",
                ));
            };
            let cull = arguments.next().unwrap_or(Value::Nil);
            let mut values = Vec::new();
            for value in distances.sequence_values::<f64>() {
                values.push(finite_distance(value?)?);
            }
            let cull = match cull {
                Value::Nil => None,
                Value::Integer(value) => Some(finite_distance(value as f64)?),
                Value::Number(value) => Some(finite_distance(value)?),
                _ => return Err(LuaError::runtime("cull_distance must be a number or nil")),
            };
            if !object_alive(&set_active, handle)? {
                return Ok(false);
            }
            with_active_call(&set_active, |transaction| {
                transaction
                    .stage_lod_distances(handle, values, cull)
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

fn add_audio_source_methods(
    lua: &Lua,
    object: &Table,
    handle: ObjectHandle,
    active_call: &ActiveCall,
) -> mlua::Result<()> {
    let get_active = active_call.clone();
    object.set(
        "get_audio_source",
        lua.create_function(move |lua, _arguments: MultiValue| {
            if !object_alive(&get_active, handle)? {
                return Ok(Value::Nil);
            }
            let audio = with_active_call(&get_active, |transaction| {
                Ok(transaction.audio_settings(handle))
            })?;
            match audio {
                Some(audio) => Ok(Value::Table(audio_table(lua, &audio)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let set_active = active_call.clone();
    object.set(
        "set_audio_source",
        lua.create_function(move |_, arguments: MultiValue| {
            let table = table_argument(arguments, "object.set_audio_source")?;
            if !object_alive(&set_active, handle)? {
                return Ok(false);
            }
            with_active_call(&set_active, |transaction| {
                let current = transaction.audio_settings(handle).ok_or_else(|| {
                    LuaError::runtime("object.set_audio_source requires an audio_source")
                })?;
                let settings = audio_from_table(&table, current)?;
                transaction
                    .stage_audio_settings(handle, settings)
                    .map_err(LuaError::runtime)?;
                Ok(true)
            })
        })?,
    )?;
    Ok(())
}

/// Свет объекта и его флаг включения: сначала направленный, затем точечный, затем конический.
fn current_light(
    transaction: &ScriptTransaction,
    handle: ObjectHandle,
) -> Option<(LightValue, bool)> {
    let value = transaction
        .directional_light(handle)
        .map(LightValue::Directional)
        .or_else(|| transaction.point_light(handle).map(LightValue::Point))
        .or_else(|| transaction.spot_light(handle).map(LightValue::Spot))?;
    let enabled = transaction.light_enabled(handle).unwrap_or(true);
    Some((value, enabled))
}

fn light_table(lua: &Lua, value: LightValue, enabled: bool) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("enabled", enabled)?;
    match value {
        LightValue::Directional(light) => {
            table.set("kind", "directional")?;
            table.set("color", vector_table(lua, light.color)?)?;
            table.set("ambient", vector_table(lua, light.ambient)?)?;
            table.set("shadows", light.shadows)?;
            table.set("shadow_distance", light.shadow_distance)?;
            table.set("shadow_cascades", light.shadow_cascades)?;
        }
        LightValue::Point(light) => {
            table.set("kind", "point")?;
            table.set("color", vector_table(lua, light.color)?)?;
            table.set("intensity", light.intensity)?;
            table.set("range", light.range)?;
            table.set("shadows", light.shadows)?;
        }
        LightValue::Spot(light) => {
            table.set("kind", "spot")?;
            table.set("color", vector_table(lua, light.color)?)?;
            table.set("intensity", light.intensity)?;
            table.set("range", light.range)?;
            table.set("inner_angle", light.inner_angle_deg)?;
            table.set("outer_angle", light.outer_angle_deg)?;
            table.set("shadows", light.shadows)?;
        }
    }
    Ok(table)
}

/// Применяет поля Lua-таблицы к свету того же вида; неизвестные поля отклоняются.
fn light_from_table(table: &Table, current: LightValue) -> mlua::Result<LightValue> {
    match current {
        LightValue::Directional(mut light) => {
            reject_unknown_fields(
                table,
                &[
                    "kind",
                    "enabled",
                    "color",
                    "ambient",
                    "shadows",
                    "shadow_distance",
                    "shadow_cascades",
                ],
                "directional light",
            )?;
            set_color(table, &mut light.color)?;
            if let Some(ambient) = vector_field(table, "ambient")? {
                light.ambient = ambient;
            }
            if let Some(shadows) = bool_field(table, "shadows")? {
                light.shadows = shadows;
            }
            if let Some(distance) = number_field(table, "shadow_distance")? {
                light.shadow_distance = distance;
            }
            if let Some(cascades) = number_field(table, "shadow_cascades")? {
                if cascades.fract() != 0.0 || !(1.0..=4.0).contains(&cascades) {
                    return Err(LuaError::runtime("shadow_cascades must be an integer 1..4"));
                }
                light.shadow_cascades = cascades as u8;
            }
            Ok(LightValue::Directional(light))
        }
        LightValue::Point(mut light) => {
            reject_unknown_fields(
                table,
                &["kind", "enabled", "color", "intensity", "range", "shadows"],
                "point light",
            )?;
            set_color(table, &mut light.color)?;
            if let Some(intensity) = number_field(table, "intensity")? {
                light.intensity = intensity;
            }
            if let Some(range) = number_field(table, "range")? {
                light.range = range;
            }
            if let Some(shadows) = bool_field(table, "shadows")? {
                light.shadows = shadows;
            }
            Ok(LightValue::Point(light))
        }
        LightValue::Spot(mut light) => {
            reject_unknown_fields(
                table,
                &[
                    "kind",
                    "enabled",
                    "color",
                    "intensity",
                    "range",
                    "inner_angle",
                    "outer_angle",
                    "shadows",
                ],
                "spot light",
            )?;
            set_color(table, &mut light.color)?;
            if let Some(intensity) = number_field(table, "intensity")? {
                light.intensity = intensity;
            }
            if let Some(range) = number_field(table, "range")? {
                light.range = range;
            }
            if let Some(inner) = number_field(table, "inner_angle")? {
                light.inner_angle_deg = inner;
            }
            if let Some(outer) = number_field(table, "outer_angle")? {
                light.outer_angle_deg = outer;
            }
            if let Some(shadows) = bool_field(table, "shadows")? {
                light.shadows = shadows;
            }
            Ok(LightValue::Spot(light))
        }
    }
}

fn set_color(table: &Table, color: &mut [f32; 3]) -> mlua::Result<()> {
    if let Some(value) = vector_field(table, "color")? {
        *color = value;
    }
    Ok(())
}

fn sky_table(lua: &Lua, sky: ProceduralSkyComponent) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("zenith_color", vector_table(lua, sky.zenith_color)?)?;
    table.set("horizon_color", vector_table(lua, sky.horizon_color)?)?;
    table.set("ground_color", vector_table(lua, sky.ground_color)?)?;
    table.set("sun_size_deg", sky.sun_size_deg)?;
    table.set("sun_intensity", sky.sun_intensity)?;
    Ok(table)
}

fn sky_from_table(
    table: &Table,
    current: ProceduralSkyComponent,
) -> mlua::Result<ProceduralSkyComponent> {
    reject_unknown_fields(
        table,
        &[
            "zenith_color",
            "horizon_color",
            "ground_color",
            "sun_size_deg",
            "sun_intensity",
        ],
        "sky",
    )?;
    let mut sky = current;
    if let Some(color) = vector_field(table, "zenith_color")? {
        sky.zenith_color = color;
    }
    if let Some(color) = vector_field(table, "horizon_color")? {
        sky.horizon_color = color;
    }
    if let Some(color) = vector_field(table, "ground_color")? {
        sky.ground_color = color;
    }
    if let Some(size) = number_field(table, "sun_size_deg")? {
        sky.sun_size_deg = size;
    }
    if let Some(intensity) = number_field(table, "sun_intensity")? {
        sky.sun_intensity = intensity;
    }
    Ok(sky)
}

fn audio_table(lua: &Lua, audio: &AudioSourceComponent) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "group",
        match audio.group {
            AudioGroup::Master => "master",
            AudioGroup::Music => "music",
            AudioGroup::Effects => "effects",
            AudioGroup::Ambient => "ambient",
        },
    )?;
    table.set("volume", audio.volume)?;
    table.set("looping", audio.looping)?;
    table.set("play_on_start", audio.play_on_start)?;
    table.set("spatial_blend", audio.spatial_blend)?;
    table.set("min_distance", audio.min_distance)?;
    table.set("max_distance", audio.max_distance)?;
    table.set(
        "rolloff",
        match audio.rolloff {
            AudioRolloff::Inverse => "inverse",
            AudioRolloff::Linear => "linear",
        },
    )?;
    table.set("pitch", audio.pitch)?;
    table.set("force_mono", audio.force_mono)?;
    Ok(table)
}

/// Применяет поля Lua-таблицы к источнику. Клип не меняется: он задаётся сценой и `[[assets]]`.
fn audio_from_table(
    table: &Table,
    current: AudioSourceComponent,
) -> mlua::Result<AudioSourceComponent> {
    reject_unknown_fields(
        table,
        &[
            "clip",
            "group",
            "volume",
            "looping",
            "play_on_start",
            "spatial_blend",
            "min_distance",
            "max_distance",
            "rolloff",
            "pitch",
            "force_mono",
        ],
        "audio source",
    )?;
    if table.contains_key("clip")? {
        return Err(LuaError::runtime(
            "audio source clip is set by the scene and cannot change at runtime",
        ));
    }
    let mut audio = current;
    if let Some(group) = table.get::<Option<String>>("group")? {
        audio.group = match group.as_str() {
            "music" => AudioGroup::Music,
            "effects" => AudioGroup::Effects,
            "ambient" => AudioGroup::Ambient,
            other => {
                return Err(LuaError::runtime(format!(
                    "unknown audio group {other:?}: expected music, effects or ambient"
                )));
            }
        };
    }
    if let Some(volume) = number_field(table, "volume")? {
        audio.volume = volume;
    }
    if let Some(looping) = bool_field(table, "looping")? {
        audio.looping = looping;
    }
    if let Some(play_on_start) = bool_field(table, "play_on_start")? {
        audio.play_on_start = play_on_start;
    }
    if let Some(blend) = number_field(table, "spatial_blend")? {
        audio.spatial_blend = blend;
    }
    if let Some(min) = number_field(table, "min_distance")? {
        audio.min_distance = min;
    }
    if let Some(max) = number_field(table, "max_distance")? {
        audio.max_distance = max;
    }
    if let Some(rolloff) = table.get::<Option<String>>("rolloff")? {
        audio.rolloff = match rolloff.as_str() {
            "inverse" => AudioRolloff::Inverse,
            "linear" => AudioRolloff::Linear,
            other => {
                return Err(LuaError::runtime(format!(
                    "unknown rolloff {other:?}: expected inverse or linear"
                )));
            }
        };
    }
    if let Some(pitch) = number_field(table, "pitch")? {
        audio.pitch = pitch;
    }
    if let Some(force_mono) = bool_field(table, "force_mono")? {
        audio.force_mono = force_mono;
    }
    Ok(audio)
}

/// Опции `rve.audio.play`, которые относятся к пространственному звуку: `follow`, `min_distance`,
/// `max_distance`, `spatial_blend` и `force_mono`. Вызывается из `rve.audio.play` с таблицей опций.
pub(super) fn read_clip_spatial_options(
    lua: &Lua,
    options: &Table,
    params: &mut engine_scene::ClipParams,
) -> mlua::Result<()> {
    if let Some(follow) = options.get::<Option<Table>>("follow")? {
        let handle = object_handle_of(lua, &follow)?
            .ok_or_else(|| LuaError::runtime("rve.audio.play follow expects a scene object"))?;
        params.follow = Some(handle);
    }
    if let Some(min) = number_field(options, "min_distance")? {
        params.min_distance = min;
    }
    if let Some(max) = number_field(options, "max_distance")? {
        params.max_distance = max;
    }
    if let Some(blend) = number_field(options, "spatial_blend")? {
        params.spatial_blend = blend;
    }
    if let Some(force_mono) = bool_field(options, "force_mono")? {
        params.force_mono = force_mono;
    }
    Ok(())
}

/// `rve.audio.set_voice_position(id, pos)`: фиксирует излучатель голоса и снимает слежение.
pub(super) fn audio_set_voice_position(
    lua: &Lua,
    active_call: &ActiveCall,
) -> mlua::Result<mlua::Function> {
    let active_call = active_call.clone();
    lua.create_function(move |_, (id, position): (i64, Table)| {
        let voice = u64::try_from(id)
            .ok()
            .and_then(engine_scene::VoiceId::from_raw)
            .ok_or_else(|| LuaError::runtime("rve.audio.set_voice_position expects a voice id"))?;
        let position = read_vector(&position, "rve.audio.set_voice_position position")?;
        if !position.iter().all(|value| value.is_finite()) {
            return Err(LuaError::runtime(
                "rve.audio.set_voice_position position must be finite",
            ));
        }
        with_active_call(&active_call, |transaction| {
            transaction
                .stage_audio(engine_scene::AudioRequest::SetVoicePosition { voice, position })
                .map_err(LuaError::runtime)
        })
    })
}

fn finite_distance(value: f64) -> mlua::Result<f32> {
    let converted = value as f32;
    if converted.is_finite() {
        Ok(converted)
    } else {
        Err(LuaError::runtime("LOD distance must be a finite number"))
    }
}
