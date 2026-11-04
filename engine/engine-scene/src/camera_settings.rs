//! Настройки проекции и вывода, назначаемые камере через `.camerasettings`.

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

/// Полный набор настроек камеры и её вывода.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraSettings {
    pub projection: ProjectionSettings,
    pub render: CameraRenderSettings,
    pub color: CameraColorSettings,
}

impl CameraSettings {
    /// Разбирает и проверяет TOML, добавляя имя файла к ошибкам.
    pub fn parse(label: impl Into<String>, input: &str) -> Result<Self, CameraSettingsError> {
        let label = label.into();
        let settings: Self = toml::from_str(input)
            .map_err(|error| CameraSettingsError::new(&label, error.to_string()))?;
        settings.validate_at(&label)?;
        Ok(settings)
    }

    /// Сериализует настройки в форматированный TOML.
    pub fn to_toml(&self) -> Result<String, CameraSettingsError> {
        toml::to_string_pretty(self)
            .map_err(|error| CameraSettingsError::new("camera settings", error.to_string()))
    }

    /// Проверяет диапазоны настроек.
    pub fn validate(&self) -> Result<(), CameraSettingsError> {
        self.validate_at("camera settings")
    }

    fn validate_at(&self, label: &str) -> Result<(), CameraSettingsError> {
        let projection = self.projection;
        if !projection.fov_deg.is_finite()
            || projection.fov_deg <= 0.0
            || projection.fov_deg >= 180.0
        {
            return Err(CameraSettingsError::new(
                label,
                "projection.fov_deg должен быть конечным и находиться между 0 и 180",
            ));
        }
        if !projection.near.is_finite()
            || !projection.far.is_finite()
            || projection.near <= 0.0
            || projection.far <= projection.near
        {
            return Err(CameraSettingsError::new(
                label,
                "projection требует конечные параметры 0 < near < far",
            ));
        }
        if !self.render.render_scale.is_finite()
            || self.render.render_scale <= 0.0
            || self.render.render_scale > 1.0
        {
            return Err(CameraSettingsError::new(
                label,
                "render.render_scale должен быть конечным и находиться в (0, 1]",
            ));
        }
        if !self
            .render
            .clear_color
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0)
        {
            return Err(CameraSettingsError::new(
                label,
                "render.clear_color должен содержать конечные компоненты >= 0",
            ));
        }
        if !self.color.exposure_ev.is_finite() || !(-16.0..=16.0).contains(&self.color.exposure_ev)
        {
            return Err(CameraSettingsError::new(
                label,
                "color.exposure_ev должен быть конечным и находиться в [-16, 16]",
            ));
        }
        Ok(())
    }
}

/// Перспективная проекция; FOV измеряется по более узкой стороне окна.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectionSettings {
    pub fov_deg: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for ProjectionSettings {
    fn default() -> Self {
        Self {
            fov_deg: 60.0,
            near: 0.1,
            far: 1000.0,
        }
    }
}

/// Масштаб render resolution, clear color сцены и пространственная фильтрация.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraRenderSettings {
    pub render_scale: f32,
    pub clear_color: [f32; 3],
    pub anti_aliasing: AntiAliasing,
}

impl Default for CameraRenderSettings {
    fn default() -> Self {
        Self {
            render_scale: 1.0,
            clear_color: [0.1, 0.1, 0.12],
            anti_aliasing: AntiAliasing::Fxaa,
        }
    }
}

/// Пространственная фильтрация в output pass камеры.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AntiAliasing {
    #[default]
    Fxaa,
    None,
    Taa,
    Fsr,
    Dlss,
}

/// Ручная экспозиция и оператор преобразования HDR в LDR.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraColorSettings {
    pub exposure_ev: f32,
    pub tonemapper: Tonemapper,
}

impl Default for CameraColorSettings {
    fn default() -> Self {
        Self {
            exposure_ev: 0.0,
            tonemapper: Tonemapper::Aces,
        }
    }
}

/// Оператор преобразования цвета HDR сцены в цвет дисплея.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tonemapper {
    #[default]
    Aces,
    Neutral,
    None,
}

/// Ошибка разбора или проверки `.camerasettings`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraSettingsError {
    pub label: String,
    pub message: String,
}

impl CameraSettingsError {
    fn new(label: &str, message: impl Into<String>) -> Self {
        Self {
            label: label.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for CameraSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.message)
    }
}

impl Error for CameraSettingsError {}

#[cfg(test)]
mod tests {
    use super::{
        AntiAliasing, CameraColorSettings, CameraRenderSettings, CameraSettings,
        ProjectionSettings, Tonemapper,
    };

    #[test]
    fn defaults_and_partial_tables_fill_every_camera_setting() {
        let settings = CameraSettings::parse("default.camerasettings", "").unwrap();
        assert_eq!(settings, CameraSettings::default());
        assert_eq!(settings.projection, ProjectionSettings::default());
        assert_eq!(settings.render, CameraRenderSettings::default());
        assert_eq!(settings.color, CameraColorSettings::default());
    }

    #[test]
    fn parse_and_toml_round_trip_keep_all_nested_values() {
        let settings = CameraSettings::parse(
            "custom.camerasettings",
            r#"
[projection]
fov_deg = 75
near = 0.25
far = 200
[render]
render_scale = 0.75
clear_color = [0.2, 0.3, 1.2]
anti_aliasing = "none"
[color]
exposure_ev = -2
tonemapper = "neutral"
"#,
        )
        .unwrap();
        let serialized = settings.to_toml().unwrap();
        assert_eq!(
            CameraSettings::parse("round-trip.camerasettings", &serialized).unwrap(),
            settings,
            "{serialized}"
        );
        assert_eq!(settings.render.anti_aliasing, AntiAliasing::None);
        assert_eq!(settings.color.tonemapper, Tonemapper::Neutral);
    }

    #[test]
    fn unknown_fields_are_denied_at_every_table_level() {
        for input in [
            "unexpected = true",
            "[projection]\nunexpected = true",
            "[render]\nunexpected = true",
            "[color]\nunexpected = true",
        ] {
            assert!(
                CameraSettings::parse("unknown.camerasettings", input).is_err(),
                "accepted {input}"
            );
        }
    }

    #[test]
    fn enum_values_are_lowercase_and_unknown_values_fail() {
        for input in [
            "[render]\nanti_aliasing = \"FXAA\"",
            "[render]\nanti_aliasing = \"msaa\"",
            "[color]\ntonemapper = \"ACES\"",
            "[color]\ntonemapper = \"filmic\"",
        ] {
            let error = CameraSettings::parse("bad-enum.camerasettings", input).unwrap_err();
            assert!(error.to_string().contains("bad-enum.camerasettings"));
        }
    }

    #[test]
    fn anti_aliasing_modes_round_trip_with_lowercase_names() {
        for (name, expected) in [
            ("fxaa", AntiAliasing::Fxaa),
            ("none", AntiAliasing::None),
            ("taa", AntiAliasing::Taa),
            ("fsr", AntiAliasing::Fsr),
            ("dlss", AntiAliasing::Dlss),
        ] {
            let settings = CameraSettings::parse(
                "anti-aliasing.camerasettings",
                &format!("[render]\nanti_aliasing = \"{name}\""),
            )
            .unwrap();
            assert_eq!(settings.render.anti_aliasing, expected);
            assert!(
                settings
                    .to_toml()
                    .unwrap()
                    .contains(&format!("anti_aliasing = \"{name}\""))
            );
        }
    }

    #[test]
    fn projection_scale_color_and_exposure_ranges_are_checked() {
        for input in [
            "[projection]\nfov_deg = 0.0",
            "[projection]\nfov_deg = 180.0",
            "[projection]\nfov_deg = nan",
            "[projection]\nfov_deg = inf",
            "[projection]\nnear = 0.0",
            "[projection]\nnear = 1001.0",
            "[projection]\nnear = nan",
            "[projection]\nfar = 0.0",
            "[projection]\nfar = 0.05",
            "[projection]\nfar = nan",
            "[projection]\nfar = inf",
            "[render]\nrender_scale = 0.0",
            "[render]\nrender_scale = 1.01",
            "[render]\nrender_scale = nan",
            "[render]\nclear_color = [0.0, -0.1, 0.0]",
            "[render]\nclear_color = [0.0, inf, 0.0]",
            "[color]\nexposure_ev = -16.1",
            "[color]\nexposure_ev = 16.1",
            "[color]\nexposure_ev = nan",
        ] {
            assert!(
                CameraSettings::parse("invalid.camerasettings", input).is_err(),
                "accepted {input}"
            );
        }
    }
}
