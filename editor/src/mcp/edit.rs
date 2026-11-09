use super::read::{component_value, parse_component_kind, required_string};
use crate::scene_edit::{Component, ComponentKind, NewObject};
use crate::state::EditorState;
use engine_files::VirtualPath;
use engine_mcp::{Permission, ToolSpec};
use engine_scene::{
    AnimatorComponent, AudioSourceComponent, CameraComponent, CharacterControllerComponent,
    ColliderComponent, DirectionalLightComponent, EnvironmentComponent, LodGroup, MeshRenderer,
    NetworkIdentityComponent, NetworkTransformComponent, ParamValue, ParticleEmitterComponent,
    PointLightComponent, RigidBodyComponent, SceneTransform, SpotLightComponent,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    let mut tools = Vec::new();
    tools.push(ToolSpec::new(
        "select_objects",
        "Replace the current selection with the listed scene object ids.",
        json!({"type":"object","properties":{"ids":{"type":"array","items":{"type":"string"}}},"required":["ids"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "create_object",
        "Create one scene object. Mesh and prefab kinds require their respective asset path.",
        json!({"type":"object","properties":{"kind":{"type":"string","enum":["empty","camera","directional_light","point_light","spot_light","environment","mesh","prefab"]},"parent":{"type":["string","null"]},"position":{"type":"array","items":{"type":"number"},"minItems":3,"maxItems":3},"model":{"type":"string"},"prefab":{"type":"string"}},"required":["kind"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "delete_objects",
        "Delete the listed objects and their descendants in one undoable scene edit.",
        json!({"type":"object","properties":{"ids":{"type":"array","items":{"type":"string"},"minItems":1}},"required":["ids"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "duplicate_objects",
        "Duplicate the listed objects, or the current selection when ids is omitted.",
        json!({"type":"object","properties":{"ids":{"type":"array","items":{"type":"string"}}},"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "set_parent",
        "Set an object's parent; pass null to make it a root object.",
        json!({"type":"object","properties":{"id":{"type":"string"},"parent":{"type":["string","null"]}},"required":["id","parent"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    for name in ["rename_object", "change_id"] {
        tools.push(ToolSpec::new(
            name,
            "Change an object's id and update references.",
            json!({"type":"object","properties":{"id":{"type":"string"},"new_id":{"type":"string"}},"required":["id","new_id"],"additionalProperties":false}),
            Permission::EditScene,
        ));
    }
    tools.push(ToolSpec::new(
        "set_transform",
        "Replace an object's transform.",
        json!({"type":"object","properties":{"id":{"type":"string"},"transform":{"type":"object"}},"required":["id","transform"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "set_component",
        "Merge component JSON over its current value by default, validate, and store one undoable edit.",
        json!({"type":"object","properties":{"id":{"type":"string"},"kind":{"type":"string","enum":["transform","camera","mesh_renderer","lod_group","directional_light","point_light","spot_light","environment","collider","rigid_body","character_controller","audio_source","animator","particle_emitter","network_identity","network_transform","ui_canvas"]},"value":{"type":"object"},"merge":{"type":"boolean"}},"required":["id","kind","value"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "remove_component",
        "Remove a component using its scene command and validation rules.",
        json!({"type":"object","properties":{"id":{"type":"string"},"kind":{"type":"string","enum":["camera","mesh_renderer","lod_group","directional_light","point_light","spot_light","environment","collider","rigid_body","character_controller","audio_source","animator","particle_emitter","network_identity","network_transform","ui_canvas"]}},"required":["id","kind"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "set_script_param",
        "Set a JSON scalar or array parameter on a lite or heavy script attached to an object.",
        json!({"type":"object","properties":{"id":{"type":"string"},"kind":{"type":"string","enum":["lite","heavy"]},"script":{"type":"string"},"param":{"type":"string"},"value":{}},"required":["id","kind","script","param","value"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    tools.push(ToolSpec::new(
        "set_camera_active",
        "Set the active camera through the editor's camera command.",
        json!({"type":"object","properties":{"id":{"type":"string"},"active":{"type":"boolean"}},"required":["id","active"],"additionalProperties":false}),
        Permission::EditScene,
    ));
    for name in ["undo", "redo"] {
        tools.push(ToolSpec::new(
            name,
            "Undo or redo one scene edit.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            Permission::EditScene,
        ));
    }
    tools
}

pub(super) fn call(
    tool: &str,
    arguments: &Value,
    state: &mut EditorState,
) -> Result<Value, String> {
    require_project(state)?;
    if state.is_playing() {
        return Err("scene edits are unavailable during Play".to_owned());
    }
    match tool {
        "select_objects" => select_objects(arguments, state),
        "create_object" => create_object(arguments, state),
        "delete_objects" => delete_objects(arguments, state),
        "duplicate_objects" => duplicate_objects(arguments, state),
        "set_parent" => set_parent(arguments, state),
        "rename_object" | "change_id" => change_id(arguments, state),
        "set_transform" => set_transform(arguments, state),
        "set_component" => set_component(arguments, state),
        "remove_component" => remove_component(arguments, state),
        "set_script_param" => set_script_param(arguments, state),
        "set_camera_active" => set_camera_active(arguments, state),
        "undo" => history(state, false),
        "redo" => history(state, true),
        _ => Err(format!("unknown or unimplemented tool: {tool}")),
    }
}

fn select_objects(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let ids = arguments["ids"]
        .as_array()
        .ok_or_else(|| "ids must be an array".to_owned())?
        .iter()
        .map(|id| {
            id.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "each id must be a string".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let scene = scene_mut(state)?;
    for id in &ids {
        if scene.object(id).is_none() {
            return Err(format!("object {id:?} was not found"));
        }
    }
    if let Some(first) = ids.first() {
        scene.select(Some(first));
        for id in &ids[1..] {
            scene.add_to_selection(id);
        }
    } else {
        scene.select(None);
    }
    Ok(json!({"selection": scene.selected_ids()}))
}

fn create_object(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let kind_name = required_string(arguments, "kind")?;
    let position = optional_position(arguments, "position")?.unwrap_or([0.0; 3]);
    let kind = match kind_name {
        "empty" => NewObject::Empty,
        "camera" => NewObject::Camera,
        "directional_light" => NewObject::DirectionalLight,
        "point_light" => NewObject::PointLight,
        "spot_light" => NewObject::SpotLight,
        "environment" => NewObject::Environment,
        "mesh" => NewObject::Mesh(
            VirtualPath::parse(required_string(arguments, "model")?)
                .map_err(|_| "model must be an Assets virtual path".to_owned())?,
            position,
        ),
        "prefab" => NewObject::Prefab(
            VirtualPath::parse(required_string(arguments, "prefab")?)
                .map_err(|_| "prefab must be an Assets virtual path".to_owned())?,
            position,
        ),
        other => return Err(format!("unknown object kind: {other}")),
    };
    let parent = nullable_string(arguments, "parent")?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    let id = scene.create_object(&kind, parent.as_deref())?;
    Ok(json!({"id": id}))
}

fn delete_objects(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let ids = string_array(arguments, "ids")?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    scene.delete_objects(&ids)?;
    Ok(json!({"deleted": ids}))
}

fn duplicate_objects(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let ids = optional_string_array(arguments, "ids")?;
    let scene = scene_mut(state)?;
    if let Some(ids) = ids {
        for id in &ids {
            if scene.object(id).is_none() {
                return Err(format!("object {id:?} was not found"));
            }
        }
        if let Some(first) = ids.first() {
            scene.select(Some(first));
            for id in &ids[1..] {
                scene.add_to_selection(id);
            }
        } else {
            scene.select(None);
        }
    }
    scene.end_continuous_edit();
    let duplicated = scene.duplicate_selection()?;
    Ok(json!({"ids": duplicated}))
}

fn set_parent(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let parent = nullable_string(arguments, "parent")?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    scene.set_parent(id, parent.as_deref())?;
    Ok(json!({"id": id, "parent": parent}))
}

fn change_id(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let new_id = required_string(arguments, "new_id")?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    scene.change_id(id, new_id)?;
    Ok(json!({"id": new_id}))
}

fn set_transform(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let transform: SceneTransform = parse_value(arguments.get("transform"), "transform")?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    scene.set_transform(id, transform, None)?;
    Ok(json!({"id": id}))
}

fn set_component(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let kind_name = required_string(arguments, "kind")?;
    let kind = parse_component_kind(kind_name)?;
    let patch = arguments
        .get("value")
        .ok_or_else(|| "value is required".to_owned())?;
    if !patch.is_object() {
        return Err("value must be a JSON object".to_owned());
    }
    let merge = arguments
        .get("merge")
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| "merge must be a boolean".to_owned())
        })
        .transpose()?
        .unwrap_or(true);
    let scene = scene_mut(state)?;
    let value = if merge {
        match scene
            .object(id)
            .and_then(|object| Component::read(object, kind))
        {
            Some(current) => {
                let mut current = component_value(current)?;
                merge_json(&mut current, patch.clone());
                current
            }
            None => patch.clone(),
        }
    } else {
        patch.clone()
    };
    let component = component_from_value(kind, value)?;
    scene.end_continuous_edit();
    match component {
        Component::Camera(camera) => scene.set_camera_component(id, camera)?,
        Component::MeshRenderer(mesh) => scene.set_mesh_renderer(id, mesh)?,
        Component::LodGroup(group) => scene.set_lod_group(id, Some(group))?,
        component => scene.set_component(id, component)?,
    }
    Ok(json!({"id": id, "kind": kind_name}))
}

fn remove_component(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let kind_name = required_string(arguments, "kind")?;
    let kind = parse_component_kind(kind_name)?;
    let scene = scene_mut(state)?;
    if Component::read(
        scene
            .object(id)
            .ok_or_else(|| format!("object {id:?} was not found"))?,
        kind,
    )
    .is_none()
    {
        return Err(format!("object {id:?} has no {kind_name} component"));
    }
    scene.end_continuous_edit();
    match kind {
        ComponentKind::Transform => return Err("transform cannot be removed".to_owned()),
        ComponentKind::Camera => scene.remove_camera(id)?,
        ComponentKind::MeshRenderer => scene.remove_mesh_renderer(id)?,
        ComponentKind::LodGroup => scene.set_lod_group(id, None)?,
        ComponentKind::Collider | ComponentKind::RigidBody | ComponentKind::CharacterController => {
            scene.remove_physics_component(id, kind)?
        }
        ComponentKind::AudioSource => scene.remove_audio_source(id)?,
        ComponentKind::Animator => scene.remove_animator(id)?,
        ComponentKind::ParticleEmitter => scene.remove_particle_emitter(id)?,
        ComponentKind::NetworkIdentity => scene.remove_network_identity(id)?,
        ComponentKind::NetworkTransform => scene.remove_network_transform(id)?,
        ComponentKind::UiCanvas => scene.edit_object(id, None, |o| {
            o.ui_canvas = None;
            Ok(())
        })?,
        _ => scene.edit_object(id, None, |object| {
            match kind {
                ComponentKind::DirectionalLight => object.directional_light = None,
                ComponentKind::PointLight => object.point_light = None,
                ComponentKind::SpotLight => object.spot_light = None,
                ComponentKind::Environment => object.environment = None,
                _ => unreachable!(),
            }
            Ok(())
        })?,
    }
    Ok(json!({"id": id, "removed": kind_name}))
}

fn set_script_param(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let script_kind = required_string(arguments, "kind")?;
    let script_name = required_string(arguments, "script")?;
    let param = required_string(arguments, "param")?;
    if param.is_empty() {
        return Err("param must not be empty".to_owned());
    }
    let value: ParamValue = serde_json::from_value(
        arguments
            .get("value")
            .cloned()
            .ok_or_else(|| "value is required".to_owned())?,
    )
    .map_err(|error| format!("invalid script parameter: {error}"))?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    scene.edit_object(id, None, |object| {
        let params = match script_kind {
            "lite" => object
                .lite_scripts
                .iter_mut()
                .find(|script| script.path.as_str() == script_name)
                .map(|script| &mut script.params),
            "heavy" => object
                .heavy_scripts
                .iter_mut()
                .find(|script| script.name == script_name)
                .map(|script| &mut script.params),
            other => return Err(format!("unknown script kind: {other}")),
        }
        .ok_or_else(|| format!("script {script_name:?} was not found on {id:?}"))?;
        params.insert(param.to_owned(), value);
        Ok(())
    })?;
    Ok(json!({"id": id, "script": script_name, "param": param}))
}

fn set_camera_active(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let active = arguments
        .get("active")
        .and_then(Value::as_bool)
        .ok_or_else(|| "active must be a boolean".to_owned())?;
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    scene.set_camera_active(id, active)?;
    Ok(json!({"id": id, "active": active}))
}

fn history(state: &mut EditorState, redo: bool) -> Result<Value, String> {
    let scene = scene_mut(state)?;
    scene.end_continuous_edit();
    let changed = if redo { scene.redo() } else { scene.undo() };
    Ok(json!({"changed": changed}))
}

fn scene_mut(state: &mut EditorState) -> Result<&mut crate::scene_edit::EditorScene, String> {
    state
        .scene
        .as_mut()
        .ok_or_else(|| "no scene is open".to_owned())
}

fn require_project(state: &EditorState) -> Result<(), String> {
    if state.project.is_some() {
        Ok(())
    } else {
        Err("project is not open".to_owned())
    }
}

fn optional_position(arguments: &Value, key: &str) -> Result<Option<[f32; 3]>, String> {
    arguments
        .get(key)
        .map(|value| {
            serde_json::from_value::<[f32; 3]>(value.clone())
                .map_err(|error| format!("{key}: {error}"))
        })
        .transpose()
}

fn nullable_string(arguments: &Value, key: &str) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string or null")),
    }
}

fn string_array(arguments: &Value, key: &str) -> Result<Vec<String>, String> {
    optional_string_array(arguments, key)?.ok_or_else(|| format!("{key} is required"))
}

fn optional_string_array(arguments: &Value, key: &str) -> Result<Option<Vec<String>>, String> {
    arguments
        .get(key)
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| format!("{key} must be an array"))?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("each {key} item must be a string"))
                })
                .collect()
        })
        .transpose()
}

fn parse_value<T: DeserializeOwned>(value: Option<&Value>, name: &str) -> Result<T, String> {
    serde_json::from_value(
        value
            .cloned()
            .ok_or_else(|| format!("{name} is required"))?,
    )
    .map_err(|error| format!("invalid {name}: {error}"))
}

fn component_from_value(kind: ComponentKind, value: Value) -> Result<Component, String> {
    macro_rules! parse {
        ($type:ty, $variant:ident) => {
            serde_json::from_value::<$type>(value)
                .map(Component::$variant)
                .map_err(|error| format!("invalid {} component: {error}", stringify!($variant)))
        };
    }
    match kind {
        ComponentKind::Transform => parse!(SceneTransform, Transform),
        ComponentKind::Camera => parse!(CameraComponent, Camera),
        ComponentKind::MeshRenderer => parse!(MeshRenderer, MeshRenderer),
        ComponentKind::LodGroup => parse!(LodGroup, LodGroup),
        ComponentKind::DirectionalLight => parse!(DirectionalLightComponent, DirectionalLight),
        ComponentKind::PointLight => parse!(PointLightComponent, PointLight),
        ComponentKind::SpotLight => parse!(SpotLightComponent, SpotLight),
        ComponentKind::Environment => parse!(EnvironmentComponent, Environment),
        ComponentKind::Collider => parse!(ColliderComponent, Collider),
        ComponentKind::RigidBody => parse!(RigidBodyComponent, RigidBody),
        ComponentKind::CharacterController => {
            parse!(CharacterControllerComponent, CharacterController)
        }
        ComponentKind::AudioSource => parse!(AudioSourceComponent, AudioSource),
        ComponentKind::Animator => parse!(AnimatorComponent, Animator),
        ComponentKind::ParticleEmitter => parse!(ParticleEmitterComponent, ParticleEmitter),
        ComponentKind::NetworkIdentity => parse!(NetworkIdentityComponent, NetworkIdentity),
        ComponentKind::NetworkTransform => parse!(NetworkTransformComponent, NetworkTransform),
        ComponentKind::UiCanvas => parse!(engine_scene::UiCanvasComponent, UiCanvas),
    }
}

fn merge_json(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                if let Some(existing) = base.get_mut(&key) {
                    merge_json(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, patch) => *base = patch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_project::create_project;
    use crate::project::OpenProject;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn ui_canvas_component_round_trips_through_mcp_and_rejects_invalid_paths() {
        let kind = parse_component_kind("ui_canvas").unwrap();
        let component = component_from_value(
            kind,
            json!({"document":"/ui/hud.rveui","visible":false,"layer":7}),
        )
        .unwrap();
        let value = component_value(component).unwrap();
        assert_eq!(value["layer"], 7);
        assert_eq!(value["visible"], false);
        assert!(component_from_value(kind, json!({"document":"../outside.rveui"})).is_err());
    }

    struct TempProject {
        root: PathBuf,
        state: EditorState,
    }

    impl TempProject {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = std::env::temp_dir().join(format!(
                "rve-mcp-edit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&parent).unwrap();
            let root = create_project("Fixture", &parent).unwrap();
            let mut state = EditorState::new(None);
            let project = OpenProject::open(&root).unwrap();
            let path = project.settings.starting_scene().unwrap();
            let source = project.read_text(path).unwrap();
            let scene = crate::scene_edit::EditorScene::parse(
                VirtualPath::parse(path).unwrap(),
                &source,
                project.prefab_loader(),
            )
            .unwrap();
            state.project = Some(project);
            state.scene = Some(scene);
            Self {
                root: parent,
                state,
            }
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn set_component_merges_fields_and_invalid_values_leave_the_document_unchanged() {
        let mut fixture = TempProject::new();
        let before = serde_json::to_value(
            fixture
                .state
                .scene
                .as_ref()
                .unwrap()
                .object("camera")
                .unwrap()
                .transform,
        )
        .unwrap();
        call(
            "set_component",
            &json!({"id":"camera","kind":"transform","value":{"position":[3.0,4.0,5.0]}}),
            &mut fixture.state,
        )
        .unwrap();
        let after = serde_json::to_value(
            fixture
                .state
                .scene
                .as_ref()
                .unwrap()
                .object("camera")
                .unwrap()
                .transform,
        )
        .unwrap();
        assert_eq!(after["position"], json!([3.0, 4.0, 5.0]));
        assert_eq!(after["scale"], before["scale"]);

        let document = fixture
            .state
            .scene
            .as_ref()
            .unwrap()
            .document()
            .to_toml()
            .unwrap();
        let error = call(
            "set_component",
            &json!({"id":"camera","kind":"transform","value":{"position":["bad",0,0]}}),
            &mut fixture.state,
        )
        .unwrap_err();
        assert!(error.contains("invalid Transform component"));
        assert_eq!(
            fixture
                .state
                .scene
                .as_ref()
                .unwrap()
                .document()
                .to_toml()
                .unwrap(),
            document
        );
    }

    #[test]
    fn one_mcp_edit_is_one_undo_step_and_can_be_redone() {
        let mut fixture = TempProject::new();
        let initial = fixture
            .state
            .scene
            .as_ref()
            .unwrap()
            .document()
            .to_toml()
            .unwrap();
        call(
            "set_component",
            &json!({"id":"camera","kind":"transform","value":{"position":[3.0,4.0,5.0]}}),
            &mut fixture.state,
        )
        .unwrap();
        let changed = fixture
            .state
            .scene
            .as_ref()
            .unwrap()
            .document()
            .to_toml()
            .unwrap();
        assert_ne!(changed, initial);
        call("undo", &json!({}), &mut fixture.state).unwrap();
        assert_eq!(
            fixture
                .state
                .scene
                .as_ref()
                .unwrap()
                .document()
                .to_toml()
                .unwrap(),
            initial
        );
        call("redo", &json!({}), &mut fixture.state).unwrap();
        assert_eq!(
            fixture
                .state
                .scene
                .as_ref()
                .unwrap()
                .document()
                .to_toml()
                .unwrap(),
            changed
        );
    }

    #[test]
    fn scene_edits_are_rejected_during_play() {
        let mut fixture = TempProject::new();
        fixture.state.scene = Some(crate::scene_edit::EditorScene::new_untitled().unwrap());
        fixture.state.play = Some(
            crate::play::PlaySession::start(
                fixture.state.scene.as_ref().unwrap().view(),
                None,
                |_| Ok(String::new()),
            )
            .unwrap(),
        );
        let before = fixture
            .state
            .scene
            .as_ref()
            .unwrap()
            .document()
            .to_toml()
            .unwrap();
        let error = call(
            "set_component",
            &json!({"id":"camera","kind":"transform","value":{"position":[1,2,3]}}),
            &mut fixture.state,
        )
        .unwrap_err();
        assert!(error.contains("during Play"));
        assert_eq!(
            fixture
                .state
                .scene
                .as_ref()
                .unwrap()
                .document()
                .to_toml()
                .unwrap(),
            before
        );
    }
}
