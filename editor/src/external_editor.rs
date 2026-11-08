//! Non-blocking launch of the platform's default text editor for source assets.

use crate::project::OpenProject;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorPlatform {
    Macos,
    Windows,
    Other,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: &'static str,
    pub args: Vec<OsString>,
}

pub fn command_spec(path: &Path, platform: EditorPlatform) -> CommandSpec {
    match platform {
        EditorPlatform::Macos => CommandSpec {
            program: "open",
            args: vec![OsString::from("-t"), path.as_os_str().to_owned()],
        },
        EditorPlatform::Windows => CommandSpec {
            program: "notepad.exe",
            args: vec![path.as_os_str().to_owned()],
        },
        EditorPlatform::Other => CommandSpec {
            program: "xdg-open",
            args: vec![path.as_os_str().to_owned()],
        },
    }
}

/// Builds the command requested by the editor's host platform without invoking a shell.
pub fn text_editor_command(path: &Path) -> Command {
    let platform = if cfg!(target_os = "macos") {
        EditorPlatform::Macos
    } else if cfg!(target_os = "windows") {
        EditorPlatform::Windows
    } else {
        EditorPlatform::Other
    };
    let spec = command_spec(path, platform);
    let mut command = Command::new(spec.program);
    command.args(spec.args);
    command
}

#[derive(Default)]
pub struct ExternalEditor {
    sender: Option<Sender<String>>,
    receiver: Option<Receiver<String>>,
}

impl ExternalEditor {
    pub fn open(&mut self, project: &OpenProject, asset_path: &str) -> Result<(), String> {
        let path = project
            .assets_dir
            .resolve(asset_path)
            .map_err(|error| format!("{asset_path}: {error}"))?;
        let child = text_editor_command(&path)
            .spawn()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        self.wait_for(child, path);
        Ok(())
    }

    fn wait_for(&mut self, mut child: Child, path: std::path::PathBuf) {
        let (sender, receiver) = self
            .sender
            .take()
            .zip(self.receiver.take())
            .unwrap_or_else(mpsc::channel);
        self.sender = Some(sender.clone());
        self.receiver = Some(receiver);
        std::thread::spawn(move || {
            if let Err(error) = child.wait() {
                let _ = sender.send(format!("{}: {error}", path.display()));
            }
        });
    }

    pub fn drain_errors(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(receiver) = &self.receiver else {
            return errors;
        };
        while let Ok(error) = receiver.try_recv() {
            errors.push(error);
        }
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_platform_specific_and_preserves_path_as_one_argument() {
        let path = Path::new("/tmp/a file.rveshader");
        let mac = command_spec(path, EditorPlatform::Macos);
        assert_eq!(mac.program, "open");
        assert_eq!(mac.args, ["-t", "/tmp/a file.rveshader"]);
        let windows = command_spec(path, EditorPlatform::Windows);
        assert_eq!(windows.program, "notepad.exe");
        assert_eq!(windows.args, ["/tmp/a file.rveshader"]);
        let other = command_spec(path, EditorPlatform::Other);
        assert_eq!(other.program, "xdg-open");
        assert_eq!(other.args, ["/tmp/a file.rveshader"]);
    }
}
