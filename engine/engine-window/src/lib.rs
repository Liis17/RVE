//! Создание окон, обработка событий окна и ввод пользователя.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::error::{EventLoopError, OsError};
use winit::event::{DeviceEvent, ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Icon, WindowId};

pub use winit::dpi::PhysicalSize;
pub use winit::window::Window;

/// Поддерживаемый набор физических клавиш.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Key {
    A = 0,
    B = 1,
    C = 2,
    D = 3,
    E = 4,
    F = 5,
    G = 6,
    H = 7,
    I = 8,
    J = 9,
    K = 10,
    L = 11,
    M = 12,
    N = 13,
    O = 14,
    P = 15,
    Q = 16,
    R = 17,
    S = 18,
    T = 19,
    U = 20,
    V = 21,
    W = 22,
    X = 23,
    Y = 24,
    Z = 25,
    Digit0 = 26,
    Digit1 = 27,
    Digit2 = 28,
    Digit3 = 29,
    Digit4 = 30,
    Digit5 = 31,
    Digit6 = 32,
    Digit7 = 33,
    Digit8 = 34,
    Digit9 = 35,
    F1 = 36,
    F2 = 37,
    F3 = 38,
    F4 = 39,
    F5 = 40,
    F6 = 41,
    F7 = 42,
    F8 = 43,
    F9 = 44,
    F10 = 45,
    F11 = 46,
    F12 = 47,
    ArrowUp = 48,
    ArrowDown = 49,
    ArrowLeft = 50,
    ArrowRight = 51,
    Space = 52,
    Enter = 53,
    Escape = 54,
    Tab = 55,
    Backspace = 56,
    ShiftLeft = 57,
    ShiftRight = 58,
    ControlLeft = 59,
    ControlRight = 60,
    AltLeft = 61,
    AltRight = 62,
    SuperLeft = 63,
    SuperRight = 64,
}

/// Поддерживаемые кнопки мыши.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

const PIXELS_PER_SCROLL_LINE: f64 = 120.0;

fn map_key_code(code: KeyCode) -> Option<Key> {
    Some(match code {
        KeyCode::KeyA => Key::A,
        KeyCode::KeyB => Key::B,
        KeyCode::KeyC => Key::C,
        KeyCode::KeyD => Key::D,
        KeyCode::KeyE => Key::E,
        KeyCode::KeyF => Key::F,
        KeyCode::KeyG => Key::G,
        KeyCode::KeyH => Key::H,
        KeyCode::KeyI => Key::I,
        KeyCode::KeyJ => Key::J,
        KeyCode::KeyK => Key::K,
        KeyCode::KeyL => Key::L,
        KeyCode::KeyM => Key::M,
        KeyCode::KeyN => Key::N,
        KeyCode::KeyO => Key::O,
        KeyCode::KeyP => Key::P,
        KeyCode::KeyQ => Key::Q,
        KeyCode::KeyR => Key::R,
        KeyCode::KeyS => Key::S,
        KeyCode::KeyT => Key::T,
        KeyCode::KeyU => Key::U,
        KeyCode::KeyV => Key::V,
        KeyCode::KeyW => Key::W,
        KeyCode::KeyX => Key::X,
        KeyCode::KeyY => Key::Y,
        KeyCode::KeyZ => Key::Z,
        KeyCode::Digit0 => Key::Digit0,
        KeyCode::Digit1 => Key::Digit1,
        KeyCode::Digit2 => Key::Digit2,
        KeyCode::Digit3 => Key::Digit3,
        KeyCode::Digit4 => Key::Digit4,
        KeyCode::Digit5 => Key::Digit5,
        KeyCode::Digit6 => Key::Digit6,
        KeyCode::Digit7 => Key::Digit7,
        KeyCode::Digit8 => Key::Digit8,
        KeyCode::Digit9 => Key::Digit9,
        KeyCode::F1 => Key::F1,
        KeyCode::F2 => Key::F2,
        KeyCode::F3 => Key::F3,
        KeyCode::F4 => Key::F4,
        KeyCode::F5 => Key::F5,
        KeyCode::F6 => Key::F6,
        KeyCode::F7 => Key::F7,
        KeyCode::F8 => Key::F8,
        KeyCode::F9 => Key::F9,
        KeyCode::F10 => Key::F10,
        KeyCode::F11 => Key::F11,
        KeyCode::F12 => Key::F12,
        KeyCode::ArrowUp => Key::ArrowUp,
        KeyCode::ArrowDown => Key::ArrowDown,
        KeyCode::ArrowLeft => Key::ArrowLeft,
        KeyCode::ArrowRight => Key::ArrowRight,
        KeyCode::Space => Key::Space,
        KeyCode::Enter => Key::Enter,
        KeyCode::Escape => Key::Escape,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::ShiftLeft => Key::ShiftLeft,
        KeyCode::ShiftRight => Key::ShiftRight,
        KeyCode::ControlLeft => Key::ControlLeft,
        KeyCode::ControlRight => Key::ControlRight,
        KeyCode::AltLeft => Key::AltLeft,
        KeyCode::AltRight => Key::AltRight,
        KeyCode::SuperLeft => Key::SuperLeft,
        KeyCode::SuperRight => Key::SuperRight,
        _ => return None,
    })
}

fn map_mouse_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    match button {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        winit::event::MouseButton::Back => Some(MouseButton::Back),
        winit::event::MouseButton::Forward => Some(MouseButton::Forward),
        winit::event::MouseButton::Other(_) => None,
    }
}

fn map_mouse_scroll(delta: MouseScrollDelta) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (x, y),
        MouseScrollDelta::PixelDelta(position) => (
            (position.x / PIXELS_PER_SCROLL_LINE) as f32,
            (position.y / PIXELS_PER_SCROLL_LINE) as f32,
        ),
    }
}

/// Параметры создаваемого окна.
pub struct WindowConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Необязательная иконка RGBA8.
    pub icon: Option<WindowIcon>,
    /// Режим перерисовки окна.
    pub redraw: RedrawMode,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "RVE Sandbox".to_owned(),
            width: 1280,
            height: 720,
            icon: None,
            redraw: RedrawMode::Continuous,
        }
    }
}

/// RGBA8-иконка окна, строки идут сверху вниз.
///
/// На macOS winit игнорирует иконку окна.
pub struct WindowIcon {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl WindowIcon {
    fn has_valid_buffer(&self) -> bool {
        self.width > 0
            && self.height > 0
            && Self::expected_buffer_len(self.width, self.height) == Some(self.rgba.len())
    }

    fn expected_buffer_len(width: u32, height: u32) -> Option<usize> {
        usize::try_from(width)
            .ok()?
            .checked_mul(usize::try_from(height).ok()?)?
            .checked_mul(4)
    }

    fn into_icon(self) -> Result<Icon, WindowError> {
        if !self.has_valid_buffer() {
            return Err(WindowError::InvalidIcon);
        }

        Icon::from_rgba(self.rgba, self.width, self.height).map_err(|_| WindowError::InvalidIcon)
    }
}

/// Определяет, когда цикл событий запрашивает перерисовку окна.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RedrawMode {
    /// Непрерывный опрос и запрос кадра после каждого прохода цикла событий.
    #[default]
    Continuous,
    /// Ожидание событий без автоматического запроса кадра.
    OnDemand,
}

impl RedrawMode {
    fn control_flow(self) -> ControlFlow {
        match self {
            Self::Continuous => ControlFlow::Poll,
            Self::OnDemand => ControlFlow::Wait,
        }
    }
}

enum InternalEvent {
    RequestFocus,
    RequestExit,
}

/// Потокобезопасный handle для запросов в оконный цикл событий.
#[derive(Clone)]
pub struct WindowProxy {
    event_loop_proxy: EventLoopProxy<InternalEvent>,
}

impl WindowProxy {
    /// Восстанавливает и фокусирует окно в потоке цикла событий.
    ///
    /// Если окно ещё не создано, запрос применяется в `resumed`. После завершения
    /// цикла событий запрос игнорируется.
    pub fn request_focus(&self) {
        let _ = self
            .event_loop_proxy
            .send_event(InternalEvent::RequestFocus);
    }

    /// Запрашивает завершение цикла событий в его главном потоке.
    ///
    /// После завершения цикла событий запрос игнорируется.
    pub fn request_exit(&self) {
        let _ = self.event_loop_proxy.send_event(InternalEvent::RequestExit);
    }
}

/// Ошибка создания окна или работы цикла событий.
#[derive(Debug)]
pub enum WindowError {
    EventLoop(EventLoopError),
    CreateWindow(OsError),
    InvalidIcon,
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(error) => write!(f, "ошибка цикла событий: {error}"),
            Self::CreateWindow(error) => write!(f, "не удалось создать окно: {error}"),
            Self::InvalidIcon => write!(f, "неверный RGBA буфер иконки окна"),
        }
    }
}

impl Error for WindowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EventLoop(error) => Some(error),
            Self::CreateWindow(error) => Some(error),
            Self::InvalidIcon => None,
        }
    }
}

/// Получает события окна и выполняет кадры приложения.
pub trait WindowHandler {
    /// Получает proxy после создания цикла событий и до `init`.
    fn attach_proxy(&mut self, _proxy: WindowProxy) {}

    /// Вызывается один раз после создания окна.
    fn init(&mut self, window: Arc<Window>) -> bool;

    /// Вызывается при запросе перерисовки.
    fn frame(&mut self, window: &Window) -> bool;

    /// Вызывается при изменении размера в физических пикселях.
    fn resized(&mut self, _width: u32, _height: u32) {}

    /// Вызывается из `about_to_wait`; `false` завершает цикл событий.
    fn update(&mut self, _window: &Window) -> bool {
        true
    }

    /// Вызывается перед уничтожением окна.
    fn exiting(&mut self) {}

    /// Вызывается при запросе закрытия окна; `true` завершает цикл, `false` оставляет его работающим.
    fn close_requested(&mut self) -> bool {
        true
    }

    /// Вызывается при изменении видимости окна из-за перекрытия.
    fn occluded(&mut self, _occluded: bool) {}

    /// Вызывается для поддерживаемой физической клавиши; автоповтор не передаётся.
    fn key(&mut self, _key: Key, _pressed: bool) {}

    /// Вызывается при нажатии или отпускании поддерживаемой кнопки мыши.
    fn mouse_button(&mut self, _button: MouseButton, _pressed: bool) {}

    /// Вызывается при движении курсора в физических пикселях относительно окна.
    fn cursor_moved(&mut self, _x: f64, _y: f64) {}

    /// Вызывается при raw-движении мыши в фокусированном окне.
    fn mouse_motion(&mut self, _dx: f64, _dy: f64) {}

    /// Вызывается при прокрутке; значения выражены в строках.
    fn mouse_wheel(&mut self, _dx: f32, _dy: f32) {}

    /// Вызывается при получении или потере фокуса окном.
    fn focused(&mut self, _focused: bool) {}
}

/// Запускает цикл окна до закрытия или запроса выхода от обработчика.
pub fn run(mut config: WindowConfig, handler: &mut impl WindowHandler) -> Result<(), WindowError> {
    let icon = config.icon.take().map(WindowIcon::into_icon).transpose()?;
    let event_loop = EventLoop::<InternalEvent>::with_user_event()
        .build()
        .map_err(WindowError::EventLoop)?;
    event_loop.set_control_flow(config.redraw.control_flow());
    handler.attach_proxy(WindowProxy {
        event_loop_proxy: event_loop.create_proxy(),
    });

    let mut app = WindowApplication {
        config,
        handler,
        icon,
        window: None,
        error: None,
        exited: false,
        focus_requests: FocusRequestState::default(),
        focused: false,
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
    icon: Option<Icon>,
    window: Option<Arc<Window>>,
    error: Option<WindowError>,
    exited: bool,
    focus_requests: FocusRequestState,
    focused: bool,
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

    fn should_exit_on_close_requested(&mut self) -> bool {
        self.handler.close_requested()
    }
}

#[derive(Default)]
struct FocusRequestState {
    window_created: bool,
    pending: bool,
}

impl FocusRequestState {
    fn request(&mut self) -> bool {
        if self.window_created {
            true
        } else {
            self.pending = true;
            false
        }
    }

    fn window_created(&mut self) -> bool {
        self.window_created = true;
        std::mem::take(&mut self.pending)
    }
}

impl<H: WindowHandler> ApplicationHandler<InternalEvent> for WindowApplication<'_, H> {
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

        if let Some(icon) = self.icon.take() {
            window.set_window_icon(Some(icon));
        }

        self.window = Some(Arc::clone(&window));
        self.focused = window.has_focus();
        self.handler.focused(self.focused);
        if self.focus_requests.window_created() {
            request_window_focus(&window);
        }
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
            WindowEvent::CloseRequested => {
                if self.should_exit_on_close_requested() {
                    self.stop(event_loop);
                }
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.handler.focused(focused);
            }
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(key) = map_key_code(code)
                {
                    self.handler.key(key, event.state == ElementState::Pressed);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = map_mouse_button(button) {
                    self.handler
                        .mouse_button(button, state == ElementState::Pressed);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.handler.cursor_moved(position.x, position.y);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = map_mouse_scroll(delta);
                self.handler.mouse_wheel(dx, dy);
            }
            WindowEvent::Resized(size) => self.handler.resized(size.width, size.height),
            WindowEvent::Occluded(occluded) => self.handler.occluded(occluded),
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

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
            && self.focused
        {
            self.handler.mouse_motion(dx, dy);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: InternalEvent) {
        match event {
            InternalEvent::RequestFocus => {
                if self.focus_requests.request()
                    && let Some(window) = self.window.as_ref()
                {
                    request_window_focus(window);
                }
            }
            InternalEvent::RequestExit => self.stop(event_loop),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            if !self.handler.update(window) {
                self.stop(event_loop);
            } else if self.config.redraw == RedrawMode::Continuous {
                window.request_redraw();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.finish();
    }
}

fn request_window_focus(window: &Window) {
    window.set_minimized(false);
    window.focus_window();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Handler {
        exit_calls: usize,
    }

    impl WindowHandler for Handler {
        fn init(&mut self, _window: Arc<Window>) -> bool {
            panic!("headless tests must not create a window");
        }

        fn frame(&mut self, _window: &Window) -> bool {
            panic!("headless tests must not render a frame");
        }

        fn exiting(&mut self) {
            self.exit_calls += 1;
        }
    }

    struct KeepOpenHandler;

    impl WindowHandler for KeepOpenHandler {
        fn init(&mut self, _window: Arc<Window>) -> bool {
            panic!("headless tests must not create a window");
        }

        fn frame(&mut self, _window: &Window) -> bool {
            panic!("headless tests must not render a frame");
        }

        fn close_requested(&mut self) -> bool {
            false
        }
    }

    #[test]
    fn default_window_has_sandbox_title_and_hd_size() {
        let config = WindowConfig::default();
        assert_eq!(config.title, "RVE Sandbox");
        assert_eq!((config.width, config.height), (1280, 720));
        assert!(config.icon.is_none());
        assert_eq!(config.redraw, RedrawMode::Continuous);
    }

    #[test]
    fn redraw_modes_select_the_expected_control_flow() {
        assert_eq!(RedrawMode::Continuous.control_flow(), ControlFlow::Poll);
        assert_eq!(RedrawMode::OnDemand.control_flow(), ControlFlow::Wait);
    }

    #[test]
    fn physical_key_mapping_covers_supported_keys_and_rejects_others() {
        assert_eq!(map_key_code(KeyCode::KeyA), Some(Key::A));
        assert_eq!(map_key_code(KeyCode::Digit7), Some(Key::Digit7));
        assert_eq!(map_key_code(KeyCode::F12), Some(Key::F12));
        assert_eq!(map_key_code(KeyCode::ArrowLeft), Some(Key::ArrowLeft));
        assert_eq!(map_key_code(KeyCode::AltRight), Some(Key::AltRight));
        assert_eq!(map_key_code(KeyCode::NumpadAdd), None);
    }

    #[test]
    fn mouse_button_mapping_rejects_unrecognized_buttons() {
        assert_eq!(
            map_mouse_button(winit::event::MouseButton::Left),
            Some(MouseButton::Left)
        );
        assert_eq!(
            map_mouse_button(winit::event::MouseButton::Back),
            Some(MouseButton::Back)
        );
        assert_eq!(map_mouse_button(winit::event::MouseButton::Other(9)), None);
    }

    #[test]
    fn mouse_scroll_delta_uses_lines_and_120_pixels_per_line() {
        assert_eq!(
            map_mouse_scroll(MouseScrollDelta::LineDelta(1.5, -2.0)),
            (1.5, -2.0)
        );
        assert_eq!(
            map_mouse_scroll(MouseScrollDelta::PixelDelta(
                winit::dpi::PhysicalPosition::new(240.0, -60.0,)
            )),
            (2.0, -0.5)
        );
    }

    #[test]
    fn window_icon_requires_exact_nonzero_rgba_buffer_size() {
        let icon = WindowIcon {
            rgba: vec![0; 24],
            width: 2,
            height: 3,
        };
        assert!(icon.has_valid_buffer());
        assert_eq!(WindowIcon::expected_buffer_len(2, 3), Some(24));

        let wrong_size = WindowIcon {
            rgba: vec![0; 23],
            width: 2,
            height: 3,
        };
        assert!(!wrong_size.has_valid_buffer());

        let zero_width = WindowIcon {
            rgba: vec![],
            width: 0,
            height: 1,
        };
        assert!(!zero_width.has_valid_buffer());
        assert_eq!(WindowIcon::expected_buffer_len(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn invalid_icon_returns_a_creation_error_without_an_event_loop() {
        let error = WindowIcon {
            rgba: vec![0; 3],
            width: 1,
            height: 1,
        }
        .into_icon()
        .unwrap_err();

        assert!(matches!(error, WindowError::InvalidIcon));
    }

    #[test]
    fn focus_request_is_remembered_until_window_creation() {
        let mut focus_requests = FocusRequestState::default();
        assert!(!focus_requests.request());
        assert!(focus_requests.window_created());
        assert!(!focus_requests.window_created());
        assert!(focus_requests.request());
    }

    #[test]
    fn close_request_uses_application_handler_decision() {
        let mut default_handler = Handler::default();
        let mut default_app = WindowApplication {
            config: WindowConfig::default(),
            handler: &mut default_handler,
            icon: None,
            window: None,
            error: None,
            exited: false,
            focus_requests: FocusRequestState::default(),
            focused: false,
        };
        assert!(default_app.should_exit_on_close_requested());

        let mut keep_open_handler = KeepOpenHandler;
        let mut keep_open_app = WindowApplication {
            config: WindowConfig::default(),
            handler: &mut keep_open_handler,
            icon: None,
            window: None,
            error: None,
            exited: false,
            focus_requests: FocusRequestState::default(),
            focused: false,
        };
        assert!(!keep_open_app.should_exit_on_close_requested());
    }

    #[test]
    fn window_proxy_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowProxy>();
    }

    #[test]
    fn finish_notifies_handler_once_even_without_a_window() {
        let mut handler = Handler::default();
        let mut app = WindowApplication {
            config: WindowConfig::default(),
            handler: &mut handler,
            icon: None,
            window: None,
            error: None,
            exited: false,
            focus_requests: FocusRequestState::default(),
            focused: false,
        };

        app.finish();
        assert_eq!(app.handler.exit_calls, 1);
        assert!(app.exited);
        app.finish();
        assert_eq!(app.handler.exit_calls, 1);
    }

    #[test]
    fn finish_preserves_failure_for_the_caller() {
        let mut handler = Handler::default();
        let mut app = WindowApplication {
            config: WindowConfig::default(),
            handler: &mut handler,
            icon: None,
            window: None,
            error: Some(WindowError::EventLoop(EventLoopError::RecreationAttempt)),
            exited: false,
            focus_requests: FocusRequestState::default(),
            focused: false,
        };

        app.finish();
        assert!(matches!(
            app.error,
            Some(WindowError::EventLoop(EventLoopError::RecreationAttempt))
        ));
        assert_eq!(app.handler.exit_calls, 1);
    }

    #[test]
    fn event_loop_error_retains_context_and_source() {
        let error = WindowError::EventLoop(EventLoopError::ExitFailure(7));
        assert!(error.to_string().contains("ошибка цикла событий"));
        assert!(matches!(
            error.source().unwrap().downcast_ref::<EventLoopError>(),
            Some(EventLoopError::ExitFailure(7))
        ));
    }
}
