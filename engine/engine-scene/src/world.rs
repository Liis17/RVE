//! Runtime-данные сцены и подготовка кадра без зависимости от renderer.

use crate::CameraSettings;
use crate::animation::AnimatorComponent;
use crate::audio::AudioSourceComponent;
use crate::camera::Camera;
use crate::document::{
    CameraComponent, CameraCursor, CharacterControllerComponent, ColliderComponent,
    DirectionalLightComponent, EnvironmentComponent, HeavyScript, LiteScript, LodGroup,
    PointLightComponent, RigidBodyComponent, SceneDocument, SceneError, SceneObject,
    SpotLightComponent, camera_from_matrix, light_direction_from_matrix,
};
use crate::light::ShadowSettings;
use engine_files::VirtualPath;
use glam::{EulerRot, Mat3, Mat4, Quat, Vec3};
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
    pub lod_group: Option<LodGroup>,
    pub camera: Option<CameraComponent>,
    pub camera_settings: Option<CameraSettings>,
    pub directional_light: Option<DirectionalLightComponent>,
    pub point_light: Option<PointLightComponent>,
    pub spot_light: Option<SpotLightComponent>,
    pub environment: Option<EnvironmentComponent>,
    pub collider: Option<ColliderComponent>,
    pub rigid_body: Option<RigidBodyComponent>,
    pub character_controller: Option<CharacterControllerComponent>,
    pub audio_source: Option<AudioSourceComponent>,
    pub animator: Option<AnimatorComponent>,
    pub lite_scripts: Vec<LiteScript>,
    pub heavy_scripts: Vec<HeavyScript>,
}

/// Параметры света для потребителя кадра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightState {
    pub direction: [f32; 3],
    pub color: [f32; 3],
    pub ambient: [f32; 3],
    pub shadow: Option<ShadowSettings>,
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

/// Пересчитанный локальный источник света в мировом пространстве.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LocalLightState {
    Point {
        position: [f32; 3],
        color: [f32; 3],
        intensity: f32,
        range: f32,
        shadows: bool,
    },
    Spot {
        position: [f32; 3],
        direction: [f32; 3],
        color: [f32; 3],
        intensity: f32,
        range: f32,
        inner_angle_deg: f32,
        outer_angle_deg: f32,
        shadows: bool,
    },
}

#[derive(Clone, Copy, Debug, Default)]
struct LastValidLocalLights {
    point: Option<LocalLightState>,
    spot: Option<LocalLightState>,
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
    pub view: CameraSettings,
    pub light: LightState,
    pub local_lights: Vec<LocalLightState>,
    pub environment: Option<EnvironmentState>,
    pub meshes: Vec<MeshInstance>,
}

/// Текущий источник окружения, мировая ориентация владельца и направление солнечного света.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentState {
    pub component: EnvironmentComponent,
    pub rotation: [[f32; 3]; 3],
    pub sun_direction: Option<[f32; 3]>,
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
    asset_paths: Vec<VirtualPath>,
    objects: Vec<WorldObject>,
    indices_by_id: HashMap<String, ObjectHandle>,
    topological_order: Vec<ObjectHandle>,
    world_matrices: Vec<Mat4>,
    world_rotations: Vec<Quat>,
    active_camera: ObjectHandle,
    last_valid_camera: Option<(Camera, CameraSettings)>,
    last_valid_light: LightState,
    last_valid_local_lights: Vec<LastValidLocalLights>,
    last_valid_environment: Option<EnvironmentState>,
    invalid_basis_reported: HashSet<ObjectHandle>,
}

impl World {
    /// Создаёт runtime-мир из разобранного и проверенного документа сцены.
    pub fn from_document(document: &SceneDocument) -> Result<Self, SceneError> {
        if !document.instances.is_empty() {
            return Err(SceneError::new(
                document.source_label(),
                None,
                "документ содержит экземпляры префабов; раскройте его через prefab::expand",
            ));
        }
        document.validate()?;
        if document
            .objects
            .iter()
            .all(|object| object.camera.is_none())
        {
            return Err(SceneError::new(
                document.source_label(),
                None,
                "для World нужна хотя бы одна камера",
            ));
        }
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
        let world_rotations = calculate_world_rotations(&transforms, &parents, &order);
        let object_count = objects.len();
        let mut world = Self {
            source_label: document.source_label().to_owned(),
            asset_paths: document
                .assets
                .iter()
                .map(|asset| asset.path.clone())
                .collect(),
            world_matrices,
            world_rotations,
            objects,
            indices_by_id,
            topological_order: order.into_iter().map(ObjectHandle).collect(),
            active_camera: document
                .objects
                .iter()
                .position(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                .or_else(|| {
                    document
                        .objects
                        .iter()
                        .position(|object| object.camera.is_some())
                })
                .map(ObjectHandle)
                .expect("SceneDocument validation requires a camera"),
            last_valid_camera: None,
            last_valid_light: LightState::default(),
            last_valid_local_lights: vec![LastValidLocalLights::default(); object_count],
            invalid_basis_reported: HashSet::new(),
            last_valid_environment: None,
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

    /// Returns the cached world rotation independent of scale, including zero scale.
    pub fn world_rotation(&self, handle: ObjectHandle) -> Option<Quat> {
        self.world_rotations.get(handle.0).copied()
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

    /// Sets world pose on a root object while preserving its local scale.
    pub fn set_root_pose(
        &mut self,
        handle: ObjectHandle,
        position: Vec3,
        rotation: Quat,
    ) -> Result<(), SceneError> {
        let length_squared = rotation.length_squared();
        if !position.is_finite()
            || !rotation.is_finite()
            || !length_squared.is_finite()
            || length_squared <= 1e-12
        {
            return Err(SceneError::new(
                &self.source_label,
                None,
                "root pose должен содержать конечные position и ненулевой rotation",
            ));
        }

        let Some(object) = self.objects.get_mut(handle.0) else {
            return Err(SceneError::new(
                &self.source_label,
                None,
                format!("неизвестный handle объекта {}", handle.0),
            ));
        };
        if object.parent.is_some() {
            return Err(SceneError::new(
                &self.source_label,
                Some(&object.id),
                "set_root_pose допускается только для корневого объекта",
            ));
        }

        let (yaw, pitch, roll) = rotation.normalize().to_euler(EulerRot::YXZ);
        object.transform.position = position;
        object.transform.rotation_deg =
            Vec3::new(pitch, yaw, roll) * (180.0 / std::f32::consts::PI);
        self.recalculate_world_matrices();
        Ok(())
    }

    pub fn world_matrix(&self, handle: ObjectHandle) -> Option<Mat4> {
        self.world_matrices.get(handle.0).copied()
    }

    /// Выбирает уровень по расстоянию до мировой позиции объекта.
    pub fn lod_level(&self, handle: ObjectHandle, eye: Vec3) -> Option<usize> {
        let object = self.object(handle)?;
        let Some(group) = &object.lod_group else {
            return Some(0);
        };
        let position = self.world_matrices.get(handle.0)?.w_axis.truncate();
        group.level_for_distance(eye.distance(position))
    }

    /// Камера объекта по текущим мировым матрицам; `None`, если камеры нет или базис невалиден.
    pub fn camera_of(&self, handle: ObjectHandle) -> Option<Camera> {
        let object = self.object(handle)?;
        object.camera.as_ref()?;
        let settings = object.camera_settings.unwrap_or_default();
        camera_from_matrix(self.world_matrix(handle)?, settings)
    }

    /// Звуковой источник объекта, если у него есть `audio_source`.
    pub fn audio_source(&self, handle: ObjectHandle) -> Option<&AudioSourceComponent> {
        self.object(handle)?.audio_source.as_ref()
    }

    /// Аниматор объекта, если у него есть `animator`.
    pub fn animator(&self, handle: ObjectHandle) -> Option<&AnimatorComponent> {
        self.object(handle)?.animator.as_ref()
    }

    /// Пути `[[assets]]` сцены в порядке документа.
    pub fn asset_paths(&self) -> &[VirtualPath] {
        &self.asset_paths
    }

    /// Настройки камеры; `None`, если объект не содержит Camera component.
    pub fn camera_settings(&self, handle: ObjectHandle) -> Option<&CameraSettings> {
        self.object(handle)?.camera.as_ref()?;
        self.object(handle)?.camera_settings.as_ref()
    }

    /// Проверяет и заменяет настройки существующей камеры.
    pub fn set_camera_settings(
        &mut self,
        handle: ObjectHandle,
        settings: CameraSettings,
    ) -> Result<(), SceneError> {
        settings.validate().map_err(|error| {
            let object_id = self.object(handle).map(|object| object.id.as_str());
            SceneError::new(&self.source_label, object_id, error.message)
        })?;
        let Some(object) = self.objects.get_mut(handle.0) else {
            return Err(SceneError::new(
                &self.source_label,
                None,
                format!("неизвестный handle объекта {}", handle.0),
            ));
        };
        if object.camera.is_none() {
            return Err(SceneError::new(
                &self.source_label,
                Some(&object.id),
                "у объекта нет Camera component",
            ));
        }
        object.camera_settings = Some(settings);
        Ok(())
    }

    /// Возвращает handle камеры, активной при построении кадра.
    pub fn active_camera(&self) -> ObjectHandle {
        self.active_camera
    }

    /// Возвращает флаги курсора активной камеры; без camera component — значения по умолчанию.
    pub fn active_camera_cursor(&self) -> CameraCursor {
        self.object(self.active_camera)
            .and_then(|object| object.camera.as_ref())
            .map(|camera| camera.cursor)
            .unwrap_or_default()
    }

    /// Назначает активной существующую камеру.
    pub fn set_active_camera(&mut self, handle: ObjectHandle) -> Result<(), SceneError> {
        let Some(object) = self.object(handle) else {
            return Err(SceneError::new(
                &self.source_label,
                None,
                format!("неизвестный handle объекта {}", handle.0),
            ));
        };
        if object.camera.is_none() {
            return Err(SceneError::new(
                &self.source_label,
                Some(&object.id),
                "у объекта нет Camera component",
            ));
        }
        self.active_camera = handle;
        Ok(())
    }

    /// Пересчитывает мировые матрицы в порядке от родителя к потомкам.
    pub fn recalculate_world_matrices(&mut self) {
        for handle in &self.topological_order {
            let object = &self.objects[handle.0];
            let local = object.transform.matrix();
            let parent = object.parent.map(|parent| parent.0);
            self.world_matrices[handle.0] =
                compose_world_matrix(local, parent, &self.world_matrices);
            let local_rotation = rotation_quat(object.transform);
            self.world_rotations[handle.0] = match parent {
                Some(parent) => (self.world_rotations[parent] * local_rotation).normalize(),
                None => local_rotation,
            };
        }
    }

    /// Строит кадр и возвращает первые предупреждения о невалидных базисах.
    /// Для камеры или света сохраняется последнее валидное динамическое состояние.
    /// Камера кадра задаётся `active_camera`; при невалидном базисе используется последнее валидное состояние.
    pub fn build_frame(&mut self) -> (SceneFrame, Vec<SceneWarning>) {
        self.recalculate_world_matrices();
        let mut warnings = Vec::new();
        let mut camera_state = self.last_valid_camera;
        let mut light = self.last_valid_light;
        let mut local_lights = Vec::new();

        for index in 0..self.objects.len() {
            let matrix = self.world_matrices[index];
            let camera_settings = (self.active_camera.0 == index)
                .then(|| self.objects[index].camera_settings.unwrap_or_default());
            let light_settings = self.objects[index].directional_light;
            let point_settings = self.objects[index].point_light;
            let spot_settings = self.objects[index].spot_light;
            if let Some(settings) = camera_settings {
                if let Some(value) = camera_from_matrix(matrix, settings) {
                    let state = (value, settings);
                    camera_state = Some(state);
                    self.last_valid_camera = Some(state);
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
            if let Some(settings) = point_settings {
                if let Some(position) = local_light_position_from_matrix(matrix) {
                    self.last_valid_local_lights[index].point = Some(LocalLightState::Point {
                        position,
                        color: settings.color,
                        intensity: settings.intensity,
                        range: settings.range,
                        shadows: settings.shadows,
                    });
                } else {
                    self.warn_invalid_basis(ObjectHandle(index), "точечного света", &mut warnings);
                }
            } else {
                self.last_valid_local_lights[index].point = None;
            }
            if let Some(settings) = spot_settings {
                if let (Some(position), Some(direction)) = (
                    local_light_position_from_matrix(matrix),
                    light_direction_from_matrix(matrix),
                ) {
                    self.last_valid_local_lights[index].spot = Some(LocalLightState::Spot {
                        position,
                        direction,
                        color: settings.color,
                        intensity: settings.intensity,
                        range: settings.range,
                        inner_angle_deg: settings.inner_angle_deg,
                        outer_angle_deg: settings.outer_angle_deg,
                        shadows: settings.shadows,
                    });
                } else {
                    self.warn_invalid_basis(ObjectHandle(index), "spot света", &mut warnings);
                }
            } else {
                self.last_valid_local_lights[index].spot = None;
            }
            if let Some(state) = self.last_valid_local_lights[index].point {
                local_lights.push(state);
            }
            if let Some(state) = self.last_valid_local_lights[index].spot {
                local_lights.push(state);
            }
        }

        // from_document проверяет сцену и сразу фиксирует её первый валидный базис камеры.
        let (camera, view) =
            camera_state.expect("World::from_document initializes the validated camera");
        let meshes = self
            .objects
            .iter()
            .filter(|object| object.mesh_renderer.is_some())
            .map(|object| MeshInstance {
                object: object.handle,
                model: self.world_matrices[object.handle.0],
            })
            .collect();
        let has_light = self
            .objects
            .iter()
            .any(|object| object.directional_light.is_some());
        let environment_owner = self
            .objects
            .iter()
            .position(|object| object.environment.is_some());
        let environment = environment_owner.and_then(|index| {
            let component = self.objects[index].environment.as_ref()?.clone();
            let rotation = self.world_rotations[index];
            let length_squared = rotation.length_squared();
            if !rotation.is_finite() || !length_squared.is_finite() || length_squared <= 1e-12 {
                self.warn_invalid_basis(ObjectHandle(index), "environment", &mut warnings);
                return self.last_valid_environment.clone();
            }
            let state = EnvironmentState {
                component,
                rotation: Mat3::from_quat(rotation.normalize()).to_cols_array_2d(),
                sun_direction: has_light.then(|| light.direction.map(|value| -value)),
            };
            self.last_valid_environment = Some(state.clone());
            Some(state)
        });

        (
            SceneFrame {
                camera,
                view,
                light,
                local_lights,
                environment,
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
        lod_group: object.lod_group.clone(),
        camera: object.camera.clone(),
        camera_settings: object.camera.as_ref().map(|_| CameraSettings::default()),
        directional_light: object.directional_light,
        point_light: object.point_light,
        spot_light: object.spot_light,
        environment: object.environment.clone(),
        collider: object.collider,
        rigid_body: object.rigid_body,
        character_controller: object.character_controller,
        audio_source: object.audio_source.clone(),
        animator: object.animator.clone(),
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

fn calculate_world_rotations(
    transforms: &[LocalTransform],
    parents: &[Option<usize>],
    order: &[usize],
) -> Vec<Quat> {
    let mut world_rotations = vec![Quat::IDENTITY; transforms.len()];
    for &index in order {
        let local = rotation_quat(transforms[index]);
        world_rotations[index] = match parents[index] {
            Some(parent) => (world_rotations[parent] * local).normalize(),
            None => local,
        };
    }
    world_rotations
}

fn rotation_quat(transform: LocalTransform) -> Quat {
    let rotation = transform.rotation_deg * (std::f32::consts::PI / 180.0);
    Quat::from_euler(EulerRot::YXZ, rotation.y, rotation.x, rotation.z).normalize()
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
        shadow: settings.shadows.then_some(ShadowSettings {
            distance: settings.shadow_distance,
            cascades: settings.shadow_cascades,
        }),
    }
}

fn local_light_position_from_matrix(matrix: Mat4) -> Option<[f32; 3]> {
    if !matrix.is_finite() {
        return None;
    }
    let position = matrix.w_axis.truncate();
    position.is_finite().then(|| position.to_array())
}

#[cfg(test)]
mod tests {
    use super::{LocalTransform, ObjectHandle, World};
    use crate::{SceneDocument, light::ShadowSettings};
    use glam::{EulerRot, Mat3, Mat4, Quat, Vec3};

    const CAMERA_SCENE: &str = r#"
[[objects]]
id = "camera"
[objects.camera]

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
            "{CAMERA_SCENE}\n[[objects]]\nid=\"sun\"\n[objects.transform]\nrotation=[0.0,90.0,0.0]\n[objects.directional_light]\nshadows=true\nshadow_distance=50.0\nshadow_cascades=2\n"
        );
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let (frame, _) = world.build_frame();
        assert!(Vec3::from_array(frame.light.direction).abs_diff_eq(Vec3::NEG_X, 1e-5));
        assert_eq!(
            frame.light.shadow,
            Some(ShadowSettings {
                distance: 50.0,
                cascades: 2,
            })
        );
    }

    #[test]
    fn environment_uses_owner_world_rotation_and_directional_light_sun_direction() {
        let scene = r#"
[[objects]]
id="camera"
[objects.camera]
[[objects]]
id="rig"
[objects.transform]
rotation=[0.0, 30.0, 0.0]
[[objects]]
id="environment"
parent="rig"
[objects.transform]
rotation=[0.0, 45.0, 0.0]
[objects.environment]
[objects.environment.procedural]
zenith_color=[0.2, 0.3, 0.5]
horizon_color=[0.8, 0.7, 0.5]
ground_color=[0.1, 0.1, 0.1]
sun_size_deg=3.0
sun_intensity=2.0
[[objects]]
id="sun"
parent="rig"
[objects.directional_light]
"#;
        let mut world = World::from_document(&parse(scene)).unwrap();
        let frame = world.build_frame().0;
        let environment = frame.environment.unwrap();
        let expected = Quat::from_rotation_y(75.0_f32.to_radians());
        let actual_rotation = Mat3::from_cols_array_2d(&environment.rotation);
        assert!(actual_rotation.abs_diff_eq(Mat3::from_quat(expected), 1e-5));
        let expected_sun = Quat::from_rotation_y(30.0_f32.to_radians()) * Vec3::Z;
        assert!(
            Vec3::from_array(environment.sun_direction.unwrap()).abs_diff_eq(expected_sun, 1e-5)
        );
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
    fn lod_level_uses_parented_world_origin_and_unconfigured_objects_use_level_zero() {
        let scene = format!(
            "[[assets]]\npath=\"/base.fbx\"\n[[assets]]\npath=\"/lod.fbx\"\n{CAMERA_SCENE}\n[[objects]]\nid=\"root\"\n[objects.transform]\nposition=[10.0,0.0,0.0]\n[[objects]]\nid=\"mesh\"\nparent=\"root\"\n[objects.transform]\nposition=[0.0,0.0,5.0]\n[objects.mesh_renderer]\nmodel=\"/base.fbx\"\n[objects.lod_group]\n[[objects.lod_group.levels]]\nmodel=\"/lod.fbx\"\ndistance=11.0\n"
        );
        let world = World::from_document(&parse(&scene)).unwrap();
        let mesh = world.handle("mesh").unwrap();
        let camera = world.handle("camera").unwrap();
        assert_eq!(world.lod_level(mesh, Vec3::ZERO), Some(1));
        assert_eq!(world.lod_level(mesh, Vec3::new(10.0, 0.0, 4.0)), Some(0));
        assert_eq!(world.lod_level(camera, Vec3::ZERO), Some(0));
    }

    #[test]
    fn zero_scale_camera_or_light_basis_is_rejected_during_load() {
        let camera = r#"
[[objects]]
id="camera"
parent="rig"
[objects.camera]

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
    fn set_root_pose_preserves_scale_and_updates_child_matrices() {
        let input = format!(
            "{CAMERA_SCENE}\n[[objects]]\nid=\"child\"\nparent=\"camera\"\n\
             [objects.transform]\nposition=[1.0,2.0,3.0]\nrotation=[10.0,20.0,30.0]\n"
        );
        let mut world = World::from_document(&parse(&input)).unwrap();
        let root = world.handle("camera").unwrap();
        let child = world.handle("child").unwrap();
        let scale = Vec3::new(-2.0, 3.0, 0.5);
        world
            .set_local_transform(
                root,
                LocalTransform {
                    scale,
                    ..LocalTransform::default()
                },
            )
            .unwrap();
        world.recalculate_world_matrices();

        let position = Vec3::new(4.0, -3.0, 7.0);
        let rotation = Quat::from_euler(EulerRot::YXZ, 0.7, -0.4, 0.2);
        world.set_root_pose(root, position, rotation).unwrap();

        let root_transform = *world.local_transform(root).unwrap();
        assert_eq!(root_transform.scale, scale);
        assert!(root_transform.position.abs_diff_eq(position, 1e-6));
        assert!(
            world
                .world_rotation(root)
                .unwrap()
                .abs_diff_eq(rotation, 1e-5)
        );
        let expected_child =
            root_transform.matrix() * world.local_transform(child).unwrap().matrix();
        assert!(
            world
                .world_matrix(child)
                .unwrap()
                .abs_diff_eq(expected_child, 1e-5)
        );
    }

    #[test]
    fn set_root_pose_rejects_non_root_objects() {
        let input = format!("{CAMERA_SCENE}\n[[objects]]\nid=\"child\"\nparent=\"camera\"\n");
        let mut world = World::from_document(&parse(&input)).unwrap();
        let child = world.handle("child").unwrap();
        let error = world
            .set_root_pose(child, Vec3::ONE, Quat::IDENTITY)
            .unwrap_err();
        assert!(error.message.contains("только для корневого"));
    }

    #[test]
    fn world_rotation_cache_is_valid_for_zero_scale_objects() {
        let input = format!(
            "{CAMERA_SCENE}\n[[objects]]\nid=\"collapsed\"\n\
             [objects.transform]\nscale=[0.0,0.0,0.0]\nrotation=[15.0,25.0,35.0]\n"
        );
        let world = World::from_document(&parse(&input)).unwrap();
        let collapsed = world.handle("collapsed").unwrap();
        let rotation = world.world_rotation(collapsed).unwrap();
        assert!(rotation.is_finite());
        assert!(rotation.abs_diff_eq(
            Quat::from_euler(
                EulerRot::YXZ,
                25.0_f32.to_radians(),
                15.0_f32.to_radians(),
                35.0_f32.to_radians(),
            ),
            1e-5
        ));
    }

    #[test]
    fn explicit_active_camera_drives_the_frame_and_camera_settings_are_per_camera() {
        let document = SceneDocument::parse(
            "cameras.rvescene",
            r#"
[[objects]]
id="main"
[objects.transform]
position=[1.0,0.0,0.0]
[objects.camera]
active=true

[[objects]]
id="second"
[objects.transform]
position=[2.0,0.0,0.0]
[objects.camera]

"#,
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let main = world.handle("main").unwrap();
        let main_settings =
            crate::CameraSettings::parse("main.camerasettings", "[color]\nexposure_ev = 2.0\n")
                .unwrap();
        world.set_camera_settings(main, main_settings).unwrap();
        assert_eq!(world.build_frame().0.camera.position.x, 1.0);
        assert_eq!(world.build_frame().0.view.color.exposure_ev, 2.0);
        let second = world.handle("second").unwrap();
        assert_eq!(world.camera_of(second).unwrap().position.x, 2.0);
        assert!(world.camera_settings(second).is_some());
        world.set_active_camera(second).unwrap();
        let (frame, _) = world.build_frame();
        assert_eq!(frame.camera.position.x, 2.0);
        assert_eq!(frame.view, crate::CameraSettings::default());
        let mut moved = *world.local_transform(second).unwrap();
        moved.position.x = 5.0;
        world.set_local_transform(second, moved).unwrap();
        assert_eq!(world.build_frame().0.camera.position.x, 5.0);
        assert_eq!(world.camera_of(second).unwrap().position.x, 5.0);
        assert_eq!(world.active_camera(), second);
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
    fn switching_to_invalid_camera_keeps_the_last_valid_camera_and_view() {
        let document = SceneDocument::parse(
            "switch.rvescene",
            r#"
[[objects]]
id="first"
[objects.camera]
active=true
[[objects]]
id="second"
[objects.camera]
"#,
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let first = world.handle("first").unwrap();
        let second = world.handle("second").unwrap();
        let settings =
            crate::CameraSettings::parse("first.camerasettings", "[color]\nexposure_ev = 3.0\n")
                .unwrap();
        world.set_camera_settings(first, settings).unwrap();
        let (valid, _) = world.build_frame();
        world
            .set_local_transform(
                second,
                LocalTransform {
                    scale: Vec3::new(1.0, 0.0, 1.0),
                    ..LocalTransform::default()
                },
            )
            .unwrap();
        world.set_active_camera(second).unwrap();
        let (fallback, warnings) = world.build_frame();
        assert_eq!(fallback.camera, valid.camera);
        assert_eq!(fallback.view, settings);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].object_id, "second");
    }

    #[test]
    fn camera_mutators_reject_non_cameras_and_unknown_handles() {
        let scene = format!("{CAMERA_SCENE}\n[[objects]]\nid=\"mesh\"\n");
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let mesh = world.handle("mesh").unwrap();
        let camera = world.handle("camera").unwrap();
        let defaults = crate::CameraSettings::default();
        assert!(world.set_active_camera(mesh).is_err());
        assert!(world.set_camera_settings(mesh, defaults).is_err());
        assert!(world.set_active_camera(ObjectHandle(99)).is_err());
        assert!(
            world
                .set_camera_settings(ObjectHandle(99), defaults)
                .is_err()
        );
        assert!(
            world
                .set_camera_settings(
                    camera,
                    crate::CameraSettings {
                        render: crate::CameraRenderSettings {
                            render_scale: 0.0,
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                )
                .is_err()
        );
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
    fn local_light_world_position_and_spot_direction_follow_full_hierarchy_matrix() {
        let scene = format!(
            "{CAMERA_SCENE}\n[[objects]]\nid=\"rig\"\n[objects.transform]\nposition=[3.0,2.0,-4.0]\nrotation=[15.0,35.0,5.0]\nscale=[-2.0,3.0,0.75]\n[[objects]]\nid=\"lamp\"\nparent=\"rig\"\n[objects.transform]\nposition=[1.0,0.5,-2.0]\nrotation=[20.0,-10.0,30.0]\n[objects.point_light]\nrange=15.0\n[[objects]]\nid=\"spot\"\nparent=\"rig\"\n[objects.transform]\nposition=[-1.0,0.25,2.0]\nrotation=[10.0,25.0,0.0]\n[objects.spot_light]\n"
        );
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let frame = world.build_frame().0;
        let lamp_matrix = world.world_matrix(world.handle("lamp").unwrap()).unwrap();
        let spot_matrix = world.world_matrix(world.handle("spot").unwrap()).unwrap();
        let expected_position = lamp_matrix.w_axis.truncate().to_array();
        let expected_spot_position = spot_matrix.w_axis.truncate().to_array();
        let expected_direction = spot_matrix
            .transform_vector3(Vec3::NEG_Z)
            .normalize()
            .to_array();
        assert_eq!(frame.local_lights.len(), 2);
        assert!(matches!(
            frame.local_lights[0],
            super::LocalLightState::Point { position, range: 15.0, .. }
                if position == expected_position
        ));
        assert!(matches!(
            frame.local_lights[1],
            super::LocalLightState::Spot { position, direction, .. }
                if position == expected_spot_position
                    && Vec3::from_array(direction).abs_diff_eq(Vec3::from_array(expected_direction), 1e-6)
        ));
    }

    #[test]
    fn dynamic_invalid_spot_basis_keeps_last_valid_state_and_warns_once() {
        let scene = format!("{CAMERA_SCENE}\n[[objects]]\nid=\"spot\"\n[objects.spot_light]\n");
        let mut world = World::from_document(&parse(&scene)).unwrap();
        let handle = world.handle("spot").unwrap();
        let previous = world.build_frame().0.local_lights;
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
        assert_eq!(first.local_lights, previous);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].object_id, "spot");
        assert!(warnings[0].message.contains("spot"));
        let (second, warnings) = world.build_frame();
        assert_eq!(second.local_lights, previous);
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_initial_spot_basis_is_rejected() {
        let scene = format!(
            "{CAMERA_SCENE}\n[[objects]]\nid=\"spot\"\n[objects.transform]\nscale=[1.0,1.0,0.0]\n[objects.spot_light]\n"
        );
        let error = SceneDocument::parse("invalid-spot.rvescene", &scene).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("spot"));
        assert!(error.message.contains("spot_light"));
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

    #[test]
    fn active_camera_cursor_comes_from_the_selected_camera_only() {
        let document = parse(
            r#"
[[objects]]
id = "first"
[objects.camera]
[objects.camera.cursor]
hide_on_start = true

[[objects]]
id = "second"
[objects.camera]
active = true
[objects.camera.cursor]
lock_on_start = true
"#,
        );
        let world = World::from_document(&document).unwrap();
        let cursor = world.active_camera_cursor();
        assert!(!cursor.hide_on_start);
        assert!(cursor.lock_on_start);
    }
}
