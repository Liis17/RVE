# EngineWindow

Parent: [[Index]]

## Назначение

`engine-window` предоставляет оконный цикл на winit: создаёт одно окно, вызывает обработчик приложения и передаёт события закрытия, изменения размера и перерисовки.

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
| `WindowHandler::exiting(&mut self)` | Вызывается при завершении до освобождения окна; по умолчанию ничего не делает |
| `run(config, handler: &mut impl WindowHandler) -> Result<(), WindowError>` | Запускает event loop и оставляет обработчик у вызывающего кода |
| `WindowError` | Ошибка event loop, создания окна или RGBA-иконки; реализует `Display` и `Error` |

Крейт также реэкспортирует `winit::window::Window` и `winit::dpi::PhysicalSize`.

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
- Фокус и выход из фоновых потоков передаются в главный поток через внутренние user events; ранний запрос фокуса применяется после создания окна.
- Sandbox освобождает renderer из `exiting`; затем окно освобождается, а сохранённую ошибку можно проверить после `run`.
