//! Монитор падений: отдельный процесс, который по запросу игры пишет minidump и сразу выходит.
//! Запускает его `rve-player`/`rve-server`; без поддерживаемой платформы монитор сообщает об ошибке.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

#[cfg(all(
    feature = "server",
    any(all(windows, target_arch = "x86_64"), target_os = "macos")
))]
mod server;

fn main() -> ExitCode {
    let args = match rve_crash_monitor::parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("Ошибка аргументов rve-crash-monitor: {error}");
            return ExitCode::from(2);
        }
    };
    run(args)
}

#[cfg(all(
    feature = "server",
    any(all(windows, target_arch = "x86_64"), target_os = "macos")
))]
fn run(args: rve_crash_monitor::MonitorArgs) -> ExitCode {
    server::run(args)
}

#[cfg(not(all(
    feature = "server",
    any(all(windows, target_arch = "x86_64"), target_os = "macos")
)))]
fn run(_args: rve_crash_monitor::MonitorArgs) -> ExitCode {
    eprintln!("rve-crash-monitor поддерживает только Windows x86_64 и macOS");
    ExitCode::from(2)
}
