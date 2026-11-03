//! egui overlay for script and engine console output.

use egui::{
    Area, Color32, Context, Event, FontId, Frame, Id, Key, Modifiers, Order, Rect, RichText,
    ScrollArea, TextEdit, Vec2, Window, pos2,
};
use engine_console::{CommandCatalog, CommandHistory, CommandSpec, ConsoleLevel, ConsoleLine};

const PANEL_HEIGHT_FRACTION: f32 = 0.45;
const PAGE_SCROLL_FACTOR: f32 = 0.8;
const MAX_SUGGESTION_POPUP_HEIGHT: f32 = 120.0;
const SUGGESTION_ROW_HEIGHT: f32 = 22.0;

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
    completion_query: Option<String>,
    selection_from_tab: bool,
    history_browsing: bool,
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
            completion_query: None,
            selection_from_tab: false,
            history_browsing: false,
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
        if !open {
            self.clear_interaction_mode();
        }
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
            if !self.open {
                self.clear_interaction_mode();
            }
            output.toggled = true;
        }
        if !self.open {
            return output;
        }

        if consume_key(ctx, Key::Escape) {
            self.open = false;
            self.clear_interaction_mode();
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
        let mut input_rect = Rect::NOTHING;
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
                    input_rect = input.rect;
                    if input.changed() {
                        self.clear_interaction_mode();
                        self.history.reset_navigation();
                    }
                });
            });

        let suggestions = self.current_suggestions(catalog);
        if !suggestions.is_empty() && input_rect.is_positive() {
            let popup = suggestion_popup_rect(panel, input_rect, suggestions.len());
            if popup.is_positive() {
                let mut clicked_suggestion = None;
                Area::new(Id::new("engine_developer_console_suggestions"))
                    .order(Order::Foreground)
                    .fixed_pos(popup.min)
                    .default_size(popup.size())
                    .constrain_to(panel)
                    .show(ctx, |ui| {
                        ui.set_min_size(popup.size());
                        ui.set_max_size(popup.size());
                        ui.painter().rect_filled(
                            ui.max_rect(),
                            4.0,
                            Color32::from_rgba_unmultiplied(20, 24, 31, 248),
                        );
                        ScrollArea::vertical()
                            .id_salt("engine_developer_console_suggestions_scroll")
                            .auto_shrink([false, false])
                            .max_width(popup.width())
                            .max_height(popup.height())
                            .show(ui, |ui| {
                                ui.set_min_width(popup.width());
                                for (index, suggestion) in suggestions.iter().enumerate() {
                                    let selected = self.selected_suggestion == Some(index);
                                    if ui
                                        .selectable_label(
                                            selected,
                                            RichText::new(format!(
                                                "{} — {}",
                                                suggestion.name, suggestion.description
                                            ))
                                            .font(FontId::monospace(11.0))
                                            .color(
                                                if selected {
                                                    Color32::from_rgb(126, 190, 255)
                                                } else {
                                                    Color32::from_gray(190)
                                                },
                                            ),
                                        )
                                        .clicked()
                                    {
                                        clicked_suggestion = Some(index);
                                    }
                                }
                            });
                    });
                if let Some(index) = clicked_suggestion {
                    let query = self
                        .completion_query
                        .clone()
                        .unwrap_or_else(|| self.input.clone());
                    self.select_suggestion(&query, &suggestions, index, false);
                }
            }
        }
        if enter {
            let submitted = self.input.trim().to_owned();
            if !submitted.is_empty() {
                self.history.push(submitted.clone());
                output.submitted = Some(submitted);
                self.input.clear();
                self.scroll_offset = Vec2::ZERO;
                self.stick_to_bottom = true;
            }
            self.clear_interaction_mode();
        }
        output
    }

    fn complete(&mut self, catalog: &CommandCatalog) {
        if self.history_browsing {
            return;
        }
        if let Some(index) = self.selected_suggestion {
            let query = self
                .completion_query
                .clone()
                .unwrap_or_else(|| self.input.clone());
            let suggestions = command_suggestions(&query, catalog);
            if self.selection_from_tab {
                if !suggestions.is_empty() {
                    self.select_suggestion(
                        &query,
                        &suggestions,
                        (index + 1) % suggestions.len(),
                        true,
                    );
                }
            } else if let Some(suggestion) = suggestions.get(index)
                && let Some((leading, _, suffix)) = command_token(&query)
            {
                self.input = format!("{leading}{}{suffix} ", suggestion.name);
                self.clear_interaction_mode();
            }
            return;
        }

        let query = self
            .completion_query
            .clone()
            .unwrap_or_else(|| self.input.clone());
        let Some((leading, token, suffix)) = command_token(&query) else {
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
        let current_token = command_token(&self.input).map_or("", |(_, current, _)| current);
        if common.len() > current_token.len() && common.starts_with(current_token) {
            self.completion_query = Some(query.clone());
            self.input = format!("{leading}{common}{suffix}");
        } else if suggestions.len() == 1
            && suggestions[0].name == current_token
            && suffix.is_empty()
        {
            self.input.push(' ');
            self.clear_interaction_mode();
        } else {
            self.select_suggestion(&query, &suggestions, 0, true);
        }
    }

    fn navigate(&mut self, up: bool, catalog: &CommandCatalog) {
        if self.history_browsing {
            self.navigate_history(up);
            return;
        }

        let query = self
            .completion_query
            .clone()
            .unwrap_or_else(|| self.input.clone());
        let suggestions = command_suggestions(&query, catalog);
        if !suggestions.is_empty() {
            let index = match self.selected_suggestion {
                Some(index) if up => (index + suggestions.len() - 1) % suggestions.len(),
                Some(index) => (index + 1) % suggestions.len(),
                None if up => suggestions.len() - 1,
                None => 0,
            };
            self.select_suggestion(&query, &suggestions, index, false);
            return;
        }

        self.navigate_history(up);
    }

    fn navigate_history(&mut self, up: bool) {
        let history = if up {
            self.history.previous()
        } else {
            self.history.following()
        };
        if let Some(history) = history {
            self.history_browsing = true;
            self.completion_query = None;
            self.selected_suggestion = None;
            self.selection_from_tab = false;
            self.input = history.to_owned();
        } else {
            self.history_browsing = false;
        }
    }

    fn current_suggestions(&self, catalog: &CommandCatalog) -> Vec<CommandSpec> {
        if self.history_browsing {
            return Vec::new();
        }
        command_suggestions(
            self.completion_query
                .as_deref()
                .unwrap_or(self.input.as_str()),
            catalog,
        )
    }

    fn select_suggestion(
        &mut self,
        query: &str,
        suggestions: &[CommandSpec],
        index: usize,
        from_tab: bool,
    ) {
        let Some(suggestion) = suggestions.get(index) else {
            return;
        };
        if let Some((leading, _, suffix)) = command_token(query) {
            self.completion_query = Some(query.to_owned());
            self.input = format!("{leading}{}{suffix}", suggestion.name);
            self.selected_suggestion = Some(index);
            self.selection_from_tab = from_tab;
            self.history_browsing = false;
        }
    }

    fn clear_interaction_mode(&mut self) {
        self.selected_suggestion = None;
        self.completion_query = None;
        self.selection_from_tab = false;
        self.history_browsing = false;
    }
}

fn suggestion_popup_rect(panel: Rect, input: Rect, suggestion_count: usize) -> Rect {
    let width = input.width().min(panel.width());
    let left = input
        .left()
        .clamp(panel.left(), (panel.right() - width).max(panel.left()));
    let bottom = input.top().clamp(panel.top(), panel.bottom());
    let available_height = (bottom - panel.top()).max(0.0);
    let desired_height =
        (suggestion_count as f32 * SUGGESTION_ROW_HEIGHT).min(MAX_SUGGESTION_POPUP_HEIGHT);
    let height = desired_height.min(available_height);
    Rect::from_min_max(pos2(left, bottom - height), pos2(left + width, bottom))
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
        frame_with_lines_and_catalog(ctx, overlay, events, lines, &catalog())
    }

    fn frame_with_catalog(
        ctx: &Context,
        overlay: &mut ConsoleOverlay,
        events: Vec<Event>,
        command_catalog: &CommandCatalog,
    ) -> ConsoleOutput {
        frame_with_lines_and_catalog(ctx, overlay, events, &[], command_catalog)
    }

    fn frame_with_lines_and_catalog(
        ctx: &Context,
        overlay: &mut ConsoleOverlay,
        events: Vec<Event>,
        lines: &[ConsoleLine],
        command_catalog: &CommandCatalog,
    ) -> ConsoleOutput {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let input = RawInput {
            screen_rect: Some(rect),
            events,
            ..Default::default()
        };
        let mut output = ConsoleOutput::default();
        let mut full_output = ctx.run_ui(input, |_ui| {
            output = overlay.show(ctx, rect, lines, command_catalog);
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
    fn repeated_arrows_cycle_the_original_suggestion_query() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        let command_catalog = catalog();
        frame_with_catalog(&ctx, &mut overlay, vec![backquote()], &command_catalog);
        frame_with_catalog(&ctx, &mut overlay, vec![text("foo.b")], &command_catalog);

        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowDown)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.bar");
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowDown)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.baz");
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowUp)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.bar");
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowUp)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.baz");
    }

    #[test]
    fn history_arrows_keep_browsing_history_for_registered_command_names() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        let command_catalog = catalog();
        frame_with_catalog(&ctx, &mut overlay, vec![backquote()], &command_catalog);
        for command in ["foo.bar", "foo.baz"] {
            frame_with_catalog(&ctx, &mut overlay, vec![text(command)], &command_catalog);
            let output =
                frame_with_catalog(&ctx, &mut overlay, vec![key(Key::Enter)], &command_catalog);
            assert_eq!(output.submitted.as_deref(), Some(command));
        }

        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowUp)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.baz");
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowUp)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.bar");
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowDown)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.baz");
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowDown)],
            &command_catalog,
        );
        assert!(overlay.input.is_empty());
    }

    #[test]
    fn repeated_tab_cycles_ambiguous_suggestions() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        let command_catalog = catalog();
        frame_with_catalog(&ctx, &mut overlay, vec![backquote()], &command_catalog);
        frame_with_catalog(&ctx, &mut overlay, vec![text("foo.b")], &command_catalog);

        for expected in ["foo.ba", "foo.bar", "foo.baz"] {
            frame_with_catalog(&ctx, &mut overlay, vec![key(Key::Tab)], &command_catalog);
            assert_eq!(overlay.input, expected);
        }
    }

    #[test]
    fn tab_accepts_an_arrow_selected_suggestion() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        let command_catalog = catalog();
        frame_with_catalog(&ctx, &mut overlay, vec![backquote()], &command_catalog);
        frame_with_catalog(&ctx, &mut overlay, vec![text("foo.b")], &command_catalog);
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowDown)],
            &command_catalog,
        );
        frame_with_catalog(
            &ctx,
            &mut overlay,
            vec![key(Key::ArrowDown)],
            &command_catalog,
        );
        assert_eq!(overlay.input, "foo.baz");
        assert_eq!(overlay.selected_suggestion, Some(1));
        assert!(!overlay.selection_from_tab);
        frame_with_catalog(&ctx, &mut overlay, vec![key(Key::Tab)], &command_catalog);
        assert_eq!(overlay.input, "foo.baz ");
    }

    #[test]
    fn large_suggestion_catalog_stays_in_a_bounded_popup_inside_the_panel() {
        let ctx = Context::default();
        let mut overlay = ConsoleOverlay::default();
        overlay.set_open(true);
        overlay.input = "find.c".to_owned();
        let mut command_catalog = CommandCatalog::default();
        for index in 0..200 {
            command_catalog
                .register(
                    CommandSpec::new(format!("find.command{index:03}"), "command description")
                        .unwrap(),
                )
                .unwrap();
        }
        let rect = Rect::from_min_size(pos2(20.0, 30.0), vec2(800.0, 600.0));
        let input = RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |_ui| {
            overlay.show(&ctx, rect, &[], &command_catalog);
        });
        output.textures_delta.clear();

        let panel = ctx
            .memory(|memory| memory.area_rect(Id::new("engine_developer_console")))
            .unwrap();
        let popup = ctx
            .memory(|memory| memory.area_rect(Id::new("engine_developer_console_suggestions")))
            .expect("suggestions should be placed in their own popup area");
        assert!(panel.min.y >= rect.min.y);
        assert!(panel.max.y <= rect.min.y + rect.height() * PANEL_HEIGHT_FRACTION + 1.0);
        assert!(popup.min.y >= panel.min.y);
        assert!(popup.max.y <= panel.max.y);
        assert!(popup.height() <= 120.0 + 1.0);
    }

    #[test]
    fn suggestion_popup_is_anchored_above_the_input_and_clamped_to_the_panel() {
        let panel = Rect::from_min_size(pos2(20.0, 30.0), vec2(800.0, 270.0));
        let input = Rect::from_min_size(pos2(40.0, 250.0), vec2(700.0, 28.0));
        let popup = suggestion_popup_rect(panel, input, 200);

        assert_eq!(popup.max.y, input.top());
        assert!(popup.min.y >= panel.top());
        assert!(popup.min.x >= panel.left());
        assert!(popup.max.x <= panel.right());
        assert!(popup.height() <= MAX_SUGGESTION_POPUP_HEIGHT);
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
