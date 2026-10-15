# EngineResources

Parent: [[Index]]

## Назначение

`engine-resources` предоставляет общий фоновый доступ к готовым бинарным ресурсам. Он читает loose files и RVEPAK, но не декодирует FBX/PNG и не зависит от окон, renderer, сценариев или async runtime. Потребители получают cloneable `ResourceHandle` и отправляют запросы на metadata, полный файл или диапазон байтов.

`engine-assets::Assets::with_resources` использует handle для загрузки FBX/PNG и внешних текстур. Sandbox запускает сервис после проверки аргументов и `bootstrap.rves`, затем синхронно подготавливает CPU-содержимое до создания окна. Renderer загружает уже готовые геометрию и пиксели на своём потоке; оконные callbacks не ждут запросы ресурсов. Порядок запуска sandbox описан в [[Engine/Sandbox]].

## Источники и namespace

`ResourceSystem::start` закрепляет относительный `ResourceConfig::root` относительно текущей рабочей директории. Если в корне найдены верхнеуровневые обычные файлы `.rvepak` без учёта регистра расширения, включается packed-only режим: `ArchiveCatalog` проверяет заголовки, индексы, владельцев и уникальность virtual paths до готовности сервиса. Loose files в этом режиме игнорируются. Ошибка архива не включает fallback. Если архивов нет, сервис читает loose files из корня; отсутствующий корень допускается при старте, а запрос к отсутствующему ресурсу возвращает typed `NotFound`.

Virtual paths используют UTF-8, чувствительный к регистру namespace `engine-files::VirtualPath`; ведущий `/` допустим, разделитель — `/`. Loose backend проверяет canonical path относительно корня и отклоняет symlink, ведущий наружу. Расширения файлов не ограничиваются. Сам RVEPAK формат и межархивные ссылки описаны в [[Engine/EnginePak]] и [`docs/rvepak-format.md`](../../../docs/rvepak-format.md).

## Потоки, запросы и память

Сервис запускает один coordinator и ограниченный пул синхронных I/O workers. По умолчанию число workers — доступный parallelism, ограниченный диапазоном 1–4; очередь принимает 128 задач. Файловый доступ и распаковка выполняются только на workers. `ResourceRequest::receive` — runtime-agnostic Future, `try_receive` не блокирует, а `wait` предназначен для startup и legacy callers, не для event loop.

`ResourceConfig` задаёт общий предел archive index в 256 MiB, максимум результата одного чтения в 256 MiB и суммарный предел временных read buffers вместе с ещё не полученными результатами в 512 MiB. Для каждого чтения резервируется footprint результата, stored/decoded chunk и codec workspace до крупных аллокаций. Временная часть освобождается при завершении чтения, а разрешение на память результата удерживается до получения или удаления ответа. После `receive` владение байтами и памятью переходит потребителю. Поэтому небольшой диапазон внутри слишком большого compressed chunk может завершиться `BudgetExceeded` до чтения payload.

Удаление request отменяет ещё не завершённую работу. `shutdown` прекращает приём запросов, завершает queued requests и joins coordinator/workers; локальный системный read нельзя прервать до ближайшей границы чтения. Сохранённые handles после остановки возвращают `Stopped`.

## Источники

| Файл | Значимые символы | Роль |
|---|---|---|
| `engine/engine-resources/src/lib.rs` | `ResourceConfig`, публичный exports | Настройки, пределы и библиотечная граница |
| `engine/engine-resources/src/service.rs` | `ResourceSystem`, `ResourceHandle`, `ResourceRequest` | Координация workers, backend выбор, отмена и memory leases |
| `engine/engine-resources/src/error.rs` | `ResourceError`, `ResourceErrorKind` | Типизированные ошибки запросов, бюджета, архивов и loose paths |
| `engine/engine-resources/src/service/tests.rs` | unit tests, recording reader | Startup modes, ranges/codecs, concurrency, shutdown и budgets |

## Зависимости и интеграция

Крейт зависит от `engine-files` для virtual paths и positional reads, `engine-pak` для discovery, каталога и декодирования архивов, а также bounded async-capable channel `flume`. Он не добавляет глобальный runtime, scheduler, asset cache или format decoder. `ResourceHandle::root` возвращает закреплённый корень для совместимого `Assets::root()` API.
