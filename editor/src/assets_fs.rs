//! Файловые операции над каталогом ресурсов проекта. Все пути остаются внутри `Assets/`.

use engine_files::VirtualPath;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Папка или файл каталога ресурсов.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetEntry {
    pub name: String,
    /// Путь от корня ресурсов с разделителем `/`, без ведущей черты.
    pub path: String,
    pub is_dir: bool,
    /// Содержимое папки; папки идут первыми, внутри групп — по имени.
    pub children: Vec<AssetEntry>,
}

/// Вид ресурса, определяемый по расширению файла.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetKind {
    Scene,
    Prefab,
    CameraSettings,
    PostProcess,
    ScreenShader,
    PhysicsMaterial,
    Material,
    Shader,
    Model,
    Texture,
    Hdr,
    Audio,
    Script,
    Other,
}

impl AssetKind {
    pub fn of(path: &str) -> Self {
        let extension = path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension.to_ascii_lowercase());
        match extension.as_deref() {
            Some("rvescene") => Self::Scene,
            Some("rveprefab") => Self::Prefab,
            Some("camerasettings") => Self::CameraSettings,
            Some("postprocess") => Self::PostProcess,
            Some("glsl") => Self::ScreenShader,
            Some("rvephysmat") => Self::PhysicsMaterial,
            Some("rvemat") => Self::Material,
            Some("rveshader") => Self::Shader,
            Some("fbx") => Self::Model,
            Some("png") => Self::Texture,
            Some("hdr") => Self::Hdr,
            Some("wav" | "ogg") => Self::Audio,
            Some("lua") => Self::Script,
            _ => Self::Other,
        }
    }
}

/// Каталог ресурсов проекта.
#[derive(Clone, Debug)]
pub struct AssetsDir {
    root: PathBuf,
}

impl AssetsDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Переводит путь от корня ресурсов в файловый; `..` и абсолютные компоненты отклоняются.
    /// Пустая строка обозначает сам корень.
    pub fn resolve(&self, path: &str) -> io::Result<PathBuf> {
        if path.is_empty() {
            return Ok(self.root.clone());
        }
        let virtual_path = VirtualPath::parse(path)
            .map_err(|_| invalid_input(format!("путь {path:?} выходит за пределы ресурсов")))?;
        let mut resolved = self.root.clone();
        resolved.extend(virtual_path.as_str().split('/'));
        Ok(resolved)
    }

    /// Путь ресурса для файла из системного диалога; файл должен лежать внутри корня.
    pub fn virtual_path_of(&self, file: &Path) -> io::Result<VirtualPath> {
        let outside = || invalid_input(format!("{} находится вне Assets", file.display()));
        let relative = file.strip_prefix(&self.root).map_err(|_| outside())?;
        let components = relative
            .components()
            .map(|component| component.as_os_str().to_str().ok_or_else(outside))
            .collect::<io::Result<Vec<_>>>()?;
        VirtualPath::parse(&components.join("/")).map_err(|_| outside())
    }

    /// Строит дерево от корня. Символические ссылки не обходятся.
    pub fn list(&self) -> io::Result<Vec<AssetEntry>> {
        self.list_directory(&self.root, "")
    }

    fn list_directory(&self, directory: &Path, prefix: &str) -> io::Result<Vec<AssetEntry>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            // Имя вне UTF-8 нельзя выразить virtual path, такие файлы движок не загрузит.
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = join(prefix, &name);
            let children = if file_type.is_dir() {
                self.list_directory(&entry.path(), &path)?
            } else {
                Vec::new()
            };
            entries.push(AssetEntry {
                name,
                path,
                is_dir: file_type.is_dir(),
                children,
            });
        }
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    /// Создаёт новый файл с содержимым `contents`; существующий файл не перезаписывается.
    pub fn create_file(&self, parent: &str, name: &str, contents: &str) -> io::Result<String> {
        validate_name(name)?;
        let path = join(parent, name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.resolve(&path)?)?;
        io::Write::write_all(&mut file, contents.as_bytes())?;
        Ok(path)
    }

    pub fn create_folder(&self, parent: &str, name: &str) -> io::Result<String> {
        validate_name(name)?;
        let path = join(parent, name);
        fs::create_dir(self.resolve(&path)?)?;
        Ok(path)
    }

    /// Переименовывает файл или папку в пределах родительской папки, не перезаписывая соседей.
    pub fn rename(&self, path: &str, new_name: &str) -> io::Result<String> {
        validate_name(new_name)?;
        if path.is_empty() {
            return Err(invalid_input("нельзя переименовать корень ресурсов"));
        }
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let target_path = join(parent, new_name);
        let source = self.resolve(path)?;
        let target = self.resolve(&target_path)?;
        // Смена только регистра на нечувствительной ФС — это тот же файл, а не коллизия.
        let same_entry = path.eq_ignore_ascii_case(&target_path);
        if target.exists() && !same_entry {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{target_path:?} уже существует"),
            ));
        }
        fs::rename(source, target)?;
        Ok(target_path)
    }

    /// Необратимо удаляет файл или папку с содержимым; подтверждение остаётся за вызывающим.
    pub fn remove(&self, path: &str) -> io::Result<()> {
        if path.is_empty() {
            return Err(invalid_input("нельзя удалить корень ресурсов"));
        }
        let target = self.resolve(path)?;
        if fs::symlink_metadata(&target)?.is_dir() {
            fs::remove_dir_all(target)
        } else {
            fs::remove_file(target)
        }
    }

    /// Копирует внешний файл в папку ресурсов; при совпадении имени добавляет номер.
    pub fn import(&self, source: &Path, folder: &str) -> io::Result<String> {
        if !fs::metadata(source)?.is_file() {
            return Err(invalid_input(format!(
                "{} не является файлом",
                source.display()
            )));
        }
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid_input(format!("{}: имя не в UTF-8", source.display())))?;
        validate_name(name)?;
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
            _ => (name, None),
        };
        for number in 0_u32.. {
            let candidate = match (number, extension) {
                (0, _) => name.to_owned(),
                (_, Some(extension)) => format!("{stem} ({number}).{extension}"),
                (_, None) => format!("{stem} ({number})"),
            };
            let path = join(folder, &candidate);
            let target = self.resolve(&path)?;
            // create_new закрывает гонку между проверкой имени и копированием.
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
            {
                Ok(file) => {
                    drop(file);
                    if let Err(error) = fs::copy(source, &target) {
                        let _ = fs::remove_file(&target);
                        return Err(error);
                    }
                    return Ok(path);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        unreachable!("u32 номеров хватает для файлов одной папки")
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn validate_name(name: &str) -> io::Result<()> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.contains(['/', '\\', ':', '\0', '<', '>', '"', '|', '?', '*']);
    if valid {
        Ok(())
    } else {
        Err(invalid_input(format!("недопустимое имя {name:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempAssets {
        base: PathBuf,
        assets: AssetsDir,
    }

    impl TempAssets {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let base = std::env::temp_dir().join(format!(
                "rve-editor-assets-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let root = base.join("Assets");
            fs::create_dir_all(&root).unwrap();
            Self {
                base,
                assets: AssetsDir::new(root),
            }
        }

        fn write(&self, path: &str) {
            let path = self.assets.root().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"data").unwrap();
        }

        fn external(&self, name: &str) -> PathBuf {
            let path = self.base.join(name);
            fs::write(&path, b"external").unwrap();
            path
        }
    }

    impl Drop for TempAssets {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn listing_sorts_folders_first_and_uses_virtual_paths() {
        let temp = TempAssets::new();
        temp.write("b.png");
        temp.write("z/c.lua");
        let tree = temp.assets.list().unwrap();
        assert_eq!(tree[0].name, "z");
        assert!(tree[0].is_dir);
        assert_eq!(tree[0].children[0].path, "z/c.lua");
        assert_eq!(tree[1].path, "b.png");
    }

    #[test]
    fn asset_kind_ignores_extension_case() {
        assert_eq!(AssetKind::of("a/b.PNG"), AssetKind::Texture);
        assert_eq!(AssetKind::of("a.b/c"), AssetKind::Other);
        assert_eq!(AssetKind::of("m.fbx"), AssetKind::Model);
        assert_eq!(AssetKind::of("s.rvescene"), AssetKind::Scene);
        assert_eq!(
            AssetKind::of("camera.CAMERASETTINGS"),
            AssetKind::CameraSettings
        );
        assert_eq!(AssetKind::of("look.PostProcess"), AssetKind::PostProcess);
        assert_eq!(AssetKind::of("s.lua"), AssetKind::Script);
    }

    #[test]
    fn physics_material_files_are_classified_and_created_without_overwrite() {
        assert_eq!(
            AssetKind::of("materials/ICE.RVEPHYSMAT"),
            AssetKind::PhysicsMaterial
        );
        let temp = TempAssets::new();
        assert_eq!(
            temp.assets
                .create_file("", "ice.rvephysmat", "static_friction = 0.1\n")
                .unwrap(),
            "ice.rvephysmat"
        );
        assert!(temp.assets.create_file("", "ice.rvephysmat", "").is_err());
        assert_eq!(
            fs::read_to_string(temp.assets.root().join("ice.rvephysmat")).unwrap(),
            "static_friction = 0.1\n"
        );
    }

    #[test]
    fn paths_cannot_escape_the_assets_root() {
        let temp = TempAssets::new();
        for path in ["../x", "a/../../x", "a\\b", "C:/x", "a//b"] {
            assert!(temp.assets.resolve(path).is_err(), "{path}");
        }
        assert_eq!(temp.assets.resolve("").unwrap(), temp.assets.root());
        assert!(temp.assets.resolve("a/b").unwrap().ends_with("Assets/a/b"));
    }

    #[test]
    fn dialog_paths_become_virtual_paths_only_inside_the_root() {
        let temp = TempAssets::new();
        let inside = temp.assets.root().join("a").join("b.rvescene");
        assert_eq!(
            temp.assets.virtual_path_of(&inside).unwrap().as_str(),
            "a/b.rvescene"
        );
        assert!(temp.assets.virtual_path_of(&temp.base.join("x")).is_err());
        assert!(temp.assets.virtual_path_of(temp.assets.root()).is_err());
    }

    #[test]
    fn invalid_names_are_rejected_by_every_operation() {
        let temp = TempAssets::new();
        temp.write("a.png");
        for name in ["", ".", "..", "x/y", "x\\y", "a:b", "x.", "x "] {
            assert!(temp.assets.create_folder("", name).is_err(), "{name:?}");
            assert!(temp.assets.rename("a.png", name).is_err(), "{name:?}");
        }
        assert!(temp.assets.root().join("a.png").is_file());
    }

    #[test]
    fn folders_are_created_inside_parent_and_duplicates_fail() {
        let temp = TempAssets::new();
        assert_eq!(temp.assets.create_folder("", "a").unwrap(), "a");
        assert_eq!(temp.assets.create_folder("a", "b").unwrap(), "a/b");
        assert!(temp.assets.root().join("a/b").is_dir());
        assert!(temp.assets.create_folder("", "a").is_err());
    }

    #[test]
    fn rename_refuses_to_overwrite_a_neighbour() {
        let temp = TempAssets::new();
        temp.write("d/a.png");
        temp.write("d/b.png");
        assert_eq!(temp.assets.rename("d/a.png", "c.png").unwrap(), "d/c.png");
        let error = temp.assets.rename("d/c.png", "b.png").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(temp.assets.root().join("d/c.png").is_file());
    }

    #[test]
    fn rename_allows_changing_only_the_letter_case() {
        let temp = TempAssets::new();
        temp.write("a.png");
        assert_eq!(temp.assets.rename("a.png", "A.png").unwrap(), "A.png");
        assert!(temp.assets.root().join("A.png").is_file());
    }

    #[test]
    fn remove_deletes_files_and_folders_but_never_the_root() {
        let temp = TempAssets::new();
        temp.write("d/a.png");
        temp.write("b.png");
        temp.assets.remove("b.png").unwrap();
        temp.assets.remove("d").unwrap();
        assert!(temp.assets.list().unwrap().is_empty());
        assert!(temp.assets.remove("").is_err());
        assert!(temp.assets.root().is_dir());
        assert!(temp.assets.remove("../Assets").is_err());
    }

    #[test]
    fn import_copies_and_numbers_conflicting_names() {
        let temp = TempAssets::new();
        temp.write("d/photo.png");
        let source = temp.external("photo.png");
        assert_eq!(temp.assets.import(&source, "d").unwrap(), "d/photo (1).png");
        assert_eq!(temp.assets.import(&source, "d").unwrap(), "d/photo (2).png");
        assert_eq!(temp.assets.import(&source, "").unwrap(), "photo.png");
        assert_eq!(
            fs::read(temp.assets.root().join("d/photo (1).png")).unwrap(),
            b"external"
        );
        assert_eq!(
            fs::read(temp.assets.root().join("d/photo.png")).unwrap(),
            b"data"
        );
    }

    #[test]
    fn import_rejects_folders_and_missing_files() {
        let temp = TempAssets::new();
        assert!(temp.assets.import(&temp.base, "").is_err());
        assert!(temp.assets.import(&temp.base.join("none.png"), "").is_err());
        assert!(temp.assets.list().unwrap().is_empty());
    }
}
