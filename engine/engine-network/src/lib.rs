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
pub use crypto::{Password, ServerIdentity};
pub use engine::{NetRole, NetworkConfig, NetworkEngine, NetworkHandle};
pub use error::NetError;
pub use server::{MAX_CLIENTS, Server, ServerConfig};
pub use transport::{DatagramSocket, Faults, MemoryEndpoint, MemoryNetwork, UdpTransport};

/// Версия крейта.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
