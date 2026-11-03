use crate::pipeline::GpuSceneTable;
use engine_assets::{Assets, Model, ModelMaterial, TextureData};
use engine_render::{
    Material, MaterialId, RenderError, Renderer, TextureColorSpace, TextureId, Vertex,
};
use engine_resources::ResourceHandle;
use engine_scene::{MaterialOverride, ObjectHandle, SceneDocument, World};
use std::collections::{BTreeMap, HashMap};

pub(crate) struct ObjectModel {
    pub object: ObjectHandle,
    pub model_path: String,
    pub replacement_texture: Option<String>,
    pub material: Option<MaterialOverride>,
}

pub(crate) struct PreparedRenderAssets {
    pub models: HashMap<String, Model>,
    pub textures: HashMap<String, TextureData>,
    pub objects: Vec<ObjectModel>,
}

pub(crate) struct PreparedScene {
    pub world: World,
    pub name: String,
    pub sources: BTreeMap<String, String>,
    pub render_assets: PreparedRenderAssets,
}

pub(crate) fn load_scene(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
) -> Result<PreparedScene, String> {
    let contents = read_utf8(resources, scene_path)?;
    let document =
        SceneDocument::parse(scene_path, &contents).map_err(|error| error.to_string())?;
    let world = World::from_document(&document).map_err(|error| error.to_string())?;
    prepare_scene(resources, assets, scene_path, world)
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
    world: World,
    mut load_model: impl FnMut(&str) -> Result<Model, String>,
) -> Result<PreparedScene, String> {
    let mut models = HashMap::new();
    let mut textures = HashMap::new();
    let mut objects = Vec::new();
    let mut lite_paths = std::collections::BTreeSet::new();

    for object in world.objects() {
        if let Some(mesh) = &object.mesh_renderer {
            let model_path = mesh.model.as_str().to_owned();
            if !models.contains_key(&model_path) {
                let model =
                    load_model(&model_path).map_err(|error| format!("{scene_path}: {error}"))?;
                models.insert(model_path.clone(), model);
            }
            let replacement_texture = mesh.texture.as_ref().map(|path| path.as_str().to_owned());
            let material = mesh.material.clone();
            let scene_texture_paths = replacement_texture
                .iter()
                .cloned()
                .chain(material.iter().flat_map(|material| {
                    [
                        material.metallic_roughness_texture.as_ref(),
                        material.normal_texture.as_ref(),
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
                model_path,
                replacement_texture,
                material,
            });
        }
        for script in &object.lite_scripts {
            lite_paths.insert(script.path.as_str().to_owned());
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
    base_color_texture: Option<MaterialTextureSource>,
    metallic_roughness_texture: Option<MaterialTextureSource>,
    normal_texture: Option<MaterialTextureSource>,
    normal_scale: f32,
}

#[derive(Default)]
struct GpuTextureCache {
    model: HashMap<(String, usize, TextureColorSpace), TextureId>,
    scene: HashMap<(String, TextureColorSpace), TextureId>,
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
        normal_scale: override_material
            .and_then(|material| material.normal_scale)
            .unwrap_or(model.normal_scale),
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
    let mut texture_cache = GpuTextureCache::default();

    for (path, model) in &assets.models {
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
            let mesh_id = renderer.create_mesh(&vertices, &mesh.indices)?;
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

    let mut table = GpuSceneTable::default();
    for object in &assets.objects {
        let model_meshes = gpu_models.get(&object.model_path).ok_or({
            RenderError::InvalidState("CPU-модель отсутствует в таблице GPU-ресурсов")
        })?;
        if object.material.is_none() && object.replacement_texture.is_none() {
            table.by_object.insert(object.object, model_meshes.clone());
        } else {
            let model = assets.models.get(&object.model_path).ok_or({
                RenderError::InvalidState(
                    "CPU-модель отсутствует в таблице подготовленных ресурсов",
                )
            })?;
            let mut draws = Vec::with_capacity(model_meshes.len());
            for (source_mesh, (mesh_id, _)) in model.meshes.iter().zip(model_meshes) {
                let resolved = resolve_material(
                    &source_mesh.material,
                    object.replacement_texture.as_deref(),
                    object.material.as_ref(),
                );
                let material_id = upload_resolved_material(
                    renderer,
                    &resolved,
                    &object.model_path,
                    model,
                    &assets.textures,
                    &mut texture_cache,
                )?;
                draws.push((*mesh_id, material_id));
            }
            table.by_object.insert(object.object, draws);
        }
    }
    Ok(table)
}

fn upload_resolved_material(
    renderer: &mut Renderer,
    resolved: &ResolvedMaterial,
    model_path: &str,
    model: &Model,
    scene_texture_data: &HashMap<String, TextureData>,
    texture_cache: &mut GpuTextureCache,
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
    renderer.create_material(&Material {
        base_color: resolved.base_color,
        metallic: resolved.metallic,
        roughness: resolved.roughness,
        base_color_texture,
        metallic_roughness_texture,
        normal_texture,
        normal_scale: resolved.normal_scale,
    })
}

fn upload_material_texture(
    renderer: &mut Renderer,
    source: Option<&MaterialTextureSource>,
    color_space: TextureColorSpace,
    model_path: &str,
    model: &Model,
    scene_texture_data: &HashMap<String, TextureData>,
    texture_cache: &mut GpuTextureCache,
) -> Result<Option<TextureId>, RenderError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let (width, height, rgba, model_key, scene_key) = match source {
        MaterialTextureSource::Model(index) => {
            let key = (model_path.to_owned(), *index, color_space);
            if let Some(&texture) = texture_cache.model.get(&key) {
                return Ok(Some(texture));
            }
            let texture = model.textures.get(*index).ok_or_else(|| {
                RenderError::InvalidTexture(format!(
                    "модель {model_path:?} ссылается на отсутствующую текстуру {index}"
                ))
            })?;
            (
                texture.width,
                texture.height,
                &texture.rgba,
                Some(key),
                None,
            )
        }
        MaterialTextureSource::Scene(path) => {
            let key = (path.clone(), color_space);
            if let Some(&texture) = texture_cache.scene.get(&key) {
                return Ok(Some(texture));
            }
            let texture = scene_texture_data.get(path).ok_or({
                RenderError::InvalidState("CPU-текстура отсутствует в таблице GPU-ресурсов")
            })?;
            (
                texture.width,
                texture.height,
                &texture.rgba,
                None,
                Some(key),
            )
        }
    };
    let texture_id = renderer.create_texture_with_color_space(width, height, rgba, color_space)?;
    if let Some(key) = model_key {
        texture_cache.model.insert(key, texture_id);
    }
    if let Some(key) = scene_key {
        texture_cache.scene.insert(key, texture_id);
    }
    Ok(Some(texture_id))
}

#[cfg(test)]
mod tests {
    use super::{
        MaterialTextureSource, load_scene, prepare_scene_with_model_loader, resolve_material,
    };
    use engine_assets::{Assets, Model, ModelMaterial};
    use engine_files::VirtualPath;
    use engine_render::TextureColorSpace;
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_scene::{MaterialOverride, SceneDocument, World};
    use std::cell::Cell;
    use std::collections::HashSet;
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
fov_deg = 60.0
near = 0.1
far = 100.0
"#;

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
    fn material_resolution_keeps_fbx_defaults_and_applies_only_specified_overrides() {
        let model = ModelMaterial {
            base_color: [0.2, 0.4, 0.6],
            metallic: 0.7,
            roughness: 0.3,
            base_color_texture: Some(0),
            metallic_roughness_texture: Some(1),
            normal_texture: Some(2),
            normal_scale: 0.8,
        };
        let unchanged = resolve_material(&model, None, None);
        assert_eq!(unchanged.base_color, model.base_color);
        assert_eq!(unchanged.metallic, model.metallic);
        assert_eq!(unchanged.roughness, model.roughness);
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

        let override_material = MaterialOverride {
            base_color: Some([0.9, 0.8, 0.7]),
            metallic: Some(1.0),
            roughness: None,
            metallic_roughness_texture: Some(VirtualPath::parse("/textures/mr.png").unwrap()),
            normal_texture: Some(VirtualPath::parse("/textures/normal.png").unwrap()),
            normal_scale: Some(0.5),
        };
        let replacement = resolve_material(
            &model,
            Some("textures/replacement.png"),
            Some(&override_material),
        );
        assert_eq!(replacement.base_color, [0.9, 0.8, 0.7]);
        assert_eq!(replacement.metallic, 1.0);
        assert_eq!(replacement.roughness, model.roughness);
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
    }

    #[test]
    fn gpu_texture_cache_keys_keep_model_texture_roles_separate() {
        let keys = HashSet::from([
            ("models/a.fbx".to_owned(), 0, TextureColorSpace::Srgb),
            ("models/a.fbx".to_owned(), 0, TextureColorSpace::Linear),
        ]);
        assert_eq!(keys.len(), 2);
    }
}
