use super::*;
use crate::abi_host::AbiCallContext;
use crate::lite::ScriptEventQueue;
use crate::transaction::ScriptTransaction;
use mlua::{Table, Value as LuaValue};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Copy)]
enum HostCapability {
    FindObject,
    GetLocal,
    SetLocal,
    Send,
    Broadcast,
    NetObjectSend,
    FindObjectsByName,
    FindObjectsByTag,
    SetActive,
    IsActive,
    ActiveInHierarchy,
    Parent,
    SetParent,
    Children,
    WorldPosition,
    SetWorldPosition,
    WorldRotation,
    SetWorldRotation,
    Tag,
    SetTag,
    Layer,
    SetLayer,
    HasComponent,
    Components,
    ScriptSetEnabled,
    ScriptEnabled,
}

// Add each script-facing capability to this shared inventory so Lua and ABI stay in parity.
// Lua finders live on `rve`; the ABI finds them through one name lookup and one tag lookup.
const CAPABILITIES: &[(&str, &str, HostCapability)] = &[
    ("rve", "find", HostCapability::FindObject),
    ("object", "get_transform", HostCapability::GetLocal),
    ("object", "set_transform", HostCapability::SetLocal),
    ("rve", "send", HostCapability::Send),
    ("rve", "broadcast", HostCapability::Broadcast),
    ("object", "net_send", HostCapability::NetObjectSend),
    ("rve", "find_by_name", HostCapability::FindObjectsByName),
    ("rve", "find_all", HostCapability::FindObjectsByName),
    ("rve", "find_with_tag", HostCapability::FindObjectsByTag),
    ("rve", "find_all_with_tag", HostCapability::FindObjectsByTag),
    ("object", "set_active", HostCapability::SetActive),
    ("object", "is_active", HostCapability::IsActive),
    (
        "object",
        "active_in_hierarchy",
        HostCapability::ActiveInHierarchy,
    ),
    ("object", "parent", HostCapability::Parent),
    ("object", "set_parent", HostCapability::SetParent),
    ("object", "children", HostCapability::Children),
    (
        "object",
        "get_world_position",
        HostCapability::WorldPosition,
    ),
    (
        "object",
        "set_world_position",
        HostCapability::SetWorldPosition,
    ),
    (
        "object",
        "get_world_rotation",
        HostCapability::WorldRotation,
    ),
    (
        "object",
        "set_world_rotation",
        HostCapability::SetWorldRotation,
    ),
    ("object", "tag", HostCapability::Tag),
    ("object", "set_tag", HostCapability::SetTag),
    ("object", "layer", HostCapability::Layer),
    ("object", "set_layer", HostCapability::SetLayer),
    ("object", "has_component", HostCapability::HasComponent),
    ("object", "components", HostCapability::Components),
    ("script", "set_enabled", HostCapability::ScriptSetEnabled),
    ("script", "enabled", HostCapability::ScriptEnabled),
];

#[test]
fn lua_and_abi_expose_the_same_script_capabilities() {
    let mut world = world(
        r#"
[[assets]]
path = "/api.lua"
[[objects]]
id = "camera"
name = "Parity Camera"
[objects.camera]
active = true
[[objects.lite_scripts]]
path = "/api.lua"
"#,
    );
    let mut host = ScriptHost::load(
        &world,
        "scene",
        &sources(&[(
            "api.lua",
            "return { on_start = function(self) rve.__parity_object = self.object rve.__parity_script = self.script end }",
        )]),
        None,
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    let rve: Table = host.lua.globals().get("rve").unwrap();
    let object: Table = rve.get("__parity_object").unwrap();
    let script: Table = rve.get("__parity_script").unwrap();
    assert_eq!(object.get::<String>("id").unwrap(), "camera");
    assert_eq!(object.get::<String>("name").unwrap(), "Parity Camera");
    let events = Rc::new(RefCell::new(ScriptEventQueue::default()));
    let mut transaction = ScriptTransaction::default();
    let mut call = AbiCallContext::new(0, &mut transaction, &events, "scene");
    let abi = call.host(ScriptTime::default());
    for (table, name, capability) in CAPABILITIES {
        let lua_value: LuaValue = match *table {
            "rve" => rve.get(*name).unwrap(),
            "object" => object.get(*name).unwrap(),
            "script" => script.get(*name).unwrap(),
            _ => unreachable!(),
        };
        assert!(
            matches!(lua_value, LuaValue::Function(_)),
            "Lua API missing {table}.{name}"
        );
        let available = match capability {
            HostCapability::FindObject => abi.find_object as usize != 0,
            HostCapability::GetLocal => abi.get_local as usize != 0,
            HostCapability::SetLocal => abi.set_local as usize != 0,
            HostCapability::Send => abi.send as usize != 0,
            HostCapability::Broadcast => abi.broadcast as usize != 0,
            HostCapability::NetObjectSend => abi.net_object_send.is_some(),
            HostCapability::FindObjectsByName => abi.find_objects_by_name.is_some(),
            HostCapability::FindObjectsByTag => abi.find_objects_by_tag.is_some(),
            HostCapability::SetActive => abi.object_set_active.is_some(),
            HostCapability::IsActive => abi.object_is_active.is_some(),
            HostCapability::ActiveInHierarchy => abi.object_active_in_hierarchy.is_some(),
            HostCapability::Parent => abi.object_parent.is_some(),
            HostCapability::SetParent => abi.object_set_parent.is_some(),
            HostCapability::Children => abi.object_children.is_some(),
            HostCapability::WorldPosition => abi.object_world_position.is_some(),
            HostCapability::SetWorldPosition => abi.object_set_world_position.is_some(),
            HostCapability::WorldRotation => abi.object_world_rotation.is_some(),
            HostCapability::SetWorldRotation => abi.object_set_world_rotation.is_some(),
            HostCapability::Tag => abi.object_tag.is_some(),
            HostCapability::SetTag => abi.object_set_tag.is_some(),
            HostCapability::Layer => abi.object_layer.is_some(),
            HostCapability::SetLayer => abi.object_set_layer.is_some(),
            HostCapability::HasComponent | HostCapability::Components => {
                abi.object_has_component.is_some()
            }
            HostCapability::ScriptSetEnabled => abi.script_set_enabled.is_some(),
            HostCapability::ScriptEnabled => abi.script_enabled.is_some(),
        };
        assert!(available, "ABI callback missing for {table}.{name}");
    }
}
