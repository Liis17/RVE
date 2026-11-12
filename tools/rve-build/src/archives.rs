use crate::error::BuildError;
use crate::project::ProjectBuild;
use crate::temporary::TempDir;
use engine_files::VirtualPath;
use engine_pak::{
    ArchiveSetRegistry, BuildCompression, BuildFileInput, BuildOptions, ProgrammaticArchive,
    build_archive_set,
};
use engine_project::PREFAB_CATALOG_RELEASE_PATH;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub(crate) fn build_project_archives(
    project: &ProjectBuild,
    output: &Path,
) -> Result<ArchiveSetRegistry, BuildError> {
    let project_settings_path = VirtualPath::parse("/rve/project.toml").expect("constant path");
    let registry_path = VirtualPath::parse("/rve/assets.toml").expect("constant path");
    let prefabs_path = VirtualPath::parse(PREFAB_CATALOG_RELEASE_PATH).expect("constant path");
    if [&project_settings_path, &registry_path, &prefabs_path]
        .iter()
        .any(|path| project.assignments.contains_key(*path))
    {
        return Err(BuildError::input(
            "Assets не может содержать зарезервированные /rve/project.toml, /rve/assets.toml или /rve/prefabs.toml",
        ));
    }

    let generated = TempDir::create(&std::env::temp_dir(), "rve-build-generated")?;
    let generated_rve = generated.path().join("rve");
    fs::create_dir_all(&generated_rve).map_err(|error| {
        BuildError::operational(format!(
            "не удалось создать временный каталог {}: {error}",
            generated_rve.display()
        ))
    })?;

    let project_toml = project
        .settings
        .to_toml()
        .map_err(|error| BuildError::input(format!("не удалось сериализовать проект: {error}")))?;
    let mut registry_paths = project.assignments.clone();
    registry_paths.insert(project_settings_path.clone(), "base".to_owned());
    registry_paths.insert(registry_path.clone(), "base".to_owned());
    registry_paths.insert(prefabs_path.clone(), "base".to_owned());
    let registry = ArchiveSetRegistry::new(project.archives.clone(), registry_paths)
        .map_err(|error| BuildError::input(format!("ошибка реестра ресурсов: {error}")))?;
    let registry_toml = registry
        .to_toml()
        .map_err(|error| BuildError::input(format!("ошибка реестра ресурсов: {error}")))?;

    let generated_project = generated_rve.join("project.toml");
    let generated_registry = generated_rve.join("assets.toml");
    fs::write(&generated_project, project_toml).map_err(|error| {
        BuildError::operational(format!(
            "не удалось записать {}: {error}",
            generated_project.display()
        ))
    })?;
    fs::write(&generated_registry, registry_toml).map_err(|error| {
        BuildError::operational(format!(
            "не удалось записать {}: {error}",
            generated_registry.display()
        ))
    })?;

    // Список пишется всегда, даже пустым: runtime читает один формат.
    let prefabs_toml = project.prefabs.to_toml().map_err(|error| {
        BuildError::input(format!(
            "не удалось сериализовать список тяжёлых префабов: {error}"
        ))
    })?;
    let generated_prefabs = generated_rve.join("prefabs.toml");
    fs::write(&generated_prefabs, prefabs_toml).map_err(|error| {
        BuildError::operational(format!(
            "не удалось записать {}: {error}",
            generated_prefabs.display()
        ))
    })?;

    let mut archive_inputs = project
        .archives
        .iter()
        .map(|name| (name.clone(), Vec::<BuildFileInput>::new()))
        .collect::<BTreeMap<_, _>>();
    for (path, source) in &project.files {
        let archive = project
            .assignments
            .get(path)
            .ok_or_else(|| BuildError::input(format!("для ресурса /{path} не назначен архив")))?;
        archive_inputs
            .get_mut(archive)
            .ok_or_else(|| {
                BuildError::input(format!("архив {archive:?} отсутствует в списке сборки"))
            })?
            .push(BuildFileInput::new(source, path.clone()));
    }
    archive_inputs
        .get_mut("base")
        .expect("base archive is always included")
        .push(BuildFileInput::new(
            &generated_project,
            project_settings_path,
        ));
    archive_inputs
        .get_mut("base")
        .expect("base archive is always included")
        .push(BuildFileInput::new(&generated_registry, registry_path));
    archive_inputs
        .get_mut("base")
        .expect("base archive is always included")
        .push(BuildFileInput::new(&generated_prefabs, prefabs_path));

    let archives = project
        .archives
        .iter()
        .map(|name| {
            let inputs = archive_inputs
                .remove(name)
                .expect("every declared archive has an input group");
            ProgrammaticArchive {
                name: name.clone(),
                inputs,
            }
        })
        .collect();
    build_archive_set(
        archives,
        output,
        BuildCompression::default(),
        BuildOptions::default(),
    )
    .map_err(|error| BuildError::operational(format!("не удалось собрать архивы: {error}")))?;

    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::build_project_archives;
    use crate::player::validate_staged_archives;
    use crate::project::prepare_project;
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use engine_pak::{Archive, ArchiveCatalog, ArchiveLimits, ArchiveSetRegistry, CatalogLimits};
    use png::{BitDepth, ColorType, Encoder};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    fn write_png(path: &Path, width: u32, height: u32, rgba: [u8; 4]) {
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes, width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = rgba.repeat((width * height) as usize);
        writer.write_image_data(&pixels).unwrap();
        writer.finish().unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn write_fixture_fbx(path: &Path, texture: Option<&str>) {
        let mut objects = String::from(
            r#"Geometry: 1000, "Geometry::Quad", "Mesh" {
 Vertices: *12 { a: -1,-1,0,1,-1,0,1,1,0,-1,1,0 }
 PolygonVertexIndex: *4 { a: 0,1,2,-4 }
 GeometryVersion: 124
 LayerElementUV: 0 {
  Version: 101
  Name: "UVMap"
  MappingInformationType: "ByPolygonVertex"
  ReferenceInformationType: "IndexToDirect"
  UV: *8 { a: 0,0,1,0,1,1,0,1 }
  UVIndex: *4 { a: 0,1,2,3 }
 }
 Layer: 0 { Version: 100
  LayerElement: { Type: "LayerElementUV" TypedIndex: 0 }
 }
}
Model: 2000, "Model::Quad", "Mesh" { Version: 232 }
Material: 3000, "Material::Paint", "" { Version: 102 ShadingModel: "lambert" }
"#,
        );
        let mut connections =
            String::from("C: \"OO\",1000,2000\nC: \"OO\",2000,0\nC: \"OO\",3000,2000\n");
        if let Some(texture) = texture {
            objects.push_str(&format!(
                "Texture: 4000, \"Texture::Paint\", \"\" {{ Type: \"TextureVideoClip\" FileName: \"{texture}\" RelativeFilename: \"{texture}\" }}\n"
            ));
            connections.push_str("C: \"OP\",4000,3000, \"DiffuseColor\"\n");
        }
        let fbx = format!(
            "; FBX 7.4.0 project file\nFBXHeaderExtension: {{ FBXHeaderVersion: 1003 FBXVersion: 7400 }}\nGlobalSettings: {{ Version: 1000 }}\nObjects: {{\n{objects}}}\nConnections: {{\n{connections}}}\n"
        );
        fs::write(path, fbx).unwrap();
    }

    pub(super) fn create_demo_like_project(root: &Path) {
        let assets = root.join("Assets");
        for path in [
            "scenes",
            "models",
            "textures",
            "icons",
            "scripts",
            "cameras",
            "prefabs",
            "environment",
        ] {
            fs::create_dir_all(assets.join(path)).unwrap();
        }
        fs::write(
            root.join("project.rveproject"),
            "id = \"test-game\"\nname = \"Test Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n[[scenes]]\npath = \"/scenes/second.rvescene\"\n[[scenes]]\npath = \"/scenes/draft.rvescene\"\nenabled = false\n",
        )
        .unwrap();
        fs::write(
            root.join("bootstrap.rves"),
            "WindowSizeX=640\nWindowSizeY=480\nAppIcon=/icons/app.png\n",
        )
        .unwrap();
        let scene_body = |extra_assets: &str, model: &str| {
            format!(
                "[[assets]]\npath = \"/models/shared.fbx\"\n[[assets]]\npath = \"/scripts/rotate.lua\"\n[[assets]]\npath = \"/cameras/shared.camerasettings\"\n{extra_assets}\n[[objects]]\nid = \"camera\"\n[objects.camera]\nsettings = \"/cameras/shared.camerasettings\"\n\n[[objects]]\nid = \"light\"\n[objects.directional_light]\nshadows = true\nshadow_distance = 80.0\nshadow_cascades = 4\n[[objects]]\nid = \"mesh\"\n[objects.mesh_renderer]\nmodel = \"{model}\"\n[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\n"
            )
        };
        let main_scene = scene_body(
            "[[assets]]\npath = \"/textures/main-only.png\"\n[[assets]]\npath = \"/prefabs/environment.rveprefab\"\n",
            "/models/shared.fbx",
        );
        fs::write(
            assets.join("scenes/main.rvescene"),
            format!(
                "{main_scene}\n[[instances]]\nid = \"sky\"\nprefab = \"/prefabs/environment.rveprefab\"\n"
            ),
        )
        .unwrap();
        fs::write(
            assets.join("scenes/second.rvescene"),
            scene_body(
                "[[assets]]\npath = \"/models/props.fbx\"\narchive = \"props\"\n",
                "/models/props.fbx",
            ),
        )
        .unwrap();
        fs::write(
            assets.join("scenes/draft.rvescene"),
            "this is intentionally invalid",
        )
        .unwrap();
        fs::write(
            assets.join("prefabs/environment.rveprefab"),
            "[[assets]]\npath = \"/environment/sky.hdr\"\n\n[[objects]]\nid = \"environment\"\n[objects.environment]\nhdr = \"/environment/sky.hdr\"\n",
        )
        .unwrap();
        fs::write(
            assets.join("environment/sky.hdr"),
            b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\x80\x80\x80\x81\x80\x80\x80\x81",
        )
        .unwrap();
        write_fixture_fbx(
            &assets.join("models/shared.fbx"),
            Some("../textures/shared.png"),
        );
        write_fixture_fbx(&assets.join("models/props.fbx"), None);
        write_fixture_fbx(&assets.join("models/draft.fbx"), None);
        fs::write(assets.join("scripts/rotate.lua"), "return {}\n").unwrap();
        fs::write(
            assets.join("cameras/shared.camerasettings"),
            "[projection]\nfov_deg = 55.0\n",
        )
        .unwrap();
        write_png(
            &assets.join("textures/shared.png"),
            2,
            2,
            [255, 64, 32, 255],
        );
        write_png(
            &assets.join("textures/main-only.png"),
            1,
            1,
            [96, 96, 96, 255],
        );
        write_png(&assets.join("textures/unused.png"), 1, 1, [0, 0, 0, 255]);
        write_png(&assets.join("icons/app.png"), 256, 256, [32, 96, 192, 255]);
    }

    fn file_assignments(registry: &ArchiveSetRegistry) -> BTreeMap<String, String> {
        registry
            .assets()
            .iter()
            .map(|(path, archive)| (format!("/{path}"), archive.clone()))
            .collect()
    }

    #[test]
    fn builds_and_validates_a_demo_like_archive_set_without_compiling_player() {
        let project_root = TempDir::create(&std::env::temp_dir(), "rve-build-project").unwrap();
        create_demo_like_project(project_root.path());
        let project = prepare_project(project_root.path()).unwrap();
        let archives_dir = project_root.path().join("out-assets");
        let registry = build_project_archives(&project, &archives_dir).unwrap();
        validate_staged_archives(&archives_dir).unwrap();

        assert_eq!(
            registry.archives(),
            ["base", "shared", "props", "scene-main"]
        );
        let catalog = ArchiveCatalog::open_directory(
            &archives_dir,
            ArchiveLimits::default(),
            CatalogLimits::default(),
        )
        .unwrap();
        let base = Archive::open_file(archives_dir.join("base.rvepak")).unwrap();
        let registry_path = VirtualPath::parse("/rve/assets.toml").unwrap();
        let registry_text = String::from_utf8(base.read_all(&registry_path).unwrap()).unwrap();
        let base_registry = ArchiveSetRegistry::parse(&registry_text).unwrap();
        assert_eq!(base_registry, registry);
        let settings_path = VirtualPath::parse("/rve/project.toml").unwrap();
        assert!(
            base.read_all(&settings_path)
                .unwrap()
                .windows(4)
                .any(|value| value == b"name")
        );

        let mut ids = BTreeMap::new();
        for name in registry.archives() {
            let archive = Archive::open_file(archives_dir.join(format!("{name}.rvepak"))).unwrap();
            ids.insert(name.clone(), archive.archive_id());
            drop(archive);
        }
        registry
            .validate_against_catalog(&catalog, &ids, &archives_dir)
            .unwrap();
        catalog.validate().unwrap();

        let assignments = file_assignments(&registry);
        assert_eq!(assignments["/models/shared.fbx"], "shared");
        assert_eq!(assignments["/textures/shared.png"], "shared");
        assert_eq!(assignments["/models/props.fbx"], "props");
        assert_eq!(assignments["/icons/app.png"], "base");
        assert_eq!(assignments["/textures/main-only.png"], "scene-main");
        assert_eq!(assignments["/environment/sky.hdr"], "scene-main");
        assert_eq!(assignments["/prefabs/environment.rveprefab"], "scene-main");
        assert_eq!(assignments["/scenes/main.rvescene"], "base");
        assert!(!assignments.contains_key("/scenes/draft.rvescene"));
        assert!(!assignments.contains_key("/models/draft.fbx"));
        assert!(!assignments.contains_key("/textures/unused.png"));
        assert!(
            !catalog
                .read_all(&VirtualPath::parse("/icons/app.png").unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_invalid_camera_settings_during_project_preparation() {
        let project_root =
            TempDir::create(&std::env::temp_dir(), "rve-build-camera-settings").unwrap();
        create_demo_like_project(project_root.path());
        fs::write(
            project_root
                .path()
                .join("Assets/cameras/shared.camerasettings"),
            "[projection]\nfov_deg = 180.0\n",
        )
        .unwrap();
        let error = prepare_project(project_root.path()).unwrap_err();
        assert!(error.to_string().contains("/cameras/shared.camerasettings"));
        assert!(error.to_string().contains("0 и 180"));
    }

    /// Записывает `/cameras/post.postprocess` и подключает его к камере главной сцены.
    fn attach_post_process(project: &Path, post: &str) {
        let assets = project.join("Assets");
        fs::write(assets.join("cameras/post.postprocess"), post).unwrap();
        let scene = assets.join("scenes/main.rvescene");
        let source = fs::read_to_string(&scene).unwrap();
        let patched = source
            .replacen(
                "[[assets]]\npath = \"/cameras/shared.camerasettings\"",
                "[[assets]]\npath = \"/cameras/shared.camerasettings\"\n[[assets]]\npath = \"/cameras/post.postprocess\"",
                1,
            )
            .replacen(
                "settings = \"/cameras/shared.camerasettings\"",
                "settings = \"/cameras/shared.camerasettings\"\npost_process = \"/cameras/post.postprocess\"",
                1,
            );
        assert_ne!(
            patched, source,
            "fixture scene must reference the post process file"
        );
        fs::write(&scene, patched).unwrap();
    }

    const LUT_POST: &str = "[color_grading]\nenabled = true\nlut = \"/luts/film.cube\"\n";

    #[test]
    fn packs_the_color_lut_of_a_postprocess_with_its_owner() {
        let project_root = TempDir::create(&std::env::temp_dir(), "rve-build-lut").unwrap();
        create_demo_like_project(project_root.path());
        attach_post_process(project_root.path(), LUT_POST);
        let luts = project_root.path().join("Assets/luts");
        fs::create_dir_all(&luts).unwrap();
        fs::write(
            luts.join("film.cube"),
            "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
        )
        .unwrap();
        fs::write(luts.join("unused.cube"), "LUT_3D_SIZE 2\n").unwrap();
        let project = prepare_project(project_root.path()).unwrap();
        let archives_dir = project_root.path().join("out-assets");
        let registry = build_project_archives(&project, &archives_dir).unwrap();
        let assignments = file_assignments(&registry);
        assert_eq!(
            assignments["/luts/film.cube"],
            assignments["/cameras/post.postprocess"]
        );
        assert!(!assignments.contains_key("/luts/unused.cube"));
    }

    #[test]
    fn rejects_missing_or_invalid_color_lut_during_project_preparation() {
        for (name, contents) in [("missing", None), ("invalid", Some("LUT_1D_SIZE 2\n"))] {
            let project_root =
                TempDir::create(&std::env::temp_dir(), &format!("rve-build-lut-{name}")).unwrap();
            create_demo_like_project(project_root.path());
            attach_post_process(project_root.path(), LUT_POST);
            if let Some(contents) = contents {
                let luts = project_root.path().join("Assets/luts");
                fs::create_dir_all(&luts).unwrap();
                fs::write(luts.join("film.cube"), contents).unwrap();
            }
            let error = prepare_project(project_root.path()).unwrap_err();
            assert!(
                error.to_string().contains("/luts/film.cube"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn rejects_invalid_postprocess_during_project_preparation() {
        let project_root = TempDir::create(&std::env::temp_dir(), "rve-build-postprocess").unwrap();
        create_demo_like_project(project_root.path());
        attach_post_process(
            project_root.path(),
            "[bloom]\nenabled = true\nintensity = -1.0\n",
        );
        let error = prepare_project(project_root.path()).unwrap_err();
        assert!(
            error.to_string().contains("/cameras/post.postprocess"),
            "{error}"
        );
        assert!(error.to_string().contains("intensity"), "{error}");
    }
}

#[cfg(test)]
mod screen_shader_asset_tests {
    use super::tests::create_demo_like_project;
    use super::*;
    use crate::project::prepare_project;

    /// Добавляет `.glsl` в `[[assets]]` демо-сцены и создаёт файл с заданным текстом.
    fn declare_screen_shader(project_root: &Path, path: &str, source: &str) {
        let assets = project_root.join("Assets");
        let file = assets.join(path.trim_start_matches('/'));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, source).unwrap();
        let scene = assets.join("scenes/main.rvescene");
        let text = fs::read_to_string(&scene).unwrap();
        let patched = text.replacen(
            "[[assets]]",
            &format!("[[assets]]\npath = \"{path}\"\n[[assets]]"),
            1,
        );
        assert_ne!(patched, text);
        fs::write(&scene, patched).unwrap();
    }

    #[test]
    fn valid_screen_shader_passes_project_preparation_and_invalid_one_names_its_file() {
        let good_root = TempDir::create(&std::env::temp_dir(), "rve-build-glsl-good").unwrap();
        create_demo_like_project(good_root.path());
        declare_screen_shader(
            good_root.path(),
            "/fx/identity.glsl",
            "vec3 rve_shade(vec2 uv) { return rve_sample(uv).rgb; }\n",
        );
        prepare_project(good_root.path()).expect("корректный экранный шейдер собирается");

        let bad_root = TempDir::create(&std::env::temp_dir(), "rve-build-glsl-bad").unwrap();
        create_demo_like_project(bad_root.path());
        declare_screen_shader(
            bad_root.path(),
            "/fx/bad.glsl",
            "vec3 rve_shade(vec2 uv) {\n    return rve_sample(uv).rgb\n}\n",
        );
        let error = prepare_project(bad_root.path()).unwrap_err();
        assert!(error.to_string().contains("/fx/bad.glsl"), "{error}");
        assert!(error.to_string().contains("строка"), "{error}");
    }
}

#[cfg(test)]
mod prefab_list_tests {
    use super::build_project_archives;
    use crate::project::prepare_project;
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use engine_pak::Archive;
    use engine_project::PrefabCatalogFile;
    use std::fs;
    use std::path::Path;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join("Assets").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Проект с одной сценой без экземпляров, префабом `/prefabs/tower.rveprefab` и
    /// необязательным `prefabs.rveprefabs`.
    fn project(root: &Path, list: Option<&str>) {
        fs::write(
            root.join("project.rveproject"),
            "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n[[scenes]]\npath = \"/scenes/main.rvescene\"\n",
        )
        .unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        if let Some(list) = list {
            fs::write(root.join("prefabs.rveprefabs"), list).unwrap();
        }
        write(
            root,
            "scenes/main.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        );
        write(root, "textures/tower.png", "texture");
        write(
            root,
            "prefabs/tower.rveprefab",
            "[[assets]]\npath = \"/textures/tower.png\"\n[[objects]]\nid = \"tower\"\n",
        );
    }

    fn base_text(archives_dir: &Path, path: &str) -> String {
        let base = Archive::open_file(archives_dir.join("base.rvepak")).unwrap();
        let bytes = base.read_all(&VirtualPath::parse(path).unwrap()).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn release_base_carries_the_prefab_list_and_its_archive_is_in_the_set() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-prefab-list").unwrap();
        project(
            root.path(),
            Some(
                "[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\nmax_instances = 3\n",
            ),
        );
        let build = prepare_project(root.path()).unwrap();
        let archives_dir = root.path().join("out-assets");
        let registry = build_project_archives(&build, &archives_dir).unwrap();

        let text = base_text(&archives_dir, "/rve/prefabs.toml");
        assert_eq!(PrefabCatalogFile::parse(&text).unwrap(), build.prefabs);
        assert!(registry.archives().iter().any(|name| name == "towers"));
        assert!(archives_dir.join("towers.rvepak").is_file());
    }

    #[test]
    fn release_base_carries_an_empty_prefab_list_when_the_project_has_no_file() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-prefab-list").unwrap();
        project(root.path(), None);
        let build = prepare_project(root.path()).unwrap();
        let archives_dir = root.path().join("out-assets");
        build_project_archives(&build, &archives_dir).unwrap();

        let text = base_text(&archives_dir, "/rve/prefabs.toml");
        assert!(PrefabCatalogFile::parse(&text).unwrap().prefabs.is_empty());
    }

    #[test]
    fn rejects_assets_at_the_reserved_prefab_list_path() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-build-prefab-list").unwrap();
        project(root.path(), None);
        write(root.path(), "rve/prefabs.toml", "prefabs = []\n");
        fs::write(
            root.path().join("Assets/scenes/main.rvescene"),
            "[[assets]]\npath = \"/rve/prefabs.toml\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let build = prepare_project(root.path()).unwrap();
        let error = build_project_archives(&build, &root.path().join("out-assets")).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("/rve/prefabs.toml"), "{error}");
    }
}
