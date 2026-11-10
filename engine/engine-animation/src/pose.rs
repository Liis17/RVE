//! Позы узлов и вычисление палитр skinning.
//!
//! Мировые матрицы считаются теми же формулами, что и в ufbx: при наследовании
//! масштаба узел использует «немасштабированную» матрицу родителя и накопленный
//! масштаб `inherit_scale`. Это проверяется оракулом ufbx в `tests/oracle.rs`.

use crate::skeleton::Skeleton;
use engine_assets::{ModelInheritMode, ModelTransform};
use glam::{Mat4, Quat, Vec3};

/// Локальная поза узла.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl Transform {
    pub const IDENTITY: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
    };

    pub(crate) fn from_model(transform: &ModelTransform) -> Self {
        Self {
            translation: Vec3::from_array(transform.translation),
            rotation: Quat::from_array(transform.rotation),
            scale: Vec3::from_array(transform.scale),
        }
    }

    fn matrix(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    fn unscaled_matrix(&self) -> Mat4 {
        Mat4::from_rotation_translation(self.rotation, self.translation)
    }
}

/// Локальные позы всех узлов скелета в порядке [`Skeleton::nodes`].
#[derive(Clone, Debug, PartialEq)]
pub struct Pose {
    pub transforms: Vec<Transform>,
}

/// Мировые матрицы узла, его «неmасштабированная» матрица и накопленный масштаб.
#[derive(Clone, Copy)]
struct NodeMatrices {
    world: Mat4,
    unscaled: Mat4,
    inherit_scale: Vec3,
}

impl Pose {
    /// Поза покоя скелета.
    pub fn rest(skeleton: &Skeleton) -> Self {
        Self {
            transforms: skeleton.nodes().iter().map(|node| node.rest).collect(),
        }
    }

    /// Возвращает позу к покою без новых аллокаций, если размер совпадает.
    pub fn reset_to_rest(&mut self, skeleton: &Skeleton) {
        self.transforms.clear();
        self.transforms
            .extend(skeleton.nodes().iter().map(|node| node.rest));
    }

    /// Палитра skinning для каждой кости: мировая матрица узла × inverse bind.
    pub fn skin_matrices(&self, skeleton: &Skeleton, out: &mut Vec<[[f32; 4]; 4]>) {
        let matrices = self.world_matrices(skeleton);
        out.clear();
        out.extend(
            skeleton
                .joints()
                .iter()
                .map(|joint| (matrices[joint.node] * joint.inverse_bind).to_cols_array_2d()),
        );
    }

    /// Мировые матрицы узлов; родитель всегда вычислен раньше потомка.
    pub fn world_matrices(&self, skeleton: &Skeleton) -> Vec<Mat4> {
        compose(skeleton, self)
            .into_iter()
            .map(|matrices| matrices.world)
            .collect()
    }
}

fn compose(skeleton: &Skeleton, pose: &Pose) -> Vec<NodeMatrices> {
    let nodes = skeleton.nodes();
    let mut out: Vec<NodeMatrices> = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        let local = pose.transforms[index];
        let entry = match node.parent {
            None => NodeMatrices {
                world: local.matrix(),
                unscaled: local.unscaled_matrix(),
                inherit_scale: local.scale,
            },
            Some(parent) => {
                let parent = out[parent];
                match node.inherit_mode {
                    ModelInheritMode::Normal => NodeMatrices {
                        world: parent.world * local.matrix(),
                        unscaled: parent.unscaled * local.unscaled_matrix(),
                        inherit_scale: local.scale,
                    },
                    ModelInheritMode::IgnoreParentScale | ModelInheritMode::ComponentwiseScale => {
                        let parent_scale = node
                            .inherit_scale_node
                            .map_or(Vec3::ONE, |scale_node| out[scale_node].inherit_scale);
                        let mut transform = local;
                        transform.scale *= parent_scale;
                        transform.translation *= parent.inherit_scale;
                        NodeMatrices {
                            world: parent.unscaled * transform.matrix(),
                            unscaled: parent.unscaled * transform.unscaled_matrix(),
                            inherit_scale: transform.scale,
                        }
                    }
                }
            }
        };
        out.push(entry);
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::pose::Pose;
    use crate::skeleton::Skeleton;
    use engine_assets::{ModelInheritMode, ModelNode, ModelSkeleton, ModelTransform};
    use glam::{Mat4, Vec3};

    fn node(
        name: &str,
        parent: Option<usize>,
        mode: ModelInheritMode,
        translation: [f32; 3],
        scale: f32,
        scale_node: Option<usize>,
    ) -> ModelNode {
        ModelNode {
            name: name.to_owned(),
            parent,
            inherit_mode: mode,
            inherit_scale_node: scale_node,
            rest: ModelTransform {
                translation,
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [scale; 3],
            },
        }
    }

    #[test]
    fn rest_palette_is_identity_when_inverse_bind_matches_the_rest_world() {
        let model = ModelSkeleton {
            nodes: vec![
                node(
                    "root",
                    None,
                    ModelInheritMode::Normal,
                    [0.0, 1.0, 0.0],
                    1.0,
                    None,
                ),
                node(
                    "bone",
                    Some(0),
                    ModelInheritMode::Normal,
                    [0.0, 1.0, 0.0],
                    1.0,
                    None,
                ),
            ],
            joints: vec![engine_assets::ModelJoint {
                node: 1,
                inverse_bind: Mat4::from_translation(Vec3::new(0.0, -2.0, 0.0)).to_cols_array_2d(),
            }],
        };
        let skeleton = Skeleton::from_model(&model);
        let mut palettes = Vec::new();
        Pose::rest(&skeleton).skin_matrices(&skeleton, &mut palettes);
        assert_eq!(palettes, [Mat4::IDENTITY.to_cols_array_2d()]);
    }

    #[test]
    fn ignore_parent_scale_keeps_child_scale_and_scales_only_its_translation() {
        // Родитель масштабирован в 2 раза; потомок не наследует его масштаб, но перенос масштабируется.
        let model = ModelSkeleton {
            nodes: vec![
                node("root", None, ModelInheritMode::Normal, [0.0; 3], 2.0, None),
                node(
                    "hip",
                    Some(0),
                    ModelInheritMode::IgnoreParentScale,
                    [0.0, 1.0, 0.0],
                    1.0,
                    None,
                ),
            ],
            joints: Vec::new(),
        };
        let skeleton = Skeleton::from_model(&model);
        let worlds = Pose::rest(&skeleton).world_matrices(&skeleton);
        let hip = worlds[1];
        assert!((hip.w_axis.truncate() - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-6);
        assert!((hip.x_axis.truncate().length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn componentwise_child_inherits_the_parent_scale_into_its_own_scale() {
        let model = ModelSkeleton {
            nodes: vec![
                node("root", None, ModelInheritMode::Normal, [0.0; 3], 2.0, None),
                node(
                    "arm",
                    Some(0),
                    ModelInheritMode::ComponentwiseScale,
                    [0.0, 1.0, 0.0],
                    1.0,
                    Some(0),
                ),
            ],
            joints: Vec::new(),
        };
        let skeleton = Skeleton::from_model(&model);
        let worlds = Pose::rest(&skeleton).world_matrices(&skeleton);
        let arm = worlds[1];
        assert!((arm.w_axis.truncate() - Vec3::new(0.0, 2.0, 0.0)).length() < 1e-6);
        assert!((arm.x_axis.truncate().length() - 2.0).abs() < 1e-6);
    }
}
