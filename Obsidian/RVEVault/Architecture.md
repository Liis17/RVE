# Architecture

Parent: [[Index]]

## Назначение и границы

RVE — workspace игрового движка на Rust. Его рабочий путь создаёт настольное окно, выбирает совместимое Vulkan-устройство, рисует меши с текстурами, направленным светом, depth buffer и shadow map в offscreen-изображение и переносит его в swapchain. Математические компоненты камер, трансформаций и света предоставляет `engine-scene`; демонстрационная сцена с вращающимся кубом и плоскостью показывает отбрасываемые тени. Модели FBX и текстуры PNG читаются из loose assets или RVEPAK через `engine-resources`. Доменные runtime для физики, звука, сети и нейросетевых вычислений, а также редактор пока не реализованы.

## Стек

| Технология | Роль | Источник |
|---|---|---|
| Rust 2024, Cargo workspace с resolver 3 | Сборка библиотечных крейтов и приложения | `Cargo.toml` |
| winit 0.30.13 | Окно и цикл событий | `engine/engine-window/Cargo.toml` |
| ash 0.38.0 | Vulkan bindings и runtime-загрузка loader | `engine/engine-render/Cargo.toml`, `engine/engine-render/src/lib.rs` |
| naga 30.0.1 | Компиляция GLSL-шейдеров в SPIR-V при сборке, без Vulkan SDK | `engine/engine-render/build.rs` |
| glam 0.34.1 | Камеры, трансформации, свет и матрицы сцены | `engine/engine-scene/src`, `engine/sandbox/src/scene.rs`, `engine/sandbox/src/content.rs` |
| blake3 1.8.7 | ArchiveId, hashes chunks и полных decoded blobs | `engine/engine-pak/src` |
| serde 1.0.229, toml 0.8.23 | Строгая типизированная конфигурация manifest для builder | `engine/engine-pak/src/builder.rs` |
| zstd 0.14.2 | Независимые Zstandard frames в RVEPAK | `engine/engine-pak/src` |
| lz4_flex 0.14.0 | Независимые LZ4 blocks в RVEPAK | `engine/engine-pak/src` |
| flume 0.11.1 | Bounded request queues и runtime-agnostic async receive в ресурсном сервисе | `engine/engine-resources/Cargo.toml` |
| sysinfo 0.39.6 (только фича `system`) | Память и загрузка CPU процесса для `--debug-overlay` | `engine/sandbox/src/debug_overlay.rs` |
| font8x8 0.3.1 | Bitmap-шрифт 8×8 оверлея | `engine/sandbox/src/debug_overlay.rs` |
| ufbx 0.11.5 | Загрузка FBX; C-библиотека собирается через `cc` | `engine/engine-assets/src/lib.rs` |
| png 0.18.1 | Декодирование PNG | `engine/engine-assets/src/lib.rs` |
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
- [[Engine/EngineFiles]] — канонические virtual paths и `ReadAt` для позиционного чтения.
- `engine-project` — настройки проекта и bootstrap-конфигурация для приложений и инструментов.
- [[Engine/EnginePak]] — версия 1 архива, потоковый writer, reader и каталог архивов.
- [[Engine/EngineResources]] — фоновый доступ к loose files и RVEPAK через ограниченный worker pool.
- `rvepak` — CLI для сборки, просмотра и проверки наборов архивов через `engine-pak`.
- [[Engine/EngineAssets]] — пути ресурсов и загрузка FBX/PNG в память CPU, без зависимости от renderer.
- [[Engine/EngineScene]] — камеры, трансформации и свет на glam, независимо от renderer.
- [[Engine/Sandbox]] — приложение, соединяющее core, window, render, assets и scene.

`engine-audio`, `engine-physics`, `engine-network` и `engine-neural` — пустые модули-заглушки: каждый предоставляет `new()` и `run(stop)`, который ждёт сигнал остановки. Их доменные runtime и полная документация появятся позже. `engine-editor` пока остаётся скелетом без реализации.

## Направления зависимостей

- `sandbox` зависит от `engine-core`, `engine-project`, `engine-window`, `engine-render`, `engine-assets`, `engine-resources`, `engine-scene`, glam, sysinfo и font8x8.
- `engine-project` зависит от `engine-files`, serde и toml. `BootstrapConfig` читает `bootstrap.rves`, а `ProjectSettings` разбирает `project.rveproject`; общая валидация virtual paths использует `engine-files::VirtualPath`.
- `engine-pak` зависит от `engine-files`, blake3, zstd, lz4_flex, serde и toml; инструмент `rvepak` зависит от него. `engine-resources` использует `engine-files`, `engine-pak` и bounded channels flume; `engine-assets` и sandbox читают через его handle. `engine-files` не имеет внешних зависимостей.
- `engine-scene` зависит от glam и не зависит от renderer; sandbox передаёт его вычисленные матрицы через API renderer.
- `engine-assets` зависит от `engine-resources`, ufbx и png; от renderer и окна не зависит.
- `engine-window` зависит от winit.
- `engine-render` зависит от ash, engine-platform и raw-window-handle, но не от winit и glam; naga — его build-зависимость.
- `engine-platform` выбирает `engine-platform-windows` или `engine-platform-macos` через target-specific зависимости Cargo. Адаптеры зависят от ash и ash-window; обратной зависимости от renderer нет.
- `engine-core`, `engine-audio`, `engine-physics`, `engine-network`, `engine-neural` и `engine-editor` не имеют внешних зависимостей.

Источники: `Cargo.toml`, `engine/engine-core/Cargo.toml`, `engine/engine-project/Cargo.toml`, `engine/engine-window/Cargo.toml`, `engine/engine-platform/Cargo.toml`, `engine/engine-platform-windows/Cargo.toml`, `engine/engine-platform-macos/Cargo.toml`, `engine/engine-render/Cargo.toml`, `engine/engine-files/Cargo.toml`, `engine/engine-pak/Cargo.toml`, `engine/engine-resources/Cargo.toml`, `engine/engine-scene/Cargo.toml`, `engine/engine-assets/Cargo.toml`, `engine/engine-physics/Cargo.toml`, `engine/engine-audio/Cargo.toml`, `engine/engine-network/Cargo.toml`, `engine/engine-neural/Cargo.toml`, `engine/engine-editor/Cargo.toml`, `engine/sandbox/Cargo.toml`.

## Файловые источники и RVEPAK

`engine-files::VirtualPath` задаёт case-sensitive UTF-8 namespace ресурсов, а `ReadAt` и `FileSource` читают абсолютные `u64`-диапазоны без общего archive lock. `engine-pak::Archive` проверяет header/index, открывает range reads по пересекающимся чанкам и проверяет chunk/full-blob BLAKE3. `ArchiveCatalog` проверяет уникальность полного virtual path и связывает external references только с local blob указанного владельца. `ArchiveWriter` записывает каждый локальный blob потоково и возвращает ArchiveId и BlobId/hash map для следующих архивов. Точная публичная раскладка v1 описана в [`docs/rvepak-format.md`](../../docs/rvepak-format.md).

`engine-pak::preflight` раскрывает и проверяет все inputs manifest до BLAKE3 prepass и записи, а `execute` строит весь набор в staging directory, проверяет его через `ArchiveCatalog` и публикует с атомарным запретом замены существующего output. `tools/rvepak` предоставляет команды `build`, `list` и `verify`; только `verify` повторно декодирует все unique local blobs. Manifest и политика dedup описаны в [[Engine/EnginePak]].

Чтение ограничивается configurable index, result, chunk и memory budgets. `ReadControl` резервирует footprint результата, stored/decoded чанка и codec workspace до аллокаций и позволяет отменить чтение между чанками. На Windows `seek_read` меняет file cursor, поэтому file-backed payload reads и worker reopenings используют независимые handles с проверкой ArchiveId; на Unix исходный positional handle остаётся закреплён за Archive.

Каталог обнаруживает верхнеуровневые `.rvepak` файлы без loose-file fallback. `engine-resources` выбирает packed-only режим при наличии архивов и loose режим без них, ограничивает очередь и memory footprint и выполняет чтение в background workers. Builder принимает любые opaque bytes и размещает архивы верхнего уровня; sandbox использует `assets` как root по умолчанию.

## Основные потоки

### Запуск и цикл окна

`engine/sandbox/src/main.rs` разбирает аргументы и загружает `BootstrapConfig` через
`engine_project::BootstrapConfig::load` из `engine/engine-project/src/bootstrap.rs`.
Конфигурация принимает `WindowSizeX`, `WindowSizeY`, `WindowTitle`,
`AllowMultipleInstances` и `AppIcon`. Sandbox применяет размеры и заголовок; из новых
полей он применяет только `WindowTitle`, а `AllowMultipleInstances` и `AppIcon`
разбирает и игнорирует. Размеры окна по умолчанию — 1280×720. После этих проверок
`ResourceSystem::start` открывает `assets` (или проверяет его архивный каталог), затем
`Assets::with_resources` загружает CPU-содержимое кадра до создания окна. Это происходит
и для процедурных сцен; отсутствующий loose root допустим, а ошибка архива или ресурса
завершает запуск с кодом 2. FBX разбирается через `ufbx::load_memory` на вызывающем
startup-потоке, внешние текстуры читаются через тот же handle, GPU upload выполняется
позже при renderer initialization.
`engine-window` создаёт event loop и окно в `resumed`, после чего вызывает `WindowHandler::init`. В каждом `about_to_wait` вызывается `update`, а затем запрашивается перерисовка; `frame` вызывается при `RedrawRequested`. Изменения размеров передаются в физических пикселях через `resized`.

При выходе `WindowHandler::exiting` вызывается до освобождения окна. Sandbox удаляет renderer в этом callback; после возврата из event loop сначала уничтожается `Sandbox`, затем `ResourceSystem` joins coordinator и workers.

Источники: `engine/engine-project/src/bootstrap.rs`, `engine/engine-project/src/project_settings.rs`, `engine/sandbox/src/main.rs`, `engine/engine-window/src/lib.rs`.

### Vulkan-кадр

`Renderer::new` загружает Vulkan loader и создаёт surface через `engine-platform`; объединяет требования платформы с расширениями surface/swapchain и конфигурации. Общий renderer создаёт instance с флагами адаптера и выбирает физическое устройство с API не ниже 1.3, поддержкой dynamic rendering и synchronization2, общей graphics/present queue family и подходящими форматами. Дискретное устройство получает приоритет. При нулевом начальном размере создание swapchain и per-frame offscreen-targets откладывается; общая shadow map создаётся при инициализации renderer.

При инициализации sandbox загружает текстуры содержимого через `Renderer::create_texture` (staging-копия с ожиданием fence) и его меши через `Renderer::create_mesh`; у модели FBX по мешу на каждый материал. `Sandbox::frame` использует одно значение elapsed для движения объекта, камеры и солнца и передаёт в `Frame` матрицы камеры и света, освещение, меняющийся clear color и `DrawCommand` для объекта и окружения. Матрицы рассчитывают `engine-scene` и glam, renderer получает их в виде массивов `f32`. Renderer сначала записывает depth-only shadow pass в общую карту глубины, затем очищает цвет и глубину offscreen-target и рисует сцену, где mesh shader читает тень. После scene pass renderer линейно blit-ит изображение в приобретённый swapchain image, при непустом `Frame::overlay` рисует поверх него 2D-треугольники оверлея в разрешении окна и вызывает present. Время shadow и scene pass измеряется timestamp queries и доступно через `Renderer::take_gpu_frame_time`; замер может немного занижать время, если начало shadow pass перекрывается с хвостом предыдущего кадра. При resize swapchain и per-frame targets пересоздаются; нулевой размер пропускает кадр. `render_scale` задаёт отдельное render resolution; размеры рассчитываются через floor и ограничиваются минимумом 1×1.

Источники: `engine/engine-render/src/lib.rs`, `engine/engine-platform-windows/src/lib.rs`, `engine/engine-platform-macos/src/lib.rs`, `engine/sandbox/src/main.rs`.

## Ограничения и инварианты

- `.github/workflows/ci.yml` при push и для PR проверяет изменённые крейты и всех их транзитивных потребителей на Windows и macOS: форматирование, Clippy, сборку и unit-/integration-/doc-тесты через `-p`. `.github/scripts/affected_packages.py` читает workspace и локальные зависимости всех видов и платформ из `cargo metadata --no-deps`; не связанные с изменениями крейты отдельно не проверяются. Для PR используется diff от merge-base до head, для push — `before..after`. Общие настройки, lockfile, CI и неизвестные пути, а также ручной запуск и push без доступного `before` проверяют весь workspace. Изменения только Markdown вне крейтов, vault Obsidian и `LICENSE` пропускают задания Rust. Тесты выбора выполняются на Ubuntu командой `python3 -B -m unittest discover -s .github/scripts -v`. Workflow создаёт `build/` и выполняет команды Cargo из него. Общая required-проверка `CI` в защите `main` требует успеха выбора крейтов и обеих платформ, если есть затронутые крейты; корректный пропуск Rust-заданий допускается только при пустом выборе. Тесты движка не создают окно и не загружают Vulkan; фактический графический путь проверяется отдельно через sandbox. Правило сопровождать новые фичи тестами закреплено в `AGENTS.md`.

- `engine-assets` собирает C-исходник ufbx через `cc`, поэтому сборке нужен C-компилятор целевой платформы: MSVC на Windows, clang на macOS. Кросс-проверка Windows с macOS (`--target x86_64-pc-windows-gnu`) требует MinGW или исключения `engine-assets` и `sandbox`.
- Целевые runtime — настольные Windows с Vulkan-драйвером GPU и macOS с Vulkan loader и MoltenVK. Остальные ОС не имеют платформенных адаптеров; мобильный lifecycle не реализован.
- Требуется Vulkan 1.3 и фактическая поддержка требуемых features, очереди, surface usage и форматов; наличие самого Vulkan loader недостаточно.
- Renderer рисует меши одним scene pipeline после depth-only shadow pass (отдельный overlay pipeline рисует 2D-треугольники оверлея): одна текстура на draw, умноженная на цвет вершины, направленный свет с ambient и PCF 3×3 для теней. Текущая реализация использует одну общую карту `D32_SFLOAT` размером 2048×2048, без specular, PBR, каскадов, mipmaps, staging-загрузки мешей и удаления мешей и текстур. HDR/тонмаппинг, DLSS/FSR и tessellation pipeline отсутствуют.
- Контракт `Frame::view_projection`: мировое пространство → clip space с осью Y вверх и глубиной `[0, 1]`; Y переворачивается отрицательной высотой viewport. `DrawCommand::model` переводит модель в мировое пространство.
- При отсутствии `VK_LAYER_KHRONOS_validation` debug renderer сообщает об этом и продолжает без слоя. Vulkan SDK для текущего пути не нужен; macOS требует отдельно установленного Vulkan loader и MoltenVK либо окружения SDK.
- Базовый teardown renderer вызывает `device_wait_idle`, но это не доказывает завершение всех операций presentation engine. Maintenance extensions для этого сейчас не включены.

## Решения и основания

- Отдельные платформенные крейты в одном workspace выбраны по требованию пользователя: изменения общего renderer и остальных модулей движка едины, зависимости от ОС локализованы в адаптерах.
- `engine-render` использует Vulkan 1.3, dynamic rendering и synchronization2. Это задано утверждёнными требованиями к минимальной Vulkan-версии.
- Render resolution отделено от output resolution: кадр рисуется в offscreen-target и передаётся в swapchain отдельным output pass. Это подтверждённая точка расширения для возможного апскейлера, а не готовая интеграция DLSS/FSR.
- Renderer принимает display/window handles через `raw-window-handle` и удерживает `Arc` окна, не вводя прямую зависимость от winit; такой контракт закреплён в утверждённых решениях.
- Расширения instance/device передаются через `RendererConfig` до создания соответствующих объектов; неподдерживаемые запросы приводят к ошибке. Эта возможность включена по утверждённому требованию о будущих интеграциях.
- Тесселяция включается как Vulkan feature только при поддержке выбранного устройства. Самих шейдеров и pipeline для тесселяции нет.
- Шейдеры пишутся на GLSL и компилируются в SPIR-V крейтом naga в `build.rs`; так выбрал пользователь, чтобы сборка локально и в CI Windows/macOS не требовала Vulkan SDK и `glslc`.
- Матрицы считаются крейтом glam по выбору пользователя. Публичный API renderer принимает матрицу как `[[f32; 4]; 4]` и не зависит от glam.
- Ресурсы лежат в `assets` рабочего каталога (`build/assets`) и адресуются путями вида `/model/cube.fbx` — так попросил пользователь. Поддерживаемые форматы — FBX для моделей и PNG для текстур, тоже по запросу пользователя.
- `engine-assets` возвращает данные CPU в собственных типах и не зависит от renderer; перевод в `engine_render::Vertex` выполняет приложение.
