//! Интеграционные тесты ABI v17 для скриптов физики. Цикл повторяет рантайм: обновление скриптов
//! по снимку прошлого шага, команды скрипта и синхронизация трансформов, шаг физики, затем
//! снимок и события. Окно и GPU не нужны; время шага фиксировано.

use super::{event_messages, sources, world};
use crate::ScriptTime;
use crate::host::ScriptHost;
use engine_physics::{
    ModelGeometry, ModelMesh, PhysicsSceneDesc, PhysicsSceneSync, Pose, QueryFilter,
    QueryOverrides, Shape, ShapeCache, StepDriver,
};
use engine_scene::{LocalTransform, ObjectEdit, ObjectHandle, World};
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
        Self::with_models(scene, scripts, &[])
    }

    fn with_models(scene: &str, scripts: &[(&str, &str)], models: &[ModelGeometry]) -> Self {
        let world = world(scene);
        let desc = PhysicsSceneDesc::from_world(&world, models, Vec3::new(0.0, -9.81, 0.0), 60, 8)
            .unwrap();
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

const REJECT_SCENE: &str = r#"
[[assets]]
path = "/reject.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "ball"
[objects.transform]
position = [0.0, 5.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
[objects.rigid_body]
kind = "dynamic"
mass = 1.0
[[objects.lite_scripts]]
path = "/reject.lua"
"#;

#[test]
fn degenerate_collider_and_mass_writes_are_rejected_at_the_call() {
    let script = r#"
local Script = {}
function Script:on_update()
  local ball = rve.find("ball")
  rve.log("collider " .. tostring(pcall(function()
    return ball:set_collider({shape = "box", size = {1e-20, 1e-20, 1e-20}})
  end)))
  rve.log("body " .. tostring(pcall(function()
    return ball:set_rigid_body({mass = 1e-39})
  end)))
end
return Script
"#;
    let mut scene = Loop::new(REJECT_SCENE, &[("reject.lua", script)]);
    let messages = scene.step();
    assert_eq!(messages, ["collider false", "body false"]);

    let ball = scene.world.object(scene.handle("ball")).unwrap();
    assert_eq!(ball.collider.as_ref().unwrap().radius, 0.5);
    assert_eq!(ball.rigid_body.unwrap().mass, 1.0);
}

const INACTIVE_SCENE: &str = r#"
[[assets]]
path = "/inactive.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "crate"
active = false
[objects.transform]
position = [0.0, 5.0, 0.0]
[objects.collider]
shape = "box"
half_extents = [0.5, 0.5, 0.5]
[objects.rigid_body]
kind = "dynamic"
[[objects]]
id = "driver"
[[objects.lite_scripts]]
path = "/inactive.lua"
"#;

#[test]
fn component_writes_on_inactive_object_apply_when_it_activates() {
    let script = r#"
local Script = {}
local done = false
function Script:on_update()
  if done then return end
  done = true
  local crate = rve.find("crate")
  rve.log("set " .. tostring(pcall(function()
    return crate:set_collider({shape = "sphere", radius = 0.25})
  end)))
end
return Script
"#;
    let mut scene = Loop::new(INACTIVE_SCENE, &[("inactive.lua", script)]);
    assert_eq!(scene.step(), ["set true"]);

    let crate_handle = scene.handle("crate");
    scene
        .world
        .apply_edit(&ObjectEdit::SetActive {
            object: crate_handle,
            active: true,
        })
        .unwrap();
    let changes = scene
        .sync
        .insert_objects(
            &scene.world,
            &[crate_handle],
            &[],
            &mut ShapeCache::default(),
        )
        .unwrap();
    scene
        .driver
        .advance_step_with_changes(DT, changes, [])
        .unwrap();

    assert!(
        !scene.overlaps_sphere(Vec3::new(0.4, 5.0, 0.0), 0.05),
        "the activated body uses the sphere written while inactive, not the box"
    );
}

const CEILING_SCENE: &str = r#"
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
id = "ceiling"
[objects.transform]
position = [0.0, 1.75, 0.0]
[objects.collider]
shape = "box"
half_extents = [10.0, 0.1, 10.0]
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

#[test]
fn ceiling_hit_zeroes_upward_speed_so_the_character_falls_back() {
    let script = r#"
local Script = {}
local jumped = false
local last_collided = false
function Script:on_update()
  local player = rve.find("player")
  if not jumped and player:is_grounded() then
    assert(player:jump())
    jumped = true
    rve.log("jump")
  end
  rve.log(string.format("c %s v %.3f", tostring(last_collided), player:vertical_velocity()))
  last_collided = player:simple_move({x=0, y=0, z=0}).collided
end
return Script
"#;
    let mut scene = Loop::new(CEILING_SCENE, &[("player.lua", script)]);
    let mut log = Vec::new();
    for _ in 0..10 {
        log.extend(scene.step());
    }

    assert!(log.iter().any(|m| m == "jump"), "jump happens: {log:?}");
    // Под потолком тело упирается каждый кадр. Без обнуления скорость вверх копилась бы
    // (4.429 и выше), а с обнулением к концу окна она не положительна.
    let last = log.last().unwrap();
    let speed: f32 = last.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(speed <= 0.0, "still rising under the ceiling: {log:?}");
}

#[test]
fn trigger_stay_skips_partners_deactivated_or_destroyed_while_the_snapshot_is_stale() {
    let mut deactivated = Loop::new(ZONE_SCENE, &[("zone.lua", ZONE)]);
    assert_eq!(deactivated.step(), Vec::<String>::new());
    assert!(deactivated.step().contains(&"stay".to_owned()));
    let ball = deactivated.handle("ball");
    deactivated
        .world
        .apply_edit(&ObjectEdit::SetActive {
            object: ball,
            active: false,
        })
        .unwrap();
    // Снимок прошлого шага ещё держит пару: стой для неактивного партнёра быть не должно.
    assert!(
        !deactivated.step().contains(&"stay".to_owned()),
        "inactive partner must not get on_trigger_stay"
    );

    let mut destroyed = Loop::new(ZONE_SCENE, &[("zone.lua", ZONE)]);
    destroyed.step();
    assert!(destroyed.step().contains(&"stay".to_owned()));
    let ball = destroyed.handle("ball");
    destroyed.world.despawn_many(&[ball]).unwrap();
    // Тело удалённого объекта ещё в снимке до следующего шага: проверяем сам кадр обновления.
    destroyed.host.update(
        &mut destroyed.world,
        DT,
        ScriptTime {
            elapsed: 1.0,
            frame: 99,
        },
    );
    assert!(
        !event_messages(&mut destroyed.host).contains(&"stay".to_owned()),
        "destroyed partner must not get on_trigger_stay"
    );
}

const ROLLBACK_BODY_SCENE: &str = r#"
[[assets]]
path = "/rollback_body.lua"
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
mass = 1.0
[[objects.lite_scripts]]
path = "/rollback_body.lua"
[[objects]]
id = "player"
[objects.transform]
position = [3.0, 0.0, 0.0]
[objects.character_controller]
radius = 0.3
height = 1.0
use_gravity = false
"#;

#[test]
fn failed_callback_discards_rigid_body_and_controller_writes() {
    let script = r#"
local Script = {}
function Script:on_update()
  assert(rve.find("crate"):set_rigid_body({kind="kinematic", mass=3.0}))
  assert(rve.find("player"):set_controller({radius=0.5}))
  error("rollback body mutation")
end
return Script
"#;
    let mut scene = Loop::new(ROLLBACK_BODY_SCENE, &[("rollback_body.lua", script)]);
    let messages = scene.step();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("rollback body mutation")),
        "{messages:?}"
    );

    let crate_handle = scene.handle("crate");
    let crate_object = scene.world.object(crate_handle).unwrap();
    assert_eq!(
        crate_object.rigid_body.unwrap().kind,
        engine_scene::RigidBodyKind::Dynamic
    );
    assert_eq!(crate_object.rigid_body.unwrap().mass, 1.0);
    let player = scene.world.object(scene.handle("player")).unwrap();
    assert_eq!(player.character_controller.unwrap().radius, 0.3);
    let body = scene
        .sync
        .bindings()
        .iter()
        .find(|binding| binding.object == crate_handle)
        .unwrap()
        .body;
    let state = scene
        .driver
        .world()
        .query_snapshot()
        .unwrap()
        .body_state(body)
        .unwrap();
    assert_eq!(state.body_kind, engine_physics::BodyKind::Dynamic);
}

const TRIMESH_PROP_SCENE: &str = r#"
[[assets]]
path = "/mesh_prop.lua"
[[assets]]
path = "/models/quad.fbx"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "prop"
[objects.mesh_renderer]
model = "/models/quad.fbx"
[objects.collider]
shape = "triangle_mesh"
[objects.rigid_body]
kind = "static"
[[objects.lite_scripts]]
path = "/mesh_prop.lua"
"#;

fn quad_model() -> ModelGeometry {
    ModelGeometry {
        path: "models/quad.fbx".to_owned(),
        meshes: vec![ModelMesh {
            positions: vec![
                Vec3::new(-1.0, 0.0, -1.0),
                Vec3::new(1.0, 0.0, -1.0),
                Vec3::new(1.0, 0.0, 1.0),
                Vec3::new(-1.0, 0.0, 1.0),
            ],
            indices: vec![0, 2, 1, 0, 3, 2],
        }],
    }
}

#[test]
fn dynamic_body_on_triangle_mesh_prop_is_rejected_at_the_call_and_world_stays_static() {
    let script = r#"
local Script = {}
local done = false
function Script:on_update()
  if done then return end
  done = true
  local ok, err = pcall(function()
    return rve.find("prop"):set_rigid_body({kind="dynamic"})
  end)
  local mesh_rule = string.find(tostring(err), "triangle mesh", 1, true) ~= nil
  rve.log("dynamic " .. tostring(ok) .. " " .. tostring(mesh_rule))
end
return Script
"#;
    let mut scene = Loop::with_models(
        TRIMESH_PROP_SCENE,
        &[("mesh_prop.lua", script)],
        &[quad_model()],
    );
    let messages = scene.step();
    assert!(
        messages.contains(&"dynamic false true".to_owned()),
        "{messages:?}"
    );

    let prop = scene.handle("prop");
    assert_eq!(
        scene.world.object(prop).unwrap().rigid_body.unwrap().kind,
        engine_scene::RigidBodyKind::Static
    );
    let body = scene
        .sync
        .bindings()
        .iter()
        .find(|binding| binding.object == prop)
        .unwrap()
        .body;
    let state = scene
        .driver
        .world()
        .query_snapshot()
        .unwrap()
        .body_state(body)
        .unwrap();
    assert_eq!(state.body_kind, engine_physics::BodyKind::Static);
}

#[test]
fn triangle_mesh_prop_becomes_a_dynamic_box_when_the_collider_changes_first_in_one_callback() {
    let script = r#"
local Script = {}
local done = false
function Script:on_update()
  if done then return end
  done = true
  local prop = rve.find("prop")
  rve.log("shape " .. tostring(pcall(function()
    return prop:set_collider({shape="box", size={x=0.5, y=0.5, z=0.5}})
  end)))
  rve.log("dynamic " .. tostring(pcall(function()
    return prop:set_rigid_body({kind="dynamic"})
  end)))
end
return Script
"#;
    let mut scene = Loop::with_models(
        TRIMESH_PROP_SCENE,
        &[("mesh_prop.lua", script)],
        &[quad_model()],
    );
    let messages = scene.step();
    assert!(messages.contains(&"shape true".to_owned()), "{messages:?}");
    assert!(
        messages.contains(&"dynamic true".to_owned()),
        "{messages:?}"
    );

    let prop = scene.handle("prop");
    let object = scene.world.object(prop).unwrap();
    assert_eq!(
        object.collider.as_ref().unwrap().shape,
        engine_scene::ColliderShape::Box
    );
    assert_eq!(
        object.rigid_body.unwrap().kind,
        engine_scene::RigidBodyKind::Dynamic
    );
    let body = scene
        .sync
        .bindings()
        .iter()
        .find(|binding| binding.object == prop)
        .unwrap()
        .body;
    let state = scene
        .driver
        .world()
        .query_snapshot()
        .unwrap()
        .body_state(body)
        .unwrap();
    assert_eq!(state.body_kind, engine_physics::BodyKind::Dynamic);
}

const DYNAMIC_CRATE_SCENE: &str = r#"
[[assets]]
path = "/crate.lua"
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
path = "/crate.lua"
"#;

#[test]
fn triangle_mesh_collider_on_dynamic_body_is_rejected_and_the_box_stays() {
    let script = r#"
local Script = {}
function Script:on_update()
  rve.log("mesh " .. tostring(pcall(function()
    return rve.find("crate"):set_collider({shape="triangle_mesh"})
  end)))
end
return Script
"#;
    let mut scene = Loop::new(DYNAMIC_CRATE_SCENE, &[("crate.lua", script)]);
    let messages = scene.step();
    assert!(messages.contains(&"mesh false".to_owned()), "{messages:?}");

    let crate_handle = scene.handle("crate");
    assert_eq!(
        scene
            .world
            .object(crate_handle)
            .unwrap()
            .collider
            .as_ref()
            .unwrap()
            .shape,
        engine_scene::ColliderShape::Box
    );
    let body = scene
        .sync
        .bindings()
        .iter()
        .find(|binding| binding.object == crate_handle)
        .unwrap()
        .body;
    let state = scene
        .driver
        .world()
        .query_snapshot()
        .unwrap()
        .body_state(body)
        .unwrap();
    assert_eq!(state.body_kind, engine_physics::BodyKind::Dynamic);
}

#[test]
fn controller_scaled_flat_or_to_zero_mid_play_keeps_the_frame_loop_running() {
    let script = r#"
local Script = {}
function Script:on_update()
  rve.find("player"):simple_move({x=0, y=0, z=0})
end
return Script
"#;
    let mut scene = Loop::new(PLAYER_SCENE, &[("player.lua", script)]);
    scene.step();
    let player = scene.handle("player");
    let transform = *scene.world.local_transform(player).unwrap();
    for scale in [Vec3::new(1.0, 0.0, 1.0), Vec3::ZERO] {
        scene
            .world
            .set_local_transform(player, LocalTransform { scale, ..transform })
            .unwrap();
        for _ in 0..5 {
            scene.step();
        }
    }
}

const INACTIVE_LAYER_SCENE: &str = r#"
[[assets]]
path = "/inactive_layer.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "crate"
active = false
[objects.transform]
position = [0.0, 5.0, 0.0]
[objects.collider]
shape = "box"
half_extents = [0.5, 0.5, 0.5]
[objects.rigid_body]
kind = "dynamic"
[[objects]]
id = "driver"
[[objects.lite_scripts]]
path = "/inactive_layer.lua"
"#;

#[test]
fn reactivated_object_takes_layer_and_shape_written_while_inactive() {
    let script = r#"
local Script = {}
local done = false
function Script:on_update()
  if done then return end
  done = true
  local crate = rve.find("crate")
  rve.log("layer " .. tostring(crate:set_layer(2)))
  rve.log("shape " .. tostring(crate:set_collider({shape="sphere", radius=0.25})))
end
return Script
"#;
    let mut scene = Loop::new(INACTIVE_LAYER_SCENE, &[("inactive_layer.lua", script)]);
    assert_eq!(scene.step(), ["layer true", "shape true"]);

    let crate_handle = scene.handle("crate");
    scene
        .world
        .apply_edit(&ObjectEdit::SetActive {
            object: crate_handle,
            active: true,
        })
        .unwrap();
    let changes = scene
        .sync
        .insert_objects(
            &scene.world,
            &[crate_handle],
            &[],
            &mut ShapeCache::default(),
        )
        .unwrap();
    scene
        .driver
        .advance_step_with_changes(DT, changes, [])
        .unwrap();

    let probe = |layer_mask: u32, x: f32| {
        let snapshot = scene.driver.world().query_snapshot().unwrap();
        !snapshot
            .overlap(
                &Shape::sphere(0.05).unwrap(),
                Pose::new(Vec3::new(x, 5.0, 0.0), Quat::IDENTITY).unwrap(),
                QueryFilter {
                    layer_mask,
                    ..QueryFilter::default()
                },
                QueryOverrides::default(),
            )
            .unwrap()
            .is_empty()
    };
    assert!(probe(1 << 2, 0.2), "the body moved to layer 2");
    assert!(!probe(1 << 0, 0.2), "the body is not on layer 0 anymore");
    assert!(
        !probe(1 << 2, 0.4),
        "the sphere written while inactive replaces the box"
    );
}
