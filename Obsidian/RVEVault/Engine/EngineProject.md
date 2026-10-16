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
| `engine/engine-project/src/project_settings.rs` | `ProjectSettings`, `ProjectScene`, `ProjectRequirements`, `RuntimeSettings` | TOML-схема проекта, валидация, стартовая сцена и сериализация |
| `engine/engine-project/src/lib.rs` | публичные реэкспорты | API крейта |
| `engine/engine-project/Cargo.toml` | зависимости | `engine-files`, `serde`, `toml` |
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
| `heavy_scripts` | Необязательный непустой относительный путь к crate Rust-скриптов внутри проекта; абсолютные пути и выход через `..` запрещены |
| `scenes` | Упорядоченный список `{ path, enabled }`; `enabled` по умолчанию `true`. Пути — уникальные virtual paths `.rvescene`; должна быть хотя бы одна включённая сцена, и первая включённая становится стартовой |
| `requirements.min_ram_mb`, `requirements.min_cpu_threads` | Минимум RAM и число логических CPU; default обоих значений — 0, то есть минимум не ограничен |
| `runtime.background_tick_hz` | Частота обновлений скриптов в фоне; default 30, ноль запрещён |

Пути загрузки проекта и коды выхода `rve-player` описаны в
[[Engine/EngineRuntime]]. `rve-build` сериализует настройки через
`ProjectSettings::to_toml` и кладёт их в `base.rvepak`; для отсутствующего,
неверно закодированного или невалидного файла проекта он возвращает код 2, а
прочая ошибка чтения считается операционной и даёт код 1.

## Потребители и границы

- `sandbox` использует только `BootstrapConfig`.
- `engine-runtime` использует `BootstrapConfig` и `ProjectSettings`; бинарник
  `rve-player` получает их поведение через runtime.
- `rve-build` проверяет обе конфигурации, сцены и связанные ресурсы перед сборкой.

Формат `.rvescene` описан в [[Engine/EngineScene]], пути и layout player — в
[[Engine/EngineRuntime]] и [[Engine/RveBuild]].
