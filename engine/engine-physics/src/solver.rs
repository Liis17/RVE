use std::collections::BTreeMap;

use glam::Vec3;

use crate::narrow::{ContactPoint, ManifoldCache, PairKey};
use crate::world::{
    ActivePair, Body, BodyHandle, BodyKind, POSITIONAL_CORRECTION, POSITIONAL_SLOP, PairKind,
    RESTITUTION_THRESHOLD, SOLVER_ITERATIONS,
};

struct ContactConstraint {
    key: PairKey,
    index: usize,
    point: Vec3,
    normal: Vec3,
    tangent_1: Vec3,
    tangent_2: Vec3,
    friction: f32,
    restitution_bias: f32,
}

pub(crate) fn solve(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    pairs: &mut BTreeMap<PairKey, ActivePair>,
    manifolds: &mut ManifoldCache,
    _dt: f32,
) {
    let constraints = build_constraints(bodies, pairs);
    warm_start(bodies, pairs, &constraints);
    for _ in 0..SOLVER_ITERATIONS {
        for constraint in &constraints {
            solve_normal(bodies, pairs, constraint);
            solve_friction(bodies, pairs, constraint);
        }
    }
    positional_correction(bodies, pairs);
    for (key, pair) in pairs.iter() {
        if pair.kind == PairKind::Collision
            && let Some(manifold) = &pair.manifold
        {
            manifolds.store_impulses(*key, manifold);
        }
    }
}

fn build_constraints(
    bodies: &BTreeMap<BodyHandle, Body>,
    pairs: &BTreeMap<PairKey, ActivePair>,
) -> Vec<ContactConstraint> {
    let mut constraints = Vec::new();
    for (key, pair) in pairs {
        if pair.kind != PairKind::Collision {
            continue;
        }
        let Some(manifold) = &pair.manifold else {
            continue;
        };
        let (Some(body_a), Some(body_b)) = (
            bodies.get(&BodyHandle(key.first)),
            bodies.get(&BodyHandle(key.second)),
        ) else {
            continue;
        };
        if !is_awake_dynamic(body_a) && !is_awake_dynamic(body_b) {
            continue;
        }
        let friction = body_a.collider.friction.sqrt() * body_b.collider.friction.sqrt();
        let restitution = body_a.collider.restitution.max(body_b.collider.restitution);
        let (tangent_1, tangent_2) = tangent_basis(manifold.normal);
        for (index, contact) in manifold.contacts.iter().enumerate() {
            let relative_velocity =
                body_b.velocity_at(contact.position) - body_a.velocity_at(contact.position);
            let normal_speed = relative_velocity.dot(manifold.normal);
            let restitution_bias = if normal_speed < -RESTITUTION_THRESHOLD {
                restitution * normal_speed
            } else {
                0.0
            };
            constraints.push(ContactConstraint {
                key: *key,
                index,
                point: contact.position,
                normal: manifold.normal,
                tangent_1,
                tangent_2,
                friction,
                restitution_bias,
            });
        }
    }
    constraints
}

fn warm_start(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    pairs: &mut BTreeMap<PairKey, ActivePair>,
    constraints: &[ContactConstraint],
) {
    for constraint in constraints {
        let Some(contact) = contact_mut(pairs, constraint.key, constraint.index) else {
            continue;
        };
        let impulse = constraint.normal * contact.normal_impulse + contact.tangent_impulse;
        apply_pair_impulse(bodies, constraint.key, constraint.point, impulse);
    }
}

fn solve_normal(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    pairs: &mut BTreeMap<PairKey, ActivePair>,
    constraint: &ContactConstraint,
) {
    let (Some(body_a), Some(body_b)) = (
        bodies.get(&BodyHandle(constraint.key.first)),
        bodies.get(&BodyHandle(constraint.key.second)),
    ) else {
        return;
    };
    let effective_mass =
        inverse_effective_mass(body_a, body_b, constraint.point, constraint.normal);
    if effective_mass <= f32::MIN_POSITIVE {
        return;
    }
    let relative_velocity =
        body_b.velocity_at(constraint.point) - body_a.velocity_at(constraint.point);
    let delta =
        -(relative_velocity.dot(constraint.normal) + constraint.restitution_bias) / effective_mass;
    let Some(contact) = contact_mut(pairs, constraint.key, constraint.index) else {
        return;
    };
    let old_impulse = contact.normal_impulse;
    contact.normal_impulse = (old_impulse + delta).max(0.0);
    let applied = constraint.normal * (contact.normal_impulse - old_impulse);
    apply_pair_impulse(bodies, constraint.key, constraint.point, applied);
}

fn solve_friction(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    pairs: &mut BTreeMap<PairKey, ActivePair>,
    constraint: &ContactConstraint,
) {
    if constraint.friction <= 0.0 {
        return;
    }
    for tangent in [constraint.tangent_1, constraint.tangent_2] {
        let (Some(body_a), Some(body_b)) = (
            bodies.get(&BodyHandle(constraint.key.first)),
            bodies.get(&BodyHandle(constraint.key.second)),
        ) else {
            return;
        };
        let effective_mass = inverse_effective_mass(body_a, body_b, constraint.point, tangent);
        if effective_mass <= f32::MIN_POSITIVE {
            continue;
        }
        let relative_velocity =
            body_b.velocity_at(constraint.point) - body_a.velocity_at(constraint.point);
        let delta = -relative_velocity.dot(tangent) / effective_mass;
        let Some(contact) = contact_mut(pairs, constraint.key, constraint.index) else {
            return;
        };
        let old_impulse = contact.tangent_impulse;
        let mut candidate = old_impulse + tangent * delta;
        let maximum = constraint.friction * contact.normal_impulse;
        let length = candidate.length();
        if maximum.is_finite() && length > maximum && length > f32::MIN_POSITIVE {
            candidate *= maximum / length;
        }
        contact.tangent_impulse = candidate;
        let applied = candidate - old_impulse;
        apply_pair_impulse(bodies, constraint.key, constraint.point, applied);
    }
}

fn positional_correction(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    pairs: &BTreeMap<PairKey, ActivePair>,
) {
    for (key, pair) in pairs {
        if pair.kind != PairKind::Collision {
            continue;
        }
        let Some(manifold) = &pair.manifold else {
            continue;
        };
        let (Some(body_a), Some(body_b)) = (
            bodies.get(&BodyHandle(key.first)),
            bodies.get(&BodyHandle(key.second)),
        ) else {
            continue;
        };
        if !is_awake_dynamic(body_a) && !is_awake_dynamic(body_b) {
            continue;
        }
        let inverse_mass_sum = body_a.inverse_mass + body_b.inverse_mass;
        if inverse_mass_sum <= f32::MIN_POSITIVE {
            continue;
        }
        let penetration = manifold
            .contacts
            .iter()
            .map(|contact| contact.penetration)
            .sum::<f32>()
            / manifold.contacts.len().max(1) as f32;
        let correction =
            ((penetration - POSITIONAL_SLOP).max(0.0) * POSITIONAL_CORRECTION) / inverse_mass_sum;
        let translation_a = -manifold.normal * (correction * body_a.inverse_mass);
        let translation_b = manifold.normal * (correction * body_b.inverse_mass);
        if let Some(body) = bodies.get_mut(&BodyHandle(key.first)) {
            body.pose.position += translation_a;
        }
        if let Some(body) = bodies.get_mut(&BodyHandle(key.second)) {
            body.pose.position += translation_b;
        }
    }
}

fn inverse_effective_mass(body_a: &Body, body_b: &Body, point: Vec3, axis: Vec3) -> f32 {
    let radius_a = point - body_a.center_of_mass_world();
    let radius_b = point - body_b.center_of_mass_world();
    let angular_a = body_a.inverse_inertia_world() * radius_a.cross(axis);
    let angular_b = body_b.inverse_inertia_world() * radius_b.cross(axis);
    body_a.inverse_mass
        + body_b.inverse_mass
        + axis.dot(angular_a.cross(radius_a) + angular_b.cross(radius_b))
}

fn tangent_basis(normal: Vec3) -> (Vec3, Vec3) {
    let axis = if normal.x.abs() < 0.577_35 {
        Vec3::X
    } else if normal.y.abs() < 0.577_35 {
        Vec3::Y
    } else {
        Vec3::Z
    };
    let tangent_1 = normal.cross(axis).normalize_or_zero();
    let tangent_2 = normal.cross(tangent_1).normalize_or_zero();
    (tangent_1, tangent_2)
}

fn apply_pair_impulse(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    key: PairKey,
    point: Vec3,
    impulse: Vec3,
) {
    if let Some(body) = bodies.get_mut(&BodyHandle(key.first)) {
        body.apply_impulse(point, -impulse);
    }
    if let Some(body) = bodies.get_mut(&BodyHandle(key.second)) {
        body.apply_impulse(point, impulse);
    }
}

fn contact_mut(
    pairs: &mut BTreeMap<PairKey, ActivePair>,
    key: PairKey,
    index: usize,
) -> Option<&mut ContactPoint> {
    pairs
        .get_mut(&key)?
        .manifold
        .as_mut()?
        .contacts
        .get_mut(index)
}

fn is_awake_dynamic(body: &Body) -> bool {
    body.kind == BodyKind::Dynamic && !body.sleeping
}
