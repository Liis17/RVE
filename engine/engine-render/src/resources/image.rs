//! Изображения устройства, mip-уровни, барьеры текстур и сэмплеры.

use crate::*;

/// View LUT для composite: `None` — встроенная identity LUT (индекс 0).
pub(crate) fn color_lut_view(
    luts: &[Option<DeviceImage>],
    lut: Option<ColorLutId>,
) -> Option<vk::ImageView> {
    luts.get(lut.map_or(0, |lut| lut.0))
        .and_then(Option::as_ref)
        .map(|image| image.view)
}

/// Форма изображения `DeviceImage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageShape {
    /// 2D-изображение или 2D-массив.
    Flat,
    /// Cube-совместимое изображение из шести слоёв.
    Cube,
    /// 3D-изображение заданной глубины.
    Volume(u32),
}

pub(crate) struct DeviceImage {
    pub(crate) device: ash::Device,
    pub(crate) image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    pub(crate) view: vk::ImageView,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) format: vk::Format,
    #[cfg(all(windows, feature = "dlss"))]
    pub(crate) subresource_range: vk::ImageSubresourceRange,
    pub(crate) layer_views: Vec<vk::ImageView>,
    pub(crate) mip_array_views: Vec<vk::ImageView>,
}

impl DeviceImage {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        subresource_range: vk::ImageSubresourceRange,
    ) -> Result<Self, RenderError> {
        Self::new_with_layers(
            instance,
            device,
            physical_device,
            extent,
            format,
            usage,
            subresource_range,
            1,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_array(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        subresource_range: vk::ImageSubresourceRange,
        array_layers: u32,
    ) -> Result<Self, RenderError> {
        Self::new_with_layers(
            instance,
            device,
            physical_device,
            extent,
            format,
            usage,
            subresource_range,
            array_layers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_layers(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        subresource_range: vk::ImageSubresourceRange,
        array_layers: u32,
    ) -> Result<Self, RenderError> {
        Self::new_with_layers_kind(
            instance,
            device,
            physical_device,
            extent,
            format,
            usage,
            subresource_range,
            array_layers,
            ImageShape::Flat,
        )
    }

    /// 3D-изображение `size³` с одним mip-уровнем (LUT цветокоррекции).
    pub(crate) fn new_3d(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        size: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
    ) -> Result<Self, RenderError> {
        Self::new_with_layers_kind(
            instance,
            device,
            physical_device,
            vk::Extent2D {
                width: size,
                height: size,
            },
            format,
            usage,
            color_mip_subresource_range(0, 1),
            1,
            ImageShape::Volume(size),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_cube(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        size: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        mip_levels: u32,
    ) -> Result<Self, RenderError> {
        Self::new_with_layers_kind(
            instance,
            device,
            physical_device,
            vk::Extent2D {
                width: size,
                height: size,
            },
            format,
            usage,
            color_mip_subresource_range(0, mip_levels),
            6,
            ImageShape::Cube,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_layers_kind(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        subresource_range: vk::ImageSubresourceRange,
        array_layers: u32,
        shape: ImageShape,
    ) -> Result<Self, RenderError> {
        let cube_compatible = shape == ImageShape::Cube;
        let depth = match shape {
            ImageShape::Volume(depth) => depth,
            ImageShape::Flat | ImageShape::Cube => 1,
        };
        if array_layers == 0 {
            return Err(RenderError::InvalidConfig(
                "image должен иметь хотя бы один слой",
            ));
        }
        let mut subresource_range = subresource_range;
        subresource_range.layer_count = array_layers;
        let image_info = vk::ImageCreateInfo::default()
            .flags(if cube_compatible {
                vk::ImageCreateFlags::CUBE_COMPATIBLE
            } else {
                vk::ImageCreateFlags::empty()
            })
            .image_type(if depth > 1 {
                vk::ImageType::TYPE_3D
            } else {
                vk::ImageType::TYPE_2D
            })
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth,
            })
            .mip_levels(subresource_range.level_count)
            .array_layers(array_layers)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: image parameters use a supported optimal format and non-zero extent.
        let image = unsafe { device.create_image(&image_info, None) }?;
        let mut target = Self {
            device: device.clone(),
            image,
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            #[cfg(all(windows, feature = "dlss"))]
            format,
            #[cfg(all(windows, feature = "dlss"))]
            subresource_range,
            layer_views: Vec::new(),
            mip_array_views: Vec::new(),
        };
        // SAFETY: image was created by this device and remains alive.
        let requirements = unsafe { device.get_image_memory_requirements(target.image) };
        let memory_type = find_memory_type(
            instance,
            physical_device,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            RenderError::Unsupported("нет DEVICE_LOCAL memory для offscreen image".to_owned())
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: allocation requirements and memory type were reported for this image/device.
        target.memory = unsafe { device.allocate_memory(&allocate_info, None) }?;
        // SAFETY: memory allocation satisfies the image's reported requirements.
        unsafe { device.bind_image_memory(target.image, target.memory, 0) }?;
        let view_info = vk::ImageViewCreateInfo::default()
            .image(target.image)
            .view_type(if cube_compatible {
                vk::ImageViewType::CUBE
            } else if depth > 1 {
                vk::ImageViewType::TYPE_3D
            } else if array_layers == 1 {
                vk::ImageViewType::TYPE_2D
            } else {
                vk::ImageViewType::TYPE_2D_ARRAY
            })
            .format(format)
            .components(vk::ComponentMapping::default())
            .subresource_range(subresource_range);
        // SAFETY: image view range matches the image's concrete mip and array-layer counts.
        target.view = unsafe { device.create_image_view(&view_info, None) }?;
        if array_layers > 1 {
            for layer in 0..array_layers {
                let layer_view_info = vk::ImageViewCreateInfo::default()
                    .image(target.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .components(vk::ComponentMapping::default())
                    .subresource_range(subresource_range.base_array_layer(layer).layer_count(1));
                // SAFETY: the per-layer view addresses one valid layer of the array image.
                target
                    .layer_views
                    .push(unsafe { device.create_image_view(&layer_view_info, None) }?);
            }
        }
        if cube_compatible {
            for mip_level in 0..subresource_range.level_count {
                let mip_view_info = vk::ImageViewCreateInfo::default()
                    .image(target.image)
                    .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
                    .format(format)
                    .components(vk::ComponentMapping::default())
                    .subresource_range(
                        color_mip_subresource_range(mip_level, 1).layer_count(array_layers),
                    );
                // SAFETY: each storage view addresses one mip and all six cube-compatible layers.
                target
                    .mip_array_views
                    .push(unsafe { device.create_image_view(&mip_view_info, None) }?);
            }
        }
        Ok(target)
    }
}

impl Drop for DeviceImage {
    fn drop(&mut self) {
        for view in self.mip_array_views.drain(..) {
            // SAFETY: each mip view is an owned child and is destroyed before its image.
            unsafe { self.device.destroy_image_view(view, None) };
        }
        for view in self.layer_views.drain(..) {
            // SAFETY: each layer view is an owned child of the image and is destroyed first.
            unsafe { self.device.destroy_image_view(view, None) };
        }
        if self.view != vk::ImageView::null() {
            // SAFETY: view is an owned child of image and is destroyed before its image.
            unsafe { self.device.destroy_image_view(self.view, None) };
        }
        if self.image != vk::Image::null() {
            // SAFETY: image is no longer used by the queue and its view was destroyed first.
            unsafe { self.device.destroy_image(self.image, None) };
        }
        if self.memory != vk::DeviceMemory::null() {
            // SAFETY: image is destroyed before freeing its bound memory.
            unsafe { self.device.free_memory(self.memory, None) };
        }
    }
}

pub(crate) fn mip_level_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).leading_zeros()
}

pub(crate) fn mip_extent(width: u32, height: u32, mip_level: u32) -> vk::Extent2D {
    vk::Extent2D {
        width: (width >> mip_level).max(1),
        height: (height >> mip_level).max(1),
    }
}

pub(crate) fn texture_mip_blit_region(
    source_mip_level: u32,
    source_extent: vk::Extent2D,
    destination_extent: vk::Extent2D,
) -> vk::ImageBlit {
    vk::ImageBlit::default()
        .src_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(source_mip_level)
                .base_array_layer(0)
                .layer_count(1),
        )
        .src_offsets([
            vk::Offset3D::default(),
            vk::Offset3D {
                x: source_extent.width as i32,
                y: source_extent.height as i32,
                z: 1,
            },
        ])
        .dst_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .mip_level(source_mip_level + 1)
                .base_array_layer(0)
                .layer_count(1),
        )
        .dst_offsets([
            vk::Offset3D::default(),
            vk::Offset3D {
                x: destination_extent.width as i32,
                y: destination_extent.height as i32,
                z: 1,
            },
        ])
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn texture_image_barrier(
    image: vk::Image,
    subresource_range: vk::ImageSubresourceRange,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage_mask: vk::PipelineStageFlags2,
    src_access_mask: vk::AccessFlags2,
    dst_stage_mask: vk::PipelineStageFlags2,
    dst_access_mask: vk::AccessFlags2,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage_mask)
        .src_access_mask(src_access_mask)
        .dst_stage_mask(dst_stage_mask)
        .dst_access_mask(dst_access_mask)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(subresource_range)
}

pub(crate) fn texture_upload_barrier(
    image: vk::Image,
    mip_levels: u32,
) -> vk::ImageMemoryBarrier2<'static> {
    texture_image_barrier(
        image,
        color_mip_subresource_range(0, mip_levels),
        vk::ImageLayout::UNDEFINED,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::NONE,
        vk::AccessFlags2::NONE,
        vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT,
        vk::AccessFlags2::TRANSFER_WRITE,
    )
}

pub(crate) fn texture_mip_source_barrier(
    image: vk::Image,
    subresource_range: vk::ImageSubresourceRange,
) -> vk::ImageMemoryBarrier2<'static> {
    texture_image_barrier(
        image,
        subresource_range,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::BLIT,
        vk::AccessFlags2::TRANSFER_READ,
    )
}

pub(crate) fn texture_mip_shader_barrier(
    image: vk::Image,
    subresource_range: vk::ImageSubresourceRange,
) -> vk::ImageMemoryBarrier2<'static> {
    texture_image_barrier(
        image,
        subresource_range,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::BLIT,
        vk::AccessFlags2::TRANSFER_READ,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    )
}

pub(crate) fn texture_mip_destination_shader_barrier(
    image: vk::Image,
    subresource_range: vk::ImageSubresourceRange,
) -> vk::ImageMemoryBarrier2<'static> {
    texture_image_barrier(
        image,
        subresource_range,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    )
}

pub(crate) fn material_sampler_info(anisotropy: Option<f32>) -> vk::SamplerCreateInfo<'static> {
    let mut info = vk::SamplerCreateInfo::default()
        .mag_filter(vk::Filter::LINEAR)
        .min_filter(vk::Filter::LINEAR)
        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
        .address_mode_u(vk::SamplerAddressMode::REPEAT)
        .address_mode_v(vk::SamplerAddressMode::REPEAT)
        .address_mode_w(vk::SamplerAddressMode::REPEAT)
        .min_lod(0.0)
        .max_lod(vk::LOD_CLAMP_NONE);
    info = match anisotropy {
        Some(max_anisotropy) => info.anisotropy_enable(true).max_anisotropy(max_anisotropy),
        None => info.anisotropy_enable(false).max_anisotropy(1.0),
    };
    info
}

pub(crate) fn sampler_anisotropy(supported: bool, device_limit: f32) -> Option<f32> {
    if supported {
        Some(16.0_f32.min(device_limit))
    } else {
        None
    }
}

pub(crate) fn texture_format(color_space: TextureColorSpace) -> vk::Format {
    match color_space {
        TextureColorSpace::Srgb => SRGB_TEXTURE_FORMAT,
        TextureColorSpace::Linear => LINEAR_TEXTURE_FORMAT,
    }
}

pub(crate) fn texture_format_reasons(
    srgb_features: vk::FormatFeatureFlags,
    linear_features: vk::FormatFeatureFlags,
) -> Vec<String> {
    [
        ("R8G8B8A8_SRGB", srgb_features),
        ("R8G8B8A8_UNORM", linear_features),
    ]
    .into_iter()
    .filter_map(|(format, features)| {
        let missing = missing_texture_format_features(features);
        (!missing.is_empty()).then(|| {
            format!(
                "{format} не поддерживает возможности: {}",
                missing.join(", ")
            )
        })
    })
    .collect()
}
