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

fn main() {
    let code = match cli::parse(std::env::args_os().skip(1)) {
        Ok(CommandLine::Help) => {
            println!("{}", cli::USAGE);
            0
        }
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
        Ok(CommandLine::Sdk { output }) => match sdk::build_sdk(&output) {
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
