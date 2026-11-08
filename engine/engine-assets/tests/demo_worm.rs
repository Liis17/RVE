//! Регрессия демо-червяка из `examples/demo-game`: единицы FBX, winding наружу и единицы клипа.
//!
//! Файлы читаются с диска через публичный API `engine-assets`; тест не использует окно, GPU или время.

use engine_assets::{Assets, Model, ModelAnimation, ModelSkeleton, ModelTrack};

const EPSILON: f32 = 1e-4;

fn demo_assets() -> Assets {
    Assets::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/demo-game/Assets"
    ))
}

/// Шестикратный объём по порядку вершин треугольников.
///
/// Положителен для замкнутого меша, треугольники которого смотрят наружу. Эталон — демо-куб.
fn signed_volume_times_six(model: &Model) -> f64 {
    model
        .meshes
        .iter()
        .flat_map(|mesh| {
            mesh.indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(move |&[i0, i1, i2]| {
                    let [a, b, c] = [i0, i1, i2]
                        .map(|index| mesh.vertices[index as usize].position.map(f64::from));
                    a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
                        + a[2] * (b[0] * c[1] - b[1] * c[0])
                })
        })
        .sum()
}

fn rest_translation(skeleton: &ModelSkeleton, bone: &str) -> [f32; 3] {
    skeleton
        .nodes
        .iter()
        .find(|node| node.name == bone)
        .unwrap_or_else(|| panic!("кость {bone} не найдена в скелете"))
        .rest
        .translation
}

fn track<'a>(animation: &'a ModelAnimation, bone: &str) -> &'a ModelTrack {
    animation
        .tracks
        .iter()
        .find(|track| track.node == bone)
        .unwrap_or_else(|| panic!("трек {bone} не найден в клипе {}", animation.name))
}

#[test]
fn demo_worm_is_a_quarter_meter_wide_and_one_and_quarter_meter_tall() {
    // Вершины червяка заданы в сантиметрах: 24 см по X и Z, 125 см по Y.
    let model = demo_assets().load_model("/models/worm.fbx").unwrap();
    let (min, max) = model.bounds().expect("у червяка есть вершины");
    let size = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
    for (axis, expected) in [(0, 0.24), (1, 1.25), (2, 0.24)] {
        assert!(
            (size[axis] - expected).abs() < EPSILON,
            "размер червяка по оси {axis}: {} м, ожидалось {expected} м (полный размер {size:?})",
            size[axis]
        );
    }
}

#[test]
fn demo_worm_winding_points_outward_like_the_demo_cube() {
    let assets = demo_assets();
    let cube = signed_volume_times_six(&assets.load_model("/models/shared-cube.fbx").unwrap());
    let worm = signed_volume_times_six(&assets.load_model("/models/worm.fbx").unwrap());
    assert!(
        cube > 0.0,
        "эталонный куб должен иметь положительный объём: {cube}"
    );
    assert!(
        worm > 0.0,
        "треугольники червяка смотрят внутрь: 6V = {worm}, у куба {cube}"
    );
}

#[test]
fn demo_worm_clip_and_skeleton_use_the_same_meter_units() {
    let assets = demo_assets();
    let model = assets.load_model("/models/worm.fbx").unwrap();
    let skeleton = model.skeleton.as_ref().expect("у червяка есть скелет");
    let clips = assets.load_animations("/anims/worm_curl.fbx").unwrap();
    let clip = clips
        .iter()
        .find(|clip| clip.name == "worm_curl")
        .expect("клип worm_curl");

    // Каждый сегмент тела червяка — 50 см, то есть 0.5 м.
    for bone in ["Bone2", "Bone3"] {
        let rest = rest_translation(skeleton, bone);
        assert!(
            (rest[1] - 0.5).abs() < EPSILON,
            "{bone}: rest translation {rest:?}, ожидалось (0, 0.5, 0) м"
        );
        let key = track(clip, bone)
            .translation
            .first()
            .expect("ключ трансляции");
        for axis in 0..3 {
            assert!(
                (key.value[axis] - rest[axis]).abs() < EPSILON,
                "{bone}: клип сдвигает кость относительно модели: {:?} против {rest:?}",
                key.value
            );
        }
    }
}
