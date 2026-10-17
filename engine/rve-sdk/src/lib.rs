//! Source SDK for Rust scripts loaded by the RVE player.
//!
//! The guest ABI catches Rust unwinds at every exported callback. A game profile using
//! `panic = "abort"` still terminates the process on panic because aborting panics cannot unwind.

pub mod abi;

pub use abi::{Key, MouseButton};
use abi::{
    RVE_VALUE_ARRAY, RVE_VALUE_BOOL, RVE_VALUE_FLOAT, RVE_VALUE_INT, RVE_VALUE_STR, RveArray,
    RveHost, RveParams, RveStr, RveTime, RveTransform, RveValue, RveValueData,
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

/// Lifecycle implemented by a guest script.
pub trait Script: 'static {
    /// Called once before the first update.
    fn on_start(&mut self, _ctx: &mut Context<'_>) {}
    /// Called for each update with the frame delta in seconds.
    fn on_update(&mut self, _ctx: &mut Context<'_>, _dt: f32) {}
    /// Called when a message is delivered to this script.
    fn on_message(&mut self, _ctx: &mut Context<'_>, _sender: &str, _name: &str, _value: &Value) {}
    /// Called when the script host is destroyed after this instance has started.
    fn on_destroy(&mut self, _ctx: &mut Context<'_>) {}
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
    _not_send_or_sync: PhantomData<*const ()>,
}

/// Read-only snapshot of keyboard, mouse, and wheel input for the current script callback.
pub struct Input<'a> {
    host: &'a RveHost,
}

/// Copyable handle to a scene object. It remains stable while its scene is loaded and may be
/// stored in a script struct. Every operation on it requires the current callback's `Context`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Object {
    handle: u64,
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

impl Input<'_> {
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

    /// Returns this object's column-major world matrix snapshot.
    pub fn world_matrix(&self, ctx: &mut Context<'_>) -> Option<[f32; 16]> {
        let mut matrix = [0.0; 16];
        // SAFETY: the host pointer and output slot are valid during this guest callback.
        if !unsafe { (ctx.host.world_matrix)(ctx.host.ctx, self.handle, &mut matrix) } {
            return None;
        }
        Some(matrix)
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

#[doc(hidden)]
pub mod __private {
    use super::{Context, Params, Script, ValueError};
    use crate::abi::{
        RveCreateFn, RveDropFn, RveHost, RveMessageFn, RveParams, RveStartFn, RveStatus, RveStr,
        RveUpdateFn,
    };
    use std::any::Any;
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    struct GuestInstance(Box<dyn Script>);

    pub const fn descriptor(
        name: &'static str,
        create: RveCreateFn,
        start: RveStartFn,
        update: RveUpdateFn,
        message: RveMessageFn,
        destroy: RveStartFn,
        drop: RveDropFn,
    ) -> crate::abi::RveScriptDesc {
        crate::abi::RveScriptDesc {
            name: RveStr::from_static(name),
            create,
            start,
            update,
            message,
            destroy,
            drop,
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
#[macro_export]
macro_rules! script_module {
    ($($name:literal => $factory:path),+ $(,)?) => {{
        const SCRIPT_COUNT: usize = <[()]>::len(&[$($crate::script_module!(@unit $name)),+]);
        static SCRIPTS: [$crate::abi::RveScriptDesc; SCRIPT_COUNT] = [
            $($crate::__rve_script_descriptor!($name, $factory)),+
        ];
        static MODULE: $crate::abi::RveModule = $crate::abi::RveModule::new(&SCRIPTS);
        &MODULE
    }};
    (@unit $name:literal) => { () };
}

/// Defines the required `rve_script_module` C ABI export for a game library.
#[macro_export]
macro_rules! export_scripts {
    ($($name:literal => $factory:path),+ $(,)?) => {
        #[unsafe(no_mangle)]
        #[doc = "Returns this game's static RVE script module table."]
        pub extern "C" fn rve_script_module() -> *const $crate::abi::RveModule {
            $crate::script_module!($($name => $factory),+)
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __rve_script_descriptor {
    ($name:literal, $factory:path) => {{
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
        )
    }};
}

#[cfg(test)]
mod tests;
