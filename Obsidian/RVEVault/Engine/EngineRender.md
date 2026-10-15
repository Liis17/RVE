# EngineRender

Parent: [[Index]]

## Назначение

`engine-render` создаёт Vulkan instance, surface, устройство и swapchain, очищает offscreen-target и переносит его в окно. Крейту не нужна конкретная оконная библиотека: окно передаёт display/window handles через `raw-window-handle`.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-render/src/lib.rs` | `RendererConfig`, `Capabilities`, `RenderError`, `FrameStatus`, `Renderer` | Публичный контракт renderer и реализация Vulkan-пути |
| `engine/engine-render/Cargo.toml` | `ash`, `engine-platform`, `raw-window-handle` | Vulkan bindings, платформенная интеграция и оконные handles без зависимости от winit |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `RendererConfig` | Публичные поля `render_scale: f32`, `extra_instance_extensions: Vec<&'static CStr>`, `extra_device_extensions: Vec<&'static CStr>`; по умолчанию масштаб 1.0 и пустые списки расширений |
| `Capabilities` | Публичные поля `device_name: String`, `vendor_id: u32`, `api_version: u32`, `discrete: bool`, `tessellation_shader: bool` о выбранном GPU |
| `Renderer::new<W>(window: Arc<W>, width: u32, height: u32, config: RendererConfig) -> Result<Renderer, RenderError>` | Проверяет конечный `render_scale` в `(0, 1]`, загружает loader и создаёт renderer; `W` реализует `HasDisplayHandle + HasWindowHandle + Send + Sync + 'static`. Renderer удерживает `Arc` окна |
| `Renderer::capabilities(&self) -> &Capabilities` | Возвращает сведения о выбранном физическом устройстве |
| `Renderer::resize(&mut self, width, height)` | Запоминает размер в физических пикселях и помечает swapchain к пересозданию |
| `Renderer::render_frame(&mut self, clear_color: [f32; 4]) -> Result<FrameStatus, RenderError>` | При необходимости пересоздаёт ресурсы или пропускает кадр; иначе очищает offscreen-target, blit-ит его в swapchain и вызывает present |
| `FrameStatus::Presented` | `queue_present` вернул успех, в том числе при suboptimal; это не подтверждение фактического сканирования кадра монитором |
| `FrameStatus::Skipped` | Нулевой размер, пересоздание swapchain либо `OUT_OF_DATE` от acquire/present без успешной презентации |
| `RenderError` | Варианты `InvalidConfig`, `Loader`, `Handle`, `Vulkan`, `Unsupported`, `NoSuitableDevice` и `InvalidState`; реализует `Display` и `Error` |

## Зависимости и взаимодействия

- Использует ash 0.38.0 и raw-window-handle 0.6.2. [[Engine/EnginePlatform]] предоставляет загрузку Vulkan, требования платформы и surface через ash-window 0.13.0.
- Используется в [[Engine/Sandbox]], который передаёт `Arc<Window>` из [[Engine/EngineWindow]].
- `engine-render` намеренно не зависит от winit; это проверяется командой `cargo tree -p engine-render`.

## Ограничения и важные детали

- Требуется Vulkan API не ниже 1.3, `dynamicRendering`, `synchronization2`, `VK_KHR_swapchain`, graphics+present queue family, surface с `TRANSFER_DST` и FIFO, surface-формат с `BLIT_DST`, а также поддержка optimal tiling offscreen-формата `R16G16B16A16_SFLOAT` с `COLOR_ATTACHMENT`, `BLIT_SRC` и `SAMPLED_IMAGE_FILTER_LINEAR`.
- Проверки расширений instance/device выполняются до их создания; требования платформы и дополнительные расширения из конфигурации дедуплицируются. Отсутствующее расширение приводит к `RenderError::Unsupported`.
- Discrete GPU получает приоритет. Выбор устройства также проверяет API, features, очередь, surface и форматы; причины отказа включаются в `NoSuitableDevice`.
- Swapchain использует FIFO и exclusive sharing. Размер ограничивается возможностями surface. При начальном размере 0×0 renderer создаёт instance, устройство и capabilities, но откладывает swapchain и offscreen-изображения до ненулевого размера.
- Существует два frame slot с отдельными command buffer, fence, acquire semaphore и offscreen-target. Render-finished semaphore привязан к swapchain image. Fence сбрасывается после acquire и записи command buffer, перед submit.
- Render extent вычисляется как `floor(output extent × render_scale)` и ограничивается минимумом 1×1. Scene pass очищает `R16G16B16A16_SFLOAT` через dynamic rendering с сохранением содержимого; output pass выполняет линейный blit в swapchain и переводит изображение в present layout.
- Изменение размера, suboptimal или out-of-date помечают swapchain для пересоздания. Нулевой размер пропускается до acquire. При ошибке после acquire обработчик завершает приложение, после чего renderer очищается.
- Частично созданные Vulkan-ресурсы освобождаются при ошибке инициализации. При `Drop` вызывается `device_wait_idle`, затем ресурсы уничтожаются в порядке владения; `Arc` окна остаётся жив до уничтожения surface и instance.
- `device_wait_idle` не доказывает завершение всех операций presentation engine. Maintenance extensions в текущем коде не используются.
- В debug-сборке validation layer включается, только если обнаружен через перечисление instance layers; иначе renderer пишет сообщение в stderr и работает без него. Validation layer в среде выполненных проверок отсутствовал.
- Текущий renderer не рисует геометрию и не загружает шейдеры. Между render resolution и output resolution уже есть offscreen-target и отдельная output-функция, но DLSS/FSR не интегрированы. Для такой интеграции ещё отсутствуют, в частности, depth/motion vectors, jitter, SDK/FFI и необходимые ресурсные usage flags.
- `tessellation_shader` отражает и включает поддерживаемый Vulkan feature, но tessellation shaders и pipeline отсутствуют.
- Sandbox генерирует компоненты clear color в диапазоне 0..1; HDR и тонмаппинга нет.

## Решения и основания

- Vulkan 1.3, dynamic rendering и synchronization2 — утверждённый минимальный уровень API.
- Render/output resolution разделены, а output pass вынесен отдельно как точка возможного апскейлера. Это не означает наличия готовой DLSS/FSR интеграции.
- Дополнительные расширения задаются в `RendererConfig` до создания instance/device по утверждённому требованию для будущих интеграций.
- `raw-window-handle` позволяет renderer принимать окно без прямой зависимости от winit; `Arc` удерживает окно живым до уничтожения surface. Это подтверждённый контракт владения.
