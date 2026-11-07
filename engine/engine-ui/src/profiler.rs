//! Таблица профайлера для egui: группы строк с последним, средним и максимальным временем в миллисекундах.

use std::sync::Arc;
use std::time::Duration;

use egui::{Align2, Color32, Context, Rect, RichText, Sense, Ui, Vec2, Window};
use engine_core::profile::{ProfileReport, ProfileValue, StatSummary};

const BAR_WIDTH: f32 = 96.0;
const BAR_HEIGHT: f32 = 8.0;

/// Рисует отчёт: заголовок группы, строка на метку и полоса среднего относительно максимума отчёта.
/// Счётчики и одиночные значения занимают колонку последнего значения.
pub fn show(ui: &mut Ui, report: &ProfileReport) {
    let scale = report_scale(report);
    for group in &report.groups {
        ui.label(RichText::new(group.name).strong());
        egui::Grid::new(("engine_profiler", group.name))
            .num_columns(5)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                for header in [
                    "metric",
                    "last, ms / count",
                    "avg, ms",
                    "max, ms",
                    "avg / max",
                ] {
                    ui.label(RichText::new(header).weak());
                }
                ui.end_row();
                for row in &group.rows {
                    ui.label(row.label);
                    match row.value {
                        ProfileValue::Timing(summary) => show_summary(ui, summary, scale),
                        ProfileValue::Single(value) => {
                            ui.monospace(format_ms(value));
                            show_empty_cells(ui, 3);
                        }
                        ProfileValue::Count(value) => {
                            ui.monospace(value.to_string());
                            show_empty_cells(ui, 3);
                        }
                        ProfileValue::Missing => {
                            ui.label("n/a");
                            show_empty_cells(ui, 3);
                        }
                    }
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
    }
}

/// Оверлей профайлера в правом верхнем углу; показывает отчёты источников подряд.
pub fn overlay(ctx: &Context, reports: &[Arc<ProfileReport>]) {
    let _ = Window::new("Profiler")
        .id(egui::Id::new("engine_profiler_overlay"))
        .anchor(Align2::RIGHT_TOP, [-8.0, 8.0])
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            for report in reports {
                show(ui, report);
            }
        });
}

fn show_summary(ui: &mut Ui, summary: StatSummary, scale: Duration) {
    ui.monospace(format_ms(summary.last));
    ui.monospace(format_ms(summary.avg));
    ui.monospace(format_ms(summary.max));
    let fraction = bar_fraction(summary.avg, scale);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(BAR_WIDTH, BAR_HEIGHT), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, Color32::from_gray(60));
    let filled = Rect::from_min_size(rect.min, Vec2::new(rect.width() * fraction, rect.height()));
    ui.painter()
        .rect_filled(filled, 2.0, Color32::from_rgb(90, 160, 220));
}

fn show_empty_cells(ui: &mut Ui, count: usize) {
    for _ in 0..count {
        ui.label("");
    }
}

/// Самое большое максимальное значение отчёта; общая шкала для полос строк.
fn report_scale(report: &ProfileReport) -> Duration {
    report
        .groups
        .iter()
        .flat_map(|group| group.rows.iter())
        .filter_map(|row| match row.value {
            ProfileValue::Timing(summary) => Some(summary.max),
            ProfileValue::Single(value) => Some(value),
            ProfileValue::Count(_) | ProfileValue::Missing => None,
        })
        .max()
        .unwrap_or(Duration::ZERO)
}

/// Доля полосы от нуля до единицы; нулевая шкала даёт пустую полосу.
pub fn bar_fraction(value: Duration, scale: Duration) -> f32 {
    if scale.is_zero() {
        return 0.0;
    }
    (value.as_secs_f32() / scale.as_secs_f32()).clamp(0.0, 1.0)
}

/// Миллисекунды с тремя знаками после запятой.
pub fn format_ms(value: Duration) -> String {
    format!("{:.3}", value.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::{bar_fraction, format_ms};
    use std::time::Duration;

    #[test]
    fn bar_fraction_is_clamped_and_empty_for_zero_scale() {
        let scale = Duration::from_millis(4);
        assert_eq!(bar_fraction(Duration::from_millis(1), scale), 0.25);
        assert_eq!(bar_fraction(Duration::from_millis(9), scale), 1.0);
        assert_eq!(bar_fraction(Duration::from_millis(1), Duration::ZERO), 0.0);
    }

    #[test]
    fn milliseconds_keep_three_decimals() {
        assert_eq!(format_ms(Duration::from_micros(1_500)), "1.500");
        assert_eq!(format_ms(Duration::ZERO), "0.000");
    }
}
