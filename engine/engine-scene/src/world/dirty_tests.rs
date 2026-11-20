//! Метки грязных мировых матриц: кеш после пересчёта равен полному пересчёту побитно, а
//! пересчитывается только изменённое поддерево.

use super::{LocalTransform, ObjectHandle, SpawnOptions, World, take_transform_updates};
use crate::document::SceneDocument;
use crate::prefab::PrefabTemplate;
use engine_files::VirtualPath;
use glam::{Mat4, Quat, Vec3};
use std::fmt::Write as _;

const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

const BULLET: &str = "[[objects]]\nid = \"root\"\n[objects.transform]\nposition = [1.0, 0.0, 0.0]\n\
[[objects]]\nid = \"child\"\nparent = \"root\"\n[objects.transform]\nposition = [0.0, 2.0, 0.0]\n\
[[objects]]\nid = \"leaf\"\nparent = \"child\"\n";

/// Два корня: `root` с поддеревом `a`/`a1` и `b`, и отдельный `other` с потомком `o1`.
const TREE: &str = concat!(
    "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
    "[[objects]]\nid = \"root\"\n[objects.transform]\nposition = [1.0, 0.0, 0.0]\n",
    "[[objects]]\nid = \"a\"\nparent = \"root\"\n[objects.transform]\nposition = [0.0, 1.0, 0.0]\n",
    "[[objects]]\nid = \"a1\"\nparent = \"a\"\n[objects.transform]\nposition = [0.0, 0.0, 1.0]\n",
    "[[objects]]\nid = \"b\"\nparent = \"root\"\n[objects.transform]\nposition = [0.0, 0.0, 2.0]\n",
    "[[objects]]\nid = \"other\"\n[objects.transform]\nposition = [5.0, 0.0, 0.0]\n",
    "[[objects]]\nid = \"o1\"\nparent = \"other\"\n[objects.transform]\nposition = [0.0, 3.0, 0.0]\n",
);

const PERSISTENT_ROOT: &str = concat!(
    "[[objects]]\nid = \"keep\"\nkeep_on_load = true\n",
    "[[objects]]\nid = \"kid\"\nparent = \"keep\"\n",
);

fn world(scene: &str) -> World {
    World::from_document(&SceneDocument::parse("dirty.rvescene", scene).unwrap()).unwrap()
}

fn template(prefab: &str) -> PrefabTemplate {
    let path = VirtualPath::parse("prefabs/bullet.rveprefab").unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(prefab.to_owned()) };
    PrefabTemplate::load(&path, &mut read).unwrap()
}

fn spawn(world: &mut World, template: &PrefabTemplate, options: SpawnOptions) -> Vec<ObjectHandle> {
    let reserved = (0..template.objects.len())
        .map(|_| world.reserve_object())
        .collect();
    world.spawn(reserved, template, options).unwrap()
}

fn handle(world: &World, id: &str) -> ObjectHandle {
    world
        .handle(id)
        .unwrap_or_else(|| panic!("no live object {id}"))
}

fn moved(position: Vec3) -> LocalTransform {
    LocalTransform {
        position,
        ..LocalTransform::default()
    }
}

fn matrix_bits(matrix: Mat4) -> [u32; 16] {
    matrix.to_cols_array().map(f32::to_bits)
}

fn rotation_bits(rotation: Quat) -> [u32; 4] {
    rotation.to_array().map(f32::to_bits)
}

/// Эталон: полный безусловный пересчёт копии мира. Кеш `world` должен совпадать с ним побитно.
fn assert_cache_matches_full_recalc(world: &World, context: &str) {
    let mut expected = world.clone();
    expected.recalculate_all_for_test();
    assert_eq!(
        world.world_matrices.len(),
        expected.world_matrices.len(),
        "{context}: число объектов"
    );
    for (index, (actual, wanted)) in world
        .world_matrices
        .iter()
        .zip(&expected.world_matrices)
        .enumerate()
    {
        assert_eq!(
            matrix_bits(*actual),
            matrix_bits(*wanted),
            "{context}: матрица плотного индекса {index}"
        );
    }
    for (index, (actual, wanted)) in world
        .world_rotations
        .iter()
        .zip(&expected.world_rotations)
        .enumerate()
    {
        assert_eq!(
            rotation_bits(*actual),
            rotation_bits(*wanted),
            "{context}: вращение плотного индекса {index}"
        );
    }
}

/// Детерминированный LCG без новой зависимости (по образцу `engine-physics/src/broad.rs`).
struct Lcg(u64);

impl Lcg {
    fn next_bits(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_bits() % bound as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn range(&mut self, low: f32, high: f32) -> f32 {
        let unit = (self.next_bits() & 0xFF_FFFF) as f32 / (1u32 << 24) as f32;
        low + (high - low) * unit
    }

    fn point(&mut self, half_extent: f32) -> Vec3 {
        Vec3::new(
            self.range(-half_extent, half_extent),
            self.range(-half_extent, half_extent),
            self.range(-half_extent, half_extent),
        )
    }

    fn transform(&mut self) -> LocalTransform {
        LocalTransform {
            position: self.point(10.0),
            rotation_deg: self.point(180.0),
            // Масштаб положительный: обратные матрицы родителей остаются конечными.
            scale: Vec3::new(
                self.range(0.5, 1.5),
                self.range(0.5, 1.5),
                self.range(0.5, 1.5),
            ),
        }
    }
}

/// Случайный лес из `count` объектов под камерой; потомок всегда указывает на более ранний объект.
fn random_scene(rng: &mut Lcg, count: usize) -> String {
    let mut text = String::from(CAMERA);
    for index in 0..count {
        let _ = write!(text, "[[objects]]\nid = \"n{index}\"\n");
        if index > 0 && rng.chance(70) {
            let _ = writeln!(text, "parent = \"n{}\"", rng.below(index));
        }
        let transform = rng.transform();
        let _ = writeln!(
            text,
            "[objects.transform]\nposition = [{:.4}, {:.4}, {:.4}]\nrotation = [{:.4}, {:.4}, {:.4}]\nscale = [{:.4}, {:.4}, {:.4}]",
            transform.position.x,
            transform.position.y,
            transform.position.z,
            transform.rotation_deg.x,
            transform.rotation_deg.y,
            transform.rotation_deg.z,
            transform.scale.x,
            transform.scale.y,
            transform.scale.z,
        );
    }
    text
}

#[test]
fn randomized_mutations_keep_cache_equal_to_full_recalc() {
    let bullet = template(BULLET);
    for seed in [1_u64, 7, 42, 2026] {
        let mut rng = Lcg(seed);
        let mut world = world(&random_scene(&mut rng, 40));
        for step in 0..300 {
            let live: Vec<ObjectHandle> =
                world.objects().iter().map(|object| object.handle).collect();
            let camera = handle(&world, "camera");
            let movable: Vec<ObjectHandle> = live
                .iter()
                .copied()
                .filter(|&object| object != camera)
                .collect();
            let operation = if movable.is_empty() { 5 } else { rng.below(9) };
            let context = format!("seed {seed}, шаг {step}, операция {operation}");
            match operation {
                0 => {
                    let object = movable[rng.below(movable.len())];
                    world
                        .set_local_transform(object, rng.transform())
                        .expect(&context);
                }
                1 => {
                    let object = live[rng.below(live.len())];
                    world
                        .set_world_position(object, rng.point(10.0))
                        .expect(&context);
                }
                2 => {
                    let object = live[rng.below(live.len())];
                    world
                        .set_world_rotation(object, rng.point(180.0))
                        .expect(&context);
                }
                3 => {
                    let roots: Vec<ObjectHandle> = live
                        .iter()
                        .copied()
                        .filter(|&object| world.object(object).is_some_and(|o| o.parent.is_none()))
                        .collect();
                    let root = roots[rng.below(roots.len())];
                    world
                        .set_root_pose(
                            root,
                            rng.point(10.0),
                            Quat::from_rotation_y(rng.range(-3.0, 3.0)),
                        )
                        .expect(&context);
                    // set_root_pose пересчитывает всю накопленную очередь сам.
                    assert_cache_matches_full_recalc(&world, &context);
                }
                4 => {
                    let child = movable[rng.below(movable.len())];
                    let excluded = world.subtree(&[child]);
                    let candidates: Vec<ObjectHandle> = live
                        .iter()
                        .copied()
                        .filter(|object| !excluded.contains(object))
                        .collect();
                    let parent = if rng.chance(20) {
                        None
                    } else {
                        Some(candidates[rng.below(candidates.len())])
                    };
                    let keep_world = rng.chance(50);
                    let reparented = world.object(child).is_some_and(|o| o.parent != parent);
                    world.set_parent(child, parent, keep_world).expect(&context);
                    // Тот же родитель — ранний выход без пересчёта: проверка только при смене.
                    if reparented {
                        assert_cache_matches_full_recalc(&world, &context);
                    }
                }
                5 => {
                    let parent = if live.is_empty() || rng.chance(30) {
                        None
                    } else {
                        Some(live[rng.below(live.len())])
                    };
                    let options = SpawnOptions {
                        parent,
                        position: Some(rng.point(10.0)),
                        ..SpawnOptions::default()
                    };
                    spawn(&mut world, &bullet, options);
                }
                6 => {
                    let object = movable[rng.below(movable.len())];
                    world.despawn_many(&[object]).expect(&context);
                }
                _ => {
                    // Пересчёт: кеш сверяется с эталоном сразу после него.
                    world.recalculate_world_matrices();
                    assert_cache_matches_full_recalc(&world, &context);
                }
            }
            if rng.chance(30) {
                world.recalculate_world_matrices();
                assert_cache_matches_full_recalc(&world, &context);
            }
        }
        world.recalculate_world_matrices();
        assert_cache_matches_full_recalc(&world, &format!("seed {seed}, финал"));
    }
}

#[test]
fn clean_world_recalculation_does_no_transform_work() {
    let mut world = world(TREE);
    take_transform_updates();

    world.recalculate_world_matrices();
    let _ = world.build_frame();

    assert_eq!(take_transform_updates(), 0);
}

#[test]
fn leaf_edit_recalculates_one_object() {
    let mut world = world(TREE);
    let leaf = handle(&world, "a1");
    take_transform_updates();

    world
        .set_local_transform(leaf, moved(Vec3::new(4.0, 0.0, 0.0)))
        .unwrap();
    world.recalculate_world_matrices();

    assert_eq!(take_transform_updates(), 1);
    assert_cache_matches_full_recalc(&world, "после правки листа");
}

#[test]
fn root_edit_recalculates_its_whole_subtree() {
    let mut world = world(TREE);
    let root = handle(&world, "root");
    let other = handle(&world, "other");
    take_transform_updates();

    // Поддерево root: root, a, a1, b.
    world
        .set_local_transform(root, moved(Vec3::new(2.0, 0.0, 0.0)))
        .unwrap();
    world.recalculate_world_matrices();
    assert_eq!(take_transform_updates(), 4);
    assert_cache_matches_full_recalc(&world, "после правки корня root");
    // Эталон сам считает все объекты через update_world_transform: сбрасываем счётчик.
    take_transform_updates();

    // Поддерево other: other, o1.
    world
        .set_local_transform(other, moved(Vec3::new(6.0, 0.0, 0.0)))
        .unwrap();
    world.recalculate_world_matrices();
    assert_eq!(take_transform_updates(), 2);
    assert_cache_matches_full_recalc(&world, "после правки корня other");
}

#[test]
fn deferred_edit_is_recalculated_by_root_pose_in_another_branch() {
    let mut world = world(TREE);
    let a1 = handle(&world, "a1");
    let other = handle(&world, "other");

    world
        .set_local_transform(a1, moved(Vec3::new(0.0, 0.0, 7.0)))
        .unwrap();
    world
        .set_root_pose(other, Vec3::new(9.0, 0.0, 0.0), Quat::from_rotation_y(0.5))
        .unwrap();

    assert_cache_matches_full_recalc(&world, "set_root_pose в другой ветке без явного пересчёта");
}

#[test]
fn deferred_edit_is_recalculated_by_set_parent_in_another_branch() {
    let mut world = world(TREE);
    let a1 = handle(&world, "a1");
    let o1 = handle(&world, "o1");

    world
        .set_local_transform(a1, moved(Vec3::new(0.0, 0.0, 7.0)))
        .unwrap();
    world.set_parent(o1, None, true).unwrap();

    assert_cache_matches_full_recalc(&world, "set_parent в другой ветке без явного пересчёта");
}

#[test]
fn replacement_marks_incoming_objects_and_keeps_survivor_edits() {
    let mut current = world(&format!(
        "{PERSISTENT_ROOT}[[objects]]\nid = \"old\"\n[[objects]]\nid = \"cam_old\"\n[objects.camera]\n"
    ));
    let kid = handle(&current, "kid");
    current
        .set_local_transform(kid, moved(Vec3::new(0.0, 4.0, 0.0)))
        .unwrap();
    let mut incoming = world(concat!(
        "[[objects]]\nid = \"cam_new\"\n[objects.camera]\n",
        "[[objects]]\nid = \"new_root\"\n[objects.transform]\nposition = [1.0, 2.0, 3.0]\n",
        "[[objects]]\nid = \"new_child\"\nparent = \"new_root\"\n[objects.transform]\nposition = [0.0, 1.0, 0.0]\n",
    ));
    let new_root = handle(&incoming, "new_root");
    incoming
        .set_local_transform(new_root, moved(Vec3::new(8.0, 0.0, 0.0)))
        .unwrap();
    let incoming_cache: Vec<Mat4> = incoming
        .objects()
        .iter()
        .map(|object| incoming.world_matrix(object.handle).unwrap())
        .collect();

    let replacement = current.replace_nonpersistent(incoming).unwrap();

    // Сразу после замены вставленные объекты несут кеш сцены до замены, как и прежде.
    for (added, expected) in replacement.added.iter().zip(&incoming_cache) {
        assert_eq!(
            matrix_bits(current.world_matrix(*added).unwrap()),
            matrix_bits(*expected),
            "кеш вставленного объекта сразу после замены"
        );
    }
    current.recalculate_world_matrices();
    assert_cache_matches_full_recalc(&current, "после пересчёта после замены");
    let new_root_now = current.world_matrix(replacement.added[1]).unwrap();
    assert_ne!(
        matrix_bits(new_root_now),
        matrix_bits(incoming_cache[1]),
        "отложенная правка входящей сцены видна после пересчёта"
    );
}

#[test]
fn append_persistent_from_recalculates_with_deferred_previous_edit() {
    // Сохраняемые хендлы не должны совпадать со слотами живых объектов новой сцены: камера
    // новой сцены занимает слот 0, поэтому сохраняемый корень стоит дальше.
    let mut previous = world(concat!(
        "[[objects]]\nid = \"cam_old\"\n[objects.camera]\n",
        "[[objects]]\nid = \"f1\"\n[[objects]]\nid = \"f2\"\n",
        "[[objects]]\nid = \"keep\"\nkeep_on_load = true\n",
        "[[objects]]\nid = \"kid\"\nparent = \"keep\"\n",
    ));
    let keep = handle(&previous, "keep");
    previous
        .set_local_transform(keep, moved(Vec3::new(3.0, 0.0, 0.0)))
        .unwrap();
    let mut current = world("[[objects]]\nid = \"cam_new\"\n[objects.camera]\n");

    let appended = current.append_persistent_from(&previous).unwrap();

    assert_eq!(appended.len(), 2);
    assert_cache_matches_full_recalc(&current, "после append_persistent_from");
}

#[test]
fn clone_with_deferred_edits_matches_original() {
    let mut original = world(TREE);
    let a1 = handle(&original, "a1");
    original
        .set_local_transform(a1, moved(Vec3::new(0.0, 0.0, 5.0)))
        .unwrap();
    let original_bits: Vec<[u32; 16]> = original
        .world_matrices
        .iter()
        .map(|matrix| matrix_bits(*matrix))
        .collect();

    let mut copy = original.clone();

    let copy_bits: Vec<[u32; 16]> = copy
        .world_matrices
        .iter()
        .map(|matrix| matrix_bits(*matrix))
        .collect();
    assert_eq!(copy_bits, original_bits, "клон копирует кеш как есть");
    original.recalculate_world_matrices();
    copy.recalculate_world_matrices();
    assert_cache_matches_full_recalc(&original, "оригинал после пересчёта");
    assert_cache_matches_full_recalc(&copy, "клон после пересчёта");
}

#[test]
fn reparent_there_and_back_matches_full_recalc() {
    let mut world = world(TREE);
    let b = handle(&world, "b");
    let other = handle(&world, "other");
    let root = handle(&world, "root");
    let a1 = handle(&world, "a1");
    world
        .set_local_transform(a1, moved(Vec3::new(0.0, 0.0, 9.0)))
        .unwrap();

    world.set_parent(b, Some(other), false).unwrap();
    assert_cache_matches_full_recalc(&world, "перенос b под other");
    world.set_parent(b, Some(a1), true).unwrap();
    assert_cache_matches_full_recalc(&world, "перенос b под a1");
    world.set_parent(b, Some(root), true).unwrap();
    assert_cache_matches_full_recalc(&world, "перенос b обратно под root");
}

#[test]
fn despawn_subtree_with_dirty_descendants_keeps_survivors_exact() {
    let mut world = world(TREE);
    let a1 = handle(&world, "a1");
    let o1 = handle(&world, "o1");
    let root = handle(&world, "root");
    world
        .set_local_transform(a1, moved(Vec3::new(0.0, 0.0, 6.0)))
        .unwrap();
    world
        .set_local_transform(o1, moved(Vec3::new(0.0, 8.0, 0.0)))
        .unwrap();

    world.despawn_many(&[root]).unwrap();

    world.recalculate_world_matrices();
    assert_cache_matches_full_recalc(&world, "после удаления поддерева с грязными потомками");
}

#[test]
fn spawn_caches_new_objects_immediately_and_under_dirty_ancestor_after_recalc() {
    let mut world = world(TREE);
    let bullet = template(BULLET);
    let root = handle(&world, "root");

    spawn(
        &mut world,
        &bullet,
        SpawnOptions {
            parent: Some(root),
            position: Some(Vec3::new(1.0, 0.0, 0.0)),
            ..SpawnOptions::default()
        },
    );
    assert_cache_matches_full_recalc(&world, "spawn в чистом мире без пересчёта");

    world
        .set_local_transform(root, moved(Vec3::new(-3.0, 0.0, 0.0)))
        .unwrap();
    spawn(
        &mut world,
        &bullet,
        SpawnOptions {
            parent: Some(root),
            ..SpawnOptions::default()
        },
    );
    world.recalculate_world_matrices();
    assert_cache_matches_full_recalc(&world, "spawn под грязным предком после пересчёта");
}
