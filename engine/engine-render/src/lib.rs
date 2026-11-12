//! Vulkan-рендерер, форматы изображений и материалы сцены.

#[cfg(windows)]
use ash::vk::Handle as _;
use ash::{Entry, vk};

/// Версия крейта (SemVer).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod environment;
mod latency;
mod skin;
pub mod surface_shader;

pub use latency::{LatencyHandle, LatencyMarker, LowLatencyMode};
pub use skin::{SkinBinding, SkinId, SkinVertex};
pub use surface_shader::{
    CompiledSurfaceShader, ShaderCompileError, SurfaceLighting, SurfaceProperty,
    SurfacePropertyKind, SurfaceShaderSource, SurfaceTextureFallback, compile_surface_shader,
};

use latency::{PresentIdTracker, Shared, VulkanLatency, low_latency_device_extensions};

use clusters::{ClusterBounds, DepthRange, GpuLocalLight, camera_cluster_bounds};
use culling::{Frustum, LocalBounds, back_to_front_order};
pub use environment::{EnvironmentId, EnvironmentSource, ProceduralSky};
use raw_window_handle::{HandleError, HasDisplayHandle, HasWindowHandle};
use std::cell::Cell;
use std::collections::HashSet;
use std::error::Error;
use std::ffi::{CStr, CString};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use temporal::{Extent as TemporalExtent, TemporalKey, TemporalPlan, TemporalState};
use ui::{
    PackedUi, RetireQueue, RetiredSwapchains, UiDraw, is_srgb_format, validate_frame_ui,
    validate_texture_region, validate_viewport_size,
};

mod ambient_occlusion;
mod bloom;
mod clusters;
mod color_lut;
mod culling;
mod exposure;
mod filters;
mod occlusion;
mod particle_gpu;
mod particles;
mod post;
mod screen_shader;

use ambient_occlusion::{AoPipelines, AoRecord, AoTarget, record_ambient_occlusion};
use bloom::{
    BloomChain, BloomPipelines, BloomRecord, bloom_record, record_bloom, write_composite_bloom,
};
use color_lut::{
    COLOR_LUT_FORMAT, IDENTITY_LUT_SIZE, color_lut_bytes, identity_color_lut, validate_color_lut,
    write_composite_lut,
};
pub use color_lut::{MAX_COLOR_LUT_SIZE, MIN_COLOR_LUT_SIZE};
#[cfg(windows)]
use exposure::{EXPOSURE_SCALE_OFFSET, buffer_barrier as exposure_buffer_barrier};
use exposure::{
    ExposureFrame, ExposureHistory, ExposurePipelines, ExposureResources, ExposureTarget,
    ViewExposure, exposure_record, prepare_view_exposure, record_exposure,
    write_composite_exposure,
};
use filters::{FilterPipelines, FilterRecord, FilterScratch, record_filters};
use occlusion::{OcclusionPipelines, OcclusionTarget};
use particle_gpu::{
    ParticleEmitter, ParticlePass, ParticleSystem, particle_depth_source_masks, record_particles,
};
pub use particles::{
    MAX_ADDITIVE_PARTICLE_CAPACITY, MAX_ALPHA_PARTICLE_CAPACITY, MAX_PARTICLE_CURVE_KEYS,
    MAX_PARTICLE_STEP, ParticleBlend, ParticleBurst, ParticleCollision, ParticleColorKey,
    ParticleEmitterDesc, ParticleFlipbook, ParticleShape, ParticleSizeKey,
};
pub use post::{
    AmbientOcclusion, AmbientOcclusionQuality, Bloom, ChromaticAberration, ColorGrading,
    ColorLutId, DepthOfField, Exposure, ExposureMode, FilmGrain, MotionBlur, PostProcess, Vignette,
};
use post::{COMPOSITE_PUSH_CONSTANTS_SIZE, OUTPUT_PUSH_CONSTANTS_SIZE, PostFrame};
#[cfg(any(test, all(windows, feature = "dlss")))]
mod dlss;
#[allow(dead_code)]
mod fsr;
mod temporal;
mod ui;

include!(concat!(env!("OUT_DIR"), "/render_constants.rs"));

pub use screen_shader::{
    SCREEN_SHADER_MAX_PASSES, SCREEN_SHADER_PARAM_COUNT, ScreenShaderError, ScreenShaderId,
    ScreenShaderPass, ScreenShaderStage, ScreenShaders, compile_screen_shader,
};
use screen_shader::{
    ScreenChainRecord, ScreenContext, ScreenShaderLayout, ScreenShaderPipelines,
    ScreenShaderScratch, chain_record, prepare_screen_scratch, record_screen_chain,
};
pub use ui::{UiImage, UiPrimitive, UiTextureId, UiVertex, ViewportId};

mod capture;
mod config;
mod error;
mod frame;
mod init;
mod lights;
mod math;
mod passes;
mod physical_device;
mod pipelines;
mod profile;
mod resources;
mod swapchain;
mod teardown;
mod temporal_views;
mod types;
mod validation;

pub use capture::*;
pub use config::*;
pub use error::*;
pub use frame::*;
use lights::*;
use math::*;
use passes::*;
pub use physical_device::*;
use pipelines::*;
pub use profile::*;
use resources::*;
use swapchain::*;
use temporal_views::*;
pub use types::*;
use validation::*;

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
const SKIN_COMPUTE_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/skin.comp.spv"));
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
const BLACK_TEXTURE: usize = 3;
const DEFAULT_MATERIAL: usize = 0;
/// Push constants mesh- и shadow-проходов: `uvec4 params` (LOD и слой тени).
const PUSH_CONSTANTS_SIZE: usize = size_of::<[u32; 4]>();
/// Данные экземпляра draw'а в вершинном буфере кадра: model, три столбца матрицы
/// нормалей и model прошлого кадра (`mesh.vert`, локации 5–15; `shadow.vert` читает model).
const INSTANCE_STRIDE: usize = MESH_INSTANCE_VECTORS as usize * size_of::<[f32; 4]>();
/// Число `vec4` в данных экземпляра.
const MESH_INSTANCE_VECTORS: u32 = 4 + 3 + 4;
/// Привязка данных экземпляров в mesh-пайплайнах (после текущих и прошлых вершин).
const MESH_INSTANCE_BINDING: u32 = 2;
/// Привязка данных экземпляров в shadow-пайплайнах.
const SHADOW_INSTANCE_BINDING: u32 = 1;
const FRAME_UNIFORMS_SIZE: usize =
    (5 + SHADOW_CASCADE_COUNT) * size_of::<[[f32; 4]; 4]>() + 9 * size_of::<[f32; 4]>();
const MATERIAL_UNIFORMS_SIZE: usize = 4 * size_of::<[f32; 4]>();
const USER_MATERIAL_UNIFORMS_SIZE: usize = 16 * size_of::<[f32; 4]>();
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
    capture_row_alignment: u64,
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
    skin_set_layout: Option<vk::DescriptorSetLayout>,
    skin_pipeline_layout: Option<vk::PipelineLayout>,
    skin_pipeline: Option<vk::Pipeline>,
    /// Экземпляры skinning по `SkinId`; освобождённые места переиспользуются.
    skin_instances: Vec<Option<skin::SkinInstance>>,
    /// Номер последнего отправленного вызова с skinning; нужен для предыдущей позы.
    skin_call_serial: u64,
    post_pipeline_layout: Option<vk::PipelineLayout>,
    bloom_pipelines: Option<BloomPipelines>,
    exposure_pipelines: Option<ExposurePipelines>,
    filter_pipelines: Option<FilterPipelines>,
    ao_pipelines: Option<AoPipelines>,
    /// Hi-Z и тест видимости для occlusion culling видов.
    occlusion_pipelines: Option<OcclusionPipelines>,
    /// Вариант непрозрачного пайплайна с LESS_OR_EQUAL для прохода после prepass.
    opaque_equal_pipeline: Option<vk::Pipeline>,
    /// Depth-only пайплайн непрозрачных мешей для prepass.
    prepass_pipeline: Option<vk::Pipeline>,
    taa_set_layout: Option<vk::DescriptorSetLayout>,
    taa_pipeline_layout: Option<vk::PipelineLayout>,
    taa_pipeline: Option<vk::Pipeline>,
    tonemap_pipeline: Option<vk::Pipeline>,
    output_pipeline: Option<vk::Pipeline>,
    viewport_output_pipeline: Option<vk::Pipeline>,
    /// Пайплайны экранных шейдеров по `ScreenShaderId`; `None` — шейдер уничтожен.
    screen_shaders: Vec<Option<ScreenShaderPipelines>>,
    /// 3D LUT цветокоррекции по `ColorLutId`; индекс 0 — встроенная identity LUT, `None` — уничтожена.
    color_luts: Vec<Option<DeviceImage>>,
    /// Layout экранных проходов; создаётся вместе с первым шейдером.
    screen_shader_layout: Option<ScreenShaderLayout>,
    overlay_pipeline_layout: Option<vk::PipelineLayout>,
    overlay_pipeline: Option<vk::Pipeline>,
    ui_pipeline_layout: Option<vk::PipelineLayout>,
    ui_pipeline: Option<vk::Pipeline>,
    /// UI игрового вида: тот же layout, формат цвета viewport target.
    viewport_ui_pipeline: Option<vk::Pipeline>,
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
    /// Устройство поддерживает multi-draw indirect с `firstInstance`; без этого occlusion
    /// culling не создаётся.
    indirect_instancing: bool,
    frame_set_layout: Option<vk::DescriptorSetLayout>,
    material_set_layout: Option<vk::DescriptorSetLayout>,
    environment_set_layout: Option<vk::DescriptorSetLayout>,
    compute_set_layout: Option<vk::DescriptorSetLayout>,
    texture_set_layout: Option<vk::DescriptorSetLayout>,
    /// Набор постобработки: HDR-источник, sampler и bloom для composite.
    post_set_layout: Option<vk::DescriptorSetLayout>,
    frame_descriptor_pool: Option<vk::DescriptorPool>,
    post_descriptor_pool: Option<vk::DescriptorPool>,
    frame_uniforms: Vec<FrameUniforms>,
    sampler: Option<vk::Sampler>,
    /// Нулевой user-UBO, общий для материалов без шейдера; живёт, пока есть материалы, и уходит с renderer.
    builtin_user_uniform: Option<Arc<HostBuffer>>,
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
    /// Индекс — `ShaderId`; surface shaders имеют отдельные pipeline variants.
    surface_shaders: Vec<Option<GpuSurfaceShader>>,
    ui_textures: Vec<Option<UiTexture>>,
    viewports: Vec<Option<Viewport>>,
    screen_temporal: TemporalView,
    retired: RetireQueue<RetiredResource>,
    retired_swapchains: RetiredSwapchains<SwapchainBundle>,
    pending_swapchain_drop: Option<SwapchainBundle>,
    frames: Vec<FrameResources>,
    captures: Vec<CaptureRecord>,
    next_capture_ticket: u64,
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
    latency: Option<Arc<Shared>>,
    latency_semaphore: vk::Semaphore,
    latency_frame: u64,
    present_ids: PresentIdTracker,
    surface_format: vk::SurfaceFormatKHR,
    present_mode: PresentMode,
    /// Номер кадра renderer; задаёт сид зерна постобработки.
    frame_serial: u32,
    last_warned_present_mode: Option<PresentMode>,
    output_extent: vk::Extent2D,
    swapchain_dirty: bool,
    frame_slots_lost: bool,
    config: RendererConfig,
    capabilities: Capabilities,
    window: Arc<dyn WindowHandleOwner>,
}

impl Renderer {
    /// Handle для Reflex-сна и маркеров симуляции; `None`, если Reflex выключен или не поддержан.
    pub fn latency_handle(&self) -> Option<LatencyHandle> {
        self.latency.as_ref().map(|shared| LatencyHandle {
            shared: Arc::clone(shared),
        })
    }

    /// Задаёт `snapshot.sequence` кадра, который следующий `render_frame*` отправит с present id.
    pub fn set_latency_frame(&mut self, frame_id: u64) {
        self.latency_frame = frame_id;
    }

    /// Выбирает present id кадра и ставит `RENDERSUBMIT_START`; 0 — кадр без Reflex-маркеров.
    fn begin_latency_frame(&mut self) -> u64 {
        let Some(latency) = self.latency.as_ref() else {
            return 0;
        };
        let Some(present_id) = self.present_ids.begin(self.latency_frame) else {
            return 0;
        };
        latency.marker(present_id, vk::LatencyMarkerNV::RENDERSUBMIT_START);
        present_id
    }

    fn latency_marker(&self, present_id: u64, marker: vk::LatencyMarkerNV) {
        if let Some(latency) = self.latency.as_ref() {
            latency.marker(present_id, marker);
        }
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

    fn post_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.post_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов постобработки отсутствует",
        ))
    }

    fn taa_set_layout(&self) -> Result<vk::DescriptorSetLayout, RenderError> {
        self.taa_set_layout.ok_or(RenderError::InvalidState(
            "layout дескрипторов TAA отсутствует",
        ))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fxaa_reference_tests;
