//! Выбор и проверка физического устройства Vulkan, расширения и поддержка форматов.

use crate::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeInstanceFailure {
    NoDriver,
    Vulkan(vk::Result),
}

pub(crate) fn classify_probe_instance_failure(error: vk::Result) -> ProbeInstanceFailure {
    match error {
        vk::Result::ERROR_INCOMPATIBLE_DRIVER | vk::Result::ERROR_INITIALIZATION_FAILED => {
            ProbeInstanceFailure::NoDriver
        }
        error => ProbeInstanceFailure::Vulkan(error),
    }
}

/// Проверяет доступные Vulkan-устройства без создания окна или surface.
///
/// `suitable` учитывает Vulkan 1.3, dynamic rendering, synchronization2, extended dynamic state,
/// graphics queue и обязательные расширения устройства, включая swapchain.
/// Возможность present и совместимость с конкретным surface проверяются
/// отдельно в [`Renderer::new`]. Если loader недоступен, возвращается
/// `RenderError::Loader`; если instance не создаётся из-за отсутствующего
/// драйвера (`ERROR_INCOMPATIBLE_DRIVER` или `ERROR_INITIALIZATION_FAILED`),
/// либо физических устройств нет, возвращается `Ok(Vec::new())`. Прочие
/// ошибки probe возвращаются как `RenderError`, а устройства, которые не
/// проходят требования, представлены отчётами с `suitable == false`.
pub fn probe_devices() -> Result<Vec<DeviceReport>, RenderError> {
    let entry =
        engine_platform::load_vulkan().map_err(|error| RenderError::Loader(error.to_string()))?;
    let requested_extensions = deduplicate_names(
        engine_platform::INSTANCE_EXTENSIONS
            .iter()
            .map(|name| CString::from(*name))
            .collect(),
    );
    // SAFETY: extension enumeration only queries the loaded Vulkan loader.
    let available_extensions = unsafe { entry.enumerate_instance_extension_properties(None) }?;
    check_extensions(&requested_extensions, &available_extensions, "instance")?;

    let extension_pointers = requested_extensions
        .iter()
        .map(|name| name.as_ptr())
        .collect::<Vec<_>>();
    let application_info = vk::ApplicationInfo::default()
        .application_name(c"RVE")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"RVE")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::make_api_version(0, 1, 3, 0));
    let instance_info = vk::InstanceCreateInfo::default()
        .flags(engine_platform::INSTANCE_FLAGS)
        .application_info(&application_info)
        .enabled_extension_names(&extension_pointers);
    // SAFETY: extension pointers and application info remain alive through instance creation.
    let instance = match unsafe { entry.create_instance(&instance_info, None) } {
        Ok(instance) => instance,
        Err(error) => match classify_probe_instance_failure(error) {
            ProbeInstanceFailure::NoDriver => return Ok(Vec::new()),
            ProbeInstanceFailure::Vulkan(error) => return Err(RenderError::Vulkan(error)),
        },
    };

    let result = (|| {
        let required_device_extensions = required_device_extensions(&[]);
        // SAFETY: the instance is live and no surface is needed to enumerate physical devices.
        let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
        physical_devices
            .into_iter()
            .map(|physical_device| {
                inspect_device(&instance, physical_device, &required_device_extensions)
                    .map(|inspection| inspection.report)
            })
            .collect()
    })();

    // SAFETY: no device or other child objects were created from this probe instance.
    unsafe { instance.destroy_instance(None) };
    result
}

pub(crate) struct DeviceChoice {
    pub(crate) physical_device: vk::PhysicalDevice,
    pub(crate) queue_family_index: u32,
    pub(crate) surface_format: vk::SurfaceFormatKHR,
    pub(crate) device_name: String,
    pub(crate) vendor_id: u32,
    pub(crate) api_version: u32,
    pub(crate) discrete: bool,
    pub(crate) tessellation_shader: bool,
    pub(crate) wireframe: bool,
    pub(crate) texture_sampler_anisotropy: Option<f32>,
    /// `multiDrawIndirect` и `drawIndirectFirstInstance`: нужны occlusion culling.
    pub(crate) indirect_instancing: bool,
    pub(crate) timestamp_valid_bits: u32,
    pub(crate) timestamp_period: f32,
}

pub(crate) struct DeviceFacts {
    pub(crate) api_version: u32,
    pub(crate) dynamic_rendering: bool,
    pub(crate) synchronization2: bool,
    pub(crate) extended_dynamic_state: bool,
    pub(crate) supported_extensions: HashSet<String>,
    pub(crate) graphics_queue_families: Vec<bool>,
    pub(crate) compute_queue_families: Vec<bool>,
    pub(crate) storage_image_extended_formats: bool,
    pub(crate) offscreen_format_features: vk::FormatFeatureFlags,
    pub(crate) velocity_format_features: vk::FormatFeatureFlags,
    pub(crate) max_push_constants_size: u32,
}

pub(crate) struct DeviceEvaluation {
    pub(crate) reasons: Vec<String>,
    pub(crate) graphics_queue_family_indices: Vec<u32>,
}

pub(crate) struct DeviceInspection {
    pub(crate) report: DeviceReport,
    pub(crate) tessellation_shader: bool,
    pub(crate) wireframe: bool,
    pub(crate) texture_sampler_anisotropy: Option<f32>,
    pub(crate) indirect_instancing: bool,
    pub(crate) timestamp_period: f32,
    pub(crate) queue_families: Vec<vk::QueueFamilyProperties>,
    pub(crate) graphics_queue_family_indices: Vec<u32>,
}

pub(crate) fn required_device_extensions(extra: &[&CStr]) -> Vec<CString> {
    let mut required = extra
        .iter()
        .map(|name| CString::from(*name))
        .collect::<Vec<_>>();
    required.push(ash::khr::swapchain::NAME.to_owned());
    required.extend(
        engine_platform::DEVICE_EXTENSIONS
            .iter()
            .map(|name| CString::from(*name)),
    );
    deduplicate_names(required)
}

pub(crate) fn evaluate_device(
    facts: &DeviceFacts,
    required_extensions: &[CString],
) -> DeviceEvaluation {
    let mut reasons = Vec::new();
    if vk::api_version_major(facts.api_version) < 1
        || (vk::api_version_major(facts.api_version) == 1
            && vk::api_version_minor(facts.api_version) < 3)
    {
        reasons.push("Vulkan API ниже 1.3".to_owned());
    }
    if !facts.dynamic_rendering {
        reasons.push("не поддерживается dynamicRendering".to_owned());
    }
    if !facts.synchronization2 {
        reasons.push("не поддерживается synchronization2".to_owned());
    }
    if !facts.storage_image_extended_formats {
        reasons.push("не поддерживается shaderStorageImageExtendedFormats".to_owned());
    }
    let missing_offscreen = missing_offscreen_format_features(facts.offscreen_format_features);
    if !missing_offscreen.is_empty() {
        reasons.push(format!(
            "R16G16B16A16_SFLOAT не поддерживает HDR scene и compute storage: {}",
            missing_offscreen.join(", ")
        ));
    }
    let missing_velocity = missing_velocity_format_features(facts.velocity_format_features);
    if !missing_velocity.is_empty() {
        reasons.push(format!(
            "R16G16_SFLOAT не поддерживает velocity attachment: {}",
            missing_velocity.join(", ")
        ));
    }
    if facts.max_push_constants_size < PUSH_CONSTANTS_SIZE as u32 {
        reasons.push(format!(
            "maxPushConstantsSize меньше {} байт",
            PUSH_CONSTANTS_SIZE
        ));
    }
    if !facts.extended_dynamic_state {
        reasons.push("не поддерживается extendedDynamicState".to_owned());
    }
    for extension in required_extensions {
        let name = extension.to_string_lossy();
        if !facts.supported_extensions.contains(name.as_ref()) {
            reasons.push(format!("device extension недоступен: {name}"));
        }
    }
    let graphics_queue_family_indices = facts
        .graphics_queue_families
        .iter()
        .enumerate()
        .filter_map(|(index, supports_graphics)| {
            (*supports_graphics
                && facts
                    .compute_queue_families
                    .get(index)
                    .copied()
                    .unwrap_or(false))
            .then_some(index as u32)
        })
        .collect::<Vec<_>>();
    if graphics_queue_family_indices.is_empty() {
        if facts
            .graphics_queue_families
            .iter()
            .any(|supports| *supports)
        {
            reasons.push(NO_GRAPHICS_COMPUTE_QUEUE_FAMILY_REASON.to_owned());
        } else {
            reasons.push(NO_GRAPHICS_QUEUE_FAMILY_REASON.to_owned());
        }
    }
    DeviceEvaluation {
        reasons,
        graphics_queue_family_indices,
    }
}

pub(crate) fn inspect_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    required_extensions: &[CString],
) -> Result<DeviceInspection, RenderError> {
    // SAFETY: physical_device was returned by the live instance.
    let properties = unsafe { instance.get_physical_device_properties(physical_device) };
    let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    // SAFETY: device feature query uses a valid physical device and a properly initialized pNext chain.
    let (
        tessellation_shader,
        fill_mode_non_solid,
        storage_image_extended_formats,
        texture_sampler_anisotropy,
        indirect_instancing,
    ) = {
        let mut features_2 = vk::PhysicalDeviceFeatures2::default();
        // SAFETY: querying core features uses a valid physical device and null pNext chain.
        unsafe { instance.get_physical_device_features2(physical_device, &mut features_2) };
        (
            features_2.features.tessellation_shader != vk::FALSE,
            features_2.features.fill_mode_non_solid != vk::FALSE,
            features_2.features.shader_storage_image_extended_formats != vk::FALSE,
            sampler_anisotropy(
                features_2.features.sampler_anisotropy != vk::FALSE,
                properties.limits.max_sampler_anisotropy,
            ),
            features_2.features.multi_draw_indirect != vk::FALSE
                && features_2.features.draw_indirect_first_instance != vk::FALSE,
        )
    };
    let mut features_13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut extended_dynamic_state = vk::PhysicalDeviceExtendedDynamicStateFeaturesEXT::default();
    let mut features_2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut features_13)
        .push_next(&mut extended_dynamic_state);
    // SAFETY: querying Vulkan 1.3 and extension feature structures uses a valid pNext chain.
    unsafe { instance.get_physical_device_features2(physical_device, &mut features_2) };
    let dynamic_rendering = features_13.dynamic_rendering != vk::FALSE;
    let synchronization2 = features_13.synchronization2 != vk::FALSE;

    // SAFETY: extension enumeration is read-only for the live physical device.
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let supported_extensions = extensions
        .iter()
        .map(|extension| {
            // SAFETY: Vulkan extension property names are fixed-size null-terminated strings.
            unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let queue_families =
        // SAFETY: queue-family properties are read from the live physical device.
        unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
    let offscreen_format_features = unsafe {
        instance.get_physical_device_format_properties(physical_device, OFFSCREEN_FORMAT)
    }
    .optimal_tiling_features;
    let velocity_format_features =
        unsafe { instance.get_physical_device_format_properties(physical_device, VELOCITY_FORMAT) }
            .optimal_tiling_features;
    let facts = DeviceFacts {
        api_version: properties.api_version,
        dynamic_rendering,
        synchronization2,
        extended_dynamic_state: extended_dynamic_state.extended_dynamic_state != vk::FALSE,
        supported_extensions,
        graphics_queue_families: queue_families
            .iter()
            .map(|family| {
                family.queue_count > 0 && family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
            })
            .collect(),
        compute_queue_families: queue_families
            .iter()
            .map(|family| {
                family.queue_count > 0 && family.queue_flags.contains(vk::QueueFlags::COMPUTE)
            })
            .collect(),
        storage_image_extended_formats,
        offscreen_format_features,
        velocity_format_features,
        max_push_constants_size: properties.limits.max_push_constants_size,
    };
    let evaluation = evaluate_device(&facts, required_extensions);
    let srgb_features = unsafe {
        instance.get_physical_device_format_properties(physical_device, SRGB_TEXTURE_FORMAT)
    }
    .optimal_tiling_features;
    let linear_features = unsafe {
        instance.get_physical_device_format_properties(physical_device, LINEAR_TEXTURE_FORMAT)
    }
    .optimal_tiling_features;
    let mut reasons = evaluation.reasons;
    reasons.extend(cluster_device_limit_reasons(&properties.limits));
    reasons.extend(particle_gpu::particle_device_limit_reasons(
        &properties.limits,
    ));
    reasons.extend(texture_format_reasons(srgb_features, linear_features));
    reasons.extend(shadow_image_device_reasons(instance, physical_device));
    reasons.extend(velocity_image_device_reasons(instance, physical_device));
    let report = DeviceReport {
        name,
        vendor_id: properties.vendor_id,
        api_version: properties.api_version,
        discrete: properties.device_type == vk::PhysicalDeviceType::DISCRETE_GPU,
        suitable: reasons.is_empty(),
        reasons,
    };

    Ok(DeviceInspection {
        report,
        tessellation_shader,
        wireframe: fill_mode_non_solid,
        texture_sampler_anisotropy,
        indirect_instancing,
        timestamp_period: properties.limits.timestamp_period,
        queue_families,
        graphics_queue_family_indices: evaluation.graphics_queue_family_indices,
    })
}

pub(crate) fn cluster_device_limit_reasons(limits: &vk::PhysicalDeviceLimits) -> Vec<String> {
    let workgroup_count = (CLUSTER_COUNT as u32).div_ceil(CLUSTER_COMPUTE_LOCAL_SIZE);
    let mut reasons = Vec::new();
    if limits.max_compute_work_group_invocations < CLUSTER_COMPUTE_LOCAL_SIZE
        || limits.max_compute_work_group_size[0] < CLUSTER_COMPUTE_LOCAL_SIZE
        || limits.max_compute_work_group_count[0] < workgroup_count
    {
        reasons.push("лимиты compute workgroup не поддерживают clustered lighting".to_owned());
    }
    if limits.max_storage_buffer_range < CLUSTER_BUFFER_SIZE as u32 {
        reasons.push("maxStorageBufferRange меньше фиксированного cluster SSBO".to_owned());
    }
    if limits.max_per_stage_descriptor_storage_buffers < 3
        || limits.max_descriptor_set_storage_buffers < 4
    {
        reasons.push(
            "лимиты storage buffer descriptors не поддерживают clustered/local shadow lighting"
                .to_owned(),
        );
    }
    reasons
}

pub(crate) fn shadow_image_device_reasons(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<String> {
    // SAFETY: format support is queried for this physical device.
    let format_properties =
        unsafe { instance.get_physical_device_format_properties(physical_device, DEPTH_FORMAT) };
    let required_features = vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT
        | vk::FormatFeatureFlags::SAMPLED_IMAGE
        | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
    let mut reasons = Vec::new();
    if !format_properties
        .optimal_tiling_features
        .contains(required_features)
    {
        reasons.push(
            "D32_SFLOAT не поддерживает depth attachment и linear comparison sampling shadow maps"
                .to_owned(),
        );
    }
    // SAFETY: image-format properties are queried for the fixed depth/sample usage.
    match unsafe {
        instance.get_physical_device_image_format_properties(
            physical_device,
            DEPTH_FORMAT,
            vk::ImageType::TYPE_2D,
            vk::ImageTiling::OPTIMAL,
            vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageCreateFlags::empty(),
        )
    } {
        Ok(properties) if shadow_array_limits_suitable(&properties) => {}
        Ok(_) => reasons.push(format!(
            "D32_SFLOAT image limits do not support the {}-layer {}x{} local/directional shadow arrays",
            MAX_LOCAL_SHADOW_LAYERS, SHADOW_MAP_RESOLUTION, SHADOW_MAP_RESOLUTION
        )),
        Err(error) => reasons.push(format!(
            "D32_SFLOAT image usage does not support shadow arrays: {error}"
        )),
    }
    reasons
}

pub(crate) fn shadow_array_limits_suitable(properties: &vk::ImageFormatProperties) -> bool {
    properties.max_array_layers >= MAX_LOCAL_SHADOW_LAYERS as u32
        && properties.max_extent.width >= SHADOW_MAP_RESOLUTION
        && properties.max_extent.height >= SHADOW_MAP_RESOLUTION
}

pub(crate) fn velocity_image_device_reasons(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<String> {
    // SAFETY: image-format support is queried for the live physical device.
    match unsafe {
        instance.get_physical_device_image_format_properties(
            physical_device,
            VELOCITY_FORMAT,
            vk::ImageType::TYPE_2D,
            vk::ImageTiling::OPTIMAL,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
            vk::ImageCreateFlags::empty(),
        )
    } {
        Ok(_) => Vec::new(),
        Err(error) => vec![format!(
            "R16G16_SFLOAT image usage does not support velocity attachment sampling: {error}"
        )],
    }
}

pub(crate) fn best_device_index<T>(
    devices: &[T],
    is_suitable: impl Fn(&T) -> bool,
    is_discrete: impl Fn(&T) -> bool,
) -> Option<usize> {
    let mut best = None;
    for (index, device) in devices.iter().enumerate() {
        if !is_suitable(device) {
            continue;
        }
        if best.is_none_or(|best_index| is_discrete(device) && !is_discrete(&devices[best_index])) {
            best = Some(index);
        }
    }
    best
}

pub(crate) fn choose_device(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    required_extensions: &[CString],
) -> Result<DeviceChoice, RenderError> {
    // SAFETY: physical-device enumeration is read-only and the instance is alive.
    let physical_devices = unsafe { instance.enumerate_physical_devices() }?;
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    for physical_device in physical_devices {
        let inspection = inspect_device(instance, physical_device, required_extensions)?;
        let name = inspection.report.name.clone();
        let mut reasons = inspection.report.reasons.clone();
        if !inspection.report.suitable {
            if let Some(reason) = reasons
                .iter_mut()
                .find(|reason| reason.as_str() == NO_GRAPHICS_QUEUE_FAMILY_REASON)
            {
                *reason = NO_GRAPHICS_PRESENT_QUEUE_FAMILY_REASON.to_owned();
            }
            rejected.push(format!("{name}: {}", reasons.join(", ")));
            continue;
        }

        // SAFETY: format support is queried for this physical device.
        let ldr_properties =
            unsafe { instance.get_physical_device_format_properties(physical_device, LDR_FORMAT) };
        let missing_ldr_features =
            missing_post_format_features(ldr_properties.optimal_tiling_features);
        if !missing_ldr_features.is_empty() {
            reasons.push(format!(
                "R8G8B8A8_SRGB не поддерживает возможности post target: {}",
                missing_ldr_features.join(", ")
            ));
        }
        // SAFETY: the surface and physical device belong to the same live instance.
        let surface_capabilities = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
        }?;
        if !surface_capabilities
            .supported_usage_flags
            .contains(SWAPCHAIN_USAGE)
        {
            reasons.push("surface не поддерживает COLOR_ATTACHMENT".to_owned());
        }
        // SAFETY: surface query only reads capabilities for the live device and surface.
        let surface_formats = unsafe {
            surface_loader.get_physical_device_surface_formats(physical_device, surface)
        }?;
        // SAFETY: surface query only reads capabilities for the live device and surface.
        let present_modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
        }?;
        if present_modes.is_empty() || !present_modes.contains(&vk::PresentModeKHR::FIFO) {
            reasons.push("surface не предоставляет FIFO present mode".to_owned());
        }
        let surface_format = choose_surface_format(instance, physical_device, &surface_formats);
        if surface_format.is_none() {
            reasons.push("нет surface-формата с COLOR_ATTACHMENT".to_owned());
        }

        let mut queue_family_index = None;
        let mut timestamp_valid_bits = 0;
        for &index in &inspection.graphics_queue_family_indices {
            // SAFETY: family index is from this device's enumerated queue-family properties.
            if unsafe {
                surface_loader.get_physical_device_surface_support(physical_device, index, surface)
            }? {
                queue_family_index = Some(index);
                timestamp_valid_bits =
                    inspection.queue_families[index as usize].timestamp_valid_bits;
                break;
            }
        }
        if queue_family_index.is_none() {
            reasons.push("нет общей graphics + present queue family".to_owned());
        }

        if !reasons.is_empty() {
            rejected.push(format!("{name}: {}", reasons.join(", ")));
            continue;
        }
        candidates.push(DeviceChoice {
            physical_device,
            queue_family_index: queue_family_index.ok_or(RenderError::InvalidState(
                "не найдена очередь после проверки",
            ))?,
            surface_format: surface_format
                .ok_or(RenderError::InvalidState("не найден формат после проверки"))?,
            device_name: inspection.report.name,
            vendor_id: inspection.report.vendor_id,
            api_version: inspection.report.api_version,
            discrete: inspection.report.discrete,
            tessellation_shader: inspection.tessellation_shader,
            wireframe: inspection.wireframe,
            texture_sampler_anisotropy: inspection.texture_sampler_anisotropy,
            indirect_instancing: inspection.indirect_instancing,
            timestamp_valid_bits,
            timestamp_period: inspection.timestamp_period,
        });
    }
    best_device_index(&candidates, |_| true, |candidate| candidate.discrete)
        .map(|index| candidates.remove(index))
        .ok_or(RenderError::NoSuitableDevice(rejected))
}

pub(crate) fn missing_texture_format_features(
    supported: vk::FormatFeatureFlags,
) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
        (vk::FormatFeatureFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (vk::FormatFeatureFlags::TRANSFER_DST, "TRANSFER_DST"),
        (vk::FormatFeatureFlags::BLIT_SRC, "BLIT_SRC"),
        (vk::FormatFeatureFlags::BLIT_DST, "BLIT_DST"),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

pub(crate) fn missing_post_format_features(supported: vk::FormatFeatureFlags) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (vk::FormatFeatureFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

pub(crate) fn missing_offscreen_format_features(
    supported: vk::FormatFeatureFlags,
) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (
            vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND,
            "COLOR_ATTACHMENT_BLEND",
        ),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
        (
            vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "SAMPLED_IMAGE_FILTER_LINEAR",
        ),
        (vk::FormatFeatureFlags::STORAGE_IMAGE, "STORAGE_IMAGE"),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

pub(crate) fn missing_velocity_format_features(
    supported: vk::FormatFeatureFlags,
) -> Vec<&'static str> {
    [
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, "SAMPLED_IMAGE"),
    ]
    .into_iter()
    .filter_map(|(feature, name)| (!supported.contains(feature)).then_some(name))
    .collect()
}

/// Проверяет, что устройство способно на NVIDIA Reflex: расширения, `presentId` и timeline semaphore.
pub(crate) fn low_latency_support(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<(), String> {
    // SAFETY: the physical device was enumerated from this live instance.
    let available = unsafe { instance.enumerate_device_extension_properties(physical_device) }
        .map_err(|error| format!("не удалось прочитать расширения устройства: {error:?}"))?;
    let requested = low_latency_device_extensions()
        .into_iter()
        .map(CString::from)
        .collect::<Vec<_>>();
    let missing = missing_extensions(&requested, &available, "NVIDIA Reflex");
    if !missing.is_empty() {
        return Err(missing.join(", "));
    }
    let mut present_id = vk::PhysicalDevicePresentIdFeaturesKHR::default();
    let mut vulkan12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut features = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut present_id)
        .push_next(&mut vulkan12);
    // SAFETY: the physical device belongs to the live instance; both feature structs are chained for the call.
    unsafe { instance.get_physical_device_features2(physical_device, &mut features) };
    if present_id.present_id != vk::TRUE {
        return Err("feature presentId недоступна".to_owned());
    }
    if vulkan12.timeline_semaphore != vk::TRUE {
        return Err("feature timelineSemaphore недоступна".to_owned());
    }
    Ok(())
}

pub(crate) fn deduplicate_names(names: Vec<CString>) -> Vec<CString> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.as_bytes().to_vec()))
        .collect()
}

pub(crate) fn check_extensions(
    requested: &[CString],
    available: &[vk::ExtensionProperties],
    kind: &str,
) -> Result<(), RenderError> {
    let missing = missing_extensions(requested, available, kind);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(RenderError::Unsupported(missing.join(", ")))
    }
}

pub(crate) fn missing_extensions(
    requested: &[CString],
    available: &[vk::ExtensionProperties],
    kind: &str,
) -> Vec<String> {
    let available_names = available
        .iter()
        .map(|extension| {
            // SAFETY: Vulkan extension property names are null-terminated fixed-size strings.
            unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }
                .to_bytes()
                .to_vec()
        })
        .collect::<HashSet<_>>();
    requested
        .iter()
        .filter(|name| !available_names.contains(name.as_bytes()))
        .map(|name| format!("{} extension недоступен: {}", kind, name.to_string_lossy()))
        .collect()
}
