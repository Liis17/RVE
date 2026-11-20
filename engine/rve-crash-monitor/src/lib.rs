//! Имена и хранение файлов отчётов о падении (`crash-*.txt` и `crash-*.dmp`).
//! Общие для игры (текстовые отчёты) и монитора (minidump).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
