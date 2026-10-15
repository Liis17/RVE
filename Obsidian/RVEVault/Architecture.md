# Architecture

Parent: [[Index]]

## Назначение и границы

RVE — workspace игрового движка на Rust. Его рабочий путь создаёт настольное окно, выбирает совместимое Vulkan-устройство, рисует треугольные меши с текстурой, направленным светом и depth buffer в offscreen-изображение и переносит его в swapchain. Модели FBX и текстуры PNG загружаются из каталога `assets`. Полноценные сцены, физика, звук и редактор пока не реализованы.

## Стек

| Технология | Роль | Источник |
|---|---|---|
| Rust 2024, Cargo workspace с resolver 3 | Сборка библиотечных крейтов и приложения | `Cargo.toml` |
| winit 0.30.13 | Окно и цикл событий | `engine/engine-window/Cargo.toml` |
| ash 0.38.0 | Vulkan bindings и runtime-загрузка loader | `engine/engine-render/Cargo.toml`, `engine/engine-render/src/lib.rs` |
| naga 30.0.1 | Компиляция GLSL-шейдеров в SPIR-V при сборке, без Vulkan SDK | `engine/engine-render/build.rs` |
| glam 0.34.1 | Матрицы трансформации и перспективы в sandbox | `engine/sandbox/src/scene.rs`, `engine/sandbox/src/content.rs` |
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
- [[Engine/EngineAssets]] — пути ресурсов и загрузка FBX/PNG в память CPU, без зависимости от renderer.
- [[Engine/Sandbox]] — приложение, соединяющее core, window, render и assets.

Четыре остальных крейта — документированные скелеты: `engine-scene`, `engine-physics`, `engine-audio`, `engine-editor`. В `engine/engine-scene/src/lib.rs`, `engine/engine-physics/src/lib.rs`, `engine/engine-audio/src/lib.rs` и `engine/engine-editor/src/lib.rs` находятся только комментарии назначения; типов, модулей и зависимостей пока нет.

## Направления зависимостей

- `sandbox` зависит от `engine-core`, `engine-window`, `engine-render`, `engine-assets`, glam, sysinfo и font8x8.
- `engine-assets` зависит от ufbx и png; от renderer и окна не зависит.
- `engine-window` зависит от winit.
- `engine-render` зависит от ash, engine-platform и raw-window-handle, но не от winit и glam; naga — его build-зависимость.
- `engine-platform` выбирает `engine-platform-windows` или `engine-platform-macos` через target-specific зависимости Cargo. Адаптеры зависят от ash и ash-window; обратной зависимости от renderer нет.
- `engine-core` и четыре крейта-скелета не имеют внешних зависимостей.

Источники: `Cargo.toml`, `engine/engine-core/Cargo.toml`, `engine/engine-window/Cargo.toml`, `engine/engine-platform/Cargo.toml`, `engine/engine-platform-windows/Cargo.toml`, `engine/engine-platform-macos/Cargo.toml`, `engine/engine-render/Cargo.toml`, `engine/engine-scene/Cargo.toml`, `engine/engine-assets/Cargo.toml`, `engine/engine-physics/Cargo.toml`, `engine/engine-audio/Cargo.toml`, `engine/engine-editor/Cargo.toml`, `engine/sandbox/Cargo.toml`.

## Основные потоки

### Запуск и цикл окна

`engine/sandbox/src/main.rs` разбирает аргументы, загружает начальный размер окна
через `bootstrap::load` из `bootstrap.rves` в рабочем каталоге, создаёт `Sandbox` и
вызывает `engine_window::run` с полученным `WindowConfig`. Параметры `WindowSizeX`
и `WindowSizeY` задают физический размер; отсутствующие параметры или файл
сохраняют defaults 1280×720, ошибочные настройки завершают запуск с кодом 2.
Затем до создания окна загружается содержимое кадра (`load_content`): демонстрационная
сцена либо модель `--model` через `Assets::load_model` и, при `--texture`, заменяющая
текстура через `Assets::load_texture`. Ошибка ресурса также даёт код 2.
`engine-window` создаёт event loop и окно в `resumed`, после чего вызывает `WindowHandler::init`. В каждом `about_to_wait` вызывается `update`, а затем запрашивается перерисовка; `frame` вызывается при `RedrawRequested`. Изменения размеров передаются в физических пикселях через `resized`.

При выходе `WindowHandler::exiting` вызывается до освобождения окна. Sandbox удаляет renderer в этом callback; renderer удерживает `Arc` окна до уничтожения Vulkan surface.

Источники: `engine/engine-window/src/lib.rs`, `engine/sandbox/src/main.rs`, `engine/sandbox/src/bootstrap.rs`.

### Vulkan-кадр

`Renderer::new` загружает Vulkan loader и создаёт surface через `engine-platform`; объединяет требования платформы с расширениями surface/swapchain и конфигурации. Общий renderer создаёт instance с флагами адаптера и выбирает физическое устройство с API не ниже 1.3, поддержкой dynamic rendering и synchronization2, общей graphics/present queue family и подходящими форматами. Дискретное устройство получает приоритет. При нулевом начальном размере создание swapchain и offscreen-изображений откладывается.

При инициализации sandbox загружает текстуры содержимого через `Renderer::create_texture` (staging-копия с ожиданием fence) и его меши через `Renderer::create_mesh`; у модели FBX по мешу на каждый материал. `Sandbox::frame` тикает `Time` и передаёт в `Renderer::render_frame` `Frame` с меняющимся clear color, матрицей камеры, направленным светом и `DrawCommand` с матрицей модели; матрицы рассчитывает glam. Renderer записывает параметры света в uniform buffer текущего слота кадра, а `clip_from_model` и матрицу нормалей — в push constants. Renderer очищает цвет и глубину offscreen-target через dynamic rendering, рисует меши единственным graphics pipeline, затем линейно blit-ит изображение в приобретённый swapchain image, при непустом `Frame::overlay` рисует поверх него 2D-треугольники оверлея в разрешении окна и вызывает present. Время scene pass измеряется timestamp queries и доступно через `Renderer::take_gpu_frame_time`. При resize swapchain и per-frame targets пересоздаются; нулевой размер пропускает кадр. `render_scale` задаёт отдельное render resolution; размеры рассчитываются через floor и ограничиваются минимумом 1×1.

Источники: `engine/engine-render/src/lib.rs`, `engine/engine-platform-windows/src/lib.rs`, `engine/engine-platform-macos/src/lib.rs`, `engine/sandbox/src/main.rs`.

## Ограничения и инварианты

- `.github/workflows/ci.yml` при push и для PR проверяет изменённые крейты и всех их транзитивных потребителей на Windows и macOS: форматирование, Clippy, сборку и unit-/integration-/doc-тесты через `-p`. `.github/scripts/affected_packages.py` читает workspace и локальные зависимости всех видов и платформ из `cargo metadata --no-deps`; не связанные с изменениями крейты отдельно не проверяются. Для PR используется diff от merge-base до head, для push — `before..after`. Общие настройки, lockfile, CI и неизвестные пути, а также ручной запуск и push без доступного `before` проверяют весь workspace. Изменения только Markdown вне крейтов, vault Obsidian и `LICENSE` пропускают задания Rust. Тесты выбора выполняются на Ubuntu командой `python3 -B -m unittest discover -s .github/scripts -v`. Workflow создаёт `build/` и выполняет команды Cargo из него. Общая required-проверка `CI` в защите `main` требует успеха выбора крейтов и обеих платформ, если есть затронутые крейты; корректный пропуск Rust-заданий допускается только при пустом выборе. Тесты движка не создают окно и не загружают Vulkan; фактический графический путь проверяется отдельно через sandbox. Правило сопровождать новые фичи тестами закреплено в `AGENTS.md`.

- `engine-assets` собирает C-исходник ufbx через `cc`, поэтому сборке нужен C-компилятор целевой платформы: MSVC на Windows, clang на macOS. Кросс-проверка Windows с macOS (`--target x86_64-pc-windows-gnu`) требует MinGW или исключения `engine-assets` и `sandbox`.
- Целевые runtime — настольные Windows с Vulkan-драйвером GPU и macOS с Vulkan loader и MoltenVK. Остальные ОС не имеют платформенных адаптеров; мобильный lifecycle не реализован.
- Требуется Vulkan 1.3 и фактическая поддержка требуемых features, очереди, surface usage и форматов; наличие самого Vulkan loader недостаточно.
- Renderer рисует треугольные меши одним mesh pipeline (отдельный overlay pipeline рисует только нетекстурированные 2D-треугольники оверлея): одна текстура на draw, умноженная на цвет вершины, и один направленный свет с ambient. Mipmaps, specular, теней, PBR-материалов, staging-загрузки мешей и удаления мешей и текстур нет. HDR/тонмаппинг, DLSS/FSR и tessellation pipeline отсутствуют.
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
