use engine_project::ProjectSettings;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PROJECT_TEMPLATE: &str = include_str!("../templates/new_project/project.rveproject");
const BOOTSTRAP_TEMPLATE: &str = include_str!("../templates/new_project/bootstrap.rves");
const CAMERA_TEMPLATE: &str =
    include_str!("../templates/new_project/Assets/cameras/default.camerasettings");
const SCENE_TEMPLATE: &str = include_str!("../templates/new_project/Assets/scenes/main.rvescene");
const JUMP_TEMPLATE: &str = include_str!("../templates/new_project/Assets/scripts/jump.lua");
const GUEST_CARGO_TEMPLATE: &str = include_str!("../templates/new_project/Scripts/Cargo.toml");
const GUEST_SOURCE_TEMPLATE: &str = include_str!("../templates/new_project/Scripts/src/lib.rs");
const GITIGNORE_TEMPLATE: &str = include_str!("../templates/new_project/.gitignore");
const CUBE_MODEL: &[u8] = include_bytes!("../templates/new_project/Assets/models/cube.fbx");

static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct NewProjectDialog {
    pub name: String,
    pub parent: String,
    pub error: Option<String>,
}

impl Default for NewProjectDialog {
    fn default() -> Self {
        Self {
            name: "New Project".to_owned(),
            parent: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .display()
                .to_string(),
            error: None,
        }
    }
}

pub fn create_project(name: &str, parent: &Path) -> Result<PathBuf, String> {
    let name = clean_project_name(name)?;
    let id = project_id(&name);
    let parent =
        std::path::absolute(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    if !parent.is_dir() {
        return Err(format!(
            "{}: родительская папка не найдена",
            parent.display()
        ));
    }
    let parent =
        fs::canonicalize(&parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let target = parent.join(&id);
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            return Err(format!(
                "{}: папка проекта уже существует",
                target.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", target.display())),
    }

    let staging = create_staging_dir(&parent, &id).map_err(|error| {
        format!(
            "не удалось создать временный каталог проекта в {}: {error}",
            parent.display()
        )
    })?;
    let result = (|| {
        write_project_template(&staging, &id, &name)?;
        publish_no_replace(&staging, &target).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                format!("{}: папка проекта уже существует", target.display())
            } else {
                format!("не удалось создать {}: {error}", target.display())
            }
        })?;
        Ok(target.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn clean_project_name(name: &str) -> Result<String, String> {
    let name = name
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let name = name.trim();
    if name.is_empty() {
        Err("название проекта не должно быть пустым".to_owned())
    } else {
        Ok(name.to_owned())
    }
}

fn project_id(name: &str) -> String {
    let mut id = String::new();
    for character in name.chars() {
        let character = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else if matches!(character, '-' | '_') {
            character
        } else {
            '-'
        };
        if character == '-' && (id.is_empty() || id.ends_with(['-', '_'])) {
            continue;
        }
        if id.len() == 64 {
            break;
        }
        id.push(character);
    }
    let id = id.trim_matches(['-', '_']);
    if id.is_empty() {
        "new-project".to_owned()
    } else {
        id.to_owned()
    }
}

fn create_staging_dir(parent: &Path, id: &str) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let serial = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{id}.rve-new-{}-{serial}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "не удалось выделить уникальный staging-каталог",
    ))
}

fn write_project_template(root: &Path, id: &str, name: &str) -> Result<(), String> {
    let mut settings = ProjectSettings::parse(PROJECT_TEMPLATE)?;
    settings.id = id.to_owned();
    settings.name = name.to_owned();
    let settings = settings.to_toml()?;
    let bootstrap =
        BOOTSTRAP_TEMPLATE.replace("WindowTitle=New Project", &format!("WindowTitle={name}"));
    for (path, contents) in [
        ("project.rveproject", settings.as_bytes()),
        ("bootstrap.rves", bootstrap.as_bytes()),
        (
            "Assets/cameras/default.camerasettings",
            CAMERA_TEMPLATE.as_bytes(),
        ),
        ("Assets/scenes/main.rvescene", SCENE_TEMPLATE.as_bytes()),
        ("Assets/scripts/jump.lua", JUMP_TEMPLATE.as_bytes()),
        ("Scripts/Cargo.toml", GUEST_CARGO_TEMPLATE.as_bytes()),
        ("Scripts/src/lib.rs", GUEST_SOURCE_TEMPLATE.as_bytes()),
        (".gitignore", GITIGNORE_TEMPLATE.as_bytes()),
        ("Assets/models/cube.fbx", CUBE_MODEL),
    ] {
        write_file(&root.join(path), contents)?;
    }
    Ok(())
}

fn write_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().expect("template file has a parent");
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    fs::write(path, contents).map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(windows)]
fn publish_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both arguments are valid null-terminated UTF-16 paths for this call.
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn publish_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "путь содержит NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "путь содержит NUL"))?;
    // SAFETY: both C strings remain valid for the duration of the atomic no-replace rename.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn publish_no_replace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "публикация проекта поддерживается на Windows и macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::{create_project, project_id};
    use crate::project::OpenProject;
    use engine_project::BootstrapConfig;
    use engine_scene::{ParamValue, SceneDocument};
    use engine_script::{ScriptInterruptHandle, lite_properties};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_parent() -> PathBuf {
        let serial = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let parent =
            std::env::temp_dir().join(format!("rve-new-project-{}-{serial}", std::process::id()));
        fs::create_dir(&parent).unwrap();
        parent
    }

    #[test]
    fn creates_a_project_the_editor_can_open_and_validate() {
        let parent = temporary_parent();
        let project_dir = create_project("First Game", &parent).unwrap();
        let project = OpenProject::open(&project_dir).unwrap();
        assert_eq!(project.settings.id, "first-game");
        assert_eq!(project.settings.name, "First Game");
        assert_eq!(project.settings.heavy_scripts.as_deref(), Some("Scripts"));
        assert_eq!(
            project.settings.starting_scene(),
            Some("/scenes/main.rvescene")
        );

        let bootstrap = BootstrapConfig::load(project_dir.join("bootstrap.rves")).unwrap();
        assert_eq!(bootstrap.window_size_x, Some(1280));
        assert_eq!(bootstrap.window_size_y, Some(720));
        assert_eq!(bootstrap.window_title.as_deref(), Some("First Game"));

        let scene_source = project.read_text("/scenes/main.rvescene").unwrap();
        let scene = SceneDocument::parse("/scenes/main.rvescene", &scene_source).unwrap();
        assert_eq!(scene.objects.len(), 3);
        assert_eq!(scene.objects[0].heavy_scripts[0].name, "Orbit");

        let jump_path = project_dir.join("Assets/scripts/jump.lua");
        let jump_source = fs::read_to_string(jump_path).unwrap();
        let interrupt = ScriptInterruptHandle::new();
        let schema = lite_properties("/scripts/jump.lua", &jump_source, &interrupt)
            .unwrap()
            .unwrap();
        let params = BTreeMap::from([
            ("height".to_owned(), ParamValue::Float(2.0)),
            ("duration".to_owned(), ParamValue::Float(0.8)),
        ]);
        assert_eq!(schema.resolve(&params).unwrap(), params);
        drop(project);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn sanitizes_project_names_to_valid_ids() {
        assert_eq!(project_id("  My Demo: Game!!  "), "my-demo-game");
        assert_eq!(project_id("---"), "new-project");
        assert_eq!(project_id(&"A".repeat(80)).len(), 64);
    }

    #[test]
    fn refuses_an_existing_project_folder_without_overwriting_it() {
        let parent = temporary_parent();
        let existing = parent.join("first-game");
        fs::create_dir(&existing).unwrap();
        let sentinel = existing.join("keep.txt");
        fs::write(&sentinel, "user data").unwrap();

        let error = create_project("First Game", &parent).unwrap_err();

        assert!(error.contains("уже существует"));
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "user data");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        fs::remove_dir_all(parent).unwrap();
    }
}
