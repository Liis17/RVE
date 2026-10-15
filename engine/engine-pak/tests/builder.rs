use engine_files::VirtualPath;
use engine_pak::{
    ArchiveCatalog, BuildErrorKind, BuildOptions, CatalogLimits, Codec, execute, load_manifest,
    parse_manifest, preflight,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rvepak-builder-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn setup_root(directory: &TempDir) -> PathBuf {
    let root = directory.path().join("assets");
    fs::create_dir(&root).unwrap();
    root
}

fn manifest(_directory: &TempDir, output: &str, archive_specs: &str) -> String {
    format!(
        "root = \"assets\"\noutput = \"{output}\"\n\n[defaults]\ncodec = \"none\"\nchunk_size = 4\nzstd_level = 3\n\n{archive_specs}"
    )
}

fn read_asset(catalog: &ArchiveCatalog, path: &str) -> Vec<u8> {
    catalog
        .read_all(&VirtualPath::parse(path).unwrap())
        .unwrap()
}

#[test]
fn recursively_builds_opaque_files_and_preserves_equal_basenames() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::create_dir_all(root.join("models")).unwrap();
    fs::create_dir_all(root.join("textures")).unwrap();
    fs::write(root.join("models/cube.data"), [0, 1, 2, 3, 255]).unwrap();
    fs::write(root.join("textures/cube.data"), b"another opaque payload").unwrap();
    fs::write(root.join("empty.bin"), []).unwrap();

    let text = manifest(
        &directory,
        "output",
        "[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \".\"\n",
    );
    let plan = preflight(
        parse_manifest(&text, directory.path()).unwrap(),
        BuildOptions::default(),
    )
    .unwrap();
    assert_eq!(plan.input_count(), 3);
    let report = execute(plan).unwrap();
    let catalog = ArchiveCatalog::open_directory(
        &report.output_directory,
        engine_pak::ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .unwrap();

    assert_eq!(read_asset(&catalog, "models/cube.data"), [0, 1, 2, 3, 255]);
    assert_eq!(
        read_asset(&catalog, "textures/cube.data"),
        b"another opaque payload"
    );
    assert_eq!(read_asset(&catalog, "empty.bin"), b"");
    assert_eq!(report.stats.input_files, 3);
    assert_eq!(report.stats.input_bytes, 27);
    assert!(report.stats.output_bytes > 0);
    assert_eq!(report.stats.deduplicated_files, 0);
    assert_eq!(report.assets.len(), 3);
}

#[test]
fn first_manifest_owner_controls_local_aliases_and_cross_archive_references() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::create_dir_all(root.join("first")).unwrap();
    fs::create_dir_all(root.join("second")).unwrap();
    let bytes = b"same opaque content";
    fs::write(root.join("first/owner.bin"), bytes).unwrap();
    fs::write(root.join("first/alias.bin"), bytes).unwrap();
    fs::write(root.join("second/reference.bin"), bytes).unwrap();
    let text = manifest(
        &directory,
        "output",
        "[[archives]]\nname = \"base.rvepak\"\n\
         [[archives.inputs]]\npath = \"first/owner.bin\"\n\
         [[archives.inputs]]\npath = \"first/alias.bin\"\ncodec = \"zstd\"\nchunk_size = 8\nzstd_level = 5\n\
         [[archives]]\nname = \"patch.rvepak\"\n\
         [[archives.inputs]]\npath = \"second/reference.bin\"\ncodec = \"lz4\"\nchunk_size = 2\n",
    );
    let plan = preflight(
        parse_manifest(&text, directory.path()).unwrap(),
        BuildOptions::default(),
    )
    .unwrap();
    let report = execute(plan).unwrap();
    let catalog = ArchiveCatalog::open_directory(
        &report.output_directory,
        engine_pak::ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .unwrap();
    let entries = catalog.entries().unwrap();
    let owner = VirtualPath::parse("first/owner.bin").unwrap();
    let alias = VirtualPath::parse("first/alias.bin").unwrap();
    let reference = VirtualPath::parse("second/reference.bin").unwrap();
    let owner_stat = catalog.stat(&owner).unwrap();

    assert_eq!(entries.len(), 3);
    assert_eq!(catalog.stat(&alias).unwrap().blob_id, owner_stat.blob_id);
    assert_eq!(
        catalog.stat(&reference).unwrap().blob_id,
        owner_stat.blob_id
    );
    assert_eq!(
        catalog.stat(&reference).unwrap().owner_archive_id,
        owner_stat.owner_archive_id
    );
    assert!(
        entries
            .iter()
            .all(|entry| read_asset(&catalog, entry.path.as_str()) == bytes)
    );
    assert_eq!(report.owners.len(), 1);
    assert_eq!(report.owners[0].path, owner);
    let alias_asset = report
        .assets
        .iter()
        .find(|asset| asset.path == alias)
        .unwrap();
    assert_eq!(alias_asset.archive_id, alias_asset.owner_archive_id);
    assert_eq!(alias_asset.blob_id, owner_stat.blob_id);
    assert_eq!(report.dependencies.len(), 1);
    assert_eq!(report.dependencies[0].owner_path, owner);
    assert_eq!(report.stats.deduplicated_files, 2);
    assert_eq!(
        report.stats.deduplicated_input_bytes,
        (bytes.len() * 2) as u64
    );
    assert_eq!(report.ignored_overrides.len(), 2);
    assert_eq!(report.ignored_overrides[0].applied.codec, Codec::None);
    assert_eq!(report.ignored_overrides[1].applied.codec, Codec::None);
}

#[test]
fn no_dedup_keeps_identical_inputs_as_distinct_local_blobs() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("a.bin"), b"repeated").unwrap();
    fs::write(root.join("b.bin"), b"repeated").unwrap();
    let text = manifest(
        &directory,
        "dedup",
        "[[archives]]\nname = \"game.rvepak\"\n\
         [[archives.inputs]]\npath = \"a.bin\"\n\
         [[archives.inputs]]\npath = \"b.bin\"\n",
    );
    let dedup_manifest = parse_manifest(&text, directory.path()).unwrap();
    let dedup =
        execute(preflight(dedup_manifest.clone(), BuildOptions::default()).unwrap()).unwrap();
    let mut no_dedup_manifest = dedup_manifest;
    no_dedup_manifest.output = directory.path().join("no-dedup");
    let no_dedup =
        execute(preflight(no_dedup_manifest, BuildOptions { deduplicate: false }).unwrap())
            .unwrap();
    let dedup_catalog = ArchiveCatalog::open_directory(
        &dedup.output_directory,
        engine_pak::ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .unwrap();
    let no_dedup_catalog = ArchiveCatalog::open_directory(
        &no_dedup.output_directory,
        engine_pak::ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .unwrap();
    let a = VirtualPath::parse("a.bin").unwrap();
    let b = VirtualPath::parse("b.bin").unwrap();

    assert_eq!(dedup_catalog.verify().unwrap().local_blobs_verified, 1);
    assert_eq!(no_dedup_catalog.verify().unwrap().local_blobs_verified, 2);
    assert_ne!(
        no_dedup_catalog.stat(&a).unwrap().blob_id,
        no_dedup_catalog.stat(&b).unwrap().blob_id
    );
    assert!(no_dedup.stats.output_bytes > dedup.stats.output_bytes);
    assert_eq!(no_dedup.stats.deduplicated_files, 0);
    assert_eq!(read_asset(&no_dedup_catalog, "a.bin"), b"repeated");
    assert_eq!(read_asset(&no_dedup_catalog, "b.bin"), b"repeated");
}

#[test]
fn overlapping_selectors_fail_before_creating_output_parent() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("nested.bin"), b"data").unwrap();
    let text = manifest(
        &directory,
        "new-parent/output",
        "[[archives]]\nname = \"game.rvepak\"\n\
         [[archives.inputs]]\npath = \".\"\n\
         [[archives.inputs]]\npath = \"nested.bin\"\n",
    );
    let error = preflight(
        parse_manifest(&text, directory.path()).unwrap(),
        BuildOptions::default(),
    )
    .unwrap_err();

    assert_eq!(error.kind(), BuildErrorKind::Input);
    assert!(error.to_string().contains("assigned more than once"));
    assert!(!directory.path().join("new-parent").exists());
}

#[test]
fn manifest_rejects_unknown_fields_empty_groups_unsafe_names_and_self_inclusion() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("file.bin"), b"data").unwrap();
    let unknown = "root = \"assets\"\noutput = \"out\"\nunknown = true\n\n[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"file.bin\"\n";
    assert_eq!(
        parse_manifest(unknown, directory.path())
            .unwrap_err()
            .kind(),
        BuildErrorKind::Input
    );
    for (name, selector, output) in [
        ("CON.rvepak", "file.bin", "out"),
        ("game.rvepak", "missing.bin", "out"),
        ("game.rvepak", "file.bin", "assets/generated"),
    ] {
        let text = manifest(
            &directory,
            output,
            &format!(
                "[[archives]]\nname = \"{name}\"\n[[archives.inputs]]\npath = \"{selector}\"\n"
            ),
        );
        assert_eq!(
            preflight(
                parse_manifest(&text, directory.path()).unwrap(),
                BuildOptions::default()
            )
            .unwrap_err()
            .kind(),
            BuildErrorKind::Input,
            "case {name}, {selector}, {output}"
        );
    }
    let empty = manifest(&directory, "out", "[[archives]]\nname = \"empty.rvepak\"\n");
    assert!(
        preflight(
            parse_manifest(&empty, directory.path()).unwrap(),
            BuildOptions::default()
        )
        .unwrap_err()
        .to_string()
        .contains("no input files")
    );
}

#[test]
fn manifest_rejects_invalid_compression_and_traversal_paths() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("file.bin"), b"data").unwrap();

    for (overrides, expected) in [
        ("codec = \"unknown\"\n", "unknown codec"),
        ("chunk_size = 0\n", "chunk_size"),
        ("codec = \"zstd\"\nzstd_level = 2147483647\n", "zstd_level"),
    ] {
        let text = manifest(
            &directory,
            "out",
            &format!(
                "[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"file.bin\"\n{overrides}"
            ),
        );
        let error = parse_manifest(&text, directory.path())
            .and_then(|manifest| preflight(manifest, BuildOptions::default()))
            .unwrap_err();
        assert_eq!(error.kind(), BuildErrorKind::Input);
        assert!(error.to_string().contains(expected), "{error}");
    }

    let traversal = manifest(
        &directory,
        "out",
        "[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"../outside.bin\"\n",
    );
    assert_eq!(
        parse_manifest(&traversal, directory.path())
            .unwrap_err()
            .kind(),
        BuildErrorKind::Input
    );

    for field in ["root", "output"] {
        let text = if field == "root" {
            "root = \"C:/outside\"\noutput = \"out\"\n".to_owned()
        } else {
            "root = \"assets\"\noutput = \"C:/outside\"\n".to_owned()
        };
        assert_eq!(
            parse_manifest(&text, directory.path()).unwrap_err().kind(),
            BuildErrorKind::Input
        );
    }
}

#[test]
fn archive_name_collisions_are_case_insensitive() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("one.bin"), b"one").unwrap();
    fs::write(root.join("two.bin"), b"two").unwrap();
    let text = manifest(
        &directory,
        "out",
        "[[archives]]\nname = \"Base.rvepak\"\n[[archives.inputs]]\npath = \"one.bin\"\n\
         [[archives]]\nname = \"base.rvepak\"\n[[archives.inputs]]\npath = \"two.bin\"\n",
    );
    assert!(
        preflight(
            parse_manifest(&text, directory.path()).unwrap(),
            BuildOptions::default()
        )
        .unwrap_err()
        .to_string()
        .contains("collide on case-insensitive")
    );
}

#[test]
fn changed_source_aborts_before_publishing_and_removes_staging() {
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("file.bin"), b"before hash").unwrap();
    let text = manifest(
        &directory,
        "output",
        "[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"file.bin\"\n",
    );
    let plan = preflight(
        parse_manifest(&text, directory.path()).unwrap(),
        BuildOptions::default(),
    )
    .unwrap();
    fs::write(root.join("file.bin"), b"changed after preflight").unwrap();

    let error = execute(plan).unwrap_err();
    assert_eq!(error.kind(), BuildErrorKind::Input);
    assert!(error.to_string().contains("streamed source hash changed"));
    assert!(!directory.path().join("output").exists());
    let names = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0], "assets");
}

#[test]
fn changed_local_alias_and_cross_archive_reference_are_rehashed_before_use() {
    for (case, archives) in [
        (
            "local-alias",
            "[[archives]]\nname = \"game.rvepak\"\n\
             [[archives.inputs]]\npath = \"owner.bin\"\n\
             [[archives.inputs]]\npath = \"duplicate.bin\"\n",
        ),
        (
            "external-reference",
            "[[archives]]\nname = \"base.rvepak\"\n[[archives.inputs]]\npath = \"owner.bin\"\n\
             [[archives]]\nname = \"patch.rvepak\"\n[[archives.inputs]]\npath = \"duplicate.bin\"\n",
        ),
    ] {
        let directory = TempDir::new();
        let root = setup_root(&directory);
        fs::write(root.join("owner.bin"), b"identical bytes").unwrap();
        fs::write(root.join("duplicate.bin"), b"identical bytes").unwrap();
        let text = manifest(&directory, "output", archives);
        let plan = preflight(
            parse_manifest(&text, directory.path()).unwrap(),
            BuildOptions::default(),
        )
        .unwrap();
        fs::write(root.join("duplicate.bin"), b"changed after hash").unwrap();

        let error = execute(plan).unwrap_err();
        assert_eq!(error.kind(), BuildErrorKind::Input, "{case}");
        assert!(
            error.to_string().contains("input changed after preflight"),
            "{case}: {error}"
        );
        assert!(!directory.path().join("output").exists(), "{case}");
        let staging_left = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".rvepak-stage-")
            });
        assert!(!staging_left, "{case}");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_inside_selected_directories_are_rejected() {
    use std::os::unix::fs::symlink;
    let directory = TempDir::new();
    let root = setup_root(&directory);
    fs::write(root.join("target.bin"), b"outside the selected tree").unwrap();
    fs::create_dir(root.join("selected")).unwrap();
    symlink(root.join("target.bin"), root.join("selected/link.bin")).unwrap();
    let text = manifest(
        &directory,
        "output",
        "[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"selected\"\n",
    );

    let error = preflight(
        parse_manifest(&text, directory.path()).unwrap(),
        BuildOptions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), BuildErrorKind::Input);
    assert!(error.to_string().contains("symlink input is not allowed"));
    assert!(!directory.path().join("output").exists());
}

#[test]
fn load_manifest_resolves_paths_relative_to_the_manifest_file() {
    let directory = TempDir::new();
    fs::create_dir(directory.path().join("config")).unwrap();
    let root = directory.path().join("config/assets");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("file.bin"), b"data").unwrap();
    let config_dir = directory.path().join("config");
    let config = manifest(
        &directory,
        "../built",
        "[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"file.bin\"\n",
    );
    fs::write(config_dir.join("assets.toml"), config).unwrap();

    let parsed = load_manifest(config_dir.join("assets.toml")).unwrap();
    assert_eq!(parsed.root, directory.path().join("config/assets"));
    assert_eq!(parsed.output, directory.path().join("built"));
}
