# EnginePostProcess

Parent: [[Index]]

## Назначение

Постобработка кадра вида: экспозиция (ручная или авто по гистограмме), bloom, tonemapping, color grading с 3D LUT `.cube`, vignette, chromatic aberration, film grain, motion blur, depth of field и ambient occlusion (GTAO). Настройки живут в файле `.postprocess`, который камера ссылается через поле `post_process` в сцене. Формат и проверка — в `engine-scene`, GPU-проходы — в `engine-render`, маппинг в renderer делают runtime, редактор и sandbox. Все эффекты, кроме exposure и tonemapping, по умолчанию выключены.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-scene/src/post_process.rs` | `PostProcessSettings`, таблицы эффектов, `ColorGradingSettings::lut_path`, `parse`, `to_toml`, `validate`, `resolve_post_process`, `PostProcessError` | Формат `.postprocess`, проверка диапазонов и правило разрешения с legacy `[color]` |
| `engine/engine-scene/src/color_lut.rs` | `CubeLut`, `CubeLutError`, `MIN_CUBE_LUT_SIZE`, `MAX_CUBE_LUT_SIZE` | Разбор 3D LUT `.cube` |
| `engine/engine-scene/src/camera_settings.rs` | `CameraSettings::color`, `CameraColorSettings` | Устаревший блок `[color]` в `.camerasettings` |
| `engine/engine-scene/src/document.rs`, `engine/engine-scene/src/prefab.rs` | `CameraComponent::post_process` | Ссылка камеры на `.postprocess` и проверка объявленного ассета |
| `engine/engine-scene/src/world.rs` | `World::post_process`, `World::set_post_process` | Постобработка на каждой камере мира, `SceneFrame` берёт её у активной |
| `engine/engine-render/src/post.rs` | `PostProcess`, `ColorLutId`, `PostPlan`, `plan`, `PostFrame`, `CompositeParams`, `ao_resolution`, `planned_color_lut` | Публичный renderer-native контракт, план проходов и параметры composite |
| `engine/engine-render/src/bloom.rs`, `exposure.rs`, `filters.rs`, `ambient_occlusion.rs`, `color_lut.rs` | `BloomChain`, `ExposureResources`, `ExposureHistory`, `ExposureTarget`, `FilterScratch`, `AoTarget`, `validate_color_lut` | Vulkan-ресурсы и запись проходов; состояние экспозиции вида — в `exposure.rs` |
| `engine/engine-render/src/lib.rs` | `ViewSettings::post`, `Renderer::create_color_lut`, `Renderer::destroy_color_lut`, `GpuPass`, `record_tonemap_pass`, `write_upscaler_exposure` | Вызов проходов в кадре вида, LUT и экспозиция FSR/DLSS |
| `engine/engine-render/shaders/` | `tonemap.frag`, `output.frag`, `bloom_*.frag`, `motion_blur.frag`, `dof.frag`, `gtao.comp`, `gtao_denoise.comp`, `exposure_histogram.wgsl`, `exposure_average.comp` | Шейдеры; `build.rs` компилирует GLSL и WGSL через naga |
| `engine/engine-runtime/src/scene.rs`, `engine/engine-runtime/src/pipeline.rs`, `engine/engine-runtime/src/scene_switch.rs` | `resolve_post_process`, `render_post_process`, `load_color_luts`, `upload_color_luts`, `GpuSceneTable::color_luts` | Загрузка `.postprocess` на камеру с кэшем, LUT сцены и маппинг в `PostProcess` |
| `engine/rve-sdk/src/abi.rs`, `engine/engine-script/src/abi_host.rs`, `engine/engine-script/src/lite.rs`, `engine/engine-script/src/transaction.rs` | `RvePostProcessSettings`, `host_get_post_process`, `host_set_post_process` | Script ABI 12 и Lua API |
| `tools/rve-build/src/project.rs` | ветки `postprocess` и `cube` | Проверка файла и его LUT при сборке релиза |
| `editor/src/viewport/render.rs`, `editor/src/ui/inspector.rs`, `editor/src/project.rs` | `render_post_process`, `SceneRenderer::sync_color_lut`, `show_post_process_editor` | Маппинг в редакторе, LUT, панель камеры и предпросмотр файла |

## Публичные контракты

### Файл `.postprocess`

TOML с таблицами `exposure`, `tonemapping`, `bloom`, `ambient_occlusion`, `color_grading`, `vignette`, `chromatic_aberration`, `film_grain`, `depth_of_field`, `motion_blur`. Каждая таблица имеет `enabled`. Пустой файл даёт значения по умолчанию.

- Неизвестные поля на любом уровне отклоняются, как и значения вне диапазона, NaN и inf.
- Enum-значения пишутся строчными буквами: `mode = "manual"` или `"auto"`, `tonemapper = "aces"`, `"neutral"`, `"none"`, `quality = "low"`, `"medium"`, `"high"`.
- По умолчанию включены `exposure` (ручная экспозиция, EV 0) и `tonemapping` (ACES). Остальные эффекты выключены, поэтому файл без правок воспроизводит прежний вид.
- `[color_grading]` принимает `lut` — путь к 3D LUT `.cube` (расширение строчными буквами) — и `lut_intensity` в `[0, 1]` (по умолчанию 1). Без `lut` поле не записывается. LUT — зависимость файла `.postprocess`, как текстуры у `.rvemat`: в `[[assets]]` сцены её не объявляют, `rve-build` пакует её с владельцем файла постобработки.

### Файл `.cube`

`CubeLut::parse` принимает 3D LUT формата Adobe Cube: `TITLE`, `LUT_3D_SIZE` в `[2, 128]`, `DOMAIN_MIN 0 0 0`, `DOMAIN_MAX 1 1 1`, `LUT_3D_INPUT_RANGE 0 1`, затем `size³` строк `r g b` (красный меняется быстрее всего). Комментарии `#`, пустые строки, BOM и CRLF допускаются. 1D LUT, другой домен, неизвестные ключевые слова, ключевые слова после данных, неконечные числа и неверное число узлов отклоняются с файлом и строкой. Выходные значения вне `[0, 1]` допускаются.

### Ссылка из сцены

В `.rvescene` поле `post_process` в `[objects.camera]` указывает на путь `.postprocess`. Путь должен быть объявлен в `[[assets]]`, а расширение — `.postprocess`; прочие проверки ссылок выполняет `engine-scene`. Несколько камер могут ссылаться на один файл.

### Правило разрешения с legacy `[color]`

`resolve_post_process(camera, post_file)` возвращает одну из трёх постобработок:

1. Если у камеры есть `post_process`, используется содержимое файла целиком.
2. Иначе, если в `.camerasettings` есть блок `[color]`, exposure становится ручным с `exposure_ev`, а tonemapper берётся из `tonemapper`; остальные эффекты выключены.
3. Иначе используются значения по умолчанию.

Блок `[color]` устарел. Он сохраняется при записи `.camerasettings` без потерь. Поля не объединяются: если у камеры есть файл, `[color]` игнорируется.

### Renderer

- `ViewSettings::post: PostProcess` задаёт экспозицию, tonemapper и эффекты вида. Поля `exposure_ev` и `tonemapper` в `ViewSettings` больше не существуют.
- `Renderer::create_color_lut(size, texels)` создаёт 3D-изображение RGBA16F из `size³` узлов RGB в порядке `.cube` и возвращает `ColorLutId`; загрузка синхронная (`submit_once`). `destroy_color_lut` отправляет изображение в `retired`. `ColorGrading::lut` ссылается на LUT вида, `lut_intensity` — доля LUT. Ссылка на неизвестную или удалённую LUT даёт `InvalidConfig` до acquire.
- Новые эффекты (всё, кроме ручной экспозиции и tonemapper) работают только при `Frame::debug_view == DebugView::Lit`. В отладочных режимах они отключаются.
- Нулевой extent у render или output даёт пустой план (`PostPlan::NONE`).

### Script ABI 12

`RVE_SCRIPT_ABI_VERSION` = 12, `RvePostProcessSettings` (`repr(C)`, плоские поля, коды enum `RVE_EXPOSURE_MODE_*` и `RVE_AO_QUALITY_*`). Host callbacks `get_post_process` и `set_post_process` работают только с камерами. Layout `RveCameraSettings` не менялся: его поля exposure и tonemapper стали проксирующими к постобработке камеры и помечены как устаревшие.

В Lua доступен API `post_process` на камере. Вызовы `set_camera_settings` и `set_post_process` в одном кадре применяются по порядку: обе операции меняют одну очередь постобработки камеры. Legacy-прокси читает exposure и tonemapper из эффективной постобработки (с учётом уже поставленных в очередь изменений), а не из `[color]`. Legacy-запись меняет копию постобработки только если значение отличается от эффективного.

## Порядок проходов

Проходы записываются в порядке `GpuPass` (`engine/engine-render/src/lib.rs`), для каждого вида отдельно.

1. Clusters, ShadowDirectional, ShadowLocal — как раньше.
2. DepthPrepass — только при включённом AO; непрозрачные меши рисуются с depth `LESS_OR_EQUAL`.
3. AmbientOcclusion — GTAO compute на depth prepass, затем denoise. Каждый слайс проверяет горизонт в обе стороны от пикселя. Сырой GTAO считается в разрешении `ao_resolution`: половинном (с округлением вверх) для `low` и `medium`, полном для `high`; качество дополнительно меняет число направлений и шагов. Пиксели AO отображаются на глубину по UV. Денойз работает в полном разрешении render-цели: bilateral-фильтр 5×5 по текселям сырого AO сравнивает глубину пикселя выхода с глубиной под каждым текселем, поэтому одновременно делает depth-aware upsample. Результат читается мешами во фрагментном шейдере (binding 8) и умножает indirect diffuse и specular. Маскированные и blend-меши AO не получают (define `NO_SCREEN_AO`). Смена качества с другим разрешением пересоздаёт цель AO кадрового слота после ожидания его fence.
4. Opaque, Skybox, Transparent, Wireframe, Particles — как раньше.
5. Exposure — гистограмма log-яркости HDR цели сцены в render-разрешении (WGSL, 256 бинов, диапазон log2 −10..+4), усреднение и адаптация по перцентилям low/high с отдельными скоростями роста и падения. Проход идёт до temporal resolve на всех путях (TAA, FSR, DLSS, FXAA, None). Адаптированный EV и множитель `exp2(ev + adapted_ev)` хранятся в буфере на вид.
6. TemporalResolve — TAA, FSR или DLSS, если включены. FSR и DLSS получают экспозицию текущего кадра: в auto-режиме множитель копируется из буфера вида в их 1×1 изображение экспозиции, в ручном — `exp2(ev)` записывается очисткой (`write_upscaler_exposure`).
7. Filters — motion blur (по velocity), затем depth of field; оба используют scratch-цели HDR с перестановкой ping-pong.
8. Bloom — prefilter, downsample и аддитивный upsample; уровней не больше 6, строится, пока меньшая сторона не меньше 8. Prefilter сравнивает порог с яркостью в той же экспонированной шкале, что composite (ручной EV плюс adapted EV в auto), и читает adapted EV из буфера вида. Сам bloom хранится без экспозиции.
9. Tonemap composite (`tonemap.frag`): chromatic aberration → сложение с bloom → экспозиция (EV плюс адаптированный EV при auto) → цветокоррекция в linear → tonemapper → lift/gamma/gain → 3D LUT → vignette. LUT читается по sRGB-кодированному цвету в `[0, 1]` с поправкой к центрам крайних узлов, результат декодируется в linear и смешивается по `lut_intensity`. LUT привязана к binding 4 набора постобработки; без LUT или при выключенной цветокоррекции привязана встроенная identity LUT, а интенсивность равна 0, и выборки нет. После composite идут экранные шейдеры `after_post` ([[Engine/EngineScreenShaders]]). Затем output-проход (`output.frag`) добавляет film grain.

Экранные шейдеры `before_post` записываются после temporal resolve и перед filters (п. 7), поэтому motion blur, DoF, bloom и composite видят их выход. Экспозиция их выход не видит: она замеряется раньше. Без пользовательских шейдеров эти проходы не записываются.

Буфер экспозиции и адаптация принадлежат виду (`TemporalView`), а не кадровому слоту. Порядок адаптации между кадрами одного вида задаёт барьер в начале записи экспозиции (`record_exposure` в `engine/engine-render/src/exposure.rs`): его источник — стадии `COMPUTE_SHADER | FRAGMENT_SHADER | ALL_TRANSFER`, назначение — заполнение гистограммы. Барьер покрывает запись адаптации, чтение composite/bloom и копирование множителя для FSR/DLSS предыдущего кадра вида; семафоров для этого нет. CPU не ждёт. Состояние адаптации меняется только после успешного submit.

## Ограничения

- Только Vulkan renderer. Проверено на macOS (Apple M4, MoltenVK) запусками sandbox с `--frames`, `--render-scale 0.5` и `--resize-test`; Windows не проверялся.
- Auto exposure сразу устанавливает значение без адаптации на первом адаптирующем кадре вида, при `Frame::reset_history` и на кадре после ручного режима.
- Адаптация опирается на интервал между кадрами, который измеряет renderer. Публичного поля `Frame::delta_seconds` нет. Resize и паузы адаптацию не сбрасывают и не ускоряют (`ExposureHistory::plan_frame`): если render-разрешение изменилось или интервал длиннее 250 мс, кадр берёт шаг предыдущего адаптирующего кадра. Snap-кадр после паузы запоминает шаг 1/60 с, а не интервал простоя; поэтому вид, который стабильно рисуется медленнее 250 мс, адаптируется с шагом 1/60 с за кадр.
- Гистограмма считается до temporal resolve и motion blur/DoF, поэтому экранные шейдеры `before_post` и эти фильтры на замер экспозиции не влияют.
- Редактор в Scene-виде отключает depth of field и motion blur, чтобы не мешать редактированию; Game-вид применяет их как есть.
- LUT поддерживаются только 3D `.cube` с доменом `[0, 1]`; 1D LUT и формат PNG-полосы не поддерживаются. LUT применяется к display-referred цвету после tonemapping, а не к HDR.
- Script ABI (`RvePostProcessSettings`) и Lua-таблица постобработки не содержат LUT: запись постобработки из Rust-модуля сохраняет LUT и интенсивность камеры (`ScriptTransaction::set_post_process_from_abi`), Lua меняет только переданные поля. Сменить LUT из скрипта нельзя.
- Runtime и редактор при ошибке чтения или разбора LUT пишут ошибку в лог и рисуют камеру без LUT; `rve-build` такую сборку отклоняет.
- Сцена копирует `PostProcessSettings` активной камеры в `SceneFrame` каждый кадр; с заданной LUT это одно небольшое выделение памяти под путь.
- Проверка шейдеров (GTAO, bloom prefilter, экспозиция) без GPU невозможна в тестах: тесты покрывают упаковку push constants, политику адаптации и план проходов, а сами формулы шейдеров проверяются запусками sandbox.

## Решения и основания

- Renderer не зависит от `engine-scene`: у него свой `PostProcess` в `ViewSettings`, маппинг делают runtime, редактор и sandbox. Это совпадает с тем, как `CameraSettings` уже переводятся в `ViewSettings`.
- Маппинг `PostProcessSettings → PostProcess` продублирован: `render_post_process` в `engine-runtime` (`pipeline.rs`), функция в `sandbox` и `render_post_process` в редакторе (`viewport/render.rs`). Общей функции нет по границам зависимостей: sandbox и редактор не зависят от `engine-runtime`, а `engine-render` не может зависеть от `engine-scene`. Редактор добавляет оверрайд Scene-вида для DoF и motion blur, поэтому его копия отличается.
- Адаптация экспозиции хранится на виде, а не на кадровом слоте: при двух кадрах в полёте слоты разделяли бы адаптацию. Порядок между адаптациями кадров одного вида задают барьеры без семафоров: первая область синхронизации `vkCmdPipelineBarrier2` охватывает команды, отправленные раньше на ту же очередь (Khronos registry: `vkCmdPipelineBarrier2`, вне render pass). Условие — все submit renderer идут через одну очередь (`self.queue`, `get_device_queue(family, 0)`). Если появится вторая очередь, этот механизм нужно пересмотреть.
- Гистограмма экспозиции считается до temporal resolve на всех путях, а не только для FSR/DLSS: FSR и DLSS получают экспозицию текущего кадра, а все пути сглаживания замеряют одно и то же изображение. Цена решения (подтверждено пользователем): экранные шейдеры `before_post` и motion blur/DoF не влияют на замер.
- После resize и паузы адаптация продолжается с прежним шагом, а не делает snap (решение пользователя): содержимое кадра после resize почти не меняется, а интервал простоя до 1 с иначе превращал первый кадр в скачок экспозиции.
- AO `low`/`medium` считаются в половинном разрешении с upsample в денойзе, а не в `mesh.frag`: mesh-шейдер и binding 8 остаются без изменений и читают AO полного разрешения, а дорогой GTAO обходится в четыре раза дешевле.
- LUT — формат `.cube` (решение пользователя): его экспортируют DaVinci Resolve, Photoshop и Premiere. Хранение RGBA16F выбрано потому, что линейная фильтрация этого формата обязательна в Vulkan, а у RGBA32F — нет. `.cube` не выделены в отдельный `AssetKind`: это расширило бы схемы параметров скриптов и MCP; инспектор ищет их среди прочих файлов.
- `PostProcessSettings` больше не `Copy`, потому что хранит путь LUT; ABI скриптов при этом не менялся.
- Значения по умолчанию воспроизводят прежний вид и стоимость: при выключенных эффектах `PostPlan` не добавляет проходов, а ручная экспозиция и tonemapper идут тем же путём.
- ABI поднят с 10 до 11, чтобы добавить callbacks и структуру постобработки; layout `RveCameraSettings` сохранён для старых модулей.
- Гистограмма написана на WGSL: naga `glsl-in` 30.0.1 не поддерживает atomics. Усреднение написано на GLSL: naga не принимает `var<push_constant>` в WGSL.
- Синхронизация адаптации между кадрами одного вида идёт через барьер и порядок отправки на очередь, а не через ожидание на CPU, чтобы не добавлять блокировок в кадр.
