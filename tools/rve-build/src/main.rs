mod archives;
mod cli;
mod error;
#[cfg(windows)]
mod ico;
mod platform;
mod player;
mod project;
mod sdk;
mod temporary;

use cli::CommandLine;
use std::process;

/// Версия сборщика из `Cargo.toml`.
#[allow(dead_code)]
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let code = match cli::parse(std::env::args_os().skip(1)) {
        Ok(CommandLine::Help) => {
            println!("{}", cli::USAGE);
            0
        }
        Ok(CommandLine::Scripts { project_dir }) => match player::scripts_project(&project_dir) {
            Ok(path) => {
                println!(
                    "{}",
                    path.map(|p| p.display().to_string()).unwrap_or_default()
                );
                0
            }
            Err(error) => {
                eprintln!("rve-build: {error}");
                error.exit_code()
            }
        },
        Ok(CommandLine::Build {
            project_dir,
            output,
            vulkan_sdk,
        }) => match player::build_distribution(&project_dir, &output, vulkan_sdk.as_deref()) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("rve-build: {error}");
                error.exit_code()
            }
        },
        Ok(CommandLine::Run {
            project_dir,
            player_args,
        }) => match player::run_project(&project_dir, &player_args) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("rve-build: {error}");
                error.exit_code()
            }
        },
        Ok(CommandLine::Sdk {
            output,
            rust_toolchain,
            vulkan_sdk,
        }) => match sdk::build_sdk(&output, rust_toolchain.as_deref(), vulkan_sdk.as_deref()) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("rve-build: {error}");
                error.exit_code()
            }
        },
        Err(error) => {
            eprintln!("rve-build: {error}");
            eprintln!("{}", cli::USAGE);
            2
        }
    };
    process::exit(code);
}
