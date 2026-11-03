//! Графический редактор игровых сцен и ресурсов RVE.

mod app;
mod assets_fs;
mod autosave;
mod build;
mod cli;
mod console;
mod input;
mod layout;
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
    let mut editor = app::EditorApp::new(args.project, args.scripts);
    if let Err(error) = engine_window::run(config, &mut editor) {
        eprintln!("{error}");
        return ExitCode::from(1);
    }
    ExitCode::from(editor.exit_code)
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
    fn without_a_project_the_default_window_is_used() {
        let config = prepare(&cli::EditorArgs::default()).unwrap();
        assert_eq!((config.width, config.height), DEFAULT_WINDOW_SIZE);
    }
}
