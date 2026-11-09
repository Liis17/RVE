use super::{ObjectHandle, World};
use crate::document::{
    CharacterControllerComponent, ColliderComponent, ColliderShape, RigidBodyComponent,
    RigidBodyKind, SceneDocument,
};

const SCENE: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
[[objects]]\nid = \"box\"\n[objects.collider]\n[objects.rigid_body]\n\
[[objects]]\nid = \"player\"\n[objects.character_controller]\n\
[[objects]]\nid = \"child\"\nparent = \"box\"\n[objects.collider]\n[objects.rigid_body]\nkind = \"kinematic\"\n";

fn world() -> World {
    World::from_document(&SceneDocument::parse("components.rvescene", SCENE).unwrap()).unwrap()
}

fn handle(world: &World, id: &str) -> ObjectHandle {
    world.handle(id).unwrap()
}

#[test]
fn collider_setter_replaces_primitives_and_rejects_bad_dimensions() {
    let mut world = world();
    let box_handle = handle(&world, "box");
    let mut collider = world.object(box_handle).unwrap().collider.clone().unwrap();
    collider.shape = ColliderShape::Sphere;
    collider.radius = 2.0;
    world.set_collider(box_handle, collider.clone()).unwrap();
    assert_eq!(
        world.object(box_handle).unwrap().collider,
        Some(collider.clone())
    );

    let mut invalid = collider.clone();
    invalid.radius = 0.0;
    assert!(world.set_collider(box_handle, invalid).is_err());
    let mut mesh = collider.clone();
    mesh.shape = ColliderShape::ConvexMesh;
    assert!(world.set_collider(box_handle, mesh).is_err());
    assert_eq!(world.object(box_handle).unwrap().collider, Some(collider));
}

#[test]
fn collider_setter_requires_an_existing_component() {
    let mut world = world();
    let player = handle(&world, "player");
    assert!(
        world
            .set_collider(player, ColliderComponent::default())
            .is_err()
    );
}

#[test]
fn rigid_body_setter_rejects_dynamic_children_and_zero_mass() {
    let mut world = world();
    let box_handle = handle(&world, "box");
    let mut body = RigidBodyComponent {
        kind: RigidBodyKind::Kinematic,
        ..RigidBodyComponent::default()
    };
    world.set_rigid_body(box_handle, body).unwrap();
    assert_eq!(world.object(box_handle).unwrap().rigid_body, Some(body));

    body.kind = RigidBodyKind::Dynamic;
    body.mass = 0.0;
    assert!(world.set_rigid_body(box_handle, body).is_err());
    body.mass = 2.0;
    body.linear_damping = -1.0;
    assert!(world.set_rigid_body(box_handle, body).is_err());
    body.linear_damping = 0.0;
    let child = handle(&world, "child");
    assert!(world.set_rigid_body(child, body).is_err());
}

#[test]
fn controller_setter_validates_new_gravity_fields_and_keeps_old_defaults() {
    let mut world = world();
    let player = handle(&world, "player");
    let defaults = world.object(player).unwrap().character_controller.unwrap();
    assert!(!defaults.use_gravity);
    assert_eq!(defaults.gravity_scale, 1.0);

    let mut controller = CharacterControllerComponent {
        use_gravity: true,
        jump_height: 2.0,
        ..defaults
    };
    world.set_character_controller(player, controller).unwrap();
    assert_eq!(
        world.object(player).unwrap().character_controller,
        Some(controller)
    );

    controller.jump_height = -1.0;
    assert!(world.set_character_controller(player, controller).is_err());
    controller.jump_height = 2.0;
    controller.gravity_scale = f32::NAN;
    assert!(world.set_character_controller(player, controller).is_err());
}

const RUNTIME_SCENE: &str = "[[assets]]\npath = \"/models/crate.fbx\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n\
[[objects]]\nid = \"sun\"\n[objects.directional_light]\n\
[[objects]]\nid = \"lamp\"\n[objects.point_light]\nintensity = 2.0\n\
[[objects]]\nid = \"crate\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n\
[[objects]]\nid = \"fire\"\n[objects.particle_emitter]\n\
[[objects]]\nid = \"sky\"\n[objects.environment.procedural]\nzenith_color = [0.1, 0.2, 0.8]\nhorizon_color = [0.6, 0.6, 0.6]\nground_color = [0.1, 0.1, 0.1]\nsun_size_deg = 1.0\nsun_intensity = 3.0\n\
[[objects]]\nid = \"tower\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n[objects.lod_group]\ncull_distance = 50.0\n[[objects.lod_group.levels]]\nmodel = \"/models/crate.fbx\"\ndistance = 10.0\n";

fn runtime_world() -> World {
    World::from_document(&SceneDocument::parse("runtime.rvescene", RUNTIME_SCENE).unwrap()).unwrap()
}

fn mesh_objects(world: &mut World) -> Vec<ObjectHandle> {
    world
        .build_frame()
        .0
        .meshes
        .iter()
        .map(|mesh| mesh.object)
        .collect()
}

#[test]
fn hidden_mesh_leaves_the_frame_and_returns_when_shown() {
    let mut world = runtime_world();
    let crate_handle = handle(&world, "crate");
    assert!(mesh_objects(&mut world).contains(&crate_handle));

    world.set_mesh_visible(crate_handle, false).unwrap();
    assert!(!mesh_objects(&mut world).contains(&crate_handle));

    world.set_mesh_visible(crate_handle, true).unwrap();
    assert!(mesh_objects(&mut world).contains(&crate_handle));
    assert!(
        world
            .set_mesh_visible(handle(&world, "camera"), false)
            .is_err()
    );
}

#[test]
fn directional_light_changes_reach_the_next_frame_and_disabling_clears_it() {
    let mut world = runtime_world();
    let sun = handle(&world, "sun");
    let mut light = world.object(sun).unwrap().directional_light.unwrap();
    light.color = [0.0, 1.0, 0.0];
    world.set_directional_light(sun, light).unwrap();
    assert_eq!(world.build_frame().0.light.color, [0.0, 1.0, 0.0]);

    world.set_light_enabled(sun, false).unwrap();
    assert_eq!(
        world.build_frame().0.light.color,
        super::super::LightState::default().color,
        "выключенный свет не освещает"
    );
    world.set_light_enabled(sun, true).unwrap();
    assert_eq!(world.build_frame().0.light.color, [0.0, 1.0, 0.0]);
}

#[test]
fn invalid_light_values_are_rejected_without_changing_the_world() {
    let mut world = runtime_world();
    let lamp = handle(&world, "lamp");
    let before = world.build_frame().0.local_lights.len();
    let mut bad = world.object(lamp).unwrap().point_light.unwrap();
    bad.intensity = -1.0;
    assert!(world.set_point_light(lamp, bad).is_err());
    assert_eq!(
        world.object(lamp).unwrap().point_light.unwrap().intensity,
        2.0
    );
    assert_eq!(world.build_frame().0.local_lights.len(), before);

    let sun = handle(&world, "sun");
    assert!(
        world.set_spot_light(sun, Default::default()).is_err(),
        "у солнца нет spot_light"
    );
}

#[test]
fn material_params_merge_and_reject_out_of_range_values() {
    use super::MeshMaterialParams;
    let mut world = runtime_world();
    let crate_handle = handle(&world, "crate");
    world
        .set_mesh_material_params(
            crate_handle,
            MeshMaterialParams {
                base_color: Some([0.2, 0.4, 0.6]),
                ..Default::default()
            },
        )
        .unwrap();
    world
        .set_mesh_material_params(
            crate_handle,
            MeshMaterialParams {
                roughness: Some(0.25),
                ..Default::default()
            },
        )
        .unwrap();
    let material = world
        .object(crate_handle)
        .unwrap()
        .mesh_renderer
        .as_ref()
        .unwrap()
        .material
        .clone()
        .unwrap();
    assert_eq!(
        material.base_color,
        Some([0.2, 0.4, 0.6]),
        "прежний фактор сохраняется"
    );
    assert_eq!(material.roughness, Some(0.25));

    let refused = MeshMaterialParams {
        metallic: Some(1.5),
        ..Default::default()
    };
    assert!(
        world
            .set_mesh_material_params(crate_handle, refused)
            .is_err()
    );
    assert_eq!(
        world
            .object(crate_handle)
            .unwrap()
            .mesh_renderer
            .as_ref()
            .unwrap()
            .material
            .clone()
            .unwrap()
            .metallic,
        None
    );
}

#[test]
fn environment_intensity_and_sky_setters_require_the_matching_source() {
    use crate::document::ProceduralSkyComponent;
    let mut world = runtime_world();
    let sky = handle(&world, "sky");
    world.set_environment_intensity(sky, 0.5).unwrap();
    assert_eq!(
        world
            .object(sky)
            .unwrap()
            .environment
            .as_ref()
            .unwrap()
            .intensity,
        0.5
    );
    assert!(world.set_environment_intensity(sky, -0.5).is_err());

    let replacement = ProceduralSkyComponent {
        zenith_color: [0.0, 0.0, 0.2],
        horizon_color: [0.3; 3],
        ground_color: [0.0; 3],
        sun_size_deg: 2.0,
        sun_intensity: 5.0,
    };
    world.set_procedural_sky(sky, replacement).unwrap();
    assert_eq!(
        world
            .object(sky)
            .unwrap()
            .environment
            .as_ref()
            .unwrap()
            .procedural,
        Some(replacement)
    );
    assert!(
        world
            .set_environment_intensity(handle(&world, "crate"), 1.0)
            .is_err()
    );
}

#[test]
fn lod_distances_replace_levels_and_must_match_their_count() {
    let mut world = runtime_world();
    let tower = handle(&world, "tower");
    world.set_lod_distances(tower, &[20.0], Some(80.0)).unwrap();
    let group = world.object(tower).unwrap().lod_group.clone().unwrap();
    assert_eq!(group.levels[0].distance, 20.0);
    assert_eq!(group.cull_distance, Some(80.0));

    assert!(
        world.set_lod_distances(tower, &[20.0, 30.0], None).is_err(),
        "число уровней не меняется"
    );
    assert!(world.set_lod_distances(tower, &[20.0], Some(10.0)).is_err());
    assert_eq!(
        world
            .object(tower)
            .unwrap()
            .lod_group
            .clone()
            .unwrap()
            .cull_distance,
        Some(80.0)
    );
}

#[test]
fn particle_playing_flag_reaches_the_frame_and_commands_are_checked() {
    use super::ParticleCommand;
    let mut world = runtime_world();
    let fire = handle(&world, "fire");
    assert!(world.build_frame().0.particle_emitters[0].emitting);
    world.set_particles_playing(fire, false).unwrap();
    assert!(!world.build_frame().0.particle_emitters[0].emitting);

    assert!(
        world
            .particle_command_target(ParticleCommand::Restart { object: fire })
            .is_ok()
    );
    assert!(
        world
            .particle_command_target(ParticleCommand::Burst {
                object: fire,
                count: 0
            })
            .is_err()
    );
    assert!(
        world
            .particle_command_target(ParticleCommand::Burst {
                object: handle(&world, "crate"),
                count: 4
            })
            .is_err()
    );
}
