//! Имена и хранение файлов отчётов о падении (`crash-*.txt` и `crash-*.dmp`).
//! Общие для игры (текстовые отчёты) и монитора (minidump).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Пользовательское сообщение, которым клиент представляется монитору сразу после подключения.
/// Без него монитор не считает подключение установленным и не выходит при его отключении.
pub const HELLO_MESSAGE_KIND: u32 = 0;

/// Сколько ждать установленного клиента по умолчанию.
pub const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько последних файлов `crash-*` хранится в каталоге отчётов.
pub const RETAINED_CRASH_FILES: usize = 20;

/// Файлы моложе этого срока не удаляются: отчёт или дамп могут ещё дописываться.
pub const RETENTION_GRACE: Duration = Duration::from_secs(10);

/// Имя файла отчёта или дампа: префикс `crash-` и расширение `.txt` или `.dmp`.
pub fn is_crash_file_name(name: &str) -> bool {
    name.starts_with("crash-") && (name.ends_with(".txt") || name.ends_with(".dmp"))
}

/// Метка времени для имени файла, `YYYYMMDD-HHMMSS` по UTC. Лексикографический порядок совпадает
/// с хронологическим.
pub fn utc_stamp(time: SystemTime) -> String {
    let t = UtcTime::from(time);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// Время для текста отчёта, `YYYY-MM-DD HH:MM:SS UTC`.
pub fn utc_datetime(time: SystemTime) -> String {
    let t = UtcTime::from(time);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// Правило хранения после записи дампа: 20 последних файлов, свежие и собственный дамп не трогаются.
pub fn prune_after_dump(dir: &Path, dump: &Path, now: SystemTime) -> io::Result<usize> {
    prune_crash_files(dir, dump, RETAINED_CRASH_FILES, now, RETENTION_GRACE)
}

/// Удаляет самые старые файлы `crash-*` сверх `keep`, возвращает число удалённых.
///
/// Порядок — по имени, а имя начинается с метки времени. Файл `own` и файлы моложе `grace`
/// не удаляются, даже если лимит из-за них не соблюдается. Ошибки удаления и чтения
/// метаданных не прерывают очистку: файл просто остаётся.
pub fn prune_crash_files(
    dir: &Path,
    own: &Path,
    keep: usize,
    now: SystemTime,
    grace: Duration,
) -> io::Result<usize> {
    let own_name = own.file_name();
    let mut files: Vec<(std::ffi::OsString, PathBuf)> = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| entry.file_name().to_str().is_some_and(is_crash_file_name))
        .map(|entry| (entry.file_name(), entry.path()))
        .collect();
    files.sort();
    let mut excess = files.len().saturating_sub(keep);
    let mut removed = 0;
    for (name, path) in files {
        if excess == 0 {
            break;
        }
        if own_name == Some(name.as_os_str()) || is_young(&path, now, grace) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
            excess -= 1;
        }
    }
    Ok(removed)
}

fn is_young(path: &Path, now: SystemTime, grace: Duration) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return true;
    };
    now.duration_since(modified).map_or(true, |age| age < grace)
}

struct UtcTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

impl From<SystemTime> for UtcTime {
    fn from(time: SystemTime) -> Self {
        let seconds = match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
        };
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: u32::try_from(rest / 3_600).unwrap_or(0),
            minute: u32::try_from(rest % 3_600 / 60).unwrap_or(0),
            second: u32::try_from(rest % 60).unwrap_or(0),
        }
    }
}

/// Дата по числу дней от 1970-01-01 (алгоритм Howard Hinnant, `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Аргументы `rve-crash-monitor`: сокет для клиента, каталог отчётов и pid процесса игры.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorArgs {
    pub socket: PathBuf,
    pub dir: PathBuf,
    pub parent: u32,
    /// Сколько ждать первого hello от клиента, прежде чем выйти.
    pub client_timeout: Duration,
}

/// Разбирает `--socket <path> --dir <path> --parent <pid>` и необязательный `--client-timeout-ms <ms>`
/// в любом порядке. Последний нужен тестам; игра его не передаёт.
pub fn parse_args<I>(args: I) -> Result<MonitorArgs, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut socket = None;
    let mut dir = None;
    let mut parent = None;
    let mut client_timeout = DEFAULT_CLIENT_TIMEOUT;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("после {} нужно значение", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--socket") => socket = Some(PathBuf::from(value)),
            Some("--dir") => dir = Some(PathBuf::from(value)),
            Some("--parent") => {
                let pid = value
                    .to_str()
                    .and_then(|text| text.parse::<u32>().ok())
                    .filter(|pid| *pid != 0)
                    .ok_or_else(|| "--parent должен быть pid больше нуля".to_owned())?;
                parent = Some(pid);
            }
            Some("--client-timeout-ms") => {
                let millis = value
                    .to_str()
                    .and_then(|text| text.parse::<u64>().ok())
                    .ok_or_else(|| "--client-timeout-ms должен быть числом".to_owned())?;
                client_timeout = Duration::from_millis(millis);
            }
            _ => return Err(format!("неизвестный аргумент {}", flag.to_string_lossy())),
        }
    }
    Ok(MonitorArgs {
        socket: socket.ok_or("нужен --socket")?,
        dir: dir.ok_or("нужен --dir")?,
        parent: parent.ok_or("нужен --parent")?,
        client_timeout,
    })
}

/// Имя дампа: `crash-<время UTC>-<pid процесса игры>.dmp`.
pub fn dump_file_name(time: SystemTime, parent: u32) -> String {
    format!("crash-{}-{parent}.dmp", utc_stamp(time))
}

/// Создаёт новый файл дампа в `dir`; существующий файл не перезаписывается.
pub fn create_dump_file(dir: &Path, parent: u32, time: SystemTime) -> io::Result<(File, PathBuf)> {
    fs::create_dir_all(dir)?;
    let path = dir.join(dump_file_name(time, parent));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    Ok((file, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rve-crash-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(dir: &Path, name: &str, modified: SystemTime) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"report").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        path
    }

    #[test]
    fn stamps_use_utc_calendar_fields() {
        assert_eq!(utc_stamp(at(0)), "19700101-000000");
        assert_eq!(utc_stamp(at(951_868_800)), "20000301-000000");
        assert_eq!(utc_stamp(at(1_709_251_199)), "20240229-235959");
        assert_eq!(utc_datetime(at(1_709_251_199)), "2024-02-29 23:59:59 UTC");
    }

    #[test]
    fn only_crash_text_and_dump_files_are_managed() {
        assert!(is_crash_file_name("crash-20240101-000000-1-1.txt"));
        assert!(is_crash_file_name("crash-20240101-000000-1.dmp"));
        assert!(!is_crash_file_name("crash-20240101.log"));
        assert!(!is_crash_file_name("save.toml"));
        assert!(!is_crash_file_name("notes-crash-1.txt"));
    }

    #[test]
    fn prune_removes_the_oldest_files_beyond_the_limit() {
        let dir = scratch_dir("old");
        let old = at(1_000);
        for index in 0..RETAINED_CRASH_FILES {
            write_file(
                &dir,
                &format!("crash-20200101-000000-1-{index:02}.txt"),
                old,
            );
        }
        let own = write_file(&dir, "crash-20240101-000000-1-99.txt", at(2_000_000_000));
        let now = at(2_000_000_100);

        let removed =
            prune_crash_files(&dir, &own, RETAINED_CRASH_FILES, now, RETENTION_GRACE).unwrap();

        assert_eq!(removed, 1);
        assert!(own.exists());
        assert!(!dir.join("crash-20200101-000000-1-00.txt").exists());
        assert!(dir.join("crash-20200101-000000-1-01.txt").exists());
        let remaining = fs::read_dir(&dir).unwrap().count();
        assert_eq!(remaining, RETAINED_CRASH_FILES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_young_files_even_over_the_limit() {
        let dir = scratch_dir("young");
        let now = at(2_000_000_100);
        for index in 0..RETAINED_CRASH_FILES {
            write_file(
                &dir,
                &format!("crash-20240101-000000-1-{index:02}.txt"),
                at(2_000_000_095),
            );
        }
        let own = write_file(&dir, "crash-20240101-000000-1-99.txt", now);

        let removed =
            prune_crash_files(&dir, &own, RETAINED_CRASH_FILES, now, RETENTION_GRACE).unwrap();

        assert_eq!(removed, 0);
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            RETAINED_CRASH_FILES + 1
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_never_touches_unrelated_files_or_its_own_file() {
        let dir = scratch_dir("unrelated");
        let old = at(1_000);
        let own = write_file(&dir, "crash-20200101-000000-1-00.txt", old);
        for index in 1..=RETAINED_CRASH_FILES {
            write_file(
                &dir,
                &format!("crash-20200101-000000-1-{index:02}.dmp"),
                old,
            );
        }
        let unrelated = write_file(&dir, "save.toml", old);

        let removed = prune_crash_files(
            &dir,
            &own,
            RETAINED_CRASH_FILES,
            at(2_000_000_000),
            RETENTION_GRACE,
        )
        .unwrap();

        assert_eq!(removed, 1);
        assert!(own.exists(), "собственный файл не удаляется");
        assert!(unrelated.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod args_and_dumps_tests {
    use super::*;
    use std::ffi::OsString;
    use std::io::Write;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_all_flags_in_any_order() {
        let parsed = parse_args(args(&[
            "--parent", "4242", "--dir", "Logs", "--socket", "s",
        ]))
        .unwrap();
        assert_eq!(
            parsed,
            MonitorArgs {
                socket: PathBuf::from("s"),
                dir: PathBuf::from("Logs"),
                parent: 4242,
                client_timeout: DEFAULT_CLIENT_TIMEOUT,
            }
        );
    }

    #[test]
    fn client_timeout_is_optional_and_parsed_in_milliseconds() {
        let parsed = parse_args(args(&[
            "--socket",
            "s",
            "--dir",
            "d",
            "--parent",
            "7",
            "--client-timeout-ms",
            "250",
        ]))
        .unwrap();
        assert_eq!(parsed.client_timeout, Duration::from_millis(250));
        assert!(
            parse_args(args(&[
                "--socket",
                "s",
                "--dir",
                "d",
                "--parent",
                "7",
                "--client-timeout-ms",
                "x"
            ]))
            .is_err()
        );
    }

    #[test]
    fn rejects_missing_values_unknown_flags_and_bad_parents() {
        assert!(parse_args(args(&["--socket"])).is_err());
        assert!(parse_args(args(&["--socket", "s", "--dir", "d"])).is_err());
        assert!(parse_args(args(&["--socket", "s", "--dir", "d", "--parent", "0"])).is_err());
        assert!(parse_args(args(&["--socket", "s", "--dir", "d", "--parent", "x"])).is_err());
        assert!(
            parse_args(args(&[
                "--socket", "s", "--dir", "d", "--parent", "1", "--extra", "y"
            ]))
            .is_err()
        );
    }

    #[test]
    fn dump_rotation_keeps_the_new_dump_and_the_newest_twenty_files() {
        let dir = std::env::temp_dir().join(format!("rve-dump-rotation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_000);
        for index in 0..RETAINED_CRASH_FILES {
            let path = dir.join(format!("crash-20200101-000000-1-{index:02}.txt"));
            fs::write(&path, b"old").unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let (file, dump) =
            create_dump_file(&dir, 9, UNIX_EPOCH + Duration::from_secs(2_000_000_000)).unwrap();
        drop(file);

        let removed =
            prune_after_dump(&dir, &dump, UNIX_EPOCH + Duration::from_secs(2_000_000_100)).unwrap();

        assert_eq!(removed, 1);
        assert!(dump.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), RETAINED_CRASH_FILES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dump_name_carries_the_utc_time_and_game_pid() {
        let time = UNIX_EPOCH + Duration::from_secs(1_709_251_199);
        assert_eq!(dump_file_name(time, 77), "crash-20240229-235959-77.dmp");
    }

    #[test]
    fn dump_file_is_created_once_in_a_missing_directory() {
        let root = std::env::temp_dir().join(format!("rve-dump-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("Logs");
        let time = UNIX_EPOCH + Duration::from_secs(0);

        let (mut file, path) = create_dump_file(&dir, 9, time).unwrap();
        file.write_all(b"MDMP").unwrap();
        drop(file);

        assert_eq!(path, dir.join("crash-19700101-000000-9.dmp"));
        assert_eq!(fs::read(&path).unwrap(), b"MDMP");
        assert!(
            create_dump_file(&dir, 9, time).is_err(),
            "дамп не перезаписывается"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
