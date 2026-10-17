use crate::abi::{
    RVE_SCRIPT_ABI_VERSION, RVE_VALUE_ARRAY, RVE_VALUE_STR, RveArray, RveHeader, RveHost,
    RveModule, RveParamEntry, RveParams, RveScriptDesc, RveStatus, RveStr, RveTime, RveTransform,
    RveValue, RveValueData,
};
use crate::{Context, Params, Script, Value};
use std::ffi::c_void;

struct Probe {
    panic_start: bool,
    panic_update: bool,
    panic_message: bool,
    panic_destroy: bool,
    panic_drop: bool,
    last_message: String,
}

impl Script for Probe {
    fn on_start(&mut self, ctx: &mut Context<'_>) {
        assert_eq!(ctx.time().elapsed, 12.5);
        assert_eq!(ctx.object_id().as_deref(), Some("camera"));
        assert_eq!(ctx.object_name().as_deref(), Some("Camera"));
        ctx.log_info("guest log");
        ctx.transform.position[0] = 7.0;
        assert!(self.panic_start.then(|| panic!("start panic")).is_none());
    }

    fn on_update(&mut self, ctx: &mut Context<'_>, dt: f32) {
        if self.panic_update {
            panic!("update panic");
        }
        ctx.transform.position[1] += dt;
        if self.last_message.is_empty() {
            ctx.log_info("guest update");
        } else {
            ctx.log_info(format!("message:{}", self.last_message));
        }
    }

    fn on_message(&mut self, sender: &str, name: &str, _: &Value) {
        if self.panic_message {
            panic!("message panic");
        }
        self.last_message = format!("{sender}:{name}");
    }

    fn on_destroy(&mut self, ctx: &mut Context<'_>) {
        if self.panic_destroy {
            panic!("destroy panic");
        }
        ctx.log_info("guest destroy");
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        if self.panic_drop {
            panic!("drop panic");
        }
    }
}

fn probe_factory(params: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(Probe {
        panic_start: matches!(params.get("panic_start"), Some(Value::Bool(true))),
        panic_update: matches!(params.get("panic_update"), Some(Value::Bool(true))),
        panic_message: matches!(params.get("panic_message"), Some(Value::Bool(true))),
        panic_destroy: matches!(params.get("panic_destroy"), Some(Value::Bool(true))),
        panic_drop: matches!(params.get("panic_drop"), Some(Value::Bool(true))),
        last_message: String::new(),
    }))
}

fn error_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Err("factory error".to_owned())
}

fn panic_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    panic!("factory panic")
}

fn scripts() -> &'static RveModule {
    crate::script_module! {
        "Probe" => probe_factory,
        "Error" => error_factory,
        "Panic" => panic_factory,
    }
}

mod factories_named_like_callbacks {
    use crate::abi::RveModule;
    use crate::{Params, Script};

    struct Named;

    impl Script for Named {}

    fn create(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Named))
    }

    fn start(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Named))
    }

    pub(super) fn module() -> &'static RveModule {
        crate::script_module! {
            "CreateFactory" => create,
            "StartFactory" => start,
        }
    }
}

mod exported {
    use crate::{Params, Script};

    struct Exported;

    impl Script for Exported {}

    fn factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Exported))
    }

    crate::export_scripts! { "Exported" => factory }
}

#[derive(Default)]
struct HostState {
    error: Option<String>,
    transform: RveTransform,
    logs: Vec<String>,
}

unsafe extern "C" fn set_error(ctx: *mut c_void, message: RveStr) {
    // SAFETY: test host points to a live HostState and guest strings are valid during the call.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: message references a live guest string for this callback.
    state.error = Some(unsafe { message.to_str() }.unwrap().to_owned());
}

unsafe extern "C" fn log(ctx: *mut c_void, _: u32, message: RveStr) {
    // SAFETY: test host points to a live HostState and guest strings are valid during the call.
    let state = unsafe { &mut *ctx.cast::<HostState>() };
    // SAFETY: message references a live guest string for this callback.
    state
        .logs
        .push(unsafe { message.to_str() }.unwrap().to_owned());
}

unsafe extern "C" fn find_object(_: *mut c_void, _: RveStr) -> u64 {
    0
}

unsafe extern "C" fn object_id(_: *mut c_void, _: u64, out: *mut RveStr) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: camera_id has process lifetime and out is supplied by the ABI caller.
    unsafe { *out = RveStr::from_static("camera") };
    true
}

unsafe extern "C" fn object_name(_: *mut c_void, _: u64, out: *mut RveStr) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: camera_name has process lifetime and out is supplied by the ABI caller.
    unsafe { *out = RveStr::from_static("Camera") };
    true
}

unsafe extern "C" fn get_local(ctx: *mut c_void, _: u64, out: *mut RveTransform) -> bool {
    if out.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and out is supplied by the ABI caller.
    unsafe { *out = (&*ctx.cast::<HostState>()).transform };
    true
}

unsafe extern "C" fn set_local(ctx: *mut c_void, _: u64, transform: *const RveTransform) -> bool {
    if transform.is_null() {
        return false;
    }
    // SAFETY: test host points to a live HostState and transform lives through this call.
    unsafe { (&mut *ctx.cast::<HostState>()).transform = *transform };
    true
}

fn host(state: &mut HostState) -> RveHost {
    RveHost {
        ctx: (state as *mut HostState).cast(),
        time: RveTime {
            elapsed: 12.5,
            frame: 8,
        },
        self_object: 1,
        set_error,
        log,
        find_object,
        object_id,
        object_name,
        get_local,
        set_local,
    }
}

fn descriptor(module: &RveModule, name: &str) -> &'static RveScriptDesc {
    // SAFETY: the macro produces a static immutable table.
    unsafe { std::slice::from_raw_parts(module.scripts, module.script_count) }
        .iter()
        .find(|script| {
            // SAFETY: descriptor names are static UTF-8 from this test macro.
            unsafe { script.name.to_str() }.unwrap() == name
        })
        .unwrap()
}

#[test]
fn script_module_macro_builds_abi_table_and_guest_callbacks() {
    let module = scripts();
    assert_eq!(module.header.abi_version, RVE_SCRIPT_ABI_VERSION);
    assert_eq!(
        module.header.header_size as usize,
        std::mem::size_of::<RveHeader>()
    );
    assert_eq!(module.script_count, 3);

    let script = descriptor(module, "Probe");
    let mut state = HostState {
        transform: RveTransform::default(),
        ..HostState::default()
    };
    let host = host(&mut state);
    let params = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: pointers are valid for the callback and the static descriptor is from this SDK.
    assert_eq!(
        unsafe { (script.create)(&host, &params, &mut instance) },
        RveStatus::Ok
    );
    assert!(!instance.is_null());
    // SAFETY: this callback uses the live host and instance created above.
    assert_eq!(unsafe { (script.start)(instance, &host) }, RveStatus::Ok);
    assert_eq!(state.transform.position, [7.0, 0.0, 0.0]);
    assert_eq!(state.logs, ["guest log"]);
    // SAFETY: drop is called exactly once for this successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);
}

#[test]
fn script_module_allows_factories_named_create_and_start() {
    let module = factories_named_like_callbacks::module();
    assert_eq!(module.script_count, 2);
    assert_eq!(
        unsafe { descriptor(module, "CreateFactory").name.to_str() }.unwrap(),
        "CreateFactory"
    );
    assert_eq!(
        unsafe { descriptor(module, "StartFactory").name.to_str() }.unwrap(),
        "StartFactory"
    );
}

#[test]
fn export_scripts_macro_exposes_a_static_module_pointer() {
    let module = exported::rve_script_module();
    assert!(!module.is_null());
    // SAFETY: the macro exports process-lifetime static module data.
    let module = unsafe { &*module };
    assert_eq!(module.header.abi_version, RVE_SCRIPT_ABI_VERSION);
    assert_eq!(module.script_count, 1);
}

#[test]
fn guest_wrappers_report_factory_and_callback_panics_and_drop_panics() {
    let module = scripts();
    let mut state = HostState::default();
    let host = host(&mut state);
    let empty = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: callback pointers are valid and the host remains live.
    assert_eq!(
        unsafe { (descriptor(module, "Panic").create)(&host, &empty, &mut instance) },
        RveStatus::Panicked
    );
    assert_eq!(state.error.as_deref(), Some("factory panic"));
    assert!(instance.is_null());

    state.error = None;
    let params = [RveParamEntry {
        key: RveStr::from_static("panic_start"),
        value: RveValue::boolean(true),
    }];
    let params = RveParams {
        entries: params.as_ptr(),
        len: params.len(),
    };
    let script = descriptor(module, "Probe");
    // SAFETY: pointers are valid for the callback and params remain live.
    assert_eq!(
        unsafe { (script.create)(&host, &params, &mut instance) },
        RveStatus::Ok
    );
    // SAFETY: callback pointers are valid and this instance was created above.
    assert_eq!(
        unsafe { (script.start)(instance, &host) },
        RveStatus::Panicked
    );
    assert_eq!(state.error.as_deref(), Some("start panic"));
    // SAFETY: drop is called exactly once after successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);

    state.error = None;
    let params = [RveParamEntry {
        key: RveStr::from_static("panic_drop"),
        value: RveValue::boolean(true),
    }];
    let params = RveParams {
        entries: params.as_ptr(),
        len: params.len(),
    };
    // SAFETY: pointers are valid for the callback and params remain live.
    assert_eq!(
        unsafe { (script.create)(&host, &params, &mut instance) },
        RveStatus::Ok
    );
    // SAFETY: drop is called exactly once after successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Panicked);
    assert_eq!(state.error, None);
}

#[test]
fn guest_wrappers_invoke_update_message_and_destroy_and_report_their_panics() {
    let script = descriptor(scripts(), "Probe");
    let mut state = HostState::default();
    let host = host(&mut state);
    let empty = RveParams {
        entries: std::ptr::null(),
        len: 0,
    };
    let mut instance = std::ptr::null_mut();
    // SAFETY: callback pointers are valid and the host remains live.
    assert_eq!(
        unsafe { (script.create)(&host, &empty, &mut instance) },
        RveStatus::Ok
    );
    // SAFETY: the instance was created above and pointers remain live.
    assert_eq!(
        unsafe { (script.update)(instance, &host, 1.5) },
        RveStatus::Ok
    );
    assert_eq!(state.transform.position[1], 1.5);
    let value = RveValue::string("hello");
    // SAFETY: message data and host remain live for the callback.
    assert_eq!(
        unsafe {
            (script.message)(
                instance,
                &host,
                RveStr::from_static("sender"),
                RveStr::from_static("greet"),
                &value,
            )
        },
        RveStatus::Ok
    );
    // SAFETY: the instance and host remain live for the callback.
    assert_eq!(
        unsafe { (script.update)(instance, &host, 0.5) },
        RveStatus::Ok
    );
    assert_eq!(state.transform.position[1], 2.0);
    assert_eq!(state.logs, ["guest update", "message:sender:greet"]);
    // SAFETY: the instance and host remain live for the callback.
    assert_eq!(unsafe { (script.destroy)(instance, &host) }, RveStatus::Ok);
    assert_eq!(
        state.logs,
        ["guest update", "message:sender:greet", "guest destroy"]
    );
    // SAFETY: drop is called exactly once after successful create.
    assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);

    for (parameter, callback, expected) in [
        ("panic_update", 0, "update panic"),
        ("panic_message", 1, "message panic"),
        ("panic_destroy", 2, "destroy panic"),
    ] {
        state.error = None;
        let entry = [RveParamEntry {
            key: RveStr::from_static(parameter),
            value: RveValue::boolean(true),
        }];
        let params = RveParams {
            entries: entry.as_ptr(),
            len: entry.len(),
        };
        // SAFETY: params and host remain live through create.
        assert_eq!(
            unsafe { (script.create)(&host, &params, &mut instance) },
            RveStatus::Ok
        );
        let status = match callback {
            0 => {
                // SAFETY: the instance and host remain live for this callback.
                unsafe { (script.update)(instance, &host, 0.25) }
            }
            1 => {
                // SAFETY: message data and host remain live for this callback.
                unsafe {
                    (script.message)(
                        instance,
                        &host,
                        RveStr::from_static("sender"),
                        RveStr::from_static("greet"),
                        &RveValue::integer(1),
                    )
                }
            }
            _ => {
                // SAFETY: the instance and host remain live for this callback.
                unsafe { (script.destroy)(instance, &host) }
            }
        };
        assert_eq!(status, RveStatus::Panicked);
        assert_eq!(state.error.as_deref(), Some(expected));
        // SAFETY: drop is called exactly once after each successful create.
        assert_eq!(unsafe { (script.drop)(instance) }, RveStatus::Ok);
    }
}

#[test]
fn borrowed_abi_values_and_params_become_owned_values() {
    let children = [RveValue::integer(-9), RveValue::string("value")];
    let nested = [RveValue {
        tag: RVE_VALUE_ARRAY,
        data: RveValueData {
            array: crate::abi::RveArray {
                ptr: children.as_ptr(),
                len: children.len(),
            },
        },
    }];
    let entries = [
        RveParamEntry {
            key: RveStr::from_static("enabled"),
            value: RveValue::boolean(true),
        },
        RveParamEntry {
            key: RveStr::from_static("ratio"),
            value: RveValue::float(1.25),
        },
        RveParamEntry {
            key: RveStr::from_static("nested"),
            value: RveValue::array(&nested),
        },
    ];
    let params = RveParams {
        entries: entries.as_ptr(),
        len: entries.len(),
    };
    // SAFETY: all pointers refer to the live arrays and static string keys above.
    let owned = unsafe { params.into_owned() }.unwrap();
    assert_eq!(owned["enabled"], Value::Bool(true));
    assert_eq!(owned["ratio"], Value::Float(1.25));
    assert_eq!(
        owned["nested"],
        Value::Array(vec![Value::Array(vec![
            Value::Int(-9),
            Value::String("value".to_owned())
        ])])
    );
}

#[test]
fn malformed_borrowed_values_and_duplicate_keys_are_rejected() {
    let invalid_utf8 = [0xff];
    let invalid_string = RveValue {
        tag: RVE_VALUE_STR,
        data: RveValueData {
            string: RveStr {
                ptr: invalid_utf8.as_ptr(),
                len: invalid_utf8.len(),
            },
        },
    };
    // SAFETY: the byte array is valid storage; its contents are intentionally invalid UTF-8.
    assert!(
        unsafe { invalid_string.into_owned() }
            .unwrap_err()
            .0
            .contains("not UTF-8")
    );

    let null_array = RveValue {
        tag: RVE_VALUE_ARRAY,
        data: RveValueData {
            array: RveArray {
                ptr: std::ptr::null(),
                len: 1,
            },
        },
    };
    // SAFETY: the converter detects the null pointer before constructing a slice.
    assert_eq!(
        unsafe { null_array.into_owned() }.unwrap_err().0,
        "null array values"
    );

    let unknown_tag = RveValue {
        tag: u32::MAX,
        data: RveValueData { integer: 0 },
    };
    // SAFETY: unknown tags are rejected before any union field is read.
    assert_eq!(
        unsafe { unknown_tag.into_owned() }.unwrap_err().0,
        "unknown value tag 4294967295"
    );

    let duplicates = [
        RveParamEntry {
            key: RveStr::from_static("same"),
            value: RveValue::integer(1),
        },
        RveParamEntry {
            key: RveStr::from_static("same"),
            value: RveValue::integer(2),
        },
    ];
    let params = RveParams {
        entries: duplicates.as_ptr(),
        len: duplicates.len(),
    };
    // SAFETY: entries and all referenced strings remain live for conversion.
    assert_eq!(
        unsafe { params.into_owned() }.unwrap_err().0,
        "duplicate parameter key \"same\""
    );
}
