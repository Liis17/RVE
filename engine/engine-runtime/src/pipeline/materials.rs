//! Синхронизация замены материалов объектов с render-потоком и их освобождение.

use super::*;

/// Замены материалов меш-рендерера скриптом на scripts thread: заявки render thread, владение
/// материалами, созданными по ним, и ответы, ещё не применённые к `by_object`.
#[derive(Default)]
pub(crate) struct MaterialSync {
    pub(crate) owned: MaterialOwnership<MaterialId>,
    /// Объекты, для которых заявка уже у render thread: новая заявка ждёт ответа.
    pub(crate) in_flight: HashSet<ObjectHandle>,
    /// Объекты, у которых материал надо создать заново.
    pub(crate) dirty: HashSet<ObjectHandle>,
    /// Принятые ответы render thread, которые ещё не применены.
    pub(crate) replies: VecDeque<MaterialReply>,
}

impl MaterialSync {
    pub(crate) fn accept(&mut self, reply: MaterialReply) {
        self.replies.push_back(reply);
    }

    /// Ставит заявки на объекты, у которых изменился материал. На объект не больше одной заявки в
    /// полёте: изменения во время заявки ждут ответа и уходят следующей заявкой.
    pub(crate) fn request_changes(
        &mut self,
        world: &mut World,
        gpu_scene: &GpuSceneTable,
        resources: &Sender<RenderResourceRequest>,
    ) {
        self.dirty.extend(world.take_mesh_material_changes());
        let ready: Vec<ObjectHandle> = self
            .dirty
            .iter()
            .copied()
            .filter(|handle| !self.in_flight.contains(handle))
            .collect();
        for handle in ready {
            self.dirty.remove(&handle);
            let Some(levels) = gpu_scene.by_object.get(&handle) else {
                continue;
            };
            let (Some(revision), Some(object)) =
                (world.mesh_material_revision(handle), world.object(handle))
            else {
                continue;
            };
            let Some(mesh) = object.mesh_renderer.as_ref() else {
                continue;
            };
            let Some(mut model) = gpu_scene.material_sources.get(&handle).cloned() else {
                log::warn!(
                    "[material] объект {}: CPU-описание модели для замены материала отсутствует",
                    object.id
                );
                continue;
            };
            model.material = mesh.material.clone();
            if let Some(path) = world.mesh_material_path(handle) {
                let part_count = levels.iter().map(Vec::len).max().unwrap_or(0);
                model.material_slots = vec![Some(path.as_str().to_owned()); part_count];
            }
            let request = MaterialRequest {
                object: handle,
                revision,
                levels: levels.clone(),
                model,
                prefab_token: gpu_scene.material_tokens.get(&handle).cloned(),
            };
            if resources
                .send(RenderResourceRequest::Material(Box::new(request)))
                .is_ok()
            {
                self.in_flight.insert(handle);
            }
        }
    }

    /// Применяет ответы: новые уровни заменяют прежние, материалы прежних освобождаются после снимков
    /// `seq ≥ last_seq`. Ответ по удалённому объекту освобождает свои материалы сразу.
    pub(crate) fn apply_replies(
        &mut self,
        world: &World,
        gpu_scene: &mut GpuSceneTable,
        resources: &Sender<RenderResourceRequest>,
        last_seq: u64,
    ) {
        while let Some(reply) = self.replies.pop_front() {
            self.in_flight.remove(&reply.object);
            match reply.result {
                Ok(MaterialLevels { levels, created })
                    if world.is_alive(reply.object)
                        && world.mesh_material_revision(reply.object) == Some(reply.revision)
                        && gpu_scene.by_object.contains_key(&reply.object) =>
                {
                    gpu_scene.by_object.insert(reply.object, levels);
                    let previous = self.owned.replace(reply.object, created);
                    send_material_release(resources, previous, last_seq);
                }
                Ok(MaterialLevels { created, .. }) => {
                    send_material_release(resources, created, 0);
                }
                Err(message) => {
                    let id = world
                        .object(reply.object)
                        .map_or("?", |object| object.id.as_str());
                    log::warn!("[material] объект {id}: материал не заменён: {message}");
                }
            }
            // Изменения, пришедшие во время заявки, ставят объект в очередь ещё раз.
            if world.mesh_material_revision(reply.object) != Some(reply.revision) {
                self.dirty.insert(reply.object);
            }
        }
        // Материалы объектов, удалённых из таблицы GPU, освобождаются вместе с ними.
        let gone = self
            .owned
            .take_dead(|handle| gpu_scene.by_object.contains_key(&handle));
        send_material_release(resources, gone, last_seq);
    }
}

/// Материалы, созданные по заявкам замены, по объектам: при следующей замене или удалении объекта
/// прежние материалы уходят на освобождение. Материалы сцены сюда не входят: они могут быть общими.
pub(crate) struct MaterialOwnership<M> {
    pub(crate) owned: HashMap<ObjectHandle, Vec<M>>,
}

impl<M> Default for MaterialOwnership<M> {
    fn default() -> Self {
        Self {
            owned: HashMap::new(),
        }
    }
}

impl<M> MaterialOwnership<M> {
    /// Записывает материалы новой замены и возвращает материалы прежней.
    pub(crate) fn replace(&mut self, object: ObjectHandle, created: Vec<M>) -> Vec<M> {
        self.owned.insert(object, created).unwrap_or_default()
    }

    /// Забирает материалы объектов, для которых `is_live` ложно.
    pub(crate) fn take_dead(&mut self, is_live: impl Fn(ObjectHandle) -> bool) -> Vec<M> {
        let dead: Vec<ObjectHandle> = self
            .owned
            .keys()
            .copied()
            .filter(|&handle| !is_live(handle))
            .collect();
        dead.into_iter()
            .filter_map(|handle| self.owned.remove(&handle))
            .flatten()
            .collect()
    }
}

pub(crate) fn send_material_release(
    resources: &Sender<RenderResourceRequest>,
    materials: Vec<MaterialId>,
    last_seq: u64,
) {
    if !materials.is_empty() {
        let _ = resources.send(RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        });
    }
}
