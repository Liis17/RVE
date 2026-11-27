use std::num::NonZeroUsize;
use std::time::Instant;

use engine_physics::{BodyDesc, BodyHandle, PhysicsWorld, Pose, Shape, StepDriver};
use glam::{EulerRot, Quat, Vec3};

const STRESS_GRID: usize = 48;
const STRESS_SPACING: f32 = 0.35;
const STRESS_CUBE_SIZE: f32 = 0.18;
const STRESS_FLOOR_HALF_EXTENT: f32 = 9.0;
const FLOOR_TOP: f32 = -1.0;
const CUBE_HALF_EXTENT: f32 = STRESS_CUBE_SIZE * 0.5;
const LIFT_MIN: f32 = 0.2;
const LIFT_RANGE: f32 = 0.4;
const SEED: u64 = 0x5eed_cafe;
const MEASURED_STEPS: usize = 120;
const SLEEPING_WARMUP_STEPS: usize = 600;
const PARALLEL_MIN_CONSTRAINTS: usize = 256;

struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
}

struct BenchResult {
    thread_count: usize,
    body_count: usize,
    active_pairs: usize,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

fn world_for(
    thread_count: usize,
    count: usize,
) -> Result<(StepDriver, Vec<BodyHandle>), Box<dyn std::error::Error>> {
    let mut world = PhysicsWorld::default();
    let floor = Shape::cuboid(Vec3::new(
        STRESS_FLOOR_HALF_EXTENT,
        0.5,
        STRESS_FLOOR_HALF_EXTENT,
    ))?;
    world.insert_body(BodyDesc::static_collider(
        floor,
        Pose {
            position: Vec3::new(0.0, FLOOR_TOP - 0.5, 0.0),
            rotation: Quat::IDENTITY,
        },
    ))?;

    let cube = Shape::cuboid(Vec3::splat(CUBE_HALF_EXTENT))?;
    let columns = if count == STRESS_GRID * STRESS_GRID {
        STRESS_GRID
    } else {
        (count as f32).sqrt().ceil() as usize
    };
    let offset = (columns.saturating_sub(1)) as f32 * STRESS_SPACING * 0.5;
    let mut random = Lcg(SEED);
    let mut dynamic_bodies = Vec::with_capacity(count);
    for index in 0..count {
        let x = (index % columns) as f32 * STRESS_SPACING - offset;
        let z = (index / columns) as f32 * STRESS_SPACING - offset;
        let lift = LIFT_MIN + LIFT_RANGE * random.next_unit();
        let rotation = Quat::from_euler(
            EulerRot::XYZ,
            random.next_unit() * std::f32::consts::TAU,
            random.next_unit() * std::f32::consts::TAU,
            random.next_unit() * std::f32::consts::TAU,
        );
        dynamic_bodies.push(world.insert_body(BodyDesc::dynamic(
            cube.clone(),
            Pose {
                position: Vec3::new(x, FLOOR_TOP + CUBE_HALF_EXTENT + lift, z),
                rotation,
            },
            1.0,
        ))?);
    }

    let threads = NonZeroUsize::new(thread_count).ok_or("thread count must be positive")?;
    world.set_solver_threads(threads);
    debug_assert_eq!(world.solver_threads().get(), thread_count);
    Ok((StepDriver::new(world, 60, 4)?, dynamic_bodies))
}

fn measure(
    thread_count: usize,
    body_count: usize,
    warmup_steps: usize,
) -> Result<BenchResult, Box<dyn std::error::Error>> {
    let (mut driver, dynamic_bodies) = world_for(thread_count, body_count)?;
    for _ in 0..warmup_steps {
        driver.advance_step(1.0 / 60.0, [])?;
        if dynamic_bodies
            .iter()
            .all(|handle| driver.world().body_is_sleeping(*handle) == Some(true))
        {
            break;
        }
    }
    if warmup_steps > 0
        && dynamic_bodies
            .iter()
            .any(|handle| driver.world().body_is_sleeping(*handle) != Some(true))
    {
        return Err("sleeping benchmark warmup left dynamic bodies awake".into());
    }
    let mut samples = Vec::with_capacity(MEASURED_STEPS);
    for _ in 0..MEASURED_STEPS {
        let started = Instant::now();
        driver.advance_step(1.0 / 60.0, [])?;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let mean_ms = samples.iter().sum::<f64>() / samples.len() as f64;
    let p50_ms = percentile(&samples, 0.50);
    let p95_ms = percentile(&samples, 0.95);
    let max_ms = *samples.last().expect("the benchmark has measured steps");
    Ok(BenchResult {
        thread_count,
        body_count,
        active_pairs: driver.world().active_pair_count(),
        mean_ms,
        p50_ms,
        p95_ms,
        max_ms,
    })
}

fn percentile(samples: &[f64], percentile: f64) -> f64 {
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index]
}

fn print_pair(name: &str, one: &BenchResult, four: &BenchResult) {
    println!(
        "{name}: bodies={}, pairs(4t)={}, {} threads mean/p50/p95/max={:.3}/{:.3}/{:.3}/{:.3} ms, {} threads={:.3}/{:.3}/{:.3}/{:.3} ms",
        one.body_count,
        four.active_pairs,
        one.thread_count,
        one.mean_ms,
        one.p50_ms,
        one.p95_ms,
        one.max_ms,
        four.thread_count,
        four.mean_ms,
        four.p50_ms,
        four.p95_ms,
        four.max_ms,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stress_bodies = STRESS_GRID * STRESS_GRID;
    let stress_one = measure(1, stress_bodies, 0)?;
    let stress_four = measure(4, stress_bodies, 0)?;
    let likely_parallel = stress_four.active_pairs >= PARALLEL_MIN_CONSTRAINTS;
    println!(
        "parallel-path heuristic: {} (active pairs={}, threshold={}); actual launch is checked by solver tests",
        if likely_parallel {
            "likely"
        } else {
            "not indicated"
        },
        stress_four.active_pairs,
        PARALLEL_MIN_CONSTRAINTS,
    );
    print_pair("stress active phase (120 steps)", &stress_one, &stress_four);

    let sleeping_one = measure(1, stress_bodies, SLEEPING_WARMUP_STEPS)?;
    let sleeping_four = measure(4, stress_bodies, SLEEPING_WARMUP_STEPS)?;
    print_pair(
        "stress all-sleep phase (up to 600 warmup steps)",
        &sleeping_one,
        &sleeping_four,
    );

    let small_one = measure(1, 16, 0)?;
    let small_four = measure(4, 16, 0)?;
    print_pair("small scene (below threshold)", &small_one, &small_four);
    Ok(())
}
