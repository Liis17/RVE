//! Source SDK for Rust scripts loaded by the RVE player.
//!
//! The guest ABI catches Rust unwinds at every exported callback. A game profile using
//! `panic = "abort"` still terminates the process on panic because aborting panics cannot unwind.

pub mod abi;

/// Version of this SDK crate, as declared in its `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use abi::{GamepadAxis, GamepadButton, Key, MouseButton, PhysicalInput};
use abi::{
    RVE_ANTI_ALIASING_DLSS, RVE_ANTI_ALIASING_FSR, RVE_ANTI_ALIASING_FXAA, RVE_ANTI_ALIASING_NONE,
    RVE_ANTI_ALIASING_TAA, RVE_NET_INVALID_INPUT, RVE_NET_MODE_CLIENT, RVE_NET_MODE_HOST,
    RVE_NET_MODE_SERVER, RVE_NET_OK, RVE_NET_TARGET_ALL, RVE_NET_TARGET_PEER,
    RVE_NET_TARGET_SERVER, RVE_TONEMAPPER_ACES, RVE_TONEMAPPER_NEUTRAL, RVE_TONEMAPPER_NONE,
    RVE_VALUE_ARRAY, RVE_VALUE_BOOL, RVE_VALUE_FLOAT, RVE_VALUE_INT, RVE_VALUE_STR, RveArray,
    RveCameraSettings, RveCharacterMoveResult, RveHost, RveParams, RvePostProcessSettings,
    RveRaycastHit, RveScreenShaderState, RveStr, RveTime, RveTransform, RveValue, RveValueData,
};
use abi::{
    RVE_UI_EVENT_BLUR, RVE_UI_EVENT_CANCEL, RVE_UI_EVENT_CLICK, RVE_UI_EVENT_FOCUS,
    RVE_UI_EVENT_HOVER_ENTER, RVE_UI_EVENT_HOVER_EXIT, RVE_UI_EVENT_SUBMIT,
    RVE_UI_EVENT_VALUE_CHANGED, RVE_UI_INVALID_INPUT, RVE_UI_OK, RVE_UI_PROP_CANVAS_LAYER,
    RVE_UI_PROP_CANVAS_VISIBLE, RVE_UI_PROP_COLOR, RVE_UI_PROP_INTERACTABLE, RVE_UI_PROP_TEXT,
    RVE_UI_PROP_VALUE, RVE_UI_PROP_VISIBLE, RVE_UI_VALUE_BOOL, RVE_UI_VALUE_COLOR,
    RVE_UI_VALUE_INDEX, RVE_UI_VALUE_INTEGER, RVE_UI_VALUE_NONE, RVE_UI_VALUE_NUMBER,
    RVE_UI_VALUE_TEXT, RveUiRect, RveUiValue,
};
use std::collections::BTreeMap;
use std::marker::PhantomData;

/// Owned script value, independent of the borrowed ABI memory.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Boolean value.
    Bool(bool),
    /// Signed integer value.
    Int(i64),
    /// Floating-point value.
    Float(f64),
    /// Owned UTF-8 string.
    String(String),
    /// Ordered array of values.
    Array(Vec<Value>),
}

/// Owned script parameters in deterministic key order.
pub type Params = BTreeMap<String, Value>;

/// A malformed borrowed ABI value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValueError(
    /// Description of malformed ABI data.
    pub String,
);

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ValueError {}

impl RveValue {
    /// Copies a borrowed ABI value into guest-owned memory.
    ///
    /// # Safety
    /// All nonempty strings and arrays reachable from this value must point to initialized,
    /// readable storage for the duration of this call. The `tag` must select the active union
    /// field.
    pub unsafe fn into_owned(self) -> Result<Value, ValueError> {
        // SAFETY: the caller guarantees pointer validity and tag/union agreement.
        unsafe { owned_value(self) }
    }
}

impl RveParams {
    /// Copies a borrowed ABI parameter table into guest-owned memory.
    ///
    /// # Safety
    /// If `len` is nonzero, `entries` must point to `len` initialized entries. Every key and
    /// value must satisfy the pointer contract of [`RveValue::into_owned`]. Duplicate keys are
    /// rejected.
    pub unsafe fn into_owned(self) -> Result<Params, ValueError> {
        if self.len == 0 {
            return Ok(Params::new());
        }
        if self.entries.is_null() {
            return Err(ValueError("null parameter entries".to_owned()));
        }
        // SAFETY: validated non-null and guaranteed readable by the caller.
        let entries = unsafe { std::slice::from_raw_parts(self.entries, self.len) };
        let mut params = Params::new();
        for entry in entries {
            // SAFETY: the caller guarantees each borrowed key is readable UTF-8.
            let key = unsafe { entry.key.to_str() }
                .map_err(|error| ValueError(format!("parameter key is not UTF-8: {error}")))?
                .to_owned();
            // SAFETY: inherited from this method's contract.
            let value = unsafe { entry.value.into_owned() }?;
            if params.insert(key.clone(), value).is_some() {
                return Err(ValueError(format!("duplicate parameter key {key:?}")));
            }
        }
        Ok(params)
    }
}

unsafe fn owned_value(value: RveValue) -> Result<Value, ValueError> {
    use abi::{RVE_VALUE_ARRAY, RVE_VALUE_BOOL, RVE_VALUE_FLOAT, RVE_VALUE_INT, RVE_VALUE_STR};
    match value.tag {
        RVE_VALUE_BOOL => {
            // SAFETY: the tag selects the bool union field.
            Ok(Value::Bool(unsafe { value.data.boolean }))
        }
        RVE_VALUE_INT => {
            // SAFETY: the tag selects the i64 union field.
            Ok(Value::Int(unsafe { value.data.integer }))
        }
        RVE_VALUE_FLOAT => {
            // SAFETY: the tag selects the f64 union field.
            Ok(Value::Float(unsafe { value.data.float }))
        }
        RVE_VALUE_STR => {
            // SAFETY: the tag selects the borrowed string union field.
            let string = unsafe { value.data.string };
            // SAFETY: forwarded from the caller's contract.
            let string = unsafe { string.to_str() }
                .map_err(|error| ValueError(format!("value string is not UTF-8: {error}")))?;
            Ok(Value::String(string.to_owned()))
        }
        RVE_VALUE_ARRAY => {
            // SAFETY: the tag selects the borrowed array union field.
            let array = unsafe { value.data.array };
            if array.len == 0 {
                return Ok(Value::Array(Vec::new()));
            }
            if array.ptr.is_null() {
                return Err(ValueError("null array values".to_owned()));
            }
            // SAFETY: forwarded from the caller's pointer contract.
            let values = unsafe { std::slice::from_raw_parts(array.ptr, array.len) };
            values
                .iter()
                .copied()
                // SAFETY: all nested values are covered by the caller's contract.
                .map(|value| unsafe { owned_value(value) })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        tag => Err(ValueError(format!("unknown value tag {tag}"))),
    }
}

/// Local transform exposed to a guest script. Rotation is in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    /// Local position.
    pub position: [f32; 3],
    /// Local Euler rotation in degrees.
    pub rotation_deg: [f32; 3],
    /// Local scale.
    pub scale: [f32; 3],
}

impl From<RveTransform> for Transform {
    fn from(value: RveTransform) -> Self {
        Self {
            position: value.position,
            rotation_deg: value.rotation_deg,
            scale: value.scale,
        }
    }
}

impl From<Transform> for RveTransform {
    fn from(value: Transform) -> Self {
        Self {
            position: value.position,
            rotation_deg: value.rotation_deg,
            scale: value.scale,
        }
    }
}

/// Camera anti-aliasing mode supported by the renderer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum AntiAliasing {
    /// FXAA 3.11 quality output pass.
    #[default]
    Fxaa = RVE_ANTI_ALIASING_FXAA,
    /// Bilinear output filtering.
    None = RVE_ANTI_ALIASING_NONE,
    /// Temporal anti-aliasing and upscaling.
    Taa = RVE_ANTI_ALIASING_TAA,
    /// AMD FidelityFX upscaling; falls back to TAA when the backend is unavailable.
    Fsr = RVE_ANTI_ALIASING_FSR,
    /// NVIDIA DLSS upscaling; falls back to TAA when the backend is unavailable.
    Dlss = RVE_ANTI_ALIASING_DLSS,
}

impl AntiAliasing {
    /// Converts a stable ABI code into a supported anti-aliasing mode.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            RVE_ANTI_ALIASING_FXAA => Some(Self::Fxaa),
            RVE_ANTI_ALIASING_NONE => Some(Self::None),
            RVE_ANTI_ALIASING_TAA => Some(Self::Taa),
            RVE_ANTI_ALIASING_FSR => Some(Self::Fsr),
            RVE_ANTI_ALIASING_DLSS => Some(Self::Dlss),
            _ => None,
        }
    }

    /// Returns the stable ABI code for this mode.
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// HDR-to-display tone mapping operator.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum Tonemapper {
    /// ACES fitted curve.
    #[default]
    Aces = RVE_TONEMAPPER_ACES,
    /// Khronos PBR Neutral curve.
    Neutral = RVE_TONEMAPPER_NEUTRAL,
    /// Clamp HDR values directly to the display range.
    None = RVE_TONEMAPPER_NONE,
}

impl Tonemapper {
    /// Converts a stable ABI code into a supported tonemapper.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            RVE_TONEMAPPER_ACES => Some(Self::Aces),
            RVE_TONEMAPPER_NEUTRAL => Some(Self::Neutral),
            RVE_TONEMAPPER_NONE => Some(Self::None),
            _ => None,
        }
    }

    /// Returns the stable ABI code for this tonemapper.
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// Flattened projection, render, and color settings for one scene camera.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraSettings {
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
    /// Spatial output filtering mode.
    pub anti_aliasing: AntiAliasing,
    /// HDR-to-display tone mapping operator.
    pub tonemapper: Tonemapper,
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            fov_deg: 60.0,
            near: 0.1,
            far: 1000.0,
            render_scale: 1.0,
            clear_color: [0.1, 0.1, 0.12],
            exposure_ev: 0.0,
            anti_aliasing: AntiAliasing::Fxaa,
            tonemapper: Tonemapper::Aces,
        }
    }
}

impl TryFrom<RveCameraSettings> for CameraSettings {
    type Error = ();

    fn try_from(value: RveCameraSettings) -> Result<Self, Self::Error> {
        let settings = Self {
            fov_deg: value.fov_deg,
            near: value.near,
            far: value.far,
            render_scale: value.render_scale,
            clear_color: value.clear_color,
            exposure_ev: value.exposure_ev,
            anti_aliasing: AntiAliasing::from_code(value.anti_aliasing).ok_or(())?,
            tonemapper: Tonemapper::from_code(value.tonemapper).ok_or(())?,
        };
        if settings.is_valid() {
            Ok(settings)
        } else {
            Err(())
        }
    }
}

impl From<CameraSettings> for RveCameraSettings {
    fn from(value: CameraSettings) -> Self {
        Self {
            fov_deg: value.fov_deg,
            near: value.near,
            far: value.far,
            render_scale: value.render_scale,
            clear_color: value.clear_color,
            exposure_ev: value.exposure_ev,
            anti_aliasing: value.anti_aliasing.code(),
            tonemapper: value.tonemapper.code(),
        }
    }
}

impl CameraSettings {
    fn is_valid(self) -> bool {
        self.fov_deg.is_finite()
            && self.fov_deg > 0.0
            && self.fov_deg < 180.0
            && self.near.is_finite()
            && self.far.is_finite()
            && self.near > 0.0
            && self.far > self.near
            && self.render_scale.is_finite()
            && self.render_scale > 0.0
            && self.render_scale <= 1.0
            && self
                .clear_color
                .into_iter()
                .all(|value| value.is_finite() && value >= 0.0)
            && self.exposure_ev.is_finite()
            && (-16.0..=16.0).contains(&self.exposure_ev)
    }
}

/// Lifecycle implemented by a guest script.
pub trait Script: 'static {
    /// Called once before the first update.
    fn on_start(&mut self, _ctx: &mut Context<'_>) {}
    /// Called for each update with the frame delta in seconds.
    fn on_update(&mut self, _ctx: &mut Context<'_>, _dt: f32) {}
    /// Called when a message is delivered to this script.
    fn on_message(&mut self, _ctx: &mut Context<'_>, _sender: &str, _name: &str, _value: &Value) {}
    /// Called when this object begins touching another collider.
    fn on_collision_enter(
        &mut self,
        _ctx: &mut Context<'_>,
        _other: Object,
        _contact: CollisionContact,
    ) {
    }
    /// Called when this object stops touching another collider.
    fn on_collision_exit(&mut self, _ctx: &mut Context<'_>, _other: Object) {}
    /// Called when this object begins overlapping a trigger.
    fn on_trigger_enter(&mut self, _ctx: &mut Context<'_>, _other: Object) {}
    /// Called when this object stops overlapping a trigger.
    fn on_trigger_exit(&mut self, _ctx: &mut Context<'_>, _other: Object) {}
    /// Called when the script host is destroyed after this instance has started.
    fn on_destroy(&mut self, _ctx: &mut Context<'_>) {}
    /// Called when a registered developer-console command is submitted.
    fn on_command(&mut self, _ctx: &mut Context<'_>, _name: &str, _args: &[&str]) {}
    /// Called when a network message for this object or broadcast reaches the script.
    fn on_net_message(&mut self, _ctx: &mut Context<'_>, _peer: u32, _name: &str, _value: &Value) {}
    /// Called when a peer connects; delivered to every instance with this callback.
    fn on_peer_connected(&mut self, _ctx: &mut Context<'_>, _peer: u32) {}
    /// Called when a peer disconnects; delivered to every instance with this callback.
    fn on_peer_disconnected(&mut self, _ctx: &mut Context<'_>, _peer: u32, _reason: &str) {}
    /// Called for a UI event on a canvas owned by this object, at the start of update.
    fn on_ui_event(&mut self, _ctx: &mut Context<'_>, _event: UiEvent) {}
}

/// Call-scoped access to the host and this script's object.
///
/// The host pointer is valid only while a guest callback is running. `Context` is deliberately
/// neither `Send` nor `Sync`.
pub struct Context<'a> {
    host: &'a RveHost,
    initial_transform: Transform,
    /// Mutable local transform of this script's object.
    pub transform: Transform,
    /// Console logging and command registration for this script instance.
    pub console: Console<'a>,
    _not_send_or_sync: PhantomData<*const ()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleLevel {
    Log,
    Warning,
    Error,
}

/// Call-scoped console API exposed to a script.
pub struct Console<'a> {
    host: &'a RveHost,
}

/// Read-only snapshot of keyboard, mouse, and wheel input for the current script callback.
pub struct Input<'a> {
    host: &'a RveHost,
}

/// Stick side for [`GamepadState::stick`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GamepadStick {
    Left,
    Right,
}

/// Read-only gamepad snapshot for one slot, or for the aggregate of all slots.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GamepadState {
    state: abi::RveGamepadState,
}

impl GamepadState {
    /// Returns whether a gamepad is connected in this slot (any slot for the aggregate).
    pub fn connected(&self) -> bool {
        self.state.connected != 0
    }

    /// Returns whether a button is currently held.
    pub fn down(&self, button: GamepadButton) -> bool {
        self.state.down & (1 << button as u32) != 0
    }

    /// Returns whether a button became held since the previous script update.
    pub fn pressed(&self, button: GamepadButton) -> bool {
        self.state.pressed & (1 << button as u32) != 0
    }

    /// Returns whether a button became released since the previous script update.
    pub fn released(&self, button: GamepadButton) -> bool {
        self.state.released & (1 << button as u32) != 0
    }

    /// Returns an analog axis value: sticks in [-1, 1] and triggers in [0, 1].
    pub fn axis(&self, axis: GamepadAxis) -> f32 {
        self.state.axes[axis as usize]
    }

    /// Returns a stick as `[x, y]`, with positive `y` pointing up.
    pub fn stick(&self, stick: GamepadStick) -> [f32; 2] {
        match stick {
            GamepadStick::Left => [self.axis(GamepadAxis::LeftX), self.axis(GamepadAxis::LeftY)],
            GamepadStick::Right => [
                self.axis(GamepadAxis::RightX),
                self.axis(GamepadAxis::RightY),
            ],
        }
    }
}

/// Physics query access available during one script callback.
pub struct Physics<'a> {
    host: &'a RveHost,
}

/// Result of a physics raycast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsRaycastHit {
    pub object: Object,
    pub point: [f32; 3],
    pub normal: [f32; 3],
    pub distance: f32,
}

/// Result of character capsule movement.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CharacterMove {
    pub translation: [f32; 3],
    pub grounded: bool,
    pub collided: bool,
}

/// Contact data for collision-enter callbacks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CollisionContact {
    pub point: [f32; 3],
    /// Points from this script object's collider toward the other collider.
    pub normal: [f32; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicsError {
    Unavailable,
    Rejected,
    InvalidInput,
}

impl std::fmt::Display for PhysicsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "physics callback is unavailable in this host",
            Self::Rejected => "physics operation was rejected by the host",
            Self::InvalidInput => "physics input must be finite and valid",
        })
    }
}

impl std::error::Error for PhysicsError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioError {
    /// The host predates ABI v7 and provides no audio callbacks.
    Unavailable,
    /// The host refused the request; the reason is reported in the host's script events.
    Rejected,
    /// A volume or position is not finite, or a volume is outside `0.0..=MAX_AUDIO_VOLUME`.
    InvalidInput,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "audio callback is unavailable in this host",
            Self::Rejected => "audio operation was rejected by the host",
            Self::InvalidInput => "audio input must be finite and valid",
        })
    }
}

impl std::error::Error for AudioError {}

/// Mixer group for clips and group volume changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioGroup {
    Master,
    Music,
    Effects,
    Ambient,
}

impl AudioGroup {
    fn code(self) -> u32 {
        match self {
            Self::Master => abi::RVE_AUDIO_GROUP_MASTER,
            Self::Music => abi::RVE_AUDIO_GROUP_MUSIC,
            Self::Effects => abi::RVE_AUDIO_GROUP_EFFECTS,
            Self::Ambient => abi::RVE_AUDIO_GROUP_AMBIENT,
        }
    }
}

/// Options for [`Context::play_clip`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipOptions {
    pub group: AudioGroup,
    pub volume: f32,
    pub looping: bool,
    /// Makes the clip spatial around this point; `None` plays it without position.
    pub position: Option<[f32; 3]>,
}

impl Default for ClipOptions {
    fn default() -> Self {
        Self {
            group: AudioGroup::Effects,
            volume: 1.0,
            looping: false,
            position: None,
        }
    }
}

fn audio_status(status: u32) -> Result<(), AudioError> {
    if status == abi::RveStatus::Ok as u32 {
        Ok(())
    } else {
        Err(AudioError::Rejected)
    }
}

/// Наибольшая громкость (×4). Совпадает с `engine_scene::MAX_AUDIO_VOLUME`: rve-sdk не зависит от
/// engine-scene, поэтому константа повторяется здесь.
pub const MAX_AUDIO_VOLUME: f32 = 4.0;

fn audio_volume(volume: f32) -> Result<f32, AudioError> {
    if volume.is_finite() && (0.0..=MAX_AUDIO_VOLUME).contains(&volume) {
        Ok(volume)
    } else {
        Err(AudioError::InvalidInput)
    }
}

impl Object {
    /// Starts this object's `audio_source` clip.
    pub fn play_sound(&self, ctx: &mut Context<'_>) -> Result<(), AudioError> {
        let callback = ctx.host.audio_play_source.ok_or(AudioError::Unavailable)?;
        // SAFETY: host callback is valid during this guest callback.
        audio_status(unsafe { callback(ctx.host.ctx, self.handle) })
    }

    /// Stops this object's current `audio_source` voice.
    pub fn stop_sound(&self, ctx: &mut Context<'_>) -> Result<(), AudioError> {
        let callback = ctx.host.audio_stop_source.ok_or(AudioError::Unavailable)?;
        // SAFETY: host callback is valid during this guest callback.
        audio_status(unsafe { callback(ctx.host.ctx, self.handle) })
    }

    /// Sets the volume of this object's current voice and of its next starts.
    pub fn set_sound_volume(&self, ctx: &mut Context<'_>, volume: f32) -> Result<(), AudioError> {
        let volume = audio_volume(volume)?;
        let callback = ctx
            .host
            .audio_set_source_volume
            .ok_or(AudioError::Unavailable)?;
        // SAFETY: host callback is valid during this guest callback.
        audio_status(unsafe { callback(ctx.host.ctx, self.handle, volume) })
    }
}

impl Context<'_> {
    /// Starts a clip by virtual path (`.wav` or `.ogg`, declared in the scene's `[[assets]]`) and
    /// returns its voice id, which can later be passed to [`Context::stop_voice`].
    pub fn play_clip(&mut self, path: &str, options: ClipOptions) -> Result<u64, AudioError> {
        let volume = audio_volume(options.volume)?;
        let position = match options.position {
            Some(position) if position.iter().all(|axis| axis.is_finite()) => position,
            Some(_) => return Err(AudioError::InvalidInput),
            None => [0.0; 3],
        };
        let callback = self.host.audio_play_clip.ok_or(AudioError::Unavailable)?;
        let params = abi::RveAudioParams {
            group: options.group.code(),
            volume,
            looping: options.looping,
            has_position: options.position.is_some(),
            position,
        };
        let mut voice = 0;
        // SAFETY: path, params and output storage remain valid during this script call.
        audio_status(unsafe {
            callback(
                self.host.ctx,
                abi::RveStr::from_str_ref(path),
                &params,
                &mut voice,
            )
        })?;
        Ok(voice)
    }

    /// Stops one voice returned by [`Context::play_clip`]. Unknown ids are ignored by the host.
    pub fn stop_voice(&mut self, voice: u64) -> Result<(), AudioError> {
        let callback = self.host.audio_stop_voice.ok_or(AudioError::Unavailable)?;
        // SAFETY: host callback is valid during this script call.
        audio_status(unsafe { callback(self.host.ctx, voice) })
    }

    /// Sets one mixer group's volume; `Master` multiplies every group.
    pub fn set_group_volume(&mut self, group: AudioGroup, volume: f32) -> Result<(), AudioError> {
        let volume = audio_volume(volume)?;
        let callback = self
            .host
            .audio_set_group_volume
            .ok_or(AudioError::Unavailable)?;
        // SAFETY: host callback is valid during this script call.
        audio_status(unsafe { callback(self.host.ctx, group.code(), volume) })
    }
}

/// Optional settings for [`Context::spawn`]. Unset fields keep the prefab's own values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpawnOptions {
    position: Option<[f32; 3]>,
    rotation_deg: Option<[f32; 3]>,
    scale: Option<[f32; 3]>,
    velocity: Option<[f32; 3]>,
    parent: Option<Object>,
    name: Option<String>,
    id: Option<String>,
}

impl SpawnOptions {
    /// Creates options with every field unset.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the new root's local position.
    pub fn position(mut self, value: [f32; 3]) -> Self {
        self.position = Some(value);
        self
    }

    /// Sets the new root's local Euler rotation in degrees.
    pub fn rotation_deg(mut self, value: [f32; 3]) -> Self {
        self.rotation_deg = Some(value);
        self
    }

    /// Sets the new root's local scale.
    pub fn scale(mut self, value: [f32; 3]) -> Self {
        self.scale = Some(value);
        self
    }

    /// Sets the initial velocity. The host applies it only to a dynamic root.
    pub fn velocity(mut self, value: [f32; 3]) -> Self {
        self.velocity = Some(value);
        self
    }

    /// Makes the new root a child of `parent`, which must be a live object.
    pub fn parent(mut self, parent: Object) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Sets the new root's display name.
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    /// Sets the new root's scene id, `[A-Za-z0-9_-]{1,64}`. The host refuses an id in use.
    pub fn id(mut self, id: &str) -> Self {
        self.id = Some(id.to_owned());
        self
    }

    /// Builds the ABI struct. Its strings borrow from `self` and are valid for one host call.
    fn to_abi(&self) -> abi::RveSpawnOptions {
        let mut options = abi::RveSpawnOptions {
            flags: 0,
            position: [0.0; 3],
            rotation_deg: [0.0; 3],
            scale: [0.0; 3],
            velocity: [0.0; 3],
            parent: 0,
            name: abi::RveStr::empty(),
            id: abi::RveStr::empty(),
        };
        if let Some(value) = self.position {
            options.flags |= abi::RVE_SPAWN_HAS_POSITION;
            options.position = value;
        }
        if let Some(value) = self.rotation_deg {
            options.flags |= abi::RVE_SPAWN_HAS_ROTATION;
            options.rotation_deg = value;
        }
        if let Some(value) = self.scale {
            options.flags |= abi::RVE_SPAWN_HAS_SCALE;
            options.scale = value;
        }
        if let Some(value) = self.velocity {
            options.flags |= abi::RVE_SPAWN_HAS_VELOCITY;
            options.velocity = value;
        }
        if let Some(parent) = self.parent {
            options.flags |= abi::RVE_SPAWN_HAS_PARENT;
            options.parent = parent.handle;
        }
        if let Some(name) = &self.name {
            options.flags |= abi::RVE_SPAWN_HAS_NAME;
            options.name = abi::RveStr::from_str_ref(name);
        }
        if let Some(id) = &self.id {
            options.flags |= abi::RVE_SPAWN_HAS_ID;
            options.id = abi::RveStr::from_str_ref(id);
        }
        options
    }
}

/// Heavy-prefab state reported by [`Prefabs::state`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefabState {
    /// The path is not in the project's prefab list.
    Unknown,
    /// A fast prefab of the scene; it is always in memory.
    Resident,
    /// A heavy prefab that is not in memory.
    Unloaded,
    /// A heavy prefab whose load is in progress.
    Loading,
    /// A heavy prefab in memory.
    Loaded,
    /// A heavy prefab whose unload is in progress.
    Unloading,
    /// A heavy prefab whose load failed.
    Failed,
    /// The host predates ABI v9 and provides no prefab callbacks.
    Unavailable,
}

impl PrefabState {
    fn from_abi(code: u32) -> Self {
        match code {
            abi::RVE_PREFAB_RESIDENT => Self::Resident,
            abi::RVE_PREFAB_UNLOADED => Self::Unloaded,
            abi::RVE_PREFAB_LOADING => Self::Loading,
            abi::RVE_PREFAB_LOADED => Self::Loaded,
            abi::RVE_PREFAB_UNLOADING => Self::Unloading,
            abi::RVE_PREFAB_FAILED => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

/// Heavy-prefab loading and state, from [`Context::prefabs`]. Load and unload requests take
/// effect when the current phase ends; [`Prefabs::state`] reports what the host knows now.
#[derive(Clone, Copy)]
pub struct Prefabs<'a> {
    host: &'a RveHost,
}

impl Prefabs<'_> {
    /// Requests a heavy prefab load. Returns `false` for an unknown path, a refused request, or a
    /// host without ABI v9.
    pub fn load(&self, path: &str) -> bool {
        self.request(self.host.prefab_load, path)
    }

    /// Requests a heavy prefab unload once its last instance and pending spawns are gone.
    /// Returns `false` for an unknown path, a refused request, or a host without ABI v9.
    pub fn unload(&self, path: &str) -> bool {
        self.request(self.host.prefab_unload, path)
    }

    /// Returns the prefab's state.
    pub fn state(&self, path: &str) -> PrefabState {
        let Some(callback) = self.host.prefab_state else {
            return PrefabState::Unavailable;
        };
        // SAFETY: the host callback and borrowed path are valid during this guest call.
        let code = unsafe { callback(self.host.ctx, abi::RveStr::from_str_ref(path)) };
        PrefabState::from_abi(code)
    }

    fn request(&self, callback: Option<abi::RvePrefabRequestFn>, path: &str) -> bool {
        let Some(callback) = callback else {
            return false;
        };
        // SAFETY: the host callback and borrowed path are valid during this guest call.
        unsafe { callback(self.host.ctx, abi::RveStr::from_str_ref(path)) }
    }
}

impl Context<'_> {
    /// Spawns a prefab by its project path without a leading slash, for example
    /// `prefabs/bullet.rveprefab`, and returns its pending object.
    ///
    /// The object enters the world when the current phase ends. Its `on_start` runs at the next
    /// update, before messages. `None` means the host refused the request (the reason is in the
    /// script events), an option is not finite, or the host predates ABI v9.
    pub fn spawn(&mut self, prefab: &str, options: &SpawnOptions) -> Option<Object> {
        let callback = self.host.spawn?;
        let finite = [
            options.position,
            options.rotation_deg,
            options.scale,
            options.velocity,
        ]
        .into_iter()
        .flatten()
        .all(finite_vector);
        if !finite {
            return None;
        }
        let options = options.to_abi();
        // SAFETY: the prefab path and the options, with their borrowed strings, outlive this call.
        let handle =
            unsafe { callback(self.host.ctx, abi::RveStr::from_str_ref(prefab), &options) };
        (handle != 0).then_some(Object { handle })
    }

    /// Returns the heavy-prefab API for this call.
    pub fn prefabs(&self) -> Prefabs<'_> {
        Prefabs { host: self.host }
    }
}

/// Animation operation failure in guest code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnimationError {
    /// The host predates ABI v10 and provides no animation callbacks.
    Unavailable,
    /// The host refused the request; the reason is reported in the host's script events.
    Rejected,
    /// A fade, weight, parameter or speed is not finite, or a fade or speed is negative.
    InvalidInput,
}

impl std::fmt::Display for AnimationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "animation callback is unavailable in this host",
            Self::Rejected => "animation operation was rejected by the host",
            Self::InvalidInput => "animation input must be finite and valid",
        })
    }
}

impl std::error::Error for AnimationError {}

/// Options for [`Object::play_animation`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayAnimationOptions<'a> {
    /// Crossfade duration in seconds; zero switches at once.
    pub fade: f32,
    /// `None` keeps the component's default loop setting.
    pub looping: Option<bool>,
    /// Motion that starts when this one finishes.
    pub then: Option<&'a str>,
}

fn animation_status(status: u32) -> Result<(), AnimationError> {
    if status == abi::RveStatus::Ok as u32 {
        Ok(())
    } else {
        Err(AnimationError::Rejected)
    }
}

fn non_negative_finite(value: f32) -> Result<f32, AnimationError> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(AnimationError::InvalidInput)
    }
}

fn finite(value: f32) -> Result<f32, AnimationError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(AnimationError::InvalidInput)
    }
}

impl Object {
    /// Starts `motion` on this object's `animator` with a crossfade and optional loop and `then`.
    pub fn play_animation(
        &self,
        ctx: &mut Context<'_>,
        motion: &str,
        options: PlayAnimationOptions<'_>,
    ) -> Result<(), AnimationError> {
        let fade = non_negative_finite(options.fade)?;
        let callback = ctx.host.animation_play.ok_or(AnimationError::Unavailable)?;
        let params = abi::RvePlayAnimationParams {
            fade,
            looping: match options.looping {
                None => -1,
                Some(false) => 0,
                Some(true) => 1,
            },
            then: abi::RveStr::from_str_ref(options.then.unwrap_or("")),
        };
        // SAFETY: the host callback, the motion string and the params stay valid during this callback.
        animation_status(unsafe {
            callback(
                ctx.host.ctx,
                self.handle,
                abi::RveStr::from_str_ref(motion),
                &params,
            )
        })
    }

    /// Sets the manual weight of one motion; the other motions keep their weights.
    pub fn set_animation_weight(
        &self,
        ctx: &mut Context<'_>,
        motion: &str,
        weight: f32,
        fade: f32,
    ) -> Result<(), AnimationError> {
        let weight = finite(weight)?;
        let fade = non_negative_finite(fade)?;
        let callback = ctx
            .host
            .animation_set_weight
            .ok_or(AnimationError::Unavailable)?;
        // SAFETY: the host callback and the motion string stay valid during this callback.
        animation_status(unsafe {
            callback(
                ctx.host.ctx,
                self.handle,
                abi::RveStr::from_str_ref(motion),
                weight,
                fade,
            )
        })
    }

    /// Sets one blend tree parameter on this object's animator.
    pub fn set_animation_parameter(
        &self,
        ctx: &mut Context<'_>,
        name: &str,
        value: f32,
    ) -> Result<(), AnimationError> {
        let value = finite(value)?;
        let callback = ctx
            .host
            .animation_set_parameter
            .ok_or(AnimationError::Unavailable)?;
        // SAFETY: the host callback and the parameter name stay valid during this callback.
        animation_status(unsafe {
            callback(
                ctx.host.ctx,
                self.handle,
                abi::RveStr::from_str_ref(name),
                value,
            )
        })
    }

    /// Sets the speed of this object's animator; zero freezes all motions.
    pub fn set_animation_speed(
        &self,
        ctx: &mut Context<'_>,
        speed: f32,
    ) -> Result<(), AnimationError> {
        let speed = non_negative_finite(speed)?;
        let callback = ctx
            .host
            .animation_set_speed
            .ok_or(AnimationError::Unavailable)?;
        // SAFETY: the host callback stays valid during this callback.
        animation_status(unsafe { callback(ctx.host.ctx, self.handle, speed) })
    }

    /// Stops every motion of this object's animator at once.
    pub fn stop_animation(&self, ctx: &mut Context<'_>) -> Result<(), AnimationError> {
        let callback = ctx.host.animation_stop.ok_or(AnimationError::Unavailable)?;
        // SAFETY: the host callback stays valid during this callback.
        animation_status(unsafe { callback(ctx.host.ctx, self.handle) })
    }
}

/// Copyable handle to a scene object. It remains stable while its scene is loaded and may be
/// stored in a script struct. Every operation on it requires the current callback's `Context`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object {
    handle: u64,
}

impl Object {
    /// Queues this object's subtree for removal, including a pending object from [`Context::spawn`].
    ///
    /// A zero delay removes it at the end of the current phase. A positive delay is measured on
    /// the script clock; when several destroys target the same object, the earliest due time wins.
    /// Returns `false` for a dead object, a refused request, a negative or non-finite delay, or a
    /// host without ABI v9.
    pub fn destroy(&self, ctx: &mut Context<'_>, delay: f32) -> bool {
        if !delay.is_finite() || delay < 0.0 {
            return false;
        }
        let Some(callback) = ctx.host.destroy else {
            return false;
        };
        // SAFETY: the host callback is valid during this guest call.
        unsafe { callback(ctx.host.ctx, self.handle, delay) }
    }

    /// Returns `true` while the object is in the world.
    pub fn is_alive(&self, ctx: &Context<'_>) -> bool {
        self.state_code(ctx) == Some(abi::RVE_OBJECT_ALIVE)
    }

    /// Returns `true` from [`Context::spawn`] until the phase that applies the spawn ends.
    pub fn is_pending(&self, ctx: &Context<'_>) -> bool {
        self.state_code(ctx) == Some(abi::RVE_OBJECT_PENDING)
    }

    /// Returns the host's state code, or `None` when the host predates ABI v9.
    fn state_code(&self, ctx: &Context<'_>) -> Option<u32> {
        let callback = ctx.host.object_state?;
        // SAFETY: the host callback is valid during this guest call.
        Some(unsafe { callback(ctx.host.ctx, self.handle) })
    }
}

impl<'a> Context<'a> {
    fn new(host: &'a RveHost) -> Result<Self, String> {
        let mut transform = RveTransform::default();
        // SAFETY: the host object is valid for this callback and owns this output slot.
        let ok = unsafe { (host.get_local)(host.ctx, host.self_object, &mut transform) };
        if !ok {
            return Err("could not read the script object's local transform".to_owned());
        }
        let initial_transform = transform.into();
        Ok(Self {
            host,
            initial_transform,
            transform: initial_transform,
            console: Console { host },
            _not_send_or_sync: PhantomData,
        })
    }

    fn commit(self) -> Result<(), String> {
        if self.transform == self.initial_transform {
            return Ok(());
        }
        let transform: RveTransform = self.transform.into();
        // SAFETY: the host pointer and transform are valid for the duration of this callback.
        let ok = unsafe { (self.host.set_local)(self.host.ctx, self.host.self_object, &transform) };
        if ok {
            Ok(())
        } else {
            Err("could not write the script object's local transform".to_owned())
        }
    }

    /// Returns the host's time snapshot for this lifecycle call.
    pub fn time(&self) -> RveTime {
        self.host.time
    }

    /// Returns the input snapshot available during this callback.
    pub fn input(&self) -> Input<'_> {
        Input { host: self.host }
    }

    /// Network state and messaging. Methods return [`NetError::Unavailable`] without host callbacks.
    pub fn net(&self) -> Net<'_> {
        Net { host: self.host }
    }

    /// Game UI: canvases of preloaded `.rveui` documents and their elements.
    pub fn ui(&self) -> Ui<'_> {
        Ui { host: self.host }
    }

    /// Returns physics queries available during this callback.
    pub fn physics(&self) -> Physics<'_> {
        Physics { host: self.host }
    }

    /// Returns this script object's stable runtime handle.
    pub fn object_id(&self) -> Option<String> {
        self.object_string(self.host.object_id)
    }

    /// Returns this script object's display name.
    pub fn object_name(&self) -> Option<String> {
        self.object_string(self.host.object_name)
    }

    /// Finds a scene object by its stable scene id.
    pub fn find(&self, id: &str) -> Option<Object> {
        // SAFETY: the host pointer and borrowed id are valid for this callback.
        let handle = unsafe { (self.host.find_object)(self.host.ctx, RveStr::from_str_ref(id)) };
        (handle != 0).then_some(Object { handle })
    }

    /// Returns the currently active scene camera, or `None` if this host lacks ABI v2 camera
    /// callbacks.
    pub fn active_camera(&self) -> Option<Object> {
        let callback = self.host.active_camera?;
        // SAFETY: host callback pointers and context are valid during this guest callback.
        let handle = unsafe { callback(self.host.ctx) };
        (handle != 0).then_some(Object { handle })
    }

    /// Stages a message for the object with `target_id`. Returns false for an unknown target or
    /// when the current phase does not allow mutations.
    pub fn send(&mut self, target_id: &str, name: &str, value: &Value) -> bool {
        let value = BorrowedAbiValue::new(value);
        // SAFETY: strings and the recursive ABI value remain alive for the duration of this call.
        unsafe {
            (self.host.send)(
                self.host.ctx,
                RveStr::from_str_ref(target_id),
                RveStr::from_str_ref(name),
                &value.value,
            )
        }
    }

    /// Stages a message for every enabled script instance.
    pub fn broadcast(&mut self, name: &str, value: &Value) -> bool {
        let value = BorrowedAbiValue::new(value);
        // SAFETY: the name and recursive ABI value remain alive for the duration of this call.
        unsafe { (self.host.broadcast)(self.host.ctx, RveStr::from_str_ref(name), &value.value) }
    }

    /// Appends an informational event to the host's script event queue.
    pub fn log_info(&self, message: impl AsRef<str>) {
        // SAFETY: the string is borrowed only for the host call.
        unsafe {
            (self.host.log)(
                self.host.ctx,
                abi::RVE_LOG_INFO,
                RveStr::from_str_ref(message.as_ref()),
            )
        }
    }

    /// Appends a warning event to the host's script event queue.
    pub fn log_warn(&self, message: impl AsRef<str>) {
        // SAFETY: the string is borrowed only for the host call.
        unsafe {
            (self.host.log)(
                self.host.ctx,
                abi::RVE_LOG_WARN,
                RveStr::from_str_ref(message.as_ref()),
            )
        }
    }

    /// Appends an error event to the host's script event queue.
    pub fn log_error(&self, message: impl AsRef<str>) {
        // SAFETY: the string is borrowed only for the host call.
        unsafe {
            (self.host.log)(
                self.host.ctx,
                abi::RVE_LOG_ERROR,
                RveStr::from_str_ref(message.as_ref()),
            )
        }
    }

    /// Stages an input context change. Returns false when unavailable or invalid.
    pub fn set_input_context(&mut self, name: &str) -> bool {
        let Some(callback) = self.host.set_input_context else {
            return false;
        };
        // SAFETY: the context name is borrowed for the duration of the callback.
        unsafe { callback(self.host.ctx, RveStr::from_str_ref(name)) }
    }

    /// Stages cursor visibility for this callback. Returns false when the host lacks ABI v8 support.
    pub fn set_cursor_visible(&mut self, visible: bool) -> bool {
        self.host
            .set_cursor_visible
            .is_some_and(|callback| unsafe { callback(self.host.ctx, visible) })
    }

    /// Stages cursor locking for this callback. Returns false when the host lacks ABI v8 support.
    pub fn set_cursor_locked(&mut self, locked: bool) -> bool {
        self.host
            .set_cursor_locked
            .is_some_and(|callback| unsafe { callback(self.host.ctx, locked) })
    }

    /// Replaces one action binding slot. Returns false when unavailable or invalid.
    pub fn rebind(
        &mut self,
        context: &str,
        action: &str,
        slot: &str,
        inputs: &[PhysicalInput],
    ) -> bool {
        let Some(callback) = self.host.rebind_action else {
            return false;
        };
        let names: Vec<_> = inputs.iter().map(|input| input.name()).collect();
        let abi_names: Vec<_> = names
            .iter()
            .map(|name| RveStr::from_str_ref(name))
            .collect();
        // SAFETY: all strings and the array remain alive for the callback.
        unsafe {
            callback(
                self.host.ctx,
                RveStr::from_str_ref(context),
                RveStr::from_str_ref(action),
                RveStr::from_str_ref(slot),
                abi_names.as_ptr(),
                abi_names.len(),
            )
        }
    }

    /// Stages restoration of all input bindings to their project defaults.
    pub fn reset_bindings(&mut self) -> bool {
        self.host
            .reset_bindings
            .is_some_and(|callback| unsafe { callback(self.host.ctx) })
    }

    fn object_string(&self, callback: abi::RveObjectStringFn) -> Option<String> {
        let mut value = RveStr::empty();
        // SAFETY: the host pointer and output slot are valid during this callback.
        if !unsafe { callback(self.host.ctx, self.host.self_object, &mut value) } {
            return None;
        }
        // SAFETY: the host keeps returned strings alive through the current guest callback.
        unsafe { value.to_str().ok().map(str::to_owned) }
    }
}

impl Console<'_> {
    pub fn write(&self, message: impl AsRef<str>, level: ConsoleLevel) {
        let level = match level {
            ConsoleLevel::Log => abi::RVE_LOG_INFO,
            ConsoleLevel::Warning => abi::RVE_LOG_WARN,
            ConsoleLevel::Error => abi::RVE_LOG_ERROR,
        };
        // SAFETY: the string is borrowed only for the host call.
        unsafe { (self.host.log)(self.host.ctx, level, RveStr::from_str_ref(message.as_ref())) }
    }

    pub fn register(&self, name: &str, description: Option<&str>) -> Result<(), String> {
        let callback = self
            .host
            .register_command
            .ok_or_else(|| "console commands are unavailable in this script ABI".to_owned())?;
        // SAFETY: both strings are borrowed only during the host callback.
        let status = unsafe {
            callback(
                self.host.ctx,
                RveStr::from_str_ref(name),
                description.map_or_else(RveStr::empty, RveStr::from_str_ref),
            )
        };
        if status == abi::RveStatus::Ok as u32 {
            Ok(())
        } else {
            Err(format!("host rejected console command {name:?} ({status})"))
        }
    }

    pub fn unregister(&self, name: &str) -> Result<(), String> {
        let callback = self
            .host
            .unregister_command
            .ok_or_else(|| "console commands are unavailable in this script ABI".to_owned())?;
        // SAFETY: the name is borrowed only during the host callback.
        let status = unsafe { callback(self.host.ctx, RveStr::from_str_ref(name)) };
        if status == abi::RveStatus::Ok as u32 {
            Ok(())
        } else {
            Err(format!(
                "host rejected console command removal {name:?} ({status})"
            ))
        }
    }
}

impl Input<'_> {
    /// Returns whether a button action is currently down.
    pub fn action_down(&self, name: &str) -> bool {
        self.action_state(name) & abi::RVE_INPUT_DOWN != 0
    }

    /// Returns whether a button action became down this frame.
    pub fn action_pressed(&self, name: &str) -> bool {
        self.action_state(name) & abi::RVE_INPUT_PRESSED != 0
    }

    /// Returns whether a button action became released this frame.
    pub fn action_released(&self, name: &str) -> bool {
        self.action_state(name) & abi::RVE_INPUT_RELEASED != 0
    }

    /// Reads an axis action, or returns `None` when unavailable, unknown, or the wrong kind.
    pub fn axis(&self, name: &str) -> Option<f32> {
        self.action_value(name, abi::RVE_ACTION_AXIS)
            .map(|value| value[0])
    }

    /// Reads a vector2 action, or returns `None` when unavailable, unknown, or the wrong kind.
    pub fn vector(&self, name: &str) -> Option<[f32; 2]> {
        self.action_value(name, abi::RVE_ACTION_VECTOR2)
    }

    /// Returns the active input context when this host exposes ABI v5 input actions.
    pub fn context(&self) -> Option<String> {
        let callback = self.host.input_context?;
        let mut value = RveStr::empty();
        // SAFETY: output storage is local and returned strings are callback-scoped.
        if !unsafe { callback(self.host.ctx, &mut value) } {
            return None;
        }
        // SAFETY: ABI v5 host keeps this borrowed string alive through the callback.
        unsafe { value.to_str().ok().map(str::to_owned) }
    }

    /// Returns a slot's bindings, including `Some(Vec::new())` for an empty valid slot.
    pub fn bindings(&self, context: &str, action: &str, slot: &str) -> Option<Vec<String>> {
        let callback = self.host.action_binding?;
        let mut values = Vec::new();
        for index in 0..=abi::RVE_INPUT_MAX_BINDINGS {
            let mut value = RveStr::empty();
            // SAFETY: names and output storage are valid during the host callback.
            if !unsafe {
                callback(
                    self.host.ctx,
                    RveStr::from_str_ref(context),
                    RveStr::from_str_ref(action),
                    RveStr::from_str_ref(slot),
                    index,
                    &mut value,
                )
            } {
                return if index == 0 { None } else { Some(values) };
            }
            // SAFETY: ABI v5 host returns callback-scoped UTF-8 strings.
            let name = unsafe { value.to_str().ok()? };
            if index == 0 && name.is_empty() {
                return Some(values);
            }
            if index == abi::RVE_INPUT_MAX_BINDINGS {
                return None;
            }
            values.push(name.to_owned());
        }
        unreachable!()
    }

    /// Returns physical inputs whose raw press edge occurred this frame.
    pub fn pressed_inputs(&self) -> Vec<String> {
        let mut pressed = Vec::new();
        for code in 0..65 {
            let Some(key) = Key::from_code(code) else {
                continue;
            };
            if self.key_state(key) & abi::RVE_INPUT_PRESSED != 0 {
                pressed.push(format!("{key:?}"));
            }
        }
        for code in 0..5 {
            let Some(button) = MouseButton::from_code(code) else {
                continue;
            };
            if self.mouse_button_state(button) & abi::RVE_INPUT_PRESSED != 0 {
                pressed.push(format!("Mouse.{button:?}"));
            }
        }
        for pad in 1..=abi::RVE_GAMEPAD_SLOTS as u8 {
            let state = self.gamepad(pad);
            for button in GamepadButton::ALL {
                if state.pressed(button) {
                    pressed.push(format!("Gamepad{pad}.{}", button.name()));
                }
            }
        }
        pressed
    }

    /// Returns the gamepad snapshot for slot `pad` (1..=4), or the aggregate of all slots for 0.
    /// Unknown slots and hosts without ABI v11 gamepad callbacks return a disconnected state.
    pub fn gamepad(&self, pad: u8) -> GamepadState {
        let Some(callback) = self.host.gamepad_state else {
            return GamepadState::default();
        };
        let mut state = abi::RveGamepadState::default();
        // SAFETY: the host pointer and output slot remain valid during this script callback.
        if unsafe { callback(self.host.ctx, u32::from(pad), &mut state) } {
            GamepadState { state }
        } else {
            GamepadState::default()
        }
    }

    /// Queues a rumble request for slot `pad` (0 for every connected gamepad). Returns `false`
    /// when the host does not support rumble, the slot is not 0..=4, rumble is unavailable during
    /// script creation, or the frame already holds the maximum number of requests. A refusal is
    /// also logged as a script error, as for audio requests, even if the guest ignores the result.
    pub fn rumble(&self, pad: u8, strong: f32, weak: f32, seconds: f32) -> bool {
        let Some(callback) = self.host.gamepad_rumble else {
            return false;
        };
        // SAFETY: the host pointer remains valid during this script callback.
        unsafe { callback(self.host.ctx, u32::from(pad), strong, weak, seconds) }
    }

    /// Returns whether a key is currently held.
    pub fn key_down(&self, key: Key) -> bool {
        self.key_state(key) & abi::RVE_INPUT_DOWN != 0
    }

    /// Returns whether a key became held in this frame.
    pub fn key_pressed(&self, key: Key) -> bool {
        self.key_state(key) & abi::RVE_INPUT_PRESSED != 0
    }

    /// Returns whether a key became released in this frame.
    pub fn key_released(&self, key: Key) -> bool {
        self.key_state(key) & abi::RVE_INPUT_RELEASED != 0
    }

    /// Returns whether a mouse button is currently held.
    pub fn mouse_down(&self, button: MouseButton) -> bool {
        self.mouse_button_state(button) & abi::RVE_INPUT_DOWN != 0
    }

    /// Returns whether a mouse button became held in this frame.
    pub fn mouse_pressed(&self, button: MouseButton) -> bool {
        self.mouse_button_state(button) & abi::RVE_INPUT_PRESSED != 0
    }

    /// Returns whether a mouse button became released in this frame.
    pub fn mouse_released(&self, button: MouseButton) -> bool {
        self.mouse_button_state(button) & abi::RVE_INPUT_RELEASED != 0
    }

    /// Returns whether the cursor is visible, or `None` when the host predates ABI v8.
    pub fn cursor_visible(&self) -> Option<bool> {
        self.cursor_state().map(|state| state.visible)
    }

    /// Returns whether the cursor is locked, or `None` when the host predates ABI v8.
    pub fn cursor_locked(&self) -> Option<bool> {
        self.cursor_state().map(|state| state.locked)
    }

    fn cursor_state(&self) -> Option<abi::RveCursorState> {
        let callback = self.host.cursor_state?;
        let mut state = abi::RveCursorState {
            visible: true,
            locked: false,
        };
        // SAFETY: the host pointer and output slot remain valid during this script callback.
        unsafe { callback(self.host.ctx, &mut state) }.then_some(state)
    }

    /// Returns the cursor position in physical window pixels.
    pub fn cursor(&self) -> [f32; 2] {
        self.input_vector(self.host.cursor_position)
    }

    /// Returns accumulated raw mouse motion since the preceding script update.
    pub fn mouse_delta(&self) -> [f32; 2] {
        self.input_vector(self.host.mouse_delta)
    }

    /// Returns accumulated wheel motion in lines since the preceding script update.
    pub fn wheel(&self) -> [f32; 2] {
        self.input_vector(self.host.wheel_delta)
    }

    fn key_state(&self, key: Key) -> u32 {
        // SAFETY: the host pointer remains valid during this script callback.
        unsafe { (self.host.key_state)(self.host.ctx, key as u32) }
    }

    fn action_state(&self, name: &str) -> u32 {
        let Some(callback) = self.host.action_state else {
            return 0;
        };
        // SAFETY: the name is borrowed only for the host callback.
        unsafe {
            callback(
                self.host.ctx,
                RveStr::from_str_ref(name),
                abi::RVE_ACTION_BUTTON,
            )
        }
    }

    fn action_value(&self, name: &str, kind: u32) -> Option<[f32; 2]> {
        let callback = self.host.action_value?;
        let mut value = [0.0; 2];
        // SAFETY: name and output storage remain valid for the host callback.
        unsafe {
            callback(self.host.ctx, RveStr::from_str_ref(name), kind, &mut value).then_some(value)
        }
    }

    fn mouse_button_state(&self, button: MouseButton) -> u32 {
        // SAFETY: the host pointer remains valid during this script callback.
        unsafe { (self.host.mouse_button_state)(self.host.ctx, button as u32) }
    }

    fn input_vector(&self, callback: abi::RveInputVectorFn) -> [f32; 2] {
        let mut value = [0.0; 2];
        // SAFETY: the host pointer and output slot remain valid during this script callback.
        let _ = unsafe { callback(self.host.ctx, &mut value) };
        value
    }
}

impl Object {
    /// Returns this object's local transform, including earlier writes in the current callback.
    pub fn transform(&self, ctx: &mut Context<'_>) -> Option<Transform> {
        let mut transform = RveTransform::default();
        // SAFETY: the host pointer and output slot are valid during this guest callback.
        if !unsafe { (ctx.host.get_local)(ctx.host.ctx, self.handle, &mut transform) } {
            return None;
        }
        Some(transform.into())
    }

    /// Stages this object's local transform.
    pub fn set_transform(&self, ctx: &mut Context<'_>, transform: Transform) -> bool {
        let transform: RveTransform = transform.into();
        // SAFETY: the host pointer and transform are valid during this guest callback.
        unsafe { (ctx.host.set_local)(ctx.host.ctx, self.handle, &transform) }
    }

    /// Returns this object's camera settings, or `None` if it is not a camera or the host does
    /// not provide ABI v2 camera callbacks.
    pub fn camera_settings(&self, ctx: &Context<'_>) -> Option<CameraSettings> {
        let callback = ctx.host.get_camera_settings?;
        let mut settings = RveCameraSettings::default();
        // SAFETY: host callback and output storage are valid during this guest callback.
        if !unsafe { callback(ctx.host.ctx, self.handle, &mut settings) } {
            return None;
        }
        settings.try_into().ok()
    }

    /// Stages this object's camera settings. Returns false when the host lacks ABI v2 camera
    /// callbacks, the object is not a camera, or the settings are invalid or read-only.
    pub fn set_camera_settings(&self, ctx: &mut Context<'_>, settings: CameraSettings) -> bool {
        let Some(callback) = ctx.host.set_camera_settings else {
            return false;
        };
        let settings: RveCameraSettings = settings.into();
        // SAFETY: host callback and settings are valid during this guest callback.
        unsafe { callback(ctx.host.ctx, self.handle, &settings) }
    }

    /// Returns this camera's post-process settings in the flat ABI v12 layout. `None` when the
    /// object is not a camera or the host does not provide ABI v12 post-process callbacks.
    pub fn post_process(&self, ctx: &Context<'_>) -> Option<RvePostProcessSettings> {
        let callback = ctx.host.get_post_process?;
        let mut settings = RvePostProcessSettings::default();
        // SAFETY: host callback and output storage are valid during this guest callback.
        if unsafe { callback(ctx.host.ctx, self.handle, &mut settings) } {
            Some(settings)
        } else {
            None
        }
    }

    /// Stages this camera's post-process settings. Returns false when the host lacks ABI v12
    /// callbacks, the object is not a camera, or the settings are out of range.
    pub fn set_post_process(
        &self,
        ctx: &mut Context<'_>,
        settings: &RvePostProcessSettings,
    ) -> bool {
        let Some(callback) = ctx.host.set_post_process else {
            return false;
        };
        // SAFETY: host callback and settings are valid during this guest callback.
        unsafe { callback(ctx.host.ctx, self.handle, settings) }
    }

    /// Number of screen shader slots of this camera; `None` when the object is not a camera or the
    /// host does not provide ABI v14 callbacks.
    pub fn screen_shader_count(&self, ctx: &Context<'_>) -> Option<u32> {
        let callback = ctx.host.screen_shader_count?;
        let mut count = 0;
        // SAFETY: host callback and output storage are valid during this guest callback.
        if unsafe { callback(ctx.host.ctx, self.handle, &mut count) } {
            Some(count)
        } else {
            None
        }
    }

    /// Returns one screen shader slot (zero-based). `None` when the slot does not exist.
    pub fn screen_shader(&self, ctx: &Context<'_>, slot: u32) -> Option<RveScreenShaderState> {
        let callback = ctx.host.get_screen_shader?;
        let mut state = RveScreenShaderState::default();
        // SAFETY: host callback and output storage are valid during this guest callback.
        if unsafe { callback(ctx.host.ctx, self.handle, slot, &mut state) } {
            Some(state)
        } else {
            None
        }
    }

    /// Stages enabled flag and parameters of one screen shader slot (zero-based). Returns false when
    /// the host lacks ABI v14 callbacks, the object is not a camera or the slot does not exist.
    pub fn set_screen_shader(
        &self,
        ctx: &mut Context<'_>,
        slot: u32,
        state: &RveScreenShaderState,
    ) -> bool {
        let Some(callback) = ctx.host.set_screen_shader else {
            return false;
        };
        // SAFETY: host callback and state are valid during this guest callback.
        unsafe { callback(ctx.host.ctx, self.handle, slot, state) }
    }

    /// Stages this object as the active scene camera.
    pub fn activate_camera(&self, ctx: &mut Context<'_>) -> bool {
        let Some(callback) = ctx.host.set_active_camera else {
            return false;
        };
        // SAFETY: host callback is valid during this guest callback.
        unsafe { callback(ctx.host.ctx, self.handle) }
    }

    /// Returns this object's column-major world matrix snapshot.
    pub fn world_matrix(&self, ctx: &mut Context<'_>) -> Option<[f32; 16]> {
        let mut matrix = [0.0; 16];
        // SAFETY: the host pointer and output slot are valid during this guest callback.
        if !unsafe { (ctx.host.world_matrix)(ctx.host.ctx, self.handle, &mut matrix) } {
            return None;
        }
        Some(matrix)
    }

    /// Reads a dynamic body's latest linear velocity.
    pub fn get_velocity(&self, ctx: &Context<'_>) -> Result<[f32; 3], PhysicsError> {
        let callback = ctx.host.get_velocity.ok_or(PhysicsError::Unavailable)?;
        let mut value = [0.0; 3];
        // SAFETY: callback and output storage remain valid during this script call.
        if !unsafe { callback(ctx.host.ctx, self.handle, &mut value) } {
            return Err(PhysicsError::Rejected);
        }
        finite_vector(value)
            .then_some(value)
            .ok_or(PhysicsError::InvalidInput)
    }

    /// Stages a dynamic body's linear velocity.
    pub fn set_velocity(
        &self,
        ctx: &mut Context<'_>,
        velocity: [f32; 3],
    ) -> Result<(), PhysicsError> {
        let callback = ctx.host.set_velocity;
        self.physics_vector_command(ctx, velocity, callback)
    }

    /// Stages an impulse on a dynamic body.
    pub fn add_impulse(
        &self,
        ctx: &mut Context<'_>,
        impulse: [f32; 3],
    ) -> Result<(), PhysicsError> {
        let callback = ctx.host.add_impulse;
        self.physics_vector_command(ctx, impulse, callback)
    }

    /// Stages a force on a dynamic body for the next physics substeps.
    pub fn add_force(&self, ctx: &mut Context<'_>, force: [f32; 3]) -> Result<(), PhysicsError> {
        let callback = ctx.host.add_force;
        self.physics_vector_command(ctx, force, callback)
    }

    fn physics_vector_command(
        &self,
        ctx: &Context<'_>,
        value: [f32; 3],
        callback: Option<abi::RvePhysicsSetVectorFn>,
    ) -> Result<(), PhysicsError> {
        if !finite_vector(value) {
            return Err(PhysicsError::InvalidInput);
        }
        let callback = callback.ok_or(PhysicsError::Unavailable)?;
        // SAFETY: callback and vector storage remain valid during this script call.
        if unsafe { callback(ctx.host.ctx, self.handle, &value) } {
            Ok(())
        } else {
            Err(PhysicsError::Rejected)
        }
    }

    /// Moves a character-controller capsule and returns its blocked translation and ground state.
    pub fn move_character(
        &self,
        ctx: &mut Context<'_>,
        displacement: [f32; 3],
    ) -> Result<CharacterMove, PhysicsError> {
        if !finite_vector(displacement) {
            return Err(PhysicsError::InvalidInput);
        }
        let callback = ctx.host.move_character.ok_or(PhysicsError::Unavailable)?;
        let mut result = RveCharacterMoveResult::default();
        // SAFETY: callback and input/output storage remain valid during this script call.
        if !unsafe { callback(ctx.host.ctx, self.handle, &displacement, &mut result) } {
            return Err(PhysicsError::Rejected);
        }
        if !finite_vector(result.translation) {
            return Err(PhysicsError::InvalidInput);
        }
        Ok(result.into())
    }

    /// Reads whether this object's character controller is grounded.
    pub fn is_grounded(&self, ctx: &Context<'_>) -> Result<bool, PhysicsError> {
        let callback = ctx.host.is_grounded.ok_or(PhysicsError::Unavailable)?;
        let mut grounded = false;
        // SAFETY: callback and output storage remain valid during this script call.
        if unsafe { callback(ctx.host.ctx, self.handle, &mut grounded) } {
            Ok(grounded)
        } else {
            Err(PhysicsError::Rejected)
        }
    }

    /// Stages whether this object's collider participates in queries and contacts.
    pub fn set_collider_enabled(
        &self,
        ctx: &mut Context<'_>,
        enabled: bool,
    ) -> Result<(), PhysicsError> {
        let callback = ctx
            .host
            .set_collider_enabled
            .ok_or(PhysicsError::Unavailable)?;
        // SAFETY: callback is valid during this script call.
        if unsafe { callback(ctx.host.ctx, self.handle, enabled) } {
            Ok(())
        } else {
            Err(PhysicsError::Rejected)
        }
    }

    /// Reads the collider's latest enabled state, including writes earlier in this callback.
    pub fn is_collider_enabled(&self, ctx: &Context<'_>) -> Result<bool, PhysicsError> {
        let callback = ctx
            .host
            .is_collider_enabled
            .ok_or(PhysicsError::Unavailable)?;
        let mut enabled = false;
        // SAFETY: callback and output storage remain valid during this script call.
        if unsafe { callback(ctx.host.ctx, self.handle, &mut enabled) } {
            Ok(enabled)
        } else {
            Err(PhysicsError::Rejected)
        }
    }

    /// Returns this object's stable scene id.
    pub fn id(&self, ctx: &mut Context<'_>) -> Option<String> {
        Self::object_string(ctx, self.handle, ctx.host.object_id)
    }

    /// Returns this object's display name.
    pub fn name(&self, ctx: &mut Context<'_>) -> Option<String> {
        Self::object_string(ctx, self.handle, ctx.host.object_name)
    }

    fn object_string(
        ctx: &Context<'_>,
        handle: u64,
        callback: abi::RveObjectStringFn,
    ) -> Option<String> {
        let mut value = RveStr::empty();
        // SAFETY: the host pointer and output slot are valid during this guest callback.
        if !unsafe { callback(ctx.host.ctx, handle, &mut value) } {
            return None;
        }
        // SAFETY: the host keeps returned strings alive through the current guest callback.
        unsafe { value.to_str().ok().map(str::to_owned) }
    }
}

impl Physics<'_> {
    /// Casts a ray without including triggers or ignoring an object.
    pub fn raycast(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
    ) -> Result<Option<PhysicsRaycastHit>, PhysicsError> {
        self.raycast_with_options(origin, direction, max_distance, false, None)
    }

    /// Casts a ray with trigger and self-exclusion options.
    pub fn raycast_with_options(
        &self,
        origin: [f32; 3],
        direction: [f32; 3],
        max_distance: f32,
        include_triggers: bool,
        ignore: Option<Object>,
    ) -> Result<Option<PhysicsRaycastHit>, PhysicsError> {
        if !finite_vector(origin)
            || !finite_vector(direction)
            || direction.iter().all(|component| *component == 0.0)
            || !max_distance.is_finite()
            || max_distance < 0.0
        {
            return Err(PhysicsError::InvalidInput);
        }
        let callback = self.host.raycast.ok_or(PhysicsError::Unavailable)?;
        let mut result = RveRaycastHit::default();
        // SAFETY: callback and all vector/output storage remain valid through this script call.
        if !unsafe {
            callback(
                self.host.ctx,
                &origin,
                &direction,
                max_distance,
                include_triggers,
                ignore.map_or(0, |object| object.handle),
                &mut result,
            )
        } {
            return Err(PhysicsError::Rejected);
        }
        if result.object == 0 {
            return Ok(None);
        }
        if !finite_vector(result.point)
            || !finite_vector(result.normal)
            || !result.distance.is_finite()
        {
            return Err(PhysicsError::InvalidInput);
        }
        Ok(Some(PhysicsRaycastHit {
            object: Object {
                handle: result.object,
            },
            point: result.point,
            normal: result.normal,
            distance: result.distance,
        }))
    }
}

impl From<RveCharacterMoveResult> for CharacterMove {
    fn from(value: RveCharacterMoveResult) -> Self {
        Self {
            translation: value.translation,
            grounded: value.grounded,
            collided: value.collided,
        }
    }
}

fn finite_vector(value: [f32; 3]) -> bool {
    value.into_iter().all(f32::is_finite)
}

struct BorrowedAbiValue<'a> {
    value: RveValue,
    _children: Vec<BorrowedAbiValue<'a>>,
    _array: Vec<RveValue>,
    _lifetime: PhantomData<&'a Value>,
}

impl<'a> BorrowedAbiValue<'a> {
    fn new(value: &'a Value) -> Self {
        match value {
            Value::Bool(value) => Self::leaf(RveValue {
                tag: RVE_VALUE_BOOL,
                data: RveValueData { boolean: *value },
            }),
            Value::Int(value) => Self::leaf(RveValue {
                tag: RVE_VALUE_INT,
                data: RveValueData { integer: *value },
            }),
            Value::Float(value) => Self::leaf(RveValue {
                tag: RVE_VALUE_FLOAT,
                data: RveValueData { float: *value },
            }),
            Value::String(value) => Self::leaf(RveValue {
                tag: RVE_VALUE_STR,
                data: RveValueData {
                    string: RveStr::from_str_ref(value),
                },
            }),
            Value::Array(values) => {
                let children = values.iter().map(Self::new).collect::<Vec<_>>();
                let array = children.iter().map(|child| child.value).collect::<Vec<_>>();
                let value = RveValue {
                    tag: RVE_VALUE_ARRAY,
                    data: RveValueData {
                        array: RveArray {
                            ptr: array.as_ptr(),
                            len: array.len(),
                        },
                    },
                };
                Self {
                    value,
                    _children: children,
                    _array: array,
                    _lifetime: PhantomData,
                }
            }
        }
    }

    fn leaf(value: RveValue) -> Self {
        Self {
            value,
            _children: Vec::new(),
            _array: Vec::new(),
            _lifetime: PhantomData,
        }
    }
}

/// Network mode seen by a guest script.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetMode {
    /// No network: the local process is the only authority.
    Offline,
    /// Listen server: the local process is the server and also has a local player.
    Host,
    /// Dedicated server.
    Server,
    /// Client connected to a server.
    Client,
}

/// Destination of a network message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetTarget {
    /// The server.
    Server,
    /// Every connected peer.
    All,
    /// One peer by number.
    Peer(u32),
}

/// Failure of a network call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetError {
    /// The host does not provide network callbacks.
    Unavailable,
    /// The host refused the call in the current phase or state.
    Rejected,
    /// The name or value violates the message limits, or the target is not valid here.
    InvalidInput,
}

impl std::fmt::Display for NetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "network callback is unavailable in this host",
            Self::Rejected => "network operation was rejected by the host",
            Self::InvalidInput => "network input violates the message limits or target rules",
        })
    }
}

impl std::error::Error for NetError {}

fn net_status(status: u32) -> Result<(), NetError> {
    match status {
        RVE_NET_OK => Ok(()),
        RVE_NET_INVALID_INPUT => Err(NetError::InvalidInput),
        _ => Err(NetError::Rejected),
    }
}

fn net_target(target: NetTarget) -> (u32, u32) {
    match target {
        NetTarget::Server => (RVE_NET_TARGET_SERVER, 0),
        NetTarget::All => (RVE_NET_TARGET_ALL, 0),
        NetTarget::Peer(peer) => (RVE_NET_TARGET_PEER, peer),
    }
}

/// Call-scoped network access from [`Context::net`].
pub struct Net<'a> {
    host: &'a RveHost,
}

impl Net<'_> {
    /// Returns the network mode; without host callbacks it is [`NetMode::Offline`].
    pub fn mode(&self) -> Result<NetMode, NetError> {
        let callback = self.host.net_mode.ok_or(NetError::Unavailable)?;
        // SAFETY: the host context is valid for this callback.
        Ok(match unsafe { callback(self.host.ctx) } {
            RVE_NET_MODE_HOST => NetMode::Host,
            RVE_NET_MODE_SERVER => NetMode::Server,
            RVE_NET_MODE_CLIENT => NetMode::Client,
            _ => NetMode::Offline,
        })
    }

    /// True for offline, listen-server and dedicated-server modes.
    pub fn is_server(&self) -> Result<bool, NetError> {
        Ok(matches!(
            self.mode()?,
            NetMode::Offline | NetMode::Host | NetMode::Server
        ))
    }

    /// True for listen-server and client modes.
    pub fn is_client(&self) -> Result<bool, NetError> {
        Ok(matches!(self.mode()?, NetMode::Host | NetMode::Client))
    }

    /// Returns the number of this process's peer.
    pub fn local_peer(&self) -> Result<u32, NetError> {
        let callback = self.host.net_local_peer.ok_or(NetError::Unavailable)?;
        // SAFETY: the host context is valid for this callback.
        Ok(unsafe { callback(self.host.ctx) })
    }

    /// Returns the remote peers, without the local one.
    pub fn peers(&self) -> Result<Vec<u32>, NetError> {
        let callback = self.host.net_peers.ok_or(NetError::Unavailable)?;
        let mut count = 0_usize;
        // SAFETY: a zero-capacity call writes only the total count.
        if !unsafe { callback(self.host.ctx, std::ptr::null_mut(), 0, &mut count) } {
            return Err(NetError::Rejected);
        }
        let mut peers = vec![0_u32; count];
        let mut total = 0_usize;
        // SAFETY: the buffer holds `count` entries; the host writes at most that many.
        if !unsafe { callback(self.host.ctx, peers.as_mut_ptr(), peers.len(), &mut total) } {
            return Err(NetError::Rejected);
        }
        peers.truncate(total.min(count));
        Ok(peers)
    }

    /// Stages a message for `target`. `reliable` selects ordered delivery.
    pub fn send(
        &self,
        target: NetTarget,
        name: &str,
        value: &Value,
        reliable: bool,
    ) -> Result<(), NetError> {
        let callback = self.host.net_send.ok_or(NetError::Unavailable)?;
        let (kind, peer) = net_target(target);
        let value = BorrowedAbiValue::new(value);
        // SAFETY: strings and the recursive ABI value remain alive for the duration of this call.
        let status = unsafe {
            callback(
                self.host.ctx,
                kind,
                peer,
                RveStr::from_str_ref(name),
                &value.value,
                reliable,
            )
        };
        net_status(status)
    }
}

impl Object {
    /// Network number of this object, or `None` without a network identity.
    pub fn net_id(&self, ctx: &Context<'_>) -> Result<Option<u32>, NetError> {
        let callback = ctx.host.net_object_id.ok_or(NetError::Unavailable)?;
        let mut id = 0_u32;
        // SAFETY: the host owns the output slot for this call.
        Ok(unsafe { callback(ctx.host.ctx, self.handle, &mut id) }.then_some(id))
    }

    /// Owner peer of this object, or `None` when no owner is assigned.
    pub fn net_owner(&self, ctx: &Context<'_>) -> Result<Option<u32>, NetError> {
        let callback = ctx.host.net_object_owner.ok_or(NetError::Unavailable)?;
        let mut owner = 0_u32;
        // SAFETY: the host owns the output slot for this call.
        Ok(unsafe { callback(ctx.host.ctx, self.handle, &mut owner) }.then_some(owner))
    }

    /// True when the local peer owns this object.
    pub fn is_owner(&self, ctx: &Context<'_>) -> Result<bool, NetError> {
        let callback = ctx.host.net_object_is_owner.ok_or(NetError::Unavailable)?;
        let mut owner = false;
        // SAFETY: the host owns the output slot for this call.
        Ok(unsafe { callback(ctx.host.ctx, self.handle, &mut owner) } && owner)
    }

    /// True when this process may change the state of this object.
    pub fn has_authority(&self, ctx: &Context<'_>) -> Result<bool, NetError> {
        let callback = ctx
            .host
            .net_object_has_authority
            .ok_or(NetError::Unavailable)?;
        let mut authority = false;
        // SAFETY: the host owns the output slot for this call.
        Ok(unsafe { callback(ctx.host.ctx, self.handle, &mut authority) } && authority)
    }

    /// Assigns the owner peer; only the server, a listen server or offline mode may do it.
    pub fn set_net_owner(&self, ctx: &mut Context<'_>, peer: u32) -> Result<(), NetError> {
        let callback = ctx.host.net_object_set_owner.ok_or(NetError::Unavailable)?;
        // SAFETY: the host context is valid for this callback.
        net_status(unsafe { callback(ctx.host.ctx, self.handle, peer) })
    }

    /// Stages a message for the same object on `target`'s side.
    pub fn net_send(
        &self,
        ctx: &mut Context<'_>,
        name: &str,
        value: &Value,
        target: NetTarget,
        reliable: bool,
    ) -> Result<(), NetError> {
        let callback = ctx.host.net_object_send.ok_or(NetError::Unavailable)?;
        let (kind, peer) = net_target(target);
        let value = BorrowedAbiValue::new(value);
        // SAFETY: strings and the recursive ABI value remain alive for the duration of this call.
        let status = unsafe {
            callback(
                ctx.host.ctx,
                self.handle,
                kind,
                peer,
                RveStr::from_str_ref(name),
                &value.value,
                reliable,
            )
        };
        net_status(status)
    }
}

/// Failure of a UI call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiError {
    /// The host does not provide UI callbacks.
    Unavailable,
    /// The host refused the call: a stale handle, an unsupported property, or a write outside
    /// lifecycle calls.
    Rejected,
    /// The value does not fit the property, for example a non-finite number or a bad index.
    InvalidInput,
}

impl std::fmt::Display for UiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "UI callback is unavailable in this host",
            Self::Rejected => "UI operation was rejected by the host",
            Self::InvalidInput => "UI value does not fit the property",
        })
    }
}

impl std::error::Error for UiError {}

fn ui_status(status: u32) -> Result<(), UiError> {
    match status {
        RVE_UI_OK => Ok(()),
        RVE_UI_INVALID_INPUT => Err(UiError::InvalidInput),
        _ => Err(UiError::Rejected),
    }
}

/// Handle of an open canvas. A closed canvas's handle is rejected, even if its slot is reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Canvas {
    raw: u64,
}

/// Handle of one element of a canvas.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Element {
    canvas: Canvas,
    index: u32,
}

impl Element {
    /// The canvas that contains this element.
    pub fn canvas(self) -> Canvas {
        self.canvas
    }
}

/// Value of an element. A dropdown index counts from zero.
#[derive(Clone, Debug, PartialEq)]
pub enum UiValue {
    Number(f64),
    Bool(bool),
    Index(usize),
    Text(String),
    Color([u8; 4]),
}

/// Kind of a UI event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiEventKind {
    Click,
    ValueChanged,
    Submit,
    Cancel,
    Focus,
    Blur,
    HoverEnter,
    HoverExit,
}

/// UI event delivered to [`Script::on_ui_event`].
#[derive(Clone, Debug, PartialEq)]
pub struct UiEvent {
    pub kind: UiEventKind,
    pub canvas: Canvas,
    pub element: Element,
    /// Identifier of the element in its document.
    pub element_id: String,
    /// Value carried by the event, when the kind has one.
    pub value: Option<UiValue>,
}

/// Rectangle of an element in canvas units: anchors, pivot, and offsets.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiRect {
    pub anchor_min: [f32; 2],
    pub anchor_max: [f32; 2],
    pub pivot: [f32; 2],
    pub offset_min: [f32; 2],
    pub offset_max: [f32; 2],
}

impl From<RveUiRect> for UiRect {
    fn from(rect: RveUiRect) -> Self {
        Self {
            anchor_min: rect.anchor_min,
            anchor_max: rect.anchor_max,
            pivot: rect.pivot,
            offset_min: rect.offset_min,
            offset_max: rect.offset_max,
        }
    }
}

impl From<UiRect> for RveUiRect {
    fn from(rect: UiRect) -> Self {
        Self {
            anchor_min: rect.anchor_min,
            anchor_max: rect.anchor_max,
            pivot: rect.pivot,
            offset_min: rect.offset_min,
            offset_max: rect.offset_max,
        }
    }
}

fn empty_ui_value() -> RveUiValue {
    RveUiValue {
        kind: RVE_UI_VALUE_NONE,
        flag: false,
        number: 0.0,
        integer: 0,
        text: RveStr::empty(),
        color: [0; 4],
    }
}

/// Converts a host value; `None` means the host reported no value.
///
/// # Safety
/// For a text value, `value.text` must refer to readable storage for this callback.
unsafe fn ui_value_from_abi(value: RveUiValue) -> Result<Option<UiValue>, UiError> {
    Ok(match value.kind {
        RVE_UI_VALUE_NONE => None,
        RVE_UI_VALUE_NUMBER => Some(UiValue::Number(value.number)),
        RVE_UI_VALUE_BOOL => Some(UiValue::Bool(value.flag)),
        RVE_UI_VALUE_INDEX => Some(UiValue::Index(
            usize::try_from(value.integer).map_err(|_| UiError::Rejected)?,
        )),
        RVE_UI_VALUE_TEXT => {
            // SAFETY: the caller guarantees the host keeps the text readable for this callback.
            let text = unsafe { value.text.to_str() }.map_err(|_| UiError::Rejected)?;
            Some(UiValue::Text(text.to_owned()))
        }
        RVE_UI_VALUE_COLOR => Some(UiValue::Color(value.color)),
        _ => return Err(UiError::Rejected),
    })
}

/// Borrowed host value for `value`; the caller keeps `value` alive for the call.
fn ui_value_to_abi(value: &UiValue) -> Result<RveUiValue, UiError> {
    let mut abi = empty_ui_value();
    match value {
        UiValue::Number(number) => {
            abi.kind = RVE_UI_VALUE_NUMBER;
            abi.number = *number;
        }
        UiValue::Bool(flag) => {
            abi.kind = RVE_UI_VALUE_BOOL;
            abi.flag = *flag;
        }
        UiValue::Index(index) => {
            abi.kind = RVE_UI_VALUE_INDEX;
            abi.integer = i64::try_from(*index).map_err(|_| UiError::InvalidInput)?;
        }
        UiValue::Text(text) => {
            abi.kind = RVE_UI_VALUE_TEXT;
            abi.text = RveStr::from_str_ref(text);
        }
        UiValue::Color(color) => {
            abi.kind = RVE_UI_VALUE_COLOR;
            abi.color = *color;
        }
    }
    Ok(abi)
}

fn ui_event_kind(kind: u32) -> Option<UiEventKind> {
    Some(match kind {
        RVE_UI_EVENT_CLICK => UiEventKind::Click,
        RVE_UI_EVENT_VALUE_CHANGED => UiEventKind::ValueChanged,
        RVE_UI_EVENT_SUBMIT => UiEventKind::Submit,
        RVE_UI_EVENT_CANCEL => UiEventKind::Cancel,
        RVE_UI_EVENT_FOCUS => UiEventKind::Focus,
        RVE_UI_EVENT_BLUR => UiEventKind::Blur,
        RVE_UI_EVENT_HOVER_ENTER => UiEventKind::HoverEnter,
        RVE_UI_EVENT_HOVER_EXIT => UiEventKind::HoverExit,
        _ => return None,
    })
}

/// Call-scoped UI access from [`Context::ui`]. Every method returns [`UiError::Unavailable`]
/// when the host has no UI callbacks.
pub struct Ui<'a> {
    host: &'a RveHost,
}

impl Ui<'_> {
    /// Opens a preloaded `.rveui` document as a canvas owned by this object.
    pub fn open(&self, path: &str) -> Result<Canvas, UiError> {
        let callback = self.host.ui_open.ok_or(UiError::Unavailable)?;
        let mut raw = 0_u64;
        // SAFETY: the host and the output slot are valid for this callback.
        let status = unsafe { callback(self.host.ctx, RveStr::from_str_ref(path), &mut raw) };
        ui_status(status)?;
        Ok(Canvas { raw })
    }

    /// Closes a canvas at the end of this call; later calls in the same callback see it closed.
    pub fn close(&self, canvas: Canvas) -> Result<(), UiError> {
        let callback = self.host.ui_close.ok_or(UiError::Unavailable)?;
        // SAFETY: the host is valid for this callback and checks the handle itself.
        ui_status(unsafe { callback(self.host.ctx, canvas.raw) })
    }

    /// The canvas of an object's `ui_canvas` component, if it is open.
    pub fn canvas_of(&self, object: Object) -> Option<Canvas> {
        let callback = self.host.ui_object_canvas?;
        let mut raw = 0_u64;
        // SAFETY: the host and the output slot are valid for this callback.
        unsafe { callback(self.host.ctx, object.handle, &mut raw) }.then_some(Canvas { raw })
    }

    /// The element that has focus, if any.
    pub fn focused(&self) -> Option<Element> {
        let callback = self.host.ui_focused?;
        let mut canvas = 0_u64;
        let mut index = 0_u32;
        // SAFETY: the host and both output slots are valid for this callback.
        unsafe { callback(self.host.ctx, &mut canvas, &mut index) }.then_some(Element {
            canvas: Canvas { raw: canvas },
            index,
        })
    }

    /// Whether the pointer was over interactive UI on the last update.
    pub fn pointer_over(&self) -> bool {
        self.host
            .ui_pointer_over
            // SAFETY: the host is valid for this callback.
            .is_some_and(|callback| unsafe { callback(self.host.ctx) })
    }

    /// The element with this identifier, or `None` when the canvas has no such element.
    pub fn find(&self, canvas: Canvas, id: &str) -> Result<Option<Element>, UiError> {
        let callback = self.host.ui_find.ok_or(UiError::Unavailable)?;
        let mut index = 0_u32;
        // SAFETY: the host and the output slot are valid for this callback.
        let found = unsafe {
            callback(
                self.host.ctx,
                canvas.raw,
                RveStr::from_str_ref(id),
                &mut index,
            )
        };
        Ok(found.then_some(Element { canvas, index }))
    }

    fn get(&self, canvas: u64, element: u32, property: u32) -> Result<RveUiValue, UiError> {
        let callback = self.host.ui_get.ok_or(UiError::Unavailable)?;
        let mut out = empty_ui_value();
        // SAFETY: the host and the output slot are valid for this callback.
        ui_status(unsafe { callback(self.host.ctx, canvas, element, property, &mut out) })?;
        Ok(out)
    }

    fn set(
        &self,
        canvas: u64,
        element: u32,
        property: u32,
        value: &UiValue,
    ) -> Result<(), UiError> {
        let callback = self.host.ui_set.ok_or(UiError::Unavailable)?;
        let value = ui_value_to_abi(value)?;
        // SAFETY: the host reads the borrowed value only during this callback; `value` lives on.
        ui_status(unsafe { callback(self.host.ctx, canvas, element, property, &value) })
    }

    fn get_bool(&self, canvas: u64, element: u32, property: u32) -> Result<bool, UiError> {
        match self.get(canvas, element, property)? {
            RveUiValue {
                kind: RVE_UI_VALUE_BOOL,
                flag,
                ..
            } => Ok(flag),
            _ => Err(UiError::Rejected),
        }
    }

    /// Whether the canvas is visible.
    pub fn canvas_visible(&self, canvas: Canvas) -> Result<bool, UiError> {
        self.get_bool(canvas.raw, 0, RVE_UI_PROP_CANVAS_VISIBLE)
    }

    pub fn set_canvas_visible(&self, canvas: Canvas, visible: bool) -> Result<(), UiError> {
        self.set(
            canvas.raw,
            0,
            RVE_UI_PROP_CANVAS_VISIBLE,
            &UiValue::Bool(visible),
        )
    }

    /// Drawing layer of the canvas; higher layers draw above lower ones.
    pub fn canvas_layer(&self, canvas: Canvas) -> Result<i32, UiError> {
        match self.get(canvas.raw, 0, RVE_UI_PROP_CANVAS_LAYER)? {
            RveUiValue {
                kind: RVE_UI_VALUE_INTEGER,
                integer,
                ..
            } => i32::try_from(integer).map_err(|_| UiError::Rejected),
            _ => Err(UiError::Rejected),
        }
    }

    pub fn set_canvas_layer(&self, canvas: Canvas, layer: i32) -> Result<(), UiError> {
        let callback = self.host.ui_set.ok_or(UiError::Unavailable)?;
        let value = RveUiValue {
            kind: RVE_UI_VALUE_INTEGER,
            integer: i64::from(layer),
            ..empty_ui_value()
        };
        // SAFETY: the host reads the value only during this callback.
        ui_status(unsafe {
            callback(
                self.host.ctx,
                canvas.raw,
                0,
                RVE_UI_PROP_CANVAS_LAYER,
                &value,
            )
        })
    }

    /// Text of a `Text` or `InputField` element.
    pub fn text(&self, element: Element) -> Result<String, UiError> {
        match self.get_value_abi(element, RVE_UI_PROP_TEXT)? {
            Some(UiValue::Text(text)) => Ok(text),
            _ => Err(UiError::Rejected),
        }
    }

    pub fn set_text(&self, element: Element, text: &str) -> Result<(), UiError> {
        self.set(
            element.canvas.raw,
            element.index,
            RVE_UI_PROP_TEXT,
            &UiValue::Text(text.to_owned()),
        )
    }

    /// Value of a slider, progress bar, toggle, dropdown, or input field. `None` for a dropdown
    /// without a selection.
    pub fn value(&self, element: Element) -> Result<Option<UiValue>, UiError> {
        self.get_value_abi(element, RVE_UI_PROP_VALUE)
    }

    pub fn set_value(&self, element: Element, value: UiValue) -> Result<(), UiError> {
        self.set(element.canvas.raw, element.index, RVE_UI_PROP_VALUE, &value)
    }

    fn get_value_abi(&self, element: Element, property: u32) -> Result<Option<UiValue>, UiError> {
        let value = self.get(element.canvas.raw, element.index, property)?;
        // SAFETY: the host keeps the text of the value readable for this callback.
        unsafe { ui_value_from_abi(value) }
    }

    pub fn element_visible(&self, element: Element) -> Result<bool, UiError> {
        self.get_bool(element.canvas.raw, element.index, RVE_UI_PROP_VISIBLE)
    }

    pub fn set_element_visible(&self, element: Element, visible: bool) -> Result<(), UiError> {
        self.set(
            element.canvas.raw,
            element.index,
            RVE_UI_PROP_VISIBLE,
            &UiValue::Bool(visible),
        )
    }

    pub fn interactable(&self, element: Element) -> Result<bool, UiError> {
        self.get_bool(element.canvas.raw, element.index, RVE_UI_PROP_INTERACTABLE)
    }

    pub fn set_interactable(&self, element: Element, interactable: bool) -> Result<(), UiError> {
        self.set(
            element.canvas.raw,
            element.index,
            RVE_UI_PROP_INTERACTABLE,
            &UiValue::Bool(interactable),
        )
    }

    /// Tint color of the element as sRGB bytes.
    pub fn color(&self, element: Element) -> Result<[u8; 4], UiError> {
        match self.get(element.canvas.raw, element.index, RVE_UI_PROP_COLOR)? {
            RveUiValue {
                kind: RVE_UI_VALUE_COLOR,
                color,
                ..
            } => Ok(color),
            _ => Err(UiError::Rejected),
        }
    }

    pub fn set_color(&self, element: Element, color: [u8; 4]) -> Result<(), UiError> {
        self.set(
            element.canvas.raw,
            element.index,
            RVE_UI_PROP_COLOR,
            &UiValue::Color(color),
        )
    }

    /// Replaces the image of an `Image` or `Button` element with a declared `.png` path.
    pub fn set_image(&self, element: Element, path: &str) -> Result<(), UiError> {
        let callback = self.host.ui_set_image.ok_or(UiError::Unavailable)?;
        // SAFETY: the host copies the borrowed path during this callback.
        ui_status(unsafe {
            callback(
                self.host.ctx,
                element.canvas.raw,
                element.index,
                RveStr::from_str_ref(path),
            )
        })
    }

    pub fn rect(&self, element: Element) -> Result<UiRect, UiError> {
        let callback = self.host.ui_get_rect.ok_or(UiError::Unavailable)?;
        let mut out = RveUiRect {
            anchor_min: [0.0; 2],
            anchor_max: [0.0; 2],
            pivot: [0.0; 2],
            offset_min: [0.0; 2],
            offset_max: [0.0; 2],
        };
        // SAFETY: the host and the output slot are valid for this callback.
        ui_status(unsafe { callback(self.host.ctx, element.canvas.raw, element.index, &mut out) })?;
        Ok(out.into())
    }

    pub fn set_rect(&self, element: Element, rect: UiRect) -> Result<(), UiError> {
        let callback = self.host.ui_set_rect.ok_or(UiError::Unavailable)?;
        let rect = RveUiRect::from(rect);
        // SAFETY: the host reads the rectangle only during this callback.
        ui_status(unsafe { callback(self.host.ctx, element.canvas.raw, element.index, &rect) })
    }

    /// Replaces the options of a dropdown; the selection is kept only while its index remains.
    pub fn set_options(&self, element: Element, options: &[&str]) -> Result<(), UiError> {
        let callback = self.host.ui_set_options.ok_or(UiError::Unavailable)?;
        let borrowed: Vec<RveStr> = options
            .iter()
            .map(|option| RveStr::from_str_ref(option))
            .collect();
        // SAFETY: `borrowed` and the strings it points to outlive this callback.
        ui_status(unsafe {
            callback(
                self.host.ctx,
                element.canvas.raw,
                element.index,
                borrowed.as_ptr(),
                borrowed.len(),
            )
        })
    }

    /// Moves focus to a focusable element.
    pub fn focus(&self, element: Element) -> Result<(), UiError> {
        let callback = self.host.ui_focus.ok_or(UiError::Unavailable)?;
        // SAFETY: the host is valid for this callback and checks the handle itself.
        ui_status(unsafe { callback(self.host.ctx, element.canvas.raw, element.index) })
    }
}

#[doc(hidden)]
pub mod __private {
    use super::{
        Canvas, CollisionContact, Context, Element, Object, Params, Script, UiEvent, ValueError,
        ui_event_kind, ui_value_from_abi,
    };
    use crate::abi::{
        RVE_COLLISION_ENTER, RVE_COLLISION_EXIT, RVE_NET_EVENT_MESSAGE,
        RVE_NET_EVENT_PEER_CONNECTED, RVE_NET_EVENT_PEER_DISCONNECTED, RVE_PARAM_HAS_MAX,
        RVE_PARAM_HAS_MIN, RVE_TRIGGER_ENTER, RVE_TRIGGER_EXIT, RveCollisionEvent, RveCollisionFn,
        RveCommandFn, RveCreateFn, RveDropFn, RveHost, RveMessageFn, RveNetEvent, RveNetEventFn,
        RveParamDesc, RveParams, RveStartFn, RveStatus, RveStr, RveUiEvent, RveUiEventFn,
        RveUpdateFn, RveValue,
    };
    use std::any::Any;
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    struct GuestInstance(Box<dyn Script>);

    // Mirrors the fields of `RveScriptDesc`, which the macro fills in one call.
    #[allow(clippy::too_many_arguments)]
    pub const fn descriptor(
        name: &'static str,
        create: RveCreateFn,
        start: RveStartFn,
        update: RveUpdateFn,
        message: RveMessageFn,
        destroy: RveStartFn,
        drop: RveDropFn,
        command: RveCommandFn,
        collision: RveCollisionFn,
        net_event: RveNetEventFn,
        ui_event: RveUiEventFn,
        params: &'static [RveParamDesc],
    ) -> crate::abi::RveScriptDesc {
        crate::abi::RveScriptDesc {
            name: RveStr::from_static(name),
            create,
            start,
            update,
            message,
            destroy,
            drop,
            params: params.as_ptr(),
            param_count: params.len(),
            command: Some(command),
            collision: Some(collision),
            net_event: Some(net_event),
            ui_event: Some(ui_event),
        }
    }

    /// One parameter descriptor for [`crate::script_module!`] tables.
    pub const fn param(
        name: &'static str,
        kind: u32,
        default: RveValue,
        min: Option<f64>,
        max: Option<f64>,
        label: Option<&'static str>,
    ) -> RveParamDesc {
        let mut flags = 0;
        let min = match min {
            Some(min) => {
                flags |= RVE_PARAM_HAS_MIN;
                min
            }
            None => 0.0,
        };
        let max = match max {
            Some(max) => {
                flags |= RVE_PARAM_HAS_MAX;
                max
            }
            None => 0.0,
        };
        RveParamDesc {
            name: RveStr::from_static(name),
            kind,
            default,
            min,
            max,
            flags,
            label: match label {
                Some(label) => RveStr::from_static(label),
                None => RveStr::empty(),
            },
        }
    }

    pub unsafe fn create(
        host: *const RveHost,
        params: *const RveParams,
        out: *mut *mut c_void,
        factory: fn(&Params) -> Result<Box<dyn Script>, String>,
    ) -> RveStatus {
        if out.is_null() {
            return report(host, "null create output pointer", RveStatus::Error);
        }
        // SAFETY: `out` is a callback output pointer provided by the ABI caller.
        unsafe { *out = std::ptr::null_mut() };
        let result = catch_unwind(AssertUnwindSafe(|| {
            if params.is_null() {
                return Err("null create parameter pointer".to_owned());
            }
            // SAFETY: params is valid for the callback per the ABI contract.
            let params = unsafe { (*params).into_owned() }.map_err(|error| error.to_string())?;
            factory(&params)
        }));
        match result {
            Ok(Ok(instance)) => {
                let instance = Box::new(GuestInstance(instance));
                // SAFETY: out was checked above and stores the unique guest allocation.
                unsafe { *out = Box::into_raw(instance).cast() };
                RveStatus::Ok
            }
            Ok(Err(message)) => report(host, &message, RveStatus::Error),
            Err(payload) => report(host, &panic_message(payload), RveStatus::Panicked),
        }
    }

    pub unsafe fn start(instance: *mut c_void, host: *const RveHost) -> RveStatus {
        unsafe { with_context(instance, host, |script, ctx| script.on_start(ctx)) }
    }

    pub unsafe fn update(instance: *mut c_void, host: *const RveHost, dt: f32) -> RveStatus {
        unsafe { with_context(instance, host, |script, ctx| script.on_update(ctx, dt)) }
    }

    pub unsafe fn message(
        instance: *mut c_void,
        host: *const RveHost,
        sender: RveStr,
        name: RveStr,
        value: *const crate::abi::RveValue,
    ) -> RveStatus {
        if instance.is_null() || host.is_null() || value.is_null() {
            return report(host, "null message callback argument", RveStatus::Error);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: pointers are valid for this ABI callback.
            let sender = unsafe { sender.to_str() }.map_err(|error| error.to_string())?;
            // SAFETY: pointers are valid for this ABI callback.
            let name = unsafe { name.to_str() }.map_err(|error| error.to_string())?;
            // SAFETY: pointers are valid for this ABI callback.
            let value =
                unsafe { (*value).into_owned() }.map_err(|error: ValueError| error.to_string())?;
            // SAFETY: instance and host are valid for this callback.
            let instance = unsafe { &mut *instance.cast::<GuestInstance>() };
            let mut context = Context::new(unsafe { &*host })?;
            instance.0.on_message(&mut context, sender, name, &value);
            context.commit()
        }));
        status_from_result(host, result)
    }

    pub unsafe fn collision(
        instance: *mut c_void,
        host: *const RveHost,
        event: RveCollisionEvent,
    ) -> RveStatus {
        if instance.is_null() || host.is_null() || event.other == 0 {
            return report(
                host,
                "null or invalid collision callback argument",
                RveStatus::Error,
            );
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: instance and host are valid for this synchronous ABI callback.
            let instance = unsafe { &mut *instance.cast::<GuestInstance>() };
            let mut context = Context::new(unsafe { &*host })?;
            let other = Object {
                handle: event.other,
            };
            match event.kind {
                RVE_COLLISION_ENTER => instance.0.on_collision_enter(
                    &mut context,
                    other,
                    CollisionContact {
                        point: event.point,
                        normal: event.normal,
                    },
                ),
                RVE_COLLISION_EXIT => instance.0.on_collision_exit(&mut context, other),
                RVE_TRIGGER_ENTER => instance.0.on_trigger_enter(&mut context, other),
                RVE_TRIGGER_EXIT => instance.0.on_trigger_exit(&mut context, other),
                _ => return Err("unknown collision event kind".to_owned()),
            }
            context.commit()
        }));
        status_from_result(host, result)
    }

    pub unsafe fn command(
        instance: *mut c_void,
        host: *const RveHost,
        name: RveStr,
        args: *const RveStr,
        arg_count: usize,
    ) -> RveStatus {
        if instance.is_null() || host.is_null() || (arg_count > 0 && args.is_null()) {
            return report(host, "null command callback argument", RveStatus::Error);
        }
        if arg_count > isize::MAX as usize / std::mem::size_of::<RveStr>() {
            return report(host, "command argument count overflow", RveStatus::Error);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: name and args are borrowed for this callback by the ABI host.
            let name = unsafe { name.to_str() }.map_err(|error| error.to_string())?;
            let args = if arg_count == 0 {
                &[]
            } else {
                // SAFETY: the ABI host provides `arg_count` valid borrowed strings.
                unsafe { std::slice::from_raw_parts(args, arg_count) }
            };
            let args = args
                .iter()
                .map(|arg| unsafe { arg.to_str() }.map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            // SAFETY: instance and host are valid for the duration of this callback.
            let instance = unsafe { &mut *instance.cast::<GuestInstance>() };
            let mut context = Context::new(unsafe { &*host })?;
            instance.0.on_command(&mut context, name, &args);
            context.commit()
        }));
        status_from_result(host, result)
    }

    pub unsafe fn net_event(
        instance: *mut c_void,
        host: *const RveHost,
        event: RveNetEvent,
    ) -> RveStatus {
        if instance.is_null() || host.is_null() {
            return report(host, "null net event callback argument", RveStatus::Error);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: instance and host are valid for this synchronous ABI callback.
            let instance = unsafe { &mut *instance.cast::<GuestInstance>() };
            let mut context = Context::new(unsafe { &*host })?;
            match event.kind {
                RVE_NET_EVENT_MESSAGE => {
                    if event.value.is_null() {
                        return Err("null net message value".to_owned());
                    }
                    // SAFETY: the host keeps the name and value readable for this callback.
                    let name = unsafe { event.name.to_str() }.map_err(|error| error.to_string())?;
                    // SAFETY: the host keeps the value readable for this callback.
                    let value = unsafe { (*event.value).into_owned() }
                        .map_err(|error: ValueError| error.to_string())?;
                    instance
                        .0
                        .on_net_message(&mut context, event.peer, name, &value);
                }
                RVE_NET_EVENT_PEER_CONNECTED => {
                    instance.0.on_peer_connected(&mut context, event.peer);
                }
                RVE_NET_EVENT_PEER_DISCONNECTED => {
                    // SAFETY: the host keeps the reason readable for this callback.
                    let reason =
                        unsafe { event.reason.to_str() }.map_err(|error| error.to_string())?;
                    instance
                        .0
                        .on_peer_disconnected(&mut context, event.peer, reason);
                }
                _ => return Err("unknown net event kind".to_owned()),
            }
            context.commit()
        }));
        status_from_result(host, result)
    }

    pub unsafe fn ui_event(
        instance: *mut c_void,
        host: *const RveHost,
        event: RveUiEvent,
    ) -> RveStatus {
        if instance.is_null() || host.is_null() {
            return report(host, "null ui event callback argument", RveStatus::Error);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: instance and host are valid for this synchronous ABI callback.
            let instance = unsafe { &mut *instance.cast::<GuestInstance>() };
            let mut context = Context::new(unsafe { &*host })?;
            let kind =
                ui_event_kind(event.kind).ok_or_else(|| "unknown ui event kind".to_owned())?;
            // SAFETY: the host keeps the identifier and value readable for this callback.
            let element_id =
                unsafe { event.element_id.to_str() }.map_err(|error| error.to_string())?;
            let value =
                unsafe { ui_value_from_abi(event.value) }.map_err(|error| error.to_string())?;
            instance.0.on_ui_event(
                &mut context,
                UiEvent {
                    kind,
                    canvas: Canvas { raw: event.canvas },
                    element: Element {
                        canvas: Canvas { raw: event.canvas },
                        index: event.element,
                    },
                    element_id: element_id.to_owned(),
                    value,
                },
            );
            context.commit()
        }));
        status_from_result(host, result)
    }

    pub unsafe fn destroy(instance: *mut c_void, host: *const RveHost) -> RveStatus {
        unsafe { with_context(instance, host, |script, ctx| script.on_destroy(ctx)) }
    }

    pub unsafe fn drop_instance(instance: *mut c_void) -> RveStatus {
        if instance.is_null() {
            return RveStatus::Error;
        }
        // SAFETY: the host calls drop exactly once for each successful create.
        match catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(instance.cast::<GuestInstance>()))
        })) {
            Ok(()) => RveStatus::Ok,
            Err(_) => RveStatus::Panicked,
        }
    }

    unsafe fn with_context(
        instance: *mut c_void,
        host: *const RveHost,
        callback: impl FnOnce(&mut dyn Script, &mut Context<'_>),
    ) -> RveStatus {
        if instance.is_null() || host.is_null() {
            return report(host, "null lifecycle callback argument", RveStatus::Error);
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: the host supplies valid call-scoped pointers.
            let instance = unsafe { &mut *instance.cast::<GuestInstance>() };
            let mut context = Context::new(unsafe { &*host })?;
            callback(instance.0.as_mut(), &mut context);
            context.commit()
        }));
        status_from_result(host, result)
    }

    fn status_from_result(
        host: *const RveHost,
        result: Result<Result<(), String>, Box<dyn Any + Send>>,
    ) -> RveStatus {
        match result {
            Ok(Ok(())) => RveStatus::Ok,
            Ok(Err(message)) => report(host, &message, RveStatus::Error),
            Err(payload) => report(host, &panic_message(payload), RveStatus::Panicked),
        }
    }

    fn report(host: *const RveHost, message: &str, status: RveStatus) -> RveStatus {
        if !host.is_null() {
            // SAFETY: host is valid while the guest callback is active.
            let host = unsafe { &*host };
            // SAFETY: host callback copies the message during this call.
            unsafe { (host.set_error)(host.ctx, RveStr::from_str_ref(message)) };
        }
        status
    }

    fn panic_message(payload: Box<dyn Any + Send>) -> String {
        if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_owned()
        } else {
            "non-string panic payload".to_owned()
        }
    }
}

/// Builds a process-lifetime module table without exporting a global symbol.
///
/// Each script may declare its parameters in an optional block, see [`export_scripts!`].
#[macro_export]
macro_rules! script_module {
    ($($name:literal => $factory:path $({ $($params:tt)* })?),+ $(,)?) => {{
        const SCRIPT_COUNT: usize = <[()]>::len(&[$($crate::script_module!(@unit $name)),+]);
        static SCRIPTS: [$crate::abi::RveScriptDesc; SCRIPT_COUNT] = [
            $($crate::__rve_script_descriptor!($name, $factory, [$($($params)*)?])),+
        ];
        static MODULE: $crate::abi::RveModule = $crate::abi::RveModule::new(&SCRIPTS);
        &MODULE
    }};
    (@unit $name:literal) => { () };
}

/// Defines the required `rve_script_module` C ABI export for a game library.
///
/// A script may declare typed parameters. The host fills missing ones with their defaults and
/// rejects wrong types, out-of-range numbers and undeclared keys before calling the factory:
///
/// ```ignore
/// rve_sdk::export_scripts! {
///     "Bobber" => create_bobber {
///         amplitude: f32 = 0.15,
///         speed: f32 = 1.4 (0.0..=10.0) "Speed",
///         axis: [f32; 3] = [0.0, 1.0, 0.0],
///     }
/// }
/// ```
///
/// Supported types are `bool`, `i64`, `f32`/`f64` (float), `&str` and `[f32; 3]`. Defaults are
/// literals; numeric types accept an inclusive `(min..=max)` range, and every parameter accepts a
/// trailing label literal for editors. A script without a block declares no schema and receives
/// scene parameters unchecked.
#[macro_export]
macro_rules! export_scripts {
    ($($name:literal => $factory:path $({ $($params:tt)* })?),+ $(,)?) => {
        #[unsafe(no_mangle)]
        #[doc = "Returns this game's static RVE script module table."]
        pub extern "C" fn rve_script_module() -> *const $crate::abi::RveModule {
            $crate::script_module!($($name => $factory $({ $($params)* })?),+)
        }
    };
}

/// Parses a parameter block into a process-lifetime `&[RveParamDesc]`.
#[doc(hidden)]
#[macro_export]
macro_rules! __rve_params {
    (@munch [$($acc:expr,)*]) => {{
        static PARAMS: &[$crate::abi::RveParamDesc] = &[$($acc),*];
        PARAMS
    }};
    (@munch [$($acc:expr,)*] $param:ident : bool = $default:literal
        $($label:literal)? $(, $($rest:tt)*)?) => {
        $crate::__rve_params!(@munch [$($acc,)* $crate::__private::param(
            stringify!($param),
            $crate::abi::RVE_PARAM_BOOL,
            $crate::abi::RveValue::boolean($default),
            None,
            None,
            $crate::__rve_label!($($label)?),
        ),] $($($rest)*)?)
    };
    (@munch [$($acc:expr,)*] $param:ident : i64 = $default:literal
        $(($min:literal ..= $max:literal))? $($label:literal)? $(, $($rest:tt)*)?) => {
        $crate::__rve_params!(@munch [$($acc,)* $crate::__private::param(
            stringify!($param),
            $crate::abi::RVE_PARAM_INT,
            $crate::abi::RveValue::integer($default),
            $crate::__rve_bound!($($min)?),
            $crate::__rve_bound!($($max)?),
            $crate::__rve_label!($($label)?),
        ),] $($($rest)*)?)
    };
    (@munch [$($acc:expr,)*] $param:ident : f32 $($tail:tt)*) => {
        $crate::__rve_params!(@munch [$($acc,)*] $param : f64 $($tail)*)
    };
    (@munch [$($acc:expr,)*] $param:ident : f64 = $default:literal
        $(($min:literal ..= $max:literal))? $($label:literal)? $(, $($rest:tt)*)?) => {
        $crate::__rve_params!(@munch [$($acc,)* $crate::__private::param(
            stringify!($param),
            $crate::abi::RVE_PARAM_FLOAT,
            $crate::abi::RveValue::float($default as f64),
            $crate::__rve_bound!($($min)?),
            $crate::__rve_bound!($($max)?),
            $crate::__rve_label!($($label)?),
        ),] $($($rest)*)?)
    };
    (@munch [$($acc:expr,)*] $param:ident : &str = $default:literal
        $($label:literal)? $(, $($rest:tt)*)?) => {
        $crate::__rve_params!(@munch [$($acc,)* $crate::__private::param(
            stringify!($param),
            $crate::abi::RVE_PARAM_STRING,
            $crate::abi::RveValue::static_string($default),
            None,
            None,
            $crate::__rve_label!($($label)?),
        ),] $($($rest)*)?)
    };
    (@munch [$($acc:expr,)*] $param:ident : [f32; 3] = [$x:literal, $y:literal, $z:literal]
        $($label:literal)? $(, $($rest:tt)*)?) => {
        $crate::__rve_params!(@munch [$($acc,)* $crate::__private::param(
            stringify!($param),
            $crate::abi::RVE_PARAM_VEC3,
            {
                static VALUES: [$crate::abi::RveValue; 3] = [
                    $crate::abi::RveValue::float($x as f64),
                    $crate::abi::RveValue::float($y as f64),
                    $crate::abi::RveValue::float($z as f64),
                ];
                $crate::abi::RveValue::array(&VALUES)
            },
            None,
            None,
            $crate::__rve_label!($($label)?),
        ),] $($($rest)*)?)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __rve_bound {
    () => {
        None
    };
    ($value:literal) => {
        Some($value as f64)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __rve_label {
    () => {
        None
    };
    ($label:literal) => {
        Some($label)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __rve_script_descriptor {
    ($name:literal, $factory:path, [$($params:tt)*]) => {{
        unsafe extern "C" fn __rve_create(
            host: *const $crate::abi::RveHost,
            params: *const $crate::abi::RveParams,
            out: *mut *mut ::std::ffi::c_void,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host ABI supplies all callback pointers for the duration of this call.
            unsafe { $crate::__private::create(host, params, out, $factory) }
        }
        unsafe extern "C" fn __rve_start(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host ABI owns the instance and call-scoped host pointer.
            unsafe { $crate::__private::start(instance, host) }
        }
        unsafe extern "C" fn __rve_update(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
            dt: f32,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host ABI owns the instance and call-scoped host pointer.
            unsafe { $crate::__private::update(instance, host, dt) }
        }
        unsafe extern "C" fn __rve_message(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
            sender: $crate::abi::RveStr,
            name: $crate::abi::RveStr,
            value: *const $crate::abi::RveValue,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host ABI owns the instance and call-scoped message pointers.
            unsafe { $crate::__private::message(instance, host, sender, name, value) }
        }
        unsafe extern "C" fn __rve_command(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
            name: $crate::abi::RveStr,
            args: *const $crate::abi::RveStr,
            arg_count: usize,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host ABI owns the instance and call-scoped command argument pointers.
            unsafe { $crate::__private::command(instance, host, name, args, arg_count) }
        }
        unsafe extern "C" fn __rve_collision(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
            event: $crate::abi::RveCollisionEvent,
        ) -> $crate::abi::RveStatus {
            // SAFETY: the host owns the instance and event for this callback.
            unsafe { $crate::__private::collision(instance, host, event) }
        }
        unsafe extern "C" fn __rve_net_event(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
            event: $crate::abi::RveNetEvent,
        ) -> $crate::abi::RveStatus {
            // SAFETY: the host owns the instance and event for this callback.
            unsafe { $crate::__private::net_event(instance, host, event) }
        }
        unsafe extern "C" fn __rve_ui_event(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
            event: $crate::abi::RveUiEvent,
        ) -> $crate::abi::RveStatus {
            // SAFETY: the host owns the instance and event for this callback.
            unsafe { $crate::__private::ui_event(instance, host, event) }
        }
        unsafe extern "C" fn __rve_destroy(
            instance: *mut ::std::ffi::c_void,
            host: *const $crate::abi::RveHost,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host ABI owns the instance and call-scoped host pointer.
            unsafe { $crate::__private::destroy(instance, host) }
        }
        unsafe extern "C" fn __rve_drop_instance(
            instance: *mut ::std::ffi::c_void,
        ) -> $crate::abi::RveStatus {
            // SAFETY: host calls exactly once for a successfully created instance.
            unsafe { $crate::__private::drop_instance(instance) }
        }
        $crate::__private::descriptor(
            $name,
            __rve_create,
            __rve_start,
            __rve_update,
            __rve_message,
            __rve_destroy,
            __rve_drop_instance,
            __rve_command,
            __rve_collision,
            __rve_net_event,
            __rve_ui_event,
            $crate::__rve_params!(@munch [] $($params)*),
        )
    }};
}

#[cfg(test)]
mod tests;
