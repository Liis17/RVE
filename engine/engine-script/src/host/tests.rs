use super::{ScriptHost, ScriptInterruptHandle, ScriptLoadError};
use crate::{Key, MouseButton, ScriptEventLevel, ScriptInput, ScriptLibrary, ScriptTime};
use engine_scene::{SceneDocument, World};
use rve_sdk::{Context, Params, Script, Value};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn world(source: &str) -> World {
    let source = if source.contains("[objects.camera]") {
        source.to_owned()
    } else {
        format!(
            "{source}\n[[objects]]\nid = \"__test_camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n"
        )
    };
    let document = SceneDocument::parse("test.rvescene", &source).unwrap();
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

#[test]
fn lua_input_api_reads_the_shared_frame_snapshot() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  local x, y = rve.input.cursor()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if rve.input.key_down("W") and rve.input.key_pressed("W")
      and rve.input.key_released("A")
      and rve.input.mouse_down("Left") and rve.input.mouse_pressed("Left")
      and rve.input.mouse_released("Right")
      and x == 40 and y == 60 and dx == 2.5 and dy == -3
      and wx == 1 and wy == 0.5 then
    rve.log_info("Lua input snapshot")
  else
    error("input snapshot mismatch")
  end
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    let mut input = ScriptInput::default();
    input.set_key(Key::W, true);
    input.set_key(Key::A, true);
    input.set_key(Key::A, false);
    input.set_mouse_button(MouseButton::Left, true);
    input.set_mouse_button(MouseButton::Right, true);
    input.set_mouse_button(MouseButton::Right, false);
    input.set_cursor(40.0, 60.0);
    input.add_mouse_delta(2.5, -3.0);
    input.add_wheel(1.0, 0.5);
    host.set_input(input);
    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "Lua input snapshot"
    }));
}

#[test]
fn lua_destroy_clears_input_edges_and_deltas_but_keeps_down_and_cursor() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  local x, y = rve.input.cursor()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if rve.input.key_down("W") and rve.input.key_pressed("W")
      and rve.input.key_released("A")
      and rve.input.mouse_down("Left") and rve.input.mouse_pressed("Left")
      and rve.input.mouse_released("Right")
      and x == 40 and y == 60 and dx == 2.5 and dy == -3
      and wx == 1 and wy == 0.5 then
    rve.log_info("input update visible")
  else
    error("input update mismatch")
  end
end
function Input:on_destroy()
  local x, y = rve.input.cursor()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if not rve.input.key_down("W") or rve.input.key_pressed("W")
      or rve.input.key_released("W") or rve.input.key_down("A")
      or rve.input.key_pressed("A") or rve.input.key_released("A")
      or not rve.input.mouse_down("Left") or rve.input.mouse_pressed("Left")
      or rve.input.mouse_released("Left") or rve.input.mouse_down("Right")
      or rve.input.mouse_pressed("Right") or rve.input.mouse_released("Right")
      or x ~= 40 or y ~= 60 or dx ~= 0 or dy ~= 0 or wx ~= 0 or wy ~= 0 then
    error("input destroy mismatch")
  end
  rve.log_info("destroy input cleared")
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    let mut input = ScriptInput::default();
    input.set_key(Key::W, true);
    input.set_key(Key::A, true);
    input.set_key(Key::A, false);
    input.set_mouse_button(MouseButton::Left, true);
    input.set_mouse_button(MouseButton::Right, true);
    input.set_mouse_button(MouseButton::Right, false);
    input.set_cursor(40.0, 60.0);
    input.add_mouse_delta(2.5, -3.0);
    input.add_wheel(1.0, 0.5);
    host.set_input(input);

    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "input update visible"
    }));

    host.destroy(&mut world, ScriptTime::default());
    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Info && event.message == "destroy input cleared"
    }));
}

#[test]
fn lua_input_rejects_unknown_key_names() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  rve.input.key_down("UnknownKey")
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("unknown input key")
    }));
}

#[test]
fn lua_input_rejects_unknown_mouse_button_names() {
    let mut world = one_camera(
        r#"
[[assets]]
path = "/input.lua"
[[objects.lite_scripts]]
path = "/input.lua"
"#,
    );
    let source = r#"
local Input = {}
function Input:on_update()
  rve.input.mouse_down("Extra")
end
return Input
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("input.lua", source)]), None).unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());

    assert!(host.drain_events().iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("unknown mouse button")
    }));
}

struct LabeledHeavy {
    label: String,
}

fn labeled_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    let Some(Value::String(label)) = params.get("label") else {
        return Err("missing string parameter label".to_owned());
    };
    Ok(Box::new(LabeledHeavy {
        label: label.clone(),
    }))
}

impl Script for LabeledHeavy {
    fn on_start(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info(format!("{}:start", self.label));
    }

    fn on_update(&mut self, ctx: &mut Context<'_>, _dt: f32) {
        ctx.log_info(format!("{}:update", self.label));
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info(format!("{}:destroy", self.label));
    }
}

struct ParamsAndTransformHeavy;

fn params_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    let expected = Params::from([
        ("flag".to_owned(), Value::Bool(true)),
        ("count".to_owned(), Value::Int(17)),
        ("ratio".to_owned(), Value::Float(1.25)),
        ("title".to_owned(), Value::String("hello".to_owned())),
        (
            "values".to_owned(),
            Value::Array(vec![
                Value::Int(2),
                Value::Bool(false),
                Value::String("x".to_owned()),
            ]),
        ),
    ]);
    if params == &expected {
        Ok(Box::new(ParamsAndTransformHeavy))
    } else {
        Err("parameter values differ".to_owned())
    }
}

impl Script for ParamsAndTransformHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _dt: f32) {
        ctx.transform.position[0] += 1.0;
    }
}

struct PanicHeavy;

fn panic_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(PanicHeavy))
}

impl Script for PanicHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _: f32) {
        ctx.transform.position[0] = 99.0;
        panic!("expected heavy panic");
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info("panic instance destroyed");
    }
}

struct KeepAliveHeavy;

fn keep_alive_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(KeepAliveHeavy))
}

impl Script for KeepAliveHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _: f32) {
        ctx.transform.position[1] += 1.0;
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info("healthy heavy destroyed");
    }
}

struct NonFiniteHeavy;

fn non_finite_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(NonFiniteHeavy))
}

impl Script for NonFiniteHeavy {
    fn on_update(&mut self, ctx: &mut Context<'_>, _: f32) {
        ctx.transform.position[0] = f32::NAN;
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        ctx.log_info("non-finite instance destroyed");
    }
}

fn fail_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Err("factory refused parameters".to_owned())
}

fn test_module() -> &'static rve_sdk::abi::RveModule {
    rve_sdk::script_module! {
        "Log" => labeled_factory,
        "Params" => params_factory,
        "Panic" => panic_factory,
        "KeepAlive" => keep_alive_factory,
        "NonFinite" => non_finite_factory,
        "Fail" => fail_factory,
    }
}

fn test_library() -> ScriptLibrary {
    // SAFETY: the SDK macro returns an immutable process-lifetime table.
    unsafe { ScriptLibrary::from_module(std::ptr::from_ref(test_module())) }
        .expect("test script module is valid")
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
    let library = test_library();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("a.lua", lite_source), ("b.lua", lite_source)]),
        Some(&library),
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
        None,
    )
    .unwrap();
    host.update(&mut world, 0.1, ScriptTime::default());
    assert_eq!(event_messages(&mut host), ["start", "update"]);
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
    let library = test_library();
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("params.lua", source)]),
        Some(&library),
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
    let mut host =
        ScriptHost::load(&world, "level", &sources(&[("log.lua", source)]), None).unwrap();
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
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("bytes.lua", source)]), None).unwrap();

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
        None,
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
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("frame.lua", source)]), None).unwrap();

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
        None,
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

#[test]
fn heavy_panic_disables_that_instance_but_other_heavy_scripts_continue() {
    let mut world = one_camera(
        "[objects.transform]\nposition=[8.0, 0.0, 0.0]\n[[objects.heavy_scripts]]\nname=\"Panic\"\n[[objects.heavy_scripts]]\nname=\"KeepAlive\"\n",
    );
    let library = test_library();
    let mut host = ScriptHost::load(&world, "scene", &BTreeMap::new(), Some(&library)).unwrap();
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

#[test]
fn non_finite_heavy_transform_disables_instance_and_rolls_back() {
    let mut world = one_camera(
        "[[objects.heavy_scripts]]\nname=\"NonFinite\"\n[objects.transform]\nposition=[8.0, 2.0, 3.0]\n",
    );
    let library = test_library();
    let mut host = ScriptHost::load(&world, "scene", &BTreeMap::new(), Some(&library)).unwrap();
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
            None,
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
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("rollback.lua", source)]), None).unwrap();

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
        let error = ScriptHost::load(&world, "scene", &sources(&[("bad.lua", source)]), None)
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
fn load_rejects_heavy_scripts_without_a_library_or_registration() {
    let unknown_world = one_camera("[[objects.heavy_scripts]]\nname=\"Missing\"\n");
    let error = ScriptHost::load(&unknown_world, "scene", &BTreeMap::new(), None)
        .err()
        .expect("unknown heavy script should fail to load");
    assert_eq!(
        error,
        ScriptLoadError::Heavy {
            object_id: "camera".to_owned(),
            name: "Missing".to_owned(),
            message:
                "no script library is loaded; pass --scripts <path> or place the library in Engine/"
                    .to_owned(),
        }
    );

    let library = test_library();
    let unknown_name = ScriptHost::load(&unknown_world, "scene", &BTreeMap::new(), Some(&library))
        .err()
        .expect("unregistered heavy script should fail to load");
    assert!(matches!(
        unknown_name,
        ScriptLoadError::Heavy { ref message, .. }
            if message.contains("does not register this name")
    ));

    let failed_world = one_camera("[[objects.heavy_scripts]]\nname=\"Fail\"\n");
    assert!(matches!(
        ScriptHost::load(&failed_world, "scene", &BTreeMap::new(), Some(&library)),
        Err(ScriptLoadError::Heavy { ref message, .. })
            if message == "factory refused parameters"
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
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("sandbox.lua", source)]), None).unwrap();
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
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("gc.lua", source)]), None).unwrap();

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
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("memory.lua", source)]), None).unwrap();

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
        None,
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
    let library = test_library();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[]),
        Some(&library),
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
        let mut host =
            ScriptHost::load(&world, "scene", &sources(&[("loop.lua", source)]), None).unwrap();
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
    let library = test_library();
    let mut host = ScriptHost::load_with_interrupt(
        &world,
        "scene",
        &sources(&[("pre-interrupted.lua", source)]),
        Some(&library),
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
            None,
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

#[test]
fn lua_object_writes_are_transactional_and_world_matrices_are_phase_snapshots() {
    let mut world = world(
        r#"
[[assets]]
path = "/objects.lua"
[[objects]]
id = "sender"
name = "Sender"
[objects.transform]
position = [1.0, 0.0, 0.0]
[[objects.lite_scripts]]
path = "/objects.lua"
params = { role = "sender" }
[[objects]]
id = "target"
name = "Target"
[objects.transform]
position = [2.0, 0.0, 0.0]
[[objects.lite_scripts]]
path = "/objects.lua"
params = { role = "target" }
[[objects]]
id = "external"
name = "External"
[objects.transform]
position = [4.0, 0.0, 0.0]
"#,
    );
    let source = r#"
local Script = {}
function Script:on_update()
  local own = assert(rve.find(self.object.id))
  local matrix = own:world_matrix()
  if self.params.role == "sender" then
    if rve.time.frame == 1 then
      assert(own.id == "sender" and own.name == "Sender")
      local external = assert(rve.find("external"))
      local transform = external:get_transform()
      transform.position = { x = 5, y = 0, z = 0 }
      external:set_transform(transform)
      assert(external:world_matrix()[13] == 4)

      local through_handle = own:get_transform()
      through_handle.position = { x = 99, y = 0, z = 0 }
      own:set_transform(through_handle)
      self.transform.position = { x = 1, y = 3, z = 0 }
      assert(matrix[13] == 1)
    else
      assert(matrix[13] == 1)
    end
  elseif rve.time.frame == 1 then
    assert(matrix[13] == 2)
    local through_handle = own:get_transform()
    through_handle.position = { x = 88, y = 0, z = 0 }
    own:set_transform(through_handle)
  else
    assert(matrix[13] == 88)
  end
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("objects.lua", source)]), None).unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let sender = world
        .local_transform(world.handle("sender").unwrap())
        .unwrap();
    let target = world
        .local_transform(world.handle("target").unwrap())
        .unwrap();
    let external = world
        .local_transform(world.handle("external").unwrap())
        .unwrap();
    assert_eq!(sender.position.to_array(), [1.0, 3.0, 0.0]);
    assert_eq!(target.position.to_array(), [88.0, 0.0, 0.0]);
    assert_eq!(external.position.to_array(), [5.0, 0.0, 0.0]);
    assert!(host.drain_events().is_empty());

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_messages_from_start_and_on_message_are_delivered_in_order_on_later_updates() {
    let mut world = world(
        r#"
[[assets]]
path = "/messages.lua"
[[objects]]
id = "sender"
name = "Sender"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
name = "Receiver"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "receiver" }
[[objects]]
id = "observer"
name = "Observer"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "observer" }
"#,
    );
    let source = r#"
local Script = {}
function Script:on_start()
  if self.params.role == "sender" then
    rve.send("receiver", "one", { "hello", 2, false })
    rve.broadcast("fan", "ready")
  end
end
function Script:on_message(sender, name, value)
  if name == "one" then
    assert(value[1] == "hello" and value[2] == 2 and value[3] == false)
    rve.send("sender", "reply", { "ok" })
  end
  rve.log_info(self.params.role .. ":" .. name .. ":" .. sender)
end
function Script:on_update()
  rve.log_info(self.params.role .. ":update")
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("messages.lua", source)]), None).unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        [
            "receiver:one:sender",
            "sender:fan:sender",
            "receiver:fan:sender",
            "observer:fan:sender",
            "sender:update",
            "receiver:update",
            "observer:update",
        ]
    );

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        [
            "sender:reply:receiver",
            "sender:update",
            "receiver:update",
            "observer:update",
        ]
    );
}

#[test]
fn lua_message_and_update_callbacks_share_one_world_matrix_snapshot() {
    let mut world = world(
        r#"
[[assets]]
path = "/sender.lua"
[[assets]]
path = "/target.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "target"
[objects.transform]
position = [2.0, 0.0, 0.0]
[[objects.lite_scripts]]
path = "/target.lua"
"#,
    );
    let sender = r#"
local Script = {}
function Script:on_start() rve.send("target", "move", true) end
return Script
"#;
    let target = r#"
local Script = {}
function Script:on_message(_, name)
  if name == "move" then
    local object = rve.find("target")
    local transform = object:get_transform()
    transform.position = { x = 10, y = 0, z = 0 }
    object:set_transform(transform)
    assert(object:world_matrix()[13] == 2)
    rve.log_info("message matrix 2")
  end
end
function Script:on_update()
  local matrix = rve.find("target"):world_matrix()
  if rve.time.frame == 1 then
    assert(matrix[13] == 2)
    rve.log_info("update matrix 2")
  else
    assert(matrix[13] == 10)
    rve.log_info("update matrix 10")
  end
end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sender.lua", sender), ("target.lua", target)]),
        None,
    )
    .unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    assert_eq!(
        event_messages(&mut host),
        ["message matrix 2", "update matrix 2"]
    );
    assert_eq!(
        world
            .local_transform(world.handle("target").unwrap())
            .unwrap()
            .position
            .x,
        10.0
    );
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert_eq!(event_messages(&mut host), ["update matrix 10"]);
}

#[test]
fn lua_failed_send_rolls_back_earlier_messages_and_unknown_targets_are_errors() {
    let mut world = world(
        r#"
[[assets]]
path = "/send.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/send.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/send.lua"
params = { role = "receiver" }
"#,
    );
    let source = r#"
local Script = {}
function Script:on_start()
  if self.params.role == "sender" then
    rve.send("receiver", "must_rollback", 1)
    rve.send("missing", "unknown", 2)
  end
end
function Script:on_message(_, name)
  rve.log_info("received:" .. name)
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("send.lua", source)]), None).unwrap();
    let sender = world.handle("sender").unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );

    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event.message.contains("target \"missing\" does not exist")
    }));
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains("received:"))
    );
    assert_eq!(
        world.local_transform(sender).unwrap().position.to_array(),
        [0.0; 3]
    );
}

#[test]
fn lua_message_error_rolls_back_its_transaction_and_disables_the_receiver() {
    let mut world = world(
        r#"
[[assets]]
path = "/messages.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "receiver" }
[[objects]]
id = "observer"
[[objects.lite_scripts]]
path = "/messages.lua"
params = { role = "observer" }
"#,
    );
    let source = r#"
local Script = {}
function Script:on_start()
  if self.params.role == "sender" then
    rve.send("receiver", "fail", true)
    rve.send("receiver", "later", true)
  end
end
function Script:on_message(_, name)
  if self.params.role == "receiver" and name == "fail" then
    local transform = rve.find("receiver"):get_transform()
    transform.position = { x = 9, y = 0, z = 0 }
    rve.find("receiver"):set_transform(transform)
    rve.send("observer", "must_rollback", true)
    rve.send("missing", "error", true)
  elseif self.params.role == "observer" then
    rve.log_info("observer received " .. name)
  end
end
function Script:on_update()
  if self.params.role == "receiver" then rve.log_info("receiver update") end
end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("messages.lua", source)]), None).unwrap();
    let receiver = world.handle("receiver").unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );

    assert_eq!(
        world.local_transform(receiver).unwrap().position.to_array(),
        [0.0; 3]
    );
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error
            && event.source == "scene/receiver/messages.lua"
            && event.message.contains("target \"missing\" does not exist")
    }));
    assert!(events.iter().all(|event| {
        !event.message.contains("receiver update") && !event.message.contains("observer received")
    }));

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );
    assert!(host.drain_events().is_empty());
}

#[test]
fn lua_message_limit_drops_excess_without_disabling_sender() {
    let mut world = world(
        r#"
[[assets]]
path = "/sender.lua"
[[assets]]
path = "/receiver.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/receiver.lua"
params = { count = 0 }
"#,
    );
    let sender = r#"
local Script = {}
function Script:on_start()
  for i = 1, 4100 do rve.send("receiver", "bulk", i) end
end
function Script:on_update() rve.log_info("sender remains enabled") end
return Script
"#;
    let receiver = r#"
local Script = {}
function Script:on_message(_, name)
  if name == "bulk" then self.params.count = self.params.count + 1 end
end
function Script:on_update() rve.log_info("received " .. self.params.count) end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sender.lua", sender), ("receiver.lua", receiver)]),
        None,
    )
    .unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("dropped 4 message(s)")
    }));
    assert!(
        events
            .iter()
            .any(|event| event.message == "sender remains enabled")
    );
    assert!(events.iter().any(|event| event.message == "received 4096"));
}

#[test]
fn lua_message_byte_budget_drops_excess_in_order_without_disabling_sender() {
    const MESSAGE_COUNT: usize = 132;
    const MESSAGE_VALUE_BYTES: usize = 65_536;
    let max_bytes = crate::transaction::MAX_MESSAGE_BYTES_PER_FRAME;
    let mut expected_bytes = 0usize;
    let expected_delivery_count = (1..=MESSAGE_COUNT)
        .take_while(|index| {
            let name = format!("bulk{index}");
            let message_bytes = std::mem::size_of::<engine_scene::ParamValue>()
                + "sender".len()
                + name.len()
                + MESSAGE_VALUE_BYTES;
            if expected_bytes.saturating_add(message_bytes) > max_bytes {
                return false;
            }
            expected_bytes += message_bytes;
            true
        })
        .count();
    let mut world = world(
        r#"
[[assets]]
path = "/sender.lua"
[[assets]]
path = "/receiver.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/sender.lua"
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/receiver.lua"
"#,
    );
    let sender = format!(
        r#"
local Script = {{}}
function Script:on_start()
  for i = 1, {MESSAGE_COUNT} do
    rve.send("receiver", "bulk" .. i, string.rep("x", {MESSAGE_VALUE_BYTES}))
  end
end
function Script:on_update() rve.log_info("sender remains enabled") end
return Script
"#
    );
    let receiver = r#"
local Script = {}
function Script:on_message(_, name) rve.log_info("received " .. name) end
return Script
"#;
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[("sender.lua", &sender), ("receiver.lua", receiver)]),
        None,
    )
    .unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    let events = host.drain_events();
    let delivered = events
        .iter()
        .filter_map(|event| {
            event
                .message
                .strip_prefix("received bulk")
                .and_then(|value| value.parse::<usize>().ok())
        })
        .collect::<Vec<_>>();
    assert_eq!(delivered, (1..=expected_delivery_count).collect::<Vec<_>>());
    let overflow_errors = events
        .iter()
        .filter(|event| {
            event.level == ScriptEventLevel::Error && event.message.contains("message queue limits")
        })
        .collect::<Vec<_>>();
    assert_eq!(overflow_errors.len(), 1);
    assert!(overflow_errors[0].message.contains(&format!(
        "dropped {} message(s)",
        MESSAGE_COUNT - expected_delivery_count
    )));
    assert_eq!(host.transaction.peak_pending_bytes(), expected_bytes);
    assert!(
        events
            .iter()
            .any(|event| event.message == "sender remains enabled")
    );
}

#[test]
fn lua_creation_phases_reject_object_writes_and_messages() {
    for (path, source) in [
        (
            "write.lua",
            r#"local object = rve.find("camera"); local transform = object:get_transform(); object:set_transform(transform); return {}"#,
        ),
        (
            "send.lua",
            r#"rve.send("camera", "early", true); return {}"#,
        ),
        (
            "broadcast.lua",
            r#"rve.broadcast("early", true); return {}"#,
        ),
    ] {
        let world = one_camera(&format!(
            "[[assets]]\npath = \"/{path}\"\n[[objects.lite_scripts]]\npath = \"/{path}\"\n"
        ));
        let handle = world.handle("camera").unwrap();
        let before = *world.local_transform(handle).unwrap();
        let result = ScriptHost::load(&world, "scene", &sources(&[(path, source)]), None);
        assert!(
            matches!(result, Err(ScriptLoadError::Lite { message, .. }) if message.contains("unavailable during script creation"))
        );
        assert_eq!(*world.local_transform(handle).unwrap(), before);
    }
}

#[test]
fn lua_warning_logging_keeps_the_warn_level() {
    let source = r#"
local Script = {}
function Script:on_start() rve.log_warn("watch this") end
return Script
"#;
    let mut world = one_camera(
        "[[assets]]\npath = \"/warn.lua\"\n[[objects.lite_scripts]]\npath = \"/warn.lua\"\n",
    );
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("warn.lua", source)]), None).unwrap();
    host.start(&mut world, ScriptTime::default());
    let events = host.drain_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].level, ScriptEventLevel::Warn);
    assert_eq!(events[0].message, "watch this");
}

#[test]
fn lua_error_rolls_back_multiple_object_writes_and_messages_but_keeps_logs() {
    let mut world = world(
        r#"
[[assets]]
path = "/rollback.lua"
[[objects]]
id = "sender"
[[objects.lite_scripts]]
path = "/rollback.lua"
params = { role = "sender" }
[[objects]]
id = "receiver"
[[objects.lite_scripts]]
path = "/rollback.lua"
params = { role = "receiver" }
[[objects]]
id = "external"
"#,
    );
    let source = r#"
local Script = {}
function Script:on_update()
  if self.params.role == "sender" then
    local own = rve.find("sender")
    local own_transform = own:get_transform()
    own_transform.position = { x = 10, y = 0, z = 0 }
    own:set_transform(own_transform)
    local external = rve.find("external")
    local other_transform = external:get_transform()
    other_transform.position = { x = 20, y = 0, z = 0 }
    external:set_transform(other_transform)
    rve.send("receiver", "rolled_back", true)
    rve.log_info("log survives rollback")
    error("transaction failure")
  end
end
function Script:on_message(_, name) rve.log_info("received:" .. name) end
return Script
"#;
    let mut host =
        ScriptHost::load(&world, "scene", &sources(&[("rollback.lua", source)]), None).unwrap();
    let sender = world.handle("sender").unwrap();
    let external = world.handle("external").unwrap();

    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.1,
            frame: 1,
        },
    );
    host.update(
        &mut world,
        0.1,
        ScriptTime {
            elapsed: 0.2,
            frame: 2,
        },
    );

    assert_eq!(
        world.local_transform(sender).unwrap().position.to_array(),
        [0.0; 3]
    );
    assert_eq!(
        world.local_transform(external).unwrap().position.to_array(),
        [0.0; 3]
    );
    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.message == "log survives rollback")
    );
    assert!(
        events
            .iter()
            .any(|event| event.message.contains("transaction failure"))
    );
    assert!(
        events
            .iter()
            .all(|event| !event.message.contains("received:"))
    );
}

mod abi;
