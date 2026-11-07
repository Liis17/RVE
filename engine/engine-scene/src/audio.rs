//! Звуковые компоненты сцены. Модуль не зависит от аудио-бэкенда: воспроизведение и микширование — в `engine-audio`.

use crate::document::{SceneError, require_asset_path, virtual_path_serde};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// Группа громкости. `Master` — общий множитель; источники сцены его не назначают.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioGroup {
    Master,
    Music,
    #[default]
    Effects,
    Ambient,
}

/// Звуковой источник на объекте сцены: клип и параметры воспроизведения.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSourceComponent {
    /// Виртуальный путь `.wav` или `.ogg`; обязан быть объявлен в `[[assets]]`.
    #[serde(with = "virtual_path_serde")]
    pub clip: VirtualPath,
    #[serde(default)]
    pub group: AudioGroup,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub looping: bool,
    #[serde(default)]
    pub play_on_start: bool,
    /// Пространственное затухание и панорама по позиции объекта относительно активной камеры.
    #[serde(default)]
    pub spatial: bool,
    #[serde(default = "default_min_distance")]
    pub min_distance: f32,
    #[serde(default = "default_max_distance")]
    pub max_distance: f32,
}

fn default_volume() -> f32 {
    1.0
}

fn default_min_distance() -> f32 {
    1.0
}

fn default_max_distance() -> f32 {
    30.0
}

/// Проверяет клип, громкость и дистанции; `assets` — объявленные `[[assets]]` документа.
pub(crate) fn validate_audio_source(
    source: &str,
    object_id: &str,
    assets: &HashSet<VirtualPath>,
    audio: &AudioSourceComponent,
) -> Result<(), SceneError> {
    require_asset_path(source, object_id, assets, &audio.clip, "аудиоклип")?;
    let extension = Path::new(audio.clip.as_str())
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    if !matches!(extension.as_deref(), Some("wav" | "ogg")) {
        return Err(SceneError::new(
            source,
            Some(object_id),
            format!(
                "аудиоклип /{} должен иметь расширение .wav или .ogg",
                audio.clip.as_str()
            ),
        ));
    }
    if audio.group == AudioGroup::Master {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "audio_source.group не может быть master",
        ));
    }
    if !audio.volume.is_finite() || audio.volume < 0.0 {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "audio_source.volume должен быть конечным и >= 0",
        ));
    }
    if !audio.min_distance.is_finite()
        || !audio.max_distance.is_finite()
        || audio.min_distance <= 0.0
        || audio.min_distance >= audio.max_distance
    {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "audio_source должен удовлетворять 0 < min_distance < max_distance",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{AudioGroup, SceneDocument, World};

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    const ASSETS: &str = "[[assets]]\npath = \"/audio/hum.wav\"\n[[assets]]\npath = \"/audio/loop.ogg\"\n[[assets]]\npath = \"/models/box.fbx\"\n";

    fn parse(input: &str) -> Result<SceneDocument, crate::SceneError> {
        SceneDocument::parse("scenes/test.rvescene", input)
    }

    fn source(fields: &str) -> String {
        format!("{ASSETS}{CAMERA}[[objects]]\nid = \"speaker\"\n[objects.audio_source]\n{fields}\n")
    }

    fn error_for(fields: &str) -> String {
        parse(&source(fields)).unwrap_err().to_string()
    }

    #[test]
    fn minimal_source_gets_documented_defaults() {
        let document = parse(&source("clip = \"/audio/hum.wav\"")).unwrap();
        let audio = document.objects[1].audio_source.as_ref().unwrap();
        assert_eq!(audio.group, AudioGroup::Effects);
        assert_eq!(audio.volume, 1.0);
        assert!(!audio.looping);
        assert!(!audio.play_on_start);
        assert!(!audio.spatial);
        assert_eq!(audio.min_distance, 1.0);
        assert_eq!(audio.max_distance, 30.0);
    }

    #[test]
    fn full_source_round_trips_through_toml() {
        let fields = "clip = \"/audio/loop.ogg\"\ngroup = \"ambient\"\nvolume = 0.5\nlooping = true\nplay_on_start = true\nspatial = true\nmin_distance = 2.0\nmax_distance = 40.0";
        let document = parse(&source(fields)).unwrap();
        let audio = document.objects[1].audio_source.clone().unwrap();
        assert_eq!(audio.group, AudioGroup::Ambient);
        assert!(audio.looping && audio.play_on_start && audio.spatial);
        assert_eq!(audio.volume, 0.5);
        assert_eq!((audio.min_distance, audio.max_distance), (2.0, 40.0));

        let text = toml::to_string_pretty(&document).unwrap();
        let again = parse(&text).unwrap();
        assert_eq!(
            again.objects[1].audio_source,
            document.objects[1].audio_source
        );
    }

    #[test]
    fn scenes_without_audio_do_not_serialize_the_field() {
        let document = parse(CAMERA).unwrap();
        let text = toml::to_string_pretty(&document).unwrap();
        assert!(!text.contains("audio_source"), "{text}");
    }

    #[test]
    fn unknown_group_and_fields_are_rejected_by_the_schema() {
        assert!(parse(&source("clip = \"/audio/hum.wav\"\ngroup = \"sfx\"")).is_err());
        assert!(parse(&source("clip = \"/audio/hum.wav\"\npitch = 2.0")).is_err());
        assert!(parse(&source("volume = 1.0")).is_err(), "clip is required");
    }

    #[test]
    fn master_is_not_a_source_group() {
        assert!(error_for("clip = \"/audio/hum.wav\"\ngroup = \"master\"").contains("master"));
    }

    #[test]
    fn clip_must_be_declared_in_assets() {
        let text = format!(
            "{CAMERA}[[objects]]\nid = \"speaker\"\n[objects.audio_source]\nclip = \"/audio/hum.wav\"\n"
        );
        let error = parse(&text).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("speaker"));
        assert!(error.message.contains("[[assets]]"), "{error}");
    }

    #[test]
    fn clip_extension_must_be_wav_or_ogg() {
        assert!(error_for("clip = \"/models/box.fbx\"").contains(".wav или .ogg"));
        let uppercase = format!(
            "{CAMERA}[[assets]]\npath = \"/audio/HUM.WAV\"\n[[objects]]\nid = \"speaker\"\n[objects.audio_source]\nclip = \"/audio/HUM.WAV\"\n"
        );
        assert!(parse(&uppercase).is_ok());
    }

    #[test]
    fn volume_must_be_finite_and_non_negative() {
        for volume in ["-0.1", "nan", "inf"] {
            let fields = format!("clip = \"/audio/hum.wav\"\nvolume = {volume}");
            assert!(error_for(&fields).contains("volume"), "{volume}");
        }
        assert!(parse(&source("clip = \"/audio/hum.wav\"\nvolume = 0.0")).is_ok());
    }

    #[test]
    fn distances_must_satisfy_zero_below_min_below_max() {
        let cases = [
            "min_distance = 0.0",
            "min_distance = -1.0",
            "min_distance = 30.0",
            "max_distance = 1.0",
            "max_distance = nan",
        ];
        for case in cases {
            let fields = format!("clip = \"/audio/hum.wav\"\n{case}");
            assert!(error_for(&fields).contains("min_distance"), "{case}");
        }
    }

    #[test]
    fn world_exposes_sources_and_declared_asset_paths() {
        let text = source("clip = \"/audio/hum.wav\"\nspatial = true");
        let world = World::from_document(&parse(&text).unwrap()).unwrap();
        let speaker = world.handle("speaker").unwrap();
        let audio = world.audio_source(speaker).unwrap();
        assert_eq!(audio.clip.as_str(), "audio/hum.wav");
        assert!(audio.spatial);
        assert!(
            world
                .audio_source(world.handle("camera").unwrap())
                .is_none()
        );
        let paths: Vec<&str> = world
            .asset_paths()
            .iter()
            .map(|path| path.as_str())
            .collect();
        assert_eq!(paths, ["audio/hum.wav", "audio/loop.ogg", "models/box.fbx"]);
    }
}
