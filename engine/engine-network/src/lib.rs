//! Сетевой модуль RVE: протокол поверх UDP с шифрованием и модель «клиент-сервер».
//!
//! [`Server`] и [`Client`] — машины состояний без ввода-вывода: им передают датаграммы и текущее
//! время, они возвращают исходящие датаграммы и события ([`Output`]). [`NetworkEngine`] владеет
//! сокетом и потоком `rve-network`, [`NetworkHandle`] даёт доступ к нему из других потоков.

mod bridge;
mod client;
pub mod codec;
mod crypto;
mod engine;
mod error;
mod handshake;
mod interpolation;
mod link;
mod packet;
mod reliable;
pub mod replication;
mod server;
mod transport;

#[cfg(test)]
mod bridge_tests;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;

use engine_scene::network::PeerId;

use crate::packet::{FRAGMENT_PAYLOAD, MAX_MESSAGE_BYTES};

pub use bridge::{
    BridgeConfig, FAULT_LIMIT, FAULT_WINDOW, NetLink, NetworkBridge, SpawnedRoot, WorldSink,
};
pub use client::{Client, ClientConfig};
pub use crypto::{Password, ServerIdentity, parse_fingerprint};
pub use engine::{NetRole, NetworkConfig, NetworkEngine, NetworkHandle};
pub use error::NetError;
pub use server::{MAX_CLIENTS, Server, ServerConfig};
pub use transport::{DatagramSocket, Faults, MemoryEndpoint, MemoryNetwork, UdpTransport};

/// Версия крейта.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Идентификатор протокола проекта: `blake3(длина id ‖ id ‖ version)`. Длина перед `id` не даёт разным
/// парам (`"ab"`, `"c"`) и (`"a"`, `"bc"`) совпасть. Клиент и сервер одной сборки получают одинаковое значение.
pub fn protocol_id(id: &str, version: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(id.len() as u64).to_le_bytes());
    hasher.update(id.as_bytes());
    hasher.update(version.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Канал доставки: задаёт надёжность и порядок сообщения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Надёжный и упорядоченный: управление, spawn и despawn.
    Reliable,
    /// Надёжный и упорядоченный, отдельный от `Reliable`, чтобы RPC не задерживали spawn.
    Rpc,
    /// Ненадёжный: при потере доставляется более новое состояние, старое отбрасывается.
    State,
    /// Ненадёжный и без порядка.
    Unreliable,
}

impl Channel {
    const ALL: [Channel; 4] = [Self::Reliable, Self::Rpc, Self::State, Self::Unreliable];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        Self::ALL.get(usize::from(value)).copied()
    }

    pub(crate) fn is_reliable(self) -> bool {
        matches!(self, Self::Reliable | Self::Rpc)
    }
}

/// Проверяет размер сообщения до постановки в очередь: надёжные — до 64 KiB, ненадёжные — один кадр.
pub(crate) fn check_message_size(channel: Channel, size: usize) -> Result<(), NetError> {
    let limit = if channel.is_reliable() {
        MAX_MESSAGE_BYTES
    } else {
        FRAGMENT_PAYLOAD
    };
    if size > limit {
        Err(NetError::MessageTooLarge { size, limit })
    } else {
        Ok(())
    }
}

/// Событие сети для приложения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkEvent {
    /// Номер, который сервер выдал этому клиенту. Приходит перед `Connected`.
    LocalPeer { peer: PeerId },
    /// Участник подключён. На клиенте `peer` — сервер.
    Connected { peer: PeerId },
    /// Участник отключён; `reason` — причина на русском языке.
    Disconnected { peer: PeerId, reason: String },
    /// Получено сообщение по каналу.
    Message {
        peer: PeerId,
        channel: Channel,
        bytes: Vec<u8>,
    },
}

/// Результат шага машины состояний: датаграммы для отправки и события для приложения.
#[derive(Debug, Default)]
pub struct Output {
    /// Адрес назначения и готовая датаграмма.
    pub datagrams: Vec<(SocketAddr, Vec<u8>)>,
    pub events: Vec<NetworkEvent>,
}

#[cfg(test)]
mod identity_tests {
    use super::{parse_fingerprint, protocol_id};

    #[test]
    fn protocol_id_depends_on_project_id_and_version_without_ambiguity() {
        assert_eq!(protocol_id("demo", "1"), protocol_id("demo", "1"));
        assert_ne!(protocol_id("demo", "1"), protocol_id("demo", "2"));
        assert_ne!(protocol_id("demo", "1"), protocol_id("other", "1"));
        assert_ne!(protocol_id("ab", "c"), protocol_id("a", "bc"));
    }

    #[test]
    fn protocol_id_value_is_pinned_for_wire_compatibility() {
        let id = protocol_id("demo", "1.2.0");
        assert_eq!(
            id,
            [
                0x36, 0xee, 0xf5, 0xf5, 0xc4, 0x29, 0x66, 0xd6, 0x56, 0xe3, 0x88, 0xd3, 0x0f, 0x27,
                0x0f, 0xe5, 0xc2, 0xe5, 0x4d, 0xd2, 0xef, 0x9e, 0xab, 0x81, 0x8e, 0x22, 0x30, 0x59,
                0x59, 0x77, 0x61, 0x2a,
            ]
        );
    }

    #[test]
    fn fingerprint_accepts_64_hex_digits_in_any_case() {
        let lower = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let upper = lower.to_ascii_uppercase();
        let expected = parse_fingerprint(lower).unwrap();
        assert_eq!(expected[0], 0x00);
        assert_eq!(expected[1], 0x11);
        assert_eq!(expected[31], 0xff);
        assert_eq!(parse_fingerprint(&upper), Some(expected));
    }

    #[test]
    fn fingerprint_rejects_wrong_length_and_non_hex_digits() {
        let valid = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        assert_eq!(parse_fingerprint(""), None);
        assert_eq!(parse_fingerprint(&valid[..63]), None);
        assert_eq!(parse_fingerprint(&format!("{valid}0")), None);
        let mut bad = valid.to_owned();
        bad.replace_range(10..11, "g");
        assert_eq!(parse_fingerprint(&bad), None);
    }
}
