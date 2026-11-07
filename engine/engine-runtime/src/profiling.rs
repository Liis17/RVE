//! CPU- и GPU-замеры потоков runtime: окна по фазам и публикация отчётов в `ProfileBoard`.
//!
//! Потоки записывают замеры в кольцевые окна без аллокаций в кадре. Окно строит
//! отчёт и публикует его не чаще [`PUBLISH_INTERVAL`]: эти аллокации происходят
//! с частотой показа, а не на горячем пути.

use engine_core::profile::{
    ProfileBoard, ProfileGroup, ProfileReport, ProfileRow, RollingStat, StatSummary,
};
use engine_render::{FrameProfile, GpuPass};
use engine_script::ScriptUpdateProfile;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const SOURCE_SCRIPTS: &str = "scripts";
pub(crate) const SOURCE_RENDER: &str = "render";
pub(crate) const SOURCE_STARTUP: &str = "startup";

/// Число последних замеров, по которым считаются last, avg и max.
const SAMPLE_WINDOW: usize = 240;
/// Частота публикации отчёта одного потока.
pub(crate) const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

/// Группы и фазы, которые записывает поток сцены.
pub(crate) mod script_phase {
    pub const LOOP: usize = 0;
    pub const UPDATE: usize = 1;

    pub const PHYSICS_WAIT: usize = 0;
    pub const CONSOLE_COMMANDS: usize = 1;
    pub const SCRIPTS_UPDATE: usize = 2;
    pub const PHYSICS_SYNC: usize = 3;
    pub const BUILD_FRAME: usize = 4;
    pub const MAKE_SNAPSHOT: usize = 5;
    pub const SNAPSHOT_SEND_WAIT: usize = 6;

    pub const UPDATE_TOTAL: usize = 0;
    pub const UPDATE_WORLD_MATRICES: usize = 1;
    pub const UPDATE_MESSAGES: usize = 2;
    pub const UPDATE_PHYSICS_EVENTS: usize = 3;
    pub const UPDATE_LUA: usize = 4;
    pub const UPDATE_RUST: usize = 5;
    pub const UPDATE_OTHER: usize = 6;
}

/// Группы и фазы, которые записывает поток рендера.
pub(crate) mod render_phase {
    pub const LOOP: usize = 0;
    pub const RENDERER: usize = 1;
    pub const GPU: usize = 2;

    pub const FRAME_WAIT: usize = 0;
    pub const RENDER_BACKEND: usize = 1;

    pub const PREPARE: usize = 0;
    pub const FENCE_WAIT: usize = 1;
    pub const UPLOADS: usize = 2;
    pub const ACQUIRE: usize = 3;
    pub const RECORD: usize = 4;
    pub const SUBMIT: usize = 5;
    pub const PRESENT: usize = 6;
}

/// Названия групп и меток окна потока сцены.
pub(crate) fn scripts_groups() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "Scripts thread",
            vec![
                "physics wait",
                "console commands",
                "scripts update",
                "physics sync",
                "build frame",
                "make snapshot",
                "snapshot send wait",
            ],
        ),
        (
            "Script update",
            vec![
                "total",
                "world matrices",
                "messages",
                "physics events",
                "Lua instances",
                "Rust instances",
                "other",
            ],
        ),
    ]
}

/// Названия групп и меток окна потока рендера, включая GPU-проходы первого вида.
pub(crate) fn render_groups() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut gpu: Vec<&'static str> = GpuPass::ALL.iter().map(|pass| pass.label()).collect();
    gpu.extend(["total", "output + UI"]);
    vec![
        ("Render thread", vec!["frame wait", "render backend"]),
        (
            "Renderer CPU",
            vec![
                "prepare",
                "fence wait",
                "uploads",
                "acquire",
                "record",
                "submit",
                "present",
            ],
        ),
        ("GPU (first view)", gpu),
    ]
}

struct PhaseGroup {
    title: &'static str,
    labels: Vec<&'static str>,
    stats: Vec<RollingStat>,
    /// Была ли запись метки с последней публикации. Метка без записей за интервал сбрасывается.
    touched: Vec<bool>,
}

/// Окна одного потока по группам фаз. Публикует отчёт в доску не чаще [`PUBLISH_INTERVAL`].
pub(crate) struct PhaseWindow {
    source: &'static str,
    board: Arc<ProfileBoard>,
    groups: Vec<PhaseGroup>,
    next_publish: Option<Instant>,
}

impl PhaseWindow {
    /// Создаёт окна на весь набор групп; память выделяется здесь, а не в кадре.
    pub(crate) fn new(
        source: &'static str,
        board: Arc<ProfileBoard>,
        groups: Vec<(&'static str, Vec<&'static str>)>,
    ) -> Self {
        let groups = groups
            .into_iter()
            .map(|(title, labels)| PhaseGroup {
                title,
                stats: labels
                    .iter()
                    .map(|_| RollingStat::new(SAMPLE_WINDOW))
                    .collect(),
                touched: vec![false; labels.len()],
                labels,
            })
            .collect();
        Self {
            source,
            board,
            groups,
            next_publish: None,
        }
    }

    /// Записывает замер метки `phase` группы `group`.
    pub(crate) fn record(&mut self, group: usize, phase: usize, elapsed: Duration) {
        let group = &mut self.groups[group];
        group.stats[phase].push(elapsed);
        group.touched[phase] = true;
    }

    /// Записывает замер, если он есть; `None` оставляет окно без изменений.
    pub(crate) fn record_optional(
        &mut self,
        group: usize,
        phase: usize,
        elapsed: Option<Duration>,
    ) {
        if let Some(elapsed) = elapsed {
            self.record(group, phase, elapsed);
        }
    }

    /// Публикует отчёт, если с прошлой публикации прошёл интервал. Первый вызов публикует сразу.
    pub(crate) fn publish_if_due(&mut self, now: Instant) {
        if self.next_publish.is_some_and(|next| now < next) {
            return;
        }
        self.next_publish = Some(now + PUBLISH_INTERVAL);
        self.retire_untouched();
        self.board.publish(self.source, self.report());
    }

    /// Сбрасывает окна меток, которых не было в интервале, и начинает новый интервал.
    fn retire_untouched(&mut self) {
        for group in &mut self.groups {
            for (stat, touched) in group.stats.iter_mut().zip(group.touched.iter_mut()) {
                if !*touched {
                    stat.clear();
                }
                *touched = false;
            }
        }
    }

    /// Строит отчёт по текущим окнам. Метки без замеров, в том числе сброшенные, получают `None`.
    pub(crate) fn report(&self) -> ProfileReport {
        ProfileReport {
            groups: self
                .groups
                .iter()
                .map(|group| ProfileGroup {
                    name: group.title,
                    rows: group
                        .labels
                        .iter()
                        .zip(&group.stats)
                        .map(|(&label, stat)| {
                            let summary = stat.summary();
                            ProfileRow {
                                label,
                                summary: (summary.samples > 0).then_some(summary),
                            }
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    pub(crate) fn board(&self) -> &Arc<ProfileBoard> {
        &self.board
    }
}

/// Переносит профиль кадра renderer в окна потока рендера.
pub(crate) fn record_frame_profile(window: &mut PhaseWindow, frame: &FrameProfile) {
    use render_phase::{ACQUIRE, FENCE_WAIT, PREPARE, PRESENT, RECORD, RENDERER, SUBMIT, UPLOADS};
    let cpu = frame.cpu;
    window.record(RENDERER, PREPARE, cpu.prepare);
    window.record(RENDERER, FENCE_WAIT, cpu.fence_wait);
    window.record(RENDERER, UPLOADS, cpu.uploads);
    window.record(RENDERER, ACQUIRE, cpu.acquire);
    window.record(RENDERER, RECORD, cpu.record);
    window.record(RENDERER, SUBMIT, cpu.submit);
    window.record(RENDERER, PRESENT, cpu.present);
    let Some(gpu) = frame.gpu else {
        return;
    };
    if let Some(view) = gpu.views[0] {
        for (pass, elapsed) in view.passes.iter().enumerate() {
            window.record_optional(render_phase::GPU, pass, *elapsed);
        }
        window.record(render_phase::GPU, GpuPass::COUNT, view.total);
    }
    window.record_optional(render_phase::GPU, GpuPass::COUNT + 1, gpu.output_ui);
}

/// Переносит замеры одного `update` скриптов в окна потока сцены.
pub(crate) fn record_update_profile(window: &mut PhaseWindow, profile: ScriptUpdateProfile) {
    use script_phase::{
        UPDATE, UPDATE_LUA, UPDATE_MESSAGES, UPDATE_OTHER, UPDATE_PHYSICS_EVENTS, UPDATE_RUST,
        UPDATE_TOTAL, UPDATE_WORLD_MATRICES,
    };
    window.record(UPDATE, UPDATE_TOTAL, profile.total);
    window.record(UPDATE, UPDATE_WORLD_MATRICES, profile.world_matrices);
    window.record(UPDATE, UPDATE_MESSAGES, profile.messages);
    window.record(UPDATE, UPDATE_PHYSICS_EVENTS, profile.physics_events);
    window.record(UPDATE, UPDATE_LUA, profile.lua);
    window.record(UPDATE, UPDATE_RUST, profile.rust);
    window.record(UPDATE, UPDATE_OTHER, profile.other());
}

/// Замеры старта по фазам в порядке выполнения.
#[derive(Default)]
pub(crate) struct StartupProfile {
    phases: Vec<(&'static str, Duration)>,
}

impl StartupProfile {
    pub(crate) fn record(&mut self, label: &'static str, elapsed: Duration) {
        self.phases.push((label, elapsed));
    }

    /// Публикует отчёт старта с итогом в конце. Вызывается один раз после подготовки.
    pub(crate) fn publish(&self, board: &ProfileBoard) {
        let total: Duration = self.phases.iter().map(|(_, elapsed)| *elapsed).sum();
        let mut rows: Vec<ProfileRow> = self
            .phases
            .iter()
            .map(|&(label, elapsed)| ProfileRow {
                label,
                summary: Some(StatSummary::once(elapsed)),
            })
            .collect();
        rows.push(ProfileRow {
            label: "total",
            summary: Some(StatSummary::once(total)),
        });
        board.publish(
            SOURCE_STARTUP,
            ProfileReport {
                groups: vec![ProfileGroup {
                    name: "Startup",
                    rows,
                }],
            },
        );
    }
}

/// Пишет опубликованный отчёт старта в лог одной записью. Пустой отчёт ничего не пишет.
pub(crate) fn log_startup_profile(board: &ProfileBoard) {
    if let Some((_, report)) = board
        .reports()
        .into_iter()
        .find(|(source, _)| *source == SOURCE_STARTUP)
    {
        log::info!("startup profile:\n{report}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_render::{GpuFrameTimings, GpuViewTimings, ProfiledView, RenderCpuTimings};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn row(report: &ProfileReport, group: usize, label: &str) -> Option<StatSummary> {
        report.groups[group]
            .rows
            .iter()
            .find(|row| row.label == label)
            .and_then(|row| row.summary)
    }

    #[test]
    fn publication_happens_at_most_once_per_interval() {
        let board = Arc::new(ProfileBoard::new(&[SOURCE_RENDER]));
        let mut window = PhaseWindow::new(SOURCE_RENDER, Arc::clone(&board), render_groups());
        let start = Instant::now();

        window.publish_if_due(start);
        let first = board.reports()[0].1.clone();
        window.publish_if_due(start + PUBLISH_INTERVAL - ms(1));
        assert!(Arc::ptr_eq(&first, &board.reports()[0].1));

        window.publish_if_due(start + PUBLISH_INTERVAL);
        assert!(!Arc::ptr_eq(&first, &board.reports()[0].1));
    }

    #[test]
    fn label_missing_for_a_whole_interval_is_reported_as_missing() {
        let board = Arc::new(ProfileBoard::new(&[SOURCE_RENDER]));
        let mut window = PhaseWindow::new(SOURCE_RENDER, Arc::clone(&board), render_groups());
        let start = Instant::now();
        let clusters = GpuPass::Clusters as usize;
        let tonemap = GpuPass::Tonemap as usize;
        window.record(render_phase::GPU, clusters, ms(2));
        window.publish_if_due(start);
        assert!(gpu_row(&board, "Clusters").is_some());

        window.record(render_phase::GPU, tonemap, ms(1));
        window.publish_if_due(start + PUBLISH_INTERVAL);

        assert_eq!(gpu_row(&board, "Clusters"), None);
        assert!(gpu_row(&board, "Tonemap").is_some());
    }

    #[test]
    fn label_seen_within_an_interval_keeps_its_window() {
        let board = Arc::new(ProfileBoard::new(&[SOURCE_RENDER]));
        let mut window = PhaseWindow::new(SOURCE_RENDER, Arc::clone(&board), render_groups());
        let start = Instant::now();
        let clusters = GpuPass::Clusters as usize;
        window.record(render_phase::GPU, clusters, ms(2));
        window.publish_if_due(start);

        window.record(render_phase::GPU, clusters, ms(4));
        window.publish_if_due(start + PUBLISH_INTERVAL);

        assert_eq!(gpu_row(&board, "Clusters").map(|s| s.samples), Some(2));
    }

    fn gpu_row(board: &ProfileBoard, label: &str) -> Option<StatSummary> {
        let reports = board.reports();
        row(&reports[0].1, render_phase::GPU, label)
    }

    #[test]
    fn labels_without_samples_are_reported_as_missing() {
        let mut window = PhaseWindow::new(
            SOURCE_RENDER,
            Arc::new(ProfileBoard::new(&[SOURCE_RENDER])),
            render_groups(),
        );
        window.record(render_phase::RENDERER, render_phase::PREPARE, ms(2));

        let report = window.report();
        assert_eq!(
            row(&report, render_phase::RENDERER, "prepare").map(|s| s.samples),
            Some(1)
        );
        assert_eq!(row(&report, render_phase::RENDERER, "fence wait"), None);
    }

    #[test]
    fn frame_profile_maps_first_view_passes_and_keeps_missing_ones_empty() {
        let mut window = PhaseWindow::new(
            SOURCE_RENDER,
            Arc::new(ProfileBoard::new(&[SOURCE_RENDER])),
            render_groups(),
        );
        let mut passes = [None; GpuPass::COUNT];
        passes[GpuPass::Clusters as usize] = Some(ms(2));
        passes[GpuPass::Tonemap as usize] = Some(ms(1));
        let frame = FrameProfile {
            cpu: RenderCpuTimings {
                prepare: ms(1),
                ..RenderCpuTimings::default()
            },
            gpu: Some(GpuFrameTimings {
                views: [
                    Some(GpuViewTimings {
                        key: ProfiledView::Screen,
                        passes,
                        total: ms(3),
                    }),
                    None,
                    None,
                    None,
                ],
                output_ui: None,
            }),
        };

        record_frame_profile(&mut window, &frame);

        let report = window.report();
        assert!(row(&report, render_phase::GPU, "Clusters").is_some());
        assert_eq!(row(&report, render_phase::GPU, "Shadow directional"), None);
        assert_eq!(
            row(&report, render_phase::GPU, "total").map(|s| s.last),
            Some(ms(3))
        );
        assert_eq!(row(&report, render_phase::GPU, "output + UI"), None);
        assert_eq!(
            row(&report, render_phase::RENDERER, "prepare").map(|s| s.last),
            Some(ms(1))
        );
    }

    #[test]
    fn startup_report_lists_phases_in_order_and_ends_with_total() {
        let board = ProfileBoard::new(&[SOURCE_STARTUP]);
        let mut startup = StartupProfile::default();
        startup.record("settings", ms(1));
        startup.record("scene", ms(3));

        startup.publish(&board);

        let reports = board.reports();
        let report = &reports[0].1;
        let labels: Vec<&str> = report.groups[0].rows.iter().map(|row| row.label).collect();
        assert_eq!(labels, ["settings", "scene", "total"]);
        assert_eq!(row(report, 0, "total"), Some(StatSummary::once(ms(4))));
        assert!(report.to_string().contains("4.000 ms"));
    }
}
