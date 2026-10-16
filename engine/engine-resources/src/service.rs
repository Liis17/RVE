use crate::{ResourceConfig, ResourceError, SourceMode};
use engine_files::{FileSource, ReadAt, VirtualPath};
use engine_pak::{
    Archive, ArchiveCatalog, ArchiveLimits, ArchiveSetRegistry, CatalogLimits, MemoryFootprint,
    PakError, ReadControl, ReadReservation, discover_archives,
};
use flume::{Receiver, Selector, Sender, TryRecvError, TrySendError};
use std::fs::{self, File};
use std::io;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

/// The minimum metadata returned by [`ResourceHandle::stat`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceStat {
    /// Decoded byte length of the resource.
    pub size: u64,
}

/// Owner of the coordinator and all resource worker threads.
///
/// Dropping the owner is equivalent to [`shutdown`](Self::shutdown). Handles
/// may outlive it; after shutdown they return [`ResourceErrorKind::Stopped`].
pub struct ResourceSystem {
    handle: ResourceHandle,
    stop_tx: Sender<()>,
    stopped: Arc<AtomicBool>,
    gate: Arc<AdmissionGate>,
    fatal_worker_panic: Arc<AtomicBool>,
    coordinator: Option<JoinHandle<()>>,
}

impl ResourceSystem {
    /// Discovers and validates the selected resource source, then starts workers.
    ///
    /// Top-level `.rvepak` files select packed-only mode. With no archives, the
    /// configured root is read as loose files. A missing root is allowed so a
    /// game can start before it has file-backed resources.
    pub fn start(config: ResourceConfig) -> Result<Self, ResourceError> {
        start_inner(config, None)
    }

    /// Returns a cloneable handle for enqueueing requests from other subsystems.
    pub fn handle(&self) -> ResourceHandle {
        self.handle.clone()
    }

    /// Stops admission, completes queued requests as stopped, and joins every
    /// resource thread. Active local OS reads finish at their next cancellation
    /// boundary; an in-progress blocking OS read cannot be interrupted.
    pub fn shutdown(&mut self) -> Result<(), ResourceError> {
        self.stopped.store(true, Ordering::Release);
        self.gate.close();
        let _ = self.stop_tx.try_send(());
        if let Some(coordinator) = self.coordinator.take()
            && coordinator.join().is_err()
        {
            self.fatal_worker_panic.store(true, Ordering::Release);
        }
        if self.fatal_worker_panic.load(Ordering::Acquire) {
            Err(ResourceError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

impl Drop for ResourceSystem {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// Cloneable, nonblocking entry point for resource access.
#[derive(Clone)]
pub struct ResourceHandle {
    inner: Arc<HandleInner>,
}

impl std::fmt::Debug for ResourceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceHandle")
            .field("root", &self.inner.root)
            .field("stopped", &self.inner.stopped.load(Ordering::Acquire))
            .finish()
    }
}

impl ResourceHandle {
    /// The configured resource root, used by consumers that also expose a root API.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Enqueues a metadata query for a canonical virtual path.
    pub fn stat(
        &self,
        path: impl AsRef<str>,
    ) -> Result<ResourceRequest<ResourceStat>, ResourceError> {
        self.enqueue(path.as_ref(), Operation::Stat, decode_stat)
    }

    /// Enqueues a read of the complete decoded resource.
    pub fn read_all(
        &self,
        path: impl AsRef<str>,
    ) -> Result<ResourceRequest<Vec<u8>>, ResourceError> {
        self.enqueue(path.as_ref(), Operation::ReadAll, decode_bytes)
    }

    /// Enqueues a read of the half-open byte range `[start, end)`.
    ///
    /// The returned vector contains decoded resource bytes. The range is
    /// checked against the decoded size on a worker and is never clamped.
    pub fn read_range(
        &self,
        path: impl AsRef<str>,
        range: Range<u64>,
    ) -> Result<ResourceRequest<Vec<u8>>, ResourceError> {
        self.enqueue(path.as_ref(), Operation::ReadRange(range), decode_bytes)
    }

    fn enqueue<T>(
        &self,
        path: &str,
        operation: Operation,
        decode: fn(ResponseValue) -> Result<T, ResourceError>,
    ) -> Result<ResourceRequest<T>, ResourceError> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(ResourceError::Stopped);
        }
        let path = VirtualPath::parse(path).map_err(ResourceError::InvalidPath)?;
        let _enqueue = self.inner.gate.try_enter()?;
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(ResourceError::Stopped);
        }
        let admission = self.inner.outstanding.try_acquire()?;
        let control = Arc::new(JobControl {
            cancelled: AtomicBool::new(false),
            stopped: Arc::clone(&self.inner.stopped),
            budget: Arc::clone(&self.inner.memory),
            max_result_bytes: self.inner.max_result_bytes,
            reservation: Mutex::new(None),
        });
        let (response_tx, response_rx) = flume::bounded(1);
        let job = Job {
            path,
            operation,
            control: Arc::clone(&control),
            response: response_tx,
            #[cfg(test)]
            panic_for_test: self.inner.panic_next.swap(false, Ordering::AcqRel),
        };

        match self.inner.jobs.try_send(job) {
            Ok(()) if !self.inner.stopped.load(Ordering::Acquire) => Ok(ResourceRequest {
                response: response_rx,
                admission: Some(admission),
                control,
                stopped: Arc::clone(&self.inner.stopped),
                fatal_worker_panic: Arc::clone(&self.inner.fatal_worker_panic),
                decode,
                completed: false,
            }),
            Ok(()) => {
                control.cancelled.store(true, Ordering::Release);
                Err(ResourceError::Stopped)
            }
            Err(TrySendError::Full(_)) => Err(ResourceError::QueueFull),
            Err(TrySendError::Disconnected(_)) => {
                if self.inner.stopped.load(Ordering::Acquire) {
                    Err(ResourceError::Stopped)
                } else {
                    Err(ResourceError::WorkerPanicked)
                }
            }
        }
    }

    #[cfg(test)]
    fn panic_on_next_worker_job(&self) {
        self.inner.panic_next.store(true, Ordering::Release);
    }
}

/// One submitted operation. Dropping it cancels work that has not completed.
#[must_use = "dropping a request cancels it; receive it, poll it, or explicitly wait for it"]
pub struct ResourceRequest<T> {
    response: Receiver<Response>,
    admission: Option<AdmissionPermit>,
    control: Arc<JobControl>,
    stopped: Arc<AtomicBool>,
    fatal_worker_panic: Arc<AtomicBool>,
    decode: fn(ResponseValue) -> Result<T, ResourceError>,
    completed: bool,
}

impl<T> ResourceRequest<T> {
    /// Receives the result asynchronously. This future is runtime-agnostic.
    pub async fn receive(mut self) -> Result<T, ResourceError> {
        if self.completed {
            return Err(ResourceError::RequestCompleted);
        }
        let response = self
            .response
            .recv_async()
            .await
            .map_err(|_| self.closed_error())?;
        self.finish_response(response)
    }

    /// Tries to receive without blocking the calling thread.
    pub fn try_receive(&mut self) -> Result<Option<T>, ResourceError> {
        if self.completed {
            return Err(ResourceError::RequestCompleted);
        }
        match self.response.try_recv() {
            Ok(response) => self.finish_response(response).map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(self.closed_error()),
        }
    }

    /// Explicit blocking receive for startup and legacy callers.
    ///
    /// Do not call this from a window or event-loop thread.
    pub fn wait(mut self) -> Result<T, ResourceError> {
        if self.completed {
            return Err(ResourceError::RequestCompleted);
        }
        let response = self.response.recv().map_err(|_| self.closed_error())?;
        self.finish_response(response)
    }

    fn finish_response(&mut self, response: Response) -> Result<T, ResourceError> {
        self.completed = true;
        self.admission.take();
        let Response { result, memory } = response;
        let value = result?;
        // The service budget covers an unreceived result. Once it is handed to
        // the consumer, ownership of the bytes and their memory moves out.
        drop(memory);
        (self.decode)(value)
    }

    fn closed_error(&self) -> ResourceError {
        if self.fatal_worker_panic.load(Ordering::Acquire) {
            ResourceError::WorkerPanicked
        } else if self.stopped.load(Ordering::Acquire) {
            ResourceError::Stopped
        } else {
            ResourceError::WorkerPanicked
        }
    }
}

impl<T> Drop for ResourceRequest<T> {
    fn drop(&mut self) {
        if !self.completed {
            self.control.cancelled.store(true, Ordering::Release);
        }
    }
}

struct HandleInner {
    root: PathBuf,
    jobs: Sender<Job>,
    outstanding: Arc<Outstanding>,
    memory: Arc<MemoryBudget>,
    max_result_bytes: u64,
    stopped: Arc<AtomicBool>,
    gate: Arc<AdmissionGate>,
    fatal_worker_panic: Arc<AtomicBool>,
    #[cfg(test)]
    panic_next: AtomicBool,
}

#[derive(Debug)]
enum Operation {
    Stat,
    ReadAll,
    ReadRange(Range<u64>),
}

struct Job {
    path: VirtualPath,
    operation: Operation,
    control: Arc<JobControl>,
    response: Sender<Response>,
    #[cfg(test)]
    panic_for_test: bool,
}

impl Job {
    fn respond(self, result: Result<ResponseValue, ResourceError>, memory: Option<BudgetLease>) {
        let _ = self.response.try_send(Response { result, memory });
    }

    fn respond_stopped(self) {
        self.respond(Err(ResourceError::Stopped), None);
    }
}

struct Response {
    result: Result<ResponseValue, ResourceError>,
    memory: Option<BudgetLease>,
}

enum ResponseValue {
    Stat(ResourceStat),
    Bytes(Vec<u8>),
}

fn decode_stat(value: ResponseValue) -> Result<ResourceStat, ResourceError> {
    match value {
        ResponseValue::Stat(stat) => Ok(stat),
        ResponseValue::Bytes(_) => Err(ResourceError::WorkerPanicked),
    }
}

fn decode_bytes(value: ResponseValue) -> Result<Vec<u8>, ResourceError> {
    match value {
        ResponseValue::Bytes(bytes) => Ok(bytes),
        ResponseValue::Stat(_) => Err(ResourceError::WorkerPanicked),
    }
}

struct JobControl {
    cancelled: AtomicBool,
    stopped: Arc<AtomicBool>,
    budget: Arc<MemoryBudget>,
    max_result_bytes: u64,
    reservation: Mutex<Option<BudgetLease>>,
}

impl JobControl {
    fn error_if_cancelled(&self) -> Option<ResourceError> {
        if self.stopped.load(Ordering::Acquire) {
            Some(ResourceError::Stopped)
        } else if self.cancelled.load(Ordering::Acquire) {
            Some(ResourceError::Cancelled)
        } else {
            None
        }
    }

    fn take_result_lease(&self, result_bytes: u64) -> Result<Option<BudgetLease>, ResourceError> {
        let mut reservation = match self.reservation.lock() {
            Ok(reservation) => reservation,
            Err(poisoned) => poisoned.into_inner(),
        };
        match reservation.take() {
            Some(mut lease) => {
                lease.shrink_to(result_bytes)?;
                Ok(Some(lease))
            }
            None if result_bytes == 0 => Ok(None),
            None => Err(ResourceError::WorkerPanicked),
        }
    }

    fn release_reservation(&self) {
        let mut reservation = match self.reservation.lock() {
            Ok(reservation) => reservation,
            Err(poisoned) => poisoned.into_inner(),
        };
        reservation.take();
    }
}

impl ReadControl for JobControl {
    fn reserve(
        &self,
        memory: MemoryFootprint,
    ) -> engine_pak::Result<Box<dyn ReadReservation + '_>> {
        if self.is_cancelled() {
            return Err(PakError::Cancelled);
        }
        if memory.result_bytes > self.max_result_bytes {
            return Err(PakError::BudgetExceeded {
                requested: memory.result_bytes,
                limit: self.max_result_bytes,
            });
        }
        let requested = memory.total_bytes().ok_or(PakError::BudgetExceeded {
            requested: u64::MAX,
            limit: self.budget.limit,
        })?;
        let lease = self.budget.reserve(requested)?;
        let mut reservation = self
            .reservation
            .lock()
            .map_err(|_| PakError::InvalidConfiguration("read reservation lock poisoned".into()))?;
        if reservation.is_some() {
            return Err(PakError::InvalidConfiguration(
                "one request attempted more than one concurrent resource read".into(),
            ));
        }
        *reservation = Some(lease);
        Ok(Box::new(ReservationMarker))
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.stopped.load(Ordering::Acquire)
    }
}

struct ReservationMarker;
impl ReadReservation for ReservationMarker {}

struct MemoryBudget {
    used: AtomicU64,
    limit: u64,
}

impl MemoryBudget {
    fn reserve(self: &Arc<Self>, amount: u64) -> engine_pak::Result<BudgetLease> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(requested) = used.checked_add(amount) else {
                return Err(PakError::BudgetExceeded {
                    requested: u64::MAX,
                    limit: self.limit,
                });
            };
            if requested > self.limit {
                return Err(PakError::BudgetExceeded {
                    requested,
                    limit: self.limit,
                });
            }
            match self.used.compare_exchange_weak(
                used,
                requested,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(BudgetLease {
                        budget: Arc::clone(self),
                        amount,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

struct BudgetLease {
    budget: Arc<MemoryBudget>,
    amount: u64,
}

impl BudgetLease {
    fn shrink_to(&mut self, amount: u64) -> Result<(), ResourceError> {
        if amount > self.amount {
            return Err(ResourceError::WorkerPanicked);
        }
        let released = self.amount - amount;
        self.amount = amount;
        self.budget.used.fetch_sub(released, Ordering::AcqRel);
        Ok(())
    }
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.amount, Ordering::AcqRel);
    }
}

struct AdmissionGate {
    state: AtomicUsize,
    wait_lock: Mutex<()>,
    idle: Condvar,
}

const ADMISSION_GATE_CLOSED: usize = 1_usize << (usize::BITS - 1);
const ADMISSION_GATE_COUNT: usize = ADMISSION_GATE_CLOSED - 1;

impl AdmissionGate {
    fn try_enter(self: &Arc<Self>) -> Result<EnqueuePermit, ResourceError> {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & ADMISSION_GATE_CLOSED != 0 {
                return Err(ResourceError::Stopped);
            }
            if state & ADMISSION_GATE_COUNT == ADMISSION_GATE_COUNT {
                return Err(ResourceError::QueueFull);
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(EnqueuePermit {
                        gate: Arc::clone(self),
                    });
                }
                Err(current) => state = current,
            }
        }
    }

    fn close(&self) {
        self.state.fetch_or(ADMISSION_GATE_CLOSED, Ordering::AcqRel);
    }

    fn wait_idle(&self) {
        let mut lock = match self.wait_lock.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        while self.state.load(Ordering::Acquire) & ADMISSION_GATE_COUNT != 0 {
            lock = match self.idle.wait(lock) {
                Ok(lock) => lock,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }
}

struct EnqueuePermit {
    gate: Arc<AdmissionGate>,
}

impl Drop for EnqueuePermit {
    fn drop(&mut self) {
        let previous = self.gate.state.fetch_sub(1, Ordering::AcqRel);
        if previous & ADMISSION_GATE_CLOSED != 0 && previous & ADMISSION_GATE_COUNT == 1 {
            let _lock = match self.gate.wait_lock.lock() {
                Ok(lock) => lock,
                Err(poisoned) => poisoned.into_inner(),
            };
            self.gate.idle.notify_all();
        }
    }
}

struct Outstanding {
    used: AtomicUsize,
    max: usize,
}

impl Outstanding {
    fn try_acquire(self: &Arc<Self>) -> Result<AdmissionPermit, ResourceError> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if used >= self.max {
                return Err(ResourceError::QueueFull);
            }
            match self.used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(AdmissionPermit {
                        outstanding: Arc::clone(self),
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

struct AdmissionPermit {
    outstanding: Arc<Outstanding>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.outstanding.used.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
enum Backend {
    Packed(Arc<ArchiveCatalog>),
    Loose(PathBuf),
}

fn start_inner(
    config: ResourceConfig,
    backend_override: Option<Backend>,
) -> Result<ResourceSystem, ResourceError> {
    let config = anchor_config_root(config)?;
    validate_config(&config)?;
    let outstanding_max = config
        .queue_capacity
        .checked_add(config.worker_count)
        .ok_or_else(|| ResourceError::InvalidConfiguration("request capacity overflow".into()))?;
    let (jobs, job_rx) = flume::bounded(config.queue_capacity);
    let (stop_tx, stop_rx) = flume::bounded(1);
    let (ready_tx, ready_rx) = flume::bounded(1);
    let stopped = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(AdmissionGate {
        state: AtomicUsize::new(0),
        wait_lock: Mutex::new(()),
        idle: Condvar::new(),
    });
    let fatal_worker_panic = Arc::new(AtomicBool::new(false));
    let memory = Arc::new(MemoryBudget {
        used: AtomicU64::new(0),
        limit: config.max_memory_bytes,
    });

    let coordinator_stop = stop_tx.clone();
    let coordinator_stopped = Arc::clone(&stopped);
    let coordinator_fatal = Arc::clone(&fatal_worker_panic);
    let coordinator_gate = Arc::clone(&gate);
    let coordinator_jobs = job_rx.clone();
    let config_for_thread = config.clone();
    let backend_for_thread = backend_override;
    let coordinator = thread::Builder::new()
        .name("resource-coordinator".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                coordinate(
                    config_for_thread,
                    backend_for_thread,
                    coordinator_jobs,
                    stop_rx,
                    ready_tx,
                    CoordinatorControl {
                        manager_stop_tx: coordinator_stop.clone(),
                        stopped: coordinator_stopped.clone(),
                        fatal_worker_panic: coordinator_fatal.clone(),
                        gate: coordinator_gate.clone(),
                    },
                )
            }));
            if result.is_err() {
                coordinator_stopped.store(true, Ordering::Release);
                let _ = coordinator_stop.try_send(());
                coordinator_fatal.store(true, Ordering::Release);
            }
        })
        .map_err(|source| ResourceError::Io { path: None, source })?;

    match ready_rx.recv() {
        Ok(Ok(())) => {
            let handle = ResourceHandle {
                inner: Arc::new(HandleInner {
                    root: config.root,
                    jobs,
                    outstanding: Arc::new(Outstanding {
                        used: AtomicUsize::new(0),
                        max: outstanding_max,
                    }),
                    memory,
                    max_result_bytes: config.max_result_bytes,
                    stopped: Arc::clone(&stopped),
                    gate: Arc::clone(&gate),
                    fatal_worker_panic: Arc::clone(&fatal_worker_panic),
                    #[cfg(test)]
                    panic_next: AtomicBool::new(false),
                }),
            };
            Ok(ResourceSystem {
                handle,
                stop_tx,
                stopped,
                gate,
                fatal_worker_panic,
                coordinator: Some(coordinator),
            })
        }
        Ok(Err(error)) => {
            let _ = coordinator.join();
            Err(error)
        }
        Err(_) => {
            let _ = coordinator.join();
            Err(ResourceError::WorkerPanicked)
        }
    }
}

fn anchor_config_root(mut config: ResourceConfig) -> Result<ResourceConfig, ResourceError> {
    if !config.root.is_absolute() {
        let current_dir =
            std::env::current_dir().map_err(|source| ResourceError::Io { path: None, source })?;
        config.root = anchor_root(&config.root, &current_dir);
    }
    Ok(config)
}

fn anchor_root(root: &Path, current_dir: &Path) -> PathBuf {
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        current_dir.join(root)
    }
}

fn validate_config(config: &ResourceConfig) -> Result<(), ResourceError> {
    if config.worker_count == 0 {
        return Err(ResourceError::InvalidConfiguration(
            "worker_count must be greater than zero".into(),
        ));
    }
    if config.queue_capacity == 0 {
        return Err(ResourceError::InvalidConfiguration(
            "queue_capacity must be greater than zero".into(),
        ));
    }
    if config
        .queue_capacity
        .checked_add(config.worker_count)
        .is_none()
    {
        return Err(ResourceError::InvalidConfiguration(
            "request capacity overflow".into(),
        ));
    }
    Ok(())
}

struct CoordinatorControl {
    manager_stop_tx: Sender<()>,
    stopped: Arc<AtomicBool>,
    fatal_worker_panic: Arc<AtomicBool>,
    gate: Arc<AdmissionGate>,
}

fn coordinate(
    config: ResourceConfig,
    backend_override: Option<Backend>,
    jobs: Receiver<Job>,
    stop_rx: Receiver<()>,
    ready: Sender<Result<(), ResourceError>>,
    control: CoordinatorControl,
) {
    let CoordinatorControl {
        manager_stop_tx,
        stopped,
        fatal_worker_panic,
        gate,
    } = control;
    let backend_result = backend_override
        .map(Ok)
        .unwrap_or_else(|| open_backend(&config));
    let backend = match backend_result {
        Ok(backend) => backend,
        Err(error) => {
            let _ = ready.try_send(Err(error));
            return;
        }
    };

    let mut pool = WorkerPool {
        workers: Vec::with_capacity(config.worker_count),
        stop_senders: Vec::with_capacity(config.worker_count),
        jobs: jobs.clone(),
        stopped: Arc::clone(&stopped),
        fatal_worker_panic: Arc::clone(&fatal_worker_panic),
        gate: Arc::clone(&gate),
        joined: false,
    };
    for worker_index in 0..config.worker_count {
        let (worker_stop_tx, worker_stop_rx) = flume::bounded(1);
        let worker_jobs = jobs.clone();
        let worker_backend = backend.clone();
        let worker_stopped = Arc::clone(&stopped);
        let worker_fatal = Arc::clone(&fatal_worker_panic);
        let worker_gate = Arc::clone(&gate);
        let worker_manager_stop = manager_stop_tx.clone();
        let failure = WorkerFailureControl {
            manager_stop_tx: worker_manager_stop.clone(),
            stopped: Arc::clone(&worker_stopped),
            fatal_worker_panic: Arc::clone(&worker_fatal),
            gate: Arc::clone(&worker_gate),
        };
        let worker = thread::Builder::new()
            .name(format!("resource-worker-{worker_index}"))
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    worker_loop(worker_jobs, worker_stop_rx, worker_backend, &failure)
                }));
                if result.is_err() {
                    failure.signal();
                }
            });
        match worker {
            Ok(worker) => {
                pool.workers.push(worker);
                pool.stop_senders.push(worker_stop_tx);
            }
            Err(source) => {
                let _ = ready.try_send(Err(ResourceError::Io { path: None, source }));
                return;
            }
        }
    }

    if stopped.load(Ordering::Acquire) {
        let _ = ready.try_send(Err(ResourceError::WorkerPanicked));
        return;
    }
    let _ = ready.try_send(Ok(()));

    let _ = stop_rx.recv();
    drop(pool);
}

struct WorkerPool {
    workers: Vec<JoinHandle<()>>,
    stop_senders: Vec<Sender<()>>,
    jobs: Receiver<Job>,
    stopped: Arc<AtomicBool>,
    fatal_worker_panic: Arc<AtomicBool>,
    gate: Arc<AdmissionGate>,
    joined: bool,
}

impl WorkerPool {
    fn shutdown(&mut self) {
        if self.joined {
            return;
        }
        self.stopped.store(true, Ordering::Release);
        self.gate.close();
        self.gate.wait_idle();
        while let Ok(job) = self.jobs.try_recv() {
            job.respond_stopped();
        }
        for stop in &self.stop_senders {
            let _ = stop.try_send(());
        }
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                self.fatal_worker_panic.store(true, Ordering::Release);
            }
        }
        self.joined = true;
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_backend(config: &ResourceConfig) -> Result<Backend, ResourceError> {
    match &config.source_mode {
        SourceMode::Auto => open_auto_backend(config),
        SourceMode::Loose => Ok(Backend::Loose(config.root.clone())),
        SourceMode::Registry => open_registry_backend(config),
    }
}

fn open_auto_backend(config: &ResourceConfig) -> Result<Backend, ResourceError> {
    match discover_archives(&config.root) {
        Ok(discovered) if discovered.is_empty() => Ok(Backend::Loose(config.root.clone())),
        Ok(_) => {
            let archive_limits = archive_limits(config);
            let catalog_limits = catalog_limits(config);
            let catalog =
                ArchiveCatalog::open_directory(&config.root, archive_limits, catalog_limits)?;
            Ok(Backend::Packed(Arc::new(catalog)))
        }
        Err(PakError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(Backend::Loose(config.root.clone()))
        }
        Err(error) => Err(error.into()),
    }
}

fn open_registry_backend(config: &ResourceConfig) -> Result<Backend, ResourceError> {
    let base_path = config.root.join("base.rvepak");
    let metadata = match fs::symlink_metadata(&base_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ResourceError::MissingBaseArchive(format!(
                "base archive is missing: {}",
                base_path.display()
            )));
        }
        Err(source) => {
            return Err(ResourceError::Io {
                path: Some(base_path),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ResourceError::NonRegularFile(
            base_path.display().to_string(),
        ));
    }

    let archive_limits = archive_limits(config);
    let base_archive = Archive::open_file_with_limits(&base_path, archive_limits.clone())?;
    let registry_path = VirtualPath::parse("/rve/assets.toml")
        .expect("registry path is a valid compile-time virtual path");
    let registry_bytes = match base_archive.read_all(&registry_path) {
        Ok(bytes) => bytes,
        Err(PakError::NotFound(_)) => {
            return Err(ResourceError::MissingRegistry(
                "base archive does not contain /rve/assets.toml".into(),
            ));
        }
        Err(PakError::MissingOwner(_)) => {
            return Err(ResourceError::InvalidRegistry(
                "/rve/assets.toml must be stored locally in base.rvepak".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    drop(base_archive);
    let registry_source = String::from_utf8(registry_bytes).map_err(|error| {
        ResourceError::InvalidRegistry(format!("/rve/assets.toml is not valid UTF-8: {error}"))
    })?;
    let registry = ArchiveSetRegistry::parse(&registry_source)
        .map_err(|error| ResourceError::InvalidRegistry(error.to_string()))?;
    registry
        .validate_archive_files(&config.root)
        .map_err(|error| ResourceError::ArchiveSetMismatch(error.to_string()))?;

    let catalog =
        ArchiveCatalog::open_directory(&config.root, archive_limits, catalog_limits(config))?;
    let archive_ids = catalog
        .archive_paths()
        .map(|(id, path)| {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| {
                    ResourceError::ArchiveSetMismatch(format!(
                        "archive filename is not valid UTF-8: {}",
                        path.display()
                    ))
                })?;
            Ok((name.to_owned(), id))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, ResourceError>>()?;
    registry
        .validate_against_catalog(&catalog, &archive_ids, &config.root)
        .map_err(|error| ResourceError::ArchiveSetMismatch(error.to_string()))?;
    Ok(Backend::Packed(Arc::new(catalog)))
}

fn archive_limits(config: &ResourceConfig) -> ArchiveLimits {
    ArchiveLimits {
        max_read_result_bytes: config.max_result_bytes,
        max_read_memory_bytes: config.max_memory_bytes,
        ..ArchiveLimits::default()
    }
}

fn catalog_limits(config: &ResourceConfig) -> CatalogLimits {
    CatalogLimits {
        max_total_index_bytes: config.max_total_index_bytes,
        ..CatalogLimits::default()
    }
}

enum Selection {
    Job(Result<Job, flume::RecvError>),
    Stop,
}

fn worker_loop(
    jobs: Receiver<Job>,
    stop: Receiver<()>,
    backend: Backend,
    failure: &WorkerFailureControl,
) {
    loop {
        let selected = Selector::new()
            .recv(&jobs, Selection::Job)
            .recv(&stop, |_| Selection::Stop)
            .wait();
        let job = match selected {
            Selection::Stop | Selection::Job(Err(_)) => break,
            Selection::Job(Ok(job)) => job,
        };
        let _panic_guard = WorkerPanicGuard { failure };
        if failure.stopped.load(Ordering::Acquire) {
            job.respond_stopped();
            continue;
        }
        if job.control.cancelled.load(Ordering::Acquire) {
            job.respond(Err(ResourceError::Cancelled), None);
            continue;
        }
        #[cfg(test)]
        if job.panic_for_test {
            panic!("injected resource worker panic");
        }
        let result = catch_unwind(AssertUnwindSafe(|| execute_job(&job, &backend)));
        match result {
            Ok((Ok(value), memory)) => job.respond(Ok(value), memory),
            Ok((Err(error), _)) => {
                job.control.release_reservation();
                job.respond(Err(error), None);
            }
            Err(_) => {
                job.control.release_reservation();
                job.respond(Err(ResourceError::WorkerPanicked), None);
            }
        }
    }
}

struct WorkerFailureControl {
    manager_stop_tx: Sender<()>,
    stopped: Arc<AtomicBool>,
    fatal_worker_panic: Arc<AtomicBool>,
    gate: Arc<AdmissionGate>,
}

impl WorkerFailureControl {
    fn signal(&self) {
        self.stopped.store(true, Ordering::Release);
        self.fatal_worker_panic.store(true, Ordering::Release);
        self.gate.close();
        let _ = self.manager_stop_tx.try_send(());
    }
}

struct WorkerPanicGuard<'a> {
    failure: &'a WorkerFailureControl,
}

impl Drop for WorkerPanicGuard<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.failure.signal();
        }
    }
}

fn execute_job(
    job: &Job,
    backend: &Backend,
) -> (Result<ResponseValue, ResourceError>, Option<BudgetLease>) {
    if let Some(error) = job.control.error_if_cancelled() {
        return (Err(error), None);
    }
    match &job.operation {
        Operation::Stat => (backend.stat(&job.path).map(ResponseValue::Stat), None),
        Operation::ReadAll => {
            let bytes = backend.read_all(&job.path, &job.control);
            finish_bytes(bytes, &job.control)
        }
        Operation::ReadRange(range) => {
            let bytes = backend.read_range(&job.path, range.clone(), &job.control);
            finish_bytes(bytes, &job.control)
        }
    }
}

fn finish_bytes(
    bytes: Result<Vec<u8>, ResourceError>,
    control: &JobControl,
) -> (Result<ResponseValue, ResourceError>, Option<BudgetLease>) {
    match bytes {
        Ok(bytes) => {
            if let Some(error) = control.error_if_cancelled() {
                control.release_reservation();
                return (Err(error), None);
            }
            let result_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            match control.take_result_lease(result_bytes) {
                Ok(memory) => (Ok(ResponseValue::Bytes(bytes)), memory),
                Err(error) => (Err(error), None),
            }
        }
        Err(error) => {
            if let Some(cancel_error) = control.error_if_cancelled() {
                (Err(cancel_error), None)
            } else {
                (Err(error), None)
            }
        }
    }
}

impl Backend {
    fn stat(&self, path: &VirtualPath) -> Result<ResourceStat, ResourceError> {
        match self {
            Self::Packed(catalog) => Ok(ResourceStat {
                size: catalog.stat(path)?.size,
            }),
            Self::Loose(root) => Ok(ResourceStat {
                size: loose_metadata(root, path)?.len(),
            }),
        }
    }

    fn read_all(&self, path: &VirtualPath, control: &JobControl) -> Result<Vec<u8>, ResourceError> {
        match self {
            Self::Packed(catalog) => {
                let size = catalog.stat(path)?.size;
                catalog
                    .read_range_with_control(path, 0..size, control)
                    .map_err(Into::into)
            }
            Self::Loose(root) => loose_read(root, path, None, control),
        }
    }

    fn read_range(
        &self,
        path: &VirtualPath,
        range: Range<u64>,
        control: &JobControl,
    ) -> Result<Vec<u8>, ResourceError> {
        match self {
            Self::Packed(catalog) => catalog
                .read_range_with_control(path, range, control)
                .map_err(Into::into),
            Self::Loose(root) => loose_read(root, path, Some(range), control),
        }
    }
}

fn loose_metadata(root: &Path, path: &VirtualPath) -> Result<fs::Metadata, ResourceError> {
    let canonical_file = canonical_loose_path(root, path)?;
    let metadata = fs::metadata(&canonical_file).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ResourceError::NotFound(path.to_string())
        } else {
            ResourceError::Io {
                path: Some(canonical_file.clone()),
                source,
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(ResourceError::NonRegularFile(path.to_string()));
    }
    Ok(metadata)
}

fn canonical_loose_path(root: &Path, path: &VirtualPath) -> Result<PathBuf, ResourceError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ResourceError::NotFound(root.display().to_string())
        } else {
            ResourceError::Io {
                path: Some(root.to_path_buf()),
                source,
            }
        }
    })?;
    let joined = canonical_root.join(path.as_str());
    let canonical_file = fs::canonicalize(&joined).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ResourceError::NotFound(path.to_string())
        } else {
            ResourceError::Io {
                path: Some(joined),
                source,
            }
        }
    })?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err(ResourceError::PathOutsideRoot(path.to_string()));
    }
    Ok(canonical_file)
}

fn loose_read(
    root: &Path,
    path: &VirtualPath,
    requested_range: Option<Range<u64>>,
    control: &JobControl,
) -> Result<Vec<u8>, ResourceError> {
    if let Some(error) = control.error_if_cancelled() {
        return Err(error);
    }
    let canonical_file = canonical_loose_path(root, path)?;
    let metadata = fs::metadata(&canonical_file).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ResourceError::NotFound(path.to_string())
        } else {
            ResourceError::Io {
                path: Some(canonical_file.clone()),
                source,
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(ResourceError::NonRegularFile(path.to_string()));
    }
    let file = File::open(&canonical_file).map_err(|source| ResourceError::Io {
        path: Some(canonical_file.clone()),
        source,
    })?;
    let opened_metadata = file.metadata().map_err(|source| ResourceError::Io {
        path: Some(canonical_file.clone()),
        source,
    })?;
    if !opened_metadata.is_file() {
        return Err(ResourceError::NonRegularFile(path.to_string()));
    }
    let size = opened_metadata.len();
    let range = requested_range.unwrap_or(0..size);
    validate_range(&range, size)?;
    let result_bytes = range.end - range.start;
    if result_bytes > control.max_result_bytes {
        return Err(ResourceError::BudgetExceeded {
            requested: result_bytes,
            limit: control.max_result_bytes,
        });
    }
    control.reserve(MemoryFootprint {
        result_bytes,
        ..MemoryFootprint::default()
    })?;
    let result_len = usize::try_from(result_bytes).map_err(|_| ResourceError::BudgetExceeded {
        requested: result_bytes,
        limit: control.max_result_bytes,
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(result_len)
        .map_err(|_| ResourceError::BudgetExceeded {
            requested: result_bytes,
            limit: control.max_result_bytes,
        })?;
    bytes.resize(result_len, 0);
    if !bytes.is_empty() {
        let source = FileSource::from_file(file);
        let mut offset = range.start;
        for slice in bytes.chunks_mut(256 * 1024) {
            if let Some(error) = control.error_if_cancelled() {
                return Err(error);
            }
            source
                .read_exact_at(offset, slice)
                .map_err(|source| ResourceError::Io {
                    path: Some(canonical_file.clone()),
                    source,
                })?;
            let slice_len =
                u64::try_from(slice.len()).map_err(|_| ResourceError::BudgetExceeded {
                    requested: result_bytes,
                    limit: control.max_result_bytes,
                })?;
            offset = offset
                .checked_add(slice_len)
                .ok_or(ResourceError::InvalidRange {
                    start: range.start,
                    end: range.end,
                    size,
                })?;
        }
    }
    Ok(bytes)
}

fn validate_range(range: &Range<u64>, size: u64) -> Result<(), ResourceError> {
    if range.start > range.end || range.end > size {
        return Err(ResourceError::InvalidRange {
            start: range.start,
            end: range.end,
            size,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
fn start_with_backend_for_test(
    config: ResourceConfig,
    backend: Backend,
) -> Result<ResourceSystem, ResourceError> {
    start_inner(config, Some(backend))
}
