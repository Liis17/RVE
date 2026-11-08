//! Сетевой поток `rve-network`: владеет сокетом и машиной состояний, принимает команды из других
//! потоков и отдаёт события. Поток не ждёт команд без таймаута: сокет читается с короткой задержкой,
//! а между чтениями выполняются команды, таймеры и отправка.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_scene::network::{NetTarget, PeerId, SERVER_PEER_ID};
use flume::{Receiver, Sender, TryRecvError, TrySendError};

use crate::client::{Client, ClientConfig};
use crate::server::{Server, ServerConfig};
use crate::transport::{DatagramSocket, UdpTransport};
use crate::{Channel, NetError, NetworkEvent, Output, Password, ServerIdentity};

const COMMAND_QUEUE: usize = 256;
const EVENT_QUEUE: usize = 1024;

/// Роль процесса в сети.
pub enum NetRole {
    Server {
        bind: SocketAddr,
        max_clients: usize,
        identity: ServerIdentity,
        password: Option<Password>,
    },
    Client {
        server_addr: SocketAddr,
        /// Закреплённый отпечаток ключа сервера.
        fingerprint: Option<[u8; 32]>,
        /// Подключение без отпечатка; без него отсутствие отпечатка — ошибка.
        insecure_no_pin: bool,
        password: Option<Password>,
    },
}

/// Настройки сетевого модуля.
pub struct NetworkConfig {
    pub role: NetRole,
    pub protocol_id: [u8; 32],
    pub scene_hash: [u8; 32],
    /// Срок молчания до разрыва соединения.
    pub timeout: Duration,
}

enum Command {
    Send {
        target: NetTarget,
        channel: Channel,
        bytes: Vec<u8>,
    },
    Disconnect {
        peer: PeerId,
        reason: String,
    },
    Shutdown,
}

/// Клонируемый доступ к сетевому потоку из других потоков.
#[derive(Clone)]
pub struct NetworkHandle {
    commands: Sender<Command>,
    events: Receiver<NetworkEvent>,
}

impl NetworkHandle {
    /// Ставит сообщение в очередь потока. `QueueFull` — очередь команд заполнена.
    pub fn send(&self, target: NetTarget, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        let command = Command::Send {
            target,
            channel,
            bytes: bytes.to_vec(),
        };
        self.commands.try_send(command).map_err(queue_error)
    }

    pub fn disconnect(&self, peer: PeerId, reason: &str) -> Result<(), NetError> {
        let command = Command::Disconnect {
            peer,
            reason: reason.to_owned(),
        };
        self.commands.try_send(command).map_err(queue_error)
    }

    /// Останавливает поток. Ждёт места в очереди команд, если она полна.
    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }

    /// Забирает накопленные события.
    pub fn poll_events(&self) -> Vec<NetworkEvent> {
        self.events.try_iter().collect()
    }
}

fn queue_error<T>(err: TrySendError<T>) -> NetError {
    match err {
        TrySendError::Full(_) => NetError::QueueFull,
        TrySendError::Disconnected(_) => NetError::Stopped,
    }
}

/// Владелец сетевого потока. [`NetworkEngine::run`] блокирует вызывающий поток.
pub struct NetworkEngine {
    machine: Machine,
    socket: Box<dyn DatagramSocket>,
    commands: Receiver<Command>,
    events: Sender<NetworkEvent>,
    pending: VecDeque<NetworkEvent>,
}

impl NetworkEngine {
    /// Проверяет настройки и открывает сокет. Ошибка настройки или bind возвращается сразу,
    /// до запуска потока.
    pub fn new(config: NetworkConfig) -> Result<(Self, NetworkHandle), NetError> {
        let NetworkConfig {
            role,
            protocol_id,
            scene_hash,
            timeout,
        } = config;
        let now = Instant::now();
        let (machine, transport) = match role {
            NetRole::Server {
                bind,
                max_clients,
                identity,
                password,
            } => {
                log::info!("отпечаток ключа сервера: {}", identity.fingerprint_hex());
                let server = Server::new(
                    ServerConfig {
                        protocol_id,
                        scene_hash,
                        max_clients,
                        password,
                        timeout,
                    },
                    identity,
                    now,
                )?;
                (Machine::Server(Box::new(server)), UdpTransport::bind(bind)?)
            }
            NetRole::Client {
                server_addr,
                fingerprint,
                insecure_no_pin,
                password,
            } => {
                let client = Client::new(
                    ClientConfig {
                        protocol_id,
                        scene_hash,
                        server: server_addr,
                        fingerprint,
                        insecure_no_pin,
                        password,
                        timeout,
                    },
                    now,
                )?;
                let any = SocketAddr::from(([0, 0, 0, 0], 0));
                (Machine::Client(Box::new(client)), UdpTransport::bind(any)?)
            }
        };
        Ok(Self::from_parts(machine, Box::new(transport)))
    }

    pub(crate) fn from_parts(
        machine: Machine,
        socket: Box<dyn DatagramSocket>,
    ) -> (Self, NetworkHandle) {
        let (command_tx, command_rx) = flume::bounded(COMMAND_QUEUE);
        let (event_tx, event_rx) = flume::bounded(EVENT_QUEUE);
        let engine = Self {
            machine,
            socket,
            commands: command_rx,
            events: event_tx,
            pending: VecDeque::new(),
        };
        let handle = NetworkHandle {
            commands: command_tx,
            events: event_rx,
        };
        (engine, handle)
    }

    /// Работает до `shutdown` или до отбрасывания всех хендлов. Перед выходом отправляет отключение.
    pub fn run(mut self) {
        log::info!("сетевой поток запущен");
        loop {
            if !self.apply_commands() {
                break;
            }
            let now = Instant::now();
            // Пока события не доставлены приложению, новые датаграммы не читаются: иначе очередь растёт без предела.
            if self.pending.is_empty() {
                match self.socket.recv_from() {
                    Ok(Some((from, bytes))) => {
                        let out = self.machine.handle_datagram(from, &bytes, now);
                        self.dispatch(out);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log::error!("сетевой сокет остановлен: {err}");
                        break;
                    }
                }
            }
            let out = self.machine.update(now);
            self.dispatch(out);
            self.deliver_events();
        }
        let out = self.machine.shutdown();
        for (addr, datagram) in out.datagrams {
            let _ = self.socket.send_to(addr, &datagram);
        }
        log::info!("сетевой поток остановлен");
    }

    /// Возвращает `false`, если поток нужно остановить.
    fn apply_commands(&mut self) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(Command::Send {
                    target,
                    channel,
                    bytes,
                }) => {
                    if let Err(err) = self.machine.send(target, channel, &bytes) {
                        log::warn!("сообщение сети не отправлено: {err}");
                    }
                }
                Ok(Command::Disconnect { peer, reason }) => {
                    let out = self.machine.disconnect(peer, &reason);
                    self.dispatch(out);
                }
                Ok(Command::Shutdown) => return false,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn dispatch(&mut self, out: Output) {
        for (addr, datagram) in out.datagrams {
            if let Err(err) = self.socket.send_to(addr, &datagram) {
                log::debug!("датаграмма на {addr} не отправлена: {err}");
            }
        }
        self.pending.extend(out.events);
    }

    fn deliver_events(&mut self) {
        while let Some(event) = self.pending.pop_front() {
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.pending.push_front(event);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.pending.clear();
                    return;
                }
            }
        }
    }
}

/// Машина состояний протокола в одном из двух режимов.
pub(crate) enum Machine {
    Server(Box<Server>),
    Client(Box<Client>),
}

impl Machine {
    fn handle_datagram(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Output {
        match self {
            Self::Server(server) => server.handle_datagram(from, bytes, now),
            Self::Client(client) => client.handle_datagram(from, bytes, now),
        }
    }

    fn update(&mut self, now: Instant) -> Output {
        match self {
            Self::Server(server) => server.update(now),
            Self::Client(client) => client.update(now),
        }
    }

    fn send(&mut self, target: NetTarget, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        match self {
            Self::Server(server) => match target {
                NetTarget::Peer(peer) => server.send(peer, channel, bytes),
                NetTarget::All => server.broadcast(channel, bytes),
                NetTarget::Server => Err(NetError::UnknownPeer),
            },
            Self::Client(client) => match target {
                NetTarget::Server | NetTarget::All => client.send(channel, bytes),
                NetTarget::Peer(peer) if peer == SERVER_PEER_ID => client.send(channel, bytes),
                NetTarget::Peer(_) => Err(NetError::UnknownPeer),
            },
        }
    }

    fn disconnect(&mut self, peer: PeerId, reason: &str) -> Output {
        match self {
            Self::Server(server) => server.disconnect(peer, reason),
            Self::Client(client) if peer == SERVER_PEER_ID => client.disconnect(reason),
            Self::Client(_) => Output::default(),
        }
    }

    fn shutdown(&mut self) -> Output {
        match self {
            Self::Server(server) => server.shutdown("сервер остановлен"),
            Self::Client(client) => client.disconnect("клиент остановлен"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Faults, MemoryNetwork};

    fn idle_engine() -> (NetworkEngine, NetworkHandle) {
        let network = MemoryNetwork::new(1, Faults::default());
        let endpoint = network.endpoint(SocketAddr::from(([127, 0, 0, 1], 9000)));
        let server = Server::new(
            ServerConfig {
                protocol_id: [1; 32],
                scene_hash: [2; 32],
                max_clients: 1,
                password: None,
                timeout: Duration::from_secs(5),
            },
            ServerIdentity::ephemeral().unwrap(),
            Instant::now(),
        )
        .unwrap();
        NetworkEngine::from_parts(Machine::Server(Box::new(server)), Box::new(endpoint))
    }

    #[test]
    fn shutdown_command_ends_run() {
        let (engine, handle) = idle_engine();
        let worker = std::thread::spawn(move || engine.run());
        handle.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn run_ends_when_every_handle_is_dropped() {
        let (engine, handle) = idle_engine();
        let worker = std::thread::spawn(move || engine.run());
        drop(handle);
        worker.join().unwrap();
    }

    #[test]
    fn full_command_queue_reports_queue_full_instead_of_blocking() {
        let (_engine, handle) = idle_engine();
        for _ in 0..COMMAND_QUEUE {
            handle
                .send(NetTarget::All, Channel::Reliable, b"x")
                .unwrap();
        }
        assert_eq!(
            handle.send(NetTarget::All, Channel::Reliable, b"x"),
            Err(NetError::QueueFull)
        );
    }

    #[test]
    fn commands_after_the_engine_is_gone_report_stopped() {
        let (engine, handle) = idle_engine();
        drop(engine);
        assert_eq!(
            handle.send(NetTarget::All, Channel::Reliable, b"x"),
            Err(NetError::Stopped)
        );
    }

    #[test]
    fn client_targets_only_the_server_peer() {
        let client = Client::new(
            ClientConfig {
                protocol_id: [1; 32],
                scene_hash: [2; 32],
                server: SocketAddr::from(([127, 0, 0, 1], 9001)),
                fingerprint: Some([3; 32]),
                insecure_no_pin: false,
                password: None,
                timeout: Duration::from_secs(5),
            },
            Instant::now(),
        )
        .unwrap();
        let mut machine = Machine::Client(Box::new(client));
        assert_eq!(
            machine.send(NetTarget::Peer(5), Channel::Reliable, b"x"),
            Err(NetError::UnknownPeer)
        );
        assert_eq!(
            machine.send(NetTarget::Server, Channel::Reliable, b"x"),
            Err(NetError::NotConnected),
            "соединение ещё не установлено"
        );
    }
}
