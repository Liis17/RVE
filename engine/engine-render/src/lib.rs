//! Vulkan-рендерер, форматы изображений и материалы сцены.

use ash::{Entry, vk};
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

const FRAMES_IN_FLIGHT: usize = 2;
const OFFSCREEN_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
const TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const SHADOW_MAP_SIZE: u32 = 2048;
const MESH_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mesh.vert.spv"));
const MESH_FRAGMENT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mesh.frag.spv"));
const SHADOW_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow.vert.spv"));
const OVERLAY_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay.vert.spv"));
const OVERLAY_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/overlay.frag.spv"));
/// Swapchain принимает blit кадра и затем рисование оверлея.
const SWAPCHAIN_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::TRANSFER_DST.as_raw() | vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw(),
);
const SURFACE_FORMAT_FEATURES: vk::FormatFeatureFlags = vk::FormatFeatureFlags::from_raw(
    vk::FormatFeatureFlags::BLIT_DST.as_raw()
        | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND.as_raw(),
);
const OVERLAY_PUSH_CONSTANTS_SIZE: usize = size_of::<[f32; 2]>();
/// Два timestamp на слот кадра: перед shadow pass и после scene pass.
const TIMESTAMP_QUERY_COUNT: u32 = 2;
/// Белая текстура 1x1 создаётся при инициализации и используется для `texture: None`.
const WHITE_TEXTURE: usize = 0;
const PUSH_CONSTANTS_SIZE: usize = 2 * size_of::<[[f32; 4]; 4]>();
const FRAME_UNIFORMS_SIZE: usize = 2 * size_of::<[[f32; 4]; 4]>() + 4 * size_of::<[f32; 4]>();
const IDENTITY_MATRIX: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// Вершина меша в пространстве модели.
///
/// `uv` отсчитывается от левого верхнего угла текстуры, `color` умножается
/// на цвет текстуры в линейном RGB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 3],
}

/// Вершина 2D-оверлея, рисуемого поверх кадра в разрешении окна.
///
/// `position` задаётся в физических пикселях от левого верхнего угла окна,
/// `color` — линейный RGB с непредумноженной альфой.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OverlayVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

/// Идентификатор меша, созданного [`Renderer::create_mesh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MeshId(usize);

/// Идентификатор текстуры, созданной [`Renderer::create_texture`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextureId(usize);

/// Команда рисования меша.
///
/// `model` переводит пространство модели в мировое и задаётся по столбцам.
/// Без текстуры меш рисуется белой текстурой, то есть цветом вершин.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawCommand {
    pub mesh: MeshId,
    pub texture: Option<TextureId>,
    pub model: [[f32; 4]; 4],
}

/// Направленный свет и фоновое освещение сцены в линейном RGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalLight {
    /// Направление лучей в мировом пространстве; длина не важна, но не может быть нулевой.
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub ambient: [f32; 3],
}

impl Default for DirectionalLight {
    /// Свет сверху слева со стороны камеры, смотрящей вдоль -Z.
    fn default() -> Self {
        Self {
            direction: [0.4, -0.7, -0.6],
            color: [1.0, 1.0, 1.0],
            ambient: [0.15, 0.15, 0.15],
        }
    }
}

/// Описание кадра для [`Renderer::render_frame`].
///
/// `view_projection` переводит мировое пространство в clip space и задаётся по столбцам.
/// Ось Y clip space направлена вверх, глубина после деления на `w` лежит в `[0, 1]`.
/// `shadow_view_projection`, если задана, переводит мировое пространство в clip space света
/// с той же конвенцией Y-up и глубиной `[0, 1]`; `None` отключает отбрасывание теней.
/// `overlay` — треугольники (по три вершины), которые рисуются с альфа-смешиванием
/// поверх итогового изображения без масштабирования `render_scale`; пустой срез
/// отключает оверлей.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub clear_color: [f32; 4],
    pub view_projection: [[f32; 4]; 4],
    pub shadow_view_projection: Option<[[f32; 4]; 4]>,
    pub light: DirectionalLight,
    pub draws: &'a [DrawCommand],
    pub overlay: &'a [OverlayVertex],
}

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
    InvalidMesh(&'static str),
    InvalidTexture(String),
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
            Self::InvalidMesh(message) => write!(f, "некорректный меш: {message}"),
            Self::InvalidTexture(message) => write!(f, "некорректная текстура: {message}"),
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
    pipeline_layout: Option<vk::PipelineLayout>,
    pipeline: Option<vk::Pipeline>,
    shadow_pipeline: Option<vk::Pipeline>,
    overlay_pipeline_layout: Option<vk::PipelineLayout>,
    overlay_pipeline: Option<vk::Pipeline>,
    /// Значащие биты timestamp очереди; ноль — измерение времени GPU недоступно.
    timestamp_valid_bits: u32,
    /// Наносекунд на один тик timestamp.
    timestamp_period: f32,
    pending_gpu_time: Option<Duration>,
    frame_set_layout: Option<vk::DescriptorSetLayout>,
    texture_set_layout: Option<vk::DescriptorSetLayout>,
    frame_descriptor_pool: Option<vk::DescriptorPool>,
    frame_uniforms: Vec<FrameUniforms>,
    sampler: Option<vk::Sampler>,
    shadow_sampler: Option<vk::Sampler>,
    shadow_image: Option<DeviceImage>,
    max_texture_size: u32,
    meshes: Vec<MeshBuffers>,
    textures: Vec<Texture>,
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
            pipeline_layout: None,
            pipeline: None,
            shadow_pipeline: None,
            overlay_pipeline_layout: None,
            overlay_pipeline: None,
            timestamp_valid_bits: 0,
            timestamp_period: 0.0,
            pending_gpu_time: None,
            frame_set_layout: None,
            texture_set_layout: None,
            frame_descriptor_pool: None,
            frame_uniforms: Vec::new(),
            sampler: None,
            shadow_sampler: None,
            shadow_image: None,
            max_texture_size: 0,
            meshes: Vec::new(),
            textures: Vec::new(),
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

    /// Забирает GPU-время scene pass последнего завершённого кадра.
    ///
    /// Каждое измерение возвращается один раз; `None` — нового измерения нет
    /// или очередь не поддерживает timestamp queries. Вывод в окно и оверлей
    /// не учитываются: они ждут готовности swapchain image.
    pub fn take_gpu_frame_time(&mut self) -> Option<Duration> {
        self.pending_gpu_time.take()
    }

    /// Отмечает физический размер клиентской области как изменённый.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.output_extent = vk::Extent2D { width, height };
        self.swapchain_dirty = true;
    }

    /// Загружает треугольный меш в память GPU. Каждые три индекса образуют треугольник.
    pub fn create_mesh(
        &mut self,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<MeshId, RenderError> {
        validate_mesh(vertices.len(), indices)?;
        let vertex_buffer = HostBuffer::new(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            vertex_bytes(vertices),
        )?;
        let index_buffer = HostBuffer::new(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::BufferUsageFlags::INDEX_BUFFER,
            index_bytes(indices),
        )?;
        self.meshes.push(MeshBuffers {
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
        });
        Ok(MeshId(self.meshes.len() - 1))
    }

    /// Загружает sRGB-текстуру RGBA8 со строками сверху вниз и ждёт завершения копирования.
    pub fn create_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<TextureId, RenderError> {
        validate_texture(width, height, rgba.len(), self.max_texture_size)?;
        let texture = self.upload_texture(width, height, rgba)?;
        self.textures.push(texture);
        Ok(TextureId(self.textures.len() - 1))
    }

    /// Очищает offscreen-таргет, рисует меши и масштабирует результат в swapchain.
    pub fn render_frame(&mut self, frame: &Frame<'_>) -> Result<FrameStatus, RenderError> {
        validate_draws(frame.draws, self.meshes.len(), self.textures.len())?;
        validate_overlay(frame.overlay)?;
        let uniforms = frame_uniform_bytes(frame)?;
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            return Ok(FrameStatus::Skipped);
        }
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.recreate_swapchain()?;
            return Ok(FrameStatus::Skipped);
        }

        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let swapchain_loader = self.swapchain_loader()?.clone();
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let frame_slot = self.frame_slot;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let fence = frame_resources.fence;
        let image_available = frame_resources.image_available;
        let command_buffer = frame_resources.command_buffer;
        let swapchain_handle = swapchain.handle;
        let image_count = swapchain.images.len();

        // SAFETY: fence принадлежит текущему слоту и предыдущая работа этого слота завершается перед повторным использованием.
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }?;
        if let Some(gpu_time) = frame_resources.read_gpu_time(
            &device,
            self.timestamp_valid_bits,
            self.timestamp_period,
        )? {
            self.pending_gpu_time = Some(gpu_time);
        }
        // Fence слота дождались, поэтому GPU больше не читает вершины оверлея этого слота.
        frame_resources.write_overlay(&instance, &device, self.physical_device, frame.overlay)?;
        let timestamps = frame_resources.timestamps;
        let frame_uniforms =
            self.frame_uniforms
                .get(frame_slot)
                .ok_or(RenderError::InvalidState(
                    "нет uniform-буфера текущего кадра",
                ))?;
        // Fence слота дождались, поэтому GPU больше не читает этот буфер.
        frame_uniforms.buffer.write(&uniforms)?;
        let frame_set = frame_uniforms.set;
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
        let shadow = ShadowPass {
            pipeline: self
                .shadow_pipeline
                .ok_or(RenderError::InvalidState("shadow pipeline отсутствует"))?,
            pipeline_layout: self
                .pipeline_layout
                .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?,
            frame_set,
            meshes: &self.meshes,
            draws: frame.draws,
            enabled: frame.shadow_view_projection.is_some(),
        };
        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let scene = ScenePass {
            pipeline: self
                .pipeline
                .ok_or(RenderError::InvalidState("pipeline отсутствует"))?,
            pipeline_layout: self
                .pipeline_layout
                .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?,
            frame_set,
            meshes: &self.meshes,
            textures: &self.textures,
            draws: frame.draws,
            clear_color: frame.clear_color,
            timestamps,
        };
        let frame_resources = &mut self.frames[frame_slot];
        let overlay =
            match &frame_resources.overlay_buffer {
                Some(buffer) if !frame.overlay.is_empty() => Some(OverlayPass {
                    pipeline: self
                        .overlay_pipeline
                        .ok_or(RenderError::InvalidState("overlay pipeline отсутствует"))?,
                    pipeline_layout: self.overlay_pipeline_layout.ok_or(
                        RenderError::InvalidState("overlay pipeline layout отсутствует"),
                    )?,
                    vertex_buffer: buffer.buffer,
                    // validate_overlay проверил, что число вершин помещается в u32.
                    vertex_count: frame.overlay.len() as u32,
                    view: swapchain.views[image_index],
                }),
                _ => None,
            };
        let target = frame_resources
            .target
            .as_mut()
            .ok_or(RenderError::InvalidState("offscreen target отсутствует"))?;
        record_frame(
            &device,
            command_buffer,
            target,
            shadow_image,
            swapchain.images[image_index],
            swapchain.extent,
            offscreen_extent,
            &shadow,
            &scene,
            overlay.as_ref(),
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
        // Результаты timestamp читаются после следующего ожидания fence этого слота.
        self.frames[frame_slot].timestamps_pending = timestamps.is_some();

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
        self.timestamp_valid_bits = choice.timestamp_valid_bits;
        self.timestamp_period = choice.timestamp_period;
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
        // SAFETY: physical_device was selected from this live instance.
        let limits = unsafe {
            self.instance()?
                .get_physical_device_properties(self.physical_device)
        }
        .limits;
        self.max_texture_size = limits.max_image_dimension2_d;
        self.create_frame_resources()?;
        self.create_shadow_resources()?;
        self.create_descriptor_resources()?;
        self.create_mesh_pipeline()?;
        self.create_shadow_pipeline()?;
        self.create_overlay_pipeline()?;
        let white = self.upload_texture(1, 1, &[255; 4])?;
        self.textures.push(white);

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
        let timestamps = self.timestamp_valid_bits != 0;
        for command_buffer in command_buffers {
            self.frames.push(FrameResources::new(
                self.device()?,
                command_buffer,
                timestamps,
            )?);
        }
        Ok(())
    }

    /// Создаёт общие layout-ы и per-frame uniform-буферы с дескрипторами shadow map.
    fn create_descriptor_resources(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let frame_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT),
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
        ];
        let frame_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&frame_bindings);
        // SAFETY: bindings match the shared frame uniform and sampled shadow map shaders.
        self.frame_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&frame_layout_info, None) }?);
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
        // SAFETY: the bindings describe a fragment-stage sampled image and sampler.
        self.texture_set_layout =
            Some(unsafe { device.create_descriptor_set_layout(&texture_layout_info, None) }?);

        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::REPEAT)
            .address_mode_v(vk::SamplerAddressMode::REPEAT)
            .address_mode_w(vk::SamplerAddressMode::REPEAT)
            .max_lod(0.0);
        // SAFETY: the sampler uses core filtering and addressing modes without anisotropy.
        self.sampler = Some(unsafe { device.create_sampler(&sampler_info, None) }?);

        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let shadow_image_info = [vk::DescriptorImageInfo::default()
            .image_view(shadow_image.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let shadow_sampler_info = [vk::DescriptorImageInfo::default().sampler(
            self.shadow_sampler
                .ok_or(RenderError::InvalidState("shadow sampler отсутствует"))?,
        )];
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(self.frames.len() as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(self.frames.len() as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(self.frames.len() as u32),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(self.frames.len() as u32)
            .pool_sizes(&pool_sizes);
        // SAFETY: the pool holds one uniform, sampled image and sampler set per frame slot.
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
            let buffer_info = [vk::DescriptorBufferInfo::default()
                .buffer(buffer.buffer)
                .offset(0)
                .range(FRAME_UNIFORMS_SIZE as vk::DeviceSize)];
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
            ];
            // SAFETY: the set and buffer are alive and no command buffer uses the set yet.
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            self.frame_uniforms.push(FrameUniforms { buffer, set });
        }
        Ok(())
    }

    /// Создаёт единственную общую depth-карту и sampler для всех слотов кадров.
    fn create_shadow_resources(&mut self) -> Result<(), RenderError> {
        let image = DeviceImage::new(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::Extent2D {
                width: SHADOW_MAP_SIZE,
                height: SHADOW_MAP_SIZE,
            },
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            depth_subresource_range(),
        )?;
        self.shadow_image = Some(image);
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        // SAFETY: the sampler uses core nearest filtering and clamp-to-edge addressing.
        self.shadow_sampler = Some(unsafe { self.device()?.create_sampler(&sampler_info, None) }?);
        Ok(())
    }

    /// Копирует пиксели через staging-буфер в device-local image и создаёт её дескрипторы.
    fn upload_texture(&self, width: u32, height: u32, rgba: &[u8]) -> Result<Texture, RenderError> {
        let device = self.device()?;
        let extent = vk::Extent2D { width, height };
        let image = DeviceImage::new(
            self.instance()?,
            device,
            self.physical_device,
            extent,
            TEXTURE_FORMAT,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            color_subresource_range(),
        )?;
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            rgba,
        )?;
        self.submit_once(|command_buffer| {
            let to_transfer = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image.image)
                .subresource_range(color_subresource_range());
            pipeline_barrier(device, command_buffer, to_transfer);
            let region = [vk::BufferImageCopy::default()
                .buffer_offset(0)
                .image_subresource(color_subresource_layers())
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            // SAFETY: the staging buffer holds width * height RGBA8 texels for this image.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                )
            };
            let to_shader = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image.image)
                .subresource_range(color_subresource_range());
            pipeline_barrier(device, command_buffer, to_shader);
        })?;
        drop(staging);

        let mut texture = Texture {
            device: device.clone(),
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            image,
        };
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
        // SAFETY: the pool holds the single descriptor set of this texture.
        texture.pool = unsafe { device.create_descriptor_pool(&pool_info, None) }?;
        let layouts = [self.texture_set_layout()?];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(texture.pool)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for one set of this layout.
        texture.set = unsafe { device.allocate_descriptor_sets(&allocate_info) }?[0];
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(texture.image.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let sampler_info = [vk::DescriptorImageInfo::default().sampler(
            self.sampler
                .ok_or(RenderError::InvalidState("sampler отсутствует"))?,
        )];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(texture.set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&image_info),
            vk::WriteDescriptorSet::default()
                .dst_set(texture.set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&sampler_info),
        ];
        // SAFETY: the set, image view and sampler are alive and the set is not in use.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(texture)
    }

    /// Записывает одноразовый command buffer, отправляет его и ждёт завершения.
    fn submit_once(&self, record: impl FnOnce(vk::CommandBuffer)) -> Result<(), RenderError> {
        let device = self.device()?;
        let command_pool = self.command_pool()?;
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the command pool belongs to this device and is used from this thread only.
        let command_buffer = unsafe { device.allocate_command_buffers(&allocate_info) }?[0];
        let result = (|| {
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: the command buffer was just allocated and is in the initial state.
            unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
            record(command_buffer);
            // SAFETY: recording started above and all commands have been recorded.
            unsafe { device.end_command_buffer(command_buffer) }?;
            // SAFETY: a default fence create info produces an unsignaled fence.
            let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
            let command_infos = [vk::CommandBufferSubmitInfo::default()
                .command_buffer(command_buffer)
                .device_mask(0)];
            let submit_info = [vk::SubmitInfo2::default().command_buffer_infos(&command_infos)];
            // SAFETY: the queue is used only by this renderer thread and the fence is unsignaled.
            let submitted = unsafe { device.queue_submit2(self.queue, &submit_info, fence) }
                // SAFETY: waiting on a fence passed to a successful submit.
                .and_then(|()| unsafe { device.wait_for_fences(&[fence], true, u64::MAX) });
            // SAFETY: the fence is either unsubmitted or already signaled.
            unsafe { device.destroy_fence(fence, None) };
            submitted.map_err(RenderError::from)
        })();
        if result.is_ok() {
            // SAFETY: the submission has completed, so the command buffer is no longer in use.
            unsafe { device.free_command_buffers(command_pool, &[command_buffer]) };
        }
        result
    }

    fn create_mesh_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(PUSH_CONSTANTS_SIZE as u32)];
        let set_layouts = [self.frame_set_layout()?, self.texture_set_layout()?];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the vertex push-constant range fits in the guaranteed 128 bytes.
        self.pipeline_layout = Some(unsafe { device.create_pipeline_layout(&layout_info, None) }?);

        let vertex_module = ShaderModule::new(&device, MESH_VERTEX_SHADER)?;
        let fragment_module = ShaderModule::new(&device, MESH_FRAGMENT_SHADER)?;
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
        let vertex_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, normal) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, uv) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(3)
                .binding(0)
                .format(vk::Format::R32G32B32_SFLOAT)
                .offset(std::mem::offset_of!(Vertex, color) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        // Отсечение граней выключено: видимость не зависит от порядка обхода вершин.
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [OFFSCREEN_FORMAT];
        let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_state)
            .layout(
                self.pipeline_layout
                    .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?,
            )
            .push_next(&mut rendering_info)];
        // SAFETY: shader modules, layout and dynamic-rendering formats are valid for this device.
        let pipelines = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
        }
        .map_err(|(_, error)| RenderError::Vulkan(error))?;
        self.pipeline = pipelines.into_iter().next();
        Ok(())
    }

    /// Pipeline для depth-only shadow pass с фиксированным depth bias.
    fn create_shadow_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let vertex_module = ShaderModule::new(&device, SHADOW_VERTEX_SHADER)?;
        let stages = [vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main")];
        let vertex_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<Vertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [vk::VertexInputAttributeDescription::default()
            .location(0)
            .binding(0)
            .format(vk::Format::R32G32B32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, position) as u32)];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_bias_enable(true)
            .depth_bias_constant_factor(1.25)
            .depth_bias_slope_factor(1.75)
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let mut rendering_info =
            vk::PipelineRenderingCreateInfo::default().depth_attachment_format(DEPTH_FORMAT);
        let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .depth_stencil_state(&depth_stencil)
            .dynamic_state(&dynamic_state)
            .layout(
                self.pipeline_layout
                    .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?,
            )
            .push_next(&mut rendering_info)];
        // SAFETY: shader, shared layout, depth format and dynamic-rendering state are valid.
        let mut pipelines = match unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
        } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: these partial pipeline handles were returned by this device.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(RenderError::Vulkan(error));
            }
        };
        self.shadow_pipeline = pipelines.pop();
        if self.shadow_pipeline.is_none() {
            return Err(RenderError::InvalidState("shadow pipeline не создан"));
        }
        Ok(())
    }

    /// Pipeline оверлея: 2D-треугольники с альфа-смешиванием прямо в swapchain image.
    fn create_overlay_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(OVERLAY_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info =
            vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push_constant_ranges);
        // SAFETY: the layout has a single small vertex push-constant range and no descriptor sets.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.overlay_pipeline_layout = Some(layout);

        let vertex_module = ShaderModule::new(&device, OVERLAY_VERTEX_SHADER)?;
        let fragment_module = ShaderModule::new(&device, OVERLAY_FRAGMENT_SHADER)?;
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
        let vertex_bindings = [vk::VertexInputBindingDescription::default()
            .binding(0)
            .stride(size_of::<OverlayVertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(OverlayVertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32B32A32_SFLOAT)
                .offset(std::mem::offset_of!(OverlayVertex, color) as u32),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&vertex_bindings)
            .vertex_attribute_descriptions(&vertex_attributes);
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
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [self.surface_format.format];
        let mut rendering_info =
            vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&color_formats);
        let pipeline_info = [vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&rasterization)
            .multisample_state(&multisample)
            .color_blend_state(&color_blend)
            .dynamic_state(&dynamic_state)
            .layout(layout)
            .push_next(&mut rendering_info)];
        // SAFETY: shader modules, layout and the surface color format are valid for this device.
        let pipelines = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
        }
        .map_err(|(_, error)| RenderError::Vulkan(error))?;
        self.overlay_pipeline = pipelines.into_iter().next();
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

    fn frame_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.frame_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов кадра отсутствует",
        ))
    }

    fn texture_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.texture_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов текстуры отсутствует",
        ))
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
        self.meshes.clear();
        self.textures.clear();
        self.frame_uniforms.clear();
        if let Some(device) = self.device.as_ref() {
            if let Some(pool) = self.frame_descriptor_pool.take() {
                // SAFETY: the device is idle; destroying the pool frees its frame sets.
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            if let Some(sampler) = self.sampler.take() {
                // SAFETY: every texture descriptor referencing the sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.shadow_sampler.take() {
                // SAFETY: the frame descriptor pool referencing the sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(image) = self.shadow_image.take() {
                // SAFETY: the frame descriptor pool is gone and the device is idle.
                drop(image);
            }
            if let Some(pipeline) = self.shadow_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(pipeline) = self.pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(pipeline) = self.overlay_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.overlay_pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            for layout in [self.frame_set_layout.take(), self.texture_set_layout.take()]
                .into_iter()
                .flatten()
            {
                // SAFETY: the pipeline layout and all descriptor pools using it are gone.
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
        }
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
    timestamp_valid_bits: u32,
    timestamp_period: f32,
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
        // SAFETY: format support is queried for this physical device.
        let depth_properties = unsafe {
            instance.get_physical_device_format_properties(physical_device, DEPTH_FORMAT)
        };
        let depth_features = vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE;
        if !depth_properties
            .optimal_tiling_features
            .contains(depth_features)
        {
            reasons.push(
                "D32_SFLOAT не поддерживает depth attachment и выборку shadow map".to_owned(),
            );
        }
        // SAFETY: format support is queried for this physical device.
        let texture_properties = unsafe {
            instance.get_physical_device_format_properties(physical_device, TEXTURE_FORMAT)
        };
        let texture_features = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_DST;
        if !texture_properties
            .optimal_tiling_features
            .contains(texture_features)
        {
            reasons.push("R8G8B8A8_SRGB не поддерживает выборку текстур".to_owned());
        }

        // SAFETY: the surface and physical device belong to the same live instance.
        let surface_capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }?;
        if !surface_capabilities
            .supported_usage_flags
            .contains(SWAPCHAIN_USAGE)
        {
            reasons.push("surface не поддерживает TRANSFER_DST и COLOR_ATTACHMENT".to_owned());
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
            reasons.push("нет surface-формата с BLIT_DST и COLOR_ATTACHMENT_BLEND".to_owned());
        }

        let queue_families =
            // SAFETY: queue-family properties are read from the live physical device.
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let mut queue_family_index = None;
        let mut timestamp_valid_bits = 0;
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
                timestamp_valid_bits = family.timestamp_valid_bits;
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
            timestamp_valid_bits,
            timestamp_period: properties.limits.timestamp_period,
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

struct SwapchainBundle {
    instance: ash::Instance,
    device: ash::Device,
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
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
        if !capabilities.supported_usage_flags.contains(SWAPCHAIN_USAGE) {
            return Err(RenderError::Unsupported(
                "surface больше не поддерживает TRANSFER_DST и COLOR_ATTACHMENT".to_owned(),
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
            .image_usage(SWAPCHAIN_USAGE)
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
            views: Vec::new(),
            render_finished: Vec::new(),
            extent,
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
            // SAFETY: the swapchain is idle before bundle replacement/destruction.
            unsafe { self.device.destroy_semaphore(semaphore, None) };
        }
        for view in self.views.drain(..) {
            // SAFETY: views of swapchain images are destroyed before the swapchain that owns the images.
            unsafe { self.device.destroy_image_view(view, None) };
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
    /// Вершины оверлея этого слота и ёмкость буфера в байтах.
    overlay_buffer: Option<HostBuffer>,
    overlay_capacity: usize,
    /// Пул из двух timestamp вокруг scene pass; `None`, если очередь их не поддерживает.
    timestamps: Option<vk::QueryPool>,
    /// Timestamp записаны и отправлены, но ещё не прочитаны.
    timestamps_pending: bool,
}

impl FrameResources {
    fn new(
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
            overlay_buffer: None,
            overlay_capacity: 0,
            timestamps: None,
            timestamps_pending: false,
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

    /// Читает GPU-время scene pass, записанное прошлой отправкой этого слота.
    ///
    /// Вызывается после ожидания fence слота.
    fn read_gpu_time(
        &mut self,
        device: &ash::Device,
        valid_bits: u32,
        period: f32,
    ) -> Result<Option<Duration>, RenderError> {
        let Some(pool) = self.timestamps.filter(|_| self.timestamps_pending) else {
            return Ok(None);
        };
        self.timestamps_pending = false;
        let mut ticks = [0_u64; TIMESTAMP_QUERY_COUNT as usize];
        // SAFETY: both queries were reset and written by a submission whose fence was waited.
        match unsafe {
            device.get_query_pool_results(pool, 0, &mut ticks, vk::QueryResultFlags::TYPE_64)
        } {
            Ok(()) => Ok(Some(timestamp_duration(
                ticks[0], ticks[1], valid_bits, period,
            ))),
            Err(vk::Result::NOT_READY) => Ok(None),
            Err(error) => Err(RenderError::Vulkan(error)),
        }
    }

    /// Копирует вершины оверлея в буфер слота, увеличивая его при нехватке места.
    ///
    /// Вызывается после ожидания fence слота, когда GPU уже не читает прежний буфер.
    fn write_overlay(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        vertices: &[OverlayVertex],
    ) -> Result<(), RenderError> {
        if vertices.is_empty() {
            return Ok(());
        }
        let bytes = overlay_vertex_bytes(vertices);
        if self.overlay_buffer.is_none() || self.overlay_capacity < bytes.len() {
            let capacity = bytes.len().next_power_of_two();
            self.overlay_buffer = None;
            self.overlay_capacity = 0;
            self.overlay_buffer = Some(HostBuffer::new(
                instance,
                device,
                physical_device,
                vk::BufferUsageFlags::VERTEX_BUFFER,
                &vec![0; capacity],
            )?);
            self.overlay_capacity = capacity;
        }
        self.overlay_buffer
            .as_ref()
            .ok_or(RenderError::InvalidState("буфер оверлея отсутствует"))?
            .write(bytes)
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

struct OffscreenTarget {
    color: DeviceImage,
    depth: DeviceImage,
    layout: vk::ImageLayout,
}

impl OffscreenTarget {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        Ok(Self {
            color: DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                color_subresource_range(),
            )?,
            depth: DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                DEPTH_FORMAT,
                vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
                depth_subresource_range(),
            )?,
            layout: vk::ImageLayout::UNDEFINED,
        })
    }
}

struct DeviceImage {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

impl DeviceImage {
    #[allow(clippy::too_many_arguments)]
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        subresource_range: vk::ImageSubresourceRange,
    ) -> Result<Self, RenderError> {
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
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
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping::default())
            .subresource_range(subresource_range);
        // SAFETY: image view matches the bound image format and its single subresource.
        target.view = unsafe { device.create_image_view(&view_info, None) }?;
        Ok(target)
    }
}

impl Drop for DeviceImage {
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

#[allow(clippy::too_many_arguments)]
fn record_frame(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut OffscreenTarget,
    shadow_image: &DeviceImage,
    output_image: vk::Image,
    output_extent: vk::Extent2D,
    render_extent: vk::Extent2D,
    shadow: &ShadowPass<'_>,
    scene: &ScenePass<'_>,
    overlay: Option<&OverlayPass>,
) -> Result<(), RenderError> {
    // SAFETY: this frame's command buffer is no longer in flight because its fence was waited.
    unsafe { device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty()) }?;
    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the command buffer was reset and belongs to the renderer's graphics command pool.
    unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;

    if let Some(pool) = scene.timestamps {
        // SAFETY: the slot fence was waited, so these queries are not in use; reset happens outside rendering.
        unsafe { device.cmd_reset_query_pool(command_buffer, pool, 0, TIMESTAMP_QUERY_COUNT) };
    }
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
        .image(target.color.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, scene_barrier);
    // Глубина не сохраняется между кадрами, поэтому прошлое содержимое отбрасывается.
    let depth_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS)
        .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
        .dst_stage_mask(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        )
        .dst_access_mask(
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        )
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(target.depth.image)
        .subresource_range(depth_subresource_range());
    pipeline_barrier(device, command_buffer, depth_barrier);
    if let Some(pool) = scene.timestamps {
        // Начало измерения предшествует shadow pass. Оно не блокирует его от хвоста предыдущего
        // кадра, поэтому время может быть немного занижено; ожидание ради точности сериализовало бы кадры.
        // SAFETY: the query was reset above, outside rendering.
        unsafe {
            device.cmd_write_timestamp2(
                command_buffer,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                pool,
                0,
            );
        }
    }

    let shadow_to_depth = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .dst_stage_mask(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        )
        .dst_access_mask(
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        )
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(shadow_image.image)
        .subresource_range(depth_subresource_range());
    pipeline_barrier(device, command_buffer, shadow_to_depth);
    let shadow_depth_attachment = vk::RenderingAttachmentInfo::default()
        .image_view(shadow_image.view)
        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: 1.0,
                stencil: 0,
            },
        });
    let shadow_rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: SHADOW_MAP_SIZE,
                height: SHADOW_MAP_SIZE,
            },
        })
        .layer_count(1)
        .depth_attachment(&shadow_depth_attachment);
    // SAFETY: the depth-only view covers the fixed shadow-map render area.
    unsafe { device.cmd_begin_rendering(command_buffer, &shadow_rendering_info) };
    if shadow.enabled {
        record_shadow_draws(device, command_buffer, SHADOW_MAP_SIZE, shadow);
    }
    // SAFETY: this matches the preceding depth-only cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };

    let shadow_to_sample = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        )
        .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(shadow_image.image)
        .subresource_range(depth_subresource_range());
    pipeline_barrier(device, command_buffer, shadow_to_sample);

    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(target.color.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue {
                float32: scene.clear_color,
            },
        })];
    let depth_attachment = vk::RenderingAttachmentInfo::default()
        .image_view(target.depth.view)
        .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::DONT_CARE)
        .clear_value(vk::ClearValue {
            depth_stencil: vk::ClearDepthStencilValue {
                depth: 1.0,
                stencil: 0,
            },
        });
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: render_extent,
        })
        .layer_count(1)
        .color_attachments(&color_attachment)
        .depth_attachment(&depth_attachment);
    // SAFETY: the dynamic-rendering attachment view covers the configured render extent.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    record_draws(device, command_buffer, render_extent, scene);
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };
    if let Some(pool) = scene.timestamps {
        // Конец scene pass: blit и оверлей ждут swapchain image и в замер не входят.
        // SAFETY: the query was reset at the start of this command buffer.
        unsafe {
            device.cmd_write_timestamp2(
                command_buffer,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                pool,
                1,
            );
        }
    }

    let source_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(target.color.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, source_barrier);
    record_output_pass(
        device,
        command_buffer,
        target.color.image,
        output_image,
        render_extent,
        output_extent,
        overlay,
    );
    // SAFETY: command recording is complete before the command buffer is submitted.
    unsafe { device.end_command_buffer(command_buffer) }?;
    target.layout = vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
    Ok(())
}

struct ScenePass<'a> {
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    frame_set: vk::DescriptorSet,
    meshes: &'a [MeshBuffers],
    textures: &'a [Texture],
    draws: &'a [DrawCommand],
    clear_color: [f32; 4],
    timestamps: Option<vk::QueryPool>,
}

/// Depth-only shadow pass. It shares the frame set and model transforms with the scene pass.
struct ShadowPass<'a> {
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    frame_set: vk::DescriptorSet,
    meshes: &'a [MeshBuffers],
    draws: &'a [DrawCommand],
    enabled: bool,
}

/// Рисование вершин оверлея поверх swapchain image после blit.
struct OverlayPass {
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    vertex_buffer: vk::Buffer,
    vertex_count: u32,
    view: vk::ImageView,
}

fn record_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
) {
    if scene.draws.is_empty() {
        return;
    }
    let viewport = [scene_viewport(render_extent)];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: render_extent,
    }];
    // SAFETY: recording happens inside dynamic rendering with a pipeline built for its formats.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.pipeline,
        );
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &scissor);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.pipeline_layout,
            0,
            &[scene.frame_set],
            &[],
        );
    }
    for draw in scene.draws {
        // Индексы проверены validate_draws до начала кадра.
        let mesh = &scene.meshes[draw.mesh.0];
        let texture = &scene.textures[draw.texture.map_or(WHITE_TEXTURE, |texture| texture.0)];
        let push_constants = push_constant_bytes(&draw.model);
        // SAFETY: mesh buffers and textures stay alive until the device is idle and match the pipeline layout.
        unsafe {
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                scene.pipeline_layout,
                1,
                &[texture.set],
                &[],
            );
            device.cmd_bind_vertex_buffers(command_buffer, 0, &[mesh.vertex_buffer.buffer], &[0]);
            device.cmd_bind_index_buffer(
                command_buffer,
                mesh.index_buffer.buffer,
                0,
                vk::IndexType::UINT32,
            );
            device.cmd_push_constants(
                command_buffer,
                scene.pipeline_layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                &push_constants,
            );
            device.cmd_draw_indexed(command_buffer, mesh.index_count, 1, 0, 0, 0);
        }
    }
}

fn record_shadow_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    extent: u32,
    shadow: &ShadowPass<'_>,
) {
    if shadow.draws.is_empty() {
        return;
    }
    let extent = vk::Extent2D {
        width: extent,
        height: extent,
    };
    let viewport = [scene_viewport(extent)];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    }];
    // SAFETY: recording happens inside depth-only dynamic rendering with the shadow pipeline.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            shadow.pipeline,
        );
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &scissor);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            shadow.pipeline_layout,
            0,
            &[shadow.frame_set],
            &[],
        );
    }
    for draw in shadow.draws {
        let mesh = &shadow.meshes[draw.mesh.0];
        let push_constants = push_constant_bytes(&draw.model);
        // SAFETY: draw ids were checked before recording; the buffers and frame set stay alive.
        unsafe {
            device.cmd_bind_vertex_buffers(command_buffer, 0, &[mesh.vertex_buffer.buffer], &[0]);
            device.cmd_bind_index_buffer(
                command_buffer,
                mesh.index_buffer.buffer,
                0,
                vk::IndexType::UINT32,
            );
            device.cmd_push_constants(
                command_buffer,
                shadow.pipeline_layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                &push_constants,
            );
            device.cmd_draw_indexed(command_buffer, mesh.index_count, 1, 0, 0, 0);
        }
    }
}

/// Viewport с отрицательной высотой направляет ось Y clip space вверх.
fn scene_viewport(extent: vk::Extent2D) -> vk::Viewport {
    vk::Viewport {
        x: 0.0,
        y: extent.height as f32,
        width: extent.width as f32,
        height: -(extent.height as f32),
        min_depth: 0.0,
        max_depth: 1.0,
    }
}

fn record_output_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source_image: vk::Image,
    output_image: vk::Image,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    overlay: Option<&OverlayPass>,
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

    let (last_stage, last_access, last_layout) = match overlay {
        Some(overlay) => {
            record_overlay(device, command_buffer, output_image, output_extent, overlay);
            (
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            )
        }
        None => (
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        ),
    };
    let present_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(last_stage)
        .src_access_mask(last_access)
        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
        .dst_access_mask(vk::AccessFlags2::NONE)
        .old_layout(last_layout)
        .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, present_barrier);
}

/// Рисует оверлей поверх результата blit; image остаётся в `COLOR_ATTACHMENT_OPTIMAL`.
fn record_overlay(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_image: vk::Image,
    output_extent: vk::Extent2D,
    overlay: &OverlayPass,
) {
    let to_attachment = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        )
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, to_attachment);

    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
    // Результат blit сохраняется: оверлей смешивается поверх него.
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(overlay.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: output_extent.width as f32,
        height: output_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let push_constants =
        float_bytes::<OVERLAY_PUSH_CONSTANTS_SIZE>(&[viewport[0].width, viewport[0].height]);
    // SAFETY: the view belongs to the output image, the pipeline matches the swapchain format,
    // and the vertex buffer holds vertex_count overlay vertices written after the slot fence.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            overlay.pipeline,
        );
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_vertex_buffers(command_buffer, 0, &[overlay.vertex_buffer], &[0]);
        device.cmd_push_constants(
            command_buffer,
            overlay.pipeline_layout,
            vk::ShaderStageFlags::VERTEX,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, overlay.vertex_count, 1, 0, 0);
        device.cmd_end_rendering(command_buffer);
    }
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

fn depth_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
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

struct ShaderModule {
    device: ash::Device,
    handle: vk::ShaderModule,
}

impl ShaderModule {
    fn new(device: &ash::Device, spirv: &[u8]) -> Result<Self, RenderError> {
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spirv))
            .map_err(|_| RenderError::InvalidState("встроенный SPIR-V повреждён"))?;
        let info = vk::ShaderModuleCreateInfo::default().code(&code);
        // SAFETY: the SPIR-V was produced and validated by naga at build time.
        let handle = unsafe { device.create_shader_module(&info, None) }?;
        Ok(Self {
            device: device.clone(),
            handle,
        })
    }
}

impl Drop for ShaderModule {
    fn drop(&mut self) {
        // SAFETY: shader modules may be destroyed once pipeline creation has returned.
        unsafe { self.device.destroy_shader_module(self.handle, None) };
    }
}

/// Uniform-буфер освещения и его набор дескрипторов для одного слота кадра.
struct FrameUniforms {
    buffer: HostBuffer,
    set: vk::DescriptorSet,
}

/// Текстура в device-local памяти с собственным пулом на один набор дескрипторов.
struct Texture {
    device: ash::Device,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    image: DeviceImage,
}

impl Drop for Texture {
    fn drop(&mut self) {
        if self.pool != vk::DescriptorPool::null() {
            // SAFETY: the renderer waits for device idle before dropping textures; this frees the set.
            unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
        }
    }
}

struct MeshBuffers {
    vertex_buffer: HostBuffer,
    index_buffer: HostBuffer,
    index_count: u32,
}

/// Буфер в host-visible памяти; данные копируются один раз при создании.
struct HostBuffer {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

impl HostBuffer {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        data: &[u8],
    ) -> Result<Self, RenderError> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(data.len() as vk::DeviceSize)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: the buffer has a non-zero size guaranteed by mesh validation.
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let mut host_buffer = Self {
            device: device.clone(),
            buffer,
            memory: vk::DeviceMemory::null(),
        };
        // SAFETY: buffer was created by this device and remains alive.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory_type = find_memory_type(
            instance,
            physical_device,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or_else(|| {
            RenderError::Unsupported("нет HOST_VISIBLE | HOST_COHERENT memory для меша".to_owned())
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: allocation requirements and memory type were reported for this buffer.
        host_buffer.memory = unsafe { device.allocate_memory(&allocate_info, None) }?;
        // SAFETY: memory allocation satisfies the buffer's reported requirements.
        unsafe { device.bind_buffer_memory(buffer, host_buffer.memory, 0) }?;
        // SAFETY: the coherent allocation is at least data.len() bytes and is not mapped elsewhere.
        unsafe {
            let mapped = device.map_memory(
                host_buffer.memory,
                0,
                data.len() as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapped.cast::<u8>(), data.len());
            device.unmap_memory(host_buffer.memory);
        }
        Ok(host_buffer)
    }
}

impl HostBuffer {
    /// Перезаписывает начало буфера; GPU не должен читать буфер во время записи.
    fn write(&self, data: &[u8]) -> Result<(), RenderError> {
        // SAFETY: callers write at most the buffer size into coherent memory that is not in use by the GPU.
        unsafe {
            let mapped = self.device.map_memory(
                self.memory,
                0,
                data.len() as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapped.cast::<u8>(), data.len());
            self.device.unmap_memory(self.memory);
        }
        Ok(())
    }
}

impl Drop for HostBuffer {
    fn drop(&mut self) {
        // SAFETY: the renderer waits for device idle before dropping meshes.
        unsafe { self.device.destroy_buffer(self.buffer, None) };
        if self.memory != vk::DeviceMemory::null() {
            // SAFETY: the buffer bound to this memory was destroyed first.
            unsafe { self.device.free_memory(self.memory, None) };
        }
    }
}

fn find_memory_type(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    type_bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    // SAFETY: memory properties are queried from the live physical device.
    let properties = unsafe { instance.get_physical_device_memory_properties(physical_device) };
    (0..properties.memory_type_count).find(|index| {
        type_bits & (1 << index) != 0
            && properties.memory_types[*index as usize]
                .property_flags
                .contains(flags)
    })
}

fn validate_mesh(vertex_count: usize, indices: &[u32]) -> Result<(), RenderError> {
    if vertex_count == 0 {
        return Err(RenderError::InvalidMesh("нет вершин"));
    }
    if indices.is_empty() || !indices.len().is_multiple_of(3) {
        return Err(RenderError::InvalidMesh(
            "число индексов должно быть положительным и кратным трём",
        ));
    }
    if u32::try_from(indices.len()).is_err() {
        return Err(RenderError::InvalidMesh("слишком много индексов"));
    }
    if indices.iter().any(|&index| index as usize >= vertex_count) {
        return Err(RenderError::InvalidMesh("индекс выходит за пределы вершин"));
    }
    Ok(())
}

fn validate_draws(
    draws: &[DrawCommand],
    mesh_count: usize,
    texture_count: usize,
) -> Result<(), RenderError> {
    if draws.iter().any(|draw| draw.mesh.0 >= mesh_count) {
        return Err(RenderError::InvalidMesh("MeshId не принадлежит renderer"));
    }
    if draws.iter().any(|draw| {
        draw.texture
            .is_some_and(|texture| texture.0 >= texture_count)
    }) {
        return Err(RenderError::InvalidTexture(
            "TextureId не принадлежит renderer".to_owned(),
        ));
    }
    Ok(())
}

fn validate_texture(
    width: u32,
    height: u32,
    byte_count: usize,
    max_size: u32,
) -> Result<(), RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidTexture(
            "размеры должны быть больше нуля".to_owned(),
        ));
    }
    if width > max_size || height > max_size {
        return Err(RenderError::InvalidTexture(format!(
            "{width}x{height} больше предела GPU {max_size}x{max_size}"
        )));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|texels| texels.checked_mul(4));
    if expected != Some(byte_count) {
        return Err(RenderError::InvalidTexture(format!(
            "для RGBA8 {width}x{height} нужно {} байт, передано {byte_count}",
            expected.map_or_else(|| "больше usize::MAX".to_owned(), |bytes| bytes.to_string())
        )));
    }
    Ok(())
}

fn validate_overlay(vertices: &[OverlayVertex]) -> Result<(), RenderError> {
    if !vertices.len().is_multiple_of(3) {
        return Err(RenderError::InvalidMesh(
            "число вершин оверлея должно быть кратным трём",
        ));
    }
    if u32::try_from(vertices.len()).is_err() {
        return Err(RenderError::InvalidMesh("слишком много вершин оверлея"));
    }
    Ok(())
}

/// Длительность между timestamp с учётом значащих бит и переполнения счётчика.
fn timestamp_duration(start: u64, end: u64, valid_bits: u32, period_ns: f32) -> Duration {
    let mask = if valid_bits >= 64 {
        u64::MAX
    } else {
        (1_u64 << valid_bits) - 1
    };
    let ticks = end.wrapping_sub(start) & mask;
    Duration::from_nanos((ticks as f64 * f64::from(period_ns)) as u64)
}

fn overlay_vertex_bytes(vertices: &[OverlayVertex]) -> &[u8] {
    // SAFETY: OverlayVertex is repr(C) and consists only of f32 fields without padding.
    unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), size_of_val(vertices)) }
}

fn vertex_bytes(vertices: &[Vertex]) -> &[u8] {
    // SAFETY: Vertex is repr(C) and consists only of f32 fields without padding.
    unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), size_of_val(vertices)) }
}

fn index_bytes(indices: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and any initialized u32 slice is readable as bytes.
    unsafe { std::slice::from_raw_parts(indices.as_ptr().cast(), size_of_val(indices)) }
}

/// Байты `FrameUniforms` в порядке std140: две матрицы и четыре `vec4`.
fn frame_uniform_bytes(frame: &Frame<'_>) -> Result<[u8; FRAME_UNIFORMS_SIZE], RenderError> {
    let light = frame.light;
    let [x, y, z] = light.direction;
    let length_squared = x * x + y * y + z * z;
    if !length_squared.is_finite() || length_squared == 0.0 {
        return Err(RenderError::InvalidConfig(
            "направление света должно быть конечным и ненулевым",
        ));
    }
    if frame
        .shadow_view_projection
        .is_some_and(|matrix| !matrix.iter().flatten().all(|value| value.is_finite()))
    {
        return Err(RenderError::InvalidConfig(
            "матрица shadow view projection должна быть конечной",
        ));
    }

    let light_view_projection = frame.shadow_view_projection.unwrap_or(IDENTITY_MATRIX);
    let mut values = [0.0; FRAME_UNIFORMS_SIZE / size_of::<f32>()];
    values[..16].copy_from_slice(frame.view_projection.as_flattened());
    values[16..32].copy_from_slice(light_view_projection.as_flattened());
    values[32..36].copy_from_slice(&[x, y, z, 0.0]);
    values[36..40].copy_from_slice(&[light.color[0], light.color[1], light.color[2], 0.0]);
    values[40..44].copy_from_slice(&[light.ambient[0], light.ambient[1], light.ambient[2], 0.0]);
    values[44..48].copy_from_slice(&[
        if frame.shadow_view_projection.is_some() {
            1.0
        } else {
            0.0
        },
        1.0 / SHADOW_MAP_SIZE as f32,
        0.0,
        0.0,
    ]);
    Ok(float_bytes(&values))
}

/// Байты push constants: матрица модели, затем матрица нормалей; обе по столбцам.
fn push_constant_bytes(model: &[[f32; 4]; 4]) -> [u8; PUSH_CONSTANTS_SIZE] {
    let normal_from_model = normal_matrix(model);
    let mut values = [0.0; PUSH_CONSTANTS_SIZE / size_of::<f32>()];
    values[..16].copy_from_slice(model.as_flattened());
    values[16..].copy_from_slice(normal_from_model.as_flattened());
    float_bytes(&values)
}

fn float_bytes<const N: usize>(values: &[f32]) -> [u8; N] {
    let mut bytes = [0; N];
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Матрица нормалей: присоединённая к блоку 3x3 модели со знаком определителя.
///
/// Она пропорциональна обратной транспонированной матрице, а длину нормали
/// восстанавливает нормализация в шейдере. Знак сохраняет внешние нормали
/// у зеркальных моделей.
fn normal_matrix(model: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let [a, b, c] = [0, 1, 2].map(|column| {
        let [x, y, z, _] = model[column];
        [x, y, z]
    });
    let cross = |u: [f32; 3], v: [f32; 3]| {
        [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ]
    };
    let columns = [cross(b, c), cross(c, a), cross(a, b)];
    let determinant: f32 = (0..3).map(|i| a[i] * columns[0][i]).sum();
    let sign = if determinant < 0.0 { -1.0 } else { 1.0 };
    let mut result = [[0.0; 4]; 4];
    for (target, column) in result.iter_mut().zip(columns) {
        *target = [column[0] * sign, column[1] * sign, column[2] * sign, 0.0];
    }
    result[3][3] = 1.0;
    result
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
        DirectionalLight, DrawCommand, FRAME_UNIFORMS_SIZE, Frame, MESH_FRAGMENT_SHADER,
        MESH_VERTEX_SHADER, MeshId, OVERLAY_FRAGMENT_SHADER, OVERLAY_VERTEX_SHADER, OverlayVertex,
        RenderError, RendererConfig, SHADOW_VERTEX_SHADER, TextureId, Vertex, check_extensions,
        deduplicate_names, frame_uniform_bytes, index_bytes, missing_extensions, normal_matrix,
        overlay_vertex_bytes, push_constant_bytes, scaled_extent, scene_viewport,
        select_composite_alpha, select_surface_extent, timestamp_duration, validate_draws,
        validate_mesh, validate_overlay, validate_render_scale, validate_texture, vertex_bytes,
    };
    use ash::vk;
    use std::ffi::{CStr, CString};
    use std::time::Duration;

    #[test]
    fn embedded_shaders_are_spirv_words() {
        for shader in [
            MESH_VERTEX_SHADER,
            MESH_FRAGMENT_SHADER,
            SHADOW_VERTEX_SHADER,
            OVERLAY_VERTEX_SHADER,
            OVERLAY_FRAGMENT_SHADER,
        ] {
            let words = ash::util::read_spv(&mut std::io::Cursor::new(shader)).unwrap();
            assert_eq!(words[0], 0x0723_0203);
        }
    }

    #[test]
    fn mesh_validation_accepts_triangle_lists_inside_vertex_range() {
        assert!(validate_mesh(3, &[0, 1, 2]).is_ok());
        assert!(validate_mesh(4, &[0, 1, 2, 2, 3, 0]).is_ok());
    }

    #[test]
    fn mesh_validation_rejects_empty_partial_and_out_of_range_input() {
        for (vertex_count, indices) in [
            (0, vec![0, 0, 0]),
            (3, vec![]),
            (3, vec![0, 1]),
            (3, vec![0, 1, 2, 0]),
            (3, vec![0, 1, 3]),
            (3, vec![0, 1, u32::MAX]),
        ] {
            assert!(
                matches!(
                    validate_mesh(vertex_count, &indices),
                    Err(RenderError::InvalidMesh(_))
                ),
                "{vertex_count} {indices:?}"
            );
        }
    }

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    fn floats(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_ne_bytes(*chunk))
            .collect()
    }

    fn transform_normal(matrix: &[[f32; 4]; 4], normal: [f32; 3]) -> [f32; 3] {
        let [x, y, z] = std::array::from_fn(|row| {
            (0..3)
                .map(|column| matrix[column][row] * normal[column])
                .sum::<f32>()
        });
        let length = (x * x + y * y + z * z).sqrt();
        [x / length, y / length, z / length]
    }

    fn assert_close(actual: [f32; 3], expected: [f32; 3]) {
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(a, e)| (a - e).abs() < 1e-5),
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn draw_validation_rejects_unknown_meshes_and_textures() {
        let draw = |mesh, texture: Option<usize>| DrawCommand {
            mesh: MeshId(mesh),
            texture: texture.map(TextureId),
            model: IDENTITY,
        };
        assert!(validate_draws(&[], 0, 1).is_ok());
        assert!(validate_draws(&[draw(0, None), draw(1, Some(1))], 2, 2).is_ok());
        assert!(matches!(
            validate_draws(&[draw(0, None), draw(2, None)], 2, 2),
            Err(RenderError::InvalidMesh(_))
        ));
        assert!(matches!(
            validate_draws(&[draw(0, Some(2))], 1, 2),
            Err(RenderError::InvalidTexture(_))
        ));
    }

    #[test]
    fn texture_validation_requires_rgba8_bytes_within_gpu_limits() {
        assert!(validate_texture(1, 1, 4, 16).is_ok());
        assert!(validate_texture(16, 2, 128, 16).is_ok());
        for (width, height, bytes) in [
            (0, 1, 0),
            (1, 0, 0),
            (17, 1, 68),
            (1, 17, 68),
            (2, 2, 15),
            (2, 2, 17),
            (2, 2, 12),
        ] {
            assert!(
                matches!(
                    validate_texture(width, height, bytes, 16),
                    Err(RenderError::InvalidTexture(_))
                ),
                "{width}x{height} {bytes}"
            );
        }
        assert!(validate_texture(u32::MAX, u32::MAX, 0, u32::MAX).is_err());
    }

    #[test]
    fn vertex_layout_matches_shader_inputs() {
        assert_eq!(size_of::<Vertex>(), 44);
        assert_eq!(std::mem::offset_of!(Vertex, position), 0);
        assert_eq!(std::mem::offset_of!(Vertex, normal), 12);
        assert_eq!(std::mem::offset_of!(Vertex, uv), 24);
        assert_eq!(std::mem::offset_of!(Vertex, color), 32);
        let vertices = [Vertex {
            position: [1.0, 2.0, 3.0],
            normal: [4.0, 5.0, 6.0],
            uv: [7.0, 8.0],
            color: [9.0, 10.0, 11.0],
        }];
        assert_eq!(
            floats(vertex_bytes(&vertices)),
            (1..=11).map(|value| value as f32).collect::<Vec<_>>()
        );
        assert_eq!(
            index_bytes(&[1, 2]),
            [1_u32.to_ne_bytes(), 2_u32.to_ne_bytes()].concat()
        );
    }

    #[test]
    fn push_constants_hold_model_and_normal_matrices_column_by_column() {
        let mut model = IDENTITY;
        model[0][0] = 2.0;
        model[3] = [5.0, 6.0, 7.0, 1.0];
        let values = floats(&push_constant_bytes(&model));
        assert_eq!(values.len(), 32);
        assert_eq!(&values[..4], &[2.0, 0.0, 0.0, 0.0]);
        assert_eq!(&values[12..16], &[5.0, 6.0, 7.0, 1.0]);
        // Матрица нормалей пропорциональна обратной транспонированной: масштаб X делит нормаль X.
        assert_eq!(&values[16..20], &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(&values[20..24], &[0.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn normal_matrix_keeps_normals_perpendicular_under_non_uniform_scale() {
        // Плоскость x + y = 0 после растяжения по X вдвое имеет нормаль, пропорциональную (1, 2, 0).
        let mut model = IDENTITY;
        model[0][0] = 2.0;
        model[3] = [5.0, 6.0, 7.0, 1.0];
        let inv_sqrt5 = 1.0 / 5.0_f32.sqrt();
        assert_close(
            transform_normal(&normal_matrix(&model), [1.0, 1.0, 0.0]),
            [inv_sqrt5, 2.0 * inv_sqrt5, 0.0],
        );
        // Поворот на 90° вокруг Z переводит нормаль +X в +Y.
        let rotation = [
            [0.0, 1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        assert_close(
            transform_normal(&normal_matrix(&rotation), [1.0, 0.0, 0.0]),
            [0.0, 1.0, 0.0],
        );
    }

    #[test]
    fn normal_matrix_keeps_outward_normals_for_mirrored_models() {
        let mut mirror = IDENTITY;
        mirror[0][0] = -1.0;
        // Зеркальная по X грань +X становится гранью -X, и нормаль должна смотреть в -X.
        assert_close(
            transform_normal(&normal_matrix(&mirror), [1.0, 0.0, 0.0]),
            [-1.0, 0.0, 0.0],
        );
        assert_close(
            transform_normal(&normal_matrix(&mirror), [0.0, 1.0, 0.0]),
            [0.0, 1.0, 0.0],
        );
    }

    #[test]
    fn frame_uniforms_pack_matrices_and_light_at_std140_offsets() {
        let matrix = |start: f32| {
            std::array::from_fn(|column| {
                std::array::from_fn(|row| start + (column * 4 + row) as f32)
            })
        };
        let light = DirectionalLight {
            direction: [1.0, 2.0, 3.0],
            color: [4.0, 5.0, 6.0],
            ambient: [7.0, 8.0, 9.0],
        };
        let frame = Frame {
            clear_color: [0.0; 4],
            view_projection: matrix(1.0),
            shadow_view_projection: Some(matrix(17.0)),
            light,
            draws: &[],
            overlay: &[],
        };
        let values = floats(&frame_uniform_bytes(&frame).unwrap());
        assert_eq!(FRAME_UNIFORMS_SIZE, 192);
        assert_eq!(
            &values[0..16],
            &(1..=16).map(|x| x as f32).collect::<Vec<_>>()
        );
        assert_eq!(
            &values[16..32],
            &(17..=32).map(|x| x as f32).collect::<Vec<_>>()
        );
        assert_eq!(&values[32..36], &[1.0, 2.0, 3.0, 0.0]);
        assert_eq!(&values[36..40], &[4.0, 5.0, 6.0, 0.0]);
        assert_eq!(&values[40..44], &[7.0, 8.0, 9.0, 0.0]);
        assert_eq!(&values[44..48], &[1.0, 1.0 / 2048.0, 0.0, 0.0]);

        let no_shadows = Frame {
            shadow_view_projection: None,
            ..frame
        };
        assert_eq!(floats(&frame_uniform_bytes(&no_shadows).unwrap())[44], 0.0);
    }

    #[test]
    fn frame_uniforms_reject_zero_light_or_non_finite_shadow_matrix() {
        for direction in [[0.0; 3], [f32::NAN, 0.0, 0.0], [f32::INFINITY, 0.0, 0.0]] {
            let light = DirectionalLight {
                direction,
                ..DirectionalLight::default()
            };
            assert!(
                matches!(
                    frame_uniform_bytes(&Frame {
                        clear_color: [0.0; 4],
                        view_projection: IDENTITY,
                        shadow_view_projection: None,
                        light,
                        draws: &[],
                        overlay: &[],
                    }),
                    Err(RenderError::InvalidConfig(_))
                ),
                "{direction:?}"
            );
        }

        let mut invalid = IDENTITY;
        invalid[2][1] = f32::NAN;
        let frame = Frame {
            clear_color: [0.0; 4],
            view_projection: IDENTITY,
            shadow_view_projection: Some(invalid),
            light: DirectionalLight::default(),
            draws: &[],
            overlay: &[],
        };
        assert!(matches!(
            frame_uniform_bytes(&frame),
            Err(RenderError::InvalidConfig(_))
        ));
    }

    #[test]
    fn scene_viewport_points_clip_space_y_up() {
        let viewport = scene_viewport(vk::Extent2D {
            width: 640,
            height: 360,
        });
        assert_eq!((viewport.x, viewport.y), (0.0, 360.0));
        assert_eq!((viewport.width, viewport.height), (640.0, -360.0));
        assert_eq!((viewport.min_depth, viewport.max_depth), (0.0, 1.0));
    }

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

    #[test]
    fn overlay_validation_requires_whole_triangles() {
        let vertex = OverlayVertex::default();
        assert!(validate_overlay(&[]).is_ok());
        assert!(validate_overlay(&[vertex; 3]).is_ok());
        assert!(validate_overlay(&[vertex; 6]).is_ok());
        for count in [1, 2, 4] {
            assert!(matches!(
                validate_overlay(&vec![vertex; count]),
                Err(RenderError::InvalidMesh(_))
            ));
        }
    }

    #[test]
    fn overlay_vertices_are_tightly_packed_floats() {
        let vertices = [OverlayVertex {
            position: [1.0, 2.0],
            color: [0.25, 0.5, 0.75, 1.0],
        }];
        let bytes = overlay_vertex_bytes(&vertices);
        assert_eq!(bytes.len(), 6 * size_of::<f32>());
        let floats = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_ne_bytes(*chunk))
            .collect::<Vec<_>>();
        assert_eq!(floats, [1.0, 2.0, 0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn timestamp_duration_scales_ticks_by_period() {
        assert_eq!(
            timestamp_duration(100, 1_100, 64, 1.0),
            Duration::from_nanos(1_000)
        );
        assert_eq!(
            timestamp_duration(0, 1_000, 64, 2.5),
            Duration::from_nanos(2_500)
        );
        assert_eq!(timestamp_duration(5, 5, 64, 1.0), Duration::ZERO);
    }

    #[test]
    fn timestamp_duration_handles_counter_wrap_within_valid_bits() {
        let max = (1_u64 << 36) - 1;
        assert_eq!(
            timestamp_duration(max - 9, 10, 36, 1.0),
            Duration::from_nanos(20)
        );
        // Биты выше timestampValidBits не определены и отбрасываются.
        assert_eq!(
            timestamp_duration(1 << 40, (1 << 40) + 7 + (1 << 37), 36, 1.0),
            Duration::from_nanos(7)
        );
        assert_eq!(
            timestamp_duration(u64::MAX, 4, 64, 1.0),
            Duration::from_nanos(5)
        );
    }
}
