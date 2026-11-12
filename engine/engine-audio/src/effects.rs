//! Эффекты шины группы: low-pass и high-pass (biquad второго порядка) и реверберация Freeverb.
//!
//! Буферы реверберации выделяются в [`Bus::new`] до запуска устройства; обработка в callback
//! не выделяет память и не берёт блокировок.

use engine_scene::{AudioGroupEffects, AudioReverb};
use std::f32::consts::{FRAC_1_SQRT_2, PI};

/// Задержки гребенчатых фильтров Freeverb при 44.1 kHz.
const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
/// Задержки всепропускающих фильтров Freeverb при 44.1 kHz.
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
/// Сдвиг задержек правого канала для стереобазы.
const STEREO_SPREAD: usize = 23;
const FIXED_GAIN: f32 = 0.015;
const SCALE_ROOM: f32 = 0.28;
const OFFSET_ROOM: f32 = 0.7;
const SCALE_DAMP: f32 = 0.4;
const ALLPASS_FEEDBACK: f32 = 0.5;
/// Значения хвоста меньше этого обнуляются: затухающие субнормальные числа резко замедляют
/// арифметику на x86 и могли бы привести к underrun в callback.
const DENORMAL_FLOOR: f32 = 1.0e-20;

fn flush_denormal(value: f32) -> f32 {
    if value.abs() < DENORMAL_FLOOR {
        0.0
    } else {
        value
    }
}

/// Biquad (RBJ cookbook, transposed direct form II) по двум каналам.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    state: [[f32; 2]; 2],
}

#[derive(Clone, Copy)]
enum FilterKind {
    LowPass,
    HighPass,
}

impl Biquad {
    fn new(kind: FilterKind, cutoff: f32, rate: u32) -> Self {
        let nyquist_guard = rate as f32 * 0.45;
        let cutoff = cutoff.clamp(1.0, nyquist_guard);
        let omega = 2.0 * PI * cutoff / rate as f32;
        let (sin, cos) = omega.sin_cos();
        let alpha = sin / (2.0 * FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = match kind {
            FilterKind::LowPass => ((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0),
            FilterKind::HighPass => ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0),
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            state: [[0.0; 2]; 2],
        }
    }

    fn process(&mut self, channel: usize, input: f32) -> f32 {
        let [z1, z2] = &mut self.state[channel];
        let output = self.b0 * input + *z1;
        *z1 = self.b1 * input - self.a1 * output + *z2;
        *z2 = self.b2 * input - self.a2 * output;
        // Нечисловое состояние не восстанавливается само: сбрасываем фильтр.
        if !output.is_finite() {
            self.state[channel] = [0.0; 2];
            return 0.0;
        }
        output
    }
}

struct Comb {
    buffer: Box<[f32]>,
    index: usize,
    filter_store: f32,
}

impl Comb {
    fn new(length: usize) -> Self {
        Self {
            buffer: vec![0.0; length.max(1)].into_boxed_slice(),
            index: 0,
            filter_store: 0.0,
        }
    }

    fn process(&mut self, input: f32, feedback: f32, damp: f32) -> f32 {
        let output = self.buffer[self.index];
        self.filter_store = flush_denormal(output * (1.0 - damp) + self.filter_store * damp);
        self.buffer[self.index] = flush_denormal(input + self.filter_store * feedback);
        self.index = (self.index + 1) % self.buffer.len();
        output
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.filter_store = 0.0;
    }
}

struct Allpass {
    buffer: Box<[f32]>,
    index: usize,
}

impl Allpass {
    fn new(length: usize) -> Self {
        Self {
            buffer: vec![0.0; length.max(1)].into_boxed_slice(),
            index: 0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let delayed = self.buffer[self.index];
        let output = delayed - input;
        self.buffer[self.index] = flush_denormal(input + delayed * ALLPASS_FEEDBACK);
        self.index = (self.index + 1) % self.buffer.len();
        output
    }

    fn clear(&mut self) {
        self.buffer.fill(0.0);
    }
}

/// Стерео-реверберация Freeverb: 8 гребенчатых и 4 всепропускающих фильтра на канал.
struct Reverb {
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<Allpass>; 2],
}

impl Reverb {
    fn new(rate: u32) -> Self {
        let scale = |samples: usize| samples * rate as usize / 44_100;
        let channel = |spread: usize| {
            (
                COMB_TUNING
                    .iter()
                    .map(|&length| Comb::new(scale(length + spread)))
                    .collect(),
                ALLPASS_TUNING
                    .iter()
                    .map(|&length| Allpass::new(scale(length + spread)))
                    .collect(),
            )
        };
        let (left_combs, left_allpasses) = channel(0);
        let (right_combs, right_allpasses) = channel(STEREO_SPREAD);
        Self {
            combs: [left_combs, right_combs],
            allpasses: [left_allpasses, right_allpasses],
        }
    }

    fn clear(&mut self) {
        self.combs.iter_mut().flatten().for_each(Comb::clear);
        self.allpasses.iter_mut().flatten().for_each(Allpass::clear);
    }

    /// Возвращает обработанный сигнал для одного кадра; вход — моно-сумма каналов.
    fn process(&mut self, input: f32, params: AudioReverb) -> [f32; 2] {
        let feedback = params.room_size * SCALE_ROOM + OFFSET_ROOM;
        let damp = params.damping * SCALE_DAMP;
        let input = input * FIXED_GAIN;
        let mut out = [0.0; 2];
        for (channel, value) in out.iter_mut().enumerate() {
            let mut sum = 0.0;
            for comb in &mut self.combs[channel] {
                sum += comb.process(input, feedback, damp);
            }
            for allpass in &mut self.allpasses[channel] {
                sum = allpass.process(sum);
            }
            *value = sum;
        }
        out
    }
}

/// Шина группы: настройки эффектов и их состояние.
pub struct Bus {
    rate: u32,
    effects: AudioGroupEffects,
    lowpass: Option<Biquad>,
    highpass: Option<Biquad>,
    reverb: Reverb,
}

impl Bus {
    /// Выделяет буферы реверберации для частоты устройства. Вызывать вне callback.
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            effects: AudioGroupEffects::default(),
            lowpass: None,
            highpass: None,
            reverb: Reverb::new(rate),
        }
    }

    /// Меняет эффекты. Включённый заново фильтр или реверберация начинают с чистого состояния,
    /// а у работающего фильтра при смене частоты сохраняется состояние, чтобы не было щелчка.
    pub fn set(&mut self, effects: AudioGroupEffects) {
        self.lowpass = filter(
            self.lowpass,
            FilterKind::LowPass,
            effects.lowpass_hz,
            self.rate,
        );
        self.highpass = filter(
            self.highpass,
            FilterKind::HighPass,
            effects.highpass_hz,
            self.rate,
        );
        if effects.reverb.is_some() && self.effects.reverb.is_none() {
            self.reverb.clear();
        }
        self.effects = effects;
    }

    /// Шина без эффектов пропускает сигнал без изменений.
    pub fn is_bypassed(&self) -> bool {
        self.lowpass.is_none() && self.highpass.is_none() && self.effects.reverb.is_none()
    }

    /// Обрабатывает interleaved-стерео блок на месте.
    pub fn process(&mut self, stereo: &mut [f32]) {
        if self.is_bypassed() {
            return;
        }
        for frame in stereo.as_chunks_mut::<2>().0 {
            for (channel, sample) in frame.iter_mut().enumerate() {
                if let Some(filter) = self.lowpass.as_mut() {
                    *sample = filter.process(channel, *sample);
                }
                if let Some(filter) = self.highpass.as_mut() {
                    *sample = filter.process(channel, *sample);
                }
            }
            if let Some(params) = self.effects.reverb {
                let input = (frame[0] + frame[1]) * 0.5;
                let wet = self.reverb.process(input, params);
                frame[0] += wet[0] * params.wet;
                frame[1] += wet[1] * params.wet;
            }
        }
    }
}

/// Пересчитывает фильтр: `0` выключает его, работающий фильтр сохраняет состояние.
fn filter(current: Option<Biquad>, kind: FilterKind, cutoff: f32, rate: u32) -> Option<Biquad> {
    if cutoff <= 0.0 || !cutoff.is_finite() {
        return None;
    }
    let mut next = Biquad::new(kind, cutoff, rate);
    if let Some(current) = current {
        next.state = current.state;
    }
    Some(next)
}

#[cfg(test)]
mod tests;
