//! Сборка релиза проекта дочерним процессом `rve-build` с выводом в Console.
//!
//! `rve-build` запускается отдельным процессом, а не подключается библиотекой: он
//! ищет player рядом со своим exe, загружает cdylib скриптов в свой процесс, а паника
//! сборки не должна ронять редактор.

use crate::console::LogLevel;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// Способ запустить `rve-build`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BuildTool {
    /// Готовый `rve-build` рядом с exe редактора.
    Binary(PathBuf),
    /// Checkout workspace: `cargo run -p rve-build` из его корня.
    Cargo { cargo: OsString, workspace: PathBuf },
}

/// Корень workspace, из которого собран редактор.
pub fn checkout_workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Ищет `rve-build` рядом с exe редактора, затем в checkout workspace через cargo.
pub fn locate_build_tool(
    exe_dir: Option<&Path>,
    workspace: &Path,
    cargo: Option<OsString>,
) -> Result<BuildTool, String> {
    if let Some(dir) = exe_dir {
        let binary = dir.join(format!("rve-build{}", std::env::consts::EXE_SUFFIX));
        if binary.is_file() {
            return Ok(BuildTool::Binary(binary));
        }
    }
    if workspace.join("Cargo.toml").is_file() {
        return Ok(BuildTool::Cargo {
            cargo: cargo.unwrap_or_else(|| "cargo".into()),
            workspace: workspace.to_owned(),
        });
    }
    Err("rve-build не найден рядом с редактором и checkout workspace недоступен".to_owned())
}

impl BuildTool {
    /// Команда `build <project> --output <output>`; пути должны быть абсолютными,
    /// потому что cargo запускается из корня workspace.
    pub fn command(&self, project: &Path, output: &Path) -> Command {
        let mut command = match self {
            Self::Binary(binary) => Command::new(binary),
            Self::Cargo { cargo, workspace } => {
                let mut command = Command::new(cargo);
                // Из корня workspace cargo находит `.cargo/config.toml` и каталог `build/`.
                command
                    .current_dir(workspace)
                    .args(["run", "-p", "rve-build", "--locked", "--"]);
                command
            }
        };
        command
            .arg("build")
            .arg(project)
            .arg("--output")
            .arg(output);
        command
    }
}

/// Уровень строки stderr: ошибки `rve-build` и компилятора, предупреждения, прогресс cargo.
pub fn classify_stderr(line: &str) -> LogLevel {
    let line = line.trim_start();
    if line.starts_with("rve-build:") || line.starts_with("error") {
        LogLevel::Error
    } else if line.starts_with("warning") {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

/// Итоговое сообщение по коду завершения; `None` — процесс завершён сигналом.
pub fn finish_message(code: Option<i32>, output: &Path) -> (LogLevel, String) {
    match code {
        Some(0) => (
            LogLevel::Info,
            format!("Build completed: {}", output.display()),
        ),
        Some(2) => (
            LogLevel::Error,
            "Build failed: ошибка проекта или аргументов (код 2)".to_owned(),
        ),
        Some(1) => (
            LogLevel::Error,
            "Build failed: операционная ошибка (код 1)".to_owned(),
        ),
        Some(code) => (LogLevel::Error, format!("Build failed (код {code})")),
        None => (LogLevel::Error, "Build terminated".to_owned()),
    }
}

enum Message {
    Line(LogLevel, String),
    Eof,
}

/// Запущенная сборка; строки и итог забираются `poll` из цикла кадра.
pub struct BuildJob {
    child: Child,
    receiver: Receiver<Message>,
    open_streams: usize,
    output: PathBuf,
    finished: bool,
}

impl BuildJob {
    /// Запускает команду с перехваченными stdout и stderr; `output` попадает в итог.
    pub fn spawn(mut command: Command, output: PathBuf) -> std::io::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn()?;
        let (sender, receiver) = mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, sender.clone(), |_| LogLevel::Info);
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, sender, classify_stderr);
        }
        Ok(Self {
            child,
            receiver,
            open_streams: 2,
            output,
            finished: false,
        })
    }

    /// Новые строки по порядку внутри каждого потока; последней идёт итоговая строка,
    /// когда оба потока закрыты и процесс завершён.
    pub fn poll(&mut self) -> Vec<(LogLevel, String)> {
        let mut lines = Vec::new();
        if self.finished {
            return lines;
        }
        loop {
            match self.receiver.try_recv() {
                Ok(Message::Line(level, line)) => lines.push((level, line)),
                Ok(Message::Eof) => self.open_streams -= 1,
                Err(TryRecvError::Empty) => break,
                // Читатель завершился без Eof только при панике; считаем потоки закрытыми.
                Err(TryRecvError::Disconnected) => {
                    self.open_streams = 0;
                    break;
                }
            }
        }
        if self.open_streams == 0 {
            match self.child.try_wait() {
                Ok(Some(status)) => self.finish(Some(status), &mut lines),
                Ok(None) => {}
                Err(error) => {
                    lines.push((LogLevel::Error, format!("rve-build: {error}")));
                    self.finish(None, &mut lines);
                }
            }
        }
        lines
    }

    fn finish(&mut self, status: Option<ExitStatus>, lines: &mut Vec<(LogLevel, String)>) {
        self.finished = true;
        lines.push(finish_message(
            status.and_then(|status| status.code()),
            &self.output,
        ));
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Останавливает процесс. Потоки-читатели не ожидаются: на Windows внук cargo
    /// может пережить `kill` и держать каналы открытыми.
    pub fn kill(mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_reader(
    stream: impl Read + Send + 'static,
    sender: Sender<Message>,
    level: fn(&str) -> LogLevel,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buffer = Vec::new();
        // `read_until` вместо `lines`: вывод инструментов не обязан быть UTF-8.
        while matches!(reader.read_until(b'\n', &mut buffer), Ok(read) if read > 0) {
            let line = String::from_utf8_lossy(&buffer);
            let line = line.trim_end_matches(['\r', '\n']).to_owned();
            buffer.clear();
            if sender.send(Message::Line(level(&line), line)).is_err() {
                return;
            }
        }
        let _ = sender.send(Message::Eof);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const HELPER: &str = "RVE_EDITOR_BUILD_HELPER_EXIT";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rve-editor-build-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stderr_lines_are_classified_by_prefix() {
        assert_eq!(classify_stderr("rve-build: сцена"), LogLevel::Error);
        assert_eq!(classify_stderr("error[E0308]: mismatched"), LogLevel::Error);
        assert_eq!(classify_stderr("warning: unused"), LogLevel::Warn);
        assert_eq!(
            classify_stderr("   Compiling rve-build v0.1.0"),
            LogLevel::Info
        );
    }

    #[test]
    fn exit_codes_map_to_messages() {
        let output = Path::new("out");
        assert_eq!(finish_message(Some(0), output).0, LogLevel::Info);
        assert!(finish_message(Some(0), output).1.contains("out"));
        assert!(finish_message(Some(2), output).1.contains("код 2"));
        assert!(finish_message(Some(1), output).1.contains("код 1"));
        assert!(finish_message(Some(101), output).1.contains("101"));
        assert_eq!(finish_message(None, output).0, LogLevel::Error);
    }

    #[test]
    fn binary_next_to_the_exe_wins_over_cargo() {
        let exe_dir = TempDir::new();
        let workspace = TempDir::new();
        fs::write(workspace.0.join("Cargo.toml"), "").unwrap();
        let cargo = Some(OsString::from("my-cargo"));
        assert_eq!(
            locate_build_tool(Some(&exe_dir.0), &workspace.0, cargo.clone()).unwrap(),
            BuildTool::Cargo {
                cargo: "my-cargo".into(),
                workspace: workspace.0.clone(),
            }
        );
        let binary = exe_dir
            .0
            .join(format!("rve-build{}", std::env::consts::EXE_SUFFIX));
        fs::write(&binary, "").unwrap();
        assert_eq!(
            locate_build_tool(Some(&exe_dir.0), &workspace.0, cargo).unwrap(),
            BuildTool::Binary(binary)
        );
    }

    #[test]
    fn without_binary_and_checkout_the_tool_is_not_found() {
        let empty = TempDir::new();
        assert!(locate_build_tool(Some(&empty.0), &empty.0, None).is_err());
        let workspace = TempDir::new();
        fs::write(workspace.0.join("Cargo.toml"), "").unwrap();
        assert_eq!(
            locate_build_tool(None, &workspace.0, None).unwrap(),
            BuildTool::Cargo {
                cargo: "cargo".into(),
                workspace: workspace.0.clone(),
            }
        );
    }

    #[test]
    fn cargo_command_runs_from_the_workspace_root() {
        let tool = BuildTool::Cargo {
            cargo: "cargo".into(),
            workspace: PathBuf::from("ws"),
        };
        let command = tool.command(Path::new("/game"), Path::new("/out"));
        assert_eq!(command.get_current_dir(), Some(Path::new("ws")));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "-p",
                "rve-build",
                "--locked",
                "--",
                "build",
                "/game",
                "--output",
                "/out"
            ]
        );
    }

    /// Дочерний процесс для `job_reports_lines_in_order_and_the_exit_code`; без
    /// переменной окружения ничего не делает.
    #[test]
    fn helper_process() {
        let Ok(code) = std::env::var(HELPER) else {
            return;
        };
        println!("marker-out-1");
        println!("marker-out-2");
        eprintln!("marker-err warning");
        eprintln!("rve-build: marker-err failure");
        std::process::exit(code.parse().unwrap());
    }

    fn run_helper(code: i32) -> Vec<(LogLevel, String)> {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "build::tests::helper_process",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(HELPER, code.to_string());
        let mut job = BuildJob::spawn(command, PathBuf::from("out-dir")).unwrap();
        let mut lines = Vec::new();
        while !job.is_finished() {
            lines.extend(job.poll());
            std::thread::yield_now();
        }
        assert!(job.poll().is_empty());
        lines
    }

    #[test]
    fn job_reports_lines_in_order_and_the_exit_code() {
        let lines = run_helper(2);
        let markers: Vec<_> = lines
            .iter()
            .filter(|(_, line)| line.contains("marker-"))
            .cloned()
            .collect();
        let stdout: Vec<_> = markers
            .iter()
            .filter(|(_, line)| line.contains("marker-out"))
            .collect();
        assert_eq!(
            stdout,
            [
                &(LogLevel::Info, "marker-out-1".to_owned()),
                &(LogLevel::Info, "marker-out-2".to_owned()),
            ]
        );
        let stderr: Vec<_> = markers
            .iter()
            .filter(|(_, line)| line.contains("marker-err"))
            .collect();
        assert_eq!(
            stderr,
            [
                &(LogLevel::Info, "marker-err warning".to_owned()),
                &(LogLevel::Error, "rve-build: marker-err failure".to_owned()),
            ]
        );
        assert_eq!(
            lines.last().unwrap(),
            &finish_message(Some(2), Path::new("out-dir"))
        );

        let lines = run_helper(0);
        assert_eq!(lines.last().unwrap().0, LogLevel::Info);
        assert!(lines.last().unwrap().1.contains("out-dir"));
    }
}
