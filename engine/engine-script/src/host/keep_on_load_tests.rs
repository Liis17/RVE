use super::*;
use engine_scene::{SceneDocument, World};
use std::collections::BTreeMap;

fn persistent_world() -> World {
    let document = SceneDocument::parse(
        "keep.rvescene",
        "[[assets]]\npath = \"/keep.lua\"\n[[objects]]\nid = \"runner\"\n[[objects.lite_scripts]]\npath = \"/keep.lua\"\n[[objects]]\nid = \"child\"\nparent = \"runner\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
    )
    .unwrap();
    World::from_document(&document).unwrap()
}

fn load(world: &World, script: &str) -> ScriptHost {
    ScriptHost::load(
        world,
        "keep",
        &BTreeMap::from([("keep.lua".to_owned(), script.to_owned())]),
        None,
    )
    .unwrap()
}

#[test]
fn lua_keep_on_load_is_readable_before_commit_and_only_changes_roots() {
    let script = r#"
local Script = {}
function Script:on_update()
  local root = rve.find("runner")
  assert(root:keep_on_load() == false)
  assert(root:set_keep_on_load(true))
  assert(root:keep_on_load() == true)
  local child = rve.find("child")
  assert(child:set_keep_on_load(true) == false)
  assert(child:keep_on_load() == false)
end
return Script
"#;
    let mut world = persistent_world();
    let mut host = load(&world, script);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let events = host.drain_events();
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(
        world.keep_on_load(world.handle("runner").unwrap()),
        Some(true)
    );
    assert_eq!(
        world.keep_on_load(world.handle("child").unwrap()),
        Some(false)
    );
}

#[test]
fn failed_lua_callback_rolls_back_keep_on_load() {
    let script = r#"
local Script = {}
function Script:on_update()
  assert(rve.find("runner"):set_keep_on_load(true))
  error("rollback keep flag")
end
return Script
"#;
    let mut world = persistent_world();
    let mut host = load(&world, script);

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    assert_eq!(
        world.keep_on_load(world.handle("runner").unwrap()),
        Some(false)
    );
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error)
    );
}
