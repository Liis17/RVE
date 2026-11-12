//! Vulkan-реализация `RenderBackend`: загрузка ресурсов сцены, префабов, материалов и отрисовка кадра.

use super::*;

/// Ресурсы эмиттеров тяжёлого префаба на render thread.
#[derive(Default)]
pub(crate) struct PrefabParticles {
    pub(crate) textures: Vec<TextureId>,
    /// Отдаются с `PrefabReady` и затем обнуляются.
    pub(crate) descs: Option<Vec<Option<ParticleEmitterDesc>>>,
}

pub(crate) struct VulkanBackend {
    pub(crate) renderer: Renderer,
    /// GPU-ресурсы тяжёлых префабов; `Renderer` выполняет создание и освобождение.
    pub(crate) prefabs: PrefabGpu<Renderer>,
    pub(crate) debug_view: DebugView,
    pub(crate) current_environment: Option<(EnvironmentCacheKey, EnvironmentId)>,
    pub(crate) pipeline_cache_path: Option<std::path::PathBuf>,
    /// Игровое время последнего отправленного кадра: шаг частиц считается от него.
    pub(crate) last_submitted_sim_time: Option<f64>,
    /// Эмиттеры частиц по хендлам объектов: сцены при загрузке, spawn-объекты по заявкам.
    pub(crate) particle_emitters: HashMap<ObjectHandle, ParticleEmitterId>,
    /// Текстуры тяжёлых префабов: описания ждут готовности, текстуры живут до освобождения.
    pub(crate) prefab_particles: HashMap<PrefabToken, PrefabParticles>,
    #[cfg(feature = "dev-console")]
    pub(crate) console_ui: Option<Arc<ConsoleUiSlot>>,
    #[cfg(feature = "dev-console")]
    pub(crate) ui_bridge: UiBridge,
    #[cfg(feature = "dev-console")]
    pub(crate) console_viewport: Option<(ViewportId, u32, u32)>,
    /// Экземпляры skinning сцены, которые освобождаются в `release_scene`.
    pub(crate) scene_skin_instances: Vec<SkinId>,
    /// GPU-ресурсы игрового UI: атлас, картинки по пути и буфер вершин.
    pub(crate) game_ui: GameUi,
    pub(crate) scene_screen_shaders: Vec<engine_render::ScreenShaderId>,
    /// LUT цветокоррекции стартовой сцены; освобождаются в `release_scene`, если их не освободила замена.
    pub(crate) scene_color_luts: Vec<ColorLutId>,
    /// Подготовленные ресурсы сцены для замены материалов объектов; нет у тестовых сцен без GPU.
    pub(crate) material_assets: Option<Arc<PreparedRenderAssets>>,
    /// CPU material sources for heavy prefabs, retained until that token's delayed release.
    pub(crate) prefab_material_assets: HashMap<PrefabToken, Arc<PreparedMaterialAssets>>,
    /// Текстуры, шейдеры и запасной материал сцены, которые переиспользуют замены материалов.
    pub(crate) material_caches: MaterialUploadCaches,
    /// Пути запрошенных скриншотов окна; `None` — скриншоты отключены.
    pub(crate) screenshots: Option<Receiver<PathBuf>>,
    /// Захваты окна, ждущие readback, и пути их PNG.
    pub(crate) pending_screenshots: Vec<(engine_render::CaptureTicket, PathBuf)>,
}

#[cfg(feature = "dev-console")]
pub(crate) type PendingConsoleCommands = VecDeque<CommandLine>;

#[cfg(not(feature = "dev-console"))]
pub(crate) type PendingConsoleCommands = ();

impl VulkanBackend {
    pub fn new(
        renderer: Renderer,
        pipeline_cache_path: Option<std::path::PathBuf>,
        debug_view: DebugView,
    ) -> Self {
        Self {
            renderer,
            prefabs: PrefabGpu::default(),
            debug_view,
            current_environment: None,
            pipeline_cache_path,
            last_submitted_sim_time: None,
            particle_emitters: HashMap::new(),
            prefab_particles: HashMap::new(),
            #[cfg(feature = "dev-console")]
            console_ui: None,
            #[cfg(feature = "dev-console")]
            ui_bridge: UiBridge::default(),
            #[cfg(feature = "dev-console")]
            console_viewport: None,
            scene_skin_instances: Vec::new(),
            game_ui: GameUi::default(),
            scene_screen_shaders: Vec::new(),
            scene_color_luts: Vec::new(),
            material_assets: None,
            prefab_material_assets: HashMap::new(),
            material_caches: MaterialUploadCaches::default(),
            screenshots: None,
            pending_screenshots: Vec::new(),
        }
    }

    /// Подключает очередь скриншотов окна: каждый путь получает захват следующего кадра.
    pub fn with_screenshots(mut self, screenshots: Receiver<PathBuf>) -> Self {
        self.screenshots = Some(screenshots);
        self
    }

    pub(crate) fn queue_screenshots(&mut self) {
        let Some(screenshots) = &self.screenshots else {
            return;
        };
        for path in screenshots.try_iter() {
            let ticket = self.renderer.request_window_capture();
            self.pending_screenshots.push((ticket, path));
        }
    }

    /// Отдаёт готовые захваты на запись PNG; не ждёт GPU.
    pub(crate) fn finish_screenshots(&mut self) {
        if self.pending_screenshots.is_empty() {
            return;
        }
        for (ticket, result) in self.renderer.take_captures() {
            let Some(index) = self
                .pending_screenshots
                .iter()
                .position(|(pending, _)| *pending == ticket)
            else {
                continue;
            };
            let (_, path) = self.pending_screenshots.swap_remove(index);
            match result {
                Ok(image) => crate::platform::save_screenshot(path, image),
                Err(error) => log::error!("Скриншот {} не снят: {error}", path.display()),
            }
        }
    }

    /// Передаёт ресурсы сцены и GPU-кэши, которые остались после загрузки: по ним создаются
    /// материалы при замене скриптом.
    pub fn with_material_uploads(
        mut self,
        assets: Arc<PreparedRenderAssets>,
        caches: MaterialUploadCaches,
    ) -> Self {
        self.material_assets = Some(assets);
        self.material_caches = caches;
        self
    }

    /// Создаёт материалы объекта по текущему состоянию мира. Меши и уровни LOD не меняются; новые
    /// материалы принадлежат только этому ответу, и при ошибке созданные уничтожаются.
    pub(crate) fn upload_object_materials(
        &mut self,
        request: MaterialRequest,
    ) -> Result<MaterialLevels, String> {
        let MaterialRequest {
            levels,
            model,
            prefab_token,
            ..
        } = request;
        let assets = match prefab_token.as_ref() {
            Some(token) => MaterialAssetView::from_prefab(
                self.prefab_material_assets
                    .get(token)
                    .ok_or("CPU-ресурсы токена префаба уже освобождены")?,
                self.material_assets.as_deref(),
            ),
            None => MaterialAssetView::from_scene(
                self.material_assets
                    .as_deref()
                    .ok_or("ресурсы сцены не переданы render thread")?,
            ),
        };
        let gpu_models: HashMap<String, Vec<(MeshId, MaterialId)>> = model
            .levels
            .iter()
            .zip(&levels)
            .map(|((model_path, _), meshes)| (model_path.clone(), meshes.clone()))
            .collect();
        let mut material_cache = HashMap::new();
        let mut material_dependencies = HashMap::new();
        let uploaded = upload_object_levels(
            &mut self.renderer,
            assets,
            &gpu_models,
            &mut self.material_caches.textures,
            &mut self.material_caches.shaders,
            &mut material_cache,
            &mut self.material_caches.error_material,
            Some(&mut material_dependencies),
            &model,
        );
        let created: Vec<MaterialId> = material_cache.into_values().collect();
        match uploaded {
            Ok(levels) => {
                self.material_caches.retain_materials(material_dependencies);
                self.material_caches.prune_unowned(&mut self.renderer);
                Ok(MaterialLevels { levels, created })
            }
            Err(error) => {
                self.destroy_materials(&created);
                self.material_caches.prune_unowned(&mut self.renderer);
                Err(error.to_string())
            }
        }
    }

    pub(crate) fn destroy_materials(&mut self, materials: &[MaterialId]) {
        for &material in materials {
            if Some(material) == self.material_caches.error_material {
                continue;
            }
            if let Err(error) = self.renderer.destroy_material(material) {
                log::error!("материал не уничтожен: {error}");
            }
        }
    }

    /// Принимает эмиттеры объектов сцены, созданные при загрузке.
    pub fn with_particle_emitters(
        mut self,
        emitters: HashMap<ObjectHandle, ParticleEmitterId>,
    ) -> Self {
        self.particle_emitters = emitters;
        self
    }

    #[cfg(feature = "dev-console")]
    pub fn with_console(mut self, console_ui: Arc<ConsoleUiSlot>) -> Self {
        self.console_ui = Some(console_ui);
        self
    }

    pub fn with_scene_screen_shaders(
        mut self,
        shaders: Vec<engine_render::ScreenShaderId>,
    ) -> Self {
        self.scene_screen_shaders = shaders;
        self
    }

    pub(crate) fn with_scene_color_luts(mut self, luts: Vec<ColorLutId>) -> Self {
        self.scene_color_luts = luts;
        self
    }

    pub fn with_scene_skin_instances(mut self, instances: Vec<SkinId>) -> Self {
        self.scene_skin_instances = instances;
        self
    }

    pub fn with_ui_images(mut self, images: HashMap<String, UiTextureId>) -> Self {
        self.game_ui = GameUi::with_scene_images_for_owner(0, images);
        self
    }

    pub(crate) fn frame_lighting(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameLighting, String> {
        let light = DirectionalLight {
            direction: snapshot.light.direction,
            color: snapshot.light.color,
            ambient: snapshot.light.ambient,
        };
        let shadow_cascades = snapshot.light.shadow.map_or_else(Vec::new, |settings| {
            cascade_view_projections(
                snapshot.camera,
                width.max(1) as f32 / height.max(1) as f32,
                Vec3::from_array(snapshot.light.direction),
                settings,
                SHADOW_MAP_RESOLUTION,
            )
            .into_iter()
            .map(|matrix| matrix.to_cols_array_2d())
            .collect()
        });
        let environment = match &snapshot.environment {
            Some(environment) => {
                let id = match &self.current_environment {
                    Some((key, id)) if key == &environment.key => *id,
                    _ => {
                        let id = match &environment.source {
                            EnvironmentSnapshotSource::Hdr(source) => {
                                self.renderer.create_environment(source)
                            }
                            EnvironmentSnapshotSource::Procedural(sky) => self
                                .renderer
                                .create_environment(&EnvironmentSource::Procedural(*sky)),
                        }
                        .map_err(|error| error.to_string())?;
                        if let Some((_, previous)) = self.current_environment.take() {
                            self.renderer
                                .destroy_environment(previous)
                                .map_err(|error| error.to_string())?;
                        }
                        self.current_environment = Some((environment.key.clone(), id));
                        id
                    }
                };
                Some(FrameEnvironment {
                    id,
                    intensity: environment.intensity,
                    rotation: environment.rotation,
                    skybox: environment.skybox,
                })
            }
            None => {
                if let Some((_, id)) = self.current_environment.take() {
                    self.renderer
                        .destroy_environment(id)
                        .map_err(|error| error.to_string())?;
                }
                None
            }
        };
        Ok((light, shadow_cascades, environment))
    }
}

impl RenderBackend for VulkanBackend {
    fn take_frame_profile(&mut self) -> Option<FrameProfile> {
        self.renderer.take_frame_profile()
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.renderer.resize(width, height);
    }

    fn submit_prefab(&mut self, mut prefab: LoadedPrefab) -> Option<PrefabReply> {
        let token = prefab.token.clone();
        let material_assets = Arc::clone(&prefab.material_assets);
        if let Err(error) =
            self.game_ui
                .attach(&mut self.renderer, token.clone(), &prefab.cpu.ui.images)
        {
            return Some(PrefabReply::Failed {
                token,
                error: error.to_string(),
            });
        }
        let particles = std::mem::take(&mut prefab.particles);
        let created = match create_prefab_particles(
            &mut self.renderer,
            &prefab.material_assets.textures,
            &particles,
        ) {
            Ok(created) => created,
            Err(error) => {
                self.game_ui.detach(&mut self.renderer, &token);
                return Some(PrefabReply::Failed {
                    token,
                    error: error.to_string(),
                });
            }
        };
        let reply = self.prefabs.submit(prefab);
        if reply.is_some() {
            release_prefab_particles(&mut self.renderer, created.textures);
            self.game_ui.detach(&mut self.renderer, &token);
        } else {
            self.prefab_particles.insert(token.clone(), created);
            self.prefab_material_assets.insert(token, material_assets);
        }
        reply
    }

    fn advance_prefab_uploads(&mut self, budget: usize) -> Vec<PrefabReply> {
        let mut replies = self.prefabs.advance(&mut self.renderer, budget);
        for reply in &mut replies {
            match reply {
                PrefabReply::Ready(ready) => {
                    if let Some(entry) = self.prefab_particles.get_mut(&ready.token) {
                        ready.particles = entry.descs.take().unwrap_or_default();
                    }
                }
                PrefabReply::Failed { token, .. } => {
                    self.game_ui.detach(&mut self.renderer, token);
                    if let Some(entry) = self.prefab_particles.remove(token) {
                        release_prefab_particles(&mut self.renderer, entry.textures);
                    }
                    self.prefab_material_assets.remove(token);
                }
            }
        }
        replies
    }

    fn has_pending_prefab_uploads(&self) -> bool {
        self.prefabs.has_pending()
    }

    fn release_prefab(&mut self, token: &PrefabToken) {
        self.prefabs.release(&mut self.renderer, token);
        self.prefab_material_assets.remove(token);
        self.game_ui.detach(&mut self.renderer, token);
        if let Some(entry) = self.prefab_particles.remove(token) {
            release_prefab_particles(&mut self.renderer, entry.textures);
        }
        self.material_caches.prune_unowned(&mut self.renderer);
    }

    fn submit_material(&mut self, request: MaterialRequest) -> MaterialReply {
        let object = request.object;
        let revision = request.revision;
        let result = self.upload_object_materials(request);
        MaterialReply {
            object,
            revision,
            result,
        }
    }

    fn submit_scene(&mut self, upload: SceneUpload) -> SceneUploadReply {
        let SceneUpload { generation, assets } = upload;
        let result = match upload_scene(
            &mut self.renderer,
            &assets,
            &mut self.material_caches,
            SceneParticles::Descriptions,
            generation,
        ) {
            Ok(mut table) => {
                self.scene_skin_instances.extend(table.skin_instance_ids());
                self.scene_screen_shaders.extend(table.screen_shader_ids());
                self.game_ui.adopt_scene_images_for_owner(
                    &mut self.renderer,
                    generation,
                    std::mem::take(&mut table.ui_images),
                );
                table.ui_scene_owners.clear();
                table.ui_scene_owners.insert(generation);
                self.material_assets = Some(assets);
                Ok(Box::new(table))
            }
            Err(error) => Err(error.to_string()),
        };
        SceneUploadReply { generation, result }
    }

    fn release_scene_resources(&mut self, ids: &SceneGpuIds) {
        // Запасной материал кэша переиспользуется следующими загрузками: его не уничтожаем.
        let cached_error_material = self.material_caches.error_material;
        for skin in &ids.skins {
            if let Err(error) = self.renderer.destroy_skin_instance(*skin) {
                log::error!("экземпляр skinning сцены не уничтожен: {error}");
            }
        }
        self.scene_skin_instances
            .retain(|skin| !ids.skins.contains(skin));
        for mesh in &ids.meshes {
            if let Err(error) = self.renderer.destroy_mesh(*mesh) {
                log::error!("меш сцены не уничтожен: {error}");
            }
        }
        for material in &ids.materials {
            if Some(*material) == cached_error_material {
                continue;
            }
            if let Err(error) = self.renderer.destroy_material(*material) {
                log::error!("материал сцены не уничтожен: {error}");
            }
        }
        self.material_caches
            .release_materials(&mut self.renderer, ids.materials.iter().copied());
        self.material_caches
            .release_scene_textures(&mut self.renderer, ids.cache_owners.iter().copied());
        self.material_caches.prune_unowned(&mut self.renderer);
        for owner in &ids.ui_scene_owners {
            self.game_ui
                .release_scene_images(&mut self.renderer, *owner);
        }
        for shader in &ids.screen_shaders {
            if let Err(error) = self.renderer.destroy_screen_shader(*shader) {
                log::error!("экранный шейдер сцены не уничтожен: {error}");
            }
        }
        // Шейдер, уничтоженный здесь, не должен уйти повторно при выгрузке (`release_scene`).
        self.scene_screen_shaders
            .retain(|shader| !ids.screen_shaders.contains(shader));
        for lut in &ids.color_luts {
            if let Err(error) = self.renderer.destroy_color_lut(*lut) {
                log::error!("LUT сцены не уничтожена: {error}");
            }
        }
        self.scene_color_luts
            .retain(|lut| !ids.color_luts.contains(lut));
    }

    fn release_materials(&mut self, materials: &[MaterialId]) {
        self.destroy_materials(materials);
        self.material_caches
            .release_materials(&mut self.renderer, materials.iter().copied());
    }

    fn apply_particle_commands(&mut self, commands: &[engine_scene::world::ParticleCommand]) {
        for (emitter, burst) in resolve_particle_commands(&self.particle_emitters, commands) {
            let outcome = match burst {
                None => self.renderer.restart_particle_emitter(emitter),
                Some(count) => self.renderer.emit_particle_burst(emitter, count),
            };
            if let Err(error) = outcome {
                log::error!("команда эмиттера частиц не применена: {error}");
            }
        }
    }

    fn apply_particle_request(&mut self, request: ParticleRequest) {
        apply_emitter_request(
            &mut self.particle_emitters,
            &mut self.renderer,
            request,
            |renderer, desc| {
                renderer
                    .create_particle_emitter(desc)
                    .map_err(|error| error.to_string())
            },
            |renderer, id| {
                if let Err(error) = renderer.destroy_particle_emitter(id) {
                    log::error!("эмиттер частиц не уничтожен: {error}");
                }
            },
        );
    }

    #[cfg(feature = "dev-console")]
    fn set_debug_view(&mut self, debug_view: DebugView) {
        self.debug_view = debug_view;
    }

    fn render(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameOutcome, String> {
        self.queue_screenshots();
        self.renderer.set_latency_frame(snapshot.sequence);
        let (light, shadow_cascades, environment) = self.frame_lighting(snapshot, width, height)?;
        let particle_step = particle_step(snapshot.sim_time, self.last_submitted_sim_time);
        let particles: Vec<ParticleEmitterFrame> = snapshot
            .particles
            .iter()
            .filter_map(|instance| {
                Some(ParticleEmitterFrame {
                    emitter: *self.particle_emitters.get(&instance.object)?,
                    transform: instance.transform,
                    emitting: instance.emitting,
                })
            })
            .collect();
        let joint_palettes = palette_refs(snapshot);
        self.game_ui
            .sync(&mut self.renderer, &snapshot.ui)
            .map_err(|error| error.to_string())?;
        let ui_primitives = self.game_ui.primitives(&snapshot.ui);
        let frame = Frame {
            joint_palettes: &joint_palettes,
            clear_color: [
                snapshot.view.render.clear_color[0],
                snapshot.view.render.clear_color[1],
                snapshot.view.render.clear_color[2],
                1.0,
            ],
            camera: FrameCamera {
                view: snapshot.camera.view().to_cols_array_2d(),
                projection: snapshot.camera.projection(width, height).to_cols_array_2d(),
                position: snapshot.camera.position.to_array(),
            },
            environment,
            lights: &snapshot.local_lights,
            local_light_shadows: &snapshot.local_light_shadows,
            shadow_cascades: &shadow_cascades,
            light,
            draws: &snapshot.draws,
            overlay: &[],
            ui: &ui_primitives,
            debug_view: self.debug_view,
            view: ViewSettings {
                render_scale: snapshot.view.render.render_scale,
                post: render_post_process(&snapshot.post_process, snapshot.color_lut),
                screen_shaders: snapshot.screen_shaders,
                anti_aliasing: match snapshot.view.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: snapshot.reset_history,
            particles: &particles,
            particle_step,
        };
        let outcome = self
            .renderer
            .render_frame_with_outcome(&frame)
            .map_err(|error| error.to_string())?;
        self.finish_screenshots();
        // Состояние частиц считается продвинутым только для отправленного кадра.
        if outcome.submitted {
            self.last_submitted_sim_time = Some(snapshot.sim_time);
        }
        Ok(outcome)
    }

    fn release_scene(&mut self) {
        self.game_ui.release_all(&mut self.renderer);
        for instance in self.scene_skin_instances.drain(..) {
            if let Err(error) = self.renderer.destroy_skin_instance(instance) {
                eprintln!("failed to release skin instance: {error}");
            }
        }
        for shader in self.scene_screen_shaders.drain(..) {
            if let Err(error) = self.renderer.destroy_screen_shader(shader) {
                eprintln!("failed to release screen shader: {error}");
            }
        }
        for lut in self.scene_color_luts.drain(..) {
            if let Err(error) = self.renderer.destroy_color_lut(lut) {
                eprintln!("failed to release color LUT: {error}");
            }
        }
    }

    fn wait_idle(&mut self) {
        match self.renderer.pipeline_cache_data() {
            Ok(data) => {
                engine_platform::save_pipeline_cache(self.pipeline_cache_path.as_deref(), &data)
            }
            Err(error) => eprintln!("failed to retrieve pipeline cache: {error}"),
        }
    }

    #[cfg(feature = "dev-console")]
    fn take_console_ui(&mut self) -> Option<ConsoleUiFrame> {
        self.console_ui.as_ref().and_then(|slot| slot.take())
    }

    #[cfg(feature = "dev-console")]
    fn discard_console_geometry(&mut self) {
        if let Some(slot) = &self.console_ui {
            slot.discard_geometry_preserve_texture_delta();
        }
    }

    #[cfg(feature = "dev-console")]
    fn render_console(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
        ui: &mut ConsoleUiFrame,
    ) -> Result<FrameOutcome, String> {
        self.queue_screenshots();
        if !ui.textures_delta.set.is_empty() {
            if let Err(error) = self
                .ui_bridge
                .managed
                .apply_updates(&ui.textures_delta, &mut self.renderer)
            {
                ui.textures_delta.clear();
                return Err(error.to_string());
            }
            ui.textures_delta.set.clear();
        }

        let viewport = match self.console_viewport {
            Some((viewport, old_width, old_height)) => {
                if old_width != width || old_height != height {
                    self.renderer
                        .resize_viewport(viewport, width, height)
                        .map_err(|error| error.to_string())?;
                    self.console_viewport = Some((viewport, width, height));
                }
                viewport
            }
            None => {
                let viewport = self
                    .renderer
                    .create_viewport(width, height)
                    .map_err(|error| error.to_string())?;
                self.console_viewport = Some((viewport, width, height));
                viewport
            }
        };

        let (light, shadow_cascades, environment) = self.frame_lighting(snapshot, width, height)?;
        let joint_palettes = palette_refs(snapshot);
        self.game_ui
            .sync(&mut self.renderer, &snapshot.ui)
            .map_err(|error| error.to_string())?;
        let game_ui = self.game_ui.primitives(&snapshot.ui);
        let scene = Frame {
            joint_palettes: &joint_palettes,
            clear_color: [
                snapshot.view.render.clear_color[0],
                snapshot.view.render.clear_color[1],
                snapshot.view.render.clear_color[2],
                1.0,
            ],
            camera: FrameCamera {
                view: snapshot.camera.view().to_cols_array_2d(),
                projection: snapshot.camera.projection(width, height).to_cols_array_2d(),
                position: snapshot.camera.position.to_array(),
            },
            environment,
            lights: &snapshot.local_lights,
            local_light_shadows: &snapshot.local_light_shadows,
            shadow_cascades: &shadow_cascades,
            light,
            draws: &snapshot.draws,
            overlay: &[],
            ui: &game_ui,
            debug_view: self.debug_view,
            view: ViewSettings {
                render_scale: snapshot.view.render.render_scale,
                post: render_post_process(&snapshot.post_process, snapshot.color_lut),
                screen_shaders: snapshot.screen_shaders,
                anti_aliasing: match snapshot.view.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: snapshot.reset_history,
            particles: &[],
            particle_step: 0.0,
        };
        let vertices = [
            UiVertex {
                position: [0.0, 0.0],
                uv: [0.0, 0.0],
                color: [255; 4],
            },
            UiVertex {
                position: [width as f32, 0.0],
                uv: [1.0, 0.0],
                color: [255; 4],
            },
            UiVertex {
                position: [width as f32, height as f32],
                uv: [1.0, 1.0],
                color: [255; 4],
            },
            UiVertex {
                position: [0.0, height as f32],
                uv: [0.0, 1.0],
                color: [255; 4],
            },
        ];
        let indices = [0, 1, 2, 0, 2, 3];
        let converted = self.ui_bridge.convert(&ui.primitives, ui.pixels_per_point);
        let mut primitives = Vec::with_capacity(converted.len() + 1);
        primitives.push(UiPrimitive {
            clip_rect: [0.0, 0.0, width as f32, height as f32],
            image: UiImage::Viewport(viewport),
            vertices: &vertices,
            indices: &indices,
        });
        primitives.extend(converted.iter().map(|mesh| mesh.primitive()));
        let outcome = self
            .renderer
            .render_frame_with_ui_outcome(&[(viewport, &scene)], &primitives, [0.0, 0.0, 0.0, 1.0])
            .map_err(|error| error.to_string())?;
        self.finish_screenshots();
        if outcome.status == FrameStatus::Presented && !ui.textures_delta.free.is_empty() {
            self.ui_bridge
                .managed
                .apply_frees(&ui.textures_delta, &mut self.renderer)
                .map_err(|error| error.to_string())?;
            ui.textures_delta.free.clear();
        }
        Ok(outcome)
    }

    #[cfg(feature = "dev-console")]
    fn close_console(&mut self) -> Result<(), String> {
        self.ui_bridge
            .managed
            .clear(&mut self.renderer)
            .map_err(|error| error.to_string())?;
        if let Some((viewport, _, _)) = self.console_viewport.take() {
            self.renderer
                .free_viewport(viewport)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}
