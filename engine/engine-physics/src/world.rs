use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

use glam::{Mat3, Quat, Vec3};

use crate::broad::{BroadItem, BroadPhase, BroadPhaseOutput, CandidatePair};
use crate::material::PhysicsMaterial;
use crate::narrow::{ManifoldCache, PairKey, collide};
use crate::query::{ColliderTree, QueryCollider, QueryColliderDesc, QueryError, QuerySnapshot};
use crate::{Aabb, BroadPhaseError, ContactManifold, GeometryError, NarrowphaseError, Pose, Shape};

/// Число слоёв столкновений; слой тела — индекс в диапазоне `0..PHYSICS_LAYER_COUNT`.
pub const PHYSICS_LAYER_COUNT: usize = 32;
pub const SOLVER_ITERATIONS: usize = 10;
pub const SLEEP_LINEAR_SPEED: f32 = 0.05;
pub const SLEEP_ANGULAR_SPEED: f32 = 0.05;
pub const SLEEP_DELAY_SECONDS: f32 = 0.5;
pub(crate) const POSITIONAL_SLOP: f32 = 0.005;
pub(crate) const POSITIONAL_CORRECTION: f32 = 0.2;
pub(crate) const RESTITUTION_THRESHOLD: f32 = 1.0;
const WAKE_SPEED_SQUARED: f32 = 1.0e-6;

fn default_solver_threads() -> NonZeroUsize {
    let count = std::thread::available_parallelism().map_or(1, |available| available.get().min(4));
    NonZeroUsize::new(count).expect("solver thread count is nonzero")
}

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
    pub material: PhysicsMaterial,
    pub enabled: bool,
}

impl ColliderDesc {
    pub fn new(shape: Shape) -> Self {
        Self {
            shape,
            offset: Vec3::ZERO,
            trigger: false,
            material: PhysicsMaterial::from_friction_restitution(0.5, 0.0),
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
    /// Слой столкновений `0..PHYSICS_LAYER_COUNT`; пара тел фильтруется матрицей мира.
    pub layer: u32,
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
            layer: 0,
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
            || self.layer as usize >= PHYSICS_LAYER_COUNT
        {
            return Err(PhysicsWorldError::InvalidBody);
        }
        self.collider.material.validate()?;
        Pose::new(self.pose.position, self.pose.rotation)?;
        self.mass_terms()?;
        Ok(())
    }

    fn mass_terms(&self) -> Result<(f32, Vec3, Mat3), PhysicsWorldError> {
        mass_terms(self.kind, &self.collider, self.mass, self.lock_rotation)
    }
}

/// Обратная масса, центр масс и обратный тензор инерции в локальных координатах.
fn mass_terms(
    kind: BodyKind,
    collider: &ColliderDesc,
    mass: f32,
    lock_rotation: bool,
) -> Result<(f32, Vec3, Mat3), PhysicsWorldError> {
    if kind != BodyKind::Dynamic {
        return Ok((0.0, Vec3::ZERO, Mat3::ZERO));
    }
    let properties = collider.shape.mass_properties(mass)?;
    let center = collider.offset + properties.center_of_mass;
    let inverse_inertia = if lock_rotation {
        Mat3::ZERO
    } else {
        properties
            .inertia
            .try_inverse()
            .ok_or(PhysicsWorldError::InvalidBody)?
    };
    Ok((1.0 / mass, center, inverse_inertia))
}

/// Примитивная форма для команды смены коллайдера. Размеры уже запечены по масштабу объекта
/// вызывающей стороной, поэтому команда `Copy` и не содержит mesh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PrimitiveShape {
    Box { half_extents: Vec3 },
    Sphere { radius: f32 },
    Capsule { radius: f32, height: f32 },
}

impl PrimitiveShape {
    /// Примитив, из которого построена форма; `None` для выпуклых оболочек и mesh.
    pub fn from_shape(shape: &Shape) -> Option<Self> {
        match *shape {
            Shape::Box { half_extents } => Some(Self::Box { half_extents }),
            Shape::Sphere { radius } => Some(Self::Sphere { radius }),
            Shape::Capsule { radius, height } => Some(Self::Capsule { radius, height }),
            _ => None,
        }
    }

    pub fn to_shape(self) -> Result<Shape, GeometryError> {
        match self {
            Self::Box { half_extents } => Shape::cuboid(half_extents),
            Self::Sphere { radius } => Shape::sphere(radius),
            Self::Capsule { radius, height } => Shape::capsule(radius, height),
        }
    }
}

/// Пара тел сталкивается, только если каждый из слоёв разрешает другой.
pub(crate) fn layers_collide(matrix: &[u32; PHYSICS_LAYER_COUNT], first: u32, second: u32) -> bool {
    let (first, second) = (first as usize, second as usize);
    (matrix[first] >> second) & 1 != 0 && (matrix[second] >> first) & 1 != 0
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
    /// Заменяет форму и смещение коллайдера; масса пересчитывается по новой форме.
    SetCollider {
        body: BodyHandle,
        shape: PrimitiveShape,
        offset: Vec3,
    },
    SetTrigger {
        body: BodyHandle,
        trigger: bool,
    },
    SetBodyProperties {
        body: BodyHandle,
        kind: BodyKind,
        mass: f32,
        linear_damping: f32,
        angular_damping: f32,
        gravity_scale: f32,
        lock_rotation: bool,
    },
    SetLayer {
        body: BodyHandle,
        layer: u32,
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
    pub(crate) layer: u32,
    /// Масса и lock_rotation нужны для пересчёта инерции при смене формы или свойств.
    mass: f32,
    lock_rotation: bool,
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

    /// Форма, смещение, trigger или слой изменились: пары пересчитываются, AABB — заново.
    fn mark_collider_changed(&mut self) {
        self.aabb = None;
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
    /// Персистентный порядок broadphase между шагами.
    broad: BroadPhase,
    /// Число refresh'ей с обрезкой broadphase с начала текущего `advance_step*`.
    broadphase_truncations: usize,
    /// Broadphase обрезан и предупреждение уже записано; выхода из деградации ещё не было.
    broadphase_degraded: bool,
    /// Маска слоёв для каждого слоя; по умолчанию все пары разрешены.
    collision_matrix: [u32; PHYSICS_LAYER_COUNT],
    /// BVH static-коллайдеров последнего снимка запросов; `query_snapshot` переиспользует его,
    /// пока static-коллайдеры не менялись. `Mutex`, потому что снимок строится по `&self`.
    query_static_tree: Mutex<Option<Arc<ColliderTree>>>,
    solver_threads: NonZeroUsize,
    #[cfg(test)]
    test_parallel_min_constraints: usize,
    #[cfg(test)]
    parallel_solver_runs: usize,
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
            broad: BroadPhase::default(),
            broadphase_truncations: 0,
            broadphase_degraded: false,
            collision_matrix: [u32::MAX; PHYSICS_LAYER_COUNT],
            query_static_tree: Mutex::new(None),
            solver_threads: default_solver_threads(),
            #[cfg(test)]
            test_parallel_min_constraints: crate::solver::PARALLEL_MIN_CONSTRAINTS,
            #[cfg(test)]
            parallel_solver_runs: 0,
        })
    }

    /// Задаёт число рабочих потоков островного решателя.
    pub fn set_solver_threads(&mut self, threads: NonZeroUsize) {
        self.solver_threads = threads;
    }

    /// Возвращает число потоков, настроенное для островного решателя.
    pub fn solver_threads(&self) -> NonZeroUsize {
        self.solver_threads
    }

    /// Задаёт маски слоёв до вставки тел; пары, запрещённые матрицей, не создаются.
    pub fn set_collision_matrix(&mut self, masks: [u32; PHYSICS_LAYER_COUNT]) {
        self.collision_matrix = masks;
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
                layer: desc.layer,
                mass: desc.mass,
                lock_rotation: desc.lock_rotation,
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
                    layer: body.layer,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let trigger_pairs = self
            .active_pairs
            .iter()
            .filter(|(_, pair)| pair.kind == PairKind::Trigger)
            .map(|(key, _)| (BodyHandle(key.first), BodyHandle(key.second)))
            .collect();
        let mut cached_static = self
            .query_static_tree
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let snapshot = QuerySnapshot::from_colliders(
            colliders,
            trigger_pairs,
            self.gravity,
            self.collision_matrix,
            cached_static.as_ref(),
        )?;
        *cached_static = Some(Arc::clone(snapshot.static_tree()));
        Ok(Arc::new(snapshot))
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

    fn islands_for_step(&self) -> Vec<crate::island::Island> {
        if !self
            .bodies
            .values()
            .any(|body| body.kind == BodyKind::Dynamic && !body.sleeping)
        {
            return Vec::new();
        }

        crate::island::build(
            self.bodies
                .values()
                .filter(|body| body.kind == BodyKind::Dynamic)
                .map(|body| (body.handle, !body.sleeping)),
            &self.active_pairs,
        )
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
        let islands = self.islands_for_step();
        if islands.is_empty() {
            return Ok(events);
        }
        #[cfg(test)]
        let parallel_min_constraints = self.test_parallel_min_constraints;
        #[cfg(not(test))]
        let parallel_min_constraints = crate::solver::PARALLEL_MIN_CONSTRAINTS;
        let ran_parallel = crate::solver::solve(
            &mut self.bodies,
            &mut self.active_pairs,
            &mut self.manifolds,
            &islands,
            self.solver_threads,
            parallel_min_constraints,
        );
        #[cfg(test)]
        if ran_parallel {
            self.parallel_solver_runs += 1;
        }
        #[cfg(not(test))]
        let _ = ran_parallel;
        for body in self.bodies.values_mut() {
            body.integrate_position(dt);
        }
        crate::solver::correct_positions(&mut self.bodies, &self.active_pairs);
        self.update_sleep(dt, &islands);
        Ok(events)
    }

    /// Статик или неподвижный kinematic, который появился или сдвинулся, может оказаться под
    /// спящими телами. Broadphase спящие тела с неактивными не сравнивает, поэтому будим такие
    /// тела по AABB. Движущийся kinematic будит спящих контактом (`wake_contact_pair`).
    fn wake_sleepers_under_changed_statics(&mut self) -> Result<(), PhysicsWorldError> {
        let mut changed = Vec::new();
        for body in self.bodies.values() {
            let stationary_blocker = body.kind == BodyKind::Static
                || (body.kind == BodyKind::Kinematic && !is_moving_kinematic(body));
            if stationary_blocker
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
        let mut items = Vec::new();
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
            items.push(BroadItem {
                key: body.handle.0,
                bounds: aabb,
                paired: is_paired,
            });
            proxies.push((
                body.handle,
                body.collider.clone(),
                body.kind,
                body.collider_pose(),
                body.layer,
            ));
        }
        for body in self.bodies.values_mut() {
            body.pending_refresh = false;
        }
        // Неактивные тела не двигались и не менялись, поэтому их пары переносятся как есть.
        // Пары с активным телом пересчитываются broadphase и narrowphase.
        let output = self.broad.update(&items)?;
        self.note_broadphase(&output, items.len());
        let mut current = BTreeMap::new();
        for (key, pair) in &self.active_pairs {
            if frozen.contains(&BodyHandle(key.first)) && frozen.contains(&BodyHandle(key.second)) {
                current.insert(*key, pair.clone());
            }
        }
        // Пары, которые обрезанный sweep не сравнил, не переносятся вслепую: narrowphase проверяет
        // их заново, поэтому разошедшиеся пары дают Exit, а пересекающиеся сохраняются.
        let truncated = output.cut.is_some();
        let mut recheck = Vec::new();
        if truncated {
            let index_of = |handle: u64| items.binary_search_by_key(&handle, |item| item.key).ok();
            for key in self.active_pairs.keys() {
                if frozen.contains(&BodyHandle(key.first))
                    && frozen.contains(&BodyHandle(key.second))
                {
                    continue;
                }
                let (Some(first), Some(second)) = (index_of(key.first), index_of(key.second))
                else {
                    continue;
                };
                if !output.pair_compared(first, second) {
                    recheck.push(CandidatePair { first, second });
                }
            }
        }
        let mut candidates = output.pairs;
        if truncated {
            candidates.extend(recheck);
            candidates.sort_by_key(|pair| (pair.first, pair.second));
        }
        for candidate in candidates {
            let (handle_a, collider_a, kind_a, pose_a, layer_a) = &proxies[candidate.first];
            let (handle_b, collider_b, kind_b, pose_b, layer_b) = &proxies[candidate.second];
            if !layers_collide(&self.collision_matrix, *layer_a, *layer_b) {
                continue;
            }
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

    /// Учитывает обрезку broadphase: предупреждение пишется один раз при входе в деградацию и
    /// одно сообщение при выходе из неё. Счётчик попадает в `StepResult.broadphase_truncated`.
    fn note_broadphase(&mut self, output: &BroadPhaseOutput, bodies: usize) {
        match output.cut {
            Some(cut) => {
                self.broadphase_truncations += 1;
                if !self.broadphase_degraded {
                    self.broadphase_degraded = true;
                    log::warn!(
                        "physics broadphase stopped at sweep position {cut} of {bodies} bodies after {} comparisons; new contacts in the tail appear late",
                        output.comparisons
                    );
                }
            }
            None if self.broadphase_degraded => {
                self.broadphase_degraded = false;
                log::info!("physics broadphase restored: all body pairs are compared again");
            }
            None => {}
        }
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
        let Some(body) = self.bodies.get_mut(&handle) else {
            return;
        };
        if body.kind != BodyKind::Dynamic {
            return;
        }
        // Бодрствующий остров не содержит спящих соседей, поэтому достаточно сбросить таймер тела.
        if !body.sleeping {
            body.wake();
            return;
        }
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
            PhysicsCommand::SetCollider { shape, offset, .. } => {
                if !offset.is_finite() {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                let collider = ColliderDesc {
                    shape: shape.to_shape()?,
                    offset,
                    ..body.collider.clone()
                };
                let (inverse_mass, center_of_mass_local, inverse_inertia_local) =
                    mass_terms(body.kind, &collider, body.mass, body.lock_rotation)?;
                // Пара тел строит AABB на каждом шаге; отказ здесь иначе остановил бы весь шаг.
                let candidate_pose = Pose {
                    position: body.pose.position + body.pose.rotation * collider.offset,
                    rotation: body.pose.rotation,
                };
                collider.shape.aabb(candidate_pose)?;
                self.wake_neighbors(handle);
                self.wake_body(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.collider = collider;
                body.inverse_mass = inverse_mass;
                body.center_of_mass_local = center_of_mass_local;
                body.inverse_inertia_local = inverse_inertia_local;
                body.mark_collider_changed();
            }
            PhysicsCommand::SetTrigger { trigger, .. } => {
                if body.collider.trigger != trigger {
                    self.wake_neighbors(handle);
                    self.wake_body(handle);
                    let body = self.bodies.get_mut(&handle).expect("body was checked");
                    body.collider.trigger = trigger;
                    body.mark_collider_changed();
                }
            }
            PhysicsCommand::SetLayer { layer, .. } => {
                if layer as usize >= PHYSICS_LAYER_COUNT {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                if body.layer != layer {
                    self.wake_neighbors(handle);
                    self.wake_body(handle);
                    let body = self.bodies.get_mut(&handle).expect("body was checked");
                    body.layer = layer;
                    body.mark_collider_changed();
                }
            }
            PhysicsCommand::SetBodyProperties {
                kind,
                mass,
                linear_damping,
                angular_damping,
                gravity_scale,
                lock_rotation,
                ..
            } => {
                let finite = mass.is_finite()
                    && linear_damping.is_finite()
                    && angular_damping.is_finite()
                    && gravity_scale.is_finite();
                if !finite
                    || linear_damping < 0.0
                    || angular_damping < 0.0
                    || (body.character_controller && kind != BodyKind::Kinematic)
                    || (kind == BodyKind::Dynamic
                        && (mass <= 0.0 || matches!(body.collider.shape, Shape::TriMesh(_))))
                {
                    return Err(PhysicsWorldError::InvalidBody);
                }
                let (inverse_mass, center_of_mass_local, inverse_inertia_local) =
                    mass_terms(kind, &body.collider, mass, lock_rotation)?;
                self.wake_neighbors(handle);
                self.wake_body(handle);
                let body = self.bodies.get_mut(&handle).expect("body was checked");
                body.kind = kind;
                body.mass = mass;
                body.lock_rotation = lock_rotation;
                body.linear_damping = linear_damping;
                body.angular_damping = angular_damping;
                body.gravity_scale = gravity_scale;
                body.inverse_mass = inverse_mass;
                body.center_of_mass_local = center_of_mass_local;
                body.inverse_inertia_local = inverse_inertia_local;
                if kind != BodyKind::Kinematic {
                    body.kinematic_target = None;
                }
                if kind != BodyKind::Dynamic {
                    body.linear_velocity = Vec3::ZERO;
                    body.angular_velocity = Vec3::ZERO;
                }
                body.mark_collider_changed();
            }
        }
        Ok(())
    }

    /// Засыпание островами: остров — компонента контактов между динамическими телами. Он засыпает
    /// целиком, когда каждое тело успокоилось и ни одно не касается движущегося kinematic.
    fn update_sleep(&mut self, dt: f32, islands: &[crate::island::Island]) {
        if islands.is_empty() {
            return;
        }

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

        for island in islands {
            if !island.has_awake_body(|handle| {
                self.bodies.get(&handle).is_some_and(|body| !body.sleeping)
            }) {
                continue;
            }
            let ready = island.bodies.iter().all(|handle| {
                !held_awake.contains(handle)
                    && self
                        .bodies
                        .get(handle)
                        .is_some_and(|body| body.sleep_time >= SLEEP_DELAY_SECONDS)
            });
            if ready {
                for handle in &island.bodies {
                    if let Some(body) = self.bodies.get_mut(handle) {
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
            | Self::KinematicTarget { body, .. }
            | Self::SetCollider { body, .. }
            | Self::SetTrigger { body, .. }
            | Self::SetBodyProperties { body, .. }
            | Self::SetLayer { body, .. } => body,
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
    /// Collider and body-property commands the solver rejected; their bodies kept old values.
    pub skipped_commands: usize,
    /// Refresh'и пар в этом вызове, где broadphase остановился на пределе сравнений. Новые пары
    /// в хвосте sweep появляются позже, а прежние проверяются narrowphase заново.
    pub broadphase_truncated: usize,
}

/// Fixed-step accumulator shared by physics and script update scheduling.
pub struct FixedStepClock {
    fixed_hz: u32,
    fixed_dt: f32,
    max_substeps: usize,
    accumulator: f64,
}

impl FixedStepClock {
    pub fn new(fixed_hz: u32, max_substeps: usize) -> Result<Self, PhysicsWorldError> {
        if !(1..=1000).contains(&fixed_hz) || !(1..=64).contains(&max_substeps) {
            return Err(PhysicsWorldError::InvalidStepConfig);
        }
        Ok(Self {
            fixed_hz,
            fixed_dt: 1.0 / fixed_hz as f32,
            max_substeps,
            accumulator: 0.0,
        })
    }

    pub fn fixed_dt(&self) -> f32 {
        self.fixed_dt
    }

    pub fn fixed_hz(&self) -> u32 {
        self.fixed_hz
    }

    pub fn max_substeps(&self) -> usize {
        self.max_substeps
    }

    pub fn accumulator(&self) -> f64 {
        self.accumulator
    }

    /// Adds one frame delta and returns the same capped count used by [`StepDriver`].
    pub fn advance(&mut self, dt: f32) -> Result<usize, PhysicsWorldError> {
        validate_nonnegative_dt(dt)?;
        if dt == 0.0 {
            return Ok(0);
        }
        let accumulated = self.accumulator + dt as f64;
        let available = (accumulated / self.fixed_dt as f64).floor();
        let substeps = available.min(self.max_substeps as f64) as usize;
        self.accumulator = accumulated - substeps as f64 * self.fixed_dt as f64;
        if available >= self.max_substeps as f64 {
            self.accumulator = self.accumulator.min(self.fixed_dt as f64);
        }
        Ok(substeps)
    }
}

pub struct StepDriver {
    world: PhysicsWorld,
    clock: FixedStepClock,
}

impl StepDriver {
    pub fn new(
        world: PhysicsWorld,
        fixed_hz: u32,
        max_substeps: usize,
    ) -> Result<Self, PhysicsWorldError> {
        Ok(Self {
            world,
            clock: FixedStepClock::new(fixed_hz, max_substeps)?,
        })
    }

    pub fn world(&self) -> &PhysicsWorld {
        &self.world
    }

    pub fn world_mut(&mut self) -> &mut PhysicsWorld {
        &mut self.world
    }

    pub fn fixed_dt(&self) -> f32 {
        self.clock.fixed_dt()
    }

    pub fn fixed_hz(&self) -> u32 {
        self.clock.fixed_hz()
    }

    pub fn max_substeps(&self) -> usize {
        self.clock.max_substeps()
    }

    pub fn accumulator(&self) -> f64 {
        self.clock.accumulator()
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
            skipped_commands: 0,
            broadphase_truncated: 0,
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
        self.world.broadphase_truncations = 0;
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
        let mut skipped_commands = 0;
        for command in commands {
            let rejectable = matches!(
                command,
                PhysicsCommand::SetCollider { .. } | PhysicsCommand::SetBodyProperties { .. }
            );
            match self.world.apply_command(command, dt) {
                Ok(()) => {}
                // Тело могло быть удалено в этом же шаге (деактивация или уничтожение) раньше команды.
                Err(PhysicsWorldError::UnknownBody(_)) => {}
                // Отказ формы или свойств не должен останавливать симуляцию: тело остаётся прежним.
                Err(_) if rejectable => skipped_commands += 1,
                Err(error) => return Err(error),
            }
        }

        events.extend(self.world.refresh_pairs()?);
        let substeps = self.clock.advance(dt)?;
        for index in 0..substeps {
            events.extend(
                self.world
                    .step_substep(self.clock.fixed_dt(), substeps - index)?,
            );
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
            skipped_commands,
            broadphase_truncated: self.world.broadphase_truncations,
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
        BodyChange, BodyDesc, BodyHandle, BodyKind, ColliderDesc, FixedStepClock, PhysicsCommand,
        PhysicsEvent, PhysicsWorld, PhysicsWorldError, SLEEP_ANGULAR_SPEED, SLEEP_LINEAR_SPEED,
        StepDriver,
    };
    use crate::broad::MAX_BROADPHASE_COMPARISONS;
    use crate::{Hull, PhysicsMaterial, Pose, Shape, TriMesh};
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

    fn run_steps_at(driver: &mut StepDriver, hz: u32, seconds: f32) {
        for _ in 0..(seconds * hz as f32).round() as usize {
            driver.advance_step(1.0 / hz as f32, []).unwrap();
        }
    }

    fn count_events(events: &[PhysicsEvent], predicate: impl Fn(&PhysicsEvent) -> bool) -> usize {
        events.iter().filter(|event| predicate(event)).count()
    }

    #[test]
    fn shared_fixed_clock_matches_physics_substeps_and_clamps_backlog() {
        let mut clock = FixedStepClock::new(10, 2).unwrap();
        let mut driver = StepDriver::new(PhysicsWorld::default(), 10, 2).unwrap();

        for dt in [0.04, 0.07, 0.25, 0.0] {
            let script_substeps = clock.advance(dt).unwrap();
            let physics_substeps = driver.advance_step(dt, []).unwrap().substeps;
            assert_eq!(script_substeps, physics_substeps);
            assert_eq!(clock.accumulator(), driver.accumulator());
        }
        assert!((clock.accumulator() - 0.06).abs() < 1.0e-6);
        assert_eq!(clock.advance(0.0).unwrap(), 0);
        assert_eq!(clock.advance(-0.1), Err(PhysicsWorldError::InvalidTimeStep));
        assert_eq!(
            FixedStepClock::new(0, 2).err(),
            Some(PhysicsWorldError::InvalidStepConfig)
        );
    }

    #[test]
    fn zero_delta_does_not_consume_a_capped_fixed_step_backlog() {
        let mut clock = FixedStepClock::new(10, 2).unwrap();
        let mut driver = StepDriver::new(PhysicsWorld::default(), 10, 2).unwrap();

        assert_eq!(clock.advance(0.35).unwrap(), 2);
        assert_eq!(driver.advance_step(0.35, []).unwrap().substeps, 2);
        assert!((clock.accumulator() - 0.1).abs() < 1.0e-6);
        assert_eq!(clock.advance(0.0).unwrap(), 0);
        assert_eq!(driver.advance_step(0.0, []).unwrap().substeps, 0);
        assert!((clock.accumulator() - 0.1).abs() < 1.0e-6);
        assert_eq!(clock.advance(0.01).unwrap(), 1);
        assert_eq!(driver.advance_step(0.01, []).unwrap().substeps, 1);
        assert!((clock.accumulator() - 0.01).abs() < 1.0e-6);
        assert_eq!(clock.accumulator(), driver.accumulator());
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
        floor.collider.material.bounciness = 0.8;
        world.insert_body(floor).unwrap();
        let mut ball = BodyDesc::dynamic(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(0.0, 1.5, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        );
        ball.collider.material.bounciness = 0.8;
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
        ramp.collider.material = PhysicsMaterial::from_friction_restitution(friction, 0.0);
        world.insert_body(ramp).unwrap();
        let mut block = BodyDesc::dynamic(
            box_shape(0.4),
            Pose::new(normal * 0.6, rotation).unwrap(),
            1.0,
        );
        block.collider.material = PhysicsMaterial::from_friction_restitution(friction, 0.0);
        block.lock_rotation = true;
        let block = world.insert_body(block).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        run_steps(&mut driver, 120);
        driver.world().body_velocity(block).unwrap()
    }

    /// Рампа под 20° и блок 0.8 на ней; материалы задаются снаружи.
    fn ramp_block_world(
        hz: u32,
        ramp_material: PhysicsMaterial,
        block_material: PhysicsMaterial,
    ) -> (StepDriver, BodyHandle) {
        let rotation = Quat::from_rotation_z(-20.0_f32.to_radians());
        let mut world = PhysicsWorld::default();
        let mut ramp = BodyDesc::static_collider(
            Shape::cuboid(Vec3::new(5.0, 0.2, 3.0)).unwrap(),
            Pose::new(Vec3::ZERO, rotation).unwrap(),
        );
        ramp.collider.material = ramp_material;
        world.insert_body(ramp).unwrap();
        let mut block = BodyDesc::dynamic(
            box_shape(0.4),
            Pose::new(rotation * Vec3::Y * 0.6, rotation).unwrap(),
            1.0,
        );
        block.collider.material = block_material;
        block.lock_rotation = true;
        let block = world.insert_body(block).unwrap();
        (StepDriver::new(world, hz, 8).unwrap(), block)
    }

    fn friction_material(static_friction: f32, dynamic_friction: f32) -> PhysicsMaterial {
        PhysicsMaterial {
            static_friction,
            dynamic_friction,
            ..PhysicsMaterial::from_friction_restitution(0.0, 0.0)
        }
    }

    #[test]
    fn static_friction_holds_a_block_and_dynamic_friction_applies_once_it_slides() {
        let downslope = Quat::from_rotation_z(-20.0_f32.to_radians()) * Vec3::X;
        for hz in [60_u32, 30] {
            let mut slid = [0.0_f32; 2];
            for (index, dynamic) in [0.1_f32, 0.6].into_iter().enumerate() {
                let (mut driver, block) = ramp_block_world(
                    hz,
                    friction_material(0.6, dynamic),
                    friction_material(0.6, dynamic),
                );
                let start = driver.world().body_pose(block).unwrap().position;
                for _ in 0..hz {
                    driver.advance_step(1.0 / hz as f32, []).unwrap();
                }
                let rest = driver.world().body_pose(block).unwrap().position;
                assert!(
                    (rest - start).length() < 0.01,
                    "hz {hz}: drifted to {rest:?}"
                );

                driver
                    .advance_step(
                        1.0 / hz as f32,
                        [PhysicsCommand::AddImpulse {
                            body: block,
                            impulse: downslope,
                            point: None,
                        }],
                    )
                    .unwrap();
                let before = driver.world().body_pose(block).unwrap().position;
                run_steps_at(&mut driver, hz, 1.0);
                let after = driver.world().body_pose(block).unwrap().position;
                slid[index] = (after - before).dot(downslope);
            }
            assert!(
                slid[0] > slid[1] + 1.0,
                "hz {hz}: slide with dynamic 0.1 {} vs dynamic 0.6 {}",
                slid[0],
                slid[1]
            );
        }
    }

    #[test]
    fn combined_friction_between_identical_runs_is_bit_for_bit_repeatable() {
        let downslope = Quat::from_rotation_z(-20.0_f32.to_radians()) * Vec3::X;
        let mut finals = Vec::new();
        for _ in 0..2 {
            let (mut driver, block) =
                ramp_block_world(60, friction_material(0.7, 0.3), friction_material(0.5, 0.2));
            run_steps_at(&mut driver, 60, 0.5);
            driver
                .advance_step(
                    1.0 / 60.0,
                    [PhysicsCommand::AddImpulse {
                        body: block,
                        impulse: downslope * 0.8,
                        point: None,
                    }],
                )
                .unwrap();
            run_steps_at(&mut driver, 60, 1.0);
            finals.push(driver.world().body_pose(block).unwrap());
        }
        assert_eq!(finals[0], finals[1]);
    }

    #[test]
    fn collider_desc_new_keeps_half_friction_and_no_bounce() {
        let desc = ColliderDesc::new(box_shape(0.5));
        assert_eq!(desc.material.static_friction, 0.5);
        assert_eq!(desc.material.dynamic_friction, 0.5);
        assert_eq!(desc.material.bounciness, 0.0);
    }

    #[test]
    fn body_with_invalid_material_is_rejected_on_insert() {
        let mut world = PhysicsWorld::default();
        let mut desc = BodyDesc::dynamic(box_shape(0.5), Pose::IDENTITY, 1.0);
        desc.collider.material.bounciness = 1.5;
        assert_eq!(
            world.insert_body(desc),
            Err(PhysicsWorldError::InvalidMaterial)
        );
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
    fn zero_dynamic_friction_slide_accelerates_like_frictionless_block() {
        let downslope = Quat::from_rotation_z(-20.0_f32.to_radians()) * Vec3::X;
        let downslope_speeds = |material: PhysicsMaterial| -> Vec<f32> {
            let (mut driver, block) = ramp_block_world(60, material, material);
            (0..60)
                .map(|_| {
                    driver.advance_step(1.0 / 60.0, []).unwrap();
                    driver.world().body_velocity(block).unwrap().dot(downslope)
                })
                .collect()
        };
        let frictionless = downslope_speeds(friction_material(0.0, 0.0));
        let sliding = downslope_speeds(friction_material(0.3, 0.0));
        // Первые шаги блок удерживается статическим трением, пока скорость скольжения не
        // превысит порог; дальше прирост скорости за шаг должен совпадать с безфрикционным.
        for step in 30..60 {
            let expected = frictionless[step] - frictionless[step - 1];
            let actual = sliding[step] - sliding[step - 1];
            assert!(
                (actual - expected).abs() < 0.005,
                "step {step}: frictionless gain {expected} vs static 0.3 / dynamic 0 gain {actual}"
            );
        }
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
    fn zero_step_force_uses_frame_dt_and_zero_delta_preserves_capped_backlog() {
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
        let paused = driver.advance_step(0.0, []).unwrap();
        assert_eq!(paused.substeps, 0);
        assert!((driver.accumulator() - f64::from(driver.fixed_dt())).abs() < 1.0e-6);
        let resumed = driver.advance_step(0.01, []).unwrap();
        assert_eq!(resumed.substeps, 1);
        assert!((driver.accumulator() - 0.01).abs() < 1.0e-6);
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
            floor.collider.material = PhysicsMaterial::from_friction_restitution(1.0, 0.0);
            world.insert_body(floor).unwrap();
            let mut ball = BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(start_x, 0.49, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            );
            ball.collider.material = PhysicsMaterial::from_friction_restitution(1.0, 0.0);
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

    #[test]
    fn commands_for_removed_bodies_are_ignored_in_the_same_and_later_steps() {
        let mut driver = StepDriver::new(PhysicsWorld::new(Vec3::ZERO).unwrap(), 60, 8).unwrap();
        let ball = BodyHandle(9);
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: ball,
                    desc: ball_desc(0.0),
                }],
                [],
            )
            .unwrap();

        let result = driver.advance_step_with_changes(
            1.0 / 60.0,
            [BodyChange::Remove { handle: ball }],
            [
                PhysicsCommand::AddImpulse {
                    body: ball,
                    impulse: Vec3::Y,
                    point: None,
                },
                PhysicsCommand::SetVelocity {
                    body: ball,
                    velocity: Vec3::X,
                },
            ],
        );
        assert!(result.is_ok(), "{:?}", result.err());
        assert!(driver.world().body_pose(ball).is_none());

        // Команда, отправленная уже после удаления (стейл-хендл), тоже не роняет шаг.
        let later = driver.advance_step_with_changes(
            1.0 / 60.0,
            [],
            [PhysicsCommand::AddImpulse {
                body: ball,
                impulse: Vec3::Y,
                point: None,
            }],
        );
        assert!(later.is_ok(), "{:?}", later.err());
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
        assert!(driver.world().islands_for_step().is_empty());
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
    fn stationary_kinematic_inserted_against_a_sleeping_box_wakes_it() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let top = *boxes.last().unwrap();
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: BodyHandle(100),
                    desc: BodyDesc::new(
                        BodyKind::Kinematic,
                        Pose::new(Vec3::new(0.0, 2.5, 0.0), Quat::IDENTITY).unwrap(),
                        ColliderDesc::new(box_shape(0.5)),
                    ),
                }],
                [],
            )
            .unwrap();
        assert_eq!(driver.world().body_is_sleeping(top), Some(false));
    }

    #[test]
    fn stationary_kinematic_teleported_against_a_sleeping_box_wakes_it() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let top = *boxes.last().unwrap();
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: BodyHandle(100),
                    desc: BodyDesc::new(
                        BodyKind::Kinematic,
                        Pose::new(Vec3::new(10.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                        ColliderDesc::new(box_shape(0.5)),
                    ),
                }],
                [],
            )
            .unwrap();
        assert_eq!(driver.world().body_is_sleeping(top), Some(true));
        driver
            .advance_step(
                1.0 / 60.0,
                [PhysicsCommand::Teleport {
                    body: BodyHandle(100),
                    pose: Pose::new(Vec3::new(0.0, 2.5, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        assert_eq!(driver.world().body_is_sleeping(top), Some(false));
    }

    #[test]
    fn commands_on_an_awake_body_leave_an_unrelated_sleeping_stack_alone() {
        let (mut driver, boxes) = sleeping_stack_driver();
        let far = BodyHandle(100);
        driver
            .advance_step_with_changes(
                1.0 / 60.0,
                [BodyChange::Insert {
                    handle: far,
                    desc: BodyDesc::dynamic(
                        box_shape(0.5),
                        Pose::new(Vec3::new(10.0, 5.0, 0.0), Quat::IDENTITY).unwrap(),
                        1.0,
                    ),
                }],
                [],
            )
            .unwrap();
        run_steps(&mut driver, 10);
        assert_eq!(driver.world().body_is_sleeping(far), Some(false));

        driver
            .advance_step(
                1.0 / 60.0,
                [
                    PhysicsCommand::SetVelocity {
                        body: far,
                        velocity: Vec3::Y,
                    },
                    PhysicsCommand::AddImpulse {
                        body: far,
                        impulse: Vec3::X,
                        point: None,
                    },
                ],
            )
            .unwrap();
        assert_eq!(driver.world().body_is_sleeping(far), Some(false));
        for handle in &boxes {
            assert_eq!(driver.world().body_is_sleeping(*handle), Some(true));
            assert_eq!(driver.world().body_velocity(*handle), Some(Vec3::ZERO));
        }
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

    #[test]
    fn box_on_hull_floor_rests_without_jitter_or_drift() {
        let mut world = PhysicsWorld::default();
        let points: Vec<Vec3> = (0..8)
            .map(|index| {
                Vec3::new(
                    if index & 1 == 0 { -2.0 } else { 2.0 },
                    if index & 2 == 0 { -2.0 } else { 2.0 },
                    if index & 4 == 0 { -2.0 } else { 2.0 },
                )
            })
            .collect();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::ConvexHull(Arc::new(Hull::from_points(&points).unwrap())),
                Pose::new(Vec3::new(0.0, -2.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let body = world
            .insert_body(BodyDesc::dynamic(
                box_shape(0.5),
                Pose::new(Vec3::new(0.0, 0.7, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let mut peak_speed = 0.0_f32;
        let mut peak_drift = 0.0_f32;
        for frame in 0..=60 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            if frame < 30 {
                continue;
            }
            if driver.world().body_is_sleeping(body) == Some(false) {
                peak_speed = peak_speed.max(driver.world().body_velocity(body).unwrap().length());
            }
            let pose = driver.world().body_pose(body).unwrap();
            peak_drift = peak_drift.max(pose.position.x.abs().max(pose.position.z.abs()));
        }
        assert!(peak_speed < SLEEP_LINEAR_SPEED, "peak speed {peak_speed}");
        assert!(peak_drift < 1.0e-3, "horizontal drift {peak_drift}");
    }

    #[test]
    fn capsule_lying_on_mesh_floor_rests_on_its_two_ends_without_rolling() {
        let mut world = PhysicsWorld::default();
        world.insert_body(diagonal_mesh_floor_desc()).unwrap();
        let radius = 0.25;
        let capsule = world
            .insert_body(BodyDesc::dynamic(
                Shape::capsule(radius, 1.0).unwrap(),
                Pose::new(
                    Vec3::new(0.0, radius - 0.002, 0.0),
                    Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
                )
                .unwrap(),
                1.0,
            ))
            .unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let mut peak_speed = 0.0_f32;
        let mut peak_spin = 0.0_f32;
        let mut peak_drift = 0.0_f32;
        for frame in 0..=60 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
            if frame < 30 {
                continue;
            }
            if driver.world().body_is_sleeping(capsule) == Some(false) {
                peak_speed =
                    peak_speed.max(driver.world().body_velocity(capsule).unwrap().length());
                peak_spin = peak_spin.max(
                    driver
                        .world()
                        .body_angular_velocity(capsule)
                        .unwrap()
                        .length(),
                );
            }
            let pose = driver.world().body_pose(capsule).unwrap();
            peak_drift = peak_drift.max(pose.position.x.abs().max(pose.position.z.abs()));
        }
        // Контакты лежат на концах отрезка капсулы: x = ±0.5 при половине длины 0.5.
        let points: Vec<Vec3> = driver
            .world()
            .active_pairs
            .values()
            .filter_map(|pair| pair.manifold.as_ref())
            .flat_map(|manifold| manifold.contacts.iter().map(|contact| contact.point_a))
            .collect();
        assert_eq!(points.len(), 2, "capsule contact points {points:?}");
        for end_x in [-0.5, 0.5] {
            assert!(
                points
                    .iter()
                    .any(|point| (point.x - end_x).abs() < 0.01 && point.z.abs() < 0.01),
                "no contact at the capsule end x = {end_x}: {points:?}"
            );
        }
        assert!(peak_speed < SLEEP_LINEAR_SPEED, "peak speed {peak_speed}");
        assert!(peak_spin < SLEEP_ANGULAR_SPEED, "peak spin {peak_spin}");
        assert!(peak_drift < 1.0e-3, "horizontal drift {peak_drift}");
    }

    /// Перехват записей лога одного потока: параллельные тесты не попадают в счёт.
    struct Capture;

    static RECORDS: std::sync::Mutex<Vec<(std::thread::ThreadId, log::Level, String)>> =
        std::sync::Mutex::new(Vec::new());
    static CAPTURE: Capture = Capture;

    impl log::Log for Capture {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                RECORDS.lock().unwrap().push((
                    std::thread::current().id(),
                    record.level(),
                    record.args().to_string(),
                ));
            }
        }

        fn flush(&self) {}
    }

    /// Логгер ставится до первой записи теста: записи, сделанные без него, не перехватываются.
    fn install_capture() {
        let _ = log::set_logger(&CAPTURE);
        log::set_max_level(log::LevelFilter::Info);
    }

    fn records_on_this_thread(level: log::Level) -> Vec<String> {
        let current = std::thread::current().id();
        RECORDS
            .lock()
            .unwrap()
            .iter()
            .filter(|(thread, record_level, _)| *thread == current && *record_level == level)
            .map(|(_, _, text)| text.clone())
            .collect()
    }

    /// Динамические боксы в ряд по X с перекрытием: все пары пересекаются. Sweep по X обрабатывает
    /// тела по порядку, поэтому при лимите 3 последнее тело оказывается в хвосте.
    fn overlapping_row(count: usize) -> (PhysicsWorld, Vec<BodyHandle>) {
        let mut world = PhysicsWorld::default();
        let mut handles = Vec::new();
        for index in 0..count {
            let pose = Pose::new(Vec3::new(index as f32 * 0.2, 5.0, 0.0), Quat::IDENTITY).unwrap();
            handles.push(
                world
                    .insert_body(BodyDesc::dynamic(box_shape(0.5), pose, 1.0))
                    .unwrap(),
            );
        }
        (world, handles)
    }

    #[test]
    fn truncated_tail_pairs_are_rechecked_and_kept_while_they_still_overlap() {
        let (mut world, _) = overlapping_row(4);
        world.refresh_pairs().unwrap();
        assert_eq!(world.active_pair_count(), 6);

        world.broad.comparison_limit = 3;
        for _ in 0..2 {
            assert!(world.refresh_pairs().unwrap().is_empty());
        }
        assert_eq!(world.active_pair_count(), 6);
    }

    #[test]
    fn diverged_tail_pairs_exit_once_and_stay_gone_under_truncation() {
        let (mut world, handles) = overlapping_row(4);
        world.refresh_pairs().unwrap();
        world.broad.comparison_limit = 3;
        let tail = world.bodies.get_mut(&handles[3]).unwrap();
        tail.pose.position.y = 7.0;
        tail.mark_pose_changed();

        let events = world.refresh_pairs().unwrap();
        let exits: Vec<_> = events
            .iter()
            .filter(|event| matches!(event, PhysicsEvent::CollisionExit { .. }))
            .collect();
        assert_eq!(
            exits.len(),
            3,
            "каждая разошедшаяся пара даёт ровно один Exit"
        );
        assert!(exits.iter().all(|event| matches!(
            event,
            PhysicsEvent::CollisionExit { second, .. } if *second == handles[3]
        )));
        assert!(world.refresh_pairs().unwrap().is_empty());
        assert_eq!(world.active_pair_count(), 3);
    }

    #[test]
    fn sleeping_body_pair_in_the_truncated_tail_is_kept_without_exit() {
        let (mut world, handles) = overlapping_row(4);
        world.refresh_pairs().unwrap();
        world.bodies.get_mut(&handles[3]).unwrap().sleeping = true;
        world.broad.comparison_limit = 3;
        assert!(world.refresh_pairs().unwrap().is_empty());
        assert_eq!(world.active_pair_count(), 6);
    }

    #[test]
    fn dense_cluster_step_completes_and_reports_truncated_refreshes() {
        let (mut world, _) = overlapping_row(6);
        world.broad.comparison_limit = 3;
        let mut driver = StepDriver::new(world, 60, 4).unwrap();
        let result = driver
            .advance_step(1.0 / 60.0, Vec::<PhysicsCommand>::new())
            .unwrap();
        assert_eq!(result.substeps, 1);
        // Предварительный refresh и refresh одного подшага.
        assert_eq!(result.broadphase_truncated, 2);
    }

    #[test]
    fn degradation_warns_once_on_entry_and_logs_once_on_recovery() {
        install_capture();
        let (mut world, _) = overlapping_row(4);
        world.broad.comparison_limit = 3;
        world.refresh_pairs().unwrap();
        world.refresh_pairs().unwrap();
        assert_eq!(records_on_this_thread(log::Level::Warn).len(), 1);
        assert!(records_on_this_thread(log::Level::Info).is_empty());

        world.broad.comparison_limit = MAX_BROADPHASE_COMPARISONS;
        world.refresh_pairs().unwrap();
        world.refresh_pairs().unwrap();
        assert_eq!(records_on_this_thread(log::Level::Warn).len(), 1);
        assert_eq!(records_on_this_thread(log::Level::Info).len(), 1);
    }
}

#[cfg(test)]
mod script_physics_tests {
    use super::*;
    use crate::query::{QueryFilter, QueryOverrides};
    use crate::ray::Ray;

    fn ball_at(x: f32, radius: f32, layer: u32) -> BodyDesc {
        let mut desc = BodyDesc::dynamic(
            Shape::sphere(radius).unwrap(),
            Pose::new(Vec3::new(x, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        );
        desc.layer = layer;
        desc
    }

    fn wall_at(x: f32, radius: f32, layer: u32) -> BodyDesc {
        let mut desc = BodyDesc::static_collider(
            Shape::sphere(radius).unwrap(),
            Pose::new(Vec3::new(x, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        desc.layer = layer;
        desc
    }

    fn count(events: &[PhysicsEvent], matches: impl Fn(&PhysicsEvent) -> bool) -> usize {
        events.iter().filter(|event| matches(event)).count()
    }

    fn is_pair(event: &PhysicsEvent, a: BodyHandle, b: BodyHandle) -> bool {
        let pair = |first: &BodyHandle, second: &BodyHandle| {
            (*first == a && *second == b) || (*first == b && *second == a)
        };
        match event {
            PhysicsEvent::CollisionEnter { first, second, .. }
            | PhysicsEvent::CollisionExit { first, second }
            | PhysicsEvent::TriggerEnter { first, second }
            | PhysicsEvent::TriggerExit { first, second } => pair(first, second),
        }
    }

    fn collision_enter(event: &PhysicsEvent) -> bool {
        matches!(event, PhysicsEvent::CollisionEnter { .. })
    }

    fn collision_exit(event: &PhysicsEvent) -> bool {
        matches!(event, PhysicsEvent::CollisionExit { .. })
    }

    fn trigger_enter(event: &PhysicsEvent) -> bool {
        matches!(event, PhysicsEvent::TriggerEnter { .. })
    }

    fn trigger_exit(event: &PhysicsEvent) -> bool {
        matches!(event, PhysicsEvent::TriggerExit { .. })
    }

    fn dynamic_props(kind: BodyKind, mass: f32) -> PhysicsCommand {
        PhysicsCommand::SetBodyProperties {
            body: BodyHandle(1),
            kind,
            mass,
            linear_damping: 0.0,
            angular_damping: 0.0,
            gravity_scale: 1.0,
            lock_rotation: false,
        }
    }

    #[test]
    fn trigger_toggle_exits_collision_and_enters_trigger_pair_and_back() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let wall = world.insert_body(wall_at(0.0, 1.0, 0)).unwrap();
        let ball = world.insert_body(ball_at(1.2, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        let first = driver.advance_step(0.0, []).unwrap();
        assert_eq!(count(&first.events, collision_enter), 1);

        let to_trigger = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetTrigger {
                    body: wall,
                    trigger: true,
                }],
            )
            .unwrap();
        assert_eq!(count(&to_trigger.events, collision_exit), 1);
        assert_eq!(count(&to_trigger.events, trigger_enter), 1);
        assert!(
            to_trigger
                .events
                .iter()
                .all(|event| is_pair(event, wall, ball))
        );

        let back = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetTrigger {
                    body: wall,
                    trigger: false,
                }],
            )
            .unwrap();
        assert_eq!(count(&back.events, trigger_exit), 1);
        assert_eq!(count(&back.events, collision_enter), 1);
    }

    #[test]
    fn collider_resize_enters_and_exits_contact_pair_on_next_refresh() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        world.insert_body(wall_at(0.0, 1.0, 0)).unwrap();
        let ball = world.insert_body(ball_at(2.0, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        assert!(driver.advance_step(0.0, []).unwrap().events.is_empty());

        let grow = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetCollider {
                    body: ball,
                    shape: PrimitiveShape::Sphere { radius: 1.5 },
                    offset: Vec3::ZERO,
                }],
            )
            .unwrap();
        assert_eq!(count(&grow.events, collision_enter), 1);

        let shrink = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetCollider {
                    body: ball,
                    shape: PrimitiveShape::Sphere { radius: 0.5 },
                    offset: Vec3::ZERO,
                }],
            )
            .unwrap();
        assert_eq!(count(&shrink.events, collision_exit), 1);
    }

    #[test]
    fn static_collider_resize_refreshes_pairs_with_dynamic_bodies() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let wall = world.insert_body(wall_at(0.0, 1.0, 0)).unwrap();
        world.insert_body(ball_at(2.0, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        assert!(driver.advance_step(0.0, []).unwrap().events.is_empty());

        let grown = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetCollider {
                    body: wall,
                    shape: PrimitiveShape::Sphere { radius: 2.0 },
                    offset: Vec3::ZERO,
                }],
            )
            .unwrap();
        assert_eq!(count(&grown.events, collision_enter), 1);
    }

    #[test]
    fn degenerate_collider_command_is_skipped_and_keeps_body() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let ball = world.insert_body(ball_at(0.0, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        let before = driver.world().bodies[&ball].collider.clone();

        driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetCollider {
                    body: ball,
                    shape: PrimitiveShape::Box {
                        half_extents: Vec3::splat(1.0e-20),
                    },
                    offset: Vec3::ZERO,
                }],
            )
            .unwrap();

        assert_eq!(driver.world().bodies[&ball].collider, before);
        assert!(driver.advance_step(1.0 / 60.0, []).is_ok());
    }

    #[test]
    fn huge_collider_command_does_not_fail_the_step() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let ball = world.insert_body(ball_at(0.0, 0.5, 0)).unwrap();
        world.insert_body(wall_at(3.0, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetCollider {
                    body: ball,
                    shape: PrimitiveShape::Box {
                        half_extents: Vec3::splat(1.0e30),
                    },
                    offset: Vec3::ZERO,
                }],
            )
            .unwrap();
        assert!(driver.advance_step(1.0 / 60.0, []).is_ok());
    }

    #[test]
    fn subnormal_mass_command_is_skipped_and_keeps_body_mass() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let ball = world.insert_body(ball_at(0.0, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetBodyProperties {
                    body: ball,
                    kind: BodyKind::Dynamic,
                    mass: 1.0e-39,
                    linear_damping: 0.0,
                    angular_damping: 0.0,
                    gravity_scale: 1.0,
                    lock_rotation: false,
                }],
            )
            .unwrap();

        assert_eq!(driver.world().bodies[&ball].mass, 1.0);
        assert!(driver.advance_step(1.0 / 60.0, []).is_ok());
    }

    #[test]
    fn collision_matrix_disables_pairs_and_set_layer_restores_them() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut masks = [u32::MAX; PHYSICS_LAYER_COUNT];
        masks[0] = 1 << 0;
        masks[1] = 1 << 1;
        world.set_collision_matrix(masks);
        world.insert_body(ball_at(0.0, 0.5, 0)).unwrap();
        let other = world.insert_body(ball_at(0.5, 0.5, 1)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        assert!(driver.advance_step(0.0, []).unwrap().events.is_empty());

        let moved = driver
            .advance_step(
                0.0,
                [PhysicsCommand::SetLayer {
                    body: other,
                    layer: 0,
                }],
            )
            .unwrap();
        assert_eq!(count(&moved.events, collision_enter), 1);
    }

    #[test]
    fn collision_matrix_also_filters_trigger_pairs() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut masks = [u32::MAX; PHYSICS_LAYER_COUNT];
        masks[0] = 1 << 0;
        masks[1] = 1 << 1;
        world.set_collision_matrix(masks);
        let mut trigger = wall_at(0.0, 1.0, 1);
        trigger.collider.trigger = true;
        world.insert_body(trigger).unwrap();
        world.insert_body(ball_at(0.5, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        let result = driver.advance_step(0.0, []).unwrap();
        assert_eq!(count(&result.events, trigger_enter), 0);
        assert!(
            driver
                .world()
                .query_snapshot()
                .unwrap()
                .trigger_pairs()
                .is_empty()
        );
    }

    #[test]
    fn snapshot_lists_active_trigger_pairs_until_they_separate() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut trigger = wall_at(0.0, 1.0, 0);
        trigger.collider.trigger = true;
        let trigger = world.insert_body(trigger).unwrap();
        let ball = world.insert_body(ball_at(0.5, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        driver.advance_step(0.0, []).unwrap();

        let inside = driver.world().query_snapshot().unwrap();
        let pairs = inside.trigger_pairs();
        assert_eq!(pairs.len(), 1);
        assert!(is_pair(
            &PhysicsEvent::TriggerEnter {
                first: pairs[0].0,
                second: pairs[0].1
            },
            trigger,
            ball
        ));

        let moved = driver
            .advance_step(
                0.0,
                [PhysicsCommand::Teleport {
                    body: ball,
                    pose: Pose::new(Vec3::new(5.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                }],
            )
            .unwrap();
        assert_eq!(count(&moved.events, trigger_exit), 1);
        assert!(
            driver
                .world()
                .query_snapshot()
                .unwrap()
                .trigger_pairs()
                .is_empty()
        );
    }

    #[test]
    fn overlap_respects_layer_mask_triggers_and_ignore_lists() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let near = world.insert_body(ball_at(0.0, 0.5, 0)).unwrap();
        let mut trigger = wall_at(0.0, 0.5, 0);
        trigger.collider.trigger = true;
        let trigger = world.insert_body(trigger).unwrap();
        let far_layer = world.insert_body(wall_at(0.0, 0.5, 3)).unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let sphere = Shape::sphere(0.6).unwrap();
        let pose = Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap();
        let overlap = |filter: QueryFilter, excluded: &[BodyHandle]| {
            snapshot
                .overlap(
                    &sphere,
                    pose,
                    filter,
                    QueryOverrides {
                        excluded,
                        enabled: &[],
                    },
                )
                .unwrap()
        };

        assert_eq!(overlap(QueryFilter::default(), &[]), vec![near, far_layer]);
        let with_triggers = QueryFilter {
            include_triggers: true,
            ..QueryFilter::default()
        };
        assert_eq!(overlap(with_triggers, &[]), vec![near, trigger, far_layer]);
        let without_layer_three = QueryFilter {
            layer_mask: !(1 << 3),
            ..QueryFilter::default()
        };
        assert_eq!(overlap(without_layer_three, &[]), vec![near]);
        let ignore_near = QueryFilter {
            ignore: Some(near),
            ..QueryFilter::default()
        };
        assert_eq!(overlap(ignore_near, &[]), vec![far_layer]);
        assert_eq!(overlap(QueryFilter::default(), &[far_layer]), vec![near]);
    }

    #[test]
    fn raycast_all_returns_every_hit_in_distance_order_and_honours_layers() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let first = world.insert_body(wall_at(0.0, 0.5, 0)).unwrap();
        let second = world.insert_body(wall_at(3.0, 0.5, 3)).unwrap();
        let third = world.insert_body(wall_at(6.0, 0.5, 0)).unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let ray = Ray {
            origin: Vec3::new(-5.0, 0.0, 0.0),
            direction: Vec3::X,
            max_distance: 20.0,
        };
        let hits = |filter: QueryFilter| {
            snapshot
                .raycast_all(&ray, filter, QueryOverrides::default())
                .unwrap()
                .iter()
                .map(|hit| hit.body)
                .collect::<Vec<_>>()
        };

        assert_eq!(hits(QueryFilter::default()), vec![first, second, third]);
        let no_layer_three = QueryFilter {
            layer_mask: !(1 << 3),
            ..QueryFilter::default()
        };
        assert_eq!(hits(no_layer_three), vec![first, third]);
        let distances: Vec<f32> = snapshot
            .raycast_all(&ray, QueryFilter::default(), QueryOverrides::default())
            .unwrap()
            .iter()
            .map(|hit| hit.distance)
            .collect();
        assert_eq!(distances, vec![4.5, 7.5, 10.5]);
    }

    #[test]
    fn set_body_properties_changes_kind_and_mass_and_rejects_invalid_dynamic_mass() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let ball = world.insert_body(ball_at(0.0, 0.5, 0)).unwrap();
        let mut driver = StepDriver::new(world, 60, 8).unwrap();

        driver
            .advance_step(0.0, [dynamic_props(BodyKind::Dynamic, 4.0)])
            .unwrap();
        let state = driver
            .world()
            .query_snapshot()
            .unwrap()
            .body_state(ball)
            .unwrap();
        assert_eq!(state.body_kind, BodyKind::Dynamic);
        assert!((state.mass - 4.0).abs() < 1.0e-5);

        driver
            .advance_step(0.0, [dynamic_props(BodyKind::Static, 4.0)])
            .unwrap();
        let state = driver
            .world()
            .query_snapshot()
            .unwrap()
            .body_state(ball)
            .unwrap();
        assert_eq!(state.body_kind, BodyKind::Static);
        assert_eq!(state.mass, 0.0);

        let rejected = driver
            .advance_step(0.0, [dynamic_props(BodyKind::Dynamic, 0.0)])
            .unwrap();
        assert_eq!(rejected.skipped_commands, 1);
        let state = driver
            .world()
            .query_snapshot()
            .unwrap()
            .body_state(ball)
            .unwrap();
        assert_eq!(state.body_kind, BodyKind::Static);
    }
}

#[cfg(test)]
mod parallel_solver_tests {
    use super::*;
    use crate::material::PhysicsMaterial;
    use crate::query::QueryFilter;
    use crate::ray::Ray;
    use glam::Quat;
    use std::num::NonZeroUsize;

    const TEST_PARALLEL_MIN_CONSTRAINTS: usize = crate::solver::PARALLEL_MIN_CONSTRAINTS;

    fn configure(world: &mut PhysicsWorld, threads: usize, threshold: usize) {
        world.set_solver_threads(NonZeroUsize::new(threads).unwrap());
        world.test_parallel_min_constraints = threshold;
    }

    fn bits3(value: Vec3) -> [u32; 3] {
        value.to_array().map(f32::to_bits)
    }

    fn assert_pose_bits(first: Pose, second: Pose) {
        assert_eq!(bits3(first.position), bits3(second.position));
        assert_eq!(
            first.rotation.to_array().map(f32::to_bits),
            second.rotation.to_array().map(f32::to_bits)
        );
    }

    fn assert_events_bits(first: &[PhysicsEvent], second: &[PhysicsEvent]) {
        assert_eq!(first.len(), second.len());
        for (first, second) in first.iter().zip(second) {
            match (first, second) {
                (
                    PhysicsEvent::CollisionEnter {
                        first: first_a,
                        second: first_b,
                        point: first_point,
                        normal: first_normal,
                    },
                    PhysicsEvent::CollisionEnter {
                        first: second_a,
                        second: second_b,
                        point: second_point,
                        normal: second_normal,
                    },
                ) => {
                    assert_eq!((first_a, first_b), (second_a, second_b));
                    assert_eq!(bits3(*first_point), bits3(*second_point));
                    assert_eq!(bits3(*first_normal), bits3(*second_normal));
                }
                (
                    PhysicsEvent::CollisionExit {
                        first: first_a,
                        second: first_b,
                    },
                    PhysicsEvent::CollisionExit {
                        first: second_a,
                        second: second_b,
                    },
                )
                | (
                    PhysicsEvent::TriggerEnter {
                        first: first_a,
                        second: first_b,
                    },
                    PhysicsEvent::TriggerEnter {
                        first: second_a,
                        second: second_b,
                    },
                )
                | (
                    PhysicsEvent::TriggerExit {
                        first: first_a,
                        second: first_b,
                    },
                    PhysicsEvent::TriggerExit {
                        first: second_a,
                        second: second_b,
                    },
                ) => assert_eq!((first_a, first_b), (second_a, second_b)),
                _ => panic!("event kinds differ: {first:?} != {second:?}"),
            }
        }
    }

    fn assert_step_bits(first: &StepResult, second: &StepResult) {
        assert_eq!(first.substeps, second.substeps);
        assert_eq!(first.skipped_pairs, second.skipped_pairs);
        assert_eq!(first.skipped_commands, second.skipped_commands);
        assert_eq!(first.broadphase_truncated, second.broadphase_truncated);
        assert_eq!(first.poses.len(), second.poses.len());
        for ((first_handle, first_pose), (second_handle, second_pose)) in
            first.poses.iter().zip(&second.poses)
        {
            assert_eq!(first_handle, second_handle);
            assert_pose_bits(*first_pose, *second_pose);
        }
        assert_events_bits(&first.events, &second.events);
    }

    fn assert_world_bits(first: &PhysicsWorld, second: &PhysicsWorld) {
        assert_eq!(
            first.bodies.keys().collect::<Vec<_>>(),
            second.bodies.keys().collect::<Vec<_>>()
        );
        for (handle, first_body) in &first.bodies {
            let second_body = &second.bodies[handle];
            assert_pose_bits(first_body.pose, second_body.pose);
            assert_eq!(
                bits3(first_body.linear_velocity),
                bits3(second_body.linear_velocity)
            );
            assert_eq!(
                bits3(first_body.angular_velocity),
                bits3(second_body.angular_velocity)
            );
            assert_eq!(first_body.sleeping, second_body.sleeping);
            assert_eq!(
                first_body.sleep_time.to_bits(),
                second_body.sleep_time.to_bits()
            );
        }
        assert_eq!(
            first.active_pairs.keys().collect::<Vec<_>>(),
            second.active_pairs.keys().collect::<Vec<_>>()
        );
        for (key, first_pair) in &first.active_pairs {
            let second_pair = &second.active_pairs[key];
            assert_eq!(first_pair.kind, second_pair.kind);
            match (&first_pair.manifold, &second_pair.manifold) {
                (Some(first), Some(second)) => {
                    assert_eq!(bits3(first.normal), bits3(second.normal));
                    assert_eq!(first.contacts.len(), second.contacts.len());
                    for (first, second) in first.contacts.iter().zip(&second.contacts) {
                        assert_eq!(bits3(first.point_a), bits3(second.point_a));
                        assert_eq!(bits3(first.point_b), bits3(second.point_b));
                        assert_eq!(bits3(first.position), bits3(second.position));
                        assert_eq!(first.penetration.to_bits(), second.penetration.to_bits());
                        assert_eq!(first.feature_id, second.feature_id);
                        assert_eq!(
                            first.normal_impulse.to_bits(),
                            second.normal_impulse.to_bits()
                        );
                        assert_eq!(bits3(first.tangent_impulse), bits3(second.tangent_impulse));
                        assert_eq!(first.sliding, second.sliding);
                    }
                }
                (None, None) => {}
                _ => panic!("manifold presence differs for {key:?}"),
            }
        }
    }

    fn assert_snapshots_bits(first: &PhysicsWorld, second: &PhysicsWorld) {
        let handles: Vec<_> = first.bodies.keys().copied().collect();
        let first = first.query_snapshot().unwrap();
        let second = second.query_snapshot().unwrap();
        for handle in handles {
            let first_state = first.body_state(handle).unwrap();
            let second_state = second.body_state(handle).unwrap();
            assert_pose_bits(first_state.pose, second_state.pose);
            assert_eq!(first_state.enabled, second_state.enabled);
            assert_eq!(first_state.trigger, second_state.trigger);
            assert_eq!(first_state.body_kind, second_state.body_kind);
            assert_eq!(
                bits3(first_state.body_velocity),
                bits3(second_state.body_velocity)
            );
            assert_eq!(first_state.mass.to_bits(), second_state.mass.to_bits());
        }
        let ray = Ray::new(Vec3::new(0.0, 20.0, 0.0), -Vec3::Y, 50.0).unwrap();
        let first_hit = first.raycast(&ray, QueryFilter::default()).unwrap();
        let second_hit = second.raycast(&ray, QueryFilter::default()).unwrap();
        match (first_hit, second_hit) {
            (Some(first), Some(second)) => {
                assert_eq!(first.body, second.body);
                assert_eq!(bits3(first.point), bits3(second.point));
                assert_eq!(bits3(first.normal), bits3(second.normal));
                assert_eq!(first.distance.to_bits(), second.distance.to_bits());
                assert_eq!(first.fraction.to_bits(), second.fraction.to_bits());
            }
            (None, None) => {}
            _ => panic!("query snapshot raycast results differ"),
        }
    }

    fn add_floor(world: &mut PhysicsWorld, half_extents: Vec3) {
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(half_extents).unwrap(),
                Pose::new(Vec3::new(0.0, -half_extents.y, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
    }

    fn grid_driver(threads: usize, threshold: usize) -> StepDriver {
        let mut world = PhysicsWorld::default();
        add_floor(&mut world, Vec3::new(8.0, 0.5, 8.0));
        for row in 0..10 {
            for column in 0..10 {
                world
                    .insert_body(BodyDesc::dynamic(
                        Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                        Pose::new(
                            Vec3::new(column as f32 * 1.2 - 5.4, 0.45, row as f32 * 1.2 - 5.4),
                            Quat::IDENTITY,
                        )
                        .unwrap(),
                        1.0,
                    ))
                    .unwrap();
            }
        }
        configure(&mut world, threads, threshold);
        StepDriver::new(world, 60, 8).unwrap()
    }

    fn composite_driver(
        threads: usize,
        threshold: usize,
    ) -> (StepDriver, BodyHandle, Vec<BodyHandle>) {
        let mut world = PhysicsWorld::default();
        add_floor(&mut world, Vec3::new(24.0, 0.5, 24.0));
        let mut dynamics = Vec::new();
        for (x, y, z) in [
            (-8.0, 0.5, 0.0),
            (-8.0, 1.5, 0.0),
            (8.0, 0.5, 0.0),
            (8.0, 1.5, 0.0),
            (0.8, 0.5, 4.0),
        ] {
            let mut collider = ColliderDesc::new(Shape::cuboid(Vec3::splat(0.5)).unwrap());
            collider.material = PhysicsMaterial {
                static_friction: 0.7,
                dynamic_friction: 0.2,
                bounciness: 0.35,
                ..PhysicsMaterial::from_friction_restitution(0.5, 0.0)
            };
            dynamics.push(
                world
                    .insert_body(BodyDesc::new(
                        BodyKind::Dynamic,
                        Pose::new(Vec3::new(x, y, z), Quat::IDENTITY).unwrap(),
                        collider,
                    ))
                    .unwrap(),
            );
        }
        let pusher = world
            .insert_body(BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::new(0.0, 0.5, 4.0), Quat::IDENTITY).unwrap(),
                ColliderDesc::new(Shape::cuboid(Vec3::splat(0.5)).unwrap()),
            ))
            .unwrap();
        let mut character = BodyDesc::new(
            BodyKind::Kinematic,
            Pose::new(Vec3::new(-2.0, 0.8, 5.0), Quat::IDENTITY).unwrap(),
            ColliderDesc::new(Shape::capsule(0.4, 1.0).unwrap()),
        );
        character.character_controller = true;
        world.insert_body(character).unwrap();
        dynamics.push(
            world
                .insert_body(BodyDesc::dynamic(
                    Shape::sphere(0.45).unwrap(),
                    Pose::new(Vec3::new(-1.55, 0.8, 5.0), Quat::IDENTITY).unwrap(),
                    1.0,
                ))
                .unwrap(),
        );
        configure(&mut world, threads, threshold);
        (StepDriver::new(world, 60, 8).unwrap(), pusher, dynamics)
    }

    fn truncated_driver(threads: usize, threshold: usize) -> (StepDriver, PairKey) {
        let mut world = PhysicsWorld::default();
        add_floor(&mut world, Vec3::new(10.0, 0.5, 5.0));
        let mut boxes = Vec::new();
        for x in [-4.0, -3.2, 4.0, 4.8] {
            boxes.push(
                world
                    .insert_body(BodyDesc::dynamic(
                        Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                        Pose::new(Vec3::new(x, 0.45, 0.0), Quat::IDENTITY).unwrap(),
                        1.0,
                    ))
                    .unwrap(),
            );
        }
        world.refresh_pairs().unwrap();
        let tail = PairKey::new(boxes[2].0, boxes[3].0);
        assert!(world.active_pairs.contains_key(&tail));
        world.broad.comparison_limit = 1;
        configure(&mut world, threads, threshold);
        (StepDriver::new(world, 60, 8).unwrap(), tail)
    }

    fn compare_steps(
        baseline: &mut StepDriver,
        candidate: &mut StepDriver,
        steps: usize,
        pusher: Option<BodyHandle>,
        initial_impulses: &[BodyHandle],
    ) {
        for frame in 0..steps {
            let mut commands = Vec::new();
            if frame == 0 {
                for (index, handle) in initial_impulses.iter().enumerate() {
                    commands.push(PhysicsCommand::AddImpulse {
                        body: *handle,
                        impulse: if index == 0 {
                            Vec3::new(0.0, 0.0, 1.5)
                        } else {
                            Vec3::new(0.0, -1.5, 0.0)
                        },
                        point: None,
                    });
                }
            }
            if let Some(pusher) = pusher {
                commands.push(PhysicsCommand::KinematicTarget {
                    body: pusher,
                    pose: Pose::new(Vec3::new(frame as f32 * 0.002, 0.5, 4.0), Quat::IDENTITY)
                        .unwrap(),
                });
            }
            let baseline_result = baseline.advance_step(1.0 / 60.0, commands.clone()).unwrap();
            let candidate_result = candidate.advance_step(1.0 / 60.0, commands).unwrap();
            assert_step_bits(&baseline_result, &candidate_result);
            assert_world_bits(baseline.world(), candidate.world());
        }
        assert_snapshots_bits(baseline.world(), candidate.world());
    }

    fn active_island_count(world: &PhysicsWorld) -> usize {
        crate::island::build(
            world
                .bodies
                .values()
                .filter(|body| body.kind == BodyKind::Dynamic)
                .map(|body| (body.handle, !body.sleeping)),
            &world.active_pairs,
        )
        .iter()
        .filter(|island| !island.pairs.is_empty())
        .count()
    }

    #[test]
    fn default_solver_thread_count_is_bounded_and_setting_is_observed() {
        let mut world = PhysicsWorld::default();
        assert!((1..=4).contains(&world.solver_threads().get()));
        for threads in [1, 2, 4, 7] {
            world.set_solver_threads(NonZeroUsize::new(threads).unwrap());
            assert_eq!(world.solver_threads().get(), threads);
        }
    }

    #[test]
    fn one_two_four_and_seven_threads_match_across_contact_scenes() {
        for threshold in [0, TEST_PARALLEL_MIN_CONSTRAINTS] {
            for threads in [2, 4, 7] {
                let mut baseline = grid_driver(1, threshold);
                let mut candidate = grid_driver(threads, threshold);
                compare_steps(&mut baseline, &mut candidate, 5, None, &[]);
                assert!(candidate.world().active_pair_count() >= 100);
                assert!(active_island_count(candidate.world()) >= 2);
                assert!(candidate.world().parallel_solver_runs > 0);

                let (mut baseline, pusher, bodies) = composite_driver(1, threshold);
                let (mut candidate, candidate_pusher, candidate_bodies) =
                    composite_driver(threads, threshold);
                assert_eq!(pusher, candidate_pusher);
                assert_eq!(bodies, candidate_bodies);
                compare_steps(
                    &mut baseline,
                    &mut candidate,
                    12,
                    Some(pusher),
                    &[bodies[4], bodies[1]],
                );
                assert!(candidate.world().active_pairs.values().any(|pair| {
                    pair.manifold.as_ref().is_some_and(|manifold| {
                        manifold.contacts.iter().any(|contact| contact.sliding)
                    })
                }));
                if threshold == 0 {
                    assert!(candidate.world().parallel_solver_runs > 0);
                }

                let (mut baseline, baseline_tail) = truncated_driver(1, threshold);
                let (mut candidate, candidate_tail) = truncated_driver(threads, threshold);
                assert_eq!(baseline_tail, candidate_tail);
                for _ in 0..5 {
                    let baseline_result = baseline.advance_step(1.0 / 60.0, []).unwrap();
                    let candidate_result = candidate.advance_step(1.0 / 60.0, []).unwrap();
                    assert!(baseline_result.broadphase_truncated > 0);
                    assert!(candidate_result.broadphase_truncated > 0);
                    assert_step_bits(&baseline_result, &candidate_result);
                    assert_world_bits(baseline.world(), candidate.world());
                }
                assert_snapshots_bits(baseline.world(), candidate.world());
                if threshold == 0 {
                    assert!(candidate.world().parallel_solver_runs > 0);
                }
                assert!(candidate.world().active_pairs.contains_key(&candidate_tail));
                assert!(active_island_count(candidate.world()) >= 2);
            }
        }
    }

    fn deep_stacks_driver(threads: usize, threshold: usize) -> StepDriver {
        let mut world = PhysicsWorld::default();
        add_floor(&mut world, Vec3::new(30.0, 0.5, 8.0));
        for x in [-10.0, 10.0] {
            for index in 0..40 {
                world
                    .insert_body(BodyDesc::dynamic(
                        Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                        Pose::new(Vec3::new(x, 0.45 + index as f32 * 0.9, 0.0), Quat::IDENTITY)
                            .unwrap(),
                        1.0,
                    ))
                    .unwrap();
            }
        }
        configure(&mut world, threads, threshold);
        StepDriver::new(world, 60, 8).unwrap()
    }

    #[test]
    fn more_threads_than_large_contact_islands_are_supported() {
        let mut baseline = deep_stacks_driver(1, TEST_PARALLEL_MIN_CONSTRAINTS);
        let mut candidate = deep_stacks_driver(7, TEST_PARALLEL_MIN_CONSTRAINTS);
        compare_steps(&mut baseline, &mut candidate, 4, None, &[]);
        assert_eq!(active_island_count(candidate.world()), 2);
        assert!(candidate.world().parallel_solver_runs > 0);
    }

    fn sleeping_stack(threads: usize, threshold: usize) -> (StepDriver, Vec<BodyHandle>) {
        let mut world = PhysicsWorld::default();
        add_floor(&mut world, Vec3::new(8.0, 0.5, 8.0));
        let mut bodies = Vec::new();
        for index in 0..4 {
            bodies.push(
                world
                    .insert_body(BodyDesc::dynamic(
                        Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                        Pose::new(
                            Vec3::new(0.0, 0.45 + index as f32 * 0.9, 0.0),
                            Quat::IDENTITY,
                        )
                        .unwrap(),
                        1.0,
                    ))
                    .unwrap(),
            );
        }
        configure(&mut world, threads, threshold);
        (StepDriver::new(world, 60, 8).unwrap(), bodies)
    }

    type ManifoldImpulses = (PairKey, Vec<u32>, Vec<[u32; 3]>, Vec<bool>);

    fn manifold_impulses(world: &PhysicsWorld) -> Vec<ManifoldImpulses> {
        world
            .active_pairs
            .iter()
            .filter_map(|(key, pair)| {
                pair.manifold.as_ref().map(|manifold| {
                    (
                        *key,
                        manifold
                            .contacts
                            .iter()
                            .map(|contact| contact.normal_impulse.to_bits())
                            .collect(),
                        manifold
                            .contacts
                            .iter()
                            .map(|contact| bits3(contact.tangent_impulse))
                            .collect(),
                        manifold
                            .contacts
                            .iter()
                            .map(|contact| contact.sliding)
                            .collect(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn sleeping_islands_are_skipped_and_impulse_wakes_the_whole_stack_identically() {
        for threads in [2, 4, 7] {
            let (mut baseline, baseline_bodies) = sleeping_stack(1, TEST_PARALLEL_MIN_CONSTRAINTS);
            let (mut candidate, candidate_bodies) =
                sleeping_stack(threads, TEST_PARALLEL_MIN_CONSTRAINTS);
            assert_eq!(baseline_bodies, candidate_bodies);
            for _ in 0..240 {
                let baseline_result = baseline.advance_step(1.0 / 60.0, []).unwrap();
                let candidate_result = candidate.advance_step(1.0 / 60.0, []).unwrap();
                assert_step_bits(&baseline_result, &candidate_result);
                assert_world_bits(baseline.world(), candidate.world());
            }
            assert!(
                baseline_bodies
                    .iter()
                    .all(|handle| baseline.world().body_is_sleeping(*handle) == Some(true))
            );
            assert!(
                candidate_bodies
                    .iter()
                    .all(|handle| candidate.world().body_is_sleeping(*handle) == Some(true))
            );
            assert_eq!(candidate.world().parallel_solver_runs, 0);
            let old_impulses = manifold_impulses(candidate.world());
            let baseline_result = baseline.advance_step(1.0 / 60.0, []).unwrap();
            let candidate_result = candidate.advance_step(1.0 / 60.0, []).unwrap();
            assert_step_bits(&baseline_result, &candidate_result);
            assert_world_bits(baseline.world(), candidate.world());
            assert_eq!(manifold_impulses(candidate.world()), old_impulses);

            let command = PhysicsCommand::AddImpulse {
                body: candidate_bodies[3],
                impulse: Vec3::Y * 0.5,
                point: None,
            };
            let baseline_result = baseline
                .advance_step(
                    1.0 / 60.0,
                    [PhysicsCommand::AddImpulse {
                        body: baseline_bodies[3],
                        impulse: Vec3::Y * 0.5,
                        point: None,
                    }],
                )
                .unwrap();
            let candidate_result = candidate.advance_step(1.0 / 60.0, [command]).unwrap();
            assert_step_bits(&baseline_result, &candidate_result);
            assert_world_bits(baseline.world(), candidate.world());
            assert!(
                candidate_bodies
                    .iter()
                    .all(|handle| candidate.world().body_is_sleeping(*handle) == Some(false))
            );
            assert_ne!(manifold_impulses(candidate.world()), old_impulses);
        }
    }

    fn mixed_sleeping_world(
        threads: usize,
        threshold: usize,
    ) -> (PhysicsWorld, BodyHandle, BodyHandle) {
        let mut world = PhysicsWorld::default();
        let first = world
            .insert_body(BodyDesc::dynamic(
                Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                Pose::new(Vec3::new(-10.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let second = world
            .insert_body(BodyDesc::dynamic(
                Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                Pose::new(Vec3::new(10.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let kinematic = world
            .insert_body(BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::new(-30.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                ColliderDesc::new(Shape::cuboid(Vec3::splat(0.5)).unwrap()),
            ))
            .unwrap();
        world.refresh_pairs().unwrap();
        for (handle, position) in [(first, Vec3::ZERO), (second, Vec3::new(0.8, 0.0, 0.0))] {
            let body = world.bodies.get_mut(&handle).unwrap();
            body.pose.position = position;
            body.sleeping = true;
            body.sleep_time = SLEEP_DELAY_SECONDS;
            body.linear_velocity = Vec3::ZERO;
            body.angular_velocity = Vec3::ZERO;
            body.mark_pose_changed();
        }
        world
            .apply_command(
                PhysicsCommand::KinematicTarget {
                    body: kinematic,
                    pose: Pose::new(Vec3::new(-0.7, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                },
                1.0 / 60.0,
            )
            .unwrap();
        configure(&mut world, threads, threshold);
        (world, first, second)
    }

    #[test]
    fn mixed_awake_sleeping_islands_match_and_wake_as_one_after_impulse() {
        for threshold in [0, TEST_PARALLEL_MIN_CONSTRAINTS] {
            for threads in [2, 4, 7] {
                let (mut baseline, awake, sleeping) = mixed_sleeping_world(1, threshold);
                let (mut candidate, candidate_awake, candidate_sleeping) =
                    mixed_sleeping_world(threads, threshold);
                assert_eq!((awake, sleeping), (candidate_awake, candidate_sleeping));
                let baseline_events = baseline.step_fixed(1.0 / 60.0).unwrap();
                let candidate_events = candidate.step_fixed(1.0 / 60.0).unwrap();
                assert_events_bits(&baseline_events, &candidate_events);
                assert_world_bits(&baseline, &candidate);
                assert_eq!(candidate.body_is_sleeping(candidate_awake), Some(false));
                assert_eq!(candidate.body_is_sleeping(candidate_sleeping), Some(true));
                let mixed_pair = PairKey::new(candidate_awake.0, candidate_sleeping.0);
                assert!(
                    candidate
                        .active_pairs
                        .get(&mixed_pair)
                        .and_then(|pair| pair.manifold.as_ref())
                        .is_some()
                );
                assert!(
                    crate::island::build(
                        candidate
                            .bodies
                            .values()
                            .filter(|body| body.kind == BodyKind::Dynamic)
                            .map(|body| (body.handle, !body.sleeping)),
                        &candidate.active_pairs,
                    )
                    .iter()
                    .any(|island| island.bodies.contains(&candidate_awake)
                        && island.bodies.contains(&candidate_sleeping))
                );

                let mut baseline = StepDriver::new(baseline, 60, 8).unwrap();
                let mut candidate = StepDriver::new(candidate, 60, 8).unwrap();
                compare_steps(&mut baseline, &mut candidate, 1, None, &[]);
                let baseline_result = baseline
                    .advance_step(
                        1.0 / 60.0,
                        [PhysicsCommand::AddImpulse {
                            body: sleeping,
                            impulse: Vec3::Y * 0.5,
                            point: None,
                        }],
                    )
                    .unwrap();
                let candidate_result = candidate
                    .advance_step(
                        1.0 / 60.0,
                        [PhysicsCommand::AddImpulse {
                            body: candidate_sleeping,
                            impulse: Vec3::Y * 0.5,
                            point: None,
                        }],
                    )
                    .unwrap();
                assert_step_bits(&baseline_result, &candidate_result);
                assert_world_bits(baseline.world(), candidate.world());
                assert_eq!(
                    candidate.world().body_is_sleeping(candidate_awake),
                    Some(false)
                );
                assert_eq!(
                    candidate.world().body_is_sleeping(candidate_sleeping),
                    Some(false)
                );
            }
        }
    }

    #[test]
    fn default_threshold_keeps_a_small_scene_on_the_calling_thread() {
        let mut world = PhysicsWorld::default();
        add_floor(&mut world, Vec3::new(8.0, 0.5, 8.0));
        for index in 0..16 {
            world
                .insert_body(BodyDesc::dynamic(
                    Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                    Pose::new(
                        Vec3::new(index as f32 * 1.2 - 9.0, 0.45, 0.0),
                        Quat::IDENTITY,
                    )
                    .unwrap(),
                    1.0,
                ))
                .unwrap();
        }
        configure(&mut world, 4, TEST_PARALLEL_MIN_CONSTRAINTS);
        let mut driver = StepDriver::new(world, 60, 8).unwrap();
        for _ in 0..6 {
            driver.advance_step(1.0 / 60.0, []).unwrap();
        }
        assert_eq!(driver.world().parallel_solver_runs, 0);
    }
}
