use std::fmt;

use glam::{Quat, Vec3};

use crate::query::{QueryError, QueryFilter, QueryHit, QueryOverrides, QuerySnapshot};
use crate::world::{BodyHandle, BodyKind};
use crate::{GeometryError, Pose, Shape};

pub const CHARACTER_MOVE_ITERATIONS: usize = 4;
pub const MAX_PUSH_SPEED: f32 = 10.0;
const STEP_EXTRA_ITERATIONS: usize = 8;
const MOVEMENT_EPSILON_SQUARED: f32 = 1.0e-10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharacterController {
    pub radius: f32,
    pub height: f32,
    pub step_height: f32,
    pub max_slope_deg: f32,
    pub snap_distance: f32,
    pub skin: f32,
    pub push_strength: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CharacterError {
    Geometry(GeometryError),
    Query(QueryError),
    InvalidSettings,
    NonFiniteInput,
}

impl fmt::Display for CharacterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::Query(error) => error.fmt(formatter),
            Self::InvalidSettings => {
                formatter.write_str("character controller settings are invalid")
            }
            Self::NonFiniteInput => formatter.write_str("character movement must be finite"),
        }
    }
}

impl std::error::Error for CharacterError {}

impl From<GeometryError> for CharacterError {
    fn from(value: GeometryError) -> Self {
        Self::Geometry(value)
    }
}

impl From<QueryError> for CharacterError {
    fn from(value: QueryError) -> Self {
        Self::Query(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharacterHit {
    pub body: BodyHandle,
    pub point: Vec3,
    /// Direction from the character capsule toward the obstacle.
    pub normal: Vec3,
    pub fraction: f32,
    pub blocked_displacement: Vec3,
    /// Full displacement requested for this controller move.
    pub requested_displacement: Vec3,
    pub body_kind: BodyKind,
    pub body_velocity: Vec3,
    pub mass: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CharacterMoveResult {
    pub translation: Vec3,
    pub blocked_displacement: Vec3,
    pub grounded: bool,
    pub hits: Vec<CharacterHit>,
    /// True when a numeric query limit stopped part of the move at its current position.
    pub degraded: bool,
}

#[derive(Default)]
struct SlideResult {
    position: Vec3,
    blocked_displacement: Vec3,
    grounded: bool,
    hits: Vec<CharacterHit>,
    degraded: bool,
}

fn is_numeric_limit(error: &CharacterError) -> bool {
    match error {
        CharacterError::Query(error) => is_numeric_query_limit(error),
        _ => false,
    }
}

fn is_numeric_query_limit(error: &QueryError) -> bool {
    match error {
        QueryError::ShapeCastIterationLimit | QueryError::CandidateLimit => true,
        QueryError::Narrowphase(error) => error.is_numeric_limit(),
        _ => false,
    }
}

impl CharacterController {
    pub fn new(
        radius: f32,
        height: f32,
        step_height: f32,
        max_slope_deg: f32,
        snap_distance: f32,
        skin: f32,
        push_strength: f32,
    ) -> Result<Self, CharacterError> {
        let controller = Self {
            radius,
            height,
            step_height,
            max_slope_deg,
            snap_distance,
            skin,
            push_strength,
        };
        if !radius.is_finite()
            || !height.is_finite()
            || !step_height.is_finite()
            || !max_slope_deg.is_finite()
            || !snap_distance.is_finite()
            || !skin.is_finite()
            || !push_strength.is_finite()
            || radius <= 0.0
            || height <= 0.0
            || step_height < 0.0
            || max_slope_deg <= 0.0
            || max_slope_deg >= 90.0
            || snap_distance < 0.0
            || skin < 0.0
            || push_strength < 0.0
        {
            return Err(CharacterError::InvalidSettings);
        }
        Ok(controller)
    }

    pub fn capsule(&self) -> Result<Shape, GeometryError> {
        Shape::capsule(self.radius, self.height)
    }

    pub fn move_capsule(
        &self,
        snapshot: &QuerySnapshot,
        body: BodyHandle,
        start_pose: Pose,
        displacement: Vec3,
    ) -> Result<CharacterMoveResult, CharacterError> {
        self.move_capsule_excluding(snapshot, body, start_pose, displacement, &[])
    }

    /// Moves a capsule while ignoring bodies disabled by staged script commands.
    pub fn move_capsule_excluding(
        &self,
        snapshot: &QuerySnapshot,
        body: BodyHandle,
        start_pose: Pose,
        displacement: Vec3,
        excluded: &[BodyHandle],
    ) -> Result<CharacterMoveResult, CharacterError> {
        self.move_capsule_with_enabled_overrides(
            snapshot,
            body,
            start_pose,
            displacement,
            excluded,
            &[],
        )
    }

    /// Moves a capsule while applying script-staged collider enabled states.
    pub fn move_capsule_with_enabled_overrides(
        &self,
        snapshot: &QuerySnapshot,
        body: BodyHandle,
        start_pose: Pose,
        displacement: Vec3,
        excluded: &[BodyHandle],
        enabled_overrides: &[(BodyHandle, bool)],
    ) -> Result<CharacterMoveResult, CharacterError> {
        if !displacement.is_finite() {
            return Err(CharacterError::NonFiniteInput);
        }
        let start_pose = Pose::new(start_pose.position, start_pose.rotation)?;
        let shape = self.capsule()?;
        let overrides = QueryOverrides {
            excluded,
            enabled: enabled_overrides,
        };
        let mut movement =
            self.slide(snapshot, body, &shape, start_pose, displacement, overrides)?;
        let mut degraded = movement.degraded;
        if self.step_height > 0.0
            && movement.hits.iter().any(|hit| {
                hit.blocked_displacement.length_squared() > MOVEMENT_EPSILON_SQUARED
                    && !self.is_walkable(-hit.normal)
            })
        {
            match self.try_step(snapshot, body, &shape, start_pose, displacement, overrides) {
                Ok(Some(stepped))
                    if horizontal_progress(stepped.position - start_pose.position)
                        > horizontal_progress(movement.position - start_pose.position) + 1.0e-4 =>
                {
                    degraded |= stepped.degraded;
                    movement = stepped;
                }
                Ok(Some(stepped)) => degraded |= stepped.degraded,
                Ok(None) => {}
                Err(error) if is_numeric_limit(&error) => degraded = true,
                Err(error) => return Err(error),
            }
        }
        if !movement.grounded && displacement.y <= 0.0 {
            match self.snap_to_ground(
                snapshot,
                body,
                start_pose.rotation,
                &shape,
                &mut movement,
                overrides,
            ) {
                Ok(()) => {}
                Err(error) if is_numeric_limit(&error) => degraded = true,
                Err(error) => return Err(error),
            }
        }
        for hit in &mut movement.hits {
            hit.requested_displacement = displacement;
        }
        Ok(CharacterMoveResult {
            translation: movement.position - start_pose.position,
            blocked_displacement: movement.blocked_displacement,
            grounded: movement.grounded,
            hits: movement.hits,
            degraded,
        })
    }

    /// Computes a dynamic-body push impulse from requested controller velocity.
    pub fn push_impulse(&self, hit: &CharacterHit, phase_dt: f32) -> Option<Vec3> {
        if hit.body_kind != BodyKind::Dynamic
            || !phase_dt.is_finite()
            || phase_dt < 1.0e-6
            || !hit.requested_displacement.is_finite()
            || !hit.normal.is_finite()
            || !hit.body_velocity.is_finite()
            || !hit.mass.is_finite()
            || hit.mass < 0.0
            || !self.push_strength.is_finite()
            || self.push_strength < 0.0
        {
            return None;
        }
        let requested_velocity = hit.requested_displacement / phase_dt;
        if !requested_velocity.is_finite() {
            return None;
        }
        let character_velocity = requested_velocity.clamp_length_max(MAX_PUSH_SPEED);
        let relative_speed = character_velocity.dot(hit.normal) - hit.body_velocity.dot(hit.normal);
        let magnitude = self.push_strength * hit.mass * relative_speed.max(0.0);
        magnitude.is_finite().then_some(hit.normal * magnitude)
    }

    fn slide(
        &self,
        snapshot: &QuerySnapshot,
        body: BodyHandle,
        shape: &Shape,
        start_pose: Pose,
        displacement: Vec3,
        overrides: QueryOverrides<'_>,
    ) -> Result<SlideResult, CharacterError> {
        let mut result = SlideResult {
            position: start_pose.position,
            ..SlideResult::default()
        };
        let mut remaining = displacement;
        let filter = QueryFilter {
            ignore: Some(body),
            include_triggers: false,
            ..QueryFilter::default()
        };

        for _ in 0..CHARACTER_MOVE_ITERATIONS {
            if remaining.length_squared() <= MOVEMENT_EPSILON_SQUARED {
                break;
            }
            let pose = Pose::new(result.position, start_pose.rotation)?;
            let hit = match snapshot
                .shape_cast_with_overrides(shape, pose, remaining, self.skin, filter, overrides)
            {
                Ok(Some(hit)) => hit,
                Ok(None) => {
                    result.position += remaining;
                    break;
                }
                Err(error) if is_numeric_query_limit(&error) => {
                    result.degraded = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            let travel = remaining * hit.fraction;
            result.position += travel;
            let unspent = remaining * (1.0 - hit.fraction);
            let into_obstacle = unspent.dot(hit.normal).max(0.0);
            if into_obstacle <= 1.0e-6 {
                result.position += unspent;
                break;
            }
            let blocked = hit.normal * into_obstacle;
            result.blocked_displacement += blocked;
            result.grounded |= self.is_walkable(-hit.normal) && displacement.dot(hit.normal) > 0.0;
            result.hits.push(character_hit(hit, blocked));

            let surface_normal = -hit.normal;
            if surface_normal.y > 1.0e-3 && !self.is_walkable(surface_normal) {
                break;
            }
            remaining = unspent - blocked;
        }
        Ok(result)
    }

    fn try_step(
        &self,
        snapshot: &QuerySnapshot,
        body: BodyHandle,
        shape: &Shape,
        start_pose: Pose,
        displacement: Vec3,
        overrides: QueryOverrides<'_>,
    ) -> Result<Option<SlideResult>, CharacterError> {
        let horizontal = Vec3::new(displacement.x, 0.0, displacement.z);
        if horizontal.length_squared() <= MOVEMENT_EPSILON_SQUARED {
            return Ok(None);
        }
        let filter = QueryFilter {
            ignore: Some(body),
            include_triggers: false,
            ..QueryFilter::default()
        };
        let up = Vec3::Y * self.step_height;
        if snapshot
            .shape_cast_with_overrides(shape, start_pose, up, self.skin, filter, overrides)?
            .is_some()
        {
            return Ok(None);
        }
        let raised_pose = Pose::new(start_pose.position + up, start_pose.rotation)?;
        let mut stepped = self.slide(snapshot, body, shape, raised_pose, horizontal, overrides)?;
        let down = -Vec3::Y * (self.step_height + self.skin);
        let dir = horizontal.normalize();
        let probe = |origin: Vec3| -> Result<Option<QueryHit>, CharacterError> {
            let pose = Pose::new(origin, start_pose.rotation)?;
            Ok(snapshot
                .shape_cast_with_overrides(shape, pose, down, self.skin, filter, overrides)?
                .filter(|hit| self.is_walkable(-hit.normal)))
        };
        let ground = match probe(stepped.position)? {
            Some(hit) => hit,
            None => {
                // Центр у грани ступени, опора на ребре: нижняя полусфера касается ребра
                // наклонной нормалью. Двигаем капсулу минимально вдоль направления запроса,
                // не дальше свободного пути и радиуса + skin, и ищем опору заново.
                let max_extra = self.radius + self.skin;
                let free = snapshot
                    .shape_cast_with_overrides(
                        shape,
                        Pose::new(stepped.position, start_pose.rotation)?,
                        dir * max_extra,
                        self.skin,
                        filter,
                        overrides,
                    )?
                    .map_or(max_extra, |hit| hit.distance);
                let mut high = free;
                if probe(stepped.position + dir * high)?.is_none() {
                    return Ok(None);
                }
                let mut low = 0.0_f32;
                for _ in 0..STEP_EXTRA_ITERATIONS {
                    let middle = 0.5 * (low + high);
                    if probe(stepped.position + dir * middle)?.is_some() {
                        high = middle;
                    } else {
                        low = middle;
                    }
                }
                stepped.position += dir * high;
                let Some(hit) = probe(stepped.position)? else {
                    return Ok(None);
                };
                hit
            }
        };
        let final_position = stepped.position + down * ground.fraction;
        if final_position.y - start_pose.position.y > self.step_height + self.skin + 1.0e-4 {
            return Ok(None);
        }
        stepped.position = final_position;
        stepped.grounded = true;
        Ok(Some(stepped))
    }

    fn snap_to_ground(
        &self,
        snapshot: &QuerySnapshot,
        body: BodyHandle,
        rotation: Quat,
        shape: &Shape,
        movement: &mut SlideResult,
        overrides: QueryOverrides<'_>,
    ) -> Result<(), CharacterError> {
        if self.snap_distance <= 0.0 && self.step_height <= 0.0 {
            return Ok(());
        }
        let pose = Pose::new(movement.position, rotation)?;
        let down = -Vec3::Y * (self.snap_distance.max(self.step_height) + self.skin);
        let filter = QueryFilter {
            ignore: Some(body),
            include_triggers: false,
            ..QueryFilter::default()
        };
        let Some(hit) =
            snapshot.shape_cast_with_overrides(shape, pose, down, self.skin, filter, overrides)?
        else {
            return Ok(());
        };
        if self.is_walkable(-hit.normal) {
            movement.position += down * hit.fraction;
            movement.grounded = true;
        }
        Ok(())
    }

    fn is_walkable(&self, surface_normal: Vec3) -> bool {
        surface_normal.y >= self.max_slope_deg.to_radians().cos()
    }
}

fn character_hit(hit: QueryHit, blocked_displacement: Vec3) -> CharacterHit {
    CharacterHit {
        body: hit.body,
        point: hit.point,
        normal: hit.normal,
        fraction: hit.fraction,
        blocked_displacement,
        requested_displacement: Vec3::ZERO,
        body_kind: hit.body_kind,
        body_velocity: hit.body_velocity,
        mass: hit.mass,
    }
}

fn horizontal_progress(displacement: Vec3) -> f32 {
    Vec3::new(displacement.x, 0.0, displacement.z).length()
}

#[cfg(test)]
mod tests {
    use super::{CharacterController, CharacterError, MAX_PUSH_SPEED};
    use crate::world::{BodyDesc, BodyHandle, BodyKind, PhysicsWorld};
    use crate::{Pose, Shape, TriMesh};
    use glam::{Quat, Vec3};
    use std::sync::Arc;

    fn controller(step_height: f32, max_slope_deg: f32, snap_distance: f32) -> CharacterController {
        CharacterController::new(
            0.25,
            0.5,
            step_height,
            max_slope_deg,
            snap_distance,
            0.01,
            1.5,
        )
        .unwrap()
    }

    #[test]
    fn constructor_rejects_zero_negative_and_ninety_degree_slope_limits() {
        for slope in [0.0, -0.1, 90.0] {
            assert_eq!(
                CharacterController::new(0.25, 0.5, 0.0, slope, 0.0, 0.01, 1.0).err(),
                Some(CharacterError::InvalidSettings),
                "slope={slope}"
            );
        }
    }

    fn floor() -> Shape {
        Shape::TriMesh(Arc::new(
            TriMesh::new(
                vec![
                    Vec3::new(-10.0, 0.0, -10.0),
                    Vec3::new(10.0, 0.0, -10.0),
                    Vec3::new(10.0, 0.0, 10.0),
                    Vec3::new(-10.0, 0.0, 10.0),
                ],
                vec![[0, 1, 2], [0, 2, 3]],
            )
            .unwrap(),
        ))
    }

    fn world_with_floor() -> PhysicsWorld {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        world
            .insert_body(BodyDesc::static_collider(floor(), Pose::IDENTITY))
            .unwrap();
        world
    }

    #[test]
    fn capsule_blocks_at_wall_and_slides_along_it() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.2, 2.0, 2.0)).unwrap(),
                Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = controller(0.0, 45.0, 0.0)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::IDENTITY,
                Vec3::new(2.0, 0.0, 1.0),
            )
            .unwrap();
        assert!((result.translation.x - 0.54).abs() < 0.03, "{result:?}");
        assert!((result.translation.z - 1.0).abs() < 1.0e-3, "{result:?}");
        assert!(!result.hits.is_empty());
    }

    #[test]
    fn capsule_does_not_tunnel_through_a_wall_on_large_displacement() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.1, 2.0, 2.0)).unwrap(),
                Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = controller(0.0, 45.0, 0.0)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::IDENTITY,
                Vec3::X * 100.0,
            )
            .unwrap();
        assert!(result.translation.x < 1.66, "{result:?}");
        assert!(result.translation.x > 1.5, "{result:?}");
    }

    #[test]
    fn upward_jump_moves_up_across_frames_then_descending_motion_snaps_to_ground() {
        let world = world_with_floor();
        let snapshot = world.query_snapshot().unwrap();
        let controller = controller(0.0, 45.0, 0.2);
        let body = BodyHandle(u64::MAX);
        let mut pose = Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap();

        for _ in 0..2 {
            let result = controller
                .move_capsule(&snapshot, body, pose, Vec3::Y * 0.04)
                .unwrap();
            assert!(result.translation.y > 0.039, "{result:?}");
            assert!(!result.grounded, "{result:?}");
            pose.position += result.translation;
        }

        let descending = controller
            .move_capsule(&snapshot, body, pose, -Vec3::Y * 0.02)
            .unwrap();
        assert!(descending.grounded, "{descending:?}");
        assert!(descending.translation.y < -0.03, "{descending:?}");
    }

    fn move_over_step(step_height: f32) -> super::CharacterMoveResult {
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.4, step_height * 0.5, 1.0)).unwrap(),
                Pose::new(Vec3::new(1.0, step_height * 0.5, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        controller(0.35, 45.0, 0.2)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::X,
            )
            .unwrap()
    }

    #[test]
    fn step_up_within_limit_succeeds_and_taller_step_stays_blocked() {
        let short = move_over_step(0.3);
        assert!(short.translation.x > 0.8, "{short:?}");
        assert!(
            short.translation.y > 0.2 && short.translation.y <= 0.36,
            "{short:?}"
        );
        assert!(short.grounded, "{short:?}");

        let tall = move_over_step(0.5);
        assert!(tall.translation.x < 0.5, "{tall:?}");
        assert!(tall.translation.y.abs() < 1.0e-3, "{tall:?}");
    }

    #[test]
    fn step_up_reaches_the_top_when_blocked_at_the_step_face() {
        // Капсула упирается в грань ступени. Её нижняя полусфера должна перейти на верхнюю
        // поверхность, а не касаться ребра наклонной нормалью, которую шаг отклоняет.
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.4, 0.1, 1.0)).unwrap(),
                Pose::new(Vec3::new(1.0, 0.1, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let start = Pose::new(Vec3::new(0.6 - 0.25 - 0.005, 0.5, 0.0), Quat::IDENTITY).unwrap();
        let result = controller(0.3, 45.0, 0.2)
            .move_capsule(&snapshot, BodyHandle(u64::MAX), start, Vec3::X * 0.05)
            .unwrap();
        assert!(result.translation.x > 0.05, "{result:?}");
        assert!(result.translation.y > 0.1, "{result:?}");
        assert!(result.grounded, "{result:?}");
    }

    #[test]
    fn step_up_does_not_add_displacement_beyond_the_request_on_a_step() {
        // Ступень 0.2 при запросе 0.1: кадр шага не должен уходить дальше запроса с запасом
        // радиуса + skin.
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.4, 0.1, 1.0)).unwrap(),
                Pose::new(Vec3::new(1.4, 0.1, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = controller(0.35, 45.0, 0.2)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(Vec3::new(0.7, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::X * 0.1,
            )
            .unwrap();
        assert!(result.translation.x <= 0.2, "{result:?}");
        assert!(result.grounded, "{result:?}");
        // Нижняя полусфера ложится на ребро ступени (наклон 45°), полный подъём — за несколько кадров.
        assert!(result.translation.y > 0.1, "{result:?}");
    }

    #[test]
    fn diagonal_slide_along_wall_keeps_requested_z_when_step_height_is_set() {
        // Вдоль стены по Z шаг не должен добавлять смещение: z равен запрошенному.
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.2, 0.15, 5.0)).unwrap(),
                Pose::new(Vec3::new(1.0, 0.15, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let requested = Vec3::new(0.5, 0.0, 0.3);
        let result = controller(0.35, 45.0, 0.2)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(Vec3::new(0.5, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                requested,
            )
            .unwrap();
        assert!(
            (result.translation.z - requested.z).abs() < 1.0e-3,
            "{result:?}"
        );
    }

    #[test]
    fn slow_ascent_with_small_steps_ends_on_the_top_face() {
        // Ступень 0.02 проходится по 0.05 за кадр: капсула должна подняться на верх, а не
        // зависнуть у грани.
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.4, 0.01, 1.0)).unwrap(),
                Pose::new(Vec3::new(1.4, 0.01, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let controller = controller(0.35, 45.0, 0.2);
        let mut pose = Pose::new(Vec3::new(0.5, 0.5, 0.0), Quat::IDENTITY).unwrap();
        let mut grounded = false;
        for _ in 0..12 {
            let result = controller
                .move_capsule(&snapshot, BodyHandle(u64::MAX), pose, Vec3::X * 0.05)
                .unwrap();
            pose.position += result.translation;
            grounded = result.grounded;
        }
        assert!(pose.position.x > 1.05 && pose.position.x < 1.5, "{pose:?}");
        // Центр на верхней грани 0.02 + половина капсулы 0.5 + skin 0.01.
        assert!((pose.position.y - 0.53).abs() < 1.0e-3, "{pose:?}");
        assert!(grounded);
    }

    #[test]
    fn low_thin_barrier_does_not_carry_capsule_beyond_the_request() {
        // Барьер 0.2 (≤ step_height) толщиной 0.05: шаг переносит капсулу, но не дальше запроса.
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.025, 0.1, 1.0)).unwrap(),
                Pose::new(Vec3::new(1.025, 0.1, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = controller(0.35, 45.0, 0.2)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(Vec3::new(0.5, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::X,
            )
            .unwrap();
        assert!(result.translation.x <= 1.0 + 1.0e-4, "{result:?}");
    }

    #[test]
    fn walking_off_a_platform_snaps_down_by_at_most_step_height() {
        let mut world = world_with_floor();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(0.5, 0.15, 1.0)).unwrap(),
                Pose::new(Vec3::new(0.0, 0.15, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = CharacterController::new(0.25, 0.5, 0.35, 45.0, 0.1, 0.01, 1.0)
            .unwrap()
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(Vec3::new(0.3, 0.81, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::X * 1.3,
            )
            .unwrap();
        assert!(result.translation.x > 1.0, "{result:?}");
        assert!(
            result.translation.y < -0.2 && result.translation.y >= -0.36,
            "{result:?}"
        );
        assert!(result.grounded, "{result:?}");
    }

    fn slope_mesh(degrees: f32) -> Shape {
        let rise = degrees.to_radians().tan() * 10.0;
        Shape::TriMesh(Arc::new(
            TriMesh::new(
                vec![
                    Vec3::new(-10.0, -rise, -10.0),
                    Vec3::new(10.0, rise, -10.0),
                    Vec3::new(10.0, rise, 10.0),
                    Vec3::new(-10.0, -rise, 10.0),
                ],
                vec![[0, 1, 2], [0, 2, 3]],
            )
            .unwrap(),
        ))
    }

    #[test]
    fn slope_limit_controls_grounding_and_downward_snap() {
        for (slope, expected_grounded) in [(30.0, true), (60.0, false)] {
            let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
            world
                .insert_body(BodyDesc::static_collider(slope_mesh(slope), Pose::IDENTITY))
                .unwrap();
            let snapshot = world.query_snapshot().unwrap();
            let result = controller(0.0, 45.0, 0.2)
                .move_capsule(
                    &snapshot,
                    BodyHandle(u64::MAX),
                    Pose::new(Vec3::new(0.0, 0.65, 0.0), Quat::IDENTITY).unwrap(),
                    Vec3::ZERO,
                )
                .unwrap();
            assert_eq!(
                result.grounded, expected_grounded,
                "slope={slope}: {result:?}"
            );
            if expected_grounded {
                assert!(
                    result.translation.y < 0.0 && result.translation.y > -0.2,
                    "{result:?}"
                );
            }

            let cosine = slope.to_radians().cos();
            let vertical_offset = (0.25 + 0.25 * cosine + 0.05) / cosine;
            let walk_result = controller(0.0, 45.0, 0.0)
                .move_capsule(
                    &snapshot,
                    BodyHandle(u64::MAX),
                    Pose::new(
                        Vec3::new(-1.0, -slope.to_radians().tan() + vertical_offset, 0.0),
                        Quat::IDENTITY,
                    )
                    .unwrap(),
                    Vec3::X * 0.5,
                )
                .unwrap();
            if expected_grounded {
                assert!(
                    walk_result.translation.x > 0.25,
                    "slope={slope}: {walk_result:?}"
                );
            } else {
                assert!(
                    walk_result.translation.x < 0.1,
                    "slope={slope}: {walk_result:?}"
                );
            }
        }

        let world = world_with_floor();
        let snapshot = world.query_snapshot().unwrap();
        let snapped = controller(0.0, 45.0, 0.2)
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::new(Vec3::new(0.0, 0.62, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::ZERO,
            )
            .unwrap();
        assert!(snapped.grounded, "{snapped:?}");
        assert!(
            snapped.translation.y < 0.0 && snapped.translation.y > -0.2,
            "{snapped:?}"
        );
    }

    #[test]
    fn character_excludes_itself_and_triggers_but_reports_dynamic_push_metadata() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let character = Shape::capsule(0.25, 0.5).unwrap();
        let mut character_desc = BodyDesc::new(
            BodyKind::Kinematic,
            Pose::IDENTITY,
            crate::ColliderDesc::new(character),
        );
        character_desc.character_controller = true;
        let character_handle = world.insert_body(character_desc).unwrap();
        let mut trigger = BodyDesc::static_collider(
            Shape::cuboid(Vec3::splat(0.3)).unwrap(),
            Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        let trigger_handle = world.insert_body(trigger).unwrap();
        let mut dynamic = BodyDesc::dynamic(
            Shape::cuboid(Vec3::splat(0.25)).unwrap(),
            Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            2.0,
        );
        dynamic.collider.material.static_friction = 0.0;
        dynamic.collider.material.dynamic_friction = 0.0;
        let dynamic_handle = world.insert_body(dynamic).unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = controller(0.0, 45.0, 0.0)
            .move_capsule(&snapshot, character_handle, Pose::IDENTITY, Vec3::X * 4.0)
            .unwrap();
        let dynamic_hit = result
            .hits
            .iter()
            .find(|hit| hit.body == dynamic_handle)
            .unwrap();
        assert!(!result.hits.iter().any(|hit| hit.body == character_handle));
        assert!(!result.hits.iter().any(|hit| hit.body == trigger_handle));
        assert_eq!(dynamic_hit.body_kind, BodyKind::Dynamic);
        assert_eq!(dynamic_hit.mass, 2.0);
        let impulse = controller(0.0, 45.0, 0.0)
            .push_impulse(dynamic_hit, 0.1)
            .unwrap();
        assert_eq!(dynamic_hit.requested_displacement, Vec3::X * 4.0);
        assert!(impulse.x > 0.0, "{impulse:?}");
        assert!(impulse.length() <= 2.0 * 1.5 * MAX_PUSH_SPEED + 1.0e-4);
        let mut invalid_hit = *dynamic_hit;
        invalid_hit.requested_displacement.x = f32::NAN;
        assert!(
            controller(0.0, 45.0, 0.0)
                .push_impulse(&invalid_hit, 0.1)
                .is_none()
        );
        assert!(
            controller(0.0, 45.0, 0.0)
                .push_impulse(dynamic_hit, 0.0)
                .is_none()
        );
    }

    #[test]
    fn numeric_query_limit_stops_the_move_without_failing_it() {
        // Центральный веер из 16385 треугольников: все кандидаты в AABB капсулы, лимит mesh-кандидатов
        // превышен. Это численный сбой: движение останавливается на текущей позиции, без ошибки.
        let mut vertices = vec![Vec3::new(0.0, 0.5, 0.0)];
        let ring = crate::narrow::MAX_MESH_CANDIDATES + 1;
        for index in 0..ring {
            let angle = index as f32 / ring as f32 * std::f32::consts::TAU;
            vertices.push(Vec3::new(0.1 * angle.cos(), 0.5, 0.1 * angle.sin()));
        }
        let triangles = (0..ring)
            .map(|index| [0, index as u32 + 1, ((index + 1) % ring) as u32 + 1])
            .collect();
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        world
            .insert_body(BodyDesc::static_collider(
                Shape::TriMesh(Arc::new(TriMesh::new(vertices, triangles).unwrap())),
                Pose::IDENTITY,
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let controller = controller(0.35, 45.0, 0.2);
        let start = Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap();

        let result = controller
            .move_capsule(&snapshot, BodyHandle(u64::MAX), start, Vec3::X * 0.1)
            .expect("numeric query limit must not fail the move");
        assert!(result.degraded, "{result:?}");
        assert!(result.translation.length() < 1.0e-6, "{result:?}");

        assert_eq!(
            controller
                .move_capsule(&snapshot, BodyHandle(u64::MAX), start, Vec3::NAN)
                .err(),
            Some(CharacterError::NonFiniteInput)
        );
    }

    #[test]
    fn push_impulse_uses_requested_velocity_for_partial_contact() {
        let controller = controller(0.0, 45.0, 0.0);
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let body = world
            .insert_body(BodyDesc::dynamic(
                Shape::cuboid(Vec3::splat(0.25)).unwrap(),
                Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let result = controller
            .move_capsule(
                &snapshot,
                BodyHandle(u64::MAX),
                Pose::IDENTITY,
                Vec3::X * 0.4,
            )
            .unwrap();
        let hit = result.hits.iter().find(|hit| hit.body == body).unwrap();
        assert!((hit.fraction - 0.6).abs() < 0.02, "{hit:?}");
        assert!((hit.blocked_displacement.x - 0.16).abs() < 0.02, "{hit:?}");

        let impulse = controller.push_impulse(hit, 0.4).unwrap();
        assert!((impulse.x - 1.5).abs() < 1.0e-4, "{impulse:?}");
    }
}
