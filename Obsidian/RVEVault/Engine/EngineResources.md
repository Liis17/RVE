# EngineResources

Parent: [[Index]]

## Назначение

`engine-resources` предоставляет общий фоновый доступ к готовым бинарным ресурсам. Он читает loose files и RVEPAK, но не декодирует FBX/PNG и не зависит от окон, renderer, сценариев или async runtime. Потребители получают cloneable `ResourceHandle` и отправляют запросы на metadata, полный файл или диапазон байтов.

`engine-assets::Assets::with_resources` использует handle для загрузки FBX/PNG и внешних текстур. Sandbox запускает сервис после проверки аргументов и `bootstrap.rves`, затем синхронно подготавливает CPU-содержимое до создания окна. Renderer загружает уже готовые геометрию и пиксели на своём потоке; оконные callbacks не ждут запросы ресурсов. Порядок запуска sandbox описан в [[Engine/Sandbox]].

## Источники и namespace

`ResourceSystem::start` закрепляет относительный `ResourceConfig::root` относительно текущей рабочей директории. `ResourceConfig::source_mode` задаёт выбор источника: `Auto` (по умолчанию) сохраняет прежнее поведение — при наличии верхнеуровневых обычных `.rvepak` включает packed-only режим, иначе loose files; `Loose` читает только loose files и не сканирует соседние архивы; `Registry` открывает фиксированный `base.rvepak`, читает из него локальный `/rve/assets.toml`, сверяет точный набор файлов, открывает `ArchiveCatalog` и проверяет все объявления путей до готовности сервиса. В packed-режимах loose files игнорируются; повреждение или несоответствие архива завершает старт без fallback. В Auto без архивов отсутствующий root допускается, а запрос к отсутствующему virtual path возвращает typed `NotFound`. Ошибка backing I/O классифицируется как `Io`, даже если системная причина — `NotFound`.

Имя base задано форматом и не настраивается. В реестре путь `/rve/assets.toml` назначен `base`, а файл хранится локально в `base.rvepak`; другие пути из `/rve/` не обязательны. Реестр и назначение каждого пути описаны в [[Engine/EnginePak]] и [`docs/rvepak-format.md`](../../../docs/rvepak-format.md). Startup errors различают отсутствие base или реестра, некорректный TOML и несовпадение реестра с файлами или каталогом.

Virtual paths используют UTF-8, чувствительный к регистру namespace `engine-files::VirtualPath`; ведущий `/` допустим, разделитель — `/`. Loose backend проверяет canonical path относительно корня и отклоняет symlink, ведущий наружу. Расширения файлов не ограничиваются. Сам RVEPAK формат и межархивные ссылки описаны в [[Engine/EnginePak]] и [`docs/rvepak-format.md`](../../../docs/rvepak-format.md).

## Потоки, запросы и память

Сервис запускает один coordinator и ограниченный пул синхронных I/O workers. По умолчанию число workers — доступный parallelism, ограниченный диапазоном 1–4; очередь принимает 128 задач. Файловый доступ и распаковка выполняются только на workers. `ResourceRequest::receive` — runtime-agnostic Future, `try_receive` не блокирует, а `wait` предназначен для startup и legacy callers, не для event loop.

`ResourceConfig` задаёт общий предел сериализованных несжатых индексов архивов в 256 MiB, максимум результата одного чтения в 256 MiB и суммарный предел временных read buffers вместе с ещё не полученными результатами в 512 MiB. Предел индексов считает байты формата на диске, а не память распарсенных метаданных: при открытии временно хранится raw index, затем создаются BTree metadata, а каталог хранит собственные записи путей. Поэтому память индексов может быть больше лимита и не входит в `max_memory_bytes`; предел RSS процесса этим контрактом не задаётся. Для каждого чтения резервируется footprint результата, stored/decoded chunk и codec workspace до крупных аллокаций. Временная часть освобождается при завершении чтения, а разрешение на память результата удерживается до получения или удаления ответа. После `receive` владение байтами и памятью переходит потребителю. Поэтому небольшой диапазон внутри слишком большого compressed chunk может завершиться `BudgetExceeded` до чтения payload.

Удаление request отменяет ещё не завершённую работу. `shutdown` прекращает приём запросов, завершает queued requests и joins coordinator/workers; локальный системный read нельзя прервать до ближайшей границы чтения. Сохранённые handles после остановки возвращают `Stopped`.

## Источники

| Файл | Значимые символы | Роль |
|---|---|---|
| `engine/engine-resources/src/lib.rs` | `SourceMode`, `ResourceConfig`, публичный exports | Выбор источника, настройки, пределы и библиотечная граница |
| `engine/engine-resources/src/service.rs` | `ResourceSystem`, `ResourceHandle`, `ResourceRequest` | Координация workers, backend выбор, отмена и memory leases |
| `engine/engine-resources/src/error.rs` | `ResourceError`, `ResourceErrorKind` | Типизированные ошибки запросов, бюджета, архивов, реестра и loose paths |
| `engine/engine-resources/src/service/tests.rs` | unit tests, recording reader | Startup modes, ranges/codecs, concurrency, shutdown и budgets |

## Зависимости и интеграция

Крейт зависит от `engine-files` для virtual paths и positional reads, `engine-pak` для discovery, каталога и декодирования архивов, а также bounded async-capable channel `flume`. Он не добавляет глобальный runtime, scheduler, asset cache или format decoder. `ResourceHandle::root` возвращает закреплённый корень для совместимого `Assets::root()` API.
