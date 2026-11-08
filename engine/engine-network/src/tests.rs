//! Интеграционные тесты протокола: машины состояний соединяются через сеть в памяти.
//! Время задаётся явно шагами, задержек и реальных сокетов нет.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_scene::network::PeerId;

use crate::crypto::EphemeralKey;
use crate::handshake::{COOKIE_BODY, Hello, confirm_datagram, hello_datagram};
use crate::packet::{HEADER_LEN, Header, Kind, MAX_DATAGRAM};
use crate::{
    Channel, Client, ClientConfig, DatagramSocket, Faults, MemoryEndpoint, MemoryNetwork, NetError,
    NetworkEvent, Output, Password, Server, ServerConfig, ServerIdentity,
};

const PROTOCOL: [u8; 32] = [5; 32];
const SCENE: [u8; 32] = [6; 32];
const TIMEOUT: Duration = Duration::from_secs(10);
const STEP: Duration = Duration::from_millis(10);

fn server_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7000))
}

fn client_addr(index: usize) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 10_000 + index as u16))
}

fn messages(events: &[NetworkEvent]) -> Vec<(Channel, Vec<u8>)> {
    events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Message { channel, bytes, .. } => Some((*channel, bytes.clone())),
            _ => None,
        })
        .collect()
}

fn disconnect_reasons(events: &[NetworkEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Disconnected { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

fn connected_peers(events: &[NetworkEvent]) -> Vec<PeerId> {
    events
        .iter()
        .filter_map(|event| match event {
            NetworkEvent::Connected { peer } => Some(*peer),
            _ => None,
        })
        .collect()
}

fn kind_of(datagram: &[u8]) -> Option<Kind> {
    Header::decode(datagram).map(|header| header.kind)
}

struct Slot {
    client: Client,
    endpoint: MemoryEndpoint,
    addr: SocketAddr,
    events: Vec<NetworkEvent>,
    sent: Vec<Vec<u8>>,
}

/// Сервер и клиенты в одной сети в памяти с общими часами.
struct World {
    now: Instant,
    fingerprint: [u8; 32],
    server: Server,
    server_ep: MemoryEndpoint,
    server_events: Vec<NetworkEvent>,
    server_sent: Vec<Vec<u8>>,
    network: MemoryNetwork,
    clients: Vec<Slot>,
}

impl World {
    fn new(max_clients: usize, password: Option<&str>, faults: Faults, seed: u64) -> Self {
        let now = Instant::now();
        let network = MemoryNetwork::new(seed, faults);
        let identity = ServerIdentity::ephemeral().unwrap();
        let fingerprint = identity.fingerprint();
        let server = Server::new(
            ServerConfig {
                protocol_id: PROTOCOL,
                scene_hash: SCENE,
                max_clients,
                password: password.map(Password::new),
                timeout: TIMEOUT,
            },
            identity,
            now,
        )
        .unwrap();
        let server_ep = network.endpoint(server_addr());
        Self {
            now,
            fingerprint,
            server,
            server_ep,
            server_events: Vec::new(),
            server_sent: Vec::new(),
            network,
            clients: Vec::new(),
        }
    }

    /// Добавляет клиента. Без отпечатка клиент подключается с явным разрешением `insecure`.
    fn add_client(
        &mut self,
        password: Option<&str>,
        fingerprint: Option<[u8; 32]>,
        protocol: [u8; 32],
    ) -> usize {
        let index = self.clients.len();
        let addr = client_addr(index);
        let client = Client::new(
            ClientConfig {
                protocol_id: protocol,
                scene_hash: SCENE,
                server: server_addr(),
                insecure_no_pin: fingerprint.is_none(),
                fingerprint,
                password: password.map(Password::new),
                timeout: TIMEOUT,
            },
            self.now,
        )
        .unwrap();
        let endpoint = self.network.endpoint(addr);
        self.clients.push(Slot {
            client,
            endpoint,
            addr,
            events: Vec::new(),
            sent: Vec::new(),
        });
        index
    }

    fn route_server(&mut self, out: Output) {
        for (to, datagram) in out.datagrams {
            self.server_sent.push(datagram.clone());
            let _ = self.server_ep.send_to(to, &datagram);
        }
        self.server_events.extend(out.events);
    }

    fn route_client(&mut self, index: usize, out: Output) {
        let slot = &mut self.clients[index];
        for (to, datagram) in out.datagrams {
            slot.sent.push(datagram.clone());
            let _ = slot.endpoint.send_to(to, &datagram);
        }
        slot.events.extend(out.events);
    }

    /// Доставляет все датаграммы, которые уже лежат во входящих очередях.
    fn pump(&mut self) {
        while let Some((from, bytes)) = self.server_ep.recv_from().unwrap() {
            let out = self.server.handle_datagram(from, &bytes, self.now);
            self.route_server(out);
        }
        for index in 0..self.clients.len() {
            while let Some((from, bytes)) = self.clients[index].endpoint.recv_from().unwrap() {
                let out = self.clients[index]
                    .client
                    .handle_datagram(from, &bytes, self.now);
                self.route_client(index, out);
            }
        }
    }

    fn step(&mut self) {
        self.now += STEP;
        self.pump();
        let out = self.server.update(self.now);
        self.route_server(out);
        for index in 0..self.clients.len() {
            let out = self.clients[index].client.update(self.now);
            self.route_client(index, out);
        }
    }

    fn run_until(&mut self, limit: usize, done: impl Fn(&World) -> bool) -> bool {
        for _ in 0..limit {
            if done(self) {
                return true;
            }
            self.step();
        }
        done(self)
    }

    fn connect(&mut self, index: usize) -> bool {
        self.run_until(2_000, |world| world.clients[index].client.is_connected())
    }

    fn sent_datagrams(&self) -> impl Iterator<Item = &Vec<u8>> {
        self.server_sent
            .iter()
            .chain(self.clients.iter().flat_map(|slot| slot.sent.iter()))
    }
}

/// Мир с одним подключённым клиентом без пароля.
fn connected_world(seed: u64) -> World {
    let mut world = World::new(4, None, Faults::default(), seed);
    let fingerprint = world.fingerprint;
    let index = world.add_client(None, Some(fingerprint), PROTOCOL);
    assert!(world.connect(index), "рукопожатие должно завершиться");
    world
}

fn server_only(max_clients: usize) -> Server {
    Server::new(
        ServerConfig {
            protocol_id: PROTOCOL,
            scene_hash: SCENE,
            max_clients,
            password: None,
            timeout: TIMEOUT,
        },
        ServerIdentity::ephemeral().unwrap(),
        Instant::now(),
    )
    .unwrap()
}

fn fresh_hello() -> Hello {
    Hello {
        client_public: EphemeralKey::random().unwrap().public(),
        nonce: [7; 16],
        protocol_id: PROTOCOL,
        scene_hash: SCENE,
    }
}

/// Берёт cookie из ответа сервера на Hello.
fn cookie_from(out: &Output) -> [u8; COOKIE_BODY] {
    let (_, datagram) = out
        .datagrams
        .first()
        .expect("сервер должен ответить cookie");
    assert_eq!(kind_of(datagram), Some(Kind::Cookie));
    datagram[HEADER_LEN..HEADER_LEN + COOKIE_BODY]
        .try_into()
        .unwrap()
}

#[test]
fn handshake_connects_and_messages_flow_both_ways() {
    let mut world = World::new(4, None, Faults::default(), 1);
    let fingerprint = world.fingerprint;
    let index = world.add_client(None, Some(fingerprint), PROTOCOL);
    assert!(world.connect(index));

    let peer = world.clients[index].client.local_peer().unwrap();
    assert_eq!(connected_peers(&world.server_events), vec![peer]);
    assert_eq!(connected_peers(&world.clients[index].events), vec![0]);

    world.clients[index]
        .client
        .send(Channel::Reliable, b"hello")
        .unwrap();
    assert!(world.run_until(500, |w| messages(&w.server_events).len() == 1));
    assert_eq!(
        messages(&world.server_events),
        vec![(Channel::Reliable, b"hello".to_vec())]
    );

    world.server.send(peer, Channel::Rpc, b"world").unwrap();
    assert!(world.run_until(500, |w| !messages(&w.clients[index].events).is_empty()));
    assert_eq!(
        messages(&world.clients[index].events),
        vec![(Channel::Rpc, b"world".to_vec())]
    );
}

#[test]
fn correct_password_connects_and_never_appears_in_clear_text() {
    let secret = "тайный-пароль-42";
    let mut world = World::new(2, Some(secret), Faults::default(), 2);
    let fingerprint = world.fingerprint;
    let index = world.add_client(Some(secret), Some(fingerprint), PROTOCOL);
    assert!(world.connect(index));
    let needle = secret.as_bytes();
    assert!(world.sent_datagrams().all(|datagram| {
        !datagram
            .windows(needle.len())
            .any(|window| window == needle)
    }));
}

#[test]
fn wrong_or_missing_password_is_refused() {
    for client_password in [Some("неверный"), None] {
        let mut world = World::new(2, Some("верный"), Faults::default(), 3);
        let fingerprint = world.fingerprint;
        let index = world.add_client(client_password, Some(fingerprint), PROTOCOL);
        assert!(!world.connect(index));
        assert_eq!(
            disconnect_reasons(&world.clients[index].events),
            vec!["не удалось проверить пароль"]
        );
        assert!(connected_peers(&world.server_events).is_empty());
    }
}

#[test]
fn wrong_fingerprint_stops_before_the_password_is_sent() {
    let mut world = World::new(2, Some("пароль"), Faults::default(), 4);
    let index = world.add_client(Some("пароль"), Some([0xEE; 32]), PROTOCOL);
    assert!(!world.connect(index));
    assert_eq!(
        disconnect_reasons(&world.clients[index].events),
        vec!["отпечаток ключа сервера не совпадает"]
    );
    assert!(
        world.clients[index]
            .sent
            .iter()
            .all(|datagram| kind_of(datagram) != Some(Kind::Data)),
        "Auth не отправлен неподтверждённому серверу"
    );
}

#[test]
fn protocol_mismatch_is_refused_with_reason() {
    let mut world = World::new(2, None, Faults::default(), 5);
    let fingerprint = world.fingerprint;
    let index = world.add_client(None, Some(fingerprint), [9; 32]);
    assert!(!world.connect(index));
    assert_eq!(
        disconnect_reasons(&world.clients[index].events),
        vec!["протокол не совпадает"]
    );
}

#[test]
fn full_server_refuses_a_second_client_and_keeps_the_first() {
    let mut world = World::new(1, None, Faults::default(), 6);
    let fingerprint = world.fingerprint;
    let first = world.add_client(None, Some(fingerprint), PROTOCOL);
    assert!(world.connect(first));
    let second = world.add_client(None, Some(fingerprint), PROTOCOL);
    assert!(!world.connect(second));
    assert_eq!(
        disconnect_reasons(&world.clients[second].events),
        vec!["сервер заполнен"]
    );
    assert_eq!(connected_peers(&world.server_events).len(), 1);
    assert!(world.clients[first].client.is_connected());
}

#[test]
fn replayed_confirm_from_a_connected_client_takes_no_slot() {
    let mut world = connected_world(7);
    let index = 0;
    let addr = world.clients[index].addr;
    let confirm = world.clients[index]
        .sent
        .iter()
        .find(|datagram| kind_of(datagram) == Some(Kind::Confirm))
        .expect("клиент отправлял Confirm")
        .clone();
    let out = world.server.handle_datagram(addr, &confirm, world.now);
    assert!(
        out.datagrams.is_empty() && out.events.is_empty(),
        "повтор Confirm игнорируется"
    );
}

#[test]
fn half_open_sessions_are_limited_per_address() {
    // Лимит полуоткрытых сессий на один адрес (см. `server::MAX_HALF_OPEN_PER_ADDR`).
    let mut server = server_only(8);
    let now = Instant::now();
    let addr = client_addr(0);
    let mut accepted = 0;
    for _ in 0..6 {
        let hello = fresh_hello();
        let cookie = cookie_from(&server.handle_datagram(addr, &hello_datagram(&hello), now));
        let out = server.handle_datagram(addr, &confirm_datagram(&cookie, &hello), now);
        for (_, datagram) in &out.datagrams {
            assert!(datagram.len() <= MAX_DATAGRAM, "ответ не длиннее запроса");
            assert_eq!(kind_of(datagram), Some(Kind::Accept));
        }
        accepted += out.datagrams.len();
    }
    assert_eq!(accepted, 4);
}

#[test]
fn cookie_responses_are_not_longer_than_the_request_and_expire() {
    let mut server = server_only(2);
    let now = Instant::now();
    let hello = fresh_hello();
    let out = server.handle_datagram(client_addr(0), &hello_datagram(&hello), now);
    assert!(
        out.datagrams
            .iter()
            .all(|(_, datagram)| datagram.len() <= MAX_DATAGRAM)
    );
    let cookie = cookie_from(&out);

    let late = now + Duration::from_millis(5_001);
    let out = server.handle_datagram(client_addr(0), &confirm_datagram(&cookie, &hello), late);
    assert!(out.datagrams.is_empty(), "просроченный cookie отвергнут");
}

#[test]
fn cookie_is_bound_to_the_address_that_received_it() {
    let mut server = server_only(2);
    let now = Instant::now();
    let hello = fresh_hello();
    let out = server.handle_datagram(client_addr(0), &hello_datagram(&hello), now);
    let cookie = cookie_from(&out);
    let out = server.handle_datagram(client_addr(1), &confirm_datagram(&cookie, &hello), now);
    assert!(out.datagrams.is_empty(), "чужой cookie отвергнут");
}

#[test]
fn tampered_or_replayed_data_is_dropped() {
    let mut world = connected_world(8);
    let addr = world.clients[0].addr;
    world.clients[0]
        .client
        .send(Channel::Reliable, b"once")
        .unwrap();
    let out = world.clients[0].client.update(world.now);
    let datagrams: Vec<Vec<u8>> = out.datagrams.into_iter().map(|(_, bytes)| bytes).collect();
    let first = datagrams.first().expect("кадры отправлены").clone();

    let mut tampered = first.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let out = world.server.handle_datagram(addr, &tampered, world.now);
    assert!(out.events.is_empty(), "изменённая датаграмма отброшена");

    let mut header = first.clone();
    header[HEADER_LEN - 1] ^= 0x01;
    let out = world.server.handle_datagram(addr, &header, world.now);
    assert!(out.events.is_empty(), "изменённый заголовок отброшен");

    let out = world.server.handle_datagram(addr, &first, world.now);
    assert_eq!(
        messages(&out.events),
        vec![(Channel::Reliable, b"once".to_vec())]
    );

    let out = world.server.handle_datagram(addr, &first, world.now);
    assert!(out.events.is_empty(), "повтор отброшен");
}

#[test]
fn reliable_messages_arrive_in_order_despite_loss_reordering_and_duplicates() {
    let faults = Faults {
        loss_permille: 200,
        duplicate_permille: 100,
        reorder: true,
    };
    let mut world = World::new(2, None, faults, 9);
    let fingerprint = world.fingerprint;
    let index = world.add_client(None, Some(fingerprint), PROTOCOL);
    assert!(world.connect(index), "рукопожатие проходит при потерях");

    let total: u32 = 200;
    for number in 0..total {
        world.clients[index]
            .client
            .send(Channel::Reliable, &number.to_be_bytes())
            .unwrap();
    }
    let done = world.run_until(20_000, |w| {
        messages(&w.server_events).len() == total as usize
    });
    assert!(done, "все сообщения доставлены");
    let received: Vec<u32> = messages(&world.server_events)
        .into_iter()
        .map(|(_, bytes)| u32::from_be_bytes(bytes.try_into().unwrap()))
        .collect();
    assert_eq!(received, (0..total).collect::<Vec<_>>());
}

#[test]
fn large_messages_are_fragmented_and_limits_are_enforced() {
    let mut world = connected_world(10);
    let big: Vec<u8> = (0..60_000u32).map(|value| (value % 251) as u8).collect();
    world.clients[0].client.send(Channel::Rpc, &big).unwrap();
    assert!(world.run_until(2_000, |w| !messages(&w.server_events).is_empty()));
    assert_eq!(messages(&world.server_events), vec![(Channel::Rpc, big)]);

    let too_big = vec![0u8; 64 * 1024 + 1];
    assert!(matches!(
        world.clients[0].client.send(Channel::Reliable, &too_big),
        Err(NetError::MessageTooLarge { .. })
    ));
    let unreliable = vec![0u8; 1001];
    assert!(matches!(
        world.clients[0]
            .client
            .send(Channel::Unreliable, &unreliable),
        Err(NetError::MessageTooLarge { .. })
    ));
}

#[test]
fn send_queue_reports_queue_full_instead_of_growing() {
    let mut world = connected_world(11);
    let chunk = vec![0u8; 1000];
    let mut accepted = 0;
    let error = loop {
        match world.clients[0].client.send(Channel::Reliable, &chunk) {
            Ok(()) => accepted += 1,
            Err(err) => break err,
        }
    };
    assert_eq!(error, NetError::QueueFull);
    assert!(accepted > 0);
}

#[test]
fn silent_peers_time_out_on_both_sides() {
    let mut world = connected_world(12);
    let later = world.now + TIMEOUT + STEP;
    let out = world.server.update(later);
    assert_eq!(disconnect_reasons(&out.events), vec!["таймаут"]);
    let out = world.clients[0].client.update(later);
    assert_eq!(disconnect_reasons(&out.events), vec!["таймаут соединения"]);
}

#[test]
fn handshake_without_an_answer_times_out() {
    let mut world = World::new(2, None, Faults::default(), 13);
    let fingerprint = world.fingerprint;
    let index = world.add_client(None, Some(fingerprint), PROTOCOL);
    let mut now = world.now;
    let mut reasons = Vec::new();
    while reasons.is_empty() {
        now += STEP;
        let out = world.clients[index].client.update(now);
        reasons = disconnect_reasons(&out.events);
    }
    assert_eq!(reasons, vec!["таймаут подключения"]);
}

#[test]
fn disconnect_is_reported_to_the_other_side() {
    let mut world = connected_world(14);
    let out = world.clients[0].client.disconnect("выход");
    world.route_client(0, out);
    assert!(world.run_until(200, |w| !disconnect_reasons(&w.server_events).is_empty()));
    assert_eq!(disconnect_reasons(&world.server_events), vec!["выход"]);
}

#[test]
fn server_targets_unknown_peers_with_an_error() {
    let mut world = connected_world(15);
    assert_eq!(
        world.server.send(999, Channel::Reliable, b"x"),
        Err(NetError::UnknownPeer)
    );
}

#[test]
fn garbage_truncated_and_bit_flipped_packets_never_panic() {
    let mut world = connected_world(16);
    let addr = world.clients[0].addr;
    let mut samples: Vec<Vec<u8>> = world.sent_datagrams().take(12).cloned().collect();
    samples.push(hello_datagram(&fresh_hello()));

    for sample in &samples {
        for len in 0..=sample.len() {
            feed(&mut world, addr, &sample[..len]);
        }
        for index in 0..sample.len() {
            let mut flipped = sample.clone();
            flipped[index] ^= 0x41;
            feed(&mut world, addr, &flipped);
        }
    }

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..3_000 {
        let mut random = Vec::new();
        for _ in 0..(state % 1300) as usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            random.push(state as u8);
        }
        if random.len() >= HEADER_LEN && state.is_multiple_of(2) {
            random[..4].copy_from_slice(b"RVEN");
            random[4..6].copy_from_slice(&1u16.to_be_bytes());
            random[6] = (state % 6) as u8 + 1;
        }
        feed(&mut world, addr, &random);
    }

    assert!(
        world.clients[0].client.is_connected(),
        "мусор не разрывает соединение"
    );
    world.clients[0]
        .client
        .send(Channel::Reliable, b"alive")
        .unwrap();
    assert!(world.run_until(500, |w| {
        messages(&w.server_events).contains(&(Channel::Reliable, b"alive".to_vec()))
    }));
}

/// Подаёт датаграмму серверу и клиенту 0 и отбрасывает ответы.
fn feed(world: &mut World, client_side: SocketAddr, datagram: &[u8]) {
    let _ = world
        .server
        .handle_datagram(client_side, datagram, world.now);
    let _ = world.clients[0]
        .client
        .handle_datagram(server_addr(), datagram, world.now);
}
