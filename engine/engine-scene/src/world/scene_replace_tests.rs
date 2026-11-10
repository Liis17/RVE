use super::{ObjectHandle, World};
use crate::document::SceneDocument;
use glam::Vec3;

const PERSISTENT_ROOT: &str = concat!(
    "[[objects]]\nid = \"keep\"\nkeep_on_load = true\n",
    "[[objects]]\nid = \"kid\"\nparent = \"keep\"\n",
);

fn world(scene: &str) -> World {
    World::from_document(&SceneDocument::parse("replace.rvescene", scene).unwrap()).unwrap()
}

fn handle(world: &World, id: &str) -> ObjectHandle {
    world
        .handle(id)
        .unwrap_or_else(|| panic!("no live object {id}"))
}

#[test]
fn replacement_keeps_persistent_handles_and_drops_nonpersistent_objects() {
    let mut current = world(&format!(
        "{PERSISTENT_ROOT}[[objects]]\nid = \"old\"\n[[objects]]\nid = \"cam_old\"\n[objects.camera]\n"
    ));
    let keep = handle(&current, "keep");
    let kid = handle(&current, "kid");
    let old = handle(&current, "old");
    let next =
        world("[[objects]]\nid = \"cam_new\"\n[objects.camera]\n[[objects]]\nid = \"new_obj\"\n");

    let replacement = current.replace_nonpersistent(next).unwrap();

    assert_eq!(handle(&current, "keep"), keep);
    assert_eq!(handle(&current, "kid"), kid);
    assert!(!current.is_alive(old));
    assert!(replacement.removed.contains(&old));
    assert!(
        replacement
            .removed
            .iter()
            .all(|handle| !current.is_alive(*handle))
    );
    let new_camera = handle(&current, "cam_new");
    assert_eq!(current.active_camera(), new_camera);
    assert!(replacement.added.contains(&new_camera));
    assert!(replacement.added.contains(&handle(&current, "new_obj")));
    assert_eq!(current.objects().len(), 4);
}

#[test]
fn persistent_active_camera_stays_active_after_replacement() {
    let mut current = world(
        "[[objects]]\nid = \"cam_keep\"\nkeep_on_load = true\n[objects.camera]\nactive = true\n[[objects]]\nid = \"cam_old\"\n[objects.camera]\n",
    );
    let persistent_camera = handle(&current, "cam_keep");
    current.set_active_camera(persistent_camera).unwrap();
    let next = world("[[objects]]\nid = \"cam_new\"\n[objects.camera]\n");

    current.replace_nonpersistent(next).unwrap();

    assert_eq!(current.active_camera(), persistent_camera);
    assert!(current.handle("cam_new").is_some());
}

#[test]
fn replacement_rejects_a_scene_id_that_collides_with_a_persistent_object_without_mutation() {
    let mut current = world(&format!(
        "{PERSISTENT_ROOT}[[objects]]\nid = \"old\"\n[[objects]]\nid = \"cam\"\n[objects.camera]\n"
    ));
    let before = current.clone();
    let next = world("[[objects]]\nid = \"keep\"\n[objects.camera]\n");

    let error = current.replace_nonpersistent(next).unwrap_err();

    assert!(error.to_string().contains("keep"), "{error}");
    assert_eq!(current.objects().len(), before.objects().len());
    for object in before.objects() {
        assert_eq!(current.handle(&object.id), Some(object.handle));
    }
}

#[test]
fn replacement_rejects_a_persistent_reference_to_a_nonpersistent_object() {
    let mut current = world(&format!(
        "[[assets]]\npath = \"/keep.lua\"\n{PERSISTENT_ROOT}[[objects.lite_scripts]]\npath = \"/keep.lua\"\nparams = {{ target = {{ object = \"old\" }} }}\n[[objects]]\nid = \"old\"\n[[objects]]\nid = \"cam\"\n[objects.camera]\n"
    ));
    let old = handle(&current, "old");
    let next = world("[[objects]]\nid = \"cam_new\"\n[objects.camera]\n");

    let error = current.replace_nonpersistent(next).unwrap_err();

    assert!(error.to_string().contains("\"old\""), "{error}");
    assert!(current.is_alive(old));
}

#[test]
fn replacement_reuses_nonpersistent_ids_and_remaps_hierarchy() {
    let mut current =
        world("[[objects]]\nid = \"old\"\n[[objects]]\nid = \"cam\"\n[objects.camera]\n");
    let old = handle(&current, "old");
    let next = world(concat!(
        "[[objects]]\nid = \"cam\"\n[objects.camera]\n",
        "[[objects]]\nid = \"a\"\n[objects.transform]\nposition = [1.0, 0.0, 0.0]\n",
        "[[objects]]\nid = \"b\"\nparent = \"a\"\n[objects.transform]\nposition = [0.0, 2.0, 0.0]\n",
    ));

    current.replace_nonpersistent(next).unwrap();

    let a = handle(&current, "a");
    let b = handle(&current, "b");
    assert!(!current.is_alive(old));
    assert_eq!(current.object(b).unwrap().parent, Some(a));
    let world_b = current
        .world_matrix(b)
        .unwrap()
        .transform_point3(Vec3::ZERO);
    assert!(
        (world_b - Vec3::new(1.0, 2.0, 0.0)).length() < 1e-5,
        "{world_b:?}"
    );
}
