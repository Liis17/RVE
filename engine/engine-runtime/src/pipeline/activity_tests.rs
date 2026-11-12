use super::*;
use engine_scene::SceneDocument;

const SPEAKER_SCENE: &str = "[[assets]]\npath = \"/audio/hum.wav\"\n\
        [[objects]]\nid = \"camera\"\n[objects.camera]\n\
        [[objects]]\nid = \"ball\"\n[objects.collider]\n\
        [objects.audio_source]\nclip = \"/audio/hum.wav\"\nplay_on_start = true\n";

#[test]
fn hiding_removes_body_and_sound_and_showing_recreates_body_and_restarts_sound() {
    let document = SceneDocument::parse("activity.rvescene", SPEAKER_SCENE).unwrap();
    let mut world = World::from_document(&document).unwrap();
    let ball = world.handle("ball").unwrap();
    let mut sync = PhysicsSceneSync::new(&world, Vec::new()).unwrap();
    let mut spawn = SpawnPhysics::default();
    let inserted = sync
        .insert_objects(&world, &[ball], &spawn.geometry, &mut spawn.shapes)
        .unwrap();
    assert_eq!(inserted.len(), 1);
    let body = sync.bindings()[0].body;

    world.set_active(ball, false).unwrap();
    let hidden = apply_activity_changes(&mut world, Some(&mut sync), &mut spawn);
    assert!(
        matches!(hidden.as_slice(), [AudioRequest::StopSource { object, .. }] if *object == ball),
        "{hidden:?}"
    );
    assert_eq!(spawn.changes, [BodyChange::Remove { handle: body }]);
    assert!(sync.bindings().is_empty());

    spawn.changes.clear();
    world.set_active(ball, true).unwrap();
    let shown = apply_activity_changes(&mut world, Some(&mut sync), &mut spawn);
    assert!(
        matches!(shown.as_slice(), [AudioRequest::PlaySource { object, .. }] if *object == ball),
        "{shown:?}"
    );
    assert!(
        matches!(spawn.changes.as_slice(), [BodyChange::Insert { .. }]),
        "{:?}",
        spawn.changes
    );
    assert_eq!(sync.bindings().len(), 1);
}

#[test]
fn destroying_an_inactive_object_removes_no_body_and_is_not_double_removed() {
    let document = SceneDocument::parse("activity.rvescene", SPEAKER_SCENE).unwrap();
    let mut world = World::from_document(&document).unwrap();
    let ball = world.handle("ball").unwrap();
    let mut sync = PhysicsSceneSync::new(&world, Vec::new()).unwrap();
    let mut spawn = SpawnPhysics::default();
    sync.insert_objects(&world, &[ball], &spawn.geometry, &mut spawn.shapes)
        .unwrap();
    let body = sync.bindings()[0].body;

    world.set_active(ball, false).unwrap();
    apply_activity_changes(&mut world, Some(&mut sync), &mut spawn);
    spawn.changes.clear();
    world.despawn_many(&[ball]).unwrap();
    let removed = sync.remove_objects(&[ball]);
    assert!(removed.is_empty(), "{removed:?}");
    assert!(spawn.changes.is_empty());
    assert_eq!(sync.object_for_body(body), Some(ball));
}
