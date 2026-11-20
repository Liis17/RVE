use std::cmp::Ordering;
use std::fmt;

use crate::Aabb;

pub const MAX_BROADPHASE_ITEMS: usize = 16_384;
pub const MAX_BROADPHASE_PAIRS: usize = 1_000_000;
pub const MAX_BROADPHASE_COMPARISONS: usize = 1_000_000;
/// Смена оси требует, чтобы оценка новой оси была больше текущей минимум в полтора раза.
const AXIS_SWITCH_RATIO: f64 = 1.5;
/// Если изменилась больше доли `1 / FULL_SORT_CHURN_DIVISOR` тел, порядок пересортировывается целиком.
const FULL_SORT_CHURN_DIVISOR: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidatePair {
    pub first: usize,
    pub second: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BroadPhaseError {
    TooManyItems,
    TooManyPairs,
    ComparisonLimitExceeded,
}

impl fmt::Display for BroadPhaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TooManyItems => "broadphase item limit exceeded",
            Self::TooManyPairs => "broadphase candidate pair limit exceeded",
            Self::ComparisonLimitExceeded => "broadphase comparison limit exceeded",
        })
    }
}

impl std::error::Error for BroadPhaseError {}

/// Тело для broadphase. Ключ уникален и строго возрастает во входном срезе; по нему порядок
/// sweep переносится между обновлениями.
#[derive(Clone, Copy, Debug)]
pub struct BroadItem {
    pub key: u64,
    pub bounds: Aabb,
    /// `false` — тело не сравнивается с другими непарными телами (неактивная статика, спящее тело).
    pub paired: bool,
}

/// Результат одного обновления [`BroadPhase`].
#[derive(Clone, Debug)]
pub struct BroadPhaseOutput {
    /// Пересекающиеся пары индексов входного среза, по возрастанию `(first, second)`.
    pub pairs: Vec<CandidatePair>,
    /// Выполненные сравнения тел по оси.
    pub comparisons: usize,
    /// Позиция в порядке sweep, до которой проход дошёл; `None`, если обработаны все тела.
    pub cut: Option<usize>,
    /// Позиция каждого входного тела в порядке sweep.
    positions: Vec<usize>,
}

impl BroadPhaseOutput {
    /// Сравнивалась ли пара тел `first` и `second` (индексы входного среза). Пара сравнена, когда
    /// более позднее из двух тел обработано до точки обрезки.
    pub fn pair_compared(&self, first: usize, second: usize) -> bool {
        match self.cut {
            None => true,
            Some(cut) => self.positions[first].max(self.positions[second]) < cut,
        }
    }
}

/// Персистентный sweep-and-prune. Порядок по оси переживает обновления: при неизменных ключах
/// переиспользуется за O(N), при изменении набора досортировывается вставками. Ось выбирается по
/// разбросу центров относительно размера тел, с гистерезисом.
pub struct BroadPhase {
    /// Ось sweep: 0 — X, 1 — Y, 2 — Z.
    axis: usize,
    /// Ось уже выбрана по оценке; до этого первое обновление с двумя телами выбирает лучшую сразу.
    scored: bool,
    /// Ключи тел прошлого обновления, по возрастанию.
    keys: Vec<u64>,
    /// Индексы входного среза в порядке sweep.
    order: Vec<usize>,
    /// Порог сравнений; обычно [`MAX_BROADPHASE_COMPARISONS`], тесты мира снижают его.
    pub(crate) comparison_limit: usize,
}

impl Default for BroadPhase {
    fn default() -> Self {
        Self {
            axis: 0,
            scored: false,
            keys: Vec::new(),
            order: Vec::new(),
            comparison_limit: MAX_BROADPHASE_COMPARISONS,
        }
    }
}

impl BroadPhase {
    /// Обновляет порядок по текущему набору тел и выполняет sweep. Превышение порога сравнений не
    /// ошибка: проход останавливается, и результат содержит `cut`.
    pub fn update(&mut self, items: &[BroadItem]) -> Result<BroadPhaseOutput, BroadPhaseError> {
        if items.len() > MAX_BROADPHASE_ITEMS {
            return Err(BroadPhaseError::TooManyItems);
        }
        debug_assert!(
            items.windows(2).all(|pair| pair[0].key < pair[1].key),
            "ключи broadphase должны строго возрастать"
        );
        let keys_unchanged = self.keys.len() == items.len()
            && self
                .keys
                .iter()
                .zip(items)
                .all(|(key, item)| *key == item.key);
        let mut full_sort = !keys_unchanged && self.remap_order(items);
        if self.choose_axis(items) {
            full_sort = true;
        }
        let axis = self.axis;
        if full_sort {
            self.order
                .sort_unstable_by(|&a, &b| sweep_order(items, axis, a, b));
        } else {
            insertion_sort(&mut self.order, items, axis);
        }
        self.sweep(items)
    }

    /// Переносит порядок прошлого обновления на новый набор ключей. Возвращает `true`, если тел
    /// изменилось слишком много для вставок.
    fn remap_order(&mut self, items: &[BroadItem]) -> bool {
        let previous_len = self.order.len();
        let mut order = Vec::with_capacity(items.len());
        for &previous in &self.order {
            if let Ok(index) = items.binary_search_by_key(&self.keys[previous], |item| item.key) {
                order.push(index);
            }
        }
        let kept = order.len();
        let mut present = vec![false; items.len()];
        for &index in &order {
            present[index] = true;
        }
        order.extend((0..items.len()).filter(|&index| !present[index]));
        let churn = (items.len() - kept) + (previous_len - kept);
        self.keys = items.iter().map(|item| item.key).collect();
        self.order = order;
        churn > items.len() / FULL_SORT_CHURN_DIVISOR
    }

    /// Выбирает ось по оценке разброса; возвращает `true`, если ось сменилась.
    fn choose_axis(&mut self, items: &[BroadItem]) -> bool {
        if items.len() < 2 {
            return false;
        }
        let scores = [0, 1, 2].map(|axis| axis_score(items, axis));
        let mut best = 0;
        for (axis, &score) in scores.iter().enumerate().skip(1) {
            if score > scores[best] {
                best = axis;
            }
        }
        let previous = self.axis;
        let switch = !self.scored || scores[best] > AXIS_SWITCH_RATIO * scores[previous];
        self.scored = true;
        if switch {
            self.axis = best;
        }
        self.axis != previous
    }

    fn sweep(&self, items: &[BroadItem]) -> Result<BroadPhaseOutput, BroadPhaseError> {
        let axis = self.axis;
        let mut positions = vec![0; items.len()];
        for (position, &index) in self.order.iter().enumerate() {
            positions[index] = position;
        }
        let mut active_paired = Vec::<usize>::new();
        let mut active_unpaired = Vec::<usize>::new();
        let mut pairs = Vec::new();
        let mut comparisons = 0;
        let mut cut = None;
        for (position, &current) in self.order.iter().enumerate() {
            let body = items[current];
            let start = body.bounds.min[axis];
            active_paired.retain(|&index| items[index].bounds.max[axis] >= start);
            active_unpaired.retain(|&index| items[index].bounds.max[axis] >= start);
            let candidates = active_paired.len()
                + if body.paired {
                    active_unpaired.len()
                } else {
                    0
                };
            // Тело обрабатывается целиком или не обрабатывается: иначе часть его пар осталась бы
            // без сравнения, но без признака обрезки.
            if comparisons + candidates > self.comparison_limit {
                cut = Some(position);
                break;
            }
            for &index in active_paired
                .iter()
                .chain(active_unpaired.iter().filter(|_| body.paired))
            {
                comparisons += 1;
                if items[index].bounds.overlaps(body.bounds) {
                    if pairs.len() >= MAX_BROADPHASE_PAIRS {
                        return Err(BroadPhaseError::TooManyPairs);
                    }
                    pairs.push(CandidatePair {
                        first: index.min(current),
                        second: index.max(current),
                    });
                }
            }
            if body.paired {
                active_paired.push(current);
            } else {
                active_unpaired.push(current);
            }
        }
        pairs.sort_by_key(|pair| (pair.first, pair.second));
        Ok(BroadPhaseOutput {
            pairs,
            comparisons,
            cut,
            positions,
        })
    }
}

/// Порядок sweep: по минимуму на оси, при равенстве — по индексу. Порядок полный, поэтому любая
/// сортировка даёт один и тот же результат.
fn sweep_order(items: &[BroadItem], axis: usize, a: usize, b: usize) -> Ordering {
    items[a].bounds.min[axis]
        .total_cmp(&items[b].bounds.min[axis])
        .then(a.cmp(&b))
}

fn insertion_sort(order: &mut [usize], items: &[BroadItem], axis: usize) {
    for index in 1..order.len() {
        let mut position = index;
        while position > 0
            && sweep_order(items, axis, order[position], order[position - 1]) == Ordering::Less
        {
            order.swap(position, position - 1);
            position -= 1;
        }
    }
}

/// Дисперсия центров по оси, делённая на квадрат средней ширины тел. Считается в f64 двумя
/// проходами, поэтому координаты порядка ±1e30 не переполняют и не дают NaN.
fn axis_score(items: &[BroadItem], axis: usize) -> f64 {
    let count = items.len() as f64;
    let center = |item: &BroadItem| {
        0.5 * (f64::from(item.bounds.min[axis]) + f64::from(item.bounds.max[axis]))
    };
    let width =
        |item: &BroadItem| f64::from(item.bounds.max[axis]) - f64::from(item.bounds.min[axis]);
    let mean_center = items.iter().map(center).sum::<f64>() / count;
    let mean_width = items.iter().map(width).sum::<f64>() / count;
    let variance = items
        .iter()
        .map(|item| {
            let delta = center(item) - mean_center;
            delta * delta
        })
        .sum::<f64>()
        / count;
    variance / (mean_width * mean_width + 1.0e-12)
}

/// Generates deterministic overlapping AABB pairs using sweep-and-prune on the best axis.
pub fn sweep_and_prune(bounds: &[Aabb]) -> Result<Vec<CandidatePair>, BroadPhaseError> {
    sweep_and_prune_pairable(bounds, |_| true)
}

/// Like [`sweep_and_prune`], but two bodies with `paired(index) == false` are never compared.
///
/// Used to skip static–static pairs that the narrowphase filter would discard anyway, so they
/// neither cost comparisons against the limit nor appear in the candidate list.
///
/// Одноразовая обёртка над [`BroadPhase`]: при превышении порога сравнений возвращает ошибку.
pub fn sweep_and_prune_pairable(
    bounds: &[Aabb],
    paired: impl Fn(usize) -> bool,
) -> Result<Vec<CandidatePair>, BroadPhaseError> {
    let items: Vec<BroadItem> = bounds
        .iter()
        .enumerate()
        .map(|(index, &bounds)| BroadItem {
            key: index as u64,
            bounds,
            paired: paired(index),
        })
        .collect();
    let output = BroadPhase::default().update(&items)?;
    if output.cut.is_some() {
        return Err(BroadPhaseError::ComparisonLimitExceeded);
    }
    Ok(output.pairs)
}

#[cfg(test)]
mod tests {
    use super::{
        BroadItem, BroadPhase, BroadPhaseError, CandidatePair, MAX_BROADPHASE_COMPARISONS,
        MAX_BROADPHASE_ITEMS, sweep_and_prune, sweep_and_prune_pairable,
    };
    use crate::Aabb;
    use glam::Vec3;
    use std::collections::BTreeMap;

    /// Детерминированный LCG без новой зависимости.
    struct Lcg(u64);

    impl Lcg {
        fn unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }

        fn range(&mut self, low: f32, high: f32) -> f32 {
            low + (high - low) * self.unit()
        }

        fn bounds(&mut self, extent: Vec3, max_size: Vec3) -> Aabb {
            let min = Vec3::new(
                self.range(-extent.x, extent.x),
                self.range(-extent.y, extent.y),
                self.range(-extent.z, extent.z),
            );
            let size = Vec3::new(
                self.range(0.0, max_size.x),
                self.range(0.0, max_size.y),
                self.range(0.0, max_size.z),
            );
            Aabb::new(min, min + size).unwrap()
        }
    }

    /// Копия прежнего sweep по оси X: эталон для сравнения результатов.
    fn legacy_x_sweep(
        bounds: &[Aabb],
        paired: impl Fn(usize) -> bool,
    ) -> Result<Vec<CandidatePair>, BroadPhaseError> {
        let mut order: Vec<usize> = (0..bounds.len()).collect();
        order.sort_by(|&left, &right| {
            bounds[left]
                .min
                .x
                .total_cmp(&bounds[right].min.x)
                .then_with(|| left.cmp(&right))
        });
        let mut active_paired = Vec::<usize>::new();
        let mut active_unpaired = Vec::<usize>::new();
        let mut pairs = Vec::new();
        let mut comparisons = 0;
        for current in order {
            let current_paired = paired(current);
            active_paired.retain(|&index| bounds[index].max.x >= bounds[current].min.x);
            active_unpaired.retain(|&index| bounds[index].max.x >= bounds[current].min.x);
            for &index in active_paired
                .iter()
                .chain(active_unpaired.iter().filter(|_| current_paired))
            {
                if comparisons >= MAX_BROADPHASE_COMPARISONS {
                    return Err(BroadPhaseError::ComparisonLimitExceeded);
                }
                comparisons += 1;
                if bounds[index].overlaps(bounds[current]) {
                    pairs.push(CandidatePair {
                        first: index.min(current),
                        second: index.max(current),
                    });
                }
            }
            if current_paired {
                active_paired.push(current);
            } else {
                active_unpaired.push(current);
            }
        }
        pairs.sort_by_key(|pair| (pair.first, pair.second));
        Ok(pairs)
    }

    fn brute_force_pairs(bounds: &[Aabb], paired: &[bool]) -> Vec<CandidatePair> {
        let mut pairs = Vec::new();
        for first in 0..bounds.len() {
            for second in first + 1..bounds.len() {
                if (paired[first] || paired[second]) && bounds[first].overlaps(bounds[second]) {
                    pairs.push(CandidatePair { first, second });
                }
            }
        }
        pairs
    }

    fn items_from(map: &BTreeMap<u64, (Aabb, bool)>) -> Vec<BroadItem> {
        map.iter()
            .map(|(&key, &(bounds, paired))| BroadItem {
                key,
                bounds,
                paired,
            })
            .collect()
    }

    #[test]
    fn sweep_and_prune_returns_only_closed_aabb_overlaps_in_stable_order() {
        let bounds = [
            Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap(),
            Aabb::new(Vec3::X, Vec3::splat(2.0)).unwrap(),
            Aabb::new(Vec3::splat(4.0), Vec3::splat(5.0)).unwrap(),
            Aabb::new(Vec3::new(0.5, 4.0, 0.0), Vec3::new(1.5, 5.0, 1.0)).unwrap(),
        ];
        assert_eq!(
            sweep_and_prune(&bounds).unwrap(),
            vec![CandidatePair {
                first: 0,
                second: 1
            }]
        );
    }

    #[test]
    fn unpaired_bodies_are_not_compared_with_each_other_but_still_pair_with_paired_ones() {
        let bounds = [
            Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap(),
            Aabb::new(Vec3::splat(0.5), Vec3::splat(1.5)).unwrap(),
            Aabb::new(Vec3::splat(0.25), Vec3::splat(0.75)).unwrap(),
        ];
        let pairs = sweep_and_prune_pairable(&bounds, |index| index == 2).unwrap();
        assert_eq!(
            pairs,
            vec![
                CandidatePair {
                    first: 0,
                    second: 2
                },
                CandidatePair {
                    first: 1,
                    second: 2
                },
            ]
        );
    }

    #[test]
    fn sweep_and_prune_enforces_item_limit() {
        let bounds = vec![Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap(); MAX_BROADPHASE_ITEMS + 1];
        assert_eq!(sweep_and_prune(&bounds), Err(BroadPhaseError::TooManyItems));
    }

    #[test]
    fn bodies_spread_along_y_need_no_comparisons_on_the_best_axis() {
        // На оси X все тела совпадают, и sweep по X потратил бы больше лимита сравнений.
        let count = ((2.0 * MAX_BROADPHASE_COMPARISONS as f64).sqrt().ceil() as usize) + 1;
        let bounds: Vec<_> = (0..count)
            .map(|index| {
                let y = index as f32 * 2.0;
                Aabb::new(Vec3::new(0.0, y, 0.0), Vec3::new(1.0, y + 1.0, 1.0)).unwrap()
            })
            .collect();
        assert_eq!(sweep_and_prune(&bounds), Ok(Vec::new()));
    }

    #[test]
    fn dense_cluster_exceeds_the_comparison_limit_in_the_free_function() {
        // N(N − 1) / 2 > MAX_BROADPHASE_COMPARISONS: все тела пересекаются по всем осям.
        let count = 1415;
        let bounds = vec![Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap(); count];
        assert_eq!(
            sweep_and_prune(&bounds),
            Err(BroadPhaseError::ComparisonLimitExceeded)
        );
    }

    #[test]
    fn broad_phase_matches_brute_force_and_legacy_x_sweep_on_random_scenes() {
        let shapes = [
            (Vec3::splat(20.0), Vec3::splat(3.0)),
            (Vec3::new(5.0, 5.0, 200.0), Vec3::new(2.0, 2.0, 1.0)),
            (Vec3::splat(10.0), Vec3::ZERO),
        ];
        for seed in [1u64, 2, 3, 7] {
            for &(extent, max_size) in &shapes {
                let mut random = Lcg(seed);
                let bounds: Vec<Aabb> = (0..220).map(|_| random.bounds(extent, max_size)).collect();
                let paired: Vec<bool> = (0..bounds.len()).map(|_| random.unit() < 0.5).collect();
                let expected = brute_force_pairs(&bounds, &paired);
                let actual = sweep_and_prune_pairable(&bounds, |index| paired[index]).unwrap();
                let legacy = legacy_x_sweep(&bounds, |index| paired[index]).unwrap();
                assert_eq!(actual, expected, "seed {seed}");
                assert_eq!(legacy, expected, "seed {seed}");
            }
        }
    }

    #[test]
    fn persistent_phase_matches_brute_force_across_inserts_removals_and_moves() {
        let mut random = Lcg(11);
        let mut bodies: BTreeMap<u64, (Aabb, bool)> = BTreeMap::new();
        for key in (0..180u64).map(|key| key * 3) {
            let bounds = random.bounds(Vec3::new(30.0, 4.0, 30.0), Vec3::splat(4.0));
            bodies.insert(key, (bounds, random.unit() < 0.7));
        }
        let mut phase = BroadPhase::default();
        for round in 0..6 {
            let items = items_from(&bodies);
            let bounds: Vec<Aabb> = items.iter().map(|item| item.bounds).collect();
            let paired: Vec<bool> = items.iter().map(|item| item.paired).collect();
            let output = phase.update(&items).unwrap();
            assert_eq!(
                output.pairs,
                brute_force_pairs(&bounds, &paired),
                "round {round}"
            );
            assert_eq!(output.cut, None);

            // Удаляем часть тел, добавляем новые между старыми ключами и двигаем оставшиеся.
            let keys: Vec<u64> = bodies.keys().copied().collect();
            for (position, key) in keys.iter().enumerate() {
                if position % 4 == round % 4 {
                    bodies.remove(key);
                } else if position % 5 == 0 {
                    let moved = random.bounds(Vec3::new(30.0, 4.0, 30.0), Vec3::splat(4.0));
                    bodies.get_mut(key).unwrap().0 = moved;
                }
            }
            for step in 0..40u64 {
                let key = step * 13 + 1 + round as u64;
                let bounds = random.bounds(Vec3::new(30.0, 4.0, 30.0), Vec3::splat(4.0));
                bodies.insert(key, (bounds, random.unit() < 0.7));
            }
        }
    }

    #[test]
    fn axis_switches_only_when_the_new_axis_clearly_dominates_and_does_not_jitter() {
        let mut random = Lcg(5);
        let mut phase = BroadPhase::default();
        let scene = |random: &mut Lcg, spread_x: f32, spread_z: f32| -> Vec<BroadItem> {
            (0..300u64)
                .map(|key| {
                    let x = random.range(0.0, spread_x);
                    let z = random.range(0.0, spread_z);
                    BroadItem {
                        key,
                        bounds: Aabb::new(Vec3::new(x, 0.0, z), Vec3::new(x + 1.0, 1.0, z + 1.0))
                            .unwrap(),
                        paired: true,
                    }
                })
                .collect()
        };

        // Разброс только по Z: первое обновление сразу выбирает Z.
        phase.update(&scene(&mut random, 0.0, 100.0)).unwrap();
        assert_eq!(phase.axis, 2);
        // X чуть больше Z: в пределах гистерезиса ось не меняется.
        phase.update(&scene(&mut random, 100.0, 90.0)).unwrap();
        assert_eq!(phase.axis, 2);
        // Разброс по X намного больше: ось переключается.
        phase.update(&scene(&mut random, 100.0, 10.0)).unwrap();
        assert_eq!(phase.axis, 0);
        // Небольшие колебания не возвращают ось обратно.
        for _ in 0..3 {
            phase.update(&scene(&mut random, 100.0, 100.0)).unwrap();
            assert_eq!(phase.axis, 0);
        }
    }

    #[test]
    fn degenerate_coordinates_and_equal_scores_keep_pairs_exact() {
        let huge = Aabb::new(Vec3::splat(-1.0e30), Vec3::splat(1.0e30)).unwrap();
        let point = Aabb::new(Vec3::splat(0.5), Vec3::splat(0.5)).unwrap();
        let bounds = [
            huge,
            point,
            Aabb::new(Vec3::splat(-2.0), Vec3::splat(-1.0)).unwrap(),
            point,
            Aabb::new(Vec3::new(1.0e30, 0.0, 0.0), Vec3::new(1.0e30, 1.0, 1.0)).unwrap(),
        ];
        let paired = [true; 5];
        assert_eq!(
            sweep_and_prune(&bounds).unwrap(),
            brute_force_pairs(&bounds, &paired)
        );

        // Одинаковые тела: все оценки равны нулю, ось не меняется и пары полные.
        let same = [Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap(); 4];
        let mut phase = BroadPhase::default();
        let items: Vec<BroadItem> = same
            .iter()
            .enumerate()
            .map(|(key, &bounds)| BroadItem {
                key: key as u64,
                bounds,
                paired: true,
            })
            .collect();
        let output = phase.update(&items).unwrap();
        assert_eq!(phase.axis, 0);
        assert_eq!(output.pairs.len(), 6);
    }

    #[test]
    fn truncated_sweep_compares_only_bodies_before_the_cut() {
        let mut phase = BroadPhase {
            comparison_limit: 3,
            ..BroadPhase::default()
        };
        let items: Vec<BroadItem> = (0..4u64)
            .map(|key| BroadItem {
                key,
                bounds: Aabb::new(
                    Vec3::new(key as f32, 0.0, 0.0),
                    Vec3::new(key as f32 + 10.0, 1.0, 1.0),
                )
                .unwrap(),
                paired: true,
            })
            .collect();
        let output = phase.update(&items).unwrap();
        assert_eq!(output.cut, Some(3));
        assert_eq!(output.comparisons, 3);
        assert_eq!(output.pairs.len(), 3);
        assert!(output.pair_compared(0, 1));
        assert!(!output.pair_compared(2, 3));
        assert!(!output.pair_compared(0, 3));
    }
}
