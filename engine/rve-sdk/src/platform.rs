//! Window control and system file dialogs for heavy scripts (ABI v24).
//!
//! Heavy scripts are native code, so files are read and written with `std::fs`. Dialogs go
//! through the host: it parents them to the game window and runs them without blocking frames.

use crate::Context;
use crate::abi::{self, RveFileDialog, RveFileDialogFilter, RveStr};
use std::path::PathBuf;

/// Kind of a system file dialog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileDialogKind {
    /// Pick one existing file.
    OpenFile,
    /// Pick several existing files.
    OpenFiles,
    /// Choose a path to save a file.
    SaveFile,
    /// Pick a folder.
    PickFolder,
}

impl FileDialogKind {
    fn code(self) -> u32 {
        match self {
            Self::OpenFile => abi::RVE_FILE_DIALOG_OPEN_FILE,
            Self::OpenFiles => abi::RVE_FILE_DIALOG_OPEN_FILES,
            Self::SaveFile => abi::RVE_FILE_DIALOG_SAVE_FILE,
            Self::PickFolder => abi::RVE_FILE_DIALOG_PICK_FOLDER,
        }
    }
}

/// Extension filter shown by a file dialog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDialogFilter {
    pub name: String,
    /// Extensions without a leading dot, for example `"png"`.
    pub extensions: Vec<String>,
}

/// System file dialog parameters; unset values leave the choice to the system.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDialog {
    pub kind: FileDialogKind,
    pub title: Option<String>,
    /// Initial folder.
    pub directory: Option<String>,
    /// Suggested file name.
    pub file_name: Option<String>,
    pub filters: Vec<FileDialogFilter>,
}

impl FileDialog {
    /// Creates a dialog of `kind` with system defaults.
    pub fn new(kind: FileDialogKind) -> Self {
        Self {
            kind,
            title: None,
            directory: None,
            file_name: None,
            filters: Vec::new(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn directory(mut self, directory: impl Into<String>) -> Self {
        self.directory = Some(directory.into());
        self
    }

    pub fn file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = Some(file_name.into());
        self
    }

    /// Adds an extension filter; extensions are given without a dot.
    pub fn filter(mut self, name: impl Into<String>, extensions: &[&str]) -> Self {
        self.filters.push(FileDialogFilter {
            name: name.into(),
            extensions: extensions
                .iter()
                .map(|&extension| extension.to_owned())
                .collect(),
        });
        self
    }
}

/// Identifier of a file dialog opened by [`Context::open_file_dialog`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileDialogId(u64);

impl FileDialogId {
    /// Raw host id, stable for the lifetime of the script host.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Result of [`Context::poll_file_dialog`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileDialogPoll {
    /// The dialog is still open.
    Pending,
    /// Selected paths; empty when the user cancelled the dialog.
    Done(Vec<PathBuf>),
}

fn optional_str(value: &Option<String>) -> RveStr {
    value
        .as_deref()
        .map_or(RveStr::empty(), RveStr::from_str_ref)
}

impl Context<'_> {
    /// Returns whether the window is in borderless fullscreen; `false` without a window or on
    /// hosts older than ABI v24.
    pub fn is_fullscreen(&self) -> bool {
        let Some(callback) = self.host.window_fullscreen else {
            return false;
        };
        let mut fullscreen = false;
        // SAFETY: the callback writes one bool into this local.
        unsafe { callback(self.host.call_ctx(), &mut fullscreen) && fullscreen }
    }

    /// Stages a borderless fullscreen change applied after this frame.
    pub fn set_fullscreen(&mut self, fullscreen: bool) -> bool {
        let Some(callback) = self.host.window_set_fullscreen else {
            return false;
        };
        // SAFETY: the host stages a request in the active transaction.
        unsafe { callback(self.host.call_ctx(), fullscreen) }
    }

    /// Stages a PNG screenshot of the window. `None` saves into the project's screenshot folder.
    pub fn take_screenshot(&mut self, path: Option<&str>) -> bool {
        let Some(callback) = self.host.window_screenshot else {
            return false;
        };
        let path = path.map_or(RveStr::empty(), RveStr::from_str_ref);
        // SAFETY: the path bytes live through this synchronous host call.
        unsafe { callback(self.host.call_ctx(), path) }
    }

    /// Stages a system file dialog that opens after this frame; read the result with
    /// [`Self::poll_file_dialog`]. Returns `None` when the host rejects the request.
    pub fn open_file_dialog(&mut self, dialog: &FileDialog) -> Option<FileDialogId> {
        let callback = self.host.file_dialog_open?;
        let extensions = dialog
            .filters
            .iter()
            .map(|filter| {
                filter
                    .extensions
                    .iter()
                    .map(|extension| RveStr::from_str_ref(extension))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let filters = dialog
            .filters
            .iter()
            .zip(&extensions)
            .map(|(filter, extensions)| RveFileDialogFilter {
                name: RveStr::from_str_ref(&filter.name),
                extensions: extensions.as_ptr(),
                extension_count: extensions.len(),
            })
            .collect::<Vec<_>>();
        let raw = RveFileDialog {
            kind: dialog.kind.code(),
            title: optional_str(&dialog.title),
            directory: optional_str(&dialog.directory),
            file_name: optional_str(&dialog.file_name),
            filters: filters.as_ptr(),
            filter_count: filters.len(),
        };
        let mut id = 0;
        // SAFETY: every borrowed string and array outlives this synchronous host call.
        (unsafe { callback(self.host.call_ctx(), &raw, &mut id) } && id != 0)
            .then_some(FileDialogId(id))
    }

    /// Reads a dialog result. A `Done` result is consumed when this callback succeeds, so the
    /// next frame reports the id as unknown and the host fails that call.
    pub fn poll_file_dialog(&mut self, id: FileDialogId) -> Option<FileDialogPoll> {
        let callback = self.host.file_dialog_poll?;
        let mut count = 0;
        let mut done = false;
        // SAFETY: the zero-capacity query only writes `count` and `done`.
        if !unsafe {
            callback(
                self.host.call_ctx(),
                id.0,
                std::ptr::null_mut(),
                0,
                &mut count,
                &mut done,
            )
        } {
            return None;
        }
        if !done {
            return Some(FileDialogPoll::Pending);
        }
        let mut paths = vec![RveStr::empty(); count];
        // SAFETY: the host writes at most `paths.len()` borrowed strings into this buffer.
        if !unsafe {
            callback(
                self.host.call_ctx(),
                id.0,
                paths.as_mut_ptr(),
                paths.len(),
                &mut count,
                &mut done,
            )
        } {
            return None;
        }
        paths
            .into_iter()
            .take(count)
            .map(|path| {
                // SAFETY: each path remains in the host call context until this callback returns.
                unsafe { path.to_str() }.ok().map(PathBuf::from)
            })
            .collect::<Option<Vec<_>>>()
            .map(FileDialogPoll::Done)
    }
}
