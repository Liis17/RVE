//! Графический редактор игровых сцен и ресурсов RVE.

mod app;
mod assets_fs;
mod autosave;
mod build;
mod cli;
mod console;
mod gestures;
mod input;
mod layout;
mod new_project;
mod play;
mod previews;
mod project;
mod scene_edit;
mod settings;
mod state;
mod ui;
mod viewport;

use engine_project::ProjectSettings;
use engine_window::{RedrawMode, WindowConfig};
#[cfg(any(target_os = "macos", test))]
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_WINDOW_SIZE: (u32, u32) = (1600, 900);

/// Проверяет аргументы до создания окна и собирает его настройки.
fn prepare(args: &cli::EditorArgs) -> Result<WindowConfig, String> {
    let mut config = WindowConfig {
        title: "RVE Editor".to_owned(),
        width: DEFAULT_WINDOW_SIZE.0,
        height: DEFAULT_WINDOW_SIZE.1,
        icon: None,
        redraw: RedrawMode::OnDemand,
    };
    if let Some(scripts) = &args.scripts
        && !scripts.is_file()
    {
        return Err(format!(
            "{}: библиотека скриптов не найдена",
            scripts.display()
        ));
    }
    if let Some(dir) = &args.project {
        ProjectSettings::load(dir.join("project.rveproject"))?;
        let bootstrap = project::OpenProject::bootstrap(dir)?;
        config.width = bootstrap.window_size_x.unwrap_or(config.width);
        config.height = bootstrap.window_size_y.unwrap_or(config.height);
    }
    Ok(config)
}

fn main() -> ExitCode {
    let args = match cli::parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let config = match prepare(&args) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    prepare_vulkan_environment();
    let mut editor = app::EditorApp::new(args.project, args.scripts);
    let run_result = engine_window::run(config, &mut editor);
    editor.save_pipeline_cache();
    if let Err(error) = run_result {
        eprintln!("{error}");
        return ExitCode::from(1);
    }
    ExitCode::from(editor.exit_code)
}

#[cfg(target_os = "macos")]
fn prepare_vulkan_environment() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = executable.parent() else {
        return;
    };
    let root = editor_vulkan_root(exe_dir, true);
    // SAFETY: called in main before EditorApp or the window creates any threads.
    unsafe { engine_platform::prepare_vulkan_environment(&root) };
}

#[cfg(not(target_os = "macos"))]
fn prepare_vulkan_environment() {}

#[cfg(any(target_os = "macos", test))]
fn editor_vulkan_root(exe_dir: &Path, is_macos: bool) -> PathBuf {
    if is_macos
        && exe_dir.file_name().is_some_and(|name| name == "MacOS")
        && let Some(contents) = exe_dir.parent()
        && contents.file_name().is_some_and(|name| name == "Contents")
        && let Some(bundle) = contents.parent()
        && bundle
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
    {
        return contents.join("Resources");
    }
    exe_dir.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn missing_project_and_scripts_are_rejected_before_the_window_opens() {
        let missing = PathBuf::from("this/does/not/exist");
        assert!(
            prepare(&cli::EditorArgs {
                project: Some(missing.clone()),
                scripts: None,
            })
            .is_err()
        );
        assert!(
            prepare(&cli::EditorArgs {
                project: None,
                scripts: Some(missing),
            })
            .is_err()
        );
    }

    #[test]
    fn demo_project_window_comes_from_bootstrap_and_redraws_on_demand() {
        let dir = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/demo-game"
        ));
        let bootstrap = project::OpenProject::bootstrap(&dir).unwrap();
        let config = prepare(&cli::EditorArgs {
            project: Some(dir),
            scripts: None,
        })
        .unwrap();
        assert_eq!(config.redraw, RedrawMode::OnDemand);
        assert_eq!(
            config.width,
            bootstrap.window_size_x.unwrap_or(DEFAULT_WINDOW_SIZE.0)
        );
    }

    #[test]
    fn mac_editor_bundle_uses_contents_resources_for_vulkan_files() {
        assert_eq!(
            editor_vulkan_root(
                Path::new("/Applications/RVE Editor.app/Contents/MacOS"),
                true
            ),
            PathBuf::from("/Applications/RVE Editor.app/Contents/Resources")
        );
        assert_eq!(
            editor_vulkan_root(Path::new("/opt/rve"), true),
            PathBuf::from("/opt/rve")
        );
        assert_eq!(
            editor_vulkan_root(
                Path::new("/Applications/RVE Editor.app/Contents/MacOS"),
                false
            ),
            PathBuf::from("/Applications/RVE Editor.app/Contents/MacOS")
        );
    }

    #[test]
    fn without_a_project_the_default_window_is_used() {
        let config = prepare(&cli::EditorArgs::default()).unwrap();
        assert_eq!((config.width, config.height), DEFAULT_WINDOW_SIZE);
    }
}
