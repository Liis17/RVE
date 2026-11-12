use crate::{Permission, Permissions, ToolSpec};
use crate::{protocol::Session, tools::ToolResult};
use serde_json::Value;
use std::{
    collections::HashMap,
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tiny_http::Server;

pub(crate) const WORKER_COUNT: usize = 4;
pub(crate) const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Как часто воркер проверяет соединение клиента, пока вызов ждёт главный поток.
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the editor's local MCP endpoint.
pub struct McpConfig {
    pub port: u16,
    pub token: String,
    pub permissions: Permissions,
    pub tools: Vec<ToolSpec>,
}

/// A tool invocation waiting for the editor's main thread.
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub deadline: Instant,
    pub required_permission: Permission,
    /// Editor-state generation captured when the HTTP worker queued this call.
    pub(crate) generation: u64,
    pub(crate) responder: flume::Sender<ToolResult>,
    /// Клиент отменил вызов (`notifications/cancelled`) или отключился.
    pub(crate) cancelled: Arc<AtomicBool>,
}

impl ToolCall {
    /// Completes the HTTP request. Returns false when the request no longer has a waiter.
    pub fn respond(&self, result: ToolResult) -> bool {
        self.responder.send(result).is_ok()
    }

    /// Reports whether the HTTP worker timed out, shut down, or dropped its response receiver,
    /// or the client cancelled the call or disconnected.
    pub fn is_abandoned(&self) -> bool {
        Instant::now() >= self.deadline
            || self.responder.is_disconnected()
            || self.cancelled.load(Ordering::Acquire)
    }

    /// Returns the editor-state generation captured when the call was queued.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Running localhost MCP server. Dropping it requests shutdown without joining workers.
pub struct McpServer {
    calls: flume::Receiver<ToolCall>,
    permissions: Arc<RwLock<Permissions>>,
    shutdown: Option<flume::Sender<()>>,
    listener: Arc<Server>,
    generation: Arc<AtomicU64>,
}

pub(crate) struct Shared {
    pub port: u16,
    pub token: String,
    pub tools: Vec<ToolSpec>,
    pub sessions: Mutex<HashMap<String, Session>>,
    /// Счётчик обращений к сессиям для вытеснения самой давно использованной.
    pub clock: AtomicU64,
    pub permissions: Arc<RwLock<Permissions>>,
    pub generation: Arc<AtomicU64>,
    pub calls: flume::Sender<ToolCall>,
    pub wake: Arc<dyn Fn() + Send + Sync>,
    /// Ожидающие вызовы по ключу сессии и JSON-RPC id: по ним `notifications/cancelled`
    /// находит вызов.
    pub inflight: Mutex<HashMap<String, InFlight>>,
}

/// Ожидающий вызов, который можно отменить.
pub(crate) struct InFlight {
    pub cancelled: Arc<AtomicBool>,
    pub signal: flume::Sender<()>,
}

impl InFlight {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.signal.try_send(());
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WaitResult {
    Response(ToolResult),
    Shutdown,
    Deadline,
    Disconnected,
    /// Клиент отменил вызов.
    Cancelled,
    /// Прошёл интервал проверки соединения, а результата ещё нет.
    Tick,
}

impl McpServer {
    /// Binds only to IPv4 localhost and starts four detached request workers.
    pub fn start(config: McpConfig, wake: Arc<dyn Fn() + Send + Sync>) -> io::Result<Self> {
        Self::start_with_generation(config, wake, 0)
    }

    /// Starts a server and stamps each queued tool call with the supplied state generation.
    pub fn start_with_generation(
        config: McpConfig,
        wake: Arc<dyn Fn() + Send + Sync>,
        generation: u64,
    ) -> io::Result<Self> {
        if config.port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP port must be non-zero",
            ));
        }
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let listener = Arc::new(Server::http(address).map_err(io::Error::other)?);
        let (calls_tx, calls_rx) = flume::unbounded();
        let (shutdown_tx, shutdown_rx) = flume::unbounded();
        let permissions = Arc::new(RwLock::new(config.permissions));
        let generation = Arc::new(AtomicU64::new(generation));
        let shared = Arc::new(Shared {
            port: config.port,
            token: config.token,
            tools: config.tools,
            sessions: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(0),
            permissions: Arc::clone(&permissions),
            generation: Arc::clone(&generation),
            calls: calls_tx,
            wake,
            inflight: Mutex::new(HashMap::new()),
        });

        for index in 0..WORKER_COUNT {
            let worker_listener = Arc::clone(&listener);
            let shared = Arc::clone(&shared);
            let shutdown = shutdown_rx.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("engine-mcp-{index}"))
                .spawn(move || crate::http::worker(worker_listener, shared, shutdown))
            {
                drop(shutdown_tx);
                listener.unblock();
                return Err(error);
            }
        }

        Ok(Self {
            calls: calls_rx,
            permissions,
            shutdown: Some(shutdown_tx),
            listener,
            generation,
        })
    }

    /// Receives one tool call without waiting.
    pub fn try_recv(&self) -> Option<ToolCall> {
        self.calls.try_recv().ok()
    }

    /// Returns the current permissions so the host can re-check them before execution.
    pub fn permissions(&self) -> Permissions {
        *self.permissions.read().unwrap()
    }

    /// Replaces permissions for subsequent HTTP checks and host-side revalidation.
    pub fn set_permissions(&self, permissions: Permissions) {
        *self.permissions.write().unwrap() = permissions;
    }

    /// Updates the generation stamped onto calls enqueued after this point.
    pub fn set_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }

    /// Signals waiting requests and unblocks workers receiving from tiny_http.
    pub fn shutdown(&mut self) {
        self.shutdown.take();
        self.listener.unblock();
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Ждёт результат, остановку сервера, отмену или deadline не дольше `tick`.
pub(crate) fn wait_for_result(
    response: &flume::Receiver<ToolResult>,
    shutdown: &flume::Receiver<()>,
    cancel: &flume::Receiver<()>,
    deadline: Instant,
    tick: Duration,
) -> WaitResult {
    use flume::{Selector, select::SelectError};

    let remaining = deadline.saturating_duration_since(Instant::now());
    let selected = Selector::new()
        .recv(response, WaitResult::from_response)
        .recv(shutdown, |_| WaitResult::Shutdown)
        // Ожидающий держит свой отправитель отмены, поэтому канал не разрывается.
        .recv(cancel, |_| WaitResult::Cancelled)
        .wait_timeout(remaining.min(tick));
    let result = match selected {
        Ok(result) => result,
        Err(SelectError::Timeout) if Instant::now() < deadline => return WaitResult::Tick,
        Err(SelectError::Timeout) => return WaitResult::Deadline,
    };
    if Instant::now() >= deadline {
        WaitResult::Deadline
    } else {
        result
    }
}

impl WaitResult {
    fn from_response(result: Result<ToolResult, flume::RecvError>) -> Self {
        match result {
            Ok(result) => Self::Response(result),
            Err(_) => Self::Disconnected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolCall, WaitResult, wait_for_result};
    use crate::{Permission, ToolResult};
    use serde_json::json;
    use std::{
        thread,
        time::{Duration, Instant},
    };

    /// Канал отмены, который не срабатывает: отправитель живёт, пока жив процесс теста.
    fn never_cancelled() -> flume::Receiver<()> {
        let (sender, receiver) = flume::bounded(1);
        std::mem::forget(sender);
        receiver
    }

    #[test]
    fn a_tick_without_a_result_is_reported_before_the_deadline() {
        let (_response_tx, response_rx) = flume::bounded(1);
        let (_shutdown_tx, shutdown_rx) = flume::unbounded();
        assert_eq!(
            wait_for_result(
                &response_rx,
                &shutdown_rx,
                &never_cancelled(),
                Instant::now() + Duration::from_secs(120),
                Duration::from_millis(1),
            ),
            WaitResult::Tick
        );
    }

    #[test]
    fn a_cancelled_flag_abandons_the_call() {
        let (responder, _receiver) = flume::bounded(1);
        let call = ToolCall {
            name: "probe".to_owned(),
            arguments: json!({}),
            deadline: Instant::now() + Duration::from_secs(5),
            required_permission: Permission::Read,
            generation: 0,
            responder,
            cancelled: Default::default(),
        };
        assert!(!call.is_abandoned());
        call.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(call.is_abandoned());
    }

    #[test]
    fn shutdown_wakes_an_waiting_call_without_a_join_or_timeout() {
        let (_response_tx, response_rx) = flume::bounded(1);
        let (shutdown_tx, shutdown_rx) = flume::unbounded::<()>();
        drop(shutdown_tx);
        assert_eq!(
            wait_for_result(
                &response_rx,
                &shutdown_rx,
                &never_cancelled(),
                Instant::now() + Duration::from_secs(120),
                Duration::from_secs(120),
            ),
            WaitResult::Shutdown
        );
    }

    #[test]
    fn an_expired_deadline_returns_without_waiting() {
        let (_response_tx, response_rx) = flume::bounded(1);
        let (_shutdown_tx, shutdown_rx) = flume::unbounded();
        assert_eq!(
            wait_for_result(
                &response_rx,
                &shutdown_rx,
                &never_cancelled(),
                Instant::now(),
                Duration::from_secs(1),
            ),
            WaitResult::Deadline
        );
    }

    #[test]
    fn another_call_can_finish_while_a_first_call_is_waiting() {
        let (first_tx, first_rx) = flume::bounded(1);
        let (second_tx, second_rx) = flume::bounded(1);
        let (_shutdown_tx, shutdown_rx) = flume::unbounded();
        let (started_tx, started_rx) = flume::bounded(1);
        let shutdown_for_first = shutdown_rx.clone();
        let first = thread::spawn(move || {
            started_tx.send(()).unwrap();
            wait_for_result(
                &first_rx,
                &shutdown_for_first,
                &never_cancelled(),
                Instant::now() + Duration::from_secs(5),
                Duration::from_secs(5),
            )
        });
        started_rx.recv().unwrap();

        second_tx.send(ToolResult::text("second")).unwrap();
        assert!(matches!(
            wait_for_result(
                &second_rx,
                &shutdown_rx,
                &never_cancelled(),
                Instant::now() + Duration::from_secs(5),
                Duration::from_secs(5),
            ),
            WaitResult::Response(ToolResult { .. })
        ));
        first_tx.send(ToolResult::text("first")).unwrap();
        assert!(matches!(first.join().unwrap(), WaitResult::Response(_)));
    }

    #[test]
    fn a_tool_call_is_abandoned_after_its_waiter_disconnects_or_deadline_passes() {
        let (responder, receiver) = flume::bounded(1);
        let call = ToolCall {
            name: "probe".to_owned(),
            arguments: json!({}),
            deadline: Instant::now() + Duration::from_secs(5),
            required_permission: Permission::Read,
            generation: 0,
            responder,
            cancelled: Default::default(),
        };
        assert!(!call.is_abandoned());
        drop(receiver);
        assert!(call.is_abandoned());

        let (responder, _receiver) = flume::bounded(1);
        let expired = ToolCall {
            name: "probe".to_owned(),
            arguments: json!({}),
            deadline: Instant::now(),
            required_permission: Permission::Read,
            generation: 0,
            responder,
            cancelled: Default::default(),
        };
        assert!(expired.is_abandoned());
    }
}
