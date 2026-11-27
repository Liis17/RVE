use std::collections::BTreeMap;

use crate::narrow::PairKey;
use crate::world::{ActivePair, BodyHandle, PairKind};

/// Компонента динамических тел, связанных активными collision-парами.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Island {
    pub(crate) bodies: Vec<BodyHandle>,
    pub(crate) pairs: Vec<PairKey>,
}

impl Island {
    pub(crate) fn has_awake_body(&self, mut is_awake: impl FnMut(BodyHandle) -> bool) -> bool {
        self.bodies.iter().copied().any(&mut is_awake)
    }
}

/// Строит острова по всем динамическим телам и collision-парам между ними.
/// Пары, связывающие dynamic со static или kinematic, принадлежат острову dynamic.
pub(crate) fn build(
    dynamic_bodies: impl IntoIterator<Item = BodyHandle>,
    pairs: &BTreeMap<PairKey, ActivePair>,
) -> Vec<Island> {
    let mut handles: Vec<_> = dynamic_bodies.into_iter().collect();
    handles.sort_unstable();
    handles.dedup();

    let indices: BTreeMap<_, _> = handles
        .iter()
        .enumerate()
        .map(|(index, handle)| (*handle, index))
        .collect();
    let mut sets = DisjointSets::new(handles.len());
    for (key, pair) in pairs {
        if pair.kind != PairKind::Collision {
            continue;
        }
        if let (Some(first), Some(second)) = (
            indices.get(&BodyHandle(key.first)),
            indices.get(&BodyHandle(key.second)),
        ) {
            sets.union(*first, *second);
        }
    }

    let mut members_by_root: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (index, handle) in handles.into_iter().enumerate() {
        let root = sets.find(index);
        members_by_root.entry(root).or_default().push(handle);
    }

    let mut islands: Vec<_> = members_by_root
        .into_values()
        .map(|bodies| Island {
            bodies,
            pairs: Vec::new(),
        })
        .collect();
    islands.sort_unstable_by_key(|island| island.bodies[0]);

    let island_by_body: BTreeMap<_, _> = islands
        .iter()
        .enumerate()
        .flat_map(|(island_index, island)| {
            island
                .bodies
                .iter()
                .map(move |handle| (*handle, island_index))
        })
        .collect();
    for (key, pair) in pairs {
        if pair.kind != PairKind::Collision {
            continue;
        }
        let first = island_by_body.get(&BodyHandle(key.first));
        let second = island_by_body.get(&BodyHandle(key.second));
        let island_index = match (first, second) {
            (Some(first), Some(second)) => {
                debug_assert_eq!(first, second);
                Some(*first)
            }
            (Some(first), None) => Some(*first),
            (None, Some(second)) => Some(*second),
            (None, None) => None,
        };
        if let Some(island_index) = island_index {
            islands[island_index].pairs.push(*key);
        }
    }
    islands
}

struct DisjointSets {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSets {
    fn new(count: usize) -> Self {
        Self {
            parent: (0..count).collect(),
            rank: vec![0; count],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            self.parent[index] = self.find(self.parent[index]);
        }
        self.parent[index]
    }

    fn union(&mut self, first: usize, second: usize) {
        let mut first = self.find(first);
        let mut second = self.find(second);
        if first == second {
            return;
        }
        if self.rank[first] < self.rank[second] {
            std::mem::swap(&mut first, &mut second);
        }
        self.parent[second] = first;
        if self.rank[first] == self.rank[second] {
            self.rank[first] += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::ActivePair;

    fn pairs(entries: &[(u64, u64, PairKind)]) -> BTreeMap<PairKey, ActivePair> {
        entries
            .iter()
            .map(|(first, second, kind)| {
                (
                    PairKey::new(*first, *second),
                    ActivePair {
                        kind: *kind,
                        manifold: None,
                    },
                )
            })
            .collect()
    }

    fn body_handles(islands: &[Island]) -> Vec<Vec<BodyHandle>> {
        islands.iter().map(|island| island.bodies.clone()).collect()
    }

    #[test]
    fn empty_and_isolated_dynamic_bodies_make_expected_islands() {
        assert!(build([], &BTreeMap::new()).is_empty());

        let islands = build([BodyHandle(9), BodyHandle(2)], &BTreeMap::new());
        assert_eq!(
            body_handles(&islands),
            vec![vec![BodyHandle(2)], vec![BodyHandle(9)]]
        );
    }

    #[test]
    fn chain_and_stack_contacts_form_one_island() {
        let contacts = pairs(&[
            (1, 2, PairKind::Collision),
            (2, 3, PairKind::Collision),
            (3, 4, PairKind::Collision),
        ]);
        let islands = build((1..=4).map(BodyHandle), &contacts);
        assert_eq!(
            body_handles(&islands),
            vec![(1..=4).map(BodyHandle).collect::<Vec<_>>()]
        );
    }

    #[test]
    fn two_stacks_on_one_static_floor_remain_separate() {
        let contacts = pairs(&[
            (1, 100, PairKind::Collision),
            (1, 2, PairKind::Collision),
            (2, 3, PairKind::Collision),
            (4, 100, PairKind::Collision),
            (4, 5, PairKind::Collision),
        ]);
        let islands = build(
            (1..=5).filter(|handle| *handle != 100).map(BodyHandle),
            &contacts,
        );
        assert_eq!(
            body_handles(&islands),
            vec![
                vec![BodyHandle(1), BodyHandle(2), BodyHandle(3)],
                vec![BodyHandle(4), BodyHandle(5)],
            ]
        );
    }

    #[test]
    fn static_kinematic_and_trigger_pairs_do_not_join_dynamic_islands() {
        let contacts = pairs(&[
            (1, 50, PairKind::Collision),
            (2, 50, PairKind::Collision),
            (2, 60, PairKind::Collision),
            (1, 2, PairKind::Trigger),
        ]);
        let islands = build([BodyHandle(2), BodyHandle(1)], &contacts);
        assert_eq!(
            body_handles(&islands),
            vec![vec![BodyHandle(1)], vec![BodyHandle(2)]]
        );
        assert_eq!(islands[0].pairs, vec![PairKey::new(1, 50)]);
        assert_eq!(
            islands[1].pairs,
            vec![PairKey::new(2, 50), PairKey::new(2, 60)]
        );
    }

    #[test]
    fn sleeping_members_stay_in_the_island_and_awake_state_is_detected() {
        let contacts = pairs(&[(1, 2, PairKind::Collision)]);
        let islands = build([BodyHandle(1), BodyHandle(2)], &contacts);
        assert_eq!(islands[0].bodies, vec![BodyHandle(1), BodyHandle(2)]);
        assert!(!islands[0].has_awake_body(|_| false));
        assert!(islands[0].has_awake_body(|handle| handle == BodyHandle(2)));
    }

    #[test]
    fn dynamic_contact_islands_are_built_without_sleep_metadata() {
        let contacts = pairs(&[(1, 2, PairKind::Collision)]);
        let islands = build([BodyHandle(1), BodyHandle(2)], &contacts);
        assert_eq!(
            body_handles(&islands),
            vec![vec![BodyHandle(1), BodyHandle(2)]]
        );
    }

    #[test]
    fn awake_island_keeps_separate_fully_sleeping_islands_in_the_partition() {
        let contacts = pairs(&[(2, 3, PairKind::Collision)]);
        let islands = build([BodyHandle(1), BodyHandle(2), BodyHandle(3)], &contacts);
        assert_eq!(
            body_handles(&islands),
            vec![vec![BodyHandle(1)], vec![BodyHandle(2), BodyHandle(3)]]
        );
        assert!(!islands[1].has_awake_body(|handle| handle == BodyHandle(1)));
    }

    #[test]
    fn body_and_pair_order_is_deterministic() {
        let first_pairs = pairs(&[
            (8, 9, PairKind::Collision),
            (1, 2, PairKind::Collision),
            (2, 3, PairKind::Collision),
        ]);
        let second_pairs = pairs(&[
            (2, 3, PairKind::Collision),
            (1, 2, PairKind::Collision),
            (8, 9, PairKind::Collision),
        ]);
        let first = build([9, 8, 3, 2, 1].map(BodyHandle), &first_pairs);
        let second = build([1, 2, 3, 8, 9].map(BodyHandle), &second_pairs);
        assert_eq!(first, second);
        assert_eq!(first[0].pairs, vec![PairKey::new(1, 2), PairKey::new(2, 3)]);
    }
}
