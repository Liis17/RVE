//! Редактируемая сцена: команды над документом, undo/redo и сохранение.

use engine_files::VirtualPath;
use engine_scene::{
    CameraComponent, DirectionalLightComponent, Expansion, LodGroup, MeshRenderer, ObjectOrigin,
    PrefabInstance, SceneAsset, SceneDocument, SceneError, SceneObject, SceneTransform,
};
use std::collections::HashSet;
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
fov_deg = 60.0
near = 0.1
far = 1000.0

[[objects]]
id = "sun"
name = "Directional Light"
[objects.transform]
rotation = [50.0, 30.0, 0.0]
[objects.directional_light]
shadows = true
shadow_radius = 20.0
"#;

/// Вид объекта, создаваемого из меню или перетаскиванием ресурса.
#[derive(Clone, Debug, PartialEq)]
pub enum NewObject {
    Empty,
    Camera,
    DirectionalLight,
    /// Модель и локальная позиция нового объекта.
    Mesh(VirtualPath, [f32; 3]),
    /// Экземпляр префаба и локальная позиция его корня.
    Prefab(VirtualPath, [f32; 3]),
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
    selection: Option<String>,
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
            selection: None,
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
        if self
            .selection
            .as_deref()
            .is_some_and(|id| self.object(id).is_none())
        {
            self.selection = None;
        }
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

    pub fn selection(&self) -> Option<&str> {
        self.selection.as_deref()
    }

    pub fn selected_object(&self) -> Option<&SceneObject> {
        self.object(self.selection.as_deref()?)
    }

    /// Объект раскрытого документа, в том числе объект экземпляра.
    pub fn object(&self, id: &str) -> Option<&SceneObject> {
        self.view().objects.iter().find(|object| object.id == id)
    }

    pub fn select(&mut self, id: Option<&str>) {
        self.selection = id.filter(|id| self.object(id).is_some()).map(str::to_owned);
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
        if self
            .selection
            .as_deref()
            .is_some_and(|id| self.object(id).is_none())
        {
            self.selection = None;
        }
    }

    /// Завершает непрерывную правку: следующее изменение создаст новую запись истории.
    pub fn end_continuous_edit(&mut self) {
        self.coalesce = None;
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
            transform: SceneTransform::default(),
            camera: None,
            mesh_renderer: None,
            lod_group: None,
            directional_light: None,
            lite_scripts: Vec::new(),
            heavy_scripts: Vec::new(),
        };
        self.edit(None, |document| {
            match kind {
                NewObject::Empty | NewObject::Prefab(..) => {}
                NewObject::Camera => {
                    object.camera = Some(CameraComponent {
                        fov_deg: 60.0,
                        near: 0.1,
                        far: 1000.0,
                    });
                }
                NewObject::DirectionalLight => {
                    object.transform.rotation = [50.0, 30.0, 0.0];
                    object.directional_light = Some(default_light());
                }
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
        self.selection = Some(id.clone());
        Ok(id)
    }

    /// Удаляет объект или экземпляр вместе с потомками; единственную камеру удалить нельзя.
    /// Часть экземпляра удалить нельзя: удаляется весь экземпляр или делается Unpack.
    pub fn delete_object(&mut self, id: &str) -> Result<(), String> {
        self.require_own(id)?;
        let removed = subtree_ids(self.view(), id)?;
        self.edit(None, |document| {
            document
                .objects
                .retain(|object| !removed.contains(&object.id));
            document
                .instances
                .retain(|instance| !removed.contains(&instance.id));
            Ok(())
        })?;
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| removed.contains(selection))
        {
            self.selection = None;
        }
        Ok(())
    }

    /// Копирует объект или экземпляр с потомками; ссылки `parent` внутри копии, в том числе
    /// на объекты скопированных экземпляров, указывают на новые копии.
    pub fn duplicate_object(&mut self, id: &str) -> Result<String, String> {
        self.require_own(id)?;
        let subtree = subtree_ids(self.view(), id)?;
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
        let mut renamed = std::collections::HashMap::new();
        for source in &source_ids {
            let copy_id = unique_id(&self.document, source, &reserved);
            reserved.insert(copy_id.clone());
            renamed.insert(source.clone(), copy_id);
        }
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
            Ok(())
        })?;
        let copy = renamed[id].clone();
        self.selection = Some(copy.clone());
        Ok(copy)
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
        if let Some(selection) = self.selection.take() {
            self.selection = Some(rename_reference(&renamed, selection));
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

    /// Удаляет mesh renderer и его обязательную LOD Group в одной правке.
    pub fn remove_mesh_renderer(&mut self, id: &str) -> Result<(), String> {
        self.edit_object(id, None, |object| {
            object.mesh_renderer = None;
            object.lod_group = None;
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
        shadow_center: [0.0; 3],
        shadow_radius: Some(20.0),
    }
}

pub(crate) fn write_atomic(file: &Path, contents: &[u8]) -> Result<(), String> {
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

fn ensure_asset(document: &mut SceneDocument, path: &VirtualPath) {
    if !document.assets.iter().any(|asset| &asset.path == path) {
        document.assets.push(SceneAsset {
            path: path.clone(),
            archive: None,
        });
    }
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
        assert!(scene.duplicate_object("sun").is_err());
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
        let result = scene.edit_object("camera", None, |object| {
            object.camera.as_mut().unwrap().near = -1.0;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(scene.object("camera").unwrap().camera.unwrap().near, 0.1);
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
            scene.set_transform(&id, moved(x), Some("drag")).unwrap();
        }
        assert_eq!(position(&scene, &id)[0], 3.0);
        scene.end_continuous_edit();
        scene.set_transform(&id, moved(4.0), Some("drag")).unwrap();
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
            scene.set_transform(&id, moved(step as f32), None).unwrap();
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
