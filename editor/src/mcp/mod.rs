mod assets;
mod build;
pub(crate) mod capture;
mod edit;
pub mod paths;
mod play;
mod read;

use crate::console::LogLevel;
use crate::settings::McpSettings;
use engine_mcp::{McpConfig, McpServer, Permission, Permissions, ToolResult, ToolSpec};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

const REBIND_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const REBIND_RETRY_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum McpStatus {
    #[default]
    Off,
    Starting,
    Running(u16),
    Error(String),
}

impl McpStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Off => "Off".to_owned(),
            Self::Starting => "Starting…".to_owned(),
            Self::Running(port) => format!("Running on 127.0.0.1:{port}"),
            Self::Error(error) => format!("Error: {error}"),
        }
    }
}

#[derive(Default)]
pub struct McpHost {
    server: Option<McpServer>,
    status: McpStatus,
    last_listener: Option<ListenerKey>,
    permissions: Permissions,
    pending: Option<PendingBind>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ListenerKey {
    enabled: bool,
    port: u16,
    token: String,
}

struct PendingBind {
    port: u16,
    token: String,
    permissions: Permissions,
    generation: u64,
    expires_at: Instant,
    retry_at: Instant,
}

impl McpHost {
    pub fn status(&self) -> &McpStatus {
        &self.status
    }

    pub fn initialize(
        &mut self,
        settings: &McpSettings,
        generation: u64,
        wake: Arc<dyn Fn() + Send + Sync>,
        log: &mut crate::console::ConsoleLog,
    ) {
        self.wake = Some(wake);
        self.apply_settings(settings, generation, log);
    }

    /// Applies listener settings, updates permissions in place, and retries transient rebinds.
    pub fn apply_settings(
        &mut self,
        settings: &McpSettings,
        generation: u64,
        log: &mut crate::console::ConsoleLog,
    ) {
        let key = ListenerKey {
            enabled: settings.enabled,
            port: settings.port,
            token: settings.token.clone(),
        };
        let permissions = settings.engine_permissions();
        if self.last_listener.as_ref() != Some(&key) {
            self.stop_listener();
            self.pending = None;
            self.last_listener = Some(key);
            self.permissions = permissions;
            if !settings.enabled {
                self.status = McpStatus::Off;
                return;
            }
            if !(1024..=u16::MAX).contains(&settings.port) {
                self.fail("MCP port must be between 1024 and 65535".to_owned(), log);
                return;
            }
            if settings.token.is_empty() {
                self.fail("MCP token is empty".to_owned(), log);
                return;
            }
            let now = Instant::now();
            self.try_start(
                PendingBind {
                    port: settings.port,
                    token: settings.token.clone(),
                    permissions,
                    generation,
                    expires_at: now + REBIND_RETRY_WINDOW,
                    retry_at: now,
                },
                now,
                log,
            );
        } else {
            if self.permissions != permissions {
                self.permissions = permissions;
                if let Some(server) = &self.server {
                    server.set_permissions(permissions);
                }
                if let Some(pending) = &mut self.pending {
                    pending.permissions = permissions;
                }
            }
            if let Some(pending) = &mut self.pending {
                pending.generation = generation;
            }
            if let Some(server) = &self.server {
                server.set_generation(generation);
            }
        }

        let now = Instant::now();
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.expires_at)
        {
            self.pending = None;
            self.fail(
                "MCP listener could not rebind within 2 seconds".to_owned(),
                log,
            );
        } else if self
            .pending
            .as_ref()
            .is_some_and(|pending| now >= pending.retry_at)
            && let Some(pending) = self.pending.take()
        {
            self.try_start(pending, now, log);
        }
    }

    pub fn drain(&mut self, app: &mut crate::app::EditorApp) -> bool {
        let mut handled = capture::poll(app, self.server.is_some());
        let Some(server) = &self.server else {
            return handled;
        };
        let generation = app.mcp_state_mut().mcp_generation;
        server.set_generation(generation);
        let permissions = server.permissions();
        while let Some(call) = server.try_recv() {
            if call.is_abandoned() {
                continue;
            }
            if call.name == "capture_view" {
                capture::begin(app, call);
                server.set_generation(app.mcp_state_mut().mcp_generation);
                handled = true;
                continue;
            }
            let result = execute_pending(
                &call.name,
                &call.arguments,
                call.required_permission,
                permissions,
                call.generation(),
                call.deadline,
                app,
            );
            server.set_generation(app.mcp_state_mut().mcp_generation);
            if let Some(result) = result {
                handled |= call.respond(result);
            }
        }
        handled
    }

    pub fn next_wakeup(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.retry_at)
    }

    pub fn shutdown(&mut self) {
        self.stop_listener();
        self.pending = None;
        self.last_listener = None;
        self.status = McpStatus::Off;
    }

    fn stop_listener(&mut self) {
        if let Some(mut server) = self.server.take() {
            server.shutdown();
        }
    }

    fn try_start(
        &mut self,
        pending: PendingBind,
        now: Instant,
        log: &mut crate::console::ConsoleLog,
    ) {
        let Some(wake) = &self.wake else {
            self.fail("MCP window wake handle is unavailable".to_owned(), log);
            return;
        };
        let config = McpConfig {
            port: pending.port,
            token: pending.token.clone(),
            permissions: pending.permissions,
            tools: tool_specs(),
        };
        match McpServer::start_with_generation(config, Arc::clone(wake), pending.generation) {
            Ok(server) => {
                self.server = Some(server);
                self.pending = None;
                self.status = McpStatus::Running(pending.port);
            }
            Err(error) if address_in_use(&error) && now < pending.expires_at => {
                self.pending = Some(PendingBind {
                    retry_at: now + REBIND_RETRY_INTERVAL,
                    ..pending
                });
                self.status = McpStatus::Starting;
            }
            Err(error) => self.fail(error.to_string(), log),
        }
    }

    fn fail(&mut self, error: String, log: &mut crate::console::ConsoleLog) {
        self.pending = None;
        self.status = McpStatus::Error(error.clone());
        log.push(LogLevel::Error, format!("[MCP] {error}"));
    }
}

fn address_in_use(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(48 | 98 | 10048))
        || error.kind() == std::io::ErrorKind::AddrInUse
        || error.to_string().contains("Address already in use")
        || error
            .to_string()
            .contains("Only one usage of each socket address")
}

struct PendingCall<'a> {
    name: &'a str,
    arguments: &'a Value,
    permission: Permission,
    generation: u64,
    deadline: Instant,
}

fn execute_pending(
    name: &str,
    arguments: &Value,
    permission: Permission,
    permissions: Permissions,
    generation: u64,
    deadline: Instant,
    app: &mut crate::app::EditorApp,
) -> Option<ToolResult> {
    if Instant::now() >= deadline {
        return None;
    }
    let call = PendingCall {
        name,
        arguments,
        permission,
        generation,
        deadline,
    };
    Some(execute_call(call, permissions, app))
}

fn execute_call(
    call: PendingCall<'_>,
    permissions: Permissions,
    app: &mut crate::app::EditorApp,
) -> ToolResult {
    let error = {
        let state = app.mcp_state_mut();
        if !permissions.allows(call.permission) {
            Some(ToolResult::error(format!(
                "permission '{}' is disabled",
                call.permission.flag()
            )))
        } else if state.mcp_generation != call.generation {
            Some(ToolResult::error(
                "project or scene changed while request was queued",
            ))
        } else if Instant::now() >= call.deadline {
            Some(ToolResult::error("tool call deadline expired"))
        } else {
            None
        }
    };
    if let Some(error) = error {
        return error;
    }
    if call.permission != Permission::Read && call.name != "build_status" {
        app.mcp_state_mut().log.info(format!("[MCP] {}", call.name));
    }
    match app.mcp_execute_tool(call.name, call.arguments, permissions) {
        Ok(value) => ToolResult::json(value),
        Err(error) => ToolResult::error(error),
    }
}

pub(crate) fn dispatch(
    app: &mut crate::app::EditorApp,
    tool: &str,
    arguments: &Value,
    permissions: Permissions,
) -> Result<Value, String> {
    match tool {
        "editor_status"
        | "list_assets"
        | "list_project_files"
        | "read_text_file"
        | "get_project_settings"
        | "get_scene_hierarchy"
        | "get_object"
        | "get_component"
        | "describe_components"
        | "get_asset_settings"
        | "get_script_params_schema"
        | "get_console_log" => read::call(tool, arguments, app.mcp_state_mut(), permissions),
        "select_objects" | "create_object" | "delete_objects" | "duplicate_objects"
        | "set_parent" | "rename_object" | "change_id" | "set_transform" | "set_component"
        | "remove_component" | "set_script_param" | "set_camera_active" | "undo" | "redo" => {
            edit::call(tool, arguments, app.mcp_state_mut())
        }
        "open_project" => assets::open_project(app, arguments, permissions),
        "save_scene"
        | "open_scene"
        | "new_scene"
        | "write_asset_settings"
        | "create_folder"
        | "write_text_file"
        | "rename_asset"
        | "delete_asset"
        | "import_files"
        | "create_prefab"
        | "apply_to_prefab" => assets::call(app, tool, arguments),
        "play_start" | "play_stop" | "play_pause" | "play_resume" | "console_command" => {
            play::call(app, tool, arguments)
        }
        "build_scripts" | "build_player" | "build_status" => build::call(app, tool, arguments),
        _ => Err(format!("unknown or unimplemented tool: {tool}")),
    }
}

fn tool_specs() -> Vec<ToolSpec> {
    let empty = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let mut tools = vec![
        ToolSpec::new(
            "editor_status",
            "Current editor, project, scene, selection, and permissions.",
            empty.clone(),
            Permission::Read,
        ),
        ToolSpec::new(
            "list_assets",
            "List files and folders below the project Assets directory.",
            json!({
                "type": "object",
                "properties": {
                    "folder": {"type": "string"},
                    "recursive": {"type": "boolean"},
                    "kind": {"type": "string", "enum": ["scene", "prefab", "camera_settings", "post_process", "physics_material", "model", "texture", "hdr", "audio", "script", "other"]}
                },
                "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "list_project_files",
            "List immediate project files without the hidden .rve directory.",
            json!({
                "type": "object", "properties": {"dir": {"type": "string"}}, "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "read_text_file",
            "Read a UTF-8 text file inside the project, up to 1 MiB.",
            json!({
                "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_project_settings",
            "Read project.rveproject and bootstrap.rves settings.",
            empty.clone(),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_scene_hierarchy",
            "Get the expanded scene hierarchy and component kinds.",
            json!({"type":"object","properties":{"source":{"type":"string","enum":["edit","play"]}},"additionalProperties":false}),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_object",
            "Get an expanded scene object and prefab origin.",
            json!({
                "type": "object", "properties": {"id": {"type": "string"}, "source":{"type":"string","enum":["edit","play"]}}, "required": ["id"], "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_component",
            "Get one component from an expanded scene object.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "kind": {"type": "string", "enum": ["transform", "camera", "mesh_renderer", "lod_group", "directional_light", "point_light", "spot_light", "environment", "collider", "rigid_body", "character_controller", "audio_source", "animator", "particle_emitter", "network_identity", "network_transform"]}
                },
                "required": ["id", "kind"], "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "describe_components",
            "Describe default fields for each scene component kind.",
            empty.clone(),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_asset_settings",
            "Read a camera, post-process, or physics-material settings asset.",
            json!({
                "type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_script_params_schema",
            "Get declared parameters for scripts attached to an object.",
            json!({
                "type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false
            }),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_console_log",
            "Read recent editor console entries.",
            json!({
                "type": "object",
                "properties": {
                    "since": {"type": "integer", "minimum": 0},
                    "level": {"type": "string", "enum": ["info", "warn", "error"]},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
                },
                "additionalProperties": false
            }),
            Permission::Read,
        ),
    ];
    tools.extend(edit::tool_specs());
    tools.extend(assets::tool_specs());
    tools.extend(play::tool_specs());
    tools.extend(build::tool_specs());
    tools.extend(capture::tool_specs());
    tools
}

#[cfg(test)]
mod tests {
    use super::{execute_pending, tool_specs};
    use crate::app::EditorApp;
    use crate::new_project::create_project;
    use crate::project::OpenProject;
    use crate::scene_edit::EditorScene;
    use crate::state::EditorState;
    use engine_files::VirtualPath;
    use engine_mcp::{Permission, Permissions};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    fn fixture_app() -> (PathBuf, EditorApp) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = std::env::temp_dir().join(format!(
            "rve-mcp-drain-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&parent).unwrap();
        let root = create_project("Fixture", &parent).unwrap();
        let project = OpenProject::open(&root).unwrap();
        let path = project.settings.starting_scene().unwrap();
        let source = project.read_text(path).unwrap();
        let scene = EditorScene::parse(
            VirtualPath::parse(path).unwrap(),
            &source,
            project.prefab_loader(),
        )
        .unwrap();
        let mut state = EditorState::new(None);
        state.project = Some(project);
        state.scene = Some(scene);
        (parent, EditorApp::new_for_mcp_tests_with_state(state))
    }

    fn clean(parent: PathBuf, app: EditorApp) {
        drop(app);
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn tools_list_registers_every_stage_four_tool_with_its_permission_and_schema() {
        let tools = tool_specs();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 47);
        assert!(names.contains(&"editor_status"));
        assert!(names.contains(&"get_console_log"));
        assert!(names.contains(&"capture_view"));
        for (name, permission) in [
            ("capture_view", Permission::Read),
            ("set_component", Permission::EditScene),
            ("undo", Permission::EditScene),
            ("write_text_file", Permission::Assets),
            ("open_project", Permission::Assets),
            ("play_start", Permission::Play),
            ("console_command", Permission::Play),
            ("build_player", Permission::Build),
            ("build_status", Permission::Build),
        ] {
            let tool = tools.iter().find(|tool| tool.name == name).unwrap();
            assert_eq!(tool.permission, permission);
            assert_eq!(tool.input_schema["type"], "object");
        }
        assert!(
            tools
                .iter()
                .all(|tool| tool.input_schema["type"] == "object")
        );
    }

    #[test]
    fn permission_revoked_after_enqueue_is_checked_before_dispatch() {
        let (parent, mut app) = fixture_app();
        let target = app
            .mcp_state_mut()
            .project
            .as_ref()
            .unwrap()
            .root
            .join("Assets/revoked.txt");
        let result = execute_pending(
            "write_text_file",
            &json!({"path": "revoked.txt", "content": "no"}),
            Permission::Assets,
            Permissions::default(),
            app.mcp_state_mut().mcp_generation,
            Instant::now() + Duration::from_secs(1),
            &mut app,
        )
        .unwrap();

        assert!(result.is_error);
        assert!(
            result.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("assets")
        );
        assert!(!target.exists());
        assert!(
            !app.mcp_state_mut()
                .log
                .entries()
                .any(|entry| entry.message.starts_with("[MCP]"))
        );
        clean(parent, app);
    }

    #[test]
    fn modifying_tool_is_logged_in_the_editor_console() {
        let (parent, mut app) = fixture_app();
        let result = execute_pending(
            "write_text_file",
            &json!({"path": "logged.txt", "content": "saved"}),
            Permission::Assets,
            Permissions {
                assets: true,
                ..Permissions::default()
            },
            app.mcp_state_mut().mcp_generation,
            Instant::now() + Duration::from_secs(1),
            &mut app,
        )
        .unwrap();

        assert!(!result.is_error);
        assert!(
            app.mcp_state_mut()
                .log
                .entries()
                .any(|entry| entry.message == "[MCP] write_text_file")
        );
        clean(parent, app);
    }

    #[test]
    fn play_source_without_an_active_session_returns_tool_errors() {
        let (parent, mut app) = fixture_app();
        let generation = app.mcp_state_mut().mcp_generation;
        for (tool, arguments) in [
            ("get_scene_hierarchy", json!({"source": "play"})),
            ("get_object", json!({"id": "camera", "source": "play"})),
        ] {
            let result = execute_pending(
                tool,
                &arguments,
                Permission::Read,
                Permissions::default(),
                generation,
                Instant::now() + Duration::from_secs(1),
                &mut app,
            )
            .unwrap();
            assert!(result.is_error);
            assert!(
                result.content[0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("without an active Play session")
            );
        }

        app.mcp_state_mut().scene = Some(EditorScene::new_untitled().unwrap());
        let play = crate::play::PlaySession::start(
            app.mcp_state_mut().scene.as_ref().unwrap().view(),
            None,
            |_| Ok(String::new()),
        )
        .unwrap();
        app.mcp_state_mut().play = Some(play);
        for (tool, arguments, expected) in [
            ("get_scene_hierarchy", json!({"source": "play"}), "objects"),
            (
                "get_object",
                json!({"id": "camera", "source": "play"}),
                "object",
            ),
        ] {
            let result = execute_pending(
                tool,
                &arguments,
                Permission::Read,
                Permissions::default(),
                app.mcp_state_mut().mcp_generation,
                Instant::now() + Duration::from_secs(1),
                &mut app,
            )
            .unwrap();
            assert!(!result.is_error);
            let value = result.structured_content.unwrap();
            assert_eq!(value["source"], "play");
            assert!(value[expected].is_array() || value[expected].is_object());
        }
        clean(parent, app);
    }

    #[test]
    fn expired_call_is_dropped_before_tool_execution() {
        let (parent, mut app) = fixture_app();
        let target = app
            .mcp_state_mut()
            .project
            .as_ref()
            .unwrap()
            .root
            .join("Assets/expired.txt");
        let result = execute_pending(
            "write_text_file",
            &json!({"path": "expired.txt", "content": "no"}),
            Permission::Assets,
            Permissions {
                assets: true,
                ..Permissions::default()
            },
            app.mcp_state_mut().mcp_generation,
            Instant::now(),
            &mut app,
        );

        assert!(result.is_none());
        assert!(!target.exists());
        assert!(
            !app.mcp_state_mut()
                .log
                .entries()
                .any(|entry| entry.message.starts_with("[MCP]"))
        );
        clean(parent, app);
    }

    #[test]
    fn scene_replacement_invalidates_queued_edit_without_touching_new_scene() {
        let (parent, mut app) = fixture_app();
        let generation = app.mcp_state_mut().mcp_generation;
        let scene = EditorScene::new_untitled().unwrap();
        app.mcp_state_mut().scene = Some(scene);
        app.mcp_state_mut().advance_mcp_generation();
        let before = app
            .mcp_state_mut()
            .scene
            .as_ref()
            .unwrap()
            .document()
            .to_toml()
            .unwrap();
        let result = execute_pending(
            "set_component",
            &json!({"id":"camera","kind":"transform","value":{"position":[9,9,9]}}),
            Permission::EditScene,
            Permissions {
                edit_scene: true,
                ..Permissions::default()
            },
            generation,
            Instant::now() + Duration::from_secs(1),
            &mut app,
        )
        .unwrap();

        assert!(result.is_error);
        assert!(
            result.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("changed")
        );
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
        clean(parent, app);
    }

    fn listener_settings(port: u16, token: &str) -> crate::settings::McpSettings {
        crate::settings::McpSettings {
            enabled: true,
            port,
            token: token.to_owned(),
            ..Default::default()
        }
    }

    /// Применяет настройки и возвращает число попыток запуска listener за этот вызов. Без окна нет
    /// wake-хэндла, поэтому попытка сразу пишет ошибку и не открывает сокет. Лог очищается перед
    /// вызовом: `ConsoleLog` не дублирует подряд идущие одинаковые записи.
    fn apply_and_count_starts(
        host: &mut super::McpHost,
        settings: &crate::settings::McpSettings,
    ) -> usize {
        let mut log = crate::console::ConsoleLog::default();
        host.apply_settings(settings, 0, &mut log);
        log.entries()
            .filter(|entry| entry.message.contains("wake handle"))
            .count()
    }

    #[test]
    fn apply_settings_restarts_the_listener_only_for_listener_fields() {
        let mut host = super::McpHost::default();
        let mut settings = listener_settings(47615, "token");
        assert_eq!(apply_and_count_starts(&mut host, &settings), 1);

        settings.permissions.edit_scene = true;
        assert_eq!(
            apply_and_count_starts(&mut host, &settings),
            0,
            "a permission change must not restart the listener"
        );
        assert!(host.permissions.edit_scene);

        settings.port = 47616;
        assert_eq!(apply_and_count_starts(&mut host, &settings), 1);
        settings.token = "other".to_owned();
        assert_eq!(apply_and_count_starts(&mut host, &settings), 1);

        settings.enabled = false;
        assert_eq!(apply_and_count_starts(&mut host, &settings), 0);
        assert_eq!(host.status(), &super::McpStatus::Off);
    }
}
