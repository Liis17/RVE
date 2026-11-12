//! Кадр окна с одной сценой (`Renderer::render_frame_with_outcome`).

use crate::*;

impl Renderer {
    /// Renders a frame and reports if its command buffer was successfully submitted.
    pub fn render_frame_with_outcome(
        &mut self,
        frame: &Frame<'_>,
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
        validate_debug_view(frame.debug_view, self.capabilities.wireframe)?;
        validate_draws(frame.draws, &self.meshes, &self.materials)?;
        self.validate_skin_draws(frame)?;
        validate_overlay(frame.overlay)?;
        validate_frame_ui(frame.ui)?;
        self.validate_screen_shaders(&frame.view.screen_shaders)?;
        self.validate_color_lut(&frame.view.post)?;
        let view = effective_view_settings(frame.view, frame.debug_view);
        self.miss_unlisted_viewports(&[]);
        let output_extent = self
            .swapchain
            .as_ref()
            .map_or(self.output_extent, |swapchain| swapchain.extent);
        let render_extent = scaled_extent(output_extent, frame.view.render_scale);
        let post_frame = PostFrame::new(
            &view.post,
            frame.debug_view,
            (render_extent.width, render_extent.height),
            (output_extent.width, output_extent.height),
            frame_serial,
            camera_near_far(frame.camera.projection),
            &view.screen_shaders,
        );
        let temporal_plan = self.screen_temporal.state.plan(
            temporal_key(
                render_extent,
                output_extent,
                view.anti_aliasing,
                frame.debug_view.code(),
            ),
            multiply_4x4(frame.camera.projection, frame.camera.view),
            frame.reset_history,
        );
        let prepared = prepare_frame_with_temporal(frame, render_extent, Some(temporal_plan))?;
        let (environment_set, skybox) = self.environment_set_for(frame.environment)?;
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            self.screen_temporal.state.missed();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        }
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.screen_temporal.state.missed();
            self.recreate_swapchain()?;
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        }
        // Размер swapchain стабилен после проверки выше: UI упаковывается под его extent.
        let mut frame_ui = PackedUi::default();
        frame_ui.append(frame.ui, output_extent)?;
        let frame_ui_draws = self.ui_texture_draws(&frame_ui.draws)?;
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
        let skin_plan = self.plan_skin(&[frame], frame_slot)?;
        let skin_record = self.skin_record(skin_plan.as_ref());
        let (instance_buffer, _) = self.frames[frame_slot].write_instances(
            &instance,
            &device,
            self.physical_device,
            [frame.draws],
        )?;
        let instances = InstanceRange {
            buffer: instance_buffer,
            first: 0,
        };
        if view.anti_aliasing.is_temporal() {
            ensure_temporal_history(
                &instance,
                &device,
                self.physical_device,
                &mut self.screen_temporal,
                output_extent,
                &mut self.retired,
            )?;
        }
        let submitted_at = Instant::now();
        #[cfg(windows)]
        let frame_time_delta_ms =
            temporal_frame_delta_ms(self.screen_temporal.last_submitted_at, submitted_at);
        #[cfg(windows)]
        let mut fsr_fallback_reason = None;
        if view.anti_aliasing == AntiAliasing::Fsr {
            #[cfg(windows)]
            if self.capabilities.fsr {
                let error = if let Some(api) = self.fsr_api.as_ref().cloned() {
                    ensure_fsr_view(
                        &instance,
                        &device,
                        self.physical_device,
                        &mut self.screen_temporal,
                        &api,
                        render_extent,
                        output_extent,
                        &mut self.retired,
                    )
                    .err()
                } else {
                    Some("FSR capability has no loaded API".to_owned())
                };
                if let Some(error) = error {
                    warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                    fsr_fallback_reason = Some(error);
                }
            } else {
                let error = self
                    .fsr_load_error
                    .clone()
                    .unwrap_or_else(|| "FSR runtime is unavailable".to_owned());
                warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                fsr_fallback_reason = Some(error);
            }
            #[cfg(not(windows))]
            {
                let error = "FSR is only available on Windows".to_owned();
                warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
            }
        } else if view.anti_aliasing == AntiAliasing::Dlss {
            #[cfg(all(windows, feature = "dlss"))]
            let error = if self.capabilities.dlss {
                ensure_dlss_view(
                    &instance,
                    &device,
                    self.physical_device,
                    &mut self.screen_temporal,
                    render_extent,
                    output_extent,
                    &mut self.retired,
                )
                .err()
            } else {
                Some(
                    self.dlss_load_error
                        .clone()
                        .unwrap_or_else(|| "DLSS runtime is unavailable".to_owned()),
                )
            };
            #[cfg(not(all(windows, feature = "dlss")))]
            let error = Some(
                self.dlss_load_error
                    .clone()
                    .unwrap_or_else(|| "DLSS support is unavailable".to_owned()),
            );
            if let Some(error) = error {
                warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                #[cfg(windows)]
                {
                    fsr_fallback_reason = Some(error);
                }
            }
        }
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let swapchain_handle = swapchain.handle;
        let swapchain_image_count = swapchain.images.len();
        let queue = self.queue;
        let taa_set_layout = self.taa_set_layout()?;
        let bloom_layout = self.texture_set_layout()?;
        let bloom_prefilter_layout = self
            .bloom_pipelines
            .ok_or(RenderError::InvalidState("bloom pipelines отсутствуют"))?
            .prefilter_set_layout();
        let ao_layout = self
            .ao_pipelines
            .ok_or(RenderError::InvalidState("ao pipelines отсутствуют"))?
            .set_layout();
        let white_ao_view = self
            .textures
            .get(WHITE_LINEAR_TEXTURE)
            .and_then(Option::as_ref)
            .map(|texture| texture.image.view)
            .ok_or(RenderError::InvalidState("белая текстура отсутствует"))?;
        let bloom_pipelines = self.bloom_pipelines;
        let exposure_layout = self
            .exposure_pipelines
            .ok_or(RenderError::InvalidState("exposure pipelines отсутствуют"))?
            .set_layout();
        let exposure_pipelines = self.exposure_pipelines;
        let filter_layout = self
            .filter_pipelines
            .ok_or(RenderError::InvalidState("filter pipelines отсутствуют"))?
            .set_layout();
        let filter_pipelines = self.filter_pipelines;
        let ao_pipelines = self.ao_pipelines;
        let opaque_equal_pipeline = self.opaque_equal_pipeline;
        let prepass_pipeline = self.prepass_pipeline;
        let exposure_dt_ms =
            temporal_frame_delta_ms(self.screen_temporal.last_submitted_at, submitted_at);
        let view_exposure = prepare_view_exposure(
            &instance,
            &device,
            self.physical_device,
            self.screen_temporal.exposure,
            &mut self.screen_temporal.exposure_resources,
            post_frame.plan.auto_exposure,
            frame.reset_history,
            render_extent,
            exposure_dt_ms,
        )?;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let image_available = frame_resources.image_available;
        let command_buffer = frame_resources.command_buffer;

        let gpu = frame_resources.read_gpu_timings(
            &device,
            self.timestamp_valid_bits,
            self.timestamp_period,
        )?;
        if let Some(total) = gpu.and_then(|gpu| gpu.first_view_total()) {
            self.pending_gpu_time = Some(total);
        }
        // Fence слота дождались, поэтому GPU больше не читает вершины оверлея этого слота.
        frame_resources.write_overlay(&instance, &device, self.physical_device, frame.overlay)?;
        if !frame_ui_draws.is_empty() {
            frame_resources.write_ui(&instance, &device, self.physical_device, &frame_ui)?;
        }
        let timestamps = frame_resources.timestamps;
        let mut gpu_layout = GpuProfileLayout::default();
        let screen_marks = timestamps.and_then(|pool| {
            gpu_layout
                .add_view(ProfiledView::Screen)
                .map(|first| GpuMarks { pool, first })
        });
        let output_marks = timestamps.map(|pool| GpuMarks {
            pool,
            first: OUTPUT_QUERY_FIRST,
        });
        frame_resources.gpu_layout = gpu_layout;
        let frame_uniforms =
            self.frame_uniforms
                .get_mut(frame_slot)
                .ok_or(RenderError::InvalidState(
                    "нет uniform-буфера текущего кадра",
                ))?;
        // Fence слота дождались, поэтому GPU больше не читает этот буфер.
        frame_uniforms.buffer.write(&prepared.uniforms)?;
        frame_uniforms
            .local_shadow_matrices
            .write(&local_shadow_matrix_bytes(&prepared.local_shadow_matrices)?)?;
        let replaced_light_buffer = frame_uniforms.local_lights.write_keep_old(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &prepared.light_bytes,
        )?;
        if replaced_light_buffer.is_some() {
            let light_buffer = frame_uniforms
                .local_lights
                .buffer
                .as_ref()
                .ok_or(RenderError::InvalidState("local light buffer отсутствует"))?;
            write_local_light_descriptor(
                &device,
                frame_uniforms.set,
                light_buffer,
                frame_uniforms.local_lights.capacity,
            );
        }
        drop(replaced_light_buffer);
        frame_uniforms
            .cluster_bounds
            .write(cluster_bounds_bytes(&prepared.cluster_bounds))?;
        let frame_set = frame_uniforms.set;
        // Цели создаются при первом кадре: редактору они не нужны. Выделение идёт до acquire,
        // чтобы ошибка памяти не оставила захваченный image и сигнальный semaphore. При смене
        // масштаба заменяется только target текущего слота после ожидания его fence.
        let post_sampler = self
            .post_sampler
            .ok_or(RenderError::InvalidState("post sampler отсутствует"))?;
        let shadow_pipeline = self
            .shadow_pipeline
            .ok_or(RenderError::InvalidState("shadow pipeline отсутствует"))?;
        let local_shadow_pipeline = self.local_shadow_pipeline.ok_or(RenderError::InvalidState(
            "local shadow pipeline отсутствует",
        ))?;
        let pipeline_layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
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
        let tonemap_pipeline = self
            .tonemap_pipeline
            .ok_or(RenderError::InvalidState("tonemap pipeline отсутствует"))?;
        let output_pipeline = self
            .output_pipeline
            .ok_or(RenderError::InvalidState("output pipeline отсутствует"))?;
        let ui_pipeline = self
            .ui_pipeline
            .ok_or(RenderError::InvalidState("UI pipeline отсутствует"))?;
        let ui_pipeline_layout = self
            .ui_pipeline_layout
            .ok_or(RenderError::InvalidState("UI pipeline layout отсутствует"))?;
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
        if view.anti_aliasing.is_temporal() && self.screen_temporal.history.is_none() {
            return Err(RenderError::InvalidState("TAA history отсутствует"));
        }
        let overlay_resources =
            if frame.overlay.is_empty() || frame_resources.overlay_buffer.buffer.is_none() {
                None
            } else {
                Some((
                    self.overlay_pipeline
                        .ok_or(RenderError::InvalidState("overlay pipeline отсутствует"))?,
                    self.overlay_pipeline_layout
                        .ok_or(RenderError::InvalidState(
                            "overlay pipeline layout отсутствует",
                        ))?,
                ))
            };
        let target_changed = frame_resources.target.as_ref().is_none_or(|target| {
            target.render_extent != render_extent || target.output_extent != output_extent
        });
        if target_changed {
            let target = FrameTarget::new(
                &instance,
                &device,
                self.physical_device,
                render_extent,
                output_extent,
                taa_set_layout,
                self.particles.depth_layout()?,
                exposure_layout,
            )?;
            write_image_set(
                &device,
                frame_resources.output_set,
                target.ldr.view,
                post_sampler,
            );
            frame_resources.target = Some(target);
        }
        if post_frame.plan.bloom_mips > 0
            && let Some(target) = frame_resources.target.as_mut()
            && target.bloom.is_none()
        {
            target.bloom = Some(BloomChain::new(
                &instance,
                &device,
                self.physical_device,
                target.render_extent,
                post_frame.plan.bloom_mips,
                bloom_layout,
                bloom_prefilter_layout,
                post_sampler,
            )?);
        }
        if post_frame.plan.ambient_occlusion
            && let Some(target) = frame_resources.target.as_mut()
        {
            ensure_ao_target(
                &instance,
                &device,
                self.physical_device,
                &mut target.scene,
                target.render_extent,
                post_frame.ao.quality,
                ao_layout,
            )?;
        }
        let occlusion_pipelines = self.occlusion_pipelines.filter(|_| {
            prepass_pipeline.is_some() && occlusion_culling_enabled(frame.debug_view, frame.draws)
        });
        if let (Some(pipelines), Some(target)) =
            (occlusion_pipelines, frame_resources.target.as_mut())
        {
            prepare_occlusion(
                &instance,
                &device,
                self.physical_device,
                &mut target.scene,
                target.render_extent,
                pipelines,
                frame.draws,
                &self.meshes,
            )?;
        }
        let frame_ao_view = frame_resources
            .target
            .as_ref()
            .and_then(|target| target.scene.ao.as_ref())
            .filter(|_| post_frame.plan.ambient_occlusion)
            .map_or(white_ao_view, |ao| ao.filtered.view);
        write_frame_ao(&device, frame_set, frame_ao_view);
        if post_frame.plan.motion_blur || post_frame.plan.depth_of_field {
            let filter_extent = if view.anti_aliasing.is_temporal() {
                output_extent
            } else {
                render_extent
            };
            if let Some(target) = frame_resources.target.as_mut()
                && target
                    .filter_scratch
                    .as_ref()
                    .is_none_or(|scratch| scratch.extent != filter_extent)
            {
                target.filter_scratch = Some(FilterScratch::new(
                    &instance,
                    &device,
                    self.physical_device,
                    filter_extent,
                    filter_layout,
                )?);
            }
        }
        let screen_set_layout = self
            .screen_shader_layout
            .as_ref()
            .map(|layout| layout.set_layout);
        let screen_before_extent = if view.anti_aliasing.is_temporal() {
            output_extent
        } else {
            render_extent
        };
        if let Some(target) = frame_resources.target.as_mut() {
            prepare_screen_scratch(
                &mut target.screen_before,
                post_frame.screen_before.len(),
                screen_before_extent,
                OFFSCREEN_FORMAT,
                &instance,
                &device,
                self.physical_device,
                screen_set_layout,
            )?;
            prepare_screen_scratch(
                &mut target.screen_after,
                post_frame.screen_after.len(),
                output_extent,
                LDR_FORMAT,
                &instance,
                &device,
                self.physical_device,
                screen_set_layout,
            )?;
        }
        let target = frame_resources
            .target
            .as_ref()
            .ok_or(RenderError::InvalidState("offscreen target отсутствует"))?;
        let tonemap_source = if view.anti_aliasing.is_temporal() {
            let history = self
                .screen_temporal
                .history
                .as_ref()
                .ok_or(RenderError::InvalidState("TAA history отсутствует"))?;
            write_taa_image_set(
                &device,
                target.taa_set,
                post_sampler,
                [
                    target.scene.color.view,
                    target.scene.velocity.view,
                    target.scene.depth.view,
                    history.images[temporal_plan.read_history].view,
                ],
            );
            history.images[temporal_plan.write_history].view
        } else {
            target.scene.color.view
        };
        write_image_set(
            &device,
            frame_resources.tonemap_set,
            tonemap_source,
            post_sampler,
        );
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
                self.screen_temporal.state.missed();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };
        let Some((image_index, acquire_suboptimal)) = acquired else {
            self.screen_temporal.state.missed();
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
        let output_is_srgb = is_srgb_format(self.surface_format.format);
        let view_projection = multiply_4x4(frame.camera.projection, frame.camera.view);
        let particle_works = self.particles.plan_view(
            frame.particles,
            frame.particle_step,
            view_projection,
            frame.debug_view,
            frame.camera.position,
        )?;
        let particle_pass = self.particle_pipelines().map(|pipelines| ParticlePass {
            works: &particle_works,
            pipelines,
            frame_set,
        });
        let record_started = Instant::now();
        let screen_context = ScreenContext {
            shaders: &self.screen_shaders,
            layout: self
                .screen_shader_layout
                .as_ref()
                .map_or(vk::PipelineLayout::null(), |layout| layout.pipeline_layout),
        };
        let recorded = (|| {
            let shadow = ShadowPass {
                pipeline: shadow_pipeline,
                local_pipeline: local_shadow_pipeline,
                pipeline_layout,
                frame_set,
                meshes: &self.meshes,
                materials: &self.materials,
                draws: frame.draws,
                skin_draws: skin::skin_draws_for(skin_plan.as_ref(), 0),
                instances,
                shadow_cascades: frame.shadow_cascades,
                local_shadow_matrices: &prepared.local_shadow_matrices,
                stats: &draw_stats,
            };
            let scene = ScenePass {
                pipeline: scene_pipeline,
                mask_pipeline,
                blend_pipeline,
                overdraw_pipeline,
                wireframe_pipeline: self.wireframe_pipeline,
                pipeline_layout,
                frame_set,
                cluster_pipeline,
                cluster_pipeline_layout,
                environment_set,
                skybox_pipeline,
                skybox,
                meshes: &self.meshes,
                materials: &self.materials,
                surface_shaders: &self.surface_shaders,
                draws: frame.draws,
                skin_draws: skin::skin_draws_for(skin_plan.as_ref(), 0),
                instances,
                view_projection: multiply_4x4(frame.camera.projection, frame.camera.view),
                #[cfg(windows)]
                fsr_camera: fsr_camera_parameters(frame.camera.projection),
                culling_padding: [
                    temporal_plan.jitter_ndc[0].abs(),
                    temporal_plan.jitter_ndc[1].abs(),
                ],
                temporal_plan,
                camera_position: frame.camera.position,
                clear_color: frame.clear_color,
                debug_view: frame.debug_view,
                timestamps: screen_marks,
                view,
                post: post_frame,
                bloom_pipelines,
                exposure_pipelines,
                exposure_dt_ms,
                color_lut: color_lut_view(
                    &self.color_luts,
                    post::planned_color_lut(&view.post, &post_frame.plan),
                )
                .ok_or(RenderError::InvalidState("LUT цветокоррекции отсутствует"))?,
                reset_history: frame.reset_history,
                filter_pipelines,
                ao_pipelines,
                opaque_equal_pipeline,
                prepass_pipeline,
                depth_prepass: (post_frame.plan.depth_prepass || occlusion_pipelines.is_some())
                    && prepass_pipeline.is_some(),
                occlusion: occlusion_pipelines,
                projection_scale: [frame.camera.projection[0][0], frame.camera.projection[1][1]],
                particles: particle_pass,
                stats: &draw_stats,
            };
            let frame_resources = &mut self.frames[frame_slot];
            let overlay = match (&frame_resources.overlay_buffer.buffer, overlay_resources) {
                (Some(buffer), Some((pipeline, pipeline_layout))) => Some(OverlayPass {
                    pipeline,
                    pipeline_layout,
                    vertex_buffer: buffer.buffer,
                    // validate_overlay проверил, что число вершин помещается в u32.
                    vertex_count: frame.overlay.len() as u32,
                }),
                _ => None,
            };
            let ui = UiDrawPass::new(
                ui_pipeline,
                ui_pipeline_layout,
                &frame_resources.ui_vertices,
                &frame_resources.ui_indices,
                output_is_srgb,
                &frame_ui_draws,
            );
            let target = frame_resources
                .target
                .as_mut()
                .expect("frame target was created before acquire");
            let mut shadow_layout = self.shadow_layout;
            let mut local_shadow_layout = self.local_shadow_layout;
            #[cfg(windows)]
            let temporal_backend = select_temporal_backend(
                view.anti_aliasing,
                self.capabilities.fsr,
                self.capabilities.dlss,
            );
            let temporal_record = if view.anti_aliasing.is_temporal() {
                #[cfg(all(windows, feature = "dlss"))]
                let dlss_api = self.dlss_api.as_ref();
                let temporal = &mut self.screen_temporal;
                let history = temporal
                    .history
                    .as_ref()
                    .expect("temporal history was validated before acquire");
                #[cfg(windows)]
                let fsr = (temporal_backend == TemporalBackend::Fsr
                    && fsr_fallback_reason.is_none())
                .then_some(temporal.fsr.as_deref_mut())
                .flatten();
                #[cfg(all(windows, feature = "dlss"))]
                let dlss = (temporal_backend == TemporalBackend::Dlss
                    && fsr_fallback_reason.is_none())
                .then_some(temporal.dlss.as_deref_mut())
                .flatten();
                Some(TemporalResolveRecord {
                    history,
                    #[cfg(windows)]
                    backend: temporal_backend,
                    plan: temporal_plan,
                    taa_pipeline,
                    taa_pipeline_layout,
                    #[cfg(windows)]
                    fsr,
                    #[cfg(windows)]
                    exposure: UpscalerExposure::new(
                        view.post.exposure.ev,
                        view_exposure.adaptation.map(|_| view_exposure.buffer),
                    ),
                    #[cfg(windows)]
                    frame_time_delta_ms,
                    #[cfg(windows)]
                    camera: fsr_camera_parameters(frame.camera.projection),
                    #[cfg(all(windows, feature = "dlss"))]
                    dlss_api,
                    #[cfg(all(windows, feature = "dlss"))]
                    dlss,
                })
            } else {
                None
            };
            let temporal_outcome = record_frame(
                &device,
                command_buffer,
                target,
                shadow_image,
                &mut shadow_layout,
                local_shadow_image,
                &mut local_shadow_layout,
                swapchain_image,
                swapchain_view,
                swapchain_extent,
                view,
                frame_resources.tonemap_set,
                post_sampler,
                frame_resources.output_set,
                tonemap_pipeline,
                output_pipeline,
                post_pipeline_layout,
                output_is_srgb,
                &shadow,
                &scene,
                view_exposure,
                overlay.as_ref(),
                ui.as_ref(),
                temporal_record,
                skin_record,
                screen_context,
                output_marks,
                &present_copies,
            )?;
            Ok((temporal_outcome, shadow_layout, local_shadow_layout))
        })();
        cpu.record = record_started.elapsed();
        let (temporal_outcome, shadow_layout, local_shadow_layout) = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                self.fail_capture_submissions(
                    &window_captures,
                    "frame recording failed before window capture",
                );
                self.screen_temporal.state.missed();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };
        if let Some(error) = temporal_outcome
            .as_ref()
            .and_then(|outcome| outcome.fsr_error.as_deref())
        {
            warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, error);
            #[cfg(windows)]
            if let Some(fsr) = self.screen_temporal.fsr.take() {
                self.retired.retire(RetiredResource::FsrView(fsr), 0);
                self.screen_temporal.fsr_error = Some(error.to_owned());
                self.screen_temporal.fsr_attempted = true;
            }
        }
        #[cfg(all(windows, feature = "dlss"))]
        if let Some(error) = temporal_outcome
            .as_ref()
            .and_then(|outcome| outcome.dlss_error.as_deref())
        {
            warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, error);
            self.capabilities.dlss = false;
            self.dlss_load_error = Some(error.to_owned());
            #[cfg(all(windows, feature = "dlss"))]
            if let Some(dlss) = self.screen_temporal.dlss.take() {
                self.retired.retire(RetiredResource::DlssView(dlss), 0);
                self.screen_temporal.dlss_error = Some(error.to_owned());
                self.screen_temporal.dlss_attempted = true;
            }
        }
        let temporal_submissions = [TemporalSubmission {
            view: TemporalViewId::Screen,
            plan: temporal_plan,
            exposure: view_exposure.adaptation,
            layouts: view
                .anti_aliasing
                .is_temporal()
                .then_some([vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL; 2])
                .filter(|_| {
                    temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_taa)
                }),
            #[cfg(windows)]
            fsr_used: temporal_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.used_fsr),
            #[cfg(all(windows, feature = "dlss"))]
            dlss_used: temporal_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.used_dlss),
            submitted_at,
        }];
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
            timestamps_written: timestamps.is_some(),
            timings: &mut cpu,
            temporal_submissions: &temporal_submissions,
            capture_submissions: window_captures,
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
                self.screen_temporal.state.missed();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                Err(error)
            }
            Err(SubmitAndPresentError::AfterSubmit(error)) => Err(error),
        }
    }
}
