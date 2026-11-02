//! Play-режим: копия мира сцены и скрипты на главном потоке редактора.

use engine_scene::{SceneDocument, World};
use engine_script::{
    ScriptEvent, ScriptHost, ScriptInput, ScriptInterruptHandle, ScriptLibrary, ScriptTime,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Дольше этого Lua-вызов считается зависшим и прерывается, чтобы UI ожил.
const SCRIPT_TIME_LIMIT: Duration = Duration::from_secs(5);
/// Шаг времени не растёт при паузах окна и отладчике.
const MAX_DELTA_SECONDS: f32 = 0.25;

/// Прерывает Lua, если отмеченный участок выполняется дольше предела.
///
/// ABI-скрипты прервать нельзя: зависший Rust-скрипт останавливает весь редактор.
struct Watchdog {
    shared: Arc<WatchdogShared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct WatchdogState {
    deadline: Option<Instant>,
    stop: bool,
}

#[derive(Default)]
struct WatchdogShared {
    state: Mutex<WatchdogState>,
    changed: Condvar,
}

impl Watchdog {
    fn start(interrupt: ScriptInterruptHandle) -> std::io::Result<Self> {
        let shared = Arc::new(WatchdogShared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("rve-editor-watchdog".to_owned())
            .spawn(move || watch(&thread_shared, &interrupt))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn set_deadline(&self, deadline: Option<Instant>) {
        lock(&self.shared.state).deadline = deadline;
        self.shared.changed.notify_all();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn watch(shared: &WatchdogShared, interrupt: &ScriptInterruptHandle) {
    let mut state = lock(&shared.state);
    while !state.stop {
        state = match state.deadline {
            None => shared
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner()),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    interrupt.interrupt();
                    state.deadline = None;
                    continue;
                }
                shared
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
        };
    }
}

/// Запущенная сцена. Документ не меняется: правки скриптов живут только в этом мире,
/// поэтому Stop возвращает редактор к исходному документу простым удалением сессии.
pub struct PlaySession {
    world: World,
    host: ScriptHost,
    watchdog: Watchdog,
    time_limit: Duration,
    paused: bool,
    elapsed: f64,
    frame: u64,
    last_tick: Instant,
    input: ScriptInput,
}

impl PlaySession {
    /// Запускает сцену. Исходники Lua читаются через `read_source`; скрипты получают `on_start`
    /// при первом обновлении.
    pub fn start(
        document: &SceneDocument,
        library: Option<&ScriptLibrary>,
        read_source: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        Self::start_with_limit(document, library, read_source, SCRIPT_TIME_LIMIT)
    }

    fn start_with_limit(
        document: &SceneDocument,
        library: Option<&ScriptLibrary>,
        mut read_source: impl FnMut(&str) -> Result<String, String>,
        time_limit: Duration,
    ) -> Result<Self, String> {
        let world = World::from_document(document).map_err(|error| error.to_string())?;
        let mut sources = BTreeMap::new();
        for object in world.objects() {
            for script in &object.lite_scripts {
                let path = script.path.as_str();
                if !sources.contains_key(path) {
                    sources.insert(path.to_owned(), read_source(path)?);
                }
            }
        }
        let interrupt = ScriptInterruptHandle::new();
        let watchdog = Watchdog::start(interrupt.clone())
            .map_err(|error| format!("не удалось запустить watchdog скриптов: {error}"))?;
        watchdog.set_deadline(Some(Instant::now() + time_limit));
        let host = ScriptHost::load_with_interrupt(
            &world,
            document.source_label(),
            &sources,
            library,
            interrupt,
        )
        .map_err(|error| error.to_string())?;
        watchdog.set_deadline(None);
        Ok(Self {
            world,
            host,
            watchdog,
            time_limit,
            paused: false,
            elapsed: 0.0,
            frame: 0,
            last_tick: Instant::now(),
            input: ScriptInput::default(),
        })
    }

    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Пауза не накапливает время: первый кадр после неё получает обычный шаг.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.last_tick = Instant::now();
    }

    /// Вводит состояние кнопок и клавиш для следующего обновления.
    pub fn input_mut(&mut self) -> &mut ScriptInput {
        &mut self.input
    }

    /// Выполняет один кадр скриптов, если сцена не на паузе.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last_tick)
            .as_secs_f32()
            .min(MAX_DELTA_SECONDS);
        self.last_tick = now;
        self.step(dt);
    }

    fn step(&mut self, dt: f32) {
        if self.paused {
            return;
        }
        self.elapsed += f64::from(dt);
        self.frame += 1;
        self.host.set_input(self.input.take_frame());
        let time = self.script_time();
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        self.host.update(&mut self.world, dt, time);
        self.watchdog.set_deadline(None);
    }

    fn script_time(&self) -> ScriptTime {
        ScriptTime {
            elapsed: self.elapsed,
            frame: self.frame,
        }
    }

    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        self.host.drain_events()
    }

    /// Завершает скрипты вызовом `on_destroy` и возвращает последние события.
    pub fn stop(mut self) -> Vec<ScriptEvent> {
        let time = self.script_time();
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        self.host.destroy(&mut self.world, time);
        self.watchdog.set_deadline(None);
        self.host.drain_events()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENE: &str = r#"
[[assets]]
path = "/rotate.lua"

[[objects]]
id = "camera"
[objects.camera]
fov_deg = 60.0
near = 0.1
far = 100.0

[[objects]]
id = "spinner"
[[objects.lite_scripts]]
path = "/rotate.lua"
params = { speed = 90.0 }
"#;

    const ROTATE: &str = r#"
local Rotate = {}
function Rotate:on_start()
  rve.log_info("started")
end
function Rotate:on_update(dt)
  local rotation = self.transform.rotation
  self.transform.rotation = { x = rotation.x, y = rotation.y + self.params.speed * dt, z = rotation.z }
end
function Rotate:on_destroy()
  rve.log_info("destroyed")
end
return Rotate
"#;

    fn document(source: &str) -> SceneDocument {
        SceneDocument::parse("scene", source).unwrap()
    }

    fn start(script: &'static str) -> Result<PlaySession, String> {
        PlaySession::start(&document(SCENE), None, |_| Ok(script.to_owned()))
    }

    fn rotation_y(session: &mut PlaySession) -> f32 {
        let world = session.world_mut();
        let handle = world.handle("spinner").unwrap();
        world.local_transform(handle).unwrap().rotation_deg.y
    }

    #[test]
    fn lua_script_changes_the_play_world_but_not_the_document() {
        let document = document(SCENE);
        let mut session = PlaySession::start(&document, None, |_| Ok(ROTATE.to_owned())).unwrap();
        session.step(0.5);
        assert!((rotation_y(&mut session) - 45.0).abs() < 1e-3);
        let original = document.objects.iter().find(|o| o.id == "spinner").unwrap();
        assert_eq!(original.transform.rotation, [0.0; 3]);
    }

    #[test]
    fn pause_stops_updates_and_resume_continues() {
        let mut session = start(ROTATE).unwrap();
        session.step(0.5);
        session.set_paused(true);
        assert!(session.is_paused());
        session.step(0.5);
        session.tick();
        assert!((rotation_y(&mut session) - 45.0).abs() < 1e-3);
        session.set_paused(false);
        session.step(0.5);
        assert!((rotation_y(&mut session) - 90.0).abs() < 1e-3);
    }

    #[test]
    fn stop_runs_on_destroy_and_reports_events() {
        let mut session = start(ROTATE).unwrap();
        session.step(0.1);
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "started")
        );
        let events = session.stop();
        assert!(events.iter().any(|event| event.message == "destroyed"));
    }

    #[test]
    fn heavy_script_without_a_library_refuses_to_start() {
        let source = format!("{SCENE}\n[[objects.heavy_scripts]]\nname = \"Bobber\"\n");
        let error = PlaySession::start(&document(&source), None, |_| Ok(ROTATE.to_owned()))
            .err()
            .unwrap();
        assert!(error.contains("Bobber"), "{error}");
    }

    #[test]
    fn unreadable_or_broken_lua_refuses_to_start() {
        let error = PlaySession::start(&document(SCENE), None, |_| Err("нет файла".to_owned()))
            .err()
            .unwrap();
        assert!(error.contains("нет файла"));
        assert!(start("this is not lua").is_err());
    }

    #[test]
    fn watchdog_interrupts_a_hanging_lua_update_and_disables_lua() {
        let hanging =
            "local H = {}\nfunction H:on_update(dt)\n  while true do end\nend\nreturn H\n";
        let mut session = PlaySession::start_with_limit(
            &document(SCENE),
            None,
            |_| Ok(hanging.to_owned()),
            Duration::from_millis(50),
        )
        .unwrap();
        session.step(0.1);
        assert!(!session.drain_events().is_empty());
        // Прерывание постоянно: следующий кадр не должен снова зависнуть.
        session.step(0.1);
    }

    #[test]
    fn watchdog_interrupts_a_hanging_top_level_chunk() {
        let result = PlaySession::start_with_limit(
            &document(SCENE),
            None,
            |_| Ok("while true do end".to_owned()),
            Duration::from_millis(50),
        );
        assert!(result.is_err());
    }

    #[test]
    fn input_snapshot_reaches_the_script() {
        let script = r#"
local S = {}
function S:on_update(dt)
  if rve.input.key_pressed("Space") then rve.log_info("space") end
end
return S
"#;
        let mut session = start(Box::leak(script.to_owned().into_boxed_str())).unwrap();
        session.input_mut().set_key(engine_script::Key::Space, true);
        session.step(0.1);
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "space")
        );
    }
}
