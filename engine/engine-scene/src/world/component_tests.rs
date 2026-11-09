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
