//! Мост между сетевым потоком и миром сцены: репликация объектов, RPC и состояний.
//!
//! Порядок вызовов в одном шаге скриптового потока:
//! 1. [`NetworkBridge::receive`] — входящие события и сообщения;
//! 2. скрипты (`ScriptHost::update`);
//! 3. [`NetworkBridge::apply`] — запросы скриптов (`ScriptHost::drain_network_requests`);
//! 4. [`NetworkBridge::tick`] — новые spawn-корни, удаления, состояния, интерполяция, порог ошибок.
//!
//! Мир меняется через [`WorldSink`]: реализация в runtime и editor оборачивает `ScriptHost`, чтобы
//! spawn и despawn шли по тому же пути, что у скриптов. Сам `engine-network` от скриптов не зависит.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use engine_scene::network::{
    NetAuthority, NetEvent, NetId, NetMode, NetRequest, NetTarget, NetworkTransformComponent,
    PeerId, SERVER_PEER_ID, scene_net_id,
};
use engine_scene::{ObjectHandle, ParamValue, SpawnOptions, World};
use log::{debug, error, warn};

use crate::interpolation::{Clock, Sample, push_sample, sample_at};
use crate::replication::{
    self, MASK_POSITION, MASK_ROTATION, MASK_SCALE, Message, Pose, SnapItem, SpawnBody,
    child_net_id, sync_mask,
};
use crate::{Channel, NetError, NetworkEvent, NetworkHandle};

/// Число ошибок от одного участника за окно, после которого он отключается.
pub const FAULT_LIMIT: u32 = 32;
/// Окно подсчёта ошибок участника.
pub const FAULT_WINDOW: Duration = Duration::from_secs(10);
/// Наибольший интервал между keyframe состояния, даже если оно не менялось.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);
/// Наибольшее число надёжных сообщений, ждущих места в очереди.
const RETRY_LIMIT: usize = 4096;
/// Наибольшее число состояний неизвестных объектов, которые буферизуются.
const BUFFER_LIMIT: usize = 4096;
/// Причина отключения участника за ошибки сообщений.
const FAULT_REASON: &str = "слишком много ошибок в сетевых сообщениях";
/// Причина отключения клиента, которому не удалось создать объект дважды подряд.
const SPAWN_REASON: &str = "не удалось создать сетевой объект после повторного запроса";

/// Отправка и приём сообщений. [`NetworkHandle`] реализует его поверх сетевого потока;
/// тесты подставляют свою реализацию.
pub trait NetLink {
    fn send(&self, target: NetTarget, channel: Channel, bytes: &[u8]) -> Result<(), NetError>;
    fn disconnect(&self, peer: PeerId, reason: &str) -> Result<(), NetError>;
    fn poll_events(&self) -> Vec<NetworkEvent>;
}

impl NetLink for NetworkHandle {
    fn send(&self, target: NetTarget, channel: Channel, bytes: &[u8]) -> Result<(), NetError> {
        NetworkHandle::send(self, target, channel, bytes)
    }

    fn disconnect(&self, peer: PeerId, reason: &str) -> Result<(), NetError> {
        NetworkHandle::disconnect(self, peer, reason)
    }

    fn poll_events(&self) -> Vec<NetworkEvent> {
        NetworkHandle::poll_events(self)
    }
}

/// Операции над миром, которые мост не может выполнить сам.
pub trait WorldSink {
    /// Режим и номера участников для скриптов (`rve.net.mode()`, `peers()`).
    fn set_network_state(&mut self, mode: NetMode, local_peer: PeerId, peers: Vec<PeerId>);
    /// Сетевой id и владелец объекта.
    fn set_object_net(&mut self, object: ObjectHandle, net_id: NetId, owner: Option<PeerId>);
    /// Создаёт корень префаба с явным id. Тяжёлый префаб может вернуть резерв, который станет живым
    /// позже; отказ возвращает причину, мир при этом не меняется.
    fn spawn(
        &mut self,
        world: &mut World,
        prefab: &str,
        options: SpawnOptions,
    ) -> Result<ObjectHandle, String>;
    /// Удаляет поддерево; для ещё не загруженного префаба отменяет spawn. `false` — не удалось.
    fn despawn(&mut self, world: &mut World, root: ObjectHandle) -> bool;
    /// Ставит события в очередь скриптов на следующий update.
    fn queue_events(&mut self, world: &World, events: Vec<NetEvent>);
    /// Число ошибок скриптов на сетевых событиях от каждого удалённого участника с прошлого вызова.
    fn drain_peer_faults(&mut self) -> Vec<(PeerId, u32)>;
}

/// Корень, созданный скриптами на сервере с прошлого шага. Берётся из `WorldChanges::spawned`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnedRoot {
    pub handle: ObjectHandle,
    pub prefab: String,
}

/// Настройки моста, общие с runtime и editor.
#[derive(Clone, Copy, Debug)]
pub struct BridgeConfig {
    pub mode: NetMode,
    /// Частота серверных меток; та же, что у `[network].tick_hz` проекта.
    pub tick_hz: u32,
}

#[derive(Clone, Debug)]
enum Kind {
    /// Объект сцены: id сцены, создан при загрузке.
    Scene,
    /// Ребёнок spawn-корня: адресуется через [`child_net_id`].
    Child,
    /// Корень spawn-объекта с путём префаба.
    Root(String),
}

struct Replica {
    handle: ObjectHandle,
    kind: Kind,
    authority: NetAuthority,
    owner: Option<PeerId>,
    sync: Option<NetworkTransformComponent>,
    /// Последняя отправленная поза и время отправки.
    last_sent: Option<(Pose, Instant)>,
    samples: VecDeque<Sample>,
}

impl Replica {
    fn new(handle: ObjectHandle, kind: Kind, owner: Option<PeerId>, world: &World) -> Option<Self> {
        let identity = world.network_identity(handle)?;
        Some(Self {
            handle,
            kind,
            authority: identity.authority,
            owner,
            sync: world.network_transform(handle).copied(),
            last_sent: None,
            samples: VecDeque::new(),
        })
    }
}

struct PendingSpawn {
    handle: ObjectHandle,
    prefab: String,
    owner: Option<PeerId>,
}

/// Последнее состояние объекта, которого ещё нет в мире.
#[derive(Clone, Copy, Debug)]
struct PartialState {
    mask: u8,
    pose: Pose,
}

#[derive(Clone, Copy, Debug)]
struct FaultWindow {
    start: Instant,
    count: u32,
}

/// Мост сетевого потока и мира. Создаётся один раз на сессию.
pub struct NetworkBridge<L: NetLink> {
    link: L,
    mode: NetMode,
    local_peer: PeerId,
    clock: Clock,
    replicas: BTreeMap<NetId, Replica>,
    by_handle: HashMap<ObjectHandle, NetId>,
    pending: BTreeMap<NetId, PendingSpawn>,
    buffered: BTreeMap<NetId, PartialState>,
    /// Объекты сцены, удалённые на сервере за сессию (только сервер).
    deleted_scene: BTreeSet<NetId>,
    peers: BTreeSet<PeerId>,
    next_serial: u32,
    faults: HashMap<PeerId, FaultWindow>,
    refusals: u32,
    retry: VecDeque<(NetTarget, Channel, Vec<u8>)>,
    /// Клиент: id, пришедшие в текущем снимке.
    snapshot_seen: Option<HashSet<NetId>>,
}

impl<L: NetLink> NetworkBridge<L> {
    /// Создаёт мост и регистрирует объекты сцены с `network_identity`. Передаёт скриптам режим.
    pub fn attach<S: WorldSink>(
        link: L,
        config: BridgeConfig,
        world: &mut World,
        sink: &mut S,
        now: Instant,
    ) -> Result<Self, NetError> {
        if config.tick_hz == 0 {
            return Err(NetError::Config("tick_hz должен быть больше нуля".into()));
        }
        let mut bridge = Self {
            link,
            mode: config.mode,
            local_peer: SERVER_PEER_ID,
            clock: Clock::new(now, config.tick_hz),
            replicas: BTreeMap::new(),
            by_handle: HashMap::new(),
            pending: BTreeMap::new(),
            buffered: BTreeMap::new(),
            deleted_scene: BTreeSet::new(),
            peers: BTreeSet::new(),
            next_serial: 1,
            faults: HashMap::new(),
            refusals: 0,
            retry: VecDeque::new(),
            snapshot_seen: None,
        };
        let scene: Vec<(NetId, ObjectHandle)> = world
            .objects()
            .iter()
            .filter(|object| object.network_identity.is_some())
            .map(|object| (scene_net_id(&object.id), object.handle))
            .collect();
        for (net, handle) in scene {
            if !bridge.adopt(net, handle, Kind::Scene, None, world, sink) {
                return Err(NetError::Config(format!(
                    "сетевой id {:#010x} объекта сцены повторяется",
                    net.get()
                )));
            }
        }
        bridge.sync_state(sink);
        Ok(bridge)
    }

    /// Обрабатывает входящие события сети. Вызывается до скриптов.
    pub fn receive<S: WorldSink>(&mut self, world: &mut World, sink: &mut S, now: Instant) {
        let mut out = Vec::new();
        for event in self.link.poll_events() {
            self.on_event(event, world, sink, now, &mut out);
        }
        if !out.is_empty() {
            sink.queue_events(world, out);
        }
    }

    /// Отправляет запросы скриптов. Запросы берутся из `ScriptHost::drain_network_requests`.
    pub fn apply<S: WorldSink>(
        &mut self,
        requests: Vec<NetRequest>,
        world: &mut World,
        sink: &mut S,
    ) {
        if self.mode == NetMode::Offline {
            return;
        }
        let mut out = Vec::new();
        for request in requests {
            match request {
                NetRequest::Send {
                    target,
                    name,
                    value,
                    reliable,
                } => self.route(target, reliable, None, name, value, &mut out),
                NetRequest::ObjectSend {
                    object,
                    name,
                    value,
                    target,
                    reliable,
                } => match self.net_of(object) {
                    Some(net) => {
                        self.route(target, reliable, Some((net, object)), name, value, &mut out)
                    }
                    None => debug!("сообщение объекту без сетевого id отброшено"),
                },
                NetRequest::SetOwner { object, peer } => self.set_owner_request(object, peer, sink),
            }
        }
        if !out.is_empty() {
            sink.queue_events(world, out);
        }
    }

    /// Шаг сети после скриптов: новые серверные корни, удаления, состояния, интерполяция и
    /// порог ошибок. `spawned` — корни, созданные скриптами с прошлого шага; клиент их игнорирует.
    pub fn tick<S: WorldSink>(
        &mut self,
        world: &mut World,
        sink: &mut S,
        spawned: &[SpawnedRoot],
        now: Instant,
    ) {
        if self.mode == NetMode::Offline {
            return;
        }
        self.flush_retry();
        if self.is_server() {
            for root in spawned {
                self.on_spawned(root, world, sink);
            }
        } else {
            self.finish_pending(world, sink);
        }
        self.scan_removed(world);
        if self.is_server() {
            self.send_states(world, now);
        } else {
            self.send_owned_states(world, now);
            self.interpolate(world, now);
        }
        for (peer, count) in sink.drain_peer_faults() {
            if self.peers.contains(&peer) {
                self.fault(peer, count, now);
            }
        }
    }

    fn is_server(&self) -> bool {
        matches!(self.mode, NetMode::Host | NetMode::Server)
    }

    fn sync_state<S: WorldSink>(&self, sink: &mut S) {
        sink.set_network_state(
            self.mode,
            self.local_peer,
            self.peers.iter().copied().collect(),
        );
    }

    fn on_event<S: WorldSink>(
        &mut self,
        event: NetworkEvent,
        world: &mut World,
        sink: &mut S,
        now: Instant,
        out: &mut Vec<NetEvent>,
    ) {
        match event {
            NetworkEvent::LocalPeer { peer } => {
                if self.mode == NetMode::Client {
                    self.local_peer = peer;
                    self.sync_state(sink);
                }
            }
            NetworkEvent::Connected { peer } => self.on_connected(peer, world, sink, out),
            NetworkEvent::Disconnected { peer, reason } => {
                self.on_disconnected(peer, reason, sink, out)
            }
            NetworkEvent::Message { peer, bytes, .. } => {
                if self.mode == NetMode::Offline {
                    return;
                }
                match replication::decode(&bytes) {
                    Ok(message) => self.on_message(peer, message, world, sink, now, out),
                    Err(err) => {
                        warn!("сообщение репликации участника {peer} отброшено: {err}");
                        self.fault(peer, 1, now);
                    }
                }
            }
        }
    }

    fn on_connected<S: WorldSink>(
        &mut self,
        peer: PeerId,
        world: &mut World,
        sink: &mut S,
        out: &mut Vec<NetEvent>,
    ) {
        match self.mode {
            NetMode::Offline => {}
            NetMode::Host | NetMode::Server => {
                if peer == SERVER_PEER_ID {
                    return;
                }
                self.peers.insert(peer);
                self.faults.remove(&peer);
                self.send_snapshot(peer, world);
                out.push(NetEvent::PeerConnected { peer });
                self.sync_state(sink);
            }
            NetMode::Client => {
                self.peers.insert(peer);
                out.push(NetEvent::PeerConnected { peer });
                self.sync_state(sink);
            }
        }
    }

    fn on_disconnected<S: WorldSink>(
        &mut self,
        peer: PeerId,
        reason: String,
        sink: &mut S,
        out: &mut Vec<NetEvent>,
    ) {
        self.peers.remove(&peer);
        self.faults.remove(&peer);
        out.push(NetEvent::PeerDisconnected { peer, reason });
        if self.mode == NetMode::Client && peer == SERVER_PEER_ID {
            // Сервер ушёл: клиент работает офлайн, объекты остаются последними известными.
            self.mode = NetMode::Offline;
            self.retry.clear();
            self.snapshot_seen = None;
        }
        self.sync_state(sink);
    }

    fn on_message<S: WorldSink>(
        &mut self,
        peer: PeerId,
        message: Message,
        world: &mut World,
        sink: &mut S,
        now: Instant,
        out: &mut Vec<NetEvent>,
    ) {
        if self.is_server() {
            if !self.peers.contains(&peer) {
                return;
            }
            match message {
                Message::State {
                    object, mask, pose, ..
                } => self.on_owner_state(peer, object, mask, pose, world),
                Message::Rpc {
                    object,
                    name,
                    value,
                } => {
                    let handle = object.and_then(|net| self.resolve(net, world));
                    out.push(NetEvent::Message {
                        peer,
                        object: handle,
                        name,
                        value,
                    });
                }
                Message::ResyncRequest => self.send_snapshot(peer, world),
                Message::Spawn(_)
                | Message::Despawn(_)
                | Message::SceneDeleted(_)
                | Message::SetOwner { .. }
                | Message::Snapshot(_)
                | Message::SnapshotEnd => {
                    warn!("участник {peer} прислал серверное сообщение репликации");
                    self.fault(peer, 1, now);
                }
            }
        } else {
            if peer != SERVER_PEER_ID {
                return;
            }
            match message {
                Message::Spawn(body) => self.on_spawn_message(body, world, sink),
                Message::Despawn(net) => self.on_despawn(net, world, sink),
                Message::SceneDeleted(objects) => {
                    for net in objects {
                        self.on_despawn(net, world, sink);
                    }
                }
                Message::SetOwner { object, owner } => self.on_set_owner(object, owner, sink),
                Message::State {
                    object,
                    tick,
                    mask,
                    pose,
                } => {
                    self.clock.observe(tick, now);
                    self.clock.last_tick = tick;
                    self.on_state(object, tick, mask, pose, world);
                }
                Message::Rpc {
                    object,
                    name,
                    value,
                } => {
                    let handle = object.and_then(|net| self.resolve(net, world));
                    out.push(NetEvent::Message {
                        peer: SERVER_PEER_ID,
                        object: handle,
                        name,
                        value,
                    });
                }
                Message::Snapshot(items) => {
                    self.snapshot_seen.get_or_insert_with(HashSet::new);
                    for item in items {
                        self.on_snapshot_item(item, world, sink);
                    }
                }
                Message::SnapshotEnd => self.finish_snapshot(world, sink),
                Message::ResyncRequest => self.fault(peer, 1, now),
            }
        }
    }

    /// Серверу пришло состояние: принимаем только от владельца объекта с authority=owner.
    fn on_owner_state(
        &mut self,
        from: PeerId,
        object: NetId,
        mask: u8,
        pose: Pose,
        world: &mut World,
    ) {
        let Some(replica) = self.replicas.get(&object) else {
            return;
        };
        if replica.authority != NetAuthority::Owner || replica.owner != Some(from) {
            return;
        }
        let Some(sync) = replica.sync else {
            return;
        };
        let mask = mask & sync_mask(&sync);
        apply_pose(world, replica.handle, pose, mask);
    }

    fn on_state(&mut self, object: NetId, tick: u32, mask: u8, pose: Pose, world: &mut World) {
        if let Some(replica) = self.replicas.get_mut(&object) {
            if replica.authority == NetAuthority::Owner && replica.owner == Some(self.local_peer) {
                return;
            }
            let Some(sync) = replica.sync else {
                return;
            };
            let mask = mask & sync_mask(&sync);
            if mask == 0 {
                return;
            }
            let base = match replica.samples.back() {
                Some(last) => last.pose,
                None => match world.local_transform(replica.handle) {
                    Some(local) => Pose::from_local(local),
                    None => return,
                },
            };
            let sample = Sample {
                time: self.clock.tick_secs(tick),
                pose: base.merged(pose, mask),
            };
            push_sample(&mut replica.samples, sample, sync.snap_distance);
        } else {
            self.buffer(object, mask, pose);
        }
    }

    /// Сохраняет последнее состояние для объекта, которого ещё нет в мире.
    fn buffer(&mut self, object: NetId, mask: u8, pose: Pose) {
        let entry = self
            .buffered
            .entry(object)
            .or_insert(PartialState { mask: 0, pose });
        entry.pose = entry.pose.merged(pose, mask);
        entry.mask |= mask;
        while self.buffered.len() > BUFFER_LIMIT {
            self.buffered.pop_first();
        }
    }

    fn on_spawn_message<S: WorldSink>(&mut self, body: SpawnBody, world: &mut World, sink: &mut S) {
        // Повтор уже известного корня из снимка не создание: счётчик отказов его не трогает.
        if self.replicas.contains_key(&body.root) || self.pending.contains_key(&body.root) {
            return;
        }
        match self.try_spawn(body, world, sink) {
            Ok(()) => self.refusals = 0,
            Err(reason) => {
                self.refusals += 1;
                warn!("клиент не создал сетевой объект: {reason}");
                if self.refusals >= 2 {
                    let _ = self.link.disconnect(SERVER_PEER_ID, SPAWN_REASON);
                } else {
                    self.transmit(
                        NetTarget::Server,
                        Channel::Reliable,
                        &Message::ResyncRequest,
                    );
                }
            }
        }
    }

    fn try_spawn<S: WorldSink>(
        &mut self,
        body: SpawnBody,
        world: &mut World,
        sink: &mut S,
    ) -> Result<(), String> {
        let parent = match body.parent {
            Some(parent) => Some(
                self.replicas
                    .get(&parent)
                    .map(|replica| replica.handle)
                    .ok_or_else(|| "родитель не найден".to_owned())?,
            ),
            None => None,
        };
        let options = SpawnOptions {
            id: Some(body.id.clone()),
            parent,
            position: Some(body.pose.position),
            rotation_deg: Some(body.pose.rotation_deg),
            scale: Some(body.pose.scale),
            name: None,
        };
        let handle = sink.spawn(world, &body.prefab, options)?;
        if world.is_alive(handle) {
            self.adopt(
                body.root,
                handle,
                Kind::Root(body.prefab.clone()),
                body.owner,
                world,
                sink,
            );
            self.adopt_children(body.root, handle, world, sink);
        } else {
            self.pending.insert(
                body.root,
                PendingSpawn {
                    handle,
                    prefab: body.prefab,
                    owner: body.owner,
                },
            );
        }
        Ok(())
    }

    /// Регистрирует объект: корень или ребёнок. Возвращает `false`, если id уже занят.
    fn adopt<S: WorldSink>(
        &mut self,
        net: NetId,
        handle: ObjectHandle,
        kind: Kind,
        owner: Option<PeerId>,
        world: &World,
        sink: &mut S,
    ) -> bool {
        if self.replicas.contains_key(&net) || self.by_handle.contains_key(&handle) {
            return false;
        }
        let Some(replica) = Replica::new(handle, kind, owner, world) else {
            return false;
        };
        self.replicas.insert(net, replica);
        self.by_handle.insert(handle, net);
        sink.set_object_net(handle, net, owner);
        true
    }

    /// Регистрирует детей живого spawn-корня и применяет к новым объектам буферизованные позы.
    fn adopt_children<S: WorldSink>(
        &mut self,
        root: NetId,
        handle: ObjectHandle,
        world: &mut World,
        sink: &mut S,
    ) {
        let Some(root_id) = world.object(handle).map(|object| object.id.clone()) else {
            return;
        };
        let prefix = format!("{root_id}.");
        let mut registered = vec![root];
        for child in world.subtree(&[handle]) {
            if child == handle || world.network_identity(child).is_none() {
                continue;
            }
            let Some(relative) = world
                .object(child)
                .and_then(|object| object.id.strip_prefix(&prefix).map(str::to_owned))
            else {
                continue;
            };
            let net = child_net_id(root, &relative);
            if self.adopt(net, child, Kind::Child, None, world, sink) {
                registered.push(net);
            } else {
                error!("сетевой id ребёнка {relative:?} совпадает с другим объектом");
            }
        }
        for net in registered {
            if let Some(partial) = self.buffered.remove(&net) {
                self.teleport(net, partial.pose, partial.mask, world);
            }
        }
    }

    fn finish_pending<S: WorldSink>(&mut self, world: &mut World, sink: &mut S) {
        let ready: Vec<NetId> = self
            .pending
            .iter()
            .filter(|(_, pending)| world.is_alive(pending.handle))
            .map(|(net, _)| *net)
            .collect();
        for net in ready {
            let Some(pending) = self.pending.remove(&net) else {
                continue;
            };
            self.adopt(
                net,
                pending.handle,
                Kind::Root(pending.prefab),
                pending.owner,
                world,
                sink,
            );
            self.adopt_children(net, pending.handle, world, sink);
        }
        let cancelled: Vec<NetId> = self
            .pending
            .iter()
            .filter(|(_, pending)| !world.is_reserved(pending.handle))
            .map(|(net, _)| *net)
            .collect();
        for net in cancelled {
            self.pending.remove(&net);
            warn!("загрузка сетевого объекта {:#010x} отменена", net.get());
        }
    }

    fn on_despawn<S: WorldSink>(&mut self, net: NetId, world: &mut World, sink: &mut S) {
        self.buffered.remove(&net);
        if let Some(pending) = self.pending.remove(&net) {
            sink.despawn(world, pending.handle);
            return;
        }
        if let Some(replica) = self.replicas.get(&net) {
            let handle = replica.handle;
            sink.despawn(world, handle);
        }
    }

    fn on_set_owner<S: WorldSink>(&mut self, object: NetId, owner: Option<PeerId>, sink: &mut S) {
        if let Some(replica) = self.replicas.get_mut(&object) {
            replica.owner = owner;
            sink.set_object_net(replica.handle, object, owner);
        } else if let Some(pending) = self.pending.get_mut(&object) {
            pending.owner = owner;
        }
    }

    fn on_snapshot_item<S: WorldSink>(&mut self, item: SnapItem, world: &mut World, sink: &mut S) {
        match item {
            SnapItem::Spawn(body) => {
                if let Some(seen) = self.snapshot_seen.as_mut() {
                    seen.insert(body.root);
                }
                self.on_spawn_message(body, world, sink);
            }
            SnapItem::Object {
                object,
                owner,
                mask,
                pose,
            } => {
                if let Some(seen) = self.snapshot_seen.as_mut() {
                    seen.insert(object);
                }
                if let Some(replica) = self.replicas.get_mut(&object) {
                    if replica.owner != owner {
                        replica.owner = owner;
                        sink.set_object_net(replica.handle, object, owner);
                    }
                    let own = replica.authority == NetAuthority::Owner
                        && replica.owner == Some(self.local_peer);
                    if !own {
                        let mask = replica.sync.map_or(0, |sync| mask & sync_mask(&sync));
                        self.teleport(object, pose, mask, world);
                    }
                } else if let Some(pending) = self.pending.get_mut(&object) {
                    pending.owner = owner;
                } else {
                    self.buffer(object, mask, pose);
                }
            }
            SnapItem::Deleted(object) => self.on_despawn(object, world, sink),
        }
    }

    /// Снимок полный: всё, чего в нём нет, клиент удаляет.
    fn finish_snapshot<S: WorldSink>(&mut self, world: &mut World, sink: &mut S) {
        let seen = self.snapshot_seen.take().unwrap_or_default();
        let stale: Vec<NetId> = self
            .replicas
            .iter()
            .filter(|(net, replica)| !matches!(replica.kind, Kind::Scene) && !seen.contains(*net))
            .map(|(net, _)| *net)
            .collect();
        for net in stale {
            self.on_despawn(net, world, sink);
        }
        let stale_pending: Vec<NetId> = self
            .pending
            .keys()
            .filter(|net| !seen.contains(*net))
            .copied()
            .collect();
        for net in stale_pending {
            self.on_despawn(net, world, sink);
        }
    }

    fn set_owner_request<S: WorldSink>(
        &mut self,
        object: ObjectHandle,
        peer: PeerId,
        sink: &mut S,
    ) {
        if !self.is_server() {
            warn!("set_net_owner вызван не на сервере и проигнорирован");
            return;
        }
        let Some(net) = self.net_of(object) else {
            debug!("set_net_owner для объекта без сетевого id проигнорирован");
            return;
        };
        if let Some(replica) = self.replicas.get_mut(&net) {
            replica.owner = Some(peer);
            sink.set_object_net(replica.handle, net, Some(peer));
        }
        self.transmit(
            NetTarget::All,
            Channel::Reliable,
            &Message::SetOwner {
                object: net,
                owner: Some(peer),
            },
        );
    }

    /// Отправляет RPC: локально (для серверных адресатов) и по сети. Клиент шлёт всё серверу.
    fn route(
        &mut self,
        target: NetTarget,
        reliable: bool,
        object: Option<(NetId, ObjectHandle)>,
        name: String,
        value: ParamValue,
        out: &mut Vec<NetEvent>,
    ) {
        let channel = if reliable {
            Channel::Rpc
        } else {
            Channel::Unreliable
        };
        let message = Message::Rpc {
            object: object.map(|(net, _)| net),
            name,
            value,
        };
        let bytes = match replication::encode(&message) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!("RPC не отправлен: {err}");
                return;
            }
        };
        if !self.is_server() {
            self.send_bytes(NetTarget::Server, channel, bytes);
            return;
        }
        let local = matches!(target, NetTarget::All | NetTarget::Server)
            || target == NetTarget::Peer(self.local_peer);
        let Message::Rpc { name, value, .. } = message else {
            return;
        };
        if local {
            out.push(NetEvent::Message {
                peer: self.local_peer,
                object: object.map(|(_, handle)| handle),
                name,
                value,
            });
        }
        match target {
            NetTarget::All => self.send_bytes(NetTarget::All, channel, bytes),
            NetTarget::Peer(peer) if peer != self.local_peer => {
                self.send_bytes(NetTarget::Peer(peer), channel, bytes)
            }
            _ => {}
        }
    }

    /// Серверные удаления и новые объекты. Удалённые объекты сцены запоминаются для снимков.
    fn scan_removed(&mut self, world: &World) {
        let dead: Vec<NetId> = self
            .replicas
            .iter()
            .filter(|(_, replica)| !world.is_alive(replica.handle))
            .map(|(net, _)| *net)
            .collect();
        let mut scene_gone = Vec::new();
        let mut despawned = Vec::new();
        for net in dead {
            let Some(replica) = self.replicas.remove(&net) else {
                continue;
            };
            self.by_handle.remove(&replica.handle);
            self.buffered.remove(&net);
            match replica.kind {
                Kind::Scene => {
                    if self.is_server() {
                        self.deleted_scene.insert(net);
                    }
                    scene_gone.push(net);
                }
                Kind::Child | Kind::Root(_) => despawned.push(net),
            }
        }
        if !self.is_server() {
            return;
        }
        if !scene_gone.is_empty() {
            self.transmit(
                NetTarget::All,
                Channel::Reliable,
                &Message::SceneDeleted(scene_gone),
            );
        }
        for net in despawned {
            self.transmit(NetTarget::All, Channel::Reliable, &Message::Despawn(net));
        }
    }

    /// Сервер заметил новый объект с `network_identity`: присваивает сетевой id, рассылает spawn.
    /// Id объекта на сервере генерируется `World`, клиенты получают [`replication::spawn_id`].
    fn on_spawned<S: WorldSink>(&mut self, root: &SpawnedRoot, world: &mut World, sink: &mut S) {
        if self.by_handle.contains_key(&root.handle)
            || !world.is_alive(root.handle)
            || world.network_identity(root.handle).is_none()
        {
            return;
        }
        let Some(object) = world.object(root.handle) else {
            return;
        };
        if root.prefab.len() > replication::MAX_PREFAB_BYTES {
            warn!(
                "префаб {:?} не реплицирован: путь слишком длинный",
                root.prefab
            );
            return;
        }
        let parent_replicated = object
            .parent
            .is_none_or(|parent| self.by_handle.contains_key(&parent));
        if !parent_replicated {
            warn!(
                "префаб {:?} не реплицирован: родитель не сетевой",
                root.prefab
            );
            return;
        }
        let Some(net) = NetId::spawned(self.next_serial) else {
            error!("закончились сетевые id для объектов, созданных при работе");
            return;
        };
        self.next_serial += 1;
        if !self.adopt(
            net,
            root.handle,
            Kind::Root(root.prefab.clone()),
            None,
            world,
            sink,
        ) {
            return;
        }
        self.adopt_children(net, root.handle, world, sink);
        if let Some(body) = self.spawn_body(net, world) {
            self.transmit(NetTarget::All, Channel::Reliable, &Message::Spawn(body));
        }
    }

    fn spawn_body(&self, net: NetId, world: &World) -> Option<SpawnBody> {
        let replica = self.replicas.get(&net)?;
        let Kind::Root(prefab) = &replica.kind else {
            return None;
        };
        let object = world.object(replica.handle)?;
        let parent = match object.parent {
            Some(parent) => Some(*self.by_handle.get(&parent)?),
            None => None,
        };
        Some(SpawnBody {
            root: net,
            prefab: prefab.clone(),
            id: replication::spawn_id(net, prefab),
            owner: replica.owner,
            parent,
            pose: Pose::from_local(&object.transform),
        })
    }

    fn send_snapshot(&mut self, peer: PeerId, world: &World) {
        let items = self.snapshot_items(world);
        match replication::snapshot_chunks(&items) {
            Ok(chunks) => {
                for chunk in chunks {
                    self.send_bytes(NetTarget::Peer(peer), Channel::Reliable, chunk);
                }
                self.transmit(
                    NetTarget::Peer(peer),
                    Channel::Reliable,
                    &Message::SnapshotEnd,
                );
            }
            Err(err) => error!("снимок для участника {peer} не собран: {err}"),
        }
    }

    fn snapshot_items(&self, world: &World) -> Vec<SnapItem> {
        let mut items = Vec::new();
        for net in self.replicas.keys() {
            if let Some(body) = self.spawn_body(*net, world) {
                items.push(SnapItem::Spawn(body));
            }
        }
        for (net, replica) in &self.replicas {
            let Some(local) = world.local_transform(replica.handle) else {
                continue;
            };
            let mask = replica.sync.map_or(0, |sync| sync_mask(&sync));
            items.push(SnapItem::Object {
                object: *net,
                owner: replica.owner,
                mask,
                pose: Pose::from_local(local),
            });
        }
        items.extend(self.deleted_scene.iter().map(|net| SnapItem::Deleted(*net)));
        items
    }

    /// Серверные состояния всем участникам: по `send_rate_hz` при изменении и keyframe раз в секунду.
    fn send_states(&mut self, world: &World, now: Instant) {
        if self.peers.is_empty() {
            return;
        }
        let tick = self.clock.server_tick(now);
        for (net, mask, pose) in self.due_states(world, now, |_| true) {
            self.transmit(
                NetTarget::All,
                Channel::State,
                &Message::State {
                    object: net,
                    tick,
                    mask,
                    pose,
                },
            );
        }
    }

    /// Клиент-владелец шлёт позу своих объектов серверу.
    fn send_owned_states(&mut self, world: &World, now: Instant) {
        if !self.peers.contains(&SERVER_PEER_ID) {
            return;
        }
        let local = self.local_peer;
        let tick = self.clock.last_tick;
        for (net, mask, pose) in self.due_states(world, now, |replica| {
            replica.authority == NetAuthority::Owner && replica.owner == Some(local)
        }) {
            self.transmit(
                NetTarget::Server,
                Channel::State,
                &Message::State {
                    object: net,
                    tick,
                    mask,
                    pose,
                },
            );
        }
    }

    fn due_states(
        &mut self,
        world: &World,
        now: Instant,
        filter: impl Fn(&Replica) -> bool,
    ) -> Vec<(NetId, u8, Pose)> {
        let mut due = Vec::new();
        for (net, replica) in self.replicas.iter_mut() {
            if !filter(replica) {
                continue;
            }
            let Some(sync) = replica.sync else {
                continue;
            };
            let mask = sync_mask(&sync);
            if mask == 0 {
                continue;
            }
            let Some(local) = world.local_transform(replica.handle) else {
                continue;
            };
            let pose = Pose::from_local(local);
            let send = match replica.last_sent {
                None => true,
                Some((previous, at)) => {
                    let elapsed = now.saturating_duration_since(at);
                    let interval =
                        Duration::from_secs_f64(1.0 / f64::from(sync.send_rate_hz.max(1)));
                    elapsed >= KEYFRAME_INTERVAL
                        || (elapsed >= interval && pose.differs(&previous, mask))
                }
            };
            if send {
                replica.last_sent = Some((pose, now));
                due.push((*net, mask, pose));
            }
        }
        due
    }

    /// Клиент применяет интерполированные позы к объектам с буфером состояний. Свои объекты
    /// (клиент — владелец) интерполяцией не трогаются: их позу ведёт локальный код.
    fn interpolate(&mut self, world: &mut World, now: Instant) {
        let Some(estimate) = self.clock.estimate(now) else {
            return;
        };
        let local = self.local_peer;
        for replica in self.replicas.values_mut() {
            let Some(sync) = replica.sync else {
                continue;
            };
            if replica.authority == NetAuthority::Owner && replica.owner == Some(local) {
                replica.samples.clear();
                continue;
            }
            let time = estimate - f64::from(sync.interpolation_delay_ms) / 1000.0;
            if let Some(pose) = sample_at(&mut replica.samples, time, sync.interpolate) {
                apply_pose(world, replica.handle, pose, sync_mask(&sync));
            }
        }
    }

    /// Мгновенно ставит позу и очищает буфер: снимок и буферизованные состояния.
    fn teleport(&mut self, net: NetId, pose: Pose, mask: u8, world: &mut World) {
        if let Some(replica) = self.replicas.get_mut(&net) {
            replica.samples.clear();
            apply_pose(world, replica.handle, pose, mask);
        }
    }

    /// Счётчик ошибок участника: за окно [`FAULT_WINDOW`] не больше [`FAULT_LIMIT`].
    fn fault(&mut self, peer: PeerId, count: u32, now: Instant) {
        let window = self.faults.entry(peer).or_insert(FaultWindow {
            start: now,
            count: 0,
        });
        if now.saturating_duration_since(window.start) >= FAULT_WINDOW {
            window.start = now;
            window.count = 0;
        }
        window.count = window.count.saturating_add(count);
        if window.count >= FAULT_LIMIT {
            self.faults.remove(&peer);
            warn!("участник {peer} отключён: {FAULT_REASON}");
            let _ = self.link.disconnect(peer, FAULT_REASON);
        }
    }

    /// Сетевой id объекта, если он реплицируется.
    fn net_of(&self, object: ObjectHandle) -> Option<NetId> {
        self.by_handle.get(&object).copied()
    }

    /// Объект по сетевому id, если он живой.
    fn resolve(&self, net: NetId, world: &World) -> Option<ObjectHandle> {
        self.replicas
            .get(&net)
            .map(|replica| replica.handle)
            .filter(|handle| world.is_alive(*handle))
    }

    fn transmit(&mut self, target: NetTarget, channel: Channel, message: &Message) {
        match replication::encode(message) {
            Ok(bytes) => self.send_bytes(target, channel, bytes),
            Err(err) => error!("сообщение репликации не закодировано: {err}"),
        }
    }

    /// Надёжные сообщения при переполненной очереди ждут в `retry`, сохраняя порядок.
    fn send_bytes(&mut self, target: NetTarget, channel: Channel, bytes: Vec<u8>) {
        if channel.is_reliable() && !self.retry.is_empty() {
            self.enqueue_retry(target, channel, bytes);
            return;
        }
        match self.link.send(target, channel, &bytes) {
            Ok(()) => {}
            Err(NetError::QueueFull) if channel.is_reliable() => {
                self.enqueue_retry(target, channel, bytes)
            }
            Err(err) => warn!("сообщение репликации не отправлено: {err}"),
        }
    }

    fn enqueue_retry(&mut self, target: NetTarget, channel: Channel, bytes: Vec<u8>) {
        if self.retry.len() < RETRY_LIMIT {
            self.retry.push_back((target, channel, bytes));
        } else {
            error!("очередь надёжных сообщений переполнена; сообщение потеряно");
        }
    }

    fn flush_retry(&mut self) {
        while let Some((target, channel, bytes)) = self.retry.pop_front() {
            match self.link.send(target, channel, &bytes) {
                Ok(()) => {}
                Err(NetError::QueueFull) => {
                    self.retry.push_front((target, channel, bytes));
                    break;
                }
                Err(err) => warn!("повторная отправка не удалась: {err}"),
            }
        }
    }
}

/// Применяет поля позы, отмеченные в маске, к локальной трансформации объекта.
fn apply_pose(world: &mut World, handle: ObjectHandle, pose: Pose, mask: u8) {
    let Some(current) = world.local_transform(handle).copied() else {
        return;
    };
    let mut next = current;
    if mask & MASK_POSITION != 0 {
        next.position = pose.position;
    }
    if mask & MASK_ROTATION != 0 {
        next.rotation_deg = pose.rotation_deg;
    }
    if mask & MASK_SCALE != 0 {
        next.scale = pose.scale;
    }
    if next != current {
        let _ = world.set_local_transform(handle, next);
    }
}
