use super::ScriptHost;
use crate::{ScriptEventLevel, ScriptTime};
use engine_scene::{AnimationRequest, SceneDocument, World};
use std::collections::BTreeMap;

const SCENE: &str = r#"
[[assets]]
path = "/models/hero.fbx"
[[assets]]
path = "/anim/walk.fbx"
[[assets]]
path = "/hero.lua"
[[assets]]
path = "/silent.lua"
[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "hero"
[objects.mesh_renderer]
model = "/models/hero.fbx"
[objects.animator]
clips = ["/anim/walk.fbx"]
[[objects.lite_scripts]]
path = "/hero.lua"

[[objects]]
id = "silent"
[objects.mesh_renderer]
model = "/models/hero.fbx"
[[objects.lite_scripts]]
path = "/silent.lua"
"#;

fn world() -> World {
    World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap()
}

/// Загружает сцену: скрипт `hero` — `source`, у объекта `silent` скрипт пустой.
fn load(source: &str) -> (World, ScriptHost) {
    load_both(source, "return {}")
}

fn load_both(hero: &str, silent: &str) -> (World, ScriptHost) {
    let world = world();
    let sources = BTreeMap::from([
        ("hero.lua".to_owned(), hero.to_owned()),
        ("silent.lua".to_owned(), silent.to_owned()),
    ]);
    let host = ScriptHost::load(&world, "scene", &sources, None).unwrap();
    (world, host)
}

#[test]
fn animation_requests_are_committed_with_their_options() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("hero"):play_animation("walk", {fade = 0.25, loop = false, ["then"] = "idle"})
  rve.find("hero").set_animation_weight("idle", 0.5)
  rve.find("hero"):set_animation_parameter("speed", 2)
  rve.find("hero"):set_animation_speed(1.5)
  rve.find("hero"):stop_animation()
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let requests = host.drain_animation_requests();
    assert_eq!(requests.len(), 5, "{requests:?}");
    assert_eq!(
        requests[0],
        AnimationRequest::Play {
            object: world.handle("hero").unwrap(),
            motion: "walk".to_owned(),
            fade: 0.25,
            looping: Some(false),
            then: Some("idle".to_owned()),
        }
    );
    assert!(matches!(
        &requests[1],
        AnimationRequest::SetWeight { motion, weight, fade, .. }
            if motion == "idle" && *weight == 0.5 && *fade == 0.0
    ));
    assert!(matches!(
        &requests[2],
        AnimationRequest::SetParameter { name, value, .. } if name == "speed" && *value == 2.0
    ));
    assert!(matches!(requests[3], AnimationRequest::SetSpeed { speed, .. } if speed == 1.5));
    assert!(matches!(requests[4], AnimationRequest::Stop { .. }));
    assert!(host.drain_events().is_empty());
}

#[test]
fn animation_calls_on_objects_without_an_animator_are_lua_errors() {
    let (mut world, mut host) = load_both(
        "return {}",
        r#"
local Script = {}
function Script:on_update()
  local ok = pcall(function() rve.find("silent"):play_animation("walk") end)
  assert(not ok)
  local stopped = pcall(function() rve.find("silent"):stop_animation() end)
  assert(not stopped)
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_animation_requests().is_empty());
    assert!(host.drain_events().is_empty());
}

#[test]
fn invalid_animation_arguments_are_rejected_without_queueing() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  local hero = rve.find("hero")
  local cases = {
    function() hero:play_animation(42) end,
    function() hero:play_animation("walk", {fade = -1}) end,
    function() hero:play_animation("walk", {speed = 1}) end,
    function() hero:play_animation("walk", {loop = "yes"}) end,
    function() hero:play_animation("") end,
    function() hero:set_animation_weight("idle", 0 / 0) end,
    function() hero:set_animation_speed(-1) end,
    function() hero:set_animation_parameter("speed", 1 / 0) end,
  }
  for index, case in ipairs(cases) do
    assert(not pcall(case), "case " .. index .. " must fail")
  end
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_animation_requests().is_empty());
    assert!(host.drain_events().is_empty());
}

#[test]
fn a_failed_script_rolls_back_its_animation_requests() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_update()
  rve.find("hero"):play_animation("walk")
  error("boom")
end
return Script
"#,
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(host.drain_animation_requests().is_empty());
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error),
        "{events:?}"
    );
}

#[test]
fn animation_finished_reaches_the_animated_object_on_the_next_update() {
    let (mut world, mut host) = load(
        r#"
local Script = {}
function Script:on_message(sender, name, value)
  if name == "animation_finished" and sender == "hero" and value == "walk" then
    rve.find("hero"):set_animation_speed(3)
  end
end
return Script
"#,
    );
    let hero = world.handle("hero").unwrap();
    let silent = world.handle("silent").unwrap();
    host.queue_animation_events(&world, [(hero, "walk".to_owned())]);
    assert!(
        host.drain_animation_requests().is_empty(),
        "события доставляются только при update"
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let requests = host.drain_animation_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        matches!(requests[0], AnimationRequest::SetSpeed { object, speed } if object == hero && speed == 3.0)
    );

    host.queue_animation_events(&world, [(silent, "walk".to_owned())]);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(
        host.drain_animation_requests().is_empty(),
        "чужой объект сообщения не получает"
    );
}
