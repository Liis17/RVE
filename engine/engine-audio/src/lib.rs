//! Игровой звук: декодирование клипов, микширование голосов и вывод на устройство.
//!
//! Потоки:
//! - поток `rve-audio` (здесь, [`AudioEngine::run`]) владеет кешем клипов, очередью ожидающих
//!   воспроизведений и выходным потоком `cpal`; он же освобождает слоты голосов;
//! - `rve-audio-decode` декодирует прочитанные байты, чтобы не блокировать цикл;
//! - `rve-audio-stream` читает и декодирует по частям клипы с `stream = true` ([`stream`]);
//! - audio callback работает только с [`mixer::Mixer`]: команды и завершённые голоса идут через
//!   SPSC-кольца `rtrb`, память и блокировки в нём не используются.
//!
//! Владельцы голосов (игровой поток через [`AudioBridge`], редактор) вызывают [`AudioHandle`].
//! Позиции излучателей и слушателя пишутся в seqlock-таблицу, а не в очередь команд.

pub mod bridge;
pub mod decode;
mod effects;
pub mod mixer;
pub mod output;
pub mod stream;

pub use bridge::AudioBridge;
pub use decode::{Clip, DecodeError};
pub use mixer::{Distances, PlayRequest, Rolloff};

use decode::{MAX_CLIP_FILE_BYTES, decode};
use engine_resources::{ResourceHandle, ResourceRequest};
use engine_scene::{AudioGroup, AudioGroupEffects, VoiceId};
use flume::{Receiver, Selector, Sender, TrySendError};
use mixer::{MixerCommand, RETIRED_CAPACITY, Spatial, VOICE_SLOTS};
use output::{Channels, Reconnect, Signals};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use stream::{ByteSource, OpenSource, StreamEvent, StreamJob, StreamReader, stream_loop};

/// Бюджет декодированных клипов в кеше (512 МБ).
const CACHE_LIMIT_BYTES: usize = 512 * 1024 * 1024;
/// Ожидание команд, пока есть незавершённые чтения или микшер (опрос кольца завершённых голосов).
const READ_POLL: Duration = Duration::from_millis(10);
/// Ожидание команд без чтений.
const IDLE_POLL: Duration = Duration::from_millis(500);
const COMMAND_CAPACITY: usize = 1024;
/// Кольцо команд микшера (поток `rve-audio` → callback).
const MIXER_CAPACITY: usize = 1024;

#[derive(Clone, Debug, Default)]
pub struct AudioConfig {
    /// Ресурсы проекта для чтения клипов. Без них клипы не загружаются.
    pub resources: Option<ResourceHandle>,
    /// Открывать устройство вывода. `false` — микширование без звука (CI, тесты).
    pub output: bool,
    /// Сводить весь вывод к моно (настройка проекта `mono_output`).
    pub mono_output: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// Все слоты заняты зацикленными голосами.
    NoFreeVoice,
    /// Очередь команд заполнена; команда не отправлена.
    QueueFull,
    /// Поток `rve-audio` завершён.
    Stopped,
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFreeVoice => write!(f, "все {VOICE_SLOTS} голосов заняты зацикленными клипами"),
            Self::QueueFull => write!(f, "очередь звуковых команд заполнена"),
            Self::Stopped => write!(f, "звуковой поток остановлен"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Событие для владельцев голосов.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioEvent {
    /// Голос закончился, был остановлен или вытеснен новым голосом.
    VoiceEnded(VoiceId),
    /// Звук не сработал: клип не прочитан или не декодирован, устройство не открылось.
    /// Текст готов для показа автору сцены.
    Failed(String),
}

/// Команды от [`AudioHandle`] к потоку `rve-audio`.
#[derive(Debug)]
pub(crate) enum Command {
    Play {
        voice: VoiceId,
        lane: usize,
        path: String,
        request: PlayRequest,
    },
    Stop(VoiceId),
    FadeOut {
        voice: VoiceId,
        seconds: f32,
    },
    StopAll,
    SetPaused(bool),
    SetMonoOutput(bool),
    SetVoiceVolume {
        voice: VoiceId,
        volume: f32,
    },
    SetGroupVolume {
        group: AudioGroup,
        volume: f32,
    },
    SetGroupEffects {
        group: AudioGroup,
        effects: AudioGroupEffects,
    },
    Preload(Vec<String>),
    /// Отвечает, когда поток дошёл до этой команды: все предыдущие уже обработаны.
    Barrier(Sender<()>),
    Shutdown,
}

/// Владение слотами голосов. Слот голоса совпадает с полосой позиции в [`Spatial`].
/// Выделяет и освобождает слоты поток, владеющий голосом; callback таблицу не трогает.
struct Lanes {
    slots: [Option<LaneSlot>; VOICE_SLOTS],
    started: u64,
}

impl Default for Lanes {
    fn default() -> Self {
        Self {
            slots: [None; VOICE_SLOTS],
            started: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LaneSlot {
    voice: VoiceId,
    looping: bool,
    started: u64,
}

impl Lanes {
    /// Возвращает свободный слот. Если их нет, вытесняет самый старый не-looping голос;
    /// его прежний слот возвращается вызывающему (на случай отката через `Lanes::restore`).
    fn allocate(
        &mut self,
        voice: VoiceId,
        looping: bool,
    ) -> Result<(usize, Option<LaneSlot>), AudioError> {
        self.started += 1;
        let slot = LaneSlot {
            voice,
            looping,
            started: self.started,
        };
        if let Some(index) = self.slots.iter().position(Option::is_none) {
            self.slots[index] = Some(slot);
            return Ok((index, None));
        }
        let victim = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, lane)| {
                lane.filter(|lane| !lane.looping)
                    .map(|lane| (index, lane.started))
            })
            .min_by_key(|&(_, started)| started)
            .map(|(index, _)| index)
            .ok_or(AudioError::NoFreeVoice)?;
        let evicted = self.slots[victim].replace(slot);
        Ok((victim, evicted))
    }

    /// Отменяет `Lanes::allocate`, если команда не ушла: слот возвращается прежнему владельцу
    /// или освобождается. Вытесненный голос при этом продолжает владеть своим слотом.
    fn restore(&mut self, lane: usize, previous: Option<LaneSlot>) {
        self.slots[lane] = previous;
    }

    /// Освобождает слот, если его занимает `voice`. Чужой слот не трогает.
    fn release(&mut self, voice: VoiceId) -> bool {
        for slot in &mut self.slots {
            if slot.is_some_and(|lane| lane.voice == voice) {
                *slot = None;
                return true;
            }
        }
        false
    }

    fn lane_of(&self, voice: VoiceId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.is_some_and(|lane| lane.voice == voice))
    }
}

/// Клиентский конец звуковой подсистемы. Клонируется; все методы не блокируют поток
/// `rve-audio`, кроме короткой блокировки таблицы слотов.
#[derive(Clone)]
pub struct AudioHandle {
    commands: Sender<Command>,
    events: Receiver<AudioEvent>,
    lanes: Arc<Mutex<Lanes>>,
    spatial: Arc<Spatial>,
    stopping: Arc<AtomicBool>,
}

impl AudioHandle {
    /// Запускает клип на голосе `voice`. Слот выделяется сразу; при отсутствии свободных слотов
    /// вытесняется самый старый не-looping голос (он обрывается, см. `mixer`).
    pub fn play(&self, voice: VoiceId, path: &str, request: PlayRequest) -> Result<(), AudioError> {
        self.start(voice, path, request, None)
    }

    /// Как [`AudioHandle::play`], но сразу задаёт позицию излучателя: первый блок звучит на месте.
    pub fn play_at(
        &self,
        voice: VoiceId,
        path: &str,
        request: PlayRequest,
        position: [f32; 3],
    ) -> Result<(), AudioError> {
        self.start(voice, path, request, Some(position))
    }

    fn start(
        &self,
        voice: VoiceId,
        path: &str,
        request: PlayRequest,
        position: Option<[f32; 3]>,
    ) -> Result<(), AudioError> {
        let mut lanes = self.locked();
        let (lane, evicted) = lanes.allocate(voice, request.looping)?;
        if let Some(position) = position {
            self.spatial.set_emitter(lane, position);
        }
        let sent = evicted
            .map_or(Ok(()), |evicted| self.send(Command::Stop(evicted.voice)))
            .and_then(|()| {
                self.send(Command::Play {
                    voice,
                    lane,
                    path: path.to_owned(),
                    request,
                })
            });
        if sent.is_err() {
            lanes.restore(lane, evicted);
        }
        sent
    }

    /// Останавливает голос коротким fade-out. Слот остаётся занятым до `VoiceEnded`, поэтому
    /// новый запуск того же источника получит другой слот и не оборвёт затухающий голос.
    /// При ошибке очереди голос продолжает звучать и владеет слотом.
    pub fn stop(&self, voice: VoiceId) -> Result<(), AudioError> {
        self.send(Command::Stop(voice))
    }

    /// Как [`AudioHandle::stop`], но голос затухает `seconds` секунд от текущей громкости.
    /// Значения короче стандартного затухания (около 5 мс) дают обычную остановку.
    pub fn stop_fade(&self, voice: VoiceId, seconds: f32) -> Result<(), AudioError> {
        self.send(Command::FadeOut { voice, seconds })
    }

    pub fn stop_all(&self) -> Result<(), AudioError> {
        self.send(Command::StopAll)
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), AudioError> {
        self.send(Command::SetPaused(paused))
    }

    /// Сводит весь вывод к моно или возвращает стерео. Применяется со следующего блока микшера.
    pub fn set_mono_output(&self, mono: bool) -> Result<(), AudioError> {
        self.send(Command::SetMonoOutput(mono))
    }

    pub fn set_voice_volume(&self, voice: VoiceId, volume: f32) -> Result<(), AudioError> {
        self.send(Command::SetVoiceVolume { voice, volume })
    }

    pub fn set_group_volume(&self, group: AudioGroup, volume: f32) -> Result<(), AudioError> {
        self.send(Command::SetGroupVolume { group, volume })
    }

    /// Задаёт фильтры и реверберацию шины группы; `Master` обрабатывает общую сумму.
    /// Значения проверяет вызывающий (`engine_scene::group_effects_problem`).
    pub fn set_group_effects(
        &self,
        group: AudioGroup,
        effects: AudioGroupEffects,
    ) -> Result<(), AudioError> {
        self.send(Command::SetGroupEffects { group, effects })
    }

    /// Загружает клипы заранее; ошибки приходят событием [`AudioEvent::Failed`]. Клипы
    /// со стримингом предзагружать не нужно: они не попадают в кеш.
    pub fn preload(&self, paths: Vec<String>) -> Result<(), AudioError> {
        self.send(Command::Preload(paths))
    }

    /// Блокирует вызывающего, пока поток `rve-audio` не обработает команды, отправленные раньше.
    /// Нужна тестам и диагностике; в игровом цикле не вызывать.
    pub fn barrier(&self) -> Result<(), AudioError> {
        let (reply, done) = flume::bounded(1);
        self.send(Command::Barrier(reply))?;
        done.recv().map_err(|_| AudioError::Stopped)
    }

    /// Пишет позицию излучателя голоса. Голос без слота (уже завершён) игнорируется.
    pub fn set_emitter(&self, voice: VoiceId, position: [f32; 3]) {
        let lane = self.locked().lane_of(voice);
        if let Some(lane) = lane {
            self.spatial.set_emitter(lane, position);
        }
    }

    /// Пишет позицию, правую и переднюю оси слушателя.
    pub fn set_listener(&self, position: [f32; 3], right: [f32; 3], forward: [f32; 3]) {
        self.spatial.set_listener(position, right, forward);
    }

    /// Забирает накопившиеся события. Вызывать из одного владельца.
    pub fn poll_events(&self) -> Vec<AudioEvent> {
        self.events.try_iter().collect()
    }

    /// Просит поток `rve-audio` завершиться. Флаг проверяется перед каждым ожиданием, поэтому
    /// остановка не зависит от заполненности очереди; `Shutdown` только будит поток. Повтор безвреден.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.commands.try_send(Command::Shutdown);
    }

    fn send(&self, command: Command) -> Result<(), AudioError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => AudioError::QueueFull,
                TrySendError::Disconnected(_) => AudioError::Stopped,
            })
    }

    fn locked(&self) -> MutexGuard<'_, Lanes> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn spatial(&self) -> &Spatial {
        &self.spatial
    }

    #[cfg(test)]
    pub(crate) fn lane_of(&self, voice: VoiceId) -> Option<usize> {
        self.locked().lane_of(voice)
    }

    /// Клиент без потока: команды читает тест через возвращённый приёмник.
    #[cfg(test)]
    pub(crate) fn detached() -> (Self, Receiver<Command>, Sender<AudioEvent>) {
        let (engine, handle) = AudioEngine::new(AudioConfig::default());
        (handle, engine.commands, engine.events_tx)
    }
}

/// Поток `rve-audio`. Создаётся вместе с [`AudioHandle`], запускается через [`AudioEngine::run`].
pub struct AudioEngine {
    config: AudioConfig,
    commands: Receiver<Command>,
    events_tx: Sender<AudioEvent>,
    lanes: Arc<Mutex<Lanes>>,
    spatial: Arc<Spatial>,
    stopping: Arc<AtomicBool>,
}

impl AudioEngine {
    pub fn new(config: AudioConfig) -> (Self, AudioHandle) {
        let (command_tx, commands) = flume::bounded(COMMAND_CAPACITY);
        let (events_tx, events_rx) = flume::unbounded();
        let lanes = Arc::new(Mutex::new(Lanes::default()));
        let spatial = Arc::new(Spatial::new());
        let stopping = Arc::new(AtomicBool::new(false));
        let handle = AudioHandle {
            commands: command_tx,
            events: events_rx,
            lanes: Arc::clone(&lanes),
            spatial: Arc::clone(&spatial),
            stopping: Arc::clone(&stopping),
        };
        // Очередь пуста, поэтому отправка стартового режима не может упереться в заполненную очередь.
        if config.mono_output {
            let _ = handle.set_mono_output(true);
        }
        let engine = Self {
            config,
            commands,
            events_tx,
            lanes,
            spatial,
            stopping,
        };
        (engine, handle)
    }

    /// Блокирует поток до [`AudioHandle::shutdown`] или до освобождения всех хендлов.
    /// Ошибка открытия и потеря устройства не прерывают работу: звук выключается с предупреждением
    /// и открывается заново по расписанию [`Reconnect`].
    pub fn run(self) {
        let AudioEngine {
            config,
            commands,
            events_tx,
            lanes,
            spatial,
            stopping,
        } = self;
        let (decode_tx, decode_rx) = flume::unbounded::<DecodeJob>();
        let (decoded_tx, decoded_rx) = flume::unbounded::<Decoded>();
        let keepalive_decoded = decoded_tx.clone();
        // Отмена нужна только при выходе из `run`: тогда очередь декодера не дочитывается.
        let cancel_decode = Arc::new(AtomicBool::new(false));
        let decoder_cancel = Arc::clone(&cancel_decode);
        let decoder = thread::Builder::new()
            .name("rve-audio-decode".into())
            .spawn(move || decode_loop(decode_rx, decoded_tx, &decoder_cancel));
        if let Err(error) = &decoder {
            log::warn!("декодер звука не запущен: {error}; клипы не загрузятся");
        }

        let (stream_tx, stream_jobs) = flume::unbounded::<StreamJob>();
        let (stream_events_tx, stream_events) = flume::unbounded::<StreamEvent>();
        let keepalive_stream_events = stream_events_tx.clone();
        let stream_cancel = Arc::new(AtomicBool::new(false));
        let streamer_cancel = Arc::clone(&stream_cancel);
        let open_stream = stream_source(config.resources.clone());
        let streamer = thread::Builder::new()
            .name("rve-audio-stream".into())
            .spawn(move || {
                stream_loop(stream_jobs, stream_events_tx, open_stream, &streamer_cancel)
            });
        if let Err(error) = &streamer {
            log::warn!("поток стриминга звука не запущен: {error}; стрим-клипы не зазвучат");
        }

        let read = read_source(config.resources);
        // Открытие устройства передаётся замыканием: тесты подставляют своё.
        let open: Option<OpenOutput<output::Output>> = config.output.then(|| {
            let spatial = Arc::clone(&spatial);
            Box::new(move |channels: Channels, signals: Signals| {
                output::open_device(channels, Arc::clone(&spatial), signals)
            }) as OpenOutput<output::Output>
        });

        let mut core = Core {
            lanes,
            read,
            loading: HashMap::new(),
            decoding: HashSet::new(),
            cache: HashMap::new(),
            cache_bytes: 0,
            failed: HashSet::new(),
            pending: Vec::new(),
            mixer: None,
            outbox: VecDeque::new(),
            in_mixer: HashSet::new(),
            settings: MixerSettings::default(),
            events: events_tx.clone(),
            decode_tx,
            stream_tx: streamer.is_ok().then_some(stream_tx),
            streams: Vec::new(),
        };
        let mut device = Device::new(open, events_tx, Instant::now());
        loop {
            if stopping.load(Ordering::Acquire) {
                break;
            }
            // Устройство присылает завершённые голоса через кольцо: опрашиваем его при каждом
            // пробуждении, а пробуждаемся не реже READ_POLL, пока есть микшер или чтения.
            // Ожидание не длиннее времени до следующей попытки открытия.
            let mut timeout = if core.mixer.is_some() || !core.loading.is_empty() {
                READ_POLL
            } else {
                IDLE_POLL
            };
            if let Some(retry) = device.retry_in(Instant::now()) {
                timeout = timeout.min(retry);
            }
            let incoming = Selector::new()
                .recv(&commands, Incoming::Command)
                .recv(&decoded_rx, Incoming::Decoded)
                .recv(&stream_events, Incoming::Stream)
                .wait_timeout(timeout);
            // Потеря устройства обрабатывается до команд этого пробуждения: они попадут уже в новый микшер.
            device.poll(Instant::now(), &mut core);
            match incoming {
                Ok(Incoming::Command(Ok(command))) => {
                    if !core.command(command) {
                        break;
                    }
                }
                Ok(Incoming::Command(Err(_))) => break,
                Ok(Incoming::Decoded(Ok(decoded))) => core.decoded(decoded),
                Ok(Incoming::Stream(Ok(event))) => core.stream_event(event),
                // `keepalive_*` живут до конца `run`, поэтому отключения не бывает.
                Ok(Incoming::Decoded(Err(_))) | Ok(Incoming::Stream(Err(_))) | Err(_) => {}
            }
            device.drain(&mut core);
            core.flush_mixer();
            core.poll_reads();
        }

        cancel_decode.store(true, Ordering::Relaxed);
        // Сначала останавливаем устройство: голоса микшера держат `Arc<Clip>`, и их последняя
        // ссылка не должна освободиться в callback. Кеш `core` живёт дольше потока вывода.
        stream_cancel.store(true, Ordering::Relaxed);
        drop(device);
        drop(core);
        drop(keepalive_decoded);
        drop(keepalive_stream_events);
        if let Ok(decoder) = decoder {
            let _ = decoder.join();
        }
        if let Ok(streamer) = streamer {
            let _ = streamer.join();
        }
    }
}

enum Incoming {
    Command(Result<Command, flume::RecvError>),
    Decoded(Result<Decoded, flume::RecvError>),
    Stream(Result<StreamEvent, flume::RecvError>),
}

type ReadPoll = Box<dyn FnMut() -> Option<Result<Vec<u8>, String>>>;
type ReadSource = Box<dyn FnMut(&str) -> Result<ReadPoll, String>>;

struct DecodeJob {
    path: String,
    bytes: Vec<u8>,
}

struct Decoded {
    path: String,
    result: Result<Clip, DecodeError>,
}

/// Воспроизведение, которое ждёт загрузки клипа.
struct Pending {
    voice: VoiceId,
    lane: usize,
    path: String,
    request: PlayRequest,
}

/// Состояние потока `rve-audio`. Без устройства и потоков, поэтому тестируется напрямую.
struct Core {
    lanes: Arc<Mutex<Lanes>>,
    read: ReadSource,
    loading: HashMap<String, ReadPoll>,
    decoding: HashSet<String>,
    cache: HashMap<String, Arc<Clip>>,
    cache_bytes: usize,
    failed: HashSet<String>,
    pending: Vec<Pending>,
    mixer: Option<Producer<MixerCommand>>,
    /// Команды микшера, которые не влезли в кольцо; уходят в порядке поступления.
    outbox: VecDeque<MixerCommand>,
    /// Голоса, которые отданы микшеру (`Play`/`PlayStream` в `outbox` или в кольце) и ещё не
    /// завершились. При потере устройства завершаются все они, по одному разу.
    in_mixer: HashSet<VoiceId>,
    /// Настройки микшера, заданные командами: новый микшер получает их первыми.
    settings: MixerSettings,
    events: Sender<AudioEvent>,
    decode_tx: Sender<DecodeJob>,
    /// Задания потоку стриминга; `None`, если поток не запустился.
    stream_tx: Option<Sender<StreamJob>>,
    /// Стрим-голоса, которые ждут заполнения кольца.
    streams: Vec<Pending>,
}

/// Настройки микшера, которые переживают смену устройства.
#[derive(Default)]
struct MixerSettings {
    paused: bool,
    mono: bool,
    volumes: Vec<(AudioGroup, f32)>,
    effects: Vec<(AudioGroup, AudioGroupEffects)>,
}

impl MixerSettings {
    fn commands(&self) -> Vec<MixerCommand> {
        let mut commands = vec![
            MixerCommand::SetPaused(self.paused),
            MixerCommand::SetMonoOutput(self.mono),
        ];
        commands.extend(
            self.volumes
                .iter()
                .map(|&(group, volume)| MixerCommand::SetGroupVolume { group, volume }),
        );
        commands.extend(
            self.effects
                .iter()
                .map(|&(group, effects)| MixerCommand::SetGroupEffects { group, effects }),
        );
        commands
    }
}

/// Запоминает последнее значение группы; в списке не больше одной записи на группу.
fn remember<T>(entries: &mut Vec<(AudioGroup, T)>, group: AudioGroup, value: T) {
    match entries.iter_mut().find(|(entry, _)| *entry == group) {
        Some(entry) => entry.1 = value,
        None => entries.push((group, value)),
    }
}

impl Core {
    /// Возвращает `false`, если поток нужно остановить.
    fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Play {
                voice,
                lane,
                path,
                request,
            } => self.play(voice, lane, path, request),
            Command::Stop(voice) => {
                self.cancel_pending(|pending| pending.voice == voice);
                self.send_mixer(MixerCommand::Stop { voice });
            }
            Command::FadeOut { voice, seconds } => {
                self.cancel_pending(|pending| pending.voice == voice);
                self.send_mixer(MixerCommand::FadeOut { voice, seconds });
            }
            Command::StopAll => {
                self.cancel_pending(|_| true);
                self.send_mixer(MixerCommand::StopAll);
            }
            Command::SetPaused(paused) => {
                self.settings.paused = paused;
                self.send_mixer(MixerCommand::SetPaused(paused));
            }
            Command::SetMonoOutput(mono) => {
                self.settings.mono = mono;
                self.send_mixer(MixerCommand::SetMonoOutput(mono));
            }
            Command::SetVoiceVolume { voice, volume } => {
                for pending in self
                    .pending
                    .iter_mut()
                    .chain(self.streams.iter_mut())
                    .filter(|pending| pending.voice == voice)
                {
                    pending.request.volume = volume;
                }
                self.send_mixer(MixerCommand::SetVoiceVolume { voice, volume });
            }
            Command::SetGroupVolume { group, volume } => {
                remember(&mut self.settings.volumes, group, volume);
                self.send_mixer(MixerCommand::SetGroupVolume { group, volume });
            }
            Command::SetGroupEffects { group, effects } => {
                remember(&mut self.settings.effects, group, effects);
                self.send_mixer(MixerCommand::SetGroupEffects { group, effects });
            }
            Command::Preload(paths) => {
                for path in paths {
                    self.ensure_loaded(&path);
                }
            }
            Command::Barrier(reply) => {
                let _ = reply.send(());
            }
            Command::Shutdown => return false,
        }
        true
    }

    fn play(&mut self, voice: VoiceId, lane: usize, path: String, request: PlayRequest) {
        if request.stream {
            return self.play_stream(voice, lane, path, request);
        }
        if let Some(clip) = self.cache.get(&path).cloned() {
            return self.start_voice(voice, lane, clip, request);
        }
        if self.failed.contains(&path) {
            return self.voice_ended(voice);
        }
        self.pending.push(Pending {
            voice,
            lane,
            path: path.clone(),
            request,
        });
        self.ensure_loaded(&path);
    }

    /// Просит поток стриминга открыть клип; голос уйдёт микшеру, когда кольцо заполнится.
    fn play_stream(&mut self, voice: VoiceId, lane: usize, path: String, request: PlayRequest) {
        if self.mixer.is_none() || self.failed.contains(&path) {
            return self.voice_ended(voice);
        }
        let job = StreamJob {
            voice,
            path: path.clone(),
            looping: request.looping,
        };
        let sent = self
            .stream_tx
            .as_ref()
            .is_some_and(|stream| stream.send(job).is_ok());
        if !sent {
            self.voice_ended(voice);
            return self.fail(&path, "поток стриминга не запущен");
        }
        self.streams.push(Pending {
            voice,
            lane,
            path,
            request,
        });
    }

    /// Ответ потока стриминга: готовое кольцо уходит микшеру, ошибка завершает голоса клипа.
    fn stream_event(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Ready { voice, reader } => {
                let Some(index) = self
                    .streams
                    .iter()
                    .position(|pending| pending.voice == voice)
                else {
                    // Голос остановили, пока стрим открывался: кольцо освобождается здесь.
                    return;
                };
                let pending = self.streams.remove(index);
                if self.mixer.is_none() {
                    return self.voice_ended(voice);
                }
                self.in_mixer.insert(voice);
                self.send_mixer(MixerCommand::PlayStream {
                    voice,
                    lane: pending.lane,
                    stream: reader,
                    request: pending.request,
                });
            }
            StreamEvent::OpenFailed {
                voice,
                path,
                message,
            } => {
                if self.failed.contains(&path) {
                    self.cancel_pending(|pending| pending.voice == voice);
                } else {
                    self.fail(&path, message);
                }
            }
            // Сбой посреди воспроизведения может быть разовым (I/O): голос доигрывает кольцо,
            // а клип не помечается ошибочным, чтобы следующие запуски и ожидающие голоса жили.
            StreamEvent::Broken { path, message } => {
                let message =
                    format!("аудиострим {path}: {message}; голос доиграет буфер и остановится");
                log::warn!("{message}");
                let _ = self.events.send(AudioEvent::Failed(message));
            }
        }
    }

    /// Запускает чтение клипа, если его нет в кеше и он не загружается и не декодируется.
    fn ensure_loaded(&mut self, path: &str) {
        if self.cache.contains_key(path)
            || self.failed.contains(path)
            || self.loading.contains_key(path)
            || self.decoding.contains(path)
        {
            return;
        }
        match (self.read)(path) {
            Ok(poll) => {
                self.loading.insert(path.to_owned(), poll);
            }
            Err(message) => self.fail(path, message),
        }
    }

    fn poll_reads(&mut self) {
        let mut finished = Vec::new();
        for (path, poll) in &mut self.loading {
            if let Some(result) = poll() {
                finished.push((path.clone(), result));
            }
        }
        for (path, result) in finished {
            self.loading.remove(&path);
            match result {
                Ok(bytes) => {
                    self.decoding.insert(path.clone());
                    if self
                        .decode_tx
                        .send(DecodeJob {
                            path: path.clone(),
                            bytes,
                        })
                        .is_err()
                    {
                        self.decoding.remove(&path);
                        self.fail(&path, "декодер остановлен");
                    }
                }
                Err(message) => self.fail(&path, message),
            }
        }
    }

    fn decoded(&mut self, decoded: Decoded) {
        let Decoded { path, result } = decoded;
        self.decoding.remove(&path);
        let clip = match result {
            Ok(clip) => clip,
            Err(error) => return self.fail(&path, error),
        };
        if self.cache_bytes + clip.bytes() > CACHE_LIMIT_BYTES {
            return self.fail(&path, "бюджет декодированных клипов исчерпан");
        }
        self.cache_bytes += clip.bytes();
        let clip = Arc::new(clip);
        self.cache.insert(path.clone(), Arc::clone(&clip));
        let (ready, waiting): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| pending.path == path);
        self.pending = waiting;
        for pending in ready {
            self.start_voice(
                pending.voice,
                pending.lane,
                Arc::clone(&clip),
                pending.request,
            );
        }
    }

    /// Отдаёт голос микшеру. Без устройства голос сразу завершается: он никогда не будет звучать.
    fn start_voice(&mut self, voice: VoiceId, lane: usize, clip: Arc<Clip>, request: PlayRequest) {
        if self.mixer.is_none() {
            return self.voice_ended(voice);
        }
        self.in_mixer.insert(voice);
        self.send_mixer(MixerCommand::Play {
            voice,
            lane,
            clip,
            request,
        });
    }

    /// Сообщает об ошибке клипа один раз и завершает его ожидающие голоса.
    fn fail(&mut self, path: &str, message: impl fmt::Display) {
        let message = format!("аудиоклип {path}: {message}; звук не будет воспроизведён");
        log::warn!("{message}");
        let _ = self.events.send(AudioEvent::Failed(message));
        self.failed.insert(path.to_owned());
        self.loading.remove(path);
        self.decoding.remove(path);
        self.cancel_pending(|pending| pending.path == path);
    }

    /// Убирает ожидающие голоса, которые подходят под `matches`, и завершает их: микшер их не видел.
    /// Касается и клипов, ждущих загрузки, и стримов, ждущих заполнения кольца.
    fn cancel_pending(&mut self, matches: impl Fn(&Pending) -> bool) {
        let mut cancelled = Vec::new();
        for list in [&mut self.pending, &mut self.streams] {
            let (removed, waiting): (Vec<Pending>, Vec<Pending>) = std::mem::take(list)
                .into_iter()
                .partition(|pending| matches(pending));
            *list = waiting;
            cancelled.extend(removed);
        }
        for pending in cancelled {
            self.voice_ended(pending.voice);
        }
    }

    /// Освобождает слот голоса и сообщает о завершении. Вызывается один раз на голос: из микшера,
    /// при отмене ожидающего голоса или когда голос не может начать звучать.
    fn voice_ended(&mut self, voice: VoiceId) {
        self.release(voice);
        let _ = self.events.send(AudioEvent::VoiceEnded(voice));
    }

    /// Завершение голоса из кольца микшера. Голос, которого нет в учёте, уже завершён (например,
    /// при потере устройства): повтор игнорируется, поэтому `VoiceEnded` уходит ровно один раз.
    fn mixer_ended(&mut self, voice: VoiceId) {
        if self.in_mixer.remove(&voice) {
            self.voice_ended(voice);
        }
    }

    /// Устройство потеряно: микшер и его очередь больше не действуют. Голоса, которые микшер
    /// не вернул, завершаются здесь. Ожидающие загрузки и стрима голоса сохраняются.
    fn device_lost(&mut self) {
        self.mixer = None;
        self.outbox.clear();
        for voice in std::mem::take(&mut self.in_mixer) {
            self.voice_ended(voice);
        }
    }

    /// Новое устройство: сначала возвращаются настройки микшера, затем продолжается очередь.
    fn device_opened(&mut self, mixer: Producer<MixerCommand>) {
        self.mixer = Some(mixer);
        for command in self.settings.commands() {
            self.send_mixer(command);
        }
    }

    fn release(&mut self, voice: VoiceId) {
        self.lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(voice);
    }

    fn send_mixer(&mut self, command: MixerCommand) {
        if self.mixer.is_none() {
            return;
        }
        self.outbox.push_back(command);
        self.flush_mixer();
    }

    /// Переносит команды из `outbox` в кольцо, пока в нём есть место.
    fn flush_mixer(&mut self) {
        let Some(mixer) = self.mixer.as_mut() else {
            return;
        };
        while let Some(command) = self.outbox.pop_front() {
            if let Err(PushError::Full(command)) = mixer.push(command) {
                self.outbox.push_front(command);
                break;
            }
        }
    }
}

/// Открытие выходного устройства: получает концы колец и сигналы callback-а, возвращает владельца потока.
type OpenOutput<O> = Box<dyn FnMut(Channels, Signals) -> Result<O, String>>;

/// Выходное устройство: открытие, потеря и повторные попытки. Открытие передаётся замыканием,
/// поэтому тесты подменяют устройство, а время приходит аргументом.
struct Device<O> {
    /// `None`, если вывод отключён (`output: false`): попыток нет.
    open: Option<OpenOutput<O>>,
    /// Открытое устройство. Дроп `output` останавливает callback.
    active: Option<Active<O>>,
    reconnect: Reconnect,
    /// Эпизод потери или отказа уже сообщён `Failed`; сбрасывается, когда открытый поток отработал.
    reported: bool,
    events: Sender<AudioEvent>,
}

struct Active<O> {
    output: O,
    signals: Signals,
    ended: Consumer<VoiceId>,
    retired: Consumer<StreamReader>,
}

impl<O> Device<O> {
    fn new(open: Option<OpenOutput<O>>, events: Sender<AudioEvent>, now: Instant) -> Self {
        let mut reconnect = Reconnect::default();
        if open.is_some() {
            reconnect.schedule(now);
        }
        Self {
            open,
            active: None,
            reconnect,
            reported: false,
            events,
        }
    }

    /// Сколько ждать до следующей попытки открытия; `None`, если устройство открыто или попытка не запланирована.
    fn retry_in(&self, now: Instant) -> Option<Duration> {
        if self.active.is_some() {
            return None;
        }
        self.reconnect.wait(now)
    }

    /// Проверяет сигналы callback-а и открывает устройство, когда пришло время попытки.
    fn poll(&mut self, now: Instant, core: &mut Core) {
        let (lost, alive) = self.active.as_ref().map_or((false, false), |active| {
            (
                active.signals.lost.load(Ordering::Acquire),
                active.signals.alive.load(Ordering::Acquire),
            )
        });
        if lost {
            self.lose(now, core);
        } else if alive {
            self.reconnect.alive();
            self.reported = false;
        }
        if self.active.is_none() && self.reconnect.due(now) {
            self.attempt(now, core);
        }
    }

    /// Переносит завершённые голоса и освобождает кольца стрим-голосов. Кольцо `retired`
    /// освобождается здесь, а не в callback.
    fn drain(&mut self, core: &mut Core) {
        if let Some(active) = self.active.as_mut() {
            drain_rings(&mut active.ended, &mut active.retired, core);
        }
    }

    fn lose(&mut self, now: Instant, core: &mut Core) {
        let Some(Active {
            output,
            mut ended,
            mut retired,
            ..
        }) = self.active.take()
        else {
            return;
        };
        // Сначала останавливаем поток: после этого callback не пишет в кольца.
        drop(output);
        drain_rings(&mut ended, &mut retired, core);
        core.device_lost();
        self.reconnect.schedule(now);
        self.report("устройство вывода потеряно; звук вернётся автоматически".to_owned());
    }

    fn attempt(&mut self, now: Instant, core: &mut Core) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        let (mixer_tx, commands) = RingBuffer::new(MIXER_CAPACITY);
        let (ended_tx, ended) = RingBuffer::new(mixer::ENDED_CAPACITY);
        let (retired_tx, retired) = RingBuffer::new(RETIRED_CAPACITY);
        let signals = Signals::default();
        let channels = Channels {
            commands,
            ended: ended_tx,
            retired: retired_tx,
        };
        match open(channels, signals.clone()) {
            Ok(output) => {
                self.reconnect.opened();
                self.active = Some(Active {
                    output,
                    signals,
                    ended,
                    retired,
                });
                core.device_opened(mixer_tx);
                log::info!("звук: устройство вывода открыто");
            }
            Err(error) => {
                self.reconnect.failed(now);
                self.report(format!(
                    "звук отключён: {error}; попытки открыть устройство продолжатся автоматически"
                ));
            }
        }
    }

    /// Сообщает об эпизоде один раз; повторные неудачи попадают только в журнал.
    fn report(&mut self, message: String) {
        if self.reported {
            log::debug!("{message}");
            return;
        }
        self.reported = true;
        log::warn!("{message}");
        let _ = self.events.send(AudioEvent::Failed(message));
    }
}

fn drain_rings(
    ended: &mut Consumer<VoiceId>,
    retired: &mut Consumer<StreamReader>,
    core: &mut Core,
) {
    while let Ok(voice) = ended.pop() {
        core.mixer_ended(voice);
    }
    while let Ok(reader) = retired.pop() {
        drop(reader);
    }
}

fn decode_loop(jobs: Receiver<DecodeJob>, results: Sender<Decoded>, cancel: &AtomicBool) {
    for DecodeJob { path, bytes } in jobs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let extension = Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        let result = decode(&extension, &bytes, cancel);
        if matches!(result, Err(DecodeError::Cancelled)) {
            break;
        }
        if results.send(Decoded { path, result }).is_err() {
            break;
        }
    }
}

/// Диапазоны файла ресурсов для потока стриминга; ожидание блокирует только этот поток.
struct ResourceBytes {
    resources: ResourceHandle,
    path: String,
    len: u64,
}

impl ByteSource for ResourceBytes {
    fn size(&self) -> u64 {
        self.len
    }

    fn read(&mut self, range: Range<u64>) -> Result<Vec<u8>, String> {
        self.resources
            .read_range(&self.path, range)
            .and_then(|request| request.wait())
            .map_err(|error| error.to_string())
    }
}

fn stream_source(resources: Option<ResourceHandle>) -> OpenSource {
    match resources {
        Some(resources) => Box::new(move |path: &str| {
            let stat = resources
                .stat(path)
                .and_then(|request| request.wait())
                .map_err(|error| error.to_string())?;
            Ok(Box::new(ResourceBytes {
                resources: resources.clone(),
                path: path.to_owned(),
                len: stat.size,
            }) as Box<dyn ByteSource>)
        }),
        None => Box::new(|_path: &str| Err("ресурсы проекта не подключены".to_owned())),
    }
}

fn read_source(resources: Option<ResourceHandle>) -> ReadSource {
    match resources {
        Some(resources) => Box::new(move |path: &str| {
            let mut stat = resources.stat(path).map_err(|error| error.to_string())?;
            let size = move || match stat.try_receive() {
                Ok(None) => None,
                Ok(Some(stat)) => Some(Ok(stat.size)),
                Err(error) => Some(Err(error.to_string())),
            };
            let reader = resources.clone();
            let path = path.to_owned();
            let read = move || {
                reader
                    .read_all(&path)
                    .map(poll_request)
                    .map_err(|error| error.to_string())
            };
            Ok(guarded_read(size, read))
        }),
        None => Box::new(|_path: &str| Err("ресурсы проекта не подключены".to_owned())),
    }
}

/// Чтение клипа в два шага: сначала размер, затем байты. Файл больше [`MAX_CLIP_FILE_BYTES`]
/// отклоняется до чтения, чтобы не держать в памяти заведомо отвергаемые байты.
fn guarded_read(
    mut size: impl FnMut() -> Option<Result<u64, String>> + 'static,
    start_read: impl FnOnce() -> Result<ReadPoll, String> + 'static,
) -> ReadPoll {
    let mut start_read = Some(start_read);
    let mut read: Option<ReadPoll> = None;
    Box::new(move || {
        if let Some(poll) = read.as_mut() {
            return poll();
        }
        let size = match size() {
            None => return None,
            Some(Err(message)) => return Some(Err(message)),
            Some(Ok(size)) => size,
        };
        if size > MAX_CLIP_FILE_BYTES {
            return Some(Err(format!(
                "файл {size} байт больше лимита {MAX_CLIP_FILE_BYTES} байт"
            )));
        }
        // Чтение стартует один раз: после этого ветка `read` выше обрабатывает все опросы.
        let start = start_read.take()?;
        match start() {
            Ok(poll) => {
                read = Some(poll);
                None
            }
            Err(message) => Some(Err(message)),
        }
    })
}

fn poll_request(mut request: ResourceRequest<Vec<u8>>) -> ReadPoll {
    Box::new(move || match request.try_receive() {
        Ok(None) => None,
        Ok(Some(bytes)) => Some(Ok(bytes)),
        Err(error) => Some(Err(error.to_string())),
    })
}

#[cfg(test)]
mod tests;

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
