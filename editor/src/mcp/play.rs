use crate::app::EditorApp;
use engine_mcp::{Permission, ToolSpec};
use serde_json::{Value, json};

pub(super) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "play_start",
            "Start Play. Optional network=true selects the project's Host mode; false selects Offline.",
            json!({"type":"object","properties":{"network":{"type":"boolean"}},"additionalProperties":false}),
            Permission::Play,
        ),
        ToolSpec::new(
            "play_stop",
            "Stop the active Play session.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            Permission::Play,
        ),
        ToolSpec::new(
            "play_pause",
            "Pause the active Play session.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            Permission::Play,
        ),
        ToolSpec::new(
            "play_resume",
            "Resume the active Play session.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            Permission::Play,
        ),
        ToolSpec::new(
            "console_command",
            "Submit a command to the editor console during Play.",
            json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}),
            Permission::Play,
        ),
    ]
}

pub(super) fn call(app: &mut EditorApp, tool: &str, arguments: &Value) -> Result<Value, String> {
    match tool {
        "play_start" => {
            let network = optional_bool(arguments, "network")?;
            let status = app.mcp_start_play(network)?;
            Ok(json!({"status": status}))
        }
        "play_stop" => {
            app.mcp_stop_play();
            Ok(json!({"status": "stopped"}))
        }
        "play_pause" | "play_resume" => {
            let paused = tool == "play_pause";
            let play = app
                .mcp_state_mut()
                .play
                .as_mut()
                .ok_or_else(|| "no active Play session".to_owned())?;
            play.set_paused(paused);
            Ok(json!({"paused": paused}))
        }
        "console_command" => {
            let command = required_string(arguments, "command")?;
            if !app.mcp_state_mut().is_playing() {
                return Err("console commands are available only during Play".to_owned());
            }
            app.mcp_console_command(command);
            Ok(json!({"submitted": true}))
        }
        _ => Err(format!("unknown Play tool: {tool}")),
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} must be a string"))
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
