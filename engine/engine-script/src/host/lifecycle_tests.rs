use super::{ScriptHost, ScriptLoadError};
use crate::lifecycle::{PrefabCatalog, PrefabKind, PrefabRequest, PrefabSpec, PrefabStatus};
use crate::{ScriptEventLevel, ScriptLoadOptions, ScriptTime};
use engine_files::VirtualPath;
use engine_scene::{PrefabTemplate, SceneDocument, World};
use std::collections::BTreeMap;
use std::sync::Arc;

const PREFAB: &str = "prefabs/bullet.rveprefab";
const PREFAB_TEXT: &str = "[[assets]]\npath = \"/bullet.lua\"\n\
[[objects]]\nid = \"root\"\n[[objects.lite_scripts]]\npath = \"/bullet.lua\"\n\
[[objects]]\nid = \"tip\"\nparent = \"root\"\n[objects.transform]\nposition = [0.0, 1.0, 0.0]\n";
const DYNAMIC_TEXT: &str =
    "[[objects]]\nid = \"root\"\n[objects.collider]\n[objects.rigid_body]\nkind = \"dynamic\"\n";
const TWO_GUNS: &str = "[[assets]]\npath = \"/gun.lua\"\n[[objects]]\nid = \"gun\"\n[[objects.lite_scripts]]\npath = \"/gun.lua\"\n[[objects]]\nid = \"gun2\"\n[[objects.lite_scripts]]\npath = \"/gun.lua\"\n";
const GUN_SCENE: &str = "[[assets]]\npath = \"/gun.lua\"\n[[objects]]\nid = \"gun\"\n\
[[objects.lite_scripts]]\npath = \"/gun.lua\"\n";

const BULLET_LUA: &str = r#"
local Bullet = {}
function Bullet:on_start()
  rve.log_info("start " .. self.object.id)
  rve.console.register("bullet_cmd", function() rve.log_info("bullet command") end, "")
end
function Bullet:on_message(sender, name, value) rve.log_info("message " .. name) end
function Bullet:on_update(dt) rve.log_info("update " .. self.object.id) end
function Bullet:on_destroy() rve.log_info("destroy " .. self.object.id) end
return Bullet
"#;

fn scene(source: &str) -> World {
    let source = format!("{source}\n[[objects]]\nid = \"__test_camera\"\n[objects.camera]\n\n");
    World::from_document(&SceneDocument::parse("test.rvescene", &source).unwrap()).unwrap()
}

fn template(path: &str, text: &str) -> Arc<PrefabTemplate> {
    let path = VirtualPath::parse(path).unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(text.to_owned()) };
    Arc::new(PrefabTemplate::load(&path, &mut read).unwrap())
}

fn sources(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
        .collect()
}

fn catalog(kind: PrefabKind, max_instances: Option<u32>) -> PrefabCatalog {
    let mut prefabs = PrefabCatalog::default();
    prefabs.insert(
        PREFAB,
        PrefabSpec {
            kind,
            max_instances,
        },
    );
    prefabs
}

fn load(world: &World, scripts: &[(&str, &str)], prefabs: PrefabCatalog) -> ScriptHost {
    ScriptHost::load_with_options(
        world,
        "scene",
        &sources(scripts),
        None,
        ScriptLoadOptions {
            prefabs,
            ..ScriptLoadOptions::default()
        },
    )
    .unwrap()
}

/// Загрузка сцены с одним быстрым префабом «bullet», уже зарегистрированным.
fn fast_host(world: &World, gun: &str, max_instances: Option<u32>) -> ScriptHost {
    let mut host = load(
        world,
        &[("gun.lua", gun), ("bullet.lua", BULLET_LUA)],
        catalog(PrefabKind::Fast, max_instances),
    );
    host.register_prefab(
        world,
        PREFAB,
        template(PREFAB, PREFAB_TEXT),
        &sources(&[("bullet.lua", BULLET_LUA)]),
    )
    .unwrap();
    host
}

fn at(elapsed: f64, frame: u64) -> ScriptTime {
    ScriptTime { elapsed, frame }
}

fn infos(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Info)
        .map(|event| event.message)
        .collect()
}

fn errors(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Error)
        .map(|event| event.message)
        .collect()
}

fn warnings(host: &mut ScriptHost) -> Vec<String> {
    host.drain_events()
        .into_iter()
        .filter(|event| event.level == ScriptEventLevel::Warn)
        .map(|event| event.message)
        .collect()
}

const GUN_FIRST: &str = r#"
local Gun = {}
local fired = false
function Gun:on_update(dt)
  if not fired then
    fired = true
    rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
  end
end
return Gun
"#;

#[test]
fn spawned_object_starts_on_the_next_update_before_messages() {
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, GUN_FIRST, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    assert!(world.handle("shot").is_some());
    assert!(infos(&mut host).is_empty());
    let changes = host.drain_world_changes();
    assert_eq!(changes.spawned.len(), 1);
    assert_eq!(changes.spawned[0].prefab, PREFAB);
    assert_eq!(changes.spawned[0].objects.len(), 2);

    host.update(&mut world, 0.1, at(0.1, 2));
    assert_eq!(infos(&mut host), ["start shot", "update shot"]);
}

#[test]
fn pending_objects_are_hidden_from_find_but_accept_messages() {
    let gun = r#"
local Gun = {}
local fired = false
function Gun:on_update(dt)
  if fired then return end
  fired = true
  local shot = rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
  rve.log_info("find " .. tostring(rve.find("shot")))
  rve.log_info("pending " .. tostring(shot:is_pending()) .. " alive " .. tostring(shot:is_alive()))
  rve.send("shot", "hit", 7)
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    assert_eq!(infos(&mut host), ["find nil", "pending true alive false"]);
    host.update(&mut world, 0.1, at(0.1, 2));
    assert_eq!(
        infos(&mut host),
        ["start shot", "message hit", "update shot"]
    );
}

#[test]
fn failing_callback_rolls_back_its_spawn_and_releases_the_id() {
    let gun = r#"
local Gun = {}
function Gun:on_update(dt)
  self.calls = (self.calls or 0) + 1
  rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
  if self.calls == 1 and self.object.id == "gun" then error("boom") end
end
return Gun
"#;
    let mut world = scene(TWO_GUNS);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("boom"));
    // The failed gun's reservation was released, so the second gun could take the id.
    assert!(world.handle("shot").is_some());
    assert_eq!(host.drain_world_changes().spawned.len(), 1);
}

#[test]
fn explicit_ids_collide_within_one_callback_and_with_live_objects() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    rve.log_info("dup " .. tostring(pcall(function()
      rve.spawn("prefabs/bullet.rveprefab", { id = "a" })
      rve.spawn("prefabs/bullet.rveprefab", { id = "a" })
    end)))
  elseif step == 2 then
    rve.log_info("live " .. tostring(pcall(rve.spawn, "prefabs/bullet.rveprefab", { id = "a" })))
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    assert_eq!(infos(&mut host), ["dup false"]);
    // Only the first spawn of the duplicated callback exists.
    assert!(world.handle("a").is_some());
    assert_eq!(host.drain_world_changes().spawned.len(), 1);
    host.update(&mut world, 0.1, at(0.1, 2));
    assert!(infos(&mut host).contains(&"live false".to_owned()));
    assert!(world.handle("a").is_some());
}

#[test]
fn spawn_and_destroy_are_refused_in_top_level_chunks_and_on_destroy() {
    let mut world = scene(GUN_SCENE);
    let top_level = r#"rve.spawn("prefabs/bullet.rveprefab", { id = "early" }) return {}"#;
    let error = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[("gun.lua", top_level)]),
        None,
        ScriptLoadOptions {
            prefabs: catalog(PrefabKind::Fast, None),
            ..ScriptLoadOptions::default()
        },
    )
    .err()
    .expect("top-level spawn fails");
    assert!(matches!(error, ScriptLoadError::Lite { .. }));
    assert!(error.to_string().contains("creation"));

    let gun = r#"
local Gun = {}
function Gun:on_destroy()
  rve.spawn("prefabs/bullet.rveprefab", { id = "late" })
end
return Gun
"#;
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    host.destroy(&mut world, at(0.1, 2));
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("destroy callbacks"));
    assert!(world.handle("late").is_none());
    assert!(host.drain_world_changes().spawned.is_empty());
}

#[test]
fn velocity_requires_a_dynamic_root_and_is_reported_for_dynamic_roots() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    rve.spawn("prefabs/static.rveprefab", { id = "still", velocity = { x = 1, y = 0, z = 0 } })
  elseif step == 2 then
    rve.spawn("prefabs/dyn.rveprefab", { id = "moving", velocity = { x = 2, y = 0, z = 0 } })
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut prefabs = catalog(PrefabKind::Fast, None);
    prefabs.insert(
        "prefabs/static.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    prefabs.insert(
        "prefabs/dyn.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let mut host = load(&world, &[("gun.lua", gun)], prefabs);
    host.register_prefab(
        &world,
        "prefabs/static.rveprefab",
        template("prefabs/static.rveprefab", PREFAB_TEXT),
        &sources(&[("bullet.lua", BULLET_LUA)]),
    )
    .unwrap();
    host.register_prefab(
        &world,
        "prefabs/dyn.rveprefab",
        template("prefabs/dyn.rveprefab", DYNAMIC_TEXT),
        &sources(&[]),
    )
    .unwrap();
    host.update(&mut world, 0.1, at(0.0, 1));
    assert!(world.handle("still").is_none());
    assert!(errors(&mut host)[0].contains("dynamic rigid body"));
    host.update(&mut world, 0.1, at(0.1, 2));
    assert!(world.handle("moving").is_some());
    let changes = host.drain_world_changes();
    assert_eq!(changes.spawned.len(), 1);
    assert_eq!(
        changes.spawned[0].initial_velocity,
        Some(glam::Vec3::new(2.0, 0.0, 0.0))
    );
}

#[test]
fn destroying_a_subtree_runs_on_destroy_and_drops_commands_and_messages() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
  elseif step == 2 then
    rve.destroy("shot")
    rve.send("shot", "late", 1)
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    host.update(&mut world, 0.1, at(0.1, 2));
    assert_eq!(
        infos(&mut host),
        ["start shot", "update shot", "destroy shot"]
    );
    assert!(world.handle("shot").is_none());
    let changes = host.drain_world_changes();
    assert_eq!(changes.despawned.len(), 2);
    assert!(
        host.execute_command(
            &mut world,
            &engine_console::parse_command_line("bullet_cmd").unwrap()
        )
        .is_err()
    );

    host.update(&mut world, 0.1, at(0.2, 3));
    assert!(infos(&mut host).is_empty());
}

#[test]
fn delayed_destroy_uses_the_earlier_deadline() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
    rve.destroy("shot", 2)
  elseif step == 2 then
    rve.destroy("shot", 1)
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    host.update(&mut world, 0.1, at(0.5, 2));
    host.update(&mut world, 0.1, at(1.4, 3));
    assert!(world.handle("shot").is_some());
    host.update(&mut world, 0.1, at(1.5, 4));
    assert!(world.handle("shot").is_none());
}

#[test]
fn max_instances_evicts_the_oldest_root() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step <= 3 then rve.spawn("prefabs/bullet.rveprefab", { id = "b" .. step }) end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, Some(2));
    for frame in 1..=3 {
        host.update(&mut world, 0.1, at(0.1 * frame as f64, frame));
    }
    assert!(world.handle("b1").is_none());
    assert!(world.handle("b2").is_some());
    assert!(world.handle("b3").is_some());
    assert!(infos(&mut host).contains(&"destroy b1".to_owned()));
}

#[test]
fn spawn_requests_per_frame_are_capped() {
    let gun = r#"
local Gun = {}
function Gun:on_update(dt)
  for _ = 1, 4097 do rve.spawn("prefabs/bullet.rveprefab") end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("limit"));
    assert!(host.drain_world_changes().spawned.is_empty());
}

#[test]
fn spawned_object_count_per_frame_is_capped_at_apply() {
    let gun = r#"
local Gun = {}
function Gun:on_update(dt)
  for _ = 1, 2100 do rve.spawn("prefabs/pair.rveprefab") end
end
return Gun
"#;
    const PAIR: &str =
        "[[objects]]\nid = \"root\"\n[[objects]]\nid = \"child\"\nparent = \"root\"\n";
    let mut world = scene(GUN_SCENE);
    let mut prefabs = PrefabCatalog::default();
    prefabs.insert(
        "prefabs/pair.rveprefab",
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let mut host = load(&world, &[("gun.lua", gun)], prefabs);
    host.register_prefab(
        &world,
        "prefabs/pair.rveprefab",
        template("prefabs/pair.rveprefab", PAIR),
        &sources(&[]),
    )
    .unwrap();
    host.update(&mut world, 0.1, at(0.0, 1));
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 2100 - 2048);
    assert!(errors.iter().all(|message| message.contains("limit")));
    assert_eq!(host.drain_world_changes().spawned.len(), 2048);
}

#[test]
fn destroyed_objects_read_nil_write_false_and_warn_once() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    self.held = rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
    rve.destroy("shot")
  elseif step == 2 then
    rve.log_info("alive " .. tostring(self.held:is_alive()))
    rve.log_info("get " .. tostring(self.held:get_transform()))
    rve.log_info("set " .. tostring(self.held:set_camera_settings({})))
    rve.log_info("activate " .. tostring(self.held:activate_camera()))
    rve.log_info("destroy " .. tostring(self.held:destroy()))
    rve.log_info("matrix " .. tostring(self.held:world_matrix()))
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    host.update(&mut world, 0.1, at(0.1, 2));
    let events = host.drain_events();
    let warns = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Warn)
        .count();
    let logs = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Info)
        .map(|event| event.message.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        logs,
        [
            "alive false",
            "get nil",
            "set false",
            "activate false",
            "destroy false",
            "matrix nil",
        ]
    );
    assert_eq!(warns, 1);
    host.update(&mut world, 0.1, at(0.2, 3));
    assert!(warnings(&mut host).is_empty());
}

#[test]
fn destroying_the_active_camera_is_rejected() {
    let gun = r#"
local Gun = {}
function Gun:on_update(dt)
  rve.destroy("__test_camera")
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    assert!(errors(&mut host)[0].contains("active camera"));
    assert!(world.handle("__test_camera").is_some());
}

#[test]
fn console_command_spawns_are_applied_at_the_end_of_the_command() {
    let gun = r#"
local Gun = {}
function Gun:on_start()
  rve.console.register("make", function()
    rve.spawn("prefabs/bullet.rveprefab", { id = "made" })
  end, "")
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    host.execute_command(
        &mut world,
        &engine_console::parse_command_line("make").unwrap(),
    )
    .unwrap();
    assert!(world.handle("made").is_some());
}

#[test]
fn thousands_of_spawn_destroy_cycles_reuse_instance_slots() {
    let gun = r#"
local Gun = {}
function Gun:on_update(dt)
  for _ = 1, 1000 do
    local object = rve.spawn("prefabs/bullet.rveprefab")
    object:destroy()
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    for frame in 1..=4 {
        host.update(&mut world, 0.1, at(0.1 * frame as f64, frame));
        assert!(errors(&mut host).is_empty());
    }
    assert_eq!(world.objects().len(), 2);
    assert!(
        host.instances.len() <= 1001,
        "slots: {}",
        host.instances.len()
    );
}

#[test]
fn heavy_prefab_spawn_waits_for_registration() {
    let gun = r#"
local Gun = {}
local fired = false
function Gun:on_update(dt)
  if not fired then
    fired = true
    rve.log_info("state " .. rve.prefabs.state("prefabs/bullet.rveprefab"))
    rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", gun), ("bullet.lua", BULLET_LUA)],
        catalog(PrefabKind::Heavy, None),
    );
    host.update(&mut world, 0.1, at(0.0, 1));
    assert_eq!(infos(&mut host), ["state unloaded"]);
    assert!(world.handle("shot").is_none());
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Load(PREFAB.to_owned())]
    );
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Loading));

    host.register_prefab(
        &world,
        PREFAB,
        template(PREFAB, PREFAB_TEXT),
        &sources(&[("bullet.lua", BULLET_LUA)]),
    )
    .unwrap();
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Loaded));
    host.update(&mut world, 0.1, at(0.1, 2));
    assert!(world.handle("shot").is_some());
    assert_eq!(host.drain_world_changes().spawned.len(), 1);
    host.update(&mut world, 0.1, at(0.2, 3));
    assert_eq!(infos(&mut host), ["start shot", "update shot"]);
}

#[test]
fn failed_heavy_load_kills_waiting_spawns_with_an_error() {
    let gun = r#"
local Gun = {}
local fired = false
function Gun:on_update(dt)
  if not fired then
    fired = true
    rve.spawn("prefabs/bullet.rveprefab", { id = "shot" })
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", gun)],
        catalog(PrefabKind::Heavy, None),
    );
    host.update(&mut world, 0.1, at(0.0, 1));
    host.prefab_failed(&world, PREFAB, "disk error");
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Failed));
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("disk error"));
    assert!(world.handle("shot").is_none());
}

#[test]
fn heavy_prefab_unloads_after_its_last_instance() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then rve.spawn("prefabs/bullet.rveprefab", { id = "shot" }) end
  if step == 2 then rve.prefabs.unload("prefabs/bullet.rveprefab") end
  if step == 3 then rve.destroy("shot") end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", gun), ("bullet.lua", BULLET_LUA)],
        catalog(PrefabKind::Heavy, None),
    );
    host.update(&mut world, 0.1, at(0.0, 1));
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Load(PREFAB.to_owned())]
    );
    host.register_prefab(
        &world,
        PREFAB,
        template(PREFAB, PREFAB_TEXT),
        &sources(&[("bullet.lua", BULLET_LUA)]),
    )
    .unwrap();
    // The queued spawn keeps the unload waiting until the object exists and dies.
    host.update(&mut world, 0.1, at(0.1, 2));
    assert!(world.handle("shot").is_some());
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Loaded));
    assert!(host.drain_world_changes().prefab_requests.is_empty());

    host.update(&mut world, 0.1, at(0.2, 3));
    assert!(world.handle("shot").is_none());
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Unload(PREFAB.to_owned())]
    );
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Unloading));
    host.prefab_released(PREFAB);
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Unloaded));
}

#[test]
fn register_prefab_rejects_unknown_paths_repeats_and_missing_sources() {
    let world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", GUN_FIRST)],
        catalog(PrefabKind::Fast, None),
    );
    let text = template(PREFAB, PREFAB_TEXT);
    assert!(
        host.register_prefab(
            &world,
            "prefabs/missing.rveprefab",
            text.clone(),
            &sources(&[])
        )
        .is_err()
    );
    let missing_source = host
        .register_prefab(&world, PREFAB, text.clone(), &sources(&[]))
        .unwrap_err();
    assert!(matches!(missing_source, ScriptLoadError::Lite { .. }));
    host.register_prefab(
        &world,
        PREFAB,
        text.clone(),
        &sources(&[("bullet.lua", BULLET_LUA)]),
    )
    .unwrap();
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Resident));
    assert!(
        host.register_prefab(
            &world,
            PREFAB,
            text,
            &sources(&[("bullet.lua", BULLET_LUA)])
        )
        .is_err()
    );
}

#[test]
fn zero_max_instances_is_a_load_error() {
    let world = scene(GUN_SCENE);
    let error = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources(&[]),
        None,
        ScriptLoadOptions {
            prefabs: catalog(PrefabKind::Fast, Some(0)),
            ..ScriptLoadOptions::default()
        },
    )
    .err()
    .expect("zero limit is rejected");
    assert!(matches!(error, ScriptLoadError::Prefab { .. }));
}

#[test]
fn max_instances_ignores_roots_destroyed_in_the_same_phase() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    rve.spawn("prefabs/bullet.rveprefab", { id = "a" })
    rve.spawn("prefabs/bullet.rveprefab", { id = "b" })
  elseif step == 2 then
    rve.spawn("prefabs/bullet.rveprefab", { id = "c" })
    rve.destroy("c")
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, Some(2));
    host.update(&mut world, 0.1, at(0.1, 1));
    host.update(&mut world, 0.1, at(0.2, 2));
    assert!(world.handle("a").is_some());
    assert!(world.handle("b").is_some());
    assert!(world.handle("c").is_none());
    assert!(!infos(&mut host).contains(&"destroy a".to_owned()));
    assert!(errors(&mut host).is_empty());
}

#[test]
fn cancelled_spawns_of_one_phase_unload_a_heavy_prefab_once() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then rve.prefabs.load("prefabs/bullet.rveprefab") end
  if step == 2 then
    rve.spawn("prefabs/bullet.rveprefab", { id = "a", velocity = { x = 1, y = 0, z = 0 } })
    rve.spawn("prefabs/bullet.rveprefab", { id = "b", velocity = { x = 1, y = 0, z = 0 } })
    rve.prefabs.unload("prefabs/bullet.rveprefab")
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", gun), ("bullet.lua", BULLET_LUA)],
        catalog(PrefabKind::Heavy, None),
    );
    host.update(&mut world, 0.1, at(0.1, 1));
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Load(PREFAB.to_owned())]
    );
    // A static root makes both queued spawns fail at apply, after the unload was requested.
    host.register_prefab(
        &world,
        PREFAB,
        template(PREFAB, PREFAB_TEXT),
        &sources(&[("bullet.lua", BULLET_LUA)]),
    )
    .unwrap();
    host.update(&mut world, 0.1, at(0.2, 2));

    assert_eq!(errors(&mut host).len(), 2);
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Unload(PREFAB.to_owned())],
        "one unload and no reload when every queued spawn is cancelled in the same phase"
    );
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Unloading));
    host.prefab_released(PREFAB);
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Unloaded));
    assert!(host.drain_world_changes().prefab_requests.is_empty());
}

#[test]
fn spawns_rejected_at_apply_do_not_use_the_frame_object_budget() {
    // A rejected fast spawn creates two objects before its velocity check fails. Counting them
    // would exhaust the 4096-object frame budget and reject the valid spawn after them.
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    for _ = 1, 2100 do
      rve.spawn("prefabs/bullet.rveprefab", { velocity = { x = 1, y = 0, z = 0 } })
    end
    rve.spawn("prefabs/bullet.rveprefab", { id = "ok" })
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.1, 1));
    assert!(world.handle("ok").is_some());
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 2100);
    assert!(
        errors
            .iter()
            .all(|message| message.contains("dynamic rigid body"))
    );
}

#[test]
fn spawn_names_are_limited_to_256_utf8_bytes() {
    let gun = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 then
    local too_long = pcall(rve.spawn, "prefabs/bullet.rveprefab", { id = "long", name = string.rep("n", 257) })
    local fits = pcall(rve.spawn, "prefabs/bullet.rveprefab", { id = "fits", name = string.rep("n", 256) })
    rve.log_info("long=" .. tostring(too_long) .. " fits=" .. tostring(fits))
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, gun, None);
    host.update(&mut world, 0.1, at(0.1, 1));
    assert_eq!(infos(&mut host), ["long=false fits=true"]);
    host.update(&mut world, 0.1, at(0.2, 2));
    assert!(world.handle("long").is_none());
    assert!(world.handle("fits").is_some());
}

#[test]
fn prefab_scripts_compile_with_read_only_world_access() {
    let world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", "local Gun = {}\nreturn Gun\n")],
        catalog(PrefabKind::Fast, None),
    );
    // Top-level `rve.find` sees the world, as it does for scene scripts.
    let finds_gun = r#"
assert(rve.find("gun") ~= nil, "gun is visible at top level")
local Bullet = {}
return Bullet
"#;
    host.register_prefab(
        &world,
        PREFAB,
        template(PREFAB, PREFAB_TEXT),
        &sources(&[("bullet.lua", finds_gun)]),
    )
    .unwrap();
    assert_eq!(host.prefab_status(PREFAB), Some(PrefabStatus::Resident));

    // Top-level spawn is refused, and a refused registration leaves the prefab unchanged.
    let spawns = r#"rve.spawn("prefabs/bullet.rveprefab", { id = "early" }) return {}"#;
    let mut other = load(
        &world,
        &[("gun.lua", "local Gun = {}\nreturn Gun\n")],
        catalog(PrefabKind::Fast, None),
    );
    let error = other
        .register_prefab(
            &world,
            PREFAB,
            template(PREFAB, PREFAB_TEXT),
            &sources(&[("bullet.lua", spawns)]),
        )
        .unwrap_err();
    assert!(error.to_string().contains("creation"), "{error}");
    assert_eq!(other.prefab_status(PREFAB), Some(PrefabStatus::Unloaded));
}

const GUN_SPAWNS_ON_STEPS: &str = r#"
local Gun = {}
local step = 0
function Gun:on_update(dt)
  step = step + 1
  if step == 1 or step == 3 then
    rve.spawn("prefabs/bullet.rveprefab", { id = "shot" .. step })
  end
end
return Gun
"#;

#[test]
fn reverted_spawn_runs_no_callbacks_and_frees_its_slot_for_the_next_spawn() {
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, GUN_SPAWNS_ON_STEPS, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    let changes = host.drain_world_changes();
    let [spawned] = changes.spawned.as_slice() else {
        panic!("first update should spawn one root: {changes:?}");
    };
    let root = spawned.handle;

    host.revert_spawn(&mut world, root, "physics refused");
    assert!(!world.is_alive(root));
    assert!(world.handle("shot1").is_none());
    assert_eq!(errors(&mut host), ["spawn rolled back: physics refused"]);
    let after_revert = host.drain_world_changes();
    assert!(after_revert.spawned.is_empty() && after_revert.despawned.is_empty());

    // Отменённый экземпляр не получает on_start и on_update и не даёт ошибку на мёртвом объекте
    // (без отката экземпляр падал бы с «object no longer exists»).
    host.update(&mut world, 0.1, at(0.1, 2));
    let events = host.drain_events();
    assert!(events.is_empty(), "{events:?}");

    // Слот освобождён: следующий spawn создаёт объект и его экземпляр, который стартует как обычно.
    host.update(&mut world, 0.1, at(0.2, 3));
    let next = host.drain_world_changes();
    assert_eq!(next.spawned.len(), 1);
    assert!(
        next.despawned.is_empty(),
        "откат не должен повторно удаляться"
    );
    host.update(&mut world, 0.1, at(0.3, 4));
    assert_eq!(infos(&mut host), ["start shot3", "update shot3"]);
    assert!(world.is_alive(next.spawned[0].handle));
}

#[test]
fn reverting_a_started_spawn_calls_on_destroy_once_and_removes_its_commands() {
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, GUN_FIRST, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    let root = host.drain_world_changes().spawned[0].handle;
    host.update(&mut world, 0.1, at(0.1, 2));
    assert_eq!(infos(&mut host), ["start shot", "update shot"]);

    host.revert_spawn(&mut world, root, "late");
    let events = host.drain_events();
    let destroyed = events
        .iter()
        .filter(|event| event.level == ScriptEventLevel::Info && event.message == "destroy shot")
        .count();
    assert_eq!(destroyed, 1, "{events:?}");
    assert!(events.iter().any(|event| {
        event.level == ScriptEventLevel::Error && event.message == "spawn rolled back: late"
    }));
    host.update(&mut world, 0.1, at(0.2, 3));
    assert!(infos(&mut host).is_empty());
}

#[test]
fn revert_of_a_dead_root_only_reports_the_error() {
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, GUN_FIRST, None);
    host.update(&mut world, 0.1, at(0.0, 1));
    let root = host.drain_world_changes().spawned[0].handle;

    host.revert_spawn(&mut world, root, "first");
    host.revert_spawn(&mut world, root, "second");
    assert_eq!(
        errors(&mut host),
        ["spawn rolled back: first", "spawn rolled back: second"]
    );
    assert!(host.drain_world_changes().despawned.is_empty());
}

/// Пути префабов принимаются и с ведущим слешем, как в файлах сцен, и без него: это один ключ.
#[test]
fn prefab_paths_with_and_without_a_leading_slash_name_one_prefab() {
    const GUN_SPAWNS_WITH_SLASH: &str = r#"
local Gun = {}
function Gun:on_update(dt)
  if self.frames == nil then
    self.frames = 1
    rve.spawn("/prefabs/bullet.rveprefab", { id = "shot" })
  end
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, GUN_SPAWNS_WITH_SLASH, None);
    assert_eq!(
        host.prefab_status("/prefabs/bullet.rveprefab"),
        Some(PrefabStatus::Resident)
    );
    assert_eq!(
        host.prefab_status("prefabs/bullet.rveprefab"),
        Some(PrefabStatus::Resident)
    );

    host.update(&mut world, 0.1, at(0.0, 1));
    assert!(errors(&mut host).is_empty());
    assert!(world.handle("shot").is_some());
}

/// Тяжёлый префаб: заявка загрузки с ведущим слешем попадает в журнал под каноническим ключом.
#[test]
fn heavy_prefab_requests_accept_a_leading_slash() {
    const GUN_LOADS_WITH_SLASH: &str = r#"
local Gun = {}
function Gun:on_start()
  rve.prefabs.load("/prefabs/bullet.rveprefab")
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = load(
        &world,
        &[("gun.lua", GUN_LOADS_WITH_SLASH)],
        catalog(PrefabKind::Heavy, None),
    );
    host.update(&mut world, 0.1, at(0.0, 1));
    assert!(errors(&mut host).is_empty());
    assert_eq!(
        host.drain_world_changes().prefab_requests,
        [PrefabRequest::Load(PREFAB.to_owned())]
    );
    assert_eq!(
        host.prefab_status("/prefabs/bullet.rveprefab"),
        Some(PrefabStatus::Loading)
    );
}

/// Некорректный путь отвергается spawn-ом с понятной ошибкой, а запросы состояния возвращают `None`.
#[test]
fn invalid_prefab_paths_are_refused_by_spawn_and_ignored_by_queries() {
    const GUN_SPAWNS_ESCAPING_PATH: &str = r#"
local Gun = {}
function Gun:on_update(dt)
  rve.spawn("/prefabs/../bullet.rveprefab")
end
return Gun
"#;
    let mut world = scene(GUN_SCENE);
    let mut host = fast_host(&world, GUN_SPAWNS_ESCAPING_PATH, None);
    assert_eq!(host.prefab_status("/prefabs/../bullet.rveprefab"), None);
    assert_eq!(host.prefab_status(""), None);

    host.update(&mut world, 0.1, at(0.0, 1));
    let errors = errors(&mut host);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("is not a valid asset path"),
        "{errors:?}"
    );
}

/// Демо-пуля: столкновение со стеной ставит след из префаба и удаляет пулю. Событие подаётся вручную,
/// поэтому тест проверяет только скрипт, а не физику.
#[test]
fn demo_bullet_collision_spawns_a_hole_and_destroys_the_bullet() {
    const DEMO_BULLET: &str =
        include_str!("../../../../examples/demo-game/Assets/scripts/bullet.lua");
    const DEMO_HOLE: &str =
        include_str!("../../../../examples/demo-game/Assets/prefabs/bullet_hole.rveprefab");
    const HOLE: &str = "prefabs/bullet_hole.rveprefab";

    let mut world = scene(
        "[[assets]]\npath = \"/bullet.lua\"\n[[objects]]\nid = \"bullet\"\n\
[[objects.lite_scripts]]\npath = \"/bullet.lua\"\n[[objects]]\nid = \"wall\"\n",
    );
    let bullet = world.handle("bullet").unwrap();
    let wall = world.handle("wall").unwrap();
    let snapshot = engine_physics::PhysicsWorld::new(glam::Vec3::ZERO)
        .unwrap()
        .query_snapshot()
        .unwrap();
    let bindings = [
        engine_physics::PhysicsSceneBinding {
            object: bullet,
            body: engine_physics::BodyHandle(11),
            kind: engine_physics::BodyKind::Dynamic,
            character_controller: None,
        },
        engine_physics::PhysicsSceneBinding {
            object: wall,
            body: engine_physics::BodyHandle(12),
            kind: engine_physics::BodyKind::Static,
            character_controller: None,
        },
    ];
    let mut prefabs = PrefabCatalog::default();
    prefabs.insert(
        HOLE,
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let mut host = load(&world, &[("bullet.lua", DEMO_BULLET)], prefabs);
    host.register_prefab(&world, HOLE, template(HOLE, DEMO_HOLE), &BTreeMap::new())
        .unwrap();
    host.set_physics_snapshot(snapshot, &bindings);
    host.queue_physics_events([engine_physics::PhysicsEvent::CollisionEnter {
        first: engine_physics::BodyHandle(11),
        second: engine_physics::BodyHandle(12),
        point: glam::Vec3::new(0.0, 0.0, -4.0),
        normal: glam::Vec3::Z,
    }]);

    host.update(&mut world, 1.0 / 60.0, at(1.0 / 60.0, 1));
    assert!(errors(&mut host).is_empty());
    assert!(world.handle("bullet").is_none());
    let holes = world
        .objects()
        .iter()
        .filter(|object| object.id.starts_with("bullet_hole@"))
        .count();
    assert_eq!(holes, 1);
    assert!(world.handle("wall").is_some());
}
