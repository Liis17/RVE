//! Публичные типы данных сцены и кадра: вершины, материалы, свет, камера и настройки вида.

use crate::*;

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
pub struct MeshId(pub(crate) usize);

/// Идентификатор текстуры, созданной [`Renderer::create_texture`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TextureId(pub(crate) usize);

/// Идентификатор неизменяемого материала, созданного [`Renderer::create_material`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MaterialId(pub(crate) usize);

/// Identifier for a runtime-compiled surface shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShaderId(pub(crate) usize);

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
    /// User surface shader; `None` selects the built-in Lit surface.
    pub shader: Option<ShaderId>,
    /// User numeric properties packed by `engine-scene` shader layout.
    pub user_values: [[f32; 4]; 16],
    /// User texture properties indexed by shader texture slot.
    pub user_textures: [Option<TextureId>; 8],
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
            shader: None,
            user_values: [[0.0; 4]; 16],
            user_textures: [None; 8],
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
    /// Экземпляр skinning меша и палитра кадра; `None` для статического меша.
    pub skin: Option<SkinBinding>,
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
pub struct ParticleEmitterId(pub(crate) usize);

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
/// отключает оверлей. `ui` — игровой UI вида, который рисуется сразу после `overlay`
/// внутри того же output pass, без своего render pass; пустой срез не создаёт работы.
/// `UiImage::Viewport` в `ui` запрещён: результат другого вида в этот момент не готов.
/// `view` задаёт render scale, экспозицию, tonemapper и сглаживание.
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
    /// Палитры skinning кадра: матрицы костей по столбцам, одна палитра на `SkinBinding::palette`.
    pub joint_palettes: &'a [&'a [[[f32; 4]; 4]]],
    pub overlay: &'a [OverlayVertex],
    /// Игровой UI вида: только `UiImage::Texture`, рисуется поверх `overlay`.
    pub ui: &'a [UiPrimitive<'a>],
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
    /// Экспозиция, tonemapper и эффекты постобработки вида.
    pub post: PostProcess,
    pub anti_aliasing: AntiAliasing,
    /// Пользовательские экранные шейдеры вида; работают только при `DebugView::Lit`.
    pub screen_shaders: ScreenShaders,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            render_scale: 1.0,
            post: PostProcess::default(),
            anti_aliasing: AntiAliasing::Fxaa,
            screen_shaders: ScreenShaders::default(),
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
    pub(crate) const fn code(self) -> u32 {
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

    pub(crate) const fn has_linear_output(self) -> bool {
        matches!(
            self,
            Self::Normals | Self::Overdraw | Self::LodLevels | Self::ShadowCascades
        )
    }
}

impl AntiAliasing {
    pub(crate) const fn code(self) -> u32 {
        match self {
            Self::Fxaa => 0,
            Self::None => 1,
            Self::Taa => 2,
            Self::Fsr => 3,
            Self::Dlss => 4,
        }
    }

    pub(crate) const fn is_temporal(self) -> bool {
        matches!(self, Self::Taa | Self::Fsr | Self::Dlss)
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
    pub(crate) fn vk(self) -> vk::PresentModeKHR {
        match self {
            Self::Fifo => vk::PresentModeKHR::FIFO,
            Self::Mailbox => vk::PresentModeKHR::MAILBOX,
            Self::Immediate => vk::PresentModeKHR::IMMEDIATE,
        }
    }

    pub(crate) fn from_vk(mode: vk::PresentModeKHR) -> Self {
        match mode {
            vk::PresentModeKHR::FIFO => Self::Fifo,
            vk::PresentModeKHR::MAILBOX => Self::Mailbox,
            vk::PresentModeKHR::IMMEDIATE => Self::Immediate,
            _ => Self::Fifo,
        }
    }
}
