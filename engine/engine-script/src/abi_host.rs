use crate::abi_string::read_abi_str;
use crate::lite::EventQueue;
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptTime, ScriptTransform};
use engine_scene::ParamValue;
use rve_sdk::abi::{
    RVE_LOG_ERROR, RVE_LOG_INFO, RVE_LOG_WARN, RveArray, RveHost, RveParamEntry, RveParams,
    RveScriptDesc, RveStatus, RveStr, RveTime, RveTransform, RveValue, RveValueData,
};
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) struct AbiCallContext<'a> {
    pub(crate) self_object: u64,
    pub(crate) object_id: &'a str,
    pub(crate) object_name: &'a str,
    pub(crate) transform: RveTransform,
    pub(crate) events: &'a EventQueue,
    pub(crate) source: &'a str,
    pub(crate) error: Option<String>,
    pub(crate) poisoned: bool,
    can_write_transform: bool,
}

pub(crate) struct AbiScriptCall<'a> {
    pub(crate) self_object: u64,
    pub(crate) object_id: &'a str,
    pub(crate) object_name: &'a str,
    pub(crate) transform: ScriptTransform,
    pub(crate) events: &'a EventQueue,
    pub(crate) source: &'a str,
}

impl<'a> AbiCallContext<'a> {
    pub(crate) fn new(
        self_object: u64,
        object_id: &'a str,
        object_name: &'a str,
        transform: ScriptTransform,
        events: &'a EventQueue,
        source: &'a str,
        can_write_transform: bool,
    ) -> Self {
        Self {
            self_object,
            object_id,
            object_name,
            transform: RveTransform {
                position: transform.position,
                rotation_deg: transform.rotation,
                scale: transform.scale,
            },
            events,
            source,
            error: None,
            poisoned: false,
            can_write_transform,
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
        }
    }

    pub(crate) fn callback_result(&mut self, status: RveStatus) -> Result<ScriptTransform, String> {
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
        Ok(ScriptTransform {
            position: self.transform.position,
            rotation: self.transform.rotation_deg,
            scale: self.transform.scale,
        })
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
        let mut context = AbiCallContext::new(
            call.self_object,
            call.object_id,
            call.object_name,
            call.transform,
            call.events,
            call.source,
            false,
        );
        let mut instance = std::ptr::null_mut();
        let status = {
            let host = context.host(ScriptTime::default());
            // SAFETY: all callback data and borrowed parameters remain alive through create.
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
    ) -> Result<ScriptTransform, String> {
        let mut context = AbiCallContext::new(
            call.self_object,
            call.object_id,
            call.object_name,
            call.transform,
            call.events,
            call.source,
            true,
        );
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
            RVE_LOG_WARN => {
                // The host currently exposes only Info and Error event levels.
                ScriptEventLevel::Error
            }
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
        if unsafe { read_abi_str(id) }.is_ok_and(|id| id == context.object_id) {
            context.self_object
        } else {
            0
        }
    })
}

unsafe extern "C" fn host_object_id(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if object != context.self_object || out.is_null() {
            return false;
        }
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(context.object_id) };
        true
    })
}

unsafe extern "C" fn host_object_name(ctx: *mut c_void, object: u64, out: *mut RveStr) -> bool {
    guarded(ctx, false, |context| {
        if object != context.self_object || out.is_null() {
            return false;
        }
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = RveStr::from_str_ref(context.object_name) };
        true
    })
}

unsafe extern "C" fn host_get_local(ctx: *mut c_void, object: u64, out: *mut RveTransform) -> bool {
    guarded(ctx, false, |context| {
        if object != context.self_object || out.is_null() {
            return false;
        }
        // SAFETY: output was checked and points to the caller's stack slot.
        unsafe { *out = context.transform };
        true
    })
}

unsafe extern "C" fn host_set_local(
    ctx: *mut c_void,
    object: u64,
    transform: *const RveTransform,
) -> bool {
    guarded(ctx, false, |context| {
        if !context.can_write_transform || object != context.self_object || transform.is_null() {
            return false;
        }
        // SAFETY: transform was checked non-null and is valid for this callback.
        context.transform = unsafe { *transform };
        true
    })
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
    fn warning_error_and_unknown_log_levels_are_errors() {
        let events = EventQueue::default();
        let transform = ScriptTransform {
            position: [0.0; 3],
            rotation: [0.0; 3],
            scale: [1.0; 3],
        };
        let mut context = AbiCallContext::new(
            1,
            "object",
            "Object",
            transform,
            &events,
            "scene/object/Probe",
            true,
        );
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
        assert_eq!(events[1].level, ScriptEventLevel::Error);
        assert_eq!(events[2].level, ScriptEventLevel::Error);
        assert_eq!(events[3].level, ScriptEventLevel::Error);
    }
}
