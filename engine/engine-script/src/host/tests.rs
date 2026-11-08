use super::{
    ScriptHost, ScriptInterruptHandle, ScriptLoadError, ScriptLoadOptions, ScriptUpdateProfile,
};
use crate::{
    Key, MouseButton, ScriptCursor, ScriptEventLevel, ScriptInput, ScriptLibrary, ScriptTime,
};
use engine_console::CommandLine;
use engine_physics::{
    BodyDesc, BodyHandle, BodyKind, CharacterController, ColliderDesc, MAX_PUSH_SPEED,
    PhysicsCommand, PhysicsSceneBinding, PhysicsWorld, Pose, Shape, StepDriver,
};
use engine_project::{InputAction, InputSettings};
use engine_scene::{LocalTransform, SceneDocument, World};
use glam::{Quat, Vec3};
use rve_sdk::{Context, Params, Script, Value};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn world(source: &str) -> World {
    let source = if source.contains("[objects.camera]") {
        source.to_owned()
    } else {
        format!("{source}\n[[objects]]\nid = \"__test_camera\"\n[objects.camera]\n\n")
    };
    let document = SceneDocument::parse("test.rvescene", &source).unwrap();
    World::from_document(&document).unwrap()
}

fn one_camera(body: &str) -> World {
    world(&format!(
        r#"
[[objects]]
id = "camera"
name = "Camera object"
[objects.camera]
{body}
"#
    ))
}

fn two_cameras_with_lite_script() -> World {
    world(
        r#"
[[assets]]
path = "/camera.lua"
[[objects]]
id = "camera"
name = "Primary"
[objects.camera]
active = true
[[objects.lite_scripts]]
path = "/camera.lua"
[[objects]]
id = "second"
name = "Secondary"
[objects.camera]
[[objects]]
id = "plain"
"#,
    )
}

fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
        .collect()
}

fn action_settings() -> InputSettings {
    InputSettings {
        default_context: Some("game".to_owned()),
        contexts: BTreeMap::from([
            (
                "game".to_owned(),
                BTreeMap::from([
                    (
                        "Jump".to_owned(),
                        InputAction::Button {
                            inputs: vec!["Space".to_owned(), "W".to_owned()],
                        },
                    ),
                    (
                        "MoveX".to_owned(),
                        InputAction::Axis {
                            negative: vec!["A".to_owned()],
                            positive: vec!["D".to_owned()],
                            analog: Some("MouseX".to_owned()),
                            scale: 0.25,
                        },
                    ),
                    (
                        "Move".to_owned(),
                        InputAction::Vector2 {
                            up: vec!["W".to_owned()],
                            down: vec!["S".to_owned()],
                            left: vec!["A".to_owned()],
                            right: vec!["D".to_owned()],
                            analog: Some("MouseDelta".to_owned()),
                            scale: 0.5,
                        },
                    ),
                ]),
            ),
            (
                "menu".to_owned(),
                BTreeMap::from([(
                    "Confirm".to_owned(),
                    InputAction::Button {
                        inputs: vec!["Enter".to_owned()],
                    },
                )]),
            ),
        ]),
    }
}

fn load_with_input(
    world: &World,
    sources: &BTreeMap<String, String>,
    input: InputSettings,
) -> ScriptHost {
    ScriptHost::load_with_options(
        world,
        "scene",
        sources,
        None,
        ScriptLoadOptions {
            input,
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn lua_input_actions_read_values_bindings_and_delayed_mutations() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
local function require_value(value, message)
  if not value then error(message) end
end
function Input:on_update()
  if rve.input.context() == "game" then
    require_value(rve.input.action_down("Jump"), "Jump down missing")
    require_value(rve.input.action_pressed("Jump"), "Jump press missing")
    require_value(not rve.input.action_released("Jump"), "unexpected Jump release")
    require_value(math.abs(rve.input.axis("MoveX") - 1.5) < 0.0001, "axis value mismatch")
    local x, y = rve.input.vector("Move")
    require_value(math.abs(x - 1.7071068) < 0.0001, "vector x mismatch")
    require_value(math.abs(y - 0.20710677) < 0.0001, "vector y mismatch")
    local bindings = rve.input.bindings("game", "Jump", "inputs")
    require_value(bindings[1] == "Space" and bindings[2] == "W", "bindings mismatch")
    local presses = {}
    for _, name in ipairs(rve.input.pressed_inputs()) do presses[name] = true end
    require_value(presses.Space and presses.W and presses.D, "pressed input names mismatch")
    rve.input.set_context("menu")
    rve.input.rebind("game", "Jump", "inputs", { "Enter" })
    require_value(rve.input.context() == "game", "context changed before next input frame")
    rve.log_info("game action snapshot")
  else
    require_value(rve.input.context() == "menu", "menu context missing")
    require_value(not rve.input.action_down("Jump"), "inactive action stayed down")
    require_value(rve.input.action_released("Jump"), "context switch release missing")
    local bindings = rve.input.bindings("game", "Jump", "inputs")
    require_value(bindings[1] == "Enter" and bindings[2] == nil, "rebind missing")
    rve.log_info("menu action snapshot")
  end
end
return Input
"#;
    let mut host = load_with_input(
        &world,
        &sources(&[("input.lua", source)]),
        action_settings(),
    );
    let mut input = ScriptInput::default();
    input.set_key(Key::Space, true);
    input.set_key(Key::W, true);
    input.set_key(Key::D, true);
    input.add_mouse_delta(2.0, -1.0);
    host.set_input(input);
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "game action snapshot"
    }));

    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "game action snapshot"
    }));

    input.clear_frame_transients();
    host.set_input(input);
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "menu action snapshot"
    }));
}

#[test]
fn lua_input_actions_reject_unknown_and_wrong_kind_queries() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
local function rejects(callback, expected)
  local ok, message = pcall(callback)
  if ok or not string.find(tostring(message), expected, 1, true) then
    error("expected input error containing " .. expected)
  end
end
function Input:on_update()
  if rve.input.action_down("Confirm") then error("inactive action is down") end
  rejects(function() rve.input.axis("Jump") end, "expected Axis")
  rejects(function() rve.input.action_down("MoveX") end, "expected Button")
  rejects(function() rve.input.vector("Missing") end, "unknown input action")
  rejects(function() rve.input.bindings("missing", "Jump", "inputs") end, "unknown input context")
  rejects(function() rve.input.bindings("game", "MoveX", "inputs") end, "unknown Axis input slot")
  rejects(function() rve.input.rebind("game", "Jump", "inputs", { "Unknown" }) end, "unknown physical input")
  rve.log_info("input action validation passed")
end
return Input
"#;
    let mut host = load_with_input(
        &world,
        &sources(&[("input.lua", source)]),
        action_settings(),
    );
    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    assert!(
        events.iter().any(|event| {
            event.level == ScriptEventLevel::Info
                && event.message == "input action validation passed"
        }),
        "unexpected input validation events: {events:?}"
    );
}

#[test]
fn lua_input_mutations_are_rolled_back_when_a_callback_fails() {
    let mut world = world(
        r#"
[[assets]]
path = "/bad.lua"
[[assets]]
path = "/observe.lua"
[[objects]]
id = "bad"
[[objects.lite_scripts]]
path = "/bad.lua"
[[objects]]
id = "observer"
[[objects.lite_scripts]]
path = "/observe.lua"
"#,
    );
    let bad = r#"
local Input = {}
function Input:on_update()
    rve.input.set_context("menu")
    error("input transaction failure")
end
return Input
"#;
    let observer = r#"
local Input = {}
function Input:on_update()
  if rve.input.context() == "game" then
    rve.log_info("rollback kept game context")
  else
    rve.log_error("input mutation escaped rollback")
  end
end
return Input
"#;
    let mut host = load_with_input(
        &world,
        &sources(&[("bad.lua", bad), ("observe.lua", observer)]),
        action_settings(),
    );
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event.message.contains("input transaction failure")
    }));
    host.drain_events();

    let input = ScriptInput::default();
    host.set_input(input);
    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "rollback kept game context"
    }));
    assert!(
        events
            .iter()
            .all(|event| { event.message != "input mutation escaped rollback" })
    );
}

#[test]
fn lua_physics_queries_and_commands_use_the_current_snapshot_and_commit_together() {
    let mut world = world(
        r#"
[[assets]]
path = "/physics.lua"
[[assets]]
path = "/reader.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/physics.lua"
[[objects.lite_scripts]]
path = "/reader.lua"
[[objects]]
id = "crate"
[[objects]]
id = "trigger"
"#,
    );
    let player = world.handle("player").unwrap();
    let crate_handle = world.handle("crate").unwrap();
    let trigger = world.handle("trigger").unwrap();
    let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let player_body = physics
        .insert_body({
            let mut desc = BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap(),
                ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let crate_body = physics
        .insert_body(BodyDesc::dynamic(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(5.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        ))
        .unwrap();
    let trigger_body = physics
        .insert_body({
            let mut desc = BodyDesc::static_collider(
                Shape::sphere(0.5).unwrap(),
                Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            );
            desc.collider.trigger = true;
            desc
        })
        .unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: player,
            body: player_body,
            kind: BodyKind::Kinematic,
            character_controller: Some(controller),
        },
        PhysicsSceneBinding {
            object: crate_handle,
            body: crate_body,
            kind: BodyKind::Dynamic,
            character_controller: None,
        },
        PhysicsSceneBinding {
            object: trigger,
            body: trigger_body,
            kind: BodyKind::Static,
            character_controller: None,
        },
    ];
    let snapshot = physics.query_snapshot().unwrap();
    let physics_script = r#"
local Script = {}
function Script:on_update()
  local hit = rve.physics.raycast({x=0,y=0,z=0}, {x=2,y=0,z=0}, 10)
  assert(hit.object.id == "crate" and hit.distance > 4)
  local trigger_hit = rve.physics.raycast({x=0,y=0,z=0}, {x=1,y=0,z=0}, 10,
      {include_triggers=true})
  assert(trigger_hit.object.id == "trigger")
  local ignored_trigger = rve.physics.raycast({x=0,y=0,z=0}, {x=1,y=0,z=0}, 10,
      {ignore=rve.find("trigger")})
  assert(ignored_trigger.object.id == "crate")
  local crate = rve.find("crate")
  local velocity = crate:get_velocity()
  assert(velocity.x == 0 and velocity.y == 0 and velocity.z == 0)
  crate:set_velocity({x=1,y=2,z=3})
  crate:add_impulse({x=0,y=1,z=0})
  crate:add_force({x=0,y=0,z=4})
  crate:set_collider_enabled(false)
  assert(not crate:is_collider_enabled())
end
return Script
"#;
    let reader_script = r#"
local Script = {}
function Script:on_update()
  local crate = rve.find("crate")
  assert(not crate:is_collider_enabled())
  assert(rve.physics.raycast({x=0,y=0,z=0}, {x=1,y=0,z=0}, 10) == nil)
  rve.log_info("physics overlay visible")
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[
            ("physics.lua", physics_script),
            ("reader.lua", reader_script),
        ]),
        None,
    )
    .unwrap();
    host.set_physics_snapshot(snapshot, &bindings);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let commands = host.drain_physics_commands();
    assert_eq!(commands.len(), 4);
    assert!(
        matches!(commands[0], PhysicsCommand::SetVelocity { body, velocity }
        if body == crate_body && velocity == Vec3::new(1.0, 2.0, 3.0))
    );
    assert!(
        matches!(commands[1], PhysicsCommand::AddImpulse { body, impulse, .. }
        if body == crate_body && impulse == Vec3::Y)
    );
    assert!(
        matches!(commands[2], PhysicsCommand::AddForce { body, force, .. }
        if body == crate_body && force == Vec3::new(0.0, 0.0, 4.0))
    );
    assert!(
        matches!(commands[3], PhysicsCommand::SetColliderEnabled { body, enabled: false }
        if body == crate_body)
    );
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message == "physics overlay visible")
    );
}

#[test]
fn character_move_and_set_transform_follow_callback_order() {
    let mut world = world(
        r#"
[[assets]]
path = "/order.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/order.lua"
"#,
    );
    let player = world.handle("player").unwrap();
    let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let body = physics
        .insert_body({
            let mut desc = BodyDesc::new(
                BodyKind::Kinematic,
                Pose::IDENTITY,
                ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let bindings = [PhysicsSceneBinding {
        object: player,
        body,
        kind: BodyKind::Kinematic,
        character_controller: Some(controller),
    }];
    let source = r#"
local Script = {}
function Script:on_update()
  local player = rve.find("player")
  local transform = player:get_transform()
  transform.position = {x=2,y=0,z=0}
  player:set_transform(transform)
  player:move({x=1,y=0,z=0})
  assert(player:get_transform().position.x == 3)

  transform = player:get_transform()
  transform.position = {x=5,y=0,z=0}
  player:set_transform(transform)
  assert(player:get_transform().position.x == 5)
  player:move({x=2,y=0,z=0})
  assert(player:get_transform().position.x == 7)
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("order.lua", source)]), None).unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert_eq!(world.local_transform(player).unwrap().position.x, 7.0);
    assert!(host.drain_physics_commands().is_empty());
}

#[test]
fn limited_character_move_warns_once_and_keeps_the_script_running() {
    // Веер из MAX_MESH_CANDIDATES + 1 треугольников в точке капсулы: численный лимит на каждом move.
    let mut world = world(
        r#"
[[assets]]
path = "/limited.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/limited.lua"
"#,
    );
    let player = world.handle("player").unwrap();
    let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let body = physics
        .insert_body({
            let mut desc = BodyDesc::new(
                BodyKind::Kinematic,
                Pose::IDENTITY,
                ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let ring = engine_physics::MAX_MESH_CANDIDATES + 1;
    let mut vertices = vec![Vec3::new(0.0, 0.5, 0.0)];
    vertices.extend((0..ring).map(|index| {
        let angle = index as f32 / ring as f32 * std::f32::consts::TAU;
        Vec3::new(0.1 * angle.cos(), 0.5, 0.1 * angle.sin())
    }));
    let triangles = (0..ring)
        .map(|index| [0, index as u32 + 1, ((index + 1) % ring) as u32 + 1])
        .collect();
    physics
        .insert_body(BodyDesc::static_collider(
            Shape::TriMesh(std::sync::Arc::new(
                engine_physics::TriMesh::new(vertices, triangles).unwrap(),
            )),
            Pose::IDENTITY,
        ))
        .unwrap();
    let bindings = [PhysicsSceneBinding {
        object: player,
        body,
        kind: BodyKind::Kinematic,
        character_controller: Some(controller),
    }];
    let source = r#"
local Script = {}
function Script:on_update()
  rve.find("player"):move({x=0.1,y=0,z=0})
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("limited.lua", source)]), None).unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    for _ in 0..3 {
        host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    }

    let events = host.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error),
        "{events:?}"
    );
    let warnings = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Warn)
        .count();
    assert_eq!(warnings, 1, "{events:?}");
    assert_eq!(world.local_transform(player).unwrap().position, Vec3::ZERO);
}

#[test]
fn failed_lua_callback_discards_movement_and_dynamic_pushes() {
    let mut world = world(
        r#"
[[assets]]
path = "/mover.lua"
[[assets]]
path = "/reader.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/mover.lua"
[[objects.lite_scripts]]
path = "/reader.lua"
[[objects]]
id = "crate"
"#,
    );
    let player = world.handle("player").unwrap();
    let crate_handle = world.handle("crate").unwrap();
    let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let player_body = physics
        .insert_body({
            let mut desc = BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap(),
                ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let crate_body = physics
        .insert_body(BodyDesc::dynamic(
            Shape::cuboid(Vec3::splat(0.25)).unwrap(),
            Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        ))
        .unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: player,
            body: player_body,
            kind: BodyKind::Kinematic,
            character_controller: Some(controller),
        },
        PhysicsSceneBinding {
            object: crate_handle,
            body: crate_body,
            kind: BodyKind::Dynamic,
            character_controller: None,
        },
    ];
    let mover = r#"
local Script = {}
function Script:on_update()
  local player = rve.find("player")
  rve.find("crate"):add_impulse({x=0,y=1,z=0})
  local result = player:move({x=0.6,y=0,z=0})
  assert(result.collided)
  error("abort physics transaction")
end
return Script
"#;
    let reader = r#"
local Script = {}
function Script:on_update()
  local player = rve.find("player")
  assert(player:get_transform().position.x == 0)
  assert(not player:is_grounded())
  assert(rve.find("crate"):is_collider_enabled())
  rve.log_info("physics rollback observed")
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("mover.lua", mover), ("reader.lua", reader)]),
        None,
    )
    .unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert_eq!(world.local_transform(player).unwrap().position, Vec3::ZERO);
    assert!(host.drain_physics_commands().is_empty());
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.message == "physics rollback observed")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message.contains("abort physics transaction"))
    );
}

#[test]
fn failed_lua_callback_discards_grounded_and_collider_state() {
    let mut world = world(
        r#"
[[assets]]
path = "/mover.lua"
[[assets]]
path = "/reader.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/mover.lua"
[[objects.lite_scripts]]
path = "/reader.lua"
[[objects]]
id = "floor"
"#,
    );
    let player = world.handle("player").unwrap();
    let floor = world.handle("floor").unwrap();
    world
        .set_local_transform(
            player,
            LocalTransform {
                position: Vec3::new(0.0, 0.5, 0.0),
                ..LocalTransform::default()
            },
        )
        .unwrap();
    let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.2, 0.01, 1.0).unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let player_body = physics
        .insert_body({
            let mut desc = BodyDesc::new(
                BodyKind::Kinematic,
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let floor_body = physics
        .insert_body(BodyDesc::static_collider(
            Shape::cuboid(Vec3::new(5.0, 0.5, 5.0)).unwrap(),
            Pose::new(Vec3::new(0.0, -0.5, 0.0), Quat::IDENTITY).unwrap(),
        ))
        .unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: player,
            body: player_body,
            kind: BodyKind::Kinematic,
            character_controller: Some(controller),
        },
        PhysicsSceneBinding {
            object: floor,
            body: floor_body,
            kind: BodyKind::Static,
            character_controller: None,
        },
    ];
    let mover = r#"
local Script = {}
function Script:on_update()
  local result = rve.find("player"):move({x=0,y=-0.1,z=0})
  assert(result.grounded)
  rve.find("floor"):set_collider_enabled(false)
  error("abort grounded transaction")
end
return Script
"#;
    let reader = r#"
local Script = {}
function Script:on_update()
  local player = rve.find("player")
  assert(player:get_transform().position.y == 0.5)
  assert(not player:is_grounded())
  assert(rve.find("floor"):is_collider_enabled())
  rve.log_info("grounded rollback observed")
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("mover.lua", mover), ("reader.lua", reader)]),
        None,
    )
    .unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert_eq!(world.local_transform(player).unwrap().position.y, 0.5);
    assert!(host.drain_physics_commands().is_empty());
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.message == "grounded rollback observed")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message.contains("abort grounded transaction"))
    );
}

#[test]
fn lua_physics_rejects_wrong_components_and_non_finite_vectors() {
    let mut world = world(
        r#"
[[assets]]
path = "/wrong_controller.lua"
[[assets]]
path = "/non_finite.lua"
[[assets]]
path = "/wrong_body.lua"
[[objects]]
id = "dynamic"
[[objects.lite_scripts]]
path = "/wrong_controller.lua"
[[objects.lite_scripts]]
path = "/non_finite.lua"
[[objects]]
id = "static"
[[objects.lite_scripts]]
path = "/wrong_body.lua"
"#,
    );
    let dynamic = world.handle("dynamic").unwrap();
    let static_handle = world.handle("static").unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let dynamic_body = physics
        .insert_body(BodyDesc::dynamic(
            Shape::sphere(0.5).unwrap(),
            Pose::IDENTITY,
            1.0,
        ))
        .unwrap();
    let static_body = physics
        .insert_body(BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::X * 3.0, Quat::IDENTITY).unwrap(),
        ))
        .unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: dynamic,
            body: dynamic_body,
            kind: BodyKind::Dynamic,
            character_controller: None,
        },
        PhysicsSceneBinding {
            object: static_handle,
            body: static_body,
            kind: BodyKind::Static,
            character_controller: None,
        },
    ];
    let wrong_controller = r#"
local Script = {}
function Script:on_update()
  rve.find("dynamic"):move({x=1,y=0,z=0})
end
return Script
"#;
    let non_finite = r#"
local Script = {}
function Script:on_update()
  rve.find("dynamic"):add_force({x=math.huge,y=0,z=0})
end
return Script
"#;
    let wrong_body = r#"
local Script = {}
function Script:on_update()
  rve.find("static"):get_velocity()
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[
            ("wrong_controller.lua", wrong_controller),
            ("non_finite.lua", non_finite),
            ("wrong_body.lua", wrong_body),
        ]),
        None,
    )
    .unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert!(host.drain_physics_commands().is_empty());
    let messages = host
        .drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("does not have a character controller"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("must be a finite f32 number"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("does not have a dynamic rigid body"))
    );
}

#[test]
fn lua_physics_reports_unavailable_context_explicitly() {
    let mut world = world(
        r#"
[[assets]]
path = "/script.lua"
[[objects]]
id = "actor"
[[objects.lite_scripts]]
path = "/script.lua"
"#,
    );
    let source = r#"
local Script = {}
function Script:on_update()
  rve.physics.raycast({x=0,y=0,z=0}, {x=1,y=0,z=0}, 5)
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("script.lua", source)]), None).unwrap();
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message.contains("physics context is not attached"))
    );
}

#[test]
fn repeated_character_moves_sum_requested_velocity_once_and_zero_dt_creates_none() {
    fn run(dt: f32, start_x: f32, move_distance: f32) -> (Vec<PhysicsCommand>, Vec3, Option<Vec3>) {
        let mut world = world(
            r#"
[[assets]]
path = "/move.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/move.lua"
[[objects]]
id = "crate"
"#,
        );
        let player = world.handle("player").unwrap();
        let crate_handle = world.handle("crate").unwrap();
        world
            .set_root_pose(player, Vec3::new(start_x, 0.0, 0.0), Quat::IDENTITY)
            .unwrap();
        let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
        let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let player_body = physics
            .insert_body({
                let mut desc = BodyDesc::new(
                    BodyKind::Kinematic,
                    Pose::new(Vec3::new(start_x, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                    ColliderDesc::new(controller.capsule().unwrap()),
                );
                desc.character_controller = true;
                desc
            })
            .unwrap();
        let crate_body = physics
            .insert_body(BodyDesc::dynamic(
                Shape::cuboid(Vec3::splat(0.25)).unwrap(),
                Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let snapshot = physics.query_snapshot().unwrap();
        let start_pose = Pose::new(Vec3::new(start_x, 0.0, 0.0), Quat::IDENTITY).unwrap();
        let first_move = controller
            .move_capsule(&snapshot, player_body, start_pose, Vec3::X * move_distance)
            .unwrap();
        let second_move = controller
            .move_capsule(
                &snapshot,
                player_body,
                Pose::new(start_pose.position + first_move.translation, Quat::IDENTITY).unwrap(),
                Vec3::X * move_distance,
            )
            .unwrap();
        let mut expected_hit = None;
        let mut blocked_displacement = Vec3::ZERO;
        let mut requested_displacement = Vec3::ZERO;
        let mut normal_sum = Vec3::ZERO;
        let mut hit_count = 0;
        for hit in first_move
            .hits
            .iter()
            .chain(&second_move.hits)
            .filter(|hit| hit.body == crate_body)
        {
            hit_count += 1;
            expected_hit.get_or_insert(*hit);
            blocked_displacement += hit.blocked_displacement;
            requested_displacement += hit.requested_displacement;
            normal_sum += hit.normal * hit.blocked_displacement.length();
        }
        let expected_impulse = expected_hit.and_then(|mut hit| {
            hit.blocked_displacement = blocked_displacement;
            hit.requested_displacement = requested_displacement;
            hit.normal = normal_sum.normalize_or_zero();
            controller.push_impulse(&hit, dt)
        });
        let bindings = [
            PhysicsSceneBinding {
                object: player,
                body: player_body,
                kind: BodyKind::Kinematic,
                character_controller: Some(controller),
            },
            PhysicsSceneBinding {
                object: crate_handle,
                body: crate_body,
                kind: BodyKind::Dynamic,
                character_controller: None,
            },
        ];
        let source = format!(
            r#"
local Script = {{}}
function Script:on_update(dt)
  local player = rve.find("player")
  player:move({{x={move_distance},y=0,z=0}})
  player:move({{x={move_distance},y=0,z=0}})
end
return Script
"#
        );
        let mut host = ScriptHost::load(
            &world,
            "scene",
            &sources(&[("move.lua", source.as_str())]),
            None,
        )
        .unwrap();
        host.set_physics_snapshot(snapshot, &bindings);
        host.update(&mut world, dt, ScriptTime::default());
        assert_eq!(hit_count, 2, "move={move_distance}, start={start_x}");
        (
            host.drain_physics_commands(),
            world.local_transform(player).unwrap().position,
            expected_impulse,
        )
    }

    let (commands, position, expected_impulse) = run(0.03, 0.2, 0.06);
    let pushes = commands
        .iter()
        .filter(|command| {
            matches!(
                command,
                PhysicsCommand::AddImpulse {
                    body: BodyHandle(2),
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(pushes.len(), 1);
    let impulse = match pushes[0] {
        PhysicsCommand::AddImpulse { impulse, .. } => *impulse,
        _ => unreachable!(),
    };
    let expected_impulse = expected_impulse.unwrap();
    assert!(
        (impulse - expected_impulse).length() < 1.0e-4,
        "impulse={impulse:?}, controller formula={expected_impulse:?}"
    );
    assert!((impulse.x - 4.0).abs() < 1.0e-4, "{impulse:?}");
    assert!(position.x > 0.2 && position.x < 0.25, "{position:?}");

    let (commands, _, expected_impulse) = run(0.03, 0.2, 0.4);
    let pushes = commands
        .iter()
        .filter(|command| {
            matches!(
                command,
                PhysicsCommand::AddImpulse {
                    body: BodyHandle(2),
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(pushes.len(), 1);
    let impulse = match pushes[0] {
        PhysicsCommand::AddImpulse { impulse, .. } => *impulse,
        _ => unreachable!(),
    };
    assert!((impulse - expected_impulse.unwrap()).length() < 1.0e-4);
    assert!(
        (impulse.length() - MAX_PUSH_SPEED).abs() < 1.0e-4,
        "{impulse:?}"
    );

    for dt in [0.0, 0.5e-6] {
        let (commands, _, expected_impulse) = run(dt, 0.2, 0.06);
        assert!(commands.is_empty(), "dt={dt}");
        assert!(expected_impulse.is_none(), "dt={dt}");
    }
}

#[test]
fn character_moves_in_start_message_and_collision_callbacks_do_not_push() {
    let mut world = world(
        r#"
[[assets]]
path = "/player.lua"
[[assets]]
path = "/sender.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/player.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "crate"
[[objects]]
id = "other"
"#,
    );
    let player = world.handle("player").unwrap();
    let crate_handle = world.handle("crate").unwrap();
    let other = world.handle("other").unwrap();
    let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let player_body = physics
        .insert_body({
            let mut desc = BodyDesc::new(
                BodyKind::Kinematic,
                Pose::IDENTITY,
                ColliderDesc::new(controller.capsule().unwrap()),
            );
            desc.character_controller = true;
            desc
        })
        .unwrap();
    let crate_body = physics
        .insert_body(BodyDesc::dynamic(
            Shape::cuboid(Vec3::splat(0.25)).unwrap(),
            Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            1.0,
        ))
        .unwrap();
    let other_body = physics
        .insert_body(BodyDesc::static_collider(
            Shape::sphere(0.25).unwrap(),
            Pose::new(Vec3::new(4.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        ))
        .unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: player,
            body: player_body,
            kind: BodyKind::Kinematic,
            character_controller: Some(controller),
        },
        PhysicsSceneBinding {
            object: crate_handle,
            body: crate_body,
            kind: BodyKind::Dynamic,
            character_controller: None,
        },
        PhysicsSceneBinding {
            object: other,
            body: other_body,
            kind: BodyKind::Static,
            character_controller: None,
        },
    ];
    let player_script = r#"
local Script = {}
function Script:on_start() rve.find("player"):move({x=1,y=0,z=0}) end
function Script:on_message(_, name)
  if name == "move" then rve.find("player"):move({x=1,y=0,z=0}) end
end
function Script:on_collision_enter()
  rve.find("player"):move({x=1,y=0,z=0})
end
return Script
"#;
    let sender_script = r#"
local Script = {}
function Script:on_update()
  if self.sent then return end
  rve.send("player", "move", true)
  self.sent = true
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("player.lua", player_script), ("sender.lua", sender_script)]),
        None,
    )
    .unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);

    host.start(&mut world, ScriptTime::default());
    assert!(
        host.drain_physics_commands().is_empty(),
        "start emitted a push"
    );

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_physics_commands().is_empty());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(
        host.drain_physics_commands().is_empty(),
        "message emitted a push"
    );

    host.queue_physics_events([engine_physics::PhysicsEvent::CollisionEnter {
        first: player_body,
        second: other_body,
        point: Vec3::X,
        normal: Vec3::X,
    }]);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(
        host.drain_physics_commands().is_empty(),
        "collision emitted a push"
    );
}

#[test]
fn character_displacement_accumulates_independently_of_render_frame_rate() {
    fn run(fps: u32) -> f32 {
        let mut world = world(
            r#"
[[assets]]
path = "/move.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/move.lua"
"#,
        );
        let player = world.handle("player").unwrap();
        let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
        let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let body = physics
            .insert_body({
                let mut desc = BodyDesc::new(
                    BodyKind::Kinematic,
                    Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap(),
                    ColliderDesc::new(controller.capsule().unwrap()),
                );
                desc.character_controller = true;
                desc
            })
            .unwrap();
        let bindings = [PhysicsSceneBinding {
            object: player,
            body,
            kind: BodyKind::Kinematic,
            character_controller: Some(controller),
        }];
        let source = r#"
local Script = {}
function Script:on_update(dt)
  rve.find("player"):move({x=2*dt,y=0,z=0})
end
return Script
"#;
        let mut host =
            ScriptHost::load(&world, "scene", &sources(&[("move.lua", source)]), None).unwrap();
        let delta = 1.0 / fps as f32;
        for frame in 0..fps {
            host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
            host.update(
                &mut world,
                delta,
                ScriptTime {
                    elapsed: (frame + 1) as f64 * delta as f64,
                    frame: u64::from(frame + 1),
                },
            );
        }
        world.local_transform(player).unwrap().position.x
    }

    let distances = [run(30), run(60), run(144)];
    for distance in distances {
        assert!((distance - 2.0).abs() < 1.0e-4, "distance={distance}");
    }
}

#[test]
fn character_push_impulse_produces_similar_dynamic_displacement_at_30_60_144_fps() {
    const FIXED_HZ: u32 = 60;

    fn run(fps: u32) -> (f32, Vec3, usize, usize) {
        let mut world = world(
            r#"
[[assets]]
path = "/move.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/move.lua"
[[objects]]
id = "crate"
"#,
        );
        let player = world.handle("player").unwrap();
        let crate_handle = world.handle("crate").unwrap();
        let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
        let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let player_body = physics
            .insert_body({
                let mut desc = BodyDesc::new(
                    BodyKind::Kinematic,
                    Pose::new(Vec3::ZERO, Quat::IDENTITY).unwrap(),
                    ColliderDesc::new(controller.capsule().unwrap()),
                );
                desc.character_controller = true;
                desc
            })
            .unwrap();
        let crate_body = physics
            .insert_body(BodyDesc::dynamic(
                Shape::cuboid(Vec3::splat(0.25)).unwrap(),
                Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let bindings = [
            PhysicsSceneBinding {
                object: player,
                body: player_body,
                kind: BodyKind::Kinematic,
                character_controller: Some(controller),
            },
            PhysicsSceneBinding {
                object: crate_handle,
                body: crate_body,
                kind: BodyKind::Dynamic,
                character_controller: None,
            },
        ];
        let mut driver = StepDriver::new(physics, FIXED_HZ, 8).unwrap();
        let source = r#"
local Script = {}
function Script:on_update(dt)
  if self.did_push then return end
  local player = rve.find("player")
  player:move({x=1,y=0,z=0})
  -- Disable contact after this one command to isolate its impulse response at each FPS.
  player:set_collider_enabled(false)
  self.did_push = true
end
return Script
"#;
        let mut host =
            ScriptHost::load(&world, "scene", &sources(&[("move.lua", source)]), None).unwrap();
        let delta = 1.0 / fps as f32;
        let mut push_count = 0;
        let mut substep_count = 0;
        for frame in 0..fps {
            host.set_physics_snapshot(driver.world().query_snapshot().unwrap(), &bindings);
            host.update(
                &mut world,
                delta,
                ScriptTime {
                    elapsed: (frame + 1) as f64 * delta as f64,
                    frame: u64::from(frame + 1),
                },
            );
            let mut commands = host.drain_physics_commands();
            push_count += commands
                .iter()
                .filter(|command| matches!(command, PhysicsCommand::AddImpulse { body, .. } if *body == crate_body))
                .count();
            commands.push(PhysicsCommand::KinematicTarget {
                body: player_body,
                pose: Pose::new(
                    world.local_transform(player).unwrap().position,
                    Quat::IDENTITY,
                )
                .unwrap(),
            });
            substep_count += driver.advance_step(delta, commands).unwrap().substeps;
        }
        (
            driver.world().body_pose(crate_body).unwrap().position.x - 0.75,
            driver.world().body_velocity(crate_body).unwrap(),
            push_count,
            substep_count,
        )
    }

    let push_results = [(30, run(30)), (60, run(60)), (144, run(144))];
    println!("FPS push results (displacement, velocity, push count, substeps): {push_results:?}");
    assert!(
        push_results
            .iter()
            .all(|(_, (displacement, velocity, push_count, substeps))| {
                *displacement > 1.0
                    && (velocity.x - MAX_PUSH_SPEED).abs() < 1.0e-4
                    && *push_count == 1
                    && *substeps >= 59
            }),
        "each FPS run must move the dynamic body and emit pushes: {push_results:?}"
    );
    let min = push_results
        .iter()
        .map(|(_, (displacement, _, _, _))| *displacement)
        .fold(f32::INFINITY, f32::min);
    let max = push_results
        .iter()
        .map(|(_, (displacement, _, _, _))| *displacement)
        .fold(f32::NEG_INFINITY, f32::max);
    let one_fixed_step_at_max_push_speed = MAX_PUSH_SPEED / FIXED_HZ as f32 + 1.0e-4;
    assert!(
        max - min <= one_fixed_step_at_max_push_speed,
        "crate displacement spread={} exceeds one 60 Hz fixed step at the capped push speed ({one_fixed_step_at_max_push_speed}): {push_results:?}",
        max - min,
    );
}

#[test]
fn sustained_character_push_stays_close_at_30_60_144_fps() {
    const FIXED_HZ: u32 = 60;
    const FIXED_STEPS: usize = FIXED_HZ as usize;

    fn run(fps: u32) -> (f32, f32, usize, usize) {
        let mut world = world(
            r#"
[[assets]]
path = "/move.lua"
[[objects]]
id = "player"
[[objects.lite_scripts]]
path = "/move.lua"
[[objects]]
id = "crate"
"#,
        );
        let player = world.handle("player").unwrap();
        let crate_handle = world.handle("crate").unwrap();
        let controller = CharacterController::new(0.25, 0.5, 0.0, 45.0, 0.0, 0.01, 1.0).unwrap();
        let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let player_body = physics
            .insert_body({
                let mut desc = BodyDesc::new(
                    BodyKind::Kinematic,
                    Pose::IDENTITY,
                    ColliderDesc::new(controller.capsule().unwrap()),
                );
                desc.character_controller = true;
                desc
            })
            .unwrap();
        let crate_body = physics
            .insert_body(BodyDesc::dynamic(
                Shape::cuboid(Vec3::splat(0.25)).unwrap(),
                Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
                1.0,
            ))
            .unwrap();
        let bindings = [
            PhysicsSceneBinding {
                object: player,
                body: player_body,
                kind: BodyKind::Kinematic,
                character_controller: Some(controller),
            },
            PhysicsSceneBinding {
                object: crate_handle,
                body: crate_body,
                kind: BodyKind::Dynamic,
                character_controller: None,
            },
        ];
        let source = r#"
local Script = {}
function Script:on_update(dt)
  rve.find("player"):move({x=dt,y=0,z=0})
end
return Script
"#;
        let mut host =
            ScriptHost::load(&world, "scene", &sources(&[("move.lua", source)]), None).unwrap();
        let mut driver = StepDriver::new(physics, FIXED_HZ, 8).unwrap();

        let nominal_dt = 1.0 / fps as f32;
        let target_duration = f64::from(1.0 / FIXED_HZ as f32) * FIXED_STEPS as f64;
        let prefix_duration = f64::from(nominal_dt) * (fps - 1) as f64;
        let mut last_dt = (target_duration - prefix_duration) as f32;
        while prefix_duration + f64::from(last_dt) < target_duration {
            last_dt = f32::from_bits(last_dt.to_bits() + 1);
        }
        let mut push_count = 0;
        let mut total_substeps = 0;
        let mut elapsed = 0.0_f64;
        for frame in 0..fps {
            let dt = if frame + 1 == fps {
                last_dt
            } else {
                nominal_dt
            };
            let snapshot = driver.world().query_snapshot().unwrap();
            host.set_physics_snapshot(snapshot, &bindings);
            elapsed += f64::from(dt);
            host.update(
                &mut world,
                dt,
                ScriptTime {
                    elapsed,
                    frame: u64::from(frame + 1),
                },
            );
            let mut commands = host.drain_physics_commands();
            push_count += commands
                .iter()
                .filter_map(|command| match command {
                    PhysicsCommand::AddImpulse { body, impulse, .. } if *body == crate_body => {
                        Some(*impulse)
                    }
                    _ => None,
                })
                .filter(|impulse| impulse.length_squared() > 1.0e-8)
                .count();
            commands.push(PhysicsCommand::KinematicTarget {
                body: player_body,
                pose: Pose::new(
                    world.local_transform(player).unwrap().position,
                    Quat::IDENTITY,
                )
                .unwrap(),
            });
            total_substeps += driver.advance_step(dt, commands).unwrap().substeps;
        }
        (
            driver.world().body_pose(crate_body).unwrap().position.x - 0.75,
            driver.world().body_velocity(crate_body).unwrap().x,
            push_count,
            total_substeps,
        )
    }

    let results = [(30, run(30)), (60, run(60)), (144, run(144))];
    println!(
        "FPS sustained push results (displacement, velocity, nonzero pushes, substeps): {results:?}"
    );
    assert!(
        results
            .iter()
            .all(|(_, (displacement, _, pushes, substeps))| {
                *displacement > 0.25 && *pushes > 0 && *substeps == FIXED_STEPS
            }),
        "each run must move the box, emit nonzero pushes, and advance exactly 60 steps: {results:?}"
    );
    let min = results
        .iter()
        .map(|(_, (displacement, _, _, _))| *displacement)
        .fold(f32::INFINITY, f32::min);
    let max = results
        .iter()
        .map(|(_, (displacement, _, _, _))| *displacement)
        .fold(f32::NEG_INFINITY, f32::max);
    let one_fixed_tick_at_input_speed = 1.0 / FIXED_HZ as f32 + 1.0e-4;
    assert!(
        max - min <= one_fixed_tick_at_input_speed,
        "sustained box displacement spread={} exceeds one 60 Hz tick at 1 m/s ({one_fixed_tick_at_input_speed}): {results:?}",
        max - min,
    );
}
#[test]
fn failed_physics_event_disables_instance_and_discards_its_commands() {
    let mut world = world(
        r#"
[[assets]]
path = "/event.lua"
[[assets]]
path = "/reader.lua"
[[objects]]
id = "sensor"
[[objects.lite_scripts]]
path = "/event.lua"
[[objects.lite_scripts]]
path = "/reader.lua"
[[objects]]
id = "other"
"#,
    );
    let sensor = world.handle("sensor").unwrap();
    let other = world.handle("other").unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let sensor_body = physics
        .insert_body({
            let mut desc = BodyDesc::static_collider(Shape::sphere(0.5).unwrap(), Pose::IDENTITY);
            desc.collider.trigger = true;
            desc
        })
        .unwrap();
    let other_body = physics
        .insert_body(BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::X * 2.0, Quat::IDENTITY).unwrap(),
        ))
        .unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: sensor,
            body: sensor_body,
            kind: BodyKind::Static,
            character_controller: None,
        },
        PhysicsSceneBinding {
            object: other,
            body: other_body,
            kind: BodyKind::Static,
            character_controller: None,
        },
    ];
    let event = r#"
local Script = {}
function Script:on_trigger_enter(other)
  rve.find("sensor"):set_collider_enabled(false)
  error("abort trigger transaction")
end
function Script:on_update()
  rve.log_info("failed event instance still active")
end
return Script
"#;
    let reader = r#"
local Script = {}
function Script:on_update()
  assert(rve.find("sensor"):is_collider_enabled())
  rve.log_info("event command rollback observed")
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("event.lua", event), ("reader.lua", reader)]),
        None,
    )
    .unwrap();
    host.set_physics_snapshot(physics.query_snapshot().unwrap(), &bindings);
    host.queue_physics_events([engine_physics::PhysicsEvent::TriggerEnter {
        first: sensor_body,
        second: other_body,
    }]);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert!(host.drain_physics_commands().is_empty());
    let messages = host
        .drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("abort trigger transaction"))
    );
    assert!(
        messages
            .iter()
            .any(|message| message == "event command rollback observed")
    );
    assert!(
        !messages
            .iter()
            .any(|message| message == "failed event instance still active")
    );
}

#[test]
fn lua_collision_and_trigger_events_follow_messages_and_keep_contact_orientation() {
    let mut world = world(
        r#"
[[assets]]
path = "/events.lua"
[[objects]]
id = "first"
[[objects.lite_scripts]]
path = "/events.lua"
[[objects]]
id = "second"
"#,
    );
    let first = world.handle("first").unwrap();
    let second = world.handle("second").unwrap();
    let physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let snapshot = physics.query_snapshot().unwrap();
    let bindings = [
        PhysicsSceneBinding {
            object: first,
            body: BodyHandle(11),
            kind: BodyKind::Static,
            character_controller: None,
        },
        PhysicsSceneBinding {
            object: second,
            body: BodyHandle(12),
            kind: BodyKind::Static,
            character_controller: None,
        },
    ];
    let source = r#"
local Script = {}
function Script:on_start() rve.send("first", "before_events", true) end
function Script:on_message(_, name) rve.log_info(name) end
function Script:on_collision_enter(other, contact)
  assert(other.id == "second")
  assert(contact.point.x == 1 and contact.normal.x == 1)
  rve.log_info("collision_enter")
end
function Script:on_collision_exit(other) rve.log_info("collision_exit:" .. other.id) end
function Script:on_trigger_enter(other) rve.log_info("trigger_enter:" .. other.id) end
function Script:on_trigger_exit(other) rve.log_info("trigger_exit:" .. other.id) end
function Script:on_update() rve.log_info("update") end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("events.lua", source)]), None).unwrap();
    host.set_physics_snapshot(snapshot, &bindings);
    host.queue_physics_events([
        engine_physics::PhysicsEvent::CollisionEnter {
            first: BodyHandle(11),
            second: BodyHandle(12),
            point: Vec3::X,
            normal: Vec3::X,
        },
        engine_physics::PhysicsEvent::CollisionExit {
            first: BodyHandle(11),
            second: BodyHandle(12),
        },
        engine_physics::PhysicsEvent::TriggerEnter {
            first: BodyHandle(11),
            second: BodyHandle(12),
        },
        engine_physics::PhysicsEvent::TriggerExit {
            first: BodyHandle(11),
            second: BodyHandle(12),
        },
    ]);
    host.update(&mut world, 0.1, ScriptTime::default());

    let messages = host
        .drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            "before_events",
            "collision_enter",
            "collision_exit:second",
            "trigger_enter:second",
            "trigger_exit:second",
            "update",
        ]
    );
}

#[test]
fn lua_input_api_reads_the_shared_frame_snapshot() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  local x, y = rve.input.cursor()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if rve.input.key_down("W") and rve.input.key_pressed("W")
      and rve.input.key_released("A")
      and rve.input.mouse_down("Left") and rve.input.mouse_pressed("Left")
      and rve.input.mouse_released("Right")
      and x == 40 and y == 60 and dx == 2.5 and dy == -3
      and wx == 1 and wy == 0.5 then
    rve.log_info("Lua input snapshot")
  else
    error("input snapshot mismatch")
  end
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    let mut input = ScriptInput::default();
    input.set_key(Key::W, true);
    input.set_key(Key::A, true);
    input.set_key(Key::A, false);
    input.set_mouse_button(MouseButton::Left, true);
    input.set_mouse_button(MouseButton::Right, true);
    input.set_mouse_button(MouseButton::Right, false);
    input.set_cursor(40.0, 60.0);
    input.add_mouse_delta(2.5, -3.0);
    input.add_wheel(1.0, 0.5);
    host.set_input(input);
    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "Lua input snapshot"
    }));
}

#[test]
fn lua_destroy_clears_input_edges_and_deltas_but_keeps_down_and_cursor() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  local x, y = rve.input.cursor()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if rve.input.key_down("W") and rve.input.key_pressed("W")
      and rve.input.key_released("A")
      and rve.input.mouse_down("Left") and rve.input.mouse_pressed("Left")
      and rve.input.mouse_released("Right")
      and x == 40 and y == 60 and dx == 2.5 and dy == -3
      and wx == 1 and wy == 0.5 then
    rve.log_info("input update visible")
  else
    error("input update mismatch")
  end
end
function Input:on_destroy()
  local x, y = rve.input.cursor()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if not rve.input.key_down("W") or rve.input.key_pressed("W")
      or rve.input.key_released("W") or rve.input.key_down("A")
      or rve.input.key_pressed("A") or rve.input.key_released("A")
      or not rve.input.mouse_down("Left") or rve.input.mouse_pressed("Left")
      or rve.input.mouse_released("Left") or rve.input.mouse_down("Right")
      or rve.input.mouse_pressed("Right") or rve.input.mouse_released("Right")
      or x ~= 40 or y ~= 60 or dx ~= 0 or dy ~= 0 or wx ~= 0 or wy ~= 0 then
    error("input destroy mismatch")
  end
  rve.log_info("destroy input cleared")
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    let mut input = ScriptInput::default();
    input.set_key(Key::W, true);
    input.set_key(Key::A, true);
    input.set_key(Key::A, false);
    input.set_mouse_button(MouseButton::Left, true);
    input.set_mouse_button(MouseButton::Right, true);
    input.set_mouse_button(MouseButton::Right, false);
    input.set_cursor(40.0, 60.0);
    input.add_mouse_delta(2.5, -3.0);
    input.add_wheel(1.0, 0.5);
    host.set_input(input);

    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "input update visible"
    }));

    host.destroy(&mut world, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "destroy input cleared"
    }));
}

#[test]
fn lua_input_rejects_unknown_key_names() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  rve.input.key_down("UnknownKey")
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("unknown input key")
    }));
}

#[test]
fn lua_input_rejects_unknown_mouse_button_names() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  rve.input.mouse_down("Extra")
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("unknown mouse button")
    }));
}

struct LabeledHeavy {
    label: String,
}

fn labeled_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    let Some(Value::String(label)) = params.get("label") else {
        return Err("missing string parameter label".to_owned());
    };
    Ok(Box::new(LabeledHeavy {
        label: label.clone(),
    }))
}

impl Script for LabeledHeavy {
    fn on_start(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info(format!("{}:start", self.label));
    }

    fn on_update(&mut self, ctx: &mut Context<'_>, _dt: f32) {
        ctx.log_info(format!("{}:update", self.label));
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info(format!("{}:destroy", self.label));
    }
}

struct ParamsAndTransformHeavy;

fn params_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    let expected = Params::from([
        ("flag".to_owned(), Value::Bool(true)),
        ("count".to_owned(), Value::Int(17)),
        ("ratio".to_owned(), Value::Float(1.25)),
        ("title".to_owned(), Value::String("hello".to_owned())),
        (
            "values".to_owned(),
            Value::Array(vec![
                Value::Int(2),
                Value::Bool(false),
                Value::String("x".to_owned()),
            ]),
        ),
    ]);
    if params == &expected {
        Ok(Box::new(ParamsAndTransformHeavy))
    } else {
        Err("parameter values differ".to_owned())
    }
}

impl Script for ParamsAndTransformHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _dt: f32) {
        ctx.transform.position[0] += 1.0;
    }
}

struct PanicHeavy;

fn panic_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(PanicHeavy))
}

impl Script for PanicHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _: f32) {
        ctx.transform.position[0] = 99.0;
        panic!("expected heavy panic");
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info("panic instance destroyed");
    }
}

struct KeepAliveHeavy;

fn keep_alive_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(KeepAliveHeavy))
}

impl Script for KeepAliveHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _: f32) {
        ctx.transform.position[1] += 1.0;
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info("healthy heavy destroyed");
    }
}

struct NonFiniteHeavy;

fn non_finite_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(NonFiniteHeavy))
}

impl Script for NonFiniteHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _: f32) {
        ctx.transform.position[0] = f32::NAN;
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info("non-finite instance destroyed");
    }
}

fn fail_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Err("factory refused parameters".to_owned())
}

fn test_module() -> &'static rve_sdk::abi::RveModule {
    rve_sdk::script_module! {
        "Log" => labeled_factory,
        "Params" => params_factory,
        "Panic" => panic_factory,
        "KeepAlive" => keep_alive_factory,
        "NonFinite" => non_finite_factory,
        "Fail" => fail_factory,
    }
}

fn test_library() -> ScriptLibrary {
    // SAFETY: the SDK macro returns an immutable process-lifetime table.
    unsafe { ScriptLibrary::from_module(std::ptr::from_ref(test_module())) }
        .expect("test script module is valid")
}

fn event_messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect()
}

#[test]
fn lifecycle_order_is_stable_and_start_runs_once() {
    let mut world = world(
        r#"
[[assets]]
path = "/a.lua"
[[assets]]
path = "/b.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects.lite_scripts]]
path = "/a.lua"
params = { label = "camera-lite" }
[[objects.heavy_scripts]]
name = "Log"
params = { label = "camera-heavy" }
[[objects]]
id = "other"
[[objects.lite_scripts]]
path = "/b.lua"
params = { label = "other-lite" }
[[objects.heavy_scripts]]
name = "Log"
params = { label = "other-heavy" }
"#,
    );
    let lite_source = r#"
local Script = {}
function Script:on_start() rve.log(self.params.label .. ":start") end
function Script:on_update() rve.log(self.params.label .. ":update") end
function Script:on_destroy() rve.log(self.params.label .. ":destroy") end
return Script
"#;
    let library = test_library();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", lite_source), ("b.lua", lite_source)]),
        Some(&library),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.start(&mut world, ScriptTime::default());
    assert_eq!(
        event_messages(&mut host),
        [
            "camera-lite:start",
            "camera-heavy:start",
            "other-lite:start",
            "other-heavy:start"
        ]
    );

    host.update(
        &mut world,
        0.25,
        ScriptTime {
            elapsed: 0.25,
            frame: 1,
        },
    );
    host.update(
        &mut world,
        0.5,
        ScriptTime {
            elapsed: 0.75,
            frame: 2,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        [
            "camera-lite:update",
            "camera-heavy:update",
            "other-lite:update",
            "other-heavy:update",
            "camera-lite:update",
            "camera-heavy:update",
            "other-lite:update",
            "other-heavy:update"
        ]
    );

    host.destroy(&mut world, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());
    assert_eq!(
        event_messages(&mut host),
        [
            "camera-lite:destroy",
            "camera-heavy:destroy",
            "other-lite:destroy",
            "other-heavy:destroy"
        ]
    );
}

#[test]
fn first_update_calls_start_before_update_when_start_was_not_explicitly_requested() {
    let source = r#"
local Script = {}
function Script:on_start() rve.log("start") end
function Script:on_update() rve.log("update") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/auto-start.lua\"\n[[objects.lite_scripts]]\npath=\"/auto-start.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("auto-start.lua", source)]),
        None,
    )
    .unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert_eq!(event_messages(&mut host), ["start", "update"]);
}

#[test]
fn lua_and_heavy_receive_all_param_types_and_write_transform_to_world() {
    let params =
        r#"{ flag = true, count = 17, ratio = 1.25, title = "hello", values = [2, false, "x"] }"#;
    let source = r#"
local Script = {}
function Script:on_start()
  local p = self.params
  assert(p.flag == true and p.count == 17 and p.ratio == 1.25 and p.title == "hello")
  assert(p.values[1] == 2 and p.values[2] == false and p.values[3] == "x")
end
function Script:on_update()
  self.transform.position = { x = 10, y = 20, z = 30 }
  self.transform.rotation = { x = 0, y = 90, z = 0 }
  self.transform.scale = { x = 2, y = 3, z = 4 }
end
return Script
"#;
    let mut world = one_camera(&format!(
        "[[assets]]\npath = \"/params.lua\"\n[[objects.lite_scripts]]\npath = \"/params.lua\"\nparams = {params}\n[[objects.heavy_scripts]]\nname = \"Params\"\nparams = {params}\n"
    ));
    let library = test_library();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("params.lua", source)]),
        Some(&library),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());

    let transform = world
        .local_transform(world.handle("camera").unwrap())
        .unwrap();
    assert_eq!(transform.position.to_array(), [11.0, 20.0, 30.0]);
    assert_eq!(transform.rotation_deg.to_array(), [0.0, 90.0, 0.0]);
    assert_eq!(transform.scale.to_array(), [2.0, 3.0, 4.0]);
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_time_print_and_rve_log_are_buffered_as_info_events() {
    let source = r#"
local Script = {}
function Script:on_start()
  assert(rve.time.elapsed == 12.5 and rve.time.frame == 7)
  print("hello", 123)
  rve.log("elapsed", rve.time.elapsed, "frame", rve.time.frame)
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/log.lua\"\n[[objects.lite_scripts]]\npath = \"/log.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "level", &sources(&[("log.lua", source)]), None).unwrap();
    host.start(
        &mut world,
        ScriptTime {
            elapsed: 12.5,
            frame: 7,
        },
    );
    let events = host.drain_events();
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
    assert_eq!(events[0].message, "hello 123");
    assert_eq!(events[1].message, "elapsed 12.5 frame 7");
    assert_eq!(events[0].source, "level/camera/log.lua");
}

#[test]
fn lua_logging_lossily_converts_non_utf8_strings() {
    let source = r#"
local Script = {}
function Script:on_start()
  print("print-\xff")
  rve.log("log-\xff")
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/bytes.lua\"\n[[objects.lite_scripts]]\npath = \"/bytes.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("bytes.lua", source)]), None).unwrap();

    host.start(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].message, "print-�");
    assert_eq!(events[1].message, "log-�");
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
}

#[test]
fn lua_files_have_separate_global_environments() {
    let mut world = one_camera(
        "[[assets]]\npath = \"/a.lua\"\n[[assets]]\npath = \"/b.lua\"\n[[objects.lite_scripts]]\npath = \"/a.lua\"\n[[objects.lite_scripts]]\npath = \"/b.lua\"\n",
    );
    let a = r#"
rve = nil
tostring = nil
foo = 1
return {}
"#;
    let b = r#"
local Script = {}
function Script:on_start()
  assert(rve.time and type(tostring) == "function")
  assert(foo == nil)
  rve.log("second file is isolated")
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", a), ("b.lua", b)]),
        None,
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, "second file is isolated");
    assert_eq!(events[0].level, ScriptEventLevel::Info);
}

#[test]
fn lua_time_above_signed_integer_range_is_passed_as_float() {
    let source = r#"
local Script = {}
function Script:on_start()
  assert(math.type(rve.time.frame) == "float")
  rve.log("large frame")
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/frame.lua\"\n[[objects.lite_scripts]]\npath = \"/frame.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("frame.lua", source)]), None).unwrap();

    host.start(
        &mut world,
        ScriptTime {
            elapsed: 0.0,
            frame: i64::MAX as u64 + 1,
        },
    );

    assert_eq!(event_messages(&mut host), ["large frame"]);
}

#[test]
fn lua_runtime_error_disables_only_its_instance_and_skips_destroy() {
    let bad = r#"
local Script = {}
function Script:on_update() error("broken callback") end
function Script:on_destroy() rve.log("bad destroyed") end
return Script
"#;
    let healthy = r#"
local Script = {}
function Script:on_update()
  local p = self.transform.position
  self.transform.position = { x = p.x + 1, y = p.y, z = p.z }
  rve.log("healthy update")
end
function Script:on_destroy() rve.log("healthy destroyed") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/bad.lua\"\n[[assets]]\npath=\"/healthy.lua\"\n[[objects.lite_scripts]]\npath=\"/bad.lua\"\n[[objects.lite_scripts]]\npath=\"/healthy.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("bad.lua", bad), ("healthy.lua", healthy)]),
        None,
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1
    );
    assert!(events[0].source.ends_with("camera/bad.lua"));
    assert!(events[0].message.contains("broken callback"));
    let messages = events
        .iter()
        .map(|event| event.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            events[0].message.as_str(),
            "healthy update",
            "healthy update",
            "healthy destroyed"
        ]
    );
    assert!(!messages.contains(&"bad destroyed"));
    let transform = world
        .local_transform(world.handle("camera").unwrap())
        .unwrap();
    assert_eq!(transform.position.x, 2.0);
}

#[test]
fn heavy_panic_disables_that_instance_but_other_heavy_scripts_continue() {
    let mut world = one_camera(
        "[objects.transform]\nposition=[8.0, 0.0, 0.0]\n[[objects.heavy_scripts]]\nname=\"Panic\"\n[[objects.heavy_scripts]]\nname=\"KeepAlive\"\n",
    );
    let library = test_library();
    let mut host = ScriptHost::load(&world, "scene", &BTreeMap::new(), Some(&library)).unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1
    );
    assert!(events[0].message.contains("expected heavy panic"));
    assert_eq!(events[1].message, "healthy heavy destroyed");
    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .to_array(),
        [8.0, 2.0, 0.0]
    );
}

#[test]
fn non_finite_heavy_transform_disables_instance_and_rolls_back() {
    let mut world = one_camera(
        "[[objects.heavy_scripts]]\nname=\"NonFinite\"\n[objects.transform]\nposition=[8.0, 2.0, 3.0]\n",
    );
    let library = test_library();
    let mut host = ScriptHost::load(&world, "scene", &BTreeMap::new(), Some(&library)).unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let transform = world
        .local_transform(world.handle("camera").unwrap())
        .unwrap();
    assert_eq!(transform.position.to_array(), [8.0, 2.0, 3.0]);
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.contains("non-finite transform"));
}

#[test]
fn invalid_lua_transform_value_is_a_runtime_error_and_rolls_back() {
    for (assignment, expected) in [
        ("x = math.huge", "finite f32"),
        (r#"x = "not a number""#, "finite number"),
    ] {
        let source = format!(
            r#"
local Script = {{}}
function Script:on_update()
  self.transform.position = {{ {assignment}, y = 0, z = 0 }}
end
return Script
"#
        );
        let mut world = one_camera(
            "[[assets]]\npath=\"/bad-transform.lua\"\n[objects.transform]\nposition=[4.0, 0.0, 0.0]\n[[objects.lite_scripts]]\npath=\"/bad-transform.lua\"\n",
        );
        let mut host = ScriptHost::load(
            &world,
            "scene",
            &sources(&[("bad-transform.lua", &source)]),
            None,
        )
        .unwrap();
        host.start(&mut world, ScriptTime::default());
        host.update(&mut world, 0.1, ScriptTime::default());

        assert_eq!(
            world
                .local_transform(world.handle("camera").unwrap())
                .unwrap()
                .position
                .x,
            4.0
        );
        let events = host.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, ScriptEventLevel::Error);
        assert!(events[0].message.contains(expected));
    }
}

#[test]
fn lua_transform_assignment_is_rolled_back_when_callback_later_errors() {
    let source = r#"
local Script = {}
function Script:on_update()
  self.transform.position = { x = 99, y = 0, z = 0 }
  error("failure after transform assignment")
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/rollback.lua\"\n[objects.transform]\nposition=[4.0, 0.0, 0.0]\n[[objects.lite_scripts]]\npath=\"/rollback.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("rollback.lua", source)]), None).unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());

    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .to_array(),
        [4.0, 0.0, 0.0]
    );
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(
        events[0]
            .message
            .contains("failure after transform assignment")
    );
}

#[test]
fn load_rejects_syntax_non_table_and_non_function_callbacks_with_path() {
    for (source, expected) in [
        ("return {", "syntax"),
        ("return 42", "class table"),
        ("return { on_update = true }", "on_update"),
    ] {
        let world = one_camera(
            "[[assets]]\npath=\"/bad.lua\"\n[[objects.lite_scripts]]\npath=\"/bad.lua\"\n",
        );
        let error = ScriptHost::load(&world, "scene", &sources(&[("bad.lua", source)]), None)
            .err()
            .expect("invalid lite script should fail to load");
        assert!(matches!(
            error,
            ScriptLoadError::Lite { ref path, ref message }
                if path == "bad.lua" && message.to_lowercase().contains(expected)
        ));
    }
}

#[test]
fn load_rejects_heavy_scripts_without_a_library_or_registration() {
    let unknown_world = one_camera("[[objects.heavy_scripts]]\nname=\"Missing\"\n");
    let error = ScriptHost::load(&unknown_world, "scene", &BTreeMap::new(), None)
        .err()
        .expect("unknown heavy script should fail to load");
    assert_eq!(
        error,
        ScriptLoadError::Heavy {
            object_id: "camera".to_owned(),
            name: "Missing".to_owned(),
            message:
                "no script library is loaded; pass --scripts <path> or place the library in Engine/"
                    .to_owned(),
        }
    );

    let library = test_library();
    let unknown_name = ScriptHost::load(&unknown_world, "scene", &BTreeMap::new(), Some(&library))
        .err()
        .expect("unregistered heavy script should fail to load");
    assert!(matches!(
        unknown_name,
        ScriptLoadError::Heavy { ref message, .. }
            if message.contains("does not register this name")
    ));

    let failed_world = one_camera("[[objects.heavy_scripts]]\nname=\"Fail\"\n");
    assert!(matches!(
        ScriptHost::load(&failed_world, "scene", &BTreeMap::new(), Some(&library)),
        Err(ScriptLoadError::Heavy { ref message, .. })
            if message == "factory refused parameters"
    ));
}

#[test]
fn lua_sandbox_has_only_requested_libraries_and_load_accepts_text_only() {
    let source = r#"
local Script = {}
function Script:on_start()
  assert(io == nil and os == nil and package == nil and debug == nil)
  assert(dofile == nil and loadfile == nil and require == nil)
  assert(type(collectgarbage) == "function")
  collectgarbage()
  assert(type(collectgarbage("count")) == "number")
  collectgarbage("step", 1)
  collectgarbage("collect")
  local text, text_error = load("return 40 + 2", "=dynamic", "t")
  assert(text ~= nil and text_error == nil and text() == 42)
  local binary, binary_error = load(string.dump(function() return 7 end))
  assert(binary == nil and binary_error ~= nil)
  local mode, mode_error = load("return 1", "=binary", "b")
  assert(mode == nil and mode_error ~= nil)
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/sandbox.lua\"\n[[objects.lite_scripts]]\npath=\"/sandbox.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("sandbox.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_collectgarbage_rejects_other_modes_as_a_runtime_error() {
    let source = r#"
local Script = {}
function Script:on_start() collectgarbage("stop") end
return Script
"#;
    let mut world =
        one_camera("[[assets]]\npath=\"/gc.lua\"\n[[objects.lite_scripts]]\npath=\"/gc.lua\"\n");
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("gc.lua", source)]), None).unwrap();

    host.start(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.contains("collectgarbage mode"));
    assert!(events[0].message.contains("not allowed"));
}

#[test]
fn lua_memory_limit_disables_instance_after_large_allocations() {
    let source = r#"
local Script = {}
function Script:on_update()
  local retained = {}
  while true do
    retained[#retained + 1] = string.rep("x", 1024 * 1024)
  end
end
function Script:on_destroy() rve.log("out of memory instance destroyed") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/memory.lua\"\n[[objects.lite_scripts]]\npath=\"/memory.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("memory.lua", source)]), None).unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.to_lowercase().contains("memory"));
    assert!(
        !events[0]
            .message
            .contains("out of memory instance destroyed")
    );
}

#[test]
fn scripts_cannot_break_host_time_updates_with_table_metamethods() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/a.lua"
[[assets]]
path = "/b.lua"
[[objects.lite_scripts]]
path = "/a.lua"
[[objects.lite_scripts]]
path = "/b.lua"
"#,
    );
    let a = r#"
local Script = {}
function Script:on_start()
  rve.time.elapsed = nil
  rve.time.frame = nil
  setmetatable(rve.time, { __newindex = function() error("hijack") end })
  rve.log("a:start")
end
function Script:on_update() rve.log("a:update") end
return Script
"#;
    let b = r#"
local Script = {}
function Script:on_update()
  rve.log("b:update", rve.time.elapsed, rve.time.frame)
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", a), ("b.lua", b)]),
        None,
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.25,
            frame: 42,
        },
    );

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.message.as_str())
            .collect::<Vec<_>>(),
        ["a:start", "a:update", "b:update 0.25 42"]
    );
}

#[test]
fn interrupt_with_only_heavy_scripts_does_not_report_lua_interruption() {
    let mut world = one_camera(
        r#"
[[objects.heavy_scripts]]
name = "Log"
params = { label = "camera-heavy" }
"#,
    );
    let handle = ScriptInterruptHandle::new();
    let library = test_library();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[]),
        Some(&library),
        handle.clone(),
    )
    .unwrap();
    handle.interrupt();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.message.as_str())
            .collect::<Vec<_>>(),
        ["camera-heavy:start", "camera-heavy:update"]
    );
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
}

#[test]
fn interrupt_handle_stops_infinite_lua_loop_and_disables_instance() {
    let (signal_tx, signal_rx) = mpsc::channel();
    let (handle_tx, handle_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut world = one_camera(
            "[[assets]]\npath=\"/loop.lua\"\n[[objects.lite_scripts]]\npath=\"/loop.lua\"\n",
        );
        let source = r#"
local Script = {}
function Script:on_update()
  signal()
  while true do
    pcall(function()
      while true do end
    end)
  end
end
function Script:on_destroy() rve.log("loop destroyed") end
return Script
"#;
        let mut host =
            ScriptHost::load(&world, "scene", &sources(&[("loop.lua", source)]), None).unwrap();
        let signal = host
            .lua
            .create_function(move |_, ()| {
                signal_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        host.lua.globals().set("signal", signal).unwrap();
        handle_tx.send(host.interrupt_handle()).unwrap();
        host.update(&mut world, 0.1, ScriptTime::default());
        host.destroy(&mut world, ScriptTime::default());
        result_tx.send(host.drain_events()).unwrap();
    });

    let handle = handle_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    signal_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    handle.interrupt();
    let events = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(handle.is_interrupted());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.contains("interrupted"));
}

#[test]
fn interrupt_set_before_start_disables_lite_once_and_skips_lite_destroy() {
    let source = r#"
local Script = {}
function Script:on_start() rve.log("lite started") end
function Script:on_destroy() rve.log("lite destroyed") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/pre-interrupted.lua\"\n[[objects.lite_scripts]]\npath=\"/pre-interrupted.lua\"\n[[objects.heavy_scripts]]\nname=\"Log\"\nparams={label=\"camera-heavy\"}\n",
    );
    let handle = ScriptInterruptHandle::new();
    let library = test_library();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[("pre-interrupted.lua", source)]),
        Some(&library),
        handle.clone(),
    )
    .unwrap();
    handle.interrupt();

    host.start(&mut world, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "scripts interrupted")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "camera-heavy:start")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "camera-heavy:destroy")
    );
    assert!(!events.iter().any(|event| event.message == "lite started"));
    assert!(!events.iter().any(|event| event.message == "lite destroyed"));
}

#[test]
fn interrupt_handle_cancels_infinite_top_level_chunk_during_load() {
    let handle = ScriptInterruptHandle::new();
    let worker_handle = handle.clone();
    let (loading_tx, loading_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let world = one_camera(
            "[[assets]]\npath=\"/load-loop.lua\"\n[[objects.lite_scripts]]\npath=\"/load-loop.lua\"\n",
        );
        let source = "while true do end";
        loading_tx.send(()).unwrap();
        let result = ScriptHost::load_with_interrupt(
            &world,
            "scene",
            &sources(&[("load-loop.lua", source)]),
            None,
            worker_handle,
        )
        .map(|_| ());
        result_tx.send(result).unwrap();
    });

    loading_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    handle.interrupt();
    let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(matches!(
        result,
        Err(ScriptLoadError::Lite { ref path, ref message })
            if path == "load-loop.lua" && message.to_lowercase().contains("interrupted")
    ));
}

#[test]
fn lua_call_deadline_disables_only_the_overrunning_script() {
    let mut world = world(
        r#"
[[assets]]
path = "/loop.lua"
[[assets]]
path = "/neighbor.lua"
[[objects]]
id = "loop"
[[objects.lite_scripts]]
path = "/loop.lua"
[[objects]]
id = "neighbor"
[[objects.lite_scripts]]
path = "/neighbor.lua"
"#,
    );
    let loop_source = r#"
local Loop = {}
function Loop:on_update()
  while true do end
end
return Loop
"#;
    let neighbor_source = r#"
local Neighbor = {}
function Neighbor:on_update()
  rve.log_info("neighbor progressed")
end
return Neighbor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("loop.lua", loop_source), ("neighbor.lua", neighbor_source)]),
        None,
        ScriptLoadOptions {
            interrupt: ScriptInterruptHandle::new(),
            call_time_limit: Some(Duration::from_millis(50)),
            input: Default::default(),
            prefabs: Default::default(),
        },
    )
    .unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());
    let first_events = host.drain_events();
    assert!(first_events.iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event
                .message
                .contains("Lua call exceeded time limit of 50ms")
    }));
    assert!(first_events.iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "neighbor progressed"
    }));

    host.update(&mut world, 0.1, ScriptTime::default());
    let next_events = host.drain_events();
    assert_eq!(
        next_events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        0
    );
    assert_eq!(
        next_events
            .iter()
            .filter(|event| event.message == "neighbor progressed")
            .count(),
        1
    );
}

#[test]
fn lua_message_callback_lookup_obeys_the_call_deadline() {
    let mut world = world(
        r#"
[[assets]]
path = "/lookup.lua"
[[assets]]
path = "/neighbor.lua"
[[objects]]
id = "lookup"
[[objects.lite_scripts]]
path = "/lookup.lua"
[[objects]]
id = "neighbor"
[[objects.lite_scripts]]
path = "/neighbor.lua"
"#,
    );
    let lookup_source = r#"
local Script = {}
function Script:on_start()
  setmetatable(Script, { __index = function(_, key)
    if key == "on_message" then
      for _ = 1, 100000000 do end
      rve.log_info("lookup escaped")
    end
  end })
  rve.broadcast("probe", true)
end
return Script
"#;
    let neighbor_source = r#"
local Neighbor = {}
function Neighbor:on_update() rve.log_info("neighbor progressed") end
return Neighbor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[
            ("lookup.lua", lookup_source),
            ("neighbor.lua", neighbor_source),
        ]),
        None,
        ScriptLoadOptions {
            call_time_limit: Some(Duration::from_millis(50)),
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error
                && event.message.contains("Lua call exceeded time limit"))
    );
    assert!(!events.iter().any(|event| event.message == "lookup escaped"));
    assert!(!host.instances[0].as_ref().expect("instance").enabled);
    assert!(host.instances[1].as_ref().expect("instance").enabled);
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message == "neighbor progressed")
    );
}

#[test]
fn lua_console_deadline_disables_owner_and_removes_its_commands() {
    let mut world = world(
        r#"
[[assets]]
path = "/command.lua"
[[assets]]
path = "/neighbor.lua"
[[objects]]
id = "command"
[[objects.lite_scripts]]
path = "/command.lua"
[[objects]]
id = "neighbor"
[[objects.lite_scripts]]
path = "/neighbor.lua"
"#,
    );
    let command_source = r#"
local Script = {}
function Script:on_start()
  rve.console.register("loop", function() while true do end end)
  rve.console.register("other", function() end)
end
return Script
"#;
    let neighbor_source = r#"
local Neighbor = {}
function Neighbor:on_update() rve.log_info("neighbor progressed") end
return Neighbor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[
            ("command.lua", command_source),
            ("neighbor.lua", neighbor_source),
        ]),
        None,
        ScriptLoadOptions {
            call_time_limit: Some(Duration::from_millis(50)),
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    let command = CommandLine {
        name: "loop".into(),
        args: vec![],
    };
    assert!(matches!(host.execute_command(&mut world, &command),
        Err(crate::CommandError::Failed(message)) if message.contains("Lua call exceeded time limit")));
    assert!(host.command_catalog().get("loop").is_none());
    assert!(host.command_catalog().get("other").is_none());
    assert!(!host.instances[0].as_ref().expect("instance").enabled);
    assert!(host.instances[1].as_ref().expect("instance").enabled);
    assert!(matches!(
        host.execute_command(&mut world, &command),
        Err(crate::CommandError::Unknown(_))
    ));
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message == "neighbor progressed")
    );
}

#[test]
fn lua_call_deadline_cannot_be_caught_by_pcall() {
    let mut world = one_camera(
        "[[assets]]\npath=\"/pcall-loop.lua\"\n[[objects.lite_scripts]]\npath=\"/pcall-loop.lua\"\n",
    );
    let source = r#"
local Script = {}
function Script:on_update()
  pcall(function() while true do end end)
  rve.log_info("pcall escaped")
end
return Script
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("pcall-loop.lua", source)]),
        None,
        ScriptLoadOptions {
            interrupt: ScriptInterruptHandle::new(),
            call_time_limit: Some(Duration::from_millis(50)),
            input: Default::default(),
            prefabs: Default::default(),
        },
    )
    .unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event
                .message
                .contains("Lua call exceeded time limit of 50ms")
    }));
    assert!(!events.iter().any(|event| event.message == "pcall escaped"));
}

#[test]
fn lua_call_deadline_applies_to_top_level_chunks_and_is_optional() {
    let mut world = one_camera(
        "[[assets]]\npath=\"/load-loop.lua\"\n[[objects.lite_scripts]]\npath=\"/load-loop.lua\"\n",
    );
    let result = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("load-loop.lua", "while true do end")]),
        None,
        ScriptLoadOptions {
            interrupt: ScriptInterruptHandle::new(),
            call_time_limit: Some(Duration::from_millis(50)),
            input: Default::default(),
            prefabs: Default::default(),
        },
    );
    assert!(matches!(
        result,
        Err(ScriptLoadError::Lite { ref path, ref message })
            if path == "load-loop.lua"
                && message.contains("Lua call exceeded time limit of 50ms")
    ));

    let source = r#"
local Script = {}
function Script:on_update()
  local total = 0
  for index = 1, 100 do total = total + index end
  rve.log_info("no limit", total)
end
return Script
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("load-loop.lua", source)]),
        None,
        ScriptLoadOptions {
            interrupt: ScriptInterruptHandle::new(),
            call_time_limit: None,
            input: Default::default(),
            prefabs: Default::default(),
        },
    )
    .unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message == "no limit 5050")
    );
}

#[test]
fn lua_print_queue_caps_at_1024_and_reports_drops_once_per_drain() {
    let mut world = one_camera(
        "[[assets]]\npath=\"/logs.lua\"\n[[objects.lite_scripts]]\npath=\"/logs.lua\"\n",
    );
    let source = r#"
local Script = {}
function Script:on_update()
  for index = 1, 2000 do print("log", index) end
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("logs.lua", source)]), None).unwrap();

    for _ in 0..2 {
        host.update(&mut world, 0.1, ScriptTime::default());
        let events = host.drain_events();
        assert_eq!(events.len(), 1_025);
        assert!(
            events[..1_024]
                .iter()
                .all(|event| event.level == ScriptEventLevel::Info)
        );
        assert_eq!(events[1_024].level, ScriptEventLevel::Warn);
        assert_eq!(events[1_024].message, "976 script log messages dropped");
    }
}

#[test]
fn host_failures_are_kept_when_the_log_queue_is_full() {
    let mut world = one_camera(
        "[[assets]]\npath=\"/failure.lua\"\n[[objects.lite_scripts]]\npath=\"/failure.lua\"\n",
    );
    let source = r#"
local Script = {}
function Script:on_update()
  for index = 1, 2000 do print("log", index) end
  error("host failure after logs")
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("failure.lua", source)]), None).unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    assert_eq!(events.len(), 1_026);
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("host failure after logs")
    }));
    assert_eq!(
        events.last().unwrap().message,
        "976 script log messages dropped"
    );
}

#[test]
fn oversized_lua_logs_are_truncated_at_a_utf8_boundary() {
    let mut world = one_camera(
        "[[assets]]\npath=\"/long-log.lua\"\n[[objects.lite_scripts]]\npath=\"/long-log.lua\"\n",
    );
    let source = r#"
local Script = {}
function Script:on_update()
  print(string.rep("é", 3000))
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("long-log.lua", source)]), None).unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert!(events[0].message.len() <= 4_096);
    assert!(events[0].message.ends_with('…'));
    assert!(events[0].message.is_char_boundary(events[0].message.len()));
}

#[test]
fn lua_object_writes_are_transactional_and_world_matrices_are_phase_snapshots() {
    let mut world = world(
        r#"
[[assets]]
path = "/objects.lua"
[[objects]]
id = "sender"
name = "Sender"
[objects.transform]
position = [1.0, 0.0, 0.0]
[[objects.lite_scripts]]
path = "/objects.lua"
params = { role = "sender" }
[[objects]]
id = "target"
name = "Target"
[objects.transform]
position = [2.0, 0.0, 0.0]
[[objects.lite_scripts]]
path = "/objects.lua"
params = { role = "target" }
[[objects]]
id = "external"
name = "External"
[objects.transform]
position = [4.0, 0.0, 0.0]
"#,
    );
    let source = r#"
local Script = {}
function Script:on_update()
  local own = assert(rve.find(self.object.id))
  local matrix = own:world_matrix()
  if self.params.role == "sender" then
    if rve.time.frame == 1 then
      assert(own.id == "sender" and own.name == "Sender")
      local external = assert(rve.find("external"))
      local transform = external:get_transform()
      transform.position = { x = 5, y = 0, z = 0 }
      external:set_transform(transform)
      assert(external:world_matrix()[13] == 4)

      local through_handle = own:get_transform()
      through_handle.position = { x = 99, y = 0, z = 0 }
      own:set_transform(through_handle)
      self.transform.position = { x = 1, y = 3, z = 0 }
      assert(matrix[13] == 1)
    else
      assert(matrix[13] == 1)
    end
  elseif rve.time.frame == 1 then
    assert(matrix[13] == 2)
    local through_handle = own:get_transform()
    through_handle.position = { x = 88, y = 0, z = 0 }
    own:set_transform(through_handle)
  else
    assert(matrix[13] == 88)
  end
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("objects.lua", source)]), None).unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let sender = world
        .local_transform(world.handle("sender").unwrap())
        .unwrap();
    let target = world
        .local_transform(world.handle("target").unwrap())
        .unwrap();
    let external = world
        .local_transform(world.handle("external").unwrap())
        .unwrap();
    assert_eq!(sender.position.to_array(), [1.0, 3.0, 0.0]);
    assert_eq!(target.position.to_array(), [88.0, 0.0, 0.0]);
    assert_eq!(external.position.to_array(), [5.0, 0.0, 0.0]);
    assert!(host.drain_events().is_empty());

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_messages_from_start_and_on_message_are_delivered_in_order_on_later_updates() {
    let mut world = world(
        r#"
[[assets]]
path = "/messages.lua"
[[objects]]
id = "sender"
name = "Sender"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
name = "Receiver"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "receiver" }
[[objects]]
id = "observer"
name = "Observer"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "observer" }
"#,
    );
    let source = r#"
local Script = {}
function Script:on_start()
  if self.params.role == "sender" then
    rve.send("receiver", "one", { "hello", 2, false })
    rve.broadcast("fan", "ready")
  end
end
function Script:on_message(sender, name, value)
  if name == "one" then
    assert(value[1] == "hello" and value[2] == 2 and value[3] == false)
    rve.send("sender", "reply", { "ok" })
  end
  rve.log_info(self.params.role .. ":" .. name .. ":" .. sender)
end
function Script:on_update()
  rve.log_info(self.params.role .. ":update")
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("messages.lua", source)]), None).unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        [
            "receiver:one:sender",
            "sender:fan:sender",
            "receiver:fan:sender",
            "observer:fan:sender",
            "sender:update",
            "receiver:update",
            "observer:update",
        ]
    );

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        [
            "sender:reply:receiver",
            "sender:update",
            "receiver:update",
            "observer:update",
        ]
    );
}

#[test]
fn lua_message_and_update_callbacks_share_one_world_matrix_snapshot() {
    let mut world = world(
        r#"
[[assets]]
path = "/sender.lua"
[[assets]]
path = "/target.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "target"
[objects.transform]
position = [2.0, 0.0, 0.0]
[[objects.lite_scripts]]
path = "/target.lua"
"#,
    );
    let sender = r#"
local Script = {}
function Script:on_start() rve.send("target", "move", true) end
return Script
"#;
    let target = r#"
local Script = {}
function Script:on_message(_, name)
  if name == "move" then
    local object = rve.find("target")
    local transform = object:get_transform()
    transform.position = { x = 10, y = 0, z = 0 }
    object:set_transform(transform)
    assert(object:world_matrix()[13] == 2)
    rve.log_info("message matrix 2")
  end
end
function Script:on_update()
  local matrix = rve.find("target"):world_matrix()
  if rve.time.frame == 1 then
    assert(matrix[13] == 2)
    rve.log_info("update matrix 2")
  else
    assert(matrix[13] == 10)
    rve.log_info("update matrix 10")
  end
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sender.lua", sender), ("target.lua", target)]),
        None,
    )
    .unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        ["message matrix 2", "update matrix 2"]
    );
    assert_eq!(
        world
            .local_transform(world.handle("target").unwrap())
            .unwrap()
            .position
            .x,
        10.0
    );
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert_eq!(event_messages(&mut host), ["update matrix 10"]);
}

#[test]
fn lua_failed_send_rolls_back_earlier_messages_and_unknown_targets_are_errors() {
    let mut world = world(
        r#"
[[assets]]
path = "/send.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/send.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/send.lua"
params = { role = "receiver" }
"#,
    );
    let source = r#"
local Script = {}
function Script:on_start()
  if self.params.role == "sender" then
    rve.send("receiver", "must_rollback", 1)
    rve.send("missing", "unknown", 2)
  end
end
function Script:on_message(_, name)
  rve.log_info("received:" .. name)
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("send.lua", source)]), None).unwrap();
    let sender = world.handle("sender").unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );

    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event.message.contains("target \"missing\" does not exist")
    }));
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains("received:"))
    );
    assert_eq!(
        world.local_transform(sender).unwrap().position.to_array(),
        [0.0; 3]
    );
}

#[test]
fn lua_message_error_rolls_back_its_transaction_and_disables_the_receiver() {
    let mut world = world(
        r#"
[[assets]]
path = "/messages.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "receiver" }
[[objects]]
id = "observer"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "observer" }
"#,
    );
    let source = r#"
local Script = {}
function Script:on_start()
  if self.params.role == "sender" then
    rve.send("receiver", "fail", true)
    rve.send("receiver", "later", true)
  end
end
function Script:on_message(_, name)
  if self.params.role == "receiver" and name == "fail" then
    local transform = rve.find("receiver"):get_transform()
    transform.position = { x = 9, y = 0, z = 0 }
    rve.find("receiver"):set_transform(transform)
    rve.send("observer", "must_rollback", true)
    rve.send("missing", "error", true)
  elseif self.params.role == "observer" then
    rve.log_info("observer received " .. name)
  end
end
function Script:on_update()
  if self.params.role == "receiver" then rve.log_info("receiver update") end
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("messages.lua", source)]), None).unwrap();
    let receiver = world.handle("receiver").unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );

    assert_eq!(
        world.local_transform(receiver).unwrap().position.to_array(),
        [0.0; 3]
    );
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event.source == "scene/receiver/messages.lua"
            && event.message.contains("target \"missing\" does not exist")
    }));
    assert!(events.iter().all(|event| {
        !event.message.contains("receiver update") && !event.message.contains("observer received")
    }));

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_message_limit_drops_excess_without_disabling_sender() {
    let mut world = world(
        r#"
[[assets]]
path = "/sender.lua"
[[assets]]
path = "/receiver.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/receiver.lua"
params = { count = 0 }
"#,
    );
    let sender = r#"
local Script = {}
function Script:on_start()
  for i = 1, 4100 do rve.send("receiver", "bulk", i) end
end
function Script:on_update() rve.log_info("sender remains enabled") end
return Script
"#;
    let receiver = r#"
local Script = {}
function Script:on_message(_, name)
  if name == "bulk" then self.params.count = self.params.count + 1 end
end
function Script:on_update() rve.log_info("received " .. self.params.count) end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sender.lua", sender), ("receiver.lua", receiver)]),
        None,
    )
    .unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("dropped 4 message(s)")
    }));
    assert!(
        events
            .iter()
            .any(|event| event.message == "sender remains enabled")
    );
    assert!(events.iter().any(|event| event.message == "received 4096"));
}

#[test]
fn lua_message_byte_budget_drops_excess_in_order_without_disabling_sender() {
    const MESSAGE_COUNT: usize = 132;
    const MESSAGE_VALUE_BYTES: usize = 65_536;
    let max_bytes = crate::transaction::MAX_MESSAGE_BYTES_PER_FRAME;
    let mut expected_bytes = 0usize;
    let expected_delivery_count = (1..=MESSAGE_COUNT)
        .take_while(|index| {
            let name = format!("bulk{index}");
            let message_bytes = std::mem::size_of::<engine_scene::ParamValue>()
                + "sender".len()
                + name.len()
                + MESSAGE_VALUE_BYTES;
            if expected_bytes.saturating_add(message_bytes) > max_bytes {
                return false;
            }
            expected_bytes += message_bytes;
            true
        })
        .count();
    let mut world = world(
        r#"
[[assets]]
path = "/sender.lua"
[[assets]]
path = "/receiver.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/receiver.lua"
"#,
    );
    let sender = format!(
        r#"
local Script = {{}}
function Script:on_start()
  for i = 1, {MESSAGE_COUNT} do
    rve.send("receiver", "bulk" .. i, string.rep("x", {MESSAGE_VALUE_BYTES}))
  end
end
function Script:on_update() rve.log_info("sender remains enabled") end
return Script
"#
    );
    let receiver = r#"
local Script = {}
function Script:on_message(_, name) rve.log_info("received " .. name) end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sender.lua", &sender), ("receiver.lua", receiver)]),
        None,
    )
    .unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let events = host.drain_events();
    let delivered = events
        .iter()
        .filter_map(|event| {
            event
                .message
                .strip_prefix("received bulk")
                .and_then(|value| value.parse::<usize>().ok())
        })
        .collect::<Vec<_>>();
    assert_eq!(delivered, (1..=expected_delivery_count).collect::<Vec<_>>());
    let overflow_errors = events
        .iter()
        .filter(|event| {
            event.level == ScriptEventLevel::Error && event.message.contains("message queue limits")
        })
        .collect::<Vec<_>>();
    assert_eq!(overflow_errors.len(), 1);
    assert!(overflow_errors[0].message.contains(&format!(
        "dropped {} message(s)",
        MESSAGE_COUNT - expected_delivery_count
    )));
    assert_eq!(host.transaction.peak_pending_bytes(), expected_bytes);
    assert!(
        events
            .iter()
            .any(|event| event.message == "sender remains enabled")
    );
}

#[test]
fn lua_creation_phases_reject_object_writes_and_messages() {
    for (path, source) in [
        (
            "write.lua",
            r#"local object = rve.find("camera"); local transform = object:get_transform(); object:set_transform(transform); return {}"#,
        ),
        (
            "send.lua",
            r#"rve.send("camera", "early", true); return {}"#,
        ),
        (
            "broadcast.lua",
            r#"rve.broadcast("early", true); return {}"#,
        ),
    ] {
        let world = one_camera(&format!(
            "[[assets]]\npath = \"/{path}\"\n[[objects.lite_scripts]]\npath = \"/{path}\"\n"
        ));
        let handle = world.handle("camera").unwrap();
        let before = *world.local_transform(handle).unwrap();
        let result = ScriptHost::load(&world, "scene", &sources(&[(path, source)]), None);
        assert!(
            matches!(result, Err(ScriptLoadError::Lite { message, .. }) if message.contains("unavailable during script creation"))
        );
        assert_eq!(*world.local_transform(handle).unwrap(), before);
    }
}

#[test]
fn lua_camera_api_reads_pending_settings_and_switches_active_camera_atomically() {
    let mut world = two_cameras_with_lite_script();
    let source = r#"
local Script = {}
function Script:on_update()
  assert(rve.find("plain"):get_camera_settings() == nil)
  local primary = rve.active_camera()
  assert(primary and primary.id == "camera")
  local before = primary:get_camera_settings()
  assert(before.projection.fov_deg == 60)
  assert(before.render.anti_aliasing == "fxaa")
  assert(before.color.tonemapper == "aces")

  primary:set_camera_settings({
    projection = { fov_deg = 72 },
    render = { clear_color = { 0.2, 0.3, 0.4 }, anti_aliasing = "none" },
    color = { exposure_ev = 1.5, tonemapper = "neutral" },
  })
  local pending = primary:get_camera_settings()
  assert(pending.projection.fov_deg == 72 and math.abs(pending.projection.near - 0.1) < 0.0001)
  assert(math.abs(pending.render.clear_color[2] - 0.3) < 0.0001 and pending.render.render_scale == 1)
  assert(pending.color.exposure_ev == 1.5 and pending.color.tonemapper == "neutral")

  local secondary = rve.find("second")
  secondary:activate_camera()
  assert(rve.active_camera().id == "second")
  secondary:set_camera_settings({ color = { exposure_ev = -2 } })
  local secondary_settings = secondary:get_camera_settings()
  assert(secondary_settings.color.exposure_ev == -2)
  assert(secondary_settings.projection.fov_deg == 60)
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("camera.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());

    let events = host.drain_events();
    assert!(events.is_empty(), "unexpected script events: {events:?}");
    let primary = world.handle("camera").unwrap();
    let secondary = world.handle("second").unwrap();
    assert_eq!(world.active_camera(), secondary);
    let primary_settings = world.camera_settings(primary).unwrap();
    assert_eq!(primary_settings.projection.fov_deg, 72.0);
    assert_eq!(primary_settings.render.clear_color, [0.2, 0.3, 0.4]);
    assert_eq!(primary_settings.color.unwrap_or_default().exposure_ev, 1.5);
    let secondary_settings = world.camera_settings(secondary).unwrap();
    assert_eq!(
        secondary_settings.color.unwrap_or_default().exposure_ev,
        -2.0
    );
}

#[test]
fn lua_camera_settings_type_enum_and_non_camera_errors_disable_the_instance() {
    for (label, body, expected) in [
        (
            "unknown",
            r#"rve.find("camera"):set_camera_settings({ color = { boost = 1 } })"#,
            "unknown color field",
        ),
        (
            "type",
            r#"rve.find("camera"):set_camera_settings({ projection = { fov_deg = "wide" } })"#,
            "fov_deg must be a number",
        ),
        (
            "enum",
            r#"rve.find("camera"):set_camera_settings({ render = { anti_aliasing = "msaa" } })"#,
            "anti_aliasing must be",
        ),
        (
            "non-camera",
            r#"rve.find("plain"):set_camera_settings({})"#,
            "has no camera",
        ),
        (
            "activate non-camera",
            r#"rve.find("plain"):activate_camera()"#,
            "has no camera",
        ),
    ] {
        let mut world = two_cameras_with_lite_script();
        let source =
            format!("local Script = {{}}\nfunction Script:on_update() {body} end\nreturn Script");
        let mut host = ScriptHost::load(
            &world,
            "scene",
            &sources(&[("camera.lua", source.as_str())]),
            None,
        )
        .unwrap();
        host.update(&mut world, 0.1, ScriptTime::default());
        assert!(
            host.drain_events()
                .iter()
                .any(|event| event.level == ScriptEventLevel::Error
                    && event.message.contains(expected)),
            "{label} input did not produce {expected:?}"
        );
        assert_eq!(
            world
                .camera_settings(world.handle("camera").unwrap())
                .unwrap()
                .projection
                .fov_deg,
            60.0
        );
    }
}

#[test]
fn invalid_lua_camera_settings_roll_back_settings_selection_and_transform() {
    let mut world = two_cameras_with_lite_script();
    let source = r#"
local Script = {}
function Script:on_update()
  local own = rve.find("camera")
  local transform = own:get_transform()
  transform.position = { x = 9, y = 0, z = 0 }
  own:set_transform(transform)
  own:set_camera_settings({ color = { exposure_ev = 17 } })
  rve.find("second"):activate_camera()
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("camera.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());

    let primary = world.handle("camera").unwrap();
    assert_eq!(world.local_transform(primary).unwrap().position.x, 0.0);
    assert_eq!(world.active_camera(), primary);
    assert_eq!(
        world
            .camera_settings(primary)
            .unwrap()
            .color
            .unwrap_or_default()
            .exposure_ev,
        0.0
    );
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("invalid camera settings")
    }));
}

#[test]
fn lua_camera_writes_are_rejected_during_creation() {
    for source in [
        r#"local camera = rve.find("camera"); camera:set_camera_settings({ color = { exposure_ev = 1 } }); return {}"#,
        r#"local camera = rve.find("camera"); camera:activate_camera(); return {}"#,
    ] {
        let world = two_cameras_with_lite_script();
        let handle = world.handle("camera").unwrap();
        let result = ScriptHost::load(&world, "scene", &sources(&[("camera.lua", source)]), None);
        assert!(
            matches!(result, Err(ScriptLoadError::Lite { message, .. }) if message.contains("unavailable during script creation"))
        );
        assert_eq!(world.active_camera(), handle);
        assert_eq!(
            world
                .camera_settings(handle)
                .unwrap()
                .color
                .unwrap_or_default()
                .exposure_ev,
            0.0
        );
    }
}

#[test]
fn lua_warning_logging_keeps_the_warn_level() {
    let source = r#"
local Script = {}
function Script:on_start() rve.log_warn("watch this") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/warn.lua\"\n[[objects.lite_scripts]]\npath = \"/warn.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("warn.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Warn);
    assert_eq!(events[0].message, "watch this");
}

#[test]
fn lua_error_rolls_back_multiple_object_writes_and_messages_but_keeps_logs() {
    let mut world = world(
        r#"
[[assets]]
path = "/rollback.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/rollback.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/rollback.lua"
params = { role = "receiver" }
[[objects]]
id = "external"
"#,
    );
    let source = r#"
local Script = {}
function Script:on_update()
  if self.params.role == "sender" then
    local own = rve.find("sender")
    local own_transform = own:get_transform()
    own_transform.position = { x = 10, y = 0, z = 0 }
    own:set_transform(own_transform)
    local external = rve.find("external")
    local other_transform = external:get_transform()
    other_transform.position = { x = 20, y = 0, z = 0 }
    external:set_transform(other_transform)
    rve.send("receiver", "rolled_back", true)
    rve.log_info("log survives rollback")
    error("transaction failure")
  end
end
function Script:on_message(_, name) rve.log_info("received:" .. name) end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("rollback.lua", source)]), None).unwrap();
    let sender = world.handle("sender").unwrap();
    let external = world.handle("external").unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );

    assert_eq!(
        world.local_transform(sender).unwrap().position.to_array(),
        [0.0; 3]
    );
    assert_eq!(
        world.local_transform(external).unwrap().position.to_array(),
        [0.0; 3]
    );
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.message == "log survives rollback")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message.contains("transaction failure"))
    );
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains("received:"))
    );
}

mod abi;

#[test]
fn lite_schema_defaults_reach_the_script_and_scene_values_are_coerced() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/schema.lua"
[[objects.lite_scripts]]
path = "/schema.lua"
params = { speed = 90 }
"#,
    );
    let source = r#"
local Schema = {}
Schema.properties = {
  speed = { type = "float", default = 30, min = 0, max = 360 },
  label = { type = "string", default = "spinner" },
}
function Schema:on_start()
  rve.log(math.type(self.params.speed) .. ":" .. self.params.speed .. ":" .. self.params.label)
end
return Schema
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("schema.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    let messages = event_messages(&mut host);
    assert!(
        messages
            .iter()
            .any(|message| message == "float:90.0:spinner"),
        "{messages:?}"
    );
}

#[test]
fn lite_params_that_violate_the_schema_fail_loading() {
    let source = r#"
local Schema = { properties = { speed = { type = "float", min = 0 } } }
return Schema
"#;
    for params in [
        "{ speed = \"fast\" }",
        "{ speed = -1.0 }",
        "{ unknown = 1 }",
    ] {
        let world = one_camera(&format!(
            "[[assets]]\npath = \"/schema.lua\"\n[[objects.lite_scripts]]\npath = \"/schema.lua\"\nparams = {params}\n"
        ));
        let error = ScriptHost::load(&world, "scene", &sources(&[("schema.lua", source)]), None)
            .err()
            .unwrap();
        match error {
            ScriptLoadError::Lite { path, message } => {
                assert_eq!(path, "schema.lua");
                assert!(message.contains("\"camera\""), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn lite_properties_reads_both_forms_without_creating_instances() {
    let interrupt = ScriptInterruptHandle::new();
    let named = r#"
return { properties = {
  speed = { type = "float", default = 30, min = 0, max = 360, label = "Speed" },
  axis = { type = "vec3", default = { 0, 1, 0 } },
} }
"#;
    let schema = crate::lite_properties("named.lua", named, &interrupt)
        .unwrap()
        .unwrap();
    let names: Vec<_> = schema
        .specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["axis", "speed"]);
    let speed = schema.get("speed").unwrap();
    assert_eq!(speed.label.as_deref(), Some("Speed"));
    assert_eq!((speed.min, speed.max), (Some(0.0), Some(360.0)));

    let listed = r#"
return { properties = {
  { name = "speed", type = "float" },
  { name = "enabled", type = "bool", default = true },
} }
"#;
    let schema = crate::lite_properties("listed.lua", listed, &interrupt)
        .unwrap()
        .unwrap();
    let names: Vec<_> = schema
        .specs()
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert_eq!(names, ["speed", "enabled"]);

    assert_eq!(
        crate::lite_properties("plain.lua", "return {}", &interrupt).unwrap(),
        None
    );
}

#[test]
fn malformed_properties_are_compile_errors_with_the_script_path() {
    let interrupt = ScriptInterruptHandle::new();
    for properties in [
        "42",
        "{ speed = 1 }",
        "{ speed = { type = \"double\" } }",
        "{ speed = { type = \"float\", default = \"x\" } }",
        "{ speed = { type = \"float\", min = \"0\" } }",
        "{ speed = { type = \"bool\", max = 1 } }",
        "{ speed = { type = \"float\", unit = \"m\" } }",
        "{ speed = { type = \"float\", name = \"speed\" } }",
        "{ { type = \"float\" } }",
        "{ { name = \"a\", type = \"int\" }, b = { type = \"int\" } }",
        "{ [1] = { name = \"a\", type = \"int\" }, [3] = { name = \"b\", type = \"int\" } }",
        "{ { name = \"a\", type = \"int\" }, { name = \"a\", type = \"int\" } }",
    ] {
        let source = format!("return {{ properties = {properties} }}");
        match crate::lite_properties("bad.lua", &source, &interrupt) {
            Err(ScriptLoadError::Lite { path, .. }) => assert_eq!(path, "bad.lua"),
            other => panic!("{properties}: {other:?}"),
        }
    }
}

#[test]
fn lite_properties_honours_the_interrupt() {
    let interrupt = ScriptInterruptHandle::new();
    interrupt.interrupt();
    assert!(crate::lite_properties("loop.lua", "while true do end", &interrupt).is_err());
}

#[test]
fn lua_console_write_routes_levels_and_rejects_unknown_levels() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
local Console = {}
function Console:on_start()
  rve.console.write("info")
  rve.console.write("warning", "warning")
  rve.console.write("error", "error")
  rve.console.write("log alias", "log")
  rve.console.write("warn alias", "warn")
end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    let events = host.drain_events();
    assert_eq!(
        events.iter().map(|event| event.level).collect::<Vec<_>>(),
        [
            ScriptEventLevel::Info,
            ScriptEventLevel::Warn,
            ScriptEventLevel::Error,
            ScriptEventLevel::Info,
            ScriptEventLevel::Warn
        ]
    );
    assert!(
        events
            .iter()
            .all(|event| event.source.contains("console.lua"))
    );

    let mut world = one_camera(
        r#"
[[assets]]
path = "/bad.lua"
[[objects.lite_scripts]]
path = "/bad.lua"
"#,
    );
    let source = r#"
local Console = {}
function Console:on_start() rve.console.write("bad", "trace") end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("bad.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("unknown console level")
    }));
}

#[test]
fn lua_console_commands_receive_string_args_return_info_and_commit_changes() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
local Console = {}
function Console:on_start()
  rve.console.register("move", function(args)
    local object = assert(rve.find("camera"))
    local transform = object:get_transform()
    transform.position = { x = tonumber(args[1]), y = 0, z = 0 }
    object:set_transform(transform)
    return args[2]
  end, "move the camera")
end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    assert_eq!(host.commands_generation(), 1);
    assert_eq!(
        host.command_catalog().get("move").unwrap().description,
        "move the camera"
    );

    host.execute_command(
        &mut world,
        &CommandLine {
            name: "MOVE".into(),
            args: vec!["4.5".into(), "moved".into()],
        },
    )
    .unwrap();
    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .x,
        4.5
    );
    assert!(
        host.drain_events()
            .iter()
            .any(|event| { event.level == ScriptEventLevel::Info && event.message == "moved" })
    );
}

#[test]
fn lua_console_rejects_invalid_reserved_and_duplicate_command_names() {
    for name in ["Bad", "help", "same"] {
        let mut world = one_camera(
            r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
        );
        let registration = if name == "same" {
            "rve.console.register(\"same\", function() end); rve.console.register(\"same\", function() end)".to_owned()
        } else {
            format!("rve.console.register(\"{name}\", function() end)")
        };
        let source = format!(
            "local Console = {{}}; function Console:on_start() {registration} end; return Console"
        );
        let mut host =
            ScriptHost::load(&world, "scene", &sources(&[("console.lua", &source)]), None).unwrap();
        host.start(&mut world, ScriptTime::default());
        assert!(
            host.drain_events()
                .iter()
                .any(|event| event.level == ScriptEventLevel::Error)
        );
        assert!(host.command_catalog().get("same").is_none());
        assert_eq!(
            host.commands_generation(),
            if name == "same" { 2 } else { 0 }
        );
    }
}

#[test]
fn lua_console_cannot_unregister_another_instances_command() {
    let mut world = world(
        r#"
[[assets]]
path = "/owner.lua"
[[assets]]
path = "/foreign.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects.lite_scripts]]
path = "/owner.lua"
[[objects]]
id = "other"
[[objects.lite_scripts]]
path = "/foreign.lua"
"#,
    );
    let owner = r#"
local Script = {}
function Script:on_start() rve.console.register("owned", function() end) end
return Script
"#;
    let foreign = r#"
local Script = {}
function Script:on_start() rve.console.unregister("owned") end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("owner.lua", owner), ("foreign.lua", foreign)]),
        None,
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    assert!(host.command_catalog().get("owned").is_some());
    assert_eq!(host.commands_generation(), 1);
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("another instance")
    }));
}

#[test]
fn lua_console_registration_is_only_available_inside_a_callback() {
    let world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
rve.console.register("outside", function() end)
return {}
"#;
    let error = ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None)
        .err()
        .unwrap();
    assert!(
        matches!(error, ScriptLoadError::Lite { message, .. } if message.contains("only available during script callbacks"))
    );
}

#[test]
fn lua_console_command_failures_roll_back_but_keep_the_instance_enabled() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
local Console = { attempts = 0 }
function Console:on_start()
  rve.console.register("mutate", function(args)
    self.attempts = self.attempts + 1
    local object = assert(rve.find("camera"))
    local transform = object:get_transform()
    transform.position = { x = 9, y = 0, z = 0 }
    object:set_transform(transform)
    if self.attempts == 1 then error("command failed") end
    return args[1]
  end)
end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    let command = CommandLine {
        name: "mutate".into(),
        args: vec!["success".into()],
    };
    assert!(matches!(
        host.execute_command(&mut world, &command),
        Err(crate::CommandError::Failed(message)) if message.contains("command failed")
    ));
    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .x,
        0.0
    );
    assert!(host.command_catalog().get("mutate").is_some());
    host.execute_command(&mut world, &command).unwrap();
    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .x,
        9.0
    );
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message == "success")
    );
}

#[test]
fn lua_console_commands_are_removed_on_unregister_disable_and_destroy() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
local Console = {}
function Console:on_start()
  rve.console.register("temporary", function() end)
end
function Console:on_update() error("disable") end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.command_catalog().get("temporary").is_none());
    assert_eq!(host.commands_generation(), 2);
    assert!(matches!(
        host.execute_command(
            &mut world,
            &CommandLine {
                name: "missing".into(),
                args: vec![]
            }
        ),
        Err(crate::CommandError::Unknown(_))
    ));

    let mut world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
local Console = {}
function Console:on_start() rve.console.register("temporary", function() end) end
function Console:on_update() rve.console.unregister("temporary") end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.command_catalog().get("temporary").is_none());
    assert_eq!(host.commands_generation(), 2);

    let mut world = one_camera(
        r#"
[[assets]]
path = "/console.lua"
[[objects.lite_scripts]]
path = "/console.lua"
"#,
    );
    let source = r#"
local Console = {}
function Console:on_start() rve.console.register("temporary", function() end) end
return Console
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("console.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());
    assert!(host.command_catalog().get("temporary").is_none());
    assert_eq!(host.commands_generation(), 2);
}

/// Two bodies in a scene: `sensor` runs the script and destroys `victim` on its first update.
const EXIT_WINDOW_SCENE: &str = r#"
[[assets]]
path = "/sensor.lua"
[[objects]]
id = "sensor"
[[objects.lite_scripts]]
path = "/sensor.lua"
[[objects]]
id = "victim"
"#;

const EXIT_WINDOW_SENSOR: &str = r#"
local Sensor = {}
local step = 0
function Sensor:on_update(dt)
  step = step + 1
  if step == 1 then rve.destroy("victim") end
  rve.log_info("update " .. step)
end
function Sensor:on_collision_exit(other)
  rve.log_info("exit " .. tostring(other.id) .. " alive=" .. tostring(other:is_alive()))
end
return Sensor
"#;

#[test]
fn exit_with_a_destroyed_other_is_delivered_only_inside_the_retired_window() {
    let mut world = world(EXIT_WINDOW_SCENE);
    let sensor = world.handle("sensor").unwrap();
    let victim = world.handle("victim").unwrap();
    let mut physics = PhysicsWorld::new(Vec3::ZERO).unwrap();
    let sensor_body = physics
        .insert_body(BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::IDENTITY,
        ))
        .unwrap();
    let victim_body = physics
        .insert_body(BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::X * 2.0, Quat::IDENTITY).unwrap(),
        ))
        .unwrap();
    let binding = |object, body| PhysicsSceneBinding {
        object,
        body,
        kind: BodyKind::Static,
        character_controller: None,
    };
    let both = [binding(sensor, sensor_body), binding(victim, victim_body)];
    let sensor_only = [binding(sensor, sensor_body)];
    let snapshot = physics.query_snapshot().unwrap();
    let exit = engine_physics::PhysicsEvent::CollisionExit {
        first: sensor_body,
        second: victim_body,
    };
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sensor.lua", EXIT_WINDOW_SENSOR)]),
        None,
    )
    .unwrap();
    let messages = |host: &mut ScriptHost| {
        host.drain_events()
            .into_iter()
            .map(|event| (event.level, event.message))
            .collect::<Vec<_>>()
    };

    host.set_physics_snapshot(snapshot.clone(), &both);
    host.update(
        &mut world,
        1.0 / 60.0,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert!(world.handle("victim").is_none());
    messages(&mut host);

    // The first step result without the victim keeps its body known for one more result.
    host.set_physics_snapshot(snapshot.clone(), &sensor_only);
    host.queue_physics_events([exit.clone()]);
    host.update(
        &mut world,
        1.0 / 60.0,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    let inside = messages(&mut host);
    assert!(
        inside
            .iter()
            .any(|(level, message)| *level == ScriptEventLevel::Info
                && message == "exit victim alive=false"),
        "{inside:?}"
    );

    // The second result closes the window. A stale binding of the removed body must skip the
    // callback instead of disabling the sensor with a Lua error.
    host.set_physics_snapshot(snapshot, &both);
    host.queue_physics_events([exit]);
    host.update(
        &mut world,
        1.0 / 60.0,
        ScriptTime {
            elapsed: 0.3,
            frame: 3,
        },
    );
    let after = messages(&mut host);
    assert!(
        after
            .iter()
            .all(|(level, _)| *level != ScriptEventLevel::Error),
        "{after:?}"
    );
    assert!(
        after.iter().any(|(_, message)| message == "update 3"),
        "the sensor keeps running after the window: {after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|(_, message)| message.starts_with("exit ")),
        "{after:?}"
    );
}

#[test]
fn update_profile_times_lua_calls_and_stays_zero_before_the_first_update() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/profile.lua"
[[objects.lite_scripts]]
path = "/profile.lua"
"#,
    );
    let source = r#"
local Profile = {}
function Profile:on_update()
  local total = 0
  for i = 1, 1000 do total = total + i end
end
return Profile
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("profile.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    assert_eq!(host.last_update_profile(), ScriptUpdateProfile::default());

    host.update(&mut world, 0.1, ScriptTime::default());

    let profile = host.last_update_profile();
    assert!(
        profile.lua > Duration::ZERO,
        "вызов Lua-скрипта должен быть измерен"
    );
    assert_eq!(
        profile.rust,
        Duration::ZERO,
        "без Rust-библиотеки время Rust-вызовов нулевое"
    );
    let named = profile.world_matrices
        + profile.messages
        + profile.physics_events
        + profile.lua
        + profile.rust;
    assert!(named <= profile.total, "фазы не перекрываются внутри total");
    assert_eq!(profile.other(), profile.total - named);
}

fn cursor_script_world() -> World {
    one_camera(
        r#"
[[assets]]
path = "/cursor.lua"
[[objects.lite_scripts]]
path = "/cursor.lua"
"#,
    )
}

#[test]
fn lua_cursor_changes_are_visible_to_later_reads_and_applied_on_commit() {
    let mut world = cursor_script_world();
    let source = r#"
local Cursor = {}
function Cursor:on_update()
  if rve.input.cursor_visible() ~= true or rve.input.cursor_locked() ~= false then
    error("applied state missing")
  end
  rve.input.set_cursor_visible(false)
  rve.input.set_cursor_locked(true)
  if rve.input.cursor_visible() ~= false or rve.input.cursor_locked() ~= true then
    error("staged state not visible to reads")
  end
  rve.log_info("cursor staged")
end
return Cursor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("cursor.lua", source)]),
        None,
        ScriptLoadOptions::default(),
    )
    .unwrap();
    assert_eq!(host.cursor(), ScriptCursor::default());

    host.update(&mut world, 0.1, ScriptTime::default());

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .all(|event| event.level != ScriptEventLevel::Error),
        "{events:?}"
    );
    assert_eq!(
        host.cursor(),
        ScriptCursor {
            visible: false,
            locked: true,
        }
    );
}

#[test]
fn failed_lua_callback_discards_staged_cursor_changes() {
    let mut world = cursor_script_world();
    let source = r#"
local Cursor = {}
function Cursor:on_update()
  rve.input.set_cursor_visible(false)
  rve.input.set_cursor_locked(true)
  error("callback failed")
end
return Cursor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("cursor.lua", source)]),
        None,
        ScriptLoadOptions::default(),
    )
    .unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("callback failed")
    }));
    assert_eq!(host.cursor(), ScriptCursor::default());
}

#[test]
fn cursor_changes_are_rejected_during_script_creation() {
    let world = cursor_script_world();
    let source = "rve.input.set_cursor_locked(true)\nreturn {}\n";
    let error = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("cursor.lua", source)]),
        None,
        ScriptLoadOptions::default(),
    )
    .err()
    .expect("cursor changes must not run while the script is created");
    assert!(
        error
            .to_string()
            .contains("cursor changes are unavailable during script creation"),
        "{error}"
    );
}

#[test]
fn cursor_arguments_must_be_lua_booleans() {
    let mut world = cursor_script_world();
    let source = r#"
local Cursor = {}
function Cursor:on_update()
  rve.input.set_cursor_visible("yes")
end
return Cursor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("cursor.lua", source)]),
        None,
        ScriptLoadOptions::default(),
    )
    .unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event
                .message
                .contains("rve.input.set_cursor_visible expects a boolean, got string")
    }));
    assert_eq!(host.cursor(), ScriptCursor::default());
}

#[test]
fn set_cursor_provides_the_initial_state_seen_by_scripts() {
    let mut world = cursor_script_world();
    let source = r#"
local Cursor = {}
function Cursor:on_update()
  if rve.input.cursor_visible() ~= false or rve.input.cursor_locked() ~= true then
    error("initial cursor state mismatch")
  end
end
return Cursor
"#;
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("cursor.lua", source)]),
        None,
        ScriptLoadOptions::default(),
    )
    .unwrap();
    host.set_cursor(ScriptCursor {
        visible: false,
        locked: true,
    });

    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(
        host.drain_events()
            .iter()
            .all(|event| event.level != ScriptEventLevel::Error)
    );
    assert_eq!(
        host.cursor(),
        ScriptCursor {
            visible: false,
            locked: true,
        }
    );
}

fn post_process_script_result(body: &str) -> (World, Vec<crate::ScriptEvent>) {
    let mut world = two_cameras_with_lite_script();
    let source =
        format!("local Script = {{}}\nfunction Script:on_update() {body} end\nreturn Script");
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("camera.lua", source.as_str())]),
        None,
    )
    .unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    let events = host.drain_events();
    (world, events)
}

#[test]
fn lua_post_process_reads_defaults_and_applies_partial_nested_tables() {
    let (world, events) = post_process_script_result(
        r#"
  local camera = rve.find("camera")
  local before = camera:get_post_process()
  assert(before.bloom.enabled == false and before.bloom.threshold == 1)
  assert(before.exposure.ev == 0 and before.exposure.mode == "manual")
  assert(before.tonemapping.tonemapper == "aces")
  assert(camera:set_post_process({ bloom = { enabled = true, intensity = 2.0 }, exposure = { ev = 1.5 } }))
  local pending = camera:get_post_process()
  assert(pending.bloom.enabled and pending.bloom.intensity == 2.0 and pending.exposure.ev == 1.5)
  assert(pending.bloom.threshold == 1)
"#,
    );
    assert!(events.is_empty(), "unexpected script events: {events:?}");
    let camera = world.handle("camera").unwrap();
    let post = world.post_process(camera).unwrap();
    assert!(post.bloom.enabled);
    assert_eq!(post.bloom.intensity, 2.0);
    assert_eq!(post.exposure.ev, 1.5);
    assert!(
        !world
            .post_process(world.handle("second").unwrap())
            .unwrap()
            .bloom
            .enabled
    );
}

#[test]
fn lua_post_process_and_legacy_color_proxy_apply_in_call_order() {
    let (world, events) = post_process_script_result(
        r#"
  local camera = rve.find("camera")
  camera:set_post_process({ exposure = { ev = 3 } })
  camera:set_camera_settings({ color = { exposure_ev = 1 } })
"#,
    );
    assert!(events.is_empty(), "unexpected script events: {events:?}");
    let camera = world.handle("camera").unwrap();
    assert_eq!(world.post_process(camera).unwrap().exposure.ev, 1.0);

    let (world, events) = post_process_script_result(
        r#"
  local camera = rve.find("camera")
  camera:set_camera_settings({ color = { exposure_ev = 1 } })
  camera:set_post_process({ exposure = { ev = 3 } })
"#,
    );
    assert!(events.is_empty(), "unexpected script events: {events:?}");
    let camera = world.handle("camera").unwrap();
    assert_eq!(world.post_process(camera).unwrap().exposure.ev, 3.0);
}

#[test]
fn lua_legacy_color_round_trip_does_not_overwrite_post_process_file_values() {
    let mut world = two_cameras_with_lite_script();
    let camera = world.handle("camera").unwrap();
    let mut file = engine_scene::PostProcessSettings::default();
    file.exposure.ev = -1.25;
    file.tonemapping.tonemapper = engine_scene::Tonemapper::Neutral;
    world.set_post_process(camera, file).unwrap();
    let source = r#"
local Script = {}
function Script:on_update()
  local camera = rve.find("camera")
  local settings = camera:get_camera_settings()
  settings.projection.fov_deg = 75
  camera:set_camera_settings(settings)
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("camera.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().is_empty());
    assert_eq!(
        world.camera_settings(camera).unwrap().projection.fov_deg,
        75.0
    );
    assert_eq!(world.post_process(camera).unwrap().exposure.ev, -1.25);
    assert_eq!(
        world.post_process(camera).unwrap().tonemapping.tonemapper,
        engine_scene::Tonemapper::Neutral
    );
}

#[test]
fn lua_post_process_rejects_unknown_fields_wrong_types_and_invalid_ranges() {
    for (label, body, expected) in [
        (
            "unknown section",
            r#"rve.find("camera"):set_post_process({ unknown = {} })"#,
            "unknown post process field",
        ),
        (
            "unknown bloom field",
            r#"rve.find("camera"):set_post_process({ bloom = { glow = 1 } })"#,
            "unknown bloom field",
        ),
        (
            "wrong type",
            r#"rve.find("camera"):set_post_process({ bloom = { enabled = 1 } })"#,
            "enabled must be a boolean",
        ),
        (
            "bad enum",
            r#"rve.find("camera"):set_post_process({ tonemapping = { tonemapper = "filmic" } })"#,
            "tonemapper must be",
        ),
        (
            "range",
            r#"rve.find("camera"):set_post_process({ bloom = { intensity = -1 } })"#,
            "bloom.intensity",
        ),
        (
            "non-camera",
            r#"rve.find("plain"):set_post_process({})"#,
            "has no camera",
        ),
    ] {
        let (world, events) = post_process_script_result(body);
        assert!(
            events
                .iter()
                .any(|event| event.level == ScriptEventLevel::Error
                    && event.message.contains(expected)),
            "{label} did not report {expected:?}: {events:?}"
        );
        let camera = world.handle("camera").unwrap();
        assert_eq!(
            world.post_process(camera),
            Some(&engine_scene::PostProcessSettings::default()),
            "{label}"
        );
    }
}
