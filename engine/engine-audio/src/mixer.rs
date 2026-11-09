//! Микшер голосов без устройства: фиксированный пул, линейный ресемплинг, затухание,
//! constant-power панорама, группы громкости и fade-out при остановке.
//!
//! Всё, что вызывается из audio callback ([`Mixer::apply`], [`Mixer::render`]), не выделяет
//! память, не берёт блокировок и не освобождает клипы: голос держит `Arc<Clip>`, а кеш
//! загрузчика хранит ту же ссылку. Граница с потоком `rve-audio` — SPSC-кольца `rtrb`.

use crate::decode::Clip;
use engine_scene::{AudioGroup, VoiceId};
use std::f32::consts::FRAC_PI_4;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::sync::{Arc, Mutex, PoisonError};

/// Размер пула голосов. Слот голоса совпадает с «полосой» позиции в [`Spatial`].
pub const VOICE_SLOTS: usize = 64;
/// Длина блока внутреннего микширования в кадрах.
const BLOCK_FRAMES: usize = 256;
/// Длительность fade-out при остановке голоса.
const FADE_SECONDS: f32 = 0.005;
/// Сколько раз callback перечитывает позицию, пока писатель её меняет; дальше берётся прежняя.
const SEQ_READ_ATTEMPTS: usize = 4;
/// Очередь завершённых голосов, которую callback держит, пока кольцо к потоку `rve-audio` полно.
/// Потеря события означает утечку слота, поэтому ёмкость с запасом на перезапуски в одном цикле
/// опроса; `push_ended` при переполнении всё же не выделяет память и не блокирует.
pub const ENDED_CAPACITY: usize = VOICE_SLOTS * 4;

/// Расстояния пространственного голоса: полная громкость до `min`, тишина от `max`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distances {
    pub min: f32,
    pub max: f32,
}

/// Кривая затухания пространственного голоса.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rolloff {
    /// `(min/d)·(max−d)/(max−min)`: прежняя кривая, по умолчанию для совместимости.
    #[default]
    Inverse,
    /// Линейно: 1 на `min`, 0 на `max`.
    Linear,
}

/// Параметры запуска голоса.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayRequest {
    pub group: AudioGroup,
    pub volume: f32,
    pub looping: bool,
    /// Дистанции пространственного голоса; `None` — 2D-звук без затухания и панорамы.
    pub spatial: Option<Distances>,
    /// Доля 3D-микширования `0..=1` при заданных `spatial`: `0` — 2D, `1` — полностью 3D.
    pub spatial_blend: f32,
    pub rolloff: Rolloff,
    /// Множитель скорости воспроизведения (и высоты тона), конечный и больше нуля.
    pub pitch: f32,
    /// Сводит стерео-клип к моно до панорамы.
    pub force_mono: bool,
}

/// Команды от потока `rve-audio` к callback.
#[derive(Clone, Debug, PartialEq)]
pub enum MixerCommand {
    Play {
        voice: VoiceId,
        lane: usize,
        clip: Arc<Clip>,
        request: PlayRequest,
    },
    Stop {
        voice: VoiceId,
    },
    StopAll,
    SetPaused(bool),
    SetVoiceVolume {
        voice: VoiceId,
        volume: f32,
    },
    SetGroupVolume {
        group: AudioGroup,
        volume: f32,
    },
    /// Сводит весь вывод к моно; применяется к следующему блоку.
    SetMonoOutput(bool),
}

/// Позиция, правая и передняя оси слушателя.
type Listener = ([f32; 3], [f32; 3], [f32; 3]);

/// Затухание за спиной слушателя: доля громкости, которую убирают при полностью заднем звуке.
const BEHIND_ATTENUATION: f32 = 0.25;
/// Коэффициент one-pole low-pass за спиной: при полностью заднем звуке `alpha = 1 - 0.8 = 0.2`.
const BEHIND_CUTOFF: f32 = 0.8;

/// Значение из нескольких слов под seqlock: писатель меняет его под блокировкой [`Spatial`],
/// читатель (callback) повторяет чтение не больше [`SEQ_READ_ATTEMPTS`] раз.
struct SeqSlot<const N: usize> {
    seq: AtomicU32,
    data: [AtomicU32; N],
}

impl<const N: usize> SeqSlot<N> {
    fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            data: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }

    fn store(&self, values: [f32; N]) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        for (cell, value) in self.data.iter().zip(values) {
            cell.store(value.to_bits(), Ordering::Relaxed);
        }
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// Возвращает согласованное значение или `None`, если писатель не успел закончить запись.
    fn try_load(&self) -> Option<[f32; N]> {
        for _ in 0..SEQ_READ_ATTEMPTS {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let values = std::array::from_fn(|index| {
                f32::from_bits(self.data[index].load(Ordering::Relaxed))
            });
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return Some(values);
            }
        }
        None
    }
}

/// Последние позиции излучателей и слушателя. Не очередь: новые значения перезаписывают старые.
pub struct Spatial {
    emitters: [SeqSlot<3>; VOICE_SLOTS],
    /// Позиция слушателя, его правая и передняя оси (`[px, py, pz, rx, ry, rz, fx, fy, fz]`).
    listener: SeqSlot<9>,
    /// Писатели (игровой поток, редактор, любые клоны хендла) выстраиваются в очередь здесь.
    /// Callback только читает и эту блокировку не берёт.
    writers: Mutex<()>,
}

impl Spatial {
    pub fn new() -> Self {
        Self {
            emitters: std::array::from_fn(|_| SeqSlot::new()),
            listener: SeqSlot::new(),
            writers: Mutex::new(()),
        }
    }

    /// Пишет позицию излучателя. Писатели сериализуются блокировкой `writers`.
    pub fn set_emitter(&self, lane: usize, position: [f32; 3]) {
        let _writer = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = self.emitters.get(lane) {
            slot.store(position);
        }
    }

    /// Позиция излучателя; `None`, если писатель сейчас меняет её (вызывающий берёт прежнюю).
    pub fn emitter(&self, lane: usize) -> Option<[f32; 3]> {
        self.emitters.get(lane)?.try_load()
    }

    /// Пишет слушателя. Писатели сериализуются блокировкой `writers`.
    pub fn set_listener(&self, position: [f32; 3], right: [f32; 3], forward: [f32; 3]) {
        let _writer = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
        self.listener.store([
            position[0],
            position[1],
            position[2],
            right[0],
            right[1],
            right[2],
            forward[0],
            forward[1],
            forward[2],
        ]);
    }

    fn listener(&self) -> Option<Listener> {
        let [px, py, pz, rx, ry, rz, fx, fy, fz] = self.listener.try_load()?;
        Some(([px, py, pz], [rx, ry, rz], [fx, fy, fz]))
    }
}

impl Default for Spatial {
    fn default() -> Self {
        Self::new()
    }
}

/// Состояние голоса внутри микшера.
struct Voice {
    id: VoiceId,
    clip: Arc<Clip>,
    /// Позиция в кадрах клипа.
    position: f64,
    /// Кадров клипа на кадр устройства.
    step: f64,
    looping: bool,
    group: AudioGroup,
    volume: f32,
    distances: Option<Distances>,
    spatial_blend: f32,
    rolloff: Rolloff,
    force_mono: bool,
    lane: usize,
    /// Текущий (сглаженный) множитель громкости; меняется линейно внутри блока.
    gain: f32,
    /// Состояние one-pole low-pass по левому и правому каналу за спиной слушателя.
    lowpass: [f32; 2],
    /// Fade-out после остановки; `None`, пока голос не останавливают.
    fade: Option<Fade>,
    /// Последняя прочитанная позиция излучателя: используется, пока писатель её меняет.
    emitter: [f32; 3],
}

/// Кадры fade-out: всего и осталось.
#[derive(Clone, Copy)]
struct Fade {
    total: u32,
    left: u32,
}

/// Завершённые голоса, которые ещё не переданы потоку `rve-audio`.
struct EndedQueue {
    items: [Option<VoiceId>; ENDED_CAPACITY],
    len: usize,
}

pub struct Mixer {
    device_rate: u32,
    fade_frames: u32,
    spatial: Arc<Spatial>,
    voices: [Option<Voice>; VOICE_SLOTS],
    /// Индексы [`AudioGroup`]: Master, Music, Effects, Ambient.
    group_gains: [f32; 4],
    paused: bool,
    mono_output: bool,
    ended: EndedQueue,
    /// Последний согласованный слушатель: используется, пока писатель его меняет.
    listener: Listener,
}

impl Mixer {
    pub fn new(device_rate: u32, spatial: Arc<Spatial>) -> Self {
        let fade_frames = ((FADE_SECONDS * device_rate as f32).ceil() as u32).max(1);
        Self {
            device_rate,
            fade_frames,
            spatial,
            voices: std::array::from_fn(|_| None),
            group_gains: [1.0; 4],
            paused: false,
            mono_output: false,
            ended: EndedQueue {
                items: [None; ENDED_CAPACITY],
                len: 0,
            },
            listener: ([0.0; 3], [0.0; 3], [0.0; 3]),
        }
    }

    /// Применяет команду потока `rve-audio`.
    pub fn apply(&mut self, command: MixerCommand) {
        match command {
            MixerCommand::Play {
                voice,
                lane,
                clip,
                request,
            } => self.start(voice, lane, clip, request),
            MixerCommand::Stop { voice } => self.begin_fade(voice),
            MixerCommand::StopAll => {
                for slot in self.voices.iter_mut().flatten() {
                    begin_fade(slot, self.fade_frames);
                }
            }
            MixerCommand::SetPaused(paused) => self.paused = paused,
            MixerCommand::SetVoiceVolume { voice, volume } => {
                if let Some(voice) = self.find_mut(voice) {
                    voice.volume = volume;
                }
            }
            MixerCommand::SetGroupVolume { group, volume } => {
                self.group_gains[group_index(group)] = volume;
            }
            MixerCommand::SetMonoOutput(mono) => self.mono_output = mono,
        }
    }

    fn start(&mut self, id: VoiceId, lane: usize, clip: Arc<Clip>, request: PlayRequest) {
        if lane >= VOICE_SLOTS {
            self.push_ended(id);
            return;
        }
        // Слот занят только при вытеснении: слот освобождается по VoiceEnded, поэтому остановленный
        // голос ещё затухает. Вытесненный голос обрывается без fade: одна дорожка — один голос.
        if let Some(previous) = self.voices[lane].take() {
            self.push_ended(previous.id);
        }
        // Некорректный pitch не должен сделать шаг NaN: голос без конца. Вызовы его уже отклоняют.
        let pitch = if request.pitch.is_finite() && request.pitch > 0.0 {
            request.pitch
        } else {
            1.0
        };
        let step = f64::from(clip.sample_rate) / f64::from(self.device_rate) * f64::from(pitch);
        self.voices[lane] = Some(Voice {
            id,
            clip,
            position: 0.0,
            step,
            looping: request.looping,
            group: request.group,
            volume: request.volume,
            distances: request.spatial,
            spatial_blend: request.spatial_blend.clamp(0.0, 1.0),
            rolloff: request.rolloff,
            force_mono: request.force_mono,
            lane,
            gain: 0.0,
            lowpass: [0.0; 2],
            fade: None,
            emitter: [0.0; 3],
        });
    }

    fn begin_fade(&mut self, id: VoiceId) {
        let frames = self.fade_frames;
        if let Some(voice) = self.find_mut(id) {
            begin_fade(voice, frames);
        }
    }

    fn find_mut(&mut self, id: VoiceId) -> Option<&mut Voice> {
        self.voices
            .iter_mut()
            .flatten()
            .find(|voice| voice.id == id)
    }

    /// Заполняет interleaved-буфер устройства. Моно получает `(L+R)/2`, каналы сверх двух — нули.
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        if channels == 0 {
            return;
        }
        out.fill(0.0);
        let frames = out.len() / channels;
        let mut scratch = [0.0f32; BLOCK_FRAMES * 2];
        let mut done = 0;
        while done < frames {
            let count = (frames - done).min(BLOCK_FRAMES);
            let stereo = &mut scratch[..count * 2];
            self.render_block(stereo);
            for index in 0..count {
                let (mut left, mut right) = (stereo[index * 2], stereo[index * 2 + 1]);
                if self.mono_output || channels == 1 {
                    left = (left + right) * 0.5;
                    right = left;
                }
                let start = (done + index) * channels;
                let frame = &mut out[start..start + channels];
                if channels == 1 {
                    frame[0] = sanitize(left);
                } else {
                    frame[0] = sanitize(left);
                    frame[1] = sanitize(right);
                }
            }
            done += count;
        }
    }

    fn render_block(&mut self, stereo: &mut [f32]) {
        stereo.fill(0.0);
        if self.paused {
            return;
        }
        let frames = stereo.len() / 2;
        if let Some(listener) = self.spatial.listener() {
            self.listener = listener;
        }
        let listener = self.listener;
        for slot in 0..VOICE_SLOTS {
            let Some(voice) = self.voices[slot].as_mut() else {
                continue;
            };
            let base = self.group_gains[0] * self.group_gains[group_index(voice.group)];
            let finished = mix_voice(voice, stereo, frames, base, &self.spatial, listener);
            if finished {
                let voice = self.voices[slot].take().expect("голос найден выше");
                self.push_ended(voice.id);
            }
        }
    }

    fn push_ended(&mut self, id: VoiceId) {
        if self.ended.len < ENDED_CAPACITY {
            self.ended.items[self.ended.len] = Some(id);
            self.ended.len += 1;
        }
    }

    /// Передаёт завершённые голоса в `send`, пока тот возвращает `true`. Непереданные остаются в очереди.
    pub fn drain_ended(&mut self, mut send: impl FnMut(VoiceId) -> bool) {
        let mut sent = 0;
        while sent < self.ended.len {
            let Some(id) = self.ended.items[sent] else {
                break;
            };
            if !send(id) {
                break;
            }
            sent += 1;
        }
        self.ended.items.copy_within(sent..self.ended.len, 0);
        for item in &mut self.ended.items[self.ended.len - sent..self.ended.len] {
            *item = None;
        }
        self.ended.len -= sent;
    }

    /// Текущий множитель группы для тестов и отладки.
    #[cfg(test)]
    pub(crate) fn group_gain(&self, group: AudioGroup) -> f32 {
        self.group_gains[group_index(group)]
    }

    #[cfg(test)]
    pub(crate) fn active_voices(&self) -> usize {
        self.voices.iter().flatten().count()
    }
}

/// Mixes one voice into `stereo` and reports whether it has finished.
fn mix_voice(
    voice: &mut Voice,
    stereo: &mut [f32],
    frames: usize,
    base_gain: f32,
    spatial: &Spatial,
    listener: Listener,
) -> bool {
    let gains = match voice.distances {
        Some(distances) => {
            if let Some(emitter) = spatial.emitter(voice.lane) {
                voice.emitter = emitter;
            }
            spatial_gains(
                voice.emitter,
                listener,
                distances,
                voice.spatial_blend,
                voice.rolloff,
            )
        }
        None => SpatialGains::FLAT,
    };
    let target = base_gain * voice.volume * gains.attenuation;
    let start_gain = voice.gain;
    let channels = usize::from(voice.clip.channels);
    let frame_count = voice.clip.frames() as f64;
    // Доля моно-сведения до панорамы: force_mono — полностью; пространственный голос — по
    // spatial_blend, чтобы стерео плавно переходило в моно; 2D-голос сохраняет стерео.
    let mono_mix = if voice.force_mono {
        1.0
    } else if voice.distances.is_some() {
        voice.spatial_blend
    } else {
        0.0
    };
    let mut finished = false;

    for index in 0..frames {
        let ramp = (index + 1) as f32 / frames as f32;
        let mut gain = start_gain + (target - start_gain) * ramp;
        if let Some(fade) = voice.fade {
            gain *= fade.left as f32 / fade.total as f32;
        }
        let (source_left, source_right) =
            sample(&voice.clip, channels, voice.position, voice.looping);
        let mono = (source_left + source_right) * 0.5;
        let mut left = source_left + (mono - source_left) * mono_mix;
        let mut right = source_right + (mono - source_right) * mono_mix;
        left = voice.lowpass_step(0, left, gains.lowpass);
        right = voice.lowpass_step(1, right, gains.lowpass);
        stereo[index * 2] += left * gain * gains.left;
        stereo[index * 2 + 1] += right * gain * gains.right;

        voice.position += voice.step;
        if voice.position >= frame_count {
            if voice.looping {
                voice.position -= frame_count;
            } else {
                finished = true;
                break;
            }
        }
        if let Some(fade) = voice.fade.as_mut() {
            fade.left = fade.left.saturating_sub(1);
            if fade.left == 0 {
                finished = true;
                break;
            }
        }
    }
    voice.gain = target;
    finished
}

fn begin_fade(voice: &mut Voice, frames: u32) {
    voice.fade.get_or_insert(Fade {
        total: frames,
        left: frames,
    });
}

fn sample(clip: &Clip, channels: usize, position: f64, looping: bool) -> (f32, f32) {
    let frames = clip.frames();
    if frames == 0 {
        return (0.0, 0.0);
    }
    let index = (position.floor() as usize).min(frames - 1);
    let fraction = (position - index as f64) as f32;
    let next = if index + 1 < frames {
        index + 1
    } else if looping {
        0
    } else {
        frames - 1
    };
    let read = |frame: usize, channel: usize| clip.samples[frame * channels + channel];
    let interpolate = |channel: usize| {
        read(index, channel) + (read(next, channel) - read(index, channel)) * fraction
    };
    if channels == 1 {
        let value = interpolate(0);
        (value, value)
    } else {
        (interpolate(0), interpolate(1))
    }
}

/// Множители голоса за блок: панорама по каналам, общее затухание и коэффициент low-pass.
#[derive(Clone, Copy, Debug, PartialEq)]
struct SpatialGains {
    left: f32,
    right: f32,
    attenuation: f32,
    /// Коэффициент one-pole фильтра; `1` — без фильтрации.
    lowpass: f32,
}

impl SpatialGains {
    /// 2D-голос: без панорамы, затухания и фильтра.
    const FLAT: Self = Self {
        left: 1.0,
        right: 1.0,
        attenuation: 1.0,
        lowpass: 1.0,
    };
}

/// Панорама, затухание и фильтр по позиции излучателя. `blend` смешивает результат с 2D:
/// `0` даёт [`SpatialGains::FLAT`], `1` — полностью пространственный звук.
fn spatial_gains(
    emitter: [f32; 3],
    listener: Listener,
    distances: Distances,
    blend: f32,
    rolloff: Rolloff,
) -> SpatialGains {
    let (position, right, forward) = listener;
    let offset = [
        emitter[0] - position[0],
        emitter[1] - position[1],
        emitter[2] - position[2],
    ];
    let distance = (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();
    let curve = match rolloff {
        Rolloff::Inverse => attenuation(distance, distances),
        Rolloff::Linear => linear_attenuation(distance, distances),
    };
    let (pan_left, pan_right, attenuated, lowpass) =
        if distance <= f32::EPSILON || distance.is_nan() {
            (
                std::f32::consts::FRAC_1_SQRT_2,
                std::f32::consts::FRAC_1_SQRT_2,
                curve,
                1.0,
            )
        } else {
            let dot_right = dot(offset, right) / distance;
            let angle = (dot_right.clamp(-1.0, 1.0) + 1.0) * FRAC_PI_4;
            // Звук за спиной: `front < 0`. Сила эффекта растёт от 0 у бока до 1 позади.
            let behind = (-dot(offset, forward) / distance).clamp(0.0, 1.0);
            (
                angle.cos(),
                angle.sin(),
                curve * (1.0 - BEHIND_ATTENUATION * behind),
                1.0 - BEHIND_CUTOFF * behind,
            )
        };
    let blend = if blend.is_finite() {
        blend.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mix = |value: f32| 1.0 + (value - 1.0) * blend;
    SpatialGains {
        left: mix(pan_left),
        right: mix(pan_right),
        attenuation: mix(attenuated),
        lowpass: mix(lowpass),
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Линейное затухание: 1 у `min`, 0 у `max`.
pub fn linear_attenuation(distance: f32, distances: Distances) -> f32 {
    if distance <= distances.min {
        1.0
    } else if distance >= distances.max {
        0.0
    } else {
        (distances.max - distance) / (distances.max - distances.min)
    }
}

impl Voice {
    /// One-pole low-pass по каналу. `alpha >= 1` пропускает вход без изменений и держит состояние
    /// равным входу, чтобы переход к фильтру не давал щелчка.
    fn lowpass_step(&mut self, channel: usize, input: f32, alpha: f32) -> f32 {
        let state = &mut self.lowpass[channel];
        // Нечисловое состояние не восстанавливается само: NaN оставался бы в фильтре навсегда.
        if !state.is_finite() {
            *state = input;
        }
        if alpha >= 1.0 {
            *state = input;
        } else {
            *state += alpha * (input - *state);
        }
        *state
    }
}

/// `d ≤ min` → 1; `d ≥ max` → 0; между ними `(min/d)·(max−d)/(max−min)`.
pub fn attenuation(distance: f32, distances: Distances) -> f32 {
    if distance <= distances.min {
        1.0
    } else if distance >= distances.max {
        0.0
    } else {
        (distances.min / distance) * (distances.max - distance) / (distances.max - distances.min)
    }
}

/// Выход устройства: конечные значения в пределах [-1, 1]; NaN и бесконечность дают тишину.
fn sanitize(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

fn group_index(group: AudioGroup) -> usize {
    match group {
        AudioGroup::Master => 0,
        AudioGroup::Music => 1,
        AudioGroup::Effects => 2,
        AudioGroup::Ambient => 3,
    }
}

#[cfg(test)]
mod tests;
