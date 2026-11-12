//! Проходы сцены: depth prepass, основной проход, wireframe, тени и skybox.

use crate::*;

/// Записывает cluster, shadow и scene pass; цвет цели остаётся в `COLOR_ATTACHMENT_OPTIMAL`.
#[allow(clippy::too_many_arguments)]
/// Записывает фазу depth prepass непрозрачных мешей. Первая фаза очищает глубину, вторая
/// (после теста occlusion culling) загружает её; основной проход глубину загружает.
pub(crate) fn record_depth_prepass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    depth_view: vk::ImageView,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
    load_op: vk::AttachmentLoadOp,
    indirect: Option<vk::Buffer>,
) {
    let depth_attachment = vk::RenderingAttachmentInfo::default()
        .image_view(depth_view)
        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .load_op(load_op)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: 1.0,
                stencil: 0,
            },
        });
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: render_extent,
        })
        .layer_count(1)
        .depth_attachment(&depth_attachment);
    // SAFETY: the depth view covers the render extent and matches the prepass pipeline.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    record_draws(
        device,
        command_buffer,
        render_extent,
        scene,
        DrawPass::Prepass,
        indirect,
    );
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_scene(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut OffscreenTarget,
    shadow_image: &DeviceImage,
    shadow_layout: &mut vk::ImageLayout,
    local_shadow_image: &DeviceImage,
    local_shadow_layout: &mut vk::ImageLayout,
    render_extent: vk::Extent2D,
    shadow: &ShadowPass<'_>,
    scene: &ScenePass<'_>,
) {
    if let Some(marks) = scene.timestamps {
        // Начало GPU-времени вида: граница стоит до clustering.
        marks.write(device, command_buffer, 0);
    }
    // Every invocation writes one fixed cluster record; zero-light frames also reset every count.
    // SAFETY: the frame set binds live light, cluster-output, bounds, and frame-uniform buffers.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            scene.cluster_pipeline,
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            scene.cluster_pipeline_layout,
            0,
            &[scene.frame_set],
            &[],
        );
        device.cmd_dispatch(
            command_buffer,
            (CLUSTER_COUNT as u32).div_ceil(CLUSTER_COMPUTE_LOCAL_SIZE),
            1,
            1,
        );
    }
    mark_pass(device, command_buffer, scene.timestamps, GpuPass::Clusters);
    let cluster_barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ);
    let memory_barriers = [cluster_barrier];
    let dependency = vk::DependencyInfo::default().memory_barriers(&memory_barriers);
    // SAFETY: the barrier publishes the just-written cluster SSBO to the fragment shader reads.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };

    let (previous_stage, previous_access) = offscreen_previous_use(target.layout);
    let scene_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(previous_stage)
        .src_access_mask(previous_access)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(target.layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(target.color.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, scene_barrier);
    let (velocity_stage, velocity_access) = offscreen_previous_use(target.velocity_layout);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.velocity.image,
            target.velocity_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            velocity_stage,
            velocity_access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );
    target.velocity_layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
    let (depth_stage, depth_access) = offscreen_previous_use(target.depth_layout);
    let depth_barrier = image_layout_barrier(
        target.depth.image,
        target.depth_layout,
        vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
        depth_stage,
        depth_access,
        vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
            | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
            | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        depth_subresource_range(),
    );
    pipeline_barrier(device, command_buffer, depth_barrier);
    target.depth_layout = vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL;
    if shadow.shadow_cascades.is_empty() {
        if *shadow_layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            let (source_stage, source_access) = shadow_layout_source(*shadow_layout);
            let shadow_to_sample = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(source_stage)
                .src_access_mask(source_access)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(*shadow_layout)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(shadow_image.image)
                .subresource_range(shadow_subresource_range());
            pipeline_barrier(device, command_buffer, shadow_to_sample);
            *shadow_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        }
    } else {
        let (source_stage, source_access) = shadow_layout_source(*shadow_layout);
        let shadow_to_depth = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(source_stage)
            .src_access_mask(source_access)
            .dst_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .dst_access_mask(
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            )
            .old_layout(*shadow_layout)
            .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(shadow_image.image)
            .subresource_range(shadow_subresource_range());
        pipeline_barrier(device, command_buffer, shadow_to_depth);
        *shadow_layout = vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL;

        for (cascade, matrix) in shadow.shadow_cascades.iter().enumerate() {
            let shadow_depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(shadow_image.layer_views[cascade])
                .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: 1.0,
                        stencil: 0,
                    },
                });
            let shadow_rendering_info = vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: vk::Extent2D {
                        width: SHADOW_MAP_RESOLUTION,
                        height: SHADOW_MAP_RESOLUTION,
                    },
                })
                .layer_count(1)
                .depth_attachment(&shadow_depth_attachment);
            // SAFETY: this depth-only view covers one fixed-size array layer.
            unsafe { device.cmd_begin_rendering(command_buffer, &shadow_rendering_info) };
            record_shadow_draws(
                device,
                command_buffer,
                SHADOW_MAP_RESOLUTION,
                cascade as u32,
                *matrix,
                shadow.pipeline,
                shadow,
            );
            // SAFETY: this matches the preceding depth-only cmd_begin_rendering call.
            unsafe { device.cmd_end_rendering(command_buffer) };
        }

        let shadow_to_sample = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(shadow_image.image)
            .subresource_range(shadow_subresource_range());
        pipeline_barrier(device, command_buffer, shadow_to_sample);
        *shadow_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    if !shadow.shadow_cascades.is_empty() {
        mark_pass(
            device,
            command_buffer,
            scene.timestamps,
            GpuPass::ShadowDirectional,
        );
    }
    record_local_shadow_layers(
        device,
        command_buffer,
        local_shadow_image,
        local_shadow_layout,
        shadow,
    );
    // Граница ставится всегда: очистка слоёв относится к проходу, даже если локальных источников нет.
    mark_pass(
        device,
        command_buffer,
        scene.timestamps,
        GpuPass::ShadowLocal,
    );

    let clear_color = if scene.debug_view == DebugView::Overdraw {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        scene.clear_color
    };
    let color_attachment = [
        vk::RenderingAttachmentInfo::default()
            .image_view(target.color.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: clear_color,
                },
            }),
        vk::RenderingAttachmentInfo::default()
            .image_view(target.velocity.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.0; 4] },
            }),
    ];
    let occlusion_pipelines = scene
        .occlusion
        .filter(|_| scene.depth_prepass && target.occlusion.is_some());
    let visible_args = occlusion_pipelines
        .and(target.occlusion.as_ref())
        .and_then(OcclusionTarget::visible_buffer);
    if scene.depth_prepass {
        if let (Some(pipelines), Some(occlusion)) = (occlusion_pipelines, target.occlusion.as_ref())
        {
            occlusion::record_early(
                device,
                command_buffer,
                pipelines,
                occlusion,
                scene.instances.first,
            );
        }
        record_depth_prepass(
            device,
            command_buffer,
            target.depth.view,
            render_extent,
            scene,
            vk::AttachmentLoadOp::CLEAR,
            visible_args,
        );
        if let (Some(pipelines), Some(occlusion)) = (occlusion_pipelines, target.occlusion.as_mut())
        {
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    target.depth.image,
                    vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                    vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    depth_subresource_range(),
                ),
            );
            occlusion::record_cull(
                device,
                command_buffer,
                pipelines,
                occlusion,
                scene.view_projection,
                scene.instances.first,
            );
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    target.depth.image,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                        | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                    vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                        | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                    depth_subresource_range(),
                ),
            );
            record_depth_prepass(
                device,
                command_buffer,
                target.depth.view,
                render_extent,
                scene,
                vk::AttachmentLoadOp::LOAD,
                occlusion.late_buffer(),
            );
        }
        mark_pass(
            device,
            command_buffer,
            scene.timestamps,
            GpuPass::DepthPrepass,
        );
    }
    if scene.depth_prepass
        && scene.post.plan.ambient_occlusion
        && let (Some(pipelines), Some(ao)) = (scene.ao_pipelines, target.ao.as_mut())
    {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.depth.image,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                depth_subresource_range(),
            ),
        );
        let gtao = post::ao_parameters(
            &scene.post.ao,
            scene.projection_scale,
            scene.post.filters.near_far,
            (ao.raw_extent.width, ao.raw_extent.height),
        );
        record_ambient_occlusion(
            device,
            command_buffer,
            render_extent,
            AoRecord {
                target: ao,
                pipelines,
                gtao,
                near_far: scene.post.filters.near_far,
            },
            scene.timestamps,
        );
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.depth.image,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                depth_subresource_range(),
            ),
        );
    }
    let depth_attachment = vk::RenderingAttachmentInfo::default()
        .image_view(target.depth.view)
        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .load_op(if scene.depth_prepass {
            vk::AttachmentLoadOp::LOAD
        } else {
            vk::AttachmentLoadOp::CLEAR
        })
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: 1.0,
                stencil: 0,
            },
        });
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: render_extent,
        })
        .layer_count(1)
        .color_attachments(&color_attachment)
        .depth_attachment(&depth_attachment);
    // SAFETY: the dynamic-rendering attachment view covers the configured render extent.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    record_draws(
        device,
        command_buffer,
        render_extent,
        scene,
        if scene.debug_view == DebugView::Overdraw {
            DrawPass::Overdraw
        } else {
            DrawPass::OpaqueAndMask
        },
        visible_args,
    );
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };
    mark_pass(device, command_buffer, scene.timestamps, GpuPass::Opaque);
    if scene.skybox && scene.debug_view != DebugView::Overdraw {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.color.image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                color_subresource_range(),
            ),
        );
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.velocity.image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                color_subresource_range(),
            ),
        );
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.depth.image,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ,
                depth_subresource_range(),
            ),
        );
        let color_attachment = [
            vk::RenderingAttachmentInfo::default()
                .image_view(target.color.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
            vk::RenderingAttachmentInfo::default()
                .image_view(target.velocity.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
        ];
        let depth_attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.depth.view)
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        let rendering_info = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: render_extent,
            })
            .layer_count(1)
            .color_attachments(&color_attachment)
            .depth_attachment(&depth_attachment);
        // SAFETY: skybox loads the preceding scene attachments and does not write depth.
        unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
        record_skybox(device, command_buffer, render_extent, scene);
        // SAFETY: this matches the preceding dynamic-rendering begin call.
        unsafe { device.cmd_end_rendering(command_buffer) };
        mark_pass(device, command_buffer, scene.timestamps, GpuPass::Skybox);
    }
    if scene.debug_view != DebugView::Overdraw && has_transparent_draws(scene) {
        let color_to_blend = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            )
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(target.color.image)
            .subresource_range(color_subresource_range());
        pipeline_barrier(device, command_buffer, color_to_blend);
        let velocity_to_blend = image_layout_barrier(
            target.velocity.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        );
        pipeline_barrier(device, command_buffer, velocity_to_blend);
        let depth_to_blend = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
            .dst_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .dst_access_mask(
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            )
            .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(target.depth.image)
            .subresource_range(depth_subresource_range());
        pipeline_barrier(device, command_buffer, depth_to_blend);
        let color_attachment = [
            vk::RenderingAttachmentInfo::default()
                .image_view(target.color.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
            vk::RenderingAttachmentInfo::default()
                .image_view(target.velocity.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
        ];
        let depth_attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.depth.view)
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        let rendering_info = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: render_extent,
            })
            .layer_count(1)
            .color_attachments(&color_attachment)
            .depth_attachment(&depth_attachment);
        // SAFETY: the color and depth attachments remain in their attachment layouts and were stored above.
        unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
        record_draws(
            device,
            command_buffer,
            render_extent,
            scene,
            DrawPass::Blend,
            visible_args,
        );
        // SAFETY: this matches the preceding transparent cmd_begin_rendering call.
        unsafe { device.cmd_end_rendering(command_buffer) };
        mark_pass(
            device,
            command_buffer,
            scene.timestamps,
            GpuPass::Transparent,
        );
    }
    if scene.debug_view == DebugView::Wireframe {
        record_wireframe(device, command_buffer, target, render_extent, scene);
        mark_pass(device, command_buffer, scene.timestamps, GpuPass::Wireframe);
    }
    if let Some(particles) = &scene.particles {
        record_particles(
            device,
            command_buffer,
            target,
            render_extent,
            particles,
            scene.timestamps,
        );
    }
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.velocity.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    target.velocity_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    // Частицы могли оставить depth в read-only: источник барьера зависит от текущего layout.
    let depth_source = particle_depth_source_masks(target.depth_layout);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.depth.image,
            target.depth_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            depth_source.0,
            depth_source.1,
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            depth_subresource_range(),
        ),
    );
    target.layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
    target.depth_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

pub(crate) fn record_wireframe(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &OffscreenTarget,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
) {
    for image in [&target.color, &target.velocity] {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                image.image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                color_subresource_range(),
            ),
        );
    }
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.depth.image,
            vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS,
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ,
            depth_subresource_range(),
        ),
    );
    let color_attachments = [
        vk::RenderingAttachmentInfo::default()
            .image_view(target.color.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE),
        vk::RenderingAttachmentInfo::default()
            .image_view(target.velocity.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE),
    ];
    let depth_attachment = vk::RenderingAttachmentInfo::default()
        .image_view(target.depth.view)
        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE);
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: render_extent,
        })
        .layer_count(1)
        .color_attachments(&color_attachments)
        .depth_attachment(&depth_attachment);
    // SAFETY: wireframe overlay attachments match the scene pass formats and remain in attachment layouts.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    record_draws(
        device,
        command_buffer,
        render_extent,
        scene,
        DrawPass::Wireframe,
        None,
    );
    // SAFETY: this matches the preceding dynamic-rendering begin call.
    unsafe { device.cmd_end_rendering(command_buffer) };
}

pub(crate) fn record_local_shadow_layers(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: &DeviceImage,
    layout: &mut vk::ImageLayout,
    shadow: &ShadowPass<'_>,
) {
    let (source_stage, source_access) = shadow_layout_source(*layout);
    let to_depth = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        )
        .dst_access_mask(
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        )
        .old_layout(*layout)
        .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image)
        .subresource_range(local_shadow_subresource_range());
    pipeline_barrier(device, command_buffer, to_depth);
    *layout = vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL;

    let extent = vk::Extent2D {
        width: LOCAL_SHADOW_MAP_RESOLUTION,
        height: LOCAL_SHADOW_MAP_RESOLUTION,
    };
    for layer in 0..MAX_LOCAL_SHADOW_LAYERS {
        let depth_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(image.layer_views[layer])
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue {
                    depth: 1.0,
                    stencil: 0,
                },
            })];
        let render_area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .depth_attachment(&depth_attachment[0]);
        // SAFETY: the single-layer view is cleared each scene pass, including unassigned layers.
        unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
        if let Some(matrix) = shadow.local_shadow_matrices.get(layer) {
            record_shadow_draws(
                device,
                command_buffer,
                LOCAL_SHADOW_MAP_RESOLUTION,
                layer as u32,
                *matrix,
                shadow.local_pipeline,
                shadow,
            );
        }
        // SAFETY: this closes the depth-only rendering instance opened above.
        unsafe { device.cmd_end_rendering(command_buffer) };
    }

    let to_sample = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        )
        .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image)
        .subresource_range(local_shadow_subresource_range());
    pipeline_barrier(device, command_buffer, to_sample);
    *layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

pub(crate) fn record_skybox(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
) {
    let viewport = [scene_viewport(render_extent)];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: render_extent,
    }];
    // SAFETY: called inside dynamic rendering with a compatible color/depth skybox pipeline.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.skybox_pipeline,
        );
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &scissor);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.pipeline_layout,
            0,
            &[scene.frame_set],
            &[],
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.pipeline_layout,
            2,
            &[scene.environment_set],
            &[],
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
    }
}

/// Occlusion culling включается для вида с draw'ами; режим Overdraw показывает всю
/// растеризуемую геометрию и проверку не использует.
pub(crate) fn occlusion_culling_enabled(debug_view: DebugView, draws: &[DrawCommand]) -> bool {
    !draws.is_empty() && debug_view != DebugView::Overdraw
}

/// Создаёт ресурсы occlusion culling цели при первом использовании и пишет входы кадра.
/// Вызывается после ожидания fence слота цели.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_occlusion(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    scene: &mut OffscreenTarget,
    render_extent: vk::Extent2D,
    pipelines: OcclusionPipelines,
    draws: &[DrawCommand],
    meshes: &[Option<MeshBuffers>],
) -> Result<(), RenderError> {
    if scene.occlusion.is_none() {
        scene.occlusion = Some(OcclusionTarget::new(
            instance,
            device,
            physical_device,
            render_extent,
            scene.depth.view,
            pipelines,
        )?);
    }
    let Some(occlusion) = scene.occlusion.as_mut() else {
        return Err(RenderError::InvalidState(
            "ресурсы occlusion culling отсутствуют",
        ));
    };
    occlusion.prepare(instance, physical_device, draws, |draw| {
        meshes
            .get(draw.mesh.0)?
            .as_ref()
            .map(|mesh| (mesh.index_count, mesh.bounds))
    })
}
