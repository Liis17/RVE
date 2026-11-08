//! Постобработка вида: настройки эффектов, план проходов и упаковка push constants.
//!
//! Модуль не зависит от Vulkan: план, упаковка параметров и проверки проверяются unit-тестами
//! без GPU. Выполнение проходов остаётся в `lib.rs` и использует результаты этого модуля.

use crate::{DebugView, Tonemapper};

/// Число `vec4` в push constants composite-прохода (tonemap.frag).
pub const COMPOSITE_PUSH_VECTORS: usize = 6;
/// Размер push constants composite-прохода в байтах.
pub const COMPOSITE_PUSH_CONSTANTS_SIZE: usize = COMPOSITE_PUSH_VECTORS * 16;
/// Число `vec4` в push constants output-прохода (output.frag).
pub const OUTPUT_PUSH_VECTORS: usize = 2;
/// Размер push constants output-прохода в байтах.
pub const OUTPUT_PUSH_CONSTANTS_SIZE: usize = OUTPUT_PUSH_VECTORS * 16;
/// Верхняя граница числа mip-уровней bloom.
pub const MAX_BLOOM_MIPS: u32 = 6;
/// Минимальный размер меньшей стороны, при котором bloom строит ещё один уровень.
const MIN_BLOOM_MIP_SIDE: u32 = 8;

const _: () = assert!(COMPOSITE_PUSH_CONSTANTS_SIZE <= 128);
const _: () = assert!(OUTPUT_PUSH_CONSTANTS_SIZE <= COMPOSITE_PUSH_CONSTANTS_SIZE);

/// Все настройки постобработки одного вида.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PostProcess {
    pub exposure: Exposure,
    pub tonemapper: Tonemapper,
    pub bloom: Bloom,
    pub ambient_occlusion: AmbientOcclusion,
    pub color_grading: ColorGrading,
    pub vignette: Vignette,
    pub chromatic_aberration: ChromaticAberration,
    pub film_grain: FilmGrain,
    pub depth_of_field: DepthOfField,
    pub motion_blur: MotionBlur,
}

impl Default for PostProcess {
    /// Значения по умолчанию совпадают с прежним цветовым путём: EV 0, ACES, без эффектов.
    fn default() -> Self {
        Self {
            exposure: Exposure::default(),
            tonemapper: Tonemapper::Aces,
            bloom: Bloom::default(),
            ambient_occlusion: AmbientOcclusion::default(),
            color_grading: ColorGrading::default(),
            vignette: Vignette::default(),
            chromatic_aberration: ChromaticAberration::default(),
            film_grain: FilmGrain::default(),
            depth_of_field: DepthOfField::default(),
            motion_blur: MotionBlur::default(),
        }
    }
}

impl PostProcess {
    /// Проверяет диапазоны всех параметров; сообщение годится для `RenderError::InvalidConfig`.
    pub fn validate(&self) -> Result<(), &'static str> {
        let exposure = self.exposure;
        if !in_range(exposure.ev, -16.0, 16.0) {
            return Err("exposure_ev должен быть конечным и находиться в [-16, 16]");
        }
        if !(in_range(exposure.min_ev, -16.0, 16.0)
            && in_range(exposure.max_ev, -16.0, 16.0)
            && exposure.min_ev <= exposure.max_ev)
        {
            return Err("exposure min_ev и max_ev должны быть в [-16, 16] и min_ev <= max_ev");
        }
        if !(positive(exposure.speed_up) && positive(exposure.speed_down)) {
            return Err("скорости адаптации экспозиции должны быть конечными и > 0");
        }
        if !(in_range(exposure.low_percent, 0.0, 100.0)
            && in_range(exposure.high_percent, 0.0, 100.0)
            && exposure.low_percent < exposure.high_percent)
        {
            return Err("перцентили экспозиции требуют 0 <= low < high <= 100");
        }
        let bloom = self.bloom;
        if !non_negative(bloom.intensity) || !non_negative(bloom.threshold) {
            return Err("bloom intensity и threshold должны быть конечными и >= 0");
        }
        if !(in_range(bloom.knee, 0.0, 1.0) && in_range(bloom.scatter, 0.0, 1.0)) {
            return Err("bloom knee и scatter должны находиться в [0, 1]");
        }
        if !bloom.tint.into_iter().all(non_negative) {
            return Err("bloom tint должен содержать конечные компоненты >= 0");
        }
        let ao = self.ambient_occlusion;
        if !positive(ao.radius) {
            return Err("ambient occlusion radius должен быть конечным и > 0");
        }
        if !(in_range(ao.intensity, 0.0, 4.0) && in_range(ao.falloff, 0.0, 1.0)) {
            return Err("ambient occlusion intensity в [0, 4], falloff в [0, 1]");
        }
        let grading = self.color_grading;
        if !(in_range(grading.temperature, -1.0, 1.0) && in_range(grading.tint, -1.0, 1.0)) {
            return Err("color grading temperature и tint должны находиться в [-1, 1]");
        }
        if !(in_range(grading.contrast, 0.0, 2.0) && in_range(grading.saturation, 0.0, 2.0)) {
            return Err("color grading contrast и saturation должны находиться в [0, 2]");
        }
        if !grading
            .lift
            .into_iter()
            .all(|value| in_range(value, -1.0, 1.0))
            || !grading
                .gamma
                .into_iter()
                .all(|value| in_range(value, 0.001, 4.0))
            || !grading
                .gain
                .into_iter()
                .all(|value| in_range(value, 0.0, 4.0))
        {
            return Err("color grading lift, gamma и gain вне допустимых диапазонов");
        }
        let vignette = self.vignette;
        if !(in_range(vignette.intensity, 0.0, 1.0)
            && in_range(vignette.smoothness, 0.0, 1.0)
            && in_range(vignette.roundness, 0.0, 1.0))
            || !vignette
                .color
                .into_iter()
                .all(|value| in_range(value, 0.0, 1.0))
        {
            return Err("vignette параметры должны находиться в [0, 1]");
        }
        if !in_range(self.chromatic_aberration.intensity, 0.0, 1.0) {
            return Err("chromatic aberration intensity должен находиться в [0, 1]");
        }
        let grain = self.film_grain;
        if !(in_range(grain.intensity, 0.0, 1.0) && in_range(grain.response, 0.0, 1.0)) {
            return Err("film grain intensity и response должны находиться в [0, 1]");
        }
        let dof = self.depth_of_field;
        if !(positive(dof.focus_distance)
            && positive(dof.aperture)
            && positive(dof.focal_length_mm))
            || !non_negative(dof.max_blur_px)
        {
            return Err(
                "depth of field требует конечные положительные фокус, диафрагму и фокусное расстояние",
            );
        }
        let motion = self.motion_blur;
        if !in_range(motion.shutter_angle_deg, f32::MIN_POSITIVE, 360.0)
            || !(4..=32).contains(&motion.samples)
            || !non_negative(motion.max_blur_px)
        {
            return Err(
                "motion blur требует shutter в (0, 360], samples в [4, 32], max_blur_px >= 0",
            );
        }
        Ok(())
    }
}

fn in_range(value: f32, min: f32, max: f32) -> bool {
    value.is_finite() && (min..=max).contains(&value)
}

fn non_negative(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

fn positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

/// Источник экспозиции.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExposureMode {
    /// Фиксированная экспозиция `ev`.
    #[default]
    Manual,
    /// Адаптация по гистограмме яркости; `ev` — компенсация.
    Auto,
}

/// Параметры экспозиции; значения по умолчанию дают EV 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exposure {
    pub mode: ExposureMode,
    pub ev: f32,
    pub min_ev: f32,
    pub max_ev: f32,
    pub speed_up: f32,
    pub speed_down: f32,
    pub low_percent: f32,
    pub high_percent: f32,
}

impl Default for Exposure {
    fn default() -> Self {
        Self {
            mode: ExposureMode::Manual,
            ev: 0.0,
            min_ev: -6.0,
            max_ev: 6.0,
            speed_up: 3.0,
            speed_down: 1.0,
            low_percent: 10.0,
            high_percent: 90.0,
        }
    }
}

/// Свечение ярких областей.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bloom {
    pub enabled: bool,
    pub intensity: f32,
    pub threshold: f32,
    pub knee: f32,
    pub scatter: f32,
    pub tint: [f32; 3],
}

impl Default for Bloom {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.5,
            threshold: 1.0,
            knee: 0.5,
            scatter: 0.7,
            tint: [1.0; 3],
        }
    }
}

/// Качество ambient occlusion: разрешение и число направлений.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AmbientOcclusionQuality {
    Low,
    #[default]
    Medium,
    High,
}

/// Screen-space ambient occlusion, применяемый к непрямому освещению.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AmbientOcclusion {
    pub enabled: bool,
    pub radius: f32,
    pub intensity: f32,
    pub falloff: f32,
    pub quality: AmbientOcclusionQuality,
}

impl Default for AmbientOcclusion {
    fn default() -> Self {
        Self {
            enabled: false,
            radius: 0.5,
            intensity: 1.0,
            falloff: 0.6,
            quality: AmbientOcclusionQuality::Medium,
        }
    }
}

/// Цветокоррекция в linear HDR до tonemapping и lift/gamma/gain после него.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorGrading {
    pub enabled: bool,
    pub temperature: f32,
    pub tint: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub lift: [f32; 3],
    pub gamma: [f32; 3],
    pub gain: [f32; 3],
}

impl Default for ColorGrading {
    fn default() -> Self {
        Self {
            enabled: false,
            temperature: 0.0,
            tint: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            lift: [0.0; 3],
            gamma: [1.0; 3],
            gain: [1.0; 3],
        }
    }
}

/// Затемнение краёв кадра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vignette {
    pub enabled: bool,
    pub intensity: f32,
    pub smoothness: f32,
    pub roundness: f32,
    pub color: [f32; 3],
}

impl Default for Vignette {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.4,
            smoothness: 0.5,
            roundness: 1.0,
            color: [0.0; 3],
        }
    }
}

/// Радиальное смещение каналов к краям кадра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChromaticAberration {
    pub enabled: bool,
    pub intensity: f32,
}

impl Default for ChromaticAberration {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.3,
        }
    }
}

/// Зернистость, добавляемая после anti-aliasing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilmGrain {
    pub enabled: bool,
    pub intensity: f32,
    pub response: f32,
}

impl Default for FilmGrain {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.3,
            response: 0.8,
        }
    }
}

/// Глубина резкости по модели тонкой линзы.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DepthOfField {
    pub enabled: bool,
    pub focus_distance: f32,
    pub aperture: f32,
    pub focal_length_mm: f32,
    pub max_blur_px: f32,
}

impl Default for DepthOfField {
    fn default() -> Self {
        Self {
            enabled: false,
            focus_distance: 10.0,
            aperture: 2.8,
            focal_length_mm: 50.0,
            max_blur_px: 16.0,
        }
    }
}

/// Размытие движения по векторам скорости.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionBlur {
    pub enabled: bool,
    pub shutter_angle_deg: f32,
    pub samples: u32,
    pub max_blur_px: f32,
}

impl Default for MotionBlur {
    fn default() -> Self {
        Self {
            enabled: false,
            shutter_angle_deg: 180.0,
            samples: 12,
            max_blur_px: 32.0,
        }
    }
}

/// Сравнение глубины основного прохода непрозрачных мешей.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthCompare {
    /// Без prepass: первая запись depth.
    Less,
    /// После depth prepass: та же глубина проходит повторно.
    LessOrEqual,
}

/// Какие проходы постобработки выполняются в кадре вида.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostPlan {
    pub opaque_depth_compare: DepthCompare,
    pub depth_prepass: bool,
    pub ambient_occlusion: bool,
    pub motion_blur: bool,
    pub depth_of_field: bool,
    pub auto_exposure: bool,
    /// Число уровней bloom; 0 — bloom отключён.
    pub bloom_mips: u32,
    pub chromatic_aberration: bool,
    pub color_grading: bool,
    pub vignette: bool,
    pub film_grain: bool,
}

impl PostPlan {
    /// План без новых проходов: прежний путь с depth `LESS`.
    pub const NONE: Self = Self {
        opaque_depth_compare: DepthCompare::Less,
        depth_prepass: false,
        ambient_occlusion: false,
        motion_blur: false,
        depth_of_field: false,
        auto_exposure: false,
        bloom_mips: 0,
        chromatic_aberration: false,
        color_grading: false,
        vignette: false,
        film_grain: false,
    };
}

/// Строит план проходов для вида.
///
/// Новые эффекты работают только для `DebugView::Lit`: отладочные режимы их отключают, а
/// ручная экспозиция и tonemapper обрабатываются в `lib.rs` как раньше. Нулевой extent любой
/// стороны даёт пустой план, как и пропуск кадра до этого.
pub fn plan(
    post: &PostProcess,
    debug_view: DebugView,
    render_extent: (u32, u32),
    output_extent: (u32, u32),
) -> PostPlan {
    if render_extent.0 == 0 || render_extent.1 == 0 || output_extent.0 == 0 || output_extent.1 == 0
    {
        return PostPlan::NONE;
    }
    let lit = debug_view == DebugView::Lit;
    let ao = &post.ambient_occlusion;
    let ambient_occlusion = lit && ao.enabled && ao.intensity > 0.0;
    let motion = &post.motion_blur;
    let bloom = &post.bloom;
    let dof = &post.depth_of_field;
    let grading = &post.color_grading;
    let vignette = &post.vignette;
    let aberration = &post.chromatic_aberration;
    let grain = &post.film_grain;
    let bloom_mips = if lit && bloom.enabled && bloom.intensity > 0.0 {
        bloom_mip_count(render_extent)
    } else {
        0
    };
    PostPlan {
        opaque_depth_compare: if ambient_occlusion {
            DepthCompare::LessOrEqual
        } else {
            DepthCompare::Less
        },
        depth_prepass: ambient_occlusion,
        ambient_occlusion,
        motion_blur: lit && motion.enabled && motion.max_blur_px > 0.0,
        depth_of_field: lit && dof.enabled && dof.max_blur_px > 0.0,
        auto_exposure: lit && post.exposure.mode == ExposureMode::Auto,
        bloom_mips,
        chromatic_aberration: lit && aberration.enabled && aberration.intensity > 0.0,
        color_grading: lit && grading.enabled,
        vignette: lit && vignette.enabled && vignette.intensity > 0.0,
        film_grain: lit && grain.enabled && grain.intensity > 0.0,
    }
}

/// Число уровней bloom для меньшей стороны extent; уровень добавляется, пока сторона >= 8.
pub fn bloom_mip_count(extent: (u32, u32)) -> u32 {
    let mut side = extent.0.min(extent.1);
    let mut mips = 0;
    while mips < MAX_BLOOM_MIPS && side >= MIN_BLOOM_MIP_SIDE {
        mips += 1;
        side /= 2;
    }
    mips
}

/// Параметры composite-прохода (tonemap.frag). Раскладка совпадает с шейдером:
/// v0 — (exposure EV, код tonemapper, код debug view, интенсивность CA);
/// v1 — (temperature, tint, contrast, saturation);
/// v2 — (lift.rgb, флаги: bit0 bloom, bit1 auto exposure); v3 — (gamma.rgb, интенсивность vignette);
/// v4 — (gain.rgb, smoothness); v5 — (цвет vignette.rgb, roundness).
/// Выключенный эффект кодируется тождественными значениями.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositeParams {
    pub vectors: [[f32; 4]; COMPOSITE_PUSH_VECTORS],
}

impl CompositeParams {
    /// Байты push constants в порядке записи.
    pub fn to_bytes(self) -> [u8; COMPOSITE_PUSH_CONSTANTS_SIZE] {
        let mut bytes = [0; COMPOSITE_PUSH_CONSTANTS_SIZE];
        for (index, value) in self.vectors.iter().flatten().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

/// Упаковывает composite-параметры для вида по плану.
pub fn composite_params(
    post: &PostProcess,
    plan: &PostPlan,
    debug_view: DebugView,
) -> CompositeParams {
    let grading = &post.color_grading;
    let (temperature, tint, contrast, saturation, lift, gamma, gain) = if plan.color_grading {
        (
            grading.temperature,
            grading.tint,
            grading.contrast,
            grading.saturation,
            grading.lift,
            grading.gamma,
            grading.gain,
        )
    } else {
        (0.0, 0.0, 1.0, 1.0, [0.0; 3], [1.0; 3], [1.0; 3])
    };
    let vignette = &post.vignette;
    let (vignette_intensity, vignette_smoothness, vignette_roundness, vignette_color) =
        if plan.vignette {
            (
                vignette.intensity,
                vignette.smoothness,
                vignette.roundness,
                vignette.color,
            )
        } else {
            (0.0, 0.0, 1.0, [0.0; 3])
        };
    // Флаги композита: бит 0 — bloom, бит 1 — экспозиция по гистограмме (читается из буфера).
    let flags =
        f32::from(u8::from(plan.bloom_mips > 0)) + 2.0 * f32::from(u8::from(plan.auto_exposure));
    let aberration = if plan.chromatic_aberration {
        post.chromatic_aberration.intensity
    } else {
        0.0
    };
    CompositeParams {
        vectors: [
            [
                post.exposure.ev,
                post.tonemapper.code() as f32,
                debug_view.code() as f32,
                aberration,
            ],
            [temperature, tint, contrast, saturation],
            [lift[0], lift[1], lift[2], flags],
            [gamma[0], gamma[1], gamma[2], vignette_intensity],
            [gain[0], gain[1], gain[2], vignette_smoothness],
            [
                vignette_color[0],
                vignette_color[1],
                vignette_color[2],
                vignette_roundness,
            ],
        ],
    }
}

/// Число бинов гистограммы экспозиции (совпадает с exposure_histogram.wgsl).
pub const EXPOSURE_BINS: usize = 256;
/// Диапазон log2-яркости гистограммы и log2 средней серой нужны только CPU-зеркалам ниже,
/// которые проверяют формулы шейдеров; сами шейдеры хранят эти значения у себя.
#[cfg(test)]
const EXPOSURE_MIN_LOG2: f32 = -10.0;
#[cfg(test)]
const EXPOSURE_MAX_LOG2: f32 = 4.0;
#[cfg(test)]
const EXPOSURE_KEY_LOG2: f32 = -2.473_931_2;

/// Push constants усредняющего прохода экспозиции: тайминг и диапазон адаптации.
/// Раскладка: timing = (dt, скорость роста, скорость падения, snap); range = (min, max, low, high).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureAverageParams {
    pub timing: [f32; 4],
    pub range: [f32; 4],
}

impl ExposureAverageParams {
    /// Байты push constants в порядке записи.
    pub fn to_bytes(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (index, value) in self.timing.iter().chain(&self.range).enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    }
}

/// Параметры адаптации для вида. Перцентили переводятся из процентов в доли.
/// `snap` — адаптация не сглаживается: первый кадр вида или явный сброс истории.
pub fn exposure_average_params(
    exposure: &Exposure,
    frame_delta_ms: f32,
    snap: bool,
) -> ExposureAverageParams {
    ExposureAverageParams {
        timing: [
            (frame_delta_ms.max(0.0)) / 1000.0,
            exposure.speed_up,
            exposure.speed_down,
            f32::from(u8::from(snap)),
        ],
        range: [
            exposure.min_ev,
            exposure.max_ev,
            exposure.low_percent / 100.0,
            exposure.high_percent / 100.0,
        ],
    }
}

/// CPU-зеркало `exposure_average.comp`: целевой EV по средней log2-яркости между перцентилями.
/// Пустая гистограмма даёт средний серый, то есть 0 EV до ограничения диапазона.
#[cfg(test)]
fn target_exposure(
    histogram: &[u32; EXPOSURE_BINS],
    low_fraction: f32,
    high_fraction: f32,
    min_ev: f32,
    max_ev: f32,
) -> f32 {
    let total: f64 = histogram.iter().map(|count| f64::from(*count)).sum();
    let low = total * f64::from(low_fraction);
    let high = total * f64::from(high_fraction);
    let mut before = 0.0_f64;
    let mut weight_sum = 0.0_f64;
    let mut weighted_log = 0.0_f64;
    let bin_width = f64::from(EXPOSURE_MAX_LOG2 - EXPOSURE_MIN_LOG2) / EXPOSURE_BINS as f64;
    for (index, count) in histogram.iter().enumerate() {
        let after = before + f64::from(*count);
        let weight = (after.min(high) - before.max(low)).max(0.0);
        let bin_log = f64::from(EXPOSURE_MIN_LOG2) + (index as f64 + 0.5) * bin_width;
        weight_sum += weight;
        weighted_log += weight * bin_log;
        before = after;
    }
    let average_log = if weight_sum > 0.0 {
        weighted_log / weight_sum
    } else {
        f64::from(EXPOSURE_KEY_LOG2)
    };
    (f64::from(EXPOSURE_KEY_LOG2) - average_log).clamp(f64::from(min_ev), f64::from(max_ev)) as f32
}

/// CPU-зеркало адаптации из `exposure_average.comp`: экспоненциальное приближение к цели.
/// Скорость роста применяется, когда цель выше текущего EV, иначе — скорость падения.
#[cfg(test)]
fn adapt_exposure(
    current: f32,
    target: f32,
    frame_delta_ms: f32,
    speed_up: f32,
    speed_down: f32,
    snap: bool,
) -> f32 {
    if snap {
        return target;
    }
    let speed = if target > current {
        speed_up
    } else {
        speed_down
    };
    let alpha = 1.0 - (-(frame_delta_ms.max(0.0) / 1000.0) * speed).exp();
    current + (target - current) * alpha
}

/// Зерно считается по сиду кадра; ограничение по модулю сохраняет точность `f32`.
const GRAIN_SEED_PERIOD: u32 = 4096;

/// Все параметры постобработки одного вида на один кадр.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PostFrame {
    pub plan: PostPlan,
    pub composite: CompositeParams,
    pub grain: [f32; 4],
}

impl PostFrame {
    /// Строит план и параметры вида; сид зерна берётся из счётчика кадров renderer.
    pub fn new(
        post: &PostProcess,
        debug_view: DebugView,
        render_extent: (u32, u32),
        output_extent: (u32, u32),
        frame_serial: u32,
    ) -> Self {
        let plan = plan(post, debug_view, render_extent, output_extent);
        Self {
            plan,
            composite: composite_params(post, &plan, debug_view),
            grain: grain_params(&plan, post, frame_serial),
        }
    }
}

/// Параметры зерна output-прохода: интенсивность, response, сид и резерв.
/// Без плана зерна интенсивность равна 0, и шейдер не меняет пиксели.
pub fn grain_params(plan: &PostPlan, post: &PostProcess, frame_serial: u32) -> [f32; 4] {
    let grain = &post.film_grain;
    let (intensity, response) = if plan.film_grain {
        (grain.intensity, grain.response)
    } else {
        (0.0, 0.0)
    };
    [
        intensity,
        response,
        (frame_serial % GRAIN_SEED_PERIOD) as f32,
        0.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::plan as build_plan;
    use super::*;

    const ALL_VIEWS: [DebugView; 6] = [
        DebugView::Lit,
        DebugView::Wireframe,
        DebugView::Normals,
        DebugView::Overdraw,
        DebugView::LodLevels,
        DebugView::ShadowCascades,
    ];

    fn everything_enabled() -> PostProcess {
        let mut post = PostProcess::default();
        post.ambient_occlusion.enabled = true;
        post.bloom.enabled = true;
        post.color_grading.enabled = true;
        post.vignette.enabled = true;
        post.chromatic_aberration.enabled = true;
        post.film_grain.enabled = true;
        post.depth_of_field.enabled = true;
        post.motion_blur.enabled = true;
        post.exposure.mode = ExposureMode::Auto;
        post
    }

    #[test]
    fn defaults_plan_no_new_passes_for_every_debug_view() {
        for debug_view in ALL_VIEWS {
            let plan = build_plan(
                &PostProcess::default(),
                debug_view,
                (1280, 720),
                (1280, 720),
            );
            assert_eq!(plan, PostPlan::NONE, "{debug_view:?}");
        }
    }

    #[test]
    fn ambient_occlusion_enables_prepass_and_less_or_equal_compare() {
        let mut post = PostProcess::default();
        post.ambient_occlusion.enabled = true;
        let plan = build_plan(&post, DebugView::Lit, (1280, 720), (1280, 720));
        assert!(plan.depth_prepass && plan.ambient_occlusion);
        assert_eq!(plan.opaque_depth_compare, DepthCompare::LessOrEqual);

        post.ambient_occlusion.intensity = 0.0;
        let plan = build_plan(&post, DebugView::Lit, (1280, 720), (1280, 720));
        assert!(!plan.depth_prepass);
        assert_eq!(plan.opaque_depth_compare, DepthCompare::Less);
    }

    #[test]
    fn non_lit_debug_views_disable_every_new_effect() {
        let post = everything_enabled();
        for debug_view in ALL_VIEWS.into_iter().filter(|view| *view != DebugView::Lit) {
            let plan = build_plan(&post, debug_view, (1280, 720), (1280, 720));
            assert_eq!(plan, PostPlan::NONE, "{debug_view:?}");
        }
    }

    #[test]
    fn lit_view_enables_all_requested_effects() {
        let plan = build_plan(
            &everything_enabled(),
            DebugView::Lit,
            (1280, 720),
            (1920, 1080),
        );
        assert_eq!(plan.opaque_depth_compare, DepthCompare::LessOrEqual);
        assert!(plan.depth_prepass);
        assert!(plan.motion_blur && plan.depth_of_field && plan.auto_exposure);
        assert!(plan.chromatic_aberration && plan.color_grading && plan.vignette);
        assert!(plan.film_grain);
        assert_eq!(plan.bloom_mips, 6);
    }

    #[test]
    fn zero_extent_produces_an_empty_plan_and_one_pixel_skips_bloom_only() {
        let post = everything_enabled();
        for (render, output) in [
            ((0, 720), (1280, 720)),
            ((1280, 0), (1280, 720)),
            ((1, 1), (0, 1)),
        ] {
            assert_eq!(
                build_plan(&post, DebugView::Lit, render, output),
                PostPlan::NONE
            );
        }
        let plan = build_plan(&post, DebugView::Lit, (1, 1), (1, 1));
        assert_eq!(plan.bloom_mips, 0);
        assert!(plan.depth_prepass && plan.color_grading);
    }

    #[test]
    fn bloom_mip_count_stops_at_small_sides_and_the_cap() {
        assert_eq!(bloom_mip_count((1, 1)), 0);
        assert_eq!(bloom_mip_count((7, 4096)), 0);
        assert_eq!(bloom_mip_count((8, 8)), 1);
        assert_eq!(bloom_mip_count((16, 1000)), 2);
        assert_eq!(bloom_mip_count((2048, 2048)), MAX_BLOOM_MIPS);
    }

    #[test]
    fn auto_exposure_is_lit_only_and_manual_mode_stays_manual() {
        let mut post = PostProcess::default();
        post.exposure.mode = ExposureMode::Auto;
        assert!(build_plan(&post, DebugView::Lit, (64, 64), (64, 64)).auto_exposure);
        assert!(!build_plan(&post, DebugView::Wireframe, (64, 64), (64, 64)).auto_exposure);
        post.exposure.mode = ExposureMode::Manual;
        assert!(!build_plan(&post, DebugView::Lit, (64, 64), (64, 64)).auto_exposure);
    }

    #[test]
    fn defaults_pack_to_identity_composite_parameters() {
        let post = PostProcess::default();
        let plan = build_plan(&post, DebugView::Lit, (64, 64), (64, 64));
        let params = composite_params(&post, &plan, DebugView::Lit);
        assert_eq!(
            params.vectors,
            [
                [0.0, 0.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 1.0],
                [0.0, 0.0, 0.0, 0.0],
                [1.0, 1.0, 1.0, 0.0],
                [1.0, 1.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ]
        );
    }

    #[test]
    fn disabled_effects_do_not_leak_into_composite_parameters() {
        let mut post = PostProcess::default();
        post.color_grading.temperature = 0.5;
        post.vignette.intensity = 0.9;
        post.chromatic_aberration.intensity = 0.7;
        let plan = build_plan(&post, DebugView::Lit, (64, 64), (64, 64));
        let params = composite_params(&post, &plan, DebugView::Lit);
        assert_eq!(params.vectors[0][3], 0.0);
        assert_eq!(params.vectors[1][0], 0.0);
        assert_eq!(params.vectors[3][3], 0.0);
    }

    #[test]
    fn composite_parameters_keep_exposure_and_tonemapper_for_wireframe() {
        let mut post = PostProcess::default();
        post.exposure.ev = 2.0;
        post.tonemapper = Tonemapper::Neutral;
        post.vignette.enabled = true;
        let plan = build_plan(&post, DebugView::Wireframe, (64, 64), (64, 64));
        let params = composite_params(&post, &plan, DebugView::Wireframe);
        assert_eq!(params.vectors[0][0], 2.0);
        assert_eq!(params.vectors[0][1], Tonemapper::Neutral.code() as f32);
        assert_eq!(params.vectors[3][3], 0.0);
    }

    #[test]
    fn composite_parameter_bytes_match_the_vector_layout() {
        let mut post = PostProcess::default();
        post.exposure.ev = -1.5;
        post.color_grading.enabled = true;
        post.color_grading.lift = [0.25, 0.5, 0.75];
        let plan = build_plan(&post, DebugView::Lit, (64, 64), (64, 64));
        let bytes = composite_params(&post, &plan, DebugView::Lit).to_bytes();
        assert_eq!(bytes.len(), COMPOSITE_PUSH_CONSTANTS_SIZE);
        assert_eq!(f32::from_ne_bytes(bytes[0..4].try_into().unwrap()), -1.5);
        assert_eq!(f32::from_ne_bytes(bytes[32..36].try_into().unwrap()), 0.25);
        assert_eq!(f32::from_ne_bytes(bytes[36..40].try_into().unwrap()), 0.5);
        assert_eq!(f32::from_ne_bytes(bytes[40..44].try_into().unwrap()), 0.75);
    }

    #[test]
    fn grain_parameters_are_zero_unless_planned_and_wrap_the_seed() {
        let mut post = PostProcess::default();
        post.film_grain.enabled = true;
        post.film_grain.intensity = 0.4;
        let lit = build_plan(&post, DebugView::Lit, (64, 64), (64, 64));
        let grain = grain_params(&lit, &post, 5000);
        assert_eq!(grain[0], 0.4);
        assert_eq!(grain[2], (5000 % GRAIN_SEED_PERIOD) as f32);
        let wireframe = build_plan(&post, DebugView::Wireframe, (64, 64), (64, 64));
        assert_eq!(grain_params(&wireframe, &post, 0)[0], 0.0);
    }

    #[test]
    fn empty_histogram_targets_the_middle_grey_exposure() {
        let histogram = [0_u32; EXPOSURE_BINS];
        assert_eq!(target_exposure(&histogram, 0.1, 0.9, -16.0, 16.0), 0.0);
    }

    #[test]
    fn target_follows_the_average_log_luminance_and_respects_the_range() {
        // Все пиксели в одном бине около log2 яркости 1.0: цель приводит её к среднему серому.
        let mut histogram = [0_u32; EXPOSURE_BINS];
        let bin = ((1.0 - EXPOSURE_MIN_LOG2) / (EXPOSURE_MAX_LOG2 - EXPOSURE_MIN_LOG2)
            * EXPOSURE_BINS as f32) as usize;
        histogram[bin] = 1000;
        let target = target_exposure(&histogram, 0.1, 0.9, -16.0, 16.0);
        assert!(
            (target - (EXPOSURE_KEY_LOG2 - 1.0)).abs() < 0.05,
            "{target}"
        );
        assert_eq!(target_exposure(&histogram, 0.1, 0.9, -0.5, 16.0), -0.5);
    }

    #[test]
    fn percentiles_ignore_samples_outside_the_requested_window() {
        // Половина пикселей очень тёмные, половина очень яркие; окно 50–100 берёт только яркие.
        let mut histogram = [0_u32; EXPOSURE_BINS];
        histogram[0] = 1000;
        histogram[EXPOSURE_BINS - 1] = 1000;
        let bright_only = target_exposure(&histogram, 0.5, 1.0, -16.0, 16.0);
        let bright_expected = target_exposure(
            &{
                let mut only = [0_u32; EXPOSURE_BINS];
                only[EXPOSURE_BINS - 1] = 1000;
                only
            },
            0.0,
            1.0,
            -16.0,
            16.0,
        );
        assert!(
            (bright_only - bright_expected).abs() < 0.05,
            "{bright_only} vs {bright_expected}"
        );
    }

    #[test]
    fn adaptation_snaps_on_request_and_holds_without_elapsed_time() {
        assert_eq!(adapt_exposure(0.0, 2.0, 16.0, 3.0, 1.0, true), 2.0);
        assert_eq!(adapt_exposure(0.5, 2.0, 0.0, 3.0, 1.0, false), 0.5);
    }

    #[test]
    fn adaptation_brightening_is_faster_than_darkening_and_never_overshoots() {
        let up = adapt_exposure(0.0, 2.0, 100.0, 3.0, 1.0, false) - 0.0;
        let down = 0.0 - adapt_exposure(0.0, -2.0, 100.0, 3.0, 1.0, false);
        assert!(up > down, "up {up} down {down}");
        let after_long_time = adapt_exposure(0.0, 2.0, 60_000.0, 3.0, 1.0, false);
        assert!(
            after_long_time <= 2.0 && after_long_time > 1.99,
            "{after_long_time}"
        );
    }

    #[test]
    fn average_params_convert_percentages_and_mark_snap() {
        let exposure = Exposure {
            speed_up: 2.5,
            speed_down: 0.5,
            low_percent: 5.0,
            high_percent: 95.0,
            min_ev: -4.0,
            max_ev: 4.0,
            ..Exposure::default()
        };
        let params = exposure_average_params(&exposure, 250.0, true);
        assert_eq!(params.timing, [0.25, 2.5, 0.5, 1.0]);
        assert_eq!(params.range, [-4.0, 4.0, 0.05, 0.95]);
        assert_eq!(
            exposure_average_params(&exposure, -5.0, false).timing[0],
            0.0
        );
    }

    #[test]
    fn composite_flags_mark_bloom_and_auto_exposure_bits() {
        let mut post = PostProcess::default();
        post.exposure.mode = ExposureMode::Auto;
        post.bloom.enabled = true;
        let both = plan(&post, DebugView::Lit, (64, 64), (64, 64));
        assert_eq!(
            composite_params(&post, &both, DebugView::Lit).vectors[2][3],
            3.0
        );
        let auto_only = plan(&post, DebugView::Wireframe, (64, 64), (64, 64));
        assert_eq!(
            composite_params(&post, &auto_only, DebugView::Wireframe).vectors[2][3],
            0.0
        );
    }

    #[test]
    fn push_constant_sizes_fit_the_shared_layout() {
        assert_eq!(COMPOSITE_PUSH_CONSTANTS_SIZE, 96);
        assert_eq!(OUTPUT_PUSH_CONSTANTS_SIZE, 32);
    }

    #[test]
    fn validation_rejects_each_group_of_out_of_range_values() {
        assert!(PostProcess::default().validate().is_ok());
        let mut post = PostProcess::default();
        post.exposure.ev = f32::NAN;
        assert!(post.validate().is_err());
        post = PostProcess::default();
        post.exposure.min_ev = 2.0;
        post.exposure.max_ev = 1.0;
        assert!(post.validate().is_err());
        post = PostProcess::default();
        post.bloom.threshold = -1.0;
        assert!(post.validate().is_err());
        post = PostProcess::default();
        post.ambient_occlusion.radius = 0.0;
        assert!(post.validate().is_err());
        post = PostProcess::default();
        post.motion_blur.samples = 3;
        assert!(post.validate().is_err());
        post = PostProcess::default();
        post.depth_of_field.aperture = f32::INFINITY;
        assert!(post.validate().is_err());
    }
}
