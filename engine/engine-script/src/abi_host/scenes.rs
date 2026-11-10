//! ABI-функции для сцены и приложения.

use super::*;

pub(super) unsafe extern "C" fn host_scene_load(ctx: *mut c_void, path: RveStr) -> bool {
    guarded(ctx, false, |context| {
        let path = match unsafe { read_abi_str(path) } {
            Ok(path) => path.to_owned(),
            Err(error) => {
                context.error = Some(format!("scene path is invalid: {error:?}"));
                return false;
            }
        };
        match context.transaction.request_scene_load(&path) {
            Ok(()) => true,
            Err(_) => false,
        }
    })
}

pub(super) unsafe extern "C" fn host_scene_current(ctx: *mut c_void, out: *mut RveValue) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            context.error = Some("scene_current received a null output pointer".to_owned());
            return false;
        }
        let name = context.transaction.current_scene();
        let value = ParamValue::String(name);
        let references = message_object_refs(&value, context.transaction);
        context
            .save_values
            .push(OwnedAbiValue::new(&value, &references));
        let value = context
            .save_values
            .last()
            .expect("scene name value was just added");
        // SAFETY: `save_values` owns all nested buffers until this ABI call context is dropped.
        unsafe { *out = value.value };
        true
    })
}

pub(super) unsafe extern "C" fn host_app_quit(ctx: *mut c_void) -> bool {
    guarded(ctx, false, |context| {
        context.transaction.request_quit().is_ok()
    })
}
