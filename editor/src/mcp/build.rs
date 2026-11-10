use crate::app::EditorApp;
use engine_mcp::{Permission, ToolSpec};
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "build_scripts",
            "Start the configured heavy-scripts build in the background and return immediately.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            Permission::Build,
        ),
        ToolSpec::new(
            "build_player",
            "Start a player build in the requested output directory and return immediately. Set with_server to also ship the dedicated rve-server executable.",
            json!({"type":"object","properties":{"output_dir":{"type":"string"},"with_server":{"type":"boolean"}},"required":["output_dir"],"additionalProperties":false}),
            Permission::Build,
        ),
        ToolSpec::new(
            "build_status",
            "Return the current or last scripts/player build state and the last 100 output lines.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            Permission::Build,
        ),
    ]
}

pub(super) fn call(app: &mut EditorApp, tool: &str, arguments: &Value) -> Result<Value, String> {
    match tool {
        "build_scripts" => {
            let status = app.mcp_start_scripts_build()?;
            Ok(json!({"status": status}))
        }
        "build_player" => {
            let output = arguments
                .get("output_dir")
                .and_then(Value::as_str)
                .ok_or_else(|| "output_dir must be a path string".to_owned())?;
            let with_server = match arguments.get("with_server") {
                Some(value) => value
                    .as_bool()
                    .ok_or_else(|| "with_server must be a boolean".to_owned())?,
                None => false,
            };
            app.mcp_start_build(Path::new(output), with_server)?;
            Ok(json!({"status": "running", "output_dir": output}))
        }
        "build_status" => {
            let state = app.mcp_state_mut();
            let status = if state.build.is_some() || state.scripts_job.is_some() {
                "running"
            } else {
                match state.build_succeeded {
                    Some(true) => "succeeded",
                    Some(false) => "failed",
                    None => "idle",
                }
            };
            Ok(json!({
                "status": status,
                "output_tail": state.build_output_tail,
            }))
        }
        _ => Err(format!("unknown Build tool: {tool}")),
    }
}

#[cfg(test)]
mod tests {
    use super::call;
    use crate::app::EditorApp;
    use crate::state::EditorState;
    use serde_json::json;

    #[test]
    fn build_player_rejects_a_non_boolean_with_server() {
        let mut app = EditorApp::new_for_mcp_tests_with_state(EditorState::new(None));

        let error = call(
            &mut app,
            "build_player",
            &json!({"output_dir": "out", "with_server": "yes"}),
        )
        .unwrap_err();
        assert_eq!(error, "with_server must be a boolean");
    }

    #[test]
    fn build_status_returns_last_result_and_output_tail() {
        let mut state = EditorState::new(None);
        state.build_output_tail = vec!["one".to_owned(), "two".to_owned()];
        state.build_succeeded = Some(false);
        let mut app = EditorApp::new_for_mcp_tests_with_state(state);

        let result = call(&mut app, "build_status", &json!({})).unwrap();
        assert_eq!(result["status"], "failed");
        assert_eq!(result["output_tail"], json!(["one", "two"]));
    }
}
