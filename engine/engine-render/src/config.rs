//! Конфигурация renderer, его возможности и отчёт о физическом устройстве.

use crate::*;

/// Настройки Vulkan-рендерера.
#[derive(Default)]
pub struct RendererConfig {
    pub extra_instance_extensions: Vec<&'static CStr>,
    pub extra_device_extensions: Vec<&'static CStr>,
    pub present_mode: PresentMode,
    /// Vulkan pipeline cache bytes validated against the selected physical device.
    pub pipeline_cache: Vec<u8>,
    pub upscalers: UpscalerConfig,
    /// NVIDIA Reflex через `VK_NV_low_latency2`; без поддержки устройством работает как `Off`.
    pub low_latency: LowLatencyMode,
}

/// Optional vendor runtime configuration for temporal upscalers.
#[derive(Default)]
pub struct UpscalerConfig {
    /// Path to the signed AMD `amd_fidelityfx_loader_vk.dll` runtime on Windows.
    pub fsr_library: Option<std::path::PathBuf>,
    /// Path to the NVIDIA `nvngx_dlss.dll` feature runtime on Windows.
    pub dlss_library: Option<std::path::PathBuf>,
    /// NGX custom project GUID-like identifier, mutually exclusive with `dlss_application_id`.
    pub dlss_project_id: Option<String>,
    /// NVIDIA-provided NGX application ID, mutually exclusive with `dlss_project_id`.
    pub dlss_application_id: Option<u64>,
    /// Engine version supplied with `dlss_project_id`.
    pub dlss_engine_version: Option<String>,
}

/// Возможности выбранного физического устройства.
pub struct Capabilities {
    pub device_name: String,
    pub vendor_id: u32,
    pub api_version: u32,
    pub discrete: bool,
    pub tessellation_shader: bool,
    pub wireframe: bool,
    pub fsr: bool,
    pub dlss: bool,
    /// Включён ли NVIDIA Reflex: расширения, features и timeline semaphore поддержаны и созданы.
    pub low_latency: bool,
}

/// Результат проверки базовых Vulkan-требований для физического устройства.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceReport {
    pub name: String,
    pub vendor_id: u32,
    pub api_version: u32,
    pub discrete: bool,
    pub suitable: bool,
    pub reasons: Vec<String>,
}
