//! Интеграционные тесты ABI v17 для скриптов физики. Цикл повторяет рантайм: обновление скриптов
//! по снимку прошлого шага, команды скрипта и синхронизация трансформов, шаг физики, затем
//! снимок и события. Окно и GPU не нужны; время шага фиксировано.

use super::{event_messages, sources, world};
use crate::ScriptTime;
use crate::host::ScriptHost;
use engine_physics::{
    PhysicsSceneDesc, PhysicsSceneSync, Pose, QueryFilter, QueryOverrides, Shape, StepDriver,
};
use engine_scene::{ObjectEdit, ObjectHandle, World};
use glam::{Quat, Vec3};

const DT: f32 = 1.0 / 60.0;

/// Физический цикл одной сцены с настоящими компонентами из `World`.
struct Loop {
    world: World,
    host: ScriptHost,
    driver: StepDriver,
    sync: PhysicsSceneSync,
    frame: u64,
}

impl Loop {
    fn new(scene: &str, scripts: &[(&str, &str)]) -> Self {
        let world = world(scene);
        let desc =
            PhysicsSceneDesc::from_world(&world, &[], Vec3::new(0.0, -9.81, 0.0), 60, 8).unwrap();
        let (driver, bindings) = desc.build().unwrap();
        let sync = PhysicsSceneSync::new(&world, bindings).unwrap();
        let mut host = ScriptHost::load(&world, "scene", &sources(scripts), None).unwrap();
        host.set_physics_snapshot(driver.world().query_snapshot().unwrap(), sync.bindings());
        Self {
            world,
            host,
            driver,
            sync,
            frame: 0,
        }
    }

    /// Один кадр: скрипты, затем физика. Возвращает сообщения, которые записали скрипты кадра.
    fn step(&mut self) -> Vec<String> {
        self.frame += 1;
        let time = ScriptTime {
            elapsed: f64::from(self.frame as f32 * DT),
            frame: self.frame,
        };
        self.host.update(&mut self.world, DT, time);
        let messages = event_messages(&mut self.host);
        let mut commands = self.host.drain_physics_commands();
        commands.extend(self.sync.sync_world_commands(&mut self.world).unwrap());
        let result = self.driver.advance_step(DT, commands).unwrap();
        self.sync
            .apply_physics_poses(&mut self.world, &result.poses)
            .unwrap();
        self.host.set_physics_snapshot(
            self.driver.world().query_snapshot().unwrap(),
            self.sync.bindings(),
        );
        self.host.queue_physics_events(result.events);
        messages
    }

    fn handle(&self, id: &str) -> ObjectHandle {
        self.world.handle(id).unwrap()
    }

    fn move_to(&mut self, id: &str, position: Vec3) {
        let object = self.handle(id);
        self.world
            .apply_edit(&ObjectEdit::SetWorldPosition { object, position })
            .unwrap();
    }

    fn overlaps_sphere(&self, center: Vec3, radius: f32) -> bool {
        let snapshot = self.driver.world().query_snapshot().unwrap();
        let shape = Shape::sphere(radius).unwrap();
        let pose = Pose::new(center, Quat::IDENTITY).unwrap();
        !snapshot
            .overlap(
                &shape,
                pose,
                QueryFilter::default(),
                QueryOverrides::default(),
            )
            .unwrap()
            .is_empty()
    }
}

const ZONE_SCENE: &str = r#"
[[assets]]
path = "/zone.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "zone"
[objects.collider]
shape = "sphere"
radius = 1.0
trigger = true
[[objects.lite_scripts]]
path = "/zone.lua"
[[objects]]
id = "ball"
[objects.transform]
position = [0.5, 0.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
"#;

const ZONE: &str = r#"
local Script = {}
function Script:on_trigger_enter(other) rve.log("enter") end
function Script:on_trigger_stay(other) rve.log("stay") end
function Script:on_trigger_exit(other) rve.log("exit") end
return Script
"#;

#[test]
fn trigger_stay_fires_once_per_update_while_the_pair_is_alive_and_stops_after_exit() {
    let mut scene = Loop::new(ZONE_SCENE, &[("zone.lua", ZONE)]);

    assert_eq!(
        scene.step(),
        Vec::<String>::new(),
        "no pair before the first step"
    );
    assert_eq!(
        scene.step(),
        ["enter", "stay"],
        "enter is delivered before stay"
    );
    assert_eq!(scene.step(), ["stay"]);
    assert_eq!(scene.step(), ["stay"]);

    scene.move_to("ball", Vec3::new(5.0, 0.0, 0.0));
    assert_eq!(
        scene.step(),
        ["stay"],
        "the pair still exists when the update runs before the teleport step"
    );
    assert_eq!(scene.step(), ["exit"]);
    assert_eq!(scene.step(), Vec::<String>::new(), "no stay after the exit");
}

const PLAYER_SCENE: &str = r#"
[[assets]]
path = "/player.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "floor"
[objects.transform]
position = [0.0, -0.5, 0.0]
[objects.collider]
shape = "box"
half_extents = [10.0, 0.5, 10.0]
[[objects]]
id = "player"
[objects.transform]
position = [0.0, 0.8, 0.0]
[objects.character_controller]
radius = 0.3
height = 1.0
step_height = 0.0
use_gravity = true
jump_height = 1.0
[[objects.lite_scripts]]
path = "/player.lua"
"#;

const JUMPER: &str = r#"
local Script = {}
local jumped = false
function Script:on_update()
  local player = rve.find("player")
  local y = player:get_transform().position.y
  if not jumped and player:is_grounded() then
    assert(player:jump())
    jumped = true
    rve.log("jump")
  end
  local moved = player:simple_move({x=0, y=0, z=0})
  rve.log(string.format("y %.6f grounded %s", y, tostring(moved.grounded)))
end
return Script
"#;

fn parse_y(message: &str) -> Option<f32> {
    message
        .strip_prefix("y ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[test]
fn simple_move_jump_reaches_the_configured_height_and_lands_grounded() {
    let mut scene = Loop::new(PLAYER_SCENE, &[("player.lua", JUMPER)]);
    let mut log = Vec::new();
    for _ in 0..240 {
        log.extend(scene.step());
    }

    let jump_frame = log
        .iter()
        .position(|message| message == "jump")
        .expect("grounded character jumps");
    let start_y = log
        .iter()
        .find_map(|message| parse_y(message))
        .expect("first frame reports the start height");
    let apex = log
        .iter()
        .filter_map(|message| parse_y(message))
        .fold(f32::MIN, f32::max);
    assert!(
        (apex - start_y - 1.0).abs() < 0.06,
        "apex {apex} should be start {start_y} plus jump_height 1.0"
    );
    let last = log.last().unwrap();
    assert!(
        last.ends_with("grounded true"),
        "character lands and is grounded again: {last}"
    );
    assert!(jump_frame > 0, "jump is not the first message");
}

#[test]
fn jump_is_refused_while_airborne_and_controller_fields_round_trip_through_the_world() {
    let scene = r#"
[[assets]]
path = "/airborne.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "player"
[objects.transform]
position = [0.0, 20.0, 0.0]
[objects.character_controller]
radius = 0.3
height = 1.0
use_gravity = true
[[objects.lite_scripts]]
path = "/airborne.lua"
"#;
    let script = r#"
local Script = {}
function Script:on_update()
  local player = rve.find("player")
  rve.log("airborne jump " .. tostring(player:jump()))
  local controller = player:get_controller()
  rve.log(string.format("radius %.3f", controller.radius))
  controller.radius = 0.5
  rve.log("set " .. tostring(player:set_controller(controller)))
  rve.log(string.format("readback %.3f", player:get_controller().radius))
end
return Script
"#;
    let mut scene_loop = Loop::new(scene, &[("airborne.lua", script)]);
    assert!(
        !scene_loop.overlaps_sphere(Vec3::new(0.45, 20.0, 0.0), 0.05),
        "the original controller capsule does not reach the probe sphere"
    );
    let messages = scene_loop.step();
    assert!(
        messages.contains(&"airborne jump false".to_owned()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"radius 0.300".to_owned()),
        "{messages:?}"
    );
    assert!(messages.contains(&"set true".to_owned()), "{messages:?}");
    assert!(
        messages.contains(&"readback 0.500".to_owned()),
        "{messages:?}"
    );
    let player = scene_loop.handle("player");
    let stored = scene_loop
        .world
        .object(player)
        .unwrap()
        .character_controller
        .unwrap();
    assert_eq!(
        stored.radius, 0.5,
        "controller write reaches the world component"
    );
    assert!(
        scene_loop.overlaps_sphere(Vec3::new(0.45, 20.0, 0.0), 0.05),
        "SetController resizes the physics capsule before the next query snapshot"
    );
}

#[test]
fn collider_and_body_components_read_back_after_writes_and_commit() {
    let scene = r#"
[[assets]]
path = "/components.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "crate"
[objects.collider]
shape = "box"
half_extents = [0.5, 0.5, 0.5]
[objects.rigid_body]
kind = "dynamic"
[[objects.lite_scripts]]
path = "/components.lua"
"#;
    let script = r#"
local Script = {}
local done = false
function Script:on_update()
  if done then return end
  done = true
  local crate = rve.find("crate")
  assert(crate:set_collider({shape="sphere", radius=0.75, trigger=true}))
  local collider = crate:get_collider()
  rve.log("shape " .. collider.shape .. " " .. tostring(collider.trigger))
  assert(crate:set_rigid_body({kind="kinematic", mass=3.0}))
  rve.log("kind " .. crate:get_rigid_body().kind)
end
return Script
"#;
    let mut scene_loop = Loop::new(scene, &[("components.lua", script)]);
    let messages = scene_loop.step();
    assert!(
        messages.contains(&"shape sphere true".to_owned()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"kind kinematic".to_owned()),
        "{messages:?}"
    );
    let crate_handle = scene_loop.handle("crate");
    let object = scene_loop.world.object(crate_handle).unwrap();
    let collider = object.collider.as_ref().unwrap();
    assert!(collider.trigger);
    assert_eq!(collider.radius, 0.75);
    assert_eq!(
        object.rigid_body.as_ref().unwrap().kind,
        engine_scene::RigidBodyKind::Kinematic
    );
}

#[test]
fn failed_callback_discards_component_writes_and_physics_commands() {
    let scene = r#"
[[assets]]
path = "/rollback.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "crate"
[objects.collider]
shape = "sphere"
radius = 0.5
[[objects.lite_scripts]]
path = "/rollback.lua"
"#;
    let script = r#"
local Script = {}
function Script:on_update()
  local crate = rve.find("crate")
  assert(crate:set_collider({radius=1.5}))
  error("rollback component mutation")
end
return Script
"#;
    let mut scene_loop = Loop::new(scene, &[("rollback.lua", script)]);
    assert!(!scene_loop.overlaps_sphere(Vec3::new(0.9, 0.0, 0.0), 0.05));

    let events = scene_loop.step();
    assert!(
        events
            .iter()
            .any(|message| message.contains("rollback component mutation"))
    );
    assert_eq!(
        scene_loop
            .world
            .object(scene_loop.handle("crate"))
            .unwrap()
            .collider
            .as_ref()
            .unwrap()
            .radius,
        0.5,
        "the World component is unchanged after the callback error"
    );
    assert!(
        !scene_loop.overlaps_sphere(Vec3::new(0.9, 0.0, 0.0), 0.05),
        "the queued SetCollider command is discarded with the callback"
    );
}

#[test]
fn runtime_set_layer_updates_physics_query_filtering() {
    let scene = r#"
[[assets]]
path = "/layers.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "observer"
[objects.collider]
shape = "sphere"
radius = 0.2
[[objects.lite_scripts]]
path = "/layers.lua"
[[objects]]
id = "target"
[objects.collider]
shape = "sphere"
radius = 0.5
"#;
    let script = r#"
local Script = {}
local changed = false
function Script:on_update()
  local observer = rve.find("observer")
  if not changed then
    changed = true
    local target = rve.find("target")
    assert(target:set_layer(3))
    rve.log("pending layer " .. tostring(target:layer()))
  else
    local origin = {x=0, y=0, z=0}
    local in_layer_three = rve.physics.overlap_sphere(origin, 1.0, {layer_mask=8, ignore=observer})
    local in_layer_zero = rve.physics.overlap_sphere(origin, 1.0, {layer_mask=1, ignore=observer})
    rve.log("layer three " .. #in_layer_three)
    rve.log("layer zero " .. #in_layer_zero)
  end
end
return Script
"#;
    let mut scene_loop = Loop::new(scene, &[("layers.lua", script)]);

    assert!(scene_loop.step().contains(&"pending layer 3".to_owned()));
    let messages = scene_loop.step();
    assert!(
        messages.contains(&"layer three 1".to_owned()),
        "{messages:?}"
    );
    assert!(
        messages.contains(&"layer zero 0".to_owned()),
        "{messages:?}"
    );
    assert_eq!(scene_loop.world.layer(scene_loop.handle("target")), Some(3));
}

#[test]
fn overlap_and_cast_queries_respect_layer_mask_triggers_and_ignore_lists() {
    let scene = r#"
[[assets]]
path = "/probe.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "probe"
[objects.collider]
shape = "sphere"
radius = 0.1
[[objects.lite_scripts]]
path = "/probe.lua"
[[objects]]
id = "a"
[objects.collider]
shape = "sphere"
radius = 0.5
[[objects]]
id = "t"
[objects.collider]
shape = "sphere"
radius = 0.5
trigger = true
[[objects]]
id = "b"
layer = 3
[objects.collider]
shape = "sphere"
radius = 0.5
"#;
    let script = r#"
local Script = {}
local ran = false
function Script:on_update()
  if ran then return end
  ran = true
  local origin = {x=0, y=0, z=0}
  local me = rve.find("probe")
  local a = rve.find("a")
  local count = function(list) return #list end
  rve.log("default " .. count(rve.physics.overlap_sphere(origin, 0.6, {})))
  rve.log("box " .. count(rve.physics.overlap_box(origin, {x=1.2, y=1.2, z=1.2}, {})))
  rve.log("capsule " .. count(rve.physics.overlap_capsule(origin, 0.6, 1.2, {})))
  rve.log("self " .. count(rve.physics.overlap_sphere(origin, 0.6, {ignore = me})))
  rve.log("list " .. count(rve.physics.overlap_sphere(origin, 0.6, {ignore = {me, a}})))
  rve.log("triggers " .. count(rve.physics.overlap_sphere(origin, 0.6, {include_triggers = true})))
  rve.log("mask " .. count(rve.physics.overlap_sphere(origin, 0.6, {layer_mask = 4294967287})))
  local hit = rve.physics.sphere_cast({x=-3, y=0, z=0}, 0.2, {x=6, y=0, z=0}, {ignore = me})
  rve.log("cast " .. (hit and hit.object.id or "none"))
  local box_hit = rve.physics.box_cast({x=-3, y=0, z=0}, {x=0.2, y=0.2, z=0.2}, {x=6, y=0, z=0}, {ignore = me})
  rve.log("box_cast " .. (box_hit and box_hit.object.id or "none"))
  local capsule_hit = rve.physics.capsule_cast({x=-3, y=0, z=0}, 0.1, 0.4, {x=6, y=0, z=0}, {ignore = me})
  rve.log("capsule_cast " .. (capsule_hit and capsule_hit.object.id or "none"))
  local trigger_hit = rve.physics.sphere_cast({x=-3, y=0, z=0}, 0.2, {x=6, y=0, z=0}, {ignore = {me, a}, include_triggers = true})
  rve.log("trigger_cast " .. (trigger_hit and trigger_hit.object.id or "none"))
  local masked = rve.physics.sphere_cast({x=-3, y=0, z=0}, 0.2, {x=6, y=0, z=0}, {ignore = {me, a}, layer_mask = 4294967287})
  rve.log("cast_masked " .. (masked and masked.object.id or "none"))
  local ray_hits = rve.physics.raycast_all({x=-3, y=0, z=0}, {x=1, y=0, z=0}, 6, {ignore = {me, a}, include_triggers = true})
  rve.log("raycast_all " .. #ray_hits .. " " .. (ray_hits[1] and ray_hits[1].object.id or "none"))
end
return Script
"#;
    let mut scene_loop = Loop::new(scene, &[("probe.lua", script)]);
    let messages = scene_loop.step();
    for expected in [
        "default 3",
        "box 3",
        "capsule 3",
        "self 2",
        "list 1",
        "triggers 4",
        "mask 2",
        "cast a",
        "box_cast a",
        "capsule_cast a",
        "trigger_cast t",
        "cast_masked none",
        "raycast_all 2 t",
    ] {
        assert!(
            messages.iter().any(|message| message == expected),
            "missing {expected:?} in {messages:?}"
        );
    }
}
