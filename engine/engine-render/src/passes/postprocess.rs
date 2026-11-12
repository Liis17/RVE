//! Проходы tonemap, TAA и вывода в swapchain.

use crate::*;

/// Проходы постобработки вида, которые записываются перед composite.
pub(crate) struct PostPasses<'a> {
    pub(crate) filters: Option<FilterRecord<'a>>,
    pub(crate) bloom: Option<BloomRecord<'a>>,
    pub(crate) exposure_buffer: vk::Buffer,
    /// 3D LUT composite: LUT вида или встроенная identity LUT.
    pub(crate) lut: vk::ImageView,
    pub(crate) sampler: vk::Sampler,
    /// Экранные шейдеры до фильтров; `None`, если вид их не использует.
    pub(crate) screen_before: Option<ScreenChainRecord<'a>>,
    /// Экранные шейдеры после composite.
    pub(crate) screen_after: Option<ScreenChainRecord<'a>>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_tonemap_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &DeviceImage,
    source_layout: vk::ImageLayout,
    output: &DeviceImage,
    output_layout: &mut vk::ImageLayout,
    extent: vk::Extent2D,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    composite: post::CompositeParams,
    passes: PostPasses<'_>,
    timestamps: Option<GpuMarks>,
) -> vk::ImageView {
    let PostPasses {
        mut filters,
        bloom,
        exposure_buffer,
        lut,
        sampler,
        mut screen_before,
        mut screen_after,
    } = passes;
    let mut source_layout = source_layout;
    // before_post идёт до motion blur и DoF: его выход становится входом всех следующих проходов.
    let before_index = match screen_before.as_mut() {
        Some(record) => record_screen_chain(
            device,
            command_buffer,
            source,
            &mut source_layout,
            extent,
            record,
            timestamps,
            GpuPass::ScreenShaderBefore,
        ),
        None => None,
    };
    let (source, mut source_layout) = match (before_index, screen_before.as_ref()) {
        (Some(index), Some(record)) => (
            record.scratch.image(index),
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        ),
        _ => (source, source_layout),
    };
    // Motion blur и DoF идут до экспозиции и bloom: им нужен цвет после resolve.
    let filtered = match filters.as_mut() {
        Some(record) => record_filters(
            device,
            command_buffer,
            source,
            &mut source_layout,
            record,
            timestamps,
        ),
        None => None,
    };
    let (final_image, mut final_layout) = match (filtered, filters.as_ref()) {
        (Some(index), Some(record)) => (
            record.scratch.image(index),
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        ),
        _ => (source, source_layout),
    };
    write_image_set(device, set, final_image.view, sampler);
    let bloom_view = bloom.map(|record| {
        record_bloom(
            device,
            command_buffer,
            final_image,
            &mut final_layout,
            record,
            timestamps,
        )
    });
    write_composite_bloom(device, set, bloom_view.unwrap_or(final_image.view));
    write_composite_exposure(device, set, exposure_buffer);
    write_composite_lut(device, set, lut);
    if final_layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        let (source_stage, source_access) = match final_layout {
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            ),
            _ => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        };
        let source_barrier = image_layout_barrier(
            final_image.image,
            final_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            source_stage,
            source_access,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        );
        pipeline_barrier(device, command_buffer, source_barrier);
    }
    let (output_stage, output_access) = offscreen_previous_use(*output_layout);
    let output_barrier = image_layout_barrier(
        output.image,
        *output_layout,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        output_stage,
        output_access,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        color_subresource_range(),
    );
    pipeline_barrier(device, command_buffer, output_barrier);

    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(output.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let push_constants = composite.to_bytes();
    // SAFETY: source and destination views match the descriptor and attachment layouts.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
        device.cmd_end_rendering(command_buffer);
    }
    mark_pass(device, command_buffer, timestamps, GpuPass::Tonemap);
    let sampled = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, sampled);
    *output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    // after_post читает composite из `output` и пишет в scratch; output-проход получает итоговое изображение.
    let mut output_read_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    let after_index = match screen_after.as_mut() {
        Some(record) => record_screen_chain(
            device,
            command_buffer,
            output,
            &mut output_read_layout,
            extent,
            record,
            timestamps,
            GpuPass::ScreenShaderAfter,
        ),
        None => None,
    };
    match (after_index, screen_after.as_ref()) {
        (Some(index), Some(record)) => record.scratch.image(index).view,
        _ => output.view,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_taa_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    history: &TemporalHistory,
    taa_set: vk::DescriptorSet,
    frame_set: vk::DescriptorSet,
    output_extent: vk::Extent2D,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    plan: TemporalPlan,
) {
    let (scene_stage, scene_access) = offscreen_previous_use(scene.layout);
    let scene_to_sample = image_layout_barrier(
        scene.color.image,
        scene.layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        scene_stage,
        scene_access,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
        color_subresource_range(),
    );
    pipeline_barrier(device, command_buffer, scene_to_sample);

    let read_layout = history.layouts[plan.read_history];
    if read_layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                history.images[plan.read_history].image,
                read_layout,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::NONE,
                vk::AccessFlags2::NONE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                color_subresource_range(),
            ),
        );
    }
    let write_image = &history.images[plan.write_history];
    let (write_stage, write_access) = match history.layouts[plan.write_history] {
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            write_image.image,
            history.layouts[plan.write_history],
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            write_stage,
            write_access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );

    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(write_image.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: output_extent.width as f32,
        height: output_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let push_constants = float_bytes::<TAA_PUSH_CONSTANTS_SIZE>(&[
        plan.jitter_render_pixels[0],
        plan.jitter_render_pixels[1],
        f32::from(u8::from(plan.history_valid)),
        f32::from(u8::from(plan.reset_history)),
    ]);
    // SAFETY: both sampled sets and the output attachment match the TAA pipeline layouts.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[taa_set, frame_set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
        device.cmd_end_rendering(command_buffer);
    }
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            write_image.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_output_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_image: vk::Image,
    output_view: vk::ImageView,
    source_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    previous_output_layout: vk::ImageLayout,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    anti_aliasing: AntiAliasing,
    output_is_srgb: bool,
    grain: [f32; 4],
    overlay: Option<&OverlayPass>,
    ui: Option<&UiDrawPass<'_>>,
    sample_output: bool,
    present_copies: &[SwapchainCopy],
) {
    let to_attachment = if sample_output {
        let (source_stage, source_access) = offscreen_previous_use(previous_output_layout);
        image_layout_barrier(
            output_image,
            previous_output_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            source_stage,
            source_access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        )
    } else {
        output_attachment_barrier(output_image)
    };
    pipeline_barrier(device, command_buffer, to_attachment);

    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(output_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: output_extent.width as f32,
        height: output_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let [texel_width, texel_height] = inverse_extent(source_extent);
    let push_constants = float_bytes::<OUTPUT_PUSH_CONSTANTS_SIZE>(&[
        texel_width,
        texel_height,
        anti_aliasing.code() as f32,
        f32::from(u8::from(output_is_srgb)),
        grain[0],
        grain[1],
        grain[2],
        grain[3],
    ]);
    // SAFETY: the output view uses the pipeline's color format and descriptor set samples LDR.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
    }
    if let Some(overlay) = overlay {
        record_overlay(device, command_buffer, output_extent, overlay);
    }
    if let Some(ui) = ui {
        record_ui_draws(device, command_buffer, ui, output_extent);
    }
    // SAFETY: this matches the output pass's dynamic rendering begin.
    unsafe { device.cmd_end_rendering(command_buffer) };

    if !sample_output && !present_copies.is_empty() {
        record_present_copies(
            device,
            command_buffer,
            output_image,
            output_extent,
            present_copies,
        );
        return;
    }
    let sampled_output = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(if sample_output {
            vk::PipelineStageFlags2::FRAGMENT_SHADER
        } else {
            vk::PipelineStageFlags2::NONE
        })
        .dst_access_mask(if sample_output {
            vk::AccessFlags2::SHADER_SAMPLED_READ
        } else {
            vk::AccessFlags2::NONE
        })
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(if sample_output {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::PRESENT_SRC_KHR
        })
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, sampled_output);
}

pub(crate) fn output_attachment_barrier(
    output_image: vk::Image,
) -> vk::ImageMemoryBarrier2<'static> {
    // Keep the layout transition in the COLOR_ATTACHMENT_OUTPUT acquire-wait scope.
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range())
}
