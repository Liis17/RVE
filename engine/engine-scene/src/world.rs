//! Runtime-данные сцены и подготовка кадра без зависимости от renderer.

use crate::CameraSettings;
use crate::animation::AnimatorComponent;
use crate::audio::AudioSourceComponent;
use crate::camera::Camera;
use crate::document::{
    CameraComponent, CameraCursor, CharacterControllerComponent, ColliderComponent,
    DirectionalLightComponent, EnvironmentComponent, HeavyScript, LiteScript, LodGroup,
    PointLightComponent, RigidBodyComponent, RigidBodyKind, SceneDocument, SceneError, SceneObject,
    SpotLightComponent, camera_from_matrix, light_direction_from_matrix, valid_ascii_name,
};
use crate::light::ShadowSettings;
use crate::network::{NetworkIdentityComponent, NetworkTransformComponent};
use crate::particles::ParticleEmitterComponent;
use crate::physics_material::resolve_collider_material;
use crate::post_process::PostProcessSettings;
use crate::prefab::{MAX_EXPANDED_ID_BYTES, PrefabTemplate};
use crate::ui_canvas::UiCanvasComponent;
use engine_files::VirtualPath;
use glam::{EulerRot, Mat3, Mat4, Quat, Vec3};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Наибольшее поколение слота. Слот, поколение которого исчерпано, больше не переиспользуется.
pub const MAX_GENERATION: u32 = i32::MAX as u32;
/// Наибольшее число живых объектов в одном мире.
pub const MAX_LIVE_OBJECTS: usize = 65_535;

/// Хендл объекта мира: индекс слота и поколение. Освобождение слота увеличивает поколение,
/// поэтому старый хендл перестаёт быть живым даже после переиспользования слота.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectHandle {
    index: u32,
    generation: u32,
}

impl ObjectHandle {
    pub fn index(self) -> usize {
        self.index as usize
    }

    pub fn generation(self) -> u32 {
        self.generation
    }

    /// ABI-значение `(generation << 32) | (index + 1)`. Ноль в ABI означает «нет объекта».
    /// `None`, если `index + 1` не помещается в 32 бита.
    pub fn to_abi(self) -> Option<u64> {
        let slot = self.index.checked_add(1)?;
        Some((u64::from(self.generation) << 32) | u64::from(slot))
    }

    /// Декодирует ABI-значение. Ноль и поколение больше [`MAX_GENERATION`] не дают хендла.
    /// Живость объекта проверяет [`World::is_alive`].
    pub fn from_abi(value: u64) -> Option<Self> {
        let slot = (value & u64::from(u32::MAX)) as u32;
        let generation = (value >> 32) as u32;
        if slot == 0 || generation > MAX_GENERATION {
            return None;
        }
        Some(Self {
            index: slot - 1,
            generation,
        })
    }
}

/// Хендл объекта документа: поколение 0, индекс — позиция в документе.
fn document_handle(index: usize) -> ObjectHandle {
    ObjectHandle {
        index: u32::try_from(index).expect("индекс объекта помещается в u32"),
        generation: 0,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    /// Слот свободен или выведен из обращения (исчерпано поколение).
    Vacant,
    /// Слот зарезервирован под объект, которого ещё нет в мире.
    Reserved,
    /// Слот занят живым объектом с этим плотным индексом.
    Live(usize),
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    generation: u32,
    state: SlotState,
}

/// Таблица слотов: свободные слоты переиспользуются, исчерпанные выводятся из обращения.
#[derive(Clone, Debug, Default)]
struct SlotTable {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl SlotTable {
    fn for_document(object_count: usize) -> Self {
        Self {
            slots: (0..object_count)
                .map(|index| Slot {
                    generation: 0,
                    state: SlotState::Live(index),
                })
                .collect(),
            free: Vec::new(),
        }
    }

    fn reserve(&mut self) -> ObjectHandle {
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    state: SlotState::Vacant,
                });
                u32::try_from(self.slots.len() - 1).expect("слоты объектов помещаются в u32")
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.state = SlotState::Reserved;
        ObjectHandle {
            index,
            generation: slot.generation,
        }
    }

    /// Слот, если хендл относится к его текущему поколению.
    fn slot(&self, handle: ObjectHandle) -> Option<&Slot> {
        self.slots
            .get(handle.index as usize)
            .filter(|slot| slot.generation == handle.generation)
    }

    fn live_index(&self, handle: ObjectHandle) -> Option<usize> {
        match self.slot(handle)?.state {
            SlotState::Live(index) => Some(index),
            SlotState::Vacant | SlotState::Reserved => None,
        }
    }

    fn is_reserved(&self, handle: ObjectHandle) -> bool {
        self.slot(handle)
            .is_some_and(|slot| slot.state == SlotState::Reserved)
    }

    /// Переводит зарезервированный слот в живое состояние с плотным индексом `index`.
    fn set_live(&mut self, handle: ObjectHandle, index: usize) {
        self.slots[handle.index as usize].state = SlotState::Live(index);
    }

    /// Освобождает слот. Поколение растёт; если оно исчерпано, слот больше не используется.
    fn vacate(&mut self, index: u32) {
        let slot = &mut self.slots[index as usize];
        slot.state = SlotState::Vacant;
        if slot.generation < MAX_GENERATION {
            slot.generation += 1;
            self.free.push(index);
        }
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
    /// Разрешённая постобработка камеры; есть только у объектов с Camera component.
    pub post_process: Option<PostProcessSettings>,
    pub directional_light: Option<DirectionalLightComponent>,
    pub point_light: Option<PointLightComponent>,
    pub spot_light: Option<SpotLightComponent>,
    pub environment: Option<EnvironmentComponent>,
    pub collider: Option<ColliderComponent>,
    pub rigid_body: Option<RigidBodyComponent>,
    pub character_controller: Option<CharacterControllerComponent>,
    pub audio_source: Option<AudioSourceComponent>,
    pub animator: Option<AnimatorComponent>,
    pub particle_emitter: Option<ParticleEmitterComponent>,
    pub network_identity: Option<NetworkIdentityComponent>,
    pub network_transform: Option<NetworkTransformComponent>,
    pub ui_canvas: Option<UiCanvasComponent>,
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

/// Эмиттер частиц объекта с мировой матрицей на текущий кадр.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParticleEmitterInstance {
    pub object: ObjectHandle,
    pub model: Mat4,
}

/// Данные мирового пространства для отрисовки одного кадра сцены.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneFrame {
    pub camera: Camera,
    pub view: CameraSettings,
    /// Постобработка активной камеры.
    pub post_process: PostProcessSettings,
    pub light: LightState,
    pub local_lights: Vec<LocalLightState>,
    pub environment: Option<EnvironmentState>,
    pub meshes: Vec<MeshInstance>,
    /// Эмиттеры частиц в порядке объектов сцены.
    pub particle_emitters: Vec<ParticleEmitterInstance>,
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

/// Параметры spawn: переопределяют transform и имя корня, задают родителя и id.
/// Неуказанные поля берутся из префаба.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnOptions {
    pub position: Option<Vec3>,
    pub rotation_deg: Option<Vec3>,
    pub scale: Option<Vec3>,
    pub parent: Option<ObjectHandle>,
    pub name: Option<String>,
    /// Авторский id корня `[A-Za-z0-9_-]{1,64}`; без него генерируется `<stem>@<serial>`,
    /// дети получают `<корень>.<id>`.
    pub id: Option<String>,
}

/// Runtime-граф сцены без зависимостей от renderer и script runtime.
///
/// Хендлы объектов проверяются по поколению. Объекты документа имеют поколение 0.
/// `objects()` содержит только живые объекты: при удалении порядок сохраняется,
/// новые объекты дописываются в конец.
#[derive(Clone, Debug)]
pub struct World {
    source_label: String,
    asset_paths: Vec<VirtualPath>,
    /// Слоты и поколения. Резервирование меняет их через `&self`.
    slots: RefCell<SlotTable>,
    objects: Vec<WorldObject>,
    indices_by_id: HashMap<String, ObjectHandle>,
    /// Плотные индексы в порядке «родитель раньше потомка».
    topological_order: Vec<usize>,
    world_matrices: Vec<Mat4>,
    world_rotations: Vec<Quat>,
    active_camera: ObjectHandle,
    last_valid_camera: Option<(Camera, CameraSettings)>,
    last_valid_light: LightState,
    last_valid_local_lights: Vec<LastValidLocalLights>,
    last_valid_environment: Option<EnvironmentState>,
    invalid_basis_reported: HashSet<ObjectHandle>,
    /// Номер для генерации id `<stem>@<serial>`.
    spawn_serial: u64,
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
            .map(|(index, object)| (object.id.clone(), document_handle(index)))
            .collect::<HashMap<_, _>>();
        let mut objects = Vec::with_capacity(document.objects.len());
        for (index, object) in document.objects.iter().enumerate() {
            objects.push(world_object(
                document_handle(index),
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
            .map(|object| object.parent.map(|parent| parent.index()))
            .collect::<Vec<_>>();
        let object_ids = objects
            .iter()
            .map(|object| object.id.as_str())
            .collect::<Vec<_>>();
        let (world_matrices, order) =
            calculate_world_matrices(&transforms, &parents, &object_ids, document.source_label())?;
        let world_rotations = calculate_world_rotations(&transforms, &parents, &order);
        let object_count = objects.len();
        let active_index = document
            .objects
            .iter()
            .position(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
            .or_else(|| {
                document
                    .objects
                    .iter()
                    .position(|object| object.camera.is_some())
            })
            .expect("SceneDocument validation requires a camera");
        let mut world = Self {
            source_label: document.source_label().to_owned(),
            asset_paths: document
                .assets
                .iter()
                .map(|asset| asset.path.clone())
                .collect(),
            world_matrices,
            world_rotations,
            active_camera: objects[active_index].handle,
            objects,
            indices_by_id,
            slots: RefCell::new(SlotTable::for_document(object_count)),
            topological_order: order,
            last_valid_camera: None,
            last_valid_light: LightState::default(),
            last_valid_local_lights: vec![LastValidLocalLights::default(); object_count],
            invalid_basis_reported: HashSet::new(),
            last_valid_environment: None,
            spawn_serial: 0,
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

    /// Плотный индекс живого объекта; `None` для устаревшего, зарезервированного или чужого хендла.
    fn dense(&self, handle: ObjectHandle) -> Option<usize> {
        self.slots.borrow().live_index(handle)
    }

    /// Живой объект с этим хендлом (проверяется поколение).
    pub fn is_alive(&self, handle: ObjectHandle) -> bool {
        self.dense(handle).is_some()
    }

    /// Хендл зарезервирован и ждёт [`World::spawn`] или [`World::release_reservation`].
    pub fn is_reserved(&self, handle: ObjectHandle) -> bool {
        self.slots.borrow().is_reserved(handle)
    }

    /// Резервирует слот под будущий объект. Хендл не живёт, пока его не примет `spawn`.
    pub fn reserve_object(&self) -> ObjectHandle {
        self.slots.borrow_mut().reserve()
    }

    /// Освобождает резерв и увеличивает поколение. `false`, если хендл не зарезервирован.
    pub fn release_reservation(&self, handle: ObjectHandle) -> bool {
        let mut slots = self.slots.borrow_mut();
        if !slots.is_reserved(handle) {
            return false;
        }
        slots.vacate(handle.index);
        true
    }

    pub fn object(&self, handle: ObjectHandle) -> Option<&WorldObject> {
        self.dense(handle).map(|index| &self.objects[index])
    }

    pub fn object_by_id(&self, id: &str) -> Option<&WorldObject> {
        self.handle(id).and_then(|handle| self.object(handle))
    }

    pub fn local_transform(&self, handle: ObjectHandle) -> Option<&LocalTransform> {
        self.object(handle).map(|object| &object.transform)
    }

    /// Returns the cached world rotation independent of scale, including zero scale.
    pub fn world_rotation(&self, handle: ObjectHandle) -> Option<Quat> {
        self.world_rotations.get(self.dense(handle)?).copied()
    }

    /// Заменяет локальную трансформацию объекта, сохраняя углы Эйлера в градусах.
    pub fn set_local_transform(
        &mut self,
        handle: ObjectHandle,
        transform: LocalTransform,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        self.objects[index].transform = transform;
        Ok(())
    }

    fn live_index_or_error(&self, handle: ObjectHandle) -> Result<usize, SceneError> {
        self.dense(handle).ok_or_else(|| {
            SceneError::new(
                &self.source_label,
                None,
                format!("неизвестный handle объекта {}", handle.index()),
            )
        })
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

        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
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
        self.world_matrices.get(self.dense(handle)?).copied()
    }

    /// Выбирает уровень по расстоянию до мировой позиции объекта.
    pub fn lod_level(&self, handle: ObjectHandle, eye: Vec3) -> Option<usize> {
        let index = self.dense(handle)?;
        let Some(group) = &self.objects[index].lod_group else {
            return Some(0);
        };
        let position = self.world_matrices[index].w_axis.truncate();
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
    /// Эмиттер частиц объекта, если у него есть `particle_emitter`.
    pub fn particle_emitter(&self, handle: ObjectHandle) -> Option<&ParticleEmitterComponent> {
        self.object(handle)?.particle_emitter.as_ref()
    }

    /// Сетевой идентификатор объекта, если у него есть `network_identity`.
    pub fn network_identity(&self, handle: ObjectHandle) -> Option<&NetworkIdentityComponent> {
        self.object(handle)?.network_identity.as_ref()
    }

    /// Сетевая трансформация объекта, если у него есть `network_transform`.
    pub fn network_transform(&self, handle: ObjectHandle) -> Option<&NetworkTransformComponent> {
        self.object(handle)?.network_transform.as_ref()
    }

    /// Игровой UI объекта, если у него есть `ui_canvas`.
    pub fn ui_canvas(&self, handle: ObjectHandle) -> Option<&UiCanvasComponent> {
        self.object(handle)?.ui_canvas.as_ref()
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
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
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

    /// Постобработка камеры; `None`, если объект не содержит Camera component.
    pub fn post_process(&self, handle: ObjectHandle) -> Option<&PostProcessSettings> {
        self.object(handle)?.camera.as_ref()?;
        self.object(handle)?.post_process.as_ref()
    }

    /// Проверяет и заменяет постобработку существующей камеры. Общий файл `.postprocess`
    /// не меняется: правится только копия этой камеры.
    pub fn set_post_process(
        &mut self,
        handle: ObjectHandle,
        settings: PostProcessSettings,
    ) -> Result<(), SceneError> {
        settings.validate().map_err(|error| {
            let object_id = self.object(handle).map(|object| object.id.as_str());
            SceneError::new(&self.source_label, object_id, error.message)
        })?;
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.camera.is_none() {
            return Err(SceneError::new(
                &self.source_label,
                Some(&object.id),
                "у объекта нет Camera component",
            ));
        }
        object.post_process = Some(settings);
        Ok(())
    }

    /// Читает `.rvephysmat` всех коллайдеров мира и заполняет их разрешённые материалы.
    pub fn resolve_physics_materials(
        &mut self,
        read: &mut dyn FnMut(&VirtualPath) -> Result<String, String>,
    ) -> Result<(), SceneError> {
        let mut cache = BTreeMap::new();
        for object in &mut self.objects {
            if let Some(collider) = object.collider.as_mut() {
                resolve_collider_material(
                    collider,
                    &self.source_label,
                    &object.id,
                    read,
                    &mut cache,
                )?;
            }
        }
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
        let index = self.live_index_or_error(handle)?;
        let object = &self.objects[index];
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
        for position in 0..self.topological_order.len() {
            let index = self.topological_order[position];
            let parent = self.parent_index(index);
            self.update_world_transform(index, parent);
        }
    }

    /// Плотный индекс родителя объекта с плотным индексом `index`.
    fn parent_index(&self, index: usize) -> Option<usize> {
        self.objects[index]
            .parent
            .and_then(|parent| self.dense(parent))
    }

    /// Пересчитывает мировую матрицу и вращение объекта; родитель уже должен быть пересчитан.
    fn update_world_transform(&mut self, index: usize, parent: Option<usize>) {
        let transform = self.objects[index].transform;
        let local = transform.matrix();
        self.world_matrices[index] = compose_world_matrix(local, parent, &self.world_matrices);
        let local_rotation = rotation_quat(transform);
        self.world_rotations[index] = match parent {
            Some(parent) => (self.world_rotations[parent] * local_rotation).normalize(),
            None => local_rotation,
        };
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
            let camera_settings = (self.active_camera == self.objects[index].handle)
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
                    self.warn_invalid_basis(index, "камеры", &mut warnings);
                }
            }
            if let Some(settings) = light_settings {
                if let Some(direction) = light_direction_from_matrix(matrix) {
                    let value = light_state(settings, direction);
                    light = value;
                    self.last_valid_light = value;
                } else {
                    self.warn_invalid_basis(index, "направленного света", &mut warnings);
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
                    self.warn_invalid_basis(index, "точечного света", &mut warnings);
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
                    self.warn_invalid_basis(index, "spot света", &mut warnings);
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
            .enumerate()
            .filter(|(_, object)| object.mesh_renderer.is_some())
            .map(|(index, object)| MeshInstance {
                object: object.handle,
                model: self.world_matrices[index],
            })
            .collect();
        let particle_emitters = self
            .objects
            .iter()
            .enumerate()
            .filter(|(_, object)| object.particle_emitter.is_some())
            .map(|(index, object)| ParticleEmitterInstance {
                object: object.handle,
                model: self.world_matrices[index],
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
                self.warn_invalid_basis(index, "environment", &mut warnings);
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

        let post_process = self
            .object(self.active_camera)
            .and_then(|object| object.post_process)
            .unwrap_or_default();

        (
            SceneFrame {
                camera,
                view,
                post_process,
                light,
                local_lights,
                environment,
                meshes,
                particle_emitters,
            },
            warnings,
        )
    }

    fn warn_invalid_basis(
        &mut self,
        index: usize,
        component: &str,
        warnings: &mut Vec<SceneWarning>,
    ) {
        let object = &self.objects[index];
        if self.invalid_basis_reported.insert(object.handle) {
            warnings.push(SceneWarning {
                source: self.source_label.clone(),
                object_id: object.id.clone(),
                message: format!("невалидный базис {component}"),
            });
        }
    }
}

impl World {
    /// Создаёт объекты шаблона на зарезервированных хендлах, по одному на объект шаблона
    /// в его порядке; возвращает эти хендлы. При отказе резервы освобождаются, мир не меняется.
    ///
    /// Явный id проверяется по формату `[A-Za-z0-9_-]{1,64}` и на коллизию с живыми объектами;
    /// раскрытые id не длиннее 255 байт. Сгенерированный id занимает первый свободный номер.
    /// Резервы без id проверяет вызывающая сторона: `World` их не знает.
    pub fn spawn(
        &mut self,
        reserved: Vec<ObjectHandle>,
        template: &PrefabTemplate,
        options: SpawnOptions,
    ) -> Result<Vec<ObjectHandle>, SceneError> {
        let (root_id, serial) = match &options.id {
            Some(id) => (id.clone(), None),
            None => {
                let serial = self.free_generated_serial(template);
                (format!("{}@{serial}", template.stem()), Some(serial))
            }
        };
        let ids = match self.check_spawn(&reserved, template, &options, &root_id) {
            Ok(ids) => ids,
            Err(error) => {
                for handle in reserved {
                    self.release_reservation(handle);
                }
                return Err(error);
            }
        };
        if let Some(serial) = serial {
            self.spawn_serial = serial + 1;
        }
        self.insert_spawned(&reserved, template, options, ids);
        Ok(reserved)
    }

    /// Первый номер `<stem>@<serial>`, при котором ни корень, ни дети не заняты живыми объектами.
    /// Занятые номера пропускаются, поэтому автоидентификатор не блокируется чужим id.
    fn free_generated_serial(&self, template: &PrefabTemplate) -> u64 {
        (self.spawn_serial..)
            .find(|serial| {
                let root_id = format!("{}@{serial}", template.stem());
                spawned_ids(&root_id, template)
                    .iter()
                    .all(|id| !self.indices_by_id.contains_key(id))
            })
            .expect("номера генерируемых id не исчерпаны")
    }

    fn check_spawn(
        &self,
        reserved: &[ObjectHandle],
        template: &PrefabTemplate,
        options: &SpawnOptions,
        root_id: &str,
    ) -> Result<Vec<String>, SceneError> {
        template.check_spawnable()?;
        let source = &self.source_label;
        if let Some(id) = &options.id
            && !valid_ascii_name(id, true)
        {
            return Err(SceneError::new(
                source,
                Some(id.as_str()),
                "id spawn-объекта должен соответствовать [A-Za-z0-9_-]{1,64}",
            ));
        }
        if reserved.len() != template.objects.len() {
            return Err(SceneError::new(
                source,
                Some(root_id),
                format!(
                    "для префаба нужно {} резервов, получено {}",
                    template.objects.len(),
                    reserved.len()
                ),
            ));
        }
        let mut distinct = HashSet::with_capacity(reserved.len());
        if !reserved
            .iter()
            .all(|&handle| self.is_reserved(handle) && distinct.insert(handle))
        {
            return Err(SceneError::new(
                source,
                Some(root_id),
                "резерв не активен или повторяется",
            ));
        }
        if self.objects.len() + reserved.len() > MAX_LIVE_OBJECTS {
            return Err(SceneError::new(
                source,
                Some(root_id),
                format!("превышен лимит живых объектов {MAX_LIVE_OBJECTS}"),
            ));
        }
        if [options.position, options.rotation_deg, options.scale]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite())
        {
            return Err(SceneError::new(
                source,
                Some(root_id),
                "transform spawn-объекта должен содержать конечные числа",
            ));
        }
        if let Some(parent) = options.parent {
            if !self.is_alive(parent) {
                return Err(SceneError::new(
                    source,
                    Some(root_id),
                    "родитель spawn-объекта не существует",
                ));
            }
            let root = &template.objects[template.root];
            let dynamic = root
                .rigid_body
                .as_ref()
                .is_some_and(|body| matches!(body.kind, RigidBodyKind::Dynamic));
            if dynamic || root.character_controller.is_some() {
                return Err(SceneError::new(
                    source,
                    Some(root_id),
                    "dynamic rigid_body и character_controller spawn-объекта допускаются только без родителя",
                ));
            }
        }
        let ids = spawned_ids(root_id, template);
        if let Some(id) = ids.iter().find(|id| id.len() > MAX_EXPANDED_ID_BYTES) {
            return Err(SceneError::new(
                source,
                Some(id.as_str()),
                format!("раскрытый id длиннее {MAX_EXPANDED_ID_BYTES} байт"),
            ));
        }
        {
            let mut seen = HashSet::with_capacity(ids.len());
            for id in &ids {
                if self.indices_by_id.contains_key(id) || !seen.insert(id.as_str()) {
                    return Err(SceneError::new(
                        source,
                        Some(id.as_str()),
                        "id объекта уже используется",
                    ));
                }
            }
        }
        Ok(ids)
    }

    /// Добавляет объекты в конец плотных массивов и пересчитывает их матрицы.
    /// Проверки уже пройдены, поэтому метод не возвращает ошибок.
    fn insert_spawned(
        &mut self,
        reserved: &[ObjectHandle],
        template: &PrefabTemplate,
        options: SpawnOptions,
        ids: Vec<String>,
    ) {
        let base = self.objects.len();
        for (local, object) in template.objects.iter().enumerate() {
            let handle = reserved[local];
            let mut scene_object = object.clone();
            scene_object.id = ids[local].clone();
            let parent = match template.parents[local] {
                Some(parent) => Some(reserved[parent]),
                None => options.parent,
            };
            let is_root = local == template.root;
            if is_root && let Some(name) = &options.name {
                scene_object.name = Some(name.clone());
            }
            let mut spawned = world_object(handle, &scene_object, parent);
            if is_root {
                let transform = &mut spawned.transform;
                transform.position = options.position.unwrap_or(transform.position);
                transform.rotation_deg = options.rotation_deg.unwrap_or(transform.rotation_deg);
                transform.scale = options.scale.unwrap_or(transform.scale);
            }
            self.objects.push(spawned);
            self.world_matrices.push(Mat4::IDENTITY);
            self.world_rotations.push(Quat::IDENTITY);
            self.last_valid_local_lights
                .push(LastValidLocalLights::default());
            self.indices_by_id.insert(ids[local].clone(), handle);
            self.slots.borrow_mut().set_live(handle, base + local);
        }
        for &local in &template.order {
            self.topological_order.push(base + local);
        }
        for &local in &template.order {
            let index = base + local;
            let parent = self.parent_index(index);
            self.update_world_transform(index, parent);
        }
    }

    /// Живые объекты поддеревьев `roots`, включая сами корни, в порядке «родитель раньше потомка».
    /// Хендлы, которые не живы, пропускаются. Один проход по порядку топологии: потомок попадает
    /// в результат, если его родитель уже отмечен.
    pub fn subtree(&self, roots: &[ObjectHandle]) -> Vec<ObjectHandle> {
        let mut marked = vec![false; self.objects.len()];
        for &handle in roots {
            if let Some(index) = self.dense(handle) {
                marked[index] = true;
            }
        }
        let mut result = Vec::new();
        for &index in &self.topological_order {
            if marked[index]
                || self
                    .parent_index(index)
                    .is_some_and(|parent| marked[parent])
            {
                marked[index] = true;
                result.push(self.objects[index].handle);
            }
        }
        result
    }

    /// Удаляет поддеревья хендлов. Возвращает удалённые хендлы: дети раньше родителей.
    ///
    /// Отказывает, если хендл не жив или в поддереве находится активная камера; при отказе
    /// мир не меняется. Плотные массивы уплотняются за один проход.
    pub fn despawn_many(
        &mut self,
        handles: &[ObjectHandle],
    ) -> Result<Vec<ObjectHandle>, SceneError> {
        let mut removed = vec![false; self.objects.len()];
        for &handle in handles {
            removed[self.live_index_or_error(handle)?] = true;
        }
        for &index in &self.topological_order {
            if let Some(parent) = self.parent_index(index)
                && removed[parent]
            {
                removed[index] = true;
            }
        }
        for (object, &gone) in self.objects.iter().zip(&removed) {
            if gone && object.handle == self.active_camera {
                return Err(SceneError::new(
                    &self.source_label,
                    Some(&object.id),
                    "активную камеру нельзя удалить",
                ));
            }
        }

        let removed_handles = self
            .topological_order
            .iter()
            .rev()
            .filter(|&&index| removed[index])
            .map(|&index| self.objects[index].handle)
            .collect::<Vec<_>>();
        let mut new_index = vec![usize::MAX; removed.len()];
        let mut kept = 0;
        {
            let mut slots = self.slots.borrow_mut();
            for (index, object) in self.objects.iter().enumerate() {
                if removed[index] {
                    slots.vacate(object.handle.index);
                } else {
                    slots.set_live(object.handle, kept);
                    new_index[index] = kept;
                    kept += 1;
                }
            }
        }
        for (object, &gone) in self.objects.iter().zip(&removed) {
            if !gone {
                continue;
            }
            self.indices_by_id.remove(&object.id);
            self.invalid_basis_reported.remove(&object.handle);
            if object.directional_light.is_some() {
                self.last_valid_light = LightState::default();
            }
            if object.environment.is_some() {
                self.last_valid_environment = None;
            }
        }
        retain_unmarked(&mut self.objects, &removed);
        retain_unmarked(&mut self.world_matrices, &removed);
        retain_unmarked(&mut self.world_rotations, &removed);
        retain_unmarked(&mut self.last_valid_local_lights, &removed);
        let order = self
            .topological_order
            .iter()
            .filter(|&&index| !removed[index])
            .map(|&index| new_index[index])
            .collect();
        self.topological_order = order;
        Ok(removed_handles)
    }
}

/// Id объектов шаблона после spawn в порядке шаблона: корень — `root_id`, дети — `<корень>.<id>`.
fn spawned_ids(root_id: &str, template: &PrefabTemplate) -> Vec<String> {
    template
        .objects
        .iter()
        .map(|object| {
            if object.id.is_empty() {
                root_id.to_owned()
            } else {
                format!("{root_id}.{}", object.id)
            }
        })
        .collect()
}

/// Убирает элементы с отмеченными индексами, сохраняя порядок остальных.
fn retain_unmarked<T>(values: &mut Vec<T>, removed: &[bool]) {
    let mut index = 0;
    values.retain(|_| {
        let keep = !removed[index];
        index += 1;
        keep
    });
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
        post_process: object
            .camera
            .as_ref()
            .map(|_| PostProcessSettings::default()),
        directional_light: object.directional_light,
        point_light: object.point_light,
        spot_light: object.spot_light,
        environment: object.environment.clone(),
        collider: object.collider.clone(),
        rigid_body: object.rigid_body,
        character_controller: object.character_controller,
        audio_source: object.audio_source.clone(),
        animator: object.animator.clone(),
        particle_emitter: object.particle_emitter.clone(),
        network_identity: object.network_identity,
        network_transform: object.network_transform,
        ui_canvas: object.ui_canvas.clone(),
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

pub(crate) fn topological_order(
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
mod spawn_tests;

#[cfg(test)]
mod tests {
    use super::{LocalTransform, ObjectHandle, World, document_handle};
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
        assert_eq!(world.objects()[0].handle, document_handle(0));
        assert_eq!(world.objects()[1].handle, document_handle(1));
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
        let main_post = crate::post_process::resolve_post_process(&main_settings, None);
        world.set_post_process(main, main_post).unwrap();
        world.set_camera_settings(main, main_settings).unwrap();
        assert_eq!(world.build_frame().0.camera.position.x, 1.0);
        assert_eq!(world.build_frame().0.post_process.exposure.ev, 2.0);
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
    fn post_process_copies_are_per_camera_and_only_cameras_accept_them() {
        let document = SceneDocument::parse(
            "post.rvescene",
            r#"
[[objects]]
id="main"
[objects.camera]
active=true
[[objects]]
id="second"
[objects.camera]
[[objects]]
id="mesh"
"#,
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let main = world.handle("main").unwrap();
        let second = world.handle("second").unwrap();
        let mesh = world.handle("mesh").unwrap();
        assert_eq!(
            world.post_process(main),
            Some(&crate::PostProcessSettings::default())
        );
        assert_eq!(world.post_process(mesh), None);

        let mut graded = crate::PostProcessSettings::default();
        graded.bloom.enabled = true;
        graded.bloom.intensity = 2.0;
        world.set_post_process(main, graded).unwrap();
        assert_eq!(world.post_process(main), Some(&graded));
        assert_eq!(
            world.post_process(second),
            Some(&crate::PostProcessSettings::default()),
            "правка одной камеры не меняет копию другой"
        );
        assert!(world.build_frame().0.post_process.bloom.enabled);

        world.set_active_camera(second).unwrap();
        assert!(!world.build_frame().0.post_process.bloom.enabled);

        let mut invalid = crate::PostProcessSettings::default();
        invalid.bloom.intensity = -1.0;
        assert!(world.set_post_process(second, invalid).is_err());
        assert_eq!(
            world.post_process(second),
            Some(&crate::PostProcessSettings::default())
        );
        assert!(world.set_post_process(mesh, graded).is_err());
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
        assert!(
            world
                .set_active_camera(ObjectHandle {
                    index: 99,
                    generation: 0
                })
                .is_err()
        );
        assert!(
            world
                .set_camera_settings(
                    ObjectHandle {
                        index: 99,
                        generation: 0
                    },
                    defaults
                )
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

#[cfg(test)]
mod particle_tests {
    use super::{ObjectHandle, World};
    use crate::SceneDocument;
    use crate::particles::ParticleEmitterComponent;

    const SCENE: &str = r#"
[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "plain"
[objects.transform]
position = [1.0, 0.0, 0.0]

[[objects]]
id = "fx"
[objects.transform]
position = [0.0, 2.0, 0.0]
[objects.particle_emitter]
capacity = 64
rate = 5.0
"#;

    fn handle_of(world: &World, id: &str) -> ObjectHandle {
        world
            .objects()
            .iter()
            .find(|object| object.id == id)
            .map(|object| object.handle)
            .expect("object exists")
    }

    #[test]
    fn world_exposes_emitters_and_frame_lists_only_their_objects() {
        let document = SceneDocument::parse("particles.rvescene", SCENE).unwrap();
        let mut world = World::from_document(&document).unwrap();
        let fx = handle_of(&world, "fx");
        let plain = handle_of(&world, "plain");
        let emitter: &ParticleEmitterComponent = world.particle_emitter(fx).expect("emitter");
        assert_eq!(emitter.capacity, 64);
        assert!(world.particle_emitter(plain).is_none());

        let (frame, _) = world.build_frame();
        assert_eq!(frame.particle_emitters.len(), 1);
        let instance = frame.particle_emitters[0];
        assert_eq!(instance.object, fx);
        let translation = instance.model.w_axis.truncate();
        assert!((translation - glam::Vec3::new(0.0, 2.0, 0.0)).length() < 1e-5);
    }
}
