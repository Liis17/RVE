use engine_physics::{
    BodyHandle, PhysicsCommand, PhysicsEvent, PhysicsSceneBinding, QuerySnapshot,
};
use engine_scene::ObjectHandle;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct PhysicsScriptState {
    snapshot: Option<Arc<QuerySnapshot>>,
    bindings: HashMap<ObjectHandle, PhysicsSceneBinding>,
    objects: HashMap<BodyHandle, ObjectHandle>,
    grounded: HashMap<ObjectHandle, bool>,
    enabled_overrides: HashMap<BodyHandle, bool>,
    commands: Vec<PhysicsCommand>,
    events: Vec<PhysicsEvent>,
}

impl PhysicsScriptState {
    pub(crate) fn is_attached(&self) -> bool {
        self.snapshot.is_some()
    }

    pub(crate) fn set_snapshot(
        &mut self,
        snapshot: Arc<QuerySnapshot>,
        bindings: &[PhysicsSceneBinding],
    ) {
        self.enabled_overrides.retain(|body, enabled| {
            snapshot
                .body_state(*body)
                .is_some_and(|state| state.enabled != *enabled)
        });
        self.bindings.clear();
        self.objects.clear();
        for binding in bindings {
            self.bindings.insert(binding.object, *binding);
            self.objects.insert(binding.body, binding.object);
        }
        self.grounded
            .retain(|object, _| self.bindings.contains_key(object));
        self.snapshot = Some(snapshot);
    }

    pub(crate) fn snapshot(&self) -> Result<&QuerySnapshot, String> {
        self.snapshot
            .as_deref()
            .ok_or_else(|| "physics context is not attached".to_owned())
    }

    pub(crate) fn binding(&self, object: ObjectHandle) -> Result<PhysicsSceneBinding, String> {
        self.snapshot()?;
        self.bindings
            .get(&object)
            .copied()
            .ok_or_else(|| "object has no physics body".to_owned())
    }

    pub(crate) fn optional_binding(&self, object: ObjectHandle) -> Option<PhysicsSceneBinding> {
        self.bindings.get(&object).copied()
    }

    pub(crate) fn object(&self, body: BodyHandle) -> Option<ObjectHandle> {
        self.objects.get(&body).copied()
    }

    pub(crate) fn grounded(&self, object: ObjectHandle) -> bool {
        self.grounded.get(&object).copied().unwrap_or(false)
    }

    pub(crate) fn collider_enabled(&self, body: BodyHandle) -> Result<bool, String> {
        if let Some(enabled) = self.enabled_overrides.get(&body) {
            return Ok(*enabled);
        }
        self.snapshot()?
            .body_state(body)
            .map(|state| state.enabled)
            .ok_or_else(|| "physics body is missing from the query snapshot".to_owned())
    }

    pub(crate) fn enabled_overrides(&self) -> Vec<(BodyHandle, bool)> {
        let mut overrides = self
            .enabled_overrides
            .iter()
            .map(|(body, enabled)| (*body, *enabled))
            .collect::<Vec<_>>();
        overrides.sort_unstable_by_key(|(body, _)| *body);
        overrides
    }

    pub(crate) fn drain_commands(&mut self) -> Vec<PhysicsCommand> {
        std::mem::take(&mut self.commands)
    }

    pub(crate) fn queue_events(&mut self, events: impl IntoIterator<Item = PhysicsEvent>) {
        self.events.extend(events);
    }

    pub(crate) fn take_events(&mut self) -> Vec<PhysicsEvent> {
        std::mem::take(&mut self.events)
    }

    pub(crate) fn commit(
        &mut self,
        commands: impl IntoIterator<Item = PhysicsCommand>,
        grounded: impl IntoIterator<Item = (ObjectHandle, bool)>,
        enabled: impl IntoIterator<Item = (BodyHandle, bool)>,
    ) {
        self.commands.extend(commands);
        self.grounded.extend(grounded);
        self.enabled_overrides.extend(enabled);
    }
}
