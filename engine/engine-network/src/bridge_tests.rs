//! Тесты моста репликации. Сервер и клиенты — отдельные миры, связанные общей очередью
//! сообщений без сокетов. Время продвигается явно, без ожиданий.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use engine_files::VirtualPath;
use engine_scene::network::{NetEvent, NetId, NetMode, NetRequest, NetTarget, PeerId};
use engine_scene::{
    LocalTransform, ObjectHandle, ParamValue, PrefabTemplate, SceneDocument, SpawnOptions, World,
};
use glam::Vec3;

use crate::bridge::{BridgeConfig, NetLink, NetworkBridge, SpawnedRoot, WorldSink};
use crate::replication::{self, MASK_POSITION, Message, child_net_id};
use crate::{Channel, NetError, NetworkEvent};

const TICK_HZ: u32 = 30;
/// Столько ошибок подряд отключают участника в пределах окна.
const FAULT_BURST: usize = 32;
const STEP: Duration = Duration::from_millis(10);
const CRATE: &str = "prefabs/crate.rveprefab";
const OWNED: &str = "prefabs/owned.rveprefab";
const HEAVY: &str = "prefabs/heavy.rveprefab";

const SCENE: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
    [[objects]]\nid = \"box\"\n[objects.network_identity]\n[objects.network_transform]\n\
    [[objects]]\nid = \"wall\"\n";
const CRATE_TEXT: &str = "[[objects]]\nid = \"body\"\n[objects.network_identity]\n\
    [objects.network_transform]\n\
    [[objects]]\nid = \"lid\"\nparent = \"body\"\n[objects.network_identity]\n\
    [objects.network_transform]\n";
const OWNED_TEXT: &str = "[[objects]]\nid = \"body\"\n[objects.network_identity]\n\
    authority = \"owner\"\n[objects.network_transform]\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    Server,
    Client(PeerId),
}

/// Общая очередь: сообщения переходят от одного участника к другому при `pump`.
#[derive(Default)]
struct Hub {
    inbox: HashMap<Side, VecDeque<NetworkEvent>>,
    outbox: Vec<(Side, NetTarget, Channel, Vec<u8>)>,
    clients: Vec<PeerId>,
    disconnects: Vec<(Side, PeerId, String)>,
    /// Пока `true`, состояния (канал `State`) не доходят до получателей.
    drop_state: bool,
    snapshot_messages: usize,
}

impl Hub {
    fn deliver(&mut self, to: Side, event: NetworkEvent) {
        self.inbox.entry(to).or_default().push_back(event);
    }

    fn pump(&mut self) {
        for (from, target, channel, bytes) in std::mem::take(&mut self.outbox) {
            if channel == Channel::State && self.drop_state {
                continue;
            }
            if matches!(replication::decode(&bytes), Ok(Message::Snapshot(_))) {
                self.snapshot_messages += 1;
            }
            match from {
                Side::Server => {
                    let recipients: Vec<PeerId> = match target {
                        NetTarget::All => self.clients.clone(),
                        NetTarget::Peer(peer) => vec![peer],
                        NetTarget::Server => Vec::new(),
                    };
                    for peer in recipients {
                        self.deliver(
                            Side::Client(peer),
                            NetworkEvent::Message {
                                peer: 0,
                                channel,
                                bytes: bytes.clone(),
                            },
                        );
                    }
                }
                Side::Client(peer) => self.deliver(
                    Side::Server,
                    NetworkEvent::Message {
                        peer,
                        channel,
                        bytes,
                    },
                ),
            }
        }
    }
}

#[derive(Clone)]
struct Endpoint {
    side: Side,
    hub: Rc<RefCell<Hub>>,
}

impl NetLink for Endpoint {
    fn send(&self, target: NetTarget, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        self.hub
            .borrow_mut()
            .outbox
            .push((self.side, target, channel, bytes.to_vec()));
        Ok(())
    }

    fn disconnect(&self, peer: PeerId, reason: &str) -> Result<(), NetError> {
        let mut hub = self.hub.borrow_mut();
        hub.disconnects.push((self.side, peer, reason.to_owned()));
        match self.side {
            Side::Server => {
                hub.clients.retain(|client| *client != peer);
                hub.deliver(
                    Side::Server,
                    NetworkEvent::Disconnected {
                        peer,
                        reason: reason.to_owned(),
                    },
                );
                hub.deliver(
                    Side::Client(peer),
                    NetworkEvent::Disconnected {
                        peer: 0,
                        reason: reason.to_owned(),
                    },
                );
            }
            Side::Client(me) => {
                hub.clients.retain(|client| *client != me);
                hub.deliver(
                    Side::Client(me),
                    NetworkEvent::Disconnected {
                        peer: 0,
                        reason: reason.to_owned(),
                    },
                );
                hub.deliver(
                    Side::Server,
                    NetworkEvent::Disconnected {
                        peer: me,
                        reason: reason.to_owned(),
                    },
                );
            }
        }
        Ok(())
    }

    fn poll_events(&self) -> Vec<NetworkEvent> {
        self.hub
            .borrow_mut()
            .inbox
            .entry(self.side)
            .or_default()
            .drain(..)
            .collect()
    }
}

/// Мир сцены с заглушками скриптового хоста: тяжёлые префабы ждут загрузки, отказы задаются.
struct TestSink {
    templates: HashMap<String, PrefabTemplate>,
    heavy: HashSet<String>,
    /// Ожидающие загрузки префабы: резервы всех объектов шаблона в порядке шаблона.
    waiting: Vec<(Vec<ObjectHandle>, String, SpawnOptions)>,
    refuse: u32,
    net_state: Option<(NetMode, PeerId, Vec<PeerId>)>,
    nets: HashMap<ObjectHandle, (NetId, Option<PeerId>)>,
    events: Vec<NetEvent>,
    faults: Vec<(PeerId, u32)>,
}

impl TestSink {
    fn new(heavy: &[&str]) -> Self {
        let mut templates = HashMap::new();
        for (path, text) in [
            (CRATE, CRATE_TEXT),
            (OWNED, OWNED_TEXT),
            (HEAVY, CRATE_TEXT),
        ] {
            templates.insert(path.to_owned(), load_template(path, text));
        }
        Self {
            templates,
            heavy: heavy.iter().map(|path| (*path).to_owned()).collect(),
            waiting: Vec::new(),
            refuse: 0,
            net_state: None,
            nets: HashMap::new(),
            events: Vec::new(),
            faults: Vec::new(),
        }
    }

    /// Загрузка тяжёлого префаба завершилась: ждущие spawn создаются.
    fn finish_loading(&mut self, world: &mut World) {
        for (reserved, prefab, options) in std::mem::take(&mut self.waiting) {
            let template = &self.templates[&prefab];
            world
                .spawn(reserved, template, options)
                .expect("ожидающий spawn создаётся после загрузки");
            self.heavy.remove(&prefab);
        }
    }
}

fn load_template(path: &str, text: &str) -> PrefabTemplate {
    let path = VirtualPath::parse(path).unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(text.to_owned()) };
    PrefabTemplate::load(&path, &mut read).unwrap()
}

impl WorldSink for TestSink {
    fn set_network_state(&mut self, mode: NetMode, local_peer: PeerId, peers: Vec<PeerId>) {
        self.net_state = Some((mode, local_peer, peers));
    }

    fn set_object_net(&mut self, object: ObjectHandle, net_id: NetId, owner: Option<PeerId>) {
        self.nets.insert(object, (net_id, owner));
    }

    fn spawn(
        &mut self,
        world: &mut World,
        prefab: &str,
        options: SpawnOptions,
    ) -> Result<ObjectHandle, String> {
        if self.refuse > 0 {
            self.refuse -= 1;
            return Err("лимит объектов".to_owned());
        }
        let template = self
            .templates
            .get(prefab)
            .ok_or_else(|| format!("нет префаба {prefab}"))?;
        let root_index = template
            .objects()
            .iter()
            .position(|object| object.parent.is_none())
            .ok_or_else(|| "у префаба нет корня".to_owned())?;
        let reserved: Vec<ObjectHandle> = (0..template.objects().len())
            .map(|_| world.reserve_object())
            .collect();
        let root = reserved[root_index];
        if self.heavy.contains(prefab) {
            self.waiting.push((reserved, prefab.to_owned(), options));
            return Ok(root);
        }
        match world.spawn(reserved, template, options) {
            Ok(_) => Ok(root),
            Err(err) => Err(err.to_string()),
        }
    }

    fn despawn(&mut self, world: &mut World, root: ObjectHandle) -> bool {
        if let Some(index) = self
            .waiting
            .iter()
            .position(|(reserved, _, _)| reserved.contains(&root))
        {
            let (reserved, _, _) = self.waiting.remove(index);
            for handle in reserved {
                world.release_reservation(handle);
            }
            return true;
        }
        world.despawn_many(&[root]).is_ok()
    }

    fn queue_events(&mut self, _world: &World, events: Vec<NetEvent>) {
        self.events.extend(events);
    }

    fn drain_peer_faults(&mut self) -> Vec<(PeerId, u32)> {
        std::mem::take(&mut self.faults)
    }
}

struct Node {
    bridge: NetworkBridge<Endpoint>,
    world: World,
    sink: TestSink,
    spawned: Vec<SpawnedRoot>,
}

struct Session {
    hub: Rc<RefCell<Hub>>,
    now: Instant,
    server: Node,
    clients: Vec<Node>,
}

fn scene_world() -> World {
    let document = SceneDocument::parse("scenes/net.rvescene", SCENE).unwrap();
    World::from_document(&document).unwrap()
}

/// Зеркало объекта у участника: живой объект с тем же сетевым id.
fn mirror(node: &Node, net: NetId) -> Option<ObjectHandle> {
    node.sink
        .nets
        .iter()
        .find(|(handle, (id, _))| *id == net && node.world.is_alive(**handle))
        .map(|(handle, _)| *handle)
}

fn position_of(world: &World, handle: ObjectHandle) -> Vec3 {
    world.local_transform(handle).unwrap().position
}

fn set_position(world: &mut World, handle: ObjectHandle, position: Vec3) {
    let local = LocalTransform {
        position,
        ..LocalTransform::default()
    };
    world.set_local_transform(handle, local).unwrap();
}

fn state_message(object: NetId, x: f32) -> Message {
    Message::State {
        object,
        tick: 0,
        mask: MASK_POSITION,
        pose: replication::Pose {
            position: Vec3::new(x, 0.0, 0.0),
            rotation_deg: Vec3::ZERO,
            scale: Vec3::ONE,
        },
    }
}

impl Session {
    fn new() -> Self {
        let hub = Rc::new(RefCell::new(Hub::default()));
        let now = Instant::now();
        let mut world = scene_world();
        let mut sink = TestSink::new(&[]);
        let bridge = NetworkBridge::attach(
            Endpoint {
                side: Side::Server,
                hub: hub.clone(),
            },
            BridgeConfig {
                mode: NetMode::Server,
                tick_hz: TICK_HZ,
            },
            &mut world,
            &mut sink,
            now,
        )
        .unwrap();
        Self {
            hub,
            now,
            server: Node {
                bridge,
                world,
                sink,
                spawned: Vec::new(),
            },
            clients: Vec::new(),
        }
    }

    fn connect(&mut self) -> PeerId {
        self.connect_with(&[])
    }

    /// Подключает клиента; `heavy` — префабы, которые его загрузчик держит в ожидании.
    fn connect_with(&mut self, heavy: &[&str]) -> PeerId {
        let peer = self.clients.len() as PeerId + 1;
        let mut world = scene_world();
        let mut sink = TestSink::new(heavy);
        let bridge = NetworkBridge::attach(
            Endpoint {
                side: Side::Client(peer),
                hub: self.hub.clone(),
            },
            BridgeConfig {
                mode: NetMode::Client,
                tick_hz: TICK_HZ,
            },
            &mut world,
            &mut sink,
            self.now,
        )
        .unwrap();
        {
            let mut hub = self.hub.borrow_mut();
            hub.clients.push(peer);
            hub.deliver(Side::Server, NetworkEvent::Connected { peer });
            hub.deliver(Side::Client(peer), NetworkEvent::LocalPeer { peer });
            hub.deliver(Side::Client(peer), NetworkEvent::Connected { peer: 0 });
        }
        self.clients.push(Node {
            bridge,
            world,
            sink,
            spawned: Vec::new(),
        });
        peer
    }

    fn client(&self, peer: PeerId) -> &Node {
        &self.clients[peer as usize - 1]
    }

    fn client_mut(&mut self, peer: PeerId) -> &mut Node {
        &mut self.clients[peer as usize - 1]
    }

    /// Один шаг: доставка, приём, скрипты-заглушки, тик. Сообщения из тика уйдут на следующем шаге.
    fn step(&mut self, dt: Duration) {
        self.now += dt;
        self.hub.borrow_mut().pump();
        let now = self.now;
        let server = &mut self.server;
        server
            .bridge
            .receive(&mut server.world, &mut server.sink, now);
        for client in &mut self.clients {
            client
                .bridge
                .receive(&mut client.world, &mut client.sink, now);
        }
        let server = &mut self.server;
        server
            .bridge
            .tick(&mut server.world, &mut server.sink, &server.spawned, now);
        server.spawned.clear();
        for client in &mut self.clients {
            client
                .bridge
                .tick(&mut client.world, &mut client.sink, &[], now);
        }
    }

    fn step_n(&mut self, count: usize, dt: Duration) {
        for _ in 0..count {
            self.step(dt);
        }
    }

    /// Создаёт корень префаба на сервере, как скрипт. Шаг сразу регистрирует его в мосте.
    fn spawn_on_server(&mut self, prefab: &str) -> ObjectHandle {
        let handle = self
            .server
            .sink
            .spawn(&mut self.server.world, prefab, SpawnOptions::default())
            .unwrap();
        self.server.spawned.push(SpawnedRoot {
            handle,
            prefab: prefab.to_owned(),
        });
        self.step(STEP);
        handle
    }

    /// Шлёт сообщение от участника серверу в обход своего моста: имитация чужого или испорченного пакета.
    fn inject(&mut self, from: Side, message: &Message, channel: Channel) {
        let bytes = replication::encode(message).unwrap();
        self.hub
            .borrow_mut()
            .outbox
            .push((from, NetTarget::Server, channel, bytes));
    }
}

fn has_message(
    events: &[NetEvent],
    peer: PeerId,
    object: Option<ObjectHandle>,
    name: &str,
) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            NetEvent::Message { peer: p, object: o, name: n, .. }
                if *p == peer && *o == object && n.as_str() == name
        )
    })
}

#[test]
fn spawn_and_despawn_replicate_with_the_server_id() {
    let mut session = Session::new();
    session.connect();
    session.step_n(2, STEP);
    let root = session.spawn_on_server(CRATE);
    let net = session.server.sink.nets[&root].0;
    session.step_n(2, STEP);

    let client = session.client(1);
    let mirrored = mirror(client, net).expect("клиент создал корень с тем же сетевым id");
    assert!(
        mirror(client, child_net_id(net, "lid")).is_some(),
        "ребёнок создан"
    );
    assert!(client.sink.nets.contains_key(&mirrored));

    assert!(session.server.sink.despawn(&mut session.server.world, root));
    session.step_n(2, STEP);
    assert!(
        mirror(session.client(1), net).is_none(),
        "удаление реплицировано"
    );
}

#[test]
fn scene_deletions_reach_connected_and_late_clients() {
    let mut session = Session::new();
    session.connect();
    session.step(STEP);
    let box_handle = session.server.world.handle("box").unwrap();
    assert!(
        session
            .server
            .sink
            .despawn(&mut session.server.world, box_handle)
    );
    session.step_n(2, STEP);
    assert!(
        session.client(1).world.handle("box").is_none(),
        "удалено у подключённого"
    );

    session.connect();
    session.step_n(3, STEP);
    assert!(
        session.client(2).world.handle("box").is_none(),
        "удалено у позднего"
    );
    assert!(
        session.client(2).world.handle("wall").is_some(),
        "не сетевой объект остался"
    );
}

#[test]
fn snapshot_larger_than_one_message_recreates_every_object() {
    let mut session = Session::new();
    for _ in 0..1000 {
        session.spawn_on_server(CRATE);
    }
    session.step(STEP);
    session.connect();
    session.step_n(4, STEP);

    assert!(session.hub.borrow().snapshot_messages >= 2);
    let client = session.client(1);
    let roots = client
        .sink
        .nets
        .iter()
        .filter(|(handle, (net, _))| {
            net.is_spawned()
                && client.world.is_alive(**handle)
                && client
                    .world
                    .object(**handle)
                    .is_some_and(|object| object.parent.is_none())
        })
        .count();
    assert_eq!(roots, 1000);
}

#[test]
fn only_the_owner_moves_an_owned_object() {
    let mut session = Session::new();
    session.connect();
    session.connect();
    let root = session.spawn_on_server(OWNED);
    let net = session.server.sink.nets[&root].0;
    session.step_n(2, STEP);

    let server = &mut session.server;
    server.bridge.apply(
        vec![NetRequest::SetOwner {
            object: root,
            peer: 1,
        }],
        &mut server.world,
        &mut server.sink,
    );
    session.step_n(2, STEP);
    let owner_mirror = mirror(session.client(1), net).unwrap();
    let (net, owner) = session.client(1).sink.nets[&owner_mirror];
    assert_eq!(owner, Some(1), "владелец реплицирован");

    let node = session.client_mut(1);
    set_position(&mut node.world, owner_mirror, Vec3::new(4.0, 0.0, 0.0));
    // Владелец шлёт позу не чаще send_rate_hz (20 Гц): нужно больше 50 мс.
    session.step_n(8, STEP);
    assert!((position_of(&session.server.world, root).x - 4.0).abs() < 1e-4);

    session.inject(Side::Client(2), &state_message(net, -9.0), Channel::State);
    session.step_n(4, STEP);
    assert!(
        (position_of(&session.server.world, root).x - 4.0).abs() < 1e-4,
        "состояние не-владельца отброшено"
    );

    session.step_n(25, STEP);
    let other_mirror = mirror(session.client(2), net).unwrap();
    assert!((position_of(&session.client(2).world, other_mirror).x - 4.0).abs() < 1e-3);
}

#[test]
fn lost_state_messages_are_repaired_by_keyframes() {
    let mut session = Session::new();
    session.connect();
    let root = session.spawn_on_server(CRATE);
    let id = session.server.world.object(root).unwrap().id.clone();
    let net = session.server.sink.nets[&root].0;
    session.step_n(3, STEP);
    let child = session.server.world.handle(&format!("{id}.lid")).unwrap();

    session.hub.borrow_mut().drop_state = true;
    set_position(&mut session.server.world, root, Vec3::new(3.0, 0.0, 0.0));
    set_position(&mut session.server.world, child, Vec3::new(0.0, 1.0, 0.0));
    session.step_n(30, STEP);
    session.hub.borrow_mut().drop_state = false;
    session.step_n(150, STEP);

    let client = session.client(1);
    let root_mirror = mirror(client, net).unwrap();
    let child_mirror = mirror(client, child_net_id(net, "lid")).unwrap();
    assert!((position_of(&client.world, root_mirror).x - 3.0).abs() < 1e-4);
    assert!((position_of(&client.world, child_mirror).y - 1.0).abs() < 1e-4);
}

#[test]
fn heavy_prefab_buffers_the_latest_state_until_loaded() {
    let mut session = Session::new();
    session.connect_with(&[HEAVY]);
    let root = session.spawn_on_server(HEAVY);
    let net = session.server.sink.nets[&root].0;
    session.step_n(2, STEP);

    assert!(mirror(session.client(1), net).is_none(), "ещё не загружен");
    assert_eq!(session.client(1).sink.waiting.len(), 1);

    set_position(&mut session.server.world, root, Vec3::new(7.0, 0.0, 0.0));
    session.step_n(10, STEP);
    assert!(mirror(session.client(1), net).is_none());

    let node = session.client_mut(1);
    node.sink.finish_loading(&mut node.world);
    session.step_n(2, STEP);
    let mirror = mirror(session.client(1), net).expect("загружен");
    assert!((position_of(&session.client(1).world, mirror).x - 7.0).abs() < 1e-4);
}

#[test]
fn despawn_before_loading_cancels_the_pending_spawn() {
    let mut session = Session::new();
    session.connect_with(&[HEAVY]);
    let root = session.spawn_on_server(HEAVY);
    let net = session.server.sink.nets[&root].0;
    session.step_n(2, STEP);
    assert_eq!(session.client(1).sink.waiting.len(), 1);

    assert!(session.server.sink.despawn(&mut session.server.world, root));
    session.step_n(2, STEP);
    assert!(session.client(1).sink.waiting.is_empty(), "ожидание снято");
    let node = session.client_mut(1);
    node.sink.finish_loading(&mut node.world);
    session.step_n(2, STEP);
    assert!(
        mirror(session.client(1), net).is_none(),
        "объект не появился"
    );
}

#[test]
fn refused_spawn_resyncs_once_and_disconnects_on_a_second_refusal() {
    let mut session = Session::new();
    session.connect();
    session.step_n(2, STEP);

    session.client_mut(1).sink.refuse = 1;
    let first = session.spawn_on_server(CRATE);
    let first_net = session.server.sink.nets[&first].0;
    session.step_n(4, STEP);
    assert!(
        mirror(session.client(1), first_net).is_some(),
        "после ресинхронизации объект создан"
    );
    assert!(session.hub.borrow().disconnects.is_empty());

    session.client_mut(1).sink.refuse = 2;
    session.spawn_on_server(CRATE);
    session.step_n(8, STEP);
    assert!(
        session
            .hub
            .borrow()
            .disconnects
            .iter()
            .any(|(who, peer, _)| *who == Side::Client(1) && *peer == 0),
        "повторный отказ разрывает соединение"
    );
}

#[test]
fn faulty_peers_are_disconnected_at_the_threshold_per_window() {
    let mut session = Session::new();
    session.connect();
    session.step(STEP);

    session.server.sink.faults.push((1, 31));
    session.step(STEP);
    assert!(session.hub.borrow().disconnects.is_empty(), "ниже порога");
    session.server.sink.faults.push((1, 1));
    session.step(STEP);
    assert!(
        session
            .hub
            .borrow()
            .disconnects
            .iter()
            .any(|(who, peer, _)| *who == Side::Server && *peer == 1),
        "порог достигнут"
    );

    session.connect();
    session.step(STEP);
    session.server.sink.faults.push((2, 31));
    session.step(STEP);
    session.step(Duration::from_secs(11));
    session.server.sink.faults.push((2, 31));
    session.step(STEP);
    assert!(
        !session
            .hub
            .borrow()
            .disconnects
            .iter()
            .any(|(_, peer, _)| *peer == 2),
        "окно сбросилось"
    );

    session.connect();
    session.step(STEP);
    for _ in 0..FAULT_BURST {
        let body = Message::Despawn(NetId::spawned(1).unwrap());
        session.inject(Side::Client(3), &body, Channel::Reliable);
    }
    session.step(STEP);
    assert!(
        session
            .hub
            .borrow()
            .disconnects
            .iter()
            .any(|(who, peer, _)| *who == Side::Server && *peer == 3),
        "служебные сообщения от клиента считаются ошибками"
    );
}

#[test]
fn rpc_reaches_the_other_side_as_an_event() {
    let mut session = Session::new();
    session.connect();
    session.step_n(2, STEP);

    let node = session.client_mut(1);
    node.bridge.apply(
        vec![NetRequest::Send {
            target: NetTarget::Server,
            name: "ping".to_owned(),
            value: ParamValue::Bool(true),
            reliable: true,
        }],
        &mut node.world,
        &mut node.sink,
    );
    session.step_n(2, STEP);
    assert!(
        has_message(&session.server.sink.events, 1, None, "ping"),
        "клиент → сервер"
    );

    let server = &mut session.server;
    server.bridge.apply(
        vec![NetRequest::Send {
            target: NetTarget::All,
            name: "hello".to_owned(),
            value: ParamValue::Int(3),
            reliable: false,
        }],
        &mut server.world,
        &mut server.sink,
    );
    session.step_n(2, STEP);
    assert!(
        has_message(&session.client(1).sink.events, 0, None, "hello"),
        "сервер → клиенты"
    );
    assert!(
        has_message(&session.server.sink.events, 0, None, "hello"),
        "локальный адресат"
    );

    let box_server = session.server.world.handle("box").unwrap();
    let box_client = session.client(1).world.handle("box").unwrap();
    let server = &mut session.server;
    server.bridge.apply(
        vec![NetRequest::ObjectSend {
            object: box_server,
            name: "tick".to_owned(),
            value: ParamValue::Bool(true),
            target: NetTarget::Peer(1),
            reliable: true,
        }],
        &mut server.world,
        &mut server.sink,
    );
    session.step_n(2, STEP);
    assert!(
        has_message(&session.client(1).sink.events, 0, Some(box_client), "tick"),
        "сообщение объекту доставлено экземпляру"
    );
}

#[test]
fn interpolation_lags_behind_small_moves_and_snaps_on_large_ones() {
    let mut session = Session::new();
    session.connect();
    let root = session.spawn_on_server(CRATE);
    let net = session.server.sink.nets[&root].0;
    session.step_n(3, STEP);
    let mirror = mirror(session.client(1), net).unwrap();

    set_position(&mut session.server.world, root, Vec3::new(0.5, 0.0, 0.0));
    session.step_n(20, STEP);
    assert!((position_of(&session.client(1).world, mirror).x - 0.5).abs() < 1e-3);

    set_position(&mut session.server.world, root, Vec3::new(1.0, 0.0, 0.0));
    session.step_n(2, STEP);
    let mid = position_of(&session.client(1).world, mirror).x;
    assert!(
        mid > 0.5 + 1e-3 && mid < 1.0 - 1e-3,
        "отстаёт на задержку: {mid}"
    );
    session.step_n(20, STEP);
    assert!((position_of(&session.client(1).world, mirror).x - 1.0).abs() < 1e-4);

    set_position(&mut session.server.world, root, Vec3::new(50.0, 0.0, 0.0));
    session.step_n(2, STEP);
    assert!(
        (position_of(&session.client(1).world, mirror).x - 50.0).abs() < 1e-4,
        "большой сдвиг — телепорт"
    );
}

#[test]
fn server_disconnect_switches_the_client_offline() {
    let mut session = Session::new();
    session.connect();
    session.step(STEP);
    let server = Endpoint {
        side: Side::Server,
        hub: session.hub.clone(),
    };
    server.disconnect(1, "тест").unwrap();
    session.step(STEP);

    let client = session.client(1);
    assert_eq!(
        client.sink.net_state.as_ref().map(|state| state.0),
        Some(NetMode::Offline)
    );
    assert!(client.sink.events.iter().any(|event| matches!(
        event,
        NetEvent::PeerDisconnected { peer: 0, reason } if reason == "тест"
    )));
}
