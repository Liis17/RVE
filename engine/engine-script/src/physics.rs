use engine_physics::{
    BodyHandle, BodyKind, CharacterController, PhysicsCommand, PhysicsEvent, PhysicsSceneBinding,
    QuerySnapshot,
};
use engine_scene::{ColliderComponent, ColliderShape, ObjectHandle};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct PhysicsScriptState {
    snapshot: Option<Arc<QuerySnapshot>>,
    bindings: HashMap<ObjectHandle, PhysicsSceneBinding>,
    objects: HashMap<BodyHandle, ObjectHandle>,
    grounded: HashMap<ObjectHandle, bool>,
    /// Вертикальная скорость контроллера, которую ведут `simple_move` и `jump`.
    vertical: HashMap<ObjectHandle, f32>,
    enabled_overrides: HashMap<BodyHandle, bool>,
    degraded_reported: HashSet<ObjectHandle>,
    commands: Vec<PhysicsCommand>,
    events: Vec<PhysicsEvent>,
}

/// Изменения привязки объекта, зафиксированные вызовом: тип тела и параметры контроллера.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BindingUpdate {
    pub(crate) kind: Option<BodyKind>,
    pub(crate) controller: Option<CharacterController>,
}

impl PhysicsScriptState {
    pub(crate) fn is_attached(&self) -> bool {
        self.snapshot.is_some()
    }

    /// Returns true only the first time a character object reports a limited move in this session.
    pub(crate) fn first_degraded_move(&mut self, object: ObjectHandle) -> bool {
        self.degraded_reported.insert(object)
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
        self.vertical
            .retain(|object, _| self.bindings.contains_key(object));
        self.snapshot = Some(snapshot);
    }

    /// Вертикальная скорость контроллера; `0`, пока `simple_move` и `jump` её не задавали.
    pub(crate) fn vertical_velocity(&self, object: ObjectHandle) -> f32 {
        self.vertical.get(&object).copied().unwrap_or(0.0)
    }

    /// Применяет изменения привязок, зафиксированные вызовом: тип тела и параметры контроллера.
    pub(crate) fn apply_binding_updates(&mut self, updates: HashMap<ObjectHandle, BindingUpdate>) {
        for (object, update) in updates {
            if let Some(binding) = self.bindings.get_mut(&object) {
                if let Some(kind) = update.kind {
                    binding.kind = kind;
                }
                if let Some(controller) = update.controller {
                    binding.character_controller = Some(controller);
                }
            }
        }
    }

    pub(crate) fn snapshot(&self) -> Result<&QuerySnapshot, String> {
        self.snapshot
            .as_deref()
            .ok_or_else(|| "physics context is not attached".to_owned())
    }

    pub(crate) fn shared_snapshot(&self) -> Result<Arc<QuerySnapshot>, String> {
        self.snapshot
            .clone()
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
        vertical: impl IntoIterator<Item = (ObjectHandle, f32)>,
    ) {
        self.commands.extend(commands);
        self.grounded.extend(grounded);
        self.enabled_overrides.extend(enabled);
        self.vertical.extend(vertical);
    }
}

/// Значения коллайдера в форме скриптов: полный размер box, радиус и высота, как в ABI v16.
/// Общие для Lua и ABI, чтобы оба пути читали и писали компонент одинаково.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ColliderValues {
    pub(crate) shape: ColliderShape,
    pub(crate) size: [f32; 3],
    pub(crate) radius: f32,
    pub(crate) height: f32,
    pub(crate) offset: [f32; 3],
    pub(crate) trigger: bool,
}

impl ColliderValues {
    pub(crate) fn from_component(component: &ColliderComponent) -> Self {
        Self {
            shape: component.shape,
            size: component.half_extents.map(|extent| extent * 2.0),
            radius: component.radius,
            height: component.height,
            offset: component.offset,
            trigger: component.trigger,
        }
    }

    /// Компонент с этими значениями; материал текущего компонента сохраняется.
    pub(crate) fn apply_to(self, current: &ColliderComponent) -> ColliderComponent {
        ColliderComponent {
            shape: self.shape,
            half_extents: self.size.map(|size| size * 0.5),
            radius: self.radius,
            height: self.height,
            offset: self.offset,
            trigger: self.trigger,
            friction: current.friction,
            restitution: current.restitution,
            material: current.material.clone(),
            resolved_material: current.resolved_material,
        }
    }
}
