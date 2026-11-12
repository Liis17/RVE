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
    token_hash: &str,
    sessions: &HashMap<String, Session>,
    rpc_method: Option<&str>,
) -> Result<(), HttpFailure> {
    if !valid_bearer(headers.authorization, token_hash) {
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

/// Сверяет хеш предъявленного токена с хранимым; сам токен сервер не хранит.
fn valid_bearer(value: Option<&str>, token_hash: &str) -> bool {
    value
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, value)| {
            scheme.eq_ignore_ascii_case("Bearer")
                && constant_time_eq(crate::token_hash(value).as_bytes(), token_hash.as_bytes())
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
    if let Err(failure) = validate_authority(headers, shared.port, &shared.token_hash) {
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

fn validate_authority(
    headers: Headers<'_>,
    port: u16,
    token_hash: &str,
) -> Result<(), HttpFailure> {
    if !valid_bearer(headers.authorization, token_hash) {
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

/// Upper bound on live sessions; the least recently used one is evicted to make room.
pub(crate) const MAX_SESSIONS: usize = 64;

/// Result of handling one POST body, before any HTTP response is written.
#[derive(Debug)]
pub(crate) enum PostOutcome {
    Failure(HttpFailure),
    Json {
        status: u16,
        body: Value,
        session_id: Option<String>,
    },
    Empty(u16),
    /// A tool call was queued for the editor thread; the caller waits for its response.
    AwaitTool {
        id: Value,
        response: flume::Receiver<ToolResult>,
        deadline: Instant,
    },
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
    match post_outcome(&body, headers, shared) {
        PostOutcome::Failure(failure) => respond_failure(request, failure),
        PostOutcome::Json {
            status,
            body,
            session_id,
        } => {
            let extra = session_headers(session_id);
            respond_json(request, status, body, &extra);
        }
        PostOutcome::Empty(status) => respond_empty(request, status),
        PostOutcome::AwaitTool {
            id,
            response,
            deadline,
        } => match wait_for_result(&response, shutdown, deadline) {
            WaitResult::Response(result) => {
                respond_json(request, 200, success(Some(id), result.to_value()), &[])
            }
            result => respond_failure(
                request,
                wait_failure(result).expect("non-response wait outcomes map to an HTTP failure"),
            ),
        },
    }
}

/// Decides the POST outcome: session lifecycle, JSON-RPC routing, and queuing of tool calls.
pub(crate) fn post_outcome(body: &[u8], headers: Headers<'_>, shared: &Shared) -> PostOutcome {
    let rpc = match parse_request(body) {
        Ok(request) => request,
        Err(rpc_error) => {
            return PostOutcome::Json {
                status: 400,
                body: error(None, rpc_error.code, rpc_error.message, None),
                session_id: None,
            };
        }
    };
    let clock = shared
        .clock
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let mut sessions = shared.sessions.lock().unwrap();
    if let Err(failure) = validate_headers(
        headers,
        shared.port,
        &shared.token_hash,
        &sessions,
        Some(&rpc.method),
    ) {
        return PostOutcome::Failure(failure);
    }

    if rpc.method == "initialize" {
        let Some(id) = rpc.id.clone() else {
            return PostOutcome::Json {
                status: 400,
                body: error(None, -32600, "Invalid Request", None),
                session_id: None,
            };
        };
        let Some(client_version) = initialize_client_version(&rpc.params) else {
            return PostOutcome::Json {
                status: 400,
                body: error(Some(id), -32602, "Invalid params", None),
                session_id: None,
            };
        };
        let (protocol_version, result) = initialize_result(Some(client_version), crate::VERSION);
        let session_id = match new_session_id() {
            Ok(id) => id,
            Err(_) => return PostOutcome::Failure(HttpFailure::InternalServerError),
        };
        insert_session(
            &mut sessions,
            session_id.clone(),
            Session::new(protocol_version, clock),
        );
        return PostOutcome::Json {
            status: 200,
            body: success(Some(id), result),
            session_id: Some(session_id),
        };
    }

    let Some(session_id) = headers.session_id else {
        return PostOutcome::Failure(HttpFailure::BadRequest);
    };
    let Some(session) = sessions.get_mut(session_id) else {
        return PostOutcome::Failure(HttpFailure::NotFound);
    };
    session.last_used = clock;
    if rpc.method == "notifications/initialized" && rpc.id.is_some() {
        return PostOutcome::Json {
            status: 400,
            body: error(rpc.id, -32600, "Invalid Request", None),
            session_id: None,
        };
    }
    match session.accept(&rpc.method) {
        Ok(false) => return PostOutcome::Empty(202),
        Ok(true) => {}
        Err(message) => {
            return PostOutcome::Json {
                status: 200,
                body: error(rpc.id, -32002, message, None),
                session_id: None,
            };
        }
    }
    drop(sessions);

    let Some(id) = rpc.id else {
        return PostOutcome::Empty(202);
    };
    match rpc.method.as_str() {
        "ping" => PostOutcome::Json {
            status: 200,
            body: success(Some(id), json!({})),
            session_id: None,
        },
        // Постоянный список: права не фильтруют его, а проверяются при вызове.
        "tools/list" => {
            let tools = shared
                .tools
                .iter()
                .map(crate::tools::ToolSpec::to_value)
                .collect::<Vec<_>>();
            PostOutcome::Json {
                status: 200,
                body: success(Some(id), json!({ "tools": tools })),
                session_id: None,
            }
        }
        "tools/call" => tool_call_outcome(id, &rpc.params, shared),
        _ => PostOutcome::Json {
            status: 200,
            body: error(Some(id), -32601, "Method not found", None),
            session_id: None,
        },
    }
}

/// Inserts a session; at the cap, the least recently used session is evicted first.
fn insert_session(sessions: &mut HashMap<String, Session>, id: String, session: Session) {
    if sessions.len() >= MAX_SESSIONS
        && let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, session)| session.last_used)
            .map(|(id, _)| id.clone())
    {
        sessions.remove(&oldest);
    }
    sessions.insert(id, session);
}

fn tool_call_outcome(id: Value, params: &Value, shared: &Shared) -> PostOutcome {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return PostOutcome::Json {
            status: 200,
            body: error(Some(id), -32602, "Invalid params", None),
            session_id: None,
        };
    };
    let Some(tool) = shared.tools.iter().find(|tool| tool.name == name) else {
        return PostOutcome::Json {
            status: 200,
            body: error(Some(id), -32602, "Unknown tool", None),
            session_id: None,
        };
    };
    let permission = tool.permission;
    let permissions = *shared.permissions.read().unwrap();
    if !permissions.allows(permission) {
        let result = ToolResult::error(format!("Permission `{}` is disabled", permission.flag()));
        return PostOutcome::Json {
            status: 200,
            body: success(Some(id), result.to_value()),
            session_id: None,
        };
    }

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let deadline = Instant::now() + TOOL_CALL_TIMEOUT;
    let generation = shared.generation.load(std::sync::atomic::Ordering::Acquire);
    let (call, response) = pending_tool_call(name, arguments, permission, deadline, generation);
    if shared.calls.send(call).is_err() {
        return PostOutcome::Failure(HttpFailure::ServiceUnavailable);
    }
    (shared.wake)();
    PostOutcome::AwaitTool {
        id,
        response,
        deadline,
    }
}

fn pending_tool_call(
    name: &str,
    arguments: Value,
    permission: Permission,
    deadline: Instant,
    generation: u64,
) -> (ToolCall, flume::Receiver<ToolResult>) {
    let (responder, response) = flume::bounded(1);
    (
        ToolCall {
            name: name.to_owned(),
            arguments,
            deadline,
            required_permission: permission,
            generation,
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
    match delete_session(headers, shared) {
        Ok(()) => respond_empty(request, 200),
        Err(failure) => respond_failure(request, failure),
    }
}

/// Ends the session named by `Mcp-Session-Id`; later requests with it get 404.
fn delete_session(headers: Headers<'_>, shared: &Shared) -> Result<(), HttpFailure> {
    let mut sessions = shared.sessions.lock().unwrap();
    validate_headers(headers, shared.port, &shared.token_hash, &sessions, None)?;
    if let Some(session_id) = headers.session_id {
        sessions.remove(session_id);
    }
    Ok(())
}

fn session_headers(session_id: Option<String>) -> Vec<Header> {
    session_id
        .map(|id| {
            Header::from_bytes("Mcp-Session-Id", id)
                .expect("MCP session header name and value are valid")
        })
        .into_iter()
        .collect()
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
        Headers, HttpFailure, MAX_BODY_BYTES, MAX_SESSIONS, PostOutcome,
        accepts_mcp_streamable_http, delete_session, insert_session, is_json_content_type,
        pending_tool_call, post_outcome, read_limited_body, validate_headers, wait_failure,
    };
    use crate::protocol::{Session, SessionPhase};
    use crate::server::{Shared, ToolCall, WaitResult, wait_for_result};
    use crate::{Permission, Permissions, ToolResult, ToolSpec};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex, RwLock};
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
    fn bearer_is_checked_against_the_stored_hash_only() {
        let stored = crate::token_hash("secret");
        assert_eq!(stored.len(), 64);
        assert!(super::valid_bearer(Some("Bearer secret"), &stored));
        assert!(super::valid_bearer(Some("bearer secret"), &stored));
        assert!(!super::valid_bearer(Some("Bearer wrong"), &stored));
        // Предъявленный хеш вместо токена доступа не даёт.
        assert!(!super::valid_bearer(
            Some(&format!("Bearer {stored}")),
            &stored
        ));
        assert!(!super::valid_bearer(None, &stored));
    }

    #[test]
    fn request_header_checks_cover_auth_host_origin_protocol_and_session() {
        let mut sessions = HashMap::new();
        sessions.insert(
            "known".to_owned(),
            Session {
                protocol_version: "2025-11-25".to_owned(),
                phase: SessionPhase::Initializing,
                last_used: 0,
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
            let result = validate_headers(
                request,
                47615,
                &crate::token_hash("secret"),
                &sessions,
                Some("initialize"),
            );
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
                &crate::token_hash("secret"),
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
            let result = validate_headers(
                request,
                47615,
                &crate::token_hash("secret"),
                &sessions,
                Some("tools/list"),
            );
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
            41,
        );
        assert!(!call.is_abandoned());
        assert_eq!(call.generation(), 41);
        drop(response);
        assert!(call.is_abandoned());
    }

    #[test]
    fn session_table_evicts_the_least_recently_used_session_at_the_cap() {
        let mut sessions = HashMap::new();
        for clock in 0..MAX_SESSIONS as u64 {
            insert_session(
                &mut sessions,
                format!("s{clock}"),
                Session::new("2025-11-25".to_owned(), clock),
            );
        }
        assert_eq!(sessions.len(), MAX_SESSIONS);
        // s0 is used again, so s1 becomes the least recently used session.
        sessions.get_mut("s0").unwrap().last_used = 1_000;
        insert_session(
            &mut sessions,
            "newest".to_owned(),
            Session::new("2025-11-25".to_owned(), 1_001),
        );
        assert_eq!(sessions.len(), MAX_SESSIONS);
        assert!(sessions.contains_key("s0"));
        assert!(!sessions.contains_key("s1"));
        assert!(sessions.contains_key("newest"));
    }

    fn shared_with(tools: Vec<ToolSpec>) -> (Shared, flume::Receiver<ToolCall>) {
        let (calls, receiver) = flume::unbounded();
        let shared = Shared {
            port: 47615,
            token_hash: crate::token_hash("secret"),
            tools,
            sessions: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(1),
            permissions: Arc::new(RwLock::new(Permissions::default())),
            generation: Arc::new(AtomicU64::new(0)),
            calls,
            wake: Arc::new(|| {}),
        };
        (shared, receiver)
    }

    fn rpc_headers<'a>(method: &'a str, session: Option<&'a str>) -> Headers<'a> {
        Headers {
            method,
            path: "/mcp",
            authorization: Some("Bearer secret"),
            host: Some("127.0.0.1:47615"),
            origin: None,
            content_type: Some("application/json"),
            accept: Some("application/json, text/event-stream"),
            protocol_version: session.map(|_| "2025-11-25"),
            session_id: session,
        }
    }

    #[test]
    fn session_phases_and_delete_are_enforced_without_sockets() {
        let (shared, calls) = shared_with(vec![
            ToolSpec::new(
                "probe",
                "Probe.",
                json!({"type": "object"}),
                Permission::Read,
            ),
            ToolSpec::new(
                "set",
                "Set.",
                json!({"type": "object"}),
                Permission::EditScene,
            ),
        ]);
        let initialize = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
        let PostOutcome::Json {
            status: 200,
            session_id: Some(session),
            ..
        } = post_outcome(initialize, rpc_headers("POST", None), &shared)
        else {
            panic!("initialize must answer 200 with a session id");
        };
        let session = session.as_str();
        let tools_list = br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let call = br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"probe"}}"#;

        // Before notifications/initialized a tool call is a JSON-RPC error and never reaches the editor.
        let PostOutcome::Json { body, .. } =
            post_outcome(call, rpc_headers("POST", Some(session)), &shared)
        else {
            panic!("tools/call before initialized must be answered in JSON");
        };
        assert_eq!(body["error"]["code"], -32002);
        assert!(calls.try_recv().is_err());

        let initialized = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(matches!(
            post_outcome(initialized, rpc_headers("POST", Some(session)), &shared),
            PostOutcome::Empty(202)
        ));

        // The list is complete for every permission state; the description names what each tool needs.
        let PostOutcome::Json { body, .. } =
            post_outcome(tools_list, rpc_headers("POST", Some(session)), &shared)
        else {
            panic!("tools/list must be answered in JSON");
        };
        let tools = body["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(
            tools[1]["description"]
                .as_str()
                .unwrap()
                .contains("Requires permission: edit_scene")
        );

        let PostOutcome::AwaitTool {
            response, deadline, ..
        } = post_outcome(call, rpc_headers("POST", Some(session)), &shared)
        else {
            panic!("tools/call after initialization must queue the call");
        };
        let queued = calls.try_recv().expect("the call reached the editor queue");
        assert_eq!(queued.name, "probe");
        assert!(!queued.is_abandoned());
        assert!(queued.respond(ToolResult::text("done")));
        let (_shutdown_tx, shutdown_rx) = flume::unbounded::<()>();
        assert_eq!(
            wait_for_result(&response, &shutdown_rx, deadline),
            WaitResult::Response(ToolResult::text("done"))
        );

        assert_eq!(
            delete_session(rpc_headers("DELETE", Some(session)), &shared),
            Ok(())
        );
        let PostOutcome::Failure(failure) =
            post_outcome(tools_list, rpc_headers("POST", Some(session)), &shared)
        else {
            panic!("a deleted session must not be accepted");
        };
        assert_eq!(failure, HttpFailure::NotFound);
    }
}
