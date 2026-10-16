//! Runtime-данные сцены и подготовка кадра без зависимости от renderer.

use crate::camera::Camera;
use crate::document::{
    CameraComponent, DirectionalLightComponent, HeavyScript, LiteScript, SceneDocument, SceneError,
    SceneObject, camera_from_matrix, light_direction_from_matrix,
};
use glam::{EulerRot, Mat4, Quat, Vec3};
use std::collections::{HashMap, HashSet, VecDeque};

/// Стабильный индекс объекта в порядке документа, принадлежащий [`World`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectHandle(usize);

impl ObjectHandle {
    pub fn index(self) -> usize {
        self.0
    }
}

/// Редактируемая локальная трансформация; при runtime-изменениях углы остаются в градусах.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalTransform {
    pub position: Vec3,
    pub rotation_deg: Vec3,
    pub scale: Vec3,
}

impl Default for LocalTransform {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            rotation_deg: Vec3::ZERO,
            scale: Vec3::ONE,
        }
    }
}

impl LocalTransform {
    /// Возвращает локальную матрицу `T · R · S` с порядком Эйлера Unity YXZ.
    pub fn matrix(self) -> Mat4 {
        let rotation = self.rotation_deg * (std::f32::consts::PI / 180.0);
        Mat4::from_translation(self.position)
            * Mat4::from_quat(Quat::from_euler(
                EulerRot::YXZ,
                rotation.y,
                rotation.x,
                rotation.z,
            ))
            * Mat4::from_scale(self.scale)
    }
}

impl From<crate::document::SceneTransform> for LocalTransform {
    fn from(transform: crate::document::SceneTransform) -> Self {
        Self {
            position: Vec3::from_array(transform.position),
            rotation_deg: Vec3::from_array(transform.rotation),
            scale: Vec3::from_array(transform.scale),
        }
    }
}

/// Данные объекта, сохранённые в runtime-мире.
#[derive(Clone, Debug)]
pub struct WorldObject {
    pub handle: ObjectHandle,
    pub id: String,
    pub name: String,
    pub parent: Option<ObjectHandle>,
    pub transform: LocalTransform,
    pub mesh_renderer: Option<crate::document::MeshRenderer>,
    pub camera: Option<CameraComponent>,
    pub directional_light: Option<DirectionalLightComponent>,
    pub lite_scripts: Vec<LiteScript>,
    pub heavy_scripts: Vec<HeavyScript>,
}

/// Параметры света для потребителя кадра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightState {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub ambient: [f32; 3],
    pub shadow: Option<(Vec3, f32)>,
}

impl Default for LightState {
    fn default() -> Self {
        Self {
            direction: [0.0, -1.0, 0.0],
            color: [0.0; 3],
            ambient: [1.0; 3],
            shadow: None,
        }
    }
}

/// Экземпляр меша с handle объекта и мировой матрицей.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshInstance {
    pub object: ObjectHandle,
    pub model: Mat4,
}

/// Данные мирового пространства для отрисовки одного кадра сцены.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneFrame {
    pub camera: Camera,
    pub light: LightState,
    pub meshes: Vec<MeshInstance>,
}

/// Нефатальное предупреждение о базисе, возвращаемое один раз для объекта.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SceneWarning {
    pub source: String,
    pub object_id: String,
    pub message: String,
}

/// Runtime-граф сцены без зависимостей от renderer и script runtime.
#[derive(Clone, Debug)]
pub struct World {
    source_label: String,
    objects: Vec<WorldObject>,
    indices_by_id: HashMap<String, ObjectHandle>,
    topological_order: Vec<ObjectHandle>,
    world_matrices: Vec<Mat4>,
    last_valid_camera: Option<Camera>,
    last_valid_light: LightState,
    invalid_basis_reported: HashSet<ObjectHandle>,
}

impl World {
    /// Создаёт runtime-мир из разобранного и проверенного документа сцены.
    pub fn from_document(document: &SceneDocument) -> Result<Self, SceneError> {
        document.validate()?;
        let indices_by_id = document
            .objects
            .iter()
            .enumerate()
            .map(|(index, object)| (object.id.clone(), ObjectHandle(index)))
            .collect::<HashMap<_, _>>();
        let mut objects = Vec::with_capacity(document.objects.len());
        for (index, object) in document.objects.iter().enumerate() {
            objects.push(world_object(
                ObjectHandle(index),
                object,
                object.parent.as_ref().map(|parent| indices_by_id[parent]),
            ));
        }
        let transforms = objects
            .iter()
            .map(|object| object.transform)
            .collect::<Vec<_>>();
        let parents = objects
            .iter()
            .map(|object| object.parent.map(|parent| parent.0))
            .collect::<Vec<_>>();
        let object_ids = objects
            .iter()
            .map(|object| object.id.as_str())
            .collect::<Vec<_>>();
        let (world_matrices, order) =
            calculate_world_matrices(&transforms, &parents, &object_ids, document.source_label())?;
        let mut world = Self {
            source_label: document.source_label().to_owned(),
            world_matrices,
            objects,
            indices_by_id,
            topological_order: order.into_iter().map(ObjectHandle).collect(),
            last_valid_camera: None,
            last_valid_light: LightState::default(),
            invalid_basis_reported: HashSet::new(),
        };
        let _ = world.build_frame();
        Ok(world)
    }

    pub fn objects(&self) -> &[WorldObject] {
        &self.objects
    }

    pub fn handle(&self, id: &str) -> Option<ObjectHandle> {
        self.indices_by_id.get(id).copied()
    }

    pub fn object(&self, handle: ObjectHandle) -> Option<&WorldObject> {
        self.objects.get(handle.0)
    }

    pub fn object_by_id(&self, id: &str) -> Option<&WorldObject> {
        self.handle(id).and_then(|handle| self.object(handle))
    }

    pub fn local_transform(&self, handle: ObjectHandle) -> Option<&LocalTransform> {
        self.object(handle).map(|object| &object.transform)
    }

    /// Заменяет локальную трансформацию объекта, сохраняя углы Эйлера в градусах.
    pub fn set_local_transform(
        &mut self,
        handle: ObjectHandle,
        transform: LocalTransform,
    ) -> Result<(), SceneError> {
        let Some(object) = self.objects.get_mut(handle.0) else {
            return Err(SceneError::new(
                &self.source_label,
                None,
                format!("неизвестный handle объекта {}", handle.0),
            ));
        };
        object.transform = transform;
        Ok(())
    }

    pub fn world_matrix(&self, handle: ObjectHandle) -> Option<Mat4> {
        self.world_matrices.get(handle.0).copied()
    }

    /// Пересчитывает мировые матрицы в порядке от родителя к потомкам.
    pub fn recalculate_world_matrices(&mut self) {
        for handle in &self.topological_order {
            let object = &self.objects[handle.0];
            let local = object.transform.matrix();
            let parent = object.parent.map(|parent| parent.0);
            self.world_matrices[handle.0] =
                compose_world_matrix(local, parent, &self.world_matrices);
        }
    }

    /// Строит кадр и возвращает первые предупреждения о невалидных базисах.
    /// Для камеры или света сохраняется последнее валидное динамическое состояние.
    pub fn build_frame(&mut self) -> (SceneFrame, Vec<SceneWarning>) {
        self.recalculate_world_matrices();
        let mut warnings = Vec::new();
        let mut camera = self.last_valid_camera;
        let mut light = self.last_valid_light;

        for index in 0..self.objects.len() {
            let matrix = self.world_matrices[index];
            let camera_settings = self.objects[index].camera;
            let light_settings = self.objects[index].directional_light;
            if let Some(settings) = camera_settings {
                if let Some(value) = camera_from_matrix(matrix, settings) {
                    camera = Some(value);
                    self.last_valid_camera = Some(value);
                } else {
                    self.warn_invalid_basis(ObjectHandle(index), "камеры", &mut warnings);
                }
            }
            if let Some(settings) = light_settings {
                if let Some(direction) = light_direction_from_matrix(matrix) {
                    let value = light_state(settings, direction);
                    light = value;
                    self.last_valid_light = value;
                } else {
                    self.warn_invalid_basis(
                        ObjectHandle(index),
                        "направленного света",
                        &mut warnings,
                    );
                }
            }
        }

        // from_document проверяет сцену и сразу фиксирует её первый валидный базис камеры.
        let camera = camera.expect("World::from_document initializes the validated camera");
        let meshes = self
            .objects
            .iter()
            .filter(|object| object.mesh_renderer.is_some())
            .map(|object| MeshInstance {
                object: object.handle,
                model: self.world_matrices[object.handle.0],
            })
            .collect();

        (
            SceneFrame {
                camera,
                light,
                meshes,
            },
            warnings,
        )
    }

    fn warn_invalid_basis(
        &mut self,
        handle: ObjectHandle,
        component: &str,
        warnings: &mut Vec<SceneWarning>,
    ) {
        if self.invalid_basis_reported.insert(handle) {
            warnings.push(SceneWarning {
                source: self.source_label.clone(),
                object_id: self.objects[handle.0].id.clone(),
                message: format!("невалидный базис {component}"),
            });
        }
    }
}

fn world_object(
    handle: ObjectHandle,
    object: &SceneObject,
    parent: Option<ObjectHandle>,
) -> WorldObject {
    WorldObject {
        handle,
        id: object.id.clone(),
        name: object.display_name().to_owned(),
        parent,
        transform: object.transform.into(),
        mesh_renderer: object.mesh_renderer.clone(),
        camera: object.camera,
        directional_light: object.directional_light,
        lite_scripts: object.lite_scripts.clone(),
        heavy_scripts: object.heavy_scripts.clone(),
    }
}

pub(crate) fn calculate_world_matrices(
    transforms: &[LocalTransform],
    parents: &[Option<usize>],
    object_ids: &[&str],
    source: &str,
) -> Result<(Vec<Mat4>, Vec<usize>), SceneError> {
    let order = topological_order(parents, object_ids, source)?;
    let mut world_matrices = vec![Mat4::IDENTITY; transforms.len()];
    for &index in &order {
        world_matrices[index] =
            compose_world_matrix(transforms[index].matrix(), parents[index], &world_matrices);
    }
    Ok((world_matrices, order))
}

fn topological_order(
    parents: &[Option<usize>],
    object_ids: &[&str],
    source: &str,
) -> Result<Vec<usize>, SceneError> {
    let mut children = vec![Vec::new(); parents.len()];
    let mut queue = VecDeque::new();
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            children[*parent].push(index);
        } else {
            queue.push_back(index);
        }
    }
    let mut result = Vec::with_capacity(parents.len());
    let mut visited = vec![false; parents.len()];
    while let Some(index) = queue.pop_front() {
        result.push(index);
        visited[index] = true;
        queue.extend(children[index].iter().copied());
    }
    if result.len() != parents.len() {
        let object_id = visited
            .iter()
            .position(|was_visited| !was_visited)
            .and_then(|index| object_ids.get(index).copied());
        return Err(SceneError::new(
            source,
            object_id,
            "обнаружен цикл в иерархии parent",
        ));
    }
    Ok(result)
}

fn compose_world_matrix(local: Mat4, parent: Option<usize>, world_matrices: &[Mat4]) -> Mat4 {
    match parent {
        Some(parent) => world_matrices[parent] * local,
        None => local,
    }
}

fn light_state(settings: DirectionalLightComponent, direction: [f32; 3]) -> LightState {
    LightState {
        direction,
        color: settings.color,
        ambient: settings.ambient,
        shadow: settings.shadows.then(|| {
            (
                Vec3::from_array(settings.shadow_center),
                settings.shadow_radius.unwrap(),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalTransform, ObjectHandle, World};
    use crate::SceneDocument;
    use glam::{Mat4, Quat, Vec3};

    const CAMERA_SCENE: &str = r#"
[[objects]]
id = "camera"
[objects.camera]
fov_deg = 60.0
near = 0.1
far = 100.0
"#;

    fn parse(input: &str) -> SceneDocument {
        SceneDocument::parse("world-test.rvescene", input).unwrap()
    }

    #[test]
    fn local_rotation_uses_unity_yxz_euler_order_and_keeps_degrees() {
        let axis_cases = [
            (
                Vec3::new(90.0, 0.0, 0.0),
                Quat::from_rotation_x(90.0_f32.to_radians()),
            ),
            (
                Vec3::new(0.0, 90.0, 0.0),
                Quat::from_rotation_y(90.0_f32.to_radians()),
            ),
            (
                Vec3::new(0.0, 0.0, 90.0),
                Quat::from_rotation_z(90.0_f32.to_radians()),
            ),
        ];
        for (rotation_deg, expected) in axis_cases {
            let transform = LocalTransform {
                rotation_deg,
                ..LocalTransform::default()
            };
            assert!(
                transform
                    .matrix()
                    .abs_diff_eq(Mat4::from_quat(expected), 1e-6)
            );
        }
        let angles = Vec3::new(31.0, -47.0, 19.0);
        let actual = LocalTransform {
            rotation_deg: angles,
            ..LocalTransform::default()
        }
        .matrix();
        let radians = angles * (std::f32::consts::PI / 180.0);
        let expected = Mat4::from_quat(
            Quat::from_rotation_y(radians.y)
                * Quat::from_rotation_x(radians.x)
                * Quat::from_rotation_z(radians.z),
        );
        assert!(actual.abs_diff_eq(expected, 1e-6));
    }

    #[test]
    fn world_composes_a_camera_under_a_translated_rotated_parent() {
        let input = r#"
[[objects]]
id="camera"
parent="rig"
[objects.transform]
position=[0.0,0.0,2.0]
[objects.camera]
fov_deg=60.0
near=0.1
far=10.0
[[objects]]
id="rig"
[objects.transform]
position=[1.0,2.0,3.0]
rotation=[0.0,90.0,0.0]
"#;
        let mut world = World::from_document(&parse(input)).unwrap();
        let (frame, warnings) = world.build_frame();
        assert!(warnings.is_empty());
        assert!(
            frame
                .camera
                .position
                .abs_diff_eq(Vec3::new(3.0, 2.0, 3.0), 1e-5)
        );
        let distance = Vec3::new(3.0, 2.0, 3.0).length();
        assert!(
            frame
                .camera
                .target
                .abs_diff_eq(Vec3::new(3.0 - distance, 2.0, 3.0), 1e-5)
        );
        assert!(frame.camera.up.abs_diff_eq(Vec3::Y, 1e-5));
    }

    #[test]
    fn portrait_projection_keeps_the_fov_on_the_narrow_side() {
        let world = World::from_document(&parse(CAMERA_SCENE)).unwrap();
        let (frame, _) = world.clone().build_frame();
        let projection = frame.camera.view_projection(720, 1280);
        assert!(projection.is_finite());
        assert!(projection.x_axis.x > projection.y_axis.y);
        let expected_narrow = 1.0 / (30.0_f32.to_radians().tan());
        assert!((projection.x_axis.x - expected_narrow).abs() < 1e-5);
    }

    #[test]
    fn absent_light_uses_unlit_defaults_and_present_light_builds_direction_and_shadow() {
        let mut world = World::from_document(&parse(CAMERA_SCENE)).unwrap();
        let (frame, _) = world.build_frame();
        assert_eq!(frame.light.color, [0.0; 3]);
        assert_eq!(frame.light.direction, [0.0, -1.0, 0.0]);
        assert!(Vec3::from_array(frame.light.direction).is_finite());
        assert!(Vec3::from_array(frame.light.direction).length() > 0.0);
        assert_eq!(frame.light.ambient, [1.0; 3]);
        assert_eq!(frame.light.shadow, None);

        let scene = format!(
            "{CAMERA_SCENE}\n[[objects]]\nid=\"sun\"\n[objects.transform]\nrotation=[0.0,90.0,0.0]\n[objects.directional_light]\nshadows=true\nshadow_radius=5.0\n"
        );
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let (frame, _) = world.build_frame();
        assert!(Vec3::from_array(frame.light.direction).abs_diff_eq(Vec3::NEG_X, 1e-5));
        assert_eq!(frame.light.shadow, Some((Vec3::ZERO, 5.0)));
    }

    #[test]
    fn zero_scale_mesh_is_allowed_and_keeps_document_order_handles() {
        let scene = format!(
            "[[assets]]\npath=\"/m.fbx\"\n{CAMERA_SCENE}\n[[objects]]\nid=\"mesh\"\n[objects.transform]\nscale=[0.0,0.0,0.0]\n[objects.mesh_renderer]\nmodel=\"/m.fbx\"\n"
        );
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let (frame, _) = world.build_frame();
        assert_eq!(world.objects()[0].handle, ObjectHandle(0));
        assert_eq!(world.objects()[1].handle, ObjectHandle(1));
        assert_eq!(frame.meshes.len(), 1);
        assert_eq!(frame.meshes[0].object.index(), 1);
        assert_eq!(frame.meshes[0].model, Mat4::from_scale(Vec3::ZERO));
    }

    #[test]
    fn zero_scale_camera_or_light_basis_is_rejected_during_load() {
        let camera = r#"
[[objects]]
id="camera"
parent="rig"
[objects.camera]
fov_deg=60.0
near=0.1
far=10.0
[[objects]]
id="rig"
[objects.transform]
scale=[0.0,0.0,0.0]
"#;
        let error = SceneDocument::parse("zero-camera.rvescene", camera).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("camera"));

        let light = format!(
            "{CAMERA_SCENE}\n[[objects]]\nid=\"sun\"\nparent=\"rig\"\n[objects.directional_light]\n[[objects]]\nid=\"rig\"\n[objects.transform]\nscale=[0.0,0.0,0.0]\n"
        );
        let error = SceneDocument::parse("zero-light.rvescene", &light).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("sun"));
    }

    #[test]
    fn local_transform_setter_preserves_rotation_degrees() {
        let mut world = World::from_document(&parse(CAMERA_SCENE)).unwrap();
        let handle = world.handle("camera").unwrap();
        let transform = LocalTransform {
            position: Vec3::new(1.0, 2.0, 3.0),
            rotation_deg: Vec3::new(12.5, 37.0, -81.0),
            scale: Vec3::ONE,
        };
        world.set_local_transform(handle, transform).unwrap();
        assert_eq!(
            world.local_transform(handle).unwrap().rotation_deg,
            transform.rotation_deg
        );
        assert_eq!(
            world.object(handle).unwrap().transform.position,
            transform.position
        );
    }

    #[test]
    fn dynamic_invalid_basis_keeps_last_valid_camera_and_warns_once_per_object() {
        let mut world = World::from_document(&parse(CAMERA_SCENE)).unwrap();
        let handle = world.handle("camera").unwrap();
        let previous = world.build_frame().0.camera;
        world
            .set_local_transform(
                handle,
                LocalTransform {
                    scale: Vec3::new(1.0, 0.0, 1.0),
                    ..LocalTransform::default()
                },
            )
            .unwrap();
        let (first, warnings) = world.build_frame();
        assert_eq!(first.camera, previous);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].object_id, "camera");
        assert!(warnings[0].message.contains("камеры"));
        let (second, warnings) = world.build_frame();
        assert_eq!(second.camera, previous);
        assert!(warnings.is_empty());
    }

    #[test]
    fn dynamic_invalid_light_keeps_last_valid_state_and_warns_once() {
        let scene =
            format!("{CAMERA_SCENE}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\n");
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let handle = world.handle("sun").unwrap();
        let previous = world.build_frame().0.light;
        world
            .set_local_transform(
                handle,
                LocalTransform {
                    scale: Vec3::new(1.0, 1.0, 0.0),
                    ..LocalTransform::default()
                },
            )
            .unwrap();

        let (first, warnings) = world.build_frame();
        assert_eq!(first.light, previous);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].object_id, "sun");
        assert!(warnings[0].message.contains("света"));

        let (second, warnings) = world.build_frame();
        assert_eq!(second.light, previous);
        assert!(warnings.is_empty());
    }

    #[test]
    fn recalculate_uses_parent_world_before_child_even_when_parent_follows_it() {
        let input = r#"
[[objects]]
id="child"
parent="parent"
[objects.transform]
position=[1.0,0.0,0.0]
[[objects]]
id="camera"
[objects.camera]
fov_deg=60.0
near=0.1
far=10.0
[[objects]]
id="parent"
[objects.transform]
position=[0.0,2.0,0.0]
"#;
        let world = World::from_document(&parse(input)).unwrap();
        let matrix = world.world_matrix(world.handle("child").unwrap()).unwrap();
        assert!(
            matrix
                .transform_point3(Vec3::ZERO)
                .abs_diff_eq(Vec3::new(1.0, 2.0, 0.0), 1e-5)
        );
    }
}
