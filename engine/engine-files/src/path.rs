use std::io;
use std::path::{Path, PathBuf};

/// Removes a Windows verbatim prefix from a disk or UNC path.
///
/// On Windows, disk paths such as `\\?\C:\game` become `C:\game`, and
/// `\\?\UNC\server\share\game` becomes `\\server\share\game`. Other
/// `\\?\` namespace forms return `InvalidInput`. On non-Windows platforms,
/// the path is returned unchanged. Windows path separators are normalized to `\`.
pub fn strip_windows_verbatim_prefix(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let normalized = strip_windows_verbatim_prefix_from_wide(
            &path.as_os_str().encode_wide().collect::<Vec<_>>(),
        )?;
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&normalized)))
    }
    #[cfg(not(windows))]
    {
        Ok(path.to_path_buf())
    }
}

#[cfg(any(windows, test))]
fn strip_windows_verbatim_prefix_from_wide(path: &[u16]) -> io::Result<Vec<u16>> {
    const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const VERBATIM_UNC: [u16; 8] = [
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];

    fn lower_ascii(value: u16) -> u16 {
        if (b'A' as u16..=b'Z' as u16).contains(&value) {
            value + (b'a' - b'A') as u16
        } else {
            value
        }
    }

    let path = if path.len() >= VERBATIM_UNC.len()
        && path[..VERBATIM_UNC.len()]
            .iter()
            .zip(VERBATIM_UNC)
            .all(|(left, right)| lower_ascii(*left) == lower_ascii(right))
    {
        let mut unc = vec![b'\\' as u16, b'\\' as u16];
        unc.extend_from_slice(&path[VERBATIM_UNC.len()..]);
        unc
    } else if path.starts_with(&VERBATIM) {
        let disk_path = &path[VERBATIM.len()..];
        if disk_path.len() < 3
            || !(b'A' as u16..=b'Z' as u16).contains(&disk_path[0])
                && !(b'a' as u16..=b'z' as u16).contains(&disk_path[0])
            || disk_path[1] != b':' as u16
            || (disk_path[2] != b'\\' as u16 && disk_path[2] != b'/' as u16)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Windows verbatim path namespace",
            ));
        }
        disk_path.to_vec()
    } else {
        path.to_vec()
    };

    Ok(path
        .into_iter()
        .map(|value| {
            if value == b'/' as u16 {
                b'\\' as u16
            } else {
                value
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{strip_windows_verbatim_prefix, strip_windows_verbatim_prefix_from_wide};
    use std::path::Path;

    fn parse_windows_path(path: &str) -> Result<String, std::io::Error> {
        let wide = path.encode_utf16().collect::<Vec<_>>();
        String::from_utf16(&strip_windows_verbatim_prefix_from_wide(&wide)?)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    #[test]
    fn parses_disk_and_unc_verbatim_paths_on_every_platform() {
        assert_eq!(
            parse_windows_path(r"\\?\C:\games/demo").unwrap(),
            r"C:\games\demo"
        );
        assert_eq!(
            parse_windows_path(r"\\?\uNc\server\share\game").unwrap(),
            r"\\server\share\game"
        );
        assert_eq!(
            parse_windows_path(r"C:/games/demo").unwrap(),
            r"C:\games\demo"
        );
    }

    #[test]
    fn rejects_other_verbatim_namespaces() {
        assert!(parse_windows_path(r"\\?\GLOBALROOT\Device\HarddiskVolume1").is_err());
        assert!(parse_windows_path(r"\\?\relative").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn public_path_function_is_identity_on_non_windows() {
        let path = Path::new(r"\\?\C:\games\demo");
        assert_eq!(strip_windows_verbatim_prefix(path).unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn public_path_function_strips_verbatim_disk_and_unc_prefixes() {
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\C:\games\demo")).unwrap(),
            Path::new(r"C:\games\demo")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\server\share\game")).unwrap(),
            Path::new(r"\\server\share\game")
        );
    }
}
