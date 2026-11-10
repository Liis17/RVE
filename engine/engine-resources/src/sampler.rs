//! Дельта статистики запросов между соседними снимками и группа отчёта профайлера.

use std::time::Duration;

use engine_core::profile::{ProfileGroup, ProfileRow, ProfileValue};

use crate::service::{ResourceStats, TimingTotals};

/// Изменения счётчиков между двумя снимками и средние времена запросов за тот же период.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ResourceInterval {
    completed: u64,
    failed: u64,
    cancelled: u64,
    bytes: u64,
    queue_avg: Option<Duration>,
    read_avg: Option<Duration>,
}

fn between(earlier: &ResourceStats, later: &ResourceStats) -> ResourceInterval {
    ResourceInterval {
        completed: later.completed.saturating_sub(earlier.completed),
        failed: later.failed.saturating_sub(earlier.failed),
        cancelled: later.cancelled.saturating_sub(earlier.cancelled),
        bytes: later.bytes.saturating_sub(earlier.bytes),
        queue_avg: average_between(earlier.queue, later.queue),
        read_avg: average_between(earlier.read, later.read),
    }
}

/// Среднее время замеров, добавленных между снимками; `None`, если новых замеров не было.
fn average_between(earlier: TimingTotals, later: TimingTotals) -> Option<Duration> {
    let count = later.count.saturating_sub(earlier.count);
    if count == 0 {
        return None;
    }
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    Some(later.total.saturating_sub(earlier.total) / count)
}

/// Превращает снимки [`ResourceStats`] в группу отчёта: дельта за последний интервал между снимками.
#[derive(Default)]
pub struct ResourceSampler {
    previous: Option<ResourceStats>,
    latest: Option<ResourceInterval>,
    current: ResourceStats,
}

impl ResourceSampler {
    /// Учитывает новый снимок. Дельту даёт пара снимков; первый снимок её не даёт.
    pub fn sample(&mut self, stats: ResourceStats) {
        if let Some(previous) = self.previous {
            self.latest = Some(between(&previous, &stats));
        }
        self.previous = Some(stats);
        self.current = stats;
    }

    /// Группа «Resources»: счётчики и средние за последний интервал, `in flight` и максимумы с запуска.
    /// До второго снимка интервальные строки равны `n/a`.
    pub fn profile_group(&self) -> ProfileGroup {
        let latest = self.latest;
        let count = |value: fn(&ResourceInterval) -> u64| match latest {
            Some(interval) => ProfileValue::Count(value(&interval)),
            None => ProfileValue::Missing,
        };
        let average = |value: fn(&ResourceInterval) -> Option<Duration>| {
            latest
                .and_then(|interval| value(&interval))
                .map_or(ProfileValue::Missing, ProfileValue::Single)
        };
        let maximum = |timing: TimingTotals| {
            if timing.count == 0 {
                ProfileValue::Missing
            } else {
                ProfileValue::Single(timing.max)
            }
        };
        ProfileGroup {
            name: "Resources",
            rows: vec![
                row("completed / interval", count(|i| i.completed)),
                row("failed / interval", count(|i| i.failed)),
                row("cancelled / interval", count(|i| i.cancelled)),
                row("bytes / interval", count(|i| i.bytes)),
                row(
                    "in flight",
                    ProfileValue::Count(self.current.in_flight as u64),
                ),
                row("queue avg / interval", average(|i| i.queue_avg)),
                row("queue max / start", maximum(self.current.queue)),
                row("read avg / interval", average(|i| i.read_avg)),
                row("read max / start", maximum(self.current.read)),
            ],
        }
    }
}

fn row(label: &'static str, value: ProfileValue) -> ProfileRow {
    ProfileRow { label, value }
}

#[cfg(test)]
mod tests {
    use super::{ResourceSampler, between};
    use crate::service::{ResourceStats, TimingTotals};
    use engine_core::profile::ProfileValue;
    use std::time::Duration;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn timing(count: u64, total: u64, max: u64) -> TimingTotals {
        TimingTotals {
            count,
            total: ms(total),
            max: ms(max),
        }
    }

    fn stats(completed: u64, bytes: u64, queue: TimingTotals) -> ResourceStats {
        ResourceStats {
            completed,
            bytes,
            queue,
            ..ResourceStats::default()
        }
    }

    fn value(sampler: &ResourceSampler, label: &str) -> ProfileValue {
        sampler
            .profile_group()
            .rows
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.value)
            .expect("строка группы Resources")
    }

    #[test]
    fn interval_counts_are_differences_of_snapshots() {
        let earlier = ResourceStats {
            completed: 2,
            failed: 1,
            bytes: 10,
            ..ResourceStats::default()
        };
        let later = ResourceStats {
            completed: 5,
            failed: 1,
            cancelled: 2,
            bytes: 40,
            ..ResourceStats::default()
        };

        let interval = between(&earlier, &later);

        assert_eq!(interval.completed, 3);
        assert_eq!(interval.failed, 0);
        assert_eq!(interval.cancelled, 2);
        assert_eq!(interval.bytes, 30);
    }

    #[test]
    fn interval_average_uses_only_samples_added_since_the_earlier_snapshot() {
        let earlier = stats(0, 0, timing(2, 4, 3));
        let later = stats(0, 0, timing(4, 10, 5));

        // Новые замеры: (10 - 4) мс на 2 запроса.
        assert_eq!(between(&earlier, &later).queue_avg, Some(ms(3)));
    }

    #[test]
    fn interval_without_new_requests_has_no_average() {
        let snapshot = stats(0, 0, timing(2, 4, 3));

        assert_eq!(between(&snapshot, &snapshot).queue_avg, None);
    }

    #[test]
    fn first_snapshot_leaves_interval_rows_missing_but_reports_in_flight() {
        let mut sampler = ResourceSampler::default();
        sampler.sample(ResourceStats {
            in_flight: 3,
            ..ResourceStats::default()
        });

        assert_eq!(
            value(&sampler, "completed / interval"),
            ProfileValue::Missing
        );
        assert_eq!(
            value(&sampler, "queue avg / interval"),
            ProfileValue::Missing
        );
        assert_eq!(value(&sampler, "in flight"), ProfileValue::Count(3));
    }

    #[test]
    fn second_snapshot_fills_interval_rows_and_keeps_zero_as_a_value() {
        let mut sampler = ResourceSampler::default();
        sampler.sample(stats(4, 100, timing(1, 2, 2)));
        sampler.sample(stats(7, 160, timing(3, 8, 6)));

        assert_eq!(
            value(&sampler, "completed / interval"),
            ProfileValue::Count(3)
        );
        assert_eq!(value(&sampler, "bytes / interval"), ProfileValue::Count(60));
        assert_eq!(
            value(&sampler, "queue avg / interval"),
            ProfileValue::Single(ms(3))
        );
        assert_eq!(
            value(&sampler, "queue max / start"),
            ProfileValue::Single(ms(6))
        );

        // Ничего не прибавилось: счётчик равен нулю, а не n/a.
        sampler.sample(stats(7, 160, timing(3, 8, 6)));
        assert_eq!(
            value(&sampler, "completed / interval"),
            ProfileValue::Count(0)
        );
        assert_eq!(
            value(&sampler, "queue avg / interval"),
            ProfileValue::Missing
        );
    }

    #[test]
    fn maxima_are_missing_until_a_request_has_finished() {
        let mut sampler = ResourceSampler::default();
        sampler.sample(ResourceStats::default());

        assert_eq!(value(&sampler, "read max / start"), ProfileValue::Missing);
    }
}
