//! Выход монитора без дампа: нет установленного клиента за отведённое время, или умер родитель.
//! Все ожидания — дедлайнами через `common::wait_for_exit` и чтение до EOF, без sleep.

#![cfg(any(all(windows, target_arch = "x86_64"), target_os = "macos"))]

mod common;

use common::{
    EXIT_DEADLINE, connect, hello, ipc_socket, monitor_command, scratch_dir, spawn_monitor,
    wait_for_exit,
};
use std::io::Read;
use std::process::{Command, Stdio};
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

/// Роль родителя: запускает монитор с собственным pid, затем сразу выходит.
/// Монитор наследует stdout и потому держит канал открытым до собственного выхода.
#[test]
#[allow(clippy::zombie_processes)]
fn parent_role_spawns_monitor_and_exits() {
    let Some(dir) = std::env::var_os(PARENT_ROLE_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let socket = ipc_socket(&dir);
    let mut command = monitor_command(&socket, &dir, std::process::id());
    command
        .args(["--client-timeout-ms", "600000"])
        .stdout(Stdio::inherit());
    // Дескриптор намеренно не ждётся: родитель выходит сразу, монитор переживает его ровно до своего выхода.
    let _monitor = command.spawn().expect("монитор запускается из родителя");
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
    let mut parent = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "parent_role_spawns_monitor_and_exits",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PARENT_ROLE_ENV, &dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let mut output = parent.stdout.take().unwrap();

    // Чтение до EOF: канал закрывается, когда родитель и монитор (унаследовавший stdout) выйдут.
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut sink = Vec::new();
        let result = output.read_to_end(&mut sink);
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(EXIT_DEADLINE)
        .expect("монитор вышел вместе с родителем в срок")
        .expect("чтение вывода родителя не удалось");
    let parent_status = wait_for_exit(parent);
    assert!(parent_status.success());
    let _ = std::fs::remove_dir_all(&dir);
}
