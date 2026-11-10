use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

/// Returns the platform cache file path for a named application.
pub fn pipeline_cache_path(application: &str) -> Option<PathBuf> {
    super::user_cache_dir().map(|root| {
        root.join("RVE")
            .join(safe_cache_app_name(application))
            .join("pipeline.cache")
    })
}

/// Returns the per-user save file path for a named application.
pub fn save_data_path(application: &str) -> Option<PathBuf> {
    super::user_data_dir().map(|root| {
        root.join("RVE")
            .join(safe_cache_app_name(application))
            .join("save.toml")
    })
}

/// Returns the per-user screenshot directory for a named application, next to its save file.
pub fn screenshots_dir(application: &str) -> Option<PathBuf> {
    super::user_data_dir().map(|root| {
        root.join("RVE")
            .join(safe_cache_app_name(application))
            .join("Screenshots")
    })
}

/// Atomically replaces a user-data file after writing and syncing a sibling temporary file.
pub fn write_user_data_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "user data path has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    let (temporary_path, mut file) = loop {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(".rve-save-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => break (temporary_path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        super::replace_file(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

/// Replaces path separators and unsafe characters in an application cache directory name.
pub fn safe_cache_app_name(application: &str) -> String {
    let mut name = application
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    if name.is_empty() {
        name.push_str("app");
    }
    if matches!(
        name.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        name.insert(0, '_');
    }
    name
}

/// Reads a cache file, treating an absent file as an empty pipeline cache.
pub fn read_pipeline_cache(path: &Path) -> io::Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Loads cached bytes; cache I/O failures are reported to stderr and use an empty cache.
pub fn load_pipeline_cache(path: Option<&Path>) -> Vec<u8> {
    let Some(path) = path else {
        eprintln!("pipeline cache path is unavailable; starting with an empty cache");
        return Vec::new();
    };
    match read_pipeline_cache(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("failed to read pipeline cache {}: {error}", path.display());
            Vec::new()
        }
    }
}

/// Writes cache bytes to a sibling temporary file and atomically renames it over the target.
pub fn write_pipeline_cache(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipeline cache path has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;

    let (temporary_path, mut file) = loop {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary_path = parent.join(format!(
            ".rve-pipeline-cache-{}-{id}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => break (temporary_path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };

    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        super::replace_file(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

/// Saves cache bytes atomically; cache I/O failures are reported to stderr and ignored.
pub fn save_pipeline_cache(path: Option<&Path>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let Some(path) = path else {
        return;
    };
    if let Err(error) = write_pipeline_cache(path, bytes) {
        eprintln!("failed to write pipeline cache {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn cache_app_name_is_a_safe_nonempty_component() {
        assert_eq!(
            safe_cache_app_name("My Game/../Preview"),
            "My_Game____Preview"
        );
        assert_eq!(safe_cache_app_name(""), "app");
        assert_eq!(safe_cache_app_name("CON"), "_CON");
        assert_eq!(safe_cache_app_name(&"a".repeat(80)).len(), 64);
    }

    #[test]
    fn cache_file_read_defaults_to_empty_and_atomic_write_replaces_existing_data() {
        let directory = temporary_directory();
        let path = directory.join("RVE").join("sandbox").join("pipeline.cache");
        assert!(read_pipeline_cache(&path).unwrap().is_empty());
        write_pipeline_cache(&path, b"first cache").unwrap();
        assert_eq!(read_pipeline_cache(&path).unwrap(), b"first cache");
        write_pipeline_cache(&path, b"replacement cache").unwrap();
        assert_eq!(read_pipeline_cache(&path).unwrap(), b"replacement cache");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn user_save_path_is_scoped_by_application_and_atomic_write_preserves_old_file_on_error() {
        let directory = temporary_directory();
        let path = directory
            .join("user_data")
            .join("RVE")
            .join("sandbox")
            .join("save.toml");
        write_user_data_atomically(&path, b"first").unwrap();
        write_user_data_atomically(&path, b"replacement").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");

        let blocked_parent = directory.join("not-a-directory");
        fs::write(&blocked_parent, b"keep").unwrap();
        assert!(write_user_data_atomically(&blocked_parent.join("save.toml"), b"new").is_err());
        assert_eq!(fs::read(&blocked_parent).unwrap(), b"keep");
        assert_eq!(
            save_data_path("A game/Title").unwrap().file_name().unwrap(),
            "save.toml"
        );
        assert_eq!(
            screenshots_dir("A game/Title").unwrap().parent(),
            save_data_path("A game/Title").unwrap().parent()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    fn temporary_directory() -> PathBuf {
        let id = NEXT_TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rve-pipeline-cache-{}-{id}", std::process::id()))
    }
}
