#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveNameError {
    InvalidStem,
    ReservedWindowsName,
}

/// Validates the filename stem used by an archive-set registry or programmatic build.
pub(crate) fn validate_archive_stem(stem: &str) -> Result<(), ArchiveNameError> {
    if stem.is_empty()
        || stem.len() > 64
        || !stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ArchiveNameError::InvalidStem);
    }

    if is_reserved_windows_device(stem) {
        return Err(ArchiveNameError::ReservedWindowsName);
    }
    Ok(())
}

pub(crate) fn is_reserved_windows_device(value: &str) -> bool {
    let device = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            device.strip_prefix(prefix).is_some_and(|number| {
                number.len() == 1 && (b'1'..=b'9').contains(&number.as_bytes()[0])
            })
        })
}

#[cfg(test)]
mod tests {
    use super::{ArchiveNameError, is_reserved_windows_device, validate_archive_stem};

    #[test]
    fn validates_stems_and_rejects_reserved_windows_names_case_insensitively() {
        let reserved = ["CON", "prn", "AUX", "nul"]
            .into_iter()
            .map(str::to_owned)
            .chain((1..=9).map(|number| format!("COM{number}")))
            .chain((1..=9).map(|number| format!("lpt{number}")));
        for name in reserved {
            assert_eq!(
                validate_archive_stem(&name),
                Err(ArchiveNameError::ReservedWindowsName)
            );
        }

        let too_long = "x".repeat(65);
        for name in ["", "bad.name", too_long.as_str()] {
            assert_eq!(
                validate_archive_stem(name),
                Err(ArchiveNameError::InvalidStem)
            );
        }
        for name in ["base", "COM0", "COM10", "LPT10"] {
            assert_eq!(validate_archive_stem(name), Ok(()));
        }

        assert!(is_reserved_windows_device("cOn.payload"));
        assert!(!is_reserved_windows_device("asset.CON"));
    }
}
