# RVE

RVE — начальный каркас игрового движка на Rust с выводом через Vulkan. Сейчас рабочий путь включает счётчик времени, окно на winit, рисование цветных треугольных мешей с depth buffer в offscreen-изображение и его вывод в swapchain. `engine/sandbox` служит примером запуска и проверки этого пути: показывает вращающийся куб или треугольник.

## Крейт-пакеты

| Крейт | Назначение | Состояние |
|---|---|---|
| `engine-core` | Время игрового цикла (`Time`) | Работает |
| `engine-window` | Окно и цикл событий на winit | Работает |
| `engine-platform` | Выбор платформенного адаптера по целевой ОС | Работает |
| `engine-platform-windows` | Windows: загрузка Vulkan и оконная поверхность | Работает |
| `engine-platform-macos` | macOS: загрузка Vulkan, MoltenVK portability и Metal surface | Работает |
| `engine-render` | Vulkan renderer: меши в offscreen-кадре и blit в swapchain | Работает |
| `engine-scene` | Игровые объекты, компоненты и сцены | Скелет |
| `engine-assets` | Загрузка и хранение текстур, моделей и ресурсов | Скелет |
| `engine-physics` | Столкновения и физическая симуляция | Скелет |
| `engine-audio` | Воспроизведение и управление игровым звуком | Скелет |
| `engine-editor` | Графический редактор сцен и ресурсов | Скелет |
| `sandbox` | Оконное приложение для проверки движка | Работает |

## Требования

- Rust stable с поддержкой edition 2024.
- Целевые среды — настольные Windows и macOS. На Windows используется Vulkan-драйвер GPU, на macOS — MoltenVK поверх Metal.
- Драйвер и Vulkan runtime должны предоставлять Vulkan 1.3, `dynamicRendering`, `synchronization2`, `VK_KHR_swapchain`, graphics- и present-очередь, а также требуемые surface-форматы, depth-формат `D32_SFLOAT` и операции blit. Renderer проверяет эти условия при выборе устройства.
- Vulkan SDK для сборки и текущего пути запуска не требуется: Vulkan loader загружается во время выполнения, а GLSL-шейдеры из `engine/engine-render/shaders/` компилируются в SPIR-V крейтом naga в `build.rs` при сборке. SDK может предоставить validation layer для debug-сборок; в среде выполненных проверок `VK_LAYER_KHRONOS_validation` отсутствовал, поэтому runtime validation не запускалась.

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

### Автоматические тесты и PR

Каждая новая фича сопровождается тестами в том же коммите; исправление бага — регрессионным тестом. Общая команда `cargo test --workspace --locked` запускает unit-, integration- и doc-тесты всех крейтов. В крейтах-скелетах пока нет поведения для тестирования.

Тесты проверяют время игрового цикла, завершение оконного приложения и ошибки, настройки renderer, расширения Vulkan и параметры swapchain, проверку мешей, раскладку вершин и встроенный SPIR-V, геометрию и проекции сцен sandbox, требования платформенных адаптеров, CLI и обработку resize в sandbox. Интеграционный тест `engine/sandbox/tests/cli.rs` запускает бинарник с неверными аргументами и проверяет код выхода 2 и диагностику. Автотестам не нужны окно, GPU или Vulkan runtime; они не проверяют фактическую отрисовку и обработку событий ОС.

Workflow [Rust CI](.github/workflows/ci.yml) запускается при push в любую ветку, для PR и вручную. Скрипт `.github/scripts/affected_packages.py` читает граф workspace из `cargo metadata --no-deps` и выбирает изменённые крейты вместе со всеми транзитивно зависящими от них крейтами. Например, изменение `engine-core` проверяет `engine-core` и `sandbox`, а изменение независимого `engine-audio` — только `engine-audio`. Зависимости для компиляции выбранных крейтов Cargo собирает автоматически.

Для PR сравниваются merge-base базовой ветки и head PR; для push — весь диапазон `before..after`. Общие настройки, `Cargo.lock`, CI и неизвестные пути запускают проверку всех крейтов. Ручной запуск, первый push новой ветки и недоступный `before` также проверяют весь workspace. Изменения только Markdown-документации вне крейтов, vault Obsidian и `LICENSE` пропускают проверки Rust.

На `windows-latest` и `macos-latest` создаётся `build/`, затем из него выполняются проверки с `-p` для каждого выбранного крейта (пример для `engine-core`):

```sh
cargo fmt -p engine-core -p sandbox --check
cargo clippy -p engine-core -p sandbox --all-targets --locked -- -D warnings
cargo build -p engine-core -p sandbox --locked
cargo test -p engine-core -p sandbox --locked
```

Тесты выбора крейтов выполняются в CI на Ubuntu; локальная команда: `python3 -B -m unittest discover -s .github/scripts -v` из корня репозитория.

Общая проверка `CI` требует успешного выбора крейтов и успеха обеих платформ, если выбраны крейты. При отсутствии затронутых крейтов она допускает пропуск платформенных заданий. Ошибка выбора крейтов, ошибка или отмена нужных проверок блокирует `CI`. Для `main` она должна быть обязательной в GitHub branch protection с требованием актуальности ветки относительно `main`; один workflow без защиты ветки не запрещает слияние. Порядок работы: коммит с кодом и тестами в рабочей ветке → push → PR → успешная `CI` → слияние.

Запуск sandbox из `build/`:

```powershell
cargo run -p sandbox
cargo run -p sandbox -- --frames 120
cargo run -p sandbox -- --frames 120 --render-scale 0.5
cargo run -p sandbox -- --frames 300 --resize-test
cargo run -p sandbox -- --scene triangle
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

`--frames N` завершает приложение после N успешных вызовов презентации; N — положительный `u64`. `--render-scale S` задаёт масштаб offscreen-разрешения и принимает конечное число из `(0, 1]`. `--resize-test` после 30 презентаций запрашивает 960×540, после 60 сворачивает окно на 250 мс и затем запрашивает восстановление 1280×720. `--scene triangle|cube` выбирает демонстрационную сцену; по умолчанию — `cube`. При совместном использовании `--resize-test` и `--frames` требуется N ≥ 120. Ошибка аргументов возвращает код 2, ошибка окна или рендерера — код 1, успешное завершение — код 0. Без `--frames` приложение работает до закрытия окна.

Команда `cargo tree -p engine-render` позволяет проверить, что `engine-render` не зависит от winit.

Проверка Windows-кода с macOS без линковки Windows-бинарника:

```sh
rustup target add x86_64-pc-windows-gnu
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
```

Эта проверка не заменяет сборку и запуск на Windows.

## Границы текущего renderer

Рендерер выбирает устройство с Vulkan 1.3, dynamic rendering и synchronization2, создаёт offscreen-таргет в `R16G16B16A16_SFLOAT` с depth buffer `D32_SFLOAT`, очищает его, рисует меши и линейно масштабирует результат в поддерживаемый surface-формат. `RendererConfig::render_scale` разделяет render resolution и output resolution. Меши создаются через `Renderer::create_mesh` из вершин с позицией и цветом и индексов треугольников; `render_frame` принимает список `DrawCommand` с матрицей в clip space (Y вверх, глубина `[0, 1]`). Есть один pipeline с вершинными цветами, без текстур, освещения и отсечения граней. Буферы мешей лежат в host-visible памяти без staging-загрузки, удаления мешей нет. В коде пока нет HDR-тонмаппинга, интеграции DLSS/FSR или tessellation pipeline. Поддержка tessellationShader сообщается через capabilities и включается при наличии у устройства; это не реализует тесселяцию.

Renderer проверяет наличие validation layer только в debug-сборке и продолжает работу без него, если слой не найден. Smoke-проверки не заменяют validation layers. При teardown вызывается `device_wait_idle`, но это ожидание само по себе не доказывает завершение всех операций presentation engine. Maintenance extensions для дополнительной синхронизации presentation resources сейчас не включены.

## Документация

- [Правила работы и оркестрация](AGENTS.md)
- [База знаний Obsidian](Obsidian/RVEVault/Index.md)
