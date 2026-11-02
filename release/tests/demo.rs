use engine_assets::Assets;
use engine_project::ProjectSettings;
use engine_resources::{ResourceConfig, ResourceHandle, ResourceSystem, SourceMode};
use engine_scene::{SceneDocument, World};
use engine_script::{ScriptEventLevel, ScriptHost, ScriptTime};
use std::collections::BTreeMap;
use std::path::Path;

fn text(handle: &ResourceHandle, path: &str) -> String {
    String::from_utf8(handle.read_all(path).unwrap().wait().unwrap()).unwrap()
}

fn check_demo(root: &Path, mode: SourceMode) {
    let mut resources = ResourceSystem::start(ResourceConfig {
        root: root.to_owned(),
        source_mode: mode,
        ..ResourceConfig::default()
    })
    .unwrap();
    let handle = resources.handle();
    let scene = SceneDocument::parse(
        "/scenes/neko.rvescene",
        &text(&handle, "/scenes/neko.rvescene"),
    )
    .unwrap();
    let assets = Assets::with_resources(handle.clone());
    let model = assets.load_model("/models/neko.fbx").unwrap();
    let (min, max) = model.bounds().unwrap();
    println!("Neko bounds (meters): {min:?} .. {max:?}");
    assert!(model.meshes.iter().all(|mesh| !mesh.indices.is_empty()));
    let texture = assets.load_texture("/models/tx.png").unwrap();
    assert_eq!((texture.width, texture.height), (2000, 2000));
    let mut world = World::from_document(&scene).unwrap();
    assert_eq!(
        world.object_by_id("neko").unwrap().parent,
        Some(world.object_by_id("turntable").unwrap().handle)
    );
    let renderer = world
        .object_by_id("neko")
        .unwrap()
        .mesh_renderer
        .as_ref()
        .unwrap();
    assert_eq!(renderer.texture.as_ref().unwrap().as_str(), "models/tx.png");
    let sources = BTreeMap::from([(
        "scripts/rotate.lua".to_owned(),
        text(&handle, "/scripts/rotate.lua"),
    )]);
    let mut host = ScriptHost::load(&world, "neko", &sources, None).unwrap();
    let initial = world.object_by_id("turntable").unwrap().transform;
    let initial_frame = world.build_frame().0;
    for (frame, dt, expected) in [
        (1, 0.0, 0.0),
        (2, 1.0, 30.0),
        (3, 11.0, 0.0),
        (4, 0.5, 15.0),
    ] {
        host.update(
            &mut world,
            dt,
            ScriptTime {
                elapsed: frame as f64,
                frame,
            },
        );
        let transform = world.object_by_id("turntable").unwrap().transform;
        assert!((transform.rotation_deg.y - expected).abs() < 0.001);
        assert_eq!(transform.position, initial.position);
        assert_eq!(transform.scale, initial.scale);
        assert_eq!(transform.rotation_deg.x, initial.rotation_deg.x);
        assert_eq!(transform.rotation_deg.z, initial.rotation_deg.z);
        let (scene_frame, warnings) = world.build_frame();
        assert!(warnings.is_empty());
        if expected != 0.0 {
            assert_ne!(scene_frame.meshes, initial_frame.meshes);
        }
    }
    host.destroy(&mut world, ScriptTime::default());
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Info);
    assert_eq!(events[0].message, "Rotate started");
    resources.shutdown().unwrap();
}

#[test]
fn source_model_texture_and_rotation() {
    check_demo(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../project/Assets"),
        SourceMode::Loose,
    );
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../project/Assets/scenes/main.rvescene"),
    )
    .unwrap();
    SceneDocument::parse("/scenes/main.rvescene", &source).unwrap();
}

#[test]
fn packed_model_texture_and_rotation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../NekoDemo/Assets");
    assert!(
        std::fs::read_dir(&root)
            .unwrap()
            .all(|file| { file.unwrap().path().extension().unwrap() == "rvepak" })
    );
    check_demo(&root, SourceMode::Registry);
    let mut resources = ResourceSystem::start(ResourceConfig {
        root,
        source_mode: SourceMode::Registry,
        ..ResourceConfig::default()
    })
    .unwrap();
    let project = ProjectSettings::parse(&text(&resources.handle(), "/rve/project.toml")).unwrap();
    assert_eq!(project.id, "neko-demo");
    for path in [
        "models/neko.fbx",
        "models/tx.png",
        "scripts/rotate.lua",
        "scenes/neko.rvescene",
    ] {
        let packed = resources.handle().read_all(path).unwrap().wait().unwrap();
        let source = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../project/Assets")
                .join(path),
        )
        .unwrap();
        assert_eq!(packed, source, "archive changed {path}");
    }
    resources.shutdown().unwrap();
}
