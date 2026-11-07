//! Общая модель данных профайлера: кольцевая статистика, отчёты и доска публикации.
//!
//! Модуль использует только `std`. Потоки-источники накапливают [`RollingStat`]
//! без аллокаций в кадре, а отчёт строят и публикуют с частотой показа.
//! [`ProfileBoard`] под lock меняет только `Arc`, читатель клонирует `Arc`
//! и сразу отпускает lock.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Итоги по окну измерений. Для пустого окна все поля равны нулю, `samples == 0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatSummary {
    pub last: Duration,
    pub avg: Duration,
    pub max: Duration,
    pub samples: usize,
}

impl StatSummary {
    /// Итог для одного измерения, например для фазы старта.
    pub fn once(value: Duration) -> Self {
        Self {
            last: value,
            avg: value,
            max: value,
            samples: 1,
        }
    }
}

/// Кольцевой буфер последних измерений фиксированной ёмкости.
///
/// Память выделяется в [`RollingStat::new`]; `push` не выделяет память.
#[derive(Clone, Debug)]
pub struct RollingStat {
    samples: Vec<Duration>,
    capacity: usize,
    next: usize,
    last: Duration,
}

impl RollingStat {
    /// Создаёт буфер на `capacity` измерений. Ёмкость не меньше одного.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: Vec::with_capacity(capacity),
            capacity,
            next: 0,
            last: Duration::ZERO,
        }
    }

    /// Добавляет измерение, вытесняя самое старое, когда буфер заполнен.
    pub fn push(&mut self, value: Duration) {
        if self.samples.len() < self.capacity {
            self.samples.push(value);
        } else {
            self.samples[self.next] = value;
        }
        self.next = (self.next + 1) % self.capacity;
        self.last = value;
    }

    /// Возвращает последнее, среднее и максимальное значение окна.
    pub fn summary(&self) -> StatSummary {
        if self.samples.is_empty() {
            return StatSummary::default();
        }
        let mut sum = Duration::ZERO;
        let mut max = Duration::ZERO;
        for &value in &self.samples {
            sum = sum.saturating_add(value);
            max = max.max(value);
        }
        let count = u32::try_from(self.samples.len()).unwrap_or(u32::MAX);
        StatSummary {
            last: self.last,
            avg: sum / count,
            max,
            samples: self.samples.len(),
        }
    }
}

/// Отчёт для показа: группы строк, каждая строка — метка и итог или её отсутствие.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileReport {
    pub groups: Vec<ProfileGroup>,
}

/// Именованная группа строк отчёта, например «Renderer CPU» или «GPU Scene».
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileGroup {
    pub name: &'static str,
    pub rows: Vec<ProfileRow>,
}

/// Одна строка отчёта. `None` означает, что значения нет в этом кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileRow {
    pub label: &'static str,
    pub summary: Option<StatSummary>,
}

impl fmt::Display for ProfileReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for group in &self.groups {
            writeln!(f, "{}", group.name)?;
            for row in &group.rows {
                match row.summary {
                    Some(s) => writeln!(
                        f,
                        "  {:<28} last {:>9.3} ms  avg {:>9.3} ms  max {:>9.3} ms  n={}",
                        row.label,
                        millis(s.last),
                        millis(s.avg),
                        millis(s.max),
                        s.samples,
                    )?,
                    None => writeln!(f, "  {:<28} n/a", row.label)?,
                }
            }
        }
        Ok(())
    }
}

fn millis(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

/// Доска публикации отчётов по фиксированному набору источников.
///
/// Каждый источник имеет свой слот. Публикация заменяет `Arc` под коротким lock,
/// а освобождение старого отчёта выполняется уже после отпускания lock.
pub struct ProfileBoard {
    slots: Vec<Slot>,
}

struct Slot {
    source: &'static str,
    report: Mutex<Option<Arc<ProfileReport>>>,
}

impl ProfileBoard {
    /// Создаёт доску с источниками в фиксированном порядке; этот порядок задаёт порядок чтения.
    pub fn new(sources: &[&'static str]) -> Self {
        Self {
            slots: sources
                .iter()
                .map(|&source| Slot {
                    source,
                    report: Mutex::new(None),
                })
                .collect(),
        }
    }

    /// Публикует отчёт источника. Возвращает `false`, если источник не зарегистрирован.
    pub fn publish(&self, source: &'static str, report: ProfileReport) -> bool {
        let Some(slot) = self.slots.iter().find(|slot| slot.source == source) else {
            return false;
        };
        let report = Arc::new(report);
        // Старый отчёт выпадает из временного значения после того, как lock отпущен.
        let _previous = lock(&slot.report).replace(report);
        true
    }

    /// Возвращает последние опубликованные отчёты в порядке регистрации источников.
    pub fn reports(&self) -> Vec<(&'static str, Arc<ProfileReport>)> {
        self.slots
            .iter()
            .filter_map(|slot| {
                let report = lock(&slot.report).clone()?;
                Some((slot.source, report))
            })
            .collect()
    }
}

fn lock(slot: &Mutex<Option<Arc<ProfileReport>>>) -> MutexGuard<'_, Option<Arc<ProfileReport>>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{ProfileBoard, ProfileGroup, ProfileReport, ProfileRow, RollingStat, StatSummary};
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn report_with(label: &'static str, value: u64) -> ProfileReport {
        ProfileReport {
            groups: vec![ProfileGroup {
                name: "Group",
                rows: vec![ProfileRow {
                    label,
                    summary: Some(StatSummary::once(ms(value))),
                }],
            }],
        }
    }

    #[test]
    fn summary_of_partially_filled_window_uses_all_samples() {
        let mut stat = RollingStat::new(8);
        stat.push(ms(1));
        stat.push(ms(3));

        let summary = stat.summary();
        assert_eq!(summary.samples, 2);
        assert_eq!(summary.avg, ms(2));
        assert_eq!(summary.max, ms(3));
        assert_eq!(summary.last, ms(3));
    }

    #[test]
    fn full_window_drops_oldest_sample() {
        let mut stat = RollingStat::new(3);
        for value in [10, 1, 2, 3] {
            stat.push(ms(value));
        }

        let summary = stat.summary();
        assert_eq!(summary.samples, 3);
        assert_eq!(summary.avg, ms(2));
        assert_eq!(
            summary.max,
            ms(3),
            "вытесненное значение 10 не должно влиять на максимум"
        );
        assert_eq!(summary.last, ms(3));
    }

    #[test]
    fn push_does_not_reallocate_after_construction() {
        let mut stat = RollingStat::new(4);
        let capacity_before = stat.samples.capacity();
        for value in 0..100 {
            stat.push(ms(value));
        }
        assert_eq!(stat.samples.capacity(), capacity_before);
        assert_eq!(stat.summary().samples, 4);
    }

    #[test]
    fn empty_window_has_zero_summary() {
        assert_eq!(RollingStat::new(4).summary(), StatSummary::default());
    }

    #[test]
    fn zero_capacity_is_clamped_to_one_sample() {
        let mut stat = RollingStat::new(0);
        stat.push(ms(5));
        stat.push(ms(7));

        let summary = stat.summary();
        assert_eq!(summary.samples, 1);
        assert_eq!(summary.avg, ms(7));
        assert_eq!(summary.max, ms(7));
    }

    #[test]
    fn report_display_prints_values_and_missing_rows() {
        let report = ProfileReport {
            groups: vec![ProfileGroup {
                name: "Render",
                rows: vec![
                    ProfileRow {
                        label: "Opaque",
                        summary: Some(StatSummary::once(ms(2))),
                    },
                    ProfileRow {
                        label: "Wireframe",
                        summary: None,
                    },
                ],
            }],
        };

        let text = report.to_string();
        assert!(text.starts_with("Render\n"));
        assert!(text.contains("Opaque"));
        assert!(text.contains("2.000 ms"));
        assert!(text.contains("Wireframe") && text.contains("n/a"));
    }

    #[test]
    fn board_reads_sources_in_registration_order() {
        let board = ProfileBoard::new(&["scripts", "render"]);
        assert!(board.publish("render", report_with("frame", 4)));
        assert!(board.publish("scripts", report_with("update", 2)));

        let reports = board.reports();
        let names: Vec<&str> = reports.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["scripts", "render"]);
        assert_eq!(reports[0].1.groups[0].rows[0].label, "update");
    }

    #[test]
    fn board_skips_sources_without_reports() {
        let board = ProfileBoard::new(&["scripts", "render"]);
        assert!(board.publish("render", report_with("frame", 4)));
        let names: Vec<&str> = board.reports().iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["render"]);
    }

    #[test]
    fn board_rejects_unknown_source() {
        let board = ProfileBoard::new(&["scripts"]);
        assert!(!board.publish("unknown", report_with("x", 1)));
        assert!(board.reports().is_empty());
    }

    #[test]
    fn board_replacement_keeps_earlier_reader_snapshot_valid() {
        let board = ProfileBoard::new(&["render"]);
        board.publish("render", report_with("first", 1));
        let held = board.reports()[0].1.clone();

        board.publish("render", report_with("second", 2));

        assert_eq!(held.groups[0].rows[0].label, "first");
        assert_eq!(board.reports()[0].1.groups[0].rows[0].label, "second");
    }

    #[test]
    fn board_receives_publications_from_two_threads() {
        let board = Arc::new(ProfileBoard::new(&["scripts", "render"]));
        let barrier = Arc::new(Barrier::new(2));

        let handles: Vec<_> = [("scripts", "update"), ("render", "frame")]
            .into_iter()
            .map(|(source, label)| {
                let board = Arc::clone(&board);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for value in 1..=50 {
                        board.publish(source, report_with(label, value));
                    }
                    board.publish(source, report_with(label, 99));
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let reports = board.reports();
        let names: Vec<&str> = reports.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["scripts", "render"]);
        assert_eq!(reports[0].1.groups[0].rows[0].label, "update");
        assert_eq!(reports[1].1.groups[0].rows[0].label, "frame");
        assert_eq!(
            reports[1].1.groups[0].rows[0].summary,
            Some(StatSummary::once(ms(99)))
        );
    }

    #[test]
    fn board_keeps_working_after_lock_poisoning() {
        let board = ProfileBoard::new(&["render"]);
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = board.slots[0].report.lock().unwrap();
            panic!("poison the slot lock");
        }));

        assert!(board.publish("render", report_with("frame", 3)));
        assert_eq!(board.reports().len(), 1);
    }
}
