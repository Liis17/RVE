use engine_scene::ParamValue;
use std::collections::BTreeMap;
use std::fmt;

/// Script parameters in deterministic key order.
pub type ScriptParams = BTreeMap<String, ParamValue>;

/// Time exposed to scripts for one lifecycle phase.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScriptTime {
    pub elapsed: f64,
    pub frame: u64,
}

/// Local object transform exposed to scripts. Rotation uses degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScriptTransform {
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: [f32; 3],
}

impl ScriptTransform {
    pub(crate) fn is_finite(self) -> bool {
        self.position
            .into_iter()
            .chain(self.rotation)
            .chain(self.scale)
            .all(f32::is_finite)
    }
}

/// Context passed to a heavy script callback.
pub struct ScriptContext<'a> {
    pub object_id: &'a str,
    pub object_name: &'a str,
    pub transform: &'a mut ScriptTransform,
    pub time: ScriptTime,
    pub(crate) logger: &'a mut dyn FnMut(String),
}

impl ScriptContext<'_> {
    /// Appends an informational event to the owning ScriptHost.
    pub fn log(&mut self, message: impl Into<String>) {
        (self.logger)(message.into());
    }
}

/// Severity for buffered script events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptEventLevel {
    Info,
    Error,
}

/// Buffered log or runtime-error event; scripts never print through the host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptEvent {
    pub level: ScriptEventLevel,
    pub source: String,
    pub message: String,
}

impl fmt::Display for ScriptEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.source, self.message)
    }
}
