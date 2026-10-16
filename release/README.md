# Neko Demo — macOS

Запуск: откройте `Neko Demo.app` двойным щелчком в Finder. Terminal не требуется.
Для переноса приложения копируйте весь `.app` целиком.

Модель `neko.fbx` и текстура `tx.png` взяты из приложенных файлов без изменения.
Объект вращается вокруг вертикальной оси Y со скоростью 30°/с (оборот за 12 секунд).
Камера неподвижна; модель центрирована относительно оси вращения.
Освещение без направленного света сохраняет цвета исходной текстуры.

## Содержимое

- `Neko Demo.app/Contents/MacOS/Neko Demo` — release player для Apple Silicon.
- `Neko Demo.app/Contents/Resources/Assets/base.rvepak` — сцена и настройки/реестр проекта.
- `Neko Demo.app/Contents/Resources/Assets/neko.rvepak` — FBX, PNG и Lua-скрипт вращения.
- `Neko Demo.app/Contents/Resources/Engine/` — bootstrap и Vulkan/MoltenVK.
- `project/` — исходный проект для редактирования и пересборки.

Готовое приложение читает ресурсы только из архивов внутри `.app`.
Каталог `project` для запуска приложения не нужен.

## Проверка архивов

Из каталога `build` репозитория:

```sh
cargo run -p rvepak --locked -- verify "../release/Neko Demo.app/Contents/Resources/Assets"
cargo run -p rvepak --locked -- list "../release/Neko Demo.app/Contents/Resources/Assets"
```

## Пересборка

Из корня репозитория, используя свободные имена каталогов:

```sh
mkdir -p build
cp -R release/project build/neko-rebuild-project
cd build
cargo run -p rve-build --locked -- build neko-rebuild-project --output ../release-rebuilt
```

Сборщик требует, чтобы output ещё не существовал. Cargo-кэш проекта останется внутри `build`.

## Проверено

- Release-сборка macOS; `Info.plist` прошёл `plutil -lint`.
- Проверка целостности двух архивов: 6 путей, 6 blobs.
- Байты упакованных модели, текстуры, сцены и скрипта совпадают с исходниками.
- Проверка скрипта через ScriptHost: нулевой dt не меняет угол; за 120 шагов по 1/60 с угол Y меняется на 60°; ошибок скрипта нет.
- Готовый player без `--project` отработал 240 кадров и завершился с кодом 0.
- Запуск `.app` через Launch Services, визуальная проверка модели и текстуры.

Сборка предназначена для macOS Apple Silicon. Windows-версия здесь не собрана.
