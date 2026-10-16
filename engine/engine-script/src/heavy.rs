use crate::types::{ScriptContext, ScriptParams};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;

/// Rust script callback factory registered by a game crate.
pub type HeavyFactory = fn(&ScriptParams) -> Result<Box<dyn HeavyScript>, String>;

/// Lifecycle implemented by statically linked Rust scripts.
pub trait HeavyScript: Send {
    fn on_start(&mut self, _ctx: &mut ScriptContext<'_>) {}
    fn on_update(&mut self, _ctx: &mut ScriptContext<'_>, _dt: f32) {}
    fn on_destroy(&mut self, _ctx: &mut ScriptContext<'_>) {}
}

/// Name validation or duplicate-registration failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeavyRegistrationError {
    InvalidName(String),
    DuplicateName(String),
}

impl fmt::Display for HeavyRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "invalid heavy script name {name:?}; expected 1-64 ASCII letters, digits, or underscores"
            ),
            Self::DuplicateName(name) => {
                write!(f, "heavy script {name:?} is already registered")
            }
        }
    }
}

impl Error for HeavyRegistrationError {}

/// Factories for heavy scripts supplied by a game crate.
#[derive(Default)]
pub struct HeavyRegistry {
    factories: HashMap<String, HeavyFactory>,
}

impl HeavyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a script factory. Duplicate and invalid names return errors.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        factory: HeavyFactory,
    ) -> Result<(), HeavyRegistrationError> {
        let name = name.into();
        if !valid_heavy_name(&name) {
            return Err(HeavyRegistrationError::InvalidName(name));
        }
        if self.factories.contains_key(&name) {
            return Err(HeavyRegistrationError::DuplicateName(name));
        }
        self.factories.insert(name, factory);
        Ok(())
    }

    pub(crate) fn factory(&self, name: &str) -> Option<HeavyFactory> {
        self.factories.get(name).copied()
    }
}

pub(crate) fn valid_heavy_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
