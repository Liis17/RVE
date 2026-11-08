use super::ScriptHost;
use crate::lifecycle::{PrefabCatalog, PrefabKind, PrefabSpec};
use crate::{ScriptEventLevel, ScriptTime};
use engine_files::VirtualPath;
use engine_scene::{
    MAX_MESSAGE_STRING_BYTES, NetEvent, NetId, NetMode, NetRequest, NetTarget, ParamValue,
    PrefabTemplate, SceneDocument, SpawnOptions, World,
};
use std::collections::BTreeMap;
use std::sync::Arc;

const SCENE: &str = "[[assets]]\npath = \"/player.lua\"\n[[assets]]\npath = \"/other.lua\"\n\
[[objects]]\nid = \"camera\"\n[objects.camera]\n\
[[objects]]\nid = \"player\"\n[objects.network_identity]\nauthority = \"owner\"\n\
[[objects.lite_scripts]]\npath = \"/player.lua\"\n\
[[objects]]\nid = \"other\"\n[[objects.lite_scripts]]\npath = \"/other.lua\"\n";

const BLANK: &str = "local Script = {}\nreturn Script\n";

const ECHO: &str = r#"
local Script = {}
function Script:on_net_message(peer, name, value)
  rve.log_info(self.object.id .. " message " .. peer .. " " .. name .. " " .. tostring(value))
end
function Script:on_peer_connected(peer)
  rve.log_info(self.object.id .. " connected " .. peer)
end
function Script:on_peer_disconnected(peer, reason)
  rve.log_info(self.object.id .. " disconnected " .. peer .. " " .. reason)
end
return Script
"#;

const BARE: &str = r#"
local Script = {}
function Script:on_peer_connected(peer)
  rve.log_info(self.object.id .. " connected " .. peer)
end
return Script
"#;

fn load(player: &str, other: &str) -> (World, ScriptHost) {
    let world =
        World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap();
    let sources = BTreeMap::from([
        ("player.lua".to_owned(), player.to_owned()),
        ("other.lua".to_owned(), other.to_owned()),
    ]);
    let host = ScriptHost::load(&world, "scene", &sources, None).unwrap();
    (world, host)
}

fn at(elapsed: f64, frame: u64) -> ScriptTime {
    ScriptTime { elapsed, frame }
}

fn run(world: &mut World, host: &mut ScriptHost, elapsed: f64, frame: u64) {
    host.update(world, 1.0 / 60.0, at(elapsed, frame));
}

/// Забирает журнал один раз и делит по уровням: повторный drain уже пуст.
struct Logs {
    infos: Vec<String>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn take_logs(host: &mut ScriptHost) -> Logs {
    let mut logs = Logs {
        infos: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for event in host.drain_events() {
        match event.level {
            ScriptEventLevel::Info => logs.infos.push(event.message),
            ScriptEventLevel::Error => logs.errors.push(event.message),
            ScriptEventLevel::Warn => logs.warnings.push(event.message),
        }
    }
    logs
}

fn infos(host: &mut ScriptHost) -> Vec<String> {
    take_logs(host).infos
}

fn errors(host: &mut ScriptHost) -> Vec<String> {
    take_logs(host).errors
}

fn message(peer: u32, name: &str, value: ParamValue) -> NetEvent {
    NetEvent::Message {
        peer,
        object: None,
        name: name.to_owned(),
        value,
    }
}

#[test]
fn rve_net_reports_the_mode_in_every_mode() {
    let reporter = r#"
local Script = {}
function Script:on_update()
  local net = rve.net
  rve.log_info(table.concat({
    net.mode(), tostring(net.is_server()), tostring(net.is_client()),
    tostring(net.local_peer()), tostring(#net.peers()),
  }, " "))
end
return Script
"#;
    let (mut world, mut host) = load(reporter, BLANK);
    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(infos(&mut host), ["offline true false 0 0"]);

    host.set_network_state(NetMode::Host, 0, vec![1, 2]);
    run(&mut world, &mut host, 0.1, 2);
    assert_eq!(infos(&mut host), ["host true true 0 2"]);

    host.set_network_state(NetMode::Server, 0, vec![]);
    run(&mut world, &mut host, 0.2, 3);
    assert_eq!(infos(&mut host), ["server true false 0 0"]);

    host.set_network_state(NetMode::Client, 5, vec![0]);
    run(&mut world, &mut host, 0.3, 4);
    assert_eq!(infos(&mut host), ["client false true 5 1"]);
}

#[test]
fn rve_net_send_stages_requests_with_targets_reliability_and_values() {
    let sender = r#"
local Script = {}
local sent = false
function Script:on_update()
  if sent then return end
  sent = true
  rve.net.send("server", "a", 1)
  rve.net.send("all", "b", {1, 2}, {reliable = false})
  rve.net.send(7, "c", "text")
  rve.find("player"):net_send("d", true, "all")
  rve.find("player"):net_send("e", 2.5, 3, {reliable = false})
  local ok, err = pcall(rve.net.send, "server", string.rep("n", 257), 1)
  rve.log_info(tostring(ok) .. " " .. tostring(err))
end
return Script
"#;
    let (mut world, mut host) = load(sender, BLANK);
    host.set_network_state(NetMode::Host, 0, vec![]);
    run(&mut world, &mut host, 0.0, 1);
    let player = world.handle("player").unwrap();
    assert_eq!(
        host.drain_network_requests(),
        vec![
            NetRequest::Send {
                target: NetTarget::Server,
                name: "a".to_owned(),
                value: ParamValue::Int(1),
                reliable: true,
            },
            NetRequest::Send {
                target: NetTarget::All,
                name: "b".to_owned(),
                value: ParamValue::Array(vec![ParamValue::Int(1), ParamValue::Int(2)]),
                reliable: false,
            },
            NetRequest::Send {
                target: NetTarget::Peer(7),
                name: "c".to_owned(),
                value: ParamValue::String("text".to_owned()),
                reliable: true,
            },
            NetRequest::ObjectSend {
                object: player,
                name: "d".to_owned(),
                value: ParamValue::Bool(true),
                target: NetTarget::All,
                reliable: true,
            },
            NetRequest::ObjectSend {
                object: player,
                name: "e".to_owned(),
                value: ParamValue::Float(2.5),
                target: NetTarget::Peer(3),
                reliable: false,
            },
        ]
    );
    let logged = infos(&mut host);
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].starts_with("false "), "{logged:?}");
    assert!(logged[0].contains("256 bytes"), "{logged:?}");
}

#[test]
fn rve_net_send_enforces_the_shared_value_limits_before_staging() {
    let sender = r#"
local Script = {}
function Script:on_update()
  local deep = 1
  for _ = 1, 17 do deep = {deep} end
  local ok, err = pcall(rve.net.send, "server", "deep", deep)
  rve.log_info(tostring(ok) .. " " .. tostring(err))
end
return Script
"#;
    let (mut world, mut host) = load(sender, BLANK);
    host.set_network_state(NetMode::Server, 0, vec![]);
    run(&mut world, &mut host, 0.0, 1);
    assert!(host.drain_network_requests().is_empty());
    let logged = infos(&mut host);
    assert!(logged[0].starts_with("false "), "{logged:?}");
    assert!(logged[0].contains("nesting"), "{logged:?}");
}

#[test]
fn offline_send_is_a_silent_no_op() {
    let sender = r#"
local Script = {}
function Script:on_update()
  rve.net.send("all", "a", 1)
  rve.find("player"):net_send("b", 1, "server")
  rve.find("player"):set_net_owner(4)
end
return Script
"#;
    let (mut world, mut host) = load(sender, BLANK);
    run(&mut world, &mut host, 0.0, 1);
    assert!(host.drain_network_requests().is_empty());
    assert!(errors(&mut host).is_empty());
}

#[test]
fn clients_send_only_to_the_server_and_cannot_set_owners() {
    let sender = r#"
local Script = {}
local sent = false
function Script:on_update()
  if sent then return end
  sent = true
  local _, all = pcall(rve.net.send, "all", "a", 1)
  local _, peer = pcall(rve.net.send, 3, "a", 1)
  local _, owner = pcall(function() rve.find("player"):set_net_owner(4) end)
  rve.net.send("server", "ok", 1)
  rve.log_info(tostring(all) .. "|" .. tostring(peer) .. "|" .. tostring(owner))
end
return Script
"#;
    let (mut world, mut host) = load(sender, BLANK);
    host.set_network_state(NetMode::Client, 5, vec![0]);
    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(
        host.drain_network_requests(),
        vec![NetRequest::Send {
            target: NetTarget::Server,
            name: "ok".to_owned(),
            value: ParamValue::Int(1),
            reliable: true,
        }]
    );
    let logged = infos(&mut host);
    let parts: Vec<&str> = logged[0].split('|').collect();
    assert_eq!(parts.len(), 3, "{logged:?}");
    assert!(
        parts.iter().all(|part| part.contains("серверу")),
        "{logged:?}"
    );
    assert!(errors(&mut host).is_empty());
}

#[test]
fn set_net_owner_is_staged_by_the_server_and_ignored_offline() {
    let owner = r#"
local Script = {}
function Script:on_update()
  rve.find("player"):set_net_owner(4)
end
return Script
"#;
    let (mut world, mut host) = load(owner, BLANK);
    host.set_network_state(NetMode::Server, 0, vec![4]);
    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(
        host.drain_network_requests(),
        vec![NetRequest::SetOwner {
            object: world.handle("player").unwrap(),
            peer: 4,
        }]
    );
}

#[test]
fn object_net_getters_read_the_attributes_set_by_the_bridge() {
    let reader = r#"
local Script = {}
function Script:on_update()
  local obj = rve.find(self.object.id)
  local owner = obj:net_owner()
  rve.log_info(tostring(obj:net_id()) .. " " .. tostring(owner) .. " "
    .. tostring(obj:is_owner()) .. " " .. tostring(obj:has_authority()))
end
return Script
"#;
    let (mut world, mut host) = load(reader, reader);
    let player = world.handle("player").unwrap();
    host.set_network_state(NetMode::Client, 5, vec![0]);
    host.set_object_net(player, NetId::scene(42).unwrap(), Some(5));
    run(&mut world, &mut host, 0.0, 1);
    let logged = infos(&mut host);
    assert!(logged.contains(&"42 5 true true".to_owned()), "{logged:?}");
    assert!(
        logged.contains(&"nil nil false true".to_owned()),
        "{logged:?}"
    );

    host.set_object_net(player, NetId::scene(42).unwrap(), Some(4));
    run(&mut world, &mut host, 0.1, 2);
    let logged = infos(&mut host);
    assert!(
        logged.contains(&"42 4 false false".to_owned()),
        "{logged:?}"
    );

    host.set_network_state(NetMode::Server, 0, vec![]);
    run(&mut world, &mut host, 0.2, 3);
    let logged = infos(&mut host);
    assert!(logged.contains(&"42 4 false true".to_owned()), "{logged:?}");
}

#[test]
fn net_messages_reach_the_addressed_object_or_every_instance_with_the_callback() {
    let (mut world, mut host) = load(ECHO, ECHO);
    let player = world.handle("player").unwrap();
    host.set_network_state(NetMode::Server, 0, vec![3]);
    host.queue_network_events(
        &world,
        [
            message(3, "hi", ParamValue::Int(1)),
            NetEvent::Message {
                peer: 4,
                object: Some(player),
                name: "only".to_owned(),
                value: ParamValue::Bool(true),
            },
        ],
    );
    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(
        infos(&mut host),
        [
            "player message 3 hi 1",
            "other message 3 hi 1",
            "player message 4 only true",
        ]
    );
}

#[test]
fn peer_callbacks_reach_every_instance_that_defines_them() {
    let (mut world, mut host) = load(ECHO, BARE);
    host.queue_network_events(
        &world,
        [
            NetEvent::PeerConnected { peer: 5 },
            NetEvent::PeerDisconnected {
                peer: 5,
                reason: "timeout".to_owned(),
            },
        ],
    );
    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(
        infos(&mut host),
        [
            "player connected 5",
            "other connected 5",
            "player disconnected 5 timeout",
        ]
    );
}

#[test]
fn a_failing_net_callback_keeps_the_instance_and_counts_the_peer_fault() {
    let fragile = r#"
local Script = {}
function Script:on_net_message(peer, name, value)
  if value == "boom" then error("bad net message") end
  rve.log_info("ok " .. value)
end
function Script:on_peer_connected(peer)
  error("connect failed")
end
return Script
"#;
    let (mut world, mut host) = load(fragile, BLANK);
    host.queue_network_events(
        &world,
        [
            message(9, "a", ParamValue::String("boom".to_owned())),
            message(9, "a", ParamValue::String("fine".to_owned())),
            NetEvent::PeerConnected { peer: 9 },
        ],
    );
    run(&mut world, &mut host, 0.0, 1);
    let logs = take_logs(&mut host);
    assert_eq!(logs.errors.len(), 2, "{:?}", logs.errors);
    assert!(
        logs.errors[0].contains("bad net message"),
        "{:?}",
        logs.errors
    );
    assert!(
        logs.errors[1].contains("connect failed"),
        "{:?}",
        logs.errors
    );
    assert_eq!(logs.infos, ["ok fine"]);
    assert_eq!(host.drain_peer_faults(), [(9, 1)]);
    assert!(host.drain_peer_faults().is_empty());

    host.queue_network_events(
        &world,
        [message(9, "a", ParamValue::String("fine".to_owned()))],
    );
    run(&mut world, &mut host, 0.1, 2);
    assert_eq!(
        infos(&mut host),
        ["ok fine"],
        "экземпляр продолжает работать"
    );
}

#[test]
fn net_error_logs_are_rate_limited_per_source_and_summarized_per_window() {
    let failing = r#"
local Script = {}
function Script:on_net_message(peer, name, value)
  error("bad net message")
end
return Script
"#;
    let (mut world, mut host) = load(failing, BLANK);
    host.queue_network_events(
        &world,
        (0..10).map(|index| message(2, "a", ParamValue::Int(index))),
    );
    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(errors(&mut host).len(), 8, "лимит на источник в секунду");
    assert_eq!(
        host.drain_peer_faults(),
        [(2, 10)],
        "счётчик считает все ошибки"
    );

    host.queue_network_events(&world, [message(2, "a", ParamValue::Int(0))]);
    run(&mut world, &mut host, 1.5, 2);
    let logged = errors(&mut host);
    assert_eq!(
        logged.len(),
        2,
        "сводка прошлого окна и новая ошибка: {logged:?}"
    );
    assert!(
        logged[0].contains("подавлено ошибок net-callback: 2"),
        "{logged:?}"
    );
    assert!(logged[1].contains("bad net message"), "{logged:?}");
}

#[test]
fn inbound_messages_beyond_the_per_peer_limit_are_dropped_with_one_warning() {
    let counter = r#"
local Script = {}
local received = 0
function Script:on_net_message(peer, name, value)
  received = received + 1
end
function Script:on_update()
  if received > 0 then rve.log_info("received " .. received) end
end
return Script
"#;
    let (mut world, mut host) = load(counter, BLANK);
    let limit = crate::net::MAX_INBOUND_MESSAGES_PER_PEER;
    let mut events: Vec<NetEvent> = (0..limit + 3)
        .map(|index| message(4, "a", ParamValue::Int(index as i64)))
        .collect();
    events.extend((0..2).map(|index| message(5, "a", ParamValue::Int(index))));
    host.queue_network_events(&world, events);
    run(&mut world, &mut host, 0.0, 1);
    let logs = take_logs(&mut host);
    assert_eq!(logs.infos, [format!("received {}", limit + 2)]);
    assert_eq!(logs.warnings.len(), 1, "{:?}", logs.warnings);
    assert!(
        logs.warnings[0].contains("сверх лимита"),
        "{:?}",
        logs.warnings
    );
}

#[test]
fn malformed_inbound_messages_are_dropped_and_count_as_peer_faults() {
    let (mut world, mut host) = load(ECHO, BLANK);
    host.queue_network_events(
        &world,
        [
            message(6, "bad", ParamValue::Float(f64::NAN)),
            message(6, &"n".repeat(257), ParamValue::Int(1)),
            message(
                6,
                "big",
                ParamValue::String("a".repeat(MAX_MESSAGE_STRING_BYTES + 1)),
            ),
        ],
    );
    run(&mut world, &mut host, 0.0, 1);
    let logs = take_logs(&mut host);
    assert!(logs.infos.is_empty(), "{:?}", logs.infos);
    assert_eq!(logs.warnings.len(), 3, "{:?}", logs.warnings);
    assert_eq!(host.drain_peer_faults(), [(6, 3)]);
}

const PREFAB: &str = "prefabs/ship.rveprefab";
const PREFAB_TEXT: &str = "[[assets]]\npath = \"/ship.lua\"\n\
[[objects]]\nid = \"root\"\n[[objects.lite_scripts]]\npath = \"/ship.lua\"\n";
const NET_PREFAB_TEXT: &str = "[[objects]]\nid = \"root\"\n[objects.network_identity]\n";
const SHIP_LUA: &str = r#"
local Ship = {}
function Ship:on_start()
  rve.log_info("start " .. self.object.id)
end
function Ship:on_destroy()
  rve.log_info("destroy " .. self.object.id)
end
return Ship
"#;

fn template(text: &str) -> Arc<PrefabTemplate> {
    let path = VirtualPath::parse(PREFAB).unwrap();
    let mut read = |_: &VirtualPath| -> Result<String, String> { Ok(text.to_owned()) };
    Arc::new(PrefabTemplate::load(&path, &mut read).unwrap())
}

/// Сцена с префабом. Регистрируется шаблон `text`, его скрипты берутся из `SHIP_LUA`;
/// скрипт игрока — `player`.
fn prefab_host(player: &str, text: &str) -> (World, ScriptHost) {
    let world =
        World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap();
    let mut prefabs = PrefabCatalog::default();
    prefabs.insert(
        PREFAB,
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let sources = BTreeMap::from([
        ("player.lua".to_owned(), player.to_owned()),
        ("other.lua".to_owned(), BLANK.to_owned()),
        ("ship.lua".to_owned(), SHIP_LUA.to_owned()),
    ]);
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources,
        None,
        crate::ScriptLoadOptions {
            prefabs,
            ..crate::ScriptLoadOptions::default()
        },
    )
    .unwrap();
    host.register_prefab(
        &world,
        PREFAB,
        template(text),
        &BTreeMap::from([("ship.lua".to_owned(), SHIP_LUA.to_owned())]),
    )
    .unwrap();
    (world, host)
}

#[test]
fn clients_cannot_spawn_networked_prefabs_locally_but_the_bridge_can() {
    let spawner = format!(
        r#"
local Script = {{}}
function Script:on_update()
  local ok, err = pcall(rve.spawn, "{PREFAB}", {{ id = "local_ship" }})
  rve.log_info(tostring(ok) .. " " .. tostring(err))
end
return Script
"#
    );
    let (mut world, mut host) = prefab_host(&spawner, NET_PREFAB_TEXT);
    host.set_network_state(NetMode::Client, 5, vec![0]);
    run(&mut world, &mut host, 0.0, 1);
    let logged = infos(&mut host);
    assert!(logged[0].starts_with("false "), "{logged:?}");
    assert!(logged[0].contains("network objects"), "{logged:?}");
    assert!(world.handle("local_ship").is_none());

    let root = host
        .spawn_external(
            &mut world,
            PREFAB,
            SpawnOptions {
                id: Some("n2147483649-ship".to_owned()),
                ..SpawnOptions::default()
            },
        )
        .unwrap();
    assert_eq!(world.handle("n2147483649-ship"), Some(root));
}

#[test]
fn spawn_external_starts_on_the_next_update_and_despawn_runs_on_destroy() {
    let (mut world, mut host) = prefab_host(BLANK, PREFAB_TEXT);
    let id = "n2147483649-ship";
    let root = host
        .spawn_external(
            &mut world,
            PREFAB,
            SpawnOptions {
                id: Some(id.to_owned()),
                ..SpawnOptions::default()
            },
        )
        .unwrap();
    assert_eq!(world.handle(id), Some(root));
    let changes = host.drain_world_changes();
    assert_eq!(changes.spawned.len(), 1);
    assert_eq!(changes.spawned[0].handle, root);
    assert!(
        infos(&mut host).is_empty(),
        "on_start ждёт следующего update"
    );

    run(&mut world, &mut host, 0.0, 1);
    assert_eq!(infos(&mut host), [format!("start {id}")]);

    assert!(host.despawn_external(&mut world, root));
    assert!(world.handle(id).is_none());
    assert_eq!(infos(&mut host), [format!("destroy {id}")]);
    assert_eq!(host.drain_world_changes().despawned, [root]);
    assert!(
        !host.despawn_external(&mut world, root),
        "повтор ничего не удаляет"
    );
}

#[test]
fn spawn_external_rolls_back_refused_requests_and_releases_their_ids() {
    let (mut world, mut host) = prefab_host(BLANK, PREFAB_TEXT);
    let error = host
        .spawn_external(
            &mut world,
            "prefabs/unknown.rveprefab",
            SpawnOptions::default(),
        )
        .unwrap_err();
    assert!(error.contains("not in the project prefab list"), "{error}");

    let id = "n2147483650-ship";
    let options = SpawnOptions {
        id: Some(id.to_owned()),
        ..SpawnOptions::default()
    };
    host.spawn_external(&mut world, PREFAB, options.clone())
        .unwrap();
    let duplicate = host.spawn_external(&mut world, PREFAB, options.clone());
    assert!(duplicate.unwrap_err().contains("already used"));

    let bad_id = host.spawn_external(
        &mut world,
        PREFAB,
        SpawnOptions {
            id: Some("bad id".to_owned()),
            ..SpawnOptions::default()
        },
    );
    assert!(bad_id.unwrap_err().contains("must match"));
    let logged = errors(&mut host);
    assert!(
        logged.is_empty(),
        "проверки до применения не пишут в журнал: {logged:?}"
    );
}

#[test]
fn spawn_external_rejects_unregistered_prefabs_without_leaving_objects() {
    let world =
        World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap();
    let mut prefabs = PrefabCatalog::default();
    prefabs.insert(
        PREFAB,
        PrefabSpec {
            kind: PrefabKind::Fast,
            max_instances: None,
        },
    );
    let sources = BTreeMap::from([
        ("player.lua".to_owned(), BLANK.to_owned()),
        ("other.lua".to_owned(), BLANK.to_owned()),
    ]);
    let mut host = ScriptHost::load_with_options(
        &world,
        "scene",
        &sources,
        None,
        crate::ScriptLoadOptions {
            prefabs,
            ..crate::ScriptLoadOptions::default()
        },
    )
    .unwrap();
    let mut world = world;
    let error = host
        .spawn_external(
            &mut world,
            PREFAB,
            SpawnOptions {
                id: Some("n2147483651-ship".to_owned()),
                ..SpawnOptions::default()
            },
        )
        .unwrap_err();
    assert!(error.contains("is not registered"), "{error}");
    assert!(world.handle("n2147483651-ship").is_none());
    assert!(host.drain_world_changes().spawned.is_empty());
    assert!(
        errors(&mut host).is_empty(),
        "отказ проверки возвращается вызывающему коду"
    );
}
