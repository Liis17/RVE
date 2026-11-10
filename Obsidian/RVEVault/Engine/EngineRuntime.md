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

На Windows `runtime.fsr_library` задаёт путь к FSR loader DLL: dev-player читает его
относительно каталога проекта, release-player ищет файл с тем же именем в `Engine/`.
`rve-build build` помещает туда также соседний `amd_fidelityfx_upscaler_vk.dll`.
Если loader, effect или поддержка Vulkan недоступны, запрос FSR переключается на TAA
и выводит одно предупреждение. На macOS FSR недоступен.

Windows `runtime.dlss_library` указывает на проектную `nvngx_dlss.dll`: dev-player
разрешает путь от проекта, release-player ищет файл по имени в `Engine/`. Для NGX
runtime `RendererConfig::upscalers` также получает один из двух идентификаторов:
GUID-like `dlss_project_id` вместе с `dlss_engine_version` либо выданный NVIDIA
`dlss_application_id`. Если player собран с feature `dlss`, renderer запрашивает
NGX instance/device extensions до их создания и проверяет DLSS support выбранного
физического устройства. При отсутствующей DLL, SDK feature или поддержке Vulkan
запрос `Dlss` переходит на TAA и пишет одно предупреждение. На macOS DLSS недоступен.

Параметр `--scripts <path>` задаёт ABI-библиотеку Rust-скриптов. В release layout,
если параметр не задан, runtime выбирает `Engine/game_scripts.dll` на Windows или
`Engine/libgame_scripts.dylib` на macOS, только если файл существует. В dev-режиме
`--project` автоматического поиска нет: библиотеку передают через `--scripts`.
Параметр разрешён и в release-режиме и перекрывает стандартный путь. Повтор,
отсутствующее значение и значение, начинающееся с `--`, являются ошибкой CLI с
кодом 2. Относительный путь `--scripts` разрешается от текущего рабочего каталога и
приводится к абсолютному; существующие пути нормализуются по правилам платформы.

`--debug-view lit|wireframe|normals|overdraw|lod|cascades` доступен только вместе с
`--project`; неправильный режим и применение к release layout завершаются кодом 2.
Dev-player с developer console циклически меняет поддерживаемые режимы клавишей F8;
режим пропускается, если окно не в фокусе или консоль открыта. Wireframe включается в
цикл только при поддержке `fillModeNonSolid`; F8 не передаёт клавишу скриптам.

Заголовок окна берётся из bootstrap, затем из `ProjectSettings.name`. `AppIcon`
загружается через `Assets::load_texture` до создания окна; ошибка загрузки относится
к коду 2. Начальный размер окна берётся из bootstrap с defaults `WindowConfig`.

Список тяжёлых префабов читается тем же способом: dev — `<project>/prefabs.rveprefabs` (нет файла — пустой список), release — `/rve/prefabs.toml` через `ResourceHandle` и обязателен. Ошибка чтения или формата даёт код 2; формат описан в [[Engine/EngineProject]].

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

`ModuleHost` начинает владеть потоками при их первом старте. Он запускает `rve-audio`, `rve-physics` и `rve-neural`, а при сетевых флагах также `rve-network` ([[#Сеть и rve-server]]), затем загрузчик тяжёлых префабов `rve-prefabs` ([[#Тяжёлые префабы]]) и именованный поток `rve-scripts`. На этом потоке runtime сначала загружает `ScriptLibrary` (если задан путь), затем создаёт `ScriptHost::load_with_options` с библиотекой и `ScriptLoadOptions`: sticky `ScriptInterruptHandle` и лимит 5 секунд на каждый Lua call, включая top-level chunk. После загрузки поток сообщает `Ready` до создания окна. Ожидание readiness ограничено 30 секундами; при timeout runtime устанавливает interrupt и ждёт ещё 2 секунды. Если ответа всё ещё нет, runtime возвращает код 1 и отсоединяет `rve-scripts` перед `ModuleHost::shutdown`; load error сохраняет код 2, а ошибка запуска потока или паника — код 1. `ScriptLibrary` намеренно не выгружает динамический модуль.

После загрузки CPU-моделей, но до запуска модулей и создания окна, `PreparedScene::build_physics` строит `PhysicsSceneDesc` по `ProjectSettings.physics` и геометрии уже загруженных базовых FBX-мешей. Hull/BVH и `StepDriver` строятся один раз; ошибка формы или геометрии возвращается с кодом 2 до окна. Runtime заранее формирует sequence 0 с immutable `QuerySnapshot`, затем запускает `PhysicsServer::run_preinitialized` как `rve-physics` в `ModuleHost`. `ScriptThreadConfig` передаёт клиент и initialization в `rve-scripts`; `ScriptHost` получает snapshot и body bindings до `Ready` и до `on_start`. Stop-handler physics thread отправляет Shutdown, а `ModuleHost` join-ит его в обратном порядке запуска при штатном закрытии и ошибках старта.

Аниматоры: при загрузке GPU-таблицы сцены на главном потоке `upload_scene` создаёт экземпляры skinning по одному на часть меша уровня 0 для объектов с `animator`, у модели которых есть скелет. Таблица хранит модели и клипы для моста (`animation_models`, `animation_clips`). На потоке `rve-scripts` `build_animation` создаёт `AnimationBridge`; в каждом тике после `update` запросы скриптов применяются (`apply`), затем мост продвигается на `dt`, а события `animation_finished` возвращаются скриптам через `queue_animation_events`. Палитры копируются в `FrameSnapshot`, объекты с экземплярами рисуются `DrawCommand.skin`, объекты без `animator` — статически. Предупреждения моста пишутся в журнал с префиксом `[animation]`. Объект, чья палитра моста не совпала с числом костей меша, теряет skinning при старте сцены с предупреждением и рисуется статично. Выгрузки сцены в runtime нет: при завершении render-потока `RenderBackend::release_scene` освобождает экземпляры skinning сцены, затем `Renderer` уничтожает остальные ресурсы. Заспавненные объекты (`spawn` и префабы) аниматор не получают: `prepare_template` и подготовка быстрых префабов не читают его клипы, мост построен только по объектам, загруженным со сценой, а `play_animation` на заспавненном объекте молча пропускается. Об этом один раз на шаблон выводится предупреждение `[animation]` при подготовке префаба с `animator`.

После Ready `engine-window` запускает `WindowConfig` с `RedrawMode::OnDemand`. `WindowHandler::init` на главном потоке создаёт `Renderer`, загружает модели, текстуры, environment maps и immutable GPU materials, запускает `rve-render` и отправляет scripts сообщение `Go` с GPU-таблицей `(MeshId, MaterialId)` по каждому уровню объекта и кэшем environment inputs. Неизменённые FBX materials переиспользуются между объектами; object overrides создают материалы для соответствующих частей. Кэш GPU-текстур учитывает color space, поэтому одна CPU-текстура при разных ролях загружается отдельно как sRGB и linear. Blend materials попадают в отдельный HDR pass renderer и сортируются по мировой bounds относительно камеры; skybox пишется между opaque и blend pass; остальные детали pipeline описаны в [[Engine/EngineRender]]. Render thread — единственный владелец renderer после передачи; каждый `FrameSnapshot` несёт активную камеру (`view`, `projection`, position), её проверенные `.camerasettings`, directional light, отображённые point/spot lights, sparse local shadow matrices с индексами исходных источников, необязательный environment (renderer-local id, intensity, owner rotation, skybox) и выбранные draw-команды. Local light цвет умножается на интенсивность в `f64` с насыщением до `f32::MAX` при переполнении; редактор применяет ту же политику. Углы spot переводятся в cosines. При `shadows = true` runtime вычисляет point/spot матрицы через общие функции `engine-scene::light`; near/range и 90° spot boundary policy описаны в [[Engine/EngineScene]]. Невалидная матрица оставляет свет без тени, не выключая само освещение. При смене HDR source или процедурных параметров renderer запекает environment один раз; для не изменившегося источника descriptor/GPU ресурсы не пересоздаются. У процедурного источника cache key квантует направление солнца, чтобы небольшое движение directional light не запускало bake каждый кадр. Camera-relative cascades вычисляются из camera, aspect, `LightState.shadow` и разрешения карты для directional light. `Frame` принимает `FrameCamera`, local lights, `LocalLightShadow` и необязательный `FrameEnvironment`; детали IBL и uniform layout описаны в [[Engine/EngineRender]].

## Конвейер и отменяемые ожидания

После каждого отправленного physics Step `rve-scripts` получает его результат через `PhysicsClient::receive`, который ждёт только при outstanding Step и одновременно выбирает physics reply или control message. Shutdown не отбрасывает уже отправленный Step: поток забирает результат и применяет dynamic poses через `PhysicsSceneSync`, обновляет snapshot и ставит события в `ScriptHost`. Скриптовые сообщения и physics events проходят по существующему порядку начала update. После update runtime объединяет physics commands с изменёнными позами мира и отправляет следующий Step с тем же monotonic `dt`; изменения transform static/dynamic тел становятся Teleport, kinematic и character — KinematicTarget. В Background симуляция продолжает получать Step с тем же временем, хотя новые render snapshots не строятся. Ошибка канала, worker или синхронизации задаёт exit code 1.

Scripts thread выполняет `start`, затем цикл `update(dt)` → `World::build_frame` → `FrameSnapshot`. Снимок включает выбранную активную камеру, её settings и постобработку (см. [[Engine/EnginePostProcess]]), мировую позицию камеры для specular view-вектора и skybox, directional light, все локальные point/spot состояния, матрицы затеняемых источников, необязательный environment state и draw-команды с material IDs объектов. Для каждого видимого объекта снимок хранит модель и диапазон его draw-команд по `ObjectHandle`. Перед каждой попыткой render thread выставляет предыдущую модель из последнего успешно отправленного renderer кадра; текущие модели и активная камера фиксируются только после успешного queue submit. Поэтому повторный render последнего снимка использует текущую модель как предыдущую, а объекты, отсутствующие в отправленном кадре, удаляются из истории. Pause, render error и пропуск до submit не продвигают историю; `OUT_OF_DATE` при present после успешного submit продвигает её. Смена активной камеры сохраняет reset temporal history до первого успешного submit с новой камерой и не повторяет reset в последующих кадрах. Телепортация объекта скриптом отдельно не распознаётся: большая разница матриц может дать большой motion vector на один кадр. LOD выбирается по расстоянию от camera position до мировой позиции объекта; `cull_distance` исключает объект из draw list и его shadow draws. Renderer выполняет culling мешей по bounds отдельно для камеры и каждой directional/local shadow view; сначала отбирает до 1024 локальных источников для camera frustum, затем назначает им до 32 shadow layers по расстоянию камеры (point = 6 слоёв, spot = 1) с исходным индексом как tie-break. LOD distance использует мировой origin объекта без учёта масштаба, размера на экране и hysteresis. Время приходит из монотонного `Clock`; `dt` отражает фактически прошедшее время. Снимки передаются через `flume::bounded(1)`, а отправитель выбирает между готовностью очереди и control-каналом через `Selector`, поэтому заполненная очередь не блокирует Shutdown. Экранные шейдеры вида берутся из `SceneFrame::screen_shaders` активной камеры: `FrameSnapshot::screen_shaders` содержит только включённые проходы с созданным шейдером. Исходники `.glsl` читает игровой клиент при загрузке сцены (`load_screen_shader_sources`), `rve-server` их не открывает; шейдеры создаются в `upload_scene` и освобождаются с выгрузкой сцены ([[Engine/EngineScreenShaders]]).

Оконный поток передаёт события в `Arc<Mutex<InputAccumulator>>` короткими операциями. Перед каждым `ScriptHost::update` scripts thread забирает `take_frame`: удерживаемые клавиши и кнопки сохраняются, pressed/released и накопленные mouse/wheel delta сбрасываются. `ScriptInput` копируется в host, поэтому все экземпляры и message callbacks одного update видят один снимок. Потеря фокуса и вход в Background освобождают все удерживаемые клавиши и кнопки с `released` edge. Фоновый snapshot не содержит удерживаемого ввода, кроме ещё не выданных `released` от сброса; новые события в Background отбрасываются. До первого `CursorMoved` позиция курсора равна `[0, 0]`; `CursorLeft` не обрабатывается, поэтому последняя позиция сохраняется после выхода из окна, потери фокуса и перехода в Background. Mouse/wheel delta сбрасываются.

Геймпады опрашиваются на scene-потоке: `engine-input` забирает события перед `take_frame`, снимок слотов проходит фокусный гейт `PadGate` внутри `InputAccumulator`, и в `ScriptInput` попадают только принятые пады. Вибрация из скриптов отдаётся после `update` и только пока ввод принят; потеря фокуса, Background и выход из цикла её останавливают. Бэкенд создаётся с настройками `[input.gamepad]` проекта. Выделенный сервер (`headless`, `rve-server`) не открывает устройства: scene-поток получает бэкенд без устройств (`gamepad_devices = false`). Тесты используют бэкенд без устройств.

Render thread выбирает между snapshots и resize/occlusion/shutdown control-каналом. `view_projection` пересчитывается по последнему размеру окна. Нулевой размер или occlusion приостанавливает отрисовку; render отбрасывает уже поставленный снимок, а scripts получают Background и продолжают вызывать `update` с частотой `ProjectSettings.runtime.background_tick_hz` ([[Engine/EngineProject]]), не собирая и не отправляя новые snapshots. При смене в Background накопитель освобождает клавиши/кнопки и выдаёт `released` один раз; фоновые тики получают только эти края, без удерживаемого ввода, но с последней известной позицией курсора и нулевыми дельтами. Клавиша, зажатая во время Background (или до получения фокуса), после возврата не считается удерживаемой, пока её не нажмут снова. Если `Background(true)` пришёл до `Go` (например, при нулевом начальном размере окна), scripts запоминают последнее значение до handshake и начинают сразу в соответствующем режиме. Ожидание следующего фонового тика выполняется через `recv_timeout` и injected clock, без `sleep`. Восстановление окна возвращает scripts в обычный цикл и публикацию snapshots.

`drain_events` выводится runtime: Info в stdout, Error в stderr с префиксом `[script]`. При штатном shutdown scripts thread сначала вызывает `destroy` (`on_destroy`), после чего уничтожает `ScriptHost` и экземпляры на том же потоке; runtime ждёт его завершения до 2 секунд. При timeout `ScriptInterruptHandle` прерывает зависший lite Lua-код; затем runtime ждёт ещё до 2 секунд через stopped-канал. Если scripts всё ещё не остановились (например, ABI-скрипт блокирует поток), runtime отсоединяет handle без `join`, пишет ошибку и возвращает код 1. Interrupted lite callbacks не получают `on_destroy` согласно контракту [[Engine/EngineScript]]. При ошибке или панике renderer interrupt подаётся сразу.

## Материалы сцены

load_scene и load_prefab_template через engine/engine-runtime/src/scene.rs подготавливают .rvemat, .rveshader и texture dependencies из scene assets. На render thread upload_scene создаёт Surface shaders и GPU materials; одинаковые shader paths переиспользуют один ShaderId. GPU-материалы кэшируются по разрешённым входным данным и пути модели: объекты, LOD и части с одинаковым результатом получают один MaterialId; материалы живут до завершения рендера. Per-part слот выбирается по индексу FBX mesh и тому же индексу LOD mesh. MeshRenderer.texture и legacy MeshRenderer.material применяются после .rvemat только для встроенной Lit schema; custom shader читает объявленные schema properties. Полный формат и precedence — в [[Engine/EngineMaterials]].

Подготавливаются все объявленные в `[[assets]]` `.rvemat` сцены и шаблонов префабов (`prepare_scene_with_model_loader`, `prepare_template`), а не только слоты: скрипт назначает их через `set_material`.

Замена материала скриптом (`MaterialSync` в `scripts_thread`, `pipeline.rs`): `World` отдаёт изменённые объекты (`take_mesh_material_changes`), на каждый объект уходит не больше одной заявки `RenderResourceRequest::Material` с текущими уровнями `GpuSceneTable.by_object`, CPU-описанием модели, переопределениями и путём. `set_material` принимает `.rvemat`, объявленный в ассетах сцены либо в шаблоне prefab этого объекта. Объекты сцены и быстрых prefab используют подготовленные ресурсы сцены. Для тяжёлого prefab `PrefabReady` передаёт CPU-описания уровней, а render thread хранит его `PreparedMaterialAssets` по `PrefabToken`; при разрешении слота проверяются ресурсы prefab и затем общие ресурсы сцены. Эти CPU-ресурсы удаляются при отложенном `Release` точного токена.

Render thread (`VulkanBackend::submit_material`) вызывает `upload_object_levels`; текстуры и шейдеры переиспользуются через `MaterialUploadCaches`. Зависимости созданных материалов учитываются по `MaterialId` и освобождаются после отложенного `ReleaseMaterials`, когда последний snapshot перестал ссылаться на материал; ресурсы сцены закреплены до завершения renderer. Владение и освобождение ресурсов описаны в [[#Тяжёлые префабы]]. Меши остаются прежними. Только успешный ответ `ScriptReply::Material` заменяет уровни объекта; изменение ревизии во время заявки ставит повтор. Частичные GPU-материалы уничтожаются при ошибке upload, а прежние уровни сохраняются. Созданные материалы принадлежат объекту (`MaterialOwnership`): при следующей замене или удалении объекта они освобождаются через `ReleaseMaterials` после snapshot с `last_seq = frame_number`; материалы загрузки сцены не входят в это владение, потому что могут быть общими. Ответ для уже удалённого объекта освобождает новые материалы сразу. Остаточные renderer-ресурсы уничтожаются при завершении renderer. Headless-ответчик отвечает ошибкой.

Ошибка чтения/разбора .rvemat, shader, texture dependency, runtime GLSL compilation или GPU material creation журналируется и заменяет слот magenta Lit error material без panic. Успешная shader generation остаётся живой при неудачной editor hot reload. Headless rve-server не создаёт renderer и не компилирует GLSL; builder валидирует asset structure и dependencies отдельно ([[Engine/RveBuild]]).

## Быстрые префабы и spawn

`load_scene` (`engine/engine-runtime/src/scene.rs`) после `World::from_document` читает `.rvephysmat` коллайдеров сцены (`World::resolve_physics_materials`); шаблоны префабов получают разрешённые материалы в `load_prefab_template`. Ошибка чтения или разбора материала — ошибка загрузки сцены или префаба. `load_scene` строит `PrefabTemplate` для каждого `.rveprefab` из `[[assets]]` раскрытой сцены, даже без экземпляров. Ассеты шаблонов добавляются в ассеты сцены, поэтому `World::asset_paths` и звуковой мост видят их как ассеты сцены. Шаблон, не прошедший `PrefabTemplate::check_spawnable` (`camera`, `directional_light`, `environment`), в каталог spawn не попадает и ошибкой загрузки не является: такой префаб остаётся пригодным экземпляром сцены, поэтому префаб с окружением грузится. Если он объявлен в `[[assets]]`, но ни один экземпляр его не использует, при загрузке пишется Warn. Ошибки чтения и разбора шаблона по-прежнему дают код 2. `max_instances` берётся из записи сцены. Шаблоны и их CPU-ресурсы (FBX, PNG, замены материалов, LOD, Lua-исходники) готовятся тем же кодом, что объекты сцены. `upload_scene` кладёт уровни шаблонов в `GpuSceneTable.prefabs` по пути и индексу объекта шаблона. Клипы звука шаблонов предзагружаются вместе со сценой.

Scripts thread регистрирует каждый быстрый шаблон до `Ready` (`ScriptHost::register_prefab`); ошибка даёт код 2. После `start`, console-команд и `update` вызывается `apply_world_changes` до `sync_world_commands`. Она берёт `drain_world_changes` и:
- переносит уровни шаблона на объекты поддерева и удаляет записи удалённых объектов;
- снимает тела удалённых объектов (`Remove`), затем вставляет тела новых (`Insert`) и добавляет `SetVelocity` для скорости spawn; всё уходит в ближайший `step_with_changes`;
- передаёт звуку `StopSource` для объектов, ставших неактивными, и `PlaySource` для spawn-объектов с `play_on_start`; объекты, снова ставшие активными, тоже получают `PlaySource`, если у них `play_on_start` (`engine_scene::activity_audio_requests`, та же логика в editor play).

Если физика отвергла вставку (например, нулевой масштаб), `apply_world_changes` вызывает `ScriptHost::revert_spawn`: поддерево удаляется из `World` вместе с экземплярами скриптов, Error из отката печатается сразу. Удаления не попадают в следующий `drain_world_changes`, поэтому GPU и физика не получают второе удаление. Тесты без GPU проверяют физику и откат, а draw-команды по GPU-таблице сквозь `make_snapshot` не проверяются, потому что `MeshId` нельзя создать вне engine-render. Тяжёлые префабы описаны в [[#Тяжёлые префабы]].

## Тяжёлые префабы

Записи `prefabs.rveprefabs` ([[Engine/EngineProject]]) читаются в `run_from_args` до запуска модулей. Путь, который сцена уже объявила быстрым, в список не попадает: быстрый префаб приоритетнее. Остальные записи добавляются в `PrefabCatalog` со `PrefabKind::Heavy` и `max_instances` из записи. Скрипты создают такой префаб через тот же `rve.spawn`; до готовности его spawn-хендл pending.

Загрузка идёт по цепочке потоков (`engine/engine-runtime/src/prefabs.rs`):
- Scripts thread обрабатывает `PrefabRequest::Load` в `apply_world_changes` и через `PrefabLoads` ставит загрузку с новым токеном `(путь, load_id)`; `load_id` растёт, поэтому старая и новая загрузка одного пути различимы.
- Поток `rve-prefabs` вызывает `load_prefab`: `PrefabTemplate::load`, `check_spawnable` и общая подготовка `prepare_template` (модели, текстуры, Lua-исходники, LOD). Прерывание проверяется перед каждым объектом, моделью и исходником. Геометрия физики строится здесь только для моделей мешевых коллайдеров шаблона (`prefab_collider_geometry`, пути — `PrefabTemplate::mesh_collider_models`); остальные модели нужны только render. Формы `ShapeCache` строятся лениво на scripts thread при первом spawn. Ошибка уходит в scripts как `Prefab(Failed)`, успех — в render как `Upload`.
- Render thread ставит задание в `PrefabGpu::submit` и выполняет до `PREFAB_UPLOAD_BUDGET` шагов (мешей и материалов) за итерацию цикла. Пока есть незавершённая загрузка, цикл не блокируется; загрузка продолжается и на паузе. Готовый префаб возвращается scripts как `Prefab(Ready)` с GPU-уровнями и CPU-частью (шаблон, исходники, геометрия).
- Scripts thread при `Ready` с токеном не текущим сразу шлёт `Release`. Текущий токен регистрируется (`register_prefab`), получает уровни в `GpuSceneTable.prefabs`, геометрия добавляется в `SpawnPhysics`. Ожидающие spawn переходят в очередь хоста и применяются в следующем `update`. Ошибка — `prefab_failed`: pending-хендлы умирают с Error-событием.

Владение GPU-ресурсами. Модель (меши с материалами по умолчанию и текстуры этих материалов) подсчитывается ссылками токенов, поэтому два префаба с одним FBX делят GPU-меши. Материалы переопределений и ссылки на текстуры принадлежат токену; освобождение уменьшает счётчики, ресурс уничтожается при нуле. Ресурсы сцены и быстрых префабов в этом учёте не участвуют и живут до конца сцены. Пост-spawn замены материалов (`VulkanBackend::upload_object_materials`) используют общий `MaterialUploadCaches`: уже загруженные ресурсы сцены закреплены до завершения сцены, а зависимости каждого нового материала учитываются по `MaterialId`. `ReleaseMaterials` сначала ждёт снимки по `last_seq`, затем уничтожает материал и снимает ссылки на его текстуры и shader. Общая зависимость остаётся, пока на неё ссылается другой живой материал; уникальная запись кэша удаляется после последней ссылки. `release_prefab` освобождает CPU-источники и ресурсы исходной загрузки, но не снимает зависимости материалов, ожидающих отложенного освобождения.

Выгрузка явная: `rve.prefabs.unload` после последнего экземпляра даёт `PrefabRequest::Unload`, автоматической выгрузки нет. Scripts thread убирает `GpuSceneTable.prefabs[путь]`, уменьшает ссылки на геометрию (при нуле удаляются геометрия и `ShapeCache::remove_model`), затем шлёт `RenderResourceRequest::Release { token, last_seq }`, где `last_seq` — номер последнего реально отправленного `FrameSnapshot`, и вызывает `prefab_released`. Render thread уничтожает ресурсы токена, когда принял снимок с `seq ≥ last_seq` или когда снимков с `seq ≤ last_seq` больше не будет; удерживаемый для перерисовки снимок с `seq ≤ last_seq` при этом сбрасывается. Поэтому освобождение происходит и на паузе, и в Background, без новых снимков. Ресурсы адресуются токеном, а не путём, поэтому освобождение старой загрузки не трогает ресурсы новой.

Тесты без GPU: учёт ресурсов проверяется подменой `GpuResources`, а поток скриптов и цикл рендера — ручными ответами загрузчика и `FakeBackend`. Draw-команды тяжёлого объекта в тестах не проверяются: `MeshId` нельзя создать вне engine-render.


## NVIDIA Reflex

`player.rs` передаёт `Reflex` из bootstrap в `RendererConfig::low_latency` и после создания renderer берёт `latency_handle()`. Handle отправляется скриптовому потоку как `ScriptControl::Latency(Arc<dyn FrameLatency>)` строго перед единственным `ScriptControl::Go`; `Go` не меняется. `FrameLatency` в `pipeline.rs` — узкий интерфейс для тестов, реализованный на `LatencyHandle`.

- Скриптовый цикл: `sleep()` вызывается до `clock.now()`, затем `SimulationStart`, `InputSample` перед `take_frame()`, `SimulationEnd` после `make_snapshot`. Идентификатор кадра — `frame_number` (он же `snapshot.sequence`).
- В background-режиме ни sleep, ни маркеров нет: id кадра в этом режиме до present не доходит.
- Render thread перед `render_frame*` вызывает `set_latency_frame(snapshot.sequence)` в `VulkanBackend::render`.
- Без handle (Reflex выключен или не поддержан) цикл работает как прежде. Тесты: `reflex_sleep_and_markers_run_in_order_for_the_submitted_sequence`, `reflex_is_not_called_while_scripts_run_in_background`, `scripts_produce_frames_without_a_reflex_handle` (`pipeline.rs`).

## Сеть и rve-server

Сетевой режим включается флагами `rve-player` и `rve-server`. Без флагов мост не создаётся, поток `rve-network` не запускается, и поведение прежнее. Контракт протокола и моста — [[Engine/EngineNetwork]].

- `rve-player`: `--host` запускает listen-server с портом из `[network].port`; `--host-addr <addr:port>` задаёт адрес прослушивания. `--connect <addr:port>` требует `--server-fingerprint <hex>` или `--insecure-no-pin`. `--host` и `--connect` взаимоисключающие. `--server-key` допустим только с хостом, `--password-file` — с хостом или клиентом.
- `rve-server` (`engine/rve-server`) использует тот же разбор аргументов. Адрес прослушивания задаёт `--bind`, сервер всегда работает как хост. Флаги клиента и `--debug-view` недоступны.
- Сетевой флаг при `[network].enabled = false` в `project.rveproject` даёт код 2. Ошибка bind и ошибка запуска потока — код 1. Ошибка запуска сетевого моста на потоке скриптов тоже даёт код 1.
- Пароль берётся из `--password-file` или переменной `RVE_NET_PASSWORD` (контракт — [[Engine/EngineNetwork]]).
- Два `rve-player` на одной машине требуют `AllowMultipleInstances=true` в bootstrap: singleton lock по `ProjectSettings.id` берёт только player с окном.

Мост подключается до `ScriptHost::start` на потоке скриптов: `on_start` уже видит режим сети. Реализация `WorldSink` — `ScriptNetSink` в `engine/engine-runtime/src/net.rs`; шаги моста вызываются вокруг `update` и применения spawn и despawn (`pipeline.rs`).

### Headless-путь `run_server`

- Окна, renderer, проверка Vulkan-устройства, звуковой вывод (`AudioConfig.output = false`) и instance lock не используются. Проверка требований касается только RAM и CPU. Ошибки уходят в stderr без системного диалога, код выхода 3.
- Скрипты работают фоновым тиком с частотой `[network].tick_hz` вместо `background_tick_hz`. Снимки для renderer не строятся.
- Вместо render thread работает ответчик ресурсов (`engine/engine-runtime/src/headless.rs`). Он подтверждает загрузку тяжёлых префабов пустыми данными, чтобы `PrefabReply::Ready` приходил и не было зависания. Это проверяется тестом.
- Остановка: `--frames N` (число тиков скриптов), строка `quit` на stdin (штатный `Shutdown` без прерывания вызовов), фатальная ошибка потока или Ctrl+C. Ctrl+C завершает процесс без `on_destroy`.
- Каталог ресурсов: на macOS, если рядом с исполняемым файлом есть `.app` с `Contents/Resources/Engine`, используется он (`server_release_root`); иначе каталог исполняемого файла.

## Завершение и ошибки

`close_requested` посылает Shutdown scripts и render и задаёт дедлайн закрытия через 2 секунды. При выходе `cleanup` сначала останавливает загрузчик `rve-prefabs` через `ModuleHost::stop_named` и ждёт его не дольше 2 секунд; при timeout поток отсоединяется с предупреждением, и код выхода не меняется: штатное закрытие не считается ошибкой, код 1 даёт только паника потока при `join`. Только после этого останавливается render. `next_wakeup` учитывает этот дедлайн и вне dev-console, а при открытой консоли выбирает более ранний момент repaint или закрытия. Render дожидается Vulkan через уничтожение renderer на своём потоке, помечает `RenderStopped` и вызывает `WindowProxy::request_exit`. Если дедлайн истёк, пока render ещё работает, `update` записывает `поток рендера не завершился после закрытия окна` и завершает оконный цикл. В `exiting` render join-ится только после `RenderStopped`; иначе главный поток ждёт до дедлайна закрытия (ноль времени, если он уже истёк; до 2 секунд при другом выходе), затем при timeout отсоединяет render handle и выставляет код 1. Затем scripts ждут до 2 секунд; после interrupt выполняется ещё одно ограниченное ожидание до 2 секунд, а при timeout поток отсоединяется без `join` и код выхода становится 1. `ModuleHost` останавливает и join-ит оставшиеся модули в обратном порядке; паника потока или stop-handler становится ошибкой запуска/завершения. После этого останавливается `ResourceSystem` и освобождается `InstanceLock`.

| Код | Значение |
|---:|---|
| 0 | Успешный выход, `--frames` после N успешных презентаций или активация уже работающего экземпляра |
| 1 | Ошибка ОС при acquire singleton lock, ошибка окна/renderer/runtime, паника потока, timeout startup readiness или аварийного завершения render/scripts |
| 2 | Ошибка CLI, layout/bootstrap, проекта, ресурсов, сцены, icon или загрузки Lua/ABI-библиотеки до окна |
| 3 | Vulkan probe/device или системные RAM/CPU требования; stderr и системный диалог |
| 4 | Экземпляр уже запущен, но активация не удалась (включая timeout; ошибка ОС при acquire возвращает 1) |

Ошибки с кодами 1 и 2 печатаются только в stderr. Для кода 3 runtime дополнительно показывает `show_error_dialog` с конкретными причинами.

## Источники

| Файл | Ответственность |
|---|---|
| `engine/engine-runtime/src/lib.rs` | `run_player`, layout startup, resources, project lock, hardware check и lifecycle модулей; передача project input settings scripts thread |
| `engine/engine-runtime/src/cli.rs` | `--project`, `--scripts`, `--frames`, dev-only `--debug-view`, сетевые флаги `--host`/`--connect` и `--bind` для `rve-server`, usage |
| `engine/engine-runtime/src/net.rs` | `prepare`, `ScriptNetSink`, `attach`, `tick` | Сетевой план из флагов и проекта, сетевой поток и мост на потоке скриптов |
| `engine/engine-runtime/src/headless.rs` | `run_resource_responder`, `spawn_quit_listener` | Headless-путь `rve-server`: ответчик ресурсов и команда `quit` |
| `engine/rve-server/src/main.rs` | — | Бинарник выделенного сервера, вызывает `run_server` |
| `engine/engine-runtime/src/layout.rs` | release/dev пути и выбор ABI-библиотеки |
| `engine/engine-runtime/src/activation.rs` | `PendingActivation` и single-instance handshake |
| `engine/engine-runtime/src/hardware.rs` | чистое решение по device/RAM/CPU требованиям |
| `engine/engine-runtime/src/module_host.rs` | именованные потоки, stop/join в обратном порядке, panic collection и detach |
| `engine/engine-runtime/src/scene.rs` | CPU-preload сцены и загрузка GPU-таблицы ресурсов |
| `engine/engine-runtime/src/pipeline.rs` | snapshots, script/render loops, fakeable backend и status guard |
| `engine/engine-runtime/src/input.rs` | `InputAccumulator`, переходы состояния и кадровые input snapshots |
| `engine/engine-runtime/src/player.rs` | `WindowHandler`, F8 debug-view control, renderer handoff и shutdown окна |
| `engine/engine-runtime/src/cursor.rs` | `CursorControl`: состояние курсора скриптов и консоли, отправка в окно и отложенная отправка при подключении |
| `engine/engine-runtime/src/console.rs` | UI snapshots, latest-only slot и интеграция console-команд с scripts thread |
| `engine/rve-player/src/main.rs` | бинарный entry point |

## Зависимости и взаимодействия

- Использует `engine-project` для `BootstrapConfig` и `ProjectSettings`, [[Engine/EngineResources]] и [[Engine/EngineAssets]] для загрузки содержимого.
- Использует [[Engine/EngineFiles]] для нормализации Windows filesystem path в `Layout`.
- Использует [[Engine/EngineScene]] для `SceneDocument`, `World`, камеры и света; [[Engine/EngineAnimation]] для аниматоров и палитр; [[Engine/EngineScript]] остаётся на scripts thread и не становится `Send`.
- Передаёт окно через [[Engine/EngineWindow]], общую Vulkan-работу через [[Engine/EngineRender]] и платформенную подготовку, диалог и singleton API через [[Engine/EnginePlatform]].
- Pipeline cache приложения использует sanitized `ProjectSettings.name` под пользовательским каталогом кэша; renderer получает bytes до создания и сохраняет данные до Drop в render thread.
- Renderer создаётся на главном потоке в `init`, затем передаётся render thread. Менять зависимости и поведение upstream-крейтов для runtime не требуется.
- Поток `rve-audio` запускает звуковой движок с ресурсами проекта и устройством вывода; без устройства звук отключается с предупреждением, игра продолжается. Scripts thread связывает звук с кадром: `AudioBridge::start` после пересчёта матриц и `start` скриптов, `apply` запросов после `update`, `sync_positions` после `build_frame`; в background-режиме позиции не обновляются. См. [[Engine/EngineAudio]].
- `rve-neural` — управляемый поток без доменной работы. `rve-network` существует только при сетевых флагах и описан в [[Engine/EngineNetwork]]. `rve-physics` выполняет шаги физики, см. [[Engine/EnginePhysics]].
- Использует `engine-network` для сетевого потока и моста; sandbox сеть не использует.

## Игровые действия ввода

Runtime передаёт `ProjectSettings.input` в `ScriptHost` вместе с исходниками сцены.
Action evaluation выполняется внутри общего `engine-script::ScriptHost::set_input`,
поэтому сырые клавиши и действия используют один кадр событий. Потеря фокуса и
Background сохраняют raw `released` edges и сбрасывают analog deltas; вместе с ними
обнуляются mouse/wheel actions, а пады читаются как отпущенные с нулевыми осями. См. [[Engine/EngineProject]] и [[Engine/EngineScript]].

## Профайлер

- `RuntimeStatus::profile` — `ProfileBoard` с источниками `scripts`, `render`, `resources` и `startup`. Потоки публикуют отчёты не чаще раза в 500 мс; окна (`profiling.rs`) хранят последние замеры в памяти, выделенной при создании, без аллокаций в кадре.
- Поток сцены: ожидание физики, команды консоли, регистрация ответов префабов, `update` скриптов с разбивкой ([[Engine/EngineScript]]), применение изменений мира (`world changes`), синхронизация физики, сборка кадра, подготовка снимка и ожидание отправки.
- Поток рендера: ожидание снимка, ресурсные запросы префабов (`resource requests`: загрузка и освобождение бюджетными шагами), вызов backend, CPU-фазы и GPU-проходы первого вида из `take_frame_profile` ([[Engine/EngineRender]]) и группа `Draws (last frame)` — счётчики `DrawStats` последнего кадра интервала; интервал без кадров группу не публикует. Вместе с окнами он публикует группу `Resources` (источник `resources`): дельту `ResourceStats` за интервал, `in flight` и максимумы очереди и чтения с запуска ([[Engine/EngineResources]]).
- Старт: `StartupProfile` записывает фазы до окна (`hardware probe`, `window icon`, `scene load`, `physics build`, `physics module`, `scripts start`, `scripts ready`) и в `PlayerWindow::init` — `renderer` (создание `Renderer`) и `GPU upload` (`upload_scene`: модели, текстуры, материалы). Отчёт публикуется после GPU upload, а в лог одной записью пишется при первом показанном кадре. Окружение (IBL) в отчёт старта не входит: `Renderer::create_environment` вызывается лениво на render-потоке при первом кадре с окружением, и его время попадает в фазу `render backend`.
- С `dev-console`: F3 переключает оверлей профайлера. Оверлей работает при закрытой консоли, не блокирует игровой ввод и обновляется раз в 500 мс. Команда `profile` выводит отчёты в консоль. Она перехватывается главным потоком до отправки скриптам и имеет приоритет над одноимённой командой скрипта; при затенении в лог пишется одно предупреждение. Строка `profile` добавлена в каталог команд для help и автодополнения. Таблица рисуется `engine_ui::profiler`.

## Активность объектов

- Перед применением spawn и destroy `apply_world_changes` вызывает `apply_activity_changes`: тела неактивных объектов удаляются, тела активированных создаются заново, звук неактивных источников останавливается. Расписание `on_enable`, `on_disable` и `on_start` ведёт `ScriptHost` ([[Engine/EngineScript]]).

## Курсор игрока

`engine/engine-runtime/src/cursor.rs` (`CursorControl`) передаёт в окно состояние курсора, которое задают скрипты. Поток скриптов переводит `ScriptCursor` в `engine_window::CursorState` без точки удержания, то есть удержание идёт в центре окна. Эффективное состояние — обычный курсор, пока открыта dev-консоль, иначе запрос скриптов. В окно уходит только изменившееся эффективное значение; запрос, записанный до подключения окна, отправляется при подключении. Отправка идёт вне замка данных, но под отдельным замком порядка, чтобы состояния приходили в том порядке, в котором менялись.

Начальное состояние задаётся до `start` из флагов `[objects.camera.cursor]` активной камеры (`World::active_camera_cursor`). Флаги неактивных камер не применяются. После `start`, каждого `update` и выполненной консольной команды поток скриптов публикует курсор, если он изменился. Освобождение курсора при выходе и потере фокуса выполняет окно ([[Engine/EngineWindow]]).

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

## GPU-частицы

- Загрузка: `prepare_scene` кладёт PNG эмиттера в `PreparedRenderAssets.textures` и `PreparedRenderAssets.particles`. `upload_scene` создаёт sRGB-текстуру и эмиттер через `create_particle_emitter`; результат попадает в `GpuSceneTable.particles` (объект → `ParticleEmitterId`) и при старте передаётся `VulkanBackend::with_particle_emitters`: дальше render thread держит эмиттеры по `ObjectHandle`. Сопоставление компонента с `ParticleEmitterDesc` выполняет локальная функция `particle_emitter_desc`, она покрыта тестами.
- Время: поток скриптов копит `sim_time` по `dt` и кладёт его в `FrameSnapshot.sim_time`; `make_snapshot` заполняет `FrameSnapshot.particles`. Render-поток считает `particle_step = sim_time − last_submitted_sim_time`; первый кадр получает шаг 0. `last_submitted_sim_time` обновляется только при `FrameOutcome.submitted`, поэтому неотправленный снимок не продвигает симуляцию. Консольный кадр частиц не содержит. Команды эмиттеров из скриптов (`particles_restart`, `particles_emit`) собираются в `scripts_thread` и попадают в `FrameSnapshot.particle_commands`. Render thread копирует их при приёме снимка в очередь `render_loop` и применяет через `RenderBackend::apply_particle_commands` ровно один раз перед отрисовкой кадра: пауза откладывает применение, перерисовка последнего снимка и его замена не повторяют и не теряют команды. `ParticleSnapshot.emitting` берётся из `ParticleEmitterInstance.emitting` (`particles_play`/`particles_stop`).
- Spawn и destroy: `apply_changes` после физики шлёт `ParticleRequest::Create` для объектов поддерева с описанием из `GpuSceneTable.particle_templates` (быстрые шаблоны — при `upload_scene`, тяжёлые — при `Ready`), а `Destroy` со всеми удалёнными хендлами через `RenderResourceRequest::Particle`. Render thread применяет заявки по хендлу; повтор и поздний `Destroy` безопасны. Текстуры эмиттеров тяжёлого префаба создаются в `submit_prefab` и освобождаются в `release_prefab`.

## Игровой UI

Runtime загружает документы и их ассеты из ресурсов проекта, инициализирует UI registry вместе со сценой, шагает его ввод на scene-потоке и помещает полученный paint в кадр render thread. Источники/текстуры UI синхронизированы с ресурсами проекта; GPU image/atlas lifecycle обслуживает `GameUi`. Контракт и границы компонентов — [[Engine/EngineGui]].
