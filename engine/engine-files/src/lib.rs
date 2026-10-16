//! Virtual resource paths and positional file reads shared by engine services.

mod path;
mod read_at;
mod virtual_path;

pub use path::strip_windows_verbatim_prefix;
pub use read_at::{FileSource, ReadAt};
pub use virtual_path::{VirtualPath, VirtualPathError};
