//! Автосохранение несохранённой сцены в служебный каталог проекта и её восстановление.
//!
//! Копии лежат в `<проект>/.rve/editor/autosave/v2/`: вне `Assets/`, поэтому не попадают
//! в ресурсы и сборку, а `.rve/` исключён из Git. Копия удаляется после Save и после отказа
//! от изменений; оставшаяся копия означает, что редактор завершился без сохранения.
//!
//! У сцены два слота `<ключ>.a.autosave` и `<ключ>.b.autosave`. Первая строка слота — заголовок
//! с путём, порядковым номером `seq` и контрольной суммой тела; новее слот с большим `seq`.
//! Прежняя схема (`autosave/<путь>.autosave` без заголовка) читается как поколение с `seq` 0
//! и удаляется, когда оба слота исправны.

use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Как часто записывать копию изменённой сцены.
pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
/// Имя копии несохранённой сцены в прежней схеме; реальный virtual path не начинается с `~`.
const UNTITLED: &str = "~untitled";
/// Ключ копии несохранённой сцены в новой схеме.
const UNTITLED_KEY: &str = "untitled";
/// Начало первой строки слота; строка остаётся комментарием TOML.
const HEADER_PREFIX: &str = "# rve-autosave ";
/// Буквы слотов сцены.
const SLOTS: [&str; 2] = ["a", "b"];

/// Нужно ли записать копию: сцена изменена после последней копии и интервал истёк.
pub fn autosave_due(
    dirty: bool,
    revision: u64,
    autosaved_revision: Option<u64>,
    since_last: Duration,
) -> bool {
    dirty && autosaved_revision != Some(revision) && since_last >= AUTOSAVE_INTERVAL
}

/// Исправная копия сцены.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutosaveCopy {
    /// Текст сцены без строки заголовка.
    pub contents: String,
    pub saved_at: SystemTime,
}

/// Найденное для одной сцены: до двух исправных копий, новейшая первой, и ошибки чтения.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct AutosaveCopies {
    pub copies: Vec<AutosaveCopy>,
    pub errors: Vec<String>,
}

/// Исправное поколение из слота или прежнего файла.
struct Generation {
    seq: u64,
    copy: AutosaveCopy,
}

/// Заголовок слота: первая строка файла после `HEADER_PREFIX`.
#[derive(Serialize, Deserialize)]
struct Header {
    /// Путь сцены; `null` — несохранённая новая сцена.
    path: Option<String>,
    seq: u64,
    /// Время записи, секунды от начала UNIX-эпохи.
    saved_at: u64,
    /// Контрольная сумма blake3 тела в шестнадцатеричной записи.
    blake3: String,
}

/// Копии одного проекта.
pub struct AutosaveStore {
    /// `<проект>/.rve/editor/autosave/v2`.
    dir: PathBuf,
    /// Каталог прежней схемы.
    legacy_dir: PathBuf,
}

impl AutosaveStore {
    pub fn for_project(root: &Path) -> Self {
        let legacy_dir = root.join(".rve").join("editor").join("autosave");
        Self {
            dir: legacy_dir.join("v2"),
            legacy_dir,
        }
    }

    /// Ключ файлов сцены: первые 32 hex-символа blake3 пути. Имя фиксированной длины для любого пути.
    fn key(scene: Option<&VirtualPath>) -> String {
        scene.map_or_else(
            || UNTITLED_KEY.to_owned(),
            |path| blake3::hash(path.as_bytes()).to_hex()[..32].to_owned(),
        )
    }

    fn slot(&self, scene: Option<&VirtualPath>, letter: &str) -> PathBuf {
        self.dir
            .join(format!("{}.{letter}.autosave", Self::key(scene)))
    }

    fn lock_path(&self, scene: Option<&VirtualPath>) -> PathBuf {
        self.dir.join(format!("{}.lock", Self::key(scene)))
    }

    /// Файл прежней схемы. `~` и `/` пути экранируются как `~7E` и `~2F`, поэтому разные
    /// сцены не получают одно имя, а `~untitled` не совпадает ни с одним путём.
    fn legacy_file(&self, scene: Option<&VirtualPath>) -> PathBuf {
        let name = scene.map_or_else(
            || UNTITLED.to_owned(),
            |path| path.as_str().replace('~', "~7E").replace('/', "~2F"),
        );
        self.legacy_dir.join(format!("{name}.autosave"))
    }

    /// Записывает текст сцены в слот с меньшим `seq` или в неисправный слот. Возвращает
    /// предупреждения, при которых запись всё же состоялась.
    pub fn write(
        &self,
        scene: Option<&VirtualPath>,
        contents: &str,
    ) -> Result<Vec<String>, String> {
        self.write_locked(scene, contents, || {})
    }

    /// `before_publish` вызывается внутри критической секции, перед публикацией слота.
    fn write_locked(
        &self,
        scene: Option<&VirtualPath>,
        contents: &str,
        before_publish: impl FnOnce(),
    ) -> Result<Vec<String>, String> {
        let (_lock, mut warnings) = self.lock(scene)?;
        // Неисправный слот заменяется; причину покажет чтение.
        let [a, b] = SLOTS.map(|letter| self.read_slot(scene, letter).ok().flatten());
        let target = match (&a, &b) {
            (Some(a), Some(b)) if b.seq < a.seq => "b",
            (Some(_), Some(_)) | (None, _) => "a",
            (Some(_), None) => "b",
        };
        let other_valid = if target == "a" {
            b.is_some()
        } else {
            a.is_some()
        };
        let seq = [&a, &b]
            .into_iter()
            .flatten()
            .map(|generation| generation.seq)
            .max()
            .unwrap_or(0)
            + 1;

        let temporary = self.temporary(scene);
        if let Err(error) = std::fs::write(&temporary, encode(scene, seq, contents)) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("{}: {error}", temporary.display()));
        }
        before_publish();
        let published = self.slot(scene, target);
        if let Err(error) = std::fs::rename(&temporary, &published) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("{}: {error}", published.display()));
        }

        // Оба слота исправны: прежний файл больше не нужен. Неудача только предупреждает.
        if other_valid {
            let legacy = self.legacy_file(scene);
            match std::fs::remove_file(&legacy) {
                Ok(()) => {}
                Err(error) if is_missing(&error) => {}
                Err(error) => warnings.push(format!("{}: {error}", legacy.display())),
            }
        }
        Ok(warnings)
    }

    /// Копии сцены: до двух исправных поколений, новейшее первым. Ошибки чтения всех файлов
    /// попадают в `errors` с путём файла.
    pub fn read(&self, scene: Option<&VirtualPath>) -> AutosaveCopies {
        let mut found = AutosaveCopies::default();
        let mut generations = Vec::new();
        let results = [
            self.read_slot(scene, "a"),
            self.read_slot(scene, "b"),
            self.read_legacy(scene),
        ];
        for result in results {
            match result {
                Ok(Some(generation)) => generations.push(generation),
                Ok(None) => {}
                Err(error) => found.errors.push(error),
            }
        }
        generations.sort_by_key(|generation| std::cmp::Reverse(generation.seq));
        found.copies = generations
            .into_iter()
            .take(2)
            .map(|generation| generation.copy)
            .collect();
        found
    }

    /// Удаляет оба слота и прежний файл; отсутствие файла не ошибка. Первая другая ошибка
    /// возвращается после попытки удалить остальные файлы.
    pub fn remove(&self, scene: Option<&VirtualPath>) -> Result<(), String> {
        let (_lock, _) = self.lock(scene)?;
        let mut first_error = None;
        for file in [
            self.slot(scene, "a"),
            self.slot(scene, "b"),
            self.legacy_file(scene),
        ] {
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if is_missing(&error) => {}
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(format!("{}: {error}", file.display()));
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Открывает `.lock` сцены и берёт эксклюзивную блокировку. Файловая система без блокировок
    /// не мешает записи: тогда возвращается предупреждение. Остальные ошибки прерывают запись.
    fn lock(&self, scene: Option<&VirtualPath>) -> Result<(File, Vec<String>), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("{}: {error}", self.dir.display()))?;
        let path = self.lock_path(scene);
        // Файл блокировки пуст: его содержимое не используется, поэтому не обрезается.
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        match file.lock() {
            Ok(()) => Ok((file, Vec::new())),
            Err(error) if error.kind() == ErrorKind::Unsupported => {
                let warning = format!(
                    "{}: блокировка недоступна, запись без неё: {error}",
                    path.display()
                );
                Ok((file, vec![warning]))
            }
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Уникальный временный файл слота: процесс и счётчик.
    fn temporary(&self, scene: Option<&VirtualPath>) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        self.dir.join(format!(
            "{}.{}-{}.tmp",
            Self::key(scene),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Слот со строкой заголовка. Отсутствующий слот — `Ok(None)`; любая проверка, которая
    /// не прошла, — `Err` с путём файла.
    fn read_slot(
        &self,
        scene: Option<&VirtualPath>,
        letter: &str,
    ) -> Result<Option<Generation>, String> {
        let file = self.slot(scene, letter);
        let Some(text) = read_text(&file)? else {
            return Ok(None);
        };
        let invalid = |reason: &str| format!("{}: {reason}", file.display());
        let (line, body) = text
            .split_once('\n')
            .ok_or_else(|| invalid("нет строки заголовка"))?;
        let header: Header = line
            .strip_prefix(HEADER_PREFIX)
            .and_then(|json| serde_json::from_str(json).ok())
            .ok_or_else(|| invalid("заголовок не разобран"))?;
        if header.path.as_deref() != scene.map(VirtualPath::as_str) {
            return Err(invalid("копия другой сцены"));
        }
        if blake3::hash(body.as_bytes()).to_hex().as_str() != header.blake3 {
            return Err(invalid("контрольная сумма тела не совпадает"));
        }
        Ok(Some(Generation {
            seq: header.seq,
            copy: AutosaveCopy {
                contents: body.to_owned(),
                saved_at: UNIX_EPOCH + Duration::from_secs(header.saved_at),
            },
        }))
    }

    /// Файл прежней схемы: поколение без заголовка с `seq` 0 и временем из mtime.
    fn read_legacy(&self, scene: Option<&VirtualPath>) -> Result<Option<Generation>, String> {
        let file = self.legacy_file(scene);
        let Some(contents) = read_text(&file)? else {
            return Ok(None);
        };
        let saved_at = std::fs::metadata(&file)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| format!("{}: {error}", file.display()))?;
        Ok(Some(Generation {
            seq: 0,
            copy: AutosaveCopy { contents, saved_at },
        }))
    }
}

/// Текст слота: строка заголовка и тело.
fn encode(scene: Option<&VirtualPath>, seq: u64, contents: &str) -> String {
    let header = Header {
        path: scene.map(|path| path.as_str().to_owned()),
        seq,
        // Часы до 1970 года дают 0: копия всё равно записывается, а возраст считается старым.
        saved_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |age| age.as_secs()),
        blake3: blake3::hash(contents.as_bytes()).to_hex().to_string(),
    };
    // Структура из строк и чисел сериализуется всегда.
    let json = serde_json::to_string(&header).expect("заголовок копии сериализуется");
    format!("{HEADER_PREFIX}{json}\n{contents}")
}

/// Текст файла. Отсутствие файла — `None`. Имя, которое файловая система не может создать
/// (прежняя схема для длинного пути), тоже означает отсутствие файла.
fn read_text(file: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(text)),
        Err(error) if is_missing(&error) => Ok(None),
        Err(error) => Err(format!("{}: {error}", file.display())),
    }
}

fn is_missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::InvalidFilename
    )
}

/// Возраст копии для окна восстановления: «только что», «N с/мин/ч/дн назад». Время в будущем —
/// «только что».
pub fn age_text(saved_at: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(saved_at).map_or(0, |age| age.as_secs());
    match seconds {
        0 => "только что".to_owned(),
        1..60 => format!("{seconds} с назад"),
        60..3600 => format!("{} мин назад", seconds / 60),
        3600..86400 => format!("{} ч назад", seconds / 3600),
        _ => format!("{} дн назад", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::{EditorScene, NewObject};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            Self(std::env::temp_dir().join(format!(
                "rve-editor-autosave-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    /// Текст самой новой исправной копии.
    fn latest(store: &AutosaveStore, scene: Option<&VirtualPath>) -> Option<String> {
        store
            .read(scene)
            .copies
            .into_iter()
            .next()
            .map(|copy| copy.contents)
    }

    /// Тексты всех найденных копий, новейшая первой.
    fn texts(store: &AutosaveStore, scene: Option<&VirtualPath>) -> Vec<String> {
        store
            .read(scene)
            .copies
            .into_iter()
            .map(|copy| copy.contents)
            .collect()
    }

    #[test]
    fn copies_are_written_found_and_removed_per_scene() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let mut scene = EditorScene::new_untitled().unwrap();
        scene.create_object(&NewObject::Empty, None).unwrap();
        let text = scene.document().to_toml().unwrap();
        let nested = path("/scenes/a.rvescene");
        store.write(Some(&nested), &text).unwrap();
        store.write(None, "untitled").unwrap();

        assert_eq!(
            latest(&store, Some(&nested)).as_deref(),
            Some(text.as_str())
        );
        assert_eq!(latest(&store, None).as_deref(), Some("untitled"));
        assert_eq!(latest(&store, Some(&path("/scenes~a.rvescene"))), None);
        assert!(project.0.join(".rve/editor/autosave/v2").is_dir());

        let restored = EditorScene::recover(Some(nested.clone()), &text).unwrap();
        assert!(restored.is_dirty());
        assert_eq!(restored.path(), Some(&nested));
        assert_eq!(restored.document().objects.len(), 3);

        store.remove(Some(&nested)).unwrap();
        store.remove(Some(&nested)).unwrap();
        assert_eq!(latest(&store, Some(&nested)), None);
        assert_eq!(latest(&store, None).as_deref(), Some("untitled"));
    }

    #[test]
    fn copy_is_due_only_for_new_changes_after_the_interval() {
        let late = AUTOSAVE_INTERVAL;
        let early = AUTOSAVE_INTERVAL - Duration::from_secs(1);
        assert!(autosave_due(true, 3, None, late));
        assert!(autosave_due(true, 3, Some(2), late));
        assert!(!autosave_due(true, 3, Some(3), late));
        assert!(!autosave_due(true, 3, None, early));
        assert!(!autosave_due(false, 3, None, late));
    }

    #[test]
    fn missing_copy_is_neither_a_copy_nor_an_error() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        assert_eq!(store.read(None), AutosaveCopies::default());
        assert_eq!(read_text(&project.0.join("missing.autosave")), Ok(None));
    }

    #[test]
    fn unreadable_copy_is_an_error_with_its_file_path() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let file = store.slot(None, "a");
        fs_create_parent(&file);

        std::fs::create_dir(&file).unwrap();
        let found = store.read(None);
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&file.display().to_string()));

        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, [0xff, 0xfe]).unwrap();
        let found = store.read(None);
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&file.display().to_string()));
    }

    fn fs_create_parent(file: &Path) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    }

    #[test]
    fn long_scene_paths_get_short_file_names_and_no_legacy_error() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let long = path(
            &(0..40)
                .map(|_| "c".repeat(30))
                .collect::<Vec<_>>()
                .join("/"),
        );
        store.write(Some(&long), "long").unwrap();

        assert_eq!(latest(&store, Some(&long)).as_deref(), Some("long"));
        assert!(store.read(Some(&long)).errors.is_empty());
        for entry in std::fs::read_dir(&store.dir).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(name.to_string_lossy().len() <= 50, "{name:?}");
        }
        store.remove(Some(&long)).unwrap();
        assert_eq!(latest(&store, Some(&long)), None);
        assert!(store.read(Some(&long)).errors.is_empty());
    }

    #[test]
    fn different_scenes_and_untitled_get_different_files() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let a = path("/scenes/a.rvescene");
        let b = path("/scenes/b.rvescene");
        assert_ne!(store.slot(Some(&a), "a"), store.slot(Some(&b), "a"));
        assert_ne!(store.slot(Some(&a), "a"), store.slot(None, "a"));

        store.write(Some(&path("untitled")), "named").unwrap();
        assert_eq!(latest(&store, None), None);
        assert_eq!(
            latest(&store, Some(&path("untitled"))).as_deref(),
            Some("named")
        );
    }

    #[test]
    fn header_with_a_line_break_in_the_path_round_trips() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let odd = path("scenes/a\nb.rvescene");
        store.write(Some(&odd), "text\nwith lines").unwrap();

        assert_eq!(
            latest(&store, Some(&odd)).as_deref(),
            Some("text\nwith lines")
        );
        assert!(store.read(Some(&odd)).errors.is_empty());
    }

    #[test]
    fn slot_of_another_scene_is_an_error_with_its_file_path() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let mine = path("/scenes/a.rvescene");
        let other = path("/scenes/b.rvescene");
        store.write(Some(&other), "other").unwrap();
        std::fs::copy(store.slot(Some(&other), "a"), store.slot(Some(&mine), "a")).unwrap();

        let found = store.read(Some(&mine));
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&store.slot(Some(&mine), "a").display().to_string()));
    }

    #[test]
    fn slot_without_header_is_an_error_with_its_file_path() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        fs_create_parent(&store.slot(None, "a"));
        std::fs::write(store.slot(None, "a"), "untitled without header").unwrap();

        let found = store.read(None);
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&store.slot(None, "a").display().to_string()));
    }

    #[test]
    fn body_that_does_not_match_its_checksum_is_an_error() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        store.write(None, "first").unwrap();
        let slot = store.slot(None, "a");
        let text = std::fs::read_to_string(&slot).unwrap();
        std::fs::write(&slot, text.replace("first", "tampered")).unwrap();

        let found = store.read(None);
        assert!(found.copies.is_empty());
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].contains("контрольная сумма"));
    }

    #[test]
    fn legacy_copies_are_read_and_restored_with_their_mtime() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let mut scene = EditorScene::new_untitled().unwrap();
        scene.create_object(&NewObject::Empty, None).unwrap();
        let text = scene.document().to_toml().unwrap();
        let nested = path("/scenes/a.rvescene");

        std::fs::create_dir_all(&store.legacy_dir).unwrap();
        let legacy = store.legacy_file(Some(&nested));
        assert_eq!(legacy.file_name().unwrap(), "scenes~2Fa.rvescene.autosave");
        std::fs::write(&legacy, &text).unwrap();
        std::fs::write(store.legacy_file(None), "legacy untitled").unwrap();

        let found = store.read(Some(&nested));
        assert!(found.errors.is_empty());
        assert_eq!(found.copies.len(), 1);
        assert_eq!(found.copies[0].contents, text);
        assert_eq!(
            found.copies[0].saved_at,
            std::fs::metadata(&legacy).unwrap().modified().unwrap()
        );
        let restored =
            EditorScene::recover(Some(nested.clone()), &found.copies[0].contents).unwrap();
        assert_eq!(restored.path(), Some(&nested));
        assert_eq!(restored.document().objects.len(), 3);

        assert_eq!(texts(&store, None), ["legacy untitled"]);
    }

    #[test]
    fn first_write_keeps_legacy_as_previous_generation_and_second_removes_it() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let scene = path("/scenes/a.rvescene");
        std::fs::create_dir_all(&store.legacy_dir).unwrap();
        std::fs::write(store.legacy_file(Some(&scene)), "legacy").unwrap();

        assert!(store.write(Some(&scene), "first").unwrap().is_empty());
        assert_eq!(texts(&store, Some(&scene)), ["first", "legacy"]);
        assert!(store.legacy_file(Some(&scene)).exists());

        store.write(Some(&scene), "second").unwrap();
        assert_eq!(texts(&store, Some(&scene)), ["second", "first"]);
        assert!(!store.legacy_file(Some(&scene)).exists());
    }

    #[test]
    fn legacy_copy_named_like_an_untitled_slot_is_not_touched_by_untitled_scene() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let named = path("untitled.a");
        std::fs::create_dir_all(&store.legacy_dir).unwrap();
        std::fs::write(store.legacy_file(Some(&named)), "legacy a").unwrap();

        store.write(None, "new untitled").unwrap();
        store.remove(None).unwrap();

        assert!(store.legacy_file(Some(&named)).exists());
        assert_eq!(texts(&store, Some(&named)), ["legacy a"]);
    }

    #[test]
    fn remove_deletes_both_slots_and_the_legacy_file() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let scene = path("/scenes/a.rvescene");
        std::fs::create_dir_all(&store.legacy_dir).unwrap();
        std::fs::write(store.legacy_file(Some(&scene)), "legacy").unwrap();
        store.write(Some(&scene), "first").unwrap();
        store.write(Some(&scene), "second").unwrap();
        std::fs::write(store.legacy_file(Some(&scene)), "legacy again").unwrap();

        store.remove(Some(&scene)).unwrap();

        assert_eq!(store.read(Some(&scene)), AutosaveCopies::default());
        assert!(!store.slot(Some(&scene), "a").exists());
        assert!(!store.slot(Some(&scene), "b").exists());
        assert!(!store.legacy_file(Some(&scene)).exists());
    }

    #[test]
    fn invalid_slot_is_replaced_first_and_the_valid_previous_generation_is_kept() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        store.write(None, "first").unwrap();
        store.write(None, "second").unwrap();
        let newest = store.slot(None, "b");
        let text = std::fs::read_to_string(&newest).unwrap();
        std::fs::write(&newest, text.replace("second", "tampered")).unwrap();

        let found = store.read(None);
        assert_eq!(found.copies.len(), 1);
        assert_eq!(found.errors.len(), 1);

        store.write(None, "third").unwrap();
        assert_eq!(texts(&store, None), ["third", "first"]);
        assert!(store.read(None).errors.is_empty());
    }

    #[test]
    fn failed_publish_keeps_the_newest_generation_and_leaves_no_temporary_file() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        store.write(None, "first").unwrap();
        store.write(None, "second").unwrap();

        // Следующая запись выберет слот `a` с меньшим `seq`; каталог на его месте не даёт опубликовать копию.
        let older = store.slot(None, "a");
        std::fs::remove_file(&older).unwrap();
        std::fs::create_dir(&older).unwrap();
        assert!(store.write(None, "third").is_err());

        assert_eq!(texts(&store, None), ["second"]);
        let leftovers = std::fs::read_dir(&store.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);

        std::fs::remove_dir(&older).unwrap();
        store.write(None, "third").unwrap();
        assert_eq!(texts(&store, None), ["third", "second"]);
    }

    #[test]
    fn write_holds_the_lock_of_its_scene_until_it_publishes() {
        use std::fs::TryLockError;

        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let lock = store.lock_path(None);
        store
            .write_locked(None, "locked", || {
                let other = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock)
                    .unwrap();
                assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));
            })
            .unwrap();

        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        assert!(other.try_lock().is_ok());
    }

    #[test]
    fn legacy_file_that_cannot_be_removed_is_a_warning_and_the_write_succeeds() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let scene = path("/scenes/a.rvescene");
        assert!(store.write(Some(&scene), "first").unwrap().is_empty());
        // Каталог на месте прежнего файла: `remove_file` его не удалит.
        let legacy = store.legacy_file(Some(&scene));
        std::fs::create_dir_all(&legacy).unwrap();

        let warnings = store.write(Some(&scene), "second").unwrap();

        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with(&legacy.display().to_string()));
        assert_eq!(texts(&store, Some(&scene)), ["second", "first"]);
    }

    #[test]
    fn third_write_replaces_the_oldest_of_two_kept_generations() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        store.write(None, "first").unwrap();
        store.write(None, "second").unwrap();
        assert_eq!(texts(&store, None), ["second", "first"]);

        store.write(None, "third").unwrap();
        assert_eq!(texts(&store, None), ["third", "second"]);
    }

    #[test]
    fn read_returns_the_readable_copies_and_reports_the_broken_slot() {
        let project = TempDir::new();
        let store = AutosaveStore::for_project(&project.0);
        let scene = path("/scenes/a.rvescene");
        std::fs::create_dir_all(&store.legacy_dir).unwrap();
        std::fs::write(store.legacy_file(Some(&scene)), "legacy").unwrap();
        store.write(Some(&scene), "first").unwrap();
        let broken = store.slot(Some(&scene), "b");
        std::fs::write(&broken, "без заголовка").unwrap();

        let found = store.read(Some(&scene));
        assert_eq!(
            found
                .copies
                .iter()
                .map(|copy| copy.contents.as_str())
                .collect::<Vec<_>>(),
            ["first", "legacy"]
        );
        assert_eq!(found.errors.len(), 1);
        assert!(found.errors[0].starts_with(&broken.display().to_string()));
    }

    #[test]
    fn age_text_is_relative_and_never_negative() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |seconds: u64| now - Duration::from_secs(seconds);
        assert_eq!(age_text(now, now), "только что");
        assert_eq!(age_text(now + Duration::from_secs(60), now), "только что");
        assert_eq!(age_text(ago(30), now), "30 с назад");
        assert_eq!(age_text(ago(90), now), "1 мин назад");
        assert_eq!(age_text(ago(7_200), now), "2 ч назад");
        assert_eq!(age_text(ago(3 * 86_400), now), "3 дн назад");
    }
}
