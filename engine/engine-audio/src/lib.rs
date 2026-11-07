//! Игровой звук: декодирование клипов, микширование голосов и вывод на устройство.
//!
//! Потоки:
//! - поток `rve-audio` (здесь, [`AudioEngine::run`]) владеет кешем клипов, очередью ожидающих
//!   воспроизведений и выходным потоком `cpal`; он же освобождает слоты голосов;
//! - `rve-audio-decode` декодирует прочитанные байты, чтобы не блокировать цикл;
//! - audio callback работает только с [`mixer::Mixer`] и не выделяет память.
//!
//! Владельцы голосов (игровой поток через [`AudioBridge`], редактор) вызывают [`AudioHandle`].
//! Позиции излучателей и слушателя пишутся в seqlock-таблицу, а не в очередь команд.

pub mod bridge;
pub mod decode;
pub mod mixer;
pub mod output;

pub use bridge::AudioBridge;
pub use decode::{Clip, DecodeError};
pub use mixer::{Distances, PlayRequest};

use decode::{MAX_CLIP_FILE_BYTES, decode};
use engine_resources::{ResourceHandle, ResourceRequest};
use engine_scene::{AudioGroup, VoiceId};
use flume::{Receiver, Selector, Sender, TrySendError};
use mixer::{MixerCommand, Spatial, VOICE_SLOTS};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

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

    pub fn stop_all(&self) -> Result<(), AudioError> {
        self.send(Command::StopAll)
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), AudioError> {
        self.send(Command::SetPaused(paused))
    }

    pub fn set_voice_volume(&self, voice: VoiceId, volume: f32) -> Result<(), AudioError> {
        self.send(Command::SetVoiceVolume { voice, volume })
    }

    pub fn set_group_volume(&self, group: AudioGroup, volume: f32) -> Result<(), AudioError> {
        self.send(Command::SetGroupVolume { group, volume })
    }

    /// Загружает клипы заранее; ошибки приходят событием [`AudioEvent::Failed`].
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

    /// Пишет позицию и правую ось слушателя.
    pub fn set_listener(&self, position: [f32; 3], right: [f32; 3]) {
        self.spatial.set_listener(position, right);
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
    ended_tx: Producer<VoiceId>,
    ended_rx: Consumer<VoiceId>,
    mixer_tx: Producer<MixerCommand>,
    mixer_rx: Consumer<MixerCommand>,
    lanes: Arc<Mutex<Lanes>>,
    spatial: Arc<Spatial>,
    stopping: Arc<AtomicBool>,
}

impl AudioEngine {
    pub fn new(config: AudioConfig) -> (Self, AudioHandle) {
        let (command_tx, commands) = flume::bounded(COMMAND_CAPACITY);
        let (events_tx, events_rx) = flume::unbounded();
        let (ended_tx, ended_rx) = RingBuffer::new(mixer::ENDED_CAPACITY);
        let (mixer_tx, mixer_rx) = RingBuffer::new(MIXER_CAPACITY);
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
        let engine = Self {
            config,
            commands,
            events_tx,
            ended_tx,
            ended_rx,
            mixer_tx,
            mixer_rx,
            lanes,
            spatial,
            stopping,
        };
        (engine, handle)
    }

    /// Блокирует поток до [`AudioHandle::shutdown`] или до освобождения всех хендлов.
    /// Ошибка открытия устройства не прерывает работу: звук выключается с предупреждением.
    pub fn run(self) {
        let AudioEngine {
            config,
            commands,
            events_tx,
            ended_tx,
            mut ended_rx,
            mixer_tx,
            mixer_rx,
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

        let read = read_source(config.resources);
        let (output, mixer) = if config.output {
            match output::open(mixer_rx, ended_tx, Arc::clone(&spatial)) {
                Ok(output) => (Some(output), Some(mixer_tx)),
                Err(error) => {
                    let message = format!("звук отключён: {error}; игра продолжает работу без звука");
                    log::warn!("{message}");
                    let _ = events_tx.send(AudioEvent::Failed(message));
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let mut core = Core {
            lanes,
            read,
            loading: HashMap::new(),
            decoding: HashSet::new(),
            cache: HashMap::new(),
            cache_bytes: 0,
            failed: HashSet::new(),
            pending: Vec::new(),
            mixer,
            outbox: VecDeque::new(),
            events: events_tx,
            decode_tx,
        };
        loop {
            if stopping.load(Ordering::Acquire) {
                break;
            }
            // Устройство присылает завершённые голоса через кольцо: опрашиваем его при каждом
            // пробуждении, а пробуждаемся не реже READ_POLL, пока есть микшер или чтения.
            let incoming = Selector::new()
                .recv(&commands, Incoming::Command)
                .recv(&decoded_rx, Incoming::Decoded)
                .wait_timeout(if core.mixer.is_some() || !core.loading.is_empty() {
                    READ_POLL
                } else {
                    IDLE_POLL
                });
            match incoming {
                Ok(Incoming::Command(Ok(command))) => {
                    if !core.command(command) {
                        break;
                    }
                }
                Ok(Incoming::Command(Err(_))) => break,
                Ok(Incoming::Decoded(Ok(decoded))) => core.decoded(decoded),
                // `keepalive_decoded` живёт до конца `run`, поэтому отключения не бывает.
                Ok(Incoming::Decoded(Err(_))) | Err(_) => {}
            }
            while let Ok(voice) = ended_rx.pop() {
                core.voice_ended(voice);
            }
            core.flush_mixer();
            core.poll_reads();
        }

        cancel_decode.store(true, Ordering::Relaxed);
        // Сначала останавливаем устройство: голоса микшера держат `Arc<Clip>`, и их последняя
        // ссылка не должна освободиться в callback. Кеш `core` живёт дольше потока вывода.
        drop(output);
        drop(core);
        drop(keepalive_decoded);
        if let Ok(decoder) = decoder {
            let _ = decoder.join();
        }
    }
}

enum Incoming {
    Command(Result<Command, flume::RecvError>),
    Decoded(Result<Decoded, flume::RecvError>),
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
    events: Sender<AudioEvent>,
    decode_tx: Sender<DecodeJob>,
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
            Command::StopAll => {
                self.cancel_pending(|_| true);
                self.send_mixer(MixerCommand::StopAll);
            }
            Command::SetPaused(paused) => self.send_mixer(MixerCommand::SetPaused(paused)),
            Command::SetVoiceVolume { voice, volume } => {
                for pending in self
                    .pending
                    .iter_mut()
                    .filter(|pending| pending.voice == voice)
                {
                    pending.request.volume = volume;
                }
                self.send_mixer(MixerCommand::SetVoiceVolume { voice, volume });
            }
            Command::SetGroupVolume { group, volume } => {
                self.send_mixer(MixerCommand::SetGroupVolume { group, volume });
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
            self.start_voice(pending.voice, pending.lane, Arc::clone(&clip), pending.request);
        }
    }

    /// Отдаёт голос микшеру. Без устройства голос сразу завершается: он никогда не будет звучать.
    fn start_voice(&mut self, voice: VoiceId, lane: usize, clip: Arc<Clip>, request: PlayRequest) {
        if self.mixer.is_none() {
            return self.voice_ended(voice);
        }
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
    fn cancel_pending(&mut self, matches: impl Fn(&Pending) -> bool) {
        let (cancelled, waiting): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| matches(pending));
        self.pending = waiting;
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

fn read_source(resources: Option<ResourceHandle>) -> ReadSource {
    match resources {
        Some(resources) => Box::new(move |path: &str| {
            let mut stat = resources
                .stat(path)
                .map_err(|error| error.to_string())?;
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
