# EngineConsole

Parent: [[Index]]

## Назначение и границы

Developer console разделена между `engine-console` (логи, команды и история),
`engine-ui` (общий egui renderer bridge и overlay), runtime player и editor.
Базовый `engine-console` не зависит от окна или GPU и не хранит состояние UI.

## Лог и команды

`ConsoleBuffer` хранит до 2000 строк в порядке поступления, удаляет старейшие при
переполнении и подавляет повтор последней идентичной строки. Строка содержит уровень
Info/Warn/Error, source и text; generation меняется при изменении содержимого.
`ConsoleLogger` отправляет Info в stdout, Warn/Error в stderr и, если задан sink,
сохраняет запись в общем буфере. Установка process logger выполняется один раз.

`parse_command_line` разделяет аргументы по пробелам и поддерживает кавычки и
экранированные кавычки; имена команд нормализуются в нижний регистр и проверяются по
`[a-z_][a-z0-9_.]{0,63}`. `CommandCatalog` содержит встроенные `help [prefix]` и
`clear`, имена зарезервированы. Подсказки сортируются по имени; сначала идут совпадения
по префиксу, затем подстрочные, а Tab дополняет общий префикс. `CommandHistory` хранит
до 100 непустых команд и не дублирует последнюю запись.

## Общий egui overlay

`engine-ui` содержит вынесенный из editor bridge (`UiBridge`, `ManagedTextures`,
`UiTextures`) и `ConsoleOverlay`. Overlay получает прямоугольник, снимок строк и каталог
команд, возвращая только отправленную строку и переключение видимости; scrollback и
история остаются у приложения. Панель имеет тёмный полупрозрачный фон, занимает верхние
45% заданного прямоугольника и прокручивает вывод к низу. PageUp/PageDown меняют
прокрутку. `FontId::monospace` использует встроенный default Hack с кириллицей. Enter
отправляет команду и очищает поле; приложения добавляют её в scrollback с префиксом
`> `. Tab выполняет автодополнение, стрелки выбирают подсказки либо историю, Escape
закрывает overlay. Физический Backquote переключает видимость и не вводит символ в поле.

## Player

В `rve-player` feature `dev-console` включена по умолчанию и передаётся в
`engine-runtime`; реализация и UI-зависимости runtime условны по этой feature.
`run_player` устанавливает logger после проверки CLI и подключает буфер как sink.
Когда overlay открыт, player блокирует игровой ввод, а закрытие и потеря фокуса
передают удерживаемые клавиши через обычный путь release в `InputAccumulator`.
Script-команды доставляются на scripts thread перед очередным update; каталог команд
публикуется снимком. Ошибки неизвестной команды и callback идут в консоль. Overlay
рисуется на render thread через очередь последнего UI-кадра; при замене кадра texture
delta объединяется, а геометрия старого кадра отбрасывается. GPU-текстуры освобождаются
при закрытии overlay и завершении runtime.

Distribution `rve-build build` выключает default features у player и не включает
console-код. Cargo-вызов `cargo build --release -p rve-player` сохраняет включённые по
умолчанию features и поэтому сам по себе console не отключает. Сборку flavor-ов и SDK
см. в [[Engine/RveBuild]].

## Editor

Editor использует тот же `ConsoleOverlay` поверх прямоугольника Game после построения
dock UI. Существующая dock-панель Console остаётся журналом `EditorState::log`;
скриптовые события текущего кадра добавляются туда. Backquote работает, если нет
модального окна; открытый overlay блокирует ввод Play и W/E/R shortcuts. `help` и
`clear` доступны всегда, остальные команды выполняются только в Play через
`PlaySession::execute_command` и тот же watchdog. Вне Play команда завершается ошибкой
`доступно только в Play`.

## Ограничения и проверки

UI и command parsing имеют headless-проверки без окна и GPU. Runtime overlay и Vulkan
ресурсный цикл требуют ручной проверки на целевой Windows/macOS с подходящим Vulkan
драйвером и не покрываются этими тестами. Lua можно прервать watchdog-ом; синхронный
Rust callback останавливается только на вызове API движка или проверке `is_interrupted`; см. [[Engine/EngineScript]] и
[[Engine/RveSdk]].

## Источники

`engine/engine-console/src/lib.rs`, `engine/engine-ui/src`,
`engine/engine-runtime/src/console.rs`, `engine/engine-runtime/src/player.rs`,
`editor/src/console.rs`, `editor/src/play.rs`, `editor/src/app.rs`.
