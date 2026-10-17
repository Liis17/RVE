//! C-compatible types shared by an RVE host and a game script library.

use std::ffi::c_void;

/// Stable script ABI version. Any incompatible ABI layout change requires a version increase.
pub const RVE_SCRIPT_ABI_VERSION: u32 = 1;

/// Input state bit: the key or button is currently held.
pub const RVE_INPUT_DOWN: u32 = 1;
/// Input state bit: the key or button became held in this frame.
pub const RVE_INPUT_PRESSED: u32 = 2;
/// Input state bit: the key or button became released in this frame.
pub const RVE_INPUT_RELEASED: u32 = 4;

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
/// Reads the down/pressed/released bit flags for one stable key code.
pub type RveKeyStateFn = unsafe extern "C" fn(ctx: *mut c_void, key: u32) -> u32;
/// Reads the down/pressed/released bit flags for one stable mouse-button code.
pub type RveMouseButtonStateFn = unsafe extern "C" fn(ctx: *mut c_void, button: u32) -> u32;
/// Copies one input vector into a guest-provided output slot.
pub type RveInputVectorFn = unsafe extern "C" fn(ctx: *mut c_void, out: *mut [f32; 2]) -> bool;

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

#[cfg(test)]
mod tests {
    use super::{Key, MouseButton};

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
}
