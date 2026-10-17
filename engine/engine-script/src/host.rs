use crate::abi_host::{AbiPhase, AbiScriptCall, AbiScriptInstance};
use crate::library::ScriptLibrary;
use crate::lite::{self, ActiveCall, ActiveSource, EventQueue, LuaInstance};
use crate::transaction::{
    MAX_MESSAGES_PER_FRAME, MessageTarget, PendingMessage, ScriptTransaction,
};
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptInput, ScriptTime, ScriptTransform};
use engine_scene::{LocalTransform, ObjectHandle, World};
use mlua::{Error as LuaError, Lua, Table};
use std::cell::{Cell, RefCell};
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
    active_call: ActiveCall,
    transaction: ScriptTransaction,
    queued_messages: Vec<PendingMessage>,
    queued_bytes: usize,
    delivery_messages: Vec<PendingMessage>,
    lua_time: Table,
    interrupt: ScriptInterruptHandle,
    scene_name: String,
    interruption_reported: bool,
    started: bool,
    destroyed: bool,
    input: ScriptInput,
}

#[derive(Clone, Copy)]
enum Phase {
    Start,
    Update(f32),
    Message,
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
        let active_call = Rc::new(Cell::new(std::ptr::null_mut()));
        let lua_state = lite::create_lua(
            events.clone(),
            active_source.clone(),
            active_call.clone(),
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
            active_call,
            transaction: ScriptTransaction::default(),
            queued_messages: Vec::with_capacity(MAX_MESSAGES_PER_FRAME),
            queued_bytes: 0,
            delivery_messages: Vec::with_capacity(MAX_MESSAGES_PER_FRAME),
            lua_time: lua_state.time,
            interrupt,
            scene_name: scene_name.to_owned(),
            interruption_reported: false,
            started: false,
            destroyed: false,
            input: ScriptInput::default(),
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
                    let source_label = event_source(scene_name, &object.id, path);
                    *host.active_source.borrow_mut() = source_label.clone();
                    host.transaction.begin(
                        world,
                        object.handle,
                        &source_label,
                        false,
                        host.input,
                        (host.queued_messages.len(), host.queued_bytes),
                    );
                    let _active_call = ActiveCallGuard::new(
                        &host.active_call,
                        &mut host.transaction as *mut ScriptTransaction,
                    );
                    let result = lite::compile_class(&host.lua, &base_environment, path, source);
                    drop(_active_call);
                    host.transaction.discard();
                    let class = result.map_err(|message| ScriptLoadError::Lite {
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
                    host.transaction.begin(
                        world,
                        object.handle,
                        &source,
                        false,
                        host.input,
                        (host.queued_messages.len(), host.queued_bytes),
                    );
                    let result = AbiScriptInstance::create(
                        descriptor,
                        &script.params,
                        AbiScriptCall {
                            self_object,
                            transaction: &mut host.transaction,
                            events: &host.events,
                            source: &source,
                        },
                    );
                    host.transaction.discard();
                    let instance = result.map_err(|message| ScriptLoadError::Heavy {
                        object_id: object.id.clone(),
                        name: script.name.clone(),
                        message,
                    })?;
                    host.instances.push(ScriptInstance {
                        handle: object.handle,
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

    /// Replaces the input snapshot used by the next update and its lifecycle callbacks.
    pub fn set_input(&mut self, input: ScriptInput) {
        self.input = input;
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
        self.input.clear_frame_transients();
        if self.started {
            self.run_phase(world, Phase::Destroy, time);
        }
    }

    /// Takes all log and runtime-error events accumulated since the previous drain.
    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
    }

    fn run_phase(&mut self, world: &mut World, phase: Phase, time: ScriptTime) {
        world.recalculate_world_matrices();
        if matches!(phase, Phase::Update(_)) {
            self.deliver_messages(world, time);
        }

        let time_error = lite::set_time(&self.lua_time, time)
            .err()
            .map(|error| error.to_string());
        let mut observed_interrupt = self.interrupt.is_interrupted();
        for index in 0..self.instances.len() {
            if !self.instances[index].enabled {
                continue;
            }
            if matches!(&self.instances[index].kind, ScriptKind::Lite(_))
                && self.interrupt.is_interrupted()
            {
                observed_interrupt = true;
                self.instances[index].enabled = false;
                continue;
            }
            match self.call_instance(index, world, phase, time, None, time_error.as_deref()) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => {
                    observed_interrupt = true;
                    self.instances[index].enabled = false;
                }
                CallOutcome::Failed(message) => {
                    self.instances[index].enabled = false;
                    push_error(&self.events, &self.instances[index], message);
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
                self.events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: self.scene_name.clone(),
                    message: "scripts interrupted".to_owned(),
                });
                self.interruption_reported = true;
            }
        }
    }

    fn deliver_messages(&mut self, world: &mut World, time: ScriptTime) {
        std::mem::swap(&mut self.queued_messages, &mut self.delivery_messages);
        self.queued_bytes = 0;
        if self.delivery_messages.is_empty() {
            return;
        }
        self.delivery_messages.reverse();
        let time_error = lite::set_time(&self.lua_time, time)
            .err()
            .map(|error| error.to_string());
        while let Some(message) = self.delivery_messages.pop() {
            for index in 0..self.instances.len() {
                let instance = &self.instances[index];
                let is_target = match message.target {
                    MessageTarget::Object(handle) => instance.handle == handle,
                    MessageTarget::Broadcast => true,
                };
                if !instance.enabled || !is_target {
                    continue;
                }
                if matches!(
                    &instance.kind,
                    ScriptKind::Lite(lite) if !lite.has_message_callback()
                ) {
                    continue;
                }
                match self.call_instance(
                    index,
                    world,
                    Phase::Message,
                    time,
                    Some(&message),
                    time_error.as_deref(),
                ) {
                    CallOutcome::Committed => {}
                    CallOutcome::Interrupted => self.instances[index].enabled = false,
                    CallOutcome::Failed(message) => {
                        self.instances[index].enabled = false;
                        push_error(&self.events, &self.instances[index], message);
                    }
                }
            }
        }
    }

    fn call_instance(
        &mut self,
        index: usize,
        world: &mut World,
        phase: Phase,
        time: ScriptTime,
        message: Option<&PendingMessage>,
        time_error: Option<&str>,
    ) -> CallOutcome {
        let Self {
            instances,
            events,
            active_source,
            active_call,
            transaction,
            input,
            queued_messages,
            queued_bytes,
            lua,
            interrupt,
            ..
        } = self;
        let instance = &mut instances[index];
        let Some(local) = world.local_transform(instance.handle).copied() else {
            transaction.discard();
            return CallOutcome::Failed("object no longer exists".to_owned());
        };
        let initial = script_transform(local);
        transaction.begin(
            world,
            instance.handle,
            &instance.source,
            true,
            *input,
            (queued_messages.len(), *queued_bytes),
        );
        let transaction_ptr = transaction as *mut ScriptTransaction;
        let callback_result: Result<Option<ScriptTransform>, String> = match &mut instance.kind {
            ScriptKind::Lite(lite_instance) => {
                *active_source.borrow_mut() = instance.source.clone();
                let _active_call = ActiveCallGuard::new(active_call, transaction_ptr);
                let callback = match time_error {
                    Some(message) => Err(LuaError::runtime(message.to_owned())),
                    None => lite_instance
                        .set_transform(initial)
                        .and_then(|()| match phase {
                            Phase::Start => lite_instance.invoke_start(),
                            Phase::Update(dt) => lite_instance.invoke_update(dt),
                            Phase::Message => match message {
                                Some(message) => lite_instance.invoke_message(
                                    lua,
                                    &message.sender_id,
                                    &message.name,
                                    &message.value,
                                ),
                                None => Err(LuaError::runtime("missing message data")),
                            },
                            Phase::Destroy => lite_instance.invoke_destroy(),
                        }),
                };
                callback
                    .and_then(|()| lite_instance.transform())
                    .map(Some)
                    .map_err(|error| error.to_string())
            }
            ScriptKind::Heavy(HeavyInstance::Abi(script)) => {
                let Some(self_object) = abi_object_handle(instance.handle) else {
                    // SAFETY: the transaction has been started and remains owned by this call.
                    unsafe { (*transaction_ptr).discard() };
                    return CallOutcome::Failed(
                        "object handle cannot be represented by the script ABI".to_owned(),
                    );
                };
                let call = AbiScriptCall {
                    self_object,
                    // SAFETY: this is the only transaction borrow for the synchronous ABI call.
                    transaction: unsafe { &mut *transaction_ptr },
                    events,
                    source: &instance.source,
                };
                match phase {
                    Phase::Message => match message {
                        Some(message) => script.invoke_message(
                            time,
                            &message.sender_id,
                            &message.name,
                            &message.value,
                            call,
                        ),
                        None => Err("missing message data".to_owned()),
                    },
                    Phase::Start => script.invoke(AbiPhase::Start, time, call),
                    Phase::Update(dt) => script.invoke(AbiPhase::Update(dt), time, call),
                    Phase::Destroy => script.invoke(AbiPhase::Destroy, time, call),
                }
                .map(|()| None)
            }
        };

        if matches!(&instance.kind, ScriptKind::Lite(_)) && interrupt.is_interrupted() {
            // SAFETY: the callback and its call-scoped transaction borrow have returned.
            unsafe { (*transaction_ptr).discard() };
            return CallOutcome::Interrupted;
        }

        match callback_result {
            Ok(Some(transform)) if transform.is_finite() => {
                // SAFETY: the callback and its call-scoped transaction borrow have returned.
                let staged = unsafe {
                    transform == initial
                        || (*transaction_ptr).set_local_transform(instance.handle, transform)
                };
                if !staged {
                    // SAFETY: the transaction is still owned by this host call.
                    unsafe { (*transaction_ptr).discard() };
                    return CallOutcome::Failed(
                        "could not stage the script object's local transform".to_owned(),
                    );
                }
            }
            Ok(Some(_)) => {
                // SAFETY: the callback and its call-scoped transaction borrow have returned.
                unsafe { (*transaction_ptr).discard() };
                return CallOutcome::Failed("script produced a non-finite transform".to_owned());
            }
            Ok(None) => {}
            Err(message) => {
                // SAFETY: the callback and its call-scoped transaction borrow have returned.
                unsafe { (*transaction_ptr).discard() };
                return CallOutcome::Failed(message);
            }
        }

        // SAFETY: the callback and all call-scoped transaction borrows have returned.
        match unsafe { (*transaction_ptr).commit(world, queued_messages, queued_bytes, events) } {
            Ok(()) => CallOutcome::Committed,
            Err(message) => CallOutcome::Failed(message),
        }
    }
}

enum CallOutcome {
    Committed,
    Interrupted,
    Failed(String),
}

struct ActiveCallGuard {
    active_call: ActiveCall,
}

impl ActiveCallGuard {
    fn new(active_call: &ActiveCall, transaction: *mut ScriptTransaction) -> Self {
        active_call.set(transaction);
        Self {
            active_call: active_call.clone(),
        }
    }
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        self.active_call.set(std::ptr::null_mut());
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
