//! Команды над экземплярами префабов: размещение, создание, Unpack и правка через overrides.

use super::{
    EditorScene, ensure_asset, expand_document_with, file_stem, rename_reference, subtree_ids,
    unique_id,
};
use engine_files::VirtualPath;
use engine_scene::{
    HeavyParamsOverride, LiteParamsOverride, MaterialAlphaMode, MaterialOverride,
    MeshRendererOverride, PrefabInstance, PrefabOverride, SceneDocument, SceneObject,
    SceneTransform, TransformOverride,
};
use std::collections::{BTreeMap, HashMap, HashSet};

impl EditorScene {
    /// Размещает экземпляр префаба; путь добавляется в список ресурсов документа.
    pub(super) fn instantiate(
        &mut self,
        prefab: &VirtualPath,
        parent: Option<&str>,
        position: [f32; 3],
    ) -> Result<String, String> {
        let id = unique_id(&self.document, file_stem(prefab), &HashSet::new());
        let instance = PrefabInstance {
            id: id.clone(),
            name: None,
            parent: parent.map(str::to_owned),
            prefab: prefab.clone(),
            transform: Some(SceneTransform {
                position,
                ..SceneTransform::default()
            }),
            overrides: Vec::new(),
        };
        self.edit(None, |document| {
            ensure_asset(document, prefab);
            document.instances.push(instance);
            Ok(())
        })?;
        self.selection = vec![id.clone()];
        Ok(id)
    }

    /// Префаб из поддерева собственного объекта `id`: объекты и экземпляры поддерева с их
    /// ресурсами. Корень получает единичную трансформацию и становится корнем префаба.
    pub fn prefab_from_subtree(&self, id: &str, label: &str) -> Result<SceneDocument, String> {
        if !self.document.objects.iter().any(|object| object.id == id) {
            return Err(format!(
                "префаб создаётся из собственного объекта сцены, {id:?} им не является"
            ));
        }
        let subtree = subtree_ids(self.view(), id)?;
        let mut prefab = self.document.clone();
        prefab.objects.retain(|object| subtree.contains(&object.id));
        prefab
            .instances
            .retain(|instance| subtree.contains(&instance.id));
        for object in &mut prefab.objects {
            if object.id == id {
                object.parent = None;
                object.transform = SceneTransform::default();
            }
        }
        let used = used_paths(&prefab);
        prefab.assets.retain(|asset| used.contains(&asset.path));
        let prefab = prefab.into_prefab(label);
        let text = prefab.to_toml().map_err(|error| error.to_string())?;
        SceneDocument::parse_prefab(label, &text).map_err(|error| error.to_string())
    }

    /// Заменяет поддерево объекта `id` экземпляром префаба `prefab` с тем же id, родителем
    /// и трансформацией корня; ссылки на объекты поддерева переходят на объекты экземпляра.
    pub fn replace_with_instance(&mut self, id: &str, prefab: &VirtualPath) -> Result<(), String> {
        let subtree = subtree_ids(self.view(), id)?;
        let root = self
            .document
            .objects
            .iter()
            .find(|object| object.id == id)
            .cloned()
            .ok_or_else(|| format!("объект {id:?} не найден"))?;
        let mapped = |reference: String| {
            let head = reference.split('.').next().unwrap_or_default();
            if head == id || !subtree.contains(head) {
                reference
            } else {
                format!("{id}.{reference}")
            }
        };
        self.edit(None, |document| {
            document
                .objects
                .retain(|object| !subtree.contains(&object.id));
            document
                .instances
                .retain(|instance| !subtree.contains(&instance.id));
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
                *parent = parent.take().map(mapped);
            }
            ensure_asset(document, prefab);
            document.instances.push(PrefabInstance {
                id: id.to_owned(),
                name: None,
                parent: root.parent.clone(),
                prefab: prefab.clone(),
                transform: Some(root.transform),
                overrides: Vec::new(),
            });
            Ok(())
        })?;
        self.selection = vec![id.to_owned()];
        Ok(())
    }

    /// Заменяет экземпляр объектами его префаба одного уровня с новыми авторскими id
    /// (`door1-handle`). Вложенные экземпляры остаются экземплярами, overrides переходят
    /// на них или на скопированные объекты.
    pub fn unpack(&mut self, id: &str) -> Result<(), String> {
        let instance = self
            .instance(id)
            .cloned()
            .ok_or_else(|| format!("{id:?} не является экземпляром префаба"))?;
        let prefab = (self.loader)(&instance.prefab).map_err(|error| error.to_string())?;
        let prefab_root = prefab
            .objects
            .iter()
            .map(|object| (&object.id, &object.parent))
            .chain(
                prefab
                    .instances
                    .iter()
                    .map(|nested| (&nested.id, &nested.parent)),
            )
            .find(|(_, parent)| parent.is_none())
            .map(|(root, _)| root.clone())
            .ok_or("у префаба нет корня")?;

        // Новые id: корень — id экземпляра, остальные — `<экземпляр>-<id>`.
        let mut reserved = HashSet::new();
        let mut renamed = HashMap::new();
        let locals = prefab
            .objects
            .iter()
            .map(|object| &object.id)
            .chain(prefab.instances.iter().map(|nested| &nested.id));
        for local in locals {
            let new = if *local == prefab_root {
                id.to_owned()
            } else {
                unique_id(&self.document, &format!("{id}-{local}"), &reserved)
            };
            reserved.insert(new.clone());
            renamed.insert(local.clone(), new);
        }

        let mut objects = prefab.objects.clone();
        let mut nested = prefab.instances.clone();
        for change in &instance.overrides {
            let (head, rest) = match change.target.split_once('.') {
                Some((head, rest)) => (head, Some(rest)),
                None => (change.target.as_str(), None),
            };
            if let Some(object) = objects.iter_mut().find(|object| object.id == head) {
                engine_scene::apply_override(object, change)?;
                continue;
            }
            let Some(inner) = nested.iter_mut().find(|inner| inner.id == head) else {
                return Err(format!("override target {:?} не найден", change.target));
            };
            let mut change = change.clone();
            let target = match rest {
                Some(rest) => rest.to_owned(),
                None => {
                    let base = self.base_object(id, &format!("{id}.{head}"))?;
                    promote_root_override(inner, &mut change, &base);
                    nested_root(&self.loader, &inner.prefab)?
                }
            };
            change.target = target;
            match inner
                .overrides
                .iter_mut()
                .find(|existing| existing.target == change.target)
            {
                Some(existing) => merge_override(existing, &change),
                None if !is_empty(&change) => inner.overrides.push(change),
                None => {}
            }
        }
        let remap =
            |parent: Option<String>| parent.map(|parent| rename_reference(&renamed, parent));
        for object in &mut objects {
            object.parent = remap(object.parent.take());
            object.id = renamed[&object.id].clone();
        }
        for inner in &mut nested {
            inner.parent = remap(inner.parent.take());
            inner.id = renamed[&inner.id].clone();
        }
        // Поля самого экземпляра переходят на новый корень.
        let apply_root = |name: &mut Option<String>,
                          parent: &mut Option<String>,
                          transform: Option<&mut SceneTransform>| {
            if let Some(value) = &instance.name {
                *name = (!value.is_empty()).then(|| value.clone());
            }
            *parent = instance.parent.clone();
            if let (Some(target), Some(value)) = (transform, instance.transform) {
                *target = value;
            }
        };
        if let Some(root) = objects.iter_mut().find(|object| object.id == id) {
            apply_root(&mut root.name, &mut root.parent, Some(&mut root.transform));
        } else if let Some(root) = nested.iter_mut().find(|inner| inner.id == id) {
            let mut transform = root.transform.unwrap_or_default();
            apply_root(&mut root.name, &mut root.parent, Some(&mut transform));
            if instance.transform.is_some() {
                root.transform = Some(transform);
            }
        }
        // Внешние ссылки `id.<local>...` переходят на новые объекты.
        let outside = |reference: String| match reference.strip_prefix(&format!("{id}.")) {
            Some(local) => rename_reference(&renamed, local.to_owned()),
            None => reference,
        };
        let assets = prefab.assets.clone();
        self.edit(None, |document| {
            document.instances.retain(|existing| existing.id != id);
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
                *parent = parent.take().map(outside);
            }
            for asset in assets {
                if !document
                    .assets
                    .iter()
                    .any(|existing| existing.path == asset.path)
                {
                    document.assets.push(asset);
                }
            }
            document.objects.extend(objects);
            document.instances.extend(nested);
            Ok(())
        })?;
        self.selection = vec![id.to_owned()];
        Ok(())
    }

    /// Правка объекта экземпляра: изменение сравнивается с раскрытием без overrides этого
    /// экземпляра и записывается разреженным override. У корня `name` и `transform`
    /// пишутся в сам экземпляр.
    pub(super) fn edit_prefab_object(
        &mut self,
        id: &str,
        coalesce: Option<&str>,
        change: impl FnOnce(&mut SceneObject) -> Result<(), String>,
    ) -> Result<(), String> {
        let origin = self
            .origin(id)
            .cloned()
            .ok_or_else(|| format!("объект {id:?} не найден"))?;
        let current = self
            .object(id)
            .cloned()
            .ok_or_else(|| format!("объект {id:?} не найден"))?;
        let mut edited = current.clone();
        change(&mut edited)?;
        if edited.parent != current.parent {
            return Err(format!(
                "объекту {id:?} внутри экземпляра нельзя менять родителя"
            ));
        }
        let base = self.base_object(&origin.instance, id)?;
        let is_root = origin.instance == id;
        let mut diff = object_override(&base, &edited, &origin.target)?;
        let mut name = None;
        let mut transform = None;
        if is_root {
            // У корня поля экземпляра побеждают override, поэтому пишутся туда.
            name = diff.name.take().map(Some);
            if diff.transform.take().is_some() || edited.transform != current.transform {
                transform = Some(edited.transform);
            }
        }
        let assets = override_paths(&diff);
        self.edit(coalesce, |document| {
            // Ресурсы override объявляются в документе экземпляра.
            for path in &assets {
                ensure_asset(document, path);
            }
            let instance = document
                .instances
                .iter_mut()
                .find(|instance| instance.id == origin.instance)
                .ok_or_else(|| format!("экземпляр {:?} не найден", origin.instance))?;
            if let Some(name) = name {
                instance.name = name;
            }
            if let Some(transform) = transform {
                instance.transform = Some(transform);
            }
            instance
                .overrides
                .retain(|existing| existing.target != origin.target);
            if !is_empty(&diff) {
                instance.overrides.push(diff);
            }
            Ok(())
        })
    }

    /// Объект экземпляра в раскрытии без overrides этого экземпляра.
    pub fn base_object(&self, instance: &str, id: &str) -> Result<SceneObject, String> {
        let plain = expand_document_with(
            &self.document,
            self.path.as_ref(),
            &self.loader,
            Some(instance),
        )?;
        plain
            .document
            .objects
            .into_iter()
            .find(|object| object.id == id)
            .ok_or_else(|| format!("объект {id:?} не найден в префабе"))
    }

    /// Override объекта экземпляра, если он задан.
    pub fn object_override(&self, id: &str) -> Option<&PrefabOverride> {
        let origin = self.origin(id)?;
        self.instance(&origin.instance)?
            .overrides
            .iter()
            .find(|change| change.target == origin.target)
    }

    /// Убирает override объекта экземпляра целиком: объект возвращается к префабу.
    /// У корня сбрасывается имя, а трансформация размещения сохраняется.
    pub fn revert_override(&mut self, id: &str) -> Result<(), String> {
        let origin = self
            .origin(id)
            .cloned()
            .ok_or_else(|| format!("{id:?} не является объектом экземпляра"))?;
        self.edit(None, |document| {
            let instance = document
                .instances
                .iter_mut()
                .find(|instance| instance.id == origin.instance)
                .ok_or_else(|| format!("экземпляр {:?} не найден", origin.instance))?;
            instance
                .overrides
                .retain(|change| change.target != origin.target);
            if origin.instance == id {
                instance.name = None;
            }
            Ok(())
        })
    }
}

/// Ресурсы, на которые ссылается документ: модели, текстуры, скрипты и префабы.
fn used_paths(document: &SceneDocument) -> HashSet<VirtualPath> {
    let mut paths = HashSet::new();
    let mut lod = |value: &str| {
        if let Ok(path) = VirtualPath::parse(value) {
            paths.insert(path);
        }
    };
    let mut collected = Vec::new();
    for object in &document.objects {
        if let Some(mesh) = &object.mesh_renderer {
            collected.push(mesh.model.clone());
            collected.extend(mesh.texture.clone());
            if let Some(material) = &mesh.material {
                collected.extend(material_paths(material));
            }
        }
        if let Some(group) = &object.lod_group {
            for level in &group.levels {
                lod(&level.model);
                if let Some(texture) = &level.texture {
                    lod(texture);
                }
            }
        }
        collected.extend(object.lite_scripts.iter().map(|script| script.path.clone()));
    }
    for instance in &document.instances {
        collected.push(instance.prefab.clone());
        for change in &instance.overrides {
            if let Some(mesh) = &change.mesh_renderer {
                collected.extend(mesh.model.clone());
                collected.extend(mesh.texture.clone());
                if let Some(material) = &mesh.material {
                    collected.extend(material_paths(material));
                }
            }
        }
    }
    paths.extend(collected);
    paths
}

fn material_paths(material: &MaterialOverride) -> impl Iterator<Item = VirtualPath> + '_ {
    material
        .metallic_roughness_texture
        .iter()
        .chain(&material.normal_texture)
        .chain(&material.emissive_texture)
        .chain(&material.occlusion_texture)
        .cloned()
}

/// Локальный id корня префаба `path` после его раскрытия.
fn nested_root(loader: &super::PrefabLoader, path: &VirtualPath) -> Result<String, String> {
    let document = loader(path).map_err(|error| error.to_string())?;
    let expansion = engine_scene::expand(&document, &mut |path| loader(path))
        .map_err(|error| error.to_string())?;
    expansion
        .document
        .objects
        .iter()
        .find(|object| object.parent.is_none())
        .map(|object| object.id.clone())
        .ok_or_else(|| format!("у префаба /{} нет корня", path.as_str()))
}

fn promote_root_override(
    instance: &mut PrefabInstance,
    change: &mut PrefabOverride,
    base: &SceneObject,
) {
    if let Some(name) = change.name.take() {
        instance.name = Some(name);
    }
    if let Some(transform) = change.transform.take() {
        let mut value = base.transform;
        value.position = transform.position.unwrap_or(value.position);
        value.rotation = transform.rotation.unwrap_or(value.rotation);
        value.scale = transform.scale.unwrap_or(value.scale);
        instance.transform = Some(value);
    }
}

/// Сливает `outer` поверх `base`: заданные поля внешнего override побеждают.
fn merge_override(base: &mut PrefabOverride, outer: &PrefabOverride) {
    if outer.name.is_some() {
        base.name = outer.name.clone();
    }
    if let Some(transform) = outer.transform {
        let merged = base
            .transform
            .get_or_insert_with(TransformOverride::default);
        merged.position = transform.position.or(merged.position);
        merged.rotation = transform.rotation.or(merged.rotation);
        merged.scale = transform.scale.or(merged.scale);
    }
    if let Some(mesh) = &outer.mesh_renderer {
        let merged = base
            .mesh_renderer
            .get_or_insert_with(MeshRendererOverride::default);
        if mesh.model.is_some() {
            merged.model = mesh.model.clone();
        }
        if mesh.texture.is_some() {
            merged.texture = mesh.texture.clone();
        }
        if let Some(material) = &mesh.material {
            merge_material(
                merged
                    .material
                    .get_or_insert_with(MaterialOverride::default),
                material,
            );
        }
    }
    if outer.camera.is_some() {
        base.camera = outer.camera.clone();
    }
    if outer.directional_light.is_some() {
        base.directional_light = outer.directional_light;
    }
    if outer.point_light.is_some() {
        base.point_light = outer.point_light;
    }
    if outer.spot_light.is_some() {
        base.spot_light = outer.spot_light;
    }
    if outer.environment.is_some() {
        base.environment = outer.environment.clone();
    }
    if outer.collider.is_some() {
        base.collider = outer.collider;
    }
    if outer.rigid_body.is_some() {
        base.rigid_body = outer.rigid_body;
    }
    if outer.character_controller.is_some() {
        base.character_controller = outer.character_controller;
    }
    for params in &outer.lite_params {
        match base
            .lite_params
            .iter_mut()
            .find(|existing| existing.path == params.path)
        {
            Some(existing) => existing.params.extend(params.params.clone()),
            None => base.lite_params.push(params.clone()),
        }
    }
    for params in &outer.heavy_params {
        match base
            .heavy_params
            .iter_mut()
            .find(|existing| existing.name == params.name)
        {
            Some(existing) => existing.params.extend(params.params.clone()),
            None => base.heavy_params.push(params.clone()),
        }
    }
}

fn is_empty(change: &PrefabOverride) -> bool {
    change.name.is_none()
        && change.transform.is_none()
        && change.mesh_renderer.is_none()
        && change.camera.is_none()
        && change.directional_light.is_none()
        && change.point_light.is_none()
        && change.spot_light.is_none()
        && change.environment.is_none()
        && change.collider.is_none()
        && change.rigid_body.is_none()
        && change.character_controller.is_none()
        && change.lite_params.is_empty()
        && change.heavy_params.is_empty()
}

const COMPONENTS: &str = "добавлять и удалять компоненты объекта префаба можно только в самом префабе (Open Prefab) или после Unpack";

/// Разреженный override, превращающий `base` в `edited`. Изменения, которые override не
/// выражает (компоненты, удаление полей и ключей, LOD), — ошибка.
pub(crate) fn object_override(
    base: &SceneObject,
    edited: &SceneObject,
    target: &str,
) -> Result<PrefabOverride, String> {
    let mut change = PrefabOverride {
        target: target.to_owned(),
        ..PrefabOverride::default()
    };
    if edited.name != base.name {
        change.name = Some(edited.name.clone().unwrap_or_default());
    }
    let (from, to) = (base.transform, edited.transform);
    let transform = TransformOverride {
        position: (to.position != from.position).then_some(to.position),
        rotation: (to.rotation != from.rotation).then_some(to.rotation),
        scale: (to.scale != from.scale).then_some(to.scale),
    };
    if transform != TransformOverride::default() {
        change.transform = Some(transform);
    }
    match (&base.mesh_renderer, &edited.mesh_renderer) {
        (None, None) => {}
        (Some(from), Some(to)) => {
            let mut mesh = MeshRendererOverride::default();
            if to.model != from.model {
                mesh.model = Some(to.model.clone());
            }
            if to.texture != from.texture {
                mesh.texture = Some(
                    to.texture
                        .clone()
                        .ok_or("override не может убрать текстуру mesh_renderer")?,
                );
            }
            if to.material != from.material {
                mesh.material = Some(material_diff(
                    from.material
                        .as_ref()
                        .unwrap_or(&MaterialOverride::default()),
                    to.material.as_ref().unwrap_or(&MaterialOverride::default()),
                )?);
            }
            if mesh != MeshRendererOverride::default() {
                change.mesh_renderer = Some(mesh);
            }
        }
        _ => return Err(COMPONENTS.to_owned()),
    }
    if edited.lod_group != base.lod_group {
        return Err("LOD Group объекта префаба меняется только в самом префабе".to_owned());
    }
    match (base.camera.as_ref(), edited.camera.as_ref()) {
        (None, None) => {}
        (Some(from), Some(to)) => change.camera = (to != from).then(|| to.clone()),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (base.directional_light, edited.directional_light) {
        (None, None) => {}
        (Some(from), Some(to)) => change.directional_light = (to != from).then_some(to),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (base.point_light, edited.point_light) {
        (None, None) => {}
        (Some(from), Some(to)) => change.point_light = (to != from).then_some(to),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (base.spot_light, edited.spot_light) {
        (None, None) => {}
        (Some(from), Some(to)) => change.spot_light = (to != from).then_some(to),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (&base.environment, &edited.environment) {
        (None, None) => {}
        (Some(from), Some(to)) => change.environment = (to != from).then(|| to.clone()),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (base.collider, edited.collider) {
        (None, None) => {}
        (Some(from), Some(to)) => change.collider = (to != from).then_some(to),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (base.rigid_body, edited.rigid_body) {
        (None, None) => {}
        (Some(from), Some(to)) => change.rigid_body = (to != from).then_some(to),
        _ => return Err(COMPONENTS.to_owned()),
    }
    match (base.character_controller, edited.character_controller) {
        (None, None) => {}
        (Some(from), Some(to)) => change.character_controller = (to != from).then_some(to),
        _ => return Err(COMPONENTS.to_owned()),
    }
    if base.lite_scripts.len() != edited.lite_scripts.len()
        || base
            .lite_scripts
            .iter()
            .zip(&edited.lite_scripts)
            .any(|(from, to)| from.path != to.path)
    {
        return Err(COMPONENTS.to_owned());
    }
    for (from, to) in base.lite_scripts.iter().zip(&edited.lite_scripts) {
        let params = params_diff(&from.params, &to.params)?;
        if !params.is_empty() {
            change.lite_params.push(LiteParamsOverride {
                path: to.path.clone(),
                params,
            });
        }
    }
    if base.heavy_scripts.len() != edited.heavy_scripts.len()
        || base
            .heavy_scripts
            .iter()
            .zip(&edited.heavy_scripts)
            .any(|(from, to)| from.name != to.name)
    {
        return Err(COMPONENTS.to_owned());
    }
    for (from, to) in base.heavy_scripts.iter().zip(&edited.heavy_scripts) {
        let params = params_diff(&from.params, &to.params)?;
        if !params.is_empty() {
            change.heavy_params.push(HeavyParamsOverride {
                name: to.name.clone(),
                params,
            });
        }
    }
    Ok(change)
}

fn params_diff(
    from: &BTreeMap<String, engine_scene::ParamValue>,
    to: &BTreeMap<String, engine_scene::ParamValue>,
) -> Result<BTreeMap<String, engine_scene::ParamValue>, String> {
    if let Some(removed) = from.keys().find(|key| !to.contains_key(*key)) {
        return Err(format!(
            "override не может удалить параметр {removed:?}, заданный в префабе"
        ));
    }
    Ok(to
        .iter()
        .filter(|(key, value)| from.get(*key) != Some(value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn material_diff(
    from: &MaterialOverride,
    to: &MaterialOverride,
) -> Result<MaterialOverride, String> {
    fn field<T: Clone + PartialEq>(from: &Option<T>, to: &Option<T>) -> Result<Option<T>, String> {
        match (from, to) {
            (_, None) if from.is_some() => {
                Err("override не может убрать поле материала, заданное в префабе".to_owned())
            }
            (from, to) if from == to => Ok(None),
            (_, to) => Ok(to.clone()),
        }
    }
    let alpha_mode = field(&from.alpha_mode, &to.alpha_mode)?;
    let alpha_cutoff = if to
        .alpha_mode
        .is_some_and(|mode| mode != MaterialAlphaMode::Mask)
        && to.alpha_cutoff.is_none()
    {
        None
    } else {
        field(&from.alpha_cutoff, &to.alpha_cutoff)?
    };
    Ok(MaterialOverride {
        base_color: field(&from.base_color, &to.base_color)?,
        metallic: field(&from.metallic, &to.metallic)?,
        roughness: field(&from.roughness, &to.roughness)?,
        emissive: field(&from.emissive, &to.emissive)?,
        opacity: field(&from.opacity, &to.opacity)?,
        alpha_mode,
        alpha_cutoff,
        metallic_roughness_texture: field(
            &from.metallic_roughness_texture,
            &to.metallic_roughness_texture,
        )?,
        normal_texture: field(&from.normal_texture, &to.normal_texture)?,
        emissive_texture: field(&from.emissive_texture, &to.emissive_texture)?,
        occlusion_texture: field(&from.occlusion_texture, &to.occlusion_texture)?,
        normal_scale: field(&from.normal_scale, &to.normal_scale)?,
        occlusion_strength: field(&from.occlusion_strength, &to.occlusion_strength)?,
        double_sided: field(&from.double_sided, &to.double_sided)?,
    })
}

/// Заданные поля `change` заменяют поля `target`.
fn merge_material(target: &mut MaterialOverride, change: &MaterialOverride) {
    target.base_color = change.base_color.or(target.base_color);
    target.metallic = change.metallic.or(target.metallic);
    target.roughness = change.roughness.or(target.roughness);
    target.emissive = change.emissive.or(target.emissive);
    target.opacity = change.opacity.or(target.opacity);
    if change.alpha_mode.is_some() {
        target.alpha_mode = change.alpha_mode;
        if change.alpha_mode != Some(MaterialAlphaMode::Mask) {
            target.alpha_cutoff = None;
        }
    }
    target.alpha_cutoff = change.alpha_cutoff.or(target.alpha_cutoff);
    if change.metallic_roughness_texture.is_some() {
        target.metallic_roughness_texture = change.metallic_roughness_texture.clone();
    }
    if change.normal_texture.is_some() {
        target.normal_texture = change.normal_texture.clone();
    }
    if change.emissive_texture.is_some() {
        target.emissive_texture = change.emissive_texture.clone();
    }
    if change.occlusion_texture.is_some() {
        target.occlusion_texture = change.occlusion_texture.clone();
    }
    target.normal_scale = change.normal_scale.or(target.normal_scale);
    target.occlusion_strength = change.occlusion_strength.or(target.occlusion_strength);
    target.double_sided = change.double_sided.or(target.double_sided);
}

/// Поле объекта экземпляра, которое может быть переопределено.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverrideField {
    Name,
    Transform,
    Mesh,
    Camera,
    Light,
    PointLight,
    SpotLight,
    Environment,
    Collider,
    RigidBody,
    CharacterController,
    LiteParams(VirtualPath),
    HeavyParams(String),
}

impl EditorScene {
    /// Поле объекта экземпляра отличается от префаба. Трансформация корня — его
    /// размещение, поэтому переопределением не считается.
    pub fn is_overridden(&self, id: &str, field: &OverrideField) -> bool {
        if field == &OverrideField::Name
            && self
                .instance(id)
                .is_some_and(|instance| instance.name.is_some())
        {
            return true;
        }
        let Some(change) = self.object_override(id) else {
            return false;
        };
        match field {
            OverrideField::Name => change.name.is_some(),
            OverrideField::Transform => change.transform.is_some(),
            OverrideField::Mesh => change.mesh_renderer.is_some(),
            OverrideField::Camera => change.camera.is_some(),
            OverrideField::Light => change.directional_light.is_some(),
            OverrideField::PointLight => change.point_light.is_some(),
            OverrideField::SpotLight => change.spot_light.is_some(),
            OverrideField::Environment => change.environment.is_some(),
            OverrideField::Collider => change.collider.is_some(),
            OverrideField::RigidBody => change.rigid_body.is_some(),
            OverrideField::CharacterController => change.character_controller.is_some(),
            OverrideField::LiteParams(path) => {
                change.lite_params.iter().any(|params| &params.path == path)
            }
            OverrideField::HeavyParams(name) => change
                .heavy_params
                .iter()
                .any(|params| &params.name == name),
        }
    }

    /// Возвращает одно поле объекта экземпляра к значению префаба.
    pub fn revert_field(&mut self, id: &str, field: &OverrideField) -> Result<(), String> {
        let origin = self
            .origin(id)
            .cloned()
            .ok_or_else(|| format!("{id:?} не является объектом экземпляра"))?;
        self.edit(None, |document| {
            let instance = document
                .instances
                .iter_mut()
                .find(|instance| instance.id == origin.instance)
                .ok_or_else(|| format!("экземпляр {:?} не найден", origin.instance))?;
            if field == &OverrideField::Name && origin.instance == id {
                instance.name = None;
            }
            if let Some(change) = instance
                .overrides
                .iter_mut()
                .find(|change| change.target == origin.target)
            {
                match field {
                    OverrideField::Name => change.name = None,
                    OverrideField::Transform => change.transform = None,
                    OverrideField::Mesh => change.mesh_renderer = None,
                    OverrideField::Camera => change.camera = None,
                    OverrideField::Light => change.directional_light = None,
                    OverrideField::PointLight => change.point_light = None,
                    OverrideField::SpotLight => change.spot_light = None,
                    OverrideField::Environment => change.environment = None,
                    OverrideField::Collider => change.collider = None,
                    OverrideField::RigidBody => change.rigid_body = None,
                    OverrideField::CharacterController => change.character_controller = None,
                    OverrideField::LiteParams(path) => {
                        change.lite_params.retain(|params| &params.path != path)
                    }
                    OverrideField::HeavyParams(name) => {
                        change.heavy_params.retain(|params| &params.name != name)
                    }
                }
            }
            instance.overrides.retain(|change| !is_empty(change));
            Ok(())
        })
    }

    /// Авторский префаб экземпляра объекта `id` с перенесённым в него override этого
    /// объекта. Поля самого экземпляра (имя, трансформация, родитель) не переносятся.
    pub fn prefab_with_override(&self, id: &str) -> Result<(VirtualPath, SceneDocument), String> {
        let origin = self
            .origin(id)
            .cloned()
            .ok_or_else(|| format!("{id:?} не является объектом экземпляра"))?;
        let change = self
            .object_override(id)
            .cloned()
            .ok_or_else(|| format!("у объекта {id:?} нет overrides"))?;
        let path = self
            .instance(&origin.instance)
            .map(|instance| instance.prefab.clone())
            .ok_or_else(|| format!("экземпляр {:?} не найден", origin.instance))?;
        let mut prefab = (self.loader)(&path).map_err(|error| error.to_string())?;
        let (head, rest) = match change.target.split_once('.') {
            Some((head, rest)) => (head, Some(rest)),
            None => (change.target.as_str(), None),
        };
        if let Some(object) = prefab.objects.iter_mut().find(|object| object.id == head) {
            engine_scene::apply_override(object, &change)?;
        } else if let Some(inner) = prefab.instances.iter_mut().find(|inner| inner.id == head) {
            let mut change = change.clone();
            let target = match rest {
                Some(rest) => rest.to_owned(),
                None => {
                    let base = self.base_object(&origin.instance, id)?;
                    promote_root_override(inner, &mut change, &base);
                    nested_root(&self.loader, &inner.prefab)?
                }
            };
            change.target = target;
            match inner
                .overrides
                .iter_mut()
                .find(|existing| existing.target == change.target)
            {
                Some(existing) => merge_override(existing, &change),
                None if !is_empty(&change) => inner.overrides.push(change),
                None => {}
            }
        } else {
            return Err(format!("override target {:?} не найден", change.target));
        }
        for asset in override_paths(&change) {
            if !prefab.assets.iter().any(|existing| existing.path == asset) {
                let archive = self
                    .document
                    .assets
                    .iter()
                    .find(|existing| existing.path == asset)
                    .and_then(|existing| existing.archive.clone());
                prefab.assets.push(engine_scene::SceneAsset {
                    path: asset,
                    archive,
                });
            }
        }
        let label = format!("/{}", path.as_str());
        let text = prefab.to_toml().map_err(|error| error.to_string())?;
        let prefab =
            SceneDocument::parse_prefab(label, &text).map_err(|error| error.to_string())?;
        Ok((path, prefab))
    }

    /// Убирает override объекта после переноса в префаб; поля экземпляра сохраняются.
    /// Файлы префабов перечитываются, поэтому вид объекта не меняется.
    pub fn drop_override(&mut self, id: &str) -> Result<(), String> {
        let origin = self
            .origin(id)
            .cloned()
            .ok_or_else(|| format!("{id:?} не является объектом экземпляра"))?;
        self.reload_prefabs()?;
        self.edit(None, |document| {
            if let Some(instance) = document
                .instances
                .iter_mut()
                .find(|instance| instance.id == origin.instance)
            {
                instance
                    .overrides
                    .retain(|change| change.target != origin.target);
            }
            Ok(())
        })
    }
}

/// Ресурсы, которые вводит override.
fn override_paths(change: &PrefabOverride) -> Vec<VirtualPath> {
    let mut paths = Vec::new();
    if let Some(mesh) = &change.mesh_renderer {
        paths.extend(mesh.model.clone());
        paths.extend(mesh.texture.clone());
        if let Some(material) = &mesh.material {
            paths.extend(material_paths(material));
        }
    }
    if let Some(settings) = change
        .camera
        .as_ref()
        .and_then(|camera| camera.settings.clone())
    {
        paths.push(settings);
    }
    if let Some(hdr) = change
        .environment
        .as_ref()
        .and_then(|environment| environment.hdr.clone())
    {
        paths.push(hdr);
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::super::{Component, ComponentKind, EditorScene, NewObject, PrefabLoader};
    use super::{OverrideField, material_diff, material_paths, merge_material, merge_override};
    use engine_files::VirtualPath;
    use engine_scene::{
        ColliderComponent, MaterialAlphaMode, MaterialOverride, ParamValue, PrefabOverride,
        RigidBodyComponent, RigidBodyKind, SceneDocument, SceneError, SceneTransform, World,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// Файлы префабов в памяти: тест может «записать» новый префаб.
    #[derive(Clone, Default)]
    struct Files(Rc<RefCell<HashMap<String, String>>>);

    impl Files {
        fn with(entries: &[(&str, &str)]) -> Self {
            let files = Self::default();
            for (path, text) in entries {
                files.write(path, text);
            }
            files
        }

        fn write(&self, path: &str, text: &str) {
            self.0
                .borrow_mut()
                .insert((*path).to_owned(), text.to_owned());
        }

        fn loader(&self) -> PrefabLoader {
            let files = self.clone();
            Rc::new(move |path: &VirtualPath| {
                let label = format!("/{}", path.as_str());
                let text = files
                    .0
                    .borrow()
                    .get(path.as_str())
                    .cloned()
                    .ok_or_else(|| SceneError {
                        source: label.clone(),
                        object_id: None,
                        message: "не найден".to_owned(),
                    })?;
                SceneDocument::parse_prefab(label, &text)
            })
        }
    }

    const DOOR: &str = "[[assets]]\npath = \"/models/door.fbx\"\n[[objects]]\nid = \"frame\"\n[objects.mesh_renderer]\nmodel = \"/models/door.fbx\"\n[[objects]]\nid = \"handle\"\nparent = \"frame\"\n[objects.transform]\nposition = [0.5, 1.0, 0.0]\n[objects.mesh_renderer]\nmodel = \"/models/door.fbx\"\n";

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    fn scene_with(files: &Files) -> EditorScene {
        let mut scene = EditorScene::new_untitled().unwrap();
        scene.set_prefab_loader(files.loader()).unwrap();
        scene
    }

    fn door_scene() -> (Files, EditorScene, String) {
        let files = Files::with(&[("prefabs/door.rveprefab", DOOR)]);
        let mut scene = scene_with(&files);
        let id = scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/door.rveprefab"), [2.0, 0.0, 0.0]),
                None,
            )
            .unwrap();
        (files, scene, id)
    }

    #[test]
    fn material_diff_keeps_stage_one_fields_and_texture_dependencies() {
        let before = MaterialOverride::default();
        let after = MaterialOverride {
            emissive: Some([2.0, 0.5, 1.0]),
            opacity: Some(0.65),
            alpha_mode: Some(MaterialAlphaMode::Mask),
            alpha_cutoff: Some(0.4),
            emissive_texture: Some(path("/textures/emissive.png")),
            occlusion_texture: Some(path("/textures/occlusion.png")),
            occlusion_strength: Some(0.7),
            ..MaterialOverride::default()
        };
        let difference = material_diff(&before, &after).unwrap();
        assert_eq!(difference, after);
        assert_eq!(
            material_paths(&difference)
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
            ["textures/emissive.png", "textures/occlusion.png"]
        );
    }

    #[test]
    fn material_diff_clears_mask_cutoff_when_mode_changes() {
        let before = MaterialOverride {
            alpha_mode: Some(MaterialAlphaMode::Mask),
            alpha_cutoff: Some(0.4),
            ..MaterialOverride::default()
        };
        let after = MaterialOverride {
            alpha_mode: Some(MaterialAlphaMode::Blend),
            ..MaterialOverride::default()
        };
        let difference = material_diff(&before, &after).unwrap();
        let mut merged = before;
        merge_material(&mut merged, &difference);
        assert_eq!(merged, after);
    }

    fn nested_door_scene() -> (Files, EditorScene) {
        let door = "[[assets]]\npath = \"/prefabs/lock.rveprefab\"\n[[objects]]\nid = \"frame\"\n[[instances]]\nid = \"lock\"\nparent = \"frame\"\nprefab = \"/prefabs/lock.rveprefab\"\nname = \"Nested\"\n[instances.transform]\nposition = [2.0, 0.0, 0.0]\nrotation = [0.0, 30.0, 0.0]\nscale = [2.0, 2.0, 2.0]\n";
        let lock = "[[objects]]\nid = \"body\"\nname = \"Body\"\n[objects.transform]\nposition = [0.0, 4.0, 0.0]\n";
        let files = Files::with(&[
            ("prefabs/door.rveprefab", door),
            ("prefabs/lock.rveprefab", lock),
        ]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/door.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();
        (files, scene)
    }

    fn edit_nested_root(scene: &mut EditorScene) {
        scene
            .edit_object("door.lock", None, |object| {
                object.name = Some("Applied".to_owned());
                object.transform.position = [7.0, 3.0, 1.0];
                Ok(())
            })
            .unwrap();
    }

    fn world_position(scene: &EditorScene, id: &str) -> [f32; 3] {
        let world = World::from_document(scene.view()).unwrap();
        let handle = world.handle(id).unwrap();
        let matrix = world.world_matrix(handle).unwrap();
        matrix.w_axis.truncate().to_array()
    }

    #[test]
    fn instantiating_a_prefab_shows_its_objects_and_is_one_undo_step() {
        let (_, mut scene, id) = door_scene();
        assert_eq!(id, "door");
        assert_eq!(scene.selection(), Some("door"));
        assert!(scene.object("door.handle").is_some());
        assert_eq!(world_position(&scene, "door.handle"), [2.5, 1.0, 0.0]);
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path.as_str() == "prefabs/door.rveprefab")
        );
        assert!(scene.is_prefab_part("door.handle"));
        assert!(!scene.is_prefab_part("door"));
        assert!(scene.undo());
        assert!(scene.object("door.handle").is_none());
        assert!(scene.redo());
        assert!(scene.object("door.handle").is_some());
    }

    #[test]
    fn camera_settings_and_active_camera_edits_write_prefab_overrides() {
        let camera_prefab = "[[assets]]\npath = \"/cameras/default.camerasettings\"\n[[objects]]\nid = \"eye\"\n[objects.camera]\nsettings = \"/cameras/default.camerasettings\"\n";
        let files = Files::with(&[("prefabs/camera.rveprefab", camera_prefab)]);
        let mut scene = scene_with(&files);
        scene.set_camera_active("camera", true).unwrap();
        let first = scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/camera.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();
        let second = scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/camera.rveprefab"), [1.0, 0.0, 0.0]),
                None,
            )
            .unwrap();
        let initial_settings = path("/cameras/custom.camerasettings");
        scene
            .set_camera_settings(&first, Some(initial_settings))
            .unwrap();
        let settings = path("/cameras/copied.camerasettings");
        let mut camera = scene.object(&first).unwrap().camera.clone().unwrap();
        camera.settings = Some(settings.clone());
        scene
            .set_component(&first, Component::Camera(camera))
            .unwrap();
        scene.set_camera_active(&second, true).unwrap();

        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path == settings)
        );
        assert_eq!(
            scene
                .object(&first)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .settings,
            Some(settings)
        );
        assert!(
            !scene
                .object("camera")
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        assert!(
            !scene
                .object(&first)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        assert!(
            scene
                .object(&second)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        let duplicate = scene.duplicate_object(&second).unwrap();
        assert!(
            !scene
                .object(&duplicate)
                .unwrap()
                .camera
                .as_ref()
                .unwrap()
                .active
        );
        World::from_document(scene.view()).unwrap();
    }

    #[test]
    fn objects_inside_an_instance_cannot_be_reparented_deleted_or_renamed() {
        let (_, mut scene, _) = door_scene();
        assert!(scene.set_parent("door.handle", None).is_err());
        assert!(scene.delete_object("door.handle").is_err());
        assert!(scene.change_id("door.handle", "x").is_err());
        assert!(scene.duplicate_object("door.handle").is_err());
        // Корень экземпляра переносится как обычный объект.
        let parent = scene.create_object(&NewObject::Empty, None).unwrap();
        scene.set_parent("door", Some(&parent)).unwrap();
        assert_eq!(
            scene.instance("door").unwrap().parent.as_deref(),
            Some(parent.as_str())
        );
    }

    #[test]
    fn deleting_and_duplicating_an_instance_keeps_references_inside_it() {
        let (_, mut scene, _) = door_scene();
        let sign = scene
            .create_object(&NewObject::Empty, Some("door.handle"))
            .unwrap();
        let copy = scene.duplicate_object("door").unwrap();
        assert_eq!(copy, "door-1");
        let copied_sign =
            scene.document().objects.iter().find(|object| {
                object.id != sign && object.parent.as_deref() == Some("door-1.handle")
            });
        assert!(copied_sign.is_some());
        scene.change_id("door", "front").unwrap();
        assert_eq!(
            scene.object(&sign).unwrap().parent.as_deref(),
            Some("front.handle")
        );
        scene.delete_object("front").unwrap();
        assert!(scene.object(&sign).is_none());
        assert!(scene.instance("front").is_none());
    }

    #[test]
    fn editing_an_instance_object_writes_a_sparse_override() {
        let (_, mut scene, _) = door_scene();
        let mut transform = scene.object("door.handle").unwrap().transform;
        transform.position[1] = 3.0;
        scene.set_transform("door.handle", transform, None).unwrap();
        let change = scene.object_override("door.handle").unwrap();
        assert_eq!(change.target, "handle");
        let moved = change.transform.unwrap();
        assert_eq!(moved.position, Some([0.5, 3.0, 0.0]));
        assert_eq!((moved.rotation, moved.scale), (None, None));
        assert!(change.mesh_renderer.is_none());
        assert_eq!(
            scene.object("door.handle").unwrap().transform.position,
            [0.5, 3.0, 0.0]
        );
        assert!(scene.is_overridden("door.handle", &OverrideField::Transform));

        // Повторная правка пересчитывает override, а не накапливает его.
        transform.position[1] = 1.0;
        scene.set_transform("door.handle", transform, None).unwrap();
        assert!(scene.object_override("door.handle").is_none());
    }

    #[test]
    fn environment_prefab_override_replaces_the_component_and_registers_hdr_asset() {
        let prefab = "[[assets]]\npath = \"/environment/original.hdr\"\n[[objects]]\nid = \"sky\"\n[objects.environment]\nhdr = \"/environment/original.hdr\"\n";
        let files = Files::with(&[("prefabs/sky.rveprefab", prefab)]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/sky.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();

        scene
            .edit_object("sky", None, |object| {
                object.environment.as_mut().unwrap().hdr = Some(path("/environment/changed.hdr"));
                Ok(())
            })
            .unwrap();
        let change = scene.object_override("sky").unwrap();
        assert_eq!(
            change.environment.as_ref().unwrap().hdr,
            Some(path("/environment/changed.hdr"))
        );
        assert!(scene.is_overridden("sky", &OverrideField::Environment));
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path == path("/environment/changed.hdr"))
        );
        let (_, applied) = scene.prefab_with_override("sky").unwrap();
        assert_eq!(
            applied.objects[0].environment.as_ref().unwrap().hdr,
            Some(path("/environment/changed.hdr"))
        );

        scene
            .revert_field("sky", &OverrideField::Environment)
            .unwrap();
        assert_eq!(
            scene
                .object("sky")
                .unwrap()
                .environment
                .as_ref()
                .unwrap()
                .hdr,
            Some(path("/environment/original.hdr"))
        );
    }

    #[test]
    fn local_light_prefab_overrides_replace_whole_components_and_revert() {
        let prefab = "[[objects]]\nid = \"lamp\"\n[objects.point_light]\ncolor = [0.8, 0.6, 0.4]\nintensity = 1.0\nrange = 5.0\n[objects.spot_light]\ncolor = [0.4, 0.6, 0.8]\nintensity = 2.0\nrange = 7.0\ninner_angle_deg = 10.0\nouter_angle_deg = 20.0\n";
        let files = Files::with(&[("prefabs/lamp.rveprefab", prefab)]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/lamp.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();

        scene
            .edit_object("lamp", None, |object| {
                let point = object.point_light.as_mut().unwrap();
                point.color = [1.5, 0.25, 0.1];
                point.intensity = 4.0;
                point.range = 12.0;
                point.shadows = true;
                let spot = object.spot_light.as_mut().unwrap();
                spot.intensity = 3.5;
                spot.range = 15.0;
                spot.inner_angle_deg = 14.0;
                spot.outer_angle_deg = 32.0;
                Ok(())
            })
            .unwrap();
        let change = scene.object_override("lamp").unwrap();
        assert_eq!(
            change.point_light,
            scene.object("lamp").unwrap().point_light
        );
        assert_eq!(change.spot_light, scene.object("lamp").unwrap().spot_light);
        assert!(scene.is_overridden("lamp", &OverrideField::PointLight));
        assert!(scene.is_overridden("lamp", &OverrideField::SpotLight));
        let (_, applied) = scene.prefab_with_override("lamp").unwrap();
        assert_eq!(applied.objects[0].point_light, change.point_light);
        assert_eq!(applied.objects[0].spot_light, change.spot_light);

        scene
            .revert_field("lamp", &OverrideField::PointLight)
            .unwrap();
        scene
            .revert_field("lamp", &OverrideField::SpotLight)
            .unwrap();
        assert!(!scene.is_overridden("lamp", &OverrideField::PointLight));
        assert!(!scene.is_overridden("lamp", &OverrideField::SpotLight));
        assert_eq!(
            scene.object("lamp").unwrap().point_light.unwrap().range,
            5.0
        );
        assert_eq!(
            scene
                .object("lamp")
                .unwrap()
                .spot_light
                .unwrap()
                .outer_angle_deg,
            20.0
        );
    }

    #[test]
    fn physics_prefab_overrides_replace_whole_components_and_keep_each_other() {
        let prefab = "[[objects]]\nid = \"crate\"\n[objects.collider]\nhalf_extents = [0.5, 0.5, 0.5]\n[objects.rigid_body]\nkind = \"dynamic\"\nmass = 2.0\n";
        let files = Files::with(&[("prefabs/crate.rveprefab", prefab)]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/crate.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();

        scene
            .edit_object("crate", None, |object| {
                object.collider.as_mut().unwrap().friction = 0.9;
                object.rigid_body.as_mut().unwrap().mass = 5.0;
                Ok(())
            })
            .unwrap();
        let change = scene.object_override("crate").unwrap();
        assert_eq!(change.collider, scene.object("crate").unwrap().collider);
        assert_eq!(change.rigid_body, scene.object("crate").unwrap().rigid_body);
        assert!(scene.is_overridden("crate", &OverrideField::Collider));
        assert!(scene.is_overridden("crate", &OverrideField::RigidBody));
        let (_, applied) = scene.prefab_with_override("crate").unwrap();
        assert_eq!(applied.objects[0].collider, change.collider);
        assert_eq!(applied.objects[0].rigid_body, change.rigid_body);

        // Откат одного компонента не должен стереть override второго.
        scene
            .revert_field("crate", &OverrideField::RigidBody)
            .unwrap();
        assert!(!scene.is_overridden("crate", &OverrideField::RigidBody));
        assert!(scene.is_overridden("crate", &OverrideField::Collider));
        assert_eq!(scene.object("crate").unwrap().rigid_body.unwrap().mass, 2.0);
        scene
            .revert_field("crate", &OverrideField::Collider)
            .unwrap();
        assert_eq!(
            scene.object("crate").unwrap().collider.unwrap().friction,
            0.5
        );
        assert!(scene.object_override("crate").is_none());
    }

    #[test]
    fn adding_or_removing_physics_components_of_instance_objects_is_rejected() {
        let prefab = "[[objects]]\nid = \"crate\"\n[objects.collider]\n";
        let files = Files::with(&[("prefabs/crate.rveprefab", prefab)]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/crate.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();
        assert!(
            scene
                .remove_physics_component("crate", ComponentKind::Collider)
                .is_err()
        );
        assert!(
            scene
                .set_component("crate", Component::RigidBody(RigidBodyComponent::default()))
                .is_err()
        );
        assert!(scene.object("crate").unwrap().rigid_body.is_none());
    }

    #[test]
    fn nested_merge_keeps_physics_overrides_from_both_levels() {
        let mut base = PrefabOverride {
            target: "crate".to_owned(),
            collider: Some(ColliderComponent {
                friction: 0.2,
                ..ColliderComponent::default()
            }),
            ..PrefabOverride::default()
        };
        let outer = PrefabOverride {
            target: "crate".to_owned(),
            rigid_body: Some(RigidBodyComponent {
                kind: RigidBodyKind::Kinematic,
                ..RigidBodyComponent::default()
            }),
            ..PrefabOverride::default()
        };
        merge_override(&mut base, &outer);
        assert_eq!(base.collider.unwrap().friction, 0.2);
        assert_eq!(base.rigid_body.unwrap().kind, RigidBodyKind::Kinematic);

        let replacement = PrefabOverride {
            target: "crate".to_owned(),
            collider: Some(ColliderComponent {
                friction: 0.8,
                ..ColliderComponent::default()
            }),
            ..PrefabOverride::default()
        };
        merge_override(&mut base, &replacement);
        assert_eq!(base.collider.unwrap().friction, 0.8);
    }

    #[test]
    fn root_name_and_transform_go_to_the_instance_and_revert_works() {
        let (_, mut scene, _) = door_scene();
        scene
            .edit_object("door", None, |object| {
                object.name = Some("Front door".to_owned());
                object.transform.position = [4.0, 0.0, 0.0];
                Ok(())
            })
            .unwrap();
        let instance = scene.instance("door").unwrap();
        assert_eq!(instance.name.as_deref(), Some("Front door"));
        assert_eq!(instance.transform.unwrap().position, [4.0, 0.0, 0.0]);
        assert!(instance.overrides.is_empty());
        scene.revert_field("door", &OverrideField::Name).unwrap();
        assert_eq!(scene.instance("door").unwrap().name, None);

        scene
            .edit_object("door.handle", None, |object| {
                object.name = Some("Knob".to_owned());
                Ok(())
            })
            .unwrap();
        assert!(scene.is_overridden("door.handle", &OverrideField::Name));
        scene
            .revert_field("door.handle", &OverrideField::Name)
            .unwrap();
        assert!(scene.object_override("door.handle").is_none());
        assert_eq!(scene.object("door.handle").unwrap().name, None);
    }

    #[test]
    fn clearing_prefab_names_uses_the_explicit_empty_name_override() {
        let prefab = "[[objects]]\nid = \"frame\"\nname = \"Door\"\n[[objects]]\nid = \"handle\"\nparent = \"frame\"\nname = \"Handle\"\n";
        let files = Files::with(&[("prefabs/door.rveprefab", prefab)]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/door.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();

        scene
            .edit_object("door.handle", None, |object| {
                object.name = None;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            scene
                .object_override("door.handle")
                .unwrap()
                .name
                .as_deref(),
            Some("")
        );
        assert_eq!(scene.object("door.handle").unwrap().name, None);
        let (_, applied) = scene.prefab_with_override("door.handle").unwrap();
        assert_eq!(
            applied
                .objects
                .iter()
                .find(|object| object.id == "handle")
                .unwrap()
                .name,
            None
        );

        scene
            .edit_object("door", None, |object| {
                object.name = None;
                Ok(())
            })
            .unwrap();
        assert_eq!(scene.instance("door").unwrap().name.as_deref(), Some(""));
        assert_eq!(scene.object("door").unwrap().name, None);

        scene.unpack("door").unwrap();
        assert_eq!(scene.object("door").unwrap().name, None);
        assert_eq!(scene.object("door-handle").unwrap().name, None);
    }

    #[test]
    fn schema_default_is_kept_as_an_override_when_prefab_defines_the_parameter() {
        let prefab = "[[assets]]\npath = \"/scripts/params.lua\"\n[[objects]]\nid = \"frame\"\n[[objects.lite_scripts]]\npath = \"/scripts/params.lua\"\nparams = { speed = 12 }\n";
        let files = Files::with(&[("prefabs/params.rveprefab", prefab)]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/params.rveprefab"), [0.0; 3]),
                None,
            )
            .unwrap();

        scene
            .edit_object("params", None, |object| {
                object.lite_scripts[0]
                    .params
                    .insert("speed".to_owned(), ParamValue::Int(30));
                Ok(())
            })
            .unwrap();

        let change = scene.object_override("params").unwrap();
        assert_eq!(change.lite_params[0].params["speed"], ParamValue::Int(30));
        assert_eq!(
            scene.object("params").unwrap().lite_scripts[0].params["speed"],
            ParamValue::Int(30)
        );
    }

    #[test]
    fn revert_all_keeps_root_instance_placement_transform() {
        let (_, mut scene, _) = door_scene();
        scene
            .set_mesh("door", path("/models/gold.fbx"), None)
            .unwrap();
        scene
            .edit_object("door", None, |object| {
                object.name = Some("Front door".to_owned());
                Ok(())
            })
            .unwrap();

        scene.revert_override("door").unwrap();

        let instance = scene.instance("door").unwrap();
        assert_eq!(instance.name, None);
        assert_eq!(instance.transform.unwrap().position, [2.0, 0.0, 0.0]);
        assert!(instance.overrides.is_empty());
        assert_eq!(scene.object("door").unwrap().name, None);
        assert_eq!(
            scene
                .object("door")
                .unwrap()
                .mesh_renderer
                .as_ref()
                .unwrap()
                .model,
            path("/models/door.fbx")
        );
    }

    #[test]
    fn applying_nested_root_name_and_transform_promotes_instance_fields() {
        let (files, mut scene) = nested_door_scene();
        edit_nested_root(&mut scene);

        let (path, prefab) = scene.prefab_with_override("door.lock").unwrap();
        let nested = prefab
            .instances
            .iter()
            .find(|instance| instance.id == "lock")
            .unwrap();
        assert_eq!(nested.name.as_deref(), Some("Applied"));
        assert_eq!(
            nested.transform.unwrap(),
            SceneTransform {
                position: [7.0, 3.0, 1.0],
                rotation: [0.0, 30.0, 0.0],
                scale: [2.0, 2.0, 2.0],
            }
        );
        assert!(nested.overrides.is_empty());

        files.write(path.as_str(), &prefab.to_toml().unwrap());
        scene.drop_override("door.lock").unwrap();
        assert_eq!(
            scene.object("door.lock").unwrap().name.as_deref(),
            Some("Applied")
        );
        assert_eq!(
            scene.object("door.lock").unwrap().transform.position,
            [7.0, 3.0, 1.0]
        );
    }

    #[test]
    fn unpack_promotes_nested_root_name_and_transform_to_instance_fields() {
        let (_, mut scene) = nested_door_scene();
        edit_nested_root(&mut scene);

        scene.unpack("door").unwrap();

        let nested = scene.instance("door-lock").unwrap();
        assert_eq!(nested.name.as_deref(), Some("Applied"));
        assert_eq!(
            nested.transform.unwrap(),
            SceneTransform {
                position: [7.0, 3.0, 1.0],
                rotation: [0.0, 30.0, 0.0],
                scale: [2.0, 2.0, 2.0],
            }
        );
        assert!(nested.overrides.is_empty());
        assert_eq!(
            scene.object("door-lock").unwrap().name.as_deref(),
            Some("Applied")
        );
        assert_eq!(
            scene.object("door-lock").unwrap().transform.position,
            [7.0, 3.0, 1.0]
        );
    }

    #[test]
    fn adding_or_removing_components_of_instance_objects_is_rejected() {
        let (_, mut scene, _) = door_scene();
        let added = scene.edit_object("door.handle", None, |object| {
            object.camera = Some(engine_scene::CameraComponent {
                settings: None,
                active: false,
                cursor: Default::default(),
            });
            Ok(())
        });
        assert!(added.unwrap_err().contains("Open Prefab"));
        assert!(scene.remove_mesh_renderer("door.handle").is_err());
        assert!(scene.object_override("door.handle").is_none());
    }

    #[test]
    fn changing_a_model_of_an_instance_object_declares_the_asset() {
        let (_, mut scene, _) = door_scene();
        scene
            .set_mesh("door.handle", path("/models/gold.fbx"), None)
            .unwrap();
        let change = scene.object_override("door.handle").unwrap();
        assert_eq!(
            change
                .mesh_renderer
                .as_ref()
                .unwrap()
                .model
                .as_ref()
                .unwrap()
                .as_str(),
            "models/gold.fbx"
        );
        assert!(
            scene
                .document()
                .assets
                .iter()
                .any(|asset| asset.path.as_str() == "models/gold.fbx")
        );
    }

    #[test]
    fn create_prefab_replaces_the_subtree_and_keeps_world_positions() {
        let files = Files::default();
        let mut scene = scene_with(&files);
        let model = path("/models/box.fbx");
        let root = scene
            .create_object(&NewObject::Mesh(model.clone(), [3.0, 0.0, 1.0]), None)
            .unwrap();
        let child = scene
            .create_object(&NewObject::Mesh(model, [0.0, 2.0, 0.0]), Some(&root))
            .unwrap();
        let leaf = scene
            .create_object(&NewObject::Empty, Some(&child))
            .unwrap();
        let before = world_position(&scene, &child);

        let prefab = scene
            .prefab_from_subtree(&root, "/prefabs/box.rveprefab")
            .unwrap();
        assert!(prefab.is_prefab());
        let root_object = prefab
            .objects
            .iter()
            .find(|object| object.id == root)
            .unwrap();
        assert_eq!(root_object.transform, SceneTransform::default());
        assert_eq!(root_object.parent, None);
        assert!(
            prefab
                .assets
                .iter()
                .all(|asset| asset.path.as_str() == "models/box.fbx")
        );
        assert!(prefab.objects.iter().any(|object| object.id == leaf));

        files.write("prefabs/box.rveprefab", &prefab.to_toml().unwrap());
        scene
            .replace_with_instance(&root, &path("/prefabs/box.rveprefab"))
            .unwrap();
        assert!(scene.instance(&root).is_some());
        assert!(
            scene
                .document()
                .objects
                .iter()
                .all(|object| object.id != child)
        );
        assert_eq!(world_position(&scene, &format!("{root}.{child}")), before);
        assert!(scene.undo());
        assert!(scene.instance(&root).is_none());
        assert_eq!(world_position(&scene, &child), before);
    }

    #[test]
    fn references_from_outside_follow_the_new_instance() {
        let files = Files::default();
        let mut scene = scene_with(&files);
        let root = scene.create_object(&NewObject::Empty, None).unwrap();
        let child = scene.create_object(&NewObject::Empty, Some(&root)).unwrap();
        let prefab = scene
            .prefab_from_subtree(&child, "/prefabs/part.rveprefab")
            .unwrap();
        files.write("prefabs/part.rveprefab", &prefab.to_toml().unwrap());
        let grandchild = scene
            .create_object(&NewObject::Empty, Some(&child))
            .unwrap();
        // Внук создан после префаба: в файле его нет, но ссылка на родителя корректна.
        scene
            .edit_object(&grandchild, None, |object| {
                object.parent = Some(root.clone());
                Ok(())
            })
            .unwrap();
        scene
            .replace_with_instance(&child, &path("/prefabs/part.rveprefab"))
            .unwrap();
        assert_eq!(
            scene.instance(&child).unwrap().parent.as_deref(),
            Some(root.as_str())
        );
    }

    #[test]
    fn unpack_turns_an_instance_into_own_objects_with_overrides_applied() {
        let lock = "[[objects]]\nid = \"body\"\n[[objects]]\nid = \"bolt\"\nparent = \"body\"\n";
        let door = "[[assets]]\npath = \"/prefabs/lock.rveprefab\"\n[[objects]]\nid = \"frame\"\n[[objects]]\nid = \"handle\"\nparent = \"frame\"\n[[instances]]\nid = \"lock\"\nparent = \"frame\"\nprefab = \"/prefabs/lock.rveprefab\"\n";
        let files = Files::with(&[
            ("prefabs/door.rveprefab", door),
            ("prefabs/lock.rveprefab", lock),
        ]);
        let mut scene = scene_with(&files);
        scene
            .create_object(
                &NewObject::Prefab(path("/prefabs/door.rveprefab"), [1.0, 0.0, 0.0]),
                None,
            )
            .unwrap();
        for (id, name) in [
            ("door.handle", "Knob"),
            ("door.lock.bolt", "Bolt"),
            ("door.lock", "Lock"),
        ] {
            scene
                .edit_object(id, None, |object| {
                    object.name = Some(name.to_owned());
                    Ok(())
                })
                .unwrap();
        }
        let sign = scene
            .create_object(&NewObject::Empty, Some("door.lock.bolt"))
            .unwrap();
        let before = world_position(&scene, "door.lock.bolt");

        scene.unpack("door").unwrap();
        assert!(scene.instance("door").is_none());
        assert!(
            scene
                .document()
                .objects
                .iter()
                .any(|object| object.id == "door")
        );
        assert_eq!(
            scene.object("door-handle").unwrap().name.as_deref(),
            Some("Knob")
        );
        let lock = scene.instance("door-lock").unwrap();
        assert_eq!(lock.parent.as_deref(), Some("door"));
        assert_eq!(
            scene.object("door-lock.bolt").unwrap().name.as_deref(),
            Some("Bolt")
        );
        assert_eq!(
            scene.object("door-lock").unwrap().name.as_deref(),
            Some("Lock")
        );
        assert_eq!(
            scene.object(&sign).unwrap().parent.as_deref(),
            Some("door-lock.bolt")
        );
        assert_eq!(world_position(&scene, "door-lock.bolt"), before);
        assert!(scene.undo());
        assert!(scene.instance("door").is_some());
    }

    #[test]
    fn apply_to_prefab_moves_the_override_into_the_prefab_file() {
        let (files, mut scene, _) = door_scene();
        scene
            .edit_object("door.handle", None, |object| {
                object.name = Some("Knob".to_owned());
                Ok(())
            })
            .unwrap();
        let (prefab_path, prefab) = scene.prefab_with_override("door.handle").unwrap();
        assert_eq!(prefab_path.as_str(), "prefabs/door.rveprefab");
        let handle = prefab
            .objects
            .iter()
            .find(|object| object.id == "handle")
            .unwrap();
        assert_eq!(handle.name.as_deref(), Some("Knob"));
        files.write(prefab_path.as_str(), &prefab.to_toml().unwrap());
        scene.drop_override("door.handle").unwrap();
        assert!(scene.object_override("door.handle").is_none());
        assert_eq!(
            scene.object("door.handle").unwrap().name.as_deref(),
            Some("Knob")
        );
    }

    #[test]
    fn a_prefab_opens_saves_and_rejects_including_itself() {
        let files = Files::with(&[("prefabs/door.rveprefab", DOOR)]);
        let label = path("/prefabs/door.rveprefab");
        let mut prefab = EditorScene::parse(label.clone(), DOOR, files.loader()).unwrap();
        assert!(prefab.document().is_prefab());
        // Нет камеры, но документ префаба валиден.
        assert!(
            prefab
                .view()
                .objects
                .iter()
                .all(|object| object.camera.is_none())
        );
        let error = prefab
            .create_object(&NewObject::Prefab(label.clone(), [0.0; 3]), Some("frame"))
            .unwrap_err();
        assert!(error.contains("цикл"), "{error}");

        let dir = std::env::temp_dir().join(format!("rve-editor-prefab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("door.rveprefab");
        prefab
            .edit_object("handle", None, |object| {
                object.name = Some("Knob".to_owned());
                Ok(())
            })
            .unwrap();
        prefab.save(&file, label.clone()).unwrap();
        let saved = std::fs::read_to_string(&file).unwrap();
        assert!(SceneDocument::parse_prefab("door", &saved).is_ok());
        // Сцену нельзя сохранить в .rveprefab и наоборот.
        assert!(
            prefab
                .save(&dir.join("door.rvescene"), path("/door.rvescene"))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
