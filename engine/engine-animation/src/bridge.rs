//! Мост между `World` и аниматорами объектов: создание состояния, запросы скриптов,
//! события окончания и палитры skinning для каждого кадра.
//!
//! Мост не владеет GPU-ресурсами и не читает файлы сам: модели и клипы приходят через
//! замыкания, поэтому тот же код работает в runtime и в редакторе.

use crate::animator::{Animator, Motion, MotionEvent, MotionSource, PlayOptions, TreePoint};
use crate::clip::Clip;
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use engine_assets::{Model, ModelAnimation};
use engine_files::VirtualPath;
use engine_scene::{AnimationRequest, AnimatorComponent, ObjectHandle, World};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Сколько предупреждений моста хранится до того, как их заберёт владелец.
const WARNING_LIMIT: usize = 32;

/// Событие объекта, которое порождает [`AnimationBridge::advance`].
#[derive(Clone, Debug, PartialEq)]
pub enum AnimationEvent {
    /// Нецикличное движение дошло до конца; `motion` — имя движения.
    Finished {
        object: ObjectHandle,
        motion: String,
    },
}

/// Состояние аниматора одного объекта.
struct ObjectAnimation {
    id: String,
    animator: Animator,
    skeleton: Skeleton,
    scratch: Pose,
    palette: Vec<[[f32; 4]; 4]>,
}

/// Имя объекта для предупреждения; неизвестный объект выводится по индексу.
fn state_id(objects: &HashMap<ObjectHandle, ObjectAnimation>, object: ObjectHandle) -> String {
    objects.get(&object).map_or_else(
        || format!("объект #{}", object.index()),
        |state| state.id.clone(),
    )
}

/// Аниматоры объектов сцены и их палитры skinning.
#[derive(Default)]
pub struct AnimationBridge {
    /// Порядок объектов как в сцене: события выходят в детерминированном порядке.
    order: Vec<ObjectHandle>,
    objects: HashMap<ObjectHandle, ObjectAnimation>,
    warnings: Vec<String>,
    /// Уже переданные предупреждения: повтор одного и того же сообщения не попадает в журнал.
    reported: HashSet<String>,
}

impl AnimationBridge {
    /// Создаёт аниматоры для всех объектов с `animator`.
    ///
    /// `load_model` возвращает модель по пути `mesh_renderer.model`; `load_clips` — клипы
    /// из `animator.clips`. Ошибка объекта становится предупреждением, и объект рисуется
    /// статически. Модели и файлы клипов кэшируются на время вызова.
    pub fn build(
        world: &World,
        mut load_model: impl FnMut(&VirtualPath) -> Result<Arc<Model>, String>,
        mut load_clips: impl FnMut(&VirtualPath) -> Result<Arc<Vec<ModelAnimation>>, String>,
    ) -> Self {
        let mut bridge = Self::default();
        let mut models: HashMap<String, Result<Arc<Model>, String>> = HashMap::new();
        let mut clip_files: HashMap<String, Result<Arc<Vec<ModelAnimation>>, String>> =
            HashMap::new();
        for object in world.objects() {
            let (Some(animator), Some(renderer)) = (&object.animator, &object.mesh_renderer) else {
                continue;
            };
            let model = models
                .entry(renderer.model.as_str().to_owned())
                .or_insert_with(|| load_model(&renderer.model))
                .clone();
            let mut clips = Vec::with_capacity(animator.clips.len());
            for path in &animator.clips {
                let loaded = clip_files
                    .entry(path.as_str().to_owned())
                    .or_insert_with(|| load_clips(path))
                    .clone();
                match loaded {
                    Ok(animations) => clips.push(animations),
                    Err(error) => {
                        bridge.warn(format!("{}: клип {}: {error}", object.id, path.as_str()));
                    }
                }
            }
            let model = match model {
                Ok(model) => model,
                Err(error) => {
                    bridge.warn(format!("{}: модель не загружена: {error}", object.id));
                    continue;
                }
            };
            match bridge.build_object(&object.id, animator, &model, &clips) {
                Ok(state) => {
                    bridge.order.push(object.handle);
                    bridge.objects.insert(object.handle, state);
                }
                Err(message) => bridge.warn(format!("{}: {message}", object.id)),
            }
        }
        bridge
    }

    /// Применяет запросы скриптов за кадр. Запросы к объектам без аниматора игнорируются.
    pub fn apply(&mut self, requests: impl IntoIterator<Item = AnimationRequest>) {
        for request in requests {
            let object = match &request {
                AnimationRequest::Play { object, .. }
                | AnimationRequest::SetWeight { object, .. }
                | AnimationRequest::SetParameter { object, .. }
                | AnimationRequest::SetSpeed { object, .. }
                | AnimationRequest::Stop { object } => *object,
            };
            let Some(state) = self.objects.get_mut(&object) else {
                continue;
            };
            let result = match request {
                AnimationRequest::Play {
                    motion,
                    fade,
                    looping,
                    then,
                    ..
                } => state.animator.play(
                    &motion,
                    PlayOptions {
                        fade,
                        looping,
                        then,
                    },
                ),
                AnimationRequest::SetWeight {
                    motion,
                    weight,
                    fade,
                    ..
                } => state.animator.set_weight(&motion, weight, fade),
                AnimationRequest::SetParameter { name, value, .. } => {
                    state.animator.set_parameter(&name, value)
                }
                AnimationRequest::SetSpeed { speed, .. } => state.animator.set_speed(speed),
                AnimationRequest::Stop { .. } => state.animator.stop(0.0),
            };
            if let Err(error) = result {
                let message = format!("{}: {error}", state_id(&self.objects, object));
                self.warn(message);
            }
        }
    }

    /// Продвигает все аниматоры на `dt` секунд, пересчитывает палитры и возвращает события.
    pub fn advance(&mut self, dt: f32) -> Vec<AnimationEvent> {
        let mut events = Vec::new();
        for &object in &self.order {
            let Some(state) = self.objects.get_mut(&object) else {
                continue;
            };
            for event in state.animator.advance(dt) {
                let MotionEvent::Finished { motion } = event;
                events.push(AnimationEvent::Finished { object, motion });
            }
            state.animator.sample(&state.skeleton, &mut state.scratch);
            state
                .scratch
                .skin_matrices(&state.skeleton, &mut state.palette);
        }
        events
    }

    /// Палитра skinning объекта на текущий момент, если у него есть аниматор со скелетом.
    pub fn palette(&self, object: ObjectHandle) -> Option<&[[[f32; 4]; 4]]> {
        self.objects
            .get(&object)
            .map(|state| state.palette.as_slice())
    }

    /// Аниматор объекта для запросов состояния, например веса или времени в редакторе.
    pub fn animator(&self, object: ObjectHandle) -> Option<&Animator> {
        self.objects.get(&object).map(|state| &state.animator)
    }

    /// Изменяемый аниматор для управления редактора, например перемотки превью.
    pub fn animator_mut(&mut self, object: ObjectHandle) -> Option<&mut Animator> {
        self.objects
            .get_mut(&object)
            .map(|state| &mut state.animator)
    }

    pub fn has_animator(&self, object: ObjectHandle) -> bool {
        self.objects.contains_key(&object)
    }

    /// Забирает накопленные предупреждения; каждое сообщение содержит объект.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn warn(&mut self, message: String) {
        if self.warnings.len() < WARNING_LIMIT && self.reported.insert(message.clone()) {
            self.warnings.push(message);
        }
    }

    fn build_object(
        &mut self,
        id: &str,
        component: &AnimatorComponent,
        model: &Model,
        file_clips: &[Arc<Vec<ModelAnimation>>],
    ) -> Result<ObjectAnimation, String> {
        let model_skeleton = model
            .skeleton
            .as_ref()
            .ok_or_else(|| "у модели нет скелета; аниматор не применяется".to_owned())?;
        let skeleton = Skeleton::from_model(model_skeleton);

        let mut clips: Vec<Clip> = Vec::new();
        let mut clip_names: HashMap<String, usize> = HashMap::new();
        let stack_names: Vec<&str> = model.animations.iter().map(|a| a.name.as_str()).collect();
        let file_animations = file_clips.iter().flat_map(|list| list.iter());
        for animation in model.animations.iter().chain(file_animations) {
            if clip_names.contains_key(&animation.name) {
                self.warn(format!(
                    "{id}: клип `{}` повторяется; используется первый",
                    animation.name
                ));
                continue;
            }
            let clip = Clip::bind(animation, &skeleton);
            if !clip.unmatched().is_empty() {
                self.warn(format!(
                    "{id}: клип `{}`: дорожки без костей: {}",
                    animation.name,
                    clip.unmatched().join(", ")
                ));
            }
            clip_names.insert(animation.name.clone(), clips.len());
            clips.push(clip);
        }

        let tree_names: HashSet<&str> = component
            .blend_trees
            .iter()
            .map(|tree| tree.name.as_str())
            .collect();
        let mut motions = Vec::new();
        for (name, &index) in &clip_names {
            if tree_names.contains(name.as_str()) {
                self.warn(format!(
                    "{id}: имя клипа `{name}` совпадает с blend tree; побеждает tree"
                ));
                continue;
            }
            motions.push(Motion {
                name: name.clone(),
                source: MotionSource::Clip { clip: index },
            });
        }
        for tree in &component.blend_trees {
            let mut points = Vec::with_capacity(tree.points.len());
            let mut resolved = true;
            for point in &tree.points {
                match clip_names.get(&point.clip) {
                    Some(&clip) => points.push(TreePoint {
                        threshold: point.threshold,
                        clip,
                    }),
                    None => {
                        self.warn(format!(
                            "{id}: blend tree `{}`: клип `{}` не найден; дерево пропущено",
                            tree.name, point.clip
                        ));
                        resolved = false;
                        break;
                    }
                }
            }
            if resolved {
                motions.push(Motion {
                    name: tree.name.clone(),
                    source: MotionSource::Tree {
                        parameter: tree.parameter.clone(),
                        points,
                    },
                });
            }
        }
        // Порядок движений детерминирован: клипы по порядку загрузки, затем деревья.
        motions.sort_by_key(|motion| match &motion.source {
            MotionSource::Clip { clip } => (0, *clip),
            MotionSource::Tree { .. } => (1, 0),
        });

        let mut animator =
            Animator::new(clips, motions, component.looping).map_err(|error| error.to_string())?;
        animator
            .set_speed(component.speed)
            .map_err(|error| error.to_string())?;
        if component.play_on_start {
            let motion = component
                .default_motion
                .clone()
                .or_else(|| stack_names.first().map(|name| (*name).to_owned()));
            match motion {
                Some(name) => animator
                    .play(&name, PlayOptions::default())
                    .map_err(|error| error.to_string())?,
                None => self.warn(format!(
                    "{id}: нет движения для запуска; показана поза покоя"
                )),
            }
        }

        let mut state = ObjectAnimation {
            id: id.to_owned(),
            animator,
            scratch: Pose::rest(&skeleton),
            skeleton,
            palette: Vec::new(),
        };
        state.animator.sample(&state.skeleton, &mut state.scratch);
        state
            .scratch
            .skin_matrices(&state.skeleton, &mut state.palette);
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::{AnimationBridge, AnimationEvent};
    use engine_assets::{
        Model, ModelAnimation, ModelInheritMode, ModelJoint, ModelKey, ModelNode, ModelSkeleton,
        ModelTrack, ModelTransform,
    };
    use engine_scene::{AnimationRequest, SceneDocument, World};
    use std::sync::Arc;

    const SCENE: &str = "[[assets]]\npath = \"/models/hero.fbx\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n";

    fn world(animator: &str) -> World {
        let text = format!("{SCENE}[objects.animator]\n{animator}\n");
        World::from_document(&SceneDocument::parse("scene.rvescene", &text).unwrap()).unwrap()
    }

    /// Модель с одной костью, которая едет по X от 0 до 2 м за секунду клипа `Walk`.
    fn walk_model() -> Model {
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
                            value: [0.0, 0.0, 0.0],
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

    fn build(world: &World) -> AnimationBridge {
        let model = Arc::new(walk_model());
        AnimationBridge::build(world, |_| Ok(model.clone()), |_| Ok(Arc::new(Vec::new())))
    }

    /// X-компонента переноса единственной палитры.
    fn palette_x(bridge: &AnimationBridge, object: engine_scene::ObjectHandle) -> f32 {
        bridge.palette(object).expect("палитра")[0][3][0]
    }

    #[test]
    fn play_on_start_uses_the_first_model_stack_and_palette_follows_time() {
        let world = world("");
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        assert!(bridge.has_animator(hero));
        assert!(bridge.take_warnings().is_empty());
        assert!(
            palette_x(&bridge, hero).abs() < 1e-5,
            "в начале клипа — первый кадр"
        );
        assert!(bridge.advance(0.5).is_empty());
        assert!((palette_x(&bridge, hero) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn finished_non_looping_motion_is_reported_once_for_its_object() {
        let world = world("looping = false");
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        let events = bridge.advance(1.5);
        assert_eq!(
            events,
            [AnimationEvent::Finished {
                object: hero,
                motion: "Walk".to_owned(),
            }]
        );
        assert!(bridge.advance(1.0).is_empty());
    }

    #[test]
    fn requests_drive_the_animator_and_stop_returns_to_rest() {
        let world = world("");
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        bridge.apply([AnimationRequest::SetSpeed {
            object: hero,
            speed: 2.0,
        }]);
        bridge.advance(0.25);
        assert!(
            (palette_x(&bridge, hero) - 1.0).abs() < 1e-4,
            "скорость 2 — 0.5 с клипа"
        );
        bridge.apply([AnimationRequest::Stop { object: hero }]);
        bridge.advance(0.0);
        assert!(
            palette_x(&bridge, hero).abs() < 1e-5,
            "остановка возвращает покой"
        );
    }

    #[test]
    fn unknown_motion_is_a_warning_and_keeps_the_state() {
        let world = world("");
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        let before = palette_x(&bridge, hero);
        bridge.apply([AnimationRequest::Play {
            object: hero,
            motion: "jump".to_owned(),
            fade: 0.0,
            looping: None,
            then: None,
        }]);
        let warnings = bridge.take_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("jump"), "{warnings:?}");
        assert_eq!(palette_x(&bridge, hero), before);
    }

    #[test]
    fn requests_for_objects_without_animator_are_ignored() {
        let world = world("");
        let mut bridge = build(&world);
        let camera = world.handle("camera").unwrap();
        bridge.apply([AnimationRequest::Stop { object: camera }]);
        assert!(bridge.take_warnings().is_empty());
        assert!(bridge.palette(camera).is_none());
    }

    #[test]
    fn model_without_skeleton_keeps_the_object_static_with_a_warning() {
        let world = world("");
        let model = Arc::new(Model {
            skeleton: None,
            ..walk_model()
        });
        let mut bridge =
            AnimationBridge::build(&world, |_| Ok(model.clone()), |_| Ok(Arc::new(Vec::new())));
        let hero = world.handle("hero").unwrap();
        assert!(bridge.palette(hero).is_none());
        let warnings = bridge.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("нет скелета")),
            "{warnings:?}"
        );
    }

    #[test]
    fn loader_errors_affect_only_their_object() {
        let world = world("");
        let mut bridge = AnimationBridge::build(
            &world,
            |_| Err("файл не найден".to_owned()),
            |_| Ok(Arc::new(Vec::new())),
        );
        let hero = world.handle("hero").unwrap();
        assert!(!bridge.has_animator(hero));
        let warnings = bridge.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("файл не найден"))
        );
    }

    #[test]
    fn repeated_request_errors_are_reported_once_until_taken() {
        let world = world("");
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        bridge.take_warnings();
        for _ in 0..3 {
            bridge.apply([AnimationRequest::Play {
                object: hero,
                motion: "Missing".to_owned(),
                fade: 0.0,
                looping: None,
                then: None,
            }]);
        }
        let warnings = bridge.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("Missing"), "{warnings:?}");
        bridge.apply([AnimationRequest::Play {
            object: hero,
            motion: "Missing".to_owned(),
            fade: 0.0,
            looping: None,
            then: None,
        }]);
        assert!(bridge.take_warnings().is_empty(), "повтор уже показан");
    }

    #[test]
    fn blend_tree_with_unknown_clip_is_skipped_with_a_warning() {
        let world = world(
            "\n[[objects.animator.blend_trees]]\nname = \"loco\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"missing\"\nthreshold = 1.0\n",
        );
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        let warnings = bridge.take_warnings();
        assert!(
            warnings.iter().any(|warning| warning.contains("missing")),
            "{warnings:?}"
        );
        let names: Vec<&str> = bridge.animator(hero).unwrap().motion_names().collect();
        assert_eq!(names, ["Walk"]);
    }

    #[test]
    fn blend_tree_takes_precedence_over_a_clip_with_the_same_name() {
        let world = world(
            "\n[[objects.animator.blend_trees]]\nname = \"Walk\"\nparameter = \"speed\"\n\n[[objects.animator.blend_trees.points]]\nclip = \"Walk\"\nthreshold = 1.0\n",
        );
        let mut bridge = build(&world);
        let hero = world.handle("hero").unwrap();
        let warnings = bridge.take_warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("побеждает tree")),
            "{warnings:?}"
        );
        let names: Vec<&str> = bridge.animator(hero).unwrap().motion_names().collect();
        assert_eq!(names, ["Walk"], "имя остаётся одним движением");
    }
}
