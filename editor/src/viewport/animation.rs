//! Скелетная анимация вьюпорта редактора: мост по миру сцены и превью выбранного объекта.
//!
//! Мост пересобирается, когда меняется набор аниматоров сцены (клипы, модели, blend tree),
//! и продвигается каждый кадр. Превью управляет одним выбранным объектом: проигрыванием,
//! перемоткой и параметрами blend tree. Превью не меняет документ и не запускает скрипты.
//! Скиннинг GPU и палитры кадра собирает `SceneRenderer`.

use engine_animation::AnimationBridge;
use engine_assets::Assets;
use engine_scene::{AnimationRequest, ObjectHandle, World};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

/// Дольше этого кадр не продвигает время превью: пауза окна не должна перематывать клип.
const MAX_FRAME_SECONDS: f32 = 0.25;

/// Настройки превью, которые меняет инспектор.
#[derive(Clone, Debug, PartialEq)]
pub struct AnimationPreview {
    /// Объект, для которого действуют настройки; смена выбора сбрасывает их.
    pub object: Option<String>,
    /// Проигрывание превью; пауза останавливает время выбранного объекта.
    pub playing: bool,
    /// Движение, которое запускает превью; `None` оставляет позу покоя.
    pub motion: Option<String>,
    /// Значения параметров blend tree, которые превью передаёт аниматору.
    pub parameters: BTreeMap<String, f32>,
    /// Новое время движения: инспектор задаёт его при перетаскивании ползунка.
    pub seek: Option<f32>,
}

impl Default for AnimationPreview {
    fn default() -> Self {
        Self {
            object: None,
            playing: true,
            motion: None,
            parameters: BTreeMap::new(),
            seek: None,
        }
    }
}

impl AnimationPreview {
    /// Переносит превью на выбранный объект; при смене объекта все настройки сбрасываются.
    pub fn retarget(&mut self, selected: Option<&str>) {
        if self.object.as_deref() != selected {
            *self = Self {
                object: selected.map(str::to_owned),
                ..Self::default()
            };
        }
    }

    /// Нужны ли кадры для превью: выбрано движение, и оно проигрывается.
    pub fn is_animating(&self) -> bool {
        self.playing && self.motion.is_some()
    }
}

/// Сведения о превью для инспектора. Кадр пересчитывает их после продвижения моста.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviewStatus {
    /// Объект, для которого собраны сведения.
    pub object: Option<String>,
    /// Движения аниматора: клипы и blend tree в порядке моста.
    pub motions: Vec<String>,
    /// Время и длительность выбранного движения; нули, пока оно не проигрывается.
    pub time: f32,
    pub duration: f32,
}

/// Мост редактора и его отпечаток: при другом отпечатке мост пересобирается.
#[derive(Default)]
pub struct EditorAnimation {
    fingerprint: String,
    bridge: Option<AnimationBridge>,
    /// Последнее движение, переданное превью, чтобы не перезапускать его каждый кадр.
    applied_motion: Option<(ObjectHandle, String)>,
    applied_playing: Option<(ObjectHandle, bool)>,
    last_frame: Option<Instant>,
}

impl EditorAnimation {
    /// Мост по текущему миру; `None`, пока в сцене нет аниматоров.
    pub fn bridge(&self) -> Option<&AnimationBridge> {
        self.bridge.as_ref()
    }

    /// Шаг времени для превью: время с прошлого кадра, ограниченное `MAX_FRAME_SECONDS`.
    pub fn frame_seconds(&mut self) -> f32 {
        let now = Instant::now();
        let dt = self.last_frame.map_or(0.0, |last| {
            now.duration_since(last)
                .as_secs_f32()
                .min(MAX_FRAME_SECONDS)
        });
        self.last_frame = Some(now);
        dt
    }

    /// Пересобирает мост при изменении аниматоров, применяет превью и продвигает время.
    ///
    /// `build` создаёт мост заново только при другом отпечатке мира. Предупреждения моста
    /// добавляются в `warnings`.
    pub fn advance(
        &mut self,
        world: &World,
        dt: f32,
        preview: Option<(ObjectHandle, &mut AnimationPreview)>,
        warnings: &mut Vec<String>,
        build: impl FnOnce(&World) -> AnimationBridge,
    ) {
        let fingerprint = animator_fingerprint(world);
        if fingerprint != self.fingerprint {
            self.fingerprint = fingerprint;
            self.applied_motion = None;
            self.applied_playing = None;
            self.bridge = if self.fingerprint.is_empty() {
                None
            } else {
                Some(build(world))
            };
        }
        let Some(bridge) = self.bridge.as_mut() else {
            return;
        };
        if let Some((object, preview)) = preview {
            let requests = preview_requests(
                object,
                preview,
                &mut self.applied_motion,
                &mut self.applied_playing,
            );
            bridge.apply(requests);
            let parameters =
                preview
                    .parameters
                    .iter()
                    .map(|(name, value)| AnimationRequest::SetParameter {
                        object,
                        name: name.clone(),
                        value: *value,
                    });
            bridge.apply(parameters);
            if let (Some(time), Some(motion)) = (preview.seek.take(), preview.motion.as_deref())
                && let Some(animator) = bridge.animator_mut(object)
                && let Err(error) = animator.seek(motion, time)
            {
                warnings.push(format!("превью: {error}"));
            }
        }
        // События окончания в редакторе не нужны: скрипты здесь не работают.
        bridge.advance(dt);
        warnings.extend(bridge.take_warnings());
    }

    /// Сведения превью объекта для инспектора: движения и положение выбранного движения.
    pub fn status(&self, object: ObjectHandle, motion: Option<&str>) -> PreviewStatus {
        let Some(animator) = self
            .bridge
            .as_ref()
            .and_then(|bridge| bridge.animator(object))
        else {
            return PreviewStatus::default();
        };
        PreviewStatus {
            object: None,
            motions: animator.motion_names().map(str::to_owned).collect(),
            time: motion
                .and_then(|name| animator.time(name))
                .unwrap_or_default(),
            duration: motion
                .and_then(|name| animator.duration(name))
                .unwrap_or_default(),
        }
    }
}

/// Запросы превью: запуск выбранного движения при смене и пауза по флагу `playing`.
fn preview_requests(
    object: ObjectHandle,
    preview: &AnimationPreview,
    applied_motion: &mut Option<(ObjectHandle, String)>,
    applied_playing: &mut Option<(ObjectHandle, bool)>,
) -> Vec<AnimationRequest> {
    let mut requests = Vec::new();
    if let Some(motion) = &preview.motion {
        let key = (object, motion.clone());
        if applied_motion.as_ref() != Some(&key) {
            requests.push(AnimationRequest::Play {
                object,
                motion: motion.clone(),
                fade: 0.0,
                looping: None,
                then: None,
            });
            *applied_motion = Some(key);
        }
    }
    let playing = (object, preview.playing);
    if applied_playing.as_ref() != Some(&playing) {
        requests.push(AnimationRequest::SetSpeed {
            object,
            speed: if preview.playing { 1.0 } else { 0.0 },
        });
        *applied_playing = Some(playing);
    }
    requests
}

/// Отпечаток аниматоров сцены: объект, модель и компонент. Меняется при любой правке,
/// которая влияет на мост, и не меняется при движении объектов.
pub fn animator_fingerprint(world: &World) -> String {
    let mut parts = Vec::new();
    for object in world.objects() {
        let (Some(animator), Some(renderer)) = (&object.animator, &object.mesh_renderer) else {
            continue;
        };
        parts.push(format!(
            "{}|{}|{animator:?}",
            object.id,
            renderer.model.as_str()
        ));
    }
    parts.join("\n")
}

/// Мост для ассетов проекта: модели и клипы читаются через `Assets`. Используется Play,
/// где `play_on_start` запускает клипы сразу.
pub fn project_bridge(world: &World, assets: &Assets) -> AnimationBridge {
    AnimationBridge::build(
        world,
        |path| {
            assets
                .load_model(&format!("/{}", path.as_str()))
                .map(Arc::new)
                .map_err(|error| error.to_string())
        },
        |path| {
            assets
                .load_animations(&format!("/{}", path.as_str()))
                .map(Arc::new)
                .map_err(|error| error.to_string())
        },
    )
}

/// Мост превью для ассетов проекта: все аниматоры начинают с позы покоя, а клип запускает
/// только выбор в инспекторе. Документ и `play_on_start` при этом не меняются.
pub fn project_preview_bridge(world: &World, assets: &Assets) -> AnimationBridge {
    let mut bridge = project_bridge(world, assets);
    rest_all(&mut bridge, world);
    bridge
}

/// Останавливает все аниматоры мира без перехода: превью показывает позу покоя.
fn rest_all(bridge: &mut AnimationBridge, world: &World) {
    let stops = world
        .objects()
        .iter()
        .filter(|object| object.animator.is_some())
        .map(|object| AnimationRequest::Stop {
            object: object.handle,
        });
    bridge.apply(stops);
}

/// Модель для тестов моста: одна кость и клип `Walk` длиной в секунду, кость едет по X на 2 м.
#[cfg(test)]
pub(crate) mod test_support {
    use engine_assets::{
        Model, ModelAnimation, ModelInheritMode, ModelJoint, ModelKey, ModelNode, ModelSkeleton,
        ModelTrack, ModelTransform,
    };

    pub fn walk_model() -> Model {
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        Model {
            meshes: Vec::new(),
            textures: Vec::new(),
            skeleton: Some(ModelSkeleton {
                nodes: vec![ModelNode {
                    name: "bone".to_owned(),
                    parent: None,
                    inherit_mode: ModelInheritMode::Normal,
                    inherit_scale_node: None,
                    rest: ModelTransform {
                        translation: [0.0; 3],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        scale: [1.0; 3],
                    },
                }],
                joints: vec![ModelJoint {
                    node: 0,
                    inverse_bind: identity,
                }],
            }),
            animations: vec![ModelAnimation {
                name: "Walk".to_owned(),
                duration: 1.0,
                tracks: vec![ModelTrack {
                    node: "bone".to_owned(),
                    translation: vec![
                        ModelKey {
                            time: 0.0,
                            value: [0.0; 3],
                        },
                        ModelKey {
                            time: 1.0,
                            value: [2.0, 0.0, 0.0],
                        },
                    ],
                    rotation: Vec::new(),
                    scale: Vec::new(),
                }],
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::walk_model;
    use super::*;
    use engine_scene::SceneDocument;

    const SCENE: &str = "[[assets]]\npath = \"/models/hero.fbx\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n";

    fn world_with(animator: Option<&str>) -> World {
        let text = match animator {
            Some(fields) => format!("{SCENE}[objects.animator]\n{fields}\n"),
            None => SCENE.to_owned(),
        };
        World::from_document(&SceneDocument::parse("scene.rvescene", &text).unwrap()).unwrap()
    }

    /// Мост превью с одной моделью и без клипов файла, как `project_preview_bridge`.
    fn test_bridge(world: &World) -> AnimationBridge {
        let mut bridge = AnimationBridge::build(
            world,
            |_| Ok(Arc::new(walk_model())),
            |_| Ok(Arc::new(Vec::new())),
        );
        rest_all(&mut bridge, world);
        bridge
    }

    /// X-компонента переноса единственной палитры объекта.
    fn palette_x(editor: &EditorAnimation, object: ObjectHandle) -> f32 {
        editor.bridge().unwrap().palette(object).unwrap()[0][3][0]
    }

    #[test]
    fn fingerprint_changes_only_with_animator_relevant_edits() {
        let plain = world_with(None);
        assert_eq!(animator_fingerprint(&plain), "");
        let animated = world_with(Some("speed = 1.0"));
        let fingerprint = animator_fingerprint(&animated);
        assert!(
            fingerprint.starts_with("hero|models/hero.fbx|"),
            "{fingerprint}"
        );
        let faster = world_with(Some("speed = 2.0"));
        assert_ne!(animator_fingerprint(&faster), fingerprint);
        assert_eq!(
            animator_fingerprint(&world_with(Some("speed = 1.0"))),
            fingerprint
        );
    }

    #[test]
    fn preview_restarts_only_when_the_motion_changes_and_pauses_by_speed() {
        let object = world_with(Some("speed = 1.0")).handle("hero").unwrap();
        let mut applied_motion = None;
        let mut applied_playing = None;
        let mut preview = AnimationPreview {
            motion: Some("walk".to_owned()),
            ..AnimationPreview::default()
        };
        let first = preview_requests(object, &preview, &mut applied_motion, &mut applied_playing);
        assert!(matches!(first[0], AnimationRequest::Play { ref motion, .. } if motion == "walk"));
        assert!(matches!(first[1], AnimationRequest::SetSpeed { speed, .. } if speed == 1.0));

        assert!(
            preview_requests(object, &preview, &mut applied_motion, &mut applied_playing)
                .is_empty(),
            "без изменений превью ничего не отправляет"
        );

        preview.playing = false;
        let paused = preview_requests(object, &preview, &mut applied_motion, &mut applied_playing);
        assert!(matches!(paused[0], AnimationRequest::SetSpeed { speed, .. } if speed == 0.0));
    }

    #[test]
    fn retarget_keeps_settings_for_the_same_object_and_resets_them_on_selection_change() {
        let mut preview = AnimationPreview {
            object: Some("hero".to_owned()),
            motion: Some("Walk".to_owned()),
            playing: false,
            ..AnimationPreview::default()
        };
        preview.retarget(Some("hero"));
        assert_eq!(preview.motion.as_deref(), Some("Walk"), "тот же объект");
        assert!(!preview.playing);
        preview.retarget(Some("camera"));
        assert_eq!(preview.object.as_deref(), Some("camera"));
        assert_eq!(preview.motion, None);
        assert!(preview.playing, "настройки прежнего объекта не переносятся");
        assert!(!preview.is_animating());
    }

    #[test]
    fn preview_stays_in_bind_pose_until_a_motion_is_chosen() {
        let world = world_with(Some("speed = 1.0"));
        let hero = world.handle("hero").unwrap();
        let mut editor = EditorAnimation::default();
        let mut warnings = Vec::new();
        let mut preview = AnimationPreview {
            object: Some("hero".to_owned()),
            ..AnimationPreview::default()
        };
        editor.advance(
            &world,
            0.0,
            Some((hero, &mut preview)),
            &mut warnings,
            test_bridge,
        );
        editor.advance(
            &world,
            0.5,
            Some((hero, &mut preview)),
            &mut warnings,
            test_bridge,
        );
        assert!(
            palette_x(&editor, hero).abs() < 1e-5,
            "без движения — поза покоя"
        );

        preview.motion = Some("Walk".to_owned());
        editor.advance(
            &world,
            0.5,
            Some((hero, &mut preview)),
            &mut warnings,
            test_bridge,
        );
        assert!(
            (palette_x(&editor, hero) - 1.0).abs() < 1e-5,
            "Walk идёт с начала"
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn seek_moves_the_previewed_motion_once_and_reports_a_bad_motion() {
        let world = world_with(Some("speed = 1.0"));
        let hero = world.handle("hero").unwrap();
        let mut editor = EditorAnimation::default();
        let mut warnings = Vec::new();
        let mut preview = AnimationPreview {
            object: Some("hero".to_owned()),
            motion: Some("Walk".to_owned()),
            seek: Some(0.75),
            ..AnimationPreview::default()
        };
        editor.advance(
            &world,
            0.0,
            Some((hero, &mut preview)),
            &mut warnings,
            test_bridge,
        );
        assert!((palette_x(&editor, hero) - 1.5).abs() < 1e-5);
        assert_eq!(preview.seek, None, "перемотка применяется один раз");

        let status = editor.status(hero, Some("Walk"));
        assert_eq!(status.motions, ["Walk"]);
        assert!((status.time - 0.75).abs() < 1e-5);
        assert!((status.duration - 1.0).abs() < 1e-5);

        preview.motion = Some("Missing".to_owned());
        preview.seek = Some(0.1);
        editor.advance(
            &world,
            0.0,
            Some((hero, &mut preview)),
            &mut warnings,
            test_bridge,
        );
        assert!(
            warnings.iter().any(|message| message.contains("Missing")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_bridge_is_rebuilt_only_when_animators_change() {
        let world = world_with(Some("speed = 1.0"));
        let mut editor = EditorAnimation::default();
        let mut warnings = Vec::new();
        let mut builds = 0;
        for _ in 0..2 {
            editor.advance(&world, 0.0, None, &mut warnings, |world| {
                builds += 1;
                test_bridge(world)
            });
        }
        assert_eq!(builds, 1, "тот же мир не пересобирает мост");

        let faster = world_with(Some("speed = 2.0"));
        editor.advance(&faster, 0.0, None, &mut warnings, |world| {
            builds += 1;
            test_bridge(world)
        });
        assert_eq!(builds, 2);
    }
}
