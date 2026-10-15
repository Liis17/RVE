# RVE

RVE — начальный каркас игрового движка на Rust с выводом через Vulkan. Сейчас рабочий путь включает счётчик времени, окно на winit, очистку offscreen-изображения и его вывод в swapchain. `engine/sandbox` служит примером запуска и проверки этого пути.

## Крейт-пакеты

| Крейт | Назначение | Состояние |
|---|---|---|
| `engine-core` | Время игрового цикла (`Time`) | Работает |
| `engine-window` | Окно и цикл событий на winit | Работает |
| `engine-platform` | Выбор платформенного адаптера по целевой ОС | Работает |
| `engine-platform-windows` | Windows: загрузка Vulkan и оконная поверхность | Работает |
| `engine-platform-macos` | macOS: загрузка Vulkan, MoltenVK portability и Metal surface | Работает |
| `engine-render` | Vulkan renderer: clear offscreen-кадра и blit в swapchain | Работает |
| `engine-scene` | Игровые объекты, компоненты и сцены | Скелет |
| `engine-assets` | Загрузка и хранение текстур, моделей и ресурсов | Скелет |
| `engine-physics` | Столкновения и физическая симуляция | Скелет |
| `engine-audio` | Воспроизведение и управление игровым звуком | Скелет |
| `engine-editor` | Графический редактор сцен и ресурсов | Скелет |
| `sandbox` | Оконное приложение для проверки движка | Работает |

## Требования

- Rust stable с поддержкой edition 2024.
- Целевые среды — настольные Windows и macOS. На Windows используется Vulkan-драйвер GPU, на macOS — MoltenVK поверх Metal.
- Драйвер и Vulkan runtime должны предоставлять Vulkan 1.3, `dynamicRendering`, `synchronization2`, `VK_KHR_swapchain`, graphics- и present-очередь, а также требуемые surface-форматы и операции blit. Renderer проверяет эти условия при выборе устройства.
- Vulkan SDK для сборки и текущего пути запуска не требуется: Vulkan loader загружается во время выполнения, а шейдеры пока не используются. SDK может предоставить validation layer для debug-сборок; в среде выполненных проверок `VK_LAYER_KHRONOS_validation` отсутствовал, поэтому runtime validation не запускалась.

### Windows

Установите Rust и актуальный драйвер GPU с поддержкой Vulkan 1.3. Платформенный адаптер загружает системный `vulkan-1.dll`.

### macOS

Установите Rust и Vulkan runtime с актуальным MoltenVK. Для Homebrew:

```sh
brew install molten-vk vulkan-loader
```

Адаптер сначала ищет `libvulkan.dylib` через системный загрузчик, затем пробует `/opt/homebrew/lib/libvulkan.dylib` для Homebrew на Apple Silicon. Альтернатива — Vulkan SDK с настроенным окружением по [инструкции MoltenVK](https://github.com/KhronosGroup/MoltenVK/blob/main/README.md). Установка одной библиотеки MoltenVK без Vulkan loader для этого пути недостаточна.

Адаптер macOS включает `VK_KHR_portability_enumeration`, флаг `ENUMERATE_PORTABILITY_KHR` и `VK_KHR_portability_subset`. Требования общего renderer к Vulkan 1.3, features и форматам сохраняются; старый MoltenVK или неподходящий GPU могут не пройти проверку устройства.

## Платформенная структура

`engine-render` использует общий интерфейс `engine-platform`; Cargo выбирает адаптер по целевой ОС. Загрузка Vulkan, интеграция поверхности окна и требования платформы находятся в `engine-platform-windows` и `engine-platform-macos`. Общая отрисовка остаётся в `engine-render`, оконный цикл на winit — в `engine-window`, приложение — в `sandbox`. Адаптеры не зависят от renderer. Остальные ОС пока не имеют адаптеров.

## Сборка и проверки

Артефакты Cargo сохраняются в `build/` в корне репозитория согласно
`.cargo/config.toml`. Каталог исключён из Git. Сборка, проверки и запуск выполняются
из `build/`. При первой подготовке создай каталог и перейди в него:

```sh
mkdir -p build
cd build
```

В PowerShell для первого создания каталога используй `New-Item -ItemType Directory -Force build`, затем `Set-Location build`.

Из `build/`:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
cargo build -p sandbox
```

Запуск sandbox из `build/`:

```powershell
cargo run -p sandbox
cargo run -p sandbox -- --frames 120
cargo run -p sandbox -- --frames 120 --render-scale 0.5
cargo run -p sandbox -- --frames 300 --resize-test
```

Для запуска собранного бинарника из того же рабочего каталога используй
`./debug/sandbox` на macOS или `.\debug\sandbox.exe` на Windows.

### Начальные настройки

Перед созданием окна sandbox читает `bootstrap.rves` из текущего рабочего
каталога. При принятом способе запуска это `build/bootstrap.rves`, независимо
от расположения бинарника в `build/debug/` или `build/release/`.

Для первой настройки скопируй шаблон из корня репозитория. Из `build/` на macOS:

```sh
cp ../bootstrap.rves.example bootstrap.rves
```

В PowerShell: `Copy-Item ../bootstrap.rves.example bootstrap.rves`.
Повторное копирование заменяет локальные настройки; для дальнейших изменений
редактируй сам `build/bootstrap.rves`:

```text
WindowSizeX=1280
WindowSizeY=720
```

Одна строка задаёт один параметр; имена чувствительны к регистру. Размеры
клиентской области окна задаются в физических пикселях положительными целыми
числами `u32`. Пробелы вокруг имени и значения и пустые строки допускаются.
Если файла или отдельного параметра нет, используется соответствующий размер
по умолчанию: 1280 по X, 720 по Y. Некорректные значения, неизвестные или повторные
параметры, строки без `=` и ошибки чтения файла останавливают запуск с кодом 2;
ошибка разбора содержит имя файла и номер строки.

Размер читается при старте; изменение файла применяется при следующем запуске.
Файл в `build/` не попадает в Git; шаблон `bootstrap.rves.example` хранится в репозитории.

`--frames N` завершает приложение после N успешных вызовов презентации; N — положительный `u64`. `--render-scale S` задаёт масштаб offscreen-разрешения и принимает конечное число из `(0, 1]`. `--resize-test` после 30 презентаций запрашивает 960×540, после 60 сворачивает окно на 250 мс и затем запрашивает восстановление 1280×720. При совместном использовании `--resize-test` и `--frames` требуется N ≥ 120. Ошибка аргументов возвращает код 2, ошибка окна или рендерера — код 1, успешное завершение — код 0. Без `--frames` приложение работает до закрытия окна.

Команда `cargo tree -p engine-render` позволяет проверить, что `engine-render` не зависит от winit.

Проверка Windows-кода с macOS без линковки Windows-бинарника:

```sh
rustup target add x86_64-pc-windows-gnu
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
```

Эта проверка не заменяет сборку и запуск на Windows.

## Границы текущего renderer

Рендерер выбирает устройство с Vulkan 1.3, dynamic rendering и synchronization2, создаёт offscreen-таргет в `R16G16B16A16_SFLOAT`, очищает его и линейно масштабирует в поддерживаемый surface-формат. `RendererConfig::render_scale` разделяет render resolution и output resolution. В коде пока нет геометрии, шейдеров, HDR-тонмаппинга, интеграции DLSS/FSR или tessellation pipeline. Поддержка tessellationShader сообщается через capabilities и включается при наличии у устройства; это не реализует тесселяцию.

Renderer проверяет наличие validation layer только в debug-сборке и продолжает работу без него, если слой не найден. Smoke-проверки не заменяют validation layers. При teardown вызывается `device_wait_idle`, но это ожидание само по себе не доказывает завершение всех операций presentation engine. Maintenance extensions для дополнительной синхронизации presentation resources сейчас не включены.

## Документация

- [Правила работы и оркестрация](AGENTS.md)
- [База знаний Obsidian](Obsidian/RVEVault/Index.md)
