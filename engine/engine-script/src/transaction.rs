use crate::actions::{ActionFrame, ActionMutation, ActionValue};
use crate::lifecycle::{
    DestroyRequest, Lifecycle, MAX_SPAWN_NAME_BYTES, MAX_SPAWNED_OBJECTS_PER_FRAME, ObjectState,
    PrefabKind, PrefabRequest, PrefabStatus, SpawnArgs, SpawnRequest, prefab_display, prefab_key,
    valid_spawn_id,
};
use crate::lite::EventQueue;
use crate::physics::PhysicsScriptState;
use crate::types::{ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptInput, ScriptTransform};
use engine_physics::{
    BodyHandle, BodyKind, CharacterHit, CharacterMoveResult, PhysicsCommand, Pose, QueryFilter,
    QueryHit, Ray,
};
use engine_scene::{AudioRequest, CameraSettings, LocalTransform, ObjectHandle, ParamValue, World};
use glam::{EulerRot, Quat, Vec3};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

pub(crate) const MAX_MESSAGES_PER_FRAME: usize = 4096;
/// Звуковых запросов за кадр, включая уже зафиксированные скриптами; сверх лимита скрипт получает ошибку.
pub(crate) const MAX_AUDIO_REQUESTS_PER_FRAME: usize = 256;
pub(crate) const MAX_MESSAGE_BYTES_PER_FRAME: usize = 8 * 1024 * 1024;
pub(crate) const MAX_MESSAGE_DEPTH: usize = 16;
pub(crate) const MAX_MESSAGE_ELEMENTS: usize = 1024;
pub(crate) const MAX_MESSAGE_STRING_BYTES: usize = 64 * 1024;
pub(crate) const MAX_MESSAGE_NAME_BYTES: usize = 256;
pub(crate) const MAX_MESSAGE_TARGET_ID_BYTES: usize = 256;

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
    Broadcast,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PendingMessage {
    pub(crate) sender_id: String,
    pub(crate) target: MessageTarget,
    pub(crate) name: String,
    pub(crate) value: ParamValue,
    estimated_size: usize,
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
    pub(crate) physics: Rc<RefCell<PhysicsScriptState>>,
    pub(crate) physics_phase_dt: Option<f32>,
    pub(crate) lifecycle: Rc<RefCell<Lifecycle>>,
    /// `false` в `create`, в `destroy` и на удалении поддерева: spawn и destroy запрещены.
    pub(crate) lifecycle_writes: bool,
    /// Серийный номер экземпляра, который выполняет вызов; нужен для предупреждений.
    pub(crate) instance_serial: u64,
    /// Состояние курсора, применённое хостом до этого вызова.
    pub(crate) cursor: ScriptCursor,
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
    self_object: Option<ObjectHandle>,
    writable: bool,
    source: String,
    pending_transforms: HashMap<ObjectHandle, LocalTransform>,
    pending_camera_settings: HashMap<ObjectHandle, CameraSettings>,
    pending_active_camera: Option<ObjectHandle>,
    pending_messages: Vec<PendingMessage>,
    pending_warnings: Vec<String>,
    pending_bytes: usize,
    queued_message_count: usize,
    queued_message_bytes: usize,
    queued_audio: usize,
    overflowed_messages: usize,
    action_frame: Option<Arc<ActionFrame>>,
    physics: Rc<RefCell<PhysicsScriptState>>,
    physics_phase_dt: Option<f32>,
    pending_physics_commands: Vec<PhysicsCommand>,
    pending_grounded: HashMap<ObjectHandle, bool>,
    pending_collider_enabled: HashMap<BodyHandle, bool>,
    pending_pushes: HashMap<BodyHandle, PendingPush>,
    pending_action_mutations: Vec<ActionMutation>,
    /// Курсор, применённый хостом; меняется только при успешном commit.
    cursor: ScriptCursor,
    pending_cursor_visible: Option<bool>,
    pending_cursor_locked: Option<bool>,
    pending_audio: Vec<AudioRequest>,
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
    #[cfg(test)]
    peak_pending_bytes: usize,
}

#[derive(Clone, Copy)]
struct PendingPush {
    controller: engine_physics::CharacterController,
    hit: CharacterHit,
    normal_sum: Vec3,
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
            pending_active_camera: None,
            pending_messages: Vec::with_capacity(16),
            pending_warnings: Vec::new(),
            pending_bytes: 0,
            queued_message_count: 0,
            queued_message_bytes: 0,
            queued_audio: 0,
            overflowed_messages: 0,
            action_frame: None,
            physics: Rc::new(RefCell::new(PhysicsScriptState::default())),
            physics_phase_dt: None,
            pending_physics_commands: Vec::new(),
            pending_grounded: HashMap::new(),
            pending_collider_enabled: HashMap::new(),
            pending_pushes: HashMap::new(),
            pending_action_mutations: Vec::with_capacity(2),
            cursor: ScriptCursor::default(),
            pending_cursor_visible: None,
            pending_cursor_locked: None,
            pending_audio: Vec::new(),
            lifecycle: Rc::new(RefCell::new(Lifecycle::default())),
            lifecycle_writes: false,
            instance_serial: 0,
            pending_spawns: Vec::new(),
            pending_destroys: Vec::new(),
            pending_prefab_requests: Vec::new(),
            pending_ids: HashMap::new(),
            pending_dead_warnings: Vec::new(),
            #[cfg(test)]
            peak_pending_bytes: 0,
        }
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
        self.source.clear();
        self.source.push_str(init.source);
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_active_camera = None;
        self.pending_messages.clear();
        self.pending_warnings.clear();
        self.pending_physics_commands.clear();
        self.pending_grounded.clear();
        self.pending_collider_enabled.clear();
        self.pending_pushes.clear();
        self.pending_bytes = 0;
        self.queued_message_count = init.queued_messages.0;
        self.queued_message_bytes = init.queued_messages.1;
        self.queued_audio = init.queued_audio;
        self.overflowed_messages = 0;
        self.pending_action_mutations.clear();
        self.cursor = init.cursor;
        self.pending_cursor_visible = None;
        self.pending_cursor_locked = None;
        self.pending_audio.clear();
        self.lifecycle = init.lifecycle;
        self.lifecycle_writes = init.lifecycle_writes;
        self.instance_serial = init.instance_serial;
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

    pub(crate) fn has_audio_source(&self, handle: ObjectHandle) -> bool {
        self.world()
            .is_some_and(|world| world.audio_source(handle).is_some())
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
        self.push_body_command(object, force, |body| PhysicsCommand::AddForce {
            body,
            force,
            point: None,
        })
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
        let controller = binding
            .character_controller
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
            let slot = lifecycle.prefabs.get(&prefab).ok_or_else(|| {
                format!(
                    "prefab {:?} is not in the project prefab list",
                    prefab_display(&prefab)
                )
            })?;
            if slot.status == PrefabStatus::Failed {
                return Err(format!(
                    "prefab {:?} failed to load",
                    prefab_display(&prefab)
                ));
            }
            if slot.kind == PrefabKind::Fast && slot.template.is_none() {
                return Err(format!(
                    "prefab {:?} is not registered",
                    prefab_display(&prefab)
                ));
            }
            if lifecycle.queued.len() + lifecycle.waiting.len() + self.pending_spawns.len()
                >= MAX_SPAWNED_OBJECTS_PER_FRAME
            {
                return Err(format!(
                    "spawn requests limit ({MAX_SPAWNED_OBJECTS_PER_FRAME} per frame) reached"
                ));
            }
        }
        let options = &args.options;
        if [options.position, options.rotation_deg, options.scale]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite())
            || args.velocity.is_some_and(|value| !value.is_finite())
        {
            return Err("spawn transform and velocity must be finite".to_owned());
        }
        if let Some(parent) = options.parent
            && !world.is_alive(parent)
        {
            return Err("spawn parent must be an existing object".to_owned());
        }
        if options
            .name
            .as_ref()
            .is_some_and(|name| name.len() > MAX_SPAWN_NAME_BYTES)
        {
            return Err(format!(
                "spawn name must be at most {MAX_SPAWN_NAME_BYTES} bytes"
            ));
        }
        if let Some(id) = &options.id {
            if !valid_spawn_id(id) {
                return Err(format!("spawn id {id:?} must match [A-Za-z0-9_-]{{1,64}}"));
            }
            if world.handle(id).is_some()
                || self.pending_ids.contains_key(id)
                || self.lifecycle.borrow().pending_ids.contains_key(id)
            {
                return Err(format!("object id {id:?} is already used"));
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
        if !self.writable
            || self
                .world()
                .and_then(|world| world.object(handle))
                .is_none_or(|object| object.camera.is_none())
        {
            return false;
        }
        self.pending_camera_settings.insert(handle, settings);
        true
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

    pub(crate) fn commit(
        &mut self,
        world: &mut World,
        queued_messages: &mut Vec<PendingMessage>,
        queued_bytes: &mut usize,
        events: &EventQueue,
        audio: &mut Vec<AudioRequest>,
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
        if !active_camera_target_valid {
            self.discard();
            return Err("script selected an object without a camera as active".to_owned());
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

        if let Some(visible) = self.pending_cursor_visible.take() {
            self.cursor.visible = visible;
        }
        if let Some(locked) = self.pending_cursor_locked.take() {
            self.cursor.locked = locked;
        }
        self.commit_lifecycle(events);
        for (handle, transform) in self.pending_transforms.drain() {
            // Handles and all transforms were validated before applying any write.
            world
                .set_local_transform(handle, transform)
                .expect("validated object handle remains present during commit");
        }
        for (handle, settings) in self.pending_camera_settings.drain() {
            world
                .set_camera_settings(handle, settings)
                .expect("validated camera settings target remains present during commit");
        }
        if let Some(handle) = self.pending_active_camera.take() {
            world
                .set_active_camera(handle)
                .expect("validated active camera target remains present during commit");
        }

        self.physics.borrow_mut().commit(
            physics_commands,
            std::mem::take(&mut self.pending_grounded),
            std::mem::take(&mut self.pending_collider_enabled),
        );

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
        audio.append(&mut self.pending_audio);
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

    pub(crate) fn discard(&mut self) {
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
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_active_camera = None;
        self.pending_messages.clear();
        self.pending_warnings.clear();
        self.pending_physics_commands.clear();
        self.pending_grounded.clear();
        self.pending_collider_enabled.clear();
        self.pending_pushes.clear();
        self.pending_bytes = 0;
        self.queued_message_count = 0;
        self.queued_message_bytes = 0;
        self.overflowed_messages = 0;
        self.pending_action_mutations.clear();
        self.pending_cursor_visible = None;
        self.pending_cursor_locked = None;
        self.pending_audio.clear();
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
        PhysicsCommand::SetColliderEnabled { .. } => true,
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
