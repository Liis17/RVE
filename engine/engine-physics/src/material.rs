use crate::world::PhysicsWorldError;

/// Правило совмещения значения двух материалов. Если у материалов пары разные правила, берётся
/// старшее по приоритету: `Average < Minimum < Multiply < Maximum`, как в Unity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombineMode {
    Average,
    Minimum,
    Multiply,
    Maximum,
}

impl CombineMode {
    fn priority(self) -> u8 {
        match self {
            Self::Average => 0,
            Self::Minimum => 1,
            Self::Multiply => 2,
            Self::Maximum => 3,
        }
    }

    fn apply(self, first: f32, second: f32) -> f32 {
        match self {
            Self::Average => (first + second) * 0.5,
            Self::Minimum => first.min(second),
            Self::Multiply => first * second,
            Self::Maximum => first.max(second),
        }
    }

    fn strongest(first: Self, second: Self) -> Self {
        if first.priority() >= second.priority() {
            first
        } else {
            second
        }
    }
}

/// Физические свойства поверхности коллайдера.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsMaterial {
    /// Трение покоя; применяется, пока точка контакта не скользит.
    pub static_friction: f32,
    /// Трение скольжения; применяется, пока точка контакта скользит.
    pub dynamic_friction: f32,
    /// Отскок в диапазоне `[0, 1]`.
    pub bounciness: f32,
    pub friction_combine: CombineMode,
    pub bounce_combine: CombineMode,
}

impl PhysicsMaterial {
    /// Материал инлайн-полей коллайдера: трение покоя и скольжения равны `friction`, отскок
    /// совмещается максимумом, трение — средним.
    pub fn from_friction_restitution(friction: f32, restitution: f32) -> Self {
        Self {
            static_friction: friction,
            dynamic_friction: friction,
            bounciness: restitution,
            friction_combine: CombineMode::Average,
            bounce_combine: CombineMode::Maximum,
        }
    }

    pub fn validate(&self) -> Result<(), PhysicsWorldError> {
        let friction_ok = [self.static_friction, self.dynamic_friction]
            .iter()
            .all(|friction| friction.is_finite() && *friction >= 0.0);
        if !friction_ok || !self.bounciness.is_finite() || !(0.0..=1.0).contains(&self.bounciness) {
            return Err(PhysicsWorldError::InvalidMaterial);
        }
        Ok(())
    }
}

/// Значения контакта двух материалов после совмещения.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CombinedMaterial {
    pub static_friction: f32,
    pub dynamic_friction: f32,
    pub bounciness: f32,
}

pub fn combine_materials(first: &PhysicsMaterial, second: &PhysicsMaterial) -> CombinedMaterial {
    let friction_mode = CombineMode::strongest(first.friction_combine, second.friction_combine);
    let bounce_mode = CombineMode::strongest(first.bounce_combine, second.bounce_combine);
    CombinedMaterial {
        static_friction: friction_mode.apply(first.static_friction, second.static_friction),
        dynamic_friction: friction_mode.apply(first.dynamic_friction, second.dynamic_friction),
        bounciness: bounce_mode.apply(first.bounciness, second.bounciness),
    }
}

#[cfg(test)]
mod tests {
    use super::{CombineMode, PhysicsMaterial, combine_materials};
    use crate::PhysicsWorldError;

    const MODES: [CombineMode; 4] = [
        CombineMode::Average,
        CombineMode::Minimum,
        CombineMode::Multiply,
        CombineMode::Maximum,
    ];

    fn material(friction: f32, bounciness: f32, mode: CombineMode) -> PhysicsMaterial {
        PhysicsMaterial {
            static_friction: friction,
            dynamic_friction: friction,
            bounciness,
            friction_combine: mode,
            bounce_combine: mode,
        }
    }

    fn apply(mode: CombineMode, first: f32, second: f32) -> f32 {
        match mode {
            CombineMode::Average => (first + second) * 0.5,
            CombineMode::Minimum => first.min(second),
            CombineMode::Multiply => first * second,
            CombineMode::Maximum => first.max(second),
        }
    }

    #[test]
    fn combine_priority_selects_the_higher_mode_for_all_sixteen_pairs() {
        for first_mode in MODES {
            for second_mode in MODES {
                let first = material(0.2, 0.3, first_mode);
                let second = material(0.8, 0.9, second_mode);
                let strongest = MODES
                    .into_iter()
                    .filter(|mode| *mode == first_mode || *mode == second_mode)
                    .max_by_key(|mode| *mode as u8)
                    .unwrap();
                let combined = combine_materials(&first, &second);
                let expected = apply(strongest, 0.2, 0.8);
                assert_eq!(
                    combined.static_friction, expected,
                    "{first_mode:?} with {second_mode:?}"
                );
                assert_eq!(combined.dynamic_friction, expected);
                assert_eq!(combined.bounciness, apply(strongest, 0.3, 0.9));
            }
        }
    }

    #[test]
    fn inline_friction_pair_uses_average_not_geometric_mean() {
        let combined = combine_materials(
            &PhysicsMaterial::from_friction_restitution(0.2, 0.0),
            &PhysicsMaterial::from_friction_restitution(0.8, 0.0),
        );
        assert!((combined.static_friction - 0.5).abs() < 1.0e-6);
        assert!((combined.dynamic_friction - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn bounciness_follows_its_own_combine_mode() {
        let mut first = material(0.5, 0.3, CombineMode::Average);
        let mut second = material(0.5, 0.9, CombineMode::Average);
        first.bounce_combine = CombineMode::Minimum;
        second.bounce_combine = CombineMode::Minimum;
        assert_eq!(combine_materials(&first, &second).bounciness, 0.3);
        second.bounce_combine = CombineMode::Maximum;
        assert_eq!(combine_materials(&first, &second).bounciness, 0.9);
    }

    #[test]
    fn invalid_material_values_are_rejected() {
        let negative = PhysicsMaterial {
            static_friction: -0.1,
            ..material(0.5, 0.0, CombineMode::Average)
        };
        let not_finite = PhysicsMaterial {
            dynamic_friction: f32::NAN,
            ..material(0.5, 0.0, CombineMode::Average)
        };
        let bouncy = material(0.5, 1.5, CombineMode::Average);
        for invalid in [negative, not_finite, bouncy] {
            assert_eq!(invalid.validate(), Err(PhysicsWorldError::InvalidMaterial));
        }
        assert!(material(0.0, 1.0, CombineMode::Multiply).validate().is_ok());
    }
}
