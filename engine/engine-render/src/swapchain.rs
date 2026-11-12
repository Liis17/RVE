//! Swapchain: формат поверхности, present mode, размер, получение изображения и пересоздание.

use crate::*;

impl Renderer {
    pub(crate) fn recreate_swapchain(&mut self) -> Result<(), RenderError> {
        let capabilities = unsafe {
            self.surface_loader()?
                .get_physical_device_surface_capabilities(self.physical_device, self.surface()?)
        }?;
        if usable_surface_extent(capabilities, self.output_extent).is_none() {
            self.swapchain_dirty = true;
            return Ok(());
        }
        let device = self.device()?.clone();
        let retired_queue_full = self.retired_swapchains.is_full();
        if self.pending_swapchain_drop.is_some() || retired_queue_full {
            // SAFETY: overflow fallback reaches idle before releasing any oldest retired swapchain.
            unsafe { device.device_wait_idle() }?;
            drop(self.pending_swapchain_drop.take());
            if retired_queue_full {
                drop(self.retired_swapchains.remove_oldest_after_idle());
            }
        }
        self.flush_uploads()?;
        self.poll_uploads()?;
        let old_swapchain = self
            .swapchain
            .as_ref()
            .map_or(vk::SwapchainKHR::null(), |bundle| bundle.handle);
        let latency = self.latency.clone();
        let new_swapchain = SwapchainBundle::new(
            self.instance()?,
            &device,
            self.swapchain_loader()?,
            self.surface_loader()?,
            self.physical_device,
            self.surface()?,
            self.surface_format,
            self.output_extent,
            capabilities,
            old_swapchain,
            self.config.present_mode,
            latency.is_some(),
        )?;
        // Под блокировкой меняется только handle: старый swapchain уничтожается позже, вне неё.
        if let Some(latency) = latency.as_deref() {
            latency.lock_swapchain().replace(new_swapchain.handle);
        }
        self.present_mode = new_swapchain.present_mode;
        if self.present_mode == self.config.present_mode {
            self.last_warned_present_mode = None;
        } else if self.last_warned_present_mode != Some(self.present_mode) {
            eprintln!(
                "requested present mode {:?} is unavailable; using {:?}",
                self.config.present_mode, self.present_mode
            );
            self.last_warned_present_mode = Some(self.present_mode);
        }
        let replacement_generation = self.swapchain_generation.saturating_add(1);
        let previous_swapchain = self.swapchain.replace(new_swapchain);
        self.swapchain_generation = replacement_generation;
        self.swapchain_dirty = false;
        if let Some(previous_swapchain) = previous_swapchain {
            let old_image_count = previous_swapchain.images.len();
            if let Some(oldest) = self.retired_swapchains.retire(
                previous_swapchain,
                self.submission_generation,
                replacement_generation,
                old_image_count,
            ) {
                self.pending_swapchain_drop = Some(oldest);
                // SAFETY: queue overflow uses device idle as the bounded-queue destruction fallback.
                unsafe { device.device_wait_idle() }?;
                drop(self.pending_swapchain_drop.take());
            }
        }
        Ok(())
    }
}

pub(crate) fn select_present_mode(
    requested: PresentMode,
    supported: &[vk::PresentModeKHR],
) -> vk::PresentModeKHR {
    let requested = requested.vk();
    if supported.contains(&requested) {
        requested
    } else {
        vk::PresentModeKHR::FIFO
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcquireAction {
    Acquired { image_index: u32, suboptimal: bool },
    OutOfDate,
    Skipped,
    Failed(vk::Result),
}

pub(crate) enum AcquireImageError {
    BeforeAcquire(RenderError),
    AfterAcquire(RenderError),
}

pub(crate) fn classify_acquire_result(result: Result<(u32, bool), vk::Result>) -> AcquireAction {
    match result {
        Ok((image_index, suboptimal)) => AcquireAction::Acquired {
            image_index,
            suboptimal,
        },
        Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => AcquireAction::OutOfDate,
        Err(vk::Result::TIMEOUT | vk::Result::NOT_READY) => AcquireAction::Skipped,
        Err(error) => AcquireAction::Failed(error),
    }
}

pub(crate) fn choose_surface_format(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    formats: &[vk::SurfaceFormatKHR],
) -> Option<vk::SurfaceFormatKHR> {
    if formats.len() == 1 && formats[0].format == vk::Format::UNDEFINED {
        // SAFETY: the legacy UNDEFINED surface format lets the application choose a format.
        let properties = unsafe {
            instance
                .get_physical_device_format_properties(physical_device, vk::Format::B8G8R8A8_SRGB)
        };
        return properties
            .optimal_tiling_features
            .contains(SURFACE_FORMAT_FEATURES)
            .then_some(vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_SRGB,
                color_space: formats[0].color_space,
            });
    }
    let mut supported = Vec::new();
    for format in formats {
        // SAFETY: format properties are queried for a format reported by this surface's physical device.
        let properties = unsafe {
            instance.get_physical_device_format_properties(physical_device, format.format)
        };
        if properties
            .optimal_tiling_features
            .contains(SURFACE_FORMAT_FEATURES)
        {
            supported.push(*format);
        }
    }
    supported
        .iter()
        .find(|format| {
            format.format == vk::Format::B8G8R8A8_SRGB
                && format.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .copied()
        .or_else(|| supported.first().copied())
}

pub(crate) struct SwapchainBundle {
    pub(crate) instance: ash::Instance,
    pub(crate) device: ash::Device,
    pub(crate) handle: vk::SwapchainKHR,
    pub(crate) images: Vec<vk::Image>,
    pub(crate) views: Vec<vk::ImageView>,
    pub(crate) render_finished: Vec<vk::Semaphore>,
    pub(crate) extent: vk::Extent2D,
    pub(crate) present_mode: PresentMode,
    /// Изображения созданы с `TRANSFER_SRC`: доступен захват окна.
    pub(crate) transfer_src: bool,
}

impl SwapchainBundle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        loader: &ash::khr::swapchain::Device,
        surface_loader: &ash::khr::surface::Instance,
        physical_device: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        surface_format: vk::SurfaceFormatKHR,
        requested_extent: vk::Extent2D,
        capabilities: vk::SurfaceCapabilitiesKHR,
        old_swapchain: vk::SwapchainKHR,
        requested_present_mode: PresentMode,
        low_latency: bool,
    ) -> Result<Self, RenderError> {
        if !capabilities.supported_usage_flags.contains(SWAPCHAIN_USAGE) {
            return Err(RenderError::Unsupported(
                "surface больше не поддерживает COLOR_ATTACHMENT".to_owned(),
            ));
        }
        let extent = usable_surface_extent(capabilities, requested_extent)
            .ok_or(RenderError::InvalidConfig("swapchain extent равен нулю"))?;
        let mut image_count = capabilities.min_image_count.saturating_add(1);
        if capabilities.max_image_count != 0 {
            image_count = image_count.min(capabilities.max_image_count);
        }
        let composite_alpha = select_composite_alpha(capabilities.supported_composite_alpha)
            .ok_or_else(|| {
                RenderError::Unsupported("surface не поддерживает composite alpha".to_owned())
            })?;
        // SAFETY: the physical device and surface belong to this live instance.
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
        }?;
        let selected_present_mode = select_present_mode(requested_present_mode, &present_modes);
        // TRANSFER_SRC нужен только захвату окна, поэтому он необязателен.
        let transfer_src = capabilities
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_SRC);
        let image_usage = if transfer_src {
            SWAPCHAIN_USAGE | vk::ImageUsageFlags::TRANSFER_SRC
        } else {
            SWAPCHAIN_USAGE
        };
        let mut create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(surface_format.format)
            .image_color_space(surface_format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(image_usage)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(composite_alpha)
            .present_mode(selected_present_mode)
            .clipped(true)
            .old_swapchain(old_swapchain);
        let mut latency_create_info =
            vk::SwapchainLatencyCreateInfoNV::default().latency_mode_enable(low_latency);
        if low_latency {
            create_info = create_info.push_next(&mut latency_create_info);
        }
        // SAFETY: all swapchain parameters were checked against the current surface capabilities.
        let handle = unsafe { loader.create_swapchain(&create_info, None) }?;
        let mut bundle = Self {
            instance: instance.clone(),
            device: device.clone(),
            handle,
            images: Vec::new(),
            views: Vec::new(),
            render_finished: Vec::new(),
            extent,
            present_mode: PresentMode::from_vk(selected_present_mode),
            transfer_src,
        };
        // SAFETY: the swapchain was just created by this loader and is still alive.
        bundle.images = unsafe { loader.get_swapchain_images(bundle.handle) }?;
        for &image in &bundle.images {
            let view_info = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(surface_format.format)
                .components(vk::ComponentMapping::default())
                .subresource_range(color_subresource_range());
            // SAFETY: the view matches the swapchain image format and its single color subresource.
            let view = unsafe { device.create_image_view(&view_info, None) }?;
            bundle.views.push(view);
            // SAFETY: a default semaphore create info is valid for a binary semaphore.
            let semaphore =
                unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
            bundle.render_finished.push(semaphore);
        }
        Ok(bundle)
    }
}

impl Drop for SwapchainBundle {
    fn drop(&mut self) {
        for semaphore in self.render_finished.drain(..) {
            // SAFETY: renderer releases retired bundles by its documented frame/present heuristic or idle fallback.
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
        for view in self.views.drain(..) {
            // SAFETY: views of swapchain images are destroyed before the swapchain that owns the images.
            unsafe { self.device.destroy_image_view(view, None) };
        }
        if self.handle != vk::SwapchainKHR::null() {
            let loader = ash::khr::swapchain::Device::new(&self.instance, &self.device);
            // SAFETY: renderer releases retired bundles by its documented frame/present heuristic or idle fallback.
            unsafe { loader.destroy_swapchain(self.handle, None) };
            self.handle = vk::SwapchainKHR::null();
        }
    }
}

/// Получает swapchain image; `None` означает, что кадр нужно пропустить.
pub(crate) fn acquire_image(
    swapchain_loader: &ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    image_available: vk::Semaphore,
    image_count: usize,
    swapchain_dirty: &mut bool,
) -> Result<Option<(usize, bool)>, AcquireImageError> {
    // SAFETY: swapchain живёт в renderer до окончания acquire и получает валидный semaphore.
    let acquire_result = unsafe {
        swapchain_loader.acquire_next_image(
            swapchain,
            ACQUIRE_TIMEOUT_NS,
            image_available,
            vk::Fence::null(),
        )
    };
    let (image_index, suboptimal) = match classify_acquire_result(acquire_result) {
        AcquireAction::Acquired {
            image_index,
            suboptimal,
        } => (image_index, suboptimal),
        AcquireAction::OutOfDate => {
            *swapchain_dirty = true;
            return Ok(None);
        }
        AcquireAction::Skipped => {
            // TIMEOUT/NOT_READY do not signal image_available; the fence is still signaled,
            // frame_slot is unchanged, and reset_fences runs only after a successful acquire.
            return Ok(None);
        }
        AcquireAction::Failed(error) => {
            return Err(AcquireImageError::BeforeAcquire(RenderError::Vulkan(error)));
        }
    };
    let image_index = image_index as usize;
    if image_index >= image_count {
        *swapchain_dirty = true;
        return Err(AcquireImageError::AfterAcquire(RenderError::InvalidState(
            "Vulkan вернул неверный индекс swapchain image",
        )));
    }
    Ok(Some((image_index, suboptimal)))
}

pub(crate) fn select_surface_extent(
    capabilities: vk::SurfaceCapabilitiesKHR,
    requested: vk::Extent2D,
) -> vk::Extent2D {
    if capabilities.current_extent.width != u32::MAX {
        capabilities.current_extent
    } else {
        vk::Extent2D {
            width: requested.width.clamp(
                capabilities.min_image_extent.width,
                capabilities.max_image_extent.width,
            ),
            height: requested.height.clamp(
                capabilities.min_image_extent.height,
                capabilities.max_image_extent.height,
            ),
        }
    }
}

pub(crate) fn usable_surface_extent(
    capabilities: vk::SurfaceCapabilitiesKHR,
    requested: vk::Extent2D,
) -> Option<vk::Extent2D> {
    if requested.width == 0 || requested.height == 0 {
        return None;
    }
    let extent = select_surface_extent(capabilities, requested);
    (extent.width != 0 && extent.height != 0).then_some(extent)
}

pub(crate) fn select_composite_alpha(
    supported: vk::CompositeAlphaFlagsKHR,
) -> Option<vk::CompositeAlphaFlagsKHR> {
    [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ]
    .into_iter()
    .find(|alpha| supported.contains(*alpha))
}
