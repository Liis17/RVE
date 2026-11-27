//! Line protocol shared by the editor and an editor-launched player process.

use std::error::Error;
use std::fmt;

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_RECORD_BYTES: usize = 64 * 1024;

const PREFIX: &str = "\u{1e}@rve-play ";
const TRUNCATED_MARKER: &str = " [record truncated]";

pub const fn is_supported_protocol_version(version: u32) -> bool {
    version == PROTOCOL_VERSION
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerStatus {
    Loading,
    Running,
    Stopping,
}

impl PlayerStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Running => "running",
            Self::Stopping => "stopping",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl PlayerLogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayerMessage {
    Hello {
        protocol_version: u32,
        pid: u32,
        runtime_version: String,
    },
    Status(PlayerStatus),
    Log {
        level: PlayerLogLevel,
        target: String,
        text: String,
    },
    Exit {
        code: i32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorMessage {
    Start { password: Option<String> },
    Stop,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedPlayerMessage {
    Message(PlayerMessage),
    Raw(String),
    /// An unrecognized message type. Consumers can ignore this for forward compatibility.
    Unknown {
        kind: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    MissingField(&'static str),
    InvalidValue(&'static str),
    InvalidEscape,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "missing {field} field"),
            Self::InvalidValue(field) => write!(f, "invalid {field} field"),
            Self::InvalidEscape => f.write_str("invalid escape sequence"),
        }
    }
}

impl Error for ProtocolError {}

pub fn encode_player_message(message: &PlayerMessage) -> String {
    let fields = match message {
        PlayerMessage::Hello {
            protocol_version,
            pid,
            runtime_version,
        } => vec![
            "hello".to_owned(),
            protocol_version.to_string(),
            pid.to_string(),
            escape(runtime_version),
        ],
        PlayerMessage::Status(status) => vec!["status".to_owned(), status.as_str().to_owned()],
        PlayerMessage::Log {
            level,
            target,
            text,
        } => vec![
            "log".to_owned(),
            level.as_str().to_owned(),
            escape(target),
            escape(text),
        ],
        PlayerMessage::Exit { code } => vec!["exit".to_owned(), code.to_string()],
    };
    format!("{PREFIX}{}\n", fields.join("\t"))
}

/// Decodes a complete stdout record, including the record separator when present.
/// Lines without the protocol marker are returned as raw player output.
pub fn decode_player_record(record: &str) -> Result<DecodedPlayerMessage, ProtocolError> {
    let record = record.strip_suffix('\r').unwrap_or(record);
    let Some(payload) = record.strip_prefix(PREFIX) else {
        return Ok(DecodedPlayerMessage::Raw(record.to_owned()));
    };
    let fields: Vec<_> = payload.split('\t').collect();
    let kind = fields.first().copied().unwrap_or_default();
    let get = |index: usize, name| {
        fields
            .get(index)
            .copied()
            .ok_or(ProtocolError::MissingField(name))
    };
    let message = match kind {
        "hello" => PlayerMessage::Hello {
            protocol_version: get(1, "protocol version")?
                .parse()
                .map_err(|_| ProtocolError::InvalidValue("protocol version"))?,
            pid: get(2, "process id")?
                .parse()
                .map_err(|_| ProtocolError::InvalidValue("process id"))?,
            runtime_version: unescape(get(3, "runtime version")?)?,
        },
        "status" => PlayerMessage::Status(match get(1, "status")? {
            "loading" => PlayerStatus::Loading,
            "running" => PlayerStatus::Running,
            "stopping" => PlayerStatus::Stopping,
            _ => return Err(ProtocolError::InvalidValue("status")),
        }),
        "log" => PlayerMessage::Log {
            level: match get(1, "log level")? {
                "trace" => PlayerLogLevel::Trace,
                "debug" => PlayerLogLevel::Debug,
                "info" => PlayerLogLevel::Info,
                "warn" => PlayerLogLevel::Warn,
                "error" => PlayerLogLevel::Error,
                _ => return Err(ProtocolError::InvalidValue("log level")),
            },
            target: unescape(get(2, "log target")?)?,
            text: unescape(get(3, "log text")?)?,
        },
        "exit" => PlayerMessage::Exit {
            code: get(1, "exit code")?
                .parse()
                .map_err(|_| ProtocolError::InvalidValue("exit code"))?,
        },
        _ => {
            return Ok(DecodedPlayerMessage::Unknown {
                kind: kind.to_owned(),
            });
        }
    };
    Ok(DecodedPlayerMessage::Message(message))
}

pub fn encode_editor_message(message: &EditorMessage) -> String {
    match message {
        EditorMessage::Start { password: None } => "start\n".to_owned(),
        EditorMessage::Start {
            password: Some(password),
        } => format!("start\t{}\n", escape(password)),
        EditorMessage::Stop => "stop\n".to_owned(),
    }
}

/// Unknown editor commands are ignored so newer editors can communicate with older players.
pub fn decode_editor_line(line: &str) -> Result<Option<EditorMessage>, ProtocolError> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut fields = line.split('\t');
    match fields.next().unwrap_or_default() {
        "start" => match fields.next() {
            Some(password) => Ok(Some(EditorMessage::Start {
                password: Some(unescape(password)?),
            })),
            None => Ok(Some(EditorMessage::Start { password: None })),
        },
        "stop" => Ok(Some(EditorMessage::Stop)),
        _ => Ok(None),
    }
}

/// Incrementally separates newline-delimited records and RS-prefixed protocol records.
/// This lets a protocol event follow raw stdout that did not end with a newline.
#[derive(Default)]
pub struct PlayerStreamDecoder {
    buffer: Vec<u8>,
    truncated: bool,
}

impl PlayerStreamDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<DecodedPlayerMessage, ProtocolError>> {
        let mut records = Vec::new();
        for byte in bytes {
            match *byte {
                b'\x1e' => {
                    self.emit(&mut records);
                    self.push_byte(*byte);
                }
                b'\n' => self.emit(&mut records),
                byte => self.push_byte(byte),
            }
        }
        records
    }

    pub fn finish(&mut self) -> Vec<Result<DecodedPlayerMessage, ProtocolError>> {
        let mut records = Vec::new();
        self.emit(&mut records);
        records
    }

    fn push_byte(&mut self, byte: u8) {
        if self.buffer.len() < MAX_RECORD_BYTES {
            self.buffer.push(byte);
        } else {
            self.truncated = true;
        }
    }

    fn emit(&mut self, records: &mut Vec<Result<DecodedPlayerMessage, ProtocolError>>) {
        if self.buffer.is_empty() && !self.truncated {
            return;
        }
        let bytes = std::mem::take(&mut self.buffer);
        if self.truncated {
            let mut raw = String::from_utf8_lossy(&bytes).into_owned();
            raw.push_str(TRUNCATED_MARKER);
            records.push(Ok(DecodedPlayerMessage::Raw(raw)));
        } else {
            let record = String::from_utf8_lossy(&bytes);
            records.push(decode_player_record(&record));
        }
        self.truncated = false;
    }
}

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn unescape(value: &str) -> Result<String, ProtocolError> {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => output.push('\\'),
            Some('t') => output.push('\t'),
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            _ => return Err(ProtocolError::InvalidEscape),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: PlayerMessage) {
        let encoded = encode_player_message(&message);
        let decoded = decode_player_record(encoded.trim_end()).unwrap();
        assert_eq!(decoded, DecodedPlayerMessage::Message(message));
    }

    #[test]
    fn player_messages_round_trip() {
        round_trip(PlayerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            pid: 42,
            runtime_version: "0.40.0".into(),
        });
        for status in [
            PlayerStatus::Loading,
            PlayerStatus::Running,
            PlayerStatus::Stopping,
        ] {
            round_trip(PlayerMessage::Status(status));
        }
        round_trip(PlayerMessage::Log {
            level: PlayerLogLevel::Warn,
            target: "engine::scene".into(),
            text: "a warning".into(),
        });
        round_trip(PlayerMessage::Exit { code: 3 });
    }

    #[test]
    fn player_messages_escape_controls_and_multiline_text() {
        let message = PlayerMessage::Log {
            level: PlayerLogLevel::Info,
            target: "a\\b\tc\nd\re".into(),
            text: "first line\nsecond\tthird\\fourth\rfifth".into(),
        };
        round_trip(message);
    }

    #[test]
    fn raw_unknown_and_malformed_records_are_distinguished() {
        assert_eq!(
            decode_player_record("ordinary output").unwrap(),
            DecodedPlayerMessage::Raw("ordinary output".into())
        );
        assert_eq!(
            decode_player_record("\u{1e}@rve-play future\tfield").unwrap(),
            DecodedPlayerMessage::Unknown {
                kind: "future".into()
            }
        );
        assert_eq!(
            decode_player_record("\u{1e}@rve-play hello\t1").unwrap_err(),
            ProtocolError::MissingField("process id")
        );
        assert_eq!(
            decode_player_record("\u{1e}@rve-play log\tinfo\ttarget\tbad\\q").unwrap_err(),
            ProtocolError::InvalidEscape
        );
    }

    #[test]
    fn hello_version_is_available_for_compatibility_checks() {
        let decoded = decode_player_record("\u{1e}@rve-play hello\t2\t42\t0.40.0").unwrap();
        assert_eq!(
            decoded,
            DecodedPlayerMessage::Message(PlayerMessage::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                pid: 42,
                runtime_version: "0.40.0".into(),
            })
        );
        assert!(!is_supported_protocol_version(PROTOCOL_VERSION + 1));
        assert!(is_supported_protocol_version(PROTOCOL_VERSION));
    }

    #[test]
    fn stream_decoder_splits_raw_output_before_rs_protocol_record() {
        let mut decoder = PlayerStreamDecoder::default();
        let mut messages = decoder.push(b"raw output without newline");
        messages.extend(
            decoder.push(
                encode_player_message(&PlayerMessage::Status(PlayerStatus::Loading)).as_bytes(),
            ),
        );
        assert_eq!(
            messages,
            vec![
                Ok(DecodedPlayerMessage::Raw(
                    "raw output without newline".into()
                )),
                Ok(DecodedPlayerMessage::Message(PlayerMessage::Status(
                    PlayerStatus::Loading
                ))),
            ]
        );
    }

    #[test]
    fn stream_decoder_caps_record_size_and_marks_truncation() {
        let mut decoder = PlayerStreamDecoder::default();
        let records = decoder.push(&vec![b'x'; MAX_RECORD_BYTES + 20]);
        assert!(records.is_empty());
        let mut records = decoder.finish();
        assert_eq!(records.len(), 1);
        let Some(Ok(DecodedPlayerMessage::Raw(raw))) = records.pop() else {
            panic!("truncated record should be raw output");
        };
        assert_eq!(raw.len(), MAX_RECORD_BYTES + TRUNCATED_MARKER.len());
        assert!(raw.ends_with(TRUNCATED_MARKER));
    }

    #[test]
    fn editor_messages_round_trip_and_ignore_unknown_commands() {
        for message in [
            EditorMessage::Start { password: None },
            EditorMessage::Start {
                password: Some("pa\\ss\tword\nnext".into()),
            },
            EditorMessage::Stop,
        ] {
            let encoded = encode_editor_message(&message);
            assert_eq!(
                decode_editor_line(encoded.trim_end()).unwrap(),
                Some(message)
            );
        }
        assert_eq!(decode_editor_line("future\tcommand").unwrap(), None);
        assert_eq!(decode_editor_line("junk").unwrap(), None);
    }
}
