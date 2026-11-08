//! Интеграционные тесты протокола: машины состояний соединяются через сеть в памяти.
//! Время задаётся явно шагами, задержек и реальных сокетов нет.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use engine_scene::network::PeerId;

use crate::crypto::{
    EphemeralKey, Session, Transcript, derive_keys, password_hash, password_proof, reject_session,
    transcript_hash,
};
use crate::handshake::{
    COOKIE_BODY, HANDSHAKE_TIMEOUT, Hello, confirm_datagram, hello_datagram, parse_accept,
    parse_reject,
};
use crate::link::{AUTH_BODY, OP_AUTH, OP_DISCONNECT, parse_disconnect};
use crate::packet::{HEADER_LEN, Header, Kind, MAX_DATAGRAM, SEQ_LEN, TAG_LEN};
use crate::server::{AUTH_FAILURE_LIMIT, MAX_HALF_OPEN_PER_IP};

/// Размер датаграммы с попыткой входа на сервере.
const AUTH_DATAGRAM: usize = HEADER_LEN + SEQ_LEN + AUTH_BODY + TAG_LEN;
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
    // Лимит полуоткрытых сессий на один IP-адрес (см. `server::MAX_HALF_OPEN_PER_IP`).
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

/// Датаграмма `Data` нужной длины: заголовок и мусор вместо шифротекста.
fn data_datagram(conn_id: u32, len: usize) -> Vec<u8> {
    let mut datagram = Vec::new();
    Header {
        kind: Kind::Data,
        conn_id,
    }
    .encode(&mut datagram);
    datagram.resize(len, 0x5a);
    datagram
}

/// Проводит рукопожатие клиента с сервером без продвижения времени. Возвращает причины отключения клиента.
fn attempt(
    server: &mut Server,
    client: &mut Client,
    from: SocketAddr,
    now: Instant,
) -> Vec<String> {
    let mut to_server: Vec<Vec<u8>> = client
        .update(now)
        .datagrams
        .into_iter()
        .map(|(_, datagram)| datagram)
        .collect();
    let mut reasons = Vec::new();
    for _ in 0..16 {
        if to_server.is_empty() {
            break;
        }
        let mut to_client = Vec::new();
        for datagram in to_server.drain(..) {
            let out = server.handle_datagram(from, &datagram, now);
            to_client.extend(out.datagrams.into_iter().map(|(_, bytes)| bytes));
        }
        for datagram in to_client {
            let out = client.handle_datagram(server_addr(), &datagram, now);
            reasons.extend(disconnect_reasons(&out.events));
            to_server.extend(out.datagrams.into_iter().map(|(_, bytes)| bytes));
        }
    }
    reasons
}

/// Регрессия: полуоткрытые сессии считаются по IP-адресу, а не по порту.
#[test]
fn half_open_sessions_are_limited_per_ip_address_not_per_port() {
    let mut server = server_only(8);
    let now = Instant::now();
    let mut accepted = 0;
    for index in 0..6 {
        let addr = client_addr(index);
        let hello = fresh_hello();
        let cookie = cookie_from(&server.handle_datagram(addr, &hello_datagram(&hello), now));
        let out = server.handle_datagram(addr, &confirm_datagram(&cookie, &hello), now);
        accepted += out.datagrams.len();
    }
    assert_eq!(accepted, MAX_HALF_OPEN_PER_IP);
}

/// Посторонний пакет полуоткрытой сессии получает отказ только если не короче его и только один раз.
#[test]
fn forged_packets_get_at_most_one_reject_and_only_when_not_shorter() {
    let mut server = server_only(4);
    let now = Instant::now();
    let addr = client_addr(0);
    let open = |server: &mut Server| -> u32 {
        let hello = fresh_hello();
        let cookie = cookie_from(&server.handle_datagram(addr, &hello_datagram(&hello), now));
        let out = server.handle_datagram(addr, &confirm_datagram(&cookie, &hello), now);
        let (_, accept) = out.datagrams.first().expect("Accept");
        Header::decode(accept).unwrap().conn_id
    };

    // Короткий мусор без ответа, но он потребляет полуоткрытую сессию: повтор на ней ответа не даёт.
    let conn_id = open(&mut server);
    let short = data_datagram(conn_id, 40);
    assert!(
        server
            .handle_datagram(addr, &short, now)
            .datagrams
            .is_empty(),
        "короткий запрос без ответа"
    );
    assert!(
        server
            .handle_datagram(addr, &data_datagram(conn_id, AUTH_DATAGRAM), now)
            .datagrams
            .is_empty(),
        "потреблённая сессия не отвечает"
    );

    let conn_id = open(&mut server);
    let long = data_datagram(conn_id, AUTH_DATAGRAM);
    let reply = server.handle_datagram(addr, &long, now).datagrams;
    assert_eq!(reply.len(), 1, "отказ на длинный запрос");
    assert!(reply[0].1.len() <= long.len(), "ответ не длиннее запроса");
    assert!(
        server
            .handle_datagram(addr, &long, now)
            .datagrams
            .is_empty(),
        "второго ответа нет"
    );
}

/// Серия неудачных входов с одного адреса приводит к временному отказу; после срока вход снова возможен.
#[test]
fn repeated_wrong_passwords_from_one_address_are_refused_for_a_while() {
    let identity = ServerIdentity::ephemeral().unwrap();
    let fingerprint = identity.fingerprint();
    let now = Instant::now();
    let mut server = Server::new(
        ServerConfig {
            protocol_id: PROTOCOL,
            scene_hash: SCENE,
            max_clients: 2,
            password: Some(Password::new("верный")),
            timeout: TIMEOUT,
        },
        identity,
        now,
    )
    .unwrap();
    let from = client_addr(0);
    let mut try_wrong = |at: Instant| {
        let mut client = Client::new(
            ClientConfig {
                protocol_id: PROTOCOL,
                scene_hash: SCENE,
                server: server_addr(),
                fingerprint: Some(fingerprint),
                insecure_no_pin: false,
                password: Some(Password::new("неверный")),
                timeout: TIMEOUT,
            },
            at,
        )
        .unwrap();
        let mut reasons = attempt(&mut server, &mut client, from, at);
        // Обычный отказ на этапе рукопожатия не обрывает соединение сразу: клиент показывает его по таймауту.
        reasons.extend(disconnect_reasons(
            &client.update(at + HANDSHAKE_TIMEOUT).events,
        ));
        reasons
    };
    for _ in 0..AUTH_FAILURE_LIMIT {
        assert_eq!(try_wrong(now), vec!["не удалось проверить пароль"]);
    }
    assert_eq!(
        try_wrong(now),
        vec!["слишком много попыток, повторите позже"],
        "после серии неудач адрес временно отклонён явным отказом"
    );
    assert_eq!(
        try_wrong(now + Duration::from_secs(2)),
        vec!["не удалось проверить пароль"],
        "срок отказа истёк"
    );
}

/// Полуоткрытая сессия, открытая клиентом с известным ключом: по ней подделывается Auth с догадкой о пароле.
struct HalfSession {
    ephemeral: EphemeralKey,
    server_public: [u8; 32],
    transcript: [u8; 32],
    conn_id: u32,
}

impl HalfSession {
    fn open(server: &mut Server, from: SocketAddr, now: Instant) -> Self {
        let ephemeral = EphemeralKey::random().unwrap();
        let hello = Hello {
            client_public: ephemeral.public(),
            nonce: [7; 16],
            protocol_id: PROTOCOL,
            scene_hash: SCENE,
        };
        let cookie = cookie_from(&server.handle_datagram(from, &hello_datagram(&hello), now));
        let out = server.handle_datagram(from, &confirm_datagram(&cookie, &hello), now);
        let (_, accept) = out.datagrams.first().expect("Accept");
        let conn_id = Header::decode(accept).unwrap().conn_id;
        let (server_public, identity_public, _) = parse_accept(&accept[HEADER_LEN..]).unwrap();
        let transcript = transcript_hash(&Transcript {
            protocol_id: &PROTOCOL,
            scene_hash: &SCENE,
            client_public: &hello.client_public,
            nonce: &hello.nonce,
            server_public: &server_public,
            identity_public: &identity_public,
            conn_id,
        });
        Self {
            ephemeral,
            server_public,
            transcript,
            conn_id,
        }
    }

    /// Auth нужной длины, запечатанный ключом догадки. Длина `AUTH_BODY` — честный формат.
    fn guess(&self, guess: Option<&Password>, body_len: usize) -> Vec<u8> {
        let shared = self.ephemeral.agree(&self.server_public).unwrap();
        let keys = derive_keys(&shared, &self.transcript, &password_hash(guess));
        let mut session = Session::new(&keys.client_to_server, &keys.server_to_client);
        let mut body = vec![OP_AUTH];
        body.extend_from_slice(&password_proof(guess, &self.transcript));
        body.resize(body_len, 0);
        session.seal(self.conn_id, &body).unwrap().1
    }

    /// Ответ сервера на датаграмму, расшифрованный ключом отказа. `None` — ответа нет.
    fn answer(
        &self,
        server: &mut Server,
        from: SocketAddr,
        datagram: &[u8],
        now: Instant,
    ) -> Option<String> {
        let out = server.handle_datagram(from, datagram, now);
        assert!(
            out.datagrams.len() <= 1,
            "не больше одного ответа на запрос"
        );
        let (_, reply) = out.datagrams.first()?;
        let shared = self.ephemeral.agree(&self.server_public).unwrap();
        let (_, body) = reject_session(&shared, &self.transcript).open(reply)?;
        assert_eq!(body.first(), Some(&OP_DISCONNECT));
        Some(parse_disconnect(&body[1..]))
    }
}

fn password_server(password: &str) -> Server {
    Server::new(
        ServerConfig {
            protocol_id: PROTOCOL,
            scene_hash: SCENE,
            max_clients: 2,
            password: Some(Password::new(password)),
            timeout: TIMEOUT,
        },
        ServerIdentity::ephemeral().unwrap(),
        Instant::now(),
    )
    .unwrap()
}

/// Регрессия: короткий Auth с верным ключом и Auth неверной длины обходили счётчик неудач и давали
/// отличимый ответ. Теперь любая догадка учитывается, а ответ зависит только от того, прошла ли она
/// (короткий запрос не получает отказа), но не от верности ключа.
#[test]
fn guessed_passwords_get_one_answer_whatever_their_length_and_always_count() {
    const SHORT: usize = 33;
    const LONG: usize = AUTH_BODY + 1;
    let mut server = password_server("верный");
    let now = Instant::now();
    let from = client_addr(0);
    let wrong = Password::new("неверный");
    let right = Password::new("верный");
    let cases = [
        (&wrong, SHORT),
        (&right, SHORT),
        (&wrong, LONG),
        (&right, LONG),
        (&wrong, SHORT),
    ];
    let mut answers = Vec::new();
    for (guess, body_len) in cases {
        let half = HalfSession::open(&mut server, from, now);
        let datagram = half.guess(Some(guess), body_len);
        answers.push(half.answer(&mut server, from, &datagram, now));
    }
    assert_eq!(
        answers,
        vec![
            None,
            None,
            Some("не удалось проверить пароль".to_owned()),
            Some("не удалось проверить пароль".to_owned()),
            None,
        ],
        "верная и неверная догадка одной длины получают одинаковый ответ"
    );

    let hello = fresh_hello();
    let cookie = cookie_from(&server.handle_datagram(from, &hello_datagram(&hello), now));
    let out = server.handle_datagram(from, &confirm_datagram(&cookie, &hello), now);
    let (_, reject) = out.datagrams.first().expect("явный отказ, а не молчание");
    assert_eq!(kind_of(reject), Some(Kind::Reject));
    assert_eq!(
        parse_reject(&reject[HEADER_LEN..]),
        "слишком много попыток, повторите позже"
    );
}

/// Счётчик неудач сбрасывается после десяти минут без неудач: редкие ошибки не приводят к блокировке.
#[test]
fn failure_counter_decays_after_ten_minutes_without_failures() {
    let mut server = password_server("верный");
    let now = Instant::now();
    let from = client_addr(0);
    let wrong = Password::new("неверный");
    for _ in 0..AUTH_FAILURE_LIMIT - 1 {
        let half = HalfSession::open(&mut server, from, now);
        let datagram = half.guess(Some(&wrong), AUTH_BODY + 1);
        assert_eq!(
            half.answer(&mut server, from, &datagram, now),
            Some("не удалось проверить пароль".to_owned())
        );
    }
    let later = now + Duration::from_secs(600);
    for _ in 0..AUTH_FAILURE_LIMIT - 1 {
        let half = HalfSession::open(&mut server, from, later);
        let datagram = half.guess(Some(&wrong), AUTH_BODY + 1);
        assert_eq!(
            half.answer(&mut server, from, &datagram, later),
            Some("не удалось проверить пароль".to_owned()),
            "после паузы счётчик начинается заново"
        );
    }
}

/// Переполнение надёжной очереди отключает медленного пира с причиной, а не теряет сообщения молча.
#[test]
fn reliable_queue_overflow_disconnects_the_slow_peer_with_a_reason() {
    let mut world = connected_world(17);
    let peer = world.clients[0].client.local_peer().unwrap();
    let chunk = vec![0u8; 1000];
    let mut error = None;
    while error.is_none() {
        error = world.server.send(peer, Channel::Reliable, &chunk).err();
    }
    assert_eq!(error, Some(NetError::QueueFull));
    let out = world.server.update(world.now);
    assert_eq!(
        disconnect_reasons(&out.events),
        vec!["очередь отправки переполнена"]
    );
    world.route_server(out);
    assert!(world.run_until(200, |w| {
        !disconnect_reasons(&w.clients[0].events).is_empty()
    }));
    assert_eq!(
        disconnect_reasons(&world.clients[0].events),
        vec!["очередь отправки переполнена"]
    );
}

/// Поток состояний заполняет только свою очередь: надёжные сообщения при этом проходят.
#[test]
fn state_flood_does_not_block_reliable_messages() {
    let mut world = connected_world(18);
    let peer = world.clients[0].client.local_peer().unwrap();
    let chunk = vec![7u8; 1000];
    for _ in 0..2_000 {
        world
            .server
            .send(peer, Channel::State, &chunk)
            .expect("состояния не отказывают");
    }
    world
        .server
        .send(peer, Channel::Reliable, b"control")
        .unwrap();
    assert!(world.run_until(3_000, |w| {
        messages(&w.clients[0].events).contains(&(Channel::Reliable, b"control".to_vec()))
    }));
}

/// Широковещание: медленный пир отключается, остальные получают сообщение.
#[test]
fn broadcast_disconnects_a_slow_peer_and_still_reaches_the_others() {
    let mut world = World::new(4, None, Faults::default(), 19);
    let fingerprint = world.fingerprint;
    let slow = world.add_client(None, Some(fingerprint), PROTOCOL);
    let fast = world.add_client(None, Some(fingerprint), PROTOCOL);
    assert!(world.connect(slow));
    assert!(world.connect(fast));
    let slow_peer = world.clients[slow].client.local_peer().unwrap();
    let chunk = vec![0u8; 1000];
    while world
        .server
        .send(slow_peer, Channel::Reliable, &chunk)
        .is_ok()
    {}
    world.server.broadcast(Channel::Reliable, b"all").unwrap();
    let out = world.server.update(world.now);
    world.route_server(out);
    assert!(world.run_until(500, |w| {
        messages(&w.clients[fast].events).contains(&(Channel::Reliable, b"all".to_vec()))
    }));
    assert!(world.run_until(500, |w| {
        !disconnect_reasons(&w.clients[slow].events).is_empty()
    }));
}
