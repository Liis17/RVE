use super::{ObjectEdit, ObjectHandle, World};
use crate::document::SceneDocument;
use crate::params::ComponentKind;
use glam::Vec3;

const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";

fn world(scene: &str) -> World {
    World::from_document(&SceneDocument::parse("hierarchy.rvescene", scene).unwrap()).unwrap()
}

fn handle(world: &World, id: &str) -> ObjectHandle {
    world.handle(id).unwrap()
}

fn assert_vec_close(actual: Vec3, expected: Vec3) {
    assert!(
        actual.distance(expected) < 1e-4,
        "{actual:?} != {expected:?}"
    );
}

/// Родитель смещён и повёрнут; дочерний объект стоит в его локальных координатах.
const HIERARCHY: &str = "[[objects]]\nid = \"parent\"\n[objects.transform]\nposition = [10.0, 0.0, 0.0]\nrotation = [0.0, 90.0, 0.0]\n\
[[objects]]\nid = \"child\"\nparent = \"parent\"\n[objects.transform]\nposition = [0.0, 0.0, 5.0]\nrotation = [0.0, 0.0, 30.0]\n\
[[objects]]\nid = \"free\"\n[objects.transform]\nposition = [-3.0, 1.0, 2.0]\nrotation = [10.0, 20.0, 30.0]\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";

#[test]
fn reparent_with_keep_world_preserves_world_pose_and_moves_locally() {
    let mut world = world(HIERARCHY);
    let child = handle(&world, "child");
    let free = handle(&world, "free");
    let before_position = world.world_position(child).unwrap();
    let before_rotation = world.world_rotation_deg(child).unwrap();

    world.set_parent(child, Some(free), true).unwrap();

    assert_vec_close(world.world_position(child).unwrap(), before_position);
    assert_vec_close(world.world_rotation_deg(child).unwrap(), before_rotation);
    assert_eq!(world.object(child).unwrap().parent, Some(free));
    assert_vec_close(
        world.world_matrix(child).unwrap().w_axis.truncate(),
        before_position,
    );
}

#[test]
fn reparent_without_keep_world_keeps_local_transform() {
    let mut world = world(HIERARCHY);
    let child = handle(&world, "child");
    let free = handle(&world, "free");
    let local = *world.local_transform(child).unwrap();
    let before_position = world.world_position(child).unwrap();

    world.set_parent(child, Some(free), false).unwrap();

    assert_eq!(*world.local_transform(child).unwrap(), local);
    assert!(
        world
            .world_position(child)
            .unwrap()
            .distance(before_position)
            > 1e-3
    );
}

#[test]
fn unparenting_with_keep_world_restores_root_world_pose() {
    let mut world = world(HIERARCHY);
    let child = handle(&world, "child");
    let before_position = world.world_position(child).unwrap();
    let before_rotation = world.world_rotation_deg(child).unwrap();

    world.set_parent(child, None, true).unwrap();

    assert_vec_close(world.world_position(child).unwrap(), before_position);
    assert_vec_close(world.world_rotation_deg(child).unwrap(), before_rotation);
    assert!(world.object(child).unwrap().parent.is_none());
}

#[test]
fn reparent_rejects_self_parent_and_cycles_without_changing_world() {
    let mut world = world(HIERARCHY);
    let parent = handle(&world, "parent");
    let child = handle(&world, "child");
    let before = world.object(parent).unwrap().parent;

    assert!(world.set_parent(parent, Some(parent), true).is_err());
    let error = world.set_parent(parent, Some(child), true).unwrap_err();
    assert!(error.to_string().contains("цикл"), "{error}");
    assert_eq!(world.object(parent).unwrap().parent, before);
    assert_eq!(world.children(parent).unwrap(), vec![child]);
}

#[test]
fn dynamic_bodies_and_character_controllers_stay_roots() {
    let scene = "[[objects]]\nid = \"parent\"\n\
[[objects]]\nid = \"body\"\n[objects.collider]\n[objects.rigid_body]\nkind = \"dynamic\"\n\
[[objects]]\nid = \"walker\"\n[objects.character_controller]\n\
[[objects]]\nid = \"kinematic\"\n[objects.collider]\n[objects.rigid_body]\nkind = \"kinematic\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    let mut world = world(scene);
    let parent = handle(&world, "parent");
    let body = handle(&world, "body");
    let walker = handle(&world, "walker");
    let kinematic = handle(&world, "kinematic");

    assert!(world.set_parent(body, Some(parent), true).is_err());
    assert!(world.set_parent(walker, Some(parent), false).is_err());
    assert!(world.object(body).unwrap().parent.is_none());
    assert!(world.object(walker).unwrap().parent.is_none());
    world.set_parent(kinematic, Some(parent), true).unwrap();
    assert_eq!(world.object(kinematic).unwrap().parent, Some(parent));
}

#[test]
fn children_list_direct_children_in_document_order() {
    let mut world = world(HIERARCHY);
    let parent = handle(&world, "parent");
    let child = handle(&world, "child");
    assert_eq!(world.children(parent).unwrap(), vec![child]);
    assert!(world.children(handle(&world, "camera")).unwrap().is_empty());
    world.despawn_many(&[child]).unwrap();
    assert_eq!(world.children(parent), Some(Vec::new()));
    assert!(world.children(child).is_none());
}

#[test]
fn activity_is_inherited_through_parents_and_reports_subtree_changes() {
    let mut world = world(HIERARCHY);
    let parent = handle(&world, "parent");
    let child = handle(&world, "child");
    let free = handle(&world, "free");

    world.set_active(parent, false).unwrap();

    assert_eq!(world.is_active(child), Some(true));
    assert_eq!(world.active_in_hierarchy(child), Some(false));
    assert_eq!(world.active_in_hierarchy(free), Some(true));
    let changes = world.take_activity_changes();
    assert_eq!(changes, vec![parent, child]);
    assert!(world.take_activity_changes().is_empty());

    world.set_active(parent, true).unwrap();
    assert_eq!(world.active_in_hierarchy(child), Some(true));
    assert_eq!(world.take_activity_changes(), vec![parent, child]);
}

#[test]
fn activity_change_to_the_same_value_is_not_reported() {
    let mut world = world(HIERARCHY);
    let free = handle(&world, "free");
    world.set_active(free, true).unwrap();
    assert!(world.take_activity_changes().is_empty());
}

#[test]
fn reparenting_under_an_inactive_parent_changes_hierarchy_activity() {
    let mut world = world(HIERARCHY);
    let parent = handle(&world, "parent");
    let free = handle(&world, "free");
    world.set_active(parent, false).unwrap();
    world.take_activity_changes();

    world.set_parent(free, Some(parent), true).unwrap();
    assert_eq!(world.active_in_hierarchy(free), Some(false));
    assert_eq!(world.take_activity_changes(), vec![free]);
}

#[test]
fn hiding_an_object_with_the_active_camera_is_rejected() {
    let mut world = world(CAMERA);
    let camera = handle(&world, "camera");
    let error = world.set_active(camera, false).unwrap_err();
    assert!(error.to_string().contains("активной камерой"), "{error}");
    assert_eq!(world.is_active(camera), Some(true));
}

#[test]
fn inactive_objects_are_absent_from_the_frame_and_their_children_too() {
    let scene = "[[assets]]\npath = \"/base.fbx\"\n\
[[objects]]\nid = \"shown\"\n[objects.mesh_renderer]\nmodel = \"/base.fbx\"\n\
[[objects]]\nid = \"hidden\"\nactive = false\n[objects.mesh_renderer]\nmodel = \"/base.fbx\"\n\
[[objects]]\nid = \"hidden_child\"\nparent = \"hidden\"\n[objects.mesh_renderer]\nmodel = \"/base.fbx\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    let mut world = world(scene);
    let shown = handle(&world, "shown");
    let meshes = |world: &mut World| {
        world
            .build_frame()
            .0
            .meshes
            .into_iter()
            .map(|mesh| mesh.object)
            .collect::<Vec<_>>()
    };
    assert_eq!(meshes(&mut world), vec![shown]);

    let hidden = handle(&world, "hidden");
    world.set_active(hidden, true).unwrap();
    assert_eq!(meshes(&mut world).len(), 3);
}

#[test]
fn inactive_directional_light_drops_the_cached_light() {
    let scene = "[[objects]]\nid = \"sun\"\n[objects.directional_light]\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    let mut world = world(scene);
    let sun = handle(&world, "sun");
    assert_eq!(world.build_frame().0.light.color, [1.0; 3]);

    world.set_active(sun, false).unwrap();
    let (frame, _) = world.build_frame();
    assert_eq!(frame.light.color, [0.0; 3]);
    assert!(frame.environment.is_none());
}

#[test]
fn inactive_camera_object_does_not_break_the_frame() {
    let scene = "[[objects]]\nid = \"camera\"\nactive = false\n[objects.camera]\n\
[[objects]]\nid = \"spare\"\n[objects.camera]\nactive = true\n";
    let mut world = world(scene);
    let camera = handle(&world, "camera");
    let spare = handle(&world, "spare");
    assert_eq!(world.active_in_hierarchy(camera), Some(false));
    assert_eq!(world.active_camera(), spare);
    assert!(world.build_frame().0.camera.position.is_finite());
}

#[test]
fn names_and_tags_find_objects_including_inactive_ones() {
    let scene = "[[objects]]\nid = \"a\"\nname = \"Crate\"\ntag = \"prop\"\n\
[[objects]]\nid = \"b\"\nname = \"Crate\"\ntag = \"prop\"\nactive = false\n\
[[objects]]\nid = \"c\"\ntag = \"enemy\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    let world = world(scene);
    let a = handle(&world, "a");
    let b = handle(&world, "b");
    let c = handle(&world, "c");
    assert_eq!(world.find_by_name("Crate"), Some(a));
    assert_eq!(world.find_all_by_name("Crate"), vec![a, b]);
    assert_eq!(world.find_by_name("camera"), Some(handle(&world, "camera")));
    assert_eq!(world.find_with_tag("prop"), Some(a));
    assert_eq!(world.find_all_with_tag("prop"), vec![a, b]);
    assert_eq!(world.find_all_with_tag("enemy"), vec![c]);
    assert!(world.find_all_with_tag("").is_empty());
    assert!(world.find_with_tag("missing").is_none());
}

#[test]
fn tags_and_layers_are_validated_on_write() {
    let mut world = world(CAMERA);
    let camera = handle(&world, "camera");
    world.set_tag(camera, "player_1".to_owned()).unwrap();
    assert_eq!(world.tag(camera), Some("player_1"));
    assert!(world.set_tag(camera, "bad tag".to_owned()).is_err());
    assert_eq!(world.tag(camera), Some("player_1"));
    world.set_tag(camera, String::new()).unwrap();
    assert_eq!(world.tag(camera), Some(""));

    world.set_layer(camera, 31).unwrap();
    assert_eq!(world.layer(camera), Some(31));
    assert!(world.set_layer(camera, 32).is_err());
    assert_eq!(world.layer(camera), Some(31));
}

#[test]
fn components_report_presence_and_scripts_by_path() {
    let scene = "[[assets]]\npath = \"/door.lua\"\n\
[[assets]]\npath = \"/ui/hud.rveui\"\n\
[[objects]]\nid = \"crate\"\n[objects.collider]\n[objects.ui_canvas]\ndocument = \"/ui/hud.rveui\"\n\
[[objects.lite_scripts]]\npath = \"/door.lua\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    let world = world(scene);
    let crate_handle = handle(&world, "crate");
    assert_eq!(
        world.has_component(crate_handle, &ComponentKind::Collider),
        Some(true)
    );
    assert_eq!(
        world.has_component(crate_handle, &ComponentKind::Camera),
        Some(false)
    );
    assert_eq!(
        world.has_component(crate_handle, &ComponentKind::Script("door.lua".to_owned())),
        Some(true)
    );
    assert_eq!(
        world.has_component(crate_handle, &ComponentKind::Transform),
        Some(true)
    );
    assert_eq!(
        world.has_component(crate_handle, &ComponentKind::UiCanvas),
        Some(true)
    );
}

#[test]
fn world_position_and_rotation_setters_use_the_parent_space() {
    let mut world = world(HIERARCHY);
    let child = handle(&world, "child");
    world
        .set_world_position(child, Vec3::new(1.0, 2.0, 3.0))
        .unwrap();
    assert_vec_close(
        world.world_position(child).unwrap(),
        Vec3::new(1.0, 2.0, 3.0),
    );

    world
        .set_world_rotation(child, Vec3::new(0.0, 45.0, 0.0))
        .unwrap();
    assert_vec_close(
        world.world_rotation_deg(child).unwrap(),
        Vec3::new(0.0, 45.0, 0.0),
    );
    assert!(world.set_world_position(child, Vec3::NAN).is_err());
}

#[test]
fn apply_edit_runs_each_edit_and_rejects_bad_targets() {
    let mut world = world(HIERARCHY);
    let parent = handle(&world, "parent");
    let child = handle(&world, "child");
    world
        .apply_edit(&ObjectEdit::SetActive {
            object: parent,
            active: false,
        })
        .unwrap();
    assert_eq!(world.active_in_hierarchy(child), Some(false));
    world
        .apply_edit(&ObjectEdit::SetParent {
            object: child,
            parent: None,
            keep_world: true,
        })
        .unwrap();
    assert_eq!(world.active_in_hierarchy(child), Some(true));
    assert!(
        world
            .apply_edit(&ObjectEdit::SetWorldPosition {
                object: child,
                position: Vec3::INFINITY,
            })
            .is_err()
    );
}

#[test]
fn keep_on_load_can_only_be_set_on_a_root() {
    let mut world = world(HIERARCHY);
    let free = handle(&world, "free");
    let child = handle(&world, "child");
    world
        .apply_edit(&ObjectEdit::SetKeepOnLoad {
            object: free,
            keep: true,
        })
        .unwrap();
    assert_eq!(world.keep_on_load(free), Some(true));
    assert!(
        world
            .apply_edit(&ObjectEdit::SetKeepOnLoad {
                object: child,
                keep: true,
            })
            .is_err()
    );
    assert_eq!(world.keep_on_load(child), Some(false));
    assert!(!world.set_keep_on_load(child, true));
    assert_eq!(world.keep_on_load(child), Some(false));
}

#[test]
fn append_persistent_scene_subtree_keeps_handles_parenting_and_physics_components() {
    let previous = world(
        "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
         [[objects]]\nid = \"persistent\"\nkeep_on_load = true\n\
         [[objects]]\nid = \"body\"\nparent = \"persistent\"\n[objects.collider]\n[objects.rigid_body]\nkind = \"kinematic\"\n",
    );
    let mut next = world(CAMERA);
    let root = handle(&previous, "persistent");
    let body = handle(&previous, "body");

    assert_eq!(
        next.append_persistent_from(&previous).unwrap(),
        vec![root, body]
    );
    assert_eq!(next.object(root).unwrap().id, "persistent");
    assert_eq!(next.object(body).unwrap().parent, Some(root));
    assert!(next.object(body).unwrap().rigid_body.is_some());
    assert!(next.object(body).unwrap().collider.is_some());
}

#[test]
fn persistent_scene_collision_fails_before_the_new_world_changes() {
    let previous = world(
        "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
         [[objects]]\nid = \"persistent\"\nkeep_on_load = true\n",
    );
    let mut next = world(
        "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
         [[objects]]\nid = \"other\"\n",
    );

    let error = next.append_persistent_from(&previous).unwrap_err();
    assert!(error.to_string().contains("handle collides"), "{error}");
    assert_eq!(next.objects().len(), 2);
    assert!(next.object_by_id("persistent").is_none());
}

#[test]
fn persistent_scene_rejects_references_outside_the_kept_subtree() {
    let previous = world(
        "[[assets]]\npath = \"/keep.lua\"\n\
         [[objects]]\nid = \"camera\"\n[objects.camera]\n\
         [[objects]]\nid = \"persistent\"\nkeep_on_load = true\n\
         [[objects.lite_scripts]]\npath = \"/keep.lua\"\nparams = { target = { object = \"temporary\" } }\n\
         [[objects]]\nid = \"temporary\"\n",
    );
    let mut next = world(CAMERA);

    let error = next.append_persistent_from(&previous).unwrap_err();
    assert!(
        error.to_string().contains("nonpersistent object"),
        "{error}"
    );
    assert_eq!(next.objects().len(), 1);
}

#[test]
fn serde_round_trips_new_fields_and_old_files_load_with_defaults() {
    let scene = "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
[[objects]]\nid = \"door\"\nactive = false\ntag = \"interactive\"\nlayer = 3\n";
    let document = SceneDocument::parse("serde.rvescene", scene).unwrap();
    let door = &document.objects[1];
    assert!(!door.active);
    assert_eq!(door.tag, "interactive");
    assert_eq!(door.layer, 3);

    let text = document.to_toml().unwrap();
    let reloaded = SceneDocument::parse("serde.rvescene", &text).unwrap();
    assert!(!reloaded.objects[1].active);
    assert_eq!(reloaded.objects[1].tag, "interactive");
    assert_eq!(reloaded.objects[1].layer, 3);

    let plain = SceneDocument::parse("plain.rvescene", CAMERA).unwrap();
    let plain_text = plain.to_toml().unwrap();
    // Only the camera component serializes its own `active` flag.
    assert_eq!(plain_text.matches("active").count(), 1);
    assert!(!plain_text.contains("tag"));
    assert!(!plain_text.contains("layer"));
    assert!(plain.objects[0].active);
}

#[test]
fn document_rejects_invalid_tags_and_layers() {
    let bad_tag = "[[objects]]\nid = \"camera\"\ntag = \"has space\"\n[objects.camera]\n";
    assert!(SceneDocument::parse("bad.rvescene", bad_tag).is_err());
    let bad_layer = "[[objects]]\nid = \"camera\"\nlayer = 32\n[objects.camera]\n";
    assert!(SceneDocument::parse("bad.rvescene", bad_layer).is_err());
}

#[test]
fn initial_active_camera_is_not_taken_from_a_hidden_subtree() {
    let scene = world(
        "[[objects]]\nid = \"rig\"\nactive = false\n\
         [[objects]]\nid = \"hidden_cam\"\nparent = \"rig\"\n[objects.camera]\nactive = true\n\
         [[objects]]\nid = \"main\"\n[objects.camera]\n",
    );
    assert_eq!(scene.active_camera(), handle(&scene, "main"));
}

#[test]
fn set_parent_refuses_to_put_a_keep_on_load_root_under_any_parent() {
    let scene = "[[objects]]\nid = \"persistent_parent\"\nkeep_on_load = true\n\
         [[objects]]\nid = \"scene_parent\"\n\
         [[objects]]\nid = \"persistent\"\nkeep_on_load = true\n\
         [[objects]]\nid = \"nonpersistent\"\n\
         [[objects]]\nid = \"persistent_child\"\nkeep_on_load = true\n\
         [[objects]]\nid = \"camera\"\n[objects.camera]\n";
    for parent_id in ["persistent_parent", "scene_parent"] {
        let mut world = world(scene);
        let persistent = handle(&world, "persistent");
        let parent = handle(&world, parent_id);

        let error = world
            .set_parent(persistent, Some(parent), true)
            .expect_err("keep_on_load is only valid on a root");

        assert!(error.to_string().contains("keep_on_load"), "{error}");
        assert_eq!(
            world.object(persistent).unwrap().parent,
            None,
            "{parent_id}"
        );
        assert_eq!(world.keep_on_load(persistent), Some(true), "{parent_id}");
        assert_eq!(world.children(parent), Some(Vec::new()), "{parent_id}");
    }
}

#[test]
fn set_parent_still_reparents_a_nonpersistent_object_under_a_persistent_root() {
    let mut world = world(
        "[[objects]]\nid = \"persistent\"\nkeep_on_load = true\n\
         [[objects]]\nid = \"nonpersistent\"\n\
         [[objects]]\nid = \"camera\"\n[objects.camera]\n",
    );
    let persistent = handle(&world, "persistent");
    let nonpersistent = handle(&world, "nonpersistent");

    world
        .set_parent(nonpersistent, Some(persistent), false)
        .unwrap();

    assert_eq!(
        world.object(nonpersistent).unwrap().parent,
        Some(persistent)
    );
}
/// `rock` и его родитель `base` статические, `lamp` под `base` и `free` — нет.
const STATIC_SCENE: &str = "[[objects]]\nid = \"base\"\nstatic = true\n\
[[objects]]\nid = \"rock\"\nparent = \"base\"\nstatic = true\n[objects.collider]\n[objects.rigid_body]\nkind = \"static\"\n\
[[objects]]\nid = \"lamp\"\nparent = \"base\"\n\
[[objects]]\nid = \"free\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n";

#[test]
fn static_object_pose_and_parent_cannot_change() {
    let mut world = world(STATIC_SCENE);
    let rock = handle(&world, "rock");
    let free = handle(&world, "free");
    let before = world.world_matrix(rock).unwrap();
    assert_eq!(world.is_static(rock), Some(true));
    assert_eq!(world.is_static(free), Some(false));

    let mut transform = *world.local_transform(rock).unwrap();
    transform.position = Vec3::ONE;
    let errors = [
        world.set_local_transform(rock, transform).unwrap_err(),
        world
            .set_root_pose(handle(&world, "base"), Vec3::ONE, glam::Quat::IDENTITY)
            .unwrap_err(),
        world.check_world_position(rock, Vec3::ONE).unwrap_err(),
        world.check_world_rotation(rock, Vec3::ONE).unwrap_err(),
        world
            .apply_edit(&ObjectEdit::SetWorldPosition {
                object: rock,
                position: Vec3::ONE,
            })
            .unwrap_err(),
        world
            .apply_edit(&ObjectEdit::SetWorldRotation {
                object: rock,
                rotation_deg: Vec3::ONE,
            })
            .unwrap_err(),
        world.set_parent(rock, Some(free), true).unwrap_err(),
        world.set_parent(rock, None, false).unwrap_err(),
    ];
    for error in errors {
        assert!(error.message.contains("статического объекта"), "{error}");
    }
    let mut body = *world.object(rock).unwrap().rigid_body.as_ref().unwrap();
    body.kind = crate::RigidBodyKind::Kinematic;
    assert!(world.set_rigid_body(rock, body).is_err());
    assert_eq!(world.world_matrix(rock).unwrap(), before);
}

#[test]
fn static_object_can_be_deactivated_and_its_dynamic_children_move() {
    let mut world = world(STATIC_SCENE);
    let rock = handle(&world, "rock");
    let lamp = handle(&world, "lamp");
    let free = handle(&world, "free");

    world.set_active(rock, false).unwrap();
    world
        .set_world_position(lamp, Vec3::new(1.0, 2.0, 3.0))
        .unwrap();
    world.set_parent(lamp, Some(free), true).unwrap();
    world.set_parent(free, Some(rock), true).unwrap();

    assert_vec_close(
        world.world_position(lamp).unwrap(),
        Vec3::new(1.0, 2.0, 3.0),
    );
}
