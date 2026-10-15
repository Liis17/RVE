use engine_pak::{
    Archive, ArchiveCatalog, ArchiveId, BuildError, BuildErrorKind, BuildOptions, CatalogLimits,
    discover_archives, execute, load_manifest, preflight,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "Usage:\n  rvepak build <manifest.toml> [--no-dedup]\n  rvepak list <archive-directory>\n  rvepak verify <archive-directory>\n  rvepak --help";

struct CliError {
    code: u8,
    message: String,
}

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rvepak: {}", error.message);
            if error.code == 2 {
                eprintln!("{USAGE}");
            }
            ExitCode::from(error.code)
        }
    }
}

fn run(args: Vec<String>) -> Result<(), CliError> {
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    let Some(command) = args.first().map(String::as_str) else {
        return Err(bad_args("missing command"));
    };
    match command {
        "build" => run_build(&args[1..]),
        "list" => run_list(&args[1..]),
        "verify" => run_verify(&args[1..]),
        "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(bad_args(format!("unknown command {command:?}"))),
    }
}

fn run_build(args: &[String]) -> Result<(), CliError> {
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        if args.len() == 1 {
            println!("{USAGE}");
            return Ok(());
        }
        return Err(bad_args("help cannot be combined with build arguments"));
    }
    let (manifest_path, deduplicate) = match args {
        [manifest] => (manifest, true),
        [manifest, flag] if flag == "--no-dedup" => (manifest, false),
        _ => {
            return Err(bad_args(
                "build requires a manifest and optional --no-dedup",
            ));
        }
    };
    let manifest = load_manifest(manifest_path).map_err(build_error)?;
    let plan = preflight(manifest, BuildOptions { deduplicate }).map_err(build_error)?;
    let report = execute(plan).map_err(build_error)?;
    println!(
        "Built {} archive(s) in {}: {} input files, {} raw bytes, {} compressed archive bytes.",
        report.stats.output_archives,
        report.output_directory.display(),
        report.stats.input_files,
        report.stats.input_bytes,
        report.stats.output_bytes
    );
    if deduplicate {
        println!(
            "Deduplicated {} file(s), representing {} raw input bytes; {} local blob owner(s), {} cross-archive dependency path(s).",
            report.stats.deduplicated_files,
            report.stats.deduplicated_input_bytes,
            report.owners.len(),
            report.dependencies.len()
        );
    }
    for ignored in &report.ignored_overrides {
        println!(
            "Ignored compression override for {}:{}; bytes use owner {}:{} ({}).",
            ignored.archive,
            ignored.path,
            ignored.owner_archive,
            ignored.owner_path,
            compression_name(ignored.applied)
        );
    }
    Ok(())
}

fn run_list(args: &[String]) -> Result<(), CliError> {
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        if args.len() == 1 {
            println!("{USAGE}");
            return Ok(());
        }
        return Err(bad_args("help cannot be combined with list arguments"));
    }
    let [directory] = args else {
        return Err(bad_args("list requires one archive directory"));
    };
    let directory = PathBuf::from(directory);
    let (catalog, names) = open_catalog(&directory)?;
    println!("PATH\tSIZE\tCODEC\tOWNER\tDEPENDENCY");
    for entry in catalog.entries().map_err(pak_error)? {
        let owner = names
            .get(&entry.owner_archive_id)
            .map(String::as_str)
            .unwrap_or("<unknown>");
        let dependency = if entry.archive_id == entry.owner_archive_id {
            "-".to_owned()
        } else {
            names
                .get(&entry.archive_id)
                .map(|source| format!("{source} -> {owner}"))
                .unwrap_or_else(|| format!("<unknown> -> {owner}"))
        };
        println!(
            "{}\t{}\t{}\t{}#{}\t{}",
            entry.path,
            entry.size,
            codec_name(entry.codec),
            owner,
            entry.blob_id,
            dependency
        );
    }
    Ok(())
}

fn run_verify(args: &[String]) -> Result<(), CliError> {
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        if args.len() == 1 {
            println!("{USAGE}");
            return Ok(());
        }
        return Err(bad_args("help cannot be combined with verify arguments"));
    }
    let [directory] = args else {
        return Err(bad_args("verify requires one archive directory"));
    };
    let directory = PathBuf::from(directory);
    let (catalog, _) = open_catalog(&directory)?;
    let report = catalog.verify().map_err(pak_error)?;
    println!(
        "Verified {} local blob(s), {} decoded bytes, and {} path(s) across {} archive(s).",
        report.local_blobs_verified,
        report.decoded_bytes_verified,
        catalog.entries().map_err(pak_error)?.len(),
        discover_archives(&directory).map_err(pak_error)?.len()
    );
    Ok(())
}

fn open_catalog(
    directory: &Path,
) -> Result<(ArchiveCatalog, BTreeMap<ArchiveId, String>), CliError> {
    let discovered = discover_archives(directory).map_err(pak_error)?;
    let mut names = BTreeMap::new();
    for item in &discovered {
        let archive = Archive::open_file(&item.path).map_err(pak_error)?;
        let name = item
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<non-UTF-8-name>")
            .to_owned();
        names.insert(archive.archive_id(), name);
    }
    let catalog = ArchiveCatalog::open_directory(
        directory,
        engine_pak::ArchiveLimits::default(),
        CatalogLimits::default(),
    )
    .map_err(pak_error)?;
    Ok((catalog, names))
}

fn codec_name(codec: engine_pak::Codec) -> &'static str {
    match codec {
        engine_pak::Codec::None => "none",
        engine_pak::Codec::Zstd => "zstd",
        engine_pak::Codec::Lz4 => "lz4",
    }
}

fn compression_name(compression: engine_pak::BuildCompression) -> String {
    if compression.codec == engine_pak::Codec::Zstd {
        format!(
            "{} chunk_size={} zstd_level={}",
            codec_name(compression.codec),
            compression.chunk_size,
            compression.zstd_level
        )
    } else {
        format!(
            "{} chunk_size={}",
            codec_name(compression.codec),
            compression.chunk_size
        )
    }
}

fn build_error(error: BuildError) -> CliError {
    let code = match error.kind() {
        BuildErrorKind::Input => 2,
        BuildErrorKind::Operational => 1,
    };
    CliError {
        code,
        message: error.to_string(),
    }
}

fn pak_error(error: engine_pak::PakError) -> CliError {
    CliError {
        code: 1,
        message: error.to_string(),
    }
}

fn bad_args(message: impl Into<String>) -> CliError {
    CliError {
        code: 2,
        message: message.into(),
    }
}
