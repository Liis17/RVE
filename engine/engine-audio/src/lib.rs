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

use decode::decode;
use engine_resources::{ResourceHandle, ResourceRequest};
use engine_scene::{AudioGroup, VoiceId};
use flume::{Receiver, Selector, Sender, TrySendError};
use mixer::{MixerCommand, Spatial, VOICE_SLOTS};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// Бюджет декодированных клипов в кеше (512 МБ).
const CACHE_LIMIT_BYTES: usize = 512 * 1024 * 1024;
/// Ожидание команд, пока есть незавершённые чтения.
const READ_POLL: Duration = Duration::from_millis(10);
/// Ожидание команд без чтений.
const IDLE_POLL: Duration = Duration::from_millis(500);
/// Сколько ждать места в очереди микшера, прежде чем отказаться от команды.
const MIXER_SEND_TIMEOUT: Duration = Duration::from_millis(20);
const COMMAND_CAPACITY: usize = 1024;
const MIXER_CAPACITY: usize = 1024;
const ENDED_CAPACITY: usize = 256;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEvent {
    /// Голос закончился, был остановлен или вытеснен новым голосом.
    VoiceEnded(VoiceId),
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

#[derive(Clone, Copy)]
struct LaneSlot {
    voice: VoiceId,
    looping: bool,
    started: u64,
}

impl Lanes {
    /// Возвращает свободный слот. Если их нет, вытесняет самый старый не-looping голос;
    /// вытесненный голос возвращается вызывающему для остановки.
    fn allocate(
        &mut self,
        voice: VoiceId,
        looping: bool,
    ) -> Result<(usize, Option<VoiceId>), AudioError> {
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
        let evicted = self.slots[victim].replace(slot).map(|lane| lane.voice);
        Ok((victim, evicted))
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

    fn clear(&mut self) {
        self.slots = [None; VOICE_SLOTS];
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
}

impl AudioHandle {
    /// Запускает клип на голосе `voice`. Слот выделяется сразу; при отсутствии свободных слотов
    /// вытесняется самый старый не-looping голос.
    pub fn play(&self, voice: VoiceId, path: &str, request: PlayRequest) -> Result<(), AudioError> {
        let (lane, evicted) = self.locked().allocate(voice, request.looping)?;
        let sent = evicted
            .map_or(Ok(()), |evicted| self.send(Command::Stop(evicted)))
            .and_then(|()| {
                self.send(Command::Play {
                    voice,
                    lane,
                    path: path.to_owned(),
                    request,
                })
            });
        if sent.is_err() {
            self.locked().release(voice);
        }
        sent
    }

    /// Останавливает голос с коротким fade-out; слот освобождается сразу.
    pub fn stop(&self, voice: VoiceId) -> Result<(), AudioError> {
        self.locked().release(voice);
        self.send(Command::Stop(voice))
    }

    pub fn stop_all(&self) -> Result<(), AudioError> {
        self.locked().clear();
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

    /// Загружает клипы заранее; ошибки появятся в логе при попытке их воспроизвести.
    pub fn preload(&self, paths: Vec<String>) -> Result<(), AudioError> {
        self.send(Command::Preload(paths))
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

    /// Просит поток `rve-audio` завершиться. Повторный вызов безвреден.
    pub fn shutdown(&self) {
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
    ended_tx: Sender<VoiceId>,
    ended_rx: Receiver<VoiceId>,
    mixer_tx: Sender<MixerCommand>,
    mixer_rx: Receiver<MixerCommand>,
    lanes: Arc<Mutex<Lanes>>,
    spatial: Arc<Spatial>,
}

impl AudioEngine {
    pub fn new(config: AudioConfig) -> (Self, AudioHandle) {
        let (command_tx, commands) = flume::bounded(COMMAND_CAPACITY);
        let (events_tx, events_rx) = flume::unbounded();
        let (ended_tx, ended_rx) = flume::bounded(ENDED_CAPACITY);
        let (mixer_tx, mixer_rx) = flume::bounded(MIXER_CAPACITY);
        let lanes = Arc::new(Mutex::new(Lanes::default()));
        let spatial = Arc::new(Spatial::new());
        let handle = AudioHandle {
            commands: command_tx,
            events: events_rx,
            lanes: Arc::clone(&lanes),
            spatial: Arc::clone(&spatial),
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
            ended_rx,
            mixer_tx,
            mixer_rx,
            lanes,
            spatial,
        } = self;
        let (decode_tx, decode_rx) = flume::unbounded::<DecodeJob>();
        let (decoded_tx, decoded_rx) = flume::unbounded::<Decoded>();
        let keepalive_decoded = decoded_tx.clone();
        let decoder = thread::Builder::new()
            .name("rve-audio-decode".into())
            .spawn(move || decode_loop(decode_rx, decoded_tx));
        if let Err(error) = &decoder {
            log::warn!("декодер звука не запущен: {error}; клипы не загрузятся");
        }

        let read = read_source(config.resources);
        let (output, mixer) = if config.output {
            match output::open(mixer_rx, ended_tx.clone(), Arc::clone(&spatial)) {
                Ok(output) => (Some(output), Some(mixer_tx)),
                Err(error) => {
                    log::warn!("звук отключён: {error}; игра продолжает работу без звука");
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
            events: events_tx,
            decode_tx,
        };
        loop {
            let incoming = Selector::new()
                .recv(&commands, Incoming::Command)
                .recv(&decoded_rx, Incoming::Decoded)
                .recv(&ended_rx, Incoming::Ended)
                .wait_timeout(if core.loading.is_empty() {
                    IDLE_POLL
                } else {
                    READ_POLL
                });
            match incoming {
                Ok(Incoming::Command(Ok(command))) => {
                    if !core.command(command) {
                        break;
                    }
                }
                Ok(Incoming::Command(Err(_))) => break,
                Ok(Incoming::Decoded(Ok(decoded))) => core.decoded(decoded),
                Ok(Incoming::Ended(Ok(voice))) => core.voice_ended(voice),
                // `keepalive_decoded` и `ended_tx` живут до конца `run`, поэтому отключения не бывает.
                Ok(Incoming::Decoded(Err(_))) | Ok(Incoming::Ended(Err(_))) | Err(_) => {}
            }
            core.poll_reads();
        }

        drop(core);
        drop(output);
        drop(keepalive_decoded);
        if let Ok(decoder) = decoder {
            let _ = decoder.join();
        }
    }
}

enum Incoming {
    Command(Result<Command, flume::RecvError>),
    Decoded(Result<Decoded, flume::RecvError>),
    Ended(Result<VoiceId, flume::RecvError>),
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
    mixer: Option<Sender<MixerCommand>>,
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
                self.pending.retain(|pending| pending.voice != voice);
                self.send_mixer(MixerCommand::Stop { voice });
            }
            Command::StopAll => {
                self.pending.clear();
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
            Command::Shutdown => return false,
        }
        true
    }

    fn play(&mut self, voice: VoiceId, lane: usize, path: String, request: PlayRequest) {
        if let Some(clip) = self.cache.get(&path).cloned() {
            self.send_mixer(MixerCommand::Play {
                voice,
                lane,
                clip,
                request,
            });
            return;
        }
        if self.failed.contains(&path) {
            self.release(voice);
            return;
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
            self.send_mixer(MixerCommand::Play {
                voice: pending.voice,
                lane: pending.lane,
                clip: Arc::clone(&clip),
                request: pending.request,
            });
        }
    }

    /// Сообщает об ошибке клипа один раз и освобождает слоты его ожидающих голосов.
    fn fail(&mut self, path: &str, message: impl fmt::Display) {
        log::warn!("аудиоклип {path}: {message}; звук не будет воспроизведён");
        self.failed.insert(path.to_owned());
        self.loading.remove(path);
        self.decoding.remove(path);
        let (dropped, waiting): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|pending| pending.path == path);
        self.pending = waiting;
        for pending in dropped {
            self.release(pending.voice);
        }
    }

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

    fn send_mixer(&self, command: MixerCommand) {
        let Some(mixer) = &self.mixer else {
            return;
        };
        if mixer.send_timeout(command, MIXER_SEND_TIMEOUT).is_err() {
            log::warn!("очередь микшера переполнена; звуковая команда отброшена");
        }
    }
}

fn decode_loop(jobs: Receiver<DecodeJob>, results: Sender<Decoded>) {
    for DecodeJob { path, bytes } in jobs {
        let extension = Path::new(&path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        let result = decode(&extension, &bytes);
        if results.send(Decoded { path, result }).is_err() {
            break;
        }
    }
}

fn read_source(resources: Option<ResourceHandle>) -> ReadSource {
    match resources {
        Some(resources) => Box::new(move |path: &str| {
            resources
                .read_all(path)
                .map(poll_request)
                .map_err(|error| error.to_string())
        }),
        None => Box::new(|_path: &str| Err("ресурсы проекта не подключены".to_owned())),
    }
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
