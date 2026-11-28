//! Play в отдельном процессе: запуск `rve-player --editor-ipc`, протокол `rve-play-ipc` поверх
//! stdin/stdout и остановка дерева процессов. Игру редактор не загружает: состояние опрашивается
//! в кадре через [`PlayProcess::poll`], а читатели потоков только будят окно.

use crate::build::ProcessTree;
use crate::console::LogLevel;
use rve_play_ipc::{
    DecodedPlayerMessage, EditorMessage, MAX_RECORD_BYTES, PROTOCOL_VERSION, PlayerLogLevel,
    PlayerMessage, PlayerStatus, PlayerStreamDecoder, encode_editor_message,
    is_supported_protocol_version,
};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::time::{Duration, Instant};

/// Сколько ждать `hello` после запуска процесса.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько ждать выхода после `stop`, прежде чем завершить дерево.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Интервал опроса, пока процесс жив.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Сколько дочитывать stdout и stderr после выхода процесса.
const READERS_GRACE: Duration = Duration::from_secs(1);
/// Сколько ждать `stop` при закрытии редактора до завершения дерева.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Записи журнала, которые читатели держат между опросами; лишние считаются пропущенными.
const LOG_CAPACITY: usize = 8192;
/// Пометка строки stderr, которую обрезали по `MAX_RECORD_BYTES`.
const LINE_TRUNCATED_MARKER: &str = " [line truncated]";
/// Не больше стольких записей журнала переносится в Console за один опрос.
const LOG_PER_POLL: usize = 512;
/// Переменная пароля сети, которую player читает сам; редактор её не передаёт дочернему процессу.
const NETWORK_PASSWORD_ENV: &str = "RVE_NET_PASSWORD";

/// Callback, который будит окно редактора при новом сообщении от процесса.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Фаза Play-процесса; редактор показывает её в тулбаре и панели Game.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// До `hello`: процесс ещё не подключился по протоколу.
    Launching,
    /// `start` отправлен, сцена и скрипты грузятся.
    Loading,
    Running,
    Stopping,
    /// Дерево завершено редактором; ждём выхода.
    Killing,
    Exited,
}

/// Действие машины состояний над процессом.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Start,
    Stop,
    Kill(String),
}

/// Машина состояний Play-процесса. Время передаётся аргументами, поэтому таймауты проверяются
/// без ожидания.
#[derive(Debug)]
pub struct PlayMachine {
    phase: Phase,
    hello_received: bool,
    stop_requested: bool,
    reached_running: bool,
    hello_deadline: Option<Instant>,
    stop_deadline: Option<Instant>,
}

impl PlayMachine {
    pub fn new(now: Instant) -> Self {
        Self {
            phase: Phase::Launching,
            hello_received: false,
            stop_requested: false,
            reached_running: false,
            hello_deadline: Some(now + HELLO_TIMEOUT),
            stop_deadline: None,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    pub fn reached_running(&self) -> bool {
        self.reached_running
    }

    /// Ближайший срок, к которому нужна проверка таймаута.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Launching => self.hello_deadline,
            Phase::Stopping => self.stop_deadline,
            _ => None,
        }
    }

    /// `hello` принимается один раз; версия протокола должна совпасть. Если Stop уже запрошен,
    /// `start` не отправляется: процесс выйдет по команде `stop`, которая лежит в stdin.
    pub fn on_hello(&mut self, protocol_version: u32) -> Vec<Effect> {
        if self.hello_received {
            return self.kill("повторный hello от rve-player".to_owned());
        }
        self.hello_received = true;
        self.hello_deadline = None;
        if !is_supported_protocol_version(protocol_version) {
            return self.kill(format!(
                "протокол rve-player {protocol_version} не поддерживается редактором (версия {PROTOCOL_VERSION})"
            ));
        }
        if self.phase != Phase::Launching || self.stop_requested {
            return Vec::new();
        }
        self.phase = Phase::Loading;
        vec![Effect::Start]
    }

    pub fn on_status(&mut self, status: PlayerStatus, now: Instant) -> Vec<Effect> {
        if !self.hello_received {
            return self.kill("статус rve-player получен до hello".to_owned());
        }
        match status {
            PlayerStatus::Loading => {}
            PlayerStatus::Running => {
                self.reached_running = true;
                if self.phase == Phase::Loading {
                    self.phase = Phase::Running;
                }
            }
            PlayerStatus::Stopping => {
                if matches!(self.phase, Phase::Loading | Phase::Running) {
                    self.phase = Phase::Stopping;
                    self.stop_deadline = Some(now + STOP_TIMEOUT);
                }
            }
        }
        Vec::new()
    }

    /// Пользователь или MCP запросили остановку. Повторный запрос не отправляет `stop` снова.
    pub fn request_stop(&mut self, now: Instant) -> Option<Effect> {
        if self.stop_requested || matches!(self.phase, Phase::Killing | Phase::Exited) {
            return None;
        }
        self.stop_requested = true;
        if self.phase != Phase::Stopping {
            self.phase = Phase::Stopping;
            self.stop_deadline = Some(now + STOP_TIMEOUT);
        }
        Some(Effect::Stop)
    }

    /// Проверяет сроки `hello` и остановки.
    pub fn poll(&mut self, now: Instant) -> Vec<Effect> {
        match self.phase {
            Phase::Launching if self.hello_deadline.is_some_and(|deadline| now >= deadline) => self
                .kill(format!(
                    "rve-player не прислал hello за {} с",
                    HELLO_TIMEOUT.as_secs()
                )),
            Phase::Stopping if self.stop_deadline.is_some_and(|deadline| now >= deadline) => self
                .kill(format!(
                    "Play не остановился за {} с",
                    STOP_TIMEOUT.as_secs()
                )),
            _ => Vec::new(),
        }
    }

    pub fn on_exit(&mut self) {
        self.phase = Phase::Exited;
    }

    fn kill(&mut self, reason: String) -> Vec<Effect> {
        if matches!(self.phase, Phase::Killing | Phase::Exited) {
            return Vec::new();
        }
        self.phase = Phase::Killing;
        vec![Effect::Kill(reason)]
    }
}

/// Итог завершённого процесса для Console. `code` — `None`, если процесс завершён сигналом.
pub fn finish_message(
    code: Option<i32>,
    killed: Option<&str>,
    stop_requested: bool,
    reached_running: bool,
) -> (LogLevel, String) {
    if let Some(reason) = killed {
        return (
            LogLevel::Warn,
            format!("Play process завершён редактором: {reason}"),
        );
    }
    match code {
        Some(0) if stop_requested || reached_running => (LogLevel::Info, "Play stopped".to_owned()),
        Some(0) => (
            LogLevel::Warn,
            "rve-player завершился до запуска окна (возможно, игра уже запущена: AllowMultipleInstances=false)"
                .to_owned(),
        ),
        Some(1) => (
            LogLevel::Error,
            "rve-player завершился с кодом 1: ошибка во время работы".to_owned(),
        ),
        Some(2) => (
            LogLevel::Error,
            "rve-player завершился с кодом 2: неверные аргументы или данные проекта".to_owned(),
        ),
        Some(3) => (
            LogLevel::Error,
            "rve-player завершился с кодом 3: не выполнены требования к системе".to_owned(),
        ),
        Some(4) => (
            LogLevel::Error,
            "rve-player завершился с кодом 4: игра уже запущена".to_owned(),
        ),
        Some(code) => (
            LogLevel::Error,
            format!("rve-player завершился с кодом {code}"),
        ),
        None => (LogLevel::Error, "Play process terminated".to_owned()),
    }
}

/// Бинарь player рядом с редактором: в SDK-раскладке (есть `rve-sdk.toml`) — dev-вариант,
/// в checkout — `rve-player` из `build/debug`.
pub fn player_program(exe_dir: &Path) -> Result<PathBuf, String> {
    let sdk = exe_dir.join("rve-sdk.toml").is_file();
    let path = exe_dir.join(player_file_name(sdk));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "не найден {}; соберите его командой `cargo build -p rve-player`",
            path.display()
        ))
    }
}

fn player_file_name(sdk: bool) -> &'static str {
    match (cfg!(windows), sdk) {
        (true, true) => "rve-player-dev.exe",
        (true, false) => "rve-player.exe",
        (false, true) => "rve-player-dev",
        (false, false) => "rve-player",
    }
}

/// Аргументы `rve-player --editor-ipc`: проект, файл сохранения, библиотека и каталог тяжёлых
/// префабов, если они заданы, затем сетевые флаги. Пароль сюда не входит.
pub fn player_args(
    project: &Path,
    save_path: &Path,
    scripts: Option<&Path>,
    prefab_catalog: Option<&Path>,
    network: &[String],
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--project"),
        project.as_os_str().to_owned(),
        OsString::from("--editor-ipc"),
        OsString::from("--save-path"),
        save_path.as_os_str().to_owned(),
    ];
    if let Some(scripts) = scripts {
        args.push(OsString::from("--scripts"));
        args.push(scripts.as_os_str().to_owned());
    }
    if let Some(catalog) = prefab_catalog {
        args.push(OsString::from("--prefab-catalog"));
        args.push(catalog.as_os_str().to_owned());
    }
    args.extend(network.iter().map(OsString::from));
    args
}

/// Всё, что нужно для одного запуска player.
pub struct PlayerLaunch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Пароль комнаты; уходит сообщением `start`, в аргументы и переменные окружения не попадает.
    pub password: Option<String>,
    /// Временные файлы сессии; удаляются после выхода процесса.
    pub temp_files: Vec<PathBuf>,
    /// Дополнительное окружение. В редакторе пусто; тесты передают им режим тестового стаба.
    pub env: Vec<(OsString, OsString)>,
}

/// Переменная, по которой тестовый стаб выбирает поведение; в редакторе не задаётся.
#[cfg(test)]
const STUB_ENV: &str = "RVE_PLAY_PROCESS_STUB";

/// Запуск тестового стаба: этот же тестовый бинарник в режиме `play_process_stub`. Нужен тестам
/// редактора, которые проверяют реальный процесс, а не его имитацию.
#[cfg(test)]
pub(crate) fn test_stub_launch(mode: &str, temp_files: Vec<PathBuf>) -> PlayerLaunch {
    PlayerLaunch {
        program: std::env::current_exe().unwrap(),
        args: [
            "--exact",
            "play_process::tests::play_process_stub",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ]
        .map(OsString::from)
        .to_vec(),
        password: Some("pa ss".to_owned()),
        temp_files,
        env: vec![(STUB_ENV.into(), mode.into())],
    }
}

/// Событие для Console и состояния редактора.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayEvent {
    Log {
        level: LogLevel,
        text: String,
    },
    /// Процесс завершён и журнал дочитан; после этого `PlayProcess` больше ничего не отдаёт.
    Finished {
        level: LogLevel,
        message: String,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct PlayLog {
    level: LogLevel,
    text: String,
}

/// Сообщения читателей, которые обрабатывает машина состояний.
#[derive(Debug, Eq, PartialEq)]
enum Control {
    Hello {
        protocol_version: u32,
        pid: u32,
        runtime_version: String,
    },
    Status(PlayerStatus),
    ProtocolError(String),
    ReaderClosed,
}

/// Запущенный `rve-player --editor-ipc`. Опрос неблокирующий: [`Self::poll`] из кадра редактора.
pub struct PlayProcess {
    child: std::process::Child,
    tree: ProcessTree,
    stdin: Option<ChildStdin>,
    password: Option<String>,
    /// Держит канал открытым, чтобы ожидание событий не возвращало ошибку при закрытых читателях.
    _control_keepalive: Sender<Control>,
    control: Receiver<Control>,
    pending: VecDeque<Control>,
    logs: Receiver<PlayLog>,
    skipped: Arc<AtomicU64>,
    /// Поднят, пока окно уже разбужено и не опросило каналы; читатели будят окно только при
    /// переходе из `false` в `true`.
    wake_pending: Arc<AtomicBool>,
    readers_open: usize,
    machine: PlayMachine,
    killed: Option<String>,
    /// `Some(code)` после выхода процесса; `code` — `None` при завершении сигналом.
    exited: Option<Option<i32>>,
    exited_at: Option<Instant>,
    temp_files: Vec<PathBuf>,
    finished: bool,
}

impl PlayProcess {
    /// Запускает процесс и читатели stdout и stderr. Дерево процессов привязывается сразу после
    /// запуска, до команды `start`: потомки player начинают работать только после неё.
    pub fn spawn(launch: PlayerLaunch, now: Instant, wake: Waker) -> io::Result<Self> {
        let PlayerLaunch {
            program,
            args,
            password,
            temp_files,
            env,
        } = launch;
        let mut command = Command::new(&program);
        command
            .args(&args)
            .envs(env)
            .env_remove(NETWORK_PASSWORD_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut tree = ProcessTree::prepare(&mut command)?;
        let mut child = command.spawn()?;
        if let Err(error) = tree.attach(&child) {
            let _ = child.kill();
            return Err(error);
        }
        let (control_keepalive, control) = mpsc::channel();
        let (log_sender, logs) = mpsc::sync_channel(LOG_CAPACITY);
        let skipped = Arc::new(AtomicU64::new(0));
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
            let _ = tree.terminate();
            let _ = child.kill();
            return Err(io::Error::other("у rve-player нет stdout или stderr"));
        };
        let wake_pending = Arc::new(AtomicBool::new(false));
        let reader = Reader {
            control: control_keepalive.clone(),
            logs: log_sender,
            skipped: Arc::clone(&skipped),
            wake,
            wake_pending: Arc::clone(&wake_pending),
        };
        let stdout_reader = reader.clone();
        let spawned = std::thread::Builder::new()
            .name("rve-play-stdout".to_owned())
            .spawn(move || read_stdout(stdout, &stdout_reader));
        if let Err(error) = spawned {
            let _ = tree.terminate();
            let _ = child.kill();
            return Err(error);
        }
        let spawned = std::thread::Builder::new()
            .name("rve-play-stderr".to_owned())
            .spawn(move || read_stderr(stderr, &reader));
        if let Err(error) = spawned {
            let _ = tree.terminate();
            let _ = child.kill();
            return Err(error);
        }
        Ok(Self {
            child,
            tree,
            stdin,
            password,
            _control_keepalive: control_keepalive,
            control,
            pending: VecDeque::new(),
            logs,
            skipped,
            wake_pending,
            readers_open: 2,
            machine: PlayMachine::new(now),
            killed: None,
            exited: None,
            exited_at: None,
            temp_files,
            finished: false,
        })
    }

    pub fn phase(&self) -> Phase {
        self.machine.phase()
    }

    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Ближайший срок, к которому нужен опрос: таймаут `hello` или остановки, либо конец
    /// дочитывания журнала после выхода процесса.
    pub fn next_deadline(&self) -> Option<Instant> {
        let grace = match (self.exited_at, self.readers_open) {
            (Some(exited_at), readers) if readers > 0 => Some(exited_at + READERS_GRACE),
            _ => None,
        };
        match (self.machine.next_deadline(), grace) {
            (Some(first), Some(second)) => Some(first.min(second)),
            (first, second) => first.or(second),
        }
    }

    /// Запрос остановки: команда `stop` и срок на выход. Повторный вызов ничего не делает.
    pub fn request_stop(&mut self, now: Instant) -> Vec<PlayEvent> {
        let mut events = Vec::new();
        if let Some(command) = self.machine.request_stop(now) {
            self.apply(vec![command], &mut events);
        }
        events
    }

    /// Неблокирующий опрос из кадра: принимает сообщения, проверяет сроки и выход процесса,
    /// переносит журнал и публикует итог после дочитывания.
    pub fn poll(&mut self, now: Instant) -> Vec<PlayEvent> {
        let mut events = Vec::new();
        if self.finished {
            return events;
        }
        // Сброс до чтения каналов: сообщение, пришедшее позже, снова поднимет флаг и разбудит окно.
        self.wake_pending.store(false, Ordering::Release);
        // Выход наблюдается раньше сообщений: ошибка протокола, пришедшая после выхода, не должна
        // подменять реальный код завершения.
        self.observe_exit(now);
        self.drain_control(now, &mut events);
        let commands = self.machine.poll(now);
        self.apply(commands, &mut events);
        self.drain_logs(LOG_PER_POLL, &mut events);
        self.finish_if_done(now, &mut events);
        events
    }

    /// Закрытие редактора: `stop`, ожидание до `grace`, затем завершение дерева. Журнал не
    /// переносится, поэтому вызывается только при закрытии, когда кадров больше не будет.
    pub fn shutdown(&mut self, grace: Duration) {
        let started = Instant::now();
        let mut discarded = self.request_stop(started);
        if self.wait_until_finished(started + grace, &mut discarded) {
            return;
        }
        let mut discarded = Vec::new();
        let now = Instant::now();
        self.kill("редактор закрывается".to_owned(), &mut discarded);
        if !self.wait_until_finished(now + SHUTDOWN_GRACE, &mut discarded) {
            self.remove_temp_files();
        }
    }

    fn wait_until_finished(&mut self, deadline: Instant, events: &mut Vec<PlayEvent>) -> bool {
        loop {
            let now = Instant::now();
            events.extend(self.poll(now));
            if self.finished {
                return true;
            }
            if now >= deadline {
                return false;
            }
            let wait = deadline.saturating_duration_since(now).min(POLL_INTERVAL);
            if let Ok(message) = self.control.recv_timeout(wait) {
                self.pending.push_back(message);
            }
        }
    }

    fn drain_control(&mut self, now: Instant, events: &mut Vec<PlayEvent>) {
        while let Some(message) = self
            .pending
            .pop_front()
            .or_else(|| self.control.try_recv().ok())
        {
            let commands = match message {
                Control::Hello {
                    protocol_version,
                    pid,
                    runtime_version,
                } => {
                    events.push(log(
                        LogLevel::Info,
                        format!("rve-player {runtime_version} (pid {pid}) подключён"),
                    ));
                    self.machine.on_hello(protocol_version)
                }
                Control::Status(status) => {
                    let was_running = self.machine.reached_running();
                    let commands = self.machine.on_status(status, now);
                    if !was_running && self.machine.reached_running() {
                        events.push(log(
                            LogLevel::Info,
                            "Play запущен в отдельном окне".to_owned(),
                        ));
                    }
                    commands
                }
                Control::ProtocolError(error) => {
                    vec![Effect::Kill(format!(
                        "ошибка протокола rve-player: {error}"
                    ))]
                }
                Control::ReaderClosed => {
                    self.readers_open = self.readers_open.saturating_sub(1);
                    Vec::new()
                }
            };
            self.apply(commands, events);
        }
    }

    fn observe_exit(&mut self, now: Instant) {
        if self.exited.is_some() {
            return;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exited = Some(status.code());
            self.exited_at = Some(now);
            self.machine.on_exit();
        }
    }

    fn apply(&mut self, commands: Vec<Effect>, events: &mut Vec<PlayEvent>) {
        for command in commands {
            match command {
                Effect::Start => {
                    let message = EditorMessage::Start {
                        password: self.password.clone(),
                    };
                    self.write_editor(&message, events);
                }
                Effect::Stop => self.write_editor(&EditorMessage::Stop, events),
                Effect::Kill(reason) => self.kill(reason, events),
            }
        }
    }

    fn write_editor(&mut self, message: &EditorMessage, events: &mut Vec<PlayEvent>) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let encoded = encode_editor_message(message);
        if let Err(error) = stdin
            .write_all(encoded.as_bytes())
            .and_then(|()| stdin.flush())
        {
            events.push(log(
                LogLevel::Warn,
                format!("не удалось отправить команду rve-player: {error}"),
            ));
        }
    }

    fn kill(&mut self, reason: String, events: &mut Vec<PlayEvent>) {
        self.observe_exit(Instant::now());
        if self.killed.is_some() || self.exited.is_some() {
            return;
        }
        // Дерево убивается до `wait`: пока лидер не собран, идентификатор группы занят.
        if let Err(error) = self.tree.terminate() {
            events.push(log(
                LogLevel::Error,
                format!("не удалось завершить дерево Play: {error}"),
            ));
            let _ = self.child.kill();
        }
        self.killed = Some(reason);
    }

    fn drain_logs(&mut self, limit: usize, events: &mut Vec<PlayEvent>) {
        for _ in 0..limit {
            let Ok(record) = self.logs.try_recv() else {
                break;
            };
            events.push(PlayEvent::Log {
                level: record.level,
                text: record.text,
            });
        }
    }

    fn finish_if_done(&mut self, now: Instant, events: &mut Vec<PlayEvent>) {
        let Some(exited_at) = self.exited_at else {
            return;
        };
        if self.readers_open > 0 {
            if now < exited_at + READERS_GRACE {
                return;
            }
            // Вывод не закрылся за grace: оставшиеся потомки держат stdout или stderr. Дерево
            // убивается, иначе редактор ждал бы их вечно.
            self.kill_leftover_tree(events);
        }
        // Итог публикуется только после последних строк: иначе причина выхода окажется раньше
        // сообщения, которое её объясняет. Канал ограничен, поэтому дочитывание тоже ограничено.
        self.drain_logs(LOG_CAPACITY, events);
        let skipped = self.skipped.swap(0, Ordering::AcqRel);
        if skipped > 0 {
            events.push(log(
                LogLevel::Warn,
                format!("пропущено {skipped} строк журнала Play"),
            ));
        }
        let code = self.exited.flatten();
        let (level, message) = finish_message(
            code,
            self.killed.as_deref(),
            self.machine.stop_requested(),
            self.machine.reached_running(),
        );
        events.push(PlayEvent::Finished { level, message });
        self.finished = true;
        self.remove_temp_files();
    }

    /// Потомки, которые остались после выхода `rve-player`. Процесс уже собран, поэтому обычный
    /// `kill` его не тронет; дерево убивается по группе, пока её идентификатор занят.
    /// `killed` не меняется: итог строится по настоящему коду выхода игрока.
    fn kill_leftover_tree(&mut self, events: &mut Vec<PlayEvent>) {
        if self.killed.is_some() {
            return;
        }
        match self.tree.terminate() {
            Ok(()) => events.push(log(
                LogLevel::Warn,
                "потомки rve-player не завершились после его выхода и завершены редактором"
                    .to_owned(),
            )),
            Err(error) => events.push(log(
                LogLevel::Error,
                format!("не удалось завершить дерево Play: {error}"),
            )),
        }
    }

    fn remove_temp_files(&mut self) {
        for path in self.temp_files.drain(..) {
            let _ = fs::remove_file(path);
        }
    }

    /// Тестовый доступ: читатели закрыты, то есть весь вывод процесса получен.
    #[cfg(test)]
    fn readers_closed(&self) -> bool {
        self.readers_open == 0
    }

    /// Ждёт сообщение читателей не дольше `timeout`; без него тест опрашивал бы процесс впустую.
    #[cfg(test)]
    pub(crate) fn wait_event(&mut self, timeout: Duration) {
        if let Ok(message) = self.control.recv_timeout(timeout) {
            self.pending.push_back(message);
        }
    }
}

impl Drop for PlayProcess {
    fn drop(&mut self) {
        if !self.finished {
            self.shutdown(SHUTDOWN_GRACE);
        }
        self.remove_temp_files();
    }
}

fn log(level: LogLevel, text: String) -> PlayEvent {
    PlayEvent::Log { level, text }
}

/// Общие каналы читателей: управляющие сообщения без ограничения, журнал ограничен.
#[derive(Clone)]
struct Reader {
    control: Sender<Control>,
    logs: SyncSender<PlayLog>,
    skipped: Arc<AtomicU64>,
    wake: Waker,
    wake_pending: Arc<AtomicBool>,
}

impl Reader {
    /// Будит окно, если оно ещё не разбужено. Пока редактор не опросил каналы, остальные
    /// сообщения ждут в очереди без новых пробуждений.
    fn wake(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    /// После закрытия редактором канал отключён: сообщение теряется и окно не будится.
    fn control(&self, message: Control) {
        if self.control.send(message).is_ok() {
            self.wake();
        }
    }

    fn log(&self, level: LogLevel, text: String) {
        match self.logs.try_send(PlayLog { level, text }) {
            Ok(()) => self.wake(),
            Err(TrySendError::Full(_)) => {
                self.skipped.fetch_add(1, Ordering::AcqRel);
                self.wake();
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn closed(&self) {
        self.control(Control::ReaderClosed);
    }
}

fn read_stdout(mut stdout: ChildStdout, reader: &Reader) {
    let mut decoder = PlayerStreamDecoder::default();
    let mut buffer = [0_u8; 8192];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                for record in decoder.push(&buffer[..read]) {
                    dispatch_record(record, reader);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    for record in decoder.finish() {
        dispatch_record(record, reader);
    }
    reader.closed();
}

fn dispatch_record(record: DecodedPlayerMessage, reader: &Reader) {
    match record {
        DecodedPlayerMessage::Raw(text) => reader.log(LogLevel::Info, text),
        DecodedPlayerMessage::Message(PlayerMessage::Hello {
            protocol_version,
            pid,
            runtime_version,
        }) => reader.control(Control::Hello {
            protocol_version,
            pid,
            runtime_version,
        }),
        DecodedPlayerMessage::Message(PlayerMessage::Status(status)) => {
            reader.control(Control::Status(status));
        }
        DecodedPlayerMessage::Message(PlayerMessage::Log {
            level,
            target,
            text,
        }) => {
            let level = match level {
                PlayerLogLevel::Trace | PlayerLogLevel::Debug | PlayerLogLevel::Info => {
                    LogLevel::Info
                }
                PlayerLogLevel::Warn => LogLevel::Warn,
                PlayerLogLevel::Error => LogLevel::Error,
            };
            let text = if target.is_empty() {
                text
            } else {
                format!("{target}: {text}")
            };
            reader.log(level, text);
        }
        // Итог процесса берётся из статуса выхода; сообщение `exit` нужно только для протокола.
        DecodedPlayerMessage::Message(PlayerMessage::Exit { .. })
        | DecodedPlayerMessage::Unknown { .. } => {}
        // Только `hello` определяет совместимость: без него редактор не знает, что за процесс
        // перед ним. Остальные испорченные записи — предупреждение в журнале, процесс продолжает работу.
        DecodedPlayerMessage::Malformed { kind, error } if kind == "hello" => {
            reader.control(Control::ProtocolError(format!("hello: {error}")));
        }
        DecodedPlayerMessage::Malformed { kind, error } => reader.log(
            LogLevel::Warn,
            format!("повреждённая запись rve-player ({kind}): {error}"),
        ),
    }
}

fn read_stderr(stderr: std::process::ChildStderr, reader: &Reader) {
    read_bounded_lines(BufReader::new(stderr), |text| {
        reader.log(LogLevel::Warn, text);
    });
    reader.closed();
}

/// Строки stderr с тем же лимитом, что у записей протокола: длинная строка без `\n` не растёт
/// сверх `MAX_RECORD_BYTES`, её хвост отбрасывается до конца строки, а сама строка получает пометку.
fn read_bounded_lines(mut reader: impl BufRead, mut on_line: impl FnMut(String)) {
    let mut line: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let (consumed, ends_line) = {
            let available = match reader.fill_buf() {
                Ok([]) => break,
                Ok(available) => available,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let (chunk, ends_line) = match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => (&available[..index], true),
                None => (available, false),
            };
            let room = MAX_RECORD_BYTES - line.len();
            if chunk.len() > room {
                line.extend_from_slice(&chunk[..room]);
                truncated = true;
            } else {
                line.extend_from_slice(chunk);
            }
            (chunk.len() + usize::from(ends_line), ends_line)
        };
        reader.consume(consumed);
        if ends_line {
            on_line(finish_line(&mut line, &mut truncated));
        }
    }
    if !line.is_empty() || truncated {
        on_line(finish_line(&mut line, &mut truncated));
    }
}

fn finish_line(line: &mut Vec<u8>, truncated: &mut bool) -> String {
    let mut text = String::from_utf8_lossy(&line[..])
        .trim_end_matches('\r')
        .to_owned();
    line.clear();
    if std::mem::take(truncated) {
        text.push_str(LINE_TRUNCATED_MARKER);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn hello_starts_the_process_once_and_checks_the_protocol_version() {
        let start = now();
        let mut machine = PlayMachine::new(start);
        assert_eq!(machine.on_hello(PROTOCOL_VERSION), vec![Effect::Start]);
        assert_eq!(machine.phase(), Phase::Loading);
        assert!(machine.next_deadline().is_none());
        assert!(matches!(
            machine.on_hello(PROTOCOL_VERSION).as_slice(),
            [Effect::Kill(reason)] if reason.contains("повторный")
        ));

        let mut other = PlayMachine::new(start);
        assert!(matches!(
            other.on_hello(PROTOCOL_VERSION + 1).as_slice(),
            [Effect::Kill(reason)] if reason.contains("не поддерживается")
        ));
        assert_eq!(other.phase(), Phase::Killing);
    }

    #[test]
    fn missing_hello_is_killed_at_the_deadline_only() {
        let start = now();
        let mut machine = PlayMachine::new(start);
        assert_eq!(machine.next_deadline(), Some(start + HELLO_TIMEOUT));
        assert!(
            machine
                .poll(start + HELLO_TIMEOUT - Duration::from_millis(1))
                .is_empty()
        );
        assert!(matches!(
            machine.poll(start + HELLO_TIMEOUT).as_slice(),
            [Effect::Kill(reason)] if reason.contains("hello")
        ));
        assert_eq!(machine.phase(), Phase::Killing);
        assert!(
            machine.poll(start + HELLO_TIMEOUT * 2).is_empty(),
            "kill is sent once"
        );
    }

    #[test]
    fn status_before_hello_is_a_protocol_error() {
        let mut machine = PlayMachine::new(now());
        assert!(matches!(
            machine.on_status(PlayerStatus::Running, now()).as_slice(),
            [Effect::Kill(reason)] if reason.contains("до hello")
        ));
    }

    #[test]
    fn stop_before_hello_never_sends_start_and_times_out_on_its_own_deadline() {
        let start = now();
        let mut machine = PlayMachine::new(start);
        assert_eq!(machine.request_stop(start), Some(Effect::Stop));
        assert!(machine.stop_requested());
        assert_eq!(machine.phase(), Phase::Stopping);
        assert_eq!(machine.request_stop(start), None, "stop is sent once");
        assert!(machine.on_hello(PROTOCOL_VERSION).is_empty());
        assert_eq!(machine.next_deadline(), Some(start + STOP_TIMEOUT));
        assert!(matches!(
            machine.poll(start + STOP_TIMEOUT).as_slice(),
            [Effect::Kill(reason)] if reason.contains("не остановился")
        ));
    }

    #[test]
    fn stop_times_out_after_running_and_a_clean_exit_needs_no_kill() {
        let start = now();
        let mut machine = PlayMachine::new(start);
        machine.on_hello(PROTOCOL_VERSION);
        machine.on_status(PlayerStatus::Running, start);
        assert_eq!(machine.phase(), Phase::Running);
        assert_eq!(machine.request_stop(start), Some(Effect::Stop));
        assert_eq!(machine.request_stop(start), None);
        machine.on_exit();
        assert_eq!(machine.phase(), Phase::Exited);
        assert!(machine.poll(start + STOP_TIMEOUT * 3).is_empty());
    }

    #[test]
    fn process_stopping_by_itself_is_timed_out_and_user_stop_still_sends_stop() {
        let start = now();
        let mut machine = PlayMachine::new(start);
        machine.on_hello(PROTOCOL_VERSION);
        machine.on_status(PlayerStatus::Running, start);
        machine.on_status(PlayerStatus::Stopping, start);
        assert_eq!(machine.next_deadline(), Some(start + STOP_TIMEOUT));
        assert!(!machine.stop_requested());
        assert_eq!(machine.request_stop(start), Some(Effect::Stop));
        assert_eq!(machine.next_deadline(), Some(start + STOP_TIMEOUT));
    }

    #[test]
    fn exit_before_running_is_reported_as_a_failed_start() {
        assert_eq!(
            finish_message(Some(0), None, false, false),
            (
                LogLevel::Warn,
                "rve-player завершился до запуска окна (возможно, игра уже запущена: AllowMultipleInstances=false)"
                    .to_owned()
            )
        );
    }

    #[test]
    fn finish_messages_cover_exit_codes_signals_and_editor_kills() {
        assert_eq!(
            finish_message(Some(0), None, true, false),
            (LogLevel::Info, "Play stopped".to_owned())
        );
        assert_eq!(
            finish_message(Some(0), None, false, true),
            (LogLevel::Info, "Play stopped".to_owned())
        );
        for (code, fragment) in [
            (1, "с кодом 1"),
            (2, "с кодом 2"),
            (3, "с кодом 3"),
            (4, "с кодом 4"),
            (9, "с кодом 9"),
        ] {
            let (level, message) = finish_message(Some(code), None, true, true);
            assert_eq!(level, LogLevel::Error);
            assert!(message.contains(fragment), "{message}");
        }
        assert_eq!(
            finish_message(None, None, false, true),
            (LogLevel::Error, "Play process terminated".to_owned())
        );
        // Код 1 от TerminateJobObject не выдаётся за ошибку игры: причина убийства важнее кода.
        let (level, message) =
            finish_message(Some(1), Some("Play не остановился за 10 с"), true, true);
        assert_eq!(level, LogLevel::Warn);
        assert!(message.contains("завершён редактором"));
    }

    #[test]
    fn player_arguments_keep_optional_overrides_and_never_carry_the_password() {
        let args = player_args(
            Path::new("/work/game"),
            Path::new("/work/game/.rve/playersave/save.toml"),
            None,
            None,
            &["--host".to_owned()],
        );
        assert_eq!(
            args,
            [
                "--project",
                "/work/game",
                "--editor-ipc",
                "--save-path",
                "/work/game/.rve/playersave/save.toml",
                "--host",
            ]
        );
        let full = player_args(
            Path::new("/p"),
            Path::new("/s.toml"),
            Some(Path::new("/lib.dylib")),
            Some(Path::new("/catalog.rveprefabs")),
            &["--connect".to_owned(), "127.0.0.1:7777".to_owned()],
        );
        let text: Vec<_> = full
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &text[5..9],
            [
                "--scripts",
                "/lib.dylib",
                "--prefab-catalog",
                "/catalog.rveprefabs"
            ]
        );
        assert_eq!(&text[9..], ["--connect", "127.0.0.1:7777"]);
    }

    #[test]
    fn player_binary_is_looked_up_next_to_the_editor_with_the_sdk_variant() {
        let dir = std::env::temp_dir().join(format!("rve-play-locate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let missing = player_program(&dir).unwrap_err();
        assert!(missing.contains("cargo build -p rve-player"), "{missing}");

        let player = dir.join(player_file_name(false));
        fs::write(&player, b"").unwrap();
        assert_eq!(player_program(&dir).unwrap(), player);

        fs::write(dir.join("rve-sdk.toml"), b"").unwrap();
        let missing_dev = player_program(&dir).unwrap_err();
        assert!(
            missing_dev.contains(player_file_name(true)),
            "{missing_dev}"
        );
        let dev = dir.join(player_file_name(true));
        fs::write(&dev, b"").unwrap();
        assert_eq!(player_program(&dir).unwrap(), dev);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Ждёт события без `sleep`: опрос сменяется ожиданием сообщения читателей.
    fn drive_until(
        process: &mut PlayProcess,
        mut done: impl FnMut(&PlayProcess, &[PlayEvent]) -> bool,
    ) -> Vec<PlayEvent> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut events = Vec::new();
        loop {
            events.extend(process.poll(Instant::now()));
            if done(process, &events) {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "play process did not reach the expected state"
            );
            if let Ok(message) = process.control.recv_timeout(POLL_INTERVAL) {
                process.pending.push_back(message);
            }
        }
    }

    #[test]
    fn child_process_never_inherits_the_network_password_variable() {
        let mut launch = test_stub_launch("env", Vec::new());
        // Переменная редактора не должна дойти до игрока; пароль идёт только сообщением `start`.
        launch
            .env
            .push((NETWORK_PASSWORD_ENV.into(), "leaked".into()));
        let mut process =
            PlayProcess::spawn(launch, now(), Arc::new(|| {})).expect("spawn test stub");
        let events = drive_until(&mut process, |_, events| {
            events.iter().any(|event| {
                matches!(event, PlayEvent::Log { text, .. } if text.contains("password env present"))
            })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayEvent::Log { text, .. } if text == "stub: password env present: false"
            )),
            "{events:?}"
        );
        process.request_stop(Instant::now());
        drive_until(&mut process, |process, _| process.is_finished());
    }

    #[test]
    fn journal_keeps_a_bounded_window_and_counts_what_it_drops() {
        let (reader, _control_rx, log_rx, skipped) = test_reader();
        // Никто не читает журнал, поэтому всё сверх ёмкости считается пропущенным.
        let records = LOG_CAPACITY + 10;
        for index in 0..records {
            dispatch_record(DecodedPlayerMessage::Raw(format!("line {index}")), &reader);
        }
        assert_eq!(log_rx.try_iter().count(), LOG_CAPACITY);
        assert_eq!(skipped.load(Ordering::Acquire), 10);
    }

    /// Читатель без процесса: сообщения и журнал видны в каналах, которые можно опросить.
    /// Счётчик пробуждений возвращается вместе с читателем, чтобы тесты видели каждый wake.
    fn test_reader() -> (Reader, Receiver<Control>, Receiver<PlayLog>, Arc<AtomicU64>) {
        let (control, control_rx) = mpsc::channel();
        let (logs, log_rx) = mpsc::sync_channel(LOG_CAPACITY);
        let skipped = Arc::new(AtomicU64::new(0));
        let reader = Reader {
            control,
            logs,
            skipped: Arc::clone(&skipped),
            wake: Arc::new(|| {}),
            wake_pending: Arc::new(AtomicBool::new(false)),
        };
        (reader, control_rx, log_rx, skipped)
    }

    /// Читатель, который считает пробуждения окна; `wake_pending` возвращается для имитации опроса.
    fn counting_reader() -> (
        Reader,
        Receiver<Control>,
        Receiver<PlayLog>,
        Arc<AtomicU64>,
        Arc<AtomicBool>,
    ) {
        let (control, control_rx) = mpsc::channel();
        let (logs, log_rx) = mpsc::sync_channel(LOG_CAPACITY);
        let wakes = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&wakes);
        let wake_pending = Arc::new(AtomicBool::new(false));
        let reader = Reader {
            control,
            logs,
            skipped: Arc::new(AtomicU64::new(0)),
            wake: Arc::new(move || {
                counter.fetch_add(1, Ordering::AcqRel);
            }),
            wake_pending: Arc::clone(&wake_pending),
        };
        (reader, control_rx, log_rx, wakes, wake_pending)
    }

    #[test]
    fn wakes_coalesce_until_the_editor_polls_the_channels() {
        let (reader, _control_rx, log_rx, wakes, wake_pending) = counting_reader();
        for index in 0..10 {
            dispatch_record(DecodedPlayerMessage::Raw(format!("line {index}")), &reader);
        }
        assert_eq!(
            wakes.load(Ordering::Acquire),
            1,
            "one wake covers every record until the editor polls"
        );
        assert_eq!(log_rx.try_iter().count(), 10);

        // Опрос сбрасывает флаг перед чтением каналов: следующее сообщение снова будит окно.
        wake_pending.store(false, Ordering::Release);
        dispatch_record(DecodedPlayerMessage::Raw("next".into()), &reader);
        assert_eq!(wakes.load(Ordering::Acquire), 2);
    }

    #[test]
    fn readers_stop_waking_the_window_once_the_editor_dropped_its_channels() {
        let (reader, control_rx, log_rx, wakes, _wake_pending) = counting_reader();
        drop(control_rx);
        drop(log_rx);
        dispatch_record(DecodedPlayerMessage::Raw("late".into()), &reader);
        reader.closed();
        assert_eq!(wakes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn stderr_lines_are_capped_even_without_a_newline() {
        let mut data = vec![b'x'; MAX_RECORD_BYTES * 3];
        data.extend_from_slice(b"\nnext\r\ntail");
        let mut lines = Vec::new();
        read_bounded_lines(io::Cursor::new(data), |line| lines.push(line));
        assert_eq!(
            lines.len(),
            3,
            "{:?}",
            lines.iter().map(String::len).collect::<Vec<_>>()
        );
        assert!(lines[0].len() <= MAX_RECORD_BYTES + LINE_TRUNCATED_MARKER.len());
        assert!(lines[0].ends_with(LINE_TRUNCATED_MARKER));
        assert_eq!(lines[1], "next");
        assert_eq!(lines[2], "tail");
    }

    #[test]
    fn damaged_records_warn_and_only_a_damaged_hello_reaches_the_state_machine() {
        use rve_play_ipc::ProtocolError;

        let (reader, control, logs, _) = test_reader();
        dispatch_record(
            DecodedPlayerMessage::Malformed {
                kind: "status".into(),
                error: ProtocolError::InvalidValue("status"),
            },
            &reader,
        );
        dispatch_record(
            DecodedPlayerMessage::Malformed {
                kind: "log".into(),
                error: ProtocolError::InvalidEscape,
            },
            &reader,
        );
        assert!(
            control.try_recv().is_err(),
            "damaged status and log must not reach the state machine"
        );
        let warnings: Vec<_> = logs.try_iter().collect();
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|record| record.level == LogLevel::Warn));
        assert!(warnings[0].text.contains("status"), "{warnings:?}");

        dispatch_record(
            DecodedPlayerMessage::Malformed {
                kind: "hello".into(),
                error: ProtocolError::MissingField("process id"),
            },
            &reader,
        );
        assert_eq!(
            control.try_recv().unwrap(),
            Control::ProtocolError("hello: missing process id field".to_owned())
        );
    }

    #[test]
    fn protocol_error_after_the_player_exited_keeps_its_real_exit_code() {
        let mut process = PlayProcess::spawn(
            test_stub_launch("exit3", Vec::new()),
            now(),
            Arc::new(|| {}),
        )
        .expect("spawn test stub");
        let mut events = drive_until(&mut process, |process, _| process.exited.is_some());
        // Ошибка протокола приходит уже после выхода: она не должна стать причиной завершения.
        process
            .pending
            .push_back(Control::ProtocolError("late".to_owned()));
        events.extend(drive_until(&mut process, |process, _| {
            process.is_finished()
        }));
        assert!(process.killed.is_none(), "exit code must not be masked");
        let (level, message) = finished(&events).expect("finish message");
        assert_eq!(level, LogLevel::Error);
        assert!(message.contains("с кодом 3"), "{message}");
    }

    fn finished(events: &[PlayEvent]) -> Option<(LogLevel, String)> {
        events.iter().find_map(|event| match event {
            PlayEvent::Finished { level, message } => Some((*level, message.clone())),
            PlayEvent::Log { .. } => None,
        })
    }

    #[test]
    fn stub_that_exits_on_stop_finishes_without_a_kill() {
        let mut process =
            PlayProcess::spawn(test_stub_launch("exit", Vec::new()), now(), Arc::new(|| {}))
                .expect("spawn test stub");
        drive_until(&mut process, |process, _| process.phase() == Phase::Running);
        assert!(process.request_stop(Instant::now()).is_empty());
        let events = drive_until(&mut process, |process, _| process.is_finished());
        assert_eq!(
            finished(&events),
            Some((LogLevel::Info, "Play stopped".to_owned()))
        );
        assert!(process.killed.is_none());
    }

    #[test]
    fn hung_stub_with_a_descendant_is_killed_as_a_tree_after_the_stop_deadline() {
        let start = Instant::now();
        let mut process =
            PlayProcess::spawn(test_stub_launch("hang", Vec::new()), start, Arc::new(|| {}))
                .expect("spawn hung test stub");
        let mut descendant_reported = false;
        drive_until(&mut process, |process, events| {
            descendant_reported |= events.iter().any(|event| {
                matches!(event, PlayEvent::Log { text, .. } if text.starts_with("stub: descendant"))
            });
            process.phase() == Phase::Running && descendant_reported
        });
        assert!(process.request_stop(Instant::now()).is_empty());
        assert_eq!(process.phase(), Phase::Stopping);

        // Срок остановки проверяется синтетическим временем: реальные 10 секунд не ждём.
        let deadline = process.next_deadline().expect("stop deadline");
        let before = process.poll(deadline - Duration::from_millis(1));
        assert!(
            process.killed.is_none(),
            "no kill before the deadline: {before:?}"
        );
        let _ = process.poll(deadline);
        assert!(process.killed.is_some());

        let events = drive_until(&mut process, |process, _| process.is_finished());
        let (level, message) = finished(&events).expect("finish message");
        assert_eq!(level, LogLevel::Warn);
        assert!(message.contains("завершён редактором"), "{message}");
        assert!(
            process.readers_closed(),
            "the descendant holds stdout, so readers close only after the whole tree is gone"
        );
    }

    #[test]
    fn descendants_that_keep_the_output_open_are_killed_after_the_grace_period() {
        let mut process = PlayProcess::spawn(
            test_stub_launch("orphan", Vec::new()),
            now(),
            Arc::new(|| {}),
        )
        .expect("spawn orphan stub");
        let mut events = drive_until(&mut process, |process, _| process.exited.is_some());
        let exited_at = process.exited_at.expect("exit observed");

        // Срок проверяется синтетическим временем: вывод открыт, пока не прошёл `READERS_GRACE`.
        events.extend(process.poll(exited_at + READERS_GRACE - Duration::from_millis(1)));
        assert!(process.killed.is_none(), "{events:?}");
        assert!(!process.is_finished());

        let open = process.readers_open;
        events.extend(process.poll(exited_at + READERS_GRACE));
        assert!(process.killed.is_none(), "{events:?}");
        assert!(process.is_finished());
        let (level, message) = finished(&events).expect("finish message");
        assert_eq!(level, LogLevel::Error);
        assert!(message.contains("кодом 1"), "{message}");
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayEvent::Log { level: LogLevel::Warn, text } if text.contains("потомки rve-player")
            )),
            "{events:?}"
        );

        // Потомок действительно убит: оба оставшихся читателя получают EOF.
        let mut closed = process
            .pending
            .iter()
            .filter(|message| matches!(message, Control::ReaderClosed))
            .count();
        let deadline = Instant::now() + Duration::from_secs(20);
        while closed < open {
            assert!(Instant::now() < deadline, "descendant kept the output open");
            if let Ok(Control::ReaderClosed) = process.control.recv_timeout(POLL_INTERVAL) {
                closed += 1;
            }
        }
    }

    #[test]
    fn temporary_files_are_removed_after_the_process_finished() {
        let dir = std::env::temp_dir().join(format!("rve-play-temp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let temp = dir.join("prefabs.rveprefabs");
        fs::write(&temp, b"").unwrap();
        let mut process = PlayProcess::spawn(
            test_stub_launch("exit", vec![temp.clone()]),
            now(),
            Arc::new(|| {}),
        )
        .expect("spawn test stub");
        drive_until(&mut process, |process, _| process.phase() == Phase::Running);
        process.request_stop(Instant::now());
        drive_until(&mut process, |process, _| process.is_finished());
        assert!(!temp.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// Дочерний процесс для тестов выше: без переменной окружения ничего не делает.
    #[test]
    fn play_process_stub() {
        let Ok(mode) = std::env::var(STUB_ENV) else {
            return;
        };
        if mode == "sleeper" {
            loop {
                std::thread::park();
            }
        }
        write_record(&PlayerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            runtime_version: "test".to_owned(),
        });
        if mode == "exit3" {
            // Режим `exit3`: игрок выходит с кодом 3 сразу после hello, не читая команд.
            std::process::exit(3);
        }
        if mode == "orphan" {
            // Режим `orphan`: потомок наследует stdout и stderr и держит их открытыми после выхода.
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "play_process::tests::play_process_stub",
                    "--nocapture",
                    "--test-threads=1",
                    "-q",
                ])
                .env(STUB_ENV, "sleeper")
                .spawn()
                .expect("spawn orphan");
            write_record(&PlayerMessage::Log {
                level: PlayerLogLevel::Info,
                target: "stub".to_owned(),
                text: format!("descendant {}", child.id()),
            });
            // Ненулевой код: итог после уборки потомков должен сохранить его.
            std::process::exit(1);
        }
        let mut stdin = io::stdin().lock();
        let mut line = String::new();
        // Как `wait_for_start` у игрока: `stop` до `start` — штатный выход с кодом 0.
        let _ = stdin.read_line(&mut line);
        if line.starts_with("stop") {
            std::process::exit(0);
        }
        if !line.starts_with("start\tpa ss") {
            std::process::exit(7);
        }
        write_record(&PlayerMessage::Status(PlayerStatus::Running));
        if mode == "env" {
            // Режим `env`: игрок сообщает, видит ли он переменную пароля сети из окружения.
            let present = std::env::var_os("RVE_NET_PASSWORD").is_some();
            write_record(&PlayerMessage::Log {
                level: PlayerLogLevel::Info,
                target: "stub".to_owned(),
                text: format!("password env present: {present}"),
            });
        }
        if mode == "hang" {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "play_process::tests::play_process_stub",
                    "--nocapture",
                    "--test-threads=1",
                    "-q",
                ])
                .env(STUB_ENV, "sleeper")
                .spawn()
                .expect("spawn descendant");
            write_record(&PlayerMessage::Log {
                level: PlayerLogLevel::Info,
                target: "stub".to_owned(),
                text: format!("descendant {}", child.id()),
            });
            let _ = child.wait();
            loop {
                std::thread::park();
            }
        }
        // Режим `exit`: читаем команды, пока не придёт `stop` или EOF, затем выходим с кодом 0.
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(_) if line.trim_end() == "stop" => std::process::exit(0),
                Ok(_) => {}
            }
        }
    }

    fn write_record(message: &PlayerMessage) {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(rve_play_ipc::encode_player_message(message).as_bytes());
        let _ = stdout.flush();
    }
}
