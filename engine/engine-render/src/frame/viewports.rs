//! Кадр с набором viewport-сцен и UI (`Renderer::render_frame_with_ui_outcome`).

use crate::*;

impl Renderer {
    /// Renders viewport scenes and UI and reports if their command buffer was submitted.
    pub fn render_frame_with_ui_outcome(
        &mut self,
        scenes: &[(ViewportId, &Frame<'_>)],
        ui: &[UiPrimitive<'_>],
        clear: [f32; 4],
    ) -> Result<FrameOutcome, RenderError> {
        let prepare_started = Instant::now();
        self.begin_particle_frame();
        self.frame_serial = self.frame_serial.wrapping_add(1);
        let frame_serial = self.frame_serial;
        if self.frame_slots_lost {
            self.fail_all_queued_captures("frame slot synchronization was lost");
            return Err(RenderError::InvalidState(
                "ресурсы синхронизации кадровых слотов потеряны",
            ));
        }
        validate_scene_viewports(scenes.iter().map(|(viewport, _)| *viewport))?;
        self.screen_temporal.state.missed();
        self.miss_unlisted_viewports(scenes);
        let mut prepared_scenes = Vec::with_capacity(scenes.len());
        let mut scene_environment_sets = Vec::with_capacity(scenes.len());
        for (viewport, frame) in scenes {
            validate_debug_view(frame.debug_view, self.capabilities.wireframe)?;
            validate_draws(frame.draws, &self.meshes, &self.materials)?;
            self.validate_screen_shaders(&frame.view.screen_shaders)?;
            self.validate_color_lut(&frame.view.post)?;
            self.validate_skin_draws(frame)?;
            self.viewports
                .get(viewport.0)
                .and_then(Option::as_ref)
                .ok_or_else(unknown_viewport)?;
            let extent = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .extent;
            let render_extent = scaled_extent(extent, frame.view.render_scale);
            let view = effective_view_settings(frame.view, frame.debug_view);
            let plan = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .temporal
                .state
                .plan(
                    temporal_key(
                        render_extent,
                        extent,
                        view.anti_aliasing,
                        frame.debug_view.code(),
                    ),
                    multiply_4x4(frame.camera.projection, frame.camera.view),
                    frame.reset_history,
                );
            prepared_scenes.push((
                prepare_frame_with_temporal(frame, render_extent, Some(plan))?,
                plan,
                None::<String>,
                0.0_f32,
            ));
            scene_environment_sets.push(self.environment_set_for(frame.environment)?);
        }
        // UI видов и swapchain лежит в одних буферах слота; смещения накапливаются.
        let mut packed = PackedUi::default();
        let mut view_ui = Vec::with_capacity(scenes.len());
        for (viewport, frame) in scenes {
            validate_frame_ui(frame.ui)?;
            let extent = self
                .viewports
                .get(viewport.0)
                .and_then(Option::as_ref)
                .ok_or_else(unknown_viewport)?
                .extent;
            view_ui.push((*viewport, packed.append(frame.ui, extent)?));
        }
        let swapchain_ui = packed.append(ui, self.output_extent)?;
        self.validate_ui_images(&packed, scenes)?;
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            self.miss_all_temporal_views();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        }
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.miss_all_temporal_views();
            self.recreate_swapchain()?;
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        }
        let present_id = self.begin_latency_frame();

        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let swapchain_loader = self.swapchain_loader()?.clone();
        let frame_slot = self.frame_slot;
        let fence = self
            .frames
            .get(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?
            .fence;
        // SAFETY: this fence belongs to the frame slot and its prior submission is finished before reuse.
        let prepare = prepare_started.elapsed();
        let fence_started = Instant::now();
        if let Err(error) = unsafe { device.wait_for_fences(&[fence], true, u64::MAX) } {
            self.fail_all_queued_captures("frame slot fence wait failed");
            return Err(error.into());
        }
        self.poll_captures(Some(frame_slot));
        let fence_wait = fence_started.elapsed();
        let uploads_started = Instant::now();
        self.poll_uploads()?;
        self.flush_uploads()?;
        let uploads = uploads_started.elapsed();
        let mut cpu = RenderCpuTimings {
            prepare,
            fence_wait,
            uploads,
            ..RenderCpuTimings::default()
        };
        let draw_stats = Cell::new(DrawStats::default());
        self.retired.slot_completed(frame_slot);
        self.retired_swapchains
            .slot_completed(frame_slot, self.frames[frame_slot].submission_generation);
        let scene_frames: Vec<&Frame<'_>> = scenes.iter().map(|(_, frame)| *frame).collect();
        let skin_plan = self.plan_skin(&scene_frames, frame_slot)?;
        let skin_record = self.skin_record(skin_plan.as_ref());
        let submitted_at = Instant::now();
        for ((viewport, frame), (_, _, _fallback_reason, _frame_delta_ms)) in
            scenes.iter().zip(&mut prepared_scenes)
        {
            let extent = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .extent;
            if extent.width > 0 && extent.height > 0 {
                let temporal = &mut self.viewports[viewport.0]
                    .as_mut()
                    .ok_or_else(unknown_viewport)?
                    .temporal;
                *_frame_delta_ms =
                    temporal_frame_delta_ms(temporal.last_submitted_at, submitted_at);
            }
            let view = effective_view_settings(frame.view, frame.debug_view);
            if extent.width > 0 && extent.height > 0 && view.anti_aliasing.is_temporal() {
                let entry = self.viewports[viewport.0]
                    .as_mut()
                    .ok_or_else(unknown_viewport)?;
                ensure_temporal_history(
                    &instance,
                    &device,
                    self.physical_device,
                    &mut entry.temporal,
                    extent,
                    &mut self.retired,
                )?;
            }
            if extent.width > 0 && extent.height > 0 && view.anti_aliasing == AntiAliasing::Fsr {
                #[cfg(windows)]
                if self.capabilities.fsr {
                    let Some(api) = self.fsr_api.as_ref().cloned() else {
                        let error = "FSR capability has no loaded API".to_owned();
                        warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                        *_fallback_reason = Some(error);
                        continue;
                    };
                    let entry = self.viewports[viewport.0]
                        .as_mut()
                        .ok_or_else(unknown_viewport)?;
                    if let Err(error) = ensure_fsr_view(
                        &instance,
                        &device,
                        self.physical_device,
                        &mut entry.temporal,
                        &api,
                        scaled_extent(extent, frame.view.render_scale),
                        extent,
                        &mut self.retired,
                    ) {
                        warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                        *_fallback_reason = Some(error);
                    }
                } else {
                    let error = self
                        .fsr_load_error
                        .clone()
                        .unwrap_or_else(|| "FSR runtime is unavailable".to_owned());
                    warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                    *_fallback_reason = Some(error);
                }
                #[cfg(not(windows))]
                {
                    let error = "FSR is only available on Windows".to_owned();
                    warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                }
            } else if extent.width > 0
                && extent.height > 0
                && view.anti_aliasing == AntiAliasing::Dlss
            {
                #[cfg(all(windows, feature = "dlss"))]
                if self.capabilities.dlss {
                    let entry = self.viewports[viewport.0]
                        .as_mut()
                        .ok_or_else(unknown_viewport)?;
                    if let Err(error) = ensure_dlss_view(
                        &instance,
                        &device,
                        self.physical_device,
                        &mut entry.temporal,
                        scaled_extent(extent, frame.view.render_scale),
                        extent,
                        &mut self.retired,
                    ) {
                        warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                        *_fallback_reason = Some(error);
                    }
                } else {
                    let error = self
                        .dlss_load_error
                        .clone()
                        .unwrap_or_else(|| "DLSS runtime is unavailable".to_owned());
                    warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                    *_fallback_reason = Some(error);
                }
                #[cfg(not(all(windows, feature = "dlss")))]
                {
                    let error = self
                        .dlss_load_error
                        .clone()
                        .unwrap_or_else(|| "DLSS support is unavailable".to_owned());
                    warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                    #[cfg(windows)]
                    {
                        *_fallback_reason = Some(error);
                    }
                }
            }
        }
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let swapchain_handle = swapchain.handle;
        let swapchain_image_count = swapchain.images.len();
        let queue = self.queue;
        let image_set_layout = self.texture_set_layout()?;
        let post_set_layout = self.post_set_layout()?;
        let bloom_prefilter_layout = self
            .bloom_pipelines
            .ok_or(RenderError::InvalidState("bloom pipelines отсутствуют"))?
            .prefilter_set_layout();
        let exposure_set_layout = self
            .exposure_pipelines
            .ok_or(RenderError::InvalidState("exposure pipelines отсутствуют"))?
            .set_layout();
        let filter_set_layout = self
            .filter_pipelines
            .ok_or(RenderError::InvalidState("filter pipelines отсутствуют"))?
            .set_layout();
        let taa_set_layout = self.taa_set_layout()?;
        let frame_set_layout = self.frame_set_layout()?;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let command_buffer = frame_resources.command_buffer;
        let gpu = frame_resources.read_gpu_timings(
            &device,
            self.timestamp_valid_bits,
            self.timestamp_period,
        )?;
        if let Some(total) = gpu.and_then(|gpu| gpu.first_view_total()) {
            self.pending_gpu_time = Some(total);
        }
        // Fence слота дождались, поэтому GPU больше не читает буферы этого слота.
        frame_resources.ui_vertices.write(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            &packed.vertices,
        )?;
        frame_resources.ui_indices.write(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::INDEX_BUFFER,
            &packed.indices,
        )?;
        let image_available = frame_resources.image_available;
        // Fence слота завершён, поэтому прежние изображения и uniform-буферы слота свободны.
        // Выделение идёт до acquire, чтобы ошибка памяти не оставила захваченный image и
        // сигнальный semaphore.
        let target_resources = ViewportTargetResources {
            image_set_layout,
            post_set_layout,
            exposure_set_layout,
            filter_set_layout,
            taa_set_layout,
            particle_depth_layout: self.particles.depth_layout()?,
            image_sampler: self
                .ui_sampler
                .ok_or(RenderError::InvalidState("UI sampler отсутствует"))?,
            post_sampler: self
                .post_sampler
                .ok_or(RenderError::InvalidState("post sampler отсутствует"))?,
            frame_set_layout,
            shadow_view: self
                .shadow_image
                .as_ref()
                .ok_or(RenderError::InvalidState("shadow map отсутствует"))?
                .view,
            local_shadow_view: self
                .local_shadow_image
                .as_ref()
                .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?
                .view,
            shadow_sampler: self
                .shadow_sampler
                .ok_or(RenderError::InvalidState("shadow sampler отсутствует"))?,
        };
        let pipeline_layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        let shadow_pipeline = self
            .shadow_pipeline
            .ok_or(RenderError::InvalidState("shadow pipeline отсутствует"))?;
        let local_shadow_pipeline = self.local_shadow_pipeline.ok_or(RenderError::InvalidState(
            "local shadow pipeline отсутствует",
        ))?;
        let scene_pipeline = self
            .pipeline
            .ok_or(RenderError::InvalidState("pipeline отсутствует"))?;
        let mask_pipeline = self
            .mask_pipeline
            .ok_or(RenderError::InvalidState("mask pipeline отсутствует"))?;
        let blend_pipeline = self
            .blend_pipeline
            .ok_or(RenderError::InvalidState("blend pipeline отсутствует"))?;
        let overdraw_pipeline = self
            .overdraw_pipeline
            .ok_or(RenderError::InvalidState("overdraw pipeline отсутствует"))?;
        let wireframe_pipeline = self.wireframe_pipeline;
        let cluster_pipeline = self
            .cluster_pipeline
            .ok_or(RenderError::InvalidState("cluster pipeline отсутствует"))?;
        let cluster_pipeline_layout =
            self.cluster_pipeline_layout
                .ok_or(RenderError::InvalidState(
                    "cluster pipeline layout отсутствует",
                ))?;
        let skybox_pipeline = self
            .skybox_pipeline
            .ok_or(RenderError::InvalidState("skybox pipeline отсутствует"))?;
        let ui_pipeline = self
            .ui_pipeline
            .ok_or(RenderError::InvalidState("UI pipeline отсутствует"))?;
        let ui_pipeline_layout = self
            .ui_pipeline_layout
            .ok_or(RenderError::InvalidState("UI pipeline layout отсутствует"))?;
        let viewport_ui_pipeline = self.viewport_ui_pipeline.ok_or(RenderError::InvalidState(
            "viewport UI pipeline отсутствует",
        ))?;
        let tonemap_pipeline = self
            .tonemap_pipeline
            .ok_or(RenderError::InvalidState("tonemap pipeline отсутствует"))?;
        let viewport_output_pipeline =
            self.viewport_output_pipeline
                .ok_or(RenderError::InvalidState(
                    "viewport output pipeline отсутствует",
                ))?;
        let post_pipeline_layout = self.post_pipeline_layout.ok_or(RenderError::InvalidState(
            "post pipeline layout отсутствует",
        ))?;
        let taa_pipeline = self
            .taa_pipeline
            .ok_or(RenderError::InvalidState("TAA pipeline отсутствует"))?;
        let taa_pipeline_layout = self
            .taa_pipeline_layout
            .ok_or(RenderError::InvalidState("TAA pipeline layout отсутствует"))?;
        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let local_shadow_image = self
            .local_shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?;
        for (viewport, frame) in scenes {
            let extent = self.viewports[viewport.0]
                .as_ref()
                .expect("viewports were validated before acquire")
                .extent;
            if extent.width > 0
                && extent.height > 0
                && effective_view_settings(frame.view, frame.debug_view)
                    .anti_aliasing
                    .is_temporal()
                && self.viewports[viewport.0]
                    .as_ref()
                    .and_then(|entry| entry.temporal.history.as_ref())
                    .is_none()
            {
                return Err(RenderError::InvalidState("TAA history отсутствует"));
            }
        }
        let slot_timestamps = frame_resources.timestamps;
        let output_marks = slot_timestamps.map(|pool| GpuMarks {
            pool,
            first: OUTPUT_QUERY_FIRST,
        });
        let framebuffer_is_srgb = is_srgb_format(self.surface_format.format);
        let physical_device = self.physical_device;
        let screen_shader_set_layout = self
            .screen_shader_layout
            .as_ref()
            .map(|layout| layout.set_layout);
        let mut rendered = Vec::with_capacity(scenes.len());
        let ao_set_layout = self
            .ao_pipelines
            .ok_or(RenderError::InvalidState("ao pipelines отсутствуют"))?
            .set_layout();
        let white_ao_view = self
            .textures
            .get(WHITE_LINEAR_TEXTURE)
            .and_then(Option::as_ref)
            .map(|texture| texture.image.view)
            .ok_or(RenderError::InvalidState("белая текстура отсутствует"))?;
        for ((scene, prepared_scene), environment) in scenes
            .iter()
            .zip(&prepared_scenes)
            .zip(&scene_environment_sets)
        {
            let (viewport, frame) = scene;
            let (prepared, plan, fallback_reason, frame_delta_ms) = prepared_scene;
            let (environment_set, skybox) = environment;
            let entry = self.viewports[viewport.0]
                .as_mut()
                .ok_or_else(unknown_viewport)?;
            let extent = entry.extent;
            if extent.width == 0 || extent.height == 0 {
                entry.temporal.state.missed();
                continue;
            }
            let render_extent = scaled_extent(extent, frame.view.render_scale);
            let slot_target = &mut entry.targets[frame_slot];
            if slot_target.as_ref().is_none_or(|target| {
                target.extent != extent || target.render_extent != render_extent
            }) {
                *slot_target = Some(ViewportTarget::new(
                    &instance,
                    &device,
                    physical_device,
                    extent,
                    render_extent,
                    &target_resources,
                )?);
            }
            let target = slot_target.as_mut().ok_or_else(unknown_viewport)?;
            let tonemap_source = if effective_view_settings(frame.view, frame.debug_view)
                .anti_aliasing
                .is_temporal()
            {
                let history = entry
                    .temporal
                    .history
                    .as_ref()
                    .ok_or(RenderError::InvalidState("TAA history отсутствует"))?;
                write_taa_image_set(
                    &device,
                    target.taa_set,
                    target_resources.post_sampler,
                    [
                        target.scene.color.view,
                        target.scene.velocity.view,
                        target.scene.depth.view,
                        history.images[plan.read_history].view,
                    ],
                );
                history.images[plan.write_history].view
            } else {
                target.scene.color.view
            };
            write_image_set(
                &device,
                target.tonemap_set,
                tonemap_source,
                target_resources.post_sampler,
            );
            let frame_set = target.frame_set.write(
                &instance,
                &device,
                physical_device,
                &prepared.uniforms,
                &prepared.light_bytes,
                &local_shadow_matrix_bytes(&prepared.local_shadow_matrices)?,
                &prepared.cluster_bounds,
            )?;
            let post_frame = PostFrame::new(
                &effective_view_settings(frame.view, frame.debug_view).post,
                frame.debug_view,
                (target.render_extent.width, target.render_extent.height),
                (target.extent.width, target.extent.height),
                frame_serial,
                camera_near_far(frame.camera.projection),
                &effective_view_settings(frame.view, frame.debug_view).screen_shaders,
            );
            if post_frame.plan.ambient_occlusion {
                ensure_ao_target(
                    &instance,
                    &device,
                    physical_device,
                    &mut target.scene,
                    target.render_extent,
                    post_frame.ao.quality,
                    ao_set_layout,
                )?;
            }
            if let Some(pipelines) = self.occlusion_pipelines.filter(|_| {
                self.prepass_pipeline.is_some()
                    && occlusion_culling_enabled(frame.debug_view, frame.draws)
            }) {
                prepare_occlusion(
                    &instance,
                    &device,
                    physical_device,
                    &mut target.scene,
                    target.render_extent,
                    pipelines,
                    frame.draws,
                    &self.meshes,
                )?;
            }
            let frame_ao_view = target
                .scene
                .ao
                .as_ref()
                .filter(|_| post_frame.plan.ambient_occlusion)
                .map_or(white_ao_view, |ao| ao.filtered.view);
            write_frame_ao(&device, frame_set, frame_ao_view);
            if post_frame.plan.bloom_mips > 0 && target.bloom.is_none() {
                target.bloom = Some(BloomChain::new(
                    &instance,
                    &device,
                    physical_device,
                    target.render_extent,
                    post_frame.plan.bloom_mips,
                    target_resources.image_set_layout,
                    bloom_prefilter_layout,
                    target_resources.post_sampler,
                )?);
            }
            if post_frame.plan.motion_blur || post_frame.plan.depth_of_field {
                let filter_extent = if effective_view_settings(frame.view, frame.debug_view)
                    .anti_aliasing
                    .is_temporal()
                {
                    target.extent
                } else {
                    target.render_extent
                };
                if target
                    .filter_scratch
                    .as_ref()
                    .is_none_or(|scratch| scratch.extent != filter_extent)
                {
                    target.filter_scratch = Some(FilterScratch::new(
                        &instance,
                        &device,
                        physical_device,
                        filter_extent,
                        target_resources.filter_set_layout,
                    )?);
                }
            }
            let screen_before_extent = if effective_view_settings(frame.view, frame.debug_view)
                .anti_aliasing
                .is_temporal()
            {
                target.extent
            } else {
                target.render_extent
            };
            prepare_screen_scratch(
                &mut target.screen_before,
                post_frame.screen_before.len(),
                screen_before_extent,
                OFFSCREEN_FORMAT,
                &instance,
                &device,
                physical_device,
                screen_shader_set_layout,
            )?;
            prepare_screen_scratch(
                &mut target.screen_after,
                post_frame.screen_after.len(),
                target.extent,
                LDR_FORMAT,
                &instance,
                &device,
                physical_device,
                screen_shader_set_layout,
            )?;
            rendered.push((
                *viewport,
                *frame,
                *plan,
                frame_set,
                *environment_set,
                skybox_pipeline,
                *skybox,
                target.tonemap_set,
                target.output_set,
                prepared.local_shadow_matrices.as_slice(),
                *frame_delta_ms,
                fallback_reason.clone(),
                post_frame,
            ));
        }

        let mut draws = Vec::with_capacity(packed.draws.len());
        let mut view_draws = Vec::with_capacity(view_ui.len());
        for (viewport, range) in view_ui {
            let start = draws.len();
            draws.extend(self.ui_texture_draws(&packed.draws[range])?);
            view_draws.push((viewport, start..draws.len()));
        }
        let swapchain_start = draws.len();
        for draw in &packed.draws[swapchain_ui] {
            let (set, image_is_linear) = match draw.image {
                UiImage::Texture(texture) => (self.ui_texture(texture)?.set, false),
                UiImage::Viewport(viewport) => {
                    if !rendered
                        .iter()
                        .any(|(drawn, _, _, _, _, _, _, _, _, _, _, _, _)| *drawn == viewport)
                    {
                        continue;
                    }
                    let target = self.viewports[viewport.0]
                        .as_ref()
                        .and_then(|entry| entry.targets[frame_slot].as_ref())
                        .ok_or_else(unknown_viewport)?;
                    (target.set, true)
                }
            };
            draws.push((*draw, set, image_is_linear));
        }
        let swapchain_draws = swapchain_start..draws.len();

        let acquire_started = Instant::now();
        let acquired = match acquire_image(
            &swapchain_loader,
            swapchain_handle,
            image_available,
            swapchain_image_count,
            &mut self.swapchain_dirty,
        ) {
            Ok(acquired) => {
                cpu.acquire = acquire_started.elapsed();
                acquired
            }
            Err(AcquireImageError::BeforeAcquire(error)) => return Err(error),
            Err(AcquireImageError::AfterAcquire(error)) => {
                self.miss_all_temporal_views();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };
        let Some((image_index, acquire_suboptimal)) = acquired else {
            self.miss_all_temporal_views();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        };
        let swapchain_image = swapchain.images[image_index];
        let swapchain_view = swapchain.views[image_index];
        let swapchain_extent = swapchain.extent;
        let render_finished = swapchain.render_finished[image_index];
        let (present_copies, window_captures) = prepare_window_captures(
            &mut self.captures,
            WindowCaptureTarget {
                instance: &instance,
                device: &device,
                physical_device: self.physical_device,
                row_alignment: self.capture_row_alignment,
                transfer_src: swapchain.transfer_src,
                format: self.surface_format.format,
                frame_slot,
                extent: swapchain_extent,
            },
        );

        let particle_pipelines = self.particle_pipelines();
        let mut particle_works = Vec::with_capacity(rendered.len());
        for entry in &rendered {
            // Вид без цели не записывается: ему не отдаём план и не коммитим состояние.
            let has_target = self
                .viewports
                .get(entry.0.0)
                .and_then(Option::as_ref)
                .and_then(|viewport| viewport.targets.get(frame_slot))
                .is_some_and(Option::is_some);
            if !has_target {
                particle_works.push(Vec::new());
                continue;
            }
            particle_works.push(self.particles.plan_view(
                entry.1.particles,
                entry.1.particle_step,
                multiply_4x4(entry.1.camera.projection, entry.1.camera.view),
                entry.1.debug_view,
                entry.1.camera.position,
            )?);
        }
        // Индексы совпадают с `rendered`: пропущенный вид без размера не получает экземпляров.
        let (instance_buffer, instance_bases) = self.frames[frame_slot].write_instances(
            &instance,
            &device,
            self.physical_device,
            rendered.iter().map(|entry| entry.1.draws),
        )?;
        let mut gpu_layout = GpuProfileLayout::default();
        let bloom_pipelines = self.bloom_pipelines;
        let exposure_pipelines = self.exposure_pipelines;
        let filter_pipelines = self.filter_pipelines;
        let ao_pipelines = self.ao_pipelines;
        let opaque_equal_pipeline = self.opaque_equal_pipeline;
        let prepass_pipeline = self.prepass_pipeline;
        let occlusion_pipelines = self.occlusion_pipelines;
        // Только viewport с запросами: без захвата внешний список остаётся пустым и не выделяет память.
        let capture_tickets_by_viewport = rendered
            .iter()
            .filter_map(|entry| {
                let tickets = self.queued_capture_tickets(entry.0);
                (!tickets.is_empty()).then_some((entry.0, tickets))
            })
            .collect::<Vec<_>>();
        let color_luts = &self.color_luts;
        let scene_passes = rendered
            .iter()
            .enumerate()
            .map(
                |(
                    index,
                    (
                        viewport,
                        frame,
                        plan,
                        frame_set,
                        environment_set,
                        skybox_pipeline,
                        skybox,
                        tonemap_set,
                        output_set,
                        local_shadow_matrices,
                        frame_delta_ms,
                        upscaler_fallback_reason,
                        post_frame,
                    ),
                )| {
                    // Условие совпадает с подготовкой цели вида в prepare_occlusion.
                    let occlusion_pipelines = occlusion_pipelines.filter(|_| {
                        prepass_pipeline.is_some()
                            && occlusion_culling_enabled(frame.debug_view, frame.draws)
                    });
                    (
                        *viewport,
                        ShadowPass {
                            pipeline: shadow_pipeline,
                            local_pipeline: local_shadow_pipeline,
                            pipeline_layout,
                            frame_set: *frame_set,
                            meshes: &self.meshes,
                            materials: &self.materials,
                            draws: frame.draws,
                            skin_draws: skin::skin_draws_for(skin_plan.as_ref(), index),
                            instances: InstanceRange {
                                buffer: instance_buffer,
                                first: instance_bases[index],
                            },
                            shadow_cascades: frame.shadow_cascades,
                            local_shadow_matrices,
                            stats: &draw_stats,
                        },
                        ScenePass {
                            pipeline: scene_pipeline,
                            mask_pipeline,
                            blend_pipeline,
                            overdraw_pipeline,
                            wireframe_pipeline,
                            cluster_pipeline,
                            cluster_pipeline_layout,
                            skybox_pipeline: *skybox_pipeline,
                            pipeline_layout,
                            frame_set: *frame_set,
                            environment_set: *environment_set,
                            skybox: *skybox,
                            meshes: &self.meshes,
                            materials: &self.materials,
                            surface_shaders: &self.surface_shaders,
                            draws: frame.draws,
                            skin_draws: skin::skin_draws_for(skin_plan.as_ref(), index),
                            instances: InstanceRange {
                                buffer: instance_buffer,
                                first: instance_bases[index],
                            },
                            view_projection: multiply_4x4(
                                frame.camera.projection,
                                frame.camera.view,
                            ),
                            #[cfg(windows)]
                            fsr_camera: fsr_camera_parameters(frame.camera.projection),
                            culling_padding: [plan.jitter_ndc[0].abs(), plan.jitter_ndc[1].abs()],
                            temporal_plan: *plan,
                            camera_position: frame.camera.position,
                            clear_color: frame.clear_color,
                            debug_view: frame.debug_view,
                            timestamps: slot_timestamps.and_then(|pool| {
                                gpu_layout
                                    .add_view(ProfiledView::Viewport(*viewport))
                                    .map(|first| GpuMarks { pool, first })
                            }),
                            view: effective_view_settings(frame.view, frame.debug_view),
                            post: *post_frame,
                            bloom_pipelines,
                            exposure_pipelines,
                            exposure_dt_ms: *frame_delta_ms,
                            // Ссылка проверена до acquire; identity LUT существует всегда.
                            color_lut: color_lut_view(
                                color_luts,
                                post::planned_color_lut(
                                    &effective_view_settings(frame.view, frame.debug_view).post,
                                    &post_frame.plan,
                                ),
                            )
                            .unwrap_or_default(),
                            reset_history: frame.reset_history,
                            filter_pipelines,
                            ao_pipelines,
                            opaque_equal_pipeline,
                            prepass_pipeline,
                            depth_prepass: (post_frame.plan.depth_prepass
                                || occlusion_pipelines.is_some())
                                && prepass_pipeline.is_some(),
                            occlusion: occlusion_pipelines,
                            projection_scale: [
                                frame.camera.projection[0][0],
                                frame.camera.projection[1][1],
                            ],
                            particles: particle_pipelines.map(|pipelines| ParticlePass {
                                works: &particle_works[index],
                                pipelines,
                                frame_set: *frame_set,
                            }),
                            stats: &draw_stats,
                        },
                        *tonemap_set,
                        *output_set,
                        *frame_delta_ms,
                        upscaler_fallback_reason.clone(),
                    )
                },
            )
            .collect::<Vec<_>>();
        self.frames[frame_slot].gpu_layout = gpu_layout;
        let timestamps = slot_timestamps.is_some();
        let frame_resources = &self.frames[frame_slot];
        let ui_pass = UiPass {
            draws: UiDrawPass::new(
                ui_pipeline,
                ui_pipeline_layout,
                &frame_resources.ui_vertices,
                &frame_resources.ui_indices,
                framebuffer_is_srgb,
                &draws[swapchain_draws],
            ),
            view: swapchain_view,
            clear,
        };
        // Каждый вид рисует свой UI в собственном output pass; буферы у видов общие с swapchain.
        let view_passes: Vec<(ViewportId, Option<UiDrawPass<'_>>)> = view_draws
            .iter()
            .map(|(viewport, range)| {
                let pass = UiDrawPass::new(
                    viewport_ui_pipeline,
                    ui_pipeline_layout,
                    &frame_resources.ui_vertices,
                    &frame_resources.ui_indices,
                    is_srgb_format(LDR_FORMAT),
                    &draws[range.clone()],
                );
                (*viewport, pass)
            })
            .collect();

        let mut capture_submissions = window_captures;
        let mut capture_attempted_viewports = Vec::new();
        let record_started = Instant::now();
        let recorded = (|| {
            begin_command_buffer(&device, command_buffer, slot_timestamps)?;
            if let Some(record) = skin_record {
                skin::record_skin(&device, command_buffer, record);
            }
            let screen_context = ScreenContext {
                shaders: &self.screen_shaders,
                layout: self
                    .screen_shader_layout
                    .as_ref()
                    .map_or(vk::PipelineLayout::null(), |layout| layout.pipeline_layout),
            };
            let mut shadow_layout = self.shadow_layout;
            let mut local_shadow_layout = self.local_shadow_layout;
            let post_sampler = target_resources.post_sampler;
            let mut temporal_submissions = Vec::with_capacity(scene_passes.len());
            let mut fsr_dispatch_errors = Vec::new();
            #[cfg(all(windows, feature = "dlss"))]
            let mut dlss_dispatch_errors = Vec::new();
            for (
                viewport,
                shadow,
                pass,
                tonemap_set,
                output_set,
                _frame_delta_ms,
                _upscaler_fallback_reason,
            ) in &scene_passes
            {
                let capture_tickets = capture_tickets_by_viewport
                    .iter()
                    .find(|(listed, _)| listed == viewport)
                    .map_or(&[][..], |(_, tickets)| tickets.as_slice());
                let Some(entry) = self.viewports[viewport.0].as_mut() else {
                    continue;
                };
                let Some(target) = entry.targets[frame_slot].as_mut() else {
                    continue;
                };
                capture_attempted_viewports.push(*viewport);
                let view_exposure = prepare_view_exposure(
                    &instance,
                    &device,
                    physical_device,
                    entry.temporal.exposure,
                    &mut entry.temporal.exposure_resources,
                    pass.post.plan.auto_exposure,
                    pass.reset_history,
                    target.render_extent,
                    pass.exposure_dt_ms,
                )?;
                record_scene(
                    &device,
                    command_buffer,
                    &mut target.scene,
                    shadow_image,
                    &mut shadow_layout,
                    local_shadow_image,
                    &mut local_shadow_layout,
                    target.render_extent,
                    shadow,
                    pass,
                );
                let exposure_buffer = view_exposure.buffer;
                let velocity_view = target.scene.velocity.view;
                let depth_view = target.scene.depth.view;
                let filters = match (target.filter_scratch.as_mut(), pass.filter_pipelines) {
                    (Some(scratch), Some(pipelines))
                        if pass.post.filters.motion.is_some()
                            || pass.post.filters.dof.is_some() =>
                    {
                        Some(FilterRecord {
                            scratch,
                            pipelines,
                            sampler: target_resources.post_sampler,
                            velocity: velocity_view,
                            depth: depth_view,
                            params: pass.post.filters,
                        })
                    }
                    _ => None,
                };
                // Экспозиция замеряется по HDR цели сцены до temporal resolve: FSR/DLSS и composite
                // получают EV этого кадра.
                let adapted_exposure = exposure_record(
                    &target.exposure,
                    pass.exposure_pipelines,
                    view_exposure,
                    &pass.view.post,
                )
                .map(|record| {
                    record_exposure(
                        &device,
                        command_buffer,
                        &target.scene.color,
                        &mut target.scene.layout,
                        target.render_extent,
                        record,
                        pass.timestamps,
                    );
                    exposure_buffer
                });
                #[cfg(not(windows))]
                let _ = adapted_exposure;
                let passes = PostPasses {
                    filters,
                    sampler: target_resources.post_sampler,
                    bloom: bloom_record(
                        target.bloom.as_mut(),
                        pass.bloom_pipelines,
                        pass.post.plan.bloom_mips,
                        target_resources.post_sampler,
                        &pass.view.post,
                        view_exposure,
                    ),
                    exposure_buffer,
                    lut: pass.color_lut,
                    screen_before: chain_record(
                        target.screen_before.as_mut(),
                        &pass.post.screen_before,
                        screen_context,
                        target_resources.post_sampler,
                    ),
                    screen_after: chain_record(
                        target.screen_after.as_mut(),
                        &pass.post.screen_after,
                        screen_context,
                        target_resources.post_sampler,
                    ),
                };
                let (temporal_outcome, tonemap_view) = if pass.view.anti_aliasing.is_temporal() {
                    let temporal = &mut entry.temporal;
                    let history = temporal
                        .history
                        .as_ref()
                        .expect("temporal history was validated before acquire");
                    #[cfg(windows)]
                    let backend = select_temporal_backend(
                        pass.view.anti_aliasing,
                        self.capabilities.fsr,
                        self.capabilities.dlss,
                    );
                    #[cfg(windows)]
                    let fsr = (backend == TemporalBackend::Fsr
                        && _upscaler_fallback_reason.is_none())
                    .then_some(temporal.fsr.as_deref_mut())
                    .flatten();
                    #[cfg(all(windows, feature = "dlss"))]
                    let dlss_api = self.dlss_api.as_ref();
                    #[cfg(all(windows, feature = "dlss"))]
                    let dlss = (backend == TemporalBackend::Dlss
                        && _upscaler_fallback_reason.is_none())
                    .then_some(temporal.dlss.as_deref_mut())
                    .flatten();
                    let outcome = record_temporal_resolve_and_tonemap(
                        &device,
                        command_buffer,
                        &mut target.scene,
                        &mut target.ldr,
                        &mut target.ldr_layout,
                        *tonemap_set,
                        post_sampler,
                        pass.frame_set,
                        target.extent,
                        pass.post.composite,
                        passes,
                        pass.timestamps,
                        tonemap_pipeline,
                        post_pipeline_layout,
                        target.taa_set,
                        TemporalResolveRecord {
                            history,
                            #[cfg(windows)]
                            backend,
                            plan: pass.temporal_plan,
                            taa_pipeline,
                            taa_pipeline_layout,
                            #[cfg(windows)]
                            fsr,
                            #[cfg(windows)]
                            exposure: UpscalerExposure::new(
                                pass.view.post.exposure.ev,
                                adapted_exposure,
                            ),
                            #[cfg(windows)]
                            frame_time_delta_ms: *_frame_delta_ms,
                            #[cfg(windows)]
                            camera: pass.fsr_camera,
                            #[cfg(all(windows, feature = "dlss"))]
                            dlss_api,
                            #[cfg(all(windows, feature = "dlss"))]
                            dlss,
                        },
                    )?;
                    let view = outcome.output_view;
                    (Some(outcome), view)
                } else {
                    write_image_set(&device, *tonemap_set, target.scene.color.view, post_sampler);
                    let view = record_tonemap_pass(
                        &device,
                        command_buffer,
                        &target.scene.color,
                        target.scene.layout,
                        &target.ldr,
                        &mut target.ldr_layout,
                        target.render_extent,
                        *tonemap_set,
                        tonemap_pipeline,
                        post_pipeline_layout,
                        pass.post.composite,
                        passes,
                        pass.timestamps,
                    );
                    target.scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                    (None, view)
                };
                write_image_set(
                    &device,
                    *output_set,
                    tonemap_view,
                    target_resources.post_sampler,
                );
                let output_source_extent = temporal_outcome
                    .as_ref()
                    .map_or(target.render_extent, |outcome| outcome.output_extent);
                let view_ui = view_passes
                    .iter()
                    .find(|(drawn, _)| drawn == viewport)
                    .and_then(|(_, pass)| pass.as_ref());
                record_output_pass(
                    &device,
                    command_buffer,
                    target.output.image,
                    target.output.view,
                    output_source_extent,
                    target.extent,
                    target.output_layout,
                    *output_set,
                    viewport_output_pipeline,
                    post_pipeline_layout,
                    pass.view.anti_aliasing,
                    true,
                    pass.post.grain,
                    None,
                    view_ui,
                    true,
                    &[],
                );
                for ticket in capture_tickets.iter().copied() {
                    let layout = capture_buffer_layout(
                        target.extent.width,
                        target.extent.height,
                        self.capture_row_alignment,
                    )?;
                    let buffer = HostBuffer::new_empty(
                        &instance,
                        &device,
                        physical_device,
                        vk::BufferUsageFlags::TRANSFER_DST,
                        layout.size,
                    )?;
                    record_viewport_capture(
                        &device,
                        command_buffer,
                        target.output.image,
                        buffer.buffer,
                        target.extent,
                        layout.row_length,
                    );
                    capture_submissions.push(CaptureSubmission {
                        ticket,
                        frame_slot,
                        buffer,
                        layout,
                        width: target.extent.width,
                        height: target.extent.height,
                        order: capture_pixel_order(LDR_FORMAT),
                        opaque: false,
                        completion: CaptureCompletion::Image,
                    });
                }
                target.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                if let Some(error) = temporal_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.fsr_error.as_deref())
                {
                    fsr_dispatch_errors.push((*viewport, error.to_owned()));
                }
                #[cfg(all(windows, feature = "dlss"))]
                if let Some(error) = temporal_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.dlss_error.as_deref())
                {
                    dlss_dispatch_errors.push((*viewport, error.to_owned()));
                }
                temporal_submissions.push(TemporalSubmission {
                    view: TemporalViewId::Viewport(*viewport),
                    plan: pass.temporal_plan,
                    exposure: view_exposure.adaptation,
                    layouts: temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_taa)
                        .then_some([vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL; 2]),
                    #[cfg(windows)]
                    fsr_used: temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_fsr),
                    #[cfg(all(windows, feature = "dlss"))]
                    dlss_used: temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_dlss),
                    submitted_at,
                });
            }
            for (_viewport, error) in fsr_dispatch_errors {
                warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                #[cfg(windows)]
                if let Some(Some(viewport)) = self.viewports.get_mut(_viewport.0) {
                    if let Some(fsr) = viewport.temporal.fsr.take() {
                        self.retired.retire(RetiredResource::FsrView(fsr), 0);
                    }
                    viewport.temporal.fsr_error = Some(error);
                    viewport.temporal.fsr_attempted = true;
                }
            }
            #[cfg(all(windows, feature = "dlss"))]
            if let Some((_, error)) = dlss_dispatch_errors.first() {
                warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, error);
                self.capabilities.dlss = false;
                self.dlss_load_error = Some(error.clone());
                if let Some(dlss) = self.screen_temporal.dlss.take() {
                    self.retired.retire(RetiredResource::DlssView(dlss), 0);
                }
                for viewport in self.viewports.iter_mut().flatten() {
                    if let Some(dlss) = viewport.temporal.dlss.take() {
                        self.retired.retire(RetiredResource::DlssView(dlss), 0);
                    }
                    viewport.temporal.dlss_attempted = true;
                    viewport.temporal.dlss_error = Some(error.clone());
                }
            }
            if let Some(marks) = output_marks {
                marks.write(&device, command_buffer, 0);
            }
            record_ui(
                &device,
                command_buffer,
                swapchain_image,
                swapchain_extent,
                &ui_pass,
                &present_copies,
            );
            if let Some(marks) = output_marks {
                marks.write(&device, command_buffer, 1);
            }
            // SAFETY: command recording is complete before the command buffer is submitted.
            unsafe { device.end_command_buffer(command_buffer) }?;
            Ok((temporal_submissions, shadow_layout, local_shadow_layout))
        })();
        cpu.record = record_started.elapsed();
        let (temporal_submissions, shadow_layout, local_shadow_layout) = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                self.fail_capture_submissions(
                    &capture_submissions,
                    "frame recording failed before viewport capture",
                );
                self.fail_queued_captures(
                    &capture_attempted_viewports,
                    "frame recording failed before viewport capture",
                );
                self.miss_all_temporal_views();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };

        let submitted = self.submit_and_present(AcquiredFrameSubmission {
            device: &device,
            swapchain_loader: &swapchain_loader,
            queue,
            present_id,
            frame_slot,
            fence,
            image_available,
            command_buffer,
            swapchain_handle,
            render_finished,
            image_index,
            acquire_suboptimal,
            wait_stage: vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            timestamps_written: timestamps,
            timings: &mut cpu,
            temporal_submissions: &temporal_submissions,
            capture_submissions,
            shadow_layout,
            local_shadow_layout,
            skin: skin_plan.as_ref(),
        });
        let particles_submitted = match &submitted {
            Ok(outcome) => outcome.submitted,
            Err(SubmitAndPresentError::BeforeSubmit(_)) => false,
            Err(SubmitAndPresentError::AfterSubmit(_)) => true,
        };
        self.commit_particle_frame(particles_submitted);
        self.pending_frame_profile = Some(FrameProfile {
            cpu,
            gpu,
            draws: draw_stats.get(),
        });
        match submitted {
            Ok(outcome) => Ok(outcome),
            Err(SubmitAndPresentError::BeforeSubmit(error)) => {
                self.miss_temporal_submissions(&temporal_submissions);
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                Err(error)
            }
            Err(SubmitAndPresentError::AfterSubmit(error)) => Err(error),
        }
    }
}
