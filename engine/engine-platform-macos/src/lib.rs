//! Платформенная интеграция Vulkan и оконной поверхности для macOS.
#![cfg(target_os = "macos")]

use ash::{Entry, LoadingError, vk};
use std::ffi::CStr;

pub use ash_window::{create_surface, enumerate_required_extensions};

/// Дополнительные к surface расширения instance, необходимые платформе.
pub const INSTANCE_EXTENSIONS: &[&CStr] = &[];
/// Флаги создания Vulkan instance для платформы.
pub const INSTANCE_FLAGS: vk::InstanceCreateFlags = vk::InstanceCreateFlags::empty();
/// Дополнительные к swapchain расширения устройства, необходимые платформе.
pub const DEVICE_EXTENSIONS: &[&CStr] = &[];

/// Загружает Vulkan loader (`libvulkan.dylib`) из окружения приложения.
pub fn load_vulkan() -> Result<Entry, LoadingError> {
    // SAFETY: загружается Vulkan loader; Entry удерживает библиотеку живой.
    unsafe { Entry::load() }
}
