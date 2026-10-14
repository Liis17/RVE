//! Создание окон, обработка событий окна и ввод пользователя.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::error::{EventLoopError, OsError};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::WindowId;

pub use winit::dpi::PhysicalSize;
pub use winit::window::Window;

/// Параметры создаваемого окна.
pub struct WindowConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "RVE Sandbox".to_owned(),
            width: 1280,
            height: 720,
        }
    }
}

/// Ошибка создания окна или работы цикла событий.
#[derive(Debug)]
pub enum WindowError {
    EventLoop(EventLoopError),
    CreateWindow(OsError),
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(error) => write!(f, "ошибка цикла событий: {error}"),
            Self::CreateWindow(error) => write!(f, "не удалось создать окно: {error}"),
        }
    }
}

impl Error for WindowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EventLoop(error) => Some(error),
            Self::CreateWindow(error) => Some(error),
        }
    }
}

/// Получает события окна и выполняет кадры приложения.
pub trait WindowHandler {
    /// Вызывается один раз после создания окна.
    fn init(&mut self, window: Arc<Window>) -> bool;

    /// Вызывается при запросе перерисовки.
    fn frame(&mut self, window: &Window) -> bool;

    /// Вызывается при изменении размера в физических пикселях.
    fn resized(&mut self, _width: u32, _height: u32) {}

    /// Вызывается при каждом проходе цикла событий перед запросом кадра.
    fn update(&mut self, _window: &Window) -> bool {
        true
    }

    /// Вызывается перед уничтожением окна.
    fn exiting(&mut self) {}
}

/// Запускает цикл окна до закрытия или запроса выхода от обработчика.
pub fn run(config: WindowConfig, handler: &mut impl WindowHandler) -> Result<(), WindowError> {
    let event_loop = EventLoop::new().map_err(WindowError::EventLoop)?;
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = WindowApplication {
        config,
        handler,
        window: None,
        error: None,
        exited: false,
    };
    let run_result = event_loop.run_app(&mut app);
    app.finish();

    if let Some(error) = app.error.take() {
        return Err(error);
    }
    run_result.map_err(WindowError::EventLoop)
}

struct WindowApplication<'a, H> {
    config: WindowConfig,
    handler: &'a mut H,
    window: Option<Arc<Window>>,
    error: Option<WindowError>,
    exited: bool,
}

impl<H: WindowHandler> WindowApplication<'_, H> {
    fn finish(&mut self) {
        if !self.exited {
            self.handler.exiting();
            self.exited = true;
        }
        self.window.take();
    }

    fn stop(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.exit();
    }
}

impl<H: WindowHandler> ApplicationHandler for WindowApplication<'_, H> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.error.is_some() {
            return;
        }

        let attributes = Window::default_attributes()
            .with_title(self.config.title.clone())
            .with_inner_size(PhysicalSize::new(self.config.width, self.config.height));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.error = Some(WindowError::CreateWindow(error));
                self.stop(event_loop);
                return;
            }
        };

        self.window = Some(Arc::clone(&window));
        if !self.handler.init(window) {
            self.stop(event_loop);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.id() != window_id)
        {
            return;
        }

        match event {
            WindowEvent::CloseRequested => self.stop(event_loop),
            WindowEvent::Resized(size) => self.handler.resized(size.width, size.height),
            WindowEvent::RedrawRequested => {
                if let Some(window) = self.window.as_ref()
                    && !self.handler.frame(window)
                {
                    self.stop(event_loop);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            if !self.handler.update(window) {
                self.stop(event_loop);
            } else {
                window.request_redraw();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.finish();
    }
}
