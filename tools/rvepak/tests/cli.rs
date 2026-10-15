use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rvepak-cli-test-{}-{}",
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

fn invoke(directory: &TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rvepak"))
        .args(args)
        .current_dir(directory.path())
        .output()
        .unwrap()
}

#[test]
fn build_list_verify_and_corruption_use_the_public_catalog_path() {
    let directory = TempDir::new();
    fs::create_dir_all(directory.path().join("assets/first")).unwrap();
    fs::create_dir_all(directory.path().join("assets/second")).unwrap();
    fs::write(
        directory.path().join("assets/first/shared.bin"),
        b"shared opaque payload",
    )
    .unwrap();
    fs::write(
        directory.path().join("assets/second/shared.bin"),
        b"shared opaque payload",
    )
    .unwrap();
    let manifest = r#"
root = "assets"
output = "built"

[defaults]
codec = "zstd"
chunk_size = 8
zstd_level = 3

[[archives]]
name = "base.rvepak"
[[archives.inputs]]
path = "first/shared.bin"

[[archives]]
name = "patch.rvepak"
[[archives.inputs]]
path = "second/shared.bin"
codec = "lz4"
"#;
    fs::write(directory.path().join("manifest.toml"), manifest).unwrap();

    let build = invoke(&directory, &["build", "manifest.toml"]);
    assert_eq!(
        build.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let build_stdout = String::from_utf8(build.stdout).unwrap();
    assert!(build_stdout.contains("Built 2 archive(s)"));
    assert!(build_stdout.contains("Deduplicated 1 file(s)"));
    assert!(build_stdout.contains("Ignored compression override"));

    let list = invoke(&directory, &["list", "built"]);
    assert_eq!(
        list.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8(list.stdout).unwrap();
    assert!(list_stdout.contains("first/shared.bin"));
    assert!(list_stdout.contains("second/shared.bin"));
    assert!(list_stdout.contains("base.rvepak#"));
    assert!(list_stdout.contains("patch.rvepak -> base.rvepak"));

    let verify = invoke(&directory, &["verify", "built"]);
    assert_eq!(
        verify.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        String::from_utf8(verify.stdout)
            .unwrap()
            .contains("Verified 1 local blob(s)")
    );

    let archive = directory.path().join("built/base.rvepak");
    let mut bytes = fs::read(&archive).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&archive, bytes).unwrap();
    let corrupted = invoke(&directory, &["verify", "built"]);
    assert_eq!(corrupted.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&corrupted.stderr).contains("corrupt archive"));
}

#[test]
fn no_dedup_flag_and_argument_errors_have_documented_exit_codes() {
    let directory = TempDir::new();
    fs::create_dir(directory.path().join("assets")).unwrap();
    fs::write(directory.path().join("assets/data.bin"), b"data").unwrap();
    fs::write(
        directory.path().join("manifest.toml"),
        "root = \"assets\"\noutput = \"built\"\n[[archives]]\nname = \"game.rvepak\"\n[[archives.inputs]]\npath = \"data.bin\"\n",
    )
    .unwrap();

    assert_eq!(invoke(&directory, &["--help"]).status.code(), Some(0));
    assert_eq!(
        invoke(&directory, &["build", "manifest.toml", "--no-dedup"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(invoke(&directory, &[]).status.code(), Some(2));
    assert_eq!(invoke(&directory, &["unknown"]).status.code(), Some(2));
    assert_eq!(
        invoke(&directory, &["build", "manifest.toml", "--bogus"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        invoke(&directory, &["verify", "missing"]).status.code(),
        Some(1)
    );
}
