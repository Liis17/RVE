use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use glam::{Mat3, Vec3};

use crate::island::Island;
use crate::material::combine_materials;
use crate::narrow::{ManifoldCache, PairKey};
use crate::world::{
    ActivePair, Body, BodyHandle, BodyKind, POSITIONAL_CORRECTION, POSITIONAL_SLOP, PairKind,
    RESTITUTION_THRESHOLD, SOLVER_ITERATIONS,
};

/// Скорость скольжения в точке контакта, выше которой упёртое в конус трение считается скольжением.
const SLIDING_SPEED: f32 = 0.05;
/// Относительный допуск, по которому касательный импульс считается упёршимся в конус трения.
const FRICTION_CLAMP_TOLERANCE: f32 = 1.0e-3;
pub(crate) const PARALLEL_MIN_CONSTRAINTS: usize = 256;
pub(crate) const MAX_SOLVER_THREADS: usize = 64;

#[derive(Clone, Copy)]
struct ContactConstraint {
    first_body: usize,
    second_body: usize,
    pair: usize,
    contact: usize,
    point: Vec3,
    normal: Vec3,
    tangent_1: Vec3,
    tangent_2: Vec3,
    friction: f32,
    restitution_bias: f32,
}

struct SolverBody {
    handle: BodyHandle,
    kind: BodyKind,
    center_of_mass: Vec3,
    inverse_mass: f32,
    inverse_inertia: Mat3,
    linear_velocity: Vec3,
    angular_velocity: Vec3,
    character_controller: bool,
}

impl SolverBody {
    fn from_body(body: &Body) -> Self {
        Self {
            handle: body.handle,
            kind: body.kind,
            center_of_mass: body.center_of_mass_world(),
            inverse_mass: body.inverse_mass,
            inverse_inertia: body.inverse_inertia_world(),
            linear_velocity: body.linear_velocity,
            angular_velocity: body.angular_velocity,
            character_controller: body.character_controller,
        }
    }

    fn velocity_at(&self, point: Vec3) -> Vec3 {
        if self.character_controller {
            return Vec3::ZERO;
        }
        self.linear_velocity + self.angular_velocity.cross(point - self.center_of_mass)
    }

    fn apply_impulse(&mut self, point: Vec3, impulse: Vec3) {
        if self.kind != BodyKind::Dynamic {
            return;
        }
        self.linear_velocity += impulse * self.inverse_mass;
        self.angular_velocity +=
            self.inverse_inertia * (point - self.center_of_mass).cross(impulse);
    }
}

struct SolverContact {
    normal_impulse: f32,
    tangent_impulse: Vec3,
    sliding: bool,
}

struct SolverPair {
    key: PairKey,
    contacts: Vec<SolverContact>,
}

struct IslandSolver {
    bodies: Vec<SolverBody>,
    body_indices: BTreeMap<BodyHandle, usize>,
    pairs: Vec<SolverPair>,
    constraints: Vec<ContactConstraint>,
}

struct IslandWork {
    order: usize,
    constraint_count: usize,
    solver: IslandSolver,
}

impl IslandSolver {
    fn build(
        island: &Island,
        bodies: &BTreeMap<BodyHandle, Body>,
        pairs: &BTreeMap<PairKey, ActivePair>,
    ) -> Self {
        let mut solver = Self {
            bodies: Vec::new(),
            body_indices: BTreeMap::new(),
            pairs: Vec::new(),
            constraints: Vec::new(),
        };

        for key in &island.pairs {
            let Some(pair) = pairs.get(key) else {
                continue;
            };
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

            let first_body = solver.body_index(body_a);
            let second_body = solver.body_index(body_b);
            let material = combine_materials(&body_a.collider.material, &body_b.collider.material);
            let (tangent_1, tangent_2) = tangent_basis(manifold.normal);
            let pair_index = solver.pairs.len();
            let contact_states = manifold
                .contacts
                .iter()
                .map(|contact| SolverContact {
                    normal_impulse: contact.normal_impulse,
                    tangent_impulse: contact.tangent_impulse,
                    sliding: contact.sliding,
                })
                .collect();
            solver.pairs.push(SolverPair {
                key: *key,
                contacts: contact_states,
            });

            for (contact_index, contact) in manifold.contacts.iter().enumerate() {
                let relative_velocity = solver.bodies[second_body].velocity_at(contact.position)
                    - solver.bodies[first_body].velocity_at(contact.position);
                let normal_speed = relative_velocity.dot(manifold.normal);
                let restitution_bias = if normal_speed < -RESTITUTION_THRESHOLD {
                    material.bounciness * normal_speed
                } else {
                    0.0
                };
                // Режим трения фиксируется на весь solve по флагу скольжения прошлого шага.
                let friction = if contact.sliding {
                    material.dynamic_friction
                } else {
                    material.static_friction
                };
                solver.constraints.push(ContactConstraint {
                    first_body,
                    second_body,
                    pair: pair_index,
                    contact: contact_index,
                    point: contact.position,
                    normal: manifold.normal,
                    tangent_1,
                    tangent_2,
                    friction,
                    restitution_bias,
                });
            }
        }
        solver
    }

    fn body_index(&mut self, body: &Body) -> usize {
        if let Some(index) = self.body_indices.get(&body.handle) {
            return *index;
        }
        let index = self.bodies.len();
        self.body_indices.insert(body.handle, index);
        self.bodies.push(SolverBody::from_body(body));
        index
    }

    fn solve(&mut self) {
        self.warm_start();
        for _ in 0..SOLVER_ITERATIONS {
            for index in 0..self.constraints.len() {
                self.solve_normal(index);
                self.solve_friction(index);
            }
        }
        for index in 0..self.constraints.len() {
            let sliding = self.slides_after_solve(index);
            let constraint = &self.constraints[index];
            self.pairs[constraint.pair].contacts[constraint.contact].sliding = sliding;
        }
    }

    fn warm_start(&mut self) {
        for index in 0..self.constraints.len() {
            let constraint = &self.constraints[index];
            let contact = &mut self.pairs[constraint.pair].contacts[constraint.contact];
            // Касательный импульс прошлого шага переносится только в пределах конуса текущего трения:
            // при смене статического трения на динамическое старый импульс не должен остаться в покое.
            contact.tangent_impulse = clamp_to_friction_cone(
                contact.tangent_impulse,
                constraint.friction * contact.normal_impulse,
            );
            let impulse = constraint.normal * contact.normal_impulse + contact.tangent_impulse;
            self.apply_pair_impulse(index, impulse);
        }
    }

    fn solve_normal(&mut self, index: usize) {
        let constraint = self.constraints[index];
        let effective_mass = inverse_effective_mass(
            &self.bodies[constraint.first_body],
            &self.bodies[constraint.second_body],
            constraint.point,
            constraint.normal,
        );
        if effective_mass <= f32::MIN_POSITIVE {
            return;
        }
        let relative_velocity = self.bodies[constraint.second_body].velocity_at(constraint.point)
            - self.bodies[constraint.first_body].velocity_at(constraint.point);
        let delta = -(relative_velocity.dot(constraint.normal) + constraint.restitution_bias)
            / effective_mass;
        let contact = &mut self.pairs[constraint.pair].contacts[constraint.contact];
        let old_impulse = contact.normal_impulse;
        contact.normal_impulse = (old_impulse + delta).max(0.0);
        let applied = constraint.normal * (contact.normal_impulse - old_impulse);
        self.apply_pair_impulse(index, applied);
    }

    fn solve_friction(&mut self, index: usize) {
        let constraint = self.constraints[index];
        if constraint.friction <= 0.0 {
            return;
        }
        for tangent in [constraint.tangent_1, constraint.tangent_2] {
            let effective_mass = inverse_effective_mass(
                &self.bodies[constraint.first_body],
                &self.bodies[constraint.second_body],
                constraint.point,
                tangent,
            );
            if effective_mass <= f32::MIN_POSITIVE {
                continue;
            }
            let relative_velocity = self.bodies[constraint.second_body]
                .velocity_at(constraint.point)
                - self.bodies[constraint.first_body].velocity_at(constraint.point);
            let delta = -relative_velocity.dot(tangent) / effective_mass;
            let contact = &mut self.pairs[constraint.pair].contacts[constraint.contact];
            let old_impulse = contact.tangent_impulse;
            let candidate = clamp_to_friction_cone(
                old_impulse + tangent * delta,
                constraint.friction * contact.normal_impulse,
            );
            contact.tangent_impulse = candidate;
            let applied = candidate - old_impulse;
            self.apply_pair_impulse(index, applied);
        }
    }

    fn slides_after_solve(&self, index: usize) -> bool {
        let constraint = &self.constraints[index];
        let relative_velocity = self.bodies[constraint.second_body].velocity_at(constraint.point)
            - self.bodies[constraint.first_body].velocity_at(constraint.point);
        let tangent_speed = (relative_velocity
            - constraint.normal * relative_velocity.dot(constraint.normal))
        .length();
        let contact = &self.pairs[constraint.pair].contacts[constraint.contact];
        let limit = constraint.friction * contact.normal_impulse;
        // Нулевое трение не удерживает точку: любое касательное движение остаётся скольжением.
        let clamped = constraint.friction <= 0.0
            || (limit > 0.0
                && contact.tangent_impulse.length() >= limit * (1.0 - FRICTION_CLAMP_TOLERANCE));
        clamped && tangent_speed > SLIDING_SPEED
    }

    fn apply_pair_impulse(&mut self, constraint_index: usize, impulse: Vec3) {
        let constraint = &self.constraints[constraint_index];
        self.bodies[constraint.first_body].apply_impulse(constraint.point, -impulse);
        self.bodies[constraint.second_body].apply_impulse(constraint.point, impulse);
    }

    fn write_back(
        self,
        bodies: &mut BTreeMap<BodyHandle, Body>,
        pairs: &mut BTreeMap<PairKey, ActivePair>,
    ) {
        for solver_body in self.bodies {
            if solver_body.kind == BodyKind::Dynamic
                && let Some(body) = bodies.get_mut(&solver_body.handle)
            {
                body.linear_velocity = solver_body.linear_velocity;
                body.angular_velocity = solver_body.angular_velocity;
            }
        }
        for solver_pair in self.pairs {
            let Some(manifold) = pairs
                .get_mut(&solver_pair.key)
                .and_then(|pair| pair.manifold.as_mut())
            else {
                continue;
            };
            for (contact, state) in manifold.contacts.iter_mut().zip(solver_pair.contacts) {
                contact.normal_impulse = state.normal_impulse;
                contact.tangent_impulse = state.tangent_impulse;
                contact.sliding = state.sliding;
            }
        }
    }
}

pub(crate) fn solve(
    bodies: &mut BTreeMap<BodyHandle, Body>,
    pairs: &mut BTreeMap<PairKey, ActivePair>,
    manifolds: &mut ManifoldCache,
    islands: &[Island],
    threads: NonZeroUsize,
    parallel_min_constraints: usize,
) -> bool {
    let mut work = Vec::new();
    let mut total_constraints = 0;
    for (order, island) in islands.iter().enumerate() {
        if !island.has_awake_body(|handle| bodies.get(&handle).is_some_and(is_awake_dynamic)) {
            continue;
        }
        let solver = IslandSolver::build(island, bodies, pairs);
        let constraint_count = solver.constraints.len();
        if constraint_count == 0 {
            continue;
        }
        total_constraints += constraint_count;
        work.push(IslandWork {
            order,
            constraint_count,
            solver,
        });
    }

    let use_parallel =
        threads.get() > 1 && work.len() >= 2 && total_constraints >= parallel_min_constraints;
    if use_parallel {
        let worker_count = threads.get().min(work.len()).min(MAX_SOLVER_THREADS);
        let mut sorted_indices: Vec<_> = (0..work.len()).collect();
        sorted_indices.sort_unstable_by(|first, second| {
            work[*second]
                .constraint_count
                .cmp(&work[*first].constraint_count)
                .then(work[*first].order.cmp(&work[*second].order))
        });
        let mut assignments: Vec<Vec<IslandWork>> = (0..worker_count).map(|_| Vec::new()).collect();
        let mut loads = vec![0; worker_count];
        let mut remaining: Vec<_> = work.into_iter().map(Some).collect();
        for index in sorted_indices {
            let worker = (0..worker_count)
                .min_by_key(|worker| (loads[*worker], *worker))
                .expect("at least two workers are assigned");
            let task = remaining[index]
                .take()
                .expect("each island is assigned once");
            loads[worker] += task.constraint_count;
            assignments[worker].push(task);
        }

        let mut assignments = assignments.into_iter();
        let current_worker = assignments.next().expect("a current worker is assigned");
        let (mut current_result, worker_results) = std::thread::scope(|scope| {
            let handles: Vec<_> = assignments
                .map(|work| scope.spawn(move || solve_worker(work)))
                .collect();
            let current_result = solve_worker(current_worker);
            let worker_results = handles
                .into_iter()
                .map(|handle| handle.join().expect("island solver worker panicked"))
                .collect::<Vec<_>>();
            (current_result, worker_results)
        });
        current_result.extend(worker_results.into_iter().flatten());
        current_result.sort_unstable_by_key(|work| work.order);
        for work in current_result {
            work.solver.write_back(bodies, pairs);
        }
    } else {
        for work in &mut work {
            work.solver.solve();
        }
        for work in work {
            work.solver.write_back(bodies, pairs);
        }
    }
    for (key, pair) in pairs.iter() {
        if pair.kind == PairKind::Collision
            && let Some(manifold) = &pair.manifold
        {
            manifolds.store_impulses(*key, manifold);
        }
    }
    use_parallel
}

fn solve_worker(mut work: Vec<IslandWork>) -> Vec<IslandWork> {
    for task in &mut work {
        task.solver.solve();
    }
    work
}

/// Ограничивает касательный импульс конусом трения радиуса `maximum` (`μ·λn`).
fn clamp_to_friction_cone(impulse: Vec3, maximum: f32) -> Vec3 {
    let length = impulse.length();
    if maximum.is_finite() && length > maximum && length > f32::MIN_POSITIVE {
        impulse * (maximum / length)
    } else {
        impulse
    }
}

/// Смещает позы на проникновение сверх `POSITIONAL_SLOP`; скорости не меняет.
pub(crate) fn correct_positions(
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
        if let Some(body) = bodies.get_mut(&BodyHandle(key.first))
            && translation_a != Vec3::ZERO
        {
            body.pose.position += translation_a;
            body.mark_pose_changed();
        }
        if let Some(body) = bodies.get_mut(&BodyHandle(key.second))
            && translation_b != Vec3::ZERO
        {
            body.pose.position += translation_b;
            body.mark_pose_changed();
        }
    }
}

fn inverse_effective_mass(
    body_a: &SolverBody,
    body_b: &SolverBody,
    point: Vec3,
    axis: Vec3,
) -> f32 {
    let radius_a = point - body_a.center_of_mass;
    let radius_b = point - body_b.center_of_mass;
    let angular_a = body_a.inverse_inertia * radius_a.cross(axis);
    let angular_b = body_b.inverse_inertia * radius_b.cross(axis);
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

fn is_awake_dynamic(body: &Body) -> bool {
    body.kind == BodyKind::Dynamic && !body.sleeping
}
