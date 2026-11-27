//! Swapchain: формат поверхности, present mode, размер, получение изображения и пересоздание.

use crate::*;

impl Renderer {
    pub(crate) fn poll_swapchain_retirement(&mut self) -> Result<(), RenderError> {
        if !self.capabilities.present_fence {
            return Ok(());
        }
        let device = self.device()?.clone();
        let completed_presents = self.present_fences.poll(|fence| {
            // SAFETY: this renderer owns the fence and only queries its status on its render thread.
            unsafe { device.get_fence_status(fence) }
        })?;
        for (_, generation) in completed_presents {
            self.retired_swapchains.present_fence_completed(generation);
        }

        let mut completed_slots = Vec::new();
        for (slot, frame) in self.frames.iter().enumerate() {
            // SAFETY: fence status queries are read-only and each frame fence belongs to this device.
            if unsafe { device.get_fence_status(frame.fence) }? {
                completed_slots.push((slot, frame.submission_generation));
            }
        }
        for (slot, generation) in completed_slots {
            self.retired_swapchains.slot_completed(slot, generation);
        }
        Ok(())
    }

    pub(crate) fn wait_present_fences_for_generation(
        &mut self,
        device: &ash::Device,
        generation: u64,
    ) -> Result<(), RenderError> {
        let fences = self.present_fences.generation_fences(generation);
        if fences.is_empty() {
            return Ok(());
        }
        // SAFETY: all fences belong to this device and were reserved only for this generation.
        unsafe { device.wait_for_fences(&fences, true, u64::MAX) }?;
        let completed = self.present_fences.complete_generation(generation);
        for _ in 0..completed {
            self.retired_swapchains.present_fence_completed(generation);
        }
        Ok(())
    }

    pub(crate) fn recreate_swapchain(&mut self) -> Result<(), RenderError> {
        if let Err(error) = self.poll_swapchain_retirement() {
            self.fail_all_queued_captures("swapchain retirement polling failed");
            return Err(error);
        }
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
            // SAFETY: overflow fallback waits for graphics work before releasing retired swapchains.
            unsafe { device.device_wait_idle() }?;
            if let Some(generation) = self
                .pending_swapchain_drop
                .as_ref()
                .map(|bundle| bundle.generation)
            {
                if self.capabilities.present_fence {
                    self.wait_present_fences_for_generation(&device, generation)?;
                }
                drop(self.pending_swapchain_drop.take());
            }
            if retired_queue_full {
                if let Some(generation) = self.retired_swapchains.oldest_generation() {
                    self.wait_present_fences_for_generation(&device, generation)?;
                    if self.retired_swapchains.oldest_generation() == Some(generation) {
                        drop(self.retired_swapchains.remove_oldest_after_idle());
                    }
                } else {
                    drop(self.retired_swapchains.remove_oldest_after_idle());
                }
            }
        }
        self.flush_uploads()?;
        self.poll_uploads()?;
        let latency = self.latency.clone();
        if let Some(latency) = latency.as_deref() {
            latency.lock_swapchain().replace(vk::SwapchainKHR::null());
        }
        let replacement_generation = self.swapchain_generation.saturating_add(1);
        let instance = self.instance()?.clone();
        let swapchain_loader = self.swapchain_loader()?.clone();
        let surface_loader = self.surface_loader()?.clone();
        let physical_device = self.physical_device;
        let surface = self.surface()?;
        let surface_format = self.surface_format;
        let output_extent = self.output_extent;
        let requested_present_mode = self.config.present_mode;
        let present_fence_mode = self.capabilities.present_fence;
        let submission_generation = self.submission_generation;
        recreate_swapchain(
            &mut self.swapchain,
            |bundle| bundle.handle,
            |previous_swapchain| {
                let old_image_count = previous_swapchain.images.len();
                let previous_generation = previous_swapchain.generation;
                let overflow = if present_fence_mode {
                    self.retired_swapchains.retire_present_fence(
                        previous_swapchain,
                        submission_generation,
                        previous_generation,
                        self.present_fences.outstanding(previous_generation),
                    )
                } else {
                    self.retired_swapchains.retire(
                        previous_swapchain,
                        submission_generation,
                        replacement_generation,
                        old_image_count,
                    )
                };
                if let Some(oldest) = overflow {
                    self.pending_swapchain_drop = Some(oldest);
                    // SAFETY: the old generation remains owned while submissions are waited.
                    unsafe { device.device_wait_idle() }?;
                    if present_fence_mode {
                        let generation = self
                            .pending_swapchain_drop
                            .as_ref()
                            .map(|bundle| bundle.generation)
                            .ok_or(RenderError::InvalidState(
                                "pending retired swapchain отсутствует",
                            ))?;
                        let fences = self.present_fences.generation_fences(generation);
                        if !fences.is_empty() {
                            // SAFETY: these fences belong to this device and track this generation's queued presents.
                            unsafe { device.wait_for_fences(&fences, true, u64::MAX) }?;
                            let completed = self.present_fences.complete_generation(generation);
                            for _ in 0..completed {
                                self.retired_swapchains.present_fence_completed(generation);
                            }
                        }
                    }
                }
                Ok(())
            },
            |old_swapchain| {
                SwapchainBundle::new(
                    &instance,
                    &device,
                    &swapchain_loader,
                    &surface_loader,
                    physical_device,
                    surface,
                    surface_format,
                    output_extent,
                    capabilities,
                    old_swapchain,
                    requested_present_mode,
                    latency.is_some(),
                    replacement_generation,
                )
            },
        )?;
        let new_swapchain = self
            .swapchain
            .as_ref()
            .ok_or(RenderError::InvalidState("новый swapchain отсутствует"))?;
        // Reflex now points at the new swapchain; retired handles remain outside its lock.
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
        self.swapchain_generation = replacement_generation;
        self.swapchain_dirty = false;
        if self.pending_swapchain_drop.is_some() {
            if self.capabilities.present_fence {
                let generation = self
                    .pending_swapchain_drop
                    .as_ref()
                    .map(|bundle| bundle.generation)
                    .ok_or(RenderError::InvalidState(
                        "pending retired swapchain отсутствует",
                    ))?;
                self.wait_present_fences_for_generation(&device, generation)?;
            }
            drop(self.pending_swapchain_drop.take());
        }
        Ok(())
    }
}

fn recreate_swapchain<T, E>(
    active: &mut Option<T>,
    handle: impl FnOnce(&T) -> vk::SwapchainKHR,
    retire: impl FnOnce(T) -> Result<(), E>,
    create: impl FnOnce(vk::SwapchainKHR) -> Result<T, E>,
) -> Result<(), E> {
    let retired = active.take();
    let old_handle = retired.as_ref().map_or(vk::SwapchainKHR::null(), handle);
    if let Some(retired) = retired {
        retire(retired)?;
    }
    *active = Some(create(old_handle)?);
    Ok(())
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
    pub(crate) generation: u64,
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
        generation: u64,
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
            generation,
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
            // SAFETY: renderer waits for slot submissions and either the present-fence or heuristic retirement condition; teardown accepts device loss.
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
        for view in self.views.drain(..) {
            // SAFETY: views of swapchain images are destroyed before the swapchain that owns the images.
            unsafe { self.device.destroy_image_view(view, None) };
        }
        if self.handle != vk::SwapchainKHR::null() {
            let loader = ash::khr::swapchain::Device::new(&self.instance, &self.device);
            // SAFETY: the renderer proves the selected retirement condition before Drop, or teardown observes device loss.
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

#[cfg(test)]
mod tests {
    use super::recreate_swapchain;
    use ash::vk::{self, Handle as _};

    #[test]
    fn failed_recreation_keeps_old_handle_retired_and_next_attempt_uses_null() {
        let old_handle = vk::SwapchainKHR::from_raw(7);
        let mut active = Some(old_handle);
        let mut retired = Vec::new();
        let first_attempt = recreate_swapchain(
            &mut active,
            |handle| *handle,
            |handle| {
                retired.push(handle);
                Ok::<_, &'static str>(())
            },
            |handle| {
                assert_eq!(handle, old_handle);
                Err("create failed")
            },
        );
        assert_eq!(first_attempt, Err("create failed"));
        assert_eq!(retired, vec![old_handle]);
        assert_eq!(active, None);

        let second_attempt = recreate_swapchain(
            &mut active,
            |handle| *handle,
            |handle| {
                retired.push(handle);
                Ok::<_, &'static str>(())
            },
            |handle| {
                assert_eq!(handle, vk::SwapchainKHR::null());
                Ok(vk::SwapchainKHR::from_raw(8))
            },
        );
        assert_eq!(second_attempt, Ok(()));
        assert_eq!(retired, vec![old_handle]);
        assert_eq!(active, Some(vk::SwapchainKHR::from_raw(8)));
    }
}
