# EnginePlatform

Parent: [[Index]]

## Назначение

`engine-platform` предоставляет общий интерфейс платформенной интеграции Vulkan и singleton IPC. Cargo подключает `engine-platform-windows` для Windows и `engine-platform-macos` для macOS по целевому `target_os`. Остальные ОС не имеют адаптера; сборка `engine-platform` завершается с явным `compile_error!`.

## Источники

| Файл | Роль |
|---|---|
| `engine/engine-platform/Cargo.toml`, `engine/engine-platform/src/lib.rs` | Зависимости по целевой ОС и реэкспорт выбранного адаптера |
| `engine/engine-platform-windows/src/lib.rs`, `src/instance.rs` | Windows: системный Vulkan loader, Win32 surface, системный диалог и singleton IPC |
| `engine/engine-platform-macos/src/lib.rs`, `src/instance.rs` | macOS: подготовка bundled Vulkan loader/ICD, MoltenVK portability, Metal surface, системный диалог и singleton IPC |

## Публичный интерфейс

- `unsafe prepare_vulkan_environment(exe_dir)` вызывается до создания любых потоков. Windows ничего не меняет. macOS сохраняет путь `Engine/libvulkan.1.dylib`, если вместе с loader найден ICD либо задана пользовательская ICD-переменная; устанавливает `VK_DRIVER_FILES` на `Engine/MoltenVK_icd.json` только если существуют оба файла и не задана ни одна из переменных `VK_DRIVER_FILES`, `VK_ICD_FILENAMES`, `VK_ADD_DRIVER_FILES`. Пользовательские переменные не изменяются.
- `load_vulkan() -> Result<ash::Entry, ash::LoadingError>` загружает loader и не меняет окружение. macOS пробует запомненный bundled loader, затем `Entry::load()` и `/opt/homebrew/lib/libvulkan.dylib`; найденный bundled loader пробуется первым даже при заданных пользователем ICD-переменных. Возвращённый `Entry` удерживает библиотеку живой; Vulkan-объекты должны быть уничтожены до освобождения `Entry`.
- `show_error_dialog(title, message)` показывает системный диалог ошибки: Win32 `MessageBoxW` или критический `NSAlert`. macOS проверяет главный поток и наличие CoreGraphics session dictionary; без главного потока или GUI-сессии пишет в stderr и возвращается. Ошибка показа не превращается в Rust-панику.
- `InstanceLock::acquire(key, on_activate: Box<dyn Fn() + Send>)` возвращает `Acquire::Acquired` или `Acquire::AlreadyRunning`; `key` ограничен `[A-Za-z0-9_-]{1,64}`. `PlatformError` — `std::io::Error`. Слушатель вызывает callback на своём потоке и подтверждает запрос после возврата callback.
- `activate_running_instance(key, deadline)` отправляет активацию и ждёт подтверждения до абсолютного срока; результат содержит `ActivationError::Timeout` или ошибку I/O/ОС. Подтверждение означает, что callback обработал запрос, а не что ОС уже перевела окно в фокус.
- `INSTANCE_EXTENSIONS`, `INSTANCE_FLAGS`, `DEVICE_EXTENSIONS` задают требования платформы, которые [[Engine/EngineRender]] объединяет с требованиями surface/swapchain и расширениями из `RendererConfig`.
- `enumerate_required_extensions` и unsafe `create_surface` реэкспортируются из ash-window. Surface требует соответствующих instance extensions, живых window/display handles и соблюдения порядка уничтожения: surface перед instance и loader. Renderer удерживает `Arc` окна и владеет surface.

## Зависимости и ограничения

- Оба адаптера используют ash 0.38.0 и ash-window 0.13.0 только при сборке для своей ОС; платформенные зависимости закреплены точными версиями в корневом `Cargo.toml`. Крейт другой платформы в workspace пустой.
- Windows загружает системный `vulkan-1.dll`; дополнительные расширения и флаги instance пустые. Singleton удерживает именованный mutex `Local\RVE-<key>`; auto-reset events называются `Local\RVE-<key>.activate` и `.ack`, а безымянный stop event завершает поток слушателя. Второй процесс под `Local\RVE-<key>.activator` сериализует запрос, сбрасывает ack перед сигналом activate и удерживает mutex до подтверждения; ожидание mutex ограничено общим deadline. Повторное открытие endpoint использует ограниченные шаги `thread::sleep`. `AllowSetForegroundWindow(ASFW_ANY)` вызывается best-effort: отказ Windows не отменяет IPC-подтверждение.
- macOS использует `libc` для `flock` на `$TMPDIR/rve-<key>.lock` и Unix socket `$TMPDIR/rve-<16 hex>.sock`, где 16 строчных hex-цифр — FNV-1a 64 от байтов ключа. Фиксированное имя сокета помещается в `sun_path` для всех допустимых ключей. Владелец удаляет устаревший socket перед bind; второй процесс повторяет `connect` при `ENOENT`/`ECONNREFUSED`. Слушатель ждёт подключения через `poll` на неблокирующем сокете и read-конце CLOEXEC wake-pipe, а принятый socket переводит в блокирующий режим перед чтением `ACT` с ограниченным timeout; ошибки временного исчерпания дескрипторов или буферов приводят к ожиданию повтора. `Drop` пишет в pipe и присоединяет поток независимо от состояния файла сокета. После завершения socket удаляется, flock освобождается; lock-файл остаётся на диске.
- Удаление lock-файла извне (например, очисткой `$TMPDIR`) может дать новому процессу отдельный inode для `flock` и позволить запуск второго экземпляра, пока старый ещё работает. При `ActivationError::Timeout` вызывающий код может повторить `acquire`: владелец мог завершиться во время активации.
- macOS использует `objc2`/AppKit/Foundation для `NSAlert` и проверки главного потока, CoreGraphics `CGSessionCopyCurrentDictionary` — для проверки доступности GUI-сессии. Для bundled Vulkan требуется пара `Engine/libvulkan.1.dylib` и `Engine/MoltenVK_icd.json`; если пользователь задаёт хотя бы одну ICD-переменную, окружение не меняется. Direct loading `libMoltenVK.dylib` отсутствует. Для Homebrew нужны пакеты `molten-vk` и `vulkan-loader`, для Vulkan SDK — настроенное окружение.
- Адаптер macOS запрашивает `VK_KHR_portability_enumeration`, `ENUMERATE_PORTABILITY_KHR` и `VK_KHR_portability_subset`. Эти настройки требуются для обнаружения и использования MoltenVK через Vulkan loader; см. [официальное руководство](https://github.com/KhronosGroup/MoltenVK/blob/main/Docs/MoltenVK_Runtime_UserGuide.md). Требования renderer к Vulkan 1.3 и features остаются общими для обеих платформ.
- Общий оконный цикл на winit остаётся в [[Engine/EngineWindow]]; общий Vulkan renderer остаётся в [[Engine/EngineRender]]. Адаптеры не зависят от этих крейтов и не дублируют отрисовку.
- Разделение в одном workspace сохраняет единую реализацию движка и локализует код, зависящий от ОС, в отдельных крейтах по требованию пользователя.
