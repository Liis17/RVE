use engine_console::{ConsoleLevel, ConsoleLine};
use rve_play_ipc::{
    EditorMessage, PROTOCOL_VERSION, PlayerLogLevel, PlayerMessage, PlayerStatus,
    decode_editor_line, encode_player_message,
};
use std::io::{self, BufRead, Write};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// Сколько живёт player после обрыва связи с редактором. Счётчик идёт в отдельном потоке и не
/// зависит от окна, поэтому его не задерживают ни этап запуска, ни зависший `cleanup`.
const PARENT_EXIT_GRACE: Duration = Duration::from_secs(15);

enum StartSignal {
    Started(Option<String>),
    Stopped,
    ParentGone,
}

pub(crate) struct EditorIpc {
    pending_stop: Arc<PendingStop>,
    start_receiver: Mutex<mpsc::Receiver<StartSignal>>,
}

impl EditorIpc {
    pub(crate) fn start() -> io::Result<Self> {
        let pending_stop = Arc::new(PendingStop::default());
        let thread_stop = Arc::clone(&pending_stop);
        let (start_sender, start_receiver) = mpsc::channel();
        thread::Builder::new()
            .name("rve-editor-ipc".to_owned())
            .spawn(move || {
                let stdin = io::stdin();
                read_commands(stdin.lock(), &thread_stop, &start_sender, || {
                    arm_parent_gone_watchdog(PARENT_EXIT_GRACE, thread::sleep, |code| {
                        process::exit(code)
                    });
                });
            })?;
        Ok(Self {
            pending_stop,
            start_receiver: Mutex::new(start_receiver),
        })
    }

    pub(crate) fn wait_for_start(&self) -> Option<Option<String>> {
        match self
            .start_receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv()
        {
            Ok(StartSignal::Started(password)) => Some(password),
            Ok(StartSignal::Stopped | StartSignal::ParentGone) | Err(_) => None,
        }
    }

    pub(crate) fn send_hello(&self) {
        write_message(&PlayerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            runtime_version: crate::VERSION.to_owned(),
        });
    }

    pub(crate) fn send_status(&self, status: PlayerStatus) {
        write_message(&PlayerMessage::Status(status));
    }

    pub(crate) fn send_exit(&self, code: u8) {
        write_message(&PlayerMessage::Exit {
            code: i32::from(code),
        });
    }

    pub(crate) fn attach_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        self.pending_stop.attach_waker(Arc::new(waker));
    }

    pub(crate) fn stop_requested(&self) -> bool {
        self.pending_stop.stop_requested()
    }
}

pub(crate) struct PendingStop {
    requested: AtomicBool,
    woken: AtomicBool,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for PendingStop {
    fn default() -> Self {
        Self {
            requested: AtomicBool::new(false),
            woken: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }
}

impl PendingStop {
    fn request_stop(&self) {
        let first_request = !self.requested.swap(true, Ordering::AcqRel);
        if first_request {
            self.wake_if_requested();
        }
    }

    fn attach_waker(&self, waker: Arc<dyn Fn() + Send + Sync>) {
        let mut current = self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_none() {
            *current = Some(waker);
        }
        drop(current);
        self.wake_if_requested();
    }

    fn wake_if_requested(&self) {
        if !self.stop_requested() {
            return;
        }
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(waker) = waker
            && !self.woken.swap(true, Ordering::AcqRel)
        {
            waker();
        }
    }

    fn stop_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

/// Читает команды редактора до EOF. Stop не завершает чтение: EOF после него тоже взводит
/// сторожевой таймер, иначе игра, которую редактор уже закрыл, может остаться висеть.
fn read_commands(
    mut reader: impl BufRead,
    pending_stop: &PendingStop,
    start_sender: &mpsc::Sender<StartSignal>,
    on_parent_gone: impl FnOnce(),
) {
    let mut line = Vec::new();
    let mut start_sent = false;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(read) if read > 0 => {}
            _ => {
                pending_stop.request_stop();
                if !start_sent {
                    let _ = start_sender.send(StartSignal::ParentGone);
                }
                on_parent_gone();
                return;
            }
        }
        let line = String::from_utf8_lossy(&line);
        let line = line.strip_suffix('\n').unwrap_or(&line);
        match decode_editor_line(line) {
            Ok(Some(EditorMessage::Start { password })) if !start_sent => {
                start_sent = true;
                let _ = start_sender.send(StartSignal::Started(password));
            }
            Ok(Some(EditorMessage::Start { .. })) => {}
            Ok(Some(EditorMessage::Stop)) => {
                pending_stop.request_stop();
                if !start_sent {
                    start_sent = true;
                    let _ = start_sender.send(StartSignal::Stopped);
                }
            }
            Ok(None) | Err(_) => {}
        }
    }
}

/// Запускает поток, который через `grace` завершает процесс с кодом 1. `wait` и `exit` подставляются
/// тестами: им не нужны настоящие секунды и настоящий выход из процесса.
fn arm_parent_gone_watchdog(
    grace: Duration,
    wait: impl FnOnce(Duration) + Send + 'static,
    exit: impl FnOnce(i32) + Send + 'static,
) {
    let spawned = thread::Builder::new()
        .name("rve-editor-watchdog".to_owned())
        .spawn(move || {
            wait(grace);
            exit(1);
        });
    if spawned.is_err() {
        // Без сторожевого потока игра может пережить редактор; завершаемся сразу.
        process::exit(1);
    }
}

pub(crate) fn write_log_line(line: &ConsoleLine) {
    let level = match line.level {
        ConsoleLevel::Info => PlayerLogLevel::Info,
        ConsoleLevel::Warn => PlayerLogLevel::Warn,
        ConsoleLevel::Error => PlayerLogLevel::Error,
    };
    write_message(&PlayerMessage::Log {
        level,
        target: line.source.clone(),
        text: line.text.clone(),
    });
}

fn write_message(message: &PlayerMessage) {
    let encoded = encode_player_message(message);
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(encoded.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::{
        PARENT_EXIT_GRACE, PendingStop, StartSignal, arm_parent_gone_watchdog, read_commands,
    };
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn start_stop_and_garbage_commands_are_handled_on_stdin() {
        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        let mut armed = false;
        read_commands(
            Cursor::new(b"garbage\nstart\tpa\\nss\nstop\n"),
            &pending,
            &sender,
            || armed = true,
        );
        assert!(matches!(
            receiver.recv().unwrap(),
            StartSignal::Started(Some(password)) if password == "pa\nss"
        ));
        assert!(pending.stop_requested());
        assert!(receiver.try_recv().is_err());
        assert!(armed);

        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        read_commands(
            Cursor::new(b"bad\\escape\nstop\n"),
            &pending,
            &sender,
            || {},
        );
        assert!(matches!(receiver.recv().unwrap(), StartSignal::Stopped));
        assert!(pending.stop_requested());
    }

    #[test]
    fn stop_does_not_end_reading_so_eof_after_stop_arms_the_watchdog() {
        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        let mut armed = false;
        read_commands(Cursor::new(b"start\nstop\n"), &pending, &sender, || {
            armed = true
        });
        assert!(matches!(
            receiver.recv().unwrap(),
            StartSignal::Started(None)
        ));
        assert!(pending.stop_requested());
        assert!(armed, "EOF after stop must reach the watchdog");
    }

    #[test]
    fn stop_before_start_is_reported_and_eof_still_arms_the_watchdog() {
        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        let mut armed = false;
        read_commands(Cursor::new(b"stop\nstart\n"), &pending, &sender, || {
            armed = true
        });
        assert!(matches!(receiver.recv().unwrap(), StartSignal::Stopped));
        assert!(receiver.try_recv().is_err());
        assert!(pending.stop_requested());
        assert!(armed);
    }

    #[test]
    fn eof_before_and_after_start_requests_stop_and_arms_the_watchdog() {
        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        let mut armed = false;
        read_commands(Cursor::new(b""), &pending, &sender, || armed = true);
        assert!(matches!(receiver.recv().unwrap(), StartSignal::ParentGone));
        assert!(pending.stop_requested());
        assert!(armed);

        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        let mut armed = false;
        read_commands(Cursor::new(b"start\n"), &pending, &sender, || armed = true);
        assert!(matches!(
            receiver.recv().unwrap(),
            StartSignal::Started(None)
        ));
        assert!(pending.stop_requested());
        assert!(armed);
    }

    #[test]
    fn watchdog_waits_for_grace_and_then_exits_with_failure_code() {
        enum Event {
            Waited(Duration),
            Exit(i32),
        }
        let (events, received) = mpsc::channel();
        let wait_events = events.clone();
        let exit_events = events;
        arm_parent_gone_watchdog(
            PARENT_EXIT_GRACE,
            move |duration| {
                let _ = wait_events.send(Event::Waited(duration));
            },
            move |code| {
                let _ = exit_events.send(Event::Exit(code));
            },
        );
        assert!(matches!(
            received.recv().unwrap(),
            Event::Waited(duration) if duration == PARENT_EXIT_GRACE
        ));
        assert!(matches!(received.recv().unwrap(), Event::Exit(1)));
    }

    #[test]
    fn pending_stop_wakes_once_when_proxy_is_attached_before_or_after_stop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let before_attach = PendingStop::default();
        let before_count = Arc::new(AtomicUsize::new(0));
        before_attach.request_stop();
        let wake_count = Arc::clone(&before_count);
        before_attach.attach_waker(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        before_attach.request_stop();
        assert_eq!(before_count.load(Ordering::Relaxed), 1);

        let after_attach = PendingStop::default();
        let after_count = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&after_count);
        after_attach.attach_waker(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        after_attach.request_stop();
        after_attach.request_stop();
        assert_eq!(after_count.load(Ordering::Relaxed), 1);
    }
}
