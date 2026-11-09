//! Host callbacks of ABI v18: lights, mesh renderers, particle emitters, environments, LOD groups,
//! audio source settings and spatial clip options. Each one stages the same transaction method as
//! the Lua API in `lite/component_api.rs`, so both guest languages see identical results.

use super::*;
use engine_scene::audio::{AudioRolloff, AudioSourceComponent};
use engine_scene::world::{MeshMaterialParams, ParticleCommand};
use engine_scene::{
    DirectionalLightComponent, PointLightComponent, ProceduralSkyComponent, SpotLightComponent,
};
use rve_sdk::abi::{
    RVE_AUDIO_ROLLOFF_INVERSE, RVE_AUDIO_ROLLOFF_LINEAR, RVE_LIGHT_DIRECTIONAL, RVE_LIGHT_POINT,
    RVE_LIGHT_SPOT, RVE_PARTICLE_BURST, RVE_PARTICLE_RESTART, RveAudioPlayExParams,
    RveAudioSourceState, RveEnvironmentState, RveLightState, RveMeshMaterialParams,
    RveProceduralSky, RveStr,
};

/// Most LOD levels one `lod_set` call may carry; a larger count is refused before allocating.
const MAX_LOD_WRITE: usize = 64;

/// Stages one component write and records its refusal as the call's error.
fn component_result(context: &mut AbiCallContext<'_>, result: Result<(), String>) -> bool {
    match result {
        Ok(()) => true,
        Err(message) => {
            context.error = Some(message);
            false
        }
    }
}

fn light_code(kind: u32) -> Result<u32, String> {
    match kind {
        RVE_LIGHT_DIRECTIONAL | RVE_LIGHT_POINT | RVE_LIGHT_SPOT => Ok(kind),
        other => Err(format!("unknown light kind {other}")),
    }
}

/// Current light of one kind with the object-level enabled flag; `None` without that light.
fn light_state(
    transaction: &ScriptTransaction,
    handle: ObjectHandle,
    kind: u32,
) -> Option<RveLightState> {
    let enabled = transaction.light_enabled(handle)?;
    let mut state = RveLightState {
        enabled,
        ..RveLightState::default()
    };
    match kind {
        RVE_LIGHT_DIRECTIONAL => {
            let light = transaction.directional_light(handle)?;
            state.color = light.color;
            state.ambient = light.ambient;
            state.shadows = light.shadows;
            state.shadow_distance = light.shadow_distance;
            state.shadow_cascades = u32::from(light.shadow_cascades);
        }
        RVE_LIGHT_POINT => {
            let light = transaction.point_light(handle)?;
            state.color = light.color;
            state.intensity = light.intensity;
            state.range = light.range;
            state.shadows = light.shadows;
        }
        _ => {
            let light = transaction.spot_light(handle)?;
            state.color = light.color;
            state.intensity = light.intensity;
            state.range = light.range;
            state.inner_angle_deg = light.inner_angle_deg;
            state.outer_angle_deg = light.outer_angle_deg;
            state.shadows = light.shadows;
        }
    }
    Some(state)
}

pub(super) unsafe extern "C" fn host_light_get(
    ctx: *mut c_void,
    object: u64,
    kind: u32,
    out: *mut RveLightState,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Ok(kind) = light_code(kind) else {
            return false;
        };
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(state) = light_state(context.transaction, handle, kind) else {
            return false;
        };
        // SAFETY: the output was checked non-null and points to the guest's writable stack slot.
        unsafe { *out = state };
        true
    })
}

pub(super) unsafe extern "C" fn host_light_set(
    ctx: *mut c_void,
    object: u64,
    kind: u32,
    value: *const RveLightState,
) -> bool {
    guarded(ctx, false, |context| {
        if value.is_null() {
            context.error = Some("light_set received a null value pointer".to_owned());
            return false;
        }
        // SAFETY: the value was checked non-null and is readable for this synchronous callback.
        let value = unsafe { *value };
        let Some(handle) = live_object(context, object, "light_set") else {
            return false;
        };
        let staged = light_code(kind).and_then(|kind| match kind {
            RVE_LIGHT_DIRECTIONAL => context.transaction.stage_directional_light(
                handle,
                DirectionalLightComponent {
                    color: value.color,
                    ambient: value.ambient,
                    shadows: value.shadows,
                    shadow_distance: value.shadow_distance,
                    shadow_cascades: u8::try_from(value.shadow_cascades)
                        .map_err(|_| "shadow_cascades must be 1..4".to_owned())?,
                },
            ),
            RVE_LIGHT_POINT => context.transaction.stage_point_light(
                handle,
                PointLightComponent {
                    color: value.color,
                    intensity: value.intensity,
                    range: value.range,
                    shadows: value.shadows,
                },
            ),
            _ => context.transaction.stage_spot_light(
                handle,
                SpotLightComponent {
                    color: value.color,
                    intensity: value.intensity,
                    range: value.range,
                    inner_angle_deg: value.inner_angle_deg,
                    outer_angle_deg: value.outer_angle_deg,
                    shadows: value.shadows,
                },
            ),
        });
        let staged = staged.and_then(|()| {
            context
                .transaction
                .stage_light_enabled(handle, value.enabled)
        });
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_mesh_set_material_params(
    ctx: *mut c_void,
    object: u64,
    params: *const RveMeshMaterialParams,
) -> bool {
    guarded(ctx, false, |context| {
        if params.is_null() {
            context.error = Some("set_material_params received a null pointer".to_owned());
            return false;
        }
        // SAFETY: the params were checked non-null and are readable for this synchronous callback.
        let params = unsafe { *params };
        let Some(handle) = live_object(context, object, "set_material_params") else {
            return false;
        };
        let staged = context.transaction.stage_mesh_material(
            handle,
            MeshMaterialParams {
                base_color: params.has_base_color.then_some(params.base_color),
                emissive: params.has_emissive.then_some(params.emissive),
                metallic: params.has_metallic.then_some(params.metallic),
                roughness: params.has_roughness.then_some(params.roughness),
            },
        );
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_mesh_set_material_path(
    ctx: *mut c_void,
    object: u64,
    path: RveStr,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = live_object(context, object, "set_material") else {
            return false;
        };
        // SAFETY: the guest keeps the path string alive for this synchronous callback.
        let staged = unsafe { read_abi_str(path) }
            .map_err(|_| "set_material received an invalid path".to_owned())
            .and_then(|path| {
                VirtualPath::parse(path).map_err(|error| format!("set_material: {error}"))
            })
            .and_then(|path| context.transaction.stage_mesh_material_path(handle, path));
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_mesh_set_visible(
    ctx: *mut c_void,
    object: u64,
    visible: bool,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = live_object(context, object, "mesh_set_visible") else {
            return false;
        };
        let staged = context.transaction.stage_mesh_visible(handle, visible);
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_particles_set_playing(
    ctx: *mut c_void,
    object: u64,
    playing: bool,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = live_object(context, object, "particles_set_playing") else {
            return false;
        };
        let staged = context.transaction.stage_particles_playing(handle, playing);
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_particles_command(
    ctx: *mut c_void,
    object: u64,
    kind: u32,
    count: u32,
) -> bool {
    guarded(ctx, false, |context| {
        let Some(handle) = live_object(context, object, "particles_command") else {
            return false;
        };
        let command = match kind {
            RVE_PARTICLE_RESTART => Ok(ParticleCommand::Restart { object: handle }),
            RVE_PARTICLE_BURST => Ok(ParticleCommand::Burst {
                object: handle,
                count,
            }),
            other => Err(format!("unknown particle command {other}")),
        };
        let staged =
            command.and_then(|command| context.transaction.stage_particle_command(command));
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_environment_get(
    ctx: *mut c_void,
    object: u64,
    out: *mut RveEnvironmentState,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some((intensity, sky)) = context.transaction.environment_state(handle) else {
            return false;
        };
        let sky = sky.map(sky_to_abi);
        let state = RveEnvironmentState {
            intensity,
            has_sky: sky.is_some(),
            sky: sky.unwrap_or_default(),
        };
        // SAFETY: the output was checked non-null and points to the guest's writable stack slot.
        unsafe { *out = state };
        true
    })
}

pub(super) unsafe extern "C" fn host_environment_set(
    ctx: *mut c_void,
    object: u64,
    value: *const RveEnvironmentState,
) -> bool {
    guarded(ctx, false, |context| {
        if value.is_null() {
            context.error = Some("environment_set received a null value pointer".to_owned());
            return false;
        }
        // SAFETY: the value was checked non-null and is readable for this synchronous callback.
        let value = unsafe { *value };
        let Some(handle) = live_object(context, object, "environment_set") else {
            return false;
        };
        let sky = value.has_sky.then(|| sky_from_abi(value.sky));
        let staged = context
            .transaction
            .stage_environment(handle, Some(value.intensity), sky);
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_lod_get(
    ctx: *mut c_void,
    object: u64,
    distances: *mut f32,
    capacity: usize,
    count: *mut usize,
    has_cull: *mut bool,
    cull: *mut f32,
) -> bool {
    guarded(ctx, false, |context| {
        if count.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some((levels, cull_distance)) = context.transaction.lod_distances(handle) else {
            return false;
        };
        // SAFETY: count was checked non-null and points to the guest's writable stack slot.
        unsafe { *count = levels.len() };
        if !distances.is_null() {
            for (index, &distance) in levels.iter().take(capacity).enumerate() {
                // SAFETY: the guest provides `capacity` writable floats at `distances`.
                unsafe { *distances.add(index) = distance };
            }
        }
        if !has_cull.is_null() && !cull.is_null() {
            // SAFETY: both outputs were checked non-null and point to writable stack slots.
            unsafe {
                *has_cull = cull_distance.is_some();
                *cull = cull_distance.unwrap_or(0.0);
            }
        }
        true
    })
}

pub(super) unsafe extern "C" fn host_lod_set(
    ctx: *mut c_void,
    object: u64,
    distances: *const f32,
    count: usize,
    has_cull: bool,
    cull: f32,
) -> bool {
    guarded(ctx, false, |context| {
        if count > MAX_LOD_WRITE || (count > 0 && distances.is_null()) {
            context.error = Some(format!("lod_set accepts 0..={MAX_LOD_WRITE} distances"));
            return false;
        }
        let Some(handle) = live_object(context, object, "lod_set") else {
            return false;
        };
        // SAFETY: the guest keeps `count` floats alive at `distances` for this synchronous callback.
        let values = if count == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(distances, count) }.to_vec()
        };
        let staged =
            context
                .transaction
                .stage_lod_distances(handle, values, has_cull.then_some(cull));
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_audio_get_source(
    ctx: *mut c_void,
    object: u64,
    out: *mut RveAudioSourceState,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(audio) = context.transaction.audio_settings(handle) else {
            return false;
        };
        // SAFETY: the output was checked non-null and points to the guest's writable stack slot.
        unsafe { *out = audio_source_to_abi(&audio) };
        true
    })
}

pub(super) unsafe extern "C" fn host_audio_set_source(
    ctx: *mut c_void,
    object: u64,
    value: *const RveAudioSourceState,
) -> bool {
    guarded(ctx, false, |context| {
        if value.is_null() {
            context.error = Some("audio_set_source received a null value pointer".to_owned());
            return false;
        }
        // SAFETY: the value was checked non-null and is readable for this synchronous callback.
        let value = unsafe { *value };
        let Some(handle) = live_object(context, object, "audio_set_source") else {
            return false;
        };
        let Some(current) = context.transaction.audio_settings(handle) else {
            context.error = Some("audio_set_source requires an audio_source".to_owned());
            return false;
        };
        let staged = audio_source_from_abi(current, value)
            .and_then(|settings| context.transaction.stage_audio_settings(handle, settings));
        component_result(context, staged)
    })
}

pub(super) unsafe extern "C" fn host_audio_play_clip_ex(
    ctx: *mut c_void,
    path: RveStr,
    params: *const RveAudioPlayExParams,
    out_voice: *mut u64,
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        if params.is_null() || out_voice.is_null() {
            return audio_result(
                context,
                Err("play_clip_ex received a null pointer".to_owned()),
            );
        }
        // SAFETY: the guest passes a valid params pointer for this synchronous callback.
        let result =
            clip_params_ex(&mut *context.transaction, unsafe { &*params }).and_then(|clip| {
                // SAFETY: the guest keeps the path string alive for this synchronous callback.
                let path = unsafe { read_abi_str(path) }
                    .map_err(|_| "play_clip_ex received an invalid path".to_owned())?;
                let path =
                    VirtualPath::parse(path).map_err(|error| format!("play_clip_ex: {error}"))?;
                let voice = VoiceId::allocate();
                context.transaction.stage_audio(AudioRequest::PlayClip {
                    voice,
                    path,
                    params: clip,
                })?;
                // SAFETY: out_voice was checked non-null and is writable for this callback.
                unsafe { *out_voice = voice.get() };
                Ok(())
            });
        audio_result(context, result)
    })
}

pub(super) unsafe extern "C" fn host_audio_set_voice_position(
    ctx: *mut c_void,
    voice: u64,
    position: *const [f32; 3],
) -> u32 {
    guarded(ctx, RveStatus::Error as u32, |context| {
        if position.is_null() {
            return audio_result(
                context,
                Err("set_voice_position received a null pointer".to_owned()),
            );
        }
        // SAFETY: the position was checked non-null and is readable for this synchronous callback.
        let position = unsafe { *position };
        let result = VoiceId::from_raw(voice)
            .ok_or_else(|| "set_voice_position expects a nonzero voice id".to_owned())
            .and_then(|voice| {
                if !position.iter().all(|axis| axis.is_finite()) {
                    return Err("voice position must be finite".to_owned());
                }
                context
                    .transaction
                    .stage_audio(AudioRequest::SetVoicePosition { voice, position })
            });
        audio_result(context, result)
    })
}

/// Clip options of ABI v18: the v7 fields plus follow, distances, blend and mono.
fn clip_params_ex(
    transaction: &mut ScriptTransaction,
    params: &RveAudioPlayExParams,
) -> Result<ClipParams, String> {
    let group = audio_group(params.group)?;
    if group == AudioGroup::Master {
        return Err("clips cannot use the master audio group".to_owned());
    }
    let position = if params.has_position {
        Some(params.position)
    } else {
        None
    };
    let follow = match params.follow {
        0 => None,
        raw => Some(
            transaction
                .object_from_abi_handle(raw)
                .ok_or_else(|| "play_clip_ex follow expects a live scene object".to_owned())?,
        ),
    };
    Ok(ClipParams {
        group,
        volume: audio_volume(params.volume)?,
        looping: params.looping,
        position,
        follow,
        spatial_blend: params.spatial_blend,
        min_distance: params.min_distance,
        max_distance: params.max_distance,
        force_mono: params.force_mono,
    })
}

fn sky_to_abi(sky: ProceduralSkyComponent) -> RveProceduralSky {
    RveProceduralSky {
        zenith_color: sky.zenith_color,
        horizon_color: sky.horizon_color,
        ground_color: sky.ground_color,
        sun_size_deg: sky.sun_size_deg,
        sun_intensity: sky.sun_intensity,
    }
}

fn sky_from_abi(sky: RveProceduralSky) -> ProceduralSkyComponent {
    ProceduralSkyComponent {
        zenith_color: sky.zenith_color,
        horizon_color: sky.horizon_color,
        ground_color: sky.ground_color,
        sun_size_deg: sky.sun_size_deg,
        sun_intensity: sky.sun_intensity,
    }
}

fn audio_group_code(group: AudioGroup) -> u32 {
    match group {
        AudioGroup::Master => 0,
        AudioGroup::Music => 1,
        AudioGroup::Effects => 2,
        AudioGroup::Ambient => 3,
    }
}

fn audio_source_to_abi(audio: &AudioSourceComponent) -> RveAudioSourceState {
    RveAudioSourceState {
        group: audio_group_code(audio.group),
        volume: audio.volume,
        looping: audio.looping,
        play_on_start: audio.play_on_start,
        spatial_blend: audio.spatial_blend,
        min_distance: audio.min_distance,
        max_distance: audio.max_distance,
        rolloff: match audio.rolloff {
            AudioRolloff::Inverse => RVE_AUDIO_ROLLOFF_INVERSE,
            AudioRolloff::Linear => RVE_AUDIO_ROLLOFF_LINEAR,
        },
        pitch: audio.pitch,
        force_mono: audio.force_mono,
    }
}

/// Applies ABI values over the current settings; the clip is kept from `current`.
fn audio_source_from_abi(
    current: AudioSourceComponent,
    value: RveAudioSourceState,
) -> Result<AudioSourceComponent, String> {
    let rolloff = match value.rolloff {
        RVE_AUDIO_ROLLOFF_INVERSE => AudioRolloff::Inverse,
        RVE_AUDIO_ROLLOFF_LINEAR => AudioRolloff::Linear,
        other => return Err(format!("unknown audio rolloff {other}")),
    };
    Ok(AudioSourceComponent {
        clip: current.clip,
        group: audio_group(value.group)?,
        volume: value.volume,
        looping: value.looping,
        play_on_start: value.play_on_start,
        spatial_blend: value.spatial_blend,
        min_distance: value.min_distance,
        max_distance: value.max_distance,
        rolloff,
        pitch: value.pitch,
        force_mono: value.force_mono,
    })
}

pub(super) unsafe extern "C" fn host_mesh_get_visible(
    ctx: *mut c_void,
    object: u64,
    out: *mut bool,
) -> bool {
    object_bool(ctx, object, out, ScriptTransaction::mesh_visible)
}

pub(super) unsafe extern "C" fn host_particles_get_playing(
    ctx: *mut c_void,
    object: u64,
    out: *mut bool,
) -> bool {
    object_bool(ctx, object, out, ScriptTransaction::particles_playing)
}
