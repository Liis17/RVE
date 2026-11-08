use super::paths;
use crate::app::EditorApp;
use crate::assets_fs::AssetKind;
use engine_files::VirtualPath;
use engine_mcp::{Permission, Permissions, ToolSpec};
use engine_scene::{CameraSettings, PhysicsMaterialSettings, PostProcessSettings};
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "save_scene",
            "Save the current scene to its existing path or an explicit Assets path.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "open_scene",
            "Open a scene from Assets; discard_changes must be true to discard a dirty scene.",
            json!({"type":"object","properties":{"path":{"type":"string"},"discard_changes":{"type":"boolean"}},"required":["path"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "new_scene",
            "Replace the current scene with a new untitled scene; dirty scenes require discard_changes.",
            json!({"type":"object","properties":{"discard_changes":{"type":"boolean"}},"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "open_project",
            "Open a directory containing project.rveproject; dirty state requires discard_changes.",
            json!({"type":"object","properties":{"dir":{"type":"string"},"discard_changes":{"type":"boolean"}},"required":["dir"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "write_asset_settings",
            "Validate and save an existing camera, post-process, or physics-material settings asset.",
            json!({"type":"object","properties":{"path":{"type":"string"},"value":{"type":"object"}},"required":["path","value"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "create_folder",
            "Create a folder below Assets.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "write_text_file",
            "Write UTF-8 text to a file below Assets.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "rename_asset",
            "Rename a file or folder within Assets.",
            json!({"type":"object","properties":{"path":{"type":"string"},"new_path":{"type":"string"}},"required":["path","new_path"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "delete_asset",
            "Delete a file or folder below Assets.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "import_files",
            "Copy external files into Assets and refresh the Project tree.",
            json!({"type":"object","properties":{"files":{"type":"array","items":{"type":"string"},"minItems":1},"folder":{"type":"string"}},"required":["files"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "create_prefab",
            "Save an object subtree as a prefab asset and replace it with an instance.",
            json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}),
            Permission::Assets,
        ),
        ToolSpec::new(
            "apply_to_prefab",
            "Write an instance's overrides to its prefab asset and clear those overrides.",
            json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}),
            Permission::Assets,
        ),
    ]
}

pub(super) fn call(app: &mut EditorApp, tool: &str, arguments: &Value) -> Result<Value, String> {
    match tool {
        "save_scene" => {
            let path = optional_string(arguments, "path")?;
            let normalized;
            let path = if let Some(path) = path {
                if extension(path) != "rvescene" {
                    return Err(format!("{path}: scene path must end with .rvescene"));
                }
                let root = project_root(app)?;
                let destination = paths::resolve_write_path(&root, path)?;
                normalized = normalized_path(&root, &destination)?;
                Some(normalized.as_str())
            } else {
                None
            };
            let path = app.mcp_save_scene(path)?;
            Ok(json!({"path": path}))
        }
        "open_scene" => {
            let path = required_string(arguments, "path")?;
            let discard = optional_bool(arguments, "discard_changes")?.unwrap_or(false);
            let root = project_root(app)?;
            let canonical = paths::resolve_asset_file(&root, path)?;
            let virtual_path = virtual_asset_path(&root, &canonical)?;
            if scene_dirty(app) && !discard {
                return Err(
                    "scene has unsaved changes; pass discard_changes=true to continue".to_owned(),
                );
            }
            app.mcp_open_scene(&virtual_path, discard)?;
            Ok(json!({"path": virtual_path}))
        }
        "new_scene" => {
            let discard = optional_bool(arguments, "discard_changes")?.unwrap_or(false);
            if scene_dirty(app) && !discard {
                return Err(
                    "scene has unsaved changes; pass discard_changes=true to continue".to_owned(),
                );
            }
            app.mcp_new_scene(discard)?;
            Ok(json!({"created": true}))
        }
        "write_asset_settings"
        | "create_folder"
        | "write_text_file"
        | "rename_asset"
        | "delete_asset"
        | "import_files" => call_file_tool(app, tool, arguments),
        "create_prefab" => create_prefab(app, arguments),
        "apply_to_prefab" => apply_to_prefab(app, arguments),
        _ => Err(format!("unknown or unimplemented tool: {tool}")),
    }
}

/// Открывает проект. Авто-сборка скриптов (`heavy_scripts`) запускает cargo, поэтому выполняется
/// только при праве Build; без него проект открывается и ответ сообщает, что сборка пропущена.
pub(super) fn open_project(
    app: &mut EditorApp,
    arguments: &Value,
    permissions: Permissions,
) -> Result<Value, String> {
    let dir = PathBuf::from(required_string(arguments, "dir")?);
    let discard = optional_bool(arguments, "discard_changes")?.unwrap_or(false);
    let project_file = dir.join("project.rveproject");
    if !project_file.is_file() {
        return Err(format!(
            "{} does not contain project.rveproject",
            dir.display()
        ));
    }
    if app.mcp_state_mut().has_unsaved_changes() && !discard {
        return Err(
            "project state has unsaved changes; pass discard_changes=true to continue".to_owned(),
        );
    }
    let build_scripts = permissions.build;
    app.mcp_open_project(&dir, build_scripts, discard)?;
    let mut result =
        json!({"project": std::fs::canonicalize(&dir).unwrap_or(dir).display().to_string()});
    if !build_scripts && app.mcp_scripts_build_needed() {
        result["scripts_build"] = json!(
            "skipped: scripts build runs cargo and requires the build_scripts permission (Build)"
        );
    }
    Ok(result)
}

fn call_file_tool(app: &mut EditorApp, tool: &str, arguments: &Value) -> Result<Value, String> {
    let root = project_root(app)?;
    let result = match tool {
        "write_asset_settings" => write_asset_settings(app, &root, arguments)?,
        "create_folder" => {
            let input = required_string(arguments, "path")?;
            let destination = paths::resolve_write_path(&root, input)?;
            if destination.exists() {
                return Err(format!("{input} already exists"));
            }
            fs::create_dir(&destination).map_err(|error| format!("{input}: {error}"))?;
            json!({"path": normalized_path(&root, &destination)?})
        }
        "write_text_file" => {
            let input = required_string(arguments, "path")?;
            let content = required_string(arguments, "content")?;
            let destination = paths::resolve_write_path(&root, input)?;
            fs::write(&destination, content).map_err(|error| format!("{input}: {error}"))?;
            json!({"path": normalized_path(&root, &destination)?})
        }
        "rename_asset" => {
            let source = required_string(arguments, "path")?;
            let destination = required_string(arguments, "new_path")?;
            let (source_path, destination_path) =
                paths::resolve_rename_paths(&root, source, destination)?;
            if destination_path.exists() && !is_case_only_rename(&source_path, &destination_path) {
                return Err(format!(
                    "{destination} already exists; rename does not overwrite"
                ));
            }
            fs::rename(&source_path, &destination_path)
                .map_err(|error| format!("{source} -> {destination}: {error}"))?;
            json!({"path": normalized_path(&root, &destination_path)?})
        }
        "delete_asset" => {
            let input = required_string(arguments, "path")?;
            let target = paths::resolve_delete_path(&root, input)?;
            let metadata =
                fs::symlink_metadata(&target).map_err(|error| format!("{input}: {error}"))?;
            if metadata.is_dir() {
                fs::remove_dir_all(&target).map_err(|error| format!("{input}: {error}"))?;
            } else {
                fs::remove_file(&target).map_err(|error| format!("{input}: {error}"))?;
            }
            json!({"deleted": normalized_path(&root, &target)?})
        }
        "import_files" => import_files(&root, arguments)?,
        _ => return Err(format!("unknown file tool: {tool}")),
    };
    app.mcp_refresh_tree();
    Ok(result)
}

fn write_asset_settings(
    app: &mut EditorApp,
    root: &Path,
    arguments: &Value,
) -> Result<Value, String> {
    let input = required_string(arguments, "path")?;
    let value = arguments
        .get("value")
        .cloned()
        .ok_or_else(|| "value is required".to_owned())?;
    let destination = paths::resolve_write_path(root, input)?;
    if !destination.is_file() {
        return Err(format!("{input}: settings asset does not exist"));
    }
    let path = VirtualPath::parse(&normalized_path(root, &destination)?)
        .map_err(|_| format!("{input}: invalid Assets path"))?;
    let project = app
        .mcp_state_mut()
        .project
        .as_ref()
        .ok_or_else(|| "project is not open".to_owned())?;
    match extension(input).as_str() {
        "camerasettings" => {
            let settings: CameraSettings = parse_settings(value, input)?;
            project.save_camera_settings(&path, &settings)?;
        }
        "postprocess" => {
            let settings: PostProcessSettings = parse_settings(value, input)?;
            project.save_post_process(&path, &settings)?;
        }
        "rvephysmat" => {
            let settings: PhysicsMaterialSettings = parse_settings(value, input)?;
            project.save_physics_material(&path, &settings)?;
        }
        other => return Err(format!("{input}: unsupported settings extension .{other}")),
    }
    Ok(json!({"path": path.as_str()}))
}

fn parse_settings<T: serde::de::DeserializeOwned>(value: Value, path: &str) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("{path}: invalid settings: {error}"))
}

/// Смена регистра имени одного и того же файла: на регистронезависимой ФС цель «существует», хотя это источник.
fn is_case_only_rename(source: &Path, destination: &Path) -> bool {
    let (Some(from), Some(to)) = (source.file_name(), destination.file_name()) else {
        return false;
    };
    from != to
        && from.to_string_lossy().to_lowercase() == to.to_string_lossy().to_lowercase()
        && matches!(
            (fs::canonicalize(source), fs::canonicalize(destination)),
            (Ok(a), Ok(b)) if a == b
        )
}

fn import_files(root: &Path, arguments: &Value) -> Result<Value, String> {
    let files = arguments["files"]
        .as_array()
        .ok_or_else(|| "files must be an array".to_owned())?;
    let folder = optional_string(arguments, "folder")?.unwrap_or("");
    let folder_path = paths::resolve_assets_directory(root, folder)?;
    let folder_name = normalized_path(root, &folder_path)?;
    // Все источники проверяются до первого копирования: при любом отказе импорт не начинается.
    let mut sources = Vec::with_capacity(files.len());
    let mut errors = Vec::new();
    for source in files {
        let Some(source) = source.as_str() else {
            errors.push("each files item must be a path string".to_owned());
            continue;
        };
        match check_import_source(Path::new(source)) {
            Ok(name) => sources.push((PathBuf::from(source), name)),
            Err(error) => errors.push(error),
        }
    }
    if !errors.is_empty() {
        return Err(format!("import rejected: {}", errors.join("; ")));
    }
    let mut imported = Vec::with_capacity(sources.len());
    for (source, name) in &sources {
        imported.push(import_one(root, source, name, &folder_name)?);
    }
    Ok(json!({"imported": imported}))
}

/// Разрешён только обычный файл известного типа ассета, не ссылка и не junction.
fn check_import_source(source: &Path) -> Result<String, String> {
    let display = source.display();
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{display}: filename is not UTF-8"))?;
    validate_name(name)?;
    let metadata = fs::symlink_metadata(source).map_err(|error| format!("{display}: {error}"))?;
    if paths::is_link_or_junction(&metadata) {
        return Err(format!(
            "{display}: symbolic links and junctions cannot be imported"
        ));
    }
    if !metadata.is_file() {
        return Err(format!("{display} is not a file"));
    }
    if AssetKind::of(name) == AssetKind::Other {
        return Err(format!("{display}: file type is not an importable asset"));
    }
    Ok(name.to_owned())
}

fn import_one(root: &Path, source: &Path, name: &str, folder: &str) -> Result<String, String> {
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    for number in 0_u32.. {
        let candidate = match (number, extension) {
            (0, _) => name.to_owned(),
            (_, Some(extension)) => format!("{stem} ({number}).{extension}"),
            (_, None) => format!("{stem} ({number})"),
        };
        let path = join(folder, &candidate);
        let target = paths::resolve_write_path(root, &path)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(file) => {
                drop(file);
                if let Err(error) = fs::copy(source, &target) {
                    let _ = fs::remove_file(&target);
                    return Err(format!("{} -> {path}: {error}", source.display()));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("{path}: {error}")),
        }
    }
    unreachable!("u32 suffixes are enough for one Assets folder")
}

fn create_prefab(app: &mut EditorApp, arguments: &Value) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    if id.is_empty() || id.contains(['/', '\\']) {
        return Err("id must be one object id, without path separators".to_owned());
    }
    let root = project_root(app)?;
    let created_directory = ensure_directory(&root, "prefabs")?;
    let output = format!("prefabs/{id}.rveprefab");
    let result = (|| {
        let target = paths::resolve_write_path(&root, &output)?;
        if target.exists() {
            return Err(format!("{output} already exists"));
        }
        app.mcp_create_prefab(id)
    })();
    if created_directory && result.is_err() {
        app.mcp_refresh_tree();
    }
    let path = result?;
    Ok(json!({"path": path}))
}

fn apply_to_prefab(app: &mut EditorApp, arguments: &Value) -> Result<Value, String> {
    let id = required_string(arguments, "id")?;
    let root = project_root(app)?;
    let path = {
        let state = app.mcp_state_mut();
        let scene = state
            .scene
            .as_ref()
            .ok_or_else(|| "no scene is open".to_owned())?;
        let (path, _) = scene.prefab_with_override(id)?;
        path
    };
    paths::resolve_write_path(&root, path.as_str())?;
    let path = app.mcp_apply_to_prefab(id)?;
    Ok(json!({"path": path}))
}

fn ensure_directory(root: &Path, path: &str) -> Result<bool, String> {
    let target = paths::resolve_write_path(root, path)?;
    if !target.exists() {
        fs::create_dir(&target).map_err(|error| format!("{path}: {error}"))?;
        Ok(true)
    } else if !target.is_dir() {
        Err(format!("{path} is not a directory"))
    } else {
        Ok(false)
    }
}

fn project_root(app: &mut EditorApp) -> Result<PathBuf, String> {
    app.mcp_state_mut()
        .project
        .as_ref()
        .map(|project| project.root.clone())
        .ok_or_else(|| "project is not open".to_owned())
}

fn scene_dirty(app: &mut EditorApp) -> bool {
    app.mcp_state_mut()
        .scene
        .as_ref()
        .is_some_and(crate::scene_edit::EditorScene::is_dirty)
}

fn virtual_asset_path(root: &Path, file: &Path) -> Result<String, String> {
    let assets = paths::resolve_assets_directory(root, "")?;
    let relative = file
        .strip_prefix(assets)
        .map_err(|_| "file is outside Assets".to_owned())?;
    Ok(path_string(relative))
}

fn normalized_path(root: &Path, path: &Path) -> Result<String, String> {
    let assets = paths::resolve_assets_directory(root, "")?;
    let relative = path
        .strip_prefix(assets)
        .map_err(|_| "path is outside Assets".to_owned())?;
    Ok(path_string(relative))
}

fn path_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with(['.', ' '])
        || name.contains(['/', '\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
    {
        return Err(format!("invalid Assets filename {name:?}"));
    }
    Ok(())
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} must be a string"))
}

fn optional_string<'a>(arguments: &'a Value, name: &str) -> Result<Option<&'a str>, String> {
    arguments
        .get(name)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| format!("{name} must be a string"))
        })
        .transpose()
}

fn optional_bool(arguments: &Value, name: &str) -> Result<Option<bool>, String> {
    arguments
        .get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| format!("{name} must be a boolean"))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_project::create_project;
    use crate::project::OpenProject;
    use crate::state::EditorState;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempProject {
        parent: PathBuf,
        state: EditorState,
    }

    impl TempProject {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let parent = std::env::temp_dir().join(format!(
                "rve-mcp-assets-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&parent).unwrap();
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
            Self { parent, state }
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn file_tools_reject_traversal_and_paths_outside_assets() {
        let mut fixture = TempProject::new();
        let outside = fixture.parent.join("escaped.txt");
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        let error = call_file_tool(
            &mut app,
            "write_text_file",
            &json!({"path":"../escaped.txt","content":"bad"}),
        )
        .unwrap_err();
        assert!(error.contains("traversal"));
        assert!(!outside.exists());
        let error = call_file_tool(
            &mut app,
            "write_text_file",
            &json!({"path":outside.display().to_string(),"content":"bad"}),
        )
        .unwrap_err();
        assert!(error.contains("relative") || error.contains("Assets"));
        assert!(!outside.exists());
    }

    #[test]
    fn scene_save_open_and_discard_changes_use_explicit_paths_without_prompts() {
        let mut fixture = TempProject::new();
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        app.mcp_state_mut()
            .scene
            .as_mut()
            .unwrap()
            .create_object(&crate::scene_edit::NewObject::Empty, None)
            .unwrap();
        let before = app
            .mcp_state_mut()
            .scene
            .as_ref()
            .unwrap()
            .document()
            .to_toml()
            .unwrap();
        let error = call(
            &mut app,
            "open_scene",
            &json!({"path":"scenes/main.rvescene"}),
        )
        .unwrap_err();
        assert!(error.contains("discard_changes=true"));
        assert_eq!(
            app.mcp_state_mut()
                .scene
                .as_ref()
                .unwrap()
                .document()
                .to_toml()
                .unwrap(),
            before
        );
        assert!(app.mcp_state_mut().unsaved_prompt.is_none());

        let saved = call(
            &mut app,
            "save_scene",
            &json!({"path":"scenes/saved.rvescene"}),
        )
        .unwrap();
        assert_eq!(saved["path"], "scenes/saved.rvescene");
        assert!(
            app.mcp_state_mut()
                .project
                .as_ref()
                .unwrap()
                .root
                .join("Assets/scenes/saved.rvescene")
                .is_file()
        );

        app.mcp_state_mut()
            .scene
            .as_mut()
            .unwrap()
            .create_object(&crate::scene_edit::NewObject::Empty, None)
            .unwrap();
        call(
            &mut app,
            "open_scene",
            &json!({"path":"scenes/main.rvescene","discard_changes":true}),
        )
        .unwrap();
        assert_eq!(
            app.mcp_state_mut()
                .scene
                .as_ref()
                .unwrap()
                .path()
                .unwrap()
                .as_str(),
            "scenes/main.rvescene"
        );
        assert!(!app.mcp_state_mut().scene.as_ref().unwrap().is_dirty());
        assert!(app.mcp_state_mut().unsaved_prompt.is_none());
    }

    #[test]
    fn new_scene_requires_an_open_project() {
        let mut app = EditorApp::new_for_mcp_tests_with_state(EditorState::new(None));
        let error = call(&mut app, "new_scene", &json!({})).unwrap_err();
        assert_eq!(error, "project is not open");
        assert!(app.mcp_state_mut().scene.is_none());
    }

    #[test]
    fn open_project_without_build_permission_does_not_start_scripts_build() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().join(format!(
            "rve-mcp-open-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&parent).unwrap();
        let root = create_project("Heavy", &parent).unwrap();
        let file = root.join("project.rveproject");
        let mut settings = engine_project::ProjectSettings::load(&file).unwrap();
        settings.heavy_scripts = Some("HeavyScripts".to_owned());
        fs::write(&file, settings.to_toml().unwrap()).unwrap();

        let mut app = EditorApp::new_for_mcp_tests_with_state(EditorState::new(None));
        let result = open_project(
            &mut app,
            &json!({"dir": root.display().to_string()}),
            Permissions {
                assets: true,
                ..Permissions::default()
            },
        )
        .unwrap();

        assert!(app.mcp_state_mut().scripts_job.is_none());
        assert!(app.mcp_state_mut().project.is_some());
        assert!(
            result["scripts_build"]
                .as_str()
                .unwrap()
                .contains("build_scripts")
        );
        drop(app);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn import_rejects_unknown_types_and_links_without_a_partial_copy() {
        let mut fixture = TempProject::new();
        let outside = fixture.parent.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        fs::write(outside.join("texture.png"), b"png").unwrap();
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        let root = app.mcp_state_mut().project.as_ref().unwrap().root.clone();
        let texture = outside.join("texture.png").display().to_string();
        let secret = outside.join("secret.txt").display().to_string();

        let error = call_file_tool(
            &mut app,
            "import_files",
            &json!({"files": [texture, secret]}),
        )
        .unwrap_err();
        assert!(error.contains("secret.txt"));
        assert!(!root.join("Assets/texture.png").exists());

        let directory = outside.display().to_string();
        let error =
            call_file_tool(&mut app, "import_files", &json!({"files": [directory]})).unwrap_err();
        assert!(error.contains("not a file"));

        assert!(!root.join("Assets/texture.png").exists());
        assert!(!root.join("Assets/secret.txt").exists());

        #[cfg(unix)]
        {
            let link = outside.join("link.png");
            std::os::unix::fs::symlink(outside.join("texture.png"), &link).unwrap();
            let error = call_file_tool(
                &mut app,
                "import_files",
                &json!({"files": [link.display().to_string()]}),
            )
            .unwrap_err();
            assert!(error.contains("symbolic links"));
        }

        let imported = call_file_tool(
            &mut app,
            "import_files",
            &json!({"files": [outside.join("texture.png").display().to_string()]}),
        )
        .unwrap();
        assert_eq!(imported["imported"][0], "texture.png");
        assert!(root.join("Assets/texture.png").is_file());
    }

    #[test]
    fn rename_never_overwrites_an_existing_target() {
        let mut fixture = TempProject::new();
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        let root = app.mcp_state_mut().project.as_ref().unwrap().root.clone();
        fs::write(root.join("Assets/a.txt"), "first").unwrap();
        fs::write(root.join("Assets/b.txt"), "second").unwrap();

        let error = call_file_tool(
            &mut app,
            "rename_asset",
            &json!({"path": "a.txt", "new_path": "b.txt"}),
        )
        .unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(
            fs::read_to_string(root.join("Assets/a.txt")).unwrap(),
            "first"
        );
        assert_eq!(
            fs::read_to_string(root.join("Assets/b.txt")).unwrap(),
            "second"
        );
    }

    #[test]
    fn rename_allows_a_case_only_change_of_the_same_file() {
        let mut fixture = TempProject::new();
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        let root = app.mcp_state_mut().project.as_ref().unwrap().root.clone();
        fs::write(root.join("Assets/a.txt"), "first").unwrap();
        fs::write(root.join("Assets/b.txt"), "second").unwrap();

        call_file_tool(
            &mut app,
            "rename_asset",
            &json!({"path": "a.txt", "new_path": "A.txt"}),
        )
        .unwrap();
        let names: Vec<String> = fs::read_dir(root.join("Assets"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name == "A.txt"));
        assert!(!names.iter().any(|name| name == "a.txt"));
        assert_eq!(
            fs::read_to_string(root.join("Assets/A.txt")).unwrap(),
            "first"
        );

        // Регистр не даёт права перезаписать другой существующий файл.
        let error = call_file_tool(
            &mut app,
            "rename_asset",
            &json!({"path": "A.txt", "new_path": "b.txt"}),
        )
        .unwrap_err();
        assert!(error.contains("already exists"));
        assert_eq!(
            fs::read_to_string(root.join("Assets/b.txt")).unwrap(),
            "second"
        );
    }

    #[test]
    fn failed_open_scene_keeps_the_autosave_of_a_discarded_dirty_scene() {
        let mut fixture = TempProject::new();
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        let root = app.mcp_state_mut().project.as_ref().unwrap().root.clone();
        let scene_path = app.mcp_state_mut().scene.as_ref().unwrap().path().cloned();
        app.mcp_state_mut()
            .scene
            .as_mut()
            .unwrap()
            .create_object(&crate::scene_edit::NewObject::Empty, None)
            .unwrap();
        let store = crate::autosave::AutosaveStore::for_project(&root);
        store.write(scene_path.as_ref(), "autosaved").unwrap();

        // Путь существует в разрешениях MCP только после проверки, поэтому открытие вызывается напрямую.
        assert!(app.mcp_open_scene("scenes/missing.rvescene", true).is_err());
        assert_eq!(
            store.read(scene_path.as_ref()).as_deref(),
            Some("autosaved")
        );
        assert!(app.mcp_state_mut().scene.as_ref().unwrap().is_dirty());

        app.mcp_new_scene(true).unwrap();
        assert!(store.read(scene_path.as_ref()).is_none());
    }

    #[test]
    fn save_scene_accepts_only_the_rvescene_extension() {
        let mut fixture = TempProject::new();
        let state = std::mem::replace(&mut fixture.state, EditorState::new(None));
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);
        let error = call(&mut app, "save_scene", &json!({"path": "scenes/x.txt"})).unwrap_err();
        assert!(error.contains(".rvescene"));
        let root = app.mcp_state_mut().project.as_ref().unwrap().root.clone();
        assert!(!root.join("Assets/scenes/x.txt").exists());
    }
}
