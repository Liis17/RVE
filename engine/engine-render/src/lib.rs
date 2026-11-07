//! Vulkan-рендерер, форматы изображений и материалы сцены.

#[cfg(windows)]
use ash::vk::Handle as _;
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
use std::time::{Duration, Instant};
use temporal::{Extent as TemporalExtent, TemporalKey, TemporalPlan, TemporalState};
use ui::{
    PackedUi, RetireQueue, RetiredSwapchains, UiDraw, is_srgb_format, pack_ui,
    validate_texture_region, validate_viewport_size,
};

mod clusters;
mod culling;
mod particle_gpu;
mod particles;

use particle_gpu::{ParticleEmitter, ParticlePass, ParticleSystem, record_particles};
pub use particles::{
    MAX_ADDITIVE_PARTICLE_CAPACITY, MAX_ALPHA_PARTICLE_CAPACITY, MAX_PARTICLE_CURVE_KEYS,
    MAX_PARTICLE_STEP, ParticleBlend, ParticleBurst, ParticleCollision, ParticleColorKey,
    ParticleEmitterDesc, ParticleFlipbook, ParticleShape, ParticleSizeKey,
};
#[cfg(any(test, all(windows, feature = "dlss")))]
mod dlss;
#[allow(dead_code)]
mod fsr;
mod temporal;
mod ui;

include!(concat!(env!("OUT_DIR"), "/render_constants.rs"));

pub use ui::{UiImage, UiPrimitive, UiTextureId, UiVertex, ViewportId};

const FRAMES_IN_FLIGHT: usize = 2;
const DEFAULT_UPLOAD_BATCH_CAPACITY: usize = 64 * 1024 * 1024;
const OFFSCREEN_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
const VELOCITY_FORMAT: vk::Format = vk::Format::R16G16_SFLOAT;
const LDR_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const DEPTH_FORMAT: vk::Format = vk::Format::D32_SFLOAT;
const SRGB_TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_SRGB;
const LINEAR_TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// egui хранит значения в гамма-пространстве, поэтому они читаются без перевода; шейдер сам
/// переводит результат для sRGB-swapchain, и смешивание в этом случае идёт в линейном пространстве.
const UI_TEXTURE_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
const WIREFRAME_FRAGMENT_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/wireframe.frag.spv"));
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
const TAA_FRAGMENT_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/taa.frag.spv"));
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
const TAA_PUSH_CONSTANTS_SIZE: usize = size_of::<[f32; 4]>();
const PIPELINE_CACHE_HEADER_SIZE: usize = size_of::<vk::PipelineCacheHeaderVersionOne>();
/// Максимум видов с GPU-границами за кадр: основной экран или viewport редактора.
pub const MAX_PROFILED_VIEWS: usize = 4;
/// Запросы одного вида: начало и граница после каждого прохода `GpuPass`.
const VIEW_QUERY_COUNT: u32 = GpuPass::COUNT as u32 + 1;
/// Первый запрос вывода на swapchain: начало и конец Output+UI.
const OUTPUT_QUERY_FIRST: u32 = MAX_PROFILED_VIEWS as u32 * VIEW_QUERY_COUNT;
/// Все запросы пула слота кадра.
const TIMESTAMP_QUERY_COUNT: u32 = OUTPUT_QUERY_FIRST + 2;
const WHITE_SRGB_TEXTURE: usize = 0;
const WHITE_LINEAR_TEXTURE: usize = 1;
const FLAT_NORMAL_TEXTURE: usize = 2;
const DEFAULT_MATERIAL: usize = 0;
const PUSH_CONSTANTS_SIZE: usize = 3 * size_of::<[[f32; 4]; 4]>() + size_of::<[u32; 4]>();
const FRAME_UNIFORMS_SIZE: usize =
    (5 + SHADOW_CASCADE_COUNT) * size_of::<[[f32; 4]; 4]>() + 9 * size_of::<[f32; 4]>();
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
    /// Previous frame's world transform, or `None` to reuse `model`.
    pub previous_model: Option<[[f32; 4]; 4]>,
    /// Selected mesh LOD; zero is the highest detail level.
    pub lod_level: u8,
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

/// Идентификатор GPU-эмиттера частиц, выдаётся `Renderer::create_particle_emitter`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParticleEmitterId(usize);

/// Эмиттер, который вид показывает в кадре.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleEmitterFrame {
    pub emitter: ParticleEmitterId,
    /// Мировая матрица эмиттера (столбцы).
    pub transform: [[f32; 4]; 4],
    /// Рождение новых частиц; симуляция продолжается и при `false`.
    pub emitting: bool,
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
    pub debug_view: DebugView,
    pub view: ViewSettings,
    /// Discards temporal history for this view before rendering it.
    pub reset_history: bool,
    /// Эмиттеры частиц, которые показывает вид. Симулирует первый записанный вид с `particle_step > 0`.
    pub particles: &'a [ParticleEmitterFrame],
    /// Шаг симуляции частиц в секундах; 0 — только отрисовка. Значения больше `MAX_PARTICLE_STEP` обрезаются.
    pub particle_step: f32,
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
    Taa,
    Fsr,
    Dlss,
}

/// Alternate scene outputs used to inspect geometry and lighting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DebugView {
    #[default]
    Lit,
    Wireframe,
    Normals,
    Overdraw,
    LodLevels,
    ShadowCascades,
}

impl DebugView {
    /// Stable shader-facing value; zero is the normal lit renderer path.
    pub const fn code(self) -> u32 {
        match self {
            Self::Lit => 0,
            Self::Wireframe => 1,
            Self::Normals => 2,
            Self::Overdraw => 3,
            Self::LodLevels => 4,
            Self::ShadowCascades => 5,
        }
    }

    const fn has_linear_output(self) -> bool {
        matches!(
            self,
            Self::Normals | Self::Overdraw | Self::LodLevels | Self::ShadowCascades
        )
    }
}

impl AntiAliasing {
    const fn code(self) -> u32 {
        match self {
            Self::Fxaa => 0,
            Self::None => 1,
            Self::Taa => 2,
            Self::Fsr => 3,
            Self::Dlss => 4,
        }
    }

    const fn is_temporal(self) -> bool {
        matches!(self, Self::Taa | Self::Fsr | Self::Dlss)
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TemporalBackend {
    Taa,
    Fsr,
    Dlss,
}

#[cfg(any(windows, test))]
fn select_temporal_backend(
    anti_aliasing: AntiAliasing,
    fsr_available: bool,
    dlss_available: bool,
) -> TemporalBackend {
    match anti_aliasing {
        AntiAliasing::Fsr if fsr_available => TemporalBackend::Fsr,
        AntiAliasing::Dlss if dlss_available => TemporalBackend::Dlss,
        _ => TemporalBackend::Taa,
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
    pub upscalers: UpscalerConfig,
}

/// Optional vendor runtime configuration for temporal upscalers.
#[derive(Default)]
pub struct UpscalerConfig {
    /// Path to the signed AMD `amd_fidelityfx_loader_vk.dll` runtime on Windows.
    pub fsr_library: Option<std::path::PathBuf>,
    /// Path to the NVIDIA `nvngx_dlss.dll` feature runtime on Windows.
    pub dlss_library: Option<std::path::PathBuf>,
    /// NGX custom project GUID-like identifier, mutually exclusive with `dlss_application_id`.
    pub dlss_project_id: Option<String>,
    /// NVIDIA-provided NGX application ID, mutually exclusive with `dlss_project_id`.
    pub dlss_application_id: Option<u64>,
    /// Engine version supplied with `dlss_project_id`.
    pub dlss_engine_version: Option<String>,
}

/// Возможности выбранного физического устройства.
pub struct Capabilities {
    pub device_name: String,
    pub vendor_id: u32,
    pub api_version: u32,
    pub discrete: bool,
    pub tessellation_shader: bool,
    pub wireframe: bool,
    pub fsr: bool,
    pub dlss: bool,
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

enum SubmitAndPresentError {
    BeforeSubmit(RenderError),
    AfterSubmit(RenderError),
}

struct AcquiredFrameSubmission<'a> {
    device: &'a ash::Device,
    swapchain_loader: &'a ash::khr::swapchain::Device,
    queue: vk::Queue,
    frame_slot: usize,
    fence: vk::Fence,
    image_available: vk::Semaphore,
    command_buffer: vk::CommandBuffer,
    swapchain_handle: vk::SwapchainKHR,
    render_finished: vk::Semaphore,
    image_index: usize,
    acquire_suboptimal: bool,
    wait_stage: vk::PipelineStageFlags2,
    timestamps_written: bool,
    timings: &'a mut RenderCpuTimings,
    temporal_submissions: &'a [TemporalSubmission],
    shadow_layout: vk::ImageLayout,
    local_shadow_layout: vk::ImageLayout,
}

/// Результат попытки показать кадр.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameStatus {
    Presented,
    Skipped,
}

/// Presentation status and whether this render attempt reached a successful queue submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameOutcome {
    pub status: FrameStatus,
    /// True after `vkQueueSubmit2` succeeds, including a later out-of-date present.
    pub submitted: bool,
}

/// Проход, границы которого записываются в GPU-профайлер; порядок совпадает с выполнением в кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuPass {
    /// Compute кластеризации света.
    Clusters,
    /// Каскады directional shadow.
    ShadowDirectional,
    /// Локальные теневые слои.
    ShadowLocal,
    /// Непрозрачные и маскированные меши.
    Opaque,
    /// Skybox.
    Skybox,
    /// Полупрозрачные меши.
    Transparent,
    /// Отладочный wireframe.
    Wireframe,
    /// Частицы: spawn, симуляция и аддитивные billboards.
    Particles,
    /// TAA, FSR или DLSS; отсутствует, если временное сглаживание выключено.
    TemporalResolve,
    /// Tonemap до вывода на swapchain.
    Tonemap,
}

impl GpuPass {
    /// Число проходов, для которых записываются границы.
    pub const COUNT: usize = 10;
    /// Все проходы в порядке выполнения.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Clusters,
        Self::ShadowDirectional,
        Self::ShadowLocal,
        Self::Opaque,
        Self::Skybox,
        Self::Transparent,
        Self::Wireframe,
        Self::Particles,
        Self::TemporalResolve,
        Self::Tonemap,
    ];

    /// Короткое имя для отчётов.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clusters => "Clusters",
            Self::ShadowDirectional => "Shadow directional",
            Self::ShadowLocal => "Shadow local",
            Self::Opaque => "Opaque",
            Self::Skybox => "Skybox",
            Self::Transparent => "Transparent",
            Self::Wireframe => "Wireframe",
            Self::Particles => "Particles",
            Self::TemporalResolve => "Temporal resolve",
            Self::Tonemap => "Tonemap",
        }
    }
}

/// Вид, которому принадлежат GPU-границы: основной экран или viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfiledView {
    Screen,
    Viewport(ViewportId),
}

/// CPU-фазы одного вызова `render_frame*`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderCpuTimings {
    /// От начала вызова до ожидания fence: проверки, подготовка кадра и окружения.
    pub prepare: Duration,
    pub fence_wait: Duration,
    /// Опрос и отправка загрузок ресурсов.
    pub uploads: Duration,
    pub acquire: Duration,
    /// Запись command buffer.
    pub record: Duration,
    /// Сброс fence и `vkQueueSubmit2`.
    pub submit: Duration,
    /// `vkQueuePresentKHR`.
    pub present: Duration,
}

/// GPU-время одного вида. `passes[pass as usize]` — `None`, если проход не выполнялся в кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuViewTimings {
    pub key: ProfiledView,
    pub passes: [Option<Duration>; GpuPass::COUNT],
    /// От начала вида до последней записанной границы. Сумма `passes` совпадает с ним
    /// с округлением до наносекунды на каждый проход.
    pub total: Duration,
}

/// GPU-время кадра: виды в порядке записи и вывод на swapchain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuFrameTimings {
    pub views: [Option<GpuViewTimings>; MAX_PROFILED_VIEWS],
    /// Вывод на swapchain (Output и UI). В `total` видов не входит.
    pub output_ui: Option<Duration>,
}

impl GpuFrameTimings {
    /// Total первого записанного вида: основного экрана или первого viewport.
    fn first_view_total(&self) -> Option<Duration> {
        self.views[0].map(|view| view.total)
    }
}

/// Профиль последнего вызова `render_frame*`. GPU-часть прочитана в начале вызова
/// из прошлой отправки того же слота, поэтому относится к более раннему кадру.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameProfile {
    pub cpu: RenderCpuTimings,
    pub gpu: Option<GpuFrameTimings>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcquireAction {
    Acquired { image_index: u32, suboptimal: bool },
    OutOfDate,
    Skipped,
    Failed(vk::Result),
}

enum AcquireImageError {
    BeforeAcquire(RenderError),
    AfterAcquire(RenderError),
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
    overdraw_pipeline: Option<vk::Pipeline>,
    wireframe_pipeline: Option<vk::Pipeline>,
    skybox_pipeline: Option<vk::Pipeline>,
    shadow_pipeline: Option<vk::Pipeline>,
    local_shadow_pipeline: Option<vk::Pipeline>,
    cluster_pipeline: Option<vk::Pipeline>,
    cluster_pipeline_layout: Option<vk::PipelineLayout>,
    post_pipeline_layout: Option<vk::PipelineLayout>,
    taa_set_layout: Option<vk::DescriptorSetLayout>,
    taa_pipeline_layout: Option<vk::PipelineLayout>,
    taa_pipeline: Option<vk::Pipeline>,
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
    /// Профиль последнего вызова render; забирается через `take_frame_profile`.
    pending_frame_profile: Option<FrameProfile>,
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
    screen_temporal: TemporalView,
    retired: RetireQueue<RetiredResource>,
    retired_swapchains: RetiredSwapchains<SwapchainBundle>,
    pending_swapchain_drop: Option<SwapchainBundle>,
    frames: Vec<FrameResources>,
    particles: ParticleSystem,
    frame_slot: usize,
    submission_generation: u64,
    swapchain_generation: u64,
    #[cfg(windows)]
    fsr_api: Option<Arc<fsr::FfxApi>>,
    #[cfg(windows)]
    fsr_load_error: Option<String>,
    #[cfg(all(windows, feature = "dlss"))]
    dlss_api: Option<dlss::NgxApi>,
    dlss_load_error: Option<String>,
    fsr_warning_emitted: bool,
    dlss_warning_emitted: bool,
    swapchain: Option<SwapchainBundle>,
    surface_format: vk::SurfaceFormatKHR,
    present_mode: PresentMode,
    last_warned_present_mode: Option<PresentMode>,
    output_extent: vk::Extent2D,
    swapchain_dirty: bool,
    frame_slots_lost: bool,
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
            overdraw_pipeline: None,
            wireframe_pipeline: None,
            skybox_pipeline: None,
            shadow_pipeline: None,
            local_shadow_pipeline: None,
            cluster_pipeline: None,
            cluster_pipeline_layout: None,
            post_pipeline_layout: None,
            taa_set_layout: None,
            taa_pipeline_layout: None,
            taa_pipeline: None,
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
            pending_frame_profile: None,
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
            screen_temporal: TemporalView::default(),
            particles: ParticleSystem::default(),
            retired: RetireQueue::new(FRAMES_IN_FLIGHT),
            retired_swapchains: RetiredSwapchains::new(FRAMES_IN_FLIGHT),
            pending_swapchain_drop: None,
            frames: Vec::new(),
            frame_slot: 0,
            submission_generation: 0,
            swapchain_generation: 0,
            #[cfg(windows)]
            fsr_api: None,
            #[cfg(windows)]
            fsr_load_error: None,
            #[cfg(all(windows, feature = "dlss"))]
            dlss_api: None,
            dlss_load_error: None,
            fsr_warning_emitted: false,
            dlss_warning_emitted: false,
            swapchain: None,
            surface_format: vk::SurfaceFormatKHR::default(),
            present_mode: config.present_mode,
            last_warned_present_mode: None,
            output_extent: vk::Extent2D { width, height },
            swapchain_dirty: true,
            frame_slots_lost: false,
            config,
            capabilities: Capabilities {
                device_name: String::new(),
                vendor_id: 0,
                api_version: 0,
                discrete: false,
                tessellation_shader: false,
                wireframe: false,
                fsr: false,
                dlss: false,
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

    #[cfg(windows)]
    fn load_fsr_runtime(&mut self) {
        let Some(path) = self.config.upscalers.fsr_library.as_deref() else {
            self.fsr_load_error = Some("FSR loader DLL is not configured".to_owned());
            return;
        };
        let effect_path = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("amd_fidelityfx_upscaler_vk.dll");
        if !effect_path.is_file() {
            self.fsr_load_error = Some(format!(
                "FSR upscaler effect DLL is missing: {}",
                effect_path.display()
            ));
            return;
        }
        let Some(instance) = self.instance.as_ref() else {
            self.fsr_load_error = Some("Vulkan instance is unavailable".to_owned());
            return;
        };
        let exposure_usage = vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED;
        // SAFETY: the selected physical device belongs to the live Vulkan instance.
        let exposure_features = unsafe {
            instance
                .get_physical_device_format_properties(self.physical_device, vk::Format::R32_SFLOAT)
        }
        .optimal_tiling_features;
        if !exposure_features
            .contains(vk::FormatFeatureFlags::TRANSFER_DST | vk::FormatFeatureFlags::SAMPLED_IMAGE)
            || unsafe {
                instance.get_physical_device_image_format_properties(
                    self.physical_device,
                    vk::Format::R32_SFLOAT,
                    vk::ImageType::TYPE_2D,
                    vk::ImageTiling::OPTIMAL,
                    exposure_usage,
                    vk::ImageCreateFlags::empty(),
                )
            }
            .is_err()
        {
            self.fsr_load_error = Some(
                "R32_SFLOAT exposure image does not support sampled and transfer-destination usage"
                    .to_owned(),
            );
            return;
        }
        match fsr::FfxApi::load(path) {
            Ok(api) => {
                self.fsr_api = Some(api);
                self.capabilities.fsr = true;
            }
            Err(error) => self.fsr_load_error = Some(error),
        }
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

    /// Забирает GPU-время последнего завершённого кадра: total первого записанного вида
    /// (основного экрана или первого viewport).
    ///
    /// Каждое измерение возвращается один раз; `None` — нового измерения нет
    /// или очередь не поддерживает timestamp queries. Вывод на swapchain в total не входит.
    /// Ожидание acquire может попасть в первый color-проход (обычно Opaque), а не в вывод:
    /// submit ждёт `image_available` на стадии `COLOR_ATTACHMENT_OUTPUT`. См. [`FrameProfile`].
    pub fn take_gpu_frame_time(&mut self) -> Option<Duration> {
        self.pending_gpu_time.take()
    }

    /// Забирает профиль последнего вызова `render_frame*`: CPU-фазы этого вызова и GPU-границы.
    ///
    /// GPU-часть относится к прошлой отправке того же слота и отстаёт от CPU-части
    /// на число кадров в полёте. Неизъятый профиль заменяется следующим вызовом.
    pub fn take_frame_profile(&mut self) -> Option<FrameProfile> {
        self.pending_frame_profile.take()
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
        self.retired
            .retire(RetiredResource::Viewport(Box::new(resource)), 0);
        Ok(())
    }

    /// Очищает offscreen-таргет, рисует меши и масштабирует результат в swapchain.
    pub fn render_frame(&mut self, frame: &Frame<'_>) -> Result<FrameStatus, RenderError> {
        self.render_frame_with_outcome(frame)
            .map(|outcome| outcome.status)
    }

    /// Renders a frame and reports if its command buffer was successfully submitted.
    pub fn render_frame_with_outcome(
        &mut self,
        frame: &Frame<'_>,
    ) -> Result<FrameOutcome, RenderError> {
        let prepare_started = Instant::now();
        self.begin_particle_frame();
        if self.frame_slots_lost {
            return Err(RenderError::InvalidState(
                "ресурсы синхронизации кадровых слотов потеряны",
            ));
        }
        validate_debug_view(frame.debug_view, self.capabilities.wireframe)?;
        validate_draws(frame.draws, &self.meshes, &self.materials)?;
        validate_overlay(frame.overlay)?;
        let view = effective_view_settings(frame.view, frame.debug_view);
        self.miss_unlisted_viewports(&[]);
        let output_extent = self
            .swapchain
            .as_ref()
            .map_or(self.output_extent, |swapchain| swapchain.extent);
        let render_extent = scaled_extent(output_extent, frame.view.render_scale);
        let temporal_plan = self.screen_temporal.state.plan(
            temporal_key(
                render_extent,
                output_extent,
                view.anti_aliasing,
                frame.debug_view.code(),
            ),
            multiply_4x4(frame.camera.projection, frame.camera.view),
            frame.reset_history,
        );
        let prepared = prepare_frame_with_temporal(frame, render_extent, Some(temporal_plan))?;
        let (environment_set, skybox) = self.environment_set_for(frame.environment)?;
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            self.screen_temporal.state.missed();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        }
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.screen_temporal.state.missed();
            self.recreate_swapchain()?;
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
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
        let prepare = prepare_started.elapsed();
        let fence_started = Instant::now();
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }?;
        let fence_wait = fence_started.elapsed();
        let uploads_started = Instant::now();
        self.poll_uploads()?;
        self.flush_uploads()?;
        let uploads = uploads_started.elapsed();
        let mut cpu = RenderCpuTimings {
            prepare,
            fence_wait,
            uploads,
            ..RenderCpuTimings::default()
        };
        self.retired.slot_completed(frame_slot);
        self.retired_swapchains
            .slot_completed(frame_slot, self.frames[frame_slot].submission_generation);
        if view.anti_aliasing.is_temporal() {
            ensure_temporal_history(
                &instance,
                &device,
                self.physical_device,
                &mut self.screen_temporal,
                output_extent,
                &mut self.retired,
            )?;
        }
        let submitted_at = Instant::now();
        #[cfg(windows)]
        let frame_time_delta_ms =
            temporal_frame_delta_ms(self.screen_temporal.last_submitted_at, submitted_at);
        #[cfg(windows)]
        let mut fsr_fallback_reason = None;
        if view.anti_aliasing == AntiAliasing::Fsr {
            #[cfg(windows)]
            if self.capabilities.fsr {
                let error = if let Some(api) = self.fsr_api.as_ref().cloned() {
                    ensure_fsr_view(
                        &instance,
                        &device,
                        self.physical_device,
                        &mut self.screen_temporal,
                        &api,
                        render_extent,
                        output_extent,
                        &mut self.retired,
                    )
                    .err()
                } else {
                    Some("FSR capability has no loaded API".to_owned())
                };
                if let Some(error) = error {
                    warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                    fsr_fallback_reason = Some(error);
                }
            } else {
                let error = self
                    .fsr_load_error
                    .clone()
                    .unwrap_or_else(|| "FSR runtime is unavailable".to_owned());
                warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                fsr_fallback_reason = Some(error);
            }
            #[cfg(not(windows))]
            {
                let error = "FSR is only available on Windows".to_owned();
                warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
            }
        } else if view.anti_aliasing == AntiAliasing::Dlss {
            #[cfg(all(windows, feature = "dlss"))]
            let error = if self.capabilities.dlss {
                ensure_dlss_view(
                    &instance,
                    &device,
                    self.physical_device,
                    &mut self.screen_temporal,
                    render_extent,
                    output_extent,
                    &mut self.retired,
                )
                .err()
            } else {
                Some(
                    self.dlss_load_error
                        .clone()
                        .unwrap_or_else(|| "DLSS runtime is unavailable".to_owned()),
                )
            };
            #[cfg(not(all(windows, feature = "dlss")))]
            let error = Some(
                self.dlss_load_error
                    .clone()
                    .unwrap_or_else(|| "DLSS support is unavailable".to_owned()),
            );
            if let Some(error) = error {
                warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                #[cfg(windows)]
                {
                    fsr_fallback_reason = Some(error);
                }
            }
        }
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let swapchain_handle = swapchain.handle;
        let swapchain_image_count = swapchain.images.len();
        let queue = self.queue;
        let taa_set_layout = self.taa_set_layout()?;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let image_available = frame_resources.image_available;
        let command_buffer = frame_resources.command_buffer;

        let gpu = frame_resources.read_gpu_timings(
            &device,
            self.timestamp_valid_bits,
            self.timestamp_period,
        )?;
        if let Some(total) = gpu.and_then(|gpu| gpu.first_view_total()) {
            self.pending_gpu_time = Some(total);
        }
        // Fence слота дождались, поэтому GPU больше не читает вершины оверлея этого слота.
        frame_resources.write_overlay(&instance, &device, self.physical_device, frame.overlay)?;
        let timestamps = frame_resources.timestamps;
        let mut gpu_layout = GpuProfileLayout::default();
        let screen_marks = timestamps.and_then(|pool| {
            gpu_layout
                .add_view(ProfiledView::Screen)
                .map(|first| GpuMarks { pool, first })
        });
        let output_marks = timestamps.map(|pool| GpuMarks {
            pool,
            first: OUTPUT_QUERY_FIRST,
        });
        frame_resources.gpu_layout = gpu_layout;
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
        let shadow_pipeline = self
            .shadow_pipeline
            .ok_or(RenderError::InvalidState("shadow pipeline отсутствует"))?;
        let local_shadow_pipeline = self.local_shadow_pipeline.ok_or(RenderError::InvalidState(
            "local shadow pipeline отсутствует",
        ))?;
        let pipeline_layout = self
            .pipeline_layout
            .ok_or(RenderError::InvalidState("pipeline layout отсутствует"))?;
        let scene_pipeline = self
            .pipeline
            .ok_or(RenderError::InvalidState("pipeline отсутствует"))?;
        let mask_pipeline = self
            .mask_pipeline
            .ok_or(RenderError::InvalidState("mask pipeline отсутствует"))?;
        let blend_pipeline = self
            .blend_pipeline
            .ok_or(RenderError::InvalidState("blend pipeline отсутствует"))?;
        let overdraw_pipeline = self
            .overdraw_pipeline
            .ok_or(RenderError::InvalidState("overdraw pipeline отсутствует"))?;
        let cluster_pipeline = self
            .cluster_pipeline
            .ok_or(RenderError::InvalidState("cluster pipeline отсутствует"))?;
        let cluster_pipeline_layout =
            self.cluster_pipeline_layout
                .ok_or(RenderError::InvalidState(
                    "cluster pipeline layout отсутствует",
                ))?;
        let skybox_pipeline = self
            .skybox_pipeline
            .ok_or(RenderError::InvalidState("skybox pipeline отсутствует"))?;
        let tonemap_pipeline = self
            .tonemap_pipeline
            .ok_or(RenderError::InvalidState("tonemap pipeline отсутствует"))?;
        let output_pipeline = self
            .output_pipeline
            .ok_or(RenderError::InvalidState("output pipeline отсутствует"))?;
        let post_pipeline_layout = self.post_pipeline_layout.ok_or(RenderError::InvalidState(
            "post pipeline layout отсутствует",
        ))?;
        let taa_pipeline = self
            .taa_pipeline
            .ok_or(RenderError::InvalidState("TAA pipeline отсутствует"))?;
        let taa_pipeline_layout = self
            .taa_pipeline_layout
            .ok_or(RenderError::InvalidState("TAA pipeline layout отсутствует"))?;
        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let local_shadow_image = self
            .local_shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?;
        if view.anti_aliasing.is_temporal() && self.screen_temporal.history.is_none() {
            return Err(RenderError::InvalidState("TAA history отсутствует"));
        }
        let overlay_resources =
            if frame.overlay.is_empty() || frame_resources.overlay_buffer.buffer.is_none() {
                None
            } else {
                Some((
                    self.overlay_pipeline
                        .ok_or(RenderError::InvalidState("overlay pipeline отсутствует"))?,
                    self.overlay_pipeline_layout
                        .ok_or(RenderError::InvalidState(
                            "overlay pipeline layout отсутствует",
                        ))?,
                ))
            };
        let target_changed = frame_resources.target.as_ref().is_none_or(|target| {
            target.render_extent != render_extent || target.output_extent != output_extent
        });
        if target_changed {
            let target = FrameTarget::new(
                &instance,
                &device,
                self.physical_device,
                render_extent,
                output_extent,
                taa_set_layout,
            )?;
            write_image_set(
                &device,
                frame_resources.output_set,
                target.ldr.view,
                post_sampler,
            );
            frame_resources.target = Some(target);
        }
        let target = frame_resources
            .target
            .as_ref()
            .ok_or(RenderError::InvalidState("offscreen target отсутствует"))?;
        let tonemap_source = if view.anti_aliasing.is_temporal() {
            let history = self
                .screen_temporal
                .history
                .as_ref()
                .ok_or(RenderError::InvalidState("TAA history отсутствует"))?;
            write_taa_image_set(
                &device,
                target.taa_set,
                post_sampler,
                [
                    target.scene.color.view,
                    target.scene.velocity.view,
                    target.scene.depth.view,
                    history.images[temporal_plan.read_history].view,
                ],
            );
            history.images[temporal_plan.write_history].view
        } else {
            target.scene.color.view
        };
        write_image_set(
            &device,
            frame_resources.tonemap_set,
            tonemap_source,
            post_sampler,
        );
        let acquire_started = Instant::now();
        let acquired = match acquire_image(
            &swapchain_loader,
            swapchain_handle,
            image_available,
            swapchain_image_count,
            &mut self.swapchain_dirty,
        ) {
            Ok(acquired) => {
                cpu.acquire = acquire_started.elapsed();
                acquired
            }
            Err(AcquireImageError::BeforeAcquire(error)) => return Err(error),
            Err(AcquireImageError::AfterAcquire(error)) => {
                self.screen_temporal.state.missed();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };
        let Some((image_index, acquire_suboptimal)) = acquired else {
            self.screen_temporal.state.missed();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        };

        let swapchain_image = swapchain.images[image_index];
        let swapchain_view = swapchain.views[image_index];
        let swapchain_extent = swapchain.extent;
        let render_finished = swapchain.render_finished[image_index];
        let output_is_srgb = is_srgb_format(self.surface_format.format);
        let view_projection = multiply_4x4(frame.camera.projection, frame.camera.view);
        let particle_works = self.particles.plan_view(
            frame.particles,
            frame.particle_step,
            view_projection,
            frame.debug_view,
            frame.camera.position,
        )?;
        let particle_pass = self.particle_pipelines().map(|pipelines| ParticlePass {
            works: &particle_works,
            pipelines,
            frame_set,
        });
        let record_started = Instant::now();
        let recorded = (|| {
            let shadow = ShadowPass {
                pipeline: shadow_pipeline,
                local_pipeline: local_shadow_pipeline,
                pipeline_layout,
                frame_set,
                meshes: &self.meshes,
                materials: &self.materials,
                draws: frame.draws,
                shadow_cascades: frame.shadow_cascades,
                local_shadow_matrices: &prepared.local_shadow_matrices,
            };
            let scene = ScenePass {
                pipeline: scene_pipeline,
                mask_pipeline,
                blend_pipeline,
                overdraw_pipeline,
                wireframe_pipeline: self.wireframe_pipeline,
                pipeline_layout,
                frame_set,
                cluster_pipeline,
                cluster_pipeline_layout,
                environment_set,
                skybox_pipeline,
                skybox,
                meshes: &self.meshes,
                materials: &self.materials,
                draws: frame.draws,
                view_projection: multiply_4x4(frame.camera.projection, frame.camera.view),
                #[cfg(windows)]
                fsr_camera: fsr_camera_parameters(frame.camera.projection),
                culling_padding: [
                    temporal_plan.jitter_ndc[0].abs(),
                    temporal_plan.jitter_ndc[1].abs(),
                ],
                temporal_plan,
                camera_position: frame.camera.position,
                clear_color: frame.clear_color,
                debug_view: frame.debug_view,
                timestamps: screen_marks,
                view,
                particles: particle_pass,
            };
            let frame_resources = &mut self.frames[frame_slot];
            let overlay = match (&frame_resources.overlay_buffer.buffer, overlay_resources) {
                (Some(buffer), Some((pipeline, pipeline_layout))) => Some(OverlayPass {
                    pipeline,
                    pipeline_layout,
                    vertex_buffer: buffer.buffer,
                    // validate_overlay проверил, что число вершин помещается в u32.
                    vertex_count: frame.overlay.len() as u32,
                }),
                _ => None,
            };
            let target = frame_resources
                .target
                .as_mut()
                .expect("frame target was created before acquire");
            let mut shadow_layout = self.shadow_layout;
            let mut local_shadow_layout = self.local_shadow_layout;
            #[cfg(windows)]
            let temporal_backend = select_temporal_backend(
                view.anti_aliasing,
                self.capabilities.fsr,
                self.capabilities.dlss,
            );
            let temporal_record = if view.anti_aliasing.is_temporal() {
                #[cfg(all(windows, feature = "dlss"))]
                let dlss_api = self.dlss_api.as_ref();
                let temporal = &mut self.screen_temporal;
                let history = temporal
                    .history
                    .as_ref()
                    .expect("temporal history was validated before acquire");
                #[cfg(windows)]
                let fsr = (temporal_backend == TemporalBackend::Fsr
                    && fsr_fallback_reason.is_none())
                .then_some(temporal.fsr.as_deref_mut())
                .flatten();
                #[cfg(all(windows, feature = "dlss"))]
                let dlss = (temporal_backend == TemporalBackend::Dlss
                    && fsr_fallback_reason.is_none())
                .then_some(temporal.dlss.as_deref_mut())
                .flatten();
                Some(TemporalResolveRecord {
                    history,
                    #[cfg(windows)]
                    backend: temporal_backend,
                    plan: temporal_plan,
                    taa_pipeline,
                    taa_pipeline_layout,
                    #[cfg(windows)]
                    fsr,
                    #[cfg(windows)]
                    exposure_ev: view.exposure_ev,
                    #[cfg(windows)]
                    frame_time_delta_ms,
                    #[cfg(windows)]
                    camera: fsr_camera_parameters(frame.camera.projection),
                    #[cfg(all(windows, feature = "dlss"))]
                    dlss_api,
                    #[cfg(all(windows, feature = "dlss"))]
                    dlss,
                })
            } else {
                None
            };
            let temporal_outcome = record_frame(
                &device,
                command_buffer,
                target,
                shadow_image,
                &mut shadow_layout,
                local_shadow_image,
                &mut local_shadow_layout,
                swapchain_image,
                swapchain_view,
                swapchain_extent,
                view,
                frame_resources.tonemap_set,
                post_sampler,
                frame_resources.output_set,
                tonemap_pipeline,
                output_pipeline,
                post_pipeline_layout,
                output_is_srgb,
                &shadow,
                &scene,
                overlay.as_ref(),
                temporal_record,
                output_marks,
            )?;
            Ok((temporal_outcome, shadow_layout, local_shadow_layout))
        })();
        cpu.record = record_started.elapsed();
        let (temporal_outcome, shadow_layout, local_shadow_layout) = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                self.screen_temporal.state.missed();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };
        if let Some(error) = temporal_outcome
            .as_ref()
            .and_then(|outcome| outcome.fsr_error.as_deref())
        {
            warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, error);
            #[cfg(windows)]
            if let Some(fsr) = self.screen_temporal.fsr.take() {
                self.retired.retire(RetiredResource::FsrView(fsr), 0);
                self.screen_temporal.fsr_error = Some(error.to_owned());
                self.screen_temporal.fsr_attempted = true;
            }
        }
        #[cfg(all(windows, feature = "dlss"))]
        if let Some(error) = temporal_outcome
            .as_ref()
            .and_then(|outcome| outcome.dlss_error.as_deref())
        {
            warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, error);
            self.capabilities.dlss = false;
            self.dlss_load_error = Some(error.to_owned());
            #[cfg(all(windows, feature = "dlss"))]
            if let Some(dlss) = self.screen_temporal.dlss.take() {
                self.retired.retire(RetiredResource::DlssView(dlss), 0);
                self.screen_temporal.dlss_error = Some(error.to_owned());
                self.screen_temporal.dlss_attempted = true;
            }
        }
        let temporal_submissions = [TemporalSubmission {
            view: TemporalViewId::Screen,
            plan: temporal_plan,
            layouts: view
                .anti_aliasing
                .is_temporal()
                .then_some([vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL; 2])
                .filter(|_| {
                    temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_taa)
                }),
            #[cfg(windows)]
            fsr_used: temporal_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.used_fsr),
            #[cfg(all(windows, feature = "dlss"))]
            dlss_used: temporal_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.used_dlss),
            submitted_at,
        }];
        let submitted = self.submit_and_present(AcquiredFrameSubmission {
            device: &device,
            swapchain_loader: &swapchain_loader,
            queue,
            frame_slot,
            fence,
            image_available,
            command_buffer,
            swapchain_handle,
            render_finished,
            image_index,
            acquire_suboptimal,
            wait_stage: vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            timestamps_written: timestamps.is_some(),
            timings: &mut cpu,
            temporal_submissions: &temporal_submissions,
            shadow_layout,
            local_shadow_layout,
        });
        let particles_submitted = match &submitted {
            Ok(outcome) => outcome.submitted,
            Err(SubmitAndPresentError::BeforeSubmit(_)) => false,
            Err(SubmitAndPresentError::AfterSubmit(_)) => true,
        };
        self.commit_particle_frame(particles_submitted);
        self.pending_frame_profile = Some(FrameProfile { cpu, gpu });
        match submitted {
            Ok(outcome) => Ok(outcome),
            Err(SubmitAndPresentError::BeforeSubmit(error)) => {
                self.screen_temporal.state.missed();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                Err(error)
            }
            Err(SubmitAndPresentError::AfterSubmit(error)) => Err(error),
        }
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
        self.render_frame_with_ui_outcome(scenes, ui, clear)
            .map(|outcome| outcome.status)
    }

    /// Renders viewport scenes and UI and reports if their command buffer was submitted.
    pub fn render_frame_with_ui_outcome(
        &mut self,
        scenes: &[(ViewportId, &Frame<'_>)],
        ui: &[UiPrimitive<'_>],
        clear: [f32; 4],
    ) -> Result<FrameOutcome, RenderError> {
        let prepare_started = Instant::now();
        self.begin_particle_frame();
        if self.frame_slots_lost {
            return Err(RenderError::InvalidState(
                "ресурсы синхронизации кадровых слотов потеряны",
            ));
        }
        validate_scene_viewports(scenes.iter().map(|(viewport, _)| *viewport))?;
        self.screen_temporal.state.missed();
        self.miss_unlisted_viewports(scenes);
        let mut prepared_scenes = Vec::with_capacity(scenes.len());
        let mut scene_environment_sets = Vec::with_capacity(scenes.len());
        for (viewport, frame) in scenes {
            validate_debug_view(frame.debug_view, self.capabilities.wireframe)?;
            validate_draws(frame.draws, &self.meshes, &self.materials)?;
            self.viewports
                .get(viewport.0)
                .and_then(Option::as_ref)
                .ok_or_else(unknown_viewport)?;
            let extent = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .extent;
            let render_extent = scaled_extent(extent, frame.view.render_scale);
            let view = effective_view_settings(frame.view, frame.debug_view);
            let plan = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .temporal
                .state
                .plan(
                    temporal_key(
                        render_extent,
                        extent,
                        view.anti_aliasing,
                        frame.debug_view.code(),
                    ),
                    multiply_4x4(frame.camera.projection, frame.camera.view),
                    frame.reset_history,
                );
            prepared_scenes.push((
                prepare_frame_with_temporal(frame, render_extent, Some(plan))?,
                plan,
                None::<String>,
                0.0_f32,
            ));
            scene_environment_sets.push(self.environment_set_for(frame.environment)?);
        }
        let packed = pack_ui(ui, self.output_extent)?;
        self.validate_ui_images(&packed, scenes)?;
        if self.output_extent.width == 0 || self.output_extent.height == 0 {
            self.miss_all_temporal_views();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        }
        if self.swapchain_dirty || self.swapchain.is_none() {
            self.miss_all_temporal_views();
            self.recreate_swapchain()?;
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
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
        let prepare = prepare_started.elapsed();
        let fence_started = Instant::now();
        unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }?;
        let fence_wait = fence_started.elapsed();
        let uploads_started = Instant::now();
        self.poll_uploads()?;
        self.flush_uploads()?;
        let uploads = uploads_started.elapsed();
        let mut cpu = RenderCpuTimings {
            prepare,
            fence_wait,
            uploads,
            ..RenderCpuTimings::default()
        };
        self.retired.slot_completed(frame_slot);
        self.retired_swapchains
            .slot_completed(frame_slot, self.frames[frame_slot].submission_generation);
        let submitted_at = Instant::now();
        for ((viewport, frame), (_, _, _fallback_reason, _frame_delta_ms)) in
            scenes.iter().zip(&mut prepared_scenes)
        {
            let extent = self.viewports[viewport.0]
                .as_ref()
                .ok_or_else(unknown_viewport)?
                .extent;
            if extent.width > 0 && extent.height > 0 {
                let temporal = &mut self.viewports[viewport.0]
                    .as_mut()
                    .ok_or_else(unknown_viewport)?
                    .temporal;
                *_frame_delta_ms =
                    temporal_frame_delta_ms(temporal.last_submitted_at, submitted_at);
            }
            let view = effective_view_settings(frame.view, frame.debug_view);
            if extent.width > 0 && extent.height > 0 && view.anti_aliasing.is_temporal() {
                let entry = self.viewports[viewport.0]
                    .as_mut()
                    .ok_or_else(unknown_viewport)?;
                ensure_temporal_history(
                    &instance,
                    &device,
                    self.physical_device,
                    &mut entry.temporal,
                    extent,
                    &mut self.retired,
                )?;
            }
            if extent.width > 0 && extent.height > 0 && view.anti_aliasing == AntiAliasing::Fsr {
                #[cfg(windows)]
                if self.capabilities.fsr {
                    let Some(api) = self.fsr_api.as_ref().cloned() else {
                        let error = "FSR capability has no loaded API".to_owned();
                        warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                        *_fallback_reason = Some(error);
                        continue;
                    };
                    let entry = self.viewports[viewport.0]
                        .as_mut()
                        .ok_or_else(unknown_viewport)?;
                    if let Err(error) = ensure_fsr_view(
                        &instance,
                        &device,
                        self.physical_device,
                        &mut entry.temporal,
                        &api,
                        scaled_extent(extent, frame.view.render_scale),
                        extent,
                        &mut self.retired,
                    ) {
                        warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                        *_fallback_reason = Some(error);
                    }
                } else {
                    let error = self
                        .fsr_load_error
                        .clone()
                        .unwrap_or_else(|| "FSR runtime is unavailable".to_owned());
                    warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                    *_fallback_reason = Some(error);
                }
                #[cfg(not(windows))]
                {
                    let error = "FSR is only available on Windows".to_owned();
                    warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                }
            } else if extent.width > 0
                && extent.height > 0
                && view.anti_aliasing == AntiAliasing::Dlss
            {
                #[cfg(all(windows, feature = "dlss"))]
                if self.capabilities.dlss {
                    let entry = self.viewports[viewport.0]
                        .as_mut()
                        .ok_or_else(unknown_viewport)?;
                    if let Err(error) = ensure_dlss_view(
                        &instance,
                        &device,
                        self.physical_device,
                        &mut entry.temporal,
                        scaled_extent(extent, frame.view.render_scale),
                        extent,
                        &mut self.retired,
                    ) {
                        warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                        *_fallback_reason = Some(error);
                    }
                } else {
                    let error = self
                        .dlss_load_error
                        .clone()
                        .unwrap_or_else(|| "DLSS runtime is unavailable".to_owned());
                    warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                    *_fallback_reason = Some(error);
                }
                #[cfg(not(all(windows, feature = "dlss")))]
                {
                    let error = self
                        .dlss_load_error
                        .clone()
                        .unwrap_or_else(|| "DLSS support is unavailable".to_owned());
                    warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, &error);
                    #[cfg(windows)]
                    {
                        *_fallback_reason = Some(error);
                    }
                }
            }
        }
        let swapchain = self.swapchain.as_ref().ok_or(RenderError::InvalidState(
            "swapchain отсутствует после создания",
        ))?;
        let swapchain_handle = swapchain.handle;
        let swapchain_image_count = swapchain.images.len();
        let queue = self.queue;
        let image_set_layout = self.texture_set_layout()?;
        let taa_set_layout = self.taa_set_layout()?;
        let frame_set_layout = self.frame_set_layout()?;
        let frame_resources = self
            .frames
            .get_mut(frame_slot)
            .ok_or(RenderError::InvalidState("нет ресурсов текущего кадра"))?;
        let command_buffer = frame_resources.command_buffer;
        let gpu = frame_resources.read_gpu_timings(
            &device,
            self.timestamp_valid_bits,
            self.timestamp_period,
        )?;
        if let Some(total) = gpu.and_then(|gpu| gpu.first_view_total()) {
            self.pending_gpu_time = Some(total);
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
            image_set_layout,
            taa_set_layout,
            image_sampler: self
                .ui_sampler
                .ok_or(RenderError::InvalidState("UI sampler отсутствует"))?,
            post_sampler: self
                .post_sampler
                .ok_or(RenderError::InvalidState("post sampler отсутствует"))?,
            frame_set_layout,
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
        let overdraw_pipeline = self
            .overdraw_pipeline
            .ok_or(RenderError::InvalidState("overdraw pipeline отсутствует"))?;
        let wireframe_pipeline = self.wireframe_pipeline;
        let cluster_pipeline = self
            .cluster_pipeline
            .ok_or(RenderError::InvalidState("cluster pipeline отсутствует"))?;
        let cluster_pipeline_layout =
            self.cluster_pipeline_layout
                .ok_or(RenderError::InvalidState(
                    "cluster pipeline layout отсутствует",
                ))?;
        let skybox_pipeline = self
            .skybox_pipeline
            .ok_or(RenderError::InvalidState("skybox pipeline отсутствует"))?;
        let ui_pipeline = self
            .ui_pipeline
            .ok_or(RenderError::InvalidState("UI pipeline отсутствует"))?;
        let ui_pipeline_layout = self
            .ui_pipeline_layout
            .ok_or(RenderError::InvalidState("UI pipeline layout отсутствует"))?;
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
        let taa_pipeline = self
            .taa_pipeline
            .ok_or(RenderError::InvalidState("TAA pipeline отсутствует"))?;
        let taa_pipeline_layout = self
            .taa_pipeline_layout
            .ok_or(RenderError::InvalidState("TAA pipeline layout отсутствует"))?;
        let shadow_image = self
            .shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("shadow map отсутствует"))?;
        let local_shadow_image = self
            .local_shadow_image
            .as_ref()
            .ok_or(RenderError::InvalidState("local shadow map отсутствует"))?;
        for (viewport, frame) in scenes {
            let extent = self.viewports[viewport.0]
                .as_ref()
                .expect("viewports were validated before acquire")
                .extent;
            if extent.width > 0
                && extent.height > 0
                && effective_view_settings(frame.view, frame.debug_view)
                    .anti_aliasing
                    .is_temporal()
                && self.viewports[viewport.0]
                    .as_ref()
                    .and_then(|entry| entry.temporal.history.as_ref())
                    .is_none()
            {
                return Err(RenderError::InvalidState("TAA history отсутствует"));
            }
        }
        let slot_timestamps = frame_resources.timestamps;
        let output_marks = slot_timestamps.map(|pool| GpuMarks {
            pool,
            first: OUTPUT_QUERY_FIRST,
        });
        let framebuffer_is_srgb = is_srgb_format(self.surface_format.format);
        let physical_device = self.physical_device;
        let mut rendered = Vec::with_capacity(scenes.len());
        for ((scene, prepared_scene), environment) in scenes
            .iter()
            .zip(&prepared_scenes)
            .zip(&scene_environment_sets)
        {
            let (viewport, frame) = scene;
            let (prepared, plan, fallback_reason, frame_delta_ms) = prepared_scene;
            let (environment_set, skybox) = environment;
            let entry = self.viewports[viewport.0]
                .as_mut()
                .ok_or_else(unknown_viewport)?;
            let extent = entry.extent;
            if extent.width == 0 || extent.height == 0 {
                entry.temporal.state.missed();
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
            let tonemap_source = if effective_view_settings(frame.view, frame.debug_view)
                .anti_aliasing
                .is_temporal()
            {
                let history = entry
                    .temporal
                    .history
                    .as_ref()
                    .ok_or(RenderError::InvalidState("TAA history отсутствует"))?;
                write_taa_image_set(
                    &device,
                    target.taa_set,
                    target_resources.post_sampler,
                    [
                        target.scene.color.view,
                        target.scene.velocity.view,
                        target.scene.depth.view,
                        history.images[plan.read_history].view,
                    ],
                );
                history.images[plan.write_history].view
            } else {
                target.scene.color.view
            };
            write_image_set(
                &device,
                target.tonemap_set,
                tonemap_source,
                target_resources.post_sampler,
            );
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
                *plan,
                frame_set,
                *environment_set,
                skybox_pipeline,
                *skybox,
                target.tonemap_set,
                target.output_set,
                prepared.local_shadow_matrices.as_slice(),
                *frame_delta_ms,
                fallback_reason.clone(),
            ));
        }

        let mut draws = Vec::with_capacity(packed.draws.len());
        for draw in &packed.draws {
            let (set, image_is_linear) = match draw.image {
                UiImage::Texture(texture) => (self.ui_texture(texture)?.set, false),
                UiImage::Viewport(viewport) => {
                    if !rendered
                        .iter()
                        .any(|(drawn, _, _, _, _, _, _, _, _, _, _, _)| *drawn == viewport)
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

        let acquire_started = Instant::now();
        let acquired = match acquire_image(
            &swapchain_loader,
            swapchain_handle,
            image_available,
            swapchain_image_count,
            &mut self.swapchain_dirty,
        ) {
            Ok(acquired) => {
                cpu.acquire = acquire_started.elapsed();
                acquired
            }
            Err(AcquireImageError::BeforeAcquire(error)) => return Err(error),
            Err(AcquireImageError::AfterAcquire(error)) => {
                self.miss_all_temporal_views();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };
        let Some((image_index, acquire_suboptimal)) = acquired else {
            self.miss_all_temporal_views();
            return Ok(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            });
        };
        let swapchain_image = swapchain.images[image_index];
        let swapchain_view = swapchain.views[image_index];
        let swapchain_extent = swapchain.extent;
        let render_finished = swapchain.render_finished[image_index];

        let particle_pipelines = self.particle_pipelines();
        let mut particle_works = Vec::with_capacity(rendered.len());
        for entry in &rendered {
            particle_works.push(self.particles.plan_view(
                entry.1.particles,
                entry.1.particle_step,
                multiply_4x4(entry.1.camera.projection, entry.1.camera.view),
                entry.1.debug_view,
                entry.1.camera.position,
            )?);
        }
        let mut gpu_layout = GpuProfileLayout::default();
        let scene_passes = rendered
            .iter()
            .enumerate()
            .map(
                |(
                    index,
                    (
                        viewport,
                        frame,
                        plan,
                        frame_set,
                        environment_set,
                        skybox_pipeline,
                        skybox,
                        tonemap_set,
                        output_set,
                        local_shadow_matrices,
                        frame_delta_ms,
                        upscaler_fallback_reason,
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
                            overdraw_pipeline,
                            wireframe_pipeline,
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
                            #[cfg(windows)]
                            fsr_camera: fsr_camera_parameters(frame.camera.projection),
                            culling_padding: [plan.jitter_ndc[0].abs(), plan.jitter_ndc[1].abs()],
                            temporal_plan: *plan,
                            camera_position: frame.camera.position,
                            clear_color: frame.clear_color,
                            debug_view: frame.debug_view,
                            timestamps: slot_timestamps.and_then(|pool| {
                                gpu_layout
                                    .add_view(ProfiledView::Viewport(*viewport))
                                    .map(|first| GpuMarks { pool, first })
                            }),
                            view: effective_view_settings(frame.view, frame.debug_view),
                            particles: particle_pipelines.map(|pipelines| ParticlePass {
                                works: &particle_works[index],
                                pipelines,
                                frame_set: *frame_set,
                            }),
                        },
                        *tonemap_set,
                        *output_set,
                        *frame_delta_ms,
                        upscaler_fallback_reason.clone(),
                    )
                },
            )
            .collect::<Vec<_>>();
        self.frames[frame_slot].gpu_layout = gpu_layout;
        let timestamps = slot_timestamps.is_some();
        let ui_pass = UiPass {
            pipeline: ui_pipeline,
            pipeline_layout: ui_pipeline_layout,
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
            framebuffer_is_srgb,
            draws: &draws,
            view: swapchain_view,
            clear,
        };

        let record_started = Instant::now();
        let recorded = (|| {
            begin_command_buffer(&device, command_buffer, slot_timestamps)?;
            let mut shadow_layout = self.shadow_layout;
            let mut local_shadow_layout = self.local_shadow_layout;
            let post_sampler = target_resources.post_sampler;
            let mut temporal_submissions = Vec::with_capacity(scene_passes.len());
            let mut fsr_dispatch_errors = Vec::new();
            #[cfg(all(windows, feature = "dlss"))]
            let mut dlss_dispatch_errors = Vec::new();
            for (
                viewport,
                shadow,
                pass,
                tonemap_set,
                output_set,
                _frame_delta_ms,
                _upscaler_fallback_reason,
            ) in &scene_passes
            {
                let Some(entry) = self.viewports[viewport.0].as_mut() else {
                    continue;
                };
                let Some(target) = entry.targets[frame_slot].as_mut() else {
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
                let temporal_outcome = if pass.view.anti_aliasing.is_temporal() {
                    let temporal = &mut entry.temporal;
                    let history = temporal
                        .history
                        .as_ref()
                        .expect("temporal history was validated before acquire");
                    #[cfg(windows)]
                    let backend = select_temporal_backend(
                        pass.view.anti_aliasing,
                        self.capabilities.fsr,
                        self.capabilities.dlss,
                    );
                    #[cfg(windows)]
                    let fsr = (backend == TemporalBackend::Fsr
                        && _upscaler_fallback_reason.is_none())
                    .then_some(temporal.fsr.as_deref_mut())
                    .flatten();
                    #[cfg(all(windows, feature = "dlss"))]
                    let dlss_api = self.dlss_api.as_ref();
                    #[cfg(all(windows, feature = "dlss"))]
                    let dlss = (backend == TemporalBackend::Dlss
                        && _upscaler_fallback_reason.is_none())
                    .then_some(temporal.dlss.as_deref_mut())
                    .flatten();
                    Some(record_temporal_resolve_and_tonemap(
                        &device,
                        command_buffer,
                        &mut target.scene,
                        &mut target.ldr,
                        &mut target.ldr_layout,
                        *tonemap_set,
                        post_sampler,
                        pass.frame_set,
                        target.extent,
                        pass.view,
                        pass.debug_view,
                        pass.timestamps,
                        tonemap_pipeline,
                        post_pipeline_layout,
                        target.taa_set,
                        TemporalResolveRecord {
                            history,
                            #[cfg(windows)]
                            backend,
                            plan: pass.temporal_plan,
                            taa_pipeline,
                            taa_pipeline_layout,
                            #[cfg(windows)]
                            fsr,
                            #[cfg(windows)]
                            exposure_ev: pass.view.exposure_ev,
                            #[cfg(windows)]
                            frame_time_delta_ms: *_frame_delta_ms,
                            #[cfg(windows)]
                            camera: pass.fsr_camera,
                            #[cfg(all(windows, feature = "dlss"))]
                            dlss_api,
                            #[cfg(all(windows, feature = "dlss"))]
                            dlss,
                        },
                    )?)
                } else {
                    write_image_set(&device, *tonemap_set, target.scene.color.view, post_sampler);
                    record_tonemap_pass(
                        &device,
                        command_buffer,
                        &target.scene.color,
                        target.scene.layout,
                        &target.ldr,
                        &mut target.ldr_layout,
                        target.render_extent,
                        *tonemap_set,
                        tonemap_pipeline,
                        post_pipeline_layout,
                        pass.view,
                        pass.debug_view,
                        pass.timestamps,
                    );
                    target.scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                    None
                };
                let output_source_extent = temporal_outcome
                    .as_ref()
                    .map_or(target.render_extent, |outcome| outcome.output_extent);
                record_output_pass(
                    &device,
                    command_buffer,
                    target.output.image,
                    target.output.view,
                    output_source_extent,
                    target.extent,
                    target.output_layout,
                    *output_set,
                    viewport_output_pipeline,
                    post_pipeline_layout,
                    pass.view.anti_aliasing,
                    true,
                    None,
                    true,
                );
                target.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                if let Some(error) = temporal_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.fsr_error.as_deref())
                {
                    fsr_dispatch_errors.push((*viewport, error.to_owned()));
                }
                #[cfg(all(windows, feature = "dlss"))]
                if let Some(error) = temporal_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.dlss_error.as_deref())
                {
                    dlss_dispatch_errors.push((*viewport, error.to_owned()));
                }
                temporal_submissions.push(TemporalSubmission {
                    view: TemporalViewId::Viewport(*viewport),
                    plan: pass.temporal_plan,
                    layouts: temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_taa)
                        .then_some([vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL; 2]),
                    #[cfg(windows)]
                    fsr_used: temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_fsr),
                    #[cfg(all(windows, feature = "dlss"))]
                    dlss_used: temporal_outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.used_dlss),
                    submitted_at,
                });
            }
            for (_viewport, error) in fsr_dispatch_errors {
                warn_temporal_fallback("FSR", &mut self.fsr_warning_emitted, &error);
                #[cfg(windows)]
                if let Some(Some(viewport)) = self.viewports.get_mut(_viewport.0) {
                    if let Some(fsr) = viewport.temporal.fsr.take() {
                        self.retired.retire(RetiredResource::FsrView(fsr), 0);
                    }
                    viewport.temporal.fsr_error = Some(error);
                    viewport.temporal.fsr_attempted = true;
                }
            }
            #[cfg(all(windows, feature = "dlss"))]
            if let Some((_, error)) = dlss_dispatch_errors.first() {
                warn_temporal_fallback("DLSS", &mut self.dlss_warning_emitted, error);
                self.capabilities.dlss = false;
                self.dlss_load_error = Some(error.clone());
                if let Some(dlss) = self.screen_temporal.dlss.take() {
                    self.retired.retire(RetiredResource::DlssView(dlss), 0);
                }
                for viewport in self.viewports.iter_mut().flatten() {
                    if let Some(dlss) = viewport.temporal.dlss.take() {
                        self.retired.retire(RetiredResource::DlssView(dlss), 0);
                    }
                    viewport.temporal.dlss_attempted = true;
                    viewport.temporal.dlss_error = Some(error.clone());
                }
            }
            if let Some(marks) = output_marks {
                marks.write(&device, command_buffer, 0);
            }
            record_ui(
                &device,
                command_buffer,
                swapchain_image,
                swapchain_extent,
                &ui_pass,
            );
            if let Some(marks) = output_marks {
                marks.write(&device, command_buffer, 1);
            }
            // SAFETY: command recording is complete before the command buffer is submitted.
            unsafe { device.end_command_buffer(command_buffer) }?;
            Ok((temporal_submissions, shadow_layout, local_shadow_layout))
        })();
        cpu.record = record_started.elapsed();
        let (temporal_submissions, shadow_layout, local_shadow_layout) = match recorded {
            Ok(recorded) => recorded,
            Err(error) => {
                self.miss_all_temporal_views();
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                return Err(error);
            }
        };

        let submitted = self.submit_and_present(AcquiredFrameSubmission {
            device: &device,
            swapchain_loader: &swapchain_loader,
            queue,
            frame_slot,
            fence,
            image_available,
            command_buffer,
            swapchain_handle,
            render_finished,
            image_index,
            acquire_suboptimal,
            wait_stage: vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            timestamps_written: timestamps,
            timings: &mut cpu,
            temporal_submissions: &temporal_submissions,
            shadow_layout,
            local_shadow_layout,
        });
        let particles_submitted = match &submitted {
            Ok(outcome) => outcome.submitted,
            Err(SubmitAndPresentError::BeforeSubmit(_)) => false,
            Err(SubmitAndPresentError::AfterSubmit(_)) => true,
        };
        self.commit_particle_frame(particles_submitted);
        self.pending_frame_profile = Some(FrameProfile { cpu, gpu });
        match submitted {
            Ok(outcome) => Ok(outcome),
            Err(SubmitAndPresentError::BeforeSubmit(error)) => {
                self.miss_temporal_submissions(&temporal_submissions);
                self.abandon_acquired_frame(frame_slot, &device, queue, image_available);
                Err(error)
            }
            Err(SubmitAndPresentError::AfterSubmit(error)) => Err(error),
        }
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

    fn miss_all_temporal_views(&mut self) {
        self.screen_temporal.state.missed();
        for viewport in self.viewports.iter_mut().flatten() {
            viewport.temporal.state.missed();
        }
    }

    fn miss_unlisted_viewports(&mut self, scenes: &[(ViewportId, &Frame<'_>)]) {
        for (index, viewport) in self.viewports.iter_mut().enumerate() {
            if let Some(viewport) = viewport
                && !scenes.iter().any(|(listed, _)| listed.0 == index)
            {
                viewport.temporal.state.missed();
            }
        }
    }

    fn commit_temporal_submissions(&mut self, submissions: &[TemporalSubmission]) {
        for submission in submissions {
            let temporal = match submission.view {
                TemporalViewId::Screen => &mut self.screen_temporal,
                TemporalViewId::Viewport(viewport) => {
                    let Some(Some(viewport)) = self.viewports.get_mut(viewport.0) else {
                        continue;
                    };
                    &mut viewport.temporal
                }
            };
            temporal.state.submitted(submission.plan);
            temporal.last_submitted_at = Some(submission.submitted_at);
            if let (Some(history), Some(layouts)) = (&mut temporal.history, submission.layouts) {
                history.layouts = layouts;
            }
        }
    }

    fn miss_temporal_submissions(&mut self, submissions: &[TemporalSubmission]) {
        for submission in submissions {
            let temporal = match submission.view {
                TemporalViewId::Screen => Some(&mut self.screen_temporal),
                TemporalViewId::Viewport(viewport) => self
                    .viewports
                    .get_mut(viewport.0)
                    .and_then(Option::as_mut)
                    .map(|viewport| &mut viewport.temporal),
            };
            if let Some(temporal) = temporal {
                temporal.state.missed();
                #[cfg(windows)]
                if submission.fsr_used {
                    if let Some(fsr) = temporal.fsr.take() {
                        self.retired.retire(RetiredResource::FsrView(fsr), 0);
                    }
                    temporal.fsr_attempted = false;
                    temporal.fsr_error = None;
                }
                #[cfg(all(windows, feature = "dlss"))]
                if submission.dlss_used {
                    if let Some(dlss) = temporal.dlss.take() {
                        self.retired.retire(RetiredResource::DlssView(dlss), 0);
                    }
                    temporal.dlss_attempted = false;
                    temporal.dlss_error = None;
                }
            }
        }
    }

    /// Отправляет записанный command buffer текущего слота и показывает изображение.
    ///
    /// `wait_stage` — стадия первого обращения к swapchain image в этом command buffer.
    fn submit_and_present(
        &mut self,
        acquired: AcquiredFrameSubmission<'_>,
    ) -> Result<FrameOutcome, SubmitAndPresentError> {
        let AcquiredFrameSubmission {
            device,
            swapchain_loader,
            queue,
            frame_slot,
            fence,
            image_available,
            command_buffer,
            swapchain_handle,
            render_finished,
            image_index,
            acquire_suboptimal,
            wait_stage,
            timestamps_written,
            timings,
            temporal_submissions,
            shadow_layout,
            local_shadow_layout,
        } = acquired;
        let submit_started = Instant::now();
        // Fence сбрасывается только после успешного acquire и полной записи command buffer.
        // SAFETY: эта точка — непосредственно перед submit, которому принадлежит fence.
        if let Err(error) = unsafe { device.reset_fences(&[fence]) } {
            return Err(SubmitAndPresentError::BeforeSubmit(RenderError::Vulkan(
                error,
            )));
        }

        let wait_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(image_available)
            .stage_mask(wait_stage)
            .device_index(0)];
        let command_infos = [vk::CommandBufferSubmitInfo::default()
            .command_buffer(command_buffer)
            .device_mask(0)];
        let signal_infos = [vk::SemaphoreSubmitInfo::default()
            .semaphore(render_finished)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .device_index(0)];
        let submit_info = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait_infos)
            .command_buffer_infos(&command_infos)
            .signal_semaphore_infos(&signal_infos)];
        // SAFETY: command buffer, semaphores and fence are owned by this renderer and are synchronized per frame/image.
        if let Err(error) = unsafe { device.queue_submit2(queue, &submit_info, fence) } {
            return Err(SubmitAndPresentError::BeforeSubmit(RenderError::Vulkan(
                error,
            )));
        }
        timings.submit = submit_started.elapsed();
        self.record_frame_submission(frame_slot);
        self.shadow_layout = shadow_layout;
        self.local_shadow_layout = local_shadow_layout;
        self.commit_temporal_submissions(temporal_submissions);
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
        let present_started = Instant::now();
        let present_result = unsafe { swapchain_loader.queue_present(queue, &present_info) };
        timings.present = present_started.elapsed();
        self.frame_slot = (self.frame_slot + 1) % self.frames.len();
        match present_result {
            Ok(present_suboptimal) => {
                self.retired_swapchains.presented(self.swapchain_generation);
                if acquire_suboptimal || present_suboptimal {
                    self.swapchain_dirty = true;
                }
                Ok(FrameOutcome {
                    status: FrameStatus::Presented,
                    submitted: true,
                })
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.swapchain_dirty = true;
                Ok(FrameOutcome {
                    status: FrameStatus::Skipped,
                    submitted: true,
                })
            }
            Err(error) => {
                self.swapchain_dirty = true;
                Err(SubmitAndPresentError::AfterSubmit(RenderError::Vulkan(
                    error,
                )))
            }
        }
    }

    fn abandon_acquired_frame(
        &mut self,
        frame_slot: usize,
        device: &ash::Device,
        queue: vk::Queue,
        image_available: vk::Semaphore,
    ) {
        self.swapchain_dirty = true;
        if let Some(frame) = self.frames.get_mut(frame_slot) {
            frame.target = None;
            for viewport in self.viewports.iter_mut().flatten() {
                if let Some(target) = viewport.targets.get_mut(frame_slot) {
                    target.take();
                }
            }
        } else {
            self.frame_slots_lost = true;
            return;
        }

        let fence = self.frames[frame_slot].fence;
        let reset = unsafe { device.reset_fences(&[fence]) };
        if reset.is_ok() {
            let wait_infos = [vk::SemaphoreSubmitInfo::default()
                .semaphore(image_available)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .device_index(0)];
            let submit_info = [vk::SubmitInfo2::default().wait_semaphore_infos(&wait_infos)];
            // SAFETY: the empty submit consumes the semaphore signaled by acquire and re-signals the slot fence.
            if unsafe { device.queue_submit2(queue, &submit_info, fence) }.is_ok() {
                self.record_frame_submission(frame_slot);
                self.frame_slot = (frame_slot + 1) % self.frames.len();
                return;
            }
        }

        // SAFETY: the old synchronization objects may have uncertain state after a failed recovery submit.
        let _ = unsafe { device.device_wait_idle() };
        let fence_info = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: this new fence starts signaled and is not referenced by any submission.
        let new_fence = match unsafe { device.create_fence(&fence_info, None) } {
            Ok(fence) => fence,
            Err(_) => {
                self.frame_slots_lost = true;
                return;
            }
        };
        // SAFETY: a default create info is valid for a new binary semaphore.
        let new_image_available =
            match unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) } {
                Ok(semaphore) => semaphore,
                Err(_) => {
                    // SAFETY: new_fence was created above and has not been submitted.
                    unsafe { device.destroy_fence(new_fence, None) };
                    self.frame_slots_lost = true;
                    return;
                }
            };

        let frame = &mut self.frames[frame_slot];
        let old_fence = std::mem::replace(&mut frame.fence, new_fence);
        let old_image_available =
            std::mem::replace(&mut frame.image_available, new_image_available);
        // SAFETY: device_wait_idle was attempted before replacing and destroying these objects.
        unsafe {
            device.destroy_fence(old_fence, None);
            device.destroy_semaphore(old_image_available, None);
        }
        self.frame_slot = (frame_slot + 1) % self.frames.len();
    }

    fn record_frame_submission(&mut self, frame_slot: usize) {
        self.submission_generation = self.submission_generation.saturating_add(1);
        self.frames[frame_slot].submission_generation = self.submission_generation;
    }

    fn initialize(&mut self) -> Result<(), RenderError> {
        #[cfg(all(windows, feature = "dlss"))]
        {
            if let Some(path) = self.config.upscalers.dlss_library.as_deref() {
                match dlss::NgxApi::new(
                    self.config.upscalers.dlss_project_id.as_deref(),
                    self.config.upscalers.dlss_application_id,
                    self.config.upscalers.dlss_engine_version.as_deref(),
                    path,
                ) {
                    Ok(api) => self.dlss_api = Some(api),
                    Err(error) => self.dlss_load_error = Some(error),
                }
            } else {
                self.dlss_load_error = Some("DLSS runtime DLL is not configured".to_owned());
            }
        }
        #[cfg(not(all(windows, feature = "dlss")))]
        {
            self.dlss_load_error = Some(if self.config.upscalers.dlss_library.is_some() {
                "DLSS support was not compiled into this target".to_owned()
            } else if cfg!(windows) {
                "DLSS runtime DLL is not configured".to_owned()
            } else {
                "DLSS is only available on Windows".to_owned()
            });
        }

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
        let available_instance_extensions =
            // SAFETY: querying loader extension properties does not require an instance.
            unsafe { self.entry.enumerate_instance_extension_properties(None) }?;
        #[cfg(all(windows, feature = "dlss"))]
        let mut dlss_instance_ready = false;
        #[cfg(all(windows, feature = "dlss"))]
        if let Some(api) = self.dlss_api.as_ref() {
            match api.instance_extensions() {
                Ok(extensions) => {
                    let missing = missing_extensions(
                        &extensions,
                        &available_instance_extensions,
                        "DLSS instance",
                    );
                    if missing.is_empty() {
                        requested_instance_extensions.extend(extensions);
                        dlss_instance_ready = true;
                    } else {
                        self.dlss_load_error = Some(missing.join(", "));
                    }
                }
                Err(error) => self.dlss_load_error = Some(error),
            }
        }
        let requested_instance_extensions = deduplicate_names(requested_instance_extensions);
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

        let base_device_extensions =
            required_device_extensions(&self.config.extra_device_extensions);
        let choice = choose_device(
            self.instance()?,
            self.surface_loader()?,
            self.surface()?,
            &base_device_extensions,
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
            wireframe: choice.wireframe,
            fsr: false,
            dlss: false,
        };

        #[allow(unused_mut)]
        let mut device_extensions = base_device_extensions;
        #[cfg(all(windows, feature = "dlss"))]
        let mut dlss_device_ready = false;
        #[cfg(all(windows, feature = "dlss"))]
        if dlss_instance_ready && let Some(api) = self.dlss_api.as_ref() {
            match api.check_support(self.instance()?.handle(), self.physical_device) {
                Ok(()) => match api
                    .device_extensions(self.instance()?.handle(), self.physical_device)
                {
                    Ok(extensions) => {
                        // SAFETY: the selected physical device belongs to the live instance.
                        let available = unsafe {
                            self.instance()?
                                .enumerate_device_extension_properties(self.physical_device)
                        }?;
                        let missing = missing_extensions(&extensions, &available, "DLSS device");
                        if missing.is_empty() {
                            device_extensions.extend(extensions);
                            dlss_device_ready = true;
                        } else {
                            self.dlss_load_error = Some(missing.join(", "));
                        }
                    }
                    Err(error) => self.dlss_load_error = Some(error),
                },
                Err(error) => self.dlss_load_error = Some(error),
            }
        }
        let device_extensions = deduplicate_names(device_extensions);

        let queue_priority = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(self.queue_family_index)
            .queue_priorities(&queue_priority)];
        let extension_pointers = device_extensions
            .iter()
            .map(|name| name.as_ptr())
            .collect::<Vec<_>>();
        let features = vk::PhysicalDeviceFeatures::default()
            .tessellation_shader(self.capabilities.tessellation_shader)
            .fill_mode_non_solid(self.capabilities.wireframe)
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
        #[cfg(all(windows, feature = "dlss"))]
        if dlss_device_ready {
            let instance_handle = self.instance()?.handle();
            let device_handle = self.device()?.handle();
            if let Some(api) = self.dlss_api.as_mut() {
                match api.initialize(instance_handle, self.physical_device, device_handle) {
                    Ok(()) => {
                        self.capabilities.dlss = true;
                        self.dlss_load_error = None;
                    }
                    Err(error) => self.dlss_load_error = Some(error),
                }
            }
        }
        self.queue =
            // SAFETY: queue family and index were checked during physical-device selection.
            unsafe { self.device()?.get_device_queue(self.queue_family_index, 0) };
        #[cfg(windows)]
        self.load_fsr_runtime();
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
        self.create_particle_pipelines()?;
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
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
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
        // SAFETY: device selection checks the required push-constant range before device creation.
        self.pipeline_layout = Some(unsafe { device.create_pipeline_layout(&layout_info, None) }?);

        let vertex_module = ShaderModule::new(&device, MESH_VERTEX_SHADER)?;
        let opaque_module = ShaderModule::new(&device, MESH_OPAQUE_FRAGMENT_SHADER)?;
        let mask_module = ShaderModule::new(&device, MESH_MASK_FRAGMENT_SHADER)?;
        let blend_module = ShaderModule::new(&device, MESH_BLEND_FRAGMENT_SHADER)?;
        let wireframe_module = ShaderModule::new(&device, WIREFRAME_FRAGMENT_SHADER)?;
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
        let wireframe_stages = [
            vertex_stage,
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(wireframe_module.handle)
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
        let velocity_write = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G);
        let blend_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA),
            velocity_write,
        ];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let wireframe_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty()),
        ];
        let wireframe_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&wireframe_attachments);
        let transparent_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
                .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
                .color_blend_op(vk::BlendOp::ADD)
                .color_write_mask(
                    vk::ColorComponentFlags::R
                        | vk::ColorComponentFlags::G
                        | vk::ColorComponentFlags::B,
                ),
            velocity_write,
        ];
        let transparent_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&transparent_attachments);
        let overdraw_attachments = [
            vk::PipelineColorBlendAttachmentState::default()
                .blend_enable(true)
                .src_color_blend_factor(vk::BlendFactor::ONE)
                .dst_color_blend_factor(vk::BlendFactor::ONE)
                .color_blend_op(vk::BlendOp::ADD)
                .color_write_mask(vk::ColorComponentFlags::R),
            vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::empty()),
        ];
        let overdraw_color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&overdraw_attachments);
        let depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(true)
            .depth_compare_op(vk::CompareOp::LESS);
        let transparent_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::LESS);
        let overdraw_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(false)
            .depth_write_enable(false);
        let wireframe_rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .depth_bias_enable(true)
            .depth_bias_constant_factor(-1.0)
            .depth_bias_slope_factor(-1.0)
            .polygon_mode(vk::PolygonMode::LINE)
            .cull_mode(vk::CullModeFlags::BACK)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let wireframe_depth_stencil = vk::PipelineDepthStencilStateCreateInfo::default()
            .depth_test_enable(true)
            .depth_write_enable(false)
            .depth_compare_op(vk::CompareOp::LESS_OR_EQUAL);
        let dynamic_states = [
            vk::DynamicState::VIEWPORT,
            vk::DynamicState::SCISSOR,
            vk::DynamicState::CULL_MODE,
            vk::DynamicState::FRONT_FACE,
        ];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
        let color_formats = [OFFSCREEN_FORMAT, VELOCITY_FORMAT];
        let mut opaque_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut mask_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut blend_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut overdraw_rendering_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&color_formats)
            .depth_attachment_format(DEPTH_FORMAT);
        let mut wireframe_rendering_info = vk::PipelineRenderingCreateInfo::default()
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
            vk::GraphicsPipelineCreateInfo::default()
                .stages(&opaque_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&overdraw_depth_stencil)
                .color_blend_state(&overdraw_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut overdraw_rendering_info),
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
        if pipelines.len() != 4 {
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
        self.overdraw_pipeline = Some(pipelines.remove(0));
        if self.capabilities.wireframe {
            let wireframe_info = [vk::GraphicsPipelineCreateInfo::default()
                .stages(&wireframe_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&input_assembly)
                .viewport_state(&viewport_state)
                .rasterization_state(&wireframe_rasterization)
                .multisample_state(&multisample)
                .depth_stencil_state(&wireframe_depth_stencil)
                .color_blend_state(&wireframe_color_blend)
                .dynamic_state(&dynamic_state)
                .layout(pipeline_layout)
                .push_next(&mut wireframe_rendering_info)];
            // SAFETY: the optional LINE pipeline uses the same live layout and dynamic-rendering formats.
            let mut wireframe_pipelines = match unsafe {
                device.create_graphics_pipelines(self.pipeline_cache()?, &wireframe_info, None)
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
            self.wireframe_pipeline = wireframe_pipelines.pop();
            if self.wireframe_pipeline.is_none() {
                return Err(RenderError::InvalidState("wireframe pipeline не создан"));
            }
        }
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
        let taa_set_layouts = [self.taa_set_layout()?, self.frame_set_layout()?];
        let taa_push_constants = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(TAA_PUSH_CONSTANTS_SIZE as u32)];
        let taa_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&taa_set_layouts)
            .push_constant_ranges(&taa_push_constants);
        // SAFETY: the TAA descriptor layouts and 16-byte fragment range match taa.frag.
        let taa_layout = unsafe { device.create_pipeline_layout(&taa_layout_info, None) }?;
        self.taa_pipeline_layout = Some(taa_layout);
        self.taa_pipeline = Some(create_post_pipeline(
            &device,
            cache,
            taa_layout,
            OFFSCREEN_FORMAT,
            TAA_FRAGMENT_SHADER,
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

    fn taa_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.taa_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов TAA отсутствует",
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
        self.pending_swapchain_drop.take();
        self.retired_swapchains.clear();
        self.frames.clear();
        self.retired.clear();
        self.environments.clear();
        self.viewports.clear();
        self.screen_temporal.history.take();
        #[cfg(windows)]
        self.screen_temporal.fsr.take();
        #[cfg(all(windows, feature = "dlss"))]
        self.screen_temporal.dlss.take();
        #[cfg(all(windows, feature = "dlss"))]
        self.dlss_api.take();
        self.ui_textures.clear();
        self.meshes.clear();
        self.materials.clear();
        self.textures.clear();
        if let Some(device) = self.device.as_ref() {
            self.particles.destroy(device);
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
                self.overdraw_pipeline.take(),
                self.wireframe_pipeline.take(),
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
                self.taa_pipeline.take(),
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
            if let Some(layout) = self.taa_pipeline_layout.take() {
                // SAFETY: the TAA pipeline created with this layout was destroyed above.
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
                self.taa_set_layout.take(),
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
    wireframe: bool,
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
    velocity_format_features: vk::FormatFeatureFlags,
    max_push_constants_size: u32,
}

struct DeviceEvaluation {
    reasons: Vec<String>,
    graphics_queue_family_indices: Vec<u32>,
}

struct DeviceInspection {
    report: DeviceReport,
    tessellation_shader: bool,
    wireframe: bool,
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
    let missing_velocity = missing_velocity_format_features(facts.velocity_format_features);
    if !missing_velocity.is_empty() {
        reasons.push(format!(
            "R16G16_SFLOAT не поддерживает velocity attachment: {}",
            missing_velocity.join(", ")
        ));
    }
    if facts.max_push_constants_size < PUSH_CONSTANTS_SIZE as u32 {
        reasons.push(format!(
            "maxPushConstantsSize меньше {} байт",
            PUSH_CONSTANTS_SIZE
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
    let (
        tessellation_shader,
        fill_mode_non_solid,
        storage_image_extended_formats,
        texture_sampler_anisotropy,
    ) = {
        let mut features_2 = vk::PhysicalDeviceFeatures2::default();
        // SAFETY: querying core features uses a valid physical device and null pNext chain.
        unsafe { instance.get_physical_device_features2(physical_device, &mut features_2) };
        (
            features_2.features.tessellation_shader != vk::FALSE,
            features_2.features.fill_mode_non_solid != vk::FALSE,
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
    let velocity_format_features =
        unsafe { instance.get_physical_device_format_properties(physical_device, VELOCITY_FORMAT) }
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
        velocity_format_features,
        max_push_constants_size: properties.limits.max_push_constants_size,
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
    reasons.extend(particle_gpu::particle_device_limit_reasons(
        &properties.limits,
    ));
    reasons.extend(texture_format_reasons(srgb_features, linear_features));
    reasons.extend(shadow_image_device_reasons(instance, physical_device));
    reasons.extend(velocity_image_device_reasons(instance, physical_device));
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
        wireframe: fill_mode_non_solid,
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

fn velocity_image_device_reasons(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<String> {
    // SAFETY: image-format support is queried for the live physical device.
    match unsafe {
        instance.get_physical_device_image_format_properties(
            physical_device,
            VELOCITY_FORMAT,
            vk::ImageType::TYPE_2D,
            vk::ImageTiling::OPTIMAL,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageCreateFlags::empty(),
        )
    } {
        Ok(_) => Vec::new(),
        Err(error) => vec![format!(
            "R16G16_SFLOAT image usage does not support velocity attachment sampling: {error}"
        )],
    }
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
            wireframe: inspection.wireframe,
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
        capabilities: vk::SurfaceCapabilitiesKHR,
        old_swapchain: vk::SwapchainKHR,
        requested_present_mode: PresentMode,
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
    /// Пул границ GPU-проходов слота; `None`, если очередь не поддерживает timestamp queries.
    timestamps: Option<vk::QueryPool>,
    /// Границы записаны и отправлены, но ещё не прочитаны.
    timestamps_pending: bool,
    /// Виды последнего записанного кадра слота; нужны для разбора прочитанных границ.
    gpu_layout: GpuProfileLayout,
    /// Последняя успешная отправка этого слота; начальный signaled fence не имеет поколения.
    submission_generation: u64,
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
            gpu_layout: GpuProfileLayout::default(),
            submission_generation: 0,
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

    /// Читает границы прошлой отправки этого слота и разбирает их по видам.
    ///
    /// Вызывается после ожидания fence слота. `NOT_READY` не отменяет чтение: доступность
    /// задаётся для каждого запроса, поэтому невыполненные проходы дают `None`.
    fn read_gpu_timings(
        &mut self,
        device: &ash::Device,
        valid_bits: u32,
        period: f32,
    ) -> Result<Option<GpuFrameTimings>, RenderError> {
        let Some(pool) = self.timestamps.filter(|_| self.timestamps_pending) else {
            return Ok(None);
        };
        self.timestamps_pending = false;
        // Для каждого запроса Vulkan пишет значение и флаг доступности: шаг буфера — 16 байт.
        let mut results = [[0_u64; 2]; TIMESTAMP_QUERY_COUNT as usize];
        // SAFETY: the pool was written by a submission whose fence was waited, and the buffer length equals its query count.
        match unsafe {
            device.get_query_pool_results(
                pool,
                0,
                &mut results,
                vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
            )
        } {
            Ok(()) | Err(vk::Result::NOT_READY) => {}
            Err(error) => return Err(RenderError::Vulkan(error)),
        }
        let queries = available_timestamps(&results);
        Ok(Some(self.gpu_layout.decode(&queries, valid_bits, period)))
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
    velocity: DeviceImage,
    depth: DeviceImage,
    layout: vk::ImageLayout,
    velocity_layout: vk::ImageLayout,
    depth_layout: vk::ImageLayout,
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
            velocity: DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                VELOCITY_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            depth: DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                DEPTH_FORMAT,
                vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                depth_subresource_range(),
            )?,
            layout: vk::ImageLayout::UNDEFINED,
            velocity_layout: vk::ImageLayout::UNDEFINED,
            depth_layout: vk::ImageLayout::UNDEFINED,
        })
    }
}

struct FrameTarget {
    _taa_pool: OwnedDescriptorPool,
    taa_set: vk::DescriptorSet,
    scene: OffscreenTarget,
    ldr: DeviceImage,
    ldr_layout: vk::ImageLayout,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
}

impl FrameTarget {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
        taa_set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let scene = OffscreenTarget::new(instance, device, physical_device, render_extent)?;
        let ldr = DeviceImage::new(
            instance,
            device,
            physical_device,
            output_extent,
            LDR_FORMAT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let (taa_pool, taa_set) = allocate_taa_image_set(device, taa_set_layout)?;
        Ok(Self {
            _taa_pool: taa_pool,
            taa_set,
            scene,
            ldr,
            ldr_layout: vk::ImageLayout::UNDEFINED,
            render_extent,
            output_extent,
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
    taa_set_layout: vk::DescriptorSetLayout,
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
    _taa_pool: OwnedDescriptorPool,
    taa_set: vk::DescriptorSet,
    post_pool: vk::DescriptorPool,
    tonemap_set: vk::DescriptorSet,
    output_set: vk::DescriptorSet,
    extent: vk::Extent2D,
    render_extent: vk::Extent2D,
    scene: OffscreenTarget,
    ldr: DeviceImage,
    ldr_layout: vk::ImageLayout,
    output: DeviceImage,
    output_layout: vk::ImageLayout,
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
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let frame_set = ViewportFrameSet::new(instance, device, physical_device, resources)?;
        let (taa_pool, taa_set) = allocate_taa_image_set(device, resources.taa_set_layout)?;
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
struct Viewport {
    extent: vk::Extent2D,
    targets: Vec<Option<ViewportTarget>>,
    temporal: TemporalView,
}

#[derive(Default)]
struct TemporalView {
    state: TemporalState,
    history: Option<TemporalHistory>,
    last_submitted_at: Option<Instant>,
    #[cfg(windows)]
    fsr: Option<Box<FsrView>>,
    #[cfg(windows)]
    fsr_attempted: bool,
    #[cfg(windows)]
    fsr_error: Option<String>,
    #[cfg(windows)]
    fsr_extents: Option<([u32; 2], [u32; 2])>,
    #[cfg(all(windows, feature = "dlss"))]
    dlss: Option<Box<DlssView>>,
    #[cfg(all(windows, feature = "dlss"))]
    dlss_attempted: bool,
    #[cfg(all(windows, feature = "dlss"))]
    dlss_error: Option<String>,
    #[cfg(all(windows, feature = "dlss"))]
    dlss_extents: Option<([u32; 2], [u32; 2])>,
}

struct TemporalHistory {
    extent: vk::Extent2D,
    images: [DeviceImage; 2],
    layouts: [vk::ImageLayout; 2],
}

#[cfg(windows)]
struct FsrView {
    context: fsr::FfxContext,
    output: DeviceImage,
    exposure: DeviceImage,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    output_layout: vk::ImageLayout,
    exposure_layout: vk::ImageLayout,
}

#[cfg(windows)]
impl FsrView {
    fn new(
        api: &Arc<fsr::FfxApi>,
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let context = api
            .create_upscale_context(
                device.handle(),
                physical_device,
                instance.fp_v1_0().get_device_proc_addr,
                [render_extent.width, render_extent.height],
                [output_extent.width, output_extent.height],
            )
            .map_err(|error| RenderError::Unsupported(format!("FSR context: {error}")))?;
        let output = DeviceImage::new(
            instance,
            device,
            physical_device,
            output_extent,
            OFFSCREEN_FORMAT,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let exposure = DeviceImage::new(
            instance,
            device,
            physical_device,
            vk::Extent2D {
                width: 1,
                height: 1,
            },
            vk::Format::R32_SFLOAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        Ok(Self {
            context,
            output,
            exposure,
            render_extent,
            output_extent,
            output_layout: vk::ImageLayout::UNDEFINED,
            exposure_layout: vk::ImageLayout::UNDEFINED,
        })
    }
}

#[cfg(all(windows, feature = "dlss"))]
struct DlssView {
    output: DeviceImage,
    exposure: DeviceImage,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    output_layout: vk::ImageLayout,
    exposure_layout: vk::ImageLayout,
    feature: Option<usize>,
}

#[cfg(all(windows, feature = "dlss"))]
impl DlssView {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        output_extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let output = DeviceImage::new(
            instance,
            device,
            physical_device,
            output_extent,
            OFFSCREEN_FORMAT,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        let exposure = DeviceImage::new(
            instance,
            device,
            physical_device,
            vk::Extent2D {
                width: 1,
                height: 1,
            },
            vk::Format::R32_SFLOAT,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED,
            color_subresource_range(),
        )?;
        Ok(Self {
            output,
            exposure,
            render_extent,
            output_extent,
            output_layout: vk::ImageLayout::UNDEFINED,
            exposure_layout: vk::ImageLayout::UNDEFINED,
            feature: None,
        })
    }
}

#[cfg(all(windows, feature = "dlss"))]
impl Drop for DlssView {
    fn drop(&mut self) {
        if let Some(feature) = self.feature.take() {
            dlss::release_feature(feature);
        }
    }
}

impl TemporalHistory {
    fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
    ) -> Result<Self, RenderError> {
        let images = [
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
        ];
        Ok(Self {
            extent,
            images,
            layouts: [vk::ImageLayout::UNDEFINED; 2],
        })
    }
}

#[derive(Clone, Copy)]
enum TemporalViewId {
    Screen,
    Viewport(ViewportId),
}

#[derive(Clone, Copy)]
struct TemporalSubmission {
    view: TemporalViewId,
    plan: TemporalPlan,
    layouts: Option<[vk::ImageLayout; 2]>,
    #[cfg(windows)]
    fsr_used: bool,
    #[cfg(all(windows, feature = "dlss"))]
    dlss_used: bool,
    submitted_at: Instant,
}

fn ensure_temporal_history(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    temporal: &mut TemporalView,
    output_extent: vk::Extent2D,
    retired: &mut RetireQueue<RetiredResource>,
) -> Result<(), RenderError> {
    if temporal
        .history
        .as_ref()
        .is_some_and(|history| history.extent == output_extent)
    {
        return Ok(());
    }
    let replacement = TemporalHistory::new(instance, device, physical_device, output_extent)?;
    if let Some(previous) = temporal.history.replace(replacement) {
        retired.retire(RetiredResource::TemporalHistory(Box::new(previous)), 0);
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn ensure_fsr_view(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    temporal: &mut TemporalView,
    api: &Arc<fsr::FfxApi>,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    retired: &mut RetireQueue<RetiredResource>,
) -> Result<(), String> {
    let render_size = [render_extent.width, render_extent.height];
    let output_size = [output_extent.width, output_extent.height];
    if temporal.fsr_extents == Some((render_size, output_size)) {
        if temporal.fsr.is_some() {
            return Ok(());
        }
        if temporal.fsr_attempted {
            return Err(temporal
                .fsr_error
                .clone()
                .unwrap_or_else(|| "FSR context creation failed".to_owned()));
        }
    }
    if temporal.fsr_extents != Some((render_size, output_size)) {
        temporal.fsr_extents = Some((render_size, output_size));
        temporal.fsr_attempted = false;
        temporal.fsr_error = None;
    }
    if temporal
        .fsr
        .as_ref()
        .is_some_and(|fsr| fsr.render_extent == render_extent && fsr.output_extent == output_extent)
    {
        return Ok(());
    }
    if let Some(previous) = temporal.fsr.take() {
        retired.retire(RetiredResource::FsrView(previous), 0);
    }
    temporal.fsr_attempted = true;
    temporal.fsr_error = None;
    match FsrView::new(
        api,
        instance,
        device,
        physical_device,
        render_extent,
        output_extent,
    ) {
        Ok(fsr) => {
            temporal.fsr = Some(Box::new(fsr));
            Ok(())
        }
        Err(error) => {
            let error = error.to_string();
            temporal.fsr_error = Some(error.clone());
            temporal.fsr_attempted = true;
            Err(error)
        }
    }
}

#[cfg(all(windows, feature = "dlss"))]
fn ensure_dlss_view(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    temporal: &mut TemporalView,
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    retired: &mut RetireQueue<RetiredResource>,
) -> Result<(), String> {
    let render_size = [render_extent.width, render_extent.height];
    let output_size = [output_extent.width, output_extent.height];
    if temporal.dlss_extents == Some((render_size, output_size)) {
        if temporal.dlss.is_some() {
            return Ok(());
        }
        if temporal.dlss_attempted {
            return Err(temporal
                .dlss_error
                .clone()
                .unwrap_or_else(|| "DLSS view creation failed".to_owned()));
        }
    }
    if temporal.dlss_extents != Some((render_size, output_size)) {
        temporal.dlss_extents = Some((render_size, output_size));
        temporal.dlss_attempted = false;
        temporal.dlss_error = None;
    }
    if temporal.dlss.as_ref().is_some_and(|view| {
        view.render_extent == render_extent && view.output_extent == output_extent
    }) {
        return Ok(());
    }
    if let Some(previous) = temporal.dlss.take() {
        retired.retire(RetiredResource::DlssView(previous), 0);
    }
    temporal.dlss_attempted = true;
    temporal.dlss_error = None;
    match DlssView::new(
        instance,
        device,
        physical_device,
        render_extent,
        output_extent,
    ) {
        Ok(view) => {
            temporal.dlss = Some(Box::new(view));
            Ok(())
        }
        Err(error) => {
            let error = error.to_string();
            temporal.dlss_error = Some(error.clone());
            Err(error)
        }
    }
}

fn temporal_frame_delta_ms(previous: Option<Instant>, now: Instant) -> f32 {
    previous
        .and_then(|previous| now.checked_duration_since(previous))
        .map_or(1000.0 / 60.0, |elapsed| {
            (elapsed.as_secs_f32() * 1000.0).clamp(1.0, 1000.0)
        })
}

fn warn_temporal_fallback(backend: &str, warning_emitted: &mut bool, reason: &str) {
    if !*warning_emitted {
        eprintln!("{backend} unavailable; falling back to TAA: {reason}");
        *warning_emitted = true;
    }
}

#[cfg(windows)]
fn fsr_camera_parameters(projection: [[f32; 4]; 4]) -> [f32; 3] {
    let depth_scale = projection[2][2];
    let depth_offset = projection[3][2];
    let fov_scale = projection[1][1];
    let near = depth_offset / depth_scale;
    let far = depth_offset / (depth_scale + 1.0);
    let fov_y = 2.0 * (1.0 / fov_scale).atan();
    if near.is_finite()
        && far.is_finite()
        && fov_y.is_finite()
        && near > 0.0
        && far > near
        && fov_y > 0.0
        && fov_y < std::f32::consts::PI
    {
        [near, far, fov_y]
    } else {
        [0.1, 1000.0, std::f32::consts::FRAC_PI_3]
    }
}

fn temporal_key(
    render_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    anti_aliasing: AntiAliasing,
    debug_view: u32,
) -> TemporalKey {
    TemporalKey {
        render_extent: TemporalExtent {
            width: render_extent.width,
            height: render_extent.height,
        },
        output_extent: TemporalExtent {
            width: output_extent.width,
            height: output_extent.height,
        },
        anti_aliasing,
        debug_view,
    }
}

fn effective_view_settings(mut view: ViewSettings, debug_view: DebugView) -> ViewSettings {
    if debug_view != DebugView::Lit {
        view.anti_aliasing = AntiAliasing::None;
    }
    if debug_view.has_linear_output() {
        view.exposure_ev = 0.0;
        view.tonemapper = Tonemapper::None;
    }
    view
}

fn validate_debug_view(
    debug_view: DebugView,
    wireframe_supported: bool,
) -> Result<(), RenderError> {
    if debug_view == DebugView::Wireframe && !wireframe_supported {
        return Err(RenderError::InvalidConfig(
            "wireframe debug view requires fillModeNonSolid support",
        ));
    }
    Ok(())
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
    Viewport(#[allow(dead_code)] Box<Viewport>),
    TemporalHistory(#[allow(dead_code)] Box<TemporalHistory>),
    #[cfg(windows)]
    FsrView(#[allow(dead_code)] Box<FsrView>),
    #[cfg(all(windows, feature = "dlss"))]
    DlssView(#[allow(dead_code)] Box<DlssView>),
    Environment(#[allow(dead_code)] Box<GpuEnvironment>),
    ParticleEmitter(#[allow(dead_code)] Box<ParticleEmitter>),
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

fn allocate_taa_image_set(
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

fn write_taa_image_set(
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

/// Получает swapchain image; `None` означает, что кадр нужно пропустить.
fn acquire_image(
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

struct DeviceImage {
    device: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    #[cfg(all(windows, feature = "dlss"))]
    format: vk::Format,
    #[cfg(all(windows, feature = "dlss"))]
    subresource_range: vk::ImageSubresourceRange,
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

struct TemporalResolveRecord<'a> {
    history: &'a TemporalHistory,
    #[cfg(windows)]
    backend: TemporalBackend,
    plan: TemporalPlan,
    taa_pipeline: vk::Pipeline,
    taa_pipeline_layout: vk::PipelineLayout,
    #[cfg(windows)]
    fsr: Option<&'a mut FsrView>,
    #[cfg(windows)]
    exposure_ev: f32,
    #[cfg(windows)]
    frame_time_delta_ms: f32,
    #[cfg(windows)]
    camera: [f32; 3],
    #[cfg(all(windows, feature = "dlss"))]
    dlss_api: Option<&'a dlss::NgxApi>,
    #[cfg(all(windows, feature = "dlss"))]
    dlss: Option<&'a mut DlssView>,
}

struct TemporalResolveOutcome {
    used_taa: bool,
    #[cfg(windows)]
    used_fsr: bool,
    #[cfg(all(windows, feature = "dlss"))]
    used_dlss: bool,
    fsr_error: Option<String>,
    #[cfg(all(windows, feature = "dlss"))]
    dlss_error: Option<String>,
    output_extent: vk::Extent2D,
}

#[allow(clippy::too_many_arguments)]
fn record_temporal_resolve_and_tonemap(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    ldr: &mut DeviceImage,
    ldr_layout: &mut vk::ImageLayout,
    tonemap_set: vk::DescriptorSet,
    post_sampler: vk::Sampler,
    frame_set: vk::DescriptorSet,
    output_extent: vk::Extent2D,
    view: ViewSettings,
    debug_view: DebugView,
    timestamps: Option<GpuMarks>,
    tonemap_pipeline: vk::Pipeline,
    post_pipeline_layout: vk::PipelineLayout,
    taa_set: vk::DescriptorSet,
    resolve: TemporalResolveRecord<'_>,
) -> Result<TemporalResolveOutcome, RenderError> {
    #[cfg(windows)]
    let mut fsr_error = None;
    #[cfg(not(windows))]
    let fsr_error = None;
    #[cfg(all(windows, feature = "dlss"))]
    let mut dlss_error = None;
    #[cfg(windows)]
    let mut used_taa = true;
    #[cfg(not(windows))]
    let used_taa = true;
    #[cfg(windows)]
    let mut used_fsr = false;
    #[cfg(all(windows, feature = "dlss"))]
    let mut used_dlss = false;
    #[cfg(windows)]
    let mut reset_taa_history = false;
    #[cfg(not(windows))]
    let reset_taa_history = false;
    #[cfg(windows)]
    let mut resolved_source = None;
    #[cfg(windows)]
    if resolve.backend == TemporalBackend::Fsr
        && let Some(fsr) = resolve.fsr
    {
        match record_fsr_pass(
            device,
            command_buffer,
            scene,
            fsr,
            resolve.plan,
            resolve.exposure_ev,
            resolve.frame_time_delta_ms,
            resolve.camera,
        ) {
            Ok(()) => {
                used_taa = false;
                used_fsr = true;
                resolved_source = Some(&fsr.output);
            }
            Err(error) => {
                fsr_error = Some(error);
                reset_taa_history = true;
            }
        }
    }
    #[cfg(all(windows, feature = "dlss"))]
    if resolve.backend == TemporalBackend::Dlss
        && let (Some(api), Some(dlss)) = (resolve.dlss_api, resolve.dlss)
    {
        match record_dlss_pass(
            device,
            command_buffer,
            scene,
            dlss,
            api,
            resolve.plan,
            resolve.exposure_ev,
            resolve.frame_time_delta_ms,
        ) {
            Ok(()) => {
                used_taa = false;
                used_dlss = true;
                resolved_source = Some(&dlss.output);
            }
            Err(error) => {
                dlss_error = Some(error);
                reset_taa_history = true;
            }
        }
    }

    let source = if used_taa {
        let mut taa_plan = resolve.plan;
        if reset_taa_history {
            taa_plan.history_valid = false;
            taa_plan.reset_history = true;
        }
        record_taa_pass(
            device,
            command_buffer,
            scene,
            resolve.history,
            taa_set,
            frame_set,
            output_extent,
            resolve.taa_pipeline,
            resolve.taa_pipeline_layout,
            taa_plan,
        );
        &resolve.history.images[resolve.plan.write_history]
    } else {
        #[cfg(windows)]
        {
            resolved_source.ok_or(RenderError::InvalidState(
                "temporal upscaler output отсутствует",
            ))?
        }
        #[cfg(not(windows))]
        {
            return Err(RenderError::InvalidState(
                "FSR backend выбран на неподдерживаемой платформе",
            ));
        }
    };
    mark_pass(device, command_buffer, timestamps, GpuPass::TemporalResolve);
    write_image_set(device, tonemap_set, source.view, post_sampler);
    record_tonemap_pass(
        device,
        command_buffer,
        source,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        ldr,
        ldr_layout,
        output_extent,
        tonemap_set,
        tonemap_pipeline,
        post_pipeline_layout,
        view,
        debug_view,
        timestamps,
    );
    Ok(TemporalResolveOutcome {
        used_taa,
        #[cfg(windows)]
        used_fsr,
        #[cfg(all(windows, feature = "dlss"))]
        used_dlss,
        fsr_error,
        #[cfg(all(windows, feature = "dlss"))]
        dlss_error,
        output_extent,
    })
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn record_fsr_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    fsr: &mut FsrView,
    plan: TemporalPlan,
    exposure_ev: f32,
    frame_time_delta_ms: f32,
    camera: [f32; 3],
) -> Result<(), String> {
    if scene.layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        let (source_stage, source_access) = offscreen_previous_use(scene.layout);
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                scene.color.image,
                scene.layout,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                source_stage,
                source_access,
                vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                color_subresource_range(),
            ),
        );
        scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }
    let exposure_src = if fsr.exposure_layout == vk::ImageLayout::UNDEFINED {
        (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
    } else {
        (
            vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        )
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            fsr.exposure.image,
            fsr.exposure_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            exposure_src.0,
            exposure_src.1,
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            color_subresource_range(),
        ),
    );
    let exposure = [fsr::exposure_value(exposure_ev), 0.0, 0.0, 0.0];
    // SAFETY: the 1x1 exposure image is in TRANSFER_DST_OPTIMAL and the range covers its only subresource.
    unsafe {
        device.cmd_clear_color_image(
            command_buffer,
            fsr.exposure.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue { float32: exposure },
            &[color_subresource_range()],
        )
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            fsr.exposure.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    fsr.exposure_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

    if fsr.output_layout == vk::ImageLayout::UNDEFINED {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                fsr.output.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::NONE,
                vk::AccessFlags2::NONE,
                vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                color_subresource_range(),
            ),
        );
        fsr.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }

    let render_size = [fsr.render_extent.width, fsr.render_extent.height];
    let output_size = [fsr.output_extent.width, fsr.output_extent.height];
    let resource_state = fsr::RESOURCE_STATE_PIXEL_COMPUTE_READ;
    let dispatch = fsr::dispatch_description(
        command_buffer.as_raw() as usize as *mut std::ffi::c_void,
        fsr::resource(
            scene.color.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R16G16B16A16_FLOAT,
            render_size[0],
            render_size[1],
            0,
            resource_state,
        ),
        fsr::resource(
            scene.depth.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R32_FLOAT,
            render_size[0],
            render_size[1],
            fsr::RESOURCE_USAGE_DEPTH_TARGET,
            resource_state,
        ),
        fsr::resource(
            scene.velocity.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R16G16_FLOAT,
            render_size[0],
            render_size[1],
            0,
            resource_state,
        ),
        fsr::resource(
            fsr.exposure.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R32_FLOAT,
            1,
            1,
            0,
            resource_state,
        ),
        fsr::resource(
            fsr.output.image.as_raw() as usize as *mut std::ffi::c_void,
            fsr::FORMAT_R16G16B16A16_FLOAT,
            output_size[0],
            output_size[1],
            fsr::RESOURCE_USAGE_UAV,
            resource_state,
        ),
        plan.jitter_render_pixels,
        render_size,
        output_size,
        frame_time_delta_ms,
        plan.reset_history,
        camera,
    );
    fsr.context.dispatch(&dispatch)?;
    fsr.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    Ok(())
}

#[cfg(all(windows, feature = "dlss"))]
#[allow(clippy::too_many_arguments)]
fn record_dlss_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    dlss_view: &mut DlssView,
    api: &dlss::NgxApi,
    plan: TemporalPlan,
    exposure_ev: f32,
    frame_time_delta_ms: f32,
) -> Result<(), String> {
    for (image, layout, range) in [
        (&scene.color, &mut scene.layout, color_subresource_range()),
        (
            &scene.velocity,
            &mut scene.velocity_layout,
            color_subresource_range(),
        ),
        (
            &scene.depth,
            &mut scene.depth_layout,
            depth_subresource_range(),
        ),
    ] {
        if *layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            let (source_stage, source_access) = offscreen_previous_use(*layout);
            pipeline_barrier(
                device,
                command_buffer,
                image_layout_barrier(
                    image.image,
                    *layout,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    source_stage,
                    source_access,
                    vk::PipelineStageFlags2::ALL_COMMANDS,
                    vk::AccessFlags2::SHADER_SAMPLED_READ,
                    range,
                ),
            );
            *layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        }
    }

    let exposure_source = if dlss_view.exposure_layout == vk::ImageLayout::UNDEFINED {
        (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
    } else {
        (
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        )
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            dlss_view.exposure.image,
            dlss_view.exposure_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            exposure_source.0,
            exposure_source.1,
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            color_subresource_range(),
        ),
    );
    let exposure = [fsr::exposure_value(exposure_ev), 0.0, 0.0, 0.0];
    // SAFETY: the 1x1 exposure image is in TRANSFER_DST_OPTIMAL and the range covers its only subresource.
    unsafe {
        device.cmd_clear_color_image(
            command_buffer,
            dlss_view.exposure.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &vk::ClearColorValue { float32: exposure },
            &[color_subresource_range()],
        )
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            dlss_view.exposure.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    dlss_view.exposure_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

    let (output_stage, output_access) = offscreen_previous_use(dlss_view.output_layout);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            dlss_view.output.image,
            dlss_view.output_layout,
            vk::ImageLayout::GENERAL,
            output_stage,
            output_access,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE,
            color_subresource_range(),
        ),
    );
    dlss_view.output_layout = vk::ImageLayout::GENERAL;

    let render_extent = dlss_view.render_extent;
    let output_extent = dlss_view.output_extent;
    let color = ngx_image_resource(&scene.color, render_extent, false);
    let depth = ngx_image_resource(&scene.depth, render_extent, false);
    let motion_vectors = ngx_image_resource(&scene.velocity, render_extent, false);
    let exposure = ngx_image_resource(
        &dlss_view.exposure,
        vk::Extent2D {
            width: 1,
            height: 1,
        },
        false,
    );
    let output = ngx_image_resource(&dlss_view.output, output_extent, true);
    api.evaluate(dlss::EvaluateParameters {
        device: device.handle(),
        command_buffer,
        feature: &mut dlss_view.feature,
        render_extent,
        output_extent,
        color: &color,
        depth: &depth,
        motion_vectors: &motion_vectors,
        exposure: &exposure,
        output: &output,
        jitter: dlss::ngx_jitter_offset(plan.jitter_render_pixels),
        motion_scale: [render_extent.width as f32, render_extent.height as f32],
        reset: plan.reset_history,
        frame_time_delta_ms,
    })?;

    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            dlss_view.output.image,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::MEMORY_WRITE,
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    dlss_view.output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    Ok(())
}

#[cfg(all(windows, feature = "dlss"))]
fn ngx_image_resource(
    image: &DeviceImage,
    extent: vk::Extent2D,
    read_write: bool,
) -> dlss::NgxResourceVk {
    dlss::NgxResourceVk {
        resource: dlss::NgxResourceUnion {
            image_view: dlss::NgxImageViewInfo {
                image_view: image.view.as_raw(),
                image: image.image.as_raw(),
                subresource_range: image.subresource_range,
                format: image.format.as_raw(),
                width: extent.width,
                height: extent.height,
            },
        },
        resource_type: 0,
        read_write,
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
    post_sampler: vk::Sampler,
    output_set: vk::DescriptorSet,
    tonemap_pipeline: vk::Pipeline,
    output_pipeline: vk::Pipeline,
    post_pipeline_layout: vk::PipelineLayout,
    output_is_srgb: bool,
    shadow: &ShadowPass<'_>,
    scene: &ScenePass<'_>,
    overlay: Option<&OverlayPass>,
    temporal: Option<TemporalResolveRecord<'_>>,
    output_marks: Option<GpuMarks>,
) -> Result<Option<TemporalResolveOutcome>, RenderError> {
    begin_command_buffer(device, command_buffer, output_marks.map(|marks| marks.pool))?;
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
    let temporal_outcome = if let Some(resolve) = temporal {
        Some(record_temporal_resolve_and_tonemap(
            device,
            command_buffer,
            &mut target.scene,
            &mut target.ldr,
            &mut target.ldr_layout,
            tonemap_set,
            post_sampler,
            scene.frame_set,
            target.output_extent,
            view,
            scene.debug_view,
            scene.timestamps,
            tonemap_pipeline,
            post_pipeline_layout,
            target.taa_set,
            resolve,
        )?)
    } else {
        write_image_set(device, tonemap_set, target.scene.color.view, post_sampler);
        record_tonemap_pass(
            device,
            command_buffer,
            &target.scene.color,
            target.scene.layout,
            &target.ldr,
            &mut target.ldr_layout,
            target.render_extent,
            tonemap_set,
            tonemap_pipeline,
            post_pipeline_layout,
            view,
            scene.debug_view,
            scene.timestamps,
        );
        target.scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        None
    };
    let output_source_extent = temporal_outcome
        .as_ref()
        .map_or(target.render_extent, |outcome| outcome.output_extent);
    if let Some(marks) = output_marks {
        marks.write(device, command_buffer, 0);
    }
    record_output_pass(
        device,
        command_buffer,
        output_image,
        output_view,
        output_source_extent,
        output_extent,
        vk::ImageLayout::UNDEFINED,
        output_set,
        output_pipeline,
        post_pipeline_layout,
        view.anti_aliasing,
        output_is_srgb,
        overlay,
        false,
    );
    if let Some(marks) = output_marks {
        marks.write(device, command_buffer, 1);
    }
    // SAFETY: command recording is complete before the command buffer is submitted.
    unsafe { device.end_command_buffer(command_buffer) }?;
    Ok(temporal_outcome)
}

fn begin_command_buffer(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    timestamps: Option<vk::QueryPool>,
) -> Result<(), RenderError> {
    // SAFETY: this frame's command buffer is no longer in flight because its fence was waited.
    unsafe { device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty()) }?;
    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the command buffer was reset and belongs to the renderer's graphics command pool.
    unsafe { device.begin_command_buffer(command_buffer, &begin_info) }?;
    if let Some(pool) = timestamps {
        // Все запросы пула сбрасываются до первого вида; reset выполняется вне rendering.
        // SAFETY: the fence was waited, so no earlier submission still uses these queries.
        unsafe { device.cmd_reset_query_pool(command_buffer, pool, 0, TIMESTAMP_QUERY_COUNT) };
    }
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
    if let Some(marks) = scene.timestamps {
        // Начало GPU-времени вида: граница стоит до clustering.
        marks.write(device, command_buffer, 0);
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
    mark_pass(device, command_buffer, scene.timestamps, GpuPass::Clusters);
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
    let (velocity_stage, velocity_access) = offscreen_previous_use(target.velocity_layout);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.velocity.image,
            target.velocity_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            velocity_stage,
            velocity_access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );
    target.velocity_layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
    let (depth_stage, depth_access) = offscreen_previous_use(target.depth_layout);
    let depth_barrier = image_layout_barrier(
        target.depth.image,
        target.depth_layout,
        vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
        depth_stage,
        depth_access,
        vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
            | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
        vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_READ
            | vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
        depth_subresource_range(),
    );
    pipeline_barrier(device, command_buffer, depth_barrier);
    target.depth_layout = vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL;
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

    if !shadow.shadow_cascades.is_empty() {
        mark_pass(
            device,
            command_buffer,
            scene.timestamps,
            GpuPass::ShadowDirectional,
        );
    }
    record_local_shadow_layers(
        device,
        command_buffer,
        local_shadow_image,
        local_shadow_layout,
        shadow,
    );
    // Граница ставится всегда: очистка слоёв относится к проходу, даже если локальных источников нет.
    mark_pass(
        device,
        command_buffer,
        scene.timestamps,
        GpuPass::ShadowLocal,
    );

    let clear_color = if scene.debug_view == DebugView::Overdraw {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        scene.clear_color
    };
    let color_attachment = [
        vk::RenderingAttachmentInfo::default()
            .image_view(target.color.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: clear_color,
                },
            }),
        vk::RenderingAttachmentInfo::default()
            .image_view(target.velocity.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.0; 4] },
            }),
    ];
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
        if scene.debug_view == DebugView::Overdraw {
            DrawPass::Overdraw
        } else {
            DrawPass::OpaqueAndMask
        },
    );
    // SAFETY: this matches the preceding cmd_begin_rendering call.
    unsafe { device.cmd_end_rendering(command_buffer) };
    mark_pass(device, command_buffer, scene.timestamps, GpuPass::Opaque);
    if scene.skybox && scene.debug_view != DebugView::Overdraw {
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
                target.velocity.image,
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
        let color_attachment = [
            vk::RenderingAttachmentInfo::default()
                .image_view(target.color.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
            vk::RenderingAttachmentInfo::default()
                .image_view(target.velocity.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
        ];
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
        mark_pass(device, command_buffer, scene.timestamps, GpuPass::Skybox);
    }
    if scene.debug_view != DebugView::Overdraw && has_transparent_draws(scene) {
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
        let velocity_to_blend = image_layout_barrier(
            target.velocity.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        );
        pipeline_barrier(device, command_buffer, velocity_to_blend);
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
        let color_attachment = [
            vk::RenderingAttachmentInfo::default()
                .image_view(target.color.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
            vk::RenderingAttachmentInfo::default()
                .image_view(target.velocity.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::LOAD)
                .store_op(vk::AttachmentStoreOp::STORE),
        ];
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
        mark_pass(
            device,
            command_buffer,
            scene.timestamps,
            GpuPass::Transparent,
        );
    }
    if scene.debug_view == DebugView::Wireframe {
        record_wireframe(device, command_buffer, target, render_extent, scene);
        mark_pass(device, command_buffer, scene.timestamps, GpuPass::Wireframe);
    }
    if let Some(particles) = &scene.particles {
        record_particles(
            device,
            command_buffer,
            target,
            render_extent,
            particles,
            scene.timestamps,
        );
    }
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.velocity.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    target.velocity_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.depth.image,
            vk::ImageLayout::DEPTH_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags2::LATE_FRAGMENT_TESTS,
            vk::AccessFlags2::DEPTH_STENCIL_ATTACHMENT_WRITE,
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            depth_subresource_range(),
        ),
    );
    target.layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
    target.depth_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

fn record_wireframe(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    target: &OffscreenTarget,
    render_extent: vk::Extent2D,
    scene: &ScenePass<'_>,
) {
    for image in [&target.color, &target.velocity] {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                image.image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
                color_subresource_range(),
            ),
        );
    }
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
    let color_attachments = [
        vk::RenderingAttachmentInfo::default()
            .image_view(target.color.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE),
        vk::RenderingAttachmentInfo::default()
            .image_view(target.velocity.view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::LOAD)
            .store_op(vk::AttachmentStoreOp::STORE),
    ];
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
        .color_attachments(&color_attachments)
        .depth_attachment(&depth_attachment);
    // SAFETY: wireframe overlay attachments match the scene pass formats and remain in attachment layouts.
    unsafe { device.cmd_begin_rendering(command_buffer, &rendering_info) };
    record_draws(
        device,
        command_buffer,
        render_extent,
        scene,
        DrawPass::Wireframe,
    );
    // SAFETY: this matches the preceding dynamic-rendering begin call.
    unsafe { device.cmd_end_rendering(command_buffer) };
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
    Overdraw,
    Wireframe,
}

struct ScenePass<'a> {
    pipeline: vk::Pipeline,
    mask_pipeline: vk::Pipeline,
    blend_pipeline: vk::Pipeline,
    overdraw_pipeline: vk::Pipeline,
    wireframe_pipeline: Option<vk::Pipeline>,
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
    #[cfg(windows)]
    fsr_camera: [f32; 3],
    culling_padding: [f32; 2],
    temporal_plan: TemporalPlan,
    camera_position: [f32; 3],
    clear_color: [f32; 4],
    timestamps: Option<GpuMarks>,
    debug_view: DebugView,
    view: ViewSettings,
    particles: Option<ParticlePass<'a>>,
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
    let frustum =
        Frustum::from_view_projection_with_padding(scene.view_projection, scene.culling_padding);
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
        let pipeline_kind = match pass {
            DrawPass::Overdraw => DrawPipeline::Overdraw,
            DrawPass::Wireframe => DrawPipeline::Wireframe,
            DrawPass::OpaqueAndMask => match material.alpha_mode {
                AlphaMode::Opaque => DrawPipeline::Opaque,
                AlphaMode::Mask { .. } => DrawPipeline::Mask,
                AlphaMode::Blend => continue,
            },
            DrawPass::Blend => match material.alpha_mode {
                AlphaMode::Blend => DrawPipeline::Blend,
                AlphaMode::Opaque | AlphaMode::Mask { .. } => continue,
            },
        };
        if bound_pipeline != Some(pipeline_kind) {
            let pipeline = match pipeline_kind {
                DrawPipeline::Opaque => scene.pipeline,
                DrawPipeline::Mask => scene.mask_pipeline,
                DrawPipeline::Blend => scene.blend_pipeline,
                DrawPipeline::Overdraw => scene.overdraw_pipeline,
                DrawPipeline::Wireframe => scene.wireframe_pipeline.unwrap_or(scene.pipeline),
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
        let push_constants = push_constant_bytes(
            &draw.model,
            draw.previous_model.as_ref().unwrap_or(&draw.model),
            draw.lod_level,
        );
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
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
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
    Overdraw,
    Wireframe,
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
        let push_constants = push_constant_bytes(
            &draw.model,
            draw.previous_model.as_ref().unwrap_or(&draw.model),
            draw.lod_level,
        );
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
    source: &DeviceImage,
    source_layout: vk::ImageLayout,
    output: &DeviceImage,
    output_layout: &mut vk::ImageLayout,
    extent: vk::Extent2D,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    view: ViewSettings,
    debug_view: DebugView,
    timestamps: Option<GpuMarks>,
) {
    if source_layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        let (source_stage, source_access) = match source_layout {
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            ),
            _ => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        };
        let source_barrier = image_layout_barrier(
            source.image,
            source_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            source_stage,
            source_access,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        );
        pipeline_barrier(device, command_buffer, source_barrier);
    }
    let (output_stage, output_access) = offscreen_previous_use(*output_layout);
    let output_barrier = image_layout_barrier(
        output.image,
        *output_layout,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        output_stage,
        output_access,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        color_subresource_range(),
    );
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
        debug_view.code() as f32,
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
    mark_pass(device, command_buffer, timestamps, GpuPass::Tonemap);
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
    *output_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

#[allow(clippy::too_many_arguments)]
fn record_taa_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scene: &mut OffscreenTarget,
    history: &TemporalHistory,
    taa_set: vk::DescriptorSet,
    frame_set: vk::DescriptorSet,
    output_extent: vk::Extent2D,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    plan: TemporalPlan,
) {
    let (scene_stage, scene_access) = offscreen_previous_use(scene.layout);
    let scene_to_sample = image_layout_barrier(
        scene.color.image,
        scene.layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        scene_stage,
        scene_access,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
        color_subresource_range(),
    );
    pipeline_barrier(device, command_buffer, scene_to_sample);

    let read_layout = history.layouts[plan.read_history];
    if read_layout != vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        pipeline_barrier(
            device,
            command_buffer,
            image_layout_barrier(
                history.images[plan.read_history].image,
                read_layout,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::NONE,
                vk::AccessFlags2::NONE,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
                color_subresource_range(),
            ),
        );
    }
    let write_image = &history.images[plan.write_history];
    let (write_stage, write_access) = match history.layouts[plan.write_history] {
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            write_image.image,
            history.layouts[plan.write_history],
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            write_stage,
            write_access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );

    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(write_image.view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: output_extent,
    };
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
    let push_constants = float_bytes::<TAA_PUSH_CONSTANTS_SIZE>(&[
        plan.jitter_render_pixels[0],
        plan.jitter_render_pixels[1],
        f32::from(u8::from(plan.history_valid)),
        f32::from(u8::from(plan.reset_history)),
    ]);
    // SAFETY: both sampled sets and the output attachment match the TAA pipeline layouts.
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
            &[taa_set, frame_set],
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
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            write_image.image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    scene.layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

#[allow(clippy::too_many_arguments)]
fn record_output_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    output_image: vk::Image,
    output_view: vk::ImageView,
    source_extent: vk::Extent2D,
    output_extent: vk::Extent2D,
    previous_output_layout: vk::ImageLayout,
    set: vk::DescriptorSet,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    anti_aliasing: AntiAliasing,
    output_is_srgb: bool,
    overlay: Option<&OverlayPass>,
    sample_output: bool,
) {
    let to_attachment = if sample_output {
        let (source_stage, source_access) = offscreen_previous_use(previous_output_layout);
        image_layout_barrier(
            output_image,
            previous_output_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            source_stage,
            source_access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        )
    } else {
        output_attachment_barrier(output_image)
    };
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

fn missing_velocity_format_features(supported: vk::FormatFeatureFlags) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
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

/// Возвращает источник доступа offscreen image перед повторной записью.
fn offscreen_previous_use(layout: vk::ImageLayout) -> (vk::PipelineStageFlags2, vk::AccessFlags2) {
    match layout {
        vk::ImageLayout::UNDEFINED => (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE),
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        ),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            offscreen_sampled_stages(),
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::TRANSFER,
            vk::AccessFlags2::TRANSFER_READ,
        ),
    }
}

fn offscreen_sampled_stages() -> vk::PipelineStageFlags2 {
    vk::PipelineStageFlags2::FRAGMENT_SHADER | vk::PipelineStageFlags2::COMPUTE_SHADER
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
    let color_attachments = [
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA),
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::R | vk::ColorComponentFlags::G),
    ];
    let color_blend =
        vk::PipelineColorBlendStateCreateInfo::default().attachments(&color_attachments);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let color_formats = [OFFSCREEN_FORMAT, VELOCITY_FORMAT];
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

/// Запросы одного вида или вывода в пуле слота: `first` — начало, далее границы проходов.
#[derive(Clone, Copy)]
struct GpuMarks {
    pool: vk::QueryPool,
    first: u32,
}

impl GpuMarks {
    /// Пишет граничный timestamp `first + query` с стадией `ALL_COMMANDS`.
    /// Вызывать только вне `cmd_begin_rendering`…`cmd_end_rendering`.
    fn write(self, device: &ash::Device, command_buffer: vk::CommandBuffer, query: u32) {
        // SAFETY: the slot pool was reset when this command buffer began, and the index lies in this view's range.
        unsafe {
            device.cmd_write_timestamp2(
                command_buffer,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                self.pool,
                self.first + query,
            );
        }
    }
}

/// Записывает границу после прохода `pass`, если вид его выполняет.
fn mark_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    marks: Option<GpuMarks>,
    pass: GpuPass,
) {
    if let Some(marks) = marks {
        marks.write(device, command_buffer, 1 + pass as u32);
    }
}

/// Виды слота в порядке записи; индекс вида задаёт диапазон его запросов.
#[derive(Clone, Copy, Debug, Default)]
struct GpuProfileLayout {
    views: [Option<ProfiledView>; MAX_PROFILED_VIEWS],
    count: usize,
}

impl GpuProfileLayout {
    /// Выделяет запросы следующему виду; `None`, если лимит видов кадра исчерпан.
    fn add_view(&mut self, view: ProfiledView) -> Option<u32> {
        if self.count == MAX_PROFILED_VIEWS {
            return None;
        }
        self.views[self.count] = Some(view);
        let first = self.count as u32 * VIEW_QUERY_COUNT;
        self.count += 1;
        Some(first)
    }

    /// Разбирает прочитанные запросы слота. `None` — запрос не записан в кадр или не готов.
    fn decode(&self, queries: &[Option<u64>], valid_bits: u32, period_ns: f32) -> GpuFrameTimings {
        let mut timings = GpuFrameTimings::default();
        for (index, view) in self.views[..self.count].iter().enumerate() {
            let Some(view) = *view else {
                continue;
            };
            let first = index * VIEW_QUERY_COUNT as usize;
            let marks = &queries[first..first + VIEW_QUERY_COUNT as usize];
            timings.views[index] = decode_view(view, marks, valid_bits, period_ns);
        }
        let output = OUTPUT_QUERY_FIRST as usize;
        timings.output_ui = decode_output(&queries[output..output + 2], valid_bits, period_ns);
        timings
    }
}

/// Разбирает границы вида: `marks[0]` — начало, `marks[1 + pass]` — граница после прохода.
/// Пропущенная граница означает, что проход не выполнялся: его время попадает в следующий записанный.
fn decode_view(
    view: ProfiledView,
    marks: &[Option<u64>],
    valid_bits: u32,
    period_ns: f32,
) -> Option<GpuViewTimings> {
    let start = marks[0]?;
    let mut passes = [None; GpuPass::COUNT];
    let mut previous = start;
    for (pass, mark) in passes.iter_mut().zip(&marks[1..]) {
        if let Some(tick) = *mark {
            *pass = Some(timestamp_duration(previous, tick, valid_bits, period_ns));
            previous = tick;
        }
    }
    Some(GpuViewTimings {
        key: view,
        passes,
        total: timestamp_duration(start, previous, valid_bits, period_ns),
    })
}

/// Разбирает границы вывода на swapchain: начало и конец.
fn decode_output(marks: &[Option<u64>], valid_bits: u32, period_ns: f32) -> Option<Duration> {
    Some(timestamp_duration(
        marks[0]?, marks[1]?, valid_bits, period_ns,
    ))
}

/// Разбирает результаты `TYPE_64 | WITH_AVAILABILITY`: на запрос значение и флаг доступности.
/// Недоступный запрос даёт `None`, даже если в буфере осталось значение прошлой записи.
fn available_timestamps<const N: usize>(results: &[[u64; 2]; N]) -> [Option<u64>; N] {
    results.map(|[tick, available]| (available != 0).then_some(tick))
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

/// Байты `FrameUniforms` в порядке std140: девять матриц и девять `vec4`.
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

#[cfg(test)]
fn prepare_frame(
    frame: &Frame<'_>,
    render_extent: vk::Extent2D,
) -> Result<PreparedFrame, RenderError> {
    prepare_frame_with_temporal(frame, render_extent, None)
}

fn prepare_frame_with_temporal(
    frame: &Frame<'_>,
    render_extent: vk::Extent2D,
    temporal: Option<TemporalPlan>,
) -> Result<PreparedFrame, RenderError> {
    let shadow_requests = index_local_light_shadows(frame.lights, frame.local_light_shadows)?;
    let culling_padding = temporal.map_or([0.0; 2], |plan| {
        [plan.jitter_ndc[0].abs(), plan.jitter_ndc[1].abs()]
    });
    let lights = select_local_light_entries(frame.camera, frame.lights, culling_padding)?;
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
    let uniforms =
        frame_uniform_bytes_with_count(frame, lights.len(), depth_range, render_extent, temporal)?;
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
    Ok(select_local_light_entries(camera, lights, [0.0; 2])?
        .into_iter()
        .map(|selected| selected.light)
        .collect())
}

fn select_local_light_entries(
    camera: FrameCamera,
    lights: &[LocalLight],
    culling_padding: [f32; 2],
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
    let frustum = Frustum::from_view_projection_with_padding(view_projection, culling_padding);
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
    temporal: Option<TemporalPlan>,
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
    let previous_view_projection = temporal
        .map(|plan| plan.previous_view_projection)
        .unwrap_or(view_projection);
    values[160..176].copy_from_slice(previous_view_projection.as_flattened());
    let [jitter_x, jitter_y, lod_bias] = temporal.map_or([0.0; 3], |plan| {
        [plan.jitter_ndc[0], plan.jitter_ndc[1], plan.lod_bias]
    });
    values[176..180].copy_from_slice(&[
        jitter_x,
        jitter_y,
        lod_bias,
        frame.debug_view.code() as f32,
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

/// Push constants: model, normal matrix, previous model and one uvec4 of draw parameters.
fn push_constant_bytes(
    model: &[[f32; 4]; 4],
    previous_model: &[[f32; 4]; 4],
    lod_level: u8,
) -> [u8; PUSH_CONSTANTS_SIZE] {
    let normal_from_model = normal_matrix(model);
    let mut bytes = [0; PUSH_CONSTANTS_SIZE];
    bytes[..64].copy_from_slice(&float_bytes::<64>(model.as_flattened()));
    bytes[64..128].copy_from_slice(&float_bytes::<64>(normal_from_model.as_flattened()));
    bytes[128..192].copy_from_slice(&float_bytes::<64>(previous_model.as_flattened()));
    bytes[192..196].copy_from_slice(&(lod_level as u32).to_ne_bytes());
    bytes
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

fn usable_surface_extent(
    capabilities: vk::SurfaceCapabilitiesKHR,
    requested: vk::Extent2D,
) -> Option<vk::Extent2D> {
    if requested.width == 0 || requested.height == 0 {
        return None;
    }
    let extent = select_surface_extent(capabilities, requested);
    (extent.width != 0 && extent.height != 0).then_some(extent)
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
        CLUSTER_RECORD_GPU_SIZE, DEFAULT_UPLOAD_BATCH_CAPACITY, DebugView, DeviceFacts,
        DeviceReport, DirectionalLight, DrawCommand, FRAME_UNIFORMS_SIZE, FULLSCREEN_VERTEX_SHADER,
        Frame, FrameCamera, GpuLocalLight, IDENTITY_MATRIX, LDR_FORMAT, LOCAL_LIGHT_GPU_SIZE,
        LOCAL_SHADOW_MAP_RESOLUTION, LOCAL_SHADOW_MATRIX_BUFFER_SIZE, LocalLight, LocalLightShadow,
        LocalLightShadowMatrices, MATERIAL_UNIFORMS_SIZE, MAX_CLUSTER_LIGHTS,
        MAX_LOCAL_SHADOW_LAYERS, MAX_RENDERED_LIGHTS, MESH_BLEND_FRAGMENT_SHADER,
        MESH_MASK_FRAGMENT_SHADER, MESH_OPAQUE_FRAGMENT_SHADER, MESH_VERTEX_SHADER, Material,
        MaterialId, MeshId, NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON, OFFSCREEN_FORMAT,
        OUTPUT_FRAGMENT_SHADER, OVERLAY_FRAGMENT_SHADER, OVERLAY_VERTEX_SHADER, OverlayVertex,
        PIPELINE_CACHE_HEADER_SIZE, PUSH_CONSTANTS_SIZE, PipelineCacheIdentity, PresentMode,
        ProbeInstanceFailure, RenderError, Renderer, RendererConfig, SHADOW_CASCADE_COUNT,
        SHADOW_FRAGMENT_SHADER, SHADOW_MAP_RESOLUTION, SHADOW_VERTEX_SHADER, SWAPCHAIN_USAGE,
        TONEMAP_FRAGMENT_SHADER, TemporalBackend, TextureColorSpace, TextureId, Tonemapper,
        UI_FRAGMENT_SHADER, UI_VERTEX_SHADER, VELOCITY_FORMAT, Vertex, ViewSettings,
        align_upload_offset, best_device_index, check_extensions, classify_acquire_result,
        classify_probe_instance_failure, cluster_device_limit_reasons, deduplicate_names,
        derivative_tangent_basis, draw_rasterization_state, effective_view_settings,
        evaluate_device, frame_uniform_bytes, index_bytes, index_local_light_shadows,
        inverse_extent, local_light_bytes, local_light_bytes_for_selected,
        local_light_contribution, local_shadow_matrix_bytes, material_sampler_info,
        material_uniform_bytes, mesh_upload_layout, mip_extent, mip_level_count,
        missing_extensions, missing_offscreen_format_features, missing_post_format_features,
        missing_texture_format_features, missing_velocity_format_features, normal_matrix,
        offscreen_previous_use, offscreen_sampled_stages, overlay_vertex_bytes,
        point_shadow_face_index, push_constant_bytes, sampler_anisotropy, scaled_extent,
        scene_viewport, select_composite_alpha, select_local_light_entries, select_local_lights,
        select_local_shadow_slots, select_present_mode, select_surface_extent,
        select_temporal_backend, shadow_array_limits_suitable, shadow_caster_is_outside,
        shadow_casters, shadow_layout_source, shadow_subresource_range, texture_format,
        texture_format_reasons, texture_mip_blit_region, texture_mip_destination_shader_barrier,
        texture_mip_shader_barrier, texture_mip_source_barrier, texture_staging_size,
        texture_upload_barrier, timestamp_duration, transform_tangent_normal,
        upload_batch_capacity, upload_batch_flush_required, upload_data_layout,
        usable_surface_extent, validate_debug_view, validate_draws, validate_material,
        validate_mesh, validate_overlay, validate_pipeline_cache, validate_render_scale,
        validate_texture, validate_view_settings, vertex_bytes,
    };
    use super::{
        GpuPass, GpuProfileLayout, GpuViewTimings, LocalShadowKind, MAX_PROFILED_VIEWS,
        OUTPUT_QUERY_FIRST, ProfiledView, SelectedLocalLight, TIMESTAMP_QUERY_COUNT,
        VIEW_QUERY_COUNT, ViewportId, available_timestamps, decode_output, decode_view,
    };
    use ash::vk;
    use std::time::{Duration, Instant};
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
    fn unavailable_temporal_backends_fall_back_to_taa() {
        assert_eq!(
            select_temporal_backend(AntiAliasing::Fsr, false, false),
            TemporalBackend::Taa
        );
        assert_eq!(
            select_temporal_backend(AntiAliasing::Fsr, true, false),
            TemporalBackend::Fsr
        );
        assert_eq!(
            select_temporal_backend(AntiAliasing::Dlss, false, false),
            TemporalBackend::Taa
        );
        assert_eq!(
            select_temporal_backend(AntiAliasing::Dlss, false, true),
            TemporalBackend::Dlss
        );
    }

    #[test]
    fn fsr_delta_time_uses_last_submitted_frame_and_clamps_invalid_intervals() {
        let now = Instant::now();
        assert!((super::temporal_frame_delta_ms(None, now) - 1000.0 / 60.0).abs() < 1e-5);
        assert_eq!(
            super::temporal_frame_delta_ms(Some(now), now + Duration::from_millis(16)),
            16.0
        );
        assert_eq!(
            super::temporal_frame_delta_ms(Some(now), now + Duration::from_micros(100)),
            1.0
        );
        assert_eq!(
            super::temporal_frame_delta_ms(Some(now), now + Duration::from_secs(2)),
            1000.0
        );
    }

    #[cfg(windows)]
    #[test]
    fn fsr_camera_parameters_decode_vulkan_zero_to_one_projection() {
        let near = 0.1_f32;
        let far = 100.0_f32;
        let fov = std::f32::consts::FRAC_PI_3;
        let projection = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0 / (fov * 0.5).tan(), 0.0, 0.0],
            [0.0, 0.0, far / (near - far), -1.0],
            [0.0, 0.0, far * near / (near - far), 0.0],
        ];
        let parameters = super::fsr_camera_parameters(projection);
        assert!((parameters[0] - near).abs() < 1e-5);
        assert!((parameters[1] - far).abs() < 1e-3);
        assert!((parameters[2] - fov).abs() < 1e-5);
    }

    #[test]
    fn debug_view_shader_codes_are_stable_and_distinct() {
        assert_eq!(
            [
                DebugView::Lit.code(),
                DebugView::Wireframe.code(),
                DebugView::Normals.code(),
                DebugView::Overdraw.code(),
                DebugView::LodLevels.code(),
                DebugView::ShadowCascades.code(),
            ],
            [0, 1, 2, 3, 4, 5]
        );
        let extent = vk::Extent2D {
            width: 640,
            height: 360,
        };
        assert_eq!(
            super::temporal_key(extent, extent, AntiAliasing::Taa, DebugView::Lit.code())
                .debug_view,
            DebugView::Lit.code()
        );
        assert_eq!(
            super::temporal_key(
                extent,
                extent,
                AntiAliasing::None,
                DebugView::ShadowCascades.code()
            )
            .debug_view,
            DebugView::ShadowCascades.code()
        );
    }

    #[test]
    fn non_lit_debug_views_disable_aa_and_raw_views_bypass_tonemapping() {
        let settings = ViewSettings {
            render_scale: 0.5,
            exposure_ev: 2.0,
            tonemapper: Tonemapper::Aces,
            anti_aliasing: AntiAliasing::Taa,
        };
        assert_eq!(effective_view_settings(settings, DebugView::Lit), settings);
        let wireframe = effective_view_settings(settings, DebugView::Wireframe);
        assert_eq!(wireframe.anti_aliasing, AntiAliasing::None);
        assert_eq!(wireframe.exposure_ev, 2.0);
        assert_eq!(wireframe.tonemapper, Tonemapper::Aces);
        for debug_view in [
            DebugView::Normals,
            DebugView::Overdraw,
            DebugView::LodLevels,
            DebugView::ShadowCascades,
        ] {
            let view = effective_view_settings(settings, debug_view);
            assert_eq!(view.anti_aliasing, AntiAliasing::None);
            assert_eq!(view.exposure_ev, 0.0);
            assert_eq!(view.tonemapper, Tonemapper::None);
        }
    }

    #[test]
    fn wireframe_view_requires_optional_device_feature() {
        assert!(validate_debug_view(DebugView::Lit, false).is_ok());
        assert!(validate_debug_view(DebugView::Wireframe, true).is_ok());
        assert!(matches!(
            validate_debug_view(DebugView::Wireframe, false),
            Err(super::RenderError::InvalidConfig(_))
        ));
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
    fn velocity_format_requires_color_attachment_and_sampling() {
        let required =
            vk::FormatFeatureFlags::COLOR_ATTACHMENT | vk::FormatFeatureFlags::SAMPLED_IMAGE;
        assert_eq!(VELOCITY_FORMAT, vk::Format::R16G16_SFLOAT);
        assert!(missing_velocity_format_features(required).is_empty());
        for (feature, name) in [
            (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
            (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        ] {
            assert_eq!(
                missing_velocity_format_features(required & !feature),
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
            velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            max_push_constants_size: 256,
        };
        let evaluation = evaluate_device(&facts, &[]);
        assert_eq!(evaluation.reasons.len(), 1);
        assert!(evaluation.reasons[0].contains("R16G16B16A16_SFLOAT"));
        assert!(evaluation.reasons[0].contains("COLOR_ATTACHMENT_BLEND"));
    }

    #[test]
    fn device_report_rejects_push_constant_limit_below_draw_layout() {
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
                | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                | vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
                | vk::FormatFeatureFlags::STORAGE_IMAGE,
            velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            max_push_constants_size: 207,
        };
        let reasons = evaluate_device(&facts, &[]).reasons;
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("maxPushConstantsSize"));
        assert!(reasons[0].contains("208"));
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
            velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            max_push_constants_size: 256,
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
                vk::PipelineStageFlags2::FRAGMENT_SHADER | vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ
            )
        );
        assert_eq!(
            offscreen_sampled_stages(),
            vk::PipelineStageFlags2::FRAGMENT_SHADER | vk::PipelineStageFlags2::COMPUTE_SHADER
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
            velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            max_push_constants_size: 256,
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
            velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            max_push_constants_size: 256,
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
            velocity_format_features: vk::FormatFeatureFlags::COLOR_ATTACHMENT
                | vk::FormatFeatureFlags::SAMPLED_IMAGE,
            max_push_constants_size: 256,
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
                particles: &[],
                particle_step: 0.0,
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
                debug_view: DebugView::Lit,
                view: ViewSettings::default(),
                reset_history: false,
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
        let selected = select_local_light_entries(camera([0.0; 3]), &lights, [0.0; 2]).unwrap();
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
            previous_model: None,
            lod_level: 0,
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
            previous_model: None,
            lod_level: 0,
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
            previous_model: None,
            lod_level: 0,
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
            previous_model: None,
            lod_level: 0,
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
        let mut previous_model = IDENTITY;
        previous_model[3] = [2.0, 3.0, 4.0, 1.0];
        let bytes = push_constant_bytes(&model, &previous_model, 6);
        let values = floats(&bytes[..192]);
        assert_eq!(PUSH_CONSTANTS_SIZE, 208);
        assert_eq!(values.len(), 48);
        assert_eq!(&values[..4], &[2.0, 0.0, 0.0, 0.0]);
        assert_eq!(&values[12..16], &[5.0, 6.0, 7.0, 1.0]);
        // Матрица нормалей пропорциональна обратной транспонированной: масштаб X делит нормаль X.
        assert_eq!(&values[16..20], &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(&values[20..24], &[0.0, 2.0, 0.0, 0.0]);
        assert_eq!(&values[32..36], &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(&values[44..48], &[2.0, 3.0, 4.0, 1.0]);
        assert_eq!(u32::from_ne_bytes(bytes[192..196].try_into().unwrap()), 6);
        assert_eq!(&bytes[196..], &[0; 12]);
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
            particles: &[],
            particle_step: 0.0,
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
            debug_view: DebugView::Lit,
            view: ViewSettings::default(),
            reset_history: false,
        };
        let values = floats(&frame_uniform_bytes(&frame).unwrap());
        assert_eq!(FRAME_UNIFORMS_SIZE, 720);
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
        assert_eq!(&values[160..176], view_projection.as_flattened());
        assert_eq!(&values[176..180], &[0.0; 4]);

        let normals = Frame {
            debug_view: DebugView::Normals,
            ..frame
        };
        assert_eq!(floats(&frame_uniform_bytes(&normals).unwrap())[179], 2.0);

        let no_shadows = Frame {
            shadow_cascades: &[],
            ..frame
        };
        assert_eq!(floats(&frame_uniform_bytes(&no_shadows).unwrap())[108], 0.0);
    }

    #[test]
    fn temporal_frame_uniforms_pack_previous_unjittered_view_and_render_parameters() {
        let current = perspective_projection();
        let mut previous = current;
        previous[3][0] += 0.25;
        let key = super::temporal::TemporalKey {
            render_extent: super::temporal::Extent {
                width: 640,
                height: 360,
            },
            output_extent: super::temporal::Extent {
                width: 1280,
                height: 720,
            },
            anti_aliasing: AntiAliasing::Taa,
            debug_view: 0,
        };
        let mut state = super::temporal::TemporalState::default();
        let first = state.plan(key, previous, false);
        state.submitted(first);
        let plan = state.plan(key, current, false);
        let frame = Frame {
            particles: &[],
            particle_step: 0.0,
            clear_color: [0.0; 4],
            camera: FrameCamera {
                view: IDENTITY,
                projection: current,
                position: [0.0; 3],
            },
            environment: None,
            shadow_cascades: &[],
            light: DirectionalLight::default(),
            lights: &[],
            local_light_shadows: &[],
            draws: &[],
            overlay: &[],
            debug_view: DebugView::Lit,
            view: ViewSettings {
                anti_aliasing: AntiAliasing::Taa,
                ..ViewSettings::default()
            },
            reset_history: false,
        };
        let uniforms = super::prepare_frame_with_temporal(
            &frame,
            vk::Extent2D {
                width: 640,
                height: 360,
            },
            Some(plan),
        )
        .unwrap()
        .uniforms;
        let values = floats(&uniforms);
        assert_eq!(&values[0..16], current.as_flattened());
        assert_eq!(&values[160..176], previous.as_flattened());
        assert_eq!(
            &values[176..180],
            &[plan.jitter_ndc[0], plan.jitter_ndc[1], -1.0, 0.0]
        );
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
                        particles: &[],
                        particle_step: 0.0,
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
                        debug_view: DebugView::Lit,
                        view: ViewSettings::default(),
                        reset_history: false,
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
            particles: &[],
            particle_step: 0.0,
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
            debug_view: DebugView::Lit,
            view: ViewSettings::default(),
            reset_history: false,
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
            particles: &[],
            particle_step: 0.0,
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
            debug_view: DebugView::Lit,
            view: ViewSettings::default(),
            reset_history: false,
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
        assert_eq!(AntiAliasing::Taa.code(), 2);
        assert_eq!(AntiAliasing::Fsr.code(), 3);
        assert_eq!(AntiAliasing::Dlss.code(), 4);
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
    fn usable_surface_extent_skips_zero_and_resolves_fixed_or_clamped_sizes() {
        let variable = vk::SurfaceCapabilitiesKHR {
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
        for requested in [
            vk::Extent2D {
                width: 0,
                height: 720,
            },
            vk::Extent2D {
                width: 1280,
                height: 0,
            },
            vk::Extent2D {
                width: 0,
                height: 0,
            },
        ] {
            assert_eq!(usable_surface_extent(variable, requested), None);
        }
        assert_eq!(
            usable_surface_extent(
                variable,
                vk::Extent2D {
                    width: 50,
                    height: 900,
                }
            ),
            Some(vk::Extent2D {
                width: 100,
                height: 600,
            })
        );

        let fixed = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: 1920,
                height: 1080,
            },
            ..variable
        };
        assert_eq!(
            usable_surface_extent(
                fixed,
                vk::Extent2D {
                    width: 1280,
                    height: 720,
                }
            ),
            Some(vk::Extent2D {
                width: 1920,
                height: 1080,
            })
        );

        let fixed_zero = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: 1920,
                height: 0,
            },
            ..fixed
        };
        assert_eq!(
            usable_surface_extent(
                fixed_zero,
                vk::Extent2D {
                    width: 1280,
                    height: 720,
                }
            ),
            None
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

    fn ns(value: u64) -> Duration {
        Duration::from_nanos(value)
    }

    fn passes_sum(view: &GpuViewTimings) -> Duration {
        view.passes.iter().flatten().sum()
    }

    #[test]
    fn view_timings_split_ticks_between_recorded_boundaries() {
        // Начало, затем граница после каждого из десяти проходов в порядке выполнения.
        let marks = [
            Some(100),
            Some(110),
            Some(125),
            Some(140),
            Some(150),
            Some(160),
            Some(175),
            Some(190),
            Some(195),
            Some(200),
            Some(215),
        ];
        let view = decode_view(ProfiledView::Screen, &marks, 64, 1.0).unwrap();

        assert_eq!(view.key, ProfiledView::Screen);
        assert_eq!(
            view.passes,
            [
                Some(ns(10)),
                Some(ns(15)),
                Some(ns(15)),
                Some(ns(10)),
                Some(ns(10)),
                Some(ns(15)),
                Some(ns(15)),
                Some(ns(5)),
                Some(ns(5)),
                Some(ns(15)),
            ]
        );
        assert_eq!(view.total, ns(115));
        assert_eq!(passes_sum(&view), view.total);
    }

    #[test]
    fn view_timings_skip_unexecuted_passes_and_keep_sum() {
        // ShadowDirectional и Wireframe не выполнялись: их границ в кадре нет.
        let marks = [
            Some(100),
            Some(110),
            None,
            Some(140),
            Some(150),
            Some(160),
            Some(175),
            None,
            Some(200),
            Some(215),
        ];
        let view = decode_view(ProfiledView::Screen, &marks, 64, 1.0).unwrap();

        assert_eq!(view.passes[0], Some(ns(10)));
        assert_eq!(view.passes[1], None);
        assert_eq!(
            view.passes[2],
            Some(ns(30)),
            "ShadowLocal считается от Clusters"
        );
        assert_eq!(view.passes[6], None);
        assert_eq!(
            view.passes[7],
            Some(ns(25)),
            "TemporalResolve считается от Transparent"
        );
        assert_eq!(view.total, ns(115));
        assert_eq!(passes_sum(&view), view.total);
    }

    #[test]
    fn view_timings_are_absent_without_start_boundary() {
        let marks = [
            None,
            Some(110),
            Some(125),
            Some(140),
            Some(150),
            Some(160),
            Some(175),
            Some(190),
            Some(200),
            Some(215),
        ];
        assert_eq!(decode_view(ProfiledView::Screen, &marks, 64, 1.0), None);
    }

    #[test]
    fn view_timings_handle_counter_wrap_within_valid_bits() {
        // Счётчик на 36 бит переполняется между началом и первым проходом.
        let max = (1_u64 << 36) - 1;
        let marks = [
            Some(max - 9),
            Some(5),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(20),
        ];
        let view = decode_view(ProfiledView::Screen, &marks, 36, 1.0).unwrap();

        assert_eq!(view.passes[0], Some(ns(15)));
        assert_eq!(view.passes[8], Some(ns(15)));
        assert_eq!(view.total, ns(30));
        assert_eq!(passes_sum(&view), view.total);
    }

    #[test]
    fn output_timings_need_both_boundaries() {
        assert_eq!(decode_output(&[Some(10), Some(35)], 64, 2.0), Some(ns(50)));
        assert_eq!(decode_output(&[Some(10), None], 64, 1.0), None);
        assert_eq!(decode_output(&[None, Some(35)], 64, 1.0), None);
    }

    #[test]
    fn unavailable_timestamps_are_absent_even_with_stale_values() {
        // Значение 99 осталось в буфере от прошлой записи и не должно попасть в разбор.
        let results = [[10, 1], [99, 0], [30, 1]];

        assert_eq!(available_timestamps(&results), [Some(10), None, Some(30)]);
    }

    #[test]
    fn all_unavailable_timestamps_are_absent() {
        assert_eq!(
            available_timestamps(&[[10, 0], [20, 0], [0, 0]]),
            [None, None, None]
        );
    }

    #[test]
    fn available_timestamps_keep_every_tick_including_zero() {
        assert_eq!(
            available_timestamps(&[[1, 1], [0, 1], [3, 7]]),
            [Some(1), Some(0), Some(3)]
        );
    }

    #[test]
    fn layout_decodes_each_view_with_the_keys_it_recorded() {
        let mut layout = GpuProfileLayout::default();
        let recorded = [3, 1, 7, 2].map(|id| ProfiledView::Viewport(ViewportId(id)));
        for (index, view) in recorded.into_iter().enumerate() {
            assert_eq!(layout.add_view(view), Some(index as u32 * VIEW_QUERY_COUNT));
        }
        assert_eq!(
            layout.add_view(ProfiledView::Screen),
            None,
            "пятый вид не профилируется"
        );

        let mut queries = [None; TIMESTAMP_QUERY_COUNT as usize];
        for index in 0..MAX_PROFILED_VIEWS {
            let first = index * VIEW_QUERY_COUNT as usize;
            queries[first] = Some(0);
            queries[first + GpuPass::COUNT] = Some(10 * (index as u64 + 1));
        }
        let timings = layout.decode(&queries, 64, 1.0);

        for (index, view) in recorded.into_iter().enumerate() {
            let timing = timings.views[index].expect("вид записан в кадр");
            assert_eq!(timing.key, view);
            assert_eq!(timing.total, ns(10 * (index as u64 + 1)));
        }
        assert_eq!(timings.output_ui, None);
        assert_eq!(timings.first_view_total(), Some(ns(10)));
    }

    #[test]
    fn layout_keeps_keys_of_the_frame_after_viewport_set_changes() {
        // Набор viewport сменился: в кадре записан один вид, ключ берётся из записи этого кадра.
        let mut layout = GpuProfileLayout::default();
        assert_eq!(
            layout.add_view(ProfiledView::Viewport(ViewportId(5))),
            Some(0)
        );
        let mut queries = [None; TIMESTAMP_QUERY_COUNT as usize];
        queries[0] = Some(0);
        queries[GpuPass::COUNT] = Some(40);

        let timings = layout.decode(&queries, 64, 1.0);

        assert_eq!(
            timings.views[0].map(|view| view.key),
            Some(ProfiledView::Viewport(ViewportId(5)))
        );
        assert_eq!(timings.views[1], None);
    }

    #[test]
    fn query_ranges_of_views_and_output_cover_pool_without_overlap() {
        assert_eq!(TIMESTAMP_QUERY_COUNT, 46);
        let mut owner = [None::<usize>; TIMESTAMP_QUERY_COUNT as usize];
        let mut layout = GpuProfileLayout::default();
        for index in 0..MAX_PROFILED_VIEWS {
            let first = layout
                .add_view(ProfiledView::Viewport(ViewportId(index)))
                .unwrap();
            for query in first..first + VIEW_QUERY_COUNT {
                assert!(owner[query as usize].replace(index).is_none());
            }
        }
        for query in OUTPUT_QUERY_FIRST..TIMESTAMP_QUERY_COUNT {
            assert!(owner[query as usize].replace(MAX_PROFILED_VIEWS).is_none());
        }
        assert!(owner.iter().all(Option::is_some));
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
