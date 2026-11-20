//! Подключение без hello и его отключение не останавливают монитор; следующий клиент получает дамп.
//! Отдельный бинарник: `CrashHandler` подключается один раз на тестовый бинарник.

#![cfg(any(all(windows, target_arch = "x86_64"), target_os = "macos"))]

mod common;

use common::{connect, dump_files, hello, ipc_socket, scratch_dir, spawn_monitor, wait_for_exit};
use crash_handler::{CrashContext, CrashEventResult, CrashHandler, make_crash_event};
use std::fs;

#[test]
fn connection_without_hello_is_ignored_and_a_later_client_still_gets_a_dump() {
    let dir = scratch_dir("early");
    let socket = ipc_socket(&dir);
    let monitor = spawn_monitor(&socket, &dir, &[]);
    drop(connect(&socket));

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
    assert_eq!(dump_files(&dir).len(), 1);
    let _ = fs::remove_dir_all(&dir);
}
