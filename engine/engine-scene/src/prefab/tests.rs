use super::*;
use crate::World;
use crate::network::{NetAuthority, NetworkTransformComponent};

const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

/// Набор префабов в памяти; загрузчик разбирает их как `.rveprefab`.
struct Library(BTreeMap<String, String>);

impl Library {
    fn new(entries: &[(&str, &str)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
                .collect(),
        )
    }

    fn expand(&self, document: &SceneDocument) -> Result<Expansion, SceneError> {
        expand(document, &mut |path| self.load(path))
    }

    fn load(&self, path: &VirtualPath) -> Result<SceneDocument, SceneError> {
        let label = format!("/{}", path.as_str());
        let text = self
            .0
            .get(path.as_str())
            .ok_or_else(|| SceneError::new(&label, None, "не найден"))?;
        SceneDocument::parse_prefab(label, text)
    }
}

fn scene(text: &str) -> SceneDocument {
    SceneDocument::parse("scene.rvescene", text).unwrap()
}

const DOOR: &str = r#"
[[assets]]
path = "/models/door.fbx"
[[assets]]
path = "/materials/frame.rvemat"
[[assets]]
path = "/scripts/rotate.lua"
[[objects]]
id = "frame"
name = "Frame"
[objects.transform]
position = [0.0, 0.0, 0.0]
[objects.mesh_renderer]
model = "/models/door.fbx"
materials = ["", "/materials/frame.rvemat"]
[objects.mesh_renderer.material]
metallic = 0.2
roughness = 0.4
[[objects]]
id = "handle"
parent = "frame"
[objects.transform]
position = [0.5, 1.0, 0.0]
rotation = [0.0, 45.0, 0.0]
[objects.mesh_renderer]
model = "/models/door.fbx"
[[objects.lite_scripts]]
path = "/scripts/rotate.lua"
params = { speed = 10.0, axis = "y" }
"#;

fn door_scene(instance_body: &str) -> String {
    format!(
        "[[assets]]\npath = \"/prefabs/door.rveprefab\"\n[[assets]]\npath = \"/textures/gold.png\"\n[[assets]]\npath = \"/models/gold.fbx\"\n[[assets]]\npath = \"/materials/gold.rvemat\"\n{CAMERA}[[objects]]\nid = \"room\"\n[[instances]]\nid = \"door1\"\nprefab = \"/prefabs/door.rveprefab\"\n{instance_body}"
    )
}

fn object<'a>(document: &'a SceneDocument, id: &str) -> &'a SceneObject {
    document
        .objects
        .iter()
        .find(|object| object.id == id)
        .unwrap_or_else(|| panic!("{id} missing"))
}

#[test]
fn one_level_instance_gets_mapped_ids_parents_and_instance_fields() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let document = scene(&door_scene(
        "name = \"Door\"\nparent = \"room\"\n[instances.transform]\nposition = [2.0, 0.0, 0.0]\n",
    ));
    let expansion = library.expand(&document).unwrap();
    let expanded = &expansion.document;
    assert!(expanded.is_expanded());
    let ids: Vec<_> = expanded
        .objects
        .iter()
        .map(|object| object.id.as_str())
        .collect();
    assert_eq!(ids, ["camera", "room", "door1", "door1.handle"]);
    let root = object(expanded, "door1");
    assert_eq!(root.name.as_deref(), Some("Door"));
    assert_eq!(root.parent.as_deref(), Some("room"));
    assert_eq!(root.transform.position, [2.0, 0.0, 0.0]);
    assert_eq!(
        object(expanded, "door1.handle").parent.as_deref(),
        Some("door1")
    );
    assert_eq!(
        expansion.prefabs,
        [VirtualPath::parse("prefabs/door.rveprefab").unwrap()]
    );
    assert_eq!(
        expansion.origins["door1.handle"],
        ObjectOrigin {
            instance: "door1".into(),
            target: "handle".into()
        }
    );
    assert_eq!(expansion.origins["door1"].target, "frame");
    assert!(!expansion.origins.contains_key("room"));
    let paths: Vec<_> = expanded
        .assets
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    assert!(paths.contains(&"models/door.fbx"));
    assert!(paths.contains(&"scripts/rotate.lua"));
    let mut world = World::from_document(expanded).unwrap();
    let handle = world.handle("door1.handle").unwrap();
    assert!(world.world_matrix(handle).is_some());
    world.build_frame();
}

#[test]
fn prefab_object_references_are_local_and_are_mapped_for_instances_and_spawns() {
    let prefab = r#"
[[assets]]
path = "/scripts/ref.lua"
[[objects]]
id = "root"
[[objects.lite_scripts]]
path = "/scripts/ref.lua"
params = { target = { object = "child" } }
[[objects]]
id = "child"
parent = "root"
"#;
    let library = Library::new(&[("prefabs/ref.rveprefab", prefab)]);
    let prefab_document = SceneDocument::parse_prefab("/prefabs/ref.rveprefab", prefab).unwrap();
    let expanded = library
        .expand(&scene(
            &format!("{CAMERA}[[assets]]\npath=\"/prefabs/ref.rveprefab\"\n[[instances]]\nid=\"door\"\nprefab=\"/prefabs/ref.rveprefab\"\n"),
        ))
        .unwrap();
    assert_eq!(
        object(&expanded.document, "door").lite_scripts[0].params["target"],
        ParamValue::Object("door.child".into())
    );

    let template = load_template(&library, "prefabs/ref.rveprefab").unwrap();
    let document = scene(CAMERA);
    let mut world = World::from_document(&document).unwrap();
    let reserved = (0..template.objects().len())
        .map(|_| world.reserve_object())
        .collect();
    world
        .spawn(
            reserved,
            &template,
            crate::SpawnOptions {
                id: Some("spawned".into()),
                ..crate::SpawnOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        world.object_by_id("spawned").unwrap().lite_scripts[0].params["target"],
        ParamValue::Object("spawned.child".into())
    );

    let outside = prefab.replace("object = \"child\"", "object = \"outside\"");
    let error = SceneDocument::parse_prefab("/prefabs/ref.rveprefab", &outside).unwrap_err();
    assert!(
        error.message.contains("outside the prefab") || error.message.contains("пределы префаба")
    );
    assert!(prefab_document.is_prefab());
}

#[test]
fn prefab_object_reference_override_keeps_a_colliding_scene_object_id() {
    let prefab = r#"
[[assets]]
path = "/scripts/ref.lua"
[[objects]]
id = "root"
[[objects.lite_scripts]]
path = "/scripts/ref.lua"
params = { target = { object = "player" } }
[[objects]]
id = "player"
parent = "root"
"#;
    let library = Library::new(&[("prefabs/ref.rveprefab", prefab)]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/prefabs/ref.rveprefab\"\n{CAMERA}[[objects]]\nid = \"player\"\n[[instances]]\nid = \"inst\"\nprefab = \"/prefabs/ref.rveprefab\"\n[[instances.overrides]]\ntarget = \"root\"\nlite_params = [{{ path = \"/scripts/ref.lua\", params = {{ target = {{ object = \"player\" }} }} }}]\n"
    ));

    let expanded = library.expand(&document).unwrap().document;

    assert_eq!(
        object(&expanded, "inst").lite_scripts[0].params["target"],
        ParamValue::Object("player".into())
    );
}

#[test]
fn prefab_object_reference_override_accepts_a_full_instance_path() {
    let prefab = r#"
[[assets]]
path = "/scripts/ref.lua"
[[objects]]
id = "root"
[[objects.lite_scripts]]
path = "/scripts/ref.lua"
params = { target = { object = "child" } }
[[objects]]
id = "child"
parent = "root"
"#;
    let library = Library::new(&[("prefabs/ref.rveprefab", prefab)]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/prefabs/ref.rveprefab\"\n{CAMERA}[[instances]]\nid = \"inst\"\nprefab = \"/prefabs/ref.rveprefab\"\n[[instances.overrides]]\ntarget = \"root\"\nlite_params = [{{ path = \"/scripts/ref.lua\", params = {{ target = {{ object = \"inst.child\" }} }} }}]\n"
    ));

    let expanded = library.expand(&document).unwrap().document;
    let world = World::from_document(&expanded).unwrap();

    assert!(world.handle("inst.child").is_some());
    assert_eq!(
        object(&expanded, "inst").lite_scripts[0].params["target"],
        ParamValue::Object("inst.child".into())
    );
}

#[test]
fn nested_prefabs_produce_dotted_ids_and_outer_overrides_win() {
    let lock = "[[assets]]\npath = \"/models/door.fbx\"\n[[objects]]\nid = \"body\"\n[[objects]]\nid = \"bolt\"\nparent = \"body\"\nname = \"Bolt\"\n[objects.mesh_renderer]\nmodel = \"/models/door.fbx\"\n";
    let door = "[[assets]]\npath = \"/prefabs/lock.rveprefab\"\n[[objects]]\nid = \"frame\"\n[[instances]]\nid = \"lock\"\nparent = \"frame\"\nprefab = \"/prefabs/lock.rveprefab\"\n[[instances.overrides]]\ntarget = \"bolt\"\nname = \"Inner\"\ntransform = { position = [1.0, 2.0, 3.0] }\n";
    let library = Library::new(&[
        ("prefabs/door.rveprefab", door),
        ("prefabs/lock.rveprefab", lock),
    ]);
    let document = scene(&door_scene(
        "[[instances.overrides]]\ntarget = \"lock.bolt\"\nname = \"Outer\"\n",
    ));
    let expansion = library.expand(&document).unwrap();
    let expanded = &expansion.document;
    let bolt = object(expanded, "door1.lock.bolt");
    assert_eq!(bolt.name.as_deref(), Some("Outer"));
    assert_eq!(bolt.transform.position, [1.0, 2.0, 3.0]);
    assert_eq!(bolt.parent.as_deref(), Some("door1.lock"));
    assert_eq!(
        object(expanded, "door1.lock").parent.as_deref(),
        Some("door1")
    );
    assert_eq!(expansion.prefabs.len(), 2);
    assert_eq!(expansion.origins["door1.lock.bolt"].target, "lock.bolt");
}

#[test]
fn overrides_change_only_the_given_fields() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let document = scene(&door_scene(
        r#"[[instances.overrides]]
target = "handle"
name = "Gold handle"
transform = { position = [0.1, 1.0, 0.0] }
mesh_renderer = { model = "/models/gold.fbx", texture = "/textures/gold.png", materials = ["", "/materials/gold.rvemat"] }
lite_params = [{ path = "/scripts/rotate.lua", params = { speed = 90.0 } }]
[[instances.overrides]]
target = "frame"
mesh_renderer = { material = { metallic = 0.9, emissive = [1.5, 0.2, 0.0], opacity = 0.4, alpha_mode = "mask", alpha_cutoff = 0.25, occlusion_strength = 0.7, double_sided = false } }
"#,
    ));
    let expanded = library.expand(&document).unwrap().document;
    let handle = object(&expanded, "door1.handle");
    assert_eq!(handle.name.as_deref(), Some("Gold handle"));
    assert_eq!(handle.transform.position, [0.1, 1.0, 0.0]);
    assert_eq!(handle.transform.rotation, [0.0, 45.0, 0.0]);
    let mesh = handle.mesh_renderer.as_ref().unwrap();
    assert_eq!(mesh.model.as_str(), "models/gold.fbx");
    assert_eq!(mesh.texture.as_ref().unwrap().as_str(), "textures/gold.png");
    assert_eq!(mesh.materials.len(), 2);
    assert!(mesh.materials[0].is_none());
    assert_eq!(
        mesh.materials[1].as_ref().unwrap().as_str(),
        "materials/gold.rvemat"
    );
    let params = &handle.lite_scripts[0].params;
    assert_eq!(params["speed"], ParamValue::Float(90.0));
    assert_eq!(params["axis"], ParamValue::String("y".into()));
    let material = object(&expanded, "door1")
        .mesh_renderer
        .as_ref()
        .unwrap()
        .material
        .as_ref()
        .unwrap();
    assert_eq!(material.metallic, Some(0.9));
    assert_eq!(material.roughness, Some(0.4));
    assert_eq!(material.emissive, Some([1.5, 0.2, 0.0]));
    assert_eq!(material.opacity, Some(0.4));
    assert_eq!(material.alpha_mode, Some(crate::MaterialAlphaMode::Mask));
    assert_eq!(material.alpha_cutoff, Some(0.25));
    assert_eq!(material.occlusion_strength, Some(0.7));
    assert_eq!(material.double_sided, Some(false));
}

#[test]
fn material_slot_override_requires_a_declared_rvemat_asset() {
    let missing = door_scene(
        "[[instances.overrides]]\ntarget = \"handle\"\nmesh_renderer = { materials = [\"/materials/missing.rvemat\"] }\n",
    );
    let error = SceneDocument::parse("scene.rvescene", &missing).unwrap_err();
    assert!(error.message.contains("отсутствует в [[assets]]"));

    let wrong_extension = door_scene(
        "[[instances.overrides]]\ntarget = \"handle\"\nmesh_renderer = { materials = [\"/materials/gold.toml\"] }\n",
    )
    .replace("/materials/gold.rvemat", "/materials/gold.toml");
    let error = SceneDocument::parse("scene.rvescene", &wrong_extension).unwrap_err();
    assert!(error.message.contains("расширение .rvemat"));
}

#[test]
fn alpha_mode_override_removes_inherited_mask_cutoff() {
    let alpha_door = DOOR.replace(
        "roughness = 0.4",
        "roughness = 0.4\nalpha_mode = \"mask\"\nalpha_cutoff = 0.4",
    );
    let library = Library::new(&[("prefabs/door.rveprefab", &alpha_door)]);
    let document = scene(&door_scene(
        "[[instances.overrides]]\ntarget = \"frame\"\nmesh_renderer = { material = { alpha_mode = \"blend\" } }\n",
    ));
    let expanded = library.expand(&document).unwrap().document;
    let material = object(&expanded, "door1")
        .mesh_renderer
        .as_ref()
        .unwrap()
        .material
        .as_ref()
        .unwrap();
    assert_eq!(material.alpha_mode, Some(crate::MaterialAlphaMode::Blend));
    assert_eq!(material.alpha_cutoff, None);
}

#[test]
fn instance_fields_win_over_a_root_override() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let document = scene(&door_scene(
        "name = \"Instance\"\n[[instances.overrides]]\ntarget = \"frame\"\nname = \"Override\"\n",
    ));
    let expanded = library.expand(&document).unwrap().document;
    assert_eq!(object(&expanded, "door1").name.as_deref(), Some("Instance"));
}

#[test]
fn empty_instance_and_object_override_names_clear_inherited_names() {
    let prefab = "[[objects]]\nid = \"root\"\nname = \"Root\"\n[objects.camera]\n[[objects]]\nid = \"handle\"\nparent = \"root\"\nname = \"Handle\"\n";
    let library = Library::new(&[("prefabs/door.rveprefab", prefab)]);
    let document = scene(
        "[[assets]]\npath = \"/prefabs/door.rveprefab\"\n[[instances]]\nid = \"door\"\nprefab = \"/prefabs/door.rveprefab\"\nname = \"\"\n[[instances.overrides]]\ntarget = \"handle\"\nname = \"\"\n",
    );
    let serialized = document.to_toml().unwrap();
    let reparsed = scene(&serialized);
    let expanded = library.expand(&reparsed).unwrap().document;

    assert_eq!(object(&expanded, "door").name, None);
    assert_eq!(object(&expanded, "door.handle").name, None);
}

#[test]
fn camera_and_light_overrides_replace_whole_components_and_heavy_params_merge() {
    let prefab = "[[assets]]\npath = \"/cameras/default.camerasettings\"\n[[objects]]\nid = \"rig\"\n[objects.camera]\nsettings = \"/cameras/default.camerasettings\"\nactive = true\n[[objects]]\nid = \"sun\"\nparent = \"rig\"\n[objects.directional_light]\nshadows = false\n[[objects.heavy_scripts]]\nname = \"Bobber\"\nparams = { speed = 1.0, amplitude = 0.5 }\n";
    let library = Library::new(&[("prefabs/rig.rveprefab", prefab)]);
    let document = scene(
        "[[assets]]\npath = \"/prefabs/rig.rveprefab\"\n[[assets]]\npath = \"/cameras/override.camerasettings\"\n[[instances]]\nid = \"rig\"\nprefab = \"/prefabs/rig.rveprefab\"\n[[instances.overrides]]\ntarget = \"rig\"\ncamera = { active = false, settings = \"/cameras/override.camerasettings\" }\n[[instances.overrides]]\ntarget = \"sun\"\ndirectional_light = { color = [1.0, 0.5, 0.5] }\nheavy_params = [{ name = \"Bobber\", params = { speed = 2.0 } }]\n",
    );
    let expanded = library.expand(&document).unwrap().document;
    let camera = object(&expanded, "rig").camera.as_ref().unwrap();
    assert!(!camera.active);
    assert_eq!(
        camera.settings.as_ref().unwrap().as_str(),
        "cameras/override.camerasettings"
    );
    let sun = object(&expanded, "rig.sun");
    let light = sun.directional_light.unwrap();
    assert_eq!(light.color, [1.0, 0.5, 0.5]);
    assert!(!light.shadows);
    assert_eq!(light.shadow_distance, 100.0);
    assert_eq!(light.shadow_cascades, 4);
    let params = &sun.heavy_scripts[0].params;
    assert_eq!(params["speed"], ParamValue::Float(2.0));
    assert_eq!(params["amplitude"], ParamValue::Float(0.5));

    let missing_camera_settings = SceneDocument::parse(
        "scene.rvescene",
        "[[assets]]\npath = \"/prefabs/rig.rveprefab\"\n[[instances]]\nid = \"rig\"\nprefab = \"/prefabs/rig.rveprefab\"\n[[instances.overrides]]\ntarget = \"rig\"\ncamera = { settings = \"/cameras/missing.camerasettings\" }\n",
    )
    .unwrap_err();
    assert!(missing_camera_settings.message.contains("camerasettings"));
}

#[test]
fn point_and_spot_prefab_overrides_replace_whole_components() {
    let prefab = "[[objects]]\nid = \"lamp\"\n[objects.point_light]\ncolor = [0.2, 0.4, 0.8]\nintensity = 1.0\nrange = 4.0\nshadows = true\n[objects.spot_light]\ncolor = [0.8, 0.4, 0.2]\nintensity = 2.0\nrange = 6.0\ninner_angle_deg = 12.0\nouter_angle_deg = 24.0\nshadows = true\n";
    let library = Library::new(&[("prefabs/lamp.rveprefab", prefab)]);
    let document = scene(
        "[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n[[instances.overrides]]\ntarget = \"lamp\"\npoint_light = { intensity = 4.0 }\nspot_light = { outer_angle_deg = 42.0 }\n",
    );
    let expanded = library.expand(&document).unwrap().document;
    let lamp = object(&expanded, "lamp");
    assert_eq!(
        lamp.point_light,
        Some(crate::PointLightComponent {
            color: [1.0; 3],
            intensity: 4.0,
            range: 10.0,
            shadows: false,
        })
    );
    assert_eq!(
        lamp.spot_light,
        Some(crate::SpotLightComponent {
            color: [1.0; 3],
            intensity: 1.0,
            range: 10.0,
            inner_angle_deg: 20.0,
            outer_angle_deg: 42.0,
            shadows: false,
        })
    );
}

#[test]
fn physics_prefab_overrides_replace_whole_components() {
    let prefab = r#"
[[objects]]
id = "box"
[objects.collider]
friction = 0.2
[objects.rigid_body]
kind = "kinematic"
mass = 5.0
"#;
    let library = Library::new(&[("prefabs/box.rveprefab", prefab)]);
    let document = scene(
        r#"
[[assets]]
path = "/prefabs/box.rveprefab"
[[objects]]
id = "camera"
[objects.camera]
[[instances]]
id = "crate"
prefab = "/prefabs/box.rveprefab"
[[instances.overrides]]
target = "box"
collider = { friction = 0.8 }
rigid_body = { mass = 2.0 }
"#,
    );
    let expanded = library.expand(&document).unwrap().document;
    let crate_object = object(&expanded, "crate");
    assert_eq!(
        crate_object.collider,
        Some(crate::ColliderComponent {
            friction: 0.8,
            ..crate::ColliderComponent::default()
        })
    );
    assert_eq!(
        crate_object.rigid_body,
        Some(crate::RigidBodyComponent {
            mass: 2.0,
            ..crate::RigidBodyComponent::default()
        })
    );

    let prefab = r#"
[[objects]]
id = "player"
[objects.character_controller]
radius = 0.6
"#;
    let library = Library::new(&[("prefabs/player.rveprefab", prefab)]);
    let document = scene(
        r#"
[[assets]]
path = "/prefabs/player.rveprefab"
[[objects]]
id = "camera"
[objects.camera]
[[instances]]
id = "player"
prefab = "/prefabs/player.rveprefab"
[[instances.overrides]]
target = "player"
character_controller = { radius = 0.4 }
"#,
    );
    let expanded = library.expand(&document).unwrap().document;
    assert_eq!(
        object(&expanded, "player").character_controller,
        Some(crate::CharacterControllerComponent {
            radius: 0.4,
            ..crate::CharacterControllerComponent::default()
        })
    );
}

#[test]
fn camera_only_in_a_prefab_satisfies_the_scene() {
    let prefab = "[[objects]]\nid = \"eye\"\n[objects.camera]\n";
    let library = Library::new(&[("prefabs/eye.rveprefab", prefab)]);
    let document = scene(
        "[[assets]]\npath = \"/prefabs/eye.rveprefab\"\n[[instances]]\nid = \"player\"\nprefab = \"/prefabs/eye.rveprefab\"\n",
    );
    let expanded = library.expand(&document).unwrap().document;
    World::from_document(&expanded).unwrap();
    assert!(World::from_document(&document).is_err());
}

#[test]
fn a_second_light_from_a_prefab_is_rejected() {
    let prefab = "[[objects]]\nid = \"lamp\"\n[objects.directional_light]\n";
    let library = Library::new(&[("prefabs/lamp.rveprefab", prefab)]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n{CAMERA}[[objects]]\nid = \"sun\"\n[objects.directional_light]\n[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n"
    ));
    let error = library.expand(&document).unwrap_err();
    assert!(error.message.contains("directional_light"), "{error}");
}

#[test]
fn prefab_cycles_are_reported_with_the_chain() {
    let a = "[[assets]]\npath = \"/b.rveprefab\"\n[[instances]]\nid = \"b\"\nprefab = \"/b.rveprefab\"\n";
    let b = "[[assets]]\npath = \"/a.rveprefab\"\n[[instances]]\nid = \"a\"\nprefab = \"/a.rveprefab\"\n";
    let library = Library::new(&[("a.rveprefab", a), ("b.rveprefab", b)]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/a.rveprefab\"\n{CAMERA}[[instances]]\nid = \"x\"\nprefab = \"/a.rveprefab\"\n"
    ));
    let error = library.expand(&document).unwrap_err();
    assert!(
        error
            .message
            .contains("/a.rveprefab -> /b.rveprefab -> /a.rveprefab"),
        "{error}"
    );

    let selfish = "[[assets]]\npath = \"/s.rveprefab\"\n[[instances]]\nid = \"s\"\nprefab = \"/s.rveprefab\"\n";
    let library = Library::new(&[("s.rveprefab", selfish)]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/s.rveprefab\"\n{CAMERA}[[instances]]\nid = \"x\"\nprefab = \"/s.rveprefab\"\n"
    ));
    assert!(
        library
            .expand(&document)
            .unwrap_err()
            .message
            .contains("цикл")
    );
}

#[test]
fn nesting_deeper_than_the_limit_is_rejected() {
    let mut entries = Vec::new();
    for level in 0..=MAX_PREFAB_DEPTH {
        let text = if level == MAX_PREFAB_DEPTH {
            "[[objects]]\nid = \"leaf\"\n".to_owned()
        } else {
            let next = level + 1;
            format!(
                "[[assets]]\npath = \"/p{next}.rveprefab\"\n[[instances]]\nid = \"n\"\nprefab = \"/p{next}.rveprefab\"\n"
            )
        };
        entries.push((format!("p{level}.rveprefab"), text));
    }
    let refs: Vec<_> = entries
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let library = Library::new(&refs);
    let document = scene(&format!(
        "[[assets]]\npath = \"/p0.rveprefab\"\n{CAMERA}[[instances]]\nid = \"x\"\nprefab = \"/p0.rveprefab\"\n"
    ));
    let error = library.expand(&document).unwrap_err();
    assert!(error.message.contains("вложенность"), "{error}");

    // На один уровень меньше — допустимо.
    let document = scene(&format!(
        "[[assets]]\npath = \"/p1.rveprefab\"\n{CAMERA}[[instances]]\nid = \"x\"\nprefab = \"/p1.rveprefab\"\n"
    ));
    library.expand(&document).unwrap();
}

#[test]
fn expanded_ids_longer_than_the_limit_are_rejected() {
    let long = "a".repeat(63);
    let inner = format!(
        "[[objects]]\nid = \"{long}\"\n[[objects]]\nid = \"{long}2\"\nparent = \"{long}\"\n"
    );
    let outer = format!(
        "[[assets]]\npath = \"/inner.rveprefab\"\n[[objects]]\nid = \"r\"\n[[instances]]\nid = \"{long}\"\nparent = \"r\"\nprefab = \"/inner.rveprefab\"\n"
    );
    let library = Library::new(&[("inner.rveprefab", &inner), ("outer.rveprefab", &outer)]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/outer.rveprefab\"\n{CAMERA}[[instances]]\nid = \"{long}\"\nprefab = \"/outer.rveprefab\"\n"
    ));
    // `L.L.L2`: 63 + 1 + 63 + 1 + 64 = 192 байта; ещё один уровень даёт 256.
    library.expand(&document).unwrap();
    let wrapper = format!(
        "[[assets]]\npath = \"/outer.rveprefab\"\n[[objects]]\nid = \"w\"\n[[instances]]\nid = \"{long}\"\nparent = \"w\"\nprefab = \"/outer.rveprefab\"\n"
    );
    let library = Library::new(&[
        ("inner.rveprefab", &inner),
        ("outer.rveprefab", &outer),
        ("wrapper.rveprefab", &wrapper),
    ]);
    let document = scene(&format!(
        "[[assets]]\npath = \"/wrapper.rveprefab\"\n{CAMERA}[[instances]]\nid = \"{long}\"\nprefab = \"/wrapper.rveprefab\"\n"
    ));
    let error = library.expand(&document).unwrap_err();
    assert!(error.message.contains("255"), "{error}");
}

#[test]
fn unknown_targets_and_missing_components_are_errors() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let missing = scene(&door_scene(
        "[[instances.overrides]]\ntarget = \"knob\"\nname = \"x\"\n",
    ));
    assert!(
        library
            .expand(&missing)
            .unwrap_err()
            .message
            .contains("knob")
    );
    let no_camera = scene(&door_scene(
        "[[instances.overrides]]\ntarget = \"handle\"\ncamera = { active = true }\n",
    ));
    assert!(
        library
            .expand(&no_camera)
            .unwrap_err()
            .message
            .contains("camera")
    );
    let no_script = scene(&door_scene(
        "[[instances.overrides]]\ntarget = \"frame\"\nlite_params = [{ path = \"/scripts/rotate.lua\", params = { speed = 1 } }]\n",
    ));
    assert!(library.expand(&no_script).is_err());
}

#[test]
fn parents_may_point_inside_an_instance_and_missing_parents_fail_after_expansion() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let mut text = door_scene("");
    text.push_str("[[objects]]\nid = \"sign\"\nparent = \"door1.handle\"\n");
    let document = SceneDocument::parse("scene.rvescene", &text).unwrap();
    let expanded = library.expand(&document).unwrap().document;
    assert_eq!(
        object(&expanded, "sign").parent.as_deref(),
        Some("door1.handle")
    );

    let mut text = door_scene("");
    text.push_str("[[objects]]\nid = \"sign\"\nparent = \"door1.knob\"\n");
    let document = SceneDocument::parse("scene.rvescene", &text).unwrap();
    assert!(
        library
            .expand(&document)
            .unwrap_err()
            .message
            .contains("door1.knob")
    );
}

#[test]
fn conflicting_archives_from_prefabs_fail_and_the_scene_archive_wins() {
    let a = "[[assets]]\npath = \"/models/door.fbx\"\narchive = \"doors\"\n[[objects]]\nid = \"a\"\n[objects.mesh_renderer]\nmodel = \"/models/door.fbx\"\n";
    let b = "[[assets]]\npath = \"/models/door.fbx\"\narchive = \"props\"\n[[objects]]\nid = \"b\"\n[objects.mesh_renderer]\nmodel = \"/models/door.fbx\"\n";
    let library = Library::new(&[("a.rveprefab", a), ("b.rveprefab", b)]);
    let instances = "[[instances]]\nid = \"x\"\nprefab = \"/a.rveprefab\"\n[[instances]]\nid = \"y\"\nprefab = \"/b.rveprefab\"\n";
    let declared = "[[assets]]\npath = \"/a.rveprefab\"\n[[assets]]\npath = \"/b.rveprefab\"\n";
    let document = scene(&format!("{declared}{CAMERA}{instances}"));
    assert!(
        library
            .expand(&document)
            .unwrap_err()
            .message
            .contains("doors")
    );

    let document = scene(&format!(
        "{declared}[[assets]]\npath = \"/models/door.fbx\"\narchive = \"scene\"\n{CAMERA}{instances}"
    ));
    let expanded = library.expand(&document).unwrap().document;
    let asset = expanded
        .assets
        .iter()
        .find(|asset| asset.path.as_str() == "models/door.fbx")
        .unwrap();
    assert_eq!(asset.archive.as_deref(), Some("scene"));
}

#[test]
fn local_rules_are_checked_at_parse() {
    let invalid = [
        // Префаб не объявлен в [[assets]].
        format!("{CAMERA}[[instances]]\nid = \"x\"\nprefab = \"/door.rveprefab\"\n"),
        // Неверное расширение.
        format!(
            "[[assets]]\npath = \"/door.rvescene\"\n{CAMERA}[[instances]]\nid = \"x\"\nprefab = \"/door.rvescene\"\n"
        ),
        // Точка в авторском id и повтор id объекта.
        door_scene("").replace("id = \"door1\"", "id = \"door.1\""),
        door_scene("").replace("id = \"door1\"", "id = \"room\""),
        // Повтор target и override с ресурсом, не объявленным в документе экземпляра.
        door_scene(
            "[[instances.overrides]]\ntarget = \"handle\"\n[[instances.overrides]]\ntarget = \"handle\"\n",
        ),
        door_scene(
            "[[instances.overrides]]\ntarget = \"handle\"\nmesh_renderer = { model = \"/models/silver.fbx\" }\n",
        ),
        door_scene("[[instances.overrides]]\ntarget = \"bad target\"\n"),
        door_scene("[[instances.overrides]]\ntarget = \"handle\"\nunknown = 1\n"),
        door_scene(
            "[[instances.overrides]]\ntarget = \"handle\"\ntransform = { skew = [1.0, 0.0, 0.0] }\n",
        ),
    ];
    for text in invalid {
        assert!(
            SceneDocument::parse("scene.rvescene", &text).is_err(),
            "{text}"
        );
    }
}

#[test]
fn prefab_documents_need_exactly_one_root_but_no_camera() {
    SceneDocument::parse_prefab("p.rveprefab", "[[objects]]\nid = \"a\"\n").unwrap();
    assert!(
        SceneDocument::parse_prefab(
            "p.rveprefab",
            "[[objects]]\nid = \"a\"\n[[objects]]\nid = \"b\"\n"
        )
        .is_err()
    );
    assert!(SceneDocument::parse_prefab("p.rveprefab", "").is_err());
    assert!(SceneDocument::parse("p.rvescene", "[[objects]]\nid = \"a\"\n").is_err());
    let loaded_as_scene = Library::new(&[]);
    let document = scene(&door_scene(""));
    let error = expand(&document, &mut |_| {
        SceneDocument::parse("door.rveprefab", CAMERA)
    })
    .unwrap_err();
    assert!(error.message.contains("не как префаб"), "{error}");
    drop(loaded_as_scene);
}

#[test]
fn instances_round_trip_and_scenes_without_them_do_not_change() {
    let text = door_scene(
        "parent = \"room\"\n[instances.transform]\nposition = [2.0, 0.0, 0.0]\n[[instances.overrides]]\ntarget = \"handle\"\ntransform = { position = [0.1, 1.0, 0.0] }\nlite_params = [{ path = \"/scripts/rotate.lua\", params = { speed = 90.0 } }]\n",
    );
    let document = scene(&text);
    let saved = document.to_toml().unwrap();
    let reparsed = SceneDocument::parse("scene.rvescene", &saved).unwrap();
    assert_eq!(reparsed.to_toml().unwrap(), saved);
    let instance = &reparsed.instances[0];
    assert_eq!(instance.parent.as_deref(), Some("room"));
    let change = &instance.overrides[0];
    assert_eq!(
        change.transform,
        Some(TransformOverride {
            position: Some([0.1, 1.0, 0.0]),
            ..TransformOverride::default()
        })
    );

    for name in ["main", "second"] {
        let path = format!(
            "{}/../../examples/demo-game/Assets/scenes/{name}.rvescene",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let document = SceneDocument::parse(path.as_str(), &text).unwrap();
        if document.instances.is_empty() {
            assert!(!document.to_toml().unwrap().contains("instances"), "{path}");
        }
    }
    let plain = scene(CAMERA);
    assert!(!plain.to_toml().unwrap().contains("instances"));
}

#[test]
fn environment_prefab_override_replaces_the_whole_component() {
    let prefab = r#"
[[objects]]
id="root"
[objects.environment]
intensity=0.5
[objects.environment.procedural]
zenith_color=[0.1, 0.2, 0.3]
horizon_color=[0.4, 0.5, 0.6]
ground_color=[0.0, 0.0, 0.0]
sun_size_deg=1.0
sun_intensity=1.0
"#;
    let library = Library::new(&[("prefabs/sky.rveprefab", prefab)]);
    let text = format!(
        "[[assets]]\npath=\"/prefabs/sky.rveprefab\"\n{CAMERA}[[instances]]\nid=\"sky\"\nprefab=\"/prefabs/sky.rveprefab\"\n[[instances.overrides]]\ntarget=\"root\"\n[instances.overrides.environment]\nintensity=2.0\n[instances.overrides.environment.procedural]\nzenith_color=[0.7, 0.8, 0.9]\nhorizon_color=[0.2, 0.3, 0.4]\nground_color=[0.1, 0.1, 0.1]\nsun_size_deg=4.0\nsun_intensity=5.0\n"
    );
    let expanded = library.expand(&scene(&text)).unwrap().document;
    let environment = object(&expanded, "sky").environment.as_ref().unwrap();
    assert_eq!(environment.intensity, 2.0);
    assert_eq!(
        environment.procedural.unwrap().zenith_color,
        [0.7, 0.8, 0.9]
    );

    let inherited_text = format!(
        "[[assets]]\npath=\"/prefabs/sky.rveprefab\"\n{CAMERA}[[instances]]\nid=\"sky\"\nprefab=\"/prefabs/sky.rveprefab\"\n"
    );
    let inherited = library.expand(&scene(&inherited_text)).unwrap().document;
    assert_eq!(
        object(&inherited, "sky")
            .environment
            .as_ref()
            .unwrap()
            .intensity,
        0.5
    );
}

#[test]
fn expanded_documents_are_not_saved() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let expanded = library.expand(&scene(&door_scene(""))).unwrap().document;
    assert!(expanded.to_toml().is_err());
}

#[test]
fn world_rejects_unexpanded_documents_without_panicking() {
    let mut text = door_scene("");
    text.push_str("[[objects]]\nid = \"sign\"\nparent = \"door1.handle\"\n");
    let document = SceneDocument::parse("scene.rvescene", &text).unwrap();
    assert!(World::from_document(&document).is_err());
}

#[test]
fn expand_without_overrides_skips_only_that_instance() {
    let library = Library::new(&[("prefabs/door.rveprefab", DOOR)]);
    let document = scene(&format!(
        "{}[[instances.overrides]]\ntarget = \"handle\"\nname = \"Gold\"\n[[instances]]\nid = \"door2\"\nprefab = \"/prefabs/door.rveprefab\"\n[[instances.overrides]]\ntarget = \"handle\"\nname = \"Silver\"\n",
        door_scene("")
    ));
    let plain = expand_without_overrides(&document, "door1", &mut |path| library.load(path))
        .unwrap()
        .document;
    assert_eq!(object(&plain, "door1.handle").name, None);
    assert_eq!(
        object(&plain, "door2.handle").name.as_deref(),
        Some("Silver")
    );
}

#[test]
fn documents_without_instances_validate_as_before() {
    let path = format!(
        "{}/../../examples/demo-game/Assets/scenes/second.rvescene",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let document = SceneDocument::parse(path.as_str(), &text).unwrap();
    let expansion = expand(&document, &mut |_| unreachable!()).unwrap();
    assert_eq!(expansion.document.objects.len(), document.objects.len());
    assert!(expansion.prefabs.is_empty());
    // Без экземпляров отсутствующий родитель — ошибка разбора, как раньше.
    assert!(
        SceneDocument::parse(
            "s.rvescene",
            &format!("{CAMERA}[[objects]]\nid = \"x\"\nparent = \"nope\"\n")
        )
        .is_err()
    );
}

#[test]
fn audio_source_prefab_override_replaces_the_whole_component_and_checks_its_clip() {
    let prefab = "[[assets]]\npath = \"/audio/hum.wav\"\n[[objects]]\nid = \"speaker\"\n[objects.audio_source]\nclip = \"/audio/hum.wav\"\nvolume = 0.4\nlooping = true\n";
    let library = Library::new(&[("prefabs/speaker.rveprefab", prefab)]);
    let assets = "[[assets]]\npath = \"/prefabs/speaker.rveprefab\"\n[[assets]]\npath = \"/audio/hum.wav\"\n[[assets]]\npath = \"/audio/bell.ogg\"\n";
    let overridden = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"spk\"\nprefab = \"/prefabs/speaker.rveprefab\"\n[[instances.overrides]]\ntarget = \"speaker\"\naudio_source = {{ clip = \"/audio/bell.ogg\", spatial = true }}\n"
    );
    let expanded = library.expand(&scene(&overridden)).unwrap().document;
    let audio = object(&expanded, "spk").audio_source.clone().unwrap();
    assert_eq!(audio.clip.as_str(), "audio/bell.ogg");
    assert!(audio.spatial);
    assert!(!audio.looping, "replacement must not keep inherited fields");
    assert_eq!(audio.volume, 1.0);

    let inherited = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"spk\"\nprefab = \"/prefabs/speaker.rveprefab\"\n"
    );
    let kept = library.expand(&scene(&inherited)).unwrap().document;
    assert_eq!(
        object(&kept, "spk").audio_source.as_ref().unwrap().volume,
        0.4
    );

    let missing_clip_asset = format!(
        "{CAMERA}[[assets]]\npath = \"/prefabs/speaker.rveprefab\"\n[[instances]]\nid = \"spk\"\nprefab = \"/prefabs/speaker.rveprefab\"\n[[instances.overrides]]\ntarget = \"speaker\"\naudio_source = {{ clip = \"/audio/bell.ogg\" }}\n"
    );
    let error = SceneDocument::parse("scene.rvescene", &missing_clip_asset).unwrap_err();
    assert!(error.message.contains("[[assets]]"), "{error}");

    let lamp = "[[objects]]\nid = \"lamp\"\n[objects.point_light]\n";
    let other = Library::new(&[("prefabs/lamp.rveprefab", lamp)]);
    let bad = format!(
        "{assets}{CAMERA}[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n[[instances.overrides]]\ntarget = \"lamp\"\naudio_source = {{ clip = \"/audio/bell.ogg\" }}\n"
    );
    let error = other.expand(&scene(&bad)).unwrap_err();
    assert!(error.message.contains("audio_source"), "{error}");
}

#[test]
fn animator_prefab_override_replaces_the_whole_component_and_checks_its_clips() {
    let prefab = "[[assets]]\npath = \"/models/hero.fbx\"\n[[assets]]\npath = \"/anims/run.fbx\"\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\nclips = [\"/anims/run.fbx\"]\nlooping = true\nspeed = 1.5\n";
    let library = Library::new(&[("prefabs/hero.rveprefab", prefab)]);
    let assets = "[[assets]]\npath = \"/prefabs/hero.rveprefab\"\n[[assets]]\npath = \"/models/hero.fbx\"\n[[assets]]\npath = \"/anims/run.fbx\"\n[[assets]]\npath = \"/anims/jump.fbx\"\n";
    let overridden = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"h\"\nprefab = \"/prefabs/hero.rveprefab\"\n[[instances.overrides]]\ntarget = \"hero\"\nanimator = {{ looping = false, clips = [\"/anims/jump.fbx\"] }}\n"
    );
    let expanded = library.expand(&scene(&overridden)).unwrap().document;
    let animator = object(&expanded, "h").animator.clone().unwrap();
    assert!(!animator.looping);
    assert_eq!(
        animator.speed, 1.0,
        "замена не сохраняет унаследованную скорость"
    );
    assert_eq!(animator.clips[0].as_str(), "anims/jump.fbx");

    let inherited = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"h\"\nprefab = \"/prefabs/hero.rveprefab\"\n"
    );
    let kept = library.expand(&scene(&inherited)).unwrap().document;
    assert_eq!(object(&kept, "h").animator.as_ref().unwrap().speed, 1.5);

    let undeclared = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"h\"\nprefab = \"/prefabs/hero.rveprefab\"\n[[instances.overrides]]\ntarget = \"hero\"\nanimator = {{ clips = [\"/anims/missing.fbx\"] }}\n"
    );
    let error = SceneDocument::parse("scene.rvescene", &undeclared).unwrap_err();
    assert!(error.message.contains("[[assets]]"), "{error}");

    let lamp = "[[objects]]\nid = \"lamp\"\n[objects.point_light]\n";
    let other = Library::new(&[("prefabs/lamp.rveprefab", lamp)]);
    let bad = format!(
        "{assets}{CAMERA}[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n[[instances.overrides]]\ntarget = \"lamp\"\nanimator = {{ looping = false }}\n"
    );
    let error = other.expand(&scene(&bad)).unwrap_err();
    assert!(error.message.contains("animator"), "{error}");
}

fn load_template(library: &Library, path: &str) -> Result<PrefabTemplate, SceneError> {
    let path = VirtualPath::parse(path).unwrap();
    let mut read = |child: &VirtualPath| -> Result<String, String> {
        library
            .0
            .get(child.as_str())
            .cloned()
            .ok_or_else(|| "не найден".to_owned())
    };
    PrefabTemplate::load(&path, &mut read)
}

#[test]
fn max_instances_from_nested_prefab_assets_never_reaches_the_scene() {
    let inner = "[[objects]]\nid = \"core\"\n";
    let outer = "[[assets]]\npath = \"/prefabs/inner.rveprefab\"\nmax_instances = 5\n[[objects]]\nid = \"base\"\n[[instances]]\nid = \"door\"\nprefab = \"/prefabs/inner.rveprefab\"\nparent = \"base\"\n";
    let library = Library::new(&[
        ("prefabs/inner.rveprefab", inner),
        ("prefabs/outer.rveprefab", outer),
    ]);
    let instances = "[[instances]]\nid = \"x\"\nprefab = \"/prefabs/outer.rveprefab\"\n";
    let max_instances = |expanded: &Expansion, path: &str| {
        expanded
            .document
            .assets
            .iter()
            .find(|asset| asset.path.as_str() == path)
            .and_then(|asset| asset.max_instances)
    };

    // Лимит внешнего префаба задан сценой, лимит вложенного в префабе не переносится.
    let declared = "[[assets]]\npath = \"/prefabs/outer.rveprefab\"\nmax_instances = 2\n";
    let expanded = library
        .expand(&scene(&format!("{declared}{CAMERA}{instances}")))
        .unwrap();
    assert_eq!(max_instances(&expanded, "prefabs/outer.rveprefab"), Some(2));
    assert_eq!(max_instances(&expanded, "prefabs/inner.rveprefab"), None);
    assert!(
        expanded
            .document
            .assets
            .iter()
            .any(|asset| asset.path.as_str() == "prefabs/inner.rveprefab")
    );

    // Собственная запись сцены остаётся при слиянии с вложенным префабом.
    let declared = "[[assets]]\npath = \"/prefabs/outer.rveprefab\"\nmax_instances = 2\n[[assets]]\npath = \"/prefabs/inner.rveprefab\"\nmax_instances = 7\n";
    let expanded = library
        .expand(&scene(&format!("{declared}{CAMERA}{instances}")))
        .unwrap();
    assert_eq!(max_instances(&expanded, "prefabs/inner.rveprefab"), Some(7));
}

#[test]
fn particle_emitter_prefab_override_replaces_the_whole_component_and_checks_texture() {
    let prefab =
        "[[objects]]\nid = \"fx\"\n[objects.particle_emitter]\ncapacity = 512\nrate = 40.0\n";
    let library = Library::new(&[("prefabs/fx.rveprefab", prefab)]);
    let assets =
        "[[assets]]\npath = \"/prefabs/fx.rveprefab\"\n[[assets]]\npath = \"/fx/smoke.png\"\n";
    let overridden = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"spark\"\nprefab = \"/prefabs/fx.rveprefab\"\n[[instances.overrides]]\ntarget = \"fx\"\nparticle_emitter = {{ blend = \"alpha\", texture = \"/fx/smoke.png\" }}\n"
    );
    let expanded = library.expand(&scene(&overridden)).unwrap().document;
    let emitter = object(&expanded, "spark").particle_emitter.clone().unwrap();
    assert_eq!(emitter.blend, crate::particles::ParticleBlendMode::Alpha);
    assert_eq!(
        emitter.rate, 10.0,
        "replacement must not keep inherited fields"
    );
    assert_eq!(emitter.capacity, 256);
    assert_eq!(
        emitter.texture.as_ref().map(|path| path.as_str()),
        Some("fx/smoke.png")
    );

    let inherited = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"spark\"\nprefab = \"/prefabs/fx.rveprefab\"\n"
    );
    let kept = library.expand(&scene(&inherited)).unwrap().document;
    assert_eq!(
        object(&kept, "spark")
            .particle_emitter
            .as_ref()
            .unwrap()
            .capacity,
        512
    );

    let undeclared = format!(
        "{CAMERA}[[objects]]\nid = \"fx\"\n[objects.particle_emitter]\ntexture = \"/fx/smoke.png\"\n"
    );
    let error = SceneDocument::parse("scene.rvescene", &undeclared).unwrap_err();
    assert!(error.message.contains("[[assets]]"), "{error}");

    let lamp = "[[objects]]\nid = \"lamp\"\n[objects.point_light]\n";
    let other = Library::new(&[("prefabs/lamp.rveprefab", lamp)]);
    let bad = format!(
        "{assets}{CAMERA}[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n[[instances.overrides]]\ntarget = \"lamp\"\nparticle_emitter = {{ rate = 5.0 }}\n"
    );
    let error = other.expand(&scene(&bad)).unwrap_err();
    assert!(error.message.contains("particle_emitter"), "{error}");
}

#[test]
fn prefab_with_environment_loads_as_a_template_and_only_spawn_is_refused() {
    let environment = "[objects.environment]\n[objects.environment.procedural]\nzenith_color = [0.2, 0.3, 0.5]\nhorizon_color = [0.8, 0.7, 0.5]\nground_color = [0.1, 0.1, 0.1]\nsun_size_deg = 3.0\nsun_intensity = 2.0\n";
    let text = format!("[[objects]]\nid = \"sky\"\n{environment}");
    let library = Library::new(&[("prefabs/sky.rveprefab", text.as_str())]);
    let template = load_template(&library, "prefabs/sky.rveprefab").unwrap();
    assert_eq!(template.objects().len(), 1);
    assert!(template.check_spawnable().is_err());
}

#[test]
fn spawn_template_accepts_point_and_spot_lights() {
    let library = Library::new(&[(
        "prefabs/lamp.rveprefab",
        "[[objects]]\nid = \"root\"\n[objects.point_light]\n[[objects]]\nid = \"bulb\"\nparent = \"root\"\n[objects.spot_light]\n",
    )]);
    let template = load_template(&library, "prefabs/lamp.rveprefab").unwrap();
    assert_eq!(template.objects.len(), 2);
}

#[test]
fn spawn_template_rejects_camera_directional_light_and_environment() {
    let environment = "[objects.environment]\n[objects.environment.procedural]\nzenith_color = [0.2, 0.3, 0.5]\nhorizon_color = [0.8, 0.7, 0.5]\nground_color = [0.1, 0.1, 0.1]\nsun_size_deg = 3.0\nsun_intensity = 2.0\n";
    for (name, component) in [
        ("camera", "[objects.camera]\n"),
        ("directional_light", "[objects.directional_light]\n"),
        ("environment", environment),
    ] {
        let text = format!(
            "[[objects]]\nid = \"root\"\n[[objects]]\nid = \"lamp\"\nparent = \"root\"\n{component}"
        );
        let library = Library::new(&[("prefabs/bullet.rveprefab", text.as_str())]);
        let template = load_template(&library, "prefabs/bullet.rveprefab").unwrap();
        let error = template.check_spawnable().unwrap_err();
        assert!(error.message.contains(name), "{error}");
        assert_eq!(error.object_id.as_deref(), Some("lamp"));
    }
}

#[test]
fn spawn_template_reports_missing_prefabs_and_dynamic_children() {
    let empty = Library::new(&[]);
    let error = load_template(&empty, "prefabs/gone.rveprefab").unwrap_err();
    assert!(error.message.contains("не найден"), "{error}");

    let library = Library::new(&[(
        "prefabs/crate.rveprefab",
        "[[objects]]\nid = \"root\"\n[[objects]]\nid = \"child\"\nparent = \"root\"\n[objects.collider]\n[objects.rigid_body]\n",
    )]);
    let error = load_template(&library, "prefabs/crate.rveprefab").unwrap_err();
    assert!(error.message.contains("корневым"), "{error}");
}

#[test]
fn spawn_template_uses_local_ids_parents_and_order_for_nested_prefabs() {
    let library = Library::new(&[
        (
            "prefabs/inner.rveprefab",
            "[[objects]]\nid = \"core\"\n[[objects]]\nid = \"arm\"\nparent = \"core\"\n",
        ),
        (
            "prefabs/outer.rveprefab",
            "[[assets]]\npath = \"/prefabs/inner.rveprefab\"\n[[objects]]\nid = \"base\"\n\
             [[instances]]\nid = \"door\"\nprefab = \"/prefabs/inner.rveprefab\"\nparent = \"base\"\n",
        ),
    ]);
    let template = load_template(&library, "prefabs/outer.rveprefab").unwrap();
    assert_eq!(template.stem(), "outer");
    let ids = template
        .objects
        .iter()
        .map(|object| object.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["", "door", "door.arm"]);
    assert_eq!(template.root, 0);
    assert_eq!(template.parents, [None, Some(0), Some(1)]);
    assert_eq!(template.order, [0, 1, 2]);
    assert!(
        template
            .assets()
            .iter()
            .any(|asset| asset.path.as_str() == "prefabs/inner.rveprefab")
    );
}

#[test]
fn spawnable_template_passes_check_and_structural_errors_still_fail_load() {
    let library = Library::new(&[(
        "prefabs/ball.rveprefab",
        "[[objects]]\nid = \"root\"\n[objects.collider]\n[objects.rigid_body]\nkind = \"dynamic\"\n",
    )]);
    assert!(
        load_template(&library, "prefabs/ball.rveprefab")
            .unwrap()
            .check_spawnable()
            .is_ok()
    );
    // Не читается — ошибка загрузки, а не отказ spawn.
    assert!(load_template(&library, "prefabs/gone.rveprefab").is_err());
    let broken = Library::new(&[(
        "prefabs/bad.rveprefab",
        "[[objects]]\nid = \"root\"\n[[objects]]\nid = \"child\"\nparent = \"missing\"\n",
    )]);
    assert!(load_template(&broken, "prefabs/bad.rveprefab").is_err());
}

#[test]
fn network_prefab_override_replaces_the_whole_component() {
    let prefab = "[[objects]]\nid = \"crate\"\n[objects.network_identity]\nauthority = \"owner\"\n[objects.network_transform]\nsend_rate_hz = 10\nsnap_distance = 2.0\n";
    let library = Library::new(&[("prefabs/crate.rveprefab", prefab)]);
    assert!(
        load_template(&library, "prefabs/crate.rveprefab")
            .unwrap()
            .check_spawnable()
            .is_ok(),
        "сетевые компоненты допустимы в spawn-префабе"
    );
    let assets = "[[assets]]\npath = \"/prefabs/crate.rveprefab\"\n";
    let overridden = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"box\"\nprefab = \"/prefabs/crate.rveprefab\"\n[[instances.overrides]]\ntarget = \"crate\"\nnetwork_transform = {{ sync_scale = true }}\n"
    );
    let expanded = library.expand(&scene(&overridden)).unwrap().document;
    let root = object(&expanded, "box");
    assert_eq!(
        root.network_transform,
        Some(NetworkTransformComponent {
            sync_scale: true,
            ..NetworkTransformComponent::default()
        }),
        "замена не сохраняет унаследованные send_rate_hz и snap_distance"
    );
    assert_eq!(
        root.network_identity.map(|identity| identity.authority),
        Some(NetAuthority::Owner),
        "identity без override наследуется"
    );

    let inherited = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"box\"\nprefab = \"/prefabs/crate.rveprefab\"\n"
    );
    let kept = library.expand(&scene(&inherited)).unwrap().document;
    assert_eq!(
        object(&kept, "box").network_transform.unwrap().send_rate_hz,
        10
    );

    let identity_override = format!(
        "{assets}{CAMERA}[[instances]]\nid = \"box\"\nprefab = \"/prefabs/crate.rveprefab\"\n[[instances.overrides]]\ntarget = \"crate\"\nnetwork_identity = {{ authority = \"server\" }}\n"
    );
    let replaced = library.expand(&scene(&identity_override)).unwrap().document;
    assert_eq!(
        object(&replaced, "box")
            .network_identity
            .map(|identity| identity.authority),
        Some(NetAuthority::Server)
    );
}

#[test]
fn network_prefab_overrides_need_existing_components_and_valid_fields() {
    let lamp = "[[objects]]\nid = \"lamp\"\n[objects.point_light]\n";
    let library = Library::new(&[("prefabs/lamp.rveprefab", lamp)]);
    let assets = "[[assets]]\npath = \"/prefabs/lamp.rveprefab\"\n";
    let instance = |override_line: &str| {
        format!(
            "{assets}{CAMERA}[[instances]]\nid = \"lamp\"\nprefab = \"/prefabs/lamp.rveprefab\"\n[[instances.overrides]]\ntarget = \"lamp\"\n{override_line}\n"
        )
    };

    let error = library
        .expand(&scene(&instance(
            "network_identity = { authority = \"owner\" }",
        )))
        .unwrap_err();
    assert!(error.message.contains("network_identity"), "{error}");

    let error = library
        .expand(&scene(&instance("network_transform = {}")))
        .unwrap_err();
    assert!(error.message.contains("network_transform"), "{error}");

    let error = SceneDocument::parse(
        "scene.rvescene",
        &instance("network_transform = { send_rate_hz = 0 }"),
    )
    .unwrap_err();
    assert!(error.message.contains("send_rate_hz"), "{error}");
}
