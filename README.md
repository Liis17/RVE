# RVE

RVE — начальный каркас игрового движка на Rust с выводом через Vulkan. Сейчас рабочий путь включает счётчик времени, окно на winit, очистку offscreen-изображения и его вывод в swapchain. `engine/sandbox` служит примером запуска и проверки этого пути.

## Крейт-пакеты

| Крейт | Назначение | Состояние |
|---|---|---|
| `engine-core` | Время игрового цикла (`Time`) | Работает |
| `engine-window` | Окно и цикл событий на winit | Работает |
| `engine-platform` | Выбор платформенного адаптера по целевой ОС | Работает |
| `engine-platform-windows` | Windows: загрузка Vulkan и оконная поверхность | Работает |
| `engine-platform-macos` | macOS: загрузка Vulkan и Metal surface | Интеграция без настроек MoltenVK portability |
| `engine-render` | Vulkan renderer: clear offscreen-кадра и blit в swapchain | Работает |
| `engine-scene` | Игровые объекты, компоненты и сцены | Скелет |
| `engine-assets` | Загрузка и хранение текстур, моделей и ресурсов | Скелет |
| `engine-physics` | Столкновения и физическая симуляция | Скелет |
| `engine-audio` | Воспроизведение и управление игровым звуком | Скелет |
| `engine-editor` | Графический редактор сцен и ресурсов | Скелет |
| `sandbox` | Оконное приложение для проверки движка | Работает |

## Требования

- Rust stable с поддержкой edition 2024.
- На текущем этапе целевая среда — настольная Windows.
- Драйвер и Vulkan runtime должны предоставлять Vulkan 1.3, `dynamicRendering`, `synchronization2`, `VK_KHR_swapchain`, graphics- и present-очередь, а также требуемые surface-форматы и операции blit. Renderer проверяет эти условия при выборе устройства.
- Vulkan SDK для сборки и текущего пути запуска не требуется: Vulkan loader загружается во время выполнения, а шейдеры пока не используются. SDK может предоставить validation layer для debug-сборок; в среде выполненных проверок `VK_LAYER_KHRONOS_validation` отсутствовал, поэтому runtime validation не запускалась.

## Сборка и проверки

Из корня репозитория:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
cargo build -p sandbox
```

Запуск sandbox:

```powershell
cargo run -p sandbox
cargo run -p sandbox -- --frames 120
cargo run -p sandbox -- --frames 120 --render-scale 0.5
cargo run -p sandbox -- --frames 300 --resize-test
```

`--frames N` завершает приложение после N успешных вызовов презентации; N — положительный `u64`. `--render-scale S` задаёт масштаб offscreen-разрешения и принимает конечное число из `(0, 1]`. `--resize-test` после 30 презентаций запрашивает 960×540, после 60 сворачивает окно на 250 мс и затем запрашивает восстановление 1280×720. При совместном использовании `--resize-test` и `--frames` требуется N ≥ 120. Ошибка аргументов возвращает код 2, ошибка окна или рендерера — код 1, успешное завершение — код 0. Без `--frames` приложение работает до закрытия окна.

Команда `cargo tree -p engine-render` позволяет проверить, что `engine-render` не зависит от winit.

## Границы текущего renderer

Рендерер выбирает устройство с Vulkan 1.3, dynamic rendering и synchronization2, создаёт offscreen-таргет в `R16G16B16A16_SFLOAT`, очищает его и линейно масштабирует в поддерживаемый surface-формат. `RendererConfig::render_scale` разделяет render resolution и output resolution. В коде пока нет геометрии, шейдеров, HDR-тонмаппинга, интеграции DLSS/FSR или tessellation pipeline. Поддержка tessellationShader сообщается через capabilities и включается при наличии у устройства; это не реализует тесселяцию.

Renderer проверяет наличие validation layer только в debug-сборке и продолжает работу без него, если слой не найден. Smoke-проверки не заменяют validation layers. При teardown вызывается `device_wait_idle`, но это ожидание само по себе не доказывает завершение всех операций presentation engine. Maintenance extensions для дополнительной синхронизации presentation resources сейчас не включены.

## Документация

- [Правила работы и оркестрация](AGENTS.md)
- [База знаний Obsidian](Obsidian/RVEVault/Index.md)
