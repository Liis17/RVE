//! Vulkan-рендерер, форматы изображений и материалы сцены.

use ash::{Entry, vk};
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::Arc;

const FRAMES_IN_FLIGHT: usize = 2;
const OFFSCREEN_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// Настройки Vulkan-рендерера.
pub struct RendererConfig {
    pub render_scale: f32,
    pub extra_instance_extensions: Vec<&'static CStr>,
    pub extra_device_extensions: Vec<&'static CStr>,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            render_scale: 1.0,
            extra_instance_extensions: Vec::new(),
            extra_device_extensions: Vec::new(),
        }
    }
}

/// Возможности выбранного физического устройства.
pub struct Capabilities {
    pub device_name: String,
    pub vendor_id: u32,
    pub api_version: u32,
    pub discrete: bool,
    pub tessellation_shader: bool,
}

/// Ошибка загрузки Vulkan или создания рендерера.
#[derive(Debug)]
pub enum RenderError {
    InvalidConfig(&'static str),
    Loader(String),
    Handle(HandleError),
    Vulkan(vk::Result),
    Unsupported(String),
    NoSuitableDevice(Vec<String>),
    InvalidState(&'static str),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "неверная конфигурация рендерера: {message}"),
            Self::Loader(message) => write!(f, "не удалось загрузить Vulkan: {message}"),
            Self::Handle(error) => write!(f, "не удалось получить window handle: {error}"),
            Self::Vulkan(error) => write!(f, "ошибка Vulkan: {error:?}"),
            Self::Unsupported(message) => {
                write!(f, "неподдерживаемая конфигурация Vulkan: {message}")
            }
            Self::NoSuitableDevice(reasons) => {
                write!(f, "не найден подходящий Vulkan GPU")?;
                for reason in reasons {
                    write!(f, "; {reason}")?;
                }
                Ok(())
            }
            Self::InvalidState(message) => write!(f, "некорректное состояние рендерера: {message}"),
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Handle(error) => Some(error),
            _ => None,
        }
    }
}

impl From<vk::Result> for RenderError {
    fn from(error: vk::Result) -> Self {
        Self::Vulkan(error)
    }
}

/// Результат попытки показать кадр.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStatus {
    Presented,
    Skipped,
}

trait WindowHandleOwner: HasDisplayHandle + HasWindowHandle + Send + Sync {}

impl<T> WindowHandleOwner for T where T: HasDisplayHandle + HasWindowHandle + Send + Sync {}

/// Создаёт offscreen-кадр и выводит его в окно через Vulkan swapchain.
pub struct Renderer {
    entry: Entry,
    instance: Option<ash::Instance>,
    surface_loader: Option<ash::khr::surface::Instance>,
    surface: Option<vk::SurfaceKHR>,
    device: Option<ash::Device>,
    swapchain_loader: Option<ash::khr::swapchain::Device>,
    physical_device: vk::PhysicalDevice,
    queue: vk::Queue,
    queue_family_index: u32,
    command_pool: Option<vk::CommandPool>,
    frames: Vec<FrameResources>,
    frame_slot: usize,
    swapchain: Option<SwapchainBundle>,
    surface_format: vk::SurfaceFormatKHR,
    output_extent: vk::Extent2D,
    swapchain_dirty: bool,
    config: RendererConfig,
    capabilities: Capabilities,
    window: Arc<dyn WindowHandleOwner>,
}

impl Renderer {
    /// Загружает Vulkan и создаёт renderer. Окно удерживается до уничтожения surface.
    pub fn new<W>(
        window: Arc<W>,
        width: u32,
        height: u32,
        config: RendererConfig,
    ) -> Result<Self, RenderError>
    where
        W: HasDisplayHandle + HasWindowHandle + Send + Sync + 'static,
    {
        validate_render_scale(config.render_scale)?;
        let entry = engine_platform::load_vulkan()
            .map_err(|error| RenderError::Loader(error.to_string()))?;
        let window: Arc<dyn WindowHandleOwner> = window;
        let mut renderer = Self {
            entry,
            instance: None,
            surface_loader: None,
            surface: None,
            device: None,
            swapchain_loader: None,
            physical_device: vk::PhysicalDevice::null(),
            queue: vk::Queue::null(),
            queue_family_index: 0,
            command_pool: None,
            frames: Vec::new(),
            frame_slot: 0,
            swapchain: None,
            surface_format: vk::SurfaceFormatKHR::default(),
            output_extent: vk::Extent2D { width, height },
            swapchain_dirty: true,
            config,
            capabilities: Capabilities {
                device_name: String::new(),
                vendor_id: 0,
                api_version: 0,
                discrete: false,
                tessellation_shader: false,
            },
            window,
        };
        renderer.initialize()?;
        Ok(renderer)
    }

    /// Возвращает характеристики выбранного GPU.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Отмечает физический размер клиентской области как изменённый.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.output_extent = vk::Extent2D { width, height };
        self.swapchain_dirty = true;
    }

    /// Очищает offscreen-таргет и масштабирует его содержимое в swapchain.
    pub fn render_frame(&mut self, clear_color: [f32; 4]) -> Result<FrameStatus, RenderError> {
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            return Ok(FrameStatus::Skipped);
        }
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.recreate_swapchain()?;
            return Ok(FrameStatus::Skipped);
        }

        let device = self.device()?.clone();
        let swapchain_loader = self.swapchain_loader()?.clone();
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let frame_slot = self.frame_slot;
        let frame = self
            .frames
            .get(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let fence = frame.fence;
        let image_available = frame.image_available;
        let command_buffer = frame.command_buffer;
        let swapchain_handle = swapchain.handle;
        let image_count = swapchain.images.len();

        // SAFETY: fence принадлежит текущему слоту и предыдущая работа этого слота завершается перед повторным использованием.
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }?;
        // SAFETY: swapchain живёт в self до окончания acquire и получает валидный semaphore.
        let (image_index, acquire_suboptimal) = match unsafe {
            swapchain_loader.acquire_next_image(
                swapchain_handle,
                u64::MAX,
                image_available,
                vk::Fence::null(),
            )
        } {
            Ok(result) => result,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.swapchain_dirty = true;
                return Ok(FrameStatus::Skipped);
            }
            Err(error) => return Err(RenderError::Vulkan(error)),
        };
        let image_index = image_index as usize;
        if image_index >= image_count {
            self.swapchain_dirty = true;
            return Err(RenderError::InvalidState(
                "Vulkan вернул неверный индекс swapchain image",
            ));
        }

        let offscreen_extent = scaled_extent(swapchain.extent, self.config.render_scale);
        let target = self.frames[frame_slot]
            .target
            .as_mut()
            .ok_or(RenderError::InvalidState("offscreen target отсутствует"))?;
        record_frame(
            &device,
            command_buffer,
            target,
            swapchain.images[image_index],
            swapchain.extent,
            offscreen_extent,
            clear_color,
        )?;

        // Fence сбрасывается только после успешного acquire и полной записи command buffer.
        // SAFETY: эта точка — непосредственно перед submit, которому принадлежит fence.
        unsafe { device.reset_fences(&[fence]) }?;

        let wait_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(image_available)
            .stage_mask(vk::PipelineStageFlags2::TRANSFER)
            .device_index(0)];
        let command_infos = [vk::CommandBufferSubmitInfo::default()
            .command_buffer(command_buffer)
            .device_mask(0)];
        let render_finished = swapchain.render_finished[image_index];
        let signal_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(render_finished)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .device_index(0)];
        let submit_info = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait_infos)
            .command_buffer_infos(&command_infos)
            .signal_semaphore_infos(&signal_infos)];
        // SAFETY: command buffer, semaphores and fence are owned by this renderer and are synchronized per frame/image.
        unsafe { device.queue_submit2(self.queue, &submit_info, fence) }?;

        let wait_semaphores = [render_finished];
        let swapchains = [swapchain_handle];
        let image_indices = [image_index as u32];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&wait_semaphores)
            .swapchains(&swapchains)
            .image_indices(&image_indices);
        // SAFETY: present waits on the semaphore signaled by the preceding queue submission.
        let present_result = unsafe { swapchain_loader.queue_present(self.queue, &present_info) };
        self.frame_slot = (self.frame_slot + 1) % self.frames.len();
        match present_result {
            Ok(present_suboptimal) => {
                if acquire_suboptimal || present_suboptimal {
                    self.swapchain_dirty = true;
                }
                Ok(FrameStatus::Presented)
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.swapchain_dirty = true;
                Ok(FrameStatus::Skipped)
            }
            Err(error) => Err(RenderError::Vulkan(error)),
        }
    }

    fn initialize(&mut self) -> Result<(), RenderError> {
        let display_handle = self.window.display_handle().map_err(RenderError::Handle)?;
        let required = engine_platform::enumerate_required_extensions(display_handle.as_raw())
            .map_err(RenderError::Vulkan)?;
        let mut requested_instance_extensions = required
            .iter()
            .map(|pointer| {
                // SAFETY: the platform adapter returns static, null-terminated extension names.
                unsafe { CStr::from_ptr(*pointer) }.to_owned()
            })
            .collect::<Vec<_>>();
        requested_instance_extensions.extend(
            engine_platform::INSTANCE_EXTENSIONS
                .iter()
                .map(|name| CString::from(*name)),
        );
        requested_instance_extensions.extend(
            self.config
                .extra_instance_extensions
                .iter()
                .map(|name| CString::from(*name)),
        );
        let requested_instance_extensions = deduplicate_names(requested_instance_extensions);
        let available_instance_extensions =
            // SAFETY: querying loader extension properties does not require an instance.
            unsafe { self.entry.enumerate_instance_extension_properties(None) }?;
        check_extensions(
            &requested_instance_extensions,
            &available_instance_extensions,
            "instance",
        )?;

        let mut enabled_layers = Vec::new();
        if cfg!(debug_assertions) {
            let validation_layer = c"VK_LAYER_KHRONOS_validation";
            let layers =
                // SAFETY: querying the loader's available instance layers is read-only.
                unsafe { self.entry.enumerate_instance_layer_properties() }?;
            let validation_available = layers.iter().any(|layer| {
                // SAFETY: Vulkan layer property names are fixed-size null-terminated strings.
                (unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) }) == validation_layer
            });
            if validation_available {
                enabled_layers.push(validation_layer);
            } else {
                eprintln!(
                    "VK_LAYER_KHRONOS_validation отсутствует; продолжаю без validation layer"
                );
            }
        }

        let instance_extension_pointers = requested_instance_extensions
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let layer_pointers = enabled_layers
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let application_info = vk::ApplicationInfo::default()
            .application_name(c"RVE")
            .application_version(vk::make_api_version(0, 0, 1, 0))
            .engine_name(c"RVE")
            .engine_version(vk::make_api_version(0, 0, 1, 0))
            .api_version(vk::make_api_version(0, 1, 3, 0));
        let instance_info = vk::InstanceCreateInfo::default()
            .flags(engine_platform::INSTANCE_FLAGS)
            .application_info(&application_info)
            .enabled_extension_names(&instance_extension_pointers)
            .enabled_layer_names(&layer_pointers);
        // SAFETY: all extension/layer names and create-info values remain alive for this call.
        let instance = unsafe { self.entry.create_instance(&instance_info, None) }?;
        self.instance = Some(instance);

        let display_handle = self.window.display_handle().map_err(RenderError::Handle)?;
        let window_handle = self.window.window_handle().map_err(RenderError::Handle)?;
        // SAFETY: handles refer to the live Arc-owned window and the instance enabled required surface extensions.
        let surface = unsafe {
            engine_platform::create_surface(
                &self.entry,
                self.instance()?,
                display_handle.as_raw(),
                window_handle.as_raw(),
                None,
            )
        }?;
        self.surface_loader = Some(ash::khr::surface::Instance::new(
            &self.entry,
            self.instance()?,
        ));
        self.surface = Some(surface);

        let mut required_device_extensions = self
            .config
            .extra_device_extensions
            .iter()
            .map(|name| CString::from(*name))
            .collect::<Vec<_>>();
        required_device_extensions.push(ash::khr::swapchain::NAME.to_owned());
        required_device_extensions.extend(
            engine_platform::DEVICE_EXTENSIONS
                .iter()
                .map(|name| CString::from(*name)),
        );
        let required_device_extensions = deduplicate_names(required_device_extensions);
        let choice = choose_device(
            self.instance()?,
            self.surface_loader()?,
            self.surface()?,
            &required_device_extensions,
        )?;
        self.physical_device = choice.physical_device;
        self.queue_family_index = choice.queue_family_index;
        self.surface_format = choice.surface_format;
        self.capabilities = Capabilities {
            device_name: choice.device_name,
            vendor_id: choice.vendor_id,
            api_version: choice.api_version,
            discrete: choice.discrete,
            tessellation_shader: choice.tessellation_shader,
        };

        let queue_priority = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(self.queue_family_index)
            .queue_priorities(&queue_priority)];
        let extension_pointers = required_device_extensions
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let features = vk::PhysicalDeviceFeatures::default()
            .tessellation_shader(self.capabilities.tessellation_shader);
        let mut features_13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&extension_pointers)
            .enabled_features(&features)
            .push_next(&mut features_13);
        // SAFETY: the selected physical device reported every requested feature and extension.
        let device = unsafe {
            self.instance()?
                .create_device(self.physical_device, &device_info, None)
        }?;
        self.device = Some(device);
        self.queue =
            // SAFETY: queue family and index were checked during physical-device selection.
            unsafe { self.device()?.get_device_queue(self.queue_family_index, 0) };
        self.swapchain_loader = Some(ash::khr::swapchain::Device::new(
            self.instance()?,
            self.device()?,
        ));

        let command_pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(self.queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: device and queue-family index are valid for this renderer.
        self.command_pool =
            Some(unsafe { self.device()?.create_command_pool(&command_pool_info, None) }?);
        self.create_frame_resources()?;

        if self.output_extent.width != 0 && self.output_extent.height != 0 {
            self.recreate_swapchain()?;
        }
        Ok(())
    }

    fn create_frame_resources(&mut self) -> Result<(), RenderError> {
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool()?)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(FRAMES_IN_FLIGHT as u32);
        // SAFETY: command pool was created for the selected graphics queue family.
        let command_buffers = unsafe { self.device()?.allocate_command_buffers(&allocate_info) }?;
        for command_buffer in command_buffers {
            self.frames
                .push(FrameResources::new(self.device()?, command_buffer)?);
        }
        Ok(())
    }

    fn recreate_swapchain(&mut self) -> Result<(), RenderError> {
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            return Ok(());
        }
        // SAFETY: recreation waits for work submitted by this device before replacing its frame targets.
        unsafe { self.device()?.device_wait_idle() }?;
        let old_swapchain = self
            .swapchain
            .as_ref()
            .map_or(vk::SwapchainKHR::null(), |bundle| bundle.handle);
        let new_swapchain = SwapchainBundle::new(
            self.instance()?,
            self.device()?,
            self.swapchain_loader()?,
            self.surface_loader()?,
            self.physical_device,
            self.surface()?,
            self.surface_format,
            self.output_extent,
            old_swapchain,
        )?;
        let render_extent = scaled_extent(new_swapchain.extent, self.config.render_scale);
        let mut targets = Vec::with_capacity(self.frames.len());
        for _ in 0..self.frames.len() {
            targets.push(OffscreenTarget::new(
                self.instance()?,
                self.device()?,
                self.physical_device,
                render_extent,
            )?);
        }
        self.swapchain = Some(new_swapchain);
        for (frame, target) in self.frames.iter_mut().zip(targets) {
            frame.target = Some(target);
        }
        self.swapchain_dirty = false;
        Ok(())
    }

    fn instance(&self) -> Result<&ash::Instance, RenderError> {
        self.instance
            .as_ref()
            .ok_or(RenderError::InvalidState("instance отсутствует"))
    }

    fn surface_loader(&self) -> Result<&ash::khr::surface::Instance, RenderError> {
        self.surface_loader
            .as_ref()
            .ok_or(RenderError::InvalidState("surface loader отсутствует"))
    }

    fn surface(&self) -> Result<vk::SurfaceKHR, RenderError> {
        self.surface
            .ok_or(RenderError::InvalidState("surface отсутствует"))
    }

    fn device(&self) -> Result<&ash::Device, RenderError> {
        self.device
            .as_ref()
            .ok_or(RenderError::InvalidState("device отсутствует"))
    }

    fn swapchain_loader(&self) -> Result<&ash::khr::swapchain::Device, RenderError> {
        self.swapchain_loader
            .as_ref()
            .ok_or(RenderError::InvalidState("swapchain loader отсутствует"))
    }

    fn command_pool(&self) -> Result<vk::CommandPool, RenderError> {
        self.command_pool
            .ok_or(RenderError::InvalidState("command pool отсутствует"))
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(device) = self.device.as_ref() {
            // SAFETY: teardown waits for renderer submissions before destroying owned Vulkan resources.
            let _ = unsafe { device.device_wait_idle() };
        }
        self.swapchain.take();
        self.frames.clear();
        if let (Some(device), Some(command_pool)) = (self.device.as_ref(), self.command_pool.take())
        {
            // SAFETY: all command buffers and per-frame objects are no longer in use.
            unsafe { device.destroy_command_pool(command_pool, None) };
        }
        if let Some(device) = self.device.take() {
            // SAFETY: swapchain, frame resources and command pool were destroyed first.
            unsafe { device.destroy_device(None) };
        }
        if let (Some(loader), Some(surface)) = (self.surface_loader.as_ref(), self.surface.take()) {
            // SAFETY: the logical device and swapchain are gone; the instance still owns this surface.
            unsafe { loader.destroy_surface(surface, None) };
        }
        self.surface_loader.take();
        if let Some(instance) = self.instance.take() {
            // SAFETY: surface and logical device children have already been destroyed.
            unsafe { instance.destroy_instance(None) };
        }
        // Entry and the Arc-owned window remain fields until after the destructor body completes.
    }
}

struct DeviceChoice {
    physical_device: vk::PhysicalDevice,
    queue_family_index: u32,
    surface_format: vk::SurfaceFormatKHR,
    device_name: String,
    vendor_id: u32,
    api_version: u32,
    discrete: bool,
    tessellation_shader: bool,
}

fn choose_device(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    required_extensions: &[CString],
) -> Result<DeviceChoice, RenderError> {
    // SAFETY: physical-device enumeration is read-only and the instance is alive.
    let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
    let mut best: Option<(i32, DeviceChoice)> = None;
    let mut rejected = Vec::new();
    for physical_device in physical_devices {
        // SAFETY: physical_device was returned by the live instance.
        let properties = unsafe { instance.get_physical_device_properties(physical_device) };
        let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mut reasons = Vec::new();
        if vk::api_version_major(properties.api_version) < 1
            || (vk::api_version_major(properties.api_version) == 1
                && vk::api_version_minor(properties.api_version) < 3)
        {
            reasons.push("Vulkan API ниже 1.3".to_owned());
        }

        // SAFETY: device feature query uses a valid physical device and a properly initialized pNext chain.
        let mut features_13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut features_2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut features_13);
        unsafe { instance.get_physical_device_features2(physical_device, &mut features_2) };
        let tessellation_shader = features_2.features.tessellation_shader != vk::FALSE;
        if features_13.dynamic_rendering == vk::FALSE {
            reasons.push("не поддерживается dynamicRendering".to_owned());
        }
        if features_13.synchronization2 == vk::FALSE {
            reasons.push("не поддерживается synchronization2".to_owned());
        }

        // SAFETY: extension enumeration is read-only for the live physical device.
        let device_extensions =
            unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
        reasons.extend(missing_extensions(
            required_extensions,
            &device_extensions,
            "device",
        ));

        // SAFETY: properties and format support are queried for this physical device.
        let offscreen_properties = unsafe {
            instance.get_physical_device_format_properties(physical_device, OFFSCREEN_FORMAT)
        };
        let offscreen_features = vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::BLIT_SRC
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
        if !offscreen_properties
            .optimal_tiling_features
            .contains(offscreen_features)
        {
            reasons.push("R16G16B16A16_SFLOAT не поддерживает нужные offscreen usage".to_owned());
        }

        // SAFETY: the surface and physical device belong to the same live instance.
        let surface_capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }?;
        if !surface_capabilities
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_DST)
        {
            reasons.push("surface не поддерживает TRANSFER_DST".to_owned());
        }
        // SAFETY: surface query only reads capabilities for the live device and surface.
        let surface_formats = unsafe {
            surface_loader.get_physical_device_surface_formats(physical_device, surface)
        }?;
        // SAFETY: surface query only reads capabilities for the live device and surface.
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
        }?;
        if present_modes.is_empty() || !present_modes.contains(&vk::PresentModeKHR::FIFO) {
            reasons.push("surface не предоставляет FIFO present mode".to_owned());
        }
        let surface_format = choose_surface_format(instance, physical_device, &surface_formats);
        if surface_format.is_none() {
            reasons.push("нет surface-формата с BLIT_DST".to_owned());
        }

        let queue_families =
            // SAFETY: queue-family properties are read from the live physical device.
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let mut queue_family_index = None;
        for (index, family) in queue_families.iter().enumerate() {
            if !family.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                continue;
            }
            // SAFETY: family index is from this device's enumerated queue-family properties.
            if unsafe {
                surface_loader.get_physical_device_surface_support(
                    physical_device,
                    index as u32,
                    surface,
                )
            }? {
                queue_family_index = Some(index as u32);
                break;
            }
        }
        if queue_family_index.is_none() {
            reasons.push("нет общей graphics + present queue family".to_owned());
        }

        if !reasons.is_empty() {
            rejected.push(format!("{name}: {}", reasons.join(", ")));
            continue;
        }
        let device_name = name;
        let choice = DeviceChoice {
            physical_device,
            queue_family_index: queue_family_index.ok_or(RenderError::InvalidState(
                "не найдена очередь после проверки",
            ))?,
            surface_format: surface_format
                .ok_or(RenderError::InvalidState("не найден формат после проверки"))?,
            device_name,
            vendor_id: properties.vendor_id,
            api_version: properties.api_version,
            discrete: properties.device_type == vk::PhysicalDeviceType::DISCRETE_GPU,
            tessellation_shader,
        };
        let score = i32::from(choice.discrete);
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, choice));
        }
    }
    best.map(|(_, choice)| choice)
        .ok_or(RenderError::NoSuitableDevice(rejected))
}

fn choose_surface_format(
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
            .contains(vk::FormatFeatureFlags::BLIT_DST)
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
            .contains(vk::FormatFeatureFlags::BLIT_DST)
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

struct SwapchainBundle {
    instance: ash::Instance,
    device: ash::Device,
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    render_finished: Vec<vk::Semaphore>,
    extent: vk::Extent2D,
}

impl SwapchainBundle {
    #[allow(clippy::too_many_arguments)]
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        loader: &ash::khr::swapchain::Device,
        surface_loader: &ash::khr::surface::Instance,
        physical_device: vk::PhysicalDevice,
        surface: vk::SurfaceKHR,
        surface_format: vk::SurfaceFormatKHR,
        requested_extent: vk::Extent2D,
        old_swapchain: vk::SwapchainKHR,
    ) -> Result<Self, RenderError> {
        // SAFETY: device and surface are alive and associated with this instance.
        let capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }?;
        if !capabilities
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::TRANSFER_DST)
        {
            return Err(RenderError::Unsupported(
                "surface больше не поддерживает TRANSFER_DST".to_owned(),
            ));
        }
        let extent = select_surface_extent(capabilities, requested_extent);
        if extent.width == 0 || extent.height == 0 {
            return Err(RenderError::InvalidConfig("swapchain extent равен нулю"));
        }
        let mut image_count = capabilities.min_image_count.saturating_add(1);
        if capabilities.max_image_count != 0 {
            image_count = image_count.min(capabilities.max_image_count);
        }
        let composite_alpha = select_composite_alpha(capabilities.supported_composite_alpha)
            .ok_or_else(|| {
                RenderError::Unsupported("surface не поддерживает composite alpha".to_owned())
            })?;
        let create_info = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(image_count)
            .image_format(surface_format.format)
            .image_color_space(surface_format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(composite_alpha)
            .present_mode(vk::PresentModeKHR::FIFO)
            .clipped(true)
            .old_swapchain(old_swapchain);
        // SAFETY: all swapchain parameters were checked against the current surface capabilities.
        let handle = unsafe { loader.create_swapchain(&create_info, None) }?;
        let mut bundle = Self {
            instance: instance.clone(),
            device: device.clone(),
            handle,
            images: Vec::new(),
            render_finished: Vec::new(),
            extent,
        };
        // SAFETY: the swapchain was just created by this loader and is still alive.
        bundle.images = unsafe { loader.get_swapchain_images(bundle.handle) }?;
        for _ in &bundle.images {
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
            // SAFETY: the swapchain is idle before bundle replacement/destruction.
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
        if self.handle != vk::SwapchainKHR::null() {
            let loader = ash::khr::swapchain::Device::new(&self.instance, &self.device);
            // SAFETY: swapchain semaphores were destroyed and no renderer work is active.
            unsafe { loader.destroy_swapchain(self.handle, None) };
            self.handle = vk::SwapchainKHR::null();
        }
    }
}

struct FrameResources {
    device: ash::Device,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    image_available: vk::Semaphore,
    target: Option<OffscreenTarget>,
}

impl FrameResources {
    fn new(device: &ash::Device, command_buffer: vk::CommandBuffer) -> Result<Self, RenderError> {
        let mut frame = Self {
            device: device.clone(),
            command_buffer,
            fence: vk::Fence::null(),
            image_available: vk::Semaphore::null(),
            target: None,
        };
        // SAFETY: creating an unsignaled binary semaphore uses a valid default create info.
        frame.image_available =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: the fence starts signaled so the first frame can wait before using the slot.
        frame.fence = unsafe { device.create_fence(&fence_info, None) }?;
        Ok(frame)
    }
}

impl Drop for FrameResources {
    fn drop(&mut self) {
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

struct OffscreenTarget {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    layout: vk::ImageLayout,
}

impl OffscreenTarget {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(OFFSCREEN_FORMAT)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: image parameters use a supported optimal format and non-zero extent.
        let image = unsafe { device.create_image(&image_info, None) }?;
        let mut target = Self {
            device: device.clone(),
            image,
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            layout: vk::ImageLayout::UNDEFINED,
        };
        // SAFETY: image was created by this device and remains alive.
        let requirements = unsafe { device.get_image_memory_requirements(target.image) };
        // SAFETY: memory properties are queried from the live physical device.
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let memory_type = (0..memory_properties.memory_type_count)
            .find(|index| {
                requirements.memory_type_bits & (1 << index) != 0
                    && memory_properties.memory_types[*index as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
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
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(OFFSCREEN_FORMAT)
            .components(vk::ComponentMapping::default())
            .subresource_range(color_subresource_range());
        // SAFETY: image view matches the bound image format and its single color subresource.
        target.view = unsafe { device.create_image_view(&view_info, None) }?;
        Ok(target)
    }
}

impl Drop for OffscreenTarget {
    fn drop(&mut self) {
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

fn record_frame(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut OffscreenTarget,
    output_image: vk::Image,
    output_extent: vk::Extent2D,
    render_extent: vk::Extent2D,
    clear_color: [f32; 4],
) -> Result<(), RenderError> {
    // SAFETY: this frame's command buffer is no longer in flight because its fence was waited.
    unsafe { device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty()) }?;
    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the command buffer was reset and belongs to the renderer's graphics command pool.
    unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;

    let scene_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(if target.layout == vk::ImageLayout::UNDEFINED {
            vk::PipelineStageFlags2::NONE
        } else {
            vk::PipelineStageFlags2::TRANSFER
        })
        .src_access_mask(if target.layout == vk::ImageLayout::UNDEFINED {
            vk::AccessFlags2::NONE
        } else {
            vk::AccessFlags2::TRANSFER_READ
        })
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(target.layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(target.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, scene_barrier);

    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue {
                float32: clear_color,
            },
        })];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: render_extent,
        })
        .layer_count(1)
        .color_attachments(&color_attachment);
    // SAFETY: the dynamic-rendering attachment view covers the configured render extent.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };

    let source_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(target.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, source_barrier);
    record_output_pass(
        device,
        command_buffer,
        target.image,
        output_image,
        render_extent,
        output_extent,
    );
    // SAFETY: command recording is complete before the command buffer is submitted.
    unsafe { device.end_command_buffer(command_buffer) }?;
    target.layout = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
    Ok(())
}

fn record_output_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source_image: vk::Image,
    output_image: vk::Image,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
) {
    // TRANSFER связывает переход layout с ожиданием acquire на этой стадии.
    let output_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, output_barrier);

    let region = [vk::ImageBlit2::default()
        .src_subresource(color_subresource_layers())
        .src_offsets([
            vk::Offset3D { x: 0, y: 0, z: 0 },
            vk::Offset3D {
                x: render_extent.width as i32,
                y: render_extent.height as i32,
                z: 1,
            },
        ])
        .dst_subresource(color_subresource_layers())
        .dst_offsets([
            vk::Offset3D { x: 0, y: 0, z: 0 },
            vk::Offset3D {
                x: output_extent.width as i32,
                y: output_extent.height as i32,
                z: 1,
            },
        ])];
    let blit_info = vk::BlitImageInfo2::default()
        .src_image(source_image)
        .src_image_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .dst_image(output_image)
        .dst_image_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .regions(&region)
        .filter(vk::Filter::LINEAR);
    // SAFETY: source/destination images support the requested blit formats and layouts.
    unsafe { device.cmd_blit_image2(command_buffer, &blit_info) };

    let present_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
        .dst_access_mask(vk::AccessFlags2::NONE)
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, present_barrier);
}

fn pipeline_barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    barrier: vk::ImageMemoryBarrier2<'_>,
) {
    let barriers = [barrier];
    let dependency = vk::DependencyInfo::default().image_memory_barriers(&barriers);
    // SAFETY: barriers reference images owned by this renderer and commands execute on the same queue family.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn color_subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .mip_level(0)
        .base_array_layer(0)
        .layer_count(1)
}

fn deduplicate_names(names: Vec<CString>) -> Vec<CString> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.as_bytes().to_vec()))
        .collect()
}

fn check_extensions(
    requested: &[CString],
    available: &[vk::ExtensionProperties],
    kind: &str,
) -> Result<(), RenderError> {
    let missing = missing_extensions(requested, available, kind);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(RenderError::Unsupported(missing.join(", ")))
    }
}

fn missing_extensions(
    requested: &[CString],
    available: &[vk::ExtensionProperties],
    kind: &str,
) -> Vec<String> {
    let available_names = available
        .iter()
        .map(|extension| {
            // SAFETY: Vulkan extension property names are null-terminated fixed-size strings.
            unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }
                .to_bytes()
                .to_vec()
        })
        .collect::<HashSet<_>>();
    requested
        .iter()
        .filter(|name| !available_names.contains(name.as_bytes()))
        .map(|name| format!("{} extension недоступен: {}", kind, name.to_string_lossy()))
        .collect()
}

fn validate_render_scale(scale: f32) -> Result<(), RenderError> {
    if scale.is_finite() && scale > 0.0 && scale <= 1.0 {
        Ok(())
    } else {
        Err(RenderError::InvalidConfig(
            "render_scale должен быть конечным и находиться в (0, 1]",
        ))
    }
}

fn scaled_extent(output: vk::Extent2D, scale: f32) -> vk::Extent2D {
    let scale = f64::from(scale);
    vk::Extent2D {
        width: ((f64::from(output.width) * scale).floor() as u32).max(1),
        height: ((f64::from(output.height) * scale).floor() as u32).max(1),
    }
}

fn select_surface_extent(
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

fn select_composite_alpha(
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
    use super::{
        RenderError, RendererConfig, check_extensions, deduplicate_names, missing_extensions,
        scaled_extent, select_composite_alpha, select_surface_extent, validate_render_scale,
    };
    use ash::vk;
    use std::ffi::{CStr, CString};

    fn extension(name: &CStr) -> vk::ExtensionProperties {
        let mut property = vk::ExtensionProperties::default();
        for (destination, source) in property
            .extension_name
            .iter_mut()
            .zip(name.to_bytes_with_nul())
        {
            *destination = *source as _;
        }
        property
    }

    #[test]
    fn default_config_uses_native_resolution_without_extra_extensions() {
        let config = RendererConfig::default();
        assert_eq!(config.render_scale, 1.0);
        assert!(validate_render_scale(config.render_scale).is_ok());
        assert!(config.extra_instance_extensions.is_empty());
        assert!(config.extra_device_extensions.is_empty());
    }

    #[test]
    fn extension_names_are_deduplicated_preserving_request_order() {
        let first = CString::new("VK_KHR_surface").unwrap();
        let second = CString::new("VK_KHR_swapchain").unwrap();
        assert_eq!(
            deduplicate_names(vec![
                first.clone(),
                second.clone(),
                first.clone(),
                second.clone(),
            ]),
            vec![first, second]
        );
        assert!(deduplicate_names(Vec::new()).is_empty());
    }

    #[test]
    fn extension_checks_accept_supported_and_empty_requests() {
        let available = [extension(c"VK_KHR_surface"), extension(c"VK_KHR_swapchain")];
        let requested = [c"VK_KHR_swapchain".to_owned()];

        assert!(check_extensions(&requested, &available, "device").is_ok());
        assert!(check_extensions(&[], &available, "instance").is_ok());
        assert!(check_extensions(&[], &[], "device").is_ok());
    }

    #[test]
    fn missing_extensions_report_all_missing_names_and_their_kind() {
        let available = [extension(c"VK_KHR_surface")];
        let requested = [
            c"VK_KHR_surface".to_owned(),
            c"VK_KHR_swapchain".to_owned(),
            c"VK_KHR_portability_subset".to_owned(),
        ];
        let missing = missing_extensions(&requested, &available, "device");

        assert_eq!(missing.len(), 2);
        assert!(missing[0].contains("device"));
        assert!(missing[0].contains("VK_KHR_swapchain"));
        assert!(missing[1].contains("VK_KHR_portability_subset"));
        match check_extensions(&requested, &available, "device") {
            Err(RenderError::Unsupported(message)) => {
                for diagnostic in missing {
                    assert!(message.contains(&diagnostic));
                }
            }
            result => panic!("expected unsupported extensions, got {result:?}"),
        }
        assert_eq!(missing_extensions(&requested, &[], "device").len(), 3);
    }

    #[test]
    fn extension_names_require_an_exact_match() {
        let available = [extension(c"VK_KHR_surface_extra")];
        let requested = [c"VK_KHR_surface".to_owned()];
        assert!(matches!(
            check_extensions(&requested, &available, "instance"),
            Err(RenderError::Unsupported(_))
        ));
    }

    #[test]
    fn composite_alpha_prefers_opaque_then_supported_fallbacks() {
        let modes = [
            vk::CompositeAlphaFlagsKHR::OPAQUE,
            vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            vk::CompositeAlphaFlagsKHR::INHERIT,
        ];
        for index in 0..modes.len() {
            let supported = modes[index..]
                .iter()
                .copied()
                .fold(vk::CompositeAlphaFlagsKHR::empty(), |all, mode| all | mode);
            assert_eq!(select_composite_alpha(supported), Some(modes[index]));
        }
        assert_eq!(
            select_composite_alpha(vk::CompositeAlphaFlagsKHR::empty()),
            None
        );
    }

    #[test]
    fn native_scale_preserves_large_output_dimensions() {
        let output = vk::Extent2D {
            width: u32::MAX,
            height: u32::MAX - 1,
        };
        assert_eq!(scaled_extent(output, 1.0), output);
    }

    #[test]
    fn variable_surface_extent_preserves_sizes_inside_limits() {
        let capabilities = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: u32::MAX,
                height: u32::MAX,
            },
            min_image_extent: vk::Extent2D {
                width: 100,
                height: 100,
            },
            max_image_extent: vk::Extent2D {
                width: 1920,
                height: 1080,
            },
            ..Default::default()
        };
        for size in [
            capabilities.min_image_extent,
            vk::Extent2D {
                width: 1280,
                height: 720,
            },
            capabilities.max_image_extent,
        ] {
            assert_eq!(select_surface_extent(capabilities, size), size);
        }
    }

    #[test]
    fn render_scale_accepts_finite_values_in_range() {
        assert!(validate_render_scale(1.0).is_ok());
        assert!(validate_render_scale(0.5).is_ok());
        assert!(validate_render_scale(f32::MIN_POSITIVE).is_ok());
    }

    #[test]
    fn render_scale_rejects_zero_nan_and_infinity() {
        for scale in [0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.01, -0.1] {
            assert!(matches!(
                validate_render_scale(scale),
                Err(RenderError::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn scaled_extent_floors_and_clamps_dimensions_to_one() {
        assert_eq!(
            scaled_extent(
                vk::Extent2D {
                    width: 1280,
                    height: 721
                },
                0.5
            ),
            vk::Extent2D {
                width: 640,
                height: 360
            }
        );
        assert_eq!(
            scaled_extent(
                vk::Extent2D {
                    width: 1,
                    height: 0
                },
                0.01
            ),
            vk::Extent2D {
                width: 1,
                height: 1
            }
        );
    }

    #[test]
    fn surface_extent_uses_fixed_extent_or_clamps_requested_size() {
        let capabilities = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: u32::MAX,
                height: u32::MAX,
            },
            min_image_extent: vk::Extent2D {
                width: 100,
                height: 100,
            },
            max_image_extent: vk::Extent2D {
                width: 800,
                height: 600,
            },
            ..Default::default()
        };
        assert_eq!(
            select_surface_extent(
                capabilities,
                vk::Extent2D {
                    width: 50,
                    height: 900,
                }
            ),
            vk::Extent2D {
                width: 100,
                height: 600,
            }
        );
        let fixed = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: 1920,
                height: 1080,
            },
            ..capabilities
        };
        assert_eq!(
            select_surface_extent(
                fixed,
                vk::Extent2D {
                    width: 1280,
                    height: 720,
                }
            ),
            vk::Extent2D {
                width: 1920,
                height: 1080,
            }
        );
    }
}
