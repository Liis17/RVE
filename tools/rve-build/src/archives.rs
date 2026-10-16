use crate::error::BuildError;
use crate::project::ProjectBuild;
use crate::temporary::TempDir;
use engine_files::VirtualPath;
use engine_pak::{
    ArchiveSetRegistry, BuildCompression, BuildFileInput, BuildOptions, ProgrammaticArchive,
    build_archive_set,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub(crate) fn build_project_archives(
    project: &ProjectBuild,
    output: &Path,
) -> Result<ArchiveSetRegistry, BuildError> {
    let project_settings_path = VirtualPath::parse("/rve/project.toml").expect("constant path");
    let registry_path = VirtualPath::parse("/rve/assets.toml").expect("constant path");
    if project.assignments.contains_key(&project_settings_path)
        || project.assignments.contains_key(&registry_path)
    {
        return Err(BuildError::input(
            "Assets не может содержать зарезервированные /rve/project.toml или /rve/assets.toml",
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

    fn create_demo_like_project(root: &Path) {
        let assets = root.join("Assets");
        for path in ["scenes", "models", "textures", "icons", "scripts"] {
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
                "[[assets]]\npath = \"/models/shared.fbx\"\n[[assets]]\npath = \"/scripts/rotate.lua\"\n{extra_assets}\n[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n[[objects]]\nid = \"light\"\n[objects.directional_light]\nshadows = true\nshadow_radius = 8.0\n[[objects]]\nid = \"mesh\"\n[objects.mesh_renderer]\nmodel = \"{model}\"\n[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\n"
            )
        };
        fs::write(
            assets.join("scenes/main.rvescene"),
            scene_body(
                "[[assets]]\npath = \"/textures/main-only.png\"\n",
                "/models/shared.fbx",
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
        write_fixture_fbx(
            &assets.join("models/shared.fbx"),
            Some("../textures/shared.png"),
        );
        write_fixture_fbx(&assets.join("models/props.fbx"), None);
        write_fixture_fbx(&assets.join("models/draft.fbx"), None);
        fs::write(assets.join("scripts/rotate.lua"), "return {}\n").unwrap();
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
}
