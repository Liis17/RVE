//! Сверка `sample_into`, `blend` и `line_weights` с прежним алгоритмом бит-в-бит и проверки
//! переиспользования рабочих буферов. Без GPU, окна, сети и ожиданий по времени.
//!
//! Оракул (`oracle_*`) — копия алгоритма до переработки: он выделяет временные векторы и не
//! знает о кешированных порогах и длительностях.

use super::{Animator, Motion, MotionSource, PlayOptions, Playback, SampleBuffers, TreePoint};
use crate::blend::{blend, line_weights, line_weights_inline};
use crate::clip::Clip;
use crate::pose::{Pose, Transform};
use crate::skeleton::Skeleton;
use engine_assets::{
    ModelAnimation, ModelInheritMode, ModelKey, ModelNode, ModelSkeleton, ModelTrack,
    ModelTransform,
};
use glam::{Quat, Vec3};

/// Детерминированный LCG, как в `engine-physics/src/broad.rs`.
struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, low: f32, high: f32) -> f32 {
        low + (high - low) * self.unit()
    }

    fn index(&mut self, len: usize) -> usize {
        (self.next_u64() >> 33) as usize % len
    }

    fn chance(&mut self, probability: f32) -> bool {
        self.unit() < probability
    }
}

// ---------- Оракул: прежний алгоритм без рабочих буферов ----------

fn oracle_sample(animator: &Animator, skeleton: &Skeleton, out: &mut Pose) {
    let rest = Pose::rest(skeleton);
    let total: f32 = animator
        .playbacks
        .iter()
        .map(|playback| playback.weight.max(0.0))
        .sum();
    let scale = if total > 1.0 { 1.0 / total } else { 1.0 };
    let poses: Vec<(Pose, f32)> = animator
        .playbacks
        .iter()
        .filter(|playback| playback.weight > 0.0)
        .map(|playback| {
            (
                oracle_motion(animator, playback, &rest),
                playback.weight * scale,
            )
        })
        .collect();
    let rest_weight = (1.0 - total * scale).max(0.0);
    let mut inputs: Vec<(&Pose, f32)> =
        poses.iter().map(|(pose, weight)| (pose, *weight)).collect();
    if rest_weight > 0.0 {
        inputs.push((&rest, rest_weight));
    }
    if inputs.is_empty() {
        out.transforms.clear();
        out.transforms.extend_from_slice(&rest.transforms);
        return;
    }
    oracle_blend(&inputs, out);
}

fn oracle_motion(animator: &Animator, playback: &Playback, rest: &Pose) -> Pose {
    let mut pose = rest.clone();
    match &animator.motions[playback.motion].source {
        MotionSource::Clip { clip } => {
            animator.clips[*clip].sample(playback.time, playback.looping, &mut pose);
        }
        MotionSource::Tree { parameter, points } => {
            let value = animator.parameter(parameter);
            let thresholds: Vec<f32> = points.iter().map(|point| point.threshold).collect();
            let duration = oracle_tree_duration(&animator.clips, points);
            let phase = if duration > 0.0 {
                (playback.time / duration).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mut children = Vec::new();
            for (index, weight) in oracle_line_weights(&thresholds, value) {
                let clip = &animator.clips[points[index].clip];
                let mut child = rest.clone();
                clip.sample(phase * clip.duration(), false, &mut child);
                children.push((child, weight));
            }
            let inputs: Vec<(&Pose, f32)> = children
                .iter()
                .map(|(child, weight)| (child, *weight))
                .collect();
            oracle_blend(&inputs, &mut pose);
        }
    }
    pose
}

fn oracle_tree_duration(clips: &[Clip], points: &[TreePoint]) -> f32 {
    let sum: f32 = points
        .iter()
        .map(|point| clips[point.clip].duration())
        .sum();
    sum / points.len() as f32
}

fn oracle_blend(inputs: &[(&Pose, f32)], out: &mut Pose) {
    let inputs: Vec<(&Pose, f32)> = inputs
        .iter()
        .filter(|(_, weight)| weight.is_finite() && *weight > 0.0)
        .map(|&(pose, weight)| (pose, weight))
        .collect();
    let total: f32 = inputs.iter().map(|(_, weight)| weight).sum();
    let Some(&(reference, _)) = inputs.first() else {
        return;
    };
    if total <= 0.0 {
        return;
    }
    let nodes = reference.transforms.len();
    out.transforms.resize(nodes, Transform::IDENTITY);
    for node in 0..nodes {
        let reference_rotation = reference.transforms[node].rotation;
        let mut translation = Vec3::ZERO;
        let mut scale = Vec3::ZERO;
        let mut rotation = glam::Vec4::ZERO;
        for &(pose, weight) in &inputs {
            let factor = weight / total;
            let transform = pose.transforms[node];
            translation += transform.translation * factor;
            scale += transform.scale * factor;
            let aligned = if reference_rotation.dot(transform.rotation) < 0.0 {
                -transform.rotation
            } else {
                transform.rotation
            };
            rotation += glam::Vec4::from(aligned) * factor;
        }
        let rotation = Quat::from_vec4(rotation);
        let rotation = if rotation.length_squared() > 1e-12 {
            rotation.normalize()
        } else {
            reference_rotation
        };
        out.transforms[node] = Transform {
            translation,
            rotation,
            scale,
        };
    }
}

fn oracle_line_weights(thresholds: &[f32], value: f32) -> Vec<(usize, f32)> {
    let Some(&first) = thresholds.first() else {
        return Vec::new();
    };
    let last_index = thresholds.len() - 1;
    if value <= first {
        return vec![(0, 1.0)];
    }
    if value >= thresholds[last_index] {
        return vec![(last_index, 1.0)];
    }
    let upper = thresholds.partition_point(|&threshold| threshold < value);
    let lower = upper - 1;
    let span = thresholds[upper] - thresholds[lower];
    let factor = (value - thresholds[lower]) / span;
    vec![(lower, 1.0 - factor), (upper, factor)]
}

// ---------- Сравнение и генераторы ----------

fn transform_bits(transform: &Transform) -> [u32; 10] {
    let parts = transform
        .translation
        .to_array()
        .into_iter()
        .chain(transform.rotation.to_array())
        .chain(transform.scale.to_array());
    let mut bits = [0; 10];
    for (slot, value) in bits.iter_mut().zip(parts) {
        *slot = value.to_bits();
    }
    bits
}

fn assert_same_pose(expected: &Pose, actual: &Pose, context: &str) {
    assert_eq!(
        expected.transforms.len(),
        actual.transforms.len(),
        "{context}: число узлов"
    );
    for (node, (want, got)) in expected
        .transforms
        .iter()
        .zip(&actual.transforms)
        .enumerate()
    {
        assert_eq!(
            transform_bits(want),
            transform_bits(got),
            "{context}: узел {node}"
        );
    }
}

/// Поза покоя с допуском: при нулевых весах покой проходит через `blend` с весом 1, а он
/// нормирует вращение и может сдвинуть последний бит. Точное совпадение с оракулом проверяет
/// `Harness::check`.
fn assert_near_rest(skeleton: &Skeleton, actual: &Pose, context: &str) {
    let rest = Pose::rest(skeleton);
    assert_eq!(
        rest.transforms.len(),
        actual.transforms.len(),
        "{context}: число узлов"
    );
    for (node, (want, got)) in rest.transforms.iter().zip(&actual.transforms).enumerate() {
        assert!(
            (want.translation - got.translation).length() < 1e-6,
            "{context}: перенос, узел {node}"
        );
        assert!(
            (want.scale - got.scale).length() < 1e-6,
            "{context}: масштаб, узел {node}"
        );
        assert!(
            want.rotation.dot(got.rotation).abs() > 1.0 - 1e-6,
            "{context}: вращение, узел {node}"
        );
    }
}

fn assert_same_pairs(expected: &[(usize, f32)], actual: &[(usize, f32)], context: &str) {
    assert_eq!(expected.len(), actual.len(), "{context}: число пар");
    for (want, got) in expected.iter().zip(actual) {
        assert_eq!(want.0, got.0, "{context}: индекс");
        assert_eq!(want.1.to_bits(), got.1.to_bits(), "{context}: вес");
    }
}

fn random_quat(rng: &mut Lcg) -> [f32; 4] {
    let quat = [
        rng.range(-1.0, 1.0),
        rng.range(-1.0, 1.0),
        rng.range(-1.0, 1.0),
        rng.range(-1.0, 1.0),
    ];
    let length = quat.iter().map(|c| c * c).sum::<f32>().sqrt();
    if length < 1e-3 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    quat.map(|c| c / length)
}

fn random_keys<T>(
    rng: &mut Lcg,
    duration: f32,
    mut value: impl FnMut(&mut Lcg) -> T,
) -> Vec<ModelKey<T>> {
    let count = 1 + rng.index(4);
    let mut times: Vec<f32> = (0..count).map(|_| rng.range(0.0, duration * 1.2)).collect();
    times.sort_by(f32::total_cmp);
    times
        .into_iter()
        .map(|time| ModelKey {
            time,
            value: value(rng),
        })
        .collect()
}

fn random_track(rng: &mut Lcg, node: &str, duration: f32) -> ModelTrack {
    ModelTrack {
        node: node.to_owned(),
        translation: if rng.chance(0.6) {
            random_keys(rng, duration, |rng| {
                [
                    rng.range(-2.0, 2.0),
                    rng.range(-2.0, 2.0),
                    rng.range(-2.0, 2.0),
                ]
            })
        } else {
            Vec::new()
        },
        rotation: if rng.chance(0.5) {
            random_keys(rng, duration, random_quat)
        } else {
            Vec::new()
        },
        scale: if rng.chance(0.3) {
            random_keys(rng, duration, |rng| [rng.range(0.5, 2.0); 3])
        } else {
            Vec::new()
        },
    }
}

fn random_skeleton(rng: &mut Lcg, count: usize) -> ModelSkeleton {
    let mut nodes = Vec::with_capacity(count);
    for index in 0..count {
        nodes.push(ModelNode {
            name: format!("bone{index}"),
            parent: (index > 0).then(|| rng.index(index)),
            inherit_mode: ModelInheritMode::Normal,
            inherit_scale_node: None,
            rest: ModelTransform {
                translation: [
                    rng.range(-1.0, 1.0),
                    rng.range(-1.0, 1.0),
                    rng.range(-1.0, 1.0),
                ],
                rotation: random_quat(rng),
                scale: [
                    rng.range(0.5, 2.0),
                    rng.range(0.5, 2.0),
                    rng.range(0.5, 2.0),
                ],
            },
        });
    }
    ModelSkeleton {
        nodes,
        joints: Vec::new(),
    }
}

/// Клипы с неполными треками: часть костей без дорожки, иногда дорожка без кости.
fn random_animations(rng: &mut Lcg, model: &ModelSkeleton, count: usize) -> Vec<ModelAnimation> {
    let mut animations = Vec::with_capacity(count);
    for index in 0..count {
        let duration = rng.range(0.2, 2.0);
        let mut tracks = Vec::new();
        for node in &model.nodes {
            if rng.chance(0.7) {
                tracks.push(random_track(rng, &node.name, duration));
            }
        }
        if rng.chance(0.2) {
            tracks.push(random_track(rng, "missing", duration));
        }
        animations.push(ModelAnimation {
            name: format!("clip{index}"),
            duration,
            tracks,
        });
    }
    animations
}

fn clip_motion(name: &str, clip: usize) -> Motion {
    Motion {
        name: name.to_owned(),
        source: MotionSource::Clip { clip },
    }
}

/// Скелет, клипы и аниматор со случайными клипами, двумя клиповыми движениями на клип
/// и одномерными деревьями с параметрами `p` и `q`.
fn random_rig(rng: &mut Lcg, bones: usize, looping: bool) -> (Skeleton, Animator) {
    let model = random_skeleton(rng, bones);
    let clip_count = 2 + rng.index(3);
    let animations = random_animations(rng, &model, clip_count);
    let skeleton = Skeleton::from_model(&model);
    let clips: Vec<Clip> = animations
        .iter()
        .map(|animation| Clip::bind(animation, &skeleton))
        .collect();
    let mut motions: Vec<Motion> = (0..clips.len())
        .map(|index| clip_motion(&format!("m{index}"), index))
        .collect();
    for tree in 0..1 + rng.index(2) {
        let mut threshold = rng.range(-1.0, 0.5);
        let mut points = Vec::new();
        for _ in 0..1 + rng.index(4) {
            points.push(TreePoint {
                threshold,
                clip: rng.index(clips.len()),
            });
            threshold += rng.range(0.2, 1.5);
        }
        motions.push(Motion {
            name: format!("t{tree}"),
            source: MotionSource::Tree {
                parameter: if tree == 0 { "p" } else { "q" }.to_owned(),
                points,
            },
        });
    }
    let animator = Animator::new(clips, motions, looping).expect("библиотека движений корректна");
    (skeleton, animator)
}

fn fade(rng: &mut Lcg) -> f32 {
    if rng.chance(0.4) {
        0.0
    } else {
        rng.range(0.05, 1.5)
    }
}

/// Значение параметра: с заметной частотой точное значение порога, иначе за краями и между.
fn parameter_value(rng: &mut Lcg, animator: &Animator) -> f32 {
    let mut thresholds = Vec::new();
    for motion in &animator.motions {
        if let MotionSource::Tree { points, .. } = &motion.source {
            thresholds.extend(points.iter().map(|point| point.threshold));
        }
    }
    if thresholds.is_empty() || !rng.chance(0.5) {
        rng.range(-2.0, 5.0)
    } else {
        thresholds[rng.index(thresholds.len())]
    }
}

/// Одна случайная операция над аниматором. Ошибки ожидаемы (например, отмотка неиграющего
/// движения), они не меняют состояние и тест их игнорирует.
fn random_op(rng: &mut Lcg, animator: &mut Animator) {
    let names: Vec<String> = animator.motion_names().map(str::to_owned).collect();
    let name = names[rng.index(names.len())].clone();
    match rng.index(9) {
        0 | 1 => {
            let then = rng
                .chance(0.3)
                .then(|| names[rng.index(names.len())].clone());
            let looping = rng.chance(0.5).then(|| rng.chance(0.5));
            let options = PlayOptions {
                fade: fade(rng),
                looping,
                then,
            };
            animator.play(&name, options).expect("имена корректны");
        }
        2 => {
            let weight = rng.range(-0.2, 1.3);
            let fade = fade(rng);
            animator
                .set_weight(&name, weight, fade)
                .expect("вес конечен");
        }
        3 => {
            let parameter = if rng.chance(0.5) { "p" } else { "q" };
            let value = parameter_value(rng, animator);
            animator
                .set_parameter(parameter, value)
                .expect("параметр конечен");
        }
        4 => animator
            .set_speed(rng.range(0.0, 2.5))
            .expect("скорость не отрицательна"),
        5 => {
            let _ = animator.seek(&name, rng.range(-0.5, 3.0));
        }
        6 => animator.stop(fade(rng)).expect("переход корректен"),
        _ => {
            animator.advance(rng.range(0.0, 0.6));
        }
    }
}

/// Тестовая обвязка: скелет, рабочие буферы и два результата для сверки с оракулом.
struct Harness {
    skeleton: Skeleton,
    buffers: SampleBuffers,
    expected: Pose,
    actual: Pose,
}

impl Harness {
    fn new(skeleton: Skeleton) -> Self {
        let filler = Pose {
            transforms: vec![Transform::IDENTITY; skeleton.nodes().len()],
        };
        Self {
            skeleton,
            buffers: SampleBuffers::default(),
            expected: filler.clone(),
            actual: filler,
        }
    }

    /// Результаты оракула и `sample_into` начинаются с одинакового содержимого `out`,
    /// поэтому совпадают и случаи, когда смешивание ничего не записывает.
    fn check(&mut self, animator: &Animator, context: &str) {
        oracle_sample(animator, &self.skeleton, &mut self.expected);
        animator.sample_into(&self.skeleton, &mut self.buffers, &mut self.actual);
        assert_same_pose(&self.expected, &self.actual, context);
    }
}

// ---------- Тесты ----------

#[test]
fn randomized_sequences_match_the_previous_algorithm_bit_for_bit() {
    for seed in 1..=40 {
        let mut rng = Lcg(seed);
        let bones = 1 + rng.index(6);
        let looping = rng.chance(0.5);
        let (skeleton, mut animator) = random_rig(&mut rng, bones, looping);
        let mut harness = Harness::new(skeleton);
        harness.check(&animator, &format!("seed {seed}, старт"));
        for step in 0..300 {
            random_op(&mut rng, &mut animator);
            harness.check(&animator, &format!("seed {seed}, шаг {step}"));
        }
    }
}

#[test]
fn edge_cases_match_the_previous_algorithm() {
    let model = random_skeleton(&mut Lcg(5), 3);
    let skeleton = Skeleton::from_model(&model);
    let animations = random_animations(&mut Lcg(6), &model, 3);
    let clips = animations
        .iter()
        .map(|animation| Clip::bind(animation, &skeleton))
        .collect();
    let tree = Motion {
        name: "t".to_owned(),
        source: MotionSource::Tree {
            parameter: "p".to_owned(),
            points: vec![
                TreePoint {
                    threshold: 0.0,
                    clip: 0,
                },
                TreePoint {
                    threshold: 1.0,
                    clip: 1,
                },
                TreePoint {
                    threshold: 2.0,
                    clip: 2,
                },
            ],
        },
    };
    let motions = vec![
        clip_motion("a", 0),
        clip_motion("b", 1),
        clip_motion("c", 2),
        tree,
    ];
    let mut animator = Animator::new(clips, motions, true).expect("библиотека корректна");
    let mut harness = Harness::new(skeleton);

    harness.check(&animator, "нет движений");
    assert_near_rest(&harness.skeleton, &harness.actual, "нет движений — покой");

    animator
        .play("t", PlayOptions::default())
        .expect("дерево есть");
    for value in [-1.0, 0.0, 0.5, 1.0, 1.5, 2.0, 3.0] {
        animator
            .set_parameter("p", value)
            .expect("значение конечно");
        harness.check(&animator, &format!("дерево при p={value}"));
    }

    animator
        .play(
            "a",
            PlayOptions {
                fade: 1.0,
                ..Default::default()
            },
        )
        .expect("клип есть");
    for _ in 0..4 {
        animator.advance(0.3);
        harness.check(&animator, "кроссфейд клипа и дерева");
    }

    animator.set_weight("b", 0.3, 0.0).expect("вес конечен");
    harness.check(&animator, "суммарный вес меньше 1: добор позой покоя");

    animator.set_weight("c", 1.0, 0.0).expect("вес конечен");
    animator.set_weight("b", 1.0, 0.0).expect("вес конечен");
    harness.check(&animator, "суммарный вес больше 1");

    for motion in ["a", "b", "c", "t"] {
        animator.set_weight(motion, 0.0, 0.0).expect("вес конечен");
    }
    harness.check(&animator, "все веса нулевые");
    assert_near_rest(
        &harness.skeleton,
        &harness.actual,
        "все веса нулевые — покой",
    );
}

#[test]
fn blend_and_line_weights_match_the_previous_algorithm() {
    let odd_weights = [0.0, -0.5, f32::NAN, f32::INFINITY, 1e-30, 0.25, 3.0];
    for seed in 0..200 {
        let mut rng = Lcg(seed + 1000);
        let nodes = 1 + rng.index(4);
        let poses: Vec<Pose> = (0..rng.index(6))
            .map(|_| random_pose(&mut rng, nodes))
            .collect();
        let inputs: Vec<(&Pose, f32)> = poses
            .iter()
            .map(|pose| {
                let weight = if rng.chance(0.3) {
                    odd_weights[rng.index(odd_weights.len())]
                } else {
                    rng.range(0.0, 2.0)
                };
                (pose, weight)
            })
            .collect();
        let initial = random_pose(&mut rng, nodes);
        let mut expected = initial.clone();
        let mut actual = initial;
        oracle_blend(&inputs, &mut expected);
        blend(&inputs, &mut actual);
        assert_same_pose(&expected, &actual, &format!("blend, seed {seed}"));
    }

    for seed in 0..200 {
        let mut rng = Lcg(seed + 5000);
        let count = 1 + rng.index(5);
        let mut thresholds = Vec::with_capacity(count);
        let mut threshold = rng.range(-2.0, 1.0);
        for _ in 0..count {
            thresholds.push(threshold);
            threshold += rng.range(0.1, 1.0);
        }
        for probe in 0..8 {
            let value = if probe % 2 == 0 {
                thresholds[rng.index(count)]
            } else {
                rng.range(-3.0, 6.0)
            };
            let context = format!("line_weights, seed {seed}, значение {value}");
            let expected = oracle_line_weights(&thresholds, value);
            assert_same_pairs(&expected, &line_weights(&thresholds, value), &context);
            assert_same_pairs(
                &expected,
                line_weights_inline(&thresholds, value).as_slice(),
                &context,
            );
        }
    }
}

fn random_pose(rng: &mut Lcg, nodes: usize) -> Pose {
    Pose {
        transforms: (0..nodes)
            .map(|_| Transform {
                translation: Vec3::new(
                    rng.range(-1.0, 1.0),
                    rng.range(-1.0, 1.0),
                    rng.range(-1.0, 1.0),
                ),
                rotation: Quat::from_array(random_quat(rng)),
                scale: Vec3::new(
                    rng.range(0.5, 2.0),
                    rng.range(0.5, 2.0),
                    rng.range(0.5, 2.0),
                ),
            })
            .collect(),
    }
}

/// Адреса и ёмкости всех векторов набора вместе с их длинами. Без выделений памяти
/// после прогрева значения не меняются.
fn fingerprint(buffers: &SampleBuffers) -> Vec<(usize, usize, usize)> {
    let mut sheet = vec![
        (
            buffers.rest.transforms.as_ptr() as usize,
            buffers.rest.transforms.capacity(),
            buffers.rest.transforms.len(),
        ),
        (
            buffers.motions.as_ptr() as usize,
            buffers.motions.capacity(),
            buffers.motions.len(),
        ),
        (
            buffers.children.as_ptr() as usize,
            buffers.children.capacity(),
            buffers.children.len(),
        ),
    ];
    for entry in &buffers.motions {
        sheet.push((
            entry.pose.transforms.as_ptr() as usize,
            entry.pose.transforms.capacity(),
            entry.pose.transforms.len(),
        ));
    }
    for child in &buffers.children {
        sheet.push((
            child.transforms.as_ptr() as usize,
            child.transforms.capacity(),
            child.transforms.len(),
        ));
    }
    sheet
}

#[test]
fn warm_buffers_do_not_reallocate_when_the_active_motion_count_changes() {
    let model = random_skeleton(&mut Lcg(11), 3);
    let skeleton = Skeleton::from_model(&model);
    let animations = random_animations(&mut Lcg(12), &model, 2);
    let clips = animations
        .iter()
        .map(|animation| Clip::bind(animation, &skeleton))
        .collect();
    let tree = Motion {
        name: "t".to_owned(),
        source: MotionSource::Tree {
            parameter: "p".to_owned(),
            points: vec![
                TreePoint {
                    threshold: 0.0,
                    clip: 0,
                },
                TreePoint {
                    threshold: 1.0,
                    clip: 1,
                },
            ],
        },
    };
    let mut animator = Animator::new(
        clips,
        vec![clip_motion("a", 0), clip_motion("b", 1), tree],
        true,
    )
    .expect("библиотека корректна");
    let mut buffers = SampleBuffers::default();
    let mut out = Pose::rest(&skeleton);

    // Прогрев на максимуме: дерево с двумя ненулевыми весами и кроссфейд двух движений.
    animator.set_parameter("p", 0.5).expect("значение конечно");
    animator
        .play("t", PlayOptions::default())
        .expect("дерево есть");
    animator.sample_into(&skeleton, &mut buffers, &mut out);
    animator
        .play(
            "a",
            PlayOptions {
                fade: 1.0,
                ..Default::default()
            },
        )
        .expect("клип есть");
    animator.advance(0.5);
    animator.sample_into(&skeleton, &mut buffers, &mut out);
    let warm = fingerprint(&buffers);

    // Кроссфейд → одно движение → снова два → одно.
    let mut check = |animator: &Animator, step: &str| {
        animator.sample_into(&skeleton, &mut buffers, &mut out);
        assert_eq!(
            fingerprint(&buffers),
            warm,
            "{step}: выделений быть не должно"
        );
    };
    animator.advance(1.0);
    check(&animator, "кроссфейд завершён");
    animator
        .play(
            "b",
            PlayOptions {
                fade: 1.0,
                ..Default::default()
            },
        )
        .expect("клип есть");
    animator.advance(0.25);
    check(&animator, "снова два движения");
    animator.set_parameter("p", -3.0).expect("значение конечно");
    animator.advance(2.0);
    check(&animator, "одно движение");
}

#[test]
fn one_buffer_set_serves_animators_with_different_skeletons_as_fresh_buffers_do() {
    let mut rng = Lcg(77);
    let mut rigs = [
        random_rig(&mut rng, 3, true),
        random_rig(&mut rng, 5, false),
        // Тот же размер, другая поза покоя.
        random_rig(&mut rng, 3, true),
    ];
    let mut shared = SampleBuffers::default();
    for round in 0..200 {
        let rig = rng.index(rigs.len());
        let (skeleton, animator) = &mut rigs[rig];
        random_op(&mut rng, animator);
        let nodes = skeleton.nodes().len();
        let filler = Pose {
            transforms: vec![Transform::IDENTITY; nodes],
        };
        let mut fresh_out = filler.clone();
        let mut shared_out = filler;
        animator.sample_into(skeleton, &mut SampleBuffers::default(), &mut fresh_out);
        animator.sample_into(skeleton, &mut shared, &mut shared_out);
        assert_same_pose(
            &fresh_out,
            &shared_out,
            &format!("раунд {round}, аниматор {rig}"),
        );
    }
}

#[test]
fn cached_duration_is_the_mean_of_tree_points_for_advance_and_seek() {
    let model = random_skeleton(&mut Lcg(3), 1);
    let skeleton = Skeleton::from_model(&model);
    let short = ModelAnimation {
        name: "short".to_owned(),
        duration: 1.0,
        tracks: Vec::new(),
    };
    let long = ModelAnimation {
        name: "long".to_owned(),
        duration: 3.0,
        tracks: Vec::new(),
    };
    let clips = vec![Clip::bind(&short, &skeleton), Clip::bind(&long, &skeleton)];
    let tree = Motion {
        name: "t".to_owned(),
        source: MotionSource::Tree {
            parameter: "p".to_owned(),
            points: vec![
                TreePoint {
                    threshold: 0.0,
                    clip: 0,
                },
                TreePoint {
                    threshold: 1.0,
                    clip: 1,
                },
            ],
        },
    };
    let mut animator = Animator::new(clips, vec![tree], false).expect("библиотека корректна");
    assert_eq!(animator.duration("t"), Some(2.0), "среднее по точкам");

    animator
        .play(
            "t",
            PlayOptions {
                fade: 0.0,
                looping: Some(false),
                then: None,
            },
        )
        .expect("дерево есть");
    assert!(
        animator.advance(1.9).is_empty(),
        "до среднего конца события нет"
    );
    assert_eq!(animator.advance(0.2).len(), 1, "конец на среднем");

    animator.seek("t", 10.0).expect("дерево проигрывается");
    assert_eq!(
        animator.time("t"),
        Some(2.0),
        "зажим по кешированной длительности"
    );
}
