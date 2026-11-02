# Neko Demo

Тестовый проект с исходными `neko.fbx` и `tx.png`. Модель вращается вокруг
своего центра по оси Y со скоростью 30°/с (полный оборот за 12 секунд).
Камера неподвижна, освещение мягкое, тени отключены.

## Запуск готовой сборки Windows

Запустите `NekoDemo/NekoDemo.exe`. Рядом с exe должны оставаться каталоги
`Engine` и `Assets`. Для выхода закройте окно.
Требуется Vulkan-драйвер видеокарты.
Release-плеер запускается без отдельного консольного окна Windows.

В `NekoDemo/Assets` лежат только упакованные ресурсы:

- `base.rvepak` — сцена, настройки проекта и реестр архивов;
- `scene-neko.rvepak` — FBX, PNG и Lua-скрипт вращения.

Player читает ресурсы через реестр RVEPAK; исходники из `project/Assets`
для запуска готовой сборки не требуются. Для переноса Windows-сборки можно
распаковать `NekoDemo.zip`.

## Запуск готовой сборки macOS

Откройте `Neko Demo.app` двойным щелчком в Finder. Terminal не требуется.
Сборка предназначена для Apple Silicon. Для переноса копируйте весь `.app`
целиком или распакуйте `Neko Demo.zip`.

Внутри бандла находятся:

- `Contents/MacOS/Neko Demo` — release player;
- `Contents/Resources/Assets/base.rvepak` — сцена и настройки/реестр проекта;
- `Contents/Resources/Assets/neko.rvepak` — FBX, PNG и Lua-скрипт;
- `Contents/Resources/Engine/` — bootstrap и Vulkan/MoltenVK.

Сохранённая macOS-сборка использует `main.rvescene` без направленного света.
В общем исходном проекте стартовой остаётся `neko.rvescene` с мягким освещением;
вариант `main.rvescene` сохранён выключенной сценой. Для его сборки включите
`main.rvescene` и выключите `neko.rvescene` в `project/project.rveproject`.
Оба варианта используют исходные модель и текстуру, центрирование и вращение 30°/с.
Имя общего проекта — `NekoDemo`: при пересборке на macOS получится `NekoDemo.app`.

## Повторная сборка

Команды выполняются из `build/` в корне репозитория:

```powershell
cargo run -p rve-build --locked -- build ../release/project --output ../release/NekoDemo
```

Сборщик требует отсутствующий каталог output и не перезаписывает готовый релиз.
Для следующей сборки укажите другой каталог, например `../release/NekoDemo-next`.
Готовая сборка и кэш `.rve` исключены из Git; исходный проект сохранён в `project/`.
В этой рабочей копии `project/.rve` связан с `build/neko-project-cache`, чтобы
артефакты компиляции хранились в `build/`.

Для macOS используйте тот же исходный проект и свободный output, например
`../release/neko-macos-rebuilt`. Чтобы кэш компиляции оставался в `build/` без
локальной ссылки `.rve`, сначала скопируйте `release/project` в
`build/neko-rebuild-project`, исключив `.rve`, и собирайте эту копию.

## Проверки

После сборки, из `build/`:

```powershell
cargo test --manifest-path ../release/tests/Cargo.toml --locked
../release/NekoDemo/NekoDemo.exe --frames 120
```

Автотесты без GPU проверяют исходные и упакованные ресурсы, соответствие байтов
архива исходникам, загрузку FBX/PNG и вращение через настоящий Lua host:
нулевой шаг времени, скорость, полный оборот и дробный шаг.
Вторая команда отдельно проверяет Vulkan-путь готового player.

Архивы сохранённого macOS-бандла можно проверить из `build/`:

```sh
cargo run -p rvepak --locked -- verify "../release/Neko Demo.app/Contents/Resources/Assets"
```

Пересобранная Windows-версия проверяется на Windows. Проверка запуска macOS-бандла
после объединения изменений требует macOS.
