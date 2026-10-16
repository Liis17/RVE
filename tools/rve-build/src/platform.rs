#[cfg(any(target_os = "macos", windows, test))]
use crate::error::BuildError;
#[cfg(any(target_os = "macos", test))]
use serde_json::Value;
use std::io;
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::path::PathBuf;

#[cfg(any(target_os = "macos", test))]
pub(crate) fn rewrite_icd_manifest(source: &[u8]) -> Result<Vec<u8>, BuildError> {
    let mut manifest: Value = serde_json::from_slice(source)
        .map_err(|error| BuildError::input(format!("некорректный Vulkan ICD JSON: {error}")))?;
    let library_path = manifest
        .get_mut("ICD")
        .and_then(Value::as_object_mut)
        .and_then(|icd| icd.get_mut("library_path"))
        .ok_or_else(|| BuildError::input("Vulkan ICD JSON не содержит строку ICD.library_path"))?;
    if !library_path.is_string() {
        return Err(BuildError::input(
            "Vulkan ICD JSON содержит нестроковое ICD.library_path",
        ));
    }
    *library_path = Value::String("./libMoltenVK.dylib".to_owned());
    let mut bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        BuildError::input(format!("не удалось записать Vulkan ICD JSON: {error}"))
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VulkanFiles {
    loader: PathBuf,
    molten: PathBuf,
    icd: PathBuf,
}

#[cfg(any(target_os = "macos", test))]
fn find_vulkan_files(sources: &[(String, PathBuf)]) -> Result<VulkanFiles, BuildError> {
    let mut tried = Vec::new();
    for (label, root) in sources {
        for sdk_root in [root.clone(), root.join("macOS")] {
            let loader = sdk_root.join("lib/libvulkan.1.dylib");
            let molten = sdk_root.join("lib/libMoltenVK.dylib");
            let icd = sdk_root.join("share/vulkan/icd.d/MoltenVK_icd.json");
            tried.extend([loader.clone(), molten.clone(), icd.clone()]);
            if is_regular_file(&loader) && is_regular_file(&molten) && is_regular_file(&icd) {
                return Ok(VulkanFiles {
                    loader,
                    molten,
                    icd,
                });
            }
        }
        // Homebrew currently links MoltenVK's ICD manifest under etc, while
        // SDK distributions use share/vulkan/icd.d.
        if label == "Homebrew" {
            let loader = root.join("lib/libvulkan.1.dylib");
            let molten = root.join("lib/libMoltenVK.dylib");
            let icd = root.join("etc/vulkan/icd.d/MoltenVK_icd.json");
            tried.extend([loader.clone(), molten.clone(), icd.clone()]);
            if is_regular_file(&loader) && is_regular_file(&molten) && is_regular_file(&icd) {
                return Ok(VulkanFiles {
                    loader,
                    molten,
                    icd,
                });
            }
        }
    }
    let tried = tried
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let hint = if sources
        .first()
        .is_some_and(|(label, _)| label == "--vulkan-sdk")
    {
        "указанный каталог --vulkan-sdk не содержит все необходимые файлы"
    } else {
        "передайте --vulkan-sdk <dir>, задайте VULKAN_SDK или установите `brew install vulkan-loader molten-vk`"
    };
    Err(BuildError::input(format!(
        "не найдены macOS Vulkan-файлы libvulkan.1.dylib, libMoltenVK.dylib и MoltenVK_icd.json; {hint}. Проверенные пути: {tried}"
    )))
}

#[cfg(any(target_os = "macos", test))]
fn vulkan_sources(
    sdk: Option<&Path>,
    environment_sdk: Option<PathBuf>,
    homebrew_roots: impl IntoIterator<Item = PathBuf>,
) -> Vec<(String, PathBuf)> {
    if let Some(path) = sdk {
        return vec![("--vulkan-sdk".to_owned(), path.to_owned())];
    }

    let mut sources = Vec::<(String, PathBuf)>::new();
    if let Some(path) = environment_sdk {
        sources.push(("VULKAN_SDK".to_owned(), path));
    }
    sources.extend(
        homebrew_roots
            .into_iter()
            .map(|path| ("Homebrew".to_owned(), path)),
    );
    sources
}

#[cfg(target_os = "macos")]
pub(crate) fn find_macos_vulkan_files(sdk: Option<&Path>) -> Result<VulkanFiles, BuildError> {
    let sources = vulkan_sources(
        sdk,
        std::env::var_os("VULKAN_SDK").map(PathBuf::from),
        [PathBuf::from("/opt/homebrew"), PathBuf::from("/usr/local")],
    );
    find_vulkan_files(&sources)
}

#[cfg(any(target_os = "macos", test))]
fn is_regular_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

#[cfg(target_os = "macos")]
pub(crate) fn copy_macos_vulkan_files(
    files: &VulkanFiles,
    engine_dir: &Path,
) -> Result<(), BuildError> {
    copy_contents(&files.loader, &engine_dir.join("libvulkan.1.dylib"))?;
    copy_contents(&files.molten, &engine_dir.join("libMoltenVK.dylib"))?;
    let manifest = std::fs::read(&files.icd).map_err(|error| io_error(&files.icd, error))?;
    let manifest = rewrite_icd_manifest(&manifest)?;
    let output = engine_dir.join("MoltenVK_icd.json");
    std::fs::write(&output, manifest).map_err(|error| io_error(&output, error))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_contents(source: &Path, destination: &Path) -> Result<(), BuildError> {
    let contents = std::fs::read(source).map_err(|error| io_error(source, error))?;
    std::fs::write(destination, contents).map_err(|error| io_error(destination, error))
}

#[cfg(target_os = "macos")]
fn io_error(path: &Path, error: io::Error) -> BuildError {
    BuildError::operational(format!("{}: {error}", path.display()))
}

pub(crate) fn publish_directory_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL в пути staging"))?;
        let destination = CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL в пути output"))?;
        // RENAME_EXCL makes a directory rename fail atomically if output appeared meanwhile.
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::MoveFileW;

        let source = source
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let destination = destination
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        let result = unsafe { MoveFileW(source.as_ptr(), destination.as_ptr()) };
        if result != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if std::fs::symlink_metadata(destination).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "output directory already exists",
            ));
        }
        std::fs::rename(source, destination)
    }
}

#[cfg(windows)]
pub(crate) fn verify_group_icon(executable: &Path) -> Result<(), BuildError> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{
        FindResourceW, LOAD_LIBRARY_AS_DATAFILE, LoadLibraryExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::RT_GROUP_ICON;

    let executable_wide = executable
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let module = unsafe {
        LoadLibraryExW(
            executable_wide.as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_AS_DATAFILE,
        )
    };
    if module.is_null() {
        return Err(BuildError::operational(format!(
            "не удалось проверить ресурсы иконки в {}: {}",
            executable.display(),
            io::Error::last_os_error()
        )));
    }
    let resource = unsafe {
        FindResourceW(
            module,
            std::ptr::without_provenance::<u16>(1),
            RT_GROUP_ICON,
        )
    };
    let release_result = unsafe { FreeLibrary(module) };
    if resource.is_null() {
        return Err(BuildError::operational(format!(
            "в {} отсутствует Windows RT_GROUP_ICON resource",
            executable.display()
        )));
    }
    if release_result == 0 {
        return Err(BuildError::operational(format!(
            "не удалось закрыть resource handle для {}: {}",
            executable.display(),
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{find_vulkan_files, rewrite_icd_manifest, vulkan_sources};
    use crate::temporary::TempDir;
    use std::fs;

    #[test]
    fn rewrites_only_the_icd_library_path() {
        let source = br#"{"file_format_version":"1.0.0","ICD":{"library_path":"old/path.dylib","api_version":"1.3.0"}}"#;
        let rewritten = rewrite_icd_manifest(source).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["ICD"]["library_path"], "./libMoltenVK.dylib");
        assert_eq!(value["ICD"]["api_version"], "1.3.0");
        assert!(rewrite_icd_manifest(br#"{"ICD":{}}"#).is_err());
        assert!(rewrite_icd_manifest(b"broken json").is_err());
    }

    #[test]
    fn searches_sdk_root_and_macos_subdirectory_in_order() {
        let first = TempDir::create(&std::env::temp_dir(), "rve-build-sdk-one").unwrap();
        let second = TempDir::create(&std::env::temp_dir(), "rve-build-sdk-two").unwrap();
        let second_root = second.path().join("macOS");
        let paths = [
            second_root.join("lib/libvulkan.1.dylib"),
            second_root.join("lib/libMoltenVK.dylib"),
            second_root.join("share/vulkan/icd.d/MoltenVK_icd.json"),
        ];
        for path in &paths {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture").unwrap();
        }
        let sources = vec![
            ("explicit".to_owned(), first.path().to_owned()),
            ("environment".to_owned(), second.path().to_owned()),
        ];
        let found = find_vulkan_files(&sources).unwrap();
        assert_eq!(found.loader, paths[0]);
        assert_eq!(found.molten, paths[1]);
        assert_eq!(found.icd, paths[2]);
        assert!(found.loader.starts_with(&second_root));
    }

    #[test]
    fn finds_homebrew_icd_manifests_linked_under_etc() {
        let homebrew = TempDir::create(&std::env::temp_dir(), "rve-build-homebrew").unwrap();
        let files = [
            homebrew.path().join("lib/libvulkan.1.dylib"),
            homebrew.path().join("lib/libMoltenVK.dylib"),
            homebrew.path().join("etc/vulkan/icd.d/MoltenVK_icd.json"),
        ];
        for path in &files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture").unwrap();
        }
        let found =
            find_vulkan_files(&[("Homebrew".to_owned(), homebrew.path().to_owned())]).unwrap();
        assert_eq!(found.loader, files[0]);
        assert_eq!(found.molten, files[1]);
        assert_eq!(found.icd, files[2]);
    }

    #[test]
    fn explicit_vulkan_sdk_does_not_search_environment_or_homebrew_roots() {
        let explicit = TempDir::create(&std::env::temp_dir(), "rve-build-sdk-explicit").unwrap();
        let fallback = TempDir::create(&std::env::temp_dir(), "rve-build-sdk-fallback").unwrap();
        let fallback_files = [
            fallback.path().join("lib/libvulkan.1.dylib"),
            fallback.path().join("lib/libMoltenVK.dylib"),
            fallback.path().join("share/vulkan/icd.d/MoltenVK_icd.json"),
        ];
        for path in &fallback_files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture").unwrap();
        }

        let sources = vulkan_sources(
            Some(explicit.path()),
            Some(fallback.path().to_owned()),
            [fallback.path().to_owned()],
        );
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "--vulkan-sdk");
        let error = find_vulkan_files(&sources).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--vulkan-sdk"));
        assert!(
            error
                .to_string()
                .contains(&explicit.path().display().to_string())
        );
    }

    #[test]
    fn publish_directory_refuses_to_replace_existing_output() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-publish-no-replace").unwrap();
        let source = root.path().join("stage");
        let destination = root.path().join("output");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("new"), b"new").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), b"keep").unwrap();

        let error = super::publish_directory_no_replace(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"keep");
        assert!(source.join("new").is_file());
        assert!(!destination.join("new").exists());
    }

    #[test]
    fn publish_directory_moves_source_to_new_output() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-publish-new-output").unwrap();
        let source = root.path().join("stage");
        let destination = root.path().join("output");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"published").unwrap();

        super::publish_directory_no_replace(&source, &destination).unwrap();

        assert_eq!(fs::read(destination.join("file")).unwrap(), b"published");
        assert!(!source.exists());
    }
}
