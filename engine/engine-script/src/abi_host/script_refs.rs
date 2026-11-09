//! ABI-функции справочника экземпляров: `object_script` и методы найденного экземпляра.

use super::*;
use crate::script_refs::normalize_script_key;

pub(super) unsafe extern "C" fn host_object_script(
    ctx: *mut c_void,
    object: u64,
    key: RveStr,
    out: *mut u64,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            context.error = Some("object_script received a null output pointer".to_owned());
            return false;
        }
        let key = match unsafe { read_abi_str(key) } {
            Ok(key) => normalize_script_key(key),
            Err(error) => {
                context.error = Some(format!("script key is invalid: {error:?}"));
                return false;
            }
        };
        let Some(handle) = context.transaction.object_from_abi_handle(object) else {
            return false;
        };
        let Some(serial) = context.transaction.script_ref(handle, &key) else {
            return false;
        };
        // SAFETY: the output pointer was checked above and points to the guest's stack slot.
        unsafe { *out = serial };
        true
    })
}

pub(super) unsafe extern "C" fn host_script_ref_get(
    ctx: *mut c_void,
    script: u64,
    name: RveStr,
    out: *mut RveValue,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            context.error = Some("script_ref_get received a null output pointer".to_owned());
            return false;
        }
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name.to_owned(),
            Err(error) => {
                context.error = Some(format!("property name is invalid: {error:?}"));
                return false;
            }
        };
        let value = match context.transaction.script_get(script, &name) {
            Ok(value) => value,
            Err(_) => {
                return false;
            }
        };
        let references = message_object_refs(&value, context.transaction);
        context
            .save_values
            .push(OwnedAbiValue::new(&value, &references));
        let value = context
            .save_values
            .last()
            .expect("script property value was just added");
        // SAFETY: `save_values` owns all nested buffers until this ABI call context is dropped.
        unsafe { *out = value.value };
        true
    })
}

/// Декодирует значение гостя; ссылки на объекты разрешаются по живым хендлам транзакции.
fn decode_guest_value(
    context: &mut AbiCallContext<'_>,
    value: *const RveValue,
) -> Result<ParamValue, String> {
    // SAFETY: the guest passes a value that lives for the duration of this synchronous callback.
    unsafe {
        read_message_value_with_objects(value, |handle| {
            match context.transaction.abi_object(handle) {
                AbiObject::Alive(object) => {
                    context.transaction.object_id(object).map(str::to_owned)
                }
                AbiObject::Malformed | AbiObject::Unavailable => None,
            }
        })
    }
    .map(|(value, _)| value)
}

pub(super) unsafe extern "C" fn host_script_ref_set(
    ctx: *mut c_void,
    script: u64,
    name: RveStr,
    value: *const RveValue,
) -> bool {
    guarded(ctx, false, |context| {
        let name = match unsafe { read_abi_str(name) } {
            Ok(name) => name.to_owned(),
            Err(error) => {
                context.error = Some(format!("property name is invalid: {error:?}"));
                return false;
            }
        };
        let value = match decode_guest_value(context, value) {
            Ok(value) => value,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        match context.transaction.script_set(script, &name, &value) {
            Ok(()) => true,
            Err(_) => false,
        }
    })
}

pub(super) unsafe extern "C" fn host_script_ref_send(
    ctx: *mut c_void,
    script: u64,
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
            Ok(name) => name.to_owned(),
            Err(error) => {
                context.error = Some(format!("invalid message name: {error:?}"));
                return false;
            }
        };
        // SAFETY: `value` is provided by the guest for the duration of this ABI callback.
        let (value, value_size) = match unsafe {
            read_message_value_with_objects(value, |handle| {
                ObjectHandle::from_abi(handle)
                    .and_then(|handle| context.transaction.object_id(handle).map(str::to_owned))
            })
        } {
            Ok(value) => value,
            Err(error) => {
                context.error = Some(error);
                return false;
            }
        };
        match context
            .transaction
            .script_send(script, &name, value, value_size)
        {
            Ok(()) => true,
            Err(error) => {
                context.error = Some(format!("script send failed: {error:?}"));
                false
            }
        }
    })
}

pub(super) unsafe extern "C" fn host_script_ref_enabled(
    ctx: *mut c_void,
    script: u64,
    out: *mut bool,
) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return false;
        }
        match context.transaction.script_enabled_of(script) {
            Ok(enabled) => {
                // SAFETY: output was checked and points to the caller's stack slot.
                unsafe { *out = enabled };
                true
            }
            Err(_) => false,
        }
    })
}

pub(super) unsafe extern "C" fn host_script_ref_set_enabled(
    ctx: *mut c_void,
    script: u64,
    enabled: bool,
) -> bool {
    guarded(ctx, false, |context| {
        match context.transaction.set_script_enabled_of(script, enabled) {
            Ok(()) => true,
            Err(_) => false,
        }
    })
}
