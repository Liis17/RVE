# EnginePlatform

Parent: [[Index]]

## Назначение

`engine-platform` предоставляет общий интерфейс платформенной интеграции Vulkan. Cargo подключает `engine-platform-windows` для Windows и `engine-platform-macos` для macOS по целевому `target_os`. Остальные ОС не имеют адаптера; сборка `engine-platform` завершается с явным `compile_error!`.

## Источники

| Файл | Роль |
|---|---|
| `engine/engine-platform/Cargo.toml`, `engine/engine-platform/src/lib.rs` | Зависимости по целевой ОС и реэкспорт выбранного адаптера |
| `engine/engine-platform-windows/src/lib.rs` | Windows: системный Vulkan loader и Win32 surface через ash-window |
| `engine/engine-platform-macos/src/lib.rs` | macOS: Vulkan loader и Metal surface через ash-window |

## Публичный интерфейс

- `load_vulkan() -> Result<ash::Entry, ash::LoadingError>` загружает loader. Возвращённый `Entry` удерживает библиотеку живой; Vulkan-объекты должны быть уничтожены до освобождения `Entry`.
- `INSTANCE_EXTENSIONS`, `INSTANCE_FLAGS`, `DEVICE_EXTENSIONS` задают требования платформы, которые [[Engine/EngineRender]] объединяет с требованиями surface/swapchain и расширениями из `RendererConfig`.
- `enumerate_required_extensions` и unsafe `create_surface` реэкспортируются из ash-window. Surface требует соответствующих instance extensions, живых window/display handles и соблюдения порядка уничтожения: surface перед instance и loader. Renderer удерживает `Arc` окна и владеет surface.

## Зависимости и ограничения

- Оба адаптера используют ash 0.38.0 и ash-window 0.13.0 только при сборке для своей ОС; крейт другой платформы в workspace пустой.
- Windows загружает `vulkan-1.dll`, macOS — `libvulkan.dylib` из окружения приложения. Дополнительные расширения платформы и флаги instance пока пусты; настройки MoltenVK portability отсутствуют.
- Общий оконный цикл на winit остаётся в [[Engine/EngineWindow]]; общий Vulkan renderer остаётся в [[Engine/EngineRender]]. Адаптеры не зависят от этих крейтов и не дублируют отрисовку.
- Разделение в одном workspace сохраняет единую реализацию движка и локализует код, зависящий от ОС, в отдельных крейтах по требованию пользователя.
