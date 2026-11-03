# EngineRuntime

Parent: [[Index]]

## Назначение и границы

`engine-runtime` связывает настройки проекта, ресурсы, сцену, скрипты, окно и renderer в игровое приложение. Публичный вход — `engine_runtime::run_player() -> ExitCode`; бинарник `engine/rve-player` вызывает его без параметров. Runtime не переносит оконную или Vulkan-логику в `engine-project`, `engine-window` либо `engine-platform`.

Схема `bootstrap.rves` и `project.rveproject`, defaults и их проверка описаны в
[[Engine/EngineProject]]. Здесь зафиксированы только runtime-пути и использование.

## Layout проекта

Все базовые пути вычисляются от каталога текущего исполняемого файла или `--project`, а не от cwd. На Windows `Layout` получает абсолютный путь и удаляет verbatim-префикс через `engine_files::strip_windows_verbatim_prefix`; ограничения Windows-форм описаны в [[Engine/EngineFiles]]. На Unix существующие пути канонизируются; каталог `--project` проверяется до создания `Layout`.

| Режим | Bootstrap | Корень assets | Настройки проекта |
|---|---|---|---|
| Release | `<release_root>/Engine/bootstrap.rves` | `<release_root>/Assets`, `SourceMode::Registry` | `/rve/project.toml` через `ResourceHandle` |
| Dev (`--project <dir>`) | `<project>/bootstrap.rves` | `<project>/Assets`, `SourceMode::Loose` | `<project>/project.rveproject` |

`release_root` (`engine/engine-runtime/src/layout.rs`) — каталог исполняемого файла.
На macOS, если executable находится в `<Name>.app/Contents/MacOS`, это
`Contents/Resources` того же бандла: Finder запускает «голый» исполняемый файл через
Terminal, поэтому `rve-build` публикует macOS-релиз как `.app`.

Параметр `--scripts <path>` задаёт ABI-библиотеку Rust-скриптов. В release layout,
если параметр не задан, runtime выбирает `Engine/game_scripts.dll` на Windows или
`Engine/libgame_scripts.dylib` на macOS, только если файл существует. В dev-режиме
`--project` автоматического поиска нет: библиотеку передают через `--scripts`.
Параметр разрешён и в release-режиме и перекрывает стандартный путь. Повтор,
отсутствующее значение и значение, начинающееся с `--`, являются ошибкой CLI с
кодом 2. Относительный путь `--scripts` разрешается от текущего рабочего каталога и
приводится к абсолютному; существующие пути нормализуются по правилам платформы.

Заголовок окна берётся из bootstrap, затем из `ProjectSettings.name`. `AppIcon`
загружается через `Assets::load_texture` до создания окна; ошибка загрузки относится
к коду 2. Начальный размер окна берётся из bootstrap с defaults `WindowConfig`.

## Потоки и протокол запуска

Первой runtime-операцией после получения пути executable вызывается `unsafe engine_platform::prepare_vulkan_environment(release_root)`. Она выполняется до создания потоков. Затем создаётся `PendingActivation`, которая сохраняет ранний сигнал singleton activation до `WindowProxy`.

После разбора CLI, bootstrap и загрузки настроек runtime пытается захватить
`InstanceLock` по `ProjectSettings.id`, если bootstrap запрещает несколько
экземпляров. Значение ключа и другие поля настроек описаны в [[Engine/EngineProject]].
`AlreadyRunning` запускает `activate_running_instance` с дедлайном 5 секунд:
подтверждение завершает второй процесс с кодом 0; timeout выполняет один повторный
`acquire`, чтобы принять блокировку, если прежний владелец завершился. Если lock
остался занят или повторная попытка завершилась ошибкой, код выхода 4.

Затем выполняется проверка hardware requirements: подходящее устройство Vulkan, RAM и число логических CPU. Ошибка `probe_devices`, отсутствие подходящего устройства или недостаток ресурсов выводятся в stderr и системный диалог; код выхода 3. CPU-загрузка сцены идёт до окна: `SceneDocument` раскрывается через `engine_scene::expand` (префабы читаются через тот же `ResourceHandle`, поэтому dev и release загружают их одинаково; формат — [[Engine/EngineScene#Префабы]]), раскрытый документ строит `World`, каждый уникальный asset `.camerasettings` читается и проверяется один раз и назначается камерам, FBX базовой модели и всех LOD levels кэшируются по пути, объявленные PNG для замены base-color, текстур уровней и `MaterialOverride` загружаются как ресурсы, а HDR environment читаются через `Assets::load_hdr` в CPU `EnvironmentSource`. `mesh.texture` заменяет только base-color map; заданные поля `mesh.material` меняют соответствующие factors, alpha mode/cutoff и карты; emissive texture загружается sRGB, occlusion — Linear, пропущенные поля сохраняют материал FBX. Lite-файлы читаются как UTF-8 через `ResourceHandle`.

`ModuleHost` начинает владеть потоками при их первом старте. Он запускает `rve-audio`, `rve-physics`, `rve-network` и `rve-neural`, затем именованный поток `rve-scripts`. На этом потоке runtime сначала загружает `ScriptLibrary` (если задан путь), затем создаёт `ScriptHost::load_with_interrupt` с библиотекой и заранее переданным `ScriptInterruptHandle`. Только после этого поток сообщает `Ready`, до создания окна. Ошибка загрузки библиотеки или Lua даёт код 2; ошибка запуска потока или его паника — код 1. `ScriptLibrary` намеренно не выгружает динамический модуль.

После Ready `engine-window` запускает `WindowConfig` с `RedrawMode::OnDemand`. `WindowHandler::init` на главном потоке создаёт `Renderer`, загружает модели, текстуры, environment maps и immutable GPU materials, запускает `rve-render` и отправляет scripts сообщение `Go` с GPU-таблицей `(MeshId, MaterialId)` по каждому уровню объекта и кэшем environment inputs. Неизменённые FBX materials переиспользуются между объектами; object overrides создают материалы для соответствующих частей. Кэш GPU-текстур учитывает color space, поэтому одна CPU-текстура при разных ролях загружается отдельно как sRGB и linear. Blend materials попадают в отдельный HDR pass renderer и сортируются по мировой bounds относительно камеры; skybox пишется между opaque и blend pass; остальные детали pipeline описаны в [[Engine/EngineRender]]. Render thread — единственный владелец renderer после передачи; каждый `FrameSnapshot` несёт активную камеру (`view`, `projection`, position), её проверенные `.camerasettings`, directional light, отображённые point/spot lights, sparse local shadow matrices с индексами исходных источников, необязательный environment (renderer-local id, intensity, owner rotation, skybox) и выбранные draw-команды. Local light цвет умножается на интенсивность в `f64` с насыщением до `f32::MAX` при переполнении; редактор применяет ту же политику. Углы spot переводятся в cosines. При `shadows = true` runtime вычисляет point/spot матрицы через общие функции `engine-scene::light`; near/range и 90° spot boundary policy описаны в [[Engine/EngineScene]]. Невалидная матрица оставляет свет без тени, не выключая само освещение. При смене HDR source или процедурных параметров renderer запекает environment один раз; для не изменившегося источника descriptor/GPU ресурсы не пересоздаются. У процедурного источника cache key квантует направление солнца, чтобы небольшое движение directional light не запускало bake каждый кадр. Camera-relative cascades вычисляются из camera, aspect, `LightState.shadow` и разрешения карты для directional light. `Frame` принимает `FrameCamera`, local lights, `LocalLightShadow` и необязательный `FrameEnvironment`; детали IBL и uniform layout описаны в [[Engine/EngineRender]].

## Конвейер и отменяемые ожидания

Scripts thread выполняет `start`, затем цикл `update(dt)` → `World::build_frame` → `FrameSnapshot`. Снимок включает выбранную активную камеру и её settings, мировую позицию камеры для specular view-вектора и skybox, directional light, все локальные point/spot состояния, матрицы затеняемых источников, необязательный environment state и draw-команды с material IDs объектов. LOD выбирается по расстоянию от camera position до мировой позиции объекта; `cull_distance` исключает объект из draw list и его shadow draws. Renderer выполняет culling мешей по bounds отдельно для камеры и каждой directional/local shadow view; сначала отбирает до 1024 локальных источников для camera frustum, затем назначает им до 32 shadow layers по расстоянию камеры (point = 6 слоёв, spot = 1) с исходным индексом как tie-break. LOD distance использует мировой origin объекта без учёта масштаба, размера на экране и hysteresis. Время приходит из монотонного `Clock`; `dt` отражает фактически прошедшее время. Снимки передаются через `flume::bounded(1)`, а отправитель выбирает между готовностью очереди и control-каналом через `Selector`, поэтому заполненная очередь не блокирует Shutdown.

Оконный поток передаёт события в `Arc<Mutex<InputAccumulator>>` короткими операциями. Перед каждым `ScriptHost::update` scripts thread забирает `take_frame`: удерживаемые клавиши и кнопки сохраняются, pressed/released и накопленные mouse/wheel delta сбрасываются. `ScriptInput` копируется в host, поэтому все экземпляры и message callbacks одного update видят один снимок. Потеря фокуса и вход в Background освобождают все удерживаемые клавиши и кнопки с `released` edge. Фоновый snapshot не содержит удерживаемого ввода, кроме ещё не выданных `released` от сброса; новые события в Background отбрасываются. До первого `CursorMoved` позиция курсора равна `[0, 0]`; `CursorLeft` не обрабатывается, поэтому последняя позиция сохраняется после выхода из окна, потери фокуса и перехода в Background. Mouse/wheel delta сбрасываются.

Render thread выбирает между snapshots и resize/occlusion/shutdown control-каналом. `view_projection` пересчитывается по последнему размеру окна. Нулевой размер или occlusion приостанавливает отрисовку; render отбрасывает уже поставленный снимок, а scripts получают Background и продолжают вызывать `update` с частотой `ProjectSettings.runtime.background_tick_hz` ([[Engine/EngineProject]]), не собирая и не отправляя новые snapshots. При смене в Background накопитель освобождает клавиши/кнопки и выдаёт `released` один раз; фоновые тики получают только эти края, без удерживаемого ввода, но с последней известной позицией курсора и нулевыми дельтами. Клавиша, зажатая во время Background (или до получения фокуса), после возврата не считается удерживаемой, пока её не нажмут снова. Если `Background(true)` пришёл до `Go` (например, при нулевом начальном размере окна), scripts запоминают последнее значение до handshake и начинают сразу в соответствующем режиме. Ожидание следующего фонового тика выполняется через `recv_timeout` и injected clock, без `sleep`. Восстановление окна возвращает scripts в обычный цикл и публикацию snapshots.

`drain_events` выводится runtime: Info в stdout, Error в stderr с префиксом `[script]`. При штатном shutdown scripts thread сначала вызывает `destroy` (`on_destroy`), после чего уничтожает `ScriptHost` и экземпляры на том же потоке; runtime ждёт его завершения до 2 секунд. При timeout `ScriptInterruptHandle` прерывает зависший lite Lua-код; затем runtime ждёт ещё до 2 секунд через stopped-канал. Если scripts всё ещё не остановились (например, ABI-скрипт блокирует поток), runtime отсоединяет handle без `join`, пишет ошибку и возвращает код 1. Interrupted lite callbacks не получают `on_destroy` согласно контракту [[Engine/EngineScript]]. При ошибке или панике renderer interrupt подаётся сразу.

## Завершение и ошибки

Закрытие окна посылает Shutdown scripts и render. Render дожидается Vulkan через уничтожение renderer на своём потоке, помечает `RenderStopped` и вызывает `WindowProxy::request_exit`. В `exiting` render join-ится только после `RenderStopped`; иначе главный поток ждёт до 2 секунд через канал и при timeout отсоединяет render handle, пишет ошибку и выставляет код 1. Затем scripts ждут до 2 секунд; после interrupt выполняется ещё одно ограниченное ожидание до 2 секунд, а при timeout поток отсоединяется без `join` и код выхода становится 1. `ModuleHost` останавливает и join-ит оставшиеся модули в обратном порядке; паника потока или stop-handler становится ошибкой запуска/завершения. После этого останавливается `ResourceSystem` и освобождается `InstanceLock`.

| Код | Значение |
|---:|---|
| 0 | Успешный выход, `--frames` после N успешных презентаций или активация уже работающего экземпляра |
| 1 | Ошибка ОС при acquire singleton lock, ошибка окна/renderer/runtime, паника потока или timeout аварийного завершения render/scripts |
| 2 | Ошибка CLI, layout/bootstrap, проекта, ресурсов, сцены, icon или загрузки Lua/ABI-библиотеки до окна |
| 3 | Vulkan probe/device или системные RAM/CPU требования; stderr и системный диалог |
| 4 | Экземпляр уже запущен, но активация не удалась (включая timeout; ошибка ОС при acquire возвращает 1) |

Ошибки с кодами 1 и 2 печатаются только в stderr. Для кода 3 runtime дополнительно показывает `show_error_dialog` с конкретными причинами.

## Источники

| Файл | Ответственность |
|---|---|
| `engine/engine-runtime/src/lib.rs` | `run_player`, layout startup, resources, project lock, hardware check и lifecycle модулей |
| `engine/engine-runtime/src/cli.rs` | `--project`, `--scripts`, `--frames` и usage |
| `engine/engine-runtime/src/layout.rs` | release/dev пути и выбор ABI-библиотеки |
| `engine/engine-runtime/src/activation.rs` | `PendingActivation` и single-instance handshake |
| `engine/engine-runtime/src/hardware.rs` | чистое решение по device/RAM/CPU требованиям |
| `engine/engine-runtime/src/module_host.rs` | именованные потоки, stop/join в обратном порядке, panic collection и detach |
| `engine/engine-runtime/src/scene.rs` | CPU-preload сцены и загрузка GPU-таблицы ресурсов |
| `engine/engine-runtime/src/pipeline.rs` | snapshots, script/render loops, fakeable backend и status guard |
| `engine/engine-runtime/src/input.rs` | `InputAccumulator`, переходы состояния и кадровые input snapshots |
| `engine/engine-runtime/src/player.rs` | `WindowHandler`, renderer handoff и shutdown окна |
| `engine/engine-runtime/src/console.rs` | UI snapshots, latest-only slot и интеграция console-команд с scripts thread |
| `engine/rve-player/src/main.rs` | бинарный entry point |

## Зависимости и взаимодействия

- Использует `engine-project` для `BootstrapConfig` и `ProjectSettings`, [[Engine/EngineResources]] и [[Engine/EngineAssets]] для загрузки содержимого.
- Использует [[Engine/EngineFiles]] для нормализации Windows filesystem path в `Layout`.
- Использует [[Engine/EngineScene]] для `SceneDocument`, `World`, камеры и света; [[Engine/EngineScript]] остаётся на scripts thread и не становится `Send`.
- Передаёт окно через [[Engine/EngineWindow]], общую Vulkan-работу через [[Engine/EngineRender]] и платформенную подготовку, диалог и singleton API через [[Engine/EnginePlatform]].
- Pipeline cache приложения использует sanitized `ProjectSettings.name` под пользовательским каталогом кэша; renderer получает bytes до создания и сохраняет данные до Drop в render thread.
- Renderer создаётся на главном потоке в `init`, затем передаётся render thread. Менять зависимости и поведение upstream-крейтов для runtime не требуется.
- Пустые `engine-audio`, `engine-physics`, `engine-network`, `engine-neural` запускаются как отдельные управляемые потоки; доменной работы пока не выполняют.

## Developer console

Runtime всегда зависит от `engine-console` для process logger; `dev-console` включает
консольный UI и event plumbing через `engine-ui`, egui, egui-winit и winit. Эта feature
включена у `rve-player` по умолчанию. `run_player` проверяет CLI до установки logger;
после установки уровни Info/Warn/Error идут в stdout/stderr, а при dev console также в
общий scrollback sink. Distribution player собирается с выключенными default features,
поэтому не содержит реализации overlay.

Ввод Backquote открывает/закрывает console без ввода символа. Пока overlay открыт,
game input не попадает в snapshot скрипта; при закрытии и потере фокуса held keys
освобождаются через `InputAccumulator`. Скриптовый поток принимает команду до
следующего update, использует `ScriptHost::execute_command`, а изменения каталога
публикует snapshot-ом. UI кадры передаются render thread через bounded latest-only slot:
при замене кадра накапливается `TexturesDelta`, но отбрасывается старая геометрия.
Overlay создаётся по требованию и освобождает managed textures при закрытии и teardown;
без открытого UI остаётся обычный `render_frame` путь. Подробный контракт —
[[Engine/EngineConsole]].
