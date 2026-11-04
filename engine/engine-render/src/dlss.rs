use ash::vk;

pub(super) fn create_flags(render_extent: vk::Extent2D, output_extent: vk::Extent2D) -> i32 {
    1 | if render_extent != output_extent { 2 } else { 0 } // IsHDR | MVLowRes
}

pub(super) fn ngx_jitter_offset(jitter: [f32; 2]) -> [f32; 2] {
    [-jitter[0], -jitter[1]]
}

pub(super) fn quality_mode(render_extent: vk::Extent2D, output_extent: vk::Extent2D) -> i32 {
    if render_extent == output_extent {
        return 5; // DLAA
    }
    let render_area = f64::from(render_extent.width) * f64::from(render_extent.height);
    let output_area = f64::from(output_extent.width) * f64::from(output_extent.height);
    let scale = (render_area / output_area).sqrt();
    [
        (0, 0.5),   // MaxPerf
        (1, 0.58),  // Balanced
        (2, 0.667), // MaxQuality
        (3, 0.333), // UltraPerformance
        (4, 0.77),  // UltraQuality
    ]
    .into_iter()
    .min_by(|(_, left), (_, right)| (scale - left).abs().total_cmp(&(scale - right).abs()))
    .expect("DLSS has at least one quality mode")
    .0
}

pub(super) fn validate_identity(
    project_id: Option<&str>,
    application_id: Option<u64>,
    engine_version: Option<&str>,
) -> Result<(), &'static str> {
    if project_id.is_some() == application_id.is_some() {
        return Err("configure exactly one of dlss_project_id or dlss_application_id");
    }
    if let Some(project_id) = project_id {
        let bytes = project_id.as_bytes();
        if bytes.len() != 36
            || !bytes.iter().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
        {
            return Err("dlss_project_id must be a GUID-like hexadecimal identifier");
        }
        let version = engine_version.unwrap_or("").trim();
        if version.is_empty() || version.len() > 64 || version.contains('\0') {
            return Err("dlss_engine_version must contain 1 to 64 non-NUL characters");
        }
    } else if application_id == Some(0) {
        return Err("dlss_application_id must be greater than zero");
    } else if engine_version.is_some() {
        return Err("dlss_engine_version requires dlss_project_id");
    }
    Ok(())
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct NgxImageViewInfo {
    pub image_view: u64,
    pub image: u64,
    pub subresource_range: vk::ImageSubresourceRange,
    pub format: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct NgxBufferInfo {
    pub buffer: u64,
    pub size_in_bytes: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) union NgxResourceUnion {
    pub image_view: NgxImageViewInfo,
    pub buffer: NgxBufferInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct NgxResourceVk {
    pub resource: NgxResourceUnion,
    pub resource_type: i32,
    pub read_write: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxProjectIdDescription {
    project_id: *const i8,
    engine_type: i32,
    engine_version: *const i8,
}

#[repr(C)]
#[derive(Clone, Copy)]
union NgxApplicationIdentifierValue {
    project: NgxProjectIdDescription,
    application_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxApplicationIdentifier {
    identifier_type: i32,
    value: NgxApplicationIdentifierValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxPathListInfo {
    path: *const *const u16,
    length: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxLoggingInfo {
    callback: *const std::ffi::c_void,
    minimum_logging_level: i32,
    disable_other_logging_sinks: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxFeatureCommonInfo {
    path_list_info: NgxPathListInfo,
    internal_data: *mut std::ffi::c_void,
    logging_info: NgxLoggingInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxFeatureDiscoveryInfo {
    sdk_version: i32,
    feature_id: i32,
    identifier: NgxApplicationIdentifier,
    application_data_path: *const u16,
    feature_info: *const NgxFeatureCommonInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NgxFeatureRequirement {
    feature_supported: i32,
    min_hw_architecture: u32,
    min_os_version: [i8; 255],
}

impl Default for NgxFeatureRequirement {
    fn default() -> Self {
        Self {
            feature_supported: 0,
            min_hw_architecture: 0,
            min_os_version: [0; 255],
        }
    }
}

#[cfg(all(windows, feature = "dlss"))]
mod runtime {
    use super::*;
    use std::ffi::{CStr, CString, OsStr};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;

    const SUCCESS: i32 = 1;
    const FEATURE_SUPER_SAMPLING: i32 = 1;
    const API_VERSION: i32 = 0x15;

    #[link(name = "nvsdk_ngx_s", kind = "static")]
    unsafe extern "C" {
        fn NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements(
            info: *const NgxFeatureDiscoveryInfo,
            count: *mut u32,
            properties: *mut *mut vk::ExtensionProperties,
        ) -> i32;
        fn NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements(
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
            info: *const NgxFeatureDiscoveryInfo,
            count: *mut u32,
            properties: *mut *mut vk::ExtensionProperties,
        ) -> i32;
        fn NVSDK_NGX_VULKAN_GetFeatureRequirements(
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
            info: *const NgxFeatureDiscoveryInfo,
            requirement: *mut NgxFeatureRequirement,
        ) -> i32;
        fn NVSDK_NGX_VULKAN_Init_with_ProjectID(
            project_id: *const i8,
            engine_type: i32,
            engine_version: *const i8,
            application_data_path: *const u16,
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
            device: vk::Device,
            get_instance_proc_addr: *const std::ffi::c_void,
            get_device_proc_addr: *const std::ffi::c_void,
            feature_info: *const NgxFeatureCommonInfo,
            sdk_version: i32,
        ) -> i32;
        fn NVSDK_NGX_VULKAN_Init(
            application_id: u64,
            application_data_path: *const u16,
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
            device: vk::Device,
            get_instance_proc_addr: *const std::ffi::c_void,
            get_device_proc_addr: *const std::ffi::c_void,
            feature_info: *const NgxFeatureCommonInfo,
            sdk_version: i32,
        ) -> i32;
        fn NVSDK_NGX_VULKAN_Shutdown1(device: vk::Device) -> i32;
        fn NVSDK_NGX_VULKAN_AllocateParameters(parameters: *mut *mut std::ffi::c_void) -> i32;
        fn NVSDK_NGX_VULKAN_DestroyParameters(parameters: *mut std::ffi::c_void) -> i32;
        fn NVSDK_NGX_Parameter_SetUI(
            parameters: *mut std::ffi::c_void,
            name: *const i8,
            value: u32,
        );
        fn NVSDK_NGX_Parameter_SetI(parameters: *mut std::ffi::c_void, name: *const i8, value: i32);
        fn NVSDK_NGX_Parameter_SetF(parameters: *mut std::ffi::c_void, name: *const i8, value: f32);
        fn NVSDK_NGX_Parameter_SetVoidPointer(
            parameters: *mut std::ffi::c_void,
            name: *const i8,
            value: *mut std::ffi::c_void,
        );
        fn NVSDK_NGX_VULKAN_CreateFeature1(
            device: vk::Device,
            command_buffer: vk::CommandBuffer,
            feature_id: i32,
            parameters: *mut std::ffi::c_void,
            feature: *mut *mut std::ffi::c_void,
        ) -> i32;
        fn NVSDK_NGX_VULKAN_ReleaseFeature(feature: *mut std::ffi::c_void) -> i32;
        fn NVSDK_NGX_VULKAN_EvaluateFeature_C(
            command_buffer: vk::CommandBuffer,
            feature: *const std::ffi::c_void,
            parameters: *const std::ffi::c_void,
            callback: *const std::ffi::c_void,
        ) -> i32;
    }

    pub(crate) struct NgxApi {
        project_id: Option<CString>,
        application_id: Option<u64>,
        engine_version: Option<CString>,
        application_data_path: Vec<u16>,
        feature_path: Vec<u16>,
        device: Option<vk::Device>,
    }

    impl NgxApi {
        pub fn new(
            project_id: Option<&str>,
            application_id: Option<u64>,
            engine_version: Option<&str>,
            library_path: &Path,
        ) -> Result<Self, String> {
            validate_identity(project_id, application_id, engine_version).map_err(str::to_owned)?;
            if !library_path.is_file() {
                return Err(format!(
                    "DLSS runtime DLL is missing: {}",
                    library_path.display()
                ));
            }
            let feature_directory = library_path
                .parent()
                .ok_or_else(|| "DLSS runtime path has no parent directory".to_owned())?;
            let application_data_path = ngx_application_data_path()?;
            Ok(Self {
                project_id: project_id
                    .map(CString::new)
                    .transpose()
                    .map_err(|_| "DLSS project ID contains NUL".to_owned())?,
                application_id,
                engine_version: engine_version
                    .map(CString::new)
                    .transpose()
                    .map_err(|_| "DLSS engine version contains NUL".to_owned())?,
                application_data_path: wide_path(&application_data_path),
                feature_path: wide_path(feature_directory),
                device: None,
            })
        }

        pub fn instance_extensions(&self) -> Result<Vec<CString>, String> {
            self.with_info(|info| {
                let mut count = 0;
                let mut properties = ptr::null_mut();
                // SAFETY: NGX fills the count and points to its extension list for this discovery struct.
                let result = unsafe {
                    NVSDK_NGX_VULKAN_GetFeatureInstanceExtensionRequirements(
                        info,
                        &mut count,
                        &mut properties,
                    )
                };
                read_extension_properties(result, count, properties, "instance")
            })
        }

        pub fn device_extensions(
            &self,
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
        ) -> Result<Vec<CString>, String> {
            self.with_info(|info| {
                let mut count = 0;
                let mut properties = ptr::null_mut();
                // SAFETY: handles refer to this live Vulkan instance and physical device.
                let result = unsafe {
                    NVSDK_NGX_VULKAN_GetFeatureDeviceExtensionRequirements(
                        instance,
                        physical_device,
                        info,
                        &mut count,
                        &mut properties,
                    )
                };
                read_extension_properties(result, count, properties, "device")
            })
        }

        pub fn check_support(
            &self,
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
        ) -> Result<(), String> {
            self.with_info(|info| {
                let mut requirement = NgxFeatureRequirement::default();
                // SAFETY: handles refer to this live Vulkan instance and physical device.
                let result = unsafe {
                    NVSDK_NGX_VULKAN_GetFeatureRequirements(
                        instance,
                        physical_device,
                        info,
                        &mut requirement,
                    )
                };
                check_result(result, "query DLSS feature support")?;
                if requirement.feature_supported != 0 {
                    return Err(format!(
                        "DLSS feature is unsupported (NGX support flags {:#x})",
                        requirement.feature_supported
                    ));
                }
                Ok(())
            })
        }

        pub fn initialize(
            &mut self,
            instance: vk::Instance,
            physical_device: vk::PhysicalDevice,
            device: vk::Device,
        ) -> Result<(), String> {
            let result = self.with_info(|info| {
                if let (Some(project_id), Some(engine_version)) =
                    (self.project_id.as_ref(), self.engine_version.as_ref())
                {
                    // SAFETY: the strings, feature-info, and Vulkan handles remain valid for the call.
                    unsafe {
                        NVSDK_NGX_VULKAN_Init_with_ProjectID(
                            project_id.as_ptr(),
                            0,
                            engine_version.as_ptr(),
                            self.application_data_path.as_ptr(),
                            instance,
                            physical_device,
                            device,
                            ptr::null(),
                            ptr::null(),
                            info.feature_info,
                            API_VERSION,
                        )
                    }
                } else {
                    // SAFETY: the app-data path, feature-info, and Vulkan handles remain valid for the call.
                    unsafe {
                        NVSDK_NGX_VULKAN_Init(
                            self.application_id.expect("validated NGX application ID"),
                            self.application_data_path.as_ptr(),
                            instance,
                            physical_device,
                            device,
                            ptr::null(),
                            ptr::null(),
                            info.feature_info,
                            API_VERSION,
                        )
                    }
                }
            });
            check_result(result, "initialize NGX")?;
            self.device = Some(device);
            Ok(())
        }

        pub fn evaluate(
            &self,
            device: vk::Device,
            command_buffer: vk::CommandBuffer,
            feature: &mut Option<usize>,
            render_extent: vk::Extent2D,
            output_extent: vk::Extent2D,
            color: &NgxResourceVk,
            depth: &NgxResourceVk,
            motion_vectors: &NgxResourceVk,
            exposure: &NgxResourceVk,
            output: &NgxResourceVk,
            jitter: [f32; 2],
            motion_scale: [f32; 2],
            reset: bool,
            frame_time_delta_ms: f32,
        ) -> Result<(), String> {
            let mut raw_parameters = ptr::null_mut();
            // SAFETY: NGX initializes the opaque parameter-map pointer on success.
            check_result(
                unsafe { NVSDK_NGX_VULKAN_AllocateParameters(&mut raw_parameters) },
                "allocate NGX parameters",
            )?;
            if raw_parameters.is_null() {
                return Err("NGX returned a null parameter map".to_owned());
            }
            let parameters = ParameterGuard(raw_parameters);
            let parameters = parameters.0;
            set_ui(parameters, c"Width", render_extent.width);
            set_ui(parameters, c"Height", render_extent.height);
            set_ui(parameters, c"OutWidth", output_extent.width);
            set_ui(parameters, c"OutHeight", output_extent.height);
            set_i(
                parameters,
                c"PerfQualityValue",
                quality_mode(render_extent, output_extent),
            );
            set_i(
                parameters,
                c"DLSS.Feature.Create.Flags",
                create_flags(render_extent, output_extent),
            );
            set_i(parameters, c"DLSS.Enable.Output.Subrects", 0);
            set_ui(parameters, c"CreationNodeMask", 1);
            set_ui(parameters, c"VisibilityNodeMask", 1);
            if feature.is_none() {
                let mut handle = ptr::null_mut();
                // SAFETY: the command buffer is recording and all create parameters are set above.
                check_result(
                    unsafe {
                        NVSDK_NGX_VULKAN_CreateFeature1(
                            device,
                            command_buffer,
                            FEATURE_SUPER_SAMPLING,
                            parameters,
                            &mut handle,
                        )
                    },
                    "create DLSS feature",
                )?;
                if handle.is_null() {
                    return Err("NGX returned a null DLSS feature handle".to_owned());
                }
                *feature = Some(handle as usize);
            }
            set_pointer(parameters, c"Color", color);
            set_pointer(parameters, c"Output", output);
            set_pointer(parameters, c"Depth", depth);
            set_pointer(parameters, c"MotionVectors", motion_vectors);
            set_pointer(parameters, c"ExposureTexture", exposure);
            set_ui(
                parameters,
                c"DLSS.Render.Subrect.Dimensions.Width",
                render_extent.width,
            );
            set_ui(
                parameters,
                c"DLSS.Render.Subrect.Dimensions.Height",
                render_extent.height,
            );
            set_f(parameters, c"Jitter.Offset.X", jitter[0]);
            set_f(parameters, c"Jitter.Offset.Y", jitter[1]);
            set_i(parameters, c"Reset", i32::from(reset));
            set_f(parameters, c"MV.Scale.X", motion_scale[0]);
            set_f(parameters, c"MV.Scale.Y", motion_scale[1]);
            set_f(parameters, c"DLSS.Pre.Exposure", 1.0);
            set_f(parameters, c"DLSS.Exposure.Scale", 1.0);
            set_f(parameters, c"FrameTimeDeltaInMsec", frame_time_delta_ms);
            let handle = (*feature).expect("DLSS feature created before evaluation") as *const _;
            // SAFETY: resource descriptors remain alive through this synchronous NGX evaluation call.
            check_result(
                unsafe {
                    NVSDK_NGX_VULKAN_EvaluateFeature_C(
                        command_buffer,
                        handle,
                        parameters,
                        ptr::null(),
                    )
                },
                "evaluate DLSS feature",
            )
        }

        fn with_info<T>(&self, callback: impl FnOnce(&NgxFeatureDiscoveryInfo) -> T) -> T {
            let path_list = [self.feature_path.as_ptr()];
            let common_info = NgxFeatureCommonInfo {
                path_list_info: NgxPathListInfo {
                    path: path_list.as_ptr(),
                    length: path_list.len() as u32,
                },
                internal_data: ptr::null_mut(),
                logging_info: NgxLoggingInfo {
                    callback: ptr::null(),
                    minimum_logging_level: 0,
                    disable_other_logging_sinks: false,
                },
            };
            let identifier = if let (Some(project_id), Some(engine_version)) =
                (self.project_id.as_ref(), self.engine_version.as_ref())
            {
                NgxApplicationIdentifier {
                    identifier_type: 1,
                    value: NgxApplicationIdentifierValue {
                        project: NgxProjectIdDescription {
                            project_id: project_id.as_ptr(),
                            engine_type: 0,
                            engine_version: engine_version.as_ptr(),
                        },
                    },
                }
            } else {
                NgxApplicationIdentifier {
                    identifier_type: 0,
                    value: NgxApplicationIdentifierValue {
                        application_id: self.application_id.expect("validated NGX ID"),
                    },
                }
            };
            let info = NgxFeatureDiscoveryInfo {
                sdk_version: API_VERSION,
                feature_id: FEATURE_SUPER_SAMPLING,
                identifier,
                application_data_path: self.application_data_path.as_ptr(),
                feature_info: &common_info,
            };
            callback(&info)
        }
    }

    impl Drop for NgxApi {
        fn drop(&mut self) {
            if let Some(device) = self.device.take() {
                // SAFETY: renderer waits for device idle and releases all feature handles before NGX shutdown.
                let _ = unsafe { NVSDK_NGX_VULKAN_Shutdown1(device) };
            }
        }
    }

    struct ParameterGuard(*mut std::ffi::c_void);

    impl Drop for ParameterGuard {
        fn drop(&mut self) {
            // SAFETY: this map was allocated by NGX and has not been destroyed yet.
            let _ = unsafe { NVSDK_NGX_VULKAN_DestroyParameters(self.0) };
        }
    }

    fn read_extension_properties(
        result: i32,
        count: u32,
        properties: *mut vk::ExtensionProperties,
        kind: &str,
    ) -> Result<Vec<CString>, String> {
        check_result(result, &format!("query NGX {kind} extensions"))?;
        if count == 0 {
            return Ok(Vec::new());
        }
        if properties.is_null() {
            return Err(format!("NGX returned a null {kind} extension list"));
        }
        // SAFETY: NGX returned a property pointer with `count` entries for this completed query.
        let properties = unsafe { std::slice::from_raw_parts(properties, count as usize) };
        properties
            .iter()
            .map(|property| {
                // SAFETY: Vulkan extension properties contain a fixed-size null-terminated name.
                Ok(unsafe { CStr::from_ptr(property.extension_name.as_ptr()) }.to_owned())
            })
            .collect()
    }

    fn check_result(result: i32, operation: &str) -> Result<(), String> {
        if result == SUCCESS {
            Ok(())
        } else {
            Err(format!("{operation} failed with NGX code {result:#010x}"))
        }
    }

    fn set_ui(parameters: *mut std::ffi::c_void, name: &CStr, value: u32) {
        // SAFETY: the NGX parameter map is live and each name is a static C string.
        unsafe { NVSDK_NGX_Parameter_SetUI(parameters, name.as_ptr(), value) };
    }

    fn set_i(parameters: *mut std::ffi::c_void, name: &CStr, value: i32) {
        // SAFETY: the NGX parameter map is live and each name is a static C string.
        unsafe { NVSDK_NGX_Parameter_SetI(parameters, name.as_ptr(), value) };
    }

    fn set_f(parameters: *mut std::ffi::c_void, name: &CStr, value: f32) {
        // SAFETY: the NGX parameter map is live and each name is a static C string.
        unsafe { NVSDK_NGX_Parameter_SetF(parameters, name.as_ptr(), value) };
    }

    fn set_pointer(parameters: *mut std::ffi::c_void, name: &CStr, value: &NgxResourceVk) {
        // SAFETY: NGX consumes the resource descriptor synchronously during create/evaluate calls.
        unsafe {
            NVSDK_NGX_Parameter_SetVoidPointer(
                parameters,
                name.as_ptr(),
                (value as *const NgxResourceVk).cast_mut().cast(),
            )
        };
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        OsStr::new(path).encode_wide().chain(Some(0)).collect()
    }

    fn ngx_application_data_path() -> Result<PathBuf, String> {
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join("RVE").join("NGX");
        std::fs::create_dir_all(&path).map_err(|error| {
            format!(
                "could not create NGX application data directory {}: {error}",
                path.display()
            )
        })?;
        Ok(path)
    }

    pub(crate) fn release_feature(feature: usize) {
        // SAFETY: the feature was created by NGX and is released only after all its submitted frames complete.
        let _ = unsafe { NVSDK_NGX_VULKAN_ReleaseFeature(feature as *mut std::ffi::c_void) };
    }
}

#[cfg(all(windows, feature = "dlss"))]
pub(super) use runtime::{NgxApi, release_feature};

#[cfg(test)]
mod tests {
    use super::{
        NgxFeatureCommonInfo, NgxFeatureDiscoveryInfo, NgxFeatureRequirement, NgxResourceVk,
        create_flags, ngx_jitter_offset, quality_mode, validate_identity,
    };
    use ash::vk;
    use std::mem::{offset_of, size_of};

    #[test]
    fn dlss_mode_and_flags_follow_scene_scale_and_input_contract() {
        assert_eq!(
            quality_mode(
                vk::Extent2D {
                    width: 1280,
                    height: 720
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720
                }
            ),
            5
        );
        assert_eq!(
            quality_mode(
                vk::Extent2D {
                    width: 640,
                    height: 360
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720
                }
            ),
            0
        );
        assert_eq!(
            quality_mode(
                vk::Extent2D {
                    width: 426,
                    height: 240,
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720,
                },
            ),
            3
        );
        assert_eq!(
            quality_mode(
                vk::Extent2D {
                    width: 742,
                    height: 417
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720
                }
            ),
            1
        );
        assert_eq!(
            quality_mode(
                vk::Extent2D {
                    width: 854,
                    height: 480
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720
                }
            ),
            2
        );
        assert_eq!(
            quality_mode(
                vk::Extent2D {
                    width: 986,
                    height: 554
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720
                }
            ),
            4
        );
        let flags = create_flags(
            vk::Extent2D {
                width: 640,
                height: 360,
            },
            vk::Extent2D {
                width: 1280,
                height: 720,
            },
        );
        assert_eq!(flags & 0b11, 0b11);
        assert_eq!(flags & (1 << 2), 0); // motion vectors are unjittered
        assert_eq!(flags & (1 << 3), 0); // depth is not inverted
        assert_eq!(
            create_flags(
                vk::Extent2D {
                    width: 1280,
                    height: 720,
                },
                vk::Extent2D {
                    width: 1280,
                    height: 720,
                },
            ),
            1
        );
    }

    #[test]
    fn ngx_vk_resource_layout_matches_x64_header_layout() {
        if cfg!(target_pointer_width = "64") {
            assert_eq!(size_of::<NgxResourceVk>(), 56);
            assert_eq!(offset_of!(NgxResourceVk, resource_type), 48);
            assert_eq!(offset_of!(NgxResourceVk, read_write), 52);
            assert_eq!(size_of::<NgxFeatureCommonInfo>(), 40);
            assert_eq!(size_of::<NgxFeatureDiscoveryInfo>(), 56);
            assert_eq!(
                offset_of!(NgxFeatureDiscoveryInfo, application_data_path),
                40
            );
            assert_eq!(offset_of!(NgxFeatureDiscoveryInfo, feature_info), 48);
            assert_eq!(size_of::<NgxFeatureRequirement>(), 264);
        }
    }

    #[test]
    fn ngx_jitter_is_the_negative_of_the_projection_offset() {
        assert_eq!(ngx_jitter_offset([0.25, -0.5]), [-0.25, 0.5]);
        assert_eq!(ngx_jitter_offset([0.0, 0.0]), [0.0, 0.0]);
    }

    #[test]
    fn dlss_identity_requires_exactly_one_valid_identifier() {
        let project = "a0f57b54-1daf-4934-90ae-c4035c19df04";
        assert!(validate_identity(Some(project), None, Some("0.1.0")).is_ok());
        assert!(validate_identity(None, Some(42), None).is_ok());
        assert!(validate_identity(None, None, None).is_err());
        assert!(validate_identity(Some(project), Some(42), Some("0.1.0")).is_err());
        assert!(validate_identity(Some("not-a-guid"), None, Some("0.1.0")).is_err());
        assert!(validate_identity(None, Some(0), None).is_err());
    }
}
