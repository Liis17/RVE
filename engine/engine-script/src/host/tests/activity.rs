use super::{event_messages, sources, world};
use crate::ScriptTime;
use crate::host::ScriptHost;
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
    assert_eq!(event_messages(&mut host), ["destroy"]);
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
