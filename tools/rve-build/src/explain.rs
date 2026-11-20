//! `rve-build explain`: архив ресурса, причина выбора архива и кратчайшая цепочка ссылок
//! от каждого корня сборки, из которого ресурс достижим.

use crate::error::BuildError;
use crate::project::{Owner, prepare_project_traced};
use crate::trace::{ArchiveReason, ArchiveSource, BuildTrace, EdgeKind};
use engine_files::VirtualPath;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::Path;

/// Объяснение назначения одного ресурса.
pub(crate) struct Explanation {
    path: VirtualPath,
    archive: String,
    reason: String,
    chains: Vec<String>,
}

/// Проверяет проект тем же разбором, что и `build`, и объясняет назначение `path`.
/// Ресурс, которого нет в сборке, — ошибка проекта.
pub(crate) fn explain_asset(
    project_dir: &Path,
    path: &VirtualPath,
) -> Result<Explanation, BuildError> {
    let (project, trace) = prepare_project_traced(project_dir)?;
    let archive = project
        .assignments
        .get(path)
        .ok_or_else(|| BuildError::input(format!("ресурс /{path} не входит в сборку")))?;
    let reason = trace
        .reasons
        .get(path)
        .expect("every assigned path has an archive reason");

    let mut chains = Vec::new();
    for (owner, root) in &trace.roots {
        if let Some(steps) = shortest_chain(&trace, owner, root, path) {
            chains.push(render_chain(root, &steps));
        }
    }
    if trace.icon.as_ref() == Some(path) {
        chains.push(format!("bootstrap.rves → /{path} (AppIcon)"));
    }

    Ok(Explanation {
        path: path.clone(),
        archive: archive.clone(),
        reason: reason_text(&trace, path, reason),
        chains,
    })
}

/// Кратчайший путь по рёбрам владельца `owner` от `start` до `target`: для каждого шага — узел и
/// вид ребра, которым он достигнут. `None`, если цель не достижима из этого корня.
fn shortest_chain(
    trace: &BuildTrace,
    owner: &Owner,
    start: &VirtualPath,
    target: &VirtualPath,
) -> Option<Vec<(VirtualPath, EdgeKind)>> {
    if start == target {
        return Some(Vec::new());
    }
    let mut came_from = BTreeMap::<VirtualPath, (VirtualPath, EdgeKind)>::new();
    let mut seen = BTreeSet::from([start.clone()]);
    let mut queue = VecDeque::from([start.clone()]);
    while let Some(node) = queue.pop_front() {
        for edge in trace.edges_from(owner, &node) {
            if seen.insert(edge.to.clone()) {
                came_from.insert(edge.to.clone(), (node.clone(), edge.kind.clone()));
                queue.push_back(edge.to.clone());
            }
        }
    }
    if !came_from.contains_key(target) {
        return None;
    }
    let mut steps = Vec::new();
    let mut node = target.clone();
    while node != *start {
        let (previous, kind) = came_from[&node].clone();
        steps.push((node, kind));
        node = previous;
    }
    steps.reverse();
    Some(steps)
}

fn render_chain(start: &VirtualPath, steps: &[(VirtualPath, EdgeKind)]) -> String {
    let mut text = node_label(start);
    if steps.is_empty() {
        text.push_str(" (корень)");
    }
    for (node, kind) in steps {
        text.push_str(&format!(" → {} ({})", node_label(node), kind_label(kind)));
    }
    text
}

fn node_label(path: &VirtualPath) -> String {
    match extension(path) {
        Some("rvescene") => format!("сцена /{path}"),
        Some("rveprefab") => format!("префаб /{path}"),
        _ => format!("/{path}"),
    }
}

fn kind_label(kind: &EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Declared => "[[assets]]",
        EdgeKind::Instance => "экземпляр префаба",
        EdgeKind::Dependency(label) => label,
    }
}

fn extension(path: &VirtualPath) -> Option<&str> {
    Path::new(path.as_str())
        .extension()
        .and_then(|extension| extension.to_str())
}

fn owner_label(trace: &BuildTrace, owner: &Owner) -> String {
    match owner {
        Owner::Scene(_) => trace
            .roots
            .iter()
            .find(|(root, _)| root == owner)
            .map_or_else(|| "сцена".to_owned(), |(_, path)| node_label(path)),
        Owner::HeavyPrefab(path) => format!("тяжёлый префаб /{path}"),
    }
}

fn reason_text(trace: &BuildTrace, path: &VirtualPath, reason: &ArchiveReason) -> String {
    match reason {
        ArchiveReason::SceneFile => "файл включённой сцены всегда лежит в base".to_owned(),
        ArchiveReason::AppIcon => "AppIcon всегда лежит в base".to_owned(),
        ArchiveReason::Explicit { owner, archive } => {
            match trace.archive_source(path, owner, archive) {
                Some(ArchiveSource::Document(document))
                    if extension(document) == Some("rvescene") =>
                {
                    format!("явный archive {archive:?} в [[assets]] сцены /{document}")
                }
                Some(ArchiveSource::Document(document)) => {
                    format!("явный archive {archive:?} в [[assets]] префаба /{document}")
                }
                Some(ArchiveSource::PrefabList(prefab)) => format!(
                    "явный archive {archive:?} записи prefabs.rveprefabs для тяжёлого префаба /{prefab}"
                ),
                None => format!("явный archive {archive:?}"),
            }
        }
        ArchiveReason::ConflictingExplicit { assignments } => {
            // Одна зависимость может быть достижима по нескольким путям: пары повторяются.
            let mut unique = Vec::new();
            for pair in assignments {
                if !unique.contains(&pair) {
                    unique.push(pair);
                }
            }
            let list = unique
                .iter()
                .map(|(owner, archive)| {
                    let label = owner_label(trace, owner);
                    match trace.archive_source(path, owner, archive) {
                        Some(ArchiveSource::PrefabList(_)) => {
                            format!("{archive:?} ({label}, запись prefabs.rveprefabs)")
                        }
                        Some(ArchiveSource::Document(document)) => {
                            format!("{archive:?} ({label}, [[assets]] /{document})")
                        }
                        None => format!("{archive:?} ({label})"),
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("явные архивы расходятся, ресурс уходит в shared: {list}")
        }
        ArchiveReason::SingleScene { scene } => {
            format!("ресурс используется одной сценой: /{scene}")
        }
        ArchiveReason::SharedScenes { scenes } => {
            let list = scenes
                .iter()
                .map(|scene| format!("/{scene}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ресурс используют несколько сцен: {list}")
        }
        ArchiveReason::NoScene => {
            "ресурс не используется включёнными сценами, только тяжёлыми префабами без archive"
                .to_owned()
        }
    }
}

impl fmt::Display for Explanation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "/{} → архив {}", self.path, self.archive)?;
        writeln!(formatter, "причина: {}", self.reason)?;
        if self.chains.is_empty() {
            writeln!(formatter, "цепочки: нет")?;
        } else {
            writeln!(formatter, "цепочки:")?;
            for chain in &self.chains {
                writeln!(formatter, "  {chain}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::explain_asset;
    use crate::archives::tests::create_demo_like_project;
    use crate::project::{prepare_project, prepare_project_traced};
    use crate::temporary::TempDir;
    use engine_files::VirtualPath;
    use std::fs;
    use std::path::Path;

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    const INSTANCE_CRATE: &str = "[[assets]]\npath = \"/prefabs/crate.rveprefab\"\n[[instances]]\nid = \"crate\"\nprefab = \"/prefabs/crate.rveprefab\"\n";
    const MATERIAL: &str = "shader = \"/shaders/surface.rveshader\"\n[properties]\ntint = [0.8, 0.7, 0.6]\nalbedo = \"/textures/checker.png\"\n";
    const SHADER: &str = "[shader]\nlighting = \"lit\"\n[[properties]]\nname = \"tint\"\ntype = \"color\"\ndefault = [1.0, 1.0, 1.0]\n[[properties]]\nname = \"albedo\"\ntype = \"texture\"\ndefault = \"/textures/fallback.png\"\ncolor_space = \"srgb\"\n---\nvoid surface(inout Surface s) { s.albedo = sample_albedo(s.uv).rgb * tint; }\n";

    fn path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    /// Пишет `settings` и `bootstrap.rves`; `list` — содержимое `prefabs.rveprefabs`.
    fn project(root: &Path, scenes: &[&str], list: Option<&str>) {
        let mut settings = "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n".to_owned();
        for scene in scenes {
            settings.push_str(&format!("[[scenes]]\npath = \"{scene}\"\n"));
        }
        fs::write(root.join("project.rveproject"), settings).unwrap();
        fs::write(root.join("bootstrap.rves"), "").unwrap();
        if let Some(list) = list {
            fs::write(root.join("prefabs.rveprefabs"), list).unwrap();
        }
    }

    fn asset(root: &Path, file: &str, text: &str) {
        let target = root.join("Assets").join(file);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text).unwrap();
    }

    /// Сцена `/scenes/<name>.rvescene` с камерой; `extra` — её `[[assets]]` и экземпляры.
    fn scene(root: &Path, name: &str, extra: &str) {
        asset(
            root,
            &format!("scenes/{name}.rvescene"),
            &format!("{extra}{CAMERA}"),
        );
    }

    /// Префаб `/prefabs/crate.rveprefab` объявляет материал и экземпляр — сам префаб без камеры.
    fn crate_prefab(root: &Path) {
        asset(
            root,
            "prefabs/crate.rveprefab",
            "[[assets]]\npath = \"/materials/wood.rvemat\"\n[[objects]]\nid = \"crate\"\n",
        );
        asset(root, "materials/wood.rvemat", MATERIAL);
        asset(root, "shaders/surface.rveshader", SHADER);
        asset(root, "textures/checker.png", "checker");
        asset(root, "textures/fallback.png", "fallback");
    }

    fn explain(root: &Path, asset_path: &str) -> String {
        explain_asset(root, &path(asset_path)).unwrap().to_string()
    }

    #[test]
    fn explains_the_chain_through_an_instanced_prefab_and_a_single_scene() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-chain").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        scene(root.path(), "main", INSTANCE_CRATE);
        crate_prefab(root.path());

        let text = explain(root.path(), "/textures/checker.png");
        assert!(
            text.starts_with("/textures/checker.png → архив scene-main\n"),
            "{text}"
        );
        assert!(
            text.contains("причина: ресурс используется одной сценой: /scenes/main.rvescene\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "  сцена /scenes/main.rvescene → префаб /prefabs/crate.rveprefab ([[assets]]) → /materials/wood.rvemat ([[assets]]) → /textures/checker.png (shader или текстура материала)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn shared_resource_lists_one_chain_per_scene_that_reaches_it() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-shared").unwrap();
        project(
            root.path(),
            &["/scenes/main.rvescene", "/scenes/second.rvescene"],
            None,
        );
        scene(root.path(), "main", INSTANCE_CRATE);
        scene(root.path(), "second", INSTANCE_CRATE);
        crate_prefab(root.path());

        let text = explain(root.path(), "/textures/checker.png");
        assert!(
            text.starts_with("/textures/checker.png → архив shared\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "причина: ресурс используют несколько сцен: /scenes/main.rvescene, /scenes/second.rvescene\n"
            ),
            "{text}"
        );
        let chains = text.lines().filter(|line| line.starts_with("  ")).count();
        assert_eq!(chains, 2, "{text}");
        assert!(
            text.contains("  сцена /scenes/second.rvescene → префаб"),
            "{text}"
        );
    }

    #[test]
    fn explicit_archive_of_the_scene_names_the_scene_as_its_source() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-explicit").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        scene(
            root.path(),
            "main",
            "[[assets]]\npath = \"/textures/logo.png\"\narchive = \"ui\"\n",
        );
        asset(root.path(), "textures/logo.png", "logo");

        let text = explain(root.path(), "/textures/logo.png");
        assert!(
            text.starts_with("/textures/logo.png → архив ui\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "причина: явный archive \"ui\" в [[assets]] сцены /scenes/main.rvescene\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  сцена /scenes/main.rvescene → /textures/logo.png ([[assets]])\n"),
            "{text}"
        );
    }

    #[test]
    fn explicit_archive_inherited_from_a_prefab_names_the_prefab_as_its_source() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-inherited").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        scene(
            root.path(),
            "main",
            "[[assets]]\npath = \"/prefabs/sign.rveprefab\"\n[[instances]]\nid = \"sign\"\nprefab = \"/prefabs/sign.rveprefab\"\n",
        );
        asset(
            root.path(),
            "prefabs/sign.rveprefab",
            "[[assets]]\npath = \"/textures/sign.png\"\narchive = \"signs\"\n[[objects]]\nid = \"sign\"\n",
        );
        asset(root.path(), "textures/sign.png", "sign");

        let text = explain(root.path(), "/textures/sign.png");
        assert!(
            text.starts_with("/textures/sign.png → архив signs\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "причина: явный archive \"signs\" в [[assets]] префаба /prefabs/sign.rveprefab\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  сцена /scenes/main.rvescene → префаб /prefabs/sign.rveprefab ([[assets]]) → /textures/sign.png ([[assets]])\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn scene_override_wins_over_the_archive_of_its_prefab() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-override").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        scene(
            root.path(),
            "main",
            "[[assets]]\npath = \"/textures/sign.png\"\narchive = \"ui\"\n[[assets]]\npath = \"/prefabs/sign.rveprefab\"\n[[instances]]\nid = \"sign\"\nprefab = \"/prefabs/sign.rveprefab\"\n",
        );
        asset(
            root.path(),
            "prefabs/sign.rveprefab",
            "[[assets]]\npath = \"/textures/sign.png\"\narchive = \"signs\"\n[[objects]]\nid = \"sign\"\n",
        );
        asset(root.path(), "textures/sign.png", "sign");

        let text = explain(root.path(), "/textures/sign.png");
        assert!(
            text.starts_with("/textures/sign.png → архив ui\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "причина: явный archive \"ui\" в [[assets]] сцены /scenes/main.rvescene\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn scene_file_declared_by_another_scene_is_a_leaf_for_it() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-leaf").unwrap();
        project(
            root.path(),
            &["/scenes/main.rvescene", "/scenes/second.rvescene"],
            None,
        );
        scene(
            root.path(),
            "main",
            "[[assets]]\npath = \"/scenes/second.rvescene\"\n",
        );
        scene(
            root.path(),
            "second",
            "[[assets]]\npath = \"/textures/b.png\"\n",
        );
        asset(root.path(), "textures/b.png", "b");

        let text = explain(root.path(), "/textures/b.png");
        assert!(
            text.starts_with("/textures/b.png → архив scene-second\n"),
            "{text}"
        );
        assert!(
            text.contains("  сцена /scenes/second.rvescene → /textures/b.png ([[assets]])\n"),
            "{text}"
        );
        assert!(!text.contains("сцена /scenes/main.rvescene →"), "{text}");

        let scene_text = explain(root.path(), "/scenes/second.rvescene");
        assert!(
            scene_text.starts_with("/scenes/second.rvescene → архив base\n"),
            "{scene_text}"
        );
        assert!(
            scene_text.contains(
                "  сцена /scenes/main.rvescene → сцена /scenes/second.rvescene ([[assets]])\n"
            ),
            "{scene_text}"
        );
        assert!(
            scene_text.contains("  сцена /scenes/second.rvescene (корень)\n"),
            "{scene_text}"
        );
    }

    #[test]
    fn declared_prefab_that_a_heavy_prefab_does_not_expand_is_a_leaf_for_it() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-heavy-leaf").unwrap();
        project(
            root.path(),
            &["/scenes/main.rvescene"],
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\n"),
        );
        scene(root.path(), "main", INSTANCE_CRATE);
        crate_prefab(root.path());
        asset(
            root.path(),
            "prefabs/tower.rveprefab",
            "[[assets]]\npath = \"/prefabs/crate.rveprefab\"\n[[objects]]\nid = \"tower\"\n",
        );

        let text = explain(root.path(), "/textures/checker.png");
        let chains = text
            .lines()
            .filter(|line| line.starts_with("  "))
            .collect::<Vec<_>>();
        assert_eq!(chains.len(), 1, "{text}");
        assert!(
            chains[0].starts_with("  сцена /scenes/main.rvescene"),
            "{text}"
        );
    }

    #[test]
    fn a_repeated_reference_is_explained_by_the_shortest_chain() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-shortest").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        scene(
            root.path(),
            "main",
            &format!("[[assets]]\npath = \"/materials/wood.rvemat\"\n{INSTANCE_CRATE}"),
        );
        crate_prefab(root.path());

        let text = explain(root.path(), "/textures/checker.png");
        assert!(
            text.contains(
                "  сцена /scenes/main.rvescene → /materials/wood.rvemat ([[assets]]) → /textures/checker.png (shader или текстура материала)\n"
            ),
            "{text}"
        );
        assert_eq!(
            text.lines().filter(|line| line.starts_with("  ")).count(),
            1,
            "{text}"
        );
    }

    #[test]
    fn postprocess_lut_is_explained_through_the_document_that_declares_the_postprocess() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-lut").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        // Камера сцены ссылается на настройки и постобработку, которые объявлены в `[[assets]]`.
        asset(
            root.path(),
            "scenes/main.rvescene",
            "[[assets]]\npath = \"/cameras/settings.camerasettings\"\n[[assets]]\npath = \"/cameras/post.postprocess\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nsettings = \"/cameras/settings.camerasettings\"\npost_process = \"/cameras/post.postprocess\"\n",
        );
        asset(
            root.path(),
            "cameras/settings.camerasettings",
            "[projection]\nfov_deg = 55.0\n",
        );
        asset(
            root.path(),
            "cameras/post.postprocess",
            "[color_grading]\nenabled = true\nlut = \"/luts/film.cube\"\n",
        );
        asset(
            root.path(),
            "luts/film.cube",
            "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n",
        );

        let text = explain(root.path(), "/luts/film.cube");
        assert!(
            text.contains(
                "  сцена /scenes/main.rvescene → /cameras/post.postprocess ([[assets]]) → /luts/film.cube (LUT цветокоррекции)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn heavy_list_entries_with_different_archives_make_the_resource_shared() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-conflict").unwrap();
        project(
            root.path(),
            &["/scenes/main.rvescene"],
            Some(
                "[[prefabs]]\npath = \"/prefabs/tower1.rveprefab\"\narchive = \"towers\"\n[[prefabs]]\npath = \"/prefabs/tower2.rveprefab\"\narchive = \"decor\"\n",
            ),
        );
        scene(root.path(), "main", "");
        // tower1 достигает текстуры дважды: напрямую и через материал. Пара назначения
        // повторяется в использовании, но в причине должна появиться один раз.
        asset(
            root.path(),
            "prefabs/tower1.rveprefab",
            "[[assets]]\npath = \"/textures/checker.png\"\n[[assets]]\npath = \"/materials/m.rvemat\"\n[[objects]]\nid = \"tower\"\n",
        );
        asset(
            root.path(),
            "prefabs/tower2.rveprefab",
            "[[assets]]\npath = \"/textures/checker.png\"\n[[objects]]\nid = \"tower\"\n",
        );
        asset(root.path(), "materials/m.rvemat", MATERIAL);
        asset(root.path(), "shaders/surface.rveshader", SHADER);
        asset(root.path(), "textures/checker.png", "a");
        asset(root.path(), "textures/fallback.png", "b");

        let text = explain(root.path(), "/textures/checker.png");
        assert!(
            text.starts_with("/textures/checker.png → архив shared\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "причина: явные архивы расходятся, ресурс уходит в shared: \"towers\" (тяжёлый префаб /prefabs/tower1.rveprefab, запись prefabs.rveprefabs), \"decor\" (тяжёлый префаб /prefabs/tower2.rveprefab, запись prefabs.rveprefabs)\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  префаб /prefabs/tower1.rveprefab → /textures/checker.png ([[assets]])\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn heavy_list_archive_names_the_heavy_prefab_as_its_source() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-list").unwrap();
        project(
            root.path(),
            &["/scenes/main.rvescene"],
            Some("[[prefabs]]\npath = \"/prefabs/tower.rveprefab\"\narchive = \"towers\"\n"),
        );
        scene(root.path(), "main", "");
        asset(
            root.path(),
            "prefabs/tower.rveprefab",
            "[[assets]]\npath = \"/textures/a.png\"\n[[objects]]\nid = \"tower\"\n",
        );
        asset(root.path(), "textures/a.png", "a");

        let text = explain(root.path(), "/textures/a.png");
        assert!(
            text.starts_with("/textures/a.png → архив towers\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "причина: явный archive \"towers\" записи prefabs.rveprefabs для тяжёлого префаба /prefabs/tower.rveprefab\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn app_icon_and_scene_file_are_base_with_their_own_reasons() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-base").unwrap();
        create_demo_like_project(root.path());

        let icon = explain(root.path(), "/icons/app.png");
        assert!(icon.starts_with("/icons/app.png → архив base\n"), "{icon}");
        assert!(
            icon.contains("причина: AppIcon всегда лежит в base\n"),
            "{icon}"
        );
        assert!(
            icon.contains("  bootstrap.rves → /icons/app.png (AppIcon)\n"),
            "{icon}"
        );

        let scene_text = explain(root.path(), "/scenes/main.rvescene");
        assert!(
            scene_text.starts_with("/scenes/main.rvescene → архив base\n"),
            "{scene_text}"
        );
        assert!(
            scene_text.contains("причина: файл включённой сцены всегда лежит в base\n"),
            "{scene_text}"
        );
        assert!(
            scene_text.contains("  сцена /scenes/main.rvescene (корень)\n"),
            "{scene_text}"
        );
    }

    #[test]
    fn shared_texture_of_a_model_is_explained_through_the_model_in_each_scene() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-fbx").unwrap();
        create_demo_like_project(root.path());

        let text = explain(root.path(), "/textures/shared.png");
        assert!(
            text.starts_with("/textures/shared.png → архив shared\n"),
            "{text}"
        );
        for scene in ["main", "second"] {
            assert!(
                text.contains(&format!(
                    "  сцена /scenes/{scene}.rvescene → /models/shared.fbx ([[assets]]) → /textures/shared.png (текстура FBX)\n"
                )),
                "{text}"
            );
        }
    }

    #[test]
    fn resource_outside_the_build_is_a_project_error() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-outside").unwrap();
        create_demo_like_project(root.path());

        let error = explain_asset(root.path(), &path("/textures/unused.png"))
            .err()
            .unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(
            error
                .to_string()
                .contains("ресурс /textures/unused.png не входит в сборку"),
            "{error}"
        );
    }

    #[test]
    fn invalid_project_fails_with_the_same_error_as_check() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-invalid").unwrap();
        create_demo_like_project(root.path());
        fs::write(
            root.path().join("Assets/scenes/main.rvescene"),
            "[[assets]]\npath = \"/textures/missing.png\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();

        let error = explain_asset(root.path(), &path("/icons/app.png"))
            .err()
            .unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(
            error.to_string().contains("/textures/missing.png"),
            "{error}"
        );
    }

    #[test]
    fn prefab_cycle_is_a_project_error_before_any_walk() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-cycle").unwrap();
        project(root.path(), &["/scenes/main.rvescene"], None);
        scene(root.path(), "main", INSTANCE_CRATE);
        asset(
            root.path(),
            "prefabs/crate.rveprefab",
            "[[assets]]\npath = \"/prefabs/other.rveprefab\"\n[[instances]]\nid = \"other\"\nprefab = \"/prefabs/other.rveprefab\"\n",
        );
        asset(
            root.path(),
            "prefabs/other.rveprefab",
            "[[assets]]\npath = \"/prefabs/crate.rveprefab\"\n[[instances]]\nid = \"crate\"\nprefab = \"/prefabs/crate.rveprefab\"\n",
        );

        let error = explain_asset(root.path(), &path("/textures/checker.png"))
            .err()
            .unwrap();
        assert_eq!(error.exit_code(), 2, "{error}");
        assert!(error.to_string().contains("цикл префабов"), "{error}");
    }

    #[test]
    fn traced_and_plain_builds_agree_on_every_assignment_and_reason() {
        let root = TempDir::create(&std::env::temp_dir(), "rve-explain-parity").unwrap();
        create_demo_like_project(root.path());

        let plain = prepare_project(root.path()).unwrap();
        let (traced, trace) = prepare_project_traced(root.path()).unwrap();
        assert_eq!(plain.assignments, traced.assignments);
        assert_eq!(trace.reasons.len(), traced.assignments.len());
    }
}
