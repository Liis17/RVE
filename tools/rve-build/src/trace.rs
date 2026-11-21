//! Трассировка сборки для `rve-build explain`: рёбра ссылок, источники явных архивов и причины
//! назначения. Заполняется только при `prepare_project_traced`; обычная сборка её не получает.

use crate::project::Owner;
use engine_files::VirtualPath;
use engine_scene::SceneDocument;
use std::collections::BTreeMap;

/// Вид ссылки от документа или ресурса к другому файлу.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EdgeKind {
    /// Объявлен в `[[assets]]` документа.
    Declared,
    /// Экземпляр префаба из `[[instances]]`.
    Instance,
    /// Зависимость ресурса; текст называет её вид, например «текстура FBX».
    Dependency(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Edge {
    pub(crate) to: VirtualPath,
    pub(crate) kind: EdgeKind,
}

/// Документ или запись списка, которые фактически задали явный архив ресурса.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveSource {
    /// `[[assets]]` сцены или префаба с этим путём.
    Document(VirtualPath),
    /// Запись `prefabs.rveprefabs` для тяжёлого префаба с этим путём.
    PrefabList(VirtualPath),
}

/// Ветка `assign_archives`, которая выбрала архив ресурса.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveReason {
    /// Файл включённой сцены всегда лежит в `base`.
    SceneFile,
    /// AppIcon из `bootstrap.rves` всегда лежит в `base`.
    AppIcon,
    /// Таблица строк языка из `[localization]` проекта всегда лежит в `base`.
    StringTable { language: String },
    /// Lua-мигратор `[save] migration` всегда лежит в `base`.
    SaveMigration,
    /// Явный архив, заданный владельцем; источник ищется в трассе по владельцу и имени архива.
    Explicit { owner: Owner, archive: String },
    /// Явные архивы тяжёлых префабов расходятся: ресурс уходит в `shared`.
    ConflictingExplicit { assignments: Vec<(Owner, String)> },
    /// Ресурс используется одной сценой: архив `scene-<stem>`.
    SingleScene { scene: VirtualPath },
    /// Ресурс используют несколько сцен: архив `shared`.
    SharedScenes { scenes: Vec<VirtualPath> },
    /// Ресурс не используется сценами, только тяжёлыми префабами без явного архива.
    NoScene,
}

#[derive(Debug, Default)]
pub(crate) struct BuildTrace {
    /// Корни обхода: включённые сцены и записи `prefabs.rveprefabs` с их владельцами.
    pub(crate) roots: Vec<(Owner, VirtualPath)>,
    /// AppIcon из `bootstrap.rves`, если задан.
    pub(crate) icon: Option<VirtualPath>,
    /// Таблицы строк `[localization]`: язык и путь.
    pub(crate) string_tables: Vec<(String, VirtualPath)>,
    /// Lua-мигратор сохранений `[save] migration`, если задан.
    pub(crate) save_migration: Option<VirtualPath>,
    pub(crate) reasons: BTreeMap<VirtualPath, ArchiveReason>,
    /// Рёбра по владельцу контекста раскрытия: одна и та же сцена или префаб может раскрываться
    /// по-разному в разных контекстах, поэтому обход идёт только по рёбрам своего владельца.
    edges: BTreeMap<Owner, BTreeMap<VirtualPath, Vec<Edge>>>,
    /// Источник явного архива по ресурсу, владельцу и имени архива. Первая запись побеждает:
    /// документ сцены записывается раньше своих префабов, как и в слиянии `[[assets]]`.
    /// Запись списка тяжёлого префаба перезаписывает запись документа.
    explicit: BTreeMap<(VirtualPath, Owner, String), ArchiveSource>,
}

impl BuildTrace {
    pub(crate) fn add_root(&mut self, owner: Owner, path: VirtualPath) {
        self.roots.push((owner, path));
    }

    /// Записывает `[[assets]]` и экземпляры документа `path`, который раскрывает `owner`.
    pub(crate) fn record_document(
        &mut self,
        owner: &Owner,
        path: &VirtualPath,
        document: &SceneDocument,
    ) {
        for asset in &document.assets {
            self.push_edge(owner, path, asset.path.clone(), EdgeKind::Declared);
            if let Some(archive) = &asset.archive {
                self.explicit
                    .entry((asset.path.clone(), owner.clone(), archive.clone()))
                    .or_insert_with(|| ArchiveSource::Document(path.clone()));
            }
        }
        for instance in &document.instances {
            self.push_edge(owner, path, instance.prefab.clone(), EdgeKind::Instance);
        }
    }

    pub(crate) fn add_dependency(
        &mut self,
        owner: &Owner,
        from: &VirtualPath,
        to: &VirtualPath,
        label: &'static str,
    ) {
        self.push_edge(owner, from, to.clone(), EdgeKind::Dependency(label));
    }

    /// Запись списка тяжёлых префабов задаёт явный архив файлу префаба и его зависимостям.
    /// Для сцены записи списка не бывает, поэтому она игнорируется.
    pub(crate) fn record_list_archive(
        &mut self,
        asset: &VirtualPath,
        owner: &Owner,
        archive: &str,
    ) {
        if let Owner::HeavyPrefab(prefab) = owner {
            self.explicit.insert(
                (asset.clone(), owner.clone(), archive.to_owned()),
                ArchiveSource::PrefabList(prefab.clone()),
            );
        }
    }

    pub(crate) fn edges_from(&self, owner: &Owner, node: &VirtualPath) -> &[Edge] {
        self.edges
            .get(owner)
            .and_then(|nodes| nodes.get(node))
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn archive_source(
        &self,
        asset: &VirtualPath,
        owner: &Owner,
        archive: &str,
    ) -> Option<&ArchiveSource> {
        self.explicit
            .get(&(asset.clone(), owner.clone(), archive.to_owned()))
    }

    fn push_edge(&mut self, owner: &Owner, from: &VirtualPath, to: VirtualPath, kind: EdgeKind) {
        let edges = self
            .edges
            .entry(owner.clone())
            .or_default()
            .entry(from.clone())
            .or_default();
        let edge = Edge { to, kind };
        if !edges.contains(&edge) {
            edges.push(edge);
        }
    }
}
