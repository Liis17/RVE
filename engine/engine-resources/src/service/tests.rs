use super::*;
use engine_files::ReadAt;
use engine_pak::{
    Archive, ArchiveCatalog, ArchiveId, ArchiveWriter, BlobId, BlobOptions, CatalogLimits, Codec,
    HEADER_SIZE, WriterOptions,
};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rve-resources-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(root: impl Into<PathBuf>) -> ResourceConfig {
    ResourceConfig {
        root: root.into(),
        worker_count: 1,
        queue_capacity: 2,
        max_total_index_bytes: 1024 * 1024,
        max_result_bytes: 1024 * 1024,
        max_memory_bytes: 512 * 1024 * 1024,
    }
}

fn virtual_path(path: &str) -> VirtualPath {
    VirtualPath::parse(path).unwrap()
}

fn archive_bytes(codec: Codec, chunk_size: u64, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(
        io::Cursor::new(Vec::new()),
        WriterOptions {
            codec,
            chunk_size,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    for (path, bytes) in files {
        writer
            .add_local_with_options(
                virtual_path(path),
                *bytes,
                BlobOptions {
                    codec,
                    chunk_size,
                    zstd_level: 3,
                },
                None,
            )
            .unwrap();
    }
    writer.finish().unwrap().into_inner().into_inner()
}

fn loose_system(root: &Path) -> ResourceSystem {
    ResourceSystem::start(config(root)).unwrap()
}

fn start_error(root: &Path) -> ResourceError {
    match ResourceSystem::start(config(root)) {
        Err(error) => error,
        Ok(mut system) => {
            let _ = system.shutdown();
            panic!("expected resource startup to fail");
        }
    }
}

#[test]
fn chooses_packed_only_when_archives_exist_and_allows_missing_loose_root() {
    let packed_root = TempDirectory::new();
    fs::write(
        packed_root.path().join("assets.RVEPAK"),
        archive_bytes(Codec::None, 4, &[("same.bin", b"packed")]),
    )
    .unwrap();
    fs::write(packed_root.path().join("same.bin"), b"loose").unwrap();
    let system = loose_system(packed_root.path());
    let handle = system.handle();
    assert_eq!(
        handle.read_all("same.bin").unwrap().wait().unwrap(),
        b"packed"
    );
    drop(system);

    let loose_root = TempDirectory::new();
    fs::write(loose_root.path().join("loose.data"), [0, 255, 7]).unwrap();
    let system = loose_system(loose_root.path());
    let handle = system.handle();
    assert_eq!(
        handle.read_all("loose.data").unwrap().wait().unwrap(),
        [0, 255, 7]
    );
    drop(system);

    let missing_root = TempDirectory::new().path().join("does-not-exist");
    let system = loose_system(&missing_root);
    let error = system.handle().stat("missing").unwrap().wait().unwrap_err();
    assert_eq!(error.kind(), crate::ResourceErrorKind::NotFound);
    drop(system);
}

#[test]
fn corrupt_missing_owner_and_duplicate_namespace_archives_fail_startup() {
    let corrupt_root = TempDirectory::new();
    fs::write(corrupt_root.path().join("broken.rvepak"), b"not an archive").unwrap();
    fs::write(
        corrupt_root.path().join("fallback.bin"),
        b"must not be used",
    )
    .unwrap();
    let error = start_error(corrupt_root.path());
    assert_eq!(error.kind(), crate::ResourceErrorKind::Corrupt);

    let not_directory_parent = TempDirectory::new();
    let not_directory = not_directory_parent.path().join("not-a-directory");
    fs::write(&not_directory, b"file").unwrap();
    let error = start_error(&not_directory);
    assert_eq!(error.kind(), crate::ResourceErrorKind::Io);

    let owner_root = TempDirectory::new();
    let mut writer =
        ArchiveWriter::new(io::Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    writer
        .add_external(
            virtual_path("needs-owner.bin"),
            ArchiveId::from_bytes([91; 32]),
            BlobId::from_u64(7),
        )
        .unwrap();
    fs::write(
        owner_root.path().join("reference.rvepak"),
        writer.finish().unwrap().into_inner().into_inner(),
    )
    .unwrap();
    let error = start_error(owner_root.path());
    assert_eq!(error.kind(), crate::ResourceErrorKind::MissingOwner);

    let duplicate_root = TempDirectory::new();
    fs::write(
        duplicate_root.path().join("one.rvepak"),
        archive_bytes(Codec::None, 4, &[("duplicate/path", b"one")]),
    )
    .unwrap();
    fs::write(
        duplicate_root.path().join("two.rvepak"),
        archive_bytes(Codec::None, 4, &[("duplicate/path", b"two")]),
    )
    .unwrap();
    let error = start_error(duplicate_root.path());
    assert_eq!(error.kind(), crate::ResourceErrorKind::DuplicatePath);

    let repeated_root = TempDirectory::new();
    let repeated = archive_bytes(Codec::None, 4, &[("one", b"data")]);
    fs::write(repeated_root.path().join("one.rvepak"), &repeated).unwrap();
    fs::write(repeated_root.path().join("two.rvepak"), repeated).unwrap();
    let error = start_error(repeated_root.path());
    assert_eq!(error.kind(), crate::ResourceErrorKind::DuplicateArchiveId);
}

#[test]
fn packed_external_reference_reads_from_its_validated_owner() {
    let root = TempDirectory::new();
    let mut owner_writer = ArchiveWriter::new(
        io::Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            chunk_size: 4,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    let blob = owner_writer
        .add_local(virtual_path("owner.bin"), b"owned bytes".as_slice(), None)
        .unwrap();
    let owner = owner_writer.finish().unwrap();
    let owner_id = owner.archive_id;
    fs::write(
        root.path().join("01-owner.rvepak"),
        owner.into_inner().into_inner(),
    )
    .unwrap();

    let mut references =
        ArchiveWriter::new(io::Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    references
        .add_external(virtual_path("alias.bin"), owner_id, blob)
        .unwrap();
    fs::write(
        root.path().join("02-reference.rvepak"),
        references.finish().unwrap().into_inner().into_inner(),
    )
    .unwrap();

    let system = loose_system(root.path());
    let handle = system.handle();
    assert_eq!(handle.stat("alias.bin").unwrap().wait().unwrap().size, 11);
    assert_eq!(
        handle
            .read_range("alias.bin", 2..8)
            .unwrap()
            .wait()
            .unwrap(),
        b"ned by"
    );
}

#[test]
fn packed_stat_ranges_and_all_codecs_preserve_decoded_bytes() {
    let root = TempDirectory::new();
    for (index, codec) in [Codec::None, Codec::Zstd, Codec::Lz4]
        .into_iter()
        .enumerate()
    {
        let archive = archive_bytes(codec, 4, &[("opaque.bin", &[0, 255, 1, 2, 3, 4, 5, 0, 9])]);
        let archive_path = root.path().join(format!("{index}.rvepak"));
        fs::write(&archive_path, archive).unwrap();
        let system = loose_system(root.path());
        let handle = system.handle();
        assert_eq!(handle.stat("opaque.bin").unwrap().wait().unwrap().size, 9);
        assert_eq!(
            handle.read_all("opaque.bin").unwrap().wait().unwrap(),
            [0, 255, 1, 2, 3, 4, 5, 0, 9]
        );
        assert_eq!(
            handle
                .read_range("opaque.bin", 2..7)
                .unwrap()
                .wait()
                .unwrap(),
            [1, 2, 3, 4, 5]
        );
        assert!(
            handle
                .read_range("opaque.bin", 9..9)
                .unwrap()
                .wait()
                .unwrap()
                .is_empty()
        );
        let error = handle
            .read_range("opaque.bin", 0..10)
            .unwrap()
            .wait()
            .unwrap_err();
        assert_eq!(error.kind(), crate::ResourceErrorKind::InvalidRange);
        drop(system);
        fs::remove_file(archive_path).unwrap();
    }

    let archive_root = TempDirectory::new();
    fs::write(
        archive_root.path().join("empty.rvepak"),
        archive_bytes(Codec::Zstd, 4, &[("empty.bin", &[])]),
    )
    .unwrap();
    let system = loose_system(archive_root.path());
    assert_eq!(
        system
            .handle()
            .stat("empty.bin")
            .unwrap()
            .wait()
            .unwrap()
            .size,
        0
    );
    assert!(
        system
            .handle()
            .read_all("empty.bin")
            .unwrap()
            .wait()
            .unwrap()
            .is_empty()
    );
}

fn ready_request() -> ResourceRequest<Vec<u8>> {
    let budget = Arc::new(MemoryBudget {
        used: AtomicU64::new(0),
        limit: 1,
    });
    let stopped = Arc::new(AtomicBool::new(false));
    let control = Arc::new(JobControl {
        cancelled: AtomicBool::new(false),
        stopped: Arc::clone(&stopped),
        budget,
        max_result_bytes: 1,
        reservation: Mutex::new(None),
    });
    let (response_tx, response_rx) = flume::bounded(1);
    response_tx
        .try_send(Response {
            result: Ok(ResponseValue::Bytes(vec![b'o', b'k'])),
            memory: None,
        })
        .unwrap();
    ResourceRequest {
        response: response_rx,
        admission: None,
        control,
        fatal_worker_panic: Arc::new(AtomicBool::new(false)),
        stopped,
        decode: decode_bytes,
        completed: false,
    }
}

#[test]
fn wait_reports_request_completed_after_try_receive() {
    let mut request = ready_request();
    assert_eq!(request.try_receive().unwrap().unwrap(), b"ok");
    assert_eq!(
        request.wait().unwrap_err().kind(),
        crate::ResourceErrorKind::RequestCompleted
    );
}

#[test]
fn receive_reports_request_completed_after_try_receive() {
    let mut request = ready_request();
    assert_eq!(request.try_receive().unwrap().unwrap(), b"ok");
    let mut future = Box::pin(request.receive());
    let waker = Waker::from(Arc::new(WakeState::default()));
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::as_mut(&mut future).poll(&mut context),
        Poll::Ready(Err(ResourceError::RequestCompleted))
    ));
}

#[cfg(unix)]
#[test]
fn loose_symlinks_cannot_escape_root_for_stat_or_reads() {
    use std::os::unix::fs::symlink;

    let root = TempDirectory::new();
    let outside = TempDirectory::new();
    fs::write(outside.path().join("secret.bin"), b"outside data").unwrap();
    symlink(
        outside.path().join("secret.bin"),
        root.path().join("external.bin"),
    )
    .unwrap();
    let system = loose_system(root.path());
    let handle = system.handle();
    assert_eq!(
        handle
            .stat("external.bin")
            .unwrap()
            .wait()
            .unwrap_err()
            .kind(),
        crate::ResourceErrorKind::PathOutsideRoot
    );
    assert_eq!(
        handle
            .read_all("external.bin")
            .unwrap()
            .wait()
            .unwrap_err()
            .kind(),
        crate::ResourceErrorKind::PathOutsideRoot
    );
    assert_eq!(
        handle
            .read_range("external.bin", 0..3)
            .unwrap()
            .wait()
            .unwrap_err()
            .kind(),
        crate::ResourceErrorKind::PathOutsideRoot
    );
    fs::create_dir(root.path().join("directory")).unwrap();
    assert_eq!(
        handle
            .read_all("directory")
            .unwrap()
            .wait()
            .unwrap_err()
            .kind(),
        crate::ResourceErrorKind::NonRegularFile
    );
}

#[test]
fn relative_root_is_anchored_against_the_start_directory() {
    let cwd = Path::new("/captured/working/directory");
    assert_eq!(anchor_root(Path::new("assets"), cwd), cwd.join("assets"));
    assert_eq!(
        anchor_root(Path::new("/fixed/assets"), cwd),
        PathBuf::from("/fixed/assets")
    );
}

#[derive(Default)]
struct GateState {
    enabled: bool,
    released: bool,
    arrivals: usize,
}

#[derive(Default)]
struct ReadGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl ReadGate {
    fn enable(&self) {
        let mut state = self.state.lock().unwrap();
        state.enabled = true;
        state.released = false;
    }

    fn enter_and_wait(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.enabled || state.released {
            return;
        }
        state.arrivals += 1;
        self.changed.notify_all();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.released {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                state.released = true;
                break;
            };
            let (next, timeout) = self.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if timeout.timed_out() {
                state.released = true;
                self.changed.notify_all();
                break;
            }
        }
    }

    fn wait_for_arrivals(&self, count: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        while state.arrivals < count {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, result) = self.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if result.timed_out() && state.arrivals < count {
                return false;
            }
        }
        true
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.released = true;
        self.changed.notify_all();
    }
}

struct RecordingReader {
    bytes: Vec<u8>,
    payload_start: u64,
    index_start: u64,
    payload_reads: AtomicUsize,
    gate: Arc<ReadGate>,
}

impl ReadAt for RecordingReader {
    fn len(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let end = offset.saturating_add(buffer.len() as u64);
        if offset < self.index_start && end > self.payload_start {
            self.payload_reads.fetch_add(1, Ordering::AcqRel);
            self.gate.enter_and_wait();
        }
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        if start >= self.bytes.len() {
            return Ok(0);
        }
        let count = buffer.len().min(self.bytes.len() - start);
        buffer[..count].copy_from_slice(&self.bytes[start..start + count]);
        Ok(count)
    }
}

struct MissingPayloadReader {
    bytes: Vec<u8>,
    index_start: u64,
}

impl ReadAt for MissingPayloadReader {
    fn len(&self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        if (HEADER_SIZE..self.index_start).contains(&offset) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "injected missing archive backing payload",
            ));
        }
        let start = usize::try_from(offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset too large"))?;
        if start >= self.bytes.len() {
            return Ok(0);
        }
        let count = buffer.len().min(self.bytes.len() - start);
        buffer[..count].copy_from_slice(&self.bytes[start..start + count]);
        Ok(count)
    }
}

fn catalog_with_reader(
    bytes: Vec<u8>,
    gate: Arc<ReadGate>,
) -> (Arc<ArchiveCatalog>, Arc<RecordingReader>) {
    let index_start = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let reader = Arc::new(RecordingReader {
        bytes,
        payload_start: engine_pak::HEADER_SIZE,
        index_start,
        payload_reads: AtomicUsize::new(0),
        gate,
    });
    let archive = Archive::open_shared(reader.clone()).unwrap();
    let mut catalog = ArchiveCatalog::new(CatalogLimits::default());
    catalog.insert(archive).unwrap();
    catalog.validate().unwrap();
    (Arc::new(catalog), reader)
}

#[test]
fn backing_io_not_found_is_distinct_from_a_missing_virtual_resource() {
    let root = TempDirectory::new();
    let bytes = archive_bytes(Codec::None, 4, &[("exists.bin", b"data")]);
    let index_start = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let reader = Arc::new(MissingPayloadReader { bytes, index_start });
    let archive = Archive::open_shared(reader).unwrap();
    let mut catalog = ArchiveCatalog::new(CatalogLimits::default());
    catalog.insert(archive).unwrap();
    catalog.validate().unwrap();
    let system = injected_system(root.path(), Backend::Packed(Arc::new(catalog)), 1, 2, 1024);
    let handle = system.handle();

    assert_eq!(
        handle
            .stat("missing.bin")
            .unwrap()
            .wait()
            .unwrap_err()
            .kind(),
        crate::ResourceErrorKind::NotFound
    );
    let error = handle.read_all("exists.bin").unwrap().wait().unwrap_err();
    assert_eq!(error.kind(), crate::ResourceErrorKind::Io);
    match error {
        ResourceError::Io { source, .. } => assert_eq!(source.kind(), io::ErrorKind::NotFound),
        other => panic!("expected the original backing I/O error, got {other:?}"),
    }
}

fn injected_system(
    root: &Path,
    backend: Backend,
    workers: usize,
    queue_capacity: usize,
    max_memory_bytes: u64,
) -> ResourceSystem {
    let mut settings = config(root);
    settings.worker_count = workers;
    settings.queue_capacity = queue_capacity;
    settings.max_memory_bytes = max_memory_bytes;
    start_with_backend_for_test(settings, backend).unwrap()
}

#[test]
fn overlapping_reads_on_one_archive_run_concurrently() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, reader) = catalog_with_reader(
        archive_bytes(Codec::None, 4, &[("item.bin", b"abcdefghijklmnop")]),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 3, 3, 1024);
    gate.enable();
    let handle = system.handle();
    let first = handle.read_range("item.bin", 0..8).unwrap();
    let second = handle.read_range("item.bin", 4..12).unwrap();
    let disjoint = handle.read_range("item.bin", 12..16).unwrap();
    let concurrent = gate.wait_for_arrivals(3, Duration::from_secs(2));
    gate.release();
    assert!(
        concurrent,
        "all three ranges should reach the backing reader together"
    );
    assert_eq!(first.wait().unwrap(), b"abcdefgh");
    assert_eq!(second.wait().unwrap(), b"efghijkl");
    assert_eq!(disjoint.wait().unwrap(), b"mnop");
    assert!(reader.payload_reads.load(Ordering::Acquire) >= 3);
}

#[test]
fn bounded_queue_reports_full_while_worker_is_busy() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, _) = catalog_with_reader(
        archive_bytes(Codec::None, 4, &[("item.bin", b"abcdefgh")]),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 1, 1, 1024);
    gate.enable();
    let handle = system.handle();
    let first = handle.read_range("item.bin", 0..4).unwrap();
    let entered = gate.wait_for_arrivals(1, Duration::from_secs(2));
    let queued = handle.read_range("item.bin", 4..8).unwrap();
    let full = matches!(
        handle.read_range("item.bin", 0..4),
        Err(ResourceError::QueueFull)
    );
    gate.release();
    assert!(entered, "worker should enter the test reader");
    assert!(full, "the one-slot queue should reject a third request");
    assert_eq!(first.wait().unwrap(), b"abcd");
    assert_eq!(queued.wait().unwrap(), b"efgh");
}

#[test]
fn unreceived_results_hold_the_memory_reservation_until_drop() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, _) = catalog_with_reader(
        archive_bytes(Codec::None, 4, &[("item.bin", b"abcdefgh")]),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 1, 2, 8);
    let handle = system.handle();
    let first_request = handle.read_range("item.bin", 0..4).unwrap();
    let first_response = first_request
        .response
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 4);
    let second_error = handle
        .read_range("item.bin", 0..4)
        .unwrap()
        .wait()
        .unwrap_err();
    assert_eq!(
        second_error.kind(),
        crate::ResourceErrorKind::BudgetExceeded
    );
    drop(first_response);
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 0);
    assert_eq!(
        handle.read_range("item.bin", 0..4).unwrap().wait().unwrap(),
        b"abcd"
    );
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 0);
}

#[test]
fn memory_budget_rejects_checked_add_overflow_at_maximum_limit() {
    let budget = Arc::new(MemoryBudget {
        used: AtomicU64::new(u64::MAX - 1),
        limit: u64::MAX,
    });
    assert!(matches!(
        budget.reserve(2),
        Err(PakError::BudgetExceeded {
            requested: u64::MAX,
            limit: u64::MAX
        })
    ));
    assert_eq!(budget.used.load(Ordering::Acquire), u64::MAX - 1);
}

#[test]
fn shutdown_preserves_a_completed_unreceived_result() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, _) = catalog_with_reader(
        archive_bytes(Codec::None, 4, &[("item.bin", b"abcdefgh")]),
        Arc::clone(&gate),
    );
    let mut system = injected_system(root.path(), Backend::Packed(catalog), 1, 2, 8);
    let handle = system.handle();
    let completed = handle.read_range("item.bin", 0..4).unwrap();
    let error = handle
        .read_range("item.bin", 0..4)
        .unwrap()
        .wait()
        .unwrap_err();
    assert_eq!(error.kind(), crate::ResourceErrorKind::BudgetExceeded);
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 4);
    assert!(system.shutdown().is_ok());
    assert_eq!(completed.wait().unwrap(), b"abcd");
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 0);
}

#[test]
fn a_small_range_fails_before_reading_an_oversized_chunk() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, reader) = catalog_with_reader(
        archive_bytes(Codec::Lz4, 64, &[("large-chunk.bin", &[42; 64])]),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 1, 2, 16);
    let error = system
        .handle()
        .read_range("large-chunk.bin", 0..1)
        .unwrap()
        .wait()
        .unwrap_err();
    assert_eq!(error.kind(), crate::ResourceErrorKind::BudgetExceeded);
    assert_eq!(reader.payload_reads.load(Ordering::Acquire), 0);
}

#[test]
fn dropping_active_and_queued_requests_cancels_between_chunks() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, reader) = catalog_with_reader(
        archive_bytes(
            Codec::None,
            4,
            &[("first.bin", b"abcdefgh"), ("second.bin", b"second")],
        ),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 1, 2, 1024);
    gate.enable();
    let handle = system.handle();
    let active = handle.read_all("first.bin").unwrap();
    let entered = gate.wait_for_arrivals(1, Duration::from_secs(2));
    let queued = handle.read_all("second.bin").unwrap();
    let reserved = handle.inner.memory.used.load(Ordering::Acquire);
    drop(active);
    drop(queued);
    gate.release();
    assert!(entered, "first read should reach the test reader");
    assert_eq!(
        reserved, 12,
        "active read holds its result and scratch budget"
    );
    assert_eq!(handle.stat("first.bin").unwrap().wait().unwrap().size, 8);
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 0);
    assert_eq!(reader.payload_reads.load(Ordering::Acquire), 1);
}

#[derive(Default)]
struct WakeState {
    notified: Mutex<bool>,
    changed: Condvar,
}

impl Wake for WakeState {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        *self.notified.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

impl WakeState {
    fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut notified = self.notified.lock().unwrap();
        while !*notified {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (next, result) = self.changed.wait_timeout(notified, remaining).unwrap();
            notified = next;
            if result.timed_out() && !*notified {
                return false;
            }
        }
        true
    }
}

#[test]
fn receive_future_is_woken_without_a_runtime() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, _) = catalog_with_reader(
        archive_bytes(Codec::None, 4, &[("async.bin", b"async")]),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 1, 1, 1024);
    gate.enable();
    let mut future = Box::pin(system.handle().read_all("async.bin").unwrap().receive());
    let wake_state = Arc::new(WakeState::default());
    let waker = Waker::from(Arc::clone(&wake_state));
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::as_mut(&mut future).poll(&mut context),
        Poll::Pending
    ));
    let entered = gate.wait_for_arrivals(1, Duration::from_secs(2));
    gate.release();
    assert!(
        entered,
        "read should wait at the backing reader after first poll"
    );
    assert!(
        wake_state.wait(Duration::from_secs(2)),
        "response should wake the future"
    );
    match Pin::as_mut(&mut future).poll(&mut context) {
        Poll::Ready(Ok(bytes)) => assert_eq!(bytes, b"async"),
        Poll::Ready(Err(error)) => panic!("async read failed: {error}"),
        Poll::Pending => panic!("woken future should be ready"),
    }
}

#[test]
fn shutdown_wakes_pending_work_rejects_retained_handles_and_joins_workers() {
    let root = TempDirectory::new();
    let gate = Arc::new(ReadGate::default());
    let (catalog, _) = catalog_with_reader(
        archive_bytes(
            Codec::None,
            4,
            &[("active.bin", b"active"), ("queued.bin", b"queued")],
        ),
        Arc::clone(&gate),
    );
    let system = injected_system(root.path(), Backend::Packed(catalog), 1, 1, 1024);
    gate.enable();
    let handle = system.handle();
    let mut active = Box::pin(handle.read_all("active.bin").unwrap().receive());
    let wake_state = Arc::new(WakeState::default());
    let waker = Waker::from(Arc::clone(&wake_state));
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::as_mut(&mut active).poll(&mut context),
        Poll::Pending
    ));
    let entered = gate.wait_for_arrivals(1, Duration::from_secs(2));
    let queued = handle.read_all("queued.bin").unwrap();
    let full = matches!(handle.read_all("active.bin"), Err(ResourceError::QueueFull));
    let shutdown = std::thread::spawn(move || {
        let mut system = system;
        system.shutdown()
    });
    let queued_response = queued.response.recv_timeout(Duration::from_secs(2));
    let stopped = match handle.stat("active.bin") {
        Err(error) => error.kind() == crate::ResourceErrorKind::Stopped,
        Ok(request) => {
            drop(request);
            false
        }
    };
    gate.release();
    assert!(entered, "active read should be inside the reader");
    assert!(full, "bounded queue should be at capacity");
    let queued_response = queued_response.unwrap();
    assert!(matches!(
        queued_response.result,
        Err(ResourceError::Stopped)
    ));
    assert!(
        stopped,
        "retained handles should reject work once shutdown begins"
    );
    assert!(shutdown.join().unwrap().is_ok());
    assert!(matches!(
        Pin::as_mut(&mut active).poll(&mut context),
        Poll::Ready(Err(ResourceError::Stopped))
    ));
    assert_eq!(
        queued.wait().unwrap_err().kind(),
        crate::ResourceErrorKind::Stopped
    );
}

#[test]
fn injected_worker_thread_panic_completes_the_request_and_releases_memory() {
    let root = TempDirectory::new();
    let mut system = loose_system(root.path());
    let handle = system.handle();
    handle.panic_on_next_worker_job();
    let error = handle.stat("anything").unwrap().wait().unwrap_err();
    assert_eq!(error.kind(), crate::ResourceErrorKind::WorkerPanicked);
    assert_eq!(handle.inner.memory.used.load(Ordering::Acquire), 0);
    let stopped_kind = match handle.stat("anything") {
        Err(error) => error.kind(),
        Ok(request) => {
            drop(request);
            panic!("retained handle should reject after worker panic");
        }
    };
    assert_eq!(stopped_kind, crate::ResourceErrorKind::Stopped);
    assert!(matches!(
        system.shutdown(),
        Err(ResourceError::WorkerPanicked)
    ));
}

#[test]
fn resource_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ResourceHandle>();
}
