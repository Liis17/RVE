//! Открытый заголовок датаграммы и константы размеров протокола.

/// Сигнатура протокола в начале каждой датаграммы.
pub const MAGIC: [u8; 4] = *b"RVEN";
/// Версия проводного формата.
pub const WIRE_VERSION: u16 = 2;
/// Наибольший размер датаграммы целиком, включая заголовок и тег AEAD.
pub const MAX_DATAGRAM: usize = 1200;
/// Длина открытого заголовка: сигнатура, версия, тип и идентификатор соединения.
pub const HEADER_LEN: usize = 11;
/// Номер пакета зашифрованной датаграммы; он же входит в nonce AEAD.
pub const SEQ_LEN: usize = 8;
/// Длина тега ChaCha20-Poly1305.
pub const TAG_LEN: usize = 16;
/// Наибольший полезный объём одного фрагмента в кадре.
pub const FRAGMENT_PAYLOAD: usize = 1000;
/// Наибольший размер сообщения канала, включая все фрагменты.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Тип датаграммы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hello,
    Cookie,
    Confirm,
    Accept,
    Reject,
    /// Зашифрованная датаграмма соединения.
    Data,
}

impl Kind {
    fn to_u8(self) -> u8 {
        match self {
            Self::Hello => 1,
            Self::Cookie => 2,
            Self::Confirm => 3,
            Self::Accept => 4,
            Self::Reject => 5,
            Self::Data => 6,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Cookie,
            3 => Self::Confirm,
            4 => Self::Accept,
            5 => Self::Reject,
            6 => Self::Data,
            _ => return None,
        })
    }
}

/// Открытый заголовок: тип и идентификатор соединения (0 до рукопожатия).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub conn_id: u32,
}

impl Header {
    pub fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&WIRE_VERSION.to_be_bytes());
        out.push(self.kind.to_u8());
        out.extend_from_slice(&self.conn_id.to_be_bytes());
    }

    /// Разбирает заголовок. `None` — датаграмма не относится к протоколу RVE.
    pub fn decode(datagram: &[u8]) -> Option<Self> {
        if datagram.len() < HEADER_LEN || datagram[..4] != MAGIC {
            return None;
        }
        if u16::from_be_bytes([datagram[4], datagram[5]]) != WIRE_VERSION {
            return None;
        }
        let kind = Kind::from_u8(datagram[6])?;
        let conn_id = u32::from_be_bytes([datagram[7], datagram[8], datagram[9], datagram[10]]);
        Some(Self { kind, conn_id })
    }
}

/// Дополняет датаграмму нулями до `MAX_DATAGRAM`: рукопожатие не должно быть короче запроса.
pub(crate) fn pad_to_max(out: &mut Vec<u8>) {
    if out.len() < MAX_DATAGRAM {
        out.resize(MAX_DATAGRAM, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let header = Header {
            kind: Kind::Data,
            conn_id: 0xdead_beef,
        };
        let mut bytes = Vec::new();
        header.encode(&mut bytes);
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(Header::decode(&bytes), Some(header));
    }

    #[test]
    fn foreign_or_short_datagrams_are_not_protocol_packets() {
        assert_eq!(Header::decode(b"RVEN"), None);
        assert_eq!(Header::decode(b"XXXX\0\x01\x06\0\0\0\0"), None);
        let mut bytes = Vec::new();
        Header {
            kind: Kind::Hello,
            conn_id: 0,
        }
        .encode(&mut bytes);
        bytes[5] = 9;
        assert_eq!(Header::decode(&bytes), None, "неизвестная версия");
        bytes[5] = 1;
        bytes[6] = 0;
        assert_eq!(Header::decode(&bytes), None, "неизвестный тип");
    }
}
