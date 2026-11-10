use engine_scene::ParamValue;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

const MAX_SAVE_FILE_BYTES: u64 = 1_048_576;

enum SaveMessage {
    Snapshot {
        generation: u64,
        values: BTreeMap<String, ParamValue>,
    },
    Flush {
        generation: u64,
        values: BTreeMap<String, ParamValue>,
        reply: SyncSender<SaveAck>,
    },
    Shutdown,
}

pub(crate) struct SaveAck {
    pub(crate) generation: u64,
    pub(crate) result: Result<(), String>,
}

/// Persistence runs off the scripts thread; only explicit flush and shutdown wait for disk I/O.
pub(crate) struct SaveWriter {
    path: PathBuf,
    sender: SyncSender<SaveMessage>,
    acknowledgements: Receiver<SaveAck>,
    worker: Option<JoinHandle<()>>,
}

impl SaveWriter {
    pub(crate) fn open(path: &Path) -> Result<(Self, BTreeMap<String, ParamValue>), String> {
        let values = read_values(path)?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let (ack_sender, acknowledgements) = mpsc::channel();
        let worker_path = path.to_owned();
        let worker = thread::Builder::new()
            .name("rve-save".to_owned())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        SaveMessage::Snapshot { generation, values } => {
                            let result = write_values(&worker_path, &values);
                            let _ = ack_sender.send(SaveAck { generation, result });
                        }
                        SaveMessage::Flush {
                            generation,
                            values,
                            reply,
                        } => {
                            let _ = reply.send(SaveAck {
                                generation,
                                result: write_values(&worker_path, &values),
                            });
                        }
                        SaveMessage::Shutdown => break,
                    }
                }
            })
            .map_err(|error| format!("cannot start save writer: {error}"))?;
        Ok((
            Self {
                path: path.to_owned(),
                sender,
                acknowledgements,
                worker: Some(worker),
            },
            values,
        ))
    }

    /// Queues a snapshot without waiting. `false` means a previous snapshot is still queued.
    pub(crate) fn snapshot(
        &self,
        generation: u64,
        values: BTreeMap<String, ParamValue>,
    ) -> Result<bool, String> {
        match self
            .sender
            .try_send(SaveMessage::Snapshot { generation, values })
        {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(format!(
                "save writer for {} has stopped",
                self.path.display()
            )),
        }
    }

    /// Persists the supplied snapshot and waits for the atomic replace to complete.
    pub(crate) fn flush(
        &self,
        generation: u64,
        values: BTreeMap<String, ParamValue>,
    ) -> Result<(), String> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .send(SaveMessage::Flush {
                generation,
                values,
                reply,
            })
            .map_err(|error| format!("save writer stopped: {error}"))?;
        let ack = result
            .recv()
            .map_err(|error| format!("save writer did not return a result: {error}"))?;
        if ack.generation != generation {
            return Err(
                "save writer returned an acknowledgement for the wrong generation".to_owned(),
            );
        }
        ack.result
    }

    /// Writes the final snapshot after script `on_destroy` and joins the worker.
    pub(crate) fn finish(
        mut self,
        generation: u64,
        values: BTreeMap<String, ParamValue>,
    ) -> Result<(), String> {
        let result = self.flush(generation, values);
        self.stop();
        result
    }

    pub(crate) fn drain_acknowledgements(&self) -> Vec<SaveAck> {
        let mut acknowledgements = Vec::new();
        while let Ok(acknowledgement) = self.acknowledgements.try_recv() {
            acknowledgements.push(acknowledgement);
        }
        acknowledgements
    }

    fn stop(&mut self) {
        let _ = self.sender.send(SaveMessage::Shutdown);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            log::error!("save writer for {} panicked", self.path.display());
        }
    }
}

impl Drop for SaveWriter {
    fn drop(&mut self) {
        self.stop();
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
        writer.flush(1, first.clone()).unwrap();
        let second = BTreeMap::from([("score".to_owned(), ParamValue::Int(44))]);
        writer.finish(2, second.clone()).unwrap();

        let (writer, loaded) = SaveWriter::open(&path).unwrap();
        assert_eq!(loaded, second);
        writer.finish(2, loaded.clone()).unwrap();
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

    #[test]
    fn asynchronous_write_failure_is_acknowledged_and_same_generation_retries() {
        let directory = temp_dir();
        fs::create_dir_all(&directory).unwrap();
        let blocked_parent = directory.join("blocked");
        fs::create_dir(&blocked_parent).unwrap();
        let path = blocked_parent.join("save.toml");
        let (writer, initial) = SaveWriter::open(&path).unwrap();
        assert!(initial.is_empty());
        fs::remove_dir(&blocked_parent).unwrap();
        fs::write(&blocked_parent, b"keep this file").unwrap();
        let values = BTreeMap::from([("score".to_owned(), ParamValue::Int(7))]);

        assert!(writer.snapshot(1, values.clone()).unwrap());
        assert!(writer.flush(1, values.clone()).is_err());
        let first_ack = writer
            .drain_acknowledgements()
            .into_iter()
            .find(|ack| ack.generation == 1)
            .expect("background write acknowledgement");
        assert!(first_ack.result.is_err());

        fs::remove_file(&blocked_parent).unwrap();
        fs::create_dir(&blocked_parent).unwrap();
        assert!(writer.snapshot(1, values.clone()).unwrap());
        writer.flush(1, values.clone()).unwrap();
        let retry_ack = writer
            .drain_acknowledgements()
            .into_iter()
            .find(|ack| ack.generation == 1)
            .expect("retry acknowledgement");
        assert!(retry_ack.result.is_ok());
        writer.finish(1, values.clone()).unwrap();
        assert_eq!(read_values(&path).unwrap(), values);
        fs::remove_dir_all(directory).unwrap();
    }

    fn temp_dir() -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rve-save-test-{}-{id}", std::process::id()))
    }
}
