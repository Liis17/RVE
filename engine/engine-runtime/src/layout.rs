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

/// Returns the directory holding release `Engine/` and `Assets/`.
///
/// On macOS a player inside `<Name>.app/Contents/MacOS` reads them from
/// `Contents/Resources`; otherwise they sit next to the executable.
pub(crate) fn release_root(exe_dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos")
        && let Some(resources) = app_bundle_resources(exe_dir)
    {
        return resources;
    }
    exe_dir.to_owned()
}

fn app_bundle_resources(exe_dir: &Path) -> Option<PathBuf> {
    let contents = exe_dir.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = exe_dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
    is_bundle.then(|| contents.join("Resources"))
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
    use super::{Layout, ProjectSettingsLocation, app_bundle_resources};
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
    fn app_bundle_executable_reads_release_files_from_contents_resources() {
        assert_eq!(
            app_bundle_resources(Path::new("/Applications/Demo Game.app/Contents/MacOS")),
            Some(Path::new("/Applications/Demo Game.app/Contents/Resources").into())
        );
        assert_eq!(
            app_bundle_resources(Path::new("/opt/Demo.APP/Contents/MacOS")),
            Some(Path::new("/opt/Demo.APP/Contents/Resources").into())
        );
        for not_bundle in [
            "/opt/game",
            "/opt/game/Contents/MacOS",
            "/opt/Demo.app/MacOS",
            "/opt/Demo.app/Contents",
            "MacOS",
        ] {
            assert_eq!(
                app_bundle_resources(Path::new(not_bundle)),
                None,
                "{not_bundle}"
            );
        }
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
