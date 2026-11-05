//! Наблюдение за файлами ресурсов и объединение изменений.

use engine_files::VirtualPath;
use engine_window::WindowProxy;
use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(250);

/// Изменённые ресурсы и признак обновления дерева.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetChanges {
    pub paths: HashSet<VirtualPath>,
    pub tree_changed: bool,
    pub rescan: bool,
}

impl AssetChanges {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.tree_changed && !self.rescan
    }

    pub fn paths_with_extension(
        &self,
        extension: &'static str,
    ) -> impl Iterator<Item = &VirtualPath> {
        self.paths.iter().filter(move |path| {
            path.as_str()
                .rsplit('/')
                .next()
                .and_then(|name| name.rsplit_once('.'))
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(extension))
        })
    }

    pub fn fbx_paths(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths_with_extension("fbx")
    }

    pub fn png_paths(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths_with_extension("png")
    }

    pub fn hdr_paths(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths_with_extension("hdr")
    }

    pub fn prefab_paths(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths_with_extension("rveprefab")
    }

    pub fn lua_paths(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths_with_extension("lua")
    }

    pub fn camera_settings_paths(&self) -> impl Iterator<Item = &VirtualPath> {
        self.paths_with_extension("camerasettings")
    }

    /// Проверяет точное совпадение или событие каталога-родителя.
    pub fn contains_path_or_parent(&self, path: &VirtualPath) -> bool {
        self.paths.iter().any(|changed| {
            let changed = changed.as_str();
            path.as_str() == changed
                || path
                    .as_str()
                    .strip_prefix(changed)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    }

    fn extend(&mut self, mut other: Self) {
        self.paths.extend(other.paths.drain());
        self.tree_changed |= other.tree_changed;
        self.rescan |= other.rescan;
    }
}

/// Корни для преобразования абсолютных путей событий в virtual paths.
#[derive(Clone, Debug)]
struct WatchRoots {
    canonical: PathBuf,
    original: PathBuf,
}

impl WatchRoots {
    fn new(root: &Path) -> Result<Self, String> {
        let canonical =
            fs::canonicalize(root).map_err(|error| format!("{}: {error}", root.display()))?;
        Ok(Self {
            canonical,
            original: root.to_owned(),
        })
    }

    fn relative_path<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.canonical)
            .or_else(|_| path.strip_prefix(&self.original))
            .ok()
    }
}

/// Отложенный и агрегированный пакет изменений ресурсов.
#[derive(Default)]
pub struct ChangeBatch {
    changes: AssetChanges,
    deadline: Option<Instant>,
}

impl ChangeBatch {
    pub fn add(&mut self, changes: AssetChanges, now: Instant) {
        if changes.is_empty() {
            return;
        }
        self.changes.extend(changes);
        self.deadline = Some(now + DEBOUNCE);
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn take_ready(&mut self, now: Instant) -> Option<AssetChanges> {
        if !self.deadline.is_some_and(|deadline| now >= deadline) {
            return None;
        }
        self.deadline = None;
        Some(std::mem::take(&mut self.changes))
    }
}

/// Файловый watcher проекта. Callback только передаёт события и будит окно.
pub struct AssetWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
    roots: WatchRoots,
    disconnected: bool,
}

impl AssetWatcher {
    pub fn new(root: &Path, proxy: WindowProxy) -> Result<Self, String> {
        let roots = WatchRoots::new(root)?;
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            if sender.send(event).is_ok() {
                proxy.wake();
            }
        })
        .map_err(|error| format!("{}: {error}", root.display()))?;
        watcher
            .watch(&roots.canonical, RecursiveMode::Recursive)
            .map_err(|error| format!("{}: {error}", root.display()))?;
        Ok(Self {
            _watcher: watcher,
            receiver,
            roots,
            disconnected: false,
        })
    }

    /// Перекладывает накопившиеся события в debounce-пакет и возвращает ошибки watcher.
    pub fn drain(&mut self, batch: &mut ChangeBatch, now: Instant) -> Vec<String> {
        let mut errors = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(event) => {
                    let classified = classify(event, &self.roots);
                    batch.add(classified.changes, now);
                    if let Some(error) = classified.error {
                        errors.push(error);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) if !self.disconnected => {
                    self.disconnected = true;
                    batch.add(
                        AssetChanges {
                            rescan: true,
                            ..AssetChanges::default()
                        },
                        now,
                    );
                    errors.push("канал file watcher закрыт".to_owned());
                    break;
                }
                Err(TryRecvError::Disconnected) => break,
            }
        }
        errors
    }
}

struct ClassifiedEvent {
    changes: AssetChanges,
    error: Option<String>,
}

fn classify(event: notify::Result<Event>, roots: &WatchRoots) -> ClassifiedEvent {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            return ClassifiedEvent {
                changes: AssetChanges {
                    rescan: true,
                    ..AssetChanges::default()
                },
                error: Some(format!("ошибка file watcher: {error}")),
            };
        }
    };
    if event.need_rescan() {
        return ClassifiedEvent {
            changes: AssetChanges {
                rescan: true,
                ..AssetChanges::default()
            },
            error: None,
        };
    }

    let tree_event = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    let mut paths = HashSet::new();
    let mut inside_assets = false;
    let mut assets_root = false;
    for path in &event.paths {
        let Some(relative) = roots.relative_path(path) else {
            continue;
        };
        inside_assets = true;
        if relative.as_os_str().is_empty() {
            assets_root = true;
            continue;
        }
        let components = relative
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<Vec<_>>>();
        let Some(components) = components else {
            continue;
        };
        if let Ok(path) = VirtualPath::parse(&components.join("/")) {
            paths.insert(path);
        }
    }
    let changes = if inside_assets {
        AssetChanges {
            paths,
            tree_changed: assets_root || tree_event,
            rescan: false,
        }
    } else {
        AssetChanges::default()
    };
    ClassifiedEvent {
        changes,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rve-editor-watch-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn roots(&self) -> WatchRoots {
            WatchRoots::new(&self.0).unwrap()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn event(kind: EventKind, paths: &[&Path]) -> notify::Result<Event> {
        Ok(paths.iter().fold(Event::new(kind), |event, path| {
            event.add_path(path.to_path_buf())
        }))
    }

    fn changes(event: notify::Result<Event>, roots: &WatchRoots) -> AssetChanges {
        classify(event, roots).changes
    }

    #[test]
    fn paths_outside_assets_are_ignored() {
        let root = TempRoot::new();
        let result = changes(
            event(
                EventKind::Create(CreateKind::File),
                &[&root.0.parent().unwrap().join("elsewhere.png")],
            ),
            &root.roots(),
        );
        assert!(result.is_empty());
    }

    #[test]
    fn create_remove_and_rename_update_the_tree() {
        let root = TempRoot::new();
        let roots = root.roots();
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
        ] {
            let result = changes(event(kind, &[&root.0.join("models/a.fbx")]), &roots);
            assert!(result.tree_changed);
            assert!(
                result
                    .paths
                    .contains(&VirtualPath::parse("models/a.fbx").unwrap())
            );
        }
    }

    #[test]
    fn watcher_errors_and_rescan_flags_request_a_rescan() {
        let root = TempRoot::new();
        let roots = root.roots();
        let error = classify(Err(notify::Error::generic("watch failed")), &roots);
        assert!(error.changes.rescan);
        assert!(error.error.unwrap().contains("watch failed"));

        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(changes(Ok(event), &roots).rescan);
    }

    #[test]
    fn rename_keeps_both_paths_including_the_destination() {
        let root = TempRoot::new();
        let roots = root.roots();
        let result = changes(
            event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &[
                    &root.0.join("models/.ship.fbx.tmp"),
                    &root.0.join("models/ship.fbx"),
                ],
            ),
            &roots,
        );
        assert!(
            result
                .paths
                .contains(&VirtualPath::parse("models/.ship.fbx.tmp").unwrap())
        );
        assert!(
            result
                .paths
                .contains(&VirtualPath::parse("models/ship.fbx").unwrap())
        );
    }

    #[test]
    fn canonical_root_is_checked_before_the_original_root() {
        let root = TempRoot::new();
        let roots = WatchRoots {
            canonical: root.0.join("canonical/Assets"),
            original: root.0.join("alias/Assets"),
        };
        let result = changes(
            event(
                EventKind::Modify(ModifyKind::Any),
                &[&roots.canonical.join("textures/stone.png")],
            ),
            &roots,
        );
        assert!(
            result
                .paths
                .contains(&VirtualPath::parse("textures/stone.png").unwrap())
        );
    }

    #[test]
    fn directory_changes_match_nested_paths_only_at_component_boundaries() {
        let mut result = AssetChanges::default();
        result
            .paths
            .insert(VirtualPath::parse("textures/world").unwrap());
        assert!(
            result
                .contains_path_or_parent(&VirtualPath::parse("textures/world/stone.png").unwrap())
        );
        assert!(
            !result
                .contains_path_or_parent(&VirtualPath::parse("textures/world-old/a.png").unwrap())
        );
    }

    #[test]
    fn asset_extensions_are_matched_without_case_sensitivity() {
        let mut result = AssetChanges::default();
        result.paths.insert(VirtualPath::parse("ship.FBX").unwrap());
        result
            .paths
            .insert(VirtualPath::parse("albedo.PNG").unwrap());
        result
            .paths
            .insert(VirtualPath::parse("default.CAMERAsettings").unwrap());
        assert_eq!(result.fbx_paths().count(), 1);
        assert_eq!(result.png_paths().count(), 1);
        assert_eq!(result.camera_settings_paths().count(), 1);
    }

    #[test]
    fn change_batch_waits_for_quiet_time_and_resets_after_delivery() {
        let start = Instant::now();
        let mut batch = ChangeBatch::default();
        let mut first = AssetChanges::default();
        first.paths.insert(VirtualPath::parse("one.png").unwrap());
        batch.add(first, start);
        let deadline = start + DEBOUNCE;
        assert_eq!(batch.deadline(), Some(deadline));
        assert!(
            batch
                .take_ready(deadline - Duration::from_nanos(1))
                .is_none()
        );
        assert!(
            batch
                .take_ready(deadline)
                .unwrap()
                .paths
                .contains(&VirtualPath::parse("one.png").unwrap())
        );
        assert_eq!(batch.deadline(), None);
        assert!(batch.take_ready(deadline).is_none());
    }

    #[test]
    fn a_new_change_moves_the_debounce_deadline() {
        let start = Instant::now();
        let mut batch = ChangeBatch::default();
        let mut first = AssetChanges::default();
        first.paths.insert(VirtualPath::parse("one.png").unwrap());
        batch.add(first, start);
        let second_at = start + Duration::from_millis(200);
        let mut second = AssetChanges::default();
        second.paths.insert(VirtualPath::parse("two.png").unwrap());
        batch.add(second, second_at);
        assert_eq!(batch.deadline(), Some(second_at + DEBOUNCE));
        assert!(batch.take_ready(start + DEBOUNCE).is_none());
        let ready = batch.take_ready(second_at + DEBOUNCE).unwrap();
        assert_eq!(ready.paths.len(), 2);
    }
}
