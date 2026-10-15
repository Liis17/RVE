use std::fmt;
use std::str::FromStr;

/// A canonical, case-sensitive path relative to an asset namespace root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VirtualPath(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VirtualPathError {
    input: String,
}

impl VirtualPath {
    /// Parses an optional-leading-slash path and stores it without that slash.
    pub fn parse(input: &str) -> Result<Self, VirtualPathError> {
        let canonical = input.strip_prefix('/').unwrap_or(input);
        if canonical.is_empty()
            || canonical.split('/').any(|component| {
                component.is_empty()
                    || component == "."
                    || component == ".."
                    || component.contains(['\\', ':', '\0'])
            })
        {
            return Err(VirtualPathError {
                input: input.to_owned(),
            });
        }

        Ok(Self(canonical.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl FromStr for VirtualPath {
    type Err = VirtualPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl AsRef<str> for VirtualPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for VirtualPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for VirtualPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid virtual path {:?}", self.input)
    }
}

impl std::error::Error for VirtualPathError {}

#[cfg(test)]
mod tests {
    use super::VirtualPath;

    #[test]
    fn canonicalizes_one_leading_slash_and_preserves_unicode_case() {
        let path = VirtualPath::parse("/模型/Texture.PNG").unwrap();

        assert_eq!(path.as_str(), "模型/Texture.PNG");
        assert_ne!(path, VirtualPath::parse("模型/texture.PNG").unwrap());
    }

    #[test]
    fn rejects_noncanonical_components() {
        for value in [
            "", "/", "//a", "a//b", "a/./b", "a/../b", "a\\b", "a:b", "a\0b",
        ] {
            assert!(VirtualPath::parse(value).is_err(), "accepted {value:?}");
        }
    }
}
