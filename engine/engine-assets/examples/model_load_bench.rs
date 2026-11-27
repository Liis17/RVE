use engine_assets::{AssetError, Assets};
use std::fmt::Debug;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ITERATIONS: usize = 31;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rve-model-load-bench-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../examples/demo-game"));
    let source_root = fs::canonicalize(project.join("Assets"))?;
    let source = Assets::new(&source_root);
    let prepared_root = TempDir::new()?;
    copy_tree(&source_root, &prepared_root.0)?;
    let paths = fbx_paths(&source_root)?;

    for path in &paths {
        let asset_path = virtual_path(&source_root, path);
        let converted = source.convert_model(&asset_path)?;
        let relative = path.strip_prefix(&source_root)?;
        fs::write(prepared_root.0.join(relative), converted)?;
    }
    let prepared = Assets::new(&prepared_root.0);

    println!("demo-game FBX versus ready model; median of {ITERATIONS} loads");
    for path in paths {
        let asset_path = virtual_path(&source_root, &path);
        let direct_model = source.load_model(&asset_path);
        let prepared_model = prepared.load_model(&asset_path);
        assert_same_result(direct_model, prepared_model);

        let direct_animations = source.load_animations(&asset_path);
        let prepared_animations = prepared.load_animations(&asset_path);
        assert_same_result(direct_animations, prepared_animations);

        let direct_dependencies = source.model_dependencies(&asset_path);
        let prepared_dependencies = prepared.model_dependencies(&asset_path);
        assert_same_result(direct_dependencies, prepared_dependencies);

        println!("\n{asset_path}");
        report(
            "load_model",
            || source.load_model(&asset_path),
            || prepared.load_model(&asset_path),
        );
        report(
            "load_animations",
            || source.load_animations(&asset_path),
            || prepared.load_animations(&asset_path),
        );
        report(
            "model_dependencies",
            || source.model_dependencies(&asset_path),
            || prepared.model_dependencies(&asset_path),
        );
    }
    Ok(())
}

fn assert_same_result<T: PartialEq + Debug>(
    direct: Result<T, AssetError>,
    prepared: Result<T, AssetError>,
) {
    match (direct, prepared) {
        (Ok(direct), Ok(prepared)) => assert_eq!(direct, prepared),
        (Err(direct), Err(prepared)) => assert_eq!(
            std::mem::discriminant(&direct),
            std::mem::discriminant(&prepared)
        ),
        (direct, prepared) => {
            panic!("FBX and ready model outcomes differ: {direct:?} / {prepared:?}")
        }
    }
}

fn report<T>(name: &str, direct: impl FnMut() -> T, prepared: impl FnMut() -> T) {
    let direct = median(direct);
    let prepared = median(prepared);
    let speedup = direct.as_secs_f64() / prepared.as_secs_f64();
    println!(
        "  {name}: FBX {:.3} ms, ready {:.3} ms, {:.2}x",
        direct.as_secs_f64() * 1_000.0,
        prepared.as_secs_f64() * 1_000.0,
        speedup
    );
}

fn median<T>(mut load: impl FnMut() -> T) -> Duration {
    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        std::hint::black_box(load());
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    samples[ITERATIONS / 2]
}

fn fbx_paths(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect_fbx_paths(root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

fn collect_fbx_paths(directory: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_fbx_paths(&entry.path(), paths)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("fbx"))
        {
            paths.push(entry.path());
        }
    }
    Ok(())
}

fn virtual_path(root: &Path, path: &Path) -> String {
    format!(
        "/{}",
        path.strip_prefix(root)
            .expect("FBX path is below the asset root")
            .to_string_lossy()
            .replace('\\', "/")
    )
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::create_dir(&destination_path)?;
            copy_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, destination_path)?;
        }
    }
    Ok(())
}
