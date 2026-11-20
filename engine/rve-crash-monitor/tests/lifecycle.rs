//! Выход монитора без дампа: нет установленного клиента за отведённое время, отключился установленный
//! клиент, или умер родитель. Все ожидания — дедлайнами через `common::wait_for_exit` и чтение до EOF, без sleep.

#![cfg(any(all(windows, target_arch = "x86_64"), target_os = "macos"))]

mod common;

use common::{
    EXIT_DEADLINE, connect, hello, ipc_socket, monitor_command, scratch_dir, spawn_monitor,
    wait_for_exit,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;

/// Переменная, по которой процесс-«родитель» (тот же тестовый бинарник) включает свою роль.
const PARENT_ROLE_ENV: &str = "RVE_CRASH_MONITOR_TEST_PARENT_DIR";

#[test]
fn monitor_exits_when_no_client_says_hello_in_time() {
    let dir = scratch_dir("no-client");
    let socket = ipc_socket(&dir);
    // Короткое время ожидания: тест завершается по выходу монитора, а не по таймауту теста.
    let monitor = spawn_monitor(&socket, &dir, &["--client-timeout-ms", "200"]);

    let status = wait_for_exit(monitor);
    assert!(status.success(), "монитор завершился с ошибкой: {status}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Точка входа процесса-«родителя». Без переменной окружения — no-op, как `panic_child` в crash.rs.
/// Запускает монитор с собственным pid и ждёт EOF на stdin: закрытие stdin для теста означает смерть
/// родителя. Монитор наследует stdout, поэтому канал держится открытым до собственного выхода.
#[test]
#[allow(clippy::zombie_processes)]
fn parent_role_spawns_monitor_until_stdin_closes() {
    let Some(dir) = std::env::var_os(PARENT_ROLE_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let socket = ipc_socket(&dir);
    let mut command = monitor_command(&socket, &dir, std::process::id());
    command
        .args(["--client-timeout-ms", "600000"])
        .stdout(Stdio::inherit());
    // Дескриптор намеренно не ждётся: монитор переживает родителя ровно до своего выхода.
    let _monitor = command.spawn().expect("монитор запускается из родителя");
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
}

fn spawn_parent(dir: &Path, stdin: Stdio) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "parent_role_spawns_monitor_until_stdin_closes",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PARENT_ROLE_ENV, dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(stdin)
        .spawn()
        .expect("родитель запускается")
}

/// Читает вывод родителя до EOF в отдельном потоке. EOF наступает, когда и родитель, и монитор
/// (унаследовавший stdout) закрыли канал, поэтому ожидание ограничено `EXIT_DEADLINE`.
fn wait_for_eof(mut output: ChildStdout) {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut sink = Vec::new();
        let result = output.read_to_end(&mut sink);
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(EXIT_DEADLINE)
        .expect("монитор вышел в срок")
        .expect("чтение вывода родителя не удалось");
}

#[test]
fn established_client_disconnect_without_a_dump_stops_the_monitor() {
    let dir = scratch_dir("disconnect");
    let socket = ipc_socket(&dir);
    let monitor = spawn_monitor(&socket, &dir, &[]);
    let client = connect(&socket);
    hello(&client);
    drop(client);

    let status = wait_for_exit(monitor);
    assert!(status.success(), "монитор завершился с ошибкой: {status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn monitor_exits_when_its_parent_process_exits() {
    let dir = scratch_dir("parent-death");
    let mut parent = spawn_parent(&dir, Stdio::null());
    let output = parent.stdout.take().unwrap();

    wait_for_eof(output);
    let parent_status = wait_for_exit(parent);
    assert!(parent_status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Подключение без hello остаётся открытым. Установленный клиент отключается, но остаётся открытым
/// ещё один клиент, поэтому монитор не должен выйти: это проверяет, что выход по отключению требует
/// отсутствия других подключений. Монитор доступен для подключения после отключения установленного
/// клиента. Затем умирает родитель, и монитор выходит, хотя подключения ещё открыты.
#[test]
fn monitor_stays_up_while_other_connections_remain_and_exits_with_its_parent() {
    let dir = scratch_dir("other-connections");
    let socket = ipc_socket(&dir);
    let mut parent = spawn_parent(&dir, Stdio::piped());
    let parent_stdin: ChildStdin = parent.stdin.take().unwrap();
    let output = parent.stdout.take().unwrap();

    let early = connect(&socket);
    let established = connect(&socket);
    hello(&established);
    drop(established);
    // Монитор ещё жив: принимает новое подключение после отключения установленного клиента.
    let late = connect(&socket);

    drop(parent_stdin);
    wait_for_eof(output);
    let parent_status = wait_for_exit(parent);
    assert!(parent_status.success());
    drop((early, late));
    let _ = std::fs::remove_dir_all(&dir);
}
