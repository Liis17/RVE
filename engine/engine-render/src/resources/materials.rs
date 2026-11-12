//! GPU-материалы, surface-шейдеры и их uniform-данные.

use crate::*;

impl Renderer {
    pub(crate) fn upload_material(&self, material: &Material) -> Result<GpuMaterial, RenderError> {
        let device = self.device()?;
        let shader_properties = material
            .shader
            .and_then(|shader| self.surface_shaders.get(shader.0))
            .and_then(Option::as_ref)
            .map_or(&[][..], |shader| shader.properties.as_slice());
        let uniform_buffer = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            &material_uniform_bytes(material),
        )?;
        let user_uniform_buffer = match &self.builtin_user_uniform {
            Some(shared) if uses_shared_user_uniform(material) => Arc::clone(shared),
            _ => Arc::new(HostBuffer::new(
                self.instance()?,
                device,
                self.physical_device,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                &user_material_uniform_bytes(material),
            )?),
        };
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(2),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(13),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(1),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        // SAFETY: the pool holds two material uniforms, thirteen images and one sampler.
        let pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;
        let result = (|| {
            let layouts = [self.material_set_layout()?];
            let allocate_info = vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(pool)
                .set_layouts(&layouts);
            // SAFETY: the pool was sized for one material set of this layout.
            let set = unsafe { device.allocate_descriptor_sets(&allocate_info) }?
                .into_iter()
                .next()
                .ok_or(RenderError::InvalidState(
                    "не создан material descriptor set",
                ))?;

            let texture_for = |texture: Option<TextureId>, fallback: usize| {
                self.textures
                    .get(texture.map_or(fallback, |texture| texture.0))
                    .and_then(Option::as_ref)
                    .cloned()
                    .ok_or(RenderError::InvalidState("material texture отсутствует"))
            };
            let texture_refs = [
                texture_for(material.base_color_texture, WHITE_SRGB_TEXTURE)?,
                texture_for(material.metallic_roughness_texture, WHITE_LINEAR_TEXTURE)?,
                texture_for(material.normal_texture, FLAT_NORMAL_TEXTURE)?,
                texture_for(material.emissive_texture, WHITE_SRGB_TEXTURE)?,
                texture_for(material.occlusion_texture, WHITE_LINEAR_TEXTURE)?,
            ];
            let user_texture_refs: [Arc<Texture>; 8] = (0..8)
                .map(|slot| {
                    let property = shader_properties.iter().find(|property| {
                        property.kind == SurfacePropertyKind::Texture && property.slot == slot
                    });
                    let fallback = match property.map(|property| property.fallback) {
                        Some(SurfaceTextureFallback::Black) => BLACK_TEXTURE,
                        Some(SurfaceTextureFallback::Normal) => FLAT_NORMAL_TEXTURE,
                        Some(SurfaceTextureFallback::White) => property
                            .and_then(|property| property.color_space)
                            .map_or(WHITE_LINEAR_TEXTURE, |space| match space {
                                TextureColorSpace::Srgb => WHITE_SRGB_TEXTURE,
                                TextureColorSpace::Linear => WHITE_LINEAR_TEXTURE,
                            }),
                        None => WHITE_LINEAR_TEXTURE,
                    };
                    self.textures
                        .get(material.user_textures[slot].map_or(fallback, |texture| texture.0))
                        .and_then(Option::as_ref)
                        .cloned()
                        .ok_or(RenderError::InvalidState(
                            "user material texture отсутствует",
                        ))
                })
                .collect::<Result<Vec<_>, _>>()?
                .try_into()
                .map_err(|_| RenderError::InvalidState("неверное число user textures"))?;
            let image_infos: [vk::DescriptorImageInfo; 5] = std::array::from_fn(|index| {
                vk::DescriptorImageInfo::default()
                    .image_view(texture_refs[index].image.view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            });
            let user_image_infos: [vk::DescriptorImageInfo; 8] = std::array::from_fn(|index| {
                vk::DescriptorImageInfo::default()
                    .image_view(user_texture_refs[index].image.view)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            });
            let sampler = self
                .sampler
                .ok_or(RenderError::InvalidState("sampler отсутствует"))?;
            let sampler_info = [vk::DescriptorImageInfo::default().sampler(sampler)];
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(uniform_buffer.buffer)
                .offset(0)
                .range(MATERIAL_UNIFORMS_SIZE as vk::DeviceSize)];
            let user_buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(user_uniform_buffer.buffer)
                .offset(0)
                .range(USER_MATERIAL_UNIFORMS_SIZE as vk::DeviceSize)];
            let mut writes = vec![
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buffer_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(7)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&user_buffer_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&sampler_info),
            ];
            for index in 0..5 {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(index as u32 + 1)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&image_infos[index..index + 1]),
                );
            }
            for index in 0..8 {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(index as u32 + 8)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(&user_image_infos[index..index + 1]),
                );
            }
            // SAFETY: texture_refs keep every image alive until this material descriptor pool drops.
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            Ok(GpuMaterial {
                device: device.clone(),
                pool,
                set,
                double_sided: material.double_sided,
                _uniform_buffer: uniform_buffer,
                _user_uniform_buffer: user_uniform_buffer,
                _textures: texture_refs,
                _user_textures: user_texture_refs,
                alpha_mode: material.alpha_mode,
                shader: material.shader,
            })
        })();
        if result.is_err() {
            // SAFETY: allocation failed before the descriptor pool was published or used.
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        result
    }
}

/// Uniform-буфер освещения и его набор дескрипторов для одного слота кадра.
pub(crate) struct FrameUniforms {
    pub(crate) buffer: HostBuffer,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) local_lights: GrowingBuffer,
    pub(crate) local_shadow_matrices: HostBuffer,
    pub(crate) _cluster_buffer: HostBuffer,
    pub(crate) cluster_bounds: HostBuffer,
}

/// Текстура в device-local памяти; материал выбирает её цветовое пространство.
pub(crate) struct Texture {
    pub(crate) image: DeviceImage,
    pub(crate) color_space: TextureColorSpace,
    pub(crate) upload_serial: u64,
}

/// Материал владеет uniform buffer и descriptor pool для собственного набора.
pub(crate) struct GpuMaterial {
    pub(crate) device: ash::Device,
    pub(crate) pool: vk::DescriptorPool,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) double_sided: bool,
    pub(crate) _uniform_buffer: HostBuffer,
    pub(crate) _user_uniform_buffer: Arc<HostBuffer>,
    pub(crate) _textures: [Arc<Texture>; 5],
    pub(crate) _user_textures: [Arc<Texture>; 8],
    pub(crate) alpha_mode: AlphaMode,
    pub(crate) shader: Option<ShaderId>,
}

impl Drop for GpuMaterial {
    fn drop(&mut self) {
        if self.pool != vk::DescriptorPool::null() {
            // SAFETY: retirement waits for submitted frames; this frees the set before owned texture refs.
            unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
        }
    }
}

pub(crate) struct GpuSurfaceShader {
    pub(crate) device: ash::Device,
    pub(crate) pipelines: SurfacePipelines,
    pub(crate) properties: Vec<SurfaceProperty>,
    pub(crate) references: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct SurfacePipelines {
    pub(crate) opaque: vk::Pipeline,
    pub(crate) opaque_equal: vk::Pipeline,
    pub(crate) mask: vk::Pipeline,
    pub(crate) blend: vk::Pipeline,
}

impl Drop for GpuSurfaceShader {
    fn drop(&mut self) {
        for pipeline in [
            self.pipelines.opaque,
            self.pipelines.opaque_equal,
            self.pipelines.mask,
            self.pipelines.blend,
        ] {
            if pipeline != vk::Pipeline::null() {
                // SAFETY: a surface shader is dropped only after its frame uses are retired.
                unsafe { self.device.destroy_pipeline(pipeline, None) };
            }
        }
    }
}

pub(crate) fn material_uniform_bytes(material: &Material) -> [u8; MATERIAL_UNIFORMS_SIZE] {
    let (alpha_cutoff, alpha_mode) = match material.alpha_mode {
        AlphaMode::Opaque => (0.0, 0.0),
        AlphaMode::Mask { cutoff } => (cutoff, 1.0),
        AlphaMode::Blend => (0.0, 2.0),
    };
    float_bytes(&[
        material.base_color[0],
        material.base_color[1],
        material.base_color[2],
        1.0,
        material.metallic,
        material.roughness,
        material.normal_scale,
        if material.normal_texture.is_some() {
            1.0
        } else {
            0.0
        },
        material.emissive[0],
        material.emissive[1],
        material.emissive[2],
        0.0,
        material.opacity,
        alpha_cutoff,
        alpha_mode,
        material.occlusion_strength,
    ])
}

/// Материал без шейдера с нулевыми user-значениями читает общий нулевой буфер вместо собственного.
pub(crate) fn uses_shared_user_uniform(material: &Material) -> bool {
    material.shader.is_none() && material.user_values == [[0.0; 4]; 16]
}

pub(crate) fn user_material_uniform_bytes(
    material: &Material,
) -> [u8; USER_MATERIAL_UNIFORMS_SIZE] {
    let mut bytes = [0; USER_MATERIAL_UNIFORMS_SIZE];
    for (index, value) in material.user_values.iter().flatten().enumerate() {
        bytes[index * size_of::<f32>()..(index + 1) * size_of::<f32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

pub(crate) fn spirv_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}
