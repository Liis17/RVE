# EngineScreenShaders

Parent: [[Index]]

## Назначение

Пользовательские экранные шейдеры камеры: полноэкранные fragment-проходы GLSL, которые автор игры добавляет к камере (выделение цвета, VHS, старый ТВ). До четырёх проходов на камеру, каждый стадии `before_post` или `after_post`, включается и выключается отдельно и управляется из скриптов. Работают только в `DebugView::Lit`. Без включённых шейдеров рендер не записывает ни одного прохода и не выделяет память под них.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-render/src/screen_shader.rs` | `compile_screen_shader`, `ScreenShaderStage`, `ScreenShaderError`, `check_interface`, `screen_push_constants`, `ScreenShaders`, `ScreenShaderPass`, `ScreenShaderPipelines`, `ScreenShaderScratch`, `record_screen_chain` | Prelude, компиляция naga, проверка IR, упаковка push constants, ресурсы и запись проходов |
| `engine/engine-render/src/post.rs` | `ScreenChain`, `PostFrame::screen_before`, `PostFrame::screen_after` | Отбор проходов стадии для вида (без Vulkan) |
| `engine/engine-render/src/resources/api.rs`, `engine/engine-render/src/profile.rs`, `engine/engine-render/src/passes/postprocess.rs`, `engine/engine-render/src/types.rs` | `Renderer::create_screen_shader`, `Renderer::destroy_screen_shader`, `GpuPass::ScreenShaderBefore`, `GpuPass::ScreenShaderAfter`, `record_tonemap_pass`, `ViewSettings::screen_shaders` | API renderer, встраивание в кадр и перепривязка output-набора |
| `engine/engine-scene/src/document.rs` | `ScreenShaderRef`, `ScreenShaderStage`, `validate_screen_shaders`, `MAX_SCREEN_SHADERS`, `MAX_SCREEN_SHADER_PARAMS` | Формат в `CameraComponent::screen_shaders` и проверка ссылок |
| `engine/engine-scene/src/world.rs` | `World::screen_shaders`, `World::set_screen_shader`, `SceneFrame::screen_shaders` | Состояние камеры в мире и выдача активной камеры кадру |
| `engine/engine-scene/src/prefab.rs` | проверка override камеры | Тот же контракт для камер в префабах и их override |
| `engine/engine-runtime/src/scene.rs` | `screen_shader_paths`, `load_screen_shader_sources`, `ScreenShaderGpu`, `upload_screen_shaders`, `PreparedRenderAssets::screen_shaders` | Чтение исходников и создание шейдеров при загрузке сцены |
| `engine/engine-runtime/src/pipeline.rs`, `engine/engine-runtime/src/pipeline/snapshot.rs` | `screen_shader_view`, `enabled_screen_passes`, `GpuSceneTable::screen_shaders`, `FrameSnapshot::screen_shaders` | Маппинг кадра в `ViewSettings`; освобождение при замене сцены по живым путям камер и при выгрузке |
| `engine/rve-sdk/src/abi.rs`, `engine/rve-sdk/src/lib.rs` | `RveScreenShaderState`, `RveHost::screen_shader_count`, `get_screen_shader`, `set_screen_shader` | Script ABI 14 и гостевые методы камеры |
| `engine/engine-script/src/abi_host.rs`, `engine/engine-script/src/transaction.rs`, `engine/engine-script/src/lite.rs` | `host_get_screen_shader`, `ScriptTransaction::set_screen_shader`, Lua `screen_shader` | Хост ABI, очередь транзакции и Lua-API |
| `editor/src/viewport/render.rs`, `editor/src/ui/inspector.rs`, `editor/src/scene_edit.rs`, `editor/src/app.rs` | `SceneRenderer::sync_screen_shaders`, `reload_changed`, `show_screen_shaders`, `EditorScene::set_camera_screen_shaders` | Инспектор, undo, горячая перезагрузка и Game-вид |
| `tools/rve-build/src/project.rs` | ветка `glsl` в `record_asset` | Проверка файла при сборке релиза |
| `engine/engine-render/shaders/fullscreen.vert` | вершинный шейдер | Общий полноэкранный треугольник всех проходов |
| `examples/demo-game/Assets/fx/*.glsl` | `color_isolate`, `vhs`, `crt` | Примеры; подключены к камере демо выключенными |

## Публичные контракты

### Формат сцены

В `[objects.camera]` массив `screen_shaders` (не больше четырёх записей):

- `shader` — путь `.glsl`, объявленный в `[[assets]]`;
- `stage` — `before_post` или `after_post`, обязательно;
- `enabled` — по умолчанию `true`;
- `params` — не больше шестнадцати конечных чисел; недостающие считаются нулями.

Неизвестные поля отклоняются. Пустой список не записывается, поэтому сцена без поля сохраняется как прежде. Проверку выполняет `validate_screen_shaders`; ту же проверку использует override префабов.

### Исходник GLSL

Автор пишет GLSL 450 без `#version`, без своего `main`, без `in`/`out` и без ресурсов. Обязательна функция `vec3 rve_shade(vec2 uv)`. Движок добавляет фиксированный заголовок и хвост (`compile_screen_shader`):

- `rve_sample(vec2 uv)` — выборка входа по UV в `[0,1]` заполненной области, с зажимом к ней;
- `rve_param(int i)` — пользовательский параметр `i` из `0..16`;
- блок `rve` с полями `params[4]`, `resolution` (размер области и обратный), `content` (масштаб области относительно физического размера и обратный размер физического изображения) и `time` (время сцены по модулю 3600 и номер кадра по модулю 4096).

Push constants занимают 112 байт. Вход — линейный цвет: в `before_post` HDR (значения больше 1), в `after_post` уже tonemapped в `[0,1]`.

Альфа: `before_post` оставляет альфу входа; `after_post` пишет перцептивную яркость результата, потому что FXAA в `output.frag` читает её из альфы (контракт `tonemap.frag`).

Проверка (`compile_screen_shader(source, stage)`, без GPU):

- директивы препроцессора (`#version`, `#extension`) отклоняются с номером строки;
- интерфейс IR: одна fragment-точка `main`, один вход `rve_in_uv` и один выход, глобальные переменные только `rve_in_uv`/`rve_out_color` (naga оформляет их как `Private`), ресурсы только texture и sampler из заголовка и один блок push constants;
- `rve_shade` есть и имеет сигнатуру `vec3(vec2)`;
- номера строк ошибок синтаксиса пересчитаны в строки исходника пользователя.

Один исходник компилируется для обеих стадий, поэтому `rve-build` и `Renderer::create_screen_shader` проверяют обе.

### Renderer

- `Renderer::create_screen_shader(source) -> ScreenShaderId` создаёт два пайплайна: вариант `before_post` в `OFFSCREEN_FORMAT` и `after_post` в `LDR_FORMAT`. Ошибка компиляции возвращает `RenderError::ScreenShader` и не меняет состояние.
- `Renderer::destroy_screen_shader(id)` отправляет пайплайны в `retired`. Ссылка на уничтоженный id в `ViewSettings` даёт `InvalidConfig`.
- `ViewSettings::screen_shaders: ScreenShaders` — до четырёх `Option<ScreenShaderPass>` в порядке записи и `time`. `ScreenShaderPass` содержит `shader`, `stage` и `params`.
- `GpuPass` имеет `ScreenShaderBefore` (перед motion blur и DoF) и `ScreenShaderAfter` (после composite), всего 17 записываемых границ.

### Порядок в кадре

- `before_post` работает после TAA/FSR/DLSS (или после сцены без temporal), до `Filters`: его выход становится входом motion blur, DoF, bloom и composite. Экспозиция замеряется раньше, до temporal resolve, и выход `before_post` не видит. Ping-pong идёт в scratch стадии, размер — физический размер входа.
- `after_post` работает после composite. Его выход (или сам composite без шейдеров) становится изображением output-прохода; вызывающий код перепривязывает набор `output_set` текущего кадрового слота к нему. Остальной порядок — в [[Engine/EnginePostProcess]].

Результат цепочки из `n` проходов лежит в ping-pong изображении `(n - 1) % 2` (`chain_output_index`). Поэтому чётность цепочки не влияет на корректность результата.

Ресурсы: по два изображения и пул с четырьмя наборами дескрипторов на стадию и кадровый слот (`ScreenShaderScratch`). Каждый записанный проход имеет собственный набор. Scratch пересоздаётся при смене физического размера. Когда цепочка стадии становится пустой, scratch освобождается сразу: он принадлежит кадровому слоту, чей fence уже дождан, и пустая цепочка его не пишет (`screen_scratch_plan`). Вместе с целью вида он освобождается при resize, `free_viewport` и выходе.

### Script ABI 14

`RVE_SCRIPT_ABI_VERSION = 14`. `RveHost` получает в конце три nullable callbacks: `screen_shader_count(object, *u32)`, `get_screen_shader(object, slot, *RveScreenShaderState)` и `set_screen_shader(object, slot, *RveScreenShaderState)`. Структура `RveScreenShaderState` (`repr(C)`, 68 байт) содержит `enabled: u32` и `params: [f32; 16]`. Слоты нумеруются с нуля. Модули ABI 13 загружаются без изменений, потому что старые поля `RveHost` не сдвинуты.

Гостевой API `rve-sdk` добавляет к камере методы `screen_shader_count`, `screen_shader` и `set_screen_shader`.

### Lua

`object:screen_shader_count()` возвращает число слотов или `nil`. `object:screen_shader(i)` возвращает `{enabled = bool, params = {16 чисел}}`. `object:set_screen_shader(i, t)` принимает таблицу с необязательными `enabled` и `params`; отсутствующие поля сохраняют текущие значения, `params` заменяет все шестнадцать значений. Индекс `i` — с единицы. Ошибки индекса и параметров ломают вызов так же, как у `set_camera_settings`.

Изменения попадают в очередь транзакции по ключу (объект, слот) и применяются на `commit` через `World::set_screen_shader`. Путь и стадия не меняются скриптом: смена шейдера требует компиляции и вне текущей версии.

### Загрузка и выгрузка

- При загрузке сцены `load_screen_shader_sources` читает все `.glsl` камер мира, включая камеры префабов, без повторов. Вызов выполняется только в игровом клиенте до передачи мира потоку скриптов. `rve-server` (headless) шейдеры не читает и не компилирует.
- `upload_screen_shaders` создаёт шейдеры через `ScreenShaderGpu`; ошибка пишется в лог, шейдер пропускается, сцена продолжает работать.
- Кадр получает только включённые проходы с созданным шейдером (`screen_shader_view`), время сцены — из снимка.
- Замена сцены освобождает шейдеры путей, которые не нужны ни одной камере мира после замены (`SceneSwitch`, `SceneGpuIds::screen_shaders`). Путь новой сцены пересоздаёт шейдер, прежний уходит на освобождение. Выгрузка уничтожает оставшиеся через `release_scene` бэкенда; каждый id уничтожается один раз, и освобождённые при замене из списка выгрузки вычёркиваются.

Камеры в префабах с `camera` нельзя заспавнить (см. [[Engine/EngineScene]]), поэтому все камеры известны при загрузке сцены.

### Редактор

- Инспектор камеры, секция «Screen Shaders»: до четырёх `.glsl`, стадия, вкл/выкл, порядок, удаление, шестнадцать параметров `p0..p15`. Каждая правка идёт через `edit_scene` и отменяется undo. Ошибка компиляции показывается под шейдером.
- Горячая перезагрузка: изменённый `.glsl` через `AssetChanges` перекомпилируется в `reload_changed`. При ошибке остаётся прежний шейдер, текст ошибки попадает в инспектор и лог.
- Game-вид применяет шейдеры выбранной Game-камеры, Scene-вид — нет (как DoF и motion blur).

### rve-build

Ветка `glsl` в `record_asset` читает файл как UTF-8 и вызывает `compile_screen_shader` для обеих стадий. Ошибка отклоняет сборку с путём файла и номером строки.

## Ограничения

- Глубина, история кадров и пользовательские текстуры в шейдере недоступны.
- Параметры безымянные: `rve_param(0..16)`.
- Только Vulkan renderer. Windows не проверялся (FSR, DLSS и их fallback); на macOS (MoltenVK) проверка идёт запусками sandbox и demo-game.
- Цепочка до четырёх проходов на камеру: стоимость каждого прохода — ответственность автора игры.
- Временная адаптация (TAA, FSR, DLSS) не передаёт шейдеру предыдущий кадр.

## Решения и основания

- Настройки лежат в камере сцены, а не в `.postprocess`: `PostProcessSettings` отражён плоско в ABI 12, и пути к файлам шейдеров в нём не передать.
- Компиляция вынесена в `engine-render` без GPU, чтобы `rve-build` проверял шейдеры тем же кодом, что и renderer; поэтому `naga` стал runtime-зависимостью `engine-render`, а `rve-build` получил зависимость на `engine-render` (см. [[Architecture]]).
- Проверка IR отклоняет любые лишние ресурсы и интерфейсы: иначе привязки шейдера не совпали бы с layout конвейера, а ошибка проявилась бы только на GPU.
- Один исходник даёт два варианта: альфа для `after_post` нужна для FXAA, а для `before_post` сохраняется альфа входа.
- Scratch живёт в кадровом слоте, как `FilterScratch`: это даёт независимые наборы дескрипторов и не требует ожидания на CPU.
- Параметры хранятся в сцене, а очередь скриптов не меняет путь и стадию, чтобы не запускать компиляцию из скриптового потока.
- Scene-вид не применяет шейдеры, чтобы искажения не мешали редактированию; это то же решение, что для DoF и motion blur.
