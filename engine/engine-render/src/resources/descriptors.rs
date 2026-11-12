//! Descriptor set layouts, пулы и запись наборов дескрипторов.

use crate::*;

impl Renderer {
    /// Создаёт общие layout-ы и per-frame uniform-буферы с дескрипторами shadow map.
    pub(crate) fn create_descriptor_resources(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let frame_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(
                    vk::ShaderStageFlags::VERTEX
                        | vk::ShaderStageFlags::FRAGMENT
                        | vk::ShaderStageFlags::COMPUTE,
                ),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT | vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT | vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(6)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(7)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(8)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let frame_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&frame_bindings);
        // SAFETY: bindings match frame, clustered-light, and local-shadow shaders.
        self.frame_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&frame_layout_info, None) }?);
        let material_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(6)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(7)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(8)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(9)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(10)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(11)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(12)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(13)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(14)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(15)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let material_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&material_bindings);
        // SAFETY: the bindings match the fragment material uniform, five maps and shared sampler.
        self.material_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&material_layout_info, None) }?);

        let environment_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(5)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let environment_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&environment_bindings);
        // SAFETY: the bindings match the split-sum IBL and skybox fragment shaders.
        self.environment_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&environment_layout_info, None) }?);

        let texture_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let texture_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&texture_bindings);
        // SAFETY: UI and viewport shaders use one sampled image and one sampler.
        self.texture_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&texture_layout_info, None) }?);

        let post_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
        ];
        let post_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&post_bindings);
        // SAFETY: composite, output and fullscreen post shaders read bindings 0, 1 and
        // (composite) 2-4: bloom, exposure buffer and 3D LUT.
        self.post_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&post_layout_info, None) }?);

        let taa_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
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
        let taa_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&taa_bindings);
        // SAFETY: the layout matches the TAA shader's four images and shared sampler.
        self.taa_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&taa_layout_info, None) }?);

        let sampler_info = material_sampler_info(self.texture_sampler_anisotropy);
        // SAFETY: sampler anisotropy is enabled only when the selected device supports it.
        self.sampler = Some(unsafe { device.create_sampler(&sampler_info, None) }?);
        let post_sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        // SAFETY: the post sampler uses core linear filtering and clamp-to-edge addressing.
        self.post_sampler = Some(unsafe { device.create_sampler(&post_sampler_info, None) }?);
        let builtin_user_uniform = HostBuffer::new(
            self.instance()?,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            &user_material_uniform_bytes(&Material::default()),
        )?;
        self.builtin_user_uniform = Some(Arc::new(builtin_user_uniform));

        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let shadow_view = shadow_image.view;
        let local_shadow_view = self
            .local_shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?
            .view;
        let shadow_sampler = self
            .shadow_sampler
            .ok_or(RenderError::InvalidState("shadow sampler отсутствует"))?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(self.frames.len() as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count((self.frames.len() * 3) as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(self.frames.len() as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count((self.frames.len() * 4) as u32),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(self.frames.len() as u32)
            .pool_sizes(&pool_sizes);
        // SAFETY: each set uses one uniform, two images, one sampler, and four storage buffers.
        let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;
        self.frame_descriptor_pool = Some(pool);
        let layouts = vec![self.frame_set_layout()?; self.frames.len()];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for these sets and the layout is alive.
        let sets = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
        for set in sets {
            let buffer = HostBuffer::new(
                self.instance()?,
                &device,
                self.physical_device,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                &[0; FRAME_UNIFORMS_SIZE],
            )?;
            let light_bytes = local_light_bytes(&[], IDENTITY_MATRIX)?;
            let light_buffer = HostBuffer::new(
                self.instance()?,
                &device,
                self.physical_device,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                &light_bytes,
            )?;
            let local_shadow_matrix_buffer = HostBuffer::new(
                self.instance()?,
                &device,
                self.physical_device,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                &local_shadow_matrix_bytes(&[])?,
            )?;
            let cluster_buffer = HostBuffer::new(
                self.instance()?,
                &device,
                self.physical_device,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                &vec![0; CLUSTER_BUFFER_SIZE],
            )?;
            let cluster_bounds = HostBuffer::new(
                self.instance()?,
                &device,
                self.physical_device,
                vk::BufferUsageFlags::STORAGE_BUFFER,
                &vec![0; CLUSTER_BOUNDS_BUFFER_SIZE],
            )?;
            write_frame_set(
                &device,
                set,
                &buffer,
                &light_buffer,
                &local_shadow_matrix_buffer,
                &cluster_buffer,
                &cluster_bounds,
                shadow_view,
                local_shadow_view,
                shadow_sampler,
            );
            self.frame_uniforms.push(FrameUniforms {
                buffer,
                set,
                local_lights: GrowingBuffer {
                    buffer: Some(light_buffer),
                    capacity: light_bytes.len(),
                },
                local_shadow_matrices: local_shadow_matrix_buffer,
                _cluster_buffer: cluster_buffer,
                cluster_bounds,
            });
        }

        let post_set_count = (self.frames.len() * 2) as u32;
        let post_pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(post_set_count * 3),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(post_set_count),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(post_set_count),
        ];
        let post_pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(post_set_count)
            .pool_sizes(&post_pool_sizes);
        // SAFETY: each frame slot owns a tonemap and output image set.
        let post_pool = unsafe { device.create_descriptor_pool(&post_pool_info, None) }?;
        self.post_descriptor_pool = Some(post_pool);
        let post_layouts = vec![self.post_set_layout()?; post_set_count as usize];
        let post_allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(post_pool)
            .set_layouts(&post_layouts);
        // SAFETY: the pool was sized for these image and sampler sets.
        let post_sets = unsafe { device.allocate_descriptor_sets(&post_allocate_info) }?;
        let (post_sets, _) = post_sets.as_chunks::<2>();
        for (frame, sets) in self.frames.iter_mut().zip(post_sets) {
            frame.tonemap_set = sets[0];
            frame.output_set = sets[1];
        }
        Ok(())
    }
}

/// Заполняет набор дескрипторов кадра: uniform, shadow map и локальные источники.
#[allow(clippy::too_many_arguments)]
/// Привязывает экранный AO цели к binding 8 набора кадра; вызывается каждый кадр до записи.
pub(crate) fn write_frame_ao(device: &ash::Device, set: vk::DescriptorSet, view: vk::ImageView) {
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(8)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(&image_info)];
    // SAFETY: the set and view are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_frame_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    buffer: &HostBuffer,
    local_lights: &HostBuffer,
    local_shadow_matrices: &HostBuffer,
    clusters: &HostBuffer,
    cluster_bounds: &HostBuffer,
    shadow_view: vk::ImageView,
    local_shadow_view: vk::ImageView,
    shadow_sampler: vk::Sampler,
) {
    let buffer_info = [vk::DescriptorBufferInfo::default()
        .buffer(buffer.buffer)
        .offset(0)
        .range(FRAME_UNIFORMS_SIZE as vk::DeviceSize)];
    let shadow_image_info = [vk::DescriptorImageInfo::default()
        .image_view(shadow_view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let shadow_sampler_info = [vk::DescriptorImageInfo::default().sampler(shadow_sampler)];
    let local_light_info = [vk::DescriptorBufferInfo::default()
        .buffer(local_lights.buffer)
        .offset(0)
        .range(LOCAL_LIGHT_GPU_SIZE as vk::DeviceSize)];
    let local_shadow_matrix_info = [vk::DescriptorBufferInfo::default()
        .buffer(local_shadow_matrices.buffer)
        .offset(0)
        .range(LOCAL_SHADOW_MATRIX_BUFFER_SIZE as vk::DeviceSize)];
    let cluster_info = [vk::DescriptorBufferInfo::default()
        .buffer(clusters.buffer)
        .offset(0)
        .range(CLUSTER_BUFFER_SIZE as vk::DeviceSize)];
    let cluster_bounds_info = [vk::DescriptorBufferInfo::default()
        .buffer(cluster_bounds.buffer)
        .offset(0)
        .range(CLUSTER_BOUNDS_BUFFER_SIZE as vk::DeviceSize)];
    let local_shadow_image_info = [vk::DescriptorImageInfo::default()
        .image_view(local_shadow_view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .buffer_info(&buffer_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&shadow_image_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(2)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .image_info(&shadow_sampler_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(3)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&local_light_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(4)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&cluster_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(5)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&cluster_bounds_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(6)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&local_shadow_matrix_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(7)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&local_shadow_image_info),
    ];
    // SAFETY: all resources are alive and no command buffer uses the set yet.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

pub(crate) fn write_local_light_descriptor(
    device: &ash::Device,
    set: vk::DescriptorSet,
    buffer: &HostBuffer,
    range: usize,
) {
    let info = [vk::DescriptorBufferInfo::default()
        .buffer(buffer.buffer)
        .offset(0)
        .range(range as vk::DeviceSize)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(3)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .buffer_info(&info)];
    // SAFETY: the owning slot fence has completed and both old/new buffers remain alive through update.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

pub(crate) struct OwnedDescriptorPool {
    pub(crate) device: ash::Device,
    pub(crate) handle: vk::DescriptorPool,
}

impl OwnedDescriptorPool {
    pub(crate) fn new(
        device: &ash::Device,
        info: &vk::DescriptorPoolCreateInfo<'_>,
    ) -> Result<Self, RenderError> {
        // SAFETY: caller sizes the pool for the descriptor sets it will allocate.
        let handle = unsafe { device.create_descriptor_pool(info, None) }?;
        Ok(Self {
            device: device.clone(),
            handle,
        })
    }
}

impl Drop for OwnedDescriptorPool {
    fn drop(&mut self) {
        if self.handle != vk::DescriptorPool::null() {
            // SAFETY: the owner is retired only after frames stop using its descriptor set.
            unsafe { self.device.destroy_descriptor_pool(self.handle, None) };
        }
    }
}

/// Создаёт пул на один набор и записывает в него изображение и sampler.
pub(crate) fn allocate_image_set(
    device: &ash::Device,
    layout: vk::DescriptorSetLayout,
    view: vk::ImageView,
    sampler: vk::Sampler,
) -> Result<(vk::DescriptorPool, vk::DescriptorSet), RenderError> {
    let pool_sizes = [
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
    // SAFETY: the pool holds the single descriptor set of this image.
    let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;
    let layouts = [layout];
    let allocate_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    // SAFETY: the pool was sized for one set of this layout.
    let set = match unsafe { device.allocate_descriptor_sets(&allocate_info) } {
        Ok(sets) => sets[0],
        Err(error) => {
            // SAFETY: the pool has no allocated sets and is not in use.
            unsafe { device.destroy_descriptor_pool(pool, None) };
            return Err(RenderError::Vulkan(error));
        }
    };
    write_image_set(device, set, view, sampler);
    Ok((pool, set))
}

pub(crate) fn allocate_post_image_sets(
    device: &ash::Device,
    layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    views: [vk::ImageView; 2],
) -> Result<(vk::DescriptorPool, [vk::DescriptorSet; 2]), RenderError> {
    let pool_sizes = [
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(6),
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::SAMPLER)
            .descriptor_count(2),
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(2),
    ];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(2)
        .pool_sizes(&pool_sizes);
    // SAFETY: the pool holds one tonemap and one output descriptor set (three sampled images each).
    let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;
    let layouts = [layout; 2];
    let allocate_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool)
        .set_layouts(&layouts);
    // SAFETY: the pool was sized for two sets of this layout.
    let sets = match unsafe { device.allocate_descriptor_sets(&allocate_info) } {
        Ok(sets) => sets,
        Err(error) => {
            // SAFETY: no set was allocated from this new pool.
            unsafe { device.destroy_descriptor_pool(pool, None) };
            return Err(RenderError::Vulkan(error));
        }
    };
    let sets: [vk::DescriptorSet; 2] = match sets.try_into() {
        Ok(sets) => sets,
        Err(_) => {
            // SAFETY: no command buffer references the newly allocated sets.
            unsafe { device.destroy_descriptor_pool(pool, None) };
            return Err(RenderError::InvalidState(
                "неверное число post descriptor sets",
            ));
        }
    };
    write_image_set(device, sets[0], views[0], sampler);
    write_image_set(device, sets[1], views[1], sampler);
    Ok((pool, sets))
}

pub(crate) fn write_image_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    view: vk::ImageView,
    sampler: vk::Sampler,
) {
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let sampler_info = [vk::DescriptorImageInfo::default().sampler(sampler)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&image_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .image_info(&sampler_info),
    ];
    // SAFETY: the set, image view and sampler are alive and the set is not in use.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

pub(crate) fn allocate_taa_image_set(
    device: &ash::Device,
    layout: vk::DescriptorSetLayout,
) -> Result<(OwnedDescriptorPool, vk::DescriptorSet), RenderError> {
    let pool_sizes = [
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(4),
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::SAMPLER)
            .descriptor_count(1),
    ];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&pool_sizes);
    let pool = OwnedDescriptorPool::new(device, &pool_info)?;
    let layouts = [layout];
    let allocate_info = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(pool.handle)
        .set_layouts(&layouts);
    // SAFETY: the pool and layout are sized for the one TAA descriptor set.
    let sets = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
    Ok((pool, sets[0]))
}

pub(crate) fn write_taa_image_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    views: [vk::ImageView; 4],
) {
    let image_infos = views.map(|view| {
        vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
    });
    let sampler_info = [vk::DescriptorImageInfo::default().sampler(sampler)];
    let image_writes = std::array::from_fn::<_, 4, _>(|index| {
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(index as u32)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(std::slice::from_ref(&image_infos[index]))
    });
    let sampler_write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(4)
        .descriptor_type(vk::DescriptorType::SAMPLER)
        .image_info(&sampler_info);
    let mut writes = image_writes.to_vec();
    writes.push(sampler_write);
    // SAFETY: the owning frame-slot fence has completed before these descriptors are updated.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}
