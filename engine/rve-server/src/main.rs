use std::process::ExitCode;

// Консольная подсистема на всех платформах: сервер читает stdin и пишет ошибки в stderr.
fn main() -> ExitCode {
    engine_runtime::run_server()
}
