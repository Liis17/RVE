//! Шаг физики из потока скриптов и тела, создаваемые при спавне.

use super::*;

pub(crate) fn apply_physics_step(
    scripts: &mut ScriptHost,
    world: &mut World,
    sync: &mut PhysicsSceneSync,
    result: PhysicsStepResult,
) -> Result<(), String> {
    if result.skipped_commands > 0 {
        log::warn!(
            "[physics] отклонено команд коллайдера или тела: {}",
            result.skipped_commands
        );
    }
    sync.apply_physics_poses(world, &result.poses)
        .map_err(|error| error.to_string())?;
    scripts.set_physics_snapshot(result.snapshot, sync.bindings());
    scripts.queue_physics_events(result.events);
    Ok(())
}

pub(crate) enum PhysicsWait {
    Idle,
    Step(PhysicsStepResult),
    Shutdown,
}

pub(crate) fn receive_physics_interruptibly(
    client: &mut PhysicsClient,
    control_rx: &Receiver<ScriptControl>,
    mut handle_control: impl FnMut(ScriptControl) -> bool,
) -> Result<PhysicsWait, String> {
    loop {
        match client
            .receive(control_rx)
            .map_err(|error| error.to_string())?
        {
            None => return Ok(PhysicsWait::Idle),
            Some(PhysicsReceive::Step(result)) => return Ok(PhysicsWait::Step(result)),
            Some(PhysicsReceive::Interrupted(control)) => {
                if handle_control(control) {
                    return Ok(PhysicsWait::Shutdown);
                }
            }
        }
    }
}

/// Состояние физики spawn-объектов, которое копится до ближайшего шага физики.
#[derive(Default)]
pub(crate) struct SpawnPhysics {
    /// Формы мешей, построенные для spawn-объектов; переиспользуются между spawn одной модели.
    pub(crate) shapes: ShapeCache,
    /// Геометрия моделей для коллайдеров мешей.
    pub(crate) geometry: Vec<ModelGeometry>,
    /// Модели, добавленные тяжёлыми префабами, и число загруженных префабов на каждую. Модели
    /// сцены сюда не входят и не удаляются.
    pub(crate) heavy_users: HashMap<String, usize>,
    /// Вставки и удаления тел; уходят в `step_with_changes` ближайшего шага.
    pub(crate) changes: Vec<BodyChange>,
    /// Скорости, заданные при spawn; применяются сразу после вставки тела в том же шаге.
    pub(crate) velocities: Vec<PhysicsCommand>,
}

impl SpawnPhysics {
    /// Добавляет геометрию тяжёлого префаба. Путь, уже занятый сценой или другим префабом,
    /// не заменяется: содержимое одной модели одинаково.
    /// Геометрия моделей замены сцены. Без счётчика ссылок: её не выгружают тяжёлые префабы.
    /// Вставляет тела объектов, добавленных заменой сцены. Пробная сборка физики сцены проверила
    /// те же объекты до замены (`SceneLoader::prepare`), поэтому отказ здесь нарушает инвариант.
    pub(crate) fn insert_scene_bodies(
        &mut self,
        sync: &mut PhysicsSceneSync,
        world: &World,
        objects: &[ObjectHandle],
    ) -> Result<(), PhysicsSceneSyncError> {
        let inserted = sync.insert_objects(world, objects, &self.geometry, &mut self.shapes)?;
        self.changes.extend(inserted);
        Ok(())
    }

    pub(crate) fn add_scene_geometry(&mut self, models: Vec<ModelGeometry>) {
        for model in models {
            if !self.geometry.iter().any(|known| known.path == model.path) {
                self.geometry.push(model);
            }
        }
    }

    pub(crate) fn add_heavy_geometry(&mut self, models: Vec<ModelGeometry>) {
        for model in models {
            if let Some(users) = self.heavy_users.get_mut(&model.path) {
                *users += 1;
                continue;
            }
            if self.geometry.iter().any(|known| known.path == model.path) {
                continue;
            }
            self.heavy_users.insert(model.path.clone(), 1);
            self.geometry.push(model);
        }
    }

    /// Снимает ссылки выгруженного префаба. Модель уходит вместе с формами кэша, когда её больше
    /// никто не использует.
    pub(crate) fn remove_heavy_geometry(&mut self, paths: &[String]) {
        for path in paths {
            let Some(users) = self.heavy_users.get_mut(path) else {
                continue;
            };
            *users -= 1;
            if *users == 0 {
                self.heavy_users.remove(path);
                self.geometry.retain(|model| &model.path != path);
                self.shapes.remove_model(path);
            }
        }
    }
}

/// Тело корня spawn-объекта; `None`, если у корня нет коллайдера и тела не создано.
pub(crate) fn root_body(sync: &PhysicsSceneSync, root: ObjectHandle) -> Option<BodyHandle> {
    sync.bindings()
        .iter()
        .rev()
        .find(|binding| binding.object == root)
        .map(|binding| binding.body)
}
