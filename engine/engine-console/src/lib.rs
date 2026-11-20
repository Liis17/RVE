//! Console command, history, scrollback, and logging primitives.

use log::{Level, Log, Metadata, Record};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, TryLockError};

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const CONSOLE_BUFFER_CAPACITY: usize = 2_000;
pub const COMMAND_HISTORY_CAPACITY: usize = 100;
/// Сколько последних Info/Warn/Error строк всегда хранится для отчётов о падении.
pub const LOG_TAIL_CAPACITY: usize = 200;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleLevel {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsoleLine {
    pub level: ConsoleLevel,
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ConsoleBuffer {
    lines: VecDeque<ConsoleLine>,
    capacity: usize,
    generation: u64,
}

impl Default for ConsoleBuffer {
    fn default() -> Self {
        Self::with_capacity(CONSOLE_BUFFER_CAPACITY)
    }
}

impl ConsoleBuffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(capacity),
            capacity,
            generation: 0,
        }
    }

    /// Appends a line, dropping the oldest one when full and suppressing an identical tail.
    pub fn push(&mut self, line: ConsoleLine) -> bool {
        if self.lines.back() == Some(&line) {
            return false;
        }
        if self.capacity == 0 {
            return false;
        }
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
        self.generation = self.generation.wrapping_add(1);
        true
    }

    pub fn clear(&mut self) {
        if !self.lines.is_empty() {
            self.lines.clear();
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub fn lines(&self) -> impl ExactSizeIterator<Item = &ConsoleLine> {
        self.lines.iter()
    }

    pub fn snapshot(&self) -> Vec<ConsoleLine> {
        self.lines.iter().cloned().collect()
    }

    pub fn take_lines(&mut self) -> Vec<ConsoleLine> {
        if self.lines.is_empty() {
            return Vec::new();
        }
        self.generation = self.generation.wrapping_add(1);
        self.lines.drain(..).collect()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandLine {
    pub name: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseCommandError {
    Empty,
    UnterminatedQuote,
    InvalidName(String),
}

impl fmt::Display for ParseCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("command line is empty"),
            Self::UnterminatedQuote => f.write_str("unterminated quoted argument"),
            Self::InvalidName(name) => write!(f, "invalid command name {name:?}"),
        }
    }
}

impl Error for ParseCommandError {}

pub fn parse_command_line(input: &str) -> Result<CommandLine, ParseCommandError> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut token_started = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                quoted = !quoted;
                token_started = true;
            }
            '\\' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                token.push('"');
                token_started = true;
            }
            '\\' if quoted && chars.peek() == Some(&'\\') => {
                chars.next();
                token.push('\\');
                token_started = true;
            }
            ch if ch.is_whitespace() && !quoted => {
                if token_started {
                    tokens.push(std::mem::take(&mut token));
                    token_started = false;
                }
            }
            ch => {
                token.push(ch);
                token_started = true;
            }
        }
    }
    if quoted {
        return Err(ParseCommandError::UnterminatedQuote);
    }
    if token_started {
        tokens.push(token);
    }
    let Some(name) = tokens.first_mut() else {
        return Err(ParseCommandError::Empty);
    };
    *name = name.to_ascii_lowercase();
    if !valid_command_name(name) {
        return Err(ParseCommandError::InvalidName(name.clone()));
    }
    Ok(CommandLine {
        name: tokens.remove(0),
        args: tokens,
    })
}

pub fn valid_command_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes[1..].iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'_' | b'.')
        })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
}

impl CommandSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Self, CatalogError> {
        let name = name.into();
        if !valid_command_name(&name) {
            return Err(CatalogError::InvalidName(name));
        }
        if is_reserved_command(&name) {
            return Err(CatalogError::Reserved(name));
        }
        Ok(Self {
            name,
            description: description.into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    InvalidName(String),
    Reserved(String),
    Duplicate(String),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(f, "invalid command name {name:?}"),
            Self::Reserved(name) => write!(f, "command name {name:?} is reserved"),
            Self::Duplicate(name) => write!(f, "command {name:?} is already registered"),
        }
    }
}

impl Error for CatalogError {}

/// Встроенные команды, отсортированные по имени; в `CommandCatalog` не хранятся.
static BUILTIN_COMMANDS: LazyLock<[CommandSpec; 2]> = LazyLock::new(|| {
    [
        CommandSpec {
            name: "clear".into(),
            description: "clear console output".into(),
        },
        CommandSpec {
            name: "help".into(),
            description: "list commands, optionally filtered by prefix".into(),
        },
    ]
});

#[derive(Clone, Debug, Default)]
pub struct CommandCatalog {
    commands: BTreeMap<String, CommandSpec>,
}

impl CommandCatalog {
    pub fn register(&mut self, spec: CommandSpec) -> Result<(), CatalogError> {
        if !valid_command_name(&spec.name) {
            return Err(CatalogError::InvalidName(spec.name));
        }
        if is_reserved_command(&spec.name) {
            return Err(CatalogError::Reserved(spec.name));
        }
        if self.commands.contains_key(&spec.name) {
            return Err(CatalogError::Duplicate(spec.name));
        }
        self.commands.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.commands.remove(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.get(name)
    }

    /// Встроенные и зарегистрированные команды по имени, без копирования и сортировки.
    pub fn iter(&self) -> impl Iterator<Item = &CommandSpec> {
        let mut builtins = BUILTIN_COMMANDS.iter().peekable();
        let mut registered = self.commands.values().peekable();
        std::iter::from_fn(move || {
            let builtin = builtins.next_if(|builtin| {
                registered
                    .peek()
                    .is_none_or(|registered| builtin.name <= registered.name)
            });
            builtin.or_else(|| registered.next())
        })
    }

    pub fn commands(&self) -> Vec<CommandSpec> {
        self.iter().cloned().collect()
    }

    /// Prefix matches are returned first; remaining substring matches follow, all by name.
    pub fn matches(&self, input: &str) -> Vec<&CommandSpec> {
        let input = input.to_ascii_lowercase();
        let mut prefix = Vec::new();
        let mut substring = Vec::new();
        for command in self.iter() {
            if command.name.starts_with(&input) {
                prefix.push(command);
            } else if command.name.contains(&input) {
                substring.push(command);
            }
        }
        prefix.extend(substring);
        prefix
    }

    pub fn complete(&self, input: &str) -> Vec<CommandSpec> {
        self.matches(input).into_iter().cloned().collect()
    }

    /// Common prefix of the prefix matches, or of the substring matches when there are none.
    pub fn common_prefix(&self, input: &str) -> String {
        let input = input.to_ascii_lowercase();
        let has_prefix_match = self.iter().any(|command| command.name.starts_with(&input));
        let names = self
            .iter()
            .map(|command| command.name.as_str())
            .filter(|name| {
                if has_prefix_match {
                    name.starts_with(&input)
                } else {
                    name.contains(&input)
                }
            });
        common_prefix(names)
    }

    pub fn help(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        self.iter()
            .filter(|command| command.name.starts_with(&prefix))
            .map(|command| format!("{} — {}", command.name, command.description))
            .collect()
    }
}

fn is_reserved_command(name: &str) -> bool {
    matches!(name, "help" | "clear")
}

fn common_prefix<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut names = names;
    let Some(first) = names.next() else {
        return String::new();
    };
    let mut end = first.len();
    for name in names {
        end = first
            .bytes()
            .zip(name.bytes())
            .take_while(|(left, right)| left == right)
            .count()
            .min(end);
    }
    first[..end].to_owned()
}

#[derive(Clone, Debug, Default)]
pub struct CommandHistory {
    entries: VecDeque<String>,
    cursor: Option<usize>,
}

impl CommandHistory {
    pub fn push(&mut self, command: impl Into<String>) {
        let command = command.into();
        if command.trim().is_empty() {
            return;
        }
        if self.entries.back() != Some(&command) {
            if self.entries.len() == COMMAND_HISTORY_CAPACITY {
                self.entries.pop_front();
            }
            self.entries.push_back(command);
        }
        self.cursor = None;
    }

    pub fn previous(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        self.cursor = Some(match self.cursor {
            Some(0) => 0,
            Some(index) => index - 1,
            None => self.entries.len() - 1,
        });
        self.entries.get(self.cursor?).map(String::as_str)
    }

    pub fn following(&mut self) -> Option<&str> {
        let index = self.cursor?;
        if index + 1 >= self.entries.len() {
            self.cursor = None;
            return Some("");
        }
        self.cursor = Some(index + 1);
        self.entries.get(index + 1).map(String::as_str)
    }

    pub fn reset_navigation(&mut self) {
        self.cursor = None;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Кольцо последних строк для отчётов о падении. В отличие от `ConsoleBuffer` не подавляет повторы:
/// одинаковые строки подряд — это тоже сведения о сбое.
struct LogRing {
    lines: VecDeque<ConsoleLine>,
}

impl LogRing {
    fn push(&mut self, line: ConsoleLine) {
        if self.lines.len() == LOG_TAIL_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    fn snapshot(&self) -> Vec<ConsoleLine> {
        self.lines.iter().cloned().collect()
    }
}

pub struct ConsoleLogger {
    sink: Option<Arc<Mutex<ConsoleBuffer>>>,
    tail: Mutex<LogRing>,
}

impl ConsoleLogger {
    pub fn new(sink: Option<Arc<Mutex<ConsoleBuffer>>>) -> Self {
        Self {
            sink,
            tail: Mutex::new(LogRing {
                lines: VecDeque::with_capacity(LOG_TAIL_CAPACITY),
            }),
        }
    }

    fn tail_snapshot(&self) -> Vec<ConsoleLine> {
        lock_tail(&self.tail).snapshot()
    }

    /// Не ждёт блокировку: `None`, если её держит другой поток.
    fn try_tail_snapshot(&self) -> Option<Vec<ConsoleLine>> {
        match self.tail.try_lock() {
            Ok(ring) => Some(ring.snapshot()),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner().snapshot()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    pub fn log_record(&self, record: &Record<'_>) {
        let level = match record.level() {
            Level::Error => ConsoleLevel::Error,
            Level::Warn => ConsoleLevel::Warn,
            Level::Info => ConsoleLevel::Info,
            Level::Debug | Level::Trace => return,
        };
        let text = record.args().to_string();
        match level {
            ConsoleLevel::Info => {
                let _ = writeln!(io::stdout().lock(), "{text}");
            }
            ConsoleLevel::Warn | ConsoleLevel::Error => {
                let _ = writeln!(io::stderr().lock(), "{text}");
            }
        }
        let line = ConsoleLine {
            level,
            source: record.target().to_owned(),
            text,
        };
        if let Some(sink) = &self.sink {
            lock_buffer(sink).push(line.clone());
        }
        lock_tail(&self.tail).push(line);
    }
}

impl Log for ConsoleLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            self.log_record(record);
        }
    }

    fn flush(&self) {
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
    }
}

static LOGGER: OnceLock<ConsoleLogger> = OnceLock::new();

/// Installs the process logger once, forwarding Info to stdout and Warn/Error to stderr.
pub fn install_logger(sink: Option<Arc<Mutex<ConsoleBuffer>>>) -> Result<(), log::SetLoggerError> {
    let logger = LOGGER.get_or_init(|| ConsoleLogger::new(sink));
    log::set_logger(logger)?;
    log::set_max_level(Level::Info.to_level_filter());
    Ok(())
}

/// Последние Info/Warn/Error строки процесса; пусто, если логгер не установлен. Ждёт блокировку.
pub fn log_tail() -> Vec<ConsoleLine> {
    LOGGER
        .get()
        .map_or_else(Vec::new, ConsoleLogger::tail_snapshot)
}

/// Как [`log_tail`], но не ждёт: `None`, если хвост сейчас занят другим потоком.
/// Нужен обработчику panic, который не должен зависнуть на чужой блокировке.
pub fn try_log_tail() -> Option<Vec<ConsoleLine>> {
    LOGGER
        .get()
        .map_or_else(|| Some(Vec::new()), ConsoleLogger::try_tail_snapshot)
}

fn lock_buffer(buffer: &Mutex<ConsoleBuffer>) -> std::sync::MutexGuard<'_, ConsoleBuffer> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_tail(tail: &Mutex<LogRing>) -> std::sync::MutexGuard<'_, LogRing> {
    tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(level: ConsoleLevel, source: &str, text: &str) -> ConsoleLine {
        ConsoleLine {
            level,
            source: source.into(),
            text: text.into(),
        }
    }

    #[test]
    fn parser_handles_whitespace_quotes_and_escaped_quotes() {
        assert_eq!(
            parse_command_line("  Spawn \"two words\" \"a \\\"quote\\\"\" ").unwrap(),
            CommandLine {
                name: "spawn".into(),
                args: vec!["two words".into(), "a \"quote\"".into()]
            }
        );
        assert_eq!(
            parse_command_line("  ").unwrap_err(),
            ParseCommandError::Empty
        );
        assert_eq!(
            parse_command_line("run \"unfinished").unwrap_err(),
            ParseCommandError::UnterminatedQuote
        );
    }

    #[test]
    fn parser_validates_and_normalizes_the_command_name() {
        assert_eq!(
            parse_command_line("Foo.Bar_2 arg").unwrap().name,
            "foo.bar_2"
        );
        assert!(matches!(
            parse_command_line("2foo"),
            Err(ParseCommandError::InvalidName(_))
        ));
        assert!(matches!(
            parse_command_line(&"a".repeat(65)),
            Err(ParseCommandError::InvalidName(_))
        ));
    }

    #[test]
    fn buffer_is_bounded_deduplicates_the_tail_and_tracks_generation() {
        let mut buffer = ConsoleBuffer::with_capacity(2);
        assert!(buffer.push(line(ConsoleLevel::Info, "test", "one")));
        assert!(!buffer.push(line(ConsoleLevel::Info, "test", "one")));
        assert!(buffer.push(line(ConsoleLevel::Warn, "test", "two")));
        assert!(buffer.push(line(ConsoleLevel::Error, "test", "three")));
        assert_eq!(
            buffer.snapshot(),
            [
                line(ConsoleLevel::Warn, "test", "two"),
                line(ConsoleLevel::Error, "test", "three")
            ]
        );
        assert_eq!(buffer.generation(), 3);
        buffer.clear();
        assert_eq!(buffer.generation(), 4);
    }

    #[test]
    fn completion_prefixes_precede_substrings_and_common_prefix_is_reported() {
        let mut catalog = CommandCatalog::default();
        for name in ["game.spawn", "spawn", "spawn_enemy"] {
            catalog
                .register(CommandSpec::new(name, "description").unwrap())
                .unwrap();
        }
        assert_eq!(
            catalog
                .complete("spawn")
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["spawn", "spawn_enemy", "game.spawn"]
        );
        assert_eq!(catalog.common_prefix("spawn"), "spawn");
        assert!(
            catalog
                .help("help")
                .iter()
                .any(|entry| entry.starts_with("help —"))
        );
    }

    fn catalog_with(names: &[&str]) -> CommandCatalog {
        let mut catalog = CommandCatalog::default();
        for name in names {
            catalog
                .register(CommandSpec::new(*name, "description").unwrap())
                .unwrap();
        }
        catalog
    }

    fn names<'a>(specs: impl IntoIterator<Item = &'a CommandSpec>) -> Vec<&'a str> {
        specs.into_iter().map(|spec| spec.name.as_str()).collect()
    }

    #[test]
    fn builtins_are_merged_into_name_order() {
        let mut catalog = catalog_with(&["zz", "aaa", "help_me", "mid"]);
        let expected = ["aaa", "clear", "help", "help_me", "mid", "zz"];
        assert_eq!(names(catalog.iter()), expected);
        assert_eq!(
            catalog
                .commands()
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(names(catalog.matches("")), expected);

        catalog.remove("aaa");
        catalog
            .register(CommandSpec::new("b", "x").unwrap())
            .unwrap();
        assert_eq!(
            names(catalog.iter()),
            ["b", "clear", "help", "help_me", "mid", "zz"]
        );
    }

    #[test]
    fn builtins_are_not_stored_in_the_registry() {
        let mut catalog = CommandCatalog::default();
        assert!(catalog.get("help").is_none());
        assert!(catalog.get("clear").is_none());
        assert!(!catalog.remove("help"));
        assert!(!catalog.remove("clear"));
        assert_eq!(names(catalog.iter()), ["clear", "help"]);
    }

    #[test]
    fn prefix_and_substring_groups_are_each_ordered_by_name() {
        let catalog = catalog_with(&["xab", "ab_b", "zab", "ab_a", "cab"]);
        assert_eq!(
            names(catalog.matches("ab")),
            ["ab_a", "ab_b", "cab", "xab", "zab"]
        );
        assert_eq!(names(catalog.matches("e")), ["clear", "help"]);
        assert_eq!(names(catalog.matches("el")), ["help"]);
    }

    #[test]
    fn matching_ignores_input_case() {
        let catalog = catalog_with(&["ab_a", "ab_b"]);
        assert_eq!(names(catalog.matches("HE")), ["help"]);
        assert_eq!(names(catalog.matches("AB")), ["ab_a", "ab_b"]);
        assert_eq!(catalog.complete("Ab")[0].name, "ab_a");
        assert_eq!(catalog.common_prefix("Ab"), "ab_");
    }

    #[test]
    fn common_prefix_uses_substring_matches_only_without_prefix_matches() {
        let catalog = catalog_with(&["game.spawn_a", "game.spawn_b"]);
        assert_eq!(catalog.common_prefix("spawn"), "game.spawn_");
        assert_eq!(catalog.common_prefix("zzz"), "");
        assert_eq!(catalog.common_prefix("cl"), "clear");
    }

    #[test]
    fn empty_input_lists_every_command_and_has_no_common_prefix() {
        let catalog = catalog_with(&["alpha", "beta"]);
        assert_eq!(
            names(catalog.matches("")),
            ["alpha", "beta", "clear", "help"]
        );
        assert_eq!(catalog.common_prefix(""), "");
    }

    #[test]
    fn help_lists_builtins_and_registered_commands_in_order() {
        let catalog = catalog_with(&["alpha", "help_x"]);
        let lines = catalog.help("h");
        assert_eq!(
            lines,
            [
                "help — list commands, optionally filtered by prefix",
                "help_x — description",
            ]
        );
    }

    #[test]
    fn catalog_rejects_reserved_duplicate_and_invalid_names() {
        assert_eq!(
            CommandSpec::new("help", "x").unwrap_err(),
            CatalogError::Reserved("help".into())
        );
        assert!(matches!(
            CommandSpec::new("Bad", "x"),
            Err(CatalogError::InvalidName(_))
        ));
        let mut catalog = CommandCatalog::default();
        catalog
            .register(CommandSpec::new("hello", "x").unwrap())
            .unwrap();
        assert_eq!(
            catalog
                .register(CommandSpec::new("hello", "again").unwrap())
                .unwrap_err(),
            CatalogError::Duplicate("hello".into())
        );
    }

    #[test]
    fn history_is_bounded_and_navigates_to_the_live_input() {
        let mut history = CommandHistory::default();
        for index in 0..105 {
            history.push(format!("cmd{index}"));
        }
        assert_eq!(history.len(), COMMAND_HISTORY_CAPACITY);
        assert_eq!(history.previous(), Some("cmd104"));
        assert_eq!(history.previous(), Some("cmd103"));
        assert_eq!(history.following(), Some("cmd104"));
        assert_eq!(history.following(), Some(""));
        assert_eq!(history.following(), None);
    }

    #[test]
    fn edited_input_can_restart_history_from_the_newest_entry() {
        let mut history = CommandHistory::default();
        history.push("first");
        history.push("second");
        assert_eq!(history.previous(), Some("second"));
        assert_eq!(history.previous(), Some("first"));

        history.reset_navigation();

        assert_eq!(history.previous(), Some("second"));
    }

    #[test]
    fn logger_routes_info_warn_and_error_to_sink_without_global_install() {
        let sink = Arc::new(Mutex::new(ConsoleBuffer::with_capacity(8)));
        let logger = ConsoleLogger::new(Some(sink.clone()));
        for (level, expected) in [
            (Level::Info, ConsoleLevel::Info),
            (Level::Warn, ConsoleLevel::Warn),
            (Level::Error, ConsoleLevel::Error),
        ] {
            let args = format_args!("[script] event");
            let record = Record::builder()
                .args(args)
                .level(level)
                .target("scene/object/script")
                .build();
            logger.log_record(&record);
            assert_eq!(lock_buffer(&sink).lines().last().unwrap().level, expected);
        }
        assert_eq!(
            lock_buffer(&sink)
                .snapshot()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["[script] event", "[script] event", "[script] event"]
        );
    }

    fn log_record_at(logger: &ConsoleLogger, level: Level, text: &str) {
        let args = format_args!("{text}");
        let record = Record::builder()
            .args(args)
            .level(level)
            .target("test")
            .build();
        logger.log_record(&record);
    }

    #[test]
    fn log_tail_keeps_only_the_last_200_lines() {
        let logger = ConsoleLogger::new(None);
        for index in 0..LOG_TAIL_CAPACITY + 5 {
            log_record_at(&logger, Level::Info, &format!("line {index}"));
        }
        let tail = logger.tail_snapshot();
        assert_eq!(tail.len(), LOG_TAIL_CAPACITY);
        assert_eq!(tail.first().unwrap().text, "line 5");
        assert_eq!(
            tail.last().unwrap().text,
            format!("line {}", LOG_TAIL_CAPACITY + 4)
        );
    }

    #[test]
    fn log_tail_keeps_repeated_lines_unlike_the_console_buffer() {
        let logger = ConsoleLogger::new(None);
        log_record_at(&logger, Level::Error, "same");
        log_record_at(&logger, Level::Error, "same");
        assert_eq!(logger.tail_snapshot().len(), 2);
    }

    #[test]
    fn log_tail_skips_debug_and_trace_records() {
        let logger = ConsoleLogger::new(None);
        log_record_at(&logger, Level::Debug, "debug detail");
        log_record_at(&logger, Level::Trace, "trace detail");
        log_record_at(&logger, Level::Warn, "kept");
        assert_eq!(
            logger
                .tail_snapshot()
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["kept"]
        );
    }

    #[test]
    fn try_log_tail_reports_unavailable_only_while_the_lock_is_held() {
        let logger = ConsoleLogger::new(None);
        log_record_at(&logger, Level::Error, "before lock");
        {
            let _held = logger.tail.lock().unwrap();
            assert_eq!(logger.try_tail_snapshot(), None);
        }
        let tail = logger.try_tail_snapshot().unwrap();
        assert_eq!(tail[0].text, "before lock");
    }

    #[test]
    fn try_log_tail_reads_a_poisoned_tail() {
        let logger = ConsoleLogger::new(None);
        log_record_at(&logger, Level::Info, "survives poison");
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = logger.tail.lock().unwrap();
            panic!("poison the tail lock");
        }));
        let tail = logger
            .try_tail_snapshot()
            .expect("poisoned lock is readable");
        assert_eq!(tail[0].text, "survives poison");
    }
}
