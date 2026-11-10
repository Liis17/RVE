use crate::abi_host::read_message_value;
use crate::abi_string::{AbiStringError, read_abi_str};
use engine_scene::{AssetKind, ComponentKind, ParamKind, ParamSchema, ParamSpec, ParamValue};
use rve_sdk::abi::{
    RVE_PARAM_ARRAY, RVE_PARAM_ASSET, RVE_PARAM_BOOL, RVE_PARAM_COLOR, RVE_PARAM_COMPONENT,
    RVE_PARAM_ENUM, RVE_PARAM_FLOAT, RVE_PARAM_HAS_MAX, RVE_PARAM_HAS_MIN, RVE_PARAM_INT,
    RVE_PARAM_OBJECT, RVE_PARAM_STRING, RVE_PARAM_VEC3, RVE_SCRIPT_ABI_VERSION, RveModule,
    RveParamDesc, RveParamDescV13, RveScriptDesc, RveScriptDescV6, RveScriptDescV13,
    RveScriptDescV15, RveScriptDescV16, RveScriptDescV17, RveScriptDescV19, RveScriptDescV20,
    RveStr, RveUiEventFn,
};
use rve_sdk::abi::{RVE_SCRIPT_ABI_MIN_VERSION, RveScriptDescV1, RveScriptDescV3, RveScriptDescV4};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::mem;
use std::path::Path;
use std::sync::Arc;

pub(crate) type SharedLibrary = Arc<libloading::Library>;

/// A validated script library. Loaded modules stay mapped while any host uses them.
pub struct ScriptLibrary {
    scripts: HashMap<String, (&'static RveScriptDesc, Option<ParamSchema>)>,
    library: Option<SharedLibrary>,
    /// ABI версия заголовка модуля: от неё зависят callbacks, которые понимает гость.
    abi_version: u32,
}

/// Failure while opening or validating an ABI script module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptLibraryError {
    Open(String),
    MissingEntryPoint(String),
    NullModule,
    AbiVersion { min: u32, max: u32, found: u32 },
    InvalidHeaderSize { expected: usize, found: u32 },
    NullScripts,
    NullScriptName { index: usize },
    InvalidUtf8ScriptName { index: usize },
    InvalidScriptName { index: usize, name: String },
    DuplicateScriptName(String),
    ScriptCountOverflow,
    AmbiguousAbiV15DescriptorLayout,
    InvalidParams { script: String, message: String },
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
            Self::AbiVersion { min, max, found } => {
                write!(
                    f,
                    "script ABI version {found} is unsupported; supported range is {min}..={max}"
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
            Self::AmbiguousAbiV15DescriptorLayout => f.write_str(
                "script ABI v15 descriptor layout is ambiguous; rebuild the script module",
            ),
            Self::InvalidParams { script, message } => {
                write!(
                    f,
                    "script {script:?} declares invalid parameters: {message}"
                )
            }
        }
    }
}

impl Error for ScriptLibraryError {}

/// ABI v15 was independently used by the reference and UI branches. Their descriptors share a
/// prefix, but the UI branch appended `ui_event` to each descriptor. The second name distinguishes
/// their strides without reading a function pointer or constructing an invalid typed descriptor.
///
/// # Safety
/// `scripts` must point to at least two process-lifetime ABI v15 descriptors in one of those two
/// layouts. The candidate second names are read only from bytes inside the first two descriptors.
unsafe fn abi_v15_descriptor_stride(
    scripts: *const u8,
    count: usize,
) -> Result<usize, ScriptLibraryError> {
    let reference_stride = std::mem::size_of::<RveScriptDescV15>();
    let ui_stride = reference_stride + std::mem::size_of::<Option<RveUiEventFn>>();
    if count == 1 {
        return Ok(reference_stride);
    }

    // SAFETY: with at least two descriptors, both candidate RveStr values lie within the first
    // two entries for either historical stride. Reading RveStr itself does not dereference its
    // string pointer or impose validity on the following callbacks.
    let reference_name = unsafe {
        scripts
            .add(reference_stride)
            .cast::<RveStr>()
            .read_unaligned()
    };
    // SAFETY: same range argument as above; the UI stride starts the actual second descriptor in
    // the UI layout and remains within the first two entries in the reference layout.
    let ui_name = unsafe { scripts.add(ui_stride).cast::<RveStr>().read_unaligned() };
    let has_name_shape = |name: RveStr| !name.ptr.is_null() && (1..=64).contains(&name.len);
    match (has_name_shape(reference_name), has_name_shape(ui_name)) {
        (true, false) => Ok(reference_stride),
        (false, true) => Ok(ui_stride),
        _ => Err(ScriptLibraryError::AmbiguousAbiV15DescriptorLayout),
    }
}

impl ScriptLibrary {
    /// Opens and validates an ABI script library.
    ///
    /// The library is unloaded after this value and all script hosts using it are dropped.
    /// Loading executes native initialization code.
    ///
    /// # Safety
    /// The library and its initializers must be trusted. Its `rve_script_module` entry point must
    /// return a valid static table for the declared ABI, and every callback must obey its ABI
    /// contract and not unwind across the C boundary.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, ScriptLibraryError> {
        // SAFETY: loading is exposed as unsafe because it runs unknown native initialization code.
        let library = unsafe { libloading::Library::new(path.as_ref()) }
            .map_err(|error| ScriptLibraryError::Open(error.to_string()))?;
        // Copy the function pointer so the Symbol borrow ends before we call into the library.
        let entry_point = unsafe {
            library.get::<unsafe extern "C" fn() -> *const RveModule>(b"rve_script_module\0")
        }
        .map(|symbol| *symbol);
        let entry_point = entry_point
            .map_err(|error| ScriptLibraryError::MissingEntryPoint(error.to_string()))?;
        // SAFETY: part of this function's caller contract.
        let module = unsafe { entry_point() };
        if module.is_null() {
            return Err(ScriptLibraryError::NullModule);
        }
        // SAFETY: the entry point must return an immutable module valid while the library remains loaded.
        let mut scripts = unsafe { Self::from_module(module) }?;
        scripts.library = Some(Arc::new(library));
        Ok(scripts)
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
        if !(RVE_SCRIPT_ABI_MIN_VERSION..=RVE_SCRIPT_ABI_VERSION).contains(&header.abi_version) {
            return Err(ScriptLibraryError::AbiVersion {
                min: RVE_SCRIPT_ABI_MIN_VERSION,
                max: RVE_SCRIPT_ABI_VERSION,
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
        let descriptor_size = match header.abi_version {
            22.. => std::mem::size_of::<RveScriptDesc>(),
            20..=21 => std::mem::size_of::<RveScriptDescV20>(),
            19 => std::mem::size_of::<RveScriptDescV19>(),
            17..=18 => std::mem::size_of::<RveScriptDescV17>(),
            16 => std::mem::size_of::<RveScriptDescV16>(),
            15 => {
                std::mem::size_of::<RveScriptDescV15>()
                    + std::mem::size_of::<Option<RveUiEventFn>>()
            }
            13..=14 => std::mem::size_of::<RveScriptDescV13>(),
            6..=12 => std::mem::size_of::<RveScriptDescV6>(),
            4 | 5 => std::mem::size_of::<RveScriptDescV4>(),
            3 => std::mem::size_of::<RveScriptDescV3>(),
            _ => std::mem::size_of::<RveScriptDescV1>(),
        };
        if count > isize::MAX as usize / descriptor_size {
            return Err(ScriptLibraryError::ScriptCountOverflow);
        }
        if count > 0 && scripts_ptr.is_null() {
            return Err(ScriptLibraryError::NullScripts);
        }
        if count == 0 {
            return Ok(Self {
                scripts: HashMap::new(),
                library: None,
                abi_version: header.abi_version,
            });
        }
        let descriptors_v15 = if header.abi_version == 15 {
            // SAFETY: ABI v15's two historical branches used distinct descriptor strides.
            Some(unsafe { abi_v15_descriptor_stride(scripts_ptr.cast(), count) }?)
        } else {
            None
        };
        // SAFETY: the ABI version selects the descriptor layout and the caller guarantees the
        // matching immutable table remains readable for process lifetime.
        let descriptors_v22 = (header.abi_version >= 22)
            .then(|| unsafe { std::slice::from_raw_parts(scripts_ptr, count) });
        let descriptors_v20 = ((20..=21).contains(&header.abi_version)).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV20>(), count)
        });
        let descriptors_v19 = (header.abi_version == 19).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV19>(), count)
        });
        let descriptors_v17 = ((17..=18).contains(&header.abi_version)).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV17>(), count)
        });
        let descriptors_v16 = (header.abi_version == 16).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV16>(), count)
        });
        // SAFETY: versions 13 and 14 have the net event tail and pre-v15 parameter descriptors.
        let descriptors_v13 = ((13..=14).contains(&header.abi_version)).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV13>(), count)
        });
        // SAFETY: versions 6 through 12 share the descriptor layout without network callbacks.
        let descriptors_v6 = (6..=12).contains(&header.abi_version).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV6>(), count)
        });
        // SAFETY: versions 4 and 5 share the command-callback descriptor without collision.
        let descriptors_v4 = (4..=5).contains(&header.abi_version).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV4>(), count)
        });
        // SAFETY: version 3 has the parameter fields but no appended command callback.
        let descriptors_v3 = (header.abi_version == 3).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV3>(), count)
        });
        // SAFETY: versions 1 and 2 share the original descriptor layout.
        let descriptors_v1 = (header.abi_version < 3).then(|| unsafe {
            std::slice::from_raw_parts(scripts_ptr.cast::<RveScriptDescV1>(), count)
        });
        let mut scripts = HashMap::with_capacity(count);
        for index in 0..count {
            let descriptor = if let Some(descriptors) = descriptors_v22 {
                &descriptors[index]
            } else if let Some(descriptors) = descriptors_v20 {
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params,
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: descriptor.ui_event,
                    on_enable: descriptor.on_enable,
                    on_disable: descriptor.on_disable,
                    on_fixed_update: descriptor.on_fixed_update,
                    on_late_update: descriptor.on_late_update,
                    on_timer: descriptor.on_timer,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v19 {
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params,
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: descriptor.ui_event,
                    on_enable: descriptor.on_enable,
                    on_disable: descriptor.on_disable,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v17 {
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params,
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: descriptor.ui_event,
                    on_enable: descriptor.on_enable,
                    on_disable: descriptor.on_disable,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v16 {
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params,
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: descriptor.ui_event,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(stride) = descriptors_v15 {
                // SAFETY: the selected stride matches the module's ABI v15 branch; the module
                // contract guarantees every descriptor prefix is initialized and process-lived.
                let descriptor = unsafe {
                    &*scripts_ptr
                        .cast::<u8>()
                        .add(index * stride)
                        .cast::<RveScriptDescV15>()
                };
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params,
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: None,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v13 {
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params.cast(),
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: None,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v6 {
                // ABI v6 through v12 have no network callback; normalize it to `None`.
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params.cast(),
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: None,
                    ui_event: None,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v4 {
                // ABI v4 and v5 have no collision pointer; normalize it to `None`.
                let descriptor = &descriptors[index];
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params.cast(),
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: None,
                    net_event: None,
                    ui_event: None,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else if let Some(descriptors) = descriptors_v3 {
                let descriptor = &descriptors[index];
                // ABI v3 has no command pointer; normalize the stable prefix and schema.
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params.cast(),
                    param_count: descriptor.param_count,
                    command: None,
                    collision: None,
                    net_event: None,
                    ui_event: None,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            } else {
                let descriptor = &descriptors_v1.expect("legacy descriptor table")[index];
                // Older ABI versions have no parameter fields. Normalize their stable callback
                // prefix so instances can keep using the same descriptor representation.
                Box::leak(Box::new(RveScriptDesc {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: std::ptr::null(),
                    param_count: 0,
                    command: None,
                    collision: None,
                    net_event: None,
                    ui_event: None,
                    on_enable: None,
                    on_disable: None,
                    on_fixed_update: None,
                    on_late_update: None,
                    on_timer: None,
                    set_property: None,
                }))
            };
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
            let schema = if header.abi_version >= 16 {
                // SAFETY: the caller guarantees the parameter table of every v3+ descriptor.
                unsafe { read_schema(descriptor) }.map_err(|message| {
                    ScriptLibraryError::InvalidParams {
                        script: name.clone(),
                        message,
                    }
                })?
            } else if header.abi_version == 15 {
                // ABI v15 was used by both the typed-reference and UI branches. Its parameter
                // table layout is selected from the shared prefix without reading either tail.
                unsafe { read_schema_v15(descriptor) }.map_err(|message| {
                    ScriptLibraryError::InvalidParams {
                        script: name.clone(),
                        message,
                    }
                })?
            } else if header.abi_version >= 3 {
                // SAFETY: versions 3 through 14 use the old parameter descriptor layout.
                unsafe { read_schema_v13(descriptor) }.map_err(|message| {
                    ScriptLibraryError::InvalidParams {
                        script: name.clone(),
                        message,
                    }
                })?
            } else {
                None
            };
            if scripts.insert(name.clone(), (descriptor, schema)).is_some() {
                return Err(ScriptLibraryError::DuplicateScriptName(name));
            }
        }
        Ok(Self {
            scripts,
            library: None,
            abi_version: header.abi_version,
        })
    }

    pub(crate) fn script(&self, name: &str) -> Option<&'static RveScriptDesc> {
        self.scripts.get(name).map(|(descriptor, _)| *descriptor)
    }

    pub(crate) fn abi_version(&self) -> u32 {
        self.abi_version
    }

    pub(crate) fn shared_library(&self) -> Option<SharedLibrary> {
        self.library.as_ref().map(Arc::clone)
    }

    /// Registered script names in ascending order.
    pub fn script_names(&self) -> Vec<&str> {
        let mut names: Vec<_> = self.scripts.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Parameter schema of a registered script; `None` for an unknown name or a script that
    /// declares no parameters.
    pub fn schema(&self, name: &str) -> Option<&ParamSchema> {
        self.scripts
            .get(name)
            .and_then(|(_, schema)| schema.as_ref())
    }

    /// Returns whether this library registers a script with the given scene name.
    pub fn contains_script(&self, name: &str) -> bool {
        self.scripts.contains_key(name)
    }
}

pub(crate) fn valid_heavy_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Reads and validates the parameter table of one descriptor. An empty table declares no schema.
///
/// # Safety
/// For a nonzero `param_count`, `params` must point to that many initialized process-lifetime
/// descriptors whose strings and default values are readable.
unsafe fn read_schema(descriptor: &RveScriptDesc) -> Result<Option<ParamSchema>, String> {
    let count = descriptor.param_count;
    if count == 0 {
        return Ok(None);
    }
    if descriptor.params.is_null() {
        return Err("null parameter table".to_owned());
    }
    if count > isize::MAX as usize / mem::size_of::<RveParamDesc>() {
        return Err("parameter table size overflows address space".to_owned());
    }
    // SAFETY: guaranteed by this function's contract and checked above.
    let params = unsafe { std::slice::from_raw_parts(descriptor.params, count) };
    read_schema_params(params.iter().map(|param| {
        (
            param.name,
            param.kind,
            param.default,
            param.min,
            param.max,
            param.flags,
            param.label,
            param.argument,
        )
    }))
}

/// Reads a v3-v14 descriptor without accessing the v15 argument tail.
unsafe fn read_schema_v13(descriptor: &RveScriptDesc) -> Result<Option<ParamSchema>, String> {
    let count = descriptor.param_count;
    if count == 0 {
        return Ok(None);
    }
    if descriptor.params.is_null() {
        return Err("null parameter table".to_owned());
    }
    if count > isize::MAX as usize / mem::size_of::<RveParamDescV13>() {
        return Err("parameter table size overflows address space".to_owned());
    }
    // SAFETY: the old ABI stores the prefix-compatible v13 descriptors at this pointer.
    let params =
        unsafe { std::slice::from_raw_parts(descriptor.params.cast::<RveParamDescV13>(), count) };
    read_schema_params(params.iter().map(|param| {
        (
            param.name,
            param.kind,
            param.default,
            param.min,
            param.max,
            param.flags,
            param.label,
            rve_sdk::abi::RveStr::empty(),
        )
    }))
}

/// ABI v15 was independently assigned to two layouts. The reference branch appended `argument`
/// to each parameter, while the UI branch kept the v13 parameter layout. New kinds identify the
/// reference layout directly; for old kinds, the next name length and the appended empty argument
/// distinguish arrays with multiple parameters. A single old-kind parameter has the same readable
/// prefix in both layouts and does not need `argument`.
///
/// # Safety
/// The caller guarantees that the module uses one of those ABI v15 layouts and that its declared
/// parameter table is readable for the selected layout.
unsafe fn read_schema_v15(descriptor: &RveScriptDesc) -> Result<Option<ParamSchema>, String> {
    let count = descriptor.param_count;
    if count == 0 {
        return Ok(None);
    }
    if descriptor.params.is_null() {
        return Err("null parameter table".to_owned());
    }

    // SAFETY: both ABI v15 parameter descriptors share this prefix.
    let first = unsafe { descriptor.params.cast::<RveParamDescV13>().read() };
    if matches!(
        first.kind,
        RVE_PARAM_OBJECT
            | RVE_PARAM_COMPONENT
            | RVE_PARAM_ASSET
            | RVE_PARAM_ENUM
            | RVE_PARAM_COLOR
            | RVE_PARAM_ARRAY
    ) {
        // SAFETY: these kinds only exist in the reference layout, which appended `argument`.
        return unsafe { read_schema(descriptor) };
    }
    if first.kind > RVE_PARAM_VEC3 {
        return unsafe { read_schema_v13(descriptor) };
    }
    if count == 1 {
        // SAFETY: the old descriptor prefix contains all fields required by these kinds.
        return unsafe { read_schema_v13(descriptor) };
    }

    // SAFETY: with two or more parameters this word is either the first reference-layout
    // argument length or the second legacy parameter's name length, both within their tables.
    let next_len = unsafe {
        descriptor
            .params
            .cast::<u8>()
            .add(
                std::mem::size_of::<RveParamDescV13>()
                    + std::mem::offset_of!(rve_sdk::abi::RveStr, len),
            )
            .cast::<usize>()
            .read()
    };
    if next_len == 0 {
        // SAFETY: the reference layout provides a complete typed table.
        unsafe { read_schema(descriptor) }
    } else if next_len <= 64 {
        // SAFETY: the legacy layout provides a complete v13 parameter table.
        unsafe { read_schema_v13(descriptor) }
    } else {
        Err("invalid ABI v15 parameter layout".to_owned())
    }
}

fn read_schema_params(
    params: impl Iterator<
        Item = (
            rve_sdk::abi::RveStr,
            u32,
            rve_sdk::abi::RveValue,
            f64,
            f64,
            u32,
            rve_sdk::abi::RveStr,
            rve_sdk::abi::RveStr,
        ),
    >,
) -> Result<Option<ParamSchema>, String> {
    let mut specs = Vec::new();
    for (index, (name, kind_code, default_value, min, max, flags, label, argument)) in
        params.enumerate()
    {
        // SAFETY: the caller guarantees readable process-lifetime strings.
        let name = unsafe { read_abi_str(name) }
            .map_err(|error| format!("parameter {index} has an invalid name: {error:?}"))?
            .to_owned();
        // SAFETY: v15 descriptors carry a process-lifetime argument; old descriptors provide empty.
        let argument = unsafe { read_abi_str(argument) }
            .map_err(|error| format!("parameter {name:?} has an invalid argument: {error:?}"))?;
        let kind = abi_param_kind(kind_code, argument)
            .ok_or_else(|| format!("parameter {name:?} has unknown or invalid kind {kind_code}"))?;
        if flags & !(RVE_PARAM_HAS_MIN | RVE_PARAM_HAS_MAX) != 0 {
            return Err(format!("parameter {name:?} has unknown flags {flags:#x}"));
        }
        let default = if matches!(kind, ParamKind::Object | ParamKind::Component(_))
            && default_value.tag == rve_sdk::abi::RVE_VALUE_OBJECT
            && unsafe { default_value.data.object } == 0
        {
            ParamValue::Object(String::new())
        } else {
            // SAFETY: the caller guarantees readable default values.
            unsafe { read_message_value(&default_value) }
                .map_err(|error| format!("parameter {name:?} has an invalid default: {error}"))?
                .0
        };
        // SAFETY: the caller guarantees readable process-lifetime strings.
        let label = match unsafe { read_abi_str(label) } {
            Ok("") | Err(AbiStringError::Null) => None,
            Ok(label) => Some(label.to_owned()),
            Err(error) => {
                return Err(format!(
                    "parameter {name:?} has an invalid label: {error:?}"
                ));
            }
        };
        specs.push(ParamSpec {
            min: (flags & RVE_PARAM_HAS_MIN != 0).then_some(min),
            max: (flags & RVE_PARAM_HAS_MAX != 0).then_some(max),
            name,
            kind,
            default,
            label,
        });
    }
    ParamSchema::new(specs).map(Some)
}

fn abi_param_kind(kind: u32, argument: &str) -> Option<ParamKind> {
    Some(match kind {
        RVE_PARAM_BOOL => ParamKind::Bool,
        RVE_PARAM_INT => ParamKind::Int,
        RVE_PARAM_FLOAT => ParamKind::Float,
        RVE_PARAM_STRING => ParamKind::String,
        RVE_PARAM_VEC3 => ParamKind::Vec3,
        RVE_PARAM_OBJECT => ParamKind::Object,
        RVE_PARAM_COMPONENT => ParamKind::Component(ComponentKind::parse(argument)?),
        RVE_PARAM_ASSET => ParamKind::Asset(if argument.is_empty() {
            None
        } else {
            Some(AssetKind::parse(argument)?)
        }),
        RVE_PARAM_ENUM => ParamKind::Enum(
            argument
                .split('\0')
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
        RVE_PARAM_COLOR => ParamKind::Color,
        RVE_PARAM_ARRAY => ParamKind::Array(Box::new(ParamKind::parse(argument)?)),
        _ => return None,
    })
}

#[cfg(test)]
mod abi_compat_tests {
    use super::ScriptLibrary;
    use rve_sdk::abi::{RVE_SCRIPT_ABI_VERSION, RveHeader, RveModule, RveScriptDescV20};
    use rve_sdk::{Params, Script};

    struct Probe;

    impl Script for Probe {}

    fn probe(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Probe))
    }

    fn second_probe(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(Probe))
    }

    #[test]
    fn abi20_and_abi21_descriptor_tables_use_the_historical_stride_on_v23_host() {
        assert_eq!(RVE_SCRIPT_ABI_VERSION, 23);
        let current = rve_sdk::script_module! {
            "Probe" => probe,
            "SecondProbe" => second_probe,
        };
        // SAFETY: the SDK macro owns an immutable process-lifetime descriptor table.
        let descriptors =
            unsafe { std::slice::from_raw_parts(current.scripts, current.script_count) };
        let legacy: &'static [RveScriptDescV20] = Box::leak(
            descriptors
                .iter()
                .map(|descriptor| RveScriptDescV20 {
                    name: descriptor.name,
                    create: descriptor.create,
                    start: descriptor.start,
                    update: descriptor.update,
                    message: descriptor.message,
                    destroy: descriptor.destroy,
                    drop: descriptor.drop,
                    params: descriptor.params,
                    param_count: descriptor.param_count,
                    command: descriptor.command,
                    collision: descriptor.collision,
                    net_event: descriptor.net_event,
                    ui_event: descriptor.ui_event,
                    on_enable: descriptor.on_enable,
                    on_disable: descriptor.on_disable,
                    on_fixed_update: descriptor.on_fixed_update,
                    on_late_update: descriptor.on_late_update,
                    on_timer: descriptor.on_timer,
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        for abi_version in [20, 21] {
            let legacy = Box::leak(Box::new(RveModule {
                header: RveHeader {
                    abi_version,
                    header_size: std::mem::size_of::<RveHeader>() as u32,
                },
                scripts: legacy.as_ptr().cast(),
                script_count: legacy.len(),
            }));
            // SAFETY: the module and exact-sized historical descriptor table are process-lifetime values.
            let library =
                unsafe { ScriptLibrary::from_module(std::ptr::from_ref(legacy)) }.unwrap();
            assert_eq!(library.abi_version(), abi_version);
            assert!(library.script("Probe").is_some());
            assert!(library.script("SecondProbe").is_some());
            assert!(library.schema("Probe").is_none());
        }
    }

    #[test]
    fn abi22_guest_descriptor_layout_loads_on_v23_host() {
        assert_eq!(RVE_SCRIPT_ABI_VERSION, 23);
        let current = rve_sdk::script_module! {
            "Probe" => probe,
            "SecondProbe" => second_probe,
        };
        let previous = Box::leak(Box::new(RveModule {
            header: RveHeader {
                abi_version: 22,
                header_size: std::mem::size_of::<RveHeader>() as u32,
            },
            // ABI v23 appends only a host callback, so the v22 descriptor layout is unchanged.
            scripts: current.scripts,
            script_count: current.script_count,
        }));
        // SAFETY: the module and descriptor table are immutable process-lifetime values.
        let library = unsafe { ScriptLibrary::from_module(std::ptr::from_ref(previous)) }.unwrap();
        assert_eq!(library.abi_version(), 22);
        assert!(library.script("Probe").is_some());
        assert!(library.script("SecondProbe").is_some());
    }
}

#[cfg(all(test, windows))]
mod ownership_tests {
    #[test]
    fn host_retains_module_until_it_is_dropped() {
        use super::*;
        let module = Arc::new(unsafe { libloading::Library::new("kernel32.dll") }.unwrap());
        let weak = Arc::downgrade(&module);
        let library = ScriptLibrary {
            scripts: HashMap::new(),
            library: Some(module),
            abi_version: 0,
        };
        let scene = engine_scene::SceneDocument::parse(
            "empty",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let world = engine_scene::World::from_document(&scene).unwrap();
        let host = crate::ScriptHost::load(
            &world,
            "empty",
            &std::collections::BTreeMap::new(),
            Some(&library),
        )
        .unwrap();
        drop(library);
        assert!(weak.upgrade().is_some());
        drop(host);
        assert!(weak.upgrade().is_none());
    }
}
