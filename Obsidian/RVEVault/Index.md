# RVE — База знаний

RVE — игровой движок на Rust с рабочим оконным путём на winit и Vulkan-рендерером на ash.

## Архитектура

- [[Architecture|Архитектура]] — устройство workspace, зависимости, поток кадра и ограничения

## Компоненты

| Заметка | Назначение |
|---|---|
| [[Engine/EngineCore]] | Счётчик времени игрового цикла |
| [[Engine/EngineProject]] | Форматы bootstrap и настроек проекта игры |
| [[Engine/EngineWindow]] | Окно и цикл событий |
| [[Engine/EnginePlatform]] | Выбор и реализация платформенной интеграции Windows/macOS |
| [[Engine/EngineRender]] | Vulkan renderer и вывод кадра |
| [[Engine/EngineFiles]] | Канонические пути и позиционное чтение файлов |
| [[Engine/EnginePak]] | Формат RVEPAK, builder manifest, CLI и межархивный каталог |
| [[Engine/EngineResources]] | Фоновое чтение loose и packed бинарных ресурсов |
| [[Engine/EngineAssets]] | Пути ресурсов, загрузка моделей FBX и текстур PNG |
| [[Engine/EngineScene]] | Формат сцен `.rvescene`, runtime-граф `World`, камеры, трансформации и свет |
| [[Engine/EngineScript]] | Lua host и ABI-загрузка Rust-скриптов, lifecycle и отмена Lua |
| [[Engine/RveSdk]] | C ABI и гостевой API для игровых Rust-скриптов в `cdylib` |
| [[Engine/EngineRuntime]] | Runtime игры: layout проекта, потоки сцены, скриптов и рендера, завершение и коды выхода |
| [[Engine/RveBuild]] | Сборка релиза проекта в player и набор RVEPAK |
| [[Engine/Sandbox]] | Пример приложения и CLI для проверки движка |

Пустые модули `engine-audio`, `engine-physics`, `engine-network` и `engine-neural`, а также скелет `engine-editor` описаны в [[Architecture]]; отдельных заметок для них нет.

## Обновление памяти

Заметки отражают текущее устройство проекта. При изменении контрактов, зависимостей, потоков или ограничений проверяй сведения по исходникам. Имена заметок и папок доменов — английские; changelog и дневники задач не ведутся.
