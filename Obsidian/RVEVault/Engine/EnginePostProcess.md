# EnginePostProcess

Parent: [[Index]]

## Назначение

Постобработка кадра вида: экспозиция (ручная или авто по гистограмме), bloom, tonemapping, color grading, vignette, chromatic aberration, film grain, motion blur, depth of field и ambient occlusion (GTAO). Настройки живут в файле `.postprocess`, который камера ссылается через поле `post_process` в сцене. Формат и проверка — в `engine-scene`, GPU-проходы — в `engine-render`, маппинг в renderer делают runtime, редактор и sandbox. Все эффекты, кроме exposure и tonemapping, по умолчанию выключены.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-scene/src/post_process.rs` | `PostProcessSettings`, таблицы эффектов, `parse`, `to_toml`, `validate`, `resolve_post_process`, `PostProcessError` | Формат `.postprocess`, проверка диапазонов и правило разрешения с legacy `[color]` |
| `engine/engine-scene/src/camera_settings.rs` | `CameraSettings::color`, `CameraColorSettings` | Устаревший блок `[color]` в `.camerasettings` |
| `engine/engine-scene/src/document.rs`, `engine/engine-scene/src/prefab.rs` | `CameraComponent::post_process` | Ссылка камеры на `.postprocess` и проверка объявленного ассета |
| `engine/engine-scene/src/world.rs` | `World::post_process`, `World::set_post_process` | Постобработка на каждой камере мира, `SceneFrame` берёт её у активной |
| `engine/engine-render/src/post.rs` | `PostProcess`, `PostPlan`, `plan`, `PostFrame`, `CompositeParams` | Публичный renderer-native контракт, план проходов и параметры composite |
| `engine/engine-render/src/bloom.rs`, `exposure.rs`, `filters.rs`, `ambient_occlusion.rs` | `BloomChain`, `ExposureResources`, `ExposureHistory`, `ExposureTarget`, `FilterScratch`, `AoTarget` | Vulkan-ресурсы и запись проходов; состояние экспозиции вида — в `exposure.rs` |
| `engine/engine-render/src/lib.rs` | `ViewSettings::post`, `GpuPass`, `record_tonemap_pass` | Вызов проходов в кадре вида |
| `engine/engine-render/shaders/` | `tonemap.frag`, `output.frag`, `bloom_*.frag`, `motion_blur.frag`, `dof.frag`, `gtao.comp`, `gtao_denoise.comp`, `exposure_histogram.wgsl`, `exposure_average.comp` | Шейдеры; `build.rs` компилирует GLSL и WGSL через naga |
| `engine/engine-runtime/src/scene.rs`, `engine/engine-runtime/src/pipeline.rs` | `resolve_post_process`, `render_post_process` | Загрузка `.postprocess` на камеру с кэшем и маппинг в `PostProcess` |
| `engine/rve-sdk/src/abi.rs`, `engine/engine-script/src/abi_host.rs`, `engine/engine-script/src/lite.rs`, `engine/engine-script/src/transaction.rs` | `RvePostProcessSettings`, `host_get_post_process`, `host_set_post_process` | Script ABI 12 и Lua API |
| `tools/rve-build/src/project.rs` | ветка `postprocess` | Проверка файла при сборке релиза |
| `editor/src/viewport/render.rs`, `editor/src/ui/inspector.rs`, `editor/src/project.rs` | `render_post_process`, `show_post_process_editor` | Маппинг в редакторе, панель камеры и предпросмотр файла |

## Публичные контракты

### Файл `.postprocess`

TOML с таблицами `exposure`, `tonemapping`, `bloom`, `ambient_occlusion`, `color_grading`, `vignette`, `chromatic_aberration`, `film_grain`, `depth_of_field`, `motion_blur`. Каждая таблица имеет `enabled`. Пустой файл даёт значения по умолчанию.

- Неизвестные поля на любом уровне отклоняются, как и значения вне диапазона, NaN и inf.
- Enum-значения пишутся строчными буквами: `mode = "manual"` или `"auto"`, `tonemapper = "aces"`, `"neutral"`, `"none"`, `quality = "low"`, `"medium"`, `"high"`.
- По умолчанию включены `exposure` (ручная экспозиция, EV 0) и `tonemapping` (ACES). Остальные эффекты выключены, поэтому файл без правок воспроизводит прежний вид.

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
- Новые эффекты (всё, кроме ручной экспозиции и tonemapper) работают только при `Frame::debug_view == DebugView::Lit`. В отладочных режимах они отключаются.
- Нулевой extent у render или output даёт пустой план (`PostPlan::NONE`).

### Script ABI 12

`RVE_SCRIPT_ABI_VERSION` = 12, `RvePostProcessSettings` (`repr(C)`, плоские поля, коды enum `RVE_EXPOSURE_MODE_*` и `RVE_AO_QUALITY_*`). Host callbacks `get_post_process` и `set_post_process` работают только с камерами. Layout `RveCameraSettings` не менялся: его поля exposure и tonemapper стали проксирующими к постобработке камеры и помечены как устаревшие.

В Lua доступен API `post_process` на камере. Вызовы `set_camera_settings` и `set_post_process` в одном кадре применяются по порядку: обе операции меняют одну очередь постобработки камеры. Legacy-прокси читает exposure и tonemapper из эффективной постобработки (с учётом уже поставленных в очередь изменений), а не из `[color]`. Legacy-запись меняет копию постобработки только если значение отличается от эффективного.

## Порядок проходов

Проходы записываются в порядке `GpuPass` (`engine/engine-render/src/lib.rs`), для каждого вида отдельно.

1. Clusters, ShadowDirectional, ShadowLocal — как раньше.
2. DepthPrepass — только при включённом AO; непрозрачные меши рисуются с depth `LESS_OR_EQUAL`.
3. AmbientOcclusion — GTAO compute на depth prepass, затем denoise. Каждый слайс проверяет горизонт в обе стороны от пикселя. Результат читается мешами во фрагментном шейдере (binding 8) и умножает indirect diffuse и specular. Маскированные и blend-меши AO не получают (define `NO_SCREEN_AO`). AO считается в полном разрешении render-цели при любом качестве: качество меняет число направлений и шагов.
4. Opaque, Skybox, Transparent, Wireframe, Particles — как раньше.
5. TemporalResolve — TAA, FSR или DLSS, если включены.
6. Filters — motion blur (по velocity), затем depth of field; оба используют scratch-цели HDR с перестановкой ping-pong.
7. Exposure — гистограмма log-яркости (WGSL, 256 бинов, диапазон log2 −10..+4), усреднение и адаптация по перцентилям low/high с отдельными скоростями роста и падения. Адаптированный EV хранится в буфере на вид.
8. Bloom — prefilter, downsample и аддитивный upsample; уровней не больше 6, строится, пока меньшая сторона не меньше 8. Prefilter сравнивает порог с яркостью в той же экспонированной шкале, что composite (ручной EV плюс adapted EV в auto), и читает adapted EV из буфера вида. Сам bloom хранится без экспозиции.
9. Tonemap composite (`tonemap.frag`): chromatic aberration → сложение с bloom → экспозиция (EV плюс адаптированный EV при auto) → цветокоррекция в linear → tonemapper → lift/gamma/gain → vignette. После composite идут экранные шейдеры `after_post` ([[Engine/EngineScreenShaders]]). Затем output-проход (`output.frag`) добавляет film grain.

Экранные шейдеры `before_post` записываются после temporal resolve и перед filters (п. 6), поэтому motion blur, DoF, экспозиция, bloom и composite видят их выход. Без пользовательских шейдеров эти проходы не записываются.

Буфер экспозиции и адаптация принадлежат виду (`TemporalView`), а не кадровому слоту. Порядок адаптации между кадрами одного вида задаёт барьер в начале записи экспозиции (`record_exposure` в `engine/engine-render/src/exposure.rs`): его источник — стадии `COMPUTE_SHADER | FRAGMENT_SHADER`, назначение — заполнение гистограммы. Барьер покрывает запись адаптации и чтение composite/bloom предыдущего кадра вида; семафоров для этого нет. CPU не ждёт. Состояние адаптации меняется только после успешного submit.

## Ограничения

- Только Vulkan renderer. Проверено на macOS (Apple M4, MoltenVK) запусками sandbox с `--frames`, `--render-scale 0.5` и `--resize-test`; Windows не проверялся.
- Auto exposure сразу устанавливает значение без адаптации на первом адаптирующем кадре вида, при `Frame::reset_history` и на кадре после ручного режима. Resize адаптацию не сбрасывает.
- Адаптация опирается на интервал между кадрами, который измеряет renderer. Публичного поля `Frame::delta_seconds` нет.
- FSR и DLSS получают экспозицию предыдущего кадра; при auto exposure их внутренняя экспозиция не синхронизирована.
- Редактор в Scene-виде отключает depth of field и motion blur, чтобы не мешать редактированию; Game-вид применяет их как есть.
- Цветокоррекция состоит из баланса белого, контраста, насыщенности и lift/gamma/gain. LUT-текстур нет.
- AO в v1 не использует половинное разрешение для `low` и `medium`: буфер AO совпадает с render-целью, поэтому качество меняет только число направлений и шагов. Половинное разрешение потребует dispatch с пересчётом координат глубины и выборки AO по UV в `mesh.frag`; не сделано.
- Проверка шейдеров (GTAO, bloom prefilter, экспозиция) без GPU невозможна в тестах: тесты покрывают упаковку push constants, политику адаптации и план проходов, а сами формулы шейдеров проверяются запусками sandbox.

## Решения и основания

- Renderer не зависит от `engine-scene`: у него свой `PostProcess` в `ViewSettings`, маппинг делают runtime, редактор и sandbox. Это совпадает с тем, как `CameraSettings` уже переводятся в `ViewSettings`.
- Маппинг `PostProcessSettings → PostProcess` продублирован: `render_post_process` в `engine-runtime` (`pipeline.rs`), функция в `sandbox` и `render_post_process` в редакторе (`viewport/render.rs`). Общей функции нет по границам зависимостей: sandbox и редактор не зависят от `engine-runtime`, а `engine-render` не может зависеть от `engine-scene`. Редактор добавляет оверрайд Scene-вида для DoF и motion blur, поэтому его копия отличается.
- Адаптация экспозиции хранится на виде, а не на кадровом слоте: при двух кадрах в полёте слоты разделяли бы адаптацию. Порядок между адаптациями кадров одного вида задают барьеры без семафоров: первая область синхронизации `vkCmdPipelineBarrier2` охватывает команды, отправленные раньше на ту же очередь (Khronos registry: `vkCmdPipelineBarrier2`, вне render pass). Условие — все submit renderer идут через одну очередь (`self.queue`, `get_device_queue(family, 0)`). Если появится вторая очередь, этот механизм нужно пересмотреть.
- Значения по умолчанию воспроизводят прежний вид и стоимость: при выключенных эффектах `PostPlan` не добавляет проходов, а ручная экспозиция и tonemapper идут тем же путём.
- ABI поднят с 10 до 11, чтобы добавить callbacks и структуру постобработки; layout `RveCameraSettings` сохранён для старых модулей.
- Гистограмма написана на WGSL: naga `glsl-in` 30.0.1 не поддерживает atomics. Усреднение написано на GLSL: naga не принимает `var<push_constant>` в WGSL.
- Синхронизация адаптации между кадрами одного вида идёт через барьер и порядок отправки на очередь, а не через ожидание на CPU, чтобы не добавлять блокировок в кадр.
