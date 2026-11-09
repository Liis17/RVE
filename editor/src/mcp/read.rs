use super::paths;
use crate::assets_fs::{AssetKind, AssetsDir};
use crate::project::OpenProject;
use crate::scene_edit::{
    Component, ComponentKind, EditorScene, default_camera, default_environment, default_light,
};
use crate::state::EditorState;
use engine_files::VirtualPath;
use engine_mcp::Permissions;
use engine_project::{BootstrapConfig, BootstrapPresentMode, BootstrapReflexMode};
use engine_scene::ParamSchema;
use engine_scene::{
    AnimatorComponent, AudioSourceComponent, CharacterControllerComponent, ColliderComponent,
    HeavyScript, LiteScript, LodGroup, MeshRenderer, NetworkIdentityComponent,
    NetworkTransformComponent, ParticleEmitterComponent, PointLightComponent, RigidBodyComponent,
    SceneObject, SpotLightComponent,
};
use serde_json::{Value, json};
use std::path::Path;

const MAX_TEXT_FILE_BYTES: u64 = 1024 * 1024;
const DEFAULT_LOG_LIMIT: usize = 100;
const MAX_LOG_LIMIT: usize = 1000;

pub(super) fn call(
    tool: &str,
    arguments: &Value,
    state: &mut EditorState,
    permissions: Permissions,
) -> Result<Value, String> {
    match tool {
        "editor_status" => Ok(editor_status(state, permissions)),
        "list_assets" => list_assets(arguments, state),
        "list_project_files" => list_project_files(arguments, state),
        "read_text_file" => read_text_file(arguments, state),
        "get_project_settings" => get_project_settings(state),
        "get_scene_hierarchy" => get_scene_hierarchy(arguments, state),
        "get_object" => get_object(arguments, state),
        "get_component" => get_component(arguments, state),
        "describe_components" => Ok(describe_components()),
        "get_asset_settings" => get_asset_settings(arguments, state),
        "get_script_params_schema" => get_script_params_schema(arguments, state),
        "get_console_log" => Ok(get_console_log(arguments, state)?),
        _ => Err(format!("unknown or unimplemented tool: {tool}")),
    }
}

fn editor_status(state: &EditorState, permissions: Permissions) -> Value {
    let project = state.project.as_ref();
    let scene = state.scene.as_ref();
    json!({
        "versions": {
            "editor": crate::VERSION,
            "engine_mcp": engine_mcp::VERSION,
            "engine_project": engine_project::VERSION,
        },
        "project": project.map(|project| json!({
            "name": project.settings.name,
            "path": project.root.display().to_string(),
        })),
        "scene": scene.map(|scene| json!({
            "path": scene.path().map(|path| path.as_str()),
            "dirty": scene.is_dirty(),
        })),
        "dirty": state.has_unsaved_changes(),
        "playing": state.is_playing(),
        "permissions": permission_value(permissions),
        "selection": scene.and_then(EditorScene::selection),
        "panel_sizes": {
            "scene": state.scene_view.size,
            "game": state.game_view.size,
        },
    })
}

fn list_assets(arguments: &Value, state: &EditorState) -> Result<Value, String> {
    let project = open_project(state)?;
    let folder = optional_string(arguments, "folder")?.unwrap_or("");
    let recursive = optional_bool(arguments, "recursive")?.unwrap_or(false);
    let kind = optional_string(arguments, "kind")?
        .map(parse_asset_kind)
        .transpose()?;
    let directory = paths::resolve_assets_directory(&project.root, folder)?;
    let assets_root = paths::resolve_assets_directory(&project.root, "")?;
    let folder = directory
        .strip_prefix(&assets_root)
        .map_err(|_| "folder is outside Assets".to_owned())?
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    let tree = AssetsDir::new(&directory)
        .list()
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let mut entries = Vec::new();
    flatten_assets(&tree, &folder, recursive, kind, &mut entries);
    Ok(json!({ "assets": entries }))
}

fn flatten_assets(
    tree: &[crate::assets_fs::AssetEntry],
    prefix: &str,
    recursive: bool,
    kind: Option<AssetKind>,
    entries: &mut Vec<Value>,
) {
    for entry in tree {
        if entry.name.eq_ignore_ascii_case(".rve") {
            continue;
        }
        let path = if prefix.is_empty() {
            entry.path.clone()
        } else {
            format!("{prefix}/{}", entry.path)
        };
        if entry.is_dir || kind.is_none_or(|kind| AssetKind::of(&path) == kind) {
            entries.push(json!({
                "name": entry.name,
                "path": path,
                "is_dir": entry.is_dir,
                "kind": (!entry.is_dir).then(|| asset_kind_name(AssetKind::of(&path))),
            }));
        }
        if recursive && entry.is_dir {
            flatten_assets(&entry.children, prefix, true, kind, entries);
        }
    }
}

fn list_project_files(arguments: &Value, state: &EditorState) -> Result<Value, String> {
    let project = open_project(state)?;
    let dir = optional_string(arguments, "dir")?.unwrap_or("");
    let directory = if dir.is_empty() {
        paths::resolve_read_path(&project.root, ".")?
    } else {
        paths::resolve_read_path(&project.root, dir)?
    };
    if !directory.is_dir() {
        return Err(format!("{} is not a directory", dir));
    }
    let project_root = std::fs::canonicalize(&project.root)
        .map_err(|error| format!("{}: {error}", project.root.display()))?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let hidden_root_entry =
            directory == project_root && name.to_string_lossy().starts_with('.');
        if hidden_root_entry || name.eq_ignore_ascii_case(".rve") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())
            .map_err(|error| format!("{}: {error}", entry.path().display()))?;
        if paths::is_link_or_junction(&metadata) {
            continue;
        }
        let Some(name_text) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        let relative = path
            .strip_prefix(&project_root)
            .map_err(|_| "listed path is outside the project".to_owned())?;
        entries.push(json!({
            "name": name_text,
            "path": path_string(relative),
            "is_dir": metadata.is_dir(),
        }));
    }
    entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(json!({ "entries": entries }))
}

fn read_text_file(arguments: &Value, state: &EditorState) -> Result<Value, String> {
    let project = open_project(state)?;
    let path = required_string(arguments, "path")?;
    let file = paths::resolve_read_path(&project.root, path)?;
    let metadata =
        std::fs::metadata(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", path));
    }
    if metadata.len() > MAX_TEXT_FILE_BYTES {
        return Err(format!("{} exceeds the 1 MiB text limit", path));
    }
    let contents = std::fs::read_to_string(&file)
        .map_err(|error| format!("{} is not a readable UTF-8 text file: {error}", path))?;
    Ok(json!({ "path": path, "content": contents }))
}

fn get_project_settings(state: &EditorState) -> Result<Value, String> {
    let project = open_project(state)?;
    let project_file = paths::resolve_read_path(&project.root, "project.rveproject")?;
    let project_settings = engine_project::ProjectSettings::load(&project_file)?;
    let project_settings =
        serde_json::to_value(project_settings).map_err(|error| error.to_string())?;
    // Отсутствующий bootstrap.rves — штатный случай (значения по умолчанию), поэтому это `null`, а не ошибка.
    let bootstrap_path = project.root.join("bootstrap.rves");
    let bootstrap = match std::fs::symlink_metadata(&bootstrap_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        _ => {
            let bootstrap_file = paths::resolve_read_path(&project.root, "bootstrap.rves")?;
            let bootstrap_text = read_limited_text(&bootstrap_file, "bootstrap.rves")?;
            let config = BootstrapConfig::parse(&bootstrap_text)?;
            json!({
                "path": bootstrap_file.display().to_string(),
                "settings": bootstrap_value(&config),
            })
        }
    };
    Ok(json!({
        "project": {
            "path": project_file.display().to_string(),
            "settings": project_settings,
        },
        "bootstrap": bootstrap,
    }))
}

fn bootstrap_value(config: &BootstrapConfig) -> Value {
    json!({
        "window_size_x": config.window_size_x,
        "window_size_y": config.window_size_y,
        "window_title": config.window_title,
        "allow_multiple_instances": config.allow_multiple_instances,
        "app_icon": config.app_icon,
        "present_mode": config.present_mode.map(|mode| match mode {
            BootstrapPresentMode::Fifo => "Fifo",
            BootstrapPresentMode::Mailbox => "Mailbox",
            BootstrapPresentMode::Immediate => "Immediate",
        }),
        "reflex": config.reflex.map(|mode| match mode {
            BootstrapReflexMode::Off => "Off",
            BootstrapReflexMode::On => "On",
            BootstrapReflexMode::Boost => "Boost",
        }),
    })
}

fn get_scene_hierarchy(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    if source(arguments)? == "play" {
        let play = state
            .play
            .as_mut()
            .ok_or_else(|| "Play source requested without an active Play session".to_owned())?;
        let world = play.world_mut();
        let objects = world.objects();
        let hierarchy = objects
            .iter()
            .map(|object| {
                let children = objects
                    .iter()
                    .filter(|child| child.parent == Some(object.handle))
                    .map(|child| child.id.as_str())
                    .collect::<Vec<_>>();
                json!({
                    "id": object.id,
                    "name": object.name,
                    "parent": object.parent.and_then(|handle| world.object(handle)).map(|parent| parent.id.as_str()),
                    "children": children,
                    "components": runtime_component_kinds(object),
                    "prefab": null,
                })
            })
            .collect::<Vec<_>>();
        return Ok(json!({"objects": hierarchy, "source": "play"}));
    }
    open_project(state)?;
    let scene = open_scene(state)?;
    let objects = &scene.view().objects;
    let hierarchy = objects
        .iter()
        .map(|object| {
            let children = objects
                .iter()
                .filter(|child| child.parent.as_deref() == Some(object.id.as_str()))
                .map(|child| child.id.as_str())
                .collect::<Vec<_>>();
            json!({
                "id": object.id,
                "name": object.display_name(),
                "parent": object.parent,
                "children": children,
                "components": component_kinds(object),
                "prefab": scene.origin(&object.id).map(|origin| json!({
                    "instance": origin.instance,
                    "target": origin.target,
                })),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "objects": hierarchy, "source": "edit" }))
}

fn get_object(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    if source(arguments)? == "play" {
        let id = required_string(arguments, "id")?;
        let play = state
            .play
            .as_mut()
            .ok_or_else(|| "Play source requested without an active Play session".to_owned())?;
        let world = play.world_mut();
        let object = world
            .object_by_id(id)
            .ok_or_else(|| format!("object {id:?} was not found in Play"))?;
        return Ok(
            json!({"object": runtime_object_value(world, object), "origin": null, "source": "play"}),
        );
    }
    open_project(state)?;
    let scene = open_scene(state)?;
    let id = required_string(arguments, "id")?;
    let object = scene
        .object(id)
        .ok_or_else(|| format!("object {id:?} was not found"))?;
    let value = serde_json::to_value(object).map_err(|error| error.to_string())?;
    Ok(json!({
        "object": value,
        "origin": scene.origin(id).map(|origin| json!({
            "instance": origin.instance,
            "target": origin.target,
        })),
        "source": "edit",
    }))
}

fn source(arguments: &Value) -> Result<&str, String> {
    let source = arguments
        .get("source")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "source must be a string".to_owned())
        })
        .transpose()?
        .unwrap_or("edit");
    match source {
        "edit" => Ok("edit"),
        "play" => Ok("play"),
        other => Err(format!("unknown scene source {other:?}")),
    }
}

fn runtime_component_kinds(object: &engine_scene::WorldObject) -> Vec<&'static str> {
    let mut components = vec!["transform"];
    components.extend(
        [
            (object.camera.is_some(), "camera"),
            (object.mesh_renderer.is_some(), "mesh_renderer"),
            (object.lod_group.is_some(), "lod_group"),
            (object.directional_light.is_some(), "directional_light"),
            (object.point_light.is_some(), "point_light"),
            (object.spot_light.is_some(), "spot_light"),
            (object.environment.is_some(), "environment"),
            (object.collider.is_some(), "collider"),
            (object.rigid_body.is_some(), "rigid_body"),
            (
                object.character_controller.is_some(),
                "character_controller",
            ),
            (object.audio_source.is_some(), "audio_source"),
            (object.animator.is_some(), "animator"),
            (object.particle_emitter.is_some(), "particle_emitter"),
            (object.network_identity.is_some(), "network_identity"),
            (object.network_transform.is_some(), "network_transform"),
            (object.ui_canvas.is_some(), "ui_canvas"),
        ]
        .into_iter()
        .filter_map(|(present, name)| present.then_some(name)),
    );
    components
}

fn runtime_object_value(world: &engine_scene::World, object: &engine_scene::WorldObject) -> Value {
    json!({
        "id": object.id,
        "name": object.name,
        "parent": object.parent.and_then(|handle| world.object(handle)).map(|parent| parent.id.as_str()),
        "transform": {
            "position": object.transform.position.to_array(),
            "rotation": object.transform.rotation_deg.to_array(),
            "scale": object.transform.scale.to_array(),
        },
        "camera": object.camera,
        "camera_settings": object.camera_settings,
        "post_process": object.post_process,
        "mesh_renderer": object.mesh_renderer,
        "lod_group": object.lod_group,
        "directional_light": object.directional_light,
        "point_light": object.point_light,
        "spot_light": object.spot_light,
        "environment": object.environment,
        "collider": object.collider,
        "rigid_body": object.rigid_body,
        "character_controller": object.character_controller,
        "audio_source": object.audio_source,
        "animator": object.animator,
        "particle_emitter": object.particle_emitter,
        "network_identity": object.network_identity,
        "network_transform": object.network_transform,
        "ui_canvas": object.ui_canvas,
        "lite_scripts": object.lite_scripts,
        "heavy_scripts": object.heavy_scripts,
    })
}

fn get_component(arguments: &Value, state: &EditorState) -> Result<Value, String> {
    open_project(state)?;
    let scene = open_scene(state)?;
    let id = required_string(arguments, "id")?;
    let kind_name = required_string(arguments, "kind")?;
    let kind = parse_component_kind(kind_name)?;
    let object = scene
        .object(id)
        .ok_or_else(|| format!("object {id:?} was not found"))?;
    let component = Component::read(object, kind)
        .ok_or_else(|| format!("object {id:?} has no {kind_name} component"))?;
    Ok(json!({
        "id": id,
        "kind": kind_name,
        "value": component_value(component)?,
    }))
}

fn describe_components() -> Value {
    let sample = component_sample();
    let defaults = component_kind_list()
        .into_iter()
        .map(|(kind, name)| {
            let default = Component::read(&sample, kind)
                .and_then(|component| component_value(component).ok())
                .unwrap_or_else(|| json!({}));
            json!({ "kind": name, "default": default })
        })
        .collect::<Vec<_>>();
    json!({ "components": defaults })
}

fn component_sample() -> SceneObject {
    let virtual_path = |path: &str| VirtualPath::parse(path).expect("schema asset path is valid");
    SceneObject {
        id: "schema".to_owned(),
        transform: Default::default(),
        camera: Some(default_camera()),
        mesh_renderer: Some(MeshRenderer {
            model: virtual_path("models/example.fbx"),
            texture: None,
            material: None,
            materials: Vec::new(),
        }),
        lod_group: Some(LodGroup {
            levels: Vec::new(),
            cull_distance: None,
        }),
        directional_light: Some(default_light()),
        point_light: Some(PointLightComponent::default()),
        spot_light: Some(SpotLightComponent::default()),
        environment: Some(default_environment()),
        collider: Some(ColliderComponent::default()),
        rigid_body: Some(RigidBodyComponent::default()),
        character_controller: Some(CharacterControllerComponent::default()),
        audio_source: Some(AudioSourceComponent::with_clip(virtual_path(
            "audio/example.wav",
        ))),
        animator: Some(AnimatorComponent::new()),
        particle_emitter: Some(ParticleEmitterComponent::default()),
        network_identity: Some(NetworkIdentityComponent::default()),
        network_transform: Some(NetworkTransformComponent::default()),
        ui_canvas: Some(engine_scene::UiCanvasComponent {
            document: virtual_path("ui/example.rveui"),
            visible: true,
            layer: None,
        }),
        ..SceneObject::default()
    }
}

fn component_kinds(object: &SceneObject) -> Vec<&'static str> {
    component_kind_list()
        .into_iter()
        .filter_map(|(kind, name)| Component::read(object, kind).map(|_| name))
        .collect()
}

fn component_kind_list() -> [(ComponentKind, &'static str); 17] {
    [
        (ComponentKind::Transform, "transform"),
        (ComponentKind::Camera, "camera"),
        (ComponentKind::MeshRenderer, "mesh_renderer"),
        (ComponentKind::LodGroup, "lod_group"),
        (ComponentKind::DirectionalLight, "directional_light"),
        (ComponentKind::PointLight, "point_light"),
        (ComponentKind::SpotLight, "spot_light"),
        (ComponentKind::Environment, "environment"),
        (ComponentKind::Collider, "collider"),
        (ComponentKind::RigidBody, "rigid_body"),
        (ComponentKind::CharacterController, "character_controller"),
        (ComponentKind::AudioSource, "audio_source"),
        (ComponentKind::Animator, "animator"),
        (ComponentKind::ParticleEmitter, "particle_emitter"),
        (ComponentKind::NetworkIdentity, "network_identity"),
        (ComponentKind::NetworkTransform, "network_transform"),
        (ComponentKind::UiCanvas, "ui_canvas"),
    ]
}

pub(super) fn component_value(component: Component) -> Result<Value, String> {
    match component {
        Component::Transform(value) => serde_json::to_value(value),
        Component::Camera(value) => serde_json::to_value(value),
        Component::MeshRenderer(value) => serde_json::to_value(value),
        Component::LodGroup(value) => serde_json::to_value(value),
        Component::DirectionalLight(value) => serde_json::to_value(value),
        Component::PointLight(value) => serde_json::to_value(value),
        Component::SpotLight(value) => serde_json::to_value(value),
        Component::Environment(value) => serde_json::to_value(value),
        Component::Collider(value) => serde_json::to_value(value),
        Component::RigidBody(value) => serde_json::to_value(value),
        Component::CharacterController(value) => serde_json::to_value(value),
        Component::AudioSource(value) => serde_json::to_value(value),
        Component::Animator(value) => serde_json::to_value(value),
        Component::ParticleEmitter(value) => serde_json::to_value(value),
        Component::NetworkIdentity(value) => serde_json::to_value(value),
        Component::NetworkTransform(value) => serde_json::to_value(value),
        Component::UiCanvas(value) => serde_json::to_value(value),
    }
    .map_err(|error| error.to_string())
}

fn get_asset_settings(arguments: &Value, state: &EditorState) -> Result<Value, String> {
    let project = open_project(state)?;
    let input = required_string(arguments, "path")?;
    let path = paths::resolve_asset_file(&project.root, input)?;
    let assets = paths::resolve_assets_directory(&project.root, "")?;
    if !path.starts_with(&assets) {
        return Err(format!("{} is outside Assets", input));
    }
    let contents = read_limited_text(&path, input)?;
    let value = match AssetKind::of(input) {
        AssetKind::CameraSettings => serde_json::to_value(
            engine_scene::CameraSettings::parse(input, &contents)
                .map_err(|error| error.to_string())?,
        ),
        AssetKind::PostProcess => serde_json::to_value(
            engine_scene::PostProcessSettings::parse(input, &contents)
                .map_err(|error| error.to_string())?,
        ),
        AssetKind::PhysicsMaterial => serde_json::to_value(
            engine_scene::PhysicsMaterialSettings::parse(input, &contents)
                .map_err(|error| error.to_string())?,
        ),
        _ => return Err("path must be .camerasettings, .postprocess, or .rvephysmat".to_owned()),
    }
    .map_err(|error| error.to_string())?;
    Ok(json!({ "path": input, "settings": value }))
}

fn get_script_params_schema(arguments: &Value, state: &mut EditorState) -> Result<Value, String> {
    open_project(state)?;
    let id = required_string(arguments, "id")?;
    let object = open_scene(state)?
        .object(id)
        .ok_or_else(|| format!("object {id:?} was not found"))?;
    let lite_scripts = object.lite_scripts.clone();
    let heavy_scripts = object.heavy_scripts.clone();
    let lite = lite_scripts
        .iter()
        .map(|script: &LiteScript| {
            let schema = state.lite_schema(script.path.as_str())?;
            Ok(json!({
                "path": script.path.as_str(),
                "schema": schema.as_ref().map(param_schema_value),
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let heavy = heavy_scripts
        .iter()
        .map(|script: &HeavyScript| {
            json!({
                "name": script.name,
                "schema": state.heavy_schema(&script.name).as_ref().map(param_schema_value),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "id": id, "lite": lite, "heavy": heavy }))
}

fn param_schema_value(schema: &ParamSchema) -> Value {
    let parameters = schema
        .specs()
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "kind": spec.kind.name(),
                "default": spec.default,
                "min": spec.min,
                "max": spec.max,
                "label": spec.label,
            })
        })
        .collect::<Vec<_>>();
    json!({ "parameters": parameters })
}

fn get_console_log(arguments: &Value, state: &EditorState) -> Result<Value, String> {
    let since = optional_u64(arguments, "since")?.unwrap_or(0) as usize;
    let level = optional_string(arguments, "level")?
        .map(parse_log_level)
        .transpose()?;
    let limit = optional_u64(arguments, "limit")?
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_LOG_LIMIT)
        .clamp(1, MAX_LOG_LIMIT);
    let entries = state
        .log
        .entries()
        .enumerate()
        .filter(|(index, entry)| *index >= since && level.is_none_or(|level| entry.level == level))
        .take(limit)
        .map(|(index, entry)| {
            json!({
                "sequence": index,
                "level": match entry.level {
                    crate::console::LogLevel::Info => "info",
                    crate::console::LogLevel::Warn => "warn",
                    crate::console::LogLevel::Error => "error",
                },
                "message": entry.message,
            })
        })
        .collect::<Vec<_>>();
    let next_since = entries
        .last()
        .and_then(|entry| entry["sequence"].as_u64())
        .map_or(since, |index| index as usize + 1);
    Ok(json!({ "entries": entries, "next_since": next_since }))
}

fn open_project(state: &EditorState) -> Result<&OpenProject, String> {
    state
        .project
        .as_ref()
        .ok_or_else(|| "project is not open".to_owned())
}

fn open_scene(state: &EditorState) -> Result<&EditorScene, String> {
    state
        .scene
        .as_ref()
        .ok_or_else(|| "scene is not open".to_owned())
}

pub(super) fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} must be a string"))
}

pub(super) fn optional_string<'a>(
    arguments: &'a Value,
    name: &str,
) -> Result<Option<&'a str>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        _ => Err(format!("{name} must be a string")),
    }
}

fn optional_bool(arguments: &Value, name: &str) -> Result<Option<bool>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(format!("{name} must be a boolean")),
    }
}

pub(super) fn optional_u64(arguments: &Value, name: &str) -> Result<Option<u64>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{name} must be a non-negative integer")),
    }
}

fn parse_asset_kind(input: &str) -> Result<AssetKind, String> {
    match input {
        "scene" => Ok(AssetKind::Scene),
        "prefab" => Ok(AssetKind::Prefab),
        "camera_settings" => Ok(AssetKind::CameraSettings),
        "post_process" => Ok(AssetKind::PostProcess),
        "screen_shader" => Ok(AssetKind::ScreenShader),
        "physics_material" => Ok(AssetKind::PhysicsMaterial),
        "material" => Ok(AssetKind::Material),
        "shader" => Ok(AssetKind::Shader),
        "model" => Ok(AssetKind::Model),
        "texture" => Ok(AssetKind::Texture),
        "hdr" => Ok(AssetKind::Hdr),
        "audio" => Ok(AssetKind::Audio),
        "script" => Ok(AssetKind::Script),
        "other" => Ok(AssetKind::Other),
        _ => Err(format!("unknown asset kind: {input}")),
    }
}

fn asset_kind_name(kind: AssetKind) -> &'static str {
    match kind {
        AssetKind::Scene => "scene",
        AssetKind::Prefab => "prefab",
        AssetKind::CameraSettings => "camera_settings",
        AssetKind::PostProcess => "post_process",
        AssetKind::ScreenShader => "screen_shader",
        AssetKind::PhysicsMaterial => "physics_material",
        AssetKind::Material => "material",
        AssetKind::Shader => "shader",
        AssetKind::Model => "model",
        AssetKind::Texture => "texture",
        AssetKind::Hdr => "hdr",
        AssetKind::Audio => "audio",
        AssetKind::Script => "script",
        AssetKind::Other => "other",
        AssetKind::Ui => "ui",
    }
}

pub(super) fn parse_component_kind(input: &str) -> Result<ComponentKind, String> {
    component_kind_list()
        .into_iter()
        .find_map(|(kind, name)| (name == input).then_some(kind))
        .ok_or_else(|| format!("unknown component kind: {input}"))
}

fn parse_log_level(input: &str) -> Result<crate::console::LogLevel, String> {
    match input {
        "info" => Ok(crate::console::LogLevel::Info),
        "warn" => Ok(crate::console::LogLevel::Warn),
        "error" => Ok(crate::console::LogLevel::Error),
        _ => Err(format!("unknown console level: {input}")),
    }
}

fn permission_value(permissions: Permissions) -> Value {
    json!({
        "read": true,
        "edit_scene": permissions.edit_scene,
        "assets": permissions.assets,
        "play": permissions.play,
        "build": permissions.build,
    })
}

fn path_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn read_limited_text(path: &Path, label: &str) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("{label}: {error}"))?;
    if metadata.len() > MAX_TEXT_FILE_BYTES {
        return Err(format!("{label} exceeds the 1 MiB text limit"));
    }
    std::fs::read_to_string(path)
        .map_err(|error| format!("{label} is not a readable UTF-8 text file: {error}"))
}

#[cfg(test)]
mod tests {
    use super::call;
    use crate::new_project::create_project;
    use crate::project::OpenProject;
    use crate::scene_edit::EditorScene;
    use crate::state::EditorState;
    use engine_files::VirtualPath;
    use engine_mcp::Permissions;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct ProjectFixture(PathBuf);

    impl ProjectFixture {
        fn new() -> (Self, EditorState) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = std::env::temp_dir().join(format!(
                "rve-editor-mcp-read-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&parent).unwrap();
            let root = create_project("Fixture", &parent).unwrap();
            let project = OpenProject::open(&root).unwrap();
            let path = project.settings.starting_scene().unwrap().to_owned();
            let source = project.read_text(&path).unwrap();
            let scene = EditorScene::parse(
                VirtualPath::parse(&path).unwrap(),
                &source,
                project.prefab_loader(),
            )
            .unwrap();
            let mut state = EditorState::new(None);
            state.project = Some(project);
            state.scene = Some(scene);
            (Self(parent), state)
        }
    }

    impl Drop for ProjectFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_bootstrap_is_null_and_hidden_root_entries_are_not_listed_or_read() {
        let (_fixture, mut state) = ProjectFixture::new();
        let root = state.project.as_ref().unwrap().root.clone();
        let permissions = Permissions::default();
        std::fs::remove_file(root.join("bootstrap.rves")).unwrap();
        let settings = call("get_project_settings", &json!({}), &mut state, permissions).unwrap();
        assert!(settings["bootstrap"].is_null());
        assert_eq!(settings["project"]["settings"]["name"], "Fixture");

        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".env"), "TOKEN=1").unwrap();
        let files = call("list_project_files", &json!({}), &mut state, permissions).unwrap();
        let names = files["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(names.contains(&"Assets".to_owned()));
        assert!(!names.iter().any(|name| name.starts_with('.')));
        assert!(
            call(
                "read_text_file",
                &json!({"path": ".env"}),
                &mut state,
                permissions
            )
            .is_err()
        );
        assert!(
            call(
                "read_text_file",
                &json!({"path": ".git"}),
                &mut state,
                permissions
            )
            .is_err()
        );
    }

    #[test]
    fn screen_shader_asset_kind_name_round_trips_through_list_filter() {
        let kind = crate::assets_fs::AssetKind::of("fx/vhs.glsl");
        assert_eq!(kind, crate::assets_fs::AssetKind::ScreenShader);
        assert_eq!(
            super::parse_asset_kind(super::asset_kind_name(kind)),
            Ok(kind)
        );
    }

    #[test]
    fn read_tools_return_project_assets_scene_component_and_log_json() {
        let (_fixture, mut state) = ProjectFixture::new();
        state.log.info("ready");
        let permissions = Permissions::default();

        let status = call("editor_status", &json!({}), &mut state, permissions).unwrap();
        assert_eq!(status["project"]["name"], "Fixture");
        assert_eq!(status["scene"]["path"], "scenes/main.rvescene");
        assert_eq!(status["playing"], false);

        let assets = call(
            "list_assets",
            &json!({"recursive": true, "kind": "script"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert!(
            assets["assets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|asset| { asset["path"] == "scripts/jump.lua" && asset["kind"] == "script" })
        );

        let files = call("list_project_files", &json!({}), &mut state, permissions).unwrap();
        assert!(
            files["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["name"] == "project.rveproject" })
        );
        assert!(
            !files["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["name"] == ".rve" })
        );

        let text = call(
            "read_text_file",
            &json!({"path": "Assets/scripts/jump.lua"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert!(
            text["content"]
                .as_str()
                .unwrap()
                .contains("Jump.properties")
        );

        let project_settings =
            call("get_project_settings", &json!({}), &mut state, permissions).unwrap();
        assert_eq!(project_settings["project"]["settings"]["name"], "Fixture");
        assert_eq!(
            project_settings["bootstrap"]["settings"]["window_size_x"],
            1280
        );

        let hierarchy = call("get_scene_hierarchy", &json!({}), &mut state, permissions).unwrap();
        assert!(hierarchy["objects"].as_array().unwrap().len() >= 3);

        let object = call(
            "get_object",
            &json!({"id": "camera"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert_eq!(object["object"]["id"], "camera");

        let component = call(
            "get_component",
            &json!({"id": "camera", "kind": "camera"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert_eq!(component["value"]["active"], false);

        let described = call("describe_components", &json!({}), &mut state, permissions).unwrap();
        assert_eq!(described["components"].as_array().unwrap().len(), 17);

        let settings = call(
            "get_asset_settings",
            &json!({"path": "cameras/default.camerasettings"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert!(settings["settings"]["projection"].is_object());
        let post_process = call(
            "get_asset_settings",
            &json!({"path": "cameras/default.postprocess"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert!(post_process["settings"]["bloom"].is_object());
        let project_root = state.project.as_ref().unwrap().root.clone();
        std::fs::create_dir_all(project_root.join("Assets/materials")).unwrap();
        std::fs::write(
            project_root.join("Assets/materials/test.rvephysmat"),
            "static_friction = 0.4\ndynamic_friction = 0.3\nbounciness = 0.2\n",
        )
        .unwrap();
        let physics = call(
            "get_asset_settings",
            &json!({"path": "materials/test.rvephysmat"}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert!((physics["settings"]["static_friction"].as_f64().unwrap() - 0.4).abs() < 1e-5);

        let schema = call(
            "get_script_params_schema",
            &json!({"id": "cube"}),
            &mut state,
            permissions,
        )
        .unwrap();
        let names = schema["lite"][0]["schema"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|parameter| parameter["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"height"));
        assert!(names.contains(&"duration"));

        let log = call(
            "get_console_log",
            &json!({"since": 0, "level": "info", "limit": 5}),
            &mut state,
            permissions,
        )
        .unwrap();
        assert_eq!(log["entries"][0]["message"], "ready");
    }

    #[test]
    fn project_tools_report_missing_project_and_bad_arguments_as_errors() {
        let mut state = EditorState::new(None);
        assert_eq!(
            call(
                "list_assets",
                &json!({}),
                &mut state,
                Permissions::default()
            )
            .unwrap_err(),
            "project is not open"
        );
        let (_fixture, mut state) = ProjectFixture::new();
        assert!(
            call(
                "read_text_file",
                &json!({"path": "Assets/missing.txt"}),
                &mut state,
                Permissions::default(),
            )
            .is_err()
        );
        assert!(
            call(
                "get_component",
                &json!({"id": "camera", "kind": "missing"}),
                &mut state,
                Permissions::default(),
            )
            .is_err()
        );
    }
}
