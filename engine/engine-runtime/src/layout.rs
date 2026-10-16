#[cfg(windows)]
use engine_files::strip_windows_verbatim_prefix;
use engine_resources::SourceMode;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn normalize_layout_path(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        let absolute = std::path::absolute(path)?;
        strip_windows_verbatim_prefix(&absolute)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
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
    use super::{Layout, ProjectSettingsLocation};
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
}
