use crate::{
    Permission, ToolResult,
    protocol::{
        SUPPORTED_PROTOCOL_VERSIONS, Session, error, initialize_client_version, initialize_result,
        parse_request, success,
    },
    server::{Shared, TOOL_CALL_TIMEOUT, ToolCall, WaitResult, wait_for_result},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, Read},
    sync::Arc,
    time::{Duration, Instant},
};
use tiny_http::{Header, Request, Response, Server, StatusCode};

pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Headers<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub authorization: Option<&'a str>,
    pub host: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub accept: Option<&'a str>,
    pub protocol_version: Option<&'a str>,
    pub session_id: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HttpFailure {
    Unauthorized,
    Forbidden,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    PayloadTooLarge,
    ServiceUnavailable,
    GatewayTimeout,
    InternalServerError,
}

impl HttpFailure {
    const fn status(self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::PayloadTooLarge => 413,
            Self::ServiceUnavailable => 503,
            Self::GatewayTimeout => 504,
            Self::InternalServerError => 500,
        }
    }
}

pub(crate) fn validate_headers(
    headers: Headers<'_>,
    port: u16,
    token: &str,
    sessions: &HashMap<String, Session>,
    rpc_method: Option<&str>,
) -> Result<(), HttpFailure> {
    if !valid_bearer(headers.authorization, token) {
        return Err(HttpFailure::Unauthorized);
    }

    let expected_host = format!("127.0.0.1:{port}");
    let expected_localhost = format!("localhost:{port}");
    if !headers.host.is_some_and(|host| {
        host.eq_ignore_ascii_case(&expected_host) || host.eq_ignore_ascii_case(&expected_localhost)
    }) {
        return Err(HttpFailure::Forbidden);
    }
    if headers.origin.is_some_and(|origin| {
        let expected_origin = format!("http://127.0.0.1:{port}");
        let expected_local_origin = format!("http://localhost:{port}");
        !origin.eq_ignore_ascii_case(&expected_origin)
            && !origin.eq_ignore_ascii_case(&expected_local_origin)
    }) {
        return Err(HttpFailure::Forbidden);
    }
    if headers.path != "/mcp" {
        return Err(HttpFailure::NotFound);
    }
    match headers.method {
        "GET" => return Err(HttpFailure::MethodNotAllowed),
        "POST" => {
            if !is_json_content_type(headers.content_type)
                || !accepts_mcp_streamable_http(headers.accept)
            {
                return Err(HttpFailure::BadRequest);
            }
        }
        "DELETE" => {}
        _ => return Err(HttpFailure::MethodNotAllowed),
    }

    let initializing = headers.method == "POST" && rpc_method == Some("initialize");
    if initializing {
        if headers.session_id.is_some()
            || headers
                .protocol_version
                .is_some_and(|version| !SUPPORTED_PROTOCOL_VERSIONS.contains(&version))
        {
            return Err(HttpFailure::BadRequest);
        }
        return Ok(());
    }

    let Some(session_id) = headers.session_id else {
        return Err(HttpFailure::BadRequest);
    };
    let Some(session) = sessions.get(session_id) else {
        return Err(HttpFailure::NotFound);
    };
    let Some(protocol_version) = headers.protocol_version else {
        return Err(HttpFailure::BadRequest);
    };
    if protocol_version != session.protocol_version {
        return Err(HttpFailure::BadRequest);
    }
    Ok(())
}

pub(crate) fn is_json_content_type(value: Option<&str>) -> bool {
    value
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

pub(crate) fn accepts_mcp_streamable_http(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let mut json = false;
    let mut event_stream = false;
    for item in value.split(',') {
        let media_type = item.split(';').next().unwrap_or_default().trim();
        json |= media_type.eq_ignore_ascii_case("application/json");
        event_stream |= media_type.eq_ignore_ascii_case("text/event-stream");
    }
    json && event_stream
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let length = left.len().max(right.len());
    let mut difference = left.len() ^ right.len();
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn valid_bearer(value: Option<&str>, token: &str) -> bool {
    value
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, value)| {
            scheme.eq_ignore_ascii_case("Bearer")
                && constant_time_eq(value.as_bytes(), token.as_bytes())
        })
}

#[derive(Clone, Debug)]
struct OwnedHeaders {
    method: String,
    path: String,
    authorization: Option<String>,
    host: Option<String>,
    origin: Option<String>,
    content_type: Option<String>,
    accept: Option<String>,
    protocol_version: Option<String>,
    session_id: Option<String>,
}

impl OwnedHeaders {
    fn view(&self) -> Headers<'_> {
        Headers {
            method: &self.method,
            path: &self.path,
            authorization: self.authorization.as_deref(),
            host: self.host.as_deref(),
            origin: self.origin.as_deref(),
            content_type: self.content_type.as_deref(),
            accept: self.accept.as_deref(),
            protocol_version: self.protocol_version.as_deref(),
            session_id: self.session_id.as_deref(),
        }
    }
}

pub(crate) fn worker(listener: Arc<Server>, shared: Arc<Shared>, shutdown: flume::Receiver<()>) {
    loop {
        if shutdown.is_disconnected() {
            break;
        }
        match listener.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(request)) if shutdown.is_disconnected() => {
                respond_failure(request, HttpFailure::ServiceUnavailable)
            }
            Ok(Some(request)) => handle_request(request, &shared, &shutdown),
            Ok(None) if shutdown.is_disconnected() => break,
            Ok(None) => {}
            Err(_) if shutdown.is_disconnected() => break,
            Err(_) => {}
        }
    }
}

fn handle_request(request: Request, shared: &Shared, shutdown: &flume::Receiver<()>) {
    let owned_headers = request_headers(&request);
    let headers = owned_headers.view();
    if let Err(failure) = validate_authority(headers, shared.port, &shared.token) {
        respond_failure(request, failure);
        return;
    }

    match owned_headers.method.as_str() {
        "GET" => respond_failure(request, HttpFailure::MethodNotAllowed),
        "POST" => handle_post(request, headers, shared, shutdown),
        "DELETE" => handle_delete(request, headers, shared),
        _ => respond_failure(request, HttpFailure::MethodNotAllowed),
    }
}

fn validate_authority(headers: Headers<'_>, port: u16, token: &str) -> Result<(), HttpFailure> {
    if !valid_bearer(headers.authorization, token) {
        return Err(HttpFailure::Unauthorized);
    }
    let expected_host = format!("127.0.0.1:{port}");
    let expected_localhost = format!("localhost:{port}");
    if !headers.host.is_some_and(|host| {
        host.eq_ignore_ascii_case(&expected_host) || host.eq_ignore_ascii_case(&expected_localhost)
    }) {
        return Err(HttpFailure::Forbidden);
    }
    if headers.origin.is_some_and(|origin| {
        let expected_origin = format!("http://127.0.0.1:{port}");
        let expected_local_origin = format!("http://localhost:{port}");
        !origin.eq_ignore_ascii_case(&expected_origin)
            && !origin.eq_ignore_ascii_case(&expected_local_origin)
    }) {
        return Err(HttpFailure::Forbidden);
    }
    if headers.path != "/mcp" {
        return Err(HttpFailure::NotFound);
    }
    match headers.method {
        "GET" => return Err(HttpFailure::MethodNotAllowed),
        "POST" => {
            if !is_json_content_type(headers.content_type)
                || !accepts_mcp_streamable_http(headers.accept)
            {
                return Err(HttpFailure::BadRequest);
            }
        }
        "DELETE" => {}
        _ => return Err(HttpFailure::MethodNotAllowed),
    }
    Ok(())
}

fn handle_post(
    mut request: Request,
    headers: Headers<'_>,
    shared: &Shared,
    shutdown: &flume::Receiver<()>,
) {
    let body = match read_limited_body(request.as_reader()) {
        Ok(Some(body)) => body,
        Ok(None) => {
            respond_failure(request, HttpFailure::PayloadTooLarge);
            return;
        }
        Err(_) => {
            respond_failure(request, HttpFailure::BadRequest);
            return;
        }
    };
    let rpc = match parse_request(&body) {
        Ok(request) => request,
        Err(rpc_error) => {
            respond_json(
                request,
                400,
                error(None, rpc_error.code, rpc_error.message, None),
                &[],
            );
            return;
        }
    };

    let mut sessions = shared.sessions.lock().unwrap();
    if let Err(failure) = validate_headers(
        headers,
        shared.port,
        &shared.token,
        &sessions,
        Some(&rpc.method),
    ) {
        drop(sessions);
        respond_failure(request, failure);
        return;
    }

    if rpc.method == "initialize" {
        let Some(id) = rpc.id.clone() else {
            drop(sessions);
            respond_json(
                request,
                400,
                error(None, -32600, "Invalid Request", None),
                &[],
            );
            return;
        };
        let Some(client_version) = initialize_client_version(&rpc.params) else {
            drop(sessions);
            respond_json(
                request,
                400,
                error(Some(id), -32602, "Invalid params", None),
                &[],
            );
            return;
        };
        let (protocol_version, result) = initialize_result(Some(client_version), crate::VERSION);
        let session_id = match new_session_id() {
            Ok(id) => id,
            Err(_) => {
                drop(sessions);
                respond_failure(request, HttpFailure::InternalServerError);
                return;
            }
        };
        sessions.insert(session_id.clone(), Session::new(protocol_version));
        drop(sessions);
        let session_header = Header::from_bytes("Mcp-Session-Id", session_id)
            .expect("MCP session header name and value are valid");
        respond_json(request, 200, success(Some(id), result), &[session_header]);
        return;
    }

    let Some(session_id) = headers.session_id else {
        drop(sessions);
        respond_failure(request, HttpFailure::BadRequest);
        return;
    };
    let Some(session) = sessions.get_mut(session_id) else {
        drop(sessions);
        respond_failure(request, HttpFailure::NotFound);
        return;
    };
    if rpc.method == "notifications/initialized" && rpc.id.is_some() {
        let id = rpc.id.clone();
        drop(sessions);
        respond_json(
            request,
            400,
            error(id, -32600, "Invalid Request", None),
            &[],
        );
        return;
    }
    match session.accept(&rpc.method) {
        Ok(false) => {
            drop(sessions);
            respond_empty(request, 202);
            return;
        }
        Ok(true) => {}
        Err(message) => {
            let id = rpc.id.clone();
            drop(sessions);
            respond_json(request, 200, error(id, -32002, message, None), &[]);
            return;
        }
    }
    drop(sessions);

    if rpc.id.is_none() {
        respond_empty(request, 202);
        return;
    }
    match rpc.method.as_str() {
        "ping" => respond_json(request, 200, success(rpc.id, json!({})), &[]),
        "tools/list" => {
            let permissions = *shared.permissions.read().unwrap();
            let tools = shared
                .tools
                .iter()
                .filter(|tool| permissions.allows(tool.permission))
                .map(crate::tools::ToolSpec::to_value)
                .collect::<Vec<_>>();
            respond_json(
                request,
                200,
                success(rpc.id, json!({ "tools": tools })),
                &[],
            );
        }
        "tools/call" => handle_tool_call(request, rpc.id, rpc.params, shared, shutdown),
        _ => respond_json(
            request,
            200,
            error(rpc.id, -32601, "Method not found", None),
            &[],
        ),
    }
}

fn handle_tool_call(
    request: Request,
    id: Option<Value>,
    params: Value,
    shared: &Shared,
    shutdown: &flume::Receiver<()>,
) {
    let Some(id) = id else {
        respond_failure(request, HttpFailure::BadRequest);
        return;
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        respond_json(
            request,
            200,
            error(Some(id), -32602, "Invalid params", None),
            &[],
        );
        return;
    };
    let Some(tool) = shared.tools.iter().find(|tool| tool.name == name) else {
        respond_json(
            request,
            200,
            error(Some(id), -32602, "Unknown tool", None),
            &[],
        );
        return;
    };
    let permission = tool.permission;
    let permissions = *shared.permissions.read().unwrap();
    if !permissions.allows(permission) {
        let result = ToolResult::error(format!("Permission `{}` is disabled", permission.flag()));
        respond_json(request, 200, success(Some(id), result.to_value()), &[]);
        return;
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let deadline = Instant::now() + TOOL_CALL_TIMEOUT;
    let (call, response) = pending_tool_call(name, arguments, permission, deadline);
    if shared.calls.send(call).is_err() {
        respond_failure(request, HttpFailure::ServiceUnavailable);
        return;
    }
    (shared.wake)();
    let result = wait_for_result(&response, shutdown, deadline);
    match result {
        WaitResult::Response(result) => {
            respond_json(request, 200, success(Some(id), result.to_value()), &[])
        }
        result => respond_failure(
            request,
            wait_failure(result).expect("non-response wait outcomes map to an HTTP failure"),
        ),
    }
}

fn pending_tool_call(
    name: &str,
    arguments: Value,
    permission: Permission,
    deadline: Instant,
) -> (ToolCall, flume::Receiver<ToolResult>) {
    let (responder, response) = flume::bounded(1);
    (
        ToolCall {
            name: name.to_owned(),
            arguments,
            deadline,
            required_permission: permission,
            responder,
        },
        response,
    )
}

fn wait_failure(result: WaitResult) -> Option<HttpFailure> {
    match result {
        WaitResult::Response(_) => None,
        WaitResult::Shutdown => Some(HttpFailure::ServiceUnavailable),
        WaitResult::Deadline => Some(HttpFailure::GatewayTimeout),
        WaitResult::Disconnected => Some(HttpFailure::InternalServerError),
    }
}

fn handle_delete(request: Request, headers: Headers<'_>, shared: &Shared) {
    let sessions = shared.sessions.lock().unwrap();
    if let Err(failure) = validate_headers(headers, shared.port, &shared.token, &sessions, None) {
        drop(sessions);
        respond_failure(request, failure);
        return;
    }
    drop(sessions);
    if let Some(session_id) = headers.session_id {
        shared.sessions.lock().unwrap().remove(session_id);
    }
    respond_empty(request, 200);
}

fn request_headers(request: &Request) -> OwnedHeaders {
    let find = |name: &str| {
        request
            .headers()
            .iter()
            .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str().to_owned())
    };
    OwnedHeaders {
        method: request.method().as_str().to_owned(),
        path: request.url().to_owned(),
        authorization: find("Authorization"),
        host: find("Host"),
        origin: find("Origin"),
        content_type: find("Content-Type"),
        accept: find("Accept"),
        protocol_version: find("MCP-Protocol-Version"),
        session_id: find("Mcp-Session-Id"),
    }
}

fn read_limited_body(reader: &mut dyn Read) -> io::Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    (&mut *reader)
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_BODY_BYTES {
        Ok(None)
    } else {
        Ok(Some(body))
    }
}

fn new_session_id() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn respond_failure(request: Request, failure: HttpFailure) {
    let body = match failure {
        HttpFailure::Unauthorized => "Unauthorized",
        HttpFailure::Forbidden => "Forbidden",
        HttpFailure::BadRequest => "Bad Request",
        HttpFailure::NotFound => "Not Found",
        HttpFailure::MethodNotAllowed => "Method Not Allowed",
        HttpFailure::PayloadTooLarge => "Payload Too Large",
        HttpFailure::ServiceUnavailable => "Service Unavailable",
        HttpFailure::GatewayTimeout => "Gateway Timeout",
        HttpFailure::InternalServerError => "Internal Server Error",
    };
    respond_raw(
        request,
        failure.status(),
        body.as_bytes(),
        "text/plain",
        &[],
    );
}

fn respond_empty(request: Request, status: u16) {
    respond_raw(request, status, &[], "application/json", &[]);
}

fn respond_json(request: Request, status: u16, body: Value, extra_headers: &[Header]) {
    let body = serde_json::to_vec(&body).expect("serializing a JSON value cannot fail");
    respond_raw(request, status, &body, "application/json", extra_headers);
}

fn respond_raw(
    request: Request,
    status: u16,
    body: &[u8],
    content_type: &str,
    extra_headers: &[Header],
) {
    let content_type = Header::from_bytes("Content-Type", content_type)
        .expect("static content type is a valid HTTP header");
    let mut response = Response::from_data(body.to_vec())
        .with_status_code(StatusCode(status))
        .with_header(content_type);
    for header in extra_headers {
        response = response.with_header(header.clone());
    }
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::{
        Headers, HttpFailure, MAX_BODY_BYTES, accepts_mcp_streamable_http, is_json_content_type,
        pending_tool_call, read_limited_body, validate_headers, wait_failure,
    };
    use crate::protocol::{Session, SessionPhase};
    use crate::{Permission, server::WaitResult};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn headers() -> Headers<'static> {
        Headers {
            method: "POST",
            path: "/mcp",
            authorization: Some("Bearer secret"),
            host: Some("127.0.0.1:47615"),
            origin: None,
            content_type: Some("application/json"),
            accept: Some("application/json, text/event-stream"),
            protocol_version: None,
            session_id: None,
        }
    }

    #[test]
    fn request_header_checks_cover_auth_host_origin_protocol_and_session() {
        let mut sessions = HashMap::new();
        sessions.insert(
            "known".to_owned(),
            Session {
                protocol_version: "2025-11-25".to_owned(),
                phase: SessionPhase::Initializing,
            },
        );
        let cases = [
            ("initialize", headers(), None),
            (
                "missing auth",
                Headers {
                    authorization: None,
                    ..headers()
                },
                Some(HttpFailure::Unauthorized),
            ),
            (
                "wrong auth",
                Headers {
                    authorization: Some("Bearer wrong"),
                    ..headers()
                },
                Some(HttpFailure::Unauthorized),
            ),
            (
                "wrong host",
                Headers {
                    host: Some("127.0.0.2:47615"),
                    ..headers()
                },
                Some(HttpFailure::Forbidden),
            ),
            (
                "wrong origin",
                Headers {
                    origin: Some("http://evil.example"),
                    ..headers()
                },
                Some(HttpFailure::Forbidden),
            ),
            (
                "unsupported init version",
                Headers {
                    protocol_version: Some("2026-07-28"),
                    ..headers()
                },
                Some(HttpFailure::BadRequest),
            ),
        ];
        for (label, request, expected) in cases {
            let result = validate_headers(request, 47615, "secret", &sessions, Some("initialize"));
            assert_eq!(result.err(), expected, "{label}");
        }
        assert_eq!(
            validate_headers(
                Headers {
                    host: Some("localhost:47615"),
                    origin: Some("http://localhost:47615"),
                    ..headers()
                },
                47615,
                "secret",
                &sessions,
                Some("initialize")
            ),
            Ok(())
        );

        let cases = [
            (
                "missing session",
                Headers {
                    protocol_version: Some("2025-11-25"),
                    ..headers()
                },
                Some(HttpFailure::BadRequest),
            ),
            (
                "unknown session",
                Headers {
                    session_id: Some("unknown"),
                    protocol_version: Some("2025-11-25"),
                    ..headers()
                },
                Some(HttpFailure::NotFound),
            ),
            (
                "missing protocol header",
                Headers {
                    session_id: Some("known"),
                    ..headers()
                },
                Some(HttpFailure::BadRequest),
            ),
            (
                "wrong protocol header",
                Headers {
                    session_id: Some("known"),
                    protocol_version: Some("2026-07-28"),
                    ..headers()
                },
                Some(HttpFailure::BadRequest),
            ),
            (
                "valid session",
                Headers {
                    session_id: Some("known"),
                    protocol_version: Some("2025-11-25"),
                    ..headers()
                },
                None,
            ),
        ];
        for (label, request, expected) in cases {
            let result = validate_headers(request, 47615, "secret", &sessions, Some("tools/list"));
            assert_eq!(result.err(), expected, "{label}");
        }
    }

    #[test]
    fn streamable_http_media_types_are_checked_without_sockets() {
        assert!(is_json_content_type(Some(
            "application/json; charset=utf-8"
        )));
        assert!(!is_json_content_type(Some("text/plain")));
        assert!(accepts_mcp_streamable_http(Some(
            "application/json, text/event-stream; charset=utf-8"
        )));
        assert!(!accepts_mcp_streamable_http(Some("application/json")));
    }

    #[test]
    fn request_body_reader_stops_at_the_four_mebibyte_limit() {
        let small = b"request";
        assert_eq!(
            read_limited_body(&mut &small[..]).unwrap(),
            Some(small.to_vec())
        );
        let exactly_at_limit = vec![0; MAX_BODY_BYTES];
        assert_eq!(
            read_limited_body(&mut &exactly_at_limit[..])
                .unwrap()
                .unwrap()
                .len(),
            MAX_BODY_BYTES
        );
        let oversized = vec![0; MAX_BODY_BYTES + 1];
        assert_eq!(read_limited_body(&mut &oversized[..]).unwrap(), None);
    }

    #[test]
    fn waiting_outcomes_map_to_shutdown_and_deadline_http_statuses() {
        assert_eq!(
            wait_failure(WaitResult::Shutdown).map(HttpFailure::status),
            Some(503)
        );
        assert_eq!(
            wait_failure(WaitResult::Deadline).map(HttpFailure::status),
            Some(504)
        );
    }

    #[test]
    fn an_internal_tool_call_is_abandoned_after_its_waiter_is_dropped() {
        let (call, response) = pending_tool_call(
            "probe",
            serde_json::json!({}),
            Permission::Read,
            Instant::now() + Duration::from_secs(5),
        );
        assert!(!call.is_abandoned());
        drop(response);
        assert!(call.is_abandoned());
    }
}
