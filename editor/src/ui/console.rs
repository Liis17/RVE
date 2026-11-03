//! Панель Console: журнал редактора и скриптов с фильтром по уровню.

use crate::console::{LogEntry, LogLevel};
use crate::state::EditorState;
use egui::{Color32, RichText};

/// Какие уровни показываются; состояние панели хранится в памяти egui.
#[derive(Clone, Copy)]
struct Filter {
    info: bool,
    warn: bool,
    error: bool,
}

impl Filter {
    fn accepts(self, level: LogLevel) -> bool {
        match level {
            LogLevel::Info => self.info,
            LogLevel::Warn => self.warn,
            LogLevel::Error => self.error,
        }
    }
}

pub fn entry_text(entry: &LogEntry) -> RichText {
    let text = RichText::new(&entry.message);
    match entry.level {
        LogLevel::Info => text,
        LogLevel::Warn => text.color(Color32::from_rgb(230, 190, 80)),
        LogLevel::Error => text.color(Color32::from_rgb(235, 100, 90)),
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let id = ui.id().with("console_filter");
    let mut filter = ui
        .ctx()
        .data(|data| data.get_temp::<Filter>(id))
        .unwrap_or(Filter {
            info: true,
            warn: true,
            error: true,
        });
    ui.horizontal(|ui| {
        if ui.button("Clear").clicked() {
            state.log.clear();
        }
        ui.separator();
        ui.toggle_value(&mut filter.info, "Info");
        ui.toggle_value(&mut filter.warn, "Warn");
        ui.toggle_value(&mut filter.error, "Error");
    });
    ui.ctx().data_mut(|data| data.insert_temp(id, filter));
    ui.separator();
    egui::ScrollArea::both()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in state
                .log
                .entries()
                .filter(|entry| filter.accepts(entry.level))
            {
                ui.label(entry_text(entry));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_selects_levels_independently() {
        let filter = Filter {
            info: false,
            warn: true,
            error: false,
        };
        assert!(!filter.accepts(LogLevel::Info));
        assert!(filter.accepts(LogLevel::Warn));
        assert!(!filter.accepts(LogLevel::Error));
    }
}
