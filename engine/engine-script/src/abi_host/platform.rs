//! ABI-функции окна и системных диалогов (ABI v24). Отказ ставит ошибку вызова, как исключение Lua.

use super::*;
use crate::platform::{FileDialogFilter, FileDialogKind, FileDialogRequest};
use rve_sdk::abi::{RveFileDialog, RveFileDialogFilter};
use std::path::PathBuf;

fn reject(context: &mut AbiCallContext<'_>, error: impl Into<String>) -> bool {
    context.error = Some(error.into());
    false
}

pub(super) unsafe extern "C" fn host_window_fullscreen(ctx: *mut c_void, out: *mut bool) -> bool {
    guarded(ctx, false, |context| {
        if out.is_null() {
            return reject(context, "window_fullscreen received a null output pointer");
        }
        // SAFETY: `out` was checked and is writable for this synchronous callback.
        unsafe { *out = context.transaction.window_fullscreen() };
        true
    })
}

pub(super) unsafe extern "C" fn host_window_set_fullscreen(
    ctx: *mut c_void,
    fullscreen: bool,
) -> bool {
    guarded(ctx, false, |context| {
        match context.transaction.request_fullscreen(fullscreen) {
            Ok(()) => true,
            Err(error) => reject(context, error),
        }
    })
}

pub(super) unsafe extern "C" fn host_window_screenshot(ctx: *mut c_void, path: RveStr) -> bool {
    guarded(ctx, false, |context| {
        let path = match unsafe { read_abi_str(path) } {
            Ok(path) => (!path.is_empty()).then(|| PathBuf::from(path)),
            Err(error) => return reject(context, format!("screenshot path is invalid: {error:?}")),
        };
        match context.transaction.request_screenshot(path) {
            Ok(()) => true,
            Err(error) => reject(context, error),
        }
    })
}

/// Непустая строка ABI или `None`.
unsafe fn optional_text(value: RveStr, what: &str) -> Result<Option<String>, String> {
    match unsafe { read_abi_str(value) } {
        Ok("") => Ok(None),
        Ok(text) => Ok(Some(text.to_owned())),
        Err(error) => Err(format!("file dialog {what} is invalid: {error:?}")),
    }
}

/// Копирует параметры диалога из памяти гостя.
///
/// # Safety
/// Указатели и длины `dialog` должны быть читаемы на время вызова.
unsafe fn read_dialog(dialog: &RveFileDialog) -> Result<FileDialogRequest, String> {
    let kind = FileDialogKind::from_code(dialog.kind)
        .ok_or_else(|| format!("unknown file dialog kind {}", dialog.kind))?;
    if dialog.filter_count > 0 && dialog.filters.is_null() {
        return Err("file dialog filters pointer is null".to_owned());
    }
    let raw_filters: &[RveFileDialogFilter] = if dialog.filter_count == 0 {
        &[]
    } else {
        // SAFETY: the pointer is non-null and the guest guarantees `filter_count` entries.
        unsafe { std::slice::from_raw_parts(dialog.filters, dialog.filter_count) }
    };
    let mut filters = Vec::with_capacity(raw_filters.len());
    for filter in raw_filters {
        if filter.extension_count > 0 && filter.extensions.is_null() {
            return Err("file dialog filter extensions pointer is null".to_owned());
        }
        let raw_extensions: &[RveStr] = if filter.extension_count == 0 {
            &[]
        } else {
            // SAFETY: the pointer is non-null and the guest guarantees `extension_count` entries.
            unsafe { std::slice::from_raw_parts(filter.extensions, filter.extension_count) }
        };
        let name = unsafe { optional_text(filter.name, "filter name") }?.unwrap_or_default();
        let extensions = raw_extensions
            .iter()
            .map(|&extension| {
                unsafe { optional_text(extension, "filter extension") }
                    .map(Option::unwrap_or_default)
            })
            .collect::<Result<Vec<_>, _>>()?;
        filters.push(FileDialogFilter { name, extensions });
    }
    Ok(FileDialogRequest {
        kind,
        title: unsafe { optional_text(dialog.title, "title") }?,
        directory: unsafe { optional_text(dialog.directory, "directory") }?.map(PathBuf::from),
        file_name: unsafe { optional_text(dialog.file_name, "file name") }?,
        filters,
    })
}

pub(super) unsafe extern "C" fn host_file_dialog_open(
    ctx: *mut c_void,
    dialog: *const RveFileDialog,
    out: *mut u64,
) -> bool {
    guarded(ctx, false, |context| {
        if dialog.is_null() || out.is_null() {
            return reject(context, "file_dialog_open received a null pointer");
        }
        // SAFETY: `dialog` was checked and the guest keeps its buffers alive for this call.
        let request = match unsafe { read_dialog(&*dialog) } {
            Ok(request) => request,
            Err(error) => return reject(context, error),
        };
        match context.transaction.open_file_dialog(request) {
            Ok(id) => {
                // SAFETY: `out` was checked and is writable for this synchronous callback.
                unsafe { *out = id };
                true
            }
            Err(error) => reject(context, error),
        }
    })
}

pub(super) unsafe extern "C" fn host_file_dialog_poll(
    ctx: *mut c_void,
    id: u64,
    paths: *mut RveStr,
    capacity: usize,
    count: *mut usize,
    done: *mut bool,
) -> bool {
    guarded(ctx, false, |context| {
        if count.is_null() || done.is_null() || capacity > 0 && paths.is_null() {
            return reject(context, "file_dialog_poll received a null buffer");
        }
        let result = match context.transaction.poll_file_dialog(id) {
            Ok(result) => result,
            Err(error) => return reject(context, error),
        };
        context.dialog_paths = result
            .iter()
            .flatten()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        // SAFETY: `count` and `done` were checked and are writable for this callback.
        unsafe {
            *done = result.is_some();
            *count = context.dialog_paths.len();
        }
        for (index, path) in context.dialog_paths.iter().take(capacity).enumerate() {
            // SAFETY: capacity and the non-null array were validated above.
            unsafe { *paths.add(index) = RveStr::from_str_ref(path) };
        }
        true
    })
}
