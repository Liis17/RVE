//! Общие помощники интеграционных тестов монитора. Ожидания ограничены дедлайнами, без sleep.
#![allow(dead_code)]

use minidumper::{Client, SocketName};
use rve_crash_monitor::HELLO_MESSAGE_KIND;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Сколько ждать выхода монитора.
pub const EXIT_DEADLINE: Duration = Duration::from_secs(60);
/// Сколько ждать готовности сервера монитора к подключению.
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(60);

pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rve-crash-monitor-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Сокет в подкаталоге IPC, как у игры: монитор удаляет и сокет, и пустой подкаталог.
pub fn ipc_socket(dir: &Path) -> PathBuf {
    let ipc = dir.join("ipc");
    fs::create_dir_all(&ipc).unwrap();
    ipc.join("s")
}

pub fn monitor_command(socket: &Path, dir: &Path, parent: u32) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rve-crash-monitor"));
    command
        .arg("--socket")
        .arg(socket)
        .arg("--dir")
        .arg(dir)
        .arg("--parent")
        .arg(parent.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub fn spawn_monitor(socket: &Path, dir: &Path, extra_args: &[&str]) -> Child {
    monitor_command(socket, dir, std::process::id())
        .args(extra_args)
        .spawn()
        .expect("монитор запускается")
}

/// Подключается, пока сервер монитора не откроет сокет. Ожидание ограничено `CONNECT_DEADLINE`.
pub fn connect(socket: &Path) -> Client {
    let deadline = Instant::now() + CONNECT_DEADLINE;
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

/// Представляется монитору, как клиент игры.
pub fn hello(client: &Client) {
    client
        .send_message(HELLO_MESSAGE_KIND, [])
        .expect("приветствие отправляется");
}

/// Ждёт выхода монитора в отдельном потоке: тест не виснет, если монитор не вышел.
pub fn wait_for_exit(child: Child) -> ExitStatus {
    let (sender, receiver) = mpsc::channel();
    let mut child = child;
    thread::spawn(move || {
        let _ = sender.send(child.wait());
    });
    receiver
        .recv_timeout(EXIT_DEADLINE)
        .expect("монитор завершился в срок")
        .expect("ожидание монитора не удалось")
}

pub fn dump_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "dmp"))
        .collect()
}
