//! Скелет и веса вершин модели, извлекаемые из FBX через ufbx.
//!
//! Типы хранят только данные без `glam`: композиция поз и палитры skinning
//! выполняется в `engine-animation`. Порядок узлов гарантирует, что родитель
//! всегда стоит раньше потомка.

use std::collections::HashMap;

/// Способ наследования масштаба родителя; значения совпадают с `ufbx::InheritMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModelInheritMode {
    #[default]
    Normal,
    /// Родительский масштаб не применяется к повороту и переносу узла.
    IgnoreParentScale,
    /// Масштаб родителя применяется покомпонентно (через накопленный масштаб).
    ComponentwiseScale,
}

/// Локальная поза узла: перенос, поворот кватернионом `[x, y, z, w]` и масштаб.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelTransform {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

/// Узел скелета: кости, узлы мешей и их предки до корня сцены.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelNode {
    pub name: String,
    pub parent: Option<usize>,
    pub inherit_mode: ModelInheritMode,
    /// Узел, накопленный масштаб которого учитывается при наследовании (как `inherit_scale_node` в ufbx).
    pub inherit_scale_node: Option<usize>,
    /// Поза файла, в которой модель загружена без анимации.
    pub rest: ModelTransform,
}

/// Палитра skinning: мировая матрица узла, умноженная на `inverse_bind`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelJoint {
    /// Индекс в [`ModelSkeleton::nodes`].
    pub node: usize,
    /// Матрица по столбцам, как в `glam::Mat4::to_cols_array_2d`.
    pub inverse_bind: [[f32; 4]; 4],
}

/// Скелет модели. Одна кость может встречаться несколько раз, если разные части
/// меша связаны с ней с разными inverse bind матрицами.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelSkeleton {
    pub nodes: Vec<ModelNode>,
    pub joints: Vec<ModelJoint>,
}

/// Веса вершин меша, выровненные по [`crate::ModelMesh::vertices`].
///
/// Индексы ссылаются на [`ModelSkeleton::joints`]; веса четырёх связей нормализованы.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSkin {
    pub joints: Vec<[u32; 4]>,
    pub weights: Vec<[f32; 4]>,
}

/// Строит скелет, добавляя узлы по требованию вместе с предками.
#[derive(Default)]
pub(crate) struct SkeletonBuilder {
    skeleton: ModelSkeleton,
    nodes: HashMap<u32, usize>,
}

impl SkeletonBuilder {
    /// Возвращает индекс узла скелета, предварительно добавив его предков.
    pub(crate) fn node(&mut self, node: &ufbx::Node) -> usize {
        if let Some(&index) = self.nodes.get(&node.element.typed_id) {
            return index;
        }
        let parent = node.parent.as_deref().map(|parent| self.node(parent));
        let inherit_scale_node = node
            .inherit_scale_node
            .as_deref()
            .map(|scale_node| self.node(scale_node));
        let index = self.skeleton.nodes.len();
        self.skeleton.nodes.push(ModelNode {
            name: node.element.name.to_string(),
            parent,
            inherit_mode: inherit_mode(node.inherit_mode),
            inherit_scale_node,
            rest: rest_transform(&node.local_transform),
        });
        self.nodes.insert(node.element.typed_id, index);
        index
    }

    /// Есть ли узел в скелете; нужен для выбора треков анимации модели.
    pub(crate) fn contains(&self, typed_id: u32) -> bool {
        self.nodes.contains_key(&typed_id)
    }

    /// Жёсткая привязка части меша к собственному узлу: палитра равна единице в позе файла.
    pub(crate) fn rigid_joint(&mut self, node: &ufbx::Node) -> u32 {
        let node_index = self.node(node);
        let inverse_bind = ufbx::matrix_invert(&node.node_to_world);
        self.push_joint(node_index, &inverse_bind)
    }

    /// Создаёт кости для кластеров деформера. Кластер без кости даёт `None`.
    ///
    /// Вершины модели уже переведены в пространство файла через `geometry_to_world`,
    /// поэтому `inverse_bind = geometry_to_bone · inverse(geometry_to_world)`.
    pub(crate) fn skin_joints(
        &mut self,
        mesh_node: &ufbx::Node,
        deformer: &ufbx::SkinDeformer,
    ) -> Vec<Option<u32>> {
        let world_to_geometry = ufbx::matrix_invert(&mesh_node.geometry_to_world);
        let mut joints = Vec::with_capacity(deformer.clusters.len());
        for cluster in deformer.clusters.iter() {
            let Some(bone) = cluster.bone_node.as_deref() else {
                joints.push(None);
                continue;
            };
            let node_index = self.node(bone);
            let inverse_bind = ufbx::matrix_mul(&cluster.geometry_to_bone, &world_to_geometry);
            joints.push(Some(self.push_joint(node_index, &inverse_bind)));
        }
        joints
    }

    pub(crate) fn finish(self) -> ModelSkeleton {
        self.skeleton
    }

    fn push_joint(&mut self, node: usize, inverse_bind: &ufbx::Matrix) -> u32 {
        let index = u32::try_from(self.skeleton.joints.len()).unwrap_or(u32::MAX);
        self.skeleton.joints.push(ModelJoint {
            node,
            inverse_bind: matrix_columns(inverse_bind),
        });
        index
    }
}

/// Веса вершины по кандидатам `(кость, вес)`: четыре крупнейших, нормализованных.
///
/// Кандидаты с весом не больше нуля или не конечным отбрасываются. Топ-4 держится
/// во фиксированном массиве вставкой без аллокаций: при равных весах более ранний
/// кандидат остаётся впереди, как при стабильной сортировке по убыванию с обрезкой до четырёх.
/// Вершина без положительных весов привязывается к `rigid` с весом 1.
pub(crate) fn influences(
    candidates: impl IntoIterator<Item = (u32, f32)>,
    rigid: u32,
) -> ([u32; 4], [f32; 4]) {
    let mut top = [(0, 0.0_f32); 4];
    let mut len = 0;
    for (joint, weight) in candidates {
        if !(weight.is_finite() && weight > 0.0) {
            continue;
        }
        // Новый кандидат встаёт после всех не меньших весов, поэтому равные сохраняют порядок входа.
        let position = top[..len]
            .iter()
            .position(|&(_, kept)| weight > kept)
            .unwrap_or(len);
        if position == top.len() {
            continue;
        }
        // Последний слот вытесняется, если список уже полон.
        top.copy_within(position..len.min(3), position + 1);
        top[position] = (joint, weight);
        len = (len + 1).min(top.len());
    }
    let sorted = &top[..len];
    let sum: f32 = sorted.iter().map(|&(_, weight)| weight).sum();
    if sum <= 0.0 {
        return ([rigid, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
    }
    let mut joints = [0; 4];
    let mut weights = [0.0; 4];
    for (slot, &(joint, weight)) in sorted.iter().enumerate() {
        joints[slot] = joint;
        weights[slot] = weight / sum;
    }
    (joints, weights)
}

/// Веса угла меша по его контрольной точке (`mesh.vertex_indices`).
pub(crate) fn corner_influences(
    deformer: &ufbx::SkinDeformer,
    joints: &[Option<u32>],
    control_point: usize,
    rigid: u32,
) -> ([u32; 4], [f32; 4]) {
    let weights = deformer
        .vertices
        .get(control_point)
        .and_then(|vertex| {
            let begin = vertex.weight_begin as usize;
            let end = begin.saturating_add(vertex.num_weights as usize);
            deformer.weights.get(begin..end)
        })
        .unwrap_or_default();
    influences(
        weights
            .iter()
            .filter_map(|weight| match joints.get(weight.cluster_index as usize) {
                Some(Some(joint)) => Some((*joint, weight.weight as f32)),
                _ => None,
            }),
        rigid,
    )
}

fn inherit_mode(mode: ufbx::InheritMode) -> ModelInheritMode {
    match mode {
        ufbx::InheritMode::Normal => ModelInheritMode::Normal,
        ufbx::InheritMode::IgnoreParentScale => ModelInheritMode::IgnoreParentScale,
        ufbx::InheritMode::ComponentwiseScale => ModelInheritMode::ComponentwiseScale,
    }
}

fn rest_transform(transform: &ufbx::Transform) -> ModelTransform {
    let t = transform.translation;
    let r = transform.rotation;
    let s = transform.scale;
    ModelTransform {
        translation: [t.x as f32, t.y as f32, t.z as f32],
        rotation: [r.x as f32, r.y as f32, r.z as f32, r.w as f32],
        scale: [s.x as f32, s.y as f32, s.z as f32],
    }
}

/// Переводит матрицу ufbx (3×4, по столбцам базиса) в 4×4 по столбцам.
fn matrix_columns(matrix: &ufbx::Matrix) -> [[f32; 4]; 4] {
    [
        [matrix.m00 as f32, matrix.m10 as f32, matrix.m20 as f32, 0.0],
        [matrix.m01 as f32, matrix.m11 as f32, matrix.m21 as f32, 0.0],
        [matrix.m02 as f32, matrix.m12 as f32, matrix.m22 as f32, 0.0],
        [matrix.m03 as f32, matrix.m13 as f32, matrix.m23 as f32, 1.0],
    ]
}

#[cfg(test)]
mod tests {
    use super::influences;

    /// Прежняя реализация с `Vec` и сортировкой: эталон для побитового сравнения.
    fn previous_influences(candidates: &[(u32, f32)], rigid: u32) -> ([u32; 4], [f32; 4]) {
        let mut sorted: Vec<(u32, f32)> = candidates
            .iter()
            .copied()
            .filter(|&(_, weight)| weight.is_finite() && weight > 0.0)
            .collect();
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1));
        sorted.truncate(4);
        let sum: f32 = sorted.iter().map(|&(_, weight)| weight).sum();
        if sum <= 0.0 {
            return ([rigid, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]);
        }
        let mut joints = [0; 4];
        let mut weights = [0.0; 4];
        for (slot, &(joint, weight)) in sorted.iter().enumerate() {
            joints[slot] = joint;
            weights[slot] = weight / sum;
        }
        (joints, weights)
    }

    fn bits((joints, weights): ([u32; 4], [f32; 4])) -> ([u32; 4], [u32; 4]) {
        (joints, weights.map(f32::to_bits))
    }

    #[test]
    fn influences_keep_four_largest_weights_sorted_and_normalized() {
        let (joints, weights) = influences([(0, 0.1), (1, 0.5), (2, 0.2), (3, 0.3), (4, 0.4)], 9);
        assert_eq!(joints, [1, 4, 3, 2]);
        let sum: f32 = weights.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        assert!((weights[0] - 0.5 / 1.4).abs() < 1e-6);
    }

    #[test]
    fn influences_ignore_zero_and_non_finite_weights_and_fall_back_to_rigid_joint() {
        assert_eq!(
            influences([(1, 0.0), (2, f32::NAN), (3, -1.0)], 7),
            ([7, 0, 0, 0], [1.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(influences([], 4), ([4, 0, 0, 0], [1.0, 0.0, 0.0, 0.0]));
    }

    #[test]
    fn influences_normalize_partial_weights_to_one() {
        let (joints, weights) = influences([(5, 0.25), (6, 0.25)], 0);
        assert_eq!(&joints[..2], &[5, 6]);
        assert_eq!(weights, [0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn influences_keep_input_order_for_equal_weights() {
        // Пять равных кандидатов: в четвёрке остаются первые четыре по порядку входа.
        let (joints, _) = influences([(1, 0.25), (2, 0.25), (3, 0.25), (4, 0.25), (5, 0.25)], 0);
        assert_eq!(joints, [1, 2, 3, 4]);
        // Более крупный вес вытесняет последнего из равных, а не первого.
        let (joints, weights) =
            influences([(1, 0.25), (2, 0.25), (3, 0.25), (4, 0.25), (5, 0.5)], 0);
        assert_eq!(joints, [5, 1, 2, 3]);
        assert_eq!(weights, [0.4, 0.2, 0.2, 0.2]);
    }

    #[test]
    fn influences_match_previous_implementation_bit_for_bit() {
        const POOL: [f32; 12] = [
            0.0,
            -0.0,
            0.25,
            0.5,
            0.125,
            1.0,
            0.3,
            1e-30,
            3.5,
            f32::NAN,
            f32::INFINITY,
            -1.0,
        ];
        // Детерминированный генератор: повторяющиеся значения дают равные веса и ничьи.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u32
        };
        for case in 0..4000_u32 {
            let count = (next() % 9) as usize;
            let candidates: Vec<(u32, f32)> = (0..count)
                .map(|_| {
                    let joint = next() % 6;
                    let weight = if next() % 2 == 0 {
                        POOL[next() as usize % POOL.len()]
                    } else {
                        (next() % 1000) as f32 / 97.0
                    };
                    (joint, weight)
                })
                .collect();
            let rigid = case % 5;
            assert_eq!(
                bits(influences(candidates.iter().copied(), rigid)),
                bits(previous_influences(&candidates, rigid)),
                "случай {case}: {candidates:?}"
            );
        }
    }
}
