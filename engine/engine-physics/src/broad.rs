use std::fmt;

use crate::Aabb;

pub const MAX_BROADPHASE_ITEMS: usize = 16_384;
pub const MAX_BROADPHASE_PAIRS: usize = 1_000_000;
pub const MAX_BROADPHASE_COMPARISONS: usize = 1_000_000;

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

/// Generates deterministic overlapping AABB pairs using sweep-and-prune on X.
pub fn sweep_and_prune(bounds: &[Aabb]) -> Result<Vec<CandidatePair>, BroadPhaseError> {
    if bounds.len() > MAX_BROADPHASE_ITEMS {
        return Err(BroadPhaseError::TooManyItems);
    }

    let mut order: Vec<usize> = (0..bounds.len()).collect();
    order.sort_by(|&left, &right| {
        bounds[left]
            .min
            .x
            .total_cmp(&bounds[right].min.x)
            .then_with(|| left.cmp(&right))
    });

    let mut active = Vec::<usize>::new();
    let mut pairs = Vec::new();
    let mut comparisons = 0;
    for current in order {
        active.retain(|&index| bounds[index].max.x >= bounds[current].min.x);
        for &index in &active {
            if comparisons >= MAX_BROADPHASE_COMPARISONS {
                return Err(BroadPhaseError::ComparisonLimitExceeded);
            }
            comparisons += 1;
            if bounds[index].overlaps(bounds[current]) {
                if pairs.len() >= MAX_BROADPHASE_PAIRS {
                    return Err(BroadPhaseError::TooManyPairs);
                }
                pairs.push(CandidatePair {
                    first: index.min(current),
                    second: index.max(current),
                });
            }
        }
        active.push(current);
    }
    pairs.sort_by_key(|pair| (pair.first, pair.second));
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::{
        BroadPhaseError, CandidatePair, MAX_BROADPHASE_COMPARISONS, MAX_BROADPHASE_ITEMS,
        sweep_and_prune,
    };
    use crate::Aabb;
    use glam::Vec3;

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
    fn sweep_and_prune_enforces_item_limit() {
        let bounds = vec![Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap(); MAX_BROADPHASE_ITEMS + 1];
        assert_eq!(sweep_and_prune(&bounds), Err(BroadPhaseError::TooManyItems));
    }

    #[test]
    fn sweep_and_prune_enforces_comparison_limit_for_sparse_y_overlaps() {
        let count = ((2.0 * MAX_BROADPHASE_COMPARISONS as f64).sqrt().ceil() as usize) + 1;
        let bounds: Vec<_> = (0..count)
            .map(|index| {
                let y = index as f32 * 2.0;
                Aabb::new(Vec3::new(0.0, y, 0.0), Vec3::new(1.0, y + 1.0, 1.0)).unwrap()
            })
            .collect();
        assert_eq!(
            sweep_and_prune(&bounds),
            Err(BroadPhaseError::ComparisonLimitExceeded)
        );
    }
}
