//! Замер пересчёта мировых матриц [`World`] на синтетических сценах без GPU, окна и файлов.
//!
//! Запуск из каталога `build/`: `cargo run -p engine-scene --release --example world_matrices`.
//! Объекты собираются текстом `.rvescene` в цепочки длины `depth` под корнями, камера — отдельный
//! корень. Кадр включает правки `set_local_transform` с последующим `recalculate_world_matrices`
//! либо серию `set_root_pose` по первым корням. Выводятся медиана и p95 времени кадра в
//! микросекундах после прогрева.

use engine_scene::SceneDocument;
use engine_scene::world::{ObjectHandle, World};
use glam::{Quat, Vec3};
use std::cmp::Reverse;
use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

const OBJECT_COUNTS: [usize; 2] = [1_000, 10_000];
const CHAIN_LENGTHS: [usize; 3] = [1, 8, 32];
const CHANGED_PERCENTS: [usize; 4] = [0, 1, 10, 100];
const POSED_ROOTS: usize = 100;
const WARMUP_FRAMES: usize = 10;
const MEASURED_FRAMES: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    /// Сначала самые глубокие объекты цепочек: правка пересчитывает один объект.
    Leaves,
    /// Сначала корни цепочек: правка пересчитывает всё поддерево.
    Roots,
}

impl EditTarget {
    fn label(self) -> &'static str {
        match self {
            Self::Leaves => "leaves",
            Self::Roots => "roots",
        }
    }
}

struct Scene {
    world: World,
    /// Корни цепочек в порядке документа.
    roots: Vec<ObjectHandle>,
    /// Объекты цепочек, самые глубокие первыми.
    leaves_first: Vec<ObjectHandle>,
    /// Объекты цепочек, корни первыми.
    roots_first: Vec<ObjectHandle>,
    /// Число объектов цепочек без камеры.
    chained: usize,
}

fn build_scene(object_count: usize, chain_len: usize) -> Scene {
    let chained = object_count - 1;
    let mut text = String::from("[[objects]]\nid = \"camera\"\n[objects.camera]\n");
    for index in 0..chained {
        let _ = write!(text, "\n[[objects]]\nid = \"o{index}\"\n");
        if index % chain_len != 0 {
            let _ = writeln!(text, "parent = \"o{}\"", index - 1);
        }
        let _ = writeln!(
            text,
            "[objects.transform]\nposition = [{:.3}, {:.3}, {:.3}]",
            (index % 13) as f32,
            (index % 7) as f32,
            (index % 5) as f32,
        );
    }
    let document = SceneDocument::parse("world-matrices-bench.rvescene", &text)
        .expect("бенчмарк строит корректный документ сцены");
    let world = World::from_document(&document).expect("бенчмарк строит корректный мир");

    let mut roots_first: Vec<usize> = (0..chained).collect();
    roots_first.sort_by_key(|&index| (index % chain_len, index));
    let mut leaves_first = roots_first.clone();
    leaves_first.sort_by_key(|&index| (Reverse(index % chain_len), index));
    let handle = |index: usize| {
        world
            .handle(&format!("o{index}"))
            .expect("объект бенчмарка жив")
    };
    let roots = (0..chained)
        .filter(|index| index % chain_len == 0)
        .map(handle)
        .collect();
    let leaves_first = leaves_first.into_iter().map(handle).collect();
    let roots_first = roots_first.into_iter().map(handle).collect();
    Scene {
        world,
        roots,
        leaves_first,
        roots_first,
        chained,
    }
}

/// Кадр правок: у каждого из первых `changed` объектов меняется позиция, значения зависят от
/// номера кадра, поэтому правка каждый раз реальная.
fn edit_frame(scene: &mut Scene, target: EditTarget, changed: usize, frame: usize) {
    let Scene {
        world,
        roots,
        leaves_first,
        roots_first,
        ..
    } = scene;
    let order = match target {
        EditTarget::Leaves => leaves_first,
        EditTarget::Roots => roots_first,
    };
    for (rank, &handle) in order.iter().take(changed).enumerate() {
        let mut transform = *world.local_transform(handle).expect("объект бенчмарка жив");
        transform.position.x = frame as f32 * 0.01 + rank as f32 * 0.001;
        world
            .set_local_transform(handle, transform)
            .expect("объект бенчмарка не статический");
    }
    world.recalculate_world_matrices();
    black_box(world.world_matrix(roots[0]));
}

/// Кадр физики: `set_root_pose` по первым корням; каждый вызов пересчитывает мир.
fn root_pose_frame(scene: &mut Scene, frame: usize) {
    let Scene { world, roots, .. } = scene;
    for (rank, &handle) in roots.iter().take(POSED_ROOTS).enumerate() {
        let position = Vec3::new(rank as f32 * 0.5, frame as f32 * 0.01, 0.0);
        let rotation = Quat::from_rotation_y(frame as f32 * 0.01 + rank as f32 * 0.001);
        world
            .set_root_pose(handle, position, rotation)
            .expect("корень бенчмарка принимает позу");
    }
    black_box(world.world_matrix(roots[0]));
}

/// Прогрев, затем время каждого измеренного кадра. Возвращает медиану и p95 в микросекундах.
fn measure(mut frame: impl FnMut(usize)) -> (f64, f64) {
    for index in 0..WARMUP_FRAMES {
        frame(index);
    }
    let mut samples: Vec<Duration> = Vec::with_capacity(MEASURED_FRAMES);
    for index in 0..MEASURED_FRAMES {
        let start = Instant::now();
        frame(WARMUP_FRAMES + index);
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    let micros = |sample: Duration| sample.as_secs_f64() * 1e6;
    let median = micros(samples[samples.len() / 2]);
    let p95 = micros(samples[(samples.len() * 95).div_ceil(100) - 1]);
    (median, p95)
}

fn main() {
    println!("World::recalculate_world_matrices: кадры правок (микросекунды)");
    println!(
        "{:>8} {:>6} {:>8} {:>8} {:>9} {:>12} {:>12}",
        "objects", "depth", "changed", "target", "edits", "median us", "p95 us"
    );
    for &object_count in &OBJECT_COUNTS {
        for &chain_len in &CHAIN_LENGTHS {
            let mut scene = build_scene(object_count, chain_len);
            for &percent in &CHANGED_PERCENTS {
                for target in [EditTarget::Leaves, EditTarget::Roots] {
                    if percent == 0 && target == EditTarget::Roots {
                        continue;
                    }
                    let changed = (scene.chained * percent).div_ceil(100);
                    let (median, p95) =
                        measure(|frame| edit_frame(&mut scene, target, changed, frame));
                    println!(
                        "{object_count:>8} {chain_len:>6} {percent:>7}% {:>8} {changed:>9} {median:>12.2} {p95:>12.2}",
                        target.label(),
                    );
                }
            }
        }
    }

    println!();
    println!("set_root_pose по первым корням: кадр из серии вызовов (микросекунды)");
    println!(
        "{:>8} {:>6} {:>10} {:>12} {:>12}",
        "objects", "depth", "posed", "median us", "p95 us"
    );
    for &object_count in &OBJECT_COUNTS {
        for &chain_len in &CHAIN_LENGTHS {
            let mut scene = build_scene(object_count, chain_len);
            let posed = POSED_ROOTS.min(scene.roots.len());
            let (median, p95) = measure(|frame| root_pose_frame(&mut scene, frame));
            println!("{object_count:>8} {chain_len:>6} {posed:>10} {median:>12.2} {p95:>12.2}");
        }
    }
}
