# RveBuild

`tools/rve-build` собирает игровой релиз из каталога проекта или запускает его в
dev-режиме. Инструмент является внешним crate workspace и зависит от
`engine-project`, `engine-scene`, `engine-assets`, `engine-resources`, `engine-pak`,
`engine-files` и `engine-script`. Форматы настроек проекта описаны в [[Engine/EngineProject]].
Подробности runtime layout проекта и релиза находятся в
[[Engine/EngineRuntime]], формат сцены — в [[Engine/EngineScene]], а формат архива и
реестр — в [[Engine/EnginePak]].

## Конвейер

Автоматическая публикация Windows x64 SDK описана в
`.github/workflows/sdk-release.yml`: push в `main` проходит отбор
`.github/scripts/select_sdk_release.py` по графу зависимостей Cargo из
`affected_packages.py`. Выпуск нужен при изменении `rve-editor`, `rve-build`,
`rve-player`, `rve-sdk` или их зависимостей, общих настроек сборки и файлов
конвейера выпуска. Документация, примеры и независимые инструменты пропускаются.
Ручной запуск из `main` принудительный; отсутствие базы сравнения также требует сборки.
SDK собирается MSVC с bundled GNU toolchain, затем
`package_sdk_release.py` создаёт `sdk-verified.zip` с корнем `sdk-verified/`,
распаковывает его во временный каталог и проверяет `scripts` и `build` на шаблоне
New Project при PATH только с системными каталогами Windows. Успешный архив
публикуется как prerelease `sdk-<полный SHA>`, без изменения latest release;
повторный запуск заменяет asset того же релиза. Проверка не запускает GUI или GPU.

`rve-build build <project-dir> --output <dir> [--vulkan-sdk <dir>]` читает
`project.rveproject` и `bootstrap.rves`, открывает `<project>/Assets` через
`ResourceSystem` в `SourceMode::Loose`, затем проверяет каждую включённую сцену с
помощью `SceneDocument`. Выключенные сцены не открываются. В сборку попадают файлы
включённых сцен, их `[[assets]]`, внешние PNG-зависимости FBX через
`Assets::model_dependencies` и заданный `BootstrapConfig.app_icon`. Каждый объявленный
asset `.camerasettings` проверяется как UTF-8/TOML с диапазонами настроек до сборки.
Иконка должна
иметь расширение `.png` и успешно загружаться через `Assets::load_texture`.
Неиспользуемые файлы каталога Assets не копируются.

Environment `.hdr` включается в архив как обычный объявленный asset из раскрытой
сцены. Это также покрывает HDR, добавленный вложенным prefab; builder не декодирует
HDR при назначении архива, проверка содержимого выполняется asset loader/runtime.

Каждая включённая сцена раскрывается `engine_scene::expand`
([[Engine/EngineScene#Префабы]]); префабы читаются через тот же `ResourceHandle`.
Источник зависимостей — `[[assets]]` раскрытого документа, куда входят ресурсы
префабов. Файлы `.rveprefab`, включая вложенные, считаются ресурсами сцены и получают
архив по тем же правилам: используемые одной сценой — `scene-<stem>`, несколькими —
`shared`. Отсутствующий или невалидный префаб — ошибка проекта с кодом 2.
Lite-, heavy-скрипты и их параметры также берутся из раскрытого документа.

Для каждого lite-скрипта включённой сцены сборщик получает схему `properties` через
`engine_script::lite_properties` и проверяет `params` объекта через
`ParamSchema::resolve` (см. [[Engine/EngineScript]]). Ошибка схемы или параметра —
ошибка проекта с кодом 2: сцена, id объекта и путь скрипта указываются в сообщении.
Выполнение верхнего Lua chunk для чтения схемы ограничено пятью секундами. По истечении
срока chunk прерывается и сборка завершается ошибкой проекта.
После сборки cdylib так же проверяются `params` heavy-скриптов по
`ScriptLibrary::schema`, вместе с проверкой имён.

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

Если Windows-проект задаёт `runtime.fsr_library`, builder проверяет, что loader DLL и
соседний `amd_fidelityfx_upscaler_vk.dll` находятся внутри каталога проекта, и
копирует их в `Engine/` рядом с player. FSR SDK DLL пользователь поставляет сам;
репозиторий их не содержит. Отсутствующая или выходящая за проект DLL — ошибка
входного проекта с кодом 2.

Если Windows-проект задаёт `runtime.dlss_library`, builder проверяет, что
`nvngx_dlss.dll` находится внутри проекта, собирает checkout player с feature `dlss`
и копирует runtime DLL в `Engine/`. Для player build требуется `RVE_DLSS_SDK` с
официальным Windows x64 SDK и `nvsdk_ngx_s.lib`; NGX binaries репозиторий не содержит.
Dev-player использует библиотеку из проекта. При сборке SDK команда включает DLSS,
только если `RVE_DLSS_SDK` задан, и записывает `dlss_enabled` в `rve-sdk.toml`;
`run`/`build` с проектом, требующим DLSS, отклоняют SDK player без этой feature.

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

Без маркера используется checkout-режим. Distribution flavor собирает пакет
`rve-player` командой `cargo build --release --locked -p rve-player
--no-default-features` и задаёт `CARGO_TARGET_DIR=<checkout>/build/player`; dev flavor
собирает default features в `build/player-dev`. Для Windows-проекта с
`runtime.dlss_library` добавляется `--features dlss`, а `RVE_DLSS_SDK` должен
указывать на SDK root с `nvsdk_ngx_s.lib`. Путь к исполняемому файлу берётся из
Cargo `compiler-artifact`; вывод сообщает `rebuilt` или `up to date` по полю `fresh`
этого артефакта.

С маркером используется SDK-режим: `rve-sdk.toml` разбирается как TOML и содержит
`engine_version`, `abi_version`, `target` и `dlss_enabled`. Последнее поле по
умолчанию false для старых SDK и отражает DLSS feature prebuilt player. Маркер
распространяемого SDK должен
указывать текущий `RVE_SCRIPT_ABI_VERSION`; player принимает heavy-script ABI версии
`RVE_SCRIPT_ABI_MIN_VERSION..=RVE_SCRIPT_ABI_VERSION`, чтобы загружать v3-модули с
таблицей скриптов предыдущей версии. `target` должен совпадать с парой `ARCH-OS`, например
`x86_64-windows` или `aarch64-macos`. Команды `run` и `build` выбирают соответственно
соседний `rve-player-dev[.exe]` и `rve-player[.exe]`. Отсутствующий player,
ошибка TOML, несовпадение ABI или платформы завершают команду операционной ошибкой.
SDK-режим не обращается к `CARGO_MANIFEST_DIR` и исходникам checkout.

`rve-build sdk --output <dir> (--rust-toolchain <dir> | --no-rust-toolchain)
[--vulkan-sdk <dir>]` создаёт пакет только из checkout. Готовый пакет работает без
исходников репозитория: marker выбирает соседние `rve-build`, player и гостевой API.
Для автономной сборки Rust-скриптов укажи `--rust-toolchain <dir>`; каталог должен
содержать `bin/cargo` и `bin/rustc` (на Windows `.exe`) и целиком копируется в
`toolchain/`. `--no-rust-toolchain` предназначен для CI и пакетов без autobuild.

Команда пересобирает release editor, builder и оба player; distribution player без
console. Пути executable берутся из Cargo JSON. Кэши изолированы в `build/sdk-player`
и `build/sdk-tools`. На Windows каждому Cargo build передаётся явный target вместе
с `-C target-feature=+crt-static`, поэтому флаг не затрагивает host proc macros.
SDK содержит четыре executable, marker, гостевой `rve-sdk/Cargo.toml` и `src/**`.
На macOS они находятся в `RVE Editor.app/Contents/MacOS`, Vulkan loader/MoltenVK
и ICD — в `Contents/Resources/Engine`. Публикация атомарная без замены output.

Для Windows автономного guest build подходит rustup toolchain
`stable-x86_64-pc-windows-gnu --profile minimal`; её sysroot получают через
`rustup run stable-x86_64-pc-windows-gnu rustc --print sysroot`. В полный bundle
попадают `rust-lld.exe`, self-contained CRT/import libraries и runtime DLL
`libgcc_s_seh-1.dll`/`libwinpthread-1.dll`. Когда bundled sysroot содержит GNU target,
`script_cargo_command` передаёт Cargo encoded rustflags с `linker-flavor=gnu-lld`,
`link-self-contained=yes` и `crt-static`, используя этот linker по абсолютному пути.
Стабильный rustc пока принимает flavor `gnu-lld` только с `-Z unstable-options`,
поэтому builder задаёт `RUSTC_BOOTSTRAP=1` для этого дочернего Cargo. Это позволяет
собирать гостевой cdylib без установленного MinGW/GCC; код связывается с C ABI и
загружается из MSVC editor/builder. На macOS для сборки SDK и guest scripts нужны
Apple Command Line Tools; `rve-build scripts` запускает `xcode-select --install`,
если CLT отсутствуют.

Если `project.rveproject` содержит `heavy_scripts`, каталог обязан содержать
`Cargo.toml` с lib-target `crate-type = ["cdylib"]`. Сборщик читает Cargo metadata,
затем выполняет `cargo build --lib --manifest-path ... --message-format=json` без
`--locked` и с `CARGO_TARGET_DIR=<project>/.rve/target`; для `build` добавляется
`--release`, а `run` использует debug-профиль. Библиотека выбирается из
`compiler-artifact` по package id из metadata, имени target, kind `cdylib` и
расширению текущей платформы. Cargo.lock скриптов остаётся в зоне проекта.

`rve-build scripts <project-dir>` собирает debug cdylib с теми же проверками ABI,
имён и параметров и выводит абсолютный путь последней строкой stdout. Без
`heavy_scripts` выводит пустую строку и завершается успешно. Перед Cargo гостевой
API из SDK либо checkout синхронизируется в `<project>/.rve/sdk/rve-sdk`;
совпадающие файлы сохраняют mtime, удалённые исходники убираются.

При наличии SDK `toolchain/` и metadata, и build запускают его Cargo с явным
`RUSTC`, отдельным `CARGO_HOME=<project>/.rve/cargo-home` и toolchain/bin в начале
PATH. Наследуемые Rust flags, rustup toolchain и compiler wrappers убираются; для
bundled Windows GNU sysroot builder устанавливает только необходимые encoded linker
flags. Без toolchain действует прежний CARGO/PATH. На macOS отсутствие CLT вызывает
`xcode-select --install` и операционную ошибку с просьбой повторить сборку после
установки.

Собранную библиотеку сборщик загружает через `ScriptLibrary` и проверяет, что она
регистрирует каждое имя heavy-скрипта из включённых сцен. Ссылка сцены на heavy без
`heavy_scripts`, отсутствие cdylib или незарегистрированное имя — ошибка проекта с
кодом 2. ABI и модель гостевого crate описаны в [[Engine/RveSdk]].

Команда `run` собирает скрипты в debug-профиле, берёт dev `rve-player` с включённой
developer console из checkout `build/player-dev` или `rve-player-dev[.exe]` из SDK и
запускает его как `--project <absolute-project-dir> [--scripts
<library>]`, затем передаёт пользовательские аргументы. Если скриптов нет, `--scripts`
не добавляется. На Windows player остаётся GUI executable без терминального окна;
dev-вариант показывает developer console внутри окна. Cargo-команда
`cargo run -p rve-player` использует default feature и включает её.

Команда `build` собирает release cdylib и checkout distribution-player без developer
console либо использует соседний distribution player из SDK, затем копирует
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

В SDK-режиме на macOS Build Player берёт Vulkan из bundled `Contents/Resources/Engine`;
явный `--vulkan-sdk` сохраняет приоритет. Windows SDK принимает только toolchain
с GNU `rust-lld.exe` и self-contained libraries. Для встроенного GNU linker используются
`RUSTC_BOOTSTRAP=1` и gated `gnu-lld`; при обновлении Rust нужна повторная smoke-проверка,
в том числе зависимостей с proc-macro и build.rs. Сторонние native зависимости могут
требовать дополнительных инструментов.
