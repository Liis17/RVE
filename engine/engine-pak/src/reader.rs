use crate::format::{
    ArchiveId, BlobId, ChunkDescriptor, Codec, ContentHash, FileReference, LocalBlob, read_header,
};
use crate::{PakError, Result};
use engine_files::{FileSource, ReadAt, VirtualPath};
use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NONE_CODEC_SCRATCH_SIZE: u64 = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ArchiveLimits {
    /// Maximum serialized, uncompressed index length. This limits format bytes, not parsed metadata memory.
    pub max_index_bytes: u64,
    pub max_path_bytes: u32,
    pub max_files: u32,
    pub max_blobs: u32,
    pub max_chunks: u32,
    pub max_chunk_size: u64,
    pub max_stored_chunk_size: u64,
    pub max_read_result_bytes: u64,
    pub max_read_memory_bytes: u64,
    pub max_zstd_window_bytes: u64,
    pub zstd_workspace_overhead_bytes: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_index_bytes: 256 * 1024 * 1024,
            max_path_bytes: 64 * 1024,
            max_files: 1_000_000,
            max_blobs: 1_000_000,
            max_chunks: 4_000_000,
            max_chunk_size: crate::format::MAX_LOGICAL_CHUNK_SIZE,
            max_stored_chunk_size: 32 * 1024 * 1024,
            max_read_result_bytes: 256 * 1024 * 1024,
            max_read_memory_bytes: 512 * 1024 * 1024,
            max_zstd_window_bytes: 8 * 1024 * 1024,
            zstd_workspace_overhead_bytes: 512 * 1024,
        }
    }
}

impl ArchiveLimits {
    fn validate(&self) -> Result<()> {
        if self.max_path_bytes == 0
            || self.max_chunk_size == 0
            || self.max_chunk_size > crate::format::MAX_LOGICAL_CHUNK_SIZE
            || self.max_stored_chunk_size == 0
            || self.max_zstd_window_bytes < 1024
            || self.max_zstd_window_bytes > crate::format::MAX_LOGICAL_CHUNK_SIZE
            || !self.max_zstd_window_bytes.is_power_of_two()
        {
            return Err(PakError::InvalidConfiguration(
                "archive limits contain an invalid path, chunk, or Zstandard size".into(),
            ));
        }
        Ok(())
    }

    fn zstd_window_log(&self) -> u32 {
        self.max_zstd_window_bytes.trailing_zeros()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryFootprint {
    /// The returned byte vector.
    pub result_bytes: u64,
    /// The largest stored chunk buffer or raw-copy scratch buffer held at one time.
    pub stored_chunk_bytes: u64,
    /// The largest decoded chunk buffer held at one time.
    pub decoded_chunk_bytes: u64,
    /// The codec workspace reservation. Zstandard uses its configured maximum window plus a fixed overhead.
    pub codec_workspace_bytes: u64,
}

impl MemoryFootprint {
    pub fn total_bytes(self) -> Option<u64> {
        self.result_bytes
            .checked_add(self.stored_chunk_bytes)?
            .checked_add(self.decoded_chunk_bytes)?
            .checked_add(self.codec_workspace_bytes)
    }
}

/// A reservation held for the duration of a read and released by dropping it.
pub trait ReadReservation {}

/// Lets an asynchronous resource service reserve memory before a reader allocates.
pub trait ReadControl: Send + Sync {
    fn reserve(&self, memory: MemoryFootprint) -> Result<Box<dyn ReadReservation + '_>>;

    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopReadControl;

struct NoopReservation;
impl ReadReservation for NoopReservation {}

impl ReadControl for NoopReadControl {
    fn reserve(&self, _memory: MemoryFootprint) -> Result<Box<dyn ReadReservation + '_>> {
        Ok(Box::new(NoopReservation))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetStat {
    pub path: VirtualPath,
    pub blob_id: BlobId,
    pub size: u64,
    pub content_hash: ContentHash,
    pub codec: Codec,
    pub chunk_count: u32,
    pub owner_archive_id: ArchiveId,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VerifyReport {
    pub local_blobs_verified: u64,
    pub decoded_bytes_verified: u64,
}

/// Immutable metadata and a pinned backing source for one archive.
pub struct Archive {
    source: Arc<dyn ReadAt>,
    file_path: Option<PathBuf>,
    archive_id: ArchiveId,
    index_offset: u64,
    index_len: u64,
    #[cfg(windows)]
    data_offset: u64,
    #[cfg(windows)]
    data_len: u64,
    files: BTreeMap<VirtualPath, FileReference>,
    blobs: BTreeMap<BlobId, LocalBlob>,
    limits: ArchiveLimits,
}

impl Archive {
    pub fn open<R: ReadAt + 'static>(reader: R) -> Result<Self> {
        Self::open_with_limits(reader, ArchiveLimits::default())
    }

    pub fn open_with_limits<R: ReadAt + 'static>(reader: R, limits: ArchiveLimits) -> Result<Self> {
        Self::open_shared_with_limits(Arc::new(reader), limits)
    }

    pub fn open_shared(reader: Arc<dyn ReadAt>) -> Result<Self> {
        Self::open_shared_with_limits(reader, ArchiveLimits::default())
    }

    pub fn open_shared_with_limits(reader: Arc<dyn ReadAt>, limits: ArchiveLimits) -> Result<Self> {
        Self::open_source(reader, None, limits)
    }

    pub fn open_file(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_file_with_limits(path, ArchiveLimits::default())
    }

    pub fn open_file_with_limits(path: impl AsRef<Path>, limits: ArchiveLimits) -> Result<Self> {
        let original = path.as_ref();
        let canonical = fs::canonicalize(original)
            .map_err(|error| PakError::io_path(original.to_path_buf(), error))?;
        let source = FileSource::open(&canonical)
            .map_err(|error| PakError::io_path(canonical.clone(), error))?;
        Self::open_source(Arc::new(source), Some(canonical), limits)
    }

    fn open_source(
        source: Arc<dyn ReadAt>,
        file_path: Option<PathBuf>,
        limits: ArchiveLimits,
    ) -> Result<Self> {
        limits.validate()?;
        let file_len = source.len().map_err(PakError::io)?;
        let header = read_header(source.as_ref(), file_len)?;
        if header.index_len > limits.max_index_bytes {
            return Err(PakError::BudgetExceeded {
                requested: header.index_len,
                limit: limits.max_index_bytes,
            });
        }
        let index_len = usize::try_from(header.index_len)
            .map_err(|_| PakError::corrupt("index does not fit address space"))?;
        let mut index_bytes = allocate_zeroed(index_len, limits.max_index_bytes)?;
        source
            .read_exact_at(header.index_offset, &mut index_bytes)
            .map_err(PakError::io)?;
        if blake3::hash(&index_bytes).as_bytes() != header.archive_id.as_bytes() {
            return Err(PakError::corrupt(
                "index checksum does not match archive id",
            ));
        }
        let data_end = header
            .data_offset
            .checked_add(header.data_len)
            .ok_or_else(|| PakError::corrupt("data extent overflow"))?;
        let (files, blobs) =
            crate::format::parse_index(&index_bytes, header.data_offset, data_end, &limits)?;

        Ok(Self {
            source,
            file_path,
            archive_id: header.archive_id,
            index_offset: header.index_offset,
            index_len: header.index_len,
            #[cfg(windows)]
            data_offset: header.data_offset,
            #[cfg(windows)]
            data_len: header.data_len,
            files,
            blobs,
            limits,
        })
    }

    pub fn archive_id(&self) -> ArchiveId {
        self.archive_id
    }

    pub fn index_len(&self) -> u64 {
        self.index_len
    }

    pub fn index_offset(&self) -> u64 {
        self.index_offset
    }

    pub fn list(&self) -> impl Iterator<Item = &VirtualPath> {
        self.files.keys()
    }

    pub fn stat(&self, path: &VirtualPath) -> Result<AssetStat> {
        let reference = self
            .files
            .get(path)
            .ok_or_else(|| PakError::NotFound(path.clone()))?;
        let blob_id = match reference {
            FileReference::Local(blob_id) => *blob_id,
            FileReference::External { archive_id, .. } => {
                return Err(PakError::MissingOwner(*archive_id));
            }
        };
        self.stat_blob(path.clone(), blob_id)
    }

    pub fn read_all(&self, path: &VirtualPath) -> Result<Vec<u8>> {
        let size = self.stat(path)?.size;
        self.read_range(path, 0..size)
    }

    pub fn read_range(&self, path: &VirtualPath, range: Range<u64>) -> Result<Vec<u8>> {
        self.read_range_with_control(path, range, &NoopReadControl)
    }

    pub fn read_range_with_control(
        &self,
        path: &VirtualPath,
        range: Range<u64>,
        control: &dyn ReadControl,
    ) -> Result<Vec<u8>> {
        let stat = self.stat(path)?;
        let blob = self.blobs.get(&stat.blob_id).ok_or(PakError::MissingBlob {
            archive_id: self.archive_id,
            blob_id: stat.blob_id,
        })?;
        self.read_blob_range(blob, range, control)
    }

    pub fn verify(&self) -> Result<VerifyReport> {
        self.verify_with_control(&NoopReadControl)
    }

    pub fn verify_with_control(&self, control: &dyn ReadControl) -> Result<VerifyReport> {
        let mut report = VerifyReport::default();
        for blob in self.blobs.values() {
            self.verify_blob(blob, control)?;
            report.local_blobs_verified += 1;
            report.decoded_bytes_verified = report
                .decoded_bytes_verified
                .checked_add(blob.total_decoded_size)
                .ok_or_else(|| PakError::corrupt("verified byte count overflow"))?;
        }
        Ok(report)
    }

    /// Opens a fresh file handle for a worker and confirms the archive identity before use.
    pub fn reopen_for_worker(&self) -> Result<Self> {
        let Some(path) = &self.file_path else {
            return Err(PakError::Unsupported(
                "injected archive readers cannot be reopened".into(),
            ));
        };
        let reopened = Self::open_file_with_limits(path, self.limits.clone())?;
        if reopened.archive_id != self.archive_id {
            return Err(PakError::BackingChanged);
        }
        Ok(reopened)
    }

    pub(crate) fn file_reference(&self, path: &VirtualPath) -> Result<FileReference> {
        self.files
            .get(path)
            .copied()
            .ok_or_else(|| PakError::NotFound(path.clone()))
    }

    pub(crate) fn stat_blob(&self, path: VirtualPath, blob_id: BlobId) -> Result<AssetStat> {
        let blob = self.blobs.get(&blob_id).ok_or(PakError::MissingBlob {
            archive_id: self.archive_id,
            blob_id,
        })?;
        Ok(AssetStat {
            path,
            blob_id,
            size: blob.total_decoded_size,
            content_hash: blob.content_hash,
            codec: blob.codec,
            chunk_count: blob.chunks.len() as u32,
            owner_archive_id: self.archive_id,
        })
    }

    pub(crate) fn has_local_blob(&self, blob_id: BlobId) -> bool {
        self.blobs.contains_key(&blob_id)
    }

    pub(crate) fn read_blob_range_with_control(
        &self,
        blob_id: BlobId,
        range: Range<u64>,
        control: &dyn ReadControl,
    ) -> Result<Vec<u8>> {
        let blob = self.blobs.get(&blob_id).ok_or(PakError::MissingBlob {
            archive_id: self.archive_id,
            blob_id,
        })?;
        self.read_blob_range(blob, range, control)
    }

    pub(crate) fn verify_with_control_report(
        &self,
        control: &dyn ReadControl,
    ) -> Result<VerifyReport> {
        self.verify_with_control(control)
    }

    fn read_blob_range(
        &self,
        blob: &LocalBlob,
        range: Range<u64>,
        control: &dyn ReadControl,
    ) -> Result<Vec<u8>> {
        validate_range(&range, blob.total_decoded_size)?;
        let result_len = range.end - range.start;
        if result_len > self.limits.max_read_result_bytes {
            return Err(PakError::BudgetExceeded {
                requested: result_len,
                limit: self.limits.max_read_result_bytes,
            });
        }
        let result_len_usize =
            usize::try_from(result_len).map_err(|_| PakError::BudgetExceeded {
                requested: result_len,
                limit: self.limits.max_read_result_bytes,
            })?;
        let footprint = self.range_footprint(blob, &range, result_len)?;
        self.check_memory_limit(footprint)?;
        if control.is_cancelled() {
            return Err(PakError::Cancelled);
        }
        let _reservation = control.reserve(footprint)?;
        let mut result = allocate_zeroed(result_len_usize, self.limits.max_read_result_bytes)?;
        if result_len == 0 {
            return Ok(result);
        }

        let reader = self.reader_for_payload()?;
        let first_chunk = range.start / blob.logical_chunk_size;
        let last_chunk = (range.end - 1) / blob.logical_chunk_size;
        let mut scratch = if blob.codec == Codec::None {
            let chunk_index = usize::try_from(first_chunk)
                .map_err(|_| PakError::corrupt("chunk index does not fit address space"))?;
            let chunk = &blob.chunks[chunk_index];
            Some(allocate_zeroed(
                usize::try_from(chunk.decoded_len.min(NONE_CODEC_SCRATCH_SIZE))
                    .map_err(|_| PakError::corrupt("scratch size overflow"))?,
                self.limits.max_read_memory_bytes,
            )?)
        } else {
            None
        };

        for chunk_index in first_chunk..=last_chunk {
            if control.is_cancelled() {
                return Err(PakError::Cancelled);
            }
            let chunk = blob
                .chunks
                .get(
                    usize::try_from(chunk_index)
                        .map_err(|_| PakError::corrupt("chunk index does not fit address space"))?,
                )
                .ok_or_else(|| PakError::corrupt("chunk index outside local blob"))?;
            let chunk_start = chunk_index
                .checked_mul(blob.logical_chunk_size)
                .ok_or_else(|| PakError::corrupt("chunk start overflow"))?;
            let chunk_end = chunk_start
                .checked_add(chunk.decoded_len)
                .ok_or_else(|| PakError::corrupt("chunk end overflow"))?;
            let copy_start = range.start.max(chunk_start);
            let copy_end = range.end.min(chunk_end);
            let output_start = usize::try_from(copy_start - range.start)
                .map_err(|_| PakError::corrupt("range output offset overflow"))?;

            match blob.codec {
                Codec::None => {
                    let scratch = scratch.as_mut().expect("none codec has scratch");
                    self.read_none_chunk(
                        reader.as_ref(),
                        chunk,
                        chunk_start,
                        copy_start..copy_end,
                        &mut result[output_start..output_start + (copy_end - copy_start) as usize],
                        scratch,
                    )?;
                }
                Codec::Zstd | Codec::Lz4 => {
                    let decoded =
                        self.decode_compressed_chunk(reader.as_ref(), chunk, blob.codec)?;
                    if blake3::hash(&decoded).as_bytes() != &chunk.hash {
                        return Err(PakError::corrupt("chunk checksum mismatch"));
                    }
                    let local_start = usize::try_from(copy_start - chunk_start)
                        .map_err(|_| PakError::corrupt("chunk copy offset overflow"))?;
                    let count = usize::try_from(copy_end - copy_start)
                        .map_err(|_| PakError::corrupt("chunk copy size overflow"))?;
                    result[output_start..output_start + count]
                        .copy_from_slice(&decoded[local_start..local_start + count]);
                }
            }
        }
        Ok(result)
    }

    fn verify_blob(&self, blob: &LocalBlob, control: &dyn ReadControl) -> Result<()> {
        let footprint = self.range_footprint(blob, &(0..blob.total_decoded_size), 0)?;
        self.check_memory_limit(footprint)?;
        if control.is_cancelled() {
            return Err(PakError::Cancelled);
        }
        let _reservation = control.reserve(footprint)?;
        let reader = self.reader_for_payload()?;
        let mut content_hasher = blake3::Hasher::new();
        for chunk in &blob.chunks {
            if control.is_cancelled() {
                return Err(PakError::Cancelled);
            }
            match blob.codec {
                Codec::None => {
                    let mut scratch = allocate_zeroed(
                        usize::try_from(chunk.decoded_len.min(NONE_CODEC_SCRATCH_SIZE))
                            .map_err(|_| PakError::corrupt("scratch size overflow"))?,
                        self.limits.max_read_memory_bytes,
                    )?;
                    self.verify_none_chunk(
                        reader.as_ref(),
                        chunk,
                        &mut scratch,
                        &mut content_hasher,
                    )?;
                }
                Codec::Zstd | Codec::Lz4 => {
                    let decoded =
                        self.decode_compressed_chunk(reader.as_ref(), chunk, blob.codec)?;
                    if blake3::hash(&decoded).as_bytes() != &chunk.hash {
                        return Err(PakError::corrupt("chunk checksum mismatch"));
                    }
                    content_hasher.update(&decoded);
                }
            }
        }
        if content_hasher.finalize().as_bytes() != blob.content_hash.as_bytes() {
            return Err(PakError::corrupt("full blob checksum mismatch"));
        }
        Ok(())
    }

    fn read_none_chunk(
        &self,
        reader: &dyn ReadAt,
        chunk: &ChunkDescriptor,
        chunk_start: u64,
        copy_range: Range<u64>,
        output: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<()> {
        let mut hasher = blake3::Hasher::new();
        let mut position = 0_u64;
        let mut output_position = 0_usize;
        while position < chunk.decoded_len {
            let read_len =
                usize::try_from((chunk.decoded_len - position).min(scratch.len() as u64))
                    .map_err(|_| PakError::corrupt("uncompressed read size overflow"))?;
            let offset = chunk
                .offset
                .checked_add(position)
                .ok_or_else(|| PakError::corrupt("chunk read offset overflow"))?;
            reader
                .read_exact_at(offset, &mut scratch[..read_len])
                .map_err(PakError::io)?;
            hasher.update(&scratch[..read_len]);

            let part_start = chunk_start + position;
            let part_end = part_start + read_len as u64;
            let copy_start = copy_range.start.max(part_start);
            let copy_end = copy_range.end.min(part_end);
            if copy_start < copy_end {
                let source_start = usize::try_from(copy_start - part_start)
                    .map_err(|_| PakError::corrupt("chunk source offset overflow"))?;
                let count = usize::try_from(copy_end - copy_start)
                    .map_err(|_| PakError::corrupt("chunk copy size overflow"))?;
                output[output_position..output_position + count]
                    .copy_from_slice(&scratch[source_start..source_start + count]);
                output_position += count;
            }
            position = position
                .checked_add(read_len as u64)
                .ok_or_else(|| PakError::corrupt("chunk read position overflow"))?;
        }
        if hasher.finalize().as_bytes() != &chunk.hash {
            return Err(PakError::corrupt("chunk checksum mismatch"));
        }
        if output_position != output.len() {
            return Err(PakError::corrupt("range did not cover the expected output"));
        }
        Ok(())
    }

    fn verify_none_chunk(
        &self,
        reader: &dyn ReadAt,
        chunk: &ChunkDescriptor,
        scratch: &mut [u8],
        content_hasher: &mut blake3::Hasher,
    ) -> Result<()> {
        let mut chunk_hasher = blake3::Hasher::new();
        let mut position = 0_u64;
        while position < chunk.decoded_len {
            let read_len =
                usize::try_from((chunk.decoded_len - position).min(scratch.len() as u64))
                    .map_err(|_| PakError::corrupt("uncompressed read size overflow"))?;
            let offset = chunk
                .offset
                .checked_add(position)
                .ok_or_else(|| PakError::corrupt("chunk read offset overflow"))?;
            reader
                .read_exact_at(offset, &mut scratch[..read_len])
                .map_err(PakError::io)?;
            chunk_hasher.update(&scratch[..read_len]);
            content_hasher.update(&scratch[..read_len]);
            position = position
                .checked_add(read_len as u64)
                .ok_or_else(|| PakError::corrupt("chunk read position overflow"))?;
        }
        if chunk_hasher.finalize().as_bytes() != &chunk.hash {
            return Err(PakError::corrupt("chunk checksum mismatch"));
        }
        Ok(())
    }

    fn decode_compressed_chunk(
        &self,
        reader: &dyn ReadAt,
        chunk: &ChunkDescriptor,
        codec: Codec,
    ) -> Result<Vec<u8>> {
        let stored_len = usize::try_from(chunk.stored_len)
            .map_err(|_| PakError::corrupt("stored chunk does not fit address space"))?;
        let decoded_len = usize::try_from(chunk.decoded_len)
            .map_err(|_| PakError::corrupt("decoded chunk does not fit address space"))?;
        let mut stored = allocate_zeroed(stored_len, self.limits.max_read_memory_bytes)?;
        reader
            .read_exact_at(chunk.offset, &mut stored)
            .map_err(PakError::io)?;
        let mut decoded = allocate_zeroed(decoded_len, self.limits.max_read_memory_bytes)?;
        match codec {
            Codec::None => unreachable!("uncompressed chunks use the streaming path"),
            Codec::Zstd => {
                let window_size = zstd_frame_window_size(&stored, chunk.decoded_len)?;
                if window_size > self.limits.max_zstd_window_bytes {
                    return Err(PakError::BudgetExceeded {
                        requested: window_size,
                        limit: self.limits.max_zstd_window_bytes,
                    });
                }
                let frame_len = zstd::zstd_safe::find_frame_compressed_size(&stored)
                    .map_err(|_| PakError::corrupt("invalid Zstandard frame"))?;
                if frame_len != stored.len() {
                    return Err(PakError::corrupt(
                        "Zstandard chunk contains trailing data or extra frames",
                    ));
                }
                let mut decompressor =
                    zstd::zstd_safe::DCtx::try_create().ok_or(PakError::BudgetExceeded {
                        requested: self
                            .limits
                            .max_zstd_window_bytes
                            .saturating_add(self.limits.zstd_workspace_overhead_bytes),
                        limit: self.limits.max_read_memory_bytes,
                    })?;
                decompressor
                    .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
                        self.limits.zstd_window_log(),
                    ))
                    .map_err(|_| {
                        PakError::Unsupported(
                            "Zstandard frame window exceeds configured limit".into(),
                        )
                    })?;
                let written = decompressor
                    .decompress(&mut decoded, &stored)
                    .map_err(|_| PakError::corrupt("invalid Zstandard chunk"))?;
                if written != decoded_len {
                    return Err(PakError::corrupt("Zstandard decoded length mismatch"));
                }
            }
            Codec::Lz4 => {
                let written = lz4_flex::block::decompress_into(&stored, &mut decoded)
                    .map_err(|_| PakError::corrupt("invalid LZ4 block"))?;
                if written != decoded_len {
                    return Err(PakError::corrupt("LZ4 decoded length mismatch"));
                }
            }
        }
        Ok(decoded)
    }

    fn range_footprint(
        &self,
        blob: &LocalBlob,
        range: &Range<u64>,
        result_bytes: u64,
    ) -> Result<MemoryFootprint> {
        validate_range(range, blob.total_decoded_size)?;
        let mut footprint = MemoryFootprint {
            result_bytes,
            ..MemoryFootprint::default()
        };
        if range.is_empty() {
            return Ok(footprint);
        }
        let first = range.start / blob.logical_chunk_size;
        let last = (range.end - 1) / blob.logical_chunk_size;
        for index in first..=last {
            let chunk = blob
                .chunks
                .get(
                    usize::try_from(index)
                        .map_err(|_| PakError::corrupt("chunk index overflow"))?,
                )
                .ok_or_else(|| PakError::corrupt("chunk index outside local blob"))?;
            let (stored, decoded, workspace) = match blob.codec {
                Codec::None => (chunk.decoded_len.min(NONE_CODEC_SCRATCH_SIZE), 0, 0),
                Codec::Lz4 => (chunk.stored_len, chunk.decoded_len, 0),
                Codec::Zstd => (
                    chunk.stored_len,
                    chunk.decoded_len,
                    self.limits
                        .max_zstd_window_bytes
                        .checked_add(self.limits.zstd_workspace_overhead_bytes)
                        .ok_or_else(|| PakError::corrupt("Zstandard workspace size overflow"))?,
                ),
            };
            if stored > footprint.stored_chunk_bytes {
                footprint.stored_chunk_bytes = stored;
            }
            if decoded > footprint.decoded_chunk_bytes {
                footprint.decoded_chunk_bytes = decoded;
            }
            if workspace > footprint.codec_workspace_bytes {
                footprint.codec_workspace_bytes = workspace;
            }
        }
        Ok(footprint)
    }

    fn check_memory_limit(&self, footprint: MemoryFootprint) -> Result<()> {
        let total = footprint
            .total_bytes()
            .ok_or_else(|| PakError::corrupt("read memory footprint overflow"))?;
        if total > self.limits.max_read_memory_bytes {
            return Err(PakError::BudgetExceeded {
                requested: total,
                limit: self.limits.max_read_memory_bytes,
            });
        }
        Ok(())
    }

    fn reader_for_payload(&self) -> Result<Arc<dyn ReadAt>> {
        #[cfg(windows)]
        if let Some(path) = &self.file_path {
            let source = Arc::new(
                FileSource::open(path).map_err(|error| PakError::io_path(path.clone(), error))?,
            );
            let file_len = source.len().map_err(PakError::io)?;
            let opened = read_header(source.as_ref(), file_len)?;
            if opened.archive_id != self.archive_id
                || opened.index_offset != self.index_offset
                || opened.index_len != self.index_len
                || opened.data_offset != self.data_offset
                || opened.data_len != self.data_len
            {
                return Err(PakError::BackingChanged);
            }
            return Ok(source);
        }

        Ok(self.source.clone())
    }
}

fn validate_range(range: &Range<u64>, size: u64) -> Result<()> {
    if range.start > range.end || range.end > size {
        return Err(PakError::InvalidRange {
            start: range.start,
            end: range.end,
            size,
        });
    }
    Ok(())
}

fn allocate_zeroed(len: usize, limit: u64) -> Result<Vec<u8>> {
    let requested = u64::try_from(len).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| PakError::BudgetExceeded { requested, limit })?;
    bytes.resize(len, 0);
    Ok(bytes)
}

/// Reads the standard Zstandard frame header before creating a decoder context.
fn zstd_frame_window_size(frame: &[u8], expected_size: u64) -> Result<u64> {
    if frame.len() < 5 || frame[..4] != [0x28, 0xb5, 0x2f, 0xfd] {
        return Err(PakError::corrupt("invalid Zstandard frame header"));
    }
    let descriptor = frame[4];
    if descriptor & 0x08 != 0 {
        return Err(PakError::corrupt("reserved Zstandard frame bit is set"));
    }
    let content_size_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    let dictionary_id_flag = descriptor & 0x03;
    let mut cursor = 5_usize;

    let window_size = if single_segment {
        None
    } else {
        let descriptor = *frame
            .get(cursor)
            .ok_or_else(|| PakError::corrupt("truncated Zstandard window descriptor"))?;
        cursor += 1;
        let exponent = 10_u32 + u32::from(descriptor >> 3);
        let base = 1_u64
            .checked_shl(exponent)
            .ok_or_else(|| PakError::corrupt("Zstandard window size overflow"))?;
        Some(base + (base >> 3) * u64::from(descriptor & 0x07))
    };

    let dictionary_size = match dictionary_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    cursor = cursor
        .checked_add(dictionary_size)
        .ok_or_else(|| PakError::corrupt("Zstandard header size overflow"))?;
    let content_size_len = match (content_size_flag, single_segment) {
        (0, false) => 0,
        (0, true) => 1,
        (1, _) => 2,
        (2, _) => 4,
        (3, _) => 8,
        _ => unreachable!(),
    };
    let content_size = if content_size_len == 0 {
        None
    } else {
        let end = cursor
            .checked_add(content_size_len)
            .ok_or_else(|| PakError::corrupt("Zstandard header size overflow"))?;
        let bytes = frame
            .get(cursor..end)
            .ok_or_else(|| PakError::corrupt("truncated Zstandard content size"))?;
        let mut value = 0_u64;
        for (shift, byte) in bytes.iter().enumerate() {
            value |= u64::from(*byte) << (shift * 8);
        }
        if content_size_flag == 1 {
            value = value
                .checked_add(256)
                .ok_or_else(|| PakError::corrupt("Zstandard content size overflow"))?;
        }
        Some(value)
    };
    if content_size.is_some_and(|size| size != expected_size) {
        return Err(PakError::corrupt("Zstandard frame content size mismatch"));
    }
    window_size
        .or(content_size)
        .ok_or_else(|| PakError::corrupt("Zstandard single-segment frame has no content size"))
}
