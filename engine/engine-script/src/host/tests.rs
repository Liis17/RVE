use super::{ScriptHost, ScriptInterruptHandle, ScriptLoadError};
use crate::{
    HeavyRegistrationError, HeavyRegistry, HeavyScript, ScriptContext, ScriptEventLevel,
    ScriptParams, ScriptTime,
};
use engine_scene::{ParamValue, SceneDocument, World};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn world(source: &str) -> World {
    let document = SceneDocument::parse("test.rvescene", source).unwrap();
    World::from_document(&document).unwrap()
}

fn one_camera(body: &str) -> World {
    world(&format!(
        r#"
[[objects]]
id = "camera"
name = "Camera object"
[objects.camera]
fov_deg = 60.0
near = 0.1
far = 100.0
{body}
"#
    ))
}

fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
        .collect()
}

fn labeled_registry() -> HeavyRegistry {
    let mut registry = HeavyRegistry::new();
    registry.register("Log", labeled_factory).unwrap();
    registry
}

fn empty_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
    Ok(Box::new(KeepAliveHeavy))
}

#[test]
fn heavy_registry_rejects_duplicate_and_invalid_names() {
    let mut registry = HeavyRegistry::new();
    registry.register("Valid_2", empty_factory).unwrap();
    registry.register("x".repeat(64), empty_factory).unwrap();
    assert_eq!(
        registry.register("Valid_2", empty_factory),
        Err(HeavyRegistrationError::DuplicateName("Valid_2".to_owned()))
    );
    for name in ["", "bad-name", "é", &"x".repeat(65)] {
        assert!(matches!(
            registry.register(name, empty_factory),
            Err(HeavyRegistrationError::InvalidName(_))
        ));
    }
}

struct LabeledHeavy {
    label: String,
}

fn labeled_factory(params: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
    let Some(ParamValue::String(label)) = params.get("label") else {
        return Err("missing string parameter label".to_owned());
    };
    Ok(Box::new(LabeledHeavy {
        label: label.clone(),
    }))
}

impl HeavyScript for LabeledHeavy {
    fn on_start(&mut self, ctx: &mut ScriptContext<'_>) {
        ctx.log(format!("{}:start", self.label));
    }

    fn on_update(&mut self, ctx: &mut ScriptContext<'_>, _dt: f32) {
        ctx.log(format!("{}:update", self.label));
    }

    fn on_destroy(&mut self, ctx: &mut ScriptContext<'_>) {
        ctx.log(format!("{}:destroy", self.label));
    }
}

fn event_messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect()
}

#[test]
fn lifecycle_order_is_stable_and_start_runs_once() {
    let mut world = world(
        r#"
[[assets]]
path = "/a.lua"
[[assets]]
path = "/b.lua"
[[objects]]
id = "camera"
[objects.camera]
fov_deg = 60.0
near = 0.1
far = 100.0
[[objects.lite_scripts]]
path = "/a.lua"
params = { label = "camera-lite" }
[[objects.heavy_scripts]]
name = "Log"
params = { label = "camera-heavy" }
[[objects]]
id = "other"
[[objects.lite_scripts]]
path = "/b.lua"
params = { label = "other-lite" }
[[objects.heavy_scripts]]
name = "Log"
params = { label = "other-heavy" }
"#,
    );
    let lite_source = r#"
local Script = {}
function Script:on_start() rve.log(self.params.label .. ":start") end
function Script:on_update() rve.log(self.params.label .. ":update") end
function Script:on_destroy() rve.log(self.params.label .. ":destroy") end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", lite_source), ("b.lua", lite_source)]),
        &labeled_registry(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.start(&mut world, ScriptTime::default());
    assert_eq!(
        event_messages(&mut host),
        [
            "camera-lite:start",
            "camera-heavy:start",
            "other-lite:start",
            "other-heavy:start"
        ]
    );

    host.update(
        &mut world,
        0.25,
        ScriptTime {
            elapsed: 0.25,
            frame: 1,
        },
    );
    host.update(
        &mut world,
        0.5,
        ScriptTime {
            elapsed: 0.75,
            frame: 2,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        [
            "camera-lite:update",
            "camera-heavy:update",
            "other-lite:update",
            "other-heavy:update",
            "camera-lite:update",
            "camera-heavy:update",
            "other-lite:update",
            "other-heavy:update"
        ]
    );

    host.destroy(&mut world, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());
    assert_eq!(
        event_messages(&mut host),
        [
            "camera-lite:destroy",
            "camera-heavy:destroy",
            "other-lite:destroy",
            "other-heavy:destroy"
        ]
    );
}

#[test]
fn first_update_calls_start_before_update_when_start_was_not_explicitly_requested() {
    let source = r#"
local Script = {}
function Script:on_start() rve.log("start") end
function Script:on_update() rve.log("update") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/auto-start.lua\"\n[[objects.lite_scripts]]\npath=\"/auto-start.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("auto-start.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert_eq!(event_messages(&mut host), ["start", "update"]);
}

struct ParamsAndTransformHeavy;

fn params_factory(params: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
    let expected = BTreeMap::from([
        ("flag", ParamValue::Bool(true)),
        ("count", ParamValue::Int(17)),
        ("ratio", ParamValue::Float(1.25)),
        ("title", ParamValue::String("hello".to_owned())),
        (
            "values",
            ParamValue::Array(vec![
                ParamValue::Int(2),
                ParamValue::Bool(false),
                ParamValue::String("x".to_owned()),
            ]),
        ),
    ]);
    if expected
        .iter()
        .all(|(key, value)| params.get(*key) == Some(value))
    {
        Ok(Box::new(ParamsAndTransformHeavy))
    } else {
        Err("parameter values differ".to_owned())
    }
}

impl HeavyScript for ParamsAndTransformHeavy {
    fn on_update(&mut self, ctx: &mut ScriptContext<'_>, _dt: f32) {
        ctx.transform.position[0] += 1.0;
    }
}

#[test]
fn lua_and_heavy_receive_all_param_types_and_write_transform_to_world() {
    let params =
        r#"{ flag = true, count = 17, ratio = 1.25, title = "hello", values = [2, false, "x"] }"#;
    let source = r#"
local Script = {}
function Script:on_start()
  local p = self.params
  assert(p.flag == true and p.count == 17 and p.ratio == 1.25 and p.title == "hello")
  assert(p.values[1] == 2 and p.values[2] == false and p.values[3] == "x")
end
function Script:on_update()
  self.transform.position = { x = 10, y = 20, z = 30 }
  self.transform.rotation = { x = 0, y = 90, z = 0 }
  self.transform.scale = { x = 2, y = 3, z = 4 }
end
return Script
"#;
    let mut world = one_camera(&format!(
        "[[assets]]\npath = \"/params.lua\"\n[[objects.lite_scripts]]\npath = \"/params.lua\"\nparams = {params}\n[[objects.heavy_scripts]]\nname = \"Params\"\nparams = {params}\n"
    ));
    let mut registry = HeavyRegistry::new();
    registry.register("Params", params_factory).unwrap();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("params.lua", source)]),
        &registry,
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());

    let transform = world
        .local_transform(world.handle("camera").unwrap())
        .unwrap();
    assert_eq!(transform.position.to_array(), [11.0, 20.0, 30.0]);
    assert_eq!(transform.rotation_deg.to_array(), [0.0, 90.0, 0.0]);
    assert_eq!(transform.scale.to_array(), [2.0, 3.0, 4.0]);
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_time_print_and_rve_log_are_buffered_as_info_events() {
    let source = r#"
local Script = {}
function Script:on_start()
  assert(rve.time.elapsed == 12.5 and rve.time.frame == 7)
  print("hello", 123)
  rve.log("elapsed", rve.time.elapsed, "frame", rve.time.frame)
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/log.lua\"\n[[objects.lite_scripts]]\npath = \"/log.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "level",
        &sources(&[("log.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();
    host.start(
        &mut world,
        ScriptTime {
            elapsed: 12.5,
            frame: 7,
        },
    );
    let events = host.drain_events();
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
    assert_eq!(events[0].message, "hello 123");
    assert_eq!(events[1].message, "elapsed 12.5 frame 7");
    assert_eq!(events[0].source, "level/camera/log.lua");
}

#[test]
fn lua_logging_lossily_converts_non_utf8_strings() {
    let source = r#"
local Script = {}
function Script:on_start()
  print("print-\xff")
  rve.log("log-\xff")
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/bytes.lua\"\n[[objects.lite_scripts]]\npath = \"/bytes.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("bytes.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].message, "print-�");
    assert_eq!(events[1].message, "log-�");
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
}

#[test]
fn lua_files_have_separate_global_environments() {
    let mut world = one_camera(
        "[[assets]]\npath = \"/a.lua\"\n[[assets]]\npath = \"/b.lua\"\n[[objects.lite_scripts]]\npath = \"/a.lua\"\n[[objects.lite_scripts]]\npath = \"/b.lua\"\n",
    );
    let a = r#"
rve = nil
tostring = nil
foo = 1
return {}
"#;
    let b = r#"
local Script = {}
function Script:on_start()
  assert(rve.time and type(tostring) == "function")
  assert(foo == nil)
  rve.log("second file is isolated")
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", a), ("b.lua", b)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].message, "second file is isolated");
    assert_eq!(events[0].level, ScriptEventLevel::Info);
}

#[test]
fn lua_time_above_signed_integer_range_is_passed_as_float() {
    let source = r#"
local Script = {}
function Script:on_start()
  assert(math.type(rve.time.frame) == "float")
  rve.log("large frame")
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/frame.lua\"\n[[objects.lite_scripts]]\npath = \"/frame.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("frame.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(
        &mut world,
        ScriptTime {
            elapsed: 0.0,
            frame: i64::MAX as u64 + 1,
        },
    );

    assert_eq!(event_messages(&mut host), ["large frame"]);
}

#[test]
fn lua_runtime_error_disables_only_its_instance_and_skips_destroy() {
    let bad = r#"
local Script = {}
function Script:on_update() error("broken callback") end
function Script:on_destroy() rve.log("bad destroyed") end
return Script
"#;
    let healthy = r#"
local Script = {}
function Script:on_update()
  local p = self.transform.position
  self.transform.position = { x = p.x + 1, y = p.y, z = p.z }
  rve.log("healthy update")
end
function Script:on_destroy() rve.log("healthy destroyed") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/bad.lua\"\n[[assets]]\npath=\"/healthy.lua\"\n[[objects.lite_scripts]]\npath=\"/bad.lua\"\n[[objects.lite_scripts]]\npath=\"/healthy.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("bad.lua", bad), ("healthy.lua", healthy)]),
        &HeavyRegistry::new(),
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1
    );
    assert!(events[0].source.ends_with("camera/bad.lua"));
    assert!(events[0].message.contains("broken callback"));
    let messages = events
        .iter()
        .map(|event| event.message.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        [
            events[0].message.as_str(),
            "healthy update",
            "healthy update",
            "healthy destroyed"
        ]
    );
    assert!(!messages.contains(&"bad destroyed"));
    let transform = world
        .local_transform(world.handle("camera").unwrap())
        .unwrap();
    assert_eq!(transform.position.x, 2.0);
}

struct PanicHeavy;

fn panic_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
    Ok(Box::new(PanicHeavy))
}

impl HeavyScript for PanicHeavy {
    fn on_update(&mut self, ctx: &mut ScriptContext<'_>, _: f32) {
        ctx.transform.position[0] = 99.0;
        panic!("expected heavy panic");
    }

    fn on_destroy(&mut self, ctx: &mut ScriptContext<'_>) {
        ctx.log("panic instance destroyed");
    }
}

struct KeepAliveHeavy;

fn keep_alive_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
    Ok(Box::new(KeepAliveHeavy))
}

impl HeavyScript for KeepAliveHeavy {
    fn on_update(&mut self, ctx: &mut ScriptContext<'_>, _: f32) {
        ctx.transform.position[1] += 1.0;
    }

    fn on_destroy(&mut self, ctx: &mut ScriptContext<'_>) {
        ctx.log("healthy heavy destroyed");
    }
}

#[test]
fn heavy_panic_disables_that_instance_but_other_heavy_scripts_continue() {
    let mut world = one_camera(
        "[objects.transform]\nposition=[8.0, 0.0, 0.0]\n[[objects.heavy_scripts]]\nname=\"Panic\"\n[[objects.heavy_scripts]]\nname=\"KeepAlive\"\n",
    );
    let mut registry = HeavyRegistry::new();
    registry.register("Panic", panic_factory).unwrap();
    registry.register("KeepAlive", keep_alive_factory).unwrap();
    let mut host = ScriptHost::load(&world, "scene", &BTreeMap::new(), &registry).unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1
    );
    assert!(events[0].message.contains("expected heavy panic"));
    assert_eq!(events[1].message, "healthy heavy destroyed");
    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .to_array(),
        [8.0, 2.0, 0.0]
    );
}

struct NonFiniteHeavy;

fn non_finite_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
    Ok(Box::new(NonFiniteHeavy))
}

impl HeavyScript for NonFiniteHeavy {
    fn on_update(&mut self, ctx: &mut ScriptContext<'_>, _: f32) {
        ctx.transform.position[0] = f32::NAN;
    }

    fn on_destroy(&mut self, ctx: &mut ScriptContext<'_>) {
        ctx.log("non-finite instance destroyed");
    }
}

#[test]
fn non_finite_heavy_transform_disables_instance_and_rolls_back() {
    let mut world = one_camera(
        "[[objects.heavy_scripts]]\nname=\"NonFinite\"\n[objects.transform]\nposition=[8.0, 2.0, 3.0]\n",
    );
    let mut registry = HeavyRegistry::new();
    registry.register("NonFinite", non_finite_factory).unwrap();
    let mut host = ScriptHost::load(&world, "scene", &BTreeMap::new(), &registry).unwrap();
    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let transform = world
        .local_transform(world.handle("camera").unwrap())
        .unwrap();
    assert_eq!(transform.position.to_array(), [8.0, 2.0, 3.0]);
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.contains("non-finite transform"));
}

#[test]
fn invalid_lua_transform_value_is_a_runtime_error_and_rolls_back() {
    for (assignment, expected) in [
        ("x = math.huge", "finite f32"),
        (r#"x = "not a number""#, "finite number"),
    ] {
        let source = format!(
            r#"
local Script = {{}}
function Script:on_update()
  self.transform.position = {{ {assignment}, y = 0, z = 0 }}
end
return Script
"#
        );
        let mut world = one_camera(
            "[[assets]]\npath=\"/bad-transform.lua\"\n[objects.transform]\nposition=[4.0, 0.0, 0.0]\n[[objects.lite_scripts]]\npath=\"/bad-transform.lua\"\n",
        );
        let mut host = ScriptHost::load(
            &world,
            "scene",
            &sources(&[("bad-transform.lua", &source)]),
            &HeavyRegistry::new(),
        )
        .unwrap();
        host.start(&mut world, ScriptTime::default());
        host.update(&mut world, 0.1, ScriptTime::default());

        assert_eq!(
            world
                .local_transform(world.handle("camera").unwrap())
                .unwrap()
                .position
                .x,
            4.0
        );
        let events = host.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, ScriptEventLevel::Error);
        assert!(events[0].message.contains(expected));
    }
}

#[test]
fn lua_transform_assignment_is_rolled_back_when_callback_later_errors() {
    let source = r#"
local Script = {}
function Script:on_update()
  self.transform.position = { x = 99, y = 0, z = 0 }
  error("failure after transform assignment")
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/rollback.lua\"\n[objects.transform]\nposition=[4.0, 0.0, 0.0]\n[[objects.lite_scripts]]\npath=\"/rollback.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("rollback.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());

    assert_eq!(
        world
            .local_transform(world.handle("camera").unwrap())
            .unwrap()
            .position
            .to_array(),
        [4.0, 0.0, 0.0]
    );
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(
        events[0]
            .message
            .contains("failure after transform assignment")
    );
}

#[test]
fn load_rejects_syntax_non_table_and_non_function_callbacks_with_path() {
    for (source, expected) in [
        ("return {", "syntax"),
        ("return 42", "class table"),
        ("return { on_update = true }", "on_update"),
    ] {
        let world = one_camera(
            "[[assets]]\npath=\"/bad.lua\"\n[[objects.lite_scripts]]\npath=\"/bad.lua\"\n",
        );
        let error = ScriptHost::load(
            &world,
            "scene",
            &sources(&[("bad.lua", source)]),
            &HeavyRegistry::new(),
        )
        .err()
        .expect("invalid lite script should fail to load");
        assert!(matches!(
            error,
            ScriptLoadError::Lite { ref path, ref message }
                if path == "bad.lua" && message.to_lowercase().contains(expected)
        ));
    }
}

#[test]
fn load_rejects_unknown_heavy_factory_and_factory_errors() {
    let unknown_world = one_camera("[[objects.heavy_scripts]]\nname=\"Missing\"\n");
    let error = ScriptHost::load(
        &unknown_world,
        "scene",
        &BTreeMap::new(),
        &HeavyRegistry::new(),
    )
    .err()
    .expect("unknown heavy script should fail to load");
    assert_eq!(
        error,
        ScriptLoadError::Heavy {
            object_id: "camera".to_owned(),
            name: "Missing".to_owned(),
            message: "no factory is registered for this name".to_owned(),
        }
    );

    fn fail_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
        Err("factory refused parameters".to_owned())
    }
    let mut registry = HeavyRegistry::new();
    registry.register("Fail", fail_factory).unwrap();
    let failed_world = one_camera("[[objects.heavy_scripts]]\nname=\"Fail\"\n");
    assert!(matches!(
        ScriptHost::load(&failed_world, "scene", &BTreeMap::new(), &registry),
        Err(ScriptLoadError::Heavy { ref message, .. })
            if message == "factory refused parameters"
    ));

    fn panic_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
        panic!("expected factory panic")
    }
    let mut panic_registry = HeavyRegistry::new();
    panic_registry.register("Panic", panic_factory).unwrap();
    let panic_world = one_camera("[[objects.heavy_scripts]]\nname=\"Panic\"\n");
    assert!(matches!(
        ScriptHost::load(&panic_world, "scene", &BTreeMap::new(), &panic_registry),
        Err(ScriptLoadError::Heavy { ref message, .. })
            if message.contains("factory panicked: expected factory panic")
    ));
}

#[test]
fn lua_sandbox_has_only_requested_libraries_and_load_accepts_text_only() {
    let source = r#"
local Script = {}
function Script:on_start()
  assert(io == nil and os == nil and package == nil and debug == nil)
  assert(dofile == nil and loadfile == nil and require == nil)
  assert(type(collectgarbage) == "function")
  collectgarbage()
  assert(type(collectgarbage("count")) == "number")
  collectgarbage("step", 1)
  collectgarbage("collect")
  local text, text_error = load("return 40 + 2", "=dynamic", "t")
  assert(text ~= nil and text_error == nil and text() == 42)
  local binary, binary_error = load(string.dump(function() return 7 end))
  assert(binary == nil and binary_error ~= nil)
  local mode, mode_error = load("return 1", "=binary", "b")
  assert(mode == nil and mode_error ~= nil)
end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/sandbox.lua\"\n[[objects.lite_scripts]]\npath=\"/sandbox.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sandbox.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_collectgarbage_rejects_other_modes_as_a_runtime_error() {
    let source = r#"
local Script = {}
function Script:on_start() collectgarbage("stop") end
return Script
"#;
    let mut world =
        one_camera("[[assets]]\npath=\"/gc.lua\"\n[[objects.lite_scripts]]\npath=\"/gc.lua\"\n");
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("gc.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.contains("collectgarbage mode"));
    assert!(events[0].message.contains("not allowed"));
}

#[test]
fn lua_memory_limit_disables_instance_after_large_allocations() {
    let source = r#"
local Script = {}
function Script:on_update()
  local retained = {}
  while true do
    retained[#retained + 1] = string.rep("x", 1024 * 1024)
  end
end
function Script:on_destroy() rve.log("out of memory instance destroyed") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/memory.lua\"\n[[objects.lite_scripts]]\npath=\"/memory.lua\"\n",
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("memory.lua", source)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.to_lowercase().contains("memory"));
    assert!(
        !events[0]
            .message
            .contains("out of memory instance destroyed")
    );
}

#[test]
fn scripts_cannot_break_host_time_updates_with_table_metamethods() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/a.lua"
[[assets]]
path = "/b.lua"
[[objects.lite_scripts]]
path = "/a.lua"
[[objects.lite_scripts]]
path = "/b.lua"
"#,
    );
    let a = r#"
local Script = {}
function Script:on_start()
  rve.time.elapsed = nil
  rve.time.frame = nil
  setmetatable(rve.time, { __newindex = function() error("hijack") end })
  rve.log("a:start")
end
function Script:on_update() rve.log("a:update") end
return Script
"#;
    let b = r#"
local Script = {}
function Script:on_update()
  rve.log("b:update", rve.time.elapsed, rve.time.frame)
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", a), ("b.lua", b)]),
        &HeavyRegistry::new(),
    )
    .unwrap();

    host.start(&mut world, ScriptTime::default());
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.25,
            frame: 42,
        },
    );

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.message.as_str())
            .collect::<Vec<_>>(),
        ["a:start", "a:update", "b:update 0.25 42"]
    );
}

#[test]
fn interrupt_with_only_heavy_scripts_does_not_report_lua_interruption() {
    let mut world = one_camera(
        r#"
[[objects.heavy_scripts]]
name = "Log"
params = { label = "camera-heavy" }
"#,
    );
    let handle = ScriptInterruptHandle::new();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[]),
        &labeled_registry(),
        handle.clone(),
    )
    .unwrap();
    handle.interrupt();

    host.start(&mut world, ScriptTime::default());
    host.update(&mut world, 0.1, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.message.as_str())
            .collect::<Vec<_>>(),
        ["camera-heavy:start", "camera-heavy:update"]
    );
    assert!(
        events
            .iter()
            .all(|event| event.level == ScriptEventLevel::Info)
    );
}

#[test]
fn interrupt_handle_stops_infinite_lua_loop_and_disables_instance() {
    let (signal_tx, signal_rx) = mpsc::channel();
    let (handle_tx, handle_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut world = one_camera(
            "[[assets]]\npath=\"/loop.lua\"\n[[objects.lite_scripts]]\npath=\"/loop.lua\"\n",
        );
        let source = r#"
local Script = {}
function Script:on_update()
  signal()
  while true do
    pcall(function()
      while true do end
    end)
  end
end
function Script:on_destroy() rve.log("loop destroyed") end
return Script
"#;
        let mut host = ScriptHost::load(
            &world,
            "scene",
            &sources(&[("loop.lua", source)]),
            &HeavyRegistry::new(),
        )
        .unwrap();
        let signal = host
            .lua
            .create_function(move |_, ()| {
                signal_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        host.lua.globals().set("signal", signal).unwrap();
        handle_tx.send(host.interrupt_handle()).unwrap();
        host.update(&mut world, 0.1, ScriptTime::default());
        host.destroy(&mut world, ScriptTime::default());
        result_tx.send(host.drain_events()).unwrap();
    });

    let handle = handle_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    signal_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    handle.interrupt();
    let events = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(handle.is_interrupted());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Error);
    assert!(events[0].message.contains("interrupted"));
}

#[test]
fn interrupt_set_before_start_disables_lite_once_and_skips_lite_destroy() {
    let source = r#"
local Script = {}
function Script:on_start() rve.log("lite started") end
function Script:on_destroy() rve.log("lite destroyed") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath=\"/pre-interrupted.lua\"\n[[objects.lite_scripts]]\npath=\"/pre-interrupted.lua\"\n[[objects.heavy_scripts]]\nname=\"Log\"\nparams={label=\"camera-heavy\"}\n",
    );
    let handle = ScriptInterruptHandle::new();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[("pre-interrupted.lua", source)]),
        &labeled_registry(),
        handle.clone(),
    )
    .unwrap();
    handle.interrupt();

    host.start(&mut world, ScriptTime::default());
    host.destroy(&mut world, ScriptTime::default());

    let events = host.drain_events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "scripts interrupted")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "camera-heavy:start")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message == "camera-heavy:destroy")
    );
    assert!(!events.iter().any(|event| event.message == "lite started"));
    assert!(!events.iter().any(|event| event.message == "lite destroyed"));
}

#[test]
fn interrupt_handle_cancels_infinite_top_level_chunk_during_load() {
    let handle = ScriptInterruptHandle::new();
    let worker_handle = handle.clone();
    let (loading_tx, loading_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let world = one_camera(
            "[[assets]]\npath=\"/load-loop.lua\"\n[[objects.lite_scripts]]\npath=\"/load-loop.lua\"\n",
        );
        let source = "while true do end";
        loading_tx.send(()).unwrap();
        let result = ScriptHost::load_with_interrupt(
            &world,
            "scene",
            &sources(&[("load-loop.lua", source)]),
            &HeavyRegistry::new(),
            worker_handle,
        )
        .map(|_| ());
        result_tx.send(result).unwrap();
    });

    loading_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    handle.interrupt();
    let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert!(matches!(
        result,
        Err(ScriptLoadError::Lite { ref path, ref message })
            if path == "load-loop.lua" && message.to_lowercase().contains("interrupted")
    ));
}

mod abi;
