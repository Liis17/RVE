//! Состояние проигрывания объекта: crossfade, ручные веса, одномерный blend tree и события окончания.
//!
//! Вес каждого проигрывания плавно идёт к цели. Оставшаяся часть суммы веса до 1 берётся
//! из позы покоя, поэтому затухание из покоя и ручной вес меньше 1 работают предсказуемо.

use crate::blend::{blend_with, line_weights_inline};
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
    /// Движение пересекло именованный маркер клипа.
    Marker {
        motion: String,
        clip: String,
        marker: String,
    },
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

/// Данные движения, которые вычисляются один раз в [`Animator::new`]: клипы после создания
/// не меняются, поэтому длительность и пороги не нужно пересчитывать на каждом кадре.
#[derive(Debug)]
struct PreparedMotion {
    /// Длительность клипа или средняя длительность точек tree.
    duration: f32,
    /// Нет маркеров — `advance` не вычисляет веса дерева для этого движения.
    has_markers: bool,
    /// Пороги точек tree в их порядке; для клипа пусто.
    thresholds: Vec<f32>,
}

/// Пересечение без строковых копий; список хранится и переиспользуется аниматором.
#[derive(Clone, Copy, Debug)]
struct MarkerIntersection {
    fraction_key: u32,
    playback: usize,
    motion: usize,
    tree_point: usize,
    clip: usize,
    marker: usize,
}

#[derive(Clone, Copy, Debug)]
struct TickInterval {
    before: f32,
    after: f32,
    unwrapped_after: f32,
    wraps: f64,
}

struct MarkerCollection<'a> {
    clips: &'a [Clip],
    parameters: &'a HashMap<String, f32>,
    playback: &'a Playback,
    playback_order: usize,
    marker_min_weight: f32,
    interval: TickInterval,
    output: &'a mut Vec<MarkerIntersection>,
}

impl PreparedMotion {
    fn new(clips: &[Clip], source: &MotionSource) -> Self {
        match source {
            MotionSource::Clip { clip } => Self {
                duration: clips[*clip].duration(),
                has_markers: !clips[*clip].markers().is_empty(),
                thresholds: Vec::new(),
            },
            MotionSource::Tree { points, .. } => Self {
                duration: tree_duration(clips, points),
                has_markers: points
                    .iter()
                    .any(|point| !clips[point.clip].markers().is_empty()),
                thresholds: points.iter().map(|point| point.threshold).collect(),
            },
        }
    }
}

/// Проигрывание нескольких движений объекта с плавными переходами.
#[derive(Debug)]
pub struct Animator {
    clips: Vec<Clip>,
    motions: Vec<Motion>,
    /// Параллельно `motions`.
    prepared: Vec<PreparedMotion>,
    playbacks: Vec<Playback>,
    parameters: HashMap<String, f32>,
    speed: f32,
    marker_min_weight: f32,
    marker_intersections: Vec<MarkerIntersection>,
    default_looping: bool,
}

/// Поза активного движения и её вес в итоговой смеси.
#[derive(Debug)]
struct WeightedPose {
    pose: Pose,
    weight: f32,
}

/// Рабочие позы [`Animator::sample_into`]. Мост держит один набор и передаёт его всем аниматорам
/// по очереди. Каждая поза перед использованием заново приводится к покою текущего скелета,
/// поэтому результат не зависит от того, чем набор был заполнен раньше.
#[derive(Debug)]
pub(crate) struct SampleBuffers {
    /// Поза покоя текущего скелета; она же добирает недостающий вес.
    rest: Pose,
    /// Позы активных движений. Лишние элементы сохраняются, чтобы не выделять память заново.
    motions: Vec<WeightedPose>,
    /// Позы детей одномерного дерева на время одного движения.
    children: Vec<Pose>,
}

impl Default for SampleBuffers {
    fn default() -> Self {
        Self {
            rest: Pose {
                transforms: Vec::new(),
            },
            motions: Vec::new(),
            children: Vec::new(),
        }
    }
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
        let prepared = motions
            .iter()
            .map(|motion| PreparedMotion::new(&clips, &motion.source))
            .collect();
        Ok(Self {
            clips,
            motions,
            prepared,
            playbacks: Vec::new(),
            parameters: HashMap::new(),
            speed: 1.0,
            marker_min_weight: 0.5,
            marker_intersections: Vec::new(),
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

    /// Задаёт минимальный эффективный вес движения для выдачи маркеров.
    pub fn set_marker_min_weight(&mut self, weight: f32) -> Result<(), AnimatorError> {
        if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
            return Err(AnimatorError::InvalidValue("порог веса маркера"));
        }
        self.marker_min_weight = weight;
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
        let duration = self.prepared[index].duration;
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

    /// Продвигает время и веса на `dt` секунд и возвращает события движения.
    pub fn advance(&mut self, dt: f32) -> Vec<MotionEvent> {
        let dt = if dt.is_finite() && dt > 0.0 { dt } else { 0.0 };
        let mut finished_events = Vec::new();
        let mut chained = Vec::new();
        let Self {
            motions,
            prepared,
            clips,
            playbacks,
            parameters,
            speed,
            marker_min_weight,
            marker_intersections,
            ..
        } = self;
        marker_intersections.clear();
        for (playback_order, playback) in playbacks.iter_mut().enumerate() {
            let step = if playback.fade > 0.0 {
                dt / playback.fade
            } else {
                f32::INFINITY
            };
            playback.weight = move_toward(playback.weight, playback.target, step);
            let before = playback.time;
            let advanced_by = dt * *speed;
            let unwrapped_after = before + advanced_by;
            let duration = prepared[playback.motion].duration;
            let (after, wraps) = if playback.looping {
                let after = wrap_time(unwrapped_after, duration, true);
                let wraps = if duration.is_finite() && duration > 0.0 && unwrapped_after.is_finite()
                {
                    (unwrapped_after as f64 / duration as f64).floor()
                } else {
                    0.0
                };
                (after, wraps)
            } else {
                (unwrapped_after, 0.0)
            };
            playback.time = after;
            if advanced_by > 0.0 && unwrapped_after > before {
                let mut collection = MarkerCollection {
                    clips,
                    parameters,
                    playback,
                    playback_order,
                    marker_min_weight: *marker_min_weight,
                    interval: TickInterval {
                        before,
                        after,
                        unwrapped_after,
                        wraps,
                    },
                    output: marker_intersections,
                };
                collection.collect(&motions[playback.motion], &prepared[playback.motion]);
            }
            if !playback.looping && unwrapped_after >= duration {
                playback.time = duration;
                if playback.target > 0.0 && !playback.finished {
                    playback.finished = true;
                    let name = &motions[playback.motion].name;
                    finished_events.push(MotionEvent::Finished {
                        motion: name.clone(),
                    });
                    if let Some(next) = playback.then {
                        chained.push((next, playback.fade));
                    }
                }
            }
        }
        playbacks.retain(|playback| playback.weight > 0.0 || playback.target > 0.0);
        let events = if marker_intersections.is_empty() {
            finished_events
        } else {
            marker_intersections.sort_unstable_by(|left, right| {
                left.fraction_key
                    .cmp(&right.fraction_key)
                    .then_with(|| left.playback.cmp(&right.playback))
                    .then_with(|| left.tree_point.cmp(&right.tree_point))
                    .then_with(|| left.marker.cmp(&right.marker))
            });
            let mut events = Vec::with_capacity(marker_intersections.len() + finished_events.len());
            for intersection in marker_intersections.iter() {
                events.push(MotionEvent::Marker {
                    motion: motions[intersection.motion].name.clone(),
                    clip: clips[intersection.clip].name().to_owned(),
                    marker: clips[intersection.clip].markers()[intersection.marker]
                        .name
                        .clone(),
                });
            }
            events.append(&mut finished_events);
            events
        };
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
        Some(self.prepared[index].duration)
    }

    /// Записывает в `out` итоговую позу: смесь активных движений и позы покоя.
    ///
    /// Для каждого кадра моста используется [`Animator::sample_into`] с общими буферами;
    /// этот вызов выделяет временные буферы сам.
    pub fn sample(&self, skeleton: &Skeleton, out: &mut Pose) {
        self.sample_into(skeleton, &mut SampleBuffers::default(), out);
    }

    /// Как [`Animator::sample`], но рабочие позы берутся из `buffers`. После прогрева повторные
    /// вызовы не выделяют память, пока число активных движений не превышает прежний максимум.
    pub(crate) fn sample_into(
        &self,
        skeleton: &Skeleton,
        buffers: &mut SampleBuffers,
        out: &mut Pose,
    ) {
        let SampleBuffers {
            rest,
            motions,
            children,
        } = buffers;
        rest.reset_to_rest(skeleton);
        let total: f32 = self
            .playbacks
            .iter()
            .map(|playback| playback.weight.max(0.0))
            .sum();
        let scale = if total > 1.0 { 1.0 / total } else { 1.0 };
        let mut active = 0;
        for playback in self
            .playbacks
            .iter()
            .filter(|playback| playback.weight > 0.0)
        {
            if active == motions.len() {
                motions.push(WeightedPose {
                    pose: Pose {
                        transforms: Vec::new(),
                    },
                    weight: 0.0,
                });
            }
            let slot = &mut motions[active];
            self.sample_motion_into(playback, skeleton, children, &mut slot.pose);
            slot.weight = playback.weight * scale;
            active += 1;
        }
        let rest_weight = (1.0 - total * scale).max(0.0);
        let count = active + usize::from(rest_weight > 0.0);
        if count == 0 {
            out.reset_to_rest(skeleton);
            return;
        }
        let motions: &[WeightedPose] = &motions[..active];
        let rest: &Pose = rest;
        blend_with(
            count,
            |index| match motions.get(index) {
                Some(entry) => (&entry.pose, entry.weight),
                None => (rest, rest_weight),
            },
            out,
        );
    }

    /// Записывает позу одного движения в `out`, начиная с позы покоя.
    fn sample_motion_into(
        &self,
        playback: &Playback,
        skeleton: &Skeleton,
        children: &mut Vec<Pose>,
        out: &mut Pose,
    ) {
        out.reset_to_rest(skeleton);
        match &self.motions[playback.motion].source {
            MotionSource::Clip { clip } => {
                self.clips[*clip].sample(playback.time, playback.looping, out);
            }
            MotionSource::Tree { parameter, points } => {
                let prepared = &self.prepared[playback.motion];
                let value = self.parameter(parameter);
                let phase = if prepared.duration > 0.0 {
                    (playback.time / prepared.duration).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let weights = line_weights_inline(&prepared.thresholds, value);
                let weights = weights.as_slice();
                for (slot, &(index, _)) in weights.iter().enumerate() {
                    if children.len() == slot {
                        children.push(Pose {
                            transforms: Vec::new(),
                        });
                    }
                    let child = &mut children[slot];
                    let clip = &self.clips[points[index].clip];
                    child.reset_to_rest(skeleton);
                    // Дочерние клипы синхронизированы по нормализованной фазе.
                    clip.sample(phase * clip.duration(), false, child);
                }
                let children: &[Pose] = children.as_slice();
                blend_with(
                    weights.len(),
                    |index| (&children[index], weights[index].1),
                    out,
                );
            }
        }
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

impl MarkerCollection<'_> {
    fn collect(&mut self, motion: &Motion, prepared: &PreparedMotion) {
        if !prepared.has_markers
            || self.playback.weight <= 0.0
            || prepared.duration <= 0.0
            || !prepared.duration.is_finite()
        {
            return;
        }
        match &motion.source {
            MotionSource::Clip { clip } => self.add_clip(
                *clip,
                self.playback.weight,
                0,
                prepared.duration,
                self.playback.looping,
            ),
            MotionSource::Tree { parameter, points } => {
                let value = self.parameters.get(parameter).copied().unwrap_or(0.0);
                let weights = line_weights_inline(&prepared.thresholds, value);
                // At most two neighboring points are active; repeated clip references are merged here.
                let mut active = [(usize::MAX, 0.0_f32, usize::MAX); 2];
                let mut active_len = 0;
                for &(point_index, weight) in weights.as_slice() {
                    if weight <= 0.0 {
                        continue;
                    }
                    let clip = points[point_index].clip;
                    if let Some(existing) = active[..active_len]
                        .iter_mut()
                        .find(|entry| entry.0 == clip)
                    {
                        existing.1 += weight;
                    } else {
                        let first_tree_point = points
                            .iter()
                            .position(|point| point.clip == clip)
                            .unwrap_or(point_index);
                        active[active_len] = (clip, weight, first_tree_point);
                        active_len += 1;
                    }
                }
                for &(clip, weight, first_tree_point) in &active[..active_len] {
                    self.add_clip(
                        clip,
                        self.playback.weight * weight,
                        first_tree_point,
                        prepared.duration,
                        self.playback.looping,
                    );
                }
            }
        }
    }

    fn add_clip(
        &mut self,
        clip_index: usize,
        weight: f32,
        tree_point: usize,
        motion_duration: f32,
        looping: bool,
    ) {
        if weight <= 0.0 || weight < self.marker_min_weight {
            return;
        }
        let clip = &self.clips[clip_index];
        let clip_duration = clip.duration();
        if clip_duration <= 0.0 || !clip_duration.is_finite() {
            return;
        }
        for (marker_index, marker) in clip.markers().iter().enumerate() {
            if !marker.time.is_finite() || marker.time < 0.0 || marker.time > clip_duration {
                continue;
            }
            let position = if motion_duration == clip_duration {
                marker.time as f64
            } else {
                marker.time as f64 / clip_duration as f64 * motion_duration as f64
            };
            if let Some(fraction) =
                marker_crossing_fraction(looping, self.interval, motion_duration, position)
            {
                self.output.push(MarkerIntersection {
                    fraction_key: (fraction * 1_000_000.0).round() as u32,
                    playback: self.playback_order,
                    motion: self.playback.motion,
                    tree_point,
                    clip: clip_index,
                    marker: marker_index,
                });
            }
        }
    }
}

fn marker_crossing_fraction(
    looping: bool,
    interval: TickInterval,
    duration: f32,
    position: f64,
) -> Option<f64> {
    if !duration.is_finite() || duration <= 0.0 || !position.is_finite() {
        return None;
    }
    let before = interval.before as f64;
    let duration = duration as f64;
    if looping {
        let after = interval.after as f64;
        let travel = if interval.wraps < 1.0 {
            after - before
        } else {
            duration - before + (interval.wraps - 1.0) * duration + after
        };
        if travel <= 0.0 {
            return None;
        }
        let position = if position == duration { 0.0 } else { position };
        let distance = if position >= before {
            position - before
        } else {
            duration - before + position
        };
        (travel >= duration || distance < travel).then_some(distance / travel)
    } else {
        let travel = interval.unwrapped_after as f64 - before;
        if travel <= 0.0 || before >= duration {
            return None;
        }
        if position == duration {
            (interval.unwrapped_after >= duration as f32).then_some((duration - before) / travel)
        } else {
            let distance = position - before;
            (distance >= 0.0 && distance < travel).then_some(distance / travel)
        }
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
mod sample_tests;

#[cfg(test)]
mod tests {
    use super::{
        Animator, AnimatorError, Motion, MotionEvent, MotionSource, PlayOptions, TreePoint,
    };
    use crate::clip::Clip;
    use crate::pose::Pose;
    use crate::skeleton::Skeleton;
    use engine_assets::{
        ModelAnimation, ModelInheritMode, ModelKey, ModelMarker, ModelNode, ModelSkeleton,
        ModelTrack, ModelTransform,
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
            markers: Vec::new(),
            file_marker_warnings: Vec::new(),
            marker_warnings: Vec::new(),
        };
        Clip::bind(&animation, &skeleton)
    }

    fn marker_clip(name: &str, duration: f32, markers: &[(&str, f32)]) -> Clip {
        let animation = ModelAnimation {
            name: name.to_owned(),
            duration,
            tracks: Vec::new(),
            markers: markers
                .iter()
                .map(|(name, time)| ModelMarker {
                    name: (*name).to_owned(),
                    time: *time,
                })
                .collect(),
            file_marker_warnings: Vec::new(),
            marker_warnings: Vec::new(),
        };
        Clip::bind(&animation, &skeleton())
    }

    fn marker_animator(clip: Clip, looping: bool) -> Animator {
        Animator::new(
            vec![clip],
            vec![Motion {
                name: "motion".to_owned(),
                source: MotionSource::Clip { clip: 0 },
            }],
            looping,
        )
        .unwrap()
    }

    fn markers(events: Vec<MotionEvent>) -> Vec<(String, String, String)> {
        events
            .into_iter()
            .filter_map(|event| match event {
                MotionEvent::Marker {
                    motion,
                    clip,
                    marker,
                } => Some((motion, clip, marker)),
                MotionEvent::Finished { .. } => None,
            })
            .collect()
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

    #[test]
    fn marker_order_uses_crossing_time_then_declared_order_for_ties() {
        let clip = marker_clip(
            "Walk",
            1.0,
            &[
                ("late", 0.75),
                ("first", 0.25),
                ("tie_a", 0.5),
                ("tie_b", 0.5),
            ],
        );
        let mut animator = marker_animator(clip, false);
        play(&mut animator, "motion", 0.0);

        assert_eq!(
            markers(animator.advance(0.8))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["first", "tie_a", "tie_b", "late"]
        );
    }

    #[test]
    fn cyclic_markers_cover_start_exact_turn_and_large_delta_once() {
        let clip = marker_clip("Walk", 1.0, &[("zero", 0.0), ("middle", 0.5), ("end", 1.0)]);
        let mut animator = marker_animator(clip, true);
        play(&mut animator, "motion", 0.0);

        assert_eq!(
            markers(animator.advance(0.25))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["zero", "end"],
            "циклический маркер на D эквивалентен нулю"
        );
        play(&mut animator, "motion", 0.0);
        assert_eq!(
            markers(animator.advance(0.75))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["middle"],
            "точное достижение границы цикла не включает нулевой маркер конца интервала"
        );
        assert_eq!(
            markers(animator.advance(0.1))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["zero", "end"]
        );

        animator.seek("motion", 0.4).unwrap();
        assert_eq!(
            markers(animator.advance(3.0))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["middle", "zero", "end"],
            "большой dt выдаёт каждый маркер единожды по первому пересечению"
        );
    }

    #[test]
    fn non_looping_end_marker_fires_on_arrival_but_not_while_holding_or_seeking_end() {
        let clip = marker_clip("Walk", 1.0, &[("middle", 0.5), ("end", 1.0)]);
        let mut animator = marker_animator(clip, false);
        play(&mut animator, "motion", 0.0);

        assert!(
            markers(animator.advance(0.5)).is_empty(),
            "верхняя граница открыта"
        );
        assert_eq!(
            markers(animator.advance(0.5))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["middle", "end"],
            "маркер на D включается при первом достижении конца"
        );
        assert!(
            markers(animator.advance(0.25)).is_empty(),
            "на D время стоит"
        );
        animator.seek("motion", 1.0).unwrap();
        assert!(
            markers(animator.advance(0.25)).is_empty(),
            "seek(D) не пересекает D"
        );

        animator.seek("motion", 0.25).unwrap();
        assert_eq!(
            markers(animator.advance(0.3))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["middle"],
            "отмотка снова разрешает пересечь маркер"
        );

        let mut large = marker_animator(
            marker_clip("Large", 1.0, &[("first", 0.25), ("second", 0.75)]),
            false,
        );
        play(&mut large, "motion", 0.0);
        assert_eq!(
            markers(large.advance(4.0))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["first", "second"],
            "большой dt не выдаёт маркер повторно в нецикличном движении"
        );
    }

    #[test]
    fn non_looping_end_marker_does_not_repeat_while_fading_at_duration() {
        let mut animator = marker_animator(marker_clip("Walk", 1.0, &[("end", 1.0)]), false);
        play(&mut animator, "motion", 0.0);

        assert_eq!(markers(animator.advance(1.0)).len(), 1);
        animator.stop(1.0).unwrap();
        assert!(markers(animator.advance(0.25)).is_empty());
        assert!(markers(animator.advance(0.25)).is_empty());
    }

    #[test]
    fn seek_skips_crossed_markers_and_fires_the_marker_at_its_new_time() {
        let clip = marker_clip(
            "Walk",
            1.0,
            &[("zero", 0.0), ("middle", 0.5), ("late", 0.75)],
        );
        let mut animator = marker_animator(clip, false);
        play(&mut animator, "motion", 0.0);
        assert_eq!(markers(animator.advance(0.1)).len(), 1);

        animator.seek("motion", 0.5).unwrap();
        assert_eq!(
            markers(animator.advance(0.1))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["middle"]
        );
        animator.seek("motion", 0.9).unwrap();
        assert!(markers(animator.advance(0.05)).is_empty());
    }

    #[test]
    fn zero_duration_zero_delta_and_lost_f32_delta_emit_no_markers() {
        let mut zero = marker_animator(marker_clip("Zero", 0.0, &[("zero", 0.0)]), true);
        play(&mut zero, "motion", 0.0);
        assert!(markers(zero.advance(1.0)).is_empty());

        let mut large = marker_animator(
            marker_clip("Large", 200_000_000.0, &[("here", 100_000_000.0)]),
            true,
        );
        play(&mut large, "motion", 0.0);
        large.seek("motion", 100_000_000.0).unwrap();
        let before = large.time("motion").unwrap();
        assert!(large.set_speed(-1.0).is_err());
        assert_eq!(
            large.time("motion"),
            Some(before),
            "отказ скорости не меняет время"
        );
        assert!(markers(large.advance(1.0e-8)).is_empty());
        assert_eq!(
            large.time("motion"),
            Some(before),
            "малый d потерян при сложении f32"
        );
        large.set_speed(0.0).unwrap();
        assert!(markers(large.advance(1.0)).is_empty());
        large.set_speed(1.0).unwrap();
        assert!(markers(large.advance(0.0)).is_empty());
    }

    #[test]
    fn marker_weight_threshold_uses_updated_crossfade_weights() {
        let clips = vec![
            marker_clip("A", 1.0, &[("a", 0.5)]),
            marker_clip("B", 1.0, &[("b", 0.5)]),
        ];
        let mut animator = Animator::new(
            clips,
            vec![
                Motion {
                    name: "A".to_owned(),
                    source: MotionSource::Clip { clip: 0 },
                },
                Motion {
                    name: "B".to_owned(),
                    source: MotionSource::Clip { clip: 1 },
                },
            ],
            true,
        )
        .unwrap();
        play(&mut animator, "A", 0.0);
        assert!(markers(animator.advance(0.4)).is_empty());
        play(&mut animator, "B", 1.0);

        assert_eq!(
            markers(animator.advance(0.2))
                .into_iter()
                .map(|(motion, _, marker)| (motion, marker))
                .collect::<Vec<_>>(),
            [("A".to_owned(), "a".to_owned())],
            "вес A после тика 0.8 проходит порог, B с весом 0.2 — нет"
        );
        assert_eq!(
            markers(animator.advance(0.4))
                .into_iter()
                .map(|(motion, _, marker)| (motion, marker))
                .collect::<Vec<_>>(),
            [("B".to_owned(), "b".to_owned())],
            "маркер B пересекается при весе 0.6; A уже ниже порога"
        );
        assert!(animator.set_marker_min_weight(f32::NAN).is_err());
        assert!(animator.set_marker_min_weight(1.1).is_err());

        let mut exact = marker_animator(marker_clip("Exact", 1.0, &[("edge", 0.0)]), true);
        exact.set_weight("motion", 0.5, 0.0).unwrap();
        assert_eq!(markers(exact.advance(0.1)).len(), 1, "порог включителен");

        let mut fading = marker_animator(
            marker_clip("Fade", 1.0, &[("above", 0.25), ("below", 0.75)]),
            true,
        );
        play(&mut fading, "motion", 0.0);
        fading.stop(1.0).unwrap();
        assert!(
            markers(fading.advance(0.25)).is_empty(),
            "маркер на конце интервала"
        );
        assert_eq!(
            markers(fading.advance(0.01)).len(),
            1,
            "fade ещё выше порога"
        );
        assert!(
            markers(fading.advance(0.49)).is_empty(),
            "маркер на конце интервала"
        );
        assert!(markers(fading.advance(0.01)).is_empty(), "fade ниже порога");
    }

    #[test]
    fn blend_tree_sums_repeated_clip_weights_and_syncs_different_durations_by_phase() {
        let repeated = marker_clip("Repeated", 1.0, &[("hit", 0.0)]);
        let mut duplicate_tree = Animator::new(
            vec![repeated],
            vec![Motion {
                name: "duplicate".to_owned(),
                source: MotionSource::Tree {
                    parameter: "speed".to_owned(),
                    points: vec![
                        TreePoint {
                            threshold: 0.0,
                            clip: 0,
                        },
                        TreePoint {
                            threshold: 1.0,
                            clip: 0,
                        },
                    ],
                },
            }],
            true,
        )
        .unwrap();
        duplicate_tree.set_parameter("speed", 0.5).unwrap();
        play(&mut duplicate_tree, "duplicate", 0.0);
        assert_eq!(
            markers(duplicate_tree.advance(0.1))
                .into_iter()
                .map(|(_, _, marker)| marker)
                .collect::<Vec<_>>(),
            ["hit"],
            "оба веса по 0.5 складываются и клип выдаёт маркер один раз"
        );

        let clips = vec![
            marker_clip("Short", 1.0, &[("short", 0.5)]),
            marker_clip("Long", 2.0, &[("long", 1.0)]),
        ];
        let mut tree = Animator::new(
            clips,
            vec![Motion {
                name: "tree".to_owned(),
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
            false,
        )
        .unwrap();
        tree.set_parameter("speed", 0.5).unwrap();
        play(&mut tree, "tree", 0.0);
        assert!(
            markers(tree.advance(0.75)).is_empty(),
            "фаза 0.5 — верхняя граница"
        );
        assert_eq!(
            markers(tree.advance(0.1))
                .into_iter()
                .map(|(_, clip, marker)| (clip, marker))
                .collect::<Vec<_>>(),
            [
                ("Short".to_owned(), "short".to_owned()),
                ("Long".to_owned(), "long".to_owned())
            ],
            "клипы разной длительности пересекают маркеры на одной фазе"
        );
    }

    #[test]
    fn blend_tree_markers_with_equal_phase_use_tree_order_after_fraction_quantization() {
        let clips = vec![
            marker_clip("A", 1.0, &[("a", 0.3)]),
            marker_clip("B", 3.0, &[("b", 0.9)]),
        ];
        let mut tree = Animator::new(
            clips,
            vec![Motion {
                name: "tree".to_owned(),
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
            false,
        )
        .unwrap();
        tree.set_parameter("speed", 0.5).unwrap();
        play(&mut tree, "tree", 0.0);

        assert_eq!(
            markers(tree.advance(1.5))
                .into_iter()
                .map(|(_, clip, marker)| (clip, marker))
                .collect::<Vec<_>>(),
            [
                ("A".to_owned(), "a".to_owned()),
                ("B".to_owned(), "b".to_owned())
            ]
        );
    }

    #[test]
    fn then_motion_starts_crossing_markers_on_the_following_tick_and_stop_zero_clears_them() {
        let clips = vec![
            marker_clip("A", 1.0, &[("a", 0.5)]),
            marker_clip("B", 1.0, &[("b", 0.0)]),
        ];
        let mut animator = Animator::new(
            clips,
            vec![
                Motion {
                    name: "A".to_owned(),
                    source: MotionSource::Clip { clip: 0 },
                },
                Motion {
                    name: "B".to_owned(),
                    source: MotionSource::Clip { clip: 1 },
                },
            ],
            false,
        )
        .unwrap();
        animator
            .play(
                "A",
                PlayOptions {
                    looping: Some(false),
                    then: Some("B".to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            markers(animator.advance(1.0))
                .into_iter()
                .map(|(motion, _, marker)| (motion, marker))
                .collect::<Vec<_>>(),
            [("A".to_owned(), "a".to_owned())]
        );
        assert_eq!(
            markers(animator.advance(0.1))
                .into_iter()
                .map(|(motion, _, marker)| (motion, marker))
                .collect::<Vec<_>>(),
            [("B".to_owned(), "b".to_owned())]
        );
        animator.stop(0.0).unwrap();
        assert!(markers(animator.advance(0.1)).is_empty());
    }
}
