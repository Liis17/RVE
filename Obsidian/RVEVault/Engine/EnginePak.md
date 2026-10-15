# EnginePak

Parent: [[Index]]

## Назначение

`engine-pak` читает, записывает, строит и проверяет неизменяемые RVEPAK v1 archives. `Archive` владеет проверенными metadata и backing source, `ArchiveWriter` добавляет локальные blobs потоково, `ArchiveCatalog` объединяет namespace и разрешает cross-archive references, а builder превращает строгий TOML manifest в полный набор архивов. CLI `tools/rvepak` использует этот библиотечный API для build/list/verify. Фоновое чтение и выбор packed-only режима предоставляет [[Engine/EngineResources]], который используется [[Engine/EngineAssets]] и sandbox.

Полная wire-раскладка — заголовок, индекс, ссылки и chunk descriptors — нормативно описана в [`docs/rvepak-format.md`](../../../docs/rvepak-format.md). Архив хранит opaque bytes без ограничений расширений. Chunk codec независим для каждой локальной blob; доступны None, Zstandard и LZ4 block.

## Источники

| Файл | Значимые символы | Роль |
|---|---|---|
| `engine/engine-pak/src/format.rs` | `ArchiveId`, `BlobId`, `ContentHash`, codec/reference types | Ручная LE serialization, формат и проверки index |
| `engine/engine-pak/src/reader.rs` | `Archive`, `ArchiveLimits`, `MemoryFootprint`, `ReadControl` | Metadata, stat/list/ranges, integrity и memory reservation |
| `engine/engine-pak/src/writer.rs` | `ArchiveWriter`, `BlobOptions`, `FinishedArchive` | Streaming writer, aliases и expected-hash check |
| `engine/engine-pak/src/catalog.rs` | `ArchiveCatalog`, `CatalogLimits`, `discover_archives` | Namespace uniqueness, owners, bounded archive discovery |
| `engine/engine-pak/src/builder.rs` | `BuildManifest`, `BuildPlan`, `BuildReport`, `preflight`, `execute` | Строгий manifest, раскрытие inputs, deduplication и безопасная публикация |
| `tools/rvepak/src/main.rs` | `build`, `list`, `verify` | CLI поверх builder и общего `ArchiveCatalog` |
| `engine/engine-pak/src/error.rs` | `PakError` | Typed path/not-found/I/O/corrupt/range/unsupported/budget/cancel categories |
| `engine/engine-pak/tests/rvepak.rs` | integration tests, sparse and recording readers | Golden v1 bytes, range semantics, corruption, memory and owner behavior |

## Публичный контракт

- `Archive` открывает один `.rvepak`, валидирует header/index до чтения assets и предоставляет `stat`, отсортированный `list`, `read_range`, `read_all` и `verify`. Диапазоны half-open; пустой диапазон допускается внутри размера, выход за границы не clamp-ится. Декодируются только пересекающиеся chunks.
- Один archive может открыться для проверки metadata даже с external references. Доступ к внешнему ресурсу требует `ArchiveCatalog`, который проверяет отсутствие дублирующих ArchiveIds и полных paths и требует, чтобы owner действительно содержал local BlobId.
- `ArchiveWriter<W: Write + Seek>` хранит bounded текущий chunk, умеет проверять optional ожидаемый BLAKE3 всего источника и не deduplicates автоматически. `add_alias` разделяет local blob между путями; `add_external` ссылается на BlobId предыдущего owner. `finish` возвращает ArchiveId и mapping local BlobId → decoded content hash.
- `BlobId` — archive-local ordinal, а не content hash, чтобы будущий builder мог сохранять повторные файлы как отдельные blobs. Content hash хранится отдельно; aliases задаются явно.
- `ReadControl::reserve` получает footprint результата, самого большого stored/decoded chunk и codec workspace до крупных allocation. Reservation живёт до завершения чтения; cancellation проверяется перед reservation и между chunks. Default limits: 256 MiB index per archive, 256 MiB result, 512 MiB aggregate read footprint; default window Zstandard — 8 MiB.
- Каталог открывает только верхний уровень каталога, сортирует `.rvepak` по имени и применяет общий лимит суммы индексов до allocation индекса следующего архива. Повреждённый архив прекращает открытие каталога; loose-file fallback отсутствует.
- Builder проверяет все имена, selectors и уникальность полного virtual path до hash prepass. Deduplication выбирает первый input в manifest порядке, использует локальные aliases внутри архива и прямые внешние ссылки на local blob ранее созданного архива. Пропускаемые повторы повторно читаются и сверяются с prepass перед ссылкой.
- `execute` пишет набор в собственный временный соседний каталог, открывает его через тот же `ArchiveCatalog`, а затем атомарно публикует с запретом замены существующей папки. Полную проверку содержимого выполняет CLI `verify`; сборка не делает второй полный проход по сжатым payloads.
- TOML manifest задаёт `root` и `output` относительно manifest-файла, упорядоченные `archives` и файловые или каталожные `inputs` относительно root. Допустимы любые opaque asset bytes; неизвестные поля, symlinks, пустые группы и небезопасные archive basenames запрещены.

## Зависимости и интеграция

Крейт зависит от `engine-files`, `blake3`, `zstd`, `lz4_flex`, `serde` и `toml`; внешние версии закреплены в workspace `Cargo.toml`. `rvepak` зависит от `engine-pak` и не содержит отдельного archive parser или owner resolver. На Windows `seek_read` меняет cursor, поэтому `Archive` использует отдельный file handle для payload read и сверяет ArchiveId до чтения; Unix сохраняет первоначальный positional handle. `Archive::reopen_for_worker` открывает новый handle и заново проверяет identity. Код не сериализует параллельные archive reads глобальным mutex.

`engine-pak` и `rvepak` не выполняют worker scheduling, async coordination или интеграцию с FBX/PNG. Worker scheduling принадлежит `engine-resources`; декодирование форматов — `engine-assets`. См. [[Engine/EngineResources]] и [[Engine/EngineAssets]].
