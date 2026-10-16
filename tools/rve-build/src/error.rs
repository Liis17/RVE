use std::fmt;

#[derive(Debug)]
pub(crate) struct BuildError {
    exit_code: i32,
    message: String,
}

impl BuildError {
    pub(crate) fn input(message: impl Into<String>) -> Self {
        Self {
            exit_code: 2,
            message: message.into(),
        }
    }

    pub(crate) fn operational(message: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            message: message.into(),
        }
    }

    pub(crate) fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BuildError {}
