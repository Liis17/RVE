use ash::vk::{self, Handle as _};
use libloading::Library;
use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::Arc;

pub(crate) const CREATE_UPSCALE_TYPE: u64 = 0x0001_0000;
pub(crate) const CREATE_BACKEND_VK_TYPE: u64 = 0x0100_0003;
pub(crate) const DISPATCH_UPSCALE_TYPE: u64 = 0x0001_0001;

pub(crate) const UPSCALE_ENABLE_HDR: u32 = 1 << 0;
pub(crate) const RESOURCE_USAGE_UAV: u32 = 1 << 1;
pub(crate) const RESOURCE_USAGE_DEPTH_TARGET: u32 = 1 << 2;
pub(crate) const RESOURCE_TYPE_TEXTURE_2D: u32 = 2;
pub(crate) const RESOURCE_STATE_UNORDERED_ACCESS: u32 = 1 << 1;
pub(crate) const RESOURCE_STATE_PIXEL_COMPUTE_READ: u32 = (1 << 2) | (1 << 3);
pub(crate) const FORMAT_R16G16B16A16_FLOAT: u32 = 4;
pub(crate) const FORMAT_R16G16_FLOAT: u32 = 18;
pub(crate) const FORMAT_R32_FLOAT: u32 = 26;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FfxApiHeader {
    pub type_id: u64,
    pub next: *mut FfxApiHeader,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FfxDimensions2D {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FfxFloatCoords2D {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FfxResourceDescription {
    pub resource_type: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_count: u32,
    pub flags: u32,
    pub usage: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FfxResource {
    pub resource: *mut c_void,
    pub description: FfxResourceDescription,
    pub state: u32,
}

#[repr(C)]
pub(crate) struct FfxVkDeviceContext {
    pub device: *mut c_void,
    pub physical_device: *mut c_void,
    pub get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
}

#[repr(C)]
pub(crate) struct FfxCreateBackendVkDesc {
    pub header: FfxApiHeader,
    pub device_context: FfxVkDeviceContext,
}

#[repr(C)]
pub(crate) struct FfxCreateContextDescUpscale {
    pub header: FfxApiHeader,
    pub flags: u32,
    pub max_render_size: FfxDimensions2D,
    pub max_upscale_size: FfxDimensions2D,
    pub message: Option<unsafe extern "C" fn(u32, *const c_void)>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FfxDispatchDescUpscale {
    pub header: FfxApiHeader,
    pub command_list: *mut c_void,
    pub color: FfxResource,
    pub depth: FfxResource,
    pub motion_vectors: FfxResource,
    pub exposure: FfxResource,
    pub reactive: FfxResource,
    pub transparency_and_composition: FfxResource,
    pub output: FfxResource,
    pub jitter_offset: FfxFloatCoords2D,
    pub motion_vector_scale: FfxFloatCoords2D,
    pub render_size: FfxDimensions2D,
    pub upscale_size: FfxDimensions2D,
    pub enable_sharpening: u8,
    pub sharpness: f32,
    pub frame_time_delta_ms: f32,
    pub pre_exposure: f32,
    pub reset: u8,
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov_y_radians: f32,
    pub view_space_to_meters: f32,
    pub flags: u32,
}

pub(crate) fn resource(
    image: *mut c_void,
    format: u32,
    width: u32,
    height: u32,
    usage: u32,
    state: u32,
) -> FfxResource {
    FfxResource {
        resource: image,
        description: FfxResourceDescription {
            resource_type: RESOURCE_TYPE_TEXTURE_2D,
            format,
            width,
            height,
            depth: 1,
            mip_count: 1,
            flags: 0,
            usage,
        },
        state,
    }
}

pub(crate) fn null_resource() -> FfxResource {
    FfxResource {
        resource: ptr::null_mut(),
        description: FfxResourceDescription {
            resource_type: 0,
            format: 0,
            width: 0,
            height: 0,
            depth: 0,
            mip_count: 0,
            flags: 0,
            usage: 0,
        },
        state: 0,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_description(
    command_list: *mut c_void,
    color: FfxResource,
    depth: FfxResource,
    motion_vectors: FfxResource,
    exposure: FfxResource,
    output: FfxResource,
    jitter_pixels: [f32; 2],
    render_size: [u32; 2],
    upscale_size: [u32; 2],
    frame_time_delta_ms: f32,
    reset: bool,
    camera: [f32; 3],
) -> FfxDispatchDescUpscale {
    FfxDispatchDescUpscale {
        header: FfxApiHeader {
            type_id: DISPATCH_UPSCALE_TYPE,
            next: ptr::null_mut(),
        },
        command_list,
        color,
        depth,
        motion_vectors,
        exposure,
        reactive: null_resource(),
        transparency_and_composition: null_resource(),
        output,
        jitter_offset: FfxFloatCoords2D {
            x: -jitter_pixels[0],
            y: -jitter_pixels[1],
        },
        motion_vector_scale: FfxFloatCoords2D {
            x: render_size[0] as f32,
            y: render_size[1] as f32,
        },
        render_size: FfxDimensions2D {
            width: render_size[0],
            height: render_size[1],
        },
        upscale_size: FfxDimensions2D {
            width: upscale_size[0],
            height: upscale_size[1],
        },
        enable_sharpening: 0,
        sharpness: 0.0,
        frame_time_delta_ms,
        pre_exposure: 1.0,
        reset: u8::from(reset),
        camera_near: camera[0],
        camera_far: camera[1],
        camera_fov_y_radians: camera[2],
        view_space_to_meters: 1.0,
        flags: 0,
    }
}

pub(crate) fn exposure_value(exposure_ev: f32) -> f32 {
    exposure_ev.exp2()
}

type CreateContextFn =
    unsafe extern "C" fn(*mut *mut c_void, *mut FfxApiHeader, *const c_void) -> u32;
type DestroyContextFn = unsafe extern "C" fn(*mut *mut c_void, *const c_void) -> u32;
#[allow(dead_code)]
type ConfigureFn = unsafe extern "C" fn(*mut *mut c_void, *const FfxApiHeader) -> u32;
#[allow(dead_code)]
type QueryFn = unsafe extern "C" fn(*mut *mut c_void, *mut FfxApiHeader) -> u32;
type DispatchFn = unsafe extern "C" fn(*mut *mut c_void, *const FfxApiHeader) -> u32;

pub(crate) struct FfxApi {
    _library: Library,
    create_context: CreateContextFn,
    destroy_context: DestroyContextFn,
    #[allow(dead_code)]
    configure: ConfigureFn,
    #[allow(dead_code)]
    query: QueryFn,
    dispatch: DispatchFn,
}

impl FfxApi {
    pub(crate) fn load(path: &Path) -> Result<Arc<Self>, String> {
        // SAFETY: the project setting explicitly names a signed AMD FSR API loader DLL.
        let library = unsafe { Library::new(path) }
            .map_err(|error| format!("{}: {error}", path.display()))?;
        // SAFETY: these five exported C functions and signatures are defined by ffx_api.h.
        let create_context = unsafe {
            *library
                .get::<CreateContextFn>(b"ffxCreateContext\0")
                .map_err(|error| format!("ffxCreateContext: {error}"))?
        };
        // SAFETY: matching FFX API entry point.
        let destroy_context = unsafe {
            *library
                .get::<DestroyContextFn>(b"ffxDestroyContext\0")
                .map_err(|error| format!("ffxDestroyContext: {error}"))?
        };
        // SAFETY: matching FFX API entry point.
        let configure = unsafe {
            *library
                .get::<ConfigureFn>(b"ffxConfigure\0")
                .map_err(|error| format!("ffxConfigure: {error}"))?
        };
        // SAFETY: matching FFX API entry point.
        let query = unsafe {
            *library
                .get::<QueryFn>(b"ffxQuery\0")
                .map_err(|error| format!("ffxQuery: {error}"))?
        };
        // SAFETY: matching FFX API entry point.
        let dispatch = unsafe {
            *library
                .get::<DispatchFn>(b"ffxDispatch\0")
                .map_err(|error| format!("ffxDispatch: {error}"))?
        };
        Ok(Arc::new(Self {
            _library: library,
            create_context,
            destroy_context,
            configure,
            query,
            dispatch,
        }))
    }

    pub(crate) fn create_upscale_context(
        self: &Arc<Self>,
        device: vk::Device,
        physical_device: vk::PhysicalDevice,
        get_device_proc_addr: vk::PFN_vkGetDeviceProcAddr,
        render_size: [u32; 2],
        upscale_size: [u32; 2],
    ) -> Result<FfxContext, String> {
        let mut backend = Box::new(FfxCreateBackendVkDesc {
            header: FfxApiHeader {
                type_id: CREATE_BACKEND_VK_TYPE,
                next: ptr::null_mut(),
            },
            device_context: FfxVkDeviceContext {
                device: device.as_raw() as usize as *mut c_void,
                physical_device: physical_device.as_raw() as usize as *mut c_void,
                get_device_proc_addr,
            },
        });
        let mut desc = Box::new(FfxCreateContextDescUpscale {
            header: FfxApiHeader {
                type_id: CREATE_UPSCALE_TYPE,
                next: ptr::null_mut(),
            },
            flags: UPSCALE_ENABLE_HDR,
            max_render_size: FfxDimensions2D {
                width: render_size[0],
                height: render_size[1],
            },
            max_upscale_size: FfxDimensions2D {
                width: upscale_size[0],
                height: upscale_size[1],
            },
            message: None,
        });
        desc.header.next = &mut backend.header;
        let mut context = ptr::null_mut();
        // SAFETY: both chained descriptors are boxed and stay alive through FfxContext::drop.
        let status = unsafe { (self.create_context)(&mut context, &mut desc.header, ptr::null()) };
        if status != 0 || context.is_null() {
            if !context.is_null() {
                // SAFETY: a partially initialized context is returned by the same loader API.
                unsafe { (self.destroy_context)(&mut context, ptr::null()) };
            }
            return Err(format!("ffxCreateContext returned {status}"));
        }
        Ok(FfxContext {
            api: Arc::clone(self),
            context,
            _desc: desc,
            _backend: backend,
        })
    }
}

pub(crate) struct FfxContext {
    api: Arc<FfxApi>,
    context: *mut c_void,
    _desc: Box<FfxCreateContextDescUpscale>,
    _backend: Box<FfxCreateBackendVkDesc>,
}

// SAFETY: the FFX context and chained descriptors are owned by one Renderer and are only
// accessed through its mutable methods; moving that owner between threads does not alias them.
unsafe impl Send for FfxContext {}

impl FfxContext {
    pub(crate) fn dispatch(&mut self, desc: &FfxDispatchDescUpscale) -> Result<(), String> {
        // SAFETY: the context is live and the descriptor matches ffxDispatchDescUpscale.
        let status =
            unsafe { (self.api.dispatch)(&mut self.context, &desc.header as *const FfxApiHeader) };
        (status == 0)
            .then_some(())
            .ok_or_else(|| format!("ffxDispatch returned {status}"))
    }
}

impl Drop for FfxContext {
    fn drop(&mut self) {
        if !self.context.is_null() {
            // SAFETY: the GPU is idle or all submitted frame slots retired before this drop.
            let status = unsafe { (self.api.destroy_context)(&mut self.context, ptr::null()) };
            if status != 0 {
                log::warn!("ffxDestroyContext returned {status}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn ffx_c_struct_layout_matches_the_64_bit_api_headers() {
        assert_eq!(size_of::<FfxApiHeader>(), 16);
        assert_eq!(size_of::<FfxResourceDescription>(), 32);
        assert_eq!(size_of::<FfxResource>(), 48);
        assert_eq!(size_of::<FfxVkDeviceContext>(), 24);
        assert_eq!(size_of::<FfxCreateBackendVkDesc>(), 40);
        assert_eq!(size_of::<FfxCreateContextDescUpscale>(), 48);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, command_list), 16);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, color), 24);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, output), 312);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, jitter_offset), 360);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, frame_time_delta_ms), 400);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, reset), 408);
        assert_eq!(offset_of!(FfxDispatchDescUpscale, flags), 428);
        assert_eq!(size_of::<FfxDispatchDescUpscale>(), 432);
    }

    #[test]
    fn dispatch_parameters_match_render_resolution_inputs_and_exposure_contract() {
        let placeholder = ptr::dangling_mut::<c_void>();
        let color = resource(
            placeholder,
            FORMAT_R16G16B16A16_FLOAT,
            640,
            360,
            0,
            RESOURCE_STATE_PIXEL_COMPUTE_READ,
        );
        let depth = resource(
            placeholder,
            FORMAT_R32_FLOAT,
            640,
            360,
            RESOURCE_USAGE_DEPTH_TARGET,
            RESOURCE_STATE_PIXEL_COMPUTE_READ,
        );
        let motion = resource(
            placeholder,
            FORMAT_R16G16_FLOAT,
            640,
            360,
            0,
            RESOURCE_STATE_PIXEL_COMPUTE_READ,
        );
        let exposure = resource(
            placeholder,
            FORMAT_R32_FLOAT,
            1,
            1,
            0,
            RESOURCE_STATE_PIXEL_COMPUTE_READ,
        );
        let output = resource(
            placeholder,
            FORMAT_R16G16B16A16_FLOAT,
            1280,
            720,
            RESOURCE_USAGE_UAV,
            RESOURCE_STATE_PIXEL_COMPUTE_READ,
        );
        let desc = dispatch_description(
            placeholder,
            color,
            depth,
            motion,
            exposure,
            output,
            [0.25, -0.125],
            [640, 360],
            [1280, 720],
            16.0,
            true,
            [0.1, 100.0, std::f32::consts::FRAC_PI_3],
        );

        assert_eq!(desc.header.type_id, DISPATCH_UPSCALE_TYPE);
        assert_eq!(desc.command_list, placeholder);
        assert_eq!(desc.color.resource, placeholder);
        assert_eq!(desc.color.description.format, FORMAT_R16G16B16A16_FLOAT);
        assert_eq!(desc.depth.description.usage, RESOURCE_USAGE_DEPTH_TARGET);
        assert_eq!(desc.motion_vectors.description.format, FORMAT_R16G16_FLOAT);
        assert_eq!(desc.exposure.description.width, 1);
        assert_eq!(desc.exposure.description.height, 1);
        assert_eq!(desc.output.description.usage, RESOURCE_USAGE_UAV);
        assert_eq!(desc.jitter_offset.x, -0.25);
        assert_eq!(desc.jitter_offset.y, 0.125);
        assert_eq!(desc.motion_vector_scale.x, 640.0);
        assert_eq!(desc.motion_vector_scale.y, 360.0);
        assert_eq!(desc.render_size.width, 640);
        assert_eq!(desc.upscale_size.width, 1280);
        assert_eq!(desc.frame_time_delta_ms, 16.0);
        assert_eq!(desc.pre_exposure, 1.0);
        assert_eq!(desc.reset, 1);
        assert_eq!(desc.camera_near, 0.1);
        assert_eq!(desc.camera_far, 100.0);
        assert_eq!(desc.camera_fov_y_radians, std::f32::consts::FRAC_PI_3);
        assert_eq!(exposure_value(2.0), 4.0);
        assert_eq!(exposure_value(-1.0), 0.5);
    }
}
