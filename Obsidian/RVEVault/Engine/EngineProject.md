# EngineProject

Parent: [[Index]]

## Назначение

`engine-project` разбирает и проверяет общие конфигурационные файлы запуска и игры:
строчный `bootstrap.rves` и TOML `project.rveproject`. Он содержит типы и проверку
данных, но не загружает ресурсы и не запускает окно, сцены или runtime.

## Источники

| Файл | Значимые символы | Роль |
|---|---|---|
| `engine/engine-project/src/bootstrap.rs` | `BootstrapConfig::{parse, load}` | Формат начальных настроек, значения по умолчанию и проверки |
| `engine/engine-project/src/project_settings.rs` | `ProjectSettings`, `ProjectScene`, `ProjectRequirements`, `RuntimeSettings`, `PhysicsSettings` | TOML-схема проекта, валидация, стартовая сцена и сериализация |
| `engine/engine-project/src/input_settings.rs` | `InputSettings`, `InputAction` | Схема игровых действий, проверка физических имён вводов и ограничений |
| `engine/engine-project/src/prefab_catalog.rs` | `PrefabCatalogFile`, `PrefabCatalogEntry`, `PREFAB_CATALOG_FILE_NAME`, `PREFAB_CATALOG_RELEASE_PATH` | Список тяжёлых префабов проекта: разбор, проверка, сериализация |
| `engine/engine-project/src/lib.rs` | публичные реэкспорты | API крейта |
| `engine/engine-project/Cargo.toml` | зависимости | `engine-files`, `rve-sdk`, `serde`, `toml` |
| `engine/sandbox/src/main.rs` | загрузка bootstrap | Настройки sandbox из текущего каталога |
| `engine/engine-runtime/src/layout.rs`, `engine/engine-runtime/src/lib.rs` | `Layout`, `load_project_settings` | Dev/release расположение файлов и загрузка проекта в `rve-player` |
| `tools/rve-build/src/project.rs`, `tools/rve-build/src/archives.rs` | `prepare_project`, сборка архивов | Проверка входного проекта и упаковка настроек для release |

## `bootstrap.rves`

Одна запись на строку: `Имя=Значение`. Парсер игнорирует пустые строки, обрезает
пробелы вокруг имени и значения, учитывает регистр и сохраняет всё после первого
`=` в значении. Комментарии не поддерживаются. Неизвестные и повторные поля,
строка без `=`, неверное значение и ошибка чтения приводят к ошибке с путём и
номером строки для ошибок разбора.

| Поле | Значение и default |
|---|---|
| `WindowSizeX`, `WindowSizeY` | Необязательные положительные `u32`; при отсутствии берётся размер `WindowConfig` — 1280×720 |
| `WindowTitle` | Необязательная непустая после trim строка |
| `AllowMultipleInstances` | Только `true` или `false`; default `true`. При `false` player использует `ProjectSettings.id` как ключ singleton lock |
| `AppIcon` | Необязательный virtual path в `Assets`; bootstrap проверяет только корректность пути |
| `PresentMode` | Необязательный `Fifo`, `Mailbox` или `Immediate`; при отсутствии используется FIFO |
| `Reflex` | Необязательный `Off`, `On` или `Boost` (`BootstrapReflexMode`); при отсутствии равен `Off`. Передаётся в `RendererConfig::low_latency` игрока, редактора и sandbox; на устройствах без поддержки Reflex работает как `Off` (см. [[Engine/EngineRender]]) |

`BootstrapConfig::load` трактует отсутствующий файл как настройки по умолчанию.
Пути bootstrap и применение `WindowTitle`/`AppIcon` в player, а также его коды
выхода описаны в [[Engine/EngineRuntime]]; sandbox читает файл из cwd
([[Engine/Sandbox]]). `rve-build` требует файл в каталоге проекта. Неверный формат,
отсутствующий обязательный файл или неверное значение дают код 2 в `rve-build`;
прочая ошибка чтения файла там считается операционной и даёт код 1.

## `project.rveproject`

Файл — TOML со строгой схемой: неизвестные поля запрещены. `id`, `name` и `version`
обязательны; остальные секции имеют значения по умолчанию, если не указаны.

| Поле | Смысл и проверка |
|---|---|
| `id` | Стабильный ключ `[A-Za-z0-9_-]{1,64}`. Используется как singleton lock key, если в bootstrap запрещены несколько экземпляров |
| `name` | Непустое имя игры |
| `version` | Непустая строка; отдельной проверки SemVer нет |
| `input` | Необязательные именованные действия и контексты; без секции остаётся пустая карта без активного контекста. Формат описан ниже |
| `heavy_scripts` | Необязательный непустой относительный путь к crate Rust-скриптов внутри проекта; абсолютные пути и выход через `..` запрещены |
| `scenes` | Упорядоченный список `{ path, enabled }`; `enabled` по умолчанию `true`. Пути — уникальные virtual paths `.rvescene`; должна быть хотя бы одна включённая сцена, и первая включённая становится стартовой |
| `requirements.min_ram_mb`, `requirements.min_cpu_threads` | Минимум RAM и число логических CPU; default обоих значений — 0, то есть минимум не ограничен |
| `runtime.background_tick_hz` | Частота обновлений скриптов в фоне; default 30, ноль запрещён |
| `runtime.fsr_library` | Необязательный относительный путь внутри проекта к AMD Vulkan loader DLL (`.dll`); default отсутствует. Соседняя `amd_fidelityfx_upscaler_vk.dll` обязательна для сборки Windows-релиза |
| `runtime.dlss_library` | Необязательный относительный путь внутри проекта к `nvngx_dlss.dll`; default отсутствует. Идентификатор NGX задаётся ровно одним из `dlss_project_id` или `dlss_application_id` |
| `runtime.dlss_project_id`, `runtime.dlss_engine_version` | GUID-подобный project ID и версия RVE engine; оба требуются при выборе project ID и запрещены с application ID |
| `runtime.dlss_application_id` | Необязательный положительный NVIDIA application ID; взаимоисключается с project ID и engine version |
| `physics.gravity` | Конечный вектор ускорения; default `[0, -9.81, 0]` |
| `physics.fixed_hz` | Частота фиксированного шага, `1..=1000`; default `60` |
| `physics.max_substeps` | Максимум подшагов за один update, `1..=64`; default `8` |

Секция `[physics]` необязательна. Сейчас `ProjectSettings` разбирает, проверяет и
сериализует эти значения; runtime ещё не использует их для симуляции.

```toml
[physics]
gravity = [0.0, -9.81, 0.0]
fixed_hz = 60
max_substeps = 8
```

### Игровой ввод `[input]`

`ProjectSettings.input` хранит `InputSettings`; конфигурация сериализуется и обратно
разбирается через `ProjectSettings::to_toml`. Если секции `[input]` нет, проект
получает пустые настройки и неактивный контекст, сохраняя совместимость со старыми
проектами.

```toml
[input]
default_context = "game"

[input.contexts.game.Jump]
kind = "button"
inputs = ["Space", "Mouse.Left"]

[input.contexts.game.Move]
kind = "vector2"
up = ["W"]
down = ["S"]
left = ["A"]
right = ["D"]

[input.contexts.game.Throttle]
kind = "axis"
negative = ["Q"]
positive = ["E"]
analog = "WheelY"
scale = 1.0
```

Контекст содержит действия с видами `button`, `axis` или `vector2`. Кнопка
использует слот `inputs`; ось — `negative`, `positive`, optional `analog`, `scale`;
вектор — `up`, `down`, `left`, `right`, optional `analog`, `scale`. Для оси
аналоговый источник принимает `MouseX`, `MouseY`, `WheelX` или `WheelY`; для
вектора — `MouseDelta` или `Wheel`. `scale` по умолчанию равен `1.0` и должен быть
конечным. Поля другого вида и любые неизвестные поля запрещены.

Имена контекстов и действий соответствуют `[A-Za-z0-9_]{1,64}`. Если задан хотя бы
один контекст, `default_context` обязателен и должен указывать на существующий
контекст. Одно действие сохраняет одинаковый вид во всех контекстах. Физический ввод
задаётся именем клавиши `Key` из [[Engine/RveSdk]] или именем кнопки с префиксом
`Mouse.`, например `Mouse.Left`. В одном слоте нельзя повторять имя; пустые слоты
разрешены для последующего переназначения. Ограничения: до 32 контекстов, 128
действий в контексте и 16 вводов в слоте. Проверка физических имён использует
`rve_sdk::PhysicalInput`.

### Runtime plugins и сериализация

Например, при наличии официальных FSR DLL в `vendor/` Windows-проект может задать:

```toml
[runtime]
fsr_library = "vendor/amd_fidelityfx_loader_vk.dll"
```

Путь к loader не может быть абсолютным или выходить за каталог проекта через `..`.
`rve-build build` проверяет и копирует loader вместе с соседним upscaler effect DLL
в `Engine/`; dev-player разрешает путь относительно каталога проекта. Без доступного
FSR runtime запрос `anti_aliasing = "fsr"` переходит на TAA с одним предупреждением.

Для Windows DLSS путь к библиотеке должен оканчиваться на `nvngx_dlss.dll`;
identifier проверяется при разборе проекта. В dev-player библиотека читается по
относительному пути в проекте, в release — по имени из `Engine/`. Для `rve-build`
нужен `RVE_DLSS_SDK` с Windows x64 SDK и библиотекой `nvsdk_ngx_s.lib`. Если
SDK/backend или подходящая поддержка Vulkan недоступны, запрос `anti_aliasing =
"dlss"` переходит на TAA с одним предупреждением.

Пути загрузки проекта и коды выхода `rve-player` описаны в
[[Engine/EngineRuntime]]. `rve-build` сериализует настройки через
`ProjectSettings::to_toml` и кладёт их в `base.rvepak`; для отсутствующего,
неверно закодированного или невалидного файла проекта он возвращает код 2, а
прочая ошибка чтения считается операционной и даёт код 1.

## `prefabs.rveprefabs`

Необязательный TOML-файл в корне проекта со списком тяжёлых префабов: это
`.rveprefab`, которые грузятся с диска во время игры, а не живут в памяти вместе со
сценой. Схема строгая, неизвестные поля запрещены; отсутствующий файл означает пустой
список. Пустой файл тоже допустим.

| Поле | Смысл и проверка |
|---|---|
| `[[prefabs]].path` | Виртуальный путь `.rveprefab` в `Assets`; путь должен быть валидным и уникальным. Расширение проверяется без учёта регистра |
| `[[prefabs]].archive` | Необязательный явный архив файла префаба и его зависимостей. Имя проверяется по правилу `SceneAsset::archive` из [[Engine/EngineScene]]: `[A-Za-z0-9_-]{1,64}`, `base` и `shared` запрещены в любом регистре |
| `[[prefabs]].max_instances` | Необязательный лимит живых spawn-экземпляров; должен быть больше нуля |

```toml
[[prefabs]]
path = "/prefabs/house.rveprefab"
archive = "buildings"
max_instances = 4

[[prefabs]]
path = "/prefabs/rock.rveprefab"
```

Порядок записей сохраняется. `PrefabCatalogFile::to_toml` проверяет список и выдаёт
детерминированный TOML, который редактор записывает обратно без изменения порядка.
Отсутствующий файл в `PrefabCatalogFile::load` даёт пустой список; ошибки формата
и ошибки чтения содержат путь к файлу.

В release список хранится как `/rve/prefabs.toml` в `base`; `rve-build` всегда пишет
этот файл, даже пустой. Путь зарезервирован: `Assets` его содержать не может. Что
именно упаковывается и как выбирается архив, описано в [[Engine/RveBuild]].

## Потребители и границы

- `sandbox` использует только `BootstrapConfig`.
- `engine-runtime` использует `BootstrapConfig` и `ProjectSettings`; бинарник
  `rve-player` получает их поведение через runtime.
- `rve-build` проверяет обе конфигурации, сцены и связанные ресурсы перед сборкой.
- `rve-editor` читает и сохраняет `prefabs.rveprefabs` в подокне «Префабы проекта» ([[Engine/RveEditor#Префабы]]).

Формат `.rvescene` описан в [[Engine/EngineScene]], пути и layout player — в
[[Engine/EngineRuntime]] и [[Engine/RveBuild]].
