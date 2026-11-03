//! egui overlay for script and engine console output.

use egui::{
    Color32, Context, Event, FontId, Frame, Id, Key, Modifiers, Order, Rect, RichText, ScrollArea,
    TextEdit, Vec2, Window, pos2,
};
use engine_console::{CommandCatalog, CommandHistory, CommandSpec, ConsoleLevel, ConsoleLine};

const PANEL_HEIGHT_FRACTION: f32 = 0.45;
const PAGE_SCROLL_FACTOR: f32 = 0.8;

/// Result of one console overlay frame.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConsoleOutput {
    /// Submitted command text; the caller owns parsing and execution.
    pub submitted: Option<String>,
    /// Whether the overlay changed between open and closed.
    pub toggled: bool,
}

/// Console view state. Scrollback and command data stay owned by the caller.
pub struct ConsoleOverlay {
    open: bool,
    input: String,
    history: CommandHistory,
    selected_suggestion: Option<usize>,
    scroll_offset: Vec2,
    stick_to_bottom: bool,
}

impl Default for ConsoleOverlay {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            history: CommandHistory::default(),
            selected_suggestion: None,
            scroll_offset: Vec2::ZERO,
            stick_to_bottom: true,
        }
    }
}

impl ConsoleOverlay {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_open(&mut self, open: bool) {
        self.open = open;
    }

    /// Shows the console over the top 45 percent of `rect`.
    pub fn show(
        &mut self,
        ctx: &Context,
        rect: Rect,
        lines: &[ConsoleLine],
        catalog: &CommandCatalog,
    ) -> ConsoleOutput {
        let mut output = ConsoleOutput::default();
        if take_toggle_key(ctx) {
            self.open = !self.open;
            output.toggled = true;
        }
        if !self.open {
            return output;
        }

        if consume_key(ctx, Key::Escape) {
            self.open = false;
            output.toggled = true;
            return output;
        }

        let tab = consume_key(ctx, Key::Tab);
        let enter = consume_key(ctx, Key::Enter);
        let up = consume_key(ctx, Key::ArrowUp);
        let down = consume_key(ctx, Key::ArrowDown);
        let page_up = consume_key(ctx, Key::PageUp);
        let page_down = consume_key(ctx, Key::PageDown);

        if tab {
            self.complete(catalog);
        }
        if up || down {
            self.navigate(up, catalog);
        }

        let panel_height = rect.height() * PANEL_HEIGHT_FRACTION;
        let panel = Rect::from_min_max(rect.min, pos2(rect.max.x, rect.min.y + panel_height));
        if page_up {
            self.stick_to_bottom = false;
            self.scroll_offset.y =
                (self.scroll_offset.y - panel_height * PAGE_SCROLL_FACTOR).max(0.0);
        } else if page_down {
            self.stick_to_bottom = false;
            self.scroll_offset.y += panel_height * PAGE_SCROLL_FACTOR;
        }

        let frame = Frame::new()
            .fill(Color32::from_rgba_unmultiplied(12, 15, 20, 224))
            .stroke(egui::Stroke::new(1.0, Color32::from_gray(76)))
            .inner_margin(8.0);
        let _ = Window::new("Developer console")
            .id(Id::new("engine_developer_console"))
            .order(Order::Foreground)
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .fixed_rect(panel)
            .frame(frame)
            .show(ctx, |ui| {
                let scroll_height = (panel.height() - 80.0).max(0.0);
                let scroll = ScrollArea::vertical()
                    .id_salt("engine_developer_console_scrollback")
                    .auto_shrink([false, false])
                    .max_height(scroll_height)
                    .scroll_offset(self.scroll_offset)
                    .stick_to_bottom(self.stick_to_bottom)
                    .show(ui, |ui| {
                        for line in lines {
                            let color = match line.level {
                                ConsoleLevel::Info => Color32::from_gray(218),
                                ConsoleLevel::Warn => Color32::from_rgb(239, 194, 96),
                                ConsoleLevel::Error => Color32::from_rgb(244, 115, 115),
                            };
                            ui.label(
                                RichText::new(format!("[{}] {}", line.source, line.text))
                                    .font(FontId::monospace(13.0))
                                    .color(color),
                            );
                        }
                    });
                self.scroll_offset = scroll.state.offset;
                let max_offset = (scroll.content_size.y - scroll.inner_rect.height()).max(0.0);
                if self.scroll_offset.y >= max_offset - 1.0 {
                    self.stick_to_bottom = true;
                }

                ui.horizontal(|ui| {
                    ui.label(RichText::new("> ").font(FontId::monospace(14.0)));
                    let input = ui.add(
                        TextEdit::singleline(&mut self.input)
                            .id(Id::new("engine_developer_console_input"))
                            .font(FontId::monospace(14.0))
                            .desired_width(f32::INFINITY)
                            .hint_text("command"),
                    );
                    input.request_focus();
                    if input.changed() {
                        self.selected_suggestion = None;
                    }
                });

                let suggestions = command_suggestions(&self.input, catalog);
                if !suggestions.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        for (index, suggestion) in suggestions.iter().enumerate() {
                            let color = if self.selected_suggestion == Some(index) {
                                Color32::from_rgb(126, 190, 255)
                            } else {
                                Color32::from_gray(160)
                            };
                            ui.label(
                                RichText::new(format!(
                                    "{} — {}",
                                    suggestion.name, suggestion.description
                                ))
                                .font(FontId::monospace(11.0))
                                .color(color),
                            );
                        }
                    });
                }
            });
        if enter {
            let submitted = self.input.trim().to_owned();
            if !submitted.is_empty() {
                self.history.push(submitted.clone());
                output.submitted = Some(submitted);
                self.input.clear();
                self.selected_suggestion = None;
                self.scroll_offset = Vec2::ZERO;
                self.stick_to_bottom = true;
            }
        }
        output
    }

    fn complete(&mut self, catalog: &CommandCatalog) {
        let Some((leading, token, suffix)) = command_token(&self.input) else {
            return;
        };
        if token.is_empty() {
            return;
        }
        let suggestions = catalog.complete(token);
        if suggestions.is_empty() {
            return;
        }
        let common = catalog.common_prefix(token);
        if common.len() > token.len() {
            self.input = format!("{leading}{common}{suffix}");
            self.selected_suggestion = None;
        } else if suggestions.len() == 1 && suggestions[0].name == token && suffix.is_empty() {
            self.input.push(' ');
            self.selected_suggestion = None;
        }
    }

    fn navigate(&mut self, up: bool, catalog: &CommandCatalog) {
        let suggestions = command_suggestions(&self.input, catalog);
        if !suggestions.is_empty() {
            let index = match self.selected_suggestion {
                Some(index) if up => (index + suggestions.len() - 1) % suggestions.len(),
                Some(index) => (index + 1) % suggestions.len(),
                None if up => suggestions.len() - 1,
                None => 0,
            };
            if let Some((leading, _, suffix)) = command_token(&self.input) {
                self.input = format!("{leading}{}{suffix}", suggestions[index].name);
            }
            self.selected_suggestion = Some(index);
            return;
        }

        let history = if up {
            self.history.previous()
        } else {
            self.history.following()
        };
        if let Some(history) = history {
            self.input = history.to_owned();
        }
    }
}

fn take_toggle_key(ctx: &Context) -> bool {
    ctx.input_mut(|input| {
        let mut toggled = false;
        let mut suppress_text = false;
        input.events.retain(|event| match event {
            Event::Key {
                physical_key: Some(Key::Backtick),
                pressed,
                repeat,
                ..
            } => {
                toggled |= *pressed && !*repeat;
                suppress_text = *pressed;
                false
            }
            Event::Text(_) if suppress_text => {
                suppress_text = false;
                false
            }
            other => {
                if !matches!(other, Event::Text(_)) {
                    suppress_text = false;
                }
                true
            }
        });
        toggled
    })
}

fn consume_key(ctx: &Context, key: Key) -> bool {
    ctx.input_mut(|input| input.consume_key(Modifiers::NONE, key))
}

fn command_suggestions(input: &str, catalog: &CommandCatalog) -> Vec<CommandSpec> {
    let Some((_, token, suffix)) = command_token(input) else {
        return Vec::new();
    };
    if token.is_empty() || !suffix.is_empty() {
        return Vec::new();
    }
    catalog.complete(token)
}

fn command_token(input: &str) -> Option<(&str, &str, &str)> {
    let leading_len = input.len() - input.trim_start().len();
    let leading = &input[..leading_len];
    let trimmed = &input[leading_len..];
    let token_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    Some((leading, &trimmed[..token_end], &trimmed[token_end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{RawInput, pos2, vec2};
    use engine_console::{ConsoleBuffer, ConsoleLine};

    fn catalog() -> CommandCatalog {
        let mut catalog = CommandCatalog::default();
        catalog
            .register(CommandSpec::new("foo.bar", "first command").unwrap())
            .unwrap();
        catalog
            .register(CommandSpec::new("foo.baz", "second command").unwrap())
            .unwrap();
        catalog
    }

    fn frame(ctx: &Context, overlay: &mut ConsoleOverlay, events: Vec<Event>) -> ConsoleOutput {
        frame_with_lines(ctx, overlay, events, &[])
    }

    fn frame_with_lines(
        ctx: &Context,
        overlay: &mut ConsoleOverlay,
        events: Vec<Event>,
        lines: &[ConsoleLine],
    ) -> ConsoleOutput {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let input = RawInput {
            screen_rect: Some(rect),
            events,
            ..Default::default()
        };
        let mut output = ConsoleOutput::default();
        let mut full_output = ctx.run_ui(input, |_ui| {
            output = overlay.show(ctx, rect, lines, &catalog());
        });
        full_output.textures_delta.clear();
        output
    }

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn backquote() -> Event {
        Event::Key {
            key: Key::Semicolon,
            physical_key: Some(Key::Backtick),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn backquote_release() -> Event {
        Event::Key {
            key: Key::Semicolon,
            physical_key: Some(Key::Backtick),
            pressed: false,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    fn text(value: &str) -> Event {
        Event::Text(value.to_owned())
    }

    #[test]
    fn physical_backquote_toggles_and_suppresses_layout_text() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        let output = frame(&ctx, &mut overlay, vec![backquote(), text("ё")]);
        assert!(output.toggled);
        assert!(overlay.is_open());
        assert!(overlay.input.is_empty());

        frame(&ctx, &mut overlay, vec![backquote_release()]);
        let output = frame(&ctx, &mut overlay, vec![backquote(), text("ё")]);
        assert!(output.toggled);
        assert!(!overlay.is_open());
        assert!(overlay.input.is_empty());
    }

    #[test]
    fn tab_completes_the_common_command_prefix() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        frame(&ctx, &mut overlay, vec![backquote()]);
        frame(&ctx, &mut overlay, vec![text("foo.b")]);
        frame(&ctx, &mut overlay, vec![key(Key::Tab)]);
        assert_eq!(overlay.input, "foo.ba");
    }

    #[test]
    fn arrows_choose_suggestions_then_navigate_command_history() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        frame(&ctx, &mut overlay, vec![backquote()]);
        frame(&ctx, &mut overlay, vec![text("foo.b")]);
        frame(&ctx, &mut overlay, vec![key(Key::ArrowDown)]);
        assert_eq!(overlay.input, "foo.bar");
        frame(&ctx, &mut overlay, vec![key(Key::Enter)]);
        frame(&ctx, &mut overlay, vec![text("help")]);
        frame(&ctx, &mut overlay, vec![key(Key::Enter)]);
        frame(&ctx, &mut overlay, vec![key(Key::ArrowUp)]);
        assert_eq!(overlay.input, "help");
    }

    #[test]
    fn enter_submits_and_clears_the_input_and_escape_closes() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        frame(&ctx, &mut overlay, vec![backquote()]);
        frame(&ctx, &mut overlay, vec![text("clear")]);
        let output = frame(&ctx, &mut overlay, vec![key(Key::Enter)]);
        assert_eq!(output.submitted.as_deref(), Some("clear"));
        assert!(overlay.input.is_empty());
        assert_eq!(overlay.history.len(), 1);

        let output = frame(&ctx, &mut overlay, vec![key(Key::Escape)]);
        assert!(output.toggled);
        assert!(!overlay.is_open());
    }

    #[test]
    fn overlay_uses_caller_owned_scrollback_without_mutating_it() {
        let lines = vec![ConsoleLine {
            level: ConsoleLevel::Info,
            source: "scene/object/script".to_owned(),
            text: "hello ё".to_owned(),
        }];
        let mut buffer = ConsoleBuffer::with_capacity(1);
        buffer.push(lines[0].clone());
        let before = buffer.snapshot();
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        frame(&ctx, &mut overlay, vec![backquote()]);
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let mut output = ctx.run_ui(RawInput::default(), |_ui| {
            overlay.show(&ctx, rect, &lines, &catalog());
        });
        output.textures_delta.clear();
        assert_eq!(buffer.snapshot(), before);
    }

    #[test]
    fn page_keys_move_the_scrollback_while_new_lines_stick_to_the_bottom() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        let lines: Vec<_> = (0..80)
            .map(|index| ConsoleLine {
                level: ConsoleLevel::Info,
                source: "scene".to_owned(),
                text: format!("line {index}"),
            })
            .collect();
        frame_with_lines(&ctx, &mut overlay, vec![backquote()], &lines);
        let bottom = overlay.scroll_offset.y;
        assert!(bottom > 0.0);

        frame_with_lines(&ctx, &mut overlay, vec![key(Key::PageUp)], &lines);
        let up = overlay.scroll_offset.y;
        assert!(up < bottom);

        frame_with_lines(&ctx, &mut overlay, vec![key(Key::PageDown)], &lines);
        assert!(overlay.scroll_offset.y > up);
    }
}
