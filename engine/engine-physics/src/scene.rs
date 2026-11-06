use std::collections::HashMap;
use std::fmt;

use engine_scene::{
    CharacterControllerComponent, ColliderComponent, ColliderShape, ObjectHandle,
    RigidBodyComponent, RigidBodyKind, World,
};
use glam::{Mat3, Vec3};

use crate::character::{CharacterController, CharacterError};
use crate::hull::Hull;
use crate::world::{
    BodyDesc, BodyHandle, BodyKind, ColliderDesc, PhysicsCommand, PhysicsWorld, PhysicsWorldError,
    StepDriver,
};
use crate::{GeometryError, Pose, Shape, TriMesh};

const POSE_POSITION_EPSILON_SQUARED: f32 = 1.0e-8;
const POSE_ROTATION_DOT_EPSILON: f32 = 1.0e-5;

#[derive(Clone, Debug, PartialEq)]
pub struct ModelMesh {
    pub positions: Vec<Vec3>,
    pub indices: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelGeometry {
    pub path: String,
    pub meshes: Vec<ModelMesh>,
}

#[derive(Clone, Debug)]
pub struct PhysicsSceneBody {
    pub object: ObjectHandle,
    pub desc: BodyDesc,
    pub character_controller: Option<CharacterController>,
}

#[derive(Clone, Debug)]
pub struct PhysicsSceneDesc {
    pub gravity: Vec3,
    pub fixed_hz: u32,
    pub max_substeps: usize,
    pub bodies: Vec<PhysicsSceneBody>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsSceneBinding {
    pub object: ObjectHandle,
    pub body: BodyHandle,
    pub kind: BodyKind,
    pub character_controller: Option<CharacterController>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PhysicsSceneError {
    Geometry(GeometryError),
    Character(CharacterError),
    PhysicsWorld(PhysicsWorldError),
    MissingObject(ObjectHandle),
    InvalidObjectTransform(ObjectHandle),
    DynamicObjectMustBeRoot(ObjectHandle),
    CharacterObjectMustBeRoot(ObjectHandle),
    ConflictingComponents(ObjectHandle),
    MissingModelGeometry(String),
    InvalidModelGeometry(String),
    ModelPathRepeated(String),
}

impl fmt::Display for PhysicsSceneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::Character(error) => error.fmt(formatter),
            Self::PhysicsWorld(error) => error.fmt(formatter),
            Self::MissingObject(handle) => {
                write!(formatter, "unknown scene object {}", handle.index())
            }
            Self::InvalidObjectTransform(handle) => {
                write!(
                    formatter,
                    "scene object {} has a zero or invalid collider scale",
                    handle.index()
                )
            }
            Self::DynamicObjectMustBeRoot(handle) => write!(
                formatter,
                "dynamic scene object {} must be a root",
                handle.index()
            ),
            Self::CharacterObjectMustBeRoot(handle) => write!(
                formatter,
                "character scene object {} must be a root",
                handle.index()
            ),
            Self::ConflictingComponents(handle) => write!(
                formatter,
                "scene object {} has incompatible physics components",
                handle.index()
            ),
            Self::MissingModelGeometry(path) => {
                write!(formatter, "missing CPU model geometry for {path:?}")
            }
            Self::InvalidModelGeometry(path) => {
                write!(formatter, "invalid CPU model geometry for {path:?}")
            }
            Self::ModelPathRepeated(path) => {
                write!(formatter, "CPU model geometry path {path:?} is repeated")
            }
        }
    }
}

impl std::error::Error for PhysicsSceneError {}

impl From<GeometryError> for PhysicsSceneError {
    fn from(value: GeometryError) -> Self {
        Self::Geometry(value)
    }
}

impl From<CharacterError> for PhysicsSceneError {
    fn from(value: CharacterError) -> Self {
        Self::Character(value)
    }
}

impl From<PhysicsWorldError> for PhysicsSceneError {
    fn from(value: PhysicsWorldError) -> Self {
        Self::PhysicsWorld(value)
    }
}

impl PhysicsSceneDesc {
    pub fn from_world(
        world: &World,
        model_geometry: &[ModelGeometry],
        gravity: Vec3,
        fixed_hz: u32,
        max_substeps: usize,
    ) -> Result<Self, PhysicsSceneError> {
        let mut models = HashMap::with_capacity(model_geometry.len());
        for model in model_geometry {
            if models.insert(model.path.as_str(), model).is_some() {
                return Err(PhysicsSceneError::ModelPathRepeated(model.path.clone()));
            }
        }

        let mut bodies = Vec::new();
        for object in world.objects() {
            let Some(collider) = object.collider else {
                if object.character_controller.is_none() {
                    continue;
                }
                let transform = object_world_transform(world, object.handle)?;
                let settings = object
                    .character_controller
                    .expect("checked character controller presence");
                if object.parent.is_some() {
                    return Err(PhysicsSceneError::CharacterObjectMustBeRoot(object.handle));
                }
                if object.rigid_body.is_some() {
                    return Err(PhysicsSceneError::ConflictingComponents(object.handle));
                }
                let radial_scale = transform.scale.x.abs().max(transform.scale.z.abs());
                let controller = scale_character(settings, radial_scale, transform.scale)?;
                let shape = controller.capsule()?;
                let mut desc = BodyDesc::new(
                    BodyKind::Kinematic,
                    transform.pose,
                    ColliderDesc::new(shape),
                );
                desc.character_controller = true;
                bodies.push(PhysicsSceneBody {
                    object: object.handle,
                    desc,
                    character_controller: Some(controller),
                });
                continue;
            };

            if object.character_controller.is_some() {
                return Err(PhysicsSceneError::ConflictingComponents(object.handle));
            }
            let transform = object_world_transform(world, object.handle)?;
            let rigid_body = object.rigid_body;
            let kind = rigid_body.map_or(BodyKind::Static, body_kind);
            if kind == BodyKind::Dynamic && object.parent.is_some() {
                return Err(PhysicsSceneError::DynamicObjectMustBeRoot(object.handle));
            }
            let shape = build_shape(
                collider,
                transform.scale,
                object.mesh_renderer.as_ref(),
                &models,
            )?;
            let mut collider_desc = ColliderDesc::new(shape);
            collider_desc.offset = Vec3::from_array(collider.offset) * transform.scale;
            collider_desc.trigger = collider.trigger;
            collider_desc.friction = collider.friction;
            collider_desc.restitution = collider.restitution;
            let mut desc = BodyDesc::new(kind, transform.pose, collider_desc);
            apply_rigid_body(&mut desc, rigid_body);
            bodies.push(PhysicsSceneBody {
                object: object.handle,
                desc,
                character_controller: None,
            });
        }

        Ok(Self {
            gravity,
            fixed_hz,
            max_substeps,
            bodies,
        })
    }

    pub fn build(self) -> Result<(StepDriver, Vec<PhysicsSceneBinding>), PhysicsSceneError> {
        let mut world = PhysicsWorld::new(self.gravity)?;
        let mut bindings = Vec::with_capacity(self.bodies.len());
        for scene_body in self.bodies {
            let kind = scene_body.desc.kind;
            let body = world.insert_body(scene_body.desc)?;
            bindings.push(PhysicsSceneBinding {
                object: scene_body.object,
                body,
                kind,
                character_controller: scene_body.character_controller,
            });
        }
        let driver = StepDriver::new(world, self.fixed_hz, self.max_substeps)?;
        Ok((driver, bindings))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PhysicsSceneSyncError {
    Scene(PhysicsSceneError),
    MissingObject(ObjectHandle),
    InvalidPose(ObjectHandle),
}

impl fmt::Display for PhysicsSceneSyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scene(error) => error.fmt(formatter),
            Self::MissingObject(handle) => {
                write!(formatter, "unknown scene object {}", handle.index())
            }
            Self::InvalidPose(handle) => write!(
                formatter,
                "scene object {} has an invalid world pose",
                handle.index()
            ),
        }
    }
}

impl std::error::Error for PhysicsSceneSyncError {}

impl From<PhysicsSceneError> for PhysicsSceneSyncError {
    fn from(value: PhysicsSceneError) -> Self {
        Self::Scene(value)
    }
}

#[derive(Clone, Debug)]
pub struct PhysicsSceneSync {
    bindings: Vec<PhysicsSceneBinding>,
    binding_indices: HashMap<BodyHandle, usize>,
    last_synced: Vec<Pose>,
}

impl PhysicsSceneSync {
    pub fn new(
        world: &World,
        bindings: Vec<PhysicsSceneBinding>,
    ) -> Result<Self, PhysicsSceneSyncError> {
        let last_synced = bindings
            .iter()
            .map(|binding| world_pose(world, binding.object))
            .collect::<Result<Vec<_>, _>>()?;
        let binding_indices = bindings
            .iter()
            .enumerate()
            .map(|(index, binding)| (binding.body, index))
            .collect();
        Ok(Self {
            bindings,
            binding_indices,
            last_synced,
        })
    }

    pub fn bindings(&self) -> &[PhysicsSceneBinding] {
        &self.bindings
    }

    /// Applies dynamic body poses, then records the pose read back from the scene cache.
    pub fn apply_physics_poses(
        &mut self,
        world: &mut World,
        poses: &[(BodyHandle, Pose)],
    ) -> Result<(), PhysicsSceneSyncError> {
        for (body, pose) in poses {
            let Some(&index) = self.binding_indices.get(body) else {
                continue;
            };
            let binding = self.bindings[index];
            if binding.kind != BodyKind::Dynamic {
                continue;
            }
            world
                .set_root_pose(binding.object, pose.position, pose.rotation)
                .map_err(|_| PhysicsSceneSyncError::MissingObject(binding.object))?;
            self.last_synced[index] = world_pose(world, binding.object)?;
        }
        Ok(())
    }

    /// Converts changed scene transforms into teleport or kinematic-target commands.
    pub fn sync_world_commands(
        &mut self,
        world: &mut World,
    ) -> Result<Vec<PhysicsCommand>, PhysicsSceneSyncError> {
        world.recalculate_world_matrices();
        let mut commands = Vec::new();
        for (index, binding) in self.bindings.iter().enumerate() {
            let current = world_pose(world, binding.object)?;
            let always_target = binding.character_controller.is_some();
            if !always_target && !pose_changed(self.last_synced[index], current) {
                continue;
            }
            let command = match binding.kind {
                BodyKind::Static | BodyKind::Dynamic => PhysicsCommand::Teleport {
                    body: binding.body,
                    pose: current,
                },
                BodyKind::Kinematic => PhysicsCommand::KinematicTarget {
                    body: binding.body,
                    pose: current,
                },
            };
            commands.push(command);
            self.last_synced[index] = current;
        }
        Ok(commands)
    }
}

/// Мировая поза объекта и знаковый масштаб, которым физика запекает его коллайдеры.
#[derive(Clone, Copy)]
pub struct ObjectWorldTransform {
    pub pose: Pose,
    pub scale: Vec3,
}

/// Поза и масштаб объекта так же, как при построении сцены физики.
pub fn object_world_transform(
    world: &World,
    object: ObjectHandle,
) -> Result<ObjectWorldTransform, PhysicsSceneError> {
    let matrix = world
        .world_matrix(object)
        .ok_or(PhysicsSceneError::MissingObject(object))?;
    let rotation = world
        .world_rotation(object)
        .ok_or(PhysicsSceneError::MissingObject(object))?;
    let world_linear = Mat3::from_mat4(matrix);
    let rotation_basis = Mat3::from_quat(rotation);
    // Bake signed component scale in the cached rotation frame; off-diagonal
    // shear from rotated nonuniform ancestors is intentionally omitted.
    let scale = Vec3::new(
        rotation_basis.x_axis.dot(world_linear.x_axis),
        rotation_basis.y_axis.dot(world_linear.y_axis),
        rotation_basis.z_axis.dot(world_linear.z_axis),
    );
    if !matrix.is_finite() || !scale.is_finite() || scale.abs().max_element() == 0.0 {
        return Err(PhysicsSceneError::InvalidObjectTransform(object));
    }
    let pose = Pose::new(matrix.w_axis.truncate(), rotation)?;
    Ok(ObjectWorldTransform { pose, scale })
}

fn world_pose(world: &World, object: ObjectHandle) -> Result<Pose, PhysicsSceneSyncError> {
    let matrix = world
        .world_matrix(object)
        .ok_or(PhysicsSceneSyncError::MissingObject(object))?;
    let rotation = world
        .world_rotation(object)
        .ok_or(PhysicsSceneSyncError::MissingObject(object))?;
    Pose::new(matrix.w_axis.truncate(), rotation)
        .map_err(|_| PhysicsSceneSyncError::InvalidPose(object))
}

fn pose_changed(previous: Pose, current: Pose) -> bool {
    previous.position.distance_squared(current.position) > POSE_POSITION_EPSILON_SQUARED
        || previous.rotation.dot(current.rotation).abs() < 1.0 - POSE_ROTATION_DOT_EPSILON
}

fn body_kind(component: RigidBodyComponent) -> BodyKind {
    match component.kind {
        RigidBodyKind::Static => BodyKind::Static,
        RigidBodyKind::Kinematic => BodyKind::Kinematic,
        RigidBodyKind::Dynamic => BodyKind::Dynamic,
    }
}

fn apply_rigid_body(desc: &mut BodyDesc, component: Option<RigidBodyComponent>) {
    if let Some(component) = component {
        desc.mass = component.mass;
        desc.linear_damping = component.linear_damping;
        desc.angular_damping = component.angular_damping;
        desc.gravity_scale = component.gravity_scale;
        desc.lock_rotation = component.lock_rotation;
    }
}

fn scale_character(
    component: CharacterControllerComponent,
    radial_scale: f32,
    scale: Vec3,
) -> Result<CharacterController, CharacterError> {
    CharacterController::new(
        component.radius * radial_scale,
        component.height * scale.y.abs(),
        component.step_height * scale.y.abs(),
        component.max_slope_deg,
        component.snap_distance * scale.y.abs(),
        component.skin * scale.abs().max_element(),
        component.push_strength,
    )
}

/// Примитив коллайдера (`box`, `sphere`, `capsule`) с размерами, запечёнными по масштабу объекта.
/// Для `convex_mesh` и `triangle_mesh` возвращает ошибку: их строит [`PhysicsSceneDesc::from_world`].
pub fn primitive_collider_shape(
    component: ColliderComponent,
    scale: Vec3,
) -> Result<Shape, GeometryError> {
    match component.shape {
        ColliderShape::Box => {
            Shape::cuboid(Vec3::from_array(component.half_extents).abs() * scale.abs())
        }
        ColliderShape::Sphere => Shape::sphere(component.radius * scale.abs().max_element()),
        ColliderShape::Capsule => {
            let radial_scale = scale.x.abs().max(scale.z.abs());
            Shape::capsule(
                component.radius * radial_scale,
                component.height * scale.y.abs(),
            )
        }
        ColliderShape::ConvexMesh | ColliderShape::TriangleMesh => {
            Err(GeometryError::InvalidDimensions)
        }
    }
}

/// Капсула character controller с размерами, запечёнными по масштабу объекта.
pub fn character_capsule_shape(
    component: CharacterControllerComponent,
    scale: Vec3,
) -> Result<Shape, PhysicsSceneError> {
    let radial_scale = scale.x.abs().max(scale.z.abs());
    Ok(scale_character(component, radial_scale, scale)?.capsule()?)
}

fn build_shape(
    component: ColliderComponent,
    scale: Vec3,
    mesh_renderer: Option<&engine_scene::MeshRenderer>,
    models: &HashMap<&str, &ModelGeometry>,
) -> Result<Shape, PhysicsSceneError> {
    Ok(match component.shape {
        ColliderShape::Box | ColliderShape::Sphere | ColliderShape::Capsule => {
            primitive_collider_shape(component, scale)?
        }
        ColliderShape::ConvexMesh => {
            let geometry = model_geometry(mesh_renderer, models)?;
            let mut vertices = Vec::new();
            for mesh in &geometry.meshes {
                vertices.extend(mesh.positions.iter().map(|position| *position * scale));
            }
            Shape::ConvexHull(std::sync::Arc::new(Hull::from_points(&vertices)?))
        }
        ColliderShape::TriangleMesh => {
            let geometry = model_geometry(mesh_renderer, models)?;
            let (vertices, triangles) = triangle_geometry(geometry, scale)?;
            Shape::TriMesh(std::sync::Arc::new(TriMesh::new(vertices, triangles)?))
        }
    })
}

fn model_geometry<'a>(
    mesh_renderer: Option<&engine_scene::MeshRenderer>,
    models: &'a HashMap<&str, &'a ModelGeometry>,
) -> Result<&'a ModelGeometry, PhysicsSceneError> {
    let Some(renderer) = mesh_renderer else {
        return Err(PhysicsSceneError::InvalidModelGeometry(
            "missing mesh_renderer".to_owned(),
        ));
    };
    let path = renderer.model.as_str();
    models
        .get(path)
        .copied()
        .ok_or_else(|| PhysicsSceneError::MissingModelGeometry(path.to_owned()))
}

fn triangle_geometry(
    geometry: &ModelGeometry,
    scale: Vec3,
) -> Result<(Vec<Vec3>, Vec<[u32; 3]>), PhysicsSceneError> {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    for mesh in &geometry.meshes {
        let (indices, remainder) = mesh.indices.as_chunks::<3>();
        if !remainder.is_empty() {
            return Err(PhysicsSceneError::InvalidModelGeometry(
                geometry.path.clone(),
            ));
        }
        let base = u32::try_from(vertices.len())
            .map_err(|_| PhysicsSceneError::InvalidModelGeometry(geometry.path.clone()))?;
        if mesh
            .indices
            .iter()
            .any(|index| *index as usize >= mesh.positions.len())
        {
            return Err(PhysicsSceneError::InvalidModelGeometry(
                geometry.path.clone(),
            ));
        }
        for indices in indices {
            let first = base
                .checked_add(indices[0])
                .ok_or_else(|| PhysicsSceneError::InvalidModelGeometry(geometry.path.clone()))?;
            let second = base
                .checked_add(indices[1])
                .ok_or_else(|| PhysicsSceneError::InvalidModelGeometry(geometry.path.clone()))?;
            let third = base
                .checked_add(indices[2])
                .ok_or_else(|| PhysicsSceneError::InvalidModelGeometry(geometry.path.clone()))?;
            triangles.push([first, second, third]);
        }
        vertices.extend(mesh.positions.iter().map(|position| *position * scale));
    }
    if vertices.is_empty() || triangles.is_empty() {
        return Err(PhysicsSceneError::InvalidModelGeometry(
            geometry.path.clone(),
        ));
    }
    Ok((vertices, triangles))
}

#[cfg(test)]
mod tests {
    use super::{
        ModelGeometry, ModelMesh, PhysicsSceneDesc, PhysicsSceneSync, character_capsule_shape,
        primitive_collider_shape,
    };
    use crate::world::{BodyKind, PhysicsCommand};
    use crate::{Pose, Ray, Shape, raycast};
    use engine_scene::{ColliderComponent, ColliderShape, LocalTransform, SceneDocument, World};
    use glam::{Quat, Vec3};

    const CAMERA: &str = "[[objects]]\nid='camera'\n[objects.camera]\n";
    const CUBE_MESH: &[Vec3] = &[
        Vec3::new(-0.5, -0.5, -0.5),
        Vec3::new(0.5, -0.5, -0.5),
        Vec3::new(0.5, 0.5, -0.5),
        Vec3::new(-0.5, 0.5, -0.5),
        Vec3::new(-0.5, -0.5, 0.5),
        Vec3::new(0.5, -0.5, 0.5),
        Vec3::new(0.5, 0.5, 0.5),
        Vec3::new(-0.5, 0.5, 0.5),
    ];
    const CUBE_INDICES: &[u32] = &[
        0, 2, 1, 0, 3, 2, 4, 5, 6, 4, 6, 7, 0, 1, 5, 0, 5, 4, 3, 7, 6, 3, 6, 2, 0, 4, 7, 0, 7, 3,
        1, 2, 6, 1, 6, 5,
    ];

    fn world(text: &str) -> World {
        World::from_document(&SceneDocument::parse("physics-test.rvescene", text).unwrap()).unwrap()
    }

    fn cube_model() -> ModelGeometry {
        ModelGeometry {
            path: "models/cube.fbx".to_owned(),
            meshes: vec![ModelMesh {
                positions: CUBE_MESH.to_vec(),
                indices: CUBE_INDICES.to_vec(),
            }],
        }
    }

    fn desc(world: &World, models: &[ModelGeometry]) -> PhysicsSceneDesc {
        PhysicsSceneDesc::from_world(world, models, Vec3::ZERO, 60, 8).unwrap()
    }

    #[test]
    fn scene_builds_convex_hull_and_triangle_mesh_from_base_model_geometry() {
        let scene = world(&format!(
            "{CAMERA}\n[[assets]]\npath='/models/cube.fbx'\n\
             [[objects]]\nid='hull'\n[objects.mesh_renderer]\nmodel='/models/cube.fbx'\n\
             [objects.collider]\nshape='convex_mesh'\n\
             [[objects]]\nid='floor'\n[objects.transform]\nposition=[0,-2,0]\n\
             [objects.mesh_renderer]\nmodel='/models/cube.fbx'\n\
             [objects.collider]\nshape='triangle_mesh'\n"
        ));
        let descriptor = desc(&scene, &[cube_model()]);
        assert_eq!(descriptor.bodies.len(), 2);
        assert!(matches!(
            descriptor.bodies[0].desc.collider.shape,
            Shape::ConvexHull(_)
        ));
        let Shape::TriMesh(mesh) = &descriptor.bodies[1].desc.collider.shape else {
            panic!("expected triangle mesh")
        };
        assert_eq!(mesh.triangles().len(), 12);
    }

    #[test]
    fn negative_root_scale_reflects_offsets_hulls_and_triangle_meshes() {
        let scene = world(&format!(
            "{CAMERA}\n[[assets]]\npath='/models/asymmetric.fbx'\n\
             [[objects]]\nid='offset'\n[objects.transform]\nscale=[-1,1,1]\n\
             [objects.collider]\nshape='box'\nhalf_extents=[0.25,0.25,0.25]\noffset=[1,0,0]\n\
             [[objects]]\nid='hull'\n[objects.transform]\nscale=[-1,1,1]\n\
             [objects.mesh_renderer]\nmodel='/models/asymmetric.fbx'\n\
             [objects.collider]\nshape='convex_mesh'\n\
             [[objects]]\nid='mesh'\n[objects.transform]\nscale=[-1,1,1]\n\
             [objects.mesh_renderer]\nmodel='/models/asymmetric.fbx'\n\
             [objects.collider]\nshape='triangle_mesh'\n"
        ));
        let model = ModelGeometry {
            path: "models/asymmetric.fbx".to_owned(),
            meshes: vec![ModelMesh {
                positions: vec![
                    Vec3::ZERO,
                    Vec3::new(2.0, 0.0, 0.0),
                    Vec3::new(0.0, 1.0, 0.0),
                    Vec3::new(0.0, 0.0, 1.0),
                ],
                indices: vec![0, 1, 2],
            }],
        };
        let descriptor = desc(&scene, &[model]);
        assert_eq!(descriptor.bodies[0].desc.collider.offset, -Vec3::X);
        let hull = &descriptor.bodies[1].desc.collider.shape;
        let triangle_mesh = &descriptor.bodies[2].desc.collider.shape;
        let left_ray = Ray::new(Vec3::new(-0.5, 0.2, 2.0), -Vec3::Z, 4.0).unwrap();
        let right_ray = Ray::new(Vec3::new(0.5, 0.2, 2.0), -Vec3::Z, 4.0).unwrap();

        assert!(raycast(&left_ray, hull, Pose::IDENTITY).unwrap().is_some());
        assert!(raycast(&right_ray, hull, Pose::IDENTITY).unwrap().is_none());
        assert!(
            raycast(&left_ray, triangle_mesh, Pose::IDENTITY)
                .unwrap()
                .is_some()
        );
        assert!(
            raycast(&right_ray, triangle_mesh, Pose::IDENTITY)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn missing_and_degenerate_model_geometry_are_errors() {
        let scene = world(&format!(
            "{CAMERA}\n[[assets]]\npath='/models/cube.fbx'\n\
             [[objects]]\nid='hull'\n[objects.mesh_renderer]\nmodel='/models/cube.fbx'\n\
             [objects.collider]\nshape='convex_mesh'\n"
        ));
        assert!(matches!(
            PhysicsSceneDesc::from_world(&scene, &[], Vec3::ZERO, 60, 8),
            Err(super::PhysicsSceneError::MissingModelGeometry(_))
        ));

        let flat = ModelGeometry {
            path: "models/cube.fbx".to_owned(),
            meshes: vec![ModelMesh {
                positions: vec![Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::X + Vec3::Y],
                indices: vec![0, 1, 2],
            }],
        };
        assert!(matches!(
            PhysicsSceneDesc::from_world(&scene, &[flat], Vec3::ZERO, 60, 8),
            Err(super::PhysicsSceneError::Geometry(
                crate::GeometryError::DegenerateHull
            ))
        ));

        let invalid_triangles = ModelGeometry {
            path: "models/cube.fbx".to_owned(),
            meshes: vec![ModelMesh {
                positions: CUBE_MESH.to_vec(),
                indices: vec![0, 1, 99],
            }],
        };
        let triangle_scene = world(&format!(
            "{CAMERA}\n[[assets]]\npath='/models/cube.fbx'\n\
             [[objects]]\nid='mesh'\n[objects.mesh_renderer]\nmodel='/models/cube.fbx'\n\
             [objects.collider]\nshape='triangle_mesh'\n"
        ));
        assert!(matches!(
            PhysicsSceneDesc::from_world(&triangle_scene, &[invalid_triangles], Vec3::ZERO, 60, 8),
            Err(super::PhysicsSceneError::InvalidModelGeometry(_))
        ));
    }

    #[test]
    fn collider_under_rotated_nonuniform_parent_uses_finite_cached_world_pose() {
        let mut scene = world(&format!(
            "{CAMERA}\n[[objects]]\nid='parent'\n\
             [objects.transform]\nrotation=[0,30,0]\nscale=[2,1,0.5]\n\
             [[objects]]\nid='floor'\nparent='parent'\n\
             [objects.transform]\nrotation=[0,45,0]\n\
             [objects.collider]\nshape='box'\nhalf_extents=[0.5,0.5,0.5]\n"
        ));
        let descriptor = desc(&scene, &[]);
        let collider = &descriptor.bodies[0].desc.collider;
        let Shape::Box { half_extents } = &collider.shape else {
            panic!("expected box")
        };
        assert!(half_extents.is_finite());
        assert!((half_extents.x - 0.625).abs() < 1.0e-5, "{half_extents:?}");
        assert!((half_extents.y - 0.5).abs() < 1.0e-5, "{half_extents:?}");
        assert!((half_extents.z - 0.625).abs() < 1.0e-5, "{half_extents:?}");
        assert!(descriptor.bodies[0].desc.pose.rotation.is_finite());
        assert!(
            descriptor.bodies[0]
                .desc
                .pose
                .rotation
                .dot(
                    scene
                        .world_rotation(scene.handle("floor").unwrap())
                        .unwrap()
                )
                .abs()
                > 0.999_99
        );

        let (_, bindings) = descriptor.clone().build().unwrap();
        let mut sync = PhysicsSceneSync::new(&scene, bindings).unwrap();
        let collapsed_parent = scene.handle("parent").unwrap();
        scene
            .set_local_transform(
                collapsed_parent,
                LocalTransform {
                    scale: Vec3::ZERO,
                    ..*scene.local_transform(collapsed_parent).unwrap()
                },
            )
            .unwrap();
        scene.recalculate_world_matrices();
        assert!(sync.sync_world_commands(&mut scene).unwrap().is_empty());
        assert!(
            scene
                .world_matrix(scene.handle("floor").unwrap())
                .unwrap()
                .is_finite()
        );
        let zero_scale = PhysicsSceneDesc::from_world(&scene, &[], Vec3::ZERO, 60, 8);
        assert!(matches!(
            zero_scale,
            Err(super::PhysicsSceneError::InvalidObjectTransform(_))
        ));
    }

    #[test]
    fn world_sync_handles_zero_scale_without_decomposition_or_false_teleport() {
        let mut scene = world(&format!(
            "{CAMERA}\n[[objects]]\nid='box'\n\
             [objects.collider]\nshape='box'\n\
             [objects.rigid_body]\nkind='dynamic'\n"
        ));
        let descriptor = desc(&scene, &[]);
        let (_, bindings) = descriptor.build().unwrap();
        let mut sync = PhysicsSceneSync::new(&scene, bindings).unwrap();
        let handle = scene.handle("box").unwrap();
        scene
            .set_local_transform(
                handle,
                LocalTransform {
                    scale: Vec3::ZERO,
                    ..*scene.local_transform(handle).unwrap()
                },
            )
            .unwrap();
        assert!(sync.sync_world_commands(&mut scene).unwrap().is_empty());
        let rotation = scene.world_rotation(handle).unwrap();
        assert!(rotation.is_finite());
        assert!(scene.world_matrix(handle).unwrap().is_finite());
    }

    #[test]
    fn dynamic_physics_pose_updates_root_and_children_then_syncs_without_teleport() {
        let mut scene = world(&format!(
            "{CAMERA}\n[[objects]]\nid='box'\n\
             [objects.collider]\nshape='box'\n\
             [objects.rigid_body]\nkind='dynamic'\n\
             [[objects]]\nid='child'\nparent='box'\n\
             [objects.transform]\nposition=[0,1,0]\n"
        ));
        let descriptor = desc(&scene, &[]);
        let (driver, bindings) = descriptor.build().unwrap();
        let mut sync = PhysicsSceneSync::new(&scene, bindings).unwrap();
        let body = sync.bindings()[0].body;
        sync.apply_physics_poses(
            &mut scene,
            &[(
                body,
                Pose::new(Vec3::new(3.0, 2.0, 0.0), Quat::IDENTITY).unwrap(),
            )],
        )
        .unwrap();
        assert_eq!(
            scene
                .world_matrix(scene.handle("child").unwrap())
                .unwrap()
                .w_axis
                .truncate(),
            Vec3::new(3.0, 3.0, 0.0)
        );
        assert!(sync.sync_world_commands(&mut scene).unwrap().is_empty());
        assert_eq!(driver.world().body_pose(body).unwrap().position, Vec3::ZERO);
    }

    #[test]
    fn primitive_helpers_bake_signed_scale_like_scene_build() {
        let box_component = ColliderComponent {
            shape: ColliderShape::Box,
            half_extents: [1.0, 2.0, 3.0],
            ..ColliderComponent::default()
        };
        assert_eq!(
            primitive_collider_shape(box_component, Vec3::new(-2.0, 0.5, -1.0)).unwrap(),
            Shape::Box {
                half_extents: Vec3::new(2.0, 1.0, 3.0)
            }
        );
        let sphere = ColliderComponent {
            shape: ColliderShape::Sphere,
            radius: 1.0,
            ..ColliderComponent::default()
        };
        assert_eq!(
            primitive_collider_shape(sphere, Vec3::new(1.0, -3.0, 2.0)).unwrap(),
            Shape::Sphere { radius: 3.0 }
        );
        let capsule = ColliderComponent {
            shape: ColliderShape::Capsule,
            radius: 0.5,
            height: 2.0,
            ..ColliderComponent::default()
        };
        assert_eq!(
            primitive_collider_shape(capsule, Vec3::new(2.0, -3.0, -1.0)).unwrap(),
            Shape::Capsule {
                radius: 1.0,
                height: 6.0
            }
        );
        let mesh = ColliderComponent {
            shape: ColliderShape::TriangleMesh,
            ..ColliderComponent::default()
        };
        assert!(primitive_collider_shape(mesh, Vec3::ONE).is_err());

        let controller = engine_scene::CharacterControllerComponent::default();
        assert_eq!(
            character_capsule_shape(controller, Vec3::new(2.0, 0.5, 1.0)).unwrap(),
            Shape::Capsule {
                radius: 0.7,
                height: 0.55
            }
        );
    }

    #[test]
    fn dynamic_scene_box_can_sleep_without_repeated_sync_commands() {
        let mut scene = world(&format!(
            "{CAMERA}\n[[objects]]\nid='floor'\n\
             [objects.transform]\nposition=[0,-0.5,0]\n\
             [objects.collider]\nshape='box'\nhalf_extents=[5,0.5,5]\n\
             [[objects]]\nid='box'\n\
             [objects.collider]\nshape='box'\n\
             [objects.rigid_body]\nkind='dynamic'\n"
        ));
        let descriptor =
            PhysicsSceneDesc::from_world(&scene, &[], Vec3::new(0.0, -9.81, 0.0), 60, 8).unwrap();
        let (mut driver, bindings) = descriptor.build().unwrap();
        let mut sync = PhysicsSceneSync::new(&scene, bindings).unwrap();
        let dynamic = sync.bindings()[1].body;
        for _ in 0..120 {
            let result = driver.advance_step(1.0 / 60.0, []).unwrap();
            sync.apply_physics_poses(&mut scene, &result.poses).unwrap();
            assert!(sync.sync_world_commands(&mut scene).unwrap().is_empty());
        }
        assert!(driver.world().body_is_sleeping(dynamic).unwrap());
    }

    #[test]
    fn built_scene_bindings_preserve_static_dynamic_kinematic_and_character_kinds() {
        let mut scene = world(&format!(
            "{CAMERA}\n[[objects]]\nid='static'\n\
             [objects.collider]\nshape='box'\n\
             [[objects]]\nid='kinematic'\n\
             [objects.collider]\nshape='box'\n\
             [objects.rigid_body]\nkind='kinematic'\n\
             [[objects]]\nid='dynamic'\n\
             [objects.collider]\nshape='sphere'\n\
             [objects.rigid_body]\nkind='dynamic'\n\
             [[objects]]\nid='character'\n\
             [objects.character_controller]\n"
        ));
        let (driver, bindings) = desc(&scene, &[]).build().unwrap();
        assert_eq!(bindings[0].kind, BodyKind::Static);
        assert_eq!(bindings[1].kind, BodyKind::Kinematic);
        assert_eq!(bindings[2].kind, BodyKind::Dynamic);
        assert_eq!(bindings[3].kind, BodyKind::Kinematic);
        assert!(bindings[3].character_controller.is_some());
        assert_eq!(
            driver.world().body_pose(bindings[3].body).unwrap().position,
            Vec3::ZERO
        );

        let mut sync = PhysicsSceneSync::new(&scene, bindings.clone()).unwrap();
        let initial = sync.sync_world_commands(&mut scene).unwrap();
        assert_eq!(initial.len(), 1);
        assert!(matches!(
            initial[0],
            PhysicsCommand::KinematicTarget { body, .. } if body == bindings[3].body
        ));
        for (id, x) in [("static", 1.0), ("kinematic", 2.0), ("dynamic", 3.0)] {
            let handle = scene.handle(id).unwrap();
            let mut transform = *scene.local_transform(handle).unwrap();
            transform.position.x = x;
            scene.set_local_transform(handle, transform).unwrap();
        }
        let changed = sync.sync_world_commands(&mut scene).unwrap();
        assert_eq!(changed.len(), 4);
        assert!(matches!(
            changed[0],
            PhysicsCommand::Teleport { body, .. } if body == bindings[0].body
        ));
        assert!(matches!(
            changed[1],
            PhysicsCommand::KinematicTarget { body, .. } if body == bindings[1].body
        ));
        assert!(matches!(
            changed[2],
            PhysicsCommand::Teleport { body, .. } if body == bindings[2].body
        ));
        assert!(matches!(
            changed[3],
            PhysicsCommand::KinematicTarget { body, .. } if body == bindings[3].body
        ));
    }

    #[test]
    fn pose_comparison_treats_quaternion_signs_as_equivalent() {
        let first = Pose::new(Vec3::ONE, Quat::from_rotation_y(0.7)).unwrap();
        let second = Pose::new(Vec3::ONE, -first.rotation).unwrap();
        assert!(!super::pose_changed(first, second));
    }
}
