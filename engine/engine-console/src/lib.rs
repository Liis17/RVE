//! Console command, history, scrollback, and logging primitives.

use log::{Level, Log, Metadata, Record};
use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

pub const CONSOLE_BUFFER_CAPACITY: usize = 2_000;
pub const COMMAND_HISTORY_CAPACITY: usize = 100;

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

    pub fn commands(&self) -> Vec<CommandSpec> {
        let mut commands = vec![
            CommandSpec {
                name: "clear".into(),
                description: "clear console output".into(),
            },
            CommandSpec {
                name: "help".into(),
                description: "list commands, optionally filtered by prefix".into(),
            },
        ];
        commands.extend(self.commands.values().cloned());
        commands.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        commands
    }

    /// Prefix matches are returned first; remaining substring matches follow, all by name.
    pub fn complete(&self, input: &str) -> Vec<CommandSpec> {
        let input = input.to_ascii_lowercase();
        let mut prefix = Vec::new();
        let mut substring = Vec::new();
        for command in self.commands() {
            if command.name.starts_with(&input) {
                prefix.push(command);
            } else if command.name.contains(&input) {
                substring.push(command);
            }
        }
        prefix.extend(substring);
        prefix
    }

    pub fn common_prefix(&self, input: &str) -> String {
        let input = input.to_ascii_lowercase();
        let prefix_matches: Vec<_> = self
            .commands()
            .into_iter()
            .filter(|command| command.name.starts_with(&input))
            .collect();
        if prefix_matches.is_empty() {
            let matches = self.complete(&input);
            common_prefix(matches.iter().map(|command| command.name.as_str()))
        } else {
            common_prefix(prefix_matches.iter().map(|command| command.name.as_str()))
        }
    }

    pub fn help(&self, prefix: &str) -> Vec<String> {
        let prefix = prefix.to_ascii_lowercase();
        self.commands()
            .into_iter()
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

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub struct ConsoleLogger {
    sink: Option<Arc<Mutex<ConsoleBuffer>>>,
}

impl ConsoleLogger {
    pub fn new(sink: Option<Arc<Mutex<ConsoleBuffer>>>) -> Self {
        Self { sink }
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
        if let Some(sink) = &self.sink {
            lock_buffer(sink).push(ConsoleLine {
                level,
                source: record.target().to_owned(),
                text,
            });
        }
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

fn lock_buffer(buffer: &Mutex<ConsoleBuffer>) -> std::sync::MutexGuard<'_, ConsoleBuffer> {
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
}
