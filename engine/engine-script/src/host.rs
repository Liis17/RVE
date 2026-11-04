use crate::abi_host::{AbiPhase, AbiScriptCall, AbiScriptInstance};
use crate::library::{ScriptLibrary, SharedLibrary};
use crate::lite::{
    self, ActiveCall, ActiveOwner, ActiveSource, CommandGeneration, EventQueue, LuaCommands,
    LuaInstance, RegisteredCommandCallback, ScriptEventQueue,
};
use crate::transaction::{
    MAX_MESSAGES_PER_FRAME, MessageTarget, PendingMessage, ScriptTransaction,
};
use crate::types::{
    ScriptEvent, ScriptEventLevel, ScriptInput, ScriptParams, ScriptTime, ScriptTransform,
};
use engine_console::{CommandCatalog, CommandLine, CommandSpec};
use engine_scene::{LocalTransform, ObjectHandle, ParamSchema, World};
use mlua::{Error as LuaError, Function, Lua, Table, Value};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandError {
    Unknown(String),
    Failed(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(f, "unknown console command {name:?}"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

impl Error for CommandError {}

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

/// Options that control Lua loading and per-call execution.
///
/// The time limit applies to Lua instruction execution; synchronous Rust ABI callbacks cannot be preempted.
#[derive(Clone, Debug, Default)]
pub struct ScriptLoadOptions {
    pub interrupt: ScriptInterruptHandle,
    pub call_time_limit: Option<Duration>,
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
    _loaded_library: Option<SharedLibrary>,
    events: EventQueue,
    active_source: ActiveSource,
    active_call: ActiveCall,
    active_owner: ActiveOwner,
    commands: LuaCommands,
    commands_generation: CommandGeneration,
    transaction: ScriptTransaction,
    queued_messages: Vec<PendingMessage>,
    queued_bytes: usize,
    delivery_messages: Vec<PendingMessage>,
    lua_time: Table,
    call_deadline: lite::CallDeadline,
    interrupt: ScriptInterruptHandle,
    scene_name: String,
    interruption_reported: bool,
    started: bool,
    destroyed: bool,
    input: ScriptInput,
    current_time: ScriptTime,
}

#[derive(Clone, Copy)]
enum Phase {
    Start,
    Update(f32),
    Message,
    Destroy,
}

enum CommandCallback {
    Lua(Function),
    Abi,
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
        Self::load_with_options(
            world,
            scene_name,
            sources,
            library,
            ScriptLoadOptions {
                interrupt,
                call_time_limit: None,
            },
        )
    }

    /// Loads scripts with a sticky interrupt and an optional limit for each Lua call.
    pub fn load_with_options(
        world: &World,
        scene_name: &str,
        sources: &BTreeMap<String, String>,
        library: Option<&ScriptLibrary>,
        options: ScriptLoadOptions,
    ) -> Result<Self, ScriptLoadError> {
        let ScriptLoadOptions {
            interrupt,
            call_time_limit,
        } = options;
        let events = Rc::new(RefCell::new(ScriptEventQueue::default()));
        let active_source = Rc::new(RefCell::new(scene_name.to_owned()));
        let active_call = Rc::new(Cell::new(std::ptr::null_mut()));
        let active_owner = Rc::new(Cell::new(None));
        let commands = Rc::new(RefCell::new(BTreeMap::new()));
        let commands_generation = Rc::new(Cell::new(0));
        let call_deadline = lite::CallDeadline::new(call_time_limit, interrupt.interrupted.clone());
        let lua_state = lite::create_lua(
            events.clone(),
            active_source.clone(),
            active_call.clone(),
            active_owner.clone(),
            commands.clone(),
            commands_generation.clone(),
            call_deadline,
        )
        .map_err(|error| ScriptLoadError::LuaHost(error.to_string()))?;
        let lua = lua_state.lua;
        let base_environment = lua_state.base_environment;
        let loaded_library = library.and_then(ScriptLibrary::shared_library);
        let mut host = Self {
            lua,
            instances: Vec::new(),
            _loaded_library: loaded_library,
            events,
            active_source,
            active_call,
            active_owner,
            commands,
            commands_generation,
            transaction: ScriptTransaction::default(),
            queued_messages: Vec::with_capacity(MAX_MESSAGES_PER_FRAME),
            queued_bytes: 0,
            delivery_messages: Vec::with_capacity(MAX_MESSAGES_PER_FRAME),
            lua_time: lua_state.time,
            call_deadline: lua_state.call_deadline,
            interrupt,
            scene_name: scene_name.to_owned(),
            interruption_reported: false,
            started: false,
            destroyed: false,
            input: ScriptInput::default(),
            current_time: ScriptTime::default(),
        };
        let mut classes: HashMap<String, (Table, Option<ParamSchema>)> = HashMap::new();

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
                let (class, schema) = if let Some(class) = classes.get(path) {
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
                        &host.lua,
                        Some(&host.call_deadline),
                        &host.active_call,
                        &mut host.transaction as *mut ScriptTransaction,
                        &host.active_owner,
                        None,
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
                let params =
                    resolve_params(schema.as_ref(), &script.params).map_err(|message| {
                        ScriptLoadError::Lite {
                            path: path.to_owned(),
                            message: format!("object {:?}: {message}", object.id),
                        }
                    })?;
                let instance = LuaInstance::new(
                    &host.lua,
                    class,
                    &object.id,
                    &object.name,
                    &params,
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
                    let schema = library.and_then(|library| library.schema(&script.name));
                    let params = resolve_params(schema, &script.params).map_err(|message| {
                        ScriptLoadError::Heavy {
                            object_id: object.id.clone(),
                            name: script.name.clone(),
                            message,
                        }
                    })?;
                    let source = event_source(scene_name, &object.id, &script.name);
                    let owner = host.instances.len();
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
                        &params,
                        AbiScriptCall {
                            self_object,
                            transaction: &mut host.transaction,
                            events: &host.events,
                            source: &source,
                            commands: &host.commands,
                            commands_generation: &host.commands_generation,
                            command_owner: owner,
                            commands_allowed: false,
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
        self.remove_all_commands();
    }

    /// Takes all log and runtime-error events accumulated since the previous drain.
    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        self.events.borrow_mut().drain()
    }

    pub fn command_catalog(&self) -> CommandCatalog {
        let mut catalog = CommandCatalog::default();
        for command in self.commands.borrow().values() {
            catalog
                .register(command.spec.clone())
                .expect("script commands are validated when registered");
        }
        catalog
    }

    pub fn commands(&self) -> Vec<CommandSpec> {
        self.command_catalog().commands()
    }

    pub fn commands_generation(&self) -> u64 {
        self.commands_generation.get()
    }

    /// Executes a registered script command inside the owning script's world transaction.
    pub fn execute_command(
        &mut self,
        world: &mut World,
        command: &CommandLine,
    ) -> Result<(), CommandError> {
        let name = command.name.to_ascii_lowercase();
        let (owner, callback) = {
            let commands = self.commands.borrow();
            let Some(command) = commands.get(&name) else {
                return Err(CommandError::Unknown(name));
            };
            let callback = match &command.callback {
                RegisteredCommandCallback::Lua(key) => CommandCallback::Lua(
                    self.lua
                        .registry_value::<Function>(key)
                        .map_err(|error| CommandError::Failed(error.to_string()))?,
                ),
                RegisteredCommandCallback::Abi => CommandCallback::Abi,
            };
            (command.owner, callback)
        };
        let Some(instance) = self.instances.get(owner) else {
            return Err(CommandError::Failed(
                "command owner no longer exists".to_owned(),
            ));
        };
        if !instance.enabled || self.destroyed {
            return Err(CommandError::Unknown(name));
        }
        if matches!(&callback, CommandCallback::Abi)
            && !matches!(&instance.kind, ScriptKind::Heavy(HeavyInstance::Abi(_)))
        {
            return Err(CommandError::Failed(
                "command callback does not match its script owner".to_owned(),
            ));
        }
        let is_lua = matches!(&callback, CommandCallback::Lua(_));
        if is_lua && self.interrupt.is_interrupted() {
            self.disable_lite_instances();
            return Err(CommandError::Failed("scripts interrupted".to_owned()));
        }
        let handle = instance.handle;
        let source = instance.source.clone();
        let args = if is_lua {
            let args = self
                .lua
                .create_table_with_capacity(command.args.len(), 0)
                .map_err(|error| CommandError::Failed(error.to_string()))?;
            for (index, value) in command.args.iter().enumerate() {
                args.raw_set(index + 1, value.as_str())
                    .map_err(|error| CommandError::Failed(error.to_string()))?;
            }
            Some(args)
        } else {
            None
        };

        world.recalculate_world_matrices();
        *self.active_source.borrow_mut() = source.clone();
        self.transaction.begin(
            world,
            handle,
            &source,
            true,
            self.input,
            (self.queued_messages.len(), self.queued_bytes),
        );
        let transaction_ptr = &mut self.transaction as *mut ScriptTransaction;
        let result: Result<Option<String>, (String, bool)> = {
            let call_deadline =
                matches!(&callback, CommandCallback::Lua(_)).then_some(&self.call_deadline);
            let _guard = ActiveCallGuard::new(
                &self.lua,
                call_deadline,
                &self.active_call,
                transaction_ptr,
                &self.active_owner,
                Some(owner),
            );
            match callback {
                CommandCallback::Lua(callback) => callback
                    .call::<Value>(args.expect("Lua command arguments exist"))
                    .map(|result| match result {
                        Value::String(message) => Some(message.to_string_lossy()),
                        _ => None,
                    })
                    .map_err(|error| (error.to_string(), self.call_deadline.is_expired())),
                CommandCallback::Abi => {
                    let ScriptKind::Heavy(HeavyInstance::Abi(script)) =
                        &mut self.instances[owner].kind
                    else {
                        unreachable!("ABI command owner was checked above")
                    };
                    let Some(self_object) = abi_object_handle(handle) else {
                        self.transaction.discard();
                        return Err(CommandError::Failed(
                            "object handle cannot be represented by the script ABI".to_owned(),
                        ));
                    };
                    let call = AbiScriptCall {
                        self_object,
                        // SAFETY: this is the only transaction borrow for the synchronous ABI call.
                        transaction: unsafe { &mut *transaction_ptr },
                        events: &self.events,
                        source: &source,
                        commands: &self.commands,
                        commands_generation: &self.commands_generation,
                        command_owner: owner,
                        commands_allowed: true,
                    };
                    script
                        .invoke_command(self.current_time, &name, &command.args, call)
                        .map(|()| None)
                }
            }
        };
        if is_lua && self.interrupt.is_interrupted() {
            self.transaction.discard();
            self.disable_lite_instances();
            return Err(CommandError::Failed("Lua execution interrupted".to_owned()));
        }
        let result = match result {
            Ok(result) => result,
            Err((message, disable_owner)) => {
                self.transaction.discard();
                if disable_owner {
                    self.disable_instance(owner);
                }
                self.events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source,
                    message: message.clone(),
                });
                return Err(CommandError::Failed(message));
            }
        };
        if let Err(message) = self.transaction.commit(
            world,
            &mut self.queued_messages,
            &mut self.queued_bytes,
            &self.events,
        ) {
            self.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source,
                message: message.clone(),
            });
            return Err(CommandError::Failed(message));
        }
        if let Some(message) = result {
            self.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Info,
                source,
                message,
            });
        }
        Ok(())
    }

    fn run_phase(&mut self, world: &mut World, phase: Phase, time: ScriptTime) {
        self.current_time = time;
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
                self.disable_instance(index);
                continue;
            }
            match self.call_instance(index, world, phase, time, None, time_error.as_deref()) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => {
                    observed_interrupt = true;
                    self.disable_instance(index);
                }
                CallOutcome::Failed(message) => {
                    self.disable_instance(index);
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
            for index in 0..self.instances.len() {
                if matches!(&self.instances[index].kind, ScriptKind::Lite(_)) {
                    self.disable_instance(index);
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
                match self.call_instance(
                    index,
                    world,
                    Phase::Message,
                    time,
                    Some(&message),
                    time_error.as_deref(),
                ) {
                    CallOutcome::Committed => {}
                    CallOutcome::Interrupted => self.disable_instance(index),
                    CallOutcome::Failed(message) => {
                        self.disable_instance(index);
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
            active_owner,
            call_deadline,
            transaction,
            input,
            queued_messages,
            queued_bytes,
            lua,
            interrupt,
            commands,
            commands_generation,
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
                let _active_call = ActiveCallGuard::new(
                    lua,
                    Some(call_deadline),
                    active_call,
                    transaction_ptr,
                    active_owner,
                    Some(index),
                );
                let has_callback = if matches!(phase, Phase::Message) {
                    lite_instance.has_message_callback()
                } else {
                    Ok(true)
                };
                has_callback
                    .and_then(|has_callback| {
                        if !has_callback {
                            return Ok(None);
                        }
                        let callback = match time_error {
                            Some(message) => Err(LuaError::runtime(message.to_owned())),
                            None => {
                                lite_instance
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
                                    })
                            }
                        };
                        callback.and_then(|()| lite_instance.transform()).map(Some)
                    })
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
                    commands,
                    commands_generation,
                    command_owner: index,
                    commands_allowed: true,
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

    fn disable_instance(&mut self, index: usize) {
        self.instances[index].enabled = false;
        self.remove_commands_for_owner(index);
    }

    fn disable_lite_instances(&mut self) {
        for index in 0..self.instances.len() {
            if matches!(&self.instances[index].kind, ScriptKind::Lite(_)) {
                self.disable_instance(index);
            }
        }
    }

    fn remove_commands_for_owner(&mut self, owner: usize) {
        let removed = {
            let mut commands = self.commands.borrow_mut();
            let names: Vec<_> = commands
                .iter()
                .filter_map(|(name, command)| (command.owner == owner).then_some(name.clone()))
                .collect();
            names
                .into_iter()
                .filter_map(|name| commands.remove(&name).map(|command| command.callback))
                .collect::<Vec<_>>()
        };
        self.remove_command_keys(removed);
    }

    fn remove_all_commands(&mut self) {
        let removed = std::mem::take(&mut *self.commands.borrow_mut())
            .into_values()
            .map(|command| command.callback)
            .collect();
        self.remove_command_keys(removed);
    }

    fn remove_command_keys(&mut self, removed: Vec<RegisteredCommandCallback>) {
        if removed.is_empty() {
            return;
        }
        self.commands_generation
            .set(self.commands_generation.get().wrapping_add(1));
        for callback in removed {
            if let RegisteredCommandCallback::Lua(key) = callback {
                let _ = self.lua.remove_registry_value(key);
            }
        }
    }
}

enum CallOutcome {
    Committed,
    Interrupted,
    Failed(String),
}

struct ActiveCallGuard<'lua> {
    active_call: ActiveCall,
    active_owner: ActiveOwner,
    _deadline: Option<lite::ActiveCallDeadline<'lua>>,
}

impl<'lua> ActiveCallGuard<'lua> {
    fn new(
        lua: &'lua Lua,
        call_deadline: Option<&lite::CallDeadline>,
        active_call: &ActiveCall,
        transaction: *mut ScriptTransaction,
        active_owner: &ActiveOwner,
        owner: Option<usize>,
    ) -> Self {
        active_call.set(transaction);
        active_owner.set(owner);
        Self {
            active_call: active_call.clone(),
            active_owner: active_owner.clone(),
            _deadline: call_deadline.map(|deadline| deadline.enter(lua)),
        }
    }
}

impl Drop for ActiveCallGuard<'_> {
    fn drop(&mut self) {
        self.active_call.set(std::ptr::null_mut());
        self.active_owner.set(None);
    }
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        self.remove_all_commands();
    }
}

fn abi_object_handle(handle: ObjectHandle) -> Option<u64> {
    u64::try_from(handle.index())
        .ok()
        .and_then(|index| index.checked_add(1))
}

/// Значения параметров экземпляра: по схеме скрипта, если она объявлена, иначе как в сцене.
fn resolve_params(
    schema: Option<&ParamSchema>,
    params: &ScriptParams,
) -> Result<ScriptParams, String> {
    match schema {
        Some(schema) => schema.resolve(params),
        None => Ok(params.clone()),
    }
}

/// Схема параметров lite-скрипта из поля класса `properties`; `None` — схема не объявлена.
///
/// Chunk выполняется в отдельном Lua с теми же ограничениями памяти и прерыванием, что и при
/// загрузке сцены, но без API движка: `rve.find` и подобные на верхнем уровне дают ошибку.
pub fn lite_properties(
    label: &str,
    source: &str,
    interrupt: &ScriptInterruptHandle,
) -> Result<Option<ParamSchema>, ScriptLoadError> {
    lite::properties(label, source, interrupt.interrupted.clone()).map_err(|message| {
        ScriptLoadError::Lite {
            path: label.to_owned(),
            message,
        }
    })
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
