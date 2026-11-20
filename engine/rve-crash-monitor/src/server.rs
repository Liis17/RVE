//! Сервер minidump: принимает одного клиента и пишет один дамп. Выход: после дампа, после
//! отключения клиента, при смерти процесса игры или если клиент так и не подключился.

use minidumper::{LoopAction, MinidumpBinary, Server, ServerHandler, SocketName};
use rve_crash_monitor::{
    MonitorArgs, RETAINED_CRASH_FILES, RETENTION_GRACE, create_dump_file, prune_crash_files,
};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Если клиент не подключился за это время, монитор выходит.
const CLIENT_DEADLINE: Duration = Duration::from_secs(10);
const CLIENT_POLL: Duration = Duration::from_millis(50);

pub(crate) fn run(args: MonitorArgs) -> ExitCode {
    let mut server = match Server::with_name(SocketName::Path(&args.socket)) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("rve-crash-monitor: не удалось открыть сокет: {error}");
            return ExitCode::from(1);
        }
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let clients = Arc::new(AtomicUsize::new(0));
    watch_parent(args.parent, Arc::clone(&shutdown));
    watch_first_client(Arc::clone(&clients), Arc::clone(&shutdown));
    let handler = Handler {
        dir: args.dir.clone(),
        parent: args.parent,
        clients,
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
    clients: Arc<AtomicUsize>,
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
            let _ = prune_crash_files(
                &self.dir,
                own,
                RETAINED_CRASH_FILES,
                SystemTime::now(),
                RETENTION_GRACE,
            );
        }
        LoopAction::Exit
    }

    fn on_message(&self, _kind: u32, _buffer: Vec<u8>) {}

    /// Обслуживается только первый клиент. Второе подключение к приватному сокету считается
    /// чужим и останавливает монитор: библиотека не даёт отказать одному клиенту и принять другого.
    fn on_client_connected(&self, _clients: usize) -> LoopAction {
        if self.clients.fetch_add(1, Ordering::SeqCst) == 0 {
            LoopAction::Continue
        } else {
            LoopAction::Exit
        }
    }

    fn on_client_disconnected(&self, _clients: usize) -> LoopAction {
        LoopAction::Exit
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn watch_first_client(clients: Arc<AtomicUsize>, shutdown: Arc<AtomicBool>) {
    thread::spawn(move || {
        let deadline = Instant::now() + CLIENT_DEADLINE;
        while clients.load(Ordering::SeqCst) == 0 {
            if Instant::now() >= deadline {
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
            thread::sleep(CLIENT_POLL);
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
