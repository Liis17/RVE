pub mod paths;
mod read;

use crate::console::LogLevel;
use crate::settings::McpSettings;
use crate::state::EditorState;
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

    pub fn drain(&mut self, state: &mut EditorState) -> bool {
        let Some(server) = &self.server else {
            return false;
        };
        server.set_generation(state.mcp_generation);
        let mut handled = false;
        while let Some(call) = server.try_recv() {
            if call.is_abandoned() {
                continue;
            }
            let result = execute_pending(
                &call.name,
                &call.arguments,
                call.required_permission,
                server.permissions(),
                call.generation(),
                call.deadline,
                state,
            );
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
    state: &mut EditorState,
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
    Some(execute_call(call, permissions, state))
}

fn execute_call(
    call: PendingCall<'_>,
    permissions: Permissions,
    state: &mut EditorState,
) -> ToolResult {
    if !permissions.allows(call.permission) {
        return ToolResult::error(format!(
            "permission '{}' is disabled",
            call.permission.flag()
        ));
    }
    if state.mcp_generation != call.generation {
        return ToolResult::error("project or scene changed while request was queued");
    }
    if Instant::now() >= call.deadline {
        return ToolResult::error("tool call deadline expired");
    }
    match read::call(call.name, call.arguments, state, permissions) {
        Ok(value) => ToolResult::json(value),
        Err(error) => ToolResult::error(error),
    }
}

fn tool_specs() -> Vec<ToolSpec> {
    let empty = json!({"type": "object", "properties": {}, "additionalProperties": false});
    vec![
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
            empty.clone(),
            Permission::Read,
        ),
        ToolSpec::new(
            "get_object",
            "Get an expanded scene object and prefab origin.",
            json!({
                "type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false
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
    ]
}

#[cfg(test)]
mod tests {
    use super::{execute_pending, tool_specs};
    use crate::state::EditorState;
    use engine_mcp::{Permission, Permissions};
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn tools_list_contains_every_read_tool_except_capture_view() {
        let tools = tool_specs();
        let names = tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 12);
        assert!(names.contains(&"editor_status"));
        assert!(names.contains(&"get_console_log"));
        assert!(!names.contains(&"capture_view"));
        assert!(tools.iter().all(|tool| {
            tool.permission == Permission::Read && tool.input_schema["type"] == "object"
        }));
    }

    #[test]
    fn permission_revoked_after_enqueue_is_checked_before_dispatch() {
        let mut state = EditorState::new(None);
        let result = execute_pending(
            "write_text_file",
            &json!({"path": "new.txt", "content": "no"}),
            Permission::Assets,
            Permissions::default(),
            state.mcp_generation,
            Instant::now() + Duration::from_secs(1),
            &mut state,
        )
        .unwrap();

        assert!(result.is_error);
        assert!(
            result.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("assets")
        );
        assert!(state.project.is_none());
    }

    #[test]
    fn expired_call_is_dropped_before_tool_execution() {
        let mut state = EditorState::new(None);
        let result = execute_pending(
            "editor_status",
            &json!({}),
            Permission::Read,
            Permissions::default(),
            state.mcp_generation,
            Instant::now(),
            &mut state,
        );

        assert!(result.is_none());
        assert!(state.project.is_none());
    }

    #[test]
    fn calls_from_an_earlier_project_generation_fail_without_execution() {
        let mut state = EditorState::new(None);
        state.advance_mcp_generation();
        let result = execute_pending(
            "editor_status",
            &json!({}),
            Permission::Read,
            Permissions::default(),
            state.mcp_generation - 1,
            Instant::now() + Duration::from_secs(1),
            &mut state,
        )
        .unwrap();

        assert!(result.is_error);
        assert!(
            result.content[0]["text"]
                .as_str()
                .unwrap()
                .contains("changed")
        );
    }
}
