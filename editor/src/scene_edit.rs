//! Редактируемая сцена: команды над документом, undo/redo и сохранение.

use engine_files::VirtualPath;
use engine_scene::document::MaterialOverride;
use engine_scene::{
    AnimatorComponent, AudioSourceComponent, CameraComponent, CameraCursor,
    CharacterControllerComponent, ColliderComponent, DirectionalLightComponent,
    EnvironmentComponent, Expansion, LodGroup, MeshRenderer, ObjectOrigin,
    ParticleEmitterComponent, PointLightComponent, PrefabInstance, PrefabOverride,
    ProceduralSkyComponent, RigidBodyComponent, SceneAsset, SceneDocument, SceneError, SceneObject,
    SceneTransform, SpotLightComponent,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

mod prefabs;

pub use prefabs::OverrideField;

/// Загрузчик префабов проекта по virtual path; документ разобран как `.rveprefab`.
pub type PrefabLoader = Rc<dyn Fn(&VirtualPath) -> Result<SceneDocument, SceneError>>;

/// Загрузчик для сцены вне проекта: префабов нет.
pub fn no_prefabs() -> PrefabLoader {
    Rc::new(|path| {
        Err(SceneError {
            source: format!("/{}", path.as_str()),
            object_id: None,
            message: "префабы доступны только в открытом проекте".to_owned(),
        })
    })
}
use std::fs;
use std::path::Path;

/// Глубина истории; снимки документа малы, поэтому простота важнее экономии памяти.
const MAX_UNDO_DEPTH: usize = 128;
const MAX_ID_LENGTH: usize = 64;

const NEW_SCENE: &str = r#"
[[objects]]
id = "camera"
name = "Camera"
[objects.transform]
position = [0.0, 1.0, 6.0]
[objects.camera]

[[objects]]
id = "sun"
name = "Directional Light"
[objects.transform]
rotation = [50.0, 30.0, 0.0]
[objects.directional_light]
shadows = true
shadow_distance = 20.0
shadow_cascades = 4
"#;

/// Вид объекта, создаваемого из меню или перетаскиванием ресурса.
#[derive(Clone, Debug, PartialEq)]
pub enum NewObject {
    Empty,
    Camera,
    DirectionalLight,
    PointLight,
    SpotLight,
    Environment,
    /// Модель и локальная позиция нового объекта.
    Mesh(VirtualPath, [f32; 3]),
    /// Экземпляр префаба и локальная позиция его корня.
    Prefab(VirtualPath, [f32; 3]),
}

/// Вид компонента для меню Reset/Copy/Paste в Inspector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentKind {
    Transform,
    Camera,
    MeshRenderer,
    LodGroup,
    DirectionalLight,
    PointLight,
    SpotLight,
    Environment,
    Collider,
    RigidBody,
    CharacterController,
    AudioSource,
    Animator,
    ParticleEmitter,
}

/// Значение компонента объекта: копируется в буфер Inspector и вставляется в объект.
#[derive(Clone, Debug)]
pub enum Component {
    Transform(SceneTransform),
    Camera(CameraComponent),
    MeshRenderer(MeshRenderer),
    LodGroup(LodGroup),
    DirectionalLight(DirectionalLightComponent),
    PointLight(PointLightComponent),
    SpotLight(SpotLightComponent),
    Environment(EnvironmentComponent),
    Collider(ColliderComponent),
    RigidBody(RigidBodyComponent),
    CharacterController(CharacterControllerComponent),
    AudioSource(AudioSourceComponent),
    Animator(AnimatorComponent),
    ParticleEmitter(ParticleEmitterComponent),
}

impl Component {
    pub fn kind(&self) -> ComponentKind {
        match self {
            Self::Transform(_) => ComponentKind::Transform,
            Self::Camera(_) => ComponentKind::Camera,
            Self::MeshRenderer(_) => ComponentKind::MeshRenderer,
            Self::LodGroup(_) => ComponentKind::LodGroup,
            Self::DirectionalLight(_) => ComponentKind::DirectionalLight,
            Self::PointLight(_) => ComponentKind::PointLight,
            Self::SpotLight(_) => ComponentKind::SpotLight,
            Self::Environment(_) => ComponentKind::Environment,
            Self::Collider(_) => ComponentKind::Collider,
            Self::RigidBody(_) => ComponentKind::RigidBody,
            Self::CharacterController(_) => ComponentKind::CharacterController,
            Self::AudioSource(_) => ComponentKind::AudioSource,
            Self::Animator(_) => ComponentKind::Animator,
            Self::ParticleEmitter(_) => ComponentKind::ParticleEmitter,
        }
    }

    /// Текущее значение компонента объекта; `None`, если компонента нет.
    pub fn read(object: &SceneObject, kind: ComponentKind) -> Option<Self> {
        Some(match kind {
            ComponentKind::Transform => Self::Transform(object.transform),
            ComponentKind::Camera => Self::Camera(object.camera.clone()?),
            ComponentKind::MeshRenderer => Self::MeshRenderer(object.mesh_renderer.clone()?),
            ComponentKind::LodGroup => Self::LodGroup(object.lod_group.clone()?),
            ComponentKind::DirectionalLight => Self::DirectionalLight(object.directional_light?),
            ComponentKind::PointLight => Self::PointLight(object.point_light?),
            ComponentKind::SpotLight => Self::SpotLight(object.spot_light?),
            ComponentKind::Environment => Self::Environment(object.environment.clone()?),
            ComponentKind::Collider => Self::Collider(object.collider.clone()?),
            ComponentKind::RigidBody => Self::RigidBody(object.rigid_body?),
            ComponentKind::CharacterController => {
                Self::CharacterController(object.character_controller?)
            }
            ComponentKind::AudioSource => Self::AudioSource(object.audio_source.clone()?),
            ComponentKind::Animator => Self::Animator(object.animator.clone()?),
            ComponentKind::ParticleEmitter => {
                Self::ParticleEmitter(object.particle_emitter.clone()?)
            }
        })
    }

    /// Значение по умолчанию для Reset. Mesh Renderer сохраняет модель, а LOD Group
    /// сброса не имеет: её уровни — это выбор ресурсов, а не настройки.
    pub fn reset(object: &SceneObject, kind: ComponentKind) -> Option<Self> {
        Some(match kind {
            ComponentKind::Transform => Self::Transform(SceneTransform::default()),
            ComponentKind::Camera => Self::Camera(default_camera()),
            ComponentKind::MeshRenderer => Self::MeshRenderer(MeshRenderer {
                model: object.mesh_renderer.as_ref()?.model.clone(),
                texture: None,
                material: None,
            }),
            ComponentKind::LodGroup => return None,
            ComponentKind::DirectionalLight => Self::DirectionalLight(default_light()),
            ComponentKind::PointLight => Self::PointLight(PointLightComponent::default()),
            ComponentKind::SpotLight => Self::SpotLight(SpotLightComponent::default()),
            ComponentKind::Environment => Self::Environment(default_environment()),
            ComponentKind::Collider => Self::Collider(ColliderComponent::default()),
            ComponentKind::RigidBody => Self::RigidBody(RigidBodyComponent::default()),
            ComponentKind::CharacterController => {
                Self::CharacterController(CharacterControllerComponent::default())
            }
            // Как и модель Mesh Renderer, клип сохраняется: без него источник не задан.
            ComponentKind::AudioSource => Self::AudioSource(AudioSourceComponent::with_clip(
                object.audio_source.as_ref()?.clip.clone(),
            )),
            // Клипы сохраняются, как у источника звука: без них аниматор пустой.
            ComponentKind::Animator => {
                let clips = object.animator.as_ref()?.clips.clone();
                Self::Animator(AnimatorComponent {
                    clips,
                    ..AnimatorComponent::new()
                })
            }
            // Текстура сохраняется по той же причине, что и клип звука.
            ComponentKind::ParticleEmitter => Self::ParticleEmitter(ParticleEmitterComponent {
                texture: object.particle_emitter.as_ref()?.texture.clone(),
                ..ParticleEmitterComponent::default()
            }),
        })
    }

    fn apply(self, object: &mut SceneObject) {
        match self {
            Self::Transform(value) => object.transform = value,
            Self::Camera(value) => {
                // Активность — состояние сцены, а не свойство компонента: вставка её не меняет.
                let active = object.camera.as_ref().is_some_and(|camera| camera.active);
                object.camera = Some(CameraComponent { active, ..value });
            }
            Self::MeshRenderer(value) => object.mesh_renderer = Some(value),
            Self::LodGroup(value) => object.lod_group = Some(value),
            Self::DirectionalLight(value) => object.directional_light = Some(value),
            Self::PointLight(value) => object.point_light = Some(value),
            Self::SpotLight(value) => object.spot_light = Some(value),
            Self::Environment(value) => object.environment = Some(value),
            Self::Collider(value) => object.collider = Some(value),
            Self::RigidBody(value) => object.rigid_body = Some(value),
            Self::CharacterController(value) => object.character_controller = Some(value),
            Self::AudioSource(value) => object.audio_source = Some(value),
            Self::Animator(value) => object.animator = Some(value),
            Self::ParticleEmitter(value) => object.particle_emitter = Some(value),
        }
    }
}

/// Открытая сцена с выделением и историей правок.
pub struct EditorScene {
    /// Авторский документ: его правят команды и он сохраняется в файл.
    document: SceneDocument,
    /// Раскрытый документ для показа, мира и Play; пересчитывается после каждой правки.
    expansion: Expansion,
    loader: PrefabLoader,
    /// Virtual path файла сцены; `None` — сцена ещё не сохранялась.
    path: Option<VirtualPath>,
    dirty: bool,
    /// Выделенные объекты в порядке выбора; последний — активный, его показывает Inspector.
    selection: Vec<String>,
    undo: Vec<SceneDocument>,
    redo: Vec<SceneDocument>,
    /// Ключ незавершённой непрерывной правки, например перетаскивания значения.
    coalesce: Option<String>,
    /// Растёт при каждом изменении документа; потребители по нему обновляют производные данные.
    revision: u64,
}

impl EditorScene {
    pub fn new_untitled() -> Result<Self, String> {
        let document =
            SceneDocument::parse("untitled.rvescene", NEW_SCENE).map_err(|e| e.to_string())?;
        Self::from_document(document, None, no_prefabs())
    }

    /// Разбирает сцену или, для пути `.rveprefab`, префаб.
    pub fn parse(path: VirtualPath, contents: &str, loader: PrefabLoader) -> Result<Self, String> {
        let document = parse_document(&path, contents).map_err(|error| error.to_string())?;
        Self::from_document(document, Some(path), loader)
    }

    /// Сцена из автосохранения: путь прежний, а изменения считаются несохранёнными.
    pub fn recover(path: Option<VirtualPath>, contents: &str) -> Result<Self, String> {
        Self::recover_with_loader(path, contents, no_prefabs())
    }

    /// Восстанавливает автосохранение проекта с тем же загрузчиком префабов.
    pub fn recover_with_loader(
        path: Option<VirtualPath>,
        contents: &str,
        loader: PrefabLoader,
    ) -> Result<Self, String> {
        let label = path
            .as_ref()
            .map_or("untitled.rvescene", |path| path.as_str());
        let document = match path.as_ref() {
            Some(path) => parse_document(path, contents),
            None => SceneDocument::parse(label, contents),
        }
        .map_err(|error| error.to_string())?;
        let mut scene = Self::from_document(document, path, loader)?;
        scene.dirty = true;
        Ok(scene)
    }

    fn from_document(
        document: SceneDocument,
        path: Option<VirtualPath>,
        loader: PrefabLoader,
    ) -> Result<Self, String> {
        let expansion = expand_document(&document, path.as_ref(), &loader)?;
        Ok(Self {
            document,
            expansion,
            loader,
            path,
            dirty: false,
            selection: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            coalesce: None,
            revision: 0,
        })
    }

    pub fn document(&self) -> &SceneDocument {
        &self.document
    }

    /// Раскрытый документ: объекты экземпляров с id через точку. Его показывают панели.
    pub fn view(&self) -> &SceneDocument {
        &self.expansion.document
    }

    /// Происхождение объекта экземпляра; `None` — собственный объект документа.
    pub fn origin(&self, id: &str) -> Option<&ObjectOrigin> {
        self.expansion.origins.get(id)
    }

    /// Файлы префабов, раскрытые экземплярами этого документа, включая вложенные.
    pub fn instanced_prefabs(&self) -> &[VirtualPath] {
        &self.expansion.prefabs
    }

    /// Авторский экземпляр префаба по id.
    pub fn instance(&self, id: &str) -> Option<&PrefabInstance> {
        self.document
            .instances
            .iter()
            .find(|instance| instance.id == id)
    }

    /// Объект, входящий в экземпляр, но не его корень: его нельзя удалить или перенести.
    pub fn is_prefab_part(&self, id: &str) -> bool {
        self.origin(id).is_some() && self.instance(id).is_none()
    }

    /// Подменяет загрузчик префабов и раскрывает документ заново.
    pub fn set_prefab_loader(&mut self, loader: PrefabLoader) -> Result<(), String> {
        self.loader = loader;
        self.reload_prefabs()
    }

    /// Раскрывает документ заново, например после изменения файлов префабов.
    pub fn reload_prefabs(&mut self) -> Result<(), String> {
        self.expansion = expand_document(&self.document, self.path.as_ref(), &self.loader)?;
        self.revision += 1;
        let visible = self
            .expansion
            .document
            .objects
            .iter()
            .map(|object| object.id.as_str())
            .collect::<HashSet<_>>();
        self.selection.retain(|id| visible.contains(id.as_str()));
        Ok(())
    }

    pub fn path(&self) -> Option<&VirtualPath> {
        self.path.as_ref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Активный объект выделения.
    pub fn selection(&self) -> Option<&str> {
        self.selection.last().map(String::as_str)
    }

    /// Все выделенные объекты, активный — последним.
    pub fn selected_ids(&self) -> &[String] {
        &self.selection
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.selection.iter().any(|selected| selected == id)
    }

    pub fn selected_object(&self) -> Option<&SceneObject> {
        self.object(self.selection()?)
    }

    /// Объект раскрытого документа, в том числе объект экземпляра.
    pub fn object(&self, id: &str) -> Option<&SceneObject> {
        self.view().objects.iter().find(|object| object.id == id)
    }

    /// Заменяет выделение одним объектом или очищает его.
    pub fn select(&mut self, id: Option<&str>) {
        self.selection = id
            .filter(|id| self.object(id).is_some())
            .map(str::to_owned)
            .into_iter()
            .collect();
    }

    /// Добавляет объект к выделению и делает его активным.
    pub fn add_to_selection(&mut self, id: &str) {
        if self.object(id).is_some() {
            self.selection.retain(|selected| selected != id);
            self.selection.push(id.to_owned());
        }
    }

    /// Снимает выделение с объекта или добавляет его, если он не выделен.
    pub fn toggle_selection(&mut self, id: &str) {
        if self.is_selected(id) {
            self.selection.retain(|selected| selected != id);
        } else {
            self.add_to_selection(id);
        }
    }

    /// Выделенные объекты без тех, чей предок тоже выделен, в порядке документа:
    /// групповые команды применяются к поддеревьям один раз.
    pub fn selection_roots(&self) -> Vec<String> {
        let parents = self
            .document
            .objects
            .iter()
            .map(|object| (object.id.as_str(), object.parent.as_deref()))
            .collect::<HashMap<_, _>>();
        let has_selected_ancestor = |id: &str| {
            let mut current = parents.get(id).copied().flatten();
            while let Some(parent) = current {
                if self.is_selected(parent) {
                    return true;
                }
                current = parents.get(parent).copied().flatten();
            }
            false
        };
        self.document
            .objects
            .iter()
            .filter(|object| self.is_selected(&object.id) && !has_selected_ancestor(&object.id))
            .map(|object| object.id.clone())
            .collect()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(document) = self.undo.pop() else {
            return false;
        };
        self.redo
            .push(std::mem::replace(&mut self.document, document));
        self.after_history_change();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(document) = self.redo.pop() else {
            return false;
        };
        self.undo
            .push(std::mem::replace(&mut self.document, document));
        self.after_history_change();
        true
    }

    fn after_history_change(&mut self) {
        self.coalesce = None;
        self.dirty = true;
        self.revision += 1;
        // Документ из истории уже раскрывался; ошибка возможна, только если файлы префабов
        // изменились, и тогда остаётся прежнее раскрытие.
        if let Ok(expansion) = expand_document(&self.document, self.path.as_ref(), &self.loader) {
            self.expansion = expansion;
        }
        let visible = self
            .expansion
            .document
            .objects
            .iter()
            .map(|object| object.id.as_str())
            .collect::<HashSet<_>>();
        self.selection.retain(|id| visible.contains(id.as_str()));
    }

    /// Завершает непрерывную правку: следующее изменение создаст новую запись истории.
    pub fn end_continuous_edit(&mut self) {
        self.coalesce = None;
    }

    /// Завершает непрерывную правку только с ключом `key`. Нужна виджетам вроде палитры цвета,
    /// которые не сообщают об окончании перетаскивания.
    pub fn end_continuous_edit_for(&mut self, key: &str) {
        if self.coalesce.as_deref() == Some(key) {
            self.coalesce = None;
        }
    }

    /// Применяет правку к копии и принимает её только после проверки всего документа.
    ///
    /// Правки с одинаковым `coalesce` подряд занимают одну запись истории.
    fn edit(
        &mut self,
        coalesce: Option<&str>,
        change: impl FnOnce(&mut SceneDocument) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut next = self.document.clone();
        change(&mut next)?;
        next.validate().map_err(|error| error.to_string())?;
        let expansion = expand_document(&next, self.path.as_ref(), &self.loader)?;
        self.expansion = expansion;
        let continues = coalesce.is_some() && coalesce == self.coalesce.as_deref();
        if !continues {
            self.undo.push(std::mem::replace(&mut self.document, next));
            if self.undo.len() > MAX_UNDO_DEPTH {
                self.undo.remove(0);
            }
            self.redo.clear();
        } else {
            self.document = next;
        }
        self.coalesce = coalesce.map(str::to_owned);
        self.dirty = true;
        self.revision += 1;
        Ok(())
    }

    /// Изменяет один объект; подходит для всех правок полей в Inspector.
    ///
    /// У корня экземпляра `name`, `transform` и `parent` пишутся в сам экземпляр.
    pub fn edit_object(
        &mut self,
        id: &str,
        coalesce: Option<&str>,
        change: impl FnOnce(&mut SceneObject) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.document.objects.iter().any(|object| object.id == id) {
            return self.edit(coalesce, |document| change(find_object_mut(document, id)?));
        }
        if self.origin(id).is_some() {
            return self.edit_prefab_object(id, coalesce, change);
        }
        Err(format!("объект {id:?} не найден"))
    }

    pub fn create_object(
        &mut self,
        kind: &NewObject,
        parent: Option<&str>,
    ) -> Result<String, String> {
        if let NewObject::Prefab(path, position) = kind {
            return self.instantiate(path, parent, *position);
        }
        let (base, name) = match kind {
            NewObject::Empty => ("object".to_owned(), "Empty".to_owned()),
            NewObject::Camera => ("camera".to_owned(), "Camera".to_owned()),
            NewObject::DirectionalLight => ("light".to_owned(), "Directional Light".to_owned()),
            NewObject::PointLight => ("point_light".to_owned(), "Point Light".to_owned()),
            NewObject::SpotLight => ("spot_light".to_owned(), "Spot Light".to_owned()),
            NewObject::Environment => ("environment".to_owned(), "Environment".to_owned()),
            NewObject::Mesh(path, _) | NewObject::Prefab(path, _) => {
                let stem = file_stem(path);
                (stem.to_owned(), stem.to_owned())
            }
        };
        let id = unique_id(&self.document, &base, &HashSet::new());
        let mut object = SceneObject {
            id: id.clone(),
            name: Some(name),
            parent: parent.map(str::to_owned),
            ..SceneObject::default()
        };
        self.edit(None, |document| {
            match kind {
                NewObject::Empty | NewObject::Prefab(..) => {}
                NewObject::Camera => {
                    if !document
                        .objects
                        .iter()
                        .any(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                        && let Some(existing_camera) = document
                            .objects
                            .iter_mut()
                            .find(|object| object.camera.is_some())
                    {
                        existing_camera.camera.as_mut().unwrap().active = true;
                    }
                    object.camera = Some(CameraComponent::default());
                }
                NewObject::DirectionalLight => {
                    object.transform.rotation = [50.0, 30.0, 0.0];
                    object.directional_light = Some(default_light());
                }
                NewObject::PointLight => object.point_light = Some(PointLightComponent::default()),
                NewObject::SpotLight => object.spot_light = Some(SpotLightComponent::default()),
                NewObject::Environment => object.environment = Some(default_environment()),
                NewObject::Mesh(path, position) => {
                    object.transform.position = *position;
                    ensure_asset(document, path);
                    object.mesh_renderer = Some(MeshRenderer {
                        model: path.clone(),
                        texture: None,
                        material: None,
                    });
                }
            }
            document.objects.push(object);
            Ok(())
        })?;
        self.selection = vec![id.clone()];
        Ok(id)
    }

    /// Удаляет объект или экземпляр вместе с потомками; единственную камеру удалить нельзя.
    /// Часть экземпляра удалить нельзя: удаляется весь экземпляр или делается Unpack.
    pub fn delete_object(&mut self, id: &str) -> Result<(), String> {
        self.delete_objects(&[id.to_owned()])
    }

    /// Удаляет объекты с потомками одной записью истории.
    pub fn delete_objects(&mut self, ids: &[String]) -> Result<(), String> {
        let mut removed = HashSet::new();
        for id in ids {
            self.require_own(id)?;
            removed.extend(subtree_ids(self.view(), id)?);
        }
        if removed.is_empty() {
            return Ok(());
        }
        self.edit(None, |document| {
            document
                .objects
                .retain(|object| !removed.contains(&object.id));
            document
                .instances
                .retain(|instance| !removed.contains(&instance.id));
            if document
                .objects
                .iter()
                .any(|object| object.camera.is_some())
                && !document
                    .objects
                    .iter()
                    .any(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                && let Some(object) = document
                    .objects
                    .iter_mut()
                    .find(|object| object.camera.is_some())
            {
                object.camera.as_mut().unwrap().active = true;
            }
            Ok(())
        })?;
        self.selection.retain(|id| !removed.contains(id));
        Ok(())
    }

    /// Удаляет выделенные объекты; потомок выделенного предка удаляется вместе с ним.
    pub fn delete_selection(&mut self) -> Result<(), String> {
        let roots = self.selection_roots();
        if roots.is_empty() {
            return Ok(());
        }
        self.delete_objects(&roots)
    }

    /// Копирует объект или экземпляр с потомками и выделяет копии.
    pub fn duplicate_object(&mut self, id: &str) -> Result<String, String> {
        self.duplicate_roots(&[id.to_owned()])?
            .into_iter()
            .next()
            .ok_or_else(|| format!("объект {id:?} не найден"))
    }

    /// Копирует выделенные поддеревья одной записью истории и выделяет копии.
    pub fn duplicate_selection(&mut self) -> Result<Vec<String>, String> {
        let roots = self.selection_roots();
        self.duplicate_roots(&roots)
    }

    fn duplicate_roots(&mut self, roots: &[String]) -> Result<Vec<String>, String> {
        let mut subtree = HashSet::new();
        for root in roots {
            self.require_own(root)?;
            subtree.extend(subtree_ids(self.view(), root)?);
        }
        if subtree.is_empty() {
            return Ok(Vec::new());
        }
        let source_ids = self
            .document
            .objects
            .iter()
            .map(|object| &object.id)
            .chain(self.document.instances.iter().map(|instance| &instance.id))
            .filter(|id| subtree.contains(*id))
            .cloned()
            .collect::<Vec<_>>();
        let mut reserved = HashSet::new();
        let mut renamed = HashMap::new();
        for source in &source_ids {
            let copy_id = unique_id(&self.document, source, &reserved);
            reserved.insert(copy_id.clone());
            renamed.insert(source.clone(), copy_id);
        }
        let copies = roots
            .iter()
            .filter_map(|root| renamed.get(root).cloned())
            .collect::<Vec<_>>();
        let prefab_cameras = self
            .view()
            .objects
            .iter()
            .filter_map(|object| {
                let origin = self.origin(&object.id)?;
                if !renamed.contains_key(&origin.instance) {
                    return None;
                }
                Some((
                    origin.instance.clone(),
                    origin.target.clone(),
                    object.camera.clone()?,
                ))
            })
            .collect::<Vec<_>>();
        let remap =
            |parent: Option<String>| parent.map(|parent| rename_reference(&renamed, parent));
        self.edit(None, |document| {
            let objects = document
                .objects
                .iter()
                .filter(|object| renamed.contains_key(&object.id))
                .cloned()
                .map(|mut object| {
                    object.parent = remap(object.parent);
                    object.id = renamed[&object.id].clone();
                    if let Some(camera) = &mut object.camera {
                        camera.active = false;
                    }
                    object
                })
                .collect::<Vec<_>>();
            let instances = document
                .instances
                .iter()
                .filter(|instance| renamed.contains_key(&instance.id))
                .cloned()
                .map(|mut instance| {
                    instance.parent = remap(instance.parent);
                    instance.id = renamed[&instance.id].clone();
                    instance
                })
                .collect::<Vec<_>>();
            document.objects.extend(objects);
            document.instances.extend(instances);
            for (source_instance, target, mut camera) in prefab_cameras {
                camera.active = false;
                if let Some(settings) = &camera.settings {
                    ensure_asset(document, settings);
                }
                let instance_id = &renamed[&source_instance];
                let instance = document
                    .instances
                    .iter_mut()
                    .find(|instance| &instance.id == instance_id)
                    .ok_or_else(|| format!("экземпляр {instance_id:?} не найден"))?;
                if let Some(change) = instance
                    .overrides
                    .iter_mut()
                    .find(|change| change.target == target)
                {
                    change.camera = Some(camera);
                } else {
                    instance.overrides.push(PrefabOverride {
                        target,
                        camera: Some(camera),
                        ..PrefabOverride::default()
                    });
                }
            }
            if document
                .objects
                .iter()
                .filter(|object| object.camera.is_some())
                .count()
                > 1
                && !document
                    .objects
                    .iter()
                    .any(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                && let Some(object) = document
                    .objects
                    .iter_mut()
                    .find(|object| object.camera.is_some())
            {
                object.camera.as_mut().unwrap().active = true;
            }
            Ok(())
        })?;
        self.selection = copies.clone();
        Ok(copies)
    }

    /// Объекты обычных поддеревьев в порядке документа для системного буфера обмена.
    pub fn subtrees(&self, roots: &[String]) -> Result<Vec<SceneObject>, String> {
        let mut ids = HashSet::new();
        for root in roots {
            self.require_own(root)?;
            ids.extend(subtree_ids(&self.document, root)?);
        }
        Ok(self
            .document
            .objects
            .iter()
            .filter(|object| ids.contains(&object.id))
            .cloned()
            .collect())
    }

    /// Вставляет объекты из системного буфера одной записью истории.
    pub fn insert_copies(&mut self, objects: Vec<SceneObject>) -> Result<Vec<String>, String> {
        let mut reserved = HashSet::new();
        let mut renamed = HashMap::new();
        for object in &objects {
            let copy_id = unique_id(&self.document, &object.id, &reserved);
            reserved.insert(copy_id.clone());
            renamed.insert(object.id.clone(), copy_id);
        }
        let roots = objects
            .iter()
            .filter(|object| {
                object
                    .parent
                    .as_ref()
                    .is_none_or(|parent| !renamed.contains_key(parent))
            })
            .map(|object| renamed[&object.id].clone())
            .collect::<Vec<_>>();
        self.edit(None, |document| {
            if document
                .objects
                .iter()
                .any(|object| object.camera.is_some())
                && !document
                    .objects
                    .iter()
                    .any(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                && let Some(object) = document
                    .objects
                    .iter_mut()
                    .find(|object| object.camera.is_some())
            {
                object.camera.as_mut().unwrap().active = true;
            }
            for mut object in objects {
                object.parent = match object.parent {
                    Some(parent) if renamed.contains_key(&parent) => Some(renamed[&parent].clone()),
                    Some(parent) if document.objects.iter().any(|known| known.id == parent) => {
                        Some(parent)
                    }
                    _ => None,
                };
                object.id = renamed[&object.id].clone();
                if let Some(camera) = &mut object.camera {
                    camera.active = false;
                }
                register_object_assets(document, &object)?;
                document.objects.push(object);
            }
            Ok(())
        })?;
        self.selection = roots.clone();
        Ok(roots)
    }

    /// Меняет трансформации нескольких объектов одной правкой: при ошибке не меняется ни одна.
    pub fn set_transforms(
        &mut self,
        transforms: &[(String, SceneTransform)],
        coalesce: Option<&str>,
    ) -> Result<(), String> {
        self.edit(coalesce, |document| {
            for (id, transform) in transforms {
                find_object_mut(document, id)?.transform = *transform;
            }
            Ok(())
        })
    }

    /// Добавляет камеру, сохраняя единственную активную камеру документа.
    pub fn add_camera(&mut self, id: &str) -> Result<(), String> {
        self.edit(None, |document| {
            if find_object(document, id)?.camera.is_some() {
                return Err(format!("объект {id:?} уже имеет камеру"));
            }
            if !document
                .objects
                .iter()
                .any(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                && let Some(existing) = document
                    .objects
                    .iter_mut()
                    .find(|object| object.camera.is_some())
            {
                existing.camera.as_mut().unwrap().active = true;
            }
            find_object_mut(document, id)?.camera = Some(CameraComponent::default());
            Ok(())
        })
    }

    /// Удаляет камеру, назначая замену, если была удалена активная камера.
    pub fn remove_camera(&mut self, id: &str) -> Result<(), String> {
        self.edit(None, |document| {
            let active = find_object(document, id)?
                .camera
                .as_ref()
                .ok_or_else(|| format!("объект {id:?} не имеет камеры"))?
                .active;
            find_object_mut(document, id)?.camera = None;
            if active
                && let Some(replacement) = document
                    .objects
                    .iter_mut()
                    .find(|object| object.camera.is_some())
            {
                replacement.camera.as_mut().unwrap().active = true;
            }
            Ok(())
        })
    }

    /// Назначает активную камеру; снятие единственного active оставляет одну камеру implicit.
    pub fn set_camera_active(&mut self, id: &str, active: bool) -> Result<(), String> {
        let mut cameras = self
            .view()
            .objects
            .iter()
            .filter_map(|object| {
                object
                    .camera
                    .clone()
                    .map(|camera| (object.id.clone(), camera))
            })
            .collect::<Vec<_>>();
        let Some(selected_index) = cameras.iter().position(|(camera_id, _)| camera_id == id) else {
            return Err(format!("объект {id:?} не имеет камеры"));
        };
        if active {
            for (camera_id, camera) in &mut cameras {
                camera.active = camera_id == id;
            }
        } else {
            cameras[selected_index].1.active = false;
            if cameras.len() > 1
                && !cameras.iter().any(|(_, camera)| camera.active)
                && let Some((_, (_, replacement))) = cameras
                    .iter_mut()
                    .enumerate()
                    .find(|(index, _)| *index != selected_index)
            {
                replacement.active = true;
            }
        }
        let updates = cameras
            .into_iter()
            .map(|(camera_id, camera)| {
                let origin = self.origin(&camera_id).cloned();
                (camera_id, origin, camera)
            })
            .collect::<Vec<_>>();
        self.edit(None, |document| {
            for (camera_id, origin, camera) in updates {
                if let Some(origin) = origin {
                    if let Some(settings) = &camera.settings {
                        ensure_asset(document, settings);
                    }
                    let instance = document
                        .instances
                        .iter_mut()
                        .find(|instance| instance.id == origin.instance)
                        .ok_or_else(|| format!("экземпляр {:?} не найден", origin.instance))?;
                    if let Some(change) = instance
                        .overrides
                        .iter_mut()
                        .find(|change| change.target == origin.target)
                    {
                        change.camera = Some(camera);
                    } else {
                        instance.overrides.push(PrefabOverride {
                            target: origin.target,
                            camera: Some(camera),
                            ..PrefabOverride::default()
                        });
                    }
                } else {
                    find_object_mut(document, &camera_id)?.camera = Some(camera);
                }
            }
            Ok(())
        })
    }

    /// Меняет идентификатор объекта или экземпляра и все ссылки на него, в том числе на
    /// объекты внутри экземпляра (`door1.handle`).
    pub fn change_id(&mut self, id: &str, new_id: &str) -> Result<(), String> {
        if id == new_id {
            return Ok(());
        }
        self.require_own(id)?;
        let renamed = std::collections::HashMap::from([(id.to_owned(), new_id.to_owned())]);
        self.edit(None, |document| {
            if let Some(object) = document.objects.iter_mut().find(|object| object.id == id) {
                object.id = new_id.to_owned();
            } else if let Some(instance) = document
                .instances
                .iter_mut()
                .find(|instance| instance.id == id)
            {
                instance.id = new_id.to_owned();
            }
            let parents = document
                .objects
                .iter_mut()
                .map(|object| &mut object.parent)
                .chain(
                    document
                        .instances
                        .iter_mut()
                        .map(|instance| &mut instance.parent),
                );
            for parent in parents {
                *parent = parent
                    .take()
                    .map(|parent| rename_reference(&renamed, parent));
            }
            Ok(())
        })?;
        for selected in &mut self.selection {
            *selected = rename_reference(&renamed, std::mem::take(selected));
        }
        Ok(())
    }

    /// Меняет родителя с сохранением локальной трансформации. Объектам внутри экземпляра
    /// родителя менять нельзя.
    pub fn set_parent(&mut self, id: &str, parent: Option<&str>) -> Result<(), String> {
        self.require_own(id)?;
        if let Some(parent) = parent
            && subtree_ids(self.view(), id)?.contains(parent)
        {
            return Err(format!(
                "объект {id:?} нельзя вложить в себя или своего потомка {parent:?}"
            ));
        }
        self.edit(None, |document| {
            let parent = parent.map(str::to_owned);
            if let Some(object) = document.objects.iter_mut().find(|object| object.id == id) {
                object.parent = parent;
            } else if let Some(instance) = document
                .instances
                .iter_mut()
                .find(|instance| instance.id == id)
            {
                instance.parent = parent;
            }
            Ok(())
        })
    }

    /// Объект — собственный объект документа или корень экземпляра.
    fn require_own(&self, id: &str) -> Result<(), String> {
        if self.is_prefab_part(id) {
            return Err(format!(
                "объект {id:?} — часть префаба: правьте префаб (Open Prefab) или сделайте Unpack"
            ));
        }
        Ok(())
    }

    pub fn set_transform(
        &mut self,
        id: &str,
        transform: SceneTransform,
        coalesce: Option<&str>,
    ) -> Result<(), String> {
        self.edit_object(id, coalesce, |object| {
            object.transform = transform;
            Ok(())
        })
    }
    /// Назначает модель и текстуру объекта; их пути добавляются в список ресурсов сцены.
    pub fn set_mesh(
        &mut self,
        id: &str,
        model: VirtualPath,
        texture: Option<VirtualPath>,
    ) -> Result<(), String> {
        if self.origin(id).is_some() {
            // Модель объекта экземпляра меняется override; ресурсы добавит сама правка.
            return self.edit_object(id, None, |object| {
                let mesh = object
                    .mesh_renderer
                    .as_mut()
                    .ok_or("Mesh Renderer объекта префаба добавляется только в самом префабе")?;
                mesh.model = model;
                mesh.texture = texture;
                Ok(())
            });
        }
        self.edit(None, |document| {
            ensure_asset(document, &model);
            if let Some(texture) = &texture {
                ensure_asset(document, texture);
            }
            let object = find_object_mut(document, id)?;
            let material = object
                .mesh_renderer
                .as_ref()
                .and_then(|mesh| mesh.material.clone());
            object.mesh_renderer = Some(MeshRenderer {
                model,
                texture,
                material,
            });
            Ok(())
        })
    }

    /// Назначает коллайдеру `.rvephysmat` и добавляет его в ресурсы сцены; `None` возвращает
    /// инлайн-поля трения и отскока.
    pub fn set_collider_material(
        &mut self,
        id: &str,
        material: Option<VirtualPath>,
    ) -> Result<(), String> {
        if let Some(path) = &material
            && !path.as_str().to_ascii_lowercase().ends_with(".rvephysmat")
        {
            return Err(format!("{}: ожидается ресурс .rvephysmat", path));
        }
        if self.origin(id).is_some() {
            return self.edit_object(id, None, |object| {
                let collider = object
                    .collider
                    .as_mut()
                    .ok_or_else(|| format!("объект {id:?} не имеет коллайдера"))?;
                collider.material = material;
                Ok(())
            });
        }
        self.edit(None, |document| {
            if let Some(path) = &material {
                ensure_asset(document, path);
            }
            let collider = find_object_mut(document, id)?
                .collider
                .as_mut()
                .ok_or_else(|| format!("объект {id:?} не имеет коллайдера"))?;
            collider.material = material;
            Ok(())
        })
    }

    /// Назначает настройки камеры и добавляет их путь в список ресурсов сцены.
    pub fn set_camera_settings(
        &mut self,
        id: &str,
        settings: Option<VirtualPath>,
    ) -> Result<(), String> {
        if let Some(path) = &settings
            && !path
                .as_str()
                .to_ascii_lowercase()
                .ends_with(".camerasettings")
        {
            return Err(format!("{}: ожидается ресурс .camerasettings", path));
        }
        if self.origin(id).is_some() {
            return self.edit_object(id, None, |object| {
                let camera = object
                    .camera
                    .as_mut()
                    .ok_or_else(|| format!("объект {id:?} не имеет камеры"))?;
                camera.settings = settings;
                Ok(())
            });
        }
        self.edit(None, |document| {
            if let Some(path) = &settings {
                ensure_asset(document, path);
            }
            let camera = find_object_mut(document, id)?
                .camera
                .as_mut()
                .ok_or_else(|| format!("объект {id:?} не имеет камеры"))?;
            camera.settings = settings;
            Ok(())
        })
    }

    /// Задаёт флаги курсора камеры для старта сцены; действуют, только если камера активна.
    pub fn set_camera_cursor(&mut self, id: &str, cursor: CameraCursor) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            let camera = object
                .camera
                .as_mut()
                .ok_or_else(|| format!("объект {id:?} не имеет камеры"))?;
            camera.cursor = cursor;
            Ok(())
        })
    }

    /// Добавляет `.rveprefab` в ресурсы сцены: только такие префабы спавнятся быстро.
    pub fn add_scene_prefab(&mut self, path: VirtualPath) -> Result<(), String> {
        require_prefab_path(&path)?;
        self.edit(None, |document| {
            if document.assets.iter().any(|asset| asset.path == path) {
                return Err(format!("/{} уже есть в ресурсах сцены", path.as_str()));
            }
            document.assets.push(SceneAsset {
                path,
                archive: None,
                max_instances: None,
            });
            Ok(())
        })
    }

    /// Удаляет `.rveprefab` из ресурсов сцены. Префаб, на который есть экземпляр, не удаляется.
    pub fn remove_scene_prefab(&mut self, path: &VirtualPath) -> Result<(), String> {
        require_prefab_path(path)?;
        if self
            .document
            .instances
            .iter()
            .any(|instance| &instance.prefab == path)
        {
            return Err(format!(
                "/{} используется экземпляром: сначала удалите экземпляр",
                path.as_str()
            ));
        }
        self.edit(None, |document| {
            let index = scene_asset_index(document, path)?;
            document.assets.remove(index);
            Ok(())
        })
    }

    /// Задаёт архив ресурса `.rveprefab`; имя проверяет [`SceneDocument::validate`].
    pub fn set_scene_prefab_archive(
        &mut self,
        path: &VirtualPath,
        archive: Option<String>,
    ) -> Result<(), String> {
        self.edit_scene_prefab(path, |asset| asset.archive = archive)
    }

    /// Задаёт лимит экземпляров ресурса `.rveprefab`; ноль отклоняет валидация документа.
    pub fn set_scene_prefab_max_instances(
        &mut self,
        path: &VirtualPath,
        max_instances: Option<u32>,
    ) -> Result<(), String> {
        self.edit_scene_prefab(path, |asset| asset.max_instances = max_instances)
    }

    fn edit_scene_prefab(
        &mut self,
        path: &VirtualPath,
        change: impl FnOnce(&mut SceneAsset),
    ) -> Result<(), String> {
        require_prefab_path(path)?;
        self.edit(None, |document| {
            let index = scene_asset_index(document, path)?;
            change(&mut document.assets[index]);
            Ok(())
        })
    }

    /// Заменяет scene material override; текстуры добавляются в список ресурсов сцены.
    /// Override без заданных полей удаляется.
    pub fn set_material(
        &mut self,
        id: &str,
        material: Option<MaterialOverride>,
        coalesce: Option<&str>,
    ) -> Result<(), String> {
        let material = material.filter(|material| !is_empty_material(material));
        self.edit(coalesce, |document| {
            if let Some(material) = &material {
                for texture in [
                    &material.metallic_roughness_texture,
                    &material.normal_texture,
                    &material.emissive_texture,
                    &material.occlusion_texture,
                ]
                .into_iter()
                .flatten()
                {
                    ensure_asset(document, texture);
                }
            }
            find_object_mut(document, id)?
                .mesh_renderer
                .as_mut()
                .ok_or_else(|| format!("объект {id:?}: нет Mesh Renderer"))?
                .material = material;
            Ok(())
        })
    }

    /// Назначает уровни LOD и добавляет их ресурсы в список сцены.
    pub fn set_lod_group(&mut self, id: &str, group: Option<LodGroup>) -> Result<(), String> {
        self.edit(None, |document| {
            if let Some(group) = &group {
                for level in &group.levels {
                    let model = parse_lod_path(id, &level.model, "lod_group.levels.model")?;
                    ensure_asset(document, &model);
                    if let Some(texture) = &level.texture {
                        let texture = parse_lod_path(id, texture, "lod_group.levels.texture")?;
                        ensure_asset(document, &texture);
                    }
                }
            }
            find_object_mut(document, id)?.lod_group = group;
            Ok(())
        })
    }

    /// Задаёт значение компонента целиком; его ресурсы добавляются в список сцены.
    pub fn set_component(&mut self, id: &str, component: Component) -> Result<(), String> {
        if self.origin(id).is_some() {
            return self.edit_object(id, None, |object| {
                component.apply(object);
                Ok(())
            });
        }
        self.edit(None, |document| {
            let object = find_object_mut(document, id)?;
            component.apply(object);
            let object = object.clone();
            register_object_assets(document, &object)
        })
    }

    /// Удаляет звуковой источник объекта.
    pub fn remove_audio_source(&mut self, id: &str) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            object.audio_source = None;
            Ok(())
        })
    }

    /// Удаляет аниматор объекта; клипы в `[[assets]]` остаются объявленными.
    pub fn remove_animator(&mut self, id: &str) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            object.animator = None;
            Ok(())
        })
    }
    /// Удаляет эмиттер частиц объекта.
    pub fn remove_particle_emitter(&mut self, id: &str) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            object.particle_emitter = None;
            Ok(())
        })
    }

    /// Удаляет mesh renderer и его обязательную LOD Group в одной правке.
    pub fn remove_mesh_renderer(&mut self, id: &str) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            object.mesh_renderer = None;
            object.lod_group = None;
            Ok(())
        })
    }

    /// Удаляет физический компонент одной правкой. Rigid Body без коллайдера документ не
    /// принимает, поэтому коллайдер удаляется вместе с телом.
    pub fn remove_physics_component(
        &mut self,
        id: &str,
        kind: ComponentKind,
    ) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            match kind {
                ComponentKind::Collider => {
                    object.collider = None;
                    object.rigid_body = None;
                }
                ComponentKind::RigidBody => object.rigid_body = None,
                ComponentKind::CharacterController => object.character_controller = None,
                other => return Err(format!("{other:?} не является физическим компонентом")),
            }
            Ok(())
        })
    }

    /// Добавляет Lua-скрипт объекту и его путь в список ресурсов сцены.
    pub fn add_lite_script(&mut self, id: &str, path: VirtualPath) -> Result<(), String> {
        self.edit(None, |document| {
            ensure_asset(document, &path);
            find_object_mut(document, id)?
                .lite_scripts
                .push(engine_scene::LiteScript {
                    path,
                    params: Default::default(),
                });
            Ok(())
        })
    }

    /// Сохраняет документ атомарно и обновляет путь сцены.
    ///
    /// Результат сначала разбирается заново, поэтому файл на диске всегда остаётся читаемым.
    pub fn save(&mut self, file: &Path, path: VirtualPath) -> Result<(), String> {
        if is_prefab_path(&path) != self.document.is_prefab() {
            return Err(format!(
                "/{}: сцена сохраняется в .rvescene, префаб — в .rveprefab",
                path.as_str()
            ));
        }
        let contents = self.document.to_toml().map_err(|error| error.to_string())?;
        let reparsed = parse_document(&path, &contents)
            .map_err(|error| format!("сериализованная сцена не прошла проверку: {error}"))?;
        // Префаб, включающий сам себя под новым путём, отклоняется до записи.
        expand_document(&reparsed, Some(&path), &self.loader)?;
        write_atomic(file, contents.as_bytes())?;
        self.document = reparsed;
        self.path = Some(path);
        self.dirty = false;
        Ok(())
    }
}

fn is_prefab_path(path: &VirtualPath) -> bool {
    std::path::Path::new(path.as_str())
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rveprefab"))
}

/// Разбирает файл по расширению: `.rveprefab` — префаб, иначе сцена.
fn parse_document(path: &VirtualPath, contents: &str) -> Result<SceneDocument, SceneError> {
    if is_prefab_path(path) {
        SceneDocument::parse_prefab(path.as_str(), contents)
    } else {
        SceneDocument::parse(path.as_str(), contents)
    }
}

/// Раскрывает документ. Если это префаб с путём `own`, ссылка на этот путь получает сам
/// документ, а не файл с диска: так цикл через несохранённую правку обнаруживается сразу.
fn expand_document(
    document: &SceneDocument,
    own: Option<&VirtualPath>,
    loader: &PrefabLoader,
) -> Result<Expansion, String> {
    expand_document_with(document, own, loader, None)
}

/// Как [`expand_document`]; `skip_overrides` — экземпляр, чьи overrides не применяются.
fn expand_document_with(
    document: &SceneDocument,
    own: Option<&VirtualPath>,
    loader: &PrefabLoader,
    skip_overrides: Option<&str>,
) -> Result<Expansion, String> {
    let own = own.filter(|_| document.is_prefab());
    let mut load = |path: &VirtualPath| match own {
        Some(own) if own == path => Ok(document.clone()),
        _ => loader(path),
    };
    match skip_overrides {
        Some(instance) => engine_scene::expand_without_overrides(document, instance, &mut load),
        None => engine_scene::expand(document, &mut load),
    }
    .map_err(|error| error.to_string())
}

fn file_stem(path: &VirtualPath) -> &str {
    path.as_str()
        .rsplit('/')
        .next()
        .and_then(|file| file.split('.').next())
        .unwrap_or("object")
}

/// Ссылка на переименованный объект: сам id или объект внутри переименованного
/// экземпляра (`old.child` → `new.child`).
fn rename_reference(renamed: &std::collections::HashMap<String, String>, id: String) -> String {
    if let Some(new) = renamed.get(&id) {
        return new.clone();
    }
    if let Some((head, rest)) = id.split_once('.')
        && let Some(new) = renamed.get(head)
    {
        return format!("{new}.{rest}");
    }
    id
}

/// Светлая настройка по умолчанию для нового directional light.
pub fn default_light() -> DirectionalLightComponent {
    DirectionalLightComponent {
        color: [1.0; 3],
        ambient: [0.18, 0.18, 0.2],
        shadows: true,
        shadow_distance: 20.0,
        shadow_cascades: 4,
    }
}

/// Ключ JSON, по которому редактор узнаёт свои объекты в буфере обмена ОС.
const CLIPBOARD_KEY: &str = "rve_editor_objects";

/// Текст буфера обмена с объектами, например из `EditorScene::subtrees`.
pub fn objects_to_clipboard(objects: &[SceneObject]) -> Result<String, String> {
    serde_json::to_string_pretty(&serde_json::json!({ CLIPBOARD_KEY: objects }))
        .map_err(|error| error.to_string())
}

/// Объекты из буфера обмена; `None` — в буфере не объекты редактора.
pub fn objects_from_clipboard(text: &str) -> Option<Result<Vec<SceneObject>, String>> {
    let mut value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let objects = value.get_mut(CLIPBOARD_KEY)?.take();
    Some(
        serde_json::from_value(objects)
            .map_err(|error| format!("объекты в буфере обмена повреждены: {error}")),
    )
}

/// Камера нового объекта или компонента.
pub fn default_camera() -> CameraComponent {
    CameraComponent::default()
}

/// Процедурное окружение по умолчанию для нового компонента.
pub fn default_environment() -> EnvironmentComponent {
    EnvironmentComponent {
        hdr: None,
        procedural: Some(ProceduralSkyComponent {
            zenith_color: [0.18, 0.32, 0.58],
            horizon_color: [0.68, 0.74, 0.82],
            ground_color: [0.12, 0.11, 0.10],
            sun_size_deg: 1.0,
            sun_intensity: 1.0,
        }),
        intensity: 1.0,
        skybox: true,
    }
}

/// Добавляет в список ресурсов сцены все пути, на которые ссылаются компоненты объекта.
fn register_object_assets(
    document: &mut SceneDocument,
    object: &SceneObject,
) -> Result<(), String> {
    if let Some(mesh) = &object.mesh_renderer {
        ensure_asset(document, &mesh.model);
        let material = mesh.material.as_ref();
        for path in [
            mesh.texture.as_ref(),
            material.and_then(|material| material.metallic_roughness_texture.as_ref()),
            material.and_then(|material| material.normal_texture.as_ref()),
            material.and_then(|material| material.emissive_texture.as_ref()),
            material.and_then(|material| material.occlusion_texture.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            ensure_asset(document, path);
        }
    }
    if let Some(group) = &object.lod_group {
        for level in &group.levels {
            let model = parse_lod_path(&object.id, &level.model, "lod_group.levels.model")?;
            ensure_asset(document, &model);
            if let Some(texture) = &level.texture {
                let texture = parse_lod_path(&object.id, texture, "lod_group.levels.texture")?;
                ensure_asset(document, &texture);
            }
        }
    }
    if let Some(audio) = &object.audio_source {
        ensure_asset(document, &audio.clip);
    }
    if let Some(animator) = &object.animator {
        for clip in &animator.clips {
            ensure_asset(document, clip);
        }
    }
    if let Some(texture) = object
        .particle_emitter
        .as_ref()
        .and_then(|emitter| emitter.texture.as_ref())
    {
        ensure_asset(document, texture);
    }
    for script in &object.lite_scripts {
        ensure_asset(document, &script.path);
    }
    if let Some(settings) = object
        .camera
        .as_ref()
        .and_then(|camera| camera.settings.as_ref())
    {
        ensure_asset(document, settings);
    }
    if let Some(hdr) = object
        .environment
        .as_ref()
        .and_then(|environment| environment.hdr.as_ref())
    {
        ensure_asset(document, hdr);
    }
    Ok(())
}

fn is_empty_material(material: &MaterialOverride) -> bool {
    material.base_color.is_none()
        && material.metallic.is_none()
        && material.roughness.is_none()
        && material.emissive.is_none()
        && material.opacity.is_none()
        && material.alpha_mode.is_none()
        && material.alpha_cutoff.is_none()
        && material.metallic_roughness_texture.is_none()
        && material.normal_texture.is_none()
        && material.emissive_texture.is_none()
        && material.occlusion_texture.is_none()
        && material.normal_scale.is_none()
        && material.occlusion_strength.is_none()
}

pub fn write_atomic(file: &Path, contents: &[u8]) -> Result<(), String> {
    let describe = |error: std::io::Error| format!("{}: {error}", file.display());
    let name = file
        .file_name()
        .ok_or_else(|| format!("{}: нет имени файла", file.display()))?;
    let mut temporary_name = name.to_owned();
    temporary_name.push(".tmp");
    let temporary = file.with_file_name(temporary_name);
    fs::write(&temporary, contents).map_err(describe)?;
    fs::rename(&temporary, file).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        describe(error)
    })
}

fn find_object_mut<'a>(
    document: &'a mut SceneDocument,
    id: &str,
) -> Result<&'a mut SceneObject, String> {
    document
        .objects
        .iter_mut()
        .find(|object| object.id == id)
        .ok_or_else(|| format!("объект {id:?} не найден"))
}

fn find_object<'a>(document: &'a SceneDocument, id: &str) -> Result<&'a SceneObject, String> {
    document
        .objects
        .iter()
        .find(|object| object.id == id)
        .ok_or_else(|| format!("объект {id:?} не найден"))
}

fn ensure_asset(document: &mut SceneDocument, path: &VirtualPath) {
    if !document.assets.iter().any(|asset| &asset.path == path) {
        document.assets.push(SceneAsset {
            path: path.clone(),
            archive: None,
            max_instances: None,
        });
    }
}

fn require_prefab_path(path: &VirtualPath) -> Result<(), String> {
    if is_prefab_path(path) {
        Ok(())
    } else {
        Err(format!("/{}: ожидается ресурс .rveprefab", path.as_str()))
    }
}

fn scene_asset_index(document: &SceneDocument, path: &VirtualPath) -> Result<usize, String> {
    document
        .assets
        .iter()
        .position(|asset| &asset.path == path)
        .ok_or_else(|| format!("/{} нет в ресурсах сцены", path.as_str()))
}

fn parse_lod_path(id: &str, value: &str, field: &str) -> Result<VirtualPath, String> {
    VirtualPath::parse(value).map_err(|_| {
        let path = value.strip_prefix('/').unwrap_or(value);
        format!("объект {id:?}: {field} /{path}: некорректный virtual path")
    })
}

/// Идентификаторы объекта и всех его потомков.
fn subtree_ids(document: &SceneDocument, id: &str) -> Result<HashSet<String>, String> {
    if !document.objects.iter().any(|object| object.id == id) {
        return Err(format!("объект {id:?} не найден"));
    }
    let mut ids = HashSet::from([id.to_owned()]);
    loop {
        let before = ids.len();
        for object in &document.objects {
            if object.parent.as_ref().is_some_and(|p| ids.contains(p)) {
                ids.insert(object.id.clone());
            }
        }
        if ids.len() == before {
            return Ok(ids);
        }
    }
}

/// Подбирает свободный идентификатор вида `[A-Za-z0-9_-]{1,64}` на основе `base`.
fn unique_id(document: &SceneDocument, base: &str, reserved: &HashSet<String>) -> String {
    let mut sanitized = base
        .chars()
        .map(|symbol| {
            if symbol.is_ascii_alphanumeric() || symbol == '_' || symbol == '-' {
                symbol
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized.push_str("object");
    }
    let taken = |candidate: &str| {
        reserved.contains(candidate)
            || document.objects.iter().any(|object| object.id == candidate)
            || document
                .instances
                .iter()
                .any(|instance| instance.id == candidate)
    };
    sanitized.truncate(MAX_ID_LENGTH);
    if !taken(&sanitized) {
        return sanitized;
    }
    for number in 1_u32.. {
        let suffix = format!("-{number}");
        let stem = &sanitized[..sanitized.len().min(MAX_ID_LENGTH - suffix.len())];
        let candidate = format!("{stem}{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 суффиксов больше, чем объектов в сцене")
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_scene::LodLevel;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn scene() -> EditorScene {
        EditorScene::new_untitled().unwrap()
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    fn position(scene: &EditorScene, id: &str) -> [f32; 3] {
        scene.object(id).unwrap().transform.position
    }

    fn moved(x: f32) -> SceneTransform {
        SceneTransform {
            position: [x, 0.0, 0.0],
            ..SceneTransform::default()
        }
    }

    #[test]
    fn new_scene_is_valid_and_clean() {
        let scene = scene();
        assert!(!scene.is_dirty());
        assert_eq!(scene.document().objects.len(), 2);
        assert!(!scene.can_undo());
    }

    #[test]
    fn created_objects_get_unique_ids_and_become_selected() {
        let mut scene = scene();
        let first = scene.create_object(&NewObject::Empty, None).unwrap();
        let second = scene.create_object(&NewObject::Empty, None).unwrap();
        assert_ne!(first, second);
        assert_eq!(scene.selection(), Some(second.as_str()));
        assert!(scene.is_dirty());
    }

    #[test]
    fn environment_object_is_valid_and_scene_allows_only_one() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Environment, None).unwrap();
        let environment = scene.object(&id).unwrap().environment.as_ref().unwrap();
        assert!(environment.hdr.is_none());
        assert!(environment.procedural.is_some());
        assert!(environment.skybox);
        assert!(scene.document().validate().is_ok());
        assert!(scene.create_object(&NewObject::Environment, None).is_err());
        assert_eq!(
            scene
                .view()
                .objects
                .iter()
                .filter(|object| object.environment.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn mesh_object_registers_its_model_as_an_asset_once() {
        let mut scene = scene();
        let model = path("/models/My Cube.v2.fbx");
        let first = scene
            .create_object(&NewObject::Mesh(model.clone(), [1.0, 0.0, 2.0]), None)
            .unwrap();
        assert_eq!(
            scene.object(&first).unwrap().transform.position,
            [1.0, 0.0, 2.0]
        );
        scene
            .create_object(&NewObject::Mesh(model, [0.0; 3]), None)
            .unwrap();
        assert_eq!(first, "My-Cube");
        assert_eq!(scene.document().assets.len(), 1);
    }

    #[test]
    fn invalid_edit_is_rolled_back_with_a_message() {
        let mut scene = scene();
        let revision = scene.revision();
        let error = scene
            .create_object(&NewObject::DirectionalLight, None)
            .unwrap_err();
        assert!(error.contains("directional_light"));
        assert_eq!(scene.document().objects.len(), 2);
        assert_eq!(scene.revision(), revision);
        assert!(!scene.can_undo());
        assert!(!scene.is_dirty());
    }

    #[test]
    fn the_only_camera_cannot_be_deleted() {
        let mut scene = scene();
        assert!(scene.delete_object("camera").is_err());
        assert!(scene.object("camera").is_some());
    }

    #[test]
    fn second_directional_light_is_rejected() {
        let mut scene = scene();
        assert!(
            scene
                .create_object(&NewObject::DirectionalLight, None)
                .is_err()
        );
    }

    #[test]
    fn delete_removes_descendants_and_clears_selection() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        scene.select(Some(&child));
        scene.delete_object(&parent).unwrap();
        assert!(scene.object(&parent).is_none());
        assert!(scene.object(&child).is_none());
        assert_eq!(scene.selection(), None);
    }

    #[test]
    fn duplicate_copies_subtree_with_remapped_parents() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        let copy = scene.duplicate_object(&parent).unwrap();
        let copied_child = scene
            .document()
            .objects
            .iter()
            .find(|object| object.parent.as_deref() == Some(copy.as_str()))
            .unwrap();
        assert_ne!(copied_child.id, child);
        assert_eq!(scene.object(&child).unwrap().parent.as_ref(), Some(&parent));
    }

    #[test]
    fn cameras_can_be_added_and_duplicated_but_the_light_cannot() {
        let mut scene = scene();
        let created = scene.create_object(&NewObject::Camera, None).unwrap();
        let copy = scene.duplicate_object("camera").unwrap();
        for id in [created, copy] {
            assert!(scene.object(&id).unwrap().camera.is_some());
        }
        assert_eq!(
            scene
                .document()
                .objects
                .iter()
                .filter(|object| object.camera.as_ref().is_some_and(|camera| camera.active))
                .count(),
            1
        );
        scene.document().validate().unwrap();
        assert!(scene.duplicate_object("sun").is_err());
    }

    #[test]
    fn removing_the_active_camera_selects_a_remaining_camera() {
        let mut scene = scene();
        let second = scene.create_object(&NewObject::Camera, None).unwrap();
        scene.remove_camera("camera").unwrap();
        assert!(
            scene
                .object(&second)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        scene.document().validate().unwrap();
        assert!(scene.remove_camera(&second).is_err());
    }

    #[test]
    fn camera_activation_switches_atomically_and_one_camera_can_be_implicit() {
        let mut editor_scene = scene();
        let second = editor_scene
            .create_object(&NewObject::Camera, None)
            .unwrap();
        editor_scene.set_camera_active(&second, true).unwrap();
        assert!(
            !editor_scene
                .object("camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        assert!(
            editor_scene
                .object(&second)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        editor_scene.set_camera_active(&second, false).unwrap();
        assert!(
            editor_scene
                .object("camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        assert!(
            !editor_scene
                .object(&second)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        editor_scene.document().validate().unwrap();

        let mut single = scene();
        single.set_camera_active("camera", false).unwrap();
        assert!(
            !single
                .object("camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        single.document().validate().unwrap();
    }

    #[test]
    fn id_change_updates_children_selection_and_rejects_bad_values() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        scene.select(Some(&parent));
        scene.change_id(&parent, "renamed").unwrap();
        assert_eq!(
            scene.object(&child).unwrap().parent.as_deref(),
            Some("renamed")
        );
        assert_eq!(scene.selection(), Some("renamed"));
        assert!(scene.change_id("renamed", "bad id!").is_err());
        assert!(scene.change_id("renamed", "camera").is_err());
        assert!(scene.object("renamed").is_some());
    }

    #[test]
    fn reparenting_into_own_subtree_is_rejected() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        assert!(scene.set_parent(&parent, Some(&child)).is_err());
        assert!(scene.set_parent(&parent, Some(&parent)).is_err());
        assert_eq!(scene.object(&parent).unwrap().parent, None);
        scene.set_parent(&child, None).unwrap();
        assert_eq!(scene.object(&child).unwrap().parent, None);
    }

    #[test]
    fn component_edit_with_invalid_values_is_rolled_back() {
        let mut scene = scene();
        let second = scene.create_object(&NewObject::Camera, None).unwrap();
        let result = scene.edit_object("camera", None, |object| {
            object.camera.as_mut().unwrap().active = false;
            Ok(())
        });
        assert!(result.is_err());
        assert!(
            scene
                .object("camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        assert!(
            !scene
                .object(&second)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
    }

    #[test]
    fn assigning_assets_registers_paths_and_rejects_wrong_extensions() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        scene
            .set_mesh(&id, path("/m.fbx"), Some(path("/t.png")))
            .unwrap();
        scene.add_lite_script(&id, path("/s.lua")).unwrap();
        assert_eq!(scene.document().assets.len(), 3);
        assert!(scene.set_mesh(&id, path("/t.png"), None).is_err());
        assert!(scene.add_lite_script(&id, path("/m.fbx")).is_err());
    }

    #[test]
    fn camera_cursor_flags_are_written_to_the_document_and_undoable() {
        let mut scene = scene();
        let cursor = CameraCursor {
            hide_on_start: true,
            lock_on_start: true,
        };
        scene.set_camera_cursor("camera", cursor).unwrap();
        let stored = |scene: &EditorScene| {
            scene
                .document()
                .objects
                .iter()
                .find(|object| object.id == "camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .cursor
        };
        assert_eq!(stored(&scene), cursor);
        assert!(scene.undo());
        assert_eq!(stored(&scene), CameraCursor::default());
        assert!(scene.redo());
        assert_eq!(stored(&scene), cursor);
        assert!(scene.set_camera_cursor("missing", cursor).is_err());
    }

    #[test]
    fn assigning_camera_settings_registers_a_valid_asset_path() {
        let mut scene = scene();
        let settings = path("/cameras/main.camerasettings");
        scene
            .set_camera_settings("camera", Some(settings.clone()))
            .unwrap();
        assert_eq!(
            scene
                .object("camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .settings,
            Some(settings.clone())
        );
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path == settings)
        );
        assert!(
            scene
                .set_camera_settings("camera", Some(path("/cameras/main.toml")))
                .is_err()
        );
    }

    #[test]
    fn assigning_collider_material_registers_asset_and_inline_clears_it() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        scene
            .set_component(&id, Component::Collider(ColliderComponent::default()))
            .unwrap();
        let material = path("/materials/ice.rvephysmat");
        scene
            .set_collider_material(&id, Some(material.clone()))
            .unwrap();
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .collider
                .as_ref()
                .unwrap()
                .material,
            Some(material.clone())
        );
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path == material)
        );
        assert!(
            scene
                .set_collider_material(&id, Some(path("/materials/ice.toml")))
                .is_err()
        );
        scene.set_collider_material(&id, None).unwrap();
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .collider
                .as_ref()
                .unwrap()
                .material,
            None
        );
    }

    #[test]
    fn lod_group_registers_assets_and_supports_undo_redo() {
        let mut scene = scene();
        let id = scene
            .create_object(&NewObject::Mesh(path("/models/base.fbx"), [0.0; 3]), None)
            .unwrap();
        let group = LodGroup {
            levels: vec![LodLevel {
                model: "/models/lod.fbx".to_owned(),
                texture: Some("/textures/lod.png".to_owned()),
                distance: 10.0,
            }],
            cull_distance: Some(20.0),
        };
        scene.set_lod_group(&id, Some(group)).unwrap();

        let assets = scene
            .document()
            .assets
            .iter()
            .map(|asset| asset.path.as_str())
            .collect::<Vec<_>>();
        assert!(assets.contains(&"models/lod.fbx"));
        assert!(assets.contains(&"textures/lod.png"));
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .lod_group
                .as_ref()
                .unwrap()
                .levels
                .len(),
            1
        );
        assert!(scene.undo());
        assert!(scene.object(&id).unwrap().lod_group.is_none());
        assert!(scene.redo());
        assert!(scene.object(&id).unwrap().lod_group.is_some());
    }

    #[test]
    fn invalid_lod_level_count_and_order_roll_back_with_validation_messages() {
        let mut scene = scene();
        let id = scene
            .create_object(&NewObject::Mesh(path("/base.fbx"), [0.0; 3]), None)
            .unwrap();
        let before = scene.document().to_toml().unwrap();
        let seven = LodGroup {
            levels: (0..7)
                .map(|index| LodLevel {
                    model: format!("/lod{index}.fbx"),
                    texture: None,
                    distance: (index + 1) as f32 * 10.0,
                })
                .collect(),
            cull_distance: None,
        };
        let error = scene.set_lod_group(&id, Some(seven)).unwrap_err();
        assert!(error.contains("от 1 до 6 уровней"), "{error}");
        assert_eq!(scene.document().to_toml().unwrap(), before);

        let unordered = LodGroup {
            levels: vec![
                LodLevel {
                    model: "/lod1.fbx".to_owned(),
                    texture: None,
                    distance: 20.0,
                },
                LodLevel {
                    model: "/lod2.fbx".to_owned(),
                    texture: None,
                    distance: 10.0,
                },
            ],
            cull_distance: None,
        };
        let error = scene.set_lod_group(&id, Some(unordered)).unwrap_err();
        assert!(
            error.contains("distance должен быть конечным, положительным и строго возрастать"),
            "{error}"
        );
        assert_eq!(scene.document().to_toml().unwrap(), before);
    }

    #[test]
    fn removing_mesh_renderer_also_removes_lod_group_and_history_restores_both() {
        let mut scene = scene();
        let id = scene
            .create_object(&NewObject::Mesh(path("/base.fbx"), [0.0; 3]), None)
            .unwrap();
        scene
            .set_lod_group(
                &id,
                Some(LodGroup {
                    levels: vec![LodLevel {
                        model: "/lod.fbx".to_owned(),
                        texture: None,
                        distance: 10.0,
                    }],
                    cull_distance: None,
                }),
            )
            .unwrap();
        scene.remove_mesh_renderer(&id).unwrap();
        let object = scene.object(&id).unwrap();
        assert!(object.mesh_renderer.is_none());
        assert!(object.lod_group.is_none());
        assert!(scene.undo());
        let object = scene.object(&id).unwrap();
        assert!(object.mesh_renderer.is_some());
        assert!(object.lod_group.is_some());
        assert!(scene.redo());
        let object = scene.object(&id).unwrap();
        assert!(object.mesh_renderer.is_none());
        assert!(object.lod_group.is_none());
    }

    #[test]
    fn assigning_a_new_mesh_preserves_its_material_override() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        scene.set_mesh(&id, path("/first.fbx"), None).unwrap();
        scene
            .edit_object(&id, None, |object| {
                object.mesh_renderer.as_mut().unwrap().material =
                    Some(engine_scene::document::MaterialOverride {
                        base_color: Some([0.3, 0.4, 0.5]),
                        metallic: Some(1.0),
                        roughness: Some(0.25),
                        metallic_roughness_texture: None,
                        normal_texture: None,
                        normal_scale: Some(0.5),
                        double_sided: None,
                        ..MaterialOverride::default()
                    });
                Ok(())
            })
            .unwrap();

        scene.set_mesh(&id, path("/second.fbx"), None).unwrap();

        let material = scene
            .object(&id)
            .unwrap()
            .mesh_renderer
            .as_ref()
            .unwrap()
            .material
            .as_ref()
            .unwrap();
        assert_eq!(material.base_color, Some([0.3, 0.4, 0.5]));
        assert_eq!(material.metallic, Some(1.0));
        assert_eq!(material.roughness, Some(0.25));
        assert_eq!(material.normal_scale, Some(0.5));
    }

    fn material(roughness: Option<f32>) -> MaterialOverride {
        MaterialOverride {
            base_color: None,
            metallic: None,
            roughness,
            metallic_roughness_texture: None,
            normal_texture: None,
            normal_scale: None,
            double_sided: None,
            ..MaterialOverride::default()
        }
    }

    fn mesh_material(scene: &EditorScene, id: &str) -> Option<MaterialOverride> {
        scene.object(id)?.mesh_renderer.as_ref()?.material.clone()
    }

    #[test]
    fn material_override_registers_textures_and_empty_override_is_removed() {
        let mut scene = scene();
        let id = scene
            .create_object(&NewObject::Mesh(path("/m.fbx"), [0.0; 3]), None)
            .unwrap();
        let mut textured = material(Some(0.3));
        textured.normal_texture = Some(path("/textures/n.png"));
        textured.emissive = Some([1.0, 0.25, 0.0]);
        textured.emissive_texture = Some(path("/textures/emissive.png"));
        textured.occlusion_texture = Some(path("/textures/occlusion.png"));
        textured.occlusion_strength = Some(0.4);
        scene.set_material(&id, Some(textured), None).unwrap();
        let assets = scene
            .document()
            .assets
            .iter()
            .map(|asset| asset.path.as_str())
            .collect::<Vec<_>>();
        assert!(assets.contains(&"textures/n.png"));
        assert!(assets.contains(&"textures/emissive.png"));
        assert!(assets.contains(&"textures/occlusion.png"));
        assert_eq!(mesh_material(&scene, &id).unwrap().roughness, Some(0.3));
        assert_eq!(
            mesh_material(&scene, &id).unwrap().emissive,
            Some([1.0, 0.25, 0.0])
        );
        assert_eq!(
            mesh_material(&scene, &id).unwrap().occlusion_strength,
            Some(0.4)
        );
        scene.set_material(&id, Some(material(None)), None).unwrap();
        assert!(mesh_material(&scene, &id).is_none());
        assert!(scene.undo());
        assert!(mesh_material(&scene, &id).is_some());
    }

    #[test]
    fn invalid_material_values_and_objects_without_mesh_are_rejected() {
        let mut scene = scene();
        let id = scene
            .create_object(&NewObject::Mesh(path("/m.fbx"), [0.0; 3]), None)
            .unwrap();
        let error = scene
            .set_material(&id, Some(material(Some(1.5))), None)
            .unwrap_err();
        assert!(error.contains("material.roughness"), "{error}");
        assert!(mesh_material(&scene, &id).is_none());
        let mut wrong = material(None);
        wrong.normal_texture = Some(path("/n.jpg"));
        assert!(scene.set_material(&id, Some(wrong), None).is_err());
        assert!(
            scene
                .set_material("camera", Some(material(Some(0.5))), None)
                .is_err()
        );
    }

    #[test]
    fn pasted_components_register_assets_and_reset_keeps_the_mesh_model() {
        let mut scene = scene();
        let source = scene
            .create_object(&NewObject::Mesh(path("/a.fbx"), [0.0; 3]), None)
            .unwrap();
        scene
            .set_mesh(&source, path("/a.fbx"), Some(path("/t.png")))
            .unwrap();
        let copied =
            Component::read(scene.object(&source).unwrap(), ComponentKind::MeshRenderer).unwrap();
        // Вставка в другую сцену, где этих ресурсов ещё нет.
        let mut target = EditorScene::new_untitled().unwrap();
        let empty = target.create_object(&NewObject::Empty, None).unwrap();
        target.set_component(&empty, copied).unwrap();
        let assets = target
            .document()
            .assets
            .iter()
            .map(|asset| asset.path.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(assets, ["a.fbx", "t.png"]);

        let object = target.object(&empty).unwrap();
        assert!(Component::reset(object, ComponentKind::LodGroup).is_none());
        let reset = Component::reset(object, ComponentKind::MeshRenderer).unwrap();
        target.set_component(&empty, reset).unwrap();
        let mesh = target
            .object(&empty)
            .unwrap()
            .mesh_renderer
            .clone()
            .unwrap();
        assert_eq!(mesh.model.as_str(), "a.fbx");
        assert!(mesh.texture.is_none());
    }

    #[test]
    fn point_and_spot_components_copy_paste_reset_and_undo_as_whole_values() {
        let mut source = scene();
        let point_id = source.create_object(&NewObject::PointLight, None).unwrap();
        source
            .edit_object(&point_id, None, |object| {
                let light = object.point_light.as_mut().unwrap();
                light.color = [0.5, 0.25, 2.0];
                light.intensity = 3.0;
                light.range = 14.0;
                Ok(())
            })
            .unwrap();
        let point =
            Component::read(source.object(&point_id).unwrap(), ComponentKind::PointLight).unwrap();
        let spot_id = source.create_object(&NewObject::SpotLight, None).unwrap();
        source
            .edit_object(&spot_id, None, |object| {
                let light = object.spot_light.as_mut().unwrap();
                light.intensity = 2.5;
                light.inner_angle_deg = 12.0;
                light.outer_angle_deg = 24.0;
                Ok(())
            })
            .unwrap();
        let spot =
            Component::read(source.object(&spot_id).unwrap(), ComponentKind::SpotLight).unwrap();

        let mut target = scene();
        let target_id = target.create_object(&NewObject::Empty, None).unwrap();
        target.set_component(&target_id, point).unwrap();
        target.set_component(&target_id, spot).unwrap();
        assert_eq!(
            target
                .object(&target_id)
                .unwrap()
                .point_light
                .unwrap()
                .range,
            14.0
        );
        assert_eq!(
            target
                .object(&target_id)
                .unwrap()
                .spot_light
                .unwrap()
                .outer_angle_deg,
            24.0
        );

        let clipboard = objects_to_clipboard(&[
            source.object(&point_id).unwrap().clone(),
            source.object(&spot_id).unwrap().clone(),
        ])
        .unwrap();
        let copied = objects_from_clipboard(&clipboard).unwrap().unwrap();
        assert_eq!(
            copied[0].point_light,
            source.object(&point_id).unwrap().point_light
        );
        assert_eq!(
            copied[1].spot_light,
            source.object(&spot_id).unwrap().spot_light
        );

        let reset = Component::reset(
            target.object(&target_id).unwrap(),
            ComponentKind::PointLight,
        )
        .unwrap();
        target.set_component(&target_id, reset).unwrap();
        assert_eq!(
            target.object(&target_id).unwrap().point_light,
            Some(PointLightComponent::default())
        );
        assert!(target.undo());
        assert_eq!(
            target
                .object(&target_id)
                .unwrap()
                .point_light
                .unwrap()
                .range,
            14.0
        );
        assert!(target.redo());
        assert_eq!(
            target.object(&target_id).unwrap().point_light,
            Some(PointLightComponent::default())
        );
    }

    #[test]
    fn physics_components_add_edit_and_remove_as_undoable_steps() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        scene
            .set_component(&id, Component::Collider(ColliderComponent::default()))
            .unwrap();
        scene
            .set_component(&id, Component::RigidBody(RigidBodyComponent::default()))
            .unwrap();
        scene
            .edit_object(&id, Some("collider-friction"), |object| {
                object.collider.as_mut().unwrap().friction = 0.9;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .collider
                .as_ref()
                .unwrap()
                .friction,
            0.9
        );
        assert!(scene.undo());
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .collider
                .as_ref()
                .unwrap()
                .friction,
            0.5
        );

        // Коллайдер нельзя убрать, оставив тело: удаление идёт одной правкой, один undo вернёт оба.
        scene
            .remove_physics_component(&id, ComponentKind::Collider)
            .unwrap();
        let object = scene.object(&id).unwrap();
        assert!(object.collider.is_none() && object.rigid_body.is_none());
        assert!(scene.undo());
        let object = scene.object(&id).unwrap();
        assert!(object.collider.is_some() && object.rigid_body.is_some());
        assert!(scene.redo());
        assert!(scene.object(&id).unwrap().collider.is_none());
    }

    #[test]
    fn controller_and_body_components_follow_document_rules_and_roll_back() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        let body = Component::RigidBody(RigidBodyComponent::default());
        let error = scene.set_component(&id, body).unwrap_err();
        assert!(error.contains("требует collider"), "{error}");
        assert!(scene.object(&id).unwrap().rigid_body.is_none());

        let mesh = Component::Collider(ColliderComponent {
            shape: engine_scene::ColliderShape::TriangleMesh,
            ..ColliderComponent::default()
        });
        let error = scene.set_component(&id, mesh).unwrap_err();
        assert!(error.contains("mesh_renderer"), "{error}");

        scene
            .set_component(
                &id,
                Component::CharacterController(CharacterControllerComponent::default()),
            )
            .unwrap();
        let error = scene
            .set_component(&id, Component::Collider(ColliderComponent::default()))
            .unwrap_err();
        assert!(error.contains("несовместим"), "{error}");
        assert!(scene.object(&id).unwrap().collider.is_none());

        scene
            .remove_physics_component(&id, ComponentKind::CharacterController)
            .unwrap();
        assert!(scene.object(&id).unwrap().character_controller.is_none());
        assert!(
            scene
                .remove_physics_component(&id, ComponentKind::PointLight)
                .is_err()
        );
    }

    #[test]
    fn audio_source_registers_its_clip_resets_keeping_it_and_rolls_back_invalid_values() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        let clip = VirtualPath::parse("audio/hum.wav").unwrap();
        let loud = AudioSourceComponent {
            volume: 0.5,
            looping: true,
            ..AudioSourceComponent::with_clip(clip.clone())
        };
        scene
            .set_component(&id, Component::AudioSource(loud))
            .unwrap();
        assert!(scene.view().assets.iter().any(|asset| asset.path == clip));

        let copied =
            Component::read(scene.object(&id).unwrap(), ComponentKind::AudioSource).unwrap();
        assert!(matches!(
            copied,
            Component::AudioSource(ref audio) if audio.volume == 0.5 && audio.looping
        ));

        let reset =
            Component::reset(scene.object(&id).unwrap(), ComponentKind::AudioSource).unwrap();
        scene.set_component(&id, reset).unwrap();
        let audio = scene.object(&id).unwrap().audio_source.clone().unwrap();
        assert_eq!(audio, AudioSourceComponent::with_clip(clip.clone()));

        let invalid = AudioSourceComponent {
            volume: -1.0,
            ..AudioSourceComponent::with_clip(clip)
        };
        let error = scene
            .set_component(&id, Component::AudioSource(invalid))
            .unwrap_err();
        assert!(error.contains("volume"), "{error}");
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .audio_source
                .as_ref()
                .unwrap()
                .volume,
            1.0
        );

        scene.remove_audio_source(&id).unwrap();
        assert!(scene.object(&id).unwrap().audio_source.is_none());
        assert!(Component::reset(scene.object(&id).unwrap(), ComponentKind::AudioSource).is_none());
    }

    #[test]
    fn animator_registers_its_clips_resets_keeping_them_and_rolls_back_invalid_values() {
        let mut scene = scene();
        let model = VirtualPath::parse("models/hero.fbx").unwrap();
        let id = scene
            .create_object(&NewObject::Mesh(model, [0.0; 3]), None)
            .unwrap();
        let clip = VirtualPath::parse("anims/walk.fbx").unwrap();
        let animator = AnimatorComponent {
            clips: vec![clip.clone()],
            speed: 2.0,
            ..AnimatorComponent::new()
        };
        scene
            .set_component(&id, Component::Animator(animator))
            .unwrap();
        assert!(scene.view().assets.iter().any(|asset| asset.path == clip));

        let copied = Component::read(scene.object(&id).unwrap(), ComponentKind::Animator).unwrap();
        assert!(matches!(
            copied,
            Component::Animator(ref value) if value.speed == 2.0
        ));

        // Reset сохраняет клипы: без них аниматор пуст, как и источник звука без клипа.
        let reset = Component::reset(scene.object(&id).unwrap(), ComponentKind::Animator).unwrap();
        scene.set_component(&id, reset).unwrap();
        assert_eq!(
            scene.object(&id).unwrap().animator.clone().unwrap(),
            AnimatorComponent {
                clips: vec![clip],
                ..AnimatorComponent::new()
            }
        );

        let invalid = AnimatorComponent {
            speed: -1.0,
            ..AnimatorComponent::new()
        };
        let error = scene
            .set_component(&id, Component::Animator(invalid))
            .unwrap_err();
        assert!(error.contains("speed"), "{error}");
        assert_eq!(
            scene.object(&id).unwrap().animator.as_ref().unwrap().speed,
            1.0
        );

        scene.remove_animator(&id).unwrap();
        assert!(scene.object(&id).unwrap().animator.is_none());
        assert!(Component::reset(scene.object(&id).unwrap(), ComponentKind::Animator).is_none());
    }

    #[test]
    fn animator_edits_and_removal_are_undoable_as_whole_components() {
        let mut scene = scene();
        let model = VirtualPath::parse("models/hero.fbx").unwrap();
        let id = scene
            .create_object(&NewObject::Mesh(model, [0.0; 3]), None)
            .unwrap();
        let speed = |scene: &EditorScene| {
            scene
                .object(&id)
                .unwrap()
                .animator
                .as_ref()
                .map(|a| a.speed)
        };
        scene
            .set_component(
                &id,
                Component::Animator(AnimatorComponent {
                    speed: 2.0,
                    ..AnimatorComponent::new()
                }),
            )
            .unwrap();
        scene
            .set_component(
                &id,
                Component::Animator(AnimatorComponent {
                    speed: 3.0,
                    ..AnimatorComponent::new()
                }),
            )
            .unwrap();
        assert_eq!(speed(&scene), Some(3.0));

        assert!(scene.undo());
        assert_eq!(speed(&scene), Some(2.0));
        assert!(scene.redo());
        assert_eq!(speed(&scene), Some(3.0));

        scene.remove_animator(&id).unwrap();
        assert_eq!(speed(&scene), None);
        assert!(scene.undo());
        assert_eq!(
            speed(&scene),
            Some(3.0),
            "удаление откатывается целым компонентом"
        );
    }

    #[test]
    fn particle_emitter_adds_registers_texture_and_undoes_removal() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        let texture = VirtualPath::parse("fx/smoke.png").unwrap();
        let emitter = engine_scene::ParticleEmitterComponent {
            texture: Some(texture.clone()),
            ..engine_scene::ParticleEmitterComponent::default()
        };
        scene
            .set_component(&id, Component::ParticleEmitter(emitter.clone()))
            .unwrap();
        assert_eq!(
            scene.object(&id).unwrap().particle_emitter.as_ref(),
            Some(&emitter)
        );
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path == texture),
            "texture must be declared in [[assets]]"
        );

        let reset =
            Component::reset(scene.object(&id).unwrap(), ComponentKind::ParticleEmitter).unwrap();
        scene.set_component(&id, reset).unwrap();
        let kept = scene.object(&id).unwrap().particle_emitter.clone().unwrap();
        assert_eq!(
            kept.texture,
            Some(texture.clone()),
            "reset keeps the texture"
        );
        assert_eq!(
            kept.capacity,
            engine_scene::ParticleEmitterComponent::default().capacity
        );

        scene.remove_particle_emitter(&id).unwrap();
        assert!(scene.object(&id).unwrap().particle_emitter.is_none());
        assert!(scene.undo());
        assert_eq!(
            scene
                .object(&id)
                .unwrap()
                .particle_emitter
                .as_ref()
                .map(|value| value.capacity),
            Some(kept.capacity)
        );
    }

    #[test]
    fn pasting_an_invalid_component_is_rolled_back() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        let light = Component::DirectionalLight(default_light());
        assert!(scene.set_component(&id, light).is_err());
        assert!(scene.object(&id).unwrap().directional_light.is_none());
        let transform = Component::Transform(moved(5.0));
        scene.set_component(&id, transform).unwrap();
        assert_eq!(position(&scene, &id)[0], 5.0);
    }

    #[test]
    fn ending_a_continuous_edit_by_key_leaves_other_keys_running() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        scene
            .set_transforms(&[(id.clone(), moved(1.0))], Some("drag"))
            .unwrap();
        scene.end_continuous_edit_for("picker");
        scene
            .set_transforms(&[(id.clone(), moved(2.0))], Some("drag"))
            .unwrap();
        scene.end_continuous_edit_for("drag");
        scene
            .set_transforms(&[(id.clone(), moved(3.0))], Some("drag"))
            .unwrap();
        assert!(scene.undo());
        assert_eq!(position(&scene, &id)[0], 2.0);
        assert!(scene.undo());
        assert_eq!(position(&scene, &id)[0], 0.0);
    }

    #[test]
    fn undo_and_redo_restore_documents_and_new_edit_clears_redo() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        assert!(scene.undo());
        assert!(scene.object(&id).is_none());
        assert_eq!(scene.selection(), None);
        assert!(scene.redo());
        assert!(scene.object(&id).is_some());
        assert!(scene.undo());
        scene.create_object(&NewObject::Empty, None).unwrap();
        assert!(!scene.can_redo());
        assert!(!scene.redo());
    }

    #[test]
    fn continuous_edit_occupies_one_history_entry_until_finished() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        for x in [1.0, 2.0, 3.0] {
            scene
                .set_transforms(&[(id.clone(), moved(x))], Some("drag"))
                .unwrap();
        }
        assert_eq!(position(&scene, &id)[0], 3.0);
        scene.end_continuous_edit();
        scene
            .set_transforms(&[(id.clone(), moved(4.0))], Some("drag"))
            .unwrap();
        assert!(scene.undo());
        assert_eq!(position(&scene, &id)[0], 3.0);
        assert!(scene.undo());
        assert_eq!(position(&scene, &id)[0], 0.0);
    }

    #[test]
    fn history_depth_is_bounded() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        for step in 0..(MAX_UNDO_DEPTH + 10) {
            scene
                .set_transforms(&[(id.clone(), moved(step as f32))], None)
                .unwrap();
        }
        let mut undone = 0;
        while scene.undo() {
            undone += 1;
        }
        assert_eq!(undone, MAX_UNDO_DEPTH);
    }

    #[test]
    fn long_ids_stay_within_the_schema_limit() {
        let mut document = scene().document;
        let long = "x".repeat(100);
        let first = unique_id(&document, &long, &HashSet::new());
        assert_eq!(first.len(), MAX_ID_LENGTH);
        document.objects[0].id = first.clone();
        let second = unique_id(&document, &long, &HashSet::new());
        assert_eq!(second.len(), MAX_ID_LENGTH);
        assert_ne!(first, second);
        assert_eq!(unique_id(&document, "", &HashSet::new()), "object");
    }

    #[test]
    fn selection_toggles_adds_and_keeps_the_last_object_active() {
        let mut scene = scene();
        scene.select(Some("camera"));
        scene.add_to_selection("sun");
        assert_eq!(scene.selected_ids(), ["camera", "sun"]);
        assert_eq!(scene.selection(), Some("sun"));
        // Повторное добавление делает объект активным, а не дублирует его.
        scene.add_to_selection("camera");
        assert_eq!(scene.selected_ids(), ["sun", "camera"]);
        scene.toggle_selection("camera");
        assert_eq!(scene.selected_ids(), ["sun"]);
        scene.toggle_selection("missing");
        assert_eq!(scene.selected_ids(), ["sun"]);
        scene.select(None);
        assert!(scene.selected_ids().is_empty());
    }

    #[test]
    fn group_delete_skips_descendants_of_selected_parents_and_is_one_undo_step() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        let other = scene.create_object(&NewObject::Empty, None).unwrap();
        scene.select(Some(&child));
        scene.add_to_selection(&parent);
        scene.add_to_selection(&other);
        assert_eq!(scene.selection_roots(), [parent.clone(), other.clone()]);
        scene.delete_selection().unwrap();
        assert_eq!(scene.document().objects.len(), 2);
        assert!(scene.selected_ids().is_empty());
        assert!(scene.undo());
        for id in [&parent, &child, &other] {
            assert!(scene.object(id).is_some());
        }
    }

    #[test]
    fn failed_group_delete_changes_nothing() {
        let mut scene = scene();
        let empty = scene.create_object(&NewObject::Empty, None).unwrap();
        scene.add_to_selection("camera");
        assert!(scene.delete_selection().is_err());
        assert!(scene.object(&empty).is_some());
        assert_eq!(scene.selected_ids(), [empty, "camera".to_owned()]);
    }

    #[test]
    fn group_duplicate_copies_each_subtree_once_and_selects_the_copies() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        scene.select(Some(&parent));
        scene.add_to_selection(&child);
        scene.add_to_selection("camera");
        let copies = scene.duplicate_selection().unwrap();
        assert_eq!(copies.len(), 2);
        assert_eq!(scene.selected_ids(), copies.as_slice());
        // Родитель, его ребёнок и камера — по одной копии.
        assert_eq!(scene.document().objects.len(), 4 + 3);
        // Копии корней идут в порядке документа: камера раньше нового родителя.
        let parent_copy = copies
            .iter()
            .find(|id| scene.object(id).unwrap().camera.is_none())
            .unwrap();
        let copied_child = scene
            .document()
            .objects
            .iter()
            .filter(|object| object.parent.as_ref() == Some(parent_copy))
            .count();
        assert_eq!(copied_child, 1);
        assert!(scene.undo());
        assert_eq!(scene.document().objects.len(), 4);
    }

    #[test]
    fn selection_follows_id_changes_and_undo() {
        let mut scene = scene();
        let id = scene.create_object(&NewObject::Empty, None).unwrap();
        scene.add_to_selection("camera");
        scene.change_id(&id, "renamed").unwrap();
        assert_eq!(scene.selected_ids(), ["renamed", "camera"]);
        // Отмена переименования убирает из выделения id, которого больше нет.
        assert!(scene.undo());
        assert_eq!(scene.selected_ids(), ["camera"]);
    }

    #[test]
    fn group_transform_edit_is_atomic() {
        let mut scene = scene();
        let a = scene.create_object(&NewObject::Empty, None).unwrap();
        let invalid = SceneTransform {
            position: [f32::NAN, 0.0, 0.0],
            ..SceneTransform::default()
        };
        let result = scene.set_transforms(
            &[(a.clone(), moved(1.0)), ("camera".to_owned(), invalid)],
            None,
        );
        assert!(result.is_err());
        assert_eq!(position(&scene, &a)[0], 0.0);
        scene
            .set_transforms(
                &[(a.clone(), moved(2.0)), ("sun".to_owned(), moved(3.0))],
                None,
            )
            .unwrap();
        assert_eq!(position(&scene, &a)[0], 2.0);
        assert_eq!(position(&scene, "sun")[0], 3.0);
        assert!(scene.undo());
        assert_eq!(position(&scene, &a)[0], 0.0);
    }

    #[test]
    fn clipboard_objects_paste_into_another_scene_with_new_ids_and_assets() {
        let mut source = scene();
        let parent = source
            .create_object(
                &NewObject::Mesh(path("/models/a.fbx"), [1.0, 0.0, 0.0]),
                None,
            )
            .unwrap();
        source
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        source
            .add_lite_script(&parent, path("/scripts/spin.lua"))
            .unwrap();
        let text =
            objects_to_clipboard(&source.subtrees(std::slice::from_ref(&parent)).unwrap()).unwrap();

        let mut target = scene();
        let objects = objects_from_clipboard(&text).unwrap().unwrap();
        let pasted = target.insert_copies(objects.clone()).unwrap();
        assert_eq!(pasted, std::slice::from_ref(&parent));
        let assets = target
            .document()
            .assets
            .iter()
            .map(|asset| asset.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(assets, ["models/a.fbx", "scripts/spin.lua"]);
        // Повторная вставка в ту же сцену получает свободные id и своего ребёнка.
        let again = target.insert_copies(objects).unwrap();
        assert_ne!(again, pasted);
        assert_eq!(target.selected_ids(), again.as_slice());
        let children = target
            .document()
            .objects
            .iter()
            .filter(|object| object.parent.as_ref() == Some(&again[0]))
            .count();
        assert_eq!(children, 1);
        assert_eq!(target.document().objects.len(), 2 + 4);
    }

    #[test]
    fn pasted_roots_keep_an_existing_parent_and_invalid_pastes_are_rejected() {
        let mut scene = scene();
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene
            .create_object(&NewObject::Empty, Some(&parent))
            .unwrap();
        let copies = scene
            .insert_copies(scene.subtrees(&[child]).unwrap())
            .unwrap();
        assert_eq!(
            scene.object(&copies[0]).unwrap().parent.as_ref(),
            Some(&parent)
        );

        let light = scene.subtrees(&["sun".to_owned()]).unwrap();
        let before = scene.document().objects.len();
        let error = scene.insert_copies(light).unwrap_err();
        assert!(error.contains("directional_light"), "{error}");
        assert_eq!(scene.document().objects.len(), before);

        assert!(objects_from_clipboard("plain text").is_none());
        assert!(objects_from_clipboard("{\"other\": 1}").is_none());
        assert!(
            objects_from_clipboard("{\"rve_editor_objects\": 5}")
                .unwrap()
                .is_err()
        );
    }

    fn prefab_loader(prefab: &'static str) -> PrefabLoader {
        Rc::new(move |path| SceneDocument::parse_prefab(format!("/{}", path.as_str()), prefab))
    }

    #[test]
    fn scene_prefab_assets_add_and_remove_as_undoable_steps() {
        let mut scene = scene();
        let bullet = path("/prefabs/bullet.rveprefab");
        scene.add_scene_prefab(bullet.clone()).unwrap();
        assert_eq!(scene.document().assets.len(), 1);
        assert!(scene.add_scene_prefab(bullet.clone()).is_err());
        assert!(scene.add_scene_prefab(path("/models/a.fbx")).is_err());
        assert_eq!(scene.document().assets.len(), 1);

        scene.remove_scene_prefab(&bullet).unwrap();
        assert!(scene.document().assets.is_empty());
        assert!(scene.remove_scene_prefab(&bullet).is_err());
        assert!(scene.remove_scene_prefab(&path("/models/a.fbx")).is_err());

        assert!(scene.undo());
        assert_eq!(scene.document().assets[0].path, bullet);
        assert!(scene.redo());
        assert!(scene.document().assets.is_empty());
    }

    #[test]
    fn scene_prefab_used_by_an_instance_cannot_be_removed() {
        let mut scene = scene();
        scene
            .set_prefab_loader(prefab_loader("[[objects]]\nid = \"frame\"\n"))
            .unwrap();
        let door = path("/prefabs/door.rveprefab");
        scene
            .create_object(&NewObject::Prefab(door.clone(), [0.0; 3]), None)
            .unwrap();
        let revision = scene.revision();

        assert!(scene.remove_scene_prefab(&door).is_err());
        assert_eq!(scene.revision(), revision);
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path == door)
        );
    }

    #[test]
    fn scene_prefab_archive_and_max_instances_are_validated_and_undoable() {
        let mut scene = scene();
        let bullet = path("/prefabs/bullet.rveprefab");
        scene.add_scene_prefab(bullet.clone()).unwrap();
        let asset = |scene: &EditorScene| scene.document().assets[0].clone();

        scene
            .set_scene_prefab_archive(&bullet, Some("weapons".to_owned()))
            .unwrap();
        scene
            .set_scene_prefab_max_instances(&bullet, Some(100))
            .unwrap();
        assert_eq!(asset(&scene).archive.as_deref(), Some("weapons"));
        assert_eq!(asset(&scene).max_instances, Some(100));

        for archive in ["shared", "base", "bad name"] {
            let archive = Some(archive.to_owned());
            assert!(scene.set_scene_prefab_archive(&bullet, archive).is_err());
        }
        assert!(
            scene
                .set_scene_prefab_max_instances(&bullet, Some(0))
                .is_err()
        );
        assert!(
            scene
                .set_scene_prefab_max_instances(&path("/models/a.fbx"), Some(1))
                .is_err()
        );
        assert_eq!(asset(&scene).archive.as_deref(), Some("weapons"));
        assert_eq!(asset(&scene).max_instances, Some(100));

        assert!(scene.undo());
        assert_eq!(asset(&scene).max_instances, None);
        assert_eq!(asset(&scene).archive.as_deref(), Some("weapons"));
        assert!(scene.undo());
        assert_eq!(asset(&scene).archive, None);
        assert!(scene.redo());
        assert!(scene.redo());
        assert_eq!(asset(&scene).max_instances, Some(100));
    }

    #[test]
    fn scene_prefab_assets_serialize_without_extra_fields() {
        let mut scene = scene();
        let bullet = path("/prefabs/bullet.rveprefab");
        scene.add_scene_prefab(bullet.clone()).unwrap();
        let text = scene.document().to_toml().unwrap();
        assert!(!text.contains("archive"), "{text}");
        assert!(!text.contains("max_instances"), "{text}");

        scene
            .set_scene_prefab_max_instances(&bullet, Some(7))
            .unwrap();
        let text = scene.document().to_toml().unwrap();
        assert!(text.contains("max_instances = 7"), "{text}");
        assert!(!text.contains("archive"), "{text}");
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rve-editor-scene-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_writes_a_parseable_file_and_leaves_no_temporary() {
        let directory = TempDir::new();
        let file = directory.0.join("a.rvescene");
        let mut scene = scene();
        scene.create_object(&NewObject::Empty, None).unwrap();
        scene.save(&file, path("/a.rvescene")).unwrap();
        assert!(!scene.is_dirty());
        assert_eq!(scene.path().unwrap().as_str(), "a.rvescene");
        assert_eq!(scene.document().source_label(), "a.rvescene");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
        let reopened = EditorScene::parse(
            path("/a.rvescene"),
            &fs::read_to_string(&file).unwrap(),
            no_prefabs(),
        )
        .unwrap();
        assert_eq!(reopened.document().objects.len(), 3);
    }

    #[test]
    fn failed_save_keeps_the_scene_dirty_and_the_previous_path() {
        let directory = TempDir::new();
        let missing = directory.0.join("missing/a.rvescene");
        let mut scene = scene();
        scene.create_object(&NewObject::Empty, None).unwrap();
        assert!(scene.save(&missing, path("/a.rvescene")).is_err());
        assert!(scene.is_dirty());
        assert!(scene.path().is_none());
    }

    #[test]
    fn demo_scene_survives_a_save_round_trip() {
        let source = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/demo-game/Assets/scenes/main.rvescene"
        );
        let directory = TempDir::new();
        let file = directory.0.join("main.rvescene");
        let label = path("/scenes/main.rvescene");
        let project = crate::project::OpenProject::open(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/demo-game"
        )))
        .unwrap();
        let original = EditorScene::parse(
            label.clone(),
            &fs::read_to_string(source).unwrap(),
            project.prefab_loader(),
        )
        .unwrap();
        let expected = original.document().to_toml().unwrap();
        let mut scene = original;
        scene.save(&file, label.clone()).unwrap();
        let saved = fs::read_to_string(&file).unwrap();
        assert_eq!(saved, expected);
        let reopened = EditorScene::parse(label, &saved, project.prefab_loader()).unwrap();
        assert_eq!(
            reopened.object("orbit.moon").unwrap().name.as_deref(),
            Some("Moon")
        );
        let cube = reopened.object("shared-cube").unwrap();
        assert_eq!(cube.lite_scripts.len(), 1);
        assert_eq!(cube.heavy_scripts[0].name, "Bobber");
        assert_eq!(cube.heavy_scripts[0].params.len(), 2);
    }
}
