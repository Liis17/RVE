use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use glam::Vec3;

use crate::broad::MAX_BROADPHASE_ITEMS;
use crate::narrow::{MAX_MESH_CANDIDATES, collide, convex_triangle_distance};
use crate::ray::{Ray, RayHit, raycast};
use crate::world::{BodyHandle, BodyKind, PHYSICS_LAYER_COUNT, layers_collide};
use crate::{Aabb, DistanceResult, GeometryError, NarrowphaseError, Pose, Shape, TriMesh};

pub const MAX_QUERY_CANDIDATES: usize = MAX_BROADPHASE_ITEMS;
pub const MAX_SHAPE_CAST_ITERATIONS: usize = 32;
/// Допуск сходимости conservative advancement: расстояние в пределах `skin` с округлением
/// float не должно растягивать каждую итерацию на доли 1e-9 до исчерпания лимита.
const CAST_DISTANCE_TOLERANCE: f32 = 1.0e-5;
/// Минимальный косинус между смещением и нормалью, при котором касание в пределах skin считается
/// попаданием. Меньший угол — скольжение вдоль поверхности.
const GRAZING_APPROACH: f32 = 1.0e-3;
/// Наибольшее число коллайдеров в листе BVH снимка.
const COLLIDER_TREE_LEAF: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryFilter {
    pub ignore: Option<BodyHandle>,
    pub include_triggers: bool,
    /// Битовая маска слоёв: коллайдер со слоем `n` участвует, если бит `n` установлен.
    pub layer_mask: u32,
}

impl Default for QueryFilter {
    fn default() -> Self {
        Self {
            ignore: None,
            include_triggers: false,
            layer_mask: u32::MAX,
        }
    }
}

/// Additional body exclusions and enabled-state overrides applied over a snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueryOverrides<'a> {
    pub excluded: &'a [BodyHandle],
    /// Entries may be in any order; the last matching body entry wins.
    pub enabled: &'a [(BodyHandle, bool)],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueryHit {
    pub body: BodyHandle,
    pub point: Vec3,
    /// For raycasts this is the outward surface normal; for shape casts it points from the cast shape to the obstacle.
    pub normal: Vec3,
    /// Ray distance or cast travel distance in world units.
    pub distance: f32,
    /// Fraction of the requested cast displacement; raycasts use distance / max_distance.
    pub fraction: f32,
    pub triangle_index: Option<u32>,
    pub body_kind: BodyKind,
    pub body_velocity: Vec3,
    /// Dynamic body mass, or zero for static and kinematic bodies.
    pub mass: f32,
    pub trigger: bool,
}

/// Immutable body metadata captured with a query snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueryBodyState {
    pub pose: Pose,
    pub enabled: bool,
    pub trigger: bool,
    pub body_kind: BodyKind,
    pub body_velocity: Vec3,
    pub mass: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryError {
    Geometry(GeometryError),
    Narrowphase(NarrowphaseError),
    CandidateLimit,
    ShapeCastIterationLimit,
    NonConvexCast,
    NonConvexShape,
    InvalidCast,
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::Narrowphase(error) => error.fmt(formatter),
            Self::CandidateLimit => formatter.write_str("query candidate limit exceeded"),
            Self::ShapeCastIterationLimit => {
                formatter.write_str("shape cast iteration limit exceeded")
            }
            Self::NonConvexCast => formatter.write_str("shape casts require a convex shape"),
            Self::NonConvexShape => formatter.write_str("overlap queries require a convex shape"),
            Self::InvalidCast => formatter.write_str("shape cast input is invalid"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<GeometryError> for QueryError {
    fn from(value: GeometryError) -> Self {
        Self::Geometry(value)
    }
}

impl From<NarrowphaseError> for QueryError {
    fn from(value: NarrowphaseError) -> Self {
        Self::Narrowphase(value)
    }
}

#[derive(Clone)]
pub struct QuerySnapshot {
    colliders: Vec<QueryCollider>,
    /// Позиция тела в `colliders`; хендлы уникальны, поэтому поиск тела — O(1).
    body_index: HashMap<BodyHandle, usize>,
    /// BVH static-коллайдеров; следующий снимок переиспользует его, пока их хендлы и AABB
    /// не менялись.
    static_tree: Arc<ColliderTree>,
    /// Позиция в `colliders` для каждого элемента `static_tree`.
    static_slots: Vec<usize>,
    /// BVH kinematic- и dynamic-коллайдеров, строится заново для каждого снимка.
    dynamic_tree: ColliderTree,
    /// Позиция в `colliders` для каждого элемента `dynamic_tree`.
    dynamic_slots: Vec<usize>,
    /// Активные пары триггера с хотя бы одним trigger-коллайдером; порядок по хендлам.
    trigger_pairs: Vec<(BodyHandle, BodyHandle)>,
    gravity: Vec3,
    collision_matrix: [u32; PHYSICS_LAYER_COUNT],
    /// Эталон для тестов: кандидаты прежним линейным проходом по X-порядку.
    #[cfg(test)]
    linear_reference: bool,
}

#[derive(Clone)]
pub(crate) struct QueryCollider {
    pub(crate) body: BodyHandle,
    pub(crate) shape: Shape,
    pub(crate) pose: Pose,
    bounds: Aabb,
    pub(crate) enabled: bool,
    pub(crate) trigger: bool,
    pub(crate) body_kind: BodyKind,
    pub(crate) body_velocity: Vec3,
    pub(crate) mass: f32,
    pub(crate) layer: u32,
}

pub(crate) struct QueryColliderDesc {
    pub(crate) body: BodyHandle,
    pub(crate) shape: Shape,
    pub(crate) pose: Pose,
    pub(crate) enabled: bool,
    pub(crate) trigger: bool,
    pub(crate) body_kind: BodyKind,
    pub(crate) body_velocity: Vec3,
    pub(crate) mass: f32,
    pub(crate) layer: u32,
}

impl QueryCollider {
    pub(crate) fn new(desc: QueryColliderDesc) -> Result<Self, GeometryError> {
        Ok(Self {
            body: desc.body,
            bounds: desc.shape.aabb(desc.pose)?,
            shape: desc.shape,
            pose: desc.pose,
            enabled: desc.enabled,
            trigger: desc.trigger,
            body_kind: desc.body_kind,
            body_velocity: desc.body_velocity,
            mass: desc.mass,
            layer: desc.layer,
        })
    }

    fn ray_hit(&self, ray: &Ray, hit: RayHit) -> QueryHit {
        let fraction = if ray.max_distance > 0.0 {
            hit.distance / ray.max_distance
        } else {
            0.0
        };
        QueryHit {
            body: self.body,
            point: hit.point,
            normal: hit.normal,
            distance: hit.distance,
            fraction,
            triangle_index: hit.triangle_index,
            body_kind: self.body_kind,
            body_velocity: self.body_velocity,
            mass: self.mass,
            trigger: self.trigger,
        }
    }
}

fn validate_ray(ray: &Ray) -> Result<(), QueryError> {
    if !ray.origin.is_finite()
        || !ray.direction.is_finite()
        || !ray.max_distance.is_finite()
        || ray.max_distance < 0.0
        || (ray.direction.length_squared() - 1.0).abs() > 1.0e-4
    {
        return Err(GeometryError::InvalidRay.into());
    }
    Ok(())
}

fn ray_bounds(ray: &Ray) -> Result<Aabb, QueryError> {
    let end = ray.point_at(ray.max_distance);
    Ok(Aabb::from_points(&[ray.origin, end])?)
}

impl QuerySnapshot {
    /// Строит снимок. `cached_static` — BVH static-коллайдеров прошлого снимка; он
    /// переиспользуется, если последовательность их хендлов и AABB точно совпадает.
    pub(crate) fn from_colliders(
        colliders: Vec<QueryCollider>,
        trigger_pairs: Vec<(BodyHandle, BodyHandle)>,
        gravity: Vec3,
        collision_matrix: [u32; PHYSICS_LAYER_COUNT],
        cached_static: Option<&Arc<ColliderTree>>,
    ) -> Result<Self, QueryError> {
        if colliders.len() > MAX_QUERY_CANDIDATES {
            return Err(QueryError::CandidateLimit);
        }
        let (static_slots, dynamic_slots): (Vec<usize>, Vec<usize>) =
            (0..colliders.len()).partition(|&index| colliders[index].body_kind == BodyKind::Static);
        let keys = |slots: &[usize]| {
            slots
                .iter()
                .map(|&index| (colliders[index].body, colliders[index].bounds))
                .collect::<Vec<_>>()
        };
        let static_tree = match cached_static {
            Some(tree)
                if tree.keys.iter().copied().eq(static_slots
                    .iter()
                    .map(|&index| (colliders[index].body, colliders[index].bounds))) =>
            {
                Arc::clone(tree)
            }
            _ => Arc::new(ColliderTree::build(keys(&static_slots))),
        };
        let dynamic_tree = ColliderTree::build(keys(&dynamic_slots));
        let mut body_index = HashMap::with_capacity(colliders.len());
        for (index, collider) in colliders.iter().enumerate() {
            let previous = body_index.insert(collider.body, index);
            debug_assert!(previous.is_none(), "хендлы тел снимка уникальны");
        }
        Ok(Self {
            colliders,
            body_index,
            static_tree,
            static_slots,
            dynamic_tree,
            dynamic_slots,
            trigger_pairs,
            gravity,
            collision_matrix,
            #[cfg(test)]
            linear_reference: false,
        })
    }

    /// BVH static-коллайдеров снимка; мир передаёт его следующему снимку для переиспользования.
    pub(crate) fn static_tree(&self) -> &Arc<ColliderTree> {
        &self.static_tree
    }

    /// Маска слоёв, с которыми тело `body` сталкивается по матрице мира. Пара фильтруется в обе
    /// стороны, как в шаге физики. Тело вне снимка не ограничивает запрос.
    pub(crate) fn collision_layer_mask(&self, body: BodyHandle) -> u32 {
        let Some(layer) = self
            .body_index
            .get(&body)
            .map(|&index| self.colliders[index].layer)
        else {
            return u32::MAX;
        };
        (0..PHYSICS_LAYER_COUNT as u32)
            .filter(|&other| layers_collide(&self.collision_matrix, layer, other))
            .fold(0, |mask, other| mask | (1 << other))
    }

    /// Активные trigger-пары на момент снимка, по возрастанию ключа пары.
    pub fn trigger_pairs(&self) -> &[(BodyHandle, BodyHandle)] {
        &self.trigger_pairs
    }

    /// Гравитация мира на момент снимка; скрипты берут ее для собственной интеграции.
    pub fn gravity(&self) -> Vec3 {
        self.gravity
    }

    pub fn raycast(&self, ray: &Ray, filter: QueryFilter) -> Result<Option<QueryHit>, QueryError> {
        self.raycast_excluding(ray, filter, &[])
    }

    /// Raycasts while excluding additional bodies, for transactional collider toggles.
    pub fn raycast_excluding(
        &self,
        ray: &Ray,
        filter: QueryFilter,
        excluded: &[BodyHandle],
    ) -> Result<Option<QueryHit>, QueryError> {
        self.raycast_with_enabled_overrides(ray, filter, excluded, &[])
    }

    /// Raycasts with script-staged collider enabled states applied over the snapshot.
    pub fn raycast_with_enabled_overrides(
        &self,
        ray: &Ray,
        filter: QueryFilter,
        excluded: &[BodyHandle],
        enabled_overrides: &[(BodyHandle, bool)],
    ) -> Result<Option<QueryHit>, QueryError> {
        validate_ray(ray)?;
        let broad_bounds = ray_bounds(ray)?;
        let mut closest = None;
        let overrides = QueryOverrides {
            excluded,
            enabled: enabled_overrides,
        };
        for index in self.candidates(broad_bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            let Some(hit) = raycast(ray, &collider.shape, collider.pose)? else {
                continue;
            };
            let candidate = collider.ray_hit(ray, hit);
            if closest.is_none_or(|old: QueryHit| query_hit_precedes(candidate, old)) {
                closest = Some(candidate);
            }
        }
        Ok(closest)
    }

    /// Первые попадания луча по всем подходящим коллайдерам, по возрастанию расстояния.
    pub fn raycast_all(
        &self,
        ray: &Ray,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Vec<QueryHit>, QueryError> {
        validate_ray(ray)?;
        let broad_bounds = ray_bounds(ray)?;
        let mut hits = Vec::new();
        for index in self.candidates(broad_bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            if let Some(hit) = raycast(ray, &collider.shape, collider.pose)? {
                hits.push(collider.ray_hit(ray, hit));
            }
        }
        hits.sort_by(|left, right| {
            left.distance
                .total_cmp(&right.distance)
                .then_with(|| left.body.cmp(&right.body))
        });
        Ok(hits)
    }

    /// Коллайдеры, пересекающиеся с выпуклой формой в позе `pose`; результат по возрастанию
    /// хендла тела. Trigger-коллайдеры входят только при `filter.include_triggers`.
    pub fn overlap(
        &self,
        shape: &Shape,
        pose: Pose,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Vec<BodyHandle>, QueryError> {
        if matches!(shape, Shape::TriMesh(_)) {
            return Err(QueryError::NonConvexShape);
        }
        let pose = Pose::new(pose.position, pose.rotation)?;
        let bounds = shape.aabb(pose)?;
        let mut bodies = Vec::new();
        for index in self.candidates(bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            if collide(shape, pose, &collider.shape, collider.pose)?.is_some() {
                bodies.push(collider.body);
            }
        }
        bodies.sort_unstable();
        Ok(bodies)
    }

    /// Самое глубокое проникновение выпуклой формы в static или kinematic коллайдер:
    /// нормаль от формы к препятствию и глубина. Dynamic-тела разводит solver, а не запрос.
    pub(crate) fn deepest_static_penetration(
        &self,
        shape: &Shape,
        pose: Pose,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Option<(Vec3, f32)>, QueryError> {
        let bounds = shape.aabb(pose)?;
        let mut deepest: Option<(Vec3, f32)> = None;
        for index in self.candidates(bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            if collider.body_kind == BodyKind::Dynamic {
                continue;
            }
            let Some(manifold) = collide(shape, pose, &collider.shape, collider.pose)? else {
                continue;
            };
            let depth = manifold
                .contacts
                .iter()
                .fold(0.0_f32, |depth, contact| depth.max(contact.penetration));
            if deepest.is_none_or(|(_, old)| depth > old) {
                deepest = Some((manifold.normal, depth));
            }
        }
        Ok(deepest)
    }

    pub fn shape_cast(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: QueryFilter,
    ) -> Result<Option<QueryHit>, QueryError> {
        self.shape_cast_excluding(shape, pose, displacement, skin, filter, &[])
    }

    /// Shape-casts while excluding additional bodies, for transactional collider toggles.
    pub fn shape_cast_excluding(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: QueryFilter,
        excluded: &[BodyHandle],
    ) -> Result<Option<QueryHit>, QueryError> {
        self.shape_cast_with_overrides(
            shape,
            pose,
            displacement,
            skin,
            filter,
            QueryOverrides {
                excluded,
                enabled: &[],
            },
        )
    }

    /// Shape-casts with caller-provided exclusions and enabled states applied over the snapshot.
    pub fn shape_cast_with_overrides(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Option<QueryHit>, QueryError> {
        if !displacement.is_finite() || !skin.is_finite() || skin < 0.0 {
            return Err(QueryError::InvalidCast);
        }
        if matches!(shape, Shape::TriMesh(_)) {
            return Err(QueryError::NonConvexCast);
        }
        let pose = Pose::new(pose.position, pose.rotation)?;
        let start_bounds = shape.aabb(pose)?;
        let end_pose = Pose::new(pose.position + displacement, pose.rotation)?;
        let end_bounds = shape.aabb(end_pose)?;
        let sweep_bounds = expand_aabb(start_bounds.union(end_bounds), skin)?;
        let tolerance = cast_tolerance(pose.position, displacement);
        let mut closest = None;

        for index in self.candidates(sweep_bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            let cast = match &collider.shape {
                Shape::TriMesh(mesh) => cast_against_mesh(
                    shape,
                    pose,
                    displacement,
                    skin,
                    collider.pose,
                    mesh,
                    sweep_bounds,
                )?,
                other => conservative_advance(displacement, skin, tolerance, |offset| {
                    let cast_pose = Pose::new(pose.position + offset, pose.rotation)?;
                    crate::distance(shape, cast_pose, other, collider.pose).map_err(Into::into)
                })?
                .map(|(fraction, distance)| (fraction, distance, None)),
            };
            let Some((fraction, distance, triangle_index)) = cast else {
                continue;
            };
            let candidate = QueryHit {
                body: collider.body,
                point: distance.point_b,
                normal: distance.normal,
                distance: displacement.length() * fraction,
                fraction,
                triangle_index,
                body_kind: collider.body_kind,
                body_velocity: collider.body_velocity,
                mass: collider.mass,
                trigger: collider.trigger,
            };
            if closest.is_none_or(|old: QueryHit| query_hit_precedes(candidate, old)) {
                closest = Some(candidate);
            }
        }
        Ok(closest)
    }

    pub fn body_state(&self, body: BodyHandle) -> Option<QueryBodyState> {
        self.body_index
            .get(&body)
            .map(|&index| &self.colliders[index])
            .map(|collider| QueryBodyState {
                pose: collider.pose,
                enabled: collider.enabled,
                trigger: collider.trigger,
                body_kind: collider.body_kind,
                body_velocity: collider.body_velocity,
                mass: collider.mass,
            })
    }

    /// Подходящие под фильтр коллайдеры, чьи AABB пересекают `query`, по возрастанию
    /// `(bounds.min.x, body)`. Порядок совпадает с прежним X-проходом, поэтому равные по глубине
    /// проникновения и первая ошибка формы выбираются так же.
    fn candidates(
        &self,
        query: Aabb,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Vec<usize>, QueryError> {
        #[cfg(test)]
        if self.linear_reference {
            return self.linear_candidates(query, filter, overrides);
        }
        let mut candidates = Vec::new();
        for (tree, slots) in [
            (&*self.static_tree, &self.static_slots),
            (&self.dynamic_tree, &self.dynamic_slots),
        ] {
            tree.query(query, |item| {
                let index = slots[item as usize];
                if accepts(&self.colliders[index], filter, overrides) {
                    candidates.push(index);
                }
            });
        }
        if candidates.len() > MAX_QUERY_CANDIDATES {
            return Err(QueryError::CandidateLimit);
        }
        candidates.sort_unstable_by(|&left, &right| {
            x_order(&self.colliders[left], &self.colliders[right])
        });
        Ok(candidates)
    }

    /// Прежний линейный проход по X-порядку; эталон для тестов эквивалентности.
    #[cfg(test)]
    fn linear_candidates(
        &self,
        query: Aabb,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Vec<usize>, QueryError> {
        let mut order = (0..self.colliders.len()).collect::<Vec<_>>();
        order.sort_by(|&left, &right| x_order(&self.colliders[left], &self.colliders[right]));
        let mut candidates = Vec::new();
        for index in order {
            let collider = &self.colliders[index];
            if collider.bounds.min.x > query.max.x {
                break;
            }
            if collider.bounds.max.x >= query.min.x
                && collider.bounds.overlaps(query)
                && accepts(collider, filter, overrides)
            {
                if candidates.len() == MAX_QUERY_CANDIDATES {
                    return Err(QueryError::CandidateLimit);
                }
                candidates.push(index);
            }
        }
        Ok(candidates)
    }
}

/// Фильтры запроса без проверки границ: enabled с переопределениями, ignore, exclusions,
/// trigger и маска слоёв.
fn accepts(collider: &QueryCollider, filter: QueryFilter, overrides: QueryOverrides<'_>) -> bool {
    let enabled = overrides
        .enabled
        .iter()
        .rev()
        .find_map(|(body, enabled)| (*body == collider.body).then_some(*enabled))
        .unwrap_or(collider.enabled);
    enabled
        && filter.ignore != Some(collider.body)
        && !overrides.excluded.contains(&collider.body)
        && (filter.include_triggers || !collider.trigger)
        && (filter.layer_mask >> collider.layer) & 1 != 0
}

fn x_order(left: &QueryCollider, right: &QueryCollider) -> std::cmp::Ordering {
    left.bounds
        .min
        .x
        .total_cmp(&right.bounds.min.x)
        .then_with(|| left.body.cmp(&right.body))
}

/// BVH по AABB части коллайдеров снимка. Элемент дерева — позиция в `keys`; по `keys`
/// (хендл и AABB) снимок решает, можно ли переиспользовать дерево.
#[derive(Clone, Debug)]
pub(crate) struct ColliderTree {
    keys: Vec<(BodyHandle, Aabb)>,
    nodes: Vec<ColliderTreeNode>,
    items: Vec<u32>,
}

#[derive(Clone, Copy, Debug)]
struct ColliderTreeNode {
    bounds: Aabb,
    /// Лист: начало диапазона в `items`. Внутренний узел: индекс правого потомка; левый
    /// потомок следует сразу за узлом.
    start: u32,
    /// Число элементов листа; у внутреннего узла 0.
    count: u32,
}

impl ColliderTree {
    fn build(keys: Vec<(BodyHandle, Aabb)>) -> Self {
        let mut tree = Self {
            nodes: Vec::with_capacity(2 * keys.len().div_ceil(COLLIDER_TREE_LEAF)),
            items: (0..keys.len() as u32).collect(),
            keys,
        };
        if !tree.keys.is_empty() {
            tree.build_node(0, tree.items.len());
        }
        tree
    }

    /// Делит элементы по медиане центров вдоль оси наибольшего разброса: глубина не больше
    /// `log2(n)`, поэтому рекурсия ограничена лимитом коллайдеров снимка.
    fn build_node(&mut self, start: usize, end: usize) {
        let keys = &self.keys;
        let items = &mut self.items[start..end];
        let bounds = items
            .iter()
            .map(|&item| keys[item as usize].1)
            .reduce(Aabb::union)
            .expect("BVH node has at least one collider");
        let node = self.nodes.len();
        self.nodes.push(ColliderTreeNode {
            bounds,
            start: start as u32,
            count: (end - start) as u32,
        });
        if end - start <= COLLIDER_TREE_LEAF {
            return;
        }
        let (low, high) = items.iter().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(low, high), &item| {
                let center = keys[item as usize].1.center();
                (low.min(center), high.max(center))
            },
        );
        let extent = high - low;
        let axis = if extent.x >= extent.y && extent.x >= extent.z {
            0
        } else if extent.y >= extent.z {
            1
        } else {
            2
        };
        let middle = (end - start) / 2;
        items.select_nth_unstable_by(middle, |&left, &right| {
            keys[left as usize].1.center()[axis]
                .total_cmp(&keys[right as usize].1.center()[axis])
                .then_with(|| left.cmp(&right))
        });
        self.build_node(start, start + middle);
        let right = self.nodes.len() as u32;
        self.build_node(start + middle, end);
        self.nodes[node].start = right;
        self.nodes[node].count = 0;
    }

    /// Передаёт `visit` элементы, чьи AABB пересекают `query`, в порядке обхода.
    fn query(&self, query: Aabb, mut visit: impl FnMut(u32)) {
        if self.nodes.is_empty() {
            return;
        }
        // Глубина дерева не больше log2(MAX_QUERY_CANDIDATES), стек не растёт сверх неё.
        let mut stack = Vec::with_capacity(32);
        stack.push(0_u32);
        while let Some(index) = stack.pop() {
            let node = self.nodes[index as usize];
            if !node.bounds.overlaps(query) {
                continue;
            }
            if node.count == 0 {
                stack.push(node.start);
                stack.push(index + 1);
                continue;
            }
            let leaf = &self.items[node.start as usize..(node.start + node.count) as usize];
            for &item in leaf {
                if self.keys[item as usize].1.overlaps(query) {
                    visit(item);
                }
            }
        }
    }
}

fn cast_against_mesh(
    shape: &Shape,
    pose: Pose,
    displacement: Vec3,
    skin: f32,
    mesh_pose: Pose,
    mesh: &TriMesh,
    sweep_bounds: Aabb,
) -> Result<Option<(f32, DistanceResult, Option<u32>)>, QueryError> {
    let tolerance = cast_tolerance(pose.position, displacement);
    let mesh_inverse = mesh_pose.rotation.inverse();
    let local_corners = sweep_bounds
        .corners()
        .map(|point| mesh_inverse * (point - mesh_pose.position));
    let local_bounds = Aabb::from_points(&local_corners)?;
    let candidates = mesh.query_aabb(local_bounds);
    if candidates.len() > MAX_MESH_CANDIDATES {
        return Err(QueryError::CandidateLimit);
    }
    let mut closest: Option<(f32, DistanceResult, Option<u32>)> = None;
    for triangle_index in candidates {
        let indices = mesh.triangles()[triangle_index as usize];
        let vertices = indices
            .map(|index| mesh_pose.position + mesh_pose.rotation * mesh.vertices()[index as usize]);
        let triangle_normal = (vertices[1] - vertices[0])
            .cross(vertices[2] - vertices[0])
            .try_normalize()
            .ok_or(GeometryError::DegenerateTriangle)?;
        let Some((fraction, distance)) =
            conservative_advance(displacement, skin, tolerance, |offset| {
                let cast_pose = Pose::new(pose.position + offset, pose.rotation)?;
                let mut distance = convex_triangle_distance(shape, cast_pose, vertices)?;
                if distance.distance <= skin {
                    let center = (vertices[0] + vertices[1] + vertices[2]) / 3.0;
                    let outward = if (cast_pose.position - center).dot(triangle_normal) >= 0.0 {
                        triangle_normal
                    } else {
                        -triangle_normal
                    };
                    distance.normal = -outward;
                }
                Ok(distance)
            })?
        else {
            continue;
        };
        if closest.is_none_or(
            |(old_fraction, _, old_triangle): (f32, DistanceResult, Option<u32>)| {
                fraction < old_fraction
                    || (fraction == old_fraction
                        && triangle_index < old_triangle.unwrap_or(u32::MAX))
            },
        ) {
            closest = Some((fraction, distance, Some(triangle_index)));
        }
    }
    Ok(closest)
}

/// Допуск сходимости: не меньше абсолютного минимума и не меньше нескольких ulp координат
/// (|x|≈500 м даёт шаг float ≈3e-5), иначе контакт на skin срывается в шум округления.
fn cast_tolerance(position: Vec3, displacement: Vec3) -> f32 {
    let scale = position
        .abs()
        .max_element()
        .max(displacement.abs().max_element())
        .max(1.0);
    (8.0 * f32::EPSILON * scale).max(CAST_DISTANCE_TOLERANCE)
}

fn conservative_advance(
    displacement: Vec3,
    skin: f32,
    tolerance: f32,
    mut distance_at: impl FnMut(Vec3) -> Result<DistanceResult, QueryError>,
) -> Result<Option<(f32, DistanceResult)>, QueryError> {
    let mut fraction = 0.0_f64;
    for _ in 0..MAX_SHAPE_CAST_ITERATIONS {
        let offset = displacement * fraction as f32;
        let distance = distance_at(offset)?;
        if !distance.distance.is_finite() || !distance.normal.is_finite() {
            return Err(QueryError::InvalidCast);
        }
        if distance.distance <= skin + tolerance {
            // Касательное движение вдоль поверхности в пределах skin не попадание: шум нормали
            // (~1e-7) иначе даёт попадание на fraction 0, которое перекрывает настоящее препятствие.
            // Заход остатка смещения за разделяющую плоскость ограничен половиной зазора, поэтому
            // длинный почти касательный бросок не входит в выпуклое препятствие.
            let approach = displacement.dot(distance.normal);
            let grazing = approach <= GRAZING_APPROACH * displacement.length()
                && approach * (1.0 - fraction as f32) <= 0.5 * distance.distance;
            return Ok((!grazing && approach > 0.0).then_some((fraction as f32, distance)));
        }
        let closing_speed = displacement.dot(distance.normal) as f64;
        if closing_speed <= 0.0 {
            return Ok(None);
        }
        let advance = (distance.distance as f64 - skin as f64) / closing_speed;
        if !advance.is_finite() || advance <= 0.0 {
            return Err(QueryError::InvalidCast);
        }
        fraction += advance;
        if fraction > 1.0 {
            return Ok(None);
        }
    }
    Err(QueryError::ShapeCastIterationLimit)
}

fn expand_aabb(bounds: Aabb, amount: f32) -> Result<Aabb, GeometryError> {
    let expansion = Vec3::splat(amount);
    Aabb::new(bounds.min - expansion, bounds.max + expansion)
}

fn query_hit_precedes(candidate: QueryHit, current: QueryHit) -> bool {
    candidate
        .fraction
        .total_cmp(&current.fraction)
        .then_with(|| candidate.body.cmp(&current.body))
        .is_lt()
}

#[cfg(test)]
mod tests {
    use super::{QueryFilter, QueryOverrides, QuerySnapshot};
    use crate::world::{
        BodyDesc, BodyHandle, BodyKind, ColliderDesc, PHYSICS_LAYER_COUNT, PhysicsWorld,
    };
    use crate::{Aabb, DistanceResult, Hull, Pose, Ray, Shape, TriMesh};
    use glam::{Quat, Vec3};
    use std::sync::Arc;

    fn floor_mesh() -> Shape {
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

    #[test]
    fn snapshot_raycast_filters_disabled_ignored_and_trigger_colliders() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut trigger = BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        let trigger_handle = world.insert_body(trigger).unwrap();
        let mut disabled_desc = BodyDesc::static_collider(
            Shape::sphere(0.25).unwrap(),
            Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        disabled_desc.collider.enabled = false;
        world.insert_body(disabled_desc).unwrap();
        let wall = world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::splat(0.25)).unwrap(),
                Pose::new(Vec3::new(4.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let ray = Ray::new(Vec3::ZERO, Vec3::X, 6.0).unwrap();

        let default_hit = snapshot
            .raycast(&ray, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(default_hit.body, wall);
        let trigger_hit = snapshot
            .raycast(
                &ray,
                QueryFilter {
                    ignore: None,
                    include_triggers: true,
                    ..QueryFilter::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(trigger_hit.body, trigger_handle);
        let ignored_hit = snapshot
            .raycast(
                &ray,
                QueryFilter {
                    ignore: Some(wall),
                    include_triggers: false,
                    ..QueryFilter::default()
                },
            )
            .unwrap();
        assert!(ignored_hit.is_none());
    }

    #[test]
    fn reversed_enabled_overrides_filter_rays_and_shape_casts() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let first = world
            .insert_body(BodyDesc::static_collider(
                Shape::sphere(0.25).unwrap(),
                Pose::new(Vec3::X, Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let second = world
            .insert_body(BodyDesc::static_collider(
                Shape::sphere(0.25).unwrap(),
                Pose::new(Vec3::X * 2.0, Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let enabled = [(second, false), (first, false)];
        let ray = Ray::new(Vec3::ZERO, Vec3::X, 3.0).unwrap();

        assert!(
            snapshot
                .raycast_with_enabled_overrides(&ray, QueryFilter::default(), &[], &enabled)
                .unwrap()
                .is_none()
        );
        assert!(
            snapshot
                .shape_cast_with_overrides(
                    &Shape::sphere(0.1).unwrap(),
                    Pose::IDENTITY,
                    Vec3::X * 3.0,
                    0.0,
                    QueryFilter::default(),
                    super::QueryOverrides {
                        excluded: &[],
                        enabled: &enabled,
                    },
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn shape_cast_crosses_mesh_floor_geometry_and_respects_movement_filters() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let floor = world
            .insert_body(BodyDesc::static_collider(floor_mesh(), Pose::IDENTITY))
            .unwrap();
        let mut trigger = BodyDesc::static_collider(
            Shape::cuboid(Vec3::new(0.2, 0.4, 1.0)).unwrap(),
            Pose::new(Vec3::new(1.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        world.insert_body(trigger).unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let capsule = Shape::capsule(0.25, 0.5).unwrap();
        let pose = Pose::new(Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY).unwrap();
        let floor_hit = snapshot
            .shape_cast(&capsule, pose, -Vec3::Y, 0.01, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(floor_hit.body, floor);
        assert_eq!(floor_hit.triangle_index, Some(0));
        assert!(floor_hit.normal.dot(-Vec3::Y) > 0.99, "{floor_hit:?}");

        let horizontal = snapshot
            .shape_cast(
                &capsule,
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::X * 2.0,
                0.0,
                QueryFilter::default(),
            )
            .unwrap();
        assert!(horizontal.is_none());
    }

    #[test]
    fn conservative_advance_stops_at_skin_contact_despite_float_noise() {
        // Расстояние на контакте чуть больше skin из-за округления (0.020000001 при skin 0.02):
        // без допуска каждая итерация продвигает луч на доли 1e-9 и лимит итераций исчерпывается.
        let skin = 0.02;
        let contact = super::conservative_advance(
            Vec3::new(0.0, 0.0, -0.0667),
            skin,
            super::CAST_DISTANCE_TOLERANCE,
            |_| {
                Ok(DistanceResult {
                    distance: skin + 1.0e-7,
                    normal: Vec3::NEG_Z,
                    point_a: Vec3::ZERO,
                    point_b: Vec3::ZERO,
                })
            },
        )
        .expect("cast must converge at skin contact");
        let (fraction, _) = contact.expect("approaching contact must be reported");
        assert_eq!(fraction, 0.0);
    }

    #[test]
    fn grazing_motion_along_a_touching_surface_is_not_a_hit() {
        // Капсула стоит на полу в пределах skin и идёт вдоль него; шум нормали пола даёт
        // крошечную положительную проекцию. Это скольжение, а не попадание на fraction 0.
        let skin = 0.02;
        let noisy_floor = DistanceResult {
            distance: skin,
            normal: Vec3::new(3.0e-7, -1.0, 0.0).normalize(),
            point_a: Vec3::ZERO,
            point_b: Vec3::ZERO,
        };
        let along = super::conservative_advance(
            Vec3::X * 0.066,
            skin,
            super::CAST_DISTANCE_TOLERANCE,
            |_| Ok(noisy_floor),
        )
        .unwrap();
        assert!(along.is_none(), "{along:?}");
        let into = super::conservative_advance(
            Vec3::new(0.066, -0.003, 0.0),
            skin,
            super::CAST_DISTANCE_TOLERANCE,
            |_| Ok(noisy_floor),
        )
        .unwrap();
        assert_eq!(into.map(|(fraction, _)| fraction), Some(0.0));
        // Длинный бросок под тем же малым углом зашёл бы в пол глубже зазора: это попадание.
        let long = super::conservative_advance(
            Vec3::new(50.0, -0.04, 0.0),
            skin,
            super::CAST_DISTANCE_TOLERANCE,
            |_| Ok(noisy_floor),
        )
        .unwrap();
        assert_eq!(long.map(|(fraction, _)| fraction), Some(0.0));
    }

    #[test]
    fn conservative_advance_converges_on_float_noise_far_from_origin() {
        // При |x|≈500 м шаг float ≈3e-5 больше абсолютного допуска 1e-5: расстояние остаётся
        // skin + шум при любом смещении и без относительного допуска исчерпывает лимит итераций.
        let skin = 0.02;
        let displacement = Vec3::new(0.0, 0.0, -0.0667);
        let tolerance = super::cast_tolerance(Vec3::new(500.2, 0.0, 0.0), displacement);
        let contact = super::conservative_advance(displacement, skin, tolerance, |_| {
            Ok(DistanceResult {
                distance: skin + 3.05e-5,
                normal: Vec3::NEG_Z,
                point_a: Vec3::ZERO,
                point_b: Vec3::ZERO,
            })
        })
        .expect("cast must converge at skin contact far from origin");
        let (fraction, _) = contact.expect("approaching contact must be reported");
        assert_eq!(fraction, 0.0);
    }

    #[test]
    fn body_lookups_find_each_of_several_bodies_and_report_unknown_handles() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        // Слои 0 и 1 сталкиваются между собой, слой 2 — только сам с собой.
        let mut masks = [u32::MAX; PHYSICS_LAYER_COUNT];
        masks[0] = 0b011;
        masks[1] = 0b011;
        masks[2] = 0b100;
        world.set_collision_matrix(masks);
        let mut handles = Vec::new();
        for (index, layer) in [0u32, 1, 2].into_iter().enumerate() {
            let pose = Pose::new(Vec3::new(index as f32 * 3.0, 0.0, 0.0), Quat::IDENTITY).unwrap();
            let mut desc = BodyDesc::dynamic(Shape::sphere(0.5).unwrap(), pose, 1.0 + index as f32);
            desc.layer = layer;
            handles.push(world.insert_body(desc).unwrap());
        }
        let snapshot = world.query_snapshot().unwrap();

        for (index, &handle) in handles.iter().enumerate() {
            let state = snapshot.body_state(handle).unwrap();
            assert_eq!(state.pose.position, Vec3::new(index as f32 * 3.0, 0.0, 0.0));
            assert!((state.mass - (1.0 + index as f32)).abs() < 1.0e-5);
        }
        assert_eq!(snapshot.collision_layer_mask(handles[0]), 0b011);
        assert_eq!(snapshot.collision_layer_mask(handles[1]), 0b011);
        assert_eq!(snapshot.collision_layer_mask(handles[2]), 0b100);

        let missing = BodyHandle(u64::MAX);
        assert!(snapshot.body_state(missing).is_none());
        assert_eq!(snapshot.collision_layer_mask(missing), u32::MAX);
    }

    /// xorshift64*: воспроизводимые случайные сцены без внешних зависимостей.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn unit(&mut self) -> f32 {
            (self.next() >> 40) as f32 / (1_u64 << 24) as f32
        }

        fn range(&mut self, low: f32, high: f32) -> f32 {
            low + (high - low) * self.unit()
        }

        fn below(&mut self, count: usize) -> usize {
            (self.next() % count as u64) as usize
        }

        fn point(&mut self, extent: f32) -> Vec3 {
            Vec3::new(
                self.range(-extent, extent),
                self.range(-extent, extent),
                self.range(-extent, extent),
            )
        }

        fn pose(&mut self, extent: f32) -> Pose {
            let rotation = Quat::from_euler(
                glam::EulerRot::XYZ,
                self.range(-3.0, 3.0),
                self.range(-3.0, 3.0),
                self.range(-3.0, 3.0),
            );
            Pose::new(self.point(extent), rotation).unwrap()
        }

        fn convex_shape(&mut self) -> Shape {
            match self.below(4) {
                0 => Shape::sphere(self.range(0.2, 2.0)).unwrap(),
                1 => Shape::cuboid(Vec3::new(
                    self.range(0.2, 2.0),
                    self.range(0.2, 2.0),
                    self.range(0.2, 2.0),
                ))
                .unwrap(),
                2 => Shape::capsule(self.range(0.2, 1.0), self.range(0.2, 2.0)).unwrap(),
                _ => {
                    let size = self.range(0.5, 2.0);
                    let corners = Aabb {
                        min: Vec3::splat(-size),
                        max: Vec3::splat(size),
                    }
                    .corners()
                    .map(|corner| corner + self.point(0.2 * size));
                    Shape::ConvexHull(Arc::new(Hull::from_points(&corners).unwrap()))
                }
            }
        }

        fn quad_mesh(&mut self) -> Shape {
            let half = self.range(1.0, 4.0);
            Shape::TriMesh(Arc::new(
                TriMesh::new(
                    vec![
                        Vec3::new(-half, 0.0, -half),
                        Vec3::new(half, 0.0, -half),
                        Vec3::new(half, 0.0, half),
                        Vec3::new(-half, 0.0, half),
                    ],
                    vec![[0, 1, 2], [0, 2, 3]],
                )
                .unwrap(),
            ))
        }
    }

    const SCENE_EXTENT: f32 = 25.0;

    /// Static, kinematic и dynamic тела всех форм, включая выключенные, trigger и разные слои.
    fn random_world(rng: &mut Rng, bodies: usize) -> (PhysicsWorld, Vec<BodyHandle>) {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut handles = Vec::new();
        for _ in 0..bodies {
            let pose = rng.pose(SCENE_EXTENT);
            let kind = match rng.below(10) {
                0..5 => BodyKind::Static,
                5..7 => BodyKind::Kinematic,
                _ => BodyKind::Dynamic,
            };
            let shape = if kind == BodyKind::Static && rng.below(5) == 0 {
                rng.quad_mesh()
            } else {
                rng.convex_shape()
            };
            let mut desc = BodyDesc::new(kind, pose, ColliderDesc::new(shape));
            desc.collider.enabled = rng.below(7) != 0;
            desc.collider.trigger = rng.below(7) == 0;
            desc.layer = rng.below(4) as u32;
            handles.push(world.insert_body(desc).unwrap());
        }
        (world, handles)
    }

    fn random_filter(rng: &mut Rng, handles: &[BodyHandle]) -> QueryFilter {
        QueryFilter {
            ignore: (!handles.is_empty() && rng.below(3) == 0)
                .then(|| handles[rng.below(handles.len())]),
            include_triggers: rng.below(2) == 0,
            layer_mask: if rng.below(2) == 0 {
                u32::MAX
            } else {
                rng.next() as u32 & 0b1111
            },
        }
    }

    fn random_overrides(
        rng: &mut Rng,
        handles: &[BodyHandle],
    ) -> (Vec<BodyHandle>, Vec<(BodyHandle, bool)>) {
        if handles.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let excluded = (0..rng.below(4))
            .map(|_| handles[rng.below(handles.len())])
            .collect();
        let enabled = (0..rng.below(6))
            .map(|_| (handles[rng.below(handles.len())], rng.below(2) == 0))
            .collect();
        (excluded, enabled)
    }

    fn linear_reference(snapshot: &QuerySnapshot) -> QuerySnapshot {
        let mut reference = snapshot.clone();
        reference.linear_reference = true;
        reference
    }

    /// Запросы всех типов через BVH и через прежний линейный проход дают одинаковые результаты,
    /// включая ошибки; возвращает число непустых результатов по типам.
    fn assert_queries_match_linear(
        rng: &mut Rng,
        snapshot: &QuerySnapshot,
        handles: &[BodyHandle],
        queries: usize,
    ) -> [usize; 6] {
        let reference = linear_reference(snapshot);
        let mut nonempty = [0; 6];
        for query in 0..queries {
            let filter = random_filter(rng, handles);
            let (excluded, enabled) = random_overrides(rng, handles);
            let overrides = QueryOverrides {
                excluded: &excluded,
                enabled: &enabled,
            };

            let center = rng.point(SCENE_EXTENT);
            let bounds = Aabb {
                min: center - Vec3::splat(rng.range(0.0, 6.0)),
                max: center + Vec3::splat(rng.range(0.0, 6.0)),
            };
            let candidates = snapshot.candidates(bounds, filter, overrides);
            assert_eq!(
                candidates,
                reference.candidates(bounds, filter, overrides),
                "candidates, query {query}"
            );
            nonempty[0] += candidates.is_ok_and(|found| !found.is_empty()) as usize;

            let direction = rng.point(1.0).try_normalize().unwrap_or(Vec3::X);
            // Короткие лучи у края сцены и длинные лучи через всю сцену.
            let ray = Ray::new(
                rng.point(SCENE_EXTENT),
                direction,
                rng.range(0.0, 2.5 * SCENE_EXTENT),
            )
            .unwrap();
            let hit = snapshot.raycast_with_enabled_overrides(&ray, filter, &excluded, &enabled);
            assert_eq!(
                hit,
                reference.raycast_with_enabled_overrides(&ray, filter, &excluded, &enabled),
                "raycast, query {query}"
            );
            nonempty[1] += matches!(hit, Ok(Some(_))) as usize;
            let hits = snapshot.raycast_all(&ray, filter, overrides);
            assert_eq!(
                hits,
                reference.raycast_all(&ray, filter, overrides),
                "raycast_all, query {query}"
            );
            nonempty[2] += hits.is_ok_and(|hits| !hits.is_empty()) as usize;

            let shape = rng.convex_shape();
            let pose = rng.pose(SCENE_EXTENT);
            let overlap = snapshot.overlap(&shape, pose, filter, overrides);
            assert_eq!(
                overlap,
                reference.overlap(&shape, pose, filter, overrides),
                "overlap, query {query}"
            );
            nonempty[3] += overlap.is_ok_and(|bodies| !bodies.is_empty()) as usize;
            let penetration = snapshot.deepest_static_penetration(&shape, pose, filter, overrides);
            assert_eq!(
                penetration,
                reference.deepest_static_penetration(&shape, pose, filter, overrides),
                "deepest_static_penetration, query {query}"
            );
            nonempty[4] += matches!(penetration, Ok(Some(_))) as usize;

            let displacement = rng.point(10.0);
            let skin = rng.range(0.0, 0.05);
            let cast = snapshot.shape_cast_with_overrides(
                &shape,
                pose,
                displacement,
                skin,
                filter,
                overrides,
            );
            assert_eq!(
                cast,
                reference.shape_cast_with_overrides(
                    &shape,
                    pose,
                    displacement,
                    skin,
                    filter,
                    overrides
                ),
                "shape_cast, query {query}"
            );
            nonempty[5] += matches!(cast, Ok(Some(_))) as usize;
        }
        nonempty
    }

    #[test]
    fn bvh_queries_match_linear_pass_on_random_scenes() {
        let mut nonempty = [0; 6];
        for seed in [1_u64, 7, 42, 2024] {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let (world, handles) = random_world(&mut rng, 160);
            let snapshot = world.query_snapshot().unwrap();
            let counts = assert_queries_match_linear(&mut rng, &snapshot, &handles, 40);
            for (total, count) in nonempty.iter_mut().zip(counts) {
                *total += count;
            }
        }
        // Пустые результаты совпадают тривиально: каждый тип запроса должен что-то находить.
        assert!(nonempty.iter().all(|&count| count > 0), "{nonempty:?}");
    }

    #[test]
    fn queries_on_empty_and_single_kind_scenes_match_linear_pass() {
        let mut rng = Rng(0x5EED);
        let empty = PhysicsWorld::new(Vec3::ZERO)
            .unwrap()
            .query_snapshot()
            .unwrap();
        let counts = assert_queries_match_linear(&mut rng, &empty, &[], 10);
        assert_eq!(counts, [0; 6]);

        // Только dynamic (пустое static-дерево) и только static (пустое dynamic-дерево).
        for kind in [BodyKind::Dynamic, BodyKind::Static] {
            let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
            let handles = (0..40)
                .map(|_| {
                    let desc = BodyDesc::new(
                        kind,
                        rng.pose(SCENE_EXTENT),
                        ColliderDesc::new(rng.convex_shape()),
                    );
                    world.insert_body(desc).unwrap()
                })
                .collect::<Vec<_>>();
            let snapshot = world.query_snapshot().unwrap();
            let counts = assert_queries_match_linear(&mut rng, &snapshot, &handles, 20);
            assert!(counts[0] > 0, "{kind:?}: {counts:?}");
        }
    }

    #[test]
    fn short_ray_at_far_edge_of_long_row_hits_the_nearest_box() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let boxes = (0..2_000)
            .map(|index| {
                world
                    .insert_body(BodyDesc::static_collider(
                        Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                        Pose::new(Vec3::new(index as f32 * 2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                    ))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let snapshot = world.query_snapshot().unwrap();
        let ray = Ray::new(Vec3::new(3_998.0, 2.0, 0.0), -Vec3::Y, 2.0).unwrap();

        let hit = snapshot
            .raycast(&ray, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(hit.body, boxes[1_999]);
        assert!((hit.distance - 1.5).abs() < 1.0e-4, "{hit:?}");
        let between = Ray::new(Vec3::new(3_997.0, 2.0, 0.0), -Vec3::Y, 2.0).unwrap();
        assert!(
            snapshot
                .raycast(&between, QueryFilter::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn static_tree_is_reused_until_static_colliders_change() {
        let mut world = PhysicsWorld::new(Vec3::new(0.0, -9.81, 0.0)).unwrap();
        let floor = world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::new(10.0, 0.5, 10.0)).unwrap(),
                Pose::new(Vec3::new(0.0, -0.5, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let mut disabled = BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(5.0, 3.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        disabled.collider.enabled = false;
        let disabled = world.insert_body(disabled).unwrap();
        let ball = world
            .insert_body(BodyDesc::dynamic(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(0.0, 5.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let first = world.query_snapshot().unwrap();
        world.step_fixed(1.0 / 60.0).unwrap();
        let second = world.query_snapshot().unwrap();

        // Движется только dynamic-тело: static-дерево общее, dynamic-часть видит новую позу.
        assert!(Arc::ptr_eq(first.static_tree(), second.static_tree()));
        assert_ne!(
            first.body_state(ball).unwrap().pose,
            second.body_state(ball).unwrap().pose
        );
        let down = Ray::new(Vec3::new(0.0, 10.0, 0.0), -Vec3::Y, 20.0).unwrap();
        let hit = second
            .raycast(&down, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(hit.body, ball);
        let expected = 10.0 - second.body_state(ball).unwrap().pose.position.y - 0.5;
        assert!((hit.distance - expected).abs() < 1.0e-4, "{hit:?}");

        // Выключенный static-коллайдер остаётся в дереве: override может включить его.
        let side = Ray::new(Vec3::new(5.0, 10.0, 0.0), -Vec3::Y, 20.0).unwrap();
        let default_hit = second
            .raycast(&side, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(default_hit.body, floor);
        let enabled_hit = second
            .raycast_with_enabled_overrides(&side, QueryFilter::default(), &[], &[(disabled, true)])
            .unwrap()
            .unwrap();
        assert_eq!(enabled_hit.body, disabled);

        let wall = world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                Pose::new(Vec3::new(-5.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let inserted = world.query_snapshot().unwrap();
        assert!(!Arc::ptr_eq(second.static_tree(), inserted.static_tree()));
        let left = Ray::new(Vec3::new(-5.0, 10.0, 0.0), -Vec3::Y, 20.0).unwrap();
        assert_eq!(
            inserted
                .raycast(&left, QueryFilter::default())
                .unwrap()
                .unwrap()
                .body,
            wall
        );

        // Тот же хендл на новом месте: ключ дерева включает AABB, поэтому оно перестраивается.
        world.remove_body(wall).unwrap();
        world
            .insert_body_with_handle(
                wall,
                BodyDesc::static_collider(
                    Shape::cuboid(Vec3::splat(0.5)).unwrap(),
                    Pose::new(Vec3::new(-8.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                ),
            )
            .unwrap();
        let moved = world.query_snapshot().unwrap();
        assert!(!Arc::ptr_eq(inserted.static_tree(), moved.static_tree()));
        let far_left = Ray::new(Vec3::new(-8.0, 10.0, 0.0), -Vec3::Y, 20.0).unwrap();
        assert_eq!(
            moved
                .raycast(&far_left, QueryFilter::default())
                .unwrap()
                .unwrap()
                .body,
            wall
        );
        assert_eq!(
            moved
                .raycast(&left, QueryFilter::default())
                .unwrap()
                .unwrap()
                .body,
            floor
        );

        world.remove_body(wall).unwrap();
        let removed = world.query_snapshot().unwrap();
        assert!(!Arc::ptr_eq(moved.static_tree(), removed.static_tree()));
        assert_eq!(
            removed
                .raycast(&left, QueryFilter::default())
                .unwrap()
                .unwrap()
                .body,
            floor
        );
        let mut rng = Rng(0xB0D1E5);
        let handles = [floor, disabled, ball];
        assert_queries_match_linear(&mut rng, &removed, &handles, 20);
    }
}
