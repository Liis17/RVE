use super::resolve_particle_commands;
use engine_scene::world::ParticleCommand;
use engine_scene::{ObjectHandle, SceneDocument, World};
use std::collections::HashMap;

const SCENE: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
[[objects]]\nid = \"fire\"\n[objects.particle_emitter]\n\
[[objects]]\nid = \"crate\"\n";

#[test]
fn commands_resolve_to_their_emitter_and_skip_objects_without_one() {
    let world =
        World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap();
    let fire: ObjectHandle = world.handle("fire").unwrap();
    let crate_handle: ObjectHandle = world.handle("crate").unwrap();
    let emitters = HashMap::from([(fire, 7_u32)]);
    let resolved = resolve_particle_commands(
        &emitters,
        &[
            ParticleCommand::Burst {
                object: fire,
                count: 4,
            },
            ParticleCommand::Restart { object: fire },
            ParticleCommand::Burst {
                object: crate_handle,
                count: 9,
            },
        ],
    );
    assert_eq!(resolved, vec![(7, Some(4)), (7, None)]);
}
