//! C-compatible types shared by an RVE host and a game script library.

use std::ffi::c_void;

/// Oldest script ABI version accepted by the host.
pub const RVE_SCRIPT_ABI_MIN_VERSION: u32 = 3;
/// Stable script ABI version. Any incompatible ABI layout change requires a version increase.
///
/// Version 8 appends cursor callbacks after the v7 audio tail.
/// Version 9 appends spawn, destroy, object-state and prefab callbacks after the v8 cursor tail.
/// Version 10 appends animation callbacks after the v9 prefab tail.
/// Version 11 appends gamepad state and rumble callbacks after the v10 animation tail.
/// Version 12 appends post-process callbacks after the v11 gamepad tail.
/// Version 14 appends UI callbacks after the v13 net tail of `RveHost` and the `ui_event` callback
/// after the v13 net event callback of the descriptor.
/// Version 13 appends network callbacks after the v12 post-process tail and the net event callback
/// after the v6 collision tail of the descriptor.
/// Each version is a new number because hosts of the previous version already accept modules
/// that end at its tail.
pub const RVE_SCRIPT_ABI_VERSION: u32 = 14;

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
/// Number of gamepad slots, numbered 1..=4 in script names and 0 for the aggregate.
pub const RVE_GAMEPAD_SLOTS: usize = 4;

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

/// Stable gamepad button codes shared by the host ABI and the Rust guest API.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GamepadButton {
    South = 0,
    East = 1,
    West = 2,
    North = 3,
    LeftBumper = 4,
    RightBumper = 5,
    LeftTrigger = 6,
    RightTrigger = 7,
    Select = 8,
    Start = 9,
    Mode = 10,
    LeftStick = 11,
    RightStick = 12,
    DPadUp = 13,
    DPadDown = 14,
    DPadLeft = 15,
    DPadRight = 16,
}

impl GamepadButton {
    /// Every button in ABI code order.
    pub const ALL: [Self; 17] = [
        Self::South,
        Self::East,
        Self::West,
        Self::North,
        Self::LeftBumper,
        Self::RightBumper,
        Self::LeftTrigger,
        Self::RightTrigger,
        Self::Select,
        Self::Start,
        Self::Mode,
        Self::LeftStick,
        Self::RightStick,
        Self::DPadUp,
        Self::DPadDown,
        Self::DPadLeft,
        Self::DPadRight,
    ];

    /// Converts an API name to a gamepad button.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|button| button.name() == name)
    }

    /// Converts a stable ABI gamepad-button code to a button when it is supported.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::South),
            1 => Some(Self::East),
            2 => Some(Self::West),
            3 => Some(Self::North),
            4 => Some(Self::LeftBumper),
            5 => Some(Self::RightBumper),
            6 => Some(Self::LeftTrigger),
            7 => Some(Self::RightTrigger),
            8 => Some(Self::Select),
            9 => Some(Self::Start),
            10 => Some(Self::Mode),
            11 => Some(Self::LeftStick),
            12 => Some(Self::RightStick),
            13 => Some(Self::DPadUp),
            14 => Some(Self::DPadDown),
            15 => Some(Self::DPadLeft),
            16 => Some(Self::DPadRight),
            _ => None,
        }
    }

    /// Returns the stable button name without the `Gamepad` prefix.
    pub const fn name(self) -> &'static str {
        match self {
            Self::South => "South",
            Self::East => "East",
            Self::West => "West",
            Self::North => "North",
            Self::LeftBumper => "LeftBumper",
            Self::RightBumper => "RightBumper",
            Self::LeftTrigger => "LeftTrigger",
            Self::RightTrigger => "RightTrigger",
            Self::Select => "Select",
            Self::Start => "Start",
            Self::Mode => "Mode",
            Self::LeftStick => "LeftStick",
            Self::RightStick => "RightStick",
            Self::DPadUp => "DPadUp",
            Self::DPadDown => "DPadDown",
            Self::DPadLeft => "DPadLeft",
            Self::DPadRight => "DPadRight",
        }
    }
}

/// Stable gamepad analog-axis codes shared by the host ABI and the Rust guest API.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GamepadAxis {
    LeftX = 0,
    LeftY = 1,
    RightX = 2,
    RightY = 3,
    LeftTrigger = 4,
    RightTrigger = 5,
}

impl GamepadAxis {
    /// Converts an API name to a gamepad axis.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "LeftX" => Some(Self::LeftX),
            "LeftY" => Some(Self::LeftY),
            "RightX" => Some(Self::RightX),
            "RightY" => Some(Self::RightY),
            "LeftTrigger" => Some(Self::LeftTrigger),
            "RightTrigger" => Some(Self::RightTrigger),
            _ => None,
        }
    }

    /// Converts a stable ABI gamepad-axis code to an axis when it is supported.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::LeftX),
            1 => Some(Self::LeftY),
            2 => Some(Self::RightX),
            3 => Some(Self::RightY),
            4 => Some(Self::LeftTrigger),
            5 => Some(Self::RightTrigger),
            _ => None,
        }
    }
}

/// Splits `Gamepad[N].<Name>` into its optional slot (1..=4, `None` for any gamepad) and the
/// trailing name. Returns `None` for other strings and for slots outside 1..=4.
pub fn split_gamepad_name(name: &str) -> Option<(Option<u8>, &str)> {
    let rest = name.strip_prefix("Gamepad")?;
    let (pad, tail) = rest.split_once('.')?;
    let pad = match pad {
        "" => None,
        digits if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
            let pad = digits.parse::<u8>().ok()?;
            if !(1..=RVE_GAMEPAD_SLOTS as u8).contains(&pad) {
                return None;
            }
            Some(pad)
        }
        _ => return None,
    };
    Some((pad, tail))
}

/// A physical keyboard key, mouse button, or gamepad button used by project input bindings.
///
/// `GamepadButton.pad` is `None` for any gamepad (`Gamepad.South`) or `Some(1..=4)` for one
/// slot (`Gamepad2.South`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PhysicalInput {
    Key(Key),
    MouseButton(MouseButton),
    GamepadButton {
        pad: Option<u8>,
        button: GamepadButton,
    },
}

impl PhysicalInput {
    /// Parses a stable key name, a `Mouse.<Button>` name, or a `Gamepad[N].<Button>` name.
    pub fn from_name(name: &str) -> Option<Self> {
        if let Some(button) = name.strip_prefix("Mouse.") {
            MouseButton::from_name(button).map(Self::MouseButton)
        } else if let Some((pad, button)) = split_gamepad_name(name) {
            GamepadButton::from_name(button).map(|button| Self::GamepadButton { pad, button })
        } else {
            Key::from_name(name).map(Self::Key)
        }
    }

    /// Returns the stable name used by project input bindings.
    pub fn name(self) -> String {
        match self {
            Self::Key(key) => format!("{key:?}"),
            Self::MouseButton(button) => format!("Mouse.{button:?}"),
            Self::GamepadButton { pad: None, button } => format!("Gamepad.{}", button.name()),
            Self::GamepadButton {
                pad: Some(pad),
                button,
            } => format!("Gamepad{pad}.{}", button.name()),
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

/// Exposure source code for [`RvePostProcessSettings::exposure_mode`].
pub const RVE_EXPOSURE_MODE_MANUAL: u32 = 0;
/// Exposure source code for [`RvePostProcessSettings::exposure_mode`].
pub const RVE_EXPOSURE_MODE_AUTO: u32 = 1;
/// Ambient occlusion quality code for [`RvePostProcessSettings::ao_quality`].
pub const RVE_AO_QUALITY_LOW: u32 = 0;
/// Ambient occlusion quality code for [`RvePostProcessSettings::ao_quality`].
pub const RVE_AO_QUALITY_MEDIUM: u32 = 1;
/// Ambient occlusion quality code for [`RvePostProcessSettings::ao_quality`].
pub const RVE_AO_QUALITY_HIGH: u32 = 2;

/// Complete post-process settings of one camera, flattened for the C ABI.
///
/// Boolean fields use `0` and `1`. Enum fields use the stable `RVE_*` codes above and
/// `RVE_TONEMAPPER_*`. Appended in ABI v12; [`RveCameraSettings`] keeps its v2 layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RvePostProcessSettings {
    pub exposure_enabled: u32,
    pub exposure_mode: u32,
    pub exposure_ev: f32,
    pub exposure_min_ev: f32,
    pub exposure_max_ev: f32,
    pub exposure_speed_up: f32,
    pub exposure_speed_down: f32,
    pub exposure_low_percent: f32,
    pub exposure_high_percent: f32,
    pub tonemapping_enabled: u32,
    pub tonemapper: u32,
    pub bloom_enabled: u32,
    pub bloom_intensity: f32,
    pub bloom_threshold: f32,
    pub bloom_knee: f32,
    pub bloom_scatter: f32,
    pub bloom_tint: [f32; 3],
    pub ao_enabled: u32,
    pub ao_radius: f32,
    pub ao_intensity: f32,
    pub ao_falloff: f32,
    pub ao_quality: u32,
    pub grading_enabled: u32,
    pub grading_temperature: f32,
    pub grading_tint: f32,
    pub grading_contrast: f32,
    pub grading_saturation: f32,
    pub grading_lift: [f32; 3],
    pub grading_gamma: [f32; 3],
    pub grading_gain: [f32; 3],
    pub vignette_enabled: u32,
    pub vignette_intensity: f32,
    pub vignette_smoothness: f32,
    pub vignette_roundness: f32,
    pub vignette_color: [f32; 3],
    pub aberration_enabled: u32,
    pub aberration_intensity: f32,
    pub grain_enabled: u32,
    pub grain_intensity: f32,
    pub grain_response: f32,
    pub dof_enabled: u32,
    pub dof_focus_distance: f32,
    pub dof_aperture: f32,
    pub dof_focal_length_mm: f32,
    pub dof_max_blur_px: f32,
    pub motion_enabled: u32,
    pub motion_shutter_angle_deg: f32,
    pub motion_samples: u32,
    pub motion_max_blur_px: f32,
}

impl Default for RvePostProcessSettings {
    /// Совпадает со значениями по умолчанию движка: все эффекты выключены, EV 0, ACES.
    fn default() -> Self {
        Self {
            exposure_enabled: 1,
            exposure_mode: RVE_EXPOSURE_MODE_MANUAL,
            exposure_ev: 0.0,
            exposure_min_ev: -6.0,
            exposure_max_ev: 6.0,
            exposure_speed_up: 3.0,
            exposure_speed_down: 1.0,
            exposure_low_percent: 10.0,
            exposure_high_percent: 90.0,
            tonemapping_enabled: 1,
            tonemapper: RVE_TONEMAPPER_ACES,
            bloom_enabled: 0,
            bloom_intensity: 0.5,
            bloom_threshold: 1.0,
            bloom_knee: 0.5,
            bloom_scatter: 0.7,
            bloom_tint: [1.0; 3],
            ao_enabled: 0,
            ao_radius: 0.5,
            ao_intensity: 1.0,
            ao_falloff: 0.6,
            ao_quality: RVE_AO_QUALITY_MEDIUM,
            grading_enabled: 0,
            grading_temperature: 0.0,
            grading_tint: 0.0,
            grading_contrast: 1.0,
            grading_saturation: 1.0,
            grading_lift: [0.0; 3],
            grading_gamma: [1.0; 3],
            grading_gain: [1.0; 3],
            vignette_enabled: 0,
            vignette_intensity: 0.4,
            vignette_smoothness: 0.5,
            vignette_roundness: 1.0,
            vignette_color: [0.0; 3],
            aberration_enabled: 0,
            aberration_intensity: 0.3,
            grain_enabled: 0,
            grain_intensity: 0.3,
            grain_response: 0.8,
            dof_enabled: 0,
            dof_focus_distance: 10.0,
            dof_aperture: 2.8,
            dof_focal_length_mm: 50.0,
            dof_max_blur_px: 16.0,
            motion_enabled: 0,
            motion_shutter_angle_deg: 180.0,
            motion_samples: 12,
            motion_max_blur_px: 32.0,
        }
    }
}

/// Copies one object's post-process settings to a guest-provided output. Appended in ABI v12.
pub type RveGetPostProcessFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut RvePostProcessSettings) -> bool;
/// Stages one object's post-process settings. Appended in ABI v12.
pub type RveSetPostProcessFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    object: u64,
    settings: *const RvePostProcessSettings,
) -> bool;

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

/// Cursor state copied to a guest-provided output slot. Appended in ABI v8.
#[repr(C)]
pub struct RveCursorState {
    pub visible: bool,
    pub locked: bool,
}

/// Copies the cursor state applied by the host. Appended in ABI v8.
pub type RveCursorStateFn =
    unsafe extern "C" fn(ctx: *mut c_void, out: *mut RveCursorState) -> bool;
/// Stages a cursor visibility or lock change. Appended in ABI v8.
pub type RveSetCursorFn = unsafe extern "C" fn(ctx: *mut c_void, value: bool) -> bool;

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

/// Audio group code for [`RveAudioParams::group`] and `audio_set_group_volume`. Appended in ABI v7.
pub const RVE_AUDIO_GROUP_MASTER: u32 = 0;
/// Audio group code; see [`RVE_AUDIO_GROUP_MASTER`].
pub const RVE_AUDIO_GROUP_MUSIC: u32 = 1;
/// Audio group code; see [`RVE_AUDIO_GROUP_MASTER`].
pub const RVE_AUDIO_GROUP_EFFECTS: u32 = 2;
/// Audio group code; see [`RVE_AUDIO_GROUP_MASTER`].
pub const RVE_AUDIO_GROUP_AMBIENT: u32 = 3;

/// Clip parameters for `audio_play_clip`. Appended in ABI v7.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RveAudioParams {
    /// One of the `RVE_AUDIO_GROUP_*` codes; `master` is rejected for clips.
    pub group: u32,
    pub volume: f32,
    pub looping: bool,
    /// When false, `position` is ignored and the clip plays without a spatial emitter.
    pub has_position: bool,
    pub position: [f32; 3],
}

/// Audio callback on one scene object; returns `RveStatus` as a `u32`. Appended in ABI v7.
pub type RveAudioObjectFn = unsafe extern "C" fn(ctx: *mut c_void, object: u64) -> u32;
/// Audio callback on one scene object with a volume; returns `RveStatus` as a `u32`. Appended in ABI v7.
pub type RveAudioObjectVolumeFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, volume: f32) -> u32;
/// Starts a clip by virtual path and writes its voice id to `out_voice`; returns `RveStatus` as a `u32`.
/// Appended in ABI v7.
pub type RveAudioPlayClipFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    path: RveStr,
    params: *const RveAudioParams,
    out_voice: *mut u64,
) -> u32;
/// Stops one voice by id; returns `RveStatus` as a `u32`. Appended in ABI v7.
pub type RveAudioVoiceFn = unsafe extern "C" fn(ctx: *mut c_void, voice: u64) -> u32;
/// Sets one audio group's volume; returns `RveStatus` as a `u32`. Appended in ABI v7.
pub type RveAudioGroupVolumeFn =
    unsafe extern "C" fn(ctx: *mut c_void, group: u32, volume: f32) -> u32;

/// Object state code: the handle is dead, or it never named a live or pending object.
/// Returned by `RveHost::object_state`. Appended in ABI v9.
pub const RVE_OBJECT_DEAD: u32 = 0;
/// Object state code: the object was spawned in this phase and is not in the world yet.
pub const RVE_OBJECT_PENDING: u32 = 1;
/// Object state code: the object is in the world.
pub const RVE_OBJECT_ALIVE: u32 = 2;

/// Prefab state code: the path is not in the project prefab list.
/// Returned by `RveHost::prefab_state`. Appended in ABI v9.
pub const RVE_PREFAB_UNKNOWN: u32 = 0;
/// Prefab state code: a scene fast prefab, always resident.
pub const RVE_PREFAB_RESIDENT: u32 = 1;
/// Prefab state code: a heavy prefab that is not in memory.
pub const RVE_PREFAB_UNLOADED: u32 = 2;
/// Prefab state code: a heavy prefab whose load is in progress.
pub const RVE_PREFAB_LOADING: u32 = 3;
/// Prefab state code: a heavy prefab in memory.
pub const RVE_PREFAB_LOADED: u32 = 4;
/// Prefab state code: a heavy prefab whose unload is in progress.
pub const RVE_PREFAB_UNLOADING: u32 = 5;
/// Prefab state code: a heavy prefab whose load failed.
pub const RVE_PREFAB_FAILED: u32 = 6;

/// [`RveSpawnOptions::flags`] bit: `position` is set.
pub const RVE_SPAWN_HAS_POSITION: u32 = 1 << 0;
/// [`RveSpawnOptions::flags`] bit: `rotation_deg` is set.
pub const RVE_SPAWN_HAS_ROTATION: u32 = 1 << 1;
/// [`RveSpawnOptions::flags`] bit: `scale` is set.
pub const RVE_SPAWN_HAS_SCALE: u32 = 1 << 2;
/// [`RveSpawnOptions::flags`] bit: `velocity` is set; it applies only to a dynamic root.
pub const RVE_SPAWN_HAS_VELOCITY: u32 = 1 << 3;
/// [`RveSpawnOptions::flags`] bit: `parent` is set.
pub const RVE_SPAWN_HAS_PARENT: u32 = 1 << 4;
/// [`RveSpawnOptions::flags`] bit: `name` is set.
pub const RVE_SPAWN_HAS_NAME: u32 = 1 << 5;
/// [`RveSpawnOptions::flags`] bit: `id` is set.
pub const RVE_SPAWN_HAS_ID: u32 = 1 << 6;
/// Every known [`RveSpawnOptions::flags`] bit. The host refuses a spawn with any other bit set.
pub const RVE_SPAWN_KNOWN_FLAGS: u32 = RVE_SPAWN_HAS_ID
    | RVE_SPAWN_HAS_NAME
    | RVE_SPAWN_HAS_PARENT
    | RVE_SPAWN_HAS_VELOCITY
    | RVE_SPAWN_HAS_SCALE
    | RVE_SPAWN_HAS_ROTATION
    | RVE_SPAWN_HAS_POSITION;

/// Optional fields of one spawn request. Read only the fields whose presence bit is set in
/// `flags`; the others may hold any value. Appended in ABI v9.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RveSpawnOptions {
    /// `RVE_SPAWN_HAS_*` presence bits; unknown bits are refused.
    pub flags: u32,
    /// Local position of the new root.
    pub position: [f32; 3],
    /// Local Euler rotation of the new root in degrees.
    pub rotation_deg: [f32; 3],
    /// Local scale of the new root.
    pub scale: [f32; 3],
    /// Initial linear velocity of a dynamic root.
    pub velocity: [f32; 3],
    /// Handle of an existing live object that becomes the parent.
    pub parent: u64,
    /// Name of the new root.
    pub name: RveStr,
    /// Author id of the new root, `[A-Za-z0-9_-]{1,64}`.
    pub id: RveStr,
}

/// Spawns a prefab by project virtual path (without a leading slash). Returns the pending handle,
/// or zero when the host refuses. The options pointer may be null. Appended in ABI v9.
pub type RveSpawnFn =
    unsafe extern "C" fn(ctx: *mut c_void, prefab: RveStr, options: *const RveSpawnOptions) -> u64;
/// Queues an object subtree for removal; a positive delay is in seconds. Returns `false` for a
/// dead handle or a refused request. Appended in ABI v9.
pub type RveDestroyObjectFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, delay: f32) -> bool;
/// Returns one of the `RVE_OBJECT_*` state codes. Appended in ABI v9.
pub type RveObjectStateFn = unsafe extern "C" fn(ctx: *mut c_void, object: u64) -> u32;
/// Requests a heavy prefab load or unload by path; returns `false` when the host refuses.
/// Appended in ABI v9.
pub type RvePrefabRequestFn = unsafe extern "C" fn(ctx: *mut c_void, path: RveStr) -> bool;
/// Returns one of the `RVE_PREFAB_*` state codes for a prefab path. Appended in ABI v9.
pub type RvePrefabStateFn = unsafe extern "C" fn(ctx: *mut c_void, path: RveStr) -> u32;
/// Playback options for `animation_play`. Appended in ABI v10.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RvePlayAnimationParams {
    /// Crossfade duration in seconds; zero switches at once.
    pub fade: f32,
    /// `-1` keeps the component's default, `0` plays once, `1` loops.
    pub looping: i32,
    /// Motion that starts when this one finishes; an empty string means none.
    pub then: RveStr,
}

/// Starts a motion on one animated object; returns `RveStatus` as a `u32`. Appended in ABI v10.
pub type RveAnimationPlayFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    object: u64,
    motion: RveStr,
    params: *const RvePlayAnimationParams,
) -> u32;
/// Sets a motion's manual weight with a fade; returns `RveStatus` as a `u32`. Appended in ABI v10.
pub type RveAnimationSetWeightFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    object: u64,
    motion: RveStr,
    weight: f32,
    fade: f32,
) -> u32;
/// Sets one blend tree parameter; returns `RveStatus` as a `u32`. Appended in ABI v10.
pub type RveAnimationSetParameterFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, name: RveStr, value: f32) -> u32;
/// Sets the animator speed; returns `RveStatus` as a `u32`. Appended in ABI v10.
pub type RveAnimationSetSpeedFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, speed: f32) -> u32;
/// Stops every motion of one animated object; returns `RveStatus` as a `u32`. Appended in ABI v10.
pub type RveAnimationStopFn = unsafe extern "C" fn(ctx: *mut c_void, object: u64) -> u32;

/// Gamepad snapshot copied by [`RveGamepadStateFn`]. Button bits use [`GamepadButton`] codes and
/// axes use [`GamepadAxis`] codes. Appended in ABI v11.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RveGamepadState {
    /// Non-zero when the slot (or any slot, for the aggregate) has a connected gamepad.
    pub connected: u32,
    /// Button bits held now.
    pub down: u32,
    /// Button bits that became held since the previous script update.
    pub pressed: u32,
    /// Button bits that became released since the previous script update.
    pub released: u32,
    /// Analog axis values in [`GamepadAxis`] code order.
    pub axes: [f32; 6],
}

/// Copies one gamepad snapshot. `pad` is 0 for the aggregate of all slots or 1..=4 for one slot.
/// Returns `false` and zeroes `out` for any other slot. Appended in ABI v11.
pub type RveGamepadStateFn =
    unsafe extern "C" fn(ctx: *mut c_void, pad: u32, out: *mut RveGamepadState) -> bool;

/// Queues one rumble request for `pad` (0 for every connected gamepad, 1..=4 for one slot).
/// Appended in ABI v11.
pub type RveGamepadRumbleFn =
    unsafe extern "C" fn(ctx: *mut c_void, pad: u32, strong: f32, weak: f32, seconds: f32) -> bool;
/// Network mode code returned by [`RveHost::net_mode`]: no network.
pub const RVE_NET_MODE_OFFLINE: u32 = 0;
/// Network mode code: listen server, which also has a local client.
pub const RVE_NET_MODE_HOST: u32 = 1;
/// Network mode code: dedicated server.
pub const RVE_NET_MODE_SERVER: u32 = 2;
/// Network mode code: client connected to a server.
pub const RVE_NET_MODE_CLIENT: u32 = 3;

/// Message target code: the server.
pub const RVE_NET_TARGET_SERVER: u32 = 0;
/// Message target code: every connected peer.
pub const RVE_NET_TARGET_ALL: u32 = 1;
/// Message target code: the peer whose number is passed next to the code.
pub const RVE_NET_TARGET_PEER: u32 = 2;

/// Network status code: the request was accepted.
pub const RVE_NET_OK: u32 = 0;
/// Network status code: the host refused the request in the current phase or state.
pub const RVE_NET_REJECTED: u32 = 1;
/// Network status code: the arguments violate the message or peer limits.
pub const RVE_NET_INVALID_INPUT: u32 = 2;

/// Net event kind: a message from a peer. Also sets `value`.
pub const RVE_NET_EVENT_MESSAGE: u32 = 0;
/// Net event kind: a peer connected.
pub const RVE_NET_EVENT_PEER_CONNECTED: u32 = 1;
/// Net event kind: a peer disconnected; `reason` is set.
pub const RVE_NET_EVENT_PEER_DISCONNECTED: u32 = 2;

/// Returns one of the `RVE_NET_MODE_*` codes.
pub type RveNetModeFn = unsafe extern "C" fn(ctx: *mut c_void) -> u32;
/// Returns the local peer number.
pub type RveNetLocalPeerFn = unsafe extern "C" fn(ctx: *mut c_void) -> u32;
/// Copies up to `capacity` remote peer numbers into `out` and writes the total count to
/// `count`. A guest may call it with zero capacity to learn the count first.
pub type RveNetPeersFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    out: *mut u32,
    capacity: usize,
    count: *mut usize,
) -> bool;
/// Stages a message for `target_kind` (`RVE_NET_TARGET_*`) and `peer`; returns `RVE_NET_*`.
pub type RveNetSendFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    target_kind: u32,
    peer: u32,
    name: RveStr,
    value: *const RveValue,
    reliable: bool,
) -> u32;
/// Copies one object's network number into `out`; returns false when it has no identity.
pub type RveNetObjectIdFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut u32) -> bool;
/// Writes one object's network boolean into `out`; returns false when the object is unavailable.
pub type RveNetObjectBoolFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut bool) -> bool;
/// Stages the owner of one object; returns `RVE_NET_*`.
pub type RveNetObjectSetOwnerFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, peer: u32) -> u32;
/// Stages a message for the same object on the other side; returns `RVE_NET_*`.
pub type RveNetObjectSendFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    object: u64,
    target_kind: u32,
    peer: u32,
    name: RveStr,
    value: *const RveValue,
    reliable: bool,
) -> u32;

/// Результат UI-вызова хоста: `RVE_UI_OK`, `RVE_UI_REJECTED` или `RVE_UI_INVALID_INPUT`.
pub const RVE_UI_OK: u32 = 0;
/// Хост отказал: устаревший хендл, неподдерживаемое свойство или запись запрещена в этой фазе.
pub const RVE_UI_REJECTED: u32 = 1;
/// Значение не подходит: не конечное число, неверный индекс, объявленная картинка не найдена.
pub const RVE_UI_INVALID_INPUT: u32 = 2;

/// Свойства канваса; передаются в `ui_get`/`ui_set` с элементом `0`.
pub const RVE_UI_PROP_CANVAS_VISIBLE: u32 = 0;
/// Слой канваса (`i64` в значении, хост проверяет диапазон `i32`).
pub const RVE_UI_PROP_CANVAS_LAYER: u32 = 1;
/// Текст `Text` или `InputField`.
pub const RVE_UI_PROP_TEXT: u32 = 2;
/// Значение элемента: число, флаг, индекс выпадающего списка (с нуля) или текст.
pub const RVE_UI_PROP_VALUE: u32 = 3;
pub const RVE_UI_PROP_VISIBLE: u32 = 4;
pub const RVE_UI_PROP_INTERACTABLE: u32 = 5;
/// Цвет элемента в `RveUiValue::color`.
pub const RVE_UI_PROP_COLOR: u32 = 6;

/// Вид значения в [`RveUiValue::kind`]; `NONE` означает отсутствие значения в событии.
pub const RVE_UI_VALUE_NONE: u32 = 0;
pub const RVE_UI_VALUE_NUMBER: u32 = 1;
pub const RVE_UI_VALUE_BOOL: u32 = 2;
pub const RVE_UI_VALUE_INDEX: u32 = 3;
pub const RVE_UI_VALUE_TEXT: u32 = 4;
pub const RVE_UI_VALUE_COLOR: u32 = 5;
/// Integer payload for the canvas layer.
pub const RVE_UI_VALUE_INTEGER: u32 = 6;

/// Вид события UI в [`RveUiEvent::kind`].
pub const RVE_UI_EVENT_CLICK: u32 = 0;
pub const RVE_UI_EVENT_VALUE_CHANGED: u32 = 1;
pub const RVE_UI_EVENT_SUBMIT: u32 = 2;
pub const RVE_UI_EVENT_CANCEL: u32 = 3;
pub const RVE_UI_EVENT_FOCUS: u32 = 4;
pub const RVE_UI_EVENT_BLUR: u32 = 5;
pub const RVE_UI_EVENT_HOVER_ENTER: u32 = 6;
pub const RVE_UI_EVENT_HOVER_EXIT: u32 = 7;

/// Значение элемента или свойства канваса. Индекс выпадающего списка считается с нуля.
/// Строка заимствована: хост или гость владеют её памятью только на время вызова.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveUiValue {
    /// One of the `RVE_UI_VALUE_*` kinds.
    pub kind: u32,
    /// Boolean payload for `RVE_UI_VALUE_BOOL`.
    pub flag: bool,
    /// Number payload for `RVE_UI_VALUE_NUMBER`.
    pub number: f64,
    /// Index payload for `RVE_UI_VALUE_INDEX`, and integer payload for `RVE_UI_VALUE_INTEGER`.
    pub integer: i64,
    /// Text payload for `RVE_UI_VALUE_TEXT`.
    pub text: RveStr,
    /// Color payload for `RVE_UI_VALUE_COLOR`, as sRGB bytes.
    pub color: [u8; 4],
}

/// Прямоугольник элемента: якоря, pivot и смещения в единицах канваса.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RveUiRect {
    pub anchor_min: [f32; 2],
    pub anchor_max: [f32; 2],
    pub pivot: [f32; 2],
    pub offset_min: [f32; 2],
    pub offset_max: [f32; 2],
}

/// Событие UI, доставляемое `on_ui_event`. Строки и значение действительны только для callback.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveUiEvent {
    /// One of the `RVE_UI_EVENT_*` kinds.
    pub kind: u32,
    /// Canvas handle as returned by `ui_open`.
    pub canvas: u64,
    /// Element index in the canvas document.
    pub element: u32,
    /// Element identifier from the document.
    pub element_id: RveStr,
    /// Event value; `kind` is `RVE_UI_VALUE_NONE` when the event carries none.
    pub value: RveUiValue,
}

/// Opens a preloaded `.rveui` document owned by the calling object; `out` receives the canvas.
pub type RveUiOpenFn = unsafe extern "C" fn(ctx: *mut c_void, path: RveStr, out: *mut u64) -> u32;
/// Closes a canvas at commit; returns `RVE_UI_*`.
pub type RveUiCloseFn = unsafe extern "C" fn(ctx: *mut c_void, canvas: u64) -> u32;
/// Writes the canvas of an object's `ui_canvas` component; false when there is none.
pub type RveUiObjectCanvasFn =
    unsafe extern "C" fn(ctx: *mut c_void, object: u64, out: *mut u64) -> bool;
/// Writes the focused element; false when nothing has focus.
pub type RveUiFocusedFn =
    unsafe extern "C" fn(ctx: *mut c_void, canvas: *mut u64, element: *mut u32) -> bool;
/// Whether the pointer was over interactive UI on the last update.
pub type RveUiPointerOverFn = unsafe extern "C" fn(ctx: *mut c_void) -> bool;
/// Writes the element index with this identifier; false when the canvas has no such element.
pub type RveUiFindFn =
    unsafe extern "C" fn(ctx: *mut c_void, canvas: u64, id: RveStr, out: *mut u32) -> bool;
/// Reads a canvas or element property into `out`.
pub type RveUiGetFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    canvas: u64,
    element: u32,
    property: u32,
    out: *mut RveUiValue,
) -> u32;
/// Writes a canvas or element property from `value`.
pub type RveUiSetFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    canvas: u64,
    element: u32,
    property: u32,
    value: *const RveUiValue,
) -> u32;
/// Reads an element rectangle into `out`.
pub type RveUiGetRectFn =
    unsafe extern "C" fn(ctx: *mut c_void, canvas: u64, element: u32, out: *mut RveUiRect) -> u32;
/// Writes an element rectangle.
pub type RveUiSetRectFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    canvas: u64,
    element: u32,
    rect: *const RveUiRect,
) -> u32;
/// Replaces the options of a dropdown; `options` holds `count` borrowed strings.
pub type RveUiSetOptionsFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    canvas: u64,
    element: u32,
    options: *const RveStr,
    count: usize,
) -> u32;
/// Sets the image of an `Image` or `Button` element to a declared `.png` path.
pub type RveUiSetImageFn =
    unsafe extern "C" fn(ctx: *mut c_void, canvas: u64, element: u32, path: RveStr) -> u32;
/// Moves focus to a focusable element.
pub type RveUiFocusFn = unsafe extern "C" fn(ctx: *mut c_void, canvas: u64, element: u32) -> u32;
/// Guest `on_ui_event` callback.
pub type RveUiEventFn = unsafe extern "C" fn(
    instance: *mut c_void,
    host: *const RveHost,
    event: RveUiEvent,
) -> RveStatus;

/// Host functions available during one guest call. The value is call-scoped and thread-local.
#[repr(C)]
pub struct RveHost {
    /// Opaque call-scoped host context passed back to every callback.
    pub ctx: *mut c_void,
    /// Time snapshot for the current lifecycle phase.
    pub time: RveTime,
    /// Current object's handle, encoded as `(generation << 32) | (index + 1)`. The generation
    /// keeps a stale handle from matching a reused slot; zero means no object.
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
    /// Audio callbacks appended in ABI v7; `None` marks an unavailable callback.
    pub audio_play_source: Option<RveAudioObjectFn>,
    pub audio_stop_source: Option<RveAudioObjectFn>,
    pub audio_set_source_volume: Option<RveAudioObjectVolumeFn>,
    pub audio_play_clip: Option<RveAudioPlayClipFn>,
    pub audio_stop_voice: Option<RveAudioVoiceFn>,
    pub audio_set_group_volume: Option<RveAudioGroupVolumeFn>,
    /// Copies the cursor state applied by the host; appended in ABI v8.
    pub cursor_state: Option<RveCursorStateFn>,
    /// Stages cursor visibility; appended in ABI v8.
    pub set_cursor_visible: Option<RveSetCursorFn>,
    /// Stages cursor locking; appended in ABI v8.
    pub set_cursor_locked: Option<RveSetCursorFn>,
    /// Spawn, destroy, object-state and prefab callbacks appended in ABI v9; `None` marks an
    /// unavailable callback.
    pub spawn: Option<RveSpawnFn>,
    pub destroy: Option<RveDestroyObjectFn>,
    pub object_state: Option<RveObjectStateFn>,
    pub prefab_load: Option<RvePrefabRequestFn>,
    pub prefab_unload: Option<RvePrefabRequestFn>,
    pub prefab_state: Option<RvePrefabStateFn>,
    /// Animation callbacks appended in ABI v10; `None` marks an unavailable callback.
    pub animation_play: Option<RveAnimationPlayFn>,
    pub animation_set_weight: Option<RveAnimationSetWeightFn>,
    pub animation_set_parameter: Option<RveAnimationSetParameterFn>,
    pub animation_set_speed: Option<RveAnimationSetSpeedFn>,
    pub animation_stop: Option<RveAnimationStopFn>,
    /// Gamepad state and rumble callbacks appended in ABI v11; `None` marks an unavailable callback.
    pub gamepad_state: Option<RveGamepadStateFn>,
    pub gamepad_rumble: Option<RveGamepadRumbleFn>,
    /// Copies one camera's post-process settings; appended in ABI v12.
    pub get_post_process: Option<RveGetPostProcessFn>,
    /// Stages one camera's post-process settings; appended in ABI v12.
    pub set_post_process: Option<RveSetPostProcessFn>,
    /// Network callbacks appended in ABI v13; `None` marks an unavailable callback.
    pub net_mode: Option<RveNetModeFn>,
    pub net_local_peer: Option<RveNetLocalPeerFn>,
    pub net_peers: Option<RveNetPeersFn>,
    pub net_send: Option<RveNetSendFn>,
    pub net_object_id: Option<RveNetObjectIdFn>,
    pub net_object_owner: Option<RveNetObjectIdFn>,
    pub net_object_is_owner: Option<RveNetObjectBoolFn>,
    pub net_object_has_authority: Option<RveNetObjectBoolFn>,
    pub net_object_set_owner: Option<RveNetObjectSetOwnerFn>,
    pub net_object_send: Option<RveNetObjectSendFn>,
    /// UI callbacks appended in ABI v14; `None` on hosts without the game UI.
    pub ui_open: Option<RveUiOpenFn>,
    pub ui_close: Option<RveUiCloseFn>,
    pub ui_object_canvas: Option<RveUiObjectCanvasFn>,
    pub ui_focused: Option<RveUiFocusedFn>,
    pub ui_pointer_over: Option<RveUiPointerOverFn>,
    pub ui_find: Option<RveUiFindFn>,
    pub ui_get: Option<RveUiGetFn>,
    pub ui_set: Option<RveUiSetFn>,
    pub ui_get_rect: Option<RveUiGetRectFn>,
    pub ui_set_rect: Option<RveUiSetRectFn>,
    pub ui_set_options: Option<RveUiSetOptionsFn>,
    pub ui_set_image: Option<RveUiSetImageFn>,
    pub ui_focus: Option<RveUiFocusFn>,
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

/// Guest network event callback. `event` is valid only for the call.
pub type RveNetEventFn = unsafe extern "C" fn(
    instance: *mut c_void,
    host: *const RveHost,
    event: RveNetEvent,
) -> RveStatus;

/// One network event delivered to a guest instance. Text fields are empty and `value` is null
/// when the event kind does not use them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RveNetEvent {
    /// One of the `RVE_NET_EVENT_*` kinds.
    pub kind: u32,
    /// Sender for messages; the connected or disconnected peer otherwise.
    pub peer: u32,
    /// Message name; empty for peer events.
    pub name: RveStr,
    /// Message value; null for peer events.
    pub value: *const RveValue,
    /// Disconnect reason; empty for other kinds.
    pub reason: RveStr,
}

/// One exported guest script implementation in ABI version 6 through 12.
#[repr(C)]
pub struct RveScriptDescV6 {
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

/// One exported guest script implementation in ABI version 13, kept for modules built against v13.
///
/// The host normalizes it to [`RveScriptDesc`] with `ui_event` set to `None`.
#[repr(C)]
pub struct RveScriptDescV13 {
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
    pub collision: Option<RveCollisionFn>,
    /// Optional network event callback appended in ABI v13.
    pub net_event: Option<RveNetEventFn>,
}

/// One exported guest script implementation in ABI version 14.
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
    pub collision: Option<RveCollisionFn>,
    pub net_event: Option<RveNetEventFn>,
    /// Optional UI event callback appended in ABI v14.
    pub ui_event: Option<RveUiEventFn>,
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
unsafe impl Sync for RveScriptDescV6 {}
unsafe impl Sync for RveScriptDescV13 {}
unsafe impl Sync for RveScriptDesc {}
unsafe impl Sync for RveModule {}

#[cfg(test)]
mod tests {
    use super::{
        Key, MouseButton, PhysicalInput, RVE_ANTI_ALIASING_DLSS, RVE_ANTI_ALIASING_FSR,
        RVE_ANTI_ALIASING_FXAA, RVE_ANTI_ALIASING_NONE, RVE_ANTI_ALIASING_TAA, RVE_AO_QUALITY_HIGH,
        RVE_EXPOSURE_MODE_AUTO, RVE_EXPOSURE_MODE_MANUAL, RVE_SCRIPT_ABI_VERSION,
        RVE_TONEMAPPER_ACES, RVE_TONEMAPPER_NEUTRAL, RVE_TONEMAPPER_NONE, RveAnimationStopFn,
        RveCameraSettings, RveGamepadRumbleFn, RveGamepadStateFn, RveGetCameraSettingsFn,
        RveGetPostProcessFn, RveHost, RvePostProcessSettings,
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

    #[test]
    fn post_process_settings_are_c_layout_and_appended_to_host_in_v12() {
        assert_eq!(RVE_SCRIPT_ABI_VERSION, 14);
        assert_eq!(RVE_EXPOSURE_MODE_AUTO, 1);
        assert_eq!(RVE_AO_QUALITY_HIGH, 2);
        assert_eq!(std::mem::align_of::<RvePostProcessSettings>(), 4);
        assert_eq!(
            std::mem::offset_of!(RvePostProcessSettings, exposure_enabled),
            0
        );
        assert_eq!(std::mem::offset_of!(RvePostProcessSettings, exposure_ev), 8);
        assert_eq!(std::mem::offset_of!(RvePostProcessSettings, tonemapper), 40);
        assert_eq!(std::mem::offset_of!(RvePostProcessSettings, bloom_tint), 64);
        assert_eq!(
            std::mem::offset_of!(RvePostProcessSettings, motion_samples),
            228
        );
        assert_eq!(std::mem::size_of::<RvePostProcessSettings>(), 236);
        assert_eq!(
            std::mem::offset_of!(RveHost, gamepad_state),
            std::mem::offset_of!(RveHost, animation_stop)
                + std::mem::size_of::<Option<RveAnimationStopFn>>()
        );
        assert_eq!(
            std::mem::offset_of!(RveHost, gamepad_rumble),
            std::mem::offset_of!(RveHost, gamepad_state)
                + std::mem::size_of::<Option<RveGamepadStateFn>>()
        );
        assert_eq!(
            std::mem::offset_of!(RveHost, get_post_process),
            std::mem::offset_of!(RveHost, gamepad_rumble)
                + std::mem::size_of::<Option<RveGamepadRumbleFn>>()
        );
        assert_eq!(
            std::mem::offset_of!(RveHost, set_post_process),
            std::mem::offset_of!(RveHost, get_post_process)
                + std::mem::size_of::<Option<RveGetPostProcessFn>>()
        );
        // Camera settings keep the v2 layout: post-process data never changes it.
        assert_eq!(std::mem::size_of::<RveCameraSettings>(), 40);
    }

    #[test]
    fn post_process_defaults_match_engine_defaults() {
        let defaults = RvePostProcessSettings::default();
        assert_eq!(defaults.exposure_enabled, 1);
        assert_eq!(defaults.exposure_mode, RVE_EXPOSURE_MODE_MANUAL);
        assert_eq!(defaults.tonemapper, RVE_TONEMAPPER_ACES);
        assert_eq!(defaults.bloom_enabled, 0);
        assert_eq!(defaults.motion_samples, 12);
    }
}
