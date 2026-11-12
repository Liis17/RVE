//! Сборка `FrameSnapshot`: история движения, анимация, свет, окружение и постобработка.

use super::*;

#[derive(Default)]
pub(crate) struct MotionHistory {
    pub(crate) active_camera: Option<ObjectHandle>,
    pub(crate) previous_models: HashMap<ObjectHandle, [[f32; 4]; 4]>,
}

impl MotionHistory {
    pub(crate) fn previous_model(
        &self,
        object: ObjectHandle,
        camera_changed: bool,
    ) -> Option<[[f32; 4]; 4]> {
        if camera_changed {
            None
        } else {
            self.previous_models.get(&object).copied()
        }
    }

    pub(crate) fn prepare(&self, snapshot: &mut FrameSnapshot) {
        let camera_changed = self
            .active_camera
            .is_some_and(|previous| previous != snapshot.active_camera);
        snapshot.reset_history = camera_changed;
        for (object, motion) in &snapshot.object_motion {
            let previous_model = self.previous_model(*object, camera_changed);
            for draw in &mut snapshot.draws[motion.draw_range.clone()] {
                draw.previous_model = previous_model;
            }
        }
    }

    pub(crate) fn submitted(&mut self, snapshot: &FrameSnapshot) {
        self.previous_models.clear();
        for (object, motion) in &snapshot.object_motion {
            self.previous_models.insert(*object, motion.model);
        }
        self.active_camera = Some(snapshot.active_camera);
    }
}

/// Включённые проходы камеры, для которых создан GPU-шейдер, в порядке файла сцены. Шейдер без
/// созданного ресурса (ошибка загрузки) пропускается; остальные сохраняют порядок и стадию.
pub(crate) fn enabled_screen_passes<T: Copy>(
    shaders: &[engine_scene::ScreenShaderRef],
    ids: &HashMap<String, T>,
) -> [Option<(
    T,
    engine_render::ScreenShaderStage,
    [f32; engine_render::SCREEN_SHADER_PARAM_COUNT],
)>; engine_render::SCREEN_SHADER_MAX_PASSES] {
    let mut passes = [None; engine_render::SCREEN_SHADER_MAX_PASSES];
    let known = shaders
        .iter()
        .filter(|shader| shader.enabled)
        .filter_map(|shader| Some((ids.get(shader.shader.as_str())?, shader)));
    for (slot, (id, shader)) in known.take(passes.len()).enumerate() {
        let mut params = [0.0_f32; engine_render::SCREEN_SHADER_PARAM_COUNT];
        for (value, parameter) in params.iter_mut().zip(&shader.params) {
            *value = *parameter;
        }
        let stage = match shader.stage {
            engine_scene::ScreenShaderStage::BeforePost => {
                engine_render::ScreenShaderStage::BeforePost
            }
            engine_scene::ScreenShaderStage::AfterPost => {
                engine_render::ScreenShaderStage::AfterPost
            }
        };
        passes[slot] = Some((*id, stage, params));
    }
    passes
}

/// Экранные шейдеры вида для кадра; время задаёт вызывающий код.
pub(crate) fn screen_shader_view(
    shaders: &[engine_scene::ScreenShaderRef],
    ids: &HashMap<String, engine_render::ScreenShaderId>,
) -> engine_render::ScreenShaders {
    let mut view = engine_render::ScreenShaders::default();
    for (target, pass) in view
        .passes
        .iter_mut()
        .zip(enabled_screen_passes(shaders, ids))
    {
        *target = pass.map(|(shader, stage, params)| engine_render::ScreenShaderPass {
            shader,
            stage,
            params,
        });
    }
    view
}

/// LUT активной камеры по пути из её постобработки; ключи таблицы — канонические пути без
/// ведущего `/`, поэтому поиск не выделяет память.
pub(crate) fn scene_color_lut(
    post: &PostProcessSettings,
    luts: &HashMap<String, ColorLutId>,
) -> Option<ColorLutId> {
    let path = post.color_grading.lut.as_deref()?;
    luts.get(path.strip_prefix('/').unwrap_or(path)).copied()
}

pub(crate) fn make_snapshot(
    scene: engine_scene::SceneFrame,
    world: &World,
    gpu_scene: &GpuSceneTable,
    animation: &AnimationBridge,
    lod: &mut LodSelector,
) -> Result<FrameSnapshot, String> {
    let mut draws = Vec::new();
    let mut joint_palettes = Vec::new();
    let camera_position = scene.camera.position;
    let fov_narrow = scene.camera.fov_narrow;
    let mut object_motion = HashMap::new();
    for instance in scene.meshes {
        let Some(levels) = gpu_scene.by_object.get(&instance.object) else {
            continue;
        };
        let Some((lod_level, meshes)) = selected_level(
            lod,
            world,
            instance.object,
            camera_position,
            fov_narrow,
            levels,
        ) else {
            continue;
        };
        let model = instance.model.to_cols_array_2d();
        let draw_start = draws.len();
        // Анимируемый объект рисуется экземплярами skinning выбранного уровня, пока у моста есть
        // палитра; иначе — bind-pose мешем без скиннинга.
        let palette = gpu_scene
            .skin_instances
            .get(&instance.object)
            .and_then(|skins| skins.get(usize::from(lod_level)))
            .filter(|skin| !skin.instances.is_empty())
            .zip(animation.palette(instance.object))
            .and_then(|(skin, palette)| {
                let palette = level_palette(skin.remap.as_deref(), palette)?;
                joint_palettes.push(palette);
                Some((&skin.instances, (joint_palettes.len() - 1) as u32))
            });
        for (index, (mesh, material)) in meshes.iter().enumerate() {
            let skin = palette.and_then(|(instances, palette)| {
                instances
                    .get(index)
                    .map(|&instance| SkinBinding { instance, palette })
            });
            draws.push(DrawCommand {
                mesh: *mesh,
                material: Some(*material),
                model,
                previous_model: None,
                lod_level,
                casts_shadow: true,
                skin,
            });
        }
        if draw_start != draws.len() {
            object_motion.insert(
                instance.object,
                ObjectMotion {
                    model,
                    draw_range: draw_start..draws.len(),
                },
            );
        }
    }
    let environment = resolve_environment_snapshot(scene.environment.as_ref(), gpu_scene)?;
    let (local_lights, local_light_shadows) = render_local_lights(&scene.local_lights);
    let particles = scene
        .particle_emitters
        .iter()
        .map(|instance| ParticleSnapshot {
            object: instance.object,
            transform: instance.model.to_cols_array_2d(),
            emitting: instance.emitting,
        })
        .collect();
    Ok(FrameSnapshot {
        sequence: 0,
        camera: scene.camera,
        active_camera: world.active_camera(),
        view: scene.view,
        color_lut: scene_color_lut(&scene.post_process, &gpu_scene.color_luts),
        post_process: scene.post_process,
        screen_shaders: screen_shader_view(&scene.screen_shaders, &gpu_scene.screen_shaders),
        reset_history: false,
        light: scene.light,
        local_lights,
        local_light_shadows,
        environment,
        draws,
        particles,
        particle_commands: Vec::new(),
        sim_time: 0.0,
        joint_palettes,
        ui: empty_paint(),
        object_motion,
    })
}

/// Палитры кадра как срезы для `Frame::joint_palettes`.
pub(crate) fn palette_refs(snapshot: &FrameSnapshot) -> Vec<&[[[f32; 4]; 4]]> {
    snapshot.joint_palettes.iter().map(Vec::as_slice).collect()
}

/// Мост анимации для потока скриптов: модели и клипы берутся из GPU-таблицы.
pub(crate) fn build_animation(world: &World, gpu_scene: &GpuSceneTable) -> AnimationBridge {
    let mut animation = AnimationBridge::build(
        world,
        |path| {
            gpu_scene
                .animation_models
                .get(path.as_str())
                .cloned()
                .ok_or_else(|| format!("модель {} не подготовлена для анимации", path.as_str()))
        },
        |path| {
            gpu_scene
                .animation_clips
                .get(path.as_str())
                .cloned()
                .ok_or_else(|| format!("клип {} не загружен", path.as_str()))
        },
    );
    log_animation_warnings(&mut animation);
    animation
}

/// Убирает skinning у объектов, чья палитра моста не совпала с числом костей меша: такой объект
/// рисуется статично и не валит кадр. Экземпляры остаются у renderer до его завершения.
pub(crate) fn drop_mismatched_skins(
    world: &World,
    gpu_scene: &mut GpuSceneTable,
    animation: &AnimationBridge,
) -> Vec<ObjectHandle> {
    let mut mismatched: Vec<ObjectHandle> = gpu_scene
        .joint_counts
        .iter()
        .filter(|(object, joints)| {
            animation
                .palette(**object)
                .is_some_and(|palette| palette.len() != **joints as usize)
        })
        .map(|(object, _)| *object)
        .collect();
    mismatched.sort_by_key(|object| object.index());
    for object in &mismatched {
        gpu_scene.skin_instances.remove(object);
        let id = world.object(*object).map_or_else(
            || format!("объект #{}", object.index()),
            |item| item.id.clone(),
        );
        log::warn!(
            "[animation] {id}: число костей палитры не совпадает с мешем; объект рисуется без скиннинга"
        );
    }
    mismatched
}

pub(crate) fn log_animation_warnings(animation: &mut AnimationBridge) {
    for warning in animation.take_warnings() {
        log::warn!("[animation] {warning}");
    }
}

/// Время для `rve.time` экранных шейдеров: остаток от 3600 считается в f64, иначе после долгой
/// игры f32 округляет дробную часть секунд.
pub(crate) fn screen_shader_time(sim_time: f64) -> f32 {
    sim_time.rem_euclid(3600.0) as f32
}

pub(crate) fn render_local_lights(
    states: &[LocalLightState],
) -> (Vec<LocalLight>, Vec<LocalLightShadow>) {
    let mut lights = Vec::with_capacity(states.len());
    let mut shadows = Vec::new();
    for (light_index, state) in states.iter().enumerate() {
        match *state {
            LocalLightState::Point {
                position,
                color,
                intensity,
                range,
                shadows: casts_shadow,
            } => {
                lights.push(LocalLight::Point {
                    position,
                    color_intensity: color.map(|component| {
                        (f64::from(component) * f64::from(intensity)).min(f64::from(f32::MAX))
                            as f32
                    }),
                    range,
                });
                if casts_shadow
                    && let Some(matrices) =
                        point_shadow_view_projections(Vec3::from_array(position), range)
                {
                    shadows.push(LocalLightShadow {
                        light_index,
                        matrices: LocalLightShadowMatrices::Point {
                            matrices: matrices.map(|matrix| matrix.to_cols_array_2d()),
                        },
                    });
                }
            }
            LocalLightState::Spot {
                position,
                direction,
                color,
                intensity,
                range,
                inner_angle_deg,
                outer_angle_deg,
                shadows: casts_shadow,
            } => {
                lights.push(LocalLight::Spot {
                    position,
                    direction,
                    color_intensity: color.map(|component| {
                        (f64::from(component) * f64::from(intensity)).min(f64::from(f32::MAX))
                            as f32
                    }),
                    range,
                    cos_inner_angle: inner_angle_deg.to_radians().cos().max(0.0),
                    cos_outer_angle: outer_angle_deg.to_radians().cos().max(0.0),
                });
                if casts_shadow
                    && let Some(matrix) = spot_shadow_view_projection(
                        Vec3::from_array(position),
                        Vec3::from_array(direction),
                        inner_angle_deg,
                        outer_angle_deg,
                        range,
                    )
                {
                    shadows.push(LocalLightShadow {
                        light_index,
                        matrices: LocalLightShadowMatrices::Spot {
                            matrix: matrix.to_cols_array_2d(),
                        },
                    });
                }
            }
        }
    }
    (lights, shadows)
}

pub(crate) fn resolve_environment_snapshot(
    state: Option<&EnvironmentState>,
    gpu_scene: &GpuSceneTable,
) -> Result<Option<SceneEnvironmentSnapshot>, String> {
    let Some(state) = state else {
        return Ok(None);
    };
    let component = &state.component;
    let (key, source) = match (&component.hdr, component.procedural) {
        (Some(path), None) => {
            let path = path.as_str().to_owned();
            let source = gpu_scene
                .environments
                .get(&path)
                .cloned()
                .ok_or_else(|| format!("не загружены HDR данные окружения {path}"))?;
            (
                EnvironmentCacheKey::Hdr(path.clone()),
                EnvironmentSnapshotSource::Hdr(source),
            )
        }
        (None, Some(procedural)) => {
            let world_sun = state.sun_direction.unwrap_or([0.0, 1.0, 0.0]);
            let world_sun = Vec3::from_array(world_sun).normalize_or_zero();
            let rotation = Mat3::from_cols_array_2d(&state.rotation);
            let local_sun = (rotation.transpose() * world_sun).normalize_or_zero();
            let sky = ProceduralSky {
                zenith_color: procedural.zenith_color,
                horizon_color: procedural.horizon_color,
                ground_color: procedural.ground_color,
                sun_size_deg: procedural.sun_size_deg,
                sun_intensity: procedural.sun_intensity,
                sun_direction: local_sun.to_array(),
            };
            (
                procedural_environment_key(sky),
                EnvironmentSnapshotSource::Procedural(sky),
            )
        }
        _ => return Err("окружение должно иметь ровно один источник".to_owned()),
    };
    Ok(Some(SceneEnvironmentSnapshot {
        key,
        source,
        intensity: component.intensity,
        rotation: state.rotation,
        skybox: component.skybox,
    }))
}

pub(crate) fn procedural_environment_key(sky: ProceduralSky) -> EnvironmentCacheKey {
    let colors_and_sun = [
        sky.zenith_color[0].to_bits(),
        sky.zenith_color[1].to_bits(),
        sky.zenith_color[2].to_bits(),
        sky.horizon_color[0].to_bits(),
        sky.horizon_color[1].to_bits(),
        sky.horizon_color[2].to_bits(),
        sky.ground_color[0].to_bits(),
        sky.ground_color[1].to_bits(),
        sky.ground_color[2].to_bits(),
        sky.sun_size_deg.to_bits(),
        sky.sun_intensity.to_bits(),
    ];
    let sun_direction = sky
        .sun_direction
        .map(|value| (value * 4096.0).round() as i32);
    EnvironmentCacheKey::Procedural {
        colors_and_sun,
        sun_direction,
    }
}

pub(crate) fn selected_level<'a, T>(
    lod: &mut LodSelector,
    world: &World,
    object: ObjectHandle,
    eye: Vec3,
    fov_narrow: f32,
    levels: &'a [T],
) -> Option<(u8, &'a T)> {
    let level = lod.select(world, object, eye, fov_narrow)?;
    Some((level.try_into().ok()?, levels.get(level)?))
}

/// Палитра уровня LOD из палитры моста: уровень 0 берёт её как есть, остальные — через перевод
/// на свой скелет. `None` — длина палитры не совпала со скелетом перевода.
pub(crate) fn level_palette(
    remap: Option<&PaletteRemap>,
    palette: &[[[f32; 4]; 4]],
) -> Option<Vec<[[f32; 4]; 4]>> {
    let Some(remap) = remap else {
        return Some(palette.to_vec());
    };
    let mut out = Vec::with_capacity(remap.joint_count());
    remap.apply(palette, &mut out).then_some(out)
}

/// Переводит постобработку камеры в настройки renderer.
///
/// Выключенная экспозиция становится ручной с EV 0, выключенный tonemapping — `None`. Выключенные
/// эффекты сохраняют параметры, но renderer их не выполняет, поэтому `enabled = false` совпадает
/// с тождественным преобразованием.
pub fn render_post_process(post: &PostProcessSettings, lut: Option<ColorLutId>) -> PostProcess {
    let exposure = if post.exposure.enabled {
        Exposure {
            mode: match post.exposure.mode {
                engine_scene::ExposureMode::Manual => engine_render::ExposureMode::Manual,
                engine_scene::ExposureMode::Auto => engine_render::ExposureMode::Auto,
            },
            ev: post.exposure.ev,
            min_ev: post.exposure.min_ev,
            max_ev: post.exposure.max_ev,
            speed_up: post.exposure.speed_up,
            speed_down: post.exposure.speed_down,
            low_percent: post.exposure.low_percent,
            high_percent: post.exposure.high_percent,
        }
    } else {
        Exposure::default()
    };
    let tonemapper = if post.tonemapping.enabled {
        match post.tonemapping.tonemapper {
            Tonemapper::Aces => RenderTonemapper::Aces,
            Tonemapper::Neutral => RenderTonemapper::Neutral,
            Tonemapper::None => RenderTonemapper::None,
        }
    } else {
        RenderTonemapper::None
    };
    let quality = match post.ambient_occlusion.quality {
        engine_scene::AmbientOcclusionQuality::Low => engine_render::AmbientOcclusionQuality::Low,
        engine_scene::AmbientOcclusionQuality::Medium => {
            engine_render::AmbientOcclusionQuality::Medium
        }
        engine_scene::AmbientOcclusionQuality::High => engine_render::AmbientOcclusionQuality::High,
    };
    let bloom = post.bloom;
    let ao = post.ambient_occlusion;
    let grading = &post.color_grading;
    let vignette = post.vignette;
    let aberration = post.chromatic_aberration;
    let grain = post.film_grain;
    let dof = post.depth_of_field;
    let motion = post.motion_blur;
    PostProcess {
        exposure,
        tonemapper,
        bloom: engine_render::Bloom {
            enabled: bloom.enabled,
            intensity: bloom.intensity,
            threshold: bloom.threshold,
            knee: bloom.knee,
            scatter: bloom.scatter,
            tint: bloom.tint,
        },
        ambient_occlusion: engine_render::AmbientOcclusion {
            enabled: ao.enabled,
            radius: ao.radius,
            intensity: ao.intensity,
            falloff: ao.falloff,
            quality,
        },
        color_grading: engine_render::ColorGrading {
            enabled: grading.enabled,
            temperature: grading.temperature,
            tint: grading.tint,
            contrast: grading.contrast,
            saturation: grading.saturation,
            lift: grading.lift,
            gamma: grading.gamma,
            gain: grading.gain,
            lut,
            lut_intensity: grading.lut_intensity,
        },
        vignette: engine_render::Vignette {
            enabled: vignette.enabled,
            intensity: vignette.intensity,
            smoothness: vignette.smoothness,
            roundness: vignette.roundness,
            color: vignette.color,
        },
        chromatic_aberration: engine_render::ChromaticAberration {
            enabled: aberration.enabled,
            intensity: aberration.intensity,
        },
        film_grain: engine_render::FilmGrain {
            enabled: grain.enabled,
            intensity: grain.intensity,
            response: grain.response,
        },
        depth_of_field: engine_render::DepthOfField {
            enabled: dof.enabled,
            focus_distance: dof.focus_distance,
            aperture: dof.aperture,
            focal_length_mm: dof.focal_length_mm,
            max_blur_px: dof.max_blur_px,
        },
        motion_blur: engine_render::MotionBlur {
            enabled: motion.enabled,
            shutter_angle_deg: motion.shutter_angle_deg,
            samples: motion.samples,
            max_blur_px: motion.max_blur_px,
        },
    }
}
