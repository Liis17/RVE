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

`rve-build scripts` синхронизирует гостевой crate в `<project>/.rve/sdk/rve-sdk`
перед сборкой, независимо от того, запущен ли builder из checkout или переносимого
SDK. Новый проект использует этот путь из `Scripts/Cargo.toml`:

```toml
[dependencies]
rve-sdk = { path = "../.rve/sdk/rve-sdk" }
```

Папка `.rve/` находится вне `Assets/` и исключена шаблонным `.gitignore`.
Синхронизация обновляет изменённые файлы, сохраняет mtime совпадающих и удаляет
исходники API, которые исчезли из установленной версии.

SDK marker рядом с переносимым `rve-build` фиксирует точный ABI. Marker ABI v6
несовместим с builder/player ABI v7: пересоздай переносимый комплект командой
`rve-build sdk`. Для проектов builder синхронизирует текущий SDK в `.rve/sdk`
перед сборкой Rust scripts.

Реализация создаёт тип, реализующий `rve_sdk::Script`, и функцию-фабрику,
принимающую `&rve_sdk::Params`. `rve_sdk::export_scripts!` экспортирует ABI entry
point с именами heavy-скриптов, на которые ссылаются сцены. Пример —
`examples/demo-game/HeavyScripts`; `rve-build` проверяет cdylib и имена скриптов при
сборке проекта ([[Engine/RveBuild]]).

Скрипт может объявить типизированные параметры необязательным блоком после фабрики:

```rust
rve_sdk::export_scripts! {
    "Bobber" => create_bobber {
        amplitude: f32 = 0.15,
        speed: f32 = 1.4 (0.0..=10.0) "Speed",
    }
}
```

- Типы: `bool`, `i64`, `f32`/`f64` (float), `&str`, `[f32; 3]` (vec3).
- Значения по умолчанию — литералы, для vec3 — массив из трёх литералов.
- Диапазон `(min..=max)` допустим только у числовых типов. Label — необязательный литерал в конце.
- Таблицы строятся в const-контексте как `'static`.
- Host проверяет значения и заполняет default до вызова фабрики, поэтому `Params` приходит заполненным ([[Engine/EngineScript]]).
- Форма без блока остаётся рабочей: такой скрипт схемы не объявляет и получает `params` сцены без проверок.

Синтаксис отличается от «естественного» Rust из-за ограничений `macro_rules`: после фрагмента `expr` нельзя ставить `(`, поэтому default — литерал.

## Контракт ABI

- Текущая версия — `RVE_SCRIPT_ABI_VERSION = 7`, минимальная принимаемая — `RVE_SCRIPT_ABI_MIN_VERSION = 3`. ABI v7 дописывает в конец `RveHost` nullable audio callbacks `audio_play_source`, `audio_stop_source`, `audio_set_source_volume`, `audio_play_clip`, `audio_stop_voice` и `audio_set_group_volume`, а также `RveAudioParams` (`group`, `volume`, `looping`, `has_position`, `position`). Callbacks возвращают `RveStatus` как `u32`: `0` — успех, `1` — отказ; причина сразу попадает в события скрипта как Error, даже если гость проигнорировал статус. Идентификаторы голосов — `u64`, ненулевые. ABI v3 добавил `params`/`param_count`, ABI v4 — nullable `command` в конец дескриптора, ABI v6 — nullable `collision` в конец `RveScriptDesc`. Host сохраняет отдельные `RveScriptDescV3` и `RveScriptDescV4` layouts и нормализует их без чтения недостающего хвоста: у v3 нет command и collision callback, у v4 и v5 — command без collision. В конец `RveHost` ABI v4 добавлены nullable `register_command`/`unregister_command`; ABI v5 дописывает nullable action-input callbacks `action_state`, `action_value`, `input_context`, `set_input_context`, `rebind_action`, `action_binding` и `reset_bindings`; ABI v6 дописывает nullable physics callbacks для raycast, dynamic body commands, character movement/grounding и collider enable state, сохраняя прежние префиксы. `action_state` использует флаги `RVE_INPUT_DOWN/PRESSED/RELEASED` и принимает только button actions; `action_value` читает axis/vector2 по `RVE_ACTION_*`, помещая значение axis в `x`. Для пустого корректного binding slot `action_binding` успешно возвращает пустую строку на index 0; неверный контекст/действие/слот возвращает `false`. Guest-код ABI3/4/5 сохраняет совместимость через свой префикс `RveHost` и не читает хвост v6. ABI v1/v2 не принимаются. ABI v2 ранее добавлял camera callbacks в конец `RveHost`, сохранив v1-префикс. Любое несовместимое изменение layout требует повышения версии. `RveModule` начинается с `RveHeader` с версией и размером заголовка; host сначала читает и проверяет его и только затем интерпретирует таблицу соответствующей версии.
- Таблица модулей и описания скриптов рассчитаны на статическое время жизни. Макрос `script_module!` строит статическую таблицу; `export_scripts!` экспортирует функцию `rve_script_module`, возвращающую указатель на модуль.
- `RveHost` и его opaque context передаются на время одного guest callback и действуют только в нём и на вызывающем потоке. Сохранять их указатели после вызова нельзя.
- ABI-строки, значения и параметры передаются как заимствованные данные. SDK копирует входные значения и параметры в owned `Value` и `Params`; память, выделенную одной стороной, освобождает только её владелец, вторая сторона не должна освобождать чужую память.
- Handle объекта кодирует `index + 1`; нулевой handle означает отсутствие объекта.
- `RveHost` предоставляет `find_object`, `object_id`, `object_name`, `get_local`, `set_local`, `world_matrix`, `send` и `broadcast`, а также `log` и callbacks ввода. `Key` и `MouseButton` имеют явные стабильные коды; key/button state — битовая комбинация `DOWN = 1`, `PRESSED = 2`, `RELEASED = 4`. Неизвестный код возвращает ноль. `cursor_position`, `mouse_delta` и `wheel_delta` копируют `[f32; 2]` через выходной указатель и возвращают `bool`. ABI v2 дописывает nullable callbacks `get_camera_settings`, `set_camera_settings`, `active_camera` и `set_active_camera`; ABI v5 дописывает nullable callbacks физики. Отсутствующие optional callbacks представлены SDK как `PhysicsError::Unavailable`, неудачный вызов — `Rejected`, а конечность и диапазоны входа проверяются до вызова. `RveCameraSettings` — `repr(C)` из 40 байт: `fov_deg`, `near`, `far`, `render_scale`, `clear_color`, `exposure_ev`, `anti_aliasing` и `tonemapper`; последние два поля — `u32` с фиксированными кодами. Чтение в `create` разрешено; начальный снимок может быть нулевым. Host-функции работают с любым валидным объектом текущего мира. `world_matrix` копирует 16 `f32` в column-major порядке. Возвращаемые строки и все указатели действительны только до завершения callback.
- `send` адресует объект по scene id до 256 UTF-8 байт, `broadcast` адресует все включённые экземпляры. Оба callback принимают borrowed `RveValue`; ABI host проверяет и копирует его в `ParamValue` до возврата. Неизвестная или слишком длинная цель, а также значение, отклонённое ограничениями сообщений, возвращают `false`. Во время `create` host запрещает записи и отправку сообщений, но разрешает чтение и логирование. Ограничения сообщений описаны в [[Engine/EngineScript]].
- Каждый lifecycle callback, включая каждую доставку `message` и physics event, транзакционный. Локальные записи, camera settings, выбор активной камеры, сообщения, body-команды, grounded state и collider enable state применяются только после успешного завершения callback; настройки камеры дополнительно проверяются по диапазонам. При `Error`, `Panicked`, poisoned host callback, не конечной трансформации или неверных настройках изменения отбрасываются. Логи не откатываются.
- Callback возвращает `RveStatus`: `Ok` при успехе, `Error` при ошибке и `Panicked` при пойманной панике. Текст ошибки передаётся host через `set_error`.
- Обе стороны ABI не допускают unwind через C boundary: SDK-обёртки ловят Rust panic и возвращают статус, host ловит панику внутри host callback и отклоняет весь вызов. Сборка guest-кода с `panic = "abort"` завершает процесс при панике; перехватить её нельзя.

## Гостевой API Rust

`Script` поддерживает `on_start`, `on_update`, `on_message`, `on_collision_enter`, `on_collision_exit`, `on_trigger_enter`, `on_trigger_exit` и `on_destroy`. `on_message(&mut self, ctx, sender, name, value)` вызывается для каждой доставки; `Value` уже скопирован в owned память гостя. Physics events идут единым nullable ABI v5 callback с kind, other handle, point и normal; SDK dispatch вызывает соответствующий trait method, default implementation ничего не делает. `Context::find(id)` возвращает `Option<Object>`; `Object` — `Copy` handle без указателя на host, стабильный в пределах загруженной сцены и пригодный для хранения в состоянии скрипта. Каждая операция с handle требует `Context` текущего callback.

Через `Object` можно прочитать или записать локальную трансформацию, получить column-major world matrix, id и name. `Object::camera_settings(ctx)` возвращает `Option<CameraSettings>`; `set_camera_settings(ctx, settings)` и `activate_camera(ctx)` возвращают `false`, если callback недоступен, объект не камера или запись запрещена. `CameraSettings` содержит `fov_deg`, `near`, `far`, `render_scale`, линейный `clear_color`, `exposure_ev`, `anti_aliasing` и `tonemapper`; enum коды доступны через `from_code` и `code`. `Context::active_camera()` возвращает `Option<Object>`. Отсутствующий camera callback означает API недоступен и аккуратно отображается в `None`/`false`. `Context::send(target_id, name, &Value)` и `Context::broadcast(name, &Value)` возвращают `bool`; `false` означает неизвестную либо превышающую 256 UTF-8 байт цель для `send`, недоступную фазу записи или отказ host из-за ограничений сообщения. `Context::log_info`, `log_warn` и `log_error` создают соответствующий уровень события. `Context.transform` остаётся удобным полем собственной трансформации.

`Context::physics().raycast` выполняет read-only запрос и по умолчанию исключает triggers; host также исключает капсулу controller текущего скрипта. `raycast_with_options` задаёт `include_triggers` и optional ignored object. `Object` предоставляет dynamic-only `get_velocity`, `set_velocity`, `add_impulse` и `add_force`; controller-only `move_character` и `is_grounded`; а также `set_collider_enabled`/`is_collider_enabled`. Методы возвращают `PhysicsError::{Unavailable, Rejected, InvalidInput}` для отсутствующего optional callback, отказа host и некорректного вектора или результата.

`Context::input()` возвращает read-only `Input`: `key_down/pressed/released(Key)`, `mouse_down/pressed/released(MouseButton)`, `cursor()`, `mouse_delta()` и `wheel()` остаются сырым API. `Input::action_down`, `action_pressed` и `action_released` возвращают button edges; `Input::axis` и `Input::vector` возвращают `Option`, `Input::context()` — активный контекст как `Option<String>`, а `Input::bindings(context, action, slot)` различает неверный слот (`None`) и пустой (`Some(Vec::new())`). `Input::pressed_inputs()` перечисляет raw physical inputs с pressed edge. Запись выполняется через `Context::set_input_context`, `Context::rebind(context, action, slot, &[PhysicalInput])` и `Context::reset_bindings`; bool `false` означает отсутствующий callback или отклонённую запись. `PhysicalInput` представляет `Key` или `MouseButton` и умеет разбирать/форматировать `Mouse.<Button>`.

Cursor — физические пиксели окна; до первого `CursorMoved` позиция равна `[0, 0]`. `CursorLeft` не обрабатывается, поэтому позиция сохраняется после выхода курсора из окна, потери фокуса и перехода в Background. Mouse delta — относительное движение устройства из `DeviceEvent::MouseMotion`, wheel — накопление в строках. На Windows для движения используется raw input; на macOS дельта может включать системное ускорение, это не проверено. На macOS при зажатом `Cmd` система может не прислать key-up других клавиш, и такая клавиша останется down до потери фокуса; это известное ограничение платформы, не проверенное в проекте. `Key` и `MouseButton` совпадают по именам с `engine-window` и Lua API. Снимок одинаков для всех экземпляров одного update; `create` видит пустой сырой snapshot, `start` — последний установленный snapshot или пустой, если update ещё не было. Перед `destroy` host очищает pressed/released и дельты, сохраняя down и позицию курсора последнего snapshot.

`Context::console` действует только в рамках текущего callback. `write(text, ConsoleLevel::{Log, Warning, Error})` отправляет сообщение соответствующего уровня. `register(name, Option<&str>)` и `unregister(name)` управляют принадлежащими экземпляру командами; для обработки реализуется `Script::on_command(ctx, name, args)`. Команды используют ABI v4 callbacks, а вызов гостевого обработчика транзакционный; детали регистрации и срока жизни описаны в [[Engine/EngineConsole]].

Если callback меняет и `Context.transform`, и собственную трансформацию через `Object::set_transform`, конечное изменившееся поле `Context.transform` перекрывает запись handle. Если поле осталось равным начальному значению, применяется запись через handle. Матрица мира остаётся снимком фазы: локальная запись не пересчитывает её внутри вызова или до следующей фазы. Полный lifecycle и расписание сообщений описаны в [[Engine/EngineScript]].

Звуковой API гостя: `Object::play_sound`, `stop_sound`, `set_sound_volume` и `Context::play_clip(path, ClipOptions)`, `stop_voice`, `set_group_volume`. `play_clip` возвращает id голоса. Ошибки — `AudioError`: `Unavailable` для хоста без audio callbacks (ABI до v7), `Rejected` при отказе хоста, `InvalidInput` для неконечных значений и громкости вне `0..=4` (`MAX_AUDIO_VOLUME`, совпадает с `engine_scene::MAX_AUDIO_VOLUME`). Клип объявляется в `[[assets]]` сцены, см. [[Engine/EngineAudio]].

## Источники

`engine/rve-sdk/Cargo.toml`, `engine/rve-sdk/src/abi.rs`, `engine/rve-sdk/src/lib.rs`.
