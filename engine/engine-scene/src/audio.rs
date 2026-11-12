//! Звуковые компоненты сцены. Модуль не зависит от аудио-бэкенда: воспроизведение и микширование — в `engine-audio`.

use crate::document::{SceneError, require_asset_path, virtual_path_serde};
use crate::world::{ObjectHandle, World};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Идентификатор голоса. Выделяется один раз за процесс и не повторяется, поэтому
/// скрипт может сохранить его и позже остановить голос.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VoiceId(NonZeroU64);

static NEXT_VOICE_ID: AtomicU64 = AtomicU64::new(1);

impl VoiceId {
    pub fn allocate() -> Self {
        let raw = NEXT_VOICE_ID.fetch_add(1, Ordering::Relaxed);
        Self(NonZeroU64::new(raw).expect("счётчик голосов не переполняется"))
    }

    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// Восстанавливает id из числа, которое скрипт получил от хоста. Ноль не является id.
    pub fn from_raw(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(Self)
    }
}

/// Параметры одноразового или зацикленного клипа, запущенного скриптом.
/// `position` делает голос пространственным с фиксированной точкой излучения; `follow` ведёт
/// излучатель за объектом сцены, пока тот жив и активен.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipParams {
    pub group: AudioGroup,
    pub volume: f32,
    pub looping: bool,
    pub position: Option<[f32; 3]>,
    pub follow: Option<ObjectHandle>,
    /// Доля 3D-микширования `0..=1`; `0` — 2D-клип, даже если задана позиция.
    pub spatial_blend: f32,
    pub min_distance: f32,
    pub max_distance: f32,
    pub force_mono: bool,
    /// Декодировать клип по частям во время воспроизведения, не загружая его целиком.
    pub stream: bool,
    /// Длительность плавного появления в секундах; `0` — сразу на полной громкости.
    pub fade_in: f32,
}

impl Default for ClipParams {
    fn default() -> Self {
        Self {
            group: AudioGroup::Effects,
            volume: 1.0,
            looping: false,
            position: None,
            follow: None,
            spatial_blend: 1.0,
            min_distance: 1.0,
            max_distance: 30.0,
            force_mono: false,
            stream: false,
            fade_in: 0.0,
        }
    }
}

/// Запрос к звуковой подсистеме, накопленный скриптами за кадр.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioRequest {
    /// `obj:play_sound()`: запуск источника объекта.
    PlaySource {
        object: ObjectHandle,
        voice: VoiceId,
    },
    /// `obj:stop_sound(fade)`: остановка текущего голоса источника. `fade` — затухание в секундах;
    /// `0` — короткое затухание без щелчка.
    StopSource { object: ObjectHandle, fade: f32 },
    /// `obj:set_sound_volume(v)`: громкость текущего голоса и следующих запусков источника.
    SetSourceVolume { object: ObjectHandle, volume: f32 },
    /// `rve.audio.play(path, ...)`: клип без привязки к объекту.
    PlayClip {
        voice: VoiceId,
        path: VirtualPath,
        params: ClipParams,
    },
    /// `rve.audio.stop(id, fade)`; `fade` как у [`AudioRequest::StopSource`].
    StopVoice { voice: VoiceId, fade: f32 },
    /// `rve.audio.set_voice_position(id, pos)`: фиксирует излучатель и снимает слежение за объектом.
    SetVoicePosition { voice: VoiceId, position: [f32; 3] },
    /// `rve.audio.set_group_volume(group, v)`.
    SetGroupVolume { group: AudioGroup, volume: f32 },
    /// `rve.audio.set_group_effects(group, effects)`: фильтры и реверберация шины группы.
    SetGroupEffects {
        group: AudioGroup,
        effects: AudioGroupEffects,
    },
}

/// Реверберация шины группы. Все параметры в диапазоне `0..=1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioReverb {
    /// Размер комнаты: длина хвоста.
    pub room_size: f32,
    /// Поглощение высоких частот в хвосте.
    pub damping: f32,
    /// Доля обработанного сигнала; сухой сигнал остаётся на полной громкости.
    pub wet: f32,
}

/// Эффекты шины группы. Частота среза `0` выключает фильтр; `reverb = None` — без реверберации.
/// Значение по умолчанию выключает всё.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioGroupEffects {
    /// Срез low-pass в Гц: убирает частоты выше.
    pub lowpass_hz: f32,
    /// Срез high-pass в Гц: убирает частоты ниже.
    pub highpass_hz: f32,
    pub reverb: Option<AudioReverb>,
}

/// Допустимые частоты среза фильтров групп, Гц.
pub const MIN_AUDIO_FILTER_HZ: f32 = 10.0;
pub const MAX_AUDIO_FILTER_HZ: f32 = 22_000.0;
/// Наибольшая длительность fade-in и fade-out, секунды.
pub const MAX_AUDIO_FADE: f32 = 60.0;

/// Причина отказа для эффектов группы или `None`.
pub fn group_effects_problem(effects: &AudioGroupEffects) -> Option<&'static str> {
    let cutoff = |hz: f32| hz == 0.0 || (MIN_AUDIO_FILTER_HZ..=MAX_AUDIO_FILTER_HZ).contains(&hz);
    if !cutoff(effects.lowpass_hz) || !cutoff(effects.highpass_hz) {
        return Some(
            "частота среза фильтра должна быть 0 (выключен) или в диапазоне 10..=22000 Гц",
        );
    }
    if let Some(reverb) = effects.reverb {
        let unit = |value: f32| (0.0..=1.0).contains(&value);
        if !unit(reverb.room_size) || !unit(reverb.damping) || !unit(reverb.wet) {
            return Some("room_size, damping и wet реверберации должны быть в диапазоне 0..=1");
        }
    }
    None
}

/// Причина отказа для длительности fade-in или fade-out или `None`.
pub fn fade_problem(seconds: f32) -> Option<&'static str> {
    if (0.0..=MAX_AUDIO_FADE).contains(&seconds) {
        None
    } else {
        Some("длительность затухания должна быть конечной и в диапазоне 0..=60 секунд")
    }
}

/// Заявки звука на смену действующей активности: источники скрытых объектов останавливаются,
/// у показанных объектов с `play_on_start` источник запускается заново, как при повторном
/// включении объекта. Вызывающий передаёт объекты, у которых активность изменилась.
pub fn activity_audio_requests(
    world: &World,
    hidden: &[ObjectHandle],
    shown: &[ObjectHandle],
) -> Vec<AudioRequest> {
    hidden
        .iter()
        .map(|&object| AudioRequest::StopSource { object, fade: 0.0 })
        .chain(
            shown
                .iter()
                .filter(|&&object| {
                    world.is_alive(object)
                        && world
                            .audio_source(object)
                            .is_some_and(|audio| audio.play_on_start)
                })
                .map(|&object| AudioRequest::PlaySource {
                    object,
                    voice: VoiceId::allocate(),
                }),
        )
        .collect()
}

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

/// Кривая затухания пространственного источника. `Inverse` — прежняя кривая сцен без этого ключа.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioRolloff {
    #[default]
    Inverse,
    Linear,
}

/// Звуковой источник на объекте сцены: клип и параметры воспроизведения.
///
/// В файле `spatial = true|false` (прежний ключ) задаёт `spatial_blend` 1 или 0; дробная доля
/// пишется ключом `spatial_blend`. Остальные ключи пишутся только при отличии от умолчания,
/// поэтому сцены без новых возможностей сохраняются как раньше.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "AudioSourceWire", into = "AudioSourceWire")]
pub struct AudioSourceComponent {
    /// Виртуальный путь `.wav` или `.ogg`; обязан быть объявлен в `[[assets]]`.
    pub clip: VirtualPath,
    pub group: AudioGroup,
    pub volume: f32,
    pub looping: bool,
    pub play_on_start: bool,
    /// Доля пространственного микширования `0..=1`: `0` — 2D, `1` — полностью по позиции объекта.
    pub spatial_blend: f32,
    pub min_distance: f32,
    pub max_distance: f32,
    pub rolloff: AudioRolloff,
    /// Множитель скорости и высоты тона, больше нуля.
    pub pitch: f32,
    /// Сводит клип к моно до панорамы.
    pub force_mono: bool,
    /// Декодировать клип по частям при воспроизведении: для длинной музыки и ambient.
    pub stream: bool,
    /// Плавное появление голоса в секундах; `0` — без него.
    pub fade_in: f32,
}

impl AudioSourceComponent {
    /// Источник с настройками по умолчанию для клипа; редактор использует его при добавлении.
    pub fn with_clip(clip: VirtualPath) -> Self {
        Self {
            clip,
            group: AudioGroup::default(),
            volume: default_volume(),
            looping: false,
            play_on_start: false,
            spatial_blend: 0.0,
            min_distance: default_min_distance(),
            max_distance: default_max_distance(),
            rolloff: AudioRolloff::default(),
            pitch: default_pitch(),
            force_mono: false,
            stream: false,
            fade_in: 0.0,
        }
    }
}

/// Файловое представление источника: порядок полей совпадает с прежним форматом.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioSourceWire {
    #[serde(with = "virtual_path_serde")]
    clip: VirtualPath,
    #[serde(default)]
    group: AudioGroup,
    #[serde(default = "default_volume")]
    volume: f32,
    #[serde(default)]
    looping: bool,
    #[serde(default)]
    play_on_start: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spatial: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spatial_blend: Option<f32>,
    #[serde(default = "default_min_distance")]
    min_distance: f32,
    #[serde(default = "default_max_distance")]
    max_distance: f32,
    #[serde(default, skip_serializing_if = "is_default_rolloff")]
    rolloff: AudioRolloff,
    #[serde(default = "default_pitch", skip_serializing_if = "is_default_pitch")]
    pitch: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    force_mono: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    fade_in: f32,
}

impl From<AudioSourceComponent> for AudioSourceWire {
    fn from(audio: AudioSourceComponent) -> Self {
        let blend = audio.spatial_blend;
        let binary = blend == 0.0 || blend == 1.0;
        Self {
            clip: audio.clip,
            group: audio.group,
            volume: audio.volume,
            looping: audio.looping,
            play_on_start: audio.play_on_start,
            spatial: binary.then_some(blend == 1.0),
            spatial_blend: (!binary).then_some(blend),
            min_distance: audio.min_distance,
            max_distance: audio.max_distance,
            rolloff: audio.rolloff,
            pitch: audio.pitch,
            force_mono: audio.force_mono,
            stream: audio.stream,
            fade_in: audio.fade_in,
        }
    }
}

impl TryFrom<AudioSourceWire> for AudioSourceComponent {
    type Error = String;

    fn try_from(wire: AudioSourceWire) -> Result<Self, Self::Error> {
        let spatial_blend = match (wire.spatial, wire.spatial_blend) {
            (Some(_), Some(_)) => {
                return Err("audio_source: задайте только одно из spatial и spatial_blend".into());
            }
            (Some(spatial), None) => f32::from(u8::from(spatial)),
            (None, Some(blend)) => blend,
            (None, None) => 0.0,
        };
        Ok(Self {
            clip: wire.clip,
            group: wire.group,
            volume: wire.volume,
            looping: wire.looping,
            play_on_start: wire.play_on_start,
            spatial_blend,
            min_distance: wire.min_distance,
            max_distance: wire.max_distance,
            rolloff: wire.rolloff,
            pitch: wire.pitch,
            force_mono: wire.force_mono,
            stream: wire.stream,
            fade_in: wire.fade_in,
        })
    }
}

fn is_default_rolloff(rolloff: &AudioRolloff) -> bool {
    *rolloff == AudioRolloff::Inverse
}

fn is_zero(value: &f32) -> bool {
    *value == 0.0
}

fn is_default_pitch(pitch: &f32) -> bool {
    *pitch == 1.0
}

fn default_pitch() -> f32 {
    1.0
}

/// Наибольший множитель скорости клипа. Ниже нуля и выше этого значения значения отклоняются.
pub const MAX_AUDIO_PITCH: f32 = 4.0;

fn default_volume() -> f32 {
    1.0
}

fn default_min_distance() -> f32 {
    1.0
}

fn default_max_distance() -> f32 {
    30.0
}

/// Наибольшая громкость источника, группы и голоса (усиление ×4). Значение совпадает с
/// `rve_sdk::MAX_AUDIO_VOLUME`; тест в engine-script сверяет обе константы.
pub const MAX_AUDIO_VOLUME: f32 = 4.0;

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
    match audio_source_problem(audio) {
        Some(problem) => Err(SceneError::new(source, Some(object_id), problem)),
        None => Ok(()),
    }
}

/// Голос пространственный, если клип задан позицией или `follow` и `spatial_blend > 0`. Без этого
/// мост не создаёт дистанций, и `set_voice_position` на таком голосе не имеет эффекта.
pub fn clip_is_spatial(params: &ClipParams) -> bool {
    (params.position.is_some() || params.follow.is_some()) && params.spatial_blend > 0.0
}

/// Причина отказа для пространственных опций и `fade_in` клипа скрипта или `None`.
/// Дистанции проверяются только при `spatial_blend > 0`: у 2D-клипа они не используются.
pub fn clip_spatial_problem(params: &ClipParams) -> Option<&'static str> {
    if !params.spatial_blend.is_finite() || !(0.0..=1.0).contains(&params.spatial_blend) {
        return Some("spatial_blend должен быть конечным и в диапазоне 0..=1");
    }
    if fade_problem(params.fade_in).is_some() {
        return Some("fade_in должен быть конечным и в диапазоне 0..=60 секунд");
    }
    if params.position.is_some() && params.follow.is_some() {
        return Some("position и follow нельзя задать одновременно");
    }
    if params.spatial_blend > 0.0
        && (!params.min_distance.is_finite()
            || !params.max_distance.is_finite()
            || params.min_distance <= 0.0
            || params.min_distance >= params.max_distance)
    {
        return Some(
            "min_distance и max_distance должны удовлетворять 0 < min_distance < max_distance",
        );
    }
    None
}

/// Причина отказа для значений источника (без проверки клипа и `[[assets]]`) или `None`.
/// Тот же набор правил применяют сцена, скрипты и редактор.
pub fn audio_source_problem(audio: &AudioSourceComponent) -> Option<&'static str> {
    if audio.group == AudioGroup::Master {
        return Some("audio_source.group не может быть master");
    }
    if !audio.volume.is_finite() || !(0.0..=MAX_AUDIO_VOLUME).contains(&audio.volume) {
        return Some("audio_source.volume должен быть конечным и в диапазоне 0..=4");
    }
    if !audio.spatial_blend.is_finite() || !(0.0..=1.0).contains(&audio.spatial_blend) {
        return Some("audio_source.spatial_blend должен быть конечным и в диапазоне 0..=1");
    }
    if !audio.pitch.is_finite() || audio.pitch <= 0.0 || audio.pitch > MAX_AUDIO_PITCH {
        return Some("audio_source.pitch должен быть конечным и в диапазоне (0..=4]");
    }
    if fade_problem(audio.fade_in).is_some() {
        return Some("audio_source.fade_in должен быть конечным и в диапазоне 0..=60 секунд");
    }
    if !audio.min_distance.is_finite()
        || !audio.max_distance.is_finite()
        || audio.min_distance <= 0.0
        || audio.min_distance >= audio.max_distance
    {
        return Some("audio_source должен удовлетворять 0 < min_distance < max_distance");
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::{AudioGroup, AudioRolloff, SceneDocument, World};

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
        assert_eq!(audio.spatial_blend, 0.0);
        assert_eq!(audio.min_distance, 1.0);
        assert_eq!(audio.max_distance, 30.0);
        assert_eq!(audio.rolloff, AudioRolloff::Inverse);
        assert_eq!(audio.pitch, 1.0);
        assert!(!audio.force_mono);
    }

    #[test]
    fn legacy_spatial_bool_maps_to_blend_and_back() {
        let fields = "clip = \"/audio/hum.wav\"\nspatial = true";
        let document = parse(&source(fields)).unwrap();
        assert_eq!(
            document.objects[1]
                .audio_source
                .as_ref()
                .unwrap()
                .spatial_blend,
            1.0
        );
        let text = toml::to_string_pretty(&document).unwrap();
        assert!(text.contains("spatial = true"), "{text}");
        assert!(!text.contains("spatial_blend"), "{text}");

        let off = parse(&source("clip = \"/audio/hum.wav\"\nspatial = false")).unwrap();
        let text = toml::to_string_pretty(&off).unwrap();
        assert!(text.contains("spatial = false"), "{text}");
    }

    #[test]
    fn fractional_blend_is_written_under_its_own_key() {
        let document = parse(&source("clip = \"/audio/hum.wav\"\nspatial_blend = 0.25")).unwrap();
        let audio = document.objects[1].audio_source.clone().unwrap();
        assert_eq!(audio.spatial_blend, 0.25);
        let text = toml::to_string_pretty(&document).unwrap();
        assert!(text.contains("spatial_blend = 0.25"), "{text}");
        assert!(!text.contains("spatial = "), "{text}");
        assert_eq!(parse(&text).unwrap().objects[1].audio_source, Some(audio));
    }

    #[test]
    fn spatial_and_blend_together_are_rejected() {
        let fields = "clip = \"/audio/hum.wav\"\nspatial = true\nspatial_blend = 0.5";
        assert!(parse(&source(fields)).is_err());
    }

    #[test]
    fn new_fields_round_trip_and_stay_out_of_default_output() {
        let fields =
            "clip = \"/audio/hum.wav\"\nrolloff = \"linear\"\npitch = 1.5\nforce_mono = true";
        let document = parse(&source(fields)).unwrap();
        let audio = document.objects[1].audio_source.clone().unwrap();
        assert_eq!(audio.rolloff, AudioRolloff::Linear);
        assert_eq!((audio.pitch, audio.force_mono), (1.5, true));
        let text = toml::to_string_pretty(&document).unwrap();
        assert_eq!(parse(&text).unwrap().objects[1].audio_source, Some(audio));

        let plain =
            toml::to_string_pretty(&parse(&source("clip = \"/audio/hum.wav\"")).unwrap()).unwrap();
        assert!(
            !plain.contains("rolloff") && !plain.contains("pitch") && !plain.contains("force_mono")
        );
    }

    #[test]
    fn blend_and_pitch_must_be_in_range() {
        for case in [
            "spatial_blend = 1.5",
            "spatial_blend = -0.1",
            "spatial_blend = nan",
        ] {
            let fields = format!("clip = \"/audio/hum.wav\"\n{case}");
            assert!(error_for(&fields).contains("spatial_blend"), "{case}");
        }
        for case in ["pitch = 0.0", "pitch = -1.0", "pitch = 4.5", "pitch = inf"] {
            let fields = format!("clip = \"/audio/hum.wav\"\n{case}");
            assert!(error_for(&fields).contains("pitch"), "{case}");
        }
    }

    #[test]
    fn full_source_round_trips_through_toml() {
        let fields = "clip = \"/audio/loop.ogg\"\ngroup = \"ambient\"\nvolume = 0.5\nlooping = true\nplay_on_start = true\nspatial = true\nmin_distance = 2.0\nmax_distance = 40.0";
        let document = parse(&source(fields)).unwrap();
        let audio = document.objects[1].audio_source.clone().unwrap();
        assert_eq!(audio.group, AudioGroup::Ambient);
        assert!(audio.looping && audio.play_on_start);
        assert_eq!(audio.spatial_blend, 1.0);
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
    fn stream_and_fade_in_round_trip_and_stay_absent_by_default() {
        let fields = "clip = \"/audio/loop.ogg\"\nstream = true\nfade_in = 2.5";
        let document = parse(&source(fields)).unwrap();
        let audio = document.objects[1].audio_source.clone().unwrap();
        assert!(audio.stream);
        assert_eq!(audio.fade_in, 2.5);
        let text = toml::to_string_pretty(&document).unwrap();
        assert!(
            text.contains("stream = true") && text.contains("fade_in = 2.5"),
            "{text}"
        );
        assert_eq!(parse(&text).unwrap().objects[1].audio_source, Some(audio));

        let plain = parse(&source("clip = \"/audio/hum.wav\"")).unwrap();
        let text = toml::to_string_pretty(&plain).unwrap();
        assert!(
            !text.contains("stream") && !text.contains("fade_in"),
            "{text}"
        );
    }

    #[test]
    fn fade_in_must_be_finite_and_within_the_limit() {
        for case in ["fade_in = -1.0", "fade_in = 61.0", "fade_in = nan"] {
            let fields = format!("clip = \"/audio/hum.wav\"\n{case}");
            assert!(error_for(&fields).contains("fade_in"), "{case}");
        }
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
        assert!(parse(&source("clip = \"/audio/hum.wav\"\ndoppler = 2.0")).is_err());
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
    fn volume_is_capped_at_the_maximum_gain() {
        let at_cap = format!(
            "clip = \"/audio/hum.wav\"\nvolume = {}",
            super::MAX_AUDIO_VOLUME
        );
        assert!(parse(&source(&at_cap)).is_ok());
        assert!(error_for("clip = \"/audio/hum.wav\"\nvolume = 4.5").contains("volume"));
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
        assert_eq!(audio.spatial_blend, 1.0);
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

#[cfg(test)]
mod clip_spatial_tests {
    use super::{ClipParams, clip_is_spatial, clip_spatial_problem};
    use crate::ObjectHandle;

    fn two_d() -> ClipParams {
        ClipParams {
            spatial_blend: 0.0,
            min_distance: 0.0,
            max_distance: 0.0,
            ..ClipParams::default()
        }
    }

    #[test]
    fn two_d_clip_ignores_distances_that_only_matter_for_spatial_mixing() {
        assert_eq!(clip_spatial_problem(&two_d()), None);
        let spatial_blend_without_distances = ClipParams {
            spatial_blend: 0.5,
            ..two_d()
        };
        assert!(clip_spatial_problem(&spatial_blend_without_distances).is_some());
    }

    #[test]
    fn position_and_follow_together_are_rejected() {
        let follow = ObjectHandle::from_abi(1).expect("живой хендл первого объекта");
        let both = ClipParams {
            position: Some([1.0, 0.0, 0.0]),
            follow: Some(follow),
            ..ClipParams::default()
        };
        assert_eq!(
            clip_spatial_problem(&both),
            Some("position и follow нельзя задать одновременно")
        );
    }

    #[test]
    fn clip_is_spatial_needs_a_position_or_follow_and_a_positive_blend() {
        let follow = ObjectHandle::from_abi(1).expect("живой хендл первого объекта");
        assert!(!clip_is_spatial(&two_d()));
        let positioned = ClipParams {
            position: Some([0.0; 3]),
            ..ClipParams::default()
        };
        assert!(clip_is_spatial(&positioned));
        let followed = ClipParams {
            follow: Some(follow),
            ..ClipParams::default()
        };
        assert!(clip_is_spatial(&followed));
        let positioned_flat = ClipParams {
            position: Some([0.0; 3]),
            spatial_blend: 0.0,
            ..ClipParams::default()
        };
        assert!(!clip_is_spatial(&positioned_flat));
    }
}

#[cfg(test)]
mod effects_tests {
    use super::{AudioGroupEffects, AudioReverb, fade_problem, group_effects_problem};

    #[test]
    fn group_effects_accept_disabled_and_valid_settings() {
        assert_eq!(group_effects_problem(&AudioGroupEffects::default()), None);
        let effects = AudioGroupEffects {
            lowpass_hz: 800.0,
            highpass_hz: 120.0,
            reverb: Some(AudioReverb {
                room_size: 0.7,
                damping: 0.5,
                wet: 0.3,
            }),
        };
        assert_eq!(group_effects_problem(&effects), None);
    }

    #[test]
    fn group_effects_reject_out_of_range_cutoffs_and_reverb() {
        for hz in [5.0, -1.0, 30_000.0, f32::NAN] {
            let lowpass = AudioGroupEffects {
                lowpass_hz: hz,
                ..AudioGroupEffects::default()
            };
            let highpass = AudioGroupEffects {
                highpass_hz: hz,
                ..AudioGroupEffects::default()
            };
            assert!(group_effects_problem(&lowpass).is_some(), "{hz}");
            assert!(group_effects_problem(&highpass).is_some(), "{hz}");
        }
        let reverb = AudioGroupEffects {
            reverb: Some(AudioReverb {
                room_size: 1.5,
                damping: 0.5,
                wet: f32::NAN,
            }),
            ..AudioGroupEffects::default()
        };
        assert!(group_effects_problem(&reverb).is_some());
    }

    #[test]
    fn fade_must_be_finite_and_within_the_limit() {
        assert_eq!(fade_problem(0.0), None);
        assert_eq!(fade_problem(60.0), None);
        for seconds in [-0.1, 60.5, f32::NAN, f32::INFINITY] {
            assert!(fade_problem(seconds).is_some(), "{seconds}");
        }
    }
}
