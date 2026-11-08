//! Компонент GPU-частиц сцены. Модуль описывает параметры эмиттера и проверяет их; симуляция и
//! отрисовка выполняются в `engine-render`, связь с ним — в `engine-runtime`.

use crate::document::{
    SceneError, option_virtual_path_serde, require_asset_path, require_extension,
};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Наибольшая ёмкость эмиттера с аддитивным смешиванием. Совпадает с `engine_render::MAX_ADDITIVE_PARTICLE_CAPACITY`;
/// согласованность проверяет тест в engine-runtime.
pub const MAX_ADDITIVE_PARTICLE_CAPACITY: u32 = 262_144;
/// Наибольшая ёмкость эмиттера с альфа-смешиванием (ограничение bitonic-сортировки).
pub const MAX_ALPHA_PARTICLE_CAPACITY: u32 = 65_536;
/// Максимум ключей кривых цвета и размера.
pub const MAX_PARTICLE_CURVE_KEYS: usize = 4;
/// Максимум строк и столбцов flipbook.
pub const MAX_FLIPBOOK_GRID: u32 = 16;

/// Смешивание частиц с кадром.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticleBlendMode {
    /// Аддитивное смешивание: огонь, искры, свет.
    #[default]
    Additive,
    /// Альфа-смешивание с сортировкой: дым, пыль.
    Alpha,
}

/// Область рождения частиц; конус и бокс вытянуты вдоль локальной оси +Y.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParticleShapeComponent {
    Point {},
    Sphere {
        radius: f32,
    },
    /// `angle` — полууглол конуса в радианах, `radius` — радиус основания.
    Cone {
        angle: f32,
        radius: f32,
    },
    Box {
        half_extents: [f32; 3],
    },
}

impl Default for ParticleShapeComponent {
    fn default() -> Self {
        Self::Point {}
    }
}

/// Разовый выброс `count` частиц в момент `time` цикла, секунды.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticleBurstComponent {
    pub time: f32,
    pub count: u32,
}

/// Ключ цвета над временем жизни; `time` в [0, 1], альфа в [0, 1].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticleColorKeyComponent {
    pub time: f32,
    pub color: [f32; 4],
}

/// Ключ размера над временем жизни; `time` в [0, 1].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticleSizeKeyComponent {
    pub time: f32,
    pub size: f32,
}

/// Анимация текстуры: сетка `columns × rows`; `frames_per_second = 0` — кадр выбирается по времени жизни.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParticleFlipbookComponent {
    pub columns: u32,
    pub rows: u32,
    pub frames_per_second: f32,
}

impl Default for ParticleFlipbookComponent {
    fn default() -> Self {
        Self {
            columns: 1,
            rows: 1,
            frames_per_second: 0.0,
        }
    }
}

/// Столкновение с видимой геометрией: восстановление скорости и трение.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParticleCollisionComponent {
    pub restitution: f32,
    pub friction: f32,
    pub thickness: f32,
}

impl Default for ParticleCollisionComponent {
    fn default() -> Self {
        Self {
            restitution: 0.5,
            friction: 0.2,
            thickness: 0.02,
        }
    }
}

/// Эмиттер частиц на объекте сцены (`[objects.particle_emitter]`).
///
/// Текстура опциональна: без неё используется процедурный мягкий диск.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParticleEmitterComponent {
    pub capacity: u32,
    pub blend: ParticleBlendMode,
    pub shape: ParticleShapeComponent,
    /// Частиц в секунду.
    pub rate: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bursts: Vec<ParticleBurstComponent>,
    /// Длительность цикла, секунды.
    pub duration: f32,
    pub looping: bool,
    /// Время жизни, секунды: `[min, max]`.
    pub lifetime: [f32; 2],
    /// Начальная скорость, м/с: `[min, max]`.
    pub speed: [f32; 2],
    /// Ширина квада в мировых единицах: `[min, max]`.
    pub size: [f32; 2],
    /// Начальный поворот квада, радианы: `[min, max]`.
    pub rotation: [f32; 2],
    /// Угловая скорость, радианы в секунду: `[min, max]`.
    pub angular_velocity: [f32; 2],
    pub gravity: [f32; 3],
    /// Линейное затухание скорости, 1/с.
    pub drag: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub color_over_life: Vec<ParticleColorKeyComponent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub size_over_life: Vec<ParticleSizeKeyComponent>,
    /// Виртуальный путь `.png`; обязан быть объявлен в `[[assets]]`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub texture: Option<VirtualPath>,
    pub flipbook: ParticleFlipbookComponent,
    /// Расстояние мягкого затухания у геометрии, мировые единицы; 0 — выключено.
    pub soft_distance: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collision: Option<ParticleCollisionComponent>,
    pub seed: u32,
}

impl Default for ParticleEmitterComponent {
    fn default() -> Self {
        Self {
            capacity: 256,
            blend: ParticleBlendMode::Additive,
            shape: ParticleShapeComponent::Point {},
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
            flipbook: ParticleFlipbookComponent::default(),
            soft_distance: 0.0,
            collision: None,
            seed: 1,
        }
    }
}

fn finite_pair(range: [f32; 2]) -> bool {
    range.iter().all(|value| value.is_finite()) && range[0] <= range[1]
}

fn curve_times_valid(times: impl Iterator<Item = f32>) -> bool {
    let mut previous = None::<f32>;
    let mut count = 0usize;
    for time in times {
        count += 1;
        if !time.is_finite()
            || !(0.0..=1.0).contains(&time)
            || previous.is_some_and(|before| time <= before)
        {
            return false;
        }
        previous = Some(time);
    }
    count <= MAX_PARTICLE_CURVE_KEYS
}

/// Проверяет эмиттер: текстуру, ёмкость, диапазоны и ключи. `assets` — объявленные `[[assets]]` документа.
pub(crate) fn validate_particle_emitter(
    source: &str,
    object_id: &str,
    assets: &HashSet<VirtualPath>,
    emitter: &ParticleEmitterComponent,
) -> Result<(), SceneError> {
    let fail = |message: &str| Err(SceneError::new(source, Some(object_id), message));
    if let Some(texture) = &emitter.texture {
        require_asset_path(source, object_id, assets, texture, "текстура частиц")?;
        require_extension(source, object_id, texture, "png", "текстура частиц")?;
    }
    let capacity_limit = match emitter.blend {
        ParticleBlendMode::Additive => MAX_ADDITIVE_PARTICLE_CAPACITY,
        ParticleBlendMode::Alpha => MAX_ALPHA_PARTICLE_CAPACITY,
    };
    if emitter.capacity == 0 || emitter.capacity > capacity_limit {
        return fail("particle_emitter.capacity вне допустимого диапазона для выбранного blend");
    }
    let shape_ok = match emitter.shape {
        ParticleShapeComponent::Point {} => true,
        ParticleShapeComponent::Sphere { radius } => radius.is_finite() && radius >= 0.0,
        ParticleShapeComponent::Cone { angle, radius } => {
            angle.is_finite()
                && (0.0..=std::f32::consts::FRAC_PI_2).contains(&angle)
                && radius.is_finite()
                && radius >= 0.0
        }
        ParticleShapeComponent::Box { half_extents } => half_extents
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0),
    };
    if !shape_ok {
        return fail("particle_emitter.shape содержит некорректные размеры");
    }
    if !emitter.rate.is_finite() || emitter.rate < 0.0 {
        return fail("particle_emitter.rate должен быть конечным и не отрицательным");
    }
    if !emitter.duration.is_finite() || emitter.duration <= 0.0 {
        return fail("particle_emitter.duration должен быть больше нуля");
    }
    for burst in &emitter.bursts {
        if !burst.time.is_finite()
            || burst.time < 0.0
            || burst.time >= emitter.duration
            || burst.count == 0
        {
            return fail("particle_emitter.bursts: время вне цикла или нулевое число частиц");
        }
    }
    if !emitter.lifetime.iter().all(|value| value.is_finite())
        || emitter.lifetime[0] <= 0.0
        || emitter.lifetime[0] > emitter.lifetime[1]
    {
        return fail("particle_emitter.lifetime: нужен диапазон [min, max] с min > 0");
    }
    if !finite_pair(emitter.speed) || emitter.speed[0] < 0.0 {
        return fail("particle_emitter.speed: нужен диапазон [min, max] с min >= 0");
    }
    if !finite_pair(emitter.size) || emitter.size[0] < 0.0 {
        return fail("particle_emitter.size: нужен диапазон [min, max] с min >= 0");
    }
    if !finite_pair(emitter.rotation) || !finite_pair(emitter.angular_velocity) {
        return fail("particle_emitter.rotation и angular_velocity: нужны диапазоны [min, max]");
    }
    if emitter.gravity.iter().any(|value| !value.is_finite())
        || !emitter.drag.is_finite()
        || emitter.drag < 0.0
    {
        return fail(
            "particle_emitter.gravity и drag должны быть конечными, drag не отрицательный",
        );
    }
    if !curve_times_valid(emitter.color_over_life.iter().map(|key| key.time)) {
        return fail(
            "particle_emitter.color_over_life: до четырёх ключей с возрастающим time в [0, 1]",
        );
    }
    for key in &emitter.color_over_life {
        let [red, green, blue, alpha] = key.color;
        if [red, green, blue]
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
            || !(0.0..=1.0).contains(&alpha)
        {
            return fail("particle_emitter.color_over_life: цвет >= 0, альфа в [0, 1]");
        }
    }
    if !curve_times_valid(emitter.size_over_life.iter().map(|key| key.time)) {
        return fail(
            "particle_emitter.size_over_life: до четырёх ключей с возрастающим time в [0, 1]",
        );
    }
    if emitter
        .size_over_life
        .iter()
        .any(|key| !key.size.is_finite() || key.size < 0.0)
    {
        return fail(
            "particle_emitter.size_over_life: размер должен быть конечным и не отрицательным",
        );
    }
    let flipbook = emitter.flipbook;
    if flipbook.columns == 0
        || flipbook.rows == 0
        || flipbook.columns > MAX_FLIPBOOK_GRID
        || flipbook.rows > MAX_FLIPBOOK_GRID
        || !flipbook.frames_per_second.is_finite()
        || flipbook.frames_per_second < 0.0
    {
        return fail("particle_emitter.flipbook: сетка 1..=16 и неотрицательная частота кадров");
    }
    if !emitter.soft_distance.is_finite() || emitter.soft_distance < 0.0 {
        return fail("particle_emitter.soft_distance должен быть конечным и не отрицательным");
    }
    if let Some(collision) = emitter.collision
        && (!(0.0..=1.0).contains(&collision.restitution)
            || !(0.0..=1.0).contains(&collision.friction)
            || !collision.thickness.is_finite()
            || collision.thickness < 0.0)
    {
        return fail("particle_emitter.collision: restitution и friction в [0, 1], thickness >= 0");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ParticleEmitterComponent, toml::de::Error> {
        toml::from_str(text)
    }

    fn check(emitter: &ParticleEmitterComponent) -> Result<(), SceneError> {
        validate_particle_emitter("scene.rvescene", "fx", &HashSet::new(), emitter)
    }

    fn texture_asset(path: &str) -> HashSet<VirtualPath> {
        let mut assets = HashSet::new();
        assets.insert(VirtualPath::parse(path).expect("valid test path"));
        assets
    }

    #[test]
    fn empty_table_uses_defaults_matching_engine_render() {
        let emitter = parse("").expect("empty component is valid");
        assert_eq!(emitter, ParticleEmitterComponent::default());
        assert_eq!(emitter.capacity, 256);
        assert_eq!(emitter.blend, ParticleBlendMode::Additive);
        assert!(emitter.looping);
        assert!(check(&emitter).is_ok());
    }

    #[test]
    fn full_component_parses_nested_tables_and_arrays() {
        let text = r#"
            capacity = 2048
            blend = "alpha"
            rate = 120.0
            duration = 2.0
            looping = false
            lifetime = [1.0, 2.5]
            gravity = [0.0, 0.4, 0.0]
            texture = "fx/smoke.png"
            soft_distance = 0.5
            bursts = [{ time = 0.25, count = 30 }]
            color_over_life = [
                { time = 0.0, color = [1.0, 0.5, 0.0, 1.0] },
                { time = 1.0, color = [0.1, 0.1, 0.1, 0.0] },
            ]
            size_over_life = [{ time = 0.0, size = 0.5 }, { time = 1.0, size = 2.0 }]
            flipbook = { columns = 4, rows = 2, frames_per_second = 8.0 }
            collision = { restitution = 0.4, friction = 0.1, thickness = 0.05 }
            seed = 7
            shape = { kind = "cone", angle = 0.5, radius = 0.25 }
        "#;
        let emitter = parse(text).expect("valid component");
        assert_eq!(emitter.blend, ParticleBlendMode::Alpha);
        assert_eq!(emitter.capacity, 2048);
        assert_eq!(emitter.bursts[0].count, 30);
        assert_eq!(emitter.color_over_life.len(), 2);
        assert_eq!(emitter.flipbook.columns, 4);
        assert_eq!(emitter.flipbook.rows, 2);
        assert_eq!(
            emitter.collision.map(|collision| collision.thickness),
            Some(0.05)
        );
        assert_eq!(
            emitter.shape,
            ParticleShapeComponent::Cone {
                angle: 0.5,
                radius: 0.25
            }
        );
        assert_eq!(emitter.seed, 7);
        assert!(emitter.texture.is_some());
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        assert!(parse("unknown = 1").is_err());
        assert!(parse("flipbook = { columns = 1, extra = 2 }").is_err());
        assert!(parse("shape = { kind = \"point\", radius = 1.0 }").is_err());
        assert!(parse("bursts = [{ time = 0.0, count = 1, extra = 2 }]").is_err());
        assert!(parse("collision = { restitution = 0.5, bogus = 1.0 }").is_err());
    }

    #[test]
    fn default_collision_and_flipbook_fill_missing_fields() {
        let emitter = parse("collision = {}\nflipbook = {}").expect("partial tables are valid");
        assert_eq!(
            emitter.collision,
            Some(ParticleCollisionComponent::default())
        );
        assert_eq!(emitter.flipbook, ParticleFlipbookComponent::default());
    }

    #[test]
    fn round_trip_preserves_component() {
        let emitter = ParticleEmitterComponent {
            blend: ParticleBlendMode::Alpha,
            capacity: 512,
            bursts: vec![ParticleBurstComponent {
                time: 0.1,
                count: 4,
            }],
            shape: ParticleShapeComponent::Box {
                half_extents: [1.0, 0.5, 1.0],
            },
            collision: Some(ParticleCollisionComponent::default()),
            ..ParticleEmitterComponent::default()
        };
        let text = toml::to_string(&emitter).expect("serializes");
        let parsed: ParticleEmitterComponent = toml::from_str(&text).expect("parses back");
        assert_eq!(parsed, emitter);
    }

    #[test]
    fn default_component_omits_optional_tables() {
        let text = toml::to_string(&ParticleEmitterComponent::default()).expect("serializes");
        assert!(!text.contains("bursts"));
        assert!(!text.contains("collision"));
        assert!(!text.contains("texture"));
        assert!(!text.contains("color_over_life"));
    }

    #[test]
    fn capacity_limit_depends_on_blend() {
        let mut emitter = ParticleEmitterComponent {
            capacity: MAX_ADDITIVE_PARTICLE_CAPACITY,
            ..ParticleEmitterComponent::default()
        };
        assert!(check(&emitter).is_ok());
        emitter.blend = ParticleBlendMode::Alpha;
        assert!(check(&emitter).is_err());
        emitter.capacity = MAX_ALPHA_PARTICLE_CAPACITY;
        assert!(check(&emitter).is_ok());
        emitter.capacity = 0;
        assert!(check(&emitter).is_err());
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let base = ParticleEmitterComponent::default();
        let cases = [
            ParticleEmitterComponent {
                rate: f32::NAN,
                ..base.clone()
            },
            ParticleEmitterComponent {
                duration: 0.0,
                ..base.clone()
            },
            ParticleEmitterComponent {
                lifetime: [0.0, 1.0],
                ..base.clone()
            },
            ParticleEmitterComponent {
                lifetime: [2.0, 1.0],
                ..base.clone()
            },
            ParticleEmitterComponent {
                speed: [-1.0, 1.0],
                ..base.clone()
            },
            ParticleEmitterComponent {
                size: [0.5, 0.1],
                ..base.clone()
            },
            ParticleEmitterComponent {
                drag: -1.0,
                ..base.clone()
            },
            ParticleEmitterComponent {
                soft_distance: -0.1,
                ..base.clone()
            },
            ParticleEmitterComponent {
                shape: ParticleShapeComponent::Cone {
                    angle: 2.0,
                    radius: 0.0,
                },
                ..base.clone()
            },
            ParticleEmitterComponent {
                flipbook: ParticleFlipbookComponent {
                    columns: 17,
                    rows: 1,
                    frames_per_second: 0.0,
                },
                ..base.clone()
            },
            ParticleEmitterComponent {
                collision: Some(ParticleCollisionComponent {
                    restitution: 1.5,
                    friction: 0.0,
                    thickness: 0.0,
                }),
                ..base.clone()
            },
            ParticleEmitterComponent {
                bursts: vec![ParticleBurstComponent {
                    time: 1.0,
                    count: 3,
                }],
                ..base.clone()
            },
            ParticleEmitterComponent {
                bursts: vec![ParticleBurstComponent {
                    time: 0.0,
                    count: 0,
                }],
                ..base.clone()
            },
        ];
        for emitter in cases {
            assert!(check(&emitter).is_err(), "{emitter:?} must be rejected");
        }
    }

    #[test]
    fn curve_keys_need_order_bounds_and_count() {
        let color = |time: f32, alpha: f32| ParticleColorKeyComponent {
            time,
            color: [1.0, 1.0, 1.0, alpha],
        };
        let with = |keys: Vec<ParticleColorKeyComponent>| ParticleEmitterComponent {
            color_over_life: keys,
            ..ParticleEmitterComponent::default()
        };
        assert!(check(&with(vec![color(0.0, 1.0), color(1.0, 0.0)])).is_ok());
        assert!(check(&with(vec![color(0.5, 1.0), color(0.5, 0.0)])).is_err());
        assert!(check(&with(vec![color(0.0, 1.2)])).is_err());
        let five: Vec<_> = (0..5).map(|index| color(index as f32 / 5.0, 1.0)).collect();
        assert!(check(&with(five)).is_err());
    }

    #[test]
    fn texture_must_be_declared_png_asset() {
        let emitter = ParticleEmitterComponent {
            texture: Some(VirtualPath::parse("fx/smoke.png").expect("path")),
            ..ParticleEmitterComponent::default()
        };
        let undeclared = validate_particle_emitter("s", "fx", &HashSet::new(), &emitter);
        assert!(undeclared.is_err());
        let declared = texture_asset("fx/smoke.png");
        assert!(validate_particle_emitter("s", "fx", &declared, &emitter).is_ok());
        let jpeg = ParticleEmitterComponent {
            texture: Some(VirtualPath::parse("fx/smoke.jpg").expect("path")),
            ..ParticleEmitterComponent::default()
        };
        assert!(
            validate_particle_emitter("s", "fx", &texture_asset("fx/smoke.jpg"), &jpeg).is_err()
        );
    }
}
