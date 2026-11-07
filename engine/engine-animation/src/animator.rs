//! Состояние проигрывания объекта: crossfade, ручные веса, одномерный blend tree и события окончания.
//!
//! Вес каждого проигрывания плавно идёт к цели. Оставшаяся часть суммы веса до 1 берётся
//! из позы покоя, поэтому затухание из покоя и ручной вес меньше 1 работают предсказуемо.

use crate::blend::{blend, line_weights};
use crate::clip::{Clip, wrap_time};
use crate::pose::Pose;
use crate::skeleton::Skeleton;
use std::collections::HashMap;
use std::fmt;

/// Максимум точек одномерного blend tree.
pub const MAX_TREE_POINTS: usize = 16;

/// Точка blend tree: клип в позиции `threshold` по параметру.
#[derive(Clone, Debug, PartialEq)]
pub struct TreePoint {
    pub threshold: f32,
    /// Индекс в списке клипов [`Animator::new`].
    pub clip: usize,
}

/// Источник движения: один клип или одномерный blend tree.
#[derive(Clone, Debug, PartialEq)]
pub enum MotionSource {
    Clip {
        clip: usize,
    },
    Tree {
        parameter: String,
        points: Vec<TreePoint>,
    },
}

/// Именованное движение объекта.
#[derive(Clone, Debug, PartialEq)]
pub struct Motion {
    pub name: String,
    pub source: MotionSource,
}

/// Параметры запуска. `looping = None` берёт значение по умолчанию аниматора.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayOptions {
    /// Длительность перехода в секундах; 0 — мгновенно.
    pub fade: f32,
    pub looping: Option<bool>,
    /// Движение, которое запускается, когда нецикличное движение доходит до конца.
    pub then: Option<String>,
}

/// Событие, которое порождает [`Animator::advance`].
#[derive(Clone, Debug, PartialEq)]
pub enum MotionEvent {
    /// Нецикличное движение дошло до конца; событие приходит один раз за запуск.
    Finished { motion: String },
}

/// Ошибка настройки или управления аниматором.
#[derive(Clone, Debug, PartialEq)]
pub enum AnimatorError {
    UnknownMotion(String),
    InvalidMotion {
        motion: String,
        reason: &'static str,
    },
    InvalidValue(&'static str),
}

impl fmt::Display for AnimatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMotion(name) => write!(f, "неизвестное движение `{name}`"),
            Self::InvalidMotion { motion, reason } => {
                write!(f, "движение `{motion}`: {reason}")
            }
            Self::InvalidValue(what) => write!(f, "{what} должно быть конечным числом"),
        }
    }
}

impl std::error::Error for AnimatorError {}

#[derive(Clone, Debug)]
struct Playback {
    motion: usize,
    weight: f32,
    target: f32,
    fade: f32,
    time: f32,
    looping: bool,
    then: Option<usize>,
    finished: bool,
}

/// Проигрывание нескольких движений объекта с плавными переходами.
#[derive(Debug)]
pub struct Animator {
    clips: Vec<Clip>,
    motions: Vec<Motion>,
    playbacks: Vec<Playback>,
    parameters: HashMap<String, f32>,
    speed: f32,
    default_looping: bool,
}

impl Animator {
    /// Создаёт аниматор и проверяет движения: имена уникальны, клипы и точки корректны.
    pub fn new(
        clips: Vec<Clip>,
        motions: Vec<Motion>,
        default_looping: bool,
    ) -> Result<Self, AnimatorError> {
        for (index, motion) in motions.iter().enumerate() {
            if motion.name.is_empty() {
                return Err(invalid(&motion.name, "имя не может быть пустым"));
            }
            if motions[..index]
                .iter()
                .any(|other| other.name == motion.name)
            {
                return Err(invalid(&motion.name, "имя повторяется"));
            }
            match &motion.source {
                MotionSource::Clip { clip } => check_clip(*clip, clips.len(), &motion.name)?,
                MotionSource::Tree { points, .. } => {
                    if points.is_empty() || points.len() > MAX_TREE_POINTS {
                        return Err(invalid(&motion.name, "в tree от 1 до 16 точек"));
                    }
                    for point in points {
                        check_clip(point.clip, clips.len(), &motion.name)?;
                        if !point.threshold.is_finite() {
                            return Err(invalid(&motion.name, "порог точки не конечен"));
                        }
                    }
                    let increasing = points
                        .windows(2)
                        .all(|pair| pair[0].threshold < pair[1].threshold);
                    if !increasing {
                        return Err(invalid(&motion.name, "пороги должны строго возрастать"));
                    }
                }
            }
        }
        Ok(Self {
            clips,
            motions,
            playbacks: Vec::new(),
            parameters: HashMap::new(),
            speed: 1.0,
            default_looping,
        })
    }

    /// Имена движений в порядке создания.
    pub fn motion_names(&self) -> impl Iterator<Item = &str> {
        self.motions.iter().map(|motion| motion.name.as_str())
    }

    /// Запускает движение: его цель веса становится 1, остальные уходят к 0.
    ///
    /// Если движение уже проигрывается и не закончилось, его время сохраняется.
    pub fn play(&mut self, motion: &str, options: PlayOptions) -> Result<(), AnimatorError> {
        let index = self.motion_index(motion)?;
        let then = options
            .then
            .as_deref()
            .map(|name| self.motion_index(name))
            .transpose()?;
        let fade = check_fade(options.fade)?;
        let looping = options.looping.unwrap_or(self.default_looping);

        for playback in &mut self.playbacks {
            if playback.motion != index {
                playback.target = 0.0;
                playback.fade = fade;
            }
        }
        match self
            .playbacks
            .iter_mut()
            .find(|playback| playback.motion == index)
        {
            Some(playback) if playback.target > 0.0 && !playback.finished => {
                // Ручной вес мог поднять цель ниже 1: запуск возвращает полный вес.
                playback.target = 1.0;
                playback.looping = looping;
                playback.then = then;
                playback.fade = fade;
            }
            Some(playback) => {
                playback.target = 1.0;
                playback.fade = fade;
                playback.time = 0.0;
                playback.looping = looping;
                playback.then = then;
                playback.finished = false;
            }
            None => self.playbacks.push(Playback {
                motion: index,
                weight: 0.0,
                target: 1.0,
                fade,
                time: 0.0,
                looping,
                then,
                finished: false,
            }),
        }
        if fade <= 0.0 {
            for playback in &mut self.playbacks {
                playback.weight = playback.target;
            }
        }
        Ok(())
    }

    /// Задаёт ручной вес движения, не меняя остальные. Значение зажимается в [0, 1].
    pub fn set_weight(
        &mut self,
        motion: &str,
        weight: f32,
        fade: f32,
    ) -> Result<(), AnimatorError> {
        let index = self.motion_index(motion)?;
        if !weight.is_finite() {
            return Err(AnimatorError::InvalidValue("вес"));
        }
        let fade = check_fade(fade)?;
        let target = weight.clamp(0.0, 1.0);
        let position = match self
            .playbacks
            .iter()
            .position(|playback| playback.motion == index)
        {
            Some(position) => {
                let playback = &mut self.playbacks[position];
                playback.target = target;
                playback.fade = fade;
                position
            }
            None => {
                self.playbacks.push(Playback {
                    motion: index,
                    weight: 0.0,
                    target,
                    fade,
                    time: 0.0,
                    looping: self.default_looping,
                    then: None,
                    finished: false,
                });
                self.playbacks.len() - 1
            }
        };
        // Мгновенный вес меняет только это движение; чужие переходы продолжают идти.
        if fade <= 0.0 {
            self.playbacks[position].weight = target;
        }
        Ok(())
    }

    /// Задаёт значение параметра blend tree.
    pub fn set_parameter(&mut self, name: &str, value: f32) -> Result<(), AnimatorError> {
        if !value.is_finite() {
            return Err(AnimatorError::InvalidValue("параметр"));
        }
        self.parameters.insert(name.to_owned(), value);
        Ok(())
    }

    /// Масштаб скорости времени всех движений, не меньше нуля.
    pub fn set_speed(&mut self, speed: f32) -> Result<(), AnimatorError> {
        if !speed.is_finite() || speed < 0.0 {
            return Err(AnimatorError::InvalidValue("скорость"));
        }
        self.speed = speed;
        Ok(())
    }

    /// Переводит проигрывающееся движение на `time` секунд. Цикл переносит время по
    /// длительности, нецикличное зажимается в диапазоне. Отмотка до конца снова даёт событие
    /// окончания; вес и остальные движения не меняются.
    pub fn seek(&mut self, motion: &str, time: f32) -> Result<(), AnimatorError> {
        if !time.is_finite() {
            return Err(AnimatorError::InvalidValue("время движения"));
        }
        let index = self.motion_index(motion)?;
        let duration = motion_duration(&self.clips, &self.motions[index].source);
        let Some(playback) = self
            .playbacks
            .iter_mut()
            .find(|playback| playback.motion == index)
        else {
            return Err(invalid(motion, "движение не проигрывается"));
        };
        playback.time = wrap_time(time, duration, playback.looping);
        if playback.time < duration {
            playback.finished = false;
        }
        Ok(())
    }

    /// Останавливает все движения: их веса уходят к 0 за `fade` секунд.
    pub fn stop(&mut self, fade: f32) -> Result<(), AnimatorError> {
        let fade = check_fade(fade)?;
        if fade <= 0.0 {
            self.playbacks.clear();
            return Ok(());
        }
        for playback in &mut self.playbacks {
            playback.target = 0.0;
            playback.fade = fade;
        }
        Ok(())
    }

    /// Продвигает время и веса на `dt` секунд и возвращает события окончания.
    pub fn advance(&mut self, dt: f32) -> Vec<MotionEvent> {
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        let mut events = Vec::new();
        let mut chained = Vec::new();
        let Self {
            clips,
            motions,
            playbacks,
            speed,
            ..
        } = self;
        for playback in playbacks.iter_mut() {
            let step = if playback.fade > 0.0 {
                dt / playback.fade
            } else {
                f32::INFINITY
            };
            playback.weight = move_toward(playback.weight, playback.target, step);
            playback.time += dt * *speed;
            let duration = motion_duration(clips, &motions[playback.motion].source);
            if playback.looping {
                playback.time = wrap_time(playback.time, duration, true);
            } else if playback.time >= duration {
                playback.time = duration;
                if playback.target > 0.0 && !playback.finished {
                    playback.finished = true;
                    let name = &motions[playback.motion].name;
                    events.push(MotionEvent::Finished {
                        motion: name.clone(),
                    });
                    if let Some(next) = playback.then {
                        chained.push((next, playback.fade));
                    }
                }
            }
        }
        playbacks.retain(|playback| playback.weight > 0.0 || playback.target > 0.0);
        for (next, fade) in chained {
            self.play_index(next, fade);
        }
        events
    }

    /// Текущий вес проигрывания движения, если оно активно.
    pub fn weight(&self, motion: &str) -> Option<f32> {
        self.playback(motion).map(|playback| playback.weight)
    }

    /// Время движения в секундах, если оно активно.
    pub fn time(&self, motion: &str) -> Option<f32> {
        self.playback(motion).map(|playback| playback.time)
    }

    pub fn parameter(&self, name: &str) -> f32 {
        self.parameters.get(name).copied().unwrap_or(0.0)
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Длительность движения: клип или среднее по точкам tree.
    pub fn duration(&self, motion: &str) -> Option<f32> {
        let index = self.motion_index(motion).ok()?;
        Some(motion_duration(&self.clips, &self.motions[index].source))
    }

    /// Записывает в `out` итоговую позу: смесь активных движений и позы покоя.
    pub fn sample(&self, skeleton: &Skeleton, out: &mut Pose) {
        let rest = Pose::rest(skeleton);
        let total: f32 = self
            .playbacks
            .iter()
            .map(|playback| playback.weight.max(0.0))
            .sum();
        let scale = if total > 1.0 { 1.0 / total } else { 1.0 };
        let poses: Vec<(Pose, f32)> = self
            .playbacks
            .iter()
            .filter(|playback| playback.weight > 0.0)
            .map(|playback| (self.sample_motion(playback, &rest), playback.weight * scale))
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
        blend(&inputs, out);
    }

    fn sample_motion(&self, playback: &Playback, rest: &Pose) -> Pose {
        let mut pose = rest.clone();
        match &self.motions[playback.motion].source {
            MotionSource::Clip { clip } => {
                self.clips[*clip].sample(playback.time, playback.looping, &mut pose);
            }
            MotionSource::Tree { parameter, points } => {
                let value = self.parameter(parameter);
                let thresholds: Vec<f32> = points.iter().map(|point| point.threshold).collect();
                let duration = tree_duration(&self.clips, points);
                let phase = if duration > 0.0 {
                    (playback.time / duration).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let mut children = Vec::new();
                for (index, weight) in line_weights(&thresholds, value) {
                    let clip = &self.clips[points[index].clip];
                    // Дочерние клипы синхронизированы по нормализованной фазе.
                    let mut child = rest.clone();
                    clip.sample(phase * clip.duration(), false, &mut child);
                    children.push((child, weight));
                }
                let inputs: Vec<(&Pose, f32)> = children
                    .iter()
                    .map(|(child, weight)| (child, *weight))
                    .collect();
                blend(&inputs, &mut pose);
            }
        }
        pose
    }

    fn playback(&self, motion: &str) -> Option<&Playback> {
        let index = self.motion_index(motion).ok()?;
        self.playbacks
            .iter()
            .find(|playback| playback.motion == index)
    }

    fn play_index(&mut self, index: usize, fade: f32) {
        let name = self.motions[index].name.clone();
        let options = PlayOptions {
            fade,
            looping: None,
            then: None,
        };
        // Имя уже проверено при создании, поэтому ошибки здесь не возникают.
        let _ = self.play(&name, options);
    }

    fn motion_index(&self, name: &str) -> Result<usize, AnimatorError> {
        self.motions
            .iter()
            .position(|motion| motion.name == name)
            .ok_or_else(|| AnimatorError::UnknownMotion(name.to_owned()))
    }
}

fn check_clip(clip: usize, count: usize, motion: &str) -> Result<(), AnimatorError> {
    if clip < count {
        Ok(())
    } else {
        Err(invalid(motion, "клип не объявлен"))
    }
}

fn check_fade(fade: f32) -> Result<f32, AnimatorError> {
    if fade.is_finite() && fade >= 0.0 {
        Ok(fade)
    } else {
        Err(AnimatorError::InvalidValue("время перехода"))
    }
}

fn invalid(motion: &str, reason: &'static str) -> AnimatorError {
    AnimatorError::InvalidMotion {
        motion: motion.to_owned(),
        reason,
    }
}

fn move_toward(current: f32, target: f32, step: f32) -> f32 {
    if current < target {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

fn motion_duration(clips: &[Clip], source: &MotionSource) -> f32 {
    match source {
        MotionSource::Clip { clip } => clips[*clip].duration(),
        MotionSource::Tree { points, .. } => tree_duration(clips, points),
    }
}

fn tree_duration(clips: &[Clip], points: &[TreePoint]) -> f32 {
    if points.is_empty() {
        return 0.0;
    }
    let sum: f32 = points
        .iter()
        .map(|point| clips[point.clip].duration())
        .sum();
    sum / points.len() as f32
}

#[cfg(test)]
mod tests {
    use super::{
        Animator, AnimatorError, Motion, MotionEvent, MotionSource, PlayOptions, TreePoint,
    };
    use crate::clip::Clip;
    use crate::pose::Pose;
    use crate::skeleton::Skeleton;
    use engine_assets::{
        ModelAnimation, ModelInheritMode, ModelKey, ModelNode, ModelSkeleton, ModelTrack,
        ModelTransform,
    };

    fn skeleton() -> Skeleton {
        Skeleton::from_model(&ModelSkeleton {
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
            joints: Vec::new(),
        })
    }

    /// Клип, который держит x = `x` на протяжении `duration` секунд.
    fn constant_clip(name: &str, x: f32, duration: f32) -> Clip {
        let skeleton = skeleton();
        let animation = ModelAnimation {
            name: name.to_owned(),
            duration,
            tracks: vec![ModelTrack {
                node: "bone".to_owned(),
                translation: vec![
                    ModelKey {
                        time: 0.0,
                        value: [x, 0.0, 0.0],
                    },
                    ModelKey {
                        time: duration,
                        value: [x, 0.0, 0.0],
                    },
                ],
                rotation: Vec::new(),
                scale: Vec::new(),
            }],
        };
        Clip::bind(&animation, &skeleton)
    }

    fn animator(looping: bool) -> Animator {
        Animator::new(
            vec![constant_clip("a", 2.0, 1.0), constant_clip("b", 10.0, 1.0)],
            vec![
                Motion {
                    name: "a".to_owned(),
                    source: MotionSource::Clip { clip: 0 },
                },
                Motion {
                    name: "b".to_owned(),
                    source: MotionSource::Clip { clip: 1 },
                },
            ],
            looping,
        )
        .unwrap()
    }

    fn sampled_x(animator: &Animator) -> f32 {
        let skeleton = skeleton();
        let mut pose = Pose::rest(&skeleton);
        animator.sample(&skeleton, &mut pose);
        pose.transforms[0].translation.x
    }

    fn play(animator: &mut Animator, motion: &str, fade: f32) {
        animator
            .play(
                motion,
                PlayOptions {
                    fade,
                    ..Default::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn crossfade_mixes_outgoing_and_incoming_motions_over_fade_time() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        assert!((sampled_x(&animator) - 2.0).abs() < 1e-5);
        play(&mut animator, "b", 1.0);
        animator.advance(0.5);
        assert!((animator.weight("a").unwrap() - 0.5).abs() < 1e-6);
        assert!((animator.weight("b").unwrap() - 0.5).abs() < 1e-6);
        assert!((sampled_x(&animator) - 6.0).abs() < 1e-4);
        animator.advance(0.5);
        assert!((sampled_x(&animator) - 10.0).abs() < 1e-4);
        animator.advance(0.0);
        assert_eq!(animator.weight("a"), None, "угасшее движение удаляется");
    }

    #[test]
    fn manual_weight_blends_against_rest_pose_without_touching_other_motions() {
        let mut animator = animator(true);
        animator.set_weight("a", 0.25, 0.0).unwrap();
        assert!(
            (sampled_x(&animator) - 0.5).abs() < 1e-5,
            "0.25 * 2 + 0.75 * rest(0)"
        );
        animator.set_weight("b", 0.5, 0.0).unwrap();
        assert!((sampled_x(&animator) - (0.5 + 5.0)).abs() < 1e-4);
    }

    #[test]
    fn instant_manual_weight_keeps_other_crossfades_running() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        play(&mut animator, "b", 1.0);
        animator.advance(0.5);
        assert!((animator.weight("a").unwrap() - 0.5).abs() < 1e-6);
        animator.set_weight("b", 0.3, 0.0).unwrap();
        assert!(
            (animator.weight("b").unwrap() - 0.3).abs() < 1e-6,
            "мгновенный вес"
        );
        assert!(
            (animator.weight("a").unwrap() - 0.5).abs() < 1e-6,
            "чужой crossfade не снапается"
        );
        // Исходящее движение продолжает угасать с прежней скоростью: 0.5 - 0.25 / 1.0 s.
        animator.advance(0.25);
        assert!(
            (animator.weight("a").unwrap() - 0.25).abs() < 1e-6,
            "переход продолжается"
        );
    }

    #[test]
    fn play_after_manual_weight_restores_full_target() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        animator.set_weight("a", 0.5, 0.0).unwrap();
        play(&mut animator, "a", 0.0);
        assert_eq!(animator.weight("a"), Some(1.0));
        animator.set_weight("a", 0.5, 1.0).unwrap();
        play(&mut animator, "a", 1.0);
        animator.advance(1.0);
        assert!(
            (animator.weight("a").unwrap() - 1.0).abs() < 1e-6,
            "play возвращает цель 1 даже после ручного веса"
        );
    }

    #[test]
    fn one_shot_motion_emits_finished_once_and_chains_then() {
        let mut animator = animator(true);
        animator
            .play(
                "a",
                PlayOptions {
                    fade: 0.0,
                    looping: Some(false),
                    then: Some("b".to_owned()),
                },
            )
            .unwrap();
        assert!(animator.advance(0.5).is_empty());
        let events = animator.advance(0.6);
        assert_eq!(
            events,
            [MotionEvent::Finished {
                motion: "a".to_owned()
            }],
            "событие приходит один раз"
        );
        assert!((animator.weight("b").unwrap() - 1.0).abs() < 1e-6);
        assert!(animator.advance(2.0).is_empty(), "повторных событий нет");
        assert!(
            (sampled_x(&animator) - 10.0).abs() < 1e-5,
            "b держит последний кадр"
        );
    }

    #[test]
    fn looping_motion_wraps_time_and_never_finishes() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        assert!(animator.advance(2.5).is_empty());
        assert!((animator.time("a").unwrap() - 0.5).abs() < 1e-5);
    }

    #[test]
    fn playing_the_same_motion_keeps_its_time_until_it_finishes() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        animator.advance(0.4);
        play(&mut animator, "a", 0.0);
        assert!((animator.time("a").unwrap() - 0.4).abs() < 1e-5);
    }

    #[test]
    fn speed_scales_motion_time() {
        let mut animator = animator(true);
        animator.set_speed(2.0).unwrap();
        play(&mut animator, "a", 0.0);
        animator.advance(0.25);
        assert!((animator.time("a").unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn seek_moves_a_playing_motion_and_rearms_its_finish_event() {
        let mut animator = animator(false);
        animator
            .play(
                "a",
                PlayOptions {
                    fade: 0.0,
                    looping: Some(false),
                    then: None,
                },
            )
            .unwrap();
        assert_eq!(animator.advance(1.5).len(), 1, "движение закончилось");

        animator.seek("a", 0.25).unwrap();
        assert!((animator.time("a").unwrap() - 0.25).abs() < 1e-6);
        assert!(animator.advance(0.5).is_empty());
        assert_eq!(
            animator.advance(0.3).len(),
            1,
            "после отмотки событие приходит снова"
        );

        animator.seek("a", 7.0).unwrap();
        assert!(
            (animator.time("a").unwrap() - 1.0).abs() < 1e-6,
            "зажим в конце"
        );
        animator.seek("a", -1.0).unwrap();
        assert!(animator.time("a").unwrap().abs() < 1e-6, "зажим в начале");
    }

    #[test]
    fn seek_wraps_looping_motions_and_rejects_idle_or_invalid_requests() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        animator.seek("a", 2.25).unwrap();
        assert!(
            (animator.time("a").unwrap() - 0.25).abs() < 1e-6,
            "цикл переносит время"
        );

        assert_eq!(
            animator.seek("b", 0.5),
            Err(AnimatorError::InvalidMotion {
                motion: "b".to_owned(),
                reason: "движение не проигрывается",
            })
        );
        assert_eq!(
            animator.seek("missing", 0.5),
            Err(AnimatorError::UnknownMotion("missing".to_owned()))
        );
        assert!(matches!(
            animator.seek("a", f32::NAN),
            Err(AnimatorError::InvalidValue(_))
        ));
        assert!(
            (animator.time("a").unwrap() - 0.25).abs() < 1e-6,
            "ошибки не меняют состояние"
        );
    }

    #[test]
    fn stop_without_fade_removes_every_motion() {
        let mut animator = animator(true);
        play(&mut animator, "a", 0.0);
        animator.stop(0.0).unwrap();
        assert_eq!(animator.weight("a"), None);
        assert!((sampled_x(&animator)).abs() < 1e-6);
    }

    #[test]
    fn unknown_motion_and_invalid_values_are_rejected_without_changing_state() {
        let mut animator = animator(true);
        assert_eq!(
            animator.play("nope", PlayOptions::default()),
            Err(AnimatorError::UnknownMotion("nope".to_owned()))
        );
        assert!(animator.set_weight("a", f32::NAN, 0.0).is_err());
        assert!(animator.set_speed(-1.0).is_err());
        assert!(animator.set_parameter("speed", f32::INFINITY).is_err());
        assert!(
            animator
                .play(
                    "a",
                    PlayOptions {
                        fade: -1.0,
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert!(
            animator
                .play(
                    "a",
                    PlayOptions {
                        then: Some("missing".to_owned()),
                        ..Default::default()
                    }
                )
                .is_err()
        );
        assert_eq!(animator.weight("a"), None, "ошибки не меняют состояние");
    }

    #[test]
    fn construction_rejects_duplicate_names_missing_clips_and_bad_trees() {
        let clips = || vec![constant_clip("a", 0.0, 1.0)];
        let clip = |name: &str| Motion {
            name: name.to_owned(),
            source: MotionSource::Clip { clip: 0 },
        };
        let duplicate = Animator::new(clips(), vec![clip("a"), clip("a")], true);
        assert!(matches!(
            duplicate,
            Err(AnimatorError::InvalidMotion { .. })
        ));
        let missing = Animator::new(
            clips(),
            vec![Motion {
                name: "m".to_owned(),
                source: MotionSource::Clip { clip: 3 },
            }],
            true,
        );
        assert!(matches!(missing, Err(AnimatorError::InvalidMotion { .. })));
        let tree = |points: Vec<TreePoint>| {
            Animator::new(
                clips(),
                vec![Motion {
                    name: "t".to_owned(),
                    source: MotionSource::Tree {
                        parameter: "speed".to_owned(),
                        points,
                    },
                }],
                true,
            )
        };
        let point = |threshold| TreePoint { threshold, clip: 0 };
        assert!(
            tree(vec![point(1.0), point(1.0)]).is_err(),
            "пороги строго возрастают"
        );
        assert!(tree(Vec::new()).is_err(), "нужна хотя бы одна точка");
        assert!(
            tree((0..17).map(|i| point(i as f32)).collect()).is_err(),
            "не больше 16 точек"
        );
        assert!(tree(vec![point(f32::NAN)]).is_err());
        assert!(tree(vec![point(0.0), point(1.0)]).is_ok());
    }

    #[test]
    fn blend_tree_selects_weights_by_parameter() {
        let clips = vec![
            constant_clip("walk", 0.0, 1.0),
            constant_clip("run", 10.0, 1.0),
        ];
        let mut animator = Animator::new(
            clips,
            vec![Motion {
                name: "locomotion".to_owned(),
                source: MotionSource::Tree {
                    parameter: "speed".to_owned(),
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
            }],
            true,
        )
        .unwrap();
        play(&mut animator, "locomotion", 0.0);
        animator.set_parameter("speed", 0.0).unwrap();
        assert!((sampled_x(&animator)).abs() < 1e-5);
        animator.set_parameter("speed", 1.0).unwrap();
        assert!((sampled_x(&animator) - 10.0).abs() < 1e-5);
        animator.set_parameter("speed", 0.25).unwrap();
        assert!((sampled_x(&animator) - 2.5).abs() < 1e-4);
    }
}
