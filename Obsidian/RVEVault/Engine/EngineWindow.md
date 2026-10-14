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
| `WindowConfig` | Публичные поля `title: String`, `width: u32`, `height: u32`; по умолчанию `RVE Sandbox`, 1280×720 |
| `WindowHandler::init(&mut self, window: Arc<Window>) -> bool` | Вызывается при создании окна; `false` завершает цикл |
| `WindowHandler::frame(&mut self, window: &Window) -> bool` | Вызывается на `RedrawRequested`; `false` завершает цикл |
| `WindowHandler::resized(&mut self, width: u32, height: u32)` | Размеры клиентской области в физических пикселях; по умолчанию ничего не делает |
| `WindowHandler::update(&mut self, window: &Window) -> bool` | Вызывается перед запросом перерисовки на каждом проходе; по умолчанию возвращает `true` |
| `WindowHandler::exiting(&mut self)` | Вызывается при завершении до освобождения окна; по умолчанию ничего не делает |
| `run(config, handler: &mut impl WindowHandler) -> Result<(), WindowError>` | Запускает event loop и оставляет обработчик у вызывающего кода |
| `WindowError` | `EventLoop(EventLoopError)` или `CreateWindow(OsError)`; реализует `Display` и `Error` |

Крейт также реэкспортирует `winit::window::Window` и `winit::dpi::PhysicalSize`.

## Зависимости и взаимодействия

- Использует winit 0.30.13 для event loop, окна и событий.
- Используется в [[Engine/Sandbox]]; renderer создаётся приложением отдельно.

## Ограничения и важные детали

- Event loop использует `ControlFlow::Poll`; `update` вызывается из `about_to_wait`, затем запрашивается redraw.
- Окно создаётся в `resumed` один раз. Ошибка создания окна сохраняется и возвращается из `run`, без `expect`/panic на этой ошибке.
- `CloseRequested`, а также `false` из `init`, `update` или `frame` завершают цикл.
- `resized` получает физический размер из события winit.
- Sandbox освобождает renderer из `exiting`; затем окно освобождается, а сохранённую ошибку можно проверить после `run`.
