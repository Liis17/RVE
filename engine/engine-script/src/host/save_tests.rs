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
    host.load_save_file(&path, &SaveSchema::default()).unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());

    assert_eq!(host.save_values().get("score"), Some(&ParamValue::Int(12)));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        crate::decode_save_data(&stored)
            .unwrap()
            .values
            .get("score"),
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
fn failed_background_save_retries_same_generation_and_reports_recovery() {
    let script = r#"
local Script = {}
local did_set = false
function Script:on_update()
  if not did_set then
    rve.save.set("score", 7)
    did_set = true
  end
end
return Script
"#;
    let mut world = save_world();
    let mut host =
        ScriptHost::load(&world, "save", &sources(&[("save.lua", script)]), None).unwrap();
    let directory = temp_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let parent = directory.join("blocked");
    std::fs::create_dir(&parent).unwrap();
    let path = parent.join("save.toml");
    host.load_save_file(&path, &SaveSchema::default()).unwrap();
    std::fs::remove_dir(&parent).unwrap();
    std::fs::write(&parent, b"leave this file intact").unwrap();

    host.update(&mut world, 0.1, ScriptTime::default());
    let generation = host.save_generation();
    assert_eq!(generation, 1);
    host.save_flush_requested.set(true);
    host.persist_save_changes();
    assert_eq!(host.persisted_save_generation, 0);
    let failure_events = host.drain_events();
    assert_eq!(
        failure_events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1,
        "{failure_events:?}"
    );

    std::fs::remove_file(&parent).unwrap();
    std::fs::create_dir(&parent).unwrap();
    host.persist_save_changes();
    let values = host.save_values();
    host.save_writer
        .as_ref()
        .unwrap()
        .flush(generation, values.clone())
        .unwrap();
    host.process_save_acknowledgements();
    assert_eq!(host.save_generation(), generation);
    assert_eq!(host.persisted_save_generation, generation);
    assert_eq!(host.save_values(), values);
    let recovery_events = host.drain_events();
    assert!(
        recovery_events.iter().any(|event| {
            event.level == ScriptEventLevel::Info && event.message == "save persistence recovered"
        }),
        "{recovery_events:?}"
    );

    host.destroy(&mut world, ScriptTime::default());
    assert_eq!(
        crate::decode_save_data(&std::fs::read_to_string(&path).unwrap())
            .unwrap()
            .values,
        values
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn invalid_save_file_is_reported_and_never_overwritten_by_memory_fallback() {
    let mut world = save_world();
    let mut host =
        ScriptHost::load(&world, "save", &sources(&[("save.lua", SAVE_SCRIPT)]), None).unwrap();
    let directory = temp_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("save.toml");
    let corrupt = "values = 7\n";
    std::fs::write(&path, corrupt).unwrap();

    assert!(host.load_save_file(&path, &SaveSchema::default()).is_err());
    let diagnostic = host.drain_events();
    assert!(
        diagnostic.iter().any(|event| {
            event.level == ScriptEventLevel::Error
                && event.message.contains("using in-memory data")
                && event.message.contains("invalid save file")
        }),
        "{diagnostic:?}"
    );

    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
    std::fs::remove_dir_all(directory).unwrap();
}

const MIGRATED_SCRIPT: &str = r#"
local Script = {}
function Script:on_update()
  assert(rve.save.get("score") == 7)
  assert(not rve.save.has("points"))
  rve.save.set("seen", true)
  rve.save.flush()
end
return Script
"#;

const LUA_MIGRATION: &str = r#"
return {
  migrate = function(from, values)
    if from == 0 then
      values.score = values.points
      values.points = nil
    end
    return values
  end,
}
"#;

fn lua_schema(version: u32, source: &str) -> SaveSchema {
    SaveSchema {
        version,
        lua_migration: Some(crate::LuaSaveMigration {
            path: "scripts/save_migration.lua".to_owned(),
            source: source.to_owned(),
        }),
    }
}

fn assert_no_errors(host: &mut ScriptHost) {
    let events = host.drain_events();
    assert!(
        !events
            .iter()
            .any(|event| event.level == ScriptEventLevel::Error),
        "{events:?}"
    );
}

#[test]
fn unversioned_save_is_migrated_by_lua_before_scripts_and_rewritten_with_new_version() {
    let mut world = save_world();
    let mut host = ScriptHost::load(
        &world,
        "save",
        &sources(&[("save.lua", MIGRATED_SCRIPT)]),
        None,
    )
    .unwrap();
    let directory = temp_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("save.toml");
    std::fs::write(&path, "[values]\npoints = 7\n").unwrap();

    host.load_save_file(&path, &lua_schema(2, LUA_MIGRATION))
        .unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert_no_errors(&mut host);
    host.destroy(&mut world, ScriptTime::default());

    let stored = crate::decode_save_data(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(stored.version, 2);
    assert_eq!(
        stored.values,
        BTreeMap::from([
            ("score".to_owned(), ParamValue::Int(7)),
            ("seen".to_owned(), ParamValue::Bool(true)),
        ])
    );
    std::fs::remove_dir_all(directory).unwrap();
}

unsafe extern "C" fn rename_points(migration: *const rve_sdk::abi::RveSaveMigration) -> u32 {
    unsafe {
        rve_sdk::run_save_migration(migration, |_, mut values| {
            let points = values.remove("points").ok_or("points is missing")?;
            values.insert("score".to_owned(), points);
            Ok(values)
        })
    }
}

static EMPTY_MODULE: rve_sdk::abi::RveModule = rve_sdk::abi::RveModule::new(&[]);

#[test]
fn rust_library_migration_runs_before_scripts_see_save_data() {
    // SAFETY: the static module and in-process migration live for the whole process.
    let library = unsafe { crate::ScriptLibrary::from_module(&EMPTY_MODULE) }.unwrap();
    let library = unsafe { library.with_save_migration(rename_points) };
    let mut world = save_world();
    let mut host = ScriptHost::load(
        &world,
        "save",
        &sources(&[("save.lua", MIGRATED_SCRIPT)]),
        Some(&library),
    )
    .unwrap();
    let directory = temp_dir();
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("save.toml");
    std::fs::write(&path, "version = 0\n\n[values]\npoints = 7\n").unwrap();
    let schema = SaveSchema {
        version: 1,
        lua_migration: None,
    };

    host.load_save_file(&path, &schema).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert_no_errors(&mut host);
    host.destroy(&mut world, ScriptTime::default());

    let stored = crate::decode_save_data(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.values.get("score"), Some(&ParamValue::Int(7)));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn newer_or_unmigratable_save_is_reported_and_left_unchanged() {
    let failing = "return { migrate = function() error('broken migrator') end }";
    for (contents, schema, expected) in [
        (
            "version = 5\n\n[values]\nscore = 1\n",
            lua_schema(2, LUA_MIGRATION),
            "version 5 is newer than save version 2",
        ),
        (
            "[values]\npoints = 1\n",
            lua_schema(1, failing),
            "broken migrator",
        ),
        (
            "[values]\npoints = 1\n",
            SaveSchema {
                version: 1,
                lua_migration: None,
            },
            "declares no save migration",
        ),
    ] {
        let mut world = save_world();
        let mut host =
            ScriptHost::load(&world, "save", &sources(&[("save.lua", SAVE_SCRIPT)]), None).unwrap();
        let directory = temp_dir();
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("save.toml");
        std::fs::write(&path, contents).unwrap();

        let error = host.load_save_file(&path, &schema).unwrap_err();
        assert!(error.contains(expected), "{error}");
        let diagnostic = host.drain_events();
        assert!(
            diagnostic.iter().any(|event| {
                event.level == ScriptEventLevel::Error
                    && event.message.contains("using in-memory data")
                    && event.message.contains(expected)
            }),
            "{diagnostic:?}"
        );

        host.update(&mut world, 0.1, ScriptTime::default());
        host.destroy(&mut world, ScriptTime::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn missing_save_file_starts_empty_at_the_game_version_without_migration() {
    let mut world = save_world();
    let mut host =
        ScriptHost::load(&world, "save", &sources(&[("save.lua", SAVE_SCRIPT)]), None).unwrap();
    let directory = temp_dir();
    let path = directory.join("save.toml");
    let schema = SaveSchema {
        version: 3,
        lua_migration: None,
    };

    host.load_save_file(&path, &schema).unwrap();
    assert!(host.save_values().is_empty());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let stored = crate::decode_save_data(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(stored.version, 3);
    assert_eq!(stored.values.get("score"), Some(&ParamValue::Int(12)));
    std::fs::remove_dir_all(directory).unwrap();
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
