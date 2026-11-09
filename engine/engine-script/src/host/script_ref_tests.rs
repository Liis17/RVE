use super::*;
use engine_scene::{SceneDocument, World};
use std::collections::BTreeMap;

const TARGET: &str = "/target.lua";

const TARGET_SOURCE: &str = r#"
local Script = {}
Script.properties = {
  speed = { type = "float", default = 1.5, min = 0, max = 10 },
  count = { type = "int", default = 2 },
}
function Script:on_update(dt)
  rve.log("target update " .. string.format("%g", self.params.speed))
end
function Script:on_message(sender, name, value)
  rve.log("target got " .. name)
end
return Script
"#;

const OTHER_SOURCE: &str = r#"
local Script = {}
function Script:on_message(sender, name, value)
  rve.log("other got " .. name)
end
return Script
"#;

fn world() -> World {
    let document = SceneDocument::parse(
        "refs.rvescene",
        concat!(
            "[[assets]]\npath = \"/driver.lua\"\n",
            "[[assets]]\npath = \"/target.lua\"\n",
            "[[assets]]\npath = \"/other.lua\"\n",
            "[[objects]]\nid = \"driver\"\n[[objects.lite_scripts]]\npath = \"/driver.lua\"\n",
            "[[objects]]\nid = \"target\"\n",
            "[[objects.lite_scripts]]\npath = \"/target.lua\"\n",
            "[[objects.lite_scripts]]\npath = \"/other.lua\"\n",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        ),
    )
    .unwrap();
    World::from_document(&document).unwrap()
}

fn load(world: &World, driver: &str) -> ScriptHost {
    let sources = BTreeMap::from([
        ("driver.lua".to_owned(), driver.to_owned()),
        ("target.lua".to_owned(), TARGET_SOURCE.to_owned()),
        ("other.lua".to_owned(), OTHER_SOURCE.to_owned()),
    ]);
    ScriptHost::load(world, "refs", &sources, None).unwrap()
}

/// Сообщения журнала; ошибка скрипта здесь всегда провал теста.
fn logs(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| {
            assert_ne!(
                event.level,
                ScriptEventLevel::Error,
                "unexpected error: {}",
                event.message
            );
            event.message
        })
        .collect()
}

fn errors(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Error)
        .map(|event| event.message)
        .collect()
}

#[test]
fn script_ref_reads_declared_properties_and_rejects_bad_writes() {
    let driver = format!(
        r#"
local Script = {{}}
function Script:on_update()
  local s = rve.find("target"):script("{TARGET}")
  assert(s ~= nil)
  assert(s:get("speed") == 1.5)
  assert(s:get("count") == 2)
  assert(rve.find("target"):script("/missing.lua") == nil)
  s:set("speed", 7)
  assert(s:get("speed") == 7)
  assert(not pcall(function() s:set("nope", 1) end))
  assert(not pcall(function() s:set("speed", 99) end))
  assert(not pcall(function() s:set("count", 1.5) end))
  assert(s:get("speed") == 7)
  assert(s:get("count") == 2)
  rve.log("driver ok")
end
return Script
"#
    );
    let mut world = world();
    let mut host = load(&world, &driver);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert!(logs(&mut host).contains(&"driver ok".to_owned()));
}

#[test]
fn failed_callback_discards_cross_instance_property_writes() {
    let driver = format!(
        r#"
local Script = {{}}
local calls = 0
function Script:on_update()
  calls = calls + 1
  if calls == 1 then
    rve.find("target"):script("{TARGET}"):set("speed", 8)
    error("rollback property write")
  end
end
return Script
"#
    );
    let mut world = world();
    let mut host = load(&world, &driver);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(
        errors(&mut host)
            .iter()
            .any(|message| message.contains("rollback property write"))
    );
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    // Target keeps its declared value: the failed driver call committed nothing.
    let read = logs(&mut host);
    assert!(read.contains(&"target update 1.5".to_owned()), "{read:?}");
    assert!(
        !read
            .iter()
            .any(|message| message.contains("target update 8")),
        "{read:?}"
    );
}

#[test]
fn cross_instance_write_reaches_owner_params_on_its_next_call() {
    let driver = format!(
        r#"
local Script = {{}}
local calls = 0
function Script:on_update()
  calls = calls + 1
  if calls == 1 then
    rve.find("target"):script("{TARGET}"):set("speed", 3)
  end
end
return Script
"#
    );
    let mut world = world();
    let mut host = load(&world, &driver);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    logs(&mut host);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert!(logs(&mut host).contains(&"target update 3".to_owned()));
}

#[test]
fn addressed_message_reaches_only_the_chosen_instance() {
    let driver = r#"
local Script = {}
local sent = false
function Script:on_update()
  if not sent then
    sent = true
    rve.find("target"):script("/other.lua"):send("ping", 1)
  end
end
return Script
"#;
    let mut world = world();
    let mut host = load(&world, driver);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    logs(&mut host);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let delivered = logs(&mut host);
    assert!(
        delivered.contains(&"other got ping".to_owned()),
        "{delivered:?}"
    );
    assert!(
        !delivered.contains(&"target got ping".to_owned()),
        "{delivered:?}"
    );
}

#[test]
fn enabled_setter_takes_effect_at_the_phase_boundary() {
    let driver = format!(
        r#"
local Script = {{}}
local calls = 0
function Script:on_update()
  calls = calls + 1
  local s = rve.find("target"):script("{TARGET}")
  if calls == 1 then
    s:set_enabled(false)
    assert(s:enabled() == false)
  end
end
return Script
"#
    );
    let mut world = world();
    let mut host = load(&world, &driver);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    logs(&mut host);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let after = logs(&mut host);
    assert!(
        !after
            .iter()
            .any(|message| message.starts_with("target update")),
        "{after:?}"
    );
}

#[test]
fn stale_script_handle_after_destroy_reports_a_defined_error() {
    let driver = format!(
        r#"
local Script = {{}}
local calls = 0
local keep
function Script:on_update()
  calls = calls + 1
  if calls == 1 then
    keep = rve.find("target"):script("{TARGET}")
  elseif calls == 2 then
    rve.destroy("target")
  else
    local ok, err = pcall(function() return keep:get("speed") end)
    rve.log("stale " .. tostring(ok) .. " " .. tostring(err))
  end
end
return Script
"#
    );
    let mut world = world();
    let mut host = load(&world, &driver);

    for _ in 0..3 {
        host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    }

    let stale = logs(&mut host)
        .into_iter()
        .find(|message| message.starts_with("stale "))
        .expect("stale handle was read");
    assert!(stale.starts_with("stale false"), "{stale}");
    assert!(
        stale.contains("script instance is no longer alive"),
        "{stale}"
    );
}

#[test]
fn cross_write_does_not_revert_a_property_the_owner_wrote_itself() {
    const SELF_WRITER: &str = r#"
local Script = {}
Script.properties = {
  speed = { type = "float", default = 1.5, min = 0, max = 10 },
  count = { type = "int", default = 2 },
}
local frame = 0
function Script:on_update(dt)
  frame = frame + 1
  if frame == 1 then
    self.params.speed = 9
  end
  rve.log("target " .. string.format("%g", self.params.speed) .. " " .. string.format("%g", self.params.count))
end
return Script
"#;
    let driver = format!(
        r#"
local Script = {{}}
local frames = 0
function Script:on_update()
  frames = frames + 1
  if frames == 2 then
    rve.find("target"):script("{TARGET}"):set("count", 5)
  end
end
return Script
"#
    );
    let mut world = world();
    let sources = BTreeMap::from([
        ("driver.lua".to_owned(), driver),
        ("target.lua".to_owned(), SELF_WRITER.to_owned()),
        ("other.lua".to_owned(), OTHER_SOURCE.to_owned()),
    ]);
    let mut host = ScriptHost::load(&world, "refs", &sources, None).unwrap();
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let second = logs(&mut host);
    assert!(second.contains(&"target 9 5".to_owned()), "{second:?}");
}
