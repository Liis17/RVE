use crate::abi_string::{AbiStringError, read_abi_str};
use crate::heavy::valid_heavy_name;
use rve_sdk::abi::{RVE_SCRIPT_ABI_VERSION, RveModule, RveScriptDesc};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::mem;
use std::path::Path;

/// A script library that has been validated and deliberately kept loaded for process lifetime.
pub struct ScriptLibrary {
    scripts: HashMap<String, &'static RveScriptDesc>,
}

/// Failure while opening or validating an ABI script module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptLibraryError {
    Open(String),
    MissingEntryPoint(String),
    NullModule,
    AbiVersion { expected: u32, found: u32 },
    InvalidHeaderSize { expected: usize, found: u32 },
    NullScripts,
    NullScriptName { index: usize },
    InvalidUtf8ScriptName { index: usize },
    InvalidScriptName { index: usize, name: String },
    DuplicateScriptName(String),
    ScriptCountOverflow,
}

impl fmt::Display for ScriptLibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(message) => write!(f, "could not open script library: {message}"),
            Self::MissingEntryPoint(message) => {
                write!(
                    f,
                    "script library has no rve_script_module entry point: {message}"
                )
            }
            Self::NullModule => f.write_str("script library returned a null module pointer"),
            Self::AbiVersion { expected, found } => {
                write!(
                    f,
                    "script ABI version {found} is unsupported; expected {expected}"
                )
            }
            Self::InvalidHeaderSize { expected, found } => {
                write!(f, "script ABI header size is {found}; expected {expected}")
            }
            Self::NullScripts => f.write_str("script module has a null script table"),
            Self::NullScriptName { index } => {
                write!(f, "script at table index {index} has a null name pointer")
            }
            Self::InvalidUtf8ScriptName { index } => {
                write!(
                    f,
                    "script at table index {index} has a name that is not UTF-8"
                )
            }
            Self::InvalidScriptName { index, name } => write!(
                f,
                "script at table index {index} has invalid name {name:?}; expected 1-64 ASCII letters, digits, or underscores"
            ),
            Self::DuplicateScriptName(name) => {
                write!(f, "script module contains duplicate name {name:?}")
            }
            Self::ScriptCountOverflow => f.write_str("script table size overflows address space"),
        }
    }
}

impl Error for ScriptLibraryError {}

impl ScriptLibrary {
    /// Opens and validates an ABI script library.
    ///
    /// A successfully opened library is intentionally never unloaded, including if symbol
    /// lookup or ABI validation later fails. Loading executes native initialization code.
    ///
    /// # Safety
    /// The library and its initializers must be trusted. Its `rve_script_module` entry point must
    /// return a valid static table for the declared ABI, and every callback must obey its ABI
    /// contract and not unwind across the C boundary.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, ScriptLibraryError> {
        // SAFETY: loading is exposed as unsafe because it runs unknown native initialization code.
        let library = unsafe { libloading::Library::new(path.as_ref()) }
            .map_err(|error| ScriptLibraryError::Open(error.to_string()))?;
        // Copy the function pointer so the Symbol borrow ends before the library is leaked.
        let entry_point = unsafe {
            library.get::<unsafe extern "C" fn() -> *const RveModule>(b"rve_script_module\0")
        }
        .map(|symbol| *symbol);
        mem::forget(library);
        let entry_point = entry_point
            .map_err(|error| ScriptLibraryError::MissingEntryPoint(error.to_string()))?;
        // SAFETY: part of this function's caller contract.
        let module = unsafe { entry_point() };
        if module.is_null() {
            return Err(ScriptLibraryError::NullModule);
        }
        // SAFETY: the entry point must return a process-lifetime module per the caller contract.
        unsafe { Self::from_module(module) }
    }

    /// Validates a process-lifetime module. Primarily useful for in-process integrations and tests.
    ///
    /// # Safety
    /// `module` may be null, in which case this returns [`ScriptLibraryError::NullModule`]. For a
    /// non-null pointer, it must be aligned and point to a readable header. If the ABI version and
    /// header size match, the remaining module fields and the aligned script table must be
    /// readable, initialized, immutable, and valid for process lifetime. Each name pointer must
    /// refer to readable process-lifetime bytes unless it is null.
    pub unsafe fn from_module(module: *const RveModule) -> Result<Self, ScriptLibraryError> {
        if module.is_null() {
            return Err(ScriptLibraryError::NullModule);
        }
        // Read only the header before validating its version and size. This avoids forming a
        // reference to the full module while the host does not yet know which layout it contains.
        let header = unsafe { std::ptr::addr_of!((*module).header).read() };
        if header.abi_version != RVE_SCRIPT_ABI_VERSION {
            return Err(ScriptLibraryError::AbiVersion {
                expected: RVE_SCRIPT_ABI_VERSION,
                found: header.abi_version,
            });
        }
        let expected_header_size = std::mem::size_of::<rve_sdk::abi::RveHeader>();
        if header.header_size as usize != expected_header_size {
            return Err(ScriptLibraryError::InvalidHeaderSize {
                expected: expected_header_size,
                found: header.header_size,
            });
        }

        // SAFETY: version and header size were checked, and the caller guarantees the full module
        // layout is readable after those checks.
        let scripts_ptr = unsafe { std::ptr::addr_of!((*module).scripts).read() };
        // SAFETY: see the `scripts_ptr` read above.
        let count = unsafe { std::ptr::addr_of!((*module).script_count).read() };
        if count > isize::MAX as usize / std::mem::size_of::<RveScriptDesc>() {
            return Err(ScriptLibraryError::ScriptCountOverflow);
        }
        if count > 0 && scripts_ptr.is_null() {
            return Err(ScriptLibraryError::NullScripts);
        }
        if count == 0 {
            return Ok(Self {
                scripts: HashMap::new(),
            });
        }
        // SAFETY: guaranteed by this method's caller contract and checked non-null/count above.
        let descriptors: &'static [RveScriptDesc] =
            unsafe { std::slice::from_raw_parts(scripts_ptr, count) };
        let mut scripts = HashMap::with_capacity(count);
        for (index, descriptor) in descriptors.iter().enumerate() {
            // SAFETY: the caller guarantees process-lifetime storage for every non-null name.
            let name = unsafe { read_abi_str(descriptor.name) }.map_err(|error| match error {
                AbiStringError::Null => ScriptLibraryError::NullScriptName { index },
                AbiStringError::InvalidUtf8 => ScriptLibraryError::InvalidUtf8ScriptName { index },
            })?;
            if !valid_heavy_name(name) {
                return Err(ScriptLibraryError::InvalidScriptName {
                    index,
                    name: name.to_owned(),
                });
            }
            let name = name.to_owned();
            if scripts.insert(name.clone(), descriptor).is_some() {
                return Err(ScriptLibraryError::DuplicateScriptName(name));
            }
        }
        Ok(Self { scripts })
    }

    pub(crate) fn script(&self, name: &str) -> Option<&'static RveScriptDesc> {
        self.scripts.get(name).copied()
    }
}
