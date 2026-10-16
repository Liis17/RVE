# RVEPAK archive format

This document defines the public wire contract for `.rvepak` files. All integers are unsigned little-endian unless a field is explicitly signed. The format is serialized field by field; Rust structure layout is never written to disk. Version 2 is the current writer format. Readers also accept the legacy version 1 layout.

## Version 2 layout

A v2 archive consists of a fixed 96-byte header, one canonical index, and a data section through the end of the file. The index begins immediately after the header; data begins immediately after the index. Chunk offsets are absolute file offsets into the data section. Chunk extents may have gaps, but may not overlap. Writers emit chunks contiguously in local blob and chunk order. An archive with no local chunks has a zero-length data section.

### Header

The fixed v2 header has this layout:

| Offset | Size | Field | v2 value or meaning |
|---:|---:|---|---|
| 0 | 8 | Magic | `RVEPAK\0\0` (`52 56 45 50 41 4b 00 00`) |
| 8 | 4 | Version | `2` |
| 12 | 4 | Flags | `0`; other values are unsupported |
| 16 | 4 | Header length | `96` |
| 20 | 4 | Section count | `2` |
| 24 | 8 | Index offset | `96` |
| 32 | 8 | Index length | Index byte length |
| 40 | 8 | Data offset | `index_offset + index_length` |
| 48 | 8 | Data length | File length minus `data_offset` |
| 56 | 32 | ArchiveId | BLAKE3 of the exact index bytes |
| 88 | 8 | Reserved | All zero |

`index_offset + index_length` and `data_offset + data_length` must not overflow. The first sum must equal `data_offset`; the second must equal the file length. Every chunk extent must lie wholly inside `[data_offset, data_offset + data_length)`, and chunk extents must not overlap.

## Legacy version 1 layout

A v1 archive contains a 64-byte header, zero or more stored chunk payloads, and one uncompressed canonical index. The index ends at the physical end of the file; payload extents lie after the header and before the index.

| Offset | Size | Field | v1 value or meaning |
|---:|---:|---|---|
| 0 | 8 | Magic | `RVEPAK\0\0` (`52 56 45 50 41 4b 00 00`) |
| 8 | 4 | Version | `1` |
| 12 | 4 | Flags | `0`; other values are unsupported |
| 16 | 8 | Index offset | Absolute file offset of the index |
| 24 | 8 | Index length | Index byte length |
| 32 | 32 | ArchiveId | BLAKE3 of the exact index bytes |

For v1, `index_offset + index_length` must not overflow and must equal the file length. The index begins at or after byte 64. Chunk extents lie wholly inside `[64, index_offset)` and must not overlap.

## Canonical index

The index begins with `file_count: u32`, then `local_blob_count: u32`, followed by `file_count` file entries and `local_blob_count` local blob entries. Parsers bound the counts by configured limits and the remaining index length before allocating entry storage.

### File table

File entries are sorted by their complete canonical UTF-8 path bytes in ascending order. A path entry consists of:

| Size | Field |
|---:|---|
| 4 | `path_length: u32`, in bytes |
| variable | UTF-8 virtual path bytes, without a NUL terminator |
| 1 | Reference tag |
| variable | Reference payload |

Reference tags are:

| Tag | Payload |
|---:|---|
| 0 | `blob_id: u64` for a blob local to this archive |
| 1 | `archive_id: [u8; 32]`, then `blob_id: u64` for an external reference |

Virtual paths are relative to the asset namespace root. Input parsing accepts one optional leading `/` and stores the path without it. `/` is the only separator. Empty paths or components, `.`, `..`, backslash, colon, and NUL are invalid. Paths are case-sensitive and are not Unicode-normalized. Extensions are opaque and have no special meaning. Thus `models/wood.bin` and `textures/wood.bin` are distinct files.

### Local blob table

Local blob entries are sorted by ascending `blob_id`. A local BlobId is an archive-local `u64` ordinal assigned when the writer adds the blob, starting at zero. It is not a content hash: adding the same bytes twice creates two independent blobs. Multiple file paths can explicitly refer to one local BlobId as aliases. A blob table entry is:

| Size | Field |
|---:|---|
| 8 | `blob_id: u64` |
| 1 | Codec tag |
| 8 | `total_decoded_size: u64` |
| 8 | `logical_chunk_size: u64` |
| 32 | `content_hash: [u8; 32]`, BLAKE3 of the complete decoded blob |
| 4 | `chunk_count: u32` |
| variable | `chunk_count` chunk descriptors |

Codec tags are `0 = None`, `1 = Zstandard`, and `2 = LZ4 block`. A codec applies to every chunk in one blob. Different blobs in one archive may use different codecs and logical chunk sizes. A writer may choose a different Zstandard compression level per blob; that level is not stored in the index.

Each chunk descriptor is 56 bytes:

| Size | Field |
|---:|---|
| 8 | `data_offset: u64`, absolute file offset of stored bytes |
| 8 | `stored_length: u64` |
| 8 | `decoded_length: u64` |
| 32 | `decoded_hash: [u8; 32]`, BLAKE3 of this decoded chunk |

Chunks are decoded in descriptor order. All non-final chunks have `decoded_length == logical_chunk_size`; the final chunk is the remaining bytes. Stored and decoded lengths are nonzero. For `None`, the two lengths must match. An empty blob has zero chunks and the BLAKE3 hash of the empty byte string. Chunk offsets and lengths use checked arithmetic and must stay inside the version-specific data region without overlapping another chunk extent.

Every file entry must resolve to a local blob in its archive or an external reference to a local blob in the named owner archive. External ownership, duplicate ArchiveIds, and duplicate complete paths are validated over the complete catalog. Opening one archive can validate its own metadata while deferring external owner resolution until a catalog is available.

## Payload encoding and integrity

Every chunk is encoded independently. `None` stores the decoded bytes directly. Zstandard uses one standard frame per chunk; LZ4 uses one `lz4_flex` block per chunk. Readers reject malformed chunks, decoded-size mismatches, checksum mismatches, and trailing or extra Zstandard frames. Readers bound Zstandard frame windows before creating a decoder context. The default writer caps its Zstandard window at 8 MiB so its output is accepted by the default reader, including 16 MiB logical chunks.

`ArchiveId` protects the index; each chunk hash protects decoded chunk content; `content_hash` protects the complete decoded blob. These are separate hashes with distinct scope.

## Reader and writer contract

The `engine-pak` crate exposes `Archive` for immutable metadata, `stat`, sorted `list`, `read_range`, `read_all`, and `verify`; `ArchiveCatalog` resolves cross-archive paths and verifies unique local blobs. Range APIs use half-open `[start, end)` offsets, allow empty ranges within the decoded size, and reject out-of-range values without clamping. A range read loads or decodes only intersecting chunks.

`ArchiveWriter<W: Write + Seek>` writes v2. It streams payload chunks to a temporary spool file and uses a bounded buffer to copy that file after the header and index at `finish`. `new` uses the system temporary directory; `with_spool_dir` lets the manifest builder place the spool in its staging directory. The spool is removed after success, failure, or dropping an unfinished writer. The writer can check an expected whole-content BLAKE3 hash at EOF and offers explicit local aliases and external references. `finish` returns the ArchiveId and local BlobId-to-content-hash mapping for a later archive build. The default chunk size is 256 KiB, default codec is Zstandard at level 3, and the maximum logical chunk size is 16 MiB.

`ReadControl::reserve` receives a `MemoryFootprint` before read-result and chunk buffers are allocated. The footprint covers the result vector, the largest stored chunk (or raw-copy scratch buffer), the largest decoded chunk, and the codec workspace reservation. A reservation lives until the read completes or is dropped; cancellation is checked before the reservation and between touched chunks. Default limits are 256 MiB per serialized, uncompressed index, 256 MiB per returned result, and 512 MiB for a read's aggregate working memory. The default catalog aggregate index limit is also 256 MiB. These index limits count bytes in the archive format; they do not bound resident or peak memory for parsing and catalog metadata. Index buffers and parsed metadata are outside the read working-memory budget. Catalog directory opening applies its remaining aggregate index budget to each archive before reading that archive's index. The default Zstandard workspace reservation is its 8 MiB window bound plus 512 KiB overhead.

`engine-files::ReadAt` supplies checked `u64` positional reads and is injectable for recording and sparse test readers. A caller-provided source must keep its bytes stable for the lifetime of an opened `Archive`. Unix `FileSource` uses positional `read_at`. Windows `seek_read` moves the file cursor, so one source serializes its own cursor access; file-backed archive payload reads and worker reopenings use independent file handles and verify ArchiveId before reading payload bytes. No archive-wide mutex serializes worker reads.

The `engine-pak` library includes a reusable TOML manifest builder, and the `rvepak` tool builds, lists, and verifies top-level archive sets. The `engine-resources` crate provides background reads over loose files and validated archive catalogs. `engine-assets` reads FBX and PNG bytes through this service, and sandbox starts it before preparing content; see the component contracts in `Obsidian/RVEVault/Engine/EngineResources.md`, `EngineAssets.md`, and `Sandbox.md`.

## Archive set registry

`/rve/assets.toml` is a set-level convention used to select and validate a collection of `.rvepak` files. It is not part of the RVEPAK wire format: each archive remains an independent v1 or v2 file.

The registry uses TOML format version 1:

```toml
format = 1
archives = ["base", "shared", "scene-main", "weapons"]

[assets]
"/rve/assets.toml" = "base"
"/rve/project.toml" = "base"
"/scenes/main.rvescene" = "base"
"/models/cube.fbx" = "shared"
```

Archive names are 1–64 ASCII letters, digits, `_` or `-`, unique without ASCII case sensitivity, and may not be reserved Windows device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, or `LPT1`–`LPT9`, case-insensitively). The required `base` archive is first and is stored as `base.rvepak`; every other file is named `<name>.rvepak`, and every non-base archive declares at least one path. The `[assets]` entry for `/rve/assets.toml` must assign that path to `base`; the file itself must be stored locally in `base.rvepak`. No other `/rve/` path is required by the registry format. Unknown fields and versions other than 1 are rejected. The complete set of top-level `.rvepak` files must match the registry exactly.

Each key under `[assets]` is parsed by `engine-files::VirtualPath`. The value names the archive whose file table declares that path. It does not name the archive that physically owns the bytes: a deduplicated path in `shared.rvepak` may externally reference a blob in `base.rvepak` while the registry still assigns the path to `shared`. Validation compares the complete registry and catalog in both directions and uses this declaring-archive identity.

`base.rvepak` stores `/rve/assets.toml` locally and can be opened alone to read its own declared paths. Build archive order with base first so deduplication can create references from later archives to base without making base depend on another archive. `engine-pak::ArchiveSetRegistry` parses, serializes, and validates this convention; `engine-resources::SourceMode::Registry` loads it at startup using the fixed `base.rvepak` filename.
