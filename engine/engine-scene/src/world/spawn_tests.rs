use super::{
    LightState, LocalLightState, LocalTransform, MAX_GENERATION, MAX_LIVE_OBJECTS, ObjectHandle,
    SpawnOptions, World, document_handle,
};
use crate::document::{SceneDocument, SceneError};
use crate::prefab::PrefabTemplate;
use engine_files::VirtualPath;
use glam::{Mat4, Quat, Vec3};

const CAMERA_SCENE: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

/// Корень с ребёнком и внуком; дети без собственного transform стоят в начале координат.
const BULLET: &str = "[[objects]]\nid = \"root\"\n[objects.transform]\nposition = [1.0, 0.0, 0.0]\n\
[[objects]]\nid = \"child\"\nparent = \"root\"\n[objects.transform]\nposition = [0.0, 2.0, 0.0]\n\
[[objects]]\nid = \"leaf\"\nparent = \"child\"\n";

const LONE: &str = "[[objects]]\nid = \"root\"\n";

const LOD_BULLET: &str = "[[assets]]\npath = \"/base.fbx\"\n[[assets]]\npath = \"/lod.fbx\"\n\
[[objects]]\nid = \"root\"\n[objects.transform]\nposition = [10.0, 0.0, 0.0]\nrotation = [0.0, 90.0, 0.0]\n\
[objects.mesh_renderer]\nmodel = \"/base.fbx\"\n[objects.lod_group]\n\
[[objects.lod_group.levels]]\nmodel = \"/lod.fbx\"\ndistance = 11.0\n\
[[objects]]\nid = \"child\"\nparent = \"root\"\n[objects.transform]\nposition = [0.0, 0.0, 5.0]\n";

fn world(scene: &str) -> World {
    World::from_document(&SceneDocument::parse("spawn.rvescene", scene).unwrap()).unwrap()
}

fn template(prefab: &str) -> PrefabTemplate {
    let path = VirtualPath::parse("prefabs/bullet.rveprefab").unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(prefab.to_owned()) };
    PrefabTemplate::load(&path, &mut read).unwrap()
}

/// Резервирует по хендлу на объект шаблона и создаёт экземпляр.
fn spawn(
    world: &mut World,
    template: &PrefabTemplate,
    options: SpawnOptions,
) -> Result<Vec<ObjectHandle>, SceneError> {
    let reserved = (0..template.objects.len())
        .map(|_| world.reserve_object())
        .collect();
    world.spawn(reserved, template, options)
}

fn ids(world: &World) -> Vec<String> {
    world
        .objects()
        .iter()
        .map(|object| object.id.clone())
        .collect()
}

#[test]
fn spawn_appends_subtrees_and_despawn_removes_them_children_first() {
    let mut world = world(CAMERA_SCENE);
    let bullet = template(BULLET);
    let first = spawn(&mut world, &bullet, SpawnOptions::default()).unwrap();
    let second = spawn(&mut world, &bullet, SpawnOptions::default()).unwrap();
    assert_eq!(
        ids(&world),
        [
            "camera",
            "bullet@0",
            "bullet@0.child",
            "bullet@0.leaf",
            "bullet@1",
            "bullet@1.child",
            "bullet@1.leaf",
        ]
    );

    // Хендлы поддерева и дубликаты внутри него удаляются один раз, дети идут раньше родителей.
    let removed = world
        .despawn_many(&[first[0], first[1]])
        .expect("поддерево удаляется");
    let removed_ids = removed
        .iter()
        .map(|&handle| world.object(handle).map(|object| object.id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(removed, [first[2], first[1], first[0]]);
    assert!(removed_ids.iter().all(Option::is_none));
    assert_eq!(
        ids(&world),
        ["camera", "bullet@1", "bullet@1.child", "bullet@1.leaf"]
    );
    assert!(second.iter().all(|&handle| world.is_alive(handle)));
}

#[test]
fn stale_handle_stays_dead_after_its_slot_is_reused() {
    let mut world = world(CAMERA_SCENE);
    let lone = template(LONE);
    let old = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    world.despawn_many(&[old]).unwrap();

    let new = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_eq!(new.index(), old.index());
    assert_eq!(new.generation(), old.generation() + 1);
    assert!(world.is_alive(new));
    assert!(!world.is_alive(old));
    assert!(world.object(old).is_none());
    assert!(world.world_matrix(old).is_none());
    assert!(
        world
            .set_local_transform(old, LocalTransform::default())
            .is_err()
    );
    assert!(world.despawn_many(&[old]).is_err());
    assert_eq!(world.objects().len(), 2);
}

#[test]
fn failed_spawn_releases_its_reservations_and_retry_succeeds() {
    let mut world = world(CAMERA_SCENE);
    let lone = template(LONE);
    let reserved = world.reserve_object();
    let error = world
        .spawn(
            vec![reserved],
            &lone,
            SpawnOptions {
                id: Some("camera".to_owned()),
                ..SpawnOptions::default()
            },
        )
        .unwrap_err();
    assert!(error.message.contains("уже используется"));
    assert!(!world.is_reserved(reserved));
    assert!(!world.is_alive(reserved));
    assert_eq!(world.objects().len(), 1);

    let retry = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_eq!(retry.index(), reserved.index());
    assert_ne!(retry, reserved);
    assert!(world.is_alive(retry));
}

#[test]
fn release_reservation_is_reported_once_and_bumps_the_generation() {
    let world = world(CAMERA_SCENE);
    let handle = world.reserve_object();
    assert!(world.is_reserved(handle));
    assert!(!world.is_alive(handle));
    assert!(world.release_reservation(handle));
    assert!(!world.release_reservation(handle));
    assert!(!world.is_reserved(handle));
    let again = world.reserve_object();
    assert_eq!(again.index(), handle.index());
    assert_eq!(again.generation(), handle.generation() + 1);
}

#[test]
fn exhausted_generation_is_never_reused() {
    let mut world = world(CAMERA_SCENE);
    let lone = template(LONE);
    let first = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    world.despawn_many(&[first]).unwrap();
    let index = first.index();
    // Слот уже в списке свободных, поэтому поколение можно задать напрямую перед переиспользованием.
    world.slots.borrow_mut().slots[index].generation = MAX_GENERATION - 1;

    let near_limit = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_eq!(near_limit.generation(), MAX_GENERATION - 1);
    world.despawn_many(&[near_limit]).unwrap();

    let last = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_eq!(last.index(), index);
    assert_eq!(last.generation(), MAX_GENERATION);
    assert_eq!(ObjectHandle::from_abi(last.to_abi().unwrap()), Some(last));
    world.despawn_many(&[last]).unwrap();

    let after = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_ne!(after.index(), index);
    assert_eq!(after.generation(), 0);
    assert!(!world.is_alive(last));
}

#[test]
fn abi_handle_encodes_generation_and_index_and_rejects_invalid_values() {
    let handle = ObjectHandle {
        index: 4,
        generation: 3,
    };
    assert_eq!(handle.to_abi(), Some((3 << 32) | 5));
    assert_eq!(ObjectHandle::from_abi((3 << 32) | 5), Some(handle));
    assert_eq!(document_handle(0).to_abi(), Some(1));
    assert_eq!(ObjectHandle::from_abi(1), Some(document_handle(0)));
    assert_eq!(ObjectHandle::from_abi(0), None);
    assert_eq!(ObjectHandle::from_abi(1 << 63 | 1), None);
    let unencodable = ObjectHandle {
        index: u32::MAX,
        generation: 0,
    };
    assert_eq!(unencodable.to_abi(), None);
}

#[test]
fn spawn_rejects_dynamic_root_under_a_parent_and_releases_reservations() {
    let scene = format!("{CAMERA_SCENE}\n[[objects]]\nid = \"rig\"\n");
    let mut world = world(&scene);
    let body = template("[[objects]]\nid = \"body\"\n[objects.collider]\n[objects.rigid_body]\n");
    let rig = world.handle("rig").unwrap();
    let reserved = vec![world.reserve_object()];
    let stale = reserved[0];
    let error = world
        .spawn(
            reserved,
            &body,
            SpawnOptions {
                parent: Some(rig),
                ..SpawnOptions::default()
            },
        )
        .unwrap_err();
    assert!(error.message.contains("без родителя"));
    assert!(!world.is_reserved(stale));

    let free = spawn(&mut world, &body, SpawnOptions::default()).unwrap();
    assert!(world.is_alive(free[0]));
}

#[test]
fn spawn_refuses_a_template_with_a_camera_and_releases_its_reservations() {
    let mut world = world(CAMERA_SCENE);
    let eye = template(
        "[[objects]]\nid = \"root\"\n[[objects]]\nid = \"eye\"\nparent = \"root\"\n[objects.camera]\n",
    );
    let reserved = vec![world.reserve_object(), world.reserve_object()];
    let stale = reserved.clone();
    let error = world
        .spawn(reserved, &eye, SpawnOptions::default())
        .unwrap_err();
    assert!(error.message.contains("camera"), "{error}");
    assert_eq!(error.object_id.as_deref(), Some("eye"));
    assert!(stale.iter().all(|&handle| !world.is_reserved(handle)));
    assert_eq!(world.objects().len(), 1);
}

#[test]
fn spawn_rejects_a_missing_parent_and_a_wrong_reservation_count() {
    let mut world = world(CAMERA_SCENE);
    let bullet = template(BULLET);
    let dead = ObjectHandle {
        index: 7,
        generation: 0,
    };
    let error = spawn(
        &mut world,
        &bullet,
        SpawnOptions {
            parent: Some(dead),
            ..SpawnOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.message.contains("родитель"));

    let short = vec![world.reserve_object()];
    let error = world
        .spawn(short, &bullet, SpawnOptions::default())
        .unwrap_err();
    assert!(error.message.contains("резервов"));
    assert_eq!(world.objects().len(), 1);
}

#[test]
fn despawn_refuses_the_active_camera_and_its_ancestors_without_changes() {
    let scene = "[[objects]]\nid = \"rig\"\n[[objects]]\nid = \"camera\"\nparent = \"rig\"\n[objects.camera]\n";
    let mut world = world(scene);
    let rig = world.handle("rig").unwrap();
    let camera = world.handle("camera").unwrap();
    let error = world.despawn_many(&[rig]).unwrap_err();
    assert!(error.message.contains("активную камеру"));
    assert!(world.despawn_many(&[camera]).is_err());
    assert_eq!(world.objects().len(), 2);
    assert!(world.is_alive(rig) && world.is_alive(camera));
}

#[test]
fn spawned_objects_respect_the_live_object_limit() {
    let mut scene = String::from(CAMERA_SCENE);
    for index in 0..MAX_LIVE_OBJECTS - 2 {
        scene.push_str(&format!("[[objects]]\nid = \"o{index}\"\n"));
    }
    let mut world = world(&scene);
    let lone = template(LONE);
    spawn(&mut world, &lone, SpawnOptions::default()).expect("последний слот под лимитом");
    assert_eq!(world.objects().len(), MAX_LIVE_OBJECTS);
    let error = spawn(&mut world, &lone, SpawnOptions::default()).unwrap_err();
    assert!(error.message.contains("лимит"));
    assert_eq!(world.objects().len(), MAX_LIVE_OBJECTS);
}

#[test]
fn spawn_applies_transform_overrides_and_composes_children_and_lod() {
    let mut world = world(CAMERA_SCENE);
    let lod = template(LOD_BULLET);
    let spawned = spawn(
        &mut world,
        &lod,
        SpawnOptions {
            position: Some(Vec3::new(20.0, 0.0, 0.0)),
            ..SpawnOptions::default()
        },
    )
    .unwrap();
    let (root, child) = (spawned[0], spawned[1]);

    let root_local = world.local_transform(root).unwrap().matrix();
    assert!(
        world
            .world_matrix(root)
            .unwrap()
            .abs_diff_eq(root_local, 1e-5)
    );
    let child_local = world.local_transform(child).unwrap().matrix();
    let expected_child = root_local * child_local;
    assert!(
        world
            .world_matrix(child)
            .unwrap()
            .abs_diff_eq(expected_child, 1e-5)
    );
    // Ребёнок (0, 0, 5) поворачивается на 90° вокруг Y и сдвигается корнем на (20, 0, 0).
    assert!(
        world
            .world_matrix(child)
            .unwrap()
            .transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(25.0, 0.0, 0.0), 1e-4)
    );

    assert_eq!(world.lod_level(root, Vec3::ZERO), Some(1));
    assert_eq!(world.lod_level(root, Vec3::new(12.0, 0.0, 0.0)), Some(0));
}

#[test]
fn spawn_under_a_parent_composes_with_the_parent_matrix() {
    let scene = format!(
        "{CAMERA_SCENE}\n[[objects]]\nid = \"rig\"\n[objects.transform]\nposition = [0.0, 0.0, 100.0]\n"
    );
    let mut world = world(&scene);
    let rig = world.handle("rig").unwrap();
    let spawned = spawn(
        &mut world,
        &template(BULLET),
        SpawnOptions {
            parent: Some(rig),
            ..SpawnOptions::default()
        },
    )
    .unwrap();
    let root_matrix = world.world_matrix(spawned[0]).unwrap();
    assert!(
        root_matrix
            .transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(1.0, 0.0, 100.0), 1e-5)
    );
    let leaf = world.world_matrix(spawned[2]).unwrap();
    assert!(
        leaf.transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(1.0, 2.0, 100.0), 1e-5)
    );
    assert!(
        world
            .world_matrix(rig)
            .unwrap()
            .abs_diff_eq(Mat4::from_translation(Vec3::new(0.0, 0.0, 100.0)), 1e-6)
    );
}

#[test]
fn explicit_id_and_name_are_used_for_the_root_and_prefixed_for_children() {
    let mut world = world(CAMERA_SCENE);
    spawn(
        &mut world,
        &template(BULLET),
        SpawnOptions {
            id: Some("gun".to_owned()),
            name: Some("Pistol".to_owned()),
            ..SpawnOptions::default()
        },
    )
    .unwrap();
    assert_eq!(world.object_by_id("gun").unwrap().name, "Pistol");
    assert!(world.handle("gun.child").is_some());
    assert!(world.handle("gun.leaf").is_some());
    assert_eq!(world.object_by_id("gun.child").unwrap().name, "gun.child");
}

#[test]
fn removing_a_directional_light_restores_default_light_state() {
    let scene = format!("{CAMERA_SCENE}\n[[objects]]\nid = \"sun\"\n[objects.directional_light]\n");
    let mut world = world(&scene);
    let sun = world.handle("sun").unwrap();
    let lit = world.build_frame().0.light;
    assert_eq!(lit.color, [1.0; 3]);
    world.despawn_many(&[sun]).unwrap();
    assert_eq!(world.build_frame().0.light, LightState::default());
}

#[test]
fn despawn_keeps_surviving_transforms_and_camera_frame_consistent() {
    let scene = format!(
        "{CAMERA_SCENE}\n[[objects]]\nid = \"rig\"\n[objects.transform]\nposition = [0.0, 3.0, 0.0]\n[[objects]]\nid = \"mover\"\n[objects.transform]\nposition = [4.0, 0.0, 0.0]\n[[objects]]\nid = \"child\"\nparent = \"rig\"\n"
    );
    let mut world = world(&scene);
    let rig = world.handle("rig").unwrap();
    let mover = world.handle("mover").unwrap();
    let child = world.handle("child").unwrap();
    world.despawn_many(&[rig]).unwrap();
    assert!(!world.is_alive(child));
    assert!(
        world
            .world_matrix(mover)
            .unwrap()
            .abs_diff_eq(Mat4::from_translation(Vec3::new(4.0, 0.0, 0.0)), 1e-6)
    );
    assert_eq!(world.objects().len(), 2);
}

/// Шаблон из набора префабов в памяти; ключи — виртуальные пути без ведущего слеша.
fn template_from(files: &[(&str, &str)], path: &str) -> PrefabTemplate {
    let path = VirtualPath::parse(path).unwrap();
    let mut read = |child: &VirtualPath| -> Result<String, String> {
        files
            .iter()
            .find(|(name, _)| *name == child.as_str())
            .map(|(_, text)| (*text).to_owned())
            .ok_or_else(|| "не найден".to_owned())
    };
    PrefabTemplate::load(&path, &mut read).unwrap()
}

#[test]
fn recalculation_after_an_earlier_despawn_keeps_child_matrix_and_rotation_consistent() {
    let scene = format!(
        "{CAMERA_SCENE}\n[[objects]]\nid = \"gone\"\n[[objects]]\nid = \"rig\"\n[objects.transform]\nposition = [1.0, 0.0, 0.0]\n[[objects]]\nid = \"kid\"\nparent = \"rig\"\n[objects.transform]\nposition = [0.0, 2.0, 0.0]\nrotation = [0.0, 0.0, 30.0]\n"
    );
    let mut world = world(&scene);
    let gone = world.handle("gone").unwrap();
    let rig = world.handle("rig").unwrap();
    let kid = world.handle("kid").unwrap();
    world.despawn_many(&[gone]).unwrap();

    world
        .set_local_transform(
            rig,
            LocalTransform {
                position: Vec3::new(5.0, 0.0, 0.0),
                rotation_deg: Vec3::new(0.0, 90.0, 0.0),
                scale: Vec3::ONE,
            },
        )
        .unwrap();
    world.recalculate_world_matrices();

    let rig_matrix = world.world_matrix(rig).unwrap();
    let kid_matrix = world.world_matrix(kid).unwrap();
    let kid_local = world.local_transform(kid).unwrap().matrix();
    assert!(kid_matrix.abs_diff_eq(rig_matrix * kid_local, 1e-5));
    // Точка (0, 2, 0) у ребёнка при повороте родителя на 90° вокруг Y и сдвиге (5, 0, 0).
    assert!(
        kid_matrix
            .transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(5.0, 2.0, 0.0), 1e-5)
    );
    // Кэш вращения совпадает с вращением, выведенным из мировой матрицы (масштаб единичный).
    let expected = Quat::from_mat4(&kid_matrix);
    let cached = world.world_rotation(kid).unwrap();
    assert!(
        cached.dot(expected).abs() > 1.0 - 1e-5,
        "{cached:?} vs {expected:?}"
    );
}

#[test]
fn spawn_under_a_parent_after_slot_reuse_keeps_matrices_on_dense_indices() {
    let scene = format!(
        "{CAMERA_SCENE}\n[[objects]]\nid = \"rig\"\n[objects.transform]\nposition = [0.0, 0.0, 100.0]\n"
    );
    let mut world = world(&scene);
    let rig = world.handle("rig").unwrap();
    let lone = template(LONE);
    let first = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    let kept = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    world.despawn_many(&[first]).unwrap();

    // Освобождённый слот переиспользуется, поэтому плотный индекс корня не совпадает со слотом.
    let spawned = spawn(
        &mut world,
        &template(BULLET),
        SpawnOptions {
            parent: Some(rig),
            ..SpawnOptions::default()
        },
    )
    .unwrap();
    assert_ne!(world.dense(spawned[0]), Some(spawned[0].index()));
    assert!(world.is_alive(kept));

    let root = world.world_matrix(spawned[0]).unwrap();
    assert!(
        root.transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(1.0, 0.0, 100.0), 1e-5)
    );
    // Ребёнок (0, 2, 0) и внук в начале его координат лежат под корнем и родителем.
    let child = world.world_matrix(spawned[1]).unwrap();
    assert!(
        child
            .transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(1.0, 2.0, 100.0), 1e-5)
    );
    let leaf = world.world_matrix(spawned[2]).unwrap();
    assert!(
        leaf.transform_point3(Vec3::ZERO)
            .abs_diff_eq(Vec3::new(1.0, 2.0, 100.0), 1e-5)
    );
    assert!(
        world
            .world_matrix(rig)
            .unwrap()
            .abs_diff_eq(Mat4::from_translation(Vec3::new(0.0, 0.0, 100.0)), 1e-6)
    );
    assert!(
        world
            .world_matrix(kept)
            .unwrap()
            .abs_diff_eq(Mat4::IDENTITY, 1e-6)
    );
}

#[test]
fn surviving_local_light_keeps_its_parameters_after_an_earlier_light_is_despawned() {
    let scene = format!(
        "{CAMERA_SCENE}\n[[objects]]\nid = \"gone\"\n[objects.transform]\nposition = [9.0, 0.0, 0.0]\n[objects.point_light]\nintensity = 50.0\n[[objects]]\nid = \"lamp\"\n[objects.transform]\nposition = [2.0, 1.0, 0.0]\n[objects.point_light]\nintensity = 3.0\nrange = 12.0\n"
    );
    let mut world = world(&scene);
    let gone = world.handle("gone").unwrap();
    let lamp = world.handle("lamp").unwrap();
    let before = world.build_frame().0.local_lights;
    assert_eq!(before.len(), 2);

    world.despawn_many(&[gone]).unwrap();
    // Невалидная позиция выжившей лампы: кадр должен взять её последнее валидное состояние,
    // а не состояние удалённого объекта, которое раньше стояло по тому же плотному индексу.
    world
        .set_local_transform(
            lamp,
            LocalTransform {
                position: Vec3::new(f32::NAN, 1.0, 0.0),
                ..LocalTransform::default()
            },
        )
        .unwrap();
    let (frame, warnings) = world.build_frame();
    let [
        LocalLightState::Point {
            position,
            intensity,
            range,
            ..
        },
    ] = frame.local_lights.as_slice()
    else {
        panic!("ожидалась одна точечная лампа: {:?}", frame.local_lights);
    };
    assert_eq!(*position, [2.0, 1.0, 0.0]);
    assert_eq!(*intensity, 3.0);
    assert_eq!(*range, 12.0);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].object_id, "lamp");
}

#[test]
fn reservations_are_distinct_and_only_held_reservations_can_be_released() {
    let world = world(CAMERA_SCENE);
    assert!(!world.release_reservation(document_handle(0)));
    assert!(!world.release_reservation(ObjectHandle {
        index: 99,
        generation: 0,
    }));

    let first = world.reserve_object();
    let second = world.reserve_object();
    assert_ne!(first, second);
    assert!(world.is_reserved(first) && world.is_reserved(second));

    assert!(world.release_reservation(first));
    assert!(!world.release_reservation(first));
    let reused = world.reserve_object();
    assert_eq!(reused.index(), first.index());
    assert_ne!(reused, first);
    assert!(world.is_reserved(reused) && !world.is_reserved(first));
}

#[test]
fn invalid_explicit_ids_are_rejected_and_release_their_reservations() {
    let mut world = world(CAMERA_SCENE);
    let lone = template(LONE);
    let too_long = "a".repeat(65);
    for bad in ["", "a.b", "a@b", "with space", too_long.as_str()] {
        let reserved = world.reserve_object();
        let error = world
            .spawn(
                vec![reserved],
                &lone,
                SpawnOptions {
                    id: Some(bad.to_owned()),
                    ..SpawnOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.message.contains("[A-Za-z0-9_-]{1,64}"), "{error}");
        assert!(
            !world.is_reserved(reserved),
            "резерв не освобождён для {bad:?}"
        );
    }
    assert_eq!(world.objects().len(), 1);

    let edge = "b".repeat(64);
    let spawned = spawn(
        &mut world,
        &lone,
        SpawnOptions {
            id: Some(edge.clone()),
            ..SpawnOptions::default()
        },
    )
    .expect("64 символа допустимы");
    assert_eq!(world.object(spawned[0]).unwrap().id, edge);
}

#[test]
fn explicit_id_colliding_with_a_live_object_is_rejected_and_leaves_it_untouched() {
    let mut world = world(CAMERA_SCENE);
    let lone = template(LONE);
    let gun = spawn(
        &mut world,
        &lone,
        SpawnOptions {
            id: Some("gun".to_owned()),
            ..SpawnOptions::default()
        },
    )
    .unwrap()[0];
    let error = spawn(
        &mut world,
        &lone,
        SpawnOptions {
            id: Some("gun".to_owned()),
            ..SpawnOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.message.contains("уже используется"), "{error}");
    assert_eq!(world.handle("gun"), Some(gun));
    assert_eq!(world.objects().len(), 2);
}

#[test]
fn expanded_child_ids_longer_than_the_limit_are_rejected() {
    let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
    let l2 = format!("[[objects]]\nid = \"r2\"\n[[objects]]\nid = \"{c}\"\nparent = \"r2\"\n");
    let l1 = format!(
        "[[assets]]\npath = \"/prefabs/l2.rveprefab\"\n[[objects]]\nid = \"r1\"\n[[instances]]\nid = \"{b}\"\nprefab = \"/prefabs/l2.rveprefab\"\nparent = \"r1\"\n"
    );
    let top = format!(
        "[[assets]]\npath = \"/prefabs/l1.rveprefab\"\n[[objects]]\nid = \"top\"\n[[instances]]\nid = \"{a}\"\nprefab = \"/prefabs/l1.rveprefab\"\nparent = \"top\"\n"
    );
    // Дети шаблона: `a.b.c` (194 байта). Корень `x` даёт `x.a.b.c` (196 байт), корень на 64 символа — 259.
    let nested = template_from(
        &[
            ("prefabs/top.rveprefab", top.as_str()),
            ("prefabs/l1.rveprefab", l1.as_str()),
            ("prefabs/l2.rveprefab", l2.as_str()),
        ],
        "prefabs/top.rveprefab",
    );
    let mut world = world(CAMERA_SCENE);
    let short = spawn(
        &mut world,
        &nested,
        SpawnOptions {
            id: Some("x".to_owned()),
            ..SpawnOptions::default()
        },
    )
    .expect("196 байт укладываются в лимит");
    assert_eq!(short.len(), 4);

    let reserved = (0..nested.objects.len())
        .map(|_| world.reserve_object())
        .collect::<Vec<_>>();
    let error = world
        .spawn(
            reserved.clone(),
            &nested,
            SpawnOptions {
                id: Some("r".repeat(64)),
                ..SpawnOptions::default()
            },
        )
        .unwrap_err();
    assert!(error.message.contains("255"), "{error}");
    assert!(reserved.iter().all(|&handle| !world.is_reserved(handle)));
    assert_eq!(world.objects().len(), 5);
}

#[test]
fn automatic_ids_skip_serials_that_a_live_object_already_holds() {
    let mut world = world(CAMERA_SCENE);
    let lone = template(LONE);
    let first = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_eq!(world.object(first).unwrap().id, "bullet@0");

    // Счётчик отстаёт от живых объектов: номер 0 занят, автоидентификатор должен его пропустить.
    world.spawn_serial = 0;
    let second = spawn(&mut world, &lone, SpawnOptions::default()).unwrap()[0];
    assert_eq!(world.object(second).unwrap().id, "bullet@1");
    assert_eq!(world.objects().len(), 3);

    world.spawn_serial = 0;
    let bullet = spawn(&mut world, &template(BULLET), SpawnOptions::default()).unwrap();
    assert_eq!(world.object(bullet[0]).unwrap().id, "bullet@2");
    assert_eq!(world.object(bullet[1]).unwrap().id, "bullet@2.child");
}

#[test]
fn subtree_lists_roots_and_descendants_parents_first_and_skips_dead_handles() {
    let mut world = world(CAMERA_SCENE);
    let bullet = template(BULLET);
    let first = spawn(&mut world, &bullet, SpawnOptions::default()).unwrap();
    let second = spawn(&mut world, &bullet, SpawnOptions::default()).unwrap();
    world.despawn_many(&[second[2]]).unwrap();

    // Корень первого поддерева захватывает детей и внука; второй корень не задет.
    assert_eq!(
        ids_of(&world, &world.subtree(&[first[0]])),
        ["bullet@0", "bullet@0.child", "bullet@0.leaf"]
    );
    // Потомок в списке корней не дублируется; удалённый и чужой хендлы пропускаются.
    assert_eq!(
        ids_of(&world, &world.subtree(&[first[2], first[0], second[0]])),
        [
            "bullet@0",
            "bullet@0.child",
            "bullet@0.leaf",
            "bullet@1",
            "bullet@1.child"
        ]
    );
    assert!(world.subtree(&[second[2]]).is_empty());
}

fn ids_of(world: &World, handles: &[ObjectHandle]) -> Vec<String> {
    handles
        .iter()
        .map(|&handle| world.object(handle).unwrap().id.clone())
        .collect()
}

const EMITTER_BULLET: &str = "[[objects]]\nid = \"root\"\n[objects.transform]\nposition = [1.0, 0.0, 0.0]\n\
[objects.particle_emitter]\ncapacity = 64\nrate = 5.0\n\
[[objects]]\nid = \"plain\"\nparent = \"root\"\n";

#[test]
fn spawned_emitters_join_the_frame_and_despawn_removes_them() {
    let mut world = world(CAMERA_SCENE);
    let bullet = template(EMITTER_BULLET);
    let first = spawn(&mut world, &bullet, SpawnOptions::default()).unwrap();

    let (frame, _) = world.build_frame();
    assert_eq!(frame.particle_emitters.len(), 1);
    assert_eq!(frame.particle_emitters[0].object, first[0]);
    assert_eq!(world.particle_emitter(first[0]).unwrap().capacity, 64);
    let translation = frame.particle_emitters[0].model.w_axis.truncate();
    assert!((translation - Vec3::new(1.0, 0.0, 0.0)).length() < 1e-5);

    world.despawn_many(&[first[0]]).unwrap();
    let (frame, _) = world.build_frame();
    assert!(frame.particle_emitters.is_empty());
    assert!(world.particle_emitter(first[0]).is_none());

    let second = spawn(&mut world, &bullet, SpawnOptions::default()).unwrap();
    let (frame, _) = world.build_frame();
    assert_eq!(frame.particle_emitters.len(), 1);
    assert_eq!(frame.particle_emitters[0].object, second[0]);
    assert_ne!(
        second[0], first[0],
        "переиспользованный слот получает новое поколение"
    );
}
