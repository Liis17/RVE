# RVE

RVE — игровой движок на Rust с рендерингом через Vulkan, скриптами на Lua и Rust
и физикой с фиксированным шагом. В репозитории есть `rve-player` для запуска
игровых проектов, `rve-build` для сборки релизов и SDK, `rve-editor` — графический
редактор сцен, `sandbox` для демонстрации движка и `rvepak` для работы с архивами
ресурсов.

## Требования

- Rust stable.
- Windows: видеодрайвер с поддержкой Vulkan 1.3.
- macOS: Vulkan loader и MoltenVK. Для Homebrew: `brew install vulkan-loader molten-vk`.

Для обычной сборки Rust-кода Vulkan SDK не требуется. Полные требования к
устройству — в [заметке renderer](Obsidian/RVEVault/Engine/EngineRender.md).

## Быстрый запуск

Cargo хранит артефакты в `build/` в корне репозитория; команды запускай из него.

```sh
mkdir -p build
cd build
cargo run -p sandbox
```

Sandbox показывает вращающийся куб над плоскостью. Примеры параметров:

```sh
cargo run -p sandbox -- --scene materials --frames 300
cargo run -p sandbox -- --model /models/cube.fbx --texture /textures/cube.png
cargo run -p sandbox -- --frames 120 --render-scale 0.5
```

Ресурсы sandbox лежат в `build/assets`. Настройки окна задаются необязательным
файлом `build/bootstrap.rves` (шаблон — `bootstrap.rves.example`). Подробнее —
в заметке [Sandbox](Obsidian/RVEVault/Engine/Sandbox.md).

## Запуск и сборка игры

Пример проекта — [examples/demo-game](examples/demo-game): сцены, физика,
Lua-скрипт и Rust-скрипт на `rve-sdk`. Из каталога `build/`:

```sh
cargo run -p rve-player -- --project ../examples/demo-game
cargo run -p rve-build -- run ../examples/demo-game -- --frames 120
cargo run -p rve-build -- build ../examples/demo-game --output ../demo-out
cargo run --release -p rve-build -- sdk --output ../rve-sdk-dist --rust-toolchain <toolchain-dir>
```

- `rve-player --project` — запуск проекта без сборки Rust-скриптов.
- `rve-build run` — собирает скрипты и запускает dev-player с developer console (клавиша `` ` ``/`ё`).
- `rve-build build` — собирает релиз без console в новый каталог.
- `rve-build sdk` — создаёт переносимый SDK: редактор, сборщик, player и Rust toolchain.

В dev-player клавиша F3 переключает оверлей профайлера: CPU-фазы потоков и renderer, GPU-проходы первого вида, шаги скриптов, статистику запросов ресурсов и замеры старта. Оверлей работает при закрытой консоли и не блокирует игровой ввод. Команда `profile` в консоли печатает те же отчёты. Если скрипт регистрирует команду с таким же именем, встроенная `profile` имеет приоритет и в лог пишется предупреждение.

## Редактор

```sh
cargo run -p rve-editor -- --project ../examples/demo-game
```

Без `--project` открывается стартовый экран; File → New Project… создаёт проект
из шаблона. Управление и ограничения — в заметке [RveEditor](Obsidian/RVEVault/Engine/RveEditor.md).

Вкладка Profiler в редакторе закрыта по умолчанию. Она открывается из меню вкладок и показывает CPU-фазы renderer, GPU-проходы видов Scene и Game, фазы шага Play и статистику ресурсов.

## Архивы ресурсов

`rvepak` собирает, просматривает и проверяет архивы по TOML-манифесту
([пример](rvepak.example.toml), [формат](docs/rvepak-format.md)):

```sh
cargo run -p rvepak -- build ../rvepak.example.toml
cargo run -p rvepak -- verify game/assets
```

## Документация

Архитектура и контракты — в [базе знаний Obsidian](Obsidian/RVEVault/Index.md).
Основные темы:

- [Настройки проекта: ввод, физика, FSR и DLSS](Obsidian/RVEVault/Engine/EngineProject.md)
- [Формат сцен](Obsidian/RVEVault/Engine/EngineScene.md)
- [Физика](Obsidian/RVEVault/Engine/EnginePhysics.md)
- [Lua-скрипты](Obsidian/RVEVault/Engine/EngineScript.md) и [Rust SDK](Obsidian/RVEVault/Engine/RveSdk.md)
- [Developer console](Obsidian/RVEVault/Engine/EngineConsole.md)
- [Runtime и player](Obsidian/RVEVault/Engine/EngineRuntime.md)
- [Сборка, SDK, CI и релизы](Obsidian/RVEVault/Engine/RveBuild.md)
- [Renderer](Obsidian/RVEVault/Engine/EngineRender.md)
