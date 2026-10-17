use crate::lite::EventQueue;
use crate::types::{ScriptEvent, ScriptEventLevel, ScriptInput, ScriptTransform};
use engine_scene::{LocalTransform, ObjectHandle, ParamValue, World};
use std::collections::HashMap;

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
    pending_messages: Vec<PendingMessage>,
    pending_bytes: usize,
    queued_message_count: usize,
    queued_message_bytes: usize,
    overflowed_messages: usize,
    input: ScriptInput,
    #[cfg(test)]
    peak_pending_bytes: usize,
}

impl Default for ScriptTransaction {
    fn default() -> Self {
        Self {
            world: std::ptr::null(),
            self_object: None,
            writable: false,
            source: String::new(),
            pending_transforms: HashMap::with_capacity(8),
            pending_messages: Vec::with_capacity(16),
            pending_bytes: 0,
            queued_message_count: 0,
            queued_message_bytes: 0,
            overflowed_messages: 0,
            input: ScriptInput::default(),
            #[cfg(test)]
            peak_pending_bytes: 0,
        }
    }
}

impl ScriptTransaction {
    pub(crate) fn begin(
        &mut self,
        world: &World,
        self_object: ObjectHandle,
        source: &str,
        writable: bool,
        input: ScriptInput,
        queued_messages: (usize, usize),
    ) {
        self.world = world;
        self.self_object = Some(self_object);
        self.writable = writable;
        self.input = input;
        self.source.clear();
        self.source.push_str(source);
        self.pending_transforms.clear();
        self.pending_messages.clear();
        self.pending_bytes = 0;
        self.queued_message_count = queued_messages.0;
        self.queued_message_bytes = queued_messages.1;
        self.overflowed_messages = 0;
    }

    pub(crate) fn writable(&self) -> bool {
        self.writable
    }

    pub(crate) fn input(&self) -> &ScriptInput {
        &self.input
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
        if !transforms_are_finite {
            self.discard();
            return Err("script produced a non-finite transform".to_owned());
        }
        if !handles_exist {
            self.discard();
            return Err("script wrote a transform for an unknown object".to_owned());
        }

        for (handle, transform) in self.pending_transforms.drain() {
            // Handles and all transforms were validated before applying any write.
            world
                .set_local_transform(handle, transform)
                .expect("validated object handle remains present during commit");
        }

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
        self.pending_messages.clear();
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
