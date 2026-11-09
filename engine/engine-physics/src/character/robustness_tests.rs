//! Стенд устойчивости контроллера: капсула ходит кадрами, как `simple_move`, по лестнице и в углы
//! из боксов и из одной triangle mesh. После каждого кадра капсула не должна проникать в статику.

use super::CharacterController;
use crate::world::{BodyDesc, BodyHandle, PhysicsWorld};
use crate::{Pose, Shape, TriMesh, collide};
use glam::{EulerRot, Quat, Vec3};
use std::sync::Arc;

const DT: f32 = 1.0 / 60.0;
const GRAVITY: f32 = -9.81;
const SPEED: f32 = 4.0;
const MAX_PENETRATION: f32 = 2.0e-3;

/// Блок по минимальному и максимальному углу.
type Block = (Vec3, Vec3);

fn controller() -> CharacterController {
    // Значения по умолчанию `CharacterControllerComponent`.
    CharacterController::new(0.35, 1.1, 0.3, 45.0, 0.2, 0.02, 1.0).unwrap()
}

fn floor() -> Block {
    (Vec3::new(-6.0, -0.5, -6.0), Vec3::new(6.0, 0.0, 6.0))
}

/// Лестница вдоль +X: шесть ступеней 0.15 × 0.3 шириной 1.6 и боковые стены — внутренние углы.
fn stairs() -> Vec<Block> {
    let mut blocks = vec![floor()];
    for step in 0..6 {
        let x = step as f32 * 0.3;
        blocks.push((
            Vec3::new(x, 0.0, -0.8),
            Vec3::new(x + 0.3, (step + 1) as f32 * 0.15, 0.8),
        ));
    }
    blocks.push((Vec3::new(1.8, 0.0, -0.8), Vec3::new(4.0, 0.9, 0.8)));
    blocks.push((Vec3::new(-3.0, 0.0, 0.8), Vec3::new(4.0, 3.0, 1.0)));
    blocks.push((Vec3::new(-3.0, 0.0, -1.0), Vec3::new(4.0, 3.0, -0.8)));
    blocks.push((Vec3::new(4.0, 0.0, -1.0), Vec3::new(4.2, 3.0, 1.0)));
    blocks
}

/// Открытая лестница без стен: капсула упирается в наружные углы ступеней.
fn open_stairs() -> Vec<Block> {
    let mut blocks = vec![floor()];
    for step in 0..6 {
        let x = step as f32 * 0.3;
        blocks.push((
            Vec3::new(x, 0.0, -0.6),
            Vec3::new(x + 0.3, (step + 1) as f32 * 0.15, 0.6),
        ));
    }
    blocks
}

/// Внутренний угол двух стен впереди и сбоку.
fn corner() -> Vec<Block> {
    vec![
        floor(),
        (Vec3::new(1.0, 0.0, -3.0), Vec3::new(1.2, 3.0, 1.2)),
        (Vec3::new(-3.0, 0.0, 1.0), Vec3::new(1.2, 3.0, 1.2)),
    ]
}

fn block_shape((min, max): Block) -> (Shape, Pose) {
    (
        Shape::cuboid((max - min) * 0.5).unwrap(),
        Pose::new((min + max) * 0.5, Quat::IDENTITY).unwrap(),
    )
}

fn block_mesh(blocks: &[Block]) -> Shape {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    for &(min, max) in blocks {
        let base = vertices.len() as u32;
        for index in 0..8 {
            vertices.push(Vec3::new(
                if index & 1 == 0 { min.x } else { max.x },
                if index & 2 == 0 { min.y } else { max.y },
                if index & 4 == 0 { min.z } else { max.z },
            ));
        }
        for [a, b, c, d] in [
            [0, 2, 6, 4],
            [1, 5, 7, 3],
            [0, 4, 5, 1],
            [2, 3, 7, 6],
            [0, 1, 3, 2],
            [4, 6, 7, 5],
        ] {
            triangles.push([base + a, base + b, base + c]);
            triangles.push([base + a, base + c, base + d]);
        }
    }
    Shape::TriMesh(Arc::new(TriMesh::new(vertices, triangles).unwrap()))
}

fn colliders(blocks: &[Block], as_mesh: bool) -> Vec<(Shape, Pose)> {
    if as_mesh {
        vec![(block_mesh(blocks), Pose::IDENTITY)]
    } else {
        blocks.iter().copied().map(block_shape).collect()
    }
}

/// Идёт капсулой кадрами `simple_move` и возвращает первое нарушение проникновения.
fn walk(
    colliders: &[(Shape, Pose)],
    start: Vec3,
    yaw_deg: f32,
    pitch_deg: f32,
    frames: usize,
) -> Result<Vec3, String> {
    let controller = controller();
    let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
    for (shape, pose) in colliders {
        world
            .insert_body(BodyDesc::static_collider(shape.clone(), *pose))
            .unwrap();
    }
    let snapshot = world.query_snapshot().unwrap();
    let capsule = controller.capsule().unwrap();
    let yaw = yaw_deg.to_radians();
    let direction = Vec3::new(yaw.cos(), 0.0, yaw.sin());
    let rotation = Quat::from_euler(EulerRot::YXZ, -yaw, pitch_deg.to_radians(), 0.0);
    let mut position = start;
    let mut vertical = 0.0_f32;
    let mut grounded = false;
    for frame in 0..frames {
        if grounded && vertical < 0.0 {
            vertical = 0.0;
        }
        vertical += GRAVITY * DT;
        let displacement = direction * SPEED * DT + Vec3::Y * vertical * DT;
        let result = controller
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(position, rotation).unwrap(),
                displacement,
            )
            .map_err(|error| format!("frame {frame}: {error}"))?;
        position += result.translation;
        grounded = result.grounded;
        if grounded && vertical < 0.0 {
            vertical = 0.0;
        }
        let upright = Pose::new(position, Quat::IDENTITY).unwrap();
        for (index, (shape, pose)) in colliders.iter().enumerate() {
            let depth = collide(&capsule, upright, shape, *pose)
                .unwrap()
                .map_or(0.0, |manifold| {
                    manifold
                        .contacts
                        .iter()
                        .fold(0.0_f32, |depth, contact| depth.max(contact.penetration))
                });
            if depth > MAX_PENETRATION {
                return Err(format!(
                    "frame {frame}: collider {index} penetrated by {depth} at {position}"
                ));
            }
        }
    }
    Ok(position)
}

fn standing_start(x: f32, z: f32) -> Vec3 {
    let controller = controller();
    Vec3::new(
        x,
        controller.height * 0.5 + controller.radius + controller.skin,
        z,
    )
}

fn sweep(name: &str, blocks: &[Block], start: Vec3, yaws: &[f32], pitch_deg: f32) {
    let mut failures = Vec::new();
    for as_mesh in [false, true] {
        let colliders = colliders(blocks, as_mesh);
        for &yaw in yaws {
            if let Err(error) = walk(&colliders, start, yaw, pitch_deg, 120) {
                failures.push(format!("{name} mesh={as_mesh} yaw={yaw}: {error}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn yaws(from: i32, to: i32, step: i32) -> Vec<f32> {
    (from..=to)
        .step_by(step as usize)
        .map(|yaw| yaw as f32)
        .collect()
}

#[test]
fn walking_up_walled_stairs_never_penetrates() {
    sweep(
        "stairs",
        &stairs(),
        standing_start(-1.0, 0.0),
        &yaws(-80, 80, 5),
        0.0,
    );
}

#[test]
fn walking_into_open_stair_corners_never_penetrates() {
    sweep(
        "open stairs",
        &open_stairs(),
        standing_start(-1.0, 0.0),
        &yaws(-85, 85, 5),
        0.0,
    );
}

#[test]
fn walking_into_an_inner_wall_corner_never_penetrates() {
    sweep(
        "corner",
        &corner(),
        standing_start(-0.5, -0.5),
        &yaws(0, 90, 5),
        0.0,
    );
}

#[test]
fn pitched_character_object_keeps_an_upright_capsule_on_stairs() {
    // Скрипт наклоняет объект игрока взглядом вниз; капсула персонажа остаётся вертикальной.
    sweep(
        "pitched stairs",
        &stairs(),
        standing_start(-1.0, 0.0),
        &yaws(-40, 40, 10),
        -60.0,
    );
}

#[test]
fn walking_straight_climbs_the_stairs_to_the_landing() {
    // Без проникновений персонаж не должен и застревать: прямой ход поднимает его на площадку.
    let landing = standing_start(0.0, 0.0).y + 0.9;
    for as_mesh in [false, true] {
        let end = walk(
            &colliders(&stairs(), as_mesh),
            standing_start(-1.0, 0.0),
            0.0,
            0.0,
            120,
        )
        .unwrap();
        assert!(end.x > 3.0, "mesh={as_mesh}: {end}");
        assert!((end.y - landing).abs() < 0.01, "mesh={as_mesh}: {end}");
    }
}

#[test]
fn capsule_started_inside_a_wall_is_pushed_out_before_moving() {
    // Капсула вошла в стену на 0.1 (spawn или наезд kinematic): нулевое движение её выталкивает,
    // и следующий ход в стену не проходит сквозь неё.
    let controller = controller();
    for as_mesh in [false, true] {
        let wall = (Vec3::new(0.0, 0.0, -2.0), Vec3::new(0.2, 3.0, 2.0));
        let colliders = colliders(&[floor(), wall], as_mesh);
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        for (shape, pose) in &colliders {
            world
                .insert_body(BodyDesc::static_collider(shape.clone(), *pose))
                .unwrap();
        }
        let snapshot = world.query_snapshot().unwrap();
        let start = standing_start(-controller.radius + 0.1, 0.0);
        let body = BodyHandle(u64::MAX);
        let recovered = controller
            .move_capsule(
                &snapshot,
                body,
                Pose::new(start, Quat::IDENTITY).unwrap(),
                Vec3::ZERO,
            )
            .unwrap();
        let position = start + recovered.translation;
        assert!(
            position.x <= -controller.radius + 1.0e-3 && position.x > -controller.radius - 0.01,
            "mesh={as_mesh}: {position}"
        );
        // Нормаль mesh-контакта берётся из EPA и может отклоняться на миллиметры.
        assert!(
            (position.y - start.y).abs() < 0.01,
            "mesh={as_mesh}: {position}"
        );

        let pushed = controller
            .move_capsule(
                &snapshot,
                body,
                Pose::new(start, Quat::IDENTITY).unwrap(),
                Vec3::X * 0.5,
            )
            .unwrap();
        assert!(
            start.x + pushed.translation.x <= -controller.radius + 1.0e-3,
            "mesh={as_mesh}: {pushed:?}"
        );
    }
}
