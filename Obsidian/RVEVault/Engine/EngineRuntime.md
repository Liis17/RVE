# EngineRuntime

Parent: [[Index]]

## Назначение и границы

`engine-runtime` связывает настройки проекта, ресурсы, сцену, скрипты, окно и renderer в игровое приложение. Публичный вход — `engine_runtime::run_player(register_heavy: fn(&mut HeavyRegistry)) -> ExitCode`; бинарник `engine/rve-player` передаёт пустой callback регистрации heavy-скриптов. Runtime не переносит оконную или Vulkan-логику в `engine-project`, `engine-window` либо `engine-platform`.

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

Затем выполняется проверка hardware requirements: подходящее устройство Vulkan, RAM и число логических CPU. Ошибка `probe_devices`, отсутствие подходящего устройства или недостаток ресурсов выводятся в stderr и системный диалог; код выхода 3. CPU-загрузка сцены идёт до окна: `SceneDocument` строит `World`, FBX кэшируется по пути модели, override PNG заменяет текстуры всех мешей соответствующего объекта, а lite-файлы читаются как UTF-8 через `ResourceHandle`.

`ModuleHost` начинает владеть потоками при их первом старте. Он запускает `rve-audio`, `rve-physics`, `rve-network` и `rve-neural`, затем именованный поток `rve-scripts`. Scripts thread создаёт `ScriptHost::load_with_interrupt` с заранее переданным `ScriptInterruptHandle` и сообщает `Ready` до создания окна. Ошибка Lua загрузки даёт код 2; ошибка запуска потока или его паника — код 1.

После Ready `engine-window` запускает `WindowConfig` с `RedrawMode::OnDemand`. `WindowHandler::init` на главном потоке создаёт `Renderer`, загружает меши и текстуры, запускает `rve-render` и отправляет scripts сообщение `Go` с GPU-таблицей. Render thread — единственный владелец renderer после передачи; он строит `Frame` с фиксированным clear color `[0.1, 0.1, 0.12, 1.0]` и вызывает `render_frame`.

## Конвейер и отменяемые ожидания

Scripts thread выполняет `start`, затем цикл `update(dt)` → `World::build_frame` → `FrameSnapshot`. Время приходит из монотонного `Clock`; `dt` отражает фактически прошедшее время. Снимки передаются через `flume::bounded(1)`, а отправитель выбирает между готовностью очереди и control-каналом через `Selector`, поэтому заполненная очередь не блокирует Shutdown.

Render thread выбирает между snapshots и resize/occlusion/shutdown control-каналом. `view_projection` пересчитывается по последнему размеру окна. Нулевой размер или occlusion приостанавливает отрисовку; render отбрасывает уже поставленный снимок, а scripts получают Background и продолжают вызывать `update` с частотой `ProjectSettings.runtime.background_tick_hz` ([[Engine/EngineProject]]), не собирая и не отправляя новые snapshots. Если `Background(true)` пришёл до `Go` (например, при нулевом начальном размере окна), scripts запоминают последнее значение до handshake и начинают сразу в соответствующем режиме. Ожидание следующего фонового тика выполняется через `recv_timeout` и injected clock, без `sleep`. Восстановление окна возвращает scripts в обычный цикл и публикацию snapshots.

`drain_events` выводится runtime: Info в stdout, Error в stderr с префиксом `[script]`. При штатном shutdown scripts thread сначала вызывает `destroy` (`on_destroy`), а runtime ждёт его завершения до 2 секунд. При timeout `ScriptInterruptHandle` прерывает зависший lite Lua-код; затем runtime ждёт ещё до 2 секунд через stopped-канал. Если scripts всё ещё не остановились (например, heavy-скрипт блокирует поток), runtime отсоединяет handle без `join`, пишет ошибку и возвращает код 1. Interrupted lite callbacks не получают `on_destroy` согласно контракту [[Engine/EngineScript]]. При ошибке или панике renderer interrupt подаётся сразу.

## Завершение и ошибки

Закрытие окна посылает Shutdown scripts и render. Render дожидается Vulkan через уничтожение renderer на своём потоке, помечает `RenderStopped` и вызывает `WindowProxy::request_exit`. В `exiting` render join-ится только после `RenderStopped`; иначе главный поток ждёт до 2 секунд через канал и при timeout отсоединяет render handle, пишет ошибку и выставляет код 1. Затем scripts ждут до 2 секунд; после interrupt выполняется ещё одно ограниченное ожидание до 2 секунд, а при timeout поток отсоединяется без `join` и код выхода становится 1. `ModuleHost` останавливает и join-ит оставшиеся модули в обратном порядке; паника потока или stop-handler становится ошибкой запуска/завершения. После этого останавливается `ResourceSystem` и освобождается `InstanceLock`.

| Код | Значение |
|---:|---|
| 0 | Успешный выход, `--frames` после N успешных презентаций или активация уже работающего экземпляра |
| 1 | Ошибка ОС при acquire singleton lock, ошибка окна/renderer/runtime, паника потока или timeout аварийного завершения render/scripts |
| 2 | Ошибка CLI, layout/bootstrap, проекта, ресурсов, сцены, icon или загрузки Lua до окна |
| 3 | Vulkan probe/device или системные RAM/CPU требования; stderr и системный диалог |
| 4 | Экземпляр уже запущен, но активация не удалась (включая timeout; ошибка ОС при acquire возвращает 1) |

Ошибки с кодами 1 и 2 печатаются только в stderr. Для кода 3 runtime дополнительно показывает `show_error_dialog` с конкретными причинами.

## Источники

| Файл | Ответственность |
|---|---|
| `engine/engine-runtime/src/lib.rs` | `run_player`, layout startup, resources, project lock, hardware check и lifecycle модулей |
| `engine/engine-runtime/src/cli.rs` | `--project`, `--frames` и usage |
| `engine/engine-runtime/src/layout.rs` | release/dev пути |
| `engine/engine-runtime/src/activation.rs` | `PendingActivation` и single-instance handshake |
| `engine/engine-runtime/src/hardware.rs` | чистое решение по device/RAM/CPU требованиям |
| `engine/engine-runtime/src/module_host.rs` | именованные потоки, stop/join в обратном порядке, panic collection и detach |
| `engine/engine-runtime/src/scene.rs` | CPU-preload сцены и загрузка GPU-таблицы ресурсов |
| `engine/engine-runtime/src/pipeline.rs` | snapshots, script/render loops, fakeable backend и status guard |
| `engine/engine-runtime/src/player.rs` | `WindowHandler`, renderer handoff и shutdown окна |
| `engine/rve-player/src/main.rs` | бинарный entry point |

## Зависимости и взаимодействия

- Использует `engine-project` для `BootstrapConfig` и `ProjectSettings`, [[Engine/EngineResources]] и [[Engine/EngineAssets]] для загрузки содержимого.
- Использует [[Engine/EngineFiles]] для нормализации Windows filesystem path в `Layout`.
- Использует [[Engine/EngineScene]] для `SceneDocument`, `World`, камеры и света; [[Engine/EngineScript]] остаётся на scripts thread и не становится `Send`.
- Передаёт окно через [[Engine/EngineWindow]], общую Vulkan-работу через [[Engine/EngineRender]] и платформенную подготовку, диалог и singleton API через [[Engine/EnginePlatform]].
- Renderer создаётся на главном потоке в `init`, затем передаётся render thread. Менять зависимости и поведение upstream-крейтов для runtime не требуется.
- Пустые `engine-audio`, `engine-physics`, `engine-network`, `engine-neural` запускаются как отдельные управляемые потоки; доменной работы пока не выполняют.
