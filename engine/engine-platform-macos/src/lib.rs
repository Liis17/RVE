//! Платформенная интеграция Vulkan и оконной поверхности для macOS.
#![cfg(target_os = "macos")]

use ash::{Entry, LoadingError, vk};
use std::ffi::CStr;

pub use ash_window::{create_surface, enumerate_required_extensions};

/// Дополнительные к surface расширения instance, необходимые платформе.
pub const INSTANCE_EXTENSIONS: &[&CStr] = &[ash::khr::portability_enumeration::NAME];
/// Флаги создания Vulkan instance для платформы.
pub const INSTANCE_FLAGS: vk::InstanceCreateFlags =
    vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;
/// Дополнительные к swapchain расширения устройства, необходимые платформе.
pub const DEVICE_EXTENSIONS: &[&CStr] = &[ash::khr::portability_subset::NAME];

/// Загружает Vulkan loader из окружения приложения или Homebrew на Apple Silicon.
pub fn load_vulkan() -> Result<Entry, LoadingError> {
    // SAFETY: оба пути указывают на Vulkan loader; Entry удерживает библиотеку живой.
    unsafe { Entry::load().or_else(|_| Entry::load_from("/opt/homebrew/lib/libvulkan.dylib")) }
}
