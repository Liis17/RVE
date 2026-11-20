//! Значимость spawn-поддеревьев: какие корни видны участнику. Чистая функция без мира и сети:
//! мост собирает наблюдателей и корни, а здесь решается, кто входит в зону и кто из неё выходит.
//!
//! Корень входит в зону, когда его расстояние до ближайшего наблюдателя не больше радиуса R, и
//! выходит, когда оно больше `1.1 R`. Между порогами решение не меняется (гистерезис). Корень с
//! `always_relevant`, корень с владельцем-наблюдателем и его сетевые предки видны всегда.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use engine_scene::network::NetId;
use glam::Vec3;

/// Во сколько раз зона выхода шире зоны входа.
pub const HYSTERESIS: f32 = 1.1;

/// Корень spawn-поддерева с данными для решения видимости.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RootInfo {
    pub net: NetId,
    pub position: Vec3,
    pub always: bool,
    /// Ближайший корень-предок: вложенный корень виден только вместе с ним.
    pub parent: Option<NetId>,
    /// Глубина в дереве мира: предки идут раньше потомков.
    pub depth: usize,
}

/// Корни, видимые участнику. `roots` — отсортированные по `depth` по возрастанию, `observers` —
/// мировые позиции объектов участника, `forced` — корни, которые участник видит всегда (его
/// объекты и их сетевые предки), `previous` — корни, видимые на прошлом шаге.
pub fn visible_roots(
    radius: f32,
    roots: &[RootInfo],
    observers: &[Vec3],
    forced: &HashSet<NetId>,
    previous: &BTreeMap<NetId, u32>,
) -> BTreeSet<NetId> {
    let enter = radius * radius;
    let leave = (radius * HYSTERESIS) * (radius * HYSTERESIS);
    let mut visible = BTreeSet::new();
    for root in roots {
        if root.parent.is_some_and(|parent| !visible.contains(&parent)) {
            continue;
        }
        let shown = root.always || forced.contains(&root.net) || {
            let distance = observers
                .iter()
                .map(|observer| observer.distance_squared(root.position))
                .fold(f32::INFINITY, f32::min);
            distance <= enter || (previous.contains_key(&root.net) && distance <= leave)
        };
        if shown {
            visible.insert(root.net);
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    const RADIUS: f32 = 10.0;

    fn root(raw: u32, x: f32) -> RootInfo {
        RootInfo {
            net: NetId::from_raw(0x8000_0000 | raw),
            position: Vec3::new(x, 0.0, 0.0),
            always: false,
            parent: None,
            depth: 0,
        }
    }

    fn net(raw: u32) -> NetId {
        NetId::from_raw(0x8000_0000 | raw)
    }

    fn decide(roots: &[RootInfo], previous: &BTreeMap<NetId, u32>) -> BTreeSet<NetId> {
        visible_roots(RADIUS, roots, &[Vec3::ZERO], &HashSet::new(), previous)
    }

    #[test]
    fn root_enters_inside_the_radius_and_stays_out_beyond_it() {
        let inside = decide(&[root(1, 10.0)], &BTreeMap::new());
        assert!(inside.contains(&net(1)));
        let outside = decide(&[root(1, 10.5)], &BTreeMap::new());
        assert!(outside.is_empty());
    }

    #[test]
    fn hysteresis_keeps_a_visible_root_between_the_thresholds() {
        let previous = BTreeMap::from([(net(1), 7)]);
        assert!(decide(&[root(1, 10.9)], &previous).contains(&net(1)));
        assert!(!decide(&[root(1, 11.1)], &previous).contains(&net(1)));
        // Без прошлого появления тот же корень между порогами не входит.
        assert!(decide(&[root(1, 10.9)], &BTreeMap::new()).is_empty());
    }

    #[test]
    fn always_relevant_and_forced_roots_are_visible_without_observers_nearby() {
        let mut always = root(1, 1000.0);
        always.always = true;
        let forced = HashSet::from([net(2)]);
        let visible = visible_roots(
            RADIUS,
            &[always, root(2, 1000.0)],
            &[Vec3::ZERO],
            &forced,
            &BTreeMap::new(),
        );
        assert_eq!(visible, BTreeSet::from([net(1), net(2)]));
    }

    #[test]
    fn nested_root_needs_a_visible_parent() {
        let parent = root(1, 1000.0);
        let mut child = root(2, 0.0);
        child.parent = Some(parent.net);
        child.depth = 1;
        assert!(decide(&[parent, child], &BTreeMap::new()).is_empty());

        let mut forced_parent = parent;
        forced_parent.always = true;
        let visible = decide(&[forced_parent, child], &BTreeMap::new());
        assert_eq!(visible, BTreeSet::from([net(1), net(2)]));
    }

    #[test]
    fn no_observers_means_no_distance_based_visibility() {
        let visible = visible_roots(
            RADIUS,
            &[root(1, 0.0)],
            &[],
            &HashSet::new(),
            &BTreeMap::new(),
        );
        assert!(visible.is_empty());
    }
}
