//! Сетевые компоненты объектов сцены и общие типы сетевого модуля. Транспорт, протокол и
//! шифрование находятся в `engine-network`; этот модуль от них не зависит.

use crate::document::{ParamValue, SceneError, SceneObject};
use crate::world::ObjectHandle;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Наибольшая глубина вложенности значения сообщения (массивы и таблицы).
pub const MAX_MESSAGE_DEPTH: usize = 16;
/// Наибольшее число элементов значения сообщения, включая вложенные.
pub const MAX_MESSAGE_ELEMENTS: usize = 1024;
/// Суммарный размер строк одного значения сообщения в байтах.
pub const MAX_MESSAGE_STRING_BYTES: usize = 64 * 1024;

/// Номер участника сети. Сервер всегда имеет номер [`SERVER_PEER_ID`].
pub type PeerId = u32;
/// Номер сервера.
pub const SERVER_PEER_ID: PeerId = 0;

const NET_CLASS_SPAWNED: u32 = 0x8000_0000;

/// Сетевой идентификатор объекта. Старший бит задаёт класс: объекты сцены (0) и объекты,
/// созданные при работе (1). Классы не пересекаются, поэтому id сцены не совпадает с id spawn.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NetId(u32);

impl NetId {
    /// Id объекта сцены; `None`, если значение занимает бит класса spawn.
    pub const fn scene(raw: u32) -> Option<Self> {
        if raw & NET_CLASS_SPAWNED == 0 {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// Id объекта, созданного при работе, по серийному номеру; `None`, если номер не помещается в 31 бит.
    pub const fn spawned(serial: u32) -> Option<Self> {
        if serial & NET_CLASS_SPAWNED == 0 {
            Some(Self(serial | NET_CLASS_SPAWNED))
        } else {
            None
        }
    }

    /// Восстанавливает id из 32-битного значения, полученного по сети. Любое значение допустимо.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub const fn is_scene(self) -> bool {
        self.0 & NET_CLASS_SPAWNED == 0
    }

    pub const fn is_spawned(self) -> bool {
        !self.is_scene()
    }
}

/// Сетевой id объекта сцены: blake3 от строкового id, усечённый до 31 бита класса scene.
pub fn scene_net_id(object_id: &str) -> NetId {
    let hash = blake3::hash(object_id.as_bytes());
    let [a, b, c, d, ..] = *hash.as_bytes();
    NetId(u32::from_le_bytes([a, b, c, d]) & !NET_CLASS_SPAWNED)
}

/// Проверяет, что сетевые id объектов не совпадают. `net_id` задаёт id по строковому id;
/// в документе используется [`scene_net_id`], в тестах — подменяемая функция.
pub(crate) fn validate_net_ids<'a>(
    source: &str,
    object_ids: impl IntoIterator<Item = &'a str>,
    net_id: impl Fn(&str) -> NetId,
) -> Result<(), SceneError> {
    let mut owners: HashMap<NetId, &str> = HashMap::new();
    for object_id in object_ids {
        let id = net_id(object_id);
        if let Some(owner) = owners.get(&id) {
            return Err(SceneError::new(
                source,
                Some(object_id),
                format!(
                    "сетевой id {:#010x} совпадает с объектом {owner:?}; переименуйте один из объектов",
                    id.get()
                ),
            ));
        }
        owners.insert(id, object_id);
    }
    Ok(())
}

/// Проверяет сетевые id объектов с `network_identity` раскрытого документа.
pub(crate) fn validate_scene_net_ids(
    source: &str,
    objects: &[SceneObject],
) -> Result<(), SceneError> {
    validate_net_ids(
        source,
        objects
            .iter()
            .filter(|object| object.network_identity.is_some())
            .map(|object| object.id.as_str()),
        scene_net_id,
    )
}

/// Кто присылает состояние объекта.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetAuthority {
    /// Состояние ведёт сервер.
    #[default]
    Server,
    /// Состояние присылает клиент-владелец; сервер принимает его только от владельца.
    Owner,
}

impl NetAuthority {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Сетевой объект: `[objects.network_identity]`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkIdentityComponent {
    #[serde(default, skip_serializing_if = "NetAuthority::is_default")]
    pub authority: NetAuthority,
}

const DEFAULT_SEND_RATE_HZ: u32 = 20;
pub const MAX_SEND_RATE_HZ: u32 = 60;
const DEFAULT_INTERPOLATION_DELAY_MS: u32 = 100;
pub const MAX_INTERPOLATION_DELAY_MS: u32 = 1000;
const DEFAULT_SNAP_DISTANCE: f32 = 5.0;

/// Синхронизация локальной трансформации объекта: `[objects.network_transform]`.
/// Требует `network_identity`. Значения по умолчанию не сериализуются.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkTransformComponent {
    #[serde(skip_serializing_if = "is_true")]
    pub sync_position: bool,
    #[serde(skip_serializing_if = "is_true")]
    pub sync_rotation: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub sync_scale: bool,
    /// Частота отправки состояния, 1..=60 Гц.
    #[serde(skip_serializing_if = "is_default_send_rate_hz")]
    pub send_rate_hz: u32,
    #[serde(skip_serializing_if = "is_true")]
    pub interpolate: bool,
    /// Задержка интерполяции на клиенте, 0..=1000 мс.
    #[serde(skip_serializing_if = "is_default_interpolation_delay_ms")]
    pub interpolation_delay_ms: u32,
    /// Расстояние, с которого реплика телепортируется вместо интерполяции.
    #[serde(skip_serializing_if = "is_default_snap_distance")]
    pub snap_distance: f32,
}

impl Default for NetworkTransformComponent {
    fn default() -> Self {
        Self {
            sync_position: true,
            sync_rotation: true,
            sync_scale: false,
            send_rate_hz: DEFAULT_SEND_RATE_HZ,
            interpolate: true,
            interpolation_delay_ms: DEFAULT_INTERPOLATION_DELAY_MS,
            snap_distance: DEFAULT_SNAP_DISTANCE,
        }
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_default_send_rate_hz(value: &u32) -> bool {
    *value == DEFAULT_SEND_RATE_HZ
}

fn is_default_interpolation_delay_ms(value: &u32) -> bool {
    *value == DEFAULT_INTERPOLATION_DELAY_MS
}

fn is_default_snap_distance(value: &f32) -> bool {
    *value == DEFAULT_SNAP_DISTANCE
}

/// Проверяет поля `network_transform`; наличие `network_identity` проверяет
/// [`validate_network_object`] на раскрытом документе.
pub(crate) fn validate_network_transform(
    source: &str,
    object_id: &str,
    transform: &NetworkTransformComponent,
) -> Result<(), SceneError> {
    if !(1..=MAX_SEND_RATE_HZ).contains(&transform.send_rate_hz) {
        return Err(SceneError::new(
            source,
            Some(object_id),
            format!(
                "network_transform.send_rate_hz должен быть в диапазоне 1..={MAX_SEND_RATE_HZ}"
            ),
        ));
    }
    if transform.interpolation_delay_ms > MAX_INTERPOLATION_DELAY_MS {
        return Err(SceneError::new(
            source,
            Some(object_id),
            format!(
                "network_transform.interpolation_delay_ms должен быть в диапазоне 0..={MAX_INTERPOLATION_DELAY_MS}"
            ),
        ));
    }
    if !transform.snap_distance.is_finite() || transform.snap_distance <= 0.0 {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "network_transform.snap_distance должен быть конечным и больше нуля",
        ));
    }
    Ok(())
}

/// Проверяет сетевые компоненты объекта раскрытого или авторского документа.
pub(crate) fn validate_network_object(
    source: &str,
    object: &SceneObject,
) -> Result<(), SceneError> {
    if let Some(transform) = &object.network_transform {
        if object.network_identity.is_none() {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "network_transform требует network_identity",
            ));
        }
        validate_network_transform(source, &object.id, transform)?;
    }
    Ok(())
}

/// Роль процесса в сети. `Offline` — сеть не используется.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetMode {
    #[default]
    Offline,
    /// Клиент и сервер в одном процессе (listen-server).
    Host,
    /// Выделенный сервер без локального игрока.
    Server,
    Client,
}

/// Адресат сообщения скрипта.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetTarget {
    Server,
    All,
    Peer(PeerId),
}

/// Запрос к сетевому модулю, накопленный скриптами за кадр.
#[derive(Clone, Debug, PartialEq)]
pub enum NetRequest {
    /// `rve.net.send(target, name, value)`: сообщение без привязки к объекту.
    Send {
        target: NetTarget,
        name: String,
        value: ParamValue,
        reliable: bool,
    },
    /// `obj:net_send(name, value, target)`: сообщение экземплярам того же объекта на другой стороне.
    ObjectSend {
        object: ObjectHandle,
        name: String,
        value: ParamValue,
        target: NetTarget,
        reliable: bool,
    },
    /// `obj:set_net_owner(peer)`: назначение клиента-владельца; только на сервере.
    SetOwner { object: ObjectHandle, peer: PeerId },
}

/// Событие сетевого модуля, которое доставляется скриптам.
#[derive(Clone, Debug, PartialEq)]
pub enum NetEvent {
    Message {
        peer: PeerId,
        object: Option<ObjectHandle>,
        name: String,
        value: ParamValue,
    },
    PeerConnected {
        peer: PeerId,
    },
    PeerDisconnected {
        peer: PeerId,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SceneDocument, World};

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

    fn parse(input: &str) -> Result<SceneDocument, SceneError> {
        SceneDocument::parse("scenes/net.rvescene", input)
    }

    fn object_with(components: &str) -> String {
        format!("{CAMERA}[[objects]]\nid = \"box\"\n{components}\n")
    }

    #[test]
    fn identity_defaults_to_server_and_serializes_as_an_empty_table() {
        let document = parse(&object_with("[objects.network_identity]\n")).unwrap();
        let identity = document.objects[1].network_identity.unwrap();
        assert_eq!(identity.authority, NetAuthority::Server);

        let text = document.to_toml().unwrap();
        assert!(text.contains("[objects.network_identity]"), "{text}");
        assert!(!text.contains("authority"), "{text}");
    }

    #[test]
    fn transform_defaults_are_documented_and_not_serialized() {
        let document = parse(&object_with(
            "[objects.network_identity]\n[objects.network_transform]\n",
        ))
        .unwrap();
        let transform = document.objects[1].network_transform.unwrap();
        assert_eq!(transform, NetworkTransformComponent::default());
        assert!(transform.sync_position && transform.sync_rotation && transform.interpolate);
        assert!(!transform.sync_scale);
        assert_eq!(transform.send_rate_hz, 20);
        assert_eq!(transform.interpolation_delay_ms, 100);
        assert_eq!(transform.snap_distance, 5.0);

        let text = document.to_toml().unwrap();
        assert!(text.contains("[objects.network_transform]"), "{text}");
        for key in ["sync_", "send_rate_hz", "interpolate", "snap_distance"] {
            assert!(!text.contains(key), "{key}: {text}");
        }
    }

    #[test]
    fn full_components_round_trip_through_toml() {
        let fields = "[objects.network_identity]\nauthority = \"owner\"\n\
            [objects.network_transform]\nsync_position = false\nsync_scale = true\n\
            send_rate_hz = 60\ninterpolate = false\ninterpolation_delay_ms = 0\nsnap_distance = 2.5\n";
        let document = parse(&object_with(fields)).unwrap();
        assert_eq!(
            document.objects[1].network_identity.unwrap().authority,
            NetAuthority::Owner
        );
        assert_eq!(
            document.objects[1].network_transform,
            Some(NetworkTransformComponent {
                sync_position: false,
                sync_rotation: true,
                sync_scale: true,
                send_rate_hz: 60,
                interpolate: false,
                interpolation_delay_ms: 0,
                snap_distance: 2.5,
            })
        );

        let text = document.to_toml().unwrap();
        let again = parse(&text).unwrap();
        assert_eq!(
            again.objects[1].network_identity,
            document.objects[1].network_identity
        );
        assert_eq!(
            again.objects[1].network_transform,
            document.objects[1].network_transform
        );
    }

    #[test]
    fn scenes_without_network_components_do_not_serialize_them() {
        let document = parse(CAMERA).unwrap();
        let text = toml::to_string_pretty(&document).unwrap();
        assert!(!text.contains("network"), "{text}");
    }

    #[test]
    fn unknown_fields_and_authority_values_are_rejected_by_the_schema() {
        assert!(
            parse(&object_with(
                "[objects.network_identity]\nauthority = \"client\"\n"
            ))
            .is_err()
        );
        assert!(parse(&object_with("[objects.network_identity]\nowner = 1\n")).is_err());
        assert!(
            parse(&object_with(
                "[objects.network_identity]\n[objects.network_transform]\nsend_rate = 10\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn transform_requires_identity() {
        let error = parse(&object_with("[objects.network_transform]\n")).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("box"));
        assert!(error.message.contains("network_identity"), "{error}");
    }

    #[test]
    fn transform_ranges_are_enforced() {
        for field in [
            "send_rate_hz = 0",
            "send_rate_hz = 61",
            "interpolation_delay_ms = 1001",
            "snap_distance = 0.0",
            "snap_distance = -1.0",
            "snap_distance = nan",
        ] {
            let text = object_with(&format!(
                "[objects.network_identity]\n[objects.network_transform]\n{field}\n"
            ));
            let error = parse(&text).unwrap_err();
            assert!(
                error.message.contains("network_transform"),
                "{field}: {error}"
            );
        }
        for field in [
            "send_rate_hz = 1",
            "send_rate_hz = 60",
            "interpolation_delay_ms = 0",
            "interpolation_delay_ms = 1000",
            "snap_distance = 0.5",
        ] {
            let text = object_with(&format!(
                "[objects.network_identity]\n[objects.network_transform]\n{field}\n"
            ));
            assert!(parse(&text).is_ok(), "{field}");
        }
    }

    #[test]
    fn colliding_scene_net_ids_are_a_scene_error() {
        let error = validate_net_ids("scenes/net.rvescene", ["box", "crate"], |_| {
            NetId::from_raw(7)
        })
        .unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("crate"));
        assert!(error.message.contains("\"box\""), "{error}");
        assert_eq!(error.source, "scenes/net.rvescene");

        let distinct = validate_net_ids("scenes/net.rvescene", ["box", "crate"], |id| {
            NetId::from_raw(if id == "box" { 1 } else { 2 })
        });
        assert!(distinct.is_ok());
    }

    #[test]
    fn scene_net_ids_are_stable_and_stay_in_the_scene_class() {
        let id = scene_net_id("box");
        assert!(id.is_scene());
        assert_eq!(id, scene_net_id("box"));
        assert_ne!(id, scene_net_id("crate"));
    }

    #[test]
    fn net_id_classes_do_not_overlap() {
        assert_eq!(NetId::scene(5).map(NetId::get), Some(5));
        assert!(NetId::scene(0x8000_0000).is_none());
        let spawned = NetId::spawned(5).unwrap();
        assert!(spawned.is_spawned());
        assert_eq!(spawned.get(), 0x8000_0005);
        assert!(NetId::spawned(0x8000_0000).is_none());
        assert_eq!(NetId::from_raw(spawned.get()), spawned);
    }

    #[test]
    fn world_exposes_network_components_by_handle() {
        let text = object_with(
            "[objects.network_identity]\nauthority = \"owner\"\n\
             [objects.network_transform]\nsend_rate_hz = 10\n",
        );
        let world = World::from_document(&parse(&text).unwrap()).unwrap();
        let handle = world.handle("box").unwrap();
        assert_eq!(
            world.network_identity(handle).unwrap().authority,
            NetAuthority::Owner
        );
        assert_eq!(world.network_transform(handle).unwrap().send_rate_hz, 10);
        let camera = world.handle("camera").unwrap();
        assert!(world.network_identity(camera).is_none());
        assert!(world.network_transform(camera).is_none());
    }
}
