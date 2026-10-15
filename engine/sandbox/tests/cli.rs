use std::process::Command;

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
fn triangle_scene_rejects_model_before_loading_assets() {
    let output = Command::new(env!("CARGO_BIN_EXE_sandbox"))
        .args(["--scene", "triangle", "--model", "/model/cube.fbx"])
        .output()
        .expect("sandbox binary should start");
    let stderr = String::from_utf8(output.stderr).expect("diagnostics should be UTF-8");

    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("Ошибка аргументов:"), "{stderr}");
}
