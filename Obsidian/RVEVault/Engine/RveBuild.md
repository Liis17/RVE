# RveBuild

`tools/rve-build` собирает игровой релиз из каталога проекта или запускает его в
dev-режиме. Инструмент является внешним crate workspace и зависит от
`engine-project`, `engine-scene`, `engine-assets`, `engine-resources`, `engine-pak`,
`engine-files` и `engine-script`. Форматы настроек проекта описаны в [[Engine/EngineProject]].
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

## Player и Rust-скрипты

`rve-build` не генерирует отдельный player crate. Режим определяется наличием
`rve-sdk.toml` рядом с исполняемым файлом `rve-build`.

Без маркера используется checkout-режим: `locate_player` собирает пакет `rve-player`
командой `cargo build --release --locked -p rve-player` из корня репозитория и задаёт
`CARGO_TARGET_DIR=<checkout>/build/player`. Путь к исполняемому файлу берётся из Cargo
`compiler-artifact`; вывод сообщает `rve-player: rebuilt` или `rve-player: up to date`
по полю `fresh` этого артефакта.

С маркером используется SDK-режим: `rve-sdk.toml` разбирается как TOML и содержит
`engine_version`, `abi_version` и `target`. `abi_version` должен совпадать с
`RVE_SCRIPT_ABI_VERSION`; `target` должен совпадать с парой `ARCH-OS`, например
`x86_64-windows` или `aarch64-macos`. `rve-build` запускает соседний
`rve-player[.exe]` и сообщает `rve-player: prebuilt (SDK)`. Отсутствующий player,
ошибка TOML, несовпадение ABI или платформы завершают команду операционной ошибкой.
SDK-режим не обращается к `CARGO_MANIFEST_DIR` и исходникам checkout.

`rve-build sdk --output <dir>` доступна только в checkout-режиме. Она собирает
release `rve-player` в `build/player`, затем атомарно публикует новый каталог без
замены существующего. Каталог содержит `rve-build[.exe]`, `rve-player[.exe]`,
`rve-sdk.toml` и `rve-sdk/Cargo.toml` с деревом `rve-sdk/src/**`; `target/` не
копируется. `rve-build` копирует собственный запущенный executable. При debug-сборке
команда предупреждает об этом; для распространяемого SDK рекомендуется запускать
`cargo run --release -p rve-build -- sdk --output <dir>`. Клиентский crate указывает
на `<sdk>/rve-sdk` как path-зависимость (см. [[Engine/RveSdk]]).

Если `project.rveproject` содержит `heavy_scripts`, каталог обязан содержать
`Cargo.toml` с lib-target `crate-type = ["cdylib"]`. Сборщик читает Cargo metadata,
затем выполняет `cargo build --lib --manifest-path ... --message-format=json` без
`--locked` и с `CARGO_TARGET_DIR=<project>/.rve/target`; для `build` добавляется
`--release`, а `run` использует debug-профиль. Библиотека выбирается из
`compiler-artifact` по package id из metadata, имени target, kind `cdylib` и
расширению текущей платформы. Cargo.lock скриптов остаётся в зоне проекта.

Собранную библиотеку сборщик загружает через `ScriptLibrary` и проверяет, что она
регистрирует каждое имя heavy-скрипта из включённых сцен. Ссылка сцены на heavy без
`heavy_scripts`, отсутствие cdylib или незарегистрированное имя — ошибка проекта с
кодом 2. ABI и модель гостевого crate описаны в [[Engine/RveSdk]].

Команда `run` собирает скрипты в debug-профиле, берёт release `rve-player` из
checkout `build/player` или соседнего каталога SDK и запускает его как `--project <absolute-project-dir> [--scripts
<library>]`, затем передаёт пользовательские аргументы. Если скриптов нет, `--scripts`
не добавляется. На Windows release-player — GUI executable и не открывает консоль;
debug-сборку player можно запустить вручную командой `cargo run -p rve-player`;
она сохраняет консоль для диагностики.

Команда `build` собирает release cdylib и checkout release-player либо использует
соседний prebuilt player из SDK, затем копирует
исполняемый файл в релиз и библиотеку в `Engine/` под фиксированным именем
`game_scripts.dll` (Windows) или `libgame_scripts.dylib` (macOS). Windows executable,
`Engine/` и `Assets/` размещаются рядом; на macOS executable входит в
`<name>.app/Contents/MacOS`, а `Engine/` и `Assets/` находятся в
`Contents/Resources`.

На Windows AppIcon преобразуется в ICO с PNG payload; каждая сторона не больше 256
пикселей. Иконка применяется к копии executable через `BeginUpdateResourceW`,
`UpdateResourceW` и `EndUpdateResourceW`: отдельные изображения записываются как
`RT_ICON`, затем группа `RT_GROUP_ICON`. Сборщик проверяет, что группа ссылается на
существующие ресурсы с ожидаемыми байтами. Player в checkout не модифицируется.

Все пути, передаваемые Cargo или player, очищаются от Windows verbatim-префикса.
Каталог `heavy_scripts` должен оставаться внутри проекта и после разрешения symlink.
Имя executable после очистки запрещено делать равным `Engine` или `Assets` без учёта
регистра; также отклоняются зарезервированные имена устройств Windows `CON`, `PRN`,
`AUX`, `NUL`, `COM1`–`COM9` и `LPT1`–`LPT9`. Ошибки проекта возвращают код 2.

При заданном `--vulkan-sdk` поиск macOS Vulkan loader, MoltenVK и ICD-манифеста
ограничен этим каталогом. Без параметра источники проверяются в `VULKAN_SDK`, затем
Homebrew. Перед публикацией staging-каталога сборщик независимо читает
`/rve/assets.toml` из `base.rvepak`, открывает весь `Assets/` через `ArchiveCatalog`
и сверяет каталог с `ArchiveSetRegistry`; ошибка проверки считается операционной
(код 1), временный staging удаляется.

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
текстурой, явный архив `props`, Lua lite script и отдельный `HeavyScripts` crate,
который зависит от `rve-sdk`, собирается как `cdylib` и экспортирует Rust-скрипт
`Bobber`. Draft-сцена отключена, а лишний файл проверяет исключение неиспользуемых
ресурсов. CI собирает проект без графического запуска; фактический Vulkan runtime
требует совместимое устройство.

Исходники: `tools/rve-build/src/main.rs`, `cli.rs`, `error.rs`, `project.rs`,
`archives.rs`, `player.rs`, `sdk.rs`, `ico.rs`, `platform.rs`, `temporary.rs`.
