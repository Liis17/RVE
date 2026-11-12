//! GPU-окружение: cubemap, irradiance и prefilter из HDR или процедурного неба.

use crate::*;

impl Renderer {
    pub(crate) fn create_environment_resources(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let compute_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(3)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let compute_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&compute_bindings);
        // SAFETY: binding types and stages match all four environment compute shaders.
        self.compute_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&compute_layout_info, None) }?);
        let compute_set_layout = self.compute_set_layout.ok_or(RenderError::InvalidState(
            "compute descriptor layout отсутствует",
        ))?;
        let compute_layouts = [compute_set_layout];
        let compute_pipeline_layout_info =
            vk::PipelineLayoutCreateInfo::default().set_layouts(&compute_layouts);
        // SAFETY: the compute pipeline layout contains the only layout used by bake shaders.
        self.compute_pipeline_layout =
            Some(unsafe { device.create_pipeline_layout(&compute_pipeline_layout_info, None) }?);
        let compute_pipeline_layout =
            self.compute_pipeline_layout
                .ok_or(RenderError::InvalidState(
                    "compute pipeline layout отсутствует",
                ))?;
        self.equirect_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_pipeline_layout,
            EQUIRECT_TO_CUBE_SHADER,
        )?);
        self.procedural_sky_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_pipeline_layout,
            PROCEDURAL_SKY_SHADER,
        )?);
        self.irradiance_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_pipeline_layout,
            IRRADIANCE_SHADER,
        )?);
        self.prefilter_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            compute_pipeline_layout,
            PREFILTER_SHADER,
        )?);

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .min_lod(0.0)
            .max_lod(9.0);
        // SAFETY: cube sampling uses supported linear filtering and clamp-to-edge addressing.
        self.environment_sampler = Some(unsafe { device.create_sampler(&sampler_info, None) }?);
        let equirect_sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::REPEAT)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        // SAFETY: longitude wraps at the equirectangular seam while latitude clamps at its poles.
        self.equirect_sampler =
            Some(unsafe { device.create_sampler(&equirect_sampler_info, None) }?);

        let lut = environment::integrate_brdf_lut(64, 256);
        let lut_bytes = lut
            .iter()
            .flat_map(|value| {
                [
                    environment::float_to_half(value[0]),
                    environment::float_to_half(value[1]),
                ]
            })
            .flat_map(u16::to_ne_bytes)
            .collect::<Vec<_>>();
        self.brdf_image =
            Some(self.upload_16bit_image(64, 64, vk::Format::R16G16_SFLOAT, 4, &lut_bytes)?);
        Ok(())
    }

    pub(crate) fn create_default_environment(&mut self) -> Result<(), RenderError> {
        let environment_map = self.upload_cube_constant([0x3c00, 0x3c00, 0x3c00, 0x3c00])?;
        let irradiance_map = self.upload_cube_constant([
            environment::float_to_half(std::f32::consts::PI),
            environment::float_to_half(std::f32::consts::PI),
            environment::float_to_half(std::f32::consts::PI),
            0x3c00,
        ])?;
        let prefiltered_map = self.upload_cube_constant([0x3c00, 0x3c00, 0x3c00, 0x3c00])?;
        let default =
            self.make_gpu_environment(environment_map, irradiance_map, prefiltered_map)?;
        self.environments.push(Some(default));
        Ok(())
    }

    pub(crate) fn make_gpu_environment(
        &self,
        environment_map: DeviceImage,
        irradiance_map: DeviceImage,
        prefiltered_map: DeviceImage,
    ) -> Result<GpuEnvironment, RenderError> {
        let device = self.device()?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(4),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(2),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let layouts = [self
            .environment_set_layout
            .ok_or(RenderError::InvalidState(
                "environment descriptor layout отсутствует",
            ))?];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool has room for one environment descriptor set.
        let set = unsafe { device.allocate_descriptor_sets(&allocate_info) }?[0];
        let brdf = self
            .brdf_image
            .as_ref()
            .ok_or(RenderError::InvalidState("BRDF LUT отсутствует"))?;
        let images = [
            vk::DescriptorImageInfo::default()
                .image_view(environment_map.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            vk::DescriptorImageInfo::default()
                .image_view(irradiance_map.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            vk::DescriptorImageInfo::default()
                .image_view(prefiltered_map.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            vk::DescriptorImageInfo::default()
                .image_view(brdf.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        ];
        let cube_sampler = [vk::DescriptorImageInfo::default().sampler(
            self.environment_sampler
                .ok_or(RenderError::InvalidState("environment sampler отсутствует"))?,
        )];
        let brdf_sampler = cube_sampler;
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&images[0..1]),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&cube_sampler),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&images[1..2]),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&images[2..3]),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&images[3..4]),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(5)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&brdf_sampler),
        ];
        // SAFETY: all views, samplers, descriptor pool and layout remain owned by this renderer.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(GpuEnvironment {
            _descriptor_pool: descriptor_pool,
            set,
            _environment_map: environment_map,
            _irradiance_map: irradiance_map,
            _prefiltered_map: prefiltered_map,
        })
    }

    pub(crate) fn create_gpu_environment(
        &self,
        source: &EnvironmentSource,
    ) -> Result<GpuEnvironment, RenderError> {
        const ENVIRONMENT_SIZE: u32 = 512;
        const ENVIRONMENT_MIPS: u32 = 10;
        const IRRADIANCE_SIZE: u32 = 32;
        const PREFILTER_SIZE: u32 = 128;
        const PREFILTER_MIPS: u32 = 6;
        const DISPATCH_COUNT: u32 = ENVIRONMENT_MIPS + 1 + PREFILTER_MIPS;

        let source_image = match source {
            EnvironmentSource::Equirect { width, height, rgb } => {
                let mut rgba = Vec::with_capacity(rgb.len() / 3 * 8);
                for pixel in rgb.as_chunks::<3>().0 {
                    for channel in pixel {
                        rgba.extend_from_slice(
                            &environment::float_to_half(channel.min(65_504.0)).to_ne_bytes(),
                        );
                    }
                    rgba.extend_from_slice(&0x3c00_u16.to_ne_bytes());
                }
                Some(self.upload_16bit_image(*width, *height, OFFSCREEN_FORMAT, 8, &rgba)?)
            }
            EnvironmentSource::Procedural(_) => None,
        };

        let cube_usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED;
        let environment_map = DeviceImage::new_cube(
            self.instance()?,
            self.device()?,
            self.physical_device,
            ENVIRONMENT_SIZE,
            OFFSCREEN_FORMAT,
            cube_usage,
            ENVIRONMENT_MIPS,
        )?;
        let irradiance_map = DeviceImage::new_cube(
            self.instance()?,
            self.device()?,
            self.physical_device,
            IRRADIANCE_SIZE,
            OFFSCREEN_FORMAT,
            cube_usage,
            1,
        )?;
        let prefiltered_map = DeviceImage::new_cube(
            self.instance()?,
            self.device()?,
            self.physical_device,
            PREFILTER_SIZE,
            OFFSCREEN_FORMAT,
            cube_usage,
            PREFILTER_MIPS,
        )?;

        let environment_sampler = self
            .environment_sampler
            .ok_or(RenderError::InvalidState("environment sampler отсутствует"))?;
        let (environment_pipeline, environment_source_view, environment_source_sampler, sky_params) =
            match source {
                EnvironmentSource::Equirect { .. } => (
                    self.equirect_pipeline
                        .ok_or(RenderError::InvalidState("equirect pipeline отсутствует"))?,
                    source_image.as_ref().map(|image| image.view),
                    self.equirect_sampler,
                    None,
                ),
                EnvironmentSource::Procedural(sky) => {
                    let length = sky
                        .sun_direction
                        .iter()
                        .map(|value| value * value)
                        .sum::<f32>()
                        .sqrt();
                    let mut parameters = [0.0_f32; 20];
                    parameters[1] = sky.sun_size_deg;
                    parameters[4..7].copy_from_slice(&[
                        sky.sun_direction[0] / length,
                        sky.sun_direction[1] / length,
                        sky.sun_direction[2] / length,
                    ]);
                    parameters[7] = sky.sun_intensity.min(65_504.0);
                    parameters[8..11].copy_from_slice(&sky.zenith_color.map(|v| v.min(65_504.0)));
                    parameters[12..15].copy_from_slice(&sky.horizon_color.map(|v| v.min(65_504.0)));
                    parameters[16..19].copy_from_slice(&sky.ground_color.map(|v| v.min(65_504.0)));
                    (
                        self.procedural_sky_pipeline
                            .ok_or(RenderError::InvalidState(
                                "procedural sky pipeline отсутствует",
                            ))?,
                        None,
                        None,
                        Some(parameters),
                    )
                }
            };

        let mut dispatches = Vec::with_capacity(DISPATCH_COUNT as usize);
        for mip_level in 0..ENVIRONMENT_MIPS {
            let size = (ENVIRONMENT_SIZE >> mip_level).max(1);
            let mut parameters = sky_params.unwrap_or([0.0; 20]);
            parameters[0] = size as f32;
            if mip_level > 0 {
                parameters[1] = -1.0;
                parameters[3] = (mip_level - 1) as f32;
            }
            dispatches.push(EnvironmentDispatch {
                pipeline: if mip_level == 0 {
                    environment_pipeline
                } else {
                    self.prefilter_pipeline
                        .ok_or(RenderError::InvalidState("prefilter pipeline отсутствует"))?
                },
                source_view: if mip_level == 0 {
                    environment_source_view
                } else {
                    Some(environment_map.view)
                },
                source_sampler: if mip_level == 0 {
                    environment_source_sampler
                } else {
                    Some(environment_sampler)
                },
                output_view: environment_map.mip_array_views[mip_level as usize],
                parameters,
                size,
            });
        }
        let mut irradiance_parameters = [0.0_f32; 20];
        irradiance_parameters[0] = IRRADIANCE_SIZE as f32;
        dispatches.push(EnvironmentDispatch {
            pipeline: self
                .irradiance_pipeline
                .ok_or(RenderError::InvalidState("irradiance pipeline отсутствует"))?,
            source_view: Some(environment_map.view),
            source_sampler: Some(environment_sampler),
            output_view: irradiance_map.mip_array_views[0],
            parameters: irradiance_parameters,
            size: IRRADIANCE_SIZE,
        });
        for mip_level in 0..PREFILTER_MIPS {
            let size = (PREFILTER_SIZE >> mip_level).max(1);
            let mut parameters = [0.0_f32; 20];
            parameters[0] = size as f32;
            parameters[1] = mip_level as f32 / (PREFILTER_MIPS - 1) as f32;
            parameters[2] = ENVIRONMENT_SIZE as f32;
            dispatches.push(EnvironmentDispatch {
                pipeline: self
                    .prefilter_pipeline
                    .ok_or(RenderError::InvalidState("prefilter pipeline отсутствует"))?,
                source_view: Some(environment_map.view),
                source_sampler: Some(environment_sampler),
                output_view: prefiltered_map.mip_array_views[mip_level as usize],
                parameters,
                size,
            });
        }

        let device = self.device()?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(DISPATCH_COUNT),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(DISPATCH_COUNT),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(DISPATCH_COUNT),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(DISPATCH_COUNT),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(DISPATCH_COUNT)
            .pool_sizes(&pool_sizes);
        let compute_pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let compute_layout = self.compute_set_layout.ok_or(RenderError::InvalidState(
            "compute descriptor layout отсутствует",
        ))?;
        let layouts = vec![compute_layout; dispatches.len()];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(compute_pool.handle)
            .set_layouts(&layouts);
        // SAFETY: compute pool was sized for one set per dispatch.
        let sets = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
        let mut parameter_buffers = Vec::with_capacity(dispatches.len());
        for (index, (dispatch, set)) in dispatches.iter().zip(&sets).enumerate() {
            let bytes = float_bytes::<80>(&dispatch.parameters);
            let buffer = HostBuffer::new(
                self.instance()?,
                device,
                self.physical_device,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                &bytes,
            )?;
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(buffer.buffer)
                .offset(0)
                .range(80)];
            let output_info = [vk::DescriptorImageInfo::default()
                .image_view(dispatch.output_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let source_info = dispatch.source_view.map(|view| {
                [vk::DescriptorImageInfo::default()
                    .image_view(view)
                    .image_layout(if index > 0 && index < ENVIRONMENT_MIPS as usize {
                        vk::ImageLayout::GENERAL
                    } else {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    })]
            });
            let sampler_info = dispatch
                .source_sampler
                .map(|sampler| [vk::DescriptorImageInfo::default().sampler(sampler)]);
            let mut writes = vec![
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(&output_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(3)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&buffer_info),
            ];
            if let Some(source_info) = &source_info {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(source_info),
                );
            }
            if let Some(sampler_info) = &sampler_info {
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(sampler_info),
                );
            }
            // SAFETY: pool, set, source/output images and uniform buffer remain alive through submission.
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            parameter_buffers.push(buffer);
        }

        let compute_pipeline_layout =
            self.compute_pipeline_layout
                .ok_or(RenderError::InvalidState(
                    "compute pipeline layout отсутствует",
                ))?;
        self.submit_once(|command_buffer| {
            for image in [&environment_map, &irradiance_map, &prefiltered_map] {
                pipeline_barrier(
                    device,
                    command_buffer,
                    image_layout_barrier(
                        image.image,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::GENERAL,
                        vk::PipelineStageFlags2::NONE,
                        vk::AccessFlags2::NONE,
                        vk::PipelineStageFlags2::COMPUTE_SHADER,
                        vk::AccessFlags2::SHADER_STORAGE_WRITE,
                        color_mip_subresource_range(0, image.mip_array_views.len() as u32)
                            .layer_count(6),
                    ),
                );
            }
            for (index, (dispatch, set)) in dispatches.iter().zip(&sets).enumerate() {
                // SAFETY: each dispatch targets a single in-bounds mip view and six cube layers.
                unsafe {
                    device.cmd_bind_pipeline(
                        command_buffer,
                        vk::PipelineBindPoint::COMPUTE,
                        dispatch.pipeline,
                    );
                    device.cmd_bind_descriptor_sets(
                        command_buffer,
                        vk::PipelineBindPoint::COMPUTE,
                        compute_pipeline_layout,
                        0,
                        &[*set],
                        &[],
                    );
                    device.cmd_dispatch(
                        command_buffer,
                        dispatch.size.div_ceil(8),
                        dispatch.size.div_ceil(8),
                        6,
                    );
                }
                if index < ENVIRONMENT_MIPS as usize {
                    pipeline_barrier(
                        device,
                        command_buffer,
                        image_layout_barrier(
                            environment_map.image,
                            vk::ImageLayout::GENERAL,
                            if index + 1 == ENVIRONMENT_MIPS as usize {
                                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                            } else {
                                vk::ImageLayout::GENERAL
                            },
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_STORAGE_WRITE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER
                                | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                            vk::AccessFlags2::SHADER_SAMPLED_READ,
                            color_mip_subresource_range(0, ENVIRONMENT_MIPS).layer_count(6),
                        ),
                    );
                }
            }
            for image in [&irradiance_map, &prefiltered_map] {
                pipeline_barrier(
                    device,
                    command_buffer,
                    image_layout_barrier(
                        image.image,
                        vk::ImageLayout::GENERAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::PipelineStageFlags2::COMPUTE_SHADER,
                        vk::AccessFlags2::SHADER_STORAGE_WRITE,
                        vk::PipelineStageFlags2::FRAGMENT_SHADER,
                        vk::AccessFlags2::SHADER_SAMPLED_READ,
                        color_mip_subresource_range(0, image.mip_array_views.len() as u32)
                            .layer_count(6),
                    ),
                );
            }
        })?;
        drop(parameter_buffers);
        drop(compute_pool);
        self.make_gpu_environment(environment_map, irradiance_map, prefiltered_map)
    }
}

pub(crate) struct GpuEnvironment {
    pub(crate) _descriptor_pool: OwnedDescriptorPool,
    pub(crate) set: vk::DescriptorSet,
    pub(crate) _environment_map: DeviceImage,
    pub(crate) _irradiance_map: DeviceImage,
    pub(crate) _prefiltered_map: DeviceImage,
}

#[derive(Clone, Copy)]
pub(crate) struct EnvironmentDispatch {
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) source_view: Option<vk::ImageView>,
    pub(crate) source_sampler: Option<vk::Sampler>,
    pub(crate) output_view: vk::ImageView,
    pub(crate) parameters: [f32; 20],
    pub(crate) size: u32,
}
