# RVE — База знаний

RVE — игровой движок на Rust с рабочим оконным путём на winit и Vulkan-рендерером на ash.

## Архитектура

- [[Architecture|Архитектура]] — устройство workspace, зависимости, поток кадра и ограничения

## Компоненты

| Заметка | Назначение |
|---|---|
| [[Engine/EngineCore]] | Счётчик времени игрового цикла |
| [[Engine/EngineProject]] | Форматы bootstrap, настроек проекта игры и списка тяжёлых префабов |
| [[Engine/EngineWindow]] | Окно и цикл событий |
| [[Engine/EnginePlatform]] | Выбор и реализация платформенной интеграции Windows/macOS |
| [[Engine/EngineRender]] | Vulkan renderer и вывод кадра |
| [[Engine/EngineFiles]] | Канонические пути и позиционное чтение файлов |
| [[Engine/EnginePak]] | Формат RVEPAK, builder manifest, CLI и межархивный каталог |
| [[Engine/EngineResources]] | Фоновое чтение loose и packed бинарных ресурсов |
| [[Engine/EngineAssets]] | Пути ресурсов, загрузка моделей FBX, текстур PNG и Radiance HDR environments |
| [[Engine/EnginePhysics]] | Геометрия, столкновения, solver и fixed-step physics world |
| [[Engine/EngineScene]] | Формат сцен `.rvescene`, runtime-граф `World` с spawn и despawn, камеры, трансформации и свет |
| [[Engine/EngineScript]] | Lua host и ABI-загрузка Rust-скриптов, lifecycle, spawn/destroy и отмена Lua |
| [[Engine/EngineAudio]] | Звук: декодирование WAV/Ogg, микшер на `cpal`, мост сцены и Play |
| [[Engine/EngineAnimation]] | Скелетная анимация: позы, клипы, blend tree, one-shot с переходом, палитры skinning и мост аниматоров |
| [[Engine/EngineNetwork]] | Сеть: UDP-протокол с шифрованием и рукопожатием, sans-IO Server/Client, репликация объектов, снимки и мост к миру |
| [[Engine/EngineConsole]] | Ограниченный буфер лога, команды и общий egui developer console |
| [[Engine/RveSdk]] | C ABI и гостевой API для игровых Rust-скриптов в `cdylib` |
| [[Engine/EngineRuntime]] | Runtime игры: layout проекта, потоки сцены, скриптов и рендера, быстрые и тяжёлые префабы, сетевой режим и выделенный сервер `rve-server`, завершение и коды выхода |
| [[Engine/RveBuild]] | Сборка релиза проекта в player и набор RVEPAK |
| [[Engine/Sandbox]] | Пример приложения и CLI для проверки движка |
| [[Engine/RveEditor]] | Графический редактор сцен на egui: панели, правки с undo, вьюпорт и Play |

Пустой модуль `engine-neural` описан в [[Architecture]]; отдельной заметки для него нет.

## Обновление памяти

Заметки отражают текущее устройство проекта. При изменении контрактов, зависимостей, потоков или ограничений проверяй сведения по исходникам. Имена заметок и папок доменов — английские; changelog и дневники задач не ведутся.
