//! Окно и системные диалоги для скриптов и горячих клавиш игрока.
//!
//! Поток скриптов управляет полноэкранным режимом через [`WindowLink`], отправляет скриншоты в
//! render thread и ставит диалоги в очередь главного потока. Главный поток открывает диалоги
//! асинхронно ([`DialogRunner`]): окно продолжает рисоваться, а результат возвращается в поток
//! скриптов через канал. Один диалог открыт одновременно, остальные ждут в очереди.

use engine_project::WindowSettings;
use engine_render::CapturedImage;
use engine_script::{FileDialogKind, FileDialogRequest, PlatformRequest};
use engine_window::{Key, Window, WindowProxy};
use flume::{Receiver, Sender};
use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake};
use std::time::{SystemTime, UNIX_EPOCH};

/// Результат диалога для потока скриптов: идентификатор и выбранные пути (пусто — отмена).
pub(crate) type DialogResult = (u64, Vec<PathBuf>);

/// Окно игрока для потока скриптов; в тестах подставляется запись.
pub(crate) trait WindowSink: Send + Sync {
    fn set_fullscreen(&self, fullscreen: bool);
    fn toggle_fullscreen(&self);
    fn is_fullscreen(&self) -> bool;
    fn wake(&self);
}

impl WindowSink for WindowProxy {
    fn set_fullscreen(&self, fullscreen: bool) {
        WindowProxy::set_fullscreen(self, fullscreen);
    }

    fn toggle_fullscreen(&self) {
        WindowProxy::toggle_fullscreen(self);
    }

    fn is_fullscreen(&self) -> bool {
        WindowProxy::is_fullscreen(self)
    }

    fn wake(&self) {
        WindowProxy::wake(self);
    }
}

#[derive(Default)]
struct LinkState {
    sink: Option<Arc<dyn WindowSink>>,
    /// Запрос полноэкранного режима до подключения окна.
    pending_fullscreen: Option<bool>,
    dialogs: VecDeque<(u64, FileDialogRequest)>,
}

/// Общая связь потока скриптов с окном. Запросы до подключения окна сохраняются.
#[derive(Clone, Default)]
pub(crate) struct WindowLink {
    state: Arc<Mutex<LinkState>>,
}

impl WindowLink {
    pub(crate) fn attach(&self, sink: Arc<dyn WindowSink>) {
        let mut state = self.lock();
        if let Some(fullscreen) = state.pending_fullscreen.take() {
            sink.set_fullscreen(fullscreen);
        }
        if !state.dialogs.is_empty() {
            sink.wake();
        }
        state.sink = Some(sink);
    }

    pub(crate) fn set_fullscreen(&self, fullscreen: bool) {
        let mut state = self.lock();
        match &state.sink {
            Some(sink) => sink.set_fullscreen(fullscreen),
            None => state.pending_fullscreen = Some(fullscreen),
        }
    }

    /// Переключает режим относительно состояния окна в момент применения.
    pub(crate) fn toggle_fullscreen(&self) {
        let mut state = self.lock();
        match &state.sink {
            Some(sink) => sink.toggle_fullscreen(),
            None => state.pending_fullscreen = Some(!state.pending_fullscreen.unwrap_or(false)),
        }
    }

    pub(crate) fn is_fullscreen(&self) -> bool {
        let state = self.lock();
        state.sink.as_ref().map_or_else(
            || state.pending_fullscreen.unwrap_or(false),
            |sink| sink.is_fullscreen(),
        )
    }

    fn push_dialog(&self, id: u64, request: FileDialogRequest) {
        let mut state = self.lock();
        state.dialogs.push_back((id, request));
        if let Some(sink) = &state.sink {
            sink.wake();
        }
    }

    fn pop_dialog(&self) -> Option<(u64, FileDialogRequest)> {
        self.lock().dialogs.pop_front()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LinkState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Встроенная горячая клавиша окна игрока.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Hotkey {
    ToggleFullscreen,
    Screenshot,
}

/// Действие нажатой клавиши с учётом `[window]` проекта.
pub(crate) fn hotkey(settings: &WindowSettings, key: Key, pressed: bool) -> Option<Hotkey> {
    match key {
        Key::F11 if pressed && settings.fullscreen_hotkey => Some(Hotkey::ToggleFullscreen),
        Key::F12 if pressed && settings.screenshot_hotkey => Some(Hotkey::Screenshot),
        _ => None,
    }
}

/// Окно, скриншоты и диалоги главного потока игрока.
pub(crate) struct PlayerPlatform {
    hotkeys: WindowSettings,
    link: WindowLink,
    dialogs: DialogRunner,
    screenshots: Sender<PathBuf>,
    /// Очередь скриншотов для backend; забирается при запуске render thread.
    screenshots_rx: Option<Receiver<PathBuf>>,
    screenshot_dir: Option<PathBuf>,
}

impl PlayerPlatform {
    /// Создаёт связанные части главного потока и потока скриптов.
    pub(crate) fn new(
        hotkeys: WindowSettings,
        screenshot_dir: Option<PathBuf>,
    ) -> (Self, ScriptPlatform) {
        let link = WindowLink::default();
        let (screenshots, screenshots_rx) = flume::unbounded();
        let (dialog_results_tx, dialog_results) = flume::unbounded();
        let script = ScriptPlatform {
            link: link.clone(),
            screenshots: Some(screenshots.clone()),
            screenshot_dir: screenshot_dir.clone(),
            dialog_results,
            window: true,
        };
        let player = Self {
            hotkeys,
            dialogs: DialogRunner::new(link.clone(), dialog_results_tx),
            link,
            screenshots,
            screenshots_rx: Some(screenshots_rx),
            screenshot_dir,
        };
        (player, script)
    }

    pub(crate) fn attach(&self, proxy: &WindowProxy) {
        self.link.attach(Arc::new(proxy.clone()));
    }

    pub(crate) fn take_screenshot_queue(&mut self) -> Option<Receiver<PathBuf>> {
        self.screenshots_rx.take()
    }

    /// Выполняет горячую клавишу; клавиша при этом остаётся доступной скриптам.
    pub(crate) fn key(&self, key: Key, pressed: bool) {
        match hotkey(&self.hotkeys, key, pressed) {
            Some(Hotkey::ToggleFullscreen) => self.link.toggle_fullscreen(),
            Some(Hotkey::Screenshot) => {
                match default_screenshot_path(self.screenshot_dir.as_deref()) {
                    Some(path) => {
                        let _ = self.screenshots.send(path);
                    }
                    None => log::warn!(
                        "папка скриншотов недоступна: не найден каталог данных пользователя"
                    ),
                }
            }
            None => {}
        }
    }

    pub(crate) fn update(&self, window: &Window, proxy: Option<&WindowProxy>) {
        self.dialogs.pump(window, proxy);
    }
}

/// Платформенные возможности потока скриптов.
pub(crate) struct ScriptPlatform {
    pub(crate) link: WindowLink,
    /// Пути скриншотов для render thread; без окна запросы отклоняются.
    pub(crate) screenshots: Option<Sender<PathBuf>>,
    /// Папка скриншотов проекта по умолчанию.
    pub(crate) screenshot_dir: Option<PathBuf>,
    pub(crate) dialog_results: Receiver<DialogResult>,
    /// Есть ли окно; без него диалоги сразу считаются отменёнными.
    pub(crate) window: bool,
}

impl Default for ScriptPlatform {
    fn default() -> Self {
        Self {
            link: WindowLink::default(),
            screenshots: None,
            screenshot_dir: None,
            dialog_results: flume::unbounded().1,
            window: false,
        }
    }
}

impl ScriptPlatform {
    /// Передаёт скриптам состояние окна и готовые результаты диалогов перед `update`.
    pub(crate) fn before_update(&self, scripts: &mut engine_script::ScriptHost) {
        scripts.set_window_fullscreen(self.link.is_fullscreen());
        for (id, paths) in self.dialog_results.try_iter() {
            scripts.complete_file_dialog(id, paths);
        }
    }

    /// Выполняет запросы, принятые скриптами за кадр.
    pub(crate) fn after_update(&self, scripts: &mut engine_script::ScriptHost) {
        for request in scripts.take_platform_requests() {
            match request {
                PlatformRequest::SetFullscreen(fullscreen) if self.window => {
                    self.link.set_fullscreen(fullscreen);
                }
                PlatformRequest::Screenshot(path) => self.screenshot(path),
                PlatformRequest::FileDialog { id, request } if self.window => {
                    self.link.push_dialog(id, request);
                }
                PlatformRequest::FileDialog { id, .. } => {
                    log::warn!("системные диалоги недоступны без окна; диалог {id} отменён");
                    scripts.complete_file_dialog(id, Vec::new());
                }
                PlatformRequest::SetFullscreen(_) => {
                    log::warn!("полноэкранный режим недоступен без окна");
                }
            }
        }
    }

    fn screenshot(&self, path: Option<PathBuf>) {
        let Some(sender) = &self.screenshots else {
            log::warn!("скриншот недоступен без окна");
            return;
        };
        match path.or_else(|| default_screenshot_path(self.screenshot_dir.as_deref())) {
            Some(path) => {
                let _ = sender.send(path);
            }
            None => {
                log::warn!("папка скриншотов недоступна: не найден каталог данных пользователя")
            }
        }
    }
}

/// Путь нового скриншота в папке по умолчанию или `None`, если папка неизвестна.
pub(crate) fn default_screenshot_path(directory: Option<&Path>) -> Option<PathBuf> {
    directory.map(|directory| directory.join(screenshot_file_name(SystemTime::now())))
}

/// Имя файла скриншота по времени UTC с миллисекундами: `screenshot_2026-11-10_14-03-22-123.png`.
pub(crate) fn screenshot_file_name(time: SystemTime) -> String {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let second_of_day = seconds % 86_400;
    format!(
        "screenshot_{year:04}-{month:02}-{day:02}_{:02}-{:02}-{:02}-{:03}.png",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
        since_epoch.subsec_millis()
    )
}

/// Дата григорианского календаря по числу дней от 1970-01-01 (алгоритм Howard Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Кодирует RGBA8-изображение в PNG и записывает его, создавая недостающие папки.
pub(crate) fn write_png(path: &Path, image: &CapturedImage) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file = fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_image_data(&image.rgba8)
        .map_err(|error| error.to_string())?;
    writer.finish().map_err(|error| error.to_string())
}

/// Записывает скриншот в отдельном потоке, чтобы кодирование не задерживало кадры.
pub(crate) fn save_screenshot(path: PathBuf, image: CapturedImage) {
    let spawned = std::thread::Builder::new()
        .name("rve-screenshot".to_owned())
        .spawn(move || match write_png(&path, &image) {
            Ok(()) => log::info!("Скриншот сохранён: {}", path.display()),
            Err(error) => log::error!("Не удалось сохранить скриншот {}: {error}", path.display()),
        });
    if let Err(error) = spawned {
        log::error!("не удалось запустить запись скриншота: {error}");
    }
}

/// Открывает диалоги из очереди [`WindowLink`] на главном потоке по одному.
pub(crate) struct DialogRunner {
    link: WindowLink,
    results: Sender<DialogResult>,
    busy: Arc<AtomicBool>,
}

impl DialogRunner {
    pub(crate) fn new(link: WindowLink, results: Sender<DialogResult>) -> Self {
        Self {
            link,
            results,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Открывает следующий диалог, если предыдущий закрыт. Вызывается из `update` окна.
    pub(crate) fn pump(&self, window: &Window, proxy: Option<&WindowProxy>) {
        if self.busy.load(Ordering::Acquire) {
            return;
        }
        let Some((id, request)) = self.link.pop_dialog() else {
            return;
        };
        self.busy.store(true, Ordering::Release);
        // Будущее создаётся на главном потоке: на macOS rfd здесь же открывает sheet окна.
        let dialog = build_dialog(&request, window);
        let future: std::pin::Pin<Box<dyn Future<Output = Vec<PathBuf>> + Send>> =
            match request.kind {
                FileDialogKind::OpenFile => Box::pin(single(dialog.pick_file())),
                FileDialogKind::OpenFiles => Box::pin(many(dialog.pick_files())),
                FileDialogKind::SaveFile => Box::pin(single(dialog.save_file())),
                FileDialogKind::PickFolder => Box::pin(single(dialog.pick_folder())),
            };
        let results = self.results.clone();
        let busy = Arc::clone(&self.busy);
        let wake = proxy.cloned();
        let spawned = std::thread::Builder::new()
            .name("rve-file-dialog".to_owned())
            .spawn(move || {
                let paths = block_on(future);
                let _ = results.send((id, paths));
                busy.store(false, Ordering::Release);
                if let Some(proxy) = wake {
                    proxy.wake();
                }
            });
        if let Err(error) = spawned {
            log::error!("не удалось открыть системный диалог: {error}");
            let _ = self.results.send((id, Vec::new()));
            self.busy.store(false, Ordering::Release);
        }
    }
}

fn build_dialog(request: &FileDialogRequest, window: &Window) -> rfd::AsyncFileDialog {
    let mut dialog = rfd::AsyncFileDialog::new().set_parent(window);
    if let Some(title) = &request.title {
        dialog = dialog.set_title(title);
    }
    if let Some(directory) = &request.directory {
        dialog = dialog.set_directory(directory);
    }
    if let Some(file_name) = &request.file_name {
        dialog = dialog.set_file_name(file_name);
    }
    for filter in &request.filters {
        dialog = dialog.add_filter(&filter.name, &filter.extensions);
    }
    dialog
}

async fn single(future: impl Future<Output = Option<rfd::FileHandle>>) -> Vec<PathBuf> {
    future
        .await
        .map(|handle| vec![handle.path().to_owned()])
        .unwrap_or_default()
}

async fn many(future: impl Future<Output = Option<Vec<rfd::FileHandle>>>) -> Vec<PathBuf> {
    future
        .await
        .unwrap_or_default()
        .iter()
        .map(|handle| handle.path().to_owned())
        .collect()
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Минимальный исполнитель: поток диалога спит до пробуждения future.
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Arc::new(ThreadWaker(std::thread::current())).into();
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_script::FileDialogKind;
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingWindow {
        fullscreen: Mutex<Vec<bool>>,
        wakes: Mutex<usize>,
    }

    impl WindowSink for RecordingWindow {
        fn set_fullscreen(&self, fullscreen: bool) {
            self.fullscreen.lock().unwrap().push(fullscreen);
        }

        fn toggle_fullscreen(&self) {
            let next = !self.is_fullscreen();
            self.set_fullscreen(next);
        }

        fn is_fullscreen(&self) -> bool {
            self.fullscreen
                .lock()
                .unwrap()
                .last()
                .copied()
                .unwrap_or(false)
        }

        fn wake(&self) {
            *self.wakes.lock().unwrap() += 1;
        }
    }

    fn dialog() -> FileDialogRequest {
        FileDialogRequest {
            kind: FileDialogKind::OpenFile,
            title: None,
            directory: None,
            file_name: None,
            filters: Vec::new(),
        }
    }

    #[test]
    fn requests_before_the_window_are_applied_when_it_attaches() {
        let link = WindowLink::default();
        link.set_fullscreen(true);
        assert!(link.is_fullscreen());
        link.push_dialog(4, dialog());

        let window = Arc::new(RecordingWindow::default());
        link.attach(window.clone());
        assert_eq!(*window.fullscreen.lock().unwrap(), [true]);
        assert_eq!(*window.wakes.lock().unwrap(), 1);
        link.set_fullscreen(false);
        assert!(!link.is_fullscreen());
        assert_eq!(link.pop_dialog().map(|(id, _)| id), Some(4));
        assert!(link.pop_dialog().is_none());
    }

    #[test]
    fn hotkeys_follow_project_settings_and_fire_on_press_only() {
        let defaults = WindowSettings::default();
        assert_eq!(
            hotkey(&defaults, Key::F11, true),
            Some(Hotkey::ToggleFullscreen)
        );
        assert_eq!(hotkey(&defaults, Key::F12, true), Some(Hotkey::Screenshot));
        assert_eq!(hotkey(&defaults, Key::F12, false), None);
        assert_eq!(hotkey(&defaults, Key::F10, true), None);
        let disabled = WindowSettings {
            fullscreen_hotkey: false,
            screenshot_hotkey: false,
        };
        assert_eq!(hotkey(&disabled, Key::F11, true), None);
        assert_eq!(hotkey(&disabled, Key::F12, true), None);
    }

    #[test]
    fn toggling_before_the_window_flips_the_pending_request() {
        let link = WindowLink::default();
        link.toggle_fullscreen();
        assert!(link.is_fullscreen());
        link.toggle_fullscreen();
        assert!(!link.is_fullscreen());
        let window = Arc::new(RecordingWindow::default());
        link.attach(window.clone());
        link.toggle_fullscreen();
        assert_eq!(*window.fullscreen.lock().unwrap(), [false, true]);
    }

    #[test]
    fn screenshot_names_use_utc_date_time_and_milliseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_762_783_402_123);
        assert_eq!(
            screenshot_file_name(time),
            "screenshot_2025-11-10_14-03-22-123.png"
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(
            default_screenshot_path(Some(Path::new("shots")))
                .unwrap()
                .parent(),
            Some(Path::new("shots"))
        );
        assert_eq!(default_screenshot_path(None), None);
    }

    #[test]
    fn png_is_written_with_missing_folders_and_decodes_to_the_same_pixels() {
        let directory = std::env::temp_dir().join(format!("rve-shot-{}", std::process::id()));
        let path = directory.join("nested").join("shot.png");
        let image = CapturedImage {
            width: 2,
            height: 1,
            rgba8: vec![255, 0, 0, 255, 0, 0, 255, 255],
        };
        write_png(&path, &image).unwrap();

        let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&pixels[..info.buffer_size()], image.rgba8.as_slice());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn headless_platform_cancels_dialogs_and_ignores_window_requests() {
        let world = engine_scene::World::from_document(
            &engine_scene::SceneDocument::parse(
                "s.rvescene",
                "[[assets]]\npath = \"/d.lua\"\n[[objects]]\nid = \"d\"\n[[objects.lite_scripts]]\npath = \"/d.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
            )
            .unwrap(),
        )
        .unwrap();
        let mut world = world;
        let sources = std::collections::BTreeMap::from([(
            "d.lua".to_owned(),
            r#"
local Script = {}
local frame = 0
function Script:on_update()
  frame = frame + 1
  if frame == 1 then
    self.dialog = rve.dialog.open_file()
    rve.window.set_fullscreen(true)
    rve.window.screenshot()
  else
    local paths = rve.dialog.poll(self.dialog)
    rve.log("paths " .. tostring(paths and #paths))
  end
end
return Script
"#
            .to_owned(),
        )]);
        let mut scripts = engine_script::ScriptHost::load(&world, "s", &sources, None).unwrap();
        let platform = ScriptPlatform::default();
        for _ in 0..2 {
            platform.before_update(&mut scripts);
            scripts.update(&mut world, 0.1, engine_script::ScriptTime::default());
            platform.after_update(&mut scripts);
        }
        let messages = scripts
            .drain_events()
            .into_iter()
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert!(messages.contains(&"paths 0".to_owned()), "{messages:?}");
        assert!(!platform.link.is_fullscreen());
    }
}
