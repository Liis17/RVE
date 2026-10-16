use engine_resources::SourceMode;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn normalize_layout_path(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let absolute = std::path::absolute(path)?;
        let wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
        let normalized = strip_windows_verbatim_prefix(&wide).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "неподдерживаемый Windows namespace в пути проекта",
            )
        })?;
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&normalized)))
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

#[cfg(any(windows, test))]
fn strip_windows_verbatim_prefix(path: &[u16]) -> Option<Vec<u16>> {
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
            return None;
        }
        disk_path.to_vec()
    } else {
        path.to_vec()
    };

    Some(
        path.into_iter()
            .map(|value| {
                if value == b'/' as u16 {
                    b'\\' as u16
                } else {
                    value
                }
            })
            .collect(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectSettingsLocation {
    File(PathBuf),
    Resource(&'static str),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Layout {
    pub bootstrap: PathBuf,
    pub assets: PathBuf,
    pub source_mode: SourceMode,
    pub project_settings: ProjectSettingsLocation,
}

impl Layout {
    pub fn release(exe_dir: &Path) -> Self {
        Self {
            bootstrap: exe_dir.join("Engine/bootstrap.rves"),
            assets: exe_dir.join("Assets"),
            source_mode: SourceMode::Registry,
            project_settings: ProjectSettingsLocation::Resource("/rve/project.toml"),
        }
    }

    pub fn dev(project_dir: &Path) -> Self {
        Self {
            bootstrap: project_dir.join("bootstrap.rves"),
            assets: project_dir.join("Assets"),
            source_mode: SourceMode::Loose,
            project_settings: ProjectSettingsLocation::File(project_dir.join("project.rveproject")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Layout, ProjectSettingsLocation, strip_windows_verbatim_prefix};
    use engine_resources::SourceMode;
    use std::path::Path;

    #[test]
    fn release_layout_is_anchored_to_executable_directory() {
        let layout = Layout::release(Path::new("/opt/game"));
        assert_eq!(
            layout.bootstrap,
            Path::new("/opt/game/Engine/bootstrap.rves")
        );
        assert_eq!(layout.assets, Path::new("/opt/game/Assets"));
        assert_eq!(layout.source_mode, SourceMode::Registry);
        assert_eq!(
            layout.project_settings,
            ProjectSettingsLocation::Resource("/rve/project.toml")
        );
    }

    #[test]
    fn dev_layout_is_anchored_to_project_and_uses_loose_files() {
        let layout = Layout::dev(Path::new("/tmp/demo"));
        assert_eq!(layout.bootstrap, Path::new("/tmp/demo/bootstrap.rves"));
        assert_eq!(layout.assets, Path::new("/tmp/demo/Assets"));
        assert_eq!(layout.source_mode, SourceMode::Loose);
        assert_eq!(
            layout.project_settings,
            ProjectSettingsLocation::File(Path::new("/tmp/demo/project.rveproject").into())
        );
    }

    #[test]
    fn windows_verbatim_paths_are_converted_to_regular_absolute_paths() {
        let disk = r"\\?\C:\games/demo".encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            String::from_utf16(&strip_windows_verbatim_prefix(&disk).unwrap()).unwrap(),
            r"C:\games\demo"
        );

        let unc = r"\\?\uNc\server\share\game"
            .encode_utf16()
            .collect::<Vec<_>>();
        assert_eq!(
            String::from_utf16(&strip_windows_verbatim_prefix(&unc).unwrap()).unwrap(),
            r"\\server\share\game"
        );

        let unsupported = r"\\?\GLOBALROOT\Device\HarddiskVolume1";
        assert!(
            strip_windows_verbatim_prefix(&unsupported.encode_utf16().collect::<Vec<_>>())
                .is_none()
        );
    }
}
