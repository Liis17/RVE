# RveBuild

`tools/rve-build` собирает игровой релиз из каталога проекта или запускает его в
dev-режиме. Инструмент является внешним crate workspace и зависит от
`engine-project`, `engine-scene`, `engine-assets`, `engine-resources`, `engine-pak`
и `engine-files`. Форматы настроек проекта описаны в [[Engine/EngineProject]].
Подробности runtime layout проекта и релиза находятся в
[[Engine/EngineRuntime]], формат сцены — в [[Engine/EngineScene]], а формат архива и
реестр — в [[Engine/EnginePak]].

## Конвейер

`rve-build build <project-dir> --output <dir> [--vulkan-sdk <dir>]` читает
`project.rveproject` и `bootstrap.rves`, открывает `<project>/Assets` через
`ResourceSystem` в `SourceMode::Loose`, затем проверяет каждую включённую сцену с
помощью `SceneDocument`. Выключенные сцены не открываются. В сборку попадают файлы
включённых сцен, их `[[assets]]`, внешние PNG-зависимости FBX через
`Assets::model_dependencies` и заданный `BootstrapConfig.app_icon`. Иконка должна
иметь расширение `.png` и успешно загружаться через `Assets::load_texture`.
Неиспользуемые файлы каталога Assets не копируются.

`build_project_archives` сериализует `ProjectSettings::to_toml` и
`ArchiveSetRegistry::to_toml` во временный каталог, передаёт generated files вместе
с проверенными исходными файлами как `BuildFileInput` в `build_archive_set`. Архивы
собираются во временном sibling-каталоге output, затем публикуются атомарно без
замены существующего каталога. `base.rvepak` содержит сцены, иконку,
`/rve/project.toml` и `/rve/assets.toml`. Runtime получает bootstrap из
`Engine/bootstrap.rves`, а наборы архивов из `Assets/` корня релиза (см.
[[Engine/EngineRuntime]]).

На Windows корень релиза — сам output: executable, `Engine/` и `Assets/` лежат рядом.
На macOS output содержит `<player>.app`: executable в `Contents/MacOS`, `Engine/` и
`Assets/` в `Contents/Resources`, `Contents/Info.plist` генерирует
`platform::info_plist` (`CFBundleIdentifier` = `rve.game.<id>` с заменой `_` на `-`,
версия из `ProjectSettings.version`). Бандл нужен, чтобы запуск из Finder не открывал
Terminal. Бандл не подписывается, `.icns` из AppIcon не создаётся.

## Назначение архивов

- Явный `[[assets]].archive` сохраняется и имеет приоритет для этого ресурса.
- Файл без явного назначения, используемый одной сценой, получает `scene-<stem>`;
  общий для нескольких сцен — `shared`.
- Внешняя текстура FBX без явного назначения следует множеству сцен, использующих
  её модели.
- Файлы сцен, AppIcon и generated project/registry TOML находятся в `base`.
- Разные явные назначения одного ресурса, имена архивов с различием только
  регистра, недопустимое имя `scene-<stem>` и назначение AppIcon вне `base` приводят
  к ошибке проекта.
- В `build_archive_set` порядок — `base`, затем `shared` при наличии, затем другие
  архивы по имени. Это задаёт владельца межархивной dedup-ссылки.

## Генерация player

Player crate создаётся в `<project>/.rve/player/` с отдельным `[workspace]`, именем
пакета `rve-game` и бинарником того же имени. `engine-runtime` подключается по
абсолютному пути в checkout, вычисленному из `CARGO_MANIFEST_DIR` самого
`rve-build`; поэтому сборщик запускается из checkout RVE. Если задан
`heavy_scripts`, пакетное имя читается из его `Cargo.toml`, добавляется path
dependency `game_heavy`, а `main.rs` вызывает
`engine_runtime::run_player(game_heavy::register)`. Без него используется пустой
реестр. Пути в manifest сериализуются TOML-библиотекой, чтобы корректно переживать
пробелы, кавычки и обратные слэши.

Если существует проектный `Cargo.lock`, он используется для повторных запусков;
при первой генерации копируется lockfile checkout. `CARGO_TARGET_DIR` указывает на
`<project>/.rve/target`. Команда `build` вызывает release build, `run` — debug build,
оба разбирают JSON-сообщение Cargo, чтобы найти `rve-game` executable. `run`
запускает его с `--project <absolute-project-dir>` и переданными аргументами.

Все пути, передаваемые Cargo или player, очищаются от Windows verbatim-префикса:
path dependencies в manifest, `CARGO_TARGET_DIR`, рабочий каталог Cargo и аргумент
`--project`. Корень checkout сначала canonicalize-ится, чтобы engine-runtime и
heavy-крейт ссылались на один crate даже при запуске checkout через symlink. Проверки
выхода `heavy_scripts` за проект сравнивают нормализованные canonical paths, включая
symlink-цели.

Имя player после очистки запрещено делать равным `Engine` или `Assets` без учёта
регистра. Также отклоняются зарезервированные имена устройств Windows: `CON`, `PRN`,
`AUX`, `NUL`, `COM1`–`COM9` и `LPT1`–`LPT9`; ошибка проекта возвращает код 2.

При заданном `--vulkan-sdk` поиск macOS Vulkan loader, MoltenVK и ICD-манифеста
ограничен этим каталогом и выполняется до компиляции player. Без параметра источники
проверяются в `VULKAN_SDK`, затем Homebrew. Перед публикацией staging-каталога
сборщик независимо читает `/rve/assets.toml` из `base.rvepak`, открывает весь
`Assets/` через `ArchiveCatalog` и сверяет каталог с `ArchiveSetRegistry`; ошибка
проверки считается операционной (код 1), временный staging удаляется.

Cargo stdout разбирается построчно: обычный текст и строки, не являющиеся JSON,
пропускаются; сборка успешна только при наличии `compiler-artifact` бинарника
`rve-game` с executable. Файлы player crate перезаписываются только при изменении
содержимого, сохраняя mtime при повторной сборке без изменений.

На Windows build script создаёт ICO с PNG payload из AppIcon и вшивает его через
`embed-resource`; размер каждой стороны должен быть не больше 256 пикселей. После
сборки проверяется `RT_GROUP_ICON`. Для этой операции нужен Windows SDK resource
compiler. Если AppIcon не задан, build script не добавляет ресурс.

На macOS в `Contents/Resources/Engine/` копируются `libvulkan.1.dylib`,
`libMoltenVK.dylib` и `MoltenVK_icd.json` с относительным `ICD.library_path`.
Если передан `--vulkan-sdk`, используется только этот каталог; допустимы его корневой
layout и подкаталог `macOS`. Без явного пути источники проверяются в порядке
`VULKAN_SDK`, `/opt/homebrew`, `/usr/local`. Для Homebrew-сборок дополнительно
проверяется `etc/vulkan/icd.d`, куда текущая формула связывает ICD-манифест. Symlink
loader копируется как содержимое его целевого файла. На Windows этот шаг пропускается.
Перед распространением macOS сборки необходимо учитывать лицензии Vulkan loader и
MoltenVK (Apache-2.0).

## Пример и ограничения

`examples/demo-game/` содержит две включённые сцены, общую FBX-модель с внешней
текстурой, явный архив `props`, Lua lite script и отдельный `HeavyScripts` crate с
зарегистрированным Rust-скриптом `Bobber`. Draft-сцена отключена, а лишний файл
проверяет исключение неиспользуемых ресурсов. CI собирает проект без графического
запуска; фактический Vulkan runtime требует совместимое устройство.

Исходники: `tools/rve-build/src/main.rs`, `cli.rs`, `error.rs`, `project.rs`,
`archives.rs`, `player.rs`, `ico.rs`, `platform.rs`, `temporary.rs`.
