use crate::lite::EventQueue;
use crate::physics::PhysicsScriptState;
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptInput, ScriptTransform};
use engine_physics::{
    BodyHandle, BodyKind, CharacterHit, CharacterMoveResult, PhysicsCommand, Pose, QueryFilter,
    QueryHit, Ray,
};
use engine_scene::{CameraSettings, LocalTransform, ObjectHandle, ParamValue, World};
use glam::{EulerRot, Quat, Vec3};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub(crate) const MAX_MESSAGES_PER_FRAME: usize = 4096;
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
    pub(crate) self_object: ObjectHandle,
    pub(crate) source: &'a str,
    pub(crate) writable: bool,
    pub(crate) input: ScriptInput,
    pub(crate) queued_messages: (usize, usize),
    pub(crate) physics: Rc<RefCell<PhysicsScriptState>>,
    pub(crate) physics_phase_dt: Option<f32>,
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
    pending_bytes: usize,
    queued_message_count: usize,
    queued_message_bytes: usize,
    overflowed_messages: usize,
    input: ScriptInput,
    physics: Rc<RefCell<PhysicsScriptState>>,
    physics_phase_dt: Option<f32>,
    pending_physics_commands: Vec<PhysicsCommand>,
    pending_grounded: HashMap<ObjectHandle, bool>,
    pending_collider_enabled: HashMap<BodyHandle, bool>,
    pending_pushes: HashMap<BodyHandle, PendingPush>,
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
            pending_bytes: 0,
            queued_message_count: 0,
            queued_message_bytes: 0,
            overflowed_messages: 0,
            input: ScriptInput::default(),
            physics: Rc::new(RefCell::new(PhysicsScriptState::default())),
            physics_phase_dt: None,
            pending_physics_commands: Vec::new(),
            pending_grounded: HashMap::new(),
            pending_collider_enabled: HashMap::new(),
            pending_pushes: HashMap::new(),
            #[cfg(test)]
            peak_pending_bytes: 0,
        }
    }
}

impl ScriptTransaction {
    pub(crate) fn begin(&mut self, init: ScriptTransactionInit<'_>) {
        self.world = init.world;
        self.self_object = Some(init.self_object);
        self.writable = init.writable;
        self.input = init.input;
        self.physics = init.physics;
        self.physics_phase_dt = init.physics_phase_dt;
        self.source.clear();
        self.source.push_str(init.source);
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_active_camera = None;
        self.pending_messages.clear();
        self.pending_physics_commands.clear();
        self.pending_grounded.clear();
        self.pending_collider_enabled.clear();
        self.pending_pushes.clear();
        self.pending_bytes = 0;
        self.queued_message_count = init.queued_messages.0;
        self.queued_message_bytes = init.queued_messages.1;
        self.overflowed_messages = 0;
    }

    pub(crate) fn writable(&self) -> bool {
        self.writable
    }

    pub(crate) fn physics_attached(&self) -> bool {
        self.physics.borrow().is_attached()
    }

    pub(crate) fn input(&self) -> &ScriptInput {
        &self.input
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
        self.physics.borrow().object(body)
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

    pub(crate) fn object_from_abi_handle(&self, handle: u64) -> Option<ObjectHandle> {
        let index = usize::try_from(handle.checked_sub(1)?).ok()?;
        self.world()?
            .objects()
            .get(index)
            .map(|object| object.handle)
    }

    pub(crate) fn abi_handle(&self, handle: ObjectHandle) -> Option<u64> {
        u64::try_from(handle.index()).ok()?.checked_add(1)
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
            .handle_by_id(target_id)
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
    ) -> Result<(), String> {
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
        self.pending_bytes = 0;
        self.overflowed_messages = 0;
        self.world = std::ptr::null();
        Ok(())
    }

    pub(crate) fn discard(&mut self) {
        self.pending_transforms.clear();
        self.pending_camera_settings.clear();
        self.pending_active_camera = None;
        self.pending_messages.clear();
        self.pending_physics_commands.clear();
        self.pending_grounded.clear();
        self.pending_collider_enabled.clear();
        self.pending_pushes.clear();
        self.pending_bytes = 0;
        self.queued_message_count = 0;
        self.queued_message_bytes = 0;
        self.overflowed_messages = 0;
        self.world = std::ptr::null();
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
