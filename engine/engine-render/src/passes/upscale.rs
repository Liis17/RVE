//! Временное разрешение и апскейлеры FSR и DLSS.

use crate::*;

pub(crate) struct TemporalResolveRecord<'a> {
    pub(crate) history: &'a TemporalHistory,
    #[cfg(windows)]
    pub(crate) backend: TemporalBackend,
    pub(crate) plan: TemporalPlan,
    pub(crate) taa_pipeline: vk::Pipeline,
    pub(crate) taa_pipeline_layout: vk::PipelineLayout,
    #[cfg(windows)]
    pub(crate) fsr: Option<&'a mut FsrView>,
    #[cfg(windows)]
    pub(crate) exposure: UpscalerExposure,
    #[cfg(windows)]
    pub(crate) frame_time_delta_ms: f32,
    #[cfg(windows)]
    pub(crate) camera: [f32; 3],
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss_api: Option<&'a dlss::NgxApi>,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss: Option<&'a mut DlssView>,
}

pub(crate) struct TemporalResolveOutcome {
    pub(crate) used_taa: bool,
    #[cfg(windows)]
    pub(crate) used_fsr: bool,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) used_dlss: bool,
    pub(crate) fsr_error: Option<String>,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) dlss_error: Option<String>,
    pub(crate) output_extent: vk::Extent2D,
    /// Изображение, которое читает output-проход: composite или результат экранных шейдеров после него.
    pub(crate) output_view: vk::ImageView,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_temporal_resolve_and_tonemap(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    ldr: &mut DeviceImage,
    ldr_layout: &mut vk::ImageLayout,
    tonemap_set: vk::DescriptorSet,
    post_sampler: vk::Sampler,
    frame_set: vk::DescriptorSet,
    output_extent: vk::Extent2D,
    composite: post::CompositeParams,
    passes: PostPasses<'_>,
    timestamps: Option<GpuMarks>,
    tonemap_pipeline: vk::Pipeline,
    post_pipeline_layout: vk::PipelineLayout,
    taa_set: vk::DescriptorSet,
    resolve: TemporalResolveRecord<'_>,
) -> Result<TemporalResolveOutcome, RenderError> {
    #[cfg(windows)]
    let mut fsr_error = None;
    #[cfg(not(windows))]
    let fsr_error = None;
    #[cfg(all(windows, feature = "dlss"))]
    let mut dlss_error = None;
    #[cfg(windows)]
    let mut used_taa = true;
    #[cfg(not(windows))]
    let used_taa = true;
    #[cfg(windows)]
    let mut used_fsr = false;
    #[cfg(all(windows, feature = "dlss"))]
    let mut used_dlss = false;
    #[cfg(windows)]
    let mut reset_taa_history = false;
    #[cfg(not(windows))]
    let reset_taa_history = false;
    #[cfg(windows)]
    let mut resolved_source = None;
    #[cfg(windows)]
    if resolve.backend == TemporalBackend::Fsr
        && let Some(fsr) = resolve.fsr
    {
        match record_fsr_pass(
            device,
            command_buffer,
            scene,
            fsr,
            resolve.plan,
            resolve.exposure,
            resolve.frame_time_delta_ms,
            resolve.camera,
        ) {
            Ok(()) => {
                used_taa = false;
                used_fsr = true;
                resolved_source = Some(&fsr.output);
            }
            Err(error) => {
                fsr_error = Some(error);
                reset_taa_history = true;
            }
        }
    }
    #[cfg(all(windows, feature = "dlss"))]
    if resolve.backend == TemporalBackend::Dlss
        && let (Some(api), Some(dlss)) = (resolve.dlss_api, resolve.dlss)
    {
        match record_dlss_pass(
            device,
            command_buffer,
            scene,
            dlss,
            api,
            resolve.plan,
            resolve.exposure,
            resolve.frame_time_delta_ms,
        ) {
            Ok(()) => {
                used_taa = false;
                used_dlss = true;
                resolved_source = Some(&dlss.output);
            }
            Err(error) => {
                dlss_error = Some(error);
                reset_taa_history = true;
            }
        }
    }

    let source = if used_taa {
        let mut taa_plan = resolve.plan;
        if reset_taa_history {
            taa_plan.history_valid = false;
            taa_plan.reset_history = true;
        }
        record_taa_pass(
            device,
            command_buffer,
            scene,
            resolve.history,
            taa_set,
            frame_set,
            output_extent,
            resolve.taa_pipeline,
            resolve.taa_pipeline_layout,
            taa_plan,
        );
        &resolve.history.images[resolve.plan.write_history]
    } else {
        #[cfg(windows)]
        {
            resolved_source.ok_or(RenderError::InvalidState(
                "temporal upscaler output отсутствует",
            ))?
        }
        #[cfg(not(windows))]
        {
            return Err(RenderError::InvalidState(
                "FSR backend выбран на неподдерживаемой платформе",
            ));
        }
    };
    mark_pass(device, command_buffer, timestamps, GpuPass::TemporalResolve);
    write_image_set(device, tonemap_set, source.view, post_sampler);
    let output_view = record_tonemap_pass(
        device,
        command_buffer,
        source,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        ldr,
        ldr_layout,
        output_extent,
        tonemap_set,
        tonemap_pipeline,
        post_pipeline_layout,
        composite,
        passes,
        timestamps,
    );
    Ok(TemporalResolveOutcome {
        output_view,
        used_taa,
        #[cfg(windows)]
        used_fsr,
        #[cfg(all(windows, feature = "dlss"))]
        used_dlss,
        fsr_error,
        #[cfg(all(windows, feature = "dlss"))]
        dlss_error,
        output_extent,
    })
}

/// Экспозиция, которую получает FSR/DLSS.
#[cfg(windows)]
#[derive(Clone, Copy)]
pub(crate) enum UpscalerExposure {
    /// Ручная экспозиция: множитель `exp2(ev)` записывается очисткой изображения.
    Manual(f32),
    /// Auto exposure: множитель `exp2(ev + adapted_ev)` текущего кадра копируется из буфера вида.
    /// Адаптация этого кадра уже записана: экспозиция замеряется до temporal resolve.
    Adapted(vk::Buffer),
}

#[cfg(windows)]
impl UpscalerExposure {
    /// Выбирает источник: адаптированный буфер, только если кадр вида записывает адаптацию.
    pub(crate) fn new(ev: f32, adapted: Option<vk::Buffer>) -> Self {
        adapted.map_or(Self::Manual(ev), Self::Adapted)
    }
}

/// Записывает 1×1 изображение экспозиции апскейлера и переводит его в чтение стадией `consumer_stage`.
#[cfg(windows)]
pub(crate) fn write_upscaler_exposure(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    layout: &mut vk::ImageLayout,
    exposure: UpscalerExposure,
    consumer_stage: vk::PipelineStageFlags2,
) {
    let (source_stage, source_access) = if *layout == vk::ImageLayout::UNDEFINED {
        (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
    } else {
        (consumer_stage, vk::AccessFlags2::SHADER_SAMPLED_READ)
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            image,
            *layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            source_stage,
            source_access,
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            color_subresource_range(),
        ),
    );
    match exposure {
        UpscalerExposure::Manual(ev) => {
            let value = [fsr::exposure_value(ev), 0.0, 0.0, 0.0];
            // SAFETY: the 1x1 exposure image is in TRANSFER_DST_OPTIMAL and the range covers its only subresource.
            unsafe {
                device.cmd_clear_color_image(
                    command_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &vk::ClearColorValue { float32: value },
                    &[color_subresource_range()],
                )
            };
        }
        UpscalerExposure::Adapted(buffer) => {
            // Усреднение экспозиции этого кадра записало множитель в compute-стадии.
            exposure_buffer_barrier(
                device,
                command_buffer,
                buffer,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_STORAGE_WRITE,
                vk::PipelineStageFlags2::TRANSFER,
                vk::AccessFlags2::TRANSFER_READ,
            );
            let region = [vk::BufferImageCopy::default()
                .buffer_offset(EXPOSURE_SCALE_OFFSET)
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .mip_level(0)
                        .base_array_layer(0)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: 1,
                    height: 1,
                    depth: 1,
                })];
            // SAFETY: the buffer holds the R32 scale at a texel-aligned offset and the 1x1 image
            // is in TRANSFER_DST_OPTIMAL.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                )
            };
        }
    }
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            consumer_stage,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    *layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_fsr_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    fsr: &mut FsrView,
    plan: TemporalPlan,
    exposure: UpscalerExposure,
    frame_time_delta_ms: f32,
    camera: [f32; 3],
) -> Result<(), String> {
    if scene.layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        let (source_stage, source_access) = offscreen_previous_use(scene.layout);
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                scene.color.image,
                scene.layout,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                source_stage,
                source_access,
                vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                color_subresource_range(),
            ),
        );
        scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }
    write_upscaler_exposure(
        device,
        command_buffer,
        fsr.exposure.image,
        &mut fsr.exposure_layout,
        exposure,
        vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
    );

    if fsr.output_layout == vk::ImageLayout::UNDEFINED {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                fsr.output.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::NONE,
                vk::AccessFlags2::NONE,
                vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                color_subresource_range(),
            ),
        );
        fsr.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    let render_size = [fsr.render_extent.width, fsr.render_extent.height];
    let output_size = [fsr.output_extent.width, fsr.output_extent.height];
    let resource_state = fsr::RESOURCE_STATE_PIXEL_COMPUTE_READ;
    let dispatch = fsr::dispatch_description(
        command_buffer.as_raw() as usize as *mut std::ffi::c_void,
        fsr::resource(
            scene.color.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R16G16B16A16_FLOAT,
            render_size[0],
            render_size[1],
            0,
            resource_state,
        ),
        fsr::resource(
            scene.depth.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R32_FLOAT,
            render_size[0],
            render_size[1],
            fsr::RESOURCE_USAGE_DEPTH_TARGET,
            resource_state,
        ),
        fsr::resource(
            scene.velocity.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R16G16_FLOAT,
            render_size[0],
            render_size[1],
            0,
            resource_state,
        ),
        fsr::resource(
            fsr.exposure.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R32_FLOAT,
            1,
            1,
            0,
            resource_state,
        ),
        fsr::resource(
            fsr.output.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R16G16B16A16_FLOAT,
            output_size[0],
            output_size[1],
            fsr::RESOURCE_USAGE_UAV,
            resource_state,
        ),
        plan.jitter_render_pixels,
        render_size,
        output_size,
        frame_time_delta_ms,
        plan.reset_history,
        camera,
    );
    fsr.context.dispatch(&dispatch)?;
    fsr.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    Ok(())
}

#[cfg(all(windows, feature = "dlss"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_dlss_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    dlss_view: &mut DlssView,
    api: &dlss::NgxApi,
    plan: TemporalPlan,
    exposure: UpscalerExposure,
    frame_time_delta_ms: f32,
) -> Result<(), String> {
    for (image, layout, range) in [
        (&scene.color, &mut scene.layout, color_subresource_range()),
        (
            &scene.velocity,
            &mut scene.velocity_layout,
            color_subresource_range(),
        ),
        (
            &scene.depth,
            &mut scene.depth_layout,
            depth_subresource_range(),
        ),
    ] {
        if *layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            let (source_stage, source_access) = offscreen_previous_use(*layout);
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    *layout,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    source_stage,
                    source_access,
                    vk::PipelineStageFlags2::ALL_COMMANDS,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    range,
                ),
            );
            *layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        }
    }

    write_upscaler_exposure(
        device,
        command_buffer,
        dlss_view.exposure.image,
        &mut dlss_view.exposure_layout,
        exposure,
        vk::PipelineStageFlags2::ALL_COMMANDS,
    );

    let (output_stage, output_access) = offscreen_previous_use(dlss_view.output_layout);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            dlss_view.output.image,
            dlss_view.output_layout,
            vk::ImageLayout::GENERAL,
            output_stage,
            output_access,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE,
            color_subresource_range(),
        ),
    );
    dlss_view.output_layout = vk::ImageLayout::GENERAL;

    let render_extent = dlss_view.render_extent;
    let output_extent = dlss_view.output_extent;
    let color = ngx_image_resource(&scene.color, render_extent, false);
    let depth = ngx_image_resource(&scene.depth, render_extent, false);
    let motion_vectors = ngx_image_resource(&scene.velocity, render_extent, false);
    let exposure = ngx_image_resource(
        &dlss_view.exposure,
        vk::Extent2D {
            width: 1,
            height: 1,
        },
        false,
    );
    let output = ngx_image_resource(&dlss_view.output, output_extent, true);
    api.evaluate(dlss::EvaluateParameters {
        device: device.handle(),
        command_buffer,
        feature: &mut dlss_view.feature,
        render_extent,
        output_extent,
        color: &color,
        depth: &depth,
        motion_vectors: &motion_vectors,
        exposure: &exposure,
        output: &output,
        jitter: dlss::ngx_jitter_offset(plan.jitter_render_pixels),
        motion_scale: [render_extent.width as f32, render_extent.height as f32],
        reset: plan.reset_history,
        frame_time_delta_ms,
    })?;

    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            dlss_view.output.image,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::MEMORY_WRITE,
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    dlss_view.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    Ok(())
}

#[cfg(all(windows, feature = "dlss"))]
pub(crate) fn ngx_image_resource(
    image: &DeviceImage,
    extent: vk::Extent2D,
    read_write: bool,
) -> dlss::NgxResourceVk {
    dlss::NgxResourceVk {
        resource: dlss::NgxResourceUnion {
            image_view: dlss::NgxImageViewInfo {
                image_view: image.view.as_raw(),
                image: image.image.as_raw(),
                subresource_range: image.subresource_range,
                format: image.format.as_raw(),
                width: extent.width,
                height: extent.height,
            },
        },
        resource_type: 0,
        read_write,
    }
}
