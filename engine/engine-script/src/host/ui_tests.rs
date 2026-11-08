use super::ScriptHost;
use crate::lifecycle::{PrefabCatalog, PrefabKind, PrefabSpec};
use crate::ui::owner_key;
use crate::{ScriptEventLevel, ScriptLoadOptions, ScriptTime};
use engine_files::VirtualPath;
use engine_gui::document::UiDocument;
use engine_gui::events::{UiEvent, UiEventKind, UiValue};
use engine_gui::system::{UiCanvasHandle, UiElementHandle};
use engine_scene::{PrefabTemplate, SceneDocument, SpawnOptions, World};
use std::collections::BTreeMap;
use std::sync::Arc;

const MENU: &str = r#"
[[elements]]
id = "label"
text = { string = "Hi" }

[[elements]]
id = "volume"
slider = { min = 0.0, max = 10.0, value = 2.0 }

[[elements]]
id = "mode"
dropdown = { options = ["a", "b"], selected = 0 }

[[elements]]
id = "go"
button = {}
"#;

const PREFAB: &str = "prefabs/popup.rveprefab";
const PREFAB_TEXT: &str = "[[assets]]\npath = \"/ui/menu.rveui\"\n[[assets]]\npath = \"/popup.lua\"\n\
[[objects]]\nid = \"root\"\n[objects.ui_canvas]\ndocument = \"/ui/menu.rveui\"\n\
[[objects.lite_scripts]]\npath = \"/popup.lua\"\n";

const POPUP_LUA: &str = r#"
local Popup = {}
function Popup:on_start()
  self.extra = rve.ui.open("ui/menu.rveui")
end
return Popup
"#;

fn scene(objects: &str, scripts: &[&str]) -> World {
    let mut source = String::from("[[assets]]\npath = \"/ui/menu.rveui\"\n");
    for script in scripts {
        source.push_str(&format!("[[assets]]\npath = \"/{script}\"\n"));
    }
    source.push_str(objects);
    source.push_str("\n[[objects]]\nid = \"__test_camera\"\n[objects.camera]\n\n");
    World::from_document(&SceneDocument::parse("test.rvescene", &source).unwrap()).unwrap()
}

/// Объект сцены с канвасом `menu.rveui` и одним lite-скриптом.
fn canvas_object(id: &str, script: &str) -> String {
    format!(
        "[[objects]]\nid = \"{id}\"\n[objects.ui_canvas]\ndocument = \"/ui/menu.rveui\"\n\
         [[objects.lite_scripts]]\npath = \"/{script}\"\n"
    )
}

fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
        .collect()
}

fn load(world: &World, entries: &[(&str, &str)]) -> ScriptHost {
    let mut host = ScriptHost::load(world, "scene", &sources(entries), None).unwrap();
    register_menu(&mut host);
    host
}

fn register_menu(host: &mut ScriptHost) {
    host.register_ui_document(
        VirtualPath::parse("ui/menu.rveui").unwrap(),
        Arc::new(UiDocument::parse("ui/menu.rveui", MENU).unwrap()),
    );
}

fn messages(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .map(|event| event.message)
        .collect()
}

fn component(host: &ScriptHost, world: &World, id: &str) -> UiCanvasHandle {
    let object = world.handle(id).unwrap();
    *host
        .ui
        .borrow()
        .component_canvases
        .get(&owner_key(object))
        .unwrap()
}

fn element(host: &ScriptHost, canvas: UiCanvasHandle, id: &str) -> UiElementHandle {
    host.ui().find(canvas, id).unwrap()
}

/// Все живые канвасы; индексы и поколения перебираются, потому что публичного списка нет.
fn open_canvases(host: &ScriptHost) -> Vec<UiCanvasHandle> {
    let ui = host.ui();
    (0..64)
        .flat_map(|index| (0..4).map(move |generation| UiCanvasHandle::from_raw(index, generation)))
        .filter(|canvas| ui.layer(*canvas).is_ok())
        .collect()
}

const LUA_CASE_SCENE_SCRIPT: &str = r#"
local Menu = {}
function Menu:on_start()
  local canvas = rve.ui.canvas(self.object)
  rve.log_info("visible " .. tostring(canvas:visible()))
  canvas:find("label"):set_text("Hello")
  canvas:find("volume"):set_value(7)
  canvas:find("mode"):set_value(2)
  canvas:set_layer(5)
  rve.log_info("label " .. canvas:find("label"):get_text())
  rve.log_info("missing " .. tostring(canvas:find("nope")))
end
return Menu
"#;

#[test]
fn lua_ui_round_trip_reads_pending_writes_and_applies_them_at_commit() {
    let mut world = scene(&canvas_object("menu", "menu.lua"), &["menu.lua"]);
    let mut host = load(&world, &[("menu.lua", LUA_CASE_SCENE_SCRIPT)]);
    host.start(&mut world, ScriptTime::default());

    let events = messages(&mut host);
    assert!(events.contains(&"visible true".to_owned()), "{events:?}");
    assert!(events.contains(&"label Hello".to_owned()), "{events:?}");
    assert!(events.contains(&"missing nil".to_owned()), "{events:?}");

    let canvas = component(&host, &world, "menu");
    let label = element(&host, canvas, "label");
    assert_eq!(host.ui().text(label), Ok("Hello".to_owned()));
    let volume = element(&host, canvas, "volume");
    assert_eq!(host.ui().value(volume), Ok(Some(UiValue::Number(7.0))));
    let mode = element(&host, canvas, "mode");
    assert_eq!(host.ui().value(mode), Ok(Some(UiValue::Index(1))));
    assert_eq!(host.ui().layer(canvas), Ok(5));
}

const FAILING_SCRIPT: &str = r#"
local Menu = {}
function Menu:on_update(dt)
  rve.ui.canvas(self.object):find("label"):set_text("changed")
  self.extra = rve.ui.open("ui/menu.rveui")
  error("callback failed")
end
return Menu
"#;

#[test]
fn failed_callback_rolls_back_ui_writes_and_script_opened_canvases() {
    let mut world = scene(&canvas_object("menu", "menu.lua"), &["menu.lua"]);
    let mut host = load(&world, &[("menu.lua", FAILING_SCRIPT)]);
    host.update(&mut world, 0.1, ScriptTime::default());

    let events = host.drain_events();
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message.contains("callback failed")
    }));
    let canvas = component(&host, &world, "menu");
    assert_eq!(
        host.ui().text(element(&host, canvas, "label")),
        Ok("Hi".to_owned())
    );
    assert_eq!(open_canvases(&host), vec![canvas]);
}

const CLOSE_SCRIPT: &str = r#"
local Menu = {}
function Menu:on_update(dt)
  if self.canvas == nil then
    self.canvas = rve.ui.canvas(self.object)
    rve.ui.close(self.canvas)
    local ok = pcall(function() return self.canvas:visible() end)
    rve.log_info(ok and "readable in call" or "stale in call")
    return
  end
  local ok = pcall(function() return self.canvas:visible() end)
  rve.log_info(ok and "still open" or "closed after commit")
end
return Menu
"#;

#[test]
fn close_is_stale_inside_the_call_and_applied_at_commit() {
    let mut world = scene(&canvas_object("menu", "menu.lua"), &["menu.lua"]);
    let mut host = load(&world, &[("menu.lua", CLOSE_SCRIPT)]);
    host.start(&mut world, ScriptTime::default());
    let canvas = component(&host, &world, "menu");

    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(messages(&mut host).contains(&"stale in call".to_owned()));
    assert!(host.ui().layer(canvas).is_err());

    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(messages(&mut host).contains(&"closed after commit".to_owned()));
}

const UNREGISTERED_SCRIPT: &str = r#"
local Menu = {}
function Menu:on_start()
  local ok, err = pcall(rve.ui.open, "ui/missing.rveui")
  rve.log_info(ok and "opened" or err)
  local canvas = rve.ui.canvas(self.object)
  local ok2, err2 = pcall(function() canvas:find("volume"):set_text("x") end)
  rve.log_info(ok2 and "written" or err2)
end
return Menu
"#;

#[test]
fn lua_ui_reports_unregistered_documents_and_unsupported_properties() {
    let mut world = scene(&canvas_object("menu", "menu.lua"), &["menu.lua"]);
    let mut host = load(&world, &[("menu.lua", UNREGISTERED_SCRIPT)]);
    host.start(&mut world, ScriptTime::default());
    let events = messages(&mut host);
    assert!(
        events
            .iter()
            .any(|event| event.contains("не зарегистрирован")),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.contains("не поддерживает свойство text")),
        "{events:?}"
    );
}

const LISTENER_SCRIPT: &str = r#"
local Listener = {}
function Listener:on_ui_event(event)
  rve.log_info("ui " .. self.object.id .. " " .. event.id .. " " .. event.kind .. " " .. tostring(event.value))
end
return Listener
"#;

#[test]
fn ui_events_reach_only_instances_of_the_canvas_owner() {
    let objects = format!(
        "{}{}",
        canvas_object("menu", "listener.lua"),
        "[[objects]]\nid = \"other\"\n[[objects.lite_scripts]]\npath = \"/listener.lua\"\n"
    );
    let mut world = scene(&objects, &["listener.lua"]);
    let mut host = load(&world, &[("listener.lua", LISTENER_SCRIPT)]);
    host.start(&mut world, ScriptTime::default());
    let canvas = component(&host, &world, "menu");
    let go = element(&host, canvas, "go");
    let volume = element(&host, canvas, "volume");

    host.queue_ui_events([
        UiEvent {
            canvas,
            element: go,
            kind: UiEventKind::Click,
            value: None,
        },
        UiEvent {
            canvas,
            element: volume,
            kind: UiEventKind::ValueChanged,
            value: Some(UiValue::Number(4.0)),
        },
    ]);
    host.update(&mut world, 0.1, ScriptTime::default());
    let events = messages(&mut host);
    assert!(
        events.contains(&"ui menu go click nil".to_owned()),
        "{events:?}"
    );
    assert!(
        events.contains(&"ui menu volume value_changed 4.0".to_owned()),
        "{events:?}"
    );
    assert!(
        events.iter().all(|event| !event.contains("ui other")),
        "{events:?}"
    );

    host.ui_mut().close(canvas).unwrap();
    host.queue_ui_events([UiEvent {
        canvas,
        element: go,
        kind: UiEventKind::Click,
        value: None,
    }]);
    host.update(&mut world, 0.1, ScriptTime::default());
    assert!(
        messages(&mut host)
            .iter()
            .all(|event| !event.starts_with("ui "))
    );
}

#[test]
fn scene_canvases_open_before_on_start_with_component_visibility_and_layer() {
    let objects = "[[objects]]\nid = \"hud\"\n[objects.ui_canvas]\ndocument = \"/ui/menu.rveui\"\n\
         visible = false\nlayer = 9\n[[objects.lite_scripts]]\npath = \"/hud.lua\"\n\
         [[objects]]\nid = \"bad\"\n[objects.ui_canvas]\ndocument = \"/ui/missing.rveui\"\n";
    let source_assets = "[[assets]]\npath = \"/ui/missing.rveui\"\n";
    let mut world = scene(&format!("{source_assets}{objects}"), &["hud.lua"]);
    let mut host = load(
        &world,
        &[(
            "hud.lua",
            r#"
local Hud = {}
function Hud:on_start()
  local canvas = rve.ui.canvas(self.object)
  rve.log_info("start " .. tostring(canvas:visible()) .. " " .. canvas:layer())
end
return Hud
"#,
        )],
    );
    host.start(&mut world, ScriptTime::default());
    let events = messages(&mut host);
    assert!(events.contains(&"start false 9".to_owned()), "{events:?}");
    assert!(
        events
            .iter()
            .any(|event| event.contains("не зарегистрирован")),
        "{events:?}"
    );
    let canvas = component(&host, &world, "hud");
    assert_eq!(host.ui().canvas_visible(canvas), Ok(false));
    assert_eq!(host.ui().layer(canvas), Ok(9));
    assert!(world.handle("bad").is_some());
    assert_eq!(open_canvases(&host), vec![canvas]);
}

/// Мир без объектов сцены и хост с зарегистрированным префабом `popup`; старт уже выполнен.
fn popup_host() -> (World, ScriptHost) {
    let mut world = scene("", &[]);
    let mut catalog = PrefabCatalog::default();
    catalog.insert(
        PREFAB,
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("popup.lua", POPUP_LUA)]),
        None,
        ScriptLoadOptions {
            prefabs: catalog,
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap();
    register_menu(&mut host);
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(PREFAB_TEXT.to_owned()) };
    let template =
        Arc::new(PrefabTemplate::load(&VirtualPath::parse(PREFAB).unwrap(), &mut read).unwrap());
    host.register_prefab(
        &world,
        PREFAB,
        template,
        &sources(&[("popup.lua", POPUP_LUA)]),
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    (world, host)
}

fn spawn(host: &mut ScriptHost, world: &mut World, id: &str) -> engine_scene::ObjectHandle {
    host.spawn_external(
        world,
        PREFAB,
        SpawnOptions {
            id: Some(id.to_owned()),
            ..SpawnOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn spawned_object_canvas_opens_and_despawn_closes_it_with_script_opened_canvases() {
    let (mut world, mut host) = popup_host();
    let popup = spawn(&mut host, &mut world, "popup");
    // on_start спавненного объекта выполняется в следующем update.
    host.update(&mut world, 0.1, ScriptTime::default());
    let owner = owner_key(popup);
    let owned: Vec<_> = open_canvases(&host)
        .into_iter()
        .filter(|canvas| host.ui().owner(*canvas) == Ok(owner))
        .collect();
    assert_eq!(owned.len(), 2, "component canvas and script-opened canvas");

    assert!(host.despawn_external(&mut world, popup));
    assert!(open_canvases(&host).is_empty());
    for canvas in owned {
        assert!(host.ui().layer(canvas).is_err());
    }
}

#[test]
fn reverted_spawn_closes_its_canvases_and_reused_slot_does_not_see_them() {
    let (mut world, mut host) = popup_host();
    let first = spawn(&mut host, &mut world, "first");
    let old_canvas = component(&host, &world, "first");
    assert!(host.ui().layer(old_canvas).is_ok());

    host.revert_spawn(&mut world, first, "test rollback");
    assert!(host.ui().layer(old_canvas).is_err());
    assert!(open_canvases(&host).is_empty());

    let second = spawn(&mut host, &mut world, "second");
    let new_canvas = component(&host, &world, "second");
    assert_ne!(new_canvas, old_canvas);
    assert!(host.ui().layer(old_canvas).is_err());
    assert_eq!(host.ui().owner(new_canvas), Ok(owner_key(second)));
    assert_ne!(owner_key(first), owner_key(second));
}
