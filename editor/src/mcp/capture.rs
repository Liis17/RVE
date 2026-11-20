//! Инструмент `capture_view`: PNG панели Scene или Game либо offscreen-вида выбранной камеры.
//!
//! Задача живёт в `EditorState` от `drain` до ответа, поэтому HTTP-ответ приходит только после
//! реального захвата. Задача проходит две фазы: `Rendering` — readback на главном потоке, где нужны
//! GPU-ресурсы; `Encoding` — PNG и base64 на потоке `rve-mcp-png` (`PngEncoder`), без GPU.
//! `CapturePlan` отделён от `ToolCall`: автомат и выбор режима тестируются без сокета и GPU, а
//! GPU-часть вызывает `EditorApp` через `mcp_capture_gpu`.

use super::read::{optional_string, optional_u64, required_string};
use crate::app::EditorApp;
use crate::console::LogLevel;
use crate::state::EditorState;
use engine_mcp::{Permission, ToolCall, ToolResult, ToolSpec};
use engine_render::{CaptureTicket, CapturedImage, RenderError};
use engine_scene::camera::Camera;
use engine_scene::{CameraSettings, PostProcessSettings, ScreenShaderRef, World};
use flume::{TryRecvError, TrySendError};
use serde_json::{Value, json};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Сколько ждать готовый захват; дольше — ошибка, а не зависание клиента.
pub(crate) const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);
const MAX_CAPTURE_SIDE: u32 = 4096;
const DEFAULT_CAPTURE_SIZE: [u32; 2] = [1280, 720];
const DEFAULT_WARMUP_FRAMES: u32 = 8;
const WARMUP_FRAMES: RangeInclusive<u64> = 1..=64;
const STALE_GENERATION: &str = "project or scene changed while request was queued";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureView {
    Scene,
    Game,
}

impl CaptureView {
    fn label(self) -> &'static str {
        match self {
            Self::Scene => "scene",
            Self::Game => "game",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CameraChoice {
    Editor,
    Game,
    Object(String),
}

impl CameraChoice {
    fn label(&self) -> String {
        match self {
            Self::Editor => "editor".to_owned(),
            Self::Game => "game".to_owned(),
            Self::Object(id) => format!("object:{id}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Offscreen {
    size: [u32; 2],
    warmup_frames: u32,
    warmup_left: u32,
    reset_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Viewport панели в том кадре, где она видна.
    Panel,
    Offscreen(Offscreen),
}

/// Состояние задачи без сетевого вызова.
pub(crate) struct CapturePlan {
    generation: u64,
    deadline: Instant,
    view: CaptureView,
    camera: CameraChoice,
    source: Source,
    ticket: Option<CaptureTicket>,
}

impl CapturePlan {
    pub(crate) fn is_offscreen(&self) -> bool {
        matches!(self.source, Source::Offscreen(_))
    }

    fn source_label(&self) -> &'static str {
        match self.source {
            Source::Panel => "panel",
            Source::Offscreen(_) => "offscreen",
        }
    }
}

/// Задача, ожидающая ответа: вызов HTTP, состояние захвата и фаза.
pub(crate) struct CaptureJob {
    call: ToolCall,
    pub(crate) plan: CapturePlan,
    pub(crate) phase: Phase,
}

impl CaptureJob {
    /// Задача ещё в фазе Rendering: участвует в отрисовке кадра и держит GPU-ресурсы.
    pub(crate) fn rendering(&self) -> bool {
        matches!(self.phase, Phase::Rendering)
    }
}

/// Фаза задачи. В `Encoding` GPU-ресурсы уже освобождены, остаются только пиксели и кодирование.
pub(crate) enum Phase {
    Rendering,
    Encoding(EncodeWait),
}

/// Кадр, переданный на кодирование: пиксели и мета ответа, собранные на главном потоке.
pub(crate) struct EncodeInput {
    image: CapturedImage,
    meta: Value,
}

/// Состояние ожидания кодирования: задание либо ждёт слота (`pending`), либо принято (`flight`).
pub(crate) struct EncodeWait {
    pending: Option<EncodeInput>,
    flight: Option<InFlight>,
}

impl EncodeWait {
    /// Отменяет принятое задание; ожидающее просто отбрасывается вместе с задачей.
    fn cancel(&self) {
        if let Some(flight) = &self.flight {
            flight.cancel();
        }
    }
}

/// Задание, принятое рабочим потоком: общий флаг отмены и канал результата ёмкостью 1.
pub(crate) struct InFlight {
    cancel: Arc<AtomicBool>,
    done: flume::Receiver<Result<ToolResult, String>>,
}

impl InFlight {
    /// Поток не начнёт задание, если оно ещё не стартовало, а готовый результат будет отброшен.
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// Результат `PngEncoder::submit`: задание принято или слот занят и задание возвращено.
pub(crate) enum Submit {
    Accepted(InFlight),
    Full(EncodeInput),
}

/// Функция кодирования на рабочем потоке; подменяется в тестах.
type EncodeFn = Arc<dyn Fn(CapturedImage, Value) -> Result<ToolResult, String> + Send + Sync>;

/// Задание для рабочего потока.
struct EncodeTask {
    image: CapturedImage,
    meta: Value,
    cancel: Arc<AtomicBool>,
    done: flume::Sender<Result<ToolResult, String>>,
}

const ENCODER_STOPPED: &str = "capture failed: PNG encoder stopped";

/// Рабочий поток `rve-mcp-png` с входом ёмкостью 1. Поток создаётся при первом задании и
/// завершается, когда закрыт вход; join не выполняется.
pub(crate) struct PngEncoder {
    encode: EncodeFn,
    tasks: Option<flume::Sender<EncodeTask>>,
}

impl PngEncoder {
    pub(crate) fn new() -> Self {
        Self::with_encode(Arc::new(encode_response))
    }

    fn with_encode(encode: EncodeFn) -> Self {
        Self {
            encode,
            tasks: None,
        }
    }

    /// Передаёт задание рабочему потоку. Занятый слот возвращает задание; закрытый поток
    /// сбрасывается, и следующее задание запустит новый.
    pub(crate) fn submit(&mut self, input: EncodeInput) -> Result<Submit, String> {
        let tasks = self.sender()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let (done, result) = flume::bounded(1);
        let task = EncodeTask {
            image: input.image,
            meta: input.meta,
            cancel: Arc::clone(&cancel),
            done,
        };
        match tasks.try_send(task) {
            Ok(()) => Ok(Submit::Accepted(InFlight {
                cancel,
                done: result,
            })),
            Err(TrySendError::Full(task)) => Ok(Submit::Full(EncodeInput {
                image: task.image,
                meta: task.meta,
            })),
            Err(TrySendError::Disconnected(_)) => {
                self.tasks = None;
                Err(ENCODER_STOPPED.to_owned())
            }
        }
    }

    /// Забирает результат, если он готов. Упавший поток сбрасывается, а вызов получает ошибку.
    pub(crate) fn fetch(&mut self, flight: &InFlight) -> Option<Result<ToolResult, String>> {
        match flight.done.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.tasks = None;
                Some(Err(ENCODER_STOPPED.to_owned()))
            }
        }
    }

    fn sender(&mut self) -> Result<flume::Sender<EncodeTask>, String> {
        if let Some(tasks) = &self.tasks {
            return Ok(tasks.clone());
        }
        let (tasks, inbox) = flume::bounded(1);
        let encode = Arc::clone(&self.encode);
        std::thread::Builder::new()
            .name("rve-mcp-png".to_owned())
            .spawn(move || run_worker(inbox, encode))
            .map_err(|error| format!("capture failed: cannot start PNG encoder: {error}"))?;
        self.tasks = Some(tasks.clone());
        Ok(tasks)
    }
}

/// Цикл рабочего потока. Отменённое до старта задание не кодируется, а результат отменённого
/// отбрасывается: его получатель уже ушёл.
fn run_worker(inbox: flume::Receiver<EncodeTask>, encode: EncodeFn) {
    while let Ok(task) = inbox.recv() {
        if task.cancel.load(Ordering::Acquire) {
            continue;
        }
        let result = encode(task.image, task.meta);
        if !task.cancel.load(Ordering::Acquire) {
            // Отправка в закрытый канал безвредна: получатель мог уйти по таймауту или отмене.
            let _ = task.done.try_send(result);
        }
    }
}

/// Параметры offscreen-кадра, которые render берёт из задачи до отрисовки.
pub(crate) struct OffscreenFrame {
    pub(crate) camera: CameraChoice,
    pub(crate) size: [u32; 2],
    pub(crate) reset_history: bool,
    /// Запросить readback в этом кадре: прогрев закончен и тикета ещё нет.
    pub(crate) request: bool,
}

/// Камера и настройки кадра, выбранные render для вида задачи.
pub(crate) struct CaptureCamera {
    pub(crate) camera: Camera,
    pub(crate) settings: CameraSettings,
    pub(crate) post: PostProcessSettings,
    /// Вид Scene: постобработка и DebugView панели Scene, без сетки в списке отрисовки.
    pub(crate) scene_like: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    view: CaptureView,
    camera_id: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
    warmup_frames: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Panel,
    Offscreen,
}

/// Внешнее состояние, от которого зависит продолжение задачи.
pub(crate) struct Environment {
    pub(crate) generation: u64,
    pub(crate) now: Instant,
    pub(crate) server_running: bool,
    pub(crate) client_gone: bool,
    pub(crate) target_ready: bool,
}

pub(crate) fn tool_specs() -> Vec<ToolSpec> {
    vec![ToolSpec::new(
        "capture_view",
        "Capture the Scene or Game panel as PNG, or render an offscreen view from the editor camera, the game camera, or a camera object.",
        json!({
            "type": "object",
            "properties": {
                "view": {"type": "string", "enum": ["scene", "game"]},
                "camera_id": {"type": "string"},
                "width": {"type": "integer", "minimum": 1, "maximum": 4096},
                "height": {"type": "integer", "minimum": 1, "maximum": 4096},
                "warmup_frames": {"type": "integer", "minimum": 1, "maximum": 64}
            },
            "required": ["view"],
            "additionalProperties": false
        }),
        Permission::Read,
    )]
}

/// Начинает задачу из `drain`: при ошибке отвечает сразу, иначе сохраняет вызов в состоянии.
pub(crate) fn begin(app: &mut EditorApp, call: ToolCall) {
    match prepare(app, &call.arguments, call.generation()) {
        Ok(plan) => {
            app.mcp_state_mut().mcp_capture = Some(CaptureJob {
                call,
                plan,
                phase: Phase::Rendering,
            })
        }
        Err(error) => {
            call.respond(ToolResult::error(error));
        }
    }
}

/// Продвигает задачу на один опрос; возвращает `true`, если ответ отправлен.
pub(crate) fn poll(app: &mut EditorApp, server_running: bool) -> bool {
    let Some(mut job) = app.mcp_state_mut().mcp_capture.take() else {
        return false;
    };
    let env = Environment {
        generation: app.mcp_state_mut().mcp_generation,
        now: Instant::now(),
        server_running,
        client_gone: job.call.is_abandoned(),
        target_ready: check_target(app.mcp_state_mut(), &job.plan.camera).is_ok(),
    };
    if advance(app, &mut job, &env) {
        return true;
    }
    app.mcp_state_mut().mcp_capture = Some(job);
    false
}

/// Отменяет задачу при выходе из редактора; renderer ещё жив, поэтому тикет освобождается,
/// а задание кодирования, если оно уже принято, получает флаг отмены.
pub(crate) fn cancel_active(app: &mut EditorApp, reason: &str) {
    if let Some(mut job) = app.mcp_state_mut().mcp_capture.take() {
        abort(app, &mut job, reason);
    }
}

fn advance(app: &mut EditorApp, job: &mut CaptureJob, env: &Environment) -> bool {
    if job.rendering() {
        advance_rendering(app, job, env)
    } else {
        advance_encoding(app, job, env)
    }
}

/// Фаза Rendering: готовый readback переводится в Encoding, если поколение совпало.
fn advance_rendering(app: &mut EditorApp, job: &mut CaptureJob, env: &Environment) -> bool {
    match take_ready(app, job.plan.ticket) {
        Some(Ok(image)) if env.generation == job.plan.generation => {
            match begin_encoding(app, job, image) {
                Ok(()) => false,
                Err(error) => {
                    complete(app, job, ToolResult::error(error));
                    true
                }
            }
        }
        Some(Err(error)) if env.generation == job.plan.generation => {
            complete(
                app,
                job,
                ToolResult::error(format!("capture failed: {error}")),
            );
            true
        }
        _ => match abort_reason(&job.plan, env) {
            Some(reason) => {
                abort(app, job, &reason);
                true
            }
            None => false,
        },
    }
}

/// Фаза Encoding: готовый PNG отправляется даже при брошенном клиенте и истёкшем сроке; отложенное
/// задание пробует занять слот, если ответа ещё нет.
fn advance_encoding(app: &mut EditorApp, job: &mut CaptureJob, env: &Environment) -> bool {
    let ready = take_encoded(app, job);
    match (encode_step(&job.plan, env, ready.is_some()), ready) {
        (EncodeStep::Cancel(reason), _) => {
            abort(app, job, &reason);
            true
        }
        (EncodeStep::Respond, Some(result)) => {
            job.call.respond(result.unwrap_or_else(ToolResult::error));
            true
        }
        _ => match offer_pending(app, job) {
            Ok(()) => false,
            Err(error) => {
                complete(app, job, ToolResult::error(error));
                true
            }
        },
    }
}

/// Переводит задачу в Encoding. GPU-ресурсы освобождаются здесь один раз; meta строится на главном
/// потоке, пиксели уходят кодировщику.
fn begin_encoding(
    app: &mut EditorApp,
    job: &mut CaptureJob,
    image: CapturedImage,
) -> Result<(), String> {
    release(app, &mut job.plan);
    let meta = meta_for(&job.plan, &image);
    job.phase = Phase::Encoding(EncodeWait {
        pending: Some(EncodeInput { image, meta }),
        flight: None,
    });
    offer_pending(app, job)
}

/// Передаёт отложенное задание рабочему потоку. Занятый слот оставляет его до следующего опроса.
fn offer_pending(app: &mut EditorApp, job: &mut CaptureJob) -> Result<(), String> {
    let Phase::Encoding(wait) = &mut job.phase else {
        return Ok(());
    };
    let Some(input) = wait.pending.take() else {
        return Ok(());
    };
    match app.mcp_state_mut().png_encoder.submit(input)? {
        Submit::Accepted(flight) => wait.flight = Some(flight),
        Submit::Full(input) => wait.pending = Some(input),
    }
    Ok(())
}

/// Результат кодирования, если он уже готов.
fn take_encoded(app: &mut EditorApp, job: &CaptureJob) -> Option<Result<ToolResult, String>> {
    let Phase::Encoding(wait) = &job.phase else {
        return None;
    };
    let flight = wait.flight.as_ref()?;
    app.mcp_state_mut().png_encoder.fetch(flight)
}

/// Нужен ли кадр, когда ни одна панель не видна: активный offscreen-захват требует отрисовки.
pub(crate) fn needs_scene_frame(
    scene_drawable: bool,
    game_drawable: bool,
    offscreen: bool,
) -> bool {
    scene_drawable || game_drawable || offscreen
}

/// Параметры offscreen-кадра текущей задачи; панельные задачи и задачи после readback кадра не
/// требуют: отрисовка offscreen нужна только в фазе Rendering.
pub(crate) fn offscreen_frame(plan: &CapturePlan, phase: &Phase) -> Option<OffscreenFrame> {
    if !matches!(phase, Phase::Rendering) {
        return None;
    }
    let Source::Offscreen(offscreen) = plan.source else {
        return None;
    };
    Some(OffscreenFrame {
        camera: plan.camera.clone(),
        size: offscreen.size,
        reset_history: offscreen.reset_pending,
        request: offscreen.warmup_left == 0 && plan.ticket.is_none(),
    })
}

/// Записывает результат кадра: тикет, если его запросили, и прогрев по показанным кадрам.
pub(crate) fn offscreen_rendered(
    job: Option<&mut CaptureJob>,
    requested: Option<CaptureTicket>,
    presented: bool,
) {
    let Some(job) = job else {
        return;
    };
    // Запрос readback выдаёт только `offscreen_frame` фазы Rendering, поэтому тикет и прогрев
    // относятся к задаче, которая ещё рисует кадр.
    if !job.rendering() {
        return;
    }
    if requested.is_some() {
        job.plan.ticket = requested;
    }
    if presented {
        note_presented(&mut job.plan);
    }
}

/// Показанный offscreen-кадр: первый кадр уже с `reset_history`, прогрев уменьшается.
fn note_presented(plan: &mut CapturePlan) {
    if let Source::Offscreen(offscreen) = &mut plan.source {
        offscreen.reset_pending = false;
        offscreen.warmup_left = offscreen.warmup_left.saturating_sub(1);
    }
}

/// Камера объекта с компонентом `camera`; `None`, если объекта нет или он не камера.
pub(crate) fn object_camera(world: &World, id: &str) -> Option<CaptureCamera> {
    let handle = world.handle(id)?;
    Some(CaptureCamera {
        camera: world.camera_of(handle)?,
        settings: world.camera_settings(handle).copied().unwrap_or_default(),
        post: world.post_process(handle).cloned().unwrap_or_default(),
        scene_like: false,
    })
}

/// Экранные шейдеры кадра захвата, как у панели того же вида: Editor их не применяет, Game берёт
/// шейдеры Game-камеры, камера объекта — свои.
pub(crate) fn screen_shaders_for(
    choice: &CameraChoice,
    game: &[ScreenShaderRef],
    world: &World,
) -> Vec<ScreenShaderRef> {
    match choice {
        CameraChoice::Editor => Vec::new(),
        CameraChoice::Game => game.to_vec(),
        CameraChoice::Object(id) => world
            .handle(id)
            .and_then(|handle| world.screen_shaders(handle))
            .map(<[_]>::to_vec)
            .unwrap_or_default(),
    }
}

fn prepare(
    app: &mut EditorApp,
    arguments: &Value,
    call_generation: u64,
) -> Result<CapturePlan, String> {
    let request = parse_request(arguments)?;
    let state = app.mcp_state_mut();
    if state.mcp_capture.is_some() {
        return Err("another capture_view is already in progress".to_owned());
    }
    if state.mcp_generation != call_generation {
        return Err(STALE_GENERATION.to_owned());
    }
    if state.project.is_none() {
        return Err("project is not open".to_owned());
    }
    let camera = camera_choice(request.view, request.camera_id.as_deref());
    check_target(state, &camera)?;
    let (panel_visible, last_size) = match request.view {
        CaptureView::Scene => (state.scene_view.visible, state.scene_view.size),
        CaptureView::Game => (state.game_view.visible, state.game_view.size),
    };
    let mode = choose_mode(panel_visible, &request);
    let Some((renderer, scene)) = app.mcp_capture_gpu() else {
        return Err("renderer is not available".to_owned());
    };
    let (source, ticket) = match mode {
        Mode::Panel => {
            let viewport = match request.view {
                CaptureView::Scene => scene.viewport,
                CaptureView::Game => scene.game_viewport,
            };
            (
                Source::Panel,
                Some(renderer.request_viewport_capture(viewport)),
            )
        }
        Mode::Offscreen => {
            let max_side = MAX_CAPTURE_SIDE.min(renderer.max_texture_size());
            let size = resolve_size(request.width, request.height, last_size, max_side)?;
            let offscreen = Offscreen {
                size,
                warmup_frames: request.warmup_frames,
                warmup_left: request.warmup_frames,
                reset_pending: true,
            };
            (Source::Offscreen(offscreen), None)
        }
    };
    Ok(CapturePlan {
        generation: call_generation,
        deadline: Instant::now() + CAPTURE_DEADLINE,
        view: request.view,
        camera,
        source,
        ticket,
    })
}

fn parse_request(arguments: &Value) -> Result<Request, String> {
    let view = match required_string(arguments, "view")? {
        "scene" => CaptureView::Scene,
        "game" => CaptureView::Game,
        _ => return Err("view must be \"scene\" or \"game\"".to_owned()),
    };
    Ok(Request {
        view,
        camera_id: optional_string(arguments, "camera_id")?.map(str::to_owned),
        width: optional_u64(arguments, "width")?,
        height: optional_u64(arguments, "height")?,
        warmup_frames: warmup_frames(optional_u64(arguments, "warmup_frames")?)?,
    })
}

fn warmup_frames(value: Option<u64>) -> Result<u32, String> {
    match value {
        None => Ok(DEFAULT_WARMUP_FRAMES),
        Some(frames) if WARMUP_FRAMES.contains(&frames) => Ok(frames as u32),
        Some(_) => Err(format!(
            "warmup_frames must be between {} and {}",
            WARMUP_FRAMES.start(),
            WARMUP_FRAMES.end()
        )),
    }
}

/// Панель берётся, только если она видна и вызов не задаёт камеру или размер.
fn choose_mode(panel_visible: bool, request: &Request) -> Mode {
    let explicit =
        request.camera_id.is_some() || request.width.is_some() || request.height.is_some();
    if panel_visible && !explicit {
        Mode::Panel
    } else {
        Mode::Offscreen
    }
}

fn camera_choice(view: CaptureView, camera_id: Option<&str>) -> CameraChoice {
    match (camera_id, view) {
        (Some(id), _) => CameraChoice::Object(id.to_owned()),
        (None, CaptureView::Scene) => CameraChoice::Editor,
        (None, CaptureView::Game) => CameraChoice::Game,
    }
}

/// Размер offscreen-кадра: явные значения проверяются, отсутствующие берутся из панели или 1280×720.
fn resolve_size(
    width: Option<u64>,
    height: Option<u64>,
    last: [u32; 2],
    max_side: u32,
) -> Result<[u32; 2], String> {
    let fallback = if last[0] > 0 && last[1] > 0 {
        last
    } else {
        DEFAULT_CAPTURE_SIZE
    };
    Ok([
        side("width", width, fallback[0].min(max_side), max_side)?,
        side("height", height, fallback[1].min(max_side), max_side)?,
    ])
}

fn side(name: &str, value: Option<u64>, fallback: u32, max_side: u32) -> Result<u32, String> {
    match value {
        None => Ok(fallback),
        Some(value) => u32::try_from(value)
            .ok()
            .filter(|side| (1..=max_side).contains(side))
            .ok_or_else(|| format!("{name} must be between 1 and {max_side}")),
    }
}

/// Проверяет, что у задачи есть сцена и, для объекта, камера. Ошибка не меняет состояние.
fn check_target(state: &mut EditorState, camera: &CameraChoice) -> Result<(), String> {
    let parts = state
        .frame_parts()
        .ok_or_else(|| "scene is not open".to_owned())?;
    match camera {
        CameraChoice::Object(id) if !camera_exists(parts.world, id) => Err(format!(
            "camera '{id}' was not found or has no camera component"
        )),
        CameraChoice::Object(_) | CameraChoice::Editor | CameraChoice::Game => Ok(()),
    }
}

fn camera_exists(world: &World, id: &str) -> bool {
    world
        .handle(id)
        .is_some_and(|handle| world.camera_of(handle).is_some())
}

/// Причина отмены задачи или `None`, если она может продолжаться.
/// Порядок важен: смена сцены и потеря сервера важнее таймаута.
fn abort_reason(plan: &CapturePlan, env: &Environment) -> Option<String> {
    if env.generation != plan.generation {
        return Some("project or scene changed while capture was pending".to_owned());
    }
    if !env.server_running {
        return Some("MCP server stopped while capture was pending".to_owned());
    }
    if env.client_gone {
        return Some("client disconnected before capture finished".to_owned());
    }
    if !env.target_ready {
        return Some("scene or camera is no longer available".to_owned());
    }
    if env.now >= plan.deadline {
        return Some(format!(
            "capture timed out after {} s: no presented frame returned the image (window minimized or frames skipped)",
            CAPTURE_DEADLINE.as_secs()
        ));
    }
    None
}

fn take_ready(
    app: &mut EditorApp,
    ticket: Option<CaptureTicket>,
) -> Option<Result<CapturedImage, RenderError>> {
    let (renderer, _) = app.mcp_capture_gpu()?;
    let mut ready = None;
    for (done, result) in renderer.take_captures() {
        if Some(done) == ticket {
            ready = Some(result);
        }
    }
    ready
}

/// Отменяет задачу: освобождает то, что держит её фаза, и отвечает ошибкой с причиной.
fn abort(app: &mut EditorApp, job: &mut CaptureJob, reason: &str) {
    release_phase(app, job);
    job.call.respond(ToolResult::error(reason));
}

/// Закрывает задачу ответом; освобождает ресурсы фазы, если они ещё есть.
fn complete(app: &mut EditorApp, job: &mut CaptureJob, response: ToolResult) {
    release_phase(app, job);
    job.call.respond(response);
}

/// Освобождает ресурсы текущей фазы. В Rendering это GPU-ресурсы; в Encoding ресурсов GPU нет, а
/// принятое задание получает флаг отмены.
fn release_phase(app: &mut EditorApp, job: &mut CaptureJob) {
    match &job.phase {
        Phase::Rendering => release(app, &mut job.plan),
        Phase::Encoding(wait) => wait.cancel(),
    }
}

/// Отменяет тикет и освобождает offscreen-viewport; `cancel_capture` безопасен в любом состоянии.
/// Вызывается один раз: при переходе в Encoding и при отмене в Rendering.
fn release(app: &mut EditorApp, plan: &mut CapturePlan) {
    let offscreen = plan.is_offscreen();
    let ticket = plan.ticket.take();
    let Some((renderer, scene)) = app.mcp_capture_gpu() else {
        return;
    };
    if let Some(ticket) = ticket {
        renderer.cancel_capture(ticket);
    }
    let released = if offscreen {
        scene.release_capture_viewport(renderer)
    } else {
        Ok(())
    };
    if let Err(error) = released {
        app.mcp_state_mut()
            .log
            .push(LogLevel::Error, format!("[MCP] capture viewport: {error}"));
    }
}

/// Мета ответа, которую собирает главный поток по готовому readback.
fn meta_for(plan: &CapturePlan, image: &CapturedImage) -> Value {
    let mut meta = json!({
        "view": plan.view.label(),
        "source": plan.source_label(),
        "camera": plan.camera.label(),
        "width": image.width,
        "height": image.height,
    });
    if let Source::Offscreen(offscreen) = plan.source {
        meta["warmup_frames"] = json!(offscreen.warmup_frames);
    }
    meta
}

/// Кодирует PNG, base64 и ответ; выполняется на рабочем потоке `rve-mcp-png`.
fn encode_response(image: CapturedImage, meta: Value) -> Result<ToolResult, String> {
    let png = encode_png(&image)?;
    let mut result = ToolResult::image_png(&png);
    result
        .content
        .push(json!({"type": "text", "text": meta.to_string()}));
    result.structured_content = Some(meta);
    Ok(result)
}

/// Решение опроса в фазе Encoding. Потеря камеры здесь не отменяет задачу: кадр уже снят.
#[derive(Debug, PartialEq, Eq)]
enum EncodeStep {
    Cancel(String),
    Respond,
    Wait,
}

/// Порядок проверок: смена поколения, затем готовый результат, затем сервер, клиент и срок.
fn encode_step(plan: &CapturePlan, env: &Environment, ready: bool) -> EncodeStep {
    if env.generation != plan.generation {
        return EncodeStep::Cancel("project or scene changed while capture was pending".to_owned());
    }
    if ready {
        return EncodeStep::Respond;
    }
    if !env.server_running {
        return EncodeStep::Cancel("MCP server stopped while capture was pending".to_owned());
    }
    if env.client_gone {
        return EncodeStep::Cancel("client disconnected before capture finished".to_owned());
    }
    if env.now >= plan.deadline {
        return EncodeStep::Cancel(format!(
            "capture timed out after {} s: PNG encoding did not finish",
            CAPTURE_DEADLINE.as_secs()
        ));
    }
    EncodeStep::Wait
}

fn encode_png(image: &CapturedImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;
    writer
        .write_image_data(&image.rgba8)
        .map_err(|error| error.to_string())?;
    writer.finish().map_err(|error| error.to_string())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(view: CaptureView) -> Request {
        Request {
            view,
            camera_id: None,
            width: None,
            height: None,
            warmup_frames: DEFAULT_WARMUP_FRAMES,
        }
    }

    fn plan(source: Source, camera: CameraChoice, deadline: Instant) -> CapturePlan {
        CapturePlan {
            generation: 3,
            deadline,
            view: CaptureView::Scene,
            camera,
            source,
            ticket: None,
        }
    }

    fn offscreen(warmup: u32) -> Source {
        Source::Offscreen(Offscreen {
            size: [640, 360],
            warmup_frames: warmup,
            warmup_left: warmup,
            reset_pending: true,
        })
    }

    fn env(plan: &CapturePlan) -> Environment {
        Environment {
            generation: plan.generation,
            now: plan.deadline - Duration::from_millis(1),
            server_running: true,
            client_gone: false,
            target_ready: true,
        }
    }

    fn parse(json: Value) -> Result<Request, String> {
        parse_request(&json)
    }

    #[test]
    fn view_is_required_and_limited_to_scene_or_game() {
        assert_eq!(
            parse(json!({"view": "scene"})).unwrap().view,
            CaptureView::Scene
        );
        assert_eq!(
            parse(json!({"view": "game"})).unwrap().view,
            CaptureView::Game
        );
        assert!(parse(json!({})).unwrap_err().contains("view"));
        assert!(
            parse(json!({"view": "preview"}))
                .unwrap_err()
                .contains("scene")
        );
    }

    #[test]
    fn camera_id_must_be_a_string_and_warmup_has_its_range() {
        assert!(parse(json!({"view": "scene", "camera_id": 4})).is_err());
        let parsed = parse(json!({"view": "game", "warmup_frames": 1})).unwrap();
        assert_eq!(parsed.warmup_frames, 1);
        assert_eq!(parsed.camera_id, None);
        assert_eq!(
            parse(json!({"view": "game"})).unwrap().warmup_frames,
            DEFAULT_WARMUP_FRAMES
        );
    }

    #[test]
    fn warmup_bounds_are_inclusive_one_to_sixty_four() {
        assert_eq!(warmup_frames(None), Ok(8));
        assert_eq!(warmup_frames(Some(1)), Ok(1));
        assert_eq!(warmup_frames(Some(64)), Ok(64));
        assert!(warmup_frames(Some(0)).is_err());
        assert!(warmup_frames(Some(65)).is_err());
    }

    #[test]
    fn visible_panel_without_overrides_is_the_panel_mode() {
        let plain = request(CaptureView::Scene);
        assert_eq!(choose_mode(true, &plain), Mode::Panel);
        assert_eq!(choose_mode(false, &plain), Mode::Offscreen);

        let with_camera = Request {
            camera_id: Some("cam".to_owned()),
            ..request(CaptureView::Game)
        };
        assert_eq!(choose_mode(true, &with_camera), Mode::Offscreen);

        let with_width = Request {
            width: Some(800),
            ..request(CaptureView::Scene)
        };
        assert_eq!(choose_mode(true, &with_width), Mode::Offscreen);
        let with_height = Request {
            height: Some(600),
            ..request(CaptureView::Scene)
        };
        assert_eq!(choose_mode(true, &with_height), Mode::Offscreen);
    }

    #[test]
    fn camera_follows_view_unless_camera_id_is_given() {
        assert_eq!(
            camera_choice(CaptureView::Scene, None),
            CameraChoice::Editor
        );
        assert_eq!(camera_choice(CaptureView::Game, None), CameraChoice::Game);
        assert_eq!(
            camera_choice(CaptureView::Game, Some("drone")),
            CameraChoice::Object("drone".to_owned())
        );
        assert_eq!(
            camera_choice(CaptureView::Scene, Some("drone")),
            CameraChoice::Object("drone".to_owned())
        );
    }

    #[test]
    fn size_defaults_to_the_last_panel_size_or_720p() {
        assert_eq!(resolve_size(None, None, [800, 600], 4096), Ok([800, 600]));
        assert_eq!(resolve_size(None, None, [0, 0], 4096), Ok([1280, 720]));
        assert_eq!(
            resolve_size(Some(640), None, [800, 600], 4096),
            Ok([640, 600])
        );
        assert_eq!(resolve_size(None, Some(480), [0, 0], 4096), Ok([1280, 480]));
    }

    #[test]
    fn explicit_size_is_bounded_by_the_side_limit_and_texture_size() {
        assert_eq!(resolve_size(Some(1), Some(1), [0, 0], 4096), Ok([1, 1]));
        assert_eq!(
            resolve_size(Some(4096), Some(4096), [0, 0], 4096),
            Ok([4096, 4096])
        );
        assert!(resolve_size(Some(0), None, [0, 0], 4096).is_err());
        assert!(resolve_size(Some(4097), None, [0, 0], 4096).is_err());
        assert!(resolve_size(None, Some(2049), [0, 0], 2048).is_err());
        assert!(resolve_size(Some(u64::MAX), None, [0, 0], 4096).is_err());
    }

    #[test]
    fn default_size_is_clamped_to_the_texture_limit() {
        assert_eq!(
            resolve_size(None, None, [4000, 3000], 2048),
            Ok([2048, 2048])
        );
    }

    #[test]
    fn warmup_gates_the_readback_request_and_reset_applies_once() {
        let mut plan = plan(offscreen(2), CameraChoice::Editor, Instant::now());
        let first = offscreen_frame(&plan, &Phase::Rendering).unwrap();
        assert!(first.reset_history);
        assert!(!first.request);

        note_presented(&mut plan);
        let second = offscreen_frame(&plan, &Phase::Rendering).unwrap();
        assert!(!second.reset_history);
        assert!(!second.request);

        note_presented(&mut plan);
        let ready = offscreen_frame(&plan, &Phase::Rendering).unwrap();
        assert!(ready.request);
        assert_eq!(ready.size, [640, 360]);
    }

    #[test]
    fn panel_jobs_do_not_produce_offscreen_frames() {
        let plan = plan(Source::Panel, CameraChoice::Editor, Instant::now());
        assert!(offscreen_frame(&plan, &Phase::Rendering).is_none());
        assert!(!plan.is_offscreen());
    }

    #[test]
    fn job_continues_before_the_deadline_and_matching_generation() {
        let plan = plan(
            offscreen(8),
            CameraChoice::Editor,
            Instant::now() + CAPTURE_DEADLINE,
        );
        assert_eq!(abort_reason(&plan, &env(&plan)), None);
    }

    #[test]
    fn job_expires_at_its_ten_second_deadline() {
        let plan = plan(offscreen(8), CameraChoice::Editor, Instant::now());
        let env = Environment {
            now: plan.deadline,
            ..env(&plan)
        };
        let reason = abort_reason(&plan, &env).unwrap();
        assert!(reason.contains("10 s"), "{reason}");
    }

    #[test]
    fn generation_change_cancels_before_any_other_check() {
        let plan = plan(offscreen(8), CameraChoice::Editor, Instant::now());
        let env = Environment {
            generation: plan.generation + 1,
            now: plan.deadline,
            server_running: false,
            client_gone: true,
            target_ready: false,
        };
        let reason = abort_reason(&plan, &env).unwrap();
        assert!(reason.contains("changed"), "{reason}");
    }

    #[test]
    fn stopped_server_disconnected_client_and_missing_camera_cancel() {
        let plan = plan(
            offscreen(8),
            CameraChoice::Object("drone".to_owned()),
            Instant::now() + CAPTURE_DEADLINE,
        );
        let stopped = Environment {
            server_running: false,
            ..env(&plan)
        };
        assert!(
            abort_reason(&plan, &stopped)
                .unwrap()
                .contains("MCP server")
        );
        let gone = Environment {
            client_gone: true,
            ..env(&plan)
        };
        assert!(abort_reason(&plan, &gone).unwrap().contains("disconnected"));
        let missing = Environment {
            target_ready: false,
            ..env(&plan)
        };
        assert!(abort_reason(&plan, &missing).unwrap().contains("no longer"));
    }

    #[test]
    fn needs_scene_frame_keeps_an_offscreen_capture_when_both_panels_are_hidden() {
        assert!(needs_scene_frame(true, false, false));
        assert!(needs_scene_frame(false, true, false));
        assert!(needs_scene_frame(false, false, true));
        assert!(!needs_scene_frame(false, false, false));
    }

    #[test]
    fn capture_screen_shaders_follow_the_panel_that_shows_the_camera() {
        let document = engine_scene::SceneDocument::parse(
            "capture.rvescene",
            r#"
[[assets]]
path="/fx/vhs.glsl"
[[objects]]
id="cam"
[objects.camera]
active=true
[[objects.camera.screen_shaders]]
shader="/fx/vhs.glsl"
stage="after_post"
[[objects]]
id="plain"
[objects.camera]
active=false
"#,
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let game = world
            .screen_shaders(world.handle("cam").unwrap())
            .unwrap()
            .to_vec();
        assert_eq!(game.len(), 1);

        assert!(screen_shaders_for(&CameraChoice::Editor, &game, &world).is_empty());
        assert_eq!(screen_shaders_for(&CameraChoice::Game, &game, &world), game);
        assert_eq!(
            screen_shaders_for(&CameraChoice::Object("cam".to_owned()), &[], &world),
            game
        );
        assert!(
            screen_shaders_for(&CameraChoice::Object("plain".to_owned()), &game, &world).is_empty()
        );
        assert!(
            screen_shaders_for(&CameraChoice::Object("missing".to_owned()), &game, &world)
                .is_empty()
        );
    }

    #[test]
    fn png_keeps_size_and_pixels_of_the_captured_image() {
        let image = CapturedImage {
            width: 2,
            height: 1,
            rgba8: vec![255, 0, 0, 255, 0, 0, 255, 128],
        };
        let bytes = encode_png(&image).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        let info = reader.info().clone();
        assert_eq!((info.width, info.height), (2, 1));
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut pixels).unwrap();
        assert_eq!(pixels, image.rgba8);
    }

    /// Предел ожидания в тестах: только страховка от зависания, сам тест синхронизируется каналами.
    const TEST_WAIT: Duration = Duration::from_secs(30);

    fn job_input(seed: u8) -> EncodeInput {
        EncodeInput {
            image: CapturedImage {
                width: 1,
                height: 1,
                rgba8: vec![seed, 0, 0, 255],
            },
            meta: json!({ "seed": seed }),
        }
    }

    /// Кодировщик для тестов: сообщает о старте, ждёт токен от теста и кодирует настоящий PNG.
    /// Токен идёт через `bounded(0)`, поэтому отправка возвращается, только когда поток его принял.
    fn gated_encoder(started: flume::Sender<()>, gate: flume::Receiver<()>) -> PngEncoder {
        PngEncoder::with_encode(Arc::new(
            move |image: CapturedImage, meta: Value| -> Result<ToolResult, String> {
                let _ = started.send(());
                let _ = gate.recv();
                encode_response(image, meta)
            },
        ))
    }

    fn accepted(submit: Result<Submit, String>) -> InFlight {
        match submit.unwrap() {
            Submit::Accepted(flight) => flight,
            Submit::Full(_) => panic!("worker slot must be free"),
        }
    }

    /// Слот освобождается, когда рабочий поток забирает отменённое задание; ждём это через `yield`.
    fn accepted_when_free(encoder: &mut PngEncoder, input: EncodeInput) -> InFlight {
        let mut input = input;
        loop {
            match encoder.submit(input).unwrap() {
                Submit::Accepted(flight) => return flight,
                Submit::Full(back) => {
                    input = back;
                    std::thread::yield_now();
                }
            }
        }
    }

    fn wait_result(flight: &InFlight) -> Result<ToolResult, String> {
        flight
            .done
            .recv_timeout(TEST_WAIT)
            .expect("encoder must answer")
    }

    #[test]
    fn png_encoder_returns_the_captured_pixels_and_meta() {
        let (started, _started_rx) = flume::unbounded();
        let (gate_tx, gate_rx) = flume::bounded::<()>(0);
        let mut encoder = gated_encoder(started, gate_rx);
        let image = CapturedImage {
            width: 2,
            height: 1,
            rgba8: vec![255, 0, 0, 255, 0, 0, 255, 128],
        };
        let meta = json!({ "view": "scene", "width": 2, "height": 1 });
        let flight = accepted(encoder.submit(EncodeInput {
            image: image.clone(),
            meta: meta.clone(),
        }));
        gate_tx.send(()).unwrap();
        let result = wait_result(&flight).unwrap();

        assert_eq!(result.structured_content, Some(meta.clone()));
        assert_eq!(
            result.content[1],
            json!({"type": "text", "text": meta.to_string()})
        );
        let data = result.content[0]["data"].as_str().unwrap();
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut pixels).unwrap();
        assert_eq!(pixels, image.rgba8);
    }

    #[test]
    fn job_cancelled_while_queued_is_never_encoded() {
        let (started, started_rx) = flume::unbounded();
        let (gate_tx, gate_rx) = flume::bounded::<()>(0);
        let mut encoder = gated_encoder(started, gate_rx);
        let running = accepted(encoder.submit(job_input(1)));
        started_rx.recv_timeout(TEST_WAIT).unwrap();
        let queued = accepted(encoder.submit(job_input(2)));
        queued.cancel();
        gate_tx.send(()).unwrap();
        wait_result(&running).unwrap();

        let third = accepted_when_free(&mut encoder, job_input(3));
        gate_tx.send(()).unwrap();
        wait_result(&third).unwrap();
        assert_eq!(
            started_rx.drain().count(),
            1,
            "the cancelled job must not reach the encoder"
        );
    }

    #[test]
    fn result_of_a_job_cancelled_during_encoding_is_discarded() {
        let (started, started_rx) = flume::unbounded();
        let (gate_tx, gate_rx) = flume::bounded::<()>(0);
        let mut encoder = gated_encoder(started, gate_rx);
        let running = accepted(encoder.submit(job_input(1)));
        started_rx.recv_timeout(TEST_WAIT).unwrap();
        running.cancel();
        gate_tx.send(()).unwrap();

        let next = accepted_when_free(&mut encoder, job_input(2));
        gate_tx.send(()).unwrap();
        let result = wait_result(&next).unwrap();

        assert_eq!(result.structured_content, Some(json!({ "seed": 2 })));
        assert!(
            running.done.try_recv().is_err(),
            "cancelled job must not deliver its result"
        );
        assert_eq!(started_rx.drain().count(), 1);
    }

    #[test]
    fn busy_worker_with_full_slot_returns_the_job_and_dropped_pending_never_encodes() {
        let (started, started_rx) = flume::unbounded();
        let (gate_tx, gate_rx) = flume::bounded::<()>(0);
        let mut encoder = gated_encoder(started, gate_rx);
        let running = accepted(encoder.submit(job_input(1)));
        started_rx.recv_timeout(TEST_WAIT).unwrap();
        let queued = accepted(encoder.submit(job_input(2)));
        let pending = match encoder.submit(job_input(3)).unwrap() {
            Submit::Full(input) => input,
            Submit::Accepted(_) => panic!("slot is occupied by the queued job"),
        };
        assert_eq!(pending.meta, json!({ "seed": 3 }));
        drop(pending);

        gate_tx.send(()).unwrap();
        wait_result(&running).unwrap();
        gate_tx.send(()).unwrap();
        wait_result(&queued).unwrap();

        let next = accepted(encoder.submit(job_input(4)));
        gate_tx.send(()).unwrap();
        wait_result(&next).unwrap();
        assert_eq!(
            started_rx.drain().count(),
            2,
            "jobs 2 and 4 start encoding; the dropped pending job 3 never does"
        );
    }

    #[test]
    fn panicked_worker_reports_an_error_and_the_next_job_starts_a_new_worker() {
        let encode: EncodeFn = Arc::new(
            |image: CapturedImage, meta: Value| -> Result<ToolResult, String> {
                if meta["crash"] == json!(true) {
                    panic!("PNG worker crashed in test");
                }
                encode_response(image, meta)
            },
        );
        let mut encoder = PngEncoder::with_encode(encode);
        let crashing = accepted(encoder.submit(EncodeInput {
            image: job_input(0).image,
            meta: json!({ "crash": true }),
        }));
        assert!(matches!(
            crashing.done.recv_timeout(TEST_WAIT),
            Err(flume::RecvTimeoutError::Disconnected)
        ));
        assert_eq!(
            encoder.fetch(&crashing),
            Some(Err(ENCODER_STOPPED.to_owned()))
        );

        let healthy = accepted(encoder.submit(job_input(5)));
        let result = wait_result(&healthy).unwrap();
        assert_eq!(result.structured_content, Some(json!({ "seed": 5 })));
    }

    #[test]
    fn encoding_cancels_on_generation_change_even_with_a_ready_result() {
        let plan = plan(
            offscreen(8),
            CameraChoice::Editor,
            Instant::now() + CAPTURE_DEADLINE,
        );
        let env = Environment {
            generation: plan.generation + 1,
            ..env(&plan)
        };
        let step = encode_step(&plan, &env, true);
        assert!(
            matches!(&step, EncodeStep::Cancel(reason) if reason.contains("changed")),
            "{step:?}"
        );
    }

    #[test]
    fn ready_png_is_sent_to_a_gone_client_after_the_deadline() {
        let plan = plan(offscreen(8), CameraChoice::Editor, Instant::now());
        let env = Environment {
            now: plan.deadline,
            client_gone: true,
            ..env(&plan)
        };
        assert_eq!(encode_step(&plan, &env, true), EncodeStep::Respond);
    }

    #[test]
    fn encoding_at_the_deadline_without_a_result_reports_png_encoding() {
        let plan = plan(offscreen(8), CameraChoice::Editor, Instant::now());
        let env = Environment {
            now: plan.deadline,
            ..env(&plan)
        };
        let step = encode_step(&plan, &env, false);
        assert!(
            matches!(&step, EncodeStep::Cancel(reason) if reason.contains("PNG encoding did not finish")),
            "{step:?}"
        );
    }

    #[test]
    fn encoding_ignores_a_lost_camera_but_not_a_stopped_server() {
        let plan = plan(
            offscreen(8),
            CameraChoice::Object("drone".to_owned()),
            Instant::now() + CAPTURE_DEADLINE,
        );
        let lost_camera = Environment {
            target_ready: false,
            ..env(&plan)
        };
        assert_eq!(encode_step(&plan, &lost_camera, false), EncodeStep::Wait);

        let stopped = Environment {
            server_running: false,
            ..env(&plan)
        };
        let step = encode_step(&plan, &stopped, false);
        assert!(
            matches!(&step, EncodeStep::Cancel(reason) if reason.contains("MCP server")),
            "{step:?}"
        );
    }

    #[test]
    fn encoding_phase_never_requests_an_offscreen_readback() {
        let plan = plan(
            offscreen(0),
            CameraChoice::Editor,
            Instant::now() + CAPTURE_DEADLINE,
        );
        assert!(offscreen_frame(&plan, &Phase::Rendering).unwrap().request);
        let encoding = Phase::Encoding(EncodeWait {
            pending: Some(job_input(1)),
            flight: None,
        });
        assert!(offscreen_frame(&plan, &encoding).is_none());
    }
}
