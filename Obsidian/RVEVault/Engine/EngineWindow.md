# EngineWindow

Parent: [[Index]]

## Назначение

`engine-window` предоставляет оконный цикл на winit: создаёт одно окно, вызывает обработчик приложения и передаёт события закрытия, изменения размера, перерисовки и пользовательского ввода.

## Источники

| Файл | Значимый символ | Роль |
|---|---|---|
| `engine/engine-window/src/lib.rs` | `WindowConfig`, `WindowError`, `WindowHandler`, `run` | Публичный API оконного цикла |
| `engine/engine-window/Cargo.toml` | `winit` | Зависимость оконного крейта |

## Публичные контракты

| Контракт | Поведение и условия |
|---|---|
| `WindowConfig` | Публичные поля заголовка, размера, необязательной RGBA8-иконки и режима перерисовки; значения по умолчанию сохраняют `RVE Sandbox`, 1280×720, без иконки и с `Continuous` |
| `WindowIcon` | RGBA8-буфер с размерами; длина должна точно равняться `width × height × 4`, строки идут сверху вниз; winit не поддерживает иконку окна на macOS |
| `RedrawMode` | `Continuous` использует `ControlFlow::Poll` и запрашивает кадр в каждом `about_to_wait`; `OnDemand` использует `ControlFlow::Wait` и не запрашивает кадр автоматически |
| `WindowProxy` | Cloneable потокобезопасный handle: `request_focus` восстанавливает и фокусирует окно, `request_exit` завершает цикл на главном потоке; запросы после завершения цикла игнорируются |
| `WindowHandler::attach_proxy(&mut self, proxy)` | Вызывается один раз после создания event loop и до `init`; реализация по умолчанию ничего не делает |
| `WindowHandler::init(&mut self, window: Arc<Window>) -> bool` | Вызывается при создании окна; `false` завершает цикл |
| `WindowHandler::frame(&mut self, window: &Window) -> bool` | Вызывается на `RedrawRequested`; `false` завершает цикл |
| `WindowHandler::resized(&mut self, width: u32, height: u32)` | Размеры клиентской области в физических пикселях; по умолчанию ничего не делает |
| `WindowHandler::update(&mut self, window: &Window) -> bool` | Вызывается из `about_to_wait`; `false` завершает цикл, по умолчанию возвращает `true` |
| `WindowHandler::close_requested(&mut self) -> bool` | `true` немедленно завершает цикл; `false` оставляет окно открытым до `WindowProxy::request_exit`; по умолчанию `true` |
| `WindowHandler::occluded(&mut self, occluded: bool)` | Получает состояние из `WindowEvent::Occluded`; по умолчанию ничего не делает. Событие доставляется не на всех платформах: на Windows свёрнутое окно видно как `resized(0, 0)`, поэтому потребитель учитывает оба сигнала |
| `WindowHandler::key(&mut self, key: Key, pressed: bool)` | Передаёт поддерживаемую физическую клавишу; повторные события `KeyEvent::repeat` не передаются |
| `WindowHandler::mouse_button(&mut self, button: MouseButton, pressed: bool)` | Передаёт нажим/отпускание поддерживаемой кнопки мыши |
| `WindowHandler::cursor_moved(&mut self, x: f64, y: f64)` | Координаты курсора относительно окна в физических пикселях |
| `WindowHandler::mouse_motion(&mut self, dx: f64, dy: f64)` | Относительное движение устройства из `DeviceEvent::MouseMotion`; вызывается, только когда окно в фокусе |
| `WindowHandler::mouse_wheel(&mut self, dx: f32, dy: f32)` | Накопитель получает величины прокрутки в строках |
| `WindowHandler::focused(&mut self, focused: bool)` | Начальное состояние после создания окна, до `init`, и дальнейшие изменения фокуса |
| `WindowHandler::exiting(&mut self)` | Вызывается при завершении до освобождения окна; по умолчанию ничего не делает |
| `run(config, handler: &mut impl WindowHandler) -> Result<(), WindowError>` | Запускает event loop и оставляет обработчик у вызывающего кода |
| `WindowError` | Ошибка event loop, создания окна или RGBA-иконки; реализует `Display` и `Error` |

Крейт также реэкспортирует `winit::window::Window` и `winit::dpi::PhysicalSize`.

Все добавленные методы ввода имеют пустые default-реализации, поэтому обработчики, которым ввод не нужен, сохраняют прежнее поведение.

## Клавиши и колесо

Публичные `Key` и `MouseButton` принадлежат `engine-window`; типы winit не входят в сигнатуры обработчика. `Key` — подмножество `KeyCode`: `A`–`Z`, `Digit0`–`Digit9`, `F1`–`F12`, стрелки, `Space`, `Enter`, `Escape`, `Tab`, `Backspace` и левые/правые `Shift`, `Control`, `Alt`, `Super`. `MouseButton` включает `Left`, `Right`, `Middle`, `Back` и `Forward`; остальные клавиши и кнопки игнорируются.

Клавиатура определяется по физическому `PhysicalKey::Code`. `LineDelta` передаётся без преобразования; `PixelDelta` делится на 120 физических пикселей за строку. Runtime маппит эти собственные типы на стабильные коды `rve-sdk`.

## Зависимости и взаимодействия

- Использует winit 0.30.13 для event loop, окна и событий.
- Оконный цикл общий для Windows и macOS; реализации ОС предоставляет winit. Vulkan loader и surface находятся в [[Engine/EnginePlatform]].
- Используется в [[Engine/Sandbox]]; renderer создаётся приложением отдельно.

## Ограничения и важные детали

- Режим перерисовки выбирает `ControlFlow`: `Continuous` использует `Poll` и запрашивает redraw в `about_to_wait`, `OnDemand` использует `Wait` без автоматического redraw.
- Окно создаётся в `resumed` один раз. Ошибка создания окна сохраняется и возвращается из `run`, без `expect`/panic на этой ошибке.
- Иконка проверяется и преобразуется до создания event loop, а устанавливается сразу после создания окна.
- `CloseRequested` завершает цикл, если `close_requested` вернул `true`; `false` позволяет приложению завершить его позже через `WindowProxy::request_exit`.
- `false` из `init`, `update` или `frame` завершает цикл.
- `resized` получает физический размер из события winit.
- `CursorMoved` использует физические пиксели относительно верхнего левого угла окна; `DeviceEvent::MouseMotion` передаёт относительное движение устройства. На Windows используется raw input; на macOS движение может включать системное ускорение — это не проверено.
- На macOS при зажатом `Cmd` система может не прислать key-up для других клавиш; тогда клавиша останется down до потери фокуса. Это известное ограничение платформы, но в проекте оно не проверено.
- Фокус и выход из фоновых потоков передаются в главный поток через внутренние user events; ранний запрос фокуса применяется после создания окна.
- Sandbox освобождает renderer из `exiting`; затем окно освобождается, а сохранённую ошибку можно проверить после `run`.
