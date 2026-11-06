//! C-compatible types shared by an RVE host and a game script library.

use std::ffi::c_void;

/// Oldest script ABI version accepted by the host.
pub const RVE_SCRIPT_ABI_MIN_VERSION: u32 = 3;
/// Stable script ABI version. Any incompatible ABI layout change requires a version increase.
pub const RVE_SCRIPT_ABI_VERSION: u32 = 6;

/// Camera anti-aliasing code for [`RveCameraSettings::anti_aliasing`].
pub const RVE_ANTI_ALIASING_FXAA: u32 = 0;
/// Camera anti-aliasing code for [`RveCameraSettings::anti_aliasing`].
pub const RVE_ANTI_ALIASING_NONE: u32 = 1;
/// Camera anti-aliasing code for [`RveCameraSettings::anti_aliasing`].
pub const RVE_ANTI_ALIASING_TAA: u32 = 2;
/// Camera anti-aliasing code for [`RveCameraSettings::anti_aliasing`].
pub const RVE_ANTI_ALIASING_FSR: u32 = 3;
/// Camera anti-aliasing code for [`RveCameraSettings::anti_aliasing`].
pub const RVE_ANTI_ALIASING_DLSS: u32 = 4;
/// Camera tonemapper code for [`RveCameraSettings::tonemapper`].
pub const RVE_TONEMAPPER_ACES: u32 = 0;
/// Camera tonemapper code for [`RveCameraSettings::tonemapper`].
pub const RVE_TONEMAPPER_NEUTRAL: u32 = 1;
/// Camera tonemapper code for [`RveCameraSettings::tonemapper`].
pub const RVE_TONEMAPPER_NONE: u32 = 2;

/// Input state bit: the key or button is currently held.
pub const RVE_INPUT_DOWN: u32 = 1;
/// Input state bit: the key or button became held in this frame.
pub const RVE_INPUT_PRESSED: u32 = 2;
/// Input state bit: the key or button became released in this frame.
pub const RVE_INPUT_RELEASED: u32 = 4;
/// Input action kind code for button actions.
pub const RVE_ACTION_BUTTON: u32 = 0;
/// Input action kind code for scalar axis actions.
pub const RVE_ACTION_AXIS: u32 = 1;
/// Input action kind code for two-dimensional vector actions.
pub const RVE_ACTION_VECTOR2: u32 = 2;
/// Maximum number of physical inputs in one binding slot.
pub const RVE_INPUT_MAX_BINDINGS: usize = 16;

/// Stable key codes shared by the host ABI and the Rust guest API.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Key {
    A = 0,
    B = 1,
    C = 2,
    D = 3,
    E = 4,
    F = 5,
    G = 6,
    H = 7,
    I = 8,
    J = 9,
    K = 10,
    L = 11,
    M = 12,
    N = 13,
    O = 14,
    P = 15,
    Q = 16,
    R = 17,
    S = 18,
    T = 19,
    U = 20,
    V = 21,
    W = 22,
    X = 23,
    Y = 24,
    Z = 25,
    Digit0 = 26,
    Digit1 = 27,
    Digit2 = 28,
    Digit3 = 29,
    Digit4 = 30,
    Digit5 = 31,
    Digit6 = 32,
    Digit7 = 33,
    Digit8 = 34,
    Digit9 = 35,
    F1 = 36,
    F2 = 37,
    F3 = 38,
    F4 = 39,
    F5 = 40,
    F6 = 41,
    F7 = 42,
    F8 = 43,
    F9 = 44,
    F10 = 45,
    F11 = 46,
    F12 = 47,
    ArrowUp = 48,
    ArrowDown = 49,
    ArrowLeft = 50,
    ArrowRight = 51,
    Space = 52,
    Enter = 53,
    Escape = 54,
    Tab = 55,
    Backspace = 56,
    ShiftLeft = 57,
    ShiftRight = 58,
    ControlLeft = 59,
    ControlRight = 60,
    AltLeft = 61,
    AltRight = 62,
    SuperLeft = 63,
    SuperRight = 64,
}

impl Key {
    /// Converts an API name to a key.
    pub fn from_name(name: &str) -> Option<Self> {
        if let [letter @ b'A'..=b'Z'] = name.as_bytes() {
            return Self::from_code(u32::from(*letter - b'A'));
        }
        if let Some(digit) = name.strip_prefix("Digit")
            && let [digit @ b'0'..=b'9'] = digit.as_bytes()
        {
            return Self::from_code(26 + u32::from(*digit - b'0'));
        }
        if let Some(function_key) = name.strip_prefix('F') {
            let number = match function_key.as_bytes() {
                [number @ b'1'..=b'9'] => u32::from(*number - b'0'),
                [b'1', tens @ b'0'..=b'2'] => 10 + u32::from(*tens - b'0'),
                _ => 0,
            };
            if (1..=12).contains(&number) {
                return Self::from_code(35 + number);
            }
        }
        match name {
            "ArrowUp" => Some(Self::ArrowUp),
            "ArrowDown" => Some(Self::ArrowDown),
            "ArrowLeft" => Some(Self::ArrowLeft),
            "ArrowRight" => Some(Self::ArrowRight),
            "Space" => Some(Self::Space),
            "Enter" => Some(Self::Enter),
            "Escape" => Some(Self::Escape),
            "Tab" => Some(Self::Tab),
            "Backspace" => Some(Self::Backspace),
            "ShiftLeft" => Some(Self::ShiftLeft),
            "ShiftRight" => Some(Self::ShiftRight),
            "ControlLeft" => Some(Self::ControlLeft),
            "ControlRight" => Some(Self::ControlRight),
            "AltLeft" => Some(Self::AltLeft),
            "AltRight" => Some(Self::AltRight),
            "SuperLeft" => Some(Self::SuperLeft),
            "SuperRight" => Some(Self::SuperRight),
            _ => None,
        }
    }

    /// Converts a stable ABI key code to a key when it is supported.
    pub const fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::A,
            1 => Self::B,
            2 => Self::C,
            3 => Self::D,
            4 => Self::E,
            5 => Self::F,
            6 => Self::G,
            7 => Self::H,
            8 => Self::I,
            9 => Self::J,
            10 => Self::K,
            11 => Self::L,
            12 => Self::M,
            13 => Self::N,
            14 => Self::O,
            15 => Self::P,
            16 => Self::Q,
            17 => Self::R,
            18 => Self::S,
            19 => Self::T,
            20 => Self::U,
            21 => Self::V,
            22 => Self::W,
            23 => Self::X,
            24 => Self::Y,
            25 => Self::Z,
            26 => Self::Digit0,
            27 => Self::Digit1,
            28 => Self::Digit2,
            29 => Self::Digit3,
            30 => Self::Digit4,
            31 => Self::Digit5,
            32 => Self::Digit6,
            33 => Self::Digit7,
            34 => Self::Digit8,
            35 => Self::Digit9,
            36 => Self::F1,
            37 => Self::F2,
            38 => Self::F3,
            39 => Self::F4,
            40 => Self::F5,
            41 => Self::F6,
            42 => Self::F7,
            43 => Self::F8,
            44 => Self::F9,
            45 => Self::F10,
            46 => Self::F11,
            47 => Self::F12,
            48 => Self::ArrowUp,
            49 => Self::ArrowDown,
            50 => Self::ArrowLeft,
            51 => Self::ArrowRight,
            52 => Self::Space,
            53 => Self::Enter,
            54 => Self::Escape,
            55 => Self::Tab,
            56 => Self::Backspace,
            57 => Self::ShiftLeft,
            58 => Self::ShiftRight,
            59 => Self::ControlLeft,
            60 => Self::ControlRight,
            61 => Self::AltLeft,
            62 => Self::AltRight,
            63 => Self::SuperLeft,
            64 => Self::SuperRight,
            _ => return None,
        })
    }
}

/// Stable mouse-button codes shared by the host ABI and the Rust guest API.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

impl MouseButton {
    /// Converts an API name to a mouse button.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Left" => Some(Self::Left),
            "Right" => Some(Self::Right),
            "Middle" => Some(Self::Middle),
            "Back" => Some(Self::Back),
            "Forward" => Some(Self::Forward),
            _ => None,
        }
    }

    /// Converts a stable ABI mouse-button code to a button when it is supported.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Left),
            1 => Some(Self::Right),
            2 => Some(Self::Middle),
            3 => Some(Self::Back),
            4 => Some(Self::Forward),
            _ => None,
        }
    }
}

/// A physical keyboard key or mouse button used by project input bindings.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PhysicalInput {
    Key(Key),
    MouseButton(MouseButton),
}

impl PhysicalInput {
    /// Parses a stable key name or a `Mouse.<Button>` name.
    pub fn from_name(name: &str) -> Option<Self> {
        if let Some(button) = name.strip_prefix("Mouse.") {
            MouseButton::from_name(button).map(Self::MouseButton)
        } else {
            Key::from_name(name).map(Self::Key)
        }
    }

    /// Returns the stable name used by project input bindings.
    pub fn name(self) -> String {
        match self {
            Self::Key(key) => format!("{key:?}"),
            Self::MouseButton(button) => format!("Mouse.{button:?}"),
        }
    }
}

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

    /// Creates a process-lifetime string value usable in constant tables.
    pub const fn static_string(value: &'static str) -> Self {
        Self {
            tag: RVE_VALUE_STR,
            data: RveValueData {
                string: RveStr::from_static(value),
            },
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

/// Complete projection, render, and color settings for one scene camera.
///
/// Enum values use the stable `RVE_ANTI_ALIASING_*` and `RVE_TONEMAPPER_*` codes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RveCameraSettings {
    /// Perspective field of view in degrees, measured on the narrow window side.
    pub fov_deg: f32,
    /// Near clip plane distance.
    pub near: f32,
    /// Far clip plane distance.
    pub far: f32,
    /// Render resolution scale in `(0, 1]`.
    pub render_scale: f32,
    /// Linear scene clear color.
    pub clear_color: [f32; 3],
    /// Manual exposure in EV.
    pub exposure_ev: f32,
    /// Stable anti-aliasing code.
    pub anti_aliasing: u32,
    /// Stable tonemapper code.
    pub tonemapper: u32,
}

impl Default for RveCameraSettings {
    fn default() -> Self {
        Self {
            fov_deg: 60.0,
            near: 0.1,
            far: 1000.0,
            render_scale: 1.0,
            clear_color: [0.1, 0.1, 0.12],
            exposure_ev: 0.0,
            anti_aliasing: RVE_ANTI_ALIASING_FXAA,
            tonemapper: RVE_TONEMAPPER_ACES,
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
/// Reads the down/pressed/released bit flags for one stable key code.
pub type RveKeyStateFn = unsafe extern "C" fn(ctx: *mut c_void, key: u32) -> u32;
/// Reads the down/pressed/released bit flags for one stable mouse-button code.
pub type RveMouseButtonStateFn = unsafe extern "C" fn(ctx: *mut c_void, button: u32) -> u32;
/// Copies one input vector into a guest-provided output slot.
pub type RveInputVectorFn = unsafe extern "C" fn(ctx: *mut c_void, out: *mut [f32; 2]) -> bool;
/// Reads one object's camera settings into a guest-owned output slot.
pub type RveGetCameraSettingsFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut RveCameraSettings) -> bool;
/// Stages one object's camera settings.
pub type RveSetCameraSettingsFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, settings: *const RveCameraSettings) -> bool;
/// Returns the active camera handle, or zero when unavailable.
pub type RveActiveCameraFn = unsafe extern "C" fn(ctx: *mut c_void) -> u64;
/// Stages the active camera selection.
pub type RveSetActiveCameraFn = unsafe extern "C" fn(ctx: *mut c_void, object: u64) -> bool;
/// Host command registration callback; returns `RveStatus` as a `u32`.
pub type RveRegisterCommandFn =
    unsafe extern "C" fn(ctx: *mut c_void, name: RveStr, description: RveStr) -> u32;
/// Host command unregistration callback; returns `RveStatus` as a `u32`.
pub type RveUnregisterCommandFn = unsafe extern "C" fn(ctx: *mut c_void, name: RveStr) -> u32;
/// Reads button action state flags. Appended in ABI v5.
pub type RveActionStateFn = unsafe extern "C" fn(ctx: *mut c_void, name: RveStr, kind: u32) -> u32;
/// Reads an axis or vector2 action value after checking its kind. Appended in ABI v5.
pub type RveActionValueFn =
    unsafe extern "C" fn(ctx: *mut c_void, name: RveStr, kind: u32, out: *mut [f32; 2]) -> bool;
/// Copies the active input context name. Appended in ABI v5.
pub type RveInputContextFn = unsafe extern "C" fn(ctx: *mut c_void, out: *mut RveStr) -> bool;
/// Stages an input context change. Appended in ABI v5.
pub type RveSetInputContextFn = unsafe extern "C" fn(ctx: *mut c_void, name: RveStr) -> bool;
/// Stages replacement of one action binding slot. Appended in ABI v5.
pub type RveRebindActionFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    context: RveStr,
    action: RveStr,
    slot: RveStr,
    inputs: *const RveStr,
    input_count: usize,
) -> bool;
/// Iterates one binding slot. A valid empty slot returns true with an empty string at index zero.
/// Appended in ABI v5.
pub type RveActionBindingFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    context: RveStr,
    action: RveStr,
    slot: RveStr,
    index: usize,
    out: *mut RveStr,
) -> bool;
/// Stages restoration of the project input bindings. Appended in ABI v5.
pub type RveResetBindingsFn = unsafe extern "C" fn(ctx: *mut c_void) -> bool;

pub const RVE_COLLISION_ENTER: u32 = 0;
pub const RVE_COLLISION_EXIT: u32 = 1;
pub const RVE_TRIGGER_ENTER: u32 = 2;
pub const RVE_TRIGGER_EXIT: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RveRaycastHit {
    pub object: u64,
    pub point: [f32; 3],
    pub normal: [f32; 3],
    pub distance: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RveCharacterMoveResult {
    pub translation: [f32; 3],
    pub grounded: bool,
    pub collided: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RveCollisionEvent {
    pub kind: u32,
    pub other: u64,
    pub point: [f32; 3],
    /// Contact normal points from this script object's collider toward `other`.
    pub normal: [f32; 3],
}

pub type RvePhysicsRaycastFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    origin: *const [f32; 3],
    direction: *const [f32; 3],
    max_distance: f32,
    include_triggers: bool,
    ignore: u64,
    out: *mut RveRaycastHit,
) -> bool;
pub type RvePhysicsVectorFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, value: *mut [f32; 3]) -> bool;
pub type RvePhysicsSetVectorFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, value: *const [f32; 3]) -> bool;
pub type RvePhysicsMoveCharacterFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    object: u64,
    displacement: *const [f32; 3],
    out: *mut RveCharacterMoveResult,
) -> bool;
pub type RvePhysicsBoolFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, value: *mut bool) -> bool;
pub type RvePhysicsSetBoolFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, value: bool) -> bool;

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
    /// Key-state callback.
    pub key_state: RveKeyStateFn,
    /// Mouse-button-state callback.
    pub mouse_button_state: RveMouseButtonStateFn,
    /// Cursor-position callback.
    pub cursor_position: RveInputVectorFn,
    /// Mouse-delta callback.
    pub mouse_delta: RveInputVectorFn,
    /// Wheel-delta callback.
    pub wheel_delta: RveInputVectorFn,
    /// Camera-settings callbacks are appended in ABI v2; `None` marks an unavailable callback.
    pub get_camera_settings: Option<RveGetCameraSettingsFn>,
    /// Stages one object's camera settings; present in ABI v2.
    pub set_camera_settings: Option<RveSetCameraSettingsFn>,
    /// Returns the active camera; present in ABI v2.
    pub active_camera: Option<RveActiveCameraFn>,
    /// Stages the active camera selection; present in ABI v2.
    pub set_active_camera: Option<RveSetActiveCameraFn>,
    /// Registers a command owned by the active script; appended in ABI v4.
    pub register_command: Option<RveRegisterCommandFn>,
    /// Unregisters a command owned by the active script; appended in ABI v4.
    pub unregister_command: Option<RveUnregisterCommandFn>,
    /// Reads button action flags; appended in ABI v5.
    pub action_state: Option<RveActionStateFn>,
    /// Reads an axis or vector2 value; appended in ABI v5.
    pub action_value: Option<RveActionValueFn>,
    /// Copies the active input context name; appended in ABI v5.
    pub input_context: Option<RveInputContextFn>,
    /// Stages an input context change; appended in ABI v5.
    pub set_input_context: Option<RveSetInputContextFn>,
    /// Stages one action binding replacement; appended in ABI v5.
    pub rebind_action: Option<RveRebindActionFn>,
    /// Iterates one action binding slot; appended in ABI v5.
    pub action_binding: Option<RveActionBindingFn>,
    /// Stages resetting bindings; appended in ABI v5.
    pub reset_bindings: Option<RveResetBindingsFn>,
    /// Optional physics callbacks appended in ABI v6.
    pub raycast: Option<RvePhysicsRaycastFn>,
    pub get_velocity: Option<RvePhysicsVectorFn>,
    pub set_velocity: Option<RvePhysicsSetVectorFn>,
    pub add_impulse: Option<RvePhysicsSetVectorFn>,
    pub add_force: Option<RvePhysicsSetVectorFn>,
    pub move_character: Option<RvePhysicsMoveCharacterFn>,
    pub is_grounded: Option<RvePhysicsBoolFn>,
    pub set_collider_enabled: Option<RvePhysicsSetBoolFn>,
    pub is_collider_enabled: Option<RvePhysicsBoolFn>,
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
/// Guest command callback.
pub type RveCommandFn = unsafe extern "C" fn(
    instance: *mut c_void,
    host: *const RveHost,
    name: RveStr,
    args: *const RveStr,
    arg_count: usize,
) -> RveStatus;
/// Guest collision and trigger callback.
pub type RveCollisionFn = unsafe extern "C" fn(
    instance: *mut c_void,
    host: *const RveHost,
    event: RveCollisionEvent,
) -> RveStatus;

/// Script descriptor layout used by ABI versions 1 and 2.
#[repr(C)]
pub struct RveScriptDescV1 {
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

/// Script descriptor layout used by ABI version 3.
#[repr(C)]
pub struct RveScriptDescV3 {
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
    /// Declared parameters in declaration order; null only when `param_count` is zero.
    pub params: *const RveParamDesc,
    /// Number of parameter descriptors. Zero means the script declares no schema.
    pub param_count: usize,
}

/// Script descriptor layout shared by ABI versions 4 and 5.
///
/// The optional command callback was appended in v4; ABI v5 kept this descriptor unchanged.
/// Versions 1 and 2 use [`RveScriptDescV1`], and version 3 uses [`RveScriptDescV3`]; both are
/// normalized by the host.
#[repr(C)]
pub struct RveScriptDescV4 {
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
    /// Declared parameters in declaration order; null only when `param_count` is zero.
    pub params: *const RveParamDesc,
    /// Number of parameter descriptors. Zero means the script declares no schema.
    pub param_count: usize,
    /// Optional callback for console commands registered by this script.
    pub command: Option<RveCommandFn>,
}

/// One exported guest script implementation in ABI version 6.
#[repr(C)]
pub struct RveScriptDesc {
    pub name: RveStr,
    pub create: RveCreateFn,
    pub start: RveStartFn,
    pub update: RveUpdateFn,
    pub message: RveMessageFn,
    pub destroy: RveDestroyFn,
    pub drop: RveDropFn,
    pub params: *const RveParamDesc,
    pub param_count: usize,
    pub command: Option<RveCommandFn>,
    /// Optional collision and trigger callback appended in ABI v6.
    pub collision: Option<RveCollisionFn>,
}

/// Parameter kind: boolean.
pub const RVE_PARAM_BOOL: u32 = 0;
/// Parameter kind: signed 64-bit integer.
pub const RVE_PARAM_INT: u32 = 1;
/// Parameter kind: 64-bit floating-point number.
pub const RVE_PARAM_FLOAT: u32 = 2;
/// Parameter kind: UTF-8 string.
pub const RVE_PARAM_STRING: u32 = 3;
/// Parameter kind: array of three floating-point numbers.
pub const RVE_PARAM_VEC3: u32 = 4;

/// [`RveParamDesc::flags`] bit: `min` is set.
pub const RVE_PARAM_HAS_MIN: u32 = 1;
/// [`RveParamDesc::flags`] bit: `max` is set.
pub const RVE_PARAM_HAS_MAX: u32 = 2;

/// One declared script parameter. All pointers refer to process-lifetime data.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveParamDesc {
    /// Parameter name in the `[A-Za-z_][A-Za-z0-9_]*` format, at most 64 bytes.
    pub name: RveStr,
    /// One of the `RVE_PARAM_*` kinds.
    pub kind: u32,
    /// Default value; it must match `kind` and the declared range.
    pub default: RveValue,
    /// Inclusive lower bound, read only with [`RVE_PARAM_HAS_MIN`].
    pub min: f64,
    /// Inclusive upper bound, read only with [`RVE_PARAM_HAS_MAX`].
    pub max: f64,
    /// `RVE_PARAM_HAS_*` bits; other bits must be zero.
    pub flags: u32,
    /// Optional UTF-8 label for editors; empty means none.
    pub label: RveStr,
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
unsafe impl Sync for RveParamDesc {}
unsafe impl Sync for RveScriptDescV1 {}
unsafe impl Sync for RveScriptDescV3 {}
unsafe impl Sync for RveScriptDescV4 {}
unsafe impl Sync for RveScriptDesc {}
unsafe impl Sync for RveModule {}

#[cfg(test)]
mod tests {
    use super::{
        Key, MouseButton, PhysicalInput, RVE_ANTI_ALIASING_DLSS, RVE_ANTI_ALIASING_FSR,
        RVE_ANTI_ALIASING_FXAA, RVE_ANTI_ALIASING_NONE, RVE_ANTI_ALIASING_TAA, RVE_TONEMAPPER_ACES,
        RVE_TONEMAPPER_NEUTRAL, RVE_TONEMAPPER_NONE, RveCameraSettings, RveGetCameraSettingsFn,
        RveHost,
    };

    #[test]
    fn key_codes_and_names_are_stable() {
        assert_eq!(Key::A as u32, 0);
        assert_eq!(Key::Z as u32, 25);
        assert_eq!(Key::Digit0 as u32, 26);
        assert_eq!(Key::Digit9 as u32, 35);
        assert_eq!(Key::F1 as u32, 36);
        assert_eq!(Key::F12 as u32, 47);
        assert_eq!(Key::SuperRight as u32, 64);

        for code in 0..=(Key::SuperRight as u32) {
            assert_eq!(Key::from_code(code).map(|key| key as u32), Some(code));
        }
        assert_eq!(Key::from_code(Key::SuperRight as u32 + 1), None);

        assert_eq!(Key::from_name("F1"), Some(Key::F1));
        assert_eq!(Key::from_name("F12"), Some(Key::F12));
        assert_eq!(Key::from_name("Digit9"), Some(Key::Digit9));
        assert_eq!(Key::from_name("ControlRight"), Some(Key::ControlRight));
        assert_eq!(Key::from_name("A"), Some(Key::A));
        assert_eq!(Key::from_name("Space"), Some(Key::Space));
        assert_eq!(Key::from_name("UnknownKey"), None);

        for code in 0..=25_u32 {
            let name = char::from(b'A' + code as u8).to_string();
            assert_eq!(
                Key::from_name(&name).map(|key| key as u32),
                Some(code),
                "key name {name}"
            );
        }
    }

    #[test]
    fn mouse_button_codes_and_names_are_stable() {
        assert_eq!(MouseButton::Left as u32, 0);
        assert_eq!(MouseButton::Right as u32, 1);
        assert_eq!(MouseButton::Middle as u32, 2);
        assert_eq!(MouseButton::Back as u32, 3);
        assert_eq!(MouseButton::Forward as u32, 4);

        for code in 0..=(MouseButton::Forward as u32) {
            assert_eq!(
                MouseButton::from_code(code).map(|button| button as u32),
                Some(code)
            );
        }
        assert_eq!(
            MouseButton::from_code(MouseButton::Forward as u32 + 1),
            None
        );

        assert_eq!(MouseButton::from_name("Left"), Some(MouseButton::Left));
        assert_eq!(MouseButton::from_name("Right"), Some(MouseButton::Right));
        assert_eq!(MouseButton::from_name("Middle"), Some(MouseButton::Middle));
        assert_eq!(MouseButton::from_name("Back"), Some(MouseButton::Back));
        assert_eq!(
            MouseButton::from_name("Forward"),
            Some(MouseButton::Forward)
        );
        assert_eq!(MouseButton::from_name("UnknownButton"), None);
    }

    #[test]
    fn physical_input_names_are_stable_and_explicit_for_mouse_buttons() {
        assert_eq!(
            PhysicalInput::from_name("Space"),
            Some(PhysicalInput::Key(Key::Space))
        );
        assert_eq!(
            PhysicalInput::from_name("Mouse.Left"),
            Some(PhysicalInput::MouseButton(MouseButton::Left))
        );
        assert_eq!(PhysicalInput::Key(Key::ArrowUp).name(), "ArrowUp");
        assert_eq!(
            PhysicalInput::MouseButton(MouseButton::Forward).name(),
            "Mouse.Forward"
        );
        assert_eq!(PhysicalInput::from_name("Left"), None);
        assert_eq!(PhysicalInput::from_name("Mouse.Unknown"), None);
        assert_eq!(PhysicalInput::from_name("Mouse.Left.Extra"), None);
    }

    #[test]
    fn camera_settings_codes_and_c_layout_are_stable_and_appended_to_host() {
        assert_eq!(RVE_ANTI_ALIASING_FXAA, 0);
        assert_eq!(RVE_ANTI_ALIASING_NONE, 1);
        assert_eq!(RVE_ANTI_ALIASING_TAA, 2);
        assert_eq!(RVE_ANTI_ALIASING_FSR, 3);
        assert_eq!(RVE_ANTI_ALIASING_DLSS, 4);
        assert_eq!(RVE_TONEMAPPER_ACES, 0);
        assert_eq!(RVE_TONEMAPPER_NEUTRAL, 1);
        assert_eq!(RVE_TONEMAPPER_NONE, 2);
        assert_eq!(std::mem::size_of::<RveCameraSettings>(), 40);
        assert_eq!(std::mem::align_of::<RveCameraSettings>(), 4);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, fov_deg), 0);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, near), 4);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, far), 8);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, render_scale), 12);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, clear_color), 16);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, exposure_ev), 28);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, anti_aliasing), 32);
        assert_eq!(std::mem::offset_of!(RveCameraSettings, tonemapper), 36);
        assert_eq!(
            std::mem::offset_of!(RveHost, get_camera_settings),
            std::mem::offset_of!(RveHost, wheel_delta)
                + std::mem::size_of::<super::RveInputVectorFn>()
        );
        assert_eq!(
            std::mem::size_of::<Option<RveGetCameraSettingsFn>>(),
            std::mem::size_of::<RveGetCameraSettingsFn>()
        );
    }
}
