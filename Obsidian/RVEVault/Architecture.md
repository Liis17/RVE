# Architecture

Parent: [[Index]]

## Назначение и границы

RVE — workspace игрового движка на Rust. Его рабочий путь создаёт настольное окно, выбирает совместимое Vulkan-устройство, очищает offscreen-изображение и переносит его в swapchain. Полноценные сцены, ресурсы, физика, звук и редактор пока не реализованы.

## Стек

| Технология | Роль | Источник |
|---|---|---|
| Rust 2024, Cargo workspace с resolver 3 | Сборка библиотечных крейтов и приложения | `Cargo.toml` |
| winit 0.30.13 | Окно и цикл событий | `engine/engine-window/Cargo.toml` |
| ash 0.38.0 | Vulkan bindings и runtime-загрузка loader | `engine/engine-render/Cargo.toml`, `engine/engine-render/src/lib.rs` |
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
- [[Engine/Sandbox]] — приложение, соединяющее core, window и render.

Пять остальных крейтов — документированные скелеты: `engine-scene`, `engine-assets`, `engine-physics`, `engine-audio`, `engine-editor`. В `engine/engine-scene/src/lib.rs`, `engine/engine-assets/src/lib.rs`, `engine/engine-physics/src/lib.rs`, `engine/engine-audio/src/lib.rs` и `engine/engine-editor/src/lib.rs` находятся только комментарии назначения; типов, модулей и зависимостей пока нет.

## Направления зависимостей

- `sandbox` зависит от `engine-core`, `engine-window`, `engine-render`.
- `engine-window` зависит от winit.
- `engine-render` зависит от ash, engine-platform и raw-window-handle, но не от winit.
- `engine-platform` выбирает `engine-platform-windows` или `engine-platform-macos` через target-specific зависимости Cargo. Адаптеры зависят от ash и ash-window; обратной зависимости от renderer нет.
- `engine-core` и пять крейтов-скелетов не имеют внешних зависимостей.

Источники: `Cargo.toml`, `engine/engine-core/Cargo.toml`, `engine/engine-window/Cargo.toml`, `engine/engine-platform/Cargo.toml`, `engine/engine-platform-windows/Cargo.toml`, `engine/engine-platform-macos/Cargo.toml`, `engine/engine-render/Cargo.toml`, `engine/engine-scene/Cargo.toml`, `engine/engine-assets/Cargo.toml`, `engine/engine-physics/Cargo.toml`, `engine/engine-audio/Cargo.toml`, `engine/engine-editor/Cargo.toml`, `engine/sandbox/Cargo.toml`.

## Основные потоки

### Запуск и цикл окна

`engine/sandbox/src/main.rs` разбирает аргументы, загружает начальный размер окна
через `bootstrap::load` из `bootstrap.rves` в рабочем каталоге, создаёт `Sandbox` и
вызывает `engine_window::run` с полученным `WindowConfig`. Параметры `WindowSizeX`
и `WindowSizeY` задают физический размер; отсутствующие параметры или файл
сохраняют defaults 1280×720, ошибочные настройки завершают запуск с кодом 2.
`engine-window` создаёт event loop и окно в `resumed`, после чего вызывает `WindowHandler::init`. В каждом `about_to_wait` вызывается `update`, а затем запрашивается перерисовка; `frame` вызывается при `RedrawRequested`. Изменения размеров передаются в физических пикселях через `resized`.

При выходе `WindowHandler::exiting` вызывается до освобождения окна. Sandbox удаляет renderer в этом callback; renderer удерживает `Arc` окна до уничтожения Vulkan surface.

Источники: `engine/engine-window/src/lib.rs`, `engine/sandbox/src/main.rs`, `engine/sandbox/src/bootstrap.rs`.

### Vulkan-кадр

`Renderer::new` загружает Vulkan loader и создаёт surface через `engine-platform`; объединяет требования платформы с расширениями surface/swapchain и конфигурации. Общий renderer создаёт instance с флагами адаптера и выбирает физическое устройство с API не ниже 1.3, поддержкой dynamic rendering и synchronization2, общей graphics/present queue family и подходящими форматами. Дискретное устройство получает приоритет. При нулевом начальном размере создание swapchain и offscreen-изображений откладывается.

`Sandbox::frame` тикает `Time` и передаёт меняющийся clear color в `Renderer::render_frame`. Renderer очищает offscreen-target через dynamic rendering, затем линейно blit-ит изображение в приобретённый swapchain image и вызывает present. При resize swapchain и per-frame targets пересоздаются; нулевой размер пропускает кадр. `render_scale` задаёт отдельное render resolution; размеры рассчитываются через floor и ограничиваются минимумом 1×1.

Источники: `engine/engine-render/src/lib.rs`, `engine/engine-platform-windows/src/lib.rs`, `engine/engine-platform-macos/src/lib.rs`, `engine/sandbox/src/main.rs`.

## Ограничения и инварианты

- `.github/workflows/ci.yml` проверяет весь workspace на Windows и macOS при push в любую ветку и для PR: форматирование, Clippy, сборку и unit-/integration-/doc-тесты. Workflow создаёт `build/` и выполняет проверки из него. Общая проверка `CI` требует успеха обеих платформ; для запрета слияния при ошибках она используется как required status check в защите `main` на GitHub. Тесты не создают окно и не загружают Vulkan; фактический графический путь проверяется отдельно через sandbox. Правило сопровождать новые фичи тестами закреплено в `AGENTS.md`.

- Целевые runtime — настольные Windows с Vulkan-драйвером GPU и macOS с Vulkan loader и MoltenVK. Остальные ОС не имеют платформенных адаптеров; мобильный lifecycle не реализован.
- Требуется Vulkan 1.3 и фактическая поддержка требуемых features, очереди, surface usage и форматов; наличие самого Vulkan loader недостаточно.
- Сейчас выполняется только clear-and-blit путь. Геометрия, шейдеры, HDR/тонмаппинг, DLSS/FSR и tessellation pipeline отсутствуют.
- При отсутствии `VK_LAYER_KHRONOS_validation` debug renderer сообщает об этом и продолжает без слоя. Vulkan SDK для текущего пути не нужен; macOS требует отдельно установленного Vulkan loader и MoltenVK либо окружения SDK.
- Базовый teardown renderer вызывает `device_wait_idle`, но это не доказывает завершение всех операций presentation engine. Maintenance extensions для этого сейчас не включены.

## Решения и основания

- Отдельные платформенные крейты в одном workspace выбраны по требованию пользователя: изменения общего renderer и остальных модулей движка едины, зависимости от ОС локализованы в адаптерах.
- `engine-render` использует Vulkan 1.3, dynamic rendering и synchronization2. Это задано утверждёнными требованиями к минимальной Vulkan-версии.
- Render resolution отделено от output resolution: кадр рисуется в offscreen-target и передаётся в swapchain отдельным output pass. Это подтверждённая точка расширения для возможного апскейлера, а не готовая интеграция DLSS/FSR.
- Renderer принимает display/window handles через `raw-window-handle` и удерживает `Arc` окна, не вводя прямую зависимость от winit; такой контракт закреплён в утверждённых решениях.
- Расширения instance/device передаются через `RendererConfig` до создания соответствующих объектов; неподдерживаемые запросы приводят к ошибке. Эта возможность включена по утверждённому требованию о будущих интеграциях.
- Тесселяция включается как Vulkan feature только при поддержке выбранного устройства. Самих шейдеров и pipeline для тесселяции нет.
