//! Скелет, веса и клипы FBX через публичный API `engine-assets`.
//!
//! Фикстуры лежат в `tests/fixtures`; тесты не используют окно, GPU или ожидания по времени.

use engine_assets::{
    AssetError, Assets, Model, ModelAnimation, ModelInheritMode, ModelSkeleton, ModelSkin,
};

fn fixtures() -> Assets {
    Assets::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
}

fn node_index(skeleton: &ModelSkeleton, name: &str) -> usize {
    skeleton
        .nodes
        .iter()
        .position(|node| node.name == name)
        .unwrap_or_else(|| panic!("узел {name} не найден в скелете"))
}

/// Имя узла, к которому привязана палитра `joint`.
fn joint_name(skeleton: &ModelSkeleton, joint: u32) -> &str {
    &skeleton.nodes[skeleton.joints[joint as usize].node].name
}

/// Веса угла по имени кости: `(имя кости, вес)` для каждой ненулевой связи.
fn corner_weights<'a>(
    skeleton: &'a ModelSkeleton,
    skin: &ModelSkin,
    corner: usize,
) -> Vec<(&'a str, f32)> {
    skin.joints[corner]
        .iter()
        .zip(skin.weights[corner])
        .filter(|&(_, weight)| weight > 0.0)
        .map(|(&joint, weight)| (joint_name(skeleton, joint), weight))
        .collect()
}

fn body_mesh(model: &Model) -> &engine_assets::ModelMesh {
    model
        .meshes
        .iter()
        .find(|mesh| mesh.vertices.iter().any(|vertex| vertex.position[0] < 0.0))
        .expect("меш Body")
}

fn bend_animation(model: &Model) -> &ModelAnimation {
    assert_eq!(model.animations.len(), 1);
    &model.animations[0]
}

#[test]
fn skin_exposes_skeleton_per_corner_weights_and_control_point_sharing() {
    let model = fixtures().load_model("/skin_chain.fbx").unwrap();
    let skeleton = model.skeleton.as_ref().expect("у модели есть skin");
    assert_eq!(model.meshes.len(), 2);

    let body = body_mesh(&model);
    let skin = body.skin.as_ref().expect("skin у меша Body");
    assert_eq!(skin.joints.len(), body.vertices.len());
    assert_eq!(skin.weights.len(), body.vertices.len());

    for (corner, vertex) in body.vertices.iter().enumerate() {
        let weights = corner_weights(skeleton, skin, corner);
        let y = vertex.position[1];
        if y < 0.5 {
            assert_eq!(weights, [("Bone1", 1.0)], "нижний ряд, угол {corner}");
        } else if y > 1.5 {
            assert_eq!(weights, [("Bone2", 1.0)], "верхний ряд, угол {corner}");
        } else {
            // Контрольные точки среднего ряда общие для двух граней и обе получают 50/50.
            assert_eq!(weights.len(), 2, "средний ряд, угол {corner}");
            for expected in ["Bone1", "Bone2"] {
                let weight = weights
                    .iter()
                    .find(|(name, _)| *name == expected)
                    .map(|(_, weight)| *weight)
                    .unwrap_or(0.0);
                assert!((weight - 0.5).abs() < 1e-6, "{expected} в углу {corner}");
            }
        }
    }
}

#[test]
fn unskinned_part_under_bone_is_rigidly_bound_to_its_node() {
    let model = fixtures().load_model("/skin_chain.fbx").unwrap();
    let skeleton = model.skeleton.as_ref().unwrap();
    let blade = model
        .meshes
        .iter()
        .find(|mesh| mesh.vertices.iter().all(|vertex| vertex.position[0] > 1.5))
        .expect("меш Blade");
    let skin = blade
        .skin
        .as_ref()
        .expect("жёсткая привязка у нескиннутой части");
    for corner in 0..blade.vertices.len() {
        assert_eq!(corner_weights(skeleton, skin, corner), [("Blade", 1.0)]);
    }
    let blade_node = node_index(skeleton, "Blade");
    assert_eq!(
        skeleton.nodes[blade_node].parent,
        Some(node_index(skeleton, "Bone2"))
    );
}

#[test]
fn skeleton_keeps_bones_mesh_nodes_and_their_ancestors_in_parent_first_order() {
    let model = fixtures().load_model("/skin_chain.fbx").unwrap();
    let skeleton = model.skeleton.as_ref().unwrap();
    let root = node_index(skeleton, "Root");
    let bone1 = node_index(skeleton, "Bone1");
    let bone2 = node_index(skeleton, "Bone2");
    // Неименованный корень сцены ufbx входит в скелет как предок всех узлов.
    let scene_root = skeleton.nodes[root].parent.expect("родитель Root");
    assert_eq!(skeleton.nodes[scene_root].name, "");
    assert_eq!(skeleton.nodes[scene_root].parent, None);
    assert_eq!(skeleton.nodes[bone1].parent, Some(root));
    assert_eq!(skeleton.nodes[bone2].parent, Some(bone1));
    for (index, node) in skeleton.nodes.iter().enumerate() {
        if let Some(parent) = node.parent {
            assert!(parent < index, "родитель {} стоит раньше", node.name);
        }
    }
    assert_eq!(skeleton.nodes[bone2].rest.translation, [0.0, 1.0, 0.0]);
    assert_eq!(skeleton.nodes[bone2].inherit_mode, ModelInheritMode::Normal);
    assert_eq!(skeleton.joints.len(), 4, "две кости, Body и Blade");
}

#[test]
fn model_stack_is_baked_into_clip_with_sampled_rotation_keys() {
    let model = fixtures().load_model("/skin_chain.fbx").unwrap();
    let clip = bend_animation(&model);
    assert!((clip.duration - 1.0).abs() < 1e-6);
    let track = clip
        .tracks
        .iter()
        .find(|track| track.node == "Bone2")
        .expect("дорожка Bone2");
    // ufbx запекает ключи с частотой 30 fps, поэтому проверяем начало, середину и конец.
    assert_eq!(track.rotation.first().map(|key| key.time), Some(0.0));
    assert!((track.rotation.last().unwrap().time - 1.0).abs() < 1e-6);
    let key_at = |time: f32| {
        track
            .rotation
            .iter()
            .find(|key| (key.time - time).abs() < 1e-4)
            .unwrap_or_else(|| panic!("ключ на {time} с"))
            .value
    };
    // Поворот на 30° в середине и 60° в конце вокруг Z: кватернион (0, 0, sin(θ/2), cos(θ/2)).
    let middle = key_at(0.5);
    assert!((middle[2] - 0.258_819).abs() < 1e-4 && (middle[3] - 0.965_926).abs() < 1e-4);
    let last = key_at(1.0);
    assert!((last[2] - 0.5).abs() < 1e-4 && (last[3] - 0.866_025_4).abs() < 1e-4);
}

#[test]
fn skin_without_stacks_has_skeleton_but_no_animations() {
    let model = fixtures().load_model("/skin_chain_bare.fbx").unwrap();
    assert!(model.skeleton.is_some());
    assert!(model.animations.is_empty());
    assert!(model.meshes.iter().all(|mesh| mesh.skin.is_some()));
}

#[test]
fn clip_only_file_loads_named_clip_without_meshes() {
    let clips = fixtures().load_animations("/bend_clip.fbx").unwrap();
    assert_eq!(clips.len(), 1);
    assert_eq!(clips[0].name, "bend_clip");
    assert!(clips[0].tracks.iter().any(|track| track.node == "Bone2"));
}

#[test]
fn clip_loading_rejects_files_without_animation_stacks() {
    let error = fixtures()
        .load_animations("/skin_chain_bare.fbx")
        .unwrap_err();
    match error {
        AssetError::Decode { message, .. } => {
            assert_eq!(message, "в FBX нет анимационных стеков");
        }
        other => panic!("ожидалась ошибка декодирования, получено {other:?}"),
    }
}

#[test]
fn inherit_modes_and_scale_nodes_follow_fbx_inherit_type() {
    let model = fixtures().load_model("/inherit_scale.fbx").unwrap();
    let skeleton = model.skeleton.as_ref().unwrap();
    let root = node_index(skeleton, "Root");
    let hip = node_index(skeleton, "Hip");
    let knee = node_index(skeleton, "Knee");

    assert_eq!(skeleton.nodes[root].inherit_mode, ModelInheritMode::Normal);
    assert_eq!(skeleton.nodes[root].rest.scale, [2.0, 2.0, 2.0]);
    assert_eq!(
        skeleton.nodes[hip].inherit_mode,
        ModelInheritMode::IgnoreParentScale
    );
    assert_eq!(skeleton.nodes[hip].inherit_scale_node, None);
    assert_eq!(
        skeleton.nodes[knee].inherit_mode,
        ModelInheritMode::ComponentwiseScale
    );
    assert_eq!(skeleton.nodes[knee].inherit_scale_node, Some(hip));
    assert_eq!(skeleton.nodes[knee].parent, Some(hip));
    assert_eq!(model.animations[0].name, "Idle");
}
