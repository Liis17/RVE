# RVE

RVE — игровой движок на Rust с выводом через Vulkan. Рабочий путь включает счётчик времени, окно на winit, камеры, трансформации, направленный свет и отбрасывание теней в offscreen-изображение с выводом в swapchain. `engine/sandbox` служит примером запуска и проверки этого пути: по умолчанию показывает вращающийся куб над плоскостью с движущимися камерой и солнцем; доступны также треугольник, прежняя сцена куба и модели FBX из `build/assets`.

## Крейт-пакеты

| Крейт | Назначение | Состояние |
|---|---|---|
| `engine-core` | Время игрового цикла (`Time`) | Работает |
| `engine-window` | Окно и цикл событий на winit | Работает |
| `engine-platform` | Выбор платформенного адаптера по целевой ОС | Работает |
| `engine-platform-windows` | Windows: загрузка Vulkan и оконная поверхность | Работает |
| `engine-platform-macos` | macOS: загрузка Vulkan, MoltenVK portability и Metal surface | Работает |
| `engine-render` | Vulkan renderer: меши в offscreen-кадре и blit в swapchain | Работает |
| `engine-files` | Канонические virtual paths и позиционное чтение файлов | Работает |
| `engine-pak` | Чтение, запись и проверка каталогов RVEPAK v1 | Работает |
| `rvepak` | Сборка, просмотр и проверка наборов RVEPAK из TOML manifest | Работает |
| `engine-scene` | Игровые объекты, компоненты и сцены | Скелет |
| `engine-assets` | Загрузка моделей FBX и текстур PNG из каталога `assets` | Работает |
| `engine-physics` | Столкновения и физическая симуляция | Скелет |
| `engine-audio` | Воспроизведение и управление игровым звуком | Скелет |
| `engine-editor` | Графический редактор сцен и ресурсов | Скелет |
| `sandbox` | Оконное приложение для проверки движка | Работает |

`engine-files` и `engine-pak` предоставляют самостоятельную библиотеку виртуальных
путей и архивов с positional reads, потоковой записью, диапазонными чтениями,
межархивными ссылками и reusable manifest builder. Инструмент `rvepak` собирает
opaque файлы любого типа, но runtime-загрузка архивов пока не подключена к
`engine-assets` или sandbox. Wire contract описан в [RVEPAK v1 format](docs/rvepak-format.md).

### Сборка архивов ресурсов

`rvepak` принимает один TOML manifest: `root` и `output` задаются относительно
manifest-файла, `[[archives]]` задают упорядоченные `.rvepak` файлы, а
`[[archives.inputs]]` выбирают файлы или каталоги относительно `root`. Каталоги
обходятся рекурсивно; виртуальные пути остаются относительно `root`. Все форматы
и расширения являются opaque bytes. Поля compression — `codec` (`none`, `zstd`,
`lz4`), `chunk_size` и `zstd_level`; значения из `[defaults]` можно переопределить
для каждого input. Неизвестные поля, повторные virtual paths, symlinks, пустые
группы и небезопасные имена архивов отклоняются до записи.

Пример полной схемы: [`rvepak.example.toml`](rvepak.example.toml). Запускай команды
из `build/`; output directory должна ещё не существовать. Builder пишет все архивы
во временный соседний каталог, проверяет общий `ArchiveCatalog`, затем публикует
набор атомарно без замены существующего output.

```sh
cargo run -p rvepak -- build ../rvepak.example.toml
cargo run -p rvepak -- build ../rvepak.example.toml --no-dedup
cargo run -p rvepak -- list game-assets
cargo run -p rvepak -- verify game-assets
```

Deduplication включена по умолчанию и использует BLAKE3 плюс размер до сжатия:
первое в manifest вхождение владеет blob, повтор в том же архиве становится
alias, а в следующем архиве — прямой ссылкой на архив-владелец. Компрессионные
настройки повторов наследуются от владельца; builder сообщает об отличающихся
overrides. `--no-dedup` сохраняет каждый input самостоятельным локальным blob.
`list` показывает полный virtual path, размер, effective codec, владельца и
межархивную зависимость; `verify` потоково проверяет каждый уникальный local blob.
Успех возвращает код 0, ошибки аргументов, manifest или входных данных — 2,
операционные ошибки ввода-вывода и повреждённого архива — 1.

## Требования

- Rust stable с поддержкой edition 2024.
- Целевые среды — настольные Windows и macOS. На Windows используется Vulkan-драйвер GPU, на macOS — MoltenVK поверх Metal.
- Драйвер и Vulkan runtime должны предоставлять Vulkan 1.3, `dynamicRendering`, `synchronization2`, `VK_KHR_swapchain`, graphics- и present-очередь, а также требуемые surface-форматы, поддержку `D32_SFLOAT` как depth attachment и sampled image и операции blit. Renderer проверяет эти условия при выборе устройства.
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

Тесты проверяют время игрового цикла, завершение оконного приложения и ошибки, настройки renderer, расширения Vulkan и параметры swapchain, virtual paths, RVEPAK golden wire fixture, manifest preflight, opaque asset build, deduplication, aliases и external references, безопасную публикацию, CLI list/verify и коды выхода, codecs и диапазонные чтения, повреждённые индексы, память и cancellation, sparse offsets выше 4 GiB, декодирование PNG, загрузку FBX (триангуляция, нормали, UV, перевод осей и единиц, внешние и встроенные текстуры, ошибки), вписывание модели в кадр, проверку мешей и текстур, раскладку вершин, push constants и uniform-блока света, матрицу нормалей, встроенный SPIR-V, геометрию и проекции сцен sandbox, требования платформенных адаптеров и обработку resize в sandbox. Интеграционные тесты CLI запускают бинарники без окна или GPU. Автотестам не нужны окно, GPU или Vulkan runtime; они не проверяют фактическую отрисовку и обработку событий ОС.

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
cargo run -p sandbox -- --model /model/cube.fbx
cargo run -p sandbox -- --model /model/cube.fbx --texture /texture/cube.png
cargo run -p sandbox -- --debug-overlay
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

### Ресурсы: модели и текстуры

Ресурсы лежат в каталоге `assets` текущего рабочего каталога, при обычном запуске —
`build/assets`, и вместе с `build/` не попадают в Git. Путь ресурса записывается от
корня этого каталога через `/`: `/model/cube.fbx` означает `build/assets/model/cube.fbx`.
Компоненты `.` и `..`, пустые компоненты, `\` и `:` запрещены.

```text
build/assets/
├── model/cube.fbx
└── texture/cube.png
```

`--model /путь.fbx` загружает модель FBX (двоичную или ASCII) как объект выбранной
демонстрационной сцены; без `--scene` используется `shadows`. Модель масштабируется так,
что наибольшая сторона её bounding box равна 1, центрируется и получает движение и
камеру выбранной сцены `cube` или `shadows`. В сцене `shadows` под ней остаётся
неподвижная плоскость. Сочетание с `--scene triangle` недопустимо. Оси и единицы файла
приводятся к Y вверх и метрам, поэтому экспорт Blender по умолчанию подходит без настройки.
Текстура материала (Base Color
или Diffuse) должна быть PNG: встроенная в FBX либо файлом. Файл ищется по пути из FBX
относительно модели, затем по имени файла рядом с моделью. Например, при экспорте из
Blender с `Path Mode: Relative` в `assets/model/` подойдёт путь `../texture/cube.png`.

`--texture /путь.png` заменяет текстуры модели или демонстрационной сцены одной
PNG-текстурой. Ошибки ресурсов (нет файла, не тот формат, повреждённые данные,
не найдена текстура модели) выводятся с путём и завершают запуск с кодом 2 до
открытия окна.

`--frames N` завершает приложение после N успешных вызовов презентации; N — положительный `u64`. `--render-scale S` задаёт масштаб offscreen-разрешения и принимает конечное число из `(0, 1]`. `--resize-test` после 30 презентаций запрашивает 960×540, после 60 сворачивает окно на 250 мс и затем запрашивает восстановление 1280×720. `--scene triangle|cube|shadows` выбирает демонстрационную сцену; по умолчанию — `shadows`. `--model` можно сочетать со сценами `cube` и `shadows`; для `triangle` это ошибка аргументов. `--texture` заменяет текстуры всех мешей сцены или модели. Параметры ресурсов описаны в разделе ниже. Сцена `shadows` показывает куб со стороной 1 м, вращающийся вокруг Z, над неподвижной плоскостью 2×2 м на `y = −1`. Камера движется вокруг куба, белый направленный свет вращается в противоположную сторону; renderer создаёт одну shadow map 2048×2048 и смягчает края тени сравнением PCF 3×3. Сцены `triangle` и `cube` сохраняют прежние камеру, свет и движение без теней. `--debug-overlay` показывает в левом верхнем углу полупрозрачную панель: крупно FPS и его график за 30 с, память процесса и её график за 2 мин, название CPU и загрузку процессора этим процессом (0–100 % от всех логических ядер), название GPU и долю времени, которую GPU тратит на shadow и scene pass кадра (Vulkan timestamps; вывод в окно и ожидание vsync не учитываются), с графиками за 30 с. Из-за перекрытия начала shadow pass с хвостом предыдущего кадра измеренное время иногда немного занижено. Метрики обновляются раз в 0,5 с, размеры панели заданы в физических пикселях. При совместном использовании `--resize-test` и `--frames` требуется N ≥ 120. Ошибка аргументов или ресурса возвращает код 2, ошибка окна или рендерера — код 1, успешное завершение — код 0. Без `--frames` приложение работает до закрытия окна.

Команда `cargo tree -p engine-render` позволяет проверить, что `engine-render` не зависит от winit.

Проверка Windows-кода с macOS без линковки Windows-бинарника:

```sh
rustup target add x86_64-pc-windows-gnu
cargo check --workspace --all-targets --target x86_64-pc-windows-gnu
```

`engine-assets` собирает C-библиотеку ufbx, поэтому для этой проверки нужен
кросс-компилятор MinGW (`brew install mingw-w64`). Без него проверь остальные крейты:
`cargo check --workspace --all-targets --target x86_64-pc-windows-gnu --exclude engine-assets --exclude sandbox`.
На самой Windows используется MSVC, на macOS — clang.

Эта проверка не заменяет сборку и запуск на Windows.

## Границы текущего renderer

Рендерер выбирает устройство с Vulkan 1.3, dynamic rendering и synchronization2, создаёт offscreen-таргет в `R16G16B16A16_SFLOAT` с depth buffer `D32_SFLOAT` и одну sampled shadow map `D32_SFLOAT` размером 2048×2048. Сначала глубина мешей записывается в shadow map, затем сцена освещается и затеняется PCF 3×3; результат линейно масштабируется в поддерживаемый surface-формат. `RendererConfig::render_scale` разделяет render resolution и output resolution. Меши создаются через `Renderer::create_mesh` из вершин с позицией, нормалью, UV и цветом и индексов треугольников, текстуры — через `Renderer::create_texture` из sRGB RGBA8. `render_frame` принимает `Frame`: матрицы камеры и света в clip space (Y вверх, глубина `[0, 1]`), направленный свет с ambient и список `DrawCommand` с мешем, необязательной текстурой и матрицей модели, а также необязательный 2D-оверлей: треугольники `OverlayVertex` в пикселях окна, которые после blit рисуются с альфа-смешиванием прямо в swapchain image без `render_scale`. `Frame::shadow_view_projection = None` отключает тени. `Renderer::take_gpu_frame_time` возвращает GPU-время shadow и scene pass по timestamp queries; измерение иногда немного занижено из-за перекрытия начала shadow pass с хвостом предыдущего кадра. Спецулярного освещения и каскадов shadow map нет; отсечения граней и mipmaps тоже нет. Буферы мешей лежат в host-visible памяти без staging-загрузки, текстуры копируются в device-local память; удаления мешей и текстур нет. В коде пока нет HDR-тонмаппинга, интеграции DLSS/FSR или tessellation pipeline. Поддержка tessellationShader сообщается через capabilities и включается при наличии у устройства; это не реализует тесселяцию.

Renderer проверяет наличие validation layer только в debug-сборке и продолжает работу без него, если слой не найден. Smoke-проверки не заменяют validation layers. При teardown вызывается `device_wait_idle`, но это ожидание само по себе не доказывает завершение всех операций presentation engine. Maintenance extensions для дополнительной синхронизации presentation resources сейчас не включены.

## Документация

- [Правила работы и оркестрация](AGENTS.md)
- [База знаний Obsidian](Obsidian/RVEVault/Index.md)
