//! Платформенная интеграция Vulkan и оконной поверхности для Windows.
#![cfg(target_os = "windows")]

use ash::{Entry, LoadingError, vk};
use std::ffi::CStr;

pub use ash_window::{create_surface, enumerate_required_extensions};

/// Дополнительные к surface расширения instance, необходимые платформе.
pub const INSTANCE_EXTENSIONS: &[&CStr] = &[];
/// Флаги создания Vulkan instance для платформы.
pub const INSTANCE_FLAGS: vk::InstanceCreateFlags = vk::InstanceCreateFlags::empty();
/// Дополнительные к swapchain расширения устройства, необходимые платформе.
pub const DEVICE_EXTENSIONS: &[&CStr] = &[];

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
