use std::{
    error::Error,
    ffi::OsStr,
    fmt, io,
    os::windows::ffi::OsStrExt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE,
        WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::Threading::{
        CreateEventW, CreateMutexW, EVENT_ALL_ACCESS, OpenEventW, ReleaseMutex, ResetEvent,
        SetEvent, WaitForMultipleObjects, WaitForSingleObject,
    },
    UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow},
};

const RETRY_INTERVAL: Duration = Duration::from_millis(50);

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

struct Handle(HANDLE);

// SAFETY: kernel handles can be used across threads; ownership remains unique and
// the wrapped object provides its own synchronization semantics.
unsafe impl Send for Handle {}

impl Handle {
    fn from_raw(handle: HANDLE) -> io::Result<Self> {
        if handle.is_null() {
            Err(last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this wrapper uniquely owns the handle returned by a Win32 create/open call.
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct ActivatorMutex(Handle);

impl Drop for ActivatorMutex {
    fn drop(&mut self) {
        // SAFETY: this thread acquired the mutex and owns its handle until this drop.
        unsafe { ReleaseMutex(self.0.raw()) };
    }
}

/// Удерживает именованный mutex и поток IPC-слушателя до завершения приложения.
pub struct InstanceLock {
    mutex: Option<Handle>,
    activate: Option<Handle>,
    ack: Option<Handle>,
    stop: Option<Handle>,
    listener: Option<JoinHandle<()>>,
}

impl InstanceLock {
    /// Захватывает блокировку и сразу запускает слушатель запросов активации.
    pub fn acquire(key: &str, on_activate: Box<dyn Fn() + Send>) -> Result<Acquire, PlatformError> {
        validate_key(key)?;
        let Some(mut lock) = acquire_mutex(key)? else {
            return Ok(Acquire::AlreadyRunning);
        };
        lock.start_listener(key, on_activate)?;
        Ok(Acquire::Acquired(lock))
    }

    fn start_listener(&mut self, key: &str, on_activate: Box<dyn Fn() + Send>) -> io::Result<()> {
        let activate_name = wide(&format!("Local\\RVE-{key}.activate"));
        let ack_name = wide(&format!("Local\\RVE-{key}.ack"));
        let activate = create_event(&activate_name, false)?;
        let ack = create_event(&ack_name, false)?;
        // SAFETY: null attributes and a null name request a private unnamed stop event.
        let stop = Handle::from_raw(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
        let activate_raw = activate.raw() as usize;
        let ack_raw = ack.raw() as usize;
        let stop_raw = stop.raw() as usize;
        let listener = thread::Builder::new()
            .name("rve-instance-activation".into())
            .spawn(move || listener_loop(activate_raw, ack_raw, stop_raw, on_activate))?;

        self.activate = Some(activate);
        self.ack = Some(ack);
        self.stop = Some(stop);
        self.listener = Some(listener);
        Ok(())
    }

    #[cfg(test)]
    fn acquire_deferred(key: &str) -> io::Result<Option<Self>> {
        validate_key(key)?;
        acquire_mutex(key)
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            // SAFETY: the stop event remains owned until the listener has joined.
            unsafe { SetEvent(stop.raw()) };
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }

        // Remove named endpoints before releasing the mutex, so a new owner cannot race
        // with handles from this instance.
        drop(self.activate.take());
        drop(self.ack.take());
        drop(self.stop.take());
        drop(self.mutex.take());
    }
}

fn acquire_mutex(key: &str) -> io::Result<Option<InstanceLock>> {
    let name = wide(&format!("Local\\RVE-{key}"));
    // SAFETY: the name is a valid NUL-terminated UTF-16 string; null attributes request defaults.
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    let mutex = Handle::from_raw(mutex)?;
    // GetLastError must be read immediately after CreateMutexW.
    // SAFETY: GetLastError has no preconditions.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        drop(mutex);
        return Ok(None);
    }
    Ok(Some(InstanceLock {
        mutex: Some(mutex),
        activate: None,
        ack: None,
        stop: None,
        listener: None,
    }))
}

fn create_event(name: &[u16], manual_reset: bool) -> io::Result<Handle> {
    // SAFETY: name is NUL-terminated; default security attributes are requested.
    Handle::from_raw(unsafe {
        CreateEventW(ptr::null(), i32::from(manual_reset), 0, name.as_ptr())
    })
}

fn listener_loop(activate: usize, ack: usize, stop: usize, on_activate: Box<dyn Fn() + Send>) {
    let handles = [activate as HANDLE, stop as HANDLE];
    loop {
        // SAFETY: both handles remain valid until this listener is joined; the array has two entries.
        let result = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX) };
        if result == WAIT_OBJECT_0 + 1 {
            break;
        }
        if result == WAIT_OBJECT_0 {
            if catch_unwind(AssertUnwindSafe(on_activate.as_ref())).is_err() {
                eprintln!("обработчик активации завершился паникой");
            }
            // SAFETY: the acknowledgement event remains valid until this thread is joined.
            if unsafe { SetEvent(ack as HANDLE) } == 0 {
                eprintln!("не удалось подтвердить активацию: {}", last_os_error());
            }
            continue;
        }
        if result == WAIT_FAILED {
            eprintln!("ошибка ожидания запроса активации: {}", last_os_error());
        }
        break;
    }
}

/// Просит уже работающий экземпляр обработать активацию и ждёт подтверждения до deadline.
pub fn activate_running_instance(key: &str, deadline: Instant) -> Result<(), ActivationError> {
    activate_running_instance_inner(key, deadline, || {})
}

fn activate_running_instance_inner(
    key: &str,
    deadline: Instant,
    mut on_endpoint_missing: impl FnMut(),
) -> Result<(), ActivationError> {
    validate_key(key).map_err(ActivationError::Io)?;

    // Foreground permission is best-effort: Windows can reject it for background callers,
    // while the IPC request and acknowledgement remain valid.
    // SAFETY: this API has no pointer arguments or other preconditions.
    unsafe { AllowSetForegroundWindow(ASFW_ANY) };

    let _activator = acquire_activator_mutex(key, deadline)?;
    let activate_name = wide(&format!("Local\\RVE-{key}.activate"));
    let ack_name = wide(&format!("Local\\RVE-{key}.ack"));
    let activate = open_event_until(&activate_name, deadline, &mut on_endpoint_missing)?;
    let ack = open_event_until(&ack_name, deadline, &mut || {})?;

    // SAFETY: the opened acknowledgement event grants EVENT_ALL_ACCESS.
    if unsafe { ResetEvent(ack.raw()) } == 0 {
        return Err(ActivationError::Io(last_os_error()));
    }

    // SAFETY: the opened handle grants EVENT_ALL_ACCESS, including SetEvent.
    if unsafe { SetEvent(activate.raw()) } == 0 {
        return Err(ActivationError::Io(last_os_error()));
    }

    let timeout = remaining_millis(deadline)?;
    // SAFETY: the opened acknowledgement event grants EVENT_ALL_ACCESS and remains alive.
    match unsafe { WaitForSingleObject(ack.raw(), timeout) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err(ActivationError::Timeout),
        WAIT_FAILED => Err(ActivationError::Io(last_os_error())),
        _ => Err(ActivationError::Io(io::Error::other(
            "неожиданный результат ожидания подтверждения активации",
        ))),
    }
}

fn open_event_until(
    name: &[u16],
    deadline: Instant,
    on_missing: &mut dyn FnMut(),
) -> Result<Handle, ActivationError> {
    loop {
        if Instant::now() >= deadline {
            return Err(ActivationError::Timeout);
        }
        // SAFETY: name is NUL-terminated and the returned event handle is owned on success.
        let event = unsafe { OpenEventW(EVENT_ALL_ACCESS, 0, name.as_ptr()) };
        if !event.is_null() {
            return Ok(Handle(event));
        }
        // SAFETY: GetLastError has no preconditions and immediately follows OpenEventW.
        let error = unsafe { GetLastError() };
        if error != ERROR_FILE_NOT_FOUND {
            return Err(ActivationError::Io(io::Error::from_raw_os_error(
                error as i32,
            )));
        }
        on_missing();
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ActivationError::Timeout)?;
        thread::sleep(RETRY_INTERVAL.min(remaining));
    }
}

fn acquire_activator_mutex(
    key: &str,
    deadline: Instant,
) -> Result<ActivatorMutex, ActivationError> {
    let name = wide(&format!("Local\\RVE-{key}.activator"));
    // SAFETY: name is NUL-terminated and null attributes request the default security descriptor.
    let handle = Handle::from_raw(unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) })
        .map_err(ActivationError::Io)?;
    let timeout = remaining_millis(deadline)?;
    // SAFETY: the owned handle refers to the named mutex and remains open during the wait.
    match unsafe { WaitForSingleObject(handle.raw(), timeout) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(ActivatorMutex(handle)),
        WAIT_TIMEOUT => Err(ActivationError::Timeout),
        WAIT_FAILED => Err(ActivationError::Io(last_os_error())),
        _ => Err(ActivationError::Io(io::Error::other(
            "неожиданный результат захвата mutex активации",
        ))),
    }
}

fn remaining_millis(deadline: Instant) -> Result<u32, ActivationError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(ActivationError::Timeout)?;
    Ok(remaining.as_millis().min((u32::MAX - 1) as u128) as u32)
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

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

fn last_os_error() -> io::Error {
    // SAFETY: GetLastError has no preconditions.
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
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
    static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

    fn assert_send<T: Send>() {}

    #[test]
    fn instance_lock_is_send() {
        assert_send::<InstanceLock>();
    }

    #[test]
    fn key_validation_matches_the_public_contract() {
        assert!(validate_key("a").is_ok());
        assert!(validate_key(&"x".repeat(64)).is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key(&"x".repeat(65)).is_err());
        assert!(InstanceLock::acquire("", Box::new(|| {})).is_err());
        assert!(matches!(
            activate_running_instance("a b", Instant::now()),
            Err(ActivationError::Io(_))
        ));
        for invalid in ["a b", "a/b", "a.b", "ключ", "a\\b"] {
            assert!(validate_key(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn instance_lock_process_protocol_covers_activation_and_lifecycle() {
        let key = unique_key();
        let mut owner = spawn_child("owner", &key);
        owner.wait_for("READY");

        let mut contender = spawn_child("probe", &key);
        contender.wait_for("RESULT:AlreadyRunning");
        contender.wait_success();

        let mut first_activator = spawn_child("activate_probe", &key);
        let mut second_activator = spawn_child("activate_probe", &key);
        first_activator.wait_for("RESULT:Ok");
        second_activator.wait_for("RESULT:Ok");
        first_activator.wait_success();
        second_activator.wait_success();
        owner.wait_for("ACTIVATED");
        owner.wait_for("ACTIVATED");
        owner.send("RELEASE");
        owner.wait_success();

        let deferred_key = unique_key();
        let mut deferred_owner = spawn_child("deferred", &deferred_key);
        deferred_owner.wait_for("LOCKED");
        let mut activator = spawn_child("activate_probe", &deferred_key);
        activator.wait_for("ENDPOINT_MISSING");
        deferred_owner.send("START");
        deferred_owner.wait_for("LISTENING");
        activator.wait_for("RESULT:Ok");
        deferred_owner.wait_for("ACTIVATED");
        deferred_owner.send("RELEASE");
        activator.wait_success();
        deferred_owner.wait_success();

        let timeout_key = unique_key();
        let mut timeout_owner = spawn_child("deferred", &timeout_key);
        timeout_owner.wait_for("LOCKED");
        assert!(matches!(
            activate_running_instance(&timeout_key, Instant::now() + Duration::from_millis(100)),
            Err(ActivationError::Timeout)
        ));
        timeout_owner.send("RELEASE");
        timeout_owner.wait_success();

        let reacquire_key = unique_key();
        let first = InstanceLock::acquire(&reacquire_key, Box::new(|| {})).unwrap();
        let Acquire::Acquired(first) = first else {
            panic!("initial acquisition unexpectedly found another instance");
        };
        drop(first);
        assert!(matches!(
            InstanceLock::acquire(&reacquire_key, Box::new(|| {})).unwrap(),
            Acquire::Acquired(_)
        ));
    }

    #[test]
    fn process_child_mode() {
        let Ok(mode) = std::env::var(CHILD_MODE) else {
            return;
        };
        let key = std::env::var(CHILD_KEY).expect("child key is set");
        match mode.as_str() {
            "owner" => {
                let Acquire::Acquired(_lock) =
                    InstanceLock::acquire(&key, Box::new(|| print_line("ACTIVATED"))).unwrap()
                else {
                    panic!("owner failed to acquire its unique key");
                };
                print_line("READY");
                wait_for_stdin("RELEASE");
            }
            "probe" => match InstanceLock::acquire(&key, Box::new(|| {})).unwrap() {
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
                let Some(mut lock) = InstanceLock::acquire_deferred(&key).unwrap() else {
                    panic!("deferred owner failed to acquire its unique key");
                };
                print_line("LOCKED");
                if read_stdin_line() == "START" {
                    lock.start_listener(&key, Box::new(|| print_line("ACTIVATED")))
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
        std::io::stdout().flush().unwrap();
    }

    fn read_stdin_line() -> String {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
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

    fn spawn_child(mode: &str, key: &str) -> TestChild {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance::tests::process_child_mode",
                "--nocapture",
            ])
            .env(CHILD_MODE, mode)
            .env(CHILD_KEY, key)
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
