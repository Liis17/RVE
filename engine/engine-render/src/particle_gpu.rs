//! Vulkan-часть GPU-частиц: ресурсы эмиттеров, пайплайны и запись проходов в кадр.
//!
//! Чистая логика кольца, bounds и плана шага — в `particles.rs`. Здесь состояние эмиттера
//! обновляется планом только после успешной отправки кадра (см. `commit_particle_frame`).

use crate::culling::Frustum;
use crate::particles::{
    EmitterParamsGpu, MAX_PARTICLE_STEP, ParticleBlend, ParticleEmitterDesc, ParticleEmitterState,
    ParticleSimOwners, ParticleStepPlan, SimPushGpu, SortPushGpu, SpawnBatch, SpawnPushGpu,
    StepInput, VertexPushGpu, emitter_params, motion_margin, shape_bounds, sort_count, sort_passes,
    validate_desc,
};
use crate::{
    DEPTH_FORMAT, DebugView, DeviceBuffer, GpuPass, HostBuffer, OFFSCREEN_FORMAT, OffscreenTarget,
    OwnedDescriptorPool, ParticleEmitterFrame, ParticleEmitterId, RenderError, Renderer,
    RetiredResource, ShaderModule, Texture, TextureColorSpace, WHITE_SRGB_TEXTURE,
    color_subresource_range, create_compute_pipeline, depth_subresource_range,
    image_layout_barrier, mark_pass, pipeline_barrier, scene_viewport,
};
use ash::vk;
use std::collections::HashMap;
use std::sync::Arc;

const PARTICLE_SPAWN_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/particle_spawn.comp.spv"));
const PARTICLE_SIM_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/particle_sim.comp.spv"));
const PARTICLE_VERTEX_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/particle.vert.spv"));
const PARTICLE_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/particle.frag.spv"));
const SORT_KEY_BYTES: u64 = 8;
const PARTICLE_SORT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/particle_sort.comp.spv"));

/// Размер одной частицы в SSBO: три vec4.
const PARTICLE_GPU_SIZE: u64 = 48;
/// Push constants compute-стадий: максимум из spawn (112 Б) и sim (16 Б).
const PARTICLE_COMPUTE_PUSH_SIZE: u32 = 112;
/// Push constants вершинного шейдера: диапазон слотов и флаг порядка.
const PARTICLE_VERTEX_PUSH_SIZE: u32 = 32;
/// Стадии push constants графического прохода: вершинный шейдер читает диапазон слотов,
/// фрагментный — размер кадра. Одна константа для layout и для `cmd_push_constants`.
const PARTICLE_GRAPHICS_PUSH_STAGES: vk::ShaderStageFlags = vk::ShaderStageFlags::from_raw(
    vk::ShaderStageFlags::VERTEX.as_raw() | vk::ShaderStageFlags::FRAGMENT.as_raw(),
);
/// Push constants compute-стадий не выходят за гарантированный Vulkan-минимум 128 байт.
const _: () = assert!(PARTICLE_COMPUTE_PUSH_SIZE <= 128);
/// Storage buffer descriptors на стадию compute: три в кадровом наборе (3, 4, 5) и два в наборе частиц (particles, sort).
const PARTICLE_COMPUTE_STORAGE_BINDINGS: u32 = 5;
/// Шаг сортировки и порядка: сохраняем «слот → instance» без сортировки в этой стадии.
const PARTICLE_DRAW_VERTICES: u32 = 6;

/// Владелец GPU-состояния эмиттера: буферы, набор дескрипторов и CPU-состояние кольца.
pub(crate) struct ParticleEmitter {
    pub(crate) desc: ParticleEmitterDesc,
    /// Держит текстуру живой, пока её descriptor используется эмиттером.
    _texture: Arc<Texture>,
    pub(crate) particles: DeviceBuffer,
    /// Ключи сортировки (alpha) или заглушка 8 байт (additive): binding 2 набора эмиттера.
    _sort: DeviceBuffer,
    _params: HostBuffer,
    _pool: OwnedDescriptorPool,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) state: ParticleEmitterState,
    /// Буфер ещё не очищен; очистка записывается в первом кадре, где эмиттер симулируется.
    pub(crate) needs_clear: bool,
}

/// Пайплайны и layout-ы частиц. Создаются один раз в `Renderer::new`.
#[derive(Default)]
pub(crate) struct ParticleSystem {
    emitters: Vec<Option<ParticleEmitter>>,
    set_layout: Option<vk::DescriptorSetLayout>,
    depth_layout: Option<vk::DescriptorSetLayout>,
    compute_layout: Option<vk::PipelineLayout>,
    graphics_layout: Option<vk::PipelineLayout>,
    spawn_pipeline: Option<vk::Pipeline>,
    sim_pipeline: Option<vk::Pipeline>,
    additive_pipeline: Option<vk::Pipeline>,
    alpha_pipeline: Option<vk::Pipeline>,
    sort_pipeline: Option<vk::Pipeline>,
    frame: ParticleFrame,
}

/// Состояние одного кадра: владелец симуляции каждого эмиттера и результаты, ожидающие коммита.
#[derive(Default)]
struct ParticleFrame {
    owners: ParticleSimOwners<usize>,
    /// Состояние после шага владельца; виды, записанные позже, рисуют именно его.
    working: HashMap<usize, ParticleEmitterState>,
    /// Состояния, которые станут текущими, только если кадр отправлен.
    commits: Vec<(usize, ParticleEmitterState)>,
}

/// Работа одного эмиттера в одном виде. Копирует всё нужное записи, без ссылок на `Renderer`.
pub(crate) struct ParticleWork {
    set: vk::DescriptorSet,
    buffer: vk::Buffer,
    /// Этот вид — владелец симуляции (spawn и sim записываются здесь).
    owner: bool,
    clear: bool,
    seed: u32,
    capacity: u32,
    transform: [[f32; 4]; 4],
    spawns: Vec<SpawnBatch>,
    sim_step: f32,
    sim_first: u32,
    sim_count: u32,
    /// Рисовать (после отсечения по frustum и при Lit-режиме).
    visible: bool,
    draw_first: u32,
    draw_count: u32,
    blend: ParticleBlend,
    /// Степень двойки для сортировки (alpha) и число ключей.
    sort_count: u32,
    /// Квадрат расстояния до центра bounds: alpha рисуется назад-вперёд между эмиттерами.
    distance_sq: f32,
}

/// Хэндлы частиц, нужные записи вида.
#[derive(Clone, Copy)]
pub(crate) struct ParticlePipelines {
    spawn: vk::Pipeline,
    sim: vk::Pipeline,
    additive: vk::Pipeline,
    alpha: vk::Pipeline,
    sort: vk::Pipeline,
    compute_layout: vk::PipelineLayout,
    graphics_layout: vk::PipelineLayout,
}

/// Проход частиц в одном виде: работа по эмиттерам и пайплайны.
pub(crate) struct ParticlePass<'a> {
    pub(crate) works: &'a [ParticleWork],
    pub(crate) pipelines: ParticlePipelines,
    pub(crate) frame_set: vk::DescriptorSet,
}

impl ParticleSystem {
    /// Layout набора с depth буфером вида (binding 0, sampled image).
    pub(crate) fn depth_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.depth_layout.ok_or(RenderError::InvalidState(
            "layout depth набора частиц отсутствует",
        ))
    }

    /// Освобождает эмиттеры и пайплайны. Вызывается до уничтожения устройства.
    pub(crate) fn destroy(&mut self, device: &ash::Device) {
        self.emitters.clear();
        for pipeline in [
            self.alpha_pipeline.take(),
            self.sort_pipeline.take(),
            self.additive_pipeline.take(),
            self.sim_pipeline.take(),
            self.spawn_pipeline.take(),
        ]
        .into_iter()
        .flatten()
        {
            // SAFETY: the renderer waited for device idle before teardown.
            unsafe { device.destroy_pipeline(pipeline, None) };
        }
        for layout in [self.graphics_layout.take(), self.compute_layout.take()]
            .into_iter()
            .flatten()
        {
            // SAFETY: every particle pipeline using this layout was destroyed above.
            unsafe { device.destroy_pipeline_layout(layout, None) };
        }
        if let Some(layout) = self.depth_layout.take() {
            // SAFETY: every target set using this layout was freed with its pool.
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
        if let Some(layout) = self.set_layout.take() {
            // SAFETY: every emitter set using this layout was freed with its pool above.
            unsafe { device.destroy_descriptor_set_layout(layout, None) };
        }
    }
    /// Планирует шаги эмиттеров для вида. Владелец симуляции — первый записанный вид с `step > 0`.
    ///
    /// Вызывается до записи вида; состояние меняет только `ParticleFrame`, коммит — при отправке.
    pub(crate) fn plan_view(
        &mut self,
        emitters: &[ParticleEmitterFrame],
        particle_step: f32,
        view_projection: [[f32; 4]; 4],
        debug_view: DebugView,
        camera_position: [f32; 3],
    ) -> Result<Vec<ParticleWork>, RenderError> {
        if !particle_step.is_finite() || particle_step < 0.0 {
            return Err(RenderError::InvalidConfig(
                "шаг симуляции частиц должен быть конечным и неотрицательным",
            ));
        }
        let particle_step = particle_step.min(MAX_PARTICLE_STEP);
        let frustum = Frustum::from_view_projection(view_projection);
        let mut works = Vec::with_capacity(emitters.len());
        for frame_emitter in emitters {
            let index = frame_emitter.emitter.0;
            let emitter = self.emitters.get(index).and_then(Option::as_ref).ok_or(
                RenderError::InvalidState("ParticleEmitterId не принадлежит renderer"),
            )?;
            let owner = self.frame.owners.claim(index, particle_step);
            let base = self
                .frame
                .working
                .get(&index)
                .cloned()
                .unwrap_or_else(|| emitter.state.clone());
            let desc = &emitter.desc;
            let origin = shape_bounds(desc.shape).transformed(frame_emitter.transform);
            let input = StepInput {
                capacity: desc.capacity,
                emitting: frame_emitter.emitting,
                rate: desc.rate,
                bursts: &desc.bursts,
                duration: desc.duration,
                looping: desc.looping,
                lifetime_max: desc.lifetime[1],
                origin,
                motion_margin: motion_margin(desc),
                step: if owner { particle_step } else { 0.0 },
            };
            let plan: ParticleStepPlan = base.plan_step(&input);
            let needs_clear = emitter.needs_clear;
            let buffer = emitter.particles.buffer;
            let seed = desc.seed;
            let blend = desc.blend;
            let visible = debug_view == DebugView::Lit
                && plan.draw_count > 0
                && plan.bounds.is_none_or(|bounds| {
                    let (center, radius) = bounds.bounding_sphere();
                    frustum.intersects_sphere(center, radius)
                });
            let center = plan.bounds.unwrap_or(origin).bounding_sphere().0;
            let distance_sq = (0..3)
                .map(|axis| (center[axis] - camera_position[axis]).powi(2))
                .sum::<f32>();
            if owner {
                self.frame.working.insert(index, plan.next.clone());
                self.frame.commits.push((index, plan.next.clone()));
            }
            works.push(ParticleWork {
                set: emitter.set,
                buffer,
                owner,
                clear: owner && needs_clear,
                seed,
                capacity: desc.capacity,
                transform: frame_emitter.transform,
                spawns: if owner {
                    plan.spawns.clone()
                } else {
                    Vec::new()
                },
                sim_step: plan.sim_step,
                sim_first: plan.draw_first,
                sim_count: plan.draw_count,
                visible,
                draw_first: plan.draw_first,
                draw_count: plan.draw_count,
                blend,
                sort_count: if blend == ParticleBlend::Alpha {
                    sort_count(plan.draw_count)
                } else {
                    0
                },
                distance_sq,
            });
        }
        Ok(works)
    }
}

impl Renderer {
    /// Создаёт эмиттер частиц: буфер частиц, параметры в UBO и набор дескрипторов.
    ///
    /// Текстура `desc.texture` должна быть sRGB. Без текстуры используется процедурный мягкий диск.
    pub fn create_particle_emitter(
        &mut self,
        desc: ParticleEmitterDesc,
    ) -> Result<ParticleEmitterId, RenderError> {
        validate_desc(&desc)?;
        let texture = match desc.texture {
            Some(id) => {
                let texture = self
                    .textures
                    .get(id.0)
                    .and_then(Option::as_ref)
                    .cloned()
                    .ok_or_else(|| {
                        RenderError::InvalidTexture(
                            "текстура частиц не принадлежит renderer".to_owned(),
                        )
                    })?;
                if texture.color_space != TextureColorSpace::Srgb {
                    return Err(RenderError::InvalidTexture(
                        "текстура частиц должна быть sRGB".to_owned(),
                    ));
                }
                texture
            }
            None => self
                .textures
                .get(WHITE_SRGB_TEXTURE)
                .and_then(Option::as_ref)
                .cloned()
                .ok_or(RenderError::InvalidState("белая текстура отсутствует"))?,
        };
        let emitter = self.upload_particle_emitter(desc, texture)?;
        let index = self.particles.emitters.len();
        self.particles.emitters.push(Some(emitter));
        Ok(ParticleEmitterId(index))
    }

    /// Откладывает освобождение эмиттера до завершения кадров, которые его используют.
    pub fn destroy_particle_emitter(
        &mut self,
        emitter: ParticleEmitterId,
    ) -> Result<(), RenderError> {
        let resource = self
            .particles
            .emitters
            .get_mut(emitter.0)
            .and_then(Option::take)
            .ok_or(RenderError::InvalidState(
                "ParticleEmitterId не принадлежит renderer",
            ))?;
        self.retired
            .retire(RetiredResource::ParticleEmitter(Box::new(resource)), 0);
        Ok(())
    }

    /// Сбрасывает кольцо, таймер и дробный перенос эмиссии; буферы не очищаются, живых частиц не остаётся.
    pub fn restart_particle_emitter(
        &mut self,
        emitter: ParticleEmitterId,
    ) -> Result<(), RenderError> {
        let entry = self
            .particles
            .emitters
            .get_mut(emitter.0)
            .and_then(Option::as_mut)
            .ok_or(RenderError::InvalidState(
                "ParticleEmitterId не принадлежит renderer",
            ))?;
        entry.state = ParticleEmitterState::default();
        Ok(())
    }

    fn upload_particle_emitter(
        &self,
        desc: ParticleEmitterDesc,
        texture: Arc<Texture>,
    ) -> Result<ParticleEmitter, RenderError> {
        let device = self.device()?;
        let instance = self.instance()?;
        let size = usize::try_from(u64::from(desc.capacity) * PARTICLE_GPU_SIZE).map_err(|_| {
            RenderError::InvalidConfig("буфер частиц превышает адресное пространство")
        })?;
        let particles = DeviceBuffer::new(
            instance,
            device,
            self.physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            size,
        )?;
        let sort_size = if desc.blend == ParticleBlend::Alpha {
            usize::try_from(u64::from(desc.capacity.next_power_of_two()) * SORT_KEY_BYTES).map_err(
                |_| RenderError::InvalidConfig("буфер сортировки превышает адресное пространство"),
            )?
        } else {
            usize::try_from(SORT_KEY_BYTES)
                .map_err(|_| RenderError::InvalidConfig("заглушка сортировки"))?
        };
        let sort = DeviceBuffer::new(
            instance,
            device,
            self.physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            sort_size,
        )?;
        let params = emitter_params(&desc, desc.texture.is_some());
        let params_buffer = HostBuffer::new(
            instance,
            device,
            self.physical_device,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            pod_bytes(&params),
        )?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(2),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(1),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let layouts = [self.particle_set_layout()?];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for one emitter set of this layout.
        let set = unsafe { device.allocate_descriptor_sets(&allocate_info) }?
            .into_iter()
            .next()
            .ok_or(RenderError::InvalidState(
                "не создан набор дескрипторов частиц",
            ))?;
        let sampler = self
            .sampler
            .ok_or(RenderError::InvalidState("sampler отсутствует"))?;
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(params_buffer.buffer)
            .offset(0)
            .range(size_of::<EmitterParamsGpu>() as vk::DeviceSize)];
        let particle_info = [vk::DescriptorBufferInfo::default()
            .buffer(particles.buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let sort_info = [vk::DescriptorBufferInfo::default()
            .buffer(sort.buffer)
            .offset(0)
            .range(vk::WHOLE_SIZE)];
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(texture.image.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let sampler_info = [vk::DescriptorImageInfo::default().sampler(sampler)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&buffer_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&particle_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&sort_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&sampler_info),
        ];
        // SAFETY: every write targets a binding of this freshly allocated set with matching info.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(ParticleEmitter {
            desc,
            _texture: texture,
            particles,
            _sort: sort,
            _params: params_buffer,
            _pool: pool,
            set,
            state: ParticleEmitterState::default(),
            needs_clear: true,
        })
    }

    /// Создаёт layout набора эмиттера, layout-ы push и три пайплайна (spawn, sim, additive).
    pub(crate) fn create_particle_pipelines(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(
                    vk::ShaderStageFlags::COMPUTE
                        | vk::ShaderStageFlags::VERTEX
                        | vk::ShaderStageFlags::FRAGMENT,
                ),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE | vk::ShaderStageFlags::VERTEX),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE | vk::ShaderStageFlags::VERTEX),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: bindings are valid and describe the particle shader interface.
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }?;
        self.particles.set_layout = Some(set_layout);
        let depth_bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE | vk::ShaderStageFlags::FRAGMENT)];
        let depth_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&depth_bindings);
        // SAFETY: one sampled-image binding describes the depth input of particle stages.
        let depth_layout =
            unsafe { device.create_descriptor_set_layout(&depth_layout_info, None) }?;
        self.particles.depth_layout = Some(depth_layout);

        let set_layouts = [self.frame_set_layout()?, set_layout, depth_layout];
        let compute_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(PARTICLE_COMPUTE_PUSH_SIZE)];
        let compute_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&compute_ranges);
        // SAFETY: set layouts and push range are valid for the particle compute shaders.
        let compute_layout = unsafe { device.create_pipeline_layout(&compute_info, None) }?;
        self.particles.compute_layout = Some(compute_layout);

        let graphics_ranges = [vk::PushConstantRange::default()
            .stage_flags(PARTICLE_GRAPHICS_PUSH_STAGES)
            .offset(0)
            .size(PARTICLE_VERTEX_PUSH_SIZE)];
        let graphics_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&graphics_ranges);
        // SAFETY: set layouts and push range are valid for the particle graphics shaders.
        let graphics_layout = unsafe { device.create_pipeline_layout(&graphics_info, None) }?;
        self.particles.graphics_layout = Some(graphics_layout);

        self.particles.spawn_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_layout,
            PARTICLE_SPAWN_SHADER,
        )?);
        self.particles.sim_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_layout,
            PARTICLE_SIM_SHADER,
        )?);
        self.particles.additive_pipeline = Some(create_particle_graphics_pipeline(
            &device,
            cache,
            graphics_layout,
            ParticleBlend::Additive,
        )?);
        self.particles.alpha_pipeline = Some(create_particle_graphics_pipeline(
            &device,
            cache,
            graphics_layout,
            ParticleBlend::Alpha,
        )?);
        self.particles.sort_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_layout,
            PARTICLE_SORT_SHADER,
        )?);
        Ok(())
    }

    fn particle_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.particles.set_layout.ok_or(RenderError::InvalidState(
            "layout набора частиц отсутствует",
        ))
    }

    /// Начало кадра: сбрасывает владельцев, рабочие состояния и ожидающие коммиты.
    pub(crate) fn begin_particle_frame(&mut self) {
        self.particles.frame = ParticleFrame::default();
    }

    /// Применяет планы шагов отправленного кадра. Неотправленный кадр ничего не меняет.
    pub(crate) fn commit_particle_frame(&mut self, submitted: bool) {
        let frame = std::mem::take(&mut self.particles.frame);
        if !submitted {
            return;
        }
        for (index, state) in frame.commits {
            if let Some(Some(emitter)) = self.particles.emitters.get_mut(index) {
                emitter.state = state;
                emitter.needs_clear = false;
            }
        }
    }

    /// Пайплайны, которые получает запись вида, либо `None`, если частицы не созданы.
    pub(crate) fn particle_pipelines(&self) -> Option<ParticlePipelines> {
        Some(ParticlePipelines {
            spawn: self.particles.spawn_pipeline?,
            sim: self.particles.sim_pipeline?,
            additive: self.particles.additive_pipeline?,
            alpha: self.particles.alpha_pipeline?,
            sort: self.particles.sort_pipeline?,
            compute_layout: self.particles.compute_layout?,
            graphics_layout: self.particles.graphics_layout?,
        })
    }
}

fn create_particle_graphics_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    blend: ParticleBlend,
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, PARTICLE_VERTEX_SHADER)?;
    let fragment_module = ShaderModule::new(device, PARTICLE_FRAGMENT_SHADER)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment_module.handle)
            .name(c"main"),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // Depth только читается: тест LESS против непрозрачной геометрии, запись выключена.
    let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(false)
        .depth_compare_op(vk::CompareOp::LESS);
    // Аддитивное смешивание: цвет складывается, альфа не меняется. Альфа-смешивание берёт
    // непредумноженный цвет и альфу из фрагментного шейдера.
    let (src_color, dst_color, src_alpha, dst_alpha) = match blend {
        ParticleBlend::Additive => (
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ZERO,
            vk::BlendFactor::ONE,
        ),
        ParticleBlend::Alpha => (
            vk::BlendFactor::SRC_ALPHA,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
            vk::BlendFactor::ONE,
            vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        ),
    };
    let color_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(src_color)
        .dst_color_blend_factor(dst_color)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(src_alpha)
        .dst_alpha_blend_factor(dst_alpha)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [OFFSCREEN_FORMAT];
    let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
        .color_attachment_formats(&color_formats)
        .depth_attachment_format(DEPTH_FORMAT);
    let info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth_stencil)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .push_next(&mut rendering_info)];
    // SAFETY: shaders, layout and the single HDR color format describe a valid dynamic-rendering pipeline.
    let mut pipelines = match unsafe { device.create_graphics_pipelines(cache, &info, None) } {
        Ok(pipelines) => pipelines,
        Err((pipelines, error)) => {
            for pipeline in pipelines {
                // SAFETY: partial graphics pipelines were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(error.into());
        }
    };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("particle pipeline не создан"))
}

/// Записывает частицы вида: spawn и sim владельца, сортировка alpha, затем billboards.
///
/// Вызывается после wireframe, когда HDR color в `COLOR_ATTACHMENT_OPTIMAL`.
pub(crate) fn record_particles(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut OffscreenTarget,
    render_extent: vk::Extent2D,
    pass: &ParticlePass<'_>,
    timestamps: Option<crate::GpuMarks>,
) {
    if pass.works.is_empty() {
        return;
    }
    let consumers = pass.works.iter().any(|work| work.owner || work.visible);
    if let Some(new_layout) = particle_depth_transition(target.depth_layout, consumers) {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.depth.image,
                target.depth_layout,
                new_layout,
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COMPUTE_SHADER
                    | vk::PipelineStageFlags2::FRAGMENT_SHADER
                    | vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::SHADER_SAMPLED_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ,
                depth_subresource_range(),
            ),
        );
        target.depth_layout = new_layout;
    }
    let needs_compute = pass.works.iter().any(|work| {
        work.owner || (work.visible && work.draw_count > 0 && work.blend == ParticleBlend::Alpha)
    });
    if needs_compute {
        // Предыдущие кадры и виды могли читать или писать общие буферы: барьер на всё.
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(
                vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::TRANSFER,
            )
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE);
        let barriers = [barrier];
        let dependency = vk::DependencyInfo::default().memory_barriers(&barriers);
        // SAFETY: the barrier orders this view's particle work after all earlier GPU access.
        unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
    }
    // Очистка новых буферов перед первым спавном эмиттера.
    let mut cleared = false;
    for work in pass.works.iter().filter(|work| work.owner && work.clear) {
        // SAFETY: the buffer was created with TRANSFER_DST and is not in use by pending work.
        unsafe { device.cmd_fill_buffer(command_buffer, work.buffer, 0, vk::WHOLE_SIZE, 0) };
        cleared = true;
    }
    if cleared {
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
            .dst_access_mask(
                vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
            );
        let barriers = [barrier];
        let dependency = vk::DependencyInfo::default().memory_barriers(&barriers);
        // SAFETY: publishes the cleared particle buffers to the compute stages.
        unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
    }

    for work in pass.works.iter().filter(|work| work.owner) {
        for spawn in &work.spawns {
            let push = SpawnPushGpu {
                transform: work.transform,
                first_slot: spawn.first_slot,
                count: spawn.count,
                serial_low: spawn.first_serial as u32,
                serial_high: (spawn.first_serial >> 32) as u32,
                capacity: work.capacity,
                step: work.sim_step,
                fixed_age: match spawn.pre_age {
                    crate::particles::PreAge::Fixed(age) => age,
                    crate::particles::PreAge::Random => 0.0,
                },
                fixed_pre_age: u32::from(matches!(
                    spawn.pre_age,
                    crate::particles::PreAge::Fixed(_)
                )),
                seed: work.seed,
                padding: [0; 3],
            };
            record_compute(
                device,
                command_buffer,
                pass,
                [work.set, target.depth_set],
                pod_bytes(&push),
                spawn.count.div_ceil(crate::PARTICLE_COMPUTE_LOCAL_SIZE),
                pass.pipelines.spawn,
            );
        }
    }
    compute_barrier(device, command_buffer);
    for work in pass
        .works
        .iter()
        .filter(|work| work.owner && work.sim_count > 0 && work.sim_step > 0.0)
    {
        let push = SimPushGpu {
            step: work.sim_step,
            draw_first: work.sim_first,
            draw_count: work.sim_count,
            capacity: work.capacity,
            render_width: render_extent.width as f32,
            render_height: render_extent.height as f32,
            padding: [0; 2],
        };
        record_compute(
            device,
            command_buffer,
            pass,
            [work.set, target.depth_set],
            pod_bytes(&push),
            work.sim_count.div_ceil(crate::PARTICLE_COMPUTE_LOCAL_SIZE),
            pass.pipelines.sim,
        );
    }
    // Ключи alpha сортируются в каждом виде, где эмиттер рисуется: после симуляции.
    compute_barrier(device, command_buffer);
    for work in pass
        .works
        .iter()
        .filter(|work| work.visible && work.draw_count > 0 && work.blend == ParticleBlend::Alpha)
    {
        record_sort(device, command_buffer, pass, work, target.depth_set);
    }

    // Публикуем результаты симуляции и сортировки для вершинного и фрагментного чтения.
    let barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .dst_stage_mask(
            vk::PipelineStageFlags2::VERTEX_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
        )
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ);
    let barriers = [barrier];
    let dependency = vk::DependencyInfo::default().memory_barriers(&barriers);
    // SAFETY: the barrier publishes particle storage to vertex and fragment shaders.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };

    record_draws(device, command_buffer, target, render_extent, pass);
    mark_pass(device, command_buffer, timestamps, GpuPass::Particles);
}

/// Барьер между compute-проходами частиц: запись storage-буферов → чтение и запись.
fn compute_barrier(device: &ash::Device, command_buffer: vk::CommandBuffer) {
    let barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .dst_access_mask(
            vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        );
    let barriers = [barrier];
    let dependency = vk::DependencyInfo::default().memory_barriers(&barriers);
    // SAFETY: orders the previous compute writes before the next compute stage.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

/// Сортировка ключей alpha-эмиттера в этом виде: ключи, локальные блоки и глобальные шаги.
fn record_sort(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pass: &ParticlePass<'_>,
    work: &ParticleWork,
    depth_set: vk::DescriptorSet,
) {
    for sort_pass in sort_passes(work.sort_count) {
        let push = SortPushGpu {
            mode: sort_pass.mode,
            k: sort_pass.k,
            j: sort_pass.j,
            count: work.sort_count,
            draw_first: work.draw_first,
            draw_count: work.draw_count,
            capacity: work.capacity,
            padding: 0,
        };
        record_compute(
            device,
            command_buffer,
            pass,
            [work.set, depth_set],
            pod_bytes(&push),
            sort_pass.groups,
            pass.pipelines.sort,
        );
        compute_barrier(device, command_buffer);
    }
}

/// Billboards видимых эмиттеров вида: сначала аддитивные в порядке кадра, затем альфа от дальних.
fn record_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &OffscreenTarget,
    render_extent: vk::Extent2D,
    pass: &ParticlePass<'_>,
) {
    let mut additive = Vec::new();
    let mut alpha = Vec::new();
    for work in pass
        .works
        .iter()
        .filter(|work| work.visible && work.draw_count > 0)
    {
        match work.blend {
            ParticleBlend::Additive => additive.push(work),
            ParticleBlend::Alpha => alpha.push(work),
        }
    }
    if additive.is_empty() && alpha.is_empty() {
        return;
    }
    alpha.sort_by(|left, right| right.distance_sq.total_cmp(&left.distance_sq));
    // HDR color записан предыдущими проходами: ждём их запись, прежде чем загружать цвет (LOAD).
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
    let color_attachments = [vk::RenderingAttachmentInfo::default()
        .image_view(target.color.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let depth_attachment = particle_depth_attachment(target.depth.view);
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: render_extent,
        })
        .layer_count(1)
        .color_attachments(&color_attachments)
        .depth_attachment(&depth_attachment);
    // SAFETY: the color and depth attachments match the pipelines and are in their attachment layouts.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_set_viewport(command_buffer, 0, &[scene_viewport(render_extent)]);
        device.cmd_set_scissor(
            command_buffer,
            0,
            &[vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: render_extent,
            }],
        );
    }
    let draws = additive
        .into_iter()
        .map(|work| (work, pass.pipelines.additive, 0u32))
        .chain(
            alpha
                .into_iter()
                .map(|work| (work, pass.pipelines.alpha, 1u32)),
        );
    for (work, pipeline, sorted) in draws {
        let sets = [pass.frame_set, work.set, target.depth_set];
        let push = VertexPushGpu {
            draw_first: work.draw_first,
            capacity: work.capacity,
            draw_count: work.draw_count,
            sorted,
            render_width: render_extent.width as f32,
            render_height: render_extent.height as f32,
            padding: [0; 2],
        };
        // SAFETY: frame and emitter sets match the graphics layout; the push range covers `push`.
        unsafe {
            device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                pass.pipelines.graphics_layout,
                0,
                &sets,
                &[],
            );
            device.cmd_push_constants(
                command_buffer,
                pass.pipelines.graphics_layout,
                PARTICLE_GRAPHICS_PUSH_STAGES,
                0,
                pod_bytes(&push),
            );
            device.cmd_draw(
                command_buffer,
                PARTICLE_DRAW_VERTICES,
                work.draw_count,
                0,
                0,
            );
        }
    }
    // SAFETY: matches the preceding cmd_begin_rendering.
    unsafe { device.cmd_end_rendering(command_buffer) };
}

fn record_compute(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pass: &ParticlePass<'_>,
    sets: [vk::DescriptorSet; 2],
    push: &[u8],
    groups: u32,
    pipeline: vk::Pipeline,
) {
    if groups == 0 {
        return;
    }
    let sets = [pass.frame_set, sets[0], sets[1]];
    // SAFETY: pipeline and layout were created together; both sets match the compute layout.
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, pipeline);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            pass.pipelines.compute_layout,
            0,
            &sets,
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pass.pipelines.compute_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push,
        );
        device.cmd_dispatch(command_buffer, groups, 1, 1);
    }
}

/// Байты POD-значения без padding: все типы `particles.rs` — `repr(C)` из `f32`/`u32`.
fn pod_bytes<T: Copy>(value: &T) -> &[u8] {
    // SAFETY: T is a `repr(C)` value of plain 4-byte scalars without padding; the slice covers exactly it.
    unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

/// Лимиты устройства, нужные GPU-частицам максимальной ёмкости. Входят в общую оценку
/// устройства, поэтому `probe_devices` и `Renderer::new` дают одинаковые причины.
pub(crate) fn particle_device_limit_reasons(limits: &vk::PhysicalDeviceLimits) -> Vec<String> {
    let workgroups = crate::particles::MAX_ADDITIVE_PARTICLE_CAPACITY
        .div_ceil(crate::PARTICLE_COMPUTE_LOCAL_SIZE);
    let mut reasons = Vec::new();
    if limits.max_compute_work_group_invocations < crate::PARTICLE_COMPUTE_LOCAL_SIZE
        || limits.max_compute_work_group_size[0] < crate::PARTICLE_COMPUTE_LOCAL_SIZE
        || limits.max_compute_work_group_count[0] < workgroups
    {
        reasons.push("лимиты compute workgroup не поддерживают GPU-частицы".to_owned());
    }
    if u64::from(limits.max_storage_buffer_range)
        < u64::from(crate::particles::MAX_ADDITIVE_PARTICLE_CAPACITY) * PARTICLE_GPU_SIZE
    {
        reasons.push("maxStorageBufferRange меньше буфера частиц максимальной ёмкости".to_owned());
    }
    if limits.max_push_constants_size < PARTICLE_COMPUTE_PUSH_SIZE {
        reasons.push("maxPushConstantsSize меньше размера push constants частиц".to_owned());
    }
    if limits.max_per_stage_descriptor_storage_buffers < PARTICLE_COMPUTE_STORAGE_BINDINGS {
        reasons.push(
            "лимит storage buffer descriptors на стадию не поддерживает GPU-частицы".to_owned(),
        );
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capable_limits() -> vk::PhysicalDeviceLimits {
        vk::PhysicalDeviceLimits {
            max_compute_work_group_invocations: 128,
            max_compute_work_group_size: [128, 128, 64],
            max_compute_work_group_count: [65_535, 65_535, 65_535],
            max_storage_buffer_range: u32::MAX,
            max_push_constants_size: 128,
            max_per_stage_descriptor_storage_buffers: 8,
            ..Default::default()
        }
    }

    #[test]
    fn particle_limits_accept_spec_capable_device() {
        assert!(particle_device_limit_reasons(&capable_limits()).is_empty());
    }

    #[test]
    fn particle_limits_report_each_missing_requirement() {
        let cases = [
            vk::PhysicalDeviceLimits {
                max_compute_work_group_invocations: 32,
                ..capable_limits()
            },
            vk::PhysicalDeviceLimits {
                max_storage_buffer_range: 1024,
                ..capable_limits()
            },
            vk::PhysicalDeviceLimits {
                max_push_constants_size: 64,
                ..capable_limits()
            },
            vk::PhysicalDeviceLimits {
                max_per_stage_descriptor_storage_buffers: 3,
                ..capable_limits()
            },
        ];
        for limits in cases {
            assert_eq!(particle_device_limit_reasons(&limits).len(), 1);
        }
    }

    #[test]
    fn graphics_push_range_covers_vertex_and_fragment_stages() {
        assert!(PARTICLE_GRAPHICS_PUSH_STAGES.contains(vk::ShaderStageFlags::VERTEX));
        assert!(PARTICLE_GRAPHICS_PUSH_STAGES.contains(vk::ShaderStageFlags::FRAGMENT));
    }

    #[test]
    fn particle_storage_stride_matches_shader_struct() {
        assert_eq!(PARTICLE_GPU_SIZE, 48);
    }
}

#[cfg(test)]
mod shader_tests {
    use super::*;

    #[test]
    fn particle_shaders_are_spirv_words() {
        for shader in [
            PARTICLE_SPAWN_SHADER,
            PARTICLE_SIM_SHADER,
            PARTICLE_VERTEX_SHADER,
            PARTICLE_FRAGMENT_SHADER,
        ] {
            let words = ash::util::read_spv(&mut std::io::Cursor::new(shader)).unwrap();
            assert_eq!(words[0], 0x0723_0203);
        }
    }
}

/// Layout depth буфера, в котором частицы читают его в compute и как read-only depth attachment.
const PARTICLE_DEPTH_LAYOUT: vk::ImageLayout = vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL;

/// Depth только для чтения: тест глубины против геометрии, запись отключена.
/// `STORE` сохраняет содержимое: TAA/апскейлеры и последующие проходы читают depth после частиц.
fn particle_depth_attachment<'a>(view: vk::ImageView) -> vk::RenderingAttachmentInfo<'a> {
    vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(PARTICLE_DEPTH_LAYOUT)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)
}

/// Маски источника барьера depth перед финальным переходом в `SHADER_READ_ONLY_OPTIMAL`. После частиц depth
/// читается в compute, fragment и depth-тестах; `STORE` в конце прохода частиц — запись в depth-тестах,
/// поэтому она входит в источник.
pub(crate) fn particle_depth_source_masks(
    layout: vk::ImageLayout,
) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    if layout == PARTICLE_DEPTH_LAYOUT {
        (
            vk::PipelineStageFlags2::COMPUTE_SHADER
                | vk::PipelineStageFlags2::FRAGMENT_SHADER
                | vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        )
    } else {
        (
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ,
        )
    }
}

/// Layout depth перед частицами. `None`, если переход не нужен: нет потребителей или depth уже read-only.
fn particle_depth_transition(current: vk::ImageLayout, consumers: bool) -> Option<vk::ImageLayout> {
    (consumers && current != PARTICLE_DEPTH_LAYOUT).then_some(PARTICLE_DEPTH_LAYOUT)
}

#[cfg(test)]
mod depth_tests {
    use super::*;

    #[test]
    fn particles_without_consumers_do_not_change_depth_layout() {
        assert_eq!(
            particle_depth_transition(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL, false),
            None
        );
    }

    #[test]
    fn read_only_depth_source_waits_for_compute_fragment_and_depth_tests_with_read_access() {
        let (stages, access) = particle_depth_source_masks(PARTICLE_DEPTH_LAYOUT);
        assert!(stages.contains(vk::PipelineStageFlags2::COMPUTE_SHADER));
        assert!(stages.contains(vk::PipelineStageFlags2::FRAGMENT_SHADER));
        assert!(stages.contains(vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS));
        assert!(stages.contains(vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS));
        assert!(access.contains(vk::AccessFlags2::SHADER_SAMPLED_READ));
        assert!(!access.contains(vk::AccessFlags2::SHADER_STORAGE_WRITE));
    }

    #[test]
    fn particle_depth_store_keeps_contents_for_later_passes() {
        let attachment = particle_depth_attachment(vk::ImageView::null());
        assert_eq!(attachment.store_op, vk::AttachmentStoreOp::STORE);
        assert_eq!(attachment.image_layout, PARTICLE_DEPTH_LAYOUT);
    }

    #[test]
    fn attachment_depth_source_keeps_depth_write_masks() {
        let (stages, access) =
            particle_depth_source_masks(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL);
        assert!(!stages.contains(vk::PipelineStageFlags2::COMPUTE_SHADER));
        assert!(access.contains(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE));
    }

    #[test]
    fn particles_move_depth_to_read_only_once_per_view() {
        assert_eq!(
            particle_depth_transition(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL, true),
            Some(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)
        );
        assert_eq!(
            particle_depth_transition(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL, true),
            None
        );
    }
}
