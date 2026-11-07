//! Замеры редактора для вкладки Profiler: CPU-фазы renderer, GPU по видам Scene и Game и шаг Play.

use std::time::Duration;

use engine_core::profile::{ProfileGroup, ProfileReport, ProfileRow, RollingStat};
use engine_render::{FrameProfile, GpuPass, ProfiledView, ViewportId};
use engine_script::ScriptUpdateProfile;

/// Число последних кадров и шагов, по которым вкладка считает last, avg и max.
const WINDOW: usize = 240;
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
}

impl Default for EditorProfile {
    fn default() -> Self {
        Self {
            cpu: windows(CPU_LABELS.len()),
            scene: windows(GpuPass::COUNT + 1),
            game: windows(GpuPass::COUNT + 1),
            play: windows(PLAY_LABELS.len()),
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
        let Some(gpu) = frame.gpu else {
            return;
        };
        for view in gpu.views.iter().flatten() {
            let target = match view.key {
                ProfiledView::Viewport(id) if id == scene => &mut self.scene,
                ProfiledView::Viewport(id) if id == game => &mut self.game,
                _ => continue,
            };
            for (window, pass) in target.iter_mut().zip(view.passes) {
                if let Some(elapsed) = pass {
                    window.push(elapsed);
                }
            }
            target[GpuPass::COUNT].push(view.total);
        }
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
            ],
        }
    }
}

fn group(name: &'static str, labels: &[&'static str], windows: &[RollingStat]) -> ProfileGroup {
    ProfileGroup {
        name,
        rows: labels
            .iter()
            .zip(windows)
            .map(|(&label, window)| {
                let summary = window.summary();
                ProfileRow {
                    label,
                    summary: (summary.samples > 0).then_some(summary),
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{EditorProfile, PlayStep};
    use std::time::Duration;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
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
            .find(|row| row.label == label)?
            .summary
            .map(|summary| summary.samples)
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
    fn report_lists_renderer_gpu_views_and_play_groups_in_order() {
        let report = EditorProfile::default().report();
        let names: Vec<&str> = report.groups.iter().map(|group| group.name).collect();
        assert_eq!(names, ["Renderer CPU", "GPU Scene", "GPU Game", "Play"]);
    }
}
