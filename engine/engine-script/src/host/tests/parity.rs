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
}

// Add each script-facing capability to this shared inventory so Lua and ABI stay in parity.
const CAPABILITIES: &[(&str, &str, HostCapability)] = &[
    ("rve", "find", HostCapability::FindObject),
    ("object", "get_transform", HostCapability::GetLocal),
    ("object", "set_transform", HostCapability::SetLocal),
    ("rve", "send", HostCapability::Send),
    ("rve", "broadcast", HostCapability::Broadcast),
    ("object", "net_send", HostCapability::NetObjectSend),
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
            "return { on_start = function(self) rve.__parity_object = self.object end }",
        )]),
        None,
    )
    .unwrap();
    host.start(&mut world, ScriptTime::default());
    let rve: Table = host.lua.globals().get("rve").unwrap();
    let object: Table = rve.get("__parity_object").unwrap();
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
        };
        assert!(available, "ABI callback missing for {table}.{name}");
    }
}
