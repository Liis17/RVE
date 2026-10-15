use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::OpenOptionsExt,
    os::unix::net::{UnixListener, UnixStream},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const RETRY_INTERVAL: Duration = Duration::from_millis(25);
const LISTENER_READ_TIMEOUT: Duration = Duration::from_millis(100);
const ACCEPT_RETRY_INTERVAL_MS: libc::c_int = 100;

pub type PlatformError = io::Error;

/// Ошибка запроса активации уже работающего экземпляра.
#[derive(Debug)]
pub enum ActivationError {
    Timeout,
    Io(io::Error),
}

impl fmt::Display for ActivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("истёк срок ожидания подтверждения активации"),
            Self::Io(error) => write!(f, "ошибка активации экземпляра: {error}"),
        }
    }
}

impl Error for ActivationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeout => None,
            Self::Io(error) => Some(error),
        }
    }
}

/// Результат попытки захватить блокировку единственного экземпляра.
pub enum Acquire {
    Acquired(InstanceLock),
    AlreadyRunning,
}

/// Удерживает flock и поток Unix socket-слушателя до завершения приложения.
pub struct InstanceLock {
    lock_file: Option<File>,
    socket_path: PathBuf,
    wake_pipe: Option<(OwnedFd, OwnedFd)>,
    listener: Option<JoinHandle<()>>,
}

impl InstanceLock {
    /// Захватывает блокировку и сразу запускает слушатель запросов активации.
    pub fn acquire(key: &str, on_activate: Box<dyn Fn() + Send>) -> Result<Acquire, PlatformError> {
        Self::acquire_in_directory(key, on_activate, &crate::temp_dir())
    }

    fn acquire_in_directory(
        key: &str,
        on_activate: Box<dyn Fn() + Send>,
        directory: &Path,
    ) -> Result<Acquire, PlatformError> {
        validate_key(key)?;
        let Some(mut lock) = acquire_file_lock_in(key, directory)? else {
            return Ok(Acquire::AlreadyRunning);
        };
        lock.start_listener(on_activate)?;
        Ok(Acquire::Acquired(lock))
    }

    fn start_listener(&mut self, on_activate: Box<dyn Fn() + Send>) -> io::Result<()> {
        if self.listener.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "слушатель активации уже запущен",
            ));
        }
        match fs::remove_file(&self.socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        listener.set_nonblocking(true)?;
        let (wake_read, wake_write) = create_wake_pipe()?;
        let wake_fd = wake_read.as_raw_fd();
        self.wake_pipe = Some((wake_read, wake_write));
        let thread = match thread::Builder::new()
            .name("rve-instance-activation".into())
            // The lock keeps both pipe ends open until after this thread is joined.
            .spawn(move || listener_loop(listener, wake_fd, on_activate))
        {
            Ok(thread) => thread,
            Err(error) => {
                self.wake_pipe.take();
                let _ = fs::remove_file(&self.socket_path);
                return Err(error);
            }
        };
        self.listener = Some(thread);
        Ok(())
    }

    #[cfg(test)]
    fn acquire_deferred_in_directory(key: &str, directory: &Path) -> io::Result<Option<Self>> {
        validate_key(key)?;
        acquire_file_lock_in(key, directory)
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Some((_, wake)) = self.wake_pipe.as_ref() {
            let byte = [1_u8];
            loop {
                // SAFETY: wake is the owned write end of a pipe whose read end stays open here.
                let result =
                    unsafe { libc::write(wake.as_raw_fd(), byte.as_ptr().cast(), byte.len()) };
                if result == byte.len() as libc::ssize_t {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        self.wake_pipe.take();
        let _ = fs::remove_file(&self.socket_path);
        // Releasing the flock happens after the listener and socket endpoint are gone.
        drop(self.lock_file.take());
    }
}

fn acquire_file_lock_in(key: &str, directory: &Path) -> io::Result<Option<InstanceLock>> {
    let lock_path = directory.join(format!("rve-{key}.lock"));
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    // SAFETY: the file descriptor is valid for the lifetime of lock_file.
    if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK)
            || error.raw_os_error() == Some(libc::EAGAIN)
        {
            return Ok(None);
        }
        return Err(error);
    }

    Ok(Some(InstanceLock {
        lock_file: Some(lock_file),
        socket_path: socket_path_for_key_in(key, directory),
        wake_pipe: None,
        listener: None,
    }))
}

fn listener_loop(listener: UnixListener, wake_fd: libc::c_int, on_activate: Box<dyn Fn() + Send>) {
    let listener_fd = listener.as_raw_fd();
    loop {
        match poll_listener(listener_fd, wake_fd) {
            Ok(true) => {}
            Ok(false) => break,
            Err(error) => {
                eprintln!("ошибка ожидания запроса активации: {error}");
                break;
            }
        }
        let stream = match accept_connection(&listener) {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                eprintln!("временная ошибка приёма запроса активации: {error}");
                continue;
            }
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EMFILE)
                        | Some(libc::ENFILE)
                        | Some(libc::ENOMEM)
                        | Some(libc::ENOBUFS)
                ) =>
            {
                eprintln!("временно не удалось принять запрос активации: {error}");
                match poll_wake(wake_fd, ACCEPT_RETRY_INTERVAL_MS) {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(poll_error) => {
                        eprintln!("ошибка ожидания повтора accept: {poll_error}");
                        break;
                    }
                }
            }
            Err(error) => {
                eprintln!("ошибка приёма запроса активации: {error}");
                match poll_wake(wake_fd, ACCEPT_RETRY_INTERVAL_MS) {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(poll_error) => {
                        eprintln!("ошибка ожидания повтора accept: {poll_error}");
                        break;
                    }
                }
            }
        };

        handle_activation_connection(stream, on_activate.as_ref(), LISTENER_READ_TIMEOUT);
    }
}

fn accept_connection(listener: &UnixListener) -> io::Result<UnixStream> {
    let (stream, _) = listener.accept()?;
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn handle_activation_connection(
    mut stream: UnixStream,
    on_activate: &dyn Fn(),
    read_timeout: Duration,
) {
    if let Err(error) = stream.set_read_timeout(Some(read_timeout)) {
        eprintln!("не удалось настроить чтение запроса активации: {error}");
        return;
    }
    let mut request = [0; 4];
    match stream.read_exact(&mut request) {
        Ok(()) if request == *b"ACT\n" => {
            if catch_unwind(AssertUnwindSafe(on_activate)).is_err() {
                eprintln!("обработчик активации завершился паникой");
            }
            if let Err(error) = stream.write_all(b"OK\n") {
                eprintln!("не удалось подтвердить активацию: {error}");
            }
        }
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) => {}
        Err(_) => {}
    }
}

/// Просит уже работающий экземпляр обработать активацию и ждёт подтверждения до deadline.
pub fn activate_running_instance(key: &str, deadline: Instant) -> Result<(), ActivationError> {
    activate_running_instance_inner(key, deadline, &crate::temp_dir(), || {})
}

fn activate_running_instance_inner(
    key: &str,
    deadline: Instant,
    directory: &Path,
    mut on_endpoint_missing: impl FnMut(),
) -> Result<(), ActivationError> {
    validate_key(key).map_err(ActivationError::Io)?;
    let socket_path = socket_path_for_key_in(key, directory);
    let mut stream = loop {
        if Instant::now() >= deadline {
            return Err(ActivationError::Timeout);
        }
        match UnixStream::connect(&socket_path) {
            Ok(stream) => break stream,
            Err(error) if is_endpoint_not_ready(&error) => {
                on_endpoint_missing();
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or(ActivationError::Timeout)?;
                thread::sleep(RETRY_INTERVAL.min(remaining));
            }
            Err(error) => return Err(ActivationError::Io(error)),
        }
    };

    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(ActivationError::Timeout)?;
    stream
        .set_read_timeout(Some(remaining))
        .map_err(ActivationError::Io)?;
    stream
        .set_write_timeout(Some(remaining))
        .map_err(ActivationError::Io)?;
    stream
        .write_all(b"ACT\n")
        .map_err(map_activation_io_error)?;
    let mut response = [0; 3];
    stream
        .read_exact(&mut response)
        .map_err(map_activation_io_error)?;
    if response == *b"OK\n" {
        Ok(())
    } else {
        Err(ActivationError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "неверный ответ слушателя активации",
        )))
    }
}

fn is_endpoint_not_ready(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
    )
}

fn map_activation_io_error(error: io::Error) -> ActivationError {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) {
        ActivationError::Timeout
    } else {
        ActivationError::Io(error)
    }
}

fn socket_path_for_key_in(key: &str, directory: &Path) -> PathBuf {
    directory.join(format!("rve-{:016x}.sock", fnv1a64(key.as_bytes())))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn create_wake_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: fds points to two writable file descriptor slots.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: each descriptor was returned by pipe and remains open here.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        // SAFETY: F_SETFD updates descriptor flags on this valid pipe descriptor.
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
        {
            let error = io::Error::last_os_error();
            // SAFETY: both descriptors were returned by pipe and have not been wrapped yet.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(error);
        }
    }
    // SAFETY: pipe returned two distinct, open descriptors; ownership transfers to OwnedFd.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn poll_listener(listener_fd: libc::c_int, wake_fd: libc::c_int) -> io::Result<bool> {
    let mut fds = [
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        for fd in &mut fds {
            fd.revents = 0;
        }
        // SAFETY: fds points to two initialized pollfd entries for live descriptors.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[0].revents != 0 {
            return Ok(false);
        }
        if fds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("ошибка сокета слушателя"));
        }
        if fds[1].revents & libc::POLLIN != 0 {
            return Ok(true);
        }
    }
}

fn poll_wake(wake_fd: libc::c_int, timeout_ms: libc::c_int) -> io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: wake_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        fd.revents = 0;
        // SAFETY: fd points to one initialized pollfd for the owned wake pipe read end.
        let result = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(fd.revents != 0);
    }
}

fn validate_key(key: &str) -> io::Result<()> {
    if (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "key должен соответствовать [A-Za-z0-9_-]{1,64}",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Child, ChildStdin, Command, Stdio},
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
    };

    const CHILD_MODE: &str = "RVE_PLATFORM_INSTANCE_TEST_MODE";
    const CHILD_KEY: &str = "RVE_PLATFORM_INSTANCE_TEST_KEY";
    const CHILD_DIRECTORY: &str = "RVE_PLATFORM_INSTANCE_TEST_DIRECTORY";
    static NEXT_KEY: AtomicU64 = AtomicU64::new(0);
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestTempDir(PathBuf);

    impl TestTempDir {
        fn new() -> Self {
            let path = crate::temp_dir().join(format!(
                "rve-platform-test-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn wait_for_accept_readiness(listener: &UnixListener) -> io::Result<()> {
        let mut fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            fd.revents = 0;
            // SAFETY: fd points to one initialized pollfd for the live listener.
            let result = unsafe { libc::poll(&mut fd, 1, -1) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fd.revents & libc::POLLIN != 0 {
                return Ok(());
            }
            if fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("ошибка сокета тестового слушателя"));
            }
        }
    }

    fn assert_send<T: Send>() {}

    #[test]
    fn accept_connection_clears_nonblocking_on_accepted_stream() {
        let directory = TestTempDir::new();
        let socket_path = socket_path_for_key_in(&unique_key(), directory.path());
        let listener = UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = UnixStream::connect(&socket_path).unwrap();

        let accepted = accept_connection(&listener).unwrap();
        // SAFETY: F_GETFL reads flags from this live accepted socket descriptor.
        let flags = unsafe { libc::fcntl(accepted.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1, "F_GETFL failed: {}", io::Error::last_os_error());
        assert_eq!(flags & libc::O_NONBLOCK, 0);

        drop((accepted, client, listener));
    }

    #[test]
    fn activation_is_read_after_the_client_is_signalled_that_accept_finished() {
        let directory = TestTempDir::new();
        let socket_path = socket_path_for_key_in(&unique_key(), directory.path());
        let listener = UnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (accepted_sender, accepted_receiver) = mpsc::channel();
        let (activated_sender, activated_receiver) = mpsc::channel();

        let server = thread::spawn(move || {
            wait_for_accept_readiness(&listener).unwrap();
            let stream = accept_connection(&listener).unwrap();
            accepted_sender.send(()).unwrap();
            let on_activate = || activated_sender.send(()).unwrap();
            handle_activation_connection(stream, &on_activate, Duration::from_secs(30));
        });

        let mut client = UnixStream::connect(&socket_path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        accepted_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not accept the connection");
        client.write_all(b"ACT\n").unwrap();

        let mut response = [0; 3];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"OK\n");
        activated_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("activation callback was not called");

        server.join().unwrap();
        drop(client);
    }

    #[test]
    fn instance_lock_is_send() {
        assert_send::<InstanceLock>();
    }

    #[test]
    fn key_validation_matches_the_public_contract() {
        let directory = TestTempDir::new();
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"hello"), 0xa430d84680aabd0b);
        assert!(validate_key("a").is_ok());
        assert!(validate_key(&"x".repeat(64)).is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key(&"x".repeat(65)).is_err());
        assert!(InstanceLock::acquire_in_directory("", Box::new(|| {}), directory.path()).is_err());
        assert!(matches!(
            activate_running_instance_inner("a b", Instant::now(), directory.path(), || {}),
            Err(ActivationError::Io(_))
        ));
        for invalid in ["a b", "a/b", "a.b", "ключ", "a\\b"] {
            assert!(validate_key(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn instance_lock_process_protocol_covers_activation_and_lifecycle() {
        let directory = TestTempDir::new();
        let key = unique_key();
        let mut owner = spawn_child("owner", &key, directory.path());
        owner.wait_for("READY");

        let mut contender = spawn_child("probe", &key, directory.path());
        contender.wait_for("RESULT:AlreadyRunning");
        contender.wait_success();

        assert!(
            activate_running_instance_inner(
                &key,
                Instant::now() + Duration::from_secs(5),
                directory.path(),
                || {},
            )
            .is_ok()
        );
        owner.wait_for("ACTIVATED");
        owner.send("RELEASE");
        owner.wait_success();

        let deferred_key = unique_key();
        let mut deferred_owner = spawn_child("deferred", &deferred_key, directory.path());
        deferred_owner.wait_for("LOCKED");
        let mut activator = spawn_child("activate_probe", &deferred_key, directory.path());
        activator.wait_for("ENDPOINT_MISSING");
        deferred_owner.send("START");
        deferred_owner.wait_for("LISTENING");
        activator.wait_for("RESULT:Ok");
        deferred_owner.wait_for("ACTIVATED");
        deferred_owner.send("RELEASE");
        activator.wait_success();
        deferred_owner.wait_success();

        let timeout_key = unique_key();
        let mut timeout_owner = spawn_child("deferred", &timeout_key, directory.path());
        timeout_owner.wait_for("LOCKED");
        assert!(matches!(
            activate_running_instance_inner(
                &timeout_key,
                Instant::now() + Duration::from_millis(100),
                directory.path(),
                || {},
            ),
            Err(ActivationError::Timeout)
        ));
        timeout_owner.send("RELEASE");
        timeout_owner.wait_success();

        let reacquire_key = unique_key();
        let first =
            InstanceLock::acquire_in_directory(&reacquire_key, Box::new(|| {}), directory.path())
                .unwrap();
        let Acquire::Acquired(first) = first else {
            panic!("initial acquisition unexpectedly found another instance");
        };
        drop(first);
        let second =
            InstanceLock::acquire_in_directory(&reacquire_key, Box::new(|| {}), directory.path())
                .unwrap();
        let Acquire::Acquired(second) = second else {
            panic!("acquisition after drop unexpectedly found another instance");
        };
        drop(second);
    }

    #[test]
    fn long_key_uses_short_socket_name_and_activates() {
        let directory = TestTempDir::new();
        let mut key = unique_key();
        key.push_str(&"x".repeat(64 - key.len()));
        let (sender, receiver) = mpsc::channel();
        let Acquire::Acquired(lock) = InstanceLock::acquire_in_directory(
            &key,
            Box::new(move || sender.send(()).unwrap()),
            directory.path(),
        )
        .unwrap() else {
            panic!("initial acquisition unexpectedly found another instance");
        };
        let socket_name = socket_path_for_key_in(&key, directory.path())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(socket_name.len(), "rve-".len() + 16 + ".sock".len());
        assert!(socket_name.starts_with("rve-") && socket_name.ends_with(".sock"));

        activate_running_instance_inner(
            &key,
            Instant::now() + Duration::from_secs(5),
            directory.path(),
            || {},
        )
        .unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(lock);
    }

    #[test]
    fn drop_wakes_listener_after_socket_is_removed() {
        let directory = TestTempDir::new();
        let key = unique_key();
        let Acquire::Acquired(lock) =
            InstanceLock::acquire_in_directory(&key, Box::new(|| {}), directory.path()).unwrap()
        else {
            panic!("initial acquisition unexpectedly found another instance");
        };
        fs::remove_file(socket_path_for_key_in(&key, directory.path())).unwrap();

        let (sender, receiver) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(lock);
            sender.send(()).unwrap();
        });
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("drop did not wake the listener through its pipe");
        dropper.join().unwrap();
    }

    #[test]
    fn killed_owner_releases_lock_and_stale_socket_is_replaced() {
        let directory = TestTempDir::new();
        let key = unique_key();
        let mut owner = spawn_child("owner", &key, directory.path());
        owner.wait_for("READY");
        let socket_path = socket_path_for_key_in(&key, directory.path());
        assert!(socket_path.exists());
        owner.child.kill().unwrap();
        assert!(!owner.child.wait().unwrap().success());
        assert!(socket_path.exists());

        let (sender, receiver) = mpsc::channel();
        let Acquire::Acquired(lock) = InstanceLock::acquire_in_directory(
            &key,
            Box::new(move || sender.send(()).unwrap()),
            directory.path(),
        )
        .unwrap() else {
            panic!("acquisition after owner termination unexpectedly failed");
        };
        assert!(socket_path.exists());
        activate_running_instance_inner(
            &key,
            Instant::now() + Duration::from_secs(5),
            directory.path(),
            || {},
        )
        .unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(lock);
    }

    #[test]
    fn process_child_mode() {
        let Ok(mode) = std::env::var(CHILD_MODE) else {
            return;
        };
        let key = std::env::var(CHILD_KEY).expect("child key is set");
        let directory =
            PathBuf::from(std::env::var_os(CHILD_DIRECTORY).expect("child directory is set"));
        match mode.as_str() {
            "owner" => {
                let Acquire::Acquired(_lock) = InstanceLock::acquire_in_directory(
                    &key,
                    Box::new(|| print_line("ACTIVATED")),
                    &directory,
                )
                .unwrap() else {
                    panic!("owner failed to acquire its unique key");
                };
                print_line("READY");
                wait_for_stdin("RELEASE");
            }
            "probe" => match InstanceLock::acquire_in_directory(&key, Box::new(|| {}), &directory)
                .unwrap()
            {
                Acquire::AlreadyRunning => print_line("RESULT:AlreadyRunning"),
                Acquire::Acquired(lock) => {
                    drop(lock);
                    print_line("RESULT:UnexpectedAcquired");
                }
            },
            "activate_probe" => {
                let mut first_miss = true;
                let result = activate_running_instance_inner(
                    &key,
                    Instant::now() + Duration::from_secs(5),
                    &directory,
                    || {
                        if first_miss {
                            first_miss = false;
                            print_line("ENDPOINT_MISSING");
                        }
                    },
                );
                print_line(&format!("RESULT:{}", activation_result_name(result)));
            }
            "deferred" => {
                let Some(mut lock) =
                    InstanceLock::acquire_deferred_in_directory(&key, &directory).unwrap()
                else {
                    panic!("deferred owner failed to acquire its unique key");
                };
                print_line("LOCKED");
                if read_stdin_line() == "START" {
                    lock.start_listener(Box::new(|| print_line("ACTIVATED")))
                        .unwrap();
                    print_line("LISTENING");
                    wait_for_stdin("RELEASE");
                }
            }
            _ => panic!("unknown child mode {mode:?}"),
        }
    }

    fn activation_result_name(result: Result<(), ActivationError>) -> &'static str {
        match result {
            Ok(()) => "Ok",
            Err(ActivationError::Timeout) => "Timeout",
            Err(ActivationError::Io(_)) => "Io",
        }
    }

    fn unique_key() -> String {
        format!(
            "t{}_{}",
            std::process::id(),
            NEXT_KEY.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn print_line(line: &str) {
        println!("{line}");
        io::stdout().flush().unwrap();
    }

    fn read_stdin_line() -> String {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line).unwrap();
        line.trim_end().to_owned()
    }

    fn wait_for_stdin(expected: &str) {
        assert_eq!(read_stdin_line(), expected);
    }

    struct TestChild {
        child: Child,
        stdin: ChildStdin,
        lines: mpsc::Receiver<String>,
        reader: Option<JoinHandle<()>>,
    }

    impl TestChild {
        fn send(&mut self, command: &str) {
            writeln!(self.stdin, "{command}").unwrap();
            self.stdin.flush().unwrap();
        }

        fn wait_for(&mut self, marker: &str) {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or_default();
                let line = self
                    .lines
                    .recv_timeout(remaining)
                    .unwrap_or_else(|error| panic!("waiting for {marker:?} failed: {error}"));
                if line.contains(marker) {
                    return;
                }
            }
        }

        fn wait_success(&mut self) {
            let status = self.child.wait().unwrap();
            assert!(status.success(), "child process exited with {status}");
            if let Some(reader) = self.reader.take() {
                reader.join().unwrap();
            }
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn spawn_child(mode: &str, key: &str, directory: &Path) -> TestChild {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance::tests::process_child_mode",
                "--nocapture",
            ])
            .env(CHILD_MODE, mode)
            .env(CHILD_KEY, key)
            .env(CHILD_DIRECTORY, directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        TestChild {
            child,
            stdin,
            lines,
            reader: Some(reader),
        }
    }
}
