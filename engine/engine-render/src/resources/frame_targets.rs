//! Ресурсы кадрового слота и offscreen-цели сцены.

use crate::*;

pub(crate) struct FrameResources {
    pub(crate) device: ash::Device,
    pub(crate) command_buffer: vk::CommandBuffer,
    pub(crate) fence: vk::Fence,
    pub(crate) image_available: vk::Semaphore,
    pub(crate) target: Option<FrameTarget>,
    pub(crate) tonemap_set: vk::DescriptorSet,
    pub(crate) output_set: vk::DescriptorSet,
    /// Вершины оверлея этого слота.
    pub(crate) overlay_buffer: GrowingBuffer,
    pub(crate) ui_vertices: GrowingBuffer,
    pub(crate) ui_indices: GrowingBuffer,
    /// Пул границ GPU-проходов слота; `None`, если очередь не поддерживает timestamp queries.
    pub(crate) timestamps: Option<vk::QueryPool>,
    /// Границы записаны и отправлены, но ещё не прочитаны.
    pub(crate) timestamps_pending: bool,
    /// Виды последнего записанного кадра слота; нужны для разбора прочитанных границ.
    pub(crate) gpu_layout: GpuProfileLayout,
    /// Последняя успешная отправка этого слота; начальный signaled fence не имеет поколения.
    pub(crate) submission_generation: u64,
    /// Палитры skinning этого слота; растут до следующей степени двойки.
    pub(crate) skin_palettes: GrowingBuffer,
    /// Данные экземпляров mesh draw'ов слота ([`pack_instances`]).
    pub(crate) instances: GrowingBuffer,
}

impl FrameResources {
    pub(crate) fn new(
        device: &ash::Device,
        command_buffer: vk::CommandBuffer,
        timestamps: bool,
    ) -> Result<Self, RenderError> {
        let mut frame = Self {
            device: device.clone(),
            command_buffer,
            fence: vk::Fence::null(),
            image_available: vk::Semaphore::null(),
            target: None,
            tonemap_set: vk::DescriptorSet::null(),
            output_set: vk::DescriptorSet::null(),
            overlay_buffer: GrowingBuffer::default(),
            ui_vertices: GrowingBuffer::default(),
            ui_indices: GrowingBuffer::default(),
            timestamps: None,
            timestamps_pending: false,
            gpu_layout: GpuProfileLayout::default(),
            submission_generation: 0,
            skin_palettes: GrowingBuffer::default(),
            instances: GrowingBuffer::default(),
        };
        // SAFETY: creating an unsignaled binary semaphore uses a valid default create info.
        frame.image_available =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: the fence starts signaled so the first frame can wait before using the slot.
        frame.fence = unsafe { device.create_fence(&fence_info, None) }?;
        if timestamps {
            let pool_info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(TIMESTAMP_QUERY_COUNT);
            // SAFETY: the queue family reported non-zero timestampValidBits.
            frame.timestamps = Some(unsafe { device.create_query_pool(&pool_info, None) }?);
        }
        Ok(frame)
    }

    /// Читает границы прошлой отправки этого слота и разбирает их по видам.
    ///
    /// Вызывается после ожидания fence слота. `NOT_READY` не отменяет чтение: доступность
    /// задаётся для каждого запроса, поэтому невыполненные проходы дают `None`.
    pub(crate) fn read_gpu_timings(
        &mut self,
        device: &ash::Device,
        valid_bits: u32,
        period: f32,
    ) -> Result<Option<GpuFrameTimings>, RenderError> {
        let Some(pool) = self.timestamps.filter(|_| self.timestamps_pending) else {
            return Ok(None);
        };
        self.timestamps_pending = false;
        // Для каждого запроса Vulkan пишет значение и флаг доступности: шаг буфера — 16 байт.
        let mut results = [[0_u64; 2]; TIMESTAMP_QUERY_COUNT as usize];
        // SAFETY: the pool was written by a submission whose fence was waited, and the buffer length equals its query count.
        match unsafe {
            device.get_query_pool_results(
                pool,
                0,
                &mut results,
                vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
            )
        } {
            Ok(()) | Err(vk::Result::NOT_READY) => {}
            Err(error) => return Err(RenderError::Vulkan(error)),
        }
        let queries = available_timestamps(&results);
        Ok(Some(self.gpu_layout.decode(&queries, valid_bits, period)))
    }

    /// Копирует вершины оверлея в буфер слота, увеличивая его при нехватке места.
    ///
    /// Вызывается после ожидания fence слота, когда GPU уже не читает прежний буфер.
    pub(crate) fn write_overlay(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        vertices: &[OverlayVertex],
    ) -> Result<(), RenderError> {
        self.overlay_buffer.write(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            overlay_vertex_bytes(vertices),
        )
    }

    /// Копирует данные экземпляров draw'ов всех видов в буфер слота и возвращает
    /// буфер с индексом первого экземпляра каждого вида; без draw'ов буфер null.
    ///
    /// Вызывается после ожидания fence слота, когда GPU уже не читает прежние данные.
    pub(crate) fn write_instances<'a>(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        views: impl IntoIterator<Item = &'a [DrawCommand]>,
    ) -> Result<(vk::Buffer, Vec<u32>), RenderError> {
        let (bytes, bases) = pack_instances(views);
        if bytes.is_empty() {
            return Ok((vk::Buffer::null(), bases));
        }
        self.instances.write(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            &bytes,
        )?;
        let buffer = self
            .instances
            .buffer
            .as_ref()
            .ok_or(RenderError::InvalidState(
                "буфер экземпляров кадра отсутствует",
            ))?;
        Ok((buffer.buffer, bases))
    }

    /// Копирует UI кадра (всех видов и swapchain, если упакован вместе) в буферы слота.
    /// Вызывается после ожидания fence слота.
    pub(crate) fn write_ui(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        packed: &PackedUi,
    ) -> Result<(), RenderError> {
        self.ui_vertices.write(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            &packed.vertices,
        )?;
        self.ui_indices.write(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::INDEX_BUFFER,
            &packed.indices,
        )
    }
}

impl Drop for FrameResources {
    fn drop(&mut self) {
        if let Some(pool) = self.timestamps.take() {
            // SAFETY: the renderer waits for device idle before dropping frame resources.
            unsafe { self.device.destroy_query_pool(pool, None) };
        }
        if self.fence != vk::Fence::null() {
            // SAFETY: the renderer waits for device idle before dropping frame resources.
            unsafe { self.device.destroy_fence(self.fence, None) };
        }
        if self.image_available != vk::Semaphore::null() {
            // SAFETY: the renderer waits for device idle before dropping frame resources.
            unsafe { self.device.destroy_semaphore(self.image_available, None) };
        }
    }
}

/// Создаёт цель AO или пересоздаёт её, если качество сменило разрешение сырого GTAO.
/// Вызывается после ожидания fence кадрового слота: прежняя цель слота больше не используется GPU.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_ao_target(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    scene: &mut OffscreenTarget,
    render_extent: vk::Extent2D,
    quality: AmbientOcclusionQuality,
    set_layout: vk::DescriptorSetLayout,
) -> Result<(), RenderError> {
    let (width, height) = post::ao_resolution(quality, (render_extent.width, render_extent.height));
    let raw_extent = vk::Extent2D { width, height };
    if scene
        .ao
        .as_ref()
        .is_some_and(|ao| ao.raw_extent == raw_extent)
    {
        return Ok(());
    }
    scene.ao = Some(AoTarget::new(
        instance,
        device,
        physical_device,
        render_extent,
        raw_extent,
        scene.depth.view,
        set_layout,
    )?);
    Ok(())
}

pub(crate) struct OffscreenTarget {
    pub(crate) color: DeviceImage,
    pub(crate) velocity: DeviceImage,
    pub(crate) depth: DeviceImage,
    pub(crate) layout: vk::ImageLayout,
    pub(crate) velocity_layout: vk::ImageLayout,
    pub(crate) depth_layout: vk::ImageLayout,
    /// Набор с depth для проходов частиц; пул освобождается вместе с целью.
    pub(crate) _depth_pool: OwnedDescriptorPool,
    pub(crate) depth_set: vk::DescriptorSet,
    /// Экранный AO цели; создаётся, когда эффект запланирован, и живёт вместе с целью.
    pub(crate) ao: Option<AoTarget>,
    /// Hi-Z и indirect-аргументы occlusion culling; создаются для первого вида с тестом.
    pub(crate) occlusion: Option<OcclusionTarget>,
}

impl OffscreenTarget {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        depth_set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let depth = DeviceImage::new(
            instance,
            device,
            physical_device,
            extent,
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            depth_subresource_range(),
        )?;
        let (depth_pool, depth_set) = allocate_depth_set(device, depth_set_layout, depth.view)?;
        Ok(Self {
            color: DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            velocity: DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                VELOCITY_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            depth,
            layout: vk::ImageLayout::UNDEFINED,
            velocity_layout: vk::ImageLayout::UNDEFINED,
            depth_layout: vk::ImageLayout::UNDEFINED,
            _depth_pool: depth_pool,
            depth_set,
            ao: None,
            occlusion: None,
        })
    }
}

/// Набор с depth изображением вида (binding 0, DEPTH_STENCIL_READ_ONLY_OPTIMAL) для проходов частиц.
/// Пул принадлежит цели и освобождается вместе с ней.
pub(crate) fn allocate_depth_set(
    device: &ash::Device,
    layout: vk::DescriptorSetLayout,
    depth_view: vk::ImageView,
) -> Result<(OwnedDescriptorPool, vk::DescriptorSet), RenderError> {
    let pool_sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::SAMPLED_IMAGE)
        .descriptor_count(1)];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&pool_sizes);
    let pool = OwnedDescriptorPool::new(device, &pool_info)?;
    let layouts = [layout];
    let allocate_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool.handle)
        .set_layouts(&layouts);
    // SAFETY: the pool was sized for exactly one set of this layout.
    let set = unsafe { device.allocate_descriptor_sets(&allocate_info) }?
        .into_iter()
        .next()
        .ok_or(RenderError::InvalidState(
            "не создан набор depth для частиц",
        ))?;
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(depth_view)
        .image_layout(vk::ImageLayout::DEPTH_STENCIL_READ_ONLY_OPTIMAL)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(&image_info)];
    // SAFETY: the write targets the sampled-image binding of the freshly allocated set.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
    Ok((pool, set))
}

pub(crate) struct FrameTarget {
    pub(crate) _taa_pool: OwnedDescriptorPool,
    pub(crate) taa_set: vk::DescriptorSet,
    pub(crate) scene: OffscreenTarget,
    pub(crate) ldr: DeviceImage,
    pub(crate) ldr_layout: vk::ImageLayout,
    pub(crate) render_extent: vk::Extent2D,
    pub(crate) output_extent: vk::Extent2D,
    /// Цепочка bloom создаётся лениво, когда эффект запланирован для этого разрешения.
    pub(crate) bloom: Option<BloomChain>,
    pub(crate) exposure: ExposureTarget,
    /// Промежуточные буферы motion blur и DoF; создаются, когда фильтр запланирован.
    pub(crate) filter_scratch: Option<FilterScratch>,
    /// Экранные шейдеры до motion blur и DoF: ping-pong на физический размер входа стадии.
    pub(crate) screen_before: Option<ScreenShaderScratch>,
    /// Экранные шейдеры после composite: ping-pong на размер ldr.
    pub(crate) screen_after: Option<ScreenShaderScratch>,
}

impl FrameTarget {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
        taa_set_layout: vk::DescriptorSetLayout,
        particle_depth_layout: vk::DescriptorSetLayout,
        exposure_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let scene = OffscreenTarget::new(
            instance,
            device,
            physical_device,
            render_extent,
            particle_depth_layout,
        )?;
        let ldr = DeviceImage::new(
            instance,
            device,
            physical_device,
            output_extent,
            LDR_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let (taa_pool, taa_set) = allocate_taa_image_set(device, taa_set_layout)?;
        let exposure = ExposureTarget::new(device, exposure_layout)?;
        Ok(Self {
            _taa_pool: taa_pool,
            taa_set,
            bloom: None,
            exposure,
            filter_scratch: None,
            screen_before: None,
            screen_after: None,
            scene,
            ldr,
            ldr_layout: vk::ImageLayout::UNDEFINED,
            render_extent,
            output_extent,
        })
    }
}
