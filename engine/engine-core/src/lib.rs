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
