//! Перенос палитры skinning на скелет другой модели (уровня LOD) по именам костей.

use engine_assets::ModelSkeleton;
use glam::Mat4;

/// Перевод палитры базовой модели в палитру модели уровня LOD с тем же скелетом.
///
/// Палитра базовой модели хранит `world(node) · inverse_bind_base`, поэтому мировая матрица
/// кости восстанавливается умножением на обратную inverse bind. Кость уровня ищется среди костей
/// базовой модели по имени узла; порядок и число костей уровня могут отличаться.
#[derive(Clone, Debug, PartialEq)]
pub struct PaletteRemap {
    base_joint_count: usize,
    /// Для каждой кости уровня: индекс кости базовой палитры и поправка
    /// `inverse(inverse_bind_base) · inverse_bind_lod`.
    joints: Vec<(usize, Mat4)>,
}

impl PaletteRemap {
    /// Строит перевод; `None`, если у уровня нет костей, кость уровня не найдена среди костей
    /// базовой модели или inverse bind базовой кости вырождена.
    pub fn new(base: &ModelSkeleton, lod: &ModelSkeleton) -> Option<Self> {
        if lod.joints.is_empty() {
            return None;
        }
        let joints = lod
            .joints
            .iter()
            .map(|joint| {
                let name = &lod.nodes.get(joint.node)?.name;
                let (index, base_joint) =
                    base.joints.iter().enumerate().find(|(_, candidate)| {
                        base.nodes
                            .get(candidate.node)
                            .is_some_and(|node| node.name == *name)
                    })?;
                let base_inverse_bind = Mat4::from_cols_array_2d(&base_joint.inverse_bind);
                let determinant = base_inverse_bind.determinant();
                if !determinant.is_finite() || determinant.abs() <= f32::EPSILON {
                    return None;
                }
                let correction =
                    base_inverse_bind.inverse() * Mat4::from_cols_array_2d(&joint.inverse_bind);
                Some((index, correction))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            base_joint_count: base.joints.len(),
            joints,
        })
    }

    /// Число костей палитры уровня.
    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    /// Пишет палитру уровня в `out`; `false` и пустой `out`, если длина базовой палитры не
    /// совпадает со скелетом, по которому построен перевод.
    pub fn apply(&self, base: &[[[f32; 4]; 4]], out: &mut Vec<[[f32; 4]; 4]>) -> bool {
        out.clear();
        if base.len() != self.base_joint_count {
            return false;
        }
        out.extend(self.joints.iter().map(|(index, correction)| {
            (Mat4::from_cols_array_2d(&base[*index]) * *correction).to_cols_array_2d()
        }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::PaletteRemap;
    use engine_assets::{ModelInheritMode, ModelJoint, ModelNode, ModelSkeleton, ModelTransform};
    use glam::{Mat4, Vec3};

    fn node(name: &str) -> ModelNode {
        ModelNode {
            name: name.to_owned(),
            parent: None,
            inherit_mode: ModelInheritMode::Normal,
            inherit_scale_node: None,
            rest: ModelTransform {
                translation: [0.0; 3],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0; 3],
            },
        }
    }

    fn skeleton(names: &[&str], joints: &[(usize, Mat4)]) -> ModelSkeleton {
        ModelSkeleton {
            nodes: names.iter().map(|name| node(name)).collect(),
            joints: joints
                .iter()
                .map(|(node, inverse_bind)| ModelJoint {
                    node: *node,
                    inverse_bind: inverse_bind.to_cols_array_2d(),
                })
                .collect(),
        }
    }

    #[test]
    fn lod_palette_follows_base_bones_by_name_with_own_inverse_bind() {
        let base = skeleton(
            &["root", "spine", "arm"],
            &[
                (1, Mat4::from_translation(Vec3::new(0.0, -1.0, 0.0))),
                (2, Mat4::from_translation(Vec3::new(0.0, -2.0, 0.0))),
            ],
        );
        // Уровень хранит только руку, в другом порядке узлов и со своей inverse bind.
        let lod = skeleton(
            &["arm", "root"],
            &[(0, Mat4::from_translation(Vec3::new(-3.0, 0.0, 0.0)))],
        );
        let remap = PaletteRemap::new(&base, &lod).unwrap();
        assert_eq!(remap.joint_count(), 1);
        let arm_world = Mat4::from_translation(Vec3::new(5.0, 2.0, 0.0));
        let base_palette = [
            Mat4::IDENTITY.to_cols_array_2d(),
            (arm_world * Mat4::from_translation(Vec3::new(0.0, -2.0, 0.0))).to_cols_array_2d(),
        ];
        let mut out = Vec::new();
        assert!(remap.apply(&base_palette, &mut out));
        let expected = arm_world * Mat4::from_translation(Vec3::new(-3.0, 0.0, 0.0));
        assert!(Mat4::from_cols_array_2d(&out[0]).abs_diff_eq(expected, 1e-5));

        assert!(!remap.apply(&base_palette[..1], &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn unknown_bone_degenerate_bind_or_boneless_level_cannot_be_remapped() {
        let base = skeleton(&["spine"], &[(0, Mat4::IDENTITY)]);
        let unknown = skeleton(&["tail"], &[(0, Mat4::IDENTITY)]);
        assert!(PaletteRemap::new(&base, &unknown).is_none());
        assert!(PaletteRemap::new(&base, &skeleton(&["spine"], &[])).is_none());
        let degenerate = skeleton(&["spine"], &[(0, Mat4::ZERO)]);
        let lod = skeleton(&["spine"], &[(0, Mat4::IDENTITY)]);
        assert!(PaletteRemap::new(&degenerate, &lod).is_none());
    }
}
