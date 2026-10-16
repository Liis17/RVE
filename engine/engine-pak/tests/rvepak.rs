use engine_files::{ReadAt, VirtualPath};
use engine_pak::{
    Archive, ArchiveCatalog, ArchiveLimits, ArchiveWriter, BlobId, BlobOptions, CatalogLimits,
    Codec, ContentHash, MemoryFootprint, PakError, ReadControl, ReadReservation, WriterOptions,
};
use std::io::{self, Cursor, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

#[derive(Clone)]
struct MemorySource {
    bytes: Arc<Vec<u8>>,
    reads: Option<Arc<Mutex<Vec<Range<u64>>>>>,
}

struct FailingSink {
    inner: Cursor<Vec<u8>>,
    fail_after: u64,
}

impl FailingSink {
    fn new(fail_after: u64) -> Self {
        Self {
            inner: Cursor::new(Vec::new()),
            fail_after,
        }
    }
}

impl Write for FailingSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let position = self.inner.position();
        if position >= self.fail_after {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected output failure",
            ));
        }
        let allowed = buffer.len().min((self.fail_after - position) as usize);
        self.inner.write(&buffer[..allowed])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for FailingSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

impl MemorySource {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(bytes),
            reads: None,
        }
    }

    fn recording(bytes: Vec<u8>) -> (Self, Arc<Mutex<Vec<Range<u64>>>>) {
        let reads = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                bytes: Arc::new(bytes),
                reads: Some(reads.clone()),
            },
            reads,
        )
    }
}

impl ReadAt for MemorySource {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        let Some(start) = usize::try_from(offset).ok() else {
            return Ok(0);
        };
        let Some(source) = self.bytes.get(start..) else {
            return Ok(0);
        };
        let count = source.len().min(buffer.len());
        buffer[..count].copy_from_slice(&source[..count]);
        if let Some(reads) = &self.reads {
            reads.lock().unwrap().push(offset..offset + count as u64);
        }
        Ok(count)
    }
}

struct SparseSource {
    len: u64,
    regions: Vec<(u64, Vec<u8>)>,
}

impl ReadAt for SparseSource {
    fn len(&self) -> std::io::Result<u64> {
        Ok(self.len)
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        for (region_offset, region) in &self.regions {
            let region_end = *region_offset + region.len() as u64;
            if (*region_offset..region_end).contains(&offset) {
                let start = (offset - *region_offset) as usize;
                let count = buffer.len().min(region.len() - start);
                buffer[..count].copy_from_slice(&region[start..start + count]);
                return Ok(count);
            }
        }
        Ok(0)
    }
}

struct TestReservation;
impl ReadReservation for TestReservation {}

struct CaptureControl {
    footprints: Mutex<Vec<MemoryFootprint>>,
    checks: std::sync::atomic::AtomicUsize,
    cancel_at: Option<usize>,
}

impl CaptureControl {
    fn new(cancel_at: Option<usize>) -> Self {
        Self {
            footprints: Mutex::new(Vec::new()),
            checks: std::sync::atomic::AtomicUsize::new(0),
            cancel_at,
        }
    }
}

impl ReadControl for CaptureControl {
    fn reserve(
        &self,
        memory: MemoryFootprint,
    ) -> engine_pak::Result<Box<dyn ReadReservation + '_>> {
        self.footprints.lock().unwrap().push(memory);
        Ok(Box::new(TestReservation))
    }

    fn is_cancelled(&self) -> bool {
        let check = self
            .checks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.cancel_at == Some(check)
    }
}

fn path(value: &str) -> VirtualPath {
    VirtualPath::parse(value).unwrap()
}

fn options(codec: Codec, chunk_size: u64) -> BlobOptions {
    BlobOptions {
        codec,
        chunk_size,
        zstd_level: 3,
    }
}

fn finish(writer: ArchiveWriter<Cursor<Vec<u8>>>) -> (Vec<u8>, engine_pak::ArchiveId) {
    let finished = writer.finish().unwrap();
    let id = finished.archive_id;
    (finished.into_inner().into_inner(), id)
}

fn archive_bytes(codec: Codec, chunk_size: u64, path_text: &str, content: &[u8]) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec,
            chunk_size,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    writer
        .add_local(path(path_text), content, Some(hash(content)))
        .unwrap();
    finish(writer).0
}

fn zstd_frame(content: &[u8], checksum: bool) -> Vec<u8> {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.include_checksum(checksum).unwrap();
    encoder.write_all(content).unwrap();
    encoder.finish().unwrap()
}

fn archive_bytes_with_zstd_frame(content: &[u8], frame: &[u8]) -> Vec<u8> {
    let archive = archive_bytes(Codec::Zstd, content.len() as u64, "asset", content);
    let old_index_offset = index_offset(&archive);
    let mut index = archive[old_index_offset..old_index_offset + index_len(&archive)].to_vec();
    let blob_entry = 8 + 4 + "asset".len() + 1 + 8;
    let chunk_descriptor = blob_entry + 8 + 1 + 8 + 8 + 32 + 4;
    let data_start = engine_pak::HEADER_SIZE + index.len() as u64;
    put_u64(&mut index, chunk_descriptor, data_start);
    put_u64(&mut index, chunk_descriptor + 8, frame.len() as u64);
    build_v2_archive(index, frame)
}

fn hash(content: &[u8]) -> ContentHash {
    ContentHash::from_bytes(*blake3::hash(content).as_bytes())
}

struct HeaderLayout {
    index_offset_field: usize,
    index_len_field: usize,
    data_offset: HeaderValue,
    archive_id_offset: usize,
}

enum HeaderValue {
    Fixed(usize),
    Field(usize),
}

fn header_layout(bytes: &[u8]) -> HeaderLayout {
    match archive_version(bytes) {
        1 => HeaderLayout {
            index_offset_field: 16,
            index_len_field: 24,
            data_offset: HeaderValue::Fixed(64),
            archive_id_offset: 32,
        },
        2 => HeaderLayout {
            index_offset_field: 24,
            index_len_field: 32,
            data_offset: HeaderValue::Field(40),
            archive_id_offset: 56,
        },
        version => panic!("unexpected archive version {version}"),
    }
}

fn index_offset(bytes: &[u8]) -> usize {
    let field = header_layout(bytes).index_offset_field;
    u64::from_le_bytes(bytes[field..field + 8].try_into().unwrap()) as usize
}

fn index_len(bytes: &[u8]) -> usize {
    let field = header_layout(bytes).index_len_field;
    u64::from_le_bytes(bytes[field..field + 8].try_into().unwrap()) as usize
}

fn index_bytes(bytes: &[u8]) -> &[u8] {
    let start = index_offset(bytes);
    &bytes[start..start + index_len(bytes)]
}

fn archive_version(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[8..12].try_into().unwrap())
}

fn data_offset(bytes: &[u8]) -> usize {
    match header_layout(bytes).data_offset {
        HeaderValue::Fixed(value) => value,
        HeaderValue::Field(field) => {
            u64::from_le_bytes(bytes[field..field + 8].try_into().unwrap()) as usize
        }
    }
}

fn archive_id_offset(bytes: &[u8]) -> usize {
    header_layout(bytes).archive_id_offset
}

fn refresh_index_hash(bytes: &mut [u8]) {
    let start = index_offset(bytes);
    let end = start + index_len(bytes);
    let hash = *blake3::hash(&bytes[start..end]).as_bytes();
    let id_offset = archive_id_offset(bytes);
    bytes[id_offset..id_offset + 32].copy_from_slice(&hash);
}

fn build_v2_archive(index: Vec<u8>, payload: &[u8]) -> Vec<u8> {
    let data_offset = engine_pak::HEADER_SIZE + index.len() as u64;
    let archive_id = *blake3::hash(&index).as_bytes();
    let header = v2_header(index.len() as u64, payload.len() as u64, archive_id);
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(&index);
    assert_eq!(bytes.len() as u64, data_offset);
    bytes.extend_from_slice(payload);
    bytes
}

fn v2_header(index_len: u64, data_len: u64, archive_id: [u8; 32]) -> [u8; 96] {
    let index_offset = engine_pak::HEADER_SIZE;
    let data_offset = index_offset + index_len;
    let mut header = [0; 96];
    header[..8].copy_from_slice(b"RVEPAK\0\0");
    put_u32(&mut header, 8, 2);
    put_u32(&mut header, 16, 96);
    put_u32(&mut header, 20, 2);
    put_u64(&mut header, 24, index_offset);
    put_u64(&mut header, 32, index_len);
    put_u64(&mut header, 40, data_offset);
    put_u64(&mut header, 48, data_len);
    header[56..88].copy_from_slice(&archive_id);
    header
}

fn reindex(mut bytes: Vec<u8>, mutate: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let start = index_offset(&bytes);
    let len = index_len(&bytes);
    mutate(&mut bytes[start..start + len]);
    refresh_index_hash(&mut bytes);
    bytes
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn blob_entry_offset() -> usize {
    // One short path entry: path length, one UTF-8 byte, ref tag and local BlobId.
    8 + 14
}

#[test]
fn reads_opaque_bytes_unicode_paths_empty_assets_and_all_codecs() {
    let mut writer = ArchiveWriter::new(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            chunk_size: 4,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    let opaque = [0x00, 0xff, 0x80, b'\n', 0x7f, 0x00, b'/', 0xfe, b'X'];
    let first = writer
        .add_local_with_options(
            path("/models/素材.bin"),
            opaque.as_slice(),
            options(Codec::None, 4),
            Some(hash(&opaque)),
        )
        .unwrap();
    writer.add_alias(path("aliases/copy.data"), first).unwrap();
    let duplicate = writer
        .add_local_with_options(
            path("duplicates/opaque"),
            opaque.as_slice(),
            options(Codec::Lz4, 3),
            Some(hash(&opaque)),
        )
        .unwrap();
    assert_ne!(
        first, duplicate,
        "independent blobs must not deduplicate implicitly"
    );
    writer
        .add_local_with_options(
            path("textures/素材.bin"),
            b"different basename peer".as_slice(),
            options(Codec::Zstd, 5),
            None,
        )
        .unwrap();
    writer
        .add_local_with_options(
            path("empty.bin"),
            &[][..],
            options(Codec::None, 4),
            Some(hash(&[])),
        )
        .unwrap();

    let (bytes, _) = finish(writer);
    let archive = Archive::open(MemorySource::new(bytes)).unwrap();
    assert_eq!(archive.read_all(&path("models/素材.bin")).unwrap(), opaque);
    assert_eq!(
        archive.read_all(&path("aliases/copy.data")).unwrap(),
        opaque
    );
    assert_eq!(
        archive.read_all(&path("duplicates/opaque")).unwrap(),
        opaque
    );
    assert_eq!(
        archive.read_all(&path("textures/素材.bin")).unwrap(),
        b"different basename peer"
    );
    assert!(archive.read_all(&path("empty.bin")).unwrap().is_empty());
    assert_eq!(
        archive.stat(&path("aliases/copy.data")).unwrap().blob_id,
        first
    );
    assert_eq!(archive.verify().unwrap().local_blobs_verified, 4);
    assert_eq!(archive.list().count(), 5);
}

#[test]
fn range_reads_touch_only_intersecting_chunks_and_validate_bounds() {
    let data = b"abcdefghijkl";
    let bytes = archive_bytes(Codec::Zstd, 4, "asset.bin", data);
    let data_start = data_offset(&bytes) as u64;
    let data_end = bytes.len() as u64;
    let (source, reads) = MemorySource::recording(bytes);
    let archive = Archive::open(source).unwrap();
    reads.lock().unwrap().clear();

    assert_eq!(archive.read_range(&path("asset.bin"), 5..7).unwrap(), b"fg");
    let touched = reads.lock().unwrap().clone();
    assert_eq!(touched.len(), 1);
    assert!(touched[0].start >= data_start && touched[0].end <= data_end);

    assert_eq!(
        archive.read_range(&path("asset.bin"), 3..9).unwrap(),
        b"defghi"
    );
    assert_eq!(
        reads.lock().unwrap().len(),
        4,
        "the next range touches only its three chunks"
    );
    assert!(
        archive
            .read_range(&path("asset.bin"), 12..12)
            .unwrap()
            .is_empty()
    );
    assert!(
        archive
            .read_range(&path("asset.bin"), 5..5)
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        archive.read_range(&path("asset.bin"), 8..13),
        Err(PakError::InvalidRange { .. })
    ));
    assert!(matches!(
        archive.read_range(&path("asset.bin"), Range { start: 4, end: 3 }),
        Err(PakError::InvalidRange { .. })
    ));
    assert!(matches!(
        archive.read_range(&path("missing"), 0..0),
        Err(PakError::NotFound(_))
    ));
}

#[test]
fn writer_checks_expected_hash_and_poisoned_output_cannot_finish() {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    let wrong = hash(b"different");
    assert!(matches!(
        writer.add_local(path("asset"), b"actual".as_slice(), Some(wrong)),
        Err(PakError::SourceMismatch { .. })
    ));
    assert!(matches!(writer.finish(), Err(PakError::PoisonedWriter)));
}

#[test]
fn read_control_reserves_memory_and_cancels_between_chunks() {
    let (source, reads) =
        MemorySource::recording(archive_bytes(Codec::Lz4, 4, "asset", b"abcdefghijkl"));
    let archive = Archive::open(source).unwrap();
    reads.lock().unwrap().clear();
    let control = CaptureControl::new(Some(3));

    assert!(matches!(
        archive.read_range_with_control(&path("asset"), 0..12, &control),
        Err(PakError::Cancelled)
    ));
    let footprint = control.footprints.lock().unwrap()[0];
    assert_eq!(footprint.result_bytes, 12);
    assert_eq!(footprint.decoded_chunk_bytes, 4);
    assert!(footprint.stored_chunk_bytes > 0);
    assert_eq!(footprint.codec_workspace_bytes, 0);
    assert_eq!(
        reads.lock().unwrap().len(),
        1,
        "cancellation is checked between chunks"
    );
}

#[test]
fn read_budget_rejects_before_allocating_or_touching_payload() {
    let (source, reads) =
        MemorySource::recording(archive_bytes(Codec::None, 4, "asset", b"abcdefgh"));
    let limits = ArchiveLimits {
        max_read_memory_bytes: 1,
        ..ArchiveLimits::default()
    };
    let archive = Archive::open_with_limits(source, limits).unwrap();
    reads.lock().unwrap().clear();

    assert!(matches!(
        archive.read_range(&path("asset"), 0..1),
        Err(PakError::BudgetExceeded { .. })
    ));
    assert!(reads.lock().unwrap().is_empty());
}

#[test]
fn chunk_and_full_blob_checksums_detect_payload_corruption() {
    let mut broken_chunk = archive_bytes(Codec::None, 4, "asset", b"data");
    let payload_offset = data_offset(&broken_chunk);
    broken_chunk[payload_offset] ^= 1;
    let archive = Archive::open(MemorySource::new(broken_chunk)).unwrap();
    assert!(matches!(
        archive.read_all(&path("asset")),
        Err(PakError::Corrupt(_))
    ));

    let broken_blob_hash = reindex(archive_bytes(Codec::None, 4, "a", b"data"), |index| {
        index[blob_entry_offset() + 25] ^= 1;
    });
    let archive = Archive::open(MemorySource::new(broken_blob_hash)).unwrap();
    assert_eq!(archive.read_all(&path("a")).unwrap(), b"data");
    assert!(matches!(archive.verify(), Err(PakError::Corrupt(_))));
}

#[test]
fn zstd_content_checksum_frames_are_accepted_and_unused_bit_is_ignored() {
    let content = b"checksum protected payload";
    let mut frame = zstd_frame(content, true);
    assert_ne!(frame[4] & 0x04, 0, "frame has the content checksum flag");
    frame[4] |= 0x10;
    let archive = Archive::open(MemorySource::new(archive_bytes_with_zstd_frame(
        content, &frame,
    )))
    .unwrap();

    assert_eq!(archive.read_all(&path("asset")).unwrap(), content);
    assert_eq!(
        archive.read_range(&path("asset"), 3..10).unwrap(),
        &content[3..10]
    );
    assert_eq!(archive.verify().unwrap().local_blobs_verified, 1);
}

#[test]
fn zstd_reserved_frame_bit_is_rejected() {
    let content = b"reserved bit";
    let mut frame = zstd_frame(content, false);
    frame[4] |= 0x08;
    let archive = Archive::open(MemorySource::new(archive_bytes_with_zstd_frame(
        content, &frame,
    )))
    .unwrap();

    assert!(matches!(
        archive.read_all(&path("asset")),
        Err(PakError::Corrupt(message)) if message == "reserved Zstandard frame bit is set"
    ));
}

#[test]
fn zstd_content_checksum_corruption_is_rejected_by_the_decoder() {
    let content = b"checksum corruption";
    let mut frame = zstd_frame(content, true);
    *frame.last_mut().unwrap() ^= 1;
    let archive = Archive::open(MemorySource::new(archive_bytes_with_zstd_frame(
        content, &frame,
    )))
    .unwrap();

    assert!(matches!(
        archive.read_all(&path("asset")),
        Err(PakError::Corrupt(message)) if message == "invalid Zstandard chunk"
    ));
}

#[test]
fn external_references_resolve_only_through_a_validated_catalog() {
    let mut owner_writer = ArchiveWriter::new(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    let owner_blob = owner_writer
        .add_local(path("owner/source"), b"shared bytes".as_slice(), None)
        .unwrap();
    let (owner_bytes, owner_id) = finish(owner_writer);

    let mut dependent_writer =
        ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    dependent_writer
        .add_external(path("packs/alias"), owner_id, owner_blob)
        .unwrap();
    let (dependent_bytes, _) = finish(dependent_writer);

    let dependent = Archive::open(MemorySource::new(dependent_bytes.clone())).unwrap();
    assert!(matches!(
        dependent.read_all(&path("packs/alias")),
        Err(PakError::MissingOwner(id)) if id == owner_id
    ));
    let mut catalog = ArchiveCatalog::default();
    catalog.insert(dependent).unwrap();
    assert!(matches!(catalog.validate(), Err(PakError::MissingOwner(id)) if id == owner_id));
    catalog
        .insert(Archive::open(MemorySource::new(owner_bytes.clone())).unwrap())
        .unwrap();
    catalog.validate().unwrap();
    assert_eq!(
        catalog.read_all(&path("packs/alias")).unwrap(),
        b"shared bytes"
    );

    let mut missing_blob_writer =
        ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    missing_blob_writer
        .add_external(
            path("invalid/alias"),
            owner_id,
            engine_pak::BlobId::from_u64(99),
        )
        .unwrap();
    let (missing_blob, _) = finish(missing_blob_writer);
    let mut invalid_catalog = ArchiveCatalog::default();
    invalid_catalog
        .insert(Archive::open(MemorySource::new(owner_bytes)).unwrap())
        .unwrap();
    invalid_catalog
        .insert(Archive::open(MemorySource::new(missing_blob)).unwrap())
        .unwrap();
    assert!(matches!(
        invalid_catalog.validate(),
        Err(PakError::MissingBlob { .. })
    ));
}

#[test]
fn mixed_v1_v2_catalog_resolves_external_references_in_both_directions() {
    let mut v2_owner_writer = ArchiveWriter::new(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    let v2_blob = v2_owner_writer
        .add_local(path("v2-owner.bin"), b"owned by v2".as_slice(), None)
        .unwrap();
    let (v2_owner_bytes, v2_owner_id) = finish(v2_owner_writer);

    let (v1_bytes, v1_id) = v1_archive_with_external(v2_owner_id, v2_blob);
    let mut v2_dependent_writer =
        ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    v2_dependent_writer
        .add_external(path("v1-link.bin"), v1_id, BlobId::from_u64(0))
        .unwrap();
    let (v2_dependent_bytes, _) = finish(v2_dependent_writer);

    let mut catalog = ArchiveCatalog::default();
    catalog
        .insert(Archive::open(MemorySource::new(v1_bytes)).unwrap())
        .unwrap();
    catalog
        .insert(Archive::open(MemorySource::new(v2_owner_bytes)).unwrap())
        .unwrap();
    catalog
        .insert(Archive::open(MemorySource::new(v2_dependent_bytes)).unwrap())
        .unwrap();
    catalog.validate().unwrap();
    assert_eq!(
        catalog.read_all(&path("v2-link.bin")).unwrap(),
        b"owned by v2"
    );
    assert_eq!(
        catalog.read_all(&path("v1-link.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );
    assert_eq!(
        catalog.read_all(&path("v1-local.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );
}

fn v1_archive_with_external(
    owner_archive_id: engine_pak::ArchiveId,
    owner_blob_id: BlobId,
) -> (Vec<u8>, engine_pak::ArchiveId) {
    let fixture = from_hex(include_str!("fixtures/v1-none.hex").trim());
    let original_index_offset = index_offset(&fixture);
    let original_index = index_bytes(&fixture);
    let old_path_len = u32::from_le_bytes(original_index[8..12].try_into().unwrap()) as usize;
    let blob_table_start = 8 + 4 + old_path_len + 1 + 8;
    let mut index = Vec::new();
    index.extend_from_slice(&2_u32.to_le_bytes());
    index.extend_from_slice(&1_u32.to_le_bytes());
    append_v1_local_path(&mut index, "v1-local.bin", BlobId::from_u64(0));
    append_v1_external_path(&mut index, "v2-link.bin", owner_archive_id, owner_blob_id);
    index.extend_from_slice(&original_index[blob_table_start..]);

    let payload = &fixture[64..original_index_offset];
    let index_offset = 64 + payload.len();
    let archive_id = engine_pak::ArchiveId::from_bytes(*blake3::hash(&index).as_bytes());
    let mut header = [0; 64];
    header[..8].copy_from_slice(b"RVEPAK\0\0");
    put_u32(&mut header, 8, 1);
    put_u64(&mut header, 16, index_offset as u64);
    put_u64(&mut header, 24, index.len() as u64);
    header[32..64].copy_from_slice(archive_id.as_bytes());
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&index);
    (bytes, archive_id)
}

fn append_v1_local_path(index: &mut Vec<u8>, path_text: &str, blob_id: BlobId) {
    index.extend_from_slice(&(path_text.len() as u32).to_le_bytes());
    index.extend_from_slice(path_text.as_bytes());
    index.push(0);
    index.extend_from_slice(&blob_id.get().to_le_bytes());
}

fn append_v1_external_path(
    index: &mut Vec<u8>,
    path_text: &str,
    archive_id: engine_pak::ArchiveId,
    blob_id: BlobId,
) {
    index.extend_from_slice(&(path_text.len() as u32).to_le_bytes());
    index.extend_from_slice(path_text.as_bytes());
    index.push(1);
    index.extend_from_slice(archive_id.as_bytes());
    index.extend_from_slice(&blob_id.get().to_le_bytes());
}

#[test]
fn writer_creates_valid_empty_v2_archive() {
    let (bytes, _) =
        finish(ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap());
    assert_eq!(archive_version(&bytes), 2);
    assert_eq!(u64::from_le_bytes(bytes[48..56].try_into().unwrap()), 0);
    let archive = Archive::open(MemorySource::new(bytes)).unwrap();
    assert_eq!(archive.list().count(), 0);
    assert_eq!(archive.verify().unwrap().local_blobs_verified, 0);
}

#[test]
fn writer_spool_is_removed_after_success_error_and_drop() {
    let spool_dir = temporary_archive_path("spool-dir").with_extension("dir");
    std::fs::create_dir(&spool_dir).unwrap();
    let spool_count = || std::fs::read_dir(&spool_dir).unwrap().count();

    let mut writer = ArchiveWriter::with_spool_dir(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            ..WriterOptions::default()
        },
        &spool_dir,
    )
    .unwrap();
    writer
        .add_local(path("success.bin"), b"ok".as_slice(), None)
        .unwrap();
    assert_eq!(spool_count(), 1);
    let _ = writer.finish().unwrap();
    assert_eq!(spool_count(), 0);

    let mut writer = ArchiveWriter::with_spool_dir(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            ..WriterOptions::default()
        },
        &spool_dir,
    )
    .unwrap();
    assert!(matches!(
        writer.add_local(
            path("error.bin"),
            b"wrong".as_slice(),
            Some(hash(b"expected"))
        ),
        Err(PakError::SourceMismatch { .. })
    ));
    assert_eq!(spool_count(), 0);
    assert!(matches!(writer.finish(), Err(PakError::PoisonedWriter)));

    let mut writer = ArchiveWriter::with_spool_dir(
        FailingSink::new(engine_pak::HEADER_SIZE + 1),
        WriterOptions {
            codec: Codec::None,
            ..WriterOptions::default()
        },
        &spool_dir,
    )
    .unwrap();
    writer
        .add_local(path("sink-error.bin"), b"payload".as_slice(), None)
        .unwrap();
    assert_eq!(spool_count(), 1);
    assert!(matches!(
        writer.finish(),
        Err(PakError::Io { source, .. }) if source.kind() == io::ErrorKind::BrokenPipe
    ));
    assert_eq!(spool_count(), 0);

    let writer = ArchiveWriter::with_spool_dir(
        Cursor::new(Vec::new()),
        WriterOptions::default(),
        &spool_dir,
    )
    .unwrap();
    assert_eq!(spool_count(), 1);
    drop(writer);
    assert_eq!(spool_count(), 0);
    std::fs::remove_dir(spool_dir).unwrap();
}

#[cfg(unix)]
#[test]
fn writer_spool_uses_private_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let spool_dir = temporary_archive_path("spool-permissions").with_extension("dir");
    std::fs::create_dir(&spool_dir).unwrap();
    let writer = ArchiveWriter::with_spool_dir(
        Cursor::new(Vec::new()),
        WriterOptions::default(),
        &spool_dir,
    )
    .unwrap();
    let spool_path = std::fs::read_dir(&spool_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mode = std::fs::metadata(&spool_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    drop(writer);
    std::fs::remove_dir(spool_dir).unwrap();
}

#[test]
fn catalog_rejects_duplicate_archive_ids_and_namespace_paths() {
    let same = archive_bytes(Codec::None, 4, "same/path", b"same");
    let mut duplicate_id_catalog = ArchiveCatalog::default();
    duplicate_id_catalog
        .insert(Archive::open(MemorySource::new(same.clone())).unwrap())
        .unwrap();
    assert!(matches!(
        duplicate_id_catalog.insert(Archive::open(MemorySource::new(same)).unwrap()),
        Err(PakError::DuplicateArchiveId(_))
    ));

    let one = archive_bytes(Codec::None, 4, "namespace/duplicate", b"one");
    let two = archive_bytes(Codec::None, 4, "namespace/duplicate", b"two");
    let mut duplicate_path_catalog = ArchiveCatalog::default();
    duplicate_path_catalog
        .insert(Archive::open(MemorySource::new(one)).unwrap())
        .unwrap();
    assert!(matches!(
        duplicate_path_catalog.insert(Archive::open(MemorySource::new(two)).unwrap()),
        Err(PakError::DuplicatePath(_))
    ));
}

#[test]
fn archive_handles_are_owned_and_reopen_checks_replacement_identity() {
    let bytes = archive_bytes(Codec::None, 4, "asset", b"stable");
    let file_path = temporary_archive_path("lifetime");
    std::fs::write(&file_path, &bytes).unwrap();
    let archive = Archive::open_file(&file_path).unwrap();
    assert_eq!(archive.read_all(&path("asset")).unwrap(), b"stable");
    let replacement = archive_bytes(Codec::None, 4, "asset", b"replaced");
    let replacement_path = temporary_archive_path("replacement");
    std::fs::write(&replacement_path, replacement).unwrap();
    #[cfg(unix)]
    {
        std::fs::rename(&replacement_path, &file_path).unwrap();
        assert!(matches!(
            archive.reopen_for_worker(),
            Err(PakError::BackingChanged)
        ));
        assert_eq!(archive.read_all(&path("asset")).unwrap(), b"stable");
    }
    #[cfg(windows)]
    {
        std::fs::write(&file_path, std::fs::read(&replacement_path).unwrap()).unwrap();
        assert!(matches!(
            archive.reopen_for_worker(),
            Err(PakError::BackingChanged)
        ));
        assert!(matches!(
            archive.read_all(&path("asset")),
            Err(PakError::BackingChanged)
        ));
    }

    drop(archive);
    let _ = std::fs::remove_file(replacement_path);
    std::fs::remove_file(file_path).unwrap();
}

#[test]
fn v1_file_backed_reader_detects_replacement() {
    let file_path = temporary_archive_path("v1-replacement");
    let replacement_path = temporary_archive_path("v1-replacement-new");
    std::fs::write(
        &file_path,
        from_hex(include_str!("fixtures/v1-none.hex").trim()),
    )
    .unwrap();
    let archive = Archive::open_file(&file_path).unwrap();
    assert_eq!(
        archive.read_all(&path("golden.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );
    std::fs::write(
        &replacement_path,
        archive_bytes(Codec::None, 4, "golden.bin", b"replacement"),
    )
    .unwrap();
    #[cfg(unix)]
    std::fs::rename(&replacement_path, &file_path).unwrap();
    #[cfg(windows)]
    std::fs::write(&file_path, std::fs::read(&replacement_path).unwrap()).unwrap();

    assert!(matches!(
        archive.reopen_for_worker(),
        Err(PakError::BackingChanged)
    ));
    #[cfg(windows)]
    assert!(matches!(
        archive.read_all(&path("golden.bin")),
        Err(PakError::BackingChanged)
    ));
    #[cfg(unix)]
    assert_eq!(
        archive.read_all(&path("golden.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );

    drop(archive);
    let _ = std::fs::remove_file(replacement_path);
    std::fs::remove_file(file_path).unwrap();
}

#[test]
fn file_backed_reader_reads_the_v1_golden_fixture() {
    let file_path = temporary_archive_path("v1-file-backed");
    std::fs::write(
        &file_path,
        from_hex(include_str!("fixtures/v1-none.hex").trim()),
    )
    .unwrap();
    let archive = Archive::open_file(&file_path).unwrap();
    assert_eq!(
        archive.read_all(&path("golden.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );
    drop(archive);
    std::fs::remove_file(file_path).unwrap();
}

fn temporary_archive_path(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "rvepak-{label}-{}-{}.rvepak",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

#[test]
fn parser_rejects_bad_headers_counts_codecs_sizes_and_overlapping_extents() {
    let original = archive_bytes(Codec::None, 4, "a", b"abcdefgh");

    let mut bad_magic = original.clone();
    bad_magic[0] ^= 1;
    assert!(matches!(
        Archive::open(MemorySource::new(bad_magic)),
        Err(PakError::Corrupt(_))
    ));

    let mut bad_version = original.clone();
    put_u32(&mut bad_version, 8, 99);
    assert!(matches!(
        Archive::open(MemorySource::new(bad_version)),
        Err(PakError::Unsupported(_))
    ));

    let mut bad_flags = original.clone();
    put_u32(&mut bad_flags, 12, 1);
    assert!(matches!(
        Archive::open(MemorySource::new(bad_flags)),
        Err(PakError::Unsupported(_))
    ));

    let mut bad_index_hash = original.clone();
    let start = index_offset(&bad_index_hash);
    bad_index_hash[start] ^= 1;
    assert!(matches!(
        Archive::open(MemorySource::new(bad_index_hash)),
        Err(PakError::Corrupt(_))
    ));

    let mut index_overflow = original.clone();
    let layout = header_layout(&index_overflow);
    put_u64(
        &mut index_overflow,
        layout.index_offset_field,
        engine_pak::HEADER_SIZE,
    );
    put_u64(&mut index_overflow, layout.index_len_field, u64::MAX);
    assert!(matches!(
        Archive::open(MemorySource::new(index_overflow)),
        Err(PakError::Corrupt(_))
    ));

    let excessive_count = reindex(original.clone(), |index| put_u32(index, 0, u32::MAX));
    assert!(matches!(
        Archive::open(MemorySource::new(excessive_count)),
        Err(PakError::Corrupt(_))
    ));

    let unknown_codec = reindex(original.clone(), |index| {
        index[blob_entry_offset() + 8] = 77;
    });
    assert!(matches!(
        Archive::open(MemorySource::new(unknown_codec)),
        Err(PakError::Unsupported(_))
    ));

    let invalid_chunk_size = reindex(original.clone(), |index| {
        put_u64(index, blob_entry_offset() + 17, 1_u64 << 40);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(invalid_chunk_size)),
        Err(PakError::Corrupt(_))
    ));

    let invalid_decoded_total = reindex(original.clone(), |index| {
        put_u64(index, blob_entry_offset() + 9, 7);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(invalid_decoded_total)),
        Err(PakError::Corrupt(_))
    ));

    let excessive_chunk_count = reindex(original.clone(), |index| {
        put_u32(index, blob_entry_offset() + 57, u32::MAX);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(excessive_chunk_count)),
        Err(PakError::Corrupt(_))
    ));

    let none_size_mismatch = reindex(original.clone(), |index| {
        put_u64(index, blob_entry_offset() + 61 + 8, 5);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(none_size_mismatch)),
        Err(PakError::Corrupt(_))
    ));

    let decoded_chunk_mismatch = reindex(original.clone(), |index| {
        put_u64(index, blob_entry_offset() + 61 + 16, 3);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(decoded_chunk_mismatch)),
        Err(PakError::Corrupt(_))
    ));

    let noncanonical_path = reindex(original.clone(), |index| {
        index[12] = b'.';
    });
    assert!(matches!(
        Archive::open(MemorySource::new(noncanonical_path)),
        Err(PakError::Corrupt(_))
    ));

    let missing_local_blob = reindex(original.clone(), |index| {
        put_u64(index, 14, 77);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(missing_local_blob)),
        Err(PakError::Corrupt(_))
    ));

    let data_overlaps_index = reindex(original.clone(), |index| {
        let data_offset = data_offset(&original);
        put_u64(index, blob_entry_offset() + 61, (data_offset - 1) as u64);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(data_overlaps_index)),
        Err(PakError::Corrupt(_))
    ));

    let duplicate_file_paths = {
        let mut writer = ArchiveWriter::new(
            Cursor::new(Vec::new()),
            WriterOptions {
                codec: Codec::None,
                ..WriterOptions::default()
            },
        )
        .unwrap();
        writer
            .add_local(path("a"), b"one".as_slice(), None)
            .unwrap();
        writer
            .add_local(path("b"), b"two".as_slice(), None)
            .unwrap();
        reindex(finish(writer).0, |index| index[8 + 14 + 4] = b'a')
    };
    assert!(matches!(
        Archive::open(MemorySource::new(duplicate_file_paths)),
        Err(PakError::Corrupt(_))
    ));

    let extent_overflow = reindex(original, |index| {
        let first_chunk = blob_entry_offset() + 61;
        put_u64(index, first_chunk, u64::MAX);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(extent_overflow)),
        Err(PakError::Corrupt(_))
    ));

    let two_chunks = archive_bytes(Codec::None, 4, "a", b"abcdefgh");
    let overlapping = reindex(two_chunks, |index| {
        let first_chunk = blob_entry_offset() + 61;
        let first_offset =
            u64::from_le_bytes(index[first_chunk..first_chunk + 8].try_into().unwrap());
        put_u64(index, first_chunk + 56, first_offset);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(overlapping)),
        Err(PakError::Corrupt(_))
    ));
}

#[test]
fn v2_header_rejects_each_invalid_section_boundary_and_reserved_field() {
    let original = archive_bytes(Codec::None, 4, "a", b"data");

    let mut bad_header_length = original.clone();
    put_u32(&mut bad_header_length, 16, 95);
    assert!(matches!(
        Archive::open(MemorySource::new(bad_header_length)),
        Err(PakError::Corrupt(_))
    ));

    let mut bad_section_count = original.clone();
    put_u32(&mut bad_section_count, 20, 1);
    assert!(matches!(
        Archive::open(MemorySource::new(bad_section_count)),
        Err(PakError::Corrupt(_))
    ));

    let mut bad_index_offset = original.clone();
    put_u64(&mut bad_index_offset, 24, 97);
    assert!(matches!(
        Archive::open(MemorySource::new(bad_index_offset)),
        Err(PakError::Corrupt(_))
    ));

    let mut data_gap = original.clone();
    let data_start = data_offset(&data_gap) as u64;
    put_u64(&mut data_gap, 40, data_start + 1);
    assert!(matches!(
        Archive::open(MemorySource::new(data_gap)),
        Err(PakError::Corrupt(_))
    ));

    let mut data_overflow = original.clone();
    put_u64(&mut data_overflow, 48, u64::MAX);
    assert!(matches!(
        Archive::open(MemorySource::new(data_overflow)),
        Err(PakError::Corrupt(_))
    ));

    let mut data_not_at_end = original.clone();
    let data_len = u64::from_le_bytes(data_not_at_end[48..56].try_into().unwrap());
    put_u64(&mut data_not_at_end, 48, data_len - 1);
    assert!(matches!(
        Archive::open(MemorySource::new(data_not_at_end)),
        Err(PakError::Corrupt(_))
    ));

    let mut nonzero_reserved = original.clone();
    nonzero_reserved[88] = 1;
    assert!(matches!(
        Archive::open(MemorySource::new(nonzero_reserved)),
        Err(PakError::Corrupt(_))
    ));

    let mut unknown_version = original;
    put_u32(&mut unknown_version, 8, 77);
    assert!(matches!(
        Archive::open(MemorySource::new(unknown_version)),
        Err(PakError::Unsupported(_))
    ));
}

#[test]
fn v2_chunk_extent_cannot_run_past_file_end() {
    let original = archive_bytes(Codec::None, 4, "a", b"data");
    let file_end = original.len() as u64;
    let out_of_file = reindex(original, |index| {
        put_u64(index, blob_entry_offset() + 61, file_end - 1);
    });
    assert!(matches!(
        Archive::open(MemorySource::new(out_of_file)),
        Err(PakError::Corrupt(_))
    ));
}

#[test]
fn zstd_window_limit_rejects_oversized_window_before_decoder_work() {
    let base = archive_bytes(Codec::None, 1, "a", b"x");
    let start = index_offset(&base);
    let mut index = base[start..start + index_len(&base)].to_vec();
    let blob = blob_entry_offset();
    let chunk = blob + 61;
    index[blob + 8] = 1; // Zstandard
    put_u64(&mut index, chunk + 8, 6);
    let frame = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xf8]; // 2 TiB advertised window; truncated frame body.
    let data_start = engine_pak::HEADER_SIZE + index.len() as u64;
    put_u64(&mut index, chunk, data_start);
    let bytes = build_v2_archive(index, &frame);

    let archive = Archive::open(MemorySource::new(bytes.clone())).unwrap();
    assert!(matches!(
        archive.read_all(&path("a")),
        Err(PakError::BudgetExceeded { requested, limit }) if requested == (1_u64 << 41) && limit == 8 * 1024 * 1024
    ));

    let mut reserved_bit = bytes.clone();
    let reserved_byte = data_offset(&reserved_bit) + 4;
    reserved_bit[reserved_byte] |= 0x08;
    let archive = Archive::open(MemorySource::new(reserved_bit)).unwrap();
    assert!(matches!(
        archive.read_all(&path("a")),
        Err(PakError::Corrupt(_))
    ));
}

#[test]
fn zstd_rejects_concatenated_frames_in_one_chunk() {
    let base = archive_bytes(Codec::Zstd, 4, "a", b"x");
    let old_index_offset = index_offset(&base);
    let mut index = base[old_index_offset..old_index_offset + index_len(&base)].to_vec();
    let chunk = blob_entry_offset() + 61;
    let stored_len = u64::from_le_bytes(index[chunk + 8..chunk + 16].try_into().unwrap()) as usize;
    let payload_start = data_offset(&base);
    let mut payload = base[payload_start..payload_start + stored_len].to_vec();
    payload.extend_from_slice(&zstd::bulk::compress(b"y", 3).unwrap());
    put_u64(&mut index, chunk + 8, payload.len() as u64);
    let data_start = engine_pak::HEADER_SIZE + index.len() as u64;
    put_u64(&mut index, chunk, data_start);
    let bytes = build_v2_archive(index, &payload);

    let archive = Archive::open(MemorySource::new(bytes)).unwrap();
    assert!(matches!(
        archive.read_all(&path("a")),
        Err(PakError::Corrupt(_))
    ));
}

#[test]
fn a_sparse_archive_can_read_a_chunk_above_four_gibibytes() {
    let compact = archive_bytes(Codec::None, 8, "a", b"abc");
    let old_index_offset = index_offset(&compact);
    let index = &compact[old_index_offset..old_index_offset + index_len(&compact)];
    let mut shifted_index = index.to_vec();
    let blob = blob_entry_offset();
    let chunk = blob + 61;
    let data_offset = (1_u64 << 32) + 19;
    put_u64(&mut shifted_index, chunk, data_offset);
    let archive_data_offset = engine_pak::HEADER_SIZE + shifted_index.len() as u64;
    let file_len = data_offset + 3;
    let header = v2_header(
        shifted_index.len() as u64,
        file_len - archive_data_offset,
        *blake3::hash(&shifted_index).as_bytes(),
    );
    let source = SparseSource {
        len: file_len,
        regions: vec![
            (0, header.to_vec()),
            (engine_pak::HEADER_SIZE, shifted_index.clone()),
            (data_offset, b"abc".to_vec()),
        ],
    };
    let archive = Archive::open(source).unwrap();
    assert_eq!(archive.read_all(&path("a")).unwrap(), b"abc");
}

#[test]
fn archive_reader_releases_injected_backing_source_on_drop() {
    let bytes = archive_bytes(Codec::None, 4, "a", b"payload");
    let source: Arc<dyn ReadAt> = Arc::new(MemorySource::new(bytes));
    let weak: Weak<dyn ReadAt> = Arc::downgrade(&source);
    let archive = Archive::open_shared(source.clone()).unwrap();
    drop(source);
    assert!(weak.upgrade().is_some());
    drop(archive);
    assert!(weak.upgrade().is_none());
}

#[test]
fn default_reader_accepts_sixteen_mib_zstd_chunks_at_a_high_level() {
    let content = vec![b'A'; 16 * 1024 * 1024];
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new()), WriterOptions::default()).unwrap();
    writer
        .add_local_with_options(
            path("large.bin"),
            content.as_slice(),
            BlobOptions {
                codec: Codec::Zstd,
                chunk_size: 16 * 1024 * 1024,
                zstd_level: 19,
            },
            Some(hash(&content)),
        )
        .unwrap();
    let archive = Archive::open(MemorySource::new(finish(writer).0)).unwrap();
    assert_eq!(archive.read_all(&path("large.bin")).unwrap(), content);
}

#[test]
fn catalog_total_index_budget_is_applied_before_each_archive_is_opened() {
    let root = temporary_archive_path("budget-dir").with_extension("dir");
    std::fs::create_dir(&root).unwrap();
    let path_a = root.join("a.rvepak");
    let path_b = root.join("b.RVEPAK");
    let first = archive_bytes(Codec::None, 4, "a", b"first");
    let second = archive_bytes(Codec::None, 4, "b", b"second");
    let first_index_len = index_len(&first) as u64;
    std::fs::write(&path_a, first).unwrap();
    std::fs::write(&path_b, second).unwrap();
    let catalog = ArchiveCatalog::open_directory(
        &root,
        ArchiveLimits::default(),
        CatalogLimits {
            max_total_index_bytes: first_index_len,
            max_archives: 2,
        },
    );
    assert!(matches!(catalog, Err(PakError::BudgetExceeded { .. })));
    let _ = std::fs::remove_file(path_a);
    let _ = std::fs::remove_file(path_b);
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn reader_opens_the_immutable_v1_golden_fixture() {
    let bytes = from_hex(include_str!("fixtures/v1-none.hex").trim());
    assert_eq!(archive_version(&bytes), 1);
    let archive = Archive::open(MemorySource::new(bytes)).unwrap();
    assert_eq!(
        archive.read_all(&path("golden.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );
}

#[test]
fn reader_rejects_truncated_golden_archives() {
    let v1 = from_hex(include_str!("fixtures/v1-none.hex").trim());
    for length in [0, 11, 12, 63] {
        assert!(
            matches!(
                Archive::open(MemorySource::new(v1[..length].to_vec())),
                Err(PakError::Corrupt(_))
            ),
            "truncated v1 archive of {length} bytes should be corrupt"
        );
    }

    let v2 = from_hex(include_str!("fixtures/v2-none.hex").trim());
    for length in [0, 11, 12, 63, 64, 95] {
        assert!(
            matches!(
                Archive::open(MemorySource::new(v2[..length].to_vec())),
                Err(PakError::Corrupt(_))
            ),
            "truncated v2 archive of {length} bytes should be corrupt"
        );
    }
}

#[test]
fn v1_header_rejects_overflowing_or_nonterminal_index_extent() {
    let original = from_hex(include_str!("fixtures/v1-none.hex").trim());

    let mut index_overflow = original.clone();
    put_u64(&mut index_overflow, 16, u64::MAX);
    put_u64(&mut index_overflow, 24, 1);
    assert!(matches!(
        Archive::open(MemorySource::new(index_overflow)),
        Err(PakError::Corrupt(_))
    ));

    let mut index_does_not_end_at_file_end = original.clone();
    put_u64(
        &mut index_does_not_end_at_file_end,
        24,
        index_len(&original) as u64 + 1,
    );
    assert!(matches!(
        Archive::open(MemorySource::new(index_does_not_end_at_file_end)),
        Err(PakError::Corrupt(_))
    ));
}

#[test]
fn none_codec_writer_matches_v2_golden_fixture_and_round_trips() {
    let mut writer = ArchiveWriter::new(
        Cursor::new(Vec::new()),
        WriterOptions {
            codec: Codec::None,
            chunk_size: 256 * 1024,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    writer
        .add_local(path("golden.bin"), [0x00, 0xff, 0x41].as_slice(), None)
        .unwrap();
    let (bytes, _) = finish(writer);
    assert_eq!(archive_version(&bytes), 2);
    assert_eq!(hex(&bytes), include_str!("fixtures/v2-none.hex").trim());
    let archive = Archive::open(MemorySource::new(bytes)).unwrap();
    assert_eq!(
        archive.read_all(&path("golden.bin")).unwrap(),
        [0x00, 0xff, 0x41]
    );
}

fn from_hex(input: &str) -> Vec<u8> {
    let (pairs, remainder) = input.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    pairs
        .iter()
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => panic!("invalid hex digit"),
            };
            digit(pair[0]) * 16 + digit(pair[1])
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
