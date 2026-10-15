use crate::format::{
    ArchiveId, BlobId, ChunkDescriptor, Codec, ContentHash, DEFAULT_CHUNK_SIZE, DEFAULT_ZSTD_LEVEL,
    FileReference, HEADER_SIZE, LocalBlob, MAX_LOGICAL_CHUNK_SIZE, encode_header, encode_index,
};
use crate::{PakError, Result};
use engine_files::VirtualPath;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

const WRITER_ZSTD_WINDOW_LOG: u32 = 23;

#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub chunk_size: u64,
    pub codec: Codec,
    pub zstd_level: i32,
    pub max_index_bytes: u64,
    pub max_path_bytes: u32,
    pub max_files: u32,
    pub max_local_blobs: u32,
    pub max_chunks: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct BlobOptions {
    pub codec: Codec,
    pub chunk_size: u64,
    pub zstd_level: i32,
}

impl From<&WriterOptions> for BlobOptions {
    fn from(options: &WriterOptions) -> Self {
        Self {
            codec: options.codec,
            chunk_size: options.chunk_size,
            zstd_level: options.zstd_level,
        }
    }
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            codec: Codec::Zstd,
            zstd_level: DEFAULT_ZSTD_LEVEL,
            max_index_bytes: 256 * 1024 * 1024,
            max_path_bytes: 64 * 1024,
            max_files: 1_000_000,
            max_local_blobs: 1_000_000,
            max_chunks: 4_000_000,
        }
    }
}

pub struct ArchiveWriter<W> {
    sink: W,
    options: WriterOptions,
    files: BTreeMap<VirtualPath, FileReference>,
    blobs: BTreeMap<BlobId, LocalBlob>,
    next_blob_id: u64,
    chunk_count: u32,
    poisoned: bool,
}

pub struct FinishedArchive<W> {
    pub archive_id: ArchiveId,
    /// Local blob ids and their whole-content BLAKE3 hashes, available to later archives.
    pub blobs: BTreeMap<BlobId, ContentHash>,
    sink: W,
}

impl<W> FinishedArchive<W> {
    pub fn into_inner(self) -> W {
        self.sink
    }
}

impl<W: Write + Seek> ArchiveWriter<W> {
    pub fn new(mut sink: W, options: WriterOptions) -> Result<Self> {
        validate_options(&options)?;
        let existing_len = sink.seek(SeekFrom::End(0)).map_err(PakError::io)?;
        if existing_len != 0 {
            return Err(PakError::InvalidConfiguration(
                "archive output must be empty".into(),
            ));
        }
        sink.write_all(&[0; HEADER_SIZE as usize])
            .map_err(PakError::io)?;
        Ok(Self {
            sink,
            options,
            files: BTreeMap::new(),
            blobs: BTreeMap::new(),
            next_blob_id: 0,
            chunk_count: 0,
            poisoned: false,
        })
    }

    /// Adds a local blob using the writer's default codec. Equal content is stored separately.
    pub fn add_local<R: Read>(
        &mut self,
        path: VirtualPath,
        source: R,
        expected_content_hash: Option<ContentHash>,
    ) -> Result<BlobId> {
        self.add_local_with_codec(path, source, self.options.codec, expected_content_hash)
    }

    /// Streams one local blob in bounded chunks. A caller-provided content hash is checked at EOF.
    pub fn add_local_with_codec<R: Read>(
        &mut self,
        path: VirtualPath,
        source: R,
        codec: Codec,
        expected_content_hash: Option<ContentHash>,
    ) -> Result<BlobId> {
        self.add_local_with_options(
            path,
            source,
            BlobOptions {
                codec,
                ..BlobOptions::from(&self.options)
            },
            expected_content_hash,
        )
    }

    /// Streams a local blob with per-input compression and chunk settings.
    pub fn add_local_with_options<R: Read>(
        &mut self,
        path: VirtualPath,
        mut source: R,
        blob_options: BlobOptions,
        expected_content_hash: Option<ContentHash>,
    ) -> Result<BlobId> {
        self.ensure_writable()?;
        self.validate_path(&path)?;
        validate_blob_options(blob_options)?;
        if self.files.contains_key(&path) {
            return Err(PakError::DuplicatePath(path));
        }
        if self.files.len() >= self.options.max_files as usize {
            return Err(PakError::InvalidConfiguration(
                "writer file count exceeds configured limit".into(),
            ));
        }
        if self.blobs.len() >= self.options.max_local_blobs as usize {
            return Err(PakError::InvalidConfiguration(
                "writer local blob count exceeds configured limit".into(),
            ));
        }

        let write_result = self.write_local(&mut source, blob_options, expected_content_hash);
        match write_result {
            Ok(blob) => {
                let id = blob.id;
                self.blobs.insert(id, blob);
                self.files.insert(path, FileReference::Local(id));
                Ok(id)
            }
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    /// Adds another path for an already-written local blob without storing another payload.
    pub fn add_alias(&mut self, path: VirtualPath, blob_id: BlobId) -> Result<()> {
        self.ensure_writable()?;
        self.validate_path(&path)?;
        if self.files.contains_key(&path) {
            return Err(PakError::DuplicatePath(path));
        }
        if !self.blobs.contains_key(&blob_id) {
            return Err(PakError::UnknownLocalBlob(blob_id));
        }
        if self.files.len() >= self.options.max_files as usize {
            return Err(PakError::InvalidConfiguration(
                "writer file count exceeds configured limit".into(),
            ));
        }
        self.files.insert(path, FileReference::Local(blob_id));
        Ok(())
    }

    /// Adds a path whose bytes are owned by another archive in the future catalog.
    pub fn add_external(
        &mut self,
        path: VirtualPath,
        archive_id: ArchiveId,
        blob_id: BlobId,
    ) -> Result<()> {
        self.ensure_writable()?;
        self.validate_path(&path)?;
        if self.files.contains_key(&path) {
            return Err(PakError::DuplicatePath(path));
        }
        if self.files.len() >= self.options.max_files as usize {
            return Err(PakError::InvalidConfiguration(
                "writer file count exceeds configured limit".into(),
            ));
        }
        self.files.insert(
            path,
            FileReference::External {
                archive_id,
                blob_id,
            },
        );
        Ok(())
    }

    pub fn finish(mut self) -> Result<FinishedArchive<W>> {
        self.ensure_writable()?;
        let index = encode_index(&self.files, &self.blobs, self.options.max_index_bytes)?;
        let index_offset = self.sink.stream_position().map_err(PakError::io)?;
        let index_len = u64::try_from(index.len())
            .map_err(|_| PakError::InvalidConfiguration("index size overflow".into()))?;
        let archive_id = ArchiveId::from_bytes(*blake3::hash(&index).as_bytes());
        self.sink.write_all(&index).map_err(PakError::io)?;
        let header = encode_header(index_offset, index_len, archive_id);
        self.sink.seek(SeekFrom::Start(0)).map_err(PakError::io)?;
        self.sink.write_all(&header).map_err(PakError::io)?;
        self.sink.flush().map_err(PakError::io)?;
        self.sink.seek(SeekFrom::End(0)).map_err(PakError::io)?;
        let blobs = self
            .blobs
            .iter()
            .map(|(id, blob)| (*id, blob.content_hash))
            .collect();
        Ok(FinishedArchive {
            archive_id,
            blobs,
            sink: self.sink,
        })
    }

    fn write_local<R: Read>(
        &mut self,
        source: &mut R,
        blob_options: BlobOptions,
        expected_content_hash: Option<ContentHash>,
    ) -> Result<LocalBlob> {
        let chunk_size = usize::try_from(blob_options.chunk_size).map_err(|_| {
            PakError::InvalidConfiguration("chunk size does not fit address space".into())
        })?;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(chunk_size)
            .map_err(|_| PakError::BudgetExceeded {
                requested: blob_options.chunk_size,
                limit: blob_options.chunk_size,
            })?;
        buffer.resize(chunk_size, 0);
        let mut content_hasher = blake3::Hasher::new();
        let mut total_decoded_size = 0_u64;
        let mut chunks = Vec::new();

        loop {
            let read = read_up_to(source, &mut buffer).map_err(PakError::io)?;
            if read == 0 {
                break;
            }
            if self.chunk_count >= self.options.max_chunks {
                return Err(PakError::InvalidConfiguration(
                    "writer chunk count exceeds configured limit".into(),
                ));
            }
            let decoded = &buffer[..read];
            content_hasher.update(decoded);
            let chunk_hash = *blake3::hash(decoded).as_bytes();
            let stored = encode_chunk(blob_options.codec, decoded, blob_options.zstd_level)?;
            let offset = self.sink.stream_position().map_err(PakError::io)?;
            self.sink.write_all(&stored).map_err(PakError::io)?;
            let decoded_len = u64::try_from(read).map_err(|_| {
                PakError::InvalidConfiguration("decoded chunk size overflow".into())
            })?;
            total_decoded_size = total_decoded_size
                .checked_add(decoded_len)
                .ok_or_else(|| PakError::InvalidConfiguration("source size overflow".into()))?;
            chunks.push(ChunkDescriptor {
                offset,
                stored_len: u64::try_from(stored.len()).map_err(|_| {
                    PakError::InvalidConfiguration("stored chunk size overflow".into())
                })?,
                decoded_len,
                hash: chunk_hash,
            });
            self.chunk_count += 1;
            if read < chunk_size {
                break;
            }
        }

        let content_hash = ContentHash::from_bytes(*content_hasher.finalize().as_bytes());
        if let Some(expected) = expected_content_hash
            && expected != content_hash
        {
            return Err(PakError::SourceMismatch {
                expected,
                actual: content_hash,
            });
        }
        let id = BlobId::from_u64(self.next_blob_id);
        self.next_blob_id = self
            .next_blob_id
            .checked_add(1)
            .ok_or_else(|| PakError::InvalidConfiguration("local blob id overflow".into()))?;
        Ok(LocalBlob {
            id,
            codec: blob_options.codec,
            total_decoded_size,
            logical_chunk_size: blob_options.chunk_size,
            content_hash,
            chunks,
        })
    }

    fn validate_path(&self, path: &VirtualPath) -> Result<()> {
        if path.as_bytes().len() > self.options.max_path_bytes as usize {
            return Err(PakError::InvalidConfiguration(
                "virtual path exceeds configured byte limit".into(),
            ));
        }
        Ok(())
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.poisoned {
            Err(PakError::PoisonedWriter)
        } else {
            Ok(())
        }
    }
}

fn validate_options(options: &WriterOptions) -> Result<()> {
    if options.max_path_bytes == 0 || options.max_index_bytes < 8 {
        return Err(PakError::InvalidConfiguration(
            "writer path and index limits are too small".into(),
        ));
    }
    validate_blob_options(BlobOptions::from(options))
}

fn validate_blob_options(options: BlobOptions) -> Result<()> {
    if options.chunk_size == 0 || options.chunk_size > MAX_LOGICAL_CHUNK_SIZE {
        return Err(PakError::InvalidConfiguration(
            "writer chunk size must be between 1 byte and 16 MiB".into(),
        ));
    }
    if options.codec == Codec::Zstd {
        let range = zstd::compression_level_range();
        if options.zstd_level < *range.start() || options.zstd_level > *range.end() {
            return Err(PakError::InvalidConfiguration(
                "Zstandard compression level is outside the supported range".into(),
            ));
        }
    }
    Ok(())
}

fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn encode_chunk(codec: Codec, decoded: &[u8], zstd_level: i32) -> Result<Vec<u8>> {
    match codec {
        Codec::None => Ok(decoded.to_vec()),
        Codec::Zstd => {
            let mut compressor = zstd::bulk::Compressor::new(zstd_level)
                .map_err(|error| PakError::Unsupported(format!("Zstandard encoder: {error}")))?;
            compressor
                .set_parameter(zstd::zstd_safe::CParameter::WindowLog(
                    WRITER_ZSTD_WINDOW_LOG,
                ))
                .map_err(|error| {
                    PakError::Unsupported(format!("Zstandard window limit: {error}"))
                })?;
            compressor
                .compress(decoded)
                .map_err(|error| PakError::Unsupported(format!("Zstandard encoder: {error}")))
        }
        Codec::Lz4 => Ok(lz4_flex::block::compress(decoded)),
    }
}
