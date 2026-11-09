use crate::actions::{ActionFrame, ActionMutation, ActionValue};
use crate::component_writes::{ComponentWrites, MAX_PARTICLE_COMMANDS_PER_CALL};
use crate::lifecycle::{
    DestroyRequest, Lifecycle, ObjectState, PrefabKind, PrefabRequest, PrefabStatus, SpawnArgs,
    SpawnRequest, check_spawn_request, prefab_display, prefab_key,
};
use crate::lite::EventQueue;
use crate::net::{MAX_NET_REQUESTS_PER_FRAME, NetObjectState, NetScriptState, networked_template};
use crate::physics::{BindingUpdate, PhysicsScriptState};
use crate::scene_state::{SharedSceneState, check_scene_request};
use crate::script_refs::{ScriptRefError, ScriptRefs, coerce_property, find_entry, find_serial};
use crate::timeline::{CoroutineWait, ScriptCoroutine, ScriptTimeline, TimelineOp, TimerCallback};
use crate::types::{
    RumbleRequest, ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptInput, ScriptTransform,
};
use crate::ui::{UiJournal, UiScriptState};
use engine_physics::{
    BodyDesc, BodyHandle, BodyKind, CharacterHit, CharacterMoveResult, ColliderDesc,
    PhysicsCommand, Pose, PrimitiveShape, QueryFilter, QueryHit, QueryOverrides, Ray, Shape,
    character_controller_scaled, object_world_transform, primitive_collider_shape,
};
use engine_scene::world::{MeshMaterialParams, ParticleCommand};
use engine_scene::{
    AnimationRequest, AudioRequest, AudioSourceComponent, CameraSettings,
    CharacterControllerComponent, ColliderComponent, ColliderShape, ComponentKind,
    DirectionalLightComponent, LAYER_COUNT, LocalTransform, MAX_MESSAGE_DEPTH,
    MAX_MESSAGE_ELEMENTS, MAX_MESSAGE_STRING_BYTES, MAX_SCREEN_SHADER_PARAMS, NetMode, NetRequest,
    NetTarget, ObjectEdit, ObjectHandle, ParamValue, PeerId, PointLightComponent,
    PostProcessSettings, ProceduralSkyComponent, RigidBodyComponent, RigidBodyKind, SERVER_PEER_ID,
    SpotLightComponent, World,
};
use glam::{EulerRot, Quat, Vec3};
use mlua::{Function, Thread};

/// Состояние одного слота экранного шейдера камеры в очереди транзакции.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScreenShaderState {
    pub(crate) enabled: bool,
    pub(crate) params: [f32; MAX_SCREEN_SHADER_PARAMS],
}
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

pub(crate) const MAX_MESSAGES_PER_FRAME: usize = 4096;
/// Звуковых запросов за кадр, включая уже зафиксированные скриптами; сверх лимита скрипт получает ошибку.
pub(crate) const MAX_AUDIO_REQUESTS_PER_FRAME: usize = 256;
/// Сколько последних голосов помнит транзакция для проверки `set_voice_position`; старые вытесняются.
const MAX_TRACKED_VOICES: usize = 4096;
/// Запросов аниматора за кадр, включая уже зафиксированные скриптами.
pub(crate) const MAX_ANIMATION_REQUESTS_PER_FRAME: usize = 256;
/// Запросов вибрации за кадр, включая уже зафиксированные скриптами; сверх лимита скрипт получает ошибку.
pub(crate) const MAX_RUMBLE_REQUESTS_PER_FRAME: usize = 256;
pub(crate) const MAX_MESSAGE_BYTES_PER_FRAME: usize = 8 * 1024 * 1024;
pub(crate) const MAX_MESSAGE_NAME_BYTES: usize = 256;
pub(crate) const MAX_MESSAGE_TARGET_ID_BYTES: usize = 256;
const MAX_SAVE_KEY_BYTES: usize = 128;
const MAX_SAVE_KEYS: usize = 4_096;
const MAX_SAVE_BYTES: usize = 1_048_576;

#[derive(Clone)]
pub(crate) struct ScriptObjectRef {
    pub(crate) handle: ObjectHandle,
    pub(crate) name: String,
}

pub(crate) struct MessageValueBudget {
    elements: usize,
    string_bytes: usize,
    estimated_size: usize,
}

impl MessageValueBudget {
    pub(crate) fn new() -> Self {
        Self {
            elements: 0,
            string_bytes: 0,
            estimated_size: 0,
        }
    }

    pub(crate) fn enter(&mut self, depth: usize) -> Result<(), &'static str> {
        if depth > MAX_MESSAGE_DEPTH {
            return Err("message value nesting exceeds 16");
        }
        self.elements = self.elements.saturating_add(1);
        if self.elements > MAX_MESSAGE_ELEMENTS {
            return Err("message value contains more than 1024 elements");
        }
        self.estimated_size = self
            .estimated_size
            .saturating_add(std::mem::size_of::<ParamValue>());
        Ok(())
    }

    pub(crate) fn add_string(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.string_bytes = self.string_bytes.saturating_add(bytes);
        if self.string_bytes > MAX_MESSAGE_STRING_BYTES {
            return Err("message value strings exceed 65536 bytes");
        }
        self.estimated_size = self.estimated_size.saturating_add(bytes);
        Ok(())
    }

    pub(crate) fn remaining_elements(&self) -> usize {
        MAX_MESSAGE_ELEMENTS.saturating_sub(self.elements)
    }
    pub(crate) fn estimated_size(&self) -> usize {
        self.estimated_size
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MessageTarget {
    Object(ObjectHandle),
    /// Один экземпляр скрипта (`obj:script(key):send`); остальные экземпляры объекта не получают.
    Instance {
        object: ObjectHandle,
        serial: u64,
    },
    Broadcast,
}

/// Заявка записи объявленного свойства экземпляра, применяется после успешного коммита.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PropertyWrite {
    pub(crate) serial: u64,
    pub(crate) name: String,
    pub(crate) value: ParamValue,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingMessage {
    pub(crate) sender_id: String,
    pub(crate) target: MessageTarget,
    pub(crate) name: String,
    pub(crate) value: ParamValue,
    estimated_size: usize,
}

impl PendingMessage {
    /// Системное сообщение движка от объекта `sender_id`, например `animation_finished`.
    pub(crate) fn engine_message(
        sender_id: String,
        target: MessageTarget,
        name: &str,
        value: ParamValue,
    ) -> Self {
        let value_size = match &value {
            ParamValue::String(text) => text.len(),
            _ => 0,
        };
        let estimated_size = sender_id
            .len()
            .saturating_add(name.len())
            .saturating_add(value_size);
        Self {
            sender_id,
            target,
            name: name.to_owned(),
            value,
            estimated_size,
        }
    }

    /// Оценка размера сообщения, которую учитывает очередь кадра.
    pub(crate) fn size(&self) -> usize {
        self.estimated_size
    }
}

pub(crate) struct ScriptTransactionInit<'a> {
    pub(crate) world: &'a World,
    /// Объект, чей скрипт выполняется; `None` для top-level chunk, у которого объекта нет.
    pub(crate) self_object: Option<ObjectHandle>,
    pub(crate) source: &'a str,
    pub(crate) writable: bool,
    pub(crate) actions: Arc<ActionFrame>,
    pub(crate) queued_messages: (usize, usize),
    /// Звуковые запросы, уже зафиксированные в этом кадре.
    pub(crate) queued_audio: usize,
    /// Запросы вибрации, уже зафиксированные в этом кадре.
    pub(crate) queued_rumble: usize,
    pub(crate) queued_animation: usize,
    pub(crate) physics: Rc<RefCell<PhysicsScriptState>>,
    pub(crate) physics_phase_dt: Option<f32>,
    pub(crate) lifecycle: Rc<RefCell<Lifecycle>>,
    /// `false` в `create`, в `destroy` и на удалении поддерева: spawn и destroy запрещены.
    pub(crate) lifecycle_writes: bool,
    /// Серийный номер экземпляра, который выполняет вызов; нужен для предупреждений.
    pub(crate) instance_serial: u64,
    /// Пользовательский флаг `set_enabled` экземпляра до этого вызова.
    pub(crate) instance_enabled: bool,
    /// Состояние курсора, применённое хостом до этого вызова.
    pub(crate) cursor: ScriptCursor,
    /// Режим сети, участники и сетевые атрибуты объектов.
    pub(crate) net: Rc<RefCell<NetScriptState>>,
    /// Сетевые запросы, уже зафиксированные в этом кадре.
    pub(crate) queued_net: usize,
}

/// Результат разбора хендла объекта в вызове ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AbiObject {
    /// Значение не декодируется как хендл: это ошибка вызова.
    Malformed,
    /// Объект мёртв или ещё pending: вызов ничего не делает и не считается ошибкой.
    Unavailable,
    Alive(ObjectHandle),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum QueueMessageError {
    ReadOnly,
    UnknownTarget,
    InvalidName,
    InvalidValue,
}

/// Scratch state reused for each Lua or ABI lifecycle call.
///
/// `world` is read during a callback and only borrowed mutably again after it returns. Lua keeps
/// a call-scoped pointer to this object; the host sets it before entering Lua and clears it before
/// this transaction is committed or discarded.
pub(crate) struct ScriptTransaction {
    world: *const World,
    pub(crate) self_object: Option<ObjectHandle>,
    writable: bool,
    source: String,
    pending_transforms: HashMap<ObjectHandle, LocalTransform>,
    pending_camera_settings: HashMap<ObjectHandle, CameraSettings>,
    pending_post_process: HashMap<ObjectHandle, PostProcessSettings>,
    /// Состояние экранных шейдеров камер по (объект, слот); применяется на commit.
    pending_screen_shaders: HashMap<(ObjectHandle, usize), ScreenShaderState>,
    pending_active_camera: Option<ObjectHandle>,
    pending_messages: Vec<PendingMessage>,
    pending_warnings: Vec<String>,
    pending_bytes: usize,
    queued_message_count: usize,
    queued_message_bytes: usize,
    queued_audio: usize,
    queued_rumble: usize,
    queued_animation: usize,
    overflowed_messages: usize,
    action_frame: Option<Arc<ActionFrame>>,
    physics: Rc<RefCell<PhysicsScriptState>>,
    physics_phase_dt: Option<f32>,
    physics_fixed_dt: Option<f32>,
    pending_physics_commands: Vec<PhysicsCommand>,
    pending_grounded: HashMap<ObjectHandle, bool>,
    pending_collider_enabled: HashMap<BodyHandle, bool>,
    pending_pushes: HashMap<BodyHandle, PendingPush>,
    /// Заявки компонентов физики; проверяются миром при коммите, затем применяются к нему.
    pending_colliders: HashMap<ObjectHandle, ColliderComponent>,
    pending_rigid_bodies: HashMap<ObjectHandle, RigidBodyComponent>,
    pending_controllers: HashMap<ObjectHandle, CharacterControllerComponent>,
    /// Изменения привязок физики (тип тела, контроллер), которые применяются после коммита.
    pending_bindings: HashMap<ObjectHandle, BindingUpdate>,
    pending_vertical: HashMap<ObjectHandle, f32>,
    pending_action_mutations: Vec<ActionMutation>,
    /// Курсор, применённый хостом; меняется только при успешном commit.
    cursor: ScriptCursor,
    pending_cursor_visible: Option<bool>,
    pending_cursor_locked: Option<bool>,
    pending_audio: Vec<AudioRequest>,
    /// Пространственность голосов по `VoiceId::get`, как её записал последний зафиксированный
    /// `play`. Хост переиспользует транзакцию между вызовами, поэтому таблица живёт весь сеанс.
    voice_spatial: BTreeMap<u64, bool>,
    pending_rumble: Vec<RumbleRequest>,
    pending_animation: Vec<AnimationRequest>,
    net: Rc<RefCell<NetScriptState>>,
    queued_net: usize,
    pending_net: Vec<NetRequest>,
    /// Игровой UI хоста; общий с `ScriptHost`, задаётся при создании хоста.
    pub(crate) ui: Rc<RefCell<UiScriptState>>,
    /// Журнал UI-записей текущего вызова: откат при ошибке, отложенные закрытия.
    pub(crate) ui_journal: UiJournal,
    save_values: Rc<RefCell<BTreeMap<String, ParamValue>>>,
    save_flush_requested: Rc<Cell<bool>>,
    save_generation: Rc<Cell<u64>>,
    pending_save: BTreeMap<String, Option<ParamValue>>,
    pending_save_flush: bool,
    lifecycle: Rc<RefCell<Lifecycle>>,
    lifecycle_writes: bool,
    instance_serial: u64,
    /// Резервы корней, созданных в этом вызове; при откате освобождаются.
    pending_spawns: Vec<SpawnRequest>,
    pending_destroys: Vec<DestroyRequest>,
    pending_prefab_requests: Vec<PrefabRequest>,
    /// Явные id spawn-объектов этого вызова: проверка коллизий внутри одного callback.
    pending_ids: HashMap<String, ObjectHandle>,
    /// Предупреждения о мёртвых объектах; в журнал попадают только при коммите.
    pending_dead_warnings: Vec<(ObjectHandle, String)>,
    /// Изменения мира в порядке записи; применяются через `World::apply_edit` при коммите.
    pending_edits: Vec<ObjectEdit>,
    /// Заявки компонентов сцены (свет, mesh, частицы, окружение, LOD, звук); применяются при коммите.
    component_writes: ComponentWrites,
    /// Новое значение `set_enabled` экземпляра; применяется в `commit_lifecycle`.
    pending_script_enabled: Option<bool>,
    /// Справочник экземпляров хоста; читается для `obj:script`, меняется только при коммите.
    script_refs: ScriptRefs,
    /// Состояние смены сцен хоста: список сцен проекта и запрос на загрузку.
    scene_state: SharedSceneState,
    /// Запрос `rve.scene.load` этого вызова; переносится в состояние хоста при коммите.
    pending_scene_request: Option<String>,
    /// Записи свойств чужих и своих экземпляров; применяются при коммите и синхронизируют Lua.
    pending_properties: Vec<PropertyWrite>,
    /// Заявки `enabled` для экземпляров по серийному номеру; применяются на границе фазы.
    pending_remote_enabled: Vec<(u64, bool)>,
    instance_enabled: bool,
    timeline: Rc<RefCell<ScriptTimeline>>,
    time_scale: Rc<Cell<f32>>,
    pending_timeline: Vec<TimelineOp>,
    pending_time_scale: Option<f32>,
    frame_time: crate::types::ScriptFrameTime,
    #[cfg(test)]
    peak_pending_bytes: usize,
}

#[derive(Clone, Copy)]
struct PendingPush {
    controller: engine_physics::CharacterController,
    hit: CharacterHit,
    normal_sum: Vec3,
}

impl ScriptTransaction {
    pub(crate) fn attach_save_state(
        &mut self,
        values: Rc<RefCell<BTreeMap<String, ParamValue>>>,
        flush_requested: Rc<Cell<bool>>,
        generation: Rc<Cell<u64>>,
    ) {
        self.save_values = values;
        self.save_flush_requested = flush_requested;
        self.save_generation = generation;
    }

    pub(crate) fn save_get(&self, key: &str) -> Option<ParamValue> {
        self.pending_save
            .get(key)
            .cloned()
            .unwrap_or_else(|| self.save_values.borrow().get(key).cloned())
    }

    pub(crate) fn save_set(&mut self, key: &str, value: ParamValue) -> Result<(), String> {
        self.require_writable("save writes")?;
        validate_save_key(key)?;
        validate_save_value(&value, 0)?;
        let mut values = self.save_values.borrow().clone();
        for (key, value) in &self.pending_save {
            match value {
                Some(value) => {
                    values.insert(key.clone(), value.clone());
                }
                None => {
                    values.remove(key);
                }
            }
        }
        values.insert(key.to_owned(), value.clone());
        if values.len() > MAX_SAVE_KEYS || save_values_size(&values) > MAX_SAVE_BYTES {
            return Err("save data exceeds the 4096 key or 1 MiB limit".to_owned());
        }
        self.pending_save.insert(key.to_owned(), Some(value));
        Ok(())
    }

    pub(crate) fn save_delete(&mut self, key: &str) -> Result<bool, String> {
        self.require_writable("save writes")?;
        validate_save_key(key)?;
        let existed = self.save_get(key).is_some();
        self.pending_save.insert(key.to_owned(), None);
        Ok(existed)
    }

    pub(crate) fn save_has(&self, key: &str) -> Result<bool, String> {
        validate_save_key(key)?;
        Ok(self.save_get(key).is_some())
    }

    pub(crate) fn save_keys(&self) -> Vec<String> {
        let mut keys = self
            .save_values
            .borrow()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for (key, value) in &self.pending_save {
            if value.is_some() {
                keys.insert(key.clone());
            } else {
                keys.remove(key);
            }
        }
        keys.into_iter().collect()
    }

    pub(crate) fn save_flush(&mut self) -> Result<(), String> {
        self.require_writable("save flush")?;
        self.pending_save_flush = true;
        Ok(())
    }

    pub(crate) fn attach_time_state(
        &mut self,
        timeline: Rc<RefCell<ScriptTimeline>>,
        time_scale: Rc<Cell<f32>>,
    ) {
        self.timeline = timeline;
        self.time_scale = time_scale;
    }

    pub(crate) fn set_frame_time(&mut self, frame_time: crate::types::ScriptFrameTime) {
        self.frame_time = frame_time;
    }

    pub(crate) fn set_fixed_update_dt(&mut self, dt: Option<f32>) {
        self.physics_fixed_dt = dt;
    }

    pub(crate) fn time_scale(&self) -> f32 {
        self.pending_time_scale
            .unwrap_or_else(|| self.time_scale.get())
    }

    pub(crate) fn frame_time(&self) -> crate::types::ScriptFrameTime {
        self.frame_time
    }

    pub(crate) fn stage_time_scale(&mut self, scale: f32) -> Result<(), String> {
        self.require_writable("time scale changes")?;
        if !scale.is_finite() || scale < 0.0 {
            return Err("time scale must be finite and nonnegative".to_owned());
        }
        self.pending_time_scale = Some(scale);
        Ok(())
    }

    pub(crate) fn schedule_lua_timer(
        &mut self,
        delay: f64,
        repeat: Option<f64>,
        callback: Function,
    ) -> Result<u64, String> {
        self.require_timer_write()?;
        validate_timer(delay, repeat)?;
        if !self.timeline.borrow().can_add_timer(&self.pending_timeline) {
            return Err("script timer limit reached".to_owned());
        }
        let due = self.frame_time.elapsed + delay;
        if !due.is_finite() {
            return Err("timer deadline is out of range".to_owned());
        }
        let id = self
            .timeline
            .borrow_mut()
            .allocate_id()
            .map_err(str::to_owned)?;
        ScriptTimeline::stage_timer(
            &mut self.pending_timeline,
            id,
            self.instance_serial,
            due,
            repeat,
            TimerCallback::Lua(callback),
        );
        Ok(id)
    }

    pub(crate) fn schedule_abi_timer(
        &mut self,
        delay: f64,
        repeat: Option<f64>,
        tag: String,
    ) -> Result<u64, String> {
        self.require_timer_write()?;
        validate_timer(delay, repeat)?;
        if !self.timeline.borrow().can_add_timer(&self.pending_timeline) {
            return Err("script timer limit reached".to_owned());
        }
        let due = self.frame_time.elapsed + delay;
        if !due.is_finite() {
            return Err("timer deadline is out of range".to_owned());
        }
        let id = self
            .timeline
            .borrow_mut()
            .allocate_id()
            .map_err(str::to_owned)?;
        ScriptTimeline::stage_timer(
            &mut self.pending_timeline,
            id,
            self.instance_serial,
            due,
            repeat,
            TimerCallback::Abi(tag),
        );
        Ok(id)
    }

    pub(crate) fn cancel_timer(&mut self, id: u64) -> Result<bool, String> {
        self.require_timer_write()?;
        if id == 0
            || (!self
                .timeline
                .borrow()
                .timer_exists(id, self.instance_serial)
                && !ScriptTimeline::timer_pending(&self.pending_timeline, id, self.instance_serial))
        {
            return Ok(false);
        }
        ScriptTimeline::stage_cancel_timer(&mut self.pending_timeline, id, self.instance_serial);
        Ok(true)
    }

    pub(crate) fn start_coroutine(&mut self, thread: Thread) -> Result<u64, String> {
        self.require_timer_write()?;
        if !self
            .timeline
            .borrow()
            .can_add_coroutine(&self.pending_timeline)
        {
            return Err("script coroutine limit reached".to_owned());
        }
        let id = self
            .timeline
            .borrow_mut()
            .allocate_id()
            .map_err(str::to_owned)?;
        let resume_frame = self.frame_time.frame.saturating_add(1);
        ScriptTimeline::stage_coroutine(
            &mut self.pending_timeline,
            ScriptCoroutine::new(id, self.instance_serial, thread, resume_frame),
        );
        Ok(id)
    }

    pub(crate) fn reschedule_coroutine(&mut self, id: u64, thread: Thread, wait: CoroutineWait) {
        ScriptTimeline::stage_coroutine(
            &mut self.pending_timeline,
            ScriptCoroutine::waiting(id, self.instance_serial, thread, wait),
        );
    }

    pub(crate) fn remove_coroutine(&mut self, id: u64) {
        ScriptTimeline::stage_coroutine_remove(
            &mut self.pending_timeline,
            id,
            self.instance_serial,
        );
    }

    fn require_timer_write(&self) -> Result<(), String> {
        self.require_writable("timers and coroutines")?;
        if !self.lifecycle_writes || self.instance_serial == 0 {
            return Err("timers and coroutines are unavailable in this callback".to_owned());
        }
        Ok(())
    }

    /// Пустая транзакция с общим состоянием UI хоста.
    pub(crate) fn with_ui(ui: Rc<RefCell<UiScriptState>>) -> Self {
        Self {
            ui,
            ..Self::default()
        }
    }
}

fn validate_timer(delay: f64, repeat: Option<f64>) -> Result<(), String> {
    if !delay.is_finite() || delay < 0.0 {
        return Err("timer delay must be finite and nonnegative".to_owned());
    }
    if repeat.is_some_and(|repeat| !repeat.is_finite() || repeat <= 0.0) {
        return Err("timer repeat interval must be finite and greater than zero".to_owned());
    }
    Ok(())
}

fn validate_save_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_SAVE_KEY_BYTES || key.chars().any(char::is_control) {
        return Err(format!(
            "save key must contain 1..={MAX_SAVE_KEY_BYTES} non-control UTF-8 bytes"
        ));
    }
    Ok(())
}

pub(crate) fn validate_save_values(values: &BTreeMap<String, ParamValue>) -> Result<(), String> {
    if values.len() > MAX_SAVE_KEYS || save_values_size(values) > MAX_SAVE_BYTES {
        return Err("save data exceeds the 4096 key or 1 MiB limit".to_owned());
    }
    for (key, value) in values {
        validate_save_key(key)?;
        validate_save_value(value, 0)?;
    }
    Ok(())
}

fn validate_save_value(value: &ParamValue, depth: usize) -> Result<(), String> {
    if depth > MAX_MESSAGE_DEPTH {
        return Err(format!("save value nesting exceeds {MAX_MESSAGE_DEPTH}"));
    }
    match value {
        ParamValue::Float(value) if !value.is_finite() => {
            Err("save numbers must be finite".to_owned())
        }
        ParamValue::String(value) if value.len() > MAX_MESSAGE_STRING_BYTES => Err(format!(
            "save strings exceed {MAX_MESSAGE_STRING_BYTES} bytes"
        )),
        ParamValue::Object(value) if value.len() > MAX_MESSAGE_TARGET_ID_BYTES => Err(format!(
            "save object references exceed {MAX_MESSAGE_TARGET_ID_BYTES} bytes"
        )),
        ParamValue::Array(values) if values.len() > MAX_MESSAGE_ELEMENTS => Err(format!(
            "save arrays exceed {MAX_MESSAGE_ELEMENTS} elements"
        )),
        ParamValue::Array(values) => {
            for value in values {
                validate_save_value(value, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn save_values_size(values: &BTreeMap<String, ParamValue>) -> usize {
    fn value_size(value: &ParamValue) -> usize {
        match value {
            ParamValue::Bool(_) => 1,
            ParamValue::Int(_) | ParamValue::Float(_) => 8,
            ParamValue::String(value) => value.len(),
            ParamValue::Object(value) => value.len().saturating_add(16),
            ParamValue::Asset(value) => value.as_str().len().saturating_add(16),
            ParamValue::Array(values) => values.iter().fold(8_usize, |size, value| {
                size.saturating_add(value_size(value))
            }),
        }
    }
    values.iter().fold(0_usize, |size, (key, value)| {
        size.saturating_add(key.len())
            .saturating_add(value_size(value))
            .saturating_add(8)
    })
}

impl Default for ScriptTransaction {
    fn default() -> Self {
        Self {
            world: std::ptr::null(),
            self_object: None,
            writable: false,
            source: String::new(),
            pending_transforms: HashMap::with_capacity(8),
            pending_camera_settings: HashMap::with_capacity(2),
            pending_post_process: HashMap::with_capacity(2),
            pending_screen_shaders: HashMap::new(),
            pending_active_camera: None,
            pending_messages: Vec::with_capacity(16),
            pending_warnings: Vec::new(),
            pending_bytes: 0,
            queued_message_count: 0,
            queued_message_bytes: 0,
            queued_audio: 0,
            queued_rumble: 0,
            queued_animation: 0,
            overflowed_messages: 0,
            action_frame: None,
            physics: Rc::new(RefCell::new(PhysicsScriptState::default())),
            physics_phase_dt: None,
            physics_fixed_dt: None,
            pending_physics_commands: Vec::new(),
            pending_grounded: HashMap::new(),
            pending_collider_enabled: HashMap::new(),
            pending_pushes: HashMap::new(),
            pending_colliders: HashMap::new(),
            pending_rigid_bodies: HashMap::new(),
            pending_controllers: HashMap::new(),
            pending_bindings: HashMap::new(),
            pending_vertical: HashMap::new(),
            pending_action_mutations: Vec::with_capacity(2),
            cursor: ScriptCursor::default(),
            pending_cursor_visible: None,
            pending_cursor_locked: None,
            pending_audio: Vec::new(),
            voice_spatial: BTreeMap::new(),
            pending_rumble: Vec::new(),
            pending_animation: Vec::new(),
            net: Rc::new(RefCell::new(NetScriptState::default())),
            queued_net: 0,
            pending_net: Vec::new(),
            ui: Rc::new(RefCell::new(UiScriptState::default())),
            ui_journal: UiJournal::default(),
            save_values: Rc::new(RefCell::new(BTreeMap::new())),
            save_flush_requested: Rc::new(Cell::new(false)),
            save_generation: Rc::new(Cell::new(0)),
            pending_save: BTreeMap::new(),
            pending_save_flush: false,
            lifecycle: Rc::new(RefCell::new(Lifecycle::default())),
            lifecycle_writes: false,
            instance_serial: 0,
            pending_spawns: Vec::new(),
            pending_destroys: Vec::new(),
            pending_prefab_requests: Vec::new(),
            pending_ids: HashMap::new(),
            pending_dead_warnings: Vec::new(),
            pending_edits: Vec::new(),
            component_writes: ComponentWrites::default(),
            pending_script_enabled: None,
            script_refs: ScriptRefs::default(),
            scene_state: SharedSceneState::default(),
            pending_scene_request: None,
            pending_properties: Vec::new(),
            pending_remote_enabled: Vec::new(),
            instance_enabled: true,
            timeline: Rc::new(RefCell::new(ScriptTimeline::default())),
            time_scale: Rc::new(Cell::new(1.0)),
            pending_timeline: Vec::with_capacity(4),
            pending_time_scale: None,
            frame_time: crate::types::ScriptFrameTime::default(),
            #[cfg(test)]
            peak_pending_bytes: 0,
        }
    }
}

#[cfg(test)]
mod save_validation_tests {
    use super::{MAX_SAVE_KEYS, ScriptTransaction, validate_save_values};
    use engine_scene::ParamValue;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::rc::Rc;

    #[test]
    fn save_accepts_documented_key_array_and_nesting_boundaries() {
        let mut nested = ParamValue::Int(0);
        for _ in 0..engine_scene::MAX_MESSAGE_DEPTH {
            nested = ParamValue::Array(vec![nested]);
        }
        let values = BTreeMap::from([
            ("é".repeat(64), nested),
            (
                "array".to_owned(),
                ParamValue::Array(vec![
                    ParamValue::Bool(true);
                    engine_scene::MAX_MESSAGE_ELEMENTS
                ]),
            ),
        ]);
        assert!(validate_save_values(&values).is_ok());
    }

    #[test]
    fn save_rejects_excess_keys_depth_elements_and_total_size() {
        let mut too_many_keys = (0..MAX_SAVE_KEYS)
            .map(|index| (format!("k{index}"), ParamValue::Int(0)))
            .collect::<BTreeMap<_, _>>();
        too_many_keys.insert("overflow".to_owned(), ParamValue::Int(0));
        assert!(validate_save_values(&too_many_keys).is_err());

        let mut too_deep = ParamValue::Int(0);
        for _ in 0..=engine_scene::MAX_MESSAGE_DEPTH {
            too_deep = ParamValue::Array(vec![too_deep]);
        }
        assert!(validate_save_values(&BTreeMap::from([("deep".to_owned(), too_deep,)])).is_err());

        let too_many_elements = ParamValue::Array(vec![
            ParamValue::Bool(true);
            engine_scene::MAX_MESSAGE_ELEMENTS + 1
        ]);
        assert!(
            validate_save_values(&BTreeMap::from([("array".to_owned(), too_many_elements,)]))
                .is_err()
        );

        let oversized = (0..18)
            .map(|index| (format!("k{index}"), ParamValue::String("x".repeat(60_000))))
            .collect();
        assert!(validate_save_values(&oversized).is_err());
    }

    #[test]
    fn rejected_save_write_does_not_stage_or_mutate_existing_values() {
        let values = Rc::new(RefCell::new(
            (0..MAX_SAVE_KEYS)
                .map(|index| (format!("k{index}"), ParamValue::Int(0)))
                .collect::<BTreeMap<_, _>>(),
        ));
        let original = values.borrow().clone();
        let mut transaction = ScriptTransaction {
            writable: true,
            ..ScriptTransaction::default()
        };
        transaction.attach_save_state(
            values.clone(),
            Rc::new(Cell::new(false)),
            Rc::new(Cell::new(0)),
        );

        assert!(
            transaction
                .save_set("overflow", ParamValue::Bool(true))
                .is_err()
        );
        assert!(transaction.pending_save.is_empty());
        assert_eq!(*values.borrow(), original);
    }
}

#[cfg(test)]
mod timeline_transaction_tests {
    use super::*;
    use crate::timeline::{ScriptTimeline, TimerCallback};

    #[test]
    fn discarded_callback_rolls_back_timer_registration_cancellation_and_time_scale() {
        let timeline = Rc::new(RefCell::new(ScriptTimeline::default()));
        let time_scale = Rc::new(Cell::new(1.0));
        let existing_id = timeline.borrow_mut().allocate_id().unwrap();
        let mut existing = Vec::new();
        ScriptTimeline::stage_timer(
            &mut existing,
            existing_id,
            7,
            1.0,
            None,
            TimerCallback::Abi("existing".to_owned()),
        );
        timeline.borrow_mut().apply(&mut existing);

        let mut transaction = ScriptTransaction::default();
        transaction.attach_time_state(timeline.clone(), time_scale.clone());
        transaction.writable = true;
        transaction.lifecycle_writes = true;
        transaction.instance_serial = 7;
        transaction.frame_time.frame = 3;
        transaction.frame_time.elapsed = 0.5;

        let pending_id = transaction
            .schedule_abi_timer(0.25, None, "pending".to_owned())
            .unwrap();
        assert!(transaction.cancel_timer(existing_id).unwrap());
        transaction.stage_time_scale(0.0).unwrap();
        transaction.discard();

        assert!(!timeline.borrow().timer_exists(pending_id, 7));
        assert!(timeline.borrow().timer_exists(existing_id, 7));
        assert_eq!(time_scale.get(), 1.0);
    }
}

impl ScriptTransaction {
    fn action_frame(&self) -> &ActionFrame {
        static EMPTY_FRAME: OnceLock<ActionFrame> = OnceLock::new();
        self.action_frame
            .as_deref()
            .unwrap_or_else(|| EMPTY_FRAME.get_or_init(ActionFrame::default))
    }

    pub(crate) fn begin(&mut self, init: ScriptTransactionInit<'_>) {
        self.world = init.world;
        self.self_object = init.self_object;
        self.writable = init.writable;
        self.action_frame = Some(init.actions);
        self.physics = init.physics;
        self.physics_phase_dt = init.physics_phase_dt;
        self.physics_fixed_dt = None;
        self.source.clear();
        self.source.push_str(init.source);
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_post_process.clear();
        self.pending_screen_shaders.clear();
        self.pending_active_camera = None;
        self.pending_messages.clear();
        self.pending_warnings.clear();
        self.pending_physics_commands.clear();
        self.pending_grounded.clear();
        self.pending_collider_enabled.clear();
        self.pending_pushes.clear();
        self.clear_component_writes();
        self.component_writes.clear();
        self.pending_bytes = 0;
        self.queued_message_count = init.queued_messages.0;
        self.queued_message_bytes = init.queued_messages.1;
        self.queued_audio = init.queued_audio;
        self.queued_rumble = init.queued_rumble;
        self.queued_animation = init.queued_animation;
        self.overflowed_messages = 0;
        self.pending_action_mutations.clear();
        self.cursor = init.cursor;
        self.pending_cursor_visible = None;
        self.pending_cursor_locked = None;
        self.pending_audio.clear();
        self.pending_rumble.clear();
        self.pending_animation.clear();
        self.net = init.net;
        self.queued_net = init.queued_net;
        self.pending_net.clear();
        self.lifecycle = init.lifecycle;
        self.lifecycle_writes = init.lifecycle_writes;
        self.instance_serial = init.instance_serial;
        self.instance_enabled = init.instance_enabled;
        self.pending_edits.clear();
        self.pending_script_enabled = None;
        self.pending_save.clear();
        self.pending_save_flush = false;
        self.pending_timeline.clear();
        self.pending_time_scale = None;
        self.pending_spawns.clear();
        self.pending_destroys.clear();
        self.pending_prefab_requests.clear();
        self.pending_ids.clear();
        self.pending_dead_warnings.clear();
    }

    pub(crate) fn writable(&self) -> bool {
        self.writable
    }

    pub(crate) fn physics_attached(&self) -> bool {
        self.physics.borrow().is_attached()
    }

    pub(crate) fn input(&self) -> &ScriptInput {
        self.action_frame().input()
    }
    pub(crate) fn action_state(
        &self,
        name: &str,
        kind: engine_project::InputActionKind,
    ) -> Result<ActionValue, String> {
        self.action_frame().state(name, kind)
    }
    pub(crate) fn action_context(&self) -> Option<&str> {
        self.action_frame().context()
    }
    pub(crate) fn action_bindings(
        &self,
        context: &str,
        action: &str,
        slot: &str,
    ) -> Result<Vec<String>, String> {
        self.action_frame().bindings(context, action, slot)
    }
    /// Возвращает курсор с учётом изменений этого вызова, ещё не применённых при commit.
    pub(crate) fn cursor(&self) -> ScriptCursor {
        ScriptCursor {
            visible: self.pending_cursor_visible.unwrap_or(self.cursor.visible),
            locked: self.pending_cursor_locked.unwrap_or(self.cursor.locked),
        }
    }

    pub(crate) fn set_cursor_visible(&mut self, visible: bool) -> Result<(), String> {
        self.ensure_cursor_writable()?;
        self.pending_cursor_visible = Some(visible);
        Ok(())
    }

    pub(crate) fn set_cursor_locked(&mut self, locked: bool) -> Result<(), String> {
        self.ensure_cursor_writable()?;
        self.pending_cursor_locked = Some(locked);
        Ok(())
    }

    fn ensure_cursor_writable(&self) -> Result<(), String> {
        if self.writable {
            Ok(())
        } else {
            Err("cursor changes are unavailable during script creation".to_owned())
        }
    }

    pub(crate) fn stage_action_mutation(&mut self, mutation: ActionMutation) -> Result<(), String> {
        if !self.writable {
            return Err("input mutations are unavailable during script creation".to_owned());
        }
        self.action_frame().validate_mutation(&mutation)?;
        self.pending_action_mutations.push(mutation);
        Ok(())
    }

    /// Ставит звуковой запрос до коммита. При отказе скрипта транзакция его отбрасывает.
    pub(crate) fn stage_audio(&mut self, request: AudioRequest) -> Result<(), String> {
        if !self.writable {
            return Err("audio is unavailable during script creation".to_owned());
        }
        if let AudioRequest::PlayClip { params, .. } = &request {
            if let Some(problem) = engine_scene::audio::clip_spatial_problem(params) {
                return Err(problem.to_owned());
            }
            if params
                .position
                .is_some_and(|position| !position.iter().all(|axis| axis.is_finite()))
            {
                return Err("clip position must be finite".to_owned());
            }
            if let Some(follow) = params.follow
                && self.active_in_hierarchy(follow) != Some(true)
            {
                return Err("clip follow object is not alive and active".to_owned());
            }
        }
        if let AudioRequest::SetVoicePosition { voice, .. } = &request
            && self.voice_is_flat(*voice)
        {
            return Err(
                "set_voice_position needs a spatial voice: play it with position or follow and spatial_blend > 0"
                    .to_owned(),
            );
        }
        if self.queued_audio.saturating_add(self.pending_audio.len())
            >= MAX_AUDIO_REQUESTS_PER_FRAME
        {
            return Err(format!(
                "audio requests limit ({MAX_AUDIO_REQUESTS_PER_FRAME} per frame) reached"
            ));
        }
        self.pending_audio.push(request);
        Ok(())
    }

    /// Голос известен как 2D: его последний `play` (в этом вызове или зафиксированный раньше) не
    /// пространственный. Неизвестный голос (вытесненный или чужой) не отвергается.
    fn voice_is_flat(&self, voice: engine_scene::VoiceId) -> bool {
        let staged = self
            .pending_audio
            .iter()
            .rev()
            .find_map(|request| match request {
                AudioRequest::PlayClip {
                    voice: played,
                    params,
                    ..
                } if *played == voice => Some(!engine_scene::audio::clip_is_spatial(params)),
                _ => None,
            });
        staged.unwrap_or_else(|| self.voice_spatial.get(&voice.get()) == Some(&false))
    }

    /// Ставит запрос вибрации до коммита. Как и звук, он отбрасывается вместе с отказавшим вызовом.
    pub(crate) fn stage_rumble(&mut self, request: RumbleRequest) -> Result<(), String> {
        if !self.writable {
            return Err("rumble is unavailable during script creation".to_owned());
        }
        if self.queued_rumble.saturating_add(self.pending_rumble.len())
            >= MAX_RUMBLE_REQUESTS_PER_FRAME
        {
            return Err(format!(
                "rumble requests limit ({MAX_RUMBLE_REQUESTS_PER_FRAME} per frame) reached"
            ));
        }
        self.pending_rumble.push(request);
        Ok(())
    }

    pub(crate) fn has_audio_source(&self, handle: ObjectHandle) -> bool {
        self.world()
            .is_some_and(|world| world.audio_source(handle).is_some())
    }

    /// Режим сети, который видит скрипт.
    pub(crate) fn net_mode(&self) -> NetMode {
        self.net.borrow().mode()
    }

    /// Номер локального участника; до подключения клиента скрипты видят 0, как и раньше.
    pub(crate) fn net_local_peer(&self) -> PeerId {
        self.net.borrow().local_peer().unwrap_or(SERVER_PEER_ID)
    }

    pub(crate) fn net_peers(&self) -> Vec<PeerId> {
        self.net.borrow().peers().to_vec()
    }

    pub(crate) fn net_object(&self, handle: ObjectHandle) -> Option<NetObjectState> {
        self.net.borrow().object(handle)
    }

    /// Авторитет над состоянием объекта. Объект без `network_identity` не реплицируется и остаётся
    /// локальным. Сервер и offline ведут все объекты; клиент — только `authority = owner`, если он владелец.
    pub(crate) fn net_has_authority(&self, handle: ObjectHandle) -> bool {
        let Some(world) = self.world() else {
            return false;
        };
        if !world.is_alive(handle) {
            return false;
        }
        let Some(identity) = world.network_identity(handle) else {
            return true;
        };
        let net = self.net.borrow();
        match net.mode() {
            NetMode::Offline | NetMode::Host | NetMode::Server => true,
            NetMode::Client => {
                let owner = net.object(handle).and_then(|state| state.owner);
                identity.authority == engine_scene::NetAuthority::Owner
                    && owner.is_some()
                    && owner == net.local_peer()
            }
        }
    }

    /// Владелец объекта совпадает с локальным участником. Пока номер неизвестен, никто не владелец.
    pub(crate) fn net_is_owner(&self, handle: ObjectHandle) -> bool {
        let net = self.net.borrow();
        let owner = net.object(handle).and_then(|state| state.owner);
        owner.is_some() && owner == net.local_peer()
    }

    /// Ставит сообщение сети до коммита. Без `object` это `rve.net.send`, с ним — `obj:net_send`.
    /// В offline-режиме запрос не ставится и ошибки нет; на клиенте адресат только сервер.
    pub(crate) fn send_net(
        &mut self,
        object: Option<ObjectHandle>,
        name: &str,
        value: ParamValue,
        target: NetTarget,
        reliable: bool,
    ) -> Result<(), String> {
        self.ensure_net_writable()?;
        let mode = self.net_mode();
        if mode == NetMode::Offline {
            return Ok(());
        }
        if mode == NetMode::Client && target != NetTarget::Server {
            return Err("на клиенте сетевые сообщения можно отправлять только серверу".to_owned());
        }
        let name = name.to_owned();
        let request = match object {
            Some(object) => NetRequest::ObjectSend {
                object,
                name,
                value,
                target,
                reliable,
            },
            None => NetRequest::Send {
                target,
                name,
                value,
                reliable,
            },
        };
        self.stage_net(request)
    }

    /// Назначает владельца объекта. Работает только на сервере, в listen-server и offline (где запрос не ставится).
    pub(crate) fn set_net_owner(
        &mut self,
        object: ObjectHandle,
        peer: PeerId,
    ) -> Result<(), String> {
        self.ensure_net_writable()?;
        if self.net_mode() == NetMode::Client {
            return Err("set_net_owner доступен только серверу, а не клиенту".to_owned());
        }
        let has_identity = self
            .world()
            .is_some_and(|world| world.network_identity(object).is_some());
        if !has_identity {
            return Err("set_net_owner требует объект с network_identity".to_owned());
        }
        if self.net_mode() == NetMode::Offline {
            return Ok(());
        }
        self.stage_net(NetRequest::SetOwner { object, peer })
    }

    fn ensure_net_writable(&self) -> Result<(), String> {
        if self.writable {
            Ok(())
        } else {
            Err("network is unavailable during script creation".to_owned())
        }
    }

    fn stage_net(&mut self, request: NetRequest) -> Result<(), String> {
        if self.queued_net.saturating_add(self.pending_net.len()) >= MAX_NET_REQUESTS_PER_FRAME {
            return Err(format!(
                "network requests limit ({MAX_NET_REQUESTS_PER_FRAME} per frame) reached"
            ));
        }
        self.pending_net.push(request);
        Ok(())
    }

    pub(crate) fn stage_animation(&mut self, request: AnimationRequest) -> Result<(), String> {
        if !self.writable {
            return Err("animation is unavailable during script creation".to_owned());
        }
        if self
            .queued_animation
            .saturating_add(self.pending_animation.len())
            >= MAX_ANIMATION_REQUESTS_PER_FRAME
        {
            return Err(format!(
                "animation requests limit ({MAX_ANIMATION_REQUESTS_PER_FRAME} per frame) reached"
            ));
        }
        self.pending_animation.push(request);
        Ok(())
    }

    pub(crate) fn has_animator(&self, handle: ObjectHandle) -> bool {
        self.world()
            .is_some_and(|world| world.animator(handle).is_some())
    }

    pub(crate) fn physics_raycast(
        &self,
        ray: Ray,
        include_triggers: bool,
        ignore: Option<ObjectHandle>,
    ) -> Result<Option<QueryHit>, String> {
        let physics = self.physics.borrow();
        let snapshot = physics.snapshot()?;
        let self_controller = self
            .self_object
            .and_then(|object| physics.optional_binding(object))
            .filter(|binding| binding.character_controller.is_some())
            .map(|binding| binding.body);
        let (ignore, excluded) = match ignore {
            Some(object) => {
                let ignore = physics.binding(object)?.body;
                let excluded = self_controller
                    .filter(|body| *body != ignore)
                    .into_iter()
                    .collect::<Vec<_>>();
                (Some(ignore), excluded)
            }
            None => (self_controller, Vec::new()),
        };
        let mut overrides = physics.enabled_overrides();
        overrides.extend(
            self.pending_collider_enabled
                .iter()
                .map(|(body, enabled)| (*body, *enabled)),
        );
        overrides = merge_enabled_overrides(overrides);
        snapshot
            .raycast_with_enabled_overrides(
                &ray,
                QueryFilter {
                    ignore,
                    include_triggers,
                    layer_mask: u32::MAX,
                },
                &excluded,
                &overrides,
            )
            .map_err(|error| error.to_string())
    }

    pub(crate) fn physics_object(&self, body: BodyHandle) -> Option<ObjectHandle> {
        self.physics
            .borrow()
            .object(body)
            .or_else(|| self.lifecycle.borrow().retired_body(body))
    }

    pub(crate) fn body_velocity(&self, object: ObjectHandle) -> Result<Vec3, String> {
        let physics = self.physics.borrow();
        let binding = physics.binding(object)?;
        if binding.kind != BodyKind::Dynamic {
            return Err("object does not have a dynamic rigid body".to_owned());
        }
        physics
            .snapshot()?
            .body_state(binding.body)
            .map(|state| state.body_velocity)
            .ok_or_else(|| "physics body is missing from the query snapshot".to_owned())
    }

    pub(crate) fn set_body_velocity(
        &mut self,
        object: ObjectHandle,
        velocity: Vec3,
    ) -> Result<(), String> {
        self.push_body_command(object, velocity, |body| PhysicsCommand::SetVelocity {
            body,
            velocity,
        })
    }

    pub(crate) fn add_body_impulse(
        &mut self,
        object: ObjectHandle,
        impulse: Vec3,
    ) -> Result<(), String> {
        self.push_body_command(object, impulse, |body| PhysicsCommand::AddImpulse {
            body,
            impulse,
            point: None,
        })
    }

    pub(crate) fn add_body_force(
        &mut self,
        object: ObjectHandle,
        force: Vec3,
    ) -> Result<(), String> {
        if let Some(dt) = self.physics_fixed_dt {
            let impulse = force * dt;
            self.push_body_command(object, force, |body| PhysicsCommand::AddImpulse {
                body,
                impulse,
                point: None,
            })
        } else {
            self.push_body_command(object, force, |body| PhysicsCommand::AddForce {
                body,
                force,
                point: None,
            })
        }
    }

    fn push_body_command(
        &mut self,
        object: ObjectHandle,
        vector: Vec3,
        command: impl FnOnce(BodyHandle) -> PhysicsCommand,
    ) -> Result<(), String> {
        if !self.writable {
            return Err(
                "physics commands are only available during writable script callbacks".to_owned(),
            );
        }
        if !vector.is_finite() {
            return Err("physics vector must be finite".to_owned());
        }
        let binding = self.physics.borrow().binding(object)?;
        if binding.kind != BodyKind::Dynamic {
            return Err("object does not have a dynamic rigid body".to_owned());
        }
        self.pending_physics_commands.push(command(binding.body));
        Ok(())
    }

    pub(crate) fn move_character(
        &mut self,
        object: ObjectHandle,
        displacement: Vec3,
    ) -> Result<CharacterMoveResult, String> {
        if !self.writable {
            return Err(
                "character movement is only available during writable script callbacks".to_owned(),
            );
        }
        if !displacement.is_finite() {
            return Err("character displacement must be finite".to_owned());
        }
        let binding = self.physics.borrow().binding(object)?;
        let controller = self
            .pending_bindings
            .get(&object)
            .and_then(|update| update.controller)
            .or(binding.character_controller)
            .ok_or_else(|| "object does not have a character controller".to_owned())?;
        let mut transform = self
            .local_transform(object)
            .ok_or_else(|| "character object no longer exists".to_owned())?;
        let rotation = Vec3::from_array(transform.rotation) * (std::f32::consts::PI / 180.0);
        let rotation = Quat::from_euler(EulerRot::YXZ, rotation.y, rotation.x, rotation.z);
        let pose = Pose::new(Vec3::from_array(transform.position), rotation)
            .map_err(|error| error.to_string())?;
        let (snapshot, mut overrides) = {
            let physics = self.physics.borrow();
            let snapshot = physics.shared_snapshot()?;
            let mut overrides = physics.enabled_overrides();
            overrides.extend(
                self.pending_collider_enabled
                    .iter()
                    .map(|(body, enabled)| (*body, *enabled)),
            );
            (snapshot, overrides)
        };
        overrides = merge_enabled_overrides(overrides);
        let result = controller
            .move_capsule_with_enabled_overrides(
                &snapshot,
                binding.body,
                pose,
                displacement,
                &[],
                &overrides,
            )
            .map_err(|error| error.to_string())?;
        transform.position = (Vec3::from_array(transform.position) + result.translation).to_array();
        if !self.set_local_transform(object, transform) {
            return Err("could not stage character transform".to_owned());
        }
        self.pending_grounded.insert(object, result.grounded);
        if result.degraded && self.physics.borrow_mut().first_degraded_move(object) {
            self.pending_warnings.push(
                "character movement hit a physics query limit and stopped at its current position"
                    .to_owned(),
            );
        }
        // A slide can report one body on multiple iterations; count this request once per body.
        for (index, hit) in result.hits.iter().enumerate() {
            if hit.body_kind != BodyKind::Dynamic {
                continue;
            }
            let first_hit_for_body = !result.hits[..index]
                .iter()
                .any(|previous| previous.body == hit.body);
            self.accumulate_push(controller, *hit, first_hit_for_body);
        }
        Ok(result)
    }

    fn accumulate_push(
        &mut self,
        controller: engine_physics::CharacterController,
        mut hit: CharacterHit,
        include_requested_displacement: bool,
    ) {
        if !include_requested_displacement {
            hit.requested_displacement = Vec3::ZERO;
        }
        self.pending_pushes
            .entry(hit.body)
            .and_modify(|pending| {
                pending.hit.blocked_displacement += hit.blocked_displacement;
                pending.hit.requested_displacement += hit.requested_displacement;
                pending.normal_sum += hit.normal * hit.blocked_displacement.length();
            })
            .or_insert(PendingPush {
                controller,
                normal_sum: hit.normal * hit.blocked_displacement.length(),
                hit,
            });
    }

    pub(crate) fn is_grounded(&self, object: ObjectHandle) -> Result<bool, String> {
        let binding = self.physics.borrow().binding(object)?;
        if binding.character_controller.is_none() {
            return Err("object does not have a character controller".to_owned());
        }
        Ok(self
            .pending_grounded
            .get(&object)
            .copied()
            .unwrap_or_else(|| self.physics.borrow().grounded(object)))
    }

    pub(crate) fn set_collider_enabled(
        &mut self,
        object: ObjectHandle,
        enabled: bool,
    ) -> Result<(), String> {
        if !self.writable {
            return Err(
                "collider changes are only available during writable script callbacks".to_owned(),
            );
        }
        let binding = self.physics.borrow().binding(object)?;
        self.pending_collider_enabled.insert(binding.body, enabled);
        self.pending_physics_commands
            .push(PhysicsCommand::SetColliderEnabled {
                body: binding.body,
                enabled,
            });
        Ok(())
    }

    pub(crate) fn collider_enabled(&self, object: ObjectHandle) -> Result<bool, String> {
        let binding = self.physics.borrow().binding(object)?;
        if let Some(enabled) = self.pending_collider_enabled.get(&binding.body) {
            return Ok(*enabled);
        }
        self.physics.borrow().collider_enabled(binding.body)
    }

    /// Подключает снимок физики без привязок; нужен тестам, которые проверяют наличие callbacks.
    #[cfg(test)]
    pub(crate) fn attach_physics_for_tests(
        &mut self,
        snapshot: std::sync::Arc<engine_physics::QuerySnapshot>,
    ) {
        self.physics.borrow_mut().set_snapshot(snapshot, &[]);
    }

    fn require_writable(&self, action: &str) -> Result<(), String> {
        if self.writable {
            Ok(())
        } else {
            Err(format!(
                "{action} is only available during writable script callbacks"
            ))
        }
    }

    /// Проверяет тело, которое получит объект после заявок вызова, теми же правилами, что `BodyDesc`
    /// при активации. Отказ возвращается вызову до заявки, поэтому шаг физики не получает тело,
    /// которое не может создать. Меши не проверяются: их геометрия приходит от модели.
    fn validate_body_candidate(
        &self,
        object: ObjectHandle,
        collider: &ColliderComponent,
        rigid: Option<RigidBodyComponent>,
    ) -> Result<(), String> {
        if matches!(
            collider.shape,
            ColliderShape::ConvexMesh | ColliderShape::TriangleMesh
        ) {
            return Ok(());
        }
        let world = self.world().ok_or("physics changes need a live world")?;
        let transform = object_world_transform(world, object).map_err(|error| error.to_string())?;
        let shape = primitive_collider_shape(collider.clone(), transform.scale)
            .map_err(|error| error.to_string())?;
        let mut desc = BodyDesc::new(BodyKind::Static, transform.pose, ColliderDesc::new(shape));
        desc.collider.offset = Vec3::from_array(collider.offset) * transform.scale;
        desc.collider.trigger = collider.trigger;
        desc.layer = u32::from(self.layer(object).unwrap_or(0));
        if let Some(rigid) = rigid {
            desc.kind = physics_kind(rigid.kind);
            desc.mass = rigid.mass;
            desc.linear_damping = rigid.linear_damping;
            desc.angular_damping = rigid.angular_damping;
            desc.gravity_scale = rigid.gravity_scale;
            desc.lock_rotation = rigid.lock_rotation;
        }
        desc.validate().map_err(|error| error.to_string())
    }

    fn clear_component_writes(&mut self) {
        self.pending_colliders.clear();
        self.pending_rigid_bodies.clear();
        self.pending_controllers.clear();
        self.pending_bindings.clear();
        self.pending_vertical.clear();
    }

    fn has_component_writes(&self) -> bool {
        !(self.pending_colliders.is_empty()
            && self.pending_rigid_bodies.is_empty()
            && self.pending_controllers.is_empty())
    }

    /// Коллайдер с учётом заявок этого вызова.
    pub(crate) fn collider_component(
        &self,
        object: ObjectHandle,
    ) -> Result<ColliderComponent, String> {
        if let Some(collider) = self.pending_colliders.get(&object) {
            return Ok(collider.clone());
        }
        self.world()
            .and_then(|world| world.object(object))
            .and_then(|object| object.collider.clone())
            .ok_or_else(|| "object has no collider".to_owned())
    }

    /// Заменяет коллайдер существующего объекта. Форма и смещение запекаются по масштабу объекта,
    /// как при загрузке сцены; trigger меняется отдельной командой, чтобы пары дали нужные события.
    pub(crate) fn set_collider_component(
        &mut self,
        object: ObjectHandle,
        collider: ColliderComponent,
    ) -> Result<(), String> {
        self.require_writable("collider changes")?;
        let body = self
            .physics
            .borrow()
            .optional_binding(object)
            .map(|binding| binding.body);
        let previous_trigger = self.collider_component(object)?.trigger;
        let world = self.world().ok_or("collider changes need a live world")?;
        let scale = object_world_transform(world, object)
            .map_err(|error| error.to_string())?
            .scale;
        let shape =
            primitive_collider_shape(collider.clone(), scale).map_err(|error| error.to_string())?;
        let shape = PrimitiveShape::from_shape(&shape)
            .ok_or_else(|| "collider must be a box, sphere or capsule".to_owned())?;
        let offset = Vec3::from_array(collider.offset) * scale;
        if !offset.is_finite() {
            return Err("collider offset must be finite".to_owned());
        }
        self.validate_body_candidate(object, &collider, self.rigid_body_component(object).ok())?;
        // Объект без тела (неактивный или ещё не привязанный) получит коллайдер из компонента при активации.
        if let Some(body) = body {
            self.pending_physics_commands
                .push(PhysicsCommand::SetCollider {
                    body,
                    shape,
                    offset,
                });
            if collider.trigger != previous_trigger {
                self.pending_physics_commands
                    .push(PhysicsCommand::SetTrigger {
                        body,
                        trigger: collider.trigger,
                    });
            }
        }
        self.pending_colliders.insert(object, collider);
        Ok(())
    }

    pub(crate) fn rigid_body_component(
        &self,
        object: ObjectHandle,
    ) -> Result<RigidBodyComponent, String> {
        if let Some(body) = self.pending_rigid_bodies.get(&object) {
            return Ok(*body);
        }
        self.world()
            .and_then(|world| world.object(object))
            .and_then(|object| object.rigid_body)
            .ok_or_else(|| "object has no rigid body".to_owned())
    }

    /// Заменяет rigid body существующего объекта. Невалидные значения отклоняются до команды, потому
    /// что физический шаг не должен получать тело, которое не может создать.
    pub(crate) fn set_rigid_body_component(
        &mut self,
        object: ObjectHandle,
        component: RigidBodyComponent,
    ) -> Result<(), String> {
        self.require_writable("rigid body changes")?;
        let binding = self.physics.borrow().optional_binding(object);
        let kind = physics_kind(component.kind);
        let finite = [
            component.mass,
            component.linear_damping,
            component.angular_damping,
            component.gravity_scale,
        ]
        .into_iter()
        .all(f32::is_finite);
        if !finite
            || component.linear_damping < 0.0
            || component.angular_damping < 0.0
            || (kind == BodyKind::Dynamic && component.mass <= 0.0)
        {
            return Err(
                "rigid body values must be finite, damping >= 0 and dynamic mass > 0".to_owned(),
            );
        }
        if kind != BodyKind::Kinematic && self.controller_component(object).is_ok() {
            return Err("character controller body must stay kinematic".to_owned());
        }
        if kind == BodyKind::Dynamic {
            let collider = self.collider_component(object)?;
            if collider.shape == ColliderShape::TriangleMesh {
                return Err("dynamic rigid body cannot use a triangle mesh collider".to_owned());
            }
            let parent = self
                .world()
                .and_then(|world| world.object(object))
                .and_then(|o| o.parent);
            if parent.is_some() {
                return Err("dynamic rigid body must be a root object".to_owned());
            }
        }
        if let Ok(collider) = self.collider_component(object) {
            self.validate_body_candidate(object, &collider, Some(component))?;
        }
        if let Some(binding) = binding {
            self.pending_physics_commands
                .push(PhysicsCommand::SetBodyProperties {
                    body: binding.body,
                    kind,
                    mass: component.mass,
                    linear_damping: component.linear_damping,
                    angular_damping: component.angular_damping,
                    gravity_scale: component.gravity_scale,
                    lock_rotation: component.lock_rotation,
                });
            self.pending_bindings.entry(object).or_default().kind = Some(kind);
        }
        self.pending_rigid_bodies.insert(object, component);
        Ok(())
    }

    pub(crate) fn controller_component(
        &self,
        object: ObjectHandle,
    ) -> Result<CharacterControllerComponent, String> {
        if let Some(controller) = self.pending_controllers.get(&object) {
            return Ok(*controller);
        }
        self.world()
            .and_then(|world| world.object(object))
            .and_then(|object| object.character_controller)
            .ok_or_else(|| "object does not have a character controller".to_owned())
    }

    /// Заменяет параметры контроллера. Радиус и высота меняют капсулу командой `SetCollider`.
    pub(crate) fn set_controller_component(
        &mut self,
        object: ObjectHandle,
        component: CharacterControllerComponent,
    ) -> Result<(), String> {
        self.require_writable("character controller changes")?;
        // Тот же компонент не перестраивает капсулу: синхронизация пересчитывает её только при смене компонента,
        // поэтому повторная запись с новым масштабом иначе разошлась бы с привязкой.
        if self.controller_component(object)? == component {
            return Ok(());
        }
        let binding = self.physics.borrow().optional_binding(object);
        let world = self.world().ok_or("controller changes need a live world")?;
        let scale = object_world_transform(world, object)
            .map_err(|error| error.to_string())?
            .scale;
        let controller =
            character_controller_scaled(component, scale).map_err(|error| error.to_string())?;
        let capsule = controller.capsule().map_err(|error| error.to_string())?;
        let shape = PrimitiveShape::from_shape(&capsule)
            .ok_or_else(|| "character capsule must be a capsule".to_owned())?;
        if let Some(binding) = binding {
            self.pending_physics_commands
                .push(PhysicsCommand::SetCollider {
                    body: binding.body,
                    shape,
                    offset: Vec3::ZERO,
                });
            self.pending_bindings.entry(object).or_default().controller = Some(controller);
        }
        self.pending_controllers.insert(object, component);
        Ok(())
    }

    /// Вертикальная скорость контроллера с учётом заявок этого вызова.
    pub(crate) fn vertical_velocity(&self, object: ObjectHandle) -> Result<f32, String> {
        self.controller_component(object)?;
        if let Some(vertical) = self.pending_vertical.get(&object) {
            return Ok(*vertical);
        }
        Ok(self.physics.borrow().vertical_velocity(object))
    }

    fn gravity(&self) -> Result<Vec3, String> {
        let physics = self.physics.borrow();
        Ok(physics.snapshot()?.gravity())
    }

    /// Горизонтальное движение по `velocity.x/z` с гравитацией контроллера. Вертикальная скорость
    /// хранится хостом; `velocity.y` не используется, прыжок задаёт `jump`.
    pub(crate) fn simple_move(
        &mut self,
        object: ObjectHandle,
        velocity: Vec3,
    ) -> Result<CharacterMoveResult, String> {
        if !velocity.is_finite() {
            return Err("character velocity must be finite".to_owned());
        }
        let dt = self
            .physics_phase_dt
            .filter(|dt| dt.is_finite() && *dt > 0.0)
            .ok_or_else(|| {
                "simple_move is only available during fixed update or update".to_owned()
            })?;
        let settings = self.controller_component(object)?;
        let mut vertical = self.vertical_velocity(object)?;
        if settings.use_gravity {
            let gravity = self.gravity()?;
            if self.is_grounded(object)? && vertical < 0.0 {
                vertical = 0.0;
            }
            vertical += gravity.y * settings.gravity_scale * dt;
        } else {
            vertical = 0.0;
        }
        let displacement = Vec3::new(velocity.x, 0.0, velocity.z) * dt + Vec3::Y * (vertical * dt);
        let result = self.move_character(object, displacement)?;
        if result.grounded && vertical < 0.0 {
            vertical = 0.0;
        }
        // Потолок срезал подъём: скорость вверх гаснет, иначе она копилась бы, пока тело упирается.
        if vertical > 0.0 && result.translation.y < displacement.y - 1.0e-4 {
            vertical = 0.0;
        }
        self.pending_vertical.insert(object, vertical);
        Ok(result)
    }

    /// Прыжок с земли: `v = sqrt(2·g·h)`. Возвращает `false`, если контроллер не на земле.
    pub(crate) fn jump(&mut self, object: ObjectHandle) -> Result<bool, String> {
        self.require_writable("jump")?;
        let settings = self.controller_component(object)?;
        if !settings.use_gravity {
            return Err("jump requires use_gravity on the character controller".to_owned());
        }
        if !self.is_grounded(object)? {
            return Ok(false);
        }
        let gravity = self.gravity()?;
        let weight = (-gravity.y * settings.gravity_scale).max(0.0);
        let speed = (2.0 * weight * settings.jump_height).sqrt();
        self.pending_vertical.insert(object, speed);
        Ok(true)
    }

    /// Объекты, которые пересекаются с триггером или контроллером `object` по активным парам.
    pub(crate) fn overlaps(&self, object: ObjectHandle) -> Result<Vec<ObjectHandle>, String> {
        let physics = self.physics.borrow();
        let body = physics.binding(object)?.body;
        let snapshot = physics.snapshot()?;
        Ok(snapshot
            .trigger_pairs()
            .iter()
            .filter_map(|(first, second)| {
                let other = if *first == body {
                    *second
                } else if *second == body {
                    *first
                } else {
                    return None;
                };
                physics.object(other)
            })
            .collect())
    }

    fn query_parts(&self, filter: &ScriptQueryFilter) -> Result<QueryParts, String> {
        let physics = self.physics.borrow();
        let snapshot = physics.shared_snapshot()?;
        let excluded = filter
            .ignore
            .iter()
            .filter_map(|object| {
                physics
                    .optional_binding(*object)
                    .map(|binding| binding.body)
            })
            .collect();
        let mut overrides = physics.enabled_overrides();
        overrides.extend(
            self.pending_collider_enabled
                .iter()
                .map(|(body, enabled)| (*body, *enabled)),
        );
        let overrides = merge_enabled_overrides(overrides);
        let query = QueryFilter {
            ignore: None,
            include_triggers: filter.include_triggers,
            layer_mask: filter.layer_mask,
        };
        Ok((snapshot, query, excluded, overrides))
    }

    /// Объекты, чьи коллайдеры пересекают выпуклую форму в позе `pose`.
    pub(crate) fn overlap_shape(
        &self,
        shape: &Shape,
        pose: Pose,
        filter: &ScriptQueryFilter,
    ) -> Result<Vec<ObjectHandle>, String> {
        let (snapshot, query, excluded, overrides) = self.query_parts(filter)?;
        let bodies = snapshot
            .overlap(
                shape,
                pose,
                query,
                QueryOverrides {
                    excluded: &excluded,
                    enabled: &overrides,
                },
            )
            .map_err(|error| error.to_string())?;
        Ok(bodies
            .into_iter()
            .filter_map(|body| self.physics_object(body))
            .collect())
    }

    /// Все первые попадания луча по коллайдерам, по возрастанию расстояния.
    pub(crate) fn raycast_all(
        &self,
        ray: Ray,
        filter: &ScriptQueryFilter,
    ) -> Result<Vec<QueryHit>, String> {
        let (snapshot, query, excluded, overrides) = self.query_parts(filter)?;
        snapshot
            .raycast_all(
                &ray,
                query,
                QueryOverrides {
                    excluded: &excluded,
                    enabled: &overrides,
                },
            )
            .map_err(|error| error.to_string())
    }

    pub(crate) fn shape_cast(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: &ScriptQueryFilter,
    ) -> Result<Option<QueryHit>, String> {
        let (snapshot, query, excluded, overrides) = self.query_parts(filter)?;
        snapshot
            .shape_cast_with_overrides(
                shape,
                pose,
                displacement,
                skin,
                query,
                QueryOverrides {
                    excluded: &excluded,
                    enabled: &overrides,
                },
            )
            .map_err(|error| error.to_string())
    }

    fn world(&self) -> Option<&World> {
        if self.world.is_null() {
            return None;
        }
        // SAFETY: `begin` receives a live world reference and the host keeps it alive until the
        // callback returns and `commit` or `discard` clears this pointer.
        Some(unsafe { &*self.world })
    }

    pub(crate) fn handle_by_id(&self, id: &str) -> Option<ObjectHandle> {
        self.world()?.handle(id)
    }

    /// Живой объект или pending-объект с явным id. Используют `send` и `rve.destroy`;
    /// `find` видит только живые объекты.
    pub(crate) fn handle_by_any_id(&self, id: &str) -> Option<ObjectHandle> {
        self.handle_by_id(id)
            .or_else(|| self.pending_ids.get(id).copied())
            .or_else(|| self.lifecycle.borrow().pending_ids.get(id).copied())
    }

    /// Живой, pending (зарезервированный) или мёртвый объект.
    pub(crate) fn object_state(&self, handle: ObjectHandle) -> ObjectState {
        match self.world() {
            Some(world) if world.is_alive(handle) => ObjectState::Alive,
            Some(world) if world.is_reserved(handle) => ObjectState::Pending,
            _ => ObjectState::Dead,
        }
    }

    /// Состояние для методов объекта. Мёртвый объект даёт Warn один раз на пару «экземпляр + объект».
    pub(crate) fn object_access(&mut self, handle: ObjectHandle) -> ObjectState {
        let state = self.object_state(handle);
        if state == ObjectState::Dead {
            self.warn_dead(handle);
        }
        state
    }

    fn warn_dead(&mut self, handle: ObjectHandle) {
        if self
            .lifecycle
            .borrow()
            .has_warned(self.instance_serial, handle)
            || self
                .pending_dead_warnings
                .iter()
                .any(|(pending, _)| *pending == handle)
        {
            return;
        }
        let name = self
            .lifecycle
            .borrow()
            .retired(handle)
            .map(|retired| retired.id.clone())
            .unwrap_or_else(|| format!("#{}", handle.index()));
        self.pending_dead_warnings
            .push((handle, format!("script used a destroyed object {name:?}")));
    }

    /// Идентичность объекта для Lua и событий: id (для объекта без явного id в pending он неизвестен) и имя.
    /// Мёртвый объект имеет идентичность, пока жива его физическая привязка.
    pub(crate) fn object_identity(&self, handle: ObjectHandle) -> Option<(Option<String>, String)> {
        let world = self.world()?;
        if let Some(object) = world.object(handle) {
            return Some((Some(object.id.clone()), object.name.clone()));
        }
        if world.is_reserved(handle) {
            if let Some(request) = self.pending_spawns.iter().find(|r| r.root == handle) {
                return Some(spawn_identity(request));
            }
            let lifecycle = self.lifecycle.borrow();
            return lifecycle.pending_spawn(handle).map(spawn_identity);
        }
        let lifecycle = self.lifecycle.borrow();
        lifecycle
            .retired(handle)
            .map(|retired| (Some(retired.id.clone()), retired.name.clone()))
    }

    /// Резервирует корень spawn-объекта и ставит заявку до коммита. Id и параметры проверяются сразу.
    pub(crate) fn spawn(&mut self, prefab: &str, args: SpawnArgs) -> Result<ObjectHandle, String> {
        if !self.writable {
            return Err("spawn is unavailable during script creation".to_owned());
        }
        if !self.lifecycle_writes {
            return Err("spawn is unavailable during destroy callbacks".to_owned());
        }
        let prefab = prefab_key(prefab)?;
        let world = self
            .world()
            .ok_or_else(|| "spawn is only available during script calls".to_owned())?;
        {
            let lifecycle = self.lifecycle.borrow();
            check_spawn_request(
                world,
                &lifecycle,
                self.pending_spawns.len(),
                &self.pending_ids,
                &prefab,
                &args,
            )?;
            // Клиент получает сетевые объекты только от сервера: локальный spawn такого префаба запрещён.
            // Незагруженный тяжёлый префаб проверяется при создании (см. `ScriptHost::materialize`).
            let networked = lifecycle
                .prefabs
                .get(&prefab)
                .and_then(|slot| slot.template.as_deref())
                .is_some_and(networked_template);
            if networked && self.net.borrow().mode() == NetMode::Client {
                return Err(format!(
                    "prefab {:?} has network objects; a client can spawn them only through the network",
                    prefab_display(&prefab)
                ));
            }
        }
        let root = world.reserve_object();
        if let Some(id) = &args.options.id {
            self.pending_ids.insert(id.clone(), root);
        }
        self.pending_spawns.push(SpawnRequest {
            seq: 0,
            root,
            prefab,
            args,
            source: self.source.clone(),
            from_script: true,
        });
        Ok(root)
    }

    /// Ставит удаление поддерева. `Ok(false)` — объект уже удалён; ничего не ставится.
    pub(crate) fn destroy(&mut self, handle: ObjectHandle, delay: f64) -> Result<bool, String> {
        if !self.writable {
            return Err("destroy is unavailable during script creation".to_owned());
        }
        if !self.lifecycle_writes {
            return Err("destroy is unavailable during destroy callbacks".to_owned());
        }
        if !delay.is_finite() || delay < 0.0 {
            return Err("destroy delay must be a finite nonnegative number".to_owned());
        }
        if self.object_access(handle) == ObjectState::Dead {
            return Ok(false);
        }
        let due = (delay > 0.0).then(|| self.lifecycle.borrow().now + delay);
        self.pending_destroys.push(DestroyRequest { handle, due });
        Ok(true)
    }

    /// Заявка загрузки или выгрузки; применяется при коммите. Быстрые префабы не грузятся.
    pub(crate) fn prefab_request(&mut self, request: PrefabRequest) -> Result<(), String> {
        if !self.writable {
            return Err("prefab loading is unavailable during script creation".to_owned());
        }
        let request = match request {
            PrefabRequest::Load(path) => PrefabRequest::Load(prefab_key(&path)?),
            PrefabRequest::Unload(path) => PrefabRequest::Unload(prefab_key(&path)?),
        };
        let path = match &request {
            PrefabRequest::Load(path) | PrefabRequest::Unload(path) => path.clone(),
        };
        let heavy = {
            let lifecycle = self.lifecycle.borrow();
            let slot = lifecycle.prefabs.get(&path).ok_or_else(|| {
                format!(
                    "prefab {:?} is not in the project prefab list",
                    prefab_display(&path)
                )
            })?;
            slot.kind == PrefabKind::Heavy
        };
        if heavy {
            self.pending_prefab_requests.push(request);
        }
        Ok(())
    }

    /// Состояние префаба по зафиксированным данным; `None` — путь не в списке проекта или некорректен.
    pub(crate) fn prefab_status(&self, path: &str) -> Option<PrefabStatus> {
        let key = prefab_key(path).ok()?;
        self.lifecycle
            .borrow()
            .prefabs
            .get(&key)
            .map(|slot| slot.status)
    }

    /// Ставит изменение мира в очередь. Цель должна быть живым объектом; состав правок
    /// проверяет коммит на копии мира, поэтому отказ отбрасывает весь вызов.
    pub(crate) fn stage_edit(&mut self, edit: ObjectEdit) -> bool {
        let target = match &edit {
            ObjectEdit::SetActive { object, .. }
            | ObjectEdit::SetKeepOnLoad { object, .. }
            | ObjectEdit::SetTag { object, .. }
            | ObjectEdit::SetLayer { object, .. }
            | ObjectEdit::SetParent { object, .. }
            | ObjectEdit::SetWorldPosition { object, .. }
            | ObjectEdit::SetWorldRotation { object, .. } => *object,
        };
        let value_valid = match &edit {
            ObjectEdit::SetTag { tag, .. } => {
                tag.is_empty()
                    || (tag.len() <= 64
                        && tag.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                        }))
            }
            ObjectEdit::SetLayer { layer, .. } => *layer < LAYER_COUNT,
            _ => true,
        };
        if !self.writable
            || !value_valid
            || self
                .world()
                .and_then(|world| world.object(target))
                .is_none()
        {
            return false;
        }
        if let ObjectEdit::SetLayer { object, layer } = &edit
            && let Some(binding) = self.physics.borrow().optional_binding(*object)
        {
            self.pending_physics_commands
                .push(PhysicsCommand::SetLayer {
                    body: binding.body,
                    layer: u32::from(*layer),
                });
        }
        self.pending_edits.push(edit);
        true
    }

    /// Проверяет заявки до записи в мир: любая отказавшая заявка отбрасывает весь вызов.
    ///
    /// Тег, слой, мировая поза и компоненты проверяются на текущем мире без копии, если вызов не
    /// меняет активность и родителя и каждая проверка видит тех же предков, что и запись (см.
    /// `pose_edits_are_local`). Во всех остальных случаях заявки применяются к копии мира в том же
    /// порядке, что и коммит. Собственные трансформы объектов от этого не зависят. Смена активной
    /// камеры конфликтует только с активностью и родителем, поэтому сама копию не требует.
    fn probe_edits(&self, world: &World) -> Result<(), String> {
        if self.pending_edits.is_empty() && !self.has_component_writes() {
            return Ok(());
        }
        // Компонентные проверки зависят только от объекта и его родителя, поэтому без смены активности
        // и родителя проверяются на текущем мире, как и позы.
        let local_only = self.pending_edits.iter().all(|edit| {
            !matches!(
                edit,
                ObjectEdit::SetActive { .. }
                    | ObjectEdit::SetKeepOnLoad { .. }
                    | ObjectEdit::SetParent { .. }
            )
        }) && self.pose_edits_are_local(world);
        if !local_only {
            let mut probe = world.clone();
            return self.apply_world_writes(&mut probe);
        }
        for (handle, collider) in &self.pending_colliders {
            world
                .check_collider(*handle, collider)
                .map_err(|error| error.to_string())?;
        }
        for (handle, body) in &self.pending_rigid_bodies {
            world
                .check_rigid_body(*handle, body)
                .map_err(|error| error.to_string())?;
        }
        for (handle, controller) in &self.pending_controllers {
            world
                .check_character_controller(*handle, controller)
                .map_err(|error| error.to_string())?;
        }
        for edit in &self.pending_edits {
            match edit {
                ObjectEdit::SetTag { object, .. } | ObjectEdit::SetLayer { object, .. } => {
                    if world.object(*object).is_none() {
                        return Err("script edited an unknown object".to_owned());
                    }
                }
                ObjectEdit::SetWorldPosition { object, position } => world
                    .check_world_position(*object, *position)
                    .map_err(|error| error.to_string())?,
                ObjectEdit::SetWorldRotation {
                    object,
                    rotation_deg,
                } => world
                    .check_world_rotation(*object, *rotation_deg)
                    .map_err(|error| error.to_string())?,
                // Исключены условием local_only выше.
                ObjectEdit::SetActive { .. }
                | ObjectEdit::SetKeepOnLoad { .. }
                | ObjectEdit::SetParent { .. } => {}
            }
        }
        Ok(())
    }

    /// Ни один объект позы не является предком другого объекта позы в этом вызове.
    /// Поза объекта зависит от мировых матриц предков, но не от его собственного трансформа. Поэтому
    /// проверка на текущем мире точна, если ни один предок позируемого объекта не получил в этом
    /// вызове трансформ и не является позируемым сам.
    fn pose_edits_are_local(&self, world: &World) -> bool {
        let posed = self
            .pending_edits
            .iter()
            .filter_map(|edit| match edit {
                ObjectEdit::SetWorldPosition { object, .. }
                | ObjectEdit::SetWorldRotation { object, .. } => Some(*object),
                _ => None,
            })
            .collect::<HashSet<_>>();
        posed.iter().all(|&handle| {
            let mut ancestor = world.object(handle).and_then(|object| object.parent);
            while let Some(current) = ancestor {
                if posed.contains(&current) || self.pending_transforms.contains_key(&current) {
                    return false;
                }
                ancestor = world.object(current).and_then(|object| object.parent);
            }
            true
        })
    }

    /// Записывает заявки вызова в мир в порядке коммита: трансформы, камеры, активная камера,
    /// правки мира. Заявки не очищаются; очистку делает коммит после успешной записи.
    fn apply_world_writes(&self, world: &mut World) -> Result<(), String> {
        for (handle, transform) in &self.pending_transforms {
            world
                .set_local_transform(*handle, *transform)
                .map_err(|error| error.to_string())?;
        }
        for (handle, settings) in &self.pending_camera_settings {
            world
                .set_camera_settings(*handle, *settings)
                .map_err(|error| error.to_string())?;
        }
        for (handle, settings) in &self.pending_post_process {
            world
                .set_post_process(*handle, *settings)
                .map_err(|error| error.to_string())?;
        }
        for ((handle, slot), state) in &self.pending_screen_shaders {
            world
                .set_screen_shader(*handle, *slot, state.enabled, &state.params)
                .map_err(|error| error.to_string())?;
        }
        for (handle, collider) in &self.pending_colliders {
            world
                .set_collider(*handle, collider.clone())
                .map_err(|error| error.to_string())?;
        }
        for (handle, body) in &self.pending_rigid_bodies {
            world
                .set_rigid_body(*handle, *body)
                .map_err(|error| error.to_string())?;
        }
        for (handle, controller) in &self.pending_controllers {
            world
                .set_character_controller(*handle, *controller)
                .map_err(|error| error.to_string())?;
        }
        if let Some(handle) = self.pending_active_camera {
            world
                .set_active_camera(handle)
                .map_err(|error| error.to_string())?;
        }
        for edit in &self.pending_edits {
            world.apply_edit(edit).map_err(|error| error.to_string())?;
        }
        let writes = &self.component_writes;
        for (handle, light) in &writes.directional_lights {
            world
                .set_directional_light(*handle, *light)
                .map_err(|error| error.to_string())?;
        }
        for (handle, light) in &writes.point_lights {
            world
                .set_point_light(*handle, *light)
                .map_err(|error| error.to_string())?;
        }
        for (handle, light) in &writes.spot_lights {
            world
                .set_spot_light(*handle, *light)
                .map_err(|error| error.to_string())?;
        }
        for (handle, enabled) in &writes.light_enabled {
            world
                .set_light_enabled(*handle, *enabled)
                .map_err(|error| error.to_string())?;
        }
        for (handle, visible) in &writes.mesh_visible {
            world
                .set_mesh_visible(*handle, *visible)
                .map_err(|error| error.to_string())?;
        }
        for (handle, params) in &writes.mesh_material {
            world
                .set_mesh_material_params(*handle, *params)
                .map_err(|error| error.to_string())?;
        }
        for (handle, path) in &writes.mesh_material_path {
            world
                .set_mesh_material_path(*handle, path.clone())
                .map_err(|error| error.to_string())?;
        }
        for (handle, playing) in &writes.particles_playing {
            world
                .set_particles_playing(*handle, *playing)
                .map_err(|error| error.to_string())?;
        }
        for (handle, intensity) in &writes.environment_intensity {
            world
                .set_environment_intensity(*handle, *intensity)
                .map_err(|error| error.to_string())?;
        }
        for (handle, sky) in &writes.sky {
            world
                .set_procedural_sky(*handle, *sky)
                .map_err(|error| error.to_string())?;
        }
        for (handle, (distances, cull)) in &writes.lod {
            world
                .set_lod_distances(*handle, distances, *cull)
                .map_err(|error| error.to_string())?;
        }
        for (handle, settings) in &writes.audio_sources {
            world
                .set_audio_source_settings(*handle, settings.clone())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Проверяет, что объекты заявок всё ещё имеют нужные компоненты. Значения проверены при
    /// вызове, поэтому здесь только присутствие: без клонирования мира.
    fn component_writes_fit(&self, world: &World) -> Result<(), String> {
        let writes = &self.component_writes;
        let has = |handle: &ObjectHandle,
                   present: fn(&engine_scene::world::WorldObject) -> bool| {
            world.object(*handle).is_some_and(present)
        };
        let missing = "script changed a component on an object that no longer has it";
        let fits = writes
            .directional_lights
            .keys()
            .all(|handle| has(handle, |object| object.directional_light.is_some()))
            && writes
                .point_lights
                .keys()
                .all(|handle| has(handle, |object| object.point_light.is_some()))
            && writes
                .spot_lights
                .keys()
                .all(|handle| has(handle, |object| object.spot_light.is_some()))
            && writes.light_enabled.keys().all(|handle| {
                has(handle, |object| {
                    object.directional_light.is_some()
                        || object.point_light.is_some()
                        || object.spot_light.is_some()
                })
            })
            && writes
                .mesh_visible
                .keys()
                .chain(writes.mesh_material.keys())
                .chain(writes.mesh_material_path.keys())
                .all(|handle| has(handle, |object| object.mesh_renderer.is_some()))
            && writes
                .particles_playing
                .keys()
                .chain(writes.particle_commands.iter().map(particle_object))
                .all(|handle| has(handle, |object| object.particle_emitter.is_some()))
            && writes
                .environment_intensity
                .keys()
                .all(|handle| has(handle, |object| object.environment.is_some()))
            && writes.sky.keys().all(|handle| {
                has(handle, |object| {
                    object
                        .environment
                        .as_ref()
                        .is_some_and(|environment| environment.procedural.is_some())
                })
            })
            && writes
                .lod
                .keys()
                .all(|handle| has(handle, |object| object.lod_group.is_some()))
            && writes
                .audio_sources
                .keys()
                .all(|handle| has(handle, |object| object.audio_source.is_some()));
        if fits {
            Ok(())
        } else {
            Err(missing.to_owned())
        }
    }

    /// Последняя заявка активности самого объекта в этом вызове.
    fn pending_self_active(&self, handle: ObjectHandle) -> Option<bool> {
        self.pending_edits.iter().rev().find_map(|edit| match edit {
            ObjectEdit::SetActive { object, active } if *object == handle => Some(*active),
            _ => None,
        })
    }

    /// Собственная активность с учётом заявок этого вызова.
    pub(crate) fn is_active(&self, handle: ObjectHandle) -> Option<bool> {
        self.pending_self_active(handle)
            .or_else(|| self.world()?.is_active(handle))
    }

    /// Действующая активность: заявки этого вызова учитываются для всех предков; родитель
    /// берётся из мира, поэтому заявки `set_parent` видны только после коммита.
    pub(crate) fn active_in_hierarchy(&self, handle: ObjectHandle) -> Option<bool> {
        let world = self.world()?;
        let mut current = Some(handle);
        while let Some(object) = current {
            let object_ref = world.object(object)?;
            if !self
                .pending_self_active(object)
                .unwrap_or(object_ref.active)
            {
                return Some(false);
            }
            current = object_ref.parent;
        }
        Some(true)
    }

    pub(crate) fn keep_on_load(&self, handle: ObjectHandle) -> Option<bool> {
        self.pending_edits
            .iter()
            .rev()
            .find_map(|edit| match edit {
                ObjectEdit::SetKeepOnLoad { object, keep } if *object == handle => Some(*keep),
                _ => None,
            })
            .or_else(|| self.world()?.keep_on_load(handle))
    }

    pub(crate) fn set_keep_on_load(&mut self, handle: ObjectHandle, keep: bool) -> bool {
        if keep && self.parent(handle).flatten().is_some() {
            return false;
        }
        self.stage_edit(ObjectEdit::SetKeepOnLoad {
            object: handle,
            keep,
        })
    }

    /// Родитель с учётом заявок этого вызова; внешний `None` — объект мёртв.
    pub(crate) fn parent(&self, handle: ObjectHandle) -> Option<Option<ObjectHandle>> {
        let committed = self.world()?.object(handle)?.parent;
        Some(
            self.pending_edits
                .iter()
                .rev()
                .find_map(|edit| match edit {
                    ObjectEdit::SetParent { object, parent, .. } if *object == handle => {
                        Some(*parent)
                    }
                    _ => None,
                })
                .unwrap_or(committed),
        )
    }

    /// Прямые потомки по последнему коммиту мира.
    pub(crate) fn children(&self, handle: ObjectHandle) -> Option<Vec<ObjectHandle>> {
        self.world()?.children(handle)
    }

    /// Мировая позиция по последнему коммиту мира.
    pub(crate) fn world_position(&self, handle: ObjectHandle) -> Option<Vec3> {
        self.world()?.world_position(handle)
    }

    /// Мировой поворот в градусах по последнему коммиту мира.
    pub(crate) fn world_rotation_deg(&self, handle: ObjectHandle) -> Option<Vec3> {
        self.world()?.world_rotation_deg(handle)
    }

    /// Тег с учётом заявок этого вызова; пустая строка означает отсутствие тега.
    pub(crate) fn tag(&self, handle: ObjectHandle) -> Option<&str> {
        let pending = self.pending_edits.iter().rev().find_map(|edit| match edit {
            ObjectEdit::SetTag { object, tag } if *object == handle => Some(tag.as_str()),
            _ => None,
        });
        pending.or_else(|| self.world()?.tag(handle))
    }

    /// Слой с учётом заявок этого вызова.
    pub(crate) fn layer(&self, handle: ObjectHandle) -> Option<u8> {
        let pending = self.pending_edits.iter().rev().find_map(|edit| match edit {
            ObjectEdit::SetLayer { object, layer } if *object == handle => Some(*layer),
            _ => None,
        });
        pending.or_else(|| self.world()?.layer(handle))
    }

    pub(crate) fn has_component(&self, handle: ObjectHandle, kind: &ComponentKind) -> Option<bool> {
        self.world()?.has_component(handle, kind)
    }

    /// Живые объекты с этим отображаемым именем по последнему коммиту мира.
    pub(crate) fn find_all_by_name(&self, name: &str) -> Vec<ObjectHandle> {
        self.world()
            .map(|world| world.find_all_by_name(name))
            .unwrap_or_default()
    }

    pub(crate) fn find_by_name(&self, name: &str) -> Option<ObjectHandle> {
        self.world()?.find_by_name(name)
    }

    /// Живые объекты с этим тегом по последнему коммиту мира.
    pub(crate) fn find_all_with_tag(&self, tag: &str) -> Vec<ObjectHandle> {
        self.world()
            .map(|world| world.find_all_with_tag(tag))
            .unwrap_or_default()
    }

    pub(crate) fn find_with_tag(&self, tag: &str) -> Option<ObjectHandle> {
        self.world()?.find_with_tag(tag)
    }

    /// Пользовательский флаг экземпляра с учётом заявки этого вызова.
    pub(crate) fn script_enabled(&self) -> bool {
        self.pending_script_enabled.unwrap_or(self.instance_enabled)
    }

    /// Ставит новый флаг `set_enabled` текущего экземпляра. Применяется после успешного коммита.
    pub(crate) fn set_script_enabled(&mut self, enabled: bool) -> bool {
        if !self.writable {
            return false;
        }
        self.pending_script_enabled = Some(enabled);
        true
    }

    pub(crate) fn attach_script_refs(&mut self, refs: ScriptRefs) {
        self.script_refs = refs;
    }

    /// Серийный номер экземпляра `key` на объекте по справочнику хоста.
    pub(crate) fn script_ref(&self, object: ObjectHandle, key: &str) -> Option<u64> {
        find_serial(&self.script_refs.borrow(), object, key)
    }

    /// Значение объявленного свойства с учётом записей этого вызова.
    pub(crate) fn script_get(&self, serial: u64, name: &str) -> Result<ParamValue, ScriptRefError> {
        if let Some(write) = self
            .pending_properties
            .iter()
            .rev()
            .find(|write| write.serial == serial && write.name == name)
        {
            return Ok(write.value.clone());
        }
        let refs = self.script_refs.borrow();
        let entry = find_entry(&refs, serial).ok_or(ScriptRefError::Dead)?;
        entry
            .properties
            .get(name)
            .cloned()
            .ok_or_else(|| ScriptRefError::Undeclared(name.to_owned()))
    }

    /// Ставит запись свойства после проверки по схеме экземпляра; применяется при коммите.
    pub(crate) fn script_set(
        &mut self,
        serial: u64,
        name: &str,
        value: &ParamValue,
    ) -> Result<(), ScriptRefError> {
        if !self.writable {
            return Err(ScriptRefError::Invalid(
                "script properties can only be set from update callbacks".to_owned(),
            ));
        }
        let value = {
            let refs = self.script_refs.borrow();
            let entry = find_entry(&refs, serial).ok_or(ScriptRefError::Dead)?;
            coerce_property(entry, name, value)?
        };
        self.pending_properties.push(PropertyWrite {
            serial,
            name: name.to_owned(),
            value,
        });
        Ok(())
    }

    /// Адресное сообщение одному экземпляру; остальные экземпляры объекта его не получают.
    pub(crate) fn script_send(
        &mut self,
        serial: u64,
        name: &str,
        value: ParamValue,
        value_size: usize,
    ) -> Result<(), QueueMessageError> {
        if !self.writable {
            return Err(QueueMessageError::ReadOnly);
        }
        let object = {
            let refs = self.script_refs.borrow();
            find_entry(&refs, serial)
                .map(|entry| entry.object)
                .ok_or(QueueMessageError::UnknownTarget)?
        };
        self.push_message(
            MessageTarget::Instance { object, serial },
            name,
            value,
            value_size,
        )
    }

    /// `enabled` экземпляра с учётом заявок этого вызова.
    pub(crate) fn script_enabled_of(&self, serial: u64) -> Result<bool, ScriptRefError> {
        if let Some((_, enabled)) = self
            .pending_remote_enabled
            .iter()
            .rev()
            .find(|(pending, _)| *pending == serial)
        {
            return Ok(*enabled);
        }
        let refs = self.script_refs.borrow();
        find_entry(&refs, serial)
            .map(|entry| entry.enabled)
            .ok_or(ScriptRefError::Dead)
    }

    /// Заявка `enabled` для экземпляра по серийному номеру; применяется на границе фазы.
    pub(crate) fn set_script_enabled_of(
        &mut self,
        serial: u64,
        enabled: bool,
    ) -> Result<(), ScriptRefError> {
        if !self.writable {
            return Err(ScriptRefError::Invalid(
                "script enabled state can only be set from update callbacks".to_owned(),
            ));
        }
        if find_entry(&self.script_refs.borrow(), serial).is_none() {
            return Err(ScriptRefError::Dead);
        }
        self.pending_remote_enabled.push((serial, enabled));
        Ok(())
    }

    pub(crate) fn attach_scene_state(&mut self, state: SharedSceneState) {
        self.scene_state = state;
    }

    /// Запрос загрузки сцены: путь должен быть объявлен и включён в проекте. Запрос последнего
    /// успешного вызова кадра отдаёт хост после конца кадра.
    pub(crate) fn request_scene_load(&mut self, path: &str) -> Result<(), String> {
        if !self.writable || !self.lifecycle_writes {
            return Err("scenes can only be loaded from update callbacks".to_owned());
        }
        let canonical = check_scene_request(&self.scene_state.borrow(), path)?;
        self.pending_scene_request = Some(canonical);
        Ok(())
    }

    /// Имя текущей сцены.
    pub(crate) fn current_scene(&self) -> String {
        self.scene_state.borrow().current.clone()
    }

    pub(crate) fn object_id(&self, handle: ObjectHandle) -> Option<&str> {
        self.world()?
            .object(handle)
            .map(|object| object.id.as_str())
    }

    pub(crate) fn object_name(&self, handle: ObjectHandle) -> Option<&str> {
        self.world()?
            .object(handle)
            .map(|object| object.name.as_str())
    }

    pub(crate) fn local_transform(&self, handle: ObjectHandle) -> Option<ScriptTransform> {
        self.pending_transforms
            .get(&handle)
            .copied()
            .or_else(|| self.world()?.local_transform(handle).copied())
            .map(script_transform)
    }

    pub(crate) fn set_local_transform(
        &mut self,
        handle: ObjectHandle,
        transform: ScriptTransform,
    ) -> bool {
        if !self.writable
            || self
                .world()
                .and_then(|world| world.object(handle))
                .is_none()
        {
            return false;
        }
        self.pending_transforms
            .insert(handle, local_transform(transform));
        true
    }

    pub(crate) fn camera_settings(&self, handle: ObjectHandle) -> Option<CameraSettings> {
        self.pending_camera_settings
            .get(&handle)
            .copied()
            .or_else(|| self.world()?.camera_settings(handle).copied())
    }

    pub(crate) fn set_camera_settings(
        &mut self,
        handle: ObjectHandle,
        settings: CameraSettings,
    ) -> bool {
        if !self.writable || !self.is_camera(handle) {
            return false;
        }
        // Устаревший блок [color] — прокси к эффективной постобработке камеры. Изменение
        // сравнивается с текущим эффективным значением (с учётом уже поставленной в очередь
        // копии), поэтому запись без изменений не затирает `.postprocess`, а порядок вызовов с
        // set_post_process сохраняется: обе операции обновляют одну и ту же очередь.
        if let Some(color) = settings.color
            && let Some(mut post) = self.post_process(handle)
        {
            let exposure_changed = color.exposure_ev != post.exposure.ev;
            let tonemapper_changed = color.tonemapper != post.tonemapping.tonemapper;
            if exposure_changed || tonemapper_changed {
                if exposure_changed {
                    post.exposure.ev = color.exposure_ev;
                }
                if tonemapper_changed {
                    post.tonemapping.tonemapper = color.tonemapper;
                }
                self.pending_post_process.insert(handle, post);
            }
        }
        self.pending_camera_settings.insert(handle, settings);
        true
    }

    /// Постобработка камеры с учётом уже поставленных в очередь изменений.
    pub(crate) fn post_process(&self, handle: ObjectHandle) -> Option<PostProcessSettings> {
        self.pending_post_process
            .get(&handle)
            .copied()
            .or_else(|| self.world()?.post_process(handle).copied())
    }

    pub(crate) fn set_post_process(
        &mut self,
        handle: ObjectHandle,
        settings: PostProcessSettings,
    ) -> bool {
        if !self.writable || !self.is_camera(handle) {
            return false;
        }
        self.pending_post_process.insert(handle, settings);
        true
    }

    pub(crate) fn set_post_process_from_abi(
        &mut self,
        handle: u64,
        settings: PostProcessSettings,
    ) -> bool {
        self.object_from_abi_handle(handle)
            .is_some_and(|handle| self.set_post_process(handle, settings))
    }

    /// Число экранных шейдеров камеры; `None`, если объект не камера.
    pub(crate) fn screen_shader_count(&self, handle: ObjectHandle) -> Option<usize> {
        self.world()?.screen_shaders(handle).map(<[_]>::len)
    }

    /// Состояние слота экранного шейдера с учётом уже поставленных в очередь изменений.
    pub(crate) fn screen_shader(
        &self,
        handle: ObjectHandle,
        slot: usize,
    ) -> Option<ScreenShaderState> {
        if let Some(state) = self.pending_screen_shaders.get(&(handle, slot)) {
            return Some(state.clone());
        }
        let shader = self.world()?.screen_shaders(handle)?.get(slot)?;
        let mut params = [0.0; MAX_SCREEN_SHADER_PARAMS];
        for (target, value) in params.iter_mut().zip(&shader.params) {
            *target = *value;
        }
        Some(ScreenShaderState {
            enabled: shader.enabled,
            params,
        })
    }

    /// Ставит состояние слота в очередь; путь и стадия не меняются. Ошибка — не камера, нет слота
    /// или параметры не конечны.
    pub(crate) fn set_screen_shader(
        &mut self,
        handle: ObjectHandle,
        slot: usize,
        state: ScreenShaderState,
    ) -> bool {
        if !self.writable
            || self
                .screen_shader_count(handle)
                .is_none_or(|count| slot >= count)
            || !state.params.iter().all(|value| value.is_finite())
        {
            return false;
        }
        self.pending_screen_shaders.insert((handle, slot), state);
        true
    }

    pub(crate) fn set_screen_shader_from_abi(
        &mut self,
        handle: u64,
        slot: usize,
        state: ScreenShaderState,
    ) -> bool {
        self.object_from_abi_handle(handle)
            .is_some_and(|handle| self.set_screen_shader(handle, slot, state))
    }

    fn is_camera(&self, handle: ObjectHandle) -> bool {
        self.world()
            .and_then(|world| world.object(handle))
            .is_some_and(|object| object.camera.is_some())
    }

    pub(crate) fn active_camera(&self) -> Option<ObjectHandle> {
        self.pending_active_camera
            .or_else(|| self.world().map(World::active_camera))
    }

    pub(crate) fn set_active_camera(&mut self, handle: ObjectHandle) -> bool {
        if !self.writable
            || self
                .world()
                .and_then(|world| world.object(handle))
                .is_none_or(|object| object.camera.is_none())
        {
            return false;
        }
        self.pending_active_camera = Some(handle);
        true
    }

    pub(crate) fn world_matrix(&self, handle: ObjectHandle) -> Option<[f32; 16]> {
        self.world()?
            .world_matrix(handle)
            .map(|matrix| matrix.to_cols_array())
    }

    /// Разбирает ABI-значение и проверяет объект. Мёртвый и pending-объект дают `Unavailable`:
    /// вызов ничего не делает, а Warn пишется один раз на пару «экземпляр + объект».
    pub(crate) fn abi_object(&mut self, handle: u64) -> AbiObject {
        let Some(handle) = ObjectHandle::from_abi(handle) else {
            return AbiObject::Malformed;
        };
        match self.object_access(handle) {
            ObjectState::Alive => AbiObject::Alive(handle),
            ObjectState::Pending | ObjectState::Dead => AbiObject::Unavailable,
        }
    }

    /// Живой объект по ABI-значению без сообщений; устаревший хендл даёт `None` даже после
    /// переиспользования слота. Мёртвый объект пишет Warn, как и [`Self::abi_object`].
    pub(crate) fn object_from_abi_handle(&mut self, handle: u64) -> Option<ObjectHandle> {
        match self.abi_object(handle) {
            AbiObject::Alive(handle) => Some(handle),
            AbiObject::Malformed | AbiObject::Unavailable => None,
        }
    }

    pub(crate) fn abi_handle(&self, handle: ObjectHandle) -> Option<u64> {
        handle.to_abi()
    }

    pub(crate) fn set_local_from_abi(&mut self, handle: u64, transform: ScriptTransform) -> bool {
        self.object_from_abi_handle(handle)
            .is_some_and(|handle| self.set_local_transform(handle, transform))
    }

    pub(crate) fn set_camera_settings_from_abi(
        &mut self,
        handle: u64,
        settings: CameraSettings,
    ) -> bool {
        self.object_from_abi_handle(handle)
            .is_some_and(|handle| self.set_camera_settings(handle, settings))
    }

    pub(crate) fn set_active_camera_from_abi(&mut self, handle: u64) -> bool {
        self.object_from_abi_handle(handle)
            .is_some_and(|handle| self.set_active_camera(handle))
    }

    pub(crate) fn send(
        &mut self,
        target_id: &str,
        name: &str,
        value: ParamValue,
        value_size: usize,
    ) -> Result<(), QueueMessageError> {
        if !self.writable {
            return Err(QueueMessageError::ReadOnly);
        }
        let target = self
            .handle_by_any_id(target_id)
            .ok_or(QueueMessageError::UnknownTarget)?;
        self.push_message(MessageTarget::Object(target), name, value, value_size)
    }

    pub(crate) fn broadcast(
        &mut self,
        name: &str,
        value: ParamValue,
        value_size: usize,
    ) -> Result<(), QueueMessageError> {
        if !self.writable {
            return Err(QueueMessageError::ReadOnly);
        }
        self.push_message(MessageTarget::Broadcast, name, value, value_size)
    }

    fn push_message(
        &mut self,
        target: MessageTarget,
        name: &str,
        value: ParamValue,
        value_size: usize,
    ) -> Result<(), QueueMessageError> {
        if name.len() > MAX_MESSAGE_NAME_BYTES {
            return Err(QueueMessageError::InvalidName);
        }
        let sender_handle = self.self_object.ok_or(QueueMessageError::InvalidValue)?;
        let sender_id_len = self
            .object_id(sender_handle)
            .map(str::len)
            .ok_or(QueueMessageError::InvalidValue)?;
        let estimated_size = sender_id_len
            .saturating_add(name.len())
            .saturating_add(value_size);
        if self
            .queued_message_count
            .saturating_add(self.pending_messages.len())
            >= MAX_MESSAGES_PER_FRAME
            || self
                .queued_message_bytes
                .saturating_add(self.pending_bytes)
                .saturating_add(estimated_size)
                > MAX_MESSAGE_BYTES_PER_FRAME
        {
            self.overflowed_messages = self.overflowed_messages.saturating_add(1);
            return Ok(());
        }
        let sender_id = self
            .object_id(sender_handle)
            .ok_or(QueueMessageError::InvalidValue)?
            .to_owned();
        self.pending_bytes = self.pending_bytes.saturating_add(estimated_size);
        #[cfg(test)]
        {
            self.peak_pending_bytes = self.peak_pending_bytes.max(self.pending_bytes);
        }
        self.pending_messages.push(PendingMessage {
            sender_id,
            target,
            name: name.to_owned(),
            value,
            estimated_size,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit(
        &mut self,
        world: &mut World,
        queued_messages: &mut Vec<PendingMessage>,
        queued_bytes: &mut usize,
        events: &EventQueue,
        audio: &mut Vec<AudioRequest>,
        animation: &mut Vec<AnimationRequest>,
        net: &mut Vec<NetRequest>,
        rumble: &mut Vec<RumbleRequest>,
        particle_commands: &mut Vec<ParticleCommand>,
    ) -> Result<Vec<ActionMutation>, String> {
        let transforms_are_finite = self.pending_transforms.values().all(|transform| {
            transform
                .position
                .to_array()
                .into_iter()
                .chain(transform.rotation_deg.to_array())
                .chain(transform.scale.to_array())
                .all(f32::is_finite)
        });
        let handles_exist = self
            .pending_transforms
            .keys()
            .all(|handle| world.local_transform(*handle).is_some());
        let camera_settings_target_valid = self.pending_camera_settings.keys().all(|handle| {
            world
                .object(*handle)
                .is_some_and(|object| object.camera.is_some())
        });
        let active_camera_target_valid = self.pending_active_camera.is_none_or(|handle| {
            world
                .object(handle)
                .is_some_and(|object| object.camera.is_some())
        });
        if !transforms_are_finite {
            self.discard();
            return Err("script produced a non-finite transform".to_owned());
        }
        if self
            .pending_physics_commands
            .iter()
            .any(|command| !physics_command_is_finite(*command))
        {
            self.discard();
            return Err("script produced a non-finite physics command".to_owned());
        }
        if !handles_exist {
            self.discard();
            return Err("script wrote a transform for an unknown object".to_owned());
        }
        let script_targets_alive = {
            let refs = self.script_refs.borrow();
            self.pending_properties
                .iter()
                .all(|write| find_entry(&refs, write.serial).is_some())
                && self
                    .pending_remote_enabled
                    .iter()
                    .all(|(serial, _)| find_entry(&refs, *serial).is_some())
        };
        if !script_targets_alive {
            self.discard();
            return Err(ScriptRefError::Dead.message());
        }
        if let Some(error) = self
            .pending_camera_settings
            .values()
            .find_map(|settings| settings.validate().err())
        {
            self.discard();
            return Err(format!("script produced invalid camera settings: {error}"));
        }
        if !camera_settings_target_valid {
            self.discard();
            return Err("script wrote camera settings for an object without a camera".to_owned());
        }
        if self.pending_screen_shaders.keys().any(|(handle, _)| {
            world
                .object(*handle)
                .is_none_or(|object| object.camera.is_none())
        }) {
            self.discard();
            return Err("script wrote screen shaders for an object without a camera".to_owned());
        }
        if let Some(error) = self
            .pending_post_process
            .values()
            .find_map(|settings| settings.validate().err())
        {
            self.discard();
            return Err(format!(
                "script produced invalid post process settings: {error}"
            ));
        }
        let post_process_target_valid = self.pending_post_process.keys().all(|handle| {
            world
                .object(*handle)
                .is_some_and(|object| object.camera.is_some())
        });
        if !post_process_target_valid {
            self.discard();
            return Err(
                "script wrote post process settings for an object without a camera".to_owned(),
            );
        }
        if !active_camera_target_valid {
            self.discard();
            return Err("script selected an object without a camera as active".to_owned());
        }
        if let Err(message) = self.component_writes_fit(world) {
            self.discard();
            return Err(message);
        }
        if let Err(message) = self.probe_edits(world) {
            self.discard();
            return Err(message);
        }
        if !self.timeline.borrow().can_add_timer(&self.pending_timeline)
            || !self
                .timeline
                .borrow()
                .coroutines_fit(&self.pending_timeline)
        {
            self.discard();
            return Err("script timer or coroutine limit reached".to_owned());
        }

        let mut physics_commands = std::mem::take(&mut self.pending_physics_commands);
        if let Some(dt) = self
            .physics_phase_dt
            .filter(|dt| *dt >= 1.0e-6 && dt.is_finite())
        {
            for pending in self.pending_pushes.values() {
                let mut hit = pending.hit;
                hit.normal = pending.normal_sum.normalize_or_zero();
                if let Some(impulse) = pending.controller.push_impulse(&hit, dt) {
                    physics_commands.push(PhysicsCommand::AddImpulse {
                        body: hit.body,
                        impulse,
                        point: None,
                    });
                }
            }
        }
        if physics_commands
            .iter()
            .any(|command| !physics_command_is_finite(*command))
        {
            self.discard();
            return Err("script produced a non-finite physics command".to_owned());
        }

        // Запись мира идёт до lifecycle: при сбое discard освободит резервации spawn.
        if let Err(message) = self.apply_world_writes(world) {
            self.discard();
            return Err(message);
        }
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_post_process.clear();
        self.pending_screen_shaders.clear();
        self.pending_active_camera = None;
        self.pending_edits.clear();
        particle_commands.append(&mut self.component_writes.particle_commands);
        self.component_writes.clear();

        if let Some(visible) = self.pending_cursor_visible.take() {
            self.cursor.visible = visible;
        }
        if let Some(locked) = self.pending_cursor_locked.take() {
            self.cursor.locked = locked;
        }
        self.commit_ui();
        self.commit_lifecycle(events);
        self.commit_script_refs();

        self.physics
            .borrow_mut()
            .apply_binding_updates(std::mem::take(&mut self.pending_bindings));
        self.physics.borrow_mut().commit(
            physics_commands,
            std::mem::take(&mut self.pending_grounded),
            std::mem::take(&mut self.pending_collider_enabled),
            std::mem::take(&mut self.pending_vertical),
        );
        self.pending_colliders.clear();
        self.pending_rigid_bodies.clear();
        self.pending_controllers.clear();

        if !self.pending_messages.is_empty() || self.overflowed_messages != 0 {
            let mut dropped = self.overflowed_messages;
            for message in self.pending_messages.drain(..) {
                if queued_messages.len() < MAX_MESSAGES_PER_FRAME
                    && queued_bytes.saturating_add(message.estimated_size)
                        <= MAX_MESSAGE_BYTES_PER_FRAME
                {
                    *queued_bytes = queued_bytes.saturating_add(message.estimated_size);
                    queued_messages.push(message);
                } else {
                    dropped = dropped.saturating_add(1);
                }
            }
            if dropped > 0 {
                events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: self.source.clone(),
                    message: format!(
                        "message queue limits ({MAX_MESSAGES_PER_FRAME} messages, {MAX_MESSAGE_BYTES_PER_FRAME} estimated bytes per frame) reached; dropped {dropped} message(s)"
                    ),
                });
            }
        }
        for message in self.pending_warnings.drain(..) {
            events.borrow_mut().push(ScriptEvent {
                level: ScriptEventLevel::Warn,
                source: self.source.clone(),
                message,
            });
        }
        self.pending_bytes = 0;
        self.overflowed_messages = 0;
        if let Some(scale) = self.pending_time_scale.take() {
            self.time_scale.set(scale);
        }
        self.timeline.borrow_mut().apply(&mut self.pending_timeline);
        if !self.pending_save.is_empty() {
            let mut values = self.save_values.borrow_mut();
            for (key, value) in std::mem::take(&mut self.pending_save) {
                match value {
                    Some(value) => {
                        values.insert(key, value);
                    }
                    None => {
                        values.remove(&key);
                    }
                }
            }
            self.save_generation
                .set(self.save_generation.get().wrapping_add(1));
        }
        if std::mem::take(&mut self.pending_save_flush) {
            self.save_flush_requested.set(true);
        }
        for request in &self.pending_audio {
            match request {
                AudioRequest::PlayClip { voice, params, .. } => {
                    self.voice_spatial
                        .insert(voice.get(), engine_scene::audio::clip_is_spatial(params));
                }
                AudioRequest::StopVoice { voice } => {
                    self.voice_spatial.remove(&voice.get());
                }
                _ => {}
            }
        }
        while self.voice_spatial.len() > MAX_TRACKED_VOICES {
            self.voice_spatial.pop_first();
        }
        audio.append(&mut self.pending_audio);
        animation.append(&mut self.pending_animation);
        net.append(&mut self.pending_net);
        rumble.append(&mut self.pending_rumble);
        self.world = std::ptr::null();
        self.action_frame = None;
        Ok(std::mem::take(&mut self.pending_action_mutations))
    }

    /// Коммит заявок spawn, destroy, загрузки и предупреждений. Вызывается после всех проверок.
    fn commit_lifecycle(&mut self, events: &EventQueue) {
        {
            let mut lifecycle = self.lifecycle.borrow_mut();
            for mut request in self.pending_spawns.drain(..) {
                request.seq = lifecycle.next_seq();
                lifecycle.queued.push_back(request);
            }
            for (id, handle) in self.pending_ids.drain() {
                lifecycle.pending_ids.insert(id, handle);
            }
            lifecycle.destroys.append(&mut self.pending_destroys);
            for request in self.pending_prefab_requests.drain(..) {
                lifecycle.apply_prefab_request(request);
            }
            if let Some(enabled) = self.pending_script_enabled.take() {
                lifecycle
                    .enabled_writes
                    .push((self.instance_serial, enabled));
            }
        }
        for (handle, message) in self.pending_dead_warnings.drain(..) {
            if self
                .lifecycle
                .borrow_mut()
                .mark_warned(self.instance_serial, handle)
            {
                events.borrow_mut().push(ScriptEvent {
                    level: ScriptEventLevel::Warn,
                    source: self.source.clone(),
                    message,
                });
            }
        }
    }

    /// Применяет записи свойств и заявки `enabled` чужих экземпляров. Цели проверены до коммита.
    fn commit_script_refs(&mut self) {
        if let Some(path) = self.pending_scene_request.take() {
            self.scene_state.borrow_mut().request = Some(path);
        }
        let writes = std::mem::take(&mut self.pending_properties);
        {
            let mut refs = self.script_refs.borrow_mut();
            for write in &writes {
                if let Some(entry) = refs.iter_mut().find(|entry| entry.serial == write.serial) {
                    entry
                        .properties
                        .insert(write.name.clone(), write.value.clone());
                    entry.dirty_properties.insert(write.name.clone());
                }
            }
        }
        let mut lifecycle = self.lifecycle.borrow_mut();
        lifecycle
            .enabled_writes
            .append(&mut self.pending_remote_enabled);
    }

    pub(crate) fn discard(&mut self) {
        self.discard_ui();
        self.pending_properties.clear();
        self.pending_remote_enabled.clear();
        self.pending_scene_request = None;
        if !self.world.is_null() {
            // SAFETY: `world` is the live reference from `begin`, cleared only below; releasing a
            // reservation needs just a shared borrow and touches no other state of the world.
            let world = unsafe { &*self.world };
            for request in self.pending_spawns.drain(..) {
                world.release_reservation(request.root);
            }
        }
        self.pending_spawns.clear();
        self.pending_destroys.clear();
        self.pending_prefab_requests.clear();
        self.pending_ids.clear();
        self.pending_dead_warnings.clear();
        self.pending_edits.clear();
        self.pending_script_enabled = None;
        self.pending_save.clear();
        self.pending_save_flush = false;
        self.pending_timeline.clear();
        self.pending_time_scale = None;
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_post_process.clear();
        self.pending_screen_shaders.clear();
        self.pending_active_camera = None;
        self.pending_messages.clear();
        self.pending_warnings.clear();
        self.pending_physics_commands.clear();
        self.pending_grounded.clear();
        self.pending_collider_enabled.clear();
        self.pending_pushes.clear();
        self.clear_component_writes();
        self.component_writes.clear();
        self.pending_bytes = 0;
        self.queued_message_count = 0;
        self.queued_message_bytes = 0;
        self.overflowed_messages = 0;
        self.pending_action_mutations.clear();
        self.pending_cursor_visible = None;
        self.pending_cursor_locked = None;
        self.pending_audio.clear();
        self.pending_rumble.clear();
        self.pending_animation.clear();
        self.pending_net.clear();
        self.world = std::ptr::null();
        self.action_frame = None;
    }

    #[cfg(test)]
    pub(crate) fn peak_pending_bytes(&self) -> usize {
        self.peak_pending_bytes
    }
}

fn merge_enabled_overrides(
    overrides: impl IntoIterator<Item = (BodyHandle, bool)>,
) -> Vec<(BodyHandle, bool)> {
    let mut merged = std::collections::BTreeMap::new();
    for (body, enabled) in overrides {
        merged.insert(body, enabled);
    }
    merged.into_iter().collect()
}

/// Снимок, фильтр и исключения тел, которые нужны любому запросу физики из скрипта.
type QueryParts = (
    std::sync::Arc<engine_physics::QuerySnapshot>,
    QueryFilter,
    Vec<BodyHandle>,
    Vec<(BodyHandle, bool)>,
);

/// Фильтр запросов физики из скрипта: слои, триггеры и объекты-исключения.
#[derive(Default)]
pub(crate) struct ScriptQueryFilter {
    pub(crate) layer_mask: u32,
    pub(crate) include_triggers: bool,
    pub(crate) ignore: Vec<ObjectHandle>,
}

fn physics_command_is_finite(command: PhysicsCommand) -> bool {
    match command {
        PhysicsCommand::SetVelocity { velocity, .. } => velocity.is_finite(),
        PhysicsCommand::AddImpulse { impulse, point, .. } => {
            impulse.is_finite() && point.is_none_or(Vec3::is_finite)
        }
        PhysicsCommand::AddForce { force, point, .. } => {
            force.is_finite() && point.is_none_or(Vec3::is_finite)
        }
        PhysicsCommand::Teleport { pose, .. } | PhysicsCommand::KinematicTarget { pose, .. } => {
            pose.position.is_finite() && pose.rotation.is_finite()
        }
        PhysicsCommand::SetColliderEnabled { .. }
        | PhysicsCommand::SetTrigger { .. }
        | PhysicsCommand::SetLayer { .. } => true,
        PhysicsCommand::SetCollider { offset, .. } => offset.is_finite(),
        PhysicsCommand::SetBodyProperties {
            mass,
            linear_damping,
            angular_damping,
            gravity_scale,
            ..
        } => [mass, linear_damping, angular_damping, gravity_scale]
            .into_iter()
            .all(f32::is_finite),
    }
}

fn physics_kind(kind: RigidBodyKind) -> BodyKind {
    match kind {
        RigidBodyKind::Static => BodyKind::Static,
        RigidBodyKind::Kinematic => BodyKind::Kinematic,
        RigidBodyKind::Dynamic => BodyKind::Dynamic,
    }
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

/// Идентичность заявки до её применения: явный id (если задан) и имя (если задано).
fn spawn_identity(request: &SpawnRequest) -> (Option<String>, String) {
    (
        request.args.options.id.clone(),
        request.args.options.name.clone().unwrap_or_default(),
    )
}

fn particle_object(command: &ParticleCommand) -> &ObjectHandle {
    match command {
        ParticleCommand::Restart { object } | ParticleCommand::Burst { object, .. } => object,
    }
}

/// Компонентные заявки скриптов. Чтения видят заявки текущего вызова, затем мир; записи
/// проверяются здесь же, чтобы ошибка значения давала ошибку вызова, а не падение кадра.
impl ScriptTransaction {
    fn stored_object(&self, handle: ObjectHandle) -> Option<&engine_scene::world::WorldObject> {
        self.world().and_then(|world| world.object(handle))
    }

    pub(crate) fn directional_light(
        &self,
        handle: ObjectHandle,
    ) -> Option<DirectionalLightComponent> {
        self.component_writes
            .directional_lights
            .get(&handle)
            .copied()
            .or_else(|| self.stored_object(handle)?.directional_light)
    }

    pub(crate) fn point_light(&self, handle: ObjectHandle) -> Option<PointLightComponent> {
        self.component_writes
            .point_lights
            .get(&handle)
            .copied()
            .or_else(|| self.stored_object(handle)?.point_light)
    }

    pub(crate) fn spot_light(&self, handle: ObjectHandle) -> Option<SpotLightComponent> {
        self.component_writes
            .spot_lights
            .get(&handle)
            .copied()
            .or_else(|| self.stored_object(handle)?.spot_light)
    }

    /// Пользовательский флаг света объекта; `None`, если у объекта нет света.
    pub(crate) fn light_enabled(&self, handle: ObjectHandle) -> Option<bool> {
        let object = self.stored_object(handle)?;
        let has_light = object.directional_light.is_some()
            || object.point_light.is_some()
            || object.spot_light.is_some();
        has_light.then(|| {
            self.component_writes
                .light_enabled
                .get(&handle)
                .copied()
                .unwrap_or(object.light_enabled)
        })
    }

    pub(crate) fn mesh_visible(&self, handle: ObjectHandle) -> Option<bool> {
        let object = self.stored_object(handle)?;
        object.mesh_renderer.as_ref()?;
        Some(
            self.component_writes
                .mesh_visible
                .get(&handle)
                .copied()
                .unwrap_or(object.mesh_visible),
        )
    }

    pub(crate) fn particles_playing(&self, handle: ObjectHandle) -> Option<bool> {
        let object = self.stored_object(handle)?;
        object.particle_emitter.as_ref()?;
        Some(
            self.component_writes
                .particles_playing
                .get(&handle)
                .copied()
                .unwrap_or(object.particles_playing),
        )
    }

    /// Интенсивность и процедурное небо окружения (с учётом заявок); `None` без `environment`.
    pub(crate) fn environment_state(
        &self,
        handle: ObjectHandle,
    ) -> Option<(f32, Option<ProceduralSkyComponent>)> {
        let environment = self.stored_object(handle)?.environment.as_ref()?;
        let intensity = self
            .component_writes
            .environment_intensity
            .get(&handle)
            .copied()
            .unwrap_or(environment.intensity);
        let sky = self
            .component_writes
            .sky
            .get(&handle)
            .copied()
            .or(environment.procedural);
        Some((intensity, sky))
    }

    /// Дистанции уровней LOD и дистанция отсечения (с учётом заявок); `None` без `lod_group`.
    pub(crate) fn lod_distances(&self, handle: ObjectHandle) -> Option<(Vec<f32>, Option<f32>)> {
        let group = self.stored_object(handle)?.lod_group.as_ref()?;
        Some(match self.component_writes.lod.get(&handle) {
            Some((distances, cull)) => (distances.clone(), *cull),
            None => (
                group.levels.iter().map(|level| level.distance).collect(),
                group.cull_distance,
            ),
        })
    }

    /// Параметры звукового источника с учётом заявок; `None` без `audio_source`.
    pub(crate) fn audio_settings(&self, handle: ObjectHandle) -> Option<AudioSourceComponent> {
        self.component_writes
            .audio_sources
            .get(&handle)
            .cloned()
            .or_else(|| self.stored_object(handle)?.audio_source.clone())
    }

    pub(crate) fn stage_directional_light(
        &mut self,
        handle: ObjectHandle,
        light: DirectionalLightComponent,
    ) -> Result<(), String> {
        self.require_writable("light changes")?;
        if self.directional_light(handle).is_none() {
            return Err("object has no directional_light".to_owned());
        }
        if let Some(problem) = engine_scene::world::directional_light_problem(&light) {
            return Err(problem.to_owned());
        }
        self.component_writes
            .directional_lights
            .insert(handle, light);
        Ok(())
    }

    pub(crate) fn stage_point_light(
        &mut self,
        handle: ObjectHandle,
        light: PointLightComponent,
    ) -> Result<(), String> {
        self.require_writable("light changes")?;
        if self.point_light(handle).is_none() {
            return Err("object has no point_light".to_owned());
        }
        if let Some(problem) = engine_scene::world::point_light_problem(&light) {
            return Err(problem.to_owned());
        }
        self.component_writes.point_lights.insert(handle, light);
        Ok(())
    }

    pub(crate) fn stage_spot_light(
        &mut self,
        handle: ObjectHandle,
        light: SpotLightComponent,
    ) -> Result<(), String> {
        self.require_writable("light changes")?;
        if self.spot_light(handle).is_none() {
            return Err("object has no spot_light".to_owned());
        }
        if let Some(problem) = engine_scene::world::spot_light_problem(&light) {
            return Err(problem.to_owned());
        }
        self.component_writes.spot_lights.insert(handle, light);
        Ok(())
    }

    pub(crate) fn stage_light_enabled(
        &mut self,
        handle: ObjectHandle,
        enabled: bool,
    ) -> Result<(), String> {
        self.require_writable("light changes")?;
        if self.light_enabled(handle).is_none() {
            return Err("object has no light".to_owned());
        }
        self.component_writes.light_enabled.insert(handle, enabled);
        Ok(())
    }

    pub(crate) fn stage_mesh_visible(
        &mut self,
        handle: ObjectHandle,
        visible: bool,
    ) -> Result<(), String> {
        self.require_writable("mesh visibility")?;
        if self.mesh_visible(handle).is_none() {
            return Err("object has no mesh_renderer".to_owned());
        }
        self.component_writes.mesh_visible.insert(handle, visible);
        Ok(())
    }

    /// Объединяет параметры материала с заявками этого вызова: заданные поля перекрывают прежние.
    pub(crate) fn stage_mesh_material(
        &mut self,
        handle: ObjectHandle,
        params: MeshMaterialParams,
    ) -> Result<(), String> {
        self.require_writable("material changes")?;
        if self.mesh_visible(handle).is_none() {
            return Err("object has no mesh_renderer".to_owned());
        }
        if let Some(problem) = engine_scene::world::mesh_material_problem(&params) {
            return Err(problem.to_owned());
        }
        let merged = self
            .component_writes
            .mesh_material
            .entry(handle)
            .or_default();
        if params.base_color.is_some() {
            merged.base_color = params.base_color;
        }
        if params.emissive.is_some() {
            merged.emissive = params.emissive;
        }
        if params.metallic.is_some() {
            merged.metallic = params.metallic;
        }
        if params.roughness.is_some() {
            merged.roughness = params.roughness;
        }
        Ok(())
    }

    /// Ставит материал всех частей меш-рендерера по пути `.rvemat`. Путь проверяется при вызове:
    /// он должен быть объявлен в `[[assets]]` сцены.
    pub(crate) fn stage_mesh_material_path(
        &mut self,
        handle: ObjectHandle,
        path: engine_files::VirtualPath,
    ) -> Result<(), String> {
        self.require_writable("material changes")?;
        if self.mesh_visible(handle).is_none() {
            return Err("object has no mesh_renderer".to_owned());
        }
        let world = self.world().ok_or("material changes need a live world")?;
        if let Some(problem) = world.mesh_material_path_problem_for(handle, &path) {
            return Err(problem);
        }
        self.component_writes
            .mesh_material_path
            .insert(handle, path);
        Ok(())
    }

    pub(crate) fn stage_particles_playing(
        &mut self,
        handle: ObjectHandle,
        playing: bool,
    ) -> Result<(), String> {
        self.require_writable("particle changes")?;
        if self.particles_playing(handle).is_none() {
            return Err("object has no particle_emitter".to_owned());
        }
        self.component_writes
            .particles_playing
            .insert(handle, playing);
        Ok(())
    }

    /// Ставит команду эмиттеру в очередь хоста: перезапуск или выброс.
    pub(crate) fn stage_particle_command(
        &mut self,
        command: ParticleCommand,
    ) -> Result<(), String> {
        self.require_writable("particle changes")?;
        if self.component_writes.particle_commands.len() >= MAX_PARTICLE_COMMANDS_PER_CALL {
            return Err(format!(
                "too many particle commands in one call (limit {MAX_PARTICLE_COMMANDS_PER_CALL})"
            ));
        }
        let world = self.world().ok_or("particle changes need a live world")?;
        world
            .particle_command_target(command)
            .map_err(|error| error.to_string())?;
        // Команда неактивному эмиттеру не применяется ни в runtime, ни в редакторе: отказ здесь
        // делает поведение одинаковым для обоих потребителей.
        let object = match command {
            ParticleCommand::Restart { object } | ParticleCommand::Burst { object, .. } => object,
        };
        if self.active_in_hierarchy(object) != Some(true) {
            return Err("particle commands need an active object".to_owned());
        }
        self.component_writes.particle_commands.push(command);
        Ok(())
    }

    pub(crate) fn stage_environment_intensity(
        &mut self,
        handle: ObjectHandle,
        intensity: f32,
    ) -> Result<(), String> {
        self.require_writable("environment changes")?;
        if self.environment_state(handle).is_none() {
            return Err("object has no environment".to_owned());
        }
        if let Some(problem) = engine_scene::world::environment_intensity_problem(intensity) {
            return Err(problem.to_owned());
        }
        self.component_writes
            .environment_intensity
            .insert(handle, intensity);
        Ok(())
    }

    pub(crate) fn stage_procedural_sky(
        &mut self,
        handle: ObjectHandle,
        sky: ProceduralSkyComponent,
    ) -> Result<(), String> {
        self.require_writable("environment changes")?;
        match self.environment_state(handle) {
            None => return Err("object has no environment".to_owned()),
            Some((_, None)) => return Err("environment uses HDR, not a procedural sky".to_owned()),
            Some(_) => {}
        }
        if let Some(problem) = engine_scene::world::procedural_sky_problem(&sky) {
            return Err(problem.to_owned());
        }
        self.component_writes.sky.insert(handle, sky);
        Ok(())
    }

    /// Интенсивность и небо одним вызовом: все значения проверяются до первой записи, поэтому
    /// отказ не оставляет частично применённого окружения.
    pub(crate) fn stage_environment(
        &mut self,
        handle: ObjectHandle,
        intensity: Option<f32>,
        sky: Option<ProceduralSkyComponent>,
    ) -> Result<(), String> {
        self.require_writable("environment changes")?;
        match self.environment_state(handle) {
            None => return Err("object has no environment".to_owned()),
            Some((_, None)) if sky.is_some() => {
                return Err("environment uses HDR, not a procedural sky".to_owned());
            }
            Some(_) => {}
        }
        if let Some(problem) =
            intensity.and_then(engine_scene::world::environment_intensity_problem)
        {
            return Err(problem.to_owned());
        }
        if let Some(problem) = sky
            .as_ref()
            .and_then(engine_scene::world::procedural_sky_problem)
        {
            return Err(problem.to_owned());
        }
        if let Some(intensity) = intensity {
            self.stage_environment_intensity(handle, intensity)?;
        }
        if let Some(sky) = sky {
            self.stage_procedural_sky(handle, sky)?;
        }
        Ok(())
    }

    pub(crate) fn stage_lod_distances(
        &mut self,
        handle: ObjectHandle,
        distances: Vec<f32>,
        cull: Option<f32>,
    ) -> Result<(), String> {
        self.require_writable("LOD changes")?;
        let Some((levels, _)) = self.lod_distances(handle) else {
            return Err("object has no lod_group".to_owned());
        };
        if levels.len() != distances.len() {
            return Err(format!(
                "lod_group has {} levels, but {} distances were given",
                levels.len(),
                distances.len()
            ));
        }
        if let Some(problem) = engine_scene::world::lod_distances_problem(&distances, cull) {
            return Err(problem.to_owned());
        }
        self.component_writes.lod.insert(handle, (distances, cull));
        Ok(())
    }

    pub(crate) fn stage_audio_settings(
        &mut self,
        handle: ObjectHandle,
        settings: AudioSourceComponent,
    ) -> Result<(), String> {
        self.require_writable("audio source changes")?;
        let Some(current) = self.audio_settings(handle) else {
            return Err("object has no audio_source".to_owned());
        };
        let settings = AudioSourceComponent {
            clip: current.clip,
            ..settings
        };
        if let Some(problem) = engine_scene::audio::audio_source_problem(&settings) {
            return Err(problem.to_owned());
        }
        self.component_writes.audio_sources.insert(handle, settings);
        Ok(())
    }
}
