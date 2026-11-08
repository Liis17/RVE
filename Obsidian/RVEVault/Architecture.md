# Architecture

Parent: [[Index]]

## Назначение и границы

RVE — workspace игрового движка на Rust. `engine-runtime` соединяет проектные настройки, сцены, скрипты, окно, ресурсы и Vulkan renderer для игры `rve-player`; sandbox остаётся отдельным приложением ручной проверки, а `rve-editor` — графический редактор сцен на egui. Рабочий путь создаёт настольное окно, выбирает совместимое Vulkan-устройство, запекает Radiance HDR panorama или procedural sky в image-based lighting (IBL), рисует PBR metallic-roughness меши с tangent-space normal maps, emissive, ambient occlusion и opaque/mask/blend alpha, одним directional light со стабильными camera-relative PCF shadow cascades и кластеризованными forward point/spot lights в HDR offscreen-изображение, затем применяет temporal AA/upscale либо FXAA/bilinear output, per-camera exposure и tonemapping перед swapchain. `engine-scene` разбирает и проверяет `.rvescene` и общие `.camerasettings`, строит runtime-граф `World` и выдаёт активную камеру с настройками вывода, directional и локальные источники света, environment и меши для кадра. `engine-script` запускает Lua и Rust-скрипты из ABI-библиотеки и работает с immutable query snapshot; runtime и editor Play передают начальный snapshot до `on_start` и соединяют команды и physics events с `PhysicsServer`. Модели FBX, текстуры PNG, environment HDR и настройки камер читаются из loose assets или RVEPAK через `engine-resources`. `engine-physics` предоставляет CPU-геометрию, bounded broadphase/narrowphase и raycast, а также fixed-step `PhysicsWorld` с solver, triggers и pair events; runtime и editor Play используют отдельный `PhysicsServer` thread (контракт — [[Engine/EnginePhysics]]). Звук, сеть и нейросетевые вычисления также представлены потоками-заглушками.

## Стек

| Технология | Роль | Источник |
|---|---|---|
| Rust 2024, Cargo workspace с resolver 3 | Сборка библиотечных крейтов и приложения | `Cargo.toml` |
| winit 0.30.13 | Окно и цикл событий | `engine/engine-window/Cargo.toml` |
| ash 0.38.0 | Vulkan bindings и runtime-загрузка loader | `engine/engine-render/Cargo.toml`, `engine/engine-render/src/lib.rs` |
| naga 30.0.1 | Компиляция GLSL-шейдеров в SPIR-V при сборке, без Vulkan SDK | `engine/engine-render/build.rs` |
| glam 0.34.1 | Камеры, трансформации, свет, матрицы сцены и physics-геометрия | `engine/engine-scene/src`, `engine/engine-physics/src`, `engine/sandbox/src/scene.rs`, `engine/sandbox/src/content.rs` |
| blake3 1.8.7 | ArchiveId, hashes chunks и полных decoded blobs | `engine/engine-pak/src` |
| serde 1.0.229, toml 0.8.23 | Строгая типизированная конфигурация manifest для builder | `engine/engine-pak/src/builder.rs` |
| zstd 0.14.2 | Независимые Zstandard frames в RVEPAK | `engine/engine-pak/src` |
| lz4_flex 0.14.0 | Независимые LZ4 blocks в RVEPAK | `engine/engine-pak/src` |
| flume 0.11.1 | Bounded очереди ресурсов и снимков кадров, каналы управления потоками runtime | `engine/engine-resources/Cargo.toml`, `engine/engine-runtime/Cargo.toml` |
| cpal 0.18.2 | Вывод звука на устройство Windows/macOS | `engine/engine-audio/src/output.rs` |
| lewton 0.10.2 | Декодирование Ogg Vorbis клипов | `engine/engine-audio/src/decode.rs` |
| rtrb 0.4.0 | SPSC-кольца между потоком `rve-audio` и callback вывода | `engine/engine-audio/src/lib.rs`, `engine/engine-audio/src/mixer.rs` |
| sysinfo 0.39.6 (только фича `system`) | Проверка RAM и логических CPU в runtime, метрики `--debug-overlay` | `engine/engine-runtime/Cargo.toml`, `engine/sandbox/src/debug_overlay.rs` |
| font8x8 0.3.1 | Bitmap-шрифт 8×8 оверлея | `engine/sandbox/src/debug_overlay.rs` |
| egui 0.36.2, egui-winit 0.36.2, egui_dock 0.21.1 | Интерфейс редактора и developer console, ввод из winit, встраиваемые панели | `engine/engine-ui/Cargo.toml`, `engine/engine-runtime/Cargo.toml`, `editor/src` |
| rfd 0.17.2 | Системные диалоги выбора файлов и папок редактора | `editor/src/app.rs` |
| ufbx 0.11.5 | Загрузка FBX; C-библиотека собирается через `cc` | `engine/engine-assets/src/lib.rs` |
| png 0.18.1 | Декодирование PNG | `engine/engine-assets/src/lib.rs` |
| mlua 0.12.2 (`lua54`, `vendored`) | Lua host для lite-скриптов без системной Lua | `engine/engine-script/Cargo.toml` |
| libloading 0.9.0 | Динамическая загрузка библиотек Rust-скриптов | `engine/engine-script/Cargo.toml`, `engine/engine-script/src/library.rs` |
| ash-window 0.13.0 | Surface и обязательные instance extensions через платформенный адаптер | `engine/engine-platform-windows/Cargo.toml`, `engine/engine-platform-macos/Cargo.toml` |
| raw-window-handle 0.6.2 | Передача display/window handles в renderer | `engine/engine-render/Cargo.toml`, `engine/engine-render/src/lib.rs` |
| Vulkan 1.3 | Dynamic rendering, synchronization2 и графический вывод | `engine/engine-render/src/lib.rs` |
| MoltenVK | Vulkan поверх Metal на macOS, portability-настройки | `engine/engine-platform-macos/src/lib.rs` |

Версии внешних крейтов закреплены в `Cargo.toml`. `engine-render` не зависит от winit: он принимает обобщённое окно через `raw-window-handle`.

`.cargo/config.toml` задаёт `build/` в корне репозитория как каталог артефактов Cargo.
Он исключён из Git через `.gitignore`. Сборка, проверки и запуск выполняются с рабочим
каталогом `build/`; debug-бинарник sandbox находится в `build/debug/`.

## Компоненты

- [[Engine/EngineCore]] — `Time`, независимый от остальных крейтов.
- [[Engine/EngineWindow]] — цикл окна на winit.
- [[Engine/EnginePlatform]] — общий интерфейс и отдельные адаптеры Windows/macOS.
- [[Engine/EngineRender]] — Vulkan renderer, независимый от конкретной оконной библиотеки.
- [[Engine/EngineFiles]] — канонические virtual paths, Windows filesystem path normalization и `ReadAt` для позиционного чтения.
- [[Engine/EngineProject]] — общие форматы настроек проекта и bootstrap для приложений и инструментов.
- [[Engine/EnginePak]] — чтение RVEPAK v1/v2, запись v2, потоковый writer и каталог архивов.
- [[Engine/EngineResources]] — фоновый доступ к loose files и RVEPAK через ограниченный worker pool.
- `rvepak` — CLI для сборки, просмотра и проверки наборов архивов через `engine-pak`.
- [[Engine/EngineAssets]] — пути ресурсов и загрузка FBX/PNG/Radiance HDR в память CPU, без зависимости от renderer.
- [[Engine/EngineScene]] — форматы `.rvescene` и `.camerasettings`, их проверка и runtime-граф `World` с множеством камер, трансформациями, светом, ресурсами и script-конфигурацией; независимо от renderer и script runtime.
- [[Engine/EngineAnimation]] — скелетная анимация без GPU и без файлов: позы, клипы по именам костей, crossfade, blend tree, разовые клипы с переходом, палитры skinning и мост `AnimationBridge` между `World` и аниматорами объектов.
- [[Engine/EngineScript]] — Lua host и загрузчик ABI-библиотеки Rust-скриптов; получает параметры, время и локальную трансформацию из `engine-scene`, не зависит от runtime, renderer или окна.
- [[Engine/EngineConsole]] — общий лог, буфер, парсер/каталог команд и egui developer-console overlay; `engine-console` не зависит от окна/GPU, `engine-ui` адаптирует egui к UI pass.
- [[Engine/RveSdk]] — независимый SDK с C ABI и guest API для игровых `cdylib`.
- [[Engine/EngineRuntime]] — потоки игры, порядок запуска, ограниченный конвейер кадров и завершение; библиотека запуска используется бинарником `rve-player`.
- [[Engine/RveBuild]] — сборка релиза из проекта, раскладка архивов и генерация player crate.
- [[Engine/Sandbox]] — приложение, соединяющее core, window, render, assets и scene.

`engine-physics` предоставляет bounded геометрию и fixed-step `PhysicsWorld`; runtime и editor Play строят физическую сцену из CPU-моделей, создают sequence 0 snapshot до callback-ов скриптов и используют lockstep `PhysicsServer`. Формы, solver, query, callbacks и server protocol описаны в [[Engine/EnginePhysics]]. [[Engine/EngineAudio]] воспроизводит клипы через свой микшер на `cpal`; `engine-network` и `engine-neural` остаются модулями-заглушками с `new()` и `run(stop)`.

## Направления зависимостей

- `rve-editor` зависит от `engine-animation`, `engine-assets`, `engine-files`, `engine-physics`, `engine-project`, `engine-render`, `engine-resources`, `engine-scene`, `engine-script`, `engine-window`, `engine-console`, `engine-ui`, egui, egui-winit, egui_dock (с feature `serde`), flume, rfd, glam, serde и serde_json. От него ничего не зависит.
- `sandbox` зависит от `engine-core`, `engine-project`, `engine-platform`, `engine-window`, `engine-render`, `engine-assets`, `engine-resources`, `engine-scene`, glam, sysinfo и font8x8.
- `rve-player` зависит от `engine-runtime`; runtime соединяет `engine-project`, `engine-files`, `engine-platform`, `engine-window`, `engine-render`, `engine-assets`, `engine-resources`, `engine-scene`, `engine-script`, `engine-console`, `engine-physics` `engine-audio` (звук), `engine-animation` (аниматоры) и заглушки `engine-network`, `engine-neural`. Feature `dev-console` включает опциональные `engine-ui`, egui и winit; она включена по умолчанию у Cargo-пакета player и отключена для distribution-сборки `rve-build`. `engine-files` снимает Windows verbatim-префикс в `Layout`; flume обслуживает управляющие каналы и ограниченную очередь снимков, sysinfo сверяет RAM и CPU с требованиями проекта.
- `rve-build` использует `engine-project`, `engine-scene`, `engine-assets`, `engine-resources`, `engine-pak`, `engine-files` и `engine-script`: проверяет включённые сцены и ресурсы, собирает Rust-скрипты как `cdylib`, проверяет их ABI и имена, затем собирает distribution-player из checkout в `build/player`, а dev-player — в `build/player-dev`. Платформенный конвейер macOS дополнительно собирает Vulkan loader и MoltenVK в релиз; подробности — в [[Engine/RveBuild]].
- `engine-project` зависит от `engine-files`, `rve-sdk`, serde и toml; `rve-sdk::PhysicalInput` переиспользуется для проверки имён клавиш и кнопок мыши в проектной input-схеме. Контракты файлов и их потребители описаны в [[Engine/EngineProject]].
- `engine-pak` зависит от `engine-files`, blake3, zstd, lz4_flex, serde и toml; он содержит и wire-формат архивов, и отдельный TOML-контракт `ArchiveSetRegistry`. Инструмент `rvepak` зависит от него. `engine-resources` использует `engine-core` (типы отчёта профайлера для `ResourceSampler`), `engine-files`, `engine-pak` и bounded channels flume; `engine-assets`, sandbox и runtime получают данные через его handle. `engine-files` не имеет внешних зависимостей.
- `engine-scene` зависит от glam, `engine-files`, serde и toml: документы `.rvescene` используют `VirtualPath`, а `World` рассчитывает сцены независимо от renderer и script runtime. Runtime игры использует документы и `World`; sandbox продолжает строить процедурную демонстрационную сцену на математических компонентах.
- `engine-script` зависит от `engine-project`, `engine-scene`, `engine-physics`, `engine-console`, `glam`, `mlua` с Lua 5.4 и vendored исходниками, `rve-sdk` и `libloading`. Он связывает `World` с Lua и Rust-скриптами из ABI-библиотеки, рассчитывает именованные input actions из `InputSettings`, принимает опциональные physics snapshots/events и выдаёт staged commands, но не зависит от runtime, renderer или window; Lua state и экземпляры остаются на scripts thread, а отдельный interrupt handle можно передать другому потоку.
- `rve-sdk` — самостоятельный крейт без зависимостей; он задаёт C ABI и гостевой API для игровых `cdylib`.
- `engine-animation` зависит от `engine-assets`, `engine-scene`, `engine-files` и glam; dev-зависимость `ufbx` нужна оракулу. Потребители: `engine-runtime` и `rve-editor`. Крейт не зависит от renderer, окна и script runtime; скелеты и клипы приходят через замыкания.
- `engine-assets` зависит от `engine-resources`, ufbx и png; от renderer и окна не зависит. Кроме статической геометрии он читает скелет, skin и клипы FBX. `Assets::model_dependencies` использует тот же поиск внешних текстур и stat-источник, что и `load_model`, чтобы builder мог собрать зависимости без декодирования PNG.
- `engine-window` зависит от winit.
- `engine-render` зависит от ash, engine-platform и raw-window-handle, но не от winit и glam; naga — его build-зависимость.
- `engine-platform` выбирает `engine-platform-windows` или `engine-platform-macos` через target-specific зависимости Cargo. Адаптеры зависят от ash и ash-window; обратной зависимости от renderer нет.
- `engine-physics` зависит от `engine-scene` (построение физической сцены из `World`), glam (CPU-геометрия) и flume (протокол потока `rve-physics`). Потребители: `engine-script`, `engine-runtime` и `rve-editor`. `engine-audio` зависит от `engine-scene`, `engine-resources`, `cpal`, `lewton`, `rtrb`, `flume` и `log`; потребители: `engine-runtime` и `rve-editor`. `engine-core`, `engine-network` и `engine-neural` не имеют внешних зависимостей.

Источники: `Cargo.toml`, `engine/engine-core/Cargo.toml`, `engine/engine-project/Cargo.toml`, `engine/engine-window/Cargo.toml`, `engine/engine-platform/Cargo.toml`, `engine/engine-platform-windows/Cargo.toml`, `engine/engine-platform-macos/Cargo.toml`, `engine/engine-render/Cargo.toml`, `engine/engine-files/Cargo.toml`, `engine/engine-pak/Cargo.toml`, `engine/engine-resources/Cargo.toml`, `engine/engine-scene/Cargo.toml`, `engine/engine-console/Cargo.toml`, `engine/engine-console/src`, `engine/engine-ui/Cargo.toml`, `engine/engine-ui/src`, `engine/engine-script/Cargo.toml`, `engine/engine-script/src`, `engine/engine-script/src/library.rs`, `engine/rve-sdk/Cargo.toml`, `engine/engine-assets/Cargo.toml`, `engine/engine-runtime/Cargo.toml`, `engine/engine-runtime/src/console.rs`, `engine/rve-player/Cargo.toml`, `engine/engine-physics/Cargo.toml`, `engine/engine-physics/src`, `engine/engine-audio/Cargo.toml`, `engine/engine-audio/src`, `engine/engine-network/Cargo.toml`, `engine/engine-neural/Cargo.toml`, `engine/sandbox/Cargo.toml`, `editor/Cargo.toml`.

## Файловые источники и RVEPAK

`engine-files::VirtualPath` задаёт case-sensitive UTF-8 namespace ресурсов, а `ReadAt` и `FileSource` читают абсолютные `u64`-диапазоны без общего archive lock. `engine-pak::Archive` читает legacy v1 и проверяет секции v2 (96-байтовый заголовок, индекс, data), открывает range reads по пересекающимся чанкам и проверяет chunk/full-blob BLAKE3. `ArchiveCatalog` смешивает версии, проверяет уникальность полного virtual path и связывает external references только с local blob указанного владельца; `CatalogAsset::archive_id` отдельно сохраняет архив, объявивший путь. `ArchiveWriter` пишет v2 через временный spool и возвращает ArchiveId и BlobId/hash map для следующих архивов. Нормативная раскладка обеих версий описана в [`docs/rvepak-format.md`](../../docs/rvepak-format.md).

`engine-pak::preflight` раскрывает и проверяет все inputs manifest до BLAKE3 prepass и записи, а `execute` строит весь набор в staging directory, проверяет его через `ArchiveCatalog` и публикует с атомарным запретом замены существующего output. `tools/rvepak` предоставляет команды `build`, `list` и `verify`; только `verify` повторно декодирует все unique local blobs. Manifest и политика dedup описаны в [[Engine/EnginePak]].

`engine-pak::build_archive_set` принимает явные файлы, virtual paths и compression overrides, формирует тот же build plan и использует общий `execute`; порядок архивов задаёт владельца при deduplication. Для каждого программного входа корнем служит родительский каталог канонического файла, поэтому файлы могут быть на разных дисках. `ArchiveSetRegistry` описывает каталог набора в `/rve/assets.toml`: запись реестра назначает этот путь архиву `base`, base содержит его локально, а остальные записи `assets` указывают declaring archive. Это соглашение набора, не часть wire-формата `.rvepak`.

Чтение ограничивается configurable index, result, chunk и memory budgets. `ReadControl` резервирует footprint результата, stored/decoded чанка и codec workspace до аллокаций и позволяет отменить чтение между чанками. На Windows `seek_read` меняет file cursor, поэтому file-backed payload reads и worker reopenings используют независимые handles с проверкой ArchiveId; на Unix исходный positional handle остаётся закреплён за Archive.

Каталог обнаруживает верхнеуровневые `.rvepak` файлы без loose-file fallback. `engine-resources::ResourceConfig` выбирает `SourceMode::Auto` (старое обнаружение packed/loose), `Loose` (только файлы) или `Registry` (base → `/rve/assets.toml` → точный состав архива → каталог и сверка путей). Любая ошибка Registry останавливает startup до запуска workers. Сервис ограничивает очередь и memory footprint и выполняет чтение в background workers. Builder принимает любые opaque bytes и размещает архивы верхнего уровня; sandbox использует `assets` как root по умолчанию с `Auto`.

`tools/rve-build` использует `SourceMode::Loose` для проекта, разбирает только включённые сцены и собирает потребляемые ими ресурсы, AppIcon и внешние FBX-текстуры. Он задаёт распределение файлов через `ArchiveSetRegistry`: сцены и служебные TOML находятся в `base`, явные архивы сохраняются, а остальные ресурсы получают `scene-<stem>` или `shared`. `build_archive_set` вызывается с порядком `base`, `shared`, затем именованные архивы. Инструмент собирает `rve-player` из checkout либо использует prebuilt player рядом с собой при наличии SDK-маркера; команда `sdk` упаковывает оба бинарника и исходник `rve-sdk` для игры-клиента. Полный конвейер, правила назначения и ограничения платформенной упаковки описаны в [[Engine/RveBuild]].

## Основные потоки

### Запуск и цикл окна

`engine/sandbox/src/main.rs` разбирает аргументы и загружает `BootstrapConfig` через
`engine_project::BootstrapConfig::load`. Sandbox применяет размер и заголовок окна;
прочие bootstrap-поля разбираются, но не меняют его поведение. Поля, defaults и
расположение файлов для приложений описаны в [[Engine/EngineProject]]. После этих проверок
`ResourceSystem::start` открывает `assets` в `SourceMode::Auto` (или проверяет его архивный каталог), затем
`Assets::with_resources` загружает CPU-содержимое кадра до создания окна. Это происходит
и для процедурных сцен; отсутствующий loose root допустим, а ошибка архива или ресурса
завершает запуск с кодом 2. FBX разбирается через `ufbx::load_memory` на вызывающем
startup-потоке, внешние текстуры читаются через тот же handle, GPU upload выполняется
позже при renderer initialization.
`engine-window` создаёт event loop и окно в `resumed`, после чего вызывает `WindowHandler::init`. В каждом `about_to_wait` вызывается `update`, а затем запрашивается перерисовка; `frame` вызывается при `RedrawRequested`. Изменения размеров передаются в физических пикселях через `resized`.

При выходе `WindowHandler::exiting` вызывается до освобождения окна. Sandbox удаляет renderer в этом callback; после возврата из event loop сначала уничтожается `Sandbox`, затем `ResourceSystem` joins coordinator и workers.

Источники: `engine/engine-project/src/bootstrap.rs`, `engine/engine-project/src/project_settings.rs`, `engine/sandbox/src/main.rs`, `engine/engine-window/src/lib.rs`.

### Игровой runtime

`rve-player` вызывает `engine_runtime::run_player()`. Runtime сначала канонизирует каталог текущего исполняемого файла (на macOS внутри `.app` — заменяет его на `Contents/Resources`) и вызывает `prepare_vulkan_environment` до создания потоков. Release layout использует `Engine/bootstrap.rves`, `Assets` в режиме Registry и `/rve/project.toml` из ResourceHandle; dev layout от `--project` использует `bootstrap.rves`, `Assets` в режиме Loose и `project.rveproject` из каталога проекта. `--scripts <path>` выбирает ABI-библиотеку; release автоматически ищет её под фиксированным именем в `Engine/`, dev требует явный путь. Ни один путь не зависит от текущего рабочего каталога. После загрузки настроек runtime применяет singleton lock по `ProjectSettings.id`, проверяет Vulkan-устройство, RAM и CPU, затем на startup-потоке читает сцену, FBX/PNG, HDR environment и исходники Lua. До запуска потоков он строит и проверяет `PhysicsSceneDesc` из CPU-геометрии базовых моделей и проектных physics settings; hull/BVH создаются один раз, sequence 0 query snapshot готовится до окна, а ошибка формы завершает запуск с кодом 2.

Аниматоры живут на потоке скриптов: мост `AnimationBridge` получает запросы скриптов и даёт события `animation_finished` в том же тике; палитры уходят в кадр renderer-а, где скиннинг считает compute-проход. В варианте player с `dev-console` runtime устанавливает `engine-console` logger с общим scrollback sink. Физический Backquote переключает overlay; пока он открыт, игровой input блокируется и команды скриптов ставятся на scripts thread перед следующим update. Distribution player собирается без этой feature. Подробности — в [[Engine/EngineConsole]].

До открытия окна runtime запускает четыре доменных потока и `rve-scripts`; среди них `rve-physics` обслуживает отправленные Step-запросы. Scripts thread получает initial snapshot до `on_start`, загружает ABI-библиотеку и Lua host и только затем подтверждает `Ready`. Ошибка загрузки возвращается как код 2 до окна. После этого `engine-window` создаёт OnDemand-окно. В `init` главный поток создаёт renderer и загружает GPU-ресурсы, затем отправляет scripts сообщение `Go` и передаёт renderer render-потоку. Scripts строят `FrameSnapshot` в ограниченную очередь ёмкостью 1, render-поток выбирает кадры и команды управления через flume Selector и создаёт `Frame` из актуального размера окна. При occlusion или нулевом размере render отбрасывает снимки, а scripts продолжают обновляться с `background_tick_hz` через control-канал и injected clock. Полный протокол, guards и порядок уничтожения описаны в [[Engine/EngineRuntime]]. Профайлер строится на модуле `engine-core::profile` (только `std`): потоки runtime публикуют отчёты в общую доску, рендерер отдаёт CPU- и GPU-замеры кадра через `take_frame_profile`, а вывод собирают оверлей F3, команда `profile` и вкладка редактора.

Источники: `engine/engine-runtime/src/lib.rs`, `engine/engine-runtime/src/player.rs`, `engine/engine-runtime/src/pipeline.rs`, `engine/engine-runtime/src/scene.rs`, `engine/engine-physics/src/{scene.rs,thread.rs}`, `engine/rve-player/src/main.rs`.

Ввод следует через отдельный поток событий: `engine-window` переводит winit в собственные `Key`/`MouseButton` и вызывает `WindowHandler`; `PlayerWindow` на главном потоке коротко обновляет `Arc<Mutex<InputAccumulator>>`. Scripts thread перед каждым update забирает кадровый snapshot и передаёт его Lua и Rust ABI через `ScriptInput`. Потеря фокуса и Background генерируют released edges и очищают удерживаемое состояние; детали описаны в [[Engine/EngineWindow]], [[Engine/EngineRuntime]], [[Engine/EngineScript]] и [[Engine/RveSdk]].

### Игровой ввод

`project.rveproject` хранит button/axis/vector2 actions и один default context в
`[input]`; схема проверяется в `engine-project`. Runtime передаёт её `engine-script`,
а редактор — при запуске Play. `ScriptHost::set_input` рассчитывает actions из общего
`ScriptInput` snapshot. Изменения контекста и bindings транзакционны: они видны со
следующего snapshot, а не меняют ввод внутри текущей callback-фазы. SDK v5 добавляет
nullable action callbacks в конец ABI host; прежние ABI v3/v4 сохраняют свой layout.
Курсор идёт в обратном направлении: скрипты задают `ScriptCursor` транзакционно в
`engine-script`, runtime или редактор переводят его в `engine_window::CursorState`, а
`engine-window` применяет состояние к winit. `engine-script` окна не знает.
Контракты схемы, API и вычисления описаны в [[Engine/EngineProject]],
[[Engine/EngineScript]], [[Engine/RveSdk]], [[Engine/EngineRuntime]] и
[[Engine/RveEditor]].

### Vulkan-кадр

`Renderer::new` загружает Vulkan loader и создаёт surface через `engine-platform`, затем выбирает физическое устройство с API не ниже 1.3, dynamic rendering, synchronization2, graphics+present queue и compute-capable graphics queue. Для IBL bake требуются `shaderStorageImageExtendedFormats` и storage-image feature у `R16G16B16A16_SFLOAT`; velocity target требует sampled/color-attachment support у `R16G16_SFLOAT`, а pipeline — не менее 208 байт push constants; `probe_devices` и оконный выбор устройства используют одну оценку и причины отказа. Clustered compute также проверяет размер dispatch, storage-buffer range и число storage descriptors. HDR scene format требует color attachment blending для отдельного alpha pass, sampling и линейную фильтрацию; post targets `R8G8B8A8_SRGB` требуют color attachment, sampling и линейную фильтрацию. D32 shadow arrays требуют depth attachment, sampling и linear comparison; image-format query проверяет до 32 layers и 2048×2048 extent. Local shadow array 1024×1024×32 занимает 128 MiB, directional array — четыре слоя 2048×2048. Для mip-цепочек материалов оба формата текстур — `R8G8B8A8_SRGB` и `R8G8B8A8_UNORM` — должны поддерживать sampling, линейную фильтрацию, transfer source/destination и blit source/destination. Дискретное устройство получает приоритет. При нулевом начальном размере swapchain и viewport targets создаются по требованию; обе общие shadow maps создаются при инициализации renderer.

Sandbox, runtime и редактор создают меши с локальной AABB, текстуры с ролью sRGB/linear, immutable PBR materials и environment resources. Base-color и emissive читаются как sRGB; metallic-roughness, tangent-space normal и occlusion — как linear UNORM. Кадр начинает clustered compute: после отбора до 1024 видимых локальных источников по вкладу каждый кластер сетки 16×9×24 получает до 128 пересекающихся источников в стабильном порядке; превышение лимита обрезается. Затем идут directional и local shadows, opaque/mask HDR с RG16F motion vectors, skybox, прозрачный blend, temporal resolve через TAA либо доступный Windows FSR, или spatial output при FXAA/None, tonemap и output. Mask повторяет alpha cutoff в shadow pass. Skybox восстанавливает направление луча через inverse view-projection, рисуется после opaque/mask и до blend; depth test `LESS_OR_EQUAL`, depth write выключен. Blend draws стабильно сортируются по расстоянию камеры до мирового AABB center и идут отдельным HDR pass с обычным alpha blending. Directional и выбранные local lights используют один прямой PBR BRDF; point/spot выбираются по camera frustum и вносят inverse-square lighting с мягкой границей range, local shadows используют отдельный 32-layer 1024² D32 array: point требует шесть слоёв, spot — один; ближайшие видимые requests получают slots, а остальные lights сохраняют direct lighting без тени. Все слои очищаются для каждого scene pass, после чего shader использует matrix/depth-consistent 3×3 PCF. Диапазон ниже 0.001 не затеняется; 90° spot сохраняет полный световой конус при shadow FOV π−0.01. Ambient occlusion умножает только indirect lighting, emissive добавляется после direct и ambient. Перед записью draws renderer консервативно проверяет восемь трансформированных углов bounds по camera clip planes отдельно для каждого scene pass и по light clip planes для shadow pass. Невалидные матрицы оставляют draw видимым, а все draw handles валидируются до culling. Mesh shader использует derivative TBN, Cook-Torrance GGX, Schlick Fresnel, diffuse irradiance cubemap и specular GGX prefilter с BRDF LUT, а также directional и local PCF shadows. Radiance equirectangular и procedural source синхронно запекаются в radiance/irradiance/prefilter cubemaps; одинаковый GPU resource используется между кадрами и заменяется при смене source. Scene pass пишет HDR color, velocity и sampled D32 depth; mesh и skybox velocity используют unjittered current/previous clip coordinates, включая движение skybox от вращения камеры. TAA jitter применяется к rasterization, а temporal resolve использует depth/velocity dilation, output-resolution reconstruction, Catmull-Rom history, YCoCg variance clipping и disocclusion rejection. HDR history хранится до EV/tonemap, поэтому exposure применяется ровно один раз после resolve. Temporal history независим для screen и каждого viewport, а фаза продвигается после успешной queue submission; смена extents/AA/debug, явный reset или пропущенный вид сбрасывают её. FXAA и None не jitter-ят кадр и сохраняют spatial output path. Оверлей и UI рисуются после обработки цвета. GPU-границы записываются после каждого прохода вида (clustered compute, shadows, scene, temporal resolve и tonemap); total вида охватывает их целиком, а вывод на swapchain измеряется отдельно (см. [[Engine/EngineRender]]). Targets пересоздаются после fence своего frame slot при изменении размера или render scale; нулевой размер пропускает кадр.

Источники: `engine/engine-render/src/lib.rs`, `engine/engine-platform-windows/src/lib.rs`, `engine/engine-platform-macos/src/lib.rs`, `engine/sandbox/src/main.rs`.

`Frame::debug_view` выбирает стабильные режимы Lit, Wireframe, Normals, Overdraw, LodLevels и ShadowCascades. Не-Lit режимы отключают temporal jitter и resolve, а смена debug key сбрасывает history. Raw color modes обходят EV и tonemapping; Wireframe — отдельный `LINE` overlay с depth `LESS_OR_EQUAL`, без depth writes и с отрицательным bias, доступный только если выбранный GPU поддерживает `fillModeNonSolid`. Overdraw суммирует красный канал для растеризованных fragments без depth test/write и alpha discard, поэтому masked holes также считаются.

### Кадр редактора

`rve-editor` синхронизирует экземпляры skinning с миром кадра и держит мост превью выбранного аниматора; в Play мост ведёт `PlaySession`. `rve-editor` готовит отдельные draw lists, camera matrices, environments, local shadow matrices и directional cascades для Scene и Game; каждая панель использует собственную камеру, размер viewport и LOD. Renderer независимо строит cluster lists и рисует compute, shadow, opaque, skybox, blend и post passes в viewport targets; затем UI pass рисует примитивы egui прямо в swapchain image, а панели показывают viewport как текстуры без копий на CPU. `engine-scene` задаёт LOD thresholds и cull distance от мировой позиции объекта; runtime и editor применяют их до отправки draws, а renderer независимо выполняет frustum culling по камере и каскадам. Подробности — в [[Engine/RveEditor]], [[Engine/EngineScene]] и [[Engine/EngineRender]].

## Ограничения и инварианты

- `.github/workflows/ci.yml` запускается на PR в `son-croissant` и `mommy`, на push в `mommy` и вручную; push в feature-ветки без PR не запускает его. Проверяет изменённые крейты и всех их транзитивных потребителей на Windows и macOS: форматирование, Clippy, сборку и unit-/integration-/doc-тесты через `-p`. `.github/scripts/affected_packages.py` читает workspace и локальные зависимости всех видов и платформ из `cargo metadata --no-deps`; не связанные с изменениями крейты отдельно не проверяются. Для PR используется diff от merge-base до head, для push — `before..after`. Общие настройки, lockfile, CI и неизвестные пути, а также ручной запуск и push без доступного `before` проверяют весь workspace. Изменения только Markdown вне крейтов, vault Obsidian и `LICENSE` пропускают задания Rust. Тесты выбора выполняются на Ubuntu командой `python3 -B -m unittest discover -s .github/scripts -v`. Workflow создаёт `build/` и выполняет команды Cargo из него. Для ускорения PR проверки идут тремя параллельными матрицами Windows/macOS: `lint` (fmt и Clippy), `test` (сборка и тесты) и `demo` (сборка demo game, проверка изоляции SDK в `rve-build` и диагностика аргументов player). Каждый job компилирует отдельно со своим кешем. `demo` запускается, только если выбраны `rve-build`, `engine-runtime`, `rve-player`, `engine-script` или `rve-sdk` (output `build_demo` job выбора). Общая required-проверка `CI` в защите `mommy` требует успеха выбора крейтов и всех трёх матриц, когда они нужны; пропуск `lint`/`test` допускается только при пустом выборе, а `demo` — только при `build_demo=false`. После успешного `CI` при push в `mommy` тот же workflow публикует SDK через job `release` ([[Engine/RveBuild]]). Тесты движка не создают окно и не загружают Vulkan; фактический графический путь проверяется отдельно через sandbox. Правило сопровождать новые фичи тестами закреплено в `AGENTS.md`.

- `engine-assets` собирает C-исходник ufbx через `cc`, поэтому сборке нужен C-компилятор целевой платформы: MSVC на Windows, clang на macOS. Кросс-проверка Windows с macOS (`--target x86_64-pc-windows-gnu`) требует MinGW или исключения `engine-assets` и `sandbox`.
- Целевые runtime — настольные Windows с Vulkan-драйвером GPU и macOS с Vulkan loader и MoltenVK. Остальные ОС не имеют платформенных адаптеров; мобильный lifecycle не реализован.
- Требуется Vulkan 1.3 и фактическая поддержка требуемых features, очереди, surface usage и форматов; наличие самого Vulkan loader недостаточно.
- Renderer использует общую четырёхслойную карту `D32_SFLOAT` 2048×2048 на слой и один directional light с 1–4 camera-relative cascades; до 1024 выбранных forward point/spot lights распределяются по grid 16×9×24, до 128 индексов на cluster с детерминированной обрезкой переполнения. Локальные тени используют отдельную карту. Опциональный FSR backend доступен только на Windows при предоставленных SDK DLL; DLSS и tessellation pipeline пока недоступны. Environment состоит из radiance cube 512 с mip chain, irradiance cube 32, prefiltered cube 128 с шестью mip levels и однократно рассчитанного CPU BRDF LUT 64×64. Материалы PBR имеют base-color, metallic-roughness, normal, emissive и occlusion maps с заданной ролью и mip-цепочками; opacity maps от FBX пакуются в base-color alpha. Blend sort использует центр AABB, поэтому пересекающиеся или сильно вытянутые прозрачные меши могут потребовать разбиения на отдельные draws. Scene pass остаётся HDR, затем отдельные tonemap и output passes выполняют обработку цвета.
- `FrameCamera` передаёт `view`, `projection` и мировую `position`; их произведение задаёт world-to-clip с осью Y вверх и глубиной `[0, 1]`, Y переворачивается отрицательной высотой viewport. `FrameEnvironment` передаёт renderer-local id, intensity, rotation и skybox flag. Отсутствующий environment выбирает встроенный ambient fallback. `DrawCommand::model` переводит модель в мировое пространство, `previous_model` задаёт transform для motion vectors, а `None` означает отсутствие движения модели.
- LOD определяется евклидовым расстоянием от камеры до мирового origin объекта; object scale, проекционный размер и hysteresis на выбор уровня не влияют. `cull_distance` удаляет object draws до renderer, а frustum culling renderer использует per-pass camera/light matrices. Instancing не реализован.
- При отсутствии `VK_LAYER_KHRONOS_validation` debug renderer сообщает об этом и продолжает без слоя. Vulkan SDK для текущего пути не нужен; macOS требует отдельно установленного Vulkan loader и MoltenVK либо окружения SDK.
- Базовый teardown renderer вызывает `device_wait_idle`, но это не доказывает завершение всех операций presentation engine. Maintenance extensions для этого сейчас не включены.

## Решения и основания

- Отдельные платформенные крейты в одном workspace выбраны по требованию пользователя: изменения общего renderer и остальных модулей движка едины, зависимости от ОС локализованы в адаптерах.
- `engine-render` использует Vulkan 1.3, dynamic rendering и synchronization2. Это задано утверждёнными требованиями к минимальной Vulkan-версии.
- Render resolution отделено от output resolution: кадр рисуется в offscreen-target и передаётся в swapchain отдельным output pass. Color, depth и velocity доступны как sampled inputs последующей temporal обработки.
- Renderer принимает display/window handles через `raw-window-handle` и удерживает `Arc` окна, не вводя прямую зависимость от winit; такой контракт закреплён в утверждённых решениях.
- Расширения instance/device передаются через `RendererConfig` до создания соответствующих объектов; неподдерживаемые запросы приводят к ошибке. Эта возможность включена по утверждённому требованию о будущих интеграциях.
- Тесселяция включается как Vulkan feature только при поддержке выбранного устройства. Самих шейдеров и pipeline для тесселяции нет.
- Шейдеры пишутся на GLSL и компилируются в SPIR-V крейтом naga в `build.rs`; так выбрал пользователь, чтобы сборка локально и в CI Windows/macOS не требовала Vulkan SDK и `glslc`.
- Матрицы считаются крейтом glam по выбору пользователя. Публичный API renderer принимает матрицу как `[[f32; 4]; 4]` и не зависит от glam.
- Ресурсы лежат в `assets` рабочего каталога (`build/assets`) и адресуются путями вида `/model/cube.fbx` — так попросил пользователь. Поддерживаются FBX для моделей, PNG для текстур и Radiance RGBE `.hdr` для environment.
- `engine-assets` возвращает данные CPU в собственных типах и не зависит от renderer; перевод в `engine_render::Vertex` выполняет приложение.

## Дистрибутив редактора

`rve-build sdk` создаёт автономный дистрибутив из checkout: на Windows корень
содержит редактор, сборщик, оба player executable, `rve-sdk.toml`, гостевой
`rve-sdk/` и выбранный Rust `toolchain/`. На macOS editor, builder, оба player,
маркер SDK, crate и toolchain лежат в `RVE Editor.app/Contents/MacOS`, а
Vulkan-библиотеки — в `Contents/Resources/Engine`. Исходники движка в пакет не
входят; установленный checkout для использования готового пакета не требуется.
Генерация SDK выполняется из checkout. Контракты дистрибутива, toolchain и
скриптов описаны в [[Engine/RveBuild]], [[Engine/RveEditor]] и [[Engine/RveSdk]].
