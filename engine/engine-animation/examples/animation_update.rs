//! Замер `AnimationBridge::advance` на синтетических персонажах: без GPU, окна и файлов.
//!
//! Запуск из `build/`: `cargo run -p engine-animation --release --example animation_update`.
//! Для каждого скелета, числа персонажей и сценария печатаются медиана и p95 времени одного
//! вызова `advance` после прогрева. Сцена и модели строятся в памяти через публичный API.

use engine_animation::{AnimationBridge, Animator, Clip, Motion, MotionSource, Skeleton};
use engine_assets::{
    Model, ModelAnimation, ModelInheritMode, ModelJoint, ModelKey, ModelMarker, ModelNode,
    ModelSkeleton, ModelTrack, ModelTransform,
};
use engine_scene::ObjectHandle;
use engine_scene::{AnimationRequest, SceneDocument, World};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BONE_COUNTS: [usize; 2] = [32, 128];
const CHARACTER_COUNTS: [usize; 3] = [1, 100, 500];
const WARMUP_FRAMES: usize = 20;
const MEASURED_FRAMES: usize = 120;
const DENSE_MARKER_COUNT: usize = 2_000;
const FRAME_TIME: f32 = 1.0 / 60.0;
/// Дольше всего замера: оба веса кроссфейда остаются ненулевыми на протяжении всех кадров.
const CROSSFADE_TIME: f32 = 60.0;
const CLIP_NAMES: [&str; 8] = [
    "walk0", "walk1", "walk2", "walk3", "run0", "run1", "run2", "run3",
];
/// Два одномерных blend tree по четыре точки; клипы точек — имена из `CLIP_NAMES`.
const TREES: [(&str, [&str; 4]); 2] = [
    ("loco", ["walk0", "walk1", "walk2", "walk3"]),
    ("loco2", ["run0", "run1", "run2", "run3"]),
];
const IDENTITY: [[f32; 4]; 4] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

#[derive(Clone, Copy)]
enum Scenario {
    /// Один клип без смешивания.
    SingleClip,
    /// Дерево из четырёх точек; параметр 1.5 даёт ненулевые веса двух соседних точек.
    TreeTwoWeights,
    /// Кроссфейд двух деревьев: первое угасает, второе набирает вес.
    CrossfadeTrees,
}

impl Scenario {
    const ALL: [Self; 3] = [Self::SingleClip, Self::TreeTwoWeights, Self::CrossfadeTrees];

    fn name(self) -> &'static str {
        match self {
            Self::SingleClip => "single_clip",
            Self::TreeTwoWeights => "tree4_two_weights",
            Self::CrossfadeTrees => "crossfade_trees",
        }
    }

    fn requests(self, object: ObjectHandle) -> Vec<AnimationRequest> {
        let play = |motion: &str, fade: f32| AnimationRequest::Play {
            object,
            motion: motion.to_owned(),
            fade,
            looping: Some(true),
            then: None,
        };
        let speed = AnimationRequest::SetParameter {
            object,
            name: "speed".to_owned(),
            value: 1.5,
        };
        match self {
            Self::SingleClip => vec![play("walk0", 0.0)],
            Self::TreeTwoWeights => vec![speed, play("loco", 0.0)],
            Self::CrossfadeTrees => vec![speed, play("loco", 0.0), play("loco2", CROSSFADE_TIME)],
        }
    }
}

struct Timing {
    median_us: f64,
    p95_us: f64,
}

/// Модель с цепочкой костей в виде двоичного дерева и клипами, которые трогают каждую кость.
fn model(bones: usize) -> Model {
    let nodes = (0..bones)
        .map(|index| ModelNode {
            name: format!("bone{index}"),
            parent: (index > 0).then(|| (index - 1) / 2),
            inherit_mode: ModelInheritMode::Normal,
            inherit_scale_node: None,
            rest: ModelTransform {
                translation: [0.0, 0.1, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            },
        })
        .collect();
    let joints = (0..bones)
        .map(|node| ModelJoint {
            node,
            inverse_bind: IDENTITY,
        })
        .collect();
    let animations = CLIP_NAMES
        .iter()
        .enumerate()
        .map(|(clip, name)| clip_animation(name, clip, bones))
        .collect();
    Model {
        meshes: Vec::new(),
        textures: Vec::new(),
        skeleton: Some(ModelSkeleton { nodes, joints }),
        animations,
    }
}

fn clip_animation(name: &str, clip: usize, bones: usize) -> ModelAnimation {
    let tracks = (0..bones)
        .map(|bone| {
            let phase = (clip + 1) as f32 * 0.1 + bone as f32 * 0.01;
            ModelTrack {
                node: format!("bone{bone}"),
                translation: vec![
                    ModelKey {
                        time: 0.0,
                        value: [phase, 0.0, 0.0],
                    },
                    ModelKey {
                        time: 1.0,
                        value: [-phase, 0.5, 0.0],
                    },
                ],
                rotation: vec![
                    ModelKey {
                        time: 0.0,
                        value: [0.0, 0.0, 0.0, 1.0],
                    },
                    ModelKey {
                        time: 1.0,
                        value: [0.0, phase.sin(), 0.0, phase.cos()],
                    },
                ],
                scale: Vec::new(),
            }
        })
        .collect();
    ModelAnimation {
        name: name.to_owned(),
        duration: 1.0,
        tracks,
        markers: Vec::new(),
        marker_warnings: Vec::new(),
    }
}

/// Сцена с `characters` персонажами; у каждого аниматор с обоими деревьями.
fn scene_text(characters: usize) -> String {
    let mut text = String::from(
        "[[assets]]\npath = \"/models/hero.fbx\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
    );
    for index in 0..characters {
        text.push_str(&format!(
            "[[objects]]\nid = \"hero{index}\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\nplay_on_start = false\n"
        ));
        for (name, clips) in TREES {
            text.push_str(&format!(
                "[[objects.animator.blend_trees]]\nname = \"{name}\"\nparameter = \"speed\"\n"
            ));
            for (threshold, clip) in clips.iter().enumerate() {
                text.push_str(&format!(
                    "[[objects.animator.blend_trees.points]]\nclip = \"{clip}\"\nthreshold = {threshold}.0\n"
                ));
            }
        }
    }
    text
}

fn measure(model: &Arc<Model>, characters: usize, scenario: Scenario) -> Timing {
    let document = SceneDocument::parse("scene.rvescene", &scene_text(characters))
        .expect("синтетическая сцена читается");
    let world = World::from_document(&document).expect("синтетический мир собирается");
    let mut bridge =
        AnimationBridge::build(&world, |_| Ok(model.clone()), |_| Ok(Arc::new(Vec::new())));
    let requests: Vec<AnimationRequest> = (0..characters)
        .flat_map(|index| {
            let object = world
                .handle(&format!("hero{index}"))
                .expect("персонаж есть в мире");
            scenario.requests(object)
        })
        .collect();
    bridge.apply(requests);
    let warnings = bridge.take_warnings();
    assert!(
        warnings.is_empty(),
        "сценарий должен работать без предупреждений: {warnings:?}"
    );

    for _ in 0..WARMUP_FRAMES {
        bridge.advance(FRAME_TIME);
    }
    let mut frames = Vec::with_capacity(MEASURED_FRAMES);
    for _ in 0..MEASURED_FRAMES {
        let start = Instant::now();
        let events = bridge.advance(FRAME_TIME);
        frames.push(start.elapsed());
        std::hint::black_box(events);
    }
    summarize(frames)
}

fn summarize(mut frames: Vec<Duration>) -> Timing {
    frames.sort_unstable();
    let micros = |duration: Duration| duration.as_secs_f64() * 1e6;
    let count = frames.len();
    let median_us = if count % 2 == 1 {
        micros(frames[count / 2])
    } else {
        (micros(frames[count / 2 - 1]) + micros(frames[count / 2])) / 2.0
    };
    let p95_index = (count * 95).div_ceil(100) - 1;
    Timing {
        median_us,
        p95_us: micros(frames[p95_index]),
    }
}

/// Замер пересечения плотных маркеров напрямую в `Animator::advance`, без поз и палитр.
fn measure_dense_markers() -> Timing {
    let source = model(1);
    let skeleton = Skeleton::from_model(source.skeleton.as_ref().expect("скелет есть"));
    let mut animation = source.animations[0].clone();
    animation.name = "dense".to_owned();
    animation.markers = (0..DENSE_MARKER_COUNT)
        .map(|index| ModelMarker {
            name: format!("marker{index}"),
            time: index as f32 / DENSE_MARKER_COUNT as f32,
        })
        .collect();
    let clip = Clip::bind(&animation, &skeleton);
    let mut animator = Animator::new(
        vec![clip],
        vec![Motion {
            name: "dense".to_owned(),
            source: MotionSource::Clip { clip: 0 },
        }],
        true,
    )
    .expect("аниматор с одним движением собирается");
    animator
        .play("dense", Default::default())
        .expect("движение запускается");

    for _ in 0..WARMUP_FRAMES {
        std::hint::black_box(animator.advance(FRAME_TIME));
    }
    let mut frames = Vec::with_capacity(MEASURED_FRAMES);
    for _ in 0..MEASURED_FRAMES {
        let start = Instant::now();
        std::hint::black_box(animator.advance(FRAME_TIME));
        frames.push(start.elapsed());
    }
    summarize(frames)
}

fn main() {
    println!(
        "animation_update: {MEASURED_FRAMES} кадров после {WARMUP_FRAMES} прогревочных, dt = 1/60 с"
    );
    println!(
        "{:>6} {:>6} {:<18} {:>12} {:>12}",
        "bones", "chars", "scenario", "median us", "p95 us"
    );
    for bones in BONE_COUNTS {
        let model = Arc::new(model(bones));
        for characters in CHARACTER_COUNTS {
            for scenario in Scenario::ALL {
                let timing = measure(&model, characters, scenario);
                println!(
                    "{bones:>6} {characters:>6} {:<18} {:>12.1} {:>12.1}",
                    scenario.name(),
                    timing.median_us,
                    timing.p95_us
                );
            }
        }
    }
    let dense = measure_dense_markers();
    println!(
        "dense_markers {DENSE_MARKER_COUNT:>5} {:>12.1} {:>12.1}",
        dense.median_us, dense.p95_us
    );
}
