use std::collections::{BTreeMap, HashSet};
use std::fmt;

use glam::{Mat3, Quat, Vec3};

use crate::broad::sweep_and_prune;
use crate::narrow::{ManifoldCache, PairKey, collide};
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
    pub mass: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub gravity_scale: f32,
    pub lock_rotation: bool,
}

impl BodyDesc {
    pub fn new(kind: BodyKind, pose: Pose, collider: ColliderDesc) -> Self {
        Self {
            kind,
            pose,
            collider,
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
    pub(crate) linear_damping: f32,
    pub(crate) angular_damping: f32,
    pub(crate) gravity_scale: f32,
    pub(crate) sleeping: bool,
    pub(crate) sleep_time: f32,
    pub(crate) kinematic_target: Option<Pose>,
}

impl Body {
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

    fn integrate_dynamic(&mut self, gravity: Vec3, dt: f32) {
        if self.kind != BodyKind::Dynamic || self.sleeping {
            return;
        }
        self.linear_velocity += gravity * (self.gravity_scale * dt);
        self.linear_velocity *= (-self.linear_damping * dt).exp();
        self.angular_velocity *= (-self.angular_damping * dt).exp();

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
        })
    }

    pub fn insert_body(&mut self, desc: BodyDesc) -> Result<BodyHandle, PhysicsWorldError> {
        validate_body_desc(&desc)?;
        let pose = Pose::new(desc.pose.position, desc.pose.rotation)?;
        let (inverse_mass, center_of_mass_local, inverse_inertia_local) =
            if desc.kind == BodyKind::Dynamic {
                let properties = desc.collider.shape.mass_properties(desc.mass)?;
                let center = desc.collider.offset + properties.center_of_mass;
                let parallel_axis = Mat3::from_diagonal(Vec3::splat(center.length_squared()))
                    - Mat3::from_cols(center * center.x, center * center.y, center * center.z);
                let inertia = properties.inertia + parallel_axis * desc.mass;
                let inverse_inertia = if desc.lock_rotation {
                    Mat3::ZERO
                } else {
                    inertia
                        .try_inverse()
                        .ok_or(PhysicsWorldError::InvalidBody)?
                };
                (1.0 / desc.mass, center, inverse_inertia)
            } else {
                (0.0, Vec3::ZERO, Mat3::ZERO)
            };
        let handle = BodyHandle(self.next_body_id);
        self.next_body_id = self.next_body_id.wrapping_add(1).max(1);
        self.bodies.insert(
            handle,
            Body {
                handle,
                kind: desc.kind,
                pose,
                collider: desc.collider,
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
            },
        );
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
        for body in self.bodies.values_mut() {
            body.integrate_dynamic(self.gravity, dt);
            body.advance_kinematic(dt, remaining_substeps);
        }
        let events = self.refresh_pairs()?;
        crate::solver::solve(
            &mut self.bodies,
            &mut self.active_pairs,
            &mut self.manifolds,
            dt,
        );
        self.update_sleep(dt);
        Ok(events)
    }

    fn refresh_pairs(&mut self) -> Result<Vec<PhysicsEvent>, PhysicsWorldError> {
        let mut proxies = Vec::new();
        let mut bounds = Vec::new();
        for body in self.bodies.values() {
            if !body.collider.enabled {
                continue;
            }
            proxies.push((
                body.handle,
                body.collider.clone(),
                body.kind,
                body.collider_pose(),
            ));
            bounds.push(body.collider_aabb()?);
        }
        let candidates = sweep_and_prune(&bounds)?;
        let mut current = BTreeMap::new();
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
            if pair_kind == PairKind::Collision
                && self.pair_can_remain_sleeping(*handle_a, *handle_b)
                && let Some(previous) = self.active_pairs.get(&key)
            {
                current.insert(key, previous.clone());
                continue;
            }
            let Some(mut manifold) =
                collide(&collider_a.shape, *pose_a, &collider_b.shape, *pose_b)?
            else {
                continue;
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

    fn pair_can_remain_sleeping(&self, first: BodyHandle, second: BodyHandle) -> bool {
        let Some(a) = self.bodies.get(&first) else {
            return false;
        };
        let Some(b) = self.bodies.get(&second) else {
            return false;
        };
        let sleeping_dynamic = |body: &Body| body.kind == BodyKind::Dynamic && body.sleeping;
        let stationary_non_dynamic = |body: &Body| {
            body.kind == BodyKind::Static
                || (body.kind == BodyKind::Kinematic
                    && body.linear_velocity.length_squared() <= WAKE_SPEED_SQUARED
                    && body.angular_velocity.length_squared() <= WAKE_SPEED_SQUARED)
        };
        (sleeping_dynamic(a) && (sleeping_dynamic(b) || stationary_non_dynamic(b)))
            || (sleeping_dynamic(b) && (sleeping_dynamic(a) || stationary_non_dynamic(a)))
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

    fn wake_body(&mut self, handle: BodyHandle) {
        if let Some(body) = self.bodies.get_mut(&handle) {
            body.wake();
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
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.linear_velocity = velocity;
                body.wake();
            }
            PhysicsCommand::AddImpulse { impulse, point, .. } => {
                if body.kind != BodyKind::Dynamic {
                    return Err(PhysicsWorldError::WrongBodyKind(handle));
                }
                if !impulse.is_finite() || point.is_some_and(|point| !point.is_finite()) {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                let point = point.unwrap_or_else(|| body.center_of_mass_world());
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.wake();
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
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.wake();
                body.apply_impulse(point, impulse);
            }
            PhysicsCommand::Teleport { pose, .. } => {
                let pose = Pose::new(pose.position, pose.rotation)?;
                self.wake_neighbors(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.pose = pose;
                body.kinematic_target = None;
                if body.kind == BodyKind::Kinematic {
                    body.linear_velocity = Vec3::ZERO;
                    body.angular_velocity = Vec3::ZERO;
                }
                body.wake();
            }
            PhysicsCommand::SetColliderEnabled { enabled, .. } => {
                if body.collider.enabled != enabled {
                    self.wake_neighbors(handle);
                    let body = self.bodies.get_mut(&handle).expect("body was checked");
                    body.collider.enabled = enabled;
                    body.wake();
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

    fn update_sleep(&mut self, dt: f32) {
        for body in self.bodies.values_mut() {
            if body.kind != BodyKind::Dynamic || body.sleeping {
                continue;
            }
            if body.linear_velocity.length_squared() <= SLEEP_LINEAR_SPEED.powi(2)
                && body.angular_velocity.length_squared() <= SLEEP_ANGULAR_SPEED.powi(2)
            {
                body.sleep_time += dt;
                if body.sleep_time >= SLEEP_DELAY_SECONDS {
                    body.sleeping = true;
                    body.linear_velocity = Vec3::ZERO;
                    body.angular_velocity = Vec3::ZERO;
                }
            } else {
                body.sleep_time = 0.0;
            }
        }
    }
}

fn is_moving_kinematic(body: &Body) -> bool {
    body.kind == BodyKind::Kinematic
        && (body.linear_velocity.length_squared() > WAKE_SPEED_SQUARED
            || body.angular_velocity.length_squared() > WAKE_SPEED_SQUARED)
}

fn validate_body_desc(desc: &BodyDesc) -> Result<(), PhysicsWorldError> {
    desc.collider.shape.local_aabb()?;
    if !desc.collider.offset.is_finite()
        || !desc.mass.is_finite()
        || !desc.linear_damping.is_finite()
        || !desc.angular_damping.is_finite()
        || !desc.gravity_scale.is_finite()
        || desc.linear_damping < 0.0
        || desc.angular_damping < 0.0
        || (desc.kind == BodyKind::Dynamic && desc.mass <= 0.0)
        || (desc.kind == BodyKind::Dynamic && matches!(desc.collider.shape, Shape::TriMesh(_)))
    {
        return Err(PhysicsWorldError::InvalidBody);
    }
    if !desc.collider.friction.is_finite()
        || desc.collider.friction < 0.0
        || !desc.collider.restitution.is_finite()
        || !(0.0..=1.0).contains(&desc.collider.restitution)
    {
        return Err(PhysicsWorldError::InvalidMaterial);
    }
    Pose::new(desc.pose.position, desc.pose.rotation)?;
    Ok(())
}

impl PhysicsCommand {
    fn body(&self) -> BodyHandle {
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
    pub poses: Vec<(BodyHandle, Pose)>,
    pub events: Vec<PhysicsEvent>,
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

    /// Applies frame commands once, then runs the capped number of fixed substeps.
    pub fn advance_step(
        &mut self,
        dt: f32,
        commands: impl IntoIterator<Item = PhysicsCommand>,
    ) -> Result<StepResult, PhysicsWorldError> {
        validate_nonnegative_dt(dt)?;
        for command in commands {
            self.world.apply_command(command, dt)?;
        }

        let mut events = self.world.refresh_pairs()?;
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
        let poses = self
            .world
            .bodies
            .values()
            .map(|body| (body.handle, body.pose))
            .collect();
        Ok(StepResult {
            substeps,
            poses,
            events,
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
        BodyDesc, BodyHandle, BodyKind, ColliderDesc, PhysicsCommand, PhysicsEvent, PhysicsWorld,
        PhysicsWorldError, StepDriver,
    };
    use crate::{Pose, Shape};
    use glam::{Quat, Vec3};

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
                pose.position.x.abs() < 0.15 && pose.position.z.abs() < 0.15,
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
        driver.advance_step(0.4, []).unwrap();
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
}
