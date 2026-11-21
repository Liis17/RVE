use super::*;
use engine_files::VirtualPath;
use rve_sdk::Value as SdkValue;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const CHAIN: &str = r#"
return {
  migrate = function(from, values)
    if from == 0 then
      values.score = values.points
      values.points = nil
    elseif from == 1 then
      values.level = 1
    end
    values.steps = values.steps or {}
    table.insert(values.steps, from)
    return values
  end,
}
"#;

fn deadline() -> CallDeadline {
    CallDeadline::new(
        Some(Duration::from_secs(5)),
        Arc::new(AtomicBool::new(false)),
    )
}

fn lua_schema(version: u32, source: &str) -> SaveSchema {
    SaveSchema {
        version,
        lua_migration: Some(LuaSaveMigration {
            path: "scripts/save_migration.lua".to_owned(),
            source: source.to_owned(),
        }),
    }
}

fn version_zero() -> SaveData {
    SaveData {
        version: 0,
        values: SaveValues::from([("points".to_owned(), ParamValue::Int(7))]),
    }
}

fn chained() -> SaveValues {
    SaveValues::from([
        ("score".to_owned(), ParamValue::Int(7)),
        ("level".to_owned(), ParamValue::Int(1)),
        (
            "steps".to_owned(),
            ParamValue::Array(vec![ParamValue::Int(0), ParamValue::Int(1)]),
        ),
    ])
}

fn typed_values() -> SaveValues {
    SaveValues::from([
        (
            "icon".to_owned(),
            ParamValue::Asset(VirtualPath::parse("textures/hero.png").unwrap()),
        ),
        ("target".to_owned(), ParamValue::Object("player".to_owned())),
        ("nothing".to_owned(), ParamValue::Object(String::new())),
        (
            "nested".to_owned(),
            ParamValue::Array(vec![
                ParamValue::Object("door".to_owned()),
                ParamValue::Float(0.5),
                ParamValue::String("text".to_owned()),
                ParamValue::Bool(false),
            ]),
        ),
    ])
}

#[test]
fn lua_migration_runs_each_step_from_zero_to_two() {
    let values = migrate_save_data(version_zero(), &lua_schema(2, CHAIN), None, &deadline());
    assert_eq!(values.unwrap(), chained());
}

#[test]
fn lua_migration_keeps_asset_and_object_types() {
    let identity = "return { migrate = function(_, values) return values end }";
    let data = SaveData {
        version: 0,
        values: typed_values(),
    };
    let values = migrate_save_data(data, &lua_schema(1, identity), None, &deadline());
    assert_eq!(values.unwrap(), typed_values());
}

#[test]
fn lua_migration_errors_name_the_chunk_and_the_step() {
    let failing = r#"return { migrate = function(from, values)
        if from == 1 then error("bad layout") end
        return values
    end }"#;
    let error =
        migrate_save_data(version_zero(), &lua_schema(2, failing), None, &deadline()).unwrap_err();
    assert!(error.contains("scripts/save_migration.lua"), "{error}");
    assert!(error.contains("from version 1 to 2"), "{error}");
    assert!(error.contains("bad layout"), "{error}");

    for (source, expected) in [
        ("return 5", "must return a table"),
        ("return {}", "migrate must be a function"),
        (
            "return { migrate = function() return 1 end }",
            "values table",
        ),
        (
            "return { migrate = function() return { [1] = true } end }",
            "save keys must be strings",
        ),
        (
            "return { migrate = function() return { bad = 0/0 } end }",
            "finite",
        ),
    ] {
        let error = migrate_save_data(version_zero(), &lua_schema(1, source), None, &deadline())
            .unwrap_err();
        assert!(error.contains(expected), "{source}: {error}");
    }
}

#[test]
fn stuck_lua_migration_stops_at_the_call_deadline() {
    let stuck = "return { migrate = function() while true do end end }";
    let deadline = CallDeadline::new(
        Some(Duration::from_millis(20)),
        Arc::new(AtomicBool::new(false)),
    );
    let error =
        migrate_save_data(version_zero(), &lua_schema(1, stuck), None, &deadline).unwrap_err();
    assert!(error.contains("time limit"), "{error}");
}

unsafe extern "C" fn rust_chain(migration: *const RveSaveMigration) -> u32 {
    unsafe {
        rve_sdk::run_save_migration(migration, |from, mut values| {
            match from {
                0 => {
                    let points = values.remove("points").ok_or("points is missing")?;
                    values.insert("score".to_owned(), points);
                }
                1 => {
                    values.insert("level".to_owned(), SdkValue::Int(1));
                }
                _ => return Err(format!("unexpected version {from}")),
            }
            let mut steps = match values.remove("steps") {
                Some(SdkValue::Array(steps)) => steps,
                _ => Vec::new(),
            };
            steps.push(SdkValue::Int(i64::from(from)));
            values.insert("steps".to_owned(), SdkValue::Array(steps));
            Ok(values)
        })
    }
}

unsafe extern "C" fn rust_identity(migration: *const RveSaveMigration) -> u32 {
    unsafe { rve_sdk::run_save_migration(migration, |_, values| Ok(values)) }
}

unsafe extern "C" fn rust_failure(migration: *const RveSaveMigration) -> u32 {
    unsafe { rve_sdk::run_save_migration(migration, |_, _| Err("cannot migrate".to_owned())) }
}

#[test]
fn rust_migration_runs_each_step_and_keeps_typed_values() {
    let schema = SaveSchema {
        version: 2,
        lua_migration: None,
    };
    let values = migrate_save_data(version_zero(), &schema, Some(rust_chain), &deadline());
    assert_eq!(values.unwrap(), chained());

    let data = SaveData {
        version: 1,
        values: typed_values(),
    };
    let values = migrate_save_data(data, &schema, Some(rust_identity), &deadline());
    assert_eq!(values.unwrap(), typed_values());

    let error =
        migrate_save_data(version_zero(), &schema, Some(rust_failure), &deadline()).unwrap_err();
    assert!(error.contains("from version 0 to 1"), "{error}");
    assert!(error.contains("cannot migrate"), "{error}");
}

#[test]
fn versions_and_declarations_are_checked_before_migrating() {
    let newer = SaveData {
        version: 3,
        values: SaveValues::new(),
    };
    let error = migrate_save_data(newer, &lua_schema(2, CHAIN), None, &deadline()).unwrap_err();
    assert!(
        error.contains("version 3 is newer than save version 2"),
        "{error}"
    );

    let plain = SaveSchema {
        version: 1,
        lua_migration: None,
    };
    let error = migrate_save_data(version_zero(), &plain, None, &deadline()).unwrap_err();
    assert!(error.contains("declares no save migration"), "{error}");

    let error = migrate_save_data(
        version_zero(),
        &lua_schema(1, CHAIN),
        Some(rust_identity),
        &deadline(),
    )
    .unwrap_err();
    assert!(error.contains("declared twice"), "{error}");

    let current = SaveData {
        version: 1,
        values: chained(),
    };
    assert_eq!(
        migrate_save_data(current, &plain, None, &deadline()).unwrap(),
        chained()
    );
}
