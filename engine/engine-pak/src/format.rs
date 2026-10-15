use crate::{PakError, Result};
use engine_files::VirtualPath;
use std::fmt;

pub const MAGIC: [u8; 8] = *b"RVEPAK\0\0";
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: u64 = 64;
pub const DEFAULT_CHUNK_SIZE: u64 = 256 * 1024;
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const MAX_LOGICAL_CHUNK_SIZE: u64 = 16 * 1024 * 1024;

const FILE_LOCAL: u8 = 0;
const FILE_EXTERNAL: u8 = 1;
const CODEC_NONE: u8 = 0;
const CODEC_ZSTD: u8 = 1;
const CODEC_LZ4: u8 = 2;
pub(crate) const CHUNK_DESCRIPTOR_SIZE: usize = 56;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArchiveId([u8; 32]);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentHash([u8; 32]);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BlobId(u64);

macro_rules! id_impls {
    ($name:ident) => {
        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    };
}

id_impls!(ArchiveId);
id_impls!(ContentHash);
impl BlobId {
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Codec {
    None,
    Zstd,
    Lz4,
}

impl Codec {
    pub(crate) fn wire_tag(self) -> u8 {
        match self {
            Self::None => CODEC_NONE,
            Self::Zstd => CODEC_ZSTD,
            Self::Lz4 => CODEC_LZ4,
        }
    }

    pub(crate) fn from_wire_tag(tag: u8) -> Result<Self> {
        match tag {
            CODEC_NONE => Ok(Self::None),
            CODEC_ZSTD => Ok(Self::Zstd),
            CODEC_LZ4 => Ok(Self::Lz4),
            value => Err(PakError::Unsupported(format!("codec id {value}"))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileReference {
    Local(BlobId),
    External {
        archive_id: ArchiveId,
        blob_id: BlobId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkDescriptor {
    pub offset: u64,
    pub stored_len: u64,
    pub decoded_len: u64,
    pub hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalBlob {
    pub id: BlobId,
    pub codec: Codec,
    pub total_decoded_size: u64,
    pub logical_chunk_size: u64,
    pub content_hash: ContentHash,
    pub chunks: Vec<ChunkDescriptor>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParsedHeader {
    pub archive_id: ArchiveId,
    pub index_offset: u64,
    pub index_len: u64,
}

pub(crate) fn decode_header(
    bytes: &[u8; HEADER_SIZE as usize],
    file_len: u64,
) -> Result<ParsedHeader> {
    if bytes[..8] != MAGIC {
        return Err(PakError::corrupt("bad magic"));
    }

    let version = read_u32(bytes, 8)?;
    if version != VERSION {
        return Err(PakError::Unsupported(format!("archive version {version}")));
    }

    let flags = read_u32(bytes, 12)?;
    if flags != 0 {
        return Err(PakError::Unsupported(format!("header flags 0x{flags:08x}")));
    }

    let index_offset = read_u64(bytes, 16)?;
    let index_len = read_u64(bytes, 24)?;
    let index_end = index_offset
        .checked_add(index_len)
        .ok_or_else(|| PakError::corrupt("index extent overflow"))?;
    if index_offset < HEADER_SIZE || index_end != file_len {
        return Err(PakError::corrupt(
            "index must end at the end of the archive",
        ));
    }

    let mut index_hash = [0; 32];
    index_hash.copy_from_slice(&bytes[32..64]);

    Ok(ParsedHeader {
        archive_id: ArchiveId(index_hash),
        index_offset,
        index_len,
    })
}

pub(crate) fn encode_header(index_offset: u64, index_len: u64, archive_id: ArchiveId) -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[..8].copy_from_slice(&MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&0_u32.to_le_bytes());
    bytes[16..24].copy_from_slice(&index_offset.to_le_bytes());
    bytes[24..32].copy_from_slice(&index_len.to_le_bytes());
    bytes[32..64].copy_from_slice(archive_id.as_bytes());
    bytes
}

pub(crate) fn parse_index(
    bytes: &[u8],
    index_offset: u64,
    limits: &crate::ArchiveLimits,
) -> Result<(
    std::collections::BTreeMap<VirtualPath, FileReference>,
    std::collections::BTreeMap<BlobId, LocalBlob>,
)> {
    let mut cursor = IndexCursor::new(bytes);
    let file_count = cursor.u32()?;
    let blob_count = cursor.u32()?;
    if file_count > limits.max_files || blob_count > limits.max_blobs {
        return Err(PakError::corrupt(
            "index entry count exceeds configured limit",
        ));
    }
    if usize::try_from(file_count).map_err(|_| PakError::corrupt("file count overflow"))?
        > cursor.remaining() / 14
    {
        return Err(PakError::corrupt(
            "file count exceeds remaining index bytes",
        ));
    }

    let mut files = std::collections::BTreeMap::new();
    let mut previous_path: Option<VirtualPath> = None;
    for _ in 0..file_count {
        let path_len = cursor.u32()?;
        if path_len == 0 || path_len > limits.max_path_bytes {
            return Err(PakError::corrupt("path length exceeds configured limit"));
        }
        let path_bytes = cursor.take(path_len as usize)?;
        let path_text = std::str::from_utf8(path_bytes)
            .map_err(|_| PakError::corrupt("path is not valid UTF-8"))?;
        let path = VirtualPath::parse(path_text)
            .map_err(|_| PakError::corrupt("path is not canonical"))?;
        if path.as_bytes() != path_bytes {
            return Err(PakError::corrupt("path is not canonical"));
        }
        if previous_path
            .as_ref()
            .is_some_and(|previous| previous >= &path)
        {
            return Err(PakError::corrupt("file paths are not strictly sorted"));
        }
        previous_path = Some(path.clone());

        let reference = match cursor.u8()? {
            FILE_LOCAL => FileReference::Local(BlobId(cursor.u64()?)),
            FILE_EXTERNAL => FileReference::External {
                archive_id: ArchiveId(cursor.array32()?),
                blob_id: BlobId(cursor.u64()?),
            },
            value => return Err(PakError::Unsupported(format!("file reference id {value}"))),
        };
        files.insert(path, reference);
    }

    let mut blobs = std::collections::BTreeMap::new();
    let mut previous_blob: Option<BlobId> = None;
    let mut chunk_total = 0_u32;
    let mut extents = Vec::new();
    for _ in 0..blob_count {
        let id = BlobId(cursor.u64()?);
        if previous_blob.is_some_and(|previous| previous >= id) {
            return Err(PakError::corrupt("local blobs are not strictly sorted"));
        }
        previous_blob = Some(id);
        let codec = Codec::from_wire_tag(cursor.u8()?)?;
        let total_decoded_size = cursor.u64()?;
        let logical_chunk_size = cursor.u64()?;
        let content_hash = ContentHash::from_bytes(cursor.array32()?);
        let chunk_count = cursor.u32()?;
        if logical_chunk_size == 0 || logical_chunk_size > limits.max_chunk_size {
            return Err(PakError::corrupt(
                "logical chunk size exceeds configured limit",
            ));
        }
        chunk_total = chunk_total
            .checked_add(chunk_count)
            .ok_or_else(|| PakError::corrupt("chunk count overflow"))?;
        if chunk_total > limits.max_chunks
            || usize::try_from(chunk_count)
                .map_err(|_| PakError::corrupt("chunk count overflow"))?
                > cursor.remaining() / CHUNK_DESCRIPTOR_SIZE
        {
            return Err(PakError::corrupt(
                "chunk count exceeds configured or index limit",
            ));
        }
        let expected_count = total_decoded_size / logical_chunk_size
            + u64::from(total_decoded_size % logical_chunk_size != 0);
        if expected_count != u64::from(chunk_count) {
            return Err(PakError::corrupt("chunk count does not cover decoded size"));
        }

        let chunk_capacity = usize::try_from(chunk_count)
            .map_err(|_| PakError::corrupt("chunk count does not fit address space"))?;
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(chunk_capacity)
            .map_err(|_| PakError::BudgetExceeded {
                requested: u64::from(chunk_count) * CHUNK_DESCRIPTOR_SIZE as u64,
                limit: bytes.len() as u64,
            })?;
        extents
            .try_reserve(chunk_capacity)
            .map_err(|_| PakError::BudgetExceeded {
                requested: u64::from(chunk_count) * std::mem::size_of::<(u64, u64)>() as u64,
                limit: bytes.len() as u64,
            })?;
        let mut decoded_total = 0_u64;
        for chunk_index in 0..chunk_count {
            let offset = cursor.u64()?;
            let stored_len = cursor.u64()?;
            let decoded_len = cursor.u64()?;
            let hash = cursor.array32()?;
            let expected_len = if chunk_index + 1 == chunk_count {
                total_decoded_size
                    .checked_sub(decoded_total)
                    .ok_or_else(|| PakError::corrupt("decoded size underflow"))?
            } else {
                logical_chunk_size
            };
            if decoded_len == 0 || decoded_len != expected_len || stored_len == 0 {
                return Err(PakError::corrupt("invalid chunk size"));
            }
            if stored_len > limits.max_stored_chunk_size {
                return Err(PakError::corrupt("stored chunk exceeds configured limit"));
            }
            if decoded_len > limits.max_chunk_size {
                return Err(PakError::corrupt("decoded chunk exceeds configured limit"));
            }
            if codec == Codec::None && stored_len != decoded_len {
                return Err(PakError::corrupt("uncompressed chunk sizes do not match"));
            }
            let end = offset
                .checked_add(stored_len)
                .ok_or_else(|| PakError::corrupt("chunk extent overflow"))?;
            if offset < HEADER_SIZE || end > index_offset {
                return Err(PakError::corrupt(
                    "chunk extent is outside archive data region",
                ));
            }
            decoded_total = decoded_total
                .checked_add(decoded_len)
                .ok_or_else(|| PakError::corrupt("decoded chunk total overflow"))?;
            extents.push((offset, end));
            chunks.push(ChunkDescriptor {
                offset,
                stored_len,
                decoded_len,
                hash,
            });
        }
        if decoded_total != total_decoded_size {
            return Err(PakError::corrupt("decoded chunks do not cover local blob"));
        }
        if total_decoded_size == 0 && content_hash.as_bytes() != blake3::hash(&[]).as_bytes() {
            return Err(PakError::corrupt("empty blob content hash is invalid"));
        }
        blobs.insert(
            id,
            LocalBlob {
                id,
                codec,
                total_decoded_size,
                logical_chunk_size,
                content_hash,
                chunks,
            },
        );
    }
    if cursor.remaining() != 0 {
        return Err(PakError::corrupt("trailing bytes in index"));
    }

    extents.sort_unstable();
    if extents.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(PakError::corrupt("local chunk extents overlap"));
    }

    let mut referenced_local = std::collections::BTreeSet::new();
    for reference in files.values() {
        if let FileReference::Local(id) = reference {
            if !blobs.contains_key(id) {
                return Err(PakError::corrupt("file references a missing local blob"));
            }
            referenced_local.insert(*id);
        }
    }
    if referenced_local.len() != blobs.len() {
        return Err(PakError::corrupt(
            "local blob is not referenced by a file path",
        ));
    }

    Ok((files, blobs))
}

pub(crate) fn encode_index(
    files: &std::collections::BTreeMap<VirtualPath, FileReference>,
    blobs: &std::collections::BTreeMap<BlobId, LocalBlob>,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    let file_count = u32::try_from(files.len())
        .map_err(|_| PakError::InvalidConfiguration("too many file paths".into()))?;
    let blob_count = u32::try_from(blobs.len())
        .map_err(|_| PakError::InvalidConfiguration("too many local blobs".into()))?;
    let mut size = 8_u64;
    for (path, reference) in files {
        let path_len = u32::try_from(path.as_bytes().len())
            .map_err(|_| PakError::InvalidConfiguration("virtual path is too long".into()))?;
        size = size
            .checked_add(4 + u64::from(path_len) + 1)
            .and_then(|value| {
                value.checked_add(match reference {
                    FileReference::Local(_) => 8,
                    FileReference::External { .. } => 40,
                })
            })
            .ok_or_else(|| PakError::InvalidConfiguration("index size overflow".into()))?;
    }
    for blob in blobs.values() {
        size = size
            .checked_add(8 + 1 + 8 + 8 + 32 + 4)
            .and_then(|value| {
                (CHUNK_DESCRIPTOR_SIZE as u64)
                    .checked_mul(blob.chunks.len() as u64)
                    .and_then(|chunk_bytes| value.checked_add(chunk_bytes))
            })
            .ok_or_else(|| PakError::InvalidConfiguration("index size overflow".into()))?;
    }
    if size > max_bytes {
        return Err(PakError::BudgetExceeded {
            requested: size,
            limit: max_bytes,
        });
    }
    let capacity = usize::try_from(size)
        .map_err(|_| PakError::InvalidConfiguration("index does not fit address space".into()))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| PakError::BudgetExceeded {
            requested: size,
            limit: max_bytes,
        })?;
    bytes.extend_from_slice(&file_count.to_le_bytes());
    bytes.extend_from_slice(&blob_count.to_le_bytes());
    for (path, reference) in files {
        let path_len = u32::try_from(path.as_bytes().len()).unwrap();
        bytes.extend_from_slice(&path_len.to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        match reference {
            FileReference::Local(blob_id) => {
                bytes.push(FILE_LOCAL);
                bytes.extend_from_slice(&blob_id.get().to_le_bytes());
            }
            FileReference::External {
                archive_id,
                blob_id,
            } => {
                bytes.push(FILE_EXTERNAL);
                bytes.extend_from_slice(archive_id.as_bytes());
                bytes.extend_from_slice(&blob_id.get().to_le_bytes());
            }
        }
    }
    for blob in blobs.values() {
        bytes.extend_from_slice(&blob.id.get().to_le_bytes());
        bytes.push(blob.codec.wire_tag());
        bytes.extend_from_slice(&blob.total_decoded_size.to_le_bytes());
        bytes.extend_from_slice(&blob.logical_chunk_size.to_le_bytes());
        bytes.extend_from_slice(blob.content_hash.as_bytes());
        bytes.extend_from_slice(&(blob.chunks.len() as u32).to_le_bytes());
        for chunk in &blob.chunks {
            bytes.extend_from_slice(&chunk.offset.to_le_bytes());
            bytes.extend_from_slice(&chunk.stored_len.to_le_bytes());
            bytes.extend_from_slice(&chunk.decoded_len.to_le_bytes());
            bytes.extend_from_slice(&chunk.hash);
        }
    }
    debug_assert_eq!(bytes.len(), capacity);
    Ok(bytes)
}

pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| PakError::corrupt("integer offset overflow"))?;
    let value = bytes
        .get(offset..end)
        .ok_or_else(|| PakError::corrupt("truncated integer"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| PakError::corrupt("integer offset overflow"))?;
    let value = bytes
        .get(offset..end)
        .ok_or_else(|| PakError::corrupt("truncated integer"))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

struct IndexCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> IndexCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| PakError::corrupt("index cursor overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| PakError::corrupt("truncated index"))?;
        self.offset = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn array32(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
}
