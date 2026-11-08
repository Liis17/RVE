use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

/// Permission required to invoke a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Read,
    EditScene,
    Assets,
    Play,
    Build,
}

impl Permission {
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::EditScene => "edit_scene",
            Self::Assets => "assets",
            Self::Play => "play",
            Self::Build => "build",
        }
    }
}

/// Runtime permissions exposed to the host for a second check before tool execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Permissions {
    pub edit_scene: bool,
    pub assets: bool,
    pub play: bool,
    pub build: bool,
}

impl Permissions {
    pub const fn allows(self, permission: Permission) -> bool {
        match permission {
            Permission::Read => true,
            Permission::EditScene => self.edit_scene,
            Permission::Assets => self.assets,
            Permission::Play => self.play,
            Permission::Build => self.build,
        }
    }
}

/// MCP tool metadata supplied by the editor host.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub permission: Permission,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        permission: Permission,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            permission,
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        })
    }
}

/// Result content returned by the host for a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": text.into()})],
            structured_content: None,
            is_error: false,
        }
    }

    pub fn json(value: Value) -> Self {
        let text = serde_json::to_string(&value).expect("serializing a JSON value cannot fail");
        Self {
            content: vec![json!({"type": "text", "text": text})],
            structured_content: Some(value),
            is_error: false,
        }
    }

    pub fn image_png(bytes: &[u8]) -> Self {
        Self {
            content: vec![json!({
                "type": "image",
                "data": STANDARD.encode(bytes),
                "mimeType": "image/png",
            })],
            structured_content: None,
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        let mut result = Self::text(text);
        result.is_error = true;
        result
    }

    pub(crate) fn to_value(&self) -> Value {
        let mut value = json!({ "content": self.content });
        if let Some(structured) = &self.structured_content {
            value["structuredContent"] = structured.clone();
        }
        if self.is_error {
            value["isError"] = Value::Bool(true);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{Permission, Permissions, ToolResult};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    #[test]
    fn read_permission_is_always_enabled_and_flags_are_independent() {
        let permissions = Permissions::default();
        assert!(permissions.allows(Permission::Read));
        assert!(!permissions.allows(Permission::EditScene));
        assert!(!permissions.allows(Permission::Assets));

        let permissions = Permissions {
            edit_scene: true,
            ..Permissions::default()
        };
        assert!(permissions.allows(Permission::EditScene));
        assert!(!permissions.allows(Permission::Assets));
    }

    #[test]
    fn tool_results_keep_json_and_encode_png_as_standard_base64() {
        let json_result = ToolResult::json(json!({"ok": true})).to_value();
        assert_eq!(json_result["structuredContent"], json!({"ok": true}));

        let image = ToolResult::image_png(b"png").to_value();
        assert_eq!(image["content"][0]["mimeType"], "image/png");
        assert_eq!(image["content"][0]["data"], STANDARD.encode(b"png"));
    }
}
