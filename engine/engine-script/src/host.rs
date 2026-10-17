use crate::abi_host::{AbiPhase, AbiScriptCall, AbiScriptInstance};
use crate::library::ScriptLibrary;
use crate::lite::{self, ActiveSource, EventQueue, LuaInstance};
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptTime, ScriptTransform};
use engine_scene::{LocalTransform, ObjectHandle, World};
use mlua::{Lua, Table};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Script source compilation or heavy-factory failure during host creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptLoadError {
    LuaHost(String),
    Lite {
        path: String,
        message: String,
    },
    Heavy {
        object_id: String,
        name: String,
        message: String,
    },
}

impl fmt::Display for ScriptLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LuaHost(message) => write!(f, "could not create Lua host: {message}"),
            Self::Lite { path, message } => write!(f, "lite script {path:?}: {message}"),
            Self::Heavy {
                object_id,
                name,
                message,
            } => write!(f, "object {object_id:?}, heavy script {name:?}: {message}"),
        }
    }
}

impl Error for ScriptLoadError {}

/// Cloneable, sticky cancellation signal for Lua execution. ABI callbacks run synchronously and
/// cannot be interrupted.
#[derive(Clone, Debug)]
pub struct ScriptInterruptHandle {
    interrupted: Arc<AtomicBool>,
}

impl ScriptInterruptHandle {
    /// Creates an interrupt signal that can be passed to [`ScriptHost::load_with_interrupt`].
    pub fn new() -> Self {
        Self {
            interrupted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Interrupts current and future Lua execution, including top-level chunks loaded later.
    /// At the next phase or lite-call boundary, the host disables all lite instances and reports
    /// one host-level interruption event if a lite instance exists; heavy scripts continue to run.
    pub fn interrupt(&self) {
        self.interrupted
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Default for ScriptInterruptHandle {
    fn default() -> Self {
        Self::new()
    }
}

enum ScriptKind {
    Lite(LuaInstance),
    Heavy(HeavyInstance),
}

enum HeavyInstance {
    Abi(AbiScriptInstance),
}

struct ScriptInstance {
    handle: ObjectHandle,
    object_id: String,
    object_name: String,
    source: String,
    kind: ScriptKind,
    enabled: bool,
}

/// Binds scene objects to one Lua state and their heavy script instances.
///
/// A host belongs on the scripts thread. Its interrupt handle is safe to clone and send to
/// another thread; it only cancels Lua execution, not Rust ABI scripts.
pub struct ScriptHost {
    lua: Lua,
    instances: Vec<ScriptInstance>,
    events: EventQueue,
    active_source: ActiveSource,
    lua_time: Table,
    interrupt: ScriptInterruptHandle,
    scene_name: String,
    interruption_reported: bool,
    started: bool,
    destroyed: bool,
}

#[derive(Clone, Copy)]
enum Phase {
    Start,
    Update(f32),
    Destroy,
}

impl ScriptHost {
    /// Loads scripts from canonical virtual paths (without a leading slash) in declaration order.
    ///
    /// Creates a fresh interrupt handle. Use [`Self::load_with_interrupt`] to cancel top-level
    /// Lua code while the host is being loaded.
    pub fn load(
        world: &World,
        scene_name: &str,
        sources: &BTreeMap<String, String>,
        library: Option<&ScriptLibrary>,
    ) -> Result<Self, ScriptLoadError> {
        Self::load_with_interrupt(
            world,
            scene_name,
            sources,
            library,
            ScriptInterruptHandle::new(),
        )
    }

    /// Loads scripts using a handle that may be interrupted from another thread before or during
    /// loading. Lua top-level chunks are cancellable; ABI callbacks are synchronous and cannot be
    /// interrupted.
    pub fn load_with_interrupt(
        world: &World,
        scene_name: &str,
        sources: &BTreeMap<String, String>,
        library: Option<&ScriptLibrary>,
        interrupt: ScriptInterruptHandle,
    ) -> Result<Self, ScriptLoadError> {
        let events = Rc::new(RefCell::new(Vec::new()));
        let active_source = Rc::new(RefCell::new(scene_name.to_owned()));
        let lua_state = lite::create_lua(
            events.clone(),
            active_source.clone(),
            interrupt.interrupted.clone(),
        )
        .map_err(|error| ScriptLoadError::LuaHost(error.to_string()))?;
        let lua = lua_state.lua;
        let base_environment = lua_state.base_environment;
        let mut host = Self {
            lua,
            instances: Vec::new(),
            events,
            active_source,
            lua_time: lua_state.time,
            interrupt,
            scene_name: scene_name.to_owned(),
            interruption_reported: false,
            started: false,
            destroyed: false,
        };
        let mut classes: HashMap<String, Table> = HashMap::new();

        for object in world.objects() {
            let transform = script_transform(object.transform);
            for script in &object.lite_scripts {
                let path = script.path.as_str();
                let Some(source) = sources.get(path) else {
                    return Err(ScriptLoadError::Lite {
                        path: path.to_owned(),
                        message: "source was not provided".to_owned(),
                    });
                };
                let class = if let Some(class) = classes.get(path) {
                    class.clone()
                } else {
                    *host.active_source.borrow_mut() = event_source(scene_name, &object.id, path);
                    let class = lite::compile_class(&host.lua, &base_environment, path, source)
                        .map_err(|message| ScriptLoadError::Lite {
                            path: path.to_owned(),
                            message,
                        })?;
                    classes.insert(path.to_owned(), class.clone());
                    class
                };
                let instance = LuaInstance::new(
                    &host.lua,
                    class,
                    &object.id,
                    &object.name,
                    &script.params,
                    transform,
                )
                .map_err(|error| ScriptLoadError::Lite {
                    path: path.to_owned(),
                    message: error.to_string(),
                })?;
                host.instances.push(ScriptInstance {
                    handle: object.handle,
                    object_id: object.id.clone(),
                    object_name: object.name.clone(),
                    source: event_source(scene_name, &object.id, path),
                    kind: ScriptKind::Lite(instance),
                    enabled: true,
                });
            }

            for script in &object.heavy_scripts {
                let descriptor = library.and_then(|library| library.script(&script.name));
                if let Some(descriptor) = descriptor {
                    let self_object =
                        abi_object_handle(object.handle).ok_or_else(|| ScriptLoadError::Heavy {
                            object_id: object.id.clone(),
                            name: script.name.clone(),
                            message: "object handle cannot be represented by the script ABI"
                                .to_owned(),
                        })?;
                    let source = event_source(scene_name, &object.id, &script.name);
                    let instance = AbiScriptInstance::create(
                        descriptor,
                        &script.params,
                        AbiScriptCall {
                            self_object,
                            object_id: &object.id,
                            object_name: &object.name,
                            transform,
                            events: &host.events,
                            source: &source,
                        },
                    )
                    .map_err(|message| ScriptLoadError::Heavy {
                        object_id: object.id.clone(),
                        name: script.name.clone(),
                        message,
                    })?;
                    host.instances.push(ScriptInstance {
                        handle: object.handle,
                        object_id: object.id.clone(),
                        object_name: object.name.clone(),
                        source,
                        kind: ScriptKind::Heavy(HeavyInstance::Abi(instance)),
                        enabled: true,
                    });
                    continue;
                }
                return Err(ScriptLoadError::Heavy {
                    object_id: object.id.clone(),
                    name: script.name.clone(),
                    message: if library.is_none() {
                        "no script library is loaded; pass --scripts <path> or place the library in Engine/".to_owned()
                    } else {
                        "the script library does not register this name".to_owned()
                    },
                });
            }
        }
        Ok(host)
    }

    /// Returns the host's sticky interrupt handle. An interrupt skips and disables lite instances
    /// at the next phase or lite-call boundary; ABI scripts continue to run.
    pub fn interrupt_handle(&self) -> ScriptInterruptHandle {
        self.interrupt.clone()
    }

    /// Runs on_start once. update also calls start automatically if needed.
    pub fn start(&mut self, world: &mut World, time: ScriptTime) {
        if self.started || self.destroyed {
            return;
        }
        self.started = true;
        self.run_phase(world, Phase::Start, time);
    }

    /// Runs on_update for enabled instances, starting the host first if needed.
    pub fn update(&mut self, world: &mut World, dt: f32, time: ScriptTime) {
        if self.destroyed {
            return;
        }
        if !self.started {
            self.start(world, time);
        }
        self.run_phase(world, Phase::Update(dt), time);
    }

    /// Runs on_destroy once for enabled instances that have started.
    pub fn destroy(&mut self, world: &mut World, time: ScriptTime) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        if self.started {
            self.run_phase(world, Phase::Destroy, time);
        }
    }

    /// Takes all log and runtime-error events accumulated since the previous drain.
    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
    }

    fn run_phase(&mut self, world: &mut World, phase: Phase, time: ScriptTime) {
        let events = &self.events;
        let time_error = lite::set_time(&self.lua_time, time)
            .err()
            .map(|error| error.to_string());
        let mut observed_interrupt = self.interrupt.is_interrupted();
        for instance in &mut self.instances {
            if !instance.enabled {
                continue;
            }
            if matches!(&instance.kind, ScriptKind::Lite(_)) && self.interrupt.is_interrupted() {
                observed_interrupt = true;
                instance.enabled = false;
                continue;
            }
            let Some(local) = world.local_transform(instance.handle).copied() else {
                instance.enabled = false;
                push_error(events, instance, "object no longer exists".to_owned());
                continue;
            };
            let initial = script_transform(local);
            let result = match &mut instance.kind {
                ScriptKind::Lite(lite_instance) => {
                    *self.active_source.borrow_mut() = instance.source.clone();
                    match time_error.as_ref() {
                        Some(message) => Err(message.clone()),
                        None => lite_instance
                            .set_transform(initial)
                            .and_then(|()| match phase {
                                Phase::Start => lite_instance.invoke_start(),
                                Phase::Update(dt) => lite_instance.invoke_update(dt),
                                Phase::Destroy => lite_instance.invoke_destroy(),
                            })
                            .and_then(|()| lite_instance.transform())
                            .map_err(|error| error.to_string()),
                    }
                }
                ScriptKind::Heavy(HeavyInstance::Abi(script)) => {
                    let self_object = abi_object_handle(instance.handle);
                    match self_object {
                        Some(self_object) => script.invoke(
                            match phase {
                                Phase::Start => AbiPhase::Start,
                                Phase::Update(dt) => AbiPhase::Update(dt),
                                Phase::Destroy => AbiPhase::Destroy,
                            },
                            time,
                            AbiScriptCall {
                                self_object,
                                object_id: &instance.object_id,
                                object_name: &instance.object_name,
                                transform: initial,
                                events,
                                source: &instance.source,
                            },
                        ),
                        None => {
                            Err("object handle cannot be represented by the script ABI".to_owned())
                        }
                    }
                }
            };

            if matches!(&instance.kind, ScriptKind::Lite(_)) && self.interrupt.is_interrupted() {
                observed_interrupt = true;
                instance.enabled = false;
                continue;
            }

            match result {
                Ok(transform) if transform.is_finite() => {
                    if let Err(error) =
                        world.set_local_transform(instance.handle, local_transform(transform))
                    {
                        instance.enabled = false;
                        push_error(events, instance, error.to_string());
                    }
                }
                Ok(_) => {
                    instance.enabled = false;
                    push_error(
                        events,
                        instance,
                        "script produced a non-finite transform".to_owned(),
                    );
                }
                Err(message) => {
                    instance.enabled = false;
                    if matches!(&instance.kind, ScriptKind::Lite(_))
                        && self.interrupt.is_interrupted()
                    {
                        observed_interrupt = true;
                    } else {
                        push_error(events, instance, message);
                    }
                }
            }
        }
        if self.interrupt.is_interrupted() {
            observed_interrupt = true;
        }
        if observed_interrupt
            && self
                .instances
                .iter()
                .any(|instance| matches!(&instance.kind, ScriptKind::Lite(_)))
        {
            for instance in &mut self.instances {
                if matches!(&instance.kind, ScriptKind::Lite(_)) {
                    instance.enabled = false;
                }
            }
            if !self.interruption_reported {
                events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: self.scene_name.clone(),
                    message: "scripts interrupted".to_owned(),
                });
                self.interruption_reported = true;
            }
        }
    }
}

fn abi_object_handle(handle: ObjectHandle) -> Option<u64> {
    u64::try_from(handle.index())
        .ok()
        .and_then(|index| index.checked_add(1))
}

fn script_transform(transform: LocalTransform) -> ScriptTransform {
    ScriptTransform {
        position: transform.position.to_array(),
        rotation: transform.rotation_deg.to_array(),
        scale: transform.scale.to_array(),
    }
}

fn local_transform(transform: ScriptTransform) -> LocalTransform {
    LocalTransform {
        position: transform.position.into(),
        rotation_deg: transform.rotation.into(),
        scale: transform.scale.into(),
    }
}

fn push_error(events: &EventQueue, instance: &ScriptInstance, message: String) {
    events.borrow_mut().push(ScriptEvent {
        level: ScriptEventLevel::Error,
        source: instance.source.clone(),
        message,
    });
}

fn event_source(scene_name: &str, object_id: &str, script_name: &str) -> String {
    format!("{scene_name}/{object_id}/{script_name}")
}

#[cfg(test)]
mod tests;
