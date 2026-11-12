//! Запись проходов кадра в командный буфер и общие барьеры.

use crate::*;

mod draws;
mod overlay;
mod postprocess;
mod scene;
mod upscale;

pub(crate) use draws::*;
pub(crate) use overlay::*;
pub(crate) use postprocess::*;
pub(crate) use scene::*;
pub(crate) use upscale::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_frame(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut FrameTarget,
    shadow_image: &DeviceImage,
    shadow_layout: &mut vk::ImageLayout,
    local_shadow_image: &DeviceImage,
    local_shadow_layout: &mut vk::ImageLayout,
    output_image: vk::Image,
    output_view: vk::ImageView,
    output_extent: vk::Extent2D,
    view: ViewSettings,
    tonemap_set: vk::DescriptorSet,
    post_sampler: vk::Sampler,
    output_set: vk::DescriptorSet,
    tonemap_pipeline: vk::Pipeline,
    output_pipeline: vk::Pipeline,
    post_pipeline_layout: vk::PipelineLayout,
    output_is_srgb: bool,
    shadow: &ShadowPass<'_>,
    scene: &ScenePass<'_>,
    exposure: ViewExposure,
    overlay: Option<&OverlayPass>,
    ui: Option<&UiDrawPass<'_>>,
    temporal: Option<TemporalResolveRecord<'_>>,
    skin: Option<skin::SkinRecord<'_>>,
    screen: ScreenContext<'_>,
    output_marks: Option<GpuMarks>,
    present_copies: &[SwapchainCopy],
) -> Result<Option<TemporalResolveOutcome>, RenderError> {
    begin_command_buffer(device, command_buffer, output_marks.map(|marks| marks.pool))?;
    if let Some(record) = skin {
        skin::record_skin(device, command_buffer, record);
    }
    record_scene(
        device,
        command_buffer,
        &mut target.scene,
        shadow_image,
        shadow_layout,
        local_shadow_image,
        local_shadow_layout,
        target.render_extent,
        shadow,
        scene,
    );
    let exposure_buffer = exposure.buffer;
    let velocity_view = target.scene.velocity.view;
    let depth_view = target.scene.depth.view;
    let filters = match (target.filter_scratch.as_mut(), scene.filter_pipelines) {
        (Some(scratch), Some(pipelines))
            if scene.post.filters.motion.is_some() || scene.post.filters.dof.is_some() =>
        {
            Some(FilterRecord {
                scratch,
                pipelines,
                sampler: post_sampler,
                velocity: velocity_view,
                depth: depth_view,
                params: scene.post.filters,
            })
        }
        _ => None,
    };
    // Экспозиция замеряется по HDR цели сцены до temporal resolve: FSR/DLSS и composite
    // получают EV этого кадра.
    if let Some(record) = exposure_record(
        &target.exposure,
        scene.exposure_pipelines,
        exposure,
        &view.post,
    ) {
        record_exposure(
            device,
            command_buffer,
            &target.scene.color,
            &mut target.scene.layout,
            target.render_extent,
            record,
            scene.timestamps,
        );
    }
    let passes = PostPasses {
        filters,
        sampler: post_sampler,
        bloom: bloom_record(
            target.bloom.as_mut(),
            scene.bloom_pipelines,
            scene.post.plan.bloom_mips,
            post_sampler,
            &view.post,
            exposure,
        ),
        exposure_buffer,
        lut: scene.color_lut,
        screen_before: chain_record(
            target.screen_before.as_mut(),
            &scene.post.screen_before,
            screen,
            post_sampler,
        ),
        screen_after: chain_record(
            target.screen_after.as_mut(),
            &scene.post.screen_after,
            screen,
            post_sampler,
        ),
    };
    let (temporal_outcome, tonemap_view) = if let Some(resolve) = temporal {
        let outcome = record_temporal_resolve_and_tonemap(
            device,
            command_buffer,
            &mut target.scene,
            &mut target.ldr,
            &mut target.ldr_layout,
            tonemap_set,
            post_sampler,
            scene.frame_set,
            target.output_extent,
            scene.post.composite,
            passes,
            scene.timestamps,
            tonemap_pipeline,
            post_pipeline_layout,
            target.taa_set,
            resolve,
        )?;
        let view = outcome.output_view;
        (Some(outcome), view)
    } else {
        write_image_set(device, tonemap_set, target.scene.color.view, post_sampler);
        let view = record_tonemap_pass(
            device,
            command_buffer,
            &target.scene.color,
            target.scene.layout,
            &target.ldr,
            &mut target.ldr_layout,
            target.render_extent,
            tonemap_set,
            tonemap_pipeline,
            post_pipeline_layout,
            scene.post.composite,
            passes,
            scene.timestamps,
        );
        target.scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        (None, view)
    };
    write_image_set(device, output_set, tonemap_view, post_sampler);
    let output_source_extent = temporal_outcome
        .as_ref()
        .map_or(target.render_extent, |outcome| outcome.output_extent);
    if let Some(marks) = output_marks {
        marks.write(device, command_buffer, 0);
    }
    record_output_pass(
        device,
        command_buffer,
        output_image,
        output_view,
        output_source_extent,
        output_extent,
        vk::ImageLayout::UNDEFINED,
        output_set,
        output_pipeline,
        post_pipeline_layout,
        view.anti_aliasing,
        output_is_srgb,
        scene.post.grain,
        overlay,
        ui,
        false,
        present_copies,
    );
    if let Some(marks) = output_marks {
        marks.write(device, command_buffer, 1);
    }
    // SAFETY: command recording is complete before the command buffer is submitted.
    unsafe { device.end_command_buffer(command_buffer) }?;
    Ok(temporal_outcome)
}

pub(crate) fn begin_command_buffer(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    timestamps: Option<vk::QueryPool>,
) -> Result<(), RenderError> {
    // SAFETY: this frame's command buffer is no longer in flight because its fence was waited.
    unsafe { device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty()) }?;
    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the command buffer was reset and belongs to the renderer's graphics command pool.
    unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
    if let Some(pool) = timestamps {
        // Все запросы пула сбрасываются до первого вида; reset выполняется вне rendering.
        // SAFETY: the fence was waited, so no earlier submission still uses these queries.
        unsafe { device.cmd_reset_query_pool(command_buffer, pool, 0, TIMESTAMP_QUERY_COUNT) };
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum DrawPass {
    /// Непрозрачные меши без маски: только глубина, перед основным проходом.
    Prepass,
    OpaqueAndMask,
    Blend,
    Overdraw,
    Wireframe,
}

pub(crate) struct ScenePass<'a> {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) mask_pipeline: vk::Pipeline,
    pub(crate) blend_pipeline: vk::Pipeline,
    pub(crate) overdraw_pipeline: vk::Pipeline,
    pub(crate) wireframe_pipeline: Option<vk::Pipeline>,
    pub(crate) cluster_pipeline: vk::Pipeline,
    pub(crate) cluster_pipeline_layout: vk::PipelineLayout,
    pub(crate) skybox_pipeline: vk::Pipeline,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) frame_set: vk::DescriptorSet,
    pub(crate) environment_set: vk::DescriptorSet,
    pub(crate) skybox: bool,
    pub(crate) meshes: &'a [Option<MeshBuffers>],
    pub(crate) materials: &'a [Option<GpuMaterial>],
    pub(crate) surface_shaders: &'a [Option<GpuSurfaceShader>],
    pub(crate) draws: &'a [DrawCommand],
    /// Буферы вершин skinned draw'ов по индексу draw; `None` у статических.
    pub(crate) skin_draws: &'a [Option<skin::SkinDrawBuffers>],
    pub(crate) instances: InstanceRange,
    pub(crate) view_projection: [[f32; 4]; 4],
    #[cfg(windows)]
    pub(crate) fsr_camera: [f32; 3],
    pub(crate) culling_padding: [f32; 2],
    pub(crate) temporal_plan: TemporalPlan,
    pub(crate) camera_position: [f32; 3],
    pub(crate) clear_color: [f32; 4],
    pub(crate) timestamps: Option<GpuMarks>,
    pub(crate) debug_view: DebugView,
    pub(crate) view: ViewSettings,
    pub(crate) post: PostFrame,
    pub(crate) bloom_pipelines: Option<BloomPipelines>,
    pub(crate) exposure_pipelines: Option<ExposurePipelines>,
    /// Время кадра вида для адаптации экспозиции.
    pub(crate) exposure_dt_ms: f32,
    /// 3D LUT composite вида: LUT цветокоррекции или встроенная identity LUT.
    pub(crate) color_lut: vk::ImageView,
    pub(crate) reset_history: bool,
    pub(crate) filter_pipelines: Option<FilterPipelines>,
    pub(crate) ao_pipelines: Option<AoPipelines>,
    pub(crate) opaque_equal_pipeline: Option<vk::Pipeline>,
    pub(crate) prepass_pipeline: Option<vk::Pipeline>,
    /// Prepass записан: основной непрозрачный проход использует LESS_OR_EQUAL и загружает глубину.
    pub(crate) depth_prepass: bool,
    /// Пайплайны occlusion culling, если тест включён для вида; требует `depth_prepass`
    /// и подготовленный `OffscreenTarget::occlusion`.
    pub(crate) occlusion: Option<OcclusionPipelines>,
    /// (proj[0][0], proj[1][1]) камеры для восстановления позиции по глубине в AO.
    pub(crate) projection_scale: [f32; 2],
    pub(crate) particles: Option<ParticlePass<'a>>,
    /// Счётчики draw'ов кадра, общие для всех видов и проходов.
    pub(crate) stats: &'a Cell<DrawStats>,
}

/// Экземпляры draw'ов вида в буфере кадра: draw `i` читает экземпляр `first + i`.
#[derive(Clone, Copy)]
pub(crate) struct InstanceRange {
    pub(crate) buffer: vk::Buffer,
    pub(crate) first: u32,
}

/// Depth-only shadow pass. It shares the frame set and model transforms with the scene pass.
pub(crate) struct ShadowPass<'a> {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) local_pipeline: vk::Pipeline,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) frame_set: vk::DescriptorSet,
    pub(crate) meshes: &'a [Option<MeshBuffers>],
    pub(crate) materials: &'a [Option<GpuMaterial>],
    pub(crate) draws: &'a [DrawCommand],
    pub(crate) skin_draws: &'a [Option<skin::SkinDrawBuffers>],
    pub(crate) instances: InstanceRange,
    pub(crate) shadow_cascades: &'a [[[f32; 4]; 4]],
    pub(crate) local_shadow_matrices: &'a [[[f32; 4]; 4]],
    pub(crate) stats: &'a Cell<DrawStats>,
}

/// Вызовы UI внутри rendering pass: пайплайн, общие буферы слота и вызовы.
///
/// `draws` хранит вызов, его набор дескрипторов и признак линейного изображения.
pub(crate) struct UiDrawPass<'a> {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) vertex_buffer: vk::Buffer,
    pub(crate) index_buffer: vk::Buffer,
    pub(crate) framebuffer_is_srgb: bool,
    pub(crate) draws: &'a [(UiDraw, vk::DescriptorSet, bool)],
}

impl<'a> UiDrawPass<'a> {
    /// Собирает проход; `None`, если вызовов нет или буферы слота не выделены.
    pub(crate) fn new(
        pipeline: vk::Pipeline,
        pipeline_layout: vk::PipelineLayout,
        vertices: &GrowingBuffer,
        indices: &GrowingBuffer,
        framebuffer_is_srgb: bool,
        draws: &'a [(UiDraw, vk::DescriptorSet, bool)],
    ) -> Option<Self> {
        if draws.is_empty() {
            return None;
        }
        Some(Self {
            pipeline,
            pipeline_layout,
            vertex_buffer: vertices.buffer.as_ref()?.buffer,
            index_buffer: indices.buffer.as_ref()?.buffer,
            framebuffer_is_srgb,
            draws,
        })
    }
}

/// Рисование UI в swapchain image после очистки; `draws` может быть пустым.
pub(crate) struct UiPass<'a> {
    pub(crate) draws: Option<UiDrawPass<'a>>,
    pub(crate) view: vk::ImageView,
    pub(crate) clear: [f32; 4],
}

/// Рисование вершин оверлея поверх swapchain image после blit.
pub(crate) struct OverlayPass {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) vertex_buffer: vk::Buffer,
    pub(crate) vertex_count: u32,
}

pub(crate) fn pipeline_barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    barrier: vk::ImageMemoryBarrier2<'_>,
) {
    let barriers = [barrier];
    let dependency = vk::DependencyInfo::default().image_memory_barriers(&barriers);
    // SAFETY: barriers reference images owned by this renderer and commands execute on the same queue family.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn image_layout_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    source_stage: vk::PipelineStageFlags2,
    source_access: vk::AccessFlags2,
    destination_stage: vk::PipelineStageFlags2,
    destination_access: vk::AccessFlags2,
    range: vk::ImageSubresourceRange,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(destination_stage)
        .dst_access_mask(destination_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range)
}

/// Возвращает источник доступа offscreen image перед повторной записью.
pub(crate) fn offscreen_previous_use(
    layout: vk::ImageLayout,
) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    match layout {
        vk::ImageLayout::UNDEFINED => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        ),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_READ,
        ),
    }
}

pub(crate) fn offscreen_sampled_stages() -> vk::PipelineStageFlags2 {
    vk::PipelineStageFlags2::FRAGMENT_SHADER | vk::PipelineStageFlags2::COMPUTE_SHADER
}

pub(crate) fn color_subresource_range() -> vk::ImageSubresourceRange {
    color_mip_subresource_range(0, 1)
}

pub(crate) fn color_mip_subresource_range(
    base_mip_level: u32,
    level_count: u32,
) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(base_mip_level)
        .level_count(level_count)
        .base_array_layer(0)
        .layer_count(1)
}

pub(crate) fn depth_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

pub(crate) fn shadow_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(SHADOW_CASCADE_COUNT as u32)
}

pub(crate) fn local_shadow_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(MAX_LOCAL_SHADOW_LAYERS as u32)
}

pub(crate) fn shadow_layout_source(
    layout: vk::ImageLayout,
) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    match layout {
        vk::ImageLayout::UNDEFINED => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        ),
    }
}

pub(crate) fn color_subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .mip_level(0)
        .base_array_layer(0)
        .layer_count(1)
}
