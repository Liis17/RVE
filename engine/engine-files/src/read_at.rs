use std::fs::File;
use std::io;
use std::path::Path;

/// A thread-safe source that can read bytes at an absolute 64-bit offset.
pub trait ReadAt: Send + Sync {
    fn len(&self) -> io::Result<u64>;

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Reads up to `buffer.len()` bytes. Short reads are allowed.
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize>;

    /// Fills `buffer`, retrying interrupted and short reads.
    fn read_exact_at(&self, mut offset: u64, mut buffer: &mut [u8]) -> io::Result<()> {
        offset
            .checked_add(u64::try_from(buffer.len()).map_err(|_| invalid_offset())?)
            .ok_or_else(invalid_offset)?;

        while !buffer.is_empty() {
            match self.read_at(offset, buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill positional read buffer",
                    ));
                }
                Ok(read) if read <= buffer.len() => {
                    offset = offset.checked_add(read as u64).ok_or_else(invalid_offset)?;
                    buffer = &mut buffer[read..];
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "positional reader returned more bytes than requested",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }

        Ok(())
    }
}

/// A pinned file handle with positional reads. Windows serializes reads on one source;
/// archive workers reopen independent sources so this lock is never archive-wide.
pub struct FileSource {
    #[cfg(windows)]
    file: std::sync::Mutex<File>,
    #[cfg(not(windows))]
    file: File,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            #[cfg(windows)]
            file: std::sync::Mutex::new(File::open(path)?),
            #[cfg(not(windows))]
            file: File::open(path)?,
        })
    }

    pub fn from_file(file: File) -> Self {
        Self {
            #[cfg(windows)]
            file: std::sync::Mutex::new(file),
            #[cfg(not(windows))]
            file,
        }
    }
}

impl ReadAt for FileSource {
    fn len(&self) -> io::Result<u64> {
        #[cfg(windows)]
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("file source lock poisoned"))?;
        #[cfg(not(windows))]
        let file = &self.file;
        Ok(file.metadata()?.len())
    }

    #[cfg(unix)]
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        self.file.read_at(buffer, offset)
    }

    #[cfg(windows)]
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::fs::FileExt;
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("file source lock poisoned"))?;
        file.seek_read(buffer, offset)
    }

    #[cfg(not(any(unix, windows)))]
    fn read_at(&self, _offset: u64, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional file reads are unavailable on this target",
        ))
    }
}

fn invalid_offset() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "positional read offset overflow",
    )
}

#[cfg(test)]
mod tests {
    use super::ReadAt;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedReader {
        calls: AtomicUsize,
    }

    impl ReadAt for ScriptedReader {
        fn len(&self) -> io::Result<u64> {
            Ok(u64::MAX)
        }

        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
            if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if offset < (1_u64 << 32) + 3 {
                return Ok(0);
            }
            let start = (offset - ((1_u64 << 32) + 3)) as usize;
            let source = b"abcdef";
            let count = buffer.len().min(2).min(source.len().saturating_sub(start));
            buffer[..count].copy_from_slice(&source[start..start + count]);
            Ok(count)
        }
    }

    #[test]
    fn read_exact_at_retries_interrupted_short_reads_above_four_gib() {
        let reader = ScriptedReader {
            calls: AtomicUsize::new(0),
        };
        let mut bytes = [0; 5];

        reader.read_exact_at((1_u64 << 32) + 3, &mut bytes).unwrap();

        assert_eq!(&bytes, b"abcde");
    }

    #[test]
    fn read_exact_at_checks_offset_overflow_before_reading() {
        let reader = ScriptedReader {
            calls: AtomicUsize::new(0),
        };
        let mut bytes = [0; 2];

        assert_eq!(
            reader
                .read_exact_at(u64::MAX, &mut bytes)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(reader.calls.load(Ordering::Relaxed), 0);
    }
}
