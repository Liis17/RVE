//! Курсор, события скриптов и консольные команды потока скриптов.

use super::*;

/// Задаёт курсор скриптов из флагов активной камеры и публикует его до запуска скриптов.
pub(crate) fn set_initial_cursor(
    scripts: &mut ScriptHost,
    world: &World,
    cursor: &CursorControl,
    published: &mut Option<ScriptCursor>,
) {
    scripts.set_cursor(ScriptCursor::from(world.active_camera_cursor()));
    publish_script_cursor(scripts, cursor, published);
}

/// Публикует курсор скриптов, если он отличается от последней опубликованного значения.
pub(crate) fn publish_script_cursor(
    scripts: &ScriptHost,
    cursor: &CursorControl,
    published: &mut Option<ScriptCursor>,
) {
    let current = scripts.cursor();
    if *published != Some(current) {
        *published = Some(current);
        cursor.set_script_cursor(current);
    }
}

pub(crate) fn print_script_events(events: Vec<ScriptEvent>) {
    log_script_events_with(&events, log::logger());
}

#[cfg(feature = "dev-console")]
pub(crate) fn publish_command_catalog(
    scripts: &ScriptHost,
    shared: &Mutex<CommandCatalogSnapshot>,
    published_generation: &mut u64,
) {
    let generation = scripts.commands_generation();
    if generation == *published_generation {
        return;
    }
    let mut snapshot = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    snapshot.generation = generation;
    snapshot.catalog = scripts.command_catalog();
    let shadowed = snapshot.catalog.get(PROFILE_COMMAND).is_some();
    if shadowed && !snapshot.profile_shadowed {
        log::warn!(
            target: "console",
            "команда profile из скрипта затенена встроенной командой профайлера"
        );
    }
    snapshot.profile_shadowed = shadowed;
    if !shadowed
        && let Ok(spec) =
            engine_console::CommandSpec::new(PROFILE_COMMAND, PROFILE_COMMAND_DESCRIPTION)
    {
        let _ = snapshot.catalog.register(spec);
    }
    *published_generation = generation;
}

#[cfg(feature = "dev-console")]
pub(crate) fn execute_console_commands(
    scripts: &mut ScriptHost,
    world: &mut World,
    pending: &mut PendingConsoleCommands,
) -> Vec<(String, CommandError)> {
    let mut errors = Vec::new();
    while let Some(command) = pending.pop_front() {
        if let Err(error) = scripts.execute_command(world, &command) {
            errors.push((command.name, error));
        }
        print_script_events(scripts.drain_events());
    }
    errors
}

pub(crate) fn log_script_events_with(events: &[ScriptEvent], logger: &dyn log::Log) {
    for event in events {
        let (level, text) = match event.level {
            ScriptEventLevel::Info => (log::Level::Info, format!("[script] {event}")),
            ScriptEventLevel::Warn => (log::Level::Warn, format!("[script warn] {event}")),
            ScriptEventLevel::Error => (log::Level::Error, format!("[script] {event}")),
        };
        let args = format_args!("{text}");
        let record = log::Record::builder()
            .args(args)
            .level(level)
            .target(&event.source)
            .build();
        logger.log(&record);
    }
}
