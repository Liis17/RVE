//! Инструмент `capture_view`: PNG панели Scene или Game либо offscreen-вида выбранной камеры.
//!
//! Задача живёт в `EditorState` от `drain` до готового кадра, поэтому HTTP-ответ приходит только
//! после реального захвата. `CapturePlan` отделён от `ToolCall`: автомат и выбор режима тестируются
//! без сокета и GPU, а GPU-часть вызывает `EditorApp` через `mcp_capture_gpu`.

use super::read::{optional_string, optional_u64, required_string};
use crate::app::EditorApp;
use crate::console::LogLevel;
use crate::state::EditorState;
use engine_mcp::{Permission, ToolCall, ToolResult, ToolSpec};
use engine_render::{CaptureTicket, CapturedImage, RenderError};
use engine_scene::camera::Camera;
use engine_scene::{CameraSettings, PostProcessSettings, ScreenShaderRef, World};
use serde_json::{Value, json};
use std::ops::RangeInclusive;
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

/// Задача, ожидающая ответа: вызов HTTP и состояние захвата.
pub(crate) struct CaptureJob {
    call: ToolCall,
    pub(crate) plan: CapturePlan,
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
        Ok(plan) => app.mcp_state_mut().mcp_capture = Some(CaptureJob { call, plan }),
        Err(error) => {
            call.respond(ToolResult::error(error));
        }
    }
}

/// Забирает готовый захват и снимает задачу при отмене; возвращает `true`, если ответ отправлен.
pub(crate) fn poll(app: &mut EditorApp, server_running: bool) -> bool {
    let Some(job) = app.mcp_state_mut().mcp_capture.take() else {
        return false;
    };
    let ready = take_ready(app, job.plan.ticket);
    let env = Environment {
        generation: app.mcp_state_mut().mcp_generation,
        now: Instant::now(),
        server_running,
        client_gone: job.call.is_abandoned(),
        target_ready: check_target(app.mcp_state_mut(), &job.plan.camera).is_ok(),
    };
    match ready {
        Some(result) if env.generation == job.plan.generation => {
            finish(app, job, result);
            true
        }
        _ => match abort_reason(&job.plan, &env) {
            Some(reason) => {
                abort(app, job, &reason);
                true
            }
            None => {
                app.mcp_state_mut().mcp_capture = Some(job);
                false
            }
        },
    }
}

/// Отменяет задачу при выходе из редактора; renderer ещё жив, поэтому тикет освобождается.
pub(crate) fn cancel_active(app: &mut EditorApp, reason: &str) {
    if let Some(job) = app.mcp_state_mut().mcp_capture.take() {
        abort(app, job, reason);
    }
}

/// Нужен ли кадр, когда ни одна панель не видна: активный offscreen-захват требует отрисовки.
pub(crate) fn needs_scene_frame(
    scene_drawable: bool,
    game_drawable: bool,
    offscreen: bool,
) -> bool {
    scene_drawable || game_drawable || offscreen
}

/// Параметры offscreen-кадра текущей задачи; панельные задачи кадра не требуют.
pub(crate) fn offscreen_frame(plan: &CapturePlan) -> Option<OffscreenFrame> {
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

fn finish(app: &mut EditorApp, mut job: CaptureJob, result: Result<CapturedImage, RenderError>) {
    let response = match result {
        Ok(image) => response_for(&image, &job.plan).unwrap_or_else(ToolResult::error),
        Err(error) => ToolResult::error(format!("capture failed: {error}")),
    };
    release(app, &mut job.plan);
    job.call.respond(response);
}

fn abort(app: &mut EditorApp, mut job: CaptureJob, reason: &str) {
    release(app, &mut job.plan);
    job.call.respond(ToolResult::error(reason));
}

/// Отменяет тикет и освобождает offscreen-viewport; `cancel_capture` безопасен в любом состоянии.
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

fn response_for(image: &CapturedImage, plan: &CapturePlan) -> Result<ToolResult, String> {
    let png = encode_png(image)?;
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
    let mut result = ToolResult::image_png(&png);
    result
        .content
        .push(json!({"type": "text", "text": meta.to_string()}));
    result.structured_content = Some(meta);
    Ok(result)
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
        let first = offscreen_frame(&plan).unwrap();
        assert!(first.reset_history);
        assert!(!first.request);

        note_presented(&mut plan);
        let second = offscreen_frame(&plan).unwrap();
        assert!(!second.reset_history);
        assert!(!second.request);

        note_presented(&mut plan);
        let ready = offscreen_frame(&plan).unwrap();
        assert!(ready.request);
        assert_eq!(ready.size, [640, 360]);
    }

    #[test]
    fn panel_jobs_do_not_produce_offscreen_frames() {
        let plan = plan(Source::Panel, CameraChoice::Editor, Instant::now());
        assert!(offscreen_frame(&plan).is_none());
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
}
