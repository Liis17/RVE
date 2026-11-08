//! Сетевой поток `rve-network`: владеет сокетом и машиной состояний, принимает команды из других
//! потоков и отдаёт события. Поток не ждёт команд без таймаута: сокет читается с короткой задержкой,
//! а между чтениями выполняются команды, таймеры и отправка.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use engine_scene::network::{NetTarget, PeerId, SERVER_PEER_ID};
use flume::{Receiver, Sender, TryRecvError, TrySendError};

use crate::client::{Client, ClientConfig};
use crate::server::{Server, ServerConfig};
use crate::transport::{DatagramSocket, RECV_TIMEOUT, UdpTransport};
use crate::{
    Channel, NetError, NetworkEvent, Output, Password, ServerIdentity, check_message_size,
};

const COMMAND_QUEUE: usize = 256;
const EVENT_QUEUE: usize = 1024;
/// Сколько событий может ждать приложения, прежде чем очередь считается переполненной. Тогда датаграммы пира,
/// который занял свою долю очереди, не принимаются.
const PENDING_LIMIT: usize = EVENT_QUEUE;
/// Непрерывное переполнение очереди, во время которого таймауты пиров не идут. Дольше таймауты идут обычно,
/// иначе молчащий пир висел бы, пока приложение не читает события.
const HOLD_LIMIT: Duration = Duration::from_secs(10);
/// Ошибки приёма подряд дольше этого срока считаются неисправным сокетом.
const RECV_ERROR_LIMIT: Duration = Duration::from_secs(5);
/// Причина отключения пира, который занял свою долю очереди событий, пока очередь переполнена.
const OVERLOAD_REASON: &str = "слишком много сообщений, очередь переполнена";

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
    /// Число событий в `pending` по пирам: по нему видно, чья доля очереди заполнена.
    pending_by_peer: HashMap<PeerId, usize>,
    /// С какого момента очередь событий непрерывно переполнена.
    backlog_since: Option<Instant>,
    /// Когда началась серия ошибок приёма, и число ошибок в ней.
    recv_error_since: Option<Instant>,
    recv_errors: u32,
    /// Последний шаг закончился ошибкой приёма: перед следующим чтением поток делает короткую паузу.
    recv_failed: bool,
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
            pending_by_peer: HashMap::new(),
            backlog_since: None,
            recv_error_since: None,
            recv_errors: 0,
            recv_failed: false,
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
            if std::mem::take(&mut self.recv_failed) {
                // Неисправный сокет возвращает ошибку сразу: пауза не даёт крутить цикл впустую.
                std::thread::sleep(RECV_TIMEOUT);
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
        let backlogged = self.pending.len() >= PENDING_LIMIT;
        self.backlog_since = if backlogged {
            self.backlog_since.or(Some(now))
        } else {
            None
        };
        match self.socket.recv_from() {
            Ok(Some((from, bytes))) => {
                self.reset_recv_errors();
                self.receive(from, &bytes, now, backlogged);
            }
            Ok(None) => self.reset_recv_errors(),
            Err(err) => {
                let since = *self.recv_error_since.get_or_insert(now);
                self.recv_errors += 1;
                self.recv_failed = true;
                if now.saturating_duration_since(since) > RECV_ERROR_LIMIT {
                    log::error!(
                        "сетевой сокет остановлен: ошибки приёма ({} подряд) длятся дольше {:?}: {err}",
                        self.recv_errors,
                        RECV_ERROR_LIMIT
                    );
                    return false;
                }
                if self.recv_errors.is_power_of_two() {
                    log::warn!("ошибка приёма сети ({} подряд): {err}", self.recv_errors);
                }
            }
        }
        // Таймауты сдвигаются только на время непрерывного переполнения не дольше `HOLD_LIMIT`.
        if let Some(since) = self.backlog_since
            && now.saturating_duration_since(since) <= HOLD_LIMIT
        {
            self.machine.hold_timeouts(now);
        }
        let out = self.machine.update(now);
        self.dispatch(out);
        self.deliver_events();
        true
    }

    fn reset_recv_errors(&mut self) {
        self.recv_errors = 0;
        self.recv_error_since = None;
    }

    /// Передаёт датаграмму машине состояний. Пока очередь переполнена, датаграмма пира, который уже занял
    /// свою долю очереди, не принимается: если в сети есть другие пиры, такой пир отключается с причиной,
    /// иначе его отправитель получает противодавление (повтор без подтверждения). Датаграммы остальных
    /// пиров обрабатываются и подтверждаются. Датаграммы без известного пира при переполнении откладываются.
    fn receive(&mut self, from: SocketAddr, bytes: &[u8], now: Instant, backlogged: bool) {
        if backlogged {
            let Some(peer) = self.machine.peer_of(from) else {
                return;
            };
            let peers = self.machine.peer_count();
            let share = PENDING_LIMIT / peers.max(1);
            if self.pending_by_peer.get(&peer).copied().unwrap_or(0) >= share {
                if peers >= 2 {
                    let out = self.machine.disconnect(peer, OVERLOAD_REASON);
                    self.dispatch(out);
                }
                return;
            }
        }
        let out = self.machine.handle_datagram(from, bytes, now);
        self.dispatch(out);
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
        for event in out.events {
            *self.pending_by_peer.entry(event_peer(&event)).or_insert(0) += 1;
            self.pending.push_back(event);
        }
    }

    fn deliver_events(&mut self) {
        while let Some(event) = self.pending.pop_front() {
            let peer = event_peer(&event);
            match self.events.try_send(event) {
                Ok(()) => self.release(peer),
                Err(TrySendError::Full(event)) => {
                    self.pending.push_front(event);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.pending.clear();
                    self.pending_by_peer.clear();
                    return;
                }
            }
        }
    }

    /// Событие покинуло очередь ожидания: доля пира освободилась.
    fn release(&mut self, peer: PeerId) {
        if let Some(count) = self.pending_by_peer.get_mut(&peer) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.pending_by_peer.remove(&peer);
            }
        }
    }
}

/// Пир, к которому относится событие.
fn event_peer(event: &NetworkEvent) -> PeerId {
    match event {
        NetworkEvent::Connected { peer }
        | NetworkEvent::Disconnected { peer, .. }
        | NetworkEvent::Message { peer, .. } => *peer,
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

    /// Пир, которому принадлежит датаграмма от `from`. Клиент знает только сервер.
    fn peer_of(&self, from: SocketAddr) -> Option<PeerId> {
        match self {
            Self::Server(server) => server.peer_of(from),
            Self::Client(client) => client.local_peer(),
        }
    }

    fn peer_count(&self) -> usize {
        match self {
            Self::Server(server) => server.peer_count(),
            Self::Client(client) => usize::from(client.is_connected()),
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

    /// Регрессия: сотни ошибок приёма за долю секунды не останавливают поток. Остановка зависит от срока
    /// серии ошибок, а не от их числа: иначе быстрый сбой сокета гасил сеть за микросекунды.
    #[test]
    fn unexpected_socket_errors_stop_the_engine_only_after_a_long_run() {
        let (mut engine, _handle) = NetworkEngine::from_parts(
            Machine::Server(Box::new(idle_server())),
            Box::new(BrokenSocket),
        );
        let start = Instant::now();
        for step in 0..10_000u32 {
            let now = start + Duration::from_micros(u64::from(step) * 10);
            assert!(
                engine.step(now),
                "серия ошибок короче срока не останавливает поток"
            );
        }
        assert!(engine.recv_failed, "после ошибки поток делает паузу");
        let later = start + RECV_ERROR_LIMIT + Duration::from_millis(1);
        assert!(
            !engine.step(later),
            "серия ошибок дольше срока останавливает поток"
        );
    }

    #[test]
    fn a_successful_receive_resets_the_error_window() {
        let (mut engine, _handle) = NetworkEngine::from_parts(
            Machine::Server(Box::new(idle_server())),
            Box::new(FlakySocket { calls: 0 }),
        );
        let start = Instant::now();
        // Ошибки перемежаются успешными чтениями: серия не длится дольше срока, поток работает.
        for step in 0..400u64 {
            let now = start + Duration::from_millis(step * 100);
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

    fn send_all(socket: &mut MemoryEndpoint, out: Output) {
        for (to, datagram) in out.datagrams {
            socket.send_to(to, &datagram).unwrap();
        }
    }

    /// Сервер в движке и клиенты в сети в памяти. Время и шаги задаёт тест, без задержек.
    struct Rig {
        engine: NetworkEngine,
        handle: NetworkHandle,
        network: MemoryNetwork,
        fingerprint: [u8; 32],
        timeout: Duration,
        now: Instant,
        /// Клиенты: машина состояний и её сокет в сети.
        clients: Vec<(Client, MemoryEndpoint)>,
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
            Self {
                engine,
                handle,
                network,
                fingerprint,
                timeout,
                now,
                clients: Vec::new(),
            }
        }

        /// Добавляет клиента с отпечатком сервера. Адрес клиента — следующий по порядку.
        fn add_client(&mut self) -> usize {
            let index = self.clients.len();
            let client = Client::new(
                ClientConfig {
                    protocol_id: [1; 32],
                    scene_hash: [2; 32],
                    server: addr(7100),
                    fingerprint: Some(self.fingerprint),
                    insecure_no_pin: false,
                    password: None,
                    timeout: self.timeout,
                },
                self.now,
            )
            .unwrap();
            let socket = self.network.endpoint(addr(7101 + index as u16));
            self.clients.push((client, socket));
            index
        }

        /// Один шаг времени: клиенты шлют, движок обрабатывает датаграммы, клиенты принимают ответы.
        fn tick(&mut self) {
            self.now += Duration::from_millis(10);
            for (client, socket) in &mut self.clients {
                let out = client.update(self.now);
                send_all(socket, out);
            }
            for _ in 0..16 {
                assert!(self.engine.step(self.now));
            }
            for (client, socket) in &mut self.clients {
                while let Some((from, bytes)) = socket.recv_from().unwrap() {
                    let out = client.handle_datagram(from, &bytes, self.now);
                    send_all(socket, out);
                }
            }
        }

        fn connect(&mut self) -> bool {
            for _ in 0..500 {
                self.tick();
                if self.clients.iter().all(|(client, _)| client.is_connected()) {
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
        rig.add_client();
        assert!(rig.connect(), "рукопожатие проходит");

        let total = EVENT_QUEUE * 2 + 100;
        for number in 0..total {
            rig.clients[0]
                .0
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
        assert!(rig.clients[0].0.is_connected());
    }

    fn disconnect_reason<'a>(events: impl IntoIterator<Item = &'a NetworkEvent>) -> Option<String> {
        events.into_iter().find_map(|event| match event {
            NetworkEvent::Disconnected { reason, .. } => Some(reason.clone()),
            _ => None,
        })
    }

    /// Регрессия: переполненная очередь не продлевает таймауты бесконечно. Молчащий пир отключается по
    /// обычному таймауту, как только истёк `HOLD_LIMIT` непрерывного переполнения.
    #[test]
    fn timeouts_are_held_only_for_a_bounded_backlog() {
        let mut rig = Rig::new(Duration::from_secs(1));
        rig.add_client();
        assert!(rig.connect());
        for number in 0..EVENT_QUEUE * 3 {
            rig.clients[0]
                .0
                .send(Channel::Reliable, &(number as u32).to_be_bytes())
                .unwrap();
        }
        let mut ticks = 0;
        while rig.engine.backlog_since.is_none() && ticks < 2_000 {
            rig.tick();
            ticks += 1;
        }
        let since = rig.engine.backlog_since.expect("очередь переполнена");
        while rig.now < since + HOLD_LIMIT - Duration::from_millis(100) {
            rig.tick();
        }
        assert_eq!(
            disconnect_reason(rig.engine.pending.iter()),
            None,
            "внутри срока таймаут не идёт"
        );
        while disconnect_reason(rig.engine.pending.iter()).is_none()
            && rig.now < since + HOLD_LIMIT + Duration::from_secs(5)
        {
            rig.tick();
        }
        assert_eq!(
            disconnect_reason(rig.engine.pending.iter()).as_deref(),
            Some("таймаут"),
            "после срока молчащий пир отключается по обычному таймауту"
        );
    }

    /// Регрессия: флудер, который заливает сообщения при переполненной очереди, отключается с причиной, а честный
    /// пир не отключается: его датаграммы обрабатываются и подтверждаются, пока очередь переполнена.
    #[test]
    fn flooder_is_disconnected_and_honest_peer_survives_a_full_queue() {
        const HONEST_BASE: u32 = 1_000_000;
        let mut rig = Rig::new(Duration::from_secs(1));
        let flooder = rig.add_client();
        let honest = rig.add_client();
        assert!(rig.connect());
        for number in 0..EVENT_QUEUE as u32 * 3 {
            rig.clients[flooder]
                .0
                .send(Channel::Reliable, &number.to_be_bytes())
                .unwrap();
        }
        let mut ticks = 0;
        while rig.engine.backlog_since.is_none() && ticks < 2_000 {
            rig.tick();
            ticks += 1;
        }
        assert!(rig.engine.backlog_since.is_some(), "очередь переполнена");
        for number in 0..20u32 {
            rig.clients[honest]
                .0
                .send(Channel::Reliable, &(HONEST_BASE + number).to_be_bytes())
                .unwrap();
        }
        // Очередь остаётся переполненной дольше таймаута, но приложение событий не читает.
        for _ in 0..600 {
            rig.tick();
        }
        let mut received = Vec::new();
        for _ in 0..5_000 {
            rig.tick();
            received.extend(rig.handle.poll_events());
            let honest_done = message_numbers(&received)
                .iter()
                .filter(|number| **number >= HONEST_BASE)
                .count()
                == 20;
            let flooder_out = received.iter().any(|event| {
                matches!(event, NetworkEvent::Disconnected { reason, .. } if reason == OVERLOAD_REASON)
            });
            if honest_done && flooder_out {
                break;
            }
        }
        let honest_peer = received
            .iter()
            .find_map(|event| match event {
                NetworkEvent::Message { peer, bytes, .. }
                    if u32::from_be_bytes(bytes[..4].try_into().unwrap()) >= HONEST_BASE =>
                {
                    Some(*peer)
                }
                _ => None,
            })
            .expect("сообщения честного пира доставлены");
        assert!(
            !received.iter().any(|event| matches!(
                event,
                NetworkEvent::Disconnected { peer, .. } if *peer == honest_peer
            )),
            "честный пир не отключён"
        );
        assert!(
            received.iter().any(|event| matches!(
                event,
                NetworkEvent::Disconnected { peer, reason }
                    if *peer != honest_peer && reason == OVERLOAD_REASON
            )),
            "флудер отключён с причиной"
        );
        let honest_numbers: Vec<u32> = message_numbers(&received)
            .into_iter()
            .filter(|number| *number >= HONEST_BASE)
            .collect();
        assert_eq!(
            honest_numbers,
            (HONEST_BASE..HONEST_BASE + 20).collect::<Vec<_>>()
        );
    }
}
