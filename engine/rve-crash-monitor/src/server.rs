//! Сервер minidump. Монитор обслуживает запросы дампа от любого подключения, но останавливается,
//! когда установленный клиент (тот, что прислал hello) отключился. Выход также при записи дампа,
//! смерти процесса игры и если установленного клиента не было за отведённое время.

use minidumper::{LoopAction, MinidumpBinary, Server, ServerHandler, SocketName};
use rve_crash_monitor::{HELLO_MESSAGE_KIND, MonitorArgs, create_dump_file, prune_after_dump};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const ESTABLISHED_POLL: Duration = Duration::from_millis(50);

pub(crate) fn run(args: MonitorArgs) -> ExitCode {
    let mut server = match Server::with_name(SocketName::Path(&args.socket)) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("rve-crash-monitor: не удалось открыть сокет: {error}");
            return ExitCode::from(1);
        }
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let established = Arc::new(AtomicBool::new(false));
    watch_parent(args.parent, Arc::clone(&shutdown));
    watch_established(
        Arc::clone(&established),
        Arc::clone(&shutdown),
        args.client_timeout,
    );
    let handler = Handler {
        dir: args.dir.clone(),
        parent: args.parent,
        established,
        dump: Mutex::new(None),
    };
    let result = server.run(Box::new(handler), &shutdown, None);
    // Удаляет сокет и пустой каталог IPC, который создала игра.
    drop(server);
    if let Some(ipc_dir) = args.socket.parent() {
        let _ = fs::remove_dir(ipc_dir);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rve-crash-monitor: ошибка цикла сервера: {error}");
            ExitCode::from(1)
        }
    }
}

struct Handler {
    dir: PathBuf,
    parent: u32,
    /// Установлен, когда клиент прислал hello. Только отключение установленного клиента
    /// останавливает монитор; ранние и посторонние подключения этого не делают.
    established: Arc<AtomicBool>,
    /// Путь дампа, который сейчас пишется.
    dump: Mutex<Option<PathBuf>>,
}

impl ServerHandler for Handler {
    fn create_minidump_file(&self) -> io::Result<(File, PathBuf)> {
        let (file, path) = create_dump_file(&self.dir, self.parent, SystemTime::now())?;
        *lock(&self.dump) = Some(path.clone());
        Ok((file, path))
    }

    fn on_minidump_created(&self, result: Result<MinidumpBinary, minidumper::Error>) -> LoopAction {
        let dump = lock(&self.dump).take();
        match result {
            Ok(mut binary) => {
                let _ = binary.file.flush();
            }
            Err(error) => {
                eprintln!("rve-crash-monitor: дамп не записан: {error}");
                if let Some(path) = &dump {
                    let _ = fs::remove_file(path);
                }
            }
        }
        if let Some(own) = &dump {
            let _ = prune_after_dump(&self.dir, own, SystemTime::now());
        }
        LoopAction::Exit
    }

    fn on_message(&self, kind: u32, _buffer: Vec<u8>) {
        if kind == HELLO_MESSAGE_KIND {
            self.established.store(true, Ordering::SeqCst);
        }
    }

    /// Подключение не останавливает монитор: клиенты, не приславшие hello, учитываются только при отключении.
    /// Библиотека не даёт отказать подключению, поэтому запрос дампа от такого клиента тоже будет обработан.
    fn on_client_connected(&self, _clients: usize) -> LoopAction {
        LoopAction::Continue
    }

    /// Выход только когда установленный клиент отключился и других подключений не осталось.
    /// Если посторонний клиент держит сокет открытым, остановит монитор выход родителя.
    fn on_client_disconnected(&self, clients: usize) -> LoopAction {
        if self.established.load(Ordering::SeqCst) && clients == 0 {
            LoopAction::Exit
        } else {
            LoopAction::Continue
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Если за `timeout` клиент не прислал hello, монитор останавливается.
fn watch_established(established: Arc<AtomicBool>, shutdown: Arc<AtomicBool>, timeout: Duration) {
    thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        while !established.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
            thread::sleep(ESTABLISHED_POLL);
        }
    });
}

#[cfg(windows)]
fn watch_parent(parent: u32, shutdown: Arc<AtomicBool>) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{INFINITE, OpenProcess, WaitForSingleObject};
    thread::spawn(move || {
        // SAFETY: дескриптор открыт с правом SYNCHRONIZE, ожидается и закрывается в этом потоке.
        unsafe {
            let handle = OpenProcess(SYNCHRONIZE, 0, parent);
            if !handle.is_null() {
                WaitForSingleObject(handle, INFINITE);
                CloseHandle(handle);
            }
        }
        shutdown.store(true, Ordering::SeqCst);
    });
}

#[cfg(target_os = "macos")]
const PARENT_POLL: Duration = Duration::from_secs(1);

#[cfg(target_os = "macos")]
fn watch_parent(parent: u32, shutdown: Arc<AtomicBool>) {
    thread::spawn(move || {
        loop {
            // SAFETY: getppid не имеет предусловий.
            let current = unsafe { libc::getppid() };
            if current != i32::try_from(parent).unwrap_or(-1) {
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
            thread::sleep(PARENT_POLL);
        }
    });
}
