use crate::abi_string::read_abi_str;
use crate::lite::EventQueue;
use crate::transaction::{
    MAX_MESSAGE_NAME_BYTES, MAX_MESSAGE_TARGET_ID_BYTES, MessageValueBudget, QueueMessageError,
    ScriptTransaction,
};
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptTime, ScriptTransform};
use engine_scene::ParamValue;
use rve_sdk::abi::{
    RVE_LOG_ERROR, RVE_LOG_INFO, RVE_LOG_WARN, RVE_VALUE_ARRAY, RVE_VALUE_BOOL, RVE_VALUE_FLOAT,
    RVE_VALUE_INT, RVE_VALUE_STR, RveArray, RveHost, RveParamEntry, RveParams, RveScriptDesc,
    RveStatus, RveStr, RveTime, RveTransform, RveValue, RveValueData,
};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) struct AbiCallContext<'a> {
    self_object: u64,
    transaction: &'a mut ScriptTransaction,
    events: EventQueue,
    source: &'a str,
    error: Option<String>,
    poisoned: bool,
}

pub(crate) struct AbiScriptCall<'a> {
    pub(crate) self_object: u64,
    pub(crate) transaction: &'a mut ScriptTransaction,
    pub(crate) events: &'a EventQueue,
    pub(crate) source: &'a str,
}

impl<'a> AbiCallContext<'a> {
    pub(crate) fn new(
        self_object: u64,
        transaction: &'a mut ScriptTransaction,
        events: &EventQueue,
        source: &'a str,
    ) -> Self {
        Self {
            self_object,
            transaction,
            events: events.clone(),
            source,
            error: None,
            poisoned: false,
        }
    }

    pub(crate) fn host(&mut self, time: ScriptTime) -> RveHost {
        RveHost {
            ctx: (self as *mut Self).cast(),
            time: RveTime {
                elapsed: time.elapsed,
                frame: time.frame,
            },
            self_object: self.self_object,
            set_error: host_set_error,
            log: host_log,
            find_object: host_find_object,
            object_id: host_object_id,
            object_name: host_object_name,
            get_local: host_get_local,
            set_local: host_set_local,
            world_matrix: host_world_matrix,
            send: host_send,
            broadcast: host_broadcast,
            key_state: host_key_state,
            mouse_button_state: host_mouse_button_state,
            cursor_position: host_cursor_position,
            mouse_delta: host_mouse_delta,
            wheel_delta: host_wheel_delta,
        }
    }

    pub(crate) fn callback_result(&mut self, status: RveStatus) -> Result<(), String> {
        if self.poisoned {
            return Err("host callback panicked; ABI call is poisoned".to_owned());
        }
        if status != RveStatus::Ok {
            return Err(self.error.take().unwrap_or_else(|| match status {
                RveStatus::Error => "ABI script callback returned Error".to_owned(),
                RveStatus::Panicked => "ABI script callback panicked".to_owned(),
                RveStatus::Ok => unreachable!(),
            }));
        }
        Ok(())
    }
}

pub(crate) struct AbiScriptInstance {
    instance: *mut c_void,
    descriptor: &'static RveScriptDesc,
    events: EventQueue,
    source: String,
}

impl AbiScriptInstance {
    pub(crate) fn create(
        descriptor: &'static RveScriptDesc,
        params: &BTreeMap<String, ParamValue>,
        call: AbiScriptCall<'_>,
    ) -> Result<Self, String> {
        let params = OwnedAbiParams::new(params);
        let mut context =
            AbiCallContext::new(call.self_object, call.transaction, call.events, call.source);
        let mut instance = std::ptr::null_mut();
        let status = {
            let host = context.host(ScriptTime::default());
            // SAFETY: callback context and borrowed parameters remain live through create.
            unsafe { (descriptor.create)(&host, &params.abi, &mut instance) }
        };
        let poisoned = context.poisoned;
        let error = context.error.take();
        drop(context);
        if poisoned {
            if status == RveStatus::Ok && !instance.is_null() {
                drop(Self {
                    instance,
                    descriptor,
                    events: call.events.clone(),
                    source: call.source.to_owned(),
                });
            }
            return Err("host callback panicked during create".to_owned());
        }
        if status != RveStatus::Ok {
            return Err(error.unwrap_or_else(|| match status {
                RveStatus::Error => "ABI create returned Error".to_owned(),
                RveStatus::Panicked => "ABI create panicked".to_owned(),
                RveStatus::Ok => unreachable!(),
            }));
        }
        if instance.is_null() {
            return Err("ABI create returned Ok with a null instance".to_owned());
        }
        Ok(Self {
            instance,
            descriptor,
            events: call.events.clone(),
            source: call.source.to_owned(),
        })
    }

    pub(crate) fn invoke(
        &mut self,
        phase: AbiPhase,
        time: ScriptTime,
        call: AbiScriptCall<'_>,
    ) -> Result<(), String> {
        let mut context =
            AbiCallContext::new(call.self_object, call.transaction, call.events, call.source);
        let status = {
            let host = context.host(time);
            // SAFETY: this instance came from its matching descriptor's successful create call.
            unsafe {
                match phase {
                    AbiPhase::Start => (self.descriptor.start)(self.instance, &host),
                    AbiPhase::Update(dt) => (self.descriptor.update)(self.instance, &host, dt),
                    AbiPhase::Destroy => (self.descriptor.destroy)(self.instance, &host),
                }
            }
        };
        context.callback_result(status)
    }

    pub(crate) fn invoke_message(
        &mut self,
        time: ScriptTime,
        sender: &str,
        name: &str,
        value: &ParamValue,
        call: AbiScriptCall<'_>,
    ) -> Result<(), String> {
        let value = OwnedAbiValue::new(value);
        let sender = RveStr::from_str_ref(sender);
        let name = RveStr::from_str_ref(name);
        let mut context =
            AbiCallContext::new(call.self_object, call.transaction, call.events, call.source);
        let status = {
            let host = context.host(time);
            // SAFETY: message data, instance, and host remain live throughout the callback.
            unsafe { (self.descriptor.message)(self.instance, &host, sender, name, &value.value) }
        };
        context.callback_result(status)
    }
}

impl Drop for AbiScriptInstance {
    fn drop(&mut self) {
        // SAFETY: this instance owns a successful create result and Drop runs exactly once.
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            (self.descriptor.drop)(self.instance)
        }));
        let message = match result {
            Ok(RveStatus::Ok) => return,
            Ok(RveStatus::Error) => "ABI script drop returned Error".to_owned(),
            Ok(RveStatus::Panicked) => "ABI script drop panicked".to_owned(),
            Err(_) => "ABI script drop unwound across its callback".to_owned(),
        };
        if let Ok(mut events) = self.events.try_borrow_mut() {
            events.push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: self.source.clone(),
                message,
            });
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AbiPhase {
    Start,
    Update(f32),
    Destroy,
}

fn guarded<T: Copy>(
    ctx: *mut c_void,
    fallback: T,
    callback: impl FnOnce(&mut AbiCallContext<'_>) -> T,
) -> T {
    if ctx.is_null() {
        return fallback;
    }
    // SAFETY: the ABI host passes a live call context for the duration of each host callback.
    let result = catch_unwind(AssertUnwindSafe(|| {
        callback(unsafe { &mut *ctx.cast::<AbiCallContext<'_>>() })
    }));
    match result {
        Ok(value) => value,
        Err(_) => {
            // SAFETY: catch_unwind has ended the temporary mutable borrow above.
            unsafe { (*ctx.cast::<AbiCallContext<'_>>()).poisoned = true };
            fallback
        }
    }
}

unsafe extern "C" fn host_set_error(ctx: *mut c_void, message: RveStr) {
    guarded(ctx, (), |context| {
        context.error = Some(
            unsafe { read_abi_str(message) }
                .map(str::to_owned)
                .unwrap_or_else(|_| "guest supplied an invalid error message".to_owned()),
        );
    });
}

unsafe extern "C" fn host_log(ctx: *mut c_void, level: u32, message: RveStr) {
    guarded(ctx, (), |context| {
        let message = unsafe { read_abi_str(message) }
            .map(str::to_owned)
            .unwrap_or_else(|_| "guest supplied an invalid log message".to_owned());
        #[cfg(test)]
        if message == "__rve_test_host_log_panic__" {
            panic!("test host log panic");
        }
        let level = match level {
            RVE_LOG_INFO => ScriptEventLevel::Info,
            RVE_LOG_WARN => ScriptEventLevel::Warn,
            RVE_LOG_ERROR => ScriptEventLevel::Error,
            _ => ScriptEventLevel::Error,
        };
        context.events.borrow_mut().push(ScriptEvent {
            level,
            source: context.source.to_owned(),
            message,
        });
    });
}

unsafe extern "C" fn host_find_object(ctx: *mut c_void, id: RveStr) -> u64 {
    guarded(ctx, 0, |context| {
        let Ok(id) = (unsafe { read_abi_str(id) }) else {
            return 0;
        };
        context
            .transaction
            .handle_by_id(id)
            .and_then(|handle| context.transaction.abi_handle(handle))
            .unwrap_or(0)
    })
}

unsafe extern "C" fn host_object_id(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(id) = context.transaction.object_id(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(id) };
        true
    })
}

unsafe extern "C" fn host_object_name(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(name) = context.transaction.object_name(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(name) };
        true
    })
}

unsafe extern "C" fn host_get_local(ctx: *mut c_void, object: u64, out: *mut RveTransform) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(transform) = context.transaction.local_transform(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe {
            *out = RveTransform {
                position: transform.position,
                rotation_deg: transform.rotation,
                scale: transform.scale,
            }
        };
        true
    })
}

unsafe extern "C" fn host_set_local(
    ctx: *mut c_void,
    object: u64,
    transform: *const RveTransform,
) -> bool {
    guarded(ctx, false, |context| {
        if transform.is_null() {
            return false;
        }
        // SAFETY: transform was checked non-null and is valid for this callback.
        let transform = unsafe { *transform };
        let transform = ScriptTransform {
            position: transform.position,
            rotation: transform.rotation_deg,
            scale: transform.scale,
        };
        context.transaction.set_local_from_abi(object, transform)
    })
}

unsafe extern "C" fn host_world_matrix(ctx: *mut c_void, object: u64, out: *mut [f32; 16]) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(matrix) = context.transaction.world_matrix(handle) else {
            return false;
        };
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = matrix };
        true
    })
}

unsafe extern "C" fn host_send(
    ctx: *mut c_void,
    target_id: RveStr,
    name: RveStr,
    value: *const RveValue,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() {
            return false;
        }
        if target_id.len > MAX_MESSAGE_TARGET_ID_BYTES {
            return false;
        }
        let target_id = match unsafe { read_abi_str(target_id) } {
            Ok(target_id) => target_id,
            Err(error) => {
                context.error = Some(format!("invalid message target id: {error:?}"));
                return false;
            }
        };
        if name.len > MAX_MESSAGE_NAME_BYTES {
            context.error = Some(format!(
                "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
            ));
            return false;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name,
            Err(error) => {
                context.error = Some(format!("invalid message name: {error:?}"));
                return false;
            }
        };
        // SAFETY: `value` is provided by the guest for the duration of this ABI callback.
        let (value, value_size) = match unsafe { read_message_value(value) } {
            Ok(value) => value,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        match context.transaction.send(target_id, name, value, value_size) {
            Ok(()) => true,
            Err(QueueMessageError::UnknownTarget | QueueMessageError::ReadOnly) => false,
            Err(QueueMessageError::InvalidName) => {
                context.error = Some(format!(
                    "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
                ));
                false
            }
            Err(QueueMessageError::InvalidValue) => {
                context.error =
                    Some("message value is invalid or exceeds the message limits".to_owned());
                false
            }
        }
    })
}

unsafe extern "C" fn host_broadcast(
    ctx: *mut c_void,
    name: RveStr,
    value: *const RveValue,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.transaction.writable() {
            return false;
        }
        if name.len > MAX_MESSAGE_NAME_BYTES {
            context.error = Some(format!(
                "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
            ));
            return false;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name,
            Err(error) => {
                context.error = Some(format!("invalid message name: {error:?}"));
                return false;
            }
        };
        // SAFETY: `value` is provided by the guest for the duration of this ABI callback.
        let (value, value_size) = match unsafe { read_message_value(value) } {
            Ok(value) => value,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        match context.transaction.broadcast(name, value, value_size) {
            Ok(()) => true,
            Err(QueueMessageError::ReadOnly) => false,
            Err(QueueMessageError::UnknownTarget) => unreachable!(),
            Err(QueueMessageError::InvalidName) => {
                context.error = Some(format!(
                    "message name exceeds {MAX_MESSAGE_NAME_BYTES} bytes"
                ));
                false
            }
            Err(QueueMessageError::InvalidValue) => {
                context.error =
                    Some("message value is invalid or exceeds the message limits".to_owned());
                false
            }
        }
    })
}

unsafe extern "C" fn host_key_state(ctx: *mut c_void, key: u32) -> u32 {
    guarded(ctx, 0, |context| context.transaction.input().key_state(key))
}

unsafe extern "C" fn host_mouse_button_state(ctx: *mut c_void, button: u32) -> u32 {
    guarded(ctx, 0, |context| {
        context.transaction.input().mouse_button_state(button)
    })
}

unsafe extern "C" fn host_cursor_position(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    host_input_vector(ctx, out, |input| input.cursor())
}

unsafe extern "C" fn host_mouse_delta(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    host_input_vector(ctx, out, |input| input.mouse_delta())
}

unsafe extern "C" fn host_wheel_delta(ctx: *mut c_void, out: *mut [f32; 2]) -> bool {
    host_input_vector(ctx, out, |input| input.wheel())
}

fn host_input_vector(
    ctx: *mut c_void,
    out: *mut [f32; 2],
    value: impl FnOnce(&crate::types::ScriptInput) -> [f32; 2],
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        // SAFETY: output was checked and points to the guest's writable stack slot.
        unsafe { *out = value(context.transaction.input()) };
        true
    })
}

unsafe fn read_message_value(value: *const RveValue) -> Result<(ParamValue, usize), String> {
    if value.is_null() {
        return Err("null message value".to_owned());
    }
    // SAFETY: the guest passes the value pointer for the duration of this host callback.
    let mut budget = MessageValueBudget::new();
    let value = unsafe { read_message_value_inner(*value, 0, &mut budget) }?;
    Ok((value, budget.estimated_size()))
}

unsafe fn read_message_value_inner(
    value: RveValue,
    depth: usize,
    budget: &mut MessageValueBudget,
) -> Result<ParamValue, String> {
    budget.enter(depth).map_err(str::to_owned)?;
    match value.tag {
        RVE_VALUE_BOOL => Ok(ParamValue::Bool(unsafe { value.data.boolean })),
        RVE_VALUE_INT => Ok(ParamValue::Int(unsafe { value.data.integer })),
        RVE_VALUE_FLOAT => {
            let value = unsafe { value.data.float };
            if value.is_finite() {
                Ok(ParamValue::Float(value))
            } else {
                Err("message numbers must be finite".to_owned())
            }
        }
        RVE_VALUE_STR => {
            let string = unsafe { value.data.string };
            budget.add_string(string.len).map_err(str::to_owned)?;
            let string = unsafe { read_abi_str(string) }
                .map_err(|error| format!("message string is invalid: {error:?}"))?;
            Ok(ParamValue::String(string.to_owned()))
        }
        RVE_VALUE_ARRAY => {
            let array = unsafe { value.data.array };
            if array.len > budget.remaining_elements() {
                return Err("message value contains more than 1024 elements".to_owned());
            }
            if array.len == 0 {
                return Ok(ParamValue::Array(Vec::new()));
            }
            if array.ptr.is_null() {
                return Err("null array values".to_owned());
            }
            let values = unsafe { std::slice::from_raw_parts(array.ptr, array.len) };
            let mut owned = Vec::with_capacity(array.len);
            for value in values.iter().copied() {
                owned.push(unsafe { read_message_value_inner(value, depth + 1, budget) }?);
            }
            Ok(ParamValue::Array(owned))
        }
        tag => Err(format!("unknown value tag {tag}")),
    }
}

struct OwnedAbiValue {
    value: RveValue,
    _string: Option<String>,
    _children: Vec<OwnedAbiValue>,
    _array: Vec<RveValue>,
}

impl OwnedAbiValue {
    fn new(value: &ParamValue) -> Self {
        match value {
            ParamValue::Bool(value) => Self::leaf(RveValue {
                tag: rve_sdk::abi::RVE_VALUE_BOOL,
                data: RveValueData { boolean: *value },
            }),
            ParamValue::Int(value) => Self::leaf(RveValue {
                tag: rve_sdk::abi::RVE_VALUE_INT,
                data: RveValueData { integer: *value },
            }),
            ParamValue::Float(value) => Self::leaf(RveValue {
                tag: rve_sdk::abi::RVE_VALUE_FLOAT,
                data: RveValueData { float: *value },
            }),
            ParamValue::String(value) => {
                let value = value.clone();
                let abi_value = RveValue {
                    tag: rve_sdk::abi::RVE_VALUE_STR,
                    data: RveValueData {
                        string: RveStr::from_str_ref(&value),
                    },
                };
                Self {
                    value: abi_value,
                    _string: Some(value),
                    _children: Vec::new(),
                    _array: Vec::new(),
                }
            }
            ParamValue::Array(values) => {
                let children = values.iter().map(Self::new).collect::<Vec<_>>();
                let array = children.iter().map(|child| child.value).collect::<Vec<_>>();
                let value = RveValue {
                    tag: rve_sdk::abi::RVE_VALUE_ARRAY,
                    data: RveValueData {
                        array: RveArray {
                            ptr: array.as_ptr(),
                            len: array.len(),
                        },
                    },
                };
                Self {
                    value,
                    _string: None,
                    _children: children,
                    _array: array,
                }
            }
        }
    }

    fn leaf(value: RveValue) -> Self {
        Self {
            value,
            _string: None,
            _children: Vec::new(),
            _array: Vec::new(),
        }
    }
}

struct OwnedAbiParams {
    _keys: Vec<String>,
    _values: Vec<OwnedAbiValue>,
    _entries: Vec<RveParamEntry>,
    abi: RveParams,
}

impl OwnedAbiParams {
    fn new(params: &BTreeMap<String, ParamValue>) -> Self {
        let keys = params.keys().cloned().collect::<Vec<_>>();
        let values = params.values().map(OwnedAbiValue::new).collect::<Vec<_>>();
        let entries = keys
            .iter()
            .zip(&values)
            .map(|(key, value)| RveParamEntry {
                key: RveStr::from_str_ref(key),
                value: value.value,
            })
            .collect::<Vec<_>>();
        let abi = RveParams {
            entries: entries.as_ptr(),
            len: entries.len(),
        };
        Self {
            _keys: keys,
            _values: values,
            _entries: entries,
            abi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_logs_keep_the_warn_level() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene/object/Probe");
        let host = context.host(ScriptTime::default());
        // SAFETY: the host context and strings remain live for these callbacks.
        unsafe {
            (host.log)(host.ctx, RVE_LOG_INFO, RveStr::from_static("info"));
            (host.log)(host.ctx, RVE_LOG_WARN, RveStr::from_static("warn"));
            (host.log)(host.ctx, RVE_LOG_ERROR, RveStr::from_static("error"));
            (host.log)(host.ctx, u32::MAX, RveStr::from_static("unknown"));
        }
        let events = events.borrow();
        assert_eq!(events[0].level, ScriptEventLevel::Info);
        assert_eq!(events[1].level, ScriptEventLevel::Warn);
        assert_eq!(events[2].level, ScriptEventLevel::Error);
        assert_eq!(events[3].level, ScriptEventLevel::Error);
    }

    #[test]
    fn unknown_input_codes_return_zero_and_vectors_are_readable_during_create() {
        let events = EventQueue::default();
        let mut transaction = ScriptTransaction::default();
        let mut context = AbiCallContext::new(1, &mut transaction, &events, "scene");
        let host = context.host(ScriptTime::default());
        let mut cursor = [1.0; 2];
        let mut mouse_delta = [1.0; 2];
        let mut wheel = [1.0; 2];

        // SAFETY: the host context and output slots remain live for these callbacks.
        unsafe {
            assert_eq!((host.key_state)(host.ctx, u32::MAX), 0);
            assert_eq!((host.mouse_button_state)(host.ctx, u32::MAX), 0);
            assert!((host.cursor_position)(host.ctx, &mut cursor));
            assert!((host.mouse_delta)(host.ctx, &mut mouse_delta));
            assert!((host.wheel_delta)(host.ctx, &mut wheel));
        }
        assert_eq!(cursor, [0.0; 2]);
        assert_eq!(mouse_delta, [0.0; 2]);
        assert_eq!(wheel, [0.0; 2]);
    }
}
