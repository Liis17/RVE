//! Тесты кодека репликации, кодека поз и хэша сцены.

use engine_scene::ParamValue;
use engine_scene::SceneDocument;
use engine_scene::network::NetId;
use engine_scene::quat_from_yxz_degrees;
use glam::{Quat, Vec3};

use super::*;

fn codec() -> PoseCodec {
    PoseCodec::new(0.01).unwrap()
}

fn pose(x: f32) -> Pose {
    Pose {
        position: Vec3::new(x, 2.0, 3.0),
        rotation: quat_from_yxz_degrees(Vec3::new(10.0, 20.0, 30.0)),
        scale: Vec3::ONE,
    }
}

fn spawn_body(serial: u32) -> SpawnBody {
    SpawnBody {
        root: NetId::spawned(serial).unwrap(),
        epoch: 0,
        prefab: "prefabs/crate.rveprefab".to_owned(),
        id: format!("crate@{serial}"),
        owner: Some(2),
        parent: Some(NetId::scene(9).unwrap()),
        pose: pose(1.0),
        children: Vec::new(),
    }
}

fn sample_messages() -> Vec<Message> {
    vec![
        Message::Spawn(SpawnBody {
            children: vec![ChildBody {
                object: child_net_id(NetId::spawned(7).unwrap(), "lid"),
                owner: Some(3),
                mask: MASK_POSITION | MASK_ROTATION,
                pose: pose(0.5),
            }],
            ..spawn_body(7)
        }),
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
        Message::Leave {
            root: NetId::spawned(4).unwrap(),
            epoch: 12,
        },
        Message::States {
            tick: 99,
            items: vec![
                StateItem {
                    object: NetId::spawned(4).unwrap(),
                    epoch: 3,
                    mask: MASK_ALL,
                    pose: pose(5.0),
                },
                StateItem {
                    object: NetId::spawned(6).unwrap(),
                    epoch: 0,
                    mask: MASK_POSITION,
                    // Поля вне маски при разборе приходят значениями по умолчанию.
                    pose: Pose {
                        position: Vec3::new(-1.0, 2.0, 3.0),
                        rotation: Quat::IDENTITY,
                        scale: Vec3::ZERO,
                    },
                },
            ],
        },
        Message::Rpc {
            object: None,
            epoch: 0,
            name: "ping".to_owned(),
            value: ParamValue::Array(vec![ParamValue::Bool(true), ParamValue::Int(-4)]),
        },
        Message::Rpc {
            object: Some(NetId::spawned(5).unwrap()),
            epoch: 9,
            name: "hit".to_owned(),
            value: ParamValue::String("boom".to_owned()),
        },
        Message::Snapshot(vec![
            SnapItem::Spawn(spawn_body(8)),
            SnapItem::Object {
                object: NetId::scene(1).unwrap(),
                epoch: 0,
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

/// Что видит получатель: сообщение после одного прохода кодека.
fn quantized(message: &Message) -> Message {
    decode(&encode(message, codec()).unwrap(), codec()).unwrap()
}

#[test]
fn every_message_round_trips_to_its_quantized_form() {
    for message in sample_messages() {
        let once = quantized(&message);
        // Квантование идемпотентно: повторный проход не меняет значения и байты.
        assert_eq!(quantized(&once), once, "{message:?}");
        assert_eq!(
            encode(&once, codec()).unwrap(),
            encode(&quantized(&once), codec()).unwrap(),
            "{message:?}"
        );
    }
}

#[test]
fn messages_without_poses_round_trip_exactly() {
    for message in sample_messages().into_iter().filter(|message| {
        !matches!(
            message,
            Message::Spawn(_) | Message::Snapshot(_) | Message::States { .. }
        )
    }) {
        assert_eq!(quantized(&message), message, "{message:?}");
    }
}

#[test]
fn leave_and_object_rpc_carry_the_epoch() {
    let leave = Message::Leave {
        root: NetId::spawned(2).unwrap(),
        epoch: 0xdead_beef,
    };
    assert_eq!(quantized(&leave), leave);
    let rpc = Message::Rpc {
        object: Some(NetId::spawned(2).unwrap()),
        epoch: 41,
        name: "hit".to_owned(),
        value: ParamValue::Bool(true),
    };
    assert_eq!(quantized(&rpc), rpc);
    // Без объекта номер появления не передаётся и при разборе равен нулю.
    let global = Message::Rpc {
        object: None,
        epoch: 41,
        name: "ping".to_owned(),
        value: ParamValue::Bool(true),
    };
    let Message::Rpc { epoch, .. } = quantized(&global) else {
        panic!("ожидался RPC");
    };
    assert_eq!(epoch, 0);
}

#[test]
fn spawn_children_keep_owner_mask_and_pose() {
    let child = ChildBody {
        object: child_net_id(NetId::spawned(7).unwrap(), "lid"),
        owner: Some(5),
        mask: MASK_POSITION,
        pose: pose(2.0),
    };
    let body = SpawnBody {
        children: vec![child.clone()],
        ..spawn_body(7)
    };
    let Message::Spawn(decoded) = quantized(&Message::Spawn(body)) else {
        panic!("ожидался Spawn");
    };
    assert_eq!(decoded.children.len(), 1);
    let got = &decoded.children[0];
    assert_eq!(got.object, child.object);
    assert_eq!(got.owner, Some(5));
    assert_eq!(got.mask, MASK_POSITION);
    assert!((got.pose.position - child.pose.position).length() <= 0.005 + 1e-4);
}

#[test]
fn too_many_spawn_children_are_refused() {
    let child = ChildBody {
        object: NetId::spawned(1).unwrap(),
        owner: None,
        mask: 0,
        pose: Pose::default(),
    };
    let body = SpawnBody {
        children: vec![child; MAX_SPAWN_CHILDREN + 1],
        ..spawn_body(1)
    };
    assert!(encode(&Message::Spawn(body), codec()).is_err());
}

fn one_state(x: f32) -> StateItem {
    StateItem {
        object: NetId::spawned(4).unwrap(),
        epoch: 0,
        mask: MASK_POSITION,
        pose: pose(x),
    }
}

#[test]
fn state_carries_only_the_masked_fields() {
    let message = Message::States {
        tick: 1,
        items: vec![one_state(5.0)],
    };
    let bytes = encode(&message, codec()).unwrap();
    // версия, тип, тик, id, номер появления, маска и три координаты по 3 байта
    assert_eq!(bytes.len(), 2 + 4 + 4 + 4 + 1 + 9);
    let Message::States { items, .. } = decode(&bytes, codec()).unwrap() else {
        panic!("ожидался пакет состояний");
    };
    assert_eq!(items[0].mask, MASK_POSITION);
    assert!((items[0].pose.position - pose(5.0).position).length() <= 0.005 + 1e-4);
    assert_eq!(items[0].pose.rotation, Quat::IDENTITY);
}

#[test]
fn position_and_rotation_state_is_smaller_than_the_v2_layout() {
    let item = StateItem {
        object: NetId::spawned(4).unwrap(),
        epoch: 0,
        mask: MASK_POSITION | MASK_ROTATION,
        pose: pose(1.0),
    };
    let bytes = encode(
        &Message::States {
            tick: 1,
            items: vec![item],
        },
        codec(),
    )
    .unwrap();
    let element = bytes.len() - (2 + 4);
    // id 4 + epoch 4 + маска 1 + позиция 9 + поворот 6. Версия 2 писала 4 + 1 + 24 = 29 байт.
    assert_eq!(element, 24);
    assert!(element < 29);
}

#[test]
fn state_batches_cover_every_item_and_fit_one_frame() {
    let items: Vec<StateItem> = (0..300)
        .map(|index| StateItem {
            object: NetId::spawned(index + 1).unwrap(),
            epoch: 0,
            mask: if index % 3 == 0 {
                MASK_POSITION
            } else {
                MASK_ALL
            },
            pose: pose(index as f32),
        })
        .collect();
    let batches = state_batches(&items, codec());
    assert!(batches.len() > 1, "300 состояний не помещаются в один кадр");
    let mut next = 0;
    for range in &batches {
        assert_eq!(range.start, next, "группы идут подряд без пропусков");
        next = range.end;
        let bytes = encode(
            &Message::States {
                tick: 7,
                items: items[range.clone()].to_vec(),
            },
            codec(),
        )
        .unwrap();
        assert!(bytes.len() <= STATE_BATCH_BYTES, "{} байт", bytes.len());
    }
    assert_eq!(next, items.len());
}

#[test]
fn empty_state_batch_is_rejected() {
    let bytes = encode(
        &Message::States {
            tick: 1,
            items: vec![one_state(0.0)],
        },
        codec(),
    )
    .unwrap();
    assert!(decode(&bytes[..6], codec()).is_err(), "пакет без элементов");
}

#[test]
fn truncated_messages_are_rejected() {
    // Пакеты состояний и снимки — последовательности без счётчика: обрыв на границе элемента даёт
    // короче, но валидный пакет. Транспорт передаёт сообщение целиком, поэтому проверяем отдельно.
    for message in sample_messages()
        .into_iter()
        .filter(|message| !matches!(message, Message::Snapshot(_) | Message::States { .. }))
    {
        let bytes = encode(&message, codec()).unwrap();
        for len in 0..bytes.len() {
            assert!(
                decode(&bytes[..len], codec()).is_err(),
                "{message:?} обрезано до {len}"
            );
        }
    }
}

#[test]
fn truncated_single_state_batch_is_rejected() {
    let bytes = encode(
        &Message::States {
            tick: 1,
            items: vec![one_state(2.0)],
        },
        codec(),
    )
    .unwrap();
    for len in 0..bytes.len() {
        assert!(
            decode(&bytes[..len], codec()).is_err(),
            "пакет обрезан до {len}"
        );
    }
}

#[test]
fn malformed_headers_and_bodies_are_errors() {
    assert!(decode(&[], codec()).is_err());
    assert!(decode(&[VERSION + 1, TAG_RESYNC], codec()).is_err());
    assert!(decode(&[VERSION, 0xEE], codec()).is_err());
    assert!(
        decode(&[VERSION, TAG_SNAPSHOT_END, 0], codec()).is_err(),
        "лишние байты"
    );
    // Маска с неизвестным битом (бит 6) в элементе состояния. Смещение маски: версия, тип, тик,
    // id и номер появления — 2 + 4 + 4 + 4 = 14.
    let mut state = encode(
        &Message::States {
            tick: 1,
            items: vec![StateItem {
                object: NetId::spawned(4).unwrap(),
                epoch: 0,
                mask: MASK_ALL,
                pose: pose(0.0),
            }],
        },
        codec(),
    )
    .unwrap();
    state[14] = 0x40;
    assert!(decode(&state, codec()).is_err());
    // Количество элементов больше, чем байт в сообщении.
    let scene_deleted = [VERSION, TAG_SCENE_DELETED, 0xFF, 0xFF, 0xFF, 0xFF];
    assert!(decode(&scene_deleted, codec()).is_err());
}

#[test]
fn non_finite_pose_values_are_rejected_both_ways() {
    let mut bad = pose(0.0);
    bad.position.x = f32::NAN;
    assert!(
        encode(
            &Message::States {
                tick: 1,
                items: vec![StateItem {
                    pose: bad,
                    ..one_state(0.0)
                }]
            },
            codec()
        )
        .is_err()
    );

    let mut bad = pose(0.0);
    bad.position.y = f32::INFINITY;
    assert!(
        encode(
            &Message::States {
                tick: 1,
                items: vec![StateItem {
                    pose: bad,
                    ..one_state(0.0)
                }]
            },
            codec()
        )
        .is_err()
    );

    let mut bad = pose(0.0);
    bad.scale.z = f32::NAN;
    let item = StateItem {
        mask: MASK_SCALE,
        pose: bad,
        ..one_state(0.0)
    };
    assert!(
        encode(
            &Message::States {
                tick: 1,
                items: vec![item]
            },
            codec()
        )
        .is_err()
    );

    let mut bad = pose(0.0);
    bad.rotation = Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0);
    let item = StateItem {
        mask: MASK_ROTATION,
        pose: bad,
        ..one_state(0.0)
    };
    assert!(
        encode(
            &Message::States {
                tick: 1,
                items: vec![item]
            },
            codec()
        )
        .is_err()
    );

    // Сырой флаг позиции с нечисловым значением на проводе отклоняется при разборе.
    let mut bytes = vec![VERSION, TAG_STATES];
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.push(MASK_POSITION | MASK_RAW_POSITION);
    for _ in 0..3 {
        bytes.extend_from_slice(&f32::NAN.to_bits().to_be_bytes());
    }
    assert!(decode(&bytes, codec()).is_err());
}

#[test]
fn raw_flag_without_position_is_rejected() {
    let mut bytes = vec![VERSION, TAG_STATES];
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&4u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.push(MASK_RAW_POSITION | MASK_ROTATION);
    bytes.extend_from_slice(&[0; 6]);
    assert!(decode(&bytes, codec()).is_err());
}

#[test]
fn long_strings_are_refused_before_encoding() {
    let message = Message::Spawn(SpawnBody {
        prefab: "p".repeat(MAX_PREFAB_BYTES + 1),
        ..spawn_body(1)
    });
    assert!(encode(&message, codec()).is_err());
    let rpc = Message::Rpc {
        object: None,
        epoch: 0,
        name: "n".repeat(MAX_NAME_BYTES + 1),
        value: ParamValue::Bool(true),
    };
    assert!(encode(&rpc, codec()).is_err());
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
        let _ = decode(&bytes, codec());
        // Тот же мусор с тегом состояний и снимка: разбор поз и детей не падает.
        if bytes.len() > 1 {
            for tag in [TAG_STATES, TAG_SPAWN, TAG_SNAPSHOT, TAG_LEAVE] {
                let mut tagged = bytes.clone();
                tagged[1] = tag;
                let _ = decode(&tagged, codec());
            }
        }
    }
}

#[test]
fn snapshot_is_split_into_bounded_messages_without_losing_items() {
    let items: Vec<SnapItem> = (1..=1000)
        .map(|serial| SnapItem::Spawn(spawn_body(serial)))
        .collect();
    let chunks = snapshot_chunks(&items, codec()).unwrap();
    assert!(chunks.len() >= 2, "{} сообщений", chunks.len());
    let mut decoded = Vec::new();
    for chunk in &chunks {
        assert!(chunk.len() <= SNAPSHOT_CHUNK_BYTES, "{}", chunk.len());
        let Message::Snapshot(part) = decode(chunk, codec()).unwrap() else {
            panic!("ожидался фрагмент снимка");
        };
        decoded.extend(part);
    }
    assert_eq!(decoded.len(), items.len());
    for (got, want) in decoded.iter().zip(&items) {
        let (SnapItem::Spawn(got), SnapItem::Spawn(want)) = (got, want) else {
            panic!("ожидались корни");
        };
        assert_eq!(got.root, want.root);
        assert_eq!(got.id, want.id);
    }
}

#[test]
fn empty_snapshot_has_no_chunks() {
    assert!(snapshot_chunks(&[], codec()).unwrap().is_empty());
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

/// Угол между поворотами в градусах. Считается в f64: f32 не отличает ошибки меньше ~0,03°.
fn angle_deg(a: Quat, b: Quat) -> f64 {
    let wide = |q: Quat| {
        let [x, y, z, w] = q.to_array().map(f64::from);
        glam::DQuat::from_xyzw(x, y, z, w).normalize()
    };
    let (a, b) = (wide(a), wide(b));
    let sign = if a.dot(b) < 0.0 { -1.0 } else { 1.0 };
    let distance = (a - b * sign).length();
    (4.0 * (distance / 2.0).min(1.0).asin()).to_degrees()
}

#[test]
fn angle_metric_resolves_hundredths_of_a_degree() {
    let base = quat_from_yxz_degrees(Vec3::new(12.0, -40.0, 7.0));
    let shifted = base * Quat::from_rotation_y(0.02f32.to_radians());
    let measured = angle_deg(base, shifted);
    assert!((measured - 0.02).abs() < 0.002, "{measured}");
    assert!(angle_deg(base, base) < 1e-6);
}

#[test]
fn pose_lerp_takes_the_short_way_around() {
    // 350° и 10° вокруг X: кватернионы лежат по разные стороны от нуля, dot < 0. Короткий путь
    // проходит через 0°; длинный дал бы 180° между серединой и identity.
    let from = Pose {
        rotation: Quat::from_rotation_x(350.0f32.to_radians()),
        ..pose(0.0)
    };
    let to = Pose {
        rotation: Quat::from_rotation_x(10.0f32.to_radians()),
        ..pose(10.0)
    };
    assert!(
        from.rotation.dot(to.rotation) < 0.0,
        "тест должен покрывать отрицательный dot"
    );
    let halfway = from.lerp(to, 0.5);
    assert!(
        angle_deg(halfway.rotation, Quat::IDENTITY) < 1.0,
        "короткий путь"
    );
    assert!((halfway.position.x - 5.0).abs() < 1e-4);
}

#[test]
fn epochs_and_identity_fields_survive_the_wire() {
    let state = StateItem {
        object: NetId::spawned(4).unwrap(),
        epoch: 77,
        mask: MASK_POSITION,
        pose: pose(1.0),
    };
    let Message::States { items, .. } = quantized(&Message::States {
        tick: 1,
        items: vec![state],
    }) else {
        panic!("ожидался пакет состояний");
    };
    assert_eq!(items[0].epoch, 77);
    assert_eq!(items[0].object, NetId::spawned(4).unwrap());
    assert_eq!(items[0].mask, MASK_POSITION);

    let child = ChildBody {
        object: child_net_id(NetId::spawned(5).unwrap(), "lid"),
        owner: Some(4),
        mask: MASK_ROTATION,
        pose: pose(2.0),
    };
    let body = SpawnBody {
        epoch: 99,
        children: vec![child.clone()],
        ..spawn_body(5)
    };
    let Message::Spawn(decoded) = quantized(&Message::Spawn(body.clone())) else {
        panic!("ожидался Spawn");
    };
    assert_eq!(decoded.epoch, 99);
    assert_eq!(decoded.root, body.root);
    assert_eq!(decoded.prefab, body.prefab);
    assert_eq!(decoded.id, body.id);
    assert_eq!(decoded.owner, body.owner);
    assert_eq!(decoded.parent, body.parent);
    assert_eq!(decoded.children.len(), 1);
    assert_eq!(decoded.children[0].object, child.object);
    assert_eq!(decoded.children[0].owner, child.owner);
    assert_eq!(decoded.children[0].mask, child.mask);

    let snapshot = Message::Snapshot(vec![
        SnapItem::Object {
            object: NetId::scene(1).unwrap(),
            epoch: 88,
            owner: Some(1),
            mask: MASK_POSITION,
            pose: pose(3.0),
        },
        SnapItem::Spawn(SpawnBody {
            epoch: 99,
            ..spawn_body(6)
        }),
    ]);
    let Message::Snapshot(items) = quantized(&snapshot) else {
        panic!("ожидался снимок");
    };
    match &items[0] {
        SnapItem::Object {
            epoch, owner, mask, ..
        } => {
            assert_eq!(*epoch, 88);
            assert_eq!(*owner, Some(1));
            assert_eq!(*mask, MASK_POSITION);
        }
        other => panic!("ожидался объект: {other:?}"),
    }
    let SnapItem::Spawn(root) = &items[1] else {
        panic!("ожидался корень");
    };
    assert_eq!(root.epoch, 99);
}

#[test]
fn position_error_stays_within_half_a_step() {
    let step = 0.01f32;
    let codec = PoseCodec::new(step).unwrap();
    let values = [
        0.0f32, -0.0, 0.004, -0.004, 0.005, -0.005, -1.23456, 1000.5, -1000.5, -83886.06, 83886.06,
    ];
    for value in values {
        let item = StateItem {
            object: NetId::spawned(4).unwrap(),
            epoch: 0,
            mask: MASK_POSITION,
            pose: Pose {
                position: Vec3::new(value, -value, value * 0.5),
                ..Pose::default()
            },
        };
        let bytes = encode(
            &Message::States {
                tick: 1,
                items: vec![item],
            },
            codec,
        )
        .unwrap();
        let Message::States { items, .. } = decode(&bytes, codec).unwrap() else {
            panic!("ожидался пакет");
        };
        assert_eq!(items[0].mask, MASK_POSITION, "{value}");
        // Ошибка не больше половины шага плюс округление результата в f32 (до 2^-23 от модуля).
        for (got, want) in items[0]
            .pose
            .position
            .to_array()
            .into_iter()
            .zip(item.pose.position.to_array())
        {
            let slack = want.abs() * 1.2e-7;
            assert!(
                (got - want).abs() <= step / 2.0 + slack,
                "{want} -> {got} при шаге {step}"
            );
        }
    }
}

#[test]
fn out_of_range_position_is_sent_raw_and_exactly() {
    let codec = PoseCodec::new(0.01).unwrap();
    let far = Vec3::new(1.0e6, -2.5e6, 0.125);
    let item = StateItem {
        object: NetId::spawned(4).unwrap(),
        epoch: 0,
        mask: MASK_POSITION,
        pose: Pose {
            position: far,
            ..Pose::default()
        },
    };
    let bytes = encode(
        &Message::States {
            tick: 1,
            items: vec![item],
        },
        codec,
    )
    .unwrap();
    // Сырой формат: 12 байт позиции вместо 9, флаг в маске.
    assert_eq!(bytes.len(), 2 + 4 + 4 + 4 + 1 + 12);
    let Message::States { items, .. } = decode(&bytes, codec).unwrap() else {
        panic!("ожидался пакет");
    };
    assert_eq!(items[0].mask, MASK_POSITION);
    assert_eq!(items[0].pose.position, far);
}

#[test]
fn position_codec_rejects_steps_outside_the_range() {
    assert!(PoseCodec::new(0.01).is_ok());
    assert!(PoseCodec::new(1e-5).is_ok());
    assert!(PoseCodec::new(1.0).is_ok());
    for step in [0.0, 1e-6, 1.5, -0.01, f32::NAN, f32::INFINITY] {
        assert!(PoseCodec::new(step).is_err(), "{step}");
    }
}

#[test]
fn rotation_round_trips_within_a_hundredth_of_a_degree() {
    let cases = [
        Quat::IDENTITY,
        Quat::from_rotation_x(std::f32::consts::PI),
        Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        quat_from_yxz_degrees(Vec3::new(90.0, 0.0, 0.0)),
        quat_from_yxz_degrees(Vec3::new(-90.0, 0.0, 0.0)),
        quat_from_yxz_degrees(Vec3::new(-170.0, 33.0, 12.0)),
        Quat::from_xyzw(0.5, 0.5, 0.5, 0.5),
        Quat::from_xyzw(0.9, 0.1, 0.2, 0.3).normalize(),
        Quat::from_xyzw(0.1, 0.9, 0.2, 0.3).normalize(),
        Quat::from_xyzw(0.1, 0.2, 0.9, 0.3).normalize(),
        Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize(),
        Quat::from_xyzw(-0.2, 0.9, -0.1, 0.3).normalize(),
    ];
    for rotation in cases {
        let bits = rotation_bits(rotation).unwrap();
        let decoded = rotation_from_bits(bits).unwrap();
        assert!(
            (decoded.length() - 1.0).abs() < 1e-5,
            "не единичный: {decoded:?}"
        );
        let angle = angle_deg(decoded, rotation.normalize());
        assert!(angle <= 0.01, "{rotation:?}: ошибка {angle}°");
        // q и −q — один поворот: канонический знак даёт одинаковые биты.
        assert_eq!(rotation_bits(-rotation), Some(bits), "{rotation:?}");
    }
}

#[test]
fn rotation_bits_fit_in_47_bits_and_reject_garbage() {
    let bits = rotation_bits(quat_from_yxz_degrees(Vec3::new(12.0, 80.0, -40.0))).unwrap();
    assert!(bits >> 47 == 0);
    assert!(rotation_from_bits(1 << 47).is_none());
    // Любые 47 бит — допустимое значение: разбор не падает и даёт единичный кватернион.
    for seed in 0..64u64 {
        let value = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 17;
        let decoded = rotation_from_bits(value).unwrap();
        assert!((decoded.length() - 1.0).abs() < 1e-5);
    }
}

#[test]
fn rotation_key_is_stable_across_equal_components() {
    // Все компоненты равны по модулю: индекс выбирается детерминированно, повторный вызов даёт то же.
    let q = Quat::from_xyzw(0.5, -0.5, 0.5, -0.5);
    assert_eq!(rotation_bits(q), rotation_bits(q));
    assert_eq!(rotation_bits(q), rotation_bits(-q));
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
