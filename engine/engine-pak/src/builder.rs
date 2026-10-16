use crate::archive_name::{ArchiveNameError, is_reserved_windows_device, validate_archive_stem};
use crate::{
    ArchiveCatalog, ArchiveId, ArchiveWriter, BlobId, BlobOptions, CatalogAsset, CatalogLimits,
    Codec, ContentHash, PakError, WriterOptions,
};
use engine_files::VirtualPath;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildErrorKind {
    Input,
    Operational,
}

#[derive(Debug)]
pub struct BuildError {
    kind: BuildErrorKind,
    message: String,
}

impl BuildError {
    fn input(message: impl Into<String>) -> Self {
        Self {
            kind: BuildErrorKind::Input,
            message: message.into(),
        }
    }

    fn operational(message: impl Into<String>) -> Self {
        Self {
            kind: BuildErrorKind::Operational,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> BuildErrorKind {
        self.kind
    }
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for BuildError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildCompression {
    pub codec: Codec,
    pub chunk_size: u64,
    pub zstd_level: i32,
}

impl Default for BuildCompression {
    fn default() -> Self {
        Self {
            codec: Codec::Zstd,
            chunk_size: crate::DEFAULT_CHUNK_SIZE,
            zstd_level: crate::DEFAULT_ZSTD_LEVEL,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildInput {
    /// A file or directory selector relative to [`BuildManifest::root`].
    pub path: PathBuf,
    pub codec: Option<Codec>,
    pub chunk_size: Option<u64>,
    pub zstd_level: Option<i32>,
}

/// One explicit source-file to virtual-path mapping for [`build_archive_set`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildFileInput {
    /// Source file on disk. It must be an existing regular file, not a symlink.
    pub source: PathBuf,
    /// Canonical destination inside the archive namespace.
    pub path: VirtualPath,
    pub codec: Option<Codec>,
    pub chunk_size: Option<u64>,
    pub zstd_level: Option<i32>,
}

impl BuildFileInput {
    pub fn new(source: impl Into<PathBuf>, path: VirtualPath) -> Self {
        Self {
            source: source.into(),
            path,
            codec: None,
            chunk_size: None,
            zstd_level: None,
        }
    }
}

/// An explicitly ordered archive in a programmatic archive-set build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgrammaticArchive {
    /// Archive name without the `.rvepak` suffix.
    pub name: String,
    pub inputs: Vec<BuildFileInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveBuildSpec {
    pub name: String,
    pub inputs: Vec<BuildInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildManifest {
    /// Absolute lexical path to the source asset root. It is canonicalized during preflight.
    pub root: PathBuf,
    /// Absolute lexical path to a new output directory.
    pub output: PathBuf,
    pub defaults: BuildCompression,
    pub archives: Vec<ArchiveBuildSpec>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    pub deduplicate: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { deduplicate: true }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildArchive {
    pub name: String,
    pub archive_id: ArchiveId,
    /// Physical compressed archive bytes, including the index and header.
    pub output_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOwner {
    pub archive: String,
    pub archive_id: ArchiveId,
    pub path: VirtualPath,
    pub blob_id: BlobId,
    pub size: u64,
    pub content_hash: ContentHash,
    pub codec: Codec,
    pub chunk_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildDependency {
    pub archive: String,
    pub owner_archive: String,
    pub path: VirtualPath,
    pub owner_path: VirtualPath,
    pub owner_archive_id: ArchiveId,
    pub blob_id: BlobId,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IgnoredCompressionOverride {
    pub archive: String,
    pub path: VirtualPath,
    pub owner_archive: String,
    pub owner_path: VirtualPath,
    pub requested: BuildCompression,
    pub applied: BuildCompression,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuildStats {
    pub input_files: u64,
    /// Logical source bytes across every path, including references and aliases.
    pub input_bytes: u64,
    pub output_archives: u64,
    /// Physical compressed archive bytes, including indexes and headers.
    pub output_bytes: u64,
    pub deduplicated_files: u64,
    /// Raw logical input bytes represented by aliases or cross-archive references.
    pub deduplicated_input_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildReport {
    pub output_directory: PathBuf,
    pub archives: Vec<BuildArchive>,
    /// Every virtual path with its declaring archive and resolved local blob owner.
    pub assets: Vec<CatalogAsset>,
    pub owners: Vec<BuildOwner>,
    pub dependencies: Vec<BuildDependency>,
    pub ignored_overrides: Vec<IgnoredCompressionOverride>,
    pub stats: BuildStats,
}

/// Fully expanded and validated build input. Deduplicating builds also contain a BLAKE3
/// prepass for every expanded source; execution checks each canonical owner's stream again.
#[derive(Debug)]
pub struct BuildPlan {
    destination: PathBuf,
    archives: Vec<PlannedArchive>,
    deduplicate: bool,
}

impl BuildPlan {
    pub fn input_count(&self) -> usize {
        self.archives
            .iter()
            .map(|archive| archive.inputs.len())
            .sum()
    }

    pub fn output_directory(&self) -> &Path {
        &self.destination
    }
}

#[derive(Debug)]
struct PlannedArchive {
    name: String,
    inputs: Vec<PlannedInput>,
}

#[derive(Debug)]
struct ExpandedInput {
    path: VirtualPath,
    source_root: PathBuf,
    source: PathBuf,
    compression: BuildCompression,
}

type ExpandedArchive = (String, Vec<ExpandedInput>);

#[derive(Debug)]
struct PlannedInput {
    path: VirtualPath,
    source_root: PathBuf,
    source: PathBuf,
    size: u64,
    content_hash: Option<ContentHash>,
    compression: BuildCompression,
}

#[derive(Clone)]
struct OwnerRecord {
    archive_id: ArchiveId,
    archive_name: String,
    path: VirtualPath,
    blob_id: BlobId,
    compression: BuildCompression,
}

#[derive(Clone)]
struct PendingOwner {
    path: VirtualPath,
    blob_id: BlobId,
    compression: BuildCompression,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ContentKey {
    hash: ContentHash,
    size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    root: String,
    output: String,
    #[serde(default)]
    defaults: RawCompression,
    archives: Vec<RawArchive>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawCompression {
    codec: Option<String>,
    chunk_size: Option<u64>,
    zstd_level: Option<i32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArchive {
    name: String,
    #[serde(default)]
    inputs: Vec<RawInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    path: String,
    codec: Option<String>,
    chunk_size: Option<u64>,
    zstd_level: Option<i32>,
}

/// Reads a TOML manifest and resolves `root` and `output` against its parent directory.
pub fn load_manifest(path: impl AsRef<Path>) -> Result<BuildManifest, BuildError> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path).map_err(|error| {
        BuildError::input(format!("cannot read manifest {}: {error}", path.display()))
    })?;
    let base = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    parse_manifest(&contents, base)
}

/// Parses the documented manifest schema, resolving filesystem paths against `manifest_dir`.
pub fn parse_manifest(
    contents: &str,
    manifest_dir: impl AsRef<Path>,
) -> Result<BuildManifest, BuildError> {
    let raw: RawManifest = toml::from_str(contents)
        .map_err(|error| BuildError::input(format!("invalid RVEPAK manifest: {error}")))?;
    let base = absolute_lexical(manifest_dir.as_ref())
        .map_err(|error| BuildError::operational(error.to_string()))?;
    let root = resolve_manifest_path(&base, &raw.root, "root")?;
    let output = resolve_manifest_path(&base, &raw.output, "output")?;
    let defaults = resolve_compression(&raw.defaults, BuildCompression::default())?;
    let mut archives = Vec::with_capacity(raw.archives.len());
    for archive in raw.archives {
        let inputs = archive
            .inputs
            .into_iter()
            .map(|input| {
                let path = parse_selector(&input.path)?;
                Ok(BuildInput {
                    path,
                    codec: input.codec.as_deref().map(parse_codec).transpose()?,
                    chunk_size: input.chunk_size,
                    zstd_level: input.zstd_level,
                })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;
        archives.push(ArchiveBuildSpec {
            name: archive.name,
            inputs,
        });
    }
    Ok(BuildManifest {
        root,
        output,
        defaults,
        archives,
    })
}

/// Expands selectors, validates the complete destination namespace and hashes all inputs
/// before any output directory or archive is created.
pub fn preflight(manifest: BuildManifest, options: BuildOptions) -> Result<BuildPlan, BuildError> {
    if manifest.archives.is_empty() {
        return Err(BuildError::input(
            "manifest must declare at least one archive",
        ));
    }
    validate_compression(manifest.defaults)?;
    let source_root = canonical_source_root(&manifest.root)?;
    let destination = prepare_output_directory(&manifest.output)?;
    if destination.starts_with(&source_root) {
        return Err(BuildError::input(format!(
            "output directory is inside the source root: {}",
            destination.display()
        )));
    }
    let output_name = destination.file_name().ok_or_else(|| {
        BuildError::input("output must name a new directory, not a filesystem root")
    })?;
    if output_name.is_empty() {
        return Err(BuildError::input("output directory name is empty"));
    }

    let mut archive_names = BTreeSet::new();
    let mut expanded_archives = Vec::with_capacity(manifest.archives.len());
    for archive in manifest.archives {
        validate_archive_name(&archive.name)?;
        let folded_name = archive.name.to_ascii_lowercase();
        if !archive_names.insert(folded_name) {
            return Err(BuildError::input(format!(
                "archive names collide on case-insensitive filesystems: {}",
                archive.name
            )));
        }
        let mut expanded = Vec::new();
        for input in &archive.inputs {
            let compression = resolve_input_compression(input, manifest.defaults);
            validate_compression(compression)?;
            expand_selector(&source_root, input, compression, &mut expanded)?;
        }
        expanded_archives.push((archive.name, expanded));
    }

    finalize_build_plan(destination, expanded_archives, options)
}

/// Builds an ordered archive set from explicit source-file and virtual-path pairs.
///
/// The archive order controls cross-archive deduplication ownership. Archive
/// names are filename-safe stems; output files are named `<name>.rvepak`.
pub fn build_archive_set(
    archives: Vec<ProgrammaticArchive>,
    output: impl AsRef<Path>,
    defaults: BuildCompression,
    options: BuildOptions,
) -> Result<BuildReport, BuildError> {
    if archives.is_empty() {
        return Err(BuildError::input(
            "archive set must declare at least one archive",
        ));
    }
    validate_compression(defaults)?;

    let destination = prepare_output_directory(output.as_ref())?;

    let mut folded_names = BTreeSet::new();
    let mut canonical_archives = Vec::with_capacity(archives.len());
    for archive in archives {
        validate_programmatic_archive_name(&archive.name)?;
        if !folded_names.insert(archive.name.to_ascii_lowercase()) {
            return Err(BuildError::input(format!(
                "archive names collide on case-insensitive filesystems: {}",
                archive.name
            )));
        }
        if archive.inputs.is_empty() {
            return Err(BuildError::input(format!(
                "archive {} has no input files",
                archive.name
            )));
        }
        let inputs =
            plan_programmatic_inputs(archive.inputs, defaults, canonical_programmatic_source)?;
        canonical_archives.push((archive.name, inputs));
    }

    let archives = canonical_archives
        .into_iter()
        .map(|(name, files)| (format!("{name}.rvepak"), files))
        .collect();
    execute(finalize_build_plan(destination, archives, options)?)
}

fn prepare_output_directory(output: &Path) -> Result<PathBuf, BuildError> {
    let output_input =
        absolute_lexical(output).map_err(|error| BuildError::operational(error.to_string()))?;
    match fs::symlink_metadata(&output_input) {
        Ok(_) => {
            return Err(BuildError::input(format!(
                "output directory already exists: {}",
                output_input.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BuildError::operational(format!(
                "cannot inspect output directory {}: {error}",
                output_input.display()
            )));
        }
    }
    let destination = resolve_missing_path(&output_input)
        .map_err(|error| BuildError::operational(error.to_string()))?;
    if destination.file_name().is_none_or(|name| name.is_empty()) {
        return Err(BuildError::input(
            "output must name a new directory, not a filesystem root",
        ));
    }
    Ok(destination)
}

fn finalize_build_plan(
    destination: PathBuf,
    expanded_archives: Vec<ExpandedArchive>,
    options: BuildOptions,
) -> Result<BuildPlan, BuildError> {
    let mut virtual_owners = BTreeMap::<VirtualPath, String>::new();
    for (name, expanded) in &expanded_archives {
        if expanded.is_empty() {
            return Err(BuildError::input(format!(
                "archive group {name} has no input files"
            )));
        }
        for input in expanded {
            if let Some(previous_archive) = virtual_owners.insert(input.path.clone(), name.clone())
            {
                return Err(BuildError::input(format!(
                    "virtual path {} is assigned more than once (archives {previous_archive} and {name})",
                    input.path
                )));
            }
        }
    }

    let mut planned_archives = Vec::with_capacity(expanded_archives.len());
    for (name, expanded) in expanded_archives {
        let mut planned_inputs = Vec::with_capacity(expanded.len());
        for input in expanded {
            let ExpandedInput {
                path,
                source_root,
                source,
                compression,
            } = input;
            let (size, content_hash) = if options.deduplicate {
                hash_source(&source_root, &source)?
            } else {
                let file = open_checked_source(&source_root, &source)?;
                let metadata = file.metadata().map_err(|error| {
                    BuildError::input(format!(
                        "cannot inspect input {}: {error}",
                        source.display()
                    ))
                })?;
                (metadata.len(), ContentHash::from_bytes([0; 32]))
            };
            planned_inputs.push(PlannedInput {
                path,
                source_root,
                source,
                size,
                content_hash: options.deduplicate.then_some(content_hash),
                compression,
            });
        }
        planned_archives.push(PlannedArchive {
            name,
            inputs: planned_inputs,
        });
    }
    Ok(BuildPlan {
        destination,
        archives: planned_archives,
        deduplicate: options.deduplicate,
    })
}

/// Streams every archive into a sibling staging directory, validates the complete catalog,
/// then publishes the directory with an atomic no-replace rename.
pub fn execute(plan: BuildPlan) -> Result<BuildReport, BuildError> {
    execute_with_publish_hook(plan, |_| Ok(()))
}

fn execute_with_publish_hook(
    plan: BuildPlan,
    before_publish: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<BuildReport, BuildError> {
    let parent = plan
        .destination
        .parent()
        .ok_or_else(|| BuildError::input("output directory must have a parent directory"))?;
    fs::create_dir_all(parent).map_err(|error| {
        BuildError::operational(format!(
            "cannot create output parent {}: {error}",
            parent.display()
        ))
    })?;
    let actual_parent = fs::canonicalize(parent).map_err(|error| {
        BuildError::operational(format!(
            "cannot resolve output parent {}: {error}",
            parent.display()
        ))
    })?;
    let destination_name = plan
        .destination
        .file_name()
        .ok_or_else(|| BuildError::input("output directory name is empty"))?;
    let destination = actual_parent.join(destination_name);
    if destination != plan.destination {
        return Err(BuildError::operational(
            "output parent changed after preflight; refusing to stage in a different location",
        ));
    }

    let mut staging = StagingDirectory::create(&actual_parent)?;
    let mut global_owners = BTreeMap::<ContentKey, OwnerRecord>::new();
    let mut owners = Vec::<(String, VirtualPath, BlobId)>::new();
    let mut dependencies = Vec::new();
    let mut ignored_overrides = Vec::new();
    let mut built_archives = Vec::with_capacity(plan.archives.len());
    let mut deduplicated_files = 0_u64;
    let mut deduplicated_input_bytes = 0_u64;

    for archive in &plan.archives {
        let archive_path = staging.path().join(&archive.name);
        let output = File::create(&archive_path).map_err(|error| {
            BuildError::operational(format!(
                "cannot create archive {}: {error}",
                archive_path.display()
            ))
        })?;
        let mut writer =
            ArchiveWriter::with_spool_dir(output, WriterOptions::default(), staging.path())
                .map_err(|error| {
                    BuildError::operational(format!("cannot initialize {}: {error}", archive.name))
                })?;
        let mut local_owners = BTreeMap::<ContentKey, PendingOwner>::new();
        let mut new_owners = Vec::<(ContentKey, PendingOwner)>::new();

        for input in &archive.inputs {
            let path = input.path.clone();
            if plan.deduplicate {
                let hash = input
                    .content_hash
                    .expect("deduplication prepass records hashes");
                let key = ContentKey {
                    hash,
                    size: input.size,
                };
                if let Some(owner) = local_owners.get(&key) {
                    verify_prehashed_source(input)?;
                    writer
                        .add_alias(path.clone(), owner.blob_id)
                        .map_err(|error| {
                            BuildError::operational(format!("cannot alias {path}: {error}"))
                        })?;
                    deduplicated_files += 1;
                    deduplicated_input_bytes = deduplicated_input_bytes.saturating_add(input.size);
                    record_ignored_override(
                        &mut ignored_overrides,
                        archive,
                        input,
                        &archive.name,
                        &owner.path,
                        owner.compression,
                    );
                    continue;
                }
                if let Some(owner) = global_owners.get(&key) {
                    verify_prehashed_source(input)?;
                    writer
                        .add_external(path.clone(), owner.archive_id, owner.blob_id)
                        .map_err(|error| {
                            BuildError::operational(format!("cannot reference {path}: {error}"))
                        })?;
                    deduplicated_files += 1;
                    deduplicated_input_bytes = deduplicated_input_bytes.saturating_add(input.size);
                    dependencies.push((
                        archive.name.clone(),
                        input.path.clone(),
                        owner.archive_name.clone(),
                        owner.path.clone(),
                        owner.archive_id,
                        owner.blob_id,
                    ));
                    record_ignored_override(
                        &mut ignored_overrides,
                        archive,
                        input,
                        &owner.archive_name,
                        &owner.path,
                        owner.compression,
                    );
                    continue;
                }
                let blob_id = add_local(&mut writer, input, Some(hash))?;
                let pending = PendingOwner {
                    path: input.path.clone(),
                    blob_id,
                    compression: input.compression,
                };
                local_owners.insert(key, pending.clone());
                new_owners.push((key, pending));
                owners.push((archive.name.clone(), input.path.clone(), blob_id));
            } else {
                let blob_id = add_local(&mut writer, input, None)?;
                owners.push((archive.name.clone(), input.path.clone(), blob_id));
            }
        }

        let finished = writer
            .finish()
            .map_err(|error| classify_writer_error(&archive.name, error))?;
        let archive_id = finished.archive_id;
        for (key, owner) in new_owners {
            global_owners.insert(
                key,
                OwnerRecord {
                    archive_id,
                    archive_name: archive.name.clone(),
                    path: owner.path,
                    blob_id: owner.blob_id,
                    compression: owner.compression,
                },
            );
        }
        let output = finished.into_inner();
        let output_bytes = output
            .metadata()
            .map_err(|error| {
                BuildError::operational(format!(
                    "cannot inspect archive {}: {error}",
                    archive_path.display()
                ))
            })?
            .len();
        drop(output);
        built_archives.push(BuildArchive {
            name: archive.name.clone(),
            archive_id,
            output_bytes,
        });
    }

    let catalog = ArchiveCatalog::open_directory(
        staging.path(),
        crate::ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .map_err(|error| {
        BuildError::operational(format!(
            "built archive set failed catalog validation: {error}"
        ))
    })?;
    let catalog_entries = catalog
        .entries()
        .map_err(|error| BuildError::operational(format!("cannot report built assets: {error}")))?;
    let mut report_owners = Vec::with_capacity(owners.len());
    for (archive_name, path, blob_id) in &owners {
        let archive_id = built_archives
            .iter()
            .find(|archive| archive.name == *archive_name)
            .map(|archive| archive.archive_id)
            .expect("owner archive has been written");
        let asset = catalog.stat(path).map_err(|error| {
            BuildError::operational(format!("cannot inspect owner {path}: {error}"))
        })?;
        if asset.owner_archive_id != archive_id || asset.blob_id != *blob_id {
            return Err(BuildError::operational(format!(
                "owner metadata mismatch for {path}"
            )));
        }
        report_owners.push(BuildOwner {
            archive: archive_name.clone(),
            archive_id,
            path: path.clone(),
            blob_id: *blob_id,
            size: asset.size,
            content_hash: asset.content_hash,
            codec: asset.codec,
            chunk_count: asset.chunk_count,
        });
    }
    let mut report_dependencies = Vec::with_capacity(dependencies.len());
    for (archive, path, owner_archive, owner_path, owner_archive_id, blob_id) in dependencies {
        let size = catalog
            .stat(&path)
            .map_err(|error| {
                BuildError::operational(format!("cannot inspect dependency {path}: {error}"))
            })?
            .size;
        report_dependencies.push(BuildDependency {
            archive,
            owner_archive,
            path,
            owner_path,
            owner_archive_id,
            blob_id,
            size,
        });
    }
    let input_files = u64::try_from(catalog_entries.len()).unwrap_or(u64::MAX);
    let input_bytes = catalog_entries
        .iter()
        .fold(0_u64, |total, entry| total.saturating_add(entry.size));
    let output_bytes = built_archives.iter().fold(0_u64, |total, archive| {
        total.saturating_add(archive.output_bytes)
    });
    let stats = BuildStats {
        input_files,
        input_bytes,
        output_archives: built_archives.len() as u64,
        output_bytes,
        deduplicated_files,
        deduplicated_input_bytes,
    };

    drop(catalog);
    before_publish(&destination).map_err(|error| {
        BuildError::operational(format!(
            "publication precondition failed for {}: {error}",
            destination.display()
        ))
    })?;
    publish_noreplace(staging.path(), &destination).map_err(|error| {
        BuildError::operational(format!(
            "cannot publish archive directory {}: {error}",
            destination.display()
        ))
    })?;
    staging.disarm();
    Ok(BuildReport {
        output_directory: destination,
        archives: built_archives,
        assets: catalog_entries,
        owners: report_owners,
        dependencies: report_dependencies,
        ignored_overrides,
        stats,
    })
}

fn add_local(
    writer: &mut ArchiveWriter<File>,
    input: &PlannedInput,
    expected_hash: Option<ContentHash>,
) -> Result<BlobId, BuildError> {
    let source = open_checked_source(&input.source_root, &input.source)?;
    writer
        .add_local_with_options(
            input.path.clone(),
            source,
            BlobOptions {
                codec: input.compression.codec,
                chunk_size: input.compression.chunk_size,
                zstd_level: input.compression.zstd_level,
            },
            expected_hash,
        )
        .map_err(|error| classify_writer_error(input.path.as_str(), error))
}

fn classify_writer_error(context: &str, error: PakError) -> BuildError {
    let message = format!("cannot build {context}: {error}");
    if matches!(
        &error,
        PakError::SourceMismatch { .. } | PakError::InvalidConfiguration(_)
    ) {
        BuildError::input(message)
    } else {
        BuildError::operational(message)
    }
}

fn resolve_input_compression(input: &BuildInput, defaults: BuildCompression) -> BuildCompression {
    BuildCompression {
        codec: input.codec.unwrap_or(defaults.codec),
        chunk_size: input.chunk_size.unwrap_or(defaults.chunk_size),
        zstd_level: input.zstd_level.unwrap_or(defaults.zstd_level),
    }
}

fn resolve_compression(
    raw: &RawCompression,
    defaults: BuildCompression,
) -> Result<BuildCompression, BuildError> {
    Ok(BuildCompression {
        codec: raw
            .codec
            .as_deref()
            .map(parse_codec)
            .transpose()?
            .unwrap_or(defaults.codec),
        chunk_size: raw.chunk_size.unwrap_or(defaults.chunk_size),
        zstd_level: raw.zstd_level.unwrap_or(defaults.zstd_level),
    })
}

fn parse_codec(value: &str) -> Result<Codec, BuildError> {
    match value {
        "none" => Ok(Codec::None),
        "zstd" => Ok(Codec::Zstd),
        "lz4" => Ok(Codec::Lz4),
        _ => Err(BuildError::input(format!(
            "unknown codec {value:?}; expected \"none\", \"zstd\", or \"lz4\""
        ))),
    }
}

fn validate_compression(compression: BuildCompression) -> Result<(), BuildError> {
    if compression.chunk_size == 0 || compression.chunk_size > 16 * 1024 * 1024 {
        return Err(BuildError::input(
            "chunk_size must be between 1 byte and 16 MiB",
        ));
    }
    if compression.codec == Codec::Zstd {
        let range = zstd::compression_level_range();
        if compression.zstd_level < *range.start() || compression.zstd_level > *range.end() {
            return Err(BuildError::input(
                "zstd_level is outside the supported Zstandard range",
            ));
        }
    }
    Ok(())
}

fn parse_selector(value: &str) -> Result<PathBuf, BuildError> {
    if value.is_empty() || value.contains(['\\', ':', '\0']) || Path::new(value).is_absolute() {
        return Err(BuildError::input(format!(
            "invalid input selector {value:?}"
        )));
    }
    if value == "." {
        return Ok(PathBuf::from("."));
    }
    let path = Path::new(value);
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(BuildError::input(format!(
                    "invalid input selector {value:?}"
                )));
            }
        }
    }
    Ok(path.to_path_buf())
}

fn resolve_manifest_path(base: &Path, value: &str, label: &str) -> Result<PathBuf, BuildError> {
    if value.is_empty() || value.contains(['\\', ':', '\0']) || Path::new(value).is_absolute() {
        return Err(BuildError::input(format!(
            "manifest {label} must be a relative path"
        )));
    }
    let path = absolute_lexical(&base.join(value))
        .map_err(|error| BuildError::operational(error.to_string()))?;
    Ok(path)
}

fn validate_archive_name(name: &str) -> Result<(), BuildError> {
    let Some(stem) = name.strip_suffix(".rvepak") else {
        return Err(BuildError::input(format!(
            "archive name {name:?} must end in lowercase .rvepak"
        )));
    };
    if stem.is_empty()
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || name.ends_with('.')
    {
        return Err(BuildError::input(format!(
            "unsafe archive basename {name:?}"
        )));
    }
    if is_reserved_windows_device(stem) {
        return Err(BuildError::input(format!(
            "reserved Windows archive name {name:?}"
        )));
    }
    Ok(())
}

fn validate_programmatic_archive_name(name: &str) -> Result<(), BuildError> {
    match validate_archive_stem(name) {
        Ok(()) => Ok(()),
        Err(ArchiveNameError::InvalidStem) => Err(BuildError::input(format!(
            "unsafe archive name {name:?}; expected 1-64 ASCII letters, digits, underscores, or hyphens"
        ))),
        Err(ArchiveNameError::ReservedWindowsName) => Err(BuildError::input(format!(
            "reserved Windows archive name {name:?}"
        ))),
    }
}

fn canonical_programmatic_source(source: &Path) -> Result<PathBuf, BuildError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| {
        BuildError::input(format!(
            "cannot inspect input {}: {error}",
            source.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BuildError::input(format!(
            "input must be a regular file, not a symlink: {}",
            source.display()
        )));
    }
    let canonical = fs::canonicalize(source).map_err(|error| {
        BuildError::input(format!(
            "cannot resolve input {}: {error}",
            source.display()
        ))
    })?;
    let canonical_metadata = fs::symlink_metadata(&canonical).map_err(|error| {
        BuildError::input(format!(
            "cannot inspect input {}: {error}",
            canonical.display()
        ))
    })?;
    if canonical_metadata.file_type().is_symlink() || !canonical_metadata.is_file() {
        return Err(BuildError::input(format!(
            "input must resolve to a regular file: {}",
            source.display()
        )));
    }
    Ok(canonical)
}

fn expand_programmatic_input(
    path: VirtualPath,
    source: PathBuf,
    compression: BuildCompression,
) -> Result<ExpandedInput, BuildError> {
    let source_root = source.parent().ok_or_else(|| {
        BuildError::input(format!(
            "input has no parent directory: {}",
            source.display()
        ))
    })?;
    Ok(ExpandedInput {
        path,
        source_root: source_root.to_path_buf(),
        source,
        compression,
    })
}

fn plan_programmatic_inputs(
    inputs: Vec<BuildFileInput>,
    defaults: BuildCompression,
    canonicalize_source: impl Fn(&Path) -> Result<PathBuf, BuildError>,
) -> Result<Vec<ExpandedInput>, BuildError> {
    inputs
        .into_iter()
        .map(|input| {
            let compression = BuildCompression {
                codec: input.codec.unwrap_or(defaults.codec),
                chunk_size: input.chunk_size.unwrap_or(defaults.chunk_size),
                zstd_level: input.zstd_level.unwrap_or(defaults.zstd_level),
            };
            validate_compression(compression)?;
            let source = canonicalize_source(&input.source)?;
            expand_programmatic_input(input.path, source, compression)
        })
        .collect()
}

fn canonical_source_root(root: &Path) -> Result<PathBuf, BuildError> {
    let metadata = fs::symlink_metadata(root).map_err(|error| {
        BuildError::input(format!(
            "cannot inspect source root {}: {error}",
            root.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BuildError::input(format!(
            "source root must be a real directory: {}",
            root.display()
        )));
    }
    fs::canonicalize(root).map_err(|error| {
        BuildError::input(format!(
            "cannot resolve source root {}: {error}",
            root.display()
        ))
    })
}

fn expand_selector(
    root: &Path,
    input: &BuildInput,
    compression: BuildCompression,
    output: &mut Vec<ExpandedInput>,
) -> Result<(), BuildError> {
    let selected = if input.path == Path::new(".") {
        root.to_path_buf()
    } else {
        root.join(&input.path)
    };
    ensure_no_symlink_components(root, &input.path)?;
    let metadata = fs::symlink_metadata(&selected).map_err(|error| {
        BuildError::input(format!(
            "cannot inspect selector {}: {error}",
            selected.display()
        ))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(BuildError::input(format!(
            "symlink input is not allowed: {}",
            selected.display()
        )));
    }
    let canonical = fs::canonicalize(&selected).map_err(|error| {
        BuildError::input(format!(
            "cannot resolve selector {}: {error}",
            selected.display()
        ))
    })?;
    if !canonical.starts_with(root) {
        return Err(BuildError::input(format!(
            "input escapes source root: {}",
            selected.display()
        )));
    }
    if metadata.is_file() {
        output.push(ExpandedInput {
            path: virtual_path_from_relative(root, &canonical)?,
            source_root: root.to_path_buf(),
            source: canonical,
            compression,
        });
    } else if metadata.is_dir() {
        let before = output.len();
        walk_directory(root, &canonical, compression, output)?;
        output[before..].sort_by(|left, right| left.path.as_str().cmp(right.path.as_str()));
    } else {
        return Err(BuildError::input(format!(
            "input selector is not a regular file or directory: {}",
            selected.display()
        )));
    }
    Ok(())
}

fn walk_directory(
    root: &Path,
    directory: &Path,
    compression: BuildCompression,
    output: &mut Vec<ExpandedInput>,
) -> Result<(), BuildError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            BuildError::input(format!(
                "cannot read directory {}: {error}",
                directory.display()
            ))
        })?
        .map(|entry| {
            entry.map(|entry| entry.path()).map_err(|error| {
                BuildError::input(format!(
                    "cannot read directory {}: {error}",
                    directory.display()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    for path in entries {
        let file_type = fs::symlink_metadata(&path)
            .map_err(|error| {
                BuildError::input(format!("cannot inspect input {}: {error}", path.display()))
            })?
            .file_type();
        if file_type.is_symlink() {
            return Err(BuildError::input(format!(
                "symlink input is not allowed: {}",
                path.display()
            )));
        }
        if file_type.is_dir() {
            walk_directory(root, &path, compression, output)?;
        } else if file_type.is_file() {
            let canonical = fs::canonicalize(&path).map_err(|error| {
                BuildError::input(format!("cannot resolve input {}: {error}", path.display()))
            })?;
            if !canonical.starts_with(root) {
                return Err(BuildError::input(format!(
                    "input escapes source root: {}",
                    path.display()
                )));
            }
            output.push(ExpandedInput {
                path: virtual_path_from_relative(root, &canonical)?,
                source_root: root.to_path_buf(),
                source: canonical,
                compression,
            });
        } else {
            return Err(BuildError::input(format!(
                "unsupported input type: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn virtual_path_from_relative(root: &Path, path: &Path) -> Result<VirtualPath, BuildError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        BuildError::input(format!("input is outside source root: {}", path.display()))
    })?;
    let mut value = String::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(BuildError::input(format!(
                "invalid source path: {}",
                path.display()
            )));
        };
        let part = part.to_str().ok_or_else(|| {
            BuildError::input(format!("input path is not valid UTF-8: {}", path.display()))
        })?;
        if !value.is_empty() {
            value.push('/');
        }
        value.push_str(part);
    }
    VirtualPath::parse(&value).map_err(|error| BuildError::input(error.to_string()))
}

fn ensure_no_symlink_components(root: &Path, relative: &Path) -> Result<(), BuildError> {
    if relative == Path::new(".") {
        return Ok(());
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(BuildError::input(format!(
                "invalid source selector {}",
                relative.display()
            )));
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current).map_err(|error| {
            BuildError::input(format!(
                "cannot inspect input {}: {error}",
                current.display()
            ))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(BuildError::input(format!(
                "symlink input is not allowed: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn hash_source(root: &Path, path: &Path) -> Result<(u64, ContentHash), BuildError> {
    let mut file = open_checked_source(root, path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            BuildError::input(format!("cannot read input {}: {error}", path.display()))
        })?;
        if read == 0 {
            break;
        }
        size = size.checked_add(read as u64).ok_or_else(|| {
            BuildError::input(format!("input size overflow for {}", path.display()))
        })?;
        hasher.update(&buffer[..read]);
    }
    Ok((size, ContentHash::from_bytes(*hasher.finalize().as_bytes())))
}

fn verify_prehashed_source(input: &PlannedInput) -> Result<(), BuildError> {
    let (size, content_hash) = hash_source(&input.source_root, &input.source)?;
    if size != input.size || input.content_hash != Some(content_hash) {
        return Err(BuildError::input(format!(
            "input changed after preflight: {}",
            input.path
        )));
    }
    Ok(())
}

fn open_checked_source(root: &Path, path: &Path) -> Result<File, BuildError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| BuildError::input(format!("input escaped source root: {}", path.display())))?;
    ensure_no_symlink_components(root, relative)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        BuildError::input(format!("cannot inspect input {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(BuildError::input(format!(
            "input changed type after preflight: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        BuildError::input(format!("cannot resolve input {}: {error}", path.display()))
    })?;
    if !canonical.starts_with(root) {
        return Err(BuildError::input(format!(
            "input escaped source root: {}",
            path.display()
        )));
    }
    File::open(canonical).map_err(|error| {
        BuildError::input(format!("cannot read input {}: {error}", path.display()))
    })
}

fn record_ignored_override(
    reports: &mut Vec<IgnoredCompressionOverride>,
    archive: &PlannedArchive,
    input: &PlannedInput,
    owner_archive: &str,
    owner_path: &VirtualPath,
    applied: BuildCompression,
) {
    if !compression_equivalent(input.compression, applied) {
        reports.push(IgnoredCompressionOverride {
            archive: archive.name.clone(),
            path: input.path.clone(),
            owner_archive: owner_archive.to_owned(),
            owner_path: owner_path.clone(),
            requested: input.compression,
            applied,
        });
    }
}

fn compression_equivalent(left: BuildCompression, right: BuildCompression) -> bool {
    left.codec == right.codec
        && left.chunk_size == right.chunk_size
        && (left.codec != Codec::Zstd || left.zstd_level == right.zstd_level)
}

fn absolute_lexical(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn resolve_missing_path(path: &Path) -> io::Result<PathBuf> {
    let mut ancestor = path.to_path_buf();
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => {
                let mut result = fs::canonicalize(&ancestor)?;
                for component in suffix.iter().rev() {
                    result.push(component);
                }
                return Ok(result);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "path has no existing ancestor")
                })?;
                suffix.push(name.to_os_string());
                if !ancestor.pop() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "path has no existing ancestor",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

struct StagingDirectory {
    path: Option<PathBuf>,
}

impl StagingDirectory {
    fn create(parent: &Path) -> Result<Self, BuildError> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..128 {
            let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = parent.join(format!(".rvepak-stage-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path: Some(path) }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(BuildError::operational(format!(
                        "cannot create staging directory {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Err(BuildError::operational(
            "cannot allocate a unique RVEPAK staging directory",
        ))
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("staging directory is armed")
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn publish_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    platform_publish_noreplace(source, destination)
}

#[cfg(target_os = "macos")]
fn platform_publish_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "output path contains NUL"))?;
    unsafe extern "C" {
        fn renamex_np(
            source: *const std::ffi::c_char,
            destination: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    const RENAME_EXCL: u32 = 0x0000_0004;
    // `renamex_np(RENAME_EXCL)` performs the directory rename atomically and fails if the
    // destination appeared after preflight.
    let result = unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn platform_publish_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "staging path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "output path contains NUL"))?;
    unsafe extern "C" {
        fn renameat2(
            old_dir: i32,
            old_path: *const std::ffi::c_char,
            new_dir: i32,
            new_path: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;
    let result = unsafe {
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            destination.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn platform_publish_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn MoveFileW(existing_name: *const u16, new_name: *const u16) -> i32;
    }
    // MoveFileW does not replace an existing destination; source and destination are siblings.
    let result = unsafe { MoveFileW(source.as_ptr(), destination.as_ptr()) };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn platform_publish_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace directory publication is supported only on macOS, Linux, and Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        BuildCompression, BuildErrorKind, BuildFileInput, BuildOptions, ProgrammaticArchive,
        build_archive_set, execute_with_publish_hook, parse_manifest, plan_programmatic_inputs,
        preflight,
    };
    use crate::{ArchiveCatalog, Codec};
    use engine_files::VirtualPath;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn virtual_path(value: &str) -> VirtualPath {
        VirtualPath::parse(value).unwrap()
    }

    fn programmatic_archive(name: &str, source: PathBuf, path: &str) -> ProgrammaticArchive {
        ProgrammaticArchive {
            name: name.into(),
            inputs: vec![BuildFileInput::new(source, virtual_path(path))],
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rvepak-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn publication_race_preserves_existing_destination_and_cleans_staging() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rvepak-publish-race-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("assets");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.bin"), b"source bytes").unwrap();
        let manifest = "root = \"assets\"\noutput = \"output\"\n[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"file.bin\"\n";
        let plan = preflight(
            parse_manifest(manifest, &base).unwrap(),
            BuildOptions::default(),
        )
        .unwrap();

        let error = execute_with_publish_hook(plan, |destination| {
            fs::create_dir(destination)?;
            fs::write(destination.join("preserved.txt"), b"existing destination")
        })
        .unwrap_err();

        assert_eq!(error.kind(), BuildErrorKind::Operational);
        assert_eq!(
            fs::read(base.join("output/preserved.txt")).unwrap(),
            b"existing destination"
        );
        let mut entries = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, ["assets", "output"].map(std::ffi::OsString::from));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn publication_race_does_not_replace_an_empty_destination() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "rvepak-empty-publish-race-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("assets");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file.bin"), b"source bytes").unwrap();
        let manifest = "root = \"assets\"\noutput = \"output\"\n[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"file.bin\"\n";
        let plan = preflight(
            parse_manifest(manifest, &base).unwrap(),
            BuildOptions::default(),
        )
        .unwrap();

        let error =
            execute_with_publish_hook(plan, |destination| fs::create_dir(destination)).unwrap_err();

        assert_eq!(error.kind(), BuildErrorKind::Operational);
        assert!(base.join("output").is_dir());
        assert_eq!(fs::read_dir(base.join("output")).unwrap().count(), 0);
        let mut entries = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, ["assets", "output"].map(std::ffi::OsString::from));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn programmatic_archive_build_uses_explicit_order_for_dedup_ownership() {
        let base = temp_dir("programmatic-order");
        let first_source = base.join("first.bin");
        let second_source = base.join("second.bin");
        fs::write(&first_source, b"shared payload").unwrap();
        fs::write(&second_source, b"shared payload").unwrap();
        let output = base.join("output");

        let report = build_archive_set(
            vec![
                programmatic_archive("first", first_source, "/first.bin"),
                programmatic_archive("second", second_source, "/second.bin"),
            ],
            &output,
            BuildCompression::default(),
            BuildOptions::default(),
        )
        .unwrap();
        assert_eq!(report.archives.len(), 2);
        assert_eq!(report.archives[0].name, "first.rvepak");
        assert_eq!(report.archives[1].name, "second.rvepak");

        let catalog =
            ArchiveCatalog::open_directory(&output, Default::default(), Default::default())
                .unwrap();
        let ids = catalog
            .archive_paths()
            .map(|(id, path)| (path.file_stem().unwrap().to_string_lossy().into_owned(), id))
            .collect::<std::collections::BTreeMap<_, _>>();
        let entries = catalog.entries().unwrap();
        let first = entries
            .iter()
            .find(|entry| entry.path == virtual_path("/first.bin"))
            .unwrap();
        let second = entries
            .iter()
            .find(|entry| entry.path == virtual_path("/second.bin"))
            .unwrap();
        assert_eq!(first.archive_id, ids["first"]);
        assert_eq!(first.owner_archive_id, ids["first"]);
        assert_eq!(second.archive_id, ids["second"]);
        assert_eq!(second.owner_archive_id, ids["first"]);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn programmatic_input_plan_keeps_a_separate_root_for_each_source() {
        #[cfg(windows)]
        let sources = [
            PathBuf::from(r"D:\generated\registry.toml"),
            PathBuf::from(r"C:\temporary\project.toml"),
        ];
        #[cfg(not(windows))]
        let sources = [
            PathBuf::from("/synthetic-d/generated/registry.toml"),
            PathBuf::from("/synthetic-c/temporary/project.toml"),
        ];

        let inputs = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                BuildFileInput::new(source, virtual_path(&format!("/generated/{index}.toml")))
            })
            .collect();
        let planned = plan_programmatic_inputs(inputs, BuildCompression::default(), |source| {
            Ok(source.to_path_buf())
        })
        .unwrap();

        assert_eq!(planned.len(), 2);
        assert_eq!(planned[0].source_root, sources[0].parent().unwrap());
        assert_eq!(planned[1].source_root, sources[1].parent().unwrap());
        assert!(!planned[0].source_root.starts_with(&planned[1].source_root));
        assert!(!planned[1].source_root.starts_with(&planned[0].source_root));
    }

    #[test]
    fn programmatic_archive_build_rejects_invalid_inputs_and_preserves_existing_output() {
        let base = temp_dir("programmatic-errors");
        let source = base.join("source.bin");
        fs::write(&source, b"source").unwrap();
        let output = base.join("existing");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep.txt"), b"keep").unwrap();

        let build = |archives| {
            build_archive_set(
                archives,
                base.join("new-output"),
                BuildCompression::default(),
                BuildOptions::default(),
            )
        };
        assert!(build(vec![programmatic_archive("bad.name", source.clone(), "/a")]).is_err());
        assert!(build(vec![programmatic_archive("CON", source.clone(), "/a")]).is_err());
        assert!(
            build(vec![
                programmatic_archive("one", source.clone(), "/same"),
                programmatic_archive("two", source.clone(), "/same")
            ])
            .is_err()
        );
        let mut entries_before = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries_before.sort();
        assert!(
            build(vec![programmatic_archive(
                "one",
                base.join("missing.bin"),
                "/a",
            )])
            .is_err()
        );
        let failed_output = base.join("new-output");
        assert!(!failed_output.exists());
        let mut entries_after = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries_after.sort();
        assert_eq!(entries_after, entries_before);
        assert!(build(vec![programmatic_archive("one", base.clone(), "/a")]).is_err());
        assert!(
            build(vec![ProgrammaticArchive {
                name: "one".into(),
                inputs: vec![]
            }])
            .is_err()
        );

        let error = build_archive_set(
            vec![programmatic_archive("one", source, "/a")],
            &output,
            BuildCompression::default(),
            BuildOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), BuildErrorKind::Input);
        assert_eq!(fs::read(output.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn programmatic_archive_build_rejects_symlink_sources() {
        use std::os::unix::fs::symlink;

        let base = temp_dir("programmatic-symlink");
        let source = base.join("source.bin");
        let link = base.join("link.bin");
        fs::write(&source, b"source").unwrap();
        symlink(&source, &link).unwrap();
        let error = build_archive_set(
            vec![programmatic_archive("one", link, "/a")],
            base.join("output"),
            BuildCompression::default(),
            BuildOptions::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), BuildErrorKind::Input);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn programmatic_build_honors_per_file_compression_overrides() {
        let base = temp_dir("programmatic-compression");
        let source = base.join("source.bin");
        fs::write(&source, b"uncompressed").unwrap();
        let mut input = BuildFileInput::new(&source, virtual_path("/raw.bin"));
        input.codec = Some(Codec::None);
        let report = build_archive_set(
            vec![ProgrammaticArchive {
                name: "base".into(),
                inputs: vec![input],
            }],
            base.join("output"),
            BuildCompression::default(),
            BuildOptions::default(),
        )
        .unwrap();
        assert_eq!(report.owners[0].codec, Codec::None);
        let _ = fs::remove_dir_all(base);
    }
}
