//! Сообщения репликации поверх каналов сетевого модуля: кодек, лимиты, позы и хэш сцены.
//!
//! Формат: версия (1 байт), тип (1 байт), тело. Числа — big-endian, как в кодеке значений.
//! Разбор проверяет длины до выделения памяти, отклоняет нечисловые значения и не паникует.

use engine_scene::document::SceneDocument;
use engine_scene::network::{NetAuthority, NetId, NetworkTransformComponent, PeerId};
use engine_scene::{LocalTransform, ParamValue};
use glam::Vec3;

use crate::NetError;
use crate::codec::{Reader, decode_value, encode_value};

/// Версия формата репликации. Несовпадение версий — ошибка разбора сообщения.
pub const VERSION: u8 = 1;
/// Наибольшая длина пути префаба в сообщении.
pub const MAX_PREFAB_BYTES: usize = 512;
/// Наибольшая длина id объекта: совпадает с лимитом авторского id в `World`.
pub const MAX_ID_BYTES: usize = 64;
/// Наибольшая длина имени RPC.
pub const MAX_NAME_BYTES: usize = 64;
/// Размер одного snapshot-сообщения; остальное уходит следующими сообщениями.
pub const SNAPSHOT_CHUNK_BYTES: usize = 60 * 1024;

/// Маски полей позы: какие значения переданы.
pub const MASK_POSITION: u8 = 1;
pub const MASK_ROTATION: u8 = 2;
pub const MASK_SCALE: u8 = 4;
pub const MASK_ALL: u8 = MASK_POSITION | MASK_ROTATION | MASK_SCALE;

const TAG_SPAWN: u8 = 1;
const TAG_DESPAWN: u8 = 2;
const TAG_SCENE_DELETED: u8 = 3;
const TAG_SET_OWNER: u8 = 4;
const TAG_STATE: u8 = 5;
const TAG_RPC: u8 = 6;
const TAG_SNAPSHOT: u8 = 7;
const TAG_SNAPSHOT_END: u8 = 8;
const TAG_RESYNC: u8 = 9;

const ITEM_SPAWN: u8 = 1;
const ITEM_OBJECT: u8 = 2;
const ITEM_DELETED: u8 = 3;

/// Локальная трансформация объекта: позиция, углы Эйлера в градусах, масштаб.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: Vec3,
    pub rotation_deg: Vec3,
    pub scale: Vec3,
}

impl Pose {
    pub fn from_local(local: &LocalTransform) -> Self {
        Self {
            position: local.position,
            rotation_deg: local.rotation_deg,
            scale: local.scale,
        }
    }

    /// Берёт из `from` поля, отмеченные в `mask`; остальные остаются от `self`.
    pub fn merged(self, from: Pose, mask: u8) -> Self {
        let mut out = self;
        if mask & MASK_POSITION != 0 {
            out.position = from.position;
        }
        if mask & MASK_ROTATION != 0 {
            out.rotation_deg = from.rotation_deg;
        }
        if mask & MASK_SCALE != 0 {
            out.scale = from.scale;
        }
        out
    }

    /// Отличаются ли поля, отмеченные в `mask`.
    pub fn differs(&self, other: &Pose, mask: u8) -> bool {
        (mask & MASK_POSITION != 0 && self.position != other.position)
            || (mask & MASK_ROTATION != 0 && self.rotation_deg != other.rotation_deg)
            || (mask & MASK_SCALE != 0 && self.scale != other.scale)
    }

    /// Линейная интерполяция; углы идут по кратчайшей дуге.
    pub fn lerp(self, to: Pose, t: f32) -> Self {
        Self {
            position: self.position.lerp(to.position, t),
            rotation_deg: Vec3::new(
                angle_lerp(self.rotation_deg.x, to.rotation_deg.x, t),
                angle_lerp(self.rotation_deg.y, to.rotation_deg.y, t),
                angle_lerp(self.rotation_deg.z, to.rotation_deg.z, t),
            ),
            scale: self.scale.lerp(to.scale, t),
        }
    }
}

fn angle_lerp(from: f32, to: f32, t: f32) -> f32 {
    let delta = (to - from + 540.0).rem_euclid(360.0) - 180.0;
    from + delta * t
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

/// Создание spawn-объекта на клиенте. Корень — `root`; дети адресуются через [`child_net_id`].
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnBody {
    pub root: NetId,
    pub prefab: String,
    /// Явный id корня: совпадает на сервере и клиентах.
    pub id: String,
    pub owner: Option<PeerId>,
    /// Родитель, если он реплицируется.
    pub parent: Option<NetId>,
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
    /// Состояние позы; `tick` — серверная метка времени, `mask` — переданные поля.
    State {
        object: NetId,
        tick: u32,
        mask: u8,
        pose: Pose,
    },
    /// RPC. `object` задаёт адресата-объект, `None` — сообщение без объекта.
    Rpc {
        object: Option<NetId>,
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

/// Кодирует сообщение. Ошибка — только при превышении лимитов значения.
pub fn encode(message: &Message) -> Result<Vec<u8>, NetError> {
    let mut out = vec![VERSION];
    match message {
        Message::Spawn(body) => {
            out.push(TAG_SPAWN);
            write_spawn(&mut out, body)?;
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
        Message::State {
            object,
            tick,
            mask,
            pose,
        } => {
            out.push(TAG_STATE);
            put_u32(&mut out, object.get());
            put_u32(&mut out, *tick);
            out.push(*mask);
            put_masked_pose(&mut out, pose, *mask);
        }
        Message::Rpc {
            object,
            name,
            value,
        } => {
            out.push(TAG_RPC);
            put_opt_peer(&mut out, object.map(NetId::get));
            put_str(&mut out, name, MAX_NAME_BYTES)?;
            put_value(&mut out, value)?;
        }
        Message::Snapshot(items) => {
            out.push(TAG_SNAPSHOT);
            for item in items {
                out.extend(encode_item(item)?);
            }
        }
        Message::SnapshotEnd => out.push(TAG_SNAPSHOT_END),
        Message::ResyncRequest => out.push(TAG_RESYNC),
    }
    Ok(out)
}

/// Делит снимок на сообщения не больше [`SNAPSHOT_CHUNK_BYTES`]. Элементы не рвутся.
pub fn snapshot_chunks(items: &[SnapItem]) -> Result<Vec<Vec<u8>>, NetError> {
    let header = [VERSION, TAG_SNAPSHOT];
    let mut chunks = Vec::new();
    let mut current = header.to_vec();
    for item in items {
        let bytes = encode_item(item)?;
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
pub fn decode(bytes: &[u8]) -> Result<Message, NetError> {
    let mut reader = Reader::new(bytes);
    if reader.u8() != Some(VERSION) {
        return Err(invalid("неизвестная версия репликации"));
    }
    let tag = take_u8(&mut reader)?;
    let message = match tag {
        TAG_SPAWN => Message::Spawn(read_spawn(&mut reader)?),
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
        TAG_STATE => {
            let object = read_net(&mut reader)?;
            let tick = take_u32(&mut reader)?;
            let mask = take_u8(&mut reader)?;
            check_mask(mask)?;
            let pose = read_masked_pose(&mut reader, mask)?;
            Message::State {
                object,
                tick,
                mask,
                pose,
            }
        }
        TAG_RPC => Message::Rpc {
            object: read_opt_peer(&mut reader)?.map(NetId::from_raw),
            name: read_str(&mut reader, MAX_NAME_BYTES)?,
            value: read_value(&mut reader)?,
        },
        TAG_SNAPSHOT => {
            let mut items = Vec::new();
            while reader.remaining() > 0 {
                items.push(read_item(&mut reader)?);
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
/// Учитываются сетевые объекты (id, authority, `network_transform`) и экземпляры префабов
/// (id, путь). Порядок в тексте и форматирование TOML на хэш не влияют.
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

fn write_spawn(out: &mut Vec<u8>, body: &SpawnBody) -> Result<(), NetError> {
    put_u32(out, body.root.get());
    put_str(out, &body.prefab, MAX_PREFAB_BYTES)?;
    put_str(out, &body.id, MAX_ID_BYTES)?;
    put_opt_peer(out, body.owner);
    put_opt_peer(out, body.parent.map(NetId::get));
    put_masked_pose(out, &body.pose, MASK_ALL);
    Ok(())
}

fn read_spawn(reader: &mut Reader<'_>) -> Result<SpawnBody, NetError> {
    let root = read_net(reader)?;
    let prefab = read_str(reader, MAX_PREFAB_BYTES)?;
    let id = read_str(reader, MAX_ID_BYTES)?;
    let owner = read_opt_peer(reader)?;
    let parent = read_opt_peer(reader)?.map(NetId::from_raw);
    let pose = read_masked_pose(reader, MASK_ALL)?;
    Ok(SpawnBody {
        root,
        prefab,
        id,
        owner,
        parent,
        pose,
    })
}

fn encode_item(item: &SnapItem) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::new();
    match item {
        SnapItem::Spawn(body) => {
            out.push(ITEM_SPAWN);
            write_spawn(&mut out, body)?;
        }
        SnapItem::Object {
            object,
            owner,
            mask,
            pose,
        } => {
            out.push(ITEM_OBJECT);
            put_u32(&mut out, object.get());
            put_opt_peer(&mut out, *owner);
            out.push(*mask);
            put_masked_pose(&mut out, pose, *mask);
        }
        SnapItem::Deleted(object) => {
            out.push(ITEM_DELETED);
            put_u32(&mut out, object.get());
        }
    }
    Ok(out)
}

fn read_item(reader: &mut Reader<'_>) -> Result<SnapItem, NetError> {
    match take_u8(reader)? {
        ITEM_SPAWN => Ok(SnapItem::Spawn(read_spawn(reader)?)),
        ITEM_OBJECT => {
            let object = read_net(reader)?;
            let owner = read_opt_peer(reader)?;
            let mask = take_u8(reader)?;
            check_mask(mask)?;
            let pose = read_masked_pose(reader, mask)?;
            Ok(SnapItem::Object {
                object,
                owner,
                mask,
                pose,
            })
        }
        ITEM_DELETED => Ok(SnapItem::Deleted(read_net(reader)?)),
        _ => Err(invalid("неизвестный элемент снимка")),
    }
}

/// Пишет только поля, отмеченные в маске; порядок фиксирован: позиция, углы, масштаб.
fn put_masked_pose(out: &mut Vec<u8>, pose: &Pose, mask: u8) {
    if mask & MASK_POSITION != 0 {
        put_vec3(out, pose.position);
    }
    if mask & MASK_ROTATION != 0 {
        put_vec3(out, pose.rotation_deg);
    }
    if mask & MASK_SCALE != 0 {
        put_vec3(out, pose.scale);
    }
}

fn read_masked_pose(reader: &mut Reader<'_>, mask: u8) -> Result<Pose, NetError> {
    let mut pose = Pose {
        position: Vec3::ZERO,
        rotation_deg: Vec3::ZERO,
        scale: Vec3::ZERO,
    };
    if mask & MASK_POSITION != 0 {
        pose.position = read_vec3(reader)?;
    }
    if mask & MASK_ROTATION != 0 {
        pose.rotation_deg = read_vec3(reader)?;
    }
    if mask & MASK_SCALE != 0 {
        pose.scale = read_vec3(reader)?;
    }
    Ok(pose)
}

fn check_mask(mask: u8) -> Result<(), NetError> {
    if mask & !MASK_ALL == 0 {
        Ok(())
    } else {
        Err(invalid("неизвестная маска полей позы"))
    }
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

fn invalid(reason: &str) -> NetError {
    NetError::InvalidMessage(reason.to_owned())
}

fn truncated() -> NetError {
    invalid("сообщение репликации обрезано")
}

#[cfg(test)]
#[path = "replication_tests.rs"]
mod tests;
