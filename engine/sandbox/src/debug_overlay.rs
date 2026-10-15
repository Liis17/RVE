//! Отладочный оверлей `--debug-overlay`: FPS, память и загрузка CPU/GPU с графиками.
//!
//! Геометрия строится в физических пикселях окна и перестраивается только при новой
//! выборке метрик, раз в [`SAMPLE_INTERVAL`].

use engine_render::OverlayVertex;
use font8x8::legacy::BASIC_LEGACY;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
/// 30 секунд при выборке раз в 0.5 с.
const SHORT_HISTORY: usize = 60;
/// 2 минуты при выборке раз в 0.5 с.
const LONG_HISTORY: usize = 240;
const MAX_NAME_CHARS: usize = 36;
/// Минимальный верх шкалы графика FPS.
const FPS_GRAPH_FLOOR: f32 = 60.0;

const GLYPH_SIZE: f32 = 8.0;
const LARGE_SCALE: f32 = 4.0;
const TEXT_SCALE: f32 = 2.0;
const MARGIN: f32 = 8.0;
const PADDING: f32 = 8.0;
const LINE_GAP: f32 = 4.0;
const SECTION_GAP: f32 = 10.0;
const GRAPH_WIDTH: f32 = 240.0;
const GRAPH_HEIGHT: f32 = 40.0;

const PANEL_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.6];
const TEXT_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const GRAPH_BACKGROUND: [f32; 4] = [1.0, 1.0, 1.0, 0.08];
const FPS_COLOR: [f32; 4] = [0.2, 0.9, 0.3, 0.9];
const RAM_COLOR: [f32; 4] = [0.3, 0.6, 1.0, 0.9];
const CPU_COLOR: [f32; 4] = [1.0, 0.6, 0.2, 0.9];
const GPU_COLOR: [f32; 4] = [0.9, 0.3, 0.9, 0.9];

/// Последние значения метрики фиксированной длины; старые вытесняются.
struct History {
    values: VecDeque<f32>,
    capacity: usize,
}

impl History {
    fn new(capacity: usize) -> Self {
        Self {
            values: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, value: f32) {
        if self.values.len() == self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    fn max(&self) -> f32 {
        self.values.iter().copied().fold(0.0, f32::max)
    }
}

/// Значения последней выборки; `None` — метрика недоступна.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sample {
    fps: f32,
    memory_bytes: Option<u64>,
    cpu_percent: Option<f32>,
    gpu_percent: Option<f32>,
}

/// Накопление кадров и GPU-времени между выборками и история значений.
struct Metrics {
    window_start: Instant,
    frames: u32,
    gpu_time: Duration,
    gpu_measured: bool,
    latest: Sample,
    fps: History,
    memory: History,
    cpu: History,
    gpu: History,
}

impl Metrics {
    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            frames: 0,
            gpu_time: Duration::ZERO,
            gpu_measured: false,
            latest: Sample::default(),
            fps: History::new(SHORT_HISTORY),
            memory: History::new(LONG_HISTORY),
            cpu: History::new(SHORT_HISTORY),
            gpu: History::new(SHORT_HISTORY),
        }
    }

    fn frame_presented(&mut self) {
        self.frames += 1;
    }

    fn add_gpu_time(&mut self, time: Duration) {
        self.gpu_time += time;
        self.gpu_measured = true;
    }

    fn sample_due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.window_start) >= SAMPLE_INTERVAL
    }

    /// Закрывает окно накопления и добавляет точку во все графики.
    fn sample(&mut self, now: Instant, memory_bytes: Option<u64>, cpu_percent: Option<f32>) {
        let elapsed = now.saturating_duration_since(self.window_start);
        let seconds = elapsed.as_secs_f32().max(f32::EPSILON);
        let gpu_percent = self
            .gpu_measured
            .then(|| (self.gpu_time.as_secs_f32() / seconds * 100.0).clamp(0.0, 100.0));
        self.latest = Sample {
            fps: self.frames as f32 / seconds,
            memory_bytes,
            cpu_percent,
            gpu_percent,
        };
        self.fps.push(self.latest.fps);
        self.memory
            .push(memory_bytes.map_or(0.0, |bytes| bytes as f32));
        self.cpu.push(cpu_percent.unwrap_or(0.0));
        self.gpu.push(gpu_percent.unwrap_or(0.0));
        self.window_start = now;
        self.frames = 0;
        self.gpu_time = Duration::ZERO;
        self.gpu_measured = false;
    }
}

/// Память и загрузка CPU текущего процесса через sysinfo.
struct SystemStats {
    system: System,
    pid: Option<Pid>,
}

impl SystemStats {
    /// Возвращает источник статистики и название CPU.
    fn new() -> (Self, String) {
        let mut system = System::new();
        // System::new не заполняет список CPU, а он нужен для названия и нормировки.
        system.refresh_cpu_list(CpuRefreshKind::nothing());
        let cpu_name = system
            .cpus()
            .first()
            .map(|cpu| cpu.brand().trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "unknown CPU".to_owned());
        let pid = sysinfo::get_current_pid().ok();
        let mut stats = Self { system, pid };
        // Первое обновление задаёт базу для расчёта загрузки CPU по разнице времени.
        stats.refresh();
        (stats, cpu_name)
    }

    /// Обновляет только текущий процесс: память в байтах и загрузку CPU в 0–100 %.
    fn refresh(&mut self) -> (Option<u64>, Option<f32>) {
        let Some(pid) = self.pid else {
            return (None, None);
        };
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let Some(process) = self.system.process(pid) else {
            return (None, None);
        };
        // sysinfo суммирует загрузку по ядрам, поэтому значение делится на их число.
        let cpu_count = self.system.cpus().len().max(1) as f32;
        let cpu = (process.cpu_usage() / cpu_count).clamp(0.0, 100.0);
        (Some(process.memory()), Some(cpu))
    }
}

/// Оверлей с метриками в левом верхнем углу окна.
pub struct DebugOverlay {
    metrics: Metrics,
    system: SystemStats,
    cpu_name: String,
    gpu_name: String,
    vertices: Vec<OverlayVertex>,
}

impl DebugOverlay {
    pub fn new(gpu_name: &str, now: Instant) -> Self {
        let (system, cpu_name) = SystemStats::new();
        let mut overlay = Self {
            metrics: Metrics::new(now),
            system,
            cpu_name: display_name(&cpu_name),
            gpu_name: display_name(gpu_name),
            vertices: Vec::new(),
        };
        overlay.rebuild();
        overlay
    }

    /// Учитывает показанный кадр в счётчике FPS.
    pub fn frame_presented(&mut self) {
        self.metrics.frame_presented();
    }

    /// Учитывает новое GPU-время кадра и при наступлении срока делает выборку.
    ///
    /// Возвращает вершины оверлея для [`engine_render::Frame::overlay`].
    pub fn update(&mut self, now: Instant, gpu_time: Option<Duration>) -> &[OverlayVertex] {
        if let Some(time) = gpu_time {
            self.metrics.add_gpu_time(time);
        }
        if self.metrics.sample_due(now) {
            let (memory, cpu) = self.system.refresh();
            self.metrics.sample(now, memory, cpu);
            self.rebuild();
        }
        &self.vertices
    }

    fn rebuild(&mut self) {
        self.vertices.clear();
        build_panel(
            &mut self.vertices,
            &self.metrics,
            &self.cpu_name,
            &self.gpu_name,
        );
    }
}

/// Обрезает имя устройства и заменяет символы вне ASCII, которых нет в шрифте.
fn display_name(name: &str) -> String {
    let name = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect::<String>();
    if name.chars().count() <= MAX_NAME_CHARS {
        name
    } else {
        let mut short = name.chars().take(MAX_NAME_CHARS - 3).collect::<String>();
        short.push_str("...");
        short
    }
}

fn percent_text(label: &str, value: Option<f32>) -> String {
    value.map_or_else(
        || format!("{label} n/a"),
        |value| format!("{label} {value:.1}%"),
    )
}

/// Строит панель: фон, затем строки и графики сверху вниз.
fn build_panel(out: &mut Vec<OverlayVertex>, metrics: &Metrics, cpu_name: &str, gpu_name: &str) {
    // Фон рисуется первым, а его размер известен только после раскладки содержимого.
    let background = out.len();
    push_quad(out, 0.0, 0.0, 0.0, 0.0, PANEL_COLOR);
    let left = MARGIN + PADDING;
    let mut layout = Layout {
        out,
        left,
        y: MARGIN + PADDING,
        right: left,
    };
    let sample = metrics.latest;

    layout.text(&format!("{:.0} FPS", sample.fps), LARGE_SCALE);
    layout.graph(
        &metrics.fps,
        metrics.fps.max().max(FPS_GRAPH_FLOOR),
        FPS_COLOR,
    );

    let memory = sample.memory_bytes.map_or_else(
        || "RAM n/a".to_owned(),
        |bytes| format!("RAM {:.1} MB", bytes as f64 / (1024.0 * 1024.0)),
    );
    layout.text(&memory, TEXT_SCALE);
    layout.graph(&metrics.memory, metrics.memory.max(), RAM_COLOR);

    layout.text(&percent_text("CPU", sample.cpu_percent), TEXT_SCALE);
    layout.text(cpu_name, TEXT_SCALE);
    layout.graph(&metrics.cpu, 100.0, CPU_COLOR);

    layout.text(&percent_text("GPU", sample.gpu_percent), TEXT_SCALE);
    layout.text(gpu_name, TEXT_SCALE);
    layout.graph(&metrics.gpu, 100.0, GPU_COLOR);

    let width = layout.right + PADDING - MARGIN;
    let height = layout.y - SECTION_GAP + PADDING - MARGIN;
    let mut panel = Vec::with_capacity(6);
    push_quad(&mut panel, MARGIN, MARGIN, width, height, PANEL_COLOR);
    layout.out[background..background + 6].copy_from_slice(&panel);
}

/// Курсор раскладки: строки идут сверху вниз, `right` — правая граница содержимого.
struct Layout<'a> {
    out: &'a mut Vec<OverlayVertex>,
    left: f32,
    y: f32,
    right: f32,
}

impl Layout<'_> {
    fn text(&mut self, text: &str, scale: f32) {
        let width = push_text(self.out, self.left, self.y, scale, text, TEXT_COLOR);
        self.right = self.right.max(self.left + width);
        self.y += GLYPH_SIZE * scale + LINE_GAP;
    }

    /// Столбики истории на подложке: новые значения справа, `max` — верх шкалы.
    fn graph(&mut self, history: &History, max: f32, color: [f32; 4]) {
        push_quad(
            self.out,
            self.left,
            self.y,
            GRAPH_WIDTH,
            GRAPH_HEIGHT,
            GRAPH_BACKGROUND,
        );
        let bar_width = GRAPH_WIDTH / history.capacity as f32;
        let first_slot = history.capacity - history.values.len();
        let bottom = self.y + GRAPH_HEIGHT;
        for (index, &value) in history.values.iter().enumerate() {
            let fraction = if max > 0.0 {
                (value / max).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let height = fraction * GRAPH_HEIGHT;
            if height > 0.0 {
                let x = self.left + (first_slot + index) as f32 * bar_width;
                push_quad(self.out, x, bottom - height, bar_width, height, color);
            }
        }
        self.right = self.right.max(self.left + GRAPH_WIDTH);
        self.y += GRAPH_HEIGHT + SECTION_GAP;
    }
}

/// Рисует строку шрифтом 8x8 и возвращает её ширину в пикселях.
///
/// Каждая горизонтальная серия зажжённых пикселей глифа — один прямоугольник.
fn push_text(
    out: &mut Vec<OverlayVertex>,
    x: f32,
    y: f32,
    scale: f32,
    text: &str,
    color: [f32; 4],
) -> f32 {
    let advance = GLYPH_SIZE * scale;
    let mut count = 0;
    for (index, character) in text.chars().enumerate() {
        let code = if character.is_ascii() {
            character as usize
        } else {
            usize::from(b'?')
        };
        let glyph_x = x + index as f32 * advance;
        // В font8x8 младший бит строки — левый пиксель, строки идут сверху вниз.
        for (row, bits) in BASIC_LEGACY[code].iter().enumerate() {
            let mut column = 0;
            while column < 8 {
                if bits & (1 << column) == 0 {
                    column += 1;
                    continue;
                }
                let start = column;
                while column < 8 && bits & (1 << column) != 0 {
                    column += 1;
                }
                push_quad(
                    out,
                    glyph_x + start as f32 * scale,
                    y + row as f32 * scale,
                    (column - start) as f32 * scale,
                    scale,
                    color,
                );
            }
        }
        count = index + 1;
    }
    count as f32 * advance
}

/// Добавляет прямоугольник двумя треугольниками.
fn push_quad(
    out: &mut Vec<OverlayVertex>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: [f32; 4],
) {
    let [left, top, right, bottom] = [x, y, x + width, y + height];
    for position in [
        [left, top],
        [right, top],
        [left, bottom],
        [left, bottom],
        [right, top],
        [right, bottom],
    ] {
        out.push(OverlayVertex { position, color });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FPS_GRAPH_FLOOR, GRAPH_HEIGHT, GRAPH_WIDTH, History, LONG_HISTORY, MARGIN, MAX_NAME_CHARS,
        Metrics, SAMPLE_INTERVAL, SHORT_HISTORY, TEXT_COLOR, build_panel, display_name, push_text,
    };
    use engine_render::OverlayVertex;
    use std::time::{Duration, Instant};

    /// Прямоугольники `[x, y, width, height]` из вершин, записанных push_quad.
    fn quads(vertices: &[OverlayVertex]) -> Vec<[f32; 4]> {
        vertices
            .as_chunks::<6>()
            .0
            .iter()
            .map(|quad| {
                let [left, top] = quad[0].position;
                let [right, bottom] = quad[5].position;
                [left, top, right - left, bottom - top]
            })
            .collect()
    }

    #[test]
    fn history_keeps_only_latest_values() {
        let mut history = History::new(3);
        for value in [1.0, 5.0, 2.0, 3.0] {
            history.push(value);
        }
        assert_eq!(history.values, [5.0, 2.0, 3.0]);
        assert_eq!(history.max(), 5.0);
        assert_eq!(History::new(2).max(), 0.0);
    }

    #[test]
    fn samples_fps_and_gpu_share_over_the_window() {
        let start = Instant::now();
        let mut metrics = Metrics::new(start);
        for _ in 0..30 {
            metrics.frame_presented();
            metrics.add_gpu_time(Duration::from_millis(5));
        }
        assert!(!metrics.sample_due(start + SAMPLE_INTERVAL / 2));
        let now = start + SAMPLE_INTERVAL;
        assert!(metrics.sample_due(now));
        metrics.sample(now, Some(64 * 1024 * 1024), Some(12.5));

        let sample = metrics.latest;
        assert!((sample.fps - 60.0).abs() < 1e-3);
        assert!((sample.gpu_percent.unwrap() - 30.0).abs() < 1e-3);
        assert_eq!(sample.memory_bytes, Some(64 * 1024 * 1024));
        assert_eq!(sample.cpu_percent, Some(12.5));
        assert_eq!(metrics.fps.values.len(), 1);
        assert!(!metrics.sample_due(now));
    }

    #[test]
    fn sample_resets_window_and_reports_missing_gpu_time() {
        let start = Instant::now();
        let mut metrics = Metrics::new(start);
        metrics.add_gpu_time(Duration::from_secs(2));
        metrics.sample(start + SAMPLE_INTERVAL, None, None);
        assert_eq!(metrics.latest.gpu_percent, Some(100.0));

        metrics.sample(start + 2 * SAMPLE_INTERVAL, None, None);
        assert_eq!(metrics.latest.fps, 0.0);
        assert_eq!(metrics.latest.gpu_percent, None);
        assert_eq!(metrics.latest.memory_bytes, None);
        assert_eq!(metrics.gpu.values, [100.0, 0.0]);
    }

    #[test]
    fn histories_cover_thirty_seconds_and_two_minutes() {
        let start = Instant::now();
        let mut metrics = Metrics::new(start);
        for step in 1..=300 {
            metrics.sample(start + SAMPLE_INTERVAL * step, Some(1), Some(1.0));
        }
        let span = |samples: usize| SAMPLE_INTERVAL * samples as u32;
        assert_eq!(metrics.fps.values.len(), SHORT_HISTORY);
        assert_eq!(metrics.cpu.values.len(), SHORT_HISTORY);
        assert_eq!(metrics.gpu.values.len(), SHORT_HISTORY);
        assert_eq!(metrics.memory.values.len(), LONG_HISTORY);
        assert_eq!(span(SHORT_HISTORY), Duration::from_secs(30));
        assert_eq!(span(LONG_HISTORY), Duration::from_secs(120));
    }

    #[test]
    fn empty_text_has_no_geometry() {
        let mut vertices = Vec::new();
        assert_eq!(push_text(&mut vertices, 0.0, 0.0, 2.0, "", TEXT_COLOR), 0.0);
        assert!(vertices.is_empty());
    }

    #[test]
    fn glyph_rows_become_horizontal_runs() {
        // '8': 0x1E даёт одну серию, 0x33 — две; всего 11 серий в семи строках.
        let mut vertices = Vec::new();
        let width = push_text(&mut vertices, 0.0, 0.0, 1.0, "8", TEXT_COLOR);
        assert_eq!(width, 8.0);
        assert_eq!(vertices.len(), 11 * 6);
    }

    #[test]
    fn glyph_is_not_mirrored_or_flipped() {
        // 'L': стебель в столбцах 1–2 строк 1–5, основание — строка 6 из столбцов 0–6.
        let mut vertices = Vec::new();
        push_text(&mut vertices, 10.0, 20.0, 2.0, "L", TEXT_COLOR);
        let quads = quads(&vertices);
        assert!(quads.contains(&[12.0, 22.0, 4.0, 2.0]));
        assert!(quads.contains(&[10.0, 32.0, 14.0, 2.0]));
        assert!(
            quads
                .iter()
                .all(|&[x, y, w, h]| { x >= 10.0 && y >= 20.0 && x + w <= 26.0 && y + h <= 34.0 })
        );
    }

    #[test]
    fn non_ascii_characters_render_as_question_marks() {
        let mut expected = Vec::new();
        push_text(&mut expected, 0.0, 0.0, 1.0, "?", TEXT_COLOR);
        let mut vertices = Vec::new();
        push_text(&mut vertices, 0.0, 0.0, 1.0, "Ж", TEXT_COLOR);
        assert_eq!(vertices, expected);
    }

    #[test]
    fn device_names_are_ascii_and_truncated() {
        assert_eq!(
            display_name("  NVIDIA GeForce RTX 5070 Ti "),
            "NVIDIA GeForce RTX 5070 Ti"
        );
        assert_eq!(display_name("Intel® Core™"), "Intel? Core?");
        let long = display_name(&"x".repeat(50));
        assert_eq!(long.chars().count(), MAX_NAME_CHARS);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn panel_encloses_all_geometry_in_top_left_corner() {
        let start = Instant::now();
        let mut metrics = Metrics::new(start);
        for step in 1..=10 {
            for _ in 0..step * 10 {
                metrics.frame_presented();
            }
            metrics.add_gpu_time(Duration::from_millis(step as u64));
            metrics.sample(
                start + SAMPLE_INTERVAL * step,
                Some(u64::from(step) << 20),
                Some(50.0),
            );
        }
        let mut vertices = Vec::new();
        build_panel(
            &mut vertices,
            &metrics,
            "Test CPU",
            &"G".repeat(MAX_NAME_CHARS),
        );

        assert!(vertices.len().is_multiple_of(3));
        let panel = quads(&vertices)[0];
        assert_eq!([panel[0], panel[1]], [MARGIN, MARGIN]);
        assert!(panel[2] >= GRAPH_WIDTH && panel[3] >= 4.0 * GRAPH_HEIGHT);
        for vertex in &vertices {
            let [x, y] = vertex.position;
            assert!(x >= panel[0] && x <= panel[0] + panel[2], "{x}");
            assert!(y >= panel[1] && y <= panel[1] + panel[3], "{y}");
        }
        // Фон полупрозрачный чёрный и рисуется первым.
        assert_eq!(vertices[0].color, [0.0, 0.0, 0.0, 0.6]);
    }

    #[test]
    fn fps_graph_scale_has_a_floor() {
        let start = Instant::now();
        let mut metrics = Metrics::new(start);
        for _ in 0..15 {
            metrics.frame_presented();
        }
        // 30 FPS при нижней границе шкалы 60 даёт столбик в половину высоты графика.
        metrics.sample(start + SAMPLE_INTERVAL, None, None);
        let mut vertices = Vec::new();
        build_panel(&mut vertices, &metrics, "", "");
        let bar_height = GRAPH_HEIGHT * 30.0 / FPS_GRAPH_FLOOR;
        assert!(
            quads(&vertices)
                .iter()
                .any(|&[_, _, _, height]| (height - bar_height).abs() < 1e-3)
        );
    }
}
