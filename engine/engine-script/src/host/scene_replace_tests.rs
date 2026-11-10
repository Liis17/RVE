use super::*;
use engine_scene::{SceneDocument, World};
use std::collections::BTreeMap;

const KEEPER: &str = r#"
local Script = {}
Script.properties = {
  speed = { type = "float", default = 2, min = 0, max = 10 },
  target = { type = "object" },
}
function Script:on_start()
  self.count = 0
  rve.start_coroutine(function()
    rve.wait_frames(3)
    rve.log("keeper coroutine done " .. tostring(self.count))
  end)
end
function Script:on_update(dt)
  self.count = self.count + 1
  rve.log("keeper update " .. self.count)
end
function Script:on_destroy()
  rve.log("keeper destroy")
end
return Script
"#;

const OLD: &str = r#"
local Script = {}
function Script:on_start()
  rve.start_coroutine(function()
    rve.wait_frames(3)
    rve.log("old coroutine")
  end)
end
function Script:on_update(dt)
  rve.log("old update")
end
function Script:on_destroy()
  rve.log("old destroy")
end
return Script
"#;

const NEW: &str = r#"
local Script = {}
function Script:on_start()
  rve.log("new start")
end
function Script:on_update(dt)
  rve.log("new update")
end
return Script
"#;

/// Время кадра для `update`: счётчик кадров нужен `wait_frames`.
fn at_frame(frame: u64) -> ScriptTime {
    ScriptTime {
        elapsed: frame as f64 / 60.0,
        frame,
    }
}

fn sources() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("keeper.lua".to_owned(), KEEPER.to_owned()),
        ("old.lua".to_owned(), OLD.to_owned()),
        ("new.lua".to_owned(), NEW.to_owned()),
    ])
}

fn old_scene(extra_keeper_params: &str) -> World {
    old_scene_with_keeper(extra_keeper_params, false)
}

fn old_static_scene() -> World {
    old_scene_with_keeper("", true)
}

fn old_scene_with_keeper(extra_keeper_params: &str, static_keeper: bool) -> World {
    let keeper_fields = if static_keeper {
        "static = true\n[objects.transform]\nposition = [1.0, 2.0, 3.0]\n"
    } else {
        ""
    };
    let document = SceneDocument::parse(
        "old.rvescene",
        &format!(
            concat!(
                "[[assets]]\npath = \"/keeper.lua\"\n[[assets]]\npath = \"/old.lua\"\n",
                "[[objects]]\nid = \"keeper\"\nkeep_on_load = true\n{}\n",
                "[[objects.lite_scripts]]\npath = \"/keeper.lua\"\n{}",
                "[[objects]]\nid = \"old\"\n[[objects.lite_scripts]]\npath = \"/old.lua\"\n",
                "[[objects]]\nid = \"cam_old\"\n[objects.camera]\nactive = true\n",
            ),
            keeper_fields, extra_keeper_params
        ),
    )
    .unwrap();
    World::from_document(&document).unwrap()
}

fn new_scene(extra_script: &str) -> World {
    new_scene_with_static(extra_script, false)
}

fn new_scene_with_static(extra_script: &str, include_static: bool) -> World {
    let static_object = if include_static {
        "[[objects]]\nid = \"replacement_static\"\nstatic = true\n"
    } else {
        ""
    };
    let document = SceneDocument::parse(
        "new.rvescene",
        &format!(
            concat!(
                "[[assets]]\npath = \"/new.lua\"\n",
                "{}",
                "[[objects]]\nid = \"new\"\n[[objects.lite_scripts]]\npath = \"/new.lua\"\n{}",
                "[[objects]]\nid = \"cam_new\"\n[objects.camera]\nactive = true\n",
            ),
            static_object, extra_script
        ),
    )
    .unwrap();
    World::from_document(&document).unwrap()
}

fn load(world: &World) -> ScriptHost {
    ScriptHost::load(world, "old", &sources(), None).unwrap()
}

fn messages(host: &mut ScriptHost) -> Vec<String> {
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

#[test]
fn scene_switch_keeps_persistent_instance_state_and_destroys_old_scripts_once() {
    let mut world = old_static_scene();
    let keeper = world.handle("keeper").unwrap();
    let old = world.handle("old").unwrap();
    let keeper_position = glam::Vec3::new(1.0, 2.0, 3.0);
    assert_eq!(world.is_static(keeper), Some(true));
    assert_eq!(world.world_position(keeper), Some(keeper_position));
    let mut host = load(&world);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    assert!(messages(&mut host).contains(&"keeper update 1".to_owned()));

    host.replace_scene(
        &mut world,
        new_scene_with_static("", true),
        &sources(),
        "new",
        ScriptTime::default(),
    )
    .unwrap();
    let replaced = messages(&mut host);
    assert_eq!(
        replaced.iter().filter(|m| *m == "old destroy").count(),
        1,
        "{replaced:?}"
    );
    assert!(
        !replaced.contains(&"keeper destroy".to_owned()),
        "{replaced:?}"
    );

    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let after = messages(&mut host);
    assert!(after.contains(&"new start".to_owned()), "{after:?}");
    assert!(after.contains(&"new update".to_owned()), "{after:?}");
    assert!(after.contains(&"keeper update 2".to_owned()), "{after:?}");
    assert!(!after.contains(&"old update".to_owned()), "{after:?}");

    assert_eq!(world.handle("keeper"), Some(keeper));
    assert_eq!(world.is_static(keeper), Some(true));
    assert_eq!(world.world_position(keeper), Some(keeper_position));
    assert!(!world.is_alive(old));
    assert!(world.handle("new").is_some());
    assert_eq!(
        world.is_static(world.handle("replacement_static").unwrap()),
        Some(true)
    );
    assert_eq!(world.active_camera(), world.handle("cam_new").unwrap());
}

#[test]
fn persistent_coroutine_finishes_after_switch_and_old_coroutine_is_cancelled() {
    let mut world = old_scene("");
    let mut host = load(&world);
    host.update(&mut world, 1.0 / 60.0, at_frame(1));
    host.replace_scene(&mut world, new_scene(""), &sources(), "new", at_frame(1))
        .unwrap();

    let mut seen = Vec::new();
    for frame in 2..=8 {
        host.update(&mut world, 1.0 / 60.0, at_frame(frame));
        seen.extend(messages(&mut host));
    }

    assert!(
        seen.iter()
            .any(|message| message.starts_with("keeper coroutine done")),
        "{seen:?}"
    );
    assert!(!seen.contains(&"old coroutine".to_owned()), "{seen:?}");
}

#[test]
fn failed_scene_switch_leaves_the_current_scene_running() {
    let mut world = old_scene("");
    let old = world.handle("old").unwrap();
    let mut host = load(&world);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    messages(&mut host);
    let unregistered = new_scene("[[objects.heavy_scripts]]\nname = \"NoSuchScript\"\n");

    let error = host
        .replace_scene(
            &mut world,
            unregistered,
            &sources(),
            "new",
            ScriptTime::default(),
        )
        .unwrap_err();

    assert!(error.contains("NoSuchScript"), "{error}");
    assert!(world.is_alive(old));
    assert!(world.handle("new").is_none());
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let after = messages(&mut host);
    assert!(after.contains(&"old update".to_owned()), "{after:?}");
    assert!(!after.iter().any(|message| message == "old destroy"));
}

#[test]
fn persistent_reference_to_a_nonpersistent_object_rejects_the_switch_before_teardown() {
    let mut world = old_scene("params = { target = { object = \"old\" } }\n");
    let old = world.handle("old").unwrap();
    let mut host = load(&world);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    messages(&mut host);

    let error = host
        .replace_scene(
            &mut world,
            new_scene(""),
            &sources(),
            "new",
            ScriptTime::default(),
        )
        .unwrap_err();

    assert!(error.contains("\"old\""), "{error}");
    assert!(world.is_alive(old));
    assert!(messages(&mut host).is_empty());
}

#[test]
fn stale_reference_to_a_destroyed_instance_reports_a_defined_error_after_switch() {
    let mut world = World::from_document(
        &SceneDocument::parse(
            "stale.rvescene",
            concat!(
                "[[assets]]\npath = \"/keeper.lua\"\n[[assets]]\npath = \"/old.lua\"\n",
                "[[assets]]\npath = \"/probe.lua\"\n",
                "[[objects]]\nid = \"probe\"\nkeep_on_load = true\n",
                "[[objects.lite_scripts]]\npath = \"/probe.lua\"\n",
                "[[objects]]\nid = \"keeper\"\nkeep_on_load = true\n",
                "[[objects.lite_scripts]]\npath = \"/keeper.lua\"\n",
                "[[objects]]\nid = \"old\"\n[[objects.lite_scripts]]\npath = \"/old.lua\"\n",
                "[[objects]]\nid = \"cam_old\"\n[objects.camera]\nactive = true\n",
            ),
        )
        .unwrap(),
    )
    .unwrap();
    let probe = r#"
local Script = {}
local held
local calls = 0
function Script:on_update()
  calls = calls + 1
  if calls == 1 then
    held = rve.find("old"):script("/old.lua")
  elseif calls == 2 then
    rve.log("held enabled " .. tostring(held:enabled()))
  end
end
return Script
"#;
    let mut sources = sources();
    sources.insert("probe.lua".to_owned(), probe.to_owned());
    let mut host = ScriptHost::load(&world, "old", &sources, None).unwrap();
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());
    let switch = new_scene("");
    host.replace_scene(&mut world, switch, &sources, "new", ScriptTime::default())
        .unwrap();
    messages(&mut host);
    host.update(&mut world, 1.0 / 60.0, ScriptTime::default());

    let events = host.drain_events();
    assert!(
        events
            .iter()
            .any(|event| event.message.contains("no longer alive")),
        "{:?}",
        events.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

const GATED: &str = r#"
local Script = {}
Script.properties = { target = { type = "object" } }
return Script
"#;

#[test]
fn instance_creation_failure_in_the_new_scene_rolls_back_without_destroying_old_scripts() {
    let mut world = old_scene("");
    let old = world.handle("old").unwrap();
    let mut host = load(&world);
    host.update(&mut world, 1.0 / 60.0, at_frame(1));
    messages(&mut host);
    let document = SceneDocument::parse(
        "gated.rvescene",
        concat!(
            "[[assets]]\npath = \"/gated.lua\"\n",
            "[[objects]]\nid = \"gated\"\n",
            "[[objects.lite_scripts]]\npath = \"/gated.lua\"\nparams = { target = { object = \"nowhere\" } }\n",
            "[[objects]]\nid = \"cam_new\"\n[objects.camera]\nactive = true\n",
        ),
    )
    .unwrap();
    let gated = World::from_document(&document).unwrap();
    let mut sources = sources();
    sources.insert("gated.lua".to_owned(), GATED.to_owned());

    let error = host
        .replace_scene(&mut world, gated, &sources, "gated", at_frame(1))
        .unwrap_err();

    assert!(error.contains("nowhere"), "{error}");
    assert!(world.is_alive(old));
    assert!(world.handle("gated").is_none());
    host.update(&mut world, 1.0 / 60.0, at_frame(2));
    let after = messages(&mut host);
    assert!(after.contains(&"old update".to_owned()), "{after:?}");
    assert!(
        !after.iter().any(|message| message == "old destroy"),
        "{after:?}"
    );
}

const DRIVER: &str = r#"
local Script = {}
local calls = 0
function Script:on_update()
  calls = calls + 1
  if calls == 1 then
    rve.log("first current " .. rve.scene.current())
    local ok = pcall(function() rve.scene.load("/levels/forbidden.rvescene") end)
    rve.log("forbidden accepted " .. tostring(ok))
    rve.scene.load("levels/next.rvescene")
  else
    rve.log("later current " .. rve.scene.current())
  end
end
return Script
"#;

fn driver_world() -> World {
    World::from_document(
        &SceneDocument::parse(
            "old.rvescene",
            concat!(
                "[[assets]]\npath = \"/driver.lua\"\n",
                "[[objects]]\nid = \"driver\"\nkeep_on_load = true\n",
                "[[objects.lite_scripts]]\npath = \"/driver.lua\"\n",
                "[[objects]]\nid = \"cam_old\"\n[objects.camera]\nactive = true\n",
            ),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn lua_scene_load_is_validated_and_applied_after_the_frame() {
    let mut world = driver_world();
    let mut sources = sources();
    sources.insert("driver.lua".to_owned(), DRIVER.to_owned());
    let mut host = ScriptHost::load(&world, "old.rvescene", &sources, None).unwrap();
    host.set_enabled_scenes(&["levels/next.rvescene".to_owned()])
        .unwrap();

    host.update(&mut world, 1.0 / 60.0, at_frame(1));

    let first = messages(&mut host);
    assert!(
        first.contains(&"first current old.rvescene".to_owned()),
        "{first:?}"
    );
    assert!(
        first.contains(&"forbidden accepted false".to_owned()),
        "{first:?}"
    );
    assert_eq!(
        host.take_scene_request().as_deref(),
        Some("levels/next.rvescene")
    );
    assert_eq!(host.take_scene_request(), None);
}

#[test]
fn lua_app_quit_is_committed_only_after_a_successful_callback() {
    let mut world = driver_world();
    let mut script_sources = sources();
    script_sources.insert(
        "driver.lua".to_owned(),
        r#"
local Script = {}
function Script:on_update()
  rve.app.quit()
  error("fail after quit")
end
return Script
"#
        .to_owned(),
    );
    let mut host = ScriptHost::load(&world, "old.rvescene", &script_sources, None).unwrap();

    host.update(&mut world, 1.0 / 60.0, at_frame(1));

    assert!(!host.take_quit_request());
    assert!(
        host.drain_events()
            .iter()
            .any(|event| event.message.contains("fail after quit"))
    );

    let mut successful_world = driver_world();
    script_sources.insert(
        "driver.lua".to_owned(),
        "return { on_update = function() rve.app.quit() end }".to_owned(),
    );
    let mut successful_host =
        ScriptHost::load(&successful_world, "old.rvescene", &script_sources, None).unwrap();
    successful_host.update(&mut successful_world, 1.0 / 60.0, at_frame(1));
    assert!(successful_host.take_quit_request());
    assert!(!successful_host.take_quit_request());
}

#[test]
fn lua_app_quit_is_rejected_from_the_top_level_chunk() {
    let world = driver_world();
    let mut script_sources = sources();
    script_sources.insert(
        "driver.lua".to_owned(),
        "rve.app.quit(); return {}".to_owned(),
    );

    let error = match ScriptHost::load(&world, "old.rvescene", &script_sources, None) {
        Ok(_) => panic!("top-level quit request should be rejected"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("quit can only be requested"));
}

#[test]
fn applied_scene_request_updates_current_and_failed_one_keeps_it() {
    let mut world = driver_world();
    let mut sources = sources();
    sources.insert("driver.lua".to_owned(), DRIVER.to_owned());
    let mut host = ScriptHost::load(&world, "old.rvescene", &sources, None).unwrap();
    host.set_enabled_scenes(&["levels/next.rvescene".to_owned()])
        .unwrap();
    host.update(&mut world, 1.0 / 60.0, at_frame(1));
    messages(&mut host);
    let requested = host.take_scene_request().unwrap();

    let failing = World::from_document(
        &SceneDocument::parse(
            "next.rvescene",
            concat!(
                "[[objects]]\nid = \"bad\"\n[[objects.heavy_scripts]]\nname = \"NoSuchScript\"\n",
                "[[objects]]\nid = \"cam_new\"\n[objects.camera]\nactive = true\n",
            ),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        host.replace_scene(&mut world, failing, &sources, &requested, at_frame(2))
            .is_err()
    );
    host.update(&mut world, 1.0 / 60.0, at_frame(2));
    assert!(messages(&mut host).contains(&"later current old.rvescene".to_owned()));

    let next = World::from_document(
        &SceneDocument::parse(
            "next.rvescene",
            "[[objects]]\nid = \"cam_new\"\n[objects.camera]\nactive = true\n",
        )
        .unwrap(),
    )
    .unwrap();
    host.replace_scene(&mut world, next, &sources, &requested, at_frame(3))
        .unwrap();
    host.update(&mut world, 1.0 / 60.0, at_frame(3));
    assert!(messages(&mut host).contains(&"later current levels/next.rvescene".to_owned()));
}

#[test]
fn undeclared_scene_paths_are_rejected_by_the_host() {
    let mut sources = sources();
    sources.insert("driver.lua".to_owned(), DRIVER.to_owned());
    let mut world = driver_world();
    let mut host = ScriptHost::load(&world, "old.rvescene", &sources, None).unwrap();
    host.set_enabled_scenes(&["levels/next.rvescene".to_owned()])
        .unwrap();

    assert!(
        host.set_enabled_scenes(&["../escape.rvescene".to_owned()])
            .is_err()
    );
    host.update(&mut world, 1.0 / 60.0, at_frame(1));
    assert_eq!(
        host.take_scene_request().as_deref(),
        Some("levels/next.rvescene")
    );
    assert_eq!(host.take_scene_request(), None);
}

/// Сохраняемый игрок: его `on_destroy` не должен вызываться, а объект должен пережить замену.
const TEARDOWN_PLAYER: &str = r#"
local Script = {}
function Script:on_update(dt)
  rve.log("player update")
end
function Script:on_destroy()
  rve.log("player destroy")
end
return Script
"#;

/// Удаляемый объект делает структурную правку сохраняемой части в `on_disable` или `on_destroy`.
const TEARDOWN_MUTATOR: &str = r#"
local Script = {}
function Script:on_disable()
  DISABLE_BODY
end
function Script:on_destroy()
  DESTROY_BODY
  rve.log("doomed destroy")
end
return Script
"#;

/// Запускает замену сцены, в которой удаляемый объект выполняет `body` в `callback`.
/// Возвращает мир, хост, сообщения хоста и исход замены.
fn teardown_mutation(
    callback: &str,
    body: &str,
) -> (
    World,
    ScriptHost,
    Vec<String>,
    Result<SceneReplacement, String>,
) {
    let (disable, destroy) = match callback {
        "on_disable" => (body, ""),
        "on_destroy" => ("", body),
        other => panic!("unknown callback {other}"),
    };
    let mutator = TEARDOWN_MUTATOR
        .replace("DISABLE_BODY", disable)
        .replace("DESTROY_BODY", destroy);
    let mut sources = sources();
    sources.insert("player.lua".to_owned(), TEARDOWN_PLAYER.to_owned());
    sources.insert("mutator.lua".to_owned(), mutator);
    let document = SceneDocument::parse(
        "old.rvescene",
        concat!(
            "[[assets]]\npath = \"/mutator.lua\"\n[[assets]]\npath = \"/player.lua\"\n",
            "[[objects]]\nid = \"player\"\nkeep_on_load = true\n",
            "[[objects.lite_scripts]]\npath = \"/player.lua\"\n",
            "[[objects]]\nid = \"doomed\"\n[[objects.lite_scripts]]\npath = \"/mutator.lua\"\n",
            "[[objects]]\nid = \"cam_old\"\n[objects.camera]\nactive = true\n",
        ),
    )
    .unwrap();
    let mut world = World::from_document(&document).unwrap();
    let mut host = ScriptHost::load(&world, "old", &sources, None).unwrap();
    host.start(&mut world, at_frame(1));
    host.update(&mut world, 1.0 / 60.0, at_frame(1));
    let mut events = messages(&mut host);
    let result = host.replace_scene(&mut world, new_scene(""), &sources, "new", at_frame(2));
    events.extend(messages(&mut host));
    (world, host, events, result)
}

#[test]
fn structural_edits_of_persistent_objects_are_refused_in_teardown_callbacks() {
    let bodies = [
        (
            "set_parent under the doomed object",
            "rve.log(\"mutation \" .. tostring(rve.find(\"player\"):set_parent(self.object, true)))",
        ),
        (
            "set_keep_on_load false on the persistent player",
            "rve.log(\"mutation \" .. tostring(rve.find(\"player\"):set_keep_on_load(false)))",
        ),
        (
            "set_keep_on_load true on the doomed root",
            "rve.log(\"mutation \" .. tostring(self.object:set_keep_on_load(true)))",
        ),
    ];
    for callback in ["on_disable", "on_destroy"] {
        for (name, body) in bodies {
            let (mut world, mut host, events, result) = teardown_mutation(callback, body);
            let context = format!("{callback}: {name}");
            result.unwrap_or_else(|error| panic!("{context}: replacement failed: {error}"));
            assert_eq!(
                events.iter().filter(|m| *m == "mutation false").count(),
                1,
                "{context}: the edit is refused as a rejected call: {events:?}"
            );
            let player = world.handle("player");
            assert!(player.is_some(), "{context}: persistent player survives");
            let player = player.unwrap();
            assert_eq!(world.object(player).unwrap().parent, None, "{context}");
            assert_eq!(world.keep_on_load(player), Some(true), "{context}");
            assert!(
                world.handle("doomed").is_none(),
                "{context}: the doomed object is removed"
            );
            assert_eq!(
                events.iter().filter(|m| *m == "doomed destroy").count(),
                1,
                "{context}: {events:?}"
            );
            assert!(
                !events.contains(&"player destroy".to_owned()),
                "{context}: {events:?}"
            );
            host.update(&mut world, 1.0 / 60.0, at_frame(3));
            assert!(
                messages(&mut host).contains(&"player update".to_owned()),
                "{context}: the persistent instance keeps running"
            );
        }
    }
}

#[test]
fn scene_prefab_conflicting_with_a_heavy_project_prefab_changes_no_prefab_state() {
    let world = old_scene("");
    let mut catalog = PrefabCatalog::default();
    catalog.insert(
        "prefabs/heavy.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Heavy,
            max_instances: None,
        },
    );
    let mut host = ScriptHost::load_with_options(
        &world,
        "old",
        &sources(),
        None,
        ScriptLoadOptions {
            prefabs: catalog,
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap();
    let path = VirtualPath::parse("prefabs/fresh.rveprefab").unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> {
        Ok("[[objects]]\nid = \"root\"\n".to_owned())
    };
    let template = Arc::new(PrefabTemplate::load(&path, &mut read).unwrap());
    let mut scene = PrefabCatalog::default();
    scene.insert(
        "prefabs/fresh.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    scene.insert(
        "prefabs/heavy.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let templates = BTreeMap::from([
        ("prefabs/fresh.rveprefab".to_owned(), Arc::clone(&template)),
        ("prefabs/heavy.rveprefab".to_owned(), template),
    ]);

    let error = host
        .check_scene_prefabs(&world, &scene, &templates, &sources())
        .err()
        .expect("a fast declaration of a heavy project prefab is refused");

    assert!(error.to_string().contains("heavy"), "{error}");
    assert_eq!(host.prefab_status("prefabs/fresh.rveprefab"), None);
    assert_eq!(
        host.prefab_status("prefabs/heavy.rveprefab"),
        Some(PrefabStatus::Unloaded)
    );
}
