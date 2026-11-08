use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;

use glam::{Mat3, Quat, Vec3};

use crate::broad::sweep_and_prune_pairable;
use crate::narrow::{ManifoldCache, PairKey, collide};
use crate::query::{QueryCollider, QueryColliderDesc, QueryError, QuerySnapshot};
use crate::{Aabb, BroadPhaseError, ContactManifold, GeometryError, NarrowphaseError, Pose, Shape};

pub const SOLVER_ITERATIONS: usize = 10;
pub const SLEEP_LINEAR_SPEED: f32 = 0.05;
pub const SLEEP_ANGULAR_SPEED: f32 = 0.05;
pub const SLEEP_DELAY_SECONDS: f32 = 0.5;
pub(crate) const POSITIONAL_SLOP: f32 = 0.005;
pub(crate) const POSITIONAL_CORRECTION: f32 = 0.2;
pub(crate) const RESTITUTION_THRESHOLD: f32 = 1.0;
const WAKE_SPEED_SQUARED: f32 = 1.0e-6;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BodyHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyKind {
    Static,
    Kinematic,
    Dynamic,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColliderDesc {
    pub shape: Shape,
    pub offset: Vec3,
    pub trigger: bool,
    pub friction: f32,
    pub restitution: f32,
    pub enabled: bool,
}

impl ColliderDesc {
    pub fn new(shape: Shape) -> Self {
        Self {
            shape,
            offset: Vec3::ZERO,
            trigger: false,
            friction: 0.5,
            restitution: 0.0,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BodyDesc {
    pub kind: BodyKind,
    pub pose: Pose,
    pub collider: ColliderDesc,
    /// Character kinematics are collision obstacles but contribute zero solver velocity.
    pub character_controller: bool,
    pub mass: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub gravity_scale: f32,
    pub lock_rotation: bool,
}

/// Изменение набора тел, которое сервер применяет до команд шага.
#[derive(Clone, Debug, PartialEq)]
pub enum BodyChange {
    Insert { handle: BodyHandle, desc: BodyDesc },
    Remove { handle: BodyHandle },
}

impl BodyDesc {
    pub fn new(kind: BodyKind, pose: Pose, collider: ColliderDesc) -> Self {
        Self {
            kind,
            pose,
            collider,
            character_controller: false,
            mass: 1.0,
            linear_damping: 0.0,
            angular_damping: 0.05,
            gravity_scale: 1.0,
            lock_rotation: false,
        }
    }

    pub fn dynamic(shape: Shape, pose: Pose, mass: f32) -> Self {
        let mut desc = Self::new(BodyKind::Dynamic, pose, ColliderDesc::new(shape));
        desc.mass = mass;
        desc
    }

    pub fn static_collider(shape: Shape, pose: Pose) -> Self {
        Self::new(BodyKind::Static, pose, ColliderDesc::new(shape))
    }

    /// Проверяет, что тело можно вставить. Клиент вызывает проверку до отправки `Insert`,
    /// поэтому сервер получает только тела, которые способен создать.
    pub fn validate(&self) -> Result<(), PhysicsWorldError> {
        self.collider.shape.local_aabb()?;
        if !self.collider.offset.is_finite()
            || !self.mass.is_finite()
            || !self.linear_damping.is_finite()
            || !self.angular_damping.is_finite()
            || !self.gravity_scale.is_finite()
            || self.linear_damping < 0.0
            || self.angular_damping < 0.0
            || (self.kind == BodyKind::Dynamic && self.mass <= 0.0)
            || (self.kind == BodyKind::Dynamic && matches!(self.collider.shape, Shape::TriMesh(_)))
            || (self.character_controller && self.kind != BodyKind::Kinematic)
        {
            return Err(PhysicsWorldError::InvalidBody);
        }
        if !self.collider.friction.is_finite()
            || self.collider.friction < 0.0
            || !self.collider.restitution.is_finite()
            || !(0.0..=1.0).contains(&self.collider.restitution)
        {
            return Err(PhysicsWorldError::InvalidMaterial);
        }
        Pose::new(self.pose.position, self.pose.rotation)?;
        self.mass_terms()?;
        Ok(())
    }

    /// Обратная масса, центр масс и обратный тензор инерции в локальных координатах.
    fn mass_terms(&self) -> Result<(f32, Vec3, Mat3), PhysicsWorldError> {
        if self.kind != BodyKind::Dynamic {
            return Ok((0.0, Vec3::ZERO, Mat3::ZERO));
        }
        let properties = self.collider.shape.mass_properties(self.mass)?;
        let center = self.collider.offset + properties.center_of_mass;
        let inverse_inertia = if self.lock_rotation {
            Mat3::ZERO
        } else {
            properties
                .inertia
                .try_inverse()
                .ok_or(PhysicsWorldError::InvalidBody)?
        };
        Ok((1.0 / self.mass, center, inverse_inertia))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PhysicsCommand {
    SetVelocity {
        body: BodyHandle,
        velocity: Vec3,
    },
    AddImpulse {
        body: BodyHandle,
        impulse: Vec3,
        point: Option<Vec3>,
    },
    AddForce {
        body: BodyHandle,
        force: Vec3,
        point: Option<Vec3>,
    },
    Teleport {
        body: BodyHandle,
        pose: Pose,
    },
    SetColliderEnabled {
        body: BodyHandle,
        enabled: bool,
    },
    KinematicTarget {
        body: BodyHandle,
        pose: Pose,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum PhysicsEvent {
    CollisionEnter {
        first: BodyHandle,
        second: BodyHandle,
        point: Vec3,
        normal: Vec3,
    },
    CollisionExit {
        first: BodyHandle,
        second: BodyHandle,
    },
    TriggerEnter {
        first: BodyHandle,
        second: BodyHandle,
    },
    TriggerExit {
        first: BodyHandle,
        second: BodyHandle,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum PhysicsWorldError {
    Geometry(GeometryError),
    BroadPhase(BroadPhaseError),
    Narrowphase(NarrowphaseError),
    NonFiniteGravity,
    InvalidBody,
    InvalidMaterial,
    InvalidTimeStep,
    InvalidStepConfig,
    UnknownBody(BodyHandle),
    DuplicateBody(BodyHandle),
    WrongBodyKind(BodyHandle),
}

impl fmt::Display for PhysicsWorldError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::BroadPhase(error) => error.fmt(formatter),
            Self::Narrowphase(error) => error.fmt(formatter),
            Self::NonFiniteGravity => formatter.write_str("physics gravity must be finite"),
            Self::InvalidBody => formatter.write_str("physics body description is invalid"),
            Self::InvalidMaterial => formatter.write_str("collider material is invalid"),
            Self::InvalidTimeStep => {
                formatter.write_str("physics step dt must be finite and nonnegative")
            }
            Self::InvalidStepConfig => {
                formatter.write_str("fixed_hz or max_substeps is out of range")
            }
            Self::UnknownBody(handle) => write!(formatter, "unknown physics body {}", handle.0),
            Self::DuplicateBody(handle) => {
                write!(formatter, "physics body {} already exists", handle.0)
            }
            Self::WrongBodyKind(handle) => {
                write!(formatter, "command is not valid for body {}", handle.0)
            }
        }
    }
}

impl std::error::Error for PhysicsWorldError {}

impl From<GeometryError> for PhysicsWorldError {
    fn from(value: GeometryError) -> Self {
        Self::Geometry(value)
    }
}

impl From<BroadPhaseError> for PhysicsWorldError {
    fn from(value: BroadPhaseError) -> Self {
        Self::BroadPhase(value)
    }
}

impl From<NarrowphaseError> for PhysicsWorldError {
    fn from(value: NarrowphaseError) -> Self {
        Self::Narrowphase(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PairKind {
    Collision,
    Trigger,
}

#[derive(Clone, Debug)]
pub(crate) struct ActivePair {
    pub(crate) kind: PairKind,
    pub(crate) manifold: Option<ContactManifold>,
}

pub(crate) struct Body {
    pub(crate) handle: BodyHandle,
    pub(crate) kind: BodyKind,
    pub(crate) pose: Pose,
    pub(crate) collider: ColliderDesc,
    pub(crate) inverse_mass: f32,
    pub(crate) center_of_mass_local: Vec3,
    pub(crate) inverse_inertia_local: Mat3,
    pub(crate) linear_velocity: Vec3,
    pub(crate) angular_velocity: Vec3,
    pub(crate) character_controller: bool,
    pub(crate) linear_damping: f32,
    pub(crate) angular_damping: f32,
    pub(crate) gravity_scale: f32,
    pub(crate) sleeping: bool,
    pub(crate) sleep_time: f32,
    pub(crate) kinematic_target: Option<Pose>,
    /// Кэш AABB коллайдера; сбрасывается вместе с изменением позы.
    aabb: Option<Aabb>,
    /// Поза менялась с последнего отчёта `StepResult.poses`.
    pose_reported: bool,
    /// Поза или коллайдер менялись с последнего `refresh_pairs`.
    pending_refresh: bool,
}

impl Body {
    pub(crate) fn mark_pose_changed(&mut self) {
        self.aabb = None;
        self.pose_reported = true;
        self.pending_refresh = true;
    }

    /// Тело участвует в broadphase как активное. Спящие, статические и неподвижные kinematic
    /// тела сравниваются только с активными, а их пары переносятся из прошлого шага без
    /// перерасчёта. Триггеры всегда активны; статики без триггера не активны никогда: их
    /// изменение будит спящие тела над ними (см. `wake_sleepers_under_changed_statics`).
    fn is_pairable(&self) -> bool {
        if self.collider.trigger {
            return true;
        }
        match self.kind {
            BodyKind::Dynamic => self.pending_refresh || !self.sleeping,
            BodyKind::Kinematic => {
                self.pending_refresh || self.kinematic_target.is_some() || is_moving_kinematic(self)
            }
            BodyKind::Static => false,
        }
    }

    pub(crate) fn center_of_mass_world(&self) -> Vec3 {
        self.pose.position + self.pose.rotation * self.center_of_mass_local
    }

    pub(crate) fn collider_pose(&self) -> Pose {
        Pose {
            position: self.pose.position + self.pose.rotation * self.collider.offset,
            rotation: self.pose.rotation,
        }
    }

    pub(crate) fn inverse_inertia_world(&self) -> Mat3 {
        if self.kind != BodyKind::Dynamic {
            return Mat3::ZERO;
        }
        let rotation = Mat3::from_quat(self.pose.rotation);
        rotation * self.inverse_inertia_local * rotation.transpose()
    }

    pub(crate) fn velocity_at(&self, point: Vec3) -> Vec3 {
        if self.character_controller {
            return Vec3::ZERO;
        }
        self.linear_velocity
            + self
                .angular_velocity
                .cross(point - self.center_of_mass_world())
    }

    pub(crate) fn apply_impulse(&mut self, point: Vec3, impulse: Vec3) {
        if self.kind != BodyKind::Dynamic {
            return;
        }
        self.linear_velocity += impulse * self.inverse_mass;
        self.angular_velocity +=
            self.inverse_inertia_world() * (point - self.center_of_mass_world()).cross(impulse);
    }

    pub(crate) fn wake(&mut self) {
        if self.kind == BodyKind::Dynamic {
            self.sleeping = false;
            self.sleep_time = 0.0;
        }
    }

    fn collider_aabb(&self) -> Result<Aabb, GeometryError> {
        self.collider.shape.aabb(self.collider_pose())
    }

    fn integrate_velocity(&mut self, gravity: Vec3, dt: f32) {
        if self.kind != BodyKind::Dynamic || self.sleeping {
            return;
        }
        self.linear_velocity += gravity * (self.gravity_scale * dt);
        self.linear_velocity *= (-self.linear_damping * dt).exp();
        self.angular_velocity *= (-self.angular_damping * dt).exp();
    }

    /// Двигает позу по скорости, уже решённой солвером; вызывается после контактов.
    fn integrate_position(&mut self, dt: f32) {
        if self.kind != BodyKind::Dynamic || self.sleeping {
            return;
        }
        let center = self.center_of_mass_world() + self.linear_velocity * dt;
        let angular_speed = self.angular_velocity.length();
        let rotation = if angular_speed > f32::MIN_POSITIVE {
            (Quat::from_axis_angle(self.angular_velocity / angular_speed, angular_speed * dt)
                * self.pose.rotation)
                .normalize()
        } else {
            self.pose.rotation
        };
        self.pose = Pose {
            position: center - rotation * self.center_of_mass_local,
            rotation,
        };
        self.mark_pose_changed();
    }

    fn advance_kinematic(&mut self, dt: f32, remaining_substeps: usize) {
        if self.kind != BodyKind::Kinematic {
            return;
        }
        let Some(target) = self.kinematic_target else {
            self.linear_velocity = Vec3::ZERO;
            self.angular_velocity = Vec3::ZERO;
            return;
        };

        let old_pose = self.pose;
        let fraction = 1.0 / remaining_substeps as f32;
        let position = if remaining_substeps == 1 {
            target.position
        } else {
            old_pose.position.lerp(target.position, fraction)
        };
        let rotation = if remaining_substeps == 1 {
            target.rotation
        } else {
            old_pose
                .rotation
                .slerp(target.rotation, fraction)
                .normalize()
        };
        self.pose = Pose { position, rotation };
        self.mark_pose_changed();
        self.linear_velocity = (self.center_of_mass_world()
            - (old_pose.position + old_pose.rotation * self.center_of_mass_local))
            / dt;
        self.angular_velocity = angular_velocity_between(old_pose.rotation, rotation, dt);
        if remaining_substeps == 1 {
            self.kinematic_target = None;
        }
    }
}

fn angular_velocity_between(from: Quat, to: Quat, dt: f32) -> Vec3 {
    let mut delta = (to * from.inverse()).normalize();
    if delta.w < 0.0 {
        delta = -delta;
    }
    let half_angle = delta.w.clamp(-1.0, 1.0).acos();
    let sine = half_angle.sin();
    if sine.abs() <= 1.0e-6 {
        Vec3::ZERO
    } else {
        Vec3::new(delta.x, delta.y, delta.z) * (2.0 * half_angle / (sine * dt))
    }
}

pub struct PhysicsWorld {
    gravity: Vec3,
    bodies: BTreeMap<BodyHandle, Body>,
    active_pairs: BTreeMap<PairKey, ActivePair>,
    manifolds: ManifoldCache,
    next_body_id: u64,
    skipped_pairs: usize,
}

impl Default for PhysicsWorld {
    fn default() -> Self {
        Self::new(Vec3::new(0.0, -9.81, 0.0)).expect("default gravity is valid")
    }
}

impl PhysicsWorld {
    pub fn new(gravity: Vec3) -> Result<Self, PhysicsWorldError> {
        if !gravity.is_finite() {
            return Err(PhysicsWorldError::NonFiniteGravity);
        }
        Ok(Self {
            gravity,
            bodies: BTreeMap::new(),
            active_pairs: BTreeMap::new(),
            manifolds: ManifoldCache::default(),
            next_body_id: 1,
            skipped_pairs: 0,
        })
    }

    pub fn insert_body(&mut self, desc: BodyDesc) -> Result<BodyHandle, PhysicsWorldError> {
        self.insert_body_with_handle(BodyHandle(self.next_body_id), desc)
    }

    /// Вставляет тело с заданным хендлом. Занятый хендл отказывает. После успеха `next_body_id`
    /// становится больше хендла, поэтому `insert_body` не выдаст его повторно.
    pub fn insert_body_with_handle(
        &mut self,
        handle: BodyHandle,
        desc: BodyDesc,
    ) -> Result<BodyHandle, PhysicsWorldError> {
        if self.bodies.contains_key(&handle) {
            return Err(PhysicsWorldError::DuplicateBody(handle));
        }
        desc.validate()?;
        let pose = Pose::new(desc.pose.position, desc.pose.rotation)?;
        let (inverse_mass, center_of_mass_local, inverse_inertia_local) = desc.mass_terms()?;
        self.bodies.insert(
            handle,
            Body {
                handle,
                kind: desc.kind,
                pose,
                collider: desc.collider,
                character_controller: desc.character_controller,
                inverse_mass,
                center_of_mass_local,
                inverse_inertia_local,
                linear_velocity: Vec3::ZERO,
                angular_velocity: Vec3::ZERO,
                linear_damping: desc.linear_damping,
                angular_damping: desc.angular_damping,
                gravity_scale: desc.gravity_scale,
                sleeping: false,
                sleep_time: 0.0,
                kinematic_target: None,
                aabb: None,
                pose_reported: true,
                pending_refresh: true,
            },
        );
        self.next_body_id = self.next_body_id.max(handle.0.saturating_add(1));
        Ok(handle)
    }

    pub fn remove_body(
        &mut self,
        handle: BodyHandle,
    ) -> Result<Vec<PhysicsEvent>, PhysicsWorldError> {
        if self.bodies.remove(&handle).is_none() {
            return Err(PhysicsWorldError::UnknownBody(handle));
        }
        let mut events = Vec::new();
        let removed: Vec<_> = self
            .active_pairs
            .keys()
            .filter(|pair| pair.first == handle.0 || pair.second == handle.0)
            .copied()
            .collect();
        for pair in removed {
            if let Some(active) = self.active_pairs.remove(&pair) {
                events.push(exit_event(pair, active.kind));
            }
            self.manifolds.remove(pair);
            let other = BodyHandle(if pair.first == handle.0 {
                pair.second
            } else {
                pair.first
            });
            self.wake_body(other);
        }
        Ok(events)
    }

    pub fn body_pose(&self, handle: BodyHandle) -> Option<Pose> {
        self.bodies.get(&handle).map(|body| body.pose)
    }

    /// Captures collider geometry and body metadata in an immutable query snapshot.
    pub fn query_snapshot(&self) -> Result<Arc<QuerySnapshot>, QueryError> {
        let colliders = self
            .bodies
            .values()
            .map(|body| {
                QueryCollider::new(QueryColliderDesc {
                    body: body.handle,
                    shape: body.collider.shape.clone(),
                    pose: body.collider_pose(),
                    enabled: body.collider.enabled,
                    trigger: body.collider.trigger,
                    body_kind: body.kind,
                    body_velocity: body.linear_velocity,
                    mass: if body.inverse_mass > 0.0 {
                        body.inverse_mass.recip()
                    } else {
                        0.0
                    },
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Arc::new(QuerySnapshot::from_colliders(colliders)?))
    }

    pub fn body_velocity(&self, handle: BodyHandle) -> Option<Vec3> {
        self.bodies.get(&handle).map(|body| body.linear_velocity)
    }

    pub fn body_angular_velocity(&self, handle: BodyHandle) -> Option<Vec3> {
        self.bodies.get(&handle).map(|body| body.angular_velocity)
    }

    pub fn body_is_sleeping(&self, handle: BodyHandle) -> Option<bool> {
        self.bodies.get(&handle).map(|body| body.sleeping)
    }

    pub fn active_pair_count(&self) -> usize {
        self.active_pairs.len()
    }

    pub fn step_fixed(&mut self, dt: f32) -> Result<Vec<PhysicsEvent>, PhysicsWorldError> {
        validate_positive_dt(dt)?;
        self.step_substep(dt, 1)
    }

    fn step_substep(
        &mut self,
        dt: f32,
        remaining_substeps: usize,
    ) -> Result<Vec<PhysicsEvent>, PhysicsWorldError> {
        // Semi-implicit порядок: скорости, контакты и импульсы, затем позы, затем позиционная
        // коррекция. Покоящееся тело не проседает на g·dt² до решения контактов.
        for body in self.bodies.values_mut() {
            body.integrate_velocity(self.gravity, dt);
            body.advance_kinematic(dt, remaining_substeps);
        }
        let events = self.refresh_pairs()?;
        crate::solver::solve(
            &mut self.bodies,
            &mut self.active_pairs,
            &mut self.manifolds,
        );
        for body in self.bodies.values_mut() {
            body.integrate_position(dt);
        }
        crate::solver::correct_positions(&mut self.bodies, &self.active_pairs);
        self.update_sleep(dt);
        Ok(events)
    }

    /// Статик, который появился или сдвинулся, может оказаться под спящими телами. Broadphase
    /// спящие тела с неактивными не сравнивает, поэтому будим такие тела по AABB.
    fn wake_sleepers_under_changed_statics(&mut self) -> Result<(), PhysicsWorldError> {
        let mut changed = Vec::new();
        for body in self.bodies.values() {
            if body.kind == BodyKind::Static
                && !body.collider.trigger
                && body.collider.enabled
                && body.pending_refresh
            {
                changed.push(body.collider_aabb()?);
            }
        }
        if changed.is_empty() {
            return Ok(());
        }
        let mut woken = Vec::new();
        for body in self.bodies.values() {
            if body.kind == BodyKind::Dynamic && body.sleeping && body.collider.enabled {
                let aabb = body.collider_aabb()?;
                if changed.iter().any(|static_aabb| static_aabb.overlaps(aabb)) {
                    woken.push(body.handle);
                }
            }
        }
        for handle in woken {
            self.wake_body(handle);
        }
        Ok(())
    }

    fn refresh_pairs(&mut self) -> Result<Vec<PhysicsEvent>, PhysicsWorldError> {
        self.wake_sleepers_under_changed_statics()?;
        let mut proxies = Vec::new();
        let mut bounds = Vec::new();
        let mut paired = Vec::new();
        let mut frozen = HashSet::new();
        for body in self.bodies.values_mut() {
            if !body.collider.enabled {
                continue;
            }
            let aabb = match body.aabb {
                Some(aabb) => aabb,
                None => {
                    let aabb = body.collider_aabb()?;
                    body.aabb = Some(aabb);
                    aabb
                }
            };
            let is_paired = body.is_pairable();
            if !is_paired {
                frozen.insert(body.handle);
            }
            paired.push(is_paired);
            bounds.push(aabb);
            proxies.push((
                body.handle,
                body.collider.clone(),
                body.kind,
                body.collider_pose(),
            ));
        }
        for body in self.bodies.values_mut() {
            body.pending_refresh = false;
        }
        // Неактивные тела не двигались и не менялись, поэтому их пары переносятся как есть.
        // Пары с активным телом пересчитываются broadphase и narrowphase.
        let candidates = sweep_and_prune_pairable(&bounds, |index| paired[index])?;
        let mut current = BTreeMap::new();
        for (key, pair) in &self.active_pairs {
            if frozen.contains(&BodyHandle(key.first)) && frozen.contains(&BodyHandle(key.second)) {
                current.insert(*key, pair.clone());
            }
        }
        for candidate in candidates {
            let (handle_a, collider_a, kind_a, pose_a) = &proxies[candidate.first];
            let (handle_b, collider_b, kind_b, pose_b) = &proxies[candidate.second];
            if collider_a.trigger && collider_b.trigger {
                continue;
            }
            let pair_kind = if collider_a.trigger || collider_b.trigger {
                PairKind::Trigger
            } else {
                if *kind_a != BodyKind::Dynamic && *kind_b != BodyKind::Dynamic {
                    continue;
                }
                PairKind::Collision
            };
            let key = PairKey::new(handle_a.0, handle_b.0);
            let outcome = collide(&collider_a.shape, *pose_a, &collider_b.shape, *pose_b);
            let mut manifold = match outcome {
                Ok(Some(manifold)) => manifold,
                Ok(None) => continue,
                Err(error) if error.is_numeric_limit() => {
                    // Пара не решается на этом шаге; существование пары сохраняется как было.
                    self.skipped_pairs += 1;
                    if let Some(previous) = self.active_pairs.get(&key) {
                        current.insert(key, previous.clone());
                    }
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if pair_kind == PairKind::Collision {
                self.wake_contact_pair(*handle_a, *handle_b);
                manifold = self.manifolds.update(key, *pose_a, *pose_b, manifold)?;
                current.insert(
                    key,
                    ActivePair {
                        kind: pair_kind,
                        manifold: Some(manifold),
                    },
                );
            } else {
                current.insert(
                    key,
                    ActivePair {
                        kind: pair_kind,
                        manifold: None,
                    },
                );
            }
        }
        let events = pair_events(&self.active_pairs, &current);
        self.active_pairs = current;
        let contact_keys: HashSet<_> = self
            .active_pairs
            .iter()
            .filter_map(|(key, pair)| (pair.kind == PairKind::Collision).then_some(*key))
            .collect();
        self.manifolds.retain(&contact_keys);
        Ok(events)
    }

    fn wake_contact_pair(&mut self, first: BodyHandle, second: BodyHandle) {
        let moving_kinematic_a = self.bodies.get(&first).is_some_and(is_moving_kinematic);
        let moving_kinematic_b = self.bodies.get(&second).is_some_and(is_moving_kinematic);
        let dynamic_awake_a = self
            .bodies
            .get(&first)
            .is_some_and(|body| body.kind == BodyKind::Dynamic && !body.sleeping);
        let dynamic_awake_b = self
            .bodies
            .get(&second)
            .is_some_and(|body| body.kind == BodyKind::Dynamic && !body.sleeping);
        let dynamic_sleeping_a = self
            .bodies
            .get(&first)
            .is_some_and(|body| body.kind == BodyKind::Dynamic && body.sleeping);
        let dynamic_sleeping_b = self
            .bodies
            .get(&second)
            .is_some_and(|body| body.kind == BodyKind::Dynamic && body.sleeping);
        if dynamic_sleeping_b && (moving_kinematic_a || dynamic_awake_a) {
            self.wake_body(second);
        }
        if dynamic_sleeping_a && (moving_kinematic_b || dynamic_awake_b) {
            self.wake_body(first);
        }
    }

    /// Соседство динамических тел по активным контактным парам; основа островов сна.
    fn dynamic_contact_graph(&self) -> BTreeMap<BodyHandle, Vec<BodyHandle>> {
        let is_dynamic = |handle: BodyHandle| {
            self.bodies
                .get(&handle)
                .is_some_and(|body| body.kind == BodyKind::Dynamic)
        };
        let mut graph: BTreeMap<BodyHandle, Vec<BodyHandle>> = BTreeMap::new();
        for (key, pair) in &self.active_pairs {
            let first = BodyHandle(key.first);
            let second = BodyHandle(key.second);
            if pair.kind == PairKind::Collision && is_dynamic(first) && is_dynamic(second) {
                graph.entry(first).or_default().push(second);
                graph.entry(second).or_default().push(first);
            }
        }
        graph
    }

    /// Будит тело вместе с его островом: спящие динамические тела, связанные контактами, тоже
    /// поднимаются в этом же вызове.
    fn wake_body(&mut self, handle: BodyHandle) {
        let graph = self.dynamic_contact_graph();
        let mut pending = vec![handle];
        let mut visited = HashSet::new();
        while let Some(current) = pending.pop() {
            if !visited.insert(current) {
                continue;
            }
            let Some(body) = self.bodies.get_mut(&current) else {
                continue;
            };
            if body.kind != BodyKind::Dynamic {
                continue;
            }
            body.wake();
            if let Some(neighbors) = graph.get(&current) {
                pending.extend(neighbors.iter().copied());
            }
        }
    }

    fn wake_neighbors(&mut self, handle: BodyHandle) {
        let neighbors: Vec<_> = self
            .active_pairs
            .keys()
            .filter_map(|pair| {
                if pair.first == handle.0 {
                    Some(BodyHandle(pair.second))
                } else if pair.second == handle.0 {
                    Some(BodyHandle(pair.first))
                } else {
                    None
                }
            })
            .collect();
        for neighbor in neighbors {
            self.wake_body(neighbor);
        }
    }

    fn apply_command(
        &mut self,
        command: PhysicsCommand,
        step_dt: f32,
    ) -> Result<(), PhysicsWorldError> {
        let handle = command.body();
        let Some(body) = self.bodies.get(&handle) else {
            return Err(PhysicsWorldError::UnknownBody(handle));
        };
        match command {
            PhysicsCommand::SetVelocity { velocity, .. } => {
                if body.kind != BodyKind::Dynamic || !velocity.is_finite() {
                    return Err(if body.kind != BodyKind::Dynamic {
                        PhysicsWorldError::WrongBodyKind(handle)
                    } else {
                        PhysicsWorldError::InvalidBody
                    });
                }
                self.wake_body(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.linear_velocity = velocity;
            }
            PhysicsCommand::AddImpulse { impulse, point, .. } => {
                if body.kind != BodyKind::Dynamic {
                    return Err(PhysicsWorldError::WrongBodyKind(handle));
                }
                if !impulse.is_finite() || point.is_some_and(|point| !point.is_finite()) {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                let point = point.unwrap_or_else(|| body.center_of_mass_world());
                self.wake_body(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.apply_impulse(point, impulse);
            }
            PhysicsCommand::AddForce { force, point, .. } => {
                if body.kind != BodyKind::Dynamic {
                    return Err(PhysicsWorldError::WrongBodyKind(handle));
                }
                if !force.is_finite() || point.is_some_and(|point| !point.is_finite()) {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                let point = point.unwrap_or_else(|| body.center_of_mass_world());
                let impulse = force * step_dt;
                if !impulse.is_finite() {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                self.wake_body(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.apply_impulse(point, impulse);
            }
            PhysicsCommand::Teleport { pose, .. } => {
                let pose = Pose::new(pose.position, pose.rotation)?;
                self.wake_neighbors(handle);
                self.wake_body(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.pose = pose;
                body.mark_pose_changed();
                body.kinematic_target = None;
                if body.kind == BodyKind::Kinematic {
                    body.linear_velocity = Vec3::ZERO;
                    body.angular_velocity = Vec3::ZERO;
                }
            }
            PhysicsCommand::SetColliderEnabled { enabled, .. } => {
                if body.collider.enabled != enabled {
                    self.wake_neighbors(handle);
                    self.wake_body(handle);
                    let body = self.bodies.get_mut(&handle).expect("body was checked");
                    body.collider.enabled = enabled;
                    body.pending_refresh = true;
                }
            }
            PhysicsCommand::KinematicTarget { pose, .. } => {
                if body.kind != BodyKind::Kinematic {
                    return Err(PhysicsWorldError::WrongBodyKind(handle));
                }
                let pose = Pose::new(pose.position, pose.rotation)?;
                if body.pose.position.distance_squared(pose.position) > WAKE_SPEED_SQUARED
                    || body.pose.rotation.dot(pose.rotation).abs() < 0.999_999
                {
                    self.wake_neighbors(handle);
                }
                self.bodies
                    .get_mut(&handle)
                    .expect("body was checked")
                    .kinematic_target = Some(pose);
            }
        }
        Ok(())
    }

    /// Засыпание островами: остров — компонента контактов между динамическими телами. Он засыпает
    /// целиком, когда каждое тело успокоилось и ни одно не касается движущегося kinematic.
    fn update_sleep(&mut self, dt: f32) {
        for body in self.bodies.values_mut() {
            if body.kind != BodyKind::Dynamic || body.sleeping {
                continue;
            }
            if body.linear_velocity.length_squared() <= SLEEP_LINEAR_SPEED.powi(2)
                && body.angular_velocity.length_squared() <= SLEEP_ANGULAR_SPEED.powi(2)
            {
                body.sleep_time += dt;
            } else {
                body.sleep_time = 0.0;
            }
        }

        let graph = self.dynamic_contact_graph();
        let mut held_awake = HashSet::new();
        for (key, pair) in &self.active_pairs {
            if pair.kind != PairKind::Collision {
                continue;
            }
            let first = BodyHandle(key.first);
            let second = BodyHandle(key.second);
            let is_moving =
                |handle: BodyHandle| self.bodies.get(&handle).is_some_and(is_moving_kinematic);
            if is_moving(first) {
                held_awake.insert(second);
            }
            if is_moving(second) {
                held_awake.insert(first);
            }
        }

        let starts: Vec<BodyHandle> = self
            .bodies
            .values()
            .filter(|body| body.kind == BodyKind::Dynamic && !body.sleeping)
            .map(|body| body.handle)
            .collect();
        let mut visited = HashSet::new();
        for start in starts {
            if !visited.insert(start) {
                continue;
            }
            let mut island = vec![start];
            let mut index = 0;
            while index < island.len() {
                let current = island[index];
                index += 1;
                for neighbor in graph.get(&current).into_iter().flatten() {
                    if visited.insert(*neighbor) {
                        island.push(*neighbor);
                    }
                }
            }
            let ready = island.iter().all(|handle| {
                !held_awake.contains(handle)
                    && self
                        .bodies
                        .get(handle)
                        .is_some_and(|body| body.sleep_time >= SLEEP_DELAY_SECONDS)
            });
            if ready {
                for handle in island {
                    if let Some(body) = self.bodies.get_mut(&handle) {
                        body.sleeping = true;
                        body.linear_velocity = Vec3::ZERO;
                        body.angular_velocity = Vec3::ZERO;
                    }
                }
            }
        }
    }
}

fn is_moving_kinematic(body: &Body) -> bool {
    body.kind == BodyKind::Kinematic
        && (body.linear_velocity.length_squared() > WAKE_SPEED_SQUARED
            || body.angular_velocity.length_squared() > WAKE_SPEED_SQUARED)
}

impl PhysicsCommand {
    /// Тело, к которому относится команда.
    pub fn body(&self) -> BodyHandle {
        match *self {
            Self::SetVelocity { body, .. }
            | Self::AddImpulse { body, .. }
            | Self::AddForce { body, .. }
            | Self::Teleport { body, .. }
            | Self::SetColliderEnabled { body, .. }
            | Self::KinematicTarget { body, .. } => body,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepResult {
    pub substeps: usize,
    /// Позы тел, изменившихся с прошлого отчёта: движение, телепорт, вставка, коррекция.
    /// Спящие и неподвижные тела сюда не входят; полный набор отдаёт `StepDriver::initial_result`.
    pub poses: Vec<(BodyHandle, Pose)>,
    pub events: Vec<PhysicsEvent>,
    /// Pairs whose narrowphase hit a numeric limit during this step and were kept unresolved.
    pub skipped_pairs: usize,
}

pub struct StepDriver {
    world: PhysicsWorld,
    fixed_dt: f32,
    max_substeps: usize,
    accumulator: f64,
}

impl StepDriver {
    pub fn new(
        world: PhysicsWorld,
        fixed_hz: u32,
        max_substeps: usize,
    ) -> Result<Self, PhysicsWorldError> {
        if !(1..=1000).contains(&fixed_hz) || !(1..=64).contains(&max_substeps) {
            return Err(PhysicsWorldError::InvalidStepConfig);
        }
        Ok(Self {
            world,
            fixed_dt: 1.0 / fixed_hz as f32,
            max_substeps,
            accumulator: 0.0,
        })
    }

    pub fn world(&self) -> &PhysicsWorld {
        &self.world
    }

    pub fn world_mut(&mut self) -> &mut PhysicsWorld {
        &mut self.world
    }

    pub fn fixed_dt(&self) -> f32 {
        self.fixed_dt
    }

    pub fn accumulator(&self) -> f64 {
        self.accumulator
    }

    /// Returns the unadvanced initial state without refreshing contact-pair events.
    pub fn initial_result(&self) -> StepResult {
        StepResult {
            substeps: 0,
            poses: self
                .world
                .bodies
                .values()
                .map(|body| (body.handle, body.pose))
                .collect(),
            events: Vec::new(),
            skipped_pairs: 0,
        }
    }

    /// Applies frame commands once, then runs the capped number of fixed substeps.
    pub fn advance_step(
        &mut self,
        dt: f32,
        commands: impl IntoIterator<Item = PhysicsCommand>,
    ) -> Result<StepResult, PhysicsWorldError> {
        self.advance_step_with_changes(dt, std::iter::empty(), commands)
    }

    /// Applies body changes, then frame commands, then runs the capped fixed substeps. Exit events
    /// of removed bodies are part of the returned step events. Removing an unknown body is ignored.
    pub fn advance_step_with_changes(
        &mut self,
        dt: f32,
        changes: impl IntoIterator<Item = BodyChange>,
        commands: impl IntoIterator<Item = PhysicsCommand>,
    ) -> Result<StepResult, PhysicsWorldError> {
        validate_nonnegative_dt(dt)?;
        self.world.skipped_pairs = 0;
        let mut events = Vec::new();
        for change in changes {
            match change {
                BodyChange::Insert { handle, desc } => {
                    self.world.insert_body_with_handle(handle, desc)?;
                }
                BodyChange::Remove { handle } => match self.world.remove_body(handle) {
                    Ok(exits) => events.extend(exits),
                    Err(PhysicsWorldError::UnknownBody(_)) => {}
                    Err(error) => return Err(error),
                },
            }
        }
        for command in commands {
            self.world.apply_command(command, dt)?;
        }

        events.extend(self.world.refresh_pairs()?);
        let accumulated = self.accumulator + dt as f64;
        let available = (accumulated / self.fixed_dt as f64).floor();
        let substeps = available.min(self.max_substeps as f64) as usize;
        self.accumulator = accumulated - substeps as f64 * self.fixed_dt as f64;
        if available >= self.max_substeps as f64 {
            self.accumulator = self.accumulator.min(self.fixed_dt as f64);
        }
        for index in 0..substeps {
            events.extend(self.world.step_substep(self.fixed_dt, substeps - index)?);
        }
        // Отчёт содержит только тела, чья поза изменилась с прошлого вызова.
        let mut poses = Vec::new();
        for body in self.world.bodies.values_mut() {
            if body.pose_reported {
                body.pose_reported = false;
                poses.push((body.handle, body.pose));
            }
        }
        Ok(StepResult {
            substeps,
            poses,
            events,
            skipped_pairs: self.world.skipped_pairs,
        })
    }
}

fn pair_events(
    previous: &BTreeMap<PairKey, ActivePair>,
    current: &BTreeMap<PairKey, ActivePair>,
) -> Vec<PhysicsEvent> {
    let mut events = Vec::new();
    for (key, old) in previous {
        if current.get(key).is_none_or(|new| new.kind != old.kind) {
            events.push(exit_event(*key, old.kind));
        }
    }
    for (key, new) in current {
        if previous.get(key).is_none_or(|old| old.kind != new.kind) {
            events.push(enter_event(*key, new));
        }
    }
    events
}

fn enter_event(key: PairKey, pair: &ActivePair) -> PhysicsEvent {
    let first = BodyHandle(key.first);
    let second = BodyHandle(key.second);
    match pair.kind {
        PairKind::Collision => {
            let manifold = pair
                .manifold
                .as_ref()
                .expect("collision pairs retain a contact manifold");
            let point = manifold
                .contacts
                .first()
                .map_or(Vec3::ZERO, |contact| contact.position);
            PhysicsEvent::CollisionEnter {
                first,
                second,
                point,
                normal: manifold.normal,
            }
        }
        PairKind::Trigger => PhysicsEvent::TriggerEnter { first, second },
    }
}

fn exit_event(key: PairKey, kind: PairKind) -> PhysicsEvent {
    let first = BodyHandle(key.first);
    let second = BodyHandle(key.second);
    match kind {
        PairKind::Collision => PhysicsEvent::CollisionExit { first, second },
        PairKind::Trigger => PhysicsEvent::TriggerExit { first, second },
    }
}

fn validate_nonnegative_dt(dt: f32) -> Result<(), PhysicsWorldError> {
    if dt.is_finite() && dt >= 0.0 {
        Ok(())
    } else {
        Err(PhysicsWorldError::InvalidTimeStep)
    }
}

fn validate_positive_dt(dt: f32) -> Result<(), PhysicsWorldError> {
    if dt.is_finite() && dt > 0.0 {
        Ok(())
    } else {
        Err(PhysicsWorldError::InvalidTimeStep)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BodyChange, BodyDesc, BodyHandle, BodyKind, ColliderDesc, PhysicsCommand, PhysicsEvent,
        PhysicsWorld, PhysicsWorldError, SLEEP_ANGULAR_SPEED, SLEEP_LINEAR_SPEED, StepDriver,
    };
    use crate::{Hull, Pose, Shape, TriMesh};
    use glam::{Quat, Vec3};
    use std::sync::Arc;

    fn box_shape(half: f32) -> Shape {
        Shape::cuboid(Vec3::splat(half)).unwrap()
    }

    fn floor_desc() -> BodyDesc {
        BodyDesc::static_collider(
            Shape::cuboid(Vec3::new(5.0, 0.5, 5.0)).unwrap(),
            Pose::new(Vec3::new(0.0, -0.5, 0.0), Quat::IDENTITY).unwrap(),
        )
    }

    fn run_steps(driver: &mut StepDriver, count: usize) -> Vec<PhysicsEvent> {
        let mut events = Vec::new();
        for _ in 0..count {
            events.extend(
                driver
                    .advance_step(1.0 / 60.0, [])
                    .expect("fixed step succeeds")
                    .events,
            );
        }
        events
    }

    fn count_events(events: &[PhysicsEvent], predicate: impl Fn(&PhysicsEvent) -> bool) -> usize {
        events.iter().filter(|event| predicate(event)).count()
    }

    fn floor_world() -> (PhysicsWorld, BodyHandle, BodyHandle) {
        let mut world = PhysicsWorld::default();
        let floor = world.insert_body(floor_desc()).unwrap();
        let mut ball = BodyDesc::dynamic(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(0.0, 2.0, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        );
        ball.linear_damping = 0.02;
        let ball = world.insert_body(ball).unwrap();
        (world, floor, ball)
    }

    fn angular_response(
        shape: Shape,
        pose: Pose,
        offset: Vec3,
        point: Vec3,
        impulse: Vec3,
    ) -> Vec3 {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut desc = BodyDesc::dynamic(shape, pose, 1.0);
        desc.collider.offset = offset;
        let body = world.insert_body(desc).unwrap();
        world
            .apply_command(
                PhysicsCommand::AddImpulse {
                    body,
                    impulse,
                    point: Some(point),
                },
                0.0,
            )
            .unwrap();
        world.body_angular_velocity(body).unwrap()
    }

    #[test]
    fn falling_sphere_rests_and_keeps_one_sleeping_contact_pair() {
        let (world, _, ball) = floor_world();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let events = run_steps(&mut driver, 300);
        let pose = driver.world().body_pose(ball).unwrap();
        assert!((pose.position.y - 0.5).abs() < 0.04, "{pose:?}");
        assert!(driver.world().body_is_sleeping(ball).unwrap());
        assert_eq!(driver.world().active_pair_count(), 1);
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionEnter { .. }
            )),
            1
        );
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionExit { .. }
            )),
            0
        );
    }

    #[test]
    fn three_box_stack_stays_bounded_for_five_seconds() {
        let mut world = PhysicsWorld::default();
        world.insert_body(floor_desc()).unwrap();
        let mut boxes = Vec::new();
        for index in 0..3 {
            let mut desc = BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(0.0, 0.5 + index as f32, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            );
            desc.linear_damping = 0.02;
            desc.angular_damping = 0.05;
            boxes.push(world.insert_body(desc).unwrap());
        }
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        run_steps(&mut driver, 300);
        for (index, handle) in boxes.iter().enumerate() {
            let pose = driver.world().body_pose(*handle).unwrap();
            let expected_y = 0.5 + index as f32;
            assert!(
                (pose.position.y - expected_y).abs() < 0.15,
                "box {index}: {pose:?}"
            );
            assert!(
                pose.position.x.abs() < 0.02 && pose.position.z.abs() < 0.02,
                "box {index}: {pose:?}"
            );
        }
    }

    #[test]
    fn restitution_produces_a_bounded_bounce() {
        let mut world = PhysicsWorld::new(Vec3::new(0.0, -9.81, 0.0)).unwrap();
        let mut floor = floor_desc();
        floor.collider.restitution = 0.8;
        world.insert_body(floor).unwrap();
        let mut ball = BodyDesc::dynamic(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(0.0, 1.5, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        );
        ball.collider.restitution = 0.8;
        let ball = world.insert_body(ball).unwrap();
        world
            .apply_command(
                PhysicsCommand::SetVelocity {
                    body: ball,
                    velocity: Vec3::new(0.0, -5.0, 0.0),
                },
                0.0,
            )
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let mut previous_velocity = -5.0;
        let mut bounce = None;
        for _ in 0..60 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            let velocity = driver.world().body_velocity(ball).unwrap().y;
            if previous_velocity < 0.0 && velocity > 0.0 {
                let incoming_speed = -(previous_velocity - 9.81 / 60.0);
                bounce = Some((incoming_speed, velocity));
                break;
            }
            previous_velocity = velocity;
        }
        let (incoming_speed, rebound_speed) = bounce.expect("sphere rebounds from the floor");
        let measured_restitution = rebound_speed / incoming_speed;
        assert!(
            (0.75..0.85).contains(&measured_restitution),
            "measured restitution {measured_restitution}"
        );
    }

    fn slope_velocity(friction: f32) -> Vec3 {
        let angle = 20.0_f32.to_radians();
        let normal = Vec3::new(angle.sin(), angle.cos(), 0.0);
        let rotation = Quat::from_rotation_z(-angle);
        let mut world = PhysicsWorld::default();
        let mut ramp = BodyDesc::static_collider(
            Shape::cuboid(Vec3::new(5.0, 0.2, 3.0)).unwrap(),
            Pose::IDENTITY,
        );
        ramp.pose.rotation = rotation;
        ramp.collider.friction = friction;
        world.insert_body(ramp).unwrap();
        let mut block = BodyDesc::dynamic(
            box_shape(0.4),
            Pose::new(normal * 0.6, rotation).unwrap(),
            1.0,
        );
        block.collider.friction = friction;
        block.lock_rotation = true;
        let block = world.insert_body(block).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        run_steps(&mut driver, 120);
        driver.world().body_velocity(block).unwrap()
    }

    #[test]
    fn friction_holds_a_box_on_a_slope_and_zero_friction_slides() {
        let held = slope_velocity(0.9);
        let sliding = slope_velocity(0.0);
        assert!(held.length() < 0.08, "high-friction velocity {held:?}");
        let down_slope = Vec3::new(
            20.0_f32.to_radians().cos(),
            -20.0_f32.to_radians().sin(),
            0.0,
        );
        assert!(
            sliding.dot(down_slope) > 0.4,
            "zero-friction velocity {sliding:?}"
        );
    }

    #[test]
    fn collision_enter_and_exit_are_emitted_once_across_teleport() {
        let (world, _, ball) = floor_world();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let mut events = run_steps(&mut driver, 120);
        let teleport = driver
            .advance_step(
                0.0,
                [PhysicsCommand::Teleport {
                    body: ball,
                    pose: Pose::new(Vec3::new(0.0, 5.0, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        events.extend(teleport.events);
        let next = driver.advance_step(0.0, []).unwrap();
        events.extend(next.events);
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionEnter { .. }
            )),
            1
        );
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionExit { .. }
            )),
            1
        );
    }

    #[test]
    fn trigger_overlap_emits_event_without_contact_response() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut trigger = BodyDesc::static_collider(Shape::sphere(1.0).unwrap(), Pose::IDENTITY);
        trigger.collider.trigger = true;
        let trigger = world.insert_body(trigger).unwrap();
        let ball = world
            .insert_body(BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(0.5, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let result = driver.advance_step(1.0 / 60.0, []).unwrap();
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            result.events[0],
            PhysicsEvent::TriggerEnter { .. }
        ));
        assert_eq!(driver.world().body_pose(ball).unwrap().position.x, 0.5);
        assert_eq!(driver.world().body_velocity(ball).unwrap(), Vec3::ZERO);

        let mut second_trigger =
            BodyDesc::static_collider(Shape::sphere(1.0).unwrap(), Pose::IDENTITY);
        second_trigger.collider.trigger = true;
        let second_trigger = driver.world_mut().insert_body(second_trigger).unwrap();
        let result = driver.advance_step(0.0, []).unwrap();
        assert!(!result.events.iter().any(|event| matches!(
            event,
            PhysicsEvent::TriggerEnter { first, second }
                if (*first == trigger && *second == second_trigger)
                    || (*first == second_trigger && *second == trigger)
        )));
        let exit = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetColliderEnabled {
                    body: trigger,
                    enabled: false,
                }],
            )
            .unwrap();
        assert_eq!(
            count_events(&exit.events, |event| matches!(
                event,
                PhysicsEvent::TriggerExit { .. }
            )),
            1
        );
        assert!(driver.advance_step(0.0, []).unwrap().events.is_empty());
    }

    #[test]
    fn kinematic_character_trigger_pair_emits_one_enter_and_one_exit() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut trigger = BodyDesc::static_collider(
            box_shape(0.5),
            Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        world.insert_body(trigger).unwrap();
        let mut character = BodyDesc::new(
            BodyKind::Kinematic,
            Pose::new(Vec3::new(-2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ColliderDesc::new(Shape::capsule(0.25, 0.5).unwrap()),
        );
        character.character_controller = true;
        let character = world.insert_body(character).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        let enter = driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::KinematicTarget {
                    body: character,
                    pose: Pose::IDENTITY,
                }],
            )
            .unwrap();
        assert_eq!(
            enter
                .events
                .iter()
                .filter(|event| matches!(event, PhysicsEvent::TriggerEnter { .. }))
                .count(),
            1
        );
        assert!(
            driver
                .advance_step(1.0 / 60.0, [])
                .unwrap()
                .events
                .is_empty()
        );

        let exit = driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::KinematicTarget {
                    body: character,
                    pose: Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        assert_eq!(
            exit.events
                .iter()
                .filter(|event| matches!(event, PhysicsEvent::TriggerExit { .. }))
                .count(),
            1
        );
        assert!(
            driver
                .advance_step(1.0 / 60.0, [])
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn character_kinematic_motion_has_zero_solver_velocity() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut character = BodyDesc::new(
            BodyKind::Kinematic,
            Pose::new(Vec3::new(-1.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ColliderDesc::new(Shape::capsule(0.5, 0.5).unwrap()),
        );
        character.character_controller = true;
        let character = world.insert_body(character).unwrap();
        let dynamic = world
            .insert_body(BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::IDENTITY,
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::KinematicTarget {
                    body: character,
                    pose: Pose::IDENTITY,
                }],
            )
            .unwrap();
        assert_eq!(driver.world().body_velocity(dynamic).unwrap(), Vec3::ZERO);
    }

    #[test]
    fn triggers_check_static_pairs_while_static_contact_pairs_are_filtered() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut trigger = BodyDesc::static_collider(Shape::sphere(1.0).unwrap(), Pose::IDENTITY);
        trigger.collider.trigger = true;
        world.insert_body(trigger).unwrap();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(0.8, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(1.7, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let result = driver.advance_step(0.0, []).unwrap();
        assert_eq!(result.events.len(), 1);
        assert!(matches!(
            result.events[0],
            PhysicsEvent::TriggerEnter { .. }
        ));
        assert_eq!(driver.world().active_pair_count(), 1);
    }

    #[test]
    fn sleeping_pair_survives_wake_without_duplicate_enter() {
        let (world, _, ball) = floor_world();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let mut events = run_steps(&mut driver, 300);
        assert!(driver.world().body_is_sleeping(ball).unwrap());
        assert_eq!(driver.world().active_pair_count(), 1);
        let result = driver
            .advance_step(
                0.0,
                [PhysicsCommand::AddImpulse {
                    body: ball,
                    impulse: Vec3::new(0.0, 0.02, 0.0),
                    point: None,
                }],
            )
            .unwrap();
        events.extend(result.events);
        assert!(!driver.world().body_is_sleeping(ball).unwrap());
        events.extend(driver.advance_step(1.0 / 60.0, []).unwrap().events);
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionEnter { .. }
            )),
            1
        );
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionExit { .. }
            )),
            0
        );
    }

    #[test]
    fn disabling_or_removing_support_wakes_body_and_emits_one_exit() {
        let (world, floor, ball) = floor_world();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        run_steps(&mut driver, 300);
        assert!(driver.world().body_is_sleeping(ball).unwrap());
        let disabled = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetColliderEnabled {
                    body: floor,
                    enabled: false,
                }],
            )
            .unwrap();
        assert_eq!(disabled.events.len(), 1);
        assert!(matches!(
            disabled.events[0],
            PhysicsEvent::CollisionExit { .. }
        ));
        assert!(!driver.world().body_is_sleeping(ball).unwrap());
        assert!(driver.advance_step(0.0, []).unwrap().events.is_empty());

        let (world, floor, ball) = floor_world();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        run_steps(&mut driver, 300);
        let removed = driver.world_mut().remove_body(floor).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(matches!(removed[0], PhysicsEvent::CollisionExit { .. }));
        assert!(!driver.world().body_is_sleeping(ball).unwrap());
        assert!(driver.advance_step(0.0, []).unwrap().events.is_empty());
    }

    #[test]
    fn moving_kinematic_body_wakes_a_sleeping_dynamic_body() {
        let mut world = PhysicsWorld::default();
        world.insert_body(floor_desc()).unwrap();
        let ball = world
            .insert_body(BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let kin = world
            .insert_body(BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::new(2.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                ColliderDesc::new(box_shape(0.5)),
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 10, 8).unwrap();
        run_steps(&mut driver, 300);
        assert!(driver.world().body_is_sleeping(ball).unwrap());
        driver
            .advance_step(
                0.0,
                [PhysicsCommand::KinematicTarget {
                    body: kin,
                    pose: Pose::new(Vec3::new(0.8, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        assert!(driver.world().body_is_sleeping(ball).unwrap());
        let result = driver.advance_step(0.4, []).unwrap();
        assert!(result.events.iter().any(|event| matches!(
            event,
            PhysicsEvent::CollisionEnter { first, second, .. }
                if (*first == ball && *second == kin) || (*first == kin && *second == ball)
        )));
        assert!(!driver.world().body_is_sleeping(ball).unwrap());
        assert!(driver.world().body_velocity(ball).unwrap().x < -0.1);
    }

    #[test]
    fn step_commands_apply_once_and_force_impulse_is_frame_rate_independent() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let body = world
            .insert_body(BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::IDENTITY,
                2.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 10, 2).unwrap();
        let result = driver
            .advance_step(
                0.25,
                [PhysicsCommand::AddImpulse {
                    body,
                    impulse: Vec3::new(10.0, 0.0, 0.0),
                    point: None,
                }],
            )
            .unwrap();
        assert_eq!(result.substeps, 2);
        assert!((driver.world().body_velocity(body).unwrap().x - 5.0).abs() < 1.0e-5);
        assert_eq!(driver.advance_step(0.0, []).unwrap().substeps, 0);
        assert!((driver.world().body_velocity(body).unwrap().x - 5.0).abs() < 1.0e-5);
        driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetVelocity {
                    body,
                    velocity: Vec3::new(3.0, 0.0, 0.0),
                }],
            )
            .unwrap();
        driver.advance_step(0.0, []).unwrap();
        assert!((driver.world().body_velocity(body).unwrap().x - 3.0).abs() < 1.0e-5);

        fn force_velocity(fps: u32) -> f32 {
            let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
            let body = world
                .insert_body(BodyDesc::dynamic(
                    Shape::sphere(0.5).unwrap(),
                    Pose::IDENTITY,
                    1.0,
                ))
                .unwrap();
            let mut driver = StepDriver::new(world, 60, 8).unwrap();
            for _ in 0..fps {
                driver
                    .advance_step(
                        1.0 / fps as f32,
                        [PhysicsCommand::AddForce {
                            body,
                            force: Vec3::new(10.0, 0.0, 0.0),
                            point: None,
                        }],
                    )
                    .unwrap();
            }
            driver.world().body_velocity(body).unwrap().x
        }
        let at_30 = force_velocity(30);
        let at_60 = force_velocity(60);
        assert!((at_30 - 10.0).abs() < 1.0e-3, "30fps: {at_30}");
        assert!((at_60 - 10.0).abs() < 1.0e-3, "60fps: {at_60}");
        assert!((at_30 - at_60).abs() < 1.0e-3);
    }

    #[test]
    fn zero_step_force_uses_frame_dt_and_accumulator_cap_discards_catchup() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let body = world
            .insert_body(BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::IDENTITY,
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 10, 2).unwrap();
        let zero_step = driver
            .advance_step(
                0.01,
                [PhysicsCommand::AddForce {
                    body,
                    force: Vec3::new(5.0, 0.0, 0.0),
                    point: None,
                }],
            )
            .unwrap();
        assert_eq!(zero_step.substeps, 0);
        assert!((driver.world().body_velocity(body).unwrap().x - 0.05).abs() < 1.0e-5);

        let capped = driver.advance_step(0.55, []).unwrap();
        assert_eq!(capped.substeps, 2);
        assert!(driver.accumulator() <= driver.fixed_dt() as f64);
        let carried = driver.advance_step(0.0, []).unwrap();
        assert_eq!(carried.substeps, 1);
        assert!(driver.accumulator() < 1.0e-5);
    }

    #[test]
    fn kinematic_target_survives_zero_steps_and_new_target_replaces_it() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let kin = world
            .insert_body(BodyDesc::new(
                BodyKind::Kinematic,
                Pose::IDENTITY,
                ColliderDesc::new(Shape::cuboid(Vec3::splat(0.5)).unwrap()),
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 10, 8).unwrap();
        driver
            .advance_step(
                0.0,
                [PhysicsCommand::KinematicTarget {
                    body: kin,
                    pose: Pose::new(Vec3::X, Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        assert_eq!(driver.world().body_pose(kin).unwrap().position, Vec3::ZERO);
        driver
            .advance_step(
                0.0,
                [PhysicsCommand::KinematicTarget {
                    body: kin,
                    pose: Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        let result = driver.advance_step(0.2, []).unwrap();
        assert_eq!(result.substeps, 2);
        assert!((driver.world().body_pose(kin).unwrap().position.x - 2.0).abs() < 1.0e-5);
        assert!((driver.world().body_velocity(kin).unwrap().x - 10.0).abs() < 1.0e-4);
    }

    #[test]
    fn off_center_impulse_changes_angular_velocity_and_step_validation_is_bounded() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let body = world
            .insert_body(BodyDesc::dynamic(box_shape(0.5), Pose::IDENTITY, 1.0))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        driver
            .advance_step(
                0.0,
                [PhysicsCommand::AddImpulse {
                    body,
                    impulse: Vec3::Y,
                    point: Some(Vec3::X),
                }],
            )
            .unwrap();
        assert!(driver.world().body_angular_velocity(body).unwrap().z > 0.1);
        assert!(matches!(
            driver.advance_step(f32::NAN, []),
            Err(PhysicsWorldError::InvalidTimeStep)
        ));
        assert!(matches!(
            StepDriver::new(PhysicsWorld::default(), 0, 8),
            Err(PhysicsWorldError::InvalidStepConfig)
        ));
        assert!(matches!(
            StepDriver::new(PhysicsWorld::default(), 60, 65),
            Err(PhysicsWorldError::InvalidStepConfig)
        ));
    }

    #[test]
    fn separated_rotated_cube_hulls_do_not_abort_a_world_step() {
        let points = [
            Vec3::new(-0.5, -0.5, -0.5),
            Vec3::new(0.5, -0.5, -0.5),
            Vec3::new(-0.5, 0.5, -0.5),
            Vec3::new(0.5, 0.5, -0.5),
            Vec3::new(-0.5, -0.5, 0.5),
            Vec3::new(0.5, -0.5, 0.5),
            Vec3::new(-0.5, 0.5, 0.5),
            Vec3::new(0.5, 0.5, 0.5),
        ];
        let shape = Shape::ConvexHull(Arc::new(Hull::from_points(&points).unwrap()));
        let poses = [
            Pose::new(
                Vec3::new(-0.804_056_2, -0.969_654_3, -0.700_818_3),
                Quat::from_xyzw(0.040_864_84, 0.080_302_73, 0.444_840_82, 0.891_065_8),
            )
            .unwrap(),
            Pose::new(
                Vec3::new(-0.746_299_9, -0.981_468_4, -0.648_807_1),
                Quat::from_xyzw(0.050_731_134, 0.061_450_034, 0.442_501, 0.893_220_66),
            )
            .unwrap(),
        ];
        for pose in poses {
            let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
            world
                .insert_body(BodyDesc::static_collider(shape.clone(), Pose::IDENTITY))
                .unwrap();
            world
                .insert_body(BodyDesc::dynamic(shape.clone(), pose, 1.0))
                .unwrap();
            assert!(world.step_fixed(1.0 / 60.0).is_ok(), "{pose:?}");
        }
    }

    #[test]
    fn mesh_contact_witness_drift_does_not_create_spurious_angular_response() {
        fn world_with_ball(start_x: f32) -> (PhysicsWorld, BodyHandle) {
            let triangle = Arc::new(
                TriMesh::new(
                    vec![
                        Vec3::new(-100.0, 0.0, -100.0),
                        Vec3::new(0.0, 0.0, 100.0),
                        Vec3::new(100.0, 0.0, -100.0),
                    ],
                    vec![[0, 1, 2]],
                )
                .unwrap(),
            );
            let mut world = PhysicsWorld::new(Vec3::new(0.0, -9.81, 0.0)).unwrap();
            let mut floor = BodyDesc::static_collider(Shape::TriMesh(triangle), Pose::IDENTITY);
            floor.collider.friction = 1.0;
            world.insert_body(floor).unwrap();
            let mut ball = BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(start_x, 0.49, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            );
            ball.collider.friction = 1.0;
            ball.linear_damping = 0.0;
            ball.angular_damping = 0.0;
            let ball = world.insert_body(ball).unwrap();
            (world, ball)
        }

        let (mut cached_world, cached_ball) = world_with_ball(0.0);
        cached_world.step_fixed(1.0 / 60.0).unwrap();
        cached_world
            .apply_command(
                PhysicsCommand::Teleport {
                    body: cached_ball,
                    pose: Pose::new(Vec3::new(5.0, 0.49, 0.0), Quat::IDENTITY).unwrap(),
                },
                0.0,
            )
            .unwrap();
        cached_world
            .apply_command(
                PhysicsCommand::SetVelocity {
                    body: cached_ball,
                    velocity: Vec3::new(0.0, 0.0, 10.0),
                },
                0.0,
            )
            .unwrap();
        cached_world.step_fixed(1.0 / 60.0).unwrap();

        let (mut fresh_world, fresh_ball) = world_with_ball(5.0);
        fresh_world
            .apply_command(
                PhysicsCommand::SetVelocity {
                    body: fresh_ball,
                    velocity: Vec3::new(0.0, 0.0, 10.0),
                },
                0.0,
            )
            .unwrap();
        fresh_world.step_fixed(1.0 / 60.0).unwrap();

        let cached_angular = cached_world.body_angular_velocity(cached_ball).unwrap();
        let fresh_angular = fresh_world.body_angular_velocity(fresh_ball).unwrap();
        assert!(
            cached_angular.y.abs() < 1.0e-3 && (cached_angular.y - fresh_angular.y).abs() < 1.0e-3,
            "cached={cached_angular:?}, fresh={fresh_angular:?}"
        );
    }

    #[test]
    fn many_overlapping_static_colliders_do_not_exhaust_the_broadphase_comparison_limit() {
        // 1500 статиков с перекрытием по X: их попарные сравнения не нужны (пары не порождаются),
        // поэтому лимит сравнений не исчерпывается, а динамическое тело шагает.
        let mut world = PhysicsWorld::new(Vec3::new(0.0, -9.81, 0.0)).unwrap();
        for index in 0..1500 {
            world
                .insert_body(BodyDesc::static_collider(
                    box_shape(0.1),
                    Pose::new(Vec3::new(0.0, 0.0, index as f32 * 0.25), Quat::IDENTITY).unwrap(),
                ))
                .unwrap();
        }
        let falling = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.1),
                Pose::new(Vec3::new(0.0, 3.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        let result = driver
            .advance_step(1.0 / 60.0, Vec::new())
            .expect("static-only overlaps must not exhaust the broadphase limit");

        let falling_y = result
            .poses
            .iter()
            .find(|(handle, _)| *handle == falling)
            .unwrap()
            .1
            .position
            .y;
        assert!(falling_y < 3.0, "{falling_y}");
    }

    #[test]
    fn character_pose_jump_through_a_crate_gives_the_crate_no_velocity() {
        // Скорость kinematic-капсулы контроллера в солвере нулевая (velocity_at), поэтому перенос
        // позы через ящик не передаёт ему скорость; толчки идут только через импульс move.
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut character = BodyDesc::new(
            BodyKind::Kinematic,
            Pose::new(Vec3::new(-2.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
            ColliderDesc::new(Shape::capsule(0.25, 0.5).unwrap()),
        );
        character.character_controller = true;
        let character = world.insert_body(character).unwrap();
        let crate_body = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.25),
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::KinematicTarget {
                    body: character,
                    pose: Pose::new(Vec3::new(2.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();

        assert_eq!(driver.world().body_velocity(crate_body), Some(Vec3::ZERO));
    }

    #[test]
    fn narrowphase_limit_skips_only_the_failing_pair_for_one_step() {
        // Веер из MAX_MESH_CANDIDATES + 1 треугольников у начала координат: пара «ящик–веер»
        // упирается в лимит кандидатов narrowphase, а свободно падающее тело шагает дальше.
        let ring = crate::narrow::MAX_MESH_CANDIDATES + 1;
        let mut vertices = vec![Vec3::new(0.0, 0.5, 0.0)];
        vertices.extend((0..ring).map(|index| {
            let angle = index as f32 / ring as f32 * std::f32::consts::TAU;
            Vec3::new(0.1 * angle.cos(), 0.5, 0.1 * angle.sin())
        }));
        let triangles = (0..ring)
            .map(|index| [0, index as u32 + 1, ((index + 1) % ring) as u32 + 1])
            .collect();
        let mut world = PhysicsWorld::new(Vec3::new(0.0, -9.81, 0.0)).unwrap();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::TriMesh(Arc::new(TriMesh::new(vertices, triangles).unwrap())),
                Pose::IDENTITY,
            ))
            .unwrap();
        world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.1),
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let falling = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.1),
                Pose::new(Vec3::new(10.0, 5.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        let result = driver
            .advance_step(1.0 / 60.0, Vec::new())
            .expect("narrowphase limit must not fail the step");

        assert!(result.skipped_pairs >= 1, "{result:?}");
        let falling_y = result
            .poses
            .iter()
            .find(|(handle, _)| *handle == falling)
            .unwrap()
            .1
            .position
            .y;
        assert!(falling_y < 5.0, "{falling_y}");
    }

    #[test]
    fn collider_origin_and_offset_preserve_center_of_mass_inertia() {
        let box_shape = box_shape(0.5);
        let origin_box = angular_response(
            box_shape.clone(),
            Pose::IDENTITY,
            Vec3::ZERO,
            Vec3::X,
            Vec3::Y,
        );
        let offset_box = angular_response(
            box_shape,
            Pose::new(Vec3::new(-2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::X,
            Vec3::Y,
        );
        assert!((origin_box.z - 6.0).abs() < 1.0e-5, "{origin_box:?}");
        assert!(origin_box.abs_diff_eq(offset_box, 1.0e-5), "{offset_box:?}");

        let hull = Shape::ConvexHull(Arc::new(
            Hull::from_points(&[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z]).unwrap(),
        ));
        let rotation = Quat::from_rotation_y(0.63) * Quat::from_rotation_z(-0.27);
        let collider_pose = Pose::new(Vec3::new(3.0, 2.0, -1.0), rotation).unwrap();
        let center = hull.mass_properties(1.0).unwrap().center_of_mass;
        let point = collider_pose.position + rotation * (center + Vec3::X);
        let impulse = rotation * Vec3::Y;
        let origin_hull = angular_response(hull.clone(), collider_pose, Vec3::ZERO, point, impulse);
        let offset = Vec3::new(1.5, -0.7, 2.0);
        let shifted_origin =
            Pose::new(collider_pose.position - rotation * offset, rotation).unwrap();
        let offset_hull = angular_response(hull, shifted_origin, offset, point, impulse);
        assert!(
            origin_hull.abs_diff_eq(offset_hull, 1.0e-5),
            "origin={origin_hull:?}, offset={offset_hull:?}"
        );
    }

    fn ball_desc(y: f32) -> BodyDesc {
        BodyDesc::dynamic(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(0.0, y, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        )
    }

    fn involves(event: &PhysicsEvent, body: BodyHandle) -> bool {
        match event {
            PhysicsEvent::CollisionEnter { first, second, .. }
            | PhysicsEvent::CollisionExit { first, second }
            | PhysicsEvent::TriggerEnter { first, second }
            | PhysicsEvent::TriggerExit { first, second } => *first == body || *second == body,
        }
    }

    #[test]
    fn insert_with_handle_rejects_occupied_handle_and_moves_next_id_past_it() {
        let mut world = PhysicsWorld::default();
        assert_eq!(
            world.insert_body_with_handle(BodyHandle(5), floor_desc()),
            Ok(BodyHandle(5))
        );
        assert_eq!(
            world.insert_body_with_handle(BodyHandle(5), floor_desc()),
            Err(PhysicsWorldError::DuplicateBody(BodyHandle(5)))
        );
        assert_eq!(world.insert_body(floor_desc()), Ok(BodyHandle(6)));
    }

    #[test]
    fn invalid_insert_leaves_its_handle_and_the_next_id_untouched() {
        let mut world = PhysicsWorld::default();
        let mut invalid = ball_desc(0.0);
        invalid.mass = 0.0;
        assert_eq!(invalid.validate(), Err(PhysicsWorldError::InvalidBody));
        assert_eq!(
            world.insert_body_with_handle(BodyHandle(3), invalid),
            Err(PhysicsWorldError::InvalidBody)
        );
        assert!(world.body_pose(BodyHandle(3)).is_none());
        assert_eq!(world.insert_body(floor_desc()), Ok(BodyHandle(1)));
    }

    #[test]
    fn inserted_body_falls_and_collides_with_the_floor_in_later_steps() {
        let mut driver = StepDriver::new(PhysicsWorld::default(), 60, 8).unwrap();
        driver.world_mut().insert_body(floor_desc()).unwrap();
        let ball = BodyHandle(7);
        let inserted = driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: ball,
                    desc: ball_desc(1.0),
                }],
                [],
            )
            .unwrap();
        assert!(driver.world().body_pose(ball).is_some());
        assert_eq!(
            count_events(&inserted.events, |event| involves(event, ball)),
            0
        );

        let events = run_steps(&mut driver, 120);
        assert_eq!(
            count_events(&events, |event| matches!(
                event,
                PhysicsEvent::CollisionEnter { .. }
            ) && involves(event, ball)),
            1
        );
    }

    #[test]
    fn velocity_command_applies_to_body_inserted_in_the_same_step() {
        let mut driver = StepDriver::new(PhysicsWorld::new(Vec3::ZERO).unwrap(), 60, 8).unwrap();
        let handle = BodyHandle(4);
        let desc = BodyDesc::dynamic(Shape::sphere(0.5).unwrap(), Pose::IDENTITY, 1.0);
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert { handle, desc }],
                [PhysicsCommand::SetVelocity {
                    body: handle,
                    velocity: Vec3::X,
                }],
            )
            .unwrap();
        let velocity = driver.world().body_velocity(handle).unwrap();
        assert!(velocity.abs_diff_eq(Vec3::X, 1.0e-5), "{velocity:?}");
    }

    #[test]
    fn removing_a_colliding_body_emits_collision_exit_and_unknown_removal_is_ignored() {
        let mut driver = StepDriver::new(PhysicsWorld::default(), 60, 8).unwrap();
        driver.world_mut().insert_body(floor_desc()).unwrap();
        let ball = BodyHandle(7);
        let inserted = driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: ball,
                    desc: ball_desc(0.4),
                }],
                [],
            )
            .unwrap();
        assert_eq!(
            count_events(&inserted.events, |event| matches!(
                event,
                PhysicsEvent::CollisionEnter { .. }
            ) && involves(event, ball)),
            1
        );

        let removed = driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [
                    BodyChange::Remove { handle: ball },
                    BodyChange::Remove {
                        handle: BodyHandle(99),
                    },
                ],
                [],
            )
            .unwrap();
        assert_eq!(
            count_events(&removed.events, |event| matches!(
                event,
                PhysicsEvent::CollisionExit { .. }
            ) && involves(event, ball)),
            1
        );
        assert!(driver.world().body_pose(ball).is_none());
    }

    #[test]
    fn removing_a_body_in_a_trigger_emits_trigger_exit() {
        let mut driver = StepDriver::new(PhysicsWorld::new(Vec3::ZERO).unwrap(), 60, 8).unwrap();
        let mut trigger = BodyDesc::static_collider(
            Shape::sphere(1.0).unwrap(),
            Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        let trigger_body = driver.world_mut().insert_body(trigger).unwrap();
        let ball = BodyHandle(9);
        let entered = driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: ball,
                    desc: ball_desc(0.0),
                }],
                [],
            )
            .unwrap();
        assert_eq!(
            count_events(&entered.events, |event| matches!(
                event,
                PhysicsEvent::TriggerEnter { .. }
            ) && involves(event, ball)),
            1
        );

        let exited = driver
            .advance_step_with_changes(1.0 / 60.0, [BodyChange::Remove { handle: ball }], [])
            .unwrap();
        assert!(exited.events.iter().any(|event| matches!(
            event,
            PhysicsEvent::TriggerExit { first, second }
                if (*first == ball && *second == trigger_body)
                    || (*first == trigger_body && *second == ball)
        )));
    }

    /// Квад 4×4 в плоскости y = 0 из двух треугольников; диагональ проходит через x = z.
    fn diagonal_mesh_floor_desc() -> BodyDesc {
        let vertices = vec![
            Vec3::new(-2.0, 0.0, -2.0),
            Vec3::new(2.0, 0.0, -2.0),
            Vec3::new(2.0, 0.0, 2.0),
            Vec3::new(-2.0, 0.0, 2.0),
        ];
        BodyDesc::static_collider(
            Shape::TriMesh(Arc::new(
                TriMesh::new(vertices, vec![[0, 1, 2], [0, 2, 3]]).unwrap(),
            )),
            Pose::IDENTITY,
        )
    }

    /// Ящик половины 0.5, центр которого лежит на диагонали квада пола.
    fn box_over_diagonal(hz: u32) -> (StepDriver, BodyHandle) {
        let mut world = PhysicsWorld::default();
        world.insert_body(diagonal_mesh_floor_desc()).unwrap();
        let body = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(0.0, 0.7, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        (StepDriver::new(world, hz, 8).unwrap(), body)
    }

    fn stacked_boxes_world(count: usize) -> (StepDriver, Vec<BodyHandle>) {
        let mut world = PhysicsWorld::default();
        world.insert_body(floor_desc()).unwrap();
        let mut boxes = Vec::new();
        for index in 0..count {
            let mut desc = BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(0.0, 0.5 + index as f32, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            );
            desc.linear_damping = 0.02;
            boxes.push(world.insert_body(desc).unwrap());
        }
        (StepDriver::new(world, 60, 8).unwrap(), boxes)
    }

    fn sleeping_stack_driver() -> (StepDriver, Vec<BodyHandle>) {
        let (mut driver, boxes) = stacked_boxes_world(3);
        run_steps(&mut driver, 300);
        assert!(
            boxes
                .iter()
                .all(|handle| driver.world().body_is_sleeping(*handle).unwrap())
        );
        (driver, boxes)
    }

    #[test]
    fn all_sleeping_world_reports_no_poses_and_keeps_pairs_and_events() {
        let (mut driver, _) = sleeping_stack_driver();
        let pairs = driver.world().active_pair_count();
        let result = driver.advance_step(1.0 / 60.0, []).unwrap();
        assert_eq!(result.substeps, 1);
        assert!(result.poses.is_empty(), "{:?}", result.poses);
        assert!(result.events.is_empty(), "{:?}", result.events);
        assert_eq!(driver.world().active_pair_count(), pairs);
    }

    #[test]
    fn teleport_of_a_sleeping_body_reports_its_pose_without_substeps() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let target = Pose::new(Vec3::new(3.0, 0.5, 0.0), Quat::IDENTITY).unwrap();
        let result = driver
            .advance_step(
                0.0,
                [PhysicsCommand::Teleport {
                    body: boxes[1],
                    pose: target,
                }],
            )
            .unwrap();
        assert_eq!(result.substeps, 0);
        assert_eq!(result.poses, vec![(boxes[1], target)]);
    }

    #[test]
    fn inserted_body_reports_its_pose_and_sleeping_stack_stays_out_of_poses() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let handle = BodyHandle(100);
        let result = driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle,
                    desc: BodyDesc::dynamic(
                        box_shape(0.5),
                        Pose::new(Vec3::new(8.0, 3.0, 0.0), Quat::IDENTITY).unwrap(),
                        1.0,
                    ),
                }],
                [],
            )
            .unwrap();
        assert!(result.poses.iter().any(|(body, _)| *body == handle));
        assert!(
            result.poses.iter().all(|(body, _)| !boxes.contains(body)),
            "{:?}",
            result.poses
        );
    }

    #[test]
    fn initial_result_still_reports_every_body() {
        let (driver, boxes) = sleeping_stack_driver();
        let initial = driver.initial_result();
        assert_eq!(initial.poses.len(), boxes.len() + 1);
    }

    #[test]
    fn static_inserted_against_a_sleeping_box_wakes_it() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let top = *boxes.last().unwrap();
        let plate_center_y = 3.0 + 0.05 - 0.01;
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: BodyHandle(100),
                    desc: BodyDesc::static_collider(
                        box_shape(0.05),
                        Pose::new(Vec3::new(0.0, plate_center_y, 0.0), Quat::IDENTITY).unwrap(),
                    ),
                }],
                [],
            )
            .unwrap();
        assert_eq!(driver.world().body_is_sleeping(top), Some(false));
    }

    #[test]
    fn falling_body_wakes_the_sleeping_stack_it_lands_on() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let bottom = boxes[0];
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: BodyHandle(100),
                    desc: BodyDesc::dynamic(
                        box_shape(0.5),
                        Pose::new(Vec3::new(0.0, 6.0, 0.0), Quat::IDENTITY).unwrap(),
                        1.0,
                    ),
                }],
                [],
            )
            .unwrap();
        let mut woke = false;
        for _ in 0..240 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            woke |= driver.world().body_is_sleeping(bottom) == Some(false);
        }
        assert!(woke);
    }

    #[test]
    fn stack_of_boxes_sleeps_as_one_island_within_three_seconds_and_stays_asleep() {
        let (mut driver, boxes) = stacked_boxes_world(4);
        let mut first_sleep_frame = vec![None; boxes.len()];
        for frame in 0..420 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            for (index, handle) in boxes.iter().enumerate() {
                let sleeping = driver.world().body_is_sleeping(*handle).unwrap();
                if first_sleep_frame[index].is_none() && sleeping {
                    first_sleep_frame[index] = Some(frame);
                }
                if first_sleep_frame[index].is_some() {
                    assert!(sleeping, "box {index} woke at frame {frame}");
                }
            }
        }
        let frames: Vec<usize> = first_sleep_frame
            .iter()
            .map(|frame| frame.expect("every box of the stack sleeps"))
            .collect();
        assert!(frames.iter().all(|frame| *frame < 180), "{frames:?}");
        assert!(frames.iter().all(|frame| *frame == frames[0]), "{frames:?}");
    }

    #[test]
    fn impulse_on_top_box_wakes_the_whole_sleeping_stack_in_the_same_step() {
        let (mut driver, boxes) = stacked_boxes_world(4);
        run_steps(&mut driver, 240);
        assert!(
            boxes
                .iter()
                .all(|handle| driver.world().body_is_sleeping(*handle).unwrap())
        );
        let top = *boxes.last().unwrap();
        driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::AddImpulse {
                    body: top,
                    impulse: Vec3::Y * 0.5,
                    point: None,
                }],
            )
            .unwrap();
        for handle in &boxes {
            assert_eq!(driver.world().body_is_sleeping(*handle), Some(false));
        }
    }

    #[test]
    fn separate_islands_on_one_floor_sleep_and_wake_independently() {
        let mut world = PhysicsWorld::default();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(20.0, 0.5, 20.0)).unwrap(),
                Pose::new(Vec3::new(0.0, -0.5, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let near = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(-4.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let far = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(4.0, 10.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        run_steps(&mut driver, 90);
        assert_eq!(driver.world().body_is_sleeping(near), Some(true));
        assert_eq!(driver.world().body_is_sleeping(far), Some(false));
        run_steps(&mut driver, 330);
        assert_eq!(driver.world().body_is_sleeping(far), Some(true));

        driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::AddImpulse {
                    body: far,
                    impulse: Vec3::Y * 0.5,
                    point: None,
                }],
            )
            .unwrap();
        assert_eq!(driver.world().body_is_sleeping(far), Some(false));
        assert_eq!(driver.world().body_is_sleeping(near), Some(true));
    }

    #[test]
    fn sleeping_contact_islands_never_hold_awake_sleeping_dynamic_pairs() {
        let (mut driver, _) = stacked_boxes_world(4);
        for _ in 0..240 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            let world = driver.world();
            for pair in world.active_pairs.keys() {
                let (Some(first), Some(second)) = (
                    world.bodies.get(&BodyHandle(pair.first)),
                    world.bodies.get(&BodyHandle(pair.second)),
                ) else {
                    continue;
                };
                if first.kind == BodyKind::Dynamic && second.kind == BodyKind::Dynamic {
                    assert_eq!(first.sleeping, second.sleeping, "{pair:?}");
                }
            }
        }
    }

    #[test]
    fn box_riding_a_slowly_moving_kinematic_platform_never_cycles_sleep_and_wake() {
        let mut world = PhysicsWorld::default();
        let platform = world
            .insert_body(BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::new(0.0, -0.5, 0.0), Quat::IDENTITY).unwrap(),
                ColliderDesc::new(Shape::cuboid(Vec3::new(20.0, 0.5, 20.0)).unwrap()),
            ))
            .unwrap();
        let rider = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(0.0, 0.7, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        for frame in 0..300 {
            let x = 0.02 * (frame + 1) as f32 / 60.0;
            driver
                .advance_step(
                    1.0 / 60.0,
                    [PhysicsCommand::KinematicTarget {
                        body: platform,
                        pose: Pose::new(Vec3::new(x, -0.5, 0.0), Quat::IDENTITY).unwrap(),
                    }],
                )
                .unwrap();
            if frame >= 60 {
                assert_eq!(
                    driver.world().body_is_sleeping(rider),
                    Some(false),
                    "frame {frame}"
                );
            }
        }
    }

    #[test]
    fn resting_box_penetration_stays_within_one_centimeter_at_common_rates() {
        for hz in [30_u32, 60, 120] {
            for floor in [diagonal_mesh_floor_desc(), floor_desc()] {
                let mut world = PhysicsWorld::default();
                world.insert_body(floor).unwrap();
                let body = world
                    .insert_body(BodyDesc::dynamic(
                        box_shape(0.5),
                        Pose::new(Vec3::new(0.0, 0.7, 0.0), Quat::IDENTITY).unwrap(),
                        1.0,
                    ))
                    .unwrap();
                let mut driver = StepDriver::new(world, hz, 8).unwrap();
                for _ in 0..(5 * hz) {
                    driver.advance_step(1.0 / hz as f32, []).unwrap();
                }
                // Верх обоих полов в y = 0, нижняя грань ящика в y = center - 0.5.
                let penetration = 0.5 - driver.world().body_pose(body).unwrap().position.y;
                assert!(
                    penetration.abs() <= 0.01,
                    "hz {hz}: steady penetration {penetration}"
                );
            }
        }
    }

    #[test]
    fn box_on_diagonal_mesh_floor_rests_without_jitter_or_drift() {
        let (mut driver, body) = box_over_diagonal(60);
        let mut peak_speed = 0.0_f32;
        let mut peak_spin = 0.0_f32;
        let mut peak_drift = 0.0_f32;
        for frame in 0..=60 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            if frame < 30 {
                continue;
            }
            // Спящее тело не имеет скорости по определению; проверяем покой только пока оно не спит.
            if driver.world().body_is_sleeping(body) == Some(false) {
                peak_speed = peak_speed.max(driver.world().body_velocity(body).unwrap().length());
                peak_spin =
                    peak_spin.max(driver.world().body_angular_velocity(body).unwrap().length());
            }
            let pose = driver.world().body_pose(body).unwrap();
            peak_drift = peak_drift.max(pose.position.x.abs().max(pose.position.z.abs()));
        }
        assert!(peak_speed < SLEEP_LINEAR_SPEED, "peak speed {peak_speed}");
        assert!(peak_spin < SLEEP_ANGULAR_SPEED, "peak spin {peak_spin}");
        assert!(peak_drift < 1.0e-3, "horizontal drift {peak_drift}");
    }
}
