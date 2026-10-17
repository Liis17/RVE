use rve_sdk::abi::RveStr;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AbiStringError {
    Null,
    InvalidUtf8,
}

/// Reads borrowed ABI bytes as UTF-8 after rejecting a null, nonempty string.
///
/// # Safety
/// For nonzero lengths, `value.ptr` must refer to initialized readable storage for `value.len`
/// bytes for the returned lifetime.
pub(crate) unsafe fn read_abi_str<'a>(value: RveStr) -> Result<&'a str, AbiStringError> {
    if value.len > 0 && value.ptr.is_null() {
        return Err(AbiStringError::Null);
    }
    // SAFETY: the caller guarantees that non-null ABI storage remains readable for this call.
    unsafe { value.to_str() }.map_err(|_| AbiStringError::InvalidUtf8)
}
