use crate::pipeline::GpuSceneTable;
use engine_assets::{
    Assets, Model, ModelAlphaMode, ModelAnimation, ModelMaterial, ModelSkin, TextureData,
};
use engine_files::VirtualPath;
use engine_physics::{
    ModelGeometry, ModelMesh as PhysicsModelMesh, PhysicsSceneBinding, PhysicsSceneDesc, StepDriver,
};
use engine_project::PhysicsSettings;
use engine_render::{
    AlphaMode, EnvironmentSource, Material, MaterialId, RenderError, Renderer, SkinVertex,
    TextureColorSpace, TextureId, Vertex,
};
use engine_resources::ResourceHandle;
use engine_scene::CameraSettings;
use engine_scene::{
    MaterialAlphaMode, MaterialOverride, ObjectHandle, SceneDocument, SceneError, World, expand,
};
use glam::Vec3;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

pub(crate) struct ObjectModel {
    pub object: ObjectHandle,
    pub levels: Vec<(String, Option<String>)>,
    pub material: Option<MaterialOverride>,
    /// Объект имеет `animator`: его модель получает скиннинг, если у модели есть скелет.
    pub animated: bool,
}

pub(crate) struct PreparedRenderAssets {
    pub models: HashMap<String, Model>,
    pub textures: HashMap<String, TextureData>,
    pub environments: HashMap<String, Arc<EnvironmentSource>>,
    /// Клипы из `animator.clips`, по виртуальному пути файла.
    pub clips: HashMap<String, Arc<Vec<ModelAnimation>>>,
    pub objects: Vec<ObjectModel>,
}

pub(crate) struct PreparedScene {
    pub world: World,
    pub name: String,
    pub sources: BTreeMap<String, String>,
    pub render_assets: PreparedRenderAssets,
}

impl PreparedScene {
    pub(crate) fn build_physics(
        &self,
        settings: &PhysicsSettings,
    ) -> Result<(StepDriver, Vec<PhysicsSceneBinding>), String> {
        let model_geometry = self
            .render_assets
            .models
            .iter()
            .map(|(path, model)| ModelGeometry {
                path: path.clone(),
                meshes: model
                    .meshes
                    .iter()
                    .map(|mesh| PhysicsModelMesh {
                        positions: mesh
                            .vertices
                            .iter()
                            .map(|vertex| Vec3::from_array(vertex.position))
                            .collect(),
                        indices: mesh.indices.clone(),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        PhysicsSceneDesc::from_world(
            &self.world,
            &model_geometry,
            Vec3::from_array(settings.gravity),
            settings.fixed_hz,
            settings.max_substeps as usize,
        )
        .and_then(|desc| desc.build())
        .map_err(|error| error.to_string())
    }
}

pub(crate) fn load_scene(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
) -> Result<PreparedScene, String> {
    let contents = read_utf8(resources, scene_path)?;
    let document =
        SceneDocument::parse(scene_path, &contents).map_err(|error| error.to_string())?;
    let expansion = expand(&document, &mut |path| load_prefab(resources, path))
        .map_err(|error| error.to_string())?;
    let world = World::from_document(&expansion.document).map_err(|error| error.to_string())?;
    prepare_scene(resources, assets, scene_path, world)
}

/// Префаб читается через тот же `ResourceHandle`, что и сцена, поэтому dev и release
/// загружают его одинаково.
fn load_prefab(
    resources: &ResourceHandle,
    path: &VirtualPath,
) -> Result<SceneDocument, SceneError> {
    let label = format!("/{}", path.as_str());
    let text = read_utf8(resources, &label).map_err(|message| SceneError {
        source: label.clone(),
        object_id: None,
        message,
    })?;
    SceneDocument::parse_prefab(label, &text)
}

fn prepare_scene(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
    world: World,
) -> Result<PreparedScene, String> {
    prepare_scene_with_model_loader(resources, assets, scene_path, world, |path| {
        assets.load_model(path).map_err(|error| error.to_string())
    })
}

fn prepare_scene_with_model_loader(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
    mut world: World,
    mut load_model: impl FnMut(&str) -> Result<Model, String>,
) -> Result<PreparedScene, String> {
    let camera_settings = world
        .objects()
        .iter()
        .filter_map(|object| {
            Some((
                object.handle,
                object
                    .camera
                    .as_ref()?
                    .settings
                    .as_ref()?
                    .as_str()
                    .to_owned(),
            ))
        })
        .collect::<Vec<_>>();
    let mut loaded_settings = HashMap::<String, CameraSettings>::new();
    for (_, path) in &camera_settings {
        if !loaded_settings.contains_key(path) {
            let source = read_utf8(resources, path)?;
            let settings =
                CameraSettings::parse(path, &source).map_err(|error| error.to_string())?;
            loaded_settings.insert(path.clone(), settings);
        }
    }
    for (handle, path) in camera_settings {
        world
            .set_camera_settings(handle, loaded_settings[&path])
            .map_err(|error| error.to_string())?;
    }

    let mut models = HashMap::new();
    let mut textures = HashMap::new();
    let mut environments = HashMap::new();
    let mut clips = HashMap::<String, Arc<Vec<ModelAnimation>>>::new();
    let mut objects = Vec::new();
    let mut lite_paths = std::collections::BTreeSet::new();

    for object in world.objects() {
        if let Some(path) = object
            .environment
            .as_ref()
            .and_then(|environment| environment.hdr.as_ref())
        {
            let key = path.as_str().to_owned();
            if let std::collections::hash_map::Entry::Vacant(entry) = environments.entry(key) {
                let image = assets
                    .load_hdr(entry.key())
                    .map_err(|error| format!("{scene_path}: {error}"))?;
                entry.insert(Arc::new(EnvironmentSource::Equirect {
                    width: image.width,
                    height: image.height,
                    rgb: image.rgb,
                }));
            }
        }
        if let Some(mesh) = &object.mesh_renderer {
            let mut levels = vec![(
                mesh.model.as_str().to_owned(),
                mesh.texture.as_ref().map(|path| path.as_str().to_owned()),
            )];
            if let Some(group) = &object.lod_group {
                for level in &group.levels {
                    let model_path = VirtualPath::parse(&level.model)
                        .map_err(|error| format!("{scene_path}: {error}"))?
                        .as_str()
                        .to_owned();
                    let texture_path = level
                        .texture
                        .as_deref()
                        .or_else(|| mesh.texture.as_ref().map(|path| path.as_str()));
                    let replacement_texture = texture_path
                        .map(|path| {
                            VirtualPath::parse(path)
                                .map(|path| path.as_str().to_owned())
                                .map_err(|error| format!("{scene_path}: {error}"))
                        })
                        .transpose()?;
                    levels.push((model_path, replacement_texture));
                }
            }
            let material = mesh.material.clone();
            for (model_path, _) in &levels {
                if !models.contains_key(model_path) {
                    let model =
                        load_model(model_path).map_err(|error| format!("{scene_path}: {error}"))?;
                    models.insert(model_path.clone(), model);
                }
            }
            let scene_texture_paths = levels
                .iter()
                .filter_map(|(_, texture)| texture.clone())
                .chain(material.iter().flat_map(|material| {
                    [
                        material.metallic_roughness_texture.as_ref(),
                        material.normal_texture.as_ref(),
                        material.emissive_texture.as_ref(),
                        material.occlusion_texture.as_ref(),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|path| path.as_str().to_owned())
                }))
                .collect::<Vec<_>>();
            for path in scene_texture_paths {
                if let std::collections::hash_map::Entry::Vacant(entry) = textures.entry(path) {
                    let texture = assets
                        .load_texture(entry.key())
                        .map_err(|error| format!("{scene_path}: {error}"))?;
                    entry.insert(texture);
                }
            }
            objects.push(ObjectModel {
                object: object.handle,
                levels,
                material,
                animated: object.animator.is_some(),
            });
        }
        for script in &object.lite_scripts {
            lite_paths.insert(script.path.as_str().to_owned());
        }
    }

    for object in world.objects() {
        let Some(animator) = &object.animator else {
            continue;
        };
        for path in &animator.clips {
            if !clips.contains_key(path.as_str()) {
                let animations = assets
                    .load_animations(&format!("/{}", path.as_str()))
                    .map_err(|error| format!("{scene_path}: {error}"))?;
                clips.insert(path.as_str().to_owned(), Arc::new(animations));
            }
        }
    }

    let mut sources = BTreeMap::new();
    for path in lite_paths {
        sources.insert(path.clone(), read_utf8(resources, &path)?);
    }

    Ok(PreparedScene {
        world,
        name: scene_path.to_owned(),
        sources,
        render_assets: PreparedRenderAssets {
            models,
            textures,
            environments,
            clips,
            objects,
        },
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MaterialTextureSource {
    Model(usize),
    Scene(String),
}

#[derive(Clone, Debug, PartialEq)]
struct ResolvedMaterial {
    base_color: [f32; 3],
    metallic: f32,
    roughness: f32,
    emissive: [f32; 3],
    opacity: f32,
    alpha_mode: AlphaMode,
    double_sided: bool,
    base_color_texture: Option<MaterialTextureSource>,
    metallic_roughness_texture: Option<MaterialTextureSource>,
    normal_texture: Option<MaterialTextureSource>,
    emissive_texture: Option<MaterialTextureSource>,
    occlusion_texture: Option<MaterialTextureSource>,
    normal_scale: f32,
    occlusion_strength: f32,
}

struct GpuTextureCache<T> {
    model: HashMap<(String, usize, TextureColorSpace), T>,
    scene: HashMap<(String, TextureColorSpace), T>,
}

impl<T> Default for GpuTextureCache<T> {
    fn default() -> Self {
        Self {
            model: HashMap::new(),
            scene: HashMap::new(),
        }
    }
}

fn get_or_insert_texture<K, T>(
    cache: &mut HashMap<K, T>,
    key: K,
    create: impl FnOnce() -> Result<T, RenderError>,
) -> Result<T, RenderError>
where
    K: Eq + Hash,
    T: Copy,
{
    if let Some(&texture) = cache.get(&key) {
        return Ok(texture);
    }
    let texture = create()?;
    cache.insert(key, texture);
    Ok(texture)
}

fn resolve_material(
    model: &ModelMaterial,
    replacement_texture: Option<&str>,
    override_material: Option<&MaterialOverride>,
) -> ResolvedMaterial {
    let override_texture = |path: Option<&engine_files::VirtualPath>| {
        path.map(|path| MaterialTextureSource::Scene(path.as_str().to_owned()))
    };
    ResolvedMaterial {
        base_color: override_material
            .and_then(|material| material.base_color)
            .unwrap_or(model.base_color),
        metallic: override_material
            .and_then(|material| material.metallic)
            .unwrap_or(model.metallic),
        roughness: override_material
            .and_then(|material| material.roughness)
            .unwrap_or(model.roughness),
        emissive: override_material
            .and_then(|material| material.emissive)
            .unwrap_or(model.emissive),
        opacity: override_material
            .and_then(|material| material.opacity)
            .unwrap_or(model.opacity),
        alpha_mode: match override_material.and_then(|material| material.alpha_mode) {
            Some(MaterialAlphaMode::Opaque) => AlphaMode::Opaque,
            Some(MaterialAlphaMode::Mask) => AlphaMode::Mask {
                cutoff: override_material
                    .and_then(|material| material.alpha_cutoff)
                    .unwrap_or(0.5),
            },
            Some(MaterialAlphaMode::Blend) => AlphaMode::Blend,
            None => match model.alpha_mode {
                ModelAlphaMode::Opaque => AlphaMode::Opaque,
                ModelAlphaMode::Blend => AlphaMode::Blend,
            },
        },
        double_sided: override_material
            .and_then(|material| material.double_sided)
            .unwrap_or(model.double_sided),
        base_color_texture: replacement_texture
            .map(|path| MaterialTextureSource::Scene(path.to_owned()))
            .or_else(|| model.base_color_texture.map(MaterialTextureSource::Model)),
        metallic_roughness_texture: override_texture(
            override_material.and_then(|material| material.metallic_roughness_texture.as_ref()),
        )
        .or_else(|| {
            model
                .metallic_roughness_texture
                .map(MaterialTextureSource::Model)
        }),
        normal_texture: override_texture(
            override_material.and_then(|material| material.normal_texture.as_ref()),
        )
        .or_else(|| model.normal_texture.map(MaterialTextureSource::Model)),
        emissive_texture: override_texture(
            override_material.and_then(|material| material.emissive_texture.as_ref()),
        )
        .or_else(|| model.emissive_texture.map(MaterialTextureSource::Model)),
        occlusion_texture: override_texture(
            override_material.and_then(|material| material.occlusion_texture.as_ref()),
        )
        .or_else(|| model.occlusion_texture.map(MaterialTextureSource::Model)),
        normal_scale: override_material
            .and_then(|material| material.normal_scale)
            .unwrap_or(model.normal_scale),
        occlusion_strength: override_material
            .and_then(|material| material.occlusion_strength)
            .unwrap_or(model.occlusion_strength),
    }
}

fn read_utf8(resources: &ResourceHandle, path: &str) -> Result<String, String> {
    let bytes = resources
        .read_all(path)
        .map_err(|error| format!("{path}: {error}"))?
        .wait()
        .map_err(|error| format!("{path}: {error}"))?;
    String::from_utf8(bytes).map_err(|error| format!("{path}: исходник должен быть UTF-8: {error}"))
}

pub(crate) fn upload_scene(
    renderer: &mut Renderer,
    assets: &PreparedRenderAssets,
) -> Result<GpuSceneTable, RenderError> {
    let mut gpu_models: HashMap<String, Vec<(engine_render::MeshId, MaterialId)>> = HashMap::new();
    let mut texture_cache = GpuTextureCache::<TextureId>::default();
    // Модели, которые анимирует хотя бы один объект, загружаются со скином.
    let animated = animated_model_paths(assets);

    for (path, model) in &assets.models {
        let skinned = animated.contains(path.as_str()) && model.skeleton.is_some();
        let joint_count = model
            .skeleton
            .as_ref()
            .map_or(0, |skeleton| skeleton.joints.len() as u32);
        let mut gpu_meshes = Vec::with_capacity(model.meshes.len());
        for mesh in &model.meshes {
            let vertices = mesh
                .vertices
                .iter()
                .map(|vertex| Vertex {
                    position: vertex.position,
                    normal: vertex.normal,
                    uv: vertex.uv,
                    color: [1.0; 3],
                })
                .collect::<Vec<_>>();
            let mesh_id = match &mesh.skin {
                Some(skin) if skinned => renderer.create_skinned_mesh(
                    &vertices,
                    &skin_vertices(skin),
                    &mesh.indices,
                    joint_count,
                )?,
                _ => renderer.create_mesh(&vertices, &mesh.indices)?,
            };
            let resolved = resolve_material(&mesh.material, None, None);
            let material_id = upload_resolved_material(
                renderer,
                &resolved,
                path,
                model,
                &assets.textures,
                &mut texture_cache,
            )?;
            gpu_meshes.push((mesh_id, material_id));
        }
        gpu_models.insert(path.clone(), gpu_meshes);
    }

    let mut table = GpuSceneTable {
        environments: assets.environments.clone(),
        animation_clips: assets.clips.clone(),
        ..GpuSceneTable::default()
    };
    for path in &animated {
        // Мосту нужны скелет и клипы; вершины остаются только в GPU-таблице.
        if let Some(model) = assets.models.get(*path) {
            table.animation_models.insert(
                (*path).to_owned(),
                Arc::new(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: model.skeleton.clone(),
                    animations: model.animations.clone(),
                }),
            );
        }
    }
    for object in &assets.objects {
        let mut levels = Vec::with_capacity(object.levels.len());
        for (model_path, replacement_texture) in &object.levels {
            let model_meshes = gpu_models.get(model_path).ok_or({
                RenderError::InvalidState("CPU-модель отсутствует в таблице GPU-ресурсов")
            })?;
            if object.material.is_none() && replacement_texture.is_none() {
                levels.push(model_meshes.clone());
                continue;
            }
            let model = assets.models.get(model_path).ok_or({
                RenderError::InvalidState(
                    "CPU-модель отсутствует в таблице подготовленных ресурсов",
                )
            })?;
            let mut draws = Vec::with_capacity(model_meshes.len());
            for (source_mesh, (mesh_id, _)) in model.meshes.iter().zip(model_meshes) {
                let resolved = resolve_material(
                    &source_mesh.material,
                    replacement_texture.as_deref(),
                    object.material.as_ref(),
                );
                let material_id = upload_resolved_material(
                    renderer,
                    &resolved,
                    model_path,
                    model,
                    &assets.textures,
                    &mut texture_cache,
                )?;
                draws.push((*mesh_id, material_id));
            }
            levels.push(draws);
        }
        if object.animated
            && let Some((model_path, _)) = object.levels.first()
            && assets
                .models
                .get(model_path)
                .is_some_and(|model| model.skeleton.is_some())
        {
            // Экземпляр skinning на каждую часть уровня 0: выходные вершины живут отдельно.
            let mut instances = Vec::with_capacity(levels[0].len());
            for (mesh_id, _) in &levels[0] {
                instances.push(renderer.create_skin_instance(*mesh_id)?);
            }
            table.skin_instances.insert(object.object, instances);
        }
        table.by_object.insert(object.object, levels);
    }
    Ok(table)
}

/// Пути моделей уровня 0 у объектов с `animator`.
fn animated_model_paths(assets: &PreparedRenderAssets) -> HashSet<&str> {
    assets
        .objects
        .iter()
        .filter(|object| object.animated)
        .filter_map(|object| object.levels.first())
        .map(|(path, _)| path.as_str())
        .collect()
}

fn skin_vertices(skin: &ModelSkin) -> Vec<SkinVertex> {
    skin.joints
        .iter()
        .zip(&skin.weights)
        .map(|(joints, weights)| SkinVertex {
            joints: *joints,
            weights: *weights,
        })
        .collect()
}

fn upload_resolved_material(
    renderer: &mut Renderer,
    resolved: &ResolvedMaterial,
    model_path: &str,
    model: &Model,
    scene_texture_data: &HashMap<String, TextureData>,
    texture_cache: &mut GpuTextureCache<TextureId>,
) -> Result<MaterialId, RenderError> {
    let base_color_texture = upload_material_texture(
        renderer,
        resolved.base_color_texture.as_ref(),
        TextureColorSpace::Srgb,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let metallic_roughness_texture = upload_material_texture(
        renderer,
        resolved.metallic_roughness_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let normal_texture = upload_material_texture(
        renderer,
        resolved.normal_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let emissive_texture = upload_material_texture(
        renderer,
        resolved.emissive_texture.as_ref(),
        TextureColorSpace::Srgb,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let occlusion_texture = upload_material_texture(
        renderer,
        resolved.occlusion_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    renderer.create_material(&Material {
        base_color: resolved.base_color,
        metallic: resolved.metallic,
        roughness: resolved.roughness,
        emissive: resolved.emissive,
        opacity: resolved.opacity,
        alpha_mode: resolved.alpha_mode,
        double_sided: resolved.double_sided,
        base_color_texture,
        metallic_roughness_texture,
        normal_texture,
        emissive_texture,
        occlusion_texture,
        normal_scale: resolved.normal_scale,
        occlusion_strength: resolved.occlusion_strength,
    })
}

fn upload_material_texture(
    renderer: &mut Renderer,
    source: Option<&MaterialTextureSource>,
    color_space: TextureColorSpace,
    model_path: &str,
    model: &Model,
    scene_texture_data: &HashMap<String, TextureData>,
    texture_cache: &mut GpuTextureCache<TextureId>,
) -> Result<Option<TextureId>, RenderError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let texture_id = match source {
        MaterialTextureSource::Model(index) => {
            let key = (model_path.to_owned(), *index, color_space);
            let texture = model.textures.get(*index).ok_or_else(|| {
                RenderError::InvalidTexture(format!(
                    "модель {model_path:?} ссылается на отсутствующую текстуру {index}"
                ))
            })?;
            get_or_insert_texture(&mut texture_cache.model, key, || {
                renderer.create_texture_with_color_space(
                    texture.width,
                    texture.height,
                    &texture.rgba,
                    color_space,
                )
            })?
        }
        MaterialTextureSource::Scene(path) => {
            let key = (path.clone(), color_space);
            let texture = scene_texture_data.get(path).ok_or({
                RenderError::InvalidState("CPU-текстура отсутствует в таблице GPU-ресурсов")
            })?;
            get_or_insert_texture(&mut texture_cache.scene, key, || {
                renderer.create_texture_with_color_space(
                    texture.width,
                    texture.height,
                    &texture.rgba,
                    color_space,
                )
            })?
        }
    };
    Ok(Some(texture_id))
}

#[cfg(test)]
mod tests {
    use super::{
        GpuTextureCache, MaterialTextureSource, get_or_insert_texture, load_scene,
        prepare_scene_with_model_loader, resolve_material,
    };
    use engine_assets::{Assets, Model, ModelAlphaMode, ModelMaterial};
    use engine_files::VirtualPath;
    use engine_physics::BodyKind;
    use engine_project::PhysicsSettings;
    use engine_render::{AlphaMode, EnvironmentSource, TextureColorSpace};
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_scene::{MaterialAlphaMode, MaterialOverride, SceneDocument, World};
    use std::cell::Cell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "rve-runtime-scene-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn root(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct LooseResources {
        system: ResourceSystem,
        assets: Assets,
    }

    impl LooseResources {
        fn new(root: &Path) -> Self {
            let system = ResourceSystem::start(ResourceConfig {
                root: root.to_owned(),
                source_mode: SourceMode::Loose,
                worker_count: 1,
                ..ResourceConfig::default()
            })
            .unwrap();
            let assets = Assets::with_resources(system.handle());
            Self { system, assets }
        }
    }

    impl Drop for LooseResources {
        fn drop(&mut self) {
            let _ = self.system.shutdown();
        }
    }

    const CAMERA_SCENE: &str = r#"
[[objects]]
id = "camera"
[objects.camera]
"#;

    fn camera_settings_scene(two_cameras: bool) -> String {
        let second = if two_cameras {
            "\n[[objects]]\nid = \"second\"\n[objects.camera]\nsettings = \"/cameras/shared.camerasettings\"\n"
        } else {
            ""
        };
        format!(
            "[[assets]]\npath = \"/cameras/shared.camerasettings\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = {two_cameras}\nsettings = \"/cameras/shared.camerasettings\"\n{second}"
        )
    }

    fn scene_with_lite_script() -> String {
        format!(
            "[[assets]]\npath = \"scripts/main.lua\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"scripted\"\n[[objects.lite_scripts]]\npath = \"scripts/main.lua\"\n"
        )
    }

    #[test]
    fn load_scene_accepts_a_scene_without_meshes_from_loose_resources() {
        let project = TempProject::new();
        project.write("scene.rvescene", CAMERA_SCENE.as_bytes());
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        assert!(prepared.render_assets.models.is_empty());
        assert!(prepared.render_assets.objects.is_empty());
        assert!(prepared.sources.is_empty());
    }

    #[test]
    fn physics_scene_is_built_from_loaded_world_before_runtime_startup() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!(
                "{CAMERA_SCENE}\n[[objects]]\nid = \"body\"\n[objects.transform]\nscale = [0.0, 0.0, 0.0]\n[objects.collider]\nshape = \"sphere\"\nradius = 0.5\n"
            )
            .as_bytes(),
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();

        let error = prepared
            .build_physics(&PhysicsSettings::default())
            .err()
            .expect("zero-scale collider should fail physics prebuild");
        assert!(error.contains("scale"), "{error}");
    }

    #[test]
    fn demo_game_scenes_load_and_build_physics_without_gpu() {
        let assets = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/demo-game/Assets"
        ));
        let resources = LooseResources::new(assets);
        let handle = resources.system.handle();
        for scene in [
            "/scenes/physics.rvescene",
            "/scenes/main.rvescene",
            "/scenes/second.rvescene",
        ] {
            let prepared = load_scene(&handle, &resources.assets, scene)
                .unwrap_or_else(|error| panic!("{scene}: {error}"));
            prepared
                .build_physics(&PhysicsSettings::default())
                .unwrap_or_else(|error| panic!("{scene}: {error}"));
        }

        let prepared = load_scene(&handle, &resources.assets, "/scenes/physics.rvescene").unwrap();
        engine_script::ScriptHost::load(
            &prepared.world,
            "/scenes/physics.rvescene",
            &prepared.sources,
            None,
        )
        .unwrap_or_else(|error| panic!("physics scripts: {error}"));
        let (_, bindings) = prepared.build_physics(&PhysicsSettings::default()).unwrap();
        // 10 статических коллайдеров, 4 монеты-триггера, 4 ящика и 1 персонаж.
        assert_eq!(bindings.len(), 19);
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.kind == BodyKind::Dynamic)
                .count(),
            4
        );
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.character_controller.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn load_scene_reports_a_missing_lua_source_from_loose_resources() {
        let project = TempProject::new();
        project.write("scene.rvescene", scene_with_lite_script().as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("scripts/main.lua"));
    }

    #[test]
    fn load_scene_rejects_a_lua_source_that_is_not_utf8() {
        let project = TempProject::new();
        project.write("scene.rvescene", scene_with_lite_script().as_bytes());
        project.write("scripts/main.lua", &[0xff, 0xfe]);
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("исходник должен быть UTF-8"));
    }

    #[test]
    fn load_scene_expands_prefab_instances_through_the_resource_handle() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/prefabs/spinner.rveprefab\"\n{CAMERA_SCENE}\n[[instances]]\nid = \"spinner1\"\nprefab = \"/prefabs/spinner.rveprefab\"\n[[instances.overrides]]\ntarget = \"blade\"\nlite_params = [{{ path = \"/scripts/spin.lua\", params = {{ speed = 2 }} }}]\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write(
            "prefabs/spinner.rveprefab",
            b"[[assets]]\npath = \"/scripts/spin.lua\"\n[[objects]]\nid = \"hub\"\n[[objects]]\nid = \"blade\"\nparent = \"hub\"\n[[objects.lite_scripts]]\npath = \"/scripts/spin.lua\"\n",
        );
        project.write("scripts/spin.lua", b"return {}\n");
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let blade = prepared.world.handle("spinner1.blade").unwrap();
        let object = prepared.world.object(blade).unwrap();
        assert_eq!(
            object.lite_scripts[0].params["speed"],
            engine_scene::ParamValue::Int(2)
        );
        assert!(prepared.world.handle("spinner1").is_some());
        assert!(prepared.sources.contains_key("scripts/spin.lua"));
    }

    #[test]
    fn load_scene_prepares_hdr_environment_assets_introduced_by_prefabs() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/prefabs/sky.rveprefab\"\n{CAMERA_SCENE}\n[[instances]]\nid = \"sky\"\nprefab = \"/prefabs/sky.rveprefab\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write(
            "prefabs/sky.rveprefab",
            b"[[assets]]\npath = \"/environment/sky.hdr\"\n[[objects]]\nid = \"environment\"\n[objects.environment]\nhdr = \"/environment/sky.hdr\"\n",
        );
        project.write(
            "environment/sky.hdr",
            b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\n\x80\x80\x80\x81\x40\x60\x80\x82",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let EnvironmentSource::Equirect { width, height, rgb } = prepared
            .render_assets
            .environments
            .get("environment/sky.hdr")
            .unwrap()
            .as_ref()
        else {
            panic!("HDR environment should be prepared as an equirectangular image");
        };
        assert_eq!((*width, *height), (2, 1));
        assert_eq!(rgb.len(), 6);
        assert!(
            prepared
                .world
                .object_by_id("sky")
                .unwrap()
                .environment
                .is_some()
        );
    }

    #[test]
    fn load_scene_reports_a_missing_prefab() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/prefabs/gone.rveprefab\"\n{CAMERA_SCENE}\n[[instances]]\nid = \"x\"\nprefab = \"/prefabs/gone.rveprefab\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("gone.rveprefab"), "{error}");
    }

    #[test]
    fn load_scene_rejects_an_invalid_scene_from_loose_resources() {
        let project = TempProject::new();
        project.write("scene.rvescene", b"objects = [");
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("scene.rvescene"));
    }

    #[test]
    fn load_scene_rejects_a_missing_camera_settings_asset() {
        let project = TempProject::new();
        let source = camera_settings_scene(false);
        project.write("scene.rvescene", source.as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("cameras/shared.camerasettings"));
    }

    #[test]
    fn load_scene_rejects_camera_settings_that_are_not_utf8_or_valid_toml() {
        let project = TempProject::new();
        let source = camera_settings_scene(false);
        project.write("scene.rvescene", source.as_bytes());
        project.write("cameras/shared.camerasettings", &[0xff, 0xfe]);
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("cameras/shared.camerasettings"));
        assert!(error.contains("UTF-8"));
        let project = TempProject::new();
        project.write("scene.rvescene", source.as_bytes());
        project.write("cameras/shared.camerasettings", b"[projection\nfov = 45");
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("cameras/shared.camerasettings"));
    }

    #[test]
    fn cameras_sharing_a_settings_asset_receive_the_same_validated_settings() {
        let project = TempProject::new();
        let source = camera_settings_scene(true);
        project.write("scene.rvescene", source.as_bytes());
        project.write(
            "cameras/shared.camerasettings",
            b"[projection]\nfov_deg = 42.0\nnear = 0.2\nfar = 250.0\n",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        for id in ["camera", "second"] {
            let world = &prepared.world;
            let handle = world.handle(id).unwrap();
            assert_eq!(
                world.camera_settings(handle).unwrap().projection.fov_deg,
                42.0
            );
            assert_eq!(world.camera_settings(handle).unwrap().projection.near, 0.2);
        }
    }

    #[test]
    fn two_objects_using_one_model_share_a_single_cached_load() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"models/quad.fbx\"\n[[assets]]\npath = \"scripts/main.lua\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"first\"\n[objects.mesh_renderer]\nmodel = \"models/quad.fbx\"\n[[objects]]\nid = \"second\"\n[objects.mesh_renderer]\nmodel = \"models/quad.fbx\"\n[[objects.lite_scripts]]\npath = \"scripts/main.lua\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write("scripts/main.lua", b"return {}\n");
        let resources = LooseResources::new(project.root());
        let document = SceneDocument::parse("scene.rvescene", &scene_text).unwrap();
        let world = World::from_document(&document).unwrap();
        let load_count = Cell::new(0);
        let prepared = prepare_scene_with_model_loader(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
            world,
            |path| {
                assert_eq!(path, "models/quad.fbx");
                load_count.set(load_count.get() + 1);
                Ok(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: None,
                    animations: Vec::new(),
                })
            },
        )
        .unwrap();
        assert_eq!(load_count.get(), 1);
        assert_eq!(prepared.render_assets.models.len(), 1);
        assert_eq!(prepared.render_assets.objects.len(), 2);
        assert_eq!(
            prepared.sources.get("scripts/main.lua").unwrap(),
            "return {}\n"
        );
    }

    #[test]
    fn lod_models_load_once_and_keep_document_level_order() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath=\"models/base.fbx\"\n[[assets]]\npath=\"models/lod.fbx\"\n{CAMERA_SCENE}\n[[objects]]\nid=\"first\"\n[objects.mesh_renderer]\nmodel=\"models/base.fbx\"\n[objects.lod_group]\ncull_distance=50.0\n[[objects.lod_group.levels]]\nmodel=\"models/lod.fbx\"\ndistance=10.0\n[[objects]]\nid=\"second\"\n[objects.mesh_renderer]\nmodel=\"models/lod.fbx\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        let resources = LooseResources::new(project.root());
        let document = SceneDocument::parse("scene.rvescene", &scene_text).unwrap();
        let world = World::from_document(&document).unwrap();
        let mut loaded_paths = Vec::new();
        let prepared = prepare_scene_with_model_loader(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
            world,
            |path| {
                loaded_paths.push(path.to_owned());
                Ok(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: None,
                    animations: Vec::new(),
                })
            },
        )
        .unwrap();

        assert_eq!(loaded_paths, ["models/base.fbx", "models/lod.fbx"]);
        assert_eq!(prepared.render_assets.models.len(), 2);
        assert_eq!(
            prepared.render_assets.objects[0].levels,
            [
                ("models/base.fbx".to_owned(), None),
                ("models/lod.fbx".to_owned(), None),
            ]
        );
        assert_eq!(
            prepared.render_assets.objects[1].levels,
            [("models/lod.fbx".to_owned(), None)]
        );
    }

    #[test]
    fn material_resolution_keeps_fbx_defaults_and_applies_only_specified_overrides() {
        let model = ModelMaterial {
            base_color: [0.2, 0.4, 0.6],
            metallic: 0.7,
            roughness: 0.3,
            emissive: [0.5, 1.0, 2.0],
            opacity: 0.8,
            alpha_mode: ModelAlphaMode::Blend,
            base_color_texture: Some(0),
            metallic_roughness_texture: Some(1),
            normal_texture: Some(2),
            emissive_texture: Some(3),
            occlusion_texture: Some(4),
            normal_scale: 0.8,
            occlusion_strength: 0.6,
            double_sided: true,
        };
        let unchanged = resolve_material(&model, None, None);
        assert_eq!(unchanged.base_color, model.base_color);
        assert_eq!(unchanged.metallic, model.metallic);
        assert_eq!(unchanged.roughness, model.roughness);
        assert_eq!(unchanged.emissive, model.emissive);
        assert_eq!(unchanged.opacity, model.opacity);
        assert_eq!(unchanged.alpha_mode, AlphaMode::Blend);
        assert!(unchanged.double_sided);
        assert_eq!(
            unchanged.base_color_texture,
            Some(MaterialTextureSource::Model(0))
        );
        assert_eq!(
            unchanged.metallic_roughness_texture,
            Some(MaterialTextureSource::Model(1))
        );
        assert_eq!(
            unchanged.normal_texture,
            Some(MaterialTextureSource::Model(2))
        );
        assert_eq!(unchanged.normal_scale, 0.8);
        assert_eq!(unchanged.occlusion_strength, 0.6);
        assert_eq!(
            unchanged.emissive_texture,
            Some(MaterialTextureSource::Model(3))
        );
        assert_eq!(
            unchanged.occlusion_texture,
            Some(MaterialTextureSource::Model(4))
        );

        let override_material = MaterialOverride {
            base_color: Some([0.9, 0.8, 0.7]),
            metallic: Some(1.0),
            roughness: None,
            emissive: Some([3.0, 2.0, 1.0]),
            opacity: Some(0.4),
            alpha_mode: Some(MaterialAlphaMode::Mask),
            alpha_cutoff: Some(0.25),
            metallic_roughness_texture: Some(VirtualPath::parse("/textures/mr.png").unwrap()),
            normal_texture: Some(VirtualPath::parse("/textures/normal.png").unwrap()),
            emissive_texture: Some(VirtualPath::parse("/textures/emissive.png").unwrap()),
            occlusion_texture: Some(VirtualPath::parse("/textures/occlusion.png").unwrap()),
            normal_scale: Some(0.5),
            occlusion_strength: Some(0.2),
            double_sided: Some(false),
        };
        let replacement = resolve_material(
            &model,
            Some("textures/replacement.png"),
            Some(&override_material),
        );
        assert_eq!(replacement.base_color, [0.9, 0.8, 0.7]);
        assert_eq!(replacement.metallic, 1.0);
        assert_eq!(replacement.roughness, model.roughness);
        assert_eq!(replacement.emissive, [3.0, 2.0, 1.0]);
        assert_eq!(replacement.opacity, 0.4);
        assert_eq!(replacement.alpha_mode, AlphaMode::Mask { cutoff: 0.25 });
        assert!(!replacement.double_sided);
        assert_eq!(
            replacement.base_color_texture,
            Some(MaterialTextureSource::Scene(
                "textures/replacement.png".to_owned()
            ))
        );
        assert_eq!(
            replacement.metallic_roughness_texture,
            Some(MaterialTextureSource::Scene("textures/mr.png".to_owned()))
        );
        assert_eq!(
            replacement.normal_texture,
            Some(MaterialTextureSource::Scene(
                "textures/normal.png".to_owned()
            ))
        );
        assert_eq!(replacement.normal_scale, 0.5);
        assert_eq!(replacement.occlusion_strength, 0.2);
        assert_eq!(
            replacement.emissive_texture,
            Some(MaterialTextureSource::Scene(
                "textures/emissive.png".to_owned()
            ))
        );
        assert_eq!(
            replacement.occlusion_texture,
            Some(MaterialTextureSource::Scene(
                "textures/occlusion.png".to_owned()
            ))
        );
    }

    #[test]
    fn gpu_texture_cache_reuses_uploads_per_source_and_color_space() {
        let mut cache = GpuTextureCache::<u32>::default();
        let uploads = Cell::new(0_u32);

        let model_srgb_key = ("models/a.fbx".to_owned(), 0, TextureColorSpace::Srgb);
        let model_srgb = get_or_insert_texture(&mut cache.model, model_srgb_key.clone(), || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let model_srgb_reused = get_or_insert_texture(&mut cache.model, model_srgb_key, || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let model_linear = get_or_insert_texture(
            &mut cache.model,
            ("models/a.fbx".to_owned(), 0, TextureColorSpace::Linear),
            || {
                uploads.set(uploads.get() + 1);
                Ok(uploads.get())
            },
        )
        .unwrap();
        assert_eq!(model_srgb, model_srgb_reused);
        assert_ne!(model_srgb, model_linear);

        let scene_srgb_key = ("textures/shared.png".to_owned(), TextureColorSpace::Srgb);
        let scene_srgb = get_or_insert_texture(&mut cache.scene, scene_srgb_key.clone(), || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let scene_srgb_reused = get_or_insert_texture(&mut cache.scene, scene_srgb_key, || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let scene_linear = get_or_insert_texture(
            &mut cache.scene,
            ("textures/shared.png".to_owned(), TextureColorSpace::Linear),
            || {
                uploads.set(uploads.get() + 1);
                Ok(uploads.get())
            },
        )
        .unwrap();
        assert_eq!(scene_srgb, scene_srgb_reused);
        assert_ne!(scene_srgb, scene_linear);
        assert_eq!(uploads.get(), 4);
    }
}
