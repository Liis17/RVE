//! Физика сцены `stress`: сетка динамических кубов падает на статичный пол.
//!
//! Мир шагает на главном потоке только после презентации кадра: кадр рисует текущие позы,
//! следующий шаг готовит позы для следующего кадра. Пропущенный кадр шага не делает.

use std::time::{Duration, Instant};

use engine_physics::{BodyDesc, BodyHandle, PhysicsCommand, PhysicsWorld, Pose, Shape, StepDriver};
use glam::{EulerRot, Mat4, Quat, Vec3};

use crate::scene::{STRESS_CUBE_SIZE, STRESS_FLOOR_HALF_EXTENT};

const FIXED_HZ: u32 = 60;
const MAX_SUBSTEPS: usize = 4;
const FLOOR_TOP: f32 = -1.0;
const CUBE_HALF_EXTENT: f32 = STRESS_CUBE_SIZE * 0.5;
const LIFT_MIN: f32 = 0.2;
const LIFT_RANGE: f32 = 0.4;
const SEED: u64 = 0x5eed_cafe;

/// Сумма и максимум времени шагов физики; среднее считается при выводе.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepStats {
    pub steps: u64,
    pub total: Duration,
    pub max: Duration,
}

impl StepStats {
    fn record(&mut self, step: Duration) {
        self.steps += 1;
        self.total += step;
        self.max = self.max.max(step);
    }

    pub fn avg_ms(&self) -> f64 {
        if self.steps == 0 {
            return 0.0;
        }
        self.total.as_secs_f64() * 1000.0 / self.steps as f64
    }

    pub fn max_ms(&self) -> f64 {
        self.max.as_secs_f64() * 1000.0
    }
}

/// Детерминированный генератор без новой зависимости: LCG Knuth с фиксированным зерном.
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

pub struct StressPhysics {
    driver: StepDriver,
    cubes: Vec<BodyHandle>,
    /// Матрицы кубов текущего кадра; пересчитываются один раз после каждого шага.
    current: Vec<Mat4>,
    /// Матрицы кубов последней презентации; до первой презентации предыдущий кадр равен текущему.
    presented: Option<Vec<Mat4>>,
    stats: StepStats,
    /// Число refresh'ей с обрезкой broadphase за весь прогон.
    truncated_refreshes: usize,
    truncation_reported: bool,
}

impl StressPhysics {
    /// Строит мир: статичный пол и по одному динамическому кубу на копию. Копии задают только
    /// горизонтальную позицию; высота и поворот выбираются детерминированно.
    pub fn new(copies: &[Mat4]) -> Result<Self, String> {
        let mut world = PhysicsWorld::default();
        let floor = Shape::cuboid(Vec3::new(
            STRESS_FLOOR_HALF_EXTENT,
            0.5,
            STRESS_FLOOR_HALF_EXTENT,
        ))
        .map_err(|error| error.to_string())?;
        let floor_pose = Pose {
            position: Vec3::new(0.0, FLOOR_TOP - 0.5, 0.0),
            rotation: Quat::IDENTITY,
        };
        world
            .insert_body(BodyDesc::static_collider(floor, floor_pose))
            .map_err(|error| error.to_string())?;

        let cube =
            Shape::cuboid(Vec3::splat(CUBE_HALF_EXTENT)).map_err(|error| error.to_string())?;
        let mut random = Lcg(SEED);
        let mut cubes = Vec::with_capacity(copies.len());
        for copy in copies {
            let column = copy.w_axis.truncate();
            let lift = LIFT_MIN + LIFT_RANGE * random.next_unit();
            let rotation = Quat::from_euler(
                EulerRot::XYZ,
                random.next_unit() * std::f32::consts::TAU,
                random.next_unit() * std::f32::consts::TAU,
                random.next_unit() * std::f32::consts::TAU,
            );
            let pose = Pose {
                position: Vec3::new(column.x, FLOOR_TOP + CUBE_HALF_EXTENT + lift, column.z),
                rotation,
            };
            let handle = world
                .insert_body(BodyDesc::dynamic(cube.clone(), pose, 1.0))
                .map_err(|error| error.to_string())?;
            cubes.push(handle);
        }
        let driver =
            StepDriver::new(world, FIXED_HZ, MAX_SUBSTEPS).map_err(|error| error.to_string())?;
        let mut physics = Self {
            driver,
            cubes,
            current: Vec::new(),
            presented: None,
            stats: StepStats::default(),
            truncated_refreshes: 0,
            truncation_reported: false,
        };
        physics.current = physics.transforms();
        Ok(physics)
    }

    /// Текущие матрицы кубов (поза и размер коллайдера); без трансформации сцены.
    pub fn transforms(&self) -> Vec<Mat4> {
        self.cubes
            .iter()
            .map(|&handle| {
                let pose = self
                    .driver
                    .world()
                    .body_pose(handle)
                    .expect("cube bodies are never removed");
                Mat4::from_scale_rotation_translation(
                    Vec3::splat(STRESS_CUBE_SIZE),
                    pose.rotation,
                    pose.position,
                )
            })
            .collect()
    }

    /// Матрицы текущего кадра и предыдущей презентации.
    pub fn frame_transforms(&self) -> (&[Mat4], &[Mat4]) {
        let previous = self.presented.as_deref().unwrap_or(&self.current);
        (&self.current, previous)
    }

    /// Фиксирует позы показанного кадра и продвигает мир на один шаг для следующего кадра.
    pub fn advance_after_presentation(&mut self) -> Result<Duration, String> {
        self.presented = Some(std::mem::take(&mut self.current));
        let started = Instant::now();
        let result = self
            .driver
            .advance_step(1.0 / FIXED_HZ as f32, std::iter::empty::<PhysicsCommand>())
            .map_err(|error| error.to_string())?;
        let step = started.elapsed();
        self.stats.record(step);
        self.truncated_refreshes += result.broadphase_truncated;
        self.current = self.transforms();
        Ok(step)
    }

    /// Истинно один раз, когда broadphase впервые обрезался: sandbox выводит предупреждение.
    pub fn take_truncation_warning(&mut self) -> bool {
        if self.truncated_refreshes > 0 && !self.truncation_reported {
            self.truncation_reported = true;
            return true;
        }
        false
    }

    pub fn stats(&self) -> StepStats {
        self.stats
    }

    /// Число кубов, которые не спят.
    pub fn awake(&self) -> usize {
        self.cubes
            .iter()
            .filter(|&&handle| self.driver.world().body_is_sleeping(handle) == Some(false))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::{STRESS_CUBE_SIZE, StressPhysics};
    use crate::scene::{STRESS_GRID, Scene};
    use glam::Mat4;

    const REST_HEIGHT: f32 = -1.0 + STRESS_CUBE_SIZE * 0.5;

    /// Блок 8×8 из сетки сцены: полный 48×48 слишком медленен для шагов в debug-сборке.
    fn block_copies() -> Vec<Mat4> {
        Scene::Stress
            .copies()
            .into_iter()
            .enumerate()
            .filter(|(index, _)| index % STRESS_GRID < 8 && index / STRESS_GRID < 8)
            .map(|(_, copy)| copy)
            .collect()
    }

    fn center_height(model: &Mat4) -> f32 {
        model.w_axis.y
    }

    /// Копия кадра: шаг меняет мир, поэтому заимствование из `frame_transforms` нельзя держать.
    fn owned((current, previous): (&[Mat4], &[Mat4])) -> (Vec<Mat4>, Vec<Mat4>) {
        (current.to_vec(), previous.to_vec())
    }

    #[test]
    fn stress_world_has_one_dynamic_cube_per_grid_copy() {
        let physics = StressPhysics::new(&Scene::Stress.copies()).unwrap();
        assert_eq!(physics.transforms().len(), STRESS_GRID * STRESS_GRID);
        assert_eq!(physics.awake(), STRESS_GRID * STRESS_GRID);
    }

    #[test]
    fn cubes_fall_onto_the_floor_and_fall_asleep() {
        let mut physics = StressPhysics::new(&block_copies()).unwrap();
        let mut steps = 0;
        while physics.awake() > 0 && steps < 1200 {
            physics.advance_after_presentation().unwrap();
            steps += 1;
        }
        assert_eq!(physics.awake(), 0, "кубы не уснули за {steps} шагов");
        for model in physics.transforms() {
            let height = center_height(&model);
            assert!(
                (height - REST_HEIGHT).abs() < 0.02,
                "куб лежит на высоте {height}, ожидалось {REST_HEIGHT}"
            );
        }
    }

    #[test]
    fn two_runs_produce_identical_poses() {
        let run = || {
            let mut physics = StressPhysics::new(&block_copies()).unwrap();
            for _ in 0..120 {
                physics.advance_after_presentation().unwrap();
            }
            physics.transforms()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn presented_skipped_presented_takes_two_steps_and_previous_is_last_presented() {
        let mut physics = StressPhysics::new(&block_copies()).unwrap();
        let (first_current, first_previous) = owned(physics.frame_transforms());
        assert_eq!(
            first_current, first_previous,
            "до презентации предыдущий кадр равен текущему"
        );
        physics.advance_after_presentation().unwrap();

        // Пропущенный кадр не презентован: шага нет, предыдущей остаётся первая презентация.
        let (skipped_current, skipped_previous) = owned(physics.frame_transforms());
        assert_eq!(physics.stats().steps, 1);
        assert_eq!(skipped_previous, first_current);

        // Следующий показанный кадр видит ту же позу, что пропущенный, и предыдущую презентацию.
        let (current, previous) = owned(physics.frame_transforms());
        assert_eq!(current, skipped_current);
        assert_eq!(previous, first_current);
        assert_ne!(current, previous, "после шага кубы сдвинулись");
        physics.advance_after_presentation().unwrap();
        assert_eq!(physics.stats().steps, 2);
    }

    #[test]
    fn current_frame_matches_the_poses_after_each_step() {
        let mut physics = StressPhysics::new(&block_copies()).unwrap();
        for _ in 0..3 {
            physics.advance_after_presentation().unwrap();
            let (current, _) = physics.frame_transforms();
            assert_eq!(current, physics.transforms().as_slice());
        }
    }

    #[test]
    fn truncation_warning_is_taken_once_after_a_truncated_refresh() {
        let mut physics = StressPhysics::new(&block_copies()).unwrap();
        assert!(!physics.take_truncation_warning());
        physics.truncated_refreshes = 1;
        assert!(physics.take_truncation_warning());
        assert!(!physics.take_truncation_warning());
    }
}
