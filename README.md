# RVE

RVE — игровой движок на Rust с рендерингом через Vulkan. В репозитории есть
`rve-player` для запуска игровых проектов, `rve-build` для сборки релизов,
`sandbox` для демонстрации движка и `rvepak` для работы с архивами ресурсов.

## Требования

- Rust stable.
- Windows: видеодрайвер с поддержкой Vulkan 1.3.
- macOS: Vulkan loader и MoltenVK. Для Homebrew: `brew install vulkan-loader molten-vk`.
- HDR `R16G16B16A16_SFLOAT` и LDR post target `R8G8B8A8_SRGB` требуют optimal
  tiling `COLOR_ATTACHMENT`, `SAMPLED_IMAGE` и `SAMPLED_IMAGE_FILTER_LINEAR`; surface
  и его format должны поддерживать color attachment, а surface format — blending.
- Форматы текстур `R8G8B8A8_SRGB` и `R8G8B8A8_UNORM` требуют `SAMPLED_IMAGE`,
  `SAMPLED_IMAGE_FILTER_LINEAR`, `TRANSFER_SRC`, `TRANSFER_DST`, `BLIT_SRC` и
  `BLIT_DST` для полной mip-цепочки.

Для обычной сборки Rust-кода Vulkan SDK не требуется.

## Быстрый запуск

Из корня репозитория создай каталог `build/` и запускай команды из него.
Cargo сохраняет там артефакты сборки.

macOS и Linux:

```sh
mkdir -p build
cd build
cargo run -p sandbox
```

Windows PowerShell:

```powershell
New-Item -ItemType Directory -Force build
Set-Location build
cargo run -p sandbox
```

Sandbox показывает вращающийся куб над плоскостью. Для выбора сцены или загрузки
модели можно использовать:

```sh
cargo run -p sandbox -- --scene triangle
cargo run -p sandbox -- --scene materials --frames 300
cargo run -p sandbox -- --scene clustered --frames 300 --resize-test --debug-overlay
cargo run -p sandbox -- --model /models/cube.fbx --texture /textures/cube.png
cargo run -p sandbox -- --frames 120 --render-scale 0.5
cargo run -p sandbox -- --camera-settings /cameras/default.camerasettings
```

Ресурсы sandbox размещаются в `build/assets`. Например, путь
`/models/cube.fbx` указывает на `build/assets/models/cube.fbx`. Поддерживаются
сцены `triangle`, `cube`, `shadows`, `materials` и `clustered`, модели FBX, текстуры PNG, ограничение
числа кадров, изменение размера окна и отладочный оверлей. Настройки проекции,
exposure, tonemapping, anti-aliasing и render scale задаются файлом
`.camerasettings`; явный `--render-scale` перекрывает масштаб из него. Сцены
`shadows` и `materials` также показывают point и spot освещение.

Настройки окна sandbox необязательны. Чтобы изменить их, скопируй шаблон
`bootstrap.rves.example` в `build/bootstrap.rves`.
Параметр `PresentMode` принимает `Fifo`, `Mailbox` или `Immediate`; неподдерживаемый
режим swapchain автоматически заменяется на `Fifo`.

## Запуск и сборка игры

Пример проекта находится в [examples/demo-game](examples/demo-game). Он содержит
настройки проекта, сцены, ресурсы, Lua-скрипт и Rust-скрипт, собранный как `cdylib`
через `rve-sdk`.

Из каталога `build/`:

```sh
cargo run -p rve-build -- run ../examples/demo-game -- --frames 120
cargo run -p rve-build -- build ../examples/demo-game --output ../demo-out
cargo run --release -p rve-build -- sdk --output ../rve-sdk-dist
```

`rve-build run` собирает игру и запускает dev-player с включённой developer console.
`rve-build build` собирает distribution-player без console и создаёт релиз в указанном
каталоге, который должен отсутствовать до сборки. Для прямого запуска player CLI имеет вид
`rve-player [--project <dir>] [--scripts <path>] [--frames N] [--debug-view lit|wireframe|normals|overdraw|lod|cascades]`.
`--debug-view` принимает только dev-запуск с `--project`; в dev-player F8 циклически
меняет поддерживаемые режимы, если окно в фокусе и developer console закрыта.

Команда `rve-build sdk` собирает оба варианта player и создаёт переносимый каталог
с `rve-build`, distribution player `rve-player`, dev player `rve-player-dev`,
исходниками `rve-sdk` и маркером `rve-sdk.toml`. Игровой
crate подключает SDK через path `../rve-sdk-dist/rve-sdk` в `Cargo.toml`. Рядом с
`rve-build` маркер включает SDK-режим: checkout движка не нужен, а совместимость
проверяется по ABI и паре `ARCH-OS` текущей платформы. Без маркера используется
checkout-режим: distribution player собирается в `build/player`, dev-player — в
`build/player-dev`.

По умолчанию Cargo feature `dev-console` включена у `rve-player`, в том числе при
`cargo build --release -p rve-player`. Для distribution-бинарника собирай с
`--no-default-features`; `rve-build build` уже делает это автоматически.

В player и редакторе физическая клавиша Backquote (клавиша `ё` в русской раскладке)
открывает консоль. В редакторе клавиша действует, когда не открыто модальное окно.
Консоль занимает верхние 45% окна, показывает log и историю команд; Enter отправляет,
Tab дополняет, Up/Down выбирают подсказки или историю, Escape закрывает. Пока консоль
открыта, игровой ввод блокируется. Встроенные команды — `help [prefix]` и `clear`;
пользовательские команды Lua и Rust доступны в Play/runtime.

Lua-скрипт пишет сообщение с уровнем по умолчанию `info` через
`rve.console.write(text, level)`, где `level` — `info`/`log`, `warning`/`warn` или
`error`. Внутри lifecycle callback скрипт регистрирует команду через
`rve.console.register(name, callback, description)`; callback получает таблицу
строковых аргументов и может вернуть строку для вывода в Info. Имя команды состоит
из строчных латинских букв, цифр, `_` и `.`, начинается с буквы или `_`; `help` и
`clear` зарезервированы. Команды принадлежат скриптовому экземпляру и удаляются вместе
с ним.

Rust SDK предоставляет `ctx.console.write(text, ConsoleLevel::{Log, Warning, Error})`,
`ctx.console.register(name, description)` и `unregister(name)`. Реализуй
`Script::on_command(ctx, name, args)` для обработки зарегистрированной команды.
Подробности поведения console и ABI приведены в [EngineConsole](Obsidian/RVEVault/Engine/EngineConsole.md),
[EngineScript](Obsidian/RVEVault/Engine/EngineScript.md) и [RveSdk](Obsidian/RVEVault/Engine/RveSdk.md).

В dev-режиме `rve-player` загружает Rust-скрипты только при явном `--scripts`;
release-плеер автоматически ищет библиотеку в `Engine/`, а этот параметр позволяет
переопределить путь. `rve-build run` сам собирает `heavy_scripts` из настроек проекта
как cdylib и передаёт её плееру; `rve-build build` копирует её в релиз под
платформенным именем. Сцены с heavy-скриптами требуют `heavy_scripts` в
`project.rveproject`; формат crate описан в [RveSdk](Obsidian/RVEVault/Engine/RveSdk.md).

На Windows release-плеер собирается как оконное приложение и не открывает консоль.
Для ручной диагностики debug-бинарник можно запустить из `build` через Cargo.

На macOS релиз создаётся как `demo-out/RVE Demo.app`: открой его в Finder для
запуска без Terminal. Ресурсы и Vulkan-библиотеки находятся внутри бандла в
`Contents/Resources`. На Windows исполняемый файл, `Engine/` и `Assets/` остаются
рядом в каталоге релиза. Подробности — в [описании сборщика](Obsidian/RVEVault/Engine/RveBuild.md)
и [runtime-путей](Obsidian/RVEVault/Engine/EngineRuntime.md).

Для FSR на Windows положи официальный `amd_fidelityfx_loader_vk.dll` и соседний
`amd_fidelityfx_upscaler_vk.dll` в каталог проекта и укажи loader в
`project.rveproject`:

```toml
[runtime]
fsr_library = "vendor/amd_fidelityfx_loader_vk.dll"
```

`rve-build build` проверяет и копирует обе DLL в `Engine/`; в dev-запуске player
читает их прямо из каталога проекта. Без доступного FSR SDK запрос `fsr` использует
TAA и выводит одно предупреждение. На macOS доступен только TAA.

Для DLSS на Windows добавь официальный `nvngx_dlss.dll` в каталог проекта и укажи
NGX identifier в `project.rveproject`:

```toml
[runtime]
dlss_library = "vendor/nvngx_dlss.dll"
dlss_project_id = "a0f57b54-1daf-4934-90ae-c4035c19df04"
dlss_engine_version = "0.1.0"
```

Вместо `dlss_project_id` и `dlss_engine_version` можно задать выданный NVIDIA
`dlss_application_id`. Для компиляции нужен Windows x64 NVIDIA DLSS SDK: задай
`RVE_DLSS_SDK` на его корневой каталог, содержащий
`lib/Windows_x86_64/x64/nvsdk_ngx_s.lib`. `rve-build run` собирает dev-player с
этой feature, а `rve-build build` копирует проектную `nvngx_dlss.dll` в `Engine/`.
Если SDK создаётся с DLSS, его marker сохраняет это свойство; `run`/`build` сообщают,
если SDK player был собран без DLSS. Репозиторий не содержит NGX SDK или DLSS DLL.
Без включённого backend, NGX runtime или поддержки выбранного GPU запрос `dlss`
переходит на TAA с одним предупреждением. На macOS доступен только TAA.

## Редактор

`rve-editor` — редактор сцен: панели Scene, Game, Hierarchy, Inspector, Project и Console можно перетаскивать и собирать во вкладки.

~~~powershell
cd build
cargo run -p rve-editor -- --project ../examples/demo-game
cargo run -p rve-editor -- --project ../examples/demo-game --scripts <path-to-script-library>
~~~

`--project` необязателен: без него открывается стартовый экран с выбором папки проекта. `--scripts` нужен для Play, если в сцене есть Rust-скрипты; без библиотеки Play не запустится и сообщит об этом в Console. Коды завершения: 0 — успех, 2 — неверные аргументы или настройки проекта, 1 — ошибка окна или Vulkan.
Scene и Game имеют отдельный selector `View`; Inspector редактирует Anti-Aliasing у
назначенного `.camerasettings` и сразу записывает этот ресурс.

| Действие | Управление |
|---|---|
| Developer console | Backquote (клавиша `ё` в русской раскладке), Escape — закрыть |
| Сохранить, отменить, повторить | Ctrl+S, Ctrl+Z, Ctrl+Y |
| Инструменты Move, Rotate, Scale | W, E, R |
| Фокус на выделенном, удаление | F, Delete |
| Орбита камеры | ПКМ или Alt+ЛКМ |
| Панорама, зум | СКМ, колесо |
| Полёт | ПКМ + WASD (Q и E — вниз и вверх) |
| Добавить модель в сцену | Перетащить `.fbx` из Project в Hierarchy или Scene |
| Импорт файлов | Кнопка Import в Project или перетаскивание файлов в окно |

Вкладка Game показывает сцену глазами камеры; при нескольких камерах её можно выбрать в выпадающем списке над изображением. Выбор влияет только на вкладку: игра идёт от первой камеры в файле сцены.

Play не меняет документ: Stop возвращает сцену к состоянию до запуска. Lua, зависший дольше 5 секунд, прерывается, но зависший Rust-скрипт прервать нельзя, и он остановит редактор.

Ограничения: раскладка панелей не сохраняется, список файлов обновляется кнопкой Refresh, а переименование или удаление файла в Project не обновляет ссылки в сценах. При сохранении комментарии в файле сцены теряются.

## Обновление проектов и renderer API

Старые сцены с полями `fov_deg`, `near` и `far` внутри `[objects.camera]` больше не разбираются. Перенеси проекцию и настройки вывода в отдельный файл `.camerasettings`, добавь его в список assets и укажи путь в поле settings камеры:

~~~toml
[[assets]]
path = "/cameras/main.camerasettings"

[[objects]]
id = "camera"
[objects.camera]
settings = "/cameras/main.camerasettings"
~~~

Файл настроек использует таблицы projection, render и color:

~~~toml
[projection]
fov_deg = 60.0
near = 0.1
far = 1000.0

[render]
render_scale = 1.0
anti_aliasing = "fxaa"

[color]
exposure_ev = 0.0
tonemapper = "aces"
~~~

Код, использующий engine-render, тоже требует миграции. Поле `RendererConfig::render_scale` удалено: масштаб, exposure, tonemapper и anti-aliasing теперь передаются в `Frame` через `view: ViewSettings`, а позиция камеры — через `camera_position`.

В `DrawCommand` поле `texture` заменено на `material: Option<MaterialId>`. Создавай `Material` через `Renderer::create_material`; карты base color, metallic-roughness и normal можно загружать с нужным пространством цвета через `Renderer::create_texture_with_color_space`. Полный текущий контракт описан в [заметке renderer](Obsidian/RVEVault/Engine/EngineRender.md).

`Frame` принимает `lights: &[LocalLight]` и разреженный `local_light_shadows: &[LocalLightShadow]`; каждый запрос ссылается на индекс источника и содержит point- или spot-матрицы. Runtime, редактор и sandbox строят их общими функциями `engine-scene`, а renderer отбирает до 1024 видимых источников и отдельно назначает тени выбранным lights: по расстоянию от камеры, с исходным индексом для равных расстояний, стоимостью 6 слоёв для point и 1 для spot при общем лимите 32. Источники без свободного бюджета освещают без теней. Общий D32-массив 1024×1024 занимает 128 MiB; point-грани идут в порядке +X, −X, +Y, −Y, +Z, −Z. Диапазон света менее 0.001 остаётся освещённым, но не отбрасывает тени; spot с внешним углом 90° сохраняет исходный конус света, а тени покрывают конечный угол чуть меньше 180° из-за сингулярности перспективы. Кластеры имеют сетку 16×9×24 и хранят до 128 индексов; сцена `clustered` по-прежнему показывает стресс-набор из 160 локальных источников. `cargo run -p sandbox -- --scene shadows --frames 300 --resize-test` показывает point/spot-тени и проверяет resize/restore.

## Архивы ресурсов

Инструмент `rvepak` собирает, просматривает и проверяет наборы архивов по
TOML-манифесту. Пример манифеста — [rvepak.example.toml](rvepak.example.toml);
нормативное описание формата — [docs/rvepak-format.md](docs/rvepak-format.md).

Перед сборкой помести файлы в `build/assets`. Пример манифеста создаёт набор в
`build/game/assets`; каталог назначения должен отсутствовать. Из каталога
`build/` выполни:

```sh
cargo run -p rvepak -- build ../rvepak.example.toml
cargo run -p rvepak -- list game/assets
cargo run -p rvepak -- verify game/assets
```

## Техническая документация

[Карта архитектуры и заметки Obsidian](Obsidian/RVEVault/Index.md) содержат
устройство компонентов и их контракты. Подробности смотрите в заметках:

- [Настройки проекта](Obsidian/RVEVault/Engine/EngineProject.md)
- [Формат сцен](Obsidian/RVEVault/Engine/EngineScene.md)
- [Загрузка ресурсов](Obsidian/RVEVault/Engine/EngineResources.md)
