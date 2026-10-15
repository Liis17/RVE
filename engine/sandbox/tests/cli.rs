use engine_files::VirtualPath;
use engine_pak::{ArchiveWriter, WriterOptions};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

fn temporary_directory(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("rve-cli-{label}-{}-{nonce}", std::process::id()));
    fs::create_dir_all(directory.join("assets")).unwrap();
    directory
}

fn write_archive(root: &Path, filename: &str, path: &str, bytes: &[u8]) {
    let mut writer = ArchiveWriter::new(
        fs::File::create(root.join(filename)).unwrap(),
        WriterOptions::default(),
    )
    .unwrap();
    writer
        .add_local(VirtualPath::parse(path).unwrap(), Cursor::new(bytes), None)
        .unwrap();
    drop(writer.finish().unwrap().into_inner());
}

fn run_procedural_scene(directory: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sandbox"))
        .args(["--scene", "triangle"])
        .current_dir(directory)
        .output()
        .expect("sandbox binary should start")
}

#[test]
fn invalid_arguments_exit_with_code_two_and_usage_before_opening_a_window() {
    for arguments in [
        vec!["--unknown"],
        vec!["--frames"],
        vec!["--frames", "0"],
        vec!["--frames", "18446744073709551616"],
        vec!["--render-scale", "NaN"],
        vec!["--render-scale", "0.5", "--render-scale", "1"],
        vec!["--resize-test", "--frames", "119"],
        vec!["--debug-overlay", "--debug-overlay"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sandbox"))
            .args(&arguments)
            .output()
            .expect("sandbox binary should start");
        let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");

        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {stderr}");
        assert!(stderr.contains("Ошибка аргументов:"), "{stderr}");
        assert!(
            stderr
                .contains("Использование: sandbox [--frames N] [--render-scale S] [--resize-test]"),
            "{stderr}"
        );
        assert!(output.stdout.is_empty(), "{arguments:?}");
    }
}

#[test]
fn missing_or_invalid_assets_exit_with_code_two_before_opening_a_window() {
    // Пустой рабочий каталог: bootstrap.rves и assets отсутствуют.
    let directory = std::env::temp_dir().join(format!("rve-cli-assets-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("temporary directory should be created");
    for (arguments, expected) in [
        (vec!["--model", "/model/missing.fbx"], "missing.fbx"),
        (vec!["--texture", "/texture/missing.png"], "missing.png"),
        (vec!["--model", "/model/cube.obj"], "ожидается fbx"),
        (
            vec!["--model", "/../outside.fbx"],
            "некорректный путь ресурса",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sandbox"))
            .args(&arguments)
            .current_dir(&directory)
            .output()
            .expect("sandbox binary should start");
        let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");

        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {stderr}");
        assert!(stderr.contains("Ошибка ресурса:"), "{stderr}");
        assert!(stderr.contains(expected), "{arguments:?}: {stderr}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn invalid_resource_catalogs_exit_with_code_two_before_procedural_window_startup() {
    let invalid = temporary_directory("invalid-archive");
    fs::write(invalid.join("assets/broken.rvepak"), b"not an archive").unwrap();
    assert_startup_resource_error(&invalid, "corrupt archive");

    let missing_owner = temporary_directory("missing-owner");
    let mut owner = ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    let owner_blob = owner
        .add_local(
            VirtualPath::parse("owner.png").unwrap(),
            Cursor::new(b"owner"),
            None,
        )
        .unwrap();
    let owner = owner.finish().unwrap();
    let owner_id = owner.archive_id;
    drop(owner);
    let mut dependent = ArchiveWriter::new(
        fs::File::create(missing_owner.join("assets/dependent.rvepak")).unwrap(),
        WriterOptions::default(),
    )
    .unwrap();
    dependent
        .add_external(
            VirtualPath::parse("missing.png").unwrap(),
            owner_id,
            owner_blob,
        )
        .unwrap();
    drop(dependent.finish().unwrap().into_inner());
    assert_startup_resource_error(&missing_owner, "external resource owner");

    let duplicate = temporary_directory("duplicate-path");
    write_archive(
        &duplicate.join("assets"),
        "a.rvepak",
        "duplicate.png",
        b"first",
    );
    write_archive(
        &duplicate.join("assets"),
        "b.rvepak",
        "duplicate.png",
        b"second",
    );
    assert_startup_resource_error(&duplicate, "duplicate resource path");

    fs::remove_dir_all(invalid).unwrap();
    fs::remove_dir_all(missing_owner).unwrap();
    fs::remove_dir_all(duplicate).unwrap();
}

fn assert_startup_resource_error(directory: &Path, expected: &str) {
    let output = run_procedural_scene(directory);
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Ошибка ресурса:"), "{stderr}");
    assert!(stderr.contains(expected), "{stderr}");
    assert!(output.stdout.is_empty(), "window startup must not run");
}

#[test]
fn triangle_scene_rejects_model_before_loading_assets() {
    let output = Command::new(env!("CARGO_BIN_EXE_sandbox"))
        .args(["--scene", "triangle", "--model", "/model/cube.fbx"])
        .output()
        .expect("sandbox binary should start");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Ошибка аргументов:"), "{stderr}");
}
