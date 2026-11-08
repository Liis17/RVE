use crate::mcp::McpStatus;
use crate::settings::McpSettings;

#[derive(Default)]
pub struct Preferences {
    open: bool,
    draft: McpSettings,
    pending: Option<McpSettings>,
    error: Option<String>,
}

impl Preferences {
    pub fn open(&mut self, settings: &McpSettings) {
        self.draft = settings.clone();
        self.error = None;
        self.open = true;
    }

    pub fn take_pending(&mut self) -> Option<McpSettings> {
        self.pending.take()
    }

    pub fn applied(&mut self, settings: &McpSettings) {
        self.draft = settings.clone();
    }
}

pub fn show(ctx: &egui::Context, preferences: &mut Preferences, status: &McpStatus) {
    if !preferences.open {
        return;
    }
    let mut open = preferences.open;
    egui::Window::new("Preferences")
        .id(egui::Id::new("editor_preferences"))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.heading("MCP Server");
            ui.checkbox(&mut preferences.draft.enabled, "Enable local MCP server");
            ui.horizontal(|ui| {
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut preferences.draft.port).range(1024..=u16::MAX));
            });
            ui.label(format!("Status: {}", status.label()));

            ui.horizontal(|ui| {
                ui.label("Bearer token");
                ui.add_sized(
                    [ui.available_width() - 110.0, 24.0],
                    egui::TextEdit::singleline(&mut preferences.draft.token).password(true),
                );
                if ui
                    .add_enabled(
                        !preferences.draft.token.is_empty(),
                        egui::Button::new("Copy"),
                    )
                    .clicked()
                {
                    ctx.copy_text(preferences.draft.token.clone());
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Regenerate").clicked()
                    && let Err(error) = preferences.draft.regenerate_token()
                {
                    preferences.error = Some(error);
                }
                if ui
                    .add_enabled(
                        !preferences.draft.token.is_empty(),
                        egui::Button::new("Copy connection command"),
                    )
                    .clicked()
                {
                    ctx.copy_text(connection_command(&preferences.draft));
                }
            });

            ui.separator();
            ui.label("Permissions");
            ui.checkbox(&mut preferences.draft.permissions.edit_scene, "Edit scene");
            ui.checkbox(&mut preferences.draft.permissions.assets, "Write assets");
            ui.checkbox(&mut preferences.draft.permissions.play, "Control Play");
            ui.checkbox(&mut preferences.draft.permissions.build, "Build");

            if let Some(error) = &preferences.error {
                ui.colored_label(egui::Color32::RED, error);
            }
            if ui.button("Apply").clicked() {
                if preferences.draft.port < 1024 {
                    preferences.error = Some("Port must be between 1024 and 65535".to_owned());
                } else {
                    preferences.pending = Some(preferences.draft.clone());
                    preferences.error = None;
                }
            }
        });
    preferences.open = open;
}

fn connection_command(settings: &McpSettings) -> String {
    format!(
        "claude mcp add --transport http rve-editor http://127.0.0.1:{}/mcp --header \"Authorization: Bearer {}\"",
        settings.port, settings.token
    )
}
