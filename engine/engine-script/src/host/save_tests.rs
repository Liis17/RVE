use super::*;
use engine_scene::ParamValue;
use engine_scene::{SceneDocument, World};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

const SAVE_SCRIPT: &str = r#"
local Script = {}
function Script:on_update()
  rve.save.set("score", 12)
  assert(rve.save.get("score") == 12)
  assert(rve.save.has("score"))
  assert(rve.save.keys()[1] == "score")
  rve.save.flush()
end
return Script
"#;

fn save_world() -> World {
    let document = SceneDocument::parse(
        "save.rvescene",
        "[[assets]]\npath = \"/save.lua\"\n[[objects]]\nid = \"runner\"\n[[objects.lite_scripts]]\npath = \"/save.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
    )
    .unwrap();
    World::from_document(&document).unwrap()
}

fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
        .collect()
}

#[test]
fn lua_save_calls_commit_together_and_flush_only_after_success() {
    let mut world = save_world();
    let mut host =
        ScriptHost::load(&world, "save", &sources(&[("save.lua", SAVE_SCRIPT)]), None).unwrap();
    let directory = temp_dir();
    let path = directory.join(".rve").join("playersave").join("save.toml");
    host.load_save_file(&path).unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());

    assert_eq!(host.save_values().get("score"), Some(&ParamValue::Int(12)));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        crate::decode_save_data(&stored).unwrap().get("score"),
        Some(&ParamValue::Int(12))
    );
    host.destroy(&mut world, ScriptTime::default());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_callback_rolls_back_save_values_and_flush_request() {
    let script = r#"
local Script = {}
function Script:on_update()
  rve.save.set("partial", true)
  rve.save.flush()
  error("rollback")
end
return Script
"#;
    let mut world = save_world();
    let mut host =
        ScriptHost::load(&world, "save", &sources(&[("save.lua", script)]), None).unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.save_values().is_empty());
    assert!(!host.take_save_flush_requested());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error)
    );
}

#[test]
fn lua_save_enforces_key_and_value_limits_without_mutating_state() {
    let script = r#"
local Script = {}
function Script:on_update()
  assert(not pcall(function() rve.save.set("bad\nkey", 1) end))
  assert(not pcall(function() rve.save.set("bad", 0/0) end))
  assert(not rve.save.has("bad"))
end
return Script
"#;
    let mut world = save_world();
    let mut host =
        ScriptHost::load(&world, "save", &sources(&[("save.lua", script)]), None).unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.save_values().is_empty());
    assert!(
        !host
            .drain_events()
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error)
    );
}

fn temp_dir() -> PathBuf {
    let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rve-script-save-test-{}-{id}", std::process::id()))
}
