use serde_json::{Value, json};

/// Implemented handshake-based MCP protocol versions, newest first.
///
/// The 2026-07-28 revision is a separate stateless transport revision and is not
/// advertised by this session-based server.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25"];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RpcRequest {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: &'static str,
}

pub(crate) fn parse_request(body: &[u8]) -> Result<RpcRequest, RpcError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| RpcError {
        code: -32700,
        message: "Parse error",
    })?;
    let Value::Object(object) = value else {
        return Err(RpcError {
            code: -32600,
            message: "Invalid Request",
        });
    };
    if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
        return Err(RpcError {
            code: -32600,
            message: "Invalid Request",
        });
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(RpcError {
            code: -32600,
            message: "Invalid Request",
        });
    };
    let id = object.get("id").cloned();
    if id
        .as_ref()
        .is_some_and(|id| !id.is_string() && !id.is_number() && !id.is_null())
    {
        return Err(RpcError {
            code: -32600,
            message: "Invalid Request",
        });
    }
    Ok(RpcRequest {
        id,
        method: method.to_owned(),
        params: object.get("params").cloned().unwrap_or(Value::Null),
    })
}

pub(crate) fn success(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

pub(crate) fn error(id: Option<Value>, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": error })
}

pub(crate) fn initialize_result(
    client_version: Option<&str>,
    server_version: &str,
) -> (String, Value) {
    let negotiated = client_version
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
        .to_owned();
    let result = json!({
        "protocolVersion": negotiated,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "rve-editor", "version": server_version },
    });
    (negotiated, result)
}

pub(crate) fn initialize_client_version(params: &Value) -> Option<&str> {
    let params = params.as_object()?;
    let protocol_version = params.get("protocolVersion")?.as_str()?;
    params.get("capabilities")?.as_object()?;
    let client_info = params.get("clientInfo")?.as_object()?;
    client_info.get("name")?.as_str()?;
    client_info.get("version")?.as_str()?;
    Some(protocol_version)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionPhase {
    Initializing,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub protocol_version: String,
    pub phase: SessionPhase,
}

impl Session {
    pub fn new(protocol_version: String) -> Self {
        Self {
            protocol_version,
            phase: SessionPhase::Initializing,
        }
    }

    pub fn accept(&mut self, method: &str) -> Result<bool, &'static str> {
        if method == "notifications/initialized" {
            if self.phase != SessionPhase::Initializing {
                return Err("Session is already initialized");
            }
            self.phase = SessionPhase::Ready;
            return Ok(false);
        }
        if self.phase != SessionPhase::Ready {
            return Err("Session is not initialized");
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SUPPORTED_PROTOCOL_VERSIONS, Session, SessionPhase, initialize_client_version,
        initialize_result, parse_request,
    };
    use serde_json::json;

    #[test]
    fn protocol_version_negotiates_only_the_advertised_session_protocol() {
        assert_eq!(SUPPORTED_PROTOCOL_VERSIONS, &["2025-11-25"]);
        let (version, result) = initialize_result(Some("2025-11-25"), "0.1.0");
        assert_eq!(version, "2025-11-25");
        assert_eq!(result["protocolVersion"], "2025-11-25");
        let (fallback, _) = initialize_result(Some("2026-07-28"), "0.1.0");
        assert_eq!(fallback, "2025-11-25");
    }

    #[test]
    fn initialize_requires_protocol_capabilities_and_client_identity() {
        let params = json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}
        });
        assert_eq!(initialize_client_version(&params), Some("2025-11-25"));
        assert_eq!(initialize_client_version(&json!({})), None);
    }

    #[test]
    fn session_requires_initialized_notification_before_requests() {
        let mut session = Session::new("2025-11-25".to_owned());
        assert_eq!(session.phase, SessionPhase::Initializing);
        assert_eq!(
            session.accept("tools/list"),
            Err("Session is not initialized")
        );
        assert_eq!(session.accept("notifications/initialized"), Ok(false));
        assert_eq!(session.phase, SessionPhase::Ready);
        assert_eq!(session.accept("tools/list"), Ok(true));
        assert!(session.accept("notifications/initialized").is_err());
    }

    #[test]
    fn json_rpc_batches_are_rejected() {
        let error = parse_request(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#)
            .expect_err("batch must be rejected");
        assert_eq!(error.code, -32600);
    }
}
