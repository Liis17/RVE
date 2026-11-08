use std::fs;
use std::path::{Component, Path, PathBuf};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

/// Resolves an existing project file or directory and rejects paths outside the project.
pub fn resolve_read_path(project_root: &Path, input: &str) -> Result<PathBuf, String> {
    let root = canonical_project_root(project_root)?;
    let path = Path::new(input);
    validate_components(path)?;
    if !path.is_absolute()
        && path
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
    {
        return Err(format!("{} must be relative to the project", input));
    }
    reject_hidden(path)?;
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| format!("{}: {error}", candidate.display()))?;
    if !canonical.starts_with(&root) || has_hidden_component(&canonical) {
        return Err(format!(
            "{} is outside readable project paths",
            path.display()
        ));
    }
    Ok(canonical)
}

/// Resolves a directory below `Assets/`; an empty path denotes the assets root.
pub fn resolve_assets_directory(project_root: &Path, input: &str) -> Result<PathBuf, String> {
    let assets = canonical_assets_root(project_root)?;
    let relative = asset_relative_path(input)?;
    let candidate = assets.join(relative);
    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| format!("{}: {error}", candidate.display()))?;
    if !canonical.starts_with(&assets) || has_hidden_component(&canonical) {
        return Err(format!("{} is outside Assets", input));
    }
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", input));
    }
    Ok(canonical)
}

/// Resolves an existing file below `Assets/`.
pub fn resolve_asset_file(project_root: &Path, input: &str) -> Result<PathBuf, String> {
    let assets = canonical_assets_root(project_root)?;
    let relative = asset_relative_path(input)?;
    if relative.as_os_str().is_empty() {
        return Err("Assets root is not a file".to_owned());
    }
    let candidate = assets.join(relative);
    let canonical = fs::canonicalize(&candidate)
        .map_err(|error| format!("{}: {error}", candidate.display()))?;
    if !canonical.starts_with(&assets) || has_hidden_component(&canonical) {
        return Err(format!("{} is outside Assets", input));
    }
    if !canonical.is_file() {
        return Err(format!("{} is not a file", input));
    }
    Ok(canonical)
}

/// Resolves an Assets target, including a new file whose parent already exists.
pub fn resolve_write_path(project_root: &Path, input: &str) -> Result<PathBuf, String> {
    let assets = canonical_assets_root(project_root)?;
    let relative = asset_relative_path(input)?;
    if relative.as_os_str().is_empty() {
        return Err("Assets root cannot be modified as a file".to_owned());
    }
    reject_hidden(&relative)?;

    let mut candidate = assets.clone();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(format!("{} contains an invalid path component", input));
        };
        candidate.push(name);
        if let Ok(metadata) = fs::symlink_metadata(&candidate)
            && is_link_or_junction(&metadata)
        {
            return Err(format!("{} contains a symbolic link or junction", input));
        }
    }

    let parent = candidate
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", input))?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    if !canonical_parent.starts_with(&assets) {
        return Err(format!("{} is outside Assets", input));
    }
    match fs::symlink_metadata(&candidate) {
        Ok(metadata) => {
            if is_link_or_junction(&metadata) {
                return Err(format!("{} is a symbolic link or junction", input));
            }
            let canonical = fs::canonicalize(&candidate)
                .map_err(|error| format!("{}: {error}", candidate.display()))?;
            if !canonical.starts_with(&assets) || has_hidden_component(&canonical) {
                return Err(format!("{} is outside Assets", input));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", candidate.display())),
    }
    Ok(candidate)
}

/// Resolves an existing target for delete.
#[allow(dead_code)]
pub fn resolve_delete_path(project_root: &Path, input: &str) -> Result<PathBuf, String> {
    let path = resolve_write_path(project_root, input)?;
    if !path.exists() {
        return Err(format!("{} does not exist", input));
    }
    Ok(path)
}

/// Resolves both ends of a rename; the source must exist and neither path may cross a link.
#[allow(dead_code)]
pub fn resolve_rename_paths(
    project_root: &Path,
    source: &str,
    destination: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let source_path = resolve_delete_path(project_root, source)?;
    let destination_path = resolve_write_path(project_root, destination)?;
    Ok((source_path, destination_path))
}

fn canonical_project_root(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(super) fn is_link_or_junction(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        // Junctions are reparse points but are not reported as symbolic links by `FileType`.
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn canonical_assets_root(project_root: &Path) -> Result<PathBuf, String> {
    let root = canonical_project_root(project_root)?;
    let assets_path = root.join("Assets");
    let metadata = fs::symlink_metadata(&assets_path)
        .map_err(|error| format!("{}: {error}", assets_path.display()))?;
    if is_link_or_junction(&metadata) {
        return Err("Assets root cannot be a symbolic link or junction".to_owned());
    }
    let assets = fs::canonicalize(&assets_path)
        .map_err(|error| format!("{}: {error}", assets_path.display()))?;
    if !assets.is_dir() || !assets.starts_with(&root) {
        return Err("Assets root is outside the project".to_owned());
    }
    Ok(assets)
}

fn asset_relative_path(input: &str) -> Result<PathBuf, String> {
    let path = Path::new(input);
    if path.is_absolute() {
        return Err(format!("{} must be relative to Assets", input));
    }
    validate_components(path)?;
    reject_hidden(path)?;
    let mut components = path.components().peekable();
    if components
        .peek()
        .is_some_and(|component| component.as_os_str().eq_ignore_ascii_case("Assets"))
    {
        components.next();
    }
    let mut relative = PathBuf::new();
    for component in components {
        let Component::Normal(name) = component else {
            return Err(format!("{} contains an invalid path component", input));
        };
        relative.push(name);
    }
    Ok(relative)
}

fn validate_components(path: &Path) -> Result<(), String> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("{} contains traversal", path.display()));
    }
    Ok(())
}

fn reject_hidden(path: &Path) -> Result<(), String> {
    if has_hidden_component(path) {
        return Err(".rve is not readable or writable through MCP".to_owned());
    }
    Ok(())
}

fn has_hidden_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().eq_ignore_ascii_case(".rve"))
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_asset_file, resolve_assets_directory, resolve_read_path, resolve_write_path,
    };
    #[cfg(unix)]
    use super::{resolve_delete_path, resolve_rename_paths};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "rve-editor-mcp-paths-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("Assets")).unwrap();
            std::fs::create_dir_all(root.join(".rve")).unwrap();
            Self(root)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_traversal_absolute_paths_outside_the_project_and_hidden_rve_files() {
        let project = TempProject::new();
        let outside = project.0.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();
        std::fs::write(project.0.join(".rve/private.txt"), "private").unwrap();

        assert!(resolve_read_path(&project.0, "Assets/../.rve/private.txt").is_err());
        assert!(resolve_read_path(&project.0, &outside_file.display().to_string()).is_err());
        assert!(resolve_read_path(&project.0, ".rve/private.txt").is_err());
        assert!(resolve_assets_directory(&project.0, "../.rve").is_err());
        assert!(resolve_write_path(&project.0, ".rve/private.txt").is_err());
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_external_symlinks_for_read_create_rename_and_delete() {
        use std::os::unix::fs::symlink;

        let project = TempProject::new();
        let outside = project.0.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("source.txt"), "secret").unwrap();
        symlink(&outside, project.0.join("Assets/link")).unwrap();

        assert!(resolve_read_path(&project.0, "Assets/link/source.txt").is_err());
        assert!(resolve_write_path(&project.0, "link/new.txt").is_err());
        assert!(resolve_rename_paths(&project.0, "link/source.txt", "renamed.txt").is_err());
        assert!(resolve_delete_path(&project.0, "link/source.txt").is_err());
        assert!(resolve_delete_path(&project.0, "link").is_err());
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn allows_existing_project_files_and_new_files_below_assets() {
        let project = TempProject::new();
        std::fs::write(project.0.join("Assets/a.txt"), "hello").unwrap();

        assert_eq!(
            resolve_read_path(&project.0, "Assets/a.txt").unwrap(),
            std::fs::canonicalize(project.0.join("Assets/a.txt")).unwrap()
        );
        assert!(
            resolve_asset_file(&project.0, "Assets/a.txt")
                .unwrap()
                .is_file()
        );
        assert_eq!(
            resolve_write_path(&project.0, "Assets/new.txt").unwrap(),
            std::fs::canonicalize(project.0.join("Assets"))
                .unwrap()
                .join("new.txt")
        );
        assert_eq!(
            resolve_assets_directory(&project.0, "").unwrap(),
            std::fs::canonicalize(project.0.join("Assets")).unwrap()
        );
    }
}
