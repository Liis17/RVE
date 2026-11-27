use engine_console::{ConsoleLevel, ConsoleLine};
use rve_play_ipc::{
    EditorMessage, PROTOCOL_VERSION, PlayerLogLevel, PlayerMessage, PlayerStatus,
    decode_editor_line, encode_player_message,
};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

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
                read_commands(stdin.lock(), &thread_stop, &start_sender);
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

    pub(crate) fn emergency_deadline(&self) -> Option<Instant> {
        self.pending_stop.emergency_deadline()
    }

    pub(crate) fn emergency_exit_due(&self, now: Instant) -> bool {
        self.pending_stop.emergency_exit_due(now)
    }
}

pub(crate) struct PendingStop {
    requested: AtomicBool,
    woken: AtomicBool,
    emergency_deadline: Mutex<Option<Instant>>,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for PendingStop {
    fn default() -> Self {
        Self {
            requested: AtomicBool::new(false),
            woken: AtomicBool::new(false),
            emergency_deadline: Mutex::new(None),
            waker: Mutex::new(None),
        }
    }
}

impl PendingStop {
    fn request_stop_at(&self, parent_gone: bool, now: Instant) -> bool {
        if parent_gone {
            let mut deadline = self
                .emergency_deadline
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            deadline.get_or_insert(now + PARENT_EXIT_GRACE);
        }
        let first_request = !self.requested.swap(true, Ordering::AcqRel);
        if first_request {
            self.wake_if_requested();
        }
        first_request
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

    fn emergency_deadline(&self) -> Option<Instant> {
        *self
            .emergency_deadline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emergency_exit_due(&self, now: Instant) -> bool {
        self.emergency_deadline()
            .is_some_and(|deadline| now >= deadline)
    }
}

fn read_commands(
    mut reader: impl BufRead,
    pending_stop: &PendingStop,
    start_sender: &mpsc::Sender<StartSignal>,
) {
    let mut line = Vec::new();
    let mut start_sent = false;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(read) if read > 0 => {}
            _ => {
                pending_stop.request_stop_at(true, Instant::now());
                if !start_sent {
                    let _ = start_sender.send(StartSignal::ParentGone);
                }
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
                pending_stop.request_stop_at(false, Instant::now());
                if !start_sent {
                    let _ = start_sender.send(StartSignal::Stopped);
                }
                return;
            }
            Ok(None) | Err(_) => {}
        }
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
    use super::{PendingStop, StartSignal, read_commands};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    #[test]
    fn start_stop_and_garbage_commands_are_handled_on_stdin() {
        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        read_commands(
            Cursor::new(b"garbage\nstart\tpa\\nss\nstop\n"),
            &pending,
            &sender,
        );
        assert!(matches!(
            receiver.recv().unwrap(),
            StartSignal::Started(Some(password)) if password == "pa\nss"
        ));
        assert!(pending.stop_requested());
        assert!(receiver.try_recv().is_err());

        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        read_commands(Cursor::new(b"bad\\escape\nstop\n"), &pending, &sender);
        assert!(matches!(receiver.recv().unwrap(), StartSignal::Stopped));
        assert!(pending.stop_requested());
    }

    #[test]
    fn eof_before_and_after_start_requests_stop_and_sets_emergency_deadline() {
        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        read_commands(Cursor::new(b""), &pending, &sender);
        assert!(matches!(receiver.recv().unwrap(), StartSignal::ParentGone));
        assert!(pending.stop_requested());
        let deadline = pending.emergency_deadline().unwrap();
        assert!(!pending.emergency_exit_due(deadline - Duration::from_millis(1)));
        assert!(pending.emergency_exit_due(deadline));

        let pending = PendingStop::default();
        let (sender, receiver) = mpsc::channel();
        read_commands(Cursor::new(b"start\n"), &pending, &sender);
        assert!(matches!(
            receiver.recv().unwrap(),
            StartSignal::Started(None)
        ));
        assert!(pending.stop_requested());
        assert!(pending.emergency_deadline().is_some());
    }

    #[test]
    fn pending_stop_wakes_once_when_proxy_is_attached_before_or_after_stop() {
        let before_attach = PendingStop::default();
        let before_count = Arc::new(AtomicUsize::new(0));
        before_attach.request_stop_at(false, Instant::now());
        let wake_count = Arc::clone(&before_count);
        before_attach.attach_waker(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        before_attach.request_stop_at(false, Instant::now());
        assert_eq!(before_count.load(Ordering::Relaxed), 1);

        let after_attach = PendingStop::default();
        let after_count = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&after_count);
        after_attach.attach_waker(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        after_attach.request_stop_at(false, Instant::now());
        after_attach.request_stop_at(false, Instant::now());
        assert_eq!(after_count.load(Ordering::Relaxed), 1);
    }
}
