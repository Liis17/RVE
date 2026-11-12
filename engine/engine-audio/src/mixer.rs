//! Микшер голосов без устройства: фиксированный пул, линейный ресемплинг, затухание,
//! constant-power панорама, группы громкости с эффектами шины, fade-in и fade-out.
//!
//! Всё, что вызывается из audio callback ([`Mixer::apply`], [`Mixer::render`]), не выделяет
//! память, не берёт блокировок и не освобождает клипы: голос держит `Arc<Clip>`, а кеш
//! загрузчика хранит ту же ссылку. Кольцо стрим-голоса возвращается потоку `rve-audio`
//! через [`Mixer::drain_retired`]. Граница с потоком `rve-audio` — SPSC-кольца `rtrb`.

use crate::decode::Clip;
use crate::effects::Bus;
use crate::stream::{Frame, Pull, StreamReader};
use engine_scene::{AudioGroup, AudioGroupEffects, VoiceId};
use std::f32::consts::FRAC_PI_4;
use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::sync::{Arc, Mutex, PoisonError};

/// Размер пула голосов. Слот голоса совпадает с «полосой» позиции в [`Spatial`].
pub const VOICE_SLOTS: usize = 64;
/// Длина блока внутреннего микширования в кадрах.
const BLOCK_FRAMES: usize = 256;
/// Длительность fade-out при остановке голоса и наименьшее затухание по запросу.
const FADE_SECONDS: f32 = 0.005;
/// Голоса, вытесненные из полосы, затухают здесь, не занимая полос.
const TAIL_SLOTS: usize = VOICE_SLOTS;
/// Полоса затухающего вытесненного голоса: позиция излучателя больше не читается.
const NO_LANE: usize = usize::MAX;
/// Стрим-голоса, которые callback держит до возврата потоку `rve-audio`.
pub const RETIRED_CAPACITY: usize = VOICE_SLOTS + TAIL_SLOTS;
const GROUPS: usize = 4;
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
    /// Плавное появление в секундах; `0` — сразу на полной громкости.
    pub fade_in: f32,
    /// Декодировать по частям в потоке стриминга вместо загрузки клипа в кеш.
    pub stream: bool,
}

/// Команды от потока `rve-audio` к callback.
#[derive(Debug)]
pub enum MixerCommand {
    Play {
        voice: VoiceId,
        lane: usize,
        clip: Arc<Clip>,
        request: PlayRequest,
    },
    /// Стрим-голос: кольцо уже заполнено потоком стриминга.
    PlayStream {
        voice: VoiceId,
        lane: usize,
        stream: StreamReader,
        request: PlayRequest,
    },
    Stop {
        voice: VoiceId,
    },
    /// Остановка с затуханием `seconds`; короче [`FADE_SECONDS`] — как `Stop`.
    FadeOut {
        voice: VoiceId,
        seconds: f32,
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
    /// Фильтры и реверберация шины группы; `Master` обрабатывает общую сумму.
    SetGroupEffects {
        group: AudioGroup,
        effects: AudioGroupEffects,
    },
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

/// Откуда голос берёт кадры.
enum Source {
    Clip {
        clip: Arc<Clip>,
        /// Позиция в кадрах клипа.
        position: f64,
    },
    Stream {
        reader: StreamReader,
        /// Два соседних кадра для интерполяции и доля между ними.
        window: [Frame; 2],
        fraction: f64,
    },
}

/// Состояние голоса внутри микшера.
struct Voice {
    id: VoiceId,
    source: Source,
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
    /// Fade-in при запуске и fade-out при остановке.
    envelope: Envelope,
    /// Последняя прочитанная позиция излучателя: используется, пока писатель её меняет.
    emitter: [f32; 3],
}

/// Огибающая голоса: множитель `0..=1`, который меняется на `step` за кадр.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Envelope {
    level: f32,
    step: f32,
    /// Кадров до тишины после остановки; `None`, пока голос не останавливают.
    stopping: Option<u32>,
}

impl Envelope {
    fn start(fade_in_frames: u32) -> Self {
        if fade_in_frames == 0 {
            Self {
                level: 1.0,
                step: 0.0,
                stopping: None,
            }
        } else {
            Self {
                level: 0.0,
                step: 1.0 / fade_in_frames as f32,
                stopping: None,
            }
        }
    }

    /// Начинает затухание от текущего уровня до нуля за `frames` кадров. Повторная остановка
    /// может только ускорить уже идущее затухание.
    fn stop(&mut self, frames: u32) {
        let frames = frames.max(1);
        if self.stopping.is_some_and(|left| left <= frames) {
            return;
        }
        self.step = -self.level / frames as f32;
        self.stopping = Some(frames);
    }

    /// Голос остановлен и уже затих.
    fn silent(&self) -> bool {
        self.stopping.is_some() && self.level <= 0.0
    }

    /// Сдвигает уровень на кадр; возвращает `true`, когда остановленный голос затих.
    fn advance(&mut self) -> bool {
        match self.stopping.as_mut() {
            Some(left) => {
                *left -= 1;
                self.level = (self.level + self.step).max(0.0);
                if *left == 0 {
                    self.level = 0.0;
                    return true;
                }
            }
            None if self.step > 0.0 => {
                self.level += self.step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.step = 0.0;
                }
            }
            None => {}
        }
        false
    }
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
    /// Вытесненные голоса, которые доигрывают fade-out без полосы и без `VoiceEnded`.
    tails: [Option<Voice>; TAIL_SLOTS],
    /// Индексы [`AudioGroup`]: Master, Music, Effects, Ambient.
    group_gains: [f32; GROUPS],
    /// Шины групп с эффектами; индекс 0 (Master) обрабатывает общую сумму.
    buses: [Bus; GROUPS],
    /// Сумма голосов каждой группы за блок.
    bus_buffers: [[f32; BLOCK_FRAMES * 2]; GROUPS],
    paused: bool,
    mono_output: bool,
    ended: EndedQueue,
    /// Кольца стрим-голосов, которые ждут возврата потоку `rve-audio`.
    retired: [Option<StreamReader>; RETIRED_CAPACITY],
    /// Последний согласованный слушатель: используется, пока писатель его меняет.
    listener: Listener,
}

impl Mixer {
    /// Выделяет буферы шин; вызывать до запуска устройства, не в callback.
    pub fn new(device_rate: u32, spatial: Arc<Spatial>) -> Self {
        let fade_frames = seconds_to_frames(FADE_SECONDS, device_rate).max(1);
        Self {
            device_rate,
            fade_frames,
            spatial,
            voices: std::array::from_fn(|_| None),
            tails: std::array::from_fn(|_| None),
            group_gains: [1.0; GROUPS],
            buses: std::array::from_fn(|_| Bus::new(device_rate)),
            bus_buffers: [[0.0; BLOCK_FRAMES * 2]; GROUPS],
            paused: false,
            mono_output: false,
            ended: EndedQueue {
                items: [None; ENDED_CAPACITY],
                len: 0,
            },
            retired: std::array::from_fn(|_| None),
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
            } => {
                let source = Source::Clip {
                    clip,
                    position: 0.0,
                };
                self.start(voice, lane, source, request);
            }
            MixerCommand::PlayStream {
                voice,
                lane,
                mut stream,
                request,
            } => {
                let first = pull_frame(&mut stream);
                let second = pull_frame(&mut stream);
                let source = Source::Stream {
                    reader: stream,
                    window: [first, second],
                    fraction: 0.0,
                };
                self.start(voice, lane, source, request);
            }
            MixerCommand::Stop { voice } => self.begin_fade(voice, self.fade_frames),
            MixerCommand::FadeOut { voice, seconds } => {
                let frames = seconds_to_frames(seconds, self.device_rate).max(self.fade_frames);
                self.begin_fade(voice, frames);
            }
            MixerCommand::StopAll => {
                for slot in self.voices.iter_mut().flatten() {
                    slot.envelope.stop(self.fade_frames);
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
            MixerCommand::SetGroupEffects { group, effects } => {
                self.buses[group_index(group)].set(effects);
            }
        }
    }

    fn start(&mut self, id: VoiceId, lane: usize, source: Source, request: PlayRequest) {
        if lane >= VOICE_SLOTS {
            self.retire_source(source);
            self.push_ended(id);
            return;
        }
        // Слот занят только при вытеснении: слот освобождается по VoiceEnded, поэтому остановленный
        // голос ещё затухает. Вытесненный голос уходит в хвосты и доигрывает fade-out там; полоса
        // и `VoiceEnded` достаются ему сразу, поэтому событие не повторяется.
        if let Some(mut previous) = self.voices[lane].take() {
            self.push_ended(previous.id);
            previous.envelope.stop(self.fade_frames);
            previous.lane = NO_LANE;
            match self.tails.iter_mut().find(|tail| tail.is_none()) {
                Some(tail) => *tail = Some(previous),
                None => self.retire(previous),
            }
        }
        // Некорректный pitch не должен сделать шаг NaN: голос без конца. Вызовы его уже отклоняют.
        let pitch = if request.pitch.is_finite() && request.pitch > 0.0 {
            request.pitch
        } else {
            1.0
        };
        let sample_rate = match &source {
            Source::Clip { clip, .. } => clip.sample_rate,
            Source::Stream { reader, .. } => reader.sample_rate,
        };
        let step = f64::from(sample_rate) / f64::from(self.device_rate) * f64::from(pitch);
        let fade_in = seconds_to_frames(request.fade_in, self.device_rate);
        self.voices[lane] = Some(Voice {
            id,
            source,
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
            envelope: Envelope::start(fade_in),
            emitter: [0.0; 3],
        });
    }

    fn begin_fade(&mut self, id: VoiceId, frames: u32) {
        if let Some(voice) = self.find_mut(id) {
            voice.envelope.stop(frames);
        }
    }

    /// Освобождает голос: кольцо стрима уходит в очередь возврата, ссылку на клип держит кеш.
    fn retire(&mut self, voice: Voice) {
        self.retire_source(voice.source);
    }

    fn retire_source(&mut self, source: Source) {
        let Source::Stream { reader, .. } = source else {
            return;
        };
        match self.retired.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some(reader),
            // Очередь рассчитана на все голоса и хвосты сразу; при переполнении кольцо утекает,
            // но не освобождается в callback.
            None => std::mem::forget(reader),
        }
    }

    /// Передаёт кольца завершённых стрим-голосов в `send`; отклонённые остаются в очереди.
    pub fn drain_retired(
        &mut self,
        mut send: impl FnMut(StreamReader) -> Result<(), StreamReader>,
    ) {
        for slot in &mut self.retired {
            let Some(reader) = slot.take() else {
                continue;
            };
            if let Err(reader) = send(reader) {
                *slot = Some(reader);
                return;
            }
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
        let samples = frames * 2;
        if let Some(listener) = self.spatial.listener() {
            self.listener = listener;
        }
        let listener = self.listener;
        for buffer in &mut self.bus_buffers {
            buffer[..samples].fill(0.0);
        }
        for slot in 0..VOICE_SLOTS {
            let Some(voice) = self.voices[slot].as_mut() else {
                continue;
            };
            let group = group_index(voice.group);
            let base = self.group_gains[0] * self.group_gains[group];
            let bus = &mut self.bus_buffers[group][..samples];
            if mix_voice(voice, bus, frames, base, &self.spatial, listener) {
                let voice = self.voices[slot].take().expect("голос найден выше");
                self.push_ended(voice.id);
                self.retire(voice);
            }
        }
        for slot in 0..TAIL_SLOTS {
            let Some(voice) = self.tails[slot].as_mut() else {
                continue;
            };
            let group = group_index(voice.group);
            let base = self.group_gains[0] * self.group_gains[group];
            let bus = &mut self.bus_buffers[group][..samples];
            if mix_voice(voice, bus, frames, base, &self.spatial, listener) {
                let voice = self.tails[slot].take().expect("хвост найден выше");
                self.retire(voice);
            }
        }
        // Голоса группы Master идут прямо в сумму; шина Master обрабатывает её целиком.
        for group in 0..GROUPS {
            let bus = &mut self.bus_buffers[group][..samples];
            if group != 0 {
                self.buses[group].process(bus);
            }
            for (out, value) in stereo.iter_mut().zip(bus.iter()) {
                *out += value;
            }
        }
        self.buses[0].process(stereo);
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

    #[cfg(test)]
    pub(crate) fn fading_tails(&self) -> usize {
        self.tails.iter().flatten().count()
    }
}

fn seconds_to_frames(seconds: f32, rate: u32) -> u32 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds * rate as f32).ceil().min(u32::MAX as f32) as u32
    } else {
        0
    }
}

/// Кадр из кольца стрима; при опустошении — тишина.
fn pull_frame(reader: &mut StreamReader) -> Frame {
    match reader.pull() {
        Pull::Frame(frame) => frame,
        Pull::Underrun | Pull::End => [0.0; 2],
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
    if voice.envelope.silent() {
        return true;
    }
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
        let gain = (start_gain + (target - start_gain) * ramp) * voice.envelope.level;
        let ((source_left, source_right), ended) = voice.next_frame();
        let mono = (source_left + source_right) * 0.5;
        let mut left = source_left + (mono - source_left) * mono_mix;
        let mut right = source_right + (mono - source_right) * mono_mix;
        left = voice.lowpass_step(0, left, gains.lowpass);
        right = voice.lowpass_step(1, right, gains.lowpass);
        stereo[index * 2] += left * gain * gains.left;
        stereo[index * 2 + 1] += right * gain * gains.right;

        if ended || voice.envelope.advance() {
            finished = true;
            break;
        }
    }
    voice.gain = target;
    finished
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
    /// Текущий кадр источника; сдвигает позицию на шаг. Второе значение — источник закончился.
    fn next_frame(&mut self) -> ((f32, f32), bool) {
        match &mut self.source {
            Source::Clip { clip, position } => {
                let channels = usize::from(clip.channels);
                let value = sample(clip, channels, *position, self.looping);
                let frame_count = clip.frames() as f64;
                *position += self.step;
                if *position >= frame_count {
                    if self.looping && frame_count > 0.0 {
                        *position %= frame_count;
                    } else {
                        return (value, true);
                    }
                }
                (value, false)
            }
            Source::Stream {
                reader,
                window,
                fraction,
            } => {
                let t = *fraction as f32;
                let [a, b] = *window;
                let value = (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t);
                *fraction += self.step;
                while *fraction >= 1.0 {
                    *fraction -= 1.0;
                    window[0] = window[1];
                    window[1] = match reader.pull() {
                        Pull::Frame(frame) => frame,
                        Pull::Underrun => [0.0; 2],
                        Pull::End => return (value, true),
                    };
                }
                (value, false)
            }
        }
    }

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
