//! Тесты кодека репликации и хэша сцены.

use engine_scene::ParamValue;
use engine_scene::SceneDocument;
use engine_scene::network::NetId;
use glam::Vec3;

use super::*;

fn pose(x: f32) -> Pose {
    Pose {
        position: Vec3::new(x, 2.0, 3.0),
        rotation_deg: Vec3::new(10.0, 20.0, 30.0),
        scale: Vec3::ONE,
    }
}

fn spawn_body(serial: u32) -> SpawnBody {
    SpawnBody {
        root: NetId::spawned(serial).unwrap(),
        prefab: "prefabs/crate.rveprefab".to_owned(),
        id: format!("crate@{serial}"),
        owner: Some(2),
        parent: Some(NetId::scene(9).unwrap()),
        pose: pose(1.0),
    }
}

fn sample_messages() -> Vec<Message> {
    vec![
        Message::Spawn(spawn_body(7)),
        Message::Despawn(NetId::spawned(7).unwrap()),
        Message::SceneDeleted(vec![NetId::scene(1).unwrap(), NetId::scene(2).unwrap()]),
        Message::SetOwner {
            object: NetId::spawned(3).unwrap(),
            owner: None,
        },
        Message::SetOwner {
            object: NetId::spawned(3).unwrap(),
            owner: Some(0),
        },
        Message::State {
            object: NetId::spawned(4).unwrap(),
            tick: 99,
            mask: MASK_ALL,
            pose: pose(5.0),
        },
        Message::Rpc {
            object: None,
            name: "ping".to_owned(),
            value: ParamValue::Array(vec![ParamValue::Bool(true), ParamValue::Int(-4)]),
        },
        Message::Rpc {
            object: Some(NetId::spawned(5).unwrap()),
            name: "hit".to_owned(),
            value: ParamValue::String("boom".to_owned()),
        },
        Message::Snapshot(vec![
            SnapItem::Spawn(spawn_body(8)),
            SnapItem::Object {
                object: NetId::scene(1).unwrap(),
                owner: Some(1),
                mask: MASK_ALL,
                pose: pose(-2.0),
            },
            SnapItem::Deleted(NetId::scene(3).unwrap()),
        ]),
        Message::SnapshotEnd,
        Message::ResyncRequest,
    ]
}

#[test]
fn every_message_round_trips() {
    for message in sample_messages() {
        let bytes = encode(&message).unwrap();
        assert_eq!(decode(&bytes).unwrap(), message, "{message:?}");
    }
}

#[test]
fn state_carries_only_the_masked_fields() {
    let message = Message::State {
        object: NetId::spawned(4).unwrap(),
        tick: 1,
        mask: MASK_POSITION,
        pose: pose(5.0),
    };
    let bytes = encode(&message).unwrap();
    // версия, тип, id, тик, маска и три числа позиции
    assert_eq!(bytes.len(), 2 + 4 + 4 + 1 + 12);
    let Message::State {
        mask,
        pose: decoded,
        ..
    } = decode(&bytes).unwrap()
    else {
        panic!("ожидалось состояние");
    };
    assert_eq!(mask, MASK_POSITION);
    assert_eq!(decoded.position, pose(5.0).position);
    assert_eq!(decoded.rotation_deg, Vec3::ZERO);
}

#[test]
fn truncated_messages_are_rejected() {
    for message in sample_messages()
        .into_iter()
        .filter(|message| !matches!(message, Message::Snapshot(_)))
    {
        let bytes = encode(&message).unwrap();
        for len in 0..bytes.len() {
            assert!(
                decode(&bytes[..len]).is_err(),
                "{message:?} обрезано до {len}"
            );
        }
    }
}

#[test]
fn malformed_headers_and_bodies_are_errors() {
    assert!(decode(&[]).is_err());
    assert!(decode(&[VERSION + 1, TAG_RESYNC]).is_err());
    assert!(decode(&[VERSION, 0xEE]).is_err());
    assert!(
        decode(&[VERSION, TAG_SNAPSHOT_END, 0]).is_err(),
        "лишние байты"
    );
    // Маска с неизвестными битами.
    let mut state = encode(&Message::State {
        object: NetId::spawned(4).unwrap(),
        tick: 1,
        mask: MASK_ALL,
        pose: pose(0.0),
    })
    .unwrap();
    state[2 + 4 + 4] = 0x40;
    assert!(decode(&state).is_err());
    // Количество элементов больше, чем байт в сообщении.
    let scene_deleted = [VERSION, TAG_SCENE_DELETED, 0xFF, 0xFF, 0xFF, 0xFF];
    assert!(decode(&scene_deleted).is_err());
}

#[test]
fn non_finite_pose_values_are_rejected() {
    let mut bytes = encode(&Message::State {
        object: NetId::spawned(4).unwrap(),
        tick: 1,
        mask: MASK_POSITION,
        pose: pose(0.0),
    })
    .unwrap();
    let nan = f32::NAN.to_bits().to_be_bytes();
    let start = 2 + 4 + 4 + 1;
    bytes[start..start + 4].copy_from_slice(&nan);
    assert!(decode(&bytes).is_err());
}

#[test]
fn long_strings_are_refused_before_encoding() {
    let message = Message::Spawn(SpawnBody {
        prefab: "p".repeat(MAX_PREFAB_BYTES + 1),
        ..spawn_body(1)
    });
    assert!(encode(&message).is_err());
    let rpc = Message::Rpc {
        object: None,
        name: "n".repeat(MAX_NAME_BYTES + 1),
        value: ParamValue::Bool(true),
    };
    assert!(encode(&rpc).is_err());
}

#[test]
fn random_bytes_never_panic() {
    // Детерминированный генератор: повторяемые данные без внешнего источника случайности.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..4000 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let len = (seed >> 56) as usize % 80;
        let mut bytes = vec![VERSION];
        for _ in 0..len {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            bytes.push((seed >> 56) as u8);
        }
        let _ = decode(&bytes);
    }
}

#[test]
fn snapshot_is_split_into_bounded_messages_without_losing_items() {
    let items: Vec<SnapItem> = (1..=1000)
        .map(|serial| SnapItem::Spawn(spawn_body(serial)))
        .collect();
    let chunks = snapshot_chunks(&items).unwrap();
    assert!(chunks.len() >= 2, "{} сообщений", chunks.len());
    let mut decoded = Vec::new();
    for chunk in &chunks {
        assert!(chunk.len() <= SNAPSHOT_CHUNK_BYTES, "{}", chunk.len());
        let Message::Snapshot(part) = decode(chunk).unwrap() else {
            panic!("ожидался фрагмент снимка");
        };
        decoded.extend(part);
    }
    assert_eq!(decoded, items);
}

#[test]
fn empty_snapshot_has_no_chunks() {
    assert!(snapshot_chunks(&[]).unwrap().is_empty());
}

#[test]
fn child_ids_are_stable_and_in_the_spawn_class() {
    let root = NetId::spawned(3).unwrap();
    let child = child_net_id(root, "lid");
    assert!(child.is_spawned());
    assert_eq!(child, child_net_id(root, "lid"));
    assert_ne!(child, child_net_id(root, "hinge"));
    assert_ne!(child, child_net_id(NetId::spawned(4).unwrap(), "lid"));
}

#[test]
fn pose_lerp_takes_the_short_way_around() {
    let from = Pose {
        rotation_deg: Vec3::new(350.0, 0.0, 0.0),
        ..pose(0.0)
    };
    let to = Pose {
        rotation_deg: Vec3::new(10.0, 0.0, 0.0),
        ..pose(10.0)
    };
    let halfway = from.lerp(to, 0.5);
    let angle = halfway.rotation_deg.x.rem_euclid(360.0);
    assert!(angle < 1e-3 || 360.0 - angle < 1e-3, "{angle}");
    assert!((halfway.position.x - 5.0).abs() < 1e-4);
}

fn scene(text: &str) -> SceneDocument {
    SceneDocument::parse("scenes/hash.rvescene", text).unwrap()
}

const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

#[test]
fn scene_hash_ignores_formatting_and_object_order() {
    let first = format!(
        "{CAMERA}[[objects]]\nid = \"box\"\n[objects.network_identity]\n\
         [objects.network_transform]\nsend_rate_hz = 10\n\
         [[objects]]\nid = \"crate\"\n[objects.network_identity]\n"
    );
    let second = format!(
        "# комментарий\n{CAMERA}\n[[objects]]\nid = \"crate\"\n\n[objects.network_identity]\n\
         [[objects]]\nid = \"box\"\n[objects.network_identity]\n\
         [objects.network_transform]\nsend_rate_hz = 10\n"
    );
    assert_eq!(scene_hash(&scene(&first)), scene_hash(&scene(&second)));
}

#[test]
fn scene_hash_changes_with_network_components() {
    let base = format!(
        "{CAMERA}[[objects]]\nid = \"box\"\n[objects.network_identity]\n\
         [objects.network_transform]\n"
    );
    let hash = scene_hash(&scene(&base));
    let owned = base.replace(
        "[objects.network_identity]",
        "[objects.network_identity]\nauthority = \"owner\"",
    );
    assert_ne!(hash, scene_hash(&scene(&owned)));
    let faster = base.replace(
        "[objects.network_transform]",
        "[objects.network_transform]\nsend_rate_hz = 30",
    );
    assert_ne!(hash, scene_hash(&scene(&faster)));
    let renamed = base.replace("\"box\"", "\"crate\"");
    assert_ne!(hash, scene_hash(&scene(&renamed)));
    let without_network = format!("{CAMERA}[[objects]]\nid = \"box\"\n");
    assert_ne!(hash, scene_hash(&scene(&without_network)));
}

#[test]
fn scene_hash_ignores_objects_without_network_components() {
    let plain = format!("{CAMERA}[[objects]]\nid = \"wall\"\n");
    let with_wall = format!("{CAMERA}[[objects]]\nid = \"wall\"\nname = \"Стена\"\n");
    assert_eq!(scene_hash(&scene(&plain)), scene_hash(&scene(&with_wall)));
}
