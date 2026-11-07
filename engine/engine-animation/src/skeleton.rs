//! Скелет времени выполнения: узлы с позами покоя и кости с inverse bind матрицами.

use crate::pose::Transform;
use engine_assets::{ModelInheritMode, ModelSkeleton};
use glam::Mat4;

/// Узел скелета; родитель всегда имеет меньший индекс.
#[derive(Clone, Debug, PartialEq)]
pub struct SkeletonNode {
    pub name: String,
    pub parent: Option<usize>,
    pub inherit_mode: ModelInheritMode,
    pub inherit_scale_node: Option<usize>,
    pub rest: Transform,
}

/// Кость: индекс узла и матрица, переводящая вершины модели в локальное пространство кости.
#[derive(Clone, Debug, PartialEq)]
pub struct Joint {
    pub node: usize,
    pub inverse_bind: Mat4,
}

/// Скелет модели в виде, пригодном для вычисления поз.
#[derive(Clone, Debug, PartialEq)]
pub struct Skeleton {
    nodes: Vec<SkeletonNode>,
    joints: Vec<Joint>,
}

impl Skeleton {
    /// Строит скелет из данных `engine-assets`; порядок узлов сохраняется.
    pub fn from_model(model: &ModelSkeleton) -> Self {
        let nodes = model
            .nodes
            .iter()
            .map(|node| SkeletonNode {
                name: node.name.clone(),
                parent: node.parent,
                inherit_mode: node.inherit_mode,
                inherit_scale_node: node.inherit_scale_node,
                rest: Transform::from_model(&node.rest),
            })
            .collect();
        let joints = model
            .joints
            .iter()
            .map(|joint| Joint {
                node: joint.node,
                inverse_bind: Mat4::from_cols_array_2d(&joint.inverse_bind),
            })
            .collect();
        Self { nodes, joints }
    }

    pub fn nodes(&self) -> &[SkeletonNode] {
        &self.nodes
    }

    pub fn joints(&self) -> &[Joint] {
        &self.joints
    }

    /// Индекс первого узла с таким именем. Имена в FBX могут повторяться; выбирается первый.
    pub fn node_index(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|node| node.name == name)
    }
}
