//! C-compatible types shared by an RVE host and a game script library.

use std::ffi::c_void;

/// Version of the script ABI. It remains provisional until the API is finalized.
pub const RVE_SCRIPT_ABI_VERSION: u32 = 1;

/// Value tag for a boolean.
pub const RVE_VALUE_BOOL: u32 = 0;
/// Value tag for a signed 64-bit integer.
pub const RVE_VALUE_INT: u32 = 1;
/// Value tag for a 64-bit floating-point number.
pub const RVE_VALUE_FLOAT: u32 = 2;
/// Value tag for a borrowed UTF-8 string.
pub const RVE_VALUE_STR: u32 = 3;
/// Value tag for a borrowed array of values.
pub const RVE_VALUE_ARRAY: u32 = 4;

/// Informational log severity.
pub const RVE_LOG_INFO: u32 = 0;
/// Warning log severity.
pub const RVE_LOG_WARN: u32 = 1;
/// Error log severity.
pub const RVE_LOG_ERROR: u32 = 2;

/// ABI header read before the host inspects any module table fields.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RveHeader {
    /// ABI version expected by the host.
    pub abi_version: u32,
    /// Size of this header in bytes.
    pub header_size: u32,
}

/// A borrowed UTF-8 byte sequence. The pointed-to bytes must remain valid for the call.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RveStr {
    /// Pointer to the first UTF-8 byte, or null when `len` is zero.
    pub ptr: *const u8,
    /// Number of UTF-8 bytes.
    pub len: usize,
}

impl RveStr {
    /// Creates an empty borrowed string.
    pub const fn empty() -> Self {
        Self {
            ptr: std::ptr::null(),
            len: 0,
        }
    }

    /// Borrows a process-lifetime string.
    pub const fn from_static(value: &'static str) -> Self {
        Self {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }

    /// Borrows a UTF-8 string for the duration of an ABI call.
    pub fn from_str_ref(value: &str) -> Self {
        Self {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }

    /// Reads the borrowed bytes as UTF-8.
    ///
    /// # Safety
    /// For nonzero `len`, `ptr` must be readable for `len` bytes for the returned lifetime.
    pub unsafe fn to_str<'a>(self) -> Result<&'a str, std::str::Utf8Error> {
        if self.len == 0 {
            return Ok("");
        }
        // SAFETY: upheld by the caller's contract.
        let bytes = unsafe { std::slice::from_raw_parts(self.ptr, self.len) };
        std::str::from_utf8(bytes)
    }
}

/// Borrowed array payload for [`RveValue`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RveArray {
    /// Pointer to the first element, or null when `len` is zero.
    pub ptr: *const RveValue,
    /// Number of elements.
    pub len: usize,
}

/// The payload union used by [`RveValue`]. Read only the field selected by `tag`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union RveValueData {
    /// Boolean payload selected by [`RVE_VALUE_BOOL`].
    pub boolean: bool,
    /// Integer payload selected by [`RVE_VALUE_INT`].
    pub integer: i64,
    /// Floating-point payload selected by [`RVE_VALUE_FLOAT`].
    pub float: f64,
    /// String payload selected by [`RVE_VALUE_STR`].
    pub string: RveStr,
    /// Array payload selected by [`RVE_VALUE_ARRAY`].
    pub array: RveArray,
}

/// A tagged value crossing the script ABI.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveValue {
    /// One of the `RVE_VALUE_*` tags.
    pub tag: u32,
    /// Payload whose active field is selected by `tag`.
    pub data: RveValueData,
}

impl RveValue {
    /// Creates a boolean value.
    pub const fn boolean(value: bool) -> Self {
        Self {
            tag: RVE_VALUE_BOOL,
            data: RveValueData { boolean: value },
        }
    }

    /// Creates a signed integer value.
    pub const fn integer(value: i64) -> Self {
        Self {
            tag: RVE_VALUE_INT,
            data: RveValueData { integer: value },
        }
    }

    /// Creates a floating-point value.
    pub const fn float(value: f64) -> Self {
        Self {
            tag: RVE_VALUE_FLOAT,
            data: RveValueData { float: value },
        }
    }

    /// Creates a borrowed string value.
    pub fn string(value: &str) -> Self {
        Self {
            tag: RVE_VALUE_STR,
            data: RveValueData {
                string: RveStr::from_str_ref(value),
            },
        }
    }

    /// Creates a borrowed array value.
    pub const fn array(values: &[RveValue]) -> Self {
        Self {
            tag: RVE_VALUE_ARRAY,
            data: RveValueData {
                array: RveArray {
                    ptr: values.as_ptr(),
                    len: values.len(),
                },
            },
        }
    }
}

/// One key/value pair in a borrowed parameter table.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveParamEntry {
    /// UTF-8 parameter name.
    pub key: RveStr,
    /// Borrowed parameter value.
    pub value: RveValue,
}

/// Borrowed script parameters, valid for the duration of the callback.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveParams {
    /// Pointer to the first parameter entry, or null when `len` is zero.
    pub entries: *const RveParamEntry,
    /// Number of parameter entries.
    pub len: usize,
}

/// Result of a guest callback.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RveStatus {
    /// Callback completed successfully.
    Ok = 0,
    /// Callback returned an error.
    Error = 1,
    /// Callback caught a Rust panic.
    Panicked = 2,
}

/// Time snapshot supplied to a guest callback.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RveTime {
    /// Elapsed scene time in seconds.
    pub elapsed: f64,
    /// Current frame number.
    pub frame: u64,
}

/// Local transform in position, Euler rotation degrees, and scale order.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RveTransform {
    /// Local position.
    pub position: [f32; 3],
    /// Local Euler rotation in degrees.
    pub rotation_deg: [f32; 3],
    /// Local scale.
    pub scale: [f32; 3],
}

impl Default for RveTransform {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            rotation_deg: [0.0; 3],
            scale: [1.0; 3],
        }
    }
}

/// Host callback that copies a guest error string for the duration of the call.
pub type RveSetErrorFn = unsafe extern "C" fn(ctx: *mut c_void, message: RveStr);
/// Host callback that appends a guest log event.
pub type RveLogFn = unsafe extern "C" fn(ctx: *mut c_void, level: u32, message: RveStr);
/// Host callback that looks up a runtime object by id, returning zero when it is not found.
pub type RveFindObjectFn = unsafe extern "C" fn(ctx: *mut c_void, id: RveStr) -> u64;
/// Host callback that copies an object's id or name into a borrowed output string.
pub type RveObjectStringFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool;
/// Host callback that copies an object's local transform.
pub type RveGetLocalFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut RveTransform) -> bool;
/// Host callback that stages an object's local transform.
pub type RveSetLocalFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, transform: *const RveTransform) -> bool;
/// Host callback that copies an object's column-major world matrix.
pub type RveWorldMatrixFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut [f32; 16]) -> bool;
/// Host callback that stages a message for one object.
pub type RveSendFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    target_id: RveStr,
    name: RveStr,
    value: *const RveValue,
) -> bool;
/// Host callback that stages a broadcast message.
pub type RveBroadcastFn =
    unsafe extern "C" fn(ctx: *mut c_void, name: RveStr, value: *const RveValue) -> bool;

/// Host functions available during one guest call. The value is call-scoped and thread-local.
#[repr(C)]
pub struct RveHost {
    /// Opaque call-scoped host context passed back to every callback.
    pub ctx: *mut c_void,
    /// Time snapshot for the current lifecycle phase.
    pub time: RveTime,
    /// Current object's handle, encoded as `index + 1`.
    pub self_object: u64,
    /// Host error-reporting callback.
    pub set_error: RveSetErrorFn,
    /// Host logging callback.
    pub log: RveLogFn,
    /// Object lookup callback.
    pub find_object: RveFindObjectFn,
    /// Object-id callback.
    pub object_id: RveObjectStringFn,
    /// Object-name callback.
    pub object_name: RveObjectStringFn,
    /// Local-transform read callback.
    pub get_local: RveGetLocalFn,
    /// Local-transform write callback.
    pub set_local: RveSetLocalFn,
    /// World-matrix callback.
    pub world_matrix: RveWorldMatrixFn,
    /// Message-send callback.
    pub send: RveSendFn,
    /// Message-broadcast callback.
    pub broadcast: RveBroadcastFn,
}

/// Guest factory callback.
pub type RveCreateFn = unsafe extern "C" fn(
    host: *const RveHost,
    params: *const RveParams,
    out: *mut *mut c_void,
) -> RveStatus;
/// Guest `on_start` and `on_destroy` callback.
pub type RveStartFn =
    unsafe extern "C" fn(instance: *mut c_void, host: *const RveHost) -> RveStatus;
/// Guest `on_update` callback.
pub type RveUpdateFn =
    unsafe extern "C" fn(instance: *mut c_void, host: *const RveHost, dt: f32) -> RveStatus;
/// Guest message callback.
pub type RveMessageFn = unsafe extern "C" fn(
    instance: *mut c_void,
    host: *const RveHost,
    sender: RveStr,
    name: RveStr,
    value: *const RveValue,
) -> RveStatus;
/// Guest `on_destroy` callback, with the same arguments as `on_start`.
pub type RveDestroyFn = RveStartFn;
/// Guest instance destructor callback.
pub type RveDropFn = unsafe extern "C" fn(instance: *mut c_void) -> RveStatus;

/// One exported guest script implementation.
#[repr(C)]
pub struct RveScriptDesc {
    /// ASCII script name in the `[A-Za-z0-9_]{1,64}` format.
    pub name: RveStr,
    /// Creates one guest instance.
    pub create: RveCreateFn,
    /// Runs the instance's start callback.
    pub start: RveStartFn,
    /// Runs the instance's update callback.
    pub update: RveUpdateFn,
    /// Runs the instance's message callback.
    pub message: RveMessageFn,
    /// Runs the instance's destroy callback.
    pub destroy: RveDestroyFn,
    /// Releases one successfully created instance.
    pub drop: RveDropFn,
}

/// Static script table returned from `rve_script_module`.
#[repr(C)]
pub struct RveModule {
    /// ABI version and header size, checked before the remaining fields.
    pub header: RveHeader,
    /// Pointer to `script_count` immutable descriptors.
    pub scripts: *const RveScriptDesc,
    /// Number of script descriptors.
    pub script_count: usize,
}

impl RveModule {
    /// Constructs a module header around a process-lifetime descriptor slice.
    pub const fn new(scripts: &'static [RveScriptDesc]) -> Self {
        Self {
            header: RveHeader {
                abi_version: RVE_SCRIPT_ABI_VERSION,
                header_size: std::mem::size_of::<RveHeader>() as u32,
            },
            scripts: scripts.as_ptr(),
            script_count: scripts.len(),
        }
    }
}

// These pointers are only dereferenced through the documented unsafe ABI contracts. The SDK
// macros use these types in immutable process-lifetime tables.
unsafe impl Sync for RveStr {}
unsafe impl Sync for RveArray {}
unsafe impl Sync for RveValueData {}
unsafe impl Sync for RveValue {}
unsafe impl Sync for RveParamEntry {}
unsafe impl Sync for RveScriptDesc {}
unsafe impl Sync for RveModule {}
