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
use std::thread::JoinHandle;

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
}

#[cfg(windows)]
struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(unix)]
struct ProcessTree {
    process_group: Option<libc::pid_t>,
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

impl ProcessTree {
    fn prepare(command: &mut Command) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            let _ = command;
            Self::new()
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            Ok(Self {
                process_group: None,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = command;
            Ok(Self)
        }
    }

    fn attach(&mut self, child: &Child) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

            let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            if unsafe { AssignProcessToJobObject(self.job, process) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(unix)]
        {
            self.process_group =
                Some(libc::pid_t::try_from(child.id()).map_err(|_| {
                    std::io::Error::other("child process id does not fit in pid_t")
                })?);
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
        }
        Ok(())
    }

    fn terminate(&self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;

            if unsafe { TerminateJobObject(self.job, 1) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(unix)]
        {
            let Some(process_group) = self.process_group else {
                return Ok(());
            };
            if unsafe { libc::kill(-process_group, libc::SIGKILL) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "process-tree cleanup is unsupported on this platform",
            ));
        }
        Ok(())
    }

    #[cfg(windows)]
    fn new() -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let process_tree = Self { job };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                process_tree.job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(process_tree)
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}

/// Запущенная сборка; строки и итог забираются `poll` из цикла кадра.
pub struct BuildJob {
    child: Child,
    receiver: Receiver<Message>,
    readers: Vec<JoinHandle<()>>,
    process_tree: Option<ProcessTree>,
    output: PathBuf,
    finished: bool,
}

impl BuildJob {
    /// Запускает команду с перехваченными stdout и stderr; `output` попадает в итог.
    pub fn spawn(mut command: Command, output: PathBuf) -> std::io::Result<Self> {
        let mut process_tree = ProcessTree::prepare(&mut command)?;
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
        if let Err(error) = process_tree.attach(&child) {
            let _ = process_tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let (sender, receiver) = mpsc::channel();
        let mut readers = Vec::with_capacity(2);
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_reader(stdout, sender.clone(), |_| LogLevel::Info));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_reader(stderr, sender, classify_stderr));
        }
        Ok(Self {
            child,
            receiver,
            readers,
            process_tree: Some(process_tree),
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
        self.read_messages(&mut lines);
        match self.child.try_wait() {
            Ok(Some(status)) => self.finish(Some(status), &mut lines),
            Ok(None) => {}
            Err(error) => {
                lines.push((LogLevel::Error, format!("rve-build: {error}")));
                self.finish(None, &mut lines);
            }
        }
        lines
    }

    fn read_messages(&self, lines: &mut Vec<(LogLevel, String)>) {
        loop {
            match self.receiver.try_recv() {
                Ok(Message::Line(level, line)) => lines.push((level, line)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn finish(&mut self, status: Option<ExitStatus>, lines: &mut Vec<(LogLevel, String)>) {
        if let Some(process_tree) = self.process_tree.take() {
            let _ = process_tree.terminate();
        }
        if status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.join_readers();
        self.read_messages(lines);
        self.finished = true;
        lines.push(finish_message(
            status.and_then(|status| status.code()),
            &self.output,
        ));
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Останавливает процесс и его потомков, затем дожидается потоков вывода.
    pub fn kill(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(process_tree) = &self.process_tree {
            let _ = process_tree.terminate();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.process_tree.take();
        self.join_readers();
        self.finished = true;
    }

    fn join_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl Drop for BuildJob {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_reader(
    stream: impl Read + Send + 'static,
    sender: Sender<Message>,
    level: fn(&str) -> LogLevel,
) -> JoinHandle<()> {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const HELPER: &str = "RVE_EDITOR_BUILD_HELPER_EXIT";
    const TREE_MARKER: &str = "RVE_EDITOR_BUILD_TREE_MARKER";

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

    #[test]
    fn process_tree_parent_process() {
        let Ok(marker) = std::env::var(TREE_MARKER) else {
            return;
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "build::tests::process_tree_descendant_process",
                "--nocapture",
                "-q",
            ])
            .env(TREE_MARKER, &marker)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        while !Path::new(&marker).exists() {
            std::thread::yield_now();
        }
        let _ = child.wait();
    }

    #[test]
    fn process_tree_exit_parent_process() {
        let Ok(marker) = std::env::var(TREE_MARKER) else {
            return;
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "build::tests::process_tree_descendant_process",
                "--nocapture",
                "-q",
            ])
            .env(TREE_MARKER, &marker)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        while !Path::new(&marker).exists() {
            std::thread::yield_now();
        }
        let _ = child.try_wait();
    }

    #[test]
    fn process_tree_descendant_process() {
        let Ok(marker) = std::env::var(TREE_MARKER) else {
            return;
        };
        fs::write(marker, "running").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn kill_terminates_descendants_and_joins_output_readers() {
        let dir = TempDir::new();
        let marker = dir.0.join("descendant-started");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "build::tests::process_tree_parent_process",
                "--nocapture",
                "-q",
            ])
            .env(TREE_MARKER, &marker);
        let mut job = BuildJob::spawn(command, PathBuf::from("out-dir")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            let _ = job.poll();
            assert!(
                !job.is_finished(),
                "parent exited before starting its child"
            );
            std::thread::yield_now();
        }
        assert!(marker.exists(), "descendant did not start");

        job.kill();
    }

    #[test]
    fn parent_exit_terminates_descendants_and_joins_output_readers() {
        let dir = TempDir::new();
        let marker = dir.0.join("descendant-started");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "build::tests::process_tree_exit_parent_process",
                "--nocapture",
                "-q",
            ])
            .env(TREE_MARKER, &marker);
        let mut job = BuildJob::spawn(command, PathBuf::from("out-dir")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !job.is_finished() && Instant::now() < deadline {
            let _ = job.poll();
            std::thread::yield_now();
        }

        assert!(marker.exists(), "descendant did not start");
        assert!(job.is_finished(), "job did not finish after parent exit");
        assert!(job.readers.is_empty(), "output readers were not joined");
    }
}
