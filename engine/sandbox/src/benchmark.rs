//! Замеры для `--benchmark-json`: серии интервалов кадров, CPU- и GPU-фаз и JSON-отчёт.
//!
//! [`BenchmarkRecorder`] получает результат каждого `render_frame`. Прогрев — первые `warmup`
//! презентаций и в отчёт не попадает. После прогрева интервал между презентациями и CPU-фазы
//! берутся из `Presented`-вызовов, а GPU-замеры — из каждого профиля, включая `Skipped`.
//! GPU-результаты последних кадров в полёте renderer ещё не прочитаны, поэтому серии GPU
//! короче числа измеренных кадров.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine_core::profile::percentile;
use engine_project::BootstrapReflexMode;
use engine_render::{Capabilities, FrameProfile, GpuPass, PresentMode};
use serde::Serialize;

use crate::scene::Scene;

/// Прогрев по умолчанию: число презентаций, которые отбрасываются до начала замеров.
pub const DEFAULT_WARMUP_FRAMES: u64 = 60;
/// Предел измеряемых кадров; больший прогон требует сотни мегабайт под серии.
pub const MAX_MEASURED_FRAMES: u64 = 100_000;
const SCHEMA: u32 = 1;
const CPU_PHASE_COUNT: usize = 7;

/// Разобранные флаги `--benchmark-json` и `--warmup-frames`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BenchmarkOptions {
    pub path: String,
    pub warmup_frames: u64,
}

/// Параметры прогона, которые не известны recorder'у и записываются в отчёт.
pub struct RunSettings {
    pub scene: Scene,
    pub model: Option<String>,
    pub texture: Option<String>,
    pub environment: Option<String>,
    pub camera_settings: Option<String>,
    pub post_process: Option<String>,
    pub render_scale: f32,
    pub debug_overlay: bool,
}

/// Результат одного вызова `render_frame` для recorder'а.
pub struct Observation {
    /// Вызов завершился `FrameStatus::Presented`.
    pub presented: bool,
    pub now: Instant,
    /// Физический размер окна в момент вызова.
    pub size: (u32, u32),
    pub present_mode: PresentMode,
    /// Профиль, полученный `take_frame_profile` после вызова.
    pub profile: Option<FrameProfile>,
}

#[derive(Clone, Copy, Debug)]
struct Start {
    size: (u32, u32),
    present_mode: PresentMode,
}

/// Накапливает серии замеров одного прогона; память выделяется в [`BenchmarkRecorder::new`].
pub struct BenchmarkRecorder {
    frames: u64,
    warmup: u64,
    gpu: Capabilities,
    reflex_requested: Option<BootstrapReflexMode>,
    /// Презентации, увиденные recorder'ом, включая прогрев.
    presented: u64,
    last_presented: Option<Instant>,
    start: Option<Start>,
    end_present_mode: PresentMode,
    resized: bool,
    frame_interval: Vec<Duration>,
    cpu: [Vec<Duration>; CPU_PHASE_COUNT],
    gpu_total: Vec<Duration>,
    gpu_passes: [Vec<Duration>; GpuPass::COUNT],
}

impl BenchmarkRecorder {
    /// Создаёт recorder для `frames` презентаций с прогревом `warmup`.
    pub fn new(
        frames: u64,
        warmup: u64,
        capabilities: &Capabilities,
        reflex_requested: Option<BootstrapReflexMode>,
    ) -> Self {
        let measured = usize::try_from(frames.saturating_sub(warmup)).unwrap_or(0);
        Self {
            frames,
            warmup,
            gpu: Capabilities {
                device_name: capabilities.device_name.clone(),
                vendor_id: capabilities.vendor_id,
                api_version: capabilities.api_version,
                discrete: capabilities.discrete,
                tessellation_shader: capabilities.tessellation_shader,
                wireframe: capabilities.wireframe,
                fsr: capabilities.fsr,
                dlss: capabilities.dlss,
                low_latency: capabilities.low_latency,
            },
            reflex_requested,
            presented: 0,
            last_presented: None,
            start: None,
            end_present_mode: PresentMode::default(),
            resized: false,
            frame_interval: Vec::with_capacity(measured),
            cpu: std::array::from_fn(|_| Vec::with_capacity(measured)),
            gpu_total: Vec::with_capacity(measured),
            gpu_passes: std::array::from_fn(|_| Vec::with_capacity(measured)),
        }
    }

    /// Учитывает результат вызова `render_frame`.
    pub fn observe(&mut self, observation: Observation) {
        let measuring = self.presented >= self.warmup;
        if measuring {
            self.observe_measured(&observation);
            if observation.presented
                && let Some(last) = self.last_presented
            {
                self.frame_interval
                    .push(observation.now.saturating_duration_since(last));
            }
        }
        if observation.presented {
            self.last_presented = Some(observation.now);
            self.presented += 1;
        }
    }

    fn observe_measured(&mut self, observation: &Observation) {
        let start = *self.start.get_or_insert(Start {
            size: observation.size,
            present_mode: observation.present_mode,
        });
        if observation.size != start.size {
            self.resized = true;
        }
        self.end_present_mode = observation.present_mode;
        let Some(profile) = observation.profile else {
            return;
        };
        if let Some(view) = profile.gpu.and_then(|gpu| gpu.views[0]) {
            self.gpu_total.push(view.total);
            for (series, value) in self.gpu_passes.iter_mut().zip(view.passes) {
                if let Some(value) = value {
                    series.push(value);
                }
            }
        }
        if observation.presented {
            for (series, value) in self.cpu.iter_mut().zip(cpu_phases(&profile)) {
                series.push(value);
            }
        }
    }

    /// Собирает отчёт. Прогон, не доведённый до `frames` презентаций, возвращает ошибку.
    pub fn finish(self, settings: RunSettings) -> Result<BenchmarkReport, String> {
        if self.presented < self.frames {
            return Err(format!(
                "бенчмарк прерван: выполнено {} из {} презентаций",
                self.presented, self.frames
            ));
        }
        let start = self
            .start
            .ok_or_else(|| "бенчмарк прерван: нет измеренных кадров".to_owned())?;
        let [
            prepare,
            fence_wait,
            uploads,
            acquire,
            record,
            submit,
            present,
        ] = self.cpu;
        let gpu = self.gpu;
        Ok(BenchmarkReport {
            schema: SCHEMA,
            sandbox_version: env!("CARGO_PKG_VERSION"),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            scene: scene_name(settings.scene),
            model: settings.model,
            texture: settings.texture,
            environment: settings.environment,
            camera_settings: settings.camera_settings,
            post_process: settings.post_process,
            render_scale: settings.render_scale,
            debug_overlay: settings.debug_overlay,
            frames: self.frames,
            warmup_frames: self.warmup,
            measured_frames: self.frames - self.warmup,
            window: WindowReport {
                width: start.size.0,
                height: start.size.1,
                resized: self.resized,
            },
            present_mode: PresentModeReport {
                start: present_mode_name(start.present_mode),
                end: present_mode_name(self.end_present_mode),
                changed: start.present_mode != self.end_present_mode,
            },
            reflex: ReflexReport {
                requested: self.reflex_requested.map(reflex_name),
                active: gpu.low_latency,
            },
            gpu: GpuReport {
                name: gpu.device_name,
                vendor_id: gpu.vendor_id,
                vulkan: vulkan_version(gpu.api_version),
                discrete: gpu.discrete,
            },
            frame_interval_ms: summarize(self.frame_interval),
            cpu_ms: CpuReport {
                prepare: summarize(prepare),
                fence_wait: summarize(fence_wait),
                uploads: summarize(uploads),
                acquire: summarize(acquire),
                record: summarize(record),
                submit: summarize(submit),
                present: summarize(present),
            },
            gpu_ms: GpuMsReport {
                total: summarize(self.gpu_total),
                passes: self
                    .gpu_passes
                    .into_iter()
                    .enumerate()
                    .map(|(index, series)| (GpuPass::ALL[index].label(), summarize(series)))
                    .collect(),
            },
        })
    }
}

/// Завершает прогон: собирает отчёт, выводит предупреждения и атомарно пишет JSON в `path`.
pub fn finish_and_write(
    recorder: BenchmarkRecorder,
    settings: RunSettings,
    path: &Path,
) -> Result<(), String> {
    let report = recorder.finish(settings)?;
    if report.present_mode.end == "Fifo" {
        eprintln!(
            "предупреждение: PresentMode Fifo — интервалы кадров зависят от vsync, сравнивайте CPU- и GPU-серии"
        );
    }
    if report.window.resized {
        eprintln!("предупреждение: размер окна менялся во время измерения");
    }
    write_report(path, &report)
}

/// Проверяет путь `--benchmark-json` до прогона: у него должно быть имя файла, а каталог-родитель
/// (пустой — текущий) должен существовать и быть каталогом. Файловая система не меняется.
pub fn validate_output_path(path: &Path) -> Result<(), String> {
    if path.file_name().is_none() {
        return Err(format!("путь {} не содержит имени файла", path.display()));
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    match fs::metadata(parent) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!("{} не является каталогом", parent.display())),
        Err(error) => Err(format!("каталог {} недоступен: {error}", parent.display())),
    }
}

/// Пишет отчёт во временный `<path>.tmp` рядом и переименовывает его в `path`.
/// При ошибке временный файл удаляется, а старый `path` остаётся нетронутым.
pub fn write_report(path: &Path, report: &BenchmarkReport) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(report)
        .map_err(|error| format!("не удалось сериализовать бенчмарк: {error}"))?;
    let temporary = temporary_path(path);
    if let Err(error) = fs::write(&temporary, &bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "не удалось записать {}: {error}",
            temporary.display()
        ));
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("не удалось заменить {}: {error}", path.display())
    })
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn cpu_phases(profile: &FrameProfile) -> [Duration; CPU_PHASE_COUNT] {
    let cpu = profile.cpu;
    [
        cpu.prepare,
        cpu.fence_wait,
        cpu.uploads,
        cpu.acquire,
        cpu.record,
        cpu.submit,
        cpu.present,
    ]
}

fn summarize(mut values: Vec<Duration>) -> SeriesReport {
    let samples = values.len();
    // Повторный выбор работает и на переставленном срезе: порядок не нужен.
    let median = percentile(&mut values, 50).map(millis);
    let p95 = percentile(&mut values, 95).map(millis);
    SeriesReport {
        samples,
        median,
        p95,
    }
}

fn millis(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn vulkan_version(api_version: u32) -> String {
    format!(
        "{}.{}",
        (api_version >> 22) & 0x7f,
        (api_version >> 12) & 0x3ff
    )
}

fn present_mode_name(mode: PresentMode) -> &'static str {
    match mode {
        PresentMode::Fifo => "Fifo",
        PresentMode::Mailbox => "Mailbox",
        PresentMode::Immediate => "Immediate",
    }
}

fn reflex_name(mode: BootstrapReflexMode) -> &'static str {
    match mode {
        BootstrapReflexMode::Off => "Off",
        BootstrapReflexMode::On => "On",
        BootstrapReflexMode::Boost => "Boost",
    }
}

fn scene_name(scene: Scene) -> &'static str {
    match scene {
        Scene::Triangle => "triangle",
        Scene::Cube => "cube",
        Scene::Shadows => "shadows",
        Scene::Materials => "materials",
        Scene::Clustered => "clustered",
        Scene::Particles => "particles",
        Scene::Stress => "stress",
    }
}

/// Корневой объект JSON-отчёта; поля и значения описаны в `Obsidian/RVEVault/Engine/Sandbox.md`.
#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    schema: u32,
    sandbox_version: &'static str,
    build: &'static str,
    scene: &'static str,
    model: Option<String>,
    texture: Option<String>,
    environment: Option<String>,
    camera_settings: Option<String>,
    post_process: Option<String>,
    render_scale: f32,
    debug_overlay: bool,
    frames: u64,
    warmup_frames: u64,
    measured_frames: u64,
    window: WindowReport,
    present_mode: PresentModeReport,
    reflex: ReflexReport,
    gpu: GpuReport,
    frame_interval_ms: SeriesReport,
    cpu_ms: CpuReport,
    gpu_ms: GpuMsReport,
}

#[derive(Debug, Serialize)]
struct WindowReport {
    width: u32,
    height: u32,
    resized: bool,
}

#[derive(Debug, Serialize)]
struct PresentModeReport {
    start: &'static str,
    end: &'static str,
    changed: bool,
}

#[derive(Debug, Serialize)]
struct ReflexReport {
    requested: Option<&'static str>,
    active: bool,
}

#[derive(Debug, Serialize)]
struct GpuReport {
    name: String,
    vendor_id: u32,
    vulkan: String,
    discrete: bool,
}

/// Серия в миллисекундах; без замеров `median` и `p95` равны `null`.
#[derive(Debug, Serialize)]
struct SeriesReport {
    samples: usize,
    median: Option<f64>,
    p95: Option<f64>,
}

#[derive(Debug, Serialize)]
struct CpuReport {
    prepare: SeriesReport,
    fence_wait: SeriesReport,
    uploads: SeriesReport,
    acquire: SeriesReport,
    record: SeriesReport,
    submit: SeriesReport,
    present: SeriesReport,
}

#[derive(Debug, Serialize)]
struct GpuMsReport {
    total: SeriesReport,
    /// Ключ — [`GpuPass::label`], порядок ключей алфавитный.
    passes: BTreeMap<&'static str, SeriesReport>,
}

#[cfg(test)]
mod tests {
    use super::{
        BenchmarkRecorder, Observation, RunSettings, temporary_path, validate_output_path,
        write_report,
    };
    use crate::scene::Scene;
    use engine_project::BootstrapReflexMode;
    use engine_render::{
        Capabilities, FrameProfile, GpuFrameTimings, GpuPass, GpuViewTimings, PresentMode,
        ProfiledView, RenderCpuTimings,
    };
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            device_name: "Test GPU".to_owned(),
            vendor_id: 0x10de,
            api_version: (1 << 22) | (3 << 12),
            discrete: true,
            tessellation_shader: false,
            wireframe: false,
            fsr: false,
            dlss: false,
            low_latency: false,
        }
    }

    fn settings() -> RunSettings {
        RunSettings {
            scene: Scene::Stress,
            model: None,
            texture: None,
            environment: None,
            camera_settings: None,
            post_process: None,
            render_scale: 1.0,
            debug_overlay: false,
        }
    }

    fn cpu_profile(prepare: u64) -> FrameProfile {
        FrameProfile {
            cpu: RenderCpuTimings {
                prepare: ms(prepare),
                ..RenderCpuTimings::default()
            },
            ..FrameProfile::default()
        }
    }

    fn gpu_profile(prepare: u64, total: u64) -> FrameProfile {
        let mut profile = cpu_profile(prepare);
        let mut gpu = GpuFrameTimings::default();
        gpu.views[0] = Some(GpuViewTimings {
            key: ProfiledView::Screen,
            passes: [None; GpuPass::COUNT],
            total: ms(total),
        });
        profile.gpu = Some(gpu);
        profile
    }

    fn observation(
        base: Instant,
        at: u64,
        presented: bool,
        profile: Option<FrameProfile>,
    ) -> Observation {
        Observation {
            presented,
            now: base + ms(at),
            size: (1280, 720),
            present_mode: PresentMode::Mailbox,
            profile,
        }
    }

    fn report_value(recorder: BenchmarkRecorder) -> serde_json::Value {
        let report = recorder.finish(settings()).expect("прогон завершён");
        serde_json::to_value(&report).expect("отчёт сериализуется")
    }

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rve-bench-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn warmup_presentations_are_discarded_and_the_first_interval_starts_at_the_last_warmup_frame() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(4, 2, &capabilities(), None);
        // Прогрев: t = 0 и 50 мс с шумными CPU-фазами. Замеры: t = 60 и 100 мс.
        recorder.observe(observation(base, 0, true, Some(cpu_profile(100))));
        recorder.observe(observation(base, 50, true, Some(cpu_profile(100))));
        recorder.observe(observation(base, 60, true, Some(cpu_profile(1))));
        recorder.observe(observation(base, 100, true, Some(cpu_profile(3))));

        let value = report_value(recorder);
        // Интервалы: 60 − 50 = 10 мс (от последнего кадра прогрева) и 100 − 60 = 40 мс.
        assert_eq!(value["frame_interval_ms"]["samples"], 2);
        assert_eq!(value["frame_interval_ms"]["median"], 10.0);
        assert_eq!(value["frame_interval_ms"]["p95"], 40.0);
        assert_eq!(value["cpu_ms"]["prepare"]["samples"], 2);
        assert_eq!(value["cpu_ms"]["prepare"]["median"], 1.0);
        assert_eq!(value["cpu_ms"]["prepare"]["p95"], 3.0);
        assert_eq!(value["measured_frames"], 2);
    }

    #[test]
    fn skipped_presentations_add_gpu_samples_but_no_interval_or_cpu_samples() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(2, 0, &capabilities(), None);
        recorder.observe(observation(base, 0, true, Some(gpu_profile(2, 5))));
        recorder.observe(observation(base, 5, false, Some(gpu_profile(9, 7))));
        recorder.observe(observation(base, 10, true, Some(gpu_profile(4, 6))));

        let value = report_value(recorder);
        assert_eq!(value["cpu_ms"]["prepare"]["samples"], 2);
        assert_eq!(value["gpu_ms"]["total"]["samples"], 3);
        assert_eq!(value["gpu_ms"]["total"]["median"], 6.0);
        // Интервал только между двумя презентациями, пропущенный кадр его не сдвигает.
        assert_eq!(value["frame_interval_ms"]["samples"], 1);
        assert_eq!(value["frame_interval_ms"]["median"], 10.0);
    }

    #[test]
    fn single_measured_frame_gives_empty_interval_series() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(1, 0, &capabilities(), None);
        recorder.observe(observation(base, 0, true, Some(cpu_profile(4))));

        let value = report_value(recorder);
        assert_eq!(value["frame_interval_ms"]["samples"], 0);
        assert!(value["frame_interval_ms"]["median"].is_null());
        assert!(value["frame_interval_ms"]["p95"].is_null());
        assert_eq!(value["cpu_ms"]["prepare"]["samples"], 1);
    }

    #[test]
    fn missing_gpu_profiles_leave_gpu_series_empty() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(2, 0, &capabilities(), None);
        recorder.observe(observation(base, 0, true, None));
        recorder.observe(observation(base, 10, true, Some(cpu_profile(1))));

        let value = report_value(recorder);
        assert_eq!(value["gpu_ms"]["total"]["samples"], 0);
        assert!(value["gpu_ms"]["total"]["median"].is_null());
        assert_eq!(value["cpu_ms"]["prepare"]["samples"], 1);
    }

    #[test]
    fn partially_recorded_pass_keeps_its_own_sample_count() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(3, 0, &capabilities(), None);
        for (at, pass_value) in [(0, Some(ms(2))), (10, None), (20, Some(ms(4)))] {
            let mut profile = gpu_profile(1, 9);
            if let Some(gpu) = profile.gpu.as_mut() {
                let view = gpu.views[0].as_mut().expect("первый вид");
                view.passes[GpuPass::Clusters as usize] = pass_value;
            }
            recorder.observe(observation(base, at, true, Some(profile)));
        }

        let value = report_value(recorder);
        assert_eq!(value["gpu_ms"]["total"]["samples"], 3);
        assert_eq!(value["gpu_ms"]["passes"]["Clusters"]["samples"], 2);
        assert_eq!(value["gpu_ms"]["passes"]["Clusters"]["median"], 2.0);
        assert_eq!(
            value["gpu_ms"]["passes"]["Shadow directional"]["samples"],
            0
        );
        assert!(value["gpu_ms"]["passes"]["Shadow directional"]["p95"].is_null());
    }

    #[test]
    fn report_contains_run_and_device_fields() {
        let base = Instant::now();
        let mut recorder =
            BenchmarkRecorder::new(2, 0, &capabilities(), Some(BootstrapReflexMode::On));
        recorder.observe(observation(base, 0, true, None));
        recorder.observe(observation(base, 10, true, None));

        let value = report_value(recorder);
        assert_eq!(value["schema"], 1);
        assert_eq!(value["scene"], "stress");
        assert_eq!(value["frames"], 2);
        assert_eq!(value["warmup_frames"], 0);
        assert_eq!(value["window"]["width"], 1280);
        assert_eq!(value["window"]["resized"], false);
        assert_eq!(value["present_mode"]["start"], "Mailbox");
        assert_eq!(value["present_mode"]["changed"], false);
        assert_eq!(value["reflex"]["requested"], "On");
        assert_eq!(value["reflex"]["active"], false);
        assert_eq!(value["gpu"]["vulkan"], "1.3");
        assert_eq!(value["gpu"]["name"], "Test GPU");
    }

    #[test]
    fn resize_during_measurement_sets_the_flag() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(2, 0, &capabilities(), None);
        recorder.observe(observation(base, 0, true, None));
        recorder.observe(Observation {
            size: (960, 540),
            ..observation(base, 10, true, None)
        });

        assert_eq!(report_value(recorder)["window"]["resized"], true);
    }

    #[test]
    fn interrupted_run_is_an_error_and_names_progress() {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(3, 0, &capabilities(), None);
        recorder.observe(observation(base, 0, true, None));
        recorder.observe(observation(base, 10, true, None));

        let error = recorder.finish(settings()).expect_err("прогон не завершён");
        assert!(error.contains("бенчмарк прерван"), "{error}");
        assert!(error.contains("2 из 3"), "{error}");
    }

    fn finished_report() -> super::BenchmarkReport {
        let base = Instant::now();
        let mut recorder = BenchmarkRecorder::new(1, 0, &capabilities(), None);
        recorder.observe(observation(base, 0, true, Some(cpu_profile(1))));
        recorder.finish(settings()).expect("прогон завершён")
    }

    #[test]
    fn write_replaces_an_existing_report_and_removes_the_temporary_file() {
        let directory = temporary_directory("replace");
        let target = directory.join("bench.json");
        fs::write(&target, b"old").unwrap();

        write_report(&target, &finished_report()).expect("запись проходит");

        let written: serde_json::Value =
            serde_json::from_slice(&fs::read(&target).unwrap()).expect("валидный JSON");
        assert_eq!(written["schema"], 1);
        assert!(!temporary_path(&target).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn output_path_is_checked_before_the_run() {
        let directory = temporary_directory("validate");
        let file = directory.join("file.txt");
        fs::write(&file, b"").unwrap();

        assert!(validate_output_path(&directory.join("bench.json")).is_ok());
        assert!(validate_output_path(Path::new("bench.json")).is_ok());

        let missing = directory.join("missing").join("bench.json");
        let error = validate_output_path(&missing).expect_err("каталога нет");
        assert!(error.contains("недоступен"), "{error}");

        let under_file = file.join("bench.json");
        let error = validate_output_path(&under_file).expect_err("родитель — файл");
        assert!(error.contains("не является каталогом"), "{error}");

        let error = validate_output_path(Path::new(".")).expect_err("нет имени файла");
        assert!(error.contains("не содержит имени файла"), "{error}");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_write_leaves_neither_report_nor_temporary_file() {
        let directory = temporary_directory("failed");
        let target = directory.join("missing").join("bench.json");

        assert!(write_report(&target, &finished_report()).is_err());
        assert!(!target.exists());
        assert!(!temporary_path(&target).exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
