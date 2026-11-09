//! Журнал редактора и скриптов для панели Console.

use engine_script::{ScriptEvent, ScriptEventLevel};
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 5000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
}

/// Кольцевой журнал с ограниченным числом записей.
#[derive(Default)]
pub struct ConsoleLog {
    entries: VecDeque<LogEntry>,
}

impl ConsoleLog {
    /// Добавляет запись; повтор последней не дублируется: ошибка при перетаскивании
    /// значения возникает на каждом кадре.
    pub fn push(&mut self, level: LogLevel, message: impl Into<String>) {
        let entry = LogEntry {
            level,
            message: message.into(),
        };
        if self.entries.back() == Some(&entry) {
            return;
        }
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.push(LogLevel::Info, message);
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.push(LogLevel::Warn, message);
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.push(LogLevel::Error, message);
    }

    pub fn push_script_events(&mut self, events: Vec<ScriptEvent>) {
        for event in events {
            let level = match event.level {
                ScriptEventLevel::Info => LogLevel::Info,
                ScriptEventLevel::Warn => LogLevel::Warn,
                ScriptEventLevel::Error => LogLevel::Error,
            };
            self.push(level, event.to_string());
        }
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &LogEntry> {
        self.entries.iter()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_entries_are_dropped_at_capacity() {
        let mut log = ConsoleLog::default();
        for index in 0..=MAX_ENTRIES {
            log.info(index.to_string());
        }
        assert_eq!(log.entries().count(), MAX_ENTRIES);
        assert_eq!(log.entries().next().unwrap().message, "1");
    }

    #[test]
    fn repeated_last_message_is_not_duplicated() {
        let mut log = ConsoleLog::default();
        log.error("a");
        log.error("a");
        log.info("a");
        assert_eq!(log.entries().count(), 2);
    }

    #[test]
    fn script_events_keep_level_and_source() {
        let mut log = ConsoleLog::default();
        log.push_script_events(vec![ScriptEvent {
            level: ScriptEventLevel::Warn,
            source: "scene".to_owned(),
            message: "careful".to_owned(),
        }]);
        let entry = log.entries().next().unwrap();
        assert_eq!(entry.level, LogLevel::Warn);
        assert_eq!(entry.message, "scene: careful");
        log.clear();
        assert_eq!(log.entries().count(), 0);
    }
}
