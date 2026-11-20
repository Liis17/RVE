//! Интеграция с настоящим монитором: клиент и `CrashHandler` живут в тестовом процессе, монитор — в дочернем.
//! Ожидания ограничены `recv_timeout`, без sleep.

#![cfg(any(all(windows, target_arch = "x86_64"), target_os = "macos"))]

use crash_handler::{CrashContext, CrashEventResult, CrashHandler, make_crash_event};
use minidumper::{Client, SocketName};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(60);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rve-crash-monitor-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Сокет в подкаталоге IPC, как у игры: монитор удаляет и сокет, и пустой подкаталог.
fn ipc_socket(dir: &Path) -> PathBuf {
    let ipc = dir.join("ipc");
    fs::create_dir_all(&ipc).unwrap();
    ipc.join("s")
}

fn spawn_monitor(socket: &Path, dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rve-crash-monitor"))
        .arg("--socket")
        .arg(socket)
        .arg("--dir")
        .arg(dir)
        .arg("--parent")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("монитор запускается")
}

/// Подключается, пока сервер монитора не откроет сокет. Ожидание ограничено `DEADLINE`.
fn connect(socket: &Path) -> Client {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match Client::with_name(SocketName::Path(socket)) {
            Ok(client) => return client,
            Err(error) => {
                assert!(Instant::now() < deadline, "клиент не подключился: {error}");
                thread::yield_now();
            }
        }
    }
}

/// Ждёт завершения монитора в отдельном потоке: тест не виснет, если монитор не вышел.
fn wait_for_exit(child: Child) -> ExitStatus {
    let (sender, receiver) = mpsc::channel();
    let mut child = child;
    thread::spawn(move || {
        let _ = sender.send(child.wait());
    });
    receiver
        .recv_timeout(DEADLINE)
        .expect("монитор завершился в срок")
        .expect("ожидание монитора не удалось")
}

fn dump_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "dmp"))
        .collect()
}

#[test]
fn simulated_crash_writes_a_minidump_and_the_monitor_exits() {
    let dir = scratch_dir("dump");
    let socket = ipc_socket(&dir);
    let monitor = spawn_monitor(&socket, &dir);
    let client = connect(&socket);

    // Единственный CrashHandler этого тестового бинарника: второй attach вернёт ошибку.
    // SAFETY: обработчик только отправляет запрос клиенту и возвращает управление.
    let handler = unsafe {
        CrashHandler::attach(make_crash_event(move |context: &CrashContext| {
            let _ = client.request_dump(context);
            CrashEventResult::Handled(false)
        }))
    }
    .expect("обработчик подключается");
    let _ = handler.simulate_exception(None);

    let status = wait_for_exit(monitor);
    assert!(status.success(), "монитор завершился с ошибкой: {status}");
    let dumps = dump_files(&dir);
    assert_eq!(dumps.len(), 1, "{dumps:?}");
    assert!(fs::metadata(&dumps[0]).unwrap().len() > 0, "дамп пуст");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn client_disconnect_without_a_dump_stops_the_monitor() {
    let dir = scratch_dir("disconnect");
    let socket = ipc_socket(&dir);
    let monitor = spawn_monitor(&socket, &dir);
    let client = connect(&socket);
    drop(client);

    let status = wait_for_exit(monitor);
    assert!(status.success(), "монитор завершился с ошибкой: {status}");
    assert!(dump_files(&dir).is_empty());
    let _ = fs::remove_dir_all(&dir);
}
