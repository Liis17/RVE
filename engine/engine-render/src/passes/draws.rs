//! Пакетирование и запись draw-вызовов сцены и теней, выбор конвейера.

use crate::*;

/// Записывает draw'ы прохода. С `indirect` каждый draw читает `VkDrawIndexedIndirectCommand`
/// по своему индексу из буфера occlusion culling, и GPU пропускает невидимые.
pub(crate) fn record_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
    pass: DrawPass,
    indirect: Option<vk::Buffer>,
) {
    if scene.draws.is_empty() {
        return;
    }
    let viewport = [scene_viewport(render_extent)];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: render_extent,
    }];
    // SAFETY: recording happens inside dynamic rendering with a pipeline built for its formats.
    unsafe {
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
    }
    let frustum =
        Frustum::from_view_projection_with_padding(scene.view_projection, scene.culling_padding);
    let sorted = matches!(pass, DrawPass::Blend).then(|| {
        back_to_front_order(scene.draws, scene.camera_position, |draw| {
            scene.meshes.get(draw.mesh.0)?.as_ref()?.bounds
        })
    });
    let draw_count = sorted.as_ref().map_or(scene.draws.len(), Vec::len);
    let visible = (0..draw_count).filter_map(|position| {
        let index = sorted
            .as_ref()
            .map_or(position, |indices| indices[position]);
        let draw = &scene.draws[index];
        let material = material_for_draw(scene.materials, draw)?;
        let pipeline_choice = select_pipeline_choice(
            material.shader,
            material.alpha_mode,
            pass,
            scene.depth_prepass,
        )?;
        let pipeline = scene_pipeline(scene, pipeline_choice)?;
        // Индексы проверены validate_draws до начала кадра.
        let mesh = scene.meshes[draw.mesh.0].as_ref()?;
        // Skinned draw'ы не отсекаются по bind-pose AABB: поза меняется каждый кадр.
        if draw.skin.is_none()
            && mesh
                .bounds
                .is_some_and(|bounds| !frustum.intersects(draw.model, bounds))
        {
            update_stats(scene.stats, |stats| stats.scene_culled += 1);
            return None;
        }
        let vertex_buffers = scene.skin_draws.get(index).copied().flatten().map_or(
            [mesh.vertex_buffer.buffer, mesh.vertex_buffer.buffer],
            |buffers| [buffers.current, buffers.previous],
        );
        Some((
            index,
            SceneDrawKey {
                pipeline,
                material_set: material.set,
                vertex_buffers,
                index_buffer: mesh.index_buffer.buffer,
                index_count: mesh.index_count,
                rasterization: draw_rasterization_state(material.double_sided, &draw.model),
                lod_level: draw.lod_level,
            },
        ))
    });
    // SAFETY: the frame-slot instance buffer stays alive until this frame's fence completes.
    unsafe {
        device.cmd_bind_vertex_buffers(command_buffer, 2, &[scene.instances.buffer], &[0]);
    }
    let mut bound_pipeline = None;
    let mut rasterization_state = None;
    for batch in batch_draws(visible) {
        let key = batch.key;
        if bound_pipeline != Some(key.pipeline) {
            // SAFETY: the selected pipeline matches the active dynamic-rendering attachments.
            unsafe {
                device.cmd_bind_pipeline(
                    command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    key.pipeline,
                )
            };
            bound_pipeline = Some(key.pipeline);
        }
        if rasterization_state != Some(key.rasterization) {
            // SAFETY: the graphics pipeline declares both values as dynamic Vulkan 1.3 states.
            unsafe {
                device.cmd_set_cull_mode(command_buffer, key.rasterization.0);
                device.cmd_set_front_face(command_buffer, key.rasterization.1);
            }
            rasterization_state = Some(key.rasterization);
        }
        let push_constants = push_constant_bytes(key.lod_level, 0);
        // SAFETY: mesh buffers and material descriptors stay alive until this frame's fence completes.
        unsafe {
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                scene.pipeline_layout,
                1,
                &[key.material_set],
                &[],
            );
            device.cmd_bind_vertex_buffers(command_buffer, 0, &key.vertex_buffers, &[0, 0]);
            device.cmd_bind_index_buffer(
                command_buffer,
                key.index_buffer,
                0,
                vk::IndexType::UINT32,
            );
            device.cmd_push_constants(
                command_buffer,
                scene.pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                &push_constants,
            );
            if let Some(buffer) = indirect {
                record_indirect_batch(device, command_buffer, buffer, batch.first, batch.count);
            } else {
                device.cmd_draw_indexed(
                    command_buffer,
                    key.index_count,
                    batch.count,
                    0,
                    0,
                    scene.instances.first + batch.first as u32,
                );
            }
        }
        update_stats(scene.stats, |stats| stats.scene_draw_calls += 1);
    }
}

/// Состояние, общее для экземпляров одного instanced-вызова сцены.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SceneDrawKey {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) material_set: vk::DescriptorSet,
    /// Текущие и прошлые вершины: у skinned draw'а — выходные копии его экземпляра.
    pub(crate) vertex_buffers: [vk::Buffer; 2],
    pub(crate) index_buffer: vk::Buffer,
    pub(crate) index_count: u32,
    pub(crate) rasterization: (vk::CullModeFlags, vk::FrontFace),
    pub(crate) lod_level: u8,
}

/// Состояние, общее для экземпляров одного instanced-вызова shadow pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShadowDrawKey {
    pub(crate) material_set: vk::DescriptorSet,
    pub(crate) vertex_buffer: vk::Buffer,
    pub(crate) index_buffer: vk::Buffer,
    pub(crate) index_count: u32,
    pub(crate) rasterization: (vk::CullModeFlags, vk::FrontFace),
}

/// Instanced-вызов: экземпляры draw'ов `first..first + count` с общим состоянием `key`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DrawBatch<K> {
    pub(crate) first: usize,
    pub(crate) count: u32,
    pub(crate) key: K,
}

/// Склеивает идущие подряд видимые draw'ы с равным состоянием в instanced-вызовы.
///
/// Элементы объединяются, только если их индексы draw'ов тоже идут подряд: экземпляры
/// вызова лежат в буфере кадра непрерывно, а порядок рисования (в том числе сортировка
/// blend-прохода) не меняется.
/// Рисует instanced-батч occlusion culling: по `VkDrawIndexedIndirectCommand` на draw
/// (`instanceCount` 0 или 1, `firstInstance` — слот экземпляра draw'а), не больше
/// гарантированного `maxDrawIndirectCount` за вызов.
pub(crate) fn record_indirect_batch(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    buffer: vk::Buffer,
    first: usize,
    count: u32,
) {
    for (offset, chunk) in indirect_chunks(first, count) {
        // SAFETY: the buffer holds one command per frame draw, written by occlusion.comp and
        // published to DRAW_INDIRECT by a barrier; multiDrawIndirect is enabled with culling.
        unsafe {
            device.cmd_draw_indexed_indirect(
                command_buffer,
                buffer,
                offset,
                chunk,
                occlusion::INDIRECT_COMMAND_SIZE,
            );
        }
    }
}

/// Делит батч на вызовы не длиннее `MAX_DRAW_INDIRECT_COUNT`: (смещение в буфере, число команд).
pub(crate) fn indirect_chunks(
    first: usize,
    count: u32,
) -> impl Iterator<Item = (vk::DeviceSize, u32)> {
    (0..count)
        .step_by(MAX_DRAW_INDIRECT_COUNT as usize)
        .map(move |start| {
            let offset = (first as vk::DeviceSize + vk::DeviceSize::from(start))
                * vk::DeviceSize::from(occlusion::INDIRECT_COMMAND_SIZE);
            (offset, (count - start).min(MAX_DRAW_INDIRECT_COUNT))
        })
}

/// Минимальный `maxDrawIndirectCount` при поддержке `multiDrawIndirect`.
pub(crate) const MAX_DRAW_INDIRECT_COUNT: u32 = u16::MAX as u32;

pub(crate) fn batch_draws<K: PartialEq>(
    items: impl IntoIterator<Item = (usize, K)>,
) -> impl Iterator<Item = DrawBatch<K>> {
    let mut items = items.into_iter().peekable();
    std::iter::from_fn(move || {
        let (first, key) = items.next()?;
        let mut count = 1_u32;
        while items
            .next_if(|(index, next)| *index == first + count as usize && *next == key)
            .is_some()
        {
            count += 1;
        }
        Some(DrawBatch { first, count, key })
    })
}

pub(crate) fn scene_pipeline(
    scene: &ScenePass<'_>,
    choice: PipelineChoice,
) -> Option<vk::Pipeline> {
    match choice {
        PipelineChoice::Builtin(DrawPipeline::Prepass) => scene.prepass_pipeline,
        PipelineChoice::Builtin(DrawPipeline::Opaque) if scene.depth_prepass => {
            Some(scene.opaque_equal_pipeline.unwrap_or(scene.pipeline))
        }
        PipelineChoice::Builtin(DrawPipeline::Opaque) => Some(scene.pipeline),
        PipelineChoice::Builtin(DrawPipeline::Mask) => Some(scene.mask_pipeline),
        PipelineChoice::Builtin(DrawPipeline::Blend) => Some(scene.blend_pipeline),
        PipelineChoice::Builtin(DrawPipeline::Overdraw) => Some(scene.overdraw_pipeline),
        PipelineChoice::Builtin(DrawPipeline::Wireframe) => {
            Some(scene.wireframe_pipeline.unwrap_or(scene.pipeline))
        }
        PipelineChoice::Surface(shader, variant) => scene
            .surface_shaders
            .get(shader.0)
            .and_then(Option::as_ref)
            .map(|shader| match variant {
                SurfacePipelineVariant::Opaque => shader.pipelines.opaque,
                SurfacePipelineVariant::OpaqueAfterPrepass => shader.pipelines.opaque_equal,
                SurfacePipelineVariant::Mask => shader.pipelines.mask,
                SurfacePipelineVariant::Blend => shader.pipelines.blend,
            }),
    }
}

pub(crate) fn update_stats(stats: &Cell<DrawStats>, update: impl FnOnce(&mut DrawStats)) {
    let mut value = stats.get();
    update(&mut value);
    stats.set(value);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrawPipeline {
    Prepass,
    Opaque,
    Mask,
    Blend,
    Overdraw,
    Wireframe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfacePipelineVariant {
    Opaque,
    OpaqueAfterPrepass,
    Mask,
    Blend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PipelineChoice {
    Builtin(DrawPipeline),
    Surface(ShaderId, SurfacePipelineVariant),
}

pub(crate) fn select_pipeline_choice(
    shader: Option<ShaderId>,
    alpha: AlphaMode,
    pass: DrawPass,
    depth_prepass: bool,
) -> Option<PipelineChoice> {
    match pass {
        DrawPass::Prepass => match alpha {
            AlphaMode::Opaque => Some(PipelineChoice::Builtin(DrawPipeline::Prepass)),
            AlphaMode::Mask { .. } | AlphaMode::Blend => None,
        },
        DrawPass::Overdraw => Some(PipelineChoice::Builtin(DrawPipeline::Overdraw)),
        DrawPass::Wireframe => Some(PipelineChoice::Builtin(DrawPipeline::Wireframe)),
        DrawPass::OpaqueAndMask => match alpha {
            AlphaMode::Opaque => Some(shader.map_or(
                PipelineChoice::Builtin(DrawPipeline::Opaque),
                |shader| {
                    PipelineChoice::Surface(
                        shader,
                        if depth_prepass {
                            SurfacePipelineVariant::OpaqueAfterPrepass
                        } else {
                            SurfacePipelineVariant::Opaque
                        },
                    )
                },
            )),
            AlphaMode::Mask { .. } => Some(
                shader.map_or(PipelineChoice::Builtin(DrawPipeline::Mask), |shader| {
                    PipelineChoice::Surface(shader, SurfacePipelineVariant::Mask)
                }),
            ),
            AlphaMode::Blend => None,
        },
        DrawPass::Blend => match alpha {
            AlphaMode::Blend => Some(
                shader.map_or(PipelineChoice::Builtin(DrawPipeline::Blend), |shader| {
                    PipelineChoice::Surface(shader, SurfacePipelineVariant::Blend)
                }),
            ),
            AlphaMode::Opaque | AlphaMode::Mask { .. } => None,
        },
    }
}

pub(crate) fn material_for_draw<'a>(
    materials: &'a [Option<GpuMaterial>],
    draw: &DrawCommand,
) -> Option<&'a GpuMaterial> {
    materials
        .get(
            draw.material
                .map_or(DEFAULT_MATERIAL, |material| material.0),
        )
        .and_then(Option::as_ref)
}

pub(crate) fn has_transparent_draws(scene: &ScenePass<'_>) -> bool {
    scene.draws.iter().any(|draw| {
        material_for_draw(scene.materials, draw)
            .is_some_and(|material| matches!(material.alpha_mode, AlphaMode::Blend))
    })
}

pub(crate) fn shadow_casters(
    draws: &[DrawCommand],
    mut alpha_mode: impl FnMut(&DrawCommand) -> AlphaMode,
) -> impl Iterator<Item = (usize, &DrawCommand)> {
    draws
        .iter()
        .enumerate()
        .filter(move |(_, draw)| draw.casts_shadow && !matches!(alpha_mode(draw), AlphaMode::Blend))
}

pub(crate) fn shadow_caster_is_outside(
    draw: &DrawCommand,
    bounds: Option<LocalBounds>,
    frustum: Option<Frustum>,
) -> bool {
    frustum
        .is_some_and(|frustum| bounds.is_some_and(|bounds| !frustum.intersects(draw.model, bounds)))
}

pub(crate) fn record_shadow_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    extent: u32,
    cascade_index: u32,
    light_view_projection: [[f32; 4]; 4],
    pipeline: vk::Pipeline,
    shadow: &ShadowPass<'_>,
) {
    if shadow.draws.is_empty() {
        return;
    }
    let extent = vk::Extent2D {
        width: extent,
        height: extent,
    };
    let viewport = [scene_viewport(extent)];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    }];
    // SAFETY: recording happens inside depth-only dynamic rendering with the shadow pipeline.
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &scissor);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            shadow.pipeline_layout,
            0,
            &[shadow.frame_set],
            &[],
        );
    }
    let frustum = Some(Frustum::from_view_projection(light_view_projection));
    let visible = shadow_casters(shadow.draws, |draw| {
        material_for_draw(shadow.materials, draw)
            .map_or(AlphaMode::Opaque, |material| material.alpha_mode)
    })
    .filter_map(|(index, draw)| {
        let material = material_for_draw(shadow.materials, draw)?;
        let mesh = shadow.meshes[draw.mesh.0].as_ref()?;
        // Skinned draw'ы не отсекаются по bind-pose AABB: поза меняется каждый кадр.
        if draw.skin.is_none() && shadow_caster_is_outside(draw, mesh.bounds, frustum) {
            update_stats(shadow.stats, |stats| stats.shadow_culled += 1);
            return None;
        }
        let vertex_buffer = shadow
            .skin_draws
            .get(index)
            .copied()
            .flatten()
            .map_or(mesh.vertex_buffer.buffer, |buffers| buffers.current);
        Some((
            index,
            ShadowDrawKey {
                material_set: material.set,
                vertex_buffer,
                index_buffer: mesh.index_buffer.buffer,
                index_count: mesh.index_count,
                rasterization: draw_rasterization_state(material.double_sided, &draw.model),
            },
        ))
    });
    let push_constants = push_constant_bytes(0, cascade_index);
    // SAFETY: the instance buffer outlives the frame; the push range matches the pipeline layout.
    unsafe {
        device.cmd_bind_vertex_buffers(command_buffer, 1, &[shadow.instances.buffer], &[0]);
        device.cmd_push_constants(
            command_buffer,
            shadow.pipeline_layout,
            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            0,
            &push_constants,
        );
    }
    let mut rasterization_state = None;
    for batch in batch_draws(visible) {
        let key = batch.key;
        if rasterization_state != Some(key.rasterization) {
            // SAFETY: the shadow graphics pipeline declares both values as dynamic Vulkan 1.3 states.
            unsafe {
                device.cmd_set_cull_mode(command_buffer, key.rasterization.0);
                device.cmd_set_front_face(command_buffer, key.rasterization.1);
            }
            rasterization_state = Some(key.rasterization);
        }
        // SAFETY: draw ids were checked before recording; material descriptors and buffers stay alive.
        unsafe {
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                shadow.pipeline_layout,
                1,
                &[key.material_set],
                &[],
            );
            device.cmd_bind_vertex_buffers(command_buffer, 0, &[key.vertex_buffer], &[0]);
            device.cmd_bind_index_buffer(
                command_buffer,
                key.index_buffer,
                0,
                vk::IndexType::UINT32,
            );
            device.cmd_draw_indexed(
                command_buffer,
                key.index_count,
                batch.count,
                0,
                0,
                shadow.instances.first + batch.first as u32,
            );
        }
        update_stats(shadow.stats, |stats| stats.shadow_draw_calls += 1);
    }
}

pub(crate) fn draw_rasterization_state(
    double_sided: bool,
    model: &[[f32; 4]; 4],
) -> (vk::CullModeFlags, vk::FrontFace) {
    let cull_mode = if double_sided {
        vk::CullModeFlags::NONE
    } else {
        vk::CullModeFlags::BACK
    };
    let x = [model[0][0], model[0][1], model[0][2]];
    let y = [model[1][0], model[1][1], model[1][2]];
    let z = [model[2][0], model[2][1], model[2][2]];
    let determinant = x[0] * (y[1] * z[2] - y[2] * z[1]) - x[1] * (y[0] * z[2] - y[2] * z[0])
        + x[2] * (y[0] * z[1] - y[1] * z[0]);
    let front_face = if determinant < 0.0 {
        vk::FrontFace::CLOCKWISE
    } else {
        vk::FrontFace::COUNTER_CLOCKWISE
    };
    (cull_mode, front_face)
}

/// Viewport с отрицательной высотой направляет ось Y clip space вверх.
pub(crate) fn scene_viewport(extent: vk::Extent2D) -> vk::Viewport {
    vk::Viewport {
        x: 0.0,
        y: extent.height as f32,
        width: extent.width as f32,
        height: -(extent.height as f32),
        min_depth: 0.0,
        max_depth: 1.0,
    }
}

/// `params.x` — LOD для отладочного вида, `params.y` — каскад или слой тени.
pub(crate) fn push_constant_bytes(lod_level: u8, shadow_layer: u32) -> [u8; PUSH_CONSTANTS_SIZE] {
    let mut bytes = [0; PUSH_CONSTANTS_SIZE];
    bytes[..4].copy_from_slice(&(lod_level as u32).to_ne_bytes());
    bytes[4..8].copy_from_slice(&shadow_layer.to_ne_bytes());
    bytes
}

/// Упаковывает данные экземпляров всех видов подряд в порядке draw'ов.
///
/// Возвращает байты и для каждого вида индекс экземпляра его первого draw'а:
/// draw `i` вида `v` читает экземпляр `bases[v] + i`.
pub(crate) fn pack_instances<'a>(
    views: impl IntoIterator<Item = &'a [DrawCommand]>,
) -> (Vec<u8>, Vec<u32>) {
    let mut bytes = Vec::new();
    let mut bases = Vec::new();
    for draws in views {
        bases.push((bytes.len() / INSTANCE_STRIDE) as u32);
        bytes.reserve(draws.len() * INSTANCE_STRIDE);
        for draw in draws {
            let normal_from_model = normal_matrix(&draw.model);
            bytes.extend_from_slice(&float_bytes::<64>(draw.model.as_flattened()));
            bytes.extend_from_slice(&float_bytes::<48>(normal_from_model[..3].as_flattened()));
            bytes.extend_from_slice(&float_bytes::<64>(
                draw.previous_model
                    .as_ref()
                    .unwrap_or(&draw.model)
                    .as_flattened(),
            ));
        }
    }
    (bytes, bases)
}
