//! Сообщения репликации поверх каналов сетевого модуля: кодек, лимиты, позы и хэш сцены.
//!
//! Формат: версия (1 байт), тип (1 байт), тело. Числа — big-endian, как в кодеке значений.
//! Разбор проверяет длины до выделения памяти, отклоняет нечисловые значения и не паникует.
//!
//! Поза кодируется так: маска полей (1 байт), затем отмеченные поля. Позиция — знаковое fixed-point
//! по 24 бита на ось с шагом [`PoseCodec`]; если координата не укладывается, позиция уходит сырым
//! `Vec3` f32, а в маске ставится [`MASK_RAW_POSITION`]. Поворот — smallest-three, 6 байт. Масштаб —
//! три f32. Шаг квантования согласован в рукопожатии, поэтому кодек передаётся в каждый вызов.

use std::ops::Range;

use engine_scene::document::SceneDocument;
use engine_scene::network::{NetAuthority, NetId, NetworkTransformComponent, PeerId};
use engine_scene::{LocalTransform, ParamValue, quat_from_yxz_degrees, yxz_degrees_from_quat};
use glam::{Quat, Vec3};

use crate::NetError;
use crate::codec::{Reader, decode_value, encode_value};
use crate::packet::FRAGMENT_PAYLOAD;

/// Версия формата репликации. Несовпадение версий — ошибка разбора сообщения.
/// Версия 2: состояния передаются пакетами [`Message::States`].
/// Версия 3: поза квантуется, у объектов появляется номер появления `epoch`, есть `Leave` и дети
/// spawn-корня в `Spawn`.
pub const VERSION: u8 = 3;
/// Наибольшая длина пути префаба в сообщении.
pub const MAX_PREFAB_BYTES: usize = 512;
/// Наибольшая длина id объекта: совпадает с лимитом авторского id в `World`.
pub const MAX_ID_BYTES: usize = 64;
/// Наибольшая длина имени RPC.
pub const MAX_NAME_BYTES: usize = 64;
/// Размер одного snapshot-сообщения; остальное уходит следующими сообщениями.
pub const SNAPSHOT_CHUNK_BYTES: usize = 60 * 1024;
/// Наибольший размер пакета состояний. Ненадёжный канал передаёт сообщение одним кадром.
pub const STATE_BATCH_BYTES: usize = FRAGMENT_PAYLOAD;
/// Наибольшее число живых сетевых детей одного spawn-корня.
pub const MAX_SPAWN_CHILDREN: usize = 1024;
/// Допустимый диапазон шага квантования позиции.
pub const POSITION_STEP_RANGE: std::ops::RangeInclusive<f32> = 1e-5..=1.0;

/// Маски полей позы: какие значения переданы.
pub const MASK_POSITION: u8 = 1;
pub const MASK_ROTATION: u8 = 2;
pub const MASK_SCALE: u8 = 4;
pub const MASK_ALL: u8 = MASK_POSITION | MASK_ROTATION | MASK_SCALE;
/// Бит в байте маски на проводе: позиция передана сырым `Vec3` f32. Не входит в поля позы.
const MASK_RAW_POSITION: u8 = 8;

const TAG_SPAWN: u8 = 1;
const TAG_DESPAWN: u8 = 2;
const TAG_SCENE_DELETED: u8 = 3;
const TAG_SET_OWNER: u8 = 4;
const TAG_STATES: u8 = 5;
const TAG_RPC: u8 = 6;
const TAG_SNAPSHOT: u8 = 7;
const TAG_SNAPSHOT_END: u8 = 8;
const TAG_RESYNC: u8 = 9;
const TAG_LEAVE: u8 = 10;

const ITEM_SPAWN: u8 = 1;
const ITEM_OBJECT: u8 = 2;
const ITEM_DELETED: u8 = 3;

/// Байт на координату в fixed-point: 24-битное знаковое число.
const COORD_BYTES: usize = 3;
/// Наибольший по модулю квантованный номер координаты; −2^23 не используется.
const COORD_LIMIT: f64 = ((1 << 23) - 1) as f64;
/// Кватернион smallest-three: 2 бита индекса наибольшей компоненты и 3 × 15 бит остальных.
const ROTATION_BYTES: usize = 6;
const ROTATION_BITS: u32 = 47;
const COMPONENT_BITS: u32 = 15;
const COMPONENT_MAX: u32 = (1 << COMPONENT_BITS) - 1;
/// Середина шкалы: квантованное значение нуля. Уровни симметричны: `0..=2·CENTER` помещаются в 15 бит.
const COMPONENT_CENTER: u32 = (1 << (COMPONENT_BITS - 1)) - 1;
/// Компоненты, отличные от наибольшей, лежат в `[-RANGE, RANGE]`: квадрат наибольшей не меньше 1/4.
const COMPONENT_RANGE: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Кодек поз: шаг квантования позиции. Клиент и сервер получают один шаг в рукопожатии.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoseCodec {
    step: f32,
}

impl PoseCodec {
    /// Проверяет диапазон шага. Нечисловой шаг тоже отклоняется.
    pub fn new(step: f32) -> Result<Self, NetError> {
        if POSITION_STEP_RANGE.contains(&step) {
            Ok(Self { step })
        } else {
            Err(NetError::Config(format!(
                "шаг квантования позиции должен быть в диапазоне {}..={}",
                POSITION_STEP_RANGE.start(),
                POSITION_STEP_RANGE.end()
            )))
        }
    }

    pub fn step(self) -> f32 {
        self.step
    }

    /// Квантованное значение позиции. Координата вне диапазона fixed-point или нечисловая
    /// переводит всю позицию в сырой формат.
    fn position_key(self, value: Vec3) -> PositionKey {
        let mut fixed = [0i32; 3];
        for (out, component) in fixed.iter_mut().zip(value.to_array()) {
            let steps = (f64::from(component) / f64::from(self.step)).round();
            // NaN не проходит сравнение и тоже уходит в сырой формат, где отклоняется при кодировании.
            if steps.is_nan() || steps.abs() > COORD_LIMIT {
                return PositionKey::Raw(value.to_array().map(f32::to_bits));
            }
            *out = steps as i32;
        }
        PositionKey::Fixed(fixed)
    }
}

/// Сравнимое квантованное представление позиции.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PositionKey {
    Fixed([i32; 3]),
    Raw([u32; 3]),
}

/// Локальная поза объекта: позиция, ориентация и масштаб. Ориентация хранится кватернионом;
/// углы Эйлера в градусах появляются только на границе с `World`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Pose {
    pub fn from_local(local: &LocalTransform) -> Self {
        Self {
            position: local.position,
            rotation: quat_from_yxz_degrees(local.rotation_deg),
            scale: local.scale,
        }
    }

    /// Углы Эйлера YXZ в градусах для `World`. Исходные углы не восстанавливаются.
    pub fn rotation_yxz_degrees(&self) -> Vec3 {
        yxz_degrees_from_quat(self.rotation)
    }

    /// Берёт из `from` поля, отмеченные в `mask`; остальные остаются от `self`.
    pub fn merged(self, from: Pose, mask: u8) -> Self {
        let mut out = self;
        if mask & MASK_POSITION != 0 {
            out.position = from.position;
        }
        if mask & MASK_ROTATION != 0 {
            out.rotation = from.rotation;
        }
        if mask & MASK_SCALE != 0 {
            out.scale = from.scale;
        }
        out
    }

    /// Отличаются ли поля, отмеченные в `mask`, после квантования: то, что увидит получатель.
    pub fn differs(&self, other: &Pose, mask: u8, codec: PoseCodec) -> bool {
        (mask & MASK_POSITION != 0
            && codec.position_key(self.position) != codec.position_key(other.position))
            || (mask & MASK_ROTATION != 0
                && rotation_bits(self.rotation) != rotation_bits(other.rotation))
            || (mask & MASK_SCALE != 0 && self.scale != other.scale)
    }

    /// Линейная интерполяция позиции и масштаба; ориентация — slerp по кратчайшей дуге.
    pub fn lerp(self, to: Pose, t: f32) -> Self {
        let rotation = if self.rotation.is_finite() && to.rotation.is_finite() {
            self.rotation.slerp(to.rotation, t)
        } else {
            self.rotation
        };
        Self {
            position: self.position.lerp(to.position, t),
            rotation,
            scale: self.scale.lerp(to.scale, t),
        }
    }
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

/// Маска полей, которые объект синхронизирует по `network_transform`.
pub fn sync_mask(sync: &NetworkTransformComponent) -> u8 {
    let mut mask = 0;
    if sync.sync_position {
        mask |= MASK_POSITION;
    }
    if sync.sync_rotation {
        mask |= MASK_ROTATION;
    }
    if sync.sync_scale {
        mask |= MASK_SCALE;
    }
    mask
}

/// Живой ребёнок spawn-корня на момент создания: его владелец и синхронизируемая поза.
#[derive(Clone, Debug, PartialEq)]
pub struct ChildBody {
    pub object: NetId,
    pub owner: Option<PeerId>,
    pub mask: u8,
    pub pose: Pose,
}

/// Создание spawn-объекта на клиенте. Корень — `root`; дети адресуются через [`child_net_id`].
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnBody {
    pub root: NetId,
    /// Номер появления корня у участника. Все сообщения этого появления несут тот же номер.
    pub epoch: u32,
    pub prefab: String,
    /// Явный id корня: совпадает на сервере и клиентах.
    pub id: String,
    pub owner: Option<PeerId>,
    /// Родитель, если он реплицируется.
    pub parent: Option<NetId>,
    pub pose: Pose,
    /// Живые сетевые дети корня с их позами и владельцами.
    pub children: Vec<ChildBody>,
    /// Список `children` полный. `false` — сервер обрезал его по [`MAX_SPAWN_CHILDREN`]: клиент не
    /// удаляет детей, которых в списке нет.
    pub children_complete: bool,
}

/// Состояние одного объекта в пакете: `mask` — переданные поля позы.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StateItem {
    pub object: NetId,
    pub epoch: u32,
    pub mask: u8,
    pub pose: Pose,
}

/// Элемент снимка для нового участника.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapItem {
    /// Живой spawn-корень.
    Spawn(SpawnBody),
    /// Состояние любого реплицируемого объекта: владелец и поля позы из `mask`.
    Object {
        object: NetId,
        epoch: u32,
        owner: Option<PeerId>,
        mask: u8,
        pose: Pose,
    },
    /// Объект сцены, удалённый за сессию.
    Deleted(NetId),
}

/// Сообщение репликации.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Spawn(SpawnBody),
    Despawn(NetId),
    SceneDeleted(Vec<NetId>),
    SetOwner {
        object: NetId,
        owner: Option<PeerId>,
    },
    /// Временное снятие spawn-корня с его поддеревом. Клиент пока игнорирует сообщение.
    Leave {
        root: NetId,
        epoch: u32,
    },
    /// Состояния поз одним пакетом; `tick` — серверная метка времени общая для всех элементов.
    States {
        tick: u32,
        items: Vec<StateItem>,
    },
    /// RPC. `object` задаёт адресата-объект, `None` — сообщение без объекта. `epoch` нужен только
    /// объектному RPC и на проводе есть лишь при `object: Some`; без объекта разбор даёт 0.
    Rpc {
        object: Option<NetId>,
        epoch: u32,
        name: String,
        value: ParamValue,
    },
    Snapshot(Vec<SnapItem>),
    SnapshotEnd,
    ResyncRequest,
}

/// Явный id корня сетевого spawn: `n<netid>-<stem>`. Клиенты создают объект с ним, поэтому он
/// одинаков у всех участников. Недопустимые символы имени заменяются на `_`.
pub fn spawn_id(root: NetId, prefab: &str) -> String {
    let stem = std::path::Path::new(prefab)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("prefab");
    let clean: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    format!("n{}-{clean}", root.get())
}

/// Id ребёнка spawn-корня: детерминированно вычисляется и сервером, и клиентом по id корня
/// и относительному id ребёнка. Коллизии с серийными id маловероятны; мост их проверяет.
pub fn child_net_id(root: NetId, relative_id: &str) -> NetId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&root.get().to_le_bytes());
    hasher.update(relative_id.as_bytes());
    let [a, b, c, d, ..] = *hasher.finalize().as_bytes();
    NetId::from_raw((u32::from_le_bytes([a, b, c, d]) & 0x7FFF_FFFF) | 0x8000_0000)
}

/// Кодирует сообщение. Ошибка — при превышении лимитов значения или нечисловой позе.
pub fn encode(message: &Message, codec: PoseCodec) -> Result<Vec<u8>, NetError> {
    let mut out = vec![VERSION];
    match message {
        Message::Spawn(body) => {
            out.push(TAG_SPAWN);
            write_spawn(&mut out, body, codec)?;
        }
        Message::Despawn(object) => {
            out.push(TAG_DESPAWN);
            put_u32(&mut out, object.get());
        }
        Message::SceneDeleted(objects) => {
            out.push(TAG_SCENE_DELETED);
            put_u32(&mut out, len_u32(objects.len())?);
            for object in objects {
                put_u32(&mut out, object.get());
            }
        }
        Message::SetOwner { object, owner } => {
            out.push(TAG_SET_OWNER);
            put_u32(&mut out, object.get());
            put_opt_peer(&mut out, *owner);
        }
        Message::Leave { root, epoch } => {
            out.push(TAG_LEAVE);
            put_u32(&mut out, root.get());
            put_u32(&mut out, *epoch);
        }
        Message::States { tick, items } => {
            out.push(TAG_STATES);
            put_u32(&mut out, *tick);
            for item in items {
                write_state_item(&mut out, item, codec)?;
            }
        }
        Message::Rpc {
            object,
            epoch,
            name,
            value,
        } => {
            out.push(TAG_RPC);
            put_opt_peer(&mut out, object.map(NetId::get));
            if object.is_some() {
                put_u32(&mut out, *epoch);
            }
            put_str(&mut out, name, MAX_NAME_BYTES)?;
            put_value(&mut out, value)?;
        }
        Message::Snapshot(items) => {
            out.push(TAG_SNAPSHOT);
            for item in items {
                out.extend(encode_item(item, codec)?);
            }
        }
        Message::SnapshotEnd => out.push(TAG_SNAPSHOT_END),
        Message::ResyncRequest => out.push(TAG_RESYNC),
    }
    Ok(out)
}

/// Делит состояния на непрерывные группы, каждая из которых кодируется в одно сообщение
/// не больше [`STATE_BATCH_BYTES`]. Одно состояние больше сообщения не бывает.
pub fn state_batches(items: &[StateItem], codec: PoseCodec) -> Vec<Range<usize>> {
    const HEADER: usize = 2 + 4;
    let mut batches = Vec::new();
    let mut start = 0;
    let mut size = HEADER;
    for (index, item) in items.iter().enumerate() {
        let len = state_item_len(item, codec);
        if index > start && size + len > STATE_BATCH_BYTES {
            batches.push(start..index);
            start = index;
            size = HEADER;
        }
        size += len;
    }
    if start < items.len() {
        batches.push(start..items.len());
    }
    batches
}

/// Делит снимок на сообщения не больше [`SNAPSHOT_CHUNK_BYTES`]. Элементы не рвутся.
pub fn snapshot_chunks(items: &[SnapItem], codec: PoseCodec) -> Result<Vec<Vec<u8>>, NetError> {
    let header = [VERSION, TAG_SNAPSHOT];
    let mut chunks = Vec::new();
    let mut current = header.to_vec();
    for item in items {
        let bytes = encode_item(item, codec)?;
        if current.len() > header.len() && current.len() + bytes.len() > SNAPSHOT_CHUNK_BYTES {
            chunks.push(std::mem::replace(&mut current, header.to_vec()));
        }
        current.extend(bytes);
    }
    if current.len() > header.len() {
        chunks.push(current);
    }
    Ok(chunks)
}

/// Разбирает сообщение. Любой мусор возвращает ошибку без паники и без выделения памяти
/// по непроверенной длине.
pub fn decode(bytes: &[u8], codec: PoseCodec) -> Result<Message, NetError> {
    let mut reader = Reader::new(bytes);
    if reader.u8() != Some(VERSION) {
        return Err(invalid("неизвестная версия репликации"));
    }
    let tag = take_u8(&mut reader)?;
    let message = match tag {
        TAG_SPAWN => Message::Spawn(read_spawn(&mut reader, codec)?),
        TAG_DESPAWN => Message::Despawn(read_net(&mut reader)?),
        TAG_SCENE_DELETED => {
            let count = take_u32(&mut reader)? as usize;
            if count > reader.remaining() / 4 {
                return Err(truncated());
            }
            let objects = (0..count)
                .map(|_| read_net(&mut reader))
                .collect::<Result<Vec<_>, _>>()?;
            Message::SceneDeleted(objects)
        }
        TAG_SET_OWNER => Message::SetOwner {
            object: read_net(&mut reader)?,
            owner: read_opt_peer(&mut reader)?,
        },
        TAG_LEAVE => Message::Leave {
            root: read_net(&mut reader)?,
            epoch: take_u32(&mut reader)?,
        },
        TAG_STATES => {
            let tick = take_u32(&mut reader)?;
            let mut items = Vec::new();
            while reader.remaining() > 0 {
                items.push(read_state_item(&mut reader, codec)?);
            }
            // Пустой пакет кодек не создаёт; обрезанное до заголовка сообщение — ошибка.
            if items.is_empty() {
                return Err(invalid("пустой пакет состояний"));
            }
            Message::States { tick, items }
        }
        TAG_RPC => {
            let object = read_opt_peer(&mut reader)?.map(NetId::from_raw);
            let epoch = if object.is_some() {
                take_u32(&mut reader)?
            } else {
                0
            };
            Message::Rpc {
                object,
                epoch,
                name: read_str(&mut reader, MAX_NAME_BYTES)?,
                value: read_value(&mut reader)?,
            }
        }
        TAG_SNAPSHOT => {
            let mut items = Vec::new();
            while reader.remaining() > 0 {
                items.push(read_item(&mut reader, codec)?);
            }
            Message::Snapshot(items)
        }
        TAG_SNAPSHOT_END => Message::SnapshotEnd,
        TAG_RESYNC => Message::ResyncRequest,
        _ => return Err(invalid("неизвестный тип сообщения репликации")),
    };
    if reader.remaining() != 0 {
        return Err(invalid("лишние байты в сообщении репликации"));
    }
    Ok(message)
}

/// Канонический хэш раскрытого документа сцены для проверки совместимости клиента и сервера.
///
/// Учитываются сетевые объекты документа (id, authority, `network_transform`). Документ — результат
/// раскрытия префабов: экземпляры `[[instances]]` в нём уже развёрнуты, поэтому путь префаба входит
/// в хэш только через раскрытые объекты. Порядок в тексте и форматирование TOML на хэш не влияют.
pub fn scene_hash(document: &SceneDocument) -> [u8; 32] {
    let mut objects: Vec<Vec<u8>> = document
        .objects
        .iter()
        .filter(|object| object.network_identity.is_some())
        .map(|object| {
            let mut item = Vec::new();
            put_bytes(&mut item, object.id.as_bytes());
            let authority =
                object
                    .network_identity
                    .map_or(0, |identity| match identity.authority {
                        NetAuthority::Server => 0,
                        NetAuthority::Owner => 1,
                    });
            item.push(authority);
            item.push(u8::from(
                object
                    .network_identity
                    .is_some_and(|identity| identity.always_relevant),
            ));
            match object.network_transform {
                Some(transform) => {
                    item.push(1);
                    item.push(
                        u8::from(transform.sync_position)
                            | u8::from(transform.sync_rotation) << 1
                            | u8::from(transform.sync_scale) << 2
                            | u8::from(transform.interpolate) << 3,
                    );
                    put_u32(&mut item, transform.send_rate_hz);
                    put_u32(&mut item, transform.interpolation_delay_ms);
                    put_u32(&mut item, transform.snap_distance.to_bits());
                }
                None => item.push(0),
            }
            item
        })
        .collect();
    objects.sort();
    let mut instances: Vec<Vec<u8>> = document
        .instances
        .iter()
        .map(|instance| {
            let mut item = Vec::new();
            put_bytes(&mut item, instance.id.as_bytes());
            put_bytes(&mut item, instance.prefab.as_str().as_bytes());
            item
        })
        .collect();
    instances.sort();

    let mut buffer = Vec::new();
    for group in [&objects, &instances] {
        put_u32(&mut buffer, group.len() as u32);
        for item in group {
            put_bytes(&mut buffer, item);
        }
    }
    let mut hasher = blake3::Hasher::new_derive_key("RVE network scene hash v1");
    hasher.update(&buffer);
    *hasher.finalize().as_bytes()
}

/// Размер элемента состояния в пакете: id, номер появления и поза с маской.
fn state_item_len(item: &StateItem, codec: PoseCodec) -> usize {
    4 + 4 + pose_len(&item.pose, item.mask, codec)
}

/// Размер позы с маской на проводе: байт маски и отмеченные поля.
fn pose_len(pose: &Pose, mask: u8, codec: PoseCodec) -> usize {
    let mask = mask & MASK_ALL;
    let mut len = 1;
    if mask & MASK_POSITION != 0 {
        len += match codec.position_key(pose.position) {
            PositionKey::Fixed(_) => COORD_BYTES * 3,
            PositionKey::Raw(_) => 12,
        };
    }
    if mask & MASK_ROTATION != 0 {
        len += ROTATION_BYTES;
    }
    if mask & MASK_SCALE != 0 {
        len += 12;
    }
    len
}

fn write_state_item(out: &mut Vec<u8>, item: &StateItem, codec: PoseCodec) -> Result<(), NetError> {
    put_u32(out, item.object.get());
    put_u32(out, item.epoch);
    put_pose(out, &item.pose, item.mask, codec)
}

fn read_state_item(reader: &mut Reader<'_>, codec: PoseCodec) -> Result<StateItem, NetError> {
    let object = read_net(reader)?;
    let epoch = take_u32(reader)?;
    let (mask, pose) = read_pose(reader, codec)?;
    Ok(StateItem {
        object,
        epoch,
        mask,
        pose,
    })
}

fn write_spawn(out: &mut Vec<u8>, body: &SpawnBody, codec: PoseCodec) -> Result<(), NetError> {
    if body.children.len() > MAX_SPAWN_CHILDREN {
        return Err(invalid(format!(
            "у корня больше {MAX_SPAWN_CHILDREN} сетевых детей"
        )));
    }
    put_u32(out, body.root.get());
    put_u32(out, body.epoch);
    put_str(out, &body.prefab, MAX_PREFAB_BYTES)?;
    put_str(out, &body.id, MAX_ID_BYTES)?;
    put_opt_peer(out, body.owner);
    put_opt_peer(out, body.parent.map(NetId::get));
    put_pose(out, &body.pose, MASK_ALL, codec)?;
    out.push(u8::from(body.children_complete));
    out.extend_from_slice(&(body.children.len() as u16).to_be_bytes());
    for child in &body.children {
        put_u32(out, child.object.get());
        put_opt_peer(out, child.owner);
        put_pose(out, &child.pose, child.mask, codec)?;
    }
    Ok(())
}

fn read_spawn(reader: &mut Reader<'_>, codec: PoseCodec) -> Result<SpawnBody, NetError> {
    let root = read_net(reader)?;
    let epoch = take_u32(reader)?;
    let prefab = read_str(reader, MAX_PREFAB_BYTES)?;
    let id = read_str(reader, MAX_ID_BYTES)?;
    let owner = read_opt_peer(reader)?;
    let parent = read_opt_peer(reader)?.map(NetId::from_raw);
    let (_, pose) = read_pose(reader, codec)?;
    let children_complete = match take_u8(reader)? {
        0 => false,
        1 => true,
        _ => return Err(invalid("неверный признак полного списка детей")),
    };
    let count = reader.u16().ok_or_else(truncated)? as usize;
    if count > MAX_SPAWN_CHILDREN {
        return Err(invalid("у корня больше допустимого числа сетевых детей"));
    }
    // Ребёнок занимает не меньше 6 байт: до выделения памяти проверяем, что данные есть.
    if count > reader.remaining() / 6 {
        return Err(truncated());
    }
    let mut children = Vec::with_capacity(count);
    for _ in 0..count {
        let object = read_net(reader)?;
        let owner = read_opt_peer(reader)?;
        let (mask, pose) = read_pose(reader, codec)?;
        children.push(ChildBody {
            object,
            owner,
            mask,
            pose,
        });
    }
    Ok(SpawnBody {
        root,
        epoch,
        prefab,
        id,
        owner,
        parent,
        pose,
        children,
        children_complete,
    })
}

fn encode_item(item: &SnapItem, codec: PoseCodec) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::new();
    match item {
        SnapItem::Spawn(body) => {
            out.push(ITEM_SPAWN);
            write_spawn(&mut out, body, codec)?;
        }
        SnapItem::Object {
            object,
            epoch,
            owner,
            mask,
            pose,
        } => {
            out.push(ITEM_OBJECT);
            put_u32(&mut out, object.get());
            put_u32(&mut out, *epoch);
            put_opt_peer(&mut out, *owner);
            put_pose(&mut out, pose, *mask, codec)?;
        }
        SnapItem::Deleted(object) => {
            out.push(ITEM_DELETED);
            put_u32(&mut out, object.get());
        }
    }
    Ok(out)
}

fn read_item(reader: &mut Reader<'_>, codec: PoseCodec) -> Result<SnapItem, NetError> {
    match take_u8(reader)? {
        ITEM_SPAWN => Ok(SnapItem::Spawn(read_spawn(reader, codec)?)),
        ITEM_OBJECT => {
            let object = read_net(reader)?;
            let epoch = take_u32(reader)?;
            let owner = read_opt_peer(reader)?;
            let (mask, pose) = read_pose(reader, codec)?;
            Ok(SnapItem::Object {
                object,
                epoch,
                owner,
                mask,
                pose,
            })
        }
        ITEM_DELETED => Ok(SnapItem::Deleted(read_net(reader)?)),
        _ => Err(invalid("неизвестный элемент снимка")),
    }
}

/// Пишет позу: байт маски и только поля из `mask`. Позиция идёт fixed-point, если укладывается в
/// диапазон, иначе сырым f32 с флагом в маске. Нечисловые значения отклоняются.
fn put_pose(out: &mut Vec<u8>, pose: &Pose, mask: u8, codec: PoseCodec) -> Result<(), NetError> {
    let mask = mask & MASK_ALL;
    let position = if mask & MASK_POSITION != 0 {
        Some(codec.position_key(pose.position))
    } else {
        None
    };
    let raw = matches!(position, Some(PositionKey::Raw(_)));
    out.push(if raw { mask | MASK_RAW_POSITION } else { mask });
    match position {
        Some(PositionKey::Fixed(steps)) => {
            for value in steps {
                out.extend_from_slice(&value.to_be_bytes()[1..]);
            }
        }
        Some(PositionKey::Raw(_)) => {
            if !pose.position.is_finite() {
                return Err(invalid("нечисловое значение позы"));
            }
            put_vec3(out, pose.position);
        }
        None => {}
    }
    if mask & MASK_ROTATION != 0 {
        let bits = rotation_bits(pose.rotation).ok_or_else(|| invalid("нечисловой поворот"))?;
        out.extend_from_slice(&bits.to_be_bytes()[2..]);
    }
    if mask & MASK_SCALE != 0 {
        if !pose.scale.is_finite() {
            return Err(invalid("нечисловое значение позы"));
        }
        put_vec3(out, pose.scale);
    }
    Ok(())
}

/// Читает позу. Возвращает маску полей без служебного бита и позу; неотмеченные поля — по умолчанию.
fn read_pose(reader: &mut Reader<'_>, codec: PoseCodec) -> Result<(u8, Pose), NetError> {
    let wire = take_u8(reader)?;
    if wire & !(MASK_ALL | MASK_RAW_POSITION) != 0 {
        return Err(invalid("неизвестная маска полей позы"));
    }
    let mask = wire & MASK_ALL;
    let raw = wire & MASK_RAW_POSITION != 0;
    if raw && mask & MASK_POSITION == 0 {
        return Err(invalid("сырой флаг без позиции"));
    }
    let mut pose = Pose {
        position: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ZERO,
    };
    if mask & MASK_POSITION != 0 {
        pose.position = if raw {
            read_vec3(reader)?
        } else {
            let mut position = [0.0f32; 3];
            for value in &mut position {
                let bytes = reader.take(COORD_BYTES).ok_or_else(truncated)?;
                let packed =
                    (i32::from(bytes[0]) << 16) | (i32::from(bytes[1]) << 8) | i32::from(bytes[2]);
                // Знаковое расширение 24-битного значения.
                let steps = (packed << 8) >> 8;
                *value = (f64::from(steps) * f64::from(codec.step)) as f32;
            }
            Vec3::from_array(position)
        };
    }
    if mask & MASK_ROTATION != 0 {
        let bytes = reader.take(ROTATION_BYTES).ok_or_else(truncated)?;
        let mut buffer = [0u8; 8];
        buffer[2..].copy_from_slice(bytes);
        let bits = u64::from_be_bytes(buffer);
        pose.rotation = rotation_from_bits(bits).ok_or_else(|| invalid("неверный поворот"))?;
    }
    if mask & MASK_SCALE != 0 {
        pose.scale = read_vec3(reader)?;
    }
    Ok((mask, pose))
}

/// Smallest-three: индекс наибольшей компоненты, затем остальные три по 15 бит. Знак канонический:
/// наибольшая компонента положительна, поэтому `q` и `-q` дают одинаковые биты. При равных по модулю
/// компонентах выбирается первая по порядку. `None` — кватернион нечисловой или нулевой.
fn rotation_bits(rotation: Quat) -> Option<u64> {
    let length_squared = rotation.length_squared();
    if !length_squared.is_finite() || length_squared <= 0.0 {
        return None;
    }
    let unit = rotation.normalize().to_array();
    let mut largest = 0;
    for index in 1..4 {
        if unit[index].abs() > unit[largest].abs() {
            largest = index;
        }
    }
    let sign = if unit[largest] < 0.0 { -1.0 } else { 1.0 };
    let mut bits = (largest as u64) << (ROTATION_BITS - 2);
    let mut slot = 0;
    for (index, value) in unit.into_iter().enumerate() {
        if index == largest {
            continue;
        }
        let quantized = quantize_component(value * sign);
        bits |= u64::from(quantized) << (2 * COMPONENT_BITS - slot * COMPONENT_BITS);
        slot += 1;
    }
    Some(bits)
}

/// Обратное преобразование smallest-three. `None` — биты вне 47-битного поля.
fn rotation_from_bits(bits: u64) -> Option<Quat> {
    if bits >> ROTATION_BITS != 0 {
        return None;
    }
    let largest = ((bits >> (ROTATION_BITS - 2)) & 3) as usize;
    let mut unit = [0.0f32; 4];
    let mut sum = 0.0f32;
    let mut slot = 0;
    for (index, value) in unit.iter_mut().enumerate() {
        if index == largest {
            continue;
        }
        let quantized =
            (bits >> (2 * COMPONENT_BITS - slot * COMPONENT_BITS)) & u64::from(COMPONENT_MAX);
        *value = dequantize_component(quantized as u32);
        sum += *value * *value;
        slot += 1;
    }
    unit[largest] = (1.0 - sum).max(0.0).sqrt();
    Some(Quat::from_array(unit).normalize())
}

/// Симметричная шкала: 0 кодируется точно как середина `COMPONENT_CENTER`, шаг одинаков по обе стороны.
fn quantize_component(value: f32) -> u32 {
    let clamped = value.clamp(-COMPONENT_RANGE, COMPONENT_RANGE);
    let steps = (clamped / COMPONENT_RANGE * COMPONENT_CENTER as f32).round() as i32;
    (steps + COMPONENT_CENTER as i32) as u32
}

fn dequantize_component(quantized: u32) -> f32 {
    let steps = quantized as i32 - COMPONENT_CENTER as i32;
    steps as f32 / COMPONENT_CENTER as f32 * COMPONENT_RANGE
}

fn put_vec3(out: &mut Vec<u8>, value: Vec3) {
    for component in value.to_array() {
        put_u32(out, component.to_bits());
    }
}

fn read_vec3(reader: &mut Reader<'_>) -> Result<Vec3, NetError> {
    Ok(Vec3::new(
        read_f32(reader)?,
        read_f32(reader)?,
        read_f32(reader)?,
    ))
}

fn read_f32(reader: &mut Reader<'_>) -> Result<f32, NetError> {
    let value = f32::from_bits(take_u32(reader)?);
    if value.is_finite() {
        Ok(value)
    } else {
        Err(invalid("нечисловое значение позы"))
    }
}

fn put_value(out: &mut Vec<u8>, value: &ParamValue) -> Result<(), NetError> {
    let mut bytes = Vec::new();
    encode_value(value, &mut bytes)?;
    put_bytes(out, &bytes);
    Ok(())
}

fn read_value(reader: &mut Reader<'_>) -> Result<ParamValue, NetError> {
    let len = take_u32(reader)? as usize;
    let bytes = reader.take(len).ok_or_else(truncated)?;
    decode_value(bytes)
}

fn put_str(out: &mut Vec<u8>, value: &str, max: usize) -> Result<(), NetError> {
    if value.len() > max {
        return Err(NetError::InvalidMessage(format!(
            "строка длиннее {max} байт"
        )));
    }
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn read_str(reader: &mut Reader<'_>, max: usize) -> Result<String, NetError> {
    let len = reader.u16().ok_or_else(truncated)? as usize;
    if len > max {
        return Err(invalid("строка длиннее допустимого"));
    }
    let bytes = reader.take(len).ok_or_else(truncated)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| invalid("строка не в UTF-8"))
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_opt_peer(out: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(value) => {
            out.push(1);
            put_u32(out, value);
        }
        None => out.push(0),
    }
}

fn read_opt_peer(reader: &mut Reader<'_>) -> Result<Option<u32>, NetError> {
    match take_u8(reader)? {
        0 => Ok(None),
        1 => Ok(Some(take_u32(reader)?)),
        _ => Err(invalid("неверный признак необязательного поля")),
    }
}

fn read_net(reader: &mut Reader<'_>) -> Result<NetId, NetError> {
    Ok(NetId::from_raw(take_u32(reader)?))
}

fn take_u8(reader: &mut Reader<'_>) -> Result<u8, NetError> {
    reader.u8().ok_or_else(truncated)
}

fn take_u32(reader: &mut Reader<'_>) -> Result<u32, NetError> {
    reader.u32().ok_or_else(truncated)
}

fn len_u32(len: usize) -> Result<u32, NetError> {
    u32::try_from(len).map_err(|_| invalid("слишком длинный список"))
}

fn invalid(reason: impl Into<String>) -> NetError {
    NetError::InvalidMessage(reason.into())
}

fn truncated() -> NetError {
    invalid("сообщение репликации обрезано")
}

#[cfg(test)]
#[path = "replication_tests.rs"]
mod tests;
