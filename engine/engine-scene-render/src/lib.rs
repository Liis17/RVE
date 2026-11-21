//! Перевод компонентов сцены `engine-scene` в описания ресурсов `engine-render`.
//!
//! Крейт связывает формат сцены и renderer, чтобы runtime игры и редактор создавали ресурсы
//! по одному сопоставлению; сами `engine-scene` и `engine-render` друг от друга не зависят.
//! Загрузка файлов и создание GPU-ресурсов остаются у вызывающей стороны.

use engine_render::{
    ParticleBlend, ParticleBurst, ParticleCollision, ParticleColorKey, ParticleEmitterDesc,
    ParticleFlipbook, ParticleShape, ParticleSizeKey, TextureId,
};
use engine_scene::{ParticleBlendMode, ParticleEmitterComponent, ParticleShapeComponent};

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Переводит эмиттер сцены в описание эмиттера renderer.
///
/// `texture` — уже созданная вызывающим текстура `component.texture`, `None` — процедурный диск.
/// Диапазоны не проверяются: компонент проверяет engine-scene, описание — `create_particle_emitter`.
pub fn particle_emitter_desc(
    component: &ParticleEmitterComponent,
    texture: Option<TextureId>,
) -> ParticleEmitterDesc {
    ParticleEmitterDesc {
        capacity: component.capacity,
        blend: match component.blend {
            ParticleBlendMode::Additive => ParticleBlend::Additive,
            ParticleBlendMode::Alpha => ParticleBlend::Alpha,
        },
        shape: match component.shape {
            ParticleShapeComponent::Point {} => ParticleShape::Point,
            ParticleShapeComponent::Sphere { radius } => ParticleShape::Sphere { radius },
            ParticleShapeComponent::Cone { angle, radius } => ParticleShape::Cone { angle, radius },
            ParticleShapeComponent::Box { half_extents } => ParticleShape::Box { half_extents },
        },
        rate: component.rate,
        bursts: component
            .bursts
            .iter()
            .map(|burst| ParticleBurst {
                time: burst.time,
                count: burst.count,
            })
            .collect(),
        duration: component.duration,
        looping: component.looping,
        lifetime: component.lifetime,
        speed: component.speed,
        size: component.size,
        rotation: component.rotation,
        angular_velocity: component.angular_velocity,
        gravity: component.gravity,
        drag: component.drag,
        color_over_life: component
            .color_over_life
            .iter()
            .map(|key| ParticleColorKey {
                time: key.time,
                color: key.color,
            })
            .collect(),
        size_over_life: component
            .size_over_life
            .iter()
            .map(|key| ParticleSizeKey {
                time: key.time,
                size: key.size,
            })
            .collect(),
        texture,
        flipbook: ParticleFlipbook {
            columns: component.flipbook.columns,
            rows: component.flipbook.rows,
            frames_per_second: component.flipbook.frames_per_second,
        },
        soft_distance: component.soft_distance,
        collision: component.collision.map(|collision| ParticleCollision {
            restitution: collision.restitution,
            friction: collision.friction,
            thickness: collision.thickness,
        }),
        seed: component.seed,
    }
}

#[cfg(test)]
mod tests {
    use super::particle_emitter_desc;
    use engine_render::{
        MAX_ADDITIVE_PARTICLE_CAPACITY as RENDER_ADDITIVE,
        MAX_ALPHA_PARTICLE_CAPACITY as RENDER_ALPHA, ParticleBlend, ParticleBurst,
        ParticleCollision, ParticleColorKey, ParticleEmitterDesc, ParticleFlipbook, ParticleShape,
        ParticleSizeKey,
    };
    use engine_scene::{
        MAX_ADDITIVE_PARTICLE_CAPACITY as SCENE_ADDITIVE,
        MAX_ALPHA_PARTICLE_CAPACITY as SCENE_ALPHA, ParticleBlendMode, ParticleBurstComponent,
        ParticleCollisionComponent, ParticleColorKeyComponent, ParticleEmitterComponent,
        ParticleFlipbookComponent, ParticleShapeComponent, ParticleSizeKeyComponent,
    };

    #[test]
    fn capacity_limits_match_between_scene_and_render() {
        assert_eq!(SCENE_ADDITIVE, RENDER_ADDITIVE);
        assert_eq!(SCENE_ALPHA, RENDER_ALPHA);
    }

    #[test]
    fn component_maps_every_field_into_render_description() {
        let component = ParticleEmitterComponent {
            capacity: 1024,
            blend: ParticleBlendMode::Alpha,
            shape: ParticleShapeComponent::Cone {
                angle: 0.3,
                radius: 0.5,
            },
            rate: 45.0,
            bursts: vec![
                ParticleBurstComponent {
                    time: 0.2,
                    count: 9,
                },
                ParticleBurstComponent {
                    time: 1.5,
                    count: 3,
                },
            ],
            duration: 2.0,
            looping: false,
            lifetime: [1.0, 3.0],
            speed: [0.5, 1.5],
            size: [0.2, 0.4],
            rotation: [0.0, 1.0],
            angular_velocity: [-1.0, 1.0],
            gravity: [0.0, -9.8, 0.0],
            drag: 0.25,
            color_over_life: vec![
                ParticleColorKeyComponent {
                    time: 0.0,
                    color: [1.0, 0.5, 0.0, 1.0],
                },
                ParticleColorKeyComponent {
                    time: 1.0,
                    color: [2.0, 2.0, 2.0, 0.0],
                },
            ],
            size_over_life: vec![ParticleSizeKeyComponent {
                time: 0.5,
                size: 2.0,
            }],
            texture: None,
            flipbook: ParticleFlipbookComponent {
                columns: 4,
                rows: 2,
                frames_per_second: 6.0,
            },
            soft_distance: 0.75,
            collision: Some(ParticleCollisionComponent {
                restitution: 0.3,
                friction: 0.1,
                thickness: 0.05,
            }),
            seed: 42,
        };
        let expected = ParticleEmitterDesc {
            capacity: 1024,
            blend: ParticleBlend::Alpha,
            shape: ParticleShape::Cone {
                angle: 0.3,
                radius: 0.5,
            },
            rate: 45.0,
            bursts: vec![
                ParticleBurst {
                    time: 0.2,
                    count: 9,
                },
                ParticleBurst {
                    time: 1.5,
                    count: 3,
                },
            ],
            duration: 2.0,
            looping: false,
            lifetime: [1.0, 3.0],
            speed: [0.5, 1.5],
            size: [0.2, 0.4],
            rotation: [0.0, 1.0],
            angular_velocity: [-1.0, 1.0],
            gravity: [0.0, -9.8, 0.0],
            drag: 0.25,
            color_over_life: vec![
                ParticleColorKey {
                    time: 0.0,
                    color: [1.0, 0.5, 0.0, 1.0],
                },
                ParticleColorKey {
                    time: 1.0,
                    color: [2.0, 2.0, 2.0, 0.0],
                },
            ],
            size_over_life: vec![ParticleSizeKey {
                time: 0.5,
                size: 2.0,
            }],
            texture: None,
            flipbook: ParticleFlipbook {
                columns: 4,
                rows: 2,
                frames_per_second: 6.0,
            },
            soft_distance: 0.75,
            collision: Some(ParticleCollision {
                restitution: 0.3,
                friction: 0.1,
                thickness: 0.05,
            }),
            seed: 42,
        };
        assert_eq!(particle_emitter_desc(&component, None), expected);
    }

    #[test]
    fn every_shape_maps_to_the_matching_render_shape() {
        let cases = [
            (ParticleShapeComponent::Point {}, ParticleShape::Point),
            (
                ParticleShapeComponent::Sphere { radius: 2.0 },
                ParticleShape::Sphere { radius: 2.0 },
            ),
            (
                ParticleShapeComponent::Cone {
                    angle: 0.1,
                    radius: 0.0,
                },
                ParticleShape::Cone {
                    angle: 0.1,
                    radius: 0.0,
                },
            ),
            (
                ParticleShapeComponent::Box {
                    half_extents: [1.0, 2.0, 3.0],
                },
                ParticleShape::Box {
                    half_extents: [1.0, 2.0, 3.0],
                },
            ),
        ];
        for (shape, expected) in cases {
            let component = ParticleEmitterComponent {
                shape,
                ..ParticleEmitterComponent::default()
            };
            assert_eq!(particle_emitter_desc(&component, None).shape, expected);
        }
    }

    #[test]
    fn scene_defaults_map_to_render_defaults() {
        // Пустой `[objects.particle_emitter]` должен давать то же, что описание renderer по умолчанию.
        assert_eq!(
            particle_emitter_desc(&ParticleEmitterComponent::default(), None),
            ParticleEmitterDesc::default()
        );
    }
}
