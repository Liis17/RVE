//! Minidump для нативных падений. Игра запускает `rve-crash-monitor` рядом с собой и подключается к нему
//! в фоне; при падении обработчик просит монитор записать дамп. Старт игры подключение не ждёт.
//! Любая ошибка — предупреждение `[crash]`, игра продолжает работу без дампов.

use crash_handler::{CrashContext, CrashEventResult, CrashHandler, make_crash_event};
use minidumper::{Client, SocketName};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const MONITOR_FILE_NAME: &str = if cfg!(windows) {
    "rve-crash-monitor.exe"
} else {
    "rve-crash-monitor"
};
const CONNECT_DEADLINE: Duration = Duration::from_secs(3);
const CONNECT_RETRY: Duration = Duration::from_millis(10);
const IPC_DIR_PREFIX: &str = "rve-crash-";
const SOCKET_NAME: &str = "s";
/// Место под путь в `sockaddr_un` вместе с завершающим NUL.
const SOCKET_PATH_CAPACITY: usize = if cfg!(target_os = "macos") { 104 } else { 108 };

/// Живёт до конца процесса: если его сбросить, обработчик снимется и монитор потеряет клиента.
struct CrashDumps {
    _handler: CrashHandler,
    _monitor: Child,
}

static DUMPS: OnceLock<CrashDumps> = OnceLock::new();

/// Запускает монитор в каталоге `exe_dir` и подключает обработчик падений; дампы пишутся в `logs_dir`.
/// `logs_dir` — `None`, если каталога отчётов нет: тогда дампы не пишутся.
pub(crate) fn start(exe_dir: &Path, logs_dir: Option<&Path>) {
    let Some(logs_dir) = logs_dir else {
        log::warn!("[crash] каталог отчётов недоступен: minidump не пишется");
        return;
    };
    let monitor = match monitor_path(exe_dir) {
        Ok(monitor) => monitor,
        Err(reason) => {
            log::warn!("[crash] {reason}: minidump не пишется");
            return;
        }
    };
    let ipc_dir = match create_ipc_dir(&std::env::temp_dir()) {
        Ok(ipc_dir) => ipc_dir,
        Err(error) => {
            log::warn!("[crash] каталог IPC не создан ({error}): minidump не пишется");
            return;
        }
    };
    let socket = ipc_dir.join(SOCKET_NAME);
    let monitor_child = match spawn_monitor(&monitor, &socket, logs_dir) {
        Ok(child) => child,
        Err(error) => {
            log::warn!(
                "[crash] не удалось запустить {}: {error}",
                monitor.display()
            );
            remove_ipc_dir(&ipc_dir);
            return;
        }
    };
    let spawned = thread::Builder::new()
        .name("rve-crash-monitor-client".into())
        .spawn(move || attach(&socket, monitor_child, &ipc_dir));
    if let Err(error) = spawned {
        log::warn!("[crash] поток подключения не запущен ({error}): minidump не пишется");
    }
}

fn attach(socket: &Path, mut monitor: Child, ipc_dir: &Path) {
    match connect(socket, &mut monitor) {
        Ok(client) => {
            // SAFETY: обработчик лишь передаёт контекст сбоя монитору и возвращает управление.
            let event = unsafe {
                make_crash_event(move |context: &CrashContext| {
                    let _ = client.request_dump(context);
                    // Прежний обработчик ОС (WER, ReportCrash) остаётся в силе.
                    CrashEventResult::Handled(false)
                })
            };
            match CrashHandler::attach(event) {
                Ok(handler) => {
                    let _ = DUMPS.set(CrashDumps {
                        _handler: handler,
                        _monitor: monitor,
                    });
                }
                Err(error) => give_up(
                    monitor,
                    ipc_dir,
                    &format!("обработчик не подключён: {error}"),
                ),
            }
        }
        Err(reason) => give_up(monitor, ipc_dir, &reason),
    }
}

/// Подключается к монитору, пока тот не откроет сокет. Ждёт не дольше `CONNECT_DEADLINE`.
fn connect(socket: &Path, monitor: &mut Child) -> Result<Client, String> {
    let deadline = Instant::now() + CONNECT_DEADLINE;
    loop {
        match Client::with_name(SocketName::Path(socket)) {
            Ok(client) => return Ok(client),
            Err(error) => {
                if let Ok(Some(status)) = monitor.try_wait() {
                    return Err(format!("монитор завершился ({status})"));
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "монитор не открыл сокет за {}с: {error}",
                        CONNECT_DEADLINE.as_secs()
                    ));
                }
                thread::sleep(CONNECT_RETRY);
            }
        }
    }
}

fn give_up(mut monitor: Child, ipc_dir: &Path, reason: &str) {
    log::warn!("[crash] {reason}: minidump не пишется");
    let _ = monitor.kill();
    let _ = monitor.wait();
    remove_ipc_dir(ipc_dir);
}

fn remove_ipc_dir(ipc_dir: &Path) {
    let _ = fs::remove_dir_all(ipc_dir);
}

fn monitor_path(exe_dir: &Path) -> Result<PathBuf, String> {
    let path = exe_dir.join(MONITOR_FILE_NAME);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{} не найден", path.display()))
    }
}

fn spawn_monitor(monitor: &Path, socket: &Path, logs_dir: &Path) -> io::Result<Child> {
    Command::new(monitor)
        .arg("--socket")
        .arg(socket)
        .arg("--dir")
        .arg(logs_dir)
        .arg("--parent")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Создаёт приватный каталог `rve-crash-<16 hex>` в `temp`; на Unix доступ только владельцу.
fn create_ipc_dir(temp: &Path) -> io::Result<PathBuf> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let dir = temp.join(ipc_dir_name(random));
    if !socket_path_fits(&dir.join(SOCKET_NAME)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "путь сокета длиннее лимита sockaddr_un",
        ));
    }
    create_private_dir(&dir)?;
    Ok(dir)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(dir)
    }
}

fn ipc_dir_name(random: [u8; 8]) -> String {
    let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{IPC_DIR_PREFIX}{hex}")
}

fn socket_path_fits(path: &Path) -> bool {
    path.as_os_str().len() < SOCKET_PATH_CAPACITY
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rve-crash-dumps-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ipc_directory_name_is_prefixed_and_sixteen_hex_digits() {
        let name = ipc_dir_name([0x00, 0x1f, 0xab, 0xff, 0x10, 0x02, 0x7e, 0x80]);
        assert_eq!(name, "rve-crash-001fabff10027e80");
    }

    #[test]
    fn socket_path_must_stay_below_the_sockaddr_un_capacity() {
        let short = Path::new("C:/Temp/rve-crash-0000000000000000/s");
        assert!(socket_path_fits(short));
        let long = PathBuf::from("x".repeat(SOCKET_PATH_CAPACITY));
        assert!(!socket_path_fits(&long));
    }

    #[test]
    fn ipc_directory_is_created_fresh_and_private() {
        let temp = scratch_dir("ipc");
        let first = create_ipc_dir(&temp).unwrap();
        let second = create_ipc_dir(&temp).unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(IPC_DIR_PREFIX)
        );
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn missing_monitor_is_reported_with_its_expected_path() {
        let exe_dir = scratch_dir("no-monitor");
        let error = monitor_path(&exe_dir).unwrap_err();
        assert!(error.contains(MONITOR_FILE_NAME), "{error}");
        assert!(error.contains("не найден"), "{error}");

        fs::write(exe_dir.join(MONITOR_FILE_NAME), b"monitor").unwrap();
        assert_eq!(
            monitor_path(&exe_dir).unwrap(),
            exe_dir.join(MONITOR_FILE_NAME)
        );
        let _ = fs::remove_dir_all(&exe_dir);
    }
}
