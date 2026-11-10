use super::{event_messages, sources, world};
use crate::host::ScriptHost;
use crate::{ScriptEventLevel, ScriptTime};
use engine_scene::World;
use mlua::Table;

const LIFE: &str = r#"
local Script = {}
function Script:on_enable() rve.log("enable") end
function Script:on_start() rve.log("start") end
function Script:on_update()
  rve.log("update")
  if rve.__disable then self.script.set_enabled(false) end
end
function Script:on_disable() rve.log("disable") end
function Script:on_destroy() rve.log("destroy") end
return Script
"#;

fn life_world(prop_active: bool) -> World {
    world(&format!(
        r#"
[[assets]]
path = "/life.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "prop"
active = {prop_active}
[[objects.lite_scripts]]
path = "/life.lua"
"#
    ))
}

fn frame() -> ScriptTime {
    ScriptTime {
        elapsed: 0.0,
        frame: 0,
    }
}

#[test]
fn on_enable_runs_before_the_first_start_and_hierarchy_activity_drives_the_callbacks() {
    let mut world = life_world(false);
    let prop = world.handle("prop").unwrap();
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("life.lua", LIFE)]), None).unwrap();

    host.start(&mut world, frame());
    assert!(
        event_messages(&mut host).is_empty(),
        "inactive object gets no callbacks"
    );

    world.set_active(prop, true).unwrap();
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(event_messages(&mut host), ["enable", "start", "update"]);

    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(event_messages(&mut host), ["update"]);

    world.set_active(prop, false).unwrap();
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(
        event_messages(&mut host),
        ["disable"],
        "no update while inactive"
    );

    world.set_active(prop, true).unwrap();
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(
        event_messages(&mut host),
        ["enable", "update"],
        "re-enabling does not start the script a second time"
    );

    host.destroy(&mut world, frame());
    assert_eq!(
        event_messages(&mut host),
        ["disable", "destroy"],
        "a running instance is disabled before it is destroyed"
    );
}

#[test]
fn script_set_enabled_disables_on_the_next_phase_and_keeps_it_off() {
    let mut world = life_world(true);
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("life.lua", LIFE)]), None).unwrap();
    host.start(&mut world, frame());
    assert_eq!(event_messages(&mut host), ["enable", "start"]);

    let rve: Table = host.lua.globals().get("rve").unwrap();
    rve.set("__disable", true).unwrap();
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(
        event_messages(&mut host),
        ["update"],
        "the write commits after the callback that made it"
    );

    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(event_messages(&mut host), ["disable"]);

    let prop = world.handle("prop").unwrap();
    world.set_active(prop, false).unwrap();
    world.set_active(prop, true).unwrap();
    host.update(&mut world, 1.0 / 60.0, frame());
    assert!(
        event_messages(&mut host).is_empty(),
        "a disabled instance stays off when its object becomes active again"
    );

    host.destroy(&mut world, frame());
    assert_eq!(event_messages(&mut host), ["destroy"]);
}

#[test]
fn an_error_in_on_enable_disables_the_instance_before_on_start() {
    const FAILING: &str = r#"
local Script = {}
function Script:on_enable() error("enable failed") end
function Script:on_start() rve.log("start") end
function Script:on_update() rve.log("update") end
return Script
"#;
    let mut world = life_world(true);
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("life.lua", FAILING)]), None).unwrap();
    host.start(&mut world, frame());
    let messages = event_messages(&mut host);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("enable failed")),
        "{messages:?}"
    );
    assert!(
        !messages.iter().any(|message| message == "start"),
        "{messages:?}"
    );
    host.update(&mut world, 1.0 / 60.0, frame());
    assert!(
        event_messages(&mut host).is_empty(),
        "the failed instance stays off"
    );
}

const RIG: &str = r#"
local Script = {}
function Script:on_start()
  local parent = rve.find_by_name("Parent")
  local kid = self.object
  rve.log("parent:" .. parent.name)
  rve.log("set_parent:" .. tostring(kid:set_parent(parent, true)))
  rve.log("tag:" .. tostring(kid:set_tag("enemy")))
  rve.log("bad_tag:" .. tostring(kid:set_tag("bad tag")))
  rve.log("layer:" .. tostring(kid:set_layer(3)))
  rve.log("bad_layer:" .. tostring(kid:set_layer(40)))
  rve.log("pos:" .. tostring(kid:set_world_position({x = 1, y = 2, z = 3})))
  rve.log("is_active:" .. tostring(kid:is_active()))
  rve.log("has_collider:" .. tostring(kid:has_component("collider")))
  rve.log("has_camera:" .. tostring(kid:has_component("camera")))
  rve.log("children:" .. #parent:children())
  rve.log("tagged:" .. #rve.find_all_with_tag("enemy"))
end
return Script
"#;

fn rig_world() -> World {
    world(
        r#"
[[assets]]
path = "/rig.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "parent"
name = "Parent"
[objects.transform]
position = [10.0, 0.0, 0.0]
[[objects]]
id = "kid"
[[objects.lite_scripts]]
path = "/rig.lua"
"#,
    )
}

#[test]
fn lua_hierarchy_tag_layer_and_world_writes_commit_with_the_call() {
    let mut world = rig_world();
    let mut host = ScriptHost::load(&world, "scene", &sources(&[("rig.lua", RIG)]), None).unwrap();
    host.start(&mut world, frame());

    let messages = event_messages(&mut host);
    assert_eq!(
        messages,
        [
            "parent:Parent",
            "set_parent:true",
            "tag:true",
            "bad_tag:false",
            "layer:true",
            "bad_layer:false",
            "pos:true",
            "is_active:true",
            "has_collider:false",
            "has_camera:false",
            // Reads of the hierarchy and tag queries see the last commit, not this call.
            "children:0",
            "tagged:0",
        ]
    );
    let parent = world.handle("parent").unwrap();
    let kid = world.handle("kid").unwrap();
    assert_eq!(world.object(kid).unwrap().parent, Some(parent));
    assert_eq!(world.tag(kid), Some("enemy"));
    assert_eq!(world.layer(kid), Some(3));
    let position = world.world_position(kid).unwrap();
    assert!(
        position.distance(glam::Vec3::new(1.0, 2.0, 3.0)) < 1e-4,
        "{position:?}"
    );
}

const CYCLE: &str = r#"
local Script = {}
function Script:on_start()
  local parent = rve.find_by_name("Parent")
  self.object:set_tag("enemy")
  self.object:set_layer(3)
  -- The kid is already a child of the parent, so making the parent a child of the kid is a cycle.
  parent:set_parent(self.object, true)
end
return Script
"#;

#[test]
fn a_cycle_in_a_call_rolls_back_every_write_of_that_call() {
    let mut world = World::from_document(
        &engine_scene::SceneDocument::parse(
            "cycle.rvescene",
            r#"
[[assets]]
path = "/cycle.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "parent"
name = "Parent"
[[objects]]
id = "kid"
parent = "parent"
[[objects.lite_scripts]]
path = "/cycle.lua"
"#,
        )
        .unwrap(),
    )
    .unwrap();
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("cycle.lua", CYCLE)]), None).unwrap();
    host.start(&mut world, frame());

    let messages = event_messages(&mut host);
    assert!(
        messages.iter().any(|message| message.contains("цикл")),
        "{messages:?}"
    );
    let kid = world.handle("kid").unwrap();
    assert_eq!(world.tag(kid), Some(""));
    assert_eq!(world.layer(kid), Some(0));
    assert_eq!(
        world
            .object(world.handle("parent").unwrap())
            .unwrap()
            .parent,
        None
    );
}

#[test]
fn a_script_declared_disabled_gets_no_callbacks_at_all() {
    let mut world = world(
        r#"
[[assets]]
path = "/life.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "prop"
[[objects.lite_scripts]]
path = "/life.lua"
enabled = false
"#,
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("life.lua", LIFE)]), None).unwrap();
    host.start(&mut world, frame());
    host.update(&mut world, 1.0 / 60.0, frame());
    host.destroy(&mut world, frame());
    assert!(event_messages(&mut host).is_empty());
}

/// Камера `cam_b` внутри `rig`; `main` вне него держит скрипт. `active_camera` выбирает стартовую камеру.
fn camera_rig_world(active_camera: &str) -> World {
    let main_active = active_camera == "main";
    world(&format!(
        r#"
[[assets]]
path = "/camera_rig.lua"
[[objects]]
id = "main"
[objects.camera]
active = {main_active}
[[objects.lite_scripts]]
path = "/camera_rig.lua"
[[objects]]
id = "rig"
[[objects]]
id = "cam_b"
parent = "rig"
[objects.camera]
active = {cam_b_active}
"#,
        cam_b_active = !main_active,
    ))
}

const ACTIVATE_INSIDE_THEN_HIDE: &str = r#"
local Script = {}
function Script:on_start()
  rve.find("cam_b"):activate_camera()
  rve.find("rig"):set_active(false)
end
return Script
"#;

const ACTIVATE_OUTSIDE_THEN_HIDE: &str = r#"
local Script = {}
function Script:on_start()
  rve.find("main"):activate_camera()
  rve.find("rig"):set_active(false)
end
return Script
"#;

#[test]
fn hiding_the_subtree_of_the_camera_activated_in_the_same_call_is_rejected_without_panic() {
    let mut world = camera_rig_world("main");
    let main = world.handle("main").unwrap();
    let rig = world.handle("rig").unwrap();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("camera_rig.lua", ACTIVATE_INSIDE_THEN_HIDE)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());

    let events = host.drain_events();
    assert!(
        events.iter().any(|event| {
            event.level == ScriptEventLevel::Error
                && event
                    .message
                    .contains("нельзя скрыть объект с активной камерой")
        }),
        "the rejection must be reported as an error: {events:?}"
    );
    assert_eq!(
        world.active_camera(),
        main,
        "rejected call applies no camera"
    );
    assert_eq!(
        world.is_active(rig),
        Some(true),
        "rejected call applies no activity"
    );
}

#[test]
fn hiding_the_old_camera_subtree_is_accepted_after_activating_a_camera_outside_it() {
    let mut world = camera_rig_world("cam_b");
    let main = world.handle("main").unwrap();
    let rig = world.handle("rig").unwrap();
    assert_eq!(world.active_camera(), world.handle("cam_b").unwrap());
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("camera_rig.lua", ACTIVATE_OUTSIDE_THEN_HIDE)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());

    assert!(
        host.drain_events().is_empty(),
        "the call is valid: the camera moves out of the subtree before it is hidden"
    );
    assert_eq!(world.active_camera(), main);
    assert_eq!(world.is_active(rig), Some(false));
}

const ACTIVATE_AND_TAG: &str = r#"
local Script = {}
function Script:on_start()
  rve.find("cam_b"):activate_camera()
  rve.find("rig"):set_tag("Rig")
end
return Script
"#;

#[test]
fn camera_switch_commits_with_edits_that_need_no_world_copy() {
    let mut world = camera_rig_world("main");
    let cam_b = world.handle("cam_b").unwrap();
    let rig = world.handle("rig").unwrap();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("camera_rig.lua", ACTIVATE_AND_TAG)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());

    assert!(host.drain_events().is_empty());
    assert_eq!(world.active_camera(), cam_b);
    assert_eq!(world.object(rig).unwrap().tag, "Rig");
}

/// `kid` под `parent` (позиция 10, масштаб `parent_scale`) и отдельный корень `solo`.
fn pose_world(parent_scale: &str) -> World {
    world(&format!(
        r#"
[[assets]]
path = "/pose.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "parent"
name = "Parent"
[objects.transform]
position = [10.0, 0.0, 0.0]
scale = {parent_scale}
[[objects]]
id = "kid"
parent = "parent"
[[objects.lite_scripts]]
path = "/pose.lua"
[[objects]]
id = "solo"
"#
    ))
}

const POSE_LOCAL: &str = r#"
local Script = {}
function Script:on_start()
  local kid = self.object
  rve.log("kid:" .. tostring(kid:set_world_position({x = 1, y = 2, z = 3})))
  rve.log("solo:" .. tostring(rve.find("solo"):set_world_rotation({x = 0, y = 90, z = 0})))
  rve.log("tag:" .. tostring(kid:set_tag("enemy")))
end
return Script
"#;

#[test]
fn world_pose_and_tag_edits_commit_without_hierarchy_changes() {
    let mut world = pose_world("[2.0, 2.0, 2.0]");
    let kid = world.handle("kid").unwrap();
    let solo = world.handle("solo").unwrap();
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("pose.lua", POSE_LOCAL)]), None).unwrap();
    host.start(&mut world, frame());

    assert_eq!(
        event_messages(&mut host),
        ["kid:true", "solo:true", "tag:true"]
    );
    let position = world.world_position(kid).unwrap();
    assert!(
        position.distance(glam::Vec3::new(1.0, 2.0, 3.0)) < 1e-4,
        "{position:?}"
    );
    let rotation = world.world_rotation_deg(solo).unwrap();
    assert!(
        rotation.distance(glam::Vec3::new(0.0, 90.0, 0.0)) < 1e-3,
        "{rotation:?}"
    );
    assert_eq!(world.tag(kid), Some("enemy"));
}

const POSE_ANCESTOR_THEN_CHILD: &str = r#"
local Script = {}
function Script:on_start()
  local kid = self.object
  rve.log("parent:" .. tostring(rve.find("parent"):set_world_position({x = 5, y = 0, z = 0})))
  rve.log("kid:" .. tostring(kid:set_world_position({x = 1, y = 2, z = 3})))
end
return Script
"#;

#[test]
fn a_child_pose_after_its_ancestor_pose_in_one_call_uses_the_moved_ancestor() {
    let mut world = pose_world("[2.0, 2.0, 2.0]");
    let kid = world.handle("kid").unwrap();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("pose.lua", POSE_ANCESTOR_THEN_CHILD)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());

    assert_eq!(event_messages(&mut host), ["parent:true", "kid:true"]);
    let position = world.world_position(kid).unwrap();
    assert!(
        position.distance(glam::Vec3::new(1.0, 2.0, 3.0)) < 1e-4,
        "{position:?}"
    );
}

#[test]
fn a_pose_under_a_degenerate_parent_rolls_back_the_whole_call_without_touching_the_world() {
    let mut world = pose_world("[0.0, 1.0, 1.0]");
    let kid = world.handle("kid").unwrap();
    let before = world.world_position(kid).unwrap();
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("pose.lua", POSE_LOCAL)]), None).unwrap();
    host.start(&mut world, frame());

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error
                && event.message.contains("вырожден")),
        "{events:?}"
    );
    assert_eq!(world.tag(kid), Some(""));
    assert_eq!(world.world_position(kid), Some(before));
}

const DESTROY_SELF: &str = r#"
local Script = {}
function Script:on_enable() rve.log("enable") end
function Script:on_start() rve.log("start") end
function Script:on_update()
  rve.log("update")
  rve.destroy("prop")
end
function Script:on_disable() rve.log("disable") end
function Script:on_destroy() rve.log("destroy") end
return Script
"#;

#[test]
fn destroying_an_active_instance_runs_on_disable_before_on_destroy() {
    let mut world = life_world(true);
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("life.lua", LIFE)]), None).unwrap();
    host.start(&mut world, frame());
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(event_messages(&mut host), ["enable", "start", "update"]);

    host.destroy(&mut world, frame());
    assert_eq!(event_messages(&mut host), ["disable", "destroy"]);
}

#[test]
fn destroying_an_inactive_instance_runs_only_on_destroy() {
    let mut world = life_world(true);
    let prop = world.handle("prop").unwrap();
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("life.lua", LIFE)]), None).unwrap();
    host.start(&mut world, frame());
    host.update(&mut world, 1.0 / 60.0, frame());
    event_messages(&mut host);

    world.set_active(prop, false).unwrap();
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(event_messages(&mut host), ["disable"]);

    host.destroy(&mut world, frame());
    assert_eq!(event_messages(&mut host), ["destroy"]);
}

#[test]
fn despawning_an_active_object_runs_on_disable_before_on_destroy() {
    let mut world = life_world(true);
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("life.lua", DESTROY_SELF)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());
    host.update(&mut world, 1.0 / 60.0, frame());
    assert_eq!(
        event_messages(&mut host),
        ["enable", "start", "update", "disable", "destroy"]
    );
    assert!(world.handle("prop").is_none());
}

const SPIN_AND_POSE: &str = r#"
local Script = {}
function Script:on_start()
  self.transform.rotation = { x = 0, y = 45, z = 0 }
  rve.log("solo:" .. tostring(rve.find("solo"):set_world_rotation({ x = 0, y = 90, z = 0 })))
  rve.log("tag:" .. tostring(self.object:set_tag("spin")))
end
return Script
"#;

#[test]
fn own_transform_write_with_unrelated_pose_and_tag_commits_together() {
    let mut world = pose_world("[1.0, 1.0, 1.0]");
    let kid = world.handle("kid").unwrap();
    let solo = world.handle("solo").unwrap();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("pose.lua", SPIN_AND_POSE)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());

    assert_eq!(event_messages(&mut host), ["solo:true", "tag:true"]);
    let spin = world.world_rotation_deg(kid).unwrap();
    assert!(
        spin.distance(glam::Vec3::new(0.0, 45.0, 0.0)) < 1e-3,
        "{spin:?}"
    );
    let rotation = world.world_rotation_deg(solo).unwrap();
    assert!(
        rotation.distance(glam::Vec3::new(0.0, 90.0, 0.0)) < 1e-3,
        "{rotation:?}"
    );
    assert_eq!(world.tag(kid), Some("spin"));
}

const STATIC_WRITES: &str = r#"
local Script = {}
function Script:on_start()
  local rock = rve.find("rock")
  local moved = rock:get_transform()
  moved.position = {x = 5, y = 0, z = 0}
  rve.log("transform:" .. tostring(rock:set_transform(moved)))
  rve.log("pos:" .. tostring(rock:set_world_position({x = 1, y = 2, z = 3})))
  rve.log("rot:" .. tostring(rock:set_world_rotation({x = 0, y = 90, z = 0})))
  rve.log("parent:" .. tostring(rock:set_parent(nil, true)))
  rve.log("active:" .. tostring(rock:set_active(false)))
  rve.log("tag:" .. tostring(rock:set_tag("prop")))
  rve.log("self:" .. tostring(self.object:set_world_position({x = 7, y = 0, z = 0})))
end
return Script
"#;

#[test]
fn static_object_rejects_pose_writes_without_rolling_back_other_writes() {
    let mut world = world(
        r#"
[[assets]]
path = "/writer.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "ground"
static = true
[[objects]]
id = "rock"
parent = "ground"
static = true
[[objects]]
id = "writer"
[[objects.lite_scripts]]
path = "/writer.lua"
"#,
    );
    let rock = world.handle("rock").unwrap();
    let writer = world.handle("writer").unwrap();
    let before = world.world_matrix(rock).unwrap();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("writer.lua", STATIC_WRITES)]),
        None,
    )
    .unwrap();
    host.start(&mut world, frame());

    assert_eq!(
        event_messages(&mut host),
        [
            "transform:false",
            "pos:false",
            "rot:false",
            "parent:false",
            "active:true",
            "tag:true",
            "self:true"
        ]
    );
    assert_eq!(world.world_matrix(rock).unwrap(), before);
    assert_eq!(world.tag(rock), Some("prop"));
    assert_eq!(
        world.world_position(writer),
        Some(glam::Vec3::new(7.0, 0.0, 0.0))
    );
}
