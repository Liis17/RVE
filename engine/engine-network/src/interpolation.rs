//! Часы сервера на клиенте и буфер состояний объекта для интерполяции.
//!
//! Серверная метка тика переводится в секунды по `tick_hz` из общей конфигурации. Смещение
//! локальных часов оценивается сглаженно по приходящим метокам: оно включает среднюю задержку
//! сети, поэтому отображение отстаёт от сервера на `interpolation_delay_ms` плюс эта задержка.

use std::collections::VecDeque;
use std::time::Instant;

use crate::replication::Pose;

/// Наибольшее число состояний в буфере одного объекта.
pub(crate) const SAMPLE_LIMIT: usize = 32;
/// Вес новой метки в сглаженном смещении часов.
const SMOOTHING: f64 = 0.1;

/// Состояние с серверным временем в секундах.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub(crate) time: f64,
    pub(crate) pose: Pose,
}

/// Оценка серверного времени по локальным часам.
#[derive(Debug)]
pub(crate) struct Clock {
    epoch: Instant,
    tick_hz: u32,
    offset: Option<f64>,
    /// Последняя серверная метка, пришедшая от сервера; клиент отправляет её с состоянием владельца.
    pub(crate) last_tick: u32,
}

impl Clock {
    /// `tick_hz` больше нуля — проверяет мост при создании.
    pub(crate) fn new(epoch: Instant, tick_hz: u32) -> Self {
        Self {
            epoch,
            tick_hz,
            offset: None,
            last_tick: 0,
        }
    }

    fn elapsed(&self, now: Instant) -> f64 {
        now.saturating_duration_since(self.epoch).as_secs_f64()
    }

    /// Серверная метка для текущего момента: целое число тиков от эпохи.
    pub(crate) fn server_tick(&self, now: Instant) -> u32 {
        let micros = now.saturating_duration_since(self.epoch).as_micros();
        (micros * u128::from(self.tick_hz) / 1_000_000) as u32
    }

    /// Серверное время метки в секундах.
    pub(crate) fn tick_secs(&self, tick: u32) -> f64 {
        f64::from(tick) / f64::from(self.tick_hz)
    }

    /// Обновляет смещение по метке, пришедшей сейчас.
    pub(crate) fn observe(&mut self, tick: u32, now: Instant) {
        let observed = self.tick_secs(tick) - self.elapsed(now);
        self.offset = Some(match self.offset {
            None => observed,
            Some(offset) => offset + (observed - offset) * SMOOTHING,
        });
    }

    /// Оценка серверного времени сейчас; `None`, пока не пришло ни одной метки.
    pub(crate) fn estimate(&self, now: Instant) -> Option<f64> {
        self.offset.map(|offset| offset + self.elapsed(now))
    }
}

/// Добавляет состояние в буфер. Если новая поза дальше `snap_distance` от предыдущей, буфер
/// очищается: объект телепортируется, а не скользит.
pub(crate) fn push_sample(samples: &mut VecDeque<Sample>, sample: Sample, snap_distance: f32) {
    if let Some(last) = samples.back()
        && (sample.pose.position - last.pose.position).length() > snap_distance
    {
        samples.clear();
    }
    samples.push_back(sample);
    while samples.len() > SAMPLE_LIMIT {
        samples.pop_front();
    }
}

/// Поза для времени `time`. Без интерполяции — последнее состояние. Время раньше первого
/// состояния даёт первое; позже последнего — последнее. Старые состояния отбрасываются.
pub(crate) fn sample_at(
    samples: &mut VecDeque<Sample>,
    time: f64,
    interpolate: bool,
) -> Option<Pose> {
    if !interpolate {
        let last = *samples.back()?;
        samples.clear();
        samples.push_back(last);
        return Some(last.pose);
    }
    while samples.len() >= 2 && samples[1].time <= time {
        samples.pop_front();
    }
    let first = *samples.front()?;
    let Some(second) = samples.get(1).copied() else {
        return Some(first.pose);
    };
    if time <= first.time {
        return Some(first.pose);
    }
    let span = second.time - first.time;
    let t = if span > 0.0 {
        ((time - first.time) / span).clamp(0.0, 1.0)
    } else {
        1.0
    };
    Some(first.pose.lerp(second.pose, t as f32))
}
