//! Смешивание поз и веса одномерного blend tree.

use crate::pose::{Pose, Transform};
use glam::Quat;

/// Смешивает позы с весами. Веса нормализуются; отрицательные и нечисловые отбрасываются.
///
/// Вращения выравниваются по знаку относительно первой позы с положительным весом,
/// затем суммируются и нормируются. Если суммарный вес нулевой, `out` не меняется.
pub fn blend(inputs: &[(&Pose, f32)], out: &mut Pose) {
    blend_with(inputs.len(), |index| inputs[index], out);
}

/// [`blend`] по индексу: позы отдаёт `input`, поэтому вызывающий код не строит временный вектор.
pub(crate) fn blend_with<'a>(
    count: usize,
    input: impl Fn(usize) -> (&'a Pose, f32),
    out: &mut Pose,
) {
    // Отбор входов тот же, что у публичного `blend`: конечный и положительный вес.
    let usable = || {
        (0..count)
            .map(&input)
            .filter(|&(_, weight)| weight.is_finite() && weight > 0.0)
    };
    let Some((reference, _)) = usable().next() else {
        return;
    };
    let total: f32 = usable().map(|(_, weight)| weight).sum();
    if total <= 0.0 {
        return;
    }
    let nodes = reference.transforms.len();
    out.transforms.resize(nodes, Transform::IDENTITY);
    // Суммы копятся прямо в `out` по входам, а не по узлам: отбор входов выполняется один раз.
    // Для каждого узла слагаемые складываются в том же порядке, что и раньше, поэтому результат
    // совпадает бит-в-бит. Вращение копится как Vec4 в поле `rotation`: `Quat::from_vec4` лишь
    // переупаковывает компоненты.
    for sum in &mut out.transforms {
        sum.translation = glam::Vec3::ZERO;
        sum.rotation = Quat::from_array([0.0; 4]);
        sum.scale = glam::Vec3::ZERO;
    }
    for (pose, weight) in usable() {
        let factor = weight / total;
        for (node, sum) in out.transforms.iter_mut().enumerate() {
            let reference_rotation = reference.transforms[node].rotation;
            let transform = pose.transforms[node];
            sum.translation += transform.translation * factor;
            sum.scale += transform.scale * factor;
            let aligned = if reference_rotation.dot(transform.rotation) < 0.0 {
                -transform.rotation
            } else {
                transform.rotation
            };
            let rotation = glam::Vec4::from(sum.rotation) + glam::Vec4::from(aligned) * factor;
            sum.rotation = Quat::from_vec4(rotation);
        }
    }
    for (node, sum) in out.transforms.iter_mut().enumerate() {
        let reference_rotation = reference.transforms[node].rotation;
        if sum.rotation.length_squared() > 1e-12 {
            sum.rotation = sum.rotation.normalize();
        } else {
            sum.rotation = reference_rotation;
        }
    }
}

/// Веса одномерного blend tree для значения параметра.
///
/// `thresholds` строго возрастают. Значения вне диапазона зажимаются к крайней точке;
/// между соседними точками вес распределяется линейно. Возвращает пары `(индекс, вес)`.
pub fn line_weights(thresholds: &[f32], value: f32) -> Vec<(usize, f32)> {
    line_weights_inline(thresholds, value).as_slice().to_vec()
}

/// Пары весов одномерного дерева без выделения памяти. У дерева их не больше двух.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LineWeights {
    len: usize,
    pairs: [(usize, f32); 2],
}

impl LineWeights {
    fn single(index: usize) -> Self {
        Self {
            len: 1,
            pairs: [(index, 1.0), (0, 0.0)],
        }
    }

    pub(crate) fn as_slice(&self) -> &[(usize, f32)] {
        &self.pairs[..self.len]
    }
}

/// [`line_weights`] без аллокации: тот же расчёт, результат в фиксированном массиве.
pub(crate) fn line_weights_inline(thresholds: &[f32], value: f32) -> LineWeights {
    let Some(&first) = thresholds.first() else {
        return LineWeights {
            len: 0,
            pairs: [(0, 0.0); 2],
        };
    };
    let last_index = thresholds.len() - 1;
    if value <= first {
        return LineWeights::single(0);
    }
    if value >= thresholds[last_index] {
        return LineWeights::single(last_index);
    }
    let upper = thresholds.partition_point(|&threshold| threshold < value);
    let lower = upper - 1;
    let span = thresholds[upper] - thresholds[lower];
    let factor = (value - thresholds[lower]) / span;
    LineWeights {
        len: 2,
        pairs: [(lower, 1.0 - factor), (upper, factor)],
    }
}

#[cfg(test)]
mod tests {
    use super::{blend, line_weights};
    use crate::pose::{Pose, Transform};
    use glam::{Quat, Vec3};

    fn pose(translation: Vec3, rotation: Quat) -> Pose {
        Pose {
            transforms: vec![Transform {
                translation,
                rotation,
                scale: Vec3::ONE,
            }],
        }
    }

    #[test]
    fn blend_normalizes_weights_and_ignores_invalid_ones() {
        let a = pose(Vec3::new(0.0, 0.0, 0.0), Quat::IDENTITY);
        let b = pose(Vec3::new(4.0, 0.0, 0.0), Quat::IDENTITY);
        let mut out = pose(Vec3::ZERO, Quat::IDENTITY);
        blend(
            &[(&a, 1.0), (&b, 3.0), (&b, -5.0), (&b, f32::NAN)],
            &mut out,
        );
        assert!((out.transforms[0].translation.x - 3.0).abs() < 1e-6);
    }

    #[test]
    fn blend_aligns_quaternion_sign_before_averaging() {
        let identity = pose(Vec3::ZERO, Quat::IDENTITY);
        // Тот же поворот, но с противоположным знаком кватерниона.
        let flipped = pose(Vec3::ZERO, -Quat::from_rotation_z(0.2));
        let mut out = identity.clone();
        blend(&[(&identity, 0.5), (&flipped, 0.5)], &mut out);
        let expected = Quat::from_rotation_z(0.1);
        assert!(out.transforms[0].rotation.dot(expected).abs() > 0.999_99);
    }

    #[test]
    fn blend_with_zero_total_weight_keeps_the_output() {
        let a = pose(Vec3::X, Quat::IDENTITY);
        let mut out = pose(Vec3::Y, Quat::IDENTITY);
        blend(&[(&a, 0.0)], &mut out);
        assert_eq!(out.transforms[0].translation, Vec3::Y);
    }

    #[test]
    fn line_weights_clamp_outside_and_split_between_thresholds() {
        let thresholds = [0.0, 1.0, 2.0];
        assert_eq!(line_weights(&thresholds, -3.0), [(0, 1.0)]);
        assert_eq!(line_weights(&thresholds, 9.0), [(2, 1.0)]);
        assert_eq!(line_weights(&thresholds, 0.0), [(0, 1.0)]);
        let middle = line_weights(&thresholds, 0.25);
        assert_eq!(middle.len(), 2);
        assert_eq!(middle[0].0, 0);
        assert!((middle[0].1 - 0.75).abs() < 1e-6);
        assert_eq!(middle[1].0, 1);
        assert!((middle[1].1 - 0.25).abs() < 1e-6);
        let on_second = line_weights(&thresholds, 1.5);
        assert_eq!(on_second[0].0, 1);
        assert!((on_second[1].1 - 0.5).abs() < 1e-6);
    }

    #[test]
    fn line_weights_of_empty_tree_are_empty() {
        assert!(line_weights(&[], 0.5).is_empty());
    }
}
