# RVE

RVE — игровой движок на Rust с рендерингом через Vulkan. В репозитории есть
`rve-player` для запуска игровых проектов, `rve-build` для сборки релизов,
`sandbox` для демонстрации движка и `rvepak` для работы с архивами ресурсов.

## Требования

- Rust stable.
- Windows: видеодрайвер с поддержкой Vulkan 1.3.
- macOS: Vulkan loader и MoltenVK. Для Homebrew: `brew install vulkan-loader molten-vk`.

Для обычной сборки Rust-кода Vulkan SDK не требуется.

## Быстрый запуск

Из корня репозитория создай каталог `build/` и запускай команды из него.
Cargo сохраняет там артефакты сборки.

macOS и Linux:

```sh
mkdir -p build
cd build
cargo run -p sandbox
```

Windows PowerShell:

```powershell
New-Item -ItemType Directory -Force build
Set-Location build
cargo run -p sandbox
```

Sandbox показывает вращающийся куб над плоскостью. Для выбора сцены или загрузки
модели можно использовать:

```sh
cargo run -p sandbox -- --scene triangle
cargo run -p sandbox -- --model /models/cube.fbx --texture /textures/cube.png
cargo run -p sandbox -- --frames 120 --render-scale 0.5
```

Ресурсы sandbox размещаются в `build/assets`. Например, путь
`/models/cube.fbx` указывает на `build/assets/models/cube.fbx`. Поддерживаются
сцены `triangle`, `cube` и `shadows`, модели FBX, текстуры PNG, ограничение
числа кадров, изменение размера окна и отладочный оверлей.

Настройки окна sandbox необязательны. Чтобы изменить их, скопируй шаблон
`bootstrap.rves.example` в `build/bootstrap.rves`.

## Запуск и сборка игры

Пример проекта находится в [examples/demo-game](examples/demo-game). Он содержит
настройки проекта, сцены, ресурсы, Lua-скрипт и пример Rust-скрипта.

Из каталога `build/`:

```sh
cargo run -p rve-player -- --project ../examples/demo-game
cargo run -p rve-build -- run ../examples/demo-game -- --frames 120
cargo run -p rve-build -- build ../examples/demo-game --output ../demo-out
```

Первая команда запускает проект в режиме разработки. Вторая запускает его через
сборщик, а третья создаёт релиз в указанном каталоге. Каталог релиза должен
отсутствовать до сборки.

На macOS релиз создаётся как `demo-out/RVE Demo.app`: открой его в Finder для
запуска без Terminal. Ресурсы и Vulkan-библиотеки находятся внутри бандла в
`Contents/Resources`. На Windows исполняемый файл, `Engine/` и `Assets/` остаются
рядом в каталоге релиза. Подробности — в [описании сборщика](Obsidian/RVEVault/Engine/RveBuild.md)
и [runtime-путей](Obsidian/RVEVault/Engine/EngineRuntime.md).

## Архивы ресурсов

Инструмент `rvepak` собирает, просматривает и проверяет наборы архивов по
TOML-манифесту. Пример манифеста — [rvepak.example.toml](rvepak.example.toml);
нормативное описание формата — [docs/rvepak-format.md](docs/rvepak-format.md).

Перед сборкой помести файлы в `build/assets`. Пример манифеста создаёт набор в
`build/game/assets`; каталог назначения должен отсутствовать. Из каталога
`build/` выполни:

```sh
cargo run -p rvepak -- build ../rvepak.example.toml
cargo run -p rvepak -- list game/assets
cargo run -p rvepak -- verify game/assets
```

## Техническая документация

[Карта архитектуры и заметки Obsidian](Obsidian/RVEVault/Index.md) содержат
устройство компонентов и их контракты. Подробности смотрите в заметках:

- [Настройки проекта](Obsidian/RVEVault/Engine/EngineProject.md)
- [Формат сцен](Obsidian/RVEVault/Engine/EngineScene.md)
- [Загрузка ресурсов](Obsidian/RVEVault/Engine/EngineResources.md)
