mod content;
mod debug_overlay;
mod scene;

use content::Content;
use debug_overlay::DebugOverlay;
use engine_assets::{ASSETS_DIR, AssetError, Assets};
use engine_core::Time;
use engine_project::BootstrapConfig;
use engine_render::{DrawCommand, Frame, FrameStatus, MeshId, Renderer, RendererConfig, TextureId};
use engine_resources::{ResourceConfig, ResourceSystem};
use engine_window::{PhysicalSize, Window, WindowConfig, WindowHandler, run};
use scene::Scene;
use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "Использование: sandbox [--frames N] [--render-scale S] [--resize-test] [--scene triangle|cube|shadows] [--model /путь.fbx] [--texture /путь.png] [--debug-overlay]";
const RESIZE_SIZE: PhysicalSize<u32> = PhysicalSize::new(960, 540);
const RESTORE_SIZE: PhysicalSize<u32> = PhysicalSize::new(1280, 720);

struct Options {
    frames: Option<u64>,
    render_scale: f32,
    resize_test: bool,
    scene: Scene,
    /// Путь модели внутри `assets`, например `/model/cube.fbx`.
    model: Option<String>,
    /// Путь PNG внутри `assets`, заменяющий текстуры сцены или модели.
    texture: Option<String>,
    /// Показывать панель FPS, памяти и загрузки CPU/GPU.
    debug_overlay: bool,
}

fn parse_args<I, S>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let mut frames = None;
    let mut render_scale = None;
    let mut resize_test = false;
    let mut scene = None;
    let mut model = None;
    let mut texture = None;
    let mut debug_overlay = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--frames" => {
                if frames.is_some() {
                    return Err("флаг --frames указан повторно".to_owned());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "после --frames требуется N".to_owned())?;
                let value = value.parse::<u64>().map_err(|_| {
                    "значение --frames должно быть положительным целым u64".to_owned()
                })?;
                if value == 0 {
                    return Err("значение --frames должно быть больше нуля".to_owned());
                }
                frames = Some(value);
            }
            "--render-scale" => {
                if render_scale.is_some() {
                    return Err("флаг --render-scale указан повторно".to_owned());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "после --render-scale требуется S".to_owned())?;
                let value = value
                    .parse::<f32>()
                    .map_err(|_| "значение --render-scale должно быть числом".to_owned())?;
                if !value.is_finite() || value <= 0.0 || value > 1.0 {
                    return Err("--render-scale должен быть конечным числом в (0, 1]".to_owned());
                }
                render_scale = Some(value);
            }
            "--resize-test" => {
                if resize_test {
                    return Err("флаг --resize-test указан повторно".to_owned());
                }
                resize_test = true;
            }
            "--debug-overlay" => {
                if debug_overlay {
                    return Err("флаг --debug-overlay указан повторно".to_owned());
                }
                debug_overlay = true;
            }
            "--scene" => {
                if scene.is_some() {
                    return Err("флаг --scene указан повторно".to_owned());
                }
                let value = args.next().ok_or_else(|| {
                    "после --scene требуется triangle, cube или shadows".to_owned()
                })?;
                scene = Some(Scene::parse(&value).ok_or_else(|| {
                    format!("неизвестная сцена: {value}; доступны triangle, cube и shadows")
                })?);
            }
            "--model" => {
                if model.is_some() {
                    return Err("флаг --model указан повторно".to_owned());
                }
                model = Some(
                    args.next()
                        .ok_or_else(|| "после --model требуется путь в assets".to_owned())?,
                );
            }
            "--texture" => {
                if texture.is_some() {
                    return Err("флаг --texture указан повторно".to_owned());
                }
                texture = Some(
                    args.next()
                        .ok_or_else(|| "после --texture требуется путь в assets".to_owned())?,
                );
            }
            _ => return Err(format!("неизвестный аргумент: {argument}")),
        }
    }
    if scene == Some(Scene::Triangle) && model.is_some() {
        return Err("--scene triangle нельзя сочетать с --model".to_owned());
    }
    if resize_test && frames.is_some_and(|count| count < 120) {
        return Err("с --resize-test значение --frames должно быть не меньше 120".to_owned());
    }
    Ok(Options {
        frames,
        render_scale: render_scale.unwrap_or(1.0),
        resize_test,
        scene: scene.unwrap_or_default(),
        model,
        texture,
        debug_overlay,
    })
}

/// Загружает модель или демонстрационную сцену и заменяет текстуры, если задан `--texture`.
fn load_content(options: &Options, assets: &Assets) -> Result<Content, AssetError> {
    let mut content = match &options.model {
        Some(path) => Content::from_model(assets.load_model(path)?, options.scene),
        None => Content::from_scene(options.scene),
    };
    if let Some(path) = &options.texture {
        content.override_texture(assets.load_texture(path)?);
    }
    Ok(content)
}

#[derive(Default)]
struct ResizeTest {
    initial_requested: bool,
    initial_resized: bool,
    minimize_deadline: Option<Instant>,
    restore_requested: bool,
    restore_size: bool,
    complete: bool,
}

struct Sandbox {
    renderer: Option<Renderer>,
    content: Content,
    meshes: Vec<(MeshId, Option<TextureId>, bool)>,
    size: PhysicalSize<u32>,
    time: Time,
    options: Options,
    presented_frames: u64,
    resize_test: ResizeTest,
    debug_overlay: Option<DebugOverlay>,
    failure: Option<String>,
}

impl Sandbox {
    fn new(options: Options, content: Content) -> Self {
        Self {
            renderer: None,
            content,
            meshes: Vec::new(),
            size: PhysicalSize::new(0, 0),
            time: Time::new(),
            options,
            presented_frames: 0,
            resize_test: ResizeTest::default(),
            debug_overlay: None,
            failure: None,
        }
    }

    fn report_resize(&mut self, width: u32, height: u32) {
        println!("Resized: {width}x{height} (physical pixels)");
        self.size = PhysicalSize::new(width, height);
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.resize(width, height);
        }
        if self.options.resize_test {
            if self.resize_test.initial_requested
                && width == RESIZE_SIZE.width
                && height == RESIZE_SIZE.height
            {
                self.resize_test.initial_resized = true;
            }
            if self.resize_test.restore_requested
                && width == RESTORE_SIZE.width
                && height == RESTORE_SIZE.height
            {
                self.resize_test.restore_size = true;
            }
        }
    }

    fn request_inner_size(&mut self, window: &Window, target: PhysicalSize<u32>) {
        if let Some(actual_size) = window.request_inner_size(target) {
            println!(
                "request_inner_size actual: {}x{} (physical pixels)",
                actual_size.width, actual_size.height
            );
            self.report_resize(actual_size.width, actual_size.height);
            if actual_size != target {
                self.failure = Some(format!(
                    "resize-test: requested {}x{}, actual {}x{}",
                    target.width, target.height, actual_size.width, actual_size.height
                ));
            }
        }
    }

    fn advance_resize_test(&mut self, window: &Window) {
        if !self.options.resize_test || self.failure.is_some() || self.resize_test.complete {
            return;
        }
        if !self.resize_test.initial_requested && self.presented_frames >= 30 {
            self.resize_test.initial_requested = true;
            println!("resize-test: requesting 960x540 after 30 presentations");
            self.request_inner_size(window, RESIZE_SIZE);
        }
        if self.resize_test.initial_resized
            && self.presented_frames >= 60
            && self.resize_test.minimize_deadline.is_none()
        {
            println!("resize-test: minimizing after 60 presentations");
            window.set_minimized(true);
            self.resize_test.minimize_deadline = Some(Instant::now() + Duration::from_millis(250));
        }
        if !self.resize_test.restore_requested
            && self
                .resize_test
                .minimize_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            println!("resize-test: restoring after 250ms");
            window.set_minimized(false);
            self.resize_test.restore_requested = true;
            self.request_inner_size(window, RESTORE_SIZE);
        }
        if self.resize_test.restore_requested
            && self.resize_test.restore_size
            && window.is_minimized() != Some(true)
        {
            self.resize_test.complete = true;
            println!("resize-test: restore complete; actual 1280x720");
        }
    }

    fn upload_content(&mut self, renderer: &mut Renderer) -> Result<(), String> {
        let mut textures = Vec::with_capacity(self.content.textures.len());
        for texture in &self.content.textures {
            textures.push(
                renderer
                    .create_texture(texture.width, texture.height, &texture.rgba)
                    .map_err(|error| format!("ошибка создания текстуры: {error}"))?,
            );
        }
        for mesh in &self.content.meshes {
            let id = renderer
                .create_mesh(&mesh.vertices, &mesh.indices)
                .map_err(|error| format!("ошибка создания меша: {error}"))?;
            self.meshes.push((
                id,
                mesh.texture.map(|index| textures[index]),
                mesh.environment,
            ));
        }
        Ok(())
    }

    fn take_failure(&mut self) -> Option<String> {
        self.failure.take()
    }
}

impl WindowHandler for Sandbox {
    fn init(&mut self, window: Arc<Window>) -> bool {
        let size = window.inner_size();
        println!("Window: {}x{} (physical pixels)", size.width, size.height);
        self.size = size;
        let config = RendererConfig {
            render_scale: self.options.render_scale,
            ..RendererConfig::default()
        };
        match Renderer::new(Arc::clone(&window), size.width, size.height, config) {
            Ok(mut renderer) => {
                let capabilities = renderer.capabilities();
                let api_major = (capabilities.api_version >> 22) & 0x7f;
                let api_minor = (capabilities.api_version >> 12) & 0x3ff;
                println!(
                    "GPU: {} (vendor 0x{:04x}), Vulkan {}.{}, discrete: {}, tessellation: {}",
                    capabilities.device_name,
                    capabilities.vendor_id,
                    api_major,
                    api_minor,
                    capabilities.discrete,
                    capabilities.tessellation_shader
                );
                if let Err(error) = self.upload_content(&mut renderer) {
                    self.failure = Some(error);
                    return false;
                }
                if self.options.debug_overlay {
                    self.debug_overlay = Some(DebugOverlay::new(
                        &renderer.capabilities().device_name,
                        Instant::now(),
                    ));
                }
                self.renderer = Some(renderer);
                true
            }
            Err(error) => {
                self.failure = Some(format!("ошибка рендерера: {error}"));
                false
            }
        }
    }

    fn frame(&mut self, window: &Window) -> bool {
        let _ = self.time.tick();
        if window.is_minimized() == Some(true) {
            return true;
        }
        let elapsed = self.time.elapsed().as_secs_f32();
        let color = [
            0.5 + 0.5 * elapsed.sin(),
            0.5 + 0.5 * (elapsed * 0.73).sin(),
            0.5 + 0.5 * (elapsed * 0.41).sin(),
            1.0,
        ];
        let Some(renderer) = self.renderer.as_mut() else {
            self.failure = Some("рендерер отсутствует".to_owned());
            return false;
        };
        let model = self.content.model(elapsed).to_cols_array_2d();
        let draws = self
            .meshes
            .iter()
            .map(|&(mesh, texture, environment)| DrawCommand {
                mesh,
                texture,
                model: if environment {
                    glam::Mat4::IDENTITY.to_cols_array_2d()
                } else {
                    model
                },
            })
            .collect::<Vec<_>>();
        let gpu_time = renderer.take_gpu_frame_time();
        let overlay = match self.debug_overlay.as_mut() {
            Some(overlay) => overlay.update(Instant::now(), gpu_time),
            None => &[],
        };
        let frame = Frame {
            clear_color: color,
            view_projection: self
                .content
                .view_projection(elapsed, self.size.width, self.size.height)
                .to_cols_array_2d(),
            shadow_view_projection: self
                .content
                .shadow_view_projection(elapsed)
                .map(|matrix| matrix.to_cols_array_2d()),
            light: self.content.light(elapsed),
            draws: &draws,
            overlay,
        };
        match renderer.render_frame(&frame) {
            Ok(FrameStatus::Presented) => {
                self.presented_frames += 1;
                if let Some(overlay) = self.debug_overlay.as_mut() {
                    overlay.frame_presented();
                }
                self.advance_resize_test(window);
                if self
                    .options
                    .frames
                    .is_some_and(|limit| self.presented_frames >= limit)
                {
                    if self.options.resize_test && !self.resize_test.complete {
                        self.failure = Some(format!(
                            "resize-test не завершился за {} успешных презентаций",
                            self.presented_frames
                        ));
                        return false;
                    }
                    return false;
                }
                true
            }
            Ok(FrameStatus::Skipped) => true,
            Err(error) => {
                self.failure = Some(format!("ошибка Vulkan render_frame: {error}"));
                false
            }
        }
    }

    fn resized(&mut self, width: u32, height: u32) {
        self.report_resize(width, height);
    }

    fn update(&mut self, window: &Window) -> bool {
        self.advance_resize_test(window);
        if self.failure.is_some() {
            return false;
        }
        true
    }

    fn exiting(&mut self) {
        self.renderer.take();
    }
}

fn main() -> ExitCode {
    let options = match parse_args(env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("Ошибка аргументов: {error}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let bootstrap = match BootstrapConfig::load("bootstrap.rves") {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Ошибка настроек: {error}");
            return ExitCode::from(2);
        }
    };
    let default_window = WindowConfig::default();
    let config = WindowConfig {
        title: bootstrap.window_title.unwrap_or(default_window.title),
        width: bootstrap.window_size_x.unwrap_or(default_window.width),
        height: bootstrap.window_size_y.unwrap_or(default_window.height),
        ..WindowConfig::default()
    };
    let mut resources = match ResourceSystem::start(ResourceConfig {
        root: ASSETS_DIR.into(),
        ..ResourceConfig::default()
    }) {
        Ok(resources) => resources,
        Err(error) => {
            eprintln!("Ошибка ресурса: {error}");
            return ExitCode::from(2);
        }
    };
    let assets = Assets::with_resources(resources.handle());
    let content = match load_content(&options, &assets) {
        Ok(content) => content,
        Err(error) => {
            eprintln!("Ошибка ресурса: {error}");
            return ExitCode::from(2);
        }
    };
    let mut app = Sandbox::new(options, content);
    let run_result = run(config, &mut app);
    let failure = app.take_failure();
    let mut exit_code = match (run_result, failure) {
        (Err(error), _) => {
            eprintln!("ошибка окна: {error}");
            1
        }
        (Ok(()), Some(error)) => {
            eprintln!("{error}");
            1
        }
        (Ok(()), None) => 0,
    };
    drop(app);
    if let Err(error) = resources.shutdown() {
        eprintln!("Ошибка завершения ресурсного сервиса: {error}");
        if exit_code == 0 {
            exit_code = 1;
        }
    }
    ExitCode::from(exit_code)
}

#[cfg(test)]
mod tests {
    use super::{Content, Options, RESIZE_SIZE, RESTORE_SIZE, Sandbox, Scene, parse_args};
    use engine_assets::Assets;
    use engine_resources::{ResourceConfig, ResourceSystem};

    fn sandbox(options: Options) -> Sandbox {
        let content = Content::from_scene(options.scene);
        Sandbox::new(options, content)
    }

    #[test]
    fn procedural_content_loads_when_the_assets_root_is_missing() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rve-sandbox-missing-assets-{}-{nonce}",
            std::process::id()
        ));
        let mut resources = ResourceSystem::start(ResourceConfig {
            root,
            ..ResourceConfig::default()
        })
        .unwrap();
        let assets = Assets::with_resources(resources.handle());
        let options = parse_args(["--scene", "triangle"]).unwrap();
        let content = super::load_content(&options, &assets).unwrap();
        assert!(!content.meshes.is_empty());
        resources.shutdown().unwrap();
    }

    #[test]
    fn frame_limit_accepts_u64_boundaries() {
        for limit in [1, u64::MAX] {
            let options = parse_args(["--frames".to_owned(), limit.to_string()]).unwrap();
            assert_eq!(options.frames, Some(limit));
        }
    }

    #[test]
    fn scale_accepts_small_positive_values_and_native_resolution() {
        for scale in [f32::MIN_POSITIVE, 1.0] {
            let options = parse_args(["--render-scale".to_owned(), scale.to_string()]).unwrap();
            assert_eq!(options.render_scale, scale);
        }
    }

    #[test]
    fn resize_test_accepts_unbounded_run_and_flags_in_any_order() {
        let options = parse_args(["--resize-test"]).unwrap();
        assert!(options.resize_test);
        assert_eq!(options.frames, None);

        let options =
            parse_args(["--resize-test", "--render-scale", "1", "--frames", "120"]).unwrap();
        assert!(options.resize_test);
        assert_eq!(options.frames, Some(120));
        assert_eq!(options.render_scale, 1.0);
    }

    #[test]
    fn rejects_negative_overflowing_and_malformed_values() {
        for arguments in [
            vec!["--frames", "-1"],
            vec!["--frames", "18446744073709551616"],
            vec!["--frames", "1.5"],
            vec!["--frames", "--resize-test"],
            vec!["--render-scale", "abc"],
            vec!["--render-scale", "-0.5"],
            vec!["--render-scale", "-inf"],
            vec!["--render-scale", "--frames", "120"],
            vec!["--resize-test", "--frames", "119"],
            vec!["120"],
        ] {
            assert!(parse_args(arguments.clone()).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn resize_progress_requires_requested_and_matching_physical_sizes() {
        let mut sandbox = sandbox(Options {
            frames: None,
            render_scale: 1.0,
            resize_test: true,
            scene: Scene::Cube,
            model: None,
            texture: None,
            debug_overlay: false,
        });
        sandbox.report_resize(RESIZE_SIZE.width, RESIZE_SIZE.height);
        sandbox.report_resize(RESTORE_SIZE.width, RESTORE_SIZE.height);
        assert!(!sandbox.resize_test.initial_resized);
        assert!(!sandbox.resize_test.restore_size);

        sandbox.resize_test.initial_requested = true;
        sandbox.resize_test.restore_requested = true;
        sandbox.report_resize(0, 0);
        sandbox.report_resize(RESIZE_SIZE.width, RESIZE_SIZE.height + 1);
        sandbox.report_resize(RESTORE_SIZE.width + 1, RESTORE_SIZE.height);
        assert!(!sandbox.resize_test.initial_resized);
        assert!(!sandbox.resize_test.restore_size);

        sandbox.report_resize(RESIZE_SIZE.width, RESIZE_SIZE.height);
        assert!(sandbox.resize_test.initial_resized);
        assert!(!sandbox.resize_test.restore_size);
        sandbox.report_resize(RESTORE_SIZE.width, RESTORE_SIZE.height);
        assert!(sandbox.resize_test.restore_size);
        assert!(!sandbox.resize_test.complete);
    }

    #[test]
    fn normal_resize_does_not_advance_disabled_resize_test() {
        let mut sandbox = sandbox(parse_args(Vec::<String>::new()).unwrap());
        sandbox.resize_test.initial_requested = true;
        sandbox.resize_test.restore_requested = true;

        sandbox.report_resize(RESIZE_SIZE.width, RESIZE_SIZE.height);
        sandbox.report_resize(RESTORE_SIZE.width, RESTORE_SIZE.height);
        assert!(!sandbox.resize_test.initial_resized);
        assert!(!sandbox.resize_test.restore_size);
    }

    #[test]
    fn failure_can_be_taken_only_once_after_the_event_loop() {
        let mut sandbox = sandbox(parse_args(Vec::<String>::new()).unwrap());
        assert_eq!(sandbox.take_failure(), None);
        sandbox.failure = Some("render failed".to_owned());
        assert_eq!(sandbox.take_failure().as_deref(), Some("render failed"));
        assert_eq!(sandbox.take_failure(), None);
    }

    #[test]
    fn parses_all_supported_flags() {
        let options = parse_args([
            "--frames",
            "120",
            "--render-scale",
            "0.5",
            "--resize-test",
            "--scene",
            "triangle",
            "--texture",
            "/texture/paint.png",
            "--debug-overlay",
        ])
        .expect("valid arguments should parse");
        assert_eq!(options.frames, Some(120));
        assert_eq!(options.render_scale, 0.5);
        assert!(options.resize_test);
        assert_eq!(options.scene, Scene::Triangle);
        assert_eq!(options.model, None);
        assert_eq!(options.texture.as_deref(), Some("/texture/paint.png"));
        assert!(options.debug_overlay);

        let options = parse_args(["--model", "/model/cube.fbx"]).unwrap();
        assert_eq!(options.model.as_deref(), Some("/model/cube.fbx"));
        assert_eq!(options.texture, None);
        for scene in ["shadows", "cube"] {
            let options = parse_args(["--scene", scene, "--model", "/model/cube.fbx"])
                .expect("cube and shadow scenes may load a model");
            assert_eq!(options.scene, Scene::parse(scene).unwrap());
            assert_eq!(options.model.as_deref(), Some("/model/cube.fbx"));
        }
        assert!(parse_args(["--scene", "triangle", "--model", "/model/cube.fbx"]).is_err());
        assert_eq!(
            parse_args(Vec::<String>::new()).unwrap().scene,
            Scene::Shadows
        );
    }

    #[test]
    fn rejects_invalid_or_repeated_flags() {
        for arguments in [
            vec!["--unknown"],
            vec!["--frames"],
            vec!["--frames", "abc"],
            vec!["--frames", "0"],
            vec!["--frames", "2", "--frames", "3"],
            vec!["--render-scale"],
            vec!["--render-scale", "NaN"],
            vec!["--render-scale", "inf"],
            vec!["--render-scale", "0"],
            vec!["--render-scale", "2"],
            vec!["--render-scale", "0.5", "--render-scale", "0.25"],
            vec!["--resize-test", "--resize-test"],
            vec!["--frames", "119", "--resize-test"],
            vec!["--scene"],
            vec!["--scene", "sphere"],
            vec!["--scene", "cube", "--scene", "triangle"],
            vec!["--model"],
            vec!["--model", "/a.fbx", "--model", "/b.fbx"],
            vec!["--texture"],
            vec!["--texture", "/a.png", "--texture", "/b.png"],
            vec!["--debug-overlay", "--debug-overlay"],
            vec!["--debug-overlay", "1"],
            vec!["--model", "/model/cube.fbx", "--scene", "triangle"],
        ] {
            assert!(parse_args(arguments).is_err());
        }
    }

    #[test]
    fn defaults_to_unbounded_run_at_native_scale() {
        let options = parse_args(Vec::<String>::new()).expect("empty args are valid");
        assert_eq!(options.frames, None);
        assert_eq!(options.render_scale, 1.0);
        assert!(!options.resize_test);
        assert_eq!(options.scene, Scene::Shadows);
        assert_eq!(options.model, None);
        assert_eq!(options.texture, None);
        assert!(!options.debug_overlay);
    }
}
