//! Платформенная интеграция Vulkan и оконной поверхности для macOS.
#![cfg(target_os = "macos")]

use ash::{Entry, LoadingError, vk};
use std::{
    ffi::{CStr, OsStr, OsString},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::OnceLock,
};

mod instance;

pub use instance::{
    Acquire, ActivationError, InstanceLock, PlatformError, activate_running_instance,
};

pub use ash_window::{create_surface, enumerate_required_extensions};

/// Platform adapter version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Дополнительные к surface расширения instance, необходимые платформе.
pub const INSTANCE_EXTENSIONS: &[&CStr] = &[ash::khr::portability_enumeration::NAME];
/// Флаги создания Vulkan instance для платформы.
pub const INSTANCE_FLAGS: vk::InstanceCreateFlags =
    vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
/// Дополнительные к swapchain расширения устройства, необходимые платформе.
pub const DEVICE_EXTENSIONS: &[&CStr] = &[ash::khr::portability_subset::NAME];

/// Returns the user's cache directory.
pub fn user_cache_dir() -> Option<PathBuf> {
    user_cache_dir_with(|name| std::env::var_os(name))
}

/// Returns the user's application-support directory.
pub fn user_data_dir() -> Option<PathBuf> {
    user_data_dir_with(|name| std::env::var_os(name))
}

fn user_data_dir_with(mut get: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    get("HOME").filter(|path| !path.is_empty()).map(|home| {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
    })
}

/// Makes the process ignore Ctrl+C (`SIGINT`) from its terminal, so the engine is not closed by
/// it. The disposition is inherited by child processes. Other signals keep their defaults.
pub fn ignore_console_interrupt() -> std::io::Result<()> {
    // SAFETY: installing SIG_IGN for SIGINT runs no handler code and is async-signal-safe.
    if unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) } == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Atomically replaces a file with POSIX rename semantics.
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

fn user_cache_dir_with(mut get: impl FnMut(&str) -> Option<OsString>) -> Option<PathBuf> {
    get("HOME")
        .filter(|path| !path.is_empty())
        .map(|home| PathBuf::from(home).join("Library").join("Caches"))
}

static BUNDLED_VULKAN_LOADER: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn temp_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Configures bundled MoltenVK files before the application creates any threads.
///
/// The first discovered bundled loader path is retained for [`load_vulkan`], even
/// when user-provided ICD variables prevent changing the process environment.
///
/// # Safety
///
/// Call this before creating any threads so no other thread can access the process
/// environment while it is being updated.
pub unsafe fn prepare_vulkan_environment(exe_dir: &Path) {
    let engine_dir = exe_dir.join("Engine");
    let loader_path = engine_dir.join("libvulkan.1.dylib");
    let icd_path = engine_dir.join("MoltenVK_icd.json");
    let loader_exists = loader_path.is_file();
    let icd_exists = icd_path.is_file();
    let vk_driver_files = std::env::var_os("VK_DRIVER_FILES");
    let vk_icd_filenames = std::env::var_os("VK_ICD_FILENAMES");
    let vk_add_driver_files = std::env::var_os("VK_ADD_DRIVER_FILES");
    let user_driver_variables_set =
        vk_driver_files.is_some() || vk_icd_filenames.is_some() || vk_add_driver_files.is_some();

    if loader_exists && (icd_exists || user_driver_variables_set) {
        let _ = BUNDLED_VULKAN_LOADER.set(loader_path);
    }

    if should_set_bundled_icd(
        loader_exists,
        icd_exists,
        vk_driver_files.as_deref(),
        vk_icd_filenames.as_deref(),
        vk_add_driver_files.as_deref(),
    ) {
        // SAFETY: this API is called as the first operation in main, before any
        // threads are created, so no thread can concurrently access the process environment.
        unsafe { std::env::set_var("VK_DRIVER_FILES", &icd_path) };
    }
}

fn should_set_bundled_icd(
    loader_exists: bool,
    icd_exists: bool,
    vk_driver_files: Option<&OsStr>,
    vk_icd_filenames: Option<&OsStr>,
    vk_add_driver_files: Option<&OsStr>,
) -> bool {
    loader_exists
        && icd_exists
        && vk_driver_files.is_none()
        && vk_icd_filenames.is_none()
        && vk_add_driver_files.is_none()
}

/// Загружает Vulkan loader из окружения приложения или Homebrew на Apple Silicon.
pub fn load_vulkan() -> Result<Entry, LoadingError> {
    if let Some(path) = BUNDLED_VULKAN_LOADER.get()
        && let Ok(entry) = unsafe { Entry::load_from(path) }
    {
        return Ok(entry);
    }

    // SAFETY: оба пути указывают на Vulkan loader; Entry удерживает библиотеку живой.
    unsafe { Entry::load().or_else(|_| Entry::load_from("/opt/homebrew/lib/libvulkan.dylib")) }
}

/// Показывает системный критический диалог на главном потоке.
///
/// При недоступной GUI-сессии или вызове не с главного потока сообщение записывается
/// в stderr.
pub fn show_error_dialog(title: &str, message: &str) {
    use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication};
    use objc2_foundation::{MainThreadMarker, NSString};
    use std::io::Write;

    let Some(mtm) = MainThreadMarker::new() else {
        let _ = writeln!(
            std::io::stderr().lock(),
            "не удалось показать диалог ошибки вне главного потока: {title}: {message}"
        );
        return;
    };
    if objc2_core_graphics::CGSessionCopyCurrentDictionary().is_none() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "не удалось показать диалог ошибки без GUI-сессии: {title}: {message}"
        );
        return;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let _application = NSApplication::sharedApplication(mtm);
        let alert = unsafe { NSAlert::new(mtm) };
        let title = NSString::from_str(title);
        let message = NSString::from_str(message);
        unsafe {
            alert.setMessageText(&title);
            alert.setInformativeText(&message);
            alert.setAlertStyle(NSAlertStyle::Critical);
            alert.runModal();
        }
    }));
    if result.is_err() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "не удалось показать системный диалог ошибки: {title}: {message}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
    };

    const VULKAN_CHILD_MODE: &str = "RVE_PLATFORM_VULKAN_TEST_MODE";
    const VULKAN_EXE_DIR: &str = "RVE_PLATFORM_VULKAN_TEST_EXE_DIR";
    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn ctrl_c_is_ignored_after_ignore_console_interrupt() {
        ignore_console_interrupt().unwrap();
        // SAFETY: an all-zero sigaction is a valid output buffer; a null new action only queries.
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current) },
            0
        );
        assert_eq!(current.sa_sigaction, libc::SIG_IGN);
        // Without the change this signal would terminate the test process.
        // SAFETY: raising SIGINT only delivers the now ignored signal to this process.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
    }

    #[test]
    fn moltenvk_requires_portability_enumeration_and_subset() {
        assert!(INSTANCE_EXTENSIONS.contains(&c"VK_KHR_portability_enumeration"));
        assert!(INSTANCE_FLAGS.contains(vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR));
        assert!(DEVICE_EXTENSIONS.contains(&c"VK_KHR_portability_subset"));
    }

    #[test]
    fn cache_directory_uses_injected_home_environment() {
        assert_eq!(
            user_cache_dir_with(|name| { (name == "HOME").then(|| OsString::from("/Users/rve")) }),
            Some(PathBuf::from("/Users/rve/Library/Caches"))
        );
        assert_eq!(user_cache_dir_with(|_| None), None);
    }

    #[test]
    fn data_directory_uses_application_support() {
        assert_eq!(
            user_data_dir_with(|name| (name == "HOME").then(|| OsString::from("/Users/rve"))),
            Some(PathBuf::from("/Users/rve/Library/Application Support"))
        );
        assert_eq!(user_data_dir_with(|_| None), None);
    }

    #[test]
    fn prepare_vulkan_environment_child_process() {
        if let Ok(mode) = std::env::var(VULKAN_CHILD_MODE) {
            assert_eq!(mode, "run");
            let exe_dir = PathBuf::from(std::env::var_os(VULKAN_EXE_DIR).unwrap());
            // SAFETY: this isolated subprocess has no other threads accessing its environment.
            unsafe { prepare_vulkan_environment(&exe_dir) };
            println!("RVE_VULKAN_RESULT:{}", environment_snapshot());
            assert!(!mode.is_empty());
            return;
        }

        let bundled_dir = test_exe_dir("bundled");
        let engine_dir = bundled_dir.0.join("Engine");
        fs::create_dir_all(&engine_dir).unwrap();
        fs::write(engine_dir.join("libvulkan.1.dylib"), b"").unwrap();
        let icd_path = engine_dir.join("MoltenVK_icd.json");
        fs::write(&icd_path, b"").unwrap();
        let output = run_vulkan_child(&bundled_dir.0, None);
        assert_eq!(
            output,
            format!(
                "VK_DRIVER_FILES=Some({:?});VK_ICD_FILENAMES=None;VK_ADD_DRIVER_FILES=None",
                icd_path.as_os_str()
            )
        );

        for variable in ["VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_ADD_DRIVER_FILES"] {
            let output = run_vulkan_child(&bundled_dir.0, Some(variable));
            let expected = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_ADD_DRIVER_FILES"]
                .map(|name| {
                    format!(
                        "{name}={:?}",
                        (name == variable).then_some(OsStr::new("user-value"))
                    )
                })
                .join(";");
            assert_eq!(
                output, expected,
                "environment changed when {variable} was set"
            );
        }

        let no_bundle_dir = test_exe_dir("no-bundle");
        let output = run_vulkan_child(&no_bundle_dir.0, None);
        assert_eq!(
            output,
            "VK_DRIVER_FILES=None;VK_ICD_FILENAMES=None;VK_ADD_DRIVER_FILES=None"
        );
    }

    fn environment_snapshot() -> String {
        ["VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_ADD_DRIVER_FILES"]
            .map(|name| format!("{name}={:?}", std::env::var_os(name)))
            .join(";")
    }

    fn run_vulkan_child(exe_dir: &Path, user_variable: Option<&str>) -> String {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::prepare_vulkan_environment_child_process",
                "--nocapture",
            ])
            .env(VULKAN_CHILD_MODE, "run")
            .env(VULKAN_EXE_DIR, exe_dir)
            .env_remove("VK_DRIVER_FILES")
            .env_remove("VK_ICD_FILENAMES")
            .env_remove("VK_ADD_DRIVER_FILES");
        if let Some(variable) = user_variable {
            command.env(variable, "user-value");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "Vulkan test subprocess failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout
            .lines()
            .find_map(|line| line.strip_prefix("RVE_VULKAN_RESULT:"))
            .expect("subprocess reported its environment")
            .to_owned()
    }

    struct TestExeDir(PathBuf);

    impl Drop for TestExeDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_exe_dir(label: &str) -> TestExeDir {
        let path = crate::temp_dir().join(format!(
            "rve-platform-vulkan-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        TestExeDir(path)
    }
}
