//! Внеэкранные viewport: цели отрисовки, изменение размера и освобождение.

use crate::*;

impl Renderer {
    /// Создаёт цель рендера сцены для UI. Изображения выделяются при первом кадре.
    pub fn create_viewport(&mut self, width: u32, height: u32) -> Result<ViewportId, RenderError> {
        validate_viewport_size(width, height, self.max_texture_size)?;
        self.viewports.push(Some(Viewport {
            extent: vk::Extent2D { width, height },
            targets: (0..FRAMES_IN_FLIGHT).map(|_| None).collect(),
            temporal: TemporalView::default(),
        }));
        Ok(ViewportId(self.viewports.len() - 1))
    }

    /// Запоминает размер в физических пикселях; ноль пропускает scene pass этого viewport.
    ///
    /// Изображения каждого кадрового слота заменяются при его следующем кадре после ожидания
    /// fence: они используются только своим слотом, поэтому ждать простоя GPU не нужно.
    pub fn resize_viewport(
        &mut self,
        viewport: ViewportId,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        validate_viewport_size(width, height, self.max_texture_size)?;
        self.viewports
            .get_mut(viewport.0)
            .and_then(Option::as_mut)
            .ok_or_else(unknown_viewport)?
            .extent = vk::Extent2D { width, height };
        Ok(())
    }

    /// Удаляет viewport; изображения освобождаются после завершения отправленных кадров.
    pub fn free_viewport(&mut self, viewport: ViewportId) -> Result<(), RenderError> {
        let resource = self
            .viewports
            .get_mut(viewport.0)
            .and_then(Option::take)
            .ok_or_else(unknown_viewport)?;
        for capture in &mut self.captures {
            if capture.viewport != Some(viewport) {
                continue;
            }
            match &mut capture.state {
                CaptureState::Queued => {
                    capture.state = CaptureState::Ready(Err(unknown_viewport()));
                }
                CaptureState::Submitted(submission) => {
                    submission.completion = CaptureCompletion::Failed("viewport was freed");
                }
                CaptureState::Ready(_) | CaptureState::Cancelled => {}
            }
        }
        self.retired
            .retire(RetiredResource::Viewport(Box::new(resource)), 0);
        Ok(())
    }
}

/// Общие объекты renderer, нужные каждой цели viewport.
pub(crate) struct ViewportTargetResources {
    pub(crate) image_set_layout: vk::DescriptorSetLayout,
    pub(crate) post_set_layout: vk::DescriptorSetLayout,
    pub(crate) exposure_set_layout: vk::DescriptorSetLayout,
    pub(crate) filter_set_layout: vk::DescriptorSetLayout,
    pub(crate) particle_depth_layout: vk::DescriptorSetLayout,
    pub(crate) taa_set_layout: vk::DescriptorSetLayout,
    pub(crate) image_sampler: vk::Sampler,
    pub(crate) post_sampler: vk::Sampler,
    pub(crate) frame_set_layout: vk::DescriptorSetLayout,
    pub(crate) shadow_view: vk::ImageView,
    pub(crate) local_shadow_view: vk::ImageView,
    pub(crate) shadow_sampler: vk::Sampler,
}

/// Uniform-буфер сцены одной цели: у каждой сцены кадра своя камера.
pub(crate) struct ViewportFrameSet {
    pub(crate) device: ash::Device,
    pub(crate) pool: vk::DescriptorPool,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) buffer: HostBuffer,
    pub(crate) local_lights: GrowingBuffer,
    pub(crate) local_shadow_matrices: HostBuffer,
    pub(crate) _cluster_buffer: HostBuffer,
    pub(crate) cluster_bounds: HostBuffer,
}

impl ViewportFrameSet {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        resources: &ViewportTargetResources,
    ) -> Result<Self, RenderError> {
        let buffer = HostBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            &[0; FRAME_UNIFORMS_SIZE],
        )?;
        let light_bytes = local_light_bytes(&[], IDENTITY_MATRIX)?;
        let light_buffer = HostBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &light_bytes,
        )?;
        let local_shadow_matrix_buffer = HostBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &local_shadow_matrix_bytes(&[])?,
        )?;
        let cluster_buffer = HostBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &vec![0; CLUSTER_BUFFER_SIZE],
        )?;
        let cluster_bounds = HostBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &vec![0; CLUSTER_BOUNDS_BUFFER_SIZE],
        )?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(3),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(4),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        // SAFETY: the pool holds exactly one frame set and its light storage buffer.
        let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;
        let layouts = [resources.frame_set_layout];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for this set and the layout is alive.
        let sets = unsafe { device.allocate_descriptor_sets(&allocate_info) };
        let set = match sets {
            Ok(sets) => sets[0],
            Err(error) => {
                // SAFETY: no set was allocated from the new pool and no command buffer uses it.
                unsafe { device.destroy_descriptor_pool(pool, None) };
                return Err(error.into());
            }
        };
        write_frame_set(
            device,
            set,
            &buffer,
            &light_buffer,
            &local_shadow_matrix_buffer,
            &cluster_buffer,
            &cluster_bounds,
            resources.shadow_view,
            resources.local_shadow_view,
            resources.shadow_sampler,
        );
        Ok(Self {
            device: device.clone(),
            pool,
            set,
            buffer,
            local_lights: GrowingBuffer {
                buffer: Some(light_buffer),
                capacity: light_bytes.len(),
            },
            local_shadow_matrices: local_shadow_matrix_buffer,
            _cluster_buffer: cluster_buffer,
            cluster_bounds,
        })
    }

    /// Записывает frame uniform и локальные lights после ожидания fence viewport-слота.
    #[allow(clippy::too_many_arguments)] // Mirrors the frame buffers uploaded to this viewport set.
    pub(crate) fn write(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        uniforms: &[u8],
        light_bytes: &[u8],
        local_shadow_matrix_bytes: &[u8; LOCAL_SHADOW_MATRIX_BUFFER_SIZE],
        cluster_bounds: &[ClusterBounds],
    ) -> Result<vk::DescriptorSet, RenderError> {
        self.buffer.write(uniforms)?;
        self.local_shadow_matrices
            .write(local_shadow_matrix_bytes)?;
        let replaced = self.local_lights.write_keep_old(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            light_bytes,
        )?;
        if replaced.is_some() {
            let buffer = self
                .local_lights
                .buffer
                .as_ref()
                .ok_or(RenderError::InvalidState(
                    "viewport local light buffer отсутствует",
                ))?;
            write_local_light_descriptor(device, self.set, buffer, self.local_lights.capacity);
        }
        drop(replaced);
        self.cluster_bounds
            .write(cluster_bounds_bytes(cluster_bounds))?;
        Ok(self.set)
    }
}

impl Drop for ViewportFrameSet {
    fn drop(&mut self) {
        // SAFETY: the set is dropped only after fences of the submissions using it were waited.
        unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
    }
}

/// Цель рендера сцены одного кадрового слота, которую UI читает как текстуру.
pub(crate) struct ViewportTarget {
    pub(crate) device: ash::Device,
    pub(crate) ui_pool: vk::DescriptorPool,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) _taa_pool: OwnedDescriptorPool,
    pub(crate) taa_set: vk::DescriptorSet,
    pub(crate) post_pool: vk::DescriptorPool,
    pub(crate) tonemap_set: vk::DescriptorSet,
    pub(crate) output_set: vk::DescriptorSet,
    pub(crate) extent: vk::Extent2D,
    pub(crate) render_extent: vk::Extent2D,
    pub(crate) scene: OffscreenTarget,
    pub(crate) ldr: DeviceImage,
    pub(crate) ldr_layout: vk::ImageLayout,
    pub(crate) output: DeviceImage,
    pub(crate) output_layout: vk::ImageLayout,
    pub(crate) frame_set: ViewportFrameSet,
    /// Цепочка bloom создаётся лениво, как у цели экрана.
    pub(crate) bloom: Option<BloomChain>,
    pub(crate) exposure: ExposureTarget,
    /// Промежуточные буферы motion blur и DoF; создаются, когда фильтр запланирован.
    pub(crate) filter_scratch: Option<FilterScratch>,
    /// Экранные шейдеры до motion blur и DoF; см. [`FrameTarget::screen_before`].
    pub(crate) screen_before: Option<ScreenShaderScratch>,
    /// Экранные шейдеры после composite; см. [`FrameTarget::screen_after`].
    pub(crate) screen_after: Option<ScreenShaderScratch>,
}

impl ViewportTarget {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        render_extent: vk::Extent2D,
        resources: &ViewportTargetResources,
    ) -> Result<Self, RenderError> {
        let scene = OffscreenTarget::new(
            instance,
            device,
            physical_device,
            render_extent,
            resources.particle_depth_layout,
        )?;
        let ldr = DeviceImage::new(
            instance,
            device,
            physical_device,
            extent,
            LDR_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let output = DeviceImage::new(
            instance,
            device,
            physical_device,
            extent,
            LDR_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC,
            color_subresource_range(),
        )?;
        let exposure = ExposureTarget::new(device, resources.exposure_set_layout)?;
        let frame_set = ViewportFrameSet::new(instance, device, physical_device, resources)?;
        let (taa_pool, taa_set) = allocate_taa_image_set(device, resources.taa_set_layout)?;
        let (post_pool, post_sets) = allocate_post_image_sets(
            device,
            resources.post_set_layout,
            resources.post_sampler,
            [scene.color.view, ldr.view],
        )?;
        let (ui_pool, set) = match allocate_image_set(
            device,
            resources.image_set_layout,
            output.view,
            resources.image_sampler,
        ) {
            Ok(set) => set,
            Err(error) => {
                // SAFETY: no command buffer references the new target during construction.
                unsafe { device.destroy_descriptor_pool(post_pool, None) };
                return Err(error);
            }
        };
        Ok(Self {
            device: device.clone(),
            bloom: None,
            exposure,
            filter_scratch: None,
            screen_before: None,
            screen_after: None,
            ui_pool,
            set,
            _taa_pool: taa_pool,
            taa_set,
            post_pool,
            tonemap_set: post_sets[0],
            output_set: post_sets[1],
            extent,
            render_extent,
            scene,
            ldr,
            ldr_layout: vk::ImageLayout::UNDEFINED,
            output,
            output_layout: vk::ImageLayout::UNDEFINED,
            frame_set,
        })
    }
}

impl Drop for ViewportTarget {
    fn drop(&mut self) {
        // SAFETY: the target is dropped only after fences of the submissions using it were waited.
        unsafe {
            self.device.destroy_descriptor_pool(self.ui_pool, None);
            self.device.destroy_descriptor_pool(self.post_pool, None);
        };
    }
}

/// Запрошенный размер viewport и его цели по кадровым слотам.
pub(crate) struct Viewport {
    pub(crate) extent: vk::Extent2D,
    pub(crate) targets: Vec<Option<ViewportTarget>>,
    pub(crate) temporal: TemporalView,
}

pub(crate) fn unknown_viewport() -> RenderError {
    RenderError::InvalidTexture("ViewportId не принадлежит renderer".to_owned())
}
