//! Сетевой поток `rve-network`: владеет сокетом и машиной состояний, принимает команды из других
//! потоков и отдаёт события. Поток не ждёт команд без таймаута: сокет читается с короткой задержкой,
//! а между чтениями выполняются команды, таймеры и отправка.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use engine_scene::network::{NetTarget, PeerId, SERVER_PEER_ID};
use flume::{Receiver, Sender, TryRecvError, TrySendError};

use crate::client::{Client, ClientConfig};
use crate::server::{Server, ServerConfig};
use crate::transport::{DatagramSocket, UdpTransport};
use crate::{
    Channel, NetError, NetworkEvent, Output, Password, ServerIdentity, check_message_size,
};

const COMMAND_QUEUE: usize = 256;
const EVENT_QUEUE: usize = 1024;
/// Сколько событий может ждать приложения, прежде чем сеть перестаёт обрабатывать датаграммы.
const PENDING_LIMIT: usize = EVENT_QUEUE;
/// Ошибки приёма подряд, после которых сокет считается неисправным.
const MAX_RECV_ERRORS: u32 = 64;

/// Локальный адрес клиента того же семейства, что и адрес сервера.
fn client_bind_addr(server: SocketAddr) -> SocketAddr {
    if server.is_ipv6() {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    }
}

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
    /// Ставит сообщение в очередь потока. `MessageTooLarge` — размер проверен сразу, до очереди.
    /// `QueueFull` — очередь команд заполнена.
    pub fn send(&self, target: NetTarget, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        check_message_size(channel, bytes.len())?;
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
    /// Ошибки приёма подряд; успешный приём сбрасывает счётчик.
    recv_errors: u32,
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
                let bind = client_bind_addr(server_addr);
                (Machine::Client(Box::new(client)), UdpTransport::bind(bind)?)
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
            recv_errors: 0,
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
            if !self.step(Instant::now()) {
                break;
            }
        }
        let out = self.machine.shutdown();
        for (addr, datagram) in out.datagrams {
            let _ = self.socket.send_to(addr, &datagram);
        }
        log::info!("сетевой поток остановлен");
    }

    /// Один шаг цикла: доставка событий, приём одной датаграммы, таймеры и отправка.
    /// Возвращает `false`, если сокет неисправен и поток нужно остановить.
    fn step(&mut self, now: Instant) -> bool {
        self.deliver_events();
        // Пока приложение не забрало накопленные события, датаграммы не обрабатываются: отправитель
        // повторит их без подтверждения, а таймаут соединения не идёт (см. `hold_timeouts`).
        let backlogged = self.pending.len() >= PENDING_LIMIT;
        match self.socket.recv_from() {
            Ok(Some((from, bytes))) => {
                self.recv_errors = 0;
                if !backlogged {
                    let out = self.machine.handle_datagram(from, &bytes, now);
                    self.dispatch(out);
                }
            }
            Ok(None) => self.recv_errors = 0,
            Err(err) => {
                self.recv_errors += 1;
                if self.recv_errors > MAX_RECV_ERRORS {
                    log::error!(
                        "сетевой сокет остановлен после {} ошибок подряд: {err}",
                        self.recv_errors
                    );
                    return false;
                }
                if self.recv_errors.is_power_of_two() {
                    log::warn!("ошибка приёма сети ({} подряд): {err}", self.recv_errors);
                }
            }
        }
        if backlogged {
            self.machine.hold_timeouts(now);
        }
        let out = self.machine.update(now);
        self.dispatch(out);
        self.deliver_events();
        true
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

    fn hold_timeouts(&mut self, now: Instant) {
        match self {
            Self::Server(server) => server.hold_timeouts(now),
            Self::Client(client) => client.hold_timeouts(now),
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
    use crate::transport::{Faults, MemoryEndpoint, MemoryNetwork};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn idle_server() -> Server {
        Server::new(
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
        .unwrap()
    }

    fn idle_engine() -> (NetworkEngine, NetworkHandle) {
        let network = MemoryNetwork::new(1, Faults::default());
        let endpoint = network.endpoint(addr(9000));
        NetworkEngine::from_parts(Machine::Server(Box::new(idle_server())), Box::new(endpoint))
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

    #[test]
    fn oversized_messages_are_refused_by_the_handle_immediately() {
        let (_engine, handle) = idle_engine();
        assert!(matches!(
            handle.send(NetTarget::All, Channel::State, &[0; 1001]),
            Err(NetError::MessageTooLarge { size: 1001, .. })
        ));
        assert!(matches!(
            handle.send(NetTarget::All, Channel::Reliable, &vec![0; 64 * 1024 + 1]),
            Err(NetError::MessageTooLarge { .. })
        ));
        assert!(
            handle
                .send(NetTarget::All, Channel::Reliable, &vec![0; 64 * 1024])
                .is_ok()
        );
    }

    #[test]
    fn client_binds_to_the_address_family_of_the_server() {
        let v4 = client_bind_addr(SocketAddr::from(([203, 0, 113, 5], 7777)));
        assert!(v4.is_ipv4() && v4.ip().is_unspecified());
        let v6 = client_bind_addr(SocketAddr::from((Ipv6Addr::LOCALHOST, 7777)));
        assert!(v6.is_ipv6() && v6.ip().is_unspecified());
    }

    /// Сокет, который всегда сообщает о неожиданной ошибке.
    struct BrokenSocket;

    impl DatagramSocket for BrokenSocket {
        fn send_to(&mut self, _: SocketAddr, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }

        fn recv_from(&mut self) -> std::io::Result<Option<(SocketAddr, Vec<u8>)>> {
            Err(std::io::Error::other("сбой"))
        }
    }

    /// Сокет, который через одну операцию сообщает об ошибке.
    struct FlakySocket {
        calls: u32,
    }

    impl DatagramSocket for FlakySocket {
        fn send_to(&mut self, _: SocketAddr, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }

        fn recv_from(&mut self) -> std::io::Result<Option<(SocketAddr, Vec<u8>)>> {
            self.calls += 1;
            if self.calls.is_multiple_of(2) {
                Err(std::io::Error::other("сбой"))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn unexpected_socket_errors_stop_the_engine_only_after_a_long_run() {
        let (mut engine, _handle) = NetworkEngine::from_parts(
            Machine::Server(Box::new(idle_server())),
            Box::new(BrokenSocket),
        );
        let now = Instant::now();
        for _ in 0..MAX_RECV_ERRORS {
            assert!(engine.step(now));
        }
        assert!(!engine.step(now), "серия ошибок подряд останавливает поток");
    }

    #[test]
    fn a_successful_receive_resets_the_error_count() {
        let (mut engine, _handle) = NetworkEngine::from_parts(
            Machine::Server(Box::new(idle_server())),
            Box::new(FlakySocket { calls: 0 }),
        );
        let now = Instant::now();
        for _ in 0..MAX_RECV_ERRORS * 4 {
            assert!(engine.step(now), "ошибки не идут подряд");
        }
    }

    fn message_numbers(events: &[NetworkEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|event| match event {
                NetworkEvent::Message { bytes, .. } => {
                    Some(u32::from_be_bytes(bytes[..4].try_into().unwrap()))
                }
                _ => None,
            })
            .collect()
    }

    /// Сервер в движке и клиент в сети в памяти. Время и шаги задаёт тест, без задержек.
    struct Rig {
        engine: NetworkEngine,
        handle: NetworkHandle,
        client: Client,
        client_socket: MemoryEndpoint,
        now: Instant,
        client_events: Vec<NetworkEvent>,
    }

    impl Rig {
        fn new(timeout: Duration) -> Self {
            let now = Instant::now();
            let network = MemoryNetwork::new(7, Faults::default());
            let identity = ServerIdentity::ephemeral().unwrap();
            let fingerprint = identity.fingerprint();
            let server = Server::new(
                ServerConfig {
                    protocol_id: [1; 32],
                    scene_hash: [2; 32],
                    max_clients: 2,
                    password: None,
                    timeout,
                },
                identity,
                now,
            )
            .unwrap();
            let (engine, handle) = NetworkEngine::from_parts(
                Machine::Server(Box::new(server)),
                Box::new(network.endpoint(addr(7100))),
            );
            let client = Client::new(
                ClientConfig {
                    protocol_id: [1; 32],
                    scene_hash: [2; 32],
                    server: addr(7100),
                    fingerprint: Some(fingerprint),
                    insecure_no_pin: false,
                    password: None,
                    timeout,
                },
                now,
            )
            .unwrap();
            Self {
                engine,
                handle,
                client,
                client_socket: network.endpoint(addr(7101)),
                now,
                client_events: Vec::new(),
            }
        }

        fn route_client(&mut self, out: Output) {
            for (to, datagram) in out.datagrams {
                self.client_socket.send_to(to, &datagram).unwrap();
            }
            self.client_events.extend(out.events);
        }

        /// Один шаг времени: клиент шлёт, движок обрабатывает датаграммы, клиент принимает ответы.
        fn tick(&mut self) {
            self.now += Duration::from_millis(10);
            let out = self.client.update(self.now);
            self.route_client(out);
            for _ in 0..16 {
                assert!(self.engine.step(self.now));
            }
            while let Some((from, bytes)) = self.client_socket.recv_from().unwrap() {
                let out = self.client.handle_datagram(from, &bytes, self.now);
                self.route_client(out);
            }
        }

        fn connect(&mut self) -> bool {
            for _ in 0..500 {
                self.tick();
                if self.client.is_connected() {
                    return true;
                }
            }
            false
        }
    }

    /// Регрессия: полная очередь событий не должна разрывать здоровое соединение по таймауту.
    /// Раньше датаграммы не читались, `last_recv` не обновлялся и пир отключался.
    #[test]
    fn full_event_queue_does_not_drop_a_healthy_connection() {
        let timeout = Duration::from_secs(1);
        let mut rig = Rig::new(timeout);
        assert!(rig.connect(), "рукопожатие проходит");

        let total = EVENT_QUEUE * 2 + 100;
        for number in 0..total {
            rig.client
                .send(Channel::Reliable, &(number as u32).to_be_bytes())
                .unwrap();
        }
        // Приложение не забирает события: очередь событий заполняется до предела.
        let mut ticks = 0;
        while rig.engine.pending.len() < PENDING_LIMIT && ticks < 2_000 {
            rig.tick();
            ticks += 1;
        }
        assert!(
            rig.engine.pending.len() >= PENDING_LIMIT,
            "очередь переполнена"
        );

        // Дольше таймаута без чтения: соединение должно остаться живым.
        for _ in 0..300 {
            rig.tick();
        }
        assert!(
            rig.engine
                .pending
                .iter()
                .all(|event| !matches!(event, NetworkEvent::Disconnected { .. })),
            "соединение не разорвано"
        );

        // Приложение начинает читать: все сообщения доходят по порядку.
        let mut received = Vec::new();
        for _ in 0..5_000 {
            rig.tick();
            received.extend(rig.handle.poll_events());
            if message_numbers(&received).len() == total {
                break;
            }
        }
        assert!(
            !received
                .iter()
                .any(|event| matches!(event, NetworkEvent::Disconnected { .. })),
            "отключения не было"
        );
        assert_eq!(
            message_numbers(&received),
            (0..total as u32).collect::<Vec<_>>()
        );
        assert!(rig.client.is_connected());
    }
}
