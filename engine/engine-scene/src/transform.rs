//! Локальное положение, вращение и масштаб объекта сцены.

use glam::{Mat4, Quat, Vec3};

/// Компоненты преобразования из локального пространства в мировое.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        }
    }
}

impl Transform {
    /// Матрица `T · R · S`, применяемая к столбцовым векторам.
    pub fn matrix(self) -> Mat4 {
        Mat4::from_translation(self.translation)
            * Mat4::from_quat(self.rotation)
            * Mat4::from_scale(self.scale)
    }
}

#[cfg(test)]
mod tests {
    use super::Transform;
    use glam::{Quat, Vec3};

    #[test]
    fn default_transform_is_identity() {
        let transform = Transform::default();
        assert_eq!(transform.matrix(), glam::Mat4::IDENTITY);
    }

    #[test]
    fn matrix_applies_scale_then_rotation_then_translation() {
        let transform = Transform {
            translation: Vec3::new(3.0, 4.0, 5.0),
            rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            scale: Vec3::new(2.0, 3.0, 4.0),
        };

        let point = transform.matrix().transform_point3(Vec3::X);
        assert!(point.abs_diff_eq(Vec3::new(3.0, 6.0, 5.0), 1e-6));
    }
}
