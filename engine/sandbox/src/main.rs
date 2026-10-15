mod bootstrap;
mod scene;

use engine_core::Time;
use engine_render::{
    DirectionalLight, DrawCommand, Frame, FrameStatus, MeshId, Renderer, RendererConfig, TextureId,
};
use engine_window::{PhysicalSize, Window, WindowHandler, run};
use scene::Scene;
use std::env;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "Использование: sandbox [--frames N] [--render-scale S] [--resize-test] [--scene triangle|cube]";
const RESIZE_SIZE: PhysicalSize<u32> = PhysicalSize::new(960, 540);
const RESTORE_SIZE: PhysicalSize<u32> = PhysicalSize::new(1280, 720);

struct Options {
    frames: Option<u64>,
    render_scale: f32,
    resize_test: bool,
    scene: Scene,
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
            "--scene" => {
                if scene.is_some() {
                    return Err("флаг --scene указан повторно".to_owned());
                }
                let value = args
                    .next()
                    .ok_or_else(|| "после --scene требуется triangle или cube".to_owned())?;
                scene = Some(Scene::parse(&value).ok_or_else(|| {
                    format!("неизвестная сцена: {value}; доступны triangle и cube")
                })?);
            }
            _ => return Err(format!("неизвестный аргумент: {argument}")),
        }
    }
    if resize_test && frames.is_some_and(|count| count < 120) {
        return Err("с --resize-test значение --frames должно быть не меньше 120".to_owned());
    }
    Ok(Options {
        frames,
        render_scale: render_scale.unwrap_or(1.0),
        resize_test,
        scene: scene.unwrap_or_default(),
    })
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
    mesh: Option<MeshId>,
    texture: Option<TextureId>,
    size: PhysicalSize<u32>,
    time: Time,
    options: Options,
    presented_frames: u64,
    resize_test: ResizeTest,
    failure: Option<String>,
}

impl Sandbox {
    fn new(options: Options) -> Self {
        Self {
            renderer: None,
            mesh: None,
            texture: None,
            size: PhysicalSize::new(0, 0),
            time: Time::new(),
            options,
            presented_frames: 0,
            resize_test: ResizeTest::default(),
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
                let (vertices, indices) = self.options.scene.mesh();
                match renderer.create_mesh(&vertices, &indices) {
                    Ok(mesh) => self.mesh = Some(mesh),
                    Err(error) => {
                        self.failure = Some(format!("ошибка создания меша: {error}"));
                        return false;
                    }
                }
                if let Some(texture) = self.options.scene.texture() {
                    match renderer.create_texture(texture.width, texture.height, &texture.rgba) {
                        Ok(texture) => self.texture = Some(texture),
                        Err(error) => {
                            self.failure = Some(format!("ошибка создания текстуры: {error}"));
                            return false;
                        }
                    }
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
        let (Some(renderer), Some(mesh)) = (self.renderer.as_mut(), self.mesh) else {
            self.failure = Some("рендерер отсутствует".to_owned());
            return false;
        };
        let scene = self.options.scene;
        let draws = [DrawCommand {
            mesh,
            texture: self.texture,
            model: scene.model(elapsed).to_cols_array_2d(),
        }];
        let frame = Frame {
            clear_color: color,
            view_projection: scene
                .view_projection(self.size.width, self.size.height)
                .to_cols_array_2d(),
            light: DirectionalLight::default(),
            draws: &draws,
        };
        match renderer.render_frame(&frame) {
            Ok(FrameStatus::Presented) => {
                self.presented_frames += 1;
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
    let config = match bootstrap::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Ошибка настроек: {error}");
            return ExitCode::from(2);
        }
    };
    let mut app = Sandbox::new(options);
    let run_result = run(config, &mut app);
    let failure = app.take_failure();
    let exit_code = match (run_result, failure) {
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
    ExitCode::from(exit_code)
}

#[cfg(test)]
mod tests {
    use super::{Options, RESIZE_SIZE, RESTORE_SIZE, Sandbox, Scene, parse_args};

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
        let mut sandbox = Sandbox::new(Options {
            frames: None,
            render_scale: 1.0,
            resize_test: true,
            scene: Scene::Cube,
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
        let mut sandbox = Sandbox::new(parse_args(Vec::<String>::new()).unwrap());
        sandbox.resize_test.initial_requested = true;
        sandbox.resize_test.restore_requested = true;

        sandbox.report_resize(RESIZE_SIZE.width, RESIZE_SIZE.height);
        sandbox.report_resize(RESTORE_SIZE.width, RESTORE_SIZE.height);
        assert!(!sandbox.resize_test.initial_resized);
        assert!(!sandbox.resize_test.restore_size);
    }

    #[test]
    fn failure_can_be_taken_only_once_after_the_event_loop() {
        let mut sandbox = Sandbox::new(parse_args(Vec::<String>::new()).unwrap());
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
        ])
        .expect("valid arguments should parse");
        assert_eq!(options.frames, Some(120));
        assert_eq!(options.render_scale, 0.5);
        assert!(options.resize_test);
        assert_eq!(options.scene, Scene::Triangle);
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
        assert_eq!(options.scene, Scene::Cube);
    }
}
