# RveSdk

Parent: [[Index]]

## Назначение

`rve-sdk` — самостоятельный крейт без зависимостей, который задаёт C ABI и гостевой Rust API для игровых скриптов, собираемых в `cdylib`. Он не зависит от движка и включает `Script`, `Context`, `Transform`, а также owned-типы значений и параметров. Host-side загрузка и вызовы описаны в [[Engine/EngineScript]].

Игровой crate подключает SDK как зависимость и объявляет cdylib-target:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
rve-sdk = { path = "<sdk>/rve-sdk" }
```

Каталог `<sdk>` создаётся командой `rve-build sdk --output <dir>` из checkout и
содержит standalone crate `rve-sdk/` без `target/`. Для разработки из checkout можно
указывать `../../engine/rve-sdk`; собранный SDK подключается тем же path-зависимым
способом без исходников движка. При распространении пары бинарников `rve-build` и
`rve-player` сохраняйте рядом `rve-sdk.toml`: он включает SDK-режим и фиксирует ABI
и платформу.

Реализация создаёт тип, реализующий `rve_sdk::Script`, и функцию-фабрику,
принимающую `&rve_sdk::Params`. `rve_sdk::export_scripts!` экспортирует ABI entry
point с именами heavy-скриптов, на которые ссылаются сцены. Пример —
`examples/demo-game/HeavyScripts`; `rve-build` проверяет cdylib и имена скриптов при
сборке проекта ([[Engine/RveBuild]]).

## Контракт ABI

- Для этого релиза зафиксирован стабильный `RVE_SCRIPT_ABI_VERSION = 1`. Любое несовместимое изменение layout ABI требует повышения версии. `RveModule` начинается с `RveHeader` с версией и размером заголовка; host сначала читает и проверяет его и только затем интерпретирует таблицу скриптов.
- Таблица модулей и описания скриптов рассчитаны на статическое время жизни. Макрос `script_module!` строит статическую таблицу; `export_scripts!` экспортирует функцию `rve_script_module`, возвращающую указатель на модуль.
- `RveHost` и его opaque context передаются на время одного guest callback и действуют только в нём и на вызывающем потоке. Сохранять их указатели после вызова нельзя.
- ABI-строки, значения и параметры передаются как заимствованные данные. SDK копирует входные значения и параметры в owned `Value` и `Params`; память, выделенную одной стороной, освобождает только её владелец, вторая сторона не должна освобождать чужую память.
- Handle объекта кодирует `index + 1`; нулевой handle означает отсутствие объекта.
- `RveHost` предоставляет `find_object`, `object_id`, `object_name`, `get_local`, `set_local`, `world_matrix`, `send` и `broadcast`, а также `log` и callbacks ввода. `Key` и `MouseButton` имеют явные стабильные коды; key/button state — битовая комбинация `DOWN = 1`, `PRESSED = 2`, `RELEASED = 4`. Неизвестный код возвращает ноль. `cursor_position`, `mouse_delta` и `wheel_delta` копируют `[f32; 2]` через выходной указатель и возвращают `bool`. Чтение в `create` разрешено; начальный снимок может быть нулевым. Host-функции работают с любым валидным объектом текущего мира. `world_matrix` копирует 16 `f32` в column-major порядке. Возвращаемые строки и все указатели действительны только до завершения callback.
- `send` адресует объект по scene id до 256 UTF-8 байт, `broadcast` адресует все включённые экземпляры. Оба callback принимают borrowed `RveValue`; ABI host проверяет и копирует его в `ParamValue` до возврата. Неизвестная или слишком длинная цель, а также значение, отклонённое ограничениями сообщений, возвращают `false`. Во время `create` host запрещает записи и отправку сообщений, но разрешает чтение и логирование. Ограничения сообщений описаны в [[Engine/EngineScript]].
- Каждый lifecycle callback, включая каждую доставку `message`, транзакционный. Локальные записи и сообщения применяются только после успешного завершения callback; при `Error`, `Panicked`, poisoned host callback или не конечной трансформации они отбрасываются. Логи не откатываются.
- Callback возвращает `RveStatus`: `Ok` при успехе, `Error` при ошибке и `Panicked` при пойманной панике. Текст ошибки передаётся host через `set_error`.
- Обе стороны ABI не допускают unwind через C boundary: SDK-обёртки ловят Rust panic и возвращают статус, host ловит панику внутри host callback и отклоняет весь вызов. Сборка guest-кода с `panic = "abort"` завершает процесс при панике; перехватить её нельзя.

## Гостевой API Rust

`Script` поддерживает `on_start`, `on_update`, `on_message` и `on_destroy`. `on_message(&mut self, ctx, sender, name, value)` вызывается для каждой доставки; `Value` уже скопирован в owned память гостя. `Context::find(id)` возвращает `Option<Object>`; `Object` — `Copy` handle без указателя на host, стабильный в пределах загруженной сцены и пригодный для хранения в состоянии скрипта. Каждая операция с handle требует `Context` текущего callback.

Через `Object` можно прочитать или записать локальную трансформацию, получить column-major world matrix, id и name. `Context::send(target_id, name, &Value)` и `Context::broadcast(name, &Value)` возвращают `bool`; `false` означает неизвестную либо превышающую 256 UTF-8 байт цель для `send`, недоступную фазу записи или отказ host из-за ограничений сообщения. `Context::log_info`, `log_warn` и `log_error` создают соответствующий уровень события. `Context.transform` остаётся удобным полем собственной трансформации.

`Context::input()` возвращает read-only `Input`: `key_down/pressed/released(Key)`, `mouse_down/pressed/released(MouseButton)`, `cursor()`, `mouse_delta()` и `wheel()`. Cursor — физические пиксели окна; до первого `CursorMoved` позиция равна `[0, 0]`. `CursorLeft` не обрабатывается, поэтому позиция сохраняется после выхода курсора из окна, потери фокуса и перехода в Background. Mouse delta — относительное движение устройства из `DeviceEvent::MouseMotion`, wheel — накопление в строках. На Windows для движения используется raw input; на macOS дельта может включать системное ускорение, это не проверено. На macOS при зажатом `Cmd` система может не прислать key-up других клавиш, и такая клавиша останется down до потери фокуса; это известное ограничение платформы, не проверенное в проекте. `Key` и `MouseButton` совпадают по именам с `engine-window` и Lua API. Снимок одинаков для всех экземпляров одного update; `create` видит пустой снимок, `start` — последний установленный snapshot или пустой, если update ещё не было. Перед `destroy` host очищает pressed/released и дельты, сохраняя down и позицию курсора последнего snapshot.

Если callback меняет и `Context.transform`, и собственную трансформацию через `Object::set_transform`, конечное изменившееся поле `Context.transform` перекрывает запись handle. Если поле осталось равным начальному значению, применяется запись через handle. Матрица мира остаётся снимком фазы: локальная запись не пересчитывает её внутри вызова или до следующей фазы. Полный lifecycle и расписание сообщений описаны в [[Engine/EngineScript]].

## Источники

`engine/rve-sdk/Cargo.toml`, `engine/rve-sdk/src/abi.rs`, `engine/rve-sdk/src/lib.rs`.
