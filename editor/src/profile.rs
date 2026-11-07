//! Замеры редактора для вкладки Profiler: CPU-фазы renderer, GPU по видам Scene и Game и шаг Play.

use std::time::{Duration, Instant};

use engine_core::profile::{ProfileGroup, ProfileReport, ProfileRow, ProfileValue, RollingStat};
use engine_render::{FrameProfile, GpuPass, GpuViewTimings, ProfiledView, ViewportId};
use engine_resources::{ResourceSampler, ResourceStats};
use engine_script::ScriptUpdateProfile;

/// Число последних кадров и шагов, по которым вкладка считает last, avg и max.
const WINDOW: usize = 240;
/// Период снимка статистики ресурсов; совпадает с публикацией в runtime.
const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const CPU_LABELS: [&str; 7] = [
    "prepare",
    "fence wait",
    "uploads",
    "acquire",
    "record",
    "submit",
    "present",
];
const PLAY_LABELS: [&str; 9] = [
    "total",
    "world matrices",
    "messages",
    "physics events",
    "Lua instances",
    "Rust instances",
    "other",
    "physics wait",
    "physics step",
];

/// Замер одного шага Play: фазы скриптов и ожидание и шаг физики.
#[derive(Clone, Copy, Debug, Default)]
pub struct PlayStep {
    pub script: ScriptUpdateProfile,
    pub physics_wait: Duration,
    pub physics_step: Duration,
}

/// Окна замеров редактора; строятся по кадрам renderer и шагам Play.
pub struct EditorProfile {
    cpu: Vec<RollingStat>,
    scene: Vec<RollingStat>,
    game: Vec<RollingStat>,
    play: Vec<RollingStat>,
    resources: ResourceSampler,
    next_resource_sample: Option<Instant>,
}

impl Default for EditorProfile {
    fn default() -> Self {
        Self {
            cpu: windows(CPU_LABELS.len()),
            scene: windows(GpuPass::COUNT + 1),
            game: windows(GpuPass::COUNT + 1),
            play: windows(PLAY_LABELS.len()),
            resources: ResourceSampler::default(),
            next_resource_sample: None,
        }
    }
}

fn windows(count: usize) -> Vec<RollingStat> {
    (0..count).map(|_| RollingStat::new(WINDOW)).collect()
}

impl EditorProfile {
    /// Записывает CPU-фазы кадра и GPU-проходы видов, сопоставляя их по ключу viewport.
    pub fn record_frame(&mut self, frame: &FrameProfile, scene: ViewportId, game: ViewportId) {
        let cpu = frame.cpu;
        let phases = [
            cpu.prepare,
            cpu.fence_wait,
            cpu.uploads,
            cpu.acquire,
            cpu.record,
            cpu.submit,
            cpu.present,
        ];
        for (window, elapsed) in self.cpu.iter_mut().zip(phases) {
            window.push(elapsed);
        }
        let views = frame
            .gpu
            .iter()
            .flat_map(|gpu| gpu.views.iter().flatten())
            .map(|view| (view.key, *view));
        record_gpu(
            &mut self.scene,
            &mut self.game,
            ProfiledView::Viewport(scene),
            ProfiledView::Viewport(game),
            views,
        );
    }

    /// Снимает статистику запросов ресурсов не чаще [`RESOURCE_SAMPLE_INTERVAL`].
    pub fn record_resources(&mut self, now: Instant, stats: ResourceStats) {
        if self.next_resource_sample.is_some_and(|next| now < next) {
            return;
        }
        self.next_resource_sample = Some(now + RESOURCE_SAMPLE_INTERVAL);
        self.resources.sample(stats);
    }

    /// Записывает фазы шага Play.
    pub fn record_play(&mut self, step: PlayStep) {
        let script = step.script;
        let phases = [
            script.total,
            script.world_matrices,
            script.messages,
            script.physics_events,
            script.lua,
            script.rust,
            script.other(),
            step.physics_wait,
            step.physics_step,
        ];
        for (window, elapsed) in self.play.iter_mut().zip(phases) {
            window.push(elapsed);
        }
    }

    /// Строит отчёт для вкладки. Вызывается при показе вкладки, а не в кадре записи.
    pub fn report(&self) -> ProfileReport {
        let mut gpu_labels: Vec<&'static str> =
            GpuPass::ALL.iter().map(|pass| pass.label()).collect();
        gpu_labels.push("total");
        ProfileReport {
            groups: vec![
                group("Renderer CPU", &CPU_LABELS, &self.cpu),
                group("GPU Scene", &gpu_labels, &self.scene),
                group("GPU Game", &gpu_labels, &self.game),
                group("Play", &PLAY_LABELS, &self.play),
                self.resources.profile_group(),
            ],
        }
    }
}

/// Записывает GPU-виды кадра в окна Scene и Game. Вид, которого нет в кадре, сбрасывает свои окна:
/// значения прошлых кадров не приписываются виду, который сейчас не рисуется.
///
/// Ключи обобщены, потому что `ViewportId` создаётся только в `engine-render`; `record_frame`
/// передаёт `ProfiledView::Viewport`.
fn record_gpu<K: Copy + PartialEq>(
    scene: &mut [RollingStat],
    game: &mut [RollingStat],
    scene_key: K,
    game_key: K,
    views: impl IntoIterator<Item = (K, GpuViewTimings)>,
) {
    let mut scene_view = None;
    let mut game_view = None;
    for (key, view) in views {
        if key == scene_key {
            scene_view = Some(view);
        } else if key == game_key {
            game_view = Some(view);
        }
    }
    record_view(scene, scene_view);
    record_view(game, game_view);
}

fn record_view(windows: &mut [RollingStat], view: Option<GpuViewTimings>) {
    let Some(view) = view else {
        windows.iter_mut().for_each(RollingStat::clear);
        return;
    };
    for (window, pass) in windows.iter_mut().zip(view.passes) {
        match pass {
            Some(elapsed) => window.push(elapsed),
            None => window.clear(),
        }
    }
    windows[GpuPass::COUNT].push(view.total);
}

fn group(name: &'static str, labels: &[&'static str], windows: &[RollingStat]) -> ProfileGroup {
    ProfileGroup {
        name,
        rows: labels
            .iter()
            .zip(windows)
            .map(|(&label, window)| ProfileRow {
                label,
                value: ProfileValue::from_summary(window.summary()),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorProfile, PlayStep, RESOURCE_SAMPLE_INTERVAL, record_gpu, windows};
    use engine_core::profile::{ProfileReport, ProfileValue};
    use engine_render::{GpuPass, GpuViewTimings, ProfiledView};
    use engine_resources::ResourceStats;
    use std::time::{Duration, Instant};

    const SCENE: u8 = 1;
    const GAME: u8 = 2;
    const OTHER: u8 = 3;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn timings(passes: &[(GpuPass, u64)], total: u64) -> GpuViewTimings {
        let mut slots = [None; GpuPass::COUNT];
        for &(pass, value) in passes {
            slots[pass as usize] = Some(ms(value));
        }
        GpuViewTimings {
            key: ProfiledView::Screen,
            passes: slots,
            total: ms(total),
        }
    }

    fn gpu_windows() -> Vec<engine_core::profile::RollingStat> {
        windows(GpuPass::COUNT + 1)
    }

    fn row_value(report: &ProfileReport, group: &str, label: &str) -> Option<ProfileValue> {
        report
            .groups
            .iter()
            .find(|candidate| candidate.name == group)?
            .rows
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.value)
    }

    fn row_samples(
        report: &engine_core::profile::ProfileReport,
        group: &str,
        label: &str,
    ) -> Option<usize> {
        report
            .groups
            .iter()
            .find(|candidate| candidate.name == group)?
            .rows
            .iter()
            .find(|row| row.label == label)
            .and_then(|row| match row.value {
                ProfileValue::Timing(summary) => Some(summary.samples),
                _ => None,
            })
    }

    #[test]
    fn play_step_fills_script_and_physics_rows() {
        let mut profile = EditorProfile::default();
        let step = PlayStep {
            physics_wait: ms(1),
            physics_step: ms(2),
            ..PlayStep::default()
        };

        profile.record_play(step);

        let report = profile.report();
        assert_eq!(row_samples(&report, "Play", "physics wait"), Some(1));
        assert_eq!(row_samples(&report, "Play", "physics step"), Some(1));
        assert_eq!(row_samples(&report, "Play", "Lua instances"), Some(1));
        assert_eq!(row_samples(&report, "GPU Scene", "total"), None);
    }

    #[test]
    fn report_lists_renderer_gpu_views_play_and_resources_in_order() {
        let report = EditorProfile::default().report();
        let names: Vec<&str> = report.groups.iter().map(|group| group.name).collect();
        assert_eq!(
            names,
            ["Renderer CPU", "GPU Scene", "GPU Game", "Play", "Resources"]
        );
    }

    #[test]
    fn resource_stats_are_sampled_at_most_once_per_interval() {
        let mut profile = EditorProfile::default();
        let start = Instant::now();
        let stats = |completed| ResourceStats {
            completed,
            ..ResourceStats::default()
        };

        profile.record_resources(start, stats(1));
        profile.record_resources(start + ms(100), stats(4));
        assert_eq!(
            row_value(&profile.report(), "Resources", "completed / interval"),
            Some(ProfileValue::Missing),
            "второй снимок раньше интервала не должен фиксировать дельту"
        );

        profile.record_resources(start + RESOURCE_SAMPLE_INTERVAL, stats(6));
        assert_eq!(
            row_value(&profile.report(), "Resources", "completed / interval"),
            Some(ProfileValue::Count(5)),
            "дельта считается от последнего принятого снимка"
        );
    }

    #[test]
    fn scene_and_game_views_keep_their_own_samples() {
        let mut scene = gpu_windows();
        let mut game = gpu_windows();

        record_gpu(
            &mut scene,
            &mut game,
            SCENE,
            GAME,
            [
                (SCENE, timings(&[(GpuPass::Clusters, 2)], 2)),
                (GAME, timings(&[(GpuPass::Tonemap, 5)], 5)),
            ],
        );

        assert_eq!(scene[GpuPass::Clusters as usize].summary().last, ms(2));
        assert_eq!(scene[GpuPass::Tonemap as usize].summary().samples, 0);
        assert_eq!(scene[GpuPass::COUNT].summary().last, ms(2));
        assert_eq!(game[GpuPass::Tonemap as usize].summary().last, ms(5));
        assert_eq!(game[GpuPass::Clusters as usize].summary().samples, 0);
        assert_eq!(game[GpuPass::COUNT].summary().last, ms(5));
    }

    #[test]
    fn view_missing_from_frame_resets_and_foreign_views_are_not_attributed() {
        let mut scene = gpu_windows();
        let mut game = gpu_windows();
        record_gpu(
            &mut scene,
            &mut game,
            SCENE,
            GAME,
            [
                (SCENE, timings(&[(GpuPass::Clusters, 2)], 2)),
                (GAME, timings(&[(GpuPass::Tonemap, 5)], 5)),
            ],
        );

        // Scene ушёл из кадра; вместо него пришёл чужой вид и новый замер Game.
        record_gpu(
            &mut scene,
            &mut game,
            SCENE,
            GAME,
            [
                (OTHER, timings(&[(GpuPass::Clusters, 9)], 9)),
                (GAME, timings(&[(GpuPass::Tonemap, 6)], 6)),
            ],
        );

        assert_eq!(scene[GpuPass::Clusters as usize].summary().samples, 0);
        assert_eq!(scene[GpuPass::COUNT].summary().samples, 0);
        assert_eq!(game[GpuPass::Clusters as usize].summary().samples, 0);
        let tonemap = game[GpuPass::Tonemap as usize].summary();
        assert_eq!(tonemap.samples, 2);
        assert_eq!(tonemap.last, ms(6));
        assert_eq!(tonemap.max, ms(6));
    }

    #[test]
    fn pass_absent_from_frame_is_reported_as_missing() {
        let mut scene = gpu_windows();
        let mut game = gpu_windows();
        record_gpu(
            &mut scene,
            &mut game,
            SCENE,
            GAME,
            [(SCENE, timings(&[(GpuPass::Clusters, 2)], 2))],
        );

        record_gpu(
            &mut scene,
            &mut game,
            SCENE,
            GAME,
            [(SCENE, timings(&[(GpuPass::Tonemap, 1)], 1))],
        );

        assert_eq!(scene[GpuPass::Clusters as usize].summary().samples, 0);
        assert_eq!(scene[GpuPass::Tonemap as usize].summary().last, ms(1));
    }
}
