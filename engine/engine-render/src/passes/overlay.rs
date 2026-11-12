//! Проходы отладочного overlay и UI.

use crate::*;

/// Рисует оверлей внутри output pass, поверх уже отфильтрованного изображения.
pub(crate) fn record_overlay(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_extent: vk::Extent2D,
    overlay: &OverlayPass,
) {
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: output_extent.width as f32,
        height: output_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let push_constants =
        float_bytes::<OVERLAY_PUSH_CONSTANTS_SIZE>(&[viewport[0].width, viewport[0].height]);
    // SAFETY: output pass rendering is active and the vertex buffer was written after the slot fence.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            overlay.pipeline,
        );
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_vertex_buffers(command_buffer, 0, &[overlay.vertex_buffer], &[0]);
        device.cmd_push_constants(
            command_buffer,
            overlay.pipeline_layout,
            vk::ShaderStageFlags::VERTEX,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, overlay.vertex_count, 1, 0, 0);
    }
}

/// Очищает swapchain image, рисует UI и переводит изображение в present layout.
pub(crate) fn record_ui(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_image: vk::Image,
    output_extent: vk::Extent2D,
    ui: &UiPass<'_>,
    present_copies: &[SwapchainCopy],
) {
    // Стадия совпадает с ожиданием acquire semaphore в submit этого command buffer.
    let to_attachment = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        )
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, to_attachment);

    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(ui.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue { float32: ui.clear },
        })];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: output_extent,
        })
        .layer_count(1)
        .color_attachments(&color_attachment);
    // SAFETY: the view belongs to the output image and the pipeline matches the swapchain format.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    if let Some(pass) = &ui.draws {
        record_ui_draws(device, command_buffer, pass, output_extent);
    }
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };

    if !present_copies.is_empty() {
        record_present_copies(
            device,
            command_buffer,
            output_image,
            output_extent,
            present_copies,
        );
        return;
    }
    let present_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
        .dst_access_mask(vk::AccessFlags2::NONE)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, present_barrier);
}

/// Рисует вызовы UI внутри уже начатого rendering pass; viewport покрывает весь `extent`.
pub(crate) fn record_ui_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pass: &UiDrawPass<'_>,
    extent: vk::Extent2D,
) {
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    // SAFETY: inside dynamic rendering; the buffers were written after the slot fence.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            pass.pipeline,
        );
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_bind_vertex_buffers(command_buffer, 0, &[pass.vertex_buffer], &[0]);
        device.cmd_bind_index_buffer(command_buffer, pass.index_buffer, 0, vk::IndexType::UINT32);
    }
    for (draw, set, image_is_linear) in pass.draws {
        let push_constants = float_bytes::<UI_PUSH_CONSTANTS_SIZE>(&[
            viewport[0].width,
            viewport[0].height,
            f32::from(u8::from(pass.framebuffer_is_srgb)),
            f32::from(u8::from(*image_is_linear)),
        ]);
        // SAFETY: the set, scissor and index range were validated before recording; the
        // descriptor's image stays alive until the fences of this submission are waited.
        unsafe {
            device.cmd_set_scissor(command_buffer, 0, &[draw.scissor]);
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pass.pipeline_layout,
                0,
                &[*set],
                &[],
            );
            device.cmd_push_constants(
                command_buffer,
                pass.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                &push_constants,
            );
            device.cmd_draw_indexed(
                command_buffer,
                draw.index_count,
                1,
                draw.first_index,
                draw.vertex_offset,
                0,
            );
        }
    }
}
