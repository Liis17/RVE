//! Клипы: дорожки, сопоставленные с узлами скелета по имени, и их сэмплирование.

use crate::pose::{Pose, Transform};
use crate::skeleton::Skeleton;
use engine_assets::ModelAnimation;
use glam::{Quat, Vec3};
use std::collections::{HashMap, HashSet};

/// Ключ дорожки: время в секундах от начала клипа и значение.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Key<T> {
    pub time: f32,
    pub value: T,
}

/// Дорожка одного узла скелета. Каналы без ключей берут позу покоя.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub node: usize,
    pub translation: Vec<Key<Vec3>>,
    pub rotation: Vec<Key<Quat>>,
    pub scale: Vec<Key<Vec3>>,
}

/// Клип, привязанный к скелету.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    name: String,
    duration: f32,
    tracks: Vec<Track>,
    unmatched: Vec<String>,
}

impl Clip {
    /// Сопоставляет дорожки с узлами скелета по имени.
    ///
    /// Дорожка без узла и вторая дорожка на тот же узел попадают в [`Clip::unmatched`].
    pub fn bind(animation: &ModelAnimation, skeleton: &Skeleton) -> Self {
        let mut node_by_name: HashMap<&str, usize> = HashMap::new();
        for (index, node) in skeleton.nodes().iter().enumerate() {
            node_by_name.entry(node.name.as_str()).or_insert(index);
        }
        let mut bound = HashSet::new();
        let mut tracks = Vec::new();
        let mut unmatched = Vec::new();
        for track in &animation.tracks {
            let node = node_by_name.get(track.node.as_str()).copied();
            match node {
                Some(node) if bound.insert(node) => tracks.push(Track {
                    node,
                    translation: track
                        .translation
                        .iter()
                        .map(|key| Key {
                            time: key.time,
                            value: Vec3::from_array(key.value),
                        })
                        .collect(),
                    rotation: track
                        .rotation
                        .iter()
                        .map(|key| Key {
                            time: key.time,
                            value: Quat::from_array(key.value),
                        })
                        .collect(),
                    scale: track
                        .scale
                        .iter()
                        .map(|key| Key {
                            time: key.time,
                            value: Vec3::from_array(key.value),
                        })
                        .collect(),
                }),
                _ => unmatched.push(track.node.clone()),
            }
        }
        Self {
            name: animation.name.clone(),
            duration: animation.duration.max(0.0),
            tracks,
            unmatched,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Длительность в секундах.
    pub fn duration(&self) -> f32 {
        self.duration
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Имена дорожек, которые не сопоставились со скелетом.
    pub fn unmatched(&self) -> &[String] {
        &self.unmatched
    }

    /// Записывает в позу значения клипа в момент `time`.
    ///
    /// Каналы без ключей не меняются, поэтому перед вызовом поза должна быть позой покоя.
    /// При `looping` время переносится по модулю длительности, иначе зажимается в [0, длительность].
    pub fn sample(&self, time: f32, looping: bool, pose: &mut Pose) {
        let time = wrap_time(time, self.duration, looping);
        for track in &self.tracks {
            let Some(current) = pose.transforms.get_mut(track.node) else {
                continue;
            };
            let mut transform: Transform = *current;
            if !track.translation.is_empty() {
                transform.translation = sample_keys(&track.translation, time, Vec3::lerp);
            }
            if !track.rotation.is_empty() {
                transform.rotation = sample_keys(&track.rotation, time, shortest_slerp);
            }
            if !track.scale.is_empty() {
                transform.scale = sample_keys(&track.scale, time, Vec3::lerp);
            }
            *current = transform;
        }
    }
}

/// Приводит время к отрезку клипа: по кругу для цикличного и с зажимом иначе.
pub(crate) fn wrap_time(time: f32, duration: f32, looping: bool) -> f32 {
    if !time.is_finite() || !duration.is_finite() || duration <= 0.0 {
        return 0.0;
    }
    if looping {
        time.rem_euclid(duration)
    } else {
        time.clamp(0.0, duration)
    }
}

fn sample_keys<T: Copy>(keys: &[Key<T>], time: f32, interpolate: impl Fn(T, T, f32) -> T) -> T {
    let next = keys.partition_point(|key| key.time <= time);
    if next == 0 {
        return keys[0].value;
    }
    if next == keys.len() {
        return keys[next - 1].value;
    }
    let before = keys[next - 1];
    let after = keys[next];
    let span = after.time - before.time;
    let factor = if span > 0.0 {
        (time - before.time) / span
    } else {
        0.0
    };
    interpolate(before.value, after.value, factor)
}

/// Интерполяция поворота по кратчайшей дуге: знак второго кватерниона выравнивается с первым.
fn shortest_slerp(from: Quat, to: Quat, factor: f32) -> Quat {
    let to = if from.dot(to) < 0.0 { -to } else { to };
    from.slerp(to, factor)
}

#[cfg(test)]
mod tests {
    use super::Clip;
    use crate::pose::Pose;
    use crate::skeleton::Skeleton;
    use engine_assets::{
        ModelAnimation, ModelInheritMode, ModelKey, ModelNode, ModelSkeleton, ModelTrack,
        ModelTransform,
    };
    use glam::{Quat, Vec3};

    fn skeleton() -> Skeleton {
        let node = |name: &str, parent| ModelNode {
            name: name.to_owned(),
            parent,
            inherit_mode: ModelInheritMode::Normal,
            inherit_scale_node: None,
            rest: ModelTransform {
                translation: [1.0, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            },
        };
        Skeleton::from_model(&ModelSkeleton {
            nodes: vec![node("root", None), node("child", Some(0))],
            joints: Vec::new(),
        })
    }

    fn track(node: &str) -> ModelTrack {
        ModelTrack {
            node: node.to_owned(),
            translation: Vec::new(),
            rotation: Vec::new(),
            scale: Vec::new(),
        }
    }

    fn animation(tracks: Vec<ModelTrack>) -> ModelAnimation {
        ModelAnimation {
            name: "clip".to_owned(),
            duration: 1.0,
            tracks,
        }
    }

    #[test]
    fn translation_clamps_outside_the_clip_and_wraps_when_looping() {
        let mut moving = track("child");
        moving.translation = vec![
            ModelKey {
                time: 0.0,
                value: [0.0, 0.0, 0.0],
            },
            ModelKey {
                time: 1.0,
                value: [2.0, 0.0, 0.0],
            },
        ];
        let clip = Clip::bind(&animation(vec![moving]), &skeleton());
        let sample = |time, looping| {
            let skeleton = skeleton();
            let mut pose = Pose::rest(&skeleton);
            clip.sample(time, looping, &mut pose);
            pose.transforms[1].translation.x
        };
        assert!((sample(0.5, false) - 1.0).abs() < 1e-6);
        assert!(
            (sample(-3.0, false) - 0.0).abs() < 1e-6,
            "до начала — первый ключ"
        );
        assert!(
            (sample(1.5, false) - 2.0).abs() < 1e-6,
            "после конца — последний ключ"
        );
        assert!(
            (sample(1.5, true) - 1.0).abs() < 1e-6,
            "цикл переносит время по модулю"
        );
    }

    #[test]
    fn rotation_interpolates_along_the_shortest_arc() {
        let mut turning = track("child");
        turning.rotation = vec![
            ModelKey {
                time: 0.0,
                value: [0.0, 0.0, 0.0, 1.0],
            },
            // Тот же конечный поворот на 90° с противоположным знаком кватерниона.
            ModelKey {
                time: 1.0,
                value: (-Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)).to_array(),
            },
        ];
        let clip = Clip::bind(&animation(vec![turning]), &skeleton());
        let skeleton = skeleton();
        let mut pose = Pose::rest(&skeleton);
        clip.sample(0.5, false, &mut pose);
        let expected = Quat::from_rotation_z(std::f32::consts::FRAC_PI_4);
        assert!(pose.transforms[1].rotation.dot(expected).abs() > 0.999_99);
    }

    #[test]
    fn channels_without_keys_keep_the_rest_pose() {
        let mut only_translation = track("child");
        only_translation.translation = vec![ModelKey {
            time: 0.0,
            value: [5.0, 0.0, 0.0],
        }];
        let clip = Clip::bind(&animation(vec![only_translation]), &skeleton());
        let skeleton = skeleton();
        let mut pose = Pose::rest(&skeleton);
        clip.sample(0.0, false, &mut pose);
        assert_eq!(pose.transforms[1].translation, Vec3::new(5.0, 0.0, 0.0));
        assert_eq!(pose.transforms[1].scale, Vec3::ONE);
        assert_eq!(pose.transforms[1].rotation, Quat::IDENTITY);
    }

    #[test]
    fn unknown_and_duplicate_tracks_are_reported_as_unmatched() {
        let clip = Clip::bind(
            &animation(vec![track("child"), track("missing"), track("child")]),
            &skeleton(),
        );
        assert_eq!(clip.tracks().len(), 1);
        assert_eq!(clip.tracks()[0].node, 1);
        assert_eq!(clip.unmatched(), ["missing", "child"]);
    }
}
