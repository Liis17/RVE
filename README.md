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
cargo run --release -p rve-build -- sdk --output ../rve-sdk-dist --rust-toolchain <toolchain-dir>
```

`rve-build run` собирает игру и запускает dev-player с включённой developer console.
`rve-build build` собирает distribution-player без console и создаёт релиз в указанном
каталоге, который должен отсутствовать до сборки. Для прямого запуска player CLI имеет вид
`rve-player [--project <dir>] [--scripts <path>] [--frames N] [--debug-view lit|wireframe|normals|overdraw|lod|cascades]`.
`--debug-view` принимает только dev-запуск с `--project`; в dev-player F8 циклически
меняет поддерживаемые режимы, если окно в фокусе и developer console закрыта.

### Игровые действия ввода

Назначай именованные действия и их клавиши в необязательной секции `[input]` файла
`project.rveproject`. Без этой секции проект работает как раньше, с пустым набором
действий. `button`, `axis` и `vector2` используют имена клавиш (`Space`, `W`) или
кнопок мыши с префиксом (`Mouse.Left`).

```toml
[input]
default_context = "game"

[input.contexts.game.Jump]
kind = "button"
inputs = ["Space"]

[input.contexts.game.Move]
kind = "vector2"
up = ["W"]
down = ["S"]
left = ["A"]
right = ["D"]

[input.contexts.game.Look]
kind = "vector2"
analog = "MouseDelta"
scale = 0.1

[input.contexts.game.Throttle]
kind = "axis"
negative = ["Q"]
positive = ["E"]
analog = "WheelY"
scale = 1.0
```

Для `axis` источниками analog служат `MouseX`, `MouseY`, `WheelX` и `WheelY`; для
`vector2` — `MouseDelta` и `Wheel`. Если заданы контексты, `default_context` обязателен;
он выбирает один начальный активный контекст. В Lua действия доступны через
`rve.input.action_down(name)`, `rve.input.action_pressed(name)`,
`rve.input.action_released(name)`, `rve.input.axis(name)` и `rve.input.vector(name)`
(возвращает `x, y`). `rve.input.context()` возвращает активный контекст;
`rve.input.set_context(name)` переключает его, а
`rve.input.rebind(context, action, slot, inputs)` временно заменяет слот.
`rve.input.bindings(context, action, slot)` читает биндинг,
`rve.input.reset_bindings()` возвращает настройки проекта,
`rve.input.pressed_inputs()` перечисляет клавиши и кнопки мыши с новым pressed edge.
Например:

```lua
if rve.input.action_pressed("Jump") then
  rve.log_info("Jump")
end
local move_x, move_y = rve.input.vector("Move")
```

Контекстные изменения и переназначения применяются со следующего кадра скриптов и
откатываются при ошибке callback; они не сохраняются между запусками. Сырой API
`key_down`/`key_pressed` и `mouse_down`/`mouse_pressed` остаётся доступен независимо
от контекста. Неизвестное действие или неправильный вид дают Lua runtime error;
известное, но неактивное действие возвращает `false` или `0`.

Команда `rve-build sdk` собирает release редактор, сборщик и оба варианта player и
создаёт переносимый каталог с `rve-editor`, `rve-build`, distribution player
`rve-player`, dev player `rve-player-dev`, гостевым API `rve-sdk`, выбранным
`toolchain/` и маркером `rve-sdk.toml`. Укажи `--rust-toolchain <sysroot>`, чтобы
включить автономную сборку Rust-скриптов. Для CI без сборки guest-кода можно явно
передать `--no-rust-toolchain`.

Guest API использует ABI v6; host продолжает загружать библиотеки ABI v3, v4 и v5.
Переносимый SDK marker требует точного совпадения ABI, поэтому комплект с marker
v5 нужно пересобрать командой `rve-build sdk`. Builder автоматически синхронизирует
SDK в `<project>/.rve/sdk` перед сборкой проектных Rust scripts.

На Windows подготовь минимальную GNU toolchain через rustup и передай её sysroot:

~~~powershell
rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
$rustToolchain = rustup run stable-x86_64-pc-windows-gnu rustc --print sysroot
cargo run --release -p rve-build -- sdk --output ../rve-sdk-dist --rust-toolchain $rustToolchain
~~~

Полный sysroot попадает в дистрибутив вместе с self-contained `rust-lld` и
MinGW runtime DLL. Сборка скриптов использует их со статическим CRT, поэтому на
Windows не нужны отдельно установленные GCC или MinGW. Для сборки SDK на macOS
сначала установи Apple Command Line Tools командой `xcode-select --install` и
обеспечь доступность Vulkan loader/MoltenVK (`--vulkan-sdk`, `VULKAN_SDK` или
Homebrew). Передай `--rust-toolchain` sysroot активной macOS toolchain.

На Windows SDK-бинарники собираются с `crt-static`; на macOS создаётся
`RVE Editor.app` с Vulkan loader/MoltenVK. Внутри SDK marker рядом с `rve-build`
включает SDK-режим, проверяющий ABI и пару `ARCH-OS`. Готовый каталог можно
перенести на машину без checkout движка: редактор, builder, player и toolchain
берутся из соседних файлов SDK. Игровой crate при прямом использовании SDK может
подключать `../rve-sdk-dist/rve-sdk`; `rve-build scripts` синхронизирует API в
`<project>/.rve/sdk/rve-sdk` перед сборкой. Без marker builder использует checkout:
distribution player собирается в `build/player`, dev-player — в `build/player-dev`.

Workflow `.github/workflows/sdk-release.yml` публикует Windows x64 дистрибутив
`sdk-verified.zip` в GitHub Releases после подходящих push в `mommy`. Изменения
редактора, builder, player, гостевого API и их транзитивных Cargo-зависимостей,
общих настроек Cargo или самого конвейера публикации запускают сборку. Изменения
документации, примеров и независимых инструментов её пропускают. Ручной запуск
`SDK Release` из `mommy` принудительно собирает дистрибутив.
Каждый коммит получает отдельный prerelease `sdk-<полный SHA>`; повторный запуск
обновляет ZIP в том же релизе. Обычный latest release не меняется. Перед публикацией
архив распаковывается во временный каталог, где из шаблона New Project собираются
Rust-скрипты и готовый player при PATH только с системными каталогами Windows.
Дополнительные секреты не нужны: публикация использует `GITHUB_TOKEN` с
`contents: write` только в задании выпуска.

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

`--project` необязателен: без него открывается стартовый экран с выбором папки проекта. В File → New Project… можно создать проект из встроенного шаблона с камерой, светом, кубом, Lua-прыжком и Rust-скриптом орбиты. Проект создаётся в выбранной родительской папке; существующая папка не перезаписывается.

Если `project.rveproject` задаёт `heavy_scripts`, редактор без `--scripts` автоматически запускает `rve-build scripts` при открытии проекта и перед Play. Play ждёт успешной сборки; при ошибке он не запускается. Готовая библиотека перезагружается без перезапуска редактора, вне Play. Явный `--scripts <library>` переопределяет автоматическую сборку и используется для Play и схем Inspector. Коды завершения: 0 — успех, 2 — неверные аргументы или настройки проекта, 1 — ошибка окна или Vulkan.

Scene и Game имеют отдельный selector `View`; Inspector редактирует Anti-Aliasing у
назначенного `.camerasettings` и сразу записывает этот ресурс.

| Действие | Управление |
|---|---|
| Developer console | Backquote (клавиша `ё` в русской раскладке), Escape — закрыть |
| Сохранить, отменить, повторить | Ctrl+S, Ctrl+Z, Ctrl+Y |
| Инструменты Move, Rotate, Scale | W, E, R |
| Фокус на выделенном, удаление | F, Delete |
| Орбита камеры | ПКМ или Alt+ЛКМ |
| Тачпад: свайп двумя пальцами | Orbit (Auto при Point scroll или режим Trackpad) |
| Shift + свайп | Pan по двум осям |
| Pinch / Cmd/Ctrl + свайп | Zoom |
| Rotation | Поворот камеры вокруг цели |
| Smart zoom (двойное касание двумя пальцами, macOS) | Frame выделения, как F |
| Alt + Cmd/Ctrl + ЛКМ | Pan |
| Ctrl + клик (macOS) | Контекстное меню, как ПКМ |
| Панорама, зум | СКМ, колесо |
| Полёт | ПКМ + WASD (Q и E — вниз и вверх) |
| Добавить модель в сцену | Перетащить `.fbx` из Project в Hierarchy или Scene |
| Импорт файлов | Кнопка Import в Project или перетаскивание файлов в окно |

Навигация Scene выбирается в Edit → Scene Navigation: Auto, Trackpad (orbit/pan), Mouse wheel (zoom). Режим сохраняется в settings.json. Auto различает Point и Line scroll; на Windows precision touchpad может требовать ручного выбора Trackpad из-за ограничения winit 0.30.

Вкладка Game показывает сцену глазами камеры; при нескольких камерах её можно выбрать в выпадающем списке над изображением. Выбор влияет только на вкладку: игра идёт от первой камеры в файле сцены.

Play не меняет документ: Stop возвращает сцену к состоянию до запуска. Lua, зависший дольше 5 секунд, прерывается, но зависший Rust-скрипт прервать нельзя, и он остановит редактор.

Ограничения: раскладка панелей не сохраняется; изменения в `Assets/` автоматически отражаются в Project и обновляют затронутые ресурсы, но переименование или удаление файла не обновляет ссылки в сценах. При сохранении комментарии в файле сцены теряются.

## Физика

Физика работает в отдельном потоке `rve-physics` с фиксированным шагом. Сцена задаёт тела компонентами, а проект — параметрами мира:

```toml
# project.rveproject
[physics]
gravity = [0.0, -9.81, 0.0]
fixed_hz = 60        # 1..=1000
max_substeps = 8     # 1..=64
```

Компоненты объекта в `.rvescene`:

- `collider` — `shape` (`box`, `sphere`, `capsule`, `convex_mesh`, `triangle_mesh`), размеры, `offset`, `trigger`, `friction`, `restitution`.
- `rigid_body` — `kind` (`static`, `kinematic`, `dynamic`), `mass`, демпфирование, `gravity_scale`, `lock_rotation`. Требует `collider`.
- `character_controller` — капсула `radius`/`height`, `step_height`, `max_slope_deg`, `snap_distance`, `skin`, `push_strength`. Несовместим с `collider` и `rigid_body`.

Объекты `dynamic` и `character_controller` должны быть корневыми; их дети разрешены. Mesh-формы используют базовую модель `mesh_renderer`, размер берётся из мирового масштаба на момент загрузки.

Lua (`rve.physics`, методы объекта, callbacks класса):

```lua
function Coin:on_trigger_enter(other)
  rve.find(self.object.id):set_collider_enabled(false)
end

local hit = rve.physics.raycast(origin, direction, 10.0, { include_triggers = false })
local result = rve.find(self.object.id):move({ x = dx, y = dy, z = dz }) -- {grounded, collided}
```

Методы: `get_velocity`, `set_velocity`, `add_impulse`, `add_force` (только dynamic), `move` и `is_grounded` (только `character_controller`), `set_collider_enabled` и `is_collider_enabled`. Callbacks: `on_collision_enter(self, other, contact)`, `on_collision_exit`, `on_trigger_enter`, `on_trigger_exit`. Rust-скрипты получают те же возможности через `rve-sdk` (ABI v5, `Context::physics()`).

Демо `examples/demo-game` стартует со сцены `Assets/scenes/physics.rvescene`: WASD двигает игрока в осях мира, Space прыгает, монеты подбираются при касании.

~~~powershell
cd build
cargo run -p rve-player -- --project ../examples/demo-game
~~~

Ограничения v1: нет интерполяции поз dynamic-тел, нет CCD, raycast и контроллер видят результат прошлого шага, масштаб запекается при загрузке, LOD не используется для коллизий, солвер без islands, стопки выше примерно пяти ящиков могут дрейфовать. Подробности и известные дефекты — в `Obsidian/RVEVault/Engine/EnginePhysics.md`.

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
