use crate::abi_host::{AbiPhase, AbiScriptCall, AbiScriptInstance};
use crate::actions::InputActions;
use crate::library::{ScriptLibrary, SharedLibrary};
use crate::lifecycle::{
    Lifecycle, MAX_SPAWNED_OBJECTS_PER_FRAME, PrefabCatalog, PrefabKind, PrefabRequest, PrefabSlot,
    PrefabSpec, PrefabStatus, SpawnArgs, SpawnRequest, SpawnedObject, WorldChanges,
    check_spawn_request, prefab_display, prefab_key,
};
use crate::lite::{
    self, ActiveCall, ActiveOwner, ActiveSource, CommandGeneration, EventQueue, LitePhysicsEvent,
    LuaCommands, LuaInstance, LuaInstanceInit, RegisteredCommandCallback, ScriptEventQueue,
};
use crate::net::{
    InboundBudget, MAX_INBOUND_MESSAGES_PER_PEER, NetErrorLog, NetObjectState, NetScriptState,
    networked_template, validate_message,
};
use crate::physics::PhysicsScriptState;
use crate::save_writer::SaveWriter;
use crate::scene_state::{SceneState, SharedSceneState, canonical_scene_path};
use crate::script_refs::{ScriptRefCapabilities, ScriptRefEntry, ScriptRefs};
use crate::timeline::{CoroutineWait, DueTimer, ScriptCoroutine, ScriptTimeline, TimerCallback};
use crate::transaction::{
    MAX_MESSAGE_BYTES_PER_FRAME, MAX_MESSAGES_PER_FRAME, MessageTarget, PendingMessage,
    ScriptObjectRef, ScriptTransaction, ScriptTransactionInit,
};
use crate::types::{
    RumbleRequest, ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptFrameTime, ScriptInput,
    ScriptParams, ScriptTime, ScriptTransform,
};
use crate::ui::{QueuedUiEvent, UiDelivery, UiScriptState, owner_key, queue_live};
use engine_console::{CommandCatalog, CommandLine, CommandSpec};
use engine_files::VirtualPath;
use engine_gui::document::UiDocument;
use engine_gui::events::UiEvent;
use engine_gui::system::UiSystem;
use engine_physics::{
    FixedStepClock, PhysicsCommand, PhysicsEvent, PhysicsSceneBinding, QuerySnapshot,
};
use engine_project::InputSettings;
use engine_scene::{
    AnimationRequest, AudioRequest, ComponentKind, LocalTransform, NetEvent, NetId, NetMode,
    NetRequest, ObjectHandle, ParamKind, ParamSchema, ParamValue, PeerId, PrefabTemplate,
    RigidBodyKind, SceneReplacement, SpawnOptions, World,
};
use mlua::{Error as LuaError, Function, Lua, Table, Value};
use rve_sdk::abi::RveScriptDesc;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::path::Path;
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
    /// `false` после ошибки или прерывания: экземпляр больше не вызывается.
    enabled: bool,
    /// Флаг `script:set_enabled`; меняется коммитом транзакции.
    user_enabled: bool,
    /// Экземпляр сейчас работает: включён, не отключён ошибкой и его объект действующе активен.
    /// Определяет доставку update, сообщений, физики и сети, а также `on_enable` и `on_disable`.
    running: bool,
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
    /// Экземпляры, существовавшие в начале кадра; новые spawn запускаются в следующем кадре.
    frame_serial_limit: u64,
    /// Скомпилированные классы lite-скриптов по путям; общие для сцены и префабов.
    classes: HashMap<String, (Table, Option<ParamSchema>)>,
    /// Heavy-скрипты библиотеки по имени: дескриптор и схема параметров.
    heavy: HashMap<String, (&'static RveScriptDesc, Option<ParamSchema>)>,
    /// ABI версия библиотеки `heavy`; определяет, какие виды событий модуль может получить.
    heavy_abi_version: u32,
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
    /// Справочник экземпляров для `obj:script`; общий с транзакциями.
    script_refs: ScriptRefs,
    /// Объявленные сцены проекта, текущая сцена и запрос `rve.scene.load`; общие с транзакциями.
    scene_state: SharedSceneState,
    save_values: Rc<RefCell<BTreeMap<String, ParamValue>>>,
    save_flush_requested: Rc<Cell<bool>>,
    save_generation: Rc<Cell<u64>>,
    save_writer: Option<SaveWriter>,
    persisted_save_generation: u64,
    pending_save_generations: BTreeSet<u64>,
    save_write_failed: bool,
    timeline: Rc<RefCell<ScriptTimeline>>,
    time_scale: Rc<Cell<f32>>,
    fixed_step: FixedStepClock,
    frame_time: ScriptFrameTime,
    due_timers: Vec<DueTimer>,
    due_coroutines: Vec<ScriptCoroutine>,
    queued_messages: Vec<PendingMessage>,
    queued_bytes: usize,
    audio_requests: Vec<AudioRequest>,
    animation_requests: Vec<AnimationRequest>,
    particle_requests: Vec<engine_scene::world::ParticleCommand>,
    rumble_requests: Vec<RumbleRequest>,
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
    /// Режим сети, участники и сетевые атрибуты объектов, общие с транзакциями.
    net: Rc<RefCell<NetScriptState>>,
    /// Сетевые запросы скриптов, зафиксированные с прошлого drain.
    net_requests: Vec<NetRequest>,
    /// Принятые сетевые события, доставляемые в начале следующего update.
    net_events: Vec<NetEvent>,
    inbound: InboundBudget,
    net_errors: NetErrorLog,
    /// Ошибки callback по участнику с прошлого drain; сообщения от одного пира.
    peer_faults: HashMap<PeerId, u32>,
    /// Игровой UI: `UiSystem`, документы, канвасы компонентов и очередь событий; общий с транзакцией.
    ui: Rc<RefCell<UiScriptState>>,
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
    /// Переход экземпляра в работу; вызывается до `Start` при первом запуске.
    Enable,
    /// Выход экземпляра из работы: флаг выключен или объект стал неактивным.
    Disable,
    /// `on_disable` перед `Destroy` активного экземпляра при уничтожении или выгрузке сцены.
    /// Как и `Destroy`, не допускает записей lifecycle.
    TeardownDisable,
    FixedUpdate(f32),
    Update(f32),
    LateUpdate(f32),
    Message,
    Timer,
    Coroutine,
    PhysicsEvent(ScriptPhysicsEvent),
    /// Сетевое событие; его данные передаются в `call_instance` отдельно.
    Net,
    /// Событие UI; его данные передаются в `call_instance` отдельно.
    Ui,
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
    TriggerStay,
}

impl ScriptPhysicsEventKind {
    fn callback_name(self) -> &'static str {
        match self {
            Self::CollisionEnter => "on_collision_enter",
            Self::CollisionExit => "on_collision_exit",
            Self::TriggerEnter => "on_trigger_enter",
            Self::TriggerExit => "on_trigger_exit",
            Self::TriggerStay => "on_trigger_stay",
        }
    }

    fn abi_kind(self) -> u32 {
        match self {
            Self::CollisionEnter => rve_sdk::abi::RVE_COLLISION_ENTER,
            Self::CollisionExit => rve_sdk::abi::RVE_COLLISION_EXIT,
            Self::TriggerEnter => rve_sdk::abi::RVE_TRIGGER_ENTER,
            Self::TriggerExit => rve_sdk::abi::RVE_TRIGGER_EXIT,
            Self::TriggerStay => rve_sdk::abi::RVE_TRIGGER_STAY,
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
        let ui = Rc::new(RefCell::new(UiScriptState::default()));
        let active_call = Rc::new(Cell::new(std::ptr::null_mut()));
        let active_owner = Rc::new(Cell::new(None));
        let commands = Rc::new(RefCell::new(BTreeMap::new()));
        let commands_generation = Rc::new(Cell::new(0));
        let timeline = Rc::new(RefCell::new(ScriptTimeline::default()));
        let time_scale = Rc::new(Cell::new(1.0));
        let save_values = Rc::new(RefCell::new(BTreeMap::new()));
        let save_flush_requested = Rc::new(Cell::new(false));
        let save_generation = Rc::new(Cell::new(0));
        let call_deadline = lite::CallDeadline::new(call_time_limit, interrupt.interrupted.clone());
        let lua_state = lite::create_lua(
            events.clone(),
            active_source.clone(),
            active_call.clone(),
            active_owner.clone(),
            commands.clone(),
            commands_generation.clone(),
            call_deadline,
            time_scale.clone(),
        )
        .map_err(|error| ScriptLoadError::LuaHost(error.to_string()))?;
        let lua = lua_state.lua;
        let base_environment = lua_state.base_environment;
        let loaded_library = library.and_then(ScriptLibrary::shared_library);
        let heavy = library.map(heavy_scripts).unwrap_or_default();
        let heavy_abi_version = library.map_or(0, ScriptLibrary::abi_version);
        let script_refs = ScriptRefs::default();
        let mut transaction = ScriptTransaction::with_ui(ui.clone());
        transaction.attach_script_refs(script_refs.clone());
        let scene_state = Rc::new(RefCell::new(SceneState {
            current: scene_name.to_owned(),
            ..SceneState::default()
        }));
        transaction.attach_scene_state(scene_state.clone());
        transaction.attach_time_state(timeline.clone(), time_scale.clone());
        transaction.attach_save_state(
            save_values.clone(),
            save_flush_requested.clone(),
            save_generation.clone(),
        );
        let fixed_step = FixedStepClock::new(60, 8).expect("default fixed-step config is valid");
        let mut host = Self {
            lua,
            base_environment,
            instances: Vec::new(),
            free_instances: Vec::new(),
            next_serial: 0,
            frame_serial_limit: u64::MAX,
            classes: HashMap::new(),
            heavy,
            heavy_abi_version,
            lifecycle: Rc::new(RefCell::new(lifecycle)),
            frame_spawned: 0,
            _loaded_library: loaded_library,
            events,
            active_source,
            active_call,
            active_owner,
            commands,
            commands_generation,
            transaction,
            script_refs,
            scene_state,
            save_values,
            save_flush_requested,
            save_generation,
            save_writer: None,
            persisted_save_generation: 0,
            pending_save_generations: BTreeSet::new(),
            save_write_failed: false,
            timeline,
            time_scale,
            fixed_step,
            frame_time: ScriptFrameTime::default(),
            due_timers: Vec::with_capacity(crate::timeline::MAX_SCRIPT_TIMERS),
            due_coroutines: Vec::with_capacity(crate::timeline::MAX_SCRIPT_COROUTINES),
            queued_messages: Vec::with_capacity(MAX_MESSAGES_PER_FRAME),
            queued_bytes: 0,
            audio_requests: Vec::new(),
            animation_requests: Vec::new(),
            particle_requests: Vec::new(),
            rumble_requests: Vec::new(),
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
            net: Rc::new(RefCell::new(NetScriptState::default())),
            net_requests: Vec::new(),
            net_events: Vec::new(),
            inbound: InboundBudget::default(),
            net_errors: NetErrorLog::default(),
            peer_faults: HashMap::new(),
            ui,
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
                        queued_rumble: host.rumble_requests.len(),
                        queued_animation: host.animation_requests.len(),
                        physics: host.physics.clone(),
                        net: host.net.clone(),
                        queued_net: host.net_requests.len(),
                        physics_phase_dt: None,
                        lifecycle: host.lifecycle.clone(),
                        lifecycle_writes: false,
                        instance_serial: 0,
                        instance_enabled: true,
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
                let object_refs =
                    resolve_object_refs(schema.as_ref(), &params, world).map_err(|message| {
                        ScriptLoadError::Lite {
                            path: path.to_owned(),
                            message: format!("object {:?}: {message}", object.id),
                        }
                    })?;
                let instance = LuaInstance::new(
                    &host.lua,
                    class,
                    LuaInstanceInit {
                        object_id: &object.id,
                        object_name: &object.name,
                        params: &params,
                        property_names: schema
                            .as_ref()
                            .map(|schema| {
                                schema
                                    .specs()
                                    .iter()
                                    .map(|spec| spec.name.clone())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        transform,
                        object_refs: &object_refs,
                        object_handle: object.handle,
                    },
                    host.active_call.clone(),
                )
                .map_err(|error| ScriptLoadError::Lite {
                    path: path.to_owned(),
                    message: error.to_string(),
                })?;
                let serial = host.next_serial();
                host.register_script_ref(
                    serial,
                    object.handle,
                    path,
                    schema.as_ref(),
                    &params,
                    ScriptRefCapabilities {
                        property_write_supported: true,
                        enabled: script.enabled,
                    },
                );
                host.insert_instance(ScriptInstance {
                    handle: object.handle,
                    source: event_source(scene_name, &object.id, path),
                    kind: ScriptKind::Lite(instance),
                    enabled: true,
                    user_enabled: script.enabled,
                    running: false,
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
                    let object_refs = resolve_object_refs(schema.as_ref(), &params, world)
                        .map_err(|message| ScriptLoadError::Heavy {
                            object_id: object.id.clone(),
                            name: script.name.clone(),
                            message,
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
                        queued_rumble: host.rumble_requests.len(),
                        queued_animation: host.animation_requests.len(),
                        physics: host.physics.clone(),
                        net: host.net.clone(),
                        queued_net: host.net_requests.len(),
                        physics_phase_dt: None,
                        lifecycle: host.lifecycle.clone(),
                        lifecycle_writes: false,
                        instance_serial: serial,
                        instance_enabled: true,
                        cursor: host.cursor,
                    });
                    let result = AbiScriptInstance::create(
                        descriptor,
                        host.heavy_abi_version,
                        &params,
                        &object_refs,
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
                    host.register_script_ref(
                        serial,
                        object.handle,
                        &script.name,
                        schema.as_ref(),
                        &params,
                        ScriptRefCapabilities {
                            property_write_supported: descriptor.set_property.is_some(),
                            enabled: script.enabled,
                        },
                    );
                    host.instances[owner] = Some(ScriptInstance {
                        handle: object.handle,
                        source,
                        kind: ScriptKind::Heavy(HeavyInstance::Abi(instance)),
                        enabled: true,
                        user_enabled: script.enabled,
                        running: false,
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

    /// Replaces save values before callbacks start. Save files are owned by the runtime host.
    pub fn set_save_values(&mut self, values: BTreeMap<String, ParamValue>) -> Result<(), String> {
        if self.started {
            return Err("save values can only be loaded before scripts start".to_owned());
        }
        crate::transaction::validate_save_values(&values)?;
        *self.save_values.borrow_mut() = values;
        Ok(())
    }

    /// Loads this project's save file and enables asynchronous persistence before callbacks start.
    pub fn load_save_file(&mut self, path: &Path) -> Result<(), String> {
        if self.started || self.destroyed || self.save_writer.is_some() {
            return Err("save file must be attached once before scripts start".to_owned());
        }
        let (writer, values) = match SaveWriter::open(path) {
            Ok(opened) => opened,
            Err(error) => {
                self.report_save_failure(format!(
                    "save persistence unavailable; using in-memory data and leaving the file unchanged: {error}"
                ));
                return Err(error);
            }
        };
        self.set_save_values(values)?;
        self.persisted_save_generation = self.save_generation();
        self.pending_save_generations.clear();
        self.save_writer = Some(writer);
        self.report_save_recovered();
        Ok(())
    }

    /// Returns the committed save data for persistence by the runtime.
    pub fn save_values(&self) -> BTreeMap<String, ParamValue> {
        self.save_values.borrow().clone()
    }

    /// Generation of committed save mutations, for nonblocking persistence outside callbacks.
    pub fn save_generation(&self) -> u64 {
        self.save_generation.get()
    }

    /// Whether an explicit `save.flush()` request was committed since the last check.
    pub fn take_save_flush_requested(&self) -> bool {
        self.save_flush_requested.replace(false)
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
        self.check_prefab_template(world, &template, sources)?;

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

    /// Проверяет быстрые префабы сцены, которую загружает `rve.scene.load`, не меняя состояние
    /// префабов. Слоты `catalog`, которых ещё нет, попадут в список; тяжёлый слот с тем же путём
    /// — отказ, потому что проектный список считает префаб тяжёлым. Шаблон уже резидентного
    /// префаба пропускается: экземпляры прежней сцены живут со своим шаблоном. Классы, которые
    /// скомпилированы для проверки, попадают в кеш; кеш не виден скриптам.
    pub fn check_scene_prefabs(
        &mut self,
        world: &World,
        catalog: &PrefabCatalog,
        templates: &BTreeMap<String, Arc<PrefabTemplate>>,
        sources: &BTreeMap<String, String>,
    ) -> Result<ScenePrefabs, ScriptLoadError> {
        let mut slots = Vec::new();
        {
            let lifecycle = self.lifecycle.borrow();
            for (path, spec) in catalog.iter() {
                let key = prefab_key(path).map_err(|message| ScriptLoadError::Prefab {
                    path: path.to_owned(),
                    message,
                })?;
                match lifecycle.prefabs.get(&key) {
                    Some(slot) if slot.kind != PrefabKind::Fast => {
                        return Err(ScriptLoadError::Prefab {
                            path: prefab_display(&key),
                            message: "the scene declares the prefab as fast, but the project lists it as heavy".to_owned(),
                        });
                    }
                    Some(_) => {}
                    None if spec.kind == PrefabKind::Fast => slots.push((key, spec)),
                    None => {
                        return Err(ScriptLoadError::Prefab {
                            path: prefab_display(&key),
                            message: "the prefab is not in the project prefab list".to_owned(),
                        });
                    }
                }
            }
        }
        let mut registrations = Vec::new();
        for (path, template) in templates {
            let key = prefab_key(path).map_err(|message| ScriptLoadError::Prefab {
                path: path.to_owned(),
                message,
            })?;
            let status = {
                let lifecycle = self.lifecycle.borrow();
                lifecycle
                    .prefabs
                    .get(&key)
                    .map(|slot| (slot.kind, slot.status))
            };
            let status = match status {
                Some(status) => status,
                None => {
                    let planned = slots.iter().any(|(slot, _)| *slot == key);
                    if !planned {
                        return Err(ScriptLoadError::Prefab {
                            path: prefab_display(&key),
                            message: "the prefab is not in the project prefab list".to_owned(),
                        });
                    }
                    (PrefabKind::Fast, PrefabStatus::Unloaded)
                }
            };
            match status {
                (PrefabKind::Fast, PrefabStatus::Resident) => continue,
                (PrefabKind::Fast, PrefabStatus::Unloaded) => {}
                (PrefabKind::Heavy, _) => {
                    return Err(ScriptLoadError::Prefab {
                        path: prefab_display(&key),
                        message: "the scene declares the prefab as fast, but the project lists it as heavy".to_owned(),
                    });
                }
                (_, other) => {
                    return Err(ScriptLoadError::Prefab {
                        path: prefab_display(&key),
                        message: format!(
                            "the prefab is {}, not waiting for registration",
                            other.as_str()
                        ),
                    });
                }
            }
            self.check_prefab_template(world, template, sources)?;
            registrations.push((key, Arc::clone(template)));
        }
        Ok(ScenePrefabs {
            slots,
            templates: registrations,
        })
    }

    /// Применяет проверенные [`ScenePrefabs`]. Отказа не бывает: проверка прошла на этом же
    /// состоянии, а слоты не удаляются между проверкой и применением.
    pub fn apply_scene_prefabs(&mut self, prefabs: ScenePrefabs) {
        let mut lifecycle = self.lifecycle.borrow_mut();
        for (key, spec) in prefabs.slots {
            lifecycle.prefabs.insert(key, PrefabSlot::new(spec));
        }
        for (key, template) in prefabs.templates {
            let slot = lifecycle
                .prefabs
                .get_mut(&key)
                .expect("prefab slot was checked before the scene switch");
            slot.register(template);
            slot.status = PrefabStatus::Resident;
        }
    }

    /// Компилирует недостающие классы шаблона и проверяет его параметры и ссылки по `world`.
    fn check_prefab_template(
        &mut self,
        world: &World,
        template: &PrefabTemplate,
        sources: &BTreeMap<String, String>,
    ) -> Result<(), ScriptLoadError> {
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
                let params =
                    resolve_params(schema.as_ref(), &script.params).map_err(|message| {
                        ScriptLoadError::Lite {
                            path: script_path.to_owned(),
                            message: format!("object {:?}: {message}", object.id),
                        }
                    })?;
                resolve_object_refs(schema.as_ref(), &params, world).map_err(|message| {
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
                let params =
                    resolve_params(schema.as_ref(), &script.params).map_err(|message| {
                        ScriptLoadError::Heavy {
                            object_id: object.id.clone(),
                            name: script.name.clone(),
                            message,
                        }
                    })?;
                resolve_object_refs(schema.as_ref(), &params, world).map_err(|message| {
                    ScriptLoadError::Heavy {
                        object_id: object.id.clone(),
                        name: script.name.clone(),
                        message,
                    }
                })?;
            }
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

    /// Задаёт включённые сцены проекта: только они могут стать целью `rve.scene.load`.
    /// Пути принимаются с ведущим слешем и без него; неверный путь возвращает ошибку и список
    /// не меняет.
    pub fn set_enabled_scenes(&mut self, paths: &[String]) -> Result<(), String> {
        let canonical = paths
            .iter()
            .map(|path| canonical_scene_path(path))
            .collect::<Result<Vec<_>, _>>()?;
        self.scene_state.borrow_mut().enabled = canonical;
        Ok(())
    }

    /// Имя текущей сцены: путь, с которым она загружена или на который заменена.
    pub fn current_scene(&self) -> String {
        self.scene_state.borrow().current.clone()
    }

    /// Запрос `rve.scene.load` или `ctx.load_scene`, принятый последним кадром. Его должен
    /// выполнить вызывающий код после кадра: загрузить сцену и передать её в [`Self::replace_scene`].
    pub fn take_scene_request(&mut self) -> Option<String> {
        self.scene_state.borrow_mut().request.take()
    }

    /// Пишет Error, если вызывающий код не смог загрузить запрошенную сцену. Текущая сцена не меняется.
    pub fn report_scene_failure(&mut self, path: &str, message: &str) {
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Error,
            source: self.scene_name.clone(),
            message: format!("failed to load scene {path:?}: {message}"),
        });
    }

    /// Заменяет несохраняемую часть текущей сцены сценой `scene` (см. [`World::replace_nonpersistent`]).
    ///
    /// Порядок: проверка мира и классов новой сцены, затем создание её экземпляров на копии мира.
    /// Любой отказ до этого шага оставляет текущую сцену без изменений. Затем `on_disable` и
    /// `on_destroy` получают несохраняемые экземпляры, мир заменяется, учёт удалённых объектов
    /// обновляется. Сохраняемые объекты и их экземпляры сохраняют хендлы, таймеры, корутины,
    /// свойства и звук.
    pub fn replace_scene(
        &mut self,
        world: &mut World,
        scene: World,
        sources: &BTreeMap<String, String>,
        scene_name: &str,
        time: ScriptTime,
    ) -> Result<SceneReplacement, String> {
        world
            .check_scene_replacement(&scene)
            .map_err(|error| error.to_string())?;
        for object in scene.objects() {
            for script in &object.heavy_scripts {
                if !self.heavy.contains_key(&script.name) {
                    return Err(format!(
                        "object {:?}: heavy script {:?} is not registered",
                        object.id, script.name
                    ));
                }
            }
            for script in &object.lite_scripts {
                let path = script.path.as_str();
                if self.classes.contains_key(path) {
                    continue;
                }
                let source = sources
                    .get(path)
                    .ok_or_else(|| format!("lite script {path:?} source was not provided"))?;
                let class = self.compile_registered_class(world, path, source)?;
                self.classes.insert(path.to_owned(), class);
            }
        }

        // Экземпляры новой сцены создаются на копии, где несохраняемая часть уже заменена:
        // их хендлы совпадают с теми, что получит настоящий мир при замене ниже.
        let previous_name = std::mem::replace(&mut self.scene_name, scene_name.to_owned());
        let mut trial = world.clone();
        let planned = match trial.replace_nonpersistent(scene.clone()) {
            Ok(planned) => planned,
            Err(error) => {
                self.scene_name = previous_name;
                return Err(error.to_string());
            }
        };
        if let Err(message) = self.create_instances(&mut trial, &planned.added) {
            self.scene_name = previous_name;
            return Err(message);
        }

        let roots = world
            .objects()
            .iter()
            .filter(|object| !object.keep_on_load && object.parent.is_none())
            .map(|object| object.handle)
            .collect::<Vec<_>>();
        let doomed: HashSet<ObjectHandle> = world.subtree(&roots).into_iter().collect();
        let doomed_ids: HashSet<String> = doomed
            .iter()
            .filter_map(|handle| world.object(*handle))
            .map(|object| object.id.clone())
            .collect();
        self.retire_bodies(world, &doomed);
        self.call_destroy_instances(world, &doomed, time);
        // Проверка и план выполнены выше на копии с теми же слотами: отказ здесь означает ошибку
        // в инварианте, а не в данных сцены. Экземпляры новой сцены уже созданы, поэтому
        // отказ не оставляет полуприменённого состояния и должен остановить процесс.
        let replacement = world
            .replace_nonpersistent(scene)
            .expect("scene replacement was validated on a copy of the world before teardown");
        assert_eq!(
            replacement.added, planned.added,
            "scene replacement must assign the handles planned on the copy"
        );
        self.cancel_requests_of(world, &previous_name, &doomed, &doomed_ids);
        self.forget_despawned(&replacement.removed);
        self.rename_script_sources(&previous_name, scene_name);
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            lifecycle
                .changes
                .despawned
                .extend(replacement.removed.iter().copied());
            lifecycle.changes.scene_replaced = Some(replacement.clone());
        }
        self.scene_state.borrow_mut().current = scene_name.to_owned();
        Ok(replacement)
    }

    /// Снимает заявки spawn и destroy, поставленные скриптами удаляемых объектов. Источник заявки
    /// имеет вид `сцена/объект/скрипт`; заявки сохраняемых объектов остаются в очереди.
    fn cancel_requests_of(
        &mut self,
        world: &World,
        previous: &str,
        doomed: &HashSet<ObjectHandle>,
        doomed_ids: &HashSet<String>,
    ) {
        let owned = |source: &str| {
            source
                .strip_prefix(previous)
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(|rest| rest.split('/').next())
                .is_some_and(|id| doomed_ids.contains(id))
        };
        let (cancelled, kept_queued, kept_waiting) = {
            let mut lifecycle = self.lifecycle.borrow_mut();
            let (queued_cancelled, queued_kept): (Vec<_>, Vec<_>) =
                std::mem::take(&mut lifecycle.queued)
                    .into_iter()
                    .partition(|request| owned(&request.source));
            let (waiting_cancelled, waiting_kept): (Vec<_>, Vec<_>) =
                std::mem::take(&mut lifecycle.waiting)
                    .into_iter()
                    .partition(|request| owned(&request.source));
            lifecycle
                .destroys
                .retain(|request| !doomed.contains(&request.handle));
            let cancelled = queued_cancelled
                .into_iter()
                .chain(waiting_cancelled)
                .collect::<Vec<_>>();
            (cancelled, queued_kept, waiting_kept)
        };
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            lifecycle.queued = kept_queued.into_iter().collect();
            lifecycle.waiting = kept_waiting;
        }
        for request in cancelled {
            self.release_spawn(world, &request);
        }
    }

    /// Переименовывает источники экземпляров сохраняемых объектов на имя новой сцены.
    fn rename_script_sources(&mut self, previous: &str, next: &str) {
        let prefix = format!("{previous}/");
        for instance in self.instances.iter_mut().flatten() {
            if let Some(rest) = instance.source.strip_prefix(&prefix) {
                instance.source = format!("{next}/{rest}");
            }
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

    /// Регистрирует предзагруженный `.rveui`. `open` и канвасы компонентов принимают только такие
    /// документы; вызывать до [`Self::start`], чтобы канвасы сцены открылись до `on_start`.
    pub fn register_ui_document(&mut self, path: VirtualPath, document: Arc<UiDocument>) {
        self.ui
            .borrow_mut()
            .documents
            .insert(path.as_str().to_owned(), document);
    }

    /// Состояние UI для runtime и редактора: ввод, регистрация шрифтов и картинок, paint.
    pub fn ui(&self) -> Ref<'_, UiSystem> {
        Ref::map(self.ui.borrow(), |state| &state.system)
    }

    /// Изменяемое состояние UI вне callback скриптов.
    pub fn ui_mut(&mut self) -> RefMut<'_, UiSystem> {
        RefMut::map(self.ui.borrow_mut(), |state| &mut state.system)
    }

    /// Current committed scale. Changes made by a script take effect on the next host update.
    pub fn time_scale(&self) -> f32 {
        self.time_scale.get()
    }

    /// Configures fixed callbacks to use the same limits as the physics scene.
    pub fn set_fixed_step_config(
        &mut self,
        fixed_hz: u32,
        max_substeps: usize,
    ) -> Result<(), String> {
        self.fixed_step =
            FixedStepClock::new(fixed_hz, max_substeps).map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Configured scaled simulation delta used by fixed callbacks.
    pub fn fixed_dt(&self) -> f32 {
        self.fixed_step.fixed_dt()
    }

    /// Ставит события UI, например из `UiSystem::update`; они доходят до `on_ui_event`
    /// в начале следующего update. Событие канваса, закрытого к доставке, пропускается.
    pub fn queue_ui_events(&mut self, events: impl IntoIterator<Item = UiEvent>) {
        let mut state = self.ui.borrow_mut();
        let queued: Vec<QueuedUiEvent> = events
            .into_iter()
            .filter_map(|event| queue_live(&state.system, event))
            .collect();
        state.queued.extend(queued);
    }

    /// Runs on_start once. update also calls start automatically if needed.
    pub fn start(&mut self, world: &mut World, time: ScriptTime) {
        self.start_frame(
            world,
            ScriptFrameTime {
                elapsed: time.elapsed,
                unscaled_elapsed: time.elapsed,
                fixed_dt: self.fixed_step.fixed_dt(),
                scale: self.time_scale.get(),
                frame: time.frame,
                ..ScriptFrameTime::default()
            },
        );
    }

    fn start_frame(&mut self, world: &mut World, frame_time: ScriptFrameTime) {
        if self.started || self.destroyed {
            return;
        }
        self.started = true;
        self.frame_time = frame_time;
        let time = ScriptTime {
            elapsed: frame_time.elapsed,
            frame: frame_time.frame,
        };
        self.open_scene_canvases(world);
        self.sync_activity(world, time, u64::MAX);
        self.apply_lifecycle(world, time);
        self.persist_save_changes();
    }

    /// Runs on_update for enabled instances, starting the host first if needed.
    pub fn update(&mut self, world: &mut World, dt: f32, time: ScriptTime) {
        self.update_frame(
            world,
            ScriptFrameTime {
                dt,
                unscaled_dt: dt,
                elapsed: time.elapsed,
                unscaled_elapsed: time.elapsed,
                fixed_dt: self.fixed_step.fixed_dt(),
                scale: self.time_scale.get(),
                frame: time.frame,
            },
        );
    }

    /// Runs fixed, frame, message, timer/coroutine and late callbacks using one clock snapshot.
    pub fn update_frame(&mut self, world: &mut World, frame_time: ScriptFrameTime) {
        if self.destroyed {
            return;
        }
        self.frame_time = frame_time;
        let time = ScriptTime {
            elapsed: frame_time.elapsed,
            frame: frame_time.frame,
        };
        let frame_serial_limit = self.next_serial;
        if !self.started {
            self.start_frame(world, frame_time);
        }
        self.frame_serial_limit = frame_serial_limit;
        self.frame_spawned = 0;
        {
            let mut timeline = self.timeline.borrow_mut();
            timeline.snapshot_due(frame_time.frame, frame_time.elapsed);
            timeline.take_due_timers(frame_time.elapsed, &mut self.due_timers);
            timeline.take_due_coroutines(&mut self.due_coroutines);
        }
        std::mem::swap(&mut self.queued_messages, &mut self.delivery_messages);
        self.queued_bytes = 0;
        match self.fixed_step.advance(frame_time.dt) {
            Ok(substeps) => {
                let fixed_dt = self.fixed_step.fixed_dt();
                for _ in 0..substeps {
                    self.run_phase(world, Phase::FixedUpdate(fixed_dt), time);
                }
            }
            Err(error) => self.events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Error,
                source: self.scene_name.clone(),
                message: format!("invalid script frame delta: {error}"),
            }),
        }
        self.run_phase(world, Phase::Update(frame_time.dt), time);
        self.sync_after_run_phase(world, time);
        self.resume_coroutines(world, time);
        self.sync_phase_boundary(world, time);
        self.deliver_messages(world, time);
        self.sync_phase_boundary(world, time);
        self.fire_timers(world, time);
        self.sync_phase_boundary(world, time);
        self.run_phase(world, Phase::LateUpdate(frame_time.dt), time);
        self.persist_save_changes();
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
        self.net_events.clear();
        self.actions.clear_frame_transients();
        self.frame_time.elapsed = time.elapsed;
        self.frame_time.unscaled_elapsed = time.elapsed;
        if self.started {
            self.run_phase(world, Phase::Destroy, time);
        }
        self.timeline.borrow_mut().clear();
        self.due_timers.clear();
        self.due_coroutines.clear();
        self.remove_all_commands();
        self.release_all_pending(world);
        self.process_save_acknowledgements();
        if let Some(writer) = self.save_writer.take() {
            let generation = self.save_generation();
            match writer.finish(generation, self.save_values()) {
                Ok(()) => {
                    self.persisted_save_generation = self.persisted_save_generation.max(generation);
                    self.pending_save_generations
                        .retain(|pending| *pending > generation);
                    self.report_save_recovered();
                }
                Err(error) => self.report_save_failure(format!("final save flush failed: {error}")),
            }
        }
    }

    fn persist_save_changes(&mut self) {
        self.process_save_acknowledgements();
        let generation = self.save_generation();
        if generation != self.persisted_save_generation
            && !self.pending_save_generations.contains(&generation)
            && let Some(writer) = self.save_writer.as_ref()
        {
            match writer.snapshot(generation, self.save_values()) {
                Ok(true) => {
                    self.pending_save_generations.insert(generation);
                }
                Ok(false) => {}
                Err(error) => self.report_save_failure(error),
            }
        }
        if self.take_save_flush_requested() {
            if let Some(writer) = self.save_writer.as_ref() {
                let result = writer.flush(generation, self.save_values());
                self.process_save_acknowledgements();
                match result {
                    Ok(()) => {
                        self.persisted_save_generation =
                            self.persisted_save_generation.max(generation);
                        self.pending_save_generations
                            .retain(|pending| *pending > generation);
                        self.report_save_recovered();
                    }
                    Err(error) => {
                        self.report_save_failure(format!("explicit save flush failed: {error}"))
                    }
                }
            } else {
                self.report_save_failure(
                    "save.flush() was requested without a save path; data remains in memory"
                        .to_owned(),
                );
            }
        }
    }

    fn process_save_acknowledgements(&mut self) {
        let acknowledgements = self
            .save_writer
            .as_ref()
            .map(SaveWriter::drain_acknowledgements)
            .unwrap_or_default();
        for acknowledgement in acknowledgements {
            self.pending_save_generations
                .remove(&acknowledgement.generation);
            match acknowledgement.result {
                Ok(()) => {
                    self.persisted_save_generation = self
                        .persisted_save_generation
                        .max(acknowledgement.generation);
                    self.report_save_recovered();
                }
                Err(error) => self.report_save_failure(format!(
                    "save snapshot generation {} failed: {error}",
                    acknowledgement.generation
                )),
            }
        }
    }

    fn report_save_failure(&mut self, message: String) {
        if self.save_write_failed {
            return;
        }
        self.save_write_failed = true;
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Error,
            source: self.scene_name.clone(),
            message,
        });
    }

    fn report_save_recovered(&mut self) {
        if !self.save_write_failed {
            return;
        }
        self.save_write_failed = false;
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Info,
            source: self.scene_name.clone(),
            message: "save persistence recovered".to_owned(),
        });
    }

    /// Takes all log and runtime-error events accumulated since the previous drain.
    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        self.events.borrow_mut().drain()
    }

    /// Takes audio requests committed by scripts since the previous drain, in commit order.
    pub fn drain_audio_requests(&mut self) -> Vec<AudioRequest> {
        std::mem::take(&mut self.audio_requests)
    }

    /// Takes animator requests committed by scripts since the previous drain, in commit order.
    pub fn drain_particle_commands(&mut self) -> Vec<engine_scene::world::ParticleCommand> {
        std::mem::take(&mut self.particle_requests)
    }

    pub fn drain_animation_requests(&mut self) -> Vec<AnimationRequest> {
        std::mem::take(&mut self.animation_requests)
    }

    /// Takes network requests committed by scripts since the previous drain, in commit order.
    pub fn drain_network_requests(&mut self) -> Vec<NetRequest> {
        std::mem::take(&mut self.net_requests)
    }

    /// Takes the failed-callback count of each peer since the previous drain, sorted by peer.
    /// Only messages from a remote peer add to the count; the bridge applies its disconnect rule.
    pub fn drain_peer_faults(&mut self) -> Vec<(PeerId, u32)> {
        let mut faults: Vec<(PeerId, u32)> = self.peer_faults.drain().collect();
        faults.sort_unstable_by_key(|(peer, _)| *peer);
        faults
    }

    /// Sets the network mode, this process's peer and the connected remote peers.
    ///
    /// Until the bridge calls this, the host is `Offline`: `is_server()` and `has_authority()` are
    /// true, `is_client()` is false, and `send` or `set_net_owner` stage no request.
    pub fn set_network_state(
        &mut self,
        mode: NetMode,
        local_peer: Option<PeerId>,
        peers: Vec<PeerId>,
    ) {
        self.net.borrow_mut().set_network(mode, local_peer, peers);
    }

    /// Sets the network number and owner of an object. Scripts see the values from the next callback.
    pub fn set_object_net(&mut self, object: ObjectHandle, net_id: NetId, owner: Option<PeerId>) {
        self.net
            .borrow_mut()
            .set_object(object, NetObjectState { net_id, owner });
    }

    /// Removes the network attributes of an object that is no longer replicated.
    pub fn clear_object_net(&mut self, object: ObjectHandle) {
        self.net.borrow_mut().clear_object(object);
    }

    /// Queues network events for the beginning of the next update, after physics events.
    ///
    /// A message for a dead object is dropped. A message beyond [`MAX_INBOUND_MESSAGES_PER_PEER`]
    /// or its byte budget is dropped for the rest of the update, with one warning per peer. A
    /// malformed message is dropped and counts as a fault of its peer.
    pub fn queue_network_events(
        &mut self,
        world: &World,
        events: impl IntoIterator<Item = NetEvent>,
    ) {
        for event in events {
            match event {
                NetEvent::Message {
                    peer,
                    object,
                    name,
                    value,
                } => {
                    if object.is_some_and(|object| !world.is_alive(object)) {
                        continue;
                    }
                    let size = match validate_message(&name, &value) {
                        Ok(size) => size,
                        Err(reason) => {
                            self.count_peer_fault(peer);
                            self.warn(format!("сообщение участника {peer} отброшено: {reason}"));
                            continue;
                        }
                    };
                    if !self.inbound.admit(peer, size) {
                        if self.inbound.first_refusal(peer) {
                            self.warn(format!(
                                "сообщения участника {peer} сверх лимита {MAX_INBOUND_MESSAGES_PER_PEER} за update отброшены"
                            ));
                        }
                        continue;
                    }
                    self.net_events.push(NetEvent::Message {
                        peer,
                        object,
                        name,
                        value,
                    });
                }
                other => self.net_events.push(other),
            }
        }
    }

    /// Creates the root of a prefab for the network bridge. The request passes the same checks and
    /// goes through the same application path as `rve.spawn`, but it is applied before this call
    /// returns. The root is in `World` afterwards, and its `on_start` runs in the next update. A heavy
    /// prefab that is still loading waits for its load. A refusal rolls back the reservation and
    /// returns `Err`; the reason is also an error event of the scene.
    pub fn spawn_external(
        &mut self,
        world: &mut World,
        prefab: &str,
        options: SpawnOptions,
    ) -> Result<ObjectHandle, String> {
        let prefab = prefab_key(prefab)?;
        let args = SpawnArgs {
            options,
            velocity: None,
        };
        check_spawn_request(
            world,
            &self.lifecycle.borrow(),
            0,
            &HashMap::new(),
            &prefab,
            &args,
        )?;
        let root = world.reserve_object();
        let seq = {
            let mut lifecycle = self.lifecycle.borrow_mut();
            if let Some(id) = &args.options.id {
                lifecycle.pending_ids.insert(id.clone(), root);
            }
            lifecycle.next_seq()
        };
        self.lifecycle.borrow_mut().queued.push_back(SpawnRequest {
            seq,
            root,
            prefab: prefab.clone(),
            args,
            source: self.scene_name.clone(),
            from_script: false,
        });
        self.apply_queued_spawns(world);
        let waiting = self
            .lifecycle
            .borrow()
            .waiting
            .iter()
            .any(|request| request.root == root);
        if world.is_alive(root) || waiting {
            Ok(root)
        } else {
            Err(format!(
                "spawn of prefab {:?} was refused; see the scene log",
                prefab_display(&prefab)
            ))
        }
    }

    /// Removes the subtree of `root` without a delay, as `rve.destroy` does: `on_destroy` of its
    /// started instances runs first. A pending spawn of `root` is cancelled. Returns `false` when
    /// `root` is neither alive nor pending, or when the removal was refused, for example for the
    /// active camera. Removed objects are listed in [`WorldChanges::despawned`].
    pub fn despawn_external(&mut self, world: &mut World, root: ObjectHandle) -> bool {
        let pending = self
            .lifecycle
            .borrow()
            .waiting
            .iter()
            .any(|request| request.root == root);
        if !world.is_alive(root) && !pending {
            return false;
        }
        let time = self.current_time;
        self.destroy_objects(world, vec![root], time);
        !world.is_alive(root)
    }

    /// Applies queued spawn requests in `seq` order. Each request is taken one at a time, so a
    /// refused request sees the rest of the queue.
    fn apply_queued_spawns(&mut self, world: &mut World) {
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
    }

    /// Takes gamepad rumble requests committed by scripts since the previous drain, in commit order.
    pub fn take_rumble_requests(&mut self) -> Vec<RumbleRequest> {
        std::mem::take(&mut self.rumble_requests)
    }

    /// Queues `animation_finished` messages for the next update, before script messages of
    /// that frame. The sender is the animated object; the value is the motion name.
    pub fn queue_animation_events(
        &mut self,
        world: &World,
        events: impl IntoIterator<Item = (ObjectHandle, String)>,
    ) {
        for (object, motion) in events {
            let Some(sender) = world.object(object) else {
                continue;
            };
            let message = PendingMessage::engine_message(
                sender.id.clone(),
                MessageTarget::Object(object),
                "animation_finished",
                ParamValue::String(motion),
            );
            if self.queued_messages.len() >= MAX_MESSAGES_PER_FRAME
                || self.queued_bytes.saturating_add(message.size()) > MAX_MESSAGE_BYTES_PER_FRAME
            {
                continue;
            }
            self.queued_bytes = self.queued_bytes.saturating_add(message.size());
            self.queued_messages.push(message);
        }
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
            queued_rumble: self.rumble_requests.len(),
            queued_animation: self.animation_requests.len(),
            physics: self.physics.clone(),
            net: self.net.clone(),
            queued_net: self.net_requests.len(),
            physics_phase_dt: None,
            lifecycle: self.lifecycle.clone(),
            lifecycle_writes: true,
            instance_serial: serial,
            instance_enabled: true,
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
            &mut self.animation_requests,
            &mut self.net_requests,
            &mut self.rumble_requests,
            &mut self.particle_requests,
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
        if !matches!(phase, Phase::Destroy | Phase::TeardownDisable) {
            self.sync_activity(world, time, self.frame_serial_limit);
        }
        if matches!(phase, Phase::Update(_)) {
            let started = Instant::now();
            self.deliver_physics_events(world, time);
            self.deliver_trigger_stays(world, time);
            profile.physics_events = started.elapsed();
            self.deliver_ui_events(world, time);
            self.deliver_net_events(world, time);
        }

        let time_error = lite::set_time(&self.lua_time, self.frame_time)
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
            if matches!(phase, Phase::Destroy) {
                if !instance.started {
                    continue;
                }
            } else if !instance.running {
                continue;
            }
            let is_lite = matches!(&instance.kind, ScriptKind::Lite(_));
            if is_lite && self.interrupt.is_interrupted() {
                observed_interrupt = true;
                self.disable_instance(index);
                continue;
            }
            let started = Instant::now();
            let outcome = if matches!(phase, Phase::Destroy) {
                self.call_destroy(index, world, time, time_error.as_deref())
            } else {
                self.call_instance(
                    index,
                    world,
                    phase,
                    time,
                    None,
                    None,
                    None,
                    time_error.as_deref(),
                    None,
                    None,
                )
            };
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

    /// Сверяет работу экземпляров с флагами и действующей активностью объектов. Переход в работу
    /// вызывает `on_enable`, а первый запуск ещё и `on_start`; выход из работы — `on_disable`.
    /// Каждый экземпляр проверяется один раз за фазу: флаги, которые callback меняет, действуют
    /// со следующей фазы, поэтому цикл переключений не возникает.
    fn sync_activity(&mut self, world: &mut World, time: ScriptTime, serial_limit: u64) {
        self.current_time = time;
        self.lifecycle.borrow_mut().now = time.elapsed;
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
            .err()
            .map(|error| error.to_string());
        for index in 0..self.instances.len() {
            let Some(instance) = self.instances[index].as_ref() else {
                continue;
            };
            if instance.serial > serial_limit && !instance.running {
                continue;
            }
            let should_run = instance.enabled
                && instance.user_enabled
                && world.active_in_hierarchy(instance.handle) == Some(true);
            if should_run == instance.running {
                continue;
            }
            if should_run {
                let first_start = !instance.started;
                if let Some(instance) = self.instances[index].as_mut() {
                    instance.running = true;
                }
                self.run_transition(index, world, Phase::Enable, time, time_error.as_deref());
                let still_running = self.instances[index]
                    .as_ref()
                    .is_some_and(|instance| instance.enabled && instance.running);
                if first_start && still_running {
                    if let Some(instance) = self.instances[index].as_mut() {
                        instance.started = true;
                    }
                    self.run_transition(index, world, Phase::Start, time, time_error.as_deref());
                }
            } else {
                let Some(instance) = self.instances[index].as_mut() else {
                    continue;
                };
                // Экземпляр, остановленный ошибкой, уже не получает `on_disable`.
                let notify = instance.enabled;
                let serial = instance.serial;
                instance.running = false;
                if notify {
                    self.run_transition(index, world, Phase::Disable, time, time_error.as_deref());
                }
                self.timeline.borrow_mut().cancel_owner(serial);
            }
        }
    }

    fn sync_phase_boundary(&mut self, world: &mut World, time: ScriptTime) {
        self.apply_lifecycle(world, time);
        self.sync_activity(world, time, self.frame_serial_limit);
        self.apply_lifecycle(world, time);
    }

    fn sync_after_run_phase(&mut self, world: &mut World, time: ScriptTime) {
        self.sync_activity(world, time, self.frame_serial_limit);
        self.apply_lifecycle(world, time);
    }

    /// Уничтожение экземпляра. Активный экземпляр сначала получает `on_disable` (порядок Unity:
    /// OnDisable, затем OnDestroy); неактивный уже отключён при выходе из работы и получает
    /// только `on_destroy`. Ошибка или прерывание `on_disable` пропускают `on_destroy`.
    fn call_destroy(
        &mut self,
        index: usize,
        world: &mut World,
        time: ScriptTime,
        time_error: Option<&str>,
    ) -> CallOutcome {
        let running = self.instances[index]
            .as_ref()
            .is_some_and(|instance| instance.running);
        if running {
            if let Some(instance) = self.instances[index].as_mut() {
                instance.running = false;
            }
            match self.call_instance(
                index,
                world,
                Phase::TeardownDisable,
                time,
                None,
                None,
                None,
                time_error,
                None,
                None,
            ) {
                CallOutcome::Committed => {}
                outcome => return outcome,
            }
        }
        self.call_instance(
            index,
            world,
            Phase::Destroy,
            time,
            None,
            None,
            None,
            time_error,
            None,
            None,
        )
    }

    fn run_transition(
        &mut self,
        index: usize,
        world: &mut World,
        phase: Phase,
        time: ScriptTime,
        time_error: Option<&str>,
    ) {
        match self.call_instance(
            index, world, phase, time, None, None, None, time_error, None, None,
        ) {
            CallOutcome::Committed => {}
            CallOutcome::Interrupted => self.disable_instance(index),
            CallOutcome::Failed(message) => self.fail_instance(index, message),
        }
    }

    fn deliver_messages(&mut self, world: &mut World, time: ScriptTime) {
        if self.delivery_messages.is_empty() {
            return;
        }
        self.delivery_messages.reverse();
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
            .err()
            .map(|error| error.to_string());
        while let Some(message) = self.delivery_messages.pop() {
            for index in 0..self.instances.len() {
                let Some(instance) = self.instances[index].as_ref() else {
                    continue;
                };
                let is_target = match message.target {
                    MessageTarget::Object(handle) => instance.handle == handle,
                    MessageTarget::Instance { serial, .. } => instance.serial == serial,
                    MessageTarget::Broadcast => true,
                };
                if !instance.enabled || !instance.running || !is_target {
                    continue;
                }
                match self.call_instance(
                    index,
                    world,
                    Phase::Message,
                    time,
                    Some(&message),
                    None,
                    None,
                    time_error.as_deref(),
                    None,
                    None,
                ) {
                    CallOutcome::Committed => {}
                    CallOutcome::Interrupted => self.disable_instance(index),
                    CallOutcome::Failed(message) => self.fail_instance(index, message),
                }
            }
        }
    }

    fn resume_coroutines(&mut self, world: &mut World, time: ScriptTime) {
        let coroutines = std::mem::take(&mut self.due_coroutines);
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
            .err()
            .map(|error| error.to_string());
        for coroutine in &coroutines {
            let Some(index) = self.instances.iter().position(|instance| {
                instance
                    .as_ref()
                    .is_some_and(|instance| instance.serial == coroutine.owner)
            }) else {
                self.timeline.borrow_mut().finish_due_coroutine(coroutine);
                continue;
            };
            if !self.instances[index]
                .as_ref()
                .is_some_and(|instance| instance.enabled && instance.running)
            {
                self.timeline.borrow_mut().finish_due_coroutine(coroutine);
                continue;
            }
            match self.call_instance(
                index,
                world,
                Phase::Coroutine,
                time,
                None,
                None,
                None,
                time_error.as_deref(),
                None,
                Some(coroutine),
            ) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
            }
            self.timeline.borrow_mut().finish_due_coroutine(coroutine);
        }
        self.due_coroutines.clear();
        self.due_coroutines = coroutines;
    }

    fn fire_timers(&mut self, world: &mut World, time: ScriptTime) {
        let timers = std::mem::take(&mut self.due_timers);
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
            .err()
            .map(|error| error.to_string());
        for timer in &timers {
            if !self.timeline.borrow().timer_exists(timer.id, timer.owner) {
                continue;
            }
            let Some(index) = self.instances.iter().position(|instance| {
                instance
                    .as_ref()
                    .is_some_and(|instance| instance.serial == timer.owner)
            }) else {
                self.timeline.borrow_mut().finish_due_timer(timer);
                continue;
            };
            if !self.instances[index]
                .as_ref()
                .is_some_and(|instance| instance.enabled && instance.running)
            {
                self.timeline.borrow_mut().finish_due_timer(timer);
                continue;
            }
            match self.call_instance(
                index,
                world,
                Phase::Timer,
                time,
                None,
                None,
                None,
                time_error.as_deref(),
                Some(timer),
                None,
            ) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
            }
            self.timeline.borrow_mut().finish_due_timer(timer);
        }
        self.due_timers.clear();
        self.due_timers = timers;
    }

    /// Доставляет принятые сетевые события: каждое событие отдельной транзакцией на каждый экземпляр.
    /// Ошибка callback не отключает экземпляр; она записывается в журнал и считается по участнику.
    fn deliver_net_events(&mut self, world: &mut World, time: ScriptTime) {
        let events = std::mem::take(&mut self.net_events);
        self.inbound.reset();
        if events.is_empty() {
            return;
        }
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
            .err()
            .map(|error| error.to_string());
        for event in events {
            let peer = match &event {
                NetEvent::Message { peer, .. } => Some(*peer),
                NetEvent::PeerConnected { .. } | NetEvent::PeerDisconnected { .. } => None,
            };
            let targets: Vec<usize> = (0..self.instances.len())
                .filter(|&index| {
                    let Some(instance) = self.instances[index].as_ref() else {
                        return false;
                    };
                    let addressed = match &event {
                        NetEvent::Message { object, .. } => {
                            object.is_none_or(|object| object == instance.handle)
                        }
                        NetEvent::PeerConnected { .. } | NetEvent::PeerDisconnected { .. } => true,
                    };
                    instance.enabled
                        && instance.running
                        && addressed
                        && self.has_net_callback(index, &event)
                })
                .collect();
            for index in targets {
                // Экземпляр мог отключиться предыдущим событием этого же кадра.
                if !self.instances[index]
                    .as_ref()
                    .is_some_and(|instance| instance.enabled && instance.running)
                {
                    continue;
                }
                match self.call_instance(
                    index,
                    world,
                    Phase::Net,
                    time,
                    None,
                    Some(&event),
                    None,
                    time_error.as_deref(),
                    None,
                    None,
                ) {
                    CallOutcome::Committed => {}
                    CallOutcome::Interrupted => self.disable_instance(index),
                    CallOutcome::Failed(message) => {
                        self.net_callback_failed(index, peer, message);
                    }
                }
            }
        }
    }

    /// Есть ли у экземпляра callback для этого события сети.
    fn has_net_callback(&self, index: usize, event: &NetEvent) -> bool {
        let Some(instance) = self.instances[index].as_ref() else {
            return false;
        };
        match &instance.kind {
            ScriptKind::Lite(lite_instance) => lite_instance
                .has_callback(net_callback_name(event))
                .unwrap_or(true),
            ScriptKind::Heavy(HeavyInstance::Abi(script)) => script.has_net_event(),
        }
    }

    /// Ошибка net-callback: экземпляр остаётся включённым, ошибка попадает в журнал с лимитом и,
    /// для сообщения, увеличивает счётчик участника.
    fn net_callback_failed(&mut self, index: usize, peer: Option<PeerId>, message: String) {
        let source = self.instances[index]
            .as_ref()
            .map(|instance| instance.source.clone())
            .unwrap_or_else(|| self.scene_name.clone());
        self.net_errors
            .report(self.current_time.elapsed, &source, message, &self.events);
        if let Some(peer) = peer {
            self.count_peer_fault(peer);
        }
    }

    fn count_peer_fault(&mut self, peer: PeerId) {
        let count = self.peer_faults.entry(peer).or_insert(0);
        *count = count.saturating_add(1);
    }

    fn warn(&mut self, message: String) {
        self.events.borrow_mut().push(ScriptEvent {
            level: ScriptEventLevel::Warn,
            source: self.scene_name.clone(),
            message,
        });
    }

    /// Доставляет `on_ui_event` экземплярам владельца канваса; каждая доставка — отдельная транзакция.
    fn deliver_ui_events(&mut self, world: &mut World, time: ScriptTime) {
        // События, поставленные во время доставки, ждут следующего update.
        let events = std::mem::take(&mut self.ui.borrow_mut().queued);
        if events.is_empty() {
            return;
        }
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
            .err()
            .map(|error| error.to_string());
        for QueuedUiEvent { owner, delivery } in events {
            let object = owner;
            let targets: Vec<usize> = (0..self.instances.len())
                .filter(|&index| {
                    self.instances[index].as_ref().is_some_and(|instance| {
                        instance.enabled && instance.running && instance.handle == object
                    }) && self.has_ui_callback(index)
                })
                .collect();
            for index in targets {
                // Экземпляр мог отключиться предыдущим событием этого же кадра.
                if !self.instances[index]
                    .as_ref()
                    .is_some_and(|instance| instance.enabled && instance.running)
                {
                    continue;
                }
                match self.call_instance(
                    index,
                    world,
                    Phase::Ui,
                    time,
                    None,
                    None,
                    Some(&delivery),
                    time_error.as_deref(),
                    None,
                    None,
                ) {
                    CallOutcome::Committed => {}
                    CallOutcome::Interrupted => self.disable_instance(index),
                    CallOutcome::Failed(message) => self.fail_instance(index, message),
                }
            }
        }
    }

    /// Есть ли у экземпляра callback `on_ui_event`; ABI-модули до v16 такого не имеют.
    fn has_ui_callback(&self, index: usize) -> bool {
        match self.instances[index]
            .as_ref()
            .map(|instance| &instance.kind)
        {
            Some(ScriptKind::Lite(instance)) => {
                instance.has_callback("on_ui_event").unwrap_or(true)
            }
            Some(ScriptKind::Heavy(HeavyInstance::Abi(script))) => script.has_ui_event(),
            None => false,
        }
    }

    /// Открывает канвасы компонентов `ui_canvas` сцены; вызывается до `on_start`.
    fn open_scene_canvases(&mut self, world: &World) {
        let handles: Vec<ObjectHandle> = world
            .objects()
            .iter()
            .filter(|object| object.ui_canvas.is_some())
            .map(|object| object.handle)
            .collect();
        self.open_component_canvases(world, &handles);
    }

    /// Открывает канвас каждого объекта с `ui_canvas`, видимость и слой берутся из компонента.
    /// Незарегистрированный документ — Error, объект остаётся без канваса.
    fn open_component_canvases(&mut self, world: &World, handles: &[ObjectHandle]) {
        for &handle in handles {
            let Some(component) = world.ui_canvas(handle) else {
                continue;
            };
            let key = owner_key(handle);
            let mut state = self.ui.borrow_mut();
            if state
                .component_canvases
                .get(&key)
                .is_some_and(|canvas| state.system.layer(*canvas).is_ok())
            {
                continue;
            }
            let path = component.document.as_str();
            let Some(document) = state.documents.get(path).cloned() else {
                drop(state);
                let object = world.object(handle).map(|object| object.id.clone());
                self.events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: self.scene_name.clone(),
                    message: format!(
                        "UI-документ {path:?} объекта {:?} не зарегистрирован",
                        object.unwrap_or_default()
                    ),
                });
                continue;
            };
            let canvas = state.system.open(document, key);
            if !component.visible {
                // Свежий канвас всегда жив, поэтому ошибок у установки видимости и слоя нет.
                let _ = state.system.set_canvas_visible(canvas, false);
            }
            if let Some(layer) = component.layer {
                let _ = state.system.set_layer(canvas, layer);
            }
            state.component_canvases.insert(key, canvas);
        }
    }

    /// Закрывает все канвасы удалённых объектов, включая открытые их скриптами. События закрытия
    /// не доставляются: владелец уже удалён.
    fn close_owned_canvases(&mut self, removed: &[ObjectHandle]) {
        let mut state = self.ui.borrow_mut();
        for &handle in removed {
            let key = owner_key(handle);
            state.component_canvases.remove(&key);
            let _ = state.system.close_owned(key);
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

    /// `on_trigger_stay` для каждой активной пары триггера на кадре обновления. Вызов идёт по
    /// снимку текущего шага, поэтому кадр входа тоже получает stay, а кадр выхода — нет. В отличие
    /// от Unity, это по кадру, а не по подшагу физики.
    fn deliver_trigger_stays(&mut self, world: &mut World, time: ScriptTime) {
        let Ok(snapshot) = self.physics.borrow().shared_snapshot() else {
            return;
        };
        for &(first, second) in snapshot.trigger_pairs() {
            let (Some(first_object), Some(second_object)) = (
                self.live_body_object(world, first),
                self.live_body_object(world, second),
            ) else {
                continue;
            };
            for (object, other) in [(first_object, second_object), (second_object, first_object)] {
                self.deliver_physics_event(
                    world,
                    time,
                    object,
                    ScriptPhysicsEvent {
                        kind: ScriptPhysicsEventKind::TriggerStay,
                        other,
                        point: glam::Vec3::ZERO,
                        normal: glam::Vec3::ZERO,
                    },
                );
            }
        }
    }

    /// Объект тела для `on_trigger_stay`: только живой и активный в иерархии. Удалённые и неактивные
    /// партнёры остаются в снимке до следующего шага, но стой им не доставляется.
    fn live_body_object(
        &self,
        world: &World,
        body: engine_physics::BodyHandle,
    ) -> Option<ObjectHandle> {
        let object = self.physics.borrow().object(body)?;
        (world.is_alive(object) && world.active_in_hierarchy(object) == Some(true))
            .then_some(object)
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
                    .filter(|instance| {
                        instance.enabled
                            && instance.running
                            && instance.handle == object
                            && accepts_physics_event(&instance.kind, event.kind)
                    })
                    .map(|_| index)
            })
            .collect::<Vec<_>>();
        for index in indexes {
            match self.call_instance(
                index,
                world,
                Phase::PhysicsEvent(event),
                time,
                None,
                None,
                None,
                None,
                None,
                None,
            ) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call_instance(
        &mut self,
        index: usize,
        world: &mut World,
        phase: Phase,
        time: ScriptTime,
        message: Option<&PendingMessage>,
        net_event: Option<&NetEvent>,
        ui_event: Option<&UiDelivery>,
        time_error: Option<&str>,
        timer: Option<&DueTimer>,
        coroutine: Option<&ScriptCoroutine>,
    ) -> CallOutcome {
        let mut callback_time = self.frame_time;
        match phase {
            Phase::FixedUpdate(dt) | Phase::Update(dt) | Phase::LateUpdate(dt) => {
                callback_time.dt = dt;
                if matches!(phase, Phase::FixedUpdate(_)) {
                    callback_time.unscaled_dt = if callback_time.scale > 0.0 {
                        dt / callback_time.scale
                    } else {
                        0.0
                    };
                }
            }
            _ => {}
        }
        if let Err(error) = lite::set_time(&self.lua_time, callback_time) {
            return CallOutcome::Failed(error.to_string());
        }
        let Self {
            instances,
            script_refs,
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
            animation_requests,
            particle_requests,
            rumble_requests,
            lua,
            interrupt,
            commands,
            commands_generation,
            physics,
            lifecycle,
            cursor,
            net,
            net_requests,
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
        transaction.set_frame_time(callback_time);
        transaction.begin(ScriptTransactionInit {
            world,
            self_object: Some(instance.handle),
            source: &instance.source,
            writable: true,
            actions: actions.frame(),
            queued_messages: (queued_messages.len(), *queued_bytes),
            queued_audio: audio_requests.len(),
            queued_rumble: rumble_requests.len(),
            queued_animation: animation_requests.len(),
            physics: physics.clone(),
            net: net.clone(),
            queued_net: net_requests.len(),
            physics_phase_dt: match phase {
                Phase::FixedUpdate(dt) | Phase::Update(dt) => Some(dt),
                _ => None,
            },
            lifecycle: lifecycle.clone(),
            lifecycle_writes: !matches!(phase, Phase::Destroy | Phase::TeardownDisable),
            instance_serial: instance.serial,
            instance_enabled: instance.user_enabled,
            cursor: *cursor,
        });
        transaction.set_fixed_update_dt(match phase {
            Phase::FixedUpdate(dt) => Some(dt),
            _ => None,
        });
        let transaction_ptr = transaction as *mut ScriptTransaction;
        let mut lua_properties_after = None;
        let callback_result: Result<Option<ScriptTransform>, String> = match &mut instance.kind {
            ScriptKind::Lite(lite_instance) => {
                *active_source.borrow_mut() = instance.source.clone();
                let _active_call = if let Some(coroutine) = coroutine {
                    ActiveCallGuard::new_thread(
                        lua,
                        Some(call_deadline),
                        &coroutine.thread,
                        active_call,
                        transaction_ptr,
                        active_owner,
                        Some(index),
                    )
                } else {
                    Ok(ActiveCallGuard::new(
                        lua,
                        Some(call_deadline),
                        active_call,
                        transaction_ptr,
                        active_owner,
                        Some(index),
                    ))
                };
                let _active_call = match _active_call {
                    Ok(guard) => guard,
                    Err(error) => {
                        transaction.discard();
                        return CallOutcome::Failed(error.to_string());
                    }
                };
                // Только свойства, записанные другим скриптом: собственные записи `self.params`
                // Lua-таблица хранит сама и не должна откатываться к значению зеркала.
                let pending_params = {
                    let mut refs = script_refs.borrow_mut();
                    refs.iter_mut()
                        .find(|entry| {
                            entry.serial == instance.serial && !entry.dirty_properties.is_empty()
                        })
                        .map(|entry| {
                            let dirty = std::mem::take(&mut entry.dirty_properties);
                            dirty
                                .iter()
                                .filter_map(|name| {
                                    entry
                                        .properties
                                        .get(name)
                                        .map(|value| (name.clone(), value.clone()))
                                })
                                .collect::<BTreeMap<_, _>>()
                        })
                };
                if let Some(properties) = pending_params
                    && let Err(error) = lite_instance.write_params(lua, &properties)
                {
                    transaction.discard();
                    return CallOutcome::Failed(error.to_string());
                }
                let has_callback = match phase {
                    Phase::Message => lite_instance.has_message_callback(),
                    Phase::Enable => lite_instance.has_callback("on_enable"),
                    Phase::Disable | Phase::TeardownDisable => {
                        lite_instance.has_callback("on_disable")
                    }
                    _ => Ok(true),
                };
                let result = has_callback
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
                                        Phase::Enable => lite_instance.invoke_enable(),
                                        Phase::Disable | Phase::TeardownDisable => {
                                            lite_instance.invoke_disable()
                                        }
                                        Phase::FixedUpdate(dt) => {
                                            lite_instance.invoke_fixed_update(dt)
                                        }
                                        Phase::Update(dt) => lite_instance.invoke_update(dt),
                                        Phase::LateUpdate(dt) => {
                                            lite_instance.invoke_late_update(dt)
                                        }
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
                                        Phase::Net => match net_event {
                                            Some(event) => {
                                                lite_instance.invoke_net_event(lua, event)
                                            }
                                            None => {
                                                Err(LuaError::runtime("missing net event data"))
                                            }
                                        },
                                        Phase::Ui => match ui_event {
                                            Some(event) => lite_instance.invoke_ui_event(
                                                lua,
                                                active_call,
                                                event,
                                            ),
                                            None => Err(LuaError::runtime("missing ui event data")),
                                        },
                                        Phase::Destroy => lite_instance.invoke_destroy(),
                                        Phase::Timer => match timer {
                                            Some(timer) => match &timer.callback {
                                                TimerCallback::Lua(callback) => {
                                                    lite_instance.invoke_timer(callback, timer.id)
                                                }
                                                TimerCallback::Abi(_) => Err(LuaError::runtime(
                                                    "ABI timer dispatched to Lua script",
                                                )),
                                            },
                                            None => Err(LuaError::runtime("missing timer data")),
                                        },
                                        Phase::Coroutine => {
                                            let Some(coroutine) = coroutine else {
                                                return Err(LuaError::runtime("missing coroutine"));
                                            };
                                            match lite_instance
                                                .resume_coroutine(&coroutine.thread)?
                                            {
                                                lite::CoroutineResume::Finished => unsafe {
                                                    (*transaction_ptr)
                                                        .remove_coroutine(coroutine.id);
                                                },
                                                lite::CoroutineResume::Yielded(yielded) => {
                                                    let wait = match yielded {
                                                        lite::CoroutineYield::NextFrame => {
                                                            CoroutineWait::Frame(
                                                                callback_time
                                                                    .frame
                                                                    .saturating_add(1),
                                                            )
                                                        }
                                                        lite::CoroutineYield::Frames(frames) => {
                                                            CoroutineWait::Frame(
                                                                callback_time
                                                                    .frame
                                                                    .saturating_add(frames.max(1)),
                                                            )
                                                        }
                                                        lite::CoroutineYield::Seconds(seconds)
                                                            if seconds > 0.0 =>
                                                        {
                                                            let due =
                                                                callback_time.elapsed + seconds;
                                                            CoroutineWait::Time(
                                                                if due.is_finite() {
                                                                    due
                                                                } else {
                                                                    f64::MAX
                                                                },
                                                            )
                                                        }
                                                        lite::CoroutineYield::Seconds(_) => {
                                                            CoroutineWait::Frame(
                                                                callback_time
                                                                    .frame
                                                                    .saturating_add(1),
                                                            )
                                                        }
                                                    };
                                                    unsafe {
                                                        (*transaction_ptr).reschedule_coroutine(
                                                            coroutine.id,
                                                            coroutine.thread.clone(),
                                                            wait,
                                                        )
                                                    };
                                                }
                                            }
                                            Ok(())
                                        }
                                    })
                            }
                        };
                        callback.and_then(|()| lite_instance.transform()).map(Some)
                    })
                    .and_then(|transform| {
                        let Some(transform) = transform else {
                            return Ok(None);
                        };
                        let mut properties = lite_instance.declared_properties(lua)?;
                        for (name, value) in &mut properties {
                            // SAFETY: the transaction remains owned by this callback.
                            *value = unsafe { (*transaction_ptr).script_set_current(name, value) }
                                .map_err(|error| LuaError::runtime(error.message()))?;
                        }
                        lite_instance.stage_params(lua, &properties)?;
                        lua_properties_after = Some(properties);
                        Ok(Some(transform))
                    });
                if result.is_err() {
                    let _ = lite_instance.restore_params(lua);
                }
                result.map_err(|error| error.to_string())
            }
            ScriptKind::Heavy(HeavyInstance::Abi(script)) => {
                let Some(self_object) = abi_object_handle(instance.handle) else {
                    // SAFETY: the transaction has been started and remains owned by this call.
                    unsafe { (*transaction_ptr).discard() };
                    return CallOutcome::Failed(
                        "object handle cannot be represented by the script ABI".to_owned(),
                    );
                };
                // Записи свойств другого скрипта доходят модулю до его следующего callback.
                let writes = {
                    let mut refs = script_refs.borrow_mut();
                    refs.iter_mut()
                        .find(|entry| {
                            entry.serial == instance.serial && !entry.dirty_properties.is_empty()
                        })
                        .map(|entry| {
                            let dirty = std::mem::take(&mut entry.dirty_properties);
                            dirty
                                .iter()
                                .filter_map(|name| {
                                    entry
                                        .properties
                                        .get(name)
                                        .map(|value| (name.clone(), value.clone()))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                };
                for (write_index, (name, value)) in writes.iter().enumerate() {
                    let call = AbiScriptCall {
                        self_object,
                        // SAFETY: the transaction is owned by this call until it commits or discards.
                        transaction: unsafe { &mut *transaction_ptr },
                        events,
                        source: &instance.source,
                        commands,
                        commands_generation,
                        command_owner: index,
                        commands_allowed: true,
                    };
                    if let Err(message) = script.invoke_set_property(time, name, value, call) {
                        for (pending_name, _) in writes.iter().skip(write_index) {
                            settle_script_ref_write(
                                script_refs,
                                instance.serial,
                                pending_name,
                                false,
                            );
                        }
                        // SAFETY: the synchronous delivery returned, so no call-scoped borrow remains.
                        unsafe { (*transaction_ptr).discard() };
                        return CallOutcome::Failed(message);
                    }
                    settle_script_ref_write(script_refs, instance.serial, name, true);
                }
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
                    Phase::Enable => script.invoke(AbiPhase::Enable, time, call),
                    Phase::Disable | Phase::TeardownDisable => {
                        script.invoke(AbiPhase::Disable, time, call)
                    }
                    Phase::Update(dt) => script.invoke(AbiPhase::Update(dt), time, call),
                    Phase::FixedUpdate(dt) => script.invoke(AbiPhase::FixedUpdate(dt), time, call),
                    Phase::LateUpdate(dt) => script.invoke(AbiPhase::LateUpdate(dt), time, call),
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
                    Phase::Net => match net_event {
                        Some(event) => script.invoke_net_event(time, event, call),
                        None => Err("missing net event data".to_owned()),
                    },
                    Phase::Ui => match ui_event {
                        Some(event) => script.invoke_ui_event(time, event, call),
                        None => Err("missing ui event data".to_owned()),
                    },
                    Phase::Destroy => script.invoke(AbiPhase::Destroy, time, call),
                    Phase::Timer => match timer.map(|timer| &timer.callback) {
                        Some(TimerCallback::Abi(tag)) => script.invoke_timer(time, tag, call),
                        _ => Err("missing ABI timer callback".to_owned()),
                    },
                    Phase::Coroutine => {
                        Err("Rust ABI scripts do not have Lua coroutines".to_owned())
                    }
                }
                .map(|()| None)
            }
        };

        if matches!(&instance.kind, ScriptKind::Lite(_)) && interrupt.is_interrupted() {
            if let ScriptKind::Lite(lite_instance) = &mut instance.kind {
                let _ = lite_instance.restore_params(lua);
            }
            // SAFETY: the callback and its call-scoped transaction borrow have returned.
            unsafe { (*transaction_ptr).discard() };
            return CallOutcome::Interrupted;
        }
        if matches!(&instance.kind, ScriptKind::Lite(_)) && call_deadline.is_expired() {
            if let ScriptKind::Lite(lite_instance) = &mut instance.kind {
                let _ = lite_instance.restore_params(lua);
            }
            // SAFETY: the callback and its call-scoped transaction borrow have returned.
            unsafe { (*transaction_ptr).discard() };
            return CallOutcome::Failed(call_deadline.timeout_message());
        }

        match callback_result {
            Ok(Some(transform)) if transform.is_finite() => {
                // SAFETY: the callback and its call-scoped transaction borrow have returned.
                let staged = unsafe {
                    transform == initial
                        || (*transaction_ptr).set_local_transform(instance.handle, transform)
                };
                if !staged {
                    if let ScriptKind::Lite(lite_instance) = &mut instance.kind {
                        let _ = lite_instance.restore_params(lua);
                    }
                    // SAFETY: the transaction is still owned by this host call.
                    unsafe { (*transaction_ptr).discard() };
                    return CallOutcome::Failed(
                        "could not stage the script object's local transform".to_owned(),
                    );
                }
            }
            Ok(Some(_)) => {
                if let ScriptKind::Lite(lite_instance) = &mut instance.kind {
                    let _ = lite_instance.restore_params(lua);
                }
                // SAFETY: the callback and its call-scoped transaction borrow have returned.
                unsafe { (*transaction_ptr).discard() };
                return CallOutcome::Failed("script produced a non-finite transform".to_owned());
            }
            Ok(None) => {}
            Err(message) => {
                if let ScriptKind::Lite(lite_instance) = &mut instance.kind {
                    let _ = lite_instance.restore_params(lua);
                }
                // SAFETY: the callback and its call-scoped transaction borrow have returned.
                unsafe { (*transaction_ptr).discard() };
                return CallOutcome::Failed(message);
            }
        }

        // SAFETY: the callback and all call-scoped transaction borrows have returned.
        match unsafe {
            (*transaction_ptr).commit(
                world,
                queued_messages,
                queued_bytes,
                events,
                audio_requests,
                animation_requests,
                net_requests,
                rumble_requests,
                particle_requests,
            )
        } {
            Ok(mutations) => {
                if let Some(properties) = lua_properties_after
                    && let ScriptKind::Lite(lite_instance) = &mut instance.kind
                {
                    lite_instance.commit_properties(properties);
                }
                // SAFETY: commit returned, so no call-scoped borrow of the transaction remains.
                *cursor = unsafe { (*transaction_ptr).cursor() };
                actions.queue_mutations(mutations);
                CallOutcome::Committed
            }
            Err(message) => {
                if let ScriptKind::Lite(lite_instance) = &mut instance.kind {
                    let _ = lite_instance.restore_params(lua);
                }
                CallOutcome::Failed(message)
            }
        }
    }

    /// Применяет накопленные заявки в конце фазы: spawn в порядке commit, затем удаления,
    /// вытеснение по `max_instances` и созревшие отложенные удаления.
    fn apply_lifecycle(&mut self, world: &mut World, time: ScriptTime) {
        let has_work = {
            let lifecycle = self.lifecycle.borrow();
            !lifecycle.enabled_writes.is_empty()
                || !lifecycle.queued.is_empty()
                || !lifecycle.destroys.is_empty()
                || lifecycle.scheduled.values().any(|due| *due <= time.elapsed)
        };
        if !has_work {
            return;
        }
        // Флаги `set_enabled` меняются здесь; переход в работу выполнит следующая `sync_activity`.
        let enabled_writes = std::mem::take(&mut self.lifecycle.borrow_mut().enabled_writes);
        for (serial, enabled) in enabled_writes {
            if let Some(instance) = self
                .instances
                .iter_mut()
                .flatten()
                .find(|instance| instance.serial == serial)
            {
                instance.user_enabled = enabled;
            }
            if let Some(entry) = self
                .script_refs
                .borrow_mut()
                .iter_mut()
                .find(|entry| entry.serial == serial)
            {
                entry.enabled = enabled;
            }
        }
        self.apply_queued_spawns(world);

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
        // Скриптовый spawn клиента проверен при вызове, но шаблон тяжёлого префаба мог ещё не быть
        // загружен: проверка повторяется здесь. Мост создаёт сетевые объекты через `spawn_external`.
        if request.from_script
            && self.net.borrow().mode() == NetMode::Client
            && networked_template(template)
        {
            return Err(format!(
                "prefab {:?} has network objects; a client can spawn them only through the network",
                prefab_display(&request.prefab)
            ));
        }
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
        self.open_component_canvases(world, &handles);
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
            Ok(()) => Ok(()),
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
                let object_refs = resolve_object_refs(schema.as_ref(), &params, world)
                    .map_err(|message| format!("object {id:?}: {message}"))?;
                let instance = LuaInstance::new(
                    &self.lua,
                    class,
                    LuaInstanceInit {
                        object_id: &id,
                        object_name: &name,
                        params: &params,
                        property_names: schema
                            .as_ref()
                            .map(|schema| {
                                schema
                                    .specs()
                                    .iter()
                                    .map(|spec| spec.name.clone())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        transform,
                        object_refs: &object_refs,
                        object_handle: handle,
                    },
                    self.active_call.clone(),
                )
                .map_err(|error| error.to_string())?;
                let serial = self.next_serial();
                self.register_script_ref(
                    serial,
                    handle,
                    path,
                    schema.as_ref(),
                    &params,
                    ScriptRefCapabilities {
                        property_write_supported: true,
                        enabled: script.enabled,
                    },
                );
                let index = self.insert_instance(ScriptInstance {
                    handle,
                    source: event_source(&self.scene_name, &id, path),
                    kind: ScriptKind::Lite(instance),
                    enabled: true,
                    user_enabled: script.enabled,
                    running: false,
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
                let object_refs = resolve_object_refs(schema.as_ref(), &params, world)
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
                    queued_rumble: self.rumble_requests.len(),
                    queued_animation: self.animation_requests.len(),
                    physics: self.physics.clone(),
                    net: self.net.clone(),
                    queued_net: self.net_requests.len(),
                    physics_phase_dt: None,
                    lifecycle: self.lifecycle.clone(),
                    lifecycle_writes: false,
                    instance_serial: serial,
                    instance_enabled: true,
                    cursor: self.cursor,
                });
                let result = AbiScriptInstance::create(
                    descriptor,
                    self.heavy_abi_version,
                    &params,
                    &object_refs,
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
                self.register_script_ref(
                    serial,
                    handle,
                    &script.name,
                    schema.as_ref(),
                    &params,
                    ScriptRefCapabilities {
                        property_write_supported: descriptor.set_property.is_some(),
                        enabled: script.enabled,
                    },
                );
                self.instances[owner] = Some(ScriptInstance {
                    handle,
                    source,
                    kind: ScriptKind::Heavy(HeavyInstance::Abi(instance)),
                    enabled: true,
                    user_enabled: script.enabled,
                    running: false,
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
        self.retire_bodies(world, &doomed);
        self.call_destroy_instances(world, &doomed, time);
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
        self.forget_despawned(&removed);
        Some(removed)
    }

    /// Физические тела удаляемых объектов уходят в очередь retire до удаления из мира.
    fn retire_bodies(&mut self, world: &World, doomed: &HashSet<ObjectHandle>) {
        let retired = doomed
            .iter()
            .filter_map(|&handle| {
                let binding = self.physics.borrow().optional_binding(handle)?;
                let object = world.object(handle)?;
                Some((handle, binding.body, object.id.clone(), object.name.clone()))
            })
            .collect::<Vec<_>>();
        let mut lifecycle = self.lifecycle.borrow_mut();
        for (handle, body, id, name) in retired {
            lifecycle.retire(handle, body, id, name);
        }
    }

    /// `on_destroy` стартовавших включённых экземпляров удаляемых объектов; ошибка отключает экземпляр.
    fn call_destroy_instances(
        &mut self,
        world: &mut World,
        doomed: &HashSet<ObjectHandle>,
        time: ScriptTime,
    ) {
        let time_error = lite::set_time(&self.lua_time, self.frame_time)
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
            match self.call_destroy(index, world, time, time_error.as_deref()) {
                CallOutcome::Committed => {}
                CallOutcome::Interrupted => self.disable_instance(index),
                CallOutcome::Failed(message) => self.fail_instance(index, message),
            }
        }
    }

    /// Учёт объектов, уже удалённых из мира: таймеры удаления, корни префабов, экземпляры, сеть,
    /// канвасы UI.
    fn forget_despawned(&mut self, removed: &[ObjectHandle]) {
        let removed_set: HashSet<ObjectHandle> = removed.iter().copied().collect();
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            let mut prefabs = Vec::new();
            for handle in removed {
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
        for handle in removed {
            self.net.borrow_mut().clear_object(*handle);
        }
        self.close_owned_canvases(removed);
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

    /// Регистрирует экземпляр в справочнике `obj:script`; значения свойств берутся из `params`.
    fn register_script_ref(
        &mut self,
        serial: u64,
        object: ObjectHandle,
        key: &str,
        schema: Option<&engine_scene::ParamSchema>,
        properties: &BTreeMap<String, ParamValue>,
        capabilities: ScriptRefCapabilities,
    ) {
        self.script_refs.borrow_mut().push(ScriptRefEntry {
            serial,
            object,
            key: key.to_owned(),
            schema: schema.cloned().map(Rc::new),
            properties: properties.clone(),
            property_write_supported: capabilities.property_write_supported,
            enabled: capabilities.enabled,
            dirty_properties: BTreeSet::new(),
            dirty_original_properties: BTreeMap::new(),
        });
    }

    /// Снимает экземпляр: сначала его команды консоли, затем освобождает слот.
    fn remove_instance(&mut self, index: usize) {
        self.remove_commands_for_owner(index);
        if let Some(instance) = self.instances[index].take() {
            self.script_refs
                .borrow_mut()
                .retain(|entry| entry.serial != instance.serial);
            self.timeline.borrow_mut().cancel_owner(instance.serial);
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
            queued_rumble: self.rumble_requests.len(),
            queued_animation: self.animation_requests.len(),
            physics: self.physics.clone(),
            net: self.net.clone(),
            queued_net: self.net_requests.len(),
            physics_phase_dt: None,
            lifecycle: self.lifecycle.clone(),
            lifecycle_writes: false,
            instance_serial: 0,
            instance_enabled: true,
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
            self.timeline.borrow_mut().cancel_owner(instance.serial);
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

fn settle_script_ref_write(refs: &ScriptRefs, serial: u64, name: &str, accepted: bool) {
    let mut refs = refs.borrow_mut();
    let Some(entry) = refs.iter_mut().find(|entry| entry.serial == serial) else {
        return;
    };
    entry.dirty_properties.remove(name);
    if accepted {
        entry.dirty_original_properties.remove(name);
    } else if let Some(previous) = entry.dirty_original_properties.remove(name) {
        entry.properties.insert(name.to_owned(), previous);
    }
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

    fn new_thread(
        lua: &'lua Lua,
        call_deadline: Option<&lite::CallDeadline>,
        thread: &mlua::Thread,
        active_call: &ActiveCall,
        transaction: *mut ScriptTransaction,
        active_owner: &ActiveOwner,
        owner: Option<usize>,
    ) -> mlua::Result<Self> {
        let deadline = call_deadline
            .map(|deadline| deadline.enter_thread(lua, thread))
            .transpose()?;
        active_call.set(transaction);
        active_owner.set(owner);
        Ok(Self {
            active_call: active_call.clone(),
            active_owner: active_owner.clone(),
            _deadline: deadline,
        })
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

/// Быстрые префабы сцены после проверки [`ScriptHost::check_scene_prefabs`], ещё не применённые.
pub struct ScenePrefabs {
    slots: Vec<(String, PrefabSpec)>,
    templates: Vec<(String, Arc<PrefabTemplate>)>,
}

fn resolve_object_refs(
    schema: Option<&ParamSchema>,
    params: &ScriptParams,
    world: &World,
) -> Result<HashMap<String, ScriptObjectRef>, String> {
    fn visit(
        value: &ParamValue,
        kind: Option<&ParamKind>,
        world: &World,
        references: &mut HashMap<String, ScriptObjectRef>,
    ) -> Result<(), String> {
        match value {
            ParamValue::Object(id) if !id.is_empty() => {
                let handle = world
                    .handle(id)
                    .ok_or_else(|| format!("object reference {id:?} does not exist"))?;
                let object = world
                    .object(handle)
                    .ok_or_else(|| format!("object reference {id:?} does not exist"))?;
                if let Some(ParamKind::Component(component)) = kind
                    && !has_component(object, component)
                {
                    return Err(format!(
                        "object reference {id:?} does not have component {:?}",
                        component.name()
                    ));
                }
                references.insert(
                    id.clone(),
                    ScriptObjectRef {
                        handle,
                        name: object.name.clone(),
                    },
                );
            }
            ParamValue::Array(values) => {
                let element = match kind {
                    Some(ParamKind::Array(element)) => Some(element.as_ref()),
                    _ => None,
                };
                for value in values {
                    visit(value, element, world, references)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut references = HashMap::new();
    if let Some(schema) = schema {
        for spec in schema.specs() {
            if let Some(value) = params.get(&spec.name) {
                visit(value, Some(&spec.kind), world, &mut references)?;
            }
        }
    } else {
        for value in params.values() {
            visit(value, None, world, &mut references)?;
        }
    }
    Ok(references)
}

fn has_component(object: &engine_scene::WorldObject, component: &ComponentKind) -> bool {
    match component {
        ComponentKind::Transform => true,
        ComponentKind::Camera => object.camera.is_some(),
        ComponentKind::MeshRenderer => object.mesh_renderer.is_some(),
        ComponentKind::LodGroup => object.lod_group.is_some(),
        ComponentKind::DirectionalLight => object.directional_light.is_some(),
        ComponentKind::PointLight => object.point_light.is_some(),
        ComponentKind::SpotLight => object.spot_light.is_some(),
        ComponentKind::Environment => object.environment.is_some(),
        ComponentKind::Collider => object.collider.is_some(),
        ComponentKind::RigidBody => object.rigid_body.is_some(),
        ComponentKind::CharacterController => object.character_controller.is_some(),
        ComponentKind::AudioSource => object.audio_source.is_some(),
        ComponentKind::Animator => object.animator.is_some(),
        ComponentKind::ParticleEmitter => object.particle_emitter.is_some(),
        ComponentKind::NetworkIdentity => object.network_identity.is_some(),
        ComponentKind::NetworkTransform => object.network_transform.is_some(),
        ComponentKind::UiCanvas => object.ui_canvas.is_some(),
        ComponentKind::Script(name) => {
            object
                .lite_scripts
                .iter()
                .any(|script| script.path.as_str() == name)
                || object
                    .heavy_scripts
                    .iter()
                    .any(|script| script.name == *name)
        }
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

/// `RVE_TRIGGER_STAY` появился в ABI v18. Модуль старше отвергает неизвестный вид события ошибкой,
/// поэтому ему stay не доставляется.
fn accepts_physics_event(kind: &ScriptKind, event: ScriptPhysicsEventKind) -> bool {
    match kind {
        ScriptKind::Heavy(HeavyInstance::Abi(script)) => {
            !matches!(event, ScriptPhysicsEventKind::TriggerStay) || script.abi_version() >= 18
        }
        _ => true,
    }
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

/// Имя callback Lua-класса для сетевого события.
fn net_callback_name(event: &NetEvent) -> &'static str {
    match event {
        NetEvent::Message { .. } => "on_net_message",
        NetEvent::PeerConnected { .. } => "on_peer_connected",
        NetEvent::PeerDisconnected { .. } => "on_peer_disconnected",
    }
}

fn event_source(scene_name: &str, object_id: &str, script_name: &str) -> String {
    format!("{scene_name}/{object_id}/{script_name}")
}

#[cfg(test)]
mod animation_tests;
#[cfg(test)]
mod audio_tests;
#[cfg(test)]
mod component_tests;
#[cfg(test)]
#[path = "host/keep_on_load_tests.rs"]
mod keep_on_load_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod net_tests;
#[cfg(test)]
#[path = "host/save_tests.rs"]
mod save_tests;
#[cfg(test)]
#[path = "host/scene_replace_tests.rs"]
mod scene_replace_tests;
#[cfg(test)]
#[path = "host/script_ref_tests.rs"]
mod script_ref_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod ui_tests;
