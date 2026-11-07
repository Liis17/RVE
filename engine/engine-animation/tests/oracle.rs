//! Оракул: позиции вершин после skinning сравниваются с `ufbx::evaluate_scene`.
//!
//! Фикстуры лежат в `engine-assets/tests/fixtures`. Загрузка ufbx повторяет опции
//! `engine-assets`, иначе совпадут только единицы и оси по соглашению движка.

use engine_animation::{Clip, Pose, Skeleton};
use engine_assets::{Assets, Model, ModelMesh};
use glam::{Mat4, Vec3};
use std::fs;
use std::path::Path;

/// Допуск по координатам в метрах; для одноосных вращений расхождение около 1e-6.
const TOLERANCE: f32 = 1e-4;
const TIMES: [f32; 5] = [0.0, 0.3, 0.5, 0.77, 1.0];

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../engine-assets/tests/fixtures")
        .join(name)
}

fn assets() -> Assets {
    Assets::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../engine-assets/tests/fixtures"))
}

/// Загружает сцену ufbx теми же опциями, что `engine-assets`.
fn load_scene(name: &str) -> ufbx::SceneRoot {
    let bytes = fs::read(fixture_path(name)).unwrap();
    let options = ufbx::LoadOpts {
        filename: name.into(),
        path_separator: b'/',
        load_external_files: false,
        open_main_file_with_default: false,
        open_file_cb: Default::default(),
        target_axes: ufbx::CoordinateAxes {
            right: ufbx::CoordinateAxis::PositiveX,
            up: ufbx::CoordinateAxis::PositiveY,
            front: ufbx::CoordinateAxis::PositiveZ,
        },
        target_unit_meters: 1.0,
        space_conversion: ufbx::SpaceConversion::ModifyGeometry,
        generate_missing_normals: true,
        ..Default::default()
    };
    ufbx::load_memory(&bytes, options).unwrap()
}

/// Углы меша в порядке `engine-assets`: части, грани и треугольники.
fn corners(mesh: &ufbx::Mesh) -> Vec<usize> {
    let mut triangle = vec![0u32; mesh.max_face_triangles * 3];
    let mut out = Vec::new();
    for part in mesh.material_parts.iter() {
        for &face_index in part.face_indices.iter() {
            let face = mesh.faces[face_index as usize];
            let triangles = mesh.triangulate_face(&mut triangle, face) as usize;
            out.extend(
                triangle[..triangles * 3]
                    .iter()
                    .map(|&index| index as usize),
            );
        }
    }
    out
}

/// Эталонные мировые позиции углов меша из `evaluate_scene` на момент `time`.
fn oracle_positions(scene: &ufbx::Scene, anim: &ufbx::Anim, time: f32, name: &str) -> Vec<Vec3> {
    let evaluated = ufbx::evaluate_scene(
        scene,
        anim,
        f64::from(time),
        ufbx::EvaluateOpts {
            evaluate_skinning: true,
            ..Default::default()
        },
    )
    .unwrap();
    let node = evaluated
        .nodes
        .iter()
        .find(|node| &*node.element.name == name && node.mesh.is_some())
        .unwrap_or_else(|| panic!("узел {name} с мешем"));
    let mesh = node.mesh.as_ref().unwrap();
    corners(mesh)
        .into_iter()
        .map(|corner| {
            let position = mesh.skinned_position[corner];
            let position = if mesh.skinned_is_local {
                ufbx::transform_position(&node.geometry_to_world, position)
            } else {
                position
            };
            Vec3::new(position.x as f32, position.y as f32, position.z as f32)
        })
        .collect()
}

/// Позиции вершин меша после линейного смешивания палитрами, как в renderer.
fn skinned_positions(mesh: &ModelMesh, palettes: &[[[f32; 4]; 4]]) -> Vec<Vec3> {
    let skin = mesh.skin.as_ref().expect("skin у меша");
    mesh.vertices
        .iter()
        .zip(&skin.joints)
        .zip(&skin.weights)
        .map(|((vertex, joints), weights)| {
            let rest = Vec3::from_array(vertex.position);
            joints
                .iter()
                .zip(weights)
                .filter(|(_, weight)| **weight > 0.0)
                .fold(Vec3::ZERO, |acc, (joint, weight)| {
                    let palette = Mat4::from_cols_array_2d(&palettes[*joint as usize]);
                    acc + palette.transform_point3(rest) * *weight
                })
        })
        .collect()
}

/// Палитры скиннинга на момент `time` при проигрывании клипа из позы покоя.
fn palettes_at(clip: &Clip, skeleton: &Skeleton, time: f32) -> Vec<[[f32; 4]; 4]> {
    let mut pose = Pose::rest(skeleton);
    clip.sample(time, false, &mut pose);
    let mut palettes = Vec::new();
    pose.skin_matrices(skeleton, &mut palettes);
    palettes
}

fn assert_positions_close(actual: &[Vec3], expected: &[Vec3], label: &str) {
    assert_eq!(actual.len(), expected.len(), "{label}: число вершин");
    for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (*a - *e).length() < TOLERANCE,
            "{label}, угол {index}: получено {a:?}, эталон {e:?}"
        );
    }
}

fn mesh_named_body_and_blade(model: &Model) -> (&ModelMesh, &ModelMesh) {
    let body = model
        .meshes
        .iter()
        .find(|mesh| mesh.vertices.iter().any(|vertex| vertex.position[0] < 0.0))
        .expect("Body");
    let blade = model
        .meshes
        .iter()
        .find(|mesh| mesh.vertices.iter().all(|vertex| vertex.position[0] > 1.5))
        .expect("Blade");
    (body, blade)
}

#[test]
fn skinned_and_rigid_vertices_match_ufbx_evaluation_over_time() {
    let model = assets().load_model("/skin_chain.fbx").unwrap();
    let skeleton = Skeleton::from_model(model.skeleton.as_ref().unwrap());
    let clip = Clip::bind(&model.animations[0], &skeleton);
    assert!(clip.unmatched().is_empty());
    let scene = load_scene("skin_chain.fbx");
    let stack = &scene.anim_stacks[0];
    let (body, blade) = mesh_named_body_and_blade(&model);

    // Проверка, что эталон действительно меняется во времени, иначе сравнение тривиально.
    let rest_body = oracle_positions(&scene, &stack.anim, 0.0, "Body");
    let end_body = oracle_positions(&scene, &stack.anim, 1.0, "Body");
    let moved = rest_body
        .iter()
        .zip(&end_body)
        .map(|(a, b)| (*a - *b).length())
        .fold(0.0_f32, f32::max);
    assert!(moved > 0.1, "анимация должна сдвигать вершины: {moved}");

    for time in TIMES {
        let palettes = palettes_at(&clip, &skeleton, time);
        let expected_body = oracle_positions(&scene, &stack.anim, time, "Body");
        assert_positions_close(
            &skinned_positions(body, &palettes),
            &expected_body,
            &format!("Body t={time}"),
        );
        let expected_blade = oracle_positions(&scene, &stack.anim, time, "Blade");
        assert_positions_close(
            &skinned_positions(blade, &palettes),
            &expected_blade,
            &format!("Blade t={time}"),
        );
    }
}

#[test]
fn rest_composition_with_inherit_modes_matches_ufbx_world_transform() {
    let model = assets().load_model("/inherit_scale.fbx").unwrap();
    let skeleton = Skeleton::from_model(model.skeleton.as_ref().unwrap());
    let clip = Clip::bind(&model.animations[0], &skeleton);
    let scene = load_scene("inherit_scale.fbx");
    let stack = &scene.anim_stacks[0];
    let body = &model.meshes[0];
    for time in [0.0, 0.5] {
        let palettes = palettes_at(&clip, &skeleton, time);
        let expected = oracle_positions(&scene, &stack.anim, time, "Body");
        assert_positions_close(
            &skinned_positions(body, &palettes),
            &expected,
            &format!("inherit Body t={time}"),
        );
    }
}

#[test]
fn clip_file_drives_bare_skin_like_the_model_own_stack() {
    let bare = assets().load_model("/skin_chain_bare.fbx").unwrap();
    let skeleton = Skeleton::from_model(bare.skeleton.as_ref().unwrap());
    let clips = assets().load_animations("/bend_clip.fbx").unwrap();
    let clip = Clip::bind(&clips[0], &skeleton);
    assert!(
        clip.unmatched().is_empty(),
        "все дорожки клипа сопоставились"
    );
    let scene = load_scene("skin_chain.fbx");
    let stack = &scene.anim_stacks[0];
    let (body, blade) = mesh_named_body_and_blade(&bare);

    for time in TIMES {
        let palettes = palettes_at(&clip, &skeleton, time);
        assert_positions_close(
            &skinned_positions(body, &palettes),
            &oracle_positions(&scene, &stack.anim, time, "Body"),
            &format!("clip Body t={time}"),
        );
        assert_positions_close(
            &skinned_positions(blade, &palettes),
            &oracle_positions(&scene, &stack.anim, time, "Blade"),
            &format!("clip Blade t={time}"),
        );
    }
}
