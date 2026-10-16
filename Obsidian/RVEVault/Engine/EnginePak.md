# EnginePak

Parent: [[Index]]

## Назначение

`engine-pak` читает legacy RVEPAK v1 и текущий v2, записывает v2, строит и проверяет наборы архивов. `Archive` владеет проверенными metadata и backing source, `ArchiveWriter` добавляет локальные blobs через временный spool, `ArchiveCatalog` объединяет namespace v1/v2 и разрешает cross-archive references, а builder превращает строгий TOML manifest в полный набор архивов. CLI `tools/rvepak` использует этот библиотечный API для build/list/verify. Фоновое чтение и выбор packed-only режима предоставляет [[Engine/EngineResources]], который используют [[Engine/EngineAssets]], runtime игры и sandbox.

Полная wire-раскладка — заголовок, индекс, ссылки и chunk descriptors — нормативно описана в [`docs/rvepak-format.md`](../../../docs/rvepak-format.md). Архив хранит opaque bytes без ограничений расширений. Chunk codec независим для каждой локальной blob; доступны None, Zstandard и LZ4 block.

## Источники

| Файл | Значимые символы | Роль |
|---|---|---|
| `engine/engine-pak/src/format.rs` | `ArchiveId`, `BlobId`, `ContentHash`, codec/reference types | Ручная LE serialization, формат и проверки index |
| `engine/engine-pak/src/reader.rs` | `Archive`, `ArchiveLimits`, `MemoryFootprint`, `ReadControl` | Metadata, stat/list/ranges, integrity и memory reservation |
| `engine/engine-pak/src/writer.rs` | `ArchiveWriter`, `BlobOptions`, `FinishedArchive` | Streaming writer, aliases и expected-hash check |
| `engine/engine-pak/src/catalog.rs` | `ArchiveCatalog`, `CatalogAsset`, `CatalogLimits`, `discover_archives` | Namespace uniqueness, declaring archive and blob owner, bounded archive discovery |
| `engine/engine-pak/src/registry.rs` | `ArchiveSetRegistry`, `ArchiveSetRegistryError` | TOML registry for a complete archive set and bidirectional validation |
| `engine/engine-pak/src/builder.rs` | `BuildManifest`, `BuildPlan`, `BuildReport`, `BuildFileInput`, `ProgrammaticArchive`, `preflight`, `execute`, `build_archive_set` | Manifest and explicit-file inputs, deduplication and safe publication |
| `tools/rvepak/src/main.rs` | `build`, `list`, `verify` | CLI поверх builder и общего `ArchiveCatalog` |
| `engine/engine-pak/src/error.rs` | `PakError` | Typed path/not-found/I/O/corrupt/range/unsupported/budget/cancel categories |
| `engine/engine-pak/tests/rvepak.rs` | integration tests, sparse and recording readers | Immutable v1 read fixture, v2 write fixture, range semantics, corruption, memory and owner behavior |

## Публичный контракт

- `Archive` открывает один `.rvepak`, валидирует header/index до чтения assets и предоставляет `stat`, отсортированный `list`, `read_range`, `read_all` и `verify`. Диапазоны half-open; пустой диапазон допускается внутри размера, выход за границы не clamp-ится. Декодируются только пересекающиеся chunks.
- Один archive может открыться для проверки metadata даже с external references. Доступ к внешнему ресурсу требует `ArchiveCatalog`, который проверяет отсутствие дублирующих ArchiveIds и полных paths и требует, чтобы owner действительно содержал local BlobId.
- `CatalogAsset::archive_id` обозначает архив, который объявляет virtual path, а `owner_archive_id` — архив local blob. Для каталога, открытого из директории, `ArchiveCatalog::archive_paths()` связывает ArchiveId с именем файла; это сопоставление использует проверка реестра.
- `ArchiveWriter<W: Write + Seek>` пишет v2: chunks сначала попадают в spool-файл, затем `finish` записывает заголовок и индекс и потоково копирует payload. `new` использует системный temp-каталог, а `with_spool_dir` позволяет builder передать staging-каталог; spool удаляется после успеха, ошибки или drop. Публичные `VERSION`/`HEADER_SIZE` описывают формат записи v2; на Unix spool создаётся с правами `0600`. Writer хранит bounded chunk buffer, проверяет optional ожидаемый BLAKE3 и не deduplicates автоматически. `add_alias` разделяет local blob между путями; `add_external` ссылается на BlobId owner. `finish` возвращает ArchiveId и mapping local BlobId → decoded content hash.
- `BlobId` — archive-local ordinal, а не content hash, чтобы будущий builder мог сохранять повторные файлы как отдельные blobs. Content hash хранится отдельно; aliases задаются явно.
- `ReadControl::reserve` получает footprint результата, самого большого stored/decoded chunk и codec workspace до крупных allocation. Reservation живёт до завершения чтения; cancellation проверяется перед reservation и между chunks. Default limits: 256 MiB serialized uncompressed index per archive, 256 MiB result, 512 MiB aggregate read footprint; default window Zstandard — 8 MiB. Index limits count bytes in the format, not parsed metadata memory.
- Reader проверяет заголовок и границы разделов v2; chunk offsets абсолютны и лежат в data-разделе после индекса. Legacy v1 с payload перед индексом остаётся читаемым. Каталог открывает только верхний уровень каталога, сортирует `.rvepak` по имени и применяет общий лимит сериализованных несжатых индексов до allocation индекса следующего архива. Парсер временно хранит raw index и строит BTree metadata, а каталог сохраняет отдельные записи путей, поэтому фактическая память может быть больше этого лимита и не входит в read budget. Повреждённый архив прекращает открытие каталога; loose-file fallback отсутствует.
- Builder проверяет все имена, selectors и уникальность полного virtual path до hash prepass. Deduplication выбирает первый input в manifest порядке, использует локальные aliases внутри архива и прямые внешние ссылки на local blob ранее созданного архива. Пропускаемые повторы повторно читаются и сверяются с prepass перед ссылкой.
- `build_archive_set(archives, output, defaults, options)` принимает упорядоченные `ProgrammaticArchive` с явными `BuildFileInput { source, path, codec, chunk_size, zstd_level }`. Проверяет уникальные virtual paths, обычные файлы-источники без symlink, безопасные имена и компрессию, затем передаёт общий `BuildPlan` в тот же `execute`: staging, каталог, spool и атомарная публикация совпадают с manifest-путём. Каждому программному входу корнем служит родительский каталог его канонического источника, поэтому источники могут находиться на разных дисках. Имена передаются без `.rvepak`; файлы получают это расширение. Первый архив в списке становится владельцем совпадающих blobs.
- `execute` пишет набор в собственный временный соседний каталог, открывает его через тот же `ArchiveCatalog`, а затем атомарно публикует с запретом замены существующей папки. Полную проверку содержимого выполняет CLI `verify`; сборка не делает второй полный проход по сжатым payloads.
- TOML manifest задаёт `root` и `output` относительно manifest-файла, упорядоченные `archives` и файловые или каталожные `inputs` относительно root. Допустимы любые opaque asset bytes; неизвестные поля, symlinks, пустые группы и небезопасные archive basenames запрещены.
- `ArchiveSetRegistry::parse` и `to_toml` обслуживают `/rve/assets.toml` формата 1. Реестр требует `base` первым, назначение `/rve/assets.toml` архиву `base`, точный список файлов `<name>.rvepak`, уникальные имена из 1–64 ASCII букв, цифр, `_` и `-` без зарезервированных имён Windows и хотя бы один путь для каждого non-base архива. Другие `/rve/` пути не обязательны; сам файл реестра должен находиться локально в base. Значение `assets` называет архив, чья file table объявляет путь; `CatalogAsset::archive_id` хранит это отдельно от `owner_archive_id`, чтобы внешняя dedup-ссылка не меняла распределение. Сверка проверяет каталожные пути и реестр в обе стороны.

## Зависимости и интеграция

Крейт зависит от `engine-files`, `blake3`, `zstd`, `lz4_flex`, `serde` и `toml`; внешние версии закреплены в workspace `Cargo.toml`. `rvepak` зависит от `engine-pak` и не содержит отдельного archive parser или owner resolver. На Windows `seek_read` меняет cursor, поэтому `Archive` использует отдельный file handle для payload read и сверяет ArchiveId до чтения; Unix сохраняет первоначальный positional handle. `Archive::reopen_for_worker` открывает новый handle и заново проверяет identity. Код не сериализует параллельные archive reads глобальным mutex.

`engine-pak` и `rvepak` не выполняют worker scheduling, async coordination или интеграцию с FBX/PNG. Worker scheduling принадлежит `engine-resources`; декодирование форматов — `engine-assets`. См. [[Engine/EngineResources]] и [[Engine/EngineAssets]].
