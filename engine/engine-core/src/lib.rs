//! Базовые типы и функции игрового цикла движка.

use std::time::{Duration, Instant};

/// Измеряет время между кадрами и с момента запуска.
pub struct Time {
    start: Instant,
    previous_tick: Instant,
    frame: u64,
}

impl Time {
    /// Создаёт счётчик времени.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            previous_tick: now,
            frame: 0,
        }
    }

    /// Запоминает новый кадр и возвращает время с предыдущего тика.
    pub fn tick(&mut self) -> Duration {
        let now = Instant::now();
        let delta = now.duration_since(self.previous_tick);
        self.previous_tick = now;
        self.frame += 1;
        delta
    }

    /// Возвращает время с момента создания счётчика.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Возвращает число выполненных тиков.
    pub fn frame(&self) -> u64 {
        self.frame
    }
}

impl Default for Time {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Time;
    use std::time::{Duration, Instant};

    #[test]
    fn default_starts_without_frames() {
        assert_eq!(Time::default().frame(), 0);
    }

    #[test]
    fn first_tick_measures_time_since_creation() {
        let start = Instant::now() - Duration::from_secs(60);
        let mut time = Time {
            start,
            previous_tick: start,
            frame: 0,
        };
        let before = start.elapsed();
        let delta = time.tick();
        let after = start.elapsed();

        assert!(delta >= before && delta <= after);
        assert!(time.elapsed() >= delta);
        assert_eq!(time.frame(), 1);
    }

    #[test]
    fn tick_measures_from_previous_tick_instead_of_start() {
        let start = Instant::now() - Duration::from_secs(120);
        let previous_tick = start + Duration::from_secs(60);
        let mut time = Time {
            start,
            previous_tick,
            frame: 4,
        };
        let before = previous_tick.elapsed();
        let delta = time.tick();
        let after = previous_tick.elapsed();

        assert!(delta >= before && delta <= after);
        assert_eq!(time.previous_tick.duration_since(previous_tick), delta);
        assert_eq!(time.frame(), 5);
    }

    #[test]
    fn tick_advances_frame_counter() {
        let mut time = Time::new();
        assert_eq!(time.frame(), 0);
        let _ = time.tick();
        assert_eq!(time.frame(), 1);
        let _ = time.tick();
        assert_eq!(time.frame(), 2);
    }

    #[test]
    fn elapsed_is_monotonic() {
        let time = Time::default();
        let first = time.elapsed();
        let second = time.elapsed();
        assert!(second >= first);
    }
}
