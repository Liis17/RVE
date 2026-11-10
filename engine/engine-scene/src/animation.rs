//! Компонент `animator` сцены и запросы управления аниматором. Модуль хранит только схему:
//! состояние проигрывания и загрузка клипов находятся в `engine-animation` и мосте runtime.

use crate::document::{SceneError, SceneObject, require_asset_path, require_extension};
use crate::world::ObjectHandle;
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// Наибольшее число точек одномерного blend tree; совпадает с `engine_animation::MAX_TREE_POINTS`.
pub const MAX_BLEND_TREE_POINTS: usize = 16;

/// Аниматор объекта с `mesh_renderer`: клипы из FBX и декларативные blend tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnimatorComponent {
    /// Дополнительные FBX только с клипами; обязаны быть объявлены в `[[assets]]`.
    #[serde(default, with = "clip_paths", skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<VirtualPath>,
    /// Движение, которое запускается при старте, если `play_on_start` включён.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_motion: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub play_on_start: bool,
    /// Значение по умолчанию для `play_animation`, если скрипт не задал `loop`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub looping: bool,
    #[serde(default = "default_speed", skip_serializing_if = "is_default_speed")]
    pub speed: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blend_trees: Vec<BlendTree>,
}

impl AnimatorComponent {
    /// Аниматор с настройками по умолчанию; редактор использует его при добавлении компонента.
    pub fn new() -> Self {
        Self {
            clips: Vec::new(),
            default_motion: None,
            play_on_start: true,
            looping: true,
            speed: 1.0,
            blend_trees: Vec::new(),
        }
    }
}

impl Default for AnimatorComponent {
    fn default() -> Self {
        Self::new()
    }
}

/// Одномерный blend tree: значение параметра выбирает и смешивает клипы по порогам.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlendTree {
    pub name: String,
    pub parameter: String,
    pub points: Vec<BlendTreePoint>,
}

/// Точка blend tree; `clip` — имя клипа: stack модели или stem файла из `clips`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlendTreePoint {
    pub clip: String,
    pub threshold: f32,
}

/// Запрос к аниматору, накопленный скриптами за кадр.
#[derive(Clone, Debug, PartialEq)]
pub enum AnimationRequest {
    /// `obj:play_animation(motion, options)`.
    Play {
        object: ObjectHandle,
        motion: String,
        fade: f32,
        looping: Option<bool>,
        then: Option<String>,
    },
    /// `obj:set_animation_weight(motion, weight, fade)`.
    SetWeight {
        object: ObjectHandle,
        motion: String,
        weight: f32,
        fade: f32,
    },
    /// `obj:set_animation_parameter(name, value)`.
    SetParameter {
        object: ObjectHandle,
        name: String,
        value: f32,
    },
    /// `obj:set_animation_speed(speed)`.
    SetSpeed { object: ObjectHandle, speed: f32 },
    /// `obj:stop_animation()`: мгновенная остановка всех движений.
    Stop { object: ObjectHandle },
}

/// Проверяет аниматор объекта: зависимости объекта и поля компонента.
pub(crate) fn validate_animator(
    source: &str,
    object: &SceneObject,
    asset_paths: &HashSet<VirtualPath>,
    animator: &AnimatorComponent,
) -> Result<(), SceneError> {
    let id = object.id.as_str();
    if object.mesh_renderer.is_none() {
        return Err(SceneError::new(
            source,
            Some(id),
            "animator требует mesh_renderer",
        ));
    }
    if object.lod_group.is_some() {
        return Err(SceneError::new(
            source,
            Some(id),
            "animator несовместим с lod_group: LOD для skinned-мешей не поддерживается",
        ));
    }
    validate_animator_fields(source, id, asset_paths, animator)
}

/// Проверяет поля компонента: скорость, клипы, имена и blend tree.
///
/// Используется и для override префаба, который заменяет компонент целиком.
pub(crate) fn validate_animator_fields(
    source: &str,
    id: &str,
    asset_paths: &HashSet<VirtualPath>,
    animator: &AnimatorComponent,
) -> Result<(), SceneError> {
    let error = |message: String| SceneError::new(source, Some(id), message);
    if !animator.speed.is_finite() || animator.speed < 0.0 {
        return Err(error(
            "animator.speed должен быть конечным и не меньше 0".to_owned(),
        ));
    }
    let mut clip_names = HashSet::new();
    for clip in &animator.clips {
        require_asset_path(source, id, asset_paths, clip, "клип")?;
        require_extension(source, id, clip, "fbx", "клип анимации")?;
        let stem = clip_stem(clip);
        if !clip_names.insert(stem.to_ascii_lowercase()) {
            return Err(error(format!(
                "имя клипа `{stem}` повторяется без учёта регистра"
            )));
        }
    }
    if animator.default_motion.as_deref() == Some("") {
        return Err(error(
            "animator.default_motion не может быть пустым".to_owned(),
        ));
    }
    let mut tree_names = HashSet::new();
    for tree in &animator.blend_trees {
        if !is_identifier(&tree.name) {
            return Err(error(format!(
                "имя blend tree `{}` должно быть идентификатором",
                tree.name
            )));
        }
        if !tree_names.insert(tree.name.clone()) {
            return Err(error(format!("blend tree `{}` повторяется", tree.name)));
        }
        if clip_names.contains(&tree.name.to_ascii_lowercase()) {
            return Err(error(format!(
                "имя blend tree `{}` совпадает с именем клипа",
                tree.name
            )));
        }
        if !is_identifier(&tree.parameter) {
            return Err(error(format!(
                "параметр blend tree `{}` должен быть идентификатором",
                tree.name
            )));
        }
        if tree.points.is_empty() || tree.points.len() > MAX_BLEND_TREE_POINTS {
            return Err(error(format!(
                "blend tree `{}` должен содержать от 1 до {MAX_BLEND_TREE_POINTS} точек",
                tree.name
            )));
        }
        for point in &tree.points {
            if point.clip.is_empty() {
                return Err(error(format!(
                    "точка blend tree `{}` должна ссылаться на клип",
                    tree.name
                )));
            }
            if !point.threshold.is_finite() {
                return Err(error(format!(
                    "порог точки blend tree `{}` должен быть конечным",
                    tree.name
                )));
            }
        }
        if !tree
            .points
            .windows(2)
            .all(|pair| pair[0].threshold < pair[1].threshold)
        {
            return Err(error(format!(
                "пороги blend tree `{}` должны строго возрастать",
                tree.name
            )));
        }
    }
    Ok(())
}

/// Stem файла клипа, например `run` для `/anims/run.fbx`.
pub fn clip_stem(clip: &VirtualPath) -> &str {
    Path::new(clip.as_str())
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn default_speed() -> f32 {
    1.0
}

fn is_default_speed(value: &f32) -> bool {
    *value == 1.0
}

/// Список виртуальных путей в TOML: строки с ведущим `/`.
mod clip_paths {
    use engine_files::VirtualPath;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S>(value: &[VirtualPath], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(value.iter().map(|path| format!("/{}", path.as_str())))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<VirtualPath>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<String>::deserialize(deserializer)?;
        values
            .into_iter()
            .map(|value| {
                VirtualPath::parse(&value)
                    .map_err(|_| D::Error::custom(format!("некорректный virtual path {value}")))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::{AnimatorComponent, SceneDocument, World};

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    const ASSETS: &str = "[[assets]]\npath = \"/models/hero.fbx\"\n[[assets]]\npath = \"/anims/run.fbx\"\n[[assets]]\npath = \"/anims/walk.fbx\"\n[[assets]]\npath = \"/other/Run.fbx\"\n[[assets]]\npath = \"/anims/idle.png\"\n";

    fn parse(input: &str) -> Result<SceneDocument, crate::SceneError> {
        SceneDocument::parse("scenes/test.rvescene", input)
    }

    fn source(animator: &str) -> String {
        format!(
            "{ASSETS}{CAMERA}[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\n{animator}\n"
        )
    }

    fn error_for(animator: &str) -> String {
        parse(&source(animator)).unwrap_err().to_string()
    }

    #[test]
    fn defaults_apply_and_default_fields_are_not_serialized() {
        let document = parse(&source("")).unwrap();
        let animator = document.objects[1].animator.clone().unwrap();
        assert_eq!(animator, AnimatorComponent::new());
        assert!(animator.play_on_start && animator.looping);
        assert_eq!(animator.speed, 1.0);
        let text = document.to_toml().unwrap();
        assert!(text.contains("[objects.animator]"), "{text}");
        for field in ["play_on_start", "looping", "speed", "clips", "blend_trees"] {
            assert!(
                !text.contains(field),
                "{field} не должен сохраняться: {text}"
            );
        }
    }

    #[test]
    fn full_component_round_trips_through_toml() {
        let fields = "clips = [\"/anims/run.fbx\", \"/anims/walk.fbx\"]\ndefault_motion = \"locomotion\"\nplay_on_start = false\nlooping = false\nspeed = 0.5\n\n[[objects.animator.blend_trees]]\nname = \"locomotion\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"walk\"\nthreshold = 1.0\n\n[[objects.animator.blend_trees.points]]\nclip = \"run\"\nthreshold = 4.0\n";
        let document = parse(&source(fields)).unwrap();
        let animator = document.objects[1].animator.clone().unwrap();
        assert_eq!(animator.clips.len(), 2);
        assert_eq!(animator.default_motion.as_deref(), Some("locomotion"));
        assert!(!animator.play_on_start && !animator.looping);
        assert_eq!(animator.speed, 0.5);
        assert_eq!(animator.blend_trees[0].points[1].clip, "run");

        let text = document.to_toml().unwrap();
        assert!(text.contains("/anims/run.fbx"), "{text}");
        let again = parse(&text).unwrap();
        assert_eq!(again.objects[1].animator, document.objects[1].animator);
    }

    #[test]
    fn scenes_without_animator_do_not_serialize_the_field() {
        let text = format!("{ASSETS}{CAMERA}");
        let document = parse(&text).unwrap();
        assert!(!document.to_toml().unwrap().contains("animator"));
    }

    #[test]
    fn validation_reports_each_broken_rule() {
        let cases = [
            ("clips = [\"/anims/missing.fbx\"]", "[[assets]]", "missing"),
            ("clips = [\"/anims/idle.png\"]", ".fbx", "png"),
            (
                "clips = [\"/anims/run.fbx\", \"/other/Run.fbx\"]",
                "без учёта регистра",
                "dup",
            ),
            ("speed = -1.0", "speed", "neg"),
            ("speed = nan", "speed", "nan speed"),
            ("default_motion = \"\"", "default_motion", "empty default"),
            (
                "clips = [\"/anims/run.fbx\"]\n\n[[objects.animator.blend_trees]]\nname = \"run\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"run\"\nthreshold = 1.0\n",
                "совпадает с именем клипа",
                "tree name clip",
            ),
            (
                "\n[[objects.animator.blend_trees]]\nname = \"1walk\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"a\"\nthreshold = 1.0\n",
                "идентификатор",
                "tree identifier",
            ),
            (
                "\n[[objects.animator.blend_trees]]\nname = \"loco\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"a\"\nthreshold = 1.0\n\n[[objects.animator.blend_trees]]\nname = \"loco\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"a\"\nthreshold = 1.0\n",
                "повторяется",
                "tree duplicate",
            ),
            (
                "\n[[objects.animator.blend_trees]]\nname = \"loco\"\nparameter = \"speed\"\npoints = []\n",
                "от 1 до 16",
                "tree empty",
            ),
            (
                "\n[[objects.animator.blend_trees]]\nname = \"loco\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"a\"\nthreshold = 2.0\n\n[[objects.animator.blend_trees.points]]\nclip = \"b\"\nthreshold = 2.0\n",
                "строго возрастать",
                "tree thresholds",
            ),
            (
                "\n[[objects.animator.blend_trees]]\nname = \"loco\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"\"\nthreshold = 1.0\n",
                "ссылаться на клип",
                "tree empty clip",
            ),
            ("pitch = 2.0", "pitch", "unknown field"),
        ];
        for (fields, expected, label) in cases {
            let message = error_for(fields);
            assert!(message.contains(expected), "{label}: {message}");
        }
    }

    #[test]
    fn clip_stems_must_be_unique_ignoring_case() {
        let message = error_for("clips = [\"/anims/run.fbx\", \"/other/Run.fbx\"]");
        assert!(message.contains("`Run`"), "{message}");
    }

    #[test]
    fn static_object_cannot_have_an_animator() {
        let message =
            parse(&source("").replace("id = \"hero\"\n", "id = \"hero\"\nstatic = true\n"))
                .unwrap_err()
                .to_string();
        assert!(
            message.contains("статический объект не может иметь animator"),
            "{message}"
        );
    }

    #[test]
    fn animator_needs_mesh_renderer_and_no_lod_group() {
        let without_mesh =
            format!("{ASSETS}{CAMERA}[[objects]]\nid = \"hero\"\n[objects.animator]\n");
        let message = parse(&without_mesh).unwrap_err().to_string();
        assert!(message.contains("mesh_renderer"), "{message}");

        let with_lod = format!(
            "{}[objects.lod_group]\n[[objects.lod_group.levels]]\nmodel = \"/models/hero.fbx\"\ndistance = 10.0\n",
            source("")
        );
        let message = parse(&with_lod).unwrap_err().to_string();
        assert!(message.contains("lod_group"), "{message}");
    }

    #[test]
    fn world_exposes_the_animator_of_its_object() {
        let document = parse(&source("clips = [\"/anims/run.fbx\"]")).unwrap();
        let world = World::from_document(&document).unwrap();
        let hero = world.handle("hero").unwrap();
        let animator = world.animator(hero).unwrap();
        assert_eq!(animator.clips[0].as_str(), "anims/run.fbx");
        assert!(world.animator(world.handle("camera").unwrap()).is_none());
    }
}
