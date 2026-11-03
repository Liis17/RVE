//! Vulkan-рендерер, форматы изображений и материалы сцены.

use ash::{Entry, vk};

pub mod environment;

use clusters::{ClusterBounds, DepthRange, GpuLocalLight, camera_cluster_bounds};
use culling::{Frustum, LocalBounds, back_to_front_order};
pub use environment::{EnvironmentId, EnvironmentSource, ProceduralSky};
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use ui::{
    PackedUi, RetireQueue, UiDraw, is_srgb_format, pack_ui, validate_texture_region,
    validate_viewport_size,
};

mod clusters;
mod culling;
mod ui;

include!(concat!(env!("OUT_DIR"), "/render_constants.rs"));

pub use ui::{UiImage, UiPrimitive, UiTextureId, UiVertex, ViewportId};

const FRAMES_IN_FLIGHT: usize = 2;
const DEFAULT_UPLOAD_BATCH_CAPACITY: usize = 64 * 1024 * 1024;
const OFFSCREEN_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const LDR_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
const SRGB_TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const LINEAR_TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// egui хранит значения в гамма-пространстве, поэтому они читаются без перевода; шейдер сам
/// переводит результат для sRGB-swapchain, и смешивание в этом случае идёт в линейном пространстве.
const UI_TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Resolution of each layer in the shared directional shadow map.
pub const SHADOW_MAP_RESOLUTION: u32 = 2048;
const SHADOW_CASCADE_COUNT: usize = 4;
/// Максимальное ожидание swapchain image перед пропуском кадра.
const ACQUIRE_TIMEOUT_NS: u64 = 100_000_000;
const NO_GRAPHICS_QUEUE_FAMILY_REASON: &str = "нет graphics queue family";
const NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON: &str =
    "нет graphics queue family с поддержкой compute";
const NO_GRAPHICS_PRESENT_QUEUE_FAMILY_REASON: &str = "нет общей graphics + present queue family";
const MESH_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mesh.vert.spv"));
const CLUSTER_COMPUTE_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/cluster.comp.spv"));
const MESH_OPAQUE_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/mesh.frag.opaque.spv"));
const MESH_MASK_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/mesh.frag.mask.spv"));
const MESH_BLEND_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/mesh.frag.blend.spv"));
const SHADOW_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow.vert.spv"));
const LOCAL_SHADOW_VERTEX_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/shadow.local.vert.spv"));
const SHADOW_FRAGMENT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shadow.frag.spv"));
const OVERLAY_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay.vert.spv"));
const OVERLAY_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/overlay.frag.spv"));
const SWAPCHAIN_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::COLOR_ATTACHMENT;
const SURFACE_FORMAT_FEATURES: vk::FormatFeatureFlags = vk::FormatFeatureFlags::COLOR_ATTACHMENT;
const UI_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui.vert.spv"));
const UI_FRAGMENT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ui.frag.spv"));
const FULLSCREEN_VERTEX_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen.vert.spv"));
const TONEMAP_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/tonemap.frag.spv"));
const OUTPUT_FRAGMENT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/output.frag.spv"));
const EQUIRECT_TO_CUBE_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/equirect_to_cube.comp.spv"));
const PROCEDURAL_SKY_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sky.comp.spv"));
const IRRADIANCE_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/irradiance.comp.spv"));
const PREFILTER_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/prefilter.comp.spv"));
const SKYBOX_VERTEX_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skybox.vert.spv"));
const SKYBOX_FRAGMENT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skybox.frag.spv"));
const OVERLAY_PUSH_CONSTANTS_SIZE: usize = size_of::<[f32; 2]>();
const UI_PUSH_CONSTANTS_SIZE: usize = size_of::<[f32; 4]>();
const POST_PUSH_CONSTANTS_SIZE: usize = size_of::<[f32; 4]>();
const PIPELINE_CACHE_HEADER_SIZE: usize = size_of::<vk::PipelineCacheHeaderVersionOne>();
/// Два timestamp на слот кадра: перед cluster compute и после tonemap pass.
const TIMESTAMP_QUERY_COUNT: u32 = 2;
const WHITE_SRGB_TEXTURE: usize = 0;
const WHITE_LINEAR_TEXTURE: usize = 1;
const FLAT_NORMAL_TEXTURE: usize = 2;
const DEFAULT_MATERIAL: usize = 0;
const PUSH_CONSTANTS_SIZE: usize = 2 * size_of::<[[f32; 4]; 4]>();
const FRAME_UNIFORMS_SIZE: usize =
    (4 + SHADOW_CASCADE_COUNT) * size_of::<[[f32; 4]; 4]>() + 8 * size_of::<[f32; 4]>();
const MATERIAL_UNIFORMS_SIZE: usize = 4 * size_of::<[f32; 4]>();
const LOCAL_LIGHT_GPU_SIZE: usize = 7 * size_of::<[f32; 4]>();
const LOCAL_SHADOW_MATRIX_BUFFER_SIZE: usize = MAX_LOCAL_SHADOW_LAYERS * size_of::<[[f32; 4]; 4]>();
const CLUSTER_BOUNDS_GPU_SIZE: usize = 2 * size_of::<[f32; 4]>();
const CLUSTER_RECORD_GPU_SIZE: usize = size_of::<u32>() * (1 + MAX_CLUSTER_LIGHTS);
const CLUSTER_BUFFER_SIZE: usize = CLUSTER_COUNT * CLUSTER_RECORD_GPU_SIZE;
const CLUSTER_BOUNDS_BUFFER_SIZE: usize = CLUSTER_COUNT * CLUSTER_BOUNDS_GPU_SIZE;
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

/// Идентификатор неизменяемого материала, созданного [`Renderer::create_material`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MaterialId(usize);

/// Интерпретация RGBA8 texel-ов GPU-текстуры.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextureColorSpace {
    Srgb,
    Linear,
}

/// Обработка альфа-канала материала.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum AlphaMode {
    /// Всегда непрозрачный; альфа текстуры и `opacity` игнорируются.
    #[default]
    Opaque,
    /// Пиксели ниже порога отбрасываются, остальные остаются непрозрачными.
    Mask { cutoff: f32 },
    /// Альфа смешивается с уже нарисованным цветом в порядке от дальнего к ближнему.
    Blend,
}

/// Неизменяемые параметры материала, отсечение граней и необязательные текстуры.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    pub base_color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub opacity: f32,
    pub alpha_mode: AlphaMode,
    pub double_sided: bool,
    pub base_color_texture: Option<TextureId>,
    pub metallic_roughness_texture: Option<TextureId>,
    pub normal_texture: Option<TextureId>,
    pub emissive_texture: Option<TextureId>,
    pub occlusion_texture: Option<TextureId>,
    pub normal_scale: f32,
    pub occlusion_strength: f32,
}

impl Default for Material {
    fn default() -> Self {
        Self {
            base_color: [1.0; 3],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            opacity: 1.0,
            alpha_mode: AlphaMode::Opaque,
            double_sided: false,
            base_color_texture: None,
            metallic_roughness_texture: None,
            normal_texture: None,
            emissive_texture: None,
            occlusion_texture: None,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
        }
    }
}

/// Команда рисования меша.
///
/// `model` переводит пространство модели в мировое и задаётся по столбцам.
/// `None` выбирает встроенный белый dielectric материал.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawCommand {
    pub mesh: MeshId,
    pub material: Option<MaterialId>,
    pub model: [[f32; 4]; 4],
    /// Попадает ли меш в shadow map; на получение тени не влияет.
    pub casts_shadow: bool,
}

/// Направленный свет и фоновое освещение сцены в линейном RGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectionalLight {
    /// Направление лучей в мировом пространстве; длина не важна, но не может быть нулевой.
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub ambient: [f32; 3],
}

/// A point or spot light accepted by the renderer. `color_intensity` is linear RGB after
/// multiplying the scene color by its intensity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LocalLight {
    Point {
        position: [f32; 3],
        color_intensity: [f32; 3],
        range: f32,
    },
    Spot {
        position: [f32; 3],
        direction: [f32; 3],
        color_intensity: [f32; 3],
        range: f32,
        cos_inner_angle: f32,
        cos_outer_angle: f32,
    },
}

/// CPU-generated matrices associated with a point or spot light. The renderer
/// validates the light index and matrix type before applying its per-frame
/// shadow-layer budget.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // The fixed six-face point matrix set is copied as frame data.
pub enum LocalLightShadowMatrices {
    Point {
        /// +X, -X, +Y, -Y, +Z, -Z face order.
        matrices: [[[f32; 4]; 4]; 6],
    },
    Spot {
        matrix: [[f32; 4]; 4],
    },
}

/// Shadow matrices for one entry in [`Frame::lights`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalLightShadow {
    pub light_index: usize,
    pub matrices: LocalLightShadowMatrices,
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

/// Camera matrices and world-space position for one rendered view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameCamera {
    /// World-to-view matrix, stored by columns. Clustering visible local lights
    /// requires a rigid affine transform (rotation/reflection and translation).
    pub view: [[f32; 4]; 4],
    /// View-to-clip matrix, stored by columns; Vulkan depth is `[0, 1]`.
    pub projection: [[f32; 4]; 4],
    pub position: [f32; 3],
}

/// Environment selected for one view. The id is renderer-local.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameEnvironment {
    pub id: EnvironmentId,
    pub intensity: f32,
    /// World rotation as three column vectors.
    pub rotation: [[f32; 3]; 3],
    pub skybox: bool,
}

/// Описание кадра для [`Renderer::render_frame`].
///
/// `camera` contains view and projection matrices with Y-up and Vulkan depth `[0, 1]`.
/// Ось Y clip space направлена вверх, глубина после деления на `w` лежит в `[0, 1]`.
/// `shadow_cascades` содержит от нуля до четырёх матриц clip space света с той же конвенцией
/// Y-up и глубиной `[0, 1]`; пустой срез отключает тени. Матрицы должны быть конечными.
/// `overlay` — треугольники (по три вершины), которые рисуются с альфа-смешиванием
/// поверх итогового изображения без масштабирования `render_scale`; пустой срез
/// отключает оверлей. `view` задаёт render scale, экспозицию, tonemapper и сглаживание.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub clear_color: [f32; 4],
    pub camera: FrameCamera,
    pub environment: Option<FrameEnvironment>,
    pub shadow_cascades: &'a [[[f32; 4]; 4]],
    pub light: DirectionalLight,
    pub lights: &'a [LocalLight],
    /// Sparse point and spot shadow requests. Each entry refers to the same
    /// index in `lights`; omitted entries render that light without a shadow.
    pub local_light_shadows: &'a [LocalLightShadow],
    pub draws: &'a [DrawCommand],
    pub overlay: &'a [OverlayVertex],
    pub view: ViewSettings,
}

/// Настройки масштабирования и обработки цвета одного вида.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewSettings {
    pub render_scale: f32,
    pub exposure_ev: f32,
    pub tonemapper: Tonemapper,
    pub anti_aliasing: AntiAliasing,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            render_scale: 1.0,
            exposure_ev: 0.0,
            tonemapper: Tonemapper::Aces,
            anti_aliasing: AntiAliasing::Fxaa,
        }
    }
}

/// Оператор преобразования HDR-цвета в SDR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tonemapper {
    #[default]
    Aces,
    Neutral,
    None,
}

impl Tonemapper {
    const fn code(self) -> u32 {
        match self {
            Self::Aces => 0,
            Self::Neutral => 1,
            Self::None => 2,
        }
    }
}

/// Пространственная фильтрация перед выводом кадра.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AntiAliasing {
    #[default]
    Fxaa,
    None,
}

impl AntiAliasing {
    const fn code(self) -> u32 {
        match self {
            Self::Fxaa => 0,
            Self::None => 1,
        }
    }
}

/// Requested presentation behavior for the swapchain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PresentMode {
    /// V-sync; Vulkan guarantees this mode for a usable surface.
    #[default]
    Fifo,
    /// Low-latency presentation that may replace queued frames.
    Mailbox,
    /// Immediate presentation without waiting for a vertical blank.
    Immediate,
}

impl PresentMode {
    fn vk(self) -> vk::PresentModeKHR {
        match self {
            Self::Fifo => vk::PresentModeKHR::FIFO,
            Self::Mailbox => vk::PresentModeKHR::MAILBOX,
            Self::Immediate => vk::PresentModeKHR::IMMEDIATE,
        }
    }

    fn from_vk(mode: vk::PresentModeKHR) -> Self {
        match mode {
            vk::PresentModeKHR::FIFO => Self::Fifo,
            vk::PresentModeKHR::MAILBOX => Self::Mailbox,
            vk::PresentModeKHR::IMMEDIATE => Self::Immediate,
            _ => Self::Fifo,
        }
    }
}

fn select_present_mode(
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

#[derive(Clone, Copy)]
struct PipelineCacheIdentity {
    vendor_id: u32,
    device_id: u32,
    pipeline_cache_uuid: [u8; vk::UUID_SIZE],
}

fn validate_pipeline_cache(
    data: &[u8],
    expected: PipelineCacheIdentity,
) -> Result<(), &'static str> {
    if data.is_empty() {
        return Ok(());
    }
    if data.len() < PIPELINE_CACHE_HEADER_SIZE {
        return Err("pipeline cache header is truncated");
    }

    let header_size = u32::from_ne_bytes(data[0..4].try_into().unwrap()) as usize;
    if header_size != PIPELINE_CACHE_HEADER_SIZE {
        return Err("pipeline cache header size is invalid");
    }
    if data.len() < header_size {
        return Err("pipeline cache header is truncated");
    }
    let header_version = i32::from_ne_bytes(data[4..8].try_into().unwrap());
    if header_version != vk::PipelineCacheHeaderVersion::ONE.as_raw() {
        return Err("pipeline cache header version is unsupported");
    }
    let vendor_id = u32::from_ne_bytes(data[8..12].try_into().unwrap());
    let device_id = u32::from_ne_bytes(data[12..16].try_into().unwrap());
    let uuid = &data[16..PIPELINE_CACHE_HEADER_SIZE];
    if vendor_id != expected.vendor_id
        || device_id != expected.device_id
        || uuid != expected.pipeline_cache_uuid
    {
        return Err("pipeline cache belongs to another physical device");
    }
    Ok(())
}

fn compatible_pipeline_cache(data: &[u8], expected: PipelineCacheIdentity) -> &[u8] {
    match validate_pipeline_cache(data, expected) {
        Ok(()) => data,
        Err(reason) => {
            eprintln!("ignoring pipeline cache: {reason}");
            &[]
        }
    }
}

fn completed_upload_count<E>(
    statuses: impl IntoIterator<Item = Result<bool, E>>,
) -> Result<usize, E> {
    let mut count = 0;
    for status in statuses {
        if !status? {
            break;
        }
        count += 1;
    }
    Ok(count)
}

/// Настройки Vulkan-рендерера.
#[derive(Default)]
pub struct RendererConfig {
    pub extra_instance_extensions: Vec<&'static CStr>,
    pub extra_device_extensions: Vec<&'static CStr>,
    pub present_mode: PresentMode,
    /// Vulkan pipeline cache bytes validated against the selected physical device.
    pub pipeline_cache: Vec<u8>,
}

/// Возможности выбранного физического устройства.
pub struct Capabilities {
    pub device_name: String,
    pub vendor_id: u32,
    pub api_version: u32,
    pub discrete: bool,
    pub tessellation_shader: bool,
}

/// Результат проверки базовых Vulkan-требований для физического устройства.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceReport {
    pub name: String,
    pub vendor_id: u32,
    pub api_version: u32,
    pub discrete: bool,
    pub suitable: bool,
    pub reasons: Vec<String>,
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
    InvalidMaterial(String),
    InvalidEnvironment(String),
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
            Self::InvalidMaterial(message) => write!(f, "некорректный материал: {message}"),
            Self::InvalidEnvironment(message) => write!(f, "некорректное окружение: {message}"),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcquireAction {
    Acquired { image_index: u32, suboptimal: bool },
    OutOfDate,
    Skipped,
    Failed(vk::Result),
}

fn classify_acquire_result(result: Result<(u32, bool), vk::Result>) -> AcquireAction {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeInstanceFailure {
    NoDriver,
    Vulkan(vk::Result),
}

fn classify_probe_instance_failure(error: vk::Result) -> ProbeInstanceFailure {
    match error {
        vk::Result::ERROR_INCOMPATIBLE_DRIVER | vk::Result::ERROR_INITIALIZATION_FAILED => {
            ProbeInstanceFailure::NoDriver
        }
        error => ProbeInstanceFailure::Vulkan(error),
    }
}

/// Проверяет доступные Vulkan-устройства без создания окна или surface.
///
/// `suitable` учитывает Vulkan 1.3, dynamic rendering, synchronization2, extended dynamic state,
/// graphics queue и обязательные расширения устройства, включая swapchain.
/// Возможность present и совместимость с конкретным surface проверяются
/// отдельно в [`Renderer::new`]. Если loader недоступен, возвращается
/// `RenderError::Loader`; если instance не создаётся из-за отсутствующего
/// драйвера (`ERROR_INCOMPATIBLE_DRIVER` или `ERROR_INITIALIZATION_FAILED`),
/// либо физических устройств нет, возвращается `Ok(Vec::new())`. Прочие
/// ошибки probe возвращаются как `RenderError`, а устройства, которые не
/// проходят требования, представлены отчётами с `suitable == false`.
pub fn probe_devices() -> Result<Vec<DeviceReport>, RenderError> {
    let entry =
        engine_platform::load_vulkan().map_err(|error| RenderError::Loader(error.to_string()))?;
    let requested_extensions = deduplicate_names(
        engine_platform::INSTANCE_EXTENSIONS
            .iter()
            .map(|name| CString::from(*name))
            .collect(),
    );
    // SAFETY: extension enumeration only queries the loaded Vulkan loader.
    let available_extensions = unsafe { entry.enumerate_instance_extension_properties(None) }?;
    check_extensions(&requested_extensions, &available_extensions, "instance")?;

    let extension_pointers = requested_extensions
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
        .enabled_extension_names(&extension_pointers);
    // SAFETY: extension pointers and application info remain alive through instance creation.
    let instance = match unsafe { entry.create_instance(&instance_info, None) } {
        Ok(instance) => instance,
        Err(error) => match classify_probe_instance_failure(error) {
            ProbeInstanceFailure::NoDriver => return Ok(Vec::new()),
            ProbeInstanceFailure::Vulkan(error) => return Err(RenderError::Vulkan(error)),
        },
    };

    let result = (|| {
        let required_device_extensions = required_device_extensions(&[]);
        // SAFETY: the instance is live and no surface is needed to enumerate physical devices.
        let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
        physical_devices
            .into_iter()
            .map(|physical_device| {
                inspect_device(&instance, physical_device, &required_device_extensions)
                    .map(|inspection| inspection.report)
            })
            .collect()
    })();

    // SAFETY: no device or other child objects were created from this probe instance.
    unsafe { instance.destroy_instance(None) };
    result
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
    upload_alignment: usize,
    next_upload_serial: u64,
    completed_upload_serial: u64,
    pending_upload: Option<UploadBatch>,
    in_flight_uploads: Vec<UploadBatch>,
    command_pool: Option<vk::CommandPool>,
    pipeline_cache: Option<vk::PipelineCache>,
    pipeline_layout: Option<vk::PipelineLayout>,
    pipeline: Option<vk::Pipeline>,
    mask_pipeline: Option<vk::Pipeline>,
    blend_pipeline: Option<vk::Pipeline>,
    skybox_pipeline: Option<vk::Pipeline>,
    shadow_pipeline: Option<vk::Pipeline>,
    local_shadow_pipeline: Option<vk::Pipeline>,
    cluster_pipeline: Option<vk::Pipeline>,
    cluster_pipeline_layout: Option<vk::PipelineLayout>,
    post_pipeline_layout: Option<vk::PipelineLayout>,
    tonemap_pipeline: Option<vk::Pipeline>,
    output_pipeline: Option<vk::Pipeline>,
    viewport_output_pipeline: Option<vk::Pipeline>,
    overlay_pipeline_layout: Option<vk::PipelineLayout>,
    overlay_pipeline: Option<vk::Pipeline>,
    ui_pipeline_layout: Option<vk::PipelineLayout>,
    ui_pipeline: Option<vk::Pipeline>,
    /// Линейный sampler с `CLAMP_TO_EDGE`: `REPEAT` дал бы артефакты на краях атласа шрифтов.
    ui_sampler: Option<vk::Sampler>,
    /// Значащие биты timestamp очереди; ноль — измерение времени GPU недоступно.
    timestamp_valid_bits: u32,
    /// Наносекунд на один тик timestamp.
    timestamp_period: f32,
    pending_gpu_time: Option<Duration>,
    texture_sampler_anisotropy: Option<f32>,
    frame_set_layout: Option<vk::DescriptorSetLayout>,
    material_set_layout: Option<vk::DescriptorSetLayout>,
    environment_set_layout: Option<vk::DescriptorSetLayout>,
    compute_set_layout: Option<vk::DescriptorSetLayout>,
    texture_set_layout: Option<vk::DescriptorSetLayout>,
    frame_descriptor_pool: Option<vk::DescriptorPool>,
    post_descriptor_pool: Option<vk::DescriptorPool>,
    frame_uniforms: Vec<FrameUniforms>,
    sampler: Option<vk::Sampler>,
    post_sampler: Option<vk::Sampler>,
    shadow_sampler: Option<vk::Sampler>,
    environment_sampler: Option<vk::Sampler>,
    equirect_sampler: Option<vk::Sampler>,
    compute_pipeline_layout: Option<vk::PipelineLayout>,
    equirect_pipeline: Option<vk::Pipeline>,
    procedural_sky_pipeline: Option<vk::Pipeline>,
    irradiance_pipeline: Option<vk::Pipeline>,
    prefilter_pipeline: Option<vk::Pipeline>,
    brdf_image: Option<DeviceImage>,
    environments: Vec<Option<GpuEnvironment>>,
    shadow_image: Option<DeviceImage>,
    shadow_layout: vk::ImageLayout,
    local_shadow_image: Option<DeviceImage>,
    local_shadow_layout: vk::ImageLayout,
    max_texture_size: u32,
    /// Индекс — `MeshId`; слот удалённого меша остаётся пустым, id не используются повторно.
    meshes: Vec<Option<MeshBuffers>>,
    /// Индекс — `TextureId`; слот удалённой текстуры остаётся пустым.
    textures: Vec<Option<Arc<Texture>>>,
    /// Индекс — `MaterialId`; удалённые материалы остаются пустыми и не переиспользуются.
    materials: Vec<Option<GpuMaterial>>,
    ui_textures: Vec<Option<UiTexture>>,
    viewports: Vec<Option<Viewport>>,
    retired: RetireQueue<RetiredResource>,
    frames: Vec<FrameResources>,
    frame_slot: usize,
    swapchain: Option<SwapchainBundle>,
    surface_format: vk::SurfaceFormatKHR,
    present_mode: PresentMode,
    last_warned_present_mode: Option<PresentMode>,
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
            upload_alignment: 16,
            next_upload_serial: 1,
            completed_upload_serial: 0,
            pending_upload: None,
            in_flight_uploads: Vec::new(),
            command_pool: None,
            pipeline_cache: None,
            pipeline_layout: None,
            pipeline: None,
            mask_pipeline: None,
            blend_pipeline: None,
            skybox_pipeline: None,
            shadow_pipeline: None,
            local_shadow_pipeline: None,
            cluster_pipeline: None,
            cluster_pipeline_layout: None,
            post_pipeline_layout: None,
            tonemap_pipeline: None,
            output_pipeline: None,
            viewport_output_pipeline: None,
            overlay_pipeline_layout: None,
            overlay_pipeline: None,
            ui_pipeline_layout: None,
            ui_pipeline: None,
            ui_sampler: None,
            timestamp_valid_bits: 0,
            timestamp_period: 0.0,
            pending_gpu_time: None,
            texture_sampler_anisotropy: None,
            frame_set_layout: None,
            material_set_layout: None,
            environment_set_layout: None,
            compute_set_layout: None,
            texture_set_layout: None,
            frame_descriptor_pool: None,
            post_descriptor_pool: None,
            frame_uniforms: Vec::new(),
            sampler: None,
            post_sampler: None,
            shadow_sampler: None,
            environment_sampler: None,
            equirect_sampler: None,
            compute_pipeline_layout: None,
            equirect_pipeline: None,
            procedural_sky_pipeline: None,
            irradiance_pipeline: None,
            prefilter_pipeline: None,
            brdf_image: None,
            environments: Vec::new(),
            shadow_image: None,
            shadow_layout: vk::ImageLayout::UNDEFINED,
            local_shadow_image: None,
            local_shadow_layout: vk::ImageLayout::UNDEFINED,
            max_texture_size: 0,
            meshes: Vec::new(),
            textures: Vec::new(),
            materials: Vec::new(),
            ui_textures: Vec::new(),
            viewports: Vec::new(),
            retired: RetireQueue::new(FRAMES_IN_FLIGHT),
            frames: Vec::new(),
            frame_slot: 0,
            swapchain: None,
            surface_format: vk::SurfaceFormatKHR::default(),
            present_mode: config.present_mode,
            last_warned_present_mode: None,
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

    /// Returns the present mode used by the current swapchain.
    pub fn present_mode(&self) -> PresentMode {
        self.present_mode
    }

    /// Returns the pipeline cache data produced by this renderer.
    pub fn pipeline_cache_data(&self) -> Result<Vec<u8>, RenderError> {
        let cache = self
            .pipeline_cache
            .ok_or(RenderError::InvalidState("pipeline cache отсутствует"))?;
        // SAFETY: renderer creation and pipeline creation have completed on its owning thread.
        unsafe { self.device()?.get_pipeline_cache_data(cache) }.map_err(RenderError::from)
    }

    /// Забирает GPU-время clustered compute, shadow, scene и tonemap последнего завершённого кадра.
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

    /// Записывает асинхронную загрузку треугольного меша в GPU. Каждые три индекса образуют треугольник.
    /// Ошибка отправки пачки может вернуться из следующего `create_mesh`, `destroy_mesh` или кадра.
    pub fn create_mesh(
        &mut self,
        vertices: &[Vertex],
        indices: &[u32],
    ) -> Result<MeshId, RenderError> {
        validate_mesh(vertices.len(), indices)?;
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let bounds = LocalBounds::from_vertices(vertices);
        let vertex_data = vertex_bytes(vertices);
        let index_data = index_bytes(indices);
        let layout = mesh_upload_layout(
            0,
            vertex_data.len(),
            index_data.len(),
            self.upload_alignment,
        )
        .ok_or(RenderError::InvalidMesh(
            "размер меша переполняет диапазон буфера",
        ))?;
        let capacity = upload_batch_capacity(layout.end, self.upload_alignment).ok_or(
            RenderError::InvalidMesh("размер меша переполняет диапазон буфера"),
        )?;
        let must_flush = self.pending_upload.as_ref().is_some_and(|batch| {
            batch.oversized
                || upload_batch_flush_required(
                    batch.cursor,
                    vertex_data.len(),
                    index_data.len(),
                    self.upload_alignment,
                    batch.capacity,
                )
                .unwrap_or(true)
        });
        if must_flush {
            self.flush_uploads()?;
        }
        if self.pending_upload.is_none() {
            self.poll_uploads()?;
            self.pending_upload = Some(UploadBatch::new(
                &instance,
                &device,
                self.physical_device,
                self.queue_family_index,
                capacity,
                capacity > DEFAULT_UPLOAD_BATCH_CAPACITY,
                self.next_upload_serial,
            )?);
            self.next_upload_serial = self
                .next_upload_serial
                .checked_add(1)
                .ok_or(RenderError::InvalidState("upload serial исчерпан"))?;
        }
        let batch = self
            .pending_upload
            .as_mut()
            .ok_or(RenderError::InvalidState("upload batch отсутствует"))?;
        let layout = mesh_upload_layout(
            batch.cursor,
            vertex_data.len(),
            index_data.len(),
            self.upload_alignment,
        )
        .ok_or(RenderError::InvalidMesh(
            "размер меша переполняет диапазон буфера",
        ))?;
        let vertex_buffer = DeviceBuffer::new(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            vertex_data.len(),
        )?;
        let index_buffer = DeviceBuffer::new(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::INDEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            index_data.len(),
        )?;
        batch.staging.write_at(layout.vertex_offset, vertex_data)?;
        batch.staging.write_at(layout.index_offset, index_data)?;
        batch.record_mesh_copies(
            &vertex_buffer,
            &index_buffer,
            layout,
            vertex_data.len(),
            index_data.len(),
        );
        batch.cursor = layout.end;
        let upload_serial = batch.serial;
        self.meshes.push(Some(MeshBuffers {
            vertex_buffer,
            index_buffer,
            index_count: indices.len() as u32,
            bounds,
            upload_serial,
        }));
        Ok(MeshId(self.meshes.len() - 1))
    }

    /// Отправляет его ожидающую загрузку при необходимости; буферы освобождаются после upload и кадров.
    pub fn destroy_mesh(&mut self, mesh: MeshId) -> Result<(), RenderError> {
        let upload_serial = self
            .meshes
            .get(mesh.0)
            .and_then(Option::as_ref)
            .map(|buffers| buffers.upload_serial)
            .ok_or(RenderError::InvalidMesh("MeshId не принадлежит renderer"))?;
        self.flush_upload_serial(upload_serial)?;
        let buffers = self
            .meshes
            .get_mut(mesh.0)
            .and_then(Option::take)
            .ok_or(RenderError::InvalidMesh("MeshId не принадлежит renderer"))?;
        self.retired
            .retire(RetiredResource::Mesh(buffers), upload_serial);
        Ok(())
    }

    /// Записывает sRGB RGBA8 загрузку со строками сверху вниз в upload batch.
    /// Ошибка отправки batch может вернуться из следующего resource API или кадра.
    pub fn create_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<TextureId, RenderError> {
        self.create_texture_with_color_space(width, height, rgba, TextureColorSpace::Srgb)
    }

    /// Записывает RGBA8-текстуру заданного цветового пространства в upload batch.
    /// Ошибка отправки batch может вернуться из следующего resource API или кадра.
    pub fn create_texture_with_color_space(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        color_space: TextureColorSpace,
    ) -> Result<TextureId, RenderError> {
        validate_texture(width, height, rgba.len(), self.max_texture_size)?;
        let texture = Arc::new(self.upload_texture(width, height, rgba, color_space)?);
        self.textures.push(Some(texture));
        Ok(TextureId(self.textures.len() - 1))
    }

    /// Uploads an equirectangular Radiance image or procedural sky and precomputes its IBL maps.
    pub fn create_environment(
        &mut self,
        source: &EnvironmentSource,
    ) -> Result<EnvironmentId, RenderError> {
        environment::validate_source(source)?;
        if let EnvironmentSource::Equirect { width, height, .. } = source
            && (*width > self.max_texture_size || *height > self.max_texture_size)
        {
            return Err(RenderError::InvalidEnvironment(
                "equirect dimensions exceed the selected GPU image limit".to_owned(),
            ));
        }
        let environment = self.create_gpu_environment(source)?;
        self.environments.push(Some(environment));
        Ok(EnvironmentId(self.environments.len() - 1))
    }

    /// Retires an environment after submitted frames have stopped using its descriptor set.
    pub fn destroy_environment(&mut self, environment: EnvironmentId) -> Result<(), RenderError> {
        if environment.0 == 0 {
            return Err(RenderError::InvalidEnvironment(
                "built-in environment cannot be destroyed".to_owned(),
            ));
        }
        let resource = self
            .environments
            .get_mut(environment.0)
            .and_then(Option::take)
            .ok_or_else(|| {
                RenderError::InvalidEnvironment(
                    "EnvironmentId does not belong to renderer".to_owned(),
                )
            })?;
        self.retired
            .retire(RetiredResource::Environment(Box::new(resource)), 0);
        Ok(())
    }

    /// Создаёт неизменяемый материал, ссылающийся только на текстуры совместимого цветового пространства.
    pub fn create_material(&mut self, material: &Material) -> Result<MaterialId, RenderError> {
        let spaces = self
            .textures
            .iter()
            .map(|texture| texture.as_ref().map(|texture| texture.color_space))
            .collect::<Vec<_>>();
        validate_material(material, &spaces)?;
        let gpu_material = self.upload_material(material)?;
        self.materials.push(Some(gpu_material));
        Ok(MaterialId(self.materials.len() - 1))
    }

    /// Удаляет неизменяемый материал после завершения его кадровых обращений.
    pub fn destroy_material(&mut self, material: MaterialId) -> Result<(), RenderError> {
        if material.0 == DEFAULT_MATERIAL {
            return Err(RenderError::InvalidMaterial(
                "встроенный материал нельзя удалить".to_owned(),
            ));
        }
        let resource = self
            .materials
            .get_mut(material.0)
            .and_then(Option::take)
            .ok_or_else(|| {
                RenderError::InvalidMaterial("MaterialId не принадлежит renderer".to_owned())
            })?;
        self.retired.retire(RetiredResource::Material(resource), 0);
        Ok(())
    }

    /// Отправляет её ожидающую загрузку при необходимости; image ждёт upload и кадров перед освобождением.
    pub fn destroy_texture(&mut self, texture: TextureId) -> Result<(), RenderError> {
        let unknown =
            || RenderError::InvalidTexture("TextureId не принадлежит renderer".to_owned());
        // Встроенная белая текстура не выдаётся наружу и нужна всем DrawCommand без текстуры.
        if texture.0 <= FLAT_NORMAL_TEXTURE {
            return Err(unknown());
        }
        let upload_serial = self
            .textures
            .get(texture.0)
            .and_then(Option::as_ref)
            .map(|texture| texture.upload_serial)
            .ok_or_else(unknown)?;
        self.flush_upload_serial(upload_serial)?;
        let resource = self
            .textures
            .get_mut(texture.0)
            .and_then(Option::take)
            .ok_or_else(unknown)?;
        self.retired
            .retire(RetiredResource::Texture(resource), upload_serial);
        Ok(())
    }

    /// Наибольшая сторона изображения, поддерживаемая GPU.
    pub fn max_texture_size(&self) -> u32 {
        self.max_texture_size
    }

    /// Записывает UI-текстуру RGBA8 в upload batch: предумноженный цвет в гамма-пространстве, строки сверху вниз.
    /// Ошибка отправки batch может вернуться из следующего resource API или кадра.
    pub fn create_ui_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<UiTextureId, RenderError> {
        validate_texture(width, height, rgba.len(), self.max_texture_size)?;
        let sampler = self
            .ui_sampler
            .ok_or(RenderError::InvalidState("UI sampler отсутствует"))?;
        let device = self.device()?.clone();
        let image_set_layout = self.texture_set_layout()?;
        let (image, upload_serial) =
            self.upload_image(UI_TEXTURE_FORMAT, 1, width, height, rgba)?;
        let (pool, set) = match allocate_image_set(&device, image_set_layout, image.view, sampler) {
            Ok(resources) => resources,
            Err(error) => {
                self.retired
                    .retire(RetiredResource::Image(image), upload_serial);
                return Err(error);
            }
        };
        self.ui_textures.push(Some(UiTexture {
            device,
            pool,
            set,
            image,
            extent: vk::Extent2D { width, height },
            upload_serial,
        }));
        Ok(UiTextureId(self.ui_textures.len() - 1))
    }

    /// Перезаписывает область UI-текстуры и ждёт завершения копирования.
    ///
    /// Барьер в той же очереди упорядочивает запись относительно уже отправленных кадров.
    pub fn update_ui_texture(
        &mut self,
        texture: UiTextureId,
        region: [u32; 4],
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        let target = self.ui_texture(texture)?;
        validate_texture_region(region, target.extent, rgba.len())?;
        let upload_serial = target.upload_serial;
        let image = target.image.image;
        self.flush_upload_serial(upload_serial)?;
        let device = self.device()?;
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            rgba,
        )?;
        let [x, y, width, height] = region;
        self.submit_once(|command_buffer| {
            // Прежний layout сохраняет остальную часть изображения: это важно для атласа шрифтов.
            let to_transfer = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_subresource_range());
            pipeline_barrier(device, command_buffer, to_transfer);
            let copy = [vk::BufferImageCopy::default()
                .image_subresource(color_subresource_layers())
                .image_offset(vk::Offset3D {
                    x: x as i32,
                    y: y as i32,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            // SAFETY: the region was validated against the image and staging holds its texels.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &copy,
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
                .image(image)
                .subresource_range(color_subresource_range());
            pipeline_barrier(device, command_buffer, to_shader);
        })?;
        self.poll_uploads()?;
        Ok(())
    }

    /// Отправляет её ожидающую загрузку при необходимости; image ждёт upload и кадров перед освобождением.
    pub fn free_ui_texture(&mut self, texture: UiTextureId) -> Result<(), RenderError> {
        let upload_serial = self.ui_texture(texture)?.upload_serial;
        self.flush_upload_serial(upload_serial)?;
        if let Some(resource) = self.ui_textures[texture.0].take() {
            self.retired
                .retire(RetiredResource::UiTexture(resource), upload_serial);
        }
        Ok(())
    }

    /// Создаёт цель рендера сцены для UI. Изображения выделяются при первом кадре.
    pub fn create_viewport(&mut self, width: u32, height: u32) -> Result<ViewportId, RenderError> {
        validate_viewport_size(width, height, self.max_texture_size)?;
        self.viewports.push(Some(Viewport {
            extent: vk::Extent2D { width, height },
            targets: (0..FRAMES_IN_FLIGHT).map(|_| None).collect(),
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
        self.retired.retire(RetiredResource::Viewport(resource), 0);
        Ok(())
    }

    /// Очищает offscreen-таргет, рисует меши и масштабирует результат в swapchain.
    pub fn render_frame(&mut self, frame: &Frame<'_>) -> Result<FrameStatus, RenderError> {
        validate_draws(frame.draws, &self.meshes, &self.materials)?;
        validate_overlay(frame.overlay)?;
        let output_extent = self
            .swapchain
            .as_ref()
            .map_or(self.output_extent, |swapchain| swapchain.extent);
        let render_extent = scaled_extent(output_extent, frame.view.render_scale);
        let prepared = prepare_frame(frame, render_extent)?;
        let (environment_set, skybox) = self.environment_set_for(frame.environment)?;
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
        let frame_slot = self.frame_slot;
        let fence = self
            .frames
            .get(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?
            .fence;
        // SAFETY: this fence belongs to the frame slot and its prior submission is finished before reuse.
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }?;
        self.poll_uploads()?;
        self.flush_uploads()?;
        self.retired.slot_completed(frame_slot);
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let image_available = frame_resources.image_available;
        let command_buffer = frame_resources.command_buffer;
        let swapchain_handle = swapchain.handle;
        let image_count = swapchain.images.len();

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
                .get_mut(frame_slot)
                .ok_or(RenderError::InvalidState(
                    "нет uniform-буфера текущего кадра",
                ))?;
        // Fence слота дождались, поэтому GPU больше не читает этот буфер.
        frame_uniforms.buffer.write(&prepared.uniforms)?;
        frame_uniforms
            .local_shadow_matrices
            .write(&local_shadow_matrix_bytes(&prepared.local_shadow_matrices)?)?;
        let replaced_light_buffer = frame_uniforms.local_lights.write_keep_old(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            &prepared.light_bytes,
        )?;
        if replaced_light_buffer.is_some() {
            let light_buffer = frame_uniforms
                .local_lights
                .buffer
                .as_ref()
                .ok_or(RenderError::InvalidState("local light buffer отсутствует"))?;
            write_local_light_descriptor(
                &device,
                frame_uniforms.set,
                light_buffer,
                frame_uniforms.local_lights.capacity,
            );
        }
        drop(replaced_light_buffer);
        frame_uniforms
            .cluster_bounds
            .write(cluster_bounds_bytes(&prepared.cluster_bounds))?;
        let frame_set = frame_uniforms.set;
        // Цели создаются при первом кадре: редактору они не нужны. Выделение идёт до acquire,
        // чтобы ошибка памяти не оставила захваченный image и сигнальный semaphore. При смене
        // масштаба заменяется только target текущего слота после ожидания его fence.
        let post_sampler = self
            .post_sampler
            .ok_or(RenderError::InvalidState("post sampler отсутствует"))?;
        let target_changed = frame_resources
            .target
            .as_ref()
            .is_none_or(|target| target.render_extent != render_extent);
        if target_changed {
            let target = FrameTarget::new(&instance, &device, self.physical_device, render_extent)?;
            write_image_set(
                &device,
                frame_resources.tonemap_set,
                target.scene.color.view,
                post_sampler,
            );
            write_image_set(
                &device,
                frame_resources.output_set,
                target.ldr.view,
                post_sampler,
            );
            frame_resources.target = Some(target);
        }
        let Some((image_index, acquire_suboptimal)) = acquire_image(
            &swapchain_loader,
            swapchain_handle,
            image_available,
            image_count,
            &mut self.swapchain_dirty,
        )?
        else {
            return Ok(FrameStatus::Skipped);
        };

        let shadow = ShadowPass {
            pipeline: self
                .shadow_pipeline
                .ok_or(RenderError::InvalidState("shadow pipeline отсутствует"))?,
            local_pipeline: self.local_shadow_pipeline.ok_or(RenderError::InvalidState(
                "local shadow pipeline отсутствует",
            ))?,
            pipeline_layout: self
                .pipeline_layout
                .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?,
            frame_set,
            meshes: &self.meshes,
            materials: &self.materials,
            draws: frame.draws,
            shadow_cascades: frame.shadow_cascades,
            local_shadow_matrices: &prepared.local_shadow_matrices,
        };
        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let local_shadow_image = self
            .local_shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?;
        let scene = ScenePass {
            pipeline: self
                .pipeline
                .ok_or(RenderError::InvalidState("pipeline отсутствует"))?,
            mask_pipeline: self
                .mask_pipeline
                .ok_or(RenderError::InvalidState("mask pipeline отсутствует"))?,
            blend_pipeline: self
                .blend_pipeline
                .ok_or(RenderError::InvalidState("blend pipeline отсутствует"))?,
            pipeline_layout: self
                .pipeline_layout
                .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?,
            frame_set,
            cluster_pipeline: self
                .cluster_pipeline
                .ok_or(RenderError::InvalidState("cluster pipeline отсутствует"))?,
            cluster_pipeline_layout: self.cluster_pipeline_layout.ok_or(
                RenderError::InvalidState("cluster pipeline layout отсутствует"),
            )?,
            environment_set,
            skybox_pipeline: self
                .skybox_pipeline
                .ok_or(RenderError::InvalidState("skybox pipeline отсутствует"))?,
            skybox,
            meshes: &self.meshes,
            materials: &self.materials,
            draws: frame.draws,
            view_projection: multiply_4x4(frame.camera.projection, frame.camera.view),
            camera_position: frame.camera.position,
            clear_color: frame.clear_color,
            timestamps,
            view: frame.view,
        };
        let frame_resources = &mut self.frames[frame_slot];
        let overlay =
            match &frame_resources.overlay_buffer.buffer {
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
                }),
                _ => None,
            };
        let target = frame_resources
            .target
            .as_mut()
            .ok_or(RenderError::InvalidState("offscreen target отсутствует"))?;
        let mut shadow_layout = self.shadow_layout;
        let mut local_shadow_layout = self.local_shadow_layout;
        record_frame(
            &device,
            command_buffer,
            target,
            shadow_image,
            &mut shadow_layout,
            local_shadow_image,
            &mut local_shadow_layout,
            swapchain.images[image_index],
            swapchain.views[image_index],
            swapchain.extent,
            frame.view,
            frame_resources.tonemap_set,
            frame_resources.output_set,
            self.tonemap_pipeline
                .ok_or(RenderError::InvalidState("tonemap pipeline отсутствует"))?,
            self.output_pipeline
                .ok_or(RenderError::InvalidState("output pipeline отсутствует"))?,
            self.post_pipeline_layout.ok_or(RenderError::InvalidState(
                "post pipeline layout отсутствует",
            ))?,
            is_srgb_format(self.surface_format.format),
            &shadow,
            &scene,
            overlay.as_ref(),
        )?;
        self.shadow_layout = shadow_layout;
        self.local_shadow_layout = local_shadow_layout;

        self.submit_and_present(
            image_index,
            acquire_suboptimal,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            timestamps.is_some(),
        )
    }

    /// Рисует сцены в их viewport targets и UI прямо в swapchain image.
    ///
    /// `clear` очищает окно перед UI и задаётся в значениях формата swapchain. `Frame::overlay`
    /// здесь не используется. Каждый viewport может встречаться в `scenes` один раз, и
    /// `UiImage::Viewport` может ссылаться только на них; примитивы viewport нулевого размера
    /// пропускаются вместе с его scene pass. GPU-время измеряет только первую нарисованную сцену.
    pub fn render_frame_with_ui(
        &mut self,
        scenes: &[(ViewportId, &Frame<'_>)],
        ui: &[UiPrimitive<'_>],
        clear: [f32; 4],
    ) -> Result<FrameStatus, RenderError> {
        validate_scene_viewports(scenes.iter().map(|(viewport, _)| *viewport))?;
        let mut prepared_scenes = Vec::with_capacity(scenes.len());
        let mut scene_environment_sets = Vec::with_capacity(scenes.len());
        for (viewport, frame) in scenes {
            validate_draws(frame.draws, &self.meshes, &self.materials)?;
            self.viewports
                .get(viewport.0)
                .and_then(Option::as_ref)
                .ok_or_else(unknown_viewport)?;
            let extent = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .extent;
            prepared_scenes.push(prepare_frame(
                frame,
                scaled_extent(extent, frame.view.render_scale),
            )?);
            scene_environment_sets.push(self.environment_set_for(frame.environment)?);
        }
        let packed = pack_ui(ui, self.output_extent)?;
        self.validate_ui_images(&packed, scenes)?;
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
        let frame_slot = self.frame_slot;
        let fence = self
            .frames
            .get(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?
            .fence;
        // SAFETY: this fence belongs to the frame slot and its prior submission is finished before reuse.
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }?;
        self.poll_uploads()?;
        self.flush_uploads()?;
        self.retired.slot_completed(frame_slot);
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let command_buffer = frame_resources.command_buffer;
        if let Some(gpu_time) = frame_resources.read_gpu_time(
            &device,
            self.timestamp_valid_bits,
            self.timestamp_period,
        )? {
            self.pending_gpu_time = Some(gpu_time);
        }
        // Fence слота дождались, поэтому GPU больше не читает буферы этого слота.
        frame_resources.ui_vertices.write(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            &packed.vertices,
        )?;
        frame_resources.ui_indices.write(
            &instance,
            &device,
            self.physical_device,
            vk::BufferUsageFlags::INDEX_BUFFER,
            &packed.indices,
        )?;
        let image_available = frame_resources.image_available;
        // Fence слота завершён, поэтому прежние изображения и uniform-буферы слота свободны.
        // Выделение идёт до acquire, чтобы ошибка памяти не оставила захваченный image и
        // сигнальный semaphore.
        let target_resources = ViewportTargetResources {
            image_set_layout: self.texture_set_layout()?,
            image_sampler: self
                .ui_sampler
                .ok_or(RenderError::InvalidState("UI sampler отсутствует"))?,
            post_sampler: self
                .post_sampler
                .ok_or(RenderError::InvalidState("post sampler отсутствует"))?,
            frame_set_layout: self.frame_set_layout()?,
            shadow_view: self
                .shadow_image
                .as_ref()
                .ok_or(RenderError::InvalidState("shadow map отсутствует"))?
                .view,
            local_shadow_view: self
                .local_shadow_image
                .as_ref()
                .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?
                .view,
            shadow_sampler: self
                .shadow_sampler
                .ok_or(RenderError::InvalidState("shadow sampler отсутствует"))?,
        };
        let physical_device = self.physical_device;
        let mut rendered = Vec::with_capacity(scenes.len());
        for (((viewport, frame), prepared), (environment_set, skybox)) in scenes
            .iter()
            .zip(&prepared_scenes)
            .zip(&scene_environment_sets)
        {
            let entry = self.viewports[viewport.0]
                .as_mut()
                .ok_or_else(unknown_viewport)?;
            let extent = entry.extent;
            if extent.width == 0 || extent.height == 0 {
                continue;
            }
            let render_extent = scaled_extent(extent, frame.view.render_scale);
            let slot_target = &mut entry.targets[frame_slot];
            if slot_target.as_ref().is_none_or(|target| {
                target.extent != extent || target.render_extent != render_extent
            }) {
                *slot_target = Some(ViewportTarget::new(
                    &instance,
                    &device,
                    physical_device,
                    extent,
                    render_extent,
                    &target_resources,
                )?);
            }
            let target = slot_target.as_mut().ok_or_else(unknown_viewport)?;
            let frame_set = target.frame_set.write(
                &instance,
                &device,
                physical_device,
                &prepared.uniforms,
                &prepared.light_bytes,
                &local_shadow_matrix_bytes(&prepared.local_shadow_matrices)?,
                &prepared.cluster_bounds,
            )?;
            rendered.push((
                *viewport,
                *frame,
                frame_set,
                *environment_set,
                self.skybox_pipeline
                    .ok_or(RenderError::InvalidState("skybox pipeline отсутствует"))?,
                *skybox,
                target.tonemap_set,
                target.output_set,
                prepared.local_shadow_matrices.as_slice(),
            ));
        }

        let Some((image_index, acquire_suboptimal)) = acquire_image(
            &swapchain_loader,
            swapchain.handle,
            image_available,
            swapchain.images.len(),
            &mut self.swapchain_dirty,
        )?
        else {
            return Ok(FrameStatus::Skipped);
        };

        let mut draws = Vec::with_capacity(packed.draws.len());
        for draw in &packed.draws {
            let (set, image_is_linear) = match draw.image {
                UiImage::Texture(texture) => (self.ui_texture(texture)?.set, false),
                UiImage::Viewport(viewport) => {
                    if !rendered
                        .iter()
                        .any(|(drawn, _, _, _, _, _, _, _, _)| *drawn == viewport)
                    {
                        continue;
                    }
                    let target = self.viewports[viewport.0]
                        .as_ref()
                        .and_then(|entry| entry.targets[frame_slot].as_ref())
                        .ok_or_else(unknown_viewport)?;
                    (target.set, true)
                }
            };
            draws.push((*draw, set, image_is_linear));
        }

        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let pipeline_layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        let shadow_pipeline = self
            .shadow_pipeline
            .ok_or(RenderError::InvalidState("shadow pipeline отсутствует"))?;
        let local_shadow_pipeline = self.local_shadow_pipeline.ok_or(RenderError::InvalidState(
            "local shadow pipeline отсутствует",
        ))?;
        let scene_pipeline = self
            .pipeline
            .ok_or(RenderError::InvalidState("pipeline отсутствует"))?;
        let mask_pipeline = self
            .mask_pipeline
            .ok_or(RenderError::InvalidState("mask pipeline отсутствует"))?;
        let blend_pipeline = self
            .blend_pipeline
            .ok_or(RenderError::InvalidState("blend pipeline отсутствует"))?;
        let cluster_pipeline = self
            .cluster_pipeline
            .ok_or(RenderError::InvalidState("cluster pipeline отсутствует"))?;
        let cluster_pipeline_layout =
            self.cluster_pipeline_layout
                .ok_or(RenderError::InvalidState(
                    "cluster pipeline layout отсутствует",
                ))?;
        let slot_timestamps = self.frames[frame_slot].timestamps;
        let scene_passes = rendered
            .iter()
            .enumerate()
            .map(
                |(
                    index,
                    (
                        viewport,
                        frame,
                        frame_set,
                        environment_set,
                        skybox_pipeline,
                        skybox,
                        tonemap_set,
                        output_set,
                        local_shadow_matrices,
                    ),
                )| {
                    (
                        *viewport,
                        ShadowPass {
                            pipeline: shadow_pipeline,
                            local_pipeline: local_shadow_pipeline,
                            pipeline_layout,
                            frame_set: *frame_set,
                            meshes: &self.meshes,
                            materials: &self.materials,
                            draws: frame.draws,
                            shadow_cascades: frame.shadow_cascades,
                            local_shadow_matrices,
                        },
                        ScenePass {
                            pipeline: scene_pipeline,
                            mask_pipeline,
                            blend_pipeline,
                            cluster_pipeline,
                            cluster_pipeline_layout,
                            skybox_pipeline: *skybox_pipeline,
                            pipeline_layout,
                            frame_set: *frame_set,
                            environment_set: *environment_set,
                            skybox: *skybox,
                            meshes: &self.meshes,
                            materials: &self.materials,
                            draws: frame.draws,
                            view_projection: multiply_4x4(
                                frame.camera.projection,
                                frame.camera.view,
                            ),
                            camera_position: frame.camera.position,
                            clear_color: frame.clear_color,
                            timestamps: slot_timestamps.filter(|_| index == 0),
                            view: frame.view,
                        },
                        *tonemap_set,
                        *output_set,
                    )
                },
            )
            .collect::<Vec<_>>();
        let timestamps = !scene_passes.is_empty() && slot_timestamps.is_some();
        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let local_shadow_image = self
            .local_shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?;
        let ui_pass = UiPass {
            pipeline: self
                .ui_pipeline
                .ok_or(RenderError::InvalidState("UI pipeline отсутствует"))?,
            pipeline_layout: self
                .ui_pipeline_layout
                .ok_or(RenderError::InvalidState("UI pipeline layout отсутствует"))?,
            vertex_buffer: self.frames[frame_slot]
                .ui_vertices
                .buffer
                .as_ref()
                .map(|buffer| buffer.buffer),
            index_buffer: self.frames[frame_slot]
                .ui_indices
                .buffer
                .as_ref()
                .map(|buffer| buffer.buffer),
            framebuffer_is_srgb: is_srgb_format(self.surface_format.format),
            draws: &draws,
            view: swapchain.views[image_index],
            clear,
        };

        begin_command_buffer(&device, command_buffer)?;
        let mut shadow_layout = self.shadow_layout;
        let mut local_shadow_layout = self.local_shadow_layout;
        let tonemap_pipeline = self
            .tonemap_pipeline
            .ok_or(RenderError::InvalidState("tonemap pipeline отсутствует"))?;
        let viewport_output_pipeline =
            self.viewport_output_pipeline
                .ok_or(RenderError::InvalidState(
                    "viewport output pipeline отсутствует",
                ))?;
        let post_pipeline_layout = self.post_pipeline_layout.ok_or(RenderError::InvalidState(
            "post pipeline layout отсутствует",
        ))?;
        for (viewport, shadow, pass, tonemap_set, output_set) in &scene_passes {
            let Some(target) = self.viewports[viewport.0]
                .as_mut()
                .and_then(|entry| entry.targets[frame_slot].as_mut())
            else {
                continue;
            };
            record_scene(
                &device,
                command_buffer,
                &mut target.scene,
                shadow_image,
                &mut shadow_layout,
                local_shadow_image,
                &mut local_shadow_layout,
                target.render_extent,
                shadow,
                pass,
            );
            record_tonemap_pass(
                &device,
                command_buffer,
                &mut target.scene,
                &target.ldr,
                target.render_extent,
                *tonemap_set,
                tonemap_pipeline,
                post_pipeline_layout,
                pass.view,
                pass.timestamps,
            );
            record_output_pass(
                &device,
                command_buffer,
                target.output.image,
                target.output.view,
                target.render_extent,
                target.extent,
                *output_set,
                viewport_output_pipeline,
                post_pipeline_layout,
                pass.view.anti_aliasing,
                true,
                None,
                true,
            );
        }
        record_ui(
            &device,
            command_buffer,
            swapchain.images[image_index],
            swapchain.extent,
            &ui_pass,
        );
        // SAFETY: command recording is complete before the command buffer is submitted.
        unsafe { device.end_command_buffer(command_buffer) }?;
        self.shadow_layout = shadow_layout;
        self.local_shadow_layout = local_shadow_layout;

        self.submit_and_present(
            image_index,
            acquire_suboptimal,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            timestamps,
        )
    }

    fn ui_texture(&self, texture: UiTextureId) -> Result<&UiTexture, RenderError> {
        self.ui_textures
            .get(texture.0)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                RenderError::InvalidTexture("UiTextureId не принадлежит renderer".to_owned())
            })
    }

    fn validate_ui_images(
        &self,
        packed: &PackedUi,
        scenes: &[(ViewportId, &Frame<'_>)],
    ) -> Result<(), RenderError> {
        for draw in &packed.draws {
            match draw.image {
                UiImage::Texture(texture) => {
                    self.ui_texture(texture)?;
                }
                UiImage::Viewport(viewport)
                    if !scenes.iter().any(|(scene, _)| *scene == viewport) =>
                {
                    return Err(RenderError::InvalidTexture(
                        "UI ссылается на viewport, который не рисуется в этом кадре".to_owned(),
                    ));
                }
                UiImage::Viewport(_) => {}
            }
        }
        Ok(())
    }

    /// Отправляет записанный command buffer текущего слота и показывает изображение.
    ///
    /// `wait_stage` — стадия первого обращения к swapchain image в этом command buffer.
    fn submit_and_present(
        &mut self,
        image_index: usize,
        acquire_suboptimal: bool,
        wait_stage: vk::PipelineStageFlags2,
        timestamps_written: bool,
    ) -> Result<FrameStatus, RenderError> {
        let device = self.device()?.clone();
        let swapchain_loader = self.swapchain_loader()?.clone();
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let frame_slot = self.frame_slot;
        let fence = self.frames[frame_slot].fence;
        let image_available = self.frames[frame_slot].image_available;
        let command_buffer = self.frames[frame_slot].command_buffer;
        let swapchain_handle = swapchain.handle;

        // Fence сбрасывается только после успешного acquire и полной записи command buffer.
        // SAFETY: эта точка — непосредственно перед submit, которому принадлежит fence.
        unsafe { device.reset_fences(&[fence]) }?;

        let wait_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(image_available)
            .stage_mask(wait_stage)
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
        self.frames[frame_slot].timestamps_pending = timestamps_written;

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
                log::warn!(
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

        let required_device_extensions =
            required_device_extensions(&self.config.extra_device_extensions);
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
        self.texture_sampler_anisotropy = choice.texture_sampler_anisotropy;
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
            .tessellation_shader(self.capabilities.tessellation_shader)
            .shader_storage_image_extended_formats(true)
            .sampler_anisotropy(self.texture_sampler_anisotropy.is_some());
        let mut features_13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let mut extended_dynamic_state =
            vk::PhysicalDeviceExtendedDynamicStateFeaturesEXT::default()
                .extended_dynamic_state(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&extension_pointers)
            .enabled_features(&features)
            .push_next(&mut features_13)
            .push_next(&mut extended_dynamic_state);
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
        let properties = unsafe {
            self.instance()?
                .get_physical_device_properties(self.physical_device)
        };
        let cache_data = compatible_pipeline_cache(
            &self.config.pipeline_cache,
            PipelineCacheIdentity {
                vendor_id: properties.vendor_id,
                device_id: properties.device_id,
                pipeline_cache_uuid: properties.pipeline_cache_uuid,
            },
        );
        let cache_info = vk::PipelineCacheCreateInfo::default().initial_data(cache_data);
        // SAFETY: supplied cache data passed the Vulkan version-one device identity checks.
        self.pipeline_cache =
            Some(unsafe { self.device()?.create_pipeline_cache(&cache_info, None) }?);
        self.config.pipeline_cache = Vec::new();
        let limits = properties.limits;
        self.max_texture_size = limits.max_image_dimension2_d;
        self.upload_alignment = usize::try_from(limits.optimal_buffer_copy_offset_alignment)
            .map_err(|_| {
                RenderError::Unsupported("слишком большое выравнивание buffer copy".to_owned())
            })?
            .max(1);
        self.create_frame_resources()?;
        self.create_shadow_resources()?;
        self.create_descriptor_resources()?;
        self.create_cluster_pipeline()?;
        self.create_environment_resources()?;
        self.create_default_environment()?;
        self.create_mesh_pipeline()?;
        self.create_skybox_pipeline()?;
        self.create_shadow_pipeline()?;
        self.create_local_shadow_pipeline()?;
        self.create_post_pipelines()?;
        self.create_overlay_pipeline()?;
        self.create_ui_pipeline()?;
        let white_srgb = Arc::new(self.upload_texture(1, 1, &[255; 4], TextureColorSpace::Srgb)?);
        self.textures.push(Some(white_srgb));
        let white_linear =
            Arc::new(self.upload_texture(1, 1, &[255; 4], TextureColorSpace::Linear)?);
        self.textures.push(Some(white_linear));
        let flat_normal = Arc::new(self.upload_texture(
            1,
            1,
            &[128, 128, 255, 255],
            TextureColorSpace::Linear,
        )?);
        self.textures.push(Some(flat_normal));
        self.create_material(&Material::default())?;

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
                .descriptor_count((self.frames.len() * 2) as u32),
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
                .descriptor_count(post_set_count),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(post_set_count),
        ];
        let post_pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(post_set_count)
            .pool_sizes(&post_pool_sizes);
        // SAFETY: each frame slot owns a tonemap and output image set.
        let post_pool = unsafe { device.create_descriptor_pool(&post_pool_info, None) }?;
        self.post_descriptor_pool = Some(post_pool);
        let post_layouts = vec![self.texture_set_layout()?; post_set_count as usize];
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

    fn create_cluster_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let set_layouts = [self.frame_set_layout()?];
        let layout_info = vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts);
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.cluster_pipeline_layout = Some(layout);
        self.cluster_pipeline = Some(create_compute_pipeline(
            &device,
            cache,
            layout,
            CLUSTER_COMPUTE_SHADER,
        )?);
        Ok(())
    }

    fn create_environment_resources(&mut self) -> Result<(), RenderError> {
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

    fn create_default_environment(&mut self) -> Result<(), RenderError> {
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

    /// Создаёт directional и local depth-массивы и sampler сравнения для всех слотов кадров.
    fn create_shadow_resources(&mut self) -> Result<(), RenderError> {
        let image = DeviceImage::new_array(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::Extent2D {
                width: SHADOW_MAP_RESOLUTION,
                height: SHADOW_MAP_RESOLUTION,
            },
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            shadow_subresource_range(),
            SHADOW_CASCADE_COUNT as u32,
        )?;
        self.shadow_image = Some(image);
        let local_image = DeviceImage::new_array(
            self.instance()?,
            self.device()?,
            self.physical_device,
            vk::Extent2D {
                width: LOCAL_SHADOW_MAP_RESOLUTION,
                height: LOCAL_SHADOW_MAP_RESOLUTION,
            },
            DEPTH_FORMAT,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            local_shadow_subresource_range(),
            MAX_LOCAL_SHADOW_LAYERS as u32,
        )?;
        self.local_shadow_image = Some(local_image);
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .compare_enable(true)
            .compare_op(vk::CompareOp::LESS_OR_EQUAL)
            .max_lod(0.0);
        // SAFETY: linear depth comparison is required during physical-device selection.
        self.shadow_sampler = Some(unsafe { self.device()?.create_sampler(&sampler_info, None) }?);
        Ok(())
    }

    /// Записывает пиксели через batch staging-буфер в device-local image.
    fn upload_texture(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        color_space: TextureColorSpace,
    ) -> Result<Texture, RenderError> {
        let mip_levels = mip_level_count(width, height);
        let format = texture_format(color_space);
        let (image, upload_serial) = self.upload_image(format, mip_levels, width, height, rgba)?;
        Ok(Texture {
            image,
            color_space,
            upload_serial,
        })
    }

    fn upload_image(
        &mut self,
        format: vk::Format,
        mip_levels: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(DeviceImage, u64), RenderError> {
        let instance = self.instance()?.clone();
        let device = self.device()?.clone();
        let layout = upload_data_layout(0, rgba.len(), self.upload_alignment).ok_or(
            RenderError::InvalidTexture("размер данных текстуры переполняет DeviceSize".to_owned()),
        )?;
        let capacity = upload_batch_capacity(layout.1, self.upload_alignment).ok_or(
            RenderError::InvalidTexture("размер данных текстуры переполняет DeviceSize".to_owned()),
        )?;
        let must_flush = self.pending_upload.as_ref().is_some_and(|batch| {
            batch.oversized
                || upload_data_layout(batch.cursor, rgba.len(), self.upload_alignment)
                    .is_none_or(|(_, end)| end > batch.capacity)
        });
        if must_flush {
            self.flush_uploads()?;
        }
        if self.pending_upload.is_none() {
            self.poll_uploads()?;
            let serial = self.next_upload_serial;
            let next_serial = serial
                .checked_add(1)
                .ok_or(RenderError::InvalidState("upload serial исчерпан"))?;
            self.pending_upload = Some(UploadBatch::new(
                &instance,
                &device,
                self.physical_device,
                self.queue_family_index,
                capacity,
                capacity > DEFAULT_UPLOAD_BATCH_CAPACITY,
                serial,
            )?);
            self.next_upload_serial = next_serial;
        }
        let image = DeviceImage::new(
            &instance,
            &device,
            self.physical_device,
            vk::Extent2D { width, height },
            format,
            vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST,
            color_mip_subresource_range(0, mip_levels),
        )?;
        let batch = self
            .pending_upload
            .as_mut()
            .ok_or(RenderError::InvalidState("upload batch отсутствует"))?;
        let layout = upload_data_layout(batch.cursor, rgba.len(), self.upload_alignment).ok_or(
            RenderError::InvalidTexture("размер данных текстуры переполняет DeviceSize".to_owned()),
        )?;
        batch.staging.write_at(layout.0, rgba)?;
        batch.record_image_upload(image.image, width, height, mip_levels, layout.0);
        batch.cursor = layout.1;
        let upload_serial = batch.serial;
        Ok((image, upload_serial))
    }

    fn upload_16bit_image(
        &self,
        width: u32,
        height: u32,
        format: vk::Format,
        bytes_per_texel: usize,
        pixels: &[u8],
    ) -> Result<DeviceImage, RenderError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|count| count.checked_mul(bytes_per_texel));
        if width == 0 || height == 0 || expected != Some(pixels.len()) {
            return Err(RenderError::InvalidEnvironment(
                "16-bit image data does not match its dimensions".to_owned(),
            ));
        }
        let device = self.device()?;
        let image = DeviceImage::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::Extent2D { width, height },
            format,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            color_mip_subresource_range(0, 1),
        )?;
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            pixels,
        )?;
        self.submit_once(|command_buffer| {
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::PipelineStageFlags2::NONE,
                    vk::AccessFlags2::NONE,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    color_subresource_range(),
                ),
            );
            let region = [vk::BufferImageCopy::default()
                .buffer_offset(0)
                .image_subresource(color_subresource_layers())
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })];
            // SAFETY: staging contains the checked texel byte count and the image is transfer-dst.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                )
            };
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    vk::PipelineStageFlags2::COMPUTE_SHADER
                        | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    color_subresource_range(),
                ),
            );
        })?;
        Ok(image)
    }

    fn upload_cube_constant(&self, texel: [u16; 4]) -> Result<DeviceImage, RenderError> {
        let device = self.device()?;
        let image = DeviceImage::new_cube(
            self.instance()?,
            device,
            self.physical_device,
            1,
            OFFSCREEN_FORMAT,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            1,
        )?;
        let mut bytes = Vec::with_capacity(6 * size_of::<[u16; 4]>());
        for _ in 0..6 {
            for channel in texel {
                bytes.extend_from_slice(&channel.to_ne_bytes());
            }
        }
        let staging = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            &bytes,
        )?;
        self.submit_once(|command_buffer| {
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::PipelineStageFlags2::NONE,
                    vk::AccessFlags2::NONE,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    color_mip_subresource_range(0, 1).layer_count(6),
                ),
            );
            let region = [vk::BufferImageCopy::default()
                .buffer_offset(0)
                .image_subresource(color_subresource_layers().layer_count(6))
                .image_extent(vk::Extent3D {
                    width: 1,
                    height: 1,
                    depth: 1,
                })];
            // SAFETY: the staging buffer contains one RGBA16F texel for each cube face.
            unsafe {
                device.cmd_copy_buffer_to_image(
                    command_buffer,
                    staging.buffer,
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                )
            };
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::PipelineStageFlags2::TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                    vk::PipelineStageFlags2::FRAGMENT_SHADER
                        | vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    color_mip_subresource_range(0, 1).layer_count(6),
                ),
            );
        })?;
        Ok(image)
    }

    fn make_gpu_environment(
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

    fn create_gpu_environment(
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

    fn upload_material(&self, material: &Material) -> Result<GpuMaterial, RenderError> {
        let device = self.device()?;
        let uniform_buffer = HostBuffer::new(
            self.instance()?,
            device,
            self.physical_device,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            &material_uniform_bytes(material),
        )?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(5),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(1),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        // SAFETY: the pool holds one material uniform, five images and one sampler.
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
            let image_infos: [vk::DescriptorImageInfo; 5] = std::array::from_fn(|index| {
                vk::DescriptorImageInfo::default()
                    .image_view(texture_refs[index].image.view)
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
                    .image_info(&image_infos[0..1]),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[1..2]),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(3)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[2..3]),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(4)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[3..4]),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos[4..5]),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(6)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&sampler_info),
            ];
            // SAFETY: texture_refs keep every image alive until this material descriptor pool drops.
            unsafe { device.update_descriptor_sets(&writes, &[]) };
            Ok(GpuMaterial {
                device: device.clone(),
                pool,
                set,
                double_sided: material.double_sided,
                _uniform_buffer: uniform_buffer,
                _textures: texture_refs,
                alpha_mode: material.alpha_mode,
            })
        })();
        if result.is_err() {
            // SAFETY: allocation failed before the descriptor pool was published or used.
            unsafe { device.destroy_descriptor_pool(pool, None) };
        }
        result
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

    fn flush_uploads(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let Some(batch) = self.pending_upload.as_mut() else {
            return Ok(());
        };
        if batch.cursor == 0 {
            self.pending_upload.take();
            return Ok(());
        }
        if batch.recording {
            // SAFETY: this batch's command buffer is recording on the renderer thread and has not been submitted.
            unsafe { device.end_command_buffer(batch.command_buffer) }?;
            batch.recording = false;
        }
        if batch.fence.is_none() {
            // SAFETY: a default fence is unsignaled and belongs to this upload submission.
            batch.fence =
                Some(unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?);
        }
        let command_infos = [vk::CommandBufferSubmitInfo::default()
            .command_buffer(batch.command_buffer)
            .device_mask(0)];
        let submit_info = [vk::SubmitInfo2::default().command_buffer_infos(&command_infos)];
        let fence = batch.fence.ok_or(RenderError::InvalidState(
            "upload fence отсутствует после создания",
        ))?;
        // SAFETY: the executable batch command buffer and unsignaled fence are owned by this renderer.
        unsafe { device.queue_submit2(self.queue, &submit_info, fence) }?;
        let batch = self.pending_upload.take().ok_or(RenderError::InvalidState(
            "upload batch отсутствует после submit",
        ))?;
        self.in_flight_uploads.push(batch);
        Ok(())
    }

    fn flush_upload_serial(&mut self, serial: u64) -> Result<(), RenderError> {
        if self
            .pending_upload
            .as_ref()
            .is_some_and(|batch| batch.serial == serial)
        {
            self.flush_uploads()?;
        }
        Ok(())
    }

    fn poll_uploads(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let completed = completed_upload_count(self.in_flight_uploads.iter().map(|batch| {
            let fence = batch
                .fence
                .ok_or(RenderError::InvalidState("upload fence отсутствует"))?;
            // SAFETY: each fence belongs to an upload submission on this device.
            unsafe { device.get_fence_status(fence) }.map_err(RenderError::from)
        }))?;
        // Uploads share one queue: only the completed prefix can be reclaimed.
        for batch in self.in_flight_uploads.drain(..completed) {
            self.completed_upload_serial = self.completed_upload_serial.max(batch.serial);
        }
        self.retired.upload_completed(self.completed_upload_serial);
        Ok(())
    }

    fn create_mesh_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(PUSH_CONSTANTS_SIZE as u32)];
        let set_layouts = [
            self.frame_set_layout()?,
            self.material_set_layout()?,
            self.environment_set_layout
                .ok_or(RenderError::InvalidState(
                    "environment descriptor layout отсутствует",
                ))?,
        ];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the vertex push-constant range fits in the guaranteed 128 bytes.
        self.pipeline_layout = Some(unsafe { device.create_pipeline_layout(&layout_info, None) }?);

        let vertex_module = ShaderModule::new(&device, MESH_VERTEX_SHADER)?;
        let opaque_module = ShaderModule::new(&device, MESH_OPAQUE_FRAGMENT_SHADER)?;
        let mask_module = ShaderModule::new(&device, MESH_MASK_FRAGMENT_SHADER)?;
        let blend_module = ShaderModule::new(&device, MESH_BLEND_FRAGMENT_SHADER)?;
        let vertex_stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex_module.handle)
            .name(c"main");
        let opaque_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(opaque_module.handle)
                .name(c"main"),
        ];
        let mask_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(mask_module.handle)
                .name(c"main"),
        ];
        let blend_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(blend_module.handle)
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
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::BACK)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let transparent_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .color_write_mask(
                vk::ColorComponentFlags::R
                    | vk::ColorComponentFlags::G
                    | vk::ColorComponentFlags::B,
            )];
        let transparent_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&transparent_attachments);
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let transparent_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::LESS);
        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
            vk::DynamicState::CULL_MODE,
            vk::DynamicState::FRONT_FACE,
        ];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [OFFSCREEN_FORMAT];
        let mut opaque_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut mask_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut blend_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let pipeline_layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        let pipeline_info = [
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&depth_stencil)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut opaque_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&mask_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&depth_stencil)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut mask_rendering_info),
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&blend_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&transparent_depth_stencil)
                .color_blend_state(&transparent_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut blend_rendering_info),
        ];
        // SAFETY: shader modules, layout and dynamic-rendering formats are valid for this device.
        let mut pipelines = match unsafe {
            device.create_graphics_pipelines(self.pipeline_cache()?, &pipeline_info, None)
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
        if pipelines.len() != 3 {
            for pipeline in pipelines {
                // SAFETY: these pipeline handles were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(RenderError::InvalidState(
                "mesh pipelines не созданы полностью",
            ));
        }
        self.pipeline = Some(pipelines.remove(0));
        self.mask_pipeline = Some(pipelines.remove(0));
        self.blend_pipeline = Some(pipelines.remove(0));
        Ok(())
    }

    fn create_skybox_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        self.skybox_pipeline = Some(create_skybox_graphics_pipeline(&device, cache, layout)?);
        Ok(())
    }

    /// Pipeline для depth-only shadow pass с фиксированным depth bias.
    fn create_shadow_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        self.shadow_pipeline = Some(create_shadow_graphics_pipeline(
            &device,
            cache,
            layout,
            SHADOW_VERTEX_SHADER,
        )?);
        Ok(())
    }

    fn create_local_shadow_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        self.local_shadow_pipeline = Some(create_shadow_graphics_pipeline(
            &device,
            cache,
            layout,
            LOCAL_SHADOW_VERTEX_SHADER,
        )?);
        Ok(())
    }

    fn create_post_pipelines(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let cache = self.pipeline_cache()?;
        let set_layouts = [self.texture_set_layout()?];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(POST_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the image/sampler set layout and 16-byte fragment range are valid.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.post_pipeline_layout = Some(layout);
        self.tonemap_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            layout,
            LDR_FORMAT,
            TONEMAP_FRAGMENT_SHADER,
        )?);
        self.output_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            layout,
            self.surface_format.format,
            OUTPUT_FRAGMENT_SHADER,
        )?);
        self.viewport_output_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            layout,
            LDR_FORMAT,
            OUTPUT_FRAGMENT_SHADER,
        )?);
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
            device.create_graphics_pipelines(self.pipeline_cache()?, &pipeline_info, None)
        }
        .map_err(|(_, error)| RenderError::Vulkan(error))?;
        self.overlay_pipeline = pipelines.into_iter().next();
        Ok(())
    }

    /// Pipeline UI: индексированные треугольники с предумноженной альфой прямо в swapchain image.
    fn create_ui_pipeline(&mut self) -> Result<(), RenderError> {
        let device = self.device()?.clone();
        let sampler_info = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .max_lod(0.0);
        // SAFETY: the sampler uses core linear filtering and clamp-to-edge addressing.
        self.ui_sampler = Some(unsafe { device.create_sampler(&sampler_info, None) }?);

        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(UI_PUSH_CONSTANTS_SIZE as u32)];
        let set_layouts = [self.texture_set_layout()?];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout is alive and the push-constant range fits in the guaranteed 128 bytes.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        self.ui_pipeline_layout = Some(layout);

        let vertex_module = ShaderModule::new(&device, UI_VERTEX_SHADER)?;
        let fragment_module = ShaderModule::new(&device, UI_FRAGMENT_SHADER)?;
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
            .stride(size_of::<UiVertex>() as u32)
            .input_rate(vk::VertexInputRate::VERTEX)];
        let vertex_attributes = [
            vk::VertexInputAttributeDescription::default()
                .location(0)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(UiVertex, position) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(1)
                .binding(0)
                .format(vk::Format::R32G32_SFLOAT)
                .offset(std::mem::offset_of!(UiVertex, uv) as u32),
            vk::VertexInputAttributeDescription::default()
                .location(2)
                .binding(0)
                .format(vk::Format::R8G8B8A8_UNORM)
                .offset(std::mem::offset_of!(UiVertex, color) as u32),
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
            .src_color_blend_factor(vk::BlendFactor::ONE)
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
            device.create_graphics_pipelines(self.pipeline_cache()?, &pipeline_info, None)
        }
        .map_err(|(_, error)| RenderError::Vulkan(error))?;
        self.ui_pipeline = pipelines.into_iter().next();
        Ok(())
    }

    fn recreate_swapchain(&mut self) -> Result<(), RenderError> {
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            return Ok(());
        }
        self.flush_uploads()?;
        // SAFETY: recreation waits for work submitted by this device before replacing its frame targets.
        unsafe { self.device()?.device_wait_idle() }?;
        self.poll_uploads()?;
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
            self.config.present_mode,
        )?;
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
        self.swapchain = Some(new_swapchain);
        for frame in &mut self.frames {
            frame.target = None;
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

    fn pipeline_cache(&self) -> Result<vk::PipelineCache, RenderError> {
        self.pipeline_cache
            .ok_or(RenderError::InvalidState("pipeline cache отсутствует"))
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

    fn environment_set_for(
        &self,
        environment: Option<FrameEnvironment>,
    ) -> Result<(vk::DescriptorSet, bool), RenderError> {
        let id = environment.map_or(EnvironmentId(0), |environment| environment.id);
        let set = self
            .environments
            .get(id.0)
            .and_then(Option::as_ref)
            .map(|environment| environment.set)
            .ok_or_else(|| {
                RenderError::InvalidEnvironment(
                    "EnvironmentId не принадлежит renderer или уже удалён".to_owned(),
                )
            })?;
        Ok((
            set,
            environment.is_some_and(|environment| environment.skybox),
        ))
    }

    fn material_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.material_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов материала отсутствует",
        ))
    }

    fn texture_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.texture_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов UI-текстуры отсутствует",
        ))
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(device) = self.device.as_ref() {
            // SAFETY: teardown waits for renderer submissions before destroying owned Vulkan resources.
            let _ = unsafe { device.device_wait_idle() };
        }
        self.pending_upload.take();
        self.in_flight_uploads.clear();
        self.swapchain.take();
        self.frames.clear();
        self.retired.clear();
        self.environments.clear();
        self.viewports.clear();
        self.ui_textures.clear();
        self.meshes.clear();
        self.materials.clear();
        self.textures.clear();
        if let Some(device) = self.device.as_ref() {
            if let Some(pool) = self.frame_descriptor_pool.take() {
                // SAFETY: the device is idle; destroying the pool frees its frame sets.
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            self.frame_uniforms.clear();
            if let Some(pool) = self.post_descriptor_pool.take() {
                // SAFETY: frame targets are gone; destroying the pool frees their post sets.
                unsafe { device.destroy_descriptor_pool(pool, None) };
            }
            if let Some(sampler) = self.sampler.take() {
                // SAFETY: every material descriptor referencing the sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.ui_sampler.take() {
                // SAFETY: every UI texture and viewport descriptor referencing the sampler is gone.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.post_sampler.take() {
                // SAFETY: every post descriptor pool referencing this sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.shadow_sampler.take() {
                // SAFETY: the frame descriptor pool referencing the sampler was destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.environment_sampler.take() {
                // SAFETY: all environment descriptor pools were destroyed above.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(sampler) = self.equirect_sampler.take() {
                // SAFETY: environment baking is synchronous and all compute sets were freed.
                unsafe { device.destroy_sampler(sampler, None) };
            }
            if let Some(image) = self.brdf_image.take() {
                // SAFETY: environment descriptor sets were destroyed above.
                drop(image);
            }
            if let Some(image) = self.shadow_image.take() {
                // SAFETY: the frame descriptor pool is gone and the device is idle.
                drop(image);
            }
            if let Some(image) = self.local_shadow_image.take() {
                // SAFETY: the frame descriptor pool is gone and the device is idle.
                drop(image);
            }
            if let Some(pipeline) = self.shadow_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(pipeline) = self.local_shadow_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            for pipeline in [
                self.pipeline.take(),
                self.mask_pipeline.take(),
                self.blend_pipeline.take(),
                self.skybox_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            for pipeline in [
                self.tonemap_pipeline.take(),
                self.output_pipeline.take(),
                self.viewport_output_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the device is idle, so no command buffer uses these pipelines.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            for pipeline in [
                self.equirect_pipeline.take(),
                self.procedural_sky_pipeline.take(),
                self.irradiance_pipeline.take(),
                self.prefilter_pipeline.take(),
                self.cluster_pipeline.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: synchronous environment bakes and all frame submissions have completed.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.post_pipeline_layout.take() {
                // SAFETY: all pipelines created with this layout were destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.compute_pipeline_layout.take() {
                // SAFETY: every compute pipeline using this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            if let Some(layout) = self.cluster_pipeline_layout.take() {
                // SAFETY: the clustered compute pipeline was destroyed above.
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
            if let Some(pipeline) = self.ui_pipeline.take() {
                // SAFETY: the device is idle, so no command buffer uses this pipeline.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            if let Some(layout) = self.ui_pipeline_layout.take() {
                // SAFETY: the only pipeline created with this layout was destroyed above.
                unsafe { device.destroy_pipeline_layout(layout, None) };
            }
            for layout in [
                self.frame_set_layout.take(),
                self.material_set_layout.take(),
                self.environment_set_layout.take(),
                self.compute_set_layout.take(),
                self.texture_set_layout.take(),
            ]
            .into_iter()
            .flatten()
            {
                // SAFETY: the pipeline layout and all descriptor pools using it are gone.
                unsafe { device.destroy_descriptor_set_layout(layout, None) };
            }
            if let Some(cache) = self.pipeline_cache.take() {
                // SAFETY: every pipeline created from the cache was destroyed above.
                unsafe { device.destroy_pipeline_cache(cache, None) };
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
    texture_sampler_anisotropy: Option<f32>,
    timestamp_valid_bits: u32,
    timestamp_period: f32,
}

struct DeviceFacts {
    api_version: u32,
    dynamic_rendering: bool,
    synchronization2: bool,
    extended_dynamic_state: bool,
    supported_extensions: HashSet<String>,
    graphics_queue_families: Vec<bool>,
    compute_queue_families: Vec<bool>,
    storage_image_extended_formats: bool,
    offscreen_format_features: vk::FormatFeatureFlags,
}

struct DeviceEvaluation {
    reasons: Vec<String>,
    graphics_queue_family_indices: Vec<u32>,
}

struct DeviceInspection {
    report: DeviceReport,
    tessellation_shader: bool,
    texture_sampler_anisotropy: Option<f32>,
    timestamp_period: f32,
    queue_families: Vec<vk::QueueFamilyProperties>,
    graphics_queue_family_indices: Vec<u32>,
}

fn required_device_extensions(extra: &[&CStr]) -> Vec<CString> {
    let mut required = extra
        .iter()
        .map(|name| CString::from(*name))
        .collect::<Vec<_>>();
    required.push(ash::khr::swapchain::NAME.to_owned());
    required.extend(
        engine_platform::DEVICE_EXTENSIONS
            .iter()
            .map(|name| CString::from(*name)),
    );
    deduplicate_names(required)
}

fn evaluate_device(facts: &DeviceFacts, required_extensions: &[CString]) -> DeviceEvaluation {
    let mut reasons = Vec::new();
    if vk::api_version_major(facts.api_version) < 1
        || (vk::api_version_major(facts.api_version) == 1
            && vk::api_version_minor(facts.api_version) < 3)
    {
        reasons.push("Vulkan API ниже 1.3".to_owned());
    }
    if !facts.dynamic_rendering {
        reasons.push("не поддерживается dynamicRendering".to_owned());
    }
    if !facts.synchronization2 {
        reasons.push("не поддерживается synchronization2".to_owned());
    }
    if !facts.storage_image_extended_formats {
        reasons.push("не поддерживается shaderStorageImageExtendedFormats".to_owned());
    }
    let missing_offscreen = missing_offscreen_format_features(facts.offscreen_format_features);
    if !missing_offscreen.is_empty() {
        reasons.push(format!(
            "R16G16B16A16_SFLOAT не поддерживает HDR scene и compute storage: {}",
            missing_offscreen.join(", ")
        ));
    }
    if !facts.extended_dynamic_state {
        reasons.push("не поддерживается extendedDynamicState".to_owned());
    }
    for extension in required_extensions {
        let name = extension.to_string_lossy();
        if !facts.supported_extensions.contains(name.as_ref()) {
            reasons.push(format!("device extension недоступен: {name}"));
        }
    }
    let graphics_queue_family_indices = facts
        .graphics_queue_families
        .iter()
        .enumerate()
        .filter_map(|(index, supports_graphics)| {
            (*supports_graphics
                && facts
                    .compute_queue_families
                    .get(index)
                    .copied()
                    .unwrap_or(false))
            .then_some(index as u32)
        })
        .collect::<Vec<_>>();
    if graphics_queue_family_indices.is_empty() {
        if facts
            .graphics_queue_families
            .iter()
            .any(|supports| *supports)
        {
            reasons.push(NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON.to_owned());
        } else {
            reasons.push(NO_GRAPHICS_QUEUE_FAMILY_REASON.to_owned());
        }
    }
    DeviceEvaluation {
        reasons,
        graphics_queue_family_indices,
    }
}

fn inspect_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    required_extensions: &[CString],
) -> Result<DeviceInspection, RenderError> {
    // SAFETY: physical_device was returned by the live instance.
    let properties = unsafe { instance.get_physical_device_properties(physical_device) };
    let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // SAFETY: device feature query uses a valid physical device and a properly initialized pNext chain.
    let (tessellation_shader, storage_image_extended_formats, texture_sampler_anisotropy) = {
        let mut features_2 = vk::PhysicalDeviceFeatures2::default();
        // SAFETY: querying core features uses a valid physical device and null pNext chain.
        unsafe { instance.get_physical_device_features2(physical_device, &mut features_2) };
        (
            features_2.features.tessellation_shader != vk::FALSE,
            features_2.features.shader_storage_image_extended_formats != vk::FALSE,
            sampler_anisotropy(
                features_2.features.sampler_anisotropy != vk::FALSE,
                properties.limits.max_sampler_anisotropy,
            ),
        )
    };
    let mut features_13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut extended_dynamic_state = vk::PhysicalDeviceExtendedDynamicStateFeaturesEXT::default();
    let mut features_2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut features_13)
        .push_next(&mut extended_dynamic_state);
    // SAFETY: querying Vulkan 1.3 and extension feature structures uses a valid pNext chain.
    unsafe { instance.get_physical_device_features2(physical_device, &mut features_2) };
    let dynamic_rendering = features_13.dynamic_rendering != vk::FALSE;
    let synchronization2 = features_13.synchronization2 != vk::FALSE;

    // SAFETY: extension enumeration is read-only for the live physical device.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let supported_extensions = extensions
        .iter()
        .map(|extension| {
            // SAFETY: Vulkan extension property names are fixed-size null-terminated strings.
            unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let queue_families =
        // SAFETY: queue-family properties are read from the live physical device.
        unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    let offscreen_format_features = unsafe {
        instance.get_physical_device_format_properties(physical_device, OFFSCREEN_FORMAT)
    }
    .optimal_tiling_features;
    let facts = DeviceFacts {
        api_version: properties.api_version,
        dynamic_rendering,
        synchronization2,
        extended_dynamic_state: extended_dynamic_state.extended_dynamic_state != vk::FALSE,
        supported_extensions,
        graphics_queue_families: queue_families
            .iter()
            .map(|family| {
                family.queue_count > 0 && family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
            })
            .collect(),
        compute_queue_families: queue_families
            .iter()
            .map(|family| {
                family.queue_count > 0 && family.queue_flags.contains(vk::QueueFlags::COMPUTE)
            })
            .collect(),
        storage_image_extended_formats,
        offscreen_format_features,
    };
    let evaluation = evaluate_device(&facts, required_extensions);
    let srgb_features = unsafe {
        instance.get_physical_device_format_properties(physical_device, SRGB_TEXTURE_FORMAT)
    }
    .optimal_tiling_features;
    let linear_features = unsafe {
        instance.get_physical_device_format_properties(physical_device, LINEAR_TEXTURE_FORMAT)
    }
    .optimal_tiling_features;
    let mut reasons = evaluation.reasons;
    reasons.extend(cluster_device_limit_reasons(&properties.limits));
    reasons.extend(texture_format_reasons(srgb_features, linear_features));
    reasons.extend(shadow_image_device_reasons(instance, physical_device));
    let report = DeviceReport {
        name,
        vendor_id: properties.vendor_id,
        api_version: properties.api_version,
        discrete: properties.device_type == vk::PhysicalDeviceType::DISCRETE_GPU,
        suitable: reasons.is_empty(),
        reasons,
    };

    Ok(DeviceInspection {
        report,
        tessellation_shader,
        texture_sampler_anisotropy,
        timestamp_period: properties.limits.timestamp_period,
        queue_families,
        graphics_queue_family_indices: evaluation.graphics_queue_family_indices,
    })
}

fn cluster_device_limit_reasons(limits: &vk::PhysicalDeviceLimits) -> Vec<String> {
    let workgroup_count = (CLUSTER_COUNT as u32).div_ceil(CLUSTER_COMPUTE_LOCAL_SIZE);
    let mut reasons = Vec::new();
    if limits.max_compute_work_group_invocations < CLUSTER_COMPUTE_LOCAL_SIZE
        || limits.max_compute_work_group_size[0] < CLUSTER_COMPUTE_LOCAL_SIZE
        || limits.max_compute_work_group_count[0] < workgroup_count
    {
        reasons.push("лимиты compute workgroup не поддерживают clustered lighting".to_owned());
    }
    if limits.max_storage_buffer_range < CLUSTER_BUFFER_SIZE as u32 {
        reasons.push("maxStorageBufferRange меньше фиксированного cluster SSBO".to_owned());
    }
    if limits.max_per_stage_descriptor_storage_buffers < 3
        || limits.max_descriptor_set_storage_buffers < 4
    {
        reasons.push(
            "лимиты storage buffer descriptors не поддерживают clustered/local shadow lighting"
                .to_owned(),
        );
    }
    reasons
}

fn shadow_image_device_reasons(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<String> {
    // SAFETY: format support is queried for this physical device.
    let format_properties =
        unsafe { instance.get_physical_device_format_properties(physical_device, DEPTH_FORMAT) };
    let required_features = vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT
        | vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    let mut reasons = Vec::new();
    if !format_properties
        .optimal_tiling_features
        .contains(required_features)
    {
        reasons.push(
            "D32_SFLOAT не поддерживает depth attachment и linear comparison sampling shadow maps"
                .to_owned(),
        );
    }
    // SAFETY: image-format properties are queried for the fixed depth/sample usage.
    match unsafe {
        instance.get_physical_device_image_format_properties(
            physical_device,
            DEPTH_FORMAT,
            vk::ImageType::TYPE_2D,
            vk::ImageTiling::OPTIMAL,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageCreateFlags::empty(),
        )
    } {
        Ok(properties) if shadow_array_limits_suitable(&properties) => {}
        Ok(_) => reasons.push(format!(
            "D32_SFLOAT image limits do not support the {}-layer {}x{} local/directional shadow arrays",
            MAX_LOCAL_SHADOW_LAYERS, SHADOW_MAP_RESOLUTION, SHADOW_MAP_RESOLUTION
        )),
        Err(error) => reasons.push(format!(
            "D32_SFLOAT image usage does not support shadow arrays: {error}"
        )),
    }
    reasons
}

fn shadow_array_limits_suitable(properties: &vk::ImageFormatProperties) -> bool {
    properties.max_array_layers >= MAX_LOCAL_SHADOW_LAYERS as u32
        && properties.max_extent.width >= SHADOW_MAP_RESOLUTION
        && properties.max_extent.height >= SHADOW_MAP_RESOLUTION
}

fn best_device_index<T>(
    devices: &[T],
    is_suitable: impl Fn(&T) -> bool,
    is_discrete: impl Fn(&T) -> bool,
) -> Option<usize> {
    let mut best = None;
    for (index, device) in devices.iter().enumerate() {
        if !is_suitable(device) {
            continue;
        }
        if best.is_none_or(|best_index| is_discrete(device) && !is_discrete(&devices[best_index])) {
            best = Some(index);
        }
    }
    best
}

fn choose_device(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    required_extensions: &[CString],
) -> Result<DeviceChoice, RenderError> {
    // SAFETY: physical-device enumeration is read-only and the instance is alive.
    let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    for physical_device in physical_devices {
        let inspection = inspect_device(instance, physical_device, required_extensions)?;
        let name = inspection.report.name.clone();
        let mut reasons = inspection.report.reasons.clone();
        if !inspection.report.suitable {
            if let Some(reason) = reasons
                .iter_mut()
                .find(|reason| reason.as_str() == NO_GRAPHICS_QUEUE_FAMILY_REASON)
            {
                *reason = NO_GRAPHICS_PRESENT_QUEUE_FAMILY_REASON.to_owned();
            }
            rejected.push(format!("{name}: {}", reasons.join(", ")));
            continue;
        }

        // SAFETY: format support is queried for this physical device.
        let ldr_properties =
            unsafe { instance.get_physical_device_format_properties(physical_device, LDR_FORMAT) };
        let missing_ldr_features =
            missing_post_format_features(ldr_properties.optimal_tiling_features);
        if !missing_ldr_features.is_empty() {
            reasons.push(format!(
                "R8G8B8A8_SRGB не поддерживает возможности post target: {}",
                missing_ldr_features.join(", ")
            ));
        }
        // SAFETY: the surface and physical device belong to the same live instance.
        let surface_capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }?;
        if !surface_capabilities
            .supported_usage_flags
            .contains(SWAPCHAIN_USAGE)
        {
            reasons.push("surface не поддерживает COLOR_ATTACHMENT".to_owned());
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
            reasons.push("нет surface-формата с COLOR_ATTACHMENT".to_owned());
        }

        let mut queue_family_index = None;
        let mut timestamp_valid_bits = 0;
        for &index in &inspection.graphics_queue_family_indices {
            // SAFETY: family index is from this device's enumerated queue-family properties.
            if unsafe {
                surface_loader.get_physical_device_surface_support(physical_device, index, surface)
            }? {
                queue_family_index = Some(index);
                timestamp_valid_bits =
                    inspection.queue_families[index as usize].timestamp_valid_bits;
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
        candidates.push(DeviceChoice {
            physical_device,
            queue_family_index: queue_family_index.ok_or(RenderError::InvalidState(
                "не найдена очередь после проверки",
            ))?,
            surface_format: surface_format
                .ok_or(RenderError::InvalidState("не найден формат после проверки"))?,
            device_name: inspection.report.name,
            vendor_id: inspection.report.vendor_id,
            api_version: inspection.report.api_version,
            discrete: inspection.report.discrete,
            tessellation_shader: inspection.tessellation_shader,
            texture_sampler_anisotropy: inspection.texture_sampler_anisotropy,
            timestamp_valid_bits,
            timestamp_period: inspection.timestamp_period,
        });
    }
    best_device_index(&candidates, |_| true, |candidate| candidate.discrete)
        .map(|index| candidates.remove(index))
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
    present_mode: PresentMode,
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
        requested_present_mode: PresentMode,
    ) -> Result<Self, RenderError> {
        // SAFETY: device and surface are alive and associated with this instance.
        let capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }?;
        if !capabilities.supported_usage_flags.contains(SWAPCHAIN_USAGE) {
            return Err(RenderError::Unsupported(
                "surface больше не поддерживает COLOR_ATTACHMENT".to_owned(),
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
        // SAFETY: the physical device and surface belong to this live instance.
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
        }?;
        let selected_present_mode = select_present_mode(requested_present_mode, &present_modes);
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
            .present_mode(selected_present_mode)
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
            present_mode: PresentMode::from_vk(selected_present_mode),
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
    target: Option<FrameTarget>,
    tonemap_set: vk::DescriptorSet,
    output_set: vk::DescriptorSet,
    /// Вершины оверлея этого слота.
    overlay_buffer: GrowingBuffer,
    ui_vertices: GrowingBuffer,
    ui_indices: GrowingBuffer,
    /// Timestamp до clustering и после tonemap; `None`, если очередь их не поддерживает.
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
            tonemap_set: vk::DescriptorSet::null(),
            output_set: vk::DescriptorSet::null(),
            overlay_buffer: GrowingBuffer::default(),
            ui_vertices: GrowingBuffer::default(),
            ui_indices: GrowingBuffer::default(),
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

    /// Читает clustered compute, shadow, scene и tonemap время прошлой отправки этого слота.
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
        self.overlay_buffer.write(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            overlay_vertex_bytes(vertices),
        )
    }
}

/// Host-visible буфер кадрового слота, растущий до следующей степени двойки.
#[derive(Default)]
struct GrowingBuffer {
    buffer: Option<HostBuffer>,
    capacity: usize,
}

impl GrowingBuffer {
    /// Копирует данные в начало буфера. Вызывается после ожидания fence слота,
    /// когда GPU уже не читает прежний буфер; пустые данные буфер не меняют.
    fn write(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        bytes: &[u8],
    ) -> Result<(), RenderError> {
        drop(self.write_keep_old(instance, device, physical_device, usage, bytes)?);
        Ok(())
    }

    /// Grows a descriptor-backed slot buffer without dropping its prior buffer until the
    /// descriptor has been redirected by the caller.
    fn write_keep_old(
        &mut self,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        bytes: &[u8],
    ) -> Result<Option<HostBuffer>, RenderError> {
        if bytes.is_empty() {
            return Ok(None);
        }
        if self.buffer.is_none() || self.capacity < bytes.len() {
            let capacity = bytes.len().next_power_of_two();
            let mut data = vec![0; capacity];
            data[..bytes.len()].copy_from_slice(bytes);
            let new_buffer = HostBuffer::new(instance, device, physical_device, usage, &data)?;
            let previous = self.buffer.replace(new_buffer);
            self.capacity = capacity;
            return Ok(previous);
        }
        self.buffer
            .as_ref()
            .ok_or(RenderError::InvalidState("буфер кадра отсутствует"))?
            .write(bytes)?;
        Ok(None)
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
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
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

struct FrameTarget {
    scene: OffscreenTarget,
    ldr: DeviceImage,
    render_extent: vk::Extent2D,
}

impl FrameTarget {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        Ok(Self {
            scene: OffscreenTarget::new(instance, device, physical_device, render_extent)?,
            ldr: DeviceImage::new(
                instance,
                device,
                physical_device,
                render_extent,
                LDR_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            render_extent,
        })
    }
}

/// Заполняет набор дескрипторов кадра: uniform, shadow map и локальные источники.
#[allow(clippy::too_many_arguments)]
fn write_frame_set(
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

fn write_local_light_descriptor(
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

/// Общие объекты renderer, нужные каждой цели viewport.
struct ViewportTargetResources {
    image_set_layout: vk::DescriptorSetLayout,
    image_sampler: vk::Sampler,
    post_sampler: vk::Sampler,
    frame_set_layout: vk::DescriptorSetLayout,
    shadow_view: vk::ImageView,
    local_shadow_view: vk::ImageView,
    shadow_sampler: vk::Sampler,
}

/// Uniform-буфер сцены одной цели: у каждой сцены кадра своя камера.
struct ViewportFrameSet {
    device: ash::Device,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    buffer: HostBuffer,
    local_lights: GrowingBuffer,
    local_shadow_matrices: HostBuffer,
    _cluster_buffer: HostBuffer,
    cluster_bounds: HostBuffer,
}

impl ViewportFrameSet {
    fn new(
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
                .descriptor_count(2),
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
    fn write(
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
struct ViewportTarget {
    device: ash::Device,
    ui_pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    post_pool: vk::DescriptorPool,
    tonemap_set: vk::DescriptorSet,
    output_set: vk::DescriptorSet,
    extent: vk::Extent2D,
    render_extent: vk::Extent2D,
    scene: OffscreenTarget,
    ldr: DeviceImage,
    output: DeviceImage,
    frame_set: ViewportFrameSet,
}

impl ViewportTarget {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        render_extent: vk::Extent2D,
        resources: &ViewportTargetResources,
    ) -> Result<Self, RenderError> {
        let scene = OffscreenTarget::new(instance, device, physical_device, render_extent)?;
        let ldr = DeviceImage::new(
            instance,
            device,
            physical_device,
            render_extent,
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
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let frame_set = ViewportFrameSet::new(instance, device, physical_device, resources)?;
        let (post_pool, post_sets) = allocate_post_image_sets(
            device,
            resources.image_set_layout,
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
            ui_pool,
            set,
            post_pool,
            tonemap_set: post_sets[0],
            output_set: post_sets[1],
            extent,
            render_extent,
            scene,
            ldr,
            output,
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
struct Viewport {
    extent: vk::Extent2D,
    targets: Vec<Option<ViewportTarget>>,
}

struct UiTexture {
    device: ash::Device,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    image: DeviceImage,
    extent: vk::Extent2D,
    upload_serial: u64,
}

impl Drop for UiTexture {
    fn drop(&mut self) {
        // SAFETY: retirement waits for frame use and the image's upload serial before freeing its descriptor pool.
        unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
    }
}

/// Ресурс, ожидающий кадры и связанный upload serial; освобождается своим `Drop`.
enum RetiredResource {
    Mesh(#[allow(dead_code)] MeshBuffers),
    Texture(#[allow(dead_code)] Arc<Texture>),
    Image(#[allow(dead_code)] DeviceImage),
    Material(#[allow(dead_code)] GpuMaterial),
    UiTexture(#[allow(dead_code)] UiTexture),
    Viewport(#[allow(dead_code)] Viewport),
    Environment(#[allow(dead_code)] Box<GpuEnvironment>),
}

struct OwnedDescriptorPool {
    device: ash::Device,
    handle: vk::DescriptorPool,
}

impl OwnedDescriptorPool {
    fn new(
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

struct GpuEnvironment {
    _descriptor_pool: OwnedDescriptorPool,
    set: vk::DescriptorSet,
    _environment_map: DeviceImage,
    _irradiance_map: DeviceImage,
    _prefiltered_map: DeviceImage,
}

#[derive(Clone, Copy)]
struct EnvironmentDispatch {
    pipeline: vk::Pipeline,
    source_view: Option<vk::ImageView>,
    source_sampler: Option<vk::Sampler>,
    output_view: vk::ImageView,
    parameters: [f32; 20],
    size: u32,
}

fn unknown_viewport() -> RenderError {
    RenderError::InvalidTexture("ViewportId не принадлежит renderer".to_owned())
}

/// Создаёт пул на один набор и записывает в него изображение и sampler.
fn allocate_image_set(
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

fn allocate_post_image_sets(
    device: &ash::Device,
    layout: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    views: [vk::ImageView; 2],
) -> Result<(vk::DescriptorPool, [vk::DescriptorSet; 2]), RenderError> {
    let pool_sizes = [
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::SAMPLED_IMAGE)
            .descriptor_count(2),
        vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::SAMPLER)
            .descriptor_count(2),
    ];
    let pool_info = vk::DescriptorPoolCreateInfo::default()
        .max_sets(2)
        .pool_sizes(&pool_sizes);
    // SAFETY: the pool holds one tonemap and one output descriptor set.
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

fn write_image_set(
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

/// Получает swapchain image; `None` означает, что кадр нужно пропустить.
fn acquire_image(
    swapchain_loader: &ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    image_available: vk::Semaphore,
    image_count: usize,
    swapchain_dirty: &mut bool,
) -> Result<Option<(usize, bool)>, RenderError> {
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
        AcquireAction::Failed(error) => return Err(RenderError::Vulkan(error)),
    };
    let image_index = image_index as usize;
    if image_index >= image_count {
        *swapchain_dirty = true;
        return Err(RenderError::InvalidState(
            "Vulkan вернул неверный индекс swapchain image",
        ));
    }
    Ok(Some((image_index, suboptimal)))
}

struct DeviceImage {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    layer_views: Vec<vk::ImageView>,
    mip_array_views: Vec<vk::ImageView>,
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
    fn new_array(
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
    fn new_with_layers(
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
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_cube(
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
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_layers_kind(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        subresource_range: vk::ImageSubresourceRange,
        array_layers: u32,
        cube_compatible: bool,
    ) -> Result<Self, RenderError> {
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
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
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

#[allow(clippy::too_many_arguments)]
fn record_frame(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut FrameTarget,
    shadow_image: &DeviceImage,
    shadow_layout: &mut vk::ImageLayout,
    local_shadow_image: &DeviceImage,
    local_shadow_layout: &mut vk::ImageLayout,
    output_image: vk::Image,
    output_view: vk::ImageView,
    output_extent: vk::Extent2D,
    view: ViewSettings,
    tonemap_set: vk::DescriptorSet,
    output_set: vk::DescriptorSet,
    tonemap_pipeline: vk::Pipeline,
    output_pipeline: vk::Pipeline,
    post_pipeline_layout: vk::PipelineLayout,
    output_is_srgb: bool,
    shadow: &ShadowPass<'_>,
    scene: &ScenePass<'_>,
    overlay: Option<&OverlayPass>,
) -> Result<(), RenderError> {
    begin_command_buffer(device, command_buffer)?;
    record_scene(
        device,
        command_buffer,
        &mut target.scene,
        shadow_image,
        shadow_layout,
        local_shadow_image,
        local_shadow_layout,
        target.render_extent,
        shadow,
        scene,
    );
    record_tonemap_pass(
        device,
        command_buffer,
        &mut target.scene,
        &target.ldr,
        target.render_extent,
        tonemap_set,
        tonemap_pipeline,
        post_pipeline_layout,
        view,
        scene.timestamps,
    );
    record_output_pass(
        device,
        command_buffer,
        output_image,
        output_view,
        target.render_extent,
        output_extent,
        output_set,
        output_pipeline,
        post_pipeline_layout,
        view.anti_aliasing,
        output_is_srgb,
        overlay,
        false,
    );
    // SAFETY: command recording is complete before the command buffer is submitted.
    unsafe { device.end_command_buffer(command_buffer) }?;
    Ok(())
}

fn begin_command_buffer(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
) -> Result<(), RenderError> {
    // SAFETY: this frame's command buffer is no longer in flight because its fence was waited.
    unsafe { device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty()) }?;
    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the command buffer was reset and belongs to the renderer's graphics command pool.
    unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
    Ok(())
}

/// Записывает cluster, shadow и scene pass; цвет цели остаётся в `COLOR_ATTACHMENT_OPTIMAL`.
#[allow(clippy::too_many_arguments)]
fn record_scene(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &mut OffscreenTarget,
    shadow_image: &DeviceImage,
    shadow_layout: &mut vk::ImageLayout,
    local_shadow_image: &DeviceImage,
    local_shadow_layout: &mut vk::ImageLayout,
    render_extent: vk::Extent2D,
    shadow: &ShadowPass<'_>,
    scene: &ScenePass<'_>,
) {
    if let Some(pool) = scene.timestamps {
        // SAFETY: the slot fence was waited, so these queries are not in use; reset happens outside rendering.
        unsafe { device.cmd_reset_query_pool(command_buffer, pool, 0, TIMESTAMP_QUERY_COUNT) };
        // GPU time starts before clustering and runs through tonemapping; it does not wait for the preceding frame.
        // SAFETY: query zero was reset above and this timestamp precedes the compute dispatch.
        unsafe {
            device.cmd_write_timestamp2(
                command_buffer,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                pool,
                0,
            );
        }
    }
    // Every invocation writes one fixed cluster record; zero-light frames also reset every count.
    // SAFETY: the frame set binds live light, cluster-output, bounds, and frame-uniform buffers.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            scene.cluster_pipeline,
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            scene.cluster_pipeline_layout,
            0,
            &[scene.frame_set],
            &[],
        );
        device.cmd_dispatch(
            command_buffer,
            (CLUSTER_COUNT as u32).div_ceil(CLUSTER_COMPUTE_LOCAL_SIZE),
            1,
            1,
        );
    }
    let cluster_barrier = vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ);
    let memory_barriers = [cluster_barrier];
    let dependency = vk::DependencyInfo::default().memory_barriers(&memory_barriers);
    // SAFETY: the barrier publishes the just-written cluster SSBO to the fragment shader reads.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };

    let (previous_stage, previous_access) = offscreen_previous_use(target.layout);
    let scene_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(previous_stage)
        .src_access_mask(previous_access)
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
    if shadow.shadow_cascades.is_empty() {
        if *shadow_layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            let (source_stage, source_access) = shadow_layout_source(*shadow_layout);
            let shadow_to_sample = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(source_stage)
                .src_access_mask(source_access)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(*shadow_layout)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(shadow_image.image)
                .subresource_range(shadow_subresource_range());
            pipeline_barrier(device, command_buffer, shadow_to_sample);
            *shadow_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        }
    } else {
        let (source_stage, source_access) = shadow_layout_source(*shadow_layout);
        let shadow_to_depth = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(source_stage)
            .src_access_mask(source_access)
            .dst_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .dst_access_mask(
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            )
            .old_layout(*shadow_layout)
            .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(shadow_image.image)
            .subresource_range(shadow_subresource_range());
        pipeline_barrier(device, command_buffer, shadow_to_depth);
        *shadow_layout = vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL;

        for (cascade, matrix) in shadow.shadow_cascades.iter().enumerate() {
            let shadow_depth_attachment = vk::RenderingAttachmentInfo::default()
                .image_view(shadow_image.layer_views[cascade])
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
                        width: SHADOW_MAP_RESOLUTION,
                        height: SHADOW_MAP_RESOLUTION,
                    },
                })
                .layer_count(1)
                .depth_attachment(&shadow_depth_attachment);
            // SAFETY: this depth-only view covers one fixed-size array layer.
            unsafe { device.cmd_begin_rendering(command_buffer, &shadow_rendering_info) };
            record_shadow_draws(
                device,
                command_buffer,
                SHADOW_MAP_RESOLUTION,
                cascade as u32,
                *matrix,
                shadow.pipeline,
                shadow,
            );
            // SAFETY: this matches the preceding depth-only cmd_begin_rendering call.
            unsafe { device.cmd_end_rendering(command_buffer) };
        }

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
            .subresource_range(shadow_subresource_range());
        pipeline_barrier(device, command_buffer, shadow_to_sample);
        *shadow_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    record_local_shadow_layers(
        device,
        command_buffer,
        local_shadow_image,
        local_shadow_layout,
        shadow,
    );

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
        .store_op(vk::AttachmentStoreOp::STORE)
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
    record_draws(
        device,
        command_buffer,
        render_extent,
        scene,
        DrawPass::OpaqueAndMask,
    );
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };
    if scene.skybox {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.color.image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                color_subresource_range(),
            ),
        );
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                target.depth.image,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS,
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ,
                depth_subresource_range(),
            ),
        );
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(target.color.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let depth_attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.depth.view)
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        let rendering_info = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: render_extent,
            })
            .layer_count(1)
            .color_attachments(&color_attachment)
            .depth_attachment(&depth_attachment);
        // SAFETY: skybox loads the preceding scene attachments and does not write depth.
        unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
        record_skybox(device, command_buffer, render_extent, scene);
        // SAFETY: this matches the preceding dynamic-rendering begin call.
        unsafe { device.cmd_end_rendering(command_buffer) };
    }
    if has_transparent_draws(scene) {
        let color_to_blend = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            )
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(target.color.image)
            .subresource_range(color_subresource_range());
        pipeline_barrier(device, command_buffer, color_to_blend);
        let depth_to_blend = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .src_access_mask(vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE)
            .dst_stage_mask(
                vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                    | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            )
            .dst_access_mask(
                vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                    | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            )
            .old_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(target.depth.image)
            .subresource_range(depth_subresource_range());
        pipeline_barrier(device, command_buffer, depth_to_blend);
        let color_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(target.color.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE)];
        let depth_attachment = vk::RenderingAttachmentInfo::default()
            .image_view(target.depth.view)
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE);
        let rendering_info = vk::RenderingInfo::default()
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: render_extent,
            })
            .layer_count(1)
            .color_attachments(&color_attachment)
            .depth_attachment(&depth_attachment);
        // SAFETY: the color and depth attachments remain in their attachment layouts and were stored above.
        unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
        record_draws(
            device,
            command_buffer,
            render_extent,
            scene,
            DrawPass::Blend,
        );
        // SAFETY: this matches the preceding transparent cmd_begin_rendering call.
        unsafe { device.cmd_end_rendering(command_buffer) };
    }
    target.layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
}

fn record_local_shadow_layers(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: &DeviceImage,
    layout: &mut vk::ImageLayout,
    shadow: &ShadowPass<'_>,
) {
    let (source_stage, source_access) = shadow_layout_source(*layout);
    let to_depth = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        )
        .dst_access_mask(
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
                | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        )
        .old_layout(*layout)
        .new_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image.image)
        .subresource_range(local_shadow_subresource_range());
    pipeline_barrier(device, command_buffer, to_depth);
    *layout = vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL;

    let extent = vk::Extent2D {
        width: LOCAL_SHADOW_MAP_RESOLUTION,
        height: LOCAL_SHADOW_MAP_RESOLUTION,
    };
    for layer in 0..MAX_LOCAL_SHADOW_LAYERS {
        let depth_attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(image.layer_views[layer])
            .image_layout(vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                depth_stencil: vk::ClearDepthStencilValue {
                    depth: 1.0,
                    stencil: 0,
                },
            })];
        let render_area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let rendering_info = vk::RenderingInfo::default()
            .render_area(render_area)
            .layer_count(1)
            .depth_attachment(&depth_attachment[0]);
        // SAFETY: the single-layer view is cleared each scene pass, including unassigned layers.
        unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
        if let Some(matrix) = shadow.local_shadow_matrices.get(layer) {
            record_shadow_draws(
                device,
                command_buffer,
                LOCAL_SHADOW_MAP_RESOLUTION,
                layer as u32,
                *matrix,
                shadow.local_pipeline,
                shadow,
            );
        }
        // SAFETY: this closes the depth-only rendering instance opened above.
        unsafe { device.cmd_end_rendering(command_buffer) };
    }

    let to_sample = vk::ImageMemoryBarrier2::default()
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
        .image(image.image)
        .subresource_range(local_shadow_subresource_range());
    pipeline_barrier(device, command_buffer, to_sample);
    *layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

fn record_skybox(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
) {
    let viewport = [scene_viewport(render_extent)];
    let scissor = [vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: render_extent,
    }];
    // SAFETY: called inside dynamic rendering with a compatible color/depth skybox pipeline.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.skybox_pipeline,
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
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.pipeline_layout,
            2,
            &[scene.environment_set],
            &[],
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
    }
}

#[derive(Clone, Copy)]
enum DrawPass {
    OpaqueAndMask,
    Blend,
}

struct ScenePass<'a> {
    pipeline: vk::Pipeline,
    mask_pipeline: vk::Pipeline,
    blend_pipeline: vk::Pipeline,
    cluster_pipeline: vk::Pipeline,
    cluster_pipeline_layout: vk::PipelineLayout,
    skybox_pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    frame_set: vk::DescriptorSet,
    environment_set: vk::DescriptorSet,
    skybox: bool,
    meshes: &'a [Option<MeshBuffers>],
    materials: &'a [Option<GpuMaterial>],
    draws: &'a [DrawCommand],
    view_projection: [[f32; 4]; 4],
    camera_position: [f32; 3],
    clear_color: [f32; 4],
    timestamps: Option<vk::QueryPool>,
    view: ViewSettings,
}

/// Depth-only shadow pass. It shares the frame set and model transforms with the scene pass.
struct ShadowPass<'a> {
    pipeline: vk::Pipeline,
    local_pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    frame_set: vk::DescriptorSet,
    meshes: &'a [Option<MeshBuffers>],
    materials: &'a [Option<GpuMaterial>],
    draws: &'a [DrawCommand],
    shadow_cascades: &'a [[[f32; 4]; 4]],
    local_shadow_matrices: &'a [[[f32; 4]; 4]],
}

/// Рисование UI в swapchain image; `draws` хранит вызов, его набор и признак линейного изображения.
struct UiPass<'a> {
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    vertex_buffer: Option<vk::Buffer>,
    index_buffer: Option<vk::Buffer>,
    framebuffer_is_srgb: bool,
    draws: &'a [(UiDraw, vk::DescriptorSet, bool)],
    view: vk::ImageView,
    clear: [f32; 4],
}

/// Рисование вершин оверлея поверх swapchain image после blit.
struct OverlayPass {
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    vertex_buffer: vk::Buffer,
    vertex_count: u32,
}

fn record_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
    pass: DrawPass,
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
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            scene.pipeline_layout,
            2,
            &[scene.environment_set],
            &[],
        );
    }
    let frustum = Frustum::from_view_projection(scene.view_projection);
    let sorted = matches!(pass, DrawPass::Blend).then(|| {
        back_to_front_order(scene.draws, scene.camera_position, |draw| {
            scene.meshes.get(draw.mesh.0)?.as_ref()?.bounds
        })
    });
    let draw_count = sorted.as_ref().map_or(scene.draws.len(), Vec::len);
    let mut bound_pipeline = None;
    let mut rasterization_state = None;
    for position in 0..draw_count {
        let index = sorted
            .as_ref()
            .map_or(position, |indices| indices[position]);
        let draw = &scene.draws[index];
        let Some(material) = material_for_draw(scene.materials, draw) else {
            continue;
        };
        let pipeline_kind = match (pass, material.alpha_mode) {
            (DrawPass::OpaqueAndMask, AlphaMode::Opaque) => DrawPipeline::Opaque,
            (DrawPass::OpaqueAndMask, AlphaMode::Mask { .. }) => DrawPipeline::Mask,
            (DrawPass::Blend, AlphaMode::Blend) => DrawPipeline::Blend,
            _ => continue,
        };
        if bound_pipeline != Some(pipeline_kind) {
            let pipeline = match pipeline_kind {
                DrawPipeline::Opaque => scene.pipeline,
                DrawPipeline::Mask => scene.mask_pipeline,
                DrawPipeline::Blend => scene.blend_pipeline,
            };
            // SAFETY: the selected pipeline matches the active dynamic-rendering attachments.
            unsafe {
                device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline)
            };
            bound_pipeline = Some(pipeline_kind);
        }

        // Индексы проверены validate_draws до начала кадра.
        let Some(mesh) = scene.meshes[draw.mesh.0].as_ref() else {
            continue;
        };
        if mesh
            .bounds
            .is_some_and(|bounds| !frustum.intersects(draw.model, bounds))
        {
            continue;
        }
        let next_rasterization_state = draw_rasterization_state(material.double_sided, &draw.model);
        if rasterization_state != Some(next_rasterization_state) {
            // SAFETY: the graphics pipeline declares both values as dynamic Vulkan 1.3 states.
            unsafe {
                device.cmd_set_cull_mode(command_buffer, next_rasterization_state.0);
                device.cmd_set_front_face(command_buffer, next_rasterization_state.1);
            }
            rasterization_state = Some(next_rasterization_state);
        }
        let push_constants = push_constant_bytes(&draw.model);
        // SAFETY: mesh buffers and material descriptors stay alive until this frame's fence completes.
        unsafe {
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                scene.pipeline_layout,
                1,
                &[material.set],
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum DrawPipeline {
    Opaque,
    Mask,
    Blend,
}

fn material_for_draw<'a>(
    materials: &'a [Option<GpuMaterial>],
    draw: &DrawCommand,
) -> Option<&'a GpuMaterial> {
    materials
        .get(
            draw.material
                .map_or(DEFAULT_MATERIAL, |material| material.0),
        )
        .and_then(Option::as_ref)
}

fn has_transparent_draws(scene: &ScenePass<'_>) -> bool {
    scene.draws.iter().any(|draw| {
        material_for_draw(scene.materials, draw)
            .is_some_and(|material| matches!(material.alpha_mode, AlphaMode::Blend))
    })
}

fn shadow_casters(
    draws: &[DrawCommand],
    mut alpha_mode: impl FnMut(&DrawCommand) -> AlphaMode,
) -> impl Iterator<Item = &DrawCommand> {
    draws
        .iter()
        .filter(move |draw| draw.casts_shadow && !matches!(alpha_mode(draw), AlphaMode::Blend))
}

fn shadow_caster_is_outside(
    draw: &DrawCommand,
    bounds: Option<LocalBounds>,
    frustum: Option<Frustum>,
) -> bool {
    frustum
        .is_some_and(|frustum| bounds.is_some_and(|bounds| !frustum.intersects(draw.model, bounds)))
}

fn record_shadow_draws(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    extent: u32,
    cascade_index: u32,
    light_view_projection: [[f32; 4]; 4],
    pipeline: vk::Pipeline,
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
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
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
    let frustum = Some(Frustum::from_view_projection(light_view_projection));
    let mut rasterization_state = None;
    for draw in shadow_casters(shadow.draws, |draw| {
        material_for_draw(shadow.materials, draw)
            .map_or(AlphaMode::Opaque, |material| material.alpha_mode)
    }) {
        let Some(material) = material_for_draw(shadow.materials, draw) else {
            continue;
        };
        let Some(mesh) = &shadow.meshes[draw.mesh.0] else {
            continue;
        };
        if shadow_caster_is_outside(draw, mesh.bounds, frustum) {
            continue;
        }
        let next_rasterization_state = draw_rasterization_state(material.double_sided, &draw.model);
        if rasterization_state != Some(next_rasterization_state) {
            // SAFETY: the shadow graphics pipeline declares both values as dynamic Vulkan 1.3 states.
            unsafe {
                device.cmd_set_cull_mode(command_buffer, next_rasterization_state.0);
                device.cmd_set_front_face(command_buffer, next_rasterization_state.1);
            }
            rasterization_state = Some(next_rasterization_state);
        }
        let push_constants = push_constant_bytes(&draw.model);
        // SAFETY: draw ids were checked before recording; material descriptors and buffers stay alive.
        unsafe {
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                shadow.pipeline_layout,
                1,
                &[material.set],
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
                shadow.pipeline_layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                &push_constants,
            );
            device.cmd_draw_indexed(command_buffer, mesh.index_count, 1, 0, 0, cascade_index);
        }
    }
}

fn draw_rasterization_state(
    double_sided: bool,
    model: &[[f32; 4]; 4],
) -> (vk::CullModeFlags, vk::FrontFace) {
    let cull_mode = if double_sided {
        vk::CullModeFlags::NONE
    } else {
        vk::CullModeFlags::BACK
    };
    let x = [model[0][0], model[0][1], model[0][2]];
    let y = [model[1][0], model[1][1], model[1][2]];
    let z = [model[2][0], model[2][1], model[2][2]];
    let determinant = x[0] * (y[1] * z[2] - y[2] * z[1]) - x[1] * (y[0] * z[2] - y[2] * z[0])
        + x[2] * (y[0] * z[1] - y[1] * z[0]);
    let front_face = if determinant < 0.0 {
        vk::FrontFace::CLOCKWISE
    } else {
        vk::FrontFace::COUNTER_CLOCKWISE
    };
    (cull_mode, front_face)
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

#[allow(clippy::too_many_arguments)]
fn record_tonemap_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &mut OffscreenTarget,
    output: &DeviceImage,
    extent: vk::Extent2D,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    view: ViewSettings,
    timestamps: Option<vk::QueryPool>,
) {
    let source_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(source.layout)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(source.color.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, source_barrier);
    let output_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::NONE)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, output_barrier);

    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(output.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let push_constants = float_bytes::<POST_PUSH_CONSTANTS_SIZE>(&[
        view.exposure_ev,
        view.tonemapper.code() as f32,
        0.0,
        0.0,
    ]);
    // SAFETY: source and destination views match the descriptor and attachment layouts.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
        device.cmd_end_rendering(command_buffer);
    }
    if let Some(pool) = timestamps {
        // End GPU timing after tone mapping and before any swapchain-dependent output work.
        // SAFETY: the query was reset by record_scene and this pass follows its scene rendering.
        unsafe {
            device.cmd_write_timestamp2(
                command_buffer,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                pool,
                1,
            );
        }
    }
    let sampled = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output.image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, sampled);
    source.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

#[allow(clippy::too_many_arguments)]
fn record_output_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_image: vk::Image,
    output_view: vk::ImageView,
    source_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    anti_aliasing: AntiAliasing,
    output_is_srgb: bool,
    overlay: Option<&OverlayPass>,
    sample_output: bool,
) {
    let to_attachment = output_attachment_barrier(output_image);
    pipeline_barrier(device, command_buffer, to_attachment);

    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(output_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: output_extent.width as f32,
        height: output_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    let [texel_width, texel_height] = inverse_extent(source_extent);
    let push_constants = float_bytes::<POST_PUSH_CONSTANTS_SIZE>(&[
        texel_width,
        texel_height,
        anti_aliasing.code() as f32,
        f32::from(u8::from(output_is_srgb)),
    ]);
    // SAFETY: the output view uses the pipeline's color format and descriptor set samples LDR.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pipeline_layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            &push_constants,
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
    }
    if let Some(overlay) = overlay {
        record_overlay(device, command_buffer, output_extent, overlay);
    }
    // SAFETY: this matches the output pass's dynamic rendering begin.
    unsafe { device.cmd_end_rendering(command_buffer) };

    let sampled_output = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(if sample_output {
            vk::PipelineStageFlags2::FRAGMENT_SHADER
        } else {
            vk::PipelineStageFlags2::NONE
        })
        .dst_access_mask(if sample_output {
            vk::AccessFlags2::SHADER_SAMPLED_READ
        } else {
            vk::AccessFlags2::NONE
        })
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(if sample_output {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::PRESENT_SRC_KHR
        })
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, sampled_output);
}

fn output_attachment_barrier(output_image: vk::Image) -> vk::ImageMemoryBarrier2<'static> {
    // Keep the layout transition in the COLOR_ATTACHMENT_OUTPUT acquire-wait scope.
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range())
}

/// Рисует оверлей внутри output pass, поверх уже отфильтрованного изображения.
fn record_overlay(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_extent: vk::Extent2D,
    overlay: &OverlayPass,
) {
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
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
    // SAFETY: output pass rendering is active and the vertex buffer was written after the slot fence.
    unsafe {
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
    }
}

/// Очищает swapchain image, рисует UI и переводит изображение в present layout.
fn record_ui(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_image: vk::Image,
    output_extent: vk::Extent2D,
    ui: &UiPass<'_>,
) {
    // Стадия совпадает с ожиданием acquire semaphore в submit этого command buffer.
    let to_attachment = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        )
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(output_image)
        .subresource_range(color_subresource_range());
    pipeline_barrier(device, command_buffer, to_attachment);

    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(ui.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue { float32: ui.clear },
        })];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: output_extent,
        })
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
    // SAFETY: the view belongs to the output image and the pipeline matches the swapchain format.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    if let (Some(vertex_buffer), Some(index_buffer)) = (ui.vertex_buffer, ui.index_buffer)
        && !ui.draws.is_empty()
    {
        // SAFETY: inside dynamic rendering; the buffers were written after the slot fence.
        unsafe {
            device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, ui.pipeline);
            device.cmd_set_viewport(command_buffer, 0, &viewport);
            device.cmd_bind_vertex_buffers(command_buffer, 0, &[vertex_buffer], &[0]);
            device.cmd_bind_index_buffer(command_buffer, index_buffer, 0, vk::IndexType::UINT32);
        }
        for (draw, set, image_is_linear) in ui.draws {
            let push_constants = float_bytes::<UI_PUSH_CONSTANTS_SIZE>(&[
                viewport[0].width,
                viewport[0].height,
                f32::from(u8::from(ui.framebuffer_is_srgb)),
                f32::from(u8::from(*image_is_linear)),
            ]);
            // SAFETY: the set, scissor and index range were validated before recording; the
            // descriptor's image stays alive until the fences of this submission are waited.
            unsafe {
                device.cmd_set_scissor(command_buffer, 0, &[draw.scissor]);
                device.cmd_bind_descriptor_sets(
                    command_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    ui.pipeline_layout,
                    0,
                    &[*set],
                    &[],
                );
                device.cmd_push_constants(
                    command_buffer,
                    ui.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &push_constants,
                );
                device.cmd_draw_indexed(
                    command_buffer,
                    draw.index_count,
                    1,
                    draw.first_index,
                    draw.vertex_offset,
                    0,
                );
            }
        }
    }
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };

    let present_barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
        .dst_access_mask(vk::AccessFlags2::NONE)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
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

#[allow(clippy::too_many_arguments)]
fn image_layout_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    source_stage: vk::PipelineStageFlags2,
    source_access: vk::AccessFlags2,
    destination_stage: vk::PipelineStageFlags2,
    destination_access: vk::AccessFlags2,
    range: vk::ImageSubresourceRange,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(destination_stage)
        .dst_access_mask(destination_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range)
}

fn mip_level_count(width: u32, height: u32) -> u32 {
    32 - width.max(height).leading_zeros()
}

fn mip_extent(width: u32, height: u32, mip_level: u32) -> vk::Extent2D {
    vk::Extent2D {
        width: (width >> mip_level).max(1),
        height: (height >> mip_level).max(1),
    }
}

fn texture_mip_blit_region(
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
fn texture_image_barrier(
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

fn texture_upload_barrier(image: vk::Image, mip_levels: u32) -> vk::ImageMemoryBarrier2<'static> {
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

fn texture_mip_source_barrier(
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

fn texture_mip_shader_barrier(
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

fn texture_mip_destination_shader_barrier(
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

fn material_sampler_info(anisotropy: Option<f32>) -> vk::SamplerCreateInfo<'static> {
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

fn sampler_anisotropy(supported: bool, device_limit: f32) -> Option<f32> {
    if supported {
        Some(16.0_f32.min(device_limit))
    } else {
        None
    }
}

fn missing_texture_format_features(supported: vk::FormatFeatureFlags) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
        (vk::FormatFeatureFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (vk::FormatFeatureFlags::TRANSFER_DST, "TRANSFER_DST"),
        (vk::FormatFeatureFlags::BLIT_SRC, "BLIT_SRC"),
        (vk::FormatFeatureFlags::BLIT_DST, "BLIT_DST"),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

fn missing_post_format_features(supported: vk::FormatFeatureFlags) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

fn missing_offscreen_format_features(supported: vk::FormatFeatureFlags) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (
            vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND,
            "COLOR_ATTACHMENT_BLEND",
        ),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
        (vk::FormatFeatureFlags::STORAGE_IMAGE, "STORAGE_IMAGE"),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

fn texture_format(color_space: TextureColorSpace) -> vk::Format {
    match color_space {
        TextureColorSpace::Srgb => SRGB_TEXTURE_FORMAT,
        TextureColorSpace::Linear => LINEAR_TEXTURE_FORMAT,
    }
}

fn texture_format_reasons(
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

/// Возвращает источник доступа текущего layout цвета перед следующим проходом сцены.
fn offscreen_previous_use(layout: vk::ImageLayout) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    match layout {
        vk::ImageLayout::UNDEFINED => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_READ,
        ),
    }
}

fn color_subresource_range() -> vk::ImageSubresourceRange {
    color_mip_subresource_range(0, 1)
}

fn color_mip_subresource_range(base_mip_level: u32, level_count: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(base_mip_level)
        .level_count(level_count)
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

fn shadow_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(SHADOW_CASCADE_COUNT as u32)
}

fn local_shadow_subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::DEPTH)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(MAX_LOCAL_SHADOW_LAYERS as u32)
}

fn shadow_layout_source(layout: vk::ImageLayout) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    match layout {
        vk::ImageLayout::UNDEFINED => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        ),
    }
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

fn create_compute_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    compute_shader: &[u8],
) -> Result<vk::Pipeline, RenderError> {
    let module = ShaderModule::new(device, compute_shader)?;
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module.handle)
        .name(c"main");
    let info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(layout);
    // SAFETY: shader module and pipeline layout are valid for this compute pipeline.
    let mut pipelines = match unsafe { device.create_compute_pipelines(cache, &[info], None) } {
        Ok(pipelines) => pipelines,
        Err((pipelines, error)) => {
            for pipeline in pipelines {
                // SAFETY: partial compute pipelines were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(error.into());
        }
    };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("compute pipeline не создан"))
}

fn create_shadow_graphics_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    vertex_shader: &[u8],
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, vertex_shader)?;
    let fragment_module = ShaderModule::new(device, SHADOW_FRAGMENT_SHADER)?;
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
            .format(vk::Format::R32G32_SFLOAT)
            .offset(std::mem::offset_of!(Vertex, uv) as u32),
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
    let dynamic_states = [
        vk::DynamicState::VIEWPORT,
        vk::DynamicState::SCISSOR,
        vk::DynamicState::CULL_MODE,
        vk::DynamicState::FRONT_FACE,
    ];
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
        .layout(layout)
        .push_next(&mut rendering_info)];
    // SAFETY: shader, shared layout, depth format and dynamic-rendering state are valid.
    let mut pipelines =
        match unsafe { device.create_graphics_pipelines(cache, &pipeline_info, None) } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: these partial pipeline handles were returned by this device.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(RenderError::Vulkan(error));
            }
        };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("shadow pipeline не создан"))
}

fn create_skybox_graphics_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, SKYBOX_VERTEX_SHADER)?;
    let fragment_module = ShaderModule::new(device, SKYBOX_FRAGMENT_SHADER)?;
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
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
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
    let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
        .depth_test_enable(true)
        .depth_write_enable(false)
        .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);
    let color_attachments = [vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [OFFSCREEN_FORMAT];
    let mut rendering_info = vk::PipelineRenderingCreateInfo::default()
        .color_attachment_formats(&color_formats)
        .depth_attachment_format(DEPTH_FORMAT);
    let info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&rasterization)
        .multisample_state(&multisample)
        .depth_stencil_state(&depth_stencil)
        .color_blend_state(&color_blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .push_next(&mut rendering_info)];
    // SAFETY: shaders, layout, color/depth formats and dynamic rendering state are valid.
    let mut pipelines = match unsafe { device.create_graphics_pipelines(cache, &info, None) } {
        Ok(pipelines) => pipelines,
        Err((pipelines, error)) => {
            for pipeline in pipelines {
                // SAFETY: partial graphics pipelines were returned by this device.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            return Err(error.into());
        }
    };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("skybox pipeline не создан"))
}

fn create_post_pipeline(
    device: &ash::Device,
    cache: vk::PipelineCache,
    layout: vk::PipelineLayout,
    color_format: vk::Format,
    fragment_shader: &[u8],
) -> Result<vk::Pipeline, RenderError> {
    let vertex_module = ShaderModule::new(device, FULLSCREEN_VERTEX_SHADER)?;
    let fragment_module = ShaderModule::new(device, fragment_shader)?;
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
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
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
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [color_format];
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
    // SAFETY: the modules, layout and dynamic-rendering format belong to this device.
    let mut pipelines =
        match unsafe { device.create_graphics_pipelines(cache, &pipeline_info, None) } {
            Ok(pipelines) => pipelines,
            Err((pipelines, error)) => {
                for pipeline in pipelines {
                    // SAFETY: partial pipeline handles were returned by this device.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                return Err(error.into());
            }
        };
    pipelines
        .pop()
        .ok_or(RenderError::InvalidState("post pipeline не создан"))
}

/// Uniform-буфер освещения и его набор дескрипторов для одного слота кадра.
struct FrameUniforms {
    buffer: HostBuffer,
    set: vk::DescriptorSet,
    local_lights: GrowingBuffer,
    local_shadow_matrices: HostBuffer,
    _cluster_buffer: HostBuffer,
    cluster_bounds: HostBuffer,
}

/// Текстура в device-local памяти; материал выбирает её цветовое пространство.
struct Texture {
    image: DeviceImage,
    color_space: TextureColorSpace,
    upload_serial: u64,
}

/// Материал владеет uniform buffer и descriptor pool для собственного набора.
struct GpuMaterial {
    device: ash::Device,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    double_sided: bool,
    _uniform_buffer: HostBuffer,
    _textures: [Arc<Texture>; 5],
    alpha_mode: AlphaMode,
}

impl Drop for GpuMaterial {
    fn drop(&mut self) {
        if self.pool != vk::DescriptorPool::null() {
            // SAFETY: retirement waits for submitted frames; this frees the set before owned texture refs.
            unsafe { self.device.destroy_descriptor_pool(self.pool, None) };
        }
    }
}

struct MeshBuffers {
    vertex_buffer: DeviceBuffer,
    index_buffer: DeviceBuffer,
    index_count: u32,
    bounds: Option<LocalBounds>,
    upload_serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeshUploadLayout {
    vertex_offset: usize,
    index_offset: usize,
    end: usize,
}

fn upload_required_alignment(alignment: usize) -> Option<usize> {
    let alignment = alignment.max(1);
    let mut left = 16;
    let mut right = alignment;
    while right != 0 {
        (left, right) = (right, left % right);
    }
    16usize.checked_div(left)?.checked_mul(alignment)
}

fn align_upload_offset(cursor: usize, alignment: usize) -> Option<usize> {
    let alignment = upload_required_alignment(alignment)?;
    let remainder = cursor % alignment;
    let offset = if cursor < 16 && remainder == 0 {
        alignment
    } else if remainder == 0 {
        cursor
    } else {
        cursor.checked_add(alignment - remainder)?
    };
    u64::try_from(offset).ok()?;
    Some(offset)
}

fn upload_data_layout(cursor: usize, size: usize, alignment: usize) -> Option<(usize, usize)> {
    let offset = align_upload_offset(cursor, alignment)?;
    let end = offset.checked_add(size)?;
    u64::try_from(end).ok()?;
    Some((offset, end))
}

fn mesh_upload_layout(
    cursor: usize,
    vertex_size: usize,
    index_size: usize,
    alignment: usize,
) -> Option<MeshUploadLayout> {
    let (vertex_offset, vertex_end) = upload_data_layout(cursor, vertex_size, alignment)?;
    let (index_offset, end) = upload_data_layout(vertex_end, index_size, alignment)?;
    Some(MeshUploadLayout {
        vertex_offset,
        index_offset,
        end,
    })
}

fn upload_batch_capacity(required_size: usize, alignment: usize) -> Option<usize> {
    Some(DEFAULT_UPLOAD_BATCH_CAPACITY.max(align_upload_offset(required_size, alignment)?))
}

fn upload_batch_flush_required(
    cursor: usize,
    vertex_size: usize,
    index_size: usize,
    alignment: usize,
    capacity: usize,
) -> Option<bool> {
    Some(mesh_upload_layout(cursor, vertex_size, index_size, alignment)?.end > capacity)
}

/// Записываемая пачка копирований и её staging память.
struct UploadBatch {
    device: ash::Device,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    staging: HostBuffer,
    capacity: usize,
    cursor: usize,
    serial: u64,
    oversized: bool,
    recording: bool,
    fence: Option<vk::Fence>,
}

impl UploadBatch {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        queue_family_index: u32,
        capacity: usize,
        oversized: bool,
        serial: u64,
    ) -> Result<Self, RenderError> {
        let staging = HostBuffer::new_empty(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::TRANSFER_SRC,
            capacity,
        )?;
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: the upload pool uses this renderer's graphics queue family.
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }?;
        let result = (|| {
            let allocate_info = vk::CommandBufferAllocateInfo::default()
                .command_pool(command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            // SAFETY: command pool belongs to this device and is not used elsewhere.
            let command_buffer = unsafe { device.allocate_command_buffers(&allocate_info) }?[0];
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            // SAFETY: command buffer was just allocated and is in the initial state.
            unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
            Ok(command_buffer)
        })();
        let command_buffer = match result {
            Ok(command_buffer) => command_buffer,
            Err(error) => {
                // SAFETY: failed initialization did not submit work from this pool.
                unsafe { device.destroy_command_pool(command_pool, None) };
                return Err(error);
            }
        };
        Ok(Self {
            device: device.clone(),
            command_pool,
            command_buffer,
            staging,
            capacity,
            cursor: 0,
            serial,
            oversized,
            recording: true,
            fence: None,
        })
    }

    fn record_mesh_copies(
        &self,
        vertex_buffer: &DeviceBuffer,
        index_buffer: &DeviceBuffer,
        layout: MeshUploadLayout,
        vertex_size: usize,
        index_size: usize,
    ) {
        let vertex_regions = [vk::BufferCopy2::default()
            .src_offset(layout.vertex_offset as vk::DeviceSize)
            .dst_offset(0)
            .size(vertex_size as vk::DeviceSize)];
        let vertex_copy = vk::CopyBufferInfo2::default()
            .src_buffer(self.staging.buffer)
            .dst_buffer(vertex_buffer.buffer)
            .regions(&vertex_regions);
        // SAFETY: source and destination ranges were allocated for these exact byte counts.
        unsafe {
            self.device
                .cmd_copy_buffer2(self.command_buffer, &vertex_copy)
        };
        let index_regions = [vk::BufferCopy2::default()
            .src_offset(layout.index_offset as vk::DeviceSize)
            .dst_offset(0)
            .size(index_size as vk::DeviceSize)];
        let index_copy = vk::CopyBufferInfo2::default()
            .src_buffer(self.staging.buffer)
            .dst_buffer(index_buffer.buffer)
            .regions(&index_regions);
        // SAFETY: source and destination ranges were allocated for these exact byte counts.
        unsafe {
            self.device
                .cmd_copy_buffer2(self.command_buffer, &index_copy)
        };
        let barriers = [
            vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::VERTEX_ATTRIBUTE_INPUT)
                .dst_access_mask(vk::AccessFlags2::VERTEX_ATTRIBUTE_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(vertex_buffer.buffer)
                .offset(0)
                .size(vertex_size as vk::DeviceSize),
            vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::INDEX_INPUT)
                .dst_access_mask(vk::AccessFlags2::INDEX_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(index_buffer.buffer)
                .offset(0)
                .size(index_size as vk::DeviceSize),
        ];
        let dependency = vk::DependencyInfo::default().buffer_memory_barriers(&barriers);
        // SAFETY: barriers make each copy visible to its subsequent vertex/index input access.
        unsafe {
            self.device
                .cmd_pipeline_barrier2(self.command_buffer, &dependency)
        };
    }

    fn record_image_upload(
        &self,
        image: vk::Image,
        width: u32,
        height: u32,
        mip_levels: u32,
        buffer_offset: usize,
    ) {
        pipeline_barrier(
            &self.device,
            self.command_buffer,
            texture_upload_barrier(image, mip_levels),
        );
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(buffer_offset as vk::DeviceSize)
            .image_subresource(color_subresource_layers())
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })];
        // SAFETY: the aligned staging range contains exactly the base-level RGBA8 texels.
        unsafe {
            self.device.cmd_copy_buffer_to_image(
                self.command_buffer,
                self.staging.buffer,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            )
        };
        let mut source_extent = vk::Extent2D { width, height };
        for mip_level in 1..mip_levels {
            let source_range = color_mip_subresource_range(mip_level - 1, 1);
            pipeline_barrier(
                &self.device,
                self.command_buffer,
                texture_mip_source_barrier(image, source_range),
            );
            let destination_extent = mip_extent(width, height, mip_level);
            let region = [texture_mip_blit_region(
                mip_level - 1,
                source_extent,
                destination_extent,
            )];
            // SAFETY: adjacent mip levels are distinct, in bounds, and use the same blit-capable format.
            unsafe {
                self.device.cmd_blit_image(
                    self.command_buffer,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                    vk::Filter::LINEAR,
                )
            };
            pipeline_barrier(
                &self.device,
                self.command_buffer,
                texture_mip_shader_barrier(image, source_range),
            );
            source_extent = destination_extent;
        }
        pipeline_barrier(
            &self.device,
            self.command_buffer,
            texture_mip_destination_shader_barrier(
                image,
                color_mip_subresource_range(mip_levels - 1, 1),
            ),
        );
    }
}

impl Drop for UploadBatch {
    fn drop(&mut self) {
        if let Some(fence) = self.fence.take() {
            // SAFETY: submitted batches are dropped only after their fence signals or device idle.
            unsafe { self.device.destroy_fence(fence, None) };
        }
        // SAFETY: this batch is either not submitted or its fence has completed.
        unsafe { self.device.destroy_command_pool(self.command_pool, None) };
    }
}

/// Буфер в host-visible памяти; данные копируются один раз при создании.
struct HostBuffer {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: usize,
}

impl HostBuffer {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        data: &[u8],
    ) -> Result<Self, RenderError> {
        let buffer = Self::new_empty(instance, device, physical_device, usage, data.len())?;
        buffer.write(data)?;
        Ok(buffer)
    }

    fn new_empty(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        size: usize,
    ) -> Result<Self, RenderError> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(u64::try_from(size).map_err(|_| {
                RenderError::InvalidConfig("размер Vulkan buffer превышает DeviceSize")
            })?)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: callers request a non-zero buffer size.
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let mut host_buffer = Self {
            device: device.clone(),
            buffer,
            memory: vk::DeviceMemory::null(),
            size,
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
            RenderError::Unsupported(
                "нет HOST_VISIBLE | HOST_COHERENT memory для буфера".to_owned(),
            )
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: allocation requirements and memory type were reported for this buffer.
        host_buffer.memory = unsafe { device.allocate_memory(&allocate_info, None) }?;
        // SAFETY: memory allocation satisfies the buffer's reported requirements.
        unsafe { device.bind_buffer_memory(buffer, host_buffer.memory, 0) }?;
        Ok(host_buffer)
    }
}

impl HostBuffer {
    /// Перезаписывает начало буфера; GPU не должен читать буфер во время записи.
    fn write(&self, data: &[u8]) -> Result<(), RenderError> {
        self.write_at(0, data)
    }

    fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), RenderError> {
        if offset
            .checked_add(data.len())
            .is_none_or(|end| end > self.size)
        {
            return Err(RenderError::InvalidState(
                "запись выходит за размер host buffer",
            ));
        }
        if data.is_empty() {
            return Ok(());
        }
        // SAFETY: callers write within the coherent buffer while it is not in use by the GPU.
        unsafe {
            let mapped = self.device.map_memory(
                self.memory,
                0,
                self.size as vk::DeviceSize,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                mapped.cast::<u8>().add(offset),
                data.len(),
            );
            self.device.unmap_memory(self.memory);
        }
        Ok(())
    }
}

/// Буфер в DEVICE_LOCAL памяти для vertex и index input.
struct DeviceBuffer {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

impl DeviceBuffer {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        usage: vk::BufferUsageFlags,
        size: usize,
    ) -> Result<Self, RenderError> {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(u64::try_from(size).map_err(|_| {
                RenderError::InvalidConfig("размер Vulkan buffer превышает DeviceSize")
            })?)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: callers provide non-empty mesh data and a valid usage.
        let buffer = unsafe { device.create_buffer(&buffer_info, None) }?;
        let mut device_buffer = Self {
            device: device.clone(),
            buffer,
            memory: vk::DeviceMemory::null(),
        };
        // SAFETY: buffer belongs to this device and is alive.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory_type = find_memory_type(
            instance,
            physical_device,
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .ok_or_else(|| {
            RenderError::Unsupported("нет DEVICE_LOCAL memory для mesh buffer".to_owned())
        })?;
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: allocation size and memory type were returned for this buffer.
        device_buffer.memory = unsafe { device.allocate_memory(&allocate_info, None) }?;
        // SAFETY: allocation satisfies this buffer's memory requirements.
        unsafe { device.bind_buffer_memory(buffer, device_buffer.memory, 0) }?;
        Ok(device_buffer)
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        // SAFETY: owning renderer waits for frame and upload serials before dropping mesh buffers.
        unsafe { self.device.destroy_buffer(self.buffer, None) };
        if self.memory != vk::DeviceMemory::null() {
            // SAFETY: the buffer bound to this allocation was destroyed first.
            unsafe { self.device.free_memory(self.memory, None) };
        }
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

fn validate_draws<M, T>(
    draws: &[DrawCommand],
    meshes: &[Option<M>],
    materials: &[Option<T>],
) -> Result<(), RenderError> {
    if draws
        .iter()
        .any(|draw| !meshes.get(draw.mesh.0).is_some_and(Option::is_some))
    {
        return Err(RenderError::InvalidMesh("MeshId не принадлежит renderer"));
    }
    for draw in draws {
        let material = draw
            .material
            .map_or(DEFAULT_MATERIAL, |material| material.0);
        if !materials.get(material).is_some_and(Option::is_some) {
            return Err(RenderError::InvalidMaterial(
                "MaterialId не принадлежит renderer".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_material(
    material: &Material,
    texture_spaces: &[Option<TextureColorSpace>],
) -> Result<(), RenderError> {
    let valid_factor = |value: f32| value.is_finite() && (0.0..=1.0).contains(&value);
    if !material.base_color.into_iter().all(valid_factor)
        || !valid_factor(material.metallic)
        || !valid_factor(material.roughness)
        || !material.normal_scale.is_finite()
        || !valid_factor(material.opacity)
        || !valid_factor(material.occlusion_strength)
    {
        return Err(RenderError::InvalidMaterial(
            "факторы должны быть конечными; base_color/metallic/roughness/opacity/occlusion_strength — в [0, 1]".to_owned(),
        ));
    }
    if !material
        .emissive
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err(RenderError::InvalidMaterial(
            "emissive должен содержать конечные компоненты >= 0".to_owned(),
        ));
    }
    if let AlphaMode::Mask { cutoff } = material.alpha_mode
        && !valid_factor(cutoff)
    {
        return Err(RenderError::InvalidMaterial(
            "alpha cutoff должен быть конечным и находиться в диапазоне [0, 1]".to_owned(),
        ));
    }
    let check_texture = |texture: Option<TextureId>, expected: TextureColorSpace, name: &str| {
        let Some(texture) = texture else {
            return Ok(());
        };
        let Some(Some(actual)) = texture_spaces.get(texture.0) else {
            return Err(RenderError::InvalidMaterial(format!(
                "TextureId для {name} не принадлежит renderer"
            )));
        };
        if *actual != expected {
            return Err(RenderError::InvalidMaterial(format!(
                "{name} должен иметь цветовое пространство {expected:?}"
            )));
        }
        Ok(())
    };
    check_texture(
        material.base_color_texture,
        TextureColorSpace::Srgb,
        "base_color_texture",
    )?;
    check_texture(
        material.metallic_roughness_texture,
        TextureColorSpace::Linear,
        "metallic_roughness_texture",
    )?;
    check_texture(
        material.normal_texture,
        TextureColorSpace::Linear,
        "normal_texture",
    )?;
    check_texture(
        material.emissive_texture,
        TextureColorSpace::Srgb,
        "emissive_texture",
    )?;
    check_texture(
        material.occlusion_texture,
        TextureColorSpace::Linear,
        "occlusion_texture",
    )
}

fn material_uniform_bytes(material: &Material) -> [u8; MATERIAL_UNIFORMS_SIZE] {
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
    let expected = texture_staging_size(width, height);
    if expected != Some(byte_count) {
        return Err(RenderError::InvalidTexture(format!(
            "для RGBA8 {width}x{height} нужно {} байт, передано {byte_count}",
            expected.map_or_else(|| "больше usize::MAX".to_owned(), |bytes| bytes.to_string())
        )));
    }
    Ok(())
}

fn texture_staging_size(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|texels| texels.checked_mul(4))
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

/// Байты `FrameUniforms` в порядке std140: восемь матриц и восемь `vec4`.
/// Viewport рисуется в кадре один раз: его цель и uniform-буфер принадлежат одной сцене.
fn validate_scene_viewports(
    viewports: impl Iterator<Item = ViewportId>,
) -> Result<(), RenderError> {
    let mut seen = Vec::new();
    for viewport in viewports {
        if seen.contains(&viewport) {
            return Err(RenderError::InvalidTexture(
                "viewport указан в кадре несколько раз".to_owned(),
            ));
        }
        seen.push(viewport);
    }
    Ok(())
}

struct PreparedFrame {
    uniforms: [u8; FRAME_UNIFORMS_SIZE],
    light_bytes: Vec<u8>,
    local_shadow_matrices: Vec<[[f32; 4]; 4]>,
    cluster_bounds: Vec<ClusterBounds>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SelectedLocalLight {
    source_index: usize,
    light: LocalLight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalShadowKind {
    Point,
    Spot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalShadowSlot {
    layer_base: u32,
    kind: LocalShadowKind,
}

struct LocalShadowPlan {
    assignments: Vec<Option<LocalShadowSlot>>,
    matrices: Vec<[[f32; 4]; 4]>,
}

fn prepare_frame(
    frame: &Frame<'_>,
    render_extent: vk::Extent2D,
) -> Result<PreparedFrame, RenderError> {
    let shadow_requests = index_local_light_shadows(frame.lights, frame.local_light_shadows)?;
    let lights = select_local_light_entries(frame.camera, frame.lights)?;
    let (depth_range, cluster_bounds) = match camera_cluster_bounds(frame.camera.projection) {
        Ok(bounds) => bounds,
        // Unlit frames also support clip-space cameras, such as sandbox's triangle.
        Err(_) if lights.is_empty() => (
            clusters::DepthRange {
                near: 1.0,
                far: 2.0,
            },
            vec![ClusterBounds::default(); CLUSTER_COUNT],
        ),
        Err(error) => return Err(error),
    };
    let shadow_plan = select_local_shadow_slots(&lights, &shadow_requests, frame.camera.position);
    let uniforms = frame_uniform_bytes_with_count(frame, lights.len(), depth_range, render_extent)?;
    let light_bytes =
        local_light_bytes_for_selected(&lights, &shadow_plan.assignments, frame.camera.view)?;
    Ok(PreparedFrame {
        uniforms,
        light_bytes,
        local_shadow_matrices: shadow_plan.matrices,
        cluster_bounds,
    })
}

#[cfg(test)]
fn select_local_lights(
    camera: FrameCamera,
    lights: &[LocalLight],
) -> Result<Vec<LocalLight>, RenderError> {
    Ok(select_local_light_entries(camera, lights)?
        .into_iter()
        .map(|selected| selected.light)
        .collect())
}

fn select_local_light_entries(
    camera: FrameCamera,
    lights: &[LocalLight],
) -> Result<Vec<SelectedLocalLight>, RenderError> {
    if !camera.position.into_iter().all(f32::is_finite)
        || !camera.view.iter().flatten().all(|value| value.is_finite())
        || !camera
            .projection
            .iter()
            .flatten()
            .all(|value| value.is_finite())
    {
        return Err(RenderError::InvalidConfig(
            "camera matrices and position must be finite",
        ));
    }
    let view_projection = multiply_4x4(camera.projection, camera.view);
    let frustum = Frustum::from_view_projection(view_projection);
    let mut visible = Vec::with_capacity(lights.len().min(MAX_RENDERED_LIGHTS));
    for (index, light) in lights.iter().copied().enumerate() {
        let light = validate_local_light(light)?;
        if local_light_visible(frustum, light) {
            visible.push((
                index,
                local_light_contribution(camera.position, light),
                light,
            ));
        }
    }
    visible.sort_by(
        |(left_index, left_score, _), (right_index, right_score, _)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    Ok(visible
        .into_iter()
        .take(MAX_RENDERED_LIGHTS)
        .map(|(source_index, _, light)| SelectedLocalLight {
            source_index,
            light,
        })
        .collect())
}

fn index_local_light_shadows<'a>(
    lights: &[LocalLight],
    shadows: &'a [LocalLightShadow],
) -> Result<Vec<Option<&'a LocalLightShadow>>, RenderError> {
    let mut indexed = vec![None; lights.len()];
    for shadow in shadows {
        let Some(light) = lights.get(shadow.light_index) else {
            return Err(RenderError::InvalidConfig(
                "local light shadow index must refer to an input light",
            ));
        };
        if indexed[shadow.light_index].is_some() {
            return Err(RenderError::InvalidConfig(
                "local light shadow indices must be unique",
            ));
        }
        let (matches_light, matrices_finite) = match (light, shadow.matrices) {
            (LocalLight::Point { .. }, LocalLightShadowMatrices::Point { matrices }) => (
                true,
                matrices
                    .iter()
                    .all(|matrix| matrix.iter().flatten().all(|value| value.is_finite())),
            ),
            (LocalLight::Spot { .. }, LocalLightShadowMatrices::Spot { matrix }) => {
                (true, matrix.iter().flatten().all(|value| value.is_finite()))
            }
            _ => (false, false),
        };
        if !matches_light || !matrices_finite {
            return Err(RenderError::InvalidConfig(
                "local light shadow matrices must be finite and match the light type",
            ));
        }
        indexed[shadow.light_index] = Some(shadow);
    }
    Ok(indexed)
}

fn select_local_shadow_slots(
    lights: &[SelectedLocalLight],
    requests: &[Option<&LocalLightShadow>],
    camera_position: [f32; 3],
) -> LocalShadowPlan {
    let mut candidates = lights
        .iter()
        .enumerate()
        .filter_map(|(selected_index, selected)| {
            let shadow = requests.get(selected.source_index).copied().flatten()?;
            let (kind, matrix_count) = match shadow.matrices {
                LocalLightShadowMatrices::Point { .. } => (LocalShadowKind::Point, 6),
                LocalLightShadowMatrices::Spot { .. } => (LocalShadowKind::Spot, 1),
            };
            Some((
                selected_index,
                selected.source_index,
                local_light_camera_distance_squared(selected.light, camera_position),
                kind,
                matrix_count,
                shadow.matrices,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.2
            .total_cmp(&right.2)
            .then_with(|| left.1.cmp(&right.1))
    });

    let mut assignments = vec![None; lights.len()];
    let mut matrices = Vec::with_capacity(MAX_LOCAL_SHADOW_LAYERS);
    for (selected_index, _, _, kind, matrix_count, source) in candidates {
        if matrices.len() + matrix_count > MAX_LOCAL_SHADOW_LAYERS {
            continue;
        }
        let layer_base = matrices.len() as u32;
        match source {
            LocalLightShadowMatrices::Point { matrices: faces } => matrices.extend(faces),
            LocalLightShadowMatrices::Spot { matrix } => matrices.push(matrix),
        }
        assignments[selected_index] = Some(LocalShadowSlot { layer_base, kind });
    }
    LocalShadowPlan {
        assignments,
        matrices,
    }
}

fn local_light_camera_distance_squared(light: LocalLight, camera: [f32; 3]) -> f64 {
    let position = match light {
        LocalLight::Point { position, .. } | LocalLight::Spot { position, .. } => position,
    };
    (0..3)
        .map(|axis| (f64::from(position[axis]) - f64::from(camera[axis])).powi(2))
        .sum()
}

#[cfg(test)]
fn point_shadow_face_index(direction: [f32; 3]) -> u32 {
    let magnitude = direction.map(f32::abs);
    if magnitude[0] >= magnitude[1] && magnitude[0] >= magnitude[2] {
        return u32::from(direction[0] < 0.0);
    }
    if magnitude[1] >= magnitude[2] {
        return if direction[1] >= 0.0 { 2 } else { 3 };
    }
    if direction[2] >= 0.0 { 4 } else { 5 }
}

fn validate_local_light(light: LocalLight) -> Result<LocalLight, RenderError> {
    let validate_common = |position: [f32; 3], color: [f32; 3], range: f32| {
        if !position.into_iter().all(f32::is_finite) {
            return Err(RenderError::InvalidConfig(
                "local light position must contain finite values",
            ));
        }
        if !color
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0)
        {
            return Err(RenderError::InvalidConfig(
                "local light color_intensity must contain finite non-negative values",
            ));
        }
        if !range.is_finite() || range <= 0.0 {
            return Err(RenderError::InvalidConfig(
                "local light range must be finite and positive",
            ));
        }
        Ok(())
    };
    match light {
        LocalLight::Point {
            position,
            color_intensity,
            range,
        } => {
            validate_common(position, color_intensity, range)?;
            Ok(light)
        }
        LocalLight::Spot {
            position,
            direction,
            color_intensity,
            range,
            cos_inner_angle,
            cos_outer_angle,
        } => {
            validate_common(position, color_intensity, range)?;
            if !direction.into_iter().all(f32::is_finite) {
                return Err(RenderError::InvalidConfig(
                    "spot light direction must contain finite values",
                ));
            }
            let length = direction
                .into_iter()
                .map(|value| f64::from(value) * f64::from(value))
                .sum::<f64>()
                .sqrt();
            if !length.is_finite() || length <= f64::EPSILON {
                return Err(RenderError::InvalidConfig(
                    "spot light direction must be non-zero",
                ));
            }
            let normalize_angle_cosine = |value: f32| {
                if (-f32::EPSILON..0.0).contains(&value) {
                    0.0
                } else {
                    value
                }
            };
            let cos_inner_angle = normalize_angle_cosine(cos_inner_angle);
            let cos_outer_angle = normalize_angle_cosine(cos_outer_angle);
            if !cos_inner_angle.is_finite()
                || !cos_outer_angle.is_finite()
                || cos_inner_angle < 0.0
                || cos_inner_angle > 1.0
                || cos_outer_angle < 0.0
                || cos_outer_angle > cos_inner_angle
            {
                return Err(RenderError::InvalidConfig(
                    "spot light cos angles must satisfy 0 <= cos_outer <= cos_inner <= 1",
                ));
            }
            Ok(LocalLight::Spot {
                position,
                direction: direction.map(|value| (f64::from(value) / length) as f32),
                color_intensity,
                range,
                cos_inner_angle,
                cos_outer_angle,
            })
        }
    }
}

fn local_light_visible(frustum: Frustum, light: LocalLight) -> bool {
    match light {
        LocalLight::Point {
            position, range, ..
        } => frustum.intersects_sphere(position, range),
        LocalLight::Spot {
            position,
            direction,
            range,
            cos_outer_angle,
            ..
        } => frustum.intersects_spot_sector(position, direction, range, cos_outer_angle),
    }
}

fn local_light_contribution(camera: [f32; 3], light: LocalLight) -> f64 {
    let (position, color, range) = match light {
        LocalLight::Point {
            position,
            color_intensity,
            range,
        }
        | LocalLight::Spot {
            position,
            color_intensity,
            range,
            ..
        } => (position, color_intensity, range),
    };
    let distance_squared = (0..3)
        .map(|axis| {
            let delta = f64::from(camera[axis]) - f64::from(position[axis]);
            delta * delta
        })
        .sum::<f64>();
    let range = f64::from(range);
    let normalized_distance_squared = distance_squared / (range * range);
    let range_window =
        (1.0 - normalized_distance_squared * normalized_distance_squared).clamp(0.0, 1.0);
    let spot_weight = match light {
        LocalLight::Point { .. } => 1.0,
        LocalLight::Spot {
            position,
            direction,
            cos_inner_angle,
            cos_outer_angle,
            ..
        } => {
            let delta = std::array::from_fn::<_, 3, _>(|axis| {
                f64::from(camera[axis]) - f64::from(position[axis])
            });
            if distance_squared <= 1e-12 {
                1.0
            } else {
                let inverse_distance = distance_squared.sqrt().recip();
                let ray_cosine = (0..3)
                    .map(|axis| f64::from(direction[axis]) * delta[axis])
                    .sum::<f64>()
                    * inverse_distance;
                let outer = f64::from(cos_outer_angle.max(0.0));
                let inner = f64::from(cos_inner_angle);
                if inner - outer <= 1e-7 {
                    f64::from(ray_cosine >= outer)
                } else {
                    let amount = ((ray_cosine - outer) / (inner - outer)).clamp(0.0, 1.0);
                    amount * amount * (3.0 - 2.0 * amount)
                }
            }
        }
    };
    let luminance =
        f64::from(color[0]) * 0.2126 + f64::from(color[1]) * 0.7152 + f64::from(color[2]) * 0.0722;
    luminance * range_window * range_window * spot_weight / distance_squared.max(1e-4)
}

fn local_light_bytes(lights: &[LocalLight], view: [[f32; 4]; 4]) -> Result<Vec<u8>, RenderError> {
    let selected = lights
        .iter()
        .copied()
        .enumerate()
        .map(|(source_index, light)| SelectedLocalLight {
            source_index,
            light,
        })
        .collect::<Vec<_>>();
    local_light_bytes_for_selected(&selected, &[], view)
}

fn local_light_bytes_for_selected(
    lights: &[SelectedLocalLight],
    assignments: &[Option<LocalShadowSlot>],
    view: [[f32; 4]; 4],
) -> Result<Vec<u8>, RenderError> {
    if !lights.is_empty() {
        // Cluster spheres and cones retain world-space radii and angles, so
        // the camera transform must preserve distances and angles.
        let rigid = (0..3).all(|a| {
            (0..3).all(|b| {
                let dot = (0..3).map(|row| view[a][row] * view[b][row]).sum::<f32>();
                (dot - if a == b { 1.0 } else { 0.0 }).abs() <= 1e-4
            })
        }) && (0..3).all(|column| view[column][3] == 0.0)
            && view[3][3] == 1.0;
        if !rigid {
            return Err(RenderError::InvalidConfig(
                "camera view must be a rigid affine transform when clustering local lights",
            ));
        }
    }
    let mut packed = Vec::with_capacity(lights.len().max(1));
    for (selected_index, selected) in lights.iter().enumerate() {
        let shadow_params =
            assignments
                .get(selected_index)
                .copied()
                .flatten()
                .map_or([0; 4], |slot| {
                    [
                        slot.layer_base,
                        match slot.kind {
                            LocalShadowKind::Point => 1,
                            LocalShadowKind::Spot => 2,
                        },
                        1,
                        0,
                    ]
                });
        packed.push(match selected.light {
            LocalLight::Point {
                position,
                color_intensity,
                range,
            } => {
                let view_position = view_position(view, position)?;
                GpuLocalLight {
                    position_range: [position[0], position[1], position[2], range],
                    direction_type: [0.0; 4],
                    color_intensity: [
                        color_intensity[0],
                        color_intensity[1],
                        color_intensity[2],
                        0.0,
                    ],
                    cone_params: [0.0; 4],
                    view_position_range: [
                        view_position[0],
                        view_position[1],
                        view_position[2],
                        range,
                    ],
                    view_direction: [0.0; 4],
                    shadow_params,
                }
            }
            LocalLight::Spot {
                position,
                direction,
                color_intensity,
                range,
                cos_inner_angle,
                cos_outer_angle,
            } => {
                let view_position = view_position(view, position)?;
                let view_direction = view_direction(view, direction)?;
                GpuLocalLight {
                    position_range: [position[0], position[1], position[2], range],
                    direction_type: [direction[0], direction[1], direction[2], 1.0],
                    color_intensity: [
                        color_intensity[0],
                        color_intensity[1],
                        color_intensity[2],
                        0.0,
                    ],
                    cone_params: [cos_inner_angle, cos_outer_angle, 0.0, 0.0],
                    view_position_range: [
                        view_position[0],
                        view_position[1],
                        view_position[2],
                        range,
                    ],
                    view_direction: [view_direction[0], view_direction[1], view_direction[2], 0.0],
                    shadow_params,
                }
            }
        });
    }
    if packed.is_empty() {
        packed.push(GpuLocalLight::default());
    }
    // SAFETY: GpuLocalLight is repr(C), aligned, and contains initialized vec4/uvec4 fields.
    Ok(unsafe {
        std::slice::from_raw_parts(packed.as_ptr().cast(), size_of_val(packed.as_slice())).to_vec()
    })
}

fn local_shadow_matrix_bytes(
    matrices: &[[[f32; 4]; 4]],
) -> Result<[u8; LOCAL_SHADOW_MATRIX_BUFFER_SIZE], RenderError> {
    if matrices.len() > MAX_LOCAL_SHADOW_LAYERS
        || matrices
            .iter()
            .any(|matrix| !matrix.iter().flatten().all(|value| value.is_finite()))
    {
        return Err(RenderError::InvalidConfig(
            "local shadow matrices must be finite and fit the shared layer array",
        ));
    }
    let mut bytes = [0; LOCAL_SHADOW_MATRIX_BUFFER_SIZE];
    for layer in 0..MAX_LOCAL_SHADOW_LAYERS {
        let matrix = matrices.get(layer).unwrap_or(&IDENTITY_MATRIX);
        let matrix_bytes = unsafe {
            std::slice::from_raw_parts(matrix.as_ptr().cast::<u8>(), size_of::<[[f32; 4]; 4]>())
        };
        let start = layer * size_of::<[[f32; 4]; 4]>();
        bytes[start..start + matrix_bytes.len()].copy_from_slice(matrix_bytes);
    }
    Ok(bytes)
}

fn cluster_bounds_bytes(bounds: &[ClusterBounds]) -> &[u8] {
    debug_assert_eq!(bounds.len(), CLUSTER_COUNT);
    debug_assert_eq!(size_of::<ClusterBounds>(), CLUSTER_BOUNDS_GPU_SIZE);
    // SAFETY: ClusterBounds is repr(C), aligned to 16 bytes, and contains only initialized vec4s.
    unsafe { std::slice::from_raw_parts(bounds.as_ptr().cast(), size_of_val(bounds)) }
}

fn transform_4x4(matrix: [[f32; 4]; 4], vector: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|row| {
        (0..4)
            .map(|column| f64::from(matrix[column][row]) * f64::from(vector[column]))
            .sum::<f64>() as f32
    })
}

fn view_position(view: [[f32; 4]; 4], position: [f32; 3]) -> Result<[f32; 3], RenderError> {
    let transformed = transform_4x4(view, [position[0], position[1], position[2], 1.0]);
    if !transformed.into_iter().all(f32::is_finite) || transformed[3].abs() <= f32::EPSILON {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid local light position",
        ));
    }
    let result = std::array::from_fn(|axis| transformed[axis] / transformed[3]);
    if !result.into_iter().all(f32::is_finite) {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid local light position",
        ));
    }
    Ok(result)
}

fn view_direction(view: [[f32; 4]; 4], direction: [f32; 3]) -> Result<[f32; 3], RenderError> {
    let transformed = transform_4x4(view, [direction[0], direction[1], direction[2], 0.0]);
    let length = transformed[..3]
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if !length.is_finite() || length <= f64::EPSILON {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid spot direction",
        ));
    }
    let result = std::array::from_fn(|axis| (f64::from(transformed[axis]) / length) as f32);
    if !result.into_iter().all(f32::is_finite) {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid spot direction",
        ));
    }
    Ok(result)
}

#[cfg(test)]
fn frame_uniform_bytes(frame: &Frame<'_>) -> Result<[u8; FRAME_UNIFORMS_SIZE], RenderError> {
    prepare_frame(
        frame,
        vk::Extent2D {
            width: 1,
            height: 1,
        },
    )
    .map(|prepared| prepared.uniforms)
}

fn frame_uniform_bytes_with_count(
    frame: &Frame<'_>,
    local_light_count: usize,
    depth_range: DepthRange,
    render_extent: vk::Extent2D,
) -> Result<[u8; FRAME_UNIFORMS_SIZE], RenderError> {
    validate_view_settings(frame.view)?;
    if !frame
        .camera
        .view
        .iter()
        .flatten()
        .all(|value| value.is_finite())
        || !frame
            .camera
            .projection
            .iter()
            .flatten()
            .all(|value| value.is_finite())
        || !frame.camera.position.into_iter().all(f32::is_finite)
    {
        return Err(RenderError::InvalidConfig(
            "camera matrices and position must be finite",
        ));
    }
    if let Some(environment) = frame.environment {
        validate_frame_environment(environment)?;
    }
    let light = frame.light;
    let [x, y, z] = light.direction;
    let length_squared = x * x + y * y + z * z;
    if !length_squared.is_finite() || length_squared == 0.0 {
        return Err(RenderError::InvalidConfig(
            "направление света должно быть конечным и ненулевым",
        ));
    }
    if frame.shadow_cascades.len() > SHADOW_CASCADE_COUNT {
        return Err(RenderError::InvalidConfig(
            "число shadow cascades должно быть не больше четырёх",
        ));
    }
    if frame
        .shadow_cascades
        .iter()
        .any(|matrix| !matrix.iter().flatten().all(|value| value.is_finite()))
    {
        return Err(RenderError::InvalidConfig(
            "матрицы shadow cascades должны быть конечными",
        ));
    }

    let view_projection = multiply_4x4(frame.camera.projection, frame.camera.view);
    let inverse_view_projection = environment::inverse4x4(view_projection).ok_or(
        RenderError::InvalidConfig("camera view-projection matrix must be invertible"),
    )?;
    let mut values = [0.0; FRAME_UNIFORMS_SIZE / size_of::<f32>()];
    values[..16].copy_from_slice(view_projection.as_flattened());
    values[16..32].copy_from_slice(inverse_view_projection.as_flattened());
    for cascade in 0..SHADOW_CASCADE_COUNT {
        let start = 32 + cascade * 16;
        let matrix = frame
            .shadow_cascades
            .get(cascade)
            .unwrap_or(&IDENTITY_MATRIX);
        values[start..start + 16].copy_from_slice(matrix.as_flattened());
    }
    values[96..100].copy_from_slice(&[x, y, z, 0.0]);
    values[100..104].copy_from_slice(&[light.color[0], light.color[1], light.color[2], 0.0]);
    values[104..108].copy_from_slice(&[light.ambient[0], light.ambient[1], light.ambient[2], 0.0]);
    values[108..112].copy_from_slice(&[
        if frame.shadow_cascades.is_empty() {
            0.0
        } else {
            1.0
        },
        1.0 / SHADOW_MAP_RESOLUTION as f32,
        frame.shadow_cascades.len() as f32,
        0.0,
    ]);
    values[112..116].copy_from_slice(&[
        frame.camera.position[0],
        frame.camera.position[1],
        frame.camera.position[2],
        0.0,
    ]);
    let (rotation, intensity, skybox, present) =
        frame
            .environment
            .map_or((IDENTITY_MATRIX, 0.0, 0.0, 0.0), |environment| {
                let [[a, b, c], [d, e, f], [g, h, i]] = environment.rotation;
                (
                    [
                        [a, b, c, 0.0],
                        [d, e, f, 0.0],
                        [g, h, i, 0.0],
                        [0.0, 0.0, 0.0, 1.0],
                    ],
                    environment.intensity,
                    if environment.skybox { 1.0 } else { 0.0 },
                    1.0,
                )
            });
    values[116..132].copy_from_slice(rotation.as_flattened());
    values[132..136].copy_from_slice(&[intensity, skybox, present, 0.0]);
    values[136..140].copy_from_slice(&[local_light_count as f32, 0.0, 0.0, 0.0]);
    values[140..156].copy_from_slice(frame.camera.view.as_flattened());
    values[156..160].copy_from_slice(&[
        depth_range.near,
        depth_range.far,
        render_extent.width.max(1) as f32,
        render_extent.height.max(1) as f32,
    ]);
    Ok(float_bytes(&values))
}

fn validate_frame_environment(environment: FrameEnvironment) -> Result<(), RenderError> {
    if !environment.intensity.is_finite() || environment.intensity < 0.0 {
        return Err(RenderError::InvalidConfig(
            "environment intensity must be finite and non-negative",
        ));
    }
    if !environment
        .rotation
        .iter()
        .flatten()
        .all(|value| value.is_finite())
    {
        return Err(RenderError::InvalidConfig(
            "environment rotation must be finite",
        ));
    }
    let columns = environment.rotation;
    let dot = |left: [f32; 3], right: [f32; 3]| {
        left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
    };
    if columns
        .iter()
        .any(|column| (dot(*column, *column) - 1.0).abs() > 1e-3)
        || (0..3)
            .any(|left| (left + 1..3).any(|right| dot(columns[left], columns[right]).abs() > 1e-3))
    {
        return Err(RenderError::InvalidConfig(
            "environment rotation must be an orthonormal basis",
        ));
    }
    let cross = [
        columns[0][1] * columns[1][2] - columns[0][2] * columns[1][1],
        columns[0][2] * columns[1][0] - columns[0][0] * columns[1][2],
        columns[0][0] * columns[1][1] - columns[0][1] * columns[1][0],
    ];
    if dot(cross, columns[2]) < 0.999 {
        return Err(RenderError::InvalidConfig(
            "environment rotation must preserve orientation",
        ));
    }
    Ok(())
}

fn multiply_4x4(left: [[f32; 4]; 4], right: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut result = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            result[column][row] = (0..4)
                .map(|index| left[index][row] * right[column][index])
                .sum();
        }
    }
    result
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

#[cfg(test)]
fn derivative_tangent_basis(
    dp1: [f32; 3],
    dp2: [f32; 3],
    duv1: [f32; 2],
    duv2: [f32; 2],
    normal: [f32; 3],
) -> Option<([f32; 3], [f32; 3])> {
    let determinant = duv1[0] * duv2[1] - duv1[1] * duv2[0];
    if !determinant.is_finite()
        || determinant.abs() <= 1e-8
        || !dp1.into_iter().chain(dp2).all(f32::is_finite)
    {
        return None;
    }
    let normal = try_normalize3(normal).unwrap_or([0.0, 0.0, 1.0]);
    let mut tangent = scale3(
        sub3(scale3(dp1, duv2[1]), scale3(dp2, duv1[1])),
        determinant.recip(),
    );
    let mut bitangent = scale3(
        sub3(scale3(dp2, duv1[0]), scale3(dp1, duv2[0])),
        -determinant.recip(),
    );
    tangent = sub3(tangent, scale3(normal, dot3(tangent, normal)));
    bitangent = sub3(bitangent, scale3(normal, dot3(bitangent, normal)));
    Some((try_normalize3(tangent)?, try_normalize3(bitangent)?))
}

#[cfg(test)]
fn transform_tangent_normal(
    tangent: [f32; 3],
    bitangent: [f32; 3],
    normal: [f32; 3],
    tangent_normal: [f32; 3],
) -> [f32; 3] {
    let mapped = add3(
        add3(
            scale3(tangent, tangent_normal[0]),
            scale3(bitangent, tangent_normal[1]),
        ),
        scale3(normal, tangent_normal[2]),
    );
    try_normalize3(mapped).unwrap_or(normal)
}

#[cfg(test)]
fn try_normalize3(value: [f32; 3]) -> Option<[f32; 3]> {
    let length_squared = dot3(value, value);
    if !length_squared.is_finite() || length_squared <= 1e-20 {
        None
    } else {
        Some(scale3(value, length_squared.sqrt().recip()))
    }
}

#[cfg(test)]
fn dot3(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

#[cfg(test)]
fn scale3(value: [f32; 3], scale: f32) -> [f32; 3] {
    [value[0] * scale, value[1] * scale, value[2] * scale]
}

#[cfg(test)]
fn sub3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

#[cfg(test)]
fn add3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
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

fn validate_view_settings(view: ViewSettings) -> Result<(), RenderError> {
    validate_render_scale(view.render_scale)?;
    if view.exposure_ev.is_finite() && (-16.0..=16.0).contains(&view.exposure_ev) {
        Ok(())
    } else {
        Err(RenderError::InvalidConfig(
            "exposure_ev должен быть конечным и находиться в [-16, 16]",
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

fn inverse_extent(extent: vk::Extent2D) -> [f32; 2] {
    [1.0 / extent.width as f32, 1.0 / extent.height as f32]
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
    use super::culling::{Frustum, LocalBounds};
    use super::{
        AcquireAction, AlphaMode, AntiAliasing, CLUSTER_BOUNDS_BUFFER_SIZE,
        CLUSTER_BOUNDS_GPU_SIZE, CLUSTER_BUFFER_SIZE, CLUSTER_COMPUTE_LOCAL_SIZE,
        CLUSTER_COMPUTE_SHADER, CLUSTER_COUNT, CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z,
        CLUSTER_RECORD_GPU_SIZE, DEFAULT_UPLOAD_BATCH_CAPACITY, DeviceFacts, DeviceReport,
        DirectionalLight, DrawCommand, FRAME_UNIFORMS_SIZE, FULLSCREEN_VERTEX_SHADER, Frame,
        FrameCamera, GpuLocalLight, IDENTITY_MATRIX, LDR_FORMAT, LOCAL_LIGHT_GPU_SIZE,
        LOCAL_SHADOW_MAP_RESOLUTION, LOCAL_SHADOW_MATRIX_BUFFER_SIZE, LocalLight, LocalLightShadow,
        LocalLightShadowMatrices, MATERIAL_UNIFORMS_SIZE, MAX_CLUSTER_LIGHTS,
        MAX_LOCAL_SHADOW_LAYERS, MAX_RENDERED_LIGHTS, MESH_BLEND_FRAGMENT_SHADER,
        MESH_MASK_FRAGMENT_SHADER, MESH_OPAQUE_FRAGMENT_SHADER, MESH_VERTEX_SHADER, Material,
        MaterialId, MeshId, NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON, OFFSCREEN_FORMAT,
        OUTPUT_FRAGMENT_SHADER, OVERLAY_FRAGMENT_SHADER, OVERLAY_VERTEX_SHADER, OverlayVertex,
        PIPELINE_CACHE_HEADER_SIZE, PipelineCacheIdentity, PresentMode, ProbeInstanceFailure,
        RenderError, Renderer, RendererConfig, SHADOW_CASCADE_COUNT, SHADOW_FRAGMENT_SHADER,
        SHADOW_MAP_RESOLUTION, SHADOW_VERTEX_SHADER, SWAPCHAIN_USAGE, TONEMAP_FRAGMENT_SHADER,
        TextureColorSpace, TextureId, Tonemapper, UI_FRAGMENT_SHADER, UI_VERTEX_SHADER, Vertex,
        ViewSettings, align_upload_offset, best_device_index, check_extensions,
        classify_acquire_result, classify_probe_instance_failure, cluster_device_limit_reasons,
        deduplicate_names, derivative_tangent_basis, draw_rasterization_state, evaluate_device,
        frame_uniform_bytes, index_bytes, index_local_light_shadows, inverse_extent,
        local_light_bytes, local_light_bytes_for_selected, local_light_contribution,
        local_shadow_matrix_bytes, material_sampler_info, material_uniform_bytes,
        mesh_upload_layout, mip_extent, mip_level_count, missing_extensions,
        missing_offscreen_format_features, missing_post_format_features,
        missing_texture_format_features, normal_matrix, offscreen_previous_use,
        overlay_vertex_bytes, point_shadow_face_index, push_constant_bytes, sampler_anisotropy,
        scaled_extent, scene_viewport, select_composite_alpha, select_local_light_entries,
        select_local_lights, select_local_shadow_slots, select_present_mode, select_surface_extent,
        shadow_array_limits_suitable, shadow_caster_is_outside, shadow_casters,
        shadow_layout_source, shadow_subresource_range, texture_format, texture_format_reasons,
        texture_mip_blit_region, texture_mip_destination_shader_barrier,
        texture_mip_shader_barrier, texture_mip_source_barrier, texture_staging_size,
        texture_upload_barrier, timestamp_duration, transform_tangent_normal,
        upload_batch_capacity, upload_batch_flush_required, upload_data_layout, validate_draws,
        validate_material, validate_mesh, validate_overlay, validate_pipeline_cache,
        validate_render_scale, validate_texture, validate_view_settings, vertex_bytes,
    };
    use super::{LocalShadowKind, SelectedLocalLight};
    use ash::vk;
    use std::time::Duration;
    use std::{
        collections::HashSet,
        ffi::{CStr, CString},
    };

    fn assert_send<T: Send>() {}

    #[test]
    fn renderer_is_send() {
        assert_send::<Renderer>();
    }

    #[test]
    fn present_mode_selection_uses_requested_mode_or_fifo_for_every_supported_set() {
        let modes = [
            (PresentMode::Fifo, vk::PresentModeKHR::FIFO),
            (PresentMode::Mailbox, vk::PresentModeKHR::MAILBOX),
            (PresentMode::Immediate, vk::PresentModeKHR::IMMEDIATE),
        ];
        for mask in 0..(1 << modes.len()) {
            let supported = modes
                .iter()
                .enumerate()
                .filter_map(|(index, (_, mode))| (mask & (1 << index) != 0).then_some(*mode))
                .collect::<Vec<_>>();
            for (requested, vulkan_mode) in modes {
                let expected = if supported.contains(&vulkan_mode) {
                    vulkan_mode
                } else {
                    vk::PresentModeKHR::FIFO
                };
                assert_eq!(select_present_mode(requested, &supported), expected);
            }
        }
        assert_eq!(
            select_present_mode(PresentMode::Immediate, &[]),
            vk::PresentModeKHR::FIFO
        );
    }

    #[test]
    fn pipeline_cache_header_rejects_truncated_foreign_and_unsupported_data() {
        let identity = PipelineCacheIdentity {
            vendor_id: 0x1234,
            device_id: 0x5678,
            pipeline_cache_uuid: [0xabu8; vk::UUID_SIZE],
        };
        let mut cache = Vec::new();
        cache.extend_from_slice(&(PIPELINE_CACHE_HEADER_SIZE as u32).to_ne_bytes());
        cache.extend_from_slice(&vk::PipelineCacheHeaderVersion::ONE.as_raw().to_ne_bytes());
        cache.extend_from_slice(&identity.vendor_id.to_ne_bytes());
        cache.extend_from_slice(&identity.device_id.to_ne_bytes());
        cache.extend_from_slice(&identity.pipeline_cache_uuid);
        cache.extend_from_slice(b"driver data");

        assert!(validate_pipeline_cache(&[], identity).is_ok());
        assert!(validate_pipeline_cache(&cache, identity).is_ok());
        assert_eq!(super::compatible_pipeline_cache(&cache, identity), cache);
        assert!(super::compatible_pipeline_cache(&cache[..12], identity).is_empty());
        let foreign_identity = PipelineCacheIdentity {
            vendor_id: identity.vendor_id + 1,
            ..identity
        };
        assert!(super::compatible_pipeline_cache(&cache, foreign_identity).is_empty());
        assert!(
            validate_pipeline_cache(&cache[..PIPELINE_CACHE_HEADER_SIZE - 1], identity).is_err()
        );

        let mut wrong_size = cache.clone();
        wrong_size[0..4].copy_from_slice(&0_u32.to_ne_bytes());
        assert!(validate_pipeline_cache(&wrong_size, identity).is_err());

        let mut wrong_version = cache.clone();
        wrong_version[4..8].copy_from_slice(&2_i32.to_ne_bytes());
        assert!(validate_pipeline_cache(&wrong_version, identity).is_err());

        for offset in [8, 12, 16] {
            let mut foreign = cache.clone();
            foreign[offset] ^= 1;
            assert!(validate_pipeline_cache(&foreign, identity).is_err());
        }
    }

    #[test]
    fn texture_mip_chains_cover_square_odd_npot_and_thin_images() {
        let cases = [
            (8, 8, vec![(8, 8), (4, 4), (2, 2), (1, 1)]),
            (7, 5, vec![(7, 5), (3, 2), (1, 1)]),
            (13, 9, vec![(13, 9), (6, 4), (3, 2), (1, 1)]),
            (1, 1, vec![(1, 1)]),
            (1, 9, vec![(1, 9), (1, 4), (1, 2), (1, 1)]),
            (9, 1, vec![(9, 1), (4, 1), (2, 1), (1, 1)]),
        ];

        for (width, height, expected) in cases {
            let levels = mip_level_count(width, height);
            let actual = (0..levels)
                .map(|level| {
                    let extent = mip_extent(width, height, level);
                    (extent.width, extent.height)
                })
                .collect::<Vec<_>>();
            assert_eq!(levels, expected.len() as u32, "{width}x{height}");
            assert_eq!(actual, expected, "{width}x{height}");
        }
    }

    #[test]
    fn texture_mip_blit_region_uses_adjacent_levels_and_floor_extents() {
        let region = texture_mip_blit_region(
            1,
            vk::Extent2D {
                width: 6,
                height: 4,
            },
            vk::Extent2D {
                width: 3,
                height: 2,
            },
        );

        assert_eq!(region.src_subresource.mip_level, 1);
        assert_eq!(region.dst_subresource.mip_level, 2);
        assert_eq!(region.src_subresource.layer_count, 1);
        assert_eq!(region.dst_subresource.layer_count, 1);
        assert_eq!(region.src_offsets[1].x, 6);
        assert_eq!(region.src_offsets[1].y, 4);
        assert_eq!(region.dst_offsets[1].x, 3);
        assert_eq!(region.dst_offsets[1].y, 2);
    }

    #[test]
    fn texture_mip_barriers_cover_the_chain_and_publish_each_level() {
        let image = vk::Image::null();
        let levels = mip_level_count(16, 1);
        let initial = texture_upload_barrier(image, levels);
        assert_eq!(initial.image, image);
        assert_eq!(initial.subresource_range.base_mip_level, 0);
        assert_eq!(initial.subresource_range.level_count, levels);
        assert_eq!(initial.old_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(initial.new_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        assert_eq!(initial.src_stage_mask, vk::PipelineStageFlags2::NONE);
        assert_eq!(initial.src_access_mask, vk::AccessFlags2::NONE);
        assert_eq!(
            initial.dst_stage_mask,
            vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT
        );
        assert_eq!(initial.dst_access_mask, vk::AccessFlags2::TRANSFER_WRITE);
        assert_eq!(initial.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        assert_eq!(initial.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);

        for mip_level in 0..levels - 1 {
            let range = super::color_mip_subresource_range(mip_level, 1);
            let source = texture_mip_source_barrier(image, range);
            assert_eq!(source.subresource_range.base_mip_level, mip_level);
            assert_eq!(source.subresource_range.level_count, 1);
            assert_eq!(source.old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
            assert_eq!(source.new_layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
            assert_eq!(
                source.src_stage_mask,
                vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT
            );
            assert_eq!(source.src_access_mask, vk::AccessFlags2::TRANSFER_WRITE);
            assert_eq!(source.dst_stage_mask, vk::PipelineStageFlags2::BLIT);
            assert_eq!(source.dst_access_mask, vk::AccessFlags2::TRANSFER_READ);
            assert_eq!(source.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
            assert_eq!(source.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);

            let sampled = texture_mip_shader_barrier(image, range);
            assert_eq!(sampled.subresource_range.base_mip_level, mip_level);
            assert_eq!(sampled.subresource_range.level_count, 1);
            assert_eq!(sampled.old_layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
            assert_eq!(
                sampled.new_layout,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
            );
            assert_eq!(sampled.src_stage_mask, vk::PipelineStageFlags2::BLIT);
            assert_eq!(sampled.src_access_mask, vk::AccessFlags2::TRANSFER_READ);
            assert_eq!(
                sampled.dst_stage_mask,
                vk::PipelineStageFlags2::FRAGMENT_SHADER
            );
            assert_eq!(
                sampled.dst_access_mask,
                vk::AccessFlags2::SHADER_SAMPLED_READ
            );
            assert_eq!(sampled.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
            assert_eq!(sampled.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        }

        let final_level = texture_mip_destination_shader_barrier(
            image,
            super::color_mip_subresource_range(levels - 1, 1),
        );
        assert_eq!(final_level.subresource_range.base_mip_level, levels - 1);
        assert_eq!(final_level.subresource_range.level_count, 1);
        assert_eq!(
            final_level.old_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL
        );
        assert_eq!(
            final_level.new_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
        assert_eq!(
            final_level.src_stage_mask,
            vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::BLIT
        );
        assert_eq!(
            final_level.src_access_mask,
            vk::AccessFlags2::TRANSFER_WRITE
        );
        assert_eq!(
            final_level.dst_stage_mask,
            vk::PipelineStageFlags2::FRAGMENT_SHADER
        );
        assert_eq!(
            final_level.dst_access_mask,
            vk::AccessFlags2::SHADER_SAMPLED_READ
        );
        assert_eq!(final_level.src_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
        assert_eq!(final_level.dst_queue_family_index, vk::QUEUE_FAMILY_IGNORED);
    }

    #[test]
    fn output_attachment_transition_is_ordered_at_color_output() {
        let image = vk::Image::null();
        let barrier = super::output_attachment_barrier(image);

        assert_eq!(barrier.image, image);
        assert_eq!(
            barrier.src_stage_mask,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT
        );
        assert_eq!(barrier.src_access_mask, vk::AccessFlags2::NONE);
        assert_eq!(
            barrier.dst_stage_mask,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT
        );
        assert_eq!(
            barrier.dst_access_mask,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE
        );
        assert_eq!(barrier.old_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(
            barrier.new_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
        );
    }

    #[test]
    fn material_sampler_enables_mip_filtering_and_optional_anisotropy() {
        let cases = [
            (false, 16.0, None),
            (true, 8.0, Some(8.0)),
            (true, 32.0, Some(16.0)),
        ];

        for (supported, device_limit, expected_anisotropy) in cases {
            let anisotropy = sampler_anisotropy(supported, device_limit);
            assert_eq!(anisotropy, expected_anisotropy);
            let info = material_sampler_info(anisotropy);
            assert_eq!(info.mag_filter, vk::Filter::LINEAR);
            assert_eq!(info.min_filter, vk::Filter::LINEAR);
            assert_eq!(info.mipmap_mode, vk::SamplerMipmapMode::LINEAR);
            assert_eq!(info.min_lod, 0.0);
            assert_eq!(info.max_lod, vk::LOD_CLAMP_NONE);
            assert_eq!(
                info.anisotropy_enable == vk::TRUE,
                expected_anisotropy.is_some()
            );
            assert_eq!(info.max_anisotropy, expected_anisotropy.unwrap_or(1.0));
        }
    }

    #[test]
    fn texture_device_requirements_report_each_missing_format_feature() {
        let features = [
            (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
            (
                vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
                "SAMPLED_IMAGE_FILTER_LINEAR",
            ),
            (vk::FormatFeatureFlags::TRANSFER_SRC, "TRANSFER_SRC"),
            (vk::FormatFeatureFlags::TRANSFER_DST, "TRANSFER_DST"),
            (vk::FormatFeatureFlags::BLIT_SRC, "BLIT_SRC"),
            (vk::FormatFeatureFlags::BLIT_DST, "BLIT_DST"),
        ];

        let required_features = features
            .iter()
            .fold(vk::FormatFeatureFlags::empty(), |required, (feature, _)| {
                required | *feature
            });
        assert!(missing_texture_format_features(required_features).is_empty());
        for (feature, name) in features {
            assert_eq!(
                missing_texture_format_features(required_features & !feature),
                [name]
            );
        }
    }

    #[test]
    fn post_format_requires_attachment_sampling_and_linear_filtering() {
        let required = vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
        assert_eq!(LDR_FORMAT, vk::Format::R8G8B8A8_SRGB);
        assert!(missing_post_format_features(required).is_empty());
        for (feature, name) in [
            (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
            (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
            (
                vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
                "SAMPLED_IMAGE_FILTER_LINEAR",
            ),
        ] {
            assert_eq!(missing_post_format_features(required & !feature), [name]);
        }
    }

    #[test]
    fn hdr_scene_format_requires_blending_and_linear_sampling() {
        let required = vk::FormatFeatureFlags::COLOR_ATTACHMENT
            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
            | vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::STORAGE_IMAGE;
        assert_eq!(OFFSCREEN_FORMAT, vk::Format::R16G16B16A16_SFLOAT);
        assert!(missing_offscreen_format_features(required).is_empty());
        for (feature, name) in [
            (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
            (
                vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND,
                "COLOR_ATTACHMENT_BLEND",
            ),
            (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
            (
                vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
                "SAMPLED_IMAGE_FILTER_LINEAR",
            ),
            (vk::FormatFeatureFlags::STORAGE_IMAGE, "STORAGE_IMAGE"),
        ] {
            assert_eq!(
                missing_offscreen_format_features(required & !feature),
                [name]
            );
        }
    }

    #[test]
    fn device_report_explains_missing_hdr_blend_support() {
        let facts = DeviceFacts {
            api_version: vk::API_VERSION_1_3,
            dynamic_rendering: true,
            synchronization2: true,
            extended_dynamic_state: true,
            supported_extensions: HashSet::new(),
            graphics_queue_families: vec![true],
            compute_queue_families: vec![true],
            storage_image_extended_formats: true,
            offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::STORAGE_IMAGE,
        };
        let evaluation = evaluate_device(&facts, &[]);
        assert_eq!(evaluation.reasons.len(), 1);
        assert!(evaluation.reasons[0].contains("R16G16B16A16_SFLOAT"));
        assert!(evaluation.reasons[0].contains("COLOR_ATTACHMENT_BLEND"));
    }

    #[test]
    fn cluster_buffers_and_dispatch_fit_the_checked_device_limits() {
        assert_eq!(
            (CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z),
            (16, 9, 24)
        );
        assert_eq!(CLUSTER_COUNT, 3_456);
        assert_eq!(MAX_CLUSTER_LIGHTS, 128);
        assert_eq!(CLUSTER_RECORD_GPU_SIZE, 516);
        assert_eq!(CLUSTER_BUFFER_SIZE, 1_783_296);
        assert_eq!(CLUSTER_BOUNDS_GPU_SIZE, 32);
        assert_eq!(CLUSTER_BOUNDS_BUFFER_SIZE, 110_592);
        assert_eq!(std::mem::size_of::<super::clusters::ClusterBounds>(), 32);
        assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);

        let mut limits = vk::PhysicalDeviceLimits {
            max_compute_work_group_invocations: CLUSTER_COMPUTE_LOCAL_SIZE,
            max_compute_work_group_size: [CLUSTER_COMPUTE_LOCAL_SIZE, 1, 1],
            max_compute_work_group_count: [54, 1, 1],
            max_storage_buffer_range: CLUSTER_BUFFER_SIZE as u32,
            max_per_stage_descriptor_storage_buffers: 3,
            max_descriptor_set_storage_buffers: 4,
            ..vk::PhysicalDeviceLimits::default()
        };
        assert!(cluster_device_limit_reasons(&limits).is_empty());

        limits.max_compute_work_group_count[0] -= 1;
        assert!(
            cluster_device_limit_reasons(&limits)
                .iter()
                .any(|reason| reason.contains("workgroup"))
        );
        limits.max_compute_work_group_count[0] += 1;
        limits.max_storage_buffer_range -= 1;
        assert!(
            cluster_device_limit_reasons(&limits)
                .iter()
                .any(|reason| reason.contains("maxStorageBufferRange"))
        );
        limits.max_storage_buffer_range += 1;
        limits.max_per_stage_descriptor_storage_buffers -= 1;
        assert!(
            cluster_device_limit_reasons(&limits)
                .iter()
                .any(|reason| reason.contains("storage buffer descriptors"))
        );
        limits.max_per_stage_descriptor_storage_buffers += 1;
        limits.max_descriptor_set_storage_buffers -= 1;
        assert!(
            cluster_device_limit_reasons(&limits)
                .iter()
                .any(|reason| reason.contains("storage buffer descriptors"))
        );
    }

    #[test]
    fn shadow_array_device_limits_require_layer_count_and_directional_extent() {
        let mut properties = vk::ImageFormatProperties {
            max_extent: vk::Extent3D {
                width: SHADOW_MAP_RESOLUTION,
                height: SHADOW_MAP_RESOLUTION,
                depth: 1,
            },
            max_array_layers: MAX_LOCAL_SHADOW_LAYERS as u32,
            ..vk::ImageFormatProperties::default()
        };
        assert!(shadow_array_limits_suitable(&properties));
        properties.max_array_layers -= 1;
        assert!(!shadow_array_limits_suitable(&properties));
        properties.max_array_layers += 1;
        properties.max_extent.width -= 1;
        assert!(!shadow_array_limits_suitable(&properties));
    }

    #[test]
    fn device_evaluation_requires_compute_on_the_selected_graphics_queue_and_extended_formats() {
        let mut facts = DeviceFacts {
            api_version: vk::API_VERSION_1_3,
            dynamic_rendering: true,
            synchronization2: true,
            extended_dynamic_state: true,
            supported_extensions: HashSet::new(),
            graphics_queue_families: vec![true],
            compute_queue_families: vec![false],
            storage_image_extended_formats: false,
            offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::STORAGE_IMAGE,
        };
        let reasons = evaluate_device(&facts, &[]).reasons;
        assert!(reasons.contains(&NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON.to_owned()));
        assert!(
            reasons.contains(&"не поддерживается shaderStorageImageExtendedFormats".to_owned())
        );
        assert!(
            !reasons
                .iter()
                .any(|reason| reason.contains("STORAGE_IMAGE"))
        );

        facts.compute_queue_families[0] = true;
        facts.storage_image_extended_formats = true;
        facts.offscreen_format_features &= !vk::FormatFeatureFlags::STORAGE_IMAGE;
        let reasons = evaluate_device(&facts, &[]).reasons;
        assert!(!reasons.contains(&NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON.to_owned()));
        assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("STORAGE_IMAGE"))
        );
    }

    #[test]
    fn postprocess_formats_and_swapchain_usage_match_the_passes() {
        assert_eq!(OFFSCREEN_FORMAT, vk::Format::R16G16B16A16_SFLOAT);
        assert_eq!(LDR_FORMAT, vk::Format::R8G8B8A8_SRGB);
        assert_eq!(SWAPCHAIN_USAGE, vk::ImageUsageFlags::COLOR_ATTACHMENT);
    }

    #[test]
    fn texture_formats_require_sampled_blit_and_transfer_support_in_both_spaces() {
        let required = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::TRANSFER_DST
            | vk::FormatFeatureFlags::BLIT_SRC
            | vk::FormatFeatureFlags::BLIT_DST;
        assert_eq!(
            texture_format(TextureColorSpace::Srgb),
            vk::Format::R8G8B8A8_SRGB
        );
        assert_eq!(
            texture_format(TextureColorSpace::Linear),
            vk::Format::R8G8B8A8_UNORM
        );
        assert!(texture_format_reasons(required, required).is_empty());
        let srgb_missing =
            texture_format_reasons(required & !vk::FormatFeatureFlags::BLIT_SRC, required);
        assert_eq!(srgb_missing.len(), 1);
        assert!(srgb_missing[0].contains("R8G8B8A8_SRGB"));
        let linear_missing =
            texture_format_reasons(required, required & !vk::FormatFeatureFlags::TRANSFER_DST);
        assert_eq!(linear_missing.len(), 1);
        assert!(linear_missing[0].contains("R8G8B8A8_UNORM"));
    }

    #[test]
    fn offscreen_barrier_source_follows_the_previous_use_of_the_target() {
        assert_eq!(
            offscreen_previous_use(vk::ImageLayout::UNDEFINED),
            (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
        );
        assert_eq!(
            offscreen_previous_use(vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
            (
                vk::PipelineStageFlags2::TRANSFER,
                vk::AccessFlags2::TRANSFER_READ
            )
        );
        assert_eq!(
            offscreen_previous_use(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            (
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ
            )
        );
    }

    #[test]
    fn device_evaluation_accepts_all_shared_requirements() {
        let facts = DeviceFacts {
            api_version: vk::make_api_version(0, 1, 3, 0),
            dynamic_rendering: true,
            synchronization2: true,
            extended_dynamic_state: true,
            supported_extensions: HashSet::from([
                "VK_KHR_swapchain".to_owned(),
                "VK_KHR_portability_subset".to_owned(),
            ]),
            graphics_queue_families: vec![false, true],
            compute_queue_families: vec![false, true],
            storage_image_extended_formats: true,
            offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::STORAGE_IMAGE,
        };
        let required = [
            CString::new("VK_KHR_swapchain").unwrap(),
            CString::new("VK_KHR_portability_subset").unwrap(),
        ];

        let evaluation = evaluate_device(&facts, &required);

        assert!(evaluation.reasons.is_empty());
        assert_eq!(evaluation.graphics_queue_family_indices, [1]);
    }

    #[test]
    fn device_evaluation_accepts_api_major_version_two() {
        let facts = DeviceFacts {
            api_version: vk::make_api_version(0, 2, 0, 0),
            dynamic_rendering: true,
            synchronization2: true,
            extended_dynamic_state: true,
            supported_extensions: HashSet::new(),
            graphics_queue_families: vec![true],
            compute_queue_families: vec![true],
            storage_image_extended_formats: true,
            offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::STORAGE_IMAGE,
        };

        let evaluation = evaluate_device(&facts, &[]);

        assert!(evaluation.reasons.is_empty());
        assert_eq!(evaluation.graphics_queue_family_indices, [0]);
    }

    #[test]
    fn probe_instance_failure_distinguishes_missing_driver_from_other_errors() {
        assert_eq!(
            classify_probe_instance_failure(vk::Result::ERROR_INCOMPATIBLE_DRIVER),
            ProbeInstanceFailure::NoDriver
        );
        assert_eq!(
            classify_probe_instance_failure(vk::Result::ERROR_INITIALIZATION_FAILED),
            ProbeInstanceFailure::NoDriver
        );
        assert_eq!(
            classify_probe_instance_failure(vk::Result::ERROR_UNKNOWN),
            ProbeInstanceFailure::Vulkan(vk::Result::ERROR_UNKNOWN)
        );
    }

    #[test]
    fn device_evaluation_reports_each_missing_shared_requirement() {
        let facts = DeviceFacts {
            api_version: vk::make_api_version(0, 1, 2, 0),
            dynamic_rendering: false,
            synchronization2: false,
            extended_dynamic_state: false,
            supported_extensions: HashSet::new(),
            graphics_queue_families: vec![false, false],
            compute_queue_families: vec![false, false],
            storage_image_extended_formats: true,
            offscreen_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::STORAGE_IMAGE,
        };
        let required = [
            CString::new("VK_KHR_swapchain").unwrap(),
            CString::new("VK_KHR_portability_subset").unwrap(),
        ];

        let evaluation = evaluate_device(&facts, &required);

        assert_eq!(
            evaluation.reasons,
            [
                "Vulkan API ниже 1.3",
                "не поддерживается dynamicRendering",
                "не поддерживается synchronization2",
                "не поддерживается extendedDynamicState",
                "device extension недоступен: VK_KHR_swapchain",
                "device extension недоступен: VK_KHR_portability_subset",
                "нет graphics queue family",
            ]
        );
        assert!(evaluation.graphics_queue_family_indices.is_empty());
    }

    #[test]
    fn best_device_index_prefers_discrete_suitable_devices_and_keeps_first_tie() {
        let reports = [
            DeviceReport {
                name: "integrated".to_owned(),
                vendor_id: 1,
                api_version: 0,
                discrete: false,
                suitable: true,
                reasons: Vec::new(),
            },
            DeviceReport {
                name: "integrated tie".to_owned(),
                vendor_id: 2,
                api_version: 0,
                discrete: false,
                suitable: true,
                reasons: Vec::new(),
            },
            DeviceReport {
                name: "discrete".to_owned(),
                vendor_id: 3,
                api_version: 0,
                discrete: true,
                suitable: true,
                reasons: Vec::new(),
            },
            DeviceReport {
                name: "discrete tie".to_owned(),
                vendor_id: 4,
                api_version: 0,
                discrete: true,
                suitable: true,
                reasons: Vec::new(),
            },
            DeviceReport {
                name: "unsupported discrete".to_owned(),
                vendor_id: 5,
                api_version: 0,
                discrete: true,
                suitable: false,
                reasons: vec!["unsupported".to_owned()],
            },
        ];
        assert_eq!(
            best_device_index(&reports, |report| report.suitable, |report| report.discrete),
            Some(2)
        );
        assert_eq!(
            best_device_index(
                &reports[..2],
                |report| report.suitable,
                |report| report.discrete
            ),
            Some(0)
        );
        assert_eq!(
            best_device_index(
                &reports[2..4],
                |report| report.suitable,
                |report| report.discrete
            ),
            Some(0)
        );
        assert_eq!(
            best_device_index(
                &reports[4..],
                |report| report.suitable,
                |report| report.discrete
            ),
            None
        );
    }

    #[test]
    fn acquire_result_classification_preserves_success_suboptimal_and_skip_actions() {
        assert_eq!(
            classify_acquire_result(Ok((3, false))),
            AcquireAction::Acquired {
                image_index: 3,
                suboptimal: false,
            }
        );
        assert_eq!(
            classify_acquire_result(Ok((2, true))),
            AcquireAction::Acquired {
                image_index: 2,
                suboptimal: true,
            }
        );
        assert_eq!(
            classify_acquire_result(Err(vk::Result::ERROR_OUT_OF_DATE_KHR)),
            AcquireAction::OutOfDate
        );
        for result in [vk::Result::TIMEOUT, vk::Result::NOT_READY] {
            assert_eq!(classify_acquire_result(Err(result)), AcquireAction::Skipped);
        }
        assert_eq!(
            classify_acquire_result(Err(vk::Result::ERROR_DEVICE_LOST)),
            AcquireAction::Failed(vk::Result::ERROR_DEVICE_LOST)
        );
    }

    #[test]
    fn embedded_shaders_are_spirv_words() {
        for shader in [
            MESH_VERTEX_SHADER,
            MESH_OPAQUE_FRAGMENT_SHADER,
            MESH_MASK_FRAGMENT_SHADER,
            MESH_BLEND_FRAGMENT_SHADER,
            SHADOW_VERTEX_SHADER,
            SHADOW_FRAGMENT_SHADER,
            OVERLAY_VERTEX_SHADER,
            OVERLAY_FRAGMENT_SHADER,
            UI_VERTEX_SHADER,
            UI_FRAGMENT_SHADER,
            FULLSCREEN_VERTEX_SHADER,
            TONEMAP_FRAGMENT_SHADER,
            OUTPUT_FRAGMENT_SHADER,
            CLUSTER_COMPUTE_SHADER,
        ] {
            let words = ash::util::read_spv(&mut std::io::Cursor::new(shader)).unwrap();
            assert_eq!(words[0], 0x0723_0203);
        }
    }

    #[test]
    fn completed_uploads_release_staging_without_a_frame() {
        use std::rc::Rc;
        let staging = Rc::new(());
        let mut batches = vec![
            Rc::clone(&staging),
            Rc::clone(&staging),
            Rc::clone(&staging),
        ];
        let completed =
            super::completed_upload_count([Ok::<_, ()>(true), Ok(true), Ok(false)]).unwrap();
        drop(batches.drain(..completed));
        assert_eq!(Rc::strong_count(&staging), 2);
        let completed = super::completed_upload_count([Ok::<_, ()>(true)]).unwrap();
        drop(batches.drain(..completed));
        assert_eq!(Rc::strong_count(&staging), 1);
        assert_eq!(super::completed_upload_count([Ok(false), Err(())]), Ok(0));
        assert_eq!(super::completed_upload_count([Ok(true), Err(())]), Err(()));
    }

    #[test]
    fn back_face_reverses_the_complete_nonflat_mapped_normal() {
        let geometric = [0.0, 0.0, 1.0];
        let tangent_sample = [0.6, 0.0, 0.8];
        let front =
            transform_tangent_normal([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], geometric, tangent_sample);
        let back = front.map(|component| -component);
        assert!((back[0] + 0.6).abs() < 1e-6);
        assert!((back[2] + 0.8).abs() < 1e-6);
        // Tie the numeric reference to the shader's operation order: the full
        // mapped normal and geometric fallback must both flip after TBN mapping.
        let shader = include_str!("../shaders/mesh.frag");
        let mapping = shader.find("vec3 normal = geometric_normal;").unwrap();
        let flip = shader.find("if (!gl_FrontFacing)").unwrap();
        assert!(flip > mapping);
        let flip_block = shader[flip..].split('}').next().unwrap();
        assert!(flip_block.contains("geometric_normal = -geometric_normal;"));
        assert!(flip_block.contains("normal = -normal;"));
    }

    fn ggx_shader_has_analytic_peak(source: &str) -> bool {
        let normalized = source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let Some(function_start) = normalized.find("floatdistribution_ggx(") else {
            return false;
        };
        let Some(body_offset) = normalized[function_start..].find('{') else {
            return false;
        };
        let body_start = function_start + body_offset;
        let Some(body_length) = normalized[body_start..].find('}') else {
            return false;
        };
        let body = &normalized[body_start..=body_start + body_length];
        [
            "floatnormal_dot_half=clamp(normal_dot_half_input,0.0,1.0);",
            "floatalpha_squared=alpha*alpha;",
            "floatdenominator=(1.0-normal_dot_half*normal_dot_half)+alpha_squared*normal_dot_half*normal_dot_half;",
            "denominator=max(denominator,alpha_squared);",
            "returnalpha_squared/(PI*denominator*denominator);",
        ]
        .iter()
        .all(|expression| body.contains(expression))
            && !body.contains("max(PI*denominator*denominator,1e-7)")
    }

    #[test]
    fn ggx_shader_preserves_the_analytic_low_roughness_peak() {
        const PI: f32 = std::f32::consts::PI;
        let shader = include_str!("../shaders/mesh.frag");
        let shader_lf = shader.replace("\r\n", "\n");
        assert!(ggx_shader_has_analytic_peak(shader));
        assert!(ggx_shader_has_analytic_peak(&shader_lf));

        let function_lf = "float distribution_ggx(float normal_dot_half_input, float alpha) {\n    float normal_dot_half = clamp(normal_dot_half_input, 0.0, 1.0);\n    float alpha_squared = alpha * alpha;\n    float denominator = (1.0 - normal_dot_half * normal_dot_half) + alpha_squared * normal_dot_half * normal_dot_half;\n    denominator = max(denominator, alpha_squared);\n    return alpha_squared / (PI * denominator * denominator);\n}";
        let function_crlf = function_lf.replace('\n', "\r\n");
        assert!(ggx_shader_has_analytic_peak(function_lf));
        assert!(ggx_shader_has_analytic_peak(&function_crlf));

        let shader_whitespace_variant = function_lf
            .replace("float distribution_ggx(", "float \t distribution_ggx (")
            .replace(
                "normal_dot_half * normal_dot_half",
                "normal_dot_half \t *\r\n normal_dot_half",
            );
        assert!(ggx_shader_has_analytic_peak(&shader_whitespace_variant));

        let clipped_shader = "float distribution_ggx(float normal_dot_half_input, float alpha) {\n    float normal_dot_half = clamp(normal_dot_half_input, 0.0, 1.0);\n    float alpha_squared = alpha * alpha;\n    float denominator = (1.0 - normal_dot_half * normal_dot_half) + alpha_squared * normal_dot_half * normal_dot_half;\n    denominator = max(denominator, alpha_squared);\n    return alpha_squared / max(PI * denominator * denominator, 1e-7);\n}";
        assert!(!ggx_shader_has_analytic_peak(clipped_shader));

        let distribution = |normal_dot_half: f32, roughness: f32| {
            let normal_dot_half = normal_dot_half.clamp(0.0, 1.0);
            let alpha = roughness * roughness;
            let alpha_squared = alpha * alpha;
            let denominator = ((1.0 - normal_dot_half * normal_dot_half)
                + alpha_squared * normal_dot_half * normal_dot_half)
                .max(alpha_squared);
            alpha_squared / (PI * denominator * denominator)
        };
        let roughnesses = [0.045, 0.1, 0.25, 1.0];
        let mut peaks = Vec::with_capacity(roughnesses.len());
        for roughness in roughnesses {
            let alpha = roughness * roughness;
            let alpha_squared = alpha * alpha;
            let expected_peak = 1.0 / (PI * alpha_squared);
            let peak = distribution(1.0, roughness);
            assert!(
                (peak / expected_peak - 1.0).abs() < 2e-5,
                "{roughness}: {peak}"
            );
            assert!(distribution(0.0, roughness).is_finite());
            assert!(distribution(-0.5, roughness).is_finite());
            assert!(distribution(1.5, roughness).is_finite());
            peaks.push(peak);
        }

        assert!(peaks[0] > peaks[1] && peaks[1] > peaks[2] && peaks[2] > peaks[3]);
        let low_roughness_shoulder = distribution(0.99, roughnesses[0]) / peaks[0];
        let next_shoulder = distribution(0.99, roughnesses[1]) / peaks[1];
        assert!(low_roughness_shoulder < next_shoulder);
        assert_eq!(
            distribution(-0.5, roughnesses[0]),
            distribution(0.0, roughnesses[0])
        );
        assert_eq!(
            distribution(1.5, roughnesses[0]),
            distribution(1.0, roughnesses[0])
        );
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

    #[test]
    fn upload_offsets_are_at_least_sixteen_and_multiple_of_device_alignment() {
        let layout = mesh_upload_layout(0, 44, 12, 256).unwrap();
        assert_eq!(layout.vertex_offset, 256);
        assert_eq!(layout.index_offset, 512);
        assert!(layout.vertex_offset >= 16 && layout.vertex_offset.is_multiple_of(256));
        assert!(layout.index_offset >= 16 && layout.index_offset.is_multiple_of(256));

        let non_power_of_two = align_upload_offset(0, 24).unwrap();
        assert_eq!(non_power_of_two, 48);
        assert!(non_power_of_two.is_multiple_of(24));
    }

    #[test]
    fn upload_layout_rejects_offset_and_size_overflow() {
        assert_eq!(align_upload_offset(usize::MAX, 256), None);
        assert_eq!(mesh_upload_layout(0, usize::MAX, 4, 16), None);
        assert_eq!(upload_batch_capacity(usize::MAX, 16), None);
    }

    #[test]
    fn upload_batches_flush_on_capacity_and_isolate_oversized_meshes() {
        assert_eq!(
            upload_batch_flush_required(16, 44, 12, 16, 128),
            Some(false)
        );
        assert_eq!(upload_batch_flush_required(80, 44, 12, 16, 128), Some(true));

        let oversized = mesh_upload_layout(0, DEFAULT_UPLOAD_BATCH_CAPACITY + 1, 12, 16).unwrap();
        let capacity = upload_batch_capacity(oversized.end, 16).unwrap();
        assert!(capacity > DEFAULT_UPLOAD_BATCH_CAPACITY);
        assert_eq!(
            upload_batch_flush_required(oversized.end, 44, 12, 16, capacity),
            Some(true)
        );
    }

    #[test]
    fn texture_upload_budget_contains_only_base_level_rgba8_source_data() {
        let width = 4;
        let height = 4;
        let mip_levels = mip_level_count(width, height);
        let base_level_bytes = texture_staging_size(width, height).unwrap();
        let whole_chain_bytes = (0..mip_levels)
            .map(|mip| {
                let extent = mip_extent(width, height, mip);
                extent.width as usize * extent.height as usize * 4
            })
            .sum::<usize>();
        assert_eq!(base_level_bytes, 4 * 4 * 4);
        assert_eq!(whole_chain_bytes, 84);
        assert_eq!(base_level_bytes, 64);
        assert!(whole_chain_bytes > base_level_bytes);
        assert_eq!(upload_data_layout(0, base_level_bytes, 16), Some((16, 80)));
        assert_eq!(texture_staging_size(u32::MAX, u32::MAX), None);
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

    fn perspective_projection() -> [[f32; 4]; 4] {
        let near = 0.1;
        let far = 20.0;
        [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, far / (near - far), -1.0],
            [0.0, 0.0, near * far / (near - far), 0.0],
        ]
    }

    fn unproject(projection: [[f32; 4]; 4], ndc: [f32; 3]) -> [f32; 3] {
        let inverse = super::environment::inverse4x4(projection).unwrap();
        let point = std::array::from_fn::<_, 4, _>(|row| {
            inverse[0][row] * ndc[0]
                + inverse[1][row] * ndc[1]
                + inverse[2][row] * ndc[2]
                + inverse[3][row]
        });
        [
            point[0] / point[3],
            point[1] / point[3],
            point[2] / point[3],
        ]
    }

    fn project(projection: [[f32; 4]; 4], point: [f32; 3]) -> [f32; 3] {
        let clip = std::array::from_fn::<_, 4, _>(|row| {
            projection[0][row] * point[0]
                + projection[1][row] * point[1]
                + projection[2][row] * point[2]
                + projection[3][row]
        });
        [clip[0] / clip[3], clip[1] / clip[3], clip[2] / clip[3]]
    }

    fn camera(position: [f32; 3]) -> FrameCamera {
        FrameCamera {
            view: IDENTITY,
            projection: perspective_projection(),
            position,
        }
    }

    fn point(position: [f32; 3], color_intensity: [f32; 3], range: f32) -> LocalLight {
        LocalLight::Point {
            position,
            color_intensity,
            range,
        }
    }

    fn spot(
        position: [f32; 3],
        direction: [f32; 3],
        range: f32,
        cos_inner_angle: f32,
        cos_outer_angle: f32,
    ) -> LocalLight {
        LocalLight::Spot {
            position,
            direction,
            color_intensity: [1.0; 3],
            range,
            cos_inner_angle,
            cos_outer_angle,
        }
    }

    #[test]
    fn triangle_clip_space_camera_without_local_lights_can_prepare_a_frame() {
        for (width, height) in [(1600.0_f32, 900.0_f32), (900.0, 1600.0)] {
            // sandbox::scene::triangle_view_projection scales the wider axis.
            let aspect = width / height;
            let mut projection = IDENTITY_MATRIX;
            projection[0][0] = if aspect >= 1.0 { 1.0 / aspect } else { 1.0 };
            projection[1][1] = if aspect < 1.0 { aspect } else { 1.0 };
            let frame = Frame {
                clear_color: [0.0; 4],
                camera: FrameCamera {
                    view: IDENTITY_MATRIX,
                    projection,
                    position: [0.0; 3],
                },
                environment: None,
                shadow_cascades: &[],
                light: DirectionalLight::default(),
                lights: &[],
                local_light_shadows: &[],
                draws: &[],
                overlay: &[],
                view: ViewSettings::default(),
            };
            assert!(frame_uniform_bytes(&frame).is_ok());
        }
    }

    #[test]
    fn scaled_camera_view_is_rejected_before_cluster_sphere_packing() {
        let mut view = IDENTITY_MATRIX;
        view[0][0] = 2.0;
        view[1][1] = 2.0;
        view[2][2] = 2.0;
        for light in [
            point([0.0, 0.0, -3.0], [1.0; 3], 1.0),
            spot([0.0, 0.0, -3.0], [0.0, 0.0, -1.0], 1.0, 0.8, 0.5),
        ] {
            assert!(matches!(
                local_light_bytes(&[light], view),
                Err(RenderError::InvalidConfig(_))
            ));
        }
        assert!(local_light_bytes(&[], view).is_ok());
    }

    #[test]
    fn local_light_ssbo_uses_the_tested_std430_layout_and_valid_empty_binding() {
        assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);
        assert_eq!(std::mem::align_of::<GpuLocalLight>(), 16);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, position_range), 0);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, direction_type), 16);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, color_intensity), 32);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, cone_params), 48);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, view_position_range), 64);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, view_direction), 80);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, shadow_params), 96);
        assert_eq!(local_light_bytes(&[], IDENTITY_MATRIX).unwrap().len(), 112);
        assert_eq!(
            floats(&local_light_bytes(&[], IDENTITY_MATRIX).unwrap()),
            [0.0; 28]
        );

        let packed = local_light_bytes(
            &[
                point([1.0, 2.0, 3.0], [4.0, 5.0, 6.0], 7.0),
                spot([8.0, 9.0, 10.0], [0.0, 0.0, -1.0], 11.0, 0.8, 0.5),
            ],
            IDENTITY_MATRIX,
        )
        .unwrap();
        assert_eq!(packed.len(), 2 * 112);
        let values = floats(&packed);
        assert_eq!(
            &values[0..16],
            &[
                1.0, 2.0, 3.0, 7.0, 0.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0, 0.0, 0.0, 0.0, 0.0, 0.0
            ]
        );
        assert_eq!(
            &values[28..44],
            &[
                8.0, 9.0, 10.0, 11.0, 0.0, 0.0, -1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 0.8, 0.5, 0.0, 0.0
            ]
        );
        assert_eq!(&values[16..24], &[1.0, 2.0, 3.0, 7.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            &values[44..52],
            &[8.0, 9.0, 10.0, 11.0, 0.0, 0.0, -1.0, 0.0]
        );
    }

    #[test]
    fn local_shadow_slot_budget_is_distance_ordered_and_stably_maps_point_faces_and_spots() {
        let lights = (0..6)
            .map(|index| {
                let distance = match index {
                    0 | 1 => 1.0,
                    other => other as f32,
                };
                point([distance, 0.0, 0.0], [1.0; 3], 10.0)
            })
            .chain((6..9).map(|index| {
                let distance = if index < 8 { 5.5 } else { 6.0 };
                spot([distance, 0.0, 0.0], [0.0, 0.0, -1.0], 10.0, 0.8, 0.5)
            }))
            .collect::<Vec<_>>();
        let marker = |value: f32| [[value; 4]; 4];
        let mut requests = (0..6)
            .map(|index| LocalLightShadow {
                light_index: index,
                matrices: LocalLightShadowMatrices::Point {
                    matrices: std::array::from_fn(|face| {
                        marker(100.0 + index as f32 * 10.0 + face as f32)
                    }),
                },
            })
            .collect::<Vec<_>>();
        requests.extend((6..9).map(|index| LocalLightShadow {
            light_index: index,
            matrices: LocalLightShadowMatrices::Spot {
                matrix: marker(200.0 + index as f32),
            },
        }));
        let indexed = index_local_light_shadows(&lights, &requests).unwrap();
        let selected = lights
            .iter()
            .copied()
            .enumerate()
            .map(|(source_index, light)| SelectedLocalLight {
                source_index,
                light,
            })
            .collect::<Vec<_>>();
        let plan = select_local_shadow_slots(&selected, &indexed, [0.0; 3]);

        for (source_index, layer_base) in [(0, 0), (1, 6), (2, 12), (3, 18), (4, 24)] {
            let slot = plan.assignments[source_index].unwrap();
            assert_eq!(slot.layer_base, layer_base);
            assert_eq!(slot.kind, LocalShadowKind::Point);
        }
        assert!(plan.assignments[5].is_none());
        for (source_index, layer_base) in [(6, 30), (7, 31)] {
            let slot = plan.assignments[source_index].unwrap();
            assert_eq!(slot.layer_base, layer_base);
            assert_eq!(slot.kind, LocalShadowKind::Spot);
        }
        assert!(plan.assignments[8].is_none());
        assert_eq!(plan.matrices.len(), MAX_LOCAL_SHADOW_LAYERS);
        assert_eq!(plan.matrices[0][0][0], 100.0);
        assert_eq!(plan.matrices[6][0][0], 110.0);
        assert_eq!(plan.matrices[30][0][0], 206.0);
        assert_eq!(plan.matrices[31][0][0], 207.0);
    }

    #[test]
    fn local_shadow_flags_follow_light_indices_after_contribution_sort_and_cap() {
        let lights = (0..=MAX_RENDERED_LIGHTS)
            .map(|index| point([0.0, 0.0, -2.0], [index as f32 + 1.0; 3], 8.0))
            .collect::<Vec<_>>();
        let requests = [0, MAX_RENDERED_LIGHTS].map(|light_index| LocalLightShadow {
            light_index,
            matrices: LocalLightShadowMatrices::Point {
                matrices: [IDENTITY; 6],
            },
        });
        let indexed = index_local_light_shadows(&lights, &requests).unwrap();
        let selected = select_local_light_entries(camera([0.0; 3]), &lights).unwrap();
        assert_eq!(selected.len(), MAX_RENDERED_LIGHTS);
        assert_eq!(selected[0].source_index, MAX_RENDERED_LIGHTS);
        assert_eq!(selected[MAX_RENDERED_LIGHTS - 1].source_index, 1);

        let plan = select_local_shadow_slots(&selected, &indexed, [0.0; 3]);
        assert_eq!(plan.assignments[0].unwrap().layer_base, 0);
        assert!(plan.assignments.iter().skip(1).all(Option::is_none));
        assert_eq!(plan.matrices.len(), 6);
        let bytes = local_light_bytes_for_selected(&selected, &plan.assignments, IDENTITY).unwrap();
        let first_shadow_params = std::array::from_fn(|index| {
            u32::from_ne_bytes(bytes[96 + index * 4..100 + index * 4].try_into().unwrap())
        });
        assert_eq!(first_shadow_params, [0, 1, 1, 0]);
        let last_offset = (MAX_RENDERED_LIGHTS - 1) * LOCAL_LIGHT_GPU_SIZE + 96;
        let last_shadow_params = std::array::from_fn(|index| {
            let start = last_offset + index * 4;
            u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap())
        });
        assert_eq!(last_shadow_params, [0; 4]);
    }

    #[test]
    fn local_shadow_request_indices_reject_duplicates_type_mismatches_and_nonfinite_matrices() {
        let lights = [point([0.0; 3], [1.0; 3], 2.0)];
        let point_shadow = LocalLightShadow {
            light_index: 0,
            matrices: LocalLightShadowMatrices::Point {
                matrices: [IDENTITY; 6],
            },
        };
        assert!(index_local_light_shadows(&lights, &[point_shadow]).is_ok());
        assert!(index_local_light_shadows(&lights, &[point_shadow, point_shadow]).is_err());
        assert!(
            index_local_light_shadows(
                &lights,
                &[LocalLightShadow {
                    light_index: 1,
                    ..point_shadow
                }]
            )
            .is_err()
        );
        assert!(
            index_local_light_shadows(
                &lights,
                &[LocalLightShadow {
                    matrices: LocalLightShadowMatrices::Spot { matrix: IDENTITY },
                    ..point_shadow
                }]
            )
            .is_err()
        );
        let mut invalid = IDENTITY;
        invalid[0][0] = f32::NAN;
        assert!(
            index_local_light_shadows(
                &lights,
                &[LocalLightShadow {
                    matrices: LocalLightShadowMatrices::Point {
                        matrices: [invalid; 6],
                    },
                    ..point_shadow
                }]
            )
            .is_err()
        );
    }

    #[test]
    fn local_shadow_matrix_storage_keeps_64_byte_layer_stride_and_finite_padding() {
        assert_eq!(LOCAL_SHADOW_MAP_RESOLUTION, 1024);
        assert_eq!(LOCAL_SHADOW_MATRIX_BUFFER_SIZE, 32 * 64);
        let first = [[2.0; 4]; 4];
        let second = [[3.0; 4]; 4];
        let bytes = local_shadow_matrix_bytes(&[first, second]).unwrap();
        let values = floats(&bytes);
        assert_eq!(&values[..16], &[2.0; 16]);
        assert_eq!(&values[16..32], &[3.0; 16]);
        assert_eq!(&values[32..48], IDENTITY.as_flattened());
        assert!(local_shadow_matrix_bytes(&vec![IDENTITY; MAX_LOCAL_SHADOW_LAYERS + 1]).is_err());
        let mut invalid = IDENTITY;
        invalid[3][0] = f32::INFINITY;
        assert!(local_shadow_matrix_bytes(&[invalid]).is_err());
    }

    #[test]
    fn point_shadow_face_selection_matches_shader_axis_and_tie_order() {
        for (direction, expected) in [
            ([1.0, 0.0, 0.0], 0),
            ([-1.0, 0.0, 0.0], 1),
            ([0.0, 1.0, 0.0], 2),
            ([0.0, -1.0, 0.0], 3),
            ([0.0, 0.0, 1.0], 4),
            ([0.0, 0.0, -1.0], 5),
            ([1.0, 1.0, 0.0], 0),
            ([0.0, 1.0, 1.0], 2),
            ([1.0, 0.0, 1.0], 0),
        ] {
            assert_eq!(point_shadow_face_index(direction), expected);
        }
        let shader = include_str!("../shaders/mesh.frag");
        assert!(shader.contains("magnitude.x >= magnitude.y && magnitude.x >= magnitude.z"));
        assert!(shader.contains("if (magnitude.y >= magnitude.z)"));
    }

    #[test]
    fn local_shadow_matrix_and_light_layouts_match_all_shader_stages() {
        let fragment = include_str!("../shaders/mesh.frag");
        let compute = include_str!("../shaders/cluster.comp");
        let local_vertex = include_str!("../shaders/shadow.vert");
        for shader in [fragment, compute] {
            assert!(shader.contains("uvec4 shadow_params;"));
        }
        assert!(fragment.contains("layout(set = 0, binding = 6, std430)"));
        assert!(fragment.contains("layout(set = 0, binding = 7) uniform texture2DArray"));
        assert!(local_vertex.contains("layout(set = 0, binding = 6, std430)"));
        assert!(local_vertex.contains("mat4 matrices[MAX_LOCAL_SHADOW_LAYERS]"));
        assert_eq!(std::mem::offset_of!(GpuLocalLight, shadow_params), 96);
        assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);
    }

    #[test]
    fn local_light_ssbo_preserves_world_shading_data_and_adds_view_space_culling_data() {
        let view = [
            [0.0, 1.0, 0.0, 0.0],
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [-4.0, 2.0, 1.0, 1.0],
        ];
        let bytes = local_light_bytes(
            &[
                point([1.0, 2.0, 3.0], [1.0; 3], 4.0),
                spot([8.0, 9.0, 10.0], [1.0, 0.0, 0.0], 5.0, 0.8, 0.5),
            ],
            view,
        )
        .unwrap();
        let values = floats(&bytes);
        assert_eq!(&values[0..4], &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(&values[16..20], &[-6.0, 3.0, 4.0, 4.0]);
        assert_eq!(&values[28..32], &[8.0, 9.0, 10.0, 5.0]);
        assert_eq!(&values[44..48], &[-13.0, 10.0, 11.0, 5.0]);
        assert_eq!(&values[48..52], &[0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn local_light_selection_uses_actual_projection_boundaries_and_both_shapes() {
        let camera = camera([0.0; 3]);
        let boundary = unproject(camera.projection, [1.0, 0.0, 0.5]);
        let projected_boundary = project(camera.projection, boundary);
        assert!((projected_boundary[0] - 1.0).abs() < 1e-5);
        assert!((projected_boundary[2] - 0.5).abs() < 1e-5);

        let point_on_boundary = point(boundary, [1.0; 3], 0.02);
        assert_eq!(
            select_local_lights(camera, &[point_on_boundary])
                .unwrap()
                .len(),
            1
        );
        let point_outside = point(
            [boundary[0] + 0.2, boundary[1], boundary[2]],
            [1.0; 3],
            0.02,
        );
        assert!(
            select_local_lights(camera, &[point_outside])
                .unwrap()
                .is_empty()
        );

        let camera_inside_sphere = point([0.0, 0.0, -0.5], [1.0; 3], 1.0);
        assert_eq!(
            select_local_lights(camera, &[camera_inside_sphere])
                .unwrap()
                .len(),
            1
        );
        let camera_inside_cone = spot([0.0, 0.0, -2.0], [0.0, 0.0, 1.0], 4.0, 0.9, 0.8);
        assert_eq!(
            select_local_lights(camera, &[camera_inside_cone])
                .unwrap()
                .len(),
            1
        );

        let outside_apex = [boundary[0] + 0.08, boundary[1], boundary[2]];
        let cone_reaches_boundary = spot(
            outside_apex,
            [-0.6, 0.0, -0.8],
            0.5,
            10.0_f32.to_radians().cos(),
            10.0_f32.to_radians().cos(),
        );
        assert_eq!(
            select_local_lights(camera, &[cone_reaches_boundary])
                .unwrap()
                .len(),
            1
        );
        let wide_cone_reaches_boundary = spot(outside_apex, [-1.0, 0.0, 0.0], 0.2, 0.0, 0.0);
        assert_eq!(
            select_local_lights(camera, &[wide_cone_reaches_boundary])
                .unwrap()
                .len(),
            1
        );
        let cone_points_away = spot(outside_apex, [1.0, 0.0, 0.0], 0.5, 0.99, 0.99);
        assert!(
            select_local_lights(camera, &[cone_points_away])
                .unwrap()
                .is_empty()
        );

        let wide_cone = spot(
            [0.0, 0.0, -1.0],
            [0.0, 0.0, -1.0],
            3.0,
            45.0_f32.to_radians().cos(),
            0.0,
        );
        assert_eq!(select_local_lights(camera, &[wide_cone]).unwrap().len(), 1);
        let rounded_90_degree_cone = spot(
            [0.0, 0.0, -1.0],
            [0.0, 0.0, -1.0],
            3.0,
            90.0_f32.to_radians().cos(),
            90.0_f32.to_radians().cos(),
        );
        let selected = select_local_lights(camera, &[rounded_90_degree_cone]).unwrap();
        assert!(matches!(
            selected.as_slice(),
            [LocalLight::Spot {
                cos_inner_angle: 0.0,
                cos_outer_angle: 0.0,
                ..
            }]
        ));
        let narrow_cone = spot([0.0, 0.0, -2.0], [0.0, 0.0, 1.0], 4.0, 1.0, 0.5);
        assert_eq!(
            select_local_lights(camera, &[narrow_cone]).unwrap().len(),
            1
        );
    }

    #[test]
    fn local_light_selection_validates_every_input_and_caps_by_contribution_stably() {
        let camera = camera([0.0; 3]);
        let mut invalid_extra = vec![point([0.0, 0.0, -1.0], [1.0; 3], 2.0); MAX_RENDERED_LIGHTS];
        invalid_extra.push(point([0.0, 0.0, -1.0], [1.0; 3], f32::NAN));
        assert!(matches!(
            select_local_lights(camera, &invalid_extra),
            Err(RenderError::InvalidConfig(_))
        ));
        for invalid in [
            point([0.0, 0.0, -1.0], [-1.0, 1.0, 1.0], 2.0),
            point([f32::INFINITY, 0.0, -1.0], [1.0; 3], 2.0),
            spot([0.0, 0.0, -1.0], [0.0; 3], 2.0, 0.8, 0.5),
            spot([0.0, 0.0, -1.0], [0.0, 0.0, -1.0], 2.0, 1.001, 0.5),
            spot([0.0, 0.0, -1.0], [0.0, 0.0, -1.0], 2.0, 0.5, 0.8),
            spot([0.0, 0.0, -1.0], [0.0, 0.0, -1.0], 2.0, 0.8, -0.001),
        ] {
            assert!(matches!(
                select_local_lights(camera, &[invalid]),
                Err(RenderError::InvalidConfig(_))
            ));
        }

        let tied = (0..MAX_RENDERED_LIGHTS + 8)
            .map(|index| point([0.0, 0.0, 0.0], [1.0; 3], (index + 1) as f32))
            .collect::<Vec<_>>();
        let selected = select_local_lights(camera, &tied).unwrap();
        assert_eq!(selected.len(), MAX_RENDERED_LIGHTS);
        for (index, light) in selected.iter().enumerate() {
            assert_eq!(*light, point([0.0, 0.0, 0.0], [1.0; 3], (index + 1) as f32));
        }

        let sorted = select_local_lights(
            camera,
            &[
                point([0.0, 0.0, -5.0], [1.0; 3], 10.0),
                point([0.0, 0.0, -1.0], [1.0; 3], 10.0),
            ],
        )
        .unwrap();
        assert_eq!(sorted[0], point([0.0, 0.0, -1.0], [1.0; 3], 10.0));
    }

    #[test]
    fn spot_cone_score_uses_light_ray_sign_and_equal_angles_are_a_hard_edge() {
        let forward = spot([0.0; 3], [0.0, 0.0, -1.0], 5.0, 0.8, 0.8);
        assert!(local_light_contribution([0.0, 0.0, -1.0], forward) > 0.0);
        assert_eq!(local_light_contribution([0.0, 0.0, 1.0], forward), 0.0);
        let extreme = spot(
            [-f32::MAX, -f32::MAX, 0.0],
            [1.0, -1.0, 0.0],
            f32::MAX,
            0.9,
            0.8,
        );
        assert!(local_light_contribution([f32::MAX, f32::MAX, 0.0], extreme).is_finite());
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
    fn shadow_pass_skips_draws_that_do_not_cast_shadows() {
        let draw = |mesh, casts_shadow| DrawCommand {
            mesh: MeshId(mesh),
            material: None,
            model: IDENTITY,
            casts_shadow,
        };
        let draws = [draw(0, true), draw(1, false), draw(2, true), draw(3, true)];
        let modes = [
            AlphaMode::Opaque,
            AlphaMode::Opaque,
            AlphaMode::Mask { cutoff: 0.4 },
            AlphaMode::Blend,
        ];
        let casters: Vec<_> = shadow_casters(&draws, |draw| modes[draw.mesh.0])
            .map(|draw| draw.mesh.0)
            .collect();
        assert_eq!(casters, [0, 2]);
    }

    #[test]
    fn shadow_culling_uses_the_current_cascade_frustum() {
        let draw = DrawCommand {
            mesh: MeshId(0),
            material: None,
            model: IDENTITY,
            casts_shadow: true,
        };
        let bounds = LocalBounds {
            min: [-0.1, -0.1, 0.4],
            max: [0.1, 0.1, 0.6],
        };
        let first = Frustum::from_view_projection(IDENTITY);
        let mut second_matrix = IDENTITY;
        second_matrix[3][0] = -10.0;
        let second = Frustum::from_view_projection(second_matrix);

        assert!(!shadow_caster_is_outside(&draw, Some(bounds), Some(first)));
        assert!(shadow_caster_is_outside(&draw, Some(bounds), Some(second)));
        assert!(!shadow_caster_is_outside(&draw, None, Some(second)));
    }

    #[test]
    fn shadow_array_has_four_depth_layers_and_tracks_sample_layout_dependencies() {
        let range = shadow_subresource_range();
        assert_eq!(range.layer_count, SHADOW_CASCADE_COUNT as u32);
        assert_eq!(range.aspect_mask, vk::ImageAspectFlags::DEPTH);
        assert_eq!(
            shadow_layout_source(vk::ImageLayout::UNDEFINED).0,
            vk::PipelineStageFlags2::NONE
        );
        assert_eq!(
            shadow_layout_source(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            (
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            )
        );
        assert_eq!(SHADOW_MAP_RESOLUTION, 2048);
    }

    #[test]
    fn draw_validation_rejects_unknown_meshes_and_materials() {
        let draw = |mesh, material: Option<usize>| DrawCommand {
            mesh: MeshId(mesh),
            material: material.map(MaterialId),
            model: IDENTITY,
            casts_shadow: true,
        };
        let slots = |count: usize| vec![Some(()); count];
        assert!(validate_draws(&[], &slots(0), &slots(1)).is_ok());
        assert!(validate_draws(&[draw(0, None), draw(1, Some(1))], &slots(2), &slots(2)).is_ok());
        assert!(matches!(
            validate_draws(&[draw(0, None), draw(2, None)], &slots(2), &slots(2)),
            Err(RenderError::InvalidMesh(_))
        ));
        assert!(matches!(
            validate_draws(&[draw(0, Some(2))], &slots(1), &slots(2)),
            Err(RenderError::InvalidMaterial(_))
        ));
        assert!(matches!(
            validate_draws(&[draw(0, None)], &slots(1), &slots(0)),
            Err(RenderError::InvalidMaterial(_))
        ));
    }

    #[test]
    fn material_validation_checks_factors_texture_ids_and_color_spaces() {
        let spaces = [
            Some(TextureColorSpace::Srgb),
            Some(TextureColorSpace::Linear),
            Some(TextureColorSpace::Linear),
            Some(TextureColorSpace::Srgb),
            Some(TextureColorSpace::Linear),
        ];
        assert!(validate_material(&Material::default(), &spaces).is_ok());
        assert!(
            validate_material(
                &Material {
                    base_color: [0.0, 1.0, 0.5],
                    metallic: 1.0,
                    roughness: 0.0,
                    normal_scale: -3.0,
                    ..Material::default()
                },
                &spaces
            )
            .is_ok()
        );
        for material in [
            Material {
                base_color: [f32::NAN, 0.0, 0.0],
                ..Material::default()
            },
            Material {
                base_color: [1.01, 0.0, 0.0],
                ..Material::default()
            },
            Material {
                metallic: f32::INFINITY,
                ..Material::default()
            },
            Material {
                roughness: -0.01,
                ..Material::default()
            },
            Material {
                normal_scale: f32::NAN,
                ..Material::default()
            },
            Material {
                opacity: -0.01,
                ..Material::default()
            },
            Material {
                occlusion_strength: f32::NAN,
                ..Material::default()
            },
            Material {
                emissive: [0.0, -0.1, 0.0],
                ..Material::default()
            },
            Material {
                emissive: [0.0, f32::INFINITY, 0.0],
                ..Material::default()
            },
            Material {
                alpha_mode: AlphaMode::Mask { cutoff: 1.01 },
                ..Material::default()
            },
        ] {
            assert!(matches!(
                validate_material(&material, &spaces),
                Err(RenderError::InvalidMaterial(_))
            ));
        }
        for material in [
            Material {
                base_color_texture: Some(TextureId(1)),
                ..Material::default()
            },
            Material {
                metallic_roughness_texture: Some(TextureId(0)),
                ..Material::default()
            },
            Material {
                normal_texture: Some(TextureId(0)),
                ..Material::default()
            },
            Material {
                normal_texture: Some(TextureId(3)),
                ..Material::default()
            },
            Material {
                emissive_texture: Some(TextureId(1)),
                ..Material::default()
            },
            Material {
                occlusion_texture: Some(TextureId(0)),
                ..Material::default()
            },
            Material {
                occlusion_texture: Some(TextureId(5)),
                ..Material::default()
            },
        ] {
            assert!(matches!(
                validate_material(&material, &spaces),
                Err(RenderError::InvalidMaterial(_))
            ));
        }
        assert!(
            validate_material(
                &Material {
                    base_color_texture: Some(TextureId(0)),
                    metallic_roughness_texture: Some(TextureId(1)),
                    normal_texture: Some(TextureId(1)),
                    emissive_texture: Some(TextureId(3)),
                    occlusion_texture: Some(TextureId(4)),
                    ..Material::default()
                },
                &spaces
            )
            .is_ok()
        );
    }

    #[test]
    fn rasterization_state_uses_material_sidedness_and_model_determinant() {
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let mirrored_x = [
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let mirrored_xy = [
            [-1.0, 0.0, 0.0, 0.0],
            [0.0, -1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];

        assert_eq!(
            draw_rasterization_state(false, &identity),
            (vk::CullModeFlags::BACK, vk::FrontFace::COUNTER_CLOCKWISE)
        );
        assert_eq!(
            draw_rasterization_state(true, &identity),
            (vk::CullModeFlags::NONE, vk::FrontFace::COUNTER_CLOCKWISE)
        );
        assert_eq!(
            draw_rasterization_state(false, &mirrored_x),
            (vk::CullModeFlags::BACK, vk::FrontFace::CLOCKWISE)
        );
        assert_eq!(
            draw_rasterization_state(true, &mirrored_x),
            (vk::CullModeFlags::NONE, vk::FrontFace::CLOCKWISE)
        );
        assert_eq!(
            draw_rasterization_state(false, &mirrored_xy),
            (vk::CullModeFlags::BACK, vk::FrontFace::COUNTER_CLOCKWISE)
        );
        assert!(!Material::default().double_sided);
    }

    #[test]
    fn material_uniforms_pack_four_std140_vec4s_with_declared_offsets() {
        let material = Material {
            base_color: [0.2, 0.4, 0.8],
            metallic: 0.3,
            roughness: 0.7,
            normal_texture: Some(TextureId(2)),
            normal_scale: 1.5,
            emissive: [2.0, 3.0, 4.0],
            opacity: 0.6,
            alpha_mode: AlphaMode::Mask { cutoff: 0.25 },
            occlusion_strength: 0.7,
            ..Material::default()
        };
        assert_eq!(MATERIAL_UNIFORMS_SIZE, 64);
        assert_eq!(
            floats(&material_uniform_bytes(&material)),
            [
                0.2, 0.4, 0.8, 1.0, 0.3, 0.7, 1.5, 1.0, 2.0, 3.0, 4.0, 0.0, 0.6, 0.25, 1.0, 0.7
            ]
        );
        assert_eq!(
            &floats(&material_uniform_bytes(&Material::default()))[4..8],
            &[0.0, 1.0, 1.0, 0.0]
        );
        assert_eq!(
            &floats(&material_uniform_bytes(&Material::default()))[12..16],
            &[1.0, 0.0, 0.0, 1.0]
        );
    }

    #[test]
    fn derivative_tangent_basis_maps_v_down_texels_and_preserves_mirrored_orientation() {
        let normal = [0.0, 0.0, 1.0];
        let basis = derivative_tangent_basis(
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            normal,
        )
        .unwrap();
        assert_close(basis.0, [1.0, 0.0, 0.0]);
        assert_close(basis.1, [0.0, 1.0, 0.0]);
        let texel = [0.5, 1.0, 0.5];
        let tangent_normal = [
            texel[0] * 2.0 - 1.0,
            texel[1] * 2.0 - 1.0,
            texel[2] * 2.0 - 1.0,
        ];
        assert_close(
            transform_tangent_normal(basis.0, basis.1, normal, tangent_normal),
            [0.0, 1.0, 0.0],
        );

        let mirrored = derivative_tangent_basis(
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [-1.0, 0.0],
            [0.0, 1.0],
            normal,
        )
        .unwrap();
        assert_close(mirrored.0, [-1.0, 0.0, 0.0]);
        assert_close(mirrored.1, [0.0, 1.0, 0.0]);
        assert_close(
            transform_tangent_normal(mirrored.0, mirrored.1, normal, [1.0, 0.0, 0.0]),
            [-1.0, 0.0, 0.0],
        );
        assert_close(
            transform_tangent_normal(mirrored.0, mirrored.1, normal, [0.0, 1.0, 0.0]),
            [0.0, 1.0, 0.0],
        );
        let degenerate = derivative_tangent_basis(
            [1.0, 0.0, 0.0],
            [0.0, -1.0, 0.0],
            [1.0, 0.0],
            [2.0, 0.0],
            normal,
        )
        .unwrap_or(([0.0; 3], [0.0; 3]));
        assert_close(
            transform_tangent_normal(degenerate.0, degenerate.1, normal, [0.0, 0.0, 1.0]),
            normal,
        );
        assert!(
            derivative_tangent_basis(
                [1.0, 0.0, 0.0],
                [0.0, -1.0, 0.0],
                [1.0, 0.0],
                [2.0, 0.0],
                normal,
            )
            .is_none()
        );
        assert!(
            derivative_tangent_basis([0.0; 3], [0.0; 3], [1.0, 0.0], [0.0, 1.0], normal,).is_none()
        );
    }

    #[test]
    fn draw_validation_rejects_destroyed_meshes_and_materials() {
        let draw = |mesh, material: Option<usize>| DrawCommand {
            mesh: MeshId(mesh),
            material: material.map(MaterialId),
            model: IDENTITY,
            casts_shadow: true,
        };
        let alive_meshes = vec![Some(()), Some(())];
        let alive_materials = vec![Some(()), Some(())];
        let destroyed_mesh = vec![Some(()), None];
        let destroyed_material = vec![Some(()), None];
        assert!(matches!(
            validate_draws(&[draw(1, None)], &destroyed_mesh, &alive_materials),
            Err(RenderError::InvalidMesh(_))
        ));
        assert!(matches!(
            validate_draws(&[draw(0, Some(1))], &alive_meshes, &destroyed_material),
            Err(RenderError::InvalidMaterial(_))
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
    fn a_viewport_may_be_rendered_only_once_per_frame() {
        use super::{ViewportId, validate_scene_viewports};
        assert!(validate_scene_viewports([ViewportId(0), ViewportId(1)].into_iter()).is_ok());
        assert!(validate_scene_viewports([ViewportId(1), ViewportId(1)].into_iter()).is_err());
    }

    #[test]
    fn frame_uniforms_pack_cascade_matrices_and_light_at_std140_offsets() {
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
        let cascades = [matrix(17.0), matrix(33.0), matrix(49.0), matrix(65.0)];
        let frame = Frame {
            clear_color: [0.0; 4],
            camera: FrameCamera {
                view: IDENTITY,
                projection: perspective_projection(),
                position: [10.0, 11.0, 12.0],
            },
            environment: None,
            shadow_cascades: &cascades,
            light,
            lights: &[],
            local_light_shadows: &[],
            draws: &[],
            overlay: &[],
            view: ViewSettings::default(),
        };
        let values = floats(&frame_uniform_bytes(&frame).unwrap());
        assert_eq!(FRAME_UNIFORMS_SIZE, 640);
        let view_projection = perspective_projection();
        assert_eq!(&values[0..16], view_projection.as_flattened());
        assert_eq!(
            &values[16..32],
            super::environment::inverse4x4(view_projection)
                .unwrap()
                .as_flattened()
        );
        for (cascade, start) in [(0, 32), (1, 48), (2, 64), (3, 80)] {
            let expected = (1..=16)
                .map(|offset| (cascade as f32 * 16.0) + 17.0 + offset as f32 - 1.0)
                .collect::<Vec<_>>();
            assert_eq!(&values[start..start + 16], expected.as_slice());
            assert_eq!(cascades[cascade].as_flattened(), expected.as_slice());
        }
        assert_eq!(&values[96..100], &[1.0, 2.0, 3.0, 0.0]);
        assert_eq!(&values[100..104], &[4.0, 5.0, 6.0, 0.0]);
        assert_eq!(&values[104..108], &[7.0, 8.0, 9.0, 0.0]);
        assert_eq!(&values[108..112], &[1.0, 1.0 / 2048.0, 4.0, 0.0]);
        assert_eq!(&values[112..116], &[10.0, 11.0, 12.0, 0.0]);
        assert_eq!(&values[136..140], &[0.0; 4]);
        assert_eq!(&values[140..156], IDENTITY.as_flattened());
        assert!((values[156] - 0.1).abs() < 1e-5);
        assert!((values[157] - 20.0).abs() < 1e-3);
        assert_eq!(&values[158..160], &[1.0, 1.0]);

        let no_shadows = Frame {
            shadow_cascades: &[],
            ..frame
        };
        assert_eq!(floats(&frame_uniform_bytes(&no_shadows).unwrap())[108], 0.0);
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
                        camera: FrameCamera {
                            view: IDENTITY,
                            projection: IDENTITY,
                            position: [0.0; 3],
                        },
                        environment: None,
                        shadow_cascades: &[],
                        light,
                        lights: &[],
                        local_light_shadows: &[],
                        draws: &[],
                        overlay: &[],
                        view: ViewSettings::default(),
                    }),
                    Err(RenderError::InvalidConfig(_))
                ),
                "{direction:?}"
            );
        }

        let mut invalid = IDENTITY;
        invalid[2][1] = f32::NAN;
        let invalid_cascades = [invalid];
        let frame = Frame {
            clear_color: [0.0; 4],
            camera: FrameCamera {
                view: IDENTITY,
                projection: IDENTITY,
                position: [0.0; 3],
            },
            environment: None,
            shadow_cascades: &invalid_cascades,
            light: DirectionalLight::default(),
            lights: &[],
            local_light_shadows: &[],
            draws: &[],
            overlay: &[],
            view: ViewSettings::default(),
        };
        assert!(matches!(
            frame_uniform_bytes(&frame),
            Err(RenderError::InvalidConfig(_))
        ));
    }

    #[test]
    fn frame_uniforms_reject_more_than_four_shadow_cascades() {
        let too_many = [IDENTITY; 5];
        let frame = Frame {
            clear_color: [0.0; 4],
            camera: FrameCamera {
                view: IDENTITY,
                projection: IDENTITY,
                position: [0.0; 3],
            },
            environment: None,
            shadow_cascades: &too_many,
            light: DirectionalLight::default(),
            lights: &[],
            local_light_shadows: &[],
            draws: &[],
            overlay: &[],
            view: ViewSettings::default(),
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
    fn default_config_has_no_extra_extensions_and_view_defaults_to_full_quality() {
        let config = RendererConfig::default();
        assert!(config.extra_instance_extensions.is_empty());
        assert!(config.extra_device_extensions.is_empty());
        let view = ViewSettings::default();
        assert_eq!(view.render_scale, 1.0);
        assert_eq!(view.exposure_ev, 0.0);
        assert_eq!(view.tonemapper, Tonemapper::Aces);
        assert_eq!(view.anti_aliasing, AntiAliasing::Fxaa);
        assert!(validate_view_settings(view).is_ok());
    }

    #[test]
    fn postprocess_enum_codes_are_stable_for_the_shader_contract() {
        assert_eq!(Tonemapper::Aces.code(), 0);
        assert_eq!(Tonemapper::Neutral.code(), 1);
        assert_eq!(Tonemapper::None.code(), 2);
        assert_eq!(AntiAliasing::Fxaa.code(), 0);
        assert_eq!(AntiAliasing::None.code(), 1);
    }

    #[test]
    fn view_settings_validate_scale_and_exposure_ranges() {
        for render_scale in [1.0, 0.5, f32::MIN_POSITIVE] {
            assert!(
                validate_view_settings(ViewSettings {
                    render_scale,
                    ..ViewSettings::default()
                })
                .is_ok()
            );
        }
        for exposure_ev in [-16.0, 0.0, 16.0] {
            assert!(
                validate_view_settings(ViewSettings {
                    exposure_ev,
                    ..ViewSettings::default()
                })
                .is_ok()
            );
        }
        for render_scale in [0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.01, -0.1] {
            assert!(matches!(
                validate_view_settings(ViewSettings {
                    render_scale,
                    ..ViewSettings::default()
                }),
                Err(RenderError::InvalidConfig(_))
            ));
        }
        for exposure_ev in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -16.01, 16.01] {
            assert!(matches!(
                validate_view_settings(ViewSettings {
                    exposure_ev,
                    ..ViewSettings::default()
                }),
                Err(RenderError::InvalidConfig(_))
            ));
        }
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
    fn output_pass_uses_reciprocal_source_extent_for_fxaa() {
        assert_eq!(
            inverse_extent(vk::Extent2D {
                width: 1280,
                height: 720,
            }),
            [1.0 / 1280.0, 1.0 / 720.0]
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

// The renderer has no CPU GLSL interpreter; build.rs compiles the actual shader with Naga.
// This small numeric oracle covers edge-coordinate behavior without a Vulkan device.
#[cfg(test)]
mod fxaa_reference_tests {
    fn fxaa_edge_offset(
        sample_luma: impl Fn(f32, f32) -> f32,
        uv: [f32; 2],
        pixel: [f32; 2],
    ) -> (bool, [f32; 2], f32) {
        let at = |x: f32, y: f32| sample_luma(uv[0] + x * pixel[0], uv[1] + y * pixel[1]);
        let m = at(0.0, 0.0);
        let n = at(0.0, -1.0);
        let w = at(-1.0, 0.0);
        let e = at(1.0, 0.0);
        let s = at(0.0, 1.0);
        let nw = at(-1.0, -1.0);
        let ne = at(1.0, -1.0);
        let sw = at(-1.0, 1.0);
        let se = at(1.0, 1.0);

        let horizontal_1 = -2.0 * m + n + s;
        let vertical_1 = -2.0 * m + w + e;
        let horizontal_2 = -2.0 * e + ne + se;
        let vertical_2 = -2.0 * n + nw + ne;
        let horizontal_3 = -2.0 * w + nw + sw;
        let vertical_3 = -2.0 * s + sw + se;
        let edge_horizontal = horizontal_3.abs() + 2.0 * horizontal_1.abs() + horizontal_2.abs();
        let edge_vertical = vertical_3.abs() + 2.0 * vertical_1.abs() + vertical_2.abs();
        let horizontal = edge_horizontal >= edge_vertical;

        let (luma_a, luma_b) = if horizontal { (n, s) } else { (w, e) };
        let gradient_a = (luma_a - m).abs();
        let gradient_b = (luma_b - m).abs();
        let use_a = gradient_a >= gradient_b;
        let edge_luma = 0.5 * (m + if use_a { luma_a } else { luma_b });
        let gradient = gradient_a.max(gradient_b);
        let normal = if horizontal { [0.0, 1.0] } else { [1.0, 0.0] };
        let along = if horizontal { [1.0, 0.0] } else { [0.0, 1.0] };
        let side = if use_a { -0.5 } else { 0.5 };
        let edge_start = [normal[0] * side, normal[1] * side];
        let mut end_a = [edge_start[0] - along[0], edge_start[1] - along[1]];
        let mut end_b = [edge_start[0] + along[0], edge_start[1] + along[1]];
        let mut luma_end_a = at(end_a[0], end_a[1]);
        let mut luma_end_b = at(end_b[0], end_b[1]);
        let mut done_a = (luma_end_a - edge_luma).abs() >= gradient * 0.25;
        let mut done_b = (luma_end_b - edge_luma).abs() >= gradient * 0.25;
        const STEPS: [f32; 11] = [1.0, 1.0, 1.0, 1.0, 1.5, 2.0, 2.0, 2.0, 2.0, 4.0, 8.0];
        for step in STEPS {
            if !done_a {
                end_a[0] -= along[0] * step;
                end_a[1] -= along[1] * step;
                luma_end_a = at(end_a[0], end_a[1]);
                done_a = (luma_end_a - edge_luma).abs() >= gradient * 0.25;
            }
            if !done_b {
                end_b[0] += along[0] * step;
                end_b[1] += along[1] * step;
                luma_end_b = at(end_b[0], end_b[1]);
                done_b = (luma_end_b - edge_luma).abs() >= gradient * 0.25;
            }
        }

        let (distance_a, distance_b) = if horizontal {
            (end_a[0].abs(), end_b[0].abs())
        } else {
            (end_a[1].abs(), end_b[1].abs())
        };
        let nearest_luma = if distance_a < distance_b {
            luma_end_a
        } else {
            luma_end_b
        };
        let good_span = (nearest_luma < edge_luma) != (m < edge_luma);
        let edge_offset = if good_span {
            0.5 - distance_a.min(distance_b) / (distance_a + distance_b)
        } else {
            0.0
        };
        let direction = if use_a { -1.0 } else { 1.0 };
        (
            horizontal,
            [normal[0] * direction, normal[1] * direction],
            edge_offset,
        )
    }

    fn finite_step_luma(
        position: [f32; 2],
        horizontal: bool,
        center_positive: bool,
        bright_inside: bool,
    ) -> f32 {
        let texel = |x: i32, y: i32| {
            let (across, along) = if horizontal { (y, x) } else { (x, y) };
            let inside_side = if center_positive {
                across >= 1
            } else {
                across <= 0
            };
            let inside = (-1..=3).contains(&along) && inside_side;
            let high = if bright_inside { inside } else { !inside };
            if high { 1.0 } else { 0.0 }
        };
        let x0 = position[0].floor() as i32;
        let y0 = position[1].floor() as i32;
        let tx = position[0] - x0 as f32;
        let ty = position[1] - y0 as f32;
        let top = texel(x0, y0) * (1.0 - tx) + texel(x0 + 1, y0) * tx;
        let bottom = texel(x0, y0 + 1) * (1.0 - tx) + texel(x0 + 1, y0 + 1) * tx;
        top * (1.0 - ty) + bottom * ty
    }

    #[test]
    fn finite_step_edges_have_oriented_offsets_for_both_polarities() {
        for horizontal in [false, true] {
            for bright_inside in [false, true] {
                let center_positive = bright_inside;
                let uv = if horizontal {
                    [0.0, if center_positive { 1.0 } else { 0.0 }]
                } else {
                    [if center_positive { 1.0 } else { 0.0 }, 0.0]
                };
                let sample =
                    |x, y| finite_step_luma([x, y], horizontal, center_positive, bright_inside);
                let (detected_horizontal, direction, offset) =
                    fxaa_edge_offset(sample, uv, [1.0, 1.0]);
                let expected_sign = if center_positive { -1.0 } else { 1.0 };
                let normal_axis = if horizontal { 1 } else { 0 };

                assert_eq!(detected_horizontal, horizontal);
                assert!(
                    offset.is_finite() && offset > 0.0,
                    "{horizontal}, {bright_inside}"
                );
                assert!(
                    direction[normal_axis] * expected_sign > 0.0,
                    "{horizontal}, {bright_inside}: {direction:?}"
                );
                assert_eq!(direction[1 - normal_axis], 0.0);
            }
        }
    }

    #[test]
    fn subpixel_filter_uses_weighted_twelve_sample_average_and_squared_smoothstep() {
        let center: f32 = 0.5;
        let cardinal_sum: f32 = 4.0;
        let diagonal_sum: f32 = 0.0;
        let range: f32 = 1.0;
        let average = (2.0 * cardinal_sum + diagonal_sum) / 12.0;
        let value = ((average - center).abs() / range).clamp(0.0, 1.0);
        let smooth = value * value * (3.0 - 2.0 * value);
        let offset = smooth * smooth * 0.75;

        assert!((offset - 0.004_115_226).abs() < 1e-6);
    }
}
