use engine_scene::ParamValue;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

const MAX_SAVE_FILE_BYTES: u64 = 1_048_576;

enum SaveMessage {
    Snapshot(BTreeMap<String, ParamValue>),
    Flush(BTreeMap<String, ParamValue>, SyncSender<Result<(), String>>),
    Shutdown(Option<BTreeMap<String, ParamValue>>),
}

/// Persistence runs off the scripts thread; only explicit flush and shutdown wait for disk I/O.
pub(crate) struct SaveWriter {
    path: PathBuf,
    sender: SyncSender<SaveMessage>,
    worker: Option<JoinHandle<()>>,
}

impl SaveWriter {
    pub(crate) fn open(path: &Path) -> Result<(Self, BTreeMap<String, ParamValue>), String> {
        let values = read_values(path)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker_path = path.to_owned();
        let worker = thread::Builder::new()
            .name("rve-save".to_owned())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        SaveMessage::Snapshot(values) => {
                            if let Err(error) = write_values(&worker_path, &values) {
                                log::error!(
                                    "failed to save user data {}: {error}",
                                    worker_path.display()
                                );
                            }
                        }
                        SaveMessage::Flush(values, reply) => {
                            let _ = reply.send(write_values(&worker_path, &values));
                        }
                        SaveMessage::Shutdown(values) => {
                            if let Some(values) = values
                                && let Err(error) = write_values(&worker_path, &values)
                            {
                                log::error!(
                                    "failed to save user data {}: {error}",
                                    worker_path.display()
                                );
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|error| format!("cannot start save writer: {error}"))?;
        Ok((
            Self {
                path: path.to_owned(),
                sender,
                worker: Some(worker),
            },
            values,
        ))
    }

    /// Queues a snapshot without waiting. `false` means a previous snapshot is still queued.
    pub(crate) fn snapshot(&self, values: BTreeMap<String, ParamValue>) -> bool {
        match self.sender.try_send(SaveMessage::Snapshot(values)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                log::error!("save writer for {} has stopped", self.path.display());
                false
            }
        }
    }

    /// Persists the supplied snapshot and waits for the atomic replace to complete.
    pub(crate) fn flush(&self, values: BTreeMap<String, ParamValue>) -> Result<(), String> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .send(SaveMessage::Flush(values, reply))
            .map_err(|error| format!("save writer stopped: {error}"))?;
        result
            .recv()
            .map_err(|error| format!("save writer did not return a result: {error}"))?
    }

    /// Writes the final snapshot after script `on_destroy` and joins the worker.
    pub(crate) fn finish(mut self, values: BTreeMap<String, ParamValue>) -> Result<(), String> {
        self.flush(values)?;
        self.stop(None);
        Ok(())
    }

    fn stop(&mut self, final_values: Option<BTreeMap<String, ParamValue>>) {
        let _ = self.sender.send(SaveMessage::Shutdown(final_values));
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("save writer for {} panicked", self.path.display());
        }
    }
}

impl Drop for SaveWriter {
    fn drop(&mut self) {
        self.stop(None);
    }
}

fn read_values(path: &Path) -> Result<BTreeMap<String, ParamValue>, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.len() > MAX_SAVE_FILE_BYTES {
        return Err(format!("save file {} exceeds 1 MiB", path.display()));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let source = String::from_utf8(bytes)
        .map_err(|error| format!("save file {} is not UTF-8: {error}", path.display()))?;
    crate::decode_save_data(&source)
        .map_err(|error| format!("invalid save file {}: {error}", path.display()))
}

fn write_values(path: &Path, values: &BTreeMap<String, ParamValue>) -> Result<(), String> {
    let source = crate::encode_save_data(values)?;
    engine_platform::write_user_data_atomically(path, source.as_bytes())
        .map_err(|error| format!("cannot atomically replace {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn isolated_save_writer_roundtrips_and_atomically_overwrites_data() {
        let directory = temp_dir();
        let path = directory.join("player-save").join("save.toml");
        let (writer, initial) = SaveWriter::open(&path).unwrap();
        assert!(initial.is_empty());
        let first = BTreeMap::from([("score".to_owned(), ParamValue::Int(12))]);
        writer.flush(first.clone()).unwrap();
        let second = BTreeMap::from([("score".to_owned(), ParamValue::Int(44))]);
        writer.finish(second.clone()).unwrap();

        let (writer, loaded) = SaveWriter::open(&path).unwrap();
        assert_eq!(loaded, second);
        writer.finish(loaded.clone()).unwrap();
        assert_eq!(read_values(&path).unwrap(), second);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_replace_preserves_existing_path_contents() {
        let directory = temp_dir();
        let path = directory.join("save.toml");
        fs::create_dir_all(&path).unwrap();
        let sentinel = path.join("keep.txt");
        fs::write(&sentinel, b"old data").unwrap();
        let result = write_values(
            &path,
            &BTreeMap::from([("score".to_owned(), ParamValue::Int(1))]),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(sentinel).unwrap(), b"old data");
        fs::remove_dir_all(directory).unwrap();
    }

    fn temp_dir() -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rve-save-test-{}-{id}", std::process::id()))
    }
}
