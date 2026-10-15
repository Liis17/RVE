# EngineRender

Parent: [[Index]]

## Назначение

`engine-render` создаёт Vulkan instance, surface, устройство и swapchain, записывает глубину сцены в shadow map, рисует меши с текстурой, направленным светом и PCF-тенями в offscreen-target и переносит его в окно. Крейту не нужна конкретная оконная библиотека: окно передаёт display/window handles через `raw-window-handle`.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-render/src/lib.rs` | `RendererConfig`, `Capabilities`, `RenderError`, `FrameStatus`, `Renderer`, `Vertex`, `MeshId`, `TextureId`, `DrawCommand`, `DirectionalLight`, `Frame`, `OverlayVertex` | Публичный контракт renderer и реализация Vulkan-пути |
| `engine/engine-render/shaders/mesh.vert`, `engine/engine-render/shaders/mesh.frag` | push constants `model` и `normal_from_model`, uniform `FrameUniforms`, `albedo_texture`, `albedo_sampler`, `shadow_map`, `shadow_sampler` | GLSL-шейдеры мешей: albedo, diffuse и ambient с ручным PCF-сравнением shadow map |
| `engine/engine-render/shaders/shadow.vert` | `light_view_projection`, `model` | Вершинный depth-only проход для карты теней |
| `engine/engine-render/shaders/overlay.vert`, `engine/engine-render/shaders/overlay.frag` | push constant `target_size` | 2D-оверлей: пиксели окна → NDC, цвет вершины с альфой |
| `engine/engine-render/build.rs` | `SHADERS` | Компиляция GLSL в SPIR-V через naga в `OUT_DIR`; renderer встраивает результат через `include_bytes!` |
| `engine/engine-render/Cargo.toml` | `ash`, `engine-platform`, `raw-window-handle`, build-зависимость `naga` | Vulkan bindings, платформенная интеграция, оконные handles без зависимости от winit и сборка шейдеров |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `RendererConfig` | Публичные поля `render_scale: f32`, `extra_instance_extensions: Vec<&'static CStr>`, `extra_device_extensions: Vec<&'static CStr>`; по умолчанию масштаб 1.0 и пустые списки расширений |
| `Capabilities` | Публичные поля `device_name: String`, `vendor_id: u32`, `api_version: u32`, `discrete: bool`, `tessellation_shader: bool` о выбранном GPU |
| `Renderer::new<W>(window: Arc<W>, width: u32, height: u32, config: RendererConfig) -> Result<Renderer, RenderError>` | Проверяет конечный `render_scale` в `(0, 1]`, загружает loader и создаёт renderer; `W` реализует `HasDisplayHandle + HasWindowHandle + Send + Sync + 'static`. Renderer удерживает `Arc` окна |
| `Renderer::capabilities(&self) -> &Capabilities` | Возвращает сведения о выбранном физическом устройстве |
| `Renderer::resize(&mut self, width, height)` | Запоминает размер в физических пикселях и помечает swapchain к пересозданию |
| `Vertex` | `#[repr(C)]`, поля `position: [f32; 3]`, `normal: [f32; 3]`, `uv: [f32; 2]`, `color: [f32; 3]` (44 байта); совпадает с входами `mesh.vert` (locations 0–3). `uv` отсчитывается от левого верхнего угла текстуры, `color` умножается на цвет текстуры |
| `Renderer::create_mesh(&mut self, vertices: &[Vertex], indices: &[u32]) -> Result<MeshId, RenderError>` | Проверяет непустые вершины, положительное и кратное трём число индексов и индексы в пределах вершин; загружает буферы и возвращает `MeshId`. Меши живут до уничтожения renderer |
| `Renderer::create_texture(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<TextureId, RenderError>` | Принимает sRGB RGBA8 по строкам сверху вниз: ровно `width × height × 4` байт, размеры от 1 до `maxImageDimension2D`. Копирует данные через staging-буфер в device-local image `R8G8B8A8_SRGB` и ждёт завершения на fence. Текстуры живут до уничтожения renderer |
| `DrawCommand` | `mesh: MeshId`, `texture: Option<TextureId>` (`None` — встроенная белая текстура 1×1, то есть только цвет вершин) и `model: [[f32; 4]; 4]` по столбцам — переход из пространства модели в мировое |
| `DirectionalLight` | `direction` — направление лучей в мировом пространстве (конечное и ненулевое, длина не важна), `color` и `ambient` в линейном RGB. `Default`: лучи `[0.4, -0.7, -0.6]` (свет сверху слева со стороны камеры, смотрящей вдоль -Z), белый цвет, ambient 0.15 |
| `Frame<'a>` | `clear_color: [f32; 4]`, `view_projection: [[f32; 4]; 4]` и необязательная `shadow_view_projection: Option<[[f32; 4]; 4]>` по столбцам (мировое пространство → clip space, Y вверх, глубина `[0, 1]`), `light: DirectionalLight`, `draws: &[DrawCommand]`, `overlay: &[OverlayVertex]` (треугольники по три вершины; пустой срез отключает оверлей). `None` отключает сравнение теней |
| `OverlayVertex` | `#[repr(C)]`, `position: [f32; 2]` в физических пикселях от левого верхнего угла окна, `color: [f32; 4]` — линейный RGB с непредумноженной альфой (24 байта) |
| `Renderer::take_gpu_frame_time(&mut self) -> Option<Duration>` | GPU-время shadow и scene pass последнего прочитанного кадра; каждое измерение возвращается один раз. `None` — нового измерения нет или очередь не поддерживает timestamps. Результат приходит с задержкой до двух кадров |
| `Renderer::render_frame(&mut self, frame: &Frame<'_>) -> Result<FrameStatus, RenderError>` | До пропуска кадра проверяет существование `MeshId` и `TextureId`, ненулевое направление света, конечность заданной shadow-матрицы и число вершин оверлея, кратное трём. Затем создаёт shadow depth pass, scene pass, blit и при непустом `overlay` overlay pass, после чего вызывает present |
| `FrameStatus::Presented` | `queue_present` вернул успех, в том числе при suboptimal; это не подтверждение фактического сканирования кадра монитором |
| `FrameStatus::Skipped` | Нулевой размер, пересоздание swapchain либо `OUT_OF_DATE` от acquire/present без успешной презентации |
| `RenderError` | Варианты `InvalidConfig`, `Loader`, `Handle`, `Vulkan`, `Unsupported`, `NoSuitableDevice`, `InvalidState`, `InvalidMesh` (неверные данные меша или неизвестный `MeshId`) и `InvalidTexture` (неверные размеры или объём пикселей, неизвестный `TextureId`). Нулевой свет или неконечная shadow-матрица дают `InvalidConfig`; реализует `Display` и `Error` |

## Зависимости и взаимодействия

- Использует ash 0.38.0 и raw-window-handle 0.6.2; naga 30.0.1 — только build-зависимость. naga 30.0.1 не собирает `glsl-in` без `wgsl-in` или `spv-in`, поэтому в `Cargo.toml` дополнительно включена `wgsl-in`. [[Engine/EnginePlatform]] предоставляет загрузку Vulkan, требования платформы и surface через ash-window 0.13.0.
- Используется в [[Engine/Sandbox]], который передаёт `Arc<Window>` из [[Engine/EngineWindow]].
- `engine-render` намеренно не зависит от winit; это проверяется командой `cargo tree -p engine-render`.

## Ограничения и важные детали

- Требуется Vulkan API не ниже 1.3, `dynamicRendering`, `synchronization2`, `VK_KHR_swapchain`, graphics+present queue family, surface с `TRANSFER_DST`, `COLOR_ATTACHMENT` и FIFO, surface-формат с `BLIT_DST` и `COLOR_ATTACHMENT_BLEND`, а также поддержка optimal tiling offscreen-формата `R16G16B16A16_SFLOAT` с `COLOR_ATTACHMENT`, `BLIT_SRC` и `SAMPLED_IMAGE_FILTER_LINEAR`, формата `D32_SFLOAT` с `DEPTH_STENCIL_ATTACHMENT` и `SAMPLED_IMAGE` для shadow map, и формата текстур `R8G8B8A8_SRGB` с `SAMPLED_IMAGE`, `SAMPLED_IMAGE_FILTER_LINEAR` и `TRANSFER_DST`.
- Проверки расширений instance/device выполняются до их создания; требования платформы и дополнительные расширения из конфигурации дедуплицируются. Отсутствующее расширение приводит к `RenderError::Unsupported`.
- Renderer одинаков для Windows и macOS. На macOS адаптер добавляет portability extensions и флаг instance для MoltenVK; отдельного Metal renderer нет. Loader и surface предоставляются платформой, владение Vulkan-объектами остаётся у renderer.
- Discrete GPU получает приоритет. Выбор устройства также проверяет API, features, очередь, surface и форматы; причины отказа включаются в `NoSuitableDevice`.
- Swapchain использует FIFO и exclusive sharing. Размер ограничивается возможностями surface. При начальном размере 0×0 renderer создаёт instance, устройство, capabilities и общую shadow map, но откладывает swapchain и per-frame offscreen-targets до ненулевого размера.
- Существует два frame slot с отдельными command buffer, fence, acquire semaphore и offscreen-target (цвет и глубина); shadow map и sampler общие, а frame descriptor каждого слота ссылается на них. Проходы используют общую graphics queue. Render-finished semaphore привязан к swapchain image. Fence сбрасывается после acquire и записи command buffer, перед submit.
- Render extent вычисляется как `floor(output extent × render_scale)` и ограничивается минимумом 1×1. До scene pass renderer переводит общую shadow map из `UNDEFINED` в depth layout, очищает её до 1.0 и, если `Frame::shadow_view_projection` задана, записывает глубину всех draw-команд. Затем барьер переводит карту в `SHADER_READ_ONLY_OPTIMAL`; scene pass очищает `R16G16B16A16_SFLOAT` и `D32_SFLOAT`, после чего рисует `DrawCommand`. Output pass выполняет линейный blit в swapchain и переводит изображение в present layout. Когда тени выключены, карта всё равно очищается для валидного дескриптора, но draw в shadow pass не записываются.
- Изменение размера, suboptimal или out-of-date помечают swapchain для пересоздания. Нулевой размер пропускается до acquire. При ошибке после acquire обработчик завершает приложение, после чего renderer очищается.
- Частично созданные Vulkan-ресурсы освобождаются при ошибке инициализации. При `Drop` вызывается `device_wait_idle`; descriptor pool уничтожается до используемых им shadow ресурсов, shadow image и sampler освобождаются до device, а shadow pipeline — до общего pipeline layout. `Arc` окна остаётся жив до уничтожения surface и instance.
- `device_wait_idle` не доказывает завершение всех операций presentation engine. Maintenance extensions в текущем коде не используются.
- В debug-сборке validation layer включается, только если обнаружен через перечисление instance layers; иначе renderer пишет сообщение в stderr и работает без него. Validation layer в среде выполненных проверок отсутствовал.
- Оверлей рисуется отдельным pass после blit прямо в swapchain image (`LOAD`, затем present), поэтому не масштабируется `render_scale` и остаётся чётким. Для него у swapchain image создаются image views, а pipeline с форматом surface, альфа-смешиванием `SRC_ALPHA / ONE_MINUS_SRC_ALPHA` и без глубины создаётся при инициализации. Вершины лежат в host-visible буфере своего frame slot, перезаписываются после ожидания fence слота и при нехватке места пересоздаются с ёмкостью до следующей степени двойки.
- GPU-время меряют два timestamp на frame slot (query pool сбрасывается в command buffer, `hostQueryReset` не нужен): начало записывается перед shadow pass, конец — после scene pass, оба на стадии `COLOR_ATTACHMENT_OUTPUT`. Измерение охватывает shadow и scene pass; blit и оверлей не входят. Начальная отметка не блокирует начало shadow pass от хвоста предыдущего кадра, поэтому время иногда немного занижается. Привязка shadow pass к предыдущему кадру сериализовала бы кадры и добавила ожидание vsync. Результат читается без ожидания после fence слота с маской `timestampValidBits` и пересчётом через `timestampPeriod`; при `timestampValidBits == 0` query pool не создаются.
- Scene pipeline создаётся при инициализации: `TRIANGLE_LIST`, depth test `LESS` с записью, без смешивания и без отсечения граней. Отдельный shadow pipeline пишет только глубину, использует только позицию вершины, `LESS`, depth bias `1.25 / 1.75`, `cull NONE` и те же динамические правила viewport/scissor в размере карты 2048×2048. Оба pipeline используют общий pipeline layout. Viewport и scissor основной сцены динамические и равны render extent, поэтому pipeline не пересоздаются при resize.
- Push constants вершинной стадии занимают 128 байт: матрица модели и матрица нормалей по столбцам. CPU рассчитывает матрицу нормалей без glam; она сохраняет нормали при неравномерном и зеркальном масштабе.
- `FrameUniforms` занимает 192 байта в set 0, binding 0, std140; его descriptor виден vertex- и fragment-стадиям. Смещения: 0 — `view_projection` (mat4), 64 — `light_view_projection` (mat4), 128 — `light_direction`, 144 — `light_color`, 160 — `ambient_color`, 176 — `shadow_params` (четыре vec4). У последнего вектора x — флаг теней, y — размер texel `1/2048`. Буфер своего frame slot перезаписывается после ожидания fence этого слота. Свет и shadow image/sampler располагаются в set 0, binding 1 — sampled image и binding 2 — sampler.
- Освещение: `albedo × (ambient + light.color × diffuse × shadow)`, где `diffuse = max(dot(n, -direction), 0)`, а `albedo` — текстура, умноженная на цвет вершины. Одна общая D32 shadow map 2048×2048; PCF вручную сравнивает глубину в окне 3×3 с bias `max(0.005 × (1 − dot(n, l)), 0.0005)`. Фрагмент за пределами shadow clip volume считается освещённым. Тени отключаются через `Frame::shadow_view_projection = None`. Specular и каскады не реализованы. Параметры света и обе матрицы лежат в per-frame uniform buffer.
- Текстура материала — set 1: `SAMPLED_IMAGE` (binding 0) и `SAMPLER` (binding 1), потому что GLSL-фронтенд naga работает с раздельными `texture2D` и `sampler`. Один общий sampler материалов: линейная фильтрация, `REPEAT`, без mipmaps и анизотропии. У каждой текстуры свой descriptor pool на один набор. Белая текстура 1×1 создаётся при инициализации под внутренним индексом 0. Shadow map использует отдельный sampler с NEAREST и `CLAMP_TO_EDGE`.
- Загрузка текстуры синхронная: одноразовый command buffer отправляется в ту же очередь, и renderer ждёт его fence. Вызывать её стоит при загрузке ресурсов, а не каждый кадр.
- Viewport имеет отрицательную высоту, поэтому ось Y clip space направлена вверх. Флаг naga `ADJUST_COORDINATE_SPACE` в `build.rs` выключен, чтобы шейдер сам не переворачивал Y.
- Vertex- и index-буферы каждого меша находятся в `HOST_VISIBLE | HOST_COHERENT` памяти и заполняются один раз при создании; staging-загрузки в device-local память нет. Индексы `u32`. Удаления отдельных мешей нет: буферы освобождаются в `Drop` после `device_wait_idle` вместе с pipeline. `MeshId` — индекс внутри renderer; id другого renderer с допустимым индексом не распознаётся.
- Mipmaps, нормальных карт, PBR-материалов, нескольких источников света, specular, каскадных и других дополнительных карт теней и удаления текстур нет. Между render resolution и output resolution уже есть offscreen-target и отдельная output-функция, но DLSS/FSR не интегрированы. Для такой интеграции ещё отсутствуют, в частности, depth/motion vectors, jitter, SDK/FFI и необходимые ресурсные usage flags.
- `tessellation_shader` отражает и включает поддерживаемый Vulkan feature, но tessellation shaders и pipeline отсутствуют.
- Sandbox генерирует компоненты clear color в диапазоне 0..1; HDR и тонмаппинга нет.

## Решения и основания

- Vulkan 1.3, dynamic rendering и synchronization2 — утверждённый минимальный уровень API.
- Render/output resolution разделены, а output pass вынесен отдельно как точка возможного апскейлера. Это не означает наличия готовой DLSS/FSR интеграции.
- GLSL компилируется naga в `build.rs` по выбору пользователя: сборке не нужны Vulkan SDK и `glslc`, ошибки шейдеров видны при `cargo build`.
- Дополнительные расширения задаются в `RendererConfig` до создания instance/device по утверждённому требованию для будущих интеграций.
- `raw-window-handle` позволяет renderer принимать окно без прямой зависимости от winit; `Arc` удерживает окно живым до уничтожения surface. Это подтверждённый контракт владения.
- Оверлей рисуется поверх swapchain после blit, а не в offscreen-target: при `render_scale < 1` текст иначе размывался бы масштабированием. Renderer принимает только готовые треугольники; шрифт, раскладка и метрики остаются в приложении ([[Engine/Sandbox]]).
- Загрузка GPU для `--debug-overlay` считается по Vulkan timestamps scene pass, а не по счётчикам ОС: так выбрал пользователь, чтобы не добавлять платформенный код. Значение отражает только работу этого приложения.
