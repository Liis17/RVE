//! Запрос дампа через настоящий монитор. `CrashHandler` подключается один раз на тестовый бинарник,
//! поэтому каждый сценарий с обработчиком живёт в своём файле.

#![cfg(any(all(windows, target_arch = "x86_64"), target_os = "macos"))]

mod common;

use common::{connect, dump_files, hello, ipc_socket, scratch_dir, spawn_monitor, wait_for_exit};
use crash_handler::{CrashContext, CrashEventResult, CrashHandler, make_crash_event};
use std::fs;

#[test]
fn simulated_crash_writes_a_minidump_and_the_monitor_exits() {
    let dir = scratch_dir("dump");
    let socket = ipc_socket(&dir);
    let monitor = spawn_monitor(&socket, &dir, &[]);
    let client = connect(&socket);
    hello(&client);

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
