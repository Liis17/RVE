//! Текстовые отчёты о panic: файл `crash-*.txt` в каталоге отчётов, хвост лога и backtrace.
//!
//! Хук ставится один раз на процесс и всегда вызывает прежний хук последним, поэтому stderr и код
//! выхода остаются прежними. Каталог отчётов выбирается после загрузки проекта; до этого хук пишет
//! в каталог, найденный по расположению исполняемого файла.

use crate::VERSION;
use engine_console::{ConsoleLevel, ConsoleLine, try_log_tail};
use rve_crash_monitor::{
    RETAINED_CRASH_FILES, RETENTION_GRACE, prune_crash_files, utc_datetime, utc_stamp,
};
use std::any::Any;
use std::backtrace::Backtrace;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once, OnceLock, RwLock, TryLockError};
use std::thread;
use std::time::SystemTime;

/// Больше отчётов за процесс не пишется; повторные panic только передаются прежнему хуку.
pub(crate) const REPORT_LIMIT: usize = 8;

const LOGS_DIR_NAME: &str = "Logs";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    Player,
    Server,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Player => "player",
            Self::Server => "server",
        }
    }
}

/// Куда и от чьего имени пишется отчёт. `dir` — `None`, если ни один каталог не доступен.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ReportTarget {
    pub(crate) role: Role,
    pub(crate) project: Option<String>,
    pub(crate) dir: Option<PathBuf>,
}

static REPORTS_WRITTEN: AtomicUsize = AtomicUsize::new(0);
static INSTALL: Once = Once::new();
/// Снимок до загрузки проекта: используется, когда `CURRENT` занят или ещё не задан.
static INITIAL: OnceLock<Arc<ReportTarget>> = OnceLock::new();
static CURRENT: RwLock<Option<Arc<ReportTarget>>> = RwLock::new(None);

/// Ставит хук panic на процесс. Повторные вызовы ничего не меняют.
pub(crate) fn install(role: Role, exe_dir: &Path) {
    INSTALL.call_once(|| {
        let initial = ReportTarget {
            role,
            project: None,
            dir: Some(release_report_dir(exe_dir, cfg!(target_os = "macos"))),
        };
        let _ = INITIAL.set(Arc::new(initial));
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            report_panic(info);
            previous(info);
        }));
    });
}

/// Выбирает каталог отчётов после загрузки проекта и запоминает его для хука.
/// Возвращает выбранный каталог, созданный на диске, или `None`, если его нет.
pub(crate) fn set_target(role: Role, primary: PathBuf, project: &str) -> Option<PathBuf> {
    let dir = choose_report_dir(&report_dir_candidates(primary, project));
    if dir.is_none() {
        let _ = writeln!(
            io::stderr(),
            "[crash] каталог отчётов недоступен: отчёты о падении будут только в stderr"
        );
    }
    let target = ReportTarget {
        role,
        project: Some(project.to_owned()),
        dir: dir.clone(),
    };
    *CURRENT
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(target));
    dir
}

/// Каталог отчётов dev-проекта: `<project>/Logs`.
pub(crate) fn dev_report_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(LOGS_DIR_NAME)
}

/// Каталог отчётов release: `Logs` рядом с исполняемым файлом. На macOS игрок в `<Name>.app/Contents/MacOS`
/// пишет рядом с бандлом, а не внутрь него.
pub(crate) fn release_report_dir(exe_dir: &Path, macos: bool) -> PathBuf {
    let base = macos
        .then(|| app_bundle_parent(exe_dir))
        .flatten()
        .unwrap_or(exe_dir);
    base.join(LOGS_DIR_NAME)
}

fn app_bundle_parent(exe_dir: &Path) -> Option<&Path> {
    let contents = exe_dir.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = exe_dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
    if is_bundle { bundle.parent() } else { None }
}

/// Основной каталог, затем `save_data_path(project).parent()/Logs`.
fn report_dir_candidates(primary: PathBuf, project: &str) -> Vec<PathBuf> {
    let mut dirs = vec![primary];
    if let Some(save) = engine_platform::save_data_path(project)
        && let Some(parent) = save.parent()
    {
        dirs.push(parent.join(LOGS_DIR_NAME));
    }
    dirs
}

/// Первый каталог из списка, который удалось создать; `None`, если подходящего нет.
fn choose_report_dir(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|dir| prepare_dir(dir)).cloned()
}

fn prepare_dir(dir: &Path) -> bool {
    fs::create_dir_all(dir).is_ok() && dir.is_dir()
}

/// Вызывается из хука. Хук не должен паниковать: std в этом случае завершает процесс с abort
/// и отчёт не записывается до конца. Поэтому здесь нет `unwrap`/`expect`/индексации, а ошибки записи
/// возвращаются значениями и печатаются в stderr.
fn report_panic(info: &PanicHookInfo<'_>) {
    let Some(slot) = claim_report_slot(&REPORTS_WRITTEN, REPORT_LIMIT) else {
        return;
    };
    write_panic_report(info, slot);
}

/// Резервирует номер отчёта до `limit`; `None`, когда лимит исчерпан.
fn claim_report_slot(counter: &AtomicUsize, limit: usize) -> Option<usize> {
    let previous = counter.fetch_add(1, Ordering::SeqCst);
    (previous < limit).then_some(previous + 1)
}

fn write_panic_report(info: &PanicHookInfo<'_>, slot: usize) {
    let Some(target) = current_target() else {
        return;
    };
    let thread = thread::current();
    let thread_label = format!("{} ({:?})", thread.name().unwrap_or("unnamed"), thread.id());
    let location = info.location().map(|location| {
        format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )
    });
    let backtrace = Backtrace::force_capture().to_string();
    let tail = try_log_tail();
    let text = render_report(&Report {
        time: SystemTime::now(),
        role: target.role,
        project: target.project.as_deref(),
        thread: &thread_label,
        message: payload_text(info.payload()),
        location: location.as_deref(),
        backtrace: &backtrace,
        log_tail: tail.as_deref(),
    });
    let mut stderr = io::stderr();
    match target.dir.as_deref() {
        Some(dir) => match write_report_file(dir, SystemTime::now(), slot, &text) {
            Ok(path) => {
                let _ = writeln!(stderr, "[crash] отчёт записан: {}", path.display());
            }
            Err(error) => {
                let _ = writeln!(
                    stderr,
                    "[crash] не удалось записать отчёт в {}: {error}\n{text}",
                    dir.display()
                );
            }
        },
        None => {
            let _ = writeln!(stderr, "[crash] отчёт (каталог недоступен):\n{text}");
        }
    }
}

fn current_target() -> Option<Arc<ReportTarget>> {
    target_snapshot(&CURRENT, &INITIAL)
}

/// Снимок для хука: `current` читается без ожидания; если он занят или ещё не задан, берётся `initial`.
fn target_snapshot(
    current: &RwLock<Option<Arc<ReportTarget>>>,
    initial: &OnceLock<Arc<ReportTarget>>,
) -> Option<Arc<ReportTarget>> {
    let snapshot = match current.try_read() {
        Ok(guard) => guard.clone(),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().clone(),
        Err(TryLockError::WouldBlock) => None,
    };
    snapshot.or_else(|| initial.get().cloned())
}

/// Записывает отчёт в новый файл и затем удаляет лишние старые файлы `crash-*`.
fn write_report_file(dir: &Path, time: SystemTime, slot: usize, text: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        "crash-{}-{}-{slot}.txt",
        utc_stamp(time),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    let _ = prune_crash_files(
        dir,
        &path,
        RETAINED_CRASH_FILES,
        SystemTime::now(),
        RETENTION_GRACE,
    );
    Ok(path)
}

/// Текст panic. `message` — payload, `location` — `file:line:column`, если известен.
struct Report<'a> {
    time: SystemTime,
    role: Role,
    project: Option<&'a str>,
    thread: &'a str,
    message: &'a str,
    location: Option<&'a str>,
    backtrace: &'a str,
    /// `None` — хвост занят другим потоком и не прочитан.
    log_tail: Option<&'a [ConsoleLine]>,
}

fn render_report(report: &Report<'_>) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "RVE crash report");
    let _ = writeln!(text, "time: {}", utc_datetime(report.time));
    let _ = writeln!(text, "engine_runtime: {VERSION}");
    let _ = writeln!(text, "role: {}", report.role.label());
    let _ = writeln!(text, "project: {}", report.project.unwrap_or("-"));
    let _ = writeln!(text, "thread: {}", report.thread);
    let _ = writeln!(text, "message: {}", report.message);
    let _ = writeln!(text, "location: {}", report.location.unwrap_or("unknown"));
    let _ = writeln!(text, "\nbacktrace:\n{}", report.backtrace);
    let _ = writeln!(text, "log tail:");
    match report.log_tail {
        None => {
            let _ = writeln!(text, "log tail unavailable");
        }
        Some([]) => {
            let _ = writeln!(text, "(empty)");
        }
        Some(lines) => {
            for line in lines {
                let _ = writeln!(text, "[{}] {}", level_label(line.level), line.text);
            }
        }
    }
    text
}

fn level_label(level: ConsoleLevel) -> &'static str {
    match level {
        ConsoleLevel::Info => "info",
        ConsoleLevel::Warn => "warn",
        ConsoleLevel::Error => "error",
    }
}

/// Текст payload panic: `&str` и `String` как есть, остальные типы — заглушка.
fn payload_text(payload: &(dyn Any + Send)) -> &str {
    if let Some(text) = payload.downcast_ref::<&str>() {
        text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text
    } else {
        "<non-string panic payload>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    const CHILD_DIR_ENV: &str = "RVE_CRASH_REPORT_TEST_DIR";

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rve-crash-report-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_tail() -> Vec<ConsoleLine> {
        vec![
            ConsoleLine {
                level: ConsoleLevel::Info,
                source: "game".into(),
                text: "loaded scene".into(),
            },
            ConsoleLine {
                level: ConsoleLevel::Error,
                source: "script".into(),
                text: "script failed".into(),
            },
        ]
    }

    fn report<'a>(tail: Option<&'a [ConsoleLine]>) -> Report<'a> {
        Report {
            time: UNIX_EPOCH + Duration::from_secs(1_709_251_199),
            role: Role::Server,
            project: Some("demo"),
            thread: "worker (ThreadId(7))",
            message: "boom",
            location: Some("src/lib.rs:10:5"),
            backtrace: "frame-one",
            log_tail: tail,
        }
    }

    #[test]
    fn report_contains_every_field_and_the_log_tail() {
        let tail = sample_tail();
        let text = render_report(&report(Some(&tail)));

        assert!(text.starts_with("RVE crash report\n"));
        assert!(text.contains("time: 2024-02-29 23:59:59 UTC"));
        assert!(text.contains(&format!("engine_runtime: {VERSION}")));
        assert!(text.contains("role: server"));
        assert!(text.contains("project: demo"));
        assert!(text.contains("thread: worker (ThreadId(7))"));
        assert!(text.contains("message: boom"));
        assert!(text.contains("location: src/lib.rs:10:5"));
        assert!(text.contains("frame-one"));
        assert!(text.contains("[info] loaded scene"));
        assert!(text.contains("[error] script failed"));
    }

    #[test]
    fn report_states_an_empty_or_unavailable_tail_and_missing_fields() {
        let empty: [ConsoleLine; 0] = [];
        let text = render_report(&report(Some(&empty)));
        assert!(text.contains("log tail:\n(empty)"));

        let unavailable = render_report(&report(None));
        assert!(unavailable.contains("log tail unavailable"));

        let mut bare = report(None);
        bare.project = None;
        bare.location = None;
        let text = render_report(&bare);
        assert!(text.contains("project: -"));
        assert!(text.contains("location: unknown"));
    }

    #[test]
    fn payload_text_reads_str_and_string_and_describes_other_types() {
        let borrowed: Box<dyn Any + Send> = Box::new("static text");
        let owned: Box<dyn Any + Send> = Box::new(String::from("owned text"));
        let other: Box<dyn Any + Send> = Box::new(42_u32);
        assert_eq!(payload_text(&*borrowed), "static text");
        assert_eq!(payload_text(&*owned), "owned text");
        assert_eq!(payload_text(&*other), "<non-string panic payload>");
    }

    #[test]
    fn report_file_is_written_with_a_unique_name_and_no_overwrite() {
        let dir = scratch_dir("write");
        let time = UNIX_EPOCH + Duration::from_secs(0);
        let path = write_report_file(&dir, time, 1, "first").unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            name,
            format!("crash-19700101-000000-{}-1.txt", std::process::id())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        assert!(write_report_file(&dir, time, 1, "again").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_into_a_full_directory_keeps_the_newest_twenty_crash_files() {
        let dir = scratch_dir("rotation");
        let old = UNIX_EPOCH + Duration::from_secs(1_000);
        for index in 0..RETAINED_CRASH_FILES {
            let path = dir.join(format!("crash-20200101-000000-1-{index:02}.txt"));
            fs::write(&path, b"old").unwrap();
            OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        let written = write_report_file(&dir, SystemTime::now(), 1, "fresh").unwrap();

        assert!(written.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), RETAINED_CRASH_FILES);
        assert!(!dir.join("crash-20200101-000000-1-00.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_directory_falls_back_when_the_first_path_is_a_file() {
        let root = scratch_dir("fallback");
        let blocked = root.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let second = root.join("second");

        let chosen = choose_report_dir(&[blocked, second.clone()]);

        assert_eq!(chosen, Some(second));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_usable_report_directory_means_stderr_only() {
        let root = scratch_dir("unusable");
        let blocked = root.join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        assert_eq!(choose_report_dir(&[blocked]), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn busy_current_target_falls_back_to_the_startup_snapshot() {
        let initial_target = Arc::new(ReportTarget {
            role: Role::Player,
            project: None,
            dir: Some(PathBuf::from("startup")),
        });
        let initial = OnceLock::new();
        let _ = initial.set(Arc::clone(&initial_target));
        let current: RwLock<Option<Arc<ReportTarget>>> = RwLock::new(None);

        let busy = current.write().unwrap();
        assert_eq!(
            target_snapshot(&current, &initial),
            Some(initial_target.clone())
        );
        drop(busy);

        let set = Arc::new(ReportTarget {
            role: Role::Server,
            project: Some("demo".into()),
            dir: Some(PathBuf::from("project")),
        });
        *current.write().unwrap() = Some(Arc::clone(&set));
        assert_eq!(target_snapshot(&current, &initial), Some(set));
    }

    #[test]
    fn report_slots_stop_at_the_limit() {
        let counter = AtomicUsize::new(0);
        let slots: Vec<_> = (0..REPORT_LIMIT + 2)
            .map(|_| claim_report_slot(&counter, REPORT_LIMIT))
            .collect();
        assert_eq!(slots[0], Some(1));
        assert_eq!(slots[REPORT_LIMIT - 1], Some(REPORT_LIMIT));
        assert_eq!(slots[REPORT_LIMIT], None);
        assert_eq!(slots[REPORT_LIMIT + 1], None);
    }

    #[test]
    fn dev_and_release_directories_follow_the_layout_rules() {
        let project = Path::new("/games/demo");
        assert_eq!(dev_report_dir(project), project.join("Logs"));

        let exe_dir = Path::new("/games/demo/Build");
        assert_eq!(
            release_report_dir(exe_dir, false),
            exe_dir.join("Logs"),
            "не macOS: рядом с исполняемым файлом"
        );
        let bundled = Path::new("/games/Demo.app/Contents/MacOS");
        assert_eq!(
            release_report_dir(bundled, true),
            Path::new("/games/Logs"),
            "macOS: рядом с .app, а не внутри бандла"
        );
        assert_eq!(
            release_report_dir(bundled, false),
            bundled.join("Logs"),
            "вне macOS бандл не распознаётся"
        );
        assert_eq!(
            release_report_dir(Path::new("/games/rve-server"), true),
            Path::new("/games/rve-server").join("Logs"),
            "сервер вне бандла пишет рядом с собой"
        );
    }

    #[test]
    fn caught_panic_in_a_worker_writes_a_report_and_keeps_the_previous_hook() {
        // Отдельный процесс: глобальный хук и его счётчики не должны попасть в другие тесты.
        if std::env::var_os(CHILD_DIR_ENV).is_none() {
            let dir = scratch_dir("child");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "crash::tests::panic_child", "--nocapture"])
                .env(CHILD_DIR_ENV, &dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("previous-hook-marker"), "{stderr}");
            assert!(stderr.contains("[crash] отчёт записан"), "{stderr}");

            let reports: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect();
            assert_eq!(reports.len(), 1, "{reports:?}");
            let text = fs::read_to_string(&reports[0]).unwrap();
            assert!(text.contains("message: worker boom"), "{text}");
            assert!(text.contains("thread: crash-test-worker"), "{text}");
            assert!(text.contains("role: player"), "{text}");
            assert!(text.contains("project: crash-test-project"), "{text}");
            assert!(text.contains("log tail:"), "{text}");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Выполняется только дочерним процессом из теста выше: ставит хук и делает panic в потоке.
    #[test]
    fn panic_child() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
            return;
        };
        panic::set_hook(Box::new(|_| eprintln!("previous-hook-marker")));
        install(Role::Player, Path::new("unused"));
        let _ = set_target(Role::Player, PathBuf::from(dir), "crash-test-project");
        let worker = thread::Builder::new()
            .name("crash-test-worker".into())
            .spawn(|| panic!("worker boom"))
            .unwrap();
        assert!(worker.join().is_err());
    }
}
