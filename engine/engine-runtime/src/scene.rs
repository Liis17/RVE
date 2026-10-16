use crate::pipeline::GpuSceneTable;
use engine_assets::{Assets, Model, TextureData};
use engine_render::{RenderError, Renderer, Vertex};
use engine_resources::ResourceHandle;
use engine_scene::{ObjectHandle, SceneDocument, World};
use std::collections::{BTreeMap, HashMap};

pub(crate) struct ObjectModel {
    pub object: ObjectHandle,
    pub model_path: String,
    pub replacement_texture: Option<String>,
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
            if let Some(path) = &replacement_texture
                && !textures.contains_key(path)
            {
                let texture = assets
                    .load_texture(path)
                    .map_err(|error| format!("{scene_path}: {error}"))?;
                textures.insert(path.clone(), texture);
            }
            objects.push(ObjectModel {
                object: object.handle,
                model_path,
                replacement_texture,
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

fn select_texture<T: Copy>(replacement: Option<T>, model_texture: Option<T>) -> Option<T> {
    replacement.or(model_texture)
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
    let mut gpu_models: HashMap<
        String,
        Vec<(engine_render::MeshId, Option<engine_render::TextureId>)>,
    > = HashMap::new();
    let mut model_textures = HashMap::new();
    let mut replacement_textures = HashMap::new();

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
            let texture_id = match mesh.texture {
                Some(index) => {
                    if let Some(texture_id) = model_textures.get(&(path.clone(), index)) {
                        Some(*texture_id)
                    } else {
                        let texture = model.textures.get(index).ok_or_else(|| {
                            RenderError::InvalidTexture(format!(
                                "модель {path:?} ссылается на отсутствующую текстуру {index}"
                            ))
                        })?;
                        let texture_id = renderer.create_texture(
                            texture.width,
                            texture.height,
                            &texture.rgba,
                        )?;
                        model_textures.insert((path.clone(), index), texture_id);
                        Some(texture_id)
                    }
                }
                None => None,
            };
            gpu_meshes.push((mesh_id, texture_id));
        }
        gpu_models.insert(path.clone(), gpu_meshes);
    }

    let mut table = GpuSceneTable::default();
    for object in &assets.objects {
        let model_meshes = gpu_models.get(&object.model_path).ok_or({
            RenderError::InvalidState("CPU-модель отсутствует в таблице GPU-ресурсов")
        })?;
        let replacement = if let Some(path) = &object.replacement_texture {
            if let Some(texture_id) = replacement_textures.get(path) {
                Some(*texture_id)
            } else {
                let texture = assets.textures.get(path).ok_or({
                    RenderError::InvalidState("CPU-текстура отсутствует в таблице GPU-ресурсов")
                })?;
                let texture_id =
                    renderer.create_texture(texture.width, texture.height, &texture.rgba)?;
                replacement_textures.insert(path.clone(), texture_id);
                Some(texture_id)
            }
        } else {
            None
        };
        let draws = model_meshes
            .iter()
            .map(|(mesh, texture)| (*mesh, select_texture(replacement, *texture)))
            .collect();
        table.by_object.insert(object.object, draws);
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::{load_scene, prepare_scene_with_model_loader, select_texture};
    use engine_assets::{Assets, Model};
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_scene::{SceneDocument, World};
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
    fn component_texture_replaces_model_texture_or_falls_back_to_mesh_texture() {
        assert_eq!(select_texture(Some(1), Some(2)), Some(1));
        assert_eq!(select_texture(None, Some(2)), Some(2));
        assert_eq!(select_texture::<u8>(None, None), None);
    }
}
