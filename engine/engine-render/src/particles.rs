//! Чистая логика GPU-частиц: описание эмиттера, кольцевой учёт слотов, bounds и план шага.
//!
//! Модуль не зависит от Vulkan. Состояние эмиттера меняется только через план шага,
//! который renderer коммитит после успешной отправки кадра. Живые частицы никогда не
//! перезаписываются: слот освобождается, когда его batch гарантированно умер.

use crate::{RenderError, TextureId};
use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Верхняя граница шага симуляции в секундах; большие `dt` обрезаются.
pub const MAX_PARTICLE_STEP: f32 = 0.1;
/// Ёмкость эмиттера с аддитивным смешиванием (без сортировки).
pub const MAX_ADDITIVE_PARTICLE_CAPACITY: u32 = 262_144;
/// Ёмкость эмиттера с альфа-смешиванием; ограничена стоимостью bitonic-сортировки.
pub const MAX_ALPHA_PARTICLE_CAPACITY: u32 = 65_536;
/// Максимум ключей кривых цвета и размера.
pub const MAX_PARTICLE_CURVE_KEYS: usize = 4;
/// Максимум строк и столбцов flipbook.
pub const MAX_FLIPBOOK_GRID: u32 = 16;

/// Форма области рождения частиц.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParticleShape {
    /// Точка; направление скорости случайное.
    Point,
    /// Поверхность сферы; скорость радиальная.
    Sphere { radius: f32 },
    /// Диск радиуса `radius` в плоскости y = 0; конус вдоль +Y с полууглом `angle` (радианы).
    Cone { angle: f32, radius: f32 },
    /// Прямоугольная область; частицы летят вдоль +Y.
    Box { half_extents: [f32; 3] },
}

/// Разовый выброс `count` частиц в момент `time` цикла (секунды).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleBurst {
    pub time: f32,
    pub count: u32,
}

/// Ключ цвета над временем жизни; `time` нормализовано в [0, 1].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleColorKey {
    pub time: f32,
    pub color: [f32; 4],
}

/// Ключ размера над временем жизни; `time` нормализовано в [0, 1].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleSizeKey {
    pub time: f32,
    pub size: f32,
}

/// Анимация текстуры: сетка `columns × rows`. `frames_per_second == 0` — кадр выбирается по времени жизни.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleFlipbook {
    pub columns: u32,
    pub rows: u32,
    pub frames_per_second: f32,
}

impl Default for ParticleFlipbook {
    fn default() -> Self {
        Self {
            columns: 1,
            rows: 1,
            frames_per_second: 0.0,
        }
    }
}

/// Смешивание частиц с кадром.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParticleBlend {
    /// Аддитивное смешивание; порядок отрисовки не важен, сортировки нет.
    #[default]
    Additive,
    /// Альфа-смешивание; частицы сортируются назад-вперёд внутри эмиттера.
    Alpha,
}

/// Столкновение частиц с видимой геометрией по depth buffer вида-владельца.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleCollision {
    pub restitution: f32,
    pub friction: f32,
    pub thickness: f32,
}

/// Полное описание эмиттера для `Renderer::create_particle_emitter`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParticleEmitterDesc {
    pub capacity: u32,
    pub blend: ParticleBlend,
    pub shape: ParticleShape,
    /// Частиц в секунду.
    pub rate: f32,
    pub bursts: Vec<ParticleBurst>,
    /// Длительность цикла в секундах.
    pub duration: f32,
    pub looping: bool,
    /// Время жизни, секунды: `[min, max]`, min > 0.
    pub lifetime: [f32; 2],
    /// Начальная скорость, м/с: `[min, max]`.
    pub speed: [f32; 2],
    /// Ширина квада частицы в мировых единицах: `[min, max]`.
    pub size: [f32; 2],
    /// Начальный поворот квада, радианы: `[min, max]`.
    pub rotation: [f32; 2],
    /// Угловая скорость, радианы в секунду: `[min, max]`.
    pub angular_velocity: [f32; 2],
    pub gravity: [f32; 3],
    /// Линейное затухание скорости, 1/с.
    pub drag: f32,
    /// Кривая цвета (включая альфу) над жизнью; пусто — белый без изменений.
    pub color_over_life: Vec<ParticleColorKey>,
    /// Кривая размера над жизнью, множитель к начальному размеру; пусто — без изменений.
    pub size_over_life: Vec<ParticleSizeKey>,
    /// Текстура частицы (sRGB); `None` — процедурный мягкий диск.
    pub texture: Option<TextureId>,
    pub flipbook: ParticleFlipbook,
    /// Расстояние в мировых единицах, на котором частица плавно затухает у геометрии (soft particles); 0 — выключено.
    pub soft_distance: f32,
    pub collision: Option<ParticleCollision>,
    pub seed: u32,
}

impl Default for ParticleEmitterDesc {
    fn default() -> Self {
        Self {
            capacity: 256,
            blend: ParticleBlend::Additive,
            shape: ParticleShape::Point,
            rate: 10.0,
            bursts: Vec::new(),
            duration: 1.0,
            looping: true,
            lifetime: [1.0, 2.0],
            speed: [0.0, 1.0],
            size: [0.1, 0.2],
            rotation: [0.0, 0.0],
            angular_velocity: [0.0, 0.0],
            gravity: [0.0, 0.0, 0.0],
            drag: 0.0,
            color_over_life: Vec::new(),
            size_over_life: Vec::new(),
            texture: None,
            flipbook: ParticleFlipbook::default(),
            soft_distance: 0.0,
            collision: None,
            seed: 1,
        }
    }
}

const fn invalid(message: &'static str) -> RenderError {
    RenderError::InvalidConfig(message)
}

fn finite_range(range: [f32; 2]) -> bool {
    range.iter().all(|value| value.is_finite()) && range[0] <= range[1]
}

/// Проверяет описание эмиттера. Ошибка — `InvalidConfig` с причиной.
pub fn validate_desc(desc: &ParticleEmitterDesc) -> Result<(), RenderError> {
    let capacity_limit = match desc.blend {
        ParticleBlend::Additive => MAX_ADDITIVE_PARTICLE_CAPACITY,
        ParticleBlend::Alpha => MAX_ALPHA_PARTICLE_CAPACITY,
    };
    if desc.capacity == 0 || desc.capacity > capacity_limit {
        return Err(invalid("ёмкость эмиттера частиц вне диапазона"));
    }
    match desc.shape {
        ParticleShape::Point => {}
        ParticleShape::Sphere { radius } => {
            if !radius.is_finite() || radius < 0.0 {
                return Err(invalid("радиус сферы эмиттера частиц некорректен"));
            }
        }
        ParticleShape::Cone { angle, radius } => {
            if !angle.is_finite()
                || !(0.0..=std::f32::consts::FRAC_PI_2).contains(&angle)
                || !radius.is_finite()
                || radius < 0.0
            {
                return Err(invalid("параметры конуса эмиттера частиц некорректны"));
            }
        }
        ParticleShape::Box { half_extents } => {
            if half_extents
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
            {
                return Err(invalid("размеры бокса эмиттера частиц некорректны"));
            }
        }
    }
    if !desc.rate.is_finite() || desc.rate < 0.0 {
        return Err(invalid("частота эмиссии частиц некорректна"));
    }
    if !desc.duration.is_finite() || desc.duration <= 0.0 {
        return Err(invalid("длительность цикла частиц должна быть больше нуля"));
    }
    for burst in &desc.bursts {
        if !burst.time.is_finite()
            || burst.time < 0.0
            || burst.time >= desc.duration
            || burst.count == 0
        {
            return Err(invalid("выброс частиц вне цикла или без частиц"));
        }
    }
    if !desc.lifetime.iter().all(|value| value.is_finite())
        || desc.lifetime[0] <= 0.0
        || desc.lifetime[0] > desc.lifetime[1]
    {
        return Err(invalid("время жизни частиц некорректно"));
    }
    if !finite_range(desc.speed) || desc.speed[0] < 0.0 {
        return Err(invalid("скорость частиц некорректна"));
    }
    if !finite_range(desc.size) || desc.size[0] < 0.0 {
        return Err(invalid("размер частиц некорректен"));
    }
    if !finite_range(desc.rotation) || !finite_range(desc.angular_velocity) {
        return Err(invalid("вращение частиц некорректно"));
    }
    if desc.gravity.iter().any(|value| !value.is_finite())
        || !desc.drag.is_finite()
        || desc.drag < 0.0
    {
        return Err(invalid("гравитация или затухание частиц некорректны"));
    }
    validate_color_keys(&desc.color_over_life)?;
    validate_size_keys(&desc.size_over_life)?;
    if desc.flipbook.columns == 0
        || desc.flipbook.rows == 0
        || desc.flipbook.columns > MAX_FLIPBOOK_GRID
        || desc.flipbook.rows > MAX_FLIPBOOK_GRID
        || !desc.flipbook.frames_per_second.is_finite()
        || desc.flipbook.frames_per_second < 0.0
    {
        return Err(invalid("flipbook частиц некорректен"));
    }
    if !desc.soft_distance.is_finite() || desc.soft_distance < 0.0 {
        return Err(invalid("расстояние soft particles некорректно"));
    }
    if let Some(collision) = desc.collision
        && (!(0.0..=1.0).contains(&collision.restitution)
            || !(0.0..=1.0).contains(&collision.friction)
            || !collision.thickness.is_finite()
            || collision.thickness < 0.0)
    {
        return Err(invalid("параметры столкновения частиц некорректны"));
    }
    Ok(())
}

fn validate_curve_times(times: impl Iterator<Item = f32>) -> Result<(), RenderError> {
    let mut previous = None::<f32>;
    let mut count = 0usize;
    for time in times {
        count += 1;
        if !time.is_finite()
            || !(0.0..=1.0).contains(&time)
            || previous.is_some_and(|before| time <= before)
        {
            return Err(invalid(
                "ключи кривой частиц должны возрастать во времени в [0, 1]",
            ));
        }
        previous = Some(time);
    }
    if count > MAX_PARTICLE_CURVE_KEYS {
        return Err(invalid("кривая частиц содержит больше четырёх ключей"));
    }
    Ok(())
}

fn validate_color_keys(keys: &[ParticleColorKey]) -> Result<(), RenderError> {
    validate_curve_times(keys.iter().map(|key| key.time))?;
    for key in keys {
        let [red, green, blue, alpha] = key.color;
        if [red, green, blue]
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
            || !(0.0..=1.0).contains(&alpha)
        {
            return Err(invalid("цвет частиц вне диапазона"));
        }
    }
    Ok(())
}

fn validate_size_keys(keys: &[ParticleSizeKey]) -> Result<(), RenderError> {
    validate_curve_times(keys.iter().map(|key| key.time))?;
    if keys
        .iter()
        .any(|key| !key.size.is_finite() || key.size < 0.0)
    {
        return Err(invalid("размер частиц по времени жизни некорректен"));
    }
    Ok(())
}

/// Выровненный по осям ограничивающий объём в мировых координатах.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleAabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl ParticleAabb {
    pub fn union(self, other: Self) -> Self {
        Self {
            min: std::array::from_fn(|axis| self.min[axis].min(other.min[axis])),
            max: std::array::from_fn(|axis| self.max[axis].max(other.max[axis])),
        }
    }

    pub fn expanded(self, margin: f32) -> Self {
        Self {
            min: self.min.map(|value| value - margin),
            max: self.max.map(|value| value + margin),
        }
    }

    /// AABB восьми углов, преобразованных матрицей (столбцы — оси).
    pub fn transformed(self, matrix: [[f32; 4]; 4]) -> Self {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for corner in 0..8u32 {
            let point = [0, 1, 2].map(|axis| {
                if corner & (1 << axis) == 0 {
                    self.min[axis]
                } else {
                    self.max[axis]
                }
            });
            let world: [f32; 3] = std::array::from_fn(|row| {
                matrix[0][row] * point[0]
                    + matrix[1][row] * point[1]
                    + matrix[2][row] * point[2]
                    + matrix[3][row]
            });
            for axis in 0..3 {
                min[axis] = min[axis].min(world[axis]);
                max[axis] = max[axis].max(world[axis]);
            }
        }
        Self { min, max }
    }

    /// Центр и радиус описанной сферы для culling.
    pub fn bounding_sphere(self) -> ([f32; 3], f32) {
        let center = std::array::from_fn(|axis| (self.min[axis] + self.max[axis]) * 0.5);
        let radius = (0..3)
            .map(|axis| (self.max[axis] - self.min[axis]) * 0.5)
            .map(|half| half * half)
            .sum::<f32>()
            .sqrt();
        (center, radius)
    }
}

/// Локальный AABB области рождения; конус и бокс вытянуты вдоль +Y.
pub fn shape_bounds(shape: ParticleShape) -> ParticleAabb {
    match shape {
        ParticleShape::Point => ParticleAabb {
            min: [0.0; 3],
            max: [0.0; 3],
        },
        ParticleShape::Sphere { radius } => ParticleAabb {
            min: [-radius; 3],
            max: [radius; 3],
        },
        ParticleShape::Cone { radius, .. } => ParticleAabb {
            min: [-radius, 0.0, -radius],
            max: [radius, 0.0, radius],
        },
        ParticleShape::Box { half_extents } => ParticleAabb {
            min: half_extents.map(|value| -value),
            max: half_extents,
        },
    }
}

/// Запас bounds на движение и размер частиц: `speed·T + ½|g|·T² + thickness + размер квада`.
///
/// Скорость рождения умножается на линейную часть `transform` (`particle_spawn.comp`), поэтому её
/// масштабирует норма этой матрицы. Берётся оценка сверху спектральной нормы `√(‖M‖₁·‖M‖∞)`
/// (максимумы сумм модулей по столбцам и строкам): точна для равномерного масштаба и корректна при сдвиге.
/// Гравитация и толщина коллизии заданы в мире. Квад шириной `size·c` имеет полудиагональ `size·c·√2/2`,
/// где `c` — максимум кривой размера, но не меньше 1 (вне ключей размер множится на 1).
pub fn motion_margin(desc: &ParticleEmitterDesc, transform: [[f32; 4]; 4]) -> f32 {
    let lifetime = desc.lifetime[1];
    let max_column_sum = (0..3)
        .map(|column| (0..3).map(|row| transform[column][row].abs()).sum::<f32>())
        .fold(0.0_f32, f32::max);
    let max_row_sum = (0..3)
        .map(|row| {
            (0..3)
                .map(|column| transform[column][row].abs())
                .sum::<f32>()
        })
        .fold(0.0_f32, f32::max);
    let linear_norm = (max_column_sum * max_row_sum).sqrt();
    let speed = desc.speed[1] * linear_norm;
    let gravity = desc
        .gravity
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    let thickness = desc.collision.map_or(0.0, |collision| collision.thickness);
    let size_scale = desc
        .size_over_life
        .iter()
        .map(|key| key.size)
        .fold(1.0_f32, f32::max);
    let half_diagonal = desc.size[1] * size_scale * std::f32::consts::FRAC_1_SQRT_2;
    speed * lifetime + 0.5 * gravity * lifetime * lifetime + thickness + half_diagonal
}

/// Batch частиц, рождённых одним спавном. Слоты batch-а не освобождаются до `retire_time`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleBatch {
    pub first_serial: u64,
    pub count: u32,
    /// Конец шага рождения + максимальное время жизни: к этому моменту все частицы мертвы.
    pub retire_time: f64,
    pub origin: ParticleAabb,
    /// Запас движения, посчитанный при рождении: масштаб эмиттера мог измениться позже.
    pub margin: f32,
}

/// Состояние эмиттера: кольцевой учёт, время симуляции и дробный перенос эмиссии.
///
/// Частица с серийным номером `s` живёт в слоте `s % capacity`. Живые частицы — серии
/// `[oldest, head)`, не длиннее `capacity`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParticleEmitterState {
    head: u64,
    batches: VecDeque<ParticleBatch>,
    time: f64,
    rate_remainder: f64,
    /// Ручной выброс, ожидающий следующего шага; рождается даже при выключенной эмиссии.
    queued_burst: u32,
}

impl ParticleEmitterState {
    /// Ставит одноразовый выброс `count` частиц на следующий шаг симуляции.
    pub fn queue_burst(&mut self, count: u32) {
        self.queued_burst = self.queued_burst.saturating_add(count);
    }
}

impl ParticleEmitterState {
    #[cfg(test)]
    pub fn time(&self) -> f64 {
        self.time
    }

    #[cfg(test)]
    pub fn head(&self) -> u64 {
        self.head
    }

    pub fn oldest_serial(&self) -> u64 {
        self.batches
            .front()
            .map_or(self.head, |batch| batch.first_serial)
    }

    /// Число слотов, занятых живыми или ещё не освобождёнными частицами.
    pub fn occupied(&self) -> u32 {
        (self.head - self.oldest_serial()) as u32
    }

    #[cfg(test)]
    pub fn batches(&self) -> impl Iterator<Item = &ParticleBatch> {
        self.batches.iter()
    }

    /// Освобождает batch-и, чей `retire_time` уже наступил. Порядок FIFO: `retire_time` монотонен.
    fn retire(&mut self, now: f64) {
        while self
            .batches
            .front()
            .is_some_and(|batch| batch.retire_time <= now)
        {
            self.batches.pop_front();
        }
    }

    /// Строит план шага. Сам `self` не меняется: план коммитится через [`ParticleStepPlan::next`].
    ///
    /// `step == 0` означает «только отрисовка»: план содержит текущее состояние без спавна и симуляции.
    pub fn plan_step(&self, input: &StepInput<'_>) -> ParticleStepPlan {
        let mut next = self.clone();
        let mut spawns = Vec::new();
        let step = input.step.clamp(0.0, MAX_PARTICLE_STEP);
        if step > 0.0 {
            let start = self.time;
            let end = start + f64::from(step);
            next.retire(start);
            let mut available = input.capacity.saturating_sub(next.occupied());
            let retire_time = end + f64::from(input.lifetime_max);

            // Ручной выброс берётся в любом случае; не умещающийся в буфер выброс отбрасывается.
            let manual = std::mem::take(&mut next.queued_burst);
            let accepted = manual.min(available);
            if accepted > 0 {
                spawns.push(next.push_batch(
                    input.capacity,
                    accepted,
                    PreAge::Fixed(0.0),
                    input.origin,
                    retire_time,
                    input.motion_margin,
                ));
                available -= accepted;
            }

            if input.emitting {
                for (time, count) in
                    burst_firings(input.bursts, input.duration, input.looping, start, end)
                {
                    let accepted = count.min(available);
                    if accepted == 0 {
                        continue;
                    }
                    spawns.push(next.push_batch(
                        input.capacity,
                        accepted,
                        PreAge::Fixed((end - time) as f32),
                        input.origin,
                        retire_time,
                        input.motion_margin,
                    ));
                    available -= accepted;
                }

                let active = input.looping || start < f64::from(input.duration);
                if active && input.rate > 0.0 {
                    let pending = next.rate_remainder + f64::from(input.rate) * f64::from(step);
                    let whole = pending.floor();
                    next.rate_remainder = pending - whole;
                    let wanted = whole as u32;
                    let accepted = wanted.min(available);
                    if accepted > 0 {
                        spawns.push(next.push_batch(
                            input.capacity,
                            accepted,
                            PreAge::Random,
                            input.origin,
                            retire_time,
                            input.motion_margin,
                        ));
                    }
                } else {
                    next.rate_remainder = 0.0;
                }
            } else {
                next.rate_remainder = 0.0;
            }
            next.time = end;
        }

        let oldest = next.oldest_serial();
        let draw_first = (oldest % u64::from(input.capacity)) as u32;
        let draw_count = next.occupied();
        let bounds = next
            .batches
            .iter()
            .map(|batch| batch.origin.expanded(batch.margin))
            .reduce(ParticleAabb::union);

        ParticleStepPlan {
            spawns,
            next,
            sim_step: step,
            draw_first,
            draw_count,
            bounds,
        }
    }

    fn push_batch(
        &mut self,
        capacity: u32,
        count: u32,
        pre_age: PreAge,
        origin: ParticleAabb,
        retire_time: f64,
        margin: f32,
    ) -> SpawnBatch {
        let first_serial = self.head;
        self.batches.push_back(ParticleBatch {
            first_serial,
            count,
            retire_time,
            origin,
            margin,
        });
        self.head += u64::from(count);
        SpawnBatch {
            first_serial,
            first_slot: (first_serial % u64::from(capacity)) as u32,
            count,
            pre_age,
            origin,
        }
    }
}

/// Вход планировщика шага. Все значения берутся из описания эмиттера и текущего transform.
pub struct StepInput<'a> {
    pub capacity: u32,
    pub emitting: bool,
    pub rate: f32,
    pub bursts: &'a [ParticleBurst],
    pub duration: f32,
    pub looping: bool,
    /// Максимальное время жизни: `desc.lifetime[1]`.
    pub lifetime_max: f32,
    /// Мировой AABB области рождения на время этого шага.
    pub origin: ParticleAabb,
    /// Запас bounds, см. [`motion_margin`].
    pub motion_margin: f32,
    /// Шаг симуляции в секундах; 0 — только отрисовка.
    pub step: f32,
}

/// Как выставить возраст новой частицы относительно конца шага.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PreAge {
    /// Рождение равномерно распределено внутри шага.
    Random,
    /// Рождение в фиксированный момент шага; значение — возраст на конце шага.
    Fixed(f32),
}

/// Пакет спавна: `count` частиц в слотах `(first_slot + k) % capacity`, серии `first_serial + k`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpawnBatch {
    pub first_serial: u64,
    pub first_slot: u32,
    pub count: u32,
    pub pre_age: PreAge,
    pub origin: ParticleAabb,
}

/// Результат шага: что спавнить, состояние после шага (для коммита) и диапазон отрисовки.
#[derive(Clone, Debug, PartialEq)]
pub struct ParticleStepPlan {
    pub spawns: Vec<SpawnBatch>,
    pub next: ParticleEmitterState,
    /// Шаг симуляции в секундах, после обрезки; 0 — симуляции нет.
    pub sim_step: f32,
    /// Первый слот кольцевого отрезка живых частиц.
    pub draw_first: u32,
    /// Число живых+занятых слотов в отрезке (≤ capacity).
    pub draw_count: u32,
    /// Консервативный мировой AABB для culling; `None`, если батчей нет.
    pub bounds: Option<ParticleAabb>,
}

/// Значения `burst.time + k·duration`, попавшие в `[start, end)`, с числом частиц.
fn burst_firings(
    bursts: &[ParticleBurst],
    duration: f32,
    looping: bool,
    start: f64,
    end: f64,
) -> Vec<(f64, u32)> {
    let duration = f64::from(duration);
    let mut firings = Vec::new();
    for burst in bursts {
        let offset = f64::from(burst.time);
        if looping {
            let first = ((start - offset) / duration).floor() as i64;
            let last = ((end - offset) / duration).ceil() as i64;
            for cycle in first..=last {
                let time = offset + cycle as f64 * duration;
                if time >= start && time < end {
                    firings.push((time, burst.count));
                }
            }
        } else if offset >= start && offset < end {
            firings.push((offset, burst.count));
        }
    }
    firings
}

/// Выбирает единственного владельца симуляции эмиттера за кадр.
///
/// Владелец — первый реально записанный вид с `step > 0`. Вид, который не был записан,
/// сюда не попадает, поэтому следующий записанный вид получает право симуляции.
#[derive(Debug)]
pub struct ParticleSimOwners<K> {
    claimed: HashSet<K>,
}

impl<K> Default for ParticleSimOwners<K> {
    fn default() -> Self {
        Self {
            claimed: HashSet::new(),
        }
    }
}

impl<K: Copy + Eq + Hash> ParticleSimOwners<K> {
    /// Возвращает `true`, если этот вид должен симулировать эмиттер.
    pub fn claim(&mut self, emitter: K, step: f32) -> bool {
        step > 0.0 && self.claimed.insert(emitter)
    }
}

/// CPU-эталон RNG шейдера (проверяется тестами): детерминированный хеш PCG (вход — `(seed, serial, channel)` свёрнутые в u32).
#[cfg(test)]
pub fn pcg_hash(input: u32) -> u32 {
    let state = input.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28).wrapping_add(4))) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// Случайное число в [0, 1) для частицы: зависит только от seed эмиттера, серии и канала.
#[cfg(test)]
pub fn particle_random(seed: u32, serial: u64, channel: u32) -> f32 {
    let serial_fold = (serial as u32) ^ ((serial >> 32) as u32).wrapping_mul(0x9E37_79B9);
    let hashed = pcg_hash(pcg_hash(seed ^ serial_fold) ^ channel.wrapping_mul(0x85EB_CA6B));
    hashed as f32 / 4_294_967_296.0
}

/// Один цветовой или размерный ключ в GPU-формате (времена упакованы в vec4).
const GPU_CURVE_KEYS: usize = MAX_PARTICLE_CURVE_KEYS;

/// Параметры эмиттера в UBO (набор 1, binding 0). Все поля — vec4/uvec4, без смещений std140.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmitterParamsGpu {
    /// (kind, radius, cone angle, cone radius); kind: 0 point, 1 sphere, 2 cone, 3 box.
    pub shape: [f32; 4],
    /// Полуразмеры бокса xyz.
    pub box_extents: [f32; 4],
    /// (lifetime min, lifetime max, speed min, speed max).
    pub lifetime_speed: [f32; 4],
    /// (size min, size max, rotation min, rotation max).
    pub size_rotation: [f32; 4],
    /// (angular velocity min, max, drag, soft distance).
    pub angular_drag: [f32; 4],
    /// Гравитация xyz.
    pub gravity: [f32; 4],
    pub color_times: [f32; 4],
    pub color_values: [[f32; 4]; GPU_CURVE_KEYS],
    pub size_times: [f32; 4],
    pub size_values: [f32; 4],
    /// (columns, rows, frames per second, 0).
    pub flipbook: [f32; 4],
    /// (restitution, friction, thickness, collision enabled 0/1).
    pub collision: [f32; 4],
    /// (color key count, size key count, blend 0 additive / 1 alpha, texture 0/1).
    pub counts: [u32; 4],
}

/// Собирает UBO-параметры эмиттера из описания. Описание должно пройти [`validate_desc`].
pub fn emitter_params(desc: &ParticleEmitterDesc, has_texture: bool) -> EmitterParamsGpu {
    let (kind, shape_a, shape_b, box_extents) = match desc.shape {
        ParticleShape::Point => (0.0, 0.0, 0.0, [0.0; 4]),
        ParticleShape::Sphere { radius } => (1.0, radius, 0.0, [0.0; 4]),
        ParticleShape::Cone { angle, radius } => (2.0, angle, radius, [0.0; 4]),
        ParticleShape::Box { half_extents } => (
            3.0,
            0.0,
            0.0,
            [half_extents[0], half_extents[1], half_extents[2], 0.0],
        ),
    };
    let mut color_times = [0.0; 4];
    let mut color_values = [[0.0; 4]; GPU_CURVE_KEYS];
    for (index, key) in desc.color_over_life.iter().enumerate() {
        color_times[index] = key.time;
        color_values[index] = key.color;
    }
    let mut size_times = [0.0; 4];
    let mut size_values = [0.0; 4];
    for (index, key) in desc.size_over_life.iter().enumerate() {
        size_times[index] = key.time;
        size_values[index] = key.size;
    }
    let collision = desc.collision.map_or([0.0; 4], |collision| {
        [
            collision.restitution,
            collision.friction,
            collision.thickness,
            1.0,
        ]
    });
    EmitterParamsGpu {
        shape: [kind, shape_a, shape_b, 0.0],
        box_extents,
        lifetime_speed: [
            desc.lifetime[0],
            desc.lifetime[1],
            desc.speed[0],
            desc.speed[1],
        ],
        size_rotation: [
            desc.size[0],
            desc.size[1],
            desc.rotation[0],
            desc.rotation[1],
        ],
        angular_drag: [
            desc.angular_velocity[0],
            desc.angular_velocity[1],
            desc.drag,
            desc.soft_distance,
        ],
        gravity: [desc.gravity[0], desc.gravity[1], desc.gravity[2], 0.0],
        color_times,
        color_values,
        size_times,
        size_values,
        flipbook: [
            desc.flipbook.columns as f32,
            desc.flipbook.rows as f32,
            desc.flipbook.frames_per_second,
            0.0,
        ],
        collision,
        counts: [
            desc.color_over_life.len() as u32,
            desc.size_over_life.len() as u32,
            match desc.blend {
                ParticleBlend::Additive => 0,
                ParticleBlend::Alpha => 1,
            },
            u32::from(has_texture),
        ],
    }
}

/// Частица в SSBO: три vec4 (48 байт). Поля соответствуют `particle_*.glsl`.
#[cfg(test)]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ParticleGpu {
    /// xyz — позиция, w — возраст.
    pub position_age: [f32; 4],
    /// xyz — скорость, w — время жизни.
    pub velocity_life: [f32; 4],
    /// x — ширина, y — поворот, z — угловая скорость, w — случайное значение для flipbook.
    pub params: [f32; 4],
}

/// Push constants спавна (`particle_spawn.comp`), 112 байт.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpawnPushGpu {
    /// Мировая матрица эмиттера, столбцы.
    pub transform: [[f32; 4]; 4],
    pub first_slot: u32,
    pub count: u32,
    pub serial_low: u32,
    pub serial_high: u32,
    pub capacity: u32,
    /// Шаг симуляции (dt).
    pub step: f32,
    /// Возраст на конце шага при `fixed_pre_age == 1`.
    pub fixed_age: f32,
    /// 0 — случайный pre-age в [0, step), 1 — `fixed_age`.
    pub fixed_pre_age: u32,
    pub seed: u32,
    /// Выравнивание до 16 байт.
    pub padding: [u32; 3],
}

/// Размер блока, который сортирует `particle_sort.comp` в shared-памяти: два элемента на поток.
pub const SORT_BLOCK_SIZE: u32 = 128;

/// Push constants сортировки (`particle_sort.comp`), 32 байта.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortPushGpu {
    /// 0 — ключи, 1 — локальная сортировка блока, 2 — локальное слияние стадии `k`, 3 — глобальный шаг `(k, j)`.
    pub mode: u32,
    pub k: u32,
    pub j: u32,
    /// Число ключей (степень двойки).
    pub count: u32,
    pub draw_first: u32,
    pub draw_count: u32,
    pub capacity: u32,
    pub padding: u32,
}

/// Проход сортировки: режим, параметры стадии и число workgroup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortPass {
    pub mode: u32,
    pub k: u32,
    pub j: u32,
    pub groups: u32,
}

/// Степень двойки для сортировки `draw_count` ключей (не меньше одного).
pub fn sort_count(draw_count: u32) -> u32 {
    draw_count.max(1).next_power_of_two()
}

/// Последовательность проходов bitonic-сети для `count` ключей (степень двойки, `count ≥ 1`).
///
/// Сначала ключи строятся, затем блоки сортируются в shared-памяти. Стадии `k > SORT_BLOCK_SIZE`
/// выполняют глобальные шаги для `j ≥ SORT_BLOCK_SIZE` и локальное слияние для меньших `j`.
pub fn sort_passes(count: u32) -> Vec<SortPass> {
    debug_assert!(count.is_power_of_two());
    let local = PARTICLE_SORT_THREADS;
    let block_groups = count.div_ceil(SORT_BLOCK_SIZE);
    let mut passes = vec![
        SortPass {
            mode: 0,
            k: 0,
            j: 0,
            groups: count.div_ceil(local),
        },
        SortPass {
            mode: 1,
            k: 0,
            j: 0,
            groups: block_groups,
        },
    ];
    let mut k = 2 * SORT_BLOCK_SIZE;
    while k <= count {
        let mut j = k / 2;
        while j >= SORT_BLOCK_SIZE {
            passes.push(SortPass {
                mode: 3,
                k,
                j,
                groups: (count / 2).div_ceil(local),
            });
            j /= 2;
        }
        passes.push(SortPass {
            mode: 2,
            k,
            j: 0,
            groups: block_groups,
        });
        k *= 2;
    }
    passes
}

/// Потоки в workgroup сортировки; совпадает с `PARTICLE_COMPUTE_LOCAL_SIZE`.
const PARTICLE_SORT_THREADS: u32 = crate::PARTICLE_COMPUTE_LOCAL_SIZE;
/// Блок сортировки в shared-памяти — два элемента на поток: CPU-эталон и шейдер используют один BLOCK.
const _: () = assert!(SORT_BLOCK_SIZE == 2 * PARTICLE_SORT_THREADS);

#[cfg(test)]
/// Ключ сортировки назад-вперёд: по возрастанию первыми идут самые дальние. Мёртвые — максимум.
pub fn sort_key(view_depth: f32, alive: bool) -> u32 {
    if !alive {
        return u32::MAX;
    }
    u32::MAX - view_depth.max(0.0).to_bits()
}

/// Push constants симуляции и отрисовки (диапазон слотов, dt и флаги).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimPushGpu {
    pub step: f32,
    pub draw_first: u32,
    pub draw_count: u32,
    pub capacity: u32,
    /// Размер кадра в пикселях: нужен для проекции частицы в depth буфер.
    pub render_width: f32,
    pub render_height: f32,
    pub padding: [u32; 2],
}

/// Push constants вершинного и фрагментного шейдеров (`particle.vert`/`particle.frag`), 32 байта.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VertexPushGpu {
    pub draw_first: u32,
    pub capacity: u32,
    pub draw_count: u32,
    /// 1 — индекс берётся из буфера сортировки (alpha), 0 — из порядка instance.
    pub sorted: u32,
    pub render_width: f32,
    pub render_height: f32,
    pub padding: [u32; 2],
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    fn step_input<'a>(bursts: &'a [ParticleBurst]) -> StepInput<'a> {
        StepInput {
            capacity: 8,
            emitting: true,
            rate: 0.0,
            bursts,
            duration: 1.0,
            looping: true,
            lifetime_max: 0.5,
            origin: ParticleAabb {
                min: [0.0; 3],
                max: [0.0; 3],
            },
            motion_margin: 0.0,
            step: 0.1,
        }
    }

    #[test]
    fn queued_burst_spawns_once_and_ignores_the_emitting_flag() {
        let mut state = ParticleEmitterState::default();
        state.queue_burst(3);
        let mut input = step_input(&[]);
        input.emitting = false;
        let plan = commit(&mut state, &input);
        assert_eq!(plan.spawns.iter().map(|spawn| spawn.count).sum::<u32>(), 3);
        let again = commit(&mut state, &input);
        assert!(again.spawns.is_empty(), "выброс берётся один раз");
    }

    #[test]
    fn queued_burst_waits_for_a_simulation_step_and_is_clamped_by_capacity() {
        let mut state = ParticleEmitterState::default();
        state.queue_burst(20);
        let mut input = step_input(&[]);
        input.capacity = 8;
        input.step = 0.0;
        assert!(
            commit(&mut state, &input).spawns.is_empty(),
            "без шага выброс ждёт"
        );
        input.step = 0.1;
        let plan = commit(&mut state, &input);
        assert_eq!(plan.spawns.iter().map(|spawn| spawn.count).sum::<u32>(), 8);
    }

    fn commit(state: &mut ParticleEmitterState, input: &StepInput<'_>) -> ParticleStepPlan {
        let plan = state.plan_step(input);
        *state = plan.next.clone();
        plan
    }

    fn live_slots(state: &ParticleEmitterState, capacity: u32) -> Vec<u32> {
        (state.oldest_serial()..state.head())
            .map(|serial| (serial % u64::from(capacity)) as u32)
            .collect()
    }

    #[test]
    fn default_desc_is_valid() {
        assert!(validate_desc(&ParticleEmitterDesc::default()).is_ok());
    }

    #[test]
    fn capacity_limits_depend_on_blend() {
        let mut desc = ParticleEmitterDesc {
            capacity: MAX_ALPHA_PARTICLE_CAPACITY + 1,
            ..ParticleEmitterDesc::default()
        };
        assert!(validate_desc(&desc).is_ok());
        desc.blend = ParticleBlend::Alpha;
        assert!(validate_desc(&desc).is_err());
        desc.capacity = MAX_ALPHA_PARTICLE_CAPACITY;
        assert!(validate_desc(&desc).is_ok());
        desc.capacity = 0;
        assert!(validate_desc(&desc).is_err());
    }

    #[test]
    fn invalid_desc_fields_are_rejected() {
        let base = ParticleEmitterDesc::default();
        let cases = [
            ParticleEmitterDesc {
                rate: f32::NAN,
                ..base.clone()
            },
            ParticleEmitterDesc {
                duration: 0.0,
                ..base.clone()
            },
            ParticleEmitterDesc {
                lifetime: [0.0, 1.0],
                ..base.clone()
            },
            ParticleEmitterDesc {
                lifetime: [2.0, 1.0],
                ..base.clone()
            },
            ParticleEmitterDesc {
                speed: [-1.0, 1.0],
                ..base.clone()
            },
            ParticleEmitterDesc {
                size: [0.5, 0.1],
                ..base.clone()
            },
            ParticleEmitterDesc {
                drag: -0.1,
                ..base.clone()
            },
            ParticleEmitterDesc {
                soft_distance: f32::INFINITY,
                ..base.clone()
            },
            ParticleEmitterDesc {
                shape: ParticleShape::Sphere { radius: -1.0 },
                ..base.clone()
            },
            ParticleEmitterDesc {
                shape: ParticleShape::Cone {
                    angle: 2.0,
                    radius: 0.0,
                },
                ..base.clone()
            },
            ParticleEmitterDesc {
                flipbook: ParticleFlipbook {
                    columns: 17,
                    rows: 1,
                    frames_per_second: 0.0,
                },
                ..base.clone()
            },
            ParticleEmitterDesc {
                collision: Some(ParticleCollision {
                    restitution: 1.5,
                    friction: 0.0,
                    thickness: 0.0,
                }),
                ..base.clone()
            },
            ParticleEmitterDesc {
                bursts: vec![ParticleBurst {
                    time: 1.0,
                    count: 4,
                }],
                ..base.clone()
            },
            ParticleEmitterDesc {
                bursts: vec![ParticleBurst {
                    time: 0.2,
                    count: 0,
                }],
                ..base.clone()
            },
        ];
        for desc in cases {
            assert!(
                matches!(validate_desc(&desc), Err(RenderError::InvalidConfig(_))),
                "{desc:?} must be rejected"
            );
        }
    }

    #[test]
    fn curve_keys_must_be_sorted_bounded_and_limited() {
        let color = |time, alpha| ParticleColorKey {
            time,
            color: [1.0, 1.0, 1.0, alpha],
        };
        let with_color = |keys: Vec<ParticleColorKey>| ParticleEmitterDesc {
            color_over_life: keys,
            ..ParticleEmitterDesc::default()
        };
        assert!(validate_desc(&with_color(vec![color(0.0, 1.0), color(1.0, 0.0)])).is_ok());
        assert!(validate_desc(&with_color(vec![color(0.5, 1.0), color(0.2, 0.0)])).is_err());
        assert!(validate_desc(&with_color(vec![color(0.0, 1.2)])).is_err());
        assert!(validate_desc(&with_color(vec![color(1.5, 1.0)])).is_err());
        let five = (0..5).map(|index| color(index as f32 / 5.0, 1.0)).collect();
        assert!(validate_desc(&with_color(five)).is_err());
        let sizes = ParticleEmitterDesc {
            size_over_life: vec![ParticleSizeKey {
                time: 0.0,
                size: -1.0,
            }],
            ..ParticleEmitterDesc::default()
        };
        assert!(validate_desc(&sizes).is_err());
    }

    #[test]
    fn ring_never_overwrites_live_slots() {
        let capacity = 8;
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.rate = 30.0;
        input.lifetime_max = 0.5;
        for _ in 0..40 {
            // Батчи, освобождаемые в начале шага, уже мертвы: их слоты допустимы для спавна.
            let mut survivors = state.clone();
            survivors.retire(state.time());
            let before = live_slots(&survivors, capacity);
            let plan = commit(&mut state, &input);
            for spawn in &plan.spawns {
                for k in 0..spawn.count {
                    let slot = (spawn.first_slot + k) % capacity;
                    assert!(
                        !before.contains(&slot),
                        "slot {slot} was live before spawn at time {}",
                        state.time()
                    );
                }
            }
            assert!(state.occupied() <= capacity);
            assert!(plan.draw_count <= capacity);
        }
    }

    #[test]
    fn slots_are_reused_only_after_batch_retires() {
        let capacity = 4;
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.rate = 40.0;
        input.lifetime_max = 0.3;
        let first = commit(&mut state, &input);
        assert_eq!(
            first.spawns.iter().map(|spawn| spawn.count).sum::<u32>(),
            capacity
        );
        input.rate = 0.0;
        input.emitting = false;
        let blocked = commit(&mut state, &input);
        assert!(blocked.spawns.is_empty());
        assert_eq!(state.occupied(), capacity);
        for _ in 0..10 {
            if state.occupied() == 0 {
                break;
            }
            commit(&mut state, &input);
        }
        assert_eq!(state.occupied(), 0, "batches retire after their lifetime");
    }

    #[test]
    fn capacity_exhaustion_drops_new_particles() {
        let mut state = ParticleEmitterState::default();
        let bursts = [ParticleBurst {
            time: 0.0,
            count: 6,
        }];
        let mut input = step_input(&bursts);
        input.capacity = 4;
        input.lifetime_max = 10.0;
        let plan = commit(&mut state, &input);
        assert_eq!(plan.spawns.iter().map(|spawn| spawn.count).sum::<u32>(), 4);
        assert_eq!(state.occupied(), 4);
        let dropped = commit(&mut state, &input);
        assert!(dropped.spawns.is_empty());
        assert_eq!(state.head(), 4);
    }

    #[test]
    fn draw_range_wraps_and_covers_only_live_particles() {
        let capacity = 4;
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.capacity = capacity;
        input.rate = 10.0;
        input.lifetime_max = 0.15;
        commit(&mut state, &input);
        input.rate = 0.0;
        let mut seen_wrap = false;
        for _ in 0..10 {
            let plan = commit(&mut state, &input);
            assert_eq!(plan.draw_count, state.occupied());
            assert!(plan.draw_first < capacity);
            if u64::from(plan.draw_first) + u64::from(plan.draw_count) > u64::from(capacity) {
                seen_wrap = true;
            }
        }
        assert!(seen_wrap || state.occupied() == 0);
    }

    #[test]
    fn short_batch_behind_long_batch_waits_for_fifo_retirement() {
        // Кольцо FIFO: короткий batch за долгим не освобождается раньше него, иначе слоты не непрерывны.
        let capacity = 4;
        let mut state = ParticleEmitterState::default();
        let mut long = step_input(&[ParticleBurst {
            time: 0.0,
            count: 2,
        }]);
        long.capacity = capacity;
        long.lifetime_max = 5.0;
        commit(&mut state, &long);
        let mut short = step_input(&[]);
        short.capacity = capacity;
        short.rate = 20.0;
        short.lifetime_max = 0.1;
        commit(&mut state, &short);
        let mut idle = step_input(&[]);
        idle.capacity = capacity;
        idle.emitting = false;
        for _ in 0..20 {
            commit(&mut state, &idle);
        }
        assert_eq!(
            state.occupied(),
            4,
            "short batch stays blocked behind the long batch"
        );
        assert_eq!(state.batches().count(), 2);
        for _ in 0..60 {
            if state.occupied() == 0 {
                break;
            }
            commit(&mut state, &idle);
        }
        assert_eq!(
            state.occupied(),
            0,
            "both batches free once the long batch retires"
        );
    }

    #[test]
    fn rate_carries_fractional_particles() {
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.capacity = 1024;
        input.rate = 2.5;
        input.lifetime_max = 100.0;
        for _ in 0..10 {
            commit(&mut state, &input);
        }
        assert_eq!(
            state.head(),
            2,
            "10 steps of 0.1 s at 2.5/s make exactly 2.5 particles"
        );
    }

    #[test]
    fn zero_step_only_draws_and_does_not_mutate() {
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.rate = 20.0;
        input.lifetime_max = 5.0;
        input.capacity = 64;
        commit(&mut state, &input);
        let before = state.clone();
        input.step = 0.0;
        let plan = state.plan_step(&input);
        assert!(plan.spawns.is_empty());
        assert_eq!(plan.sim_step, 0.0);
        assert_eq!(plan.next, before);
        assert_eq!(state, before);
    }

    #[test]
    fn step_is_clamped_to_maximum() {
        let state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.step = 5.0;
        let plan = state.plan_step(&input);
        assert_eq!(plan.sim_step, MAX_PARTICLE_STEP);
        assert!((plan.next.time() - f64::from(MAX_PARTICLE_STEP)).abs() < 1e-9);
    }

    #[test]
    fn paused_emitter_does_not_spawn_but_still_simulates() {
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[ParticleBurst {
            time: 0.0,
            count: 3,
        }]);
        input.emitting = false;
        input.rate = 100.0;
        let plan = commit(&mut state, &input);
        assert!(plan.spawns.is_empty());
        assert_eq!(plan.sim_step, 0.1);
        assert!(state.time() > 0.0);
    }

    #[test]
    fn bursts_fire_once_per_cycle_when_looping() {
        let bursts = [ParticleBurst {
            time: 0.25,
            count: 3,
        }];
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&bursts);
        input.capacity = 1024;
        input.lifetime_max = 100.0;
        input.duration = 1.0;
        let mut total = 0;
        for _ in 0..25 {
            total += commit(&mut state, &input)
                .spawns
                .iter()
                .map(|spawn| spawn.count)
                .sum::<u32>();
        }
        assert_eq!(
            total, 9,
            "2.5 s of looping fires the 3-particle burst at 0.25, 1.25 and 2.25"
        );
    }

    #[test]
    fn non_looping_bursts_fire_once_and_rate_stops_after_duration() {
        let bursts = [ParticleBurst {
            time: 0.05,
            count: 2,
        }];
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&bursts);
        input.looping = false;
        input.duration = 0.25;
        input.rate = 100.0;
        input.capacity = 1024;
        input.lifetime_max = 100.0;
        for _ in 0..10 {
            commit(&mut state, &input);
        }
        assert_eq!(
            state.head(),
            2 + 3 * 10,
            "burst of 2, then rate 100/s on the three steps starting before 0.25 s"
        );
    }

    #[test]
    fn burst_pre_age_places_birth_at_firing_time() {
        let state = ParticleEmitterState::default();
        let bursts = [ParticleBurst {
            time: 0.07,
            count: 1,
        }];
        let input = step_input(&bursts);
        let plan = state.plan_step(&input);
        assert_eq!(plan.spawns.len(), 1);
        match plan.spawns[0].pre_age {
            PreAge::Fixed(age) => assert!((age - 0.03).abs() < 1e-6, "age {age}"),
            PreAge::Random => panic!("burst must have fixed pre-age"),
        }
    }

    #[test]
    fn bounds_are_conservative_union_of_live_origins() {
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.rate = 10.0;
        input.lifetime_max = 1.0;
        input.motion_margin = 2.0;
        input.origin = ParticleAabb {
            min: [0.0, 0.0, 0.0],
            max: [1.0, 0.0, 0.0],
        };
        commit(&mut state, &input);
        input.origin = ParticleAabb {
            min: [5.0, -1.0, 0.0],
            max: [5.0, 1.0, 0.0],
        };
        let plan = commit(&mut state, &input);
        let bounds = plan.bounds.expect("live batches produce bounds");
        assert!(bounds.min[0] <= -2.0 + 0.0 && bounds.max[0] >= 7.0 - 1e-6);
        assert!(bounds.min[1] <= -3.0 + 1e-6 && bounds.max[1] >= 3.0 - 1e-6);
    }

    #[test]
    fn empty_state_has_no_bounds() {
        let state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.emitting = false;
        assert!(state.plan_step(&input).bounds.is_none());
    }

    #[test]
    fn motion_margin_covers_speed_gravity_and_size() {
        let desc = ParticleEmitterDesc {
            lifetime: [1.0, 2.0],
            speed: [0.0, 3.0],
            size: [0.1, 1.0],
            gravity: [0.0, -4.0, 0.0],
            collision: Some(ParticleCollision {
                restitution: 0.5,
                friction: 0.5,
                thickness: 0.2,
            }),
            ..ParticleEmitterDesc::default()
        };
        let identity = IDENTITY;
        let expected = 3.0 * 2.0 + 0.5 * 4.0 * 4.0 + 0.2 + std::f32::consts::FRAC_1_SQRT_2;
        assert!((motion_margin(&desc, identity) - expected).abs() < 1e-5);
    }

    #[test]
    fn motion_margin_covers_size_curve_above_one() {
        let desc = ParticleEmitterDesc {
            speed: [0.0, 0.0],
            size: [0.1, 1.0],
            size_over_life: vec![
                ParticleSizeKey {
                    time: 0.0,
                    size: 0.5,
                },
                ParticleSizeKey {
                    time: 1.0,
                    size: 2.5,
                },
            ],
            ..ParticleEmitterDesc::default()
        };
        let expected = 2.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((motion_margin(&desc, IDENTITY) - expected).abs() < 1e-5);
    }

    #[test]
    fn motion_margin_scales_speed_by_emitter_scale_but_not_translation() {
        let desc = ParticleEmitterDesc {
            lifetime: [1.0, 2.0],
            speed: [0.0, 3.0],
            size: [0.0, 0.0],
            gravity: [0.0, -4.0, 0.0],
            ..ParticleEmitterDesc::default()
        };
        let scaled = [
            [3.0, 0.0, 0.0, 0.0],
            [0.0, 3.0, 0.0, 0.0],
            [0.0, 0.0, 3.0, 0.0],
            [7.0, 8.0, 9.0, 1.0],
        ];
        let expected = 3.0 * 3.0 * 2.0 + 0.5 * 4.0 * 4.0;
        assert!((motion_margin(&desc, scaled) - expected).abs() < 1e-4);
    }

    #[test]
    fn motion_margin_bounds_shear_without_underestimating() {
        // Спектральная норма блока [[1, 1], [0, 1]] — золотое сечение (≈1.618).
        let desc = ParticleEmitterDesc {
            lifetime: [1.0, 1.0],
            speed: [0.0, 1.0],
            ..ParticleEmitterDesc::default()
        };
        let sheared = [
            [1.0, 0.0, 0.0, 0.0],
            [1.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0; 4],
        ];
        assert!(motion_margin(&desc, sheared) >= 1.618);
    }

    #[test]
    fn batch_keeps_margin_from_its_spawn_step() {
        let mut state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.rate = 10.0;
        input.lifetime_max = 1.0;
        input.motion_margin = 2.0;
        commit(&mut state, &input);
        // Следующий шаг без спавна и с меньшим запасом (масштаб эмиттера уменьшился).
        input.emitting = false;
        input.motion_margin = 0.5;
        let plan = commit(&mut state, &input);
        let bounds = plan.bounds.expect("live batch produces bounds");
        assert!(bounds.max[0] >= 2.0 - 1e-6 && bounds.min[0] <= -2.0 + 1e-6);
    }

    #[test]
    fn aabb_transform_handles_translation_and_scale() {
        let aabb = ParticleAabb {
            min: [-1.0; 3],
            max: [1.0; 3],
        };
        let matrix = [
            [2.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [10.0, 0.0, 0.0, 1.0],
        ];
        let transformed = aabb.transformed(matrix);
        assert_eq!(transformed.min, [8.0, -1.0, -1.0]);
        assert_eq!(transformed.max, [12.0, 1.0, 1.0]);
        let (center, radius) = transformed.bounding_sphere();
        assert_eq!(center, [10.0, 0.0, 0.0]);
        assert!((radius - (4.0f32 + 1.0 + 1.0).sqrt()).abs() < 1e-6);
    }

    #[test]
    fn shape_bounds_match_shape_extents() {
        assert_eq!(
            shape_bounds(ParticleShape::Cone {
                angle: 0.3,
                radius: 2.0
            }),
            ParticleAabb {
                min: [-2.0, 0.0, -2.0],
                max: [2.0, 0.0, 2.0]
            }
        );
        assert_eq!(
            shape_bounds(ParticleShape::Box {
                half_extents: [1.0, 2.0, 3.0]
            }),
            ParticleAabb {
                min: [-1.0, -2.0, -3.0],
                max: [1.0, 2.0, 3.0]
            }
        );
    }

    #[test]
    fn sim_owner_is_first_recorded_view_with_step() {
        let mut owners = ParticleSimOwners::default();
        assert!(!owners.claim(7u32, 0.0), "zero step never simulates");
        assert!(
            owners.claim(7u32, 0.016),
            "first recorded view with step owns the emitter"
        );
        assert!(!owners.claim(7u32, 0.016), "second view only draws");
        assert!(owners.claim(8u32, 0.016), "other emitters are independent");
    }

    #[test]
    fn skipped_owner_leaves_state_for_next_view() {
        // Первый вид не записан (нет target), поэтому он не получает claim и не коммитит состояние.
        let mut owners = ParticleSimOwners::default();
        let state = ParticleEmitterState::default();
        let mut input = step_input(&[]);
        input.rate = 10.0;
        input.lifetime_max = 1.0;
        let dropped_view_plan = state.plan_step(&input);
        assert!(!dropped_view_plan.spawns.is_empty());
        assert!(
            owners.claim(1u32, input.step),
            "next recorded view simulates"
        );
        let owner_plan = state.plan_step(&input);
        assert_eq!(owner_plan.spawns, dropped_view_plan.spawns);
    }

    #[test]
    fn pcg_hash_is_deterministic_and_spreads_values() {
        assert_eq!(pcg_hash(1), pcg_hash(1));
        assert_ne!(pcg_hash(1), pcg_hash(2));
        let samples: Vec<f32> = (0..256u64)
            .map(|serial| particle_random(3, serial, 0))
            .collect();
        assert!(samples.iter().all(|value| (0.0..1.0).contains(value)));
        let mean = samples.iter().sum::<f32>() / samples.len() as f32;
        assert!((0.3..0.7).contains(&mean), "mean {mean}");
        assert_eq!(particle_random(3, 9, 2), particle_random(3, 9, 2));
        assert_ne!(particle_random(3, 9, 2), particle_random(3, 9, 3));
    }

    #[test]
    fn gpu_layouts_match_shader_contracts() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<ParticleGpu>(), 48);
        assert_eq!(offset_of!(ParticleGpu, velocity_life), 16);
        assert_eq!(offset_of!(ParticleGpu, params), 32);
        assert_eq!(size_of::<SpawnPushGpu>(), 112);
        assert!(
            size_of::<SpawnPushGpu>() <= 128,
            "guaranteed Vulkan push constant minimum"
        );
        assert_eq!(size_of::<SimPushGpu>(), 32);
        assert_eq!(size_of::<VertexPushGpu>(), 32);
        assert_eq!(size_of::<EmitterParamsGpu>() % 16, 0);
        assert_eq!(size_of::<EmitterParamsGpu>(), 256);
        assert_eq!(offset_of!(EmitterParamsGpu, color_values), 16 * 7);
    }

    #[test]
    fn emitter_params_std140_offsets_match_ubo_layout() {
        use std::mem::offset_of;
        // Все поля — vec4/uvec4; массив цветов занимает четыре слота подряд (112..176), как в `particle_*.comp`.
        let offsets = [
            offset_of!(EmitterParamsGpu, shape),
            offset_of!(EmitterParamsGpu, box_extents),
            offset_of!(EmitterParamsGpu, lifetime_speed),
            offset_of!(EmitterParamsGpu, size_rotation),
            offset_of!(EmitterParamsGpu, angular_drag),
            offset_of!(EmitterParamsGpu, gravity),
            offset_of!(EmitterParamsGpu, color_times),
            offset_of!(EmitterParamsGpu, color_values),
            offset_of!(EmitterParamsGpu, size_times),
            offset_of!(EmitterParamsGpu, size_values),
            offset_of!(EmitterParamsGpu, flipbook),
            offset_of!(EmitterParamsGpu, collision),
            offset_of!(EmitterParamsGpu, counts),
        ];
        assert_eq!(
            offsets,
            [0, 16, 32, 48, 64, 80, 96, 112, 176, 192, 208, 224, 240]
        );
        assert_eq!(size_of::<EmitterParamsGpu>(), 256);
    }

    #[test]
    fn emitter_params_pack_keys_and_flags() {
        let desc = ParticleEmitterDesc {
            shape: ParticleShape::Cone {
                angle: 0.5,
                radius: 2.0,
            },
            blend: ParticleBlend::Alpha,
            color_over_life: vec![
                ParticleColorKey {
                    time: 0.0,
                    color: [1.0, 0.0, 0.0, 1.0],
                },
                ParticleColorKey {
                    time: 1.0,
                    color: [0.0, 0.0, 1.0, 0.0],
                },
            ],
            size_over_life: vec![ParticleSizeKey {
                time: 0.5,
                size: 2.0,
            }],
            collision: Some(ParticleCollision {
                restitution: 0.4,
                friction: 0.2,
                thickness: 0.1,
            }),
            flipbook: ParticleFlipbook {
                columns: 4,
                rows: 2,
                frames_per_second: 8.0,
            },
            ..ParticleEmitterDesc::default()
        };
        let params = emitter_params(&desc, true);
        assert_eq!(params.shape, [2.0, 0.5, 2.0, 0.0]);
        assert_eq!(params.counts, [2, 1, 1, 1]);
        assert_eq!(params.color_times, [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(params.color_values[1], [0.0, 0.0, 1.0, 0.0]);
        assert_eq!(params.size_times[0], 0.5);
        assert_eq!(params.flipbook, [4.0, 2.0, 8.0, 0.0]);
        assert_eq!(params.collision, [0.4, 0.2, 0.1, 1.0]);
        let plain = emitter_params(&ParticleEmitterDesc::default(), false);
        assert_eq!(plain.counts, [0, 0, 0, 0]);
        assert_eq!(plain.collision, [0.0; 4]);
    }
}

#[cfg(test)]
mod sort_tests {
    use super::*;

    /// CPU-эталон прохода сортировки; повторяет семантику `particle_sort.comp` для ключей (key, index).
    fn apply_pass(keys: &mut [(u32, u32)], pass: SortPass) {
        let count = keys.len();
        let block = SORT_BLOCK_SIZE as usize;
        let compare = |keys: &mut [(u32, u32)], i: usize, l: usize, ascending: bool| {
            // Лексикографический порядок (ключ, индекс), как в `particle_sort.comp`: ничьи детерминированы.
            let out_of_order = if ascending {
                keys[i] > keys[l]
            } else {
                keys[i] < keys[l]
            };
            if out_of_order {
                keys.swap(i, l);
            }
        };
        match pass.mode {
            // Ключи строятся шейдером; в эталоне входные ключи заданы тестом.
            0 => {}
            1 | 2 => {
                let k_values: Vec<u32> = if pass.mode == 1 {
                    std::iter::successors(Some(2u32), |k| Some(k * 2))
                        .take_while(|k| *k <= SORT_BLOCK_SIZE)
                        .collect()
                } else {
                    vec![pass.k]
                };
                for base in (0..count).step_by(block) {
                    for &k in &k_values {
                        let mut j = if pass.mode == 1 {
                            k / 2
                        } else {
                            SORT_BLOCK_SIZE / 2
                        };
                        while j >= 1 {
                            for lane in 0..(block / 2) {
                                let j = j as usize;
                                let i = 2 * j * (lane / j) + (lane % j);
                                let l = i + j;
                                let ascending = ((base + i) as u32 & k) == 0;
                                compare(keys, base + i, base + l, ascending);
                            }
                            j /= 2;
                        }
                    }
                }
            }
            _ => {
                let j = pass.j as usize;
                for pair in 0..count / 2 {
                    let i = 2 * j * (pair / j) + (pair % j);
                    let l = i + j;
                    if l < count {
                        let ascending = (i as u32 & pass.k) == 0;
                        compare(keys, i, l, ascending);
                    }
                }
            }
        }
    }

    /// Прогоняет сеть по ключам; короткий вход дополняется сентинелами до блока, как в шейдере.
    fn run_network(keys: &mut [(u32, u32)]) {
        let live = keys.len();
        let padded_len = live.max(SORT_BLOCK_SIZE as usize);
        let mut padded = keys.to_vec();
        padded.extend((live..padded_len).map(|index| (u32::MAX, index as u32)));
        for pass in sort_passes(padded_len as u32).into_iter().skip(1) {
            apply_pass(&mut padded, pass);
        }
        keys.copy_from_slice(&padded[..live]);
    }

    fn pseudo_random_keys(len: usize, seed: u32) -> Vec<(u32, u32)> {
        let mut state = seed;
        (0..len as u32)
            .map(|index| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state, index)
            })
            .collect()
    }

    #[test]
    fn sort_count_rounds_up_to_power_of_two() {
        assert_eq!(sort_count(0), 1);
        assert_eq!(sort_count(1), 1);
        assert_eq!(sort_count(5), 8);
        assert_eq!(sort_count(256), 256);
    }

    #[test]
    fn small_counts_need_only_key_and_block_passes() {
        let modes: Vec<u32> = sort_passes(128).iter().map(|pass| pass.mode).collect();
        assert_eq!(modes, [0, 1]);
    }

    #[test]
    fn bitonic_schedule_has_global_then_merge_stages() {
        let passes = sort_passes(256);
        assert_eq!(
            passes,
            [
                SortPass {
                    mode: 0,
                    k: 0,
                    j: 0,
                    groups: 4
                },
                SortPass {
                    mode: 1,
                    k: 0,
                    j: 0,
                    groups: 2
                },
                SortPass {
                    mode: 3,
                    k: 256,
                    j: 128,
                    groups: 2
                },
                SortPass {
                    mode: 2,
                    k: 256,
                    j: 0,
                    groups: 2
                },
            ]
        );
        let count_1024 = sort_passes(1024);
        let globals = count_1024.iter().filter(|pass| pass.mode == 3).count();
        let merges = count_1024.iter().filter(|pass| pass.mode == 2).count();
        assert_eq!((globals, merges), (1 + 2 + 3, 3));
    }

    #[test]
    fn reference_network_sorts_random_keys_for_each_size() {
        for count in [2usize, 128, 256, 512, 1024] {
            let mut keys = pseudo_random_keys(count, count as u32 * 7 + 1);
            let before: Vec<u32> = {
                let mut values: Vec<u32> = keys.iter().map(|(key, _)| *key).collect();
                values.sort_unstable();
                values
            };
            run_network(&mut keys);
            assert!(
                keys.windows(2).all(|pair| pair[0].0 <= pair[1].0),
                "count {count} is not sorted"
            );
            let mut after: Vec<u32> = keys.iter().map(|(key, _)| *key).collect();
            after.sort_unstable();
            assert_eq!(after, before, "count {count} lost or duplicated keys");
        }
    }

    #[test]
    fn padding_with_sentinels_keeps_live_keys_in_front() {
        // Живые ключи — 37 штук из 64; паддинг (u32::MAX) должен оказаться в конце.
        let live = 37;
        let mut keys = pseudo_random_keys(64, 99);
        for (index, key) in keys.iter_mut().enumerate() {
            if index >= live {
                *key = (u32::MAX, index as u32);
            }
        }
        run_network(&mut keys);
        assert!(keys[..live].iter().all(|(key, _)| *key != u32::MAX));
        assert!(keys[live..].iter().all(|(key, _)| *key == u32::MAX));
    }

    #[test]
    fn depth_keys_draw_far_before_near_and_dead_last() {
        let far = sort_key(10.0, true);
        let middle = sort_key(2.0, true);
        let near = sort_key(0.5, true);
        assert!(
            far < middle && middle < near,
            "far particles must sort first"
        );
        assert_eq!(sort_key(3.0, false), u32::MAX);
        assert!(near < u32::MAX);
        assert_eq!(
            sort_key(-4.0, true),
            sort_key(0.0, true),
            "points behind the camera clamp to zero depth"
        );
    }

    /// Ключи как `write_keys`: живая частица — ключ глубины, мёртвая и паддинг — сентинел.
    /// `depths[index]` — глубина частицы в позиции `index` диапазона отрисовки (`None` — мёртвая).
    fn draw_keys(depths: &[Option<f32>], draw_count: u32) -> Vec<(u32, u32)> {
        let count = sort_count(draw_count) as usize;
        (0..count)
            .map(|index| {
                let key = match depths.get(index).copied().flatten() {
                    Some(depth) if index < draw_count as usize => sort_key(depth, true),
                    _ => u32::MAX,
                };
                (key, index as u32)
            })
            .collect()
    }

    /// Слоты, которые рисует `particle.vert` с `sorted = 1`: индекс из буфера сортировки,
    /// вершины с индексом вне диапазона отрисовки отбрасываются, мёртвые слоты не рисуются.
    fn drawn_slots(
        keys: &[(u32, u32)],
        draw_first: u32,
        draw_count: u32,
        capacity: u32,
        alive_slot: &[bool],
    ) -> Vec<u32> {
        keys[..draw_count as usize]
            .iter()
            .filter(|(_, index)| *index < draw_count)
            .map(|(_, index)| (draw_first + index) % capacity)
            .filter(|slot| alive_slot[*slot as usize])
            .collect()
    }

    fn check_sorted_draw_range(capacity: u32, draw_first: u32, draw_count: u32, dead_every: u32) {
        let mut alive_slot = vec![false; capacity as usize];
        let depths: Vec<Option<f32>> = (0..draw_count)
            .map(|index| {
                let slot = (draw_first + index) % capacity;
                let alive = index % dead_every != 0;
                alive_slot[slot as usize] = alive;
                // Глубина 0 даёт ключ `u32::MAX`, как у мёртвых и паддинга: ничьи в сортировке.
                let depth = if index % 5 == 1 {
                    0.0
                } else {
                    index as f32 * 0.37 + 0.5
                };
                alive.then_some(depth)
            })
            .collect();
        let mut keys = draw_keys(&depths, draw_count);
        run_network(&mut keys);
        let mut drawn = drawn_slots(&keys, draw_first, draw_count, capacity, &alive_slot);
        let mut expected: Vec<u32> = (0..draw_count)
            .map(|index| (draw_first + index) % capacity)
            .filter(|slot| alive_slot[*slot as usize])
            .collect();
        drawn.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            drawn, expected,
            "capacity {capacity}, first {draw_first}, count {draw_count}: each live slot drawn once"
        );
    }

    #[test]
    fn sorted_draw_range_draws_each_live_slot_once_with_padding_and_wrap() {
        // Ёмкость не степень двойки: сорт на 1024 ключей, паддинг попадает в кольцо по модулю.
        check_sorted_draw_range(1000, 700, 600, 3);
        check_sorted_draw_range(1000, 0, 512, 2);
        check_sorted_draw_range(1000, 999, 1000, 4);
        check_sorted_draw_range(777, 500, 513, 3);
    }

    #[test]
    fn network_keeps_every_index_once_for_sizes_with_sentinels() {
        for count in [64usize, 128, 256, 1024] {
            let mut keys = pseudo_random_keys(count, count as u32 * 13 + 5);
            for (index, key) in keys
                .iter_mut()
                .enumerate()
                .filter(|(index, _)| index % 4 == 3)
            {
                *key = (u32::MAX, index as u32);
            }
            run_network(&mut keys);
            assert!(
                keys.windows(2).all(|pair| pair[0].0 <= pair[1].0),
                "count {count} is not sorted"
            );
            let mut indices: Vec<u32> = keys.iter().map(|(_, index)| *index).collect();
            indices.sort_unstable();
            assert_eq!(
                indices,
                (0..count as u32).collect::<Vec<_>>(),
                "count {count}"
            );
        }
    }
}
