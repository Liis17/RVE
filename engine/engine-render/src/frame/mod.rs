//! Кадр: подготовка данных, отправка командного буфера и present.

use crate::*;

mod viewports;
mod window;

impl Renderer {
    /// Очищает offscreen-таргет, рисует меши и масштабирует результат в swapchain.
    pub fn render_frame(&mut self, frame: &Frame<'_>) -> Result<FrameStatus, RenderError> {
        self.render_frame_with_outcome(frame)
            .map(|outcome| outcome.status)
    }

    /// Рисует сцены в их viewport targets и UI прямо в swapchain image.
    ///
    /// `clear` очищает окно перед UI и задаётся в значениях формата swapchain. `Frame::overlay`
    /// здесь не используется. Каждый viewport может встречаться в `scenes` один раз, и
    /// `UiImage::Viewport` может ссылаться только на них; примитивы viewport нулевого размера
    /// пропускаются вместе с его scene pass. GPU-время измеряет только первую нарисованную сцену.
    pub fn render_frame_with_ui(
        &mut self,
        scenes: &[(ViewportId, &Frame<'_>)],
        ui: &[UiPrimitive<'_>],
        clear: [f32; 4],
    ) -> Result<FrameStatus, RenderError> {
        self.render_frame_with_ui_outcome(scenes, ui, clear)
            .map(|outcome| outcome.status)
    }

    /// Сопоставляет вызовы вида с наборами текстур; вид не может ссылаться на viewport.
    pub(crate) fn ui_texture_draws(
        &self,
        draws: &[UiDraw],
    ) -> Result<Vec<(UiDraw, vk::DescriptorSet, bool)>, RenderError> {
        draws
            .iter()
            .map(|draw| {
                let UiImage::Texture(texture) = draw.image else {
                    return Err(RenderError::InvalidState("UI вида ссылается на viewport"));
                };
                Ok((*draw, self.ui_texture(texture)?.set, false))
            })
            .collect()
    }

    pub(crate) fn ui_texture(&self, texture: UiTextureId) -> Result<&UiTexture, RenderError> {
        self.ui_textures
            .get(texture.0)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                RenderError::InvalidTexture("UiTextureId не принадлежит renderer".to_owned())
            })
    }

    pub(crate) fn validate_ui_images(
        &self,
        packed: &PackedUi,
        scenes: &[(ViewportId, &Frame<'_>)],
    ) -> Result<(), RenderError> {
        for draw in &packed.draws {
            match draw.image {
                UiImage::Texture(texture) => {
                    self.ui_texture(texture)?;
                }
                UiImage::Viewport(viewport)
                    if !scenes.iter().any(|(scene, _)| *scene == viewport) =>
                {
                    return Err(RenderError::InvalidTexture(
                        "UI ссылается на viewport, который не рисуется в этом кадре".to_owned(),
                    ));
                }
                UiImage::Viewport(_) => {}
            }
        }
        Ok(())
    }

    pub(crate) fn miss_all_temporal_views(&mut self) {
        self.screen_temporal.state.missed();
        for viewport in self.viewports.iter_mut().flatten() {
            viewport.temporal.state.missed();
        }
    }

    pub(crate) fn miss_unlisted_viewports(&mut self, scenes: &[(ViewportId, &Frame<'_>)]) {
        for (index, viewport) in self.viewports.iter_mut().enumerate() {
            if let Some(viewport) = viewport
                && !scenes.iter().any(|(listed, _)| listed.0 == index)
            {
                viewport.temporal.state.missed();
            }
        }
    }

    pub(crate) fn commit_temporal_submissions(&mut self, submissions: &[TemporalSubmission]) {
        for submission in submissions {
            let temporal = match submission.view {
                TemporalViewId::Screen => &mut self.screen_temporal,
                TemporalViewId::Viewport(viewport) => {
                    let Some(Some(viewport)) = self.viewports.get_mut(viewport.0) else {
                        continue;
                    };
                    &mut viewport.temporal
                }
            };
            temporal.state.submitted(submission.plan);
            temporal.last_submitted_at = Some(submission.submitted_at);
            temporal.exposure.submitted(submission.exposure);
            if let (Some(history), Some(layouts)) = (&mut temporal.history, submission.layouts) {
                history.layouts = layouts;
            }
        }
    }

    pub(crate) fn miss_temporal_submissions(&mut self, submissions: &[TemporalSubmission]) {
        for submission in submissions {
            let temporal = match submission.view {
                TemporalViewId::Screen => Some(&mut self.screen_temporal),
                TemporalViewId::Viewport(viewport) => self
                    .viewports
                    .get_mut(viewport.0)
                    .and_then(Option::as_mut)
                    .map(|viewport| &mut viewport.temporal),
            };
            if let Some(temporal) = temporal {
                temporal.state.missed();
                #[cfg(windows)]
                if submission.fsr_used {
                    if let Some(fsr) = temporal.fsr.take() {
                        self.retired.retire(RetiredResource::FsrView(fsr), 0);
                    }
                    temporal.fsr_attempted = false;
                    temporal.fsr_error = None;
                }
                #[cfg(all(windows, feature = "dlss"))]
                if submission.dlss_used {
                    if let Some(dlss) = temporal.dlss.take() {
                        self.retired.retire(RetiredResource::DlssView(dlss), 0);
                    }
                    temporal.dlss_attempted = false;
                    temporal.dlss_error = None;
                }
            }
        }
    }

    /// Отправляет записанный command buffer текущего слота и показывает изображение.
    ///
    /// `wait_stage` — стадия первого обращения к swapchain image в этом command buffer.
    pub(crate) fn submit_and_present(
        &mut self,
        acquired: AcquiredFrameSubmission<'_>,
    ) -> Result<FrameOutcome, SubmitAndPresentError> {
        let AcquiredFrameSubmission {
            device,
            swapchain_loader,
            queue,
            frame_slot,
            fence,
            image_available,
            command_buffer,
            swapchain_handle,
            render_finished,
            image_index,
            acquire_suboptimal,
            wait_stage,
            timestamps_written,
            timings,
            temporal_submissions,
            capture_submissions,
            shadow_layout,
            local_shadow_layout,
            skin,
            present_id,
        } = acquired;
        let submit_started = Instant::now();
        // Fence сбрасывается только после успешного acquire и полной записи command buffer.
        // SAFETY: эта точка — непосредственно перед submit, которому принадлежит fence.
        if let Err(error) = unsafe { device.reset_fences(&[fence]) } {
            self.fail_capture_submissions(
                &capture_submissions,
                "frame submission failed before viewport capture",
            );
            return Err(SubmitAndPresentError::BeforeSubmit(RenderError::Vulkan(
                error,
            )));
        }

        let wait_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(image_available)
            .stage_mask(wait_stage)
            .device_index(0)];
        let command_infos = [vk::CommandBufferSubmitInfo::default()
            .command_buffer(command_buffer)
            .device_mask(0)];
        let signal_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(render_finished)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .device_index(0)];
        let mut latency_submission =
            vk::LatencySubmissionPresentIdNV::default().present_id(present_id);
        let mut submit_info = vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait_infos)
            .command_buffer_infos(&command_infos)
            .signal_semaphore_infos(&signal_infos);
        if present_id != 0 {
            submit_info = submit_info.push_next(&mut latency_submission);
        }
        let submit_info = [submit_info];
        // SAFETY: command buffer, semaphores and fence are owned by this renderer and are synchronized per frame/image.
        if let Err(error) = unsafe { device.queue_submit2(queue, &submit_info, fence) } {
            self.fail_capture_submissions(
                &capture_submissions,
                "frame submission failed before viewport capture",
            );
            return Err(SubmitAndPresentError::BeforeSubmit(RenderError::Vulkan(
                error,
            )));
        }
        self.commit_capture_submissions(capture_submissions);
        timings.submit = submit_started.elapsed();
        if present_id != 0 {
            self.present_ids.submitted(present_id);
            self.latency_marker(present_id, vk::LatencyMarkerNV::RENDERSUBMIT_END);
        }
        self.record_frame_submission(frame_slot);
        self.commit_skin(skin);
        self.shadow_layout = shadow_layout;
        self.local_shadow_layout = local_shadow_layout;
        self.commit_temporal_submissions(temporal_submissions);
        // Результаты timestamp читаются после следующего ожидания fence этого слота.
        self.frames[frame_slot].timestamps_pending = timestamps_written;

        let wait_semaphores = [render_finished];
        let swapchains = [swapchain_handle];
        let image_indices = [image_index as u32];
        let present_ids = [present_id];
        let mut present_id_info = vk::PresentIdKHR::default().present_ids(&present_ids);
        let mut present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&wait_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);
        if present_id != 0 {
            present_info = present_info.push_next(&mut present_id_info);
        }
        self.latency_marker(present_id, vk::LatencyMarkerNV::PRESENT_START);
        // SAFETY: present waits on the semaphore signaled by the preceding queue submission.
        let present_started = Instant::now();
        let present_result = unsafe { swapchain_loader.queue_present(queue, &present_info) };
        self.latency_marker(present_id, vk::LatencyMarkerNV::PRESENT_END);
        timings.present = present_started.elapsed();
        self.frame_slot = (self.frame_slot + 1) % self.frames.len();
        match present_result {
            Ok(present_suboptimal) => {
                self.retired_swapchains.presented(self.swapchain_generation);
                if acquire_suboptimal || present_suboptimal {
                    self.swapchain_dirty = true;
                }
                Ok(FrameOutcome {
                    status: FrameStatus::Presented,
                    submitted: true,
                })
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.swapchain_dirty = true;
                Ok(FrameOutcome {
                    status: FrameStatus::Skipped,
                    submitted: true,
                })
            }
            Err(error) => {
                self.swapchain_dirty = true;
                Err(SubmitAndPresentError::AfterSubmit(RenderError::Vulkan(
                    error,
                )))
            }
        }
    }

    pub(crate) fn abandon_acquired_frame(
        &mut self,
        frame_slot: usize,
        device: &ash::Device,
        queue: vk::Queue,
        image_available: vk::Semaphore,
    ) {
        self.swapchain_dirty = true;
        if let Some(frame) = self.frames.get_mut(frame_slot) {
            frame.target = None;
            for viewport in self.viewports.iter_mut().flatten() {
                if let Some(target) = viewport.targets.get_mut(frame_slot) {
                    target.take();
                }
            }
        } else {
            self.frame_slots_lost = true;
            return;
        }

        let fence = self.frames[frame_slot].fence;
        let reset = unsafe { device.reset_fences(&[fence]) };
        if reset.is_ok() {
            let wait_infos = [vk::SemaphoreSubmitInfo::default()
                .semaphore(image_available)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .device_index(0)];
            let submit_info = [vk::SubmitInfo2::default().wait_semaphore_infos(&wait_infos)];
            // SAFETY: the empty submit consumes the semaphore signaled by acquire and re-signals the slot fence.
            if unsafe { device.queue_submit2(queue, &submit_info, fence) }.is_ok() {
                self.record_frame_submission(frame_slot);
                self.frame_slot = (frame_slot + 1) % self.frames.len();
                return;
            }
        }

        // SAFETY: the old synchronization objects may have uncertain state after a failed recovery submit.
        let _ = unsafe { device.device_wait_idle() };
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: this new fence starts signaled and is not referenced by any submission.
        let new_fence = match unsafe { device.create_fence(&fence_info, None) } {
            Ok(fence) => fence,
            Err(_) => {
                self.frame_slots_lost = true;
                return;
            }
        };
        // SAFETY: a default create info is valid for a new binary semaphore.
        let new_image_available =
            match unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) } {
                Ok(semaphore) => semaphore,
                Err(_) => {
                    // SAFETY: new_fence was created above and has not been submitted.
                    unsafe { device.destroy_fence(new_fence, None) };
                    self.frame_slots_lost = true;
                    return;
                }
            };

        let frame = &mut self.frames[frame_slot];
        let old_fence = std::mem::replace(&mut frame.fence, new_fence);
        let old_image_available =
            std::mem::replace(&mut frame.image_available, new_image_available);
        // SAFETY: device_wait_idle was attempted before replacing and destroying these objects.
        unsafe {
            device.destroy_fence(old_fence, None);
            device.destroy_semaphore(old_image_available, None);
        }
        self.frame_slot = (frame_slot + 1) % self.frames.len();
    }

    pub(crate) fn record_frame_submission(&mut self, frame_slot: usize) {
        self.submission_generation = self.submission_generation.saturating_add(1);
        self.frames[frame_slot].submission_generation = self.submission_generation;
    }
}

pub(crate) enum SubmitAndPresentError {
    BeforeSubmit(RenderError),
    AfterSubmit(RenderError),
}

pub(crate) struct AcquiredFrameSubmission<'a> {
    pub(crate) device: &'a ash::Device,
    pub(crate) swapchain_loader: &'a ash::khr::swapchain::Device,
    pub(crate) queue: vk::Queue,
    pub(crate) frame_slot: usize,
    pub(crate) fence: vk::Fence,
    pub(crate) image_available: vk::Semaphore,
    pub(crate) command_buffer: vk::CommandBuffer,
    pub(crate) swapchain_handle: vk::SwapchainKHR,
    pub(crate) render_finished: vk::Semaphore,
    pub(crate) image_index: usize,
    pub(crate) acquire_suboptimal: bool,
    pub(crate) wait_stage: vk::PipelineStageFlags2,
    pub(crate) timestamps_written: bool,
    pub(crate) timings: &'a mut RenderCpuTimings,
    pub(crate) temporal_submissions: &'a [TemporalSubmission],
    pub(crate) capture_submissions: Vec<CaptureSubmission>,
    pub(crate) shadow_layout: vk::ImageLayout,
    pub(crate) local_shadow_layout: vk::ImageLayout,
    /// План skinning вызова; состояние кольца фиксируется после отправки.
    pub(crate) skin: Option<&'a skin::SkinPlan>,
    /// Present id кадра Reflex; 0, если кадр без Reflex.
    pub(crate) present_id: u64,
}

/// Результат попытки показать кадр.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStatus {
    Presented,
    Skipped,
}

/// Presentation status and whether this render attempt reached a successful queue submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameOutcome {
    pub status: FrameStatus,
    /// True after `vkQueueSubmit2` succeeds, including a later out-of-date present.
    pub submitted: bool,
}

pub(crate) struct PreparedFrame {
    pub(crate) uniforms: [u8; FRAME_UNIFORMS_SIZE],
    pub(crate) light_bytes: Vec<u8>,
    pub(crate) local_shadow_matrices: Vec<[[f32; 4]; 4]>,
    pub(crate) cluster_bounds: Vec<ClusterBounds>,
}

#[cfg(test)]
pub(crate) fn prepare_frame(
    frame: &Frame<'_>,
    render_extent: vk::Extent2D,
) -> Result<PreparedFrame, RenderError> {
    prepare_frame_with_temporal(frame, render_extent, None)
}

pub(crate) fn prepare_frame_with_temporal(
    frame: &Frame<'_>,
    render_extent: vk::Extent2D,
    temporal: Option<TemporalPlan>,
) -> Result<PreparedFrame, RenderError> {
    let shadow_requests = index_local_light_shadows(frame.lights, frame.local_light_shadows)?;
    let culling_padding = temporal.map_or([0.0; 2], |plan| {
        [plan.jitter_ndc[0].abs(), plan.jitter_ndc[1].abs()]
    });
    let lights = select_local_light_entries(frame.camera, frame.lights, culling_padding)?;
    let (depth_range, cluster_bounds) = match camera_cluster_bounds(frame.camera.projection) {
        Ok(bounds) => bounds,
        // Unlit frames also support clip-space cameras, such as sandbox's triangle.
        Err(_) if lights.is_empty() => (
            clusters::DepthRange {
                near: 1.0,
                far: 2.0,
            },
            vec![ClusterBounds::default(); CLUSTER_COUNT],
        ),
        Err(error) => return Err(error),
    };
    let shadow_plan = select_local_shadow_slots(&lights, &shadow_requests, frame.camera.position);
    let uniforms =
        frame_uniform_bytes_with_count(frame, lights.len(), depth_range, render_extent, temporal)?;
    let light_bytes =
        local_light_bytes_for_selected(&lights, &shadow_plan.assignments, frame.camera.view)?;
    Ok(PreparedFrame {
        uniforms,
        light_bytes,
        local_shadow_matrices: shadow_plan.matrices,
        cluster_bounds,
    })
}

#[cfg(test)]
pub(crate) fn frame_uniform_bytes(
    frame: &Frame<'_>,
) -> Result<[u8; FRAME_UNIFORMS_SIZE], RenderError> {
    prepare_frame(
        frame,
        vk::Extent2D {
            width: 1,
            height: 1,
        },
    )
    .map(|prepared| prepared.uniforms)
}

pub(crate) fn frame_uniform_bytes_with_count(
    frame: &Frame<'_>,
    local_light_count: usize,
    depth_range: DepthRange,
    render_extent: vk::Extent2D,
    temporal: Option<TemporalPlan>,
) -> Result<[u8; FRAME_UNIFORMS_SIZE], RenderError> {
    validate_view_settings(frame.view)?;
    if !frame
        .camera
        .view
        .iter()
        .flatten()
        .all(|value| value.is_finite())
        || !frame
            .camera
            .projection
            .iter()
            .flatten()
            .all(|value| value.is_finite())
        || !frame.camera.position.into_iter().all(f32::is_finite)
    {
        return Err(RenderError::InvalidConfig(
            "camera matrices and position must be finite",
        ));
    }
    if let Some(environment) = frame.environment {
        validate_frame_environment(environment)?;
    }
    let light = frame.light;
    let [x, y, z] = light.direction;
    let length_squared = x * x + y * y + z * z;
    if !length_squared.is_finite() || length_squared == 0.0 {
        return Err(RenderError::InvalidConfig(
            "направление света должно быть конечным и ненулевым",
        ));
    }
    if frame.shadow_cascades.len() > SHADOW_CASCADE_COUNT {
        return Err(RenderError::InvalidConfig(
            "число shadow cascades должно быть не больше четырёх",
        ));
    }
    if frame
        .shadow_cascades
        .iter()
        .any(|matrix| !matrix.iter().flatten().all(|value| value.is_finite()))
    {
        return Err(RenderError::InvalidConfig(
            "матрицы shadow cascades должны быть конечными",
        ));
    }

    let view_projection = multiply_4x4(frame.camera.projection, frame.camera.view);
    let inverse_view_projection = environment::inverse4x4(view_projection).ok_or(
        RenderError::InvalidConfig("camera view-projection matrix must be invertible"),
    )?;
    let mut values = [0.0; FRAME_UNIFORMS_SIZE / size_of::<f32>()];
    values[..16].copy_from_slice(view_projection.as_flattened());
    values[16..32].copy_from_slice(inverse_view_projection.as_flattened());
    for cascade in 0..SHADOW_CASCADE_COUNT {
        let start = 32 + cascade * 16;
        let matrix = frame
            .shadow_cascades
            .get(cascade)
            .unwrap_or(&IDENTITY_MATRIX);
        values[start..start + 16].copy_from_slice(matrix.as_flattened());
    }
    values[96..100].copy_from_slice(&[x, y, z, 0.0]);
    values[100..104].copy_from_slice(&[light.color[0], light.color[1], light.color[2], 0.0]);
    values[104..108].copy_from_slice(&[light.ambient[0], light.ambient[1], light.ambient[2], 0.0]);
    values[108..112].copy_from_slice(&[
        if frame.shadow_cascades.is_empty() {
            0.0
        } else {
            1.0
        },
        1.0 / SHADOW_MAP_RESOLUTION as f32,
        frame.shadow_cascades.len() as f32,
        0.0,
    ]);
    values[112..116].copy_from_slice(&[
        frame.camera.position[0],
        frame.camera.position[1],
        frame.camera.position[2],
        0.0,
    ]);
    let (rotation, intensity, skybox, present) =
        frame
            .environment
            .map_or((IDENTITY_MATRIX, 0.0, 0.0, 0.0), |environment| {
                let [[a, b, c], [d, e, f], [g, h, i]] = environment.rotation;
                (
                    [
                        [a, b, c, 0.0],
                        [d, e, f, 0.0],
                        [g, h, i, 0.0],
                        [0.0, 0.0, 0.0, 1.0],
                    ],
                    environment.intensity,
                    if environment.skybox { 1.0 } else { 0.0 },
                    1.0,
                )
            });
    values[116..132].copy_from_slice(rotation.as_flattened());
    values[132..136].copy_from_slice(&[intensity, skybox, present, 0.0]);
    values[136..140].copy_from_slice(&[local_light_count as f32, 0.0, 0.0, 0.0]);
    values[140..156].copy_from_slice(frame.camera.view.as_flattened());
    values[156..160].copy_from_slice(&[
        depth_range.near,
        depth_range.far,
        render_extent.width.max(1) as f32,
        render_extent.height.max(1) as f32,
    ]);
    let previous_view_projection = temporal
        .map(|plan| plan.previous_view_projection)
        .unwrap_or(view_projection);
    values[160..176].copy_from_slice(previous_view_projection.as_flattened());
    let [jitter_x, jitter_y, lod_bias] = temporal.map_or([0.0; 3], |plan| {
        [plan.jitter_ndc[0], plan.jitter_ndc[1], plan.lod_bias]
    });
    values[176..180].copy_from_slice(&[
        jitter_x,
        jitter_y,
        lod_bias,
        frame.debug_view.code() as f32,
    ]);
    Ok(float_bytes(&values))
}
