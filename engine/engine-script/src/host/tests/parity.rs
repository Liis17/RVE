use super::*;
use crate::abi_host::AbiCallContext;
use crate::lite::ScriptEventQueue;
use crate::transaction::ScriptTransaction;
use mlua::{ObjectLike, Table, Value as LuaValue};
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
    GetCollider,
    SetCollider,
    GetRigidBody,
    SetRigidBody,
    GetController,
    SetController,
    SimpleMove,
    Jump,
    VerticalVelocity,
    Overlaps,
    PhysicsOverlap,
    PhysicsRaycastAll,
    PhysicsShapeCast,
    LightGet,
    LightSet,
    MeshGetVisible,
    MeshSetVisible,
    MeshSetMaterialParams,
    MeshSetMaterialPath,
    ParticlesGetPlaying,
    ParticlesSetPlaying,
    ParticlesCommand,
    EnvironmentGet,
    EnvironmentSet,
    LodGet,
    LodSet,
    AudioGetSource,
    AudioSetSource,
    AudioPlayClipEx,
    AudioSetVoicePosition,
    TimeScale,
    TimerAfter,
    TimerEvery,
    TimerCancel,
    SaveSet,
    SaveGet,
    SaveDelete,
    SaveHas,
    SaveKeys,
    SaveFlush,
    KeepOnLoad,
    SetKeepOnLoad,
    ObjectScript,
    ScriptRefGet,
    ScriptRefSet,
    ScriptRefSend,
    ScriptRefEnabled,
    ScriptRefSetEnabled,
    SceneLoad,
    SceneCurrent,
    AppQuit,
}

// Add each script-facing capability to this shared inventory so Lua and ABI stay in parity.
// Lua finders live on `rve`; the ABI finds them through one name lookup and one tag lookup.
const CAPABILITIES: &[(&str, &str, HostCapability)] = &[
    ("rve", "find", HostCapability::FindObject),
    ("object", "get_transform", HostCapability::GetLocal),
    ("object", "set_transform", HostCapability::SetLocal),
    ("rve", "send", HostCapability::Send),
    ("rve", "broadcast", HostCapability::Broadcast),
    ("timer", "after", HostCapability::TimerAfter),
    ("timer", "every", HostCapability::TimerEvery),
    ("timer", "cancel", HostCapability::TimerCancel),
    ("time", "scale", HostCapability::TimeScale),
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
    ("object", "keep_on_load", HostCapability::KeepOnLoad),
    ("object", "set_keep_on_load", HostCapability::SetKeepOnLoad),
    ("object", "script", HostCapability::ObjectScript),
    ("script_ref", "get", HostCapability::ScriptRefGet),
    ("script_ref", "set", HostCapability::ScriptRefSet),
    ("script_ref", "send", HostCapability::ScriptRefSend),
    ("script_ref", "enabled", HostCapability::ScriptRefEnabled),
    (
        "script_ref",
        "set_enabled",
        HostCapability::ScriptRefSetEnabled,
    ),
    ("scene", "load", HostCapability::SceneLoad),
    ("scene", "current", HostCapability::SceneCurrent),
    ("app", "quit", HostCapability::AppQuit),
    ("save", "set", HostCapability::SaveSet),
    ("save", "get", HostCapability::SaveGet),
    ("save", "delete", HostCapability::SaveDelete),
    ("save", "has", HostCapability::SaveHas),
    ("save", "keys", HostCapability::SaveKeys),
    ("save", "flush", HostCapability::SaveFlush),
    ("object", "get_collider", HostCapability::GetCollider),
    ("object", "set_collider", HostCapability::SetCollider),
    ("object", "get_rigid_body", HostCapability::GetRigidBody),
    ("object", "set_rigid_body", HostCapability::SetRigidBody),
    ("object", "get_controller", HostCapability::GetController),
    ("object", "set_controller", HostCapability::SetController),
    ("object", "simple_move", HostCapability::SimpleMove),
    ("object", "jump", HostCapability::Jump),
    (
        "object",
        "vertical_velocity",
        HostCapability::VerticalVelocity,
    ),
    ("object", "get_overlaps", HostCapability::Overlaps),
    ("physics", "overlap_sphere", HostCapability::PhysicsOverlap),
    ("physics", "overlap_box", HostCapability::PhysicsOverlap),
    ("physics", "overlap_capsule", HostCapability::PhysicsOverlap),
    ("physics", "raycast_all", HostCapability::PhysicsRaycastAll),
    ("physics", "sphere_cast", HostCapability::PhysicsShapeCast),
    ("physics", "box_cast", HostCapability::PhysicsShapeCast),
    ("physics", "capsule_cast", HostCapability::PhysicsShapeCast),
    ("object", "get_light", HostCapability::LightGet),
    ("object", "set_light", HostCapability::LightSet),
    ("object", "is_visible", HostCapability::MeshGetVisible),
    ("object", "set_visible", HostCapability::MeshSetVisible),
    (
        "object",
        "set_material_params",
        HostCapability::MeshSetMaterialParams,
    ),
    (
        "object",
        "set_material",
        HostCapability::MeshSetMaterialPath,
    ),
    (
        "object",
        "particles_is_playing",
        HostCapability::ParticlesGetPlaying,
    ),
    (
        "object",
        "particles_play",
        HostCapability::ParticlesSetPlaying,
    ),
    (
        "object",
        "particles_stop",
        HostCapability::ParticlesSetPlaying,
    ),
    (
        "object",
        "particles_restart",
        HostCapability::ParticlesCommand,
    ),
    ("object", "particles_emit", HostCapability::ParticlesCommand),
    ("object", "get_environment", HostCapability::EnvironmentGet),
    ("object", "set_environment", HostCapability::EnvironmentSet),
    ("object", "get_lod", HostCapability::LodGet),
    ("object", "set_lod_distances", HostCapability::LodSet),
    ("object", "get_audio_source", HostCapability::AudioGetSource),
    ("object", "set_audio_source", HostCapability::AudioSetSource),
    ("audio", "play", HostCapability::AudioPlayClipEx),
    (
        "audio",
        "set_voice_position",
        HostCapability::AudioSetVoicePosition,
    ),
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
            "return { on_start = function(self) rve.__parity_object = self.object rve.__parity_script = self.script rve.__parity_ref = self.object:script(\"/api.lua\") end }",
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
    // Physics callbacks exist only while a physics snapshot is attached.
    let snapshot = engine_physics::PhysicsWorld::default()
        .query_snapshot()
        .unwrap();
    transaction.attach_physics_for_tests(snapshot);
    let mut call = AbiCallContext::new(0, &mut transaction, &events, "scene");
    let abi = call.host(ScriptTime::default());
    for (table, name, capability) in CAPABILITIES {
        let lua_value: LuaValue = match *table {
            "rve" => rve.get(*name).unwrap(),
            "object" => object.get(*name).unwrap(),
            "script" => script.get(*name).unwrap(),
            "physics" => rve.get::<Table>("physics").unwrap().get(*name).unwrap(),
            "audio" => rve.get::<Table>("audio").unwrap().get(*name).unwrap(),
            "timer" => rve.get::<Table>("timer").unwrap().get(*name).unwrap(),
            "time" => rve.get::<Table>("time").unwrap().get(*name).unwrap(),
            "save" => rve.get::<Table>("save").unwrap().get(*name).unwrap(),
            "scene" => rve.get::<Table>("scene").unwrap().get(*name).unwrap(),
            "app" => rve.get::<Table>("app").unwrap().get(*name).unwrap(),
            "script_ref" => match rve.get::<LuaValue>("__parity_ref").unwrap() {
                LuaValue::UserData(reference) => reference.get(*name).unwrap(),
                other => panic!("script reference is {}", other.type_name()),
            },
            _ => unreachable!(),
        };
        if matches!(capability, HostCapability::TimeScale) {
            assert!(matches!(
                lua_value,
                LuaValue::Number(_) | LuaValue::Integer(_)
            ));
        } else {
            assert!(
                matches!(lua_value, LuaValue::Function(_)),
                "Lua API missing {table}.{name}"
            );
        }
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
            HostCapability::GetCollider => abi.physics_get_collider.is_some(),
            HostCapability::SetCollider => abi.physics_set_collider.is_some(),
            HostCapability::GetRigidBody => abi.physics_get_rigid_body.is_some(),
            HostCapability::SetRigidBody => abi.physics_set_rigid_body.is_some(),
            HostCapability::GetController => abi.physics_get_controller.is_some(),
            HostCapability::SetController => abi.physics_set_controller.is_some(),
            HostCapability::SimpleMove => abi.physics_simple_move.is_some(),
            HostCapability::Jump => abi.physics_jump.is_some(),
            HostCapability::VerticalVelocity => abi.physics_vertical_velocity.is_some(),
            HostCapability::Overlaps => abi.physics_overlaps.is_some(),
            HostCapability::PhysicsOverlap => abi.physics_overlap.is_some(),
            HostCapability::PhysicsRaycastAll => abi.physics_raycast_all.is_some(),
            HostCapability::PhysicsShapeCast => abi.physics_shape_cast.is_some(),
            HostCapability::LightGet => abi.light_get.is_some(),
            HostCapability::LightSet => abi.light_set.is_some(),
            HostCapability::MeshGetVisible => abi.mesh_get_visible.is_some(),
            HostCapability::MeshSetVisible => abi.mesh_set_visible.is_some(),
            HostCapability::MeshSetMaterialParams => abi.mesh_set_material_params.is_some(),
            HostCapability::MeshSetMaterialPath => abi.mesh_set_material_path.is_some(),
            HostCapability::ParticlesGetPlaying => abi.particles_get_playing.is_some(),
            HostCapability::ParticlesSetPlaying => abi.particles_set_playing.is_some(),
            HostCapability::ParticlesCommand => abi.particles_command.is_some(),
            HostCapability::EnvironmentGet => abi.environment_get.is_some(),
            HostCapability::EnvironmentSet => abi.environment_set.is_some(),
            HostCapability::LodGet => abi.lod_get.is_some(),
            HostCapability::LodSet => abi.lod_set.is_some(),
            HostCapability::AudioGetSource => abi.audio_get_source.is_some(),
            HostCapability::AudioSetSource => abi.audio_set_source.is_some(),
            HostCapability::AudioPlayClipEx => abi.audio_play_clip_ex.is_some(),
            HostCapability::AudioSetVoicePosition => abi.audio_set_voice_position.is_some(),
            HostCapability::TimeScale => {
                abi.script_time.fixed_dt > 0.0 && abi.set_time_scale.is_some()
            }
            HostCapability::TimerAfter | HostCapability::TimerEvery => abi.schedule_timer.is_some(),
            HostCapability::TimerCancel => abi.cancel_timer.is_some(),
            HostCapability::SaveSet => abi.save_set.is_some(),
            HostCapability::SaveGet => abi.save_get.is_some(),
            HostCapability::SaveDelete => abi.save_delete.is_some(),
            HostCapability::SaveHas => abi.save_has.is_some(),
            HostCapability::SaveKeys => abi.save_keys.is_some(),
            HostCapability::SaveFlush => abi.save_flush.is_some(),
            HostCapability::KeepOnLoad => abi.object_keep_on_load.is_some(),
            HostCapability::SetKeepOnLoad => abi.object_set_keep_on_load.is_some(),
            HostCapability::ObjectScript => abi.object_script.is_some(),
            HostCapability::ScriptRefGet => abi.script_ref_get.is_some(),
            HostCapability::ScriptRefSet => abi.script_ref_set.is_some(),
            HostCapability::ScriptRefSend => abi.script_ref_send.is_some(),
            HostCapability::ScriptRefEnabled => abi.script_ref_enabled.is_some(),
            HostCapability::ScriptRefSetEnabled => abi.script_ref_set_enabled.is_some(),
            HostCapability::SceneLoad => abi.scene_load.is_some(),
            HostCapability::SceneCurrent => abi.scene_current.is_some(),
            HostCapability::AppQuit => abi.app_quit.is_some(),
        };
        assert!(available, "ABI callback missing for {table}.{name}");
    }
}
