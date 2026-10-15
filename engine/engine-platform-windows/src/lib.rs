//! Платформенная интеграция Vulkan и оконной поверхности для Windows.
#![cfg(target_os = "windows")]

use ash::{Entry, LoadingError, vk};
use std::{ffi::CStr, path::Path};

mod instance;

pub use instance::{
    Acquire, ActivationError, InstanceLock, PlatformError, activate_running_instance,
};

pub use ash_window::{create_surface, enumerate_required_extensions};

/// Дополнительные к surface расширения instance, необходимые платформе.
pub const INSTANCE_EXTENSIONS: &[&CStr] = &[];
/// Флаги создания Vulkan instance для платформы.
pub const INSTANCE_FLAGS: vk::InstanceCreateFlags = vk::InstanceCreateFlags::empty();
/// Дополнительные к swapchain расширения устройства, необходимые платформе.
pub const DEVICE_EXTENSIONS: &[&CStr] = &[];

/// Показывает системный диалог ошибки. При недоступной GUI-сессии пишет сообщение в stderr.
pub fn show_error_dialog(title: &str, message: &str) {
    use std::{ffi::OsStr, io::Write, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let title: Vec<u16> = OsStr::new(title).encode_wide().chain(Some(0)).collect();
    let message: Vec<u16> = OsStr::new(message).encode_wide().chain(Some(0)).collect();
    // SAFETY: обе строки имеют UTF-16 NUL-terminated буферы, действующие на время вызова.
    let result = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
    if result == 0 {
        let _ = writeln!(
            std::io::stderr().lock(),
            "не удалось показать системный диалог ошибки: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// На Windows выбор Vulkan loader и ICD остаётся за системным окружением.
///
/// # Safety
///
/// Вызывайте эту функцию до создания любых потоков, чтобы сигнатура совпадала с
/// macOS-контрактом подготовки окружения.
pub unsafe fn prepare_vulkan_environment(_exe_dir: &Path) {}

/// Загружает системный Vulkan loader (`vulkan-1.dll`).
pub fn load_vulkan() -> Result<Entry, LoadingError> {
    // SAFETY: загружается системный Vulkan loader; Entry удерживает библиотеку живой.
    unsafe { Entry::load() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_vulkan_requires_no_additional_platform_extensions() {
        assert!(INSTANCE_EXTENSIONS.is_empty());
        assert!(INSTANCE_FLAGS.is_empty());
        assert!(DEVICE_EXTENSIONS.is_empty());
    }
}
