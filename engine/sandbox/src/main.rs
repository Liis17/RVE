mod content;
mod debug_overlay;
mod scene;

use content::Content;
use debug_overlay::DebugOverlay;
use engine_assets::{ASSETS_DIR, AssetError, Assets, ModelAlphaMode};
use engine_core::Time;
use engine_files::VirtualPath;
use engine_project::{BootstrapConfig, BootstrapPresentMode};
use engine_render::{
    AlphaMode, AntiAliasing as RenderAntiAliasing, DebugView, DrawCommand, EnvironmentId,
    EnvironmentSource, Frame, FrameCamera, FrameEnvironment, FrameStatus, Material, MaterialId,
    MeshId, ParticleBlend, ParticleCollision, ParticleEmitterDesc, ParticleEmitterFrame,
    ParticleShape, ProceduralSky, Renderer, RendererConfig, TextureColorSpace, TextureId,
    Tonemapper as RenderTonemapper, ViewSettings,
};
use engine_resources::{ResourceConfig, ResourceHandle, ResourceSystem};
use engine_scene::{AntiAliasing, CameraSettings, Tonemapper};
use engine_window::{PhysicalSize, Window, WindowConfig, WindowHandler, run};
use glam::{Mat4, Vec3};
use scene::Scene;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "Использование: sandbox [--frames N] [--render-scale S] [--camera-settings /путь.camerasettings] [--resize-test] [--scene triangle|cube|shadows|materials|clustered|particles] [--model /путь.fbx] [--texture /путь.png] [--environment procedural|/путь.hdr] [--debug-overlay]";
const RESIZE_SIZE: PhysicalSize<u32> = PhysicalSize::new(960, 540);
const RESTORE_SIZE: PhysicalSize<u32> = PhysicalSize::new(1280, 720);

struct Options {
    frames: Option<u64>,
    render_scale: Option<f32>,
    camera_settings: Option<String>,
    resize_test: bool,
    scene: Scene,
    /// Путь модели внутри `assets`, например `/model/cube.fbx`.
    model: Option<String>,
    /// Путь PNG внутри `assets`, заменяющий base-color texture сцены или модели.
    texture: Option<String>,
    environment: Option<EnvironmentMode>,
    /// Показывать панель FPS, памяти и загрузки CPU/GPU.
    debug_overlay: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EnvironmentMode {
    Procedural,
    Hdr(String),
}

fn parse_args<I, S>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let mut frames = None;
    let mut render_scale = None;
    let mut camera_settings = None;
    let mut resize_test = false;
    let mut scene = None;
    let mut model = None;
    let mut texture = None;
    let mut environment = None;
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
            "--camera-settings" => {
                if camera_settings.is_some() {
                    return Err("флаг --camera-settings указан повторно".to_owned());
                }
                let value = args.next().ok_or_else(|| {
                    "после --camera-settings требуется путь к .camerasettings".to_owned()
                })?;
                let path = VirtualPath::parse(&value)
                    .map_err(|error| format!("некорректный путь --camera-settings: {error}"))?;
                let is_camera_settings = path
                    .as_str()
                    .rsplit('/')
                    .next()
                    .and_then(|name| name.rsplit_once('.'))
                    .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("camerasettings"));
                if !is_camera_settings {
                    return Err("--camera-settings должен указывать на .camerasettings".to_owned());
                }
                camera_settings = Some(path.as_str().to_owned());
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
                    "после --scene требуется triangle, cube, shadows, materials, clustered или particles"
                        .to_owned()
                })?;
                scene = Some(Scene::parse(&value).ok_or_else(|| {
                    format!(
                        "неизвестная сцена: {value}; доступны triangle, cube, shadows, materials и clustered"
                    )
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
            "--environment" => {
                if environment.is_some() {
                    return Err("флаг --environment указан повторно".to_owned());
                }
                let value = args.next().ok_or_else(|| {
                    "после --environment требуется procedural или путь .hdr".to_owned()
                })?;
                environment = Some(if value == "procedural" {
                    EnvironmentMode::Procedural
                } else {
                    let path = VirtualPath::parse(&value)
                        .map_err(|error| format!("некорректный путь --environment: {error}"))?;
                    let is_hdr = path
                        .as_str()
                        .rsplit('/')
                        .next()
                        .and_then(|name| name.rsplit_once('.'))
                        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("hdr"));
                    if !is_hdr {
                        return Err(
                            "--environment должен указывать на procedural или .hdr".to_owned()
                        );
                    }
                    EnvironmentMode::Hdr(path.as_str().to_owned())
                });
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
        render_scale,
        camera_settings,
        resize_test,
        scene: scene.unwrap_or_default(),
        model,
        texture,
        environment,
        debug_overlay,
    })
}

fn load_camera_settings(
    options: &Options,
    resources: &ResourceHandle,
) -> Result<CameraSettings, String> {
    if let Some(path) = &options.camera_settings {
        let bytes = resources
            .read_all(path)
            .map_err(|error| format!("{path}: {error}"))?
            .wait()
            .map_err(|error| format!("{path}: {error}"))?;
        let source = String::from_utf8(bytes)
            .map_err(|error| format!("{path}: файл должен быть UTF-8: {error}"))?;
        CameraSettings::parse(path, &source).map_err(|error| error.to_string())
    } else {
        Ok(CameraSettings::default())
    }
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

fn load_environment(
    mode: Option<&EnvironmentMode>,
    assets: &Assets,
    content: &Content,
) -> Result<Option<EnvironmentSource>, String> {
    match mode {
        None => Ok(None),
        Some(EnvironmentMode::Hdr(path)) => {
            let image = assets
                .load_hdr(path)
                .map_err(|error| format!("{path}: {error}"))?;
            Ok(Some(EnvironmentSource::Equirect {
                width: image.width,
                height: image.height,
                rgb: image.rgb,
            }))
        }
        Some(EnvironmentMode::Procedural) => {
            let light = content.light(0.0);
            let sun_direction = -Vec3::from_array(light.direction).normalize_or_zero();
            let sun_direction = if sun_direction.length_squared() > 1e-8 {
                sun_direction.to_array()
            } else {
                Vec3::Y.to_array()
            };
            Ok(Some(EnvironmentSource::Procedural(ProceduralSky {
                zenith_color: [0.2, 0.35, 0.65],
                horizon_color: [0.65, 0.7, 0.75],
                ground_color: [0.12, 0.11, 0.1],
                sun_size_deg: 1.0,
                sun_intensity: 1.0,
                sun_direction,
            })))
        }
    }
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

/// Процедурный аддитивный фонтан над кубом для ручной проверки путя частиц.
fn create_particle_emitters(renderer: &mut Renderer) -> Result<Vec<ParticleEmitterFrame>, String> {
    let fountain = ParticleEmitterDesc {
        capacity: 8192,
        blend: ParticleBlend::Additive,
        shape: ParticleShape::Cone {
            angle: 0.35,
            radius: 0.05,
        },
        rate: 900.0,
        lifetime: [1.2, 2.2],
        speed: [2.5, 3.5],
        size: [0.12, 0.2],
        gravity: [0.0, -3.0, 0.0],
        drag: 0.3,
        collision: Some(ParticleCollision {
            restitution: 0.4,
            friction: 0.2,
            thickness: 0.05,
        }),
        ..ParticleEmitterDesc::default()
    };
    let smoke = ParticleEmitterDesc {
        capacity: 2048,
        blend: ParticleBlend::Alpha,
        shape: ParticleShape::Box {
            half_extents: [0.4, 0.1, 0.4],
        },
        rate: 120.0,
        lifetime: [2.0, 3.0],
        speed: [0.3, 0.6],
        size: [0.6, 0.9],
        gravity: [0.0, 0.4, 0.0],
        drag: 0.5,
        soft_distance: 0.5,
        ..ParticleEmitterDesc::default()
    };
    [
        (fountain, Vec3::new(0.0, 1.0, 0.0)),
        (smoke, Vec3::new(-1.6, 0.5, 0.0)),
    ]
    .into_iter()
    .map(|(desc, position)| {
        let emitter = renderer
            .create_particle_emitter(desc)
            .map_err(|error| format!("ошибка создания эмиттера частиц: {error}"))?;
        Ok(ParticleEmitterFrame {
            emitter,
            transform: Mat4::from_translation(position).to_cols_array_2d(),
            emitting: true,
        })
    })
    .collect()
}

struct Sandbox {
    renderer: Option<Renderer>,
    content: Content,
    environment_source: Option<EnvironmentSource>,
    environment: Option<EnvironmentId>,
    camera_settings: CameraSettings,
    present_mode: Option<BootstrapPresentMode>,
    pipeline_cache_path: Option<PathBuf>,
    pipeline_cache: Vec<u8>,
    meshes: Vec<(MeshId, MaterialId, bool)>,
    size: PhysicalSize<u32>,
    time: Time,
    previous_elapsed: Option<f32>,
    options: Options,
    presented_frames: u64,
    resize_test: ResizeTest,
    debug_overlay: Option<DebugOverlay>,
    particle_emitters: Vec<ParticleEmitterFrame>,
    failure: Option<String>,
}

impl Sandbox {
    fn new(
        options: Options,
        content: Content,
        mut camera_settings: CameraSettings,
        environment_source: Option<EnvironmentSource>,
        present_mode: Option<BootstrapPresentMode>,
        pipeline_cache_path: Option<PathBuf>,
        pipeline_cache: Vec<u8>,
    ) -> Self {
        if let Some(render_scale) = options.render_scale {
            camera_settings.render.render_scale = render_scale;
        }
        Self {
            renderer: None,
            content,
            environment_source,
            environment: None,
            camera_settings,
            present_mode,
            pipeline_cache_path,
            pipeline_cache,
            meshes: Vec::new(),
            size: PhysicalSize::new(0, 0),
            time: Time::new(),
            previous_elapsed: None,
            options,
            presented_frames: 0,
            resize_test: ResizeTest::default(),
            debug_overlay: None,
            particle_emitters: Vec::new(),
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
        let mut texture_cache = HashMap::new();
        for mesh in &self.content.meshes {
            let id = renderer
                .create_mesh(&mesh.vertices, &mesh.indices)
                .map_err(|error| format!("ошибка создания меша: {error}"))?;
            let base_color_texture = mesh
                .material
                .base_color_texture
                .map(|index| {
                    upload_content_texture(
                        renderer,
                        &self.content.textures,
                        &mut texture_cache,
                        index,
                        TextureColorSpace::Srgb,
                    )
                })
                .transpose()?;
            let metallic_roughness_texture = mesh
                .material
                .metallic_roughness_texture
                .map(|index| {
                    upload_content_texture(
                        renderer,
                        &self.content.textures,
                        &mut texture_cache,
                        index,
                        TextureColorSpace::Linear,
                    )
                })
                .transpose()?;
            let normal_texture = mesh
                .material
                .normal_texture
                .map(|index| {
                    upload_content_texture(
                        renderer,
                        &self.content.textures,
                        &mut texture_cache,
                        index,
                        TextureColorSpace::Linear,
                    )
                })
                .transpose()?;
            let emissive_texture = mesh
                .material
                .emissive_texture
                .map(|index| {
                    upload_content_texture(
                        renderer,
                        &self.content.textures,
                        &mut texture_cache,
                        index,
                        TextureColorSpace::Srgb,
                    )
                })
                .transpose()?;
            let occlusion_texture = mesh
                .material
                .occlusion_texture
                .map(|index| {
                    upload_content_texture(
                        renderer,
                        &self.content.textures,
                        &mut texture_cache,
                        index,
                        TextureColorSpace::Linear,
                    )
                })
                .transpose()?;
            let material_id = renderer
                .create_material(&Material {
                    base_color: mesh.material.base_color,
                    metallic: mesh.material.metallic,
                    roughness: mesh.material.roughness,
                    emissive: mesh.material.emissive,
                    opacity: mesh.material.opacity,
                    alpha_mode: match mesh.material.alpha_mode {
                        ModelAlphaMode::Opaque => AlphaMode::Opaque,
                        ModelAlphaMode::Blend => AlphaMode::Blend,
                    },
                    double_sided: mesh.material.double_sided,
                    base_color_texture,
                    metallic_roughness_texture,
                    normal_texture,
                    emissive_texture,
                    occlusion_texture,
                    normal_scale: mesh.material.normal_scale,
                    occlusion_strength: mesh.material.occlusion_strength,
                })
                .map_err(|error| format!("ошибка создания материала: {error}"))?;
            self.meshes.push((id, material_id, mesh.environment));
        }
        Ok(())
    }

    fn take_failure(&mut self) -> Option<String> {
        self.failure.take()
    }

    fn save_pipeline_cache(&self) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        match renderer.pipeline_cache_data() {
            Ok(data) => {
                engine_platform::save_pipeline_cache(self.pipeline_cache_path.as_deref(), &data)
            }
            Err(error) => eprintln!("failed to retrieve pipeline cache: {error}"),
        }
    }
}

impl WindowHandler for Sandbox {
    fn init(&mut self, window: Arc<Window>) -> bool {
        let size = window.inner_size();
        println!("Window: {}x{} (physical pixels)", size.width, size.height);
        self.size = size;
        let config = RendererConfig {
            present_mode: self
                .present_mode
                .map(|mode| match mode {
                    BootstrapPresentMode::Fifo => engine_render::PresentMode::Fifo,
                    BootstrapPresentMode::Mailbox => engine_render::PresentMode::Mailbox,
                    BootstrapPresentMode::Immediate => engine_render::PresentMode::Immediate,
                })
                .unwrap_or_default(),
            pipeline_cache: self.pipeline_cache.clone(),
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
                if self.content.motion == Scene::Particles {
                    match create_particle_emitters(&mut renderer) {
                        Ok(emitters) => self.particle_emitters = emitters,
                        Err(error) => {
                            self.failure = Some(error);
                            return false;
                        }
                    }
                }
                if let Err(error) = self.upload_content(&mut renderer) {
                    self.failure = Some(error);
                    return false;
                }
                if let Some(source) = &self.environment_source {
                    match renderer.create_environment(source) {
                        Ok(environment) => self.environment = Some(environment),
                        Err(error) => {
                            self.failure = Some(format!("ошибка создания environment: {error}"));
                            return false;
                        }
                    }
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
        let Some(renderer) = self.renderer.as_mut() else {
            self.failure = Some("рендерер отсутствует".to_owned());
            return false;
        };
        let model = self.content.model(elapsed).to_cols_array_2d();
        let previous_model = self
            .content
            .model(self.previous_elapsed.unwrap_or(elapsed))
            .to_cols_array_2d();
        let draws = self
            .meshes
            .iter()
            .map(|&(mesh, material, environment)| DrawCommand {
                mesh,
                material: Some(material),
                model: if environment {
                    glam::Mat4::IDENTITY.to_cols_array_2d()
                } else {
                    model
                },
                previous_model: Some(if environment {
                    glam::Mat4::IDENTITY.to_cols_array_2d()
                } else {
                    previous_model
                }),
                lod_level: 0,
                casts_shadow: true,
                skin: None,
            })
            .collect::<Vec<_>>();
        let gpu_time = renderer.take_gpu_frame_time();
        let frame_profile = renderer.take_frame_profile();
        let overlay = match self.debug_overlay.as_mut() {
            Some(overlay) => {
                if let Some(profile) = &frame_profile {
                    overlay.record_frame_profile(profile);
                }
                overlay.update(Instant::now(), gpu_time)
            }
            None => &[],
        };
        let shadow_cascades = self.content.shadow_cascades(
            elapsed,
            self.size.width,
            self.size.height,
            self.camera_settings,
        );
        let view_projection = self.content.view_projection(
            elapsed,
            self.size.width,
            self.size.height,
            self.camera_settings,
        );
        let (view, projection) = self
            .content
            .motion
            .camera_at(elapsed, self.camera_settings)
            .map(|camera| {
                (
                    camera.view(),
                    camera.projection(self.size.width, self.size.height),
                )
            })
            .unwrap_or_else(|| {
                if self.content.motion == Scene::Cube {
                    let view = Mat4::from_translation(Vec3::new(0.0, 0.0, -3.0));
                    (view, view_projection * view.inverse())
                } else {
                    (Mat4::IDENTITY, view_projection)
                }
            });
        let local_lights = self.content.local_lights();
        let local_light_shadows = self.content.local_light_shadows();
        let particle_step = self
            .previous_elapsed
            .map_or(0.0, |previous| (elapsed - previous).max(0.0));
        let frame = Frame {
            joint_palettes: &[],
            clear_color: [
                self.camera_settings.render.clear_color[0],
                self.camera_settings.render.clear_color[1],
                self.camera_settings.render.clear_color[2],
                1.0,
            ],
            camera: FrameCamera {
                view: view.to_cols_array_2d(),
                projection: projection.to_cols_array_2d(),
                position: self.content.motion.camera_position(elapsed),
            },
            environment: self.environment.map(|id| FrameEnvironment {
                id,
                intensity: 1.0,
                rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                skybox: true,
            }),
            shadow_cascades: &shadow_cascades,
            light: self.content.light(elapsed),
            lights: &local_lights,
            local_light_shadows: &local_light_shadows,
            draws: &draws,
            overlay,
            debug_view: DebugView::Lit,
            view: ViewSettings {
                render_scale: self.camera_settings.render.render_scale,
                exposure_ev: self.camera_settings.color.exposure_ev,
                tonemapper: match self.camera_settings.color.tonemapper {
                    Tonemapper::Aces => RenderTonemapper::Aces,
                    Tonemapper::Neutral => RenderTonemapper::Neutral,
                    Tonemapper::None => RenderTonemapper::None,
                },
                anti_aliasing: match self.camera_settings.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: false,
            particles: &self.particle_emitters,
            particle_step,
        };
        match renderer.render_frame(&frame) {
            Ok(FrameStatus::Presented) => {
                self.previous_elapsed = Some(elapsed);
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

fn upload_content_texture(
    renderer: &mut Renderer,
    textures: &[engine_assets::TextureData],
    cache: &mut HashMap<(usize, TextureColorSpace), TextureId>,
    index: usize,
    color_space: TextureColorSpace,
) -> Result<TextureId, String> {
    if let Some(&texture_id) = cache.get(&(index, color_space)) {
        return Ok(texture_id);
    }
    let texture = textures
        .get(index)
        .ok_or_else(|| format!("материал ссылается на отсутствующую текстуру {index}"))?;
    let texture_id = renderer
        .create_texture_with_color_space(texture.width, texture.height, &texture.rgba, color_space)
        .map_err(|error| format!("ошибка создания текстуры: {error}"))?;
    cache.insert((index, color_space), texture_id);
    Ok(texture_id)
}

fn main() -> ExitCode {
    with_executable_directory(std::env::current_exe(), |exe_dir| {
        // SAFETY: called before parsing, resource startup, or any other thread creation.
        unsafe { engine_platform::prepare_vulkan_environment(exe_dir) };
    });
    main_after_vulkan_environment()
}

fn with_executable_directory(
    executable: Result<PathBuf, std::io::Error>,
    prepare: impl FnOnce(&Path),
) {
    let Some(exe_dir) = executable
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
        .and_then(|path| path.parent().map(Path::to_path_buf))
    else {
        return;
    };
    prepare(&exe_dir);
}

fn main_after_vulkan_environment() -> ExitCode {
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
    let present_mode = bootstrap.present_mode;
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
    let camera_settings = match load_camera_settings(&options, &resources.handle()) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("Ошибка настроек камеры: {error}");
            return ExitCode::from(2);
        }
    };
    let environment_source = match load_environment(options.environment.as_ref(), &assets, &content)
    {
        Ok(environment) => environment,
        Err(error) => {
            eprintln!("Ошибка environment: {error}");
            return ExitCode::from(2);
        }
    };
    let pipeline_cache_path = engine_platform::pipeline_cache_path("sandbox");
    let pipeline_cache = engine_platform::load_pipeline_cache(pipeline_cache_path.as_deref());
    let mut app = Sandbox::new(
        options,
        content,
        camera_settings,
        environment_source,
        present_mode,
        pipeline_cache_path,
        pipeline_cache,
    );
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
    app.save_pipeline_cache();
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
    use super::{
        CameraSettings, Content, EnvironmentMode, Options, RESIZE_SIZE, RESTORE_SIZE, Sandbox,
        Scene, parse_args, with_executable_directory,
    };
    use engine_assets::Assets;
    use engine_resources::{ResourceConfig, ResourceSystem};

    fn sandbox(options: Options) -> Sandbox {
        let content = Content::from_scene(options.scene);
        Sandbox::new(
            options,
            content,
            CameraSettings::default(),
            None,
            None,
            None,
            Vec::new(),
        )
    }

    #[test]
    fn missing_current_exe_skips_vulkan_environment_preparation() {
        let mut prepared = false;
        with_executable_directory(Err(std::io::Error::other("current_exe failed")), |_| {
            prepared = true
        });
        assert!(!prepared);
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
            assert_eq!(options.render_scale, Some(scale));
        }
    }

    #[test]
    fn explicit_render_scale_overrides_the_camera_asset_scale() {
        let options = parse_args(["--render-scale", "0.4"]).unwrap();
        let mut settings = CameraSettings::default();
        settings.render.render_scale = 0.75;
        let sandbox = Sandbox::new(
            options,
            Content::from_scene(Scene::Cube),
            settings,
            None,
            None,
            None,
            Vec::new(),
        );
        assert_eq!(sandbox.camera_settings.render.render_scale, 0.4);
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
        assert_eq!(options.render_scale, Some(1.0));
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
            render_scale: Some(1.0),
            camera_settings: None,
            resize_test: true,
            scene: Scene::Cube,
            model: None,
            texture: None,
            environment: None,
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
            "--camera-settings",
            "/camera/default.camerasettings",
            "--resize-test",
            "--scene",
            "triangle",
            "--texture",
            "/texture/paint.png",
            "--debug-overlay",
        ])
        .expect("valid arguments should parse");
        assert_eq!(options.frames, Some(120));
        assert_eq!(options.render_scale, Some(0.5));
        assert_eq!(
            options.camera_settings.as_deref(),
            Some("camera/default.camerasettings")
        );
        assert!(options.resize_test);
        assert_eq!(options.scene, Scene::Triangle);
        assert_eq!(options.model, None);
        assert_eq!(options.texture.as_deref(), Some("/texture/paint.png"));
        assert!(options.debug_overlay);

        let procedural = parse_args(["--environment", "procedural"]).unwrap();
        assert_eq!(procedural.environment, Some(EnvironmentMode::Procedural));
        let hdr = parse_args(["--environment", "/environment/sky.hdr"]).unwrap();
        assert_eq!(
            hdr.environment,
            Some(EnvironmentMode::Hdr("environment/sky.hdr".to_owned()))
        );

        let options = parse_args(["--model", "/model/cube.fbx"]).unwrap();
        assert_eq!(options.model.as_deref(), Some("/model/cube.fbx"));
        assert_eq!(options.texture, None);
        for scene in ["shadows", "cube", "materials", "clustered"] {
            let options = parse_args(["--scene", scene, "--model", "/model/cube.fbx"])
                .expect("non-triangle scenes may load a model");
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
            vec!["--camera-settings"],
            vec!["--camera-settings", "../camera.camerasettings"],
            vec!["--camera-settings", "/camera/default.toml"],
            vec![
                "--camera-settings",
                "/a.camerasettings",
                "--camera-settings",
                "/b.camerasettings",
            ],
            vec!["--resize-test", "--resize-test"],
            vec!["--frames", "119", "--resize-test"],
            vec!["--scene"],
            vec!["--scene", "sphere"],
            vec!["--scene", "cube", "--scene", "triangle"],
            vec!["--model"],
            vec!["--model", "/a.fbx", "--model", "/b.fbx"],
            vec!["--texture"],
            vec!["--texture", "/a.png", "--texture", "/b.png"],
            vec!["--environment"],
            vec!["--environment", "/environment/sky.png"],
            vec!["--environment", "procedural", "--environment", "procedural"],
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
        assert_eq!(options.render_scale, None);
        assert_eq!(options.camera_settings, None);
        assert!(!options.resize_test);
        assert_eq!(options.scene, Scene::Shadows);
        assert_eq!(options.model, None);
        assert_eq!(options.texture, None);
        assert!(!options.debug_overlay);
    }
}
