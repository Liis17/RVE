use crate::abi_host::{AbiPhase, AbiScriptCall, AbiScriptInstance};
use crate::actions::InputActions;
use crate::library::{ScriptLibrary, SharedLibrary};
use crate::lifecycle::{
    Lifecycle, MAX_SPAWNED_OBJECTS_PER_FRAME, PrefabCatalog, PrefabKind, PrefabRequest, PrefabSlot,
    PrefabStatus, SpawnRequest, SpawnedObject, WorldChanges, prefab_display, prefab_key,
};
use crate::lite::{
    self, ActiveCall, ActiveOwner, ActiveSource, CommandGeneration, EventQueue, LitePhysicsEvent,
    LuaCommands, LuaInstance, RegisteredCommandCallback, ScriptEventQueue,
};
use crate::physics::PhysicsScriptState;
use crate::transaction::{
    MAX_MESSAGES_PER_FRAME, MessageTarget, PendingMessage, ScriptTransaction, ScriptTransactionInit,
};
use crate::types::{
    ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptInput, ScriptParams, ScriptTime,
    ScriptTransform,
};
use engine_console::{CommandCatalog, CommandLine, CommandSpec};
use engine_physics::{PhysicsCommand, PhysicsEvent, PhysicsSceneBinding, QuerySnapshot};
use engine_project::InputSettings;
use engine_scene::{
    AudioRequest, LocalTransform, ObjectHandle, ParamSchema, PrefabTemplate, RigidBodyKind, World,
};
use mlua::{Error as LuaError, Function, Lua, Table, Value};
use rve_sdk::abi::RveScriptDesc;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

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
    Input(String),
    /// Ошибка записи списка префабов или регистрации шаблона.
    Prefab {
        path: String,
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
            Self::Input(message) => write!(f, "invalid project input settings: {message}"),
            Self::Prefab { path, message } => write!(f, "prefab {path:?}: {message}"),
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
    pub input: InputSettings,
    /// Префабы, которые скрипты могут создавать: быстрые и тяжёлые.
    pub prefabs: PrefabCatalog,
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
    /// `on_start` уже вызван (или вызывается) для этого экземпляра.
    started: bool,
    /// Уникальный номер экземпляра; отличает занятый слот от освобождённого.
    serial: u64,
}

/// Binds scene objects to one Lua state and their heavy script instances.
///
/// A host belongs on the scripts thread. Its interrupt handle is safe to clone and send to
/// another thread; it only cancels Lua execution, not Rust ABI scripts.
pub struct ScriptHost {
    lua: Lua,
    base_environment: Table,
    /// Слоты экземпляров: освобождённые переиспользуются через `free_instances`.
    instances: Vec<Option<ScriptInstance>>,
    free_instances: Vec<usize>,
    next_serial: u64,
    /// Экземпляры, которым нужен `on_start` в начале следующего update.
    start_queue: Vec<(usize, u64)>,
    /// Скомпилированные классы lite-скриптов по путям; общие для сцены и префабов.
    classes: HashMap<String, (Table, Option<ParamSchema>)>,
    /// Heavy-скрипты библиотеки по имени: дескриптор и схема параметров.
    heavy: HashMap<String, (&'static RveScriptDesc, Option<ParamSchema>)>,
    lifecycle: Rc<RefCell<Lifecycle>>,
    /// Spawn-объектов, уже созданных в текущем кадре; сбрасывается в начале update.
    frame_spawned: usize,
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
    audio_requests: Vec<AudioRequest>,
    delivery_messages: Vec<PendingMessage>,
    lua_time: Table,
    call_deadline: lite::CallDeadline,
    interrupt: ScriptInterruptHandle,
    scene_name: String,
    interruption_reported: bool,
    started: bool,
    destroyed: bool,
    actions: InputActions,
    current_time: ScriptTime,
    physics: Rc<RefCell<PhysicsScriptState>>,
    update_profile: ScriptUpdateProfile,
    cursor: ScriptCursor,
}

/// Время последнего `update` хоста по фазам. Все поля нулевые до первого `update`.
///
/// `world_matrices`, `messages`, `physics_events`, `lua` и `rust` не перекрываются
/// и вместе не превышают `total`; остаток доступен через [`Self::other`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScriptUpdateProfile {
    /// Весь `update`, включая перечисленные ниже фазы.
    pub total: Duration,
    /// Пересчёт мировых матриц перед скриптами.
    pub world_matrices: Duration,
    /// Доставка сообщений между скриптами.
    pub messages: Duration,
    /// Доставка событий физики.
    pub physics_events: Duration,
    /// Сумма вызовов Lua-экземпляров.
    pub lua: Duration,
    /// Сумма вызовов экземпляров Rust ABI.
    pub rust: Duration,
}

impl ScriptUpdateProfile {
    /// Остаток `total` после перечисленных фаз: выбор экземпляров и служебная работа.
    pub fn other(&self) -> Duration {
        let named =
            self.world_matrices + self.messages + self.physics_events + self.lua + self.rust;
        self.total.saturating_sub(named)
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Start,
    Update(f32),
    Message,
    PhysicsEvent(ScriptPhysicsEvent),
    Destroy,
}

#[derive(Clone, Copy)]
struct ScriptPhysicsEvent {
    kind: ScriptPhysicsEventKind,
    other: ObjectHandle,
    point: glam::Vec3,
    normal: glam::Vec3,
}

#[derive(Clone, Copy)]
enum ScriptPhysicsEventKind {
    CollisionEnter,
    CollisionExit,
    TriggerEnter,
    TriggerExit,
}

impl ScriptPhysicsEventKind {
    fn callback_name(self) -> &'static str {
        match self {
            Self::CollisionEnter => "on_collision_enter",
            Self::CollisionExit => "on_collision_exit",
            Self::TriggerEnter => "on_trigger_enter",
            Self::TriggerExit => "on_trigger_exit",
        }
    }

    fn abi_kind(self) -> u32 {
        match self {
            Self::CollisionEnter => rve_sdk::abi::RVE_COLLISION_ENTER,
            Self::CollisionExit => rve_sdk::abi::RVE_COLLISION_EXIT,
            Self::TriggerEnter => rve_sdk::abi::RVE_TRIGGER_ENTER,
            Self::TriggerExit => rve_sdk::abi::RVE_TRIGGER_EXIT,
        }
    }
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
                input: InputSettings::default(),
                prefabs: PrefabCatalog::default(),
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
            input: input_settings,
            prefabs,
        } = options;
        let actions = InputActions::new(input_settings).map_err(ScriptLoadError::Input)?;
        let mut lifecycle = Lifecycle::default();
        for (path, spec) in prefabs.iter() {
            if spec.max_instances == Some(0) {
                return Err(ScriptLoadError::Prefab {
                    path: path.to_owned(),
                    message: "max_instances must be greater than zero".to_owned(),
                });
            }
            lifecycle
                .prefabs
                .insert(path.to_owned(), PrefabSlot::new(spec));
        }
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
        let heavy = library.map(heavy_scripts).unwrap_or_default();
        let mut host = Self {
            lua,
            base_environment,
            instances: Vec::new(),
            free_instances: Vec::new(),
            next_serial: 0,
            start_queue: Vec::new(),
            classes: HashMap::new(),
            heavy,
            lifecycle: Rc::new(RefCell::new(lifecycle)),
            frame_spawned: 0,
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
            audio_requests: Vec::new(),
            delivery_messages: Vec::with_capacity(MAX_MESSAGES_PER_FRAME),
            lua_time: lua_state.time,
            call_deadline: lua_state.call_deadline,
            interrupt,
            scene_name: scene_name.to_owned(),
            interruption_reported: false,
            started: false,
            update_profile: ScriptUpdateProfile::default(),
            destroyed: false,
            actions,
            current_time: ScriptTime::default(),
            physics: Rc::new(RefCell::new(PhysicsScriptState::default())),
            cursor: ScriptCursor::default(),
        };

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
                let (class, schema) = if let Some(class) = host.classes.get(path) {
                    class.clone()
                } else {
                    let source_label = event_source(scene_name, &object.id, path);
                    *host.active_source.borrow_mut() = source_label.clone();
                    host.transaction.begin(ScriptTransactionInit {
                        world,
                        self_object: Some(object.handle),
                        source: &source_label,
                        writable: false,
                        actions: host.actions.frame(),
                        queued_messages: (host.queued_messages.len(), host.queued_bytes),
                        queued_audio: host.audio_requests.len(),
                        physics: host.physics.clone(),
                        physics_phase_dt: None,
                        lifecycle: host.lifecycle.clone(),
                        lifecycle_writes: false,
                        instance_serial: 0,
                        cursor: host.cursor,
                    });
                    let _active_call = ActiveCallGuard::new(
                        &host.lua,
                        Some(&host.call_deadline),
                        &host.active_call,
                        &mut host.transaction as *mut ScriptTransaction,
                        &host.active_owner,
                        None,
                    );
                    let result =
                        lite::compile_class(&host.lua, &host.base_environment, path, source);
                    drop(_active_call);
                    host.transaction.discard();
                    let class = result.map_err(|message| ScriptLoadError::Lite {
                        path: path.to_owned(),
                        message,
                    })?;
                    host.classes.insert(path.to_owned(), class.clone());
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
                let serial = host.next_serial();
                host.insert_instance(ScriptInstance {
                    handle: object.handle,
                    source: event_source(scene_name, &object.id, path),
                    kind: ScriptKind::Lite(instance),
                    enabled: true,
                    started: false,
                    serial,
                });
            }

            for script in &object.heavy_scripts {
                let descriptor = host
                    .heavy
                    .get(&script.name)
                    .map(|(descriptor, _)| *descriptor);
                if let Some(descriptor) = descriptor {
                    let self_object =
                        abi_object_handle(object.handle).ok_or_else(|| ScriptLoadError::Heavy {
                            object_id: object.id.clone(),
                            name: script.name.clone(),
                            message: "object handle cannot be represented by the script ABI"
                                .to_owned(),
                        })?;
                    let schema = host
                        .heavy
                        .get(&script.name)
                        .and_then(|(_, schema)| schema.clone());
                    let params =
                        resolve_params(schema.as_ref(), &script.params).map_err(|message| {
                            ScriptLoadError::Heavy {
                                object_id: object.id.clone(),
                                name: script.name.clone(),
                                message,
                            }
                        })?;
                    let source = event_source(scene_name, &object.id, &script.name);
                    let owner = host.allocate_slot();
                    let serial = host.next_serial();
                    host.transaction.begin(ScriptTransactionInit {
                        world,
                        self_object: Some(object.handle),
                        source: &source,
                        writable: false,
                        actions: host.actions.frame(),
                        queued_messages: (host.queued_messages.len(), host.queued_bytes),
                        queued_audio: host.audio_requests.len(),
                        physics: host.physics.clone(),
                        physics_phase_dt: None,
                        lifecycle: host.lifecycle.clone(),
                        lifecycle_writes: false,
                        instance_serial: serial,
                        cursor: host.cursor,
                    });
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
                    let instance = match result {
                        Ok(instance) => instance,
                        Err(message) => {
                            host.free_slot(owner);
                            return Err(ScriptLoadError::Heavy {
                                object_id: object.id.clone(),
                                name: script.name.clone(),
                                message,
                            });
                        }
                    };
                    host.instances[owner] = Some(ScriptInstance {
                        handle: object.handle,
                        source,
                        kind: ScriptKind::Heavy(HeavyInstance::Abi(instance)),
                        enabled: true,
                        started: false,
                        serial,
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
        self.actions.set_input(input);
    }

    /// Returns the cursor state applied by the last successful script commit or `set_cursor`.
    pub fn cursor(&self) -> ScriptCursor {
        self.cursor
    }

    /// Sets the initial cursor state before scripts start, for example from the active camera.
    pub fn set_cursor(&mut self, cursor: ScriptCursor) {
        self.cursor = cursor;
    }

    /// Attaches the current immutable physics query snapshot and scene-body mapping.
    pub fn set_physics_snapshot(
        &mut self,
        snapshot: Arc<QuerySnapshot>,
        bindings: &[PhysicsSceneBinding],
    ) {
        self.physics.borrow_mut().set_snapshot(snapshot, bindings);
        self.lifecycle.borrow_mut().advance_physics_snapshot();
    }

    /// Queues physics events for delivery after messages at the beginning of the next update.
    pub fn queue_physics_events(&mut self, events: impl IntoIterator<Item = PhysicsEvent>) {
        self.physics.borrow_mut().queue_events(events);
    }

    /// Drains commands committed by successful script callbacks.
    pub fn drain_physics_commands(&mut self) -> Vec<PhysicsCommand> {
        self.physics.borrow_mut().drain_commands()
    }

    /// Регистрирует шаблон префаба из списка. Быстрый префаб регистрируется до старта, тяжёлый
    /// — после `Loading`. Lua-классы компилируются сразу, параметры проверяются по схемам.
    ///
    /// При ошибке состояние не меняется. Для тяжёлого префаба вызывающий код обязан
    /// вызвать [`Self::prefab_failed`], иначе его spawn-заявки останутся в ожидании.
    pub fn register_prefab(
        &mut self,
        world: &World,
        path: &str,
        template: Arc<PrefabTemplate>,
        sources: &BTreeMap<String, String>,
    ) -> Result<(), ScriptLoadError> {
        let key = prefab_key(path).map_err(|message| ScriptLoadError::Prefab {
            path: path.to_owned(),
            message,
        })?;
        let (kind, status) = {
            let lifecycle = self.lifecycle.borrow();
            let slot = lifecycle
                .prefabs
                .get(&key)
                .ok_or_else(|| ScriptLoadError::Prefab {
                    path: prefab_display(&key),
                    message: "the prefab is not in the project prefab list".to_owned(),
                })?;
            (slot.kind, slot.status)
        };
        let expected = match kind {
            PrefabKind::Fast => PrefabStatus::Unloaded,
            PrefabKind::Heavy => PrefabStatus::Loading,
        };
        if status != expected {
            return Err(ScriptLoadError::Prefab {
                path: prefab_display(&key),
                message: format!(
                    "the prefab is {}, not waiting for registration",
                    status.as_str()
                ),
            });
        }
        for (script_path, source) in sources {
            if self.classes.contains_key(script_path) {
                continue;
            }
            let class = self
                .compile_registered_class(world, script_path, source)
                .map_err(|message| ScriptLoadError::Lite {
                    path: script_path.clone(),
                    message,
                })?;
            self.classes.insert(script_path.clone(), class);
        }
        for object in template.objects() {
            for script in &object.lite_scripts {
                let script_path = script.path.as_str();
                let (_, schema) =
                    self.classes
                        .get(script_path)
                        .ok_or_else(|| ScriptLoadError::Lite {
                            path: script_path.to_owned(),
                            message: "source was not provided".to_owned(),
                        })?;
                resolve_params(schema.as_ref(), &script.params).map_err(|message| {
                    ScriptLoadError::Lite {
                        path: script_path.to_owned(),
                        message: format!("object {:?}: {message}", object.id),
                    }
                })?;
            }
            for script in &object.heavy_scripts {
                let (_, schema) =
                    self.heavy
                        .get(&script.name)
                        .ok_or_else(|| ScriptLoadError::Heavy {
                            object_id: object.id.clone(),
                            name: script.name.clone(),
                            message: "the script library does not register this name".to_owned(),
                        })?;
                resolve_params(schema.as_ref(), &script.params).map_err(|message| {
                    ScriptLoadError::Heavy {
                        object_id: object.id.clone(),
                        name: script.name.clone(),
                        message,
                    }
                })?;
            }
        }

        let mut lifecycle = self.lifecycle.borrow_mut();
        let slot = lifecycle
            .prefabs
            .get_mut(&key)
            .expect("prefab slot was checked above");
        slot.register(template);
        slot.status = match kind {
            PrefabKind::Fast => PrefabStatus::Resident,
            PrefabKind::Heavy => PrefabStatus::Loaded,
        };
        if kind == PrefabKind::Heavy {
            let waiting = std::mem::take(&mut lifecycle.waiting);
            let (ready, rest): (Vec<SpawnRequest>, Vec<SpawnRequest>) = waiting
                .into_iter()
                .partition(|request| request.prefab == key);
            lifecycle.waiting = rest;
            lifecycle.queued.extend(ready);
            lifecycle.begin_unload_if_idle(path);
        }
        Ok(())
    }

    /// Состояние префаба; `None` — путь не в списке проекта.
    pub fn prefab_status(&self, path: &str) -> Option<PrefabStatus> {
        let key = prefab_key(path).ok()?;
        self.lifecycle
            .borrow()
            .prefabs
            .get(&key)
            .map(|slot| slot.status)
    }

    /// Вызывающий код освободил ресурсы выгруженного префаба. Если на него ждут spawn,
    /// загрузка начинается снова.
    pub fn prefab_released(&mut self, path: &str) {
        if let Ok(key) = prefab_key(path) {
            self.lifecycle.borrow_mut().prefab_released(&key);
        }
    }

    /// Загрузка тяжёлого префаба не удалась. Его ожидающие spawn умирают с Error-событием;
    /// `world` нужен, чтобы освободить их резервы.
    pub fn prefab_failed(&mut self, world: &World, path: &str, error: &str) {
        let Ok(key) = prefab_key(path) else {
            return;
        };
        let failed = {
            let mut lifecycle = self.lifecycle.borrow_mut();
            let Some(slot) = lifecycle.prefabs.get_mut(&key) else {
                return;
            };
            if slot.status != PrefabStatus::Loading {
                return;
            }
            slot.status = PrefabStatus::Failed;
            slot.template = None;
            let waiting = std::mem::take(&mut lifecycle.waiting);
            let (failed, rest): (Vec<SpawnRequest>, Vec<SpawnRequest>) = waiting
                .into_iter()
                .partition(|request| request.prefab == key);
            lifecycle.waiting = rest;
            failed
        };
        for request in failed {
            self.cancel_spawn(
                world,
                &request,
                format!("prefab {:?} failed to load: {error}", prefab_display(&key)),
            );
        }
    }

    /// Изменения мира с прошлого вызова: созданные и удалённые объекты, запросы загрузки.
    pub fn drain_world_changes(&mut self) -> WorldChanges {
        std::mem::take(&mut self.lifecycle.borrow_mut().changes)
    }

    /// Откатывает spawn, который хост уже применил, но мир или физика его отвергли. Поддерево
    /// `root` удаляется вместе с экземплярами: не стартовавшие экземпляры снимаются без
    /// `on_destroy` и `on_start`, стартовавшие получают `on_destroy` как при `destroy`.
    ///
    /// Удаления не попадают в [`WorldChanges`]: вызывающий код уже получил `spawned` для этого
    /// корня из последнего `drain_world_changes` и сам снимает его ресурсы. Пишет Error с
    /// причиной `reason`; его нужно забрать через `drain_events`.
    pub fn revert_spawn(&mut self, world: &mut World, root: ObjectHandle, reason: &str) {
        if world.is_alive(root) {
            let count = world.subtree(&[root]).len();
            if self
                .remove_subtrees(world, &[root], self.current_time)
                .is_some()
            {
                // Откат возвращает место в лимите кадра: отказавший spawn его не тратит.
                self.frame_spawned = self.frame_spawned.saturating_sub(count);
            }
        }
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Error,
            source: self.scene_name.clone(),
            message: format!("spawn rolled back: {reason}"),
        });
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

    /// Возвращает время последнего `update` по фазам; `Start` и `Destroy` сюда не входят.
    pub fn last_update_profile(&self) -> ScriptUpdateProfile {
        self.update_profile
    }

    /// Runs on_destroy once for enabled instances that have started.
    pub fn destroy(&mut self, world: &mut World, time: ScriptTime) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;
        self.actions.clear_frame_transients();
        if self.started {
            self.run_phase(world, Phase::Destroy, time);
        }
        self.remove_all_commands();
        self.release_all_pending(world);
    }

    /// Takes all log and runtime-error events accumulated since the previous drain.
    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        self.events.borrow_mut().drain()
    }

    /// Takes audio requests committed by scripts since the previous drain, in commit order.
    pub fn drain_audio_requests(&mut self) -> Vec<AudioRequest> {
        std::mem::take(&mut self.audio_requests)
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
        let Some(instance) = self.instances.get(owner).and_then(Option::as_ref) else {
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
        let serial = instance.serial;
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
        self.transaction.begin(ScriptTransactionInit {
            world,
            self_object: Some(handle),
            source: &source,
            writable: true,
            actions: self.actions.frame(),
            queued_messages: (self.queued_messages.len(), self.queued_bytes),
            queued_audio: self.audio_requests.len(),
            physics: self.physics.clone(),
            physics_phase_dt: None,
            lifecycle: self.lifecycle.clone(),
            lifecycle_writes: true,
            instance_serial: serial,
            cursor: self.cursor,
        });
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
                    let Some(ScriptInstance {
                        kind: ScriptKind::Heavy(HeavyInstance::Abi(script)),
                        ..
                    }) = self.instances[owner].as_mut()
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
        let mutations = match self.transaction.commit(
            world,
            &mut self.queued_messages,
            &mut self.queued_bytes,
            &self.events,
            &mut self.audio_requests,
        ) {
            Ok(mutations) => {
                self.cursor = self.transaction.cursor();
                mutations
            }
            Err(message) => {
                self.events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source,
                    message: message.clone(),
                });
                return Err(CommandError::Failed(message));
            }
        };
        self.actions.queue_mutations(mutations);
        if let Some(message) = result {
            self.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Info,
                source,
                message,
            });
        }
        let time = self.current_time;
        self.apply_lifecycle(world, time);
        Ok(())
    }

    fn run_phase(&mut self, world: &mut World, phase: Phase, time: ScriptTime) {
        let total_started = Instant::now();
        let mut profile = ScriptUpdateProfile::default();
        self.current_time = time;
        self.lifecycle.borrow_mut().now = time.elapsed;
        let started = Instant::now();
        world.recalculate_world_matrices();
        profile.world_matrices = started.elapsed();
        if matches!(phase, Phase::Update(_)) {
            self.frame_spawned = 0;
            self.run_pending_starts(world, time);
            let started = Instant::now();
            self.deliver_messages(world, time);
            profile.messages = started.elapsed();
            let started = Instant::now();
            self.deliver_physics_events(world, time);
            profile.physics_events = started.elapsed();
        }

        let time_error = lite::set_time(&self.lua_time, time)
            .err()
            .map(|error| error.to_string());
        let mut observed_interrupt = self.interrupt.is_interrupted();
        for index in 0..self.instances.len() {
            let Some(instance) = self.instances[index].as_mut() else {
                continue;
            };
            if !instance.enabled {
                continue;
            }
            if matches!(phase, Phase::Destroy) && !instance.started {
                continue;
            }
            if matches!(phase, Phase::Start) {
                instance.started = true;
            }
            let is_lite = matches!(&instance.kind, ScriptKind::Lite(_));
            if is_lite && self.interrupt.is_interrupted() {
                observed_interrupt = true;
                self.disable_instance(index);
                continue;
            }
            let started = Instant::now();
            let outcome =
                self.call_instance(index, world, phase, time, None, time_error.as_deref());
            let elapsed = started.elapsed();
            if is_lite {
                profile.lua += elapsed;
            } else {
                profile.rust += elapsed;
            }
            match outcome {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => {
                    observed_interrupt = true;
                    self.disable_instance(index);
                }
                CallOutcome::Failed(message) => self.fail_instance(index, message),
            }
        }
        if self.interrupt.is_interrupted() {
            observed_interrupt = true;
        }
        if observed_interrupt
            && self
                .instances
                .iter()
                .flatten()
                .any(|instance| matches!(&instance.kind, ScriptKind::Lite(_)))
        {
            self.disable_lite_instances();
            if !self.interruption_reported {
                self.events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: self.scene_name.clone(),
                    message: "scripts interrupted".to_owned(),
                });
                self.interruption_reported = true;
            }
        }
        if !matches!(phase, Phase::Destroy) {
            self.apply_lifecycle(world, time);
        }
        if matches!(phase, Phase::Update(_)) {
            profile.total = total_started.elapsed();
            self.update_profile = profile;
        }
    }

    /// `on_start` экземпляров, созданных spawn: до сообщений первого update после применения.
    fn run_pending_starts(&mut self, world: &mut World, time: ScriptTime) {
        let queue = std::mem::take(&mut self.start_queue);
        let time_error = lite::set_time(&self.lua_time, time)
            .err()
            .map(|error| error.to_string());
        for (index, serial) in queue {
            let Some(instance) = self.instances.get_mut(index).and_then(Option::as_mut) else {
                continue;
            };
            if instance.serial != serial || !instance.enabled || instance.started {
                continue;
            }
            instance.started = true;
            match self.call_instance(
                index,
                world,
                Phase::Start,
                time,
                None,
                time_error.as_deref(),
            ) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
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
                let Some(instance) = self.instances[index].as_ref() else {
                    continue;
                };
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
                    CallOutcome::Failed(message) => self.fail_instance(index, message),
                }
            }
        }
    }

    fn deliver_physics_events(&mut self, world: &mut World, time: ScriptTime) {
        let events = self.physics.borrow_mut().take_events();
        for event in events {
            let endpoints = match event {
                PhysicsEvent::CollisionEnter {
                    first,
                    second,
                    point,
                    normal,
                } => Some((
                    first,
                    second,
                    ScriptPhysicsEventKind::CollisionEnter,
                    point,
                    normal,
                )),
                PhysicsEvent::CollisionExit { first, second } => Some((
                    first,
                    second,
                    ScriptPhysicsEventKind::CollisionExit,
                    glam::Vec3::ZERO,
                    glam::Vec3::ZERO,
                )),
                PhysicsEvent::TriggerEnter { first, second } => Some((
                    first,
                    second,
                    ScriptPhysicsEventKind::TriggerEnter,
                    glam::Vec3::ZERO,
                    glam::Vec3::ZERO,
                )),
                PhysicsEvent::TriggerExit { first, second } => Some((
                    first,
                    second,
                    ScriptPhysicsEventKind::TriggerExit,
                    glam::Vec3::ZERO,
                    glam::Vec3::ZERO,
                )),
            };
            let Some((first, second, kind, point, normal)) = endpoints else {
                continue;
            };
            // Удалённое тело ещё известно через привязку, которая живёт до следующего результата шага.
            let first_object = self.body_object(world, first);
            let second_object = self.body_object(world, second);
            let (Some(first_object), Some(second_object)) = (first_object, second_object) else {
                continue;
            };
            self.deliver_physics_event(
                world,
                time,
                first_object,
                ScriptPhysicsEvent {
                    kind,
                    other: second_object,
                    point,
                    normal,
                },
            );
            self.deliver_physics_event(
                world,
                time,
                second_object,
                ScriptPhysicsEvent {
                    kind,
                    other: first_object,
                    point,
                    normal: -normal,
                },
            );
        }
    }

    /// Объект тела, если он ещё известен: живой или удалённый, но в окне удаления. Для объекта вне
    /// этих двух состояний `other` не имеет идентичности, и пара пропускается.
    fn body_object(&self, world: &World, body: engine_physics::BodyHandle) -> Option<ObjectHandle> {
        let lifecycle = self.lifecycle.borrow();
        let object = self
            .physics
            .borrow()
            .object(body)
            .or_else(|| lifecycle.retired_body(body))?;
        (world.is_alive(object) || lifecycle.retired(object).is_some()).then_some(object)
    }

    fn deliver_physics_event(
        &mut self,
        world: &mut World,
        time: ScriptTime,
        object: ObjectHandle,
        event: ScriptPhysicsEvent,
    ) {
        let indexes = self
            .instances
            .iter()
            .enumerate()
            .filter_map(|(index, instance)| {
                instance
                    .as_ref()
                    .filter(|instance| instance.enabled && instance.handle == object)
                    .map(|_| index)
            })
            .collect::<Vec<_>>();
        for index in indexes {
            match self.call_instance(index, world, Phase::PhysicsEvent(event), time, None, None) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
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
            actions,
            queued_messages,
            queued_bytes,
            audio_requests,
            lua,
            interrupt,
            commands,
            commands_generation,
            physics,
            lifecycle,
            cursor,
            ..
        } = self;
        let Some(instance) = instances[index].as_mut() else {
            return CallOutcome::Failed("script instance no longer exists".to_owned());
        };
        let Some(local) = world.local_transform(instance.handle).copied() else {
            transaction.discard();
            return CallOutcome::Failed("object no longer exists".to_owned());
        };
        let initial = script_transform(local);
        transaction.begin(ScriptTransactionInit {
            world,
            self_object: Some(instance.handle),
            source: &instance.source,
            writable: true,
            actions: actions.frame(),
            queued_messages: (queued_messages.len(), *queued_bytes),
            queued_audio: audio_requests.len(),
            physics: physics.clone(),
            physics_phase_dt: match phase {
                Phase::Update(dt) => Some(dt),
                _ => None,
            },
            lifecycle: lifecycle.clone(),
            lifecycle_writes: !matches!(phase, Phase::Destroy),
            instance_serial: instance.serial,
            cursor: *cursor,
        });
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
                                        Phase::PhysicsEvent(event) => lite_instance
                                            .invoke_physics_event(
                                                lua,
                                                active_call,
                                                LitePhysicsEvent {
                                                    callback: event.kind.callback_name(),
                                                    other: event.other,
                                                    point: event.point,
                                                    normal: event.normal,
                                                    has_contact: matches!(
                                                        event.kind,
                                                        ScriptPhysicsEventKind::CollisionEnter
                                                    ),
                                                },
                                            ),
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
                    Phase::PhysicsEvent(event) => {
                        let Some(other) = (unsafe { (*transaction_ptr).abi_handle(event.other) })
                        else {
                            return CallOutcome::Failed(
                                "physics event object handle cannot be represented by the ABI"
                                    .to_owned(),
                            );
                        };
                        script.invoke_physics_event(
                            time,
                            event.kind.abi_kind(),
                            other,
                            event.point.to_array(),
                            event.normal.to_array(),
                            call,
                        )
                    }
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
        match unsafe {
            (*transaction_ptr).commit(world, queued_messages, queued_bytes, events, audio_requests)
        } {
            Ok(mutations) => {
                // SAFETY: commit returned, so no call-scoped borrow of the transaction remains.
                *cursor = unsafe { (*transaction_ptr).cursor() };
                actions.queue_mutations(mutations);
                CallOutcome::Committed
            }
            Err(message) => CallOutcome::Failed(message),
        }
    }

    /// Применяет накопленные заявки в конце фазы: spawn в порядке commit, затем удаления,
    /// вытеснение по `max_instances` и созревшие отложенные удаления.
    fn apply_lifecycle(&mut self, world: &mut World, time: ScriptTime) {
        self.lifecycle
            .borrow_mut()
            .queued
            .make_contiguous()
            .sort_by_key(|request| request.seq);
        // Заявки снимаются по одной, и оставшиеся остаются в очереди: отмена одной заявки видит
        // остальные и не начинает выгрузку префаба раньше времени.
        loop {
            let next = self.lifecycle.borrow_mut().queued.pop_front();
            let Some(request) = next else {
                break;
            };
            self.process_spawn(world, &request);
        }

        let mut doomed = Vec::new();
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            for request in std::mem::take(&mut lifecycle.destroys) {
                match request.due {
                    None => doomed.push(request.handle),
                    Some(due) => {
                        lifecycle
                            .scheduled
                            .entry(request.handle)
                            .and_modify(|current| *current = current.min(due))
                            .or_insert(due);
                    }
                }
            }
            let now = time.elapsed;
            let due: Vec<ObjectHandle> = lifecycle
                .scheduled
                .iter()
                .filter(|(_, due)| **due <= now)
                .map(|(handle, _)| *handle)
                .collect();
            for handle in &due {
                lifecycle.scheduled.remove(handle);
            }
            doomed.extend(due);

            // Лимит считает только корни, которые останутся после фазы. Корни, удаляемые сейчас,
            // в него не входят, поэтому spawn с немедленным destroy не вытесняет живой экземпляр.
            let going: HashSet<ObjectHandle> = doomed.iter().copied().collect();
            for slot in lifecycle.prefabs.values() {
                let Some(max) = slot.max_instances else {
                    continue;
                };
                let mut staying = slot
                    .live
                    .iter()
                    .filter(|root| !going.contains(root))
                    .count();
                for root in &slot.live {
                    if staying <= max as usize {
                        break;
                    }
                    if !going.contains(root) {
                        doomed.push(*root);
                        staying -= 1;
                    }
                }
            }
        }
        self.destroy_objects(world, doomed, time);
    }

    /// Создаёт объекты заявки или откладывает её до загрузки префаба.
    fn process_spawn(&mut self, world: &mut World, request: &SpawnRequest) {
        let state = self
            .lifecycle
            .borrow()
            .prefabs
            .get(&request.prefab)
            .map(|slot| {
                (
                    slot.kind,
                    slot.status,
                    slot.template.clone(),
                    slot.template_index,
                )
            });
        let Some((kind, status, template, template_index)) = state else {
            self.cancel_spawn(
                world,
                request,
                format!(
                    "prefab {:?} is not in the project prefab list",
                    prefab_display(&request.prefab)
                ),
            );
            return;
        };
        match (status, template) {
            (PrefabStatus::Resident | PrefabStatus::Loaded, Some(template)) => {
                if let Err(message) = self.materialize(world, request, &template, template_index) {
                    self.cancel_spawn(world, request, message);
                }
            }
            (PrefabStatus::Failed, _) => self.cancel_spawn(
                world,
                request,
                format!(
                    "prefab {:?} failed to load",
                    prefab_display(&request.prefab)
                ),
            ),
            (PrefabStatus::Unloaded | PrefabStatus::Loading | PrefabStatus::Unloading, _)
                if kind == PrefabKind::Heavy =>
            {
                let mut lifecycle = self.lifecycle.borrow_mut();
                lifecycle.apply_prefab_request(PrefabRequest::Load(request.prefab.clone()));
                lifecycle.waiting.push(request.clone());
            }
            _ => self.cancel_spawn(
                world,
                request,
                format!(
                    "prefab {:?} is not registered",
                    prefab_display(&request.prefab)
                ),
            ),
        }
    }

    /// Вставляет поддерево шаблона в мир и создаёт его экземпляры. При любой ошибке
    /// поддерево и экземпляры откатываются, а резерв корня освобождается.
    fn materialize(
        &mut self,
        world: &mut World,
        request: &SpawnRequest,
        template: &PrefabTemplate,
        template_index: usize,
    ) -> Result<(), String> {
        let objects = template.objects();
        let count = objects.len();
        if self.frame_spawned + count > MAX_SPAWNED_OBJECTS_PER_FRAME {
            return Err(format!(
                "spawn objects limit ({MAX_SPAWNED_OBJECTS_PER_FRAME} per frame) reached"
            ));
        }
        let root_local = objects
            .iter()
            .position(|object| object.parent.is_none())
            .ok_or_else(|| "prefab has no root object".to_owned())?;
        let reserved = (0..count)
            .map(|local| {
                if local == root_local {
                    request.root
                } else {
                    world.reserve_object()
                }
            })
            .collect::<Vec<_>>();
        let handles = world
            .spawn(reserved, template, request.args.options.clone())
            .map_err(|error| error.to_string())?;
        let root = request.root;

        if request.args.velocity.is_some() {
            let dynamic = world
                .object(root)
                .and_then(|object| object.rigid_body.as_ref())
                .is_some_and(|body| body.kind == RigidBodyKind::Dynamic);
            if !dynamic {
                let _ = world.despawn_many(&handles);
                return Err("spawn velocity requires a dynamic rigid body on the root".to_owned());
            }
        }
        if let Err(message) = self.create_instances(world, &handles) {
            let _ = world.despawn_many(&handles);
            return Err(message);
        }
        // Счётчик кадра растёт только после успеха: откат поддерева не тратит лимит кадра.
        self.frame_spawned += count;

        let mut lifecycle = self.lifecycle.borrow_mut();
        lifecycle.roots.insert(root, request.prefab.clone());
        if let Some(slot) = lifecycle.prefabs.get_mut(&request.prefab) {
            slot.live_count += 1;
            if slot.max_instances.is_some() {
                slot.live.push_back(root);
            }
        }
        if let Some(id) = &request.args.options.id
            && lifecycle.pending_ids.get(id) == Some(&root)
        {
            lifecycle.pending_ids.remove(id);
        }
        lifecycle.changes.spawned.push(SpawnedObject {
            handle: root,
            objects: handles,
            prefab: request.prefab.clone(),
            template_index,
            initial_velocity: request.args.velocity,
        });
        Ok(())
    }

    /// Создаёт lite- и heavy-экземпляры каждого объекта поддерева и ставит им `on_start`.
    fn create_instances(
        &mut self,
        world: &mut World,
        handles: &[ObjectHandle],
    ) -> Result<(), String> {
        let mut created = Vec::new();
        match self.create_instances_into(world, handles, &mut created) {
            Ok(()) => {
                self.start_queue.extend(created);
                Ok(())
            }
            Err(message) => {
                for (index, _) in created {
                    self.remove_instance(index);
                }
                Err(message)
            }
        }
    }

    fn create_instances_into(
        &mut self,
        world: &World,
        handles: &[ObjectHandle],
        created: &mut Vec<(usize, u64)>,
    ) -> Result<(), String> {
        for &handle in handles {
            let object = world
                .object(handle)
                .ok_or_else(|| "spawned object is missing from the world".to_owned())?;
            let id = object.id.clone();
            let name = object.name.clone();
            let transform = script_transform(object.transform);
            let lite_scripts = object.lite_scripts.clone();
            let heavy_scripts = object.heavy_scripts.clone();
            for script in &lite_scripts {
                let path = script.path.as_str();
                let (class, schema) = self
                    .classes
                    .get(path)
                    .cloned()
                    .ok_or_else(|| format!("lite script {path:?} has no compiled class"))?;
                let params = resolve_params(schema.as_ref(), &script.params)
                    .map_err(|message| format!("object {id:?}: {message}"))?;
                let instance = LuaInstance::new(&self.lua, class, &id, &name, &params, transform)
                    .map_err(|error| error.to_string())?;
                let serial = self.next_serial();
                let index = self.insert_instance(ScriptInstance {
                    handle,
                    source: event_source(&self.scene_name, &id, path),
                    kind: ScriptKind::Lite(instance),
                    enabled: true,
                    started: false,
                    serial,
                });
                created.push((index, serial));
            }
            for script in &heavy_scripts {
                let (descriptor, schema) = self
                    .heavy
                    .get(&script.name)
                    .map(|(descriptor, schema)| (*descriptor, schema.clone()))
                    .ok_or_else(|| format!("heavy script {:?} is not registered", script.name))?;
                let params = resolve_params(schema.as_ref(), &script.params)
                    .map_err(|message| format!("object {id:?}: {message}"))?;
                let self_object = abi_object_handle(handle).ok_or_else(|| {
                    "object handle cannot be represented by the script ABI".to_owned()
                })?;
                let source = event_source(&self.scene_name, &id, &script.name);
                let owner = self.allocate_slot();
                let serial = self.next_serial();
                self.transaction.begin(ScriptTransactionInit {
                    world,
                    self_object: Some(handle),
                    source: &source,
                    writable: false,
                    actions: self.actions.frame(),
                    queued_messages: (self.queued_messages.len(), self.queued_bytes),
                    queued_audio: self.audio_requests.len(),
                    physics: self.physics.clone(),
                    physics_phase_dt: None,
                    lifecycle: self.lifecycle.clone(),
                    lifecycle_writes: false,
                    instance_serial: serial,
                    cursor: self.cursor,
                });
                let result = AbiScriptInstance::create(
                    descriptor,
                    &params,
                    AbiScriptCall {
                        self_object,
                        transaction: &mut self.transaction,
                        events: &self.events,
                        source: &source,
                        commands: &self.commands,
                        commands_generation: &self.commands_generation,
                        command_owner: owner,
                        commands_allowed: false,
                    },
                );
                self.transaction.discard();
                let instance = match result {
                    Ok(instance) => instance,
                    Err(message) => {
                        self.free_slot(owner);
                        return Err(message);
                    }
                };
                self.instances[owner] = Some(ScriptInstance {
                    handle,
                    source,
                    kind: ScriptKind::Heavy(HeavyInstance::Abi(instance)),
                    enabled: true,
                    started: false,
                    serial,
                });
                created.push((owner, serial));
            }
        }
        Ok(())
    }

    /// Удаляет поддеревья. `on_destroy` вызывается, пока объекты ещё есть в мире.
    fn destroy_objects(
        &mut self,
        world: &mut World,
        requested: Vec<ObjectHandle>,
        time: ScriptTime,
    ) {
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for handle in requested {
            if !seen.insert(handle) {
                continue;
            }
            if world.is_alive(handle) {
                targets.push(handle);
            } else if let Some(request) = self.take_waiting(handle) {
                self.release_spawn(world, &request);
            }
        }

        // Активная камера и её предки не могут быть удалены вместе с поддеревом.
        let mut camera_chain = HashSet::new();
        let mut cursor = Some(world.active_camera());
        while let Some(handle) = cursor {
            if !camera_chain.insert(handle) {
                break;
            }
            cursor = world.object(handle).and_then(|object| object.parent);
        }
        let (rejected, accepted): (Vec<_>, Vec<_>) = targets
            .into_iter()
            .partition(|handle| camera_chain.contains(handle));
        if !rejected.is_empty() {
            self.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: self.scene_name.clone(),
                message: "the active camera cannot be destroyed".to_owned(),
            });
        }
        if accepted.is_empty() {
            return;
        }
        if let Some(removed) = self.remove_subtrees(world, &accepted, time) {
            self.lifecycle
                .borrow_mut()
                .changes
                .despawned
                .extend(removed);
        }
    }

    /// Удаляет поддеревья `targets` вместе с экземплярами: тела уходят в retire, стартовавшие
    /// экземпляры получают `on_destroy`, затем объекты удаляются из мира. Возвращает удалённые
    /// объекты или `None`, если мир отказал (тогда пишется Error и ничего не меняется в учёте).
    fn remove_subtrees(
        &mut self,
        world: &mut World,
        targets: &[ObjectHandle],
        time: ScriptTime,
    ) -> Option<Vec<ObjectHandle>> {
        let doomed: HashSet<ObjectHandle> = world.subtree(targets).into_iter().collect();

        let retired = doomed
            .iter()
            .filter_map(|&handle| {
                let binding = self.physics.borrow().optional_binding(handle)?;
                let object = world.object(handle)?;
                Some((handle, binding.body, object.id.clone(), object.name.clone()))
            })
            .collect::<Vec<_>>();
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            for (handle, body, id, name) in retired {
                lifecycle.retire(handle, body, id, name);
            }
        }

        let time_error = lite::set_time(&self.lua_time, time)
            .err()
            .map(|error| error.to_string());
        let indexes = (0..self.instances.len())
            .filter(|&index| {
                self.instances[index].as_ref().is_some_and(|instance| {
                    doomed.contains(&instance.handle) && instance.enabled && instance.started
                })
            })
            .collect::<Vec<_>>();
        for index in indexes {
            match self.call_instance(
                index,
                world,
                Phase::Destroy,
                time,
                None,
                time_error.as_deref(),
            ) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
            }
        }

        let removed = match world.despawn_many(targets) {
            Ok(removed) => removed,
            Err(error) => {
                self.events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: self.scene_name.clone(),
                    message: error.to_string(),
                });
                return None;
            }
        };
        let removed_set: HashSet<ObjectHandle> = removed.iter().copied().collect();
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            let mut prefabs = Vec::new();
            for handle in &removed {
                lifecycle.scheduled.remove(handle);
                if let Some(prefab) = lifecycle.roots.remove(handle)
                    && let Some(slot) = lifecycle.prefabs.get_mut(&prefab)
                {
                    slot.live_count = slot.live_count.saturating_sub(1);
                    if slot.max_instances.is_some() {
                        slot.live.retain(|root| root != handle);
                    }
                    prefabs.push(prefab);
                }
            }
            for prefab in prefabs {
                lifecycle.begin_unload_if_idle(&prefab);
            }
        }
        for index in 0..self.instances.len() {
            if self.instances[index]
                .as_ref()
                .is_some_and(|instance| removed_set.contains(&instance.handle))
            {
                self.remove_instance(index);
            }
        }
        // Экземпляры, которым ещё не поставили on_start, из очереди уходят вместе с ними.
        let instances = &self.instances;
        self.start_queue.retain(|&(index, serial)| {
            instances
                .get(index)
                .and_then(Option::as_ref)
                .is_some_and(|instance| instance.serial == serial)
        });
        Some(removed)
    }

    fn take_waiting(&mut self, root: ObjectHandle) -> Option<SpawnRequest> {
        let mut lifecycle = self.lifecycle.borrow_mut();
        let index = lifecycle
            .waiting
            .iter()
            .position(|request| request.root == root)?;
        Some(lifecycle.waiting.remove(index))
    }

    /// Освобождает резерв и id заявки без сообщения: заявку отменил вызывающий код.
    fn release_spawn(&mut self, world: &World, request: &SpawnRequest) {
        world.release_reservation(request.root);
        self.forget_pending_id(request);
        let mut lifecycle = self.lifecycle.borrow_mut();
        lifecycle.scheduled.remove(&request.root);
        lifecycle.begin_unload_if_idle(&request.prefab);
    }

    /// Отказ заявки: освобождает резерв и пишет Error в лог скрипта, который её поставил.
    fn cancel_spawn(&mut self, world: &World, request: &SpawnRequest, message: String) {
        world.release_reservation(request.root);
        self.forget_pending_id(request);
        self.lifecycle
            .borrow_mut()
            .begin_unload_if_idle(&request.prefab);
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Error,
            source: request.source.clone(),
            message,
        });
    }

    fn forget_pending_id(&mut self, request: &SpawnRequest) {
        if let Some(id) = &request.args.options.id {
            let mut lifecycle = self.lifecycle.borrow_mut();
            if lifecycle.pending_ids.get(id) == Some(&request.root) {
                lifecycle.pending_ids.remove(id);
            }
        }
    }

    /// Отменяет всё, что ещё не создано: при `destroy` хоста мир больше не получит эти объекты.
    fn release_all_pending(&mut self, world: &World) {
        let (waiting, queued) = {
            let mut lifecycle = self.lifecycle.borrow_mut();
            lifecycle.destroys.clear();
            lifecycle.scheduled.clear();
            (
                std::mem::take(&mut lifecycle.waiting),
                std::mem::take(&mut lifecycle.queued),
            )
        };
        for request in waiting.iter().chain(queued.iter()) {
            world.release_reservation(request.root);
            self.forget_pending_id(request);
        }
        self.lifecycle.borrow_mut().pending_ids.clear();
    }

    fn next_serial(&mut self) -> u64 {
        self.next_serial += 1;
        self.next_serial
    }

    /// Выделяет слот экземпляра; `None` до заполнения держит место для владельца команд.
    fn allocate_slot(&mut self) -> usize {
        if let Some(index) = self.free_instances.pop() {
            index
        } else {
            self.instances.push(None);
            self.instances.len() - 1
        }
    }

    fn insert_instance(&mut self, instance: ScriptInstance) -> usize {
        let index = self.allocate_slot();
        self.instances[index] = Some(instance);
        index
    }

    fn free_slot(&mut self, index: usize) {
        self.instances[index] = None;
        self.free_instances.push(index);
    }

    /// Снимает экземпляр: сначала его команды консоли, затем освобождает слот.
    fn remove_instance(&mut self, index: usize) {
        self.remove_commands_for_owner(index);
        if let Some(instance) = self.instances[index].take() {
            self.lifecycle.borrow_mut().forget_instance(instance.serial);
            self.free_instances.push(index);
        }
    }

    /// Компилирует класс lite-скрипта вне callback: API движка на верхнем уровне недоступен.
    fn compile_registered_class(
        &mut self,
        world: &World,
        path: &str,
        source: &str,
    ) -> Result<(Table, Option<ParamSchema>), String> {
        *self.active_source.borrow_mut() = path.to_owned();
        // Как у скриптов сцены: только чтение мира, top-level `rve.find` работает, spawn и destroy нет.
        self.transaction.begin(ScriptTransactionInit {
            world,
            self_object: None,
            source: path,
            writable: false,
            actions: self.actions.frame(),
            queued_messages: (self.queued_messages.len(), self.queued_bytes),
            queued_audio: self.audio_requests.len(),
            physics: self.physics.clone(),
            physics_phase_dt: None,
            lifecycle: self.lifecycle.clone(),
            lifecycle_writes: false,
            instance_serial: 0,
            cursor: self.cursor,
        });
        let guard = ActiveCallGuard::new(
            &self.lua,
            Some(&self.call_deadline),
            &self.active_call,
            &mut self.transaction as *mut ScriptTransaction,
            &self.active_owner,
            None,
        );
        let result = lite::compile_class(&self.lua, &self.base_environment, path, source);
        drop(guard);
        self.transaction.discard();
        result
    }

    fn disable_instance(&mut self, index: usize) {
        if let Some(instance) = self.instances[index].as_mut() {
            instance.enabled = false;
        }
        self.remove_commands_for_owner(index);
    }

    /// Отключает экземпляр после ошибки и сообщает её в лог его скрипта.
    fn fail_instance(&mut self, index: usize, message: String) {
        let source = self.instances[index]
            .as_ref()
            .map(|instance| instance.source.clone())
            .unwrap_or_else(|| self.scene_name.clone());
        self.disable_instance(index);
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Error,
            source,
            message,
        });
    }

    fn disable_lite_instances(&mut self) {
        for index in 0..self.instances.len() {
            if self.instances[index]
                .as_ref()
                .is_some_and(|instance| matches!(&instance.kind, ScriptKind::Lite(_)))
            {
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
    handle.to_abi()
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

/// Heavy-скрипты библиотеки: дескриптор и схема по имени.
fn heavy_scripts(
    library: &ScriptLibrary,
) -> HashMap<String, (&'static RveScriptDesc, Option<ParamSchema>)> {
    library
        .script_names()
        .into_iter()
        .filter_map(|name| {
            library
                .script(name)
                .map(|descriptor| (name.to_owned(), (descriptor, library.schema(name).cloned())))
        })
        .collect()
}

/// Находится ли объект внутри одного из поддеревьев `targets` (включая сами корни).
fn script_transform(transform: LocalTransform) -> ScriptTransform {
    ScriptTransform {
        position: transform.position.to_array(),
        rotation: transform.rotation_deg.to_array(),
        scale: transform.scale.to_array(),
    }
}

fn event_source(scene_name: &str, object_id: &str, script_name: &str) -> String {
    format!("{scene_name}/{object_id}/{script_name}")
}

#[cfg(test)]
mod audio_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
