//! Настройки постобработки камеры, задаваемые через `.postprocess`.
//!
//! Каждая таблица содержит `enabled`. Выключенный эффект не выполняется; `exposure` и
//! `tonemapping` при `enabled = false` дают тождественное преобразование (EV 0 вручную и без
//! tonemapper). Значения по умолчанию соответствуют прежнему поведению цветового пути камеры.

use crate::camera_settings::{CameraColorSettings, CameraSettings, Tonemapper};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// Полный набор постобработки одной камеры.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PostProcessSettings {
    pub exposure: ExposureSettings,
    pub tonemapping: TonemappingSettings,
    pub bloom: BloomSettings,
    pub ambient_occlusion: AmbientOcclusionSettings,
    pub color_grading: ColorGradingSettings,
    pub vignette: VignetteSettings,
    pub chromatic_aberration: ChromaticAberrationSettings,
    pub film_grain: FilmGrainSettings,
    pub depth_of_field: DepthOfFieldSettings,
    pub motion_blur: MotionBlurSettings,
}

impl PostProcessSettings {
    /// Разбирает и проверяет TOML, добавляя имя файла к ошибкам.
    pub fn parse(label: impl Into<String>, input: &str) -> Result<Self, PostProcessError> {
        let label = label.into();
        let settings: Self = toml::from_str(input)
            .map_err(|error| PostProcessError::new(&label, error.to_string()))?;
        settings.validate_at(&label)?;
        Ok(settings)
    }

    /// Сериализует настройки в форматированный TOML.
    pub fn to_toml(&self) -> Result<String, PostProcessError> {
        toml::to_string_pretty(self)
            .map_err(|error| PostProcessError::new("post process", error.to_string()))
    }

    /// Проверяет диапазоны всех таблиц, включая выключенные.
    pub fn validate(&self) -> Result<(), PostProcessError> {
        self.validate_at("post process")
    }

    fn validate_at(&self, label: &str) -> Result<(), PostProcessError> {
        let check = |condition: bool, message: &str| {
            if condition {
                Ok(())
            } else {
                Err(PostProcessError::new(label, message))
            }
        };
        let exposure = self.exposure;
        check(
            finite_in(exposure.ev, -16.0, 16.0),
            "exposure.ev должен быть конечным и находиться в [-16, 16]",
        )?;
        check(
            finite_in(exposure.min_ev, -16.0, 16.0)
                && finite_in(exposure.max_ev, -16.0, 16.0)
                && exposure.min_ev <= exposure.max_ev,
            "exposure требует конечные min_ev и max_ev в [-16, 16] с min_ev <= max_ev",
        )?;
        check(
            finite_positive(exposure.speed_up) && finite_positive(exposure.speed_down),
            "exposure.speed_up и exposure.speed_down должны быть конечными и > 0",
        )?;
        check(
            finite_in(exposure.low_percent, 0.0, 100.0)
                && finite_in(exposure.high_percent, 0.0, 100.0)
                && exposure.low_percent < exposure.high_percent,
            "exposure требует 0 <= low_percent < high_percent <= 100",
        )?;

        let bloom = self.bloom;
        check(
            finite_non_negative(bloom.intensity),
            "bloom.intensity должен быть конечным и >= 0",
        )?;
        check(
            finite_non_negative(bloom.threshold),
            "bloom.threshold должен быть конечным и >= 0",
        )?;
        check(
            finite_in(bloom.knee, 0.0, 1.0) && finite_in(bloom.scatter, 0.0, 1.0),
            "bloom.knee и bloom.scatter должны находиться в [0, 1]",
        )?;
        check(
            bloom.tint.into_iter().all(finite_non_negative),
            "bloom.tint должен содержать конечные компоненты >= 0",
        )?;

        let ao = self.ambient_occlusion;
        check(
            finite_positive(ao.radius),
            "ambient_occlusion.radius должен быть конечным и > 0",
        )?;
        check(
            finite_in(ao.intensity, 0.0, 4.0) && finite_in(ao.falloff, 0.0, 1.0),
            "ambient_occlusion.intensity должен быть в [0, 4], falloff — в [0, 1]",
        )?;

        let grading = &self.color_grading;
        check(
            finite_in(grading.temperature, -1.0, 1.0) && finite_in(grading.tint, -1.0, 1.0),
            "color_grading.temperature и color_grading.tint должны находиться в [-1, 1]",
        )?;
        check(
            finite_in(grading.contrast, 0.0, 2.0) && finite_in(grading.saturation, 0.0, 2.0),
            "color_grading.contrast и color_grading.saturation должны находиться в [0, 2]",
        )?;
        check(
            grading
                .lift
                .into_iter()
                .all(|value| finite_in(value, -1.0, 1.0)),
            "color_grading.lift должен содержать компоненты в [-1, 1]",
        )?;
        check(
            grading
                .gamma
                .into_iter()
                .all(|value| finite_in(value, 0.001, 4.0)),
            "color_grading.gamma должен содержать компоненты в [0.001, 4]",
        )?;
        check(
            grading
                .gain
                .into_iter()
                .all(|value| finite_in(value, 0.0, 4.0)),
            "color_grading.gain должен содержать компоненты в [0, 4]",
        )?;
        check(
            finite_in(grading.lut_intensity, 0.0, 1.0),
            "color_grading.lut_intensity должен находиться в [0, 1]",
        )?;
        check(
            grading
                .lut
                .as_deref()
                .is_none_or(|path| VirtualPath::parse(path).is_ok() && path.ends_with(".cube")),
            "color_grading.lut должен быть путём к файлу .cube",
        )?;

        let vignette = self.vignette;
        check(
            finite_in(vignette.intensity, 0.0, 1.0)
                && finite_in(vignette.smoothness, 0.0, 1.0)
                && finite_in(vignette.roundness, 0.0, 1.0),
            "vignette.intensity, smoothness и roundness должны находиться в [0, 1]",
        )?;
        check(
            vignette
                .color
                .into_iter()
                .all(|value| finite_in(value, 0.0, 1.0)),
            "vignette.color должен содержать компоненты в [0, 1]",
        )?;

        check(
            finite_in(self.chromatic_aberration.intensity, 0.0, 1.0),
            "chromatic_aberration.intensity должен находиться в [0, 1]",
        )?;
        let grain = self.film_grain;
        check(
            finite_in(grain.intensity, 0.0, 1.0) && finite_in(grain.response, 0.0, 1.0),
            "film_grain.intensity и film_grain.response должны находиться в [0, 1]",
        )?;

        let dof = self.depth_of_field;
        check(
            finite_positive(dof.focus_distance)
                && finite_positive(dof.aperture)
                && finite_positive(dof.focal_length_mm),
            "depth_of_field.focus_distance, aperture и focal_length_mm должны быть конечными и > 0",
        )?;
        check(
            finite_non_negative(dof.max_blur_px),
            "depth_of_field.max_blur_px должен быть конечным и >= 0",
        )?;

        let motion = self.motion_blur;
        check(
            finite_in(motion.shutter_angle_deg, f32::MIN_POSITIVE, 360.0),
            "motion_blur.shutter_angle_deg должен быть конечным и находиться в (0, 360]",
        )?;
        check(
            (MIN_MOTION_BLUR_SAMPLES..=MAX_MOTION_BLUR_SAMPLES).contains(&motion.samples),
            "motion_blur.samples должен находиться в [4, 32]",
        )?;
        check(
            finite_non_negative(motion.max_blur_px),
            "motion_blur.max_blur_px должен быть конечным и >= 0",
        )?;
        Ok(())
    }
}

/// Минимум выборок motion blur по формату `.postprocess`.
pub const MIN_MOTION_BLUR_SAMPLES: u32 = 4;
/// Максимум выборок motion blur по формату `.postprocess`.
pub const MAX_MOTION_BLUR_SAMPLES: u32 = 32;

/// Ручная или автоматическая экспозиция по гистограмме яркости.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExposureSettings {
    pub enabled: bool,
    pub mode: ExposureMode,
    /// В режиме `manual` — экспозиция; в `auto` — компенсация поверх адаптации.
    pub ev: f32,
    pub min_ev: f32,
    pub max_ev: f32,
    /// Скорость адаптации к более яркой сцене, EV в секунду.
    pub speed_up: f32,
    /// Скорость адаптации к более тёмной сцене, EV в секунду.
    pub speed_down: f32,
    /// Нижняя и верхняя границы перцентилей гистограммы в процентах.
    pub low_percent: f32,
    pub high_percent: f32,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        Self {
            enabled: true,
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

/// Источник экспозиции.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExposureMode {
    #[default]
    Manual,
    Auto,
}

/// Оператор преобразования HDR в LDR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TonemappingSettings {
    pub enabled: bool,
    pub tonemapper: Tonemapper,
}

impl Default for TonemappingSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            tonemapper: Tonemapper::Aces,
        }
    }
}

/// Свечение ярких областей.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BloomSettings {
    pub enabled: bool,
    pub intensity: f32,
    pub threshold: f32,
    pub knee: f32,
    pub scatter: f32,
    pub tint: [f32; 3],
}

impl Default for BloomSettings {
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

/// Screen-space ambient occlusion, применяемый к ambient-освещению.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AmbientOcclusionSettings {
    pub enabled: bool,
    /// Радиус в метрах мира.
    pub radius: f32,
    pub intensity: f32,
    /// Доля радиуса, на которой вклад затухает.
    pub falloff: f32,
    pub quality: AmbientOcclusionQuality,
}

impl Default for AmbientOcclusionSettings {
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

/// Качество AO: разрешение и число направлений/шагов.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AmbientOcclusionQuality {
    Low,
    #[default]
    Medium,
    High,
}

/// Цветокоррекция в linear HDR до tonemapping, lift/gamma/gain после него и 3D LUT в конце.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorGradingSettings {
    pub enabled: bool,
    pub temperature: f32,
    pub tint: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub lift: [f32; 3],
    pub gamma: [f32; 3],
    pub gain: [f32; 3],
    /// Путь к 3D LUT `.cube` (см. `color_lut`); LUT применяется после lift/gamma/gain.
    /// Это зависимость файла `.postprocess`, а не ассет сцены: объявлять её в `[[assets]]` не нужно.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lut: Option<String>,
    /// Доля LUT в `[0, 1]`.
    pub lut_intensity: f32,
}

impl ColorGradingSettings {
    /// Путь LUT в каноническом виде; `None`, если LUT не задана или путь некорректен.
    pub fn lut_path(&self) -> Option<VirtualPath> {
        self.lut
            .as_deref()
            .and_then(|path| VirtualPath::parse(path).ok())
    }
}

impl Default for ColorGradingSettings {
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
            lut: None,
            lut_intensity: 1.0,
        }
    }
}

/// Затемнение краёв кадра.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VignetteSettings {
    pub enabled: bool,
    pub intensity: f32,
    pub smoothness: f32,
    pub roundness: f32,
    pub color: [f32; 3],
}

impl Default for VignetteSettings {
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

/// Радиальное смещение каналов цвета к краям кадра.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChromaticAberrationSettings {
    pub enabled: bool,
    pub intensity: f32,
}

impl Default for ChromaticAberrationSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.3,
        }
    }
}

/// Зернистость плёнки, добавляемая после anti-aliasing.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilmGrainSettings {
    pub enabled: bool,
    pub intensity: f32,
    /// Доля зерна, которая зависит от яркости пикселя.
    pub response: f32,
}

impl Default for FilmGrainSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.3,
            response: 0.8,
        }
    }
}

/// Глубина резкости по модели тонкой линзы.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DepthOfFieldSettings {
    pub enabled: bool,
    /// Расстояние до плоскости фокуса в метрах мира.
    pub focus_distance: f32,
    /// Диафрагма, f-stop.
    pub aperture: f32,
    pub focal_length_mm: f32,
    pub max_blur_px: f32,
}

impl Default for DepthOfFieldSettings {
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
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MotionBlurSettings {
    pub enabled: bool,
    pub shutter_angle_deg: f32,
    pub samples: u32,
    pub max_blur_px: f32,
}

impl Default for MotionBlurSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            shutter_angle_deg: 180.0,
            samples: 12,
            max_blur_px: 32.0,
        }
    }
}

/// Постобработка, которую получает камера: файл побеждает, без файла — legacy `[color]`.
///
/// Если у камеры есть `post_file`, возвращается его копия. Без файла exposure и tonemapper
/// берутся из устаревшего блока `[color]` камеры поверх значений по умолчанию; без обоих
/// источников возвращаются значения по умолчанию.
pub fn resolve_post_process(
    camera: &CameraSettings,
    post_file: Option<&PostProcessSettings>,
) -> PostProcessSettings {
    if let Some(file) = post_file {
        return file.clone();
    }
    let mut settings = PostProcessSettings::default();
    if let Some(color) = camera.color {
        settings.exposure = legacy_exposure(color);
        settings.tonemapping.tonemapper = color.tonemapper;
    }
    settings
}

fn legacy_exposure(color: CameraColorSettings) -> ExposureSettings {
    ExposureSettings {
        mode: ExposureMode::Manual,
        ev: color.exposure_ev,
        ..ExposureSettings::default()
    }
}

fn finite_in(value: f32, min: f32, max: f32) -> bool {
    value.is_finite() && (min..=max).contains(&value)
}

fn finite_non_negative(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

fn finite_positive(value: f32) -> bool {
    value.is_finite() && value > 0.0
}

/// Ошибка разбора или проверки `.postprocess`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostProcessError {
    pub label: String,
    pub message: String,
}

impl PostProcessError {
    fn new(label: &str, message: impl Into<String>) -> Self {
        Self {
            label: label.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for PostProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.message)
    }
}

impl Error for PostProcessError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_settings::CameraSettings;

    #[test]
    fn color_grading_lut_parses_round_trips_and_is_omitted_when_absent() {
        let source =
            "[color_grading]\nenabled = true\nlut = \"/luts/film.cube\"\nlut_intensity = 0.5\n";
        let settings = PostProcessSettings::parse("graded.postprocess", source).unwrap();
        assert_eq!(
            settings.color_grading.lut.as_deref(),
            Some("/luts/film.cube")
        );
        assert_eq!(settings.color_grading.lut_intensity, 0.5);
        assert_eq!(
            settings.color_grading.lut_path().unwrap().as_str(),
            "luts/film.cube"
        );
        let written = settings.to_toml().unwrap();
        assert_eq!(
            PostProcessSettings::parse("graded.postprocess", &written).unwrap(),
            settings
        );
        let defaults = PostProcessSettings::default().to_toml().unwrap();
        assert!(!defaults.contains("lut ="), "{defaults}");
        assert_eq!(
            PostProcessSettings::default().color_grading.lut_intensity,
            1.0
        );
    }

    #[test]
    fn color_grading_lut_requires_a_cube_path_and_unit_intensity() {
        for lut in ["/luts/film.png", "/luts/film.CUBE", "/../film.cube", ""] {
            let source = format!("[color_grading]\nlut = \"{lut}\"\n");
            let error = PostProcessSettings::parse("bad.postprocess", &source).unwrap_err();
            assert!(
                error.message.contains("color_grading.lut"),
                "{lut}: {error}"
            );
        }
        for intensity in ["-0.1", "1.5"] {
            let source = format!("[color_grading]\nlut_intensity = {intensity}\n");
            assert!(PostProcessSettings::parse("bad.postprocess", &source).is_err());
        }
    }

    #[test]
    fn empty_file_matches_defaults_and_defaults_keep_effects_off() {
        let settings = PostProcessSettings::parse("default.postprocess", "").unwrap();
        assert_eq!(settings, PostProcessSettings::default());
        assert!(settings.exposure.enabled);
        assert_eq!(settings.exposure.mode, ExposureMode::Manual);
        assert_eq!(settings.exposure.ev, 0.0);
        assert!(settings.tonemapping.enabled);
        assert_eq!(settings.tonemapping.tonemapper, Tonemapper::Aces);
        for enabled in [
            settings.bloom.enabled,
            settings.ambient_occlusion.enabled,
            settings.color_grading.enabled,
            settings.vignette.enabled,
            settings.chromatic_aberration.enabled,
            settings.film_grain.enabled,
            settings.depth_of_field.enabled,
            settings.motion_blur.enabled,
        ] {
            assert!(!enabled);
        }
    }

    #[test]
    fn partial_tables_keep_defaults_for_missing_fields() {
        let settings = PostProcessSettings::parse(
            "partial.postprocess",
            "[bloom]\nenabled = true\nintensity = 2.0\n[exposure]\nmode = \"auto\"\n",
        )
        .unwrap();
        assert!(settings.bloom.enabled);
        assert_eq!(settings.bloom.intensity, 2.0);
        assert_eq!(settings.bloom.knee, BloomSettings::default().knee);
        assert_eq!(settings.exposure.mode, ExposureMode::Auto);
        assert_eq!(
            settings.exposure.speed_up,
            ExposureSettings::default().speed_up
        );
    }

    #[test]
    fn parse_and_toml_round_trip_keep_every_table() {
        let settings = PostProcessSettings::parse(
            "custom.postprocess",
            r#"
[exposure]
mode = "auto"
ev = 0.5
min_ev = -4
max_ev = 4
speed_up = 2.5
speed_down = 0.75
low_percent = 5
high_percent = 95
[tonemapping]
tonemapper = "neutral"
[bloom]
enabled = true
intensity = 0.8
threshold = 1.2
knee = 0.3
scatter = 0.6
tint = [1.0, 0.9, 0.8]
[ambient_occlusion]
enabled = true
radius = 0.75
intensity = 1.5
falloff = 0.4
quality = "high"
[color_grading]
enabled = true
temperature = 0.1
tint = -0.1
contrast = 1.1
saturation = 0.9
lift = [0.0, 0.01, 0.02]
gamma = [1.0, 1.1, 1.2]
gain = [1.0, 0.9, 0.8]
[vignette]
enabled = true
intensity = 0.3
smoothness = 0.6
roundness = 0.8
color = [0.0, 0.0, 0.0]
[chromatic_aberration]
enabled = true
intensity = 0.2
[film_grain]
enabled = true
intensity = 0.1
response = 0.5
[depth_of_field]
enabled = true
focus_distance = 5.0
aperture = 1.8
focal_length_mm = 35.0
max_blur_px = 12.0
[motion_blur]
enabled = true
shutter_angle_deg = 90.0
samples = 16
max_blur_px = 24.0
"#,
        )
        .unwrap();
        let serialized = settings.to_toml().unwrap();
        assert_eq!(
            PostProcessSettings::parse("round-trip.postprocess", &serialized).unwrap(),
            settings,
            "{serialized}"
        );
        assert_eq!(settings.tonemapping.tonemapper, Tonemapper::Neutral);
        assert_eq!(
            settings.ambient_occlusion.quality,
            AmbientOcclusionQuality::High
        );
        assert_eq!(settings.motion_blur.samples, 16);
    }

    #[test]
    fn unknown_fields_are_denied_at_every_table_level() {
        let tables = [
            "exposure",
            "tonemapping",
            "bloom",
            "ambient_occlusion",
            "color_grading",
            "vignette",
            "chromatic_aberration",
            "film_grain",
            "depth_of_field",
            "motion_blur",
        ];
        assert!(PostProcessSettings::parse("unknown.postprocess", "unexpected = true").is_err());
        for table in tables {
            let input = format!("[{table}]\nunexpected = true");
            assert!(
                PostProcessSettings::parse("unknown.postprocess", &input).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn enum_values_are_lowercase_and_unknown_values_fail() {
        for input in [
            "[exposure]\nmode = \"Auto\"",
            "[exposure]\nmode = \"automatic\"",
            "[tonemapping]\ntonemapper = \"ACES\"",
            "[tonemapping]\ntonemapper = \"filmic\"",
            "[ambient_occlusion]\nquality = \"ultra\"",
            "[ambient_occlusion]\nquality = \"High\"",
        ] {
            let error = PostProcessSettings::parse("bad-enum.postprocess", input).unwrap_err();
            assert!(
                error.to_string().contains("bad-enum.postprocess"),
                "{error}"
            );
        }
    }

    #[test]
    fn numeric_ranges_reject_nan_infinity_and_out_of_range_values() {
        for input in [
            "[exposure]\nev = 16.5",
            "[exposure]\nev = nan",
            "[exposure]\nmin_ev = 2\nmax_ev = 1",
            "[exposure]\nspeed_up = 0",
            "[exposure]\nspeed_down = inf",
            "[exposure]\nlow_percent = 60\nhigh_percent = 50",
            "[exposure]\nhigh_percent = 101",
            "[bloom]\nintensity = -0.1",
            "[bloom]\nthreshold = nan",
            "[bloom]\nknee = 1.5",
            "[bloom]\ntint = [1.0, -1.0, 1.0]",
            "[ambient_occlusion]\nradius = 0",
            "[ambient_occlusion]\nintensity = 4.5",
            "[ambient_occlusion]\nfalloff = -0.1",
            "[color_grading]\ntemperature = 1.5",
            "[color_grading]\nsaturation = 2.5",
            "[color_grading]\ngamma = [0.0, 1.0, 1.0]",
            "[color_grading]\ngain = [1.0, 1.0, 5.0]",
            "[vignette]\nintensity = 1.5",
            "[chromatic_aberration]\nintensity = -0.5",
            "[film_grain]\nresponse = 2.0",
            "[depth_of_field]\nfocus_distance = 0",
            "[depth_of_field]\naperture = -1",
            "[depth_of_field]\nfocal_length_mm = inf",
            "[depth_of_field]\nmax_blur_px = -2",
            "[motion_blur]\nshutter_angle_deg = 0",
            "[motion_blur]\nshutter_angle_deg = 361",
            "[motion_blur]\nsamples = 3",
            "[motion_blur]\nsamples = 33",
            "[motion_blur]\nmax_blur_px = nan",
        ] {
            assert!(
                PostProcessSettings::parse("invalid.postprocess", input).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn validate_checks_values_constructed_in_code() {
        let mut settings = PostProcessSettings::default();
        settings.bloom.intensity = f32::NAN;
        let error = settings.validate().unwrap_err();
        assert!(error.message.contains("bloom.intensity"), "{error}");
    }

    #[test]
    fn resolve_prefers_post_file_over_legacy_color() {
        let legacy = CameraSettings::parse(
            "legacy.camerasettings",
            "[color]\nexposure_ev = 2.0\ntonemapper = \"none\"\n",
        )
        .unwrap();
        let mut file = PostProcessSettings::default();
        file.exposure.ev = -1.0;
        file.tonemapping.tonemapper = Tonemapper::Neutral;

        let resolved = resolve_post_process(&legacy, Some(&file));
        assert_eq!(resolved, file);
    }

    #[test]
    fn resolve_uses_legacy_color_only_without_post_file() {
        let legacy = CameraSettings::parse(
            "legacy.camerasettings",
            "[color]\nexposure_ev = 2.0\ntonemapper = \"none\"\n",
        )
        .unwrap();
        let resolved = resolve_post_process(&legacy, None);
        assert_eq!(resolved.exposure.ev, 2.0);
        assert_eq!(resolved.exposure.mode, ExposureMode::Manual);
        assert_eq!(resolved.tonemapping.tonemapper, Tonemapper::None);
        assert_eq!(resolved.bloom, BloomSettings::default());
    }

    #[test]
    fn resolve_without_any_source_returns_defaults() {
        let resolved = resolve_post_process(&CameraSettings::default(), None);
        assert_eq!(resolved, PostProcessSettings::default());
    }

    #[test]
    fn motion_blur_sample_bounds_are_inclusive() {
        for samples in [MIN_MOTION_BLUR_SAMPLES, MAX_MOTION_BLUR_SAMPLES] {
            let input = format!("[motion_blur]\nsamples = {samples}");
            assert!(PostProcessSettings::parse("bounds.postprocess", &input).is_ok());
        }
    }
}
