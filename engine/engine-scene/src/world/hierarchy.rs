//! Иерархия, действующая активность, теги, слои и мировые трансформации объектов [`World`].
//!
//! Мировые геттеры и сеттеры читают состояние, пересчитанное по локальным трансформациям
//! на момент вызова, а не кэш последнего кадра. Изменения активности поддерева копятся в
//! очереди [`World::take_activity_changes`]: по ней физика и звук переключают тела и источники.

use super::{LocalTransform, ObjectHandle, World, WorldObject, topological_order};
use crate::document::{LAYER_COUNT, RigidBodyKind, SceneError, valid_ascii_name};
use crate::params::ComponentKind;
use glam::{EulerRot, Mat4, Quat, Vec3};
use std::collections::HashSet;

/// Изменение мира, которое скрипт ставит в очередь транзакции. Применяется через
/// [`World::apply_edit`] строго в порядке записи.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectEdit {
    SetActive {
        object: ObjectHandle,
        active: bool,
    },
    SetTag {
        object: ObjectHandle,
        tag: String,
    },
    SetLayer {
        object: ObjectHandle,
        layer: u8,
    },
    SetParent {
        object: ObjectHandle,
        parent: Option<ObjectHandle>,
        keep_world: bool,
    },
    SetWorldPosition {
        object: ObjectHandle,
        position: Vec3,
    },
    SetWorldRotation {
        object: ObjectHandle,
        rotation_deg: Vec3,
    },
}

impl World {
    /// Прямые потомки в порядке документа и spawn; `None`, если хендл не жив.
    pub fn children(&self, handle: ObjectHandle) -> Option<Vec<ObjectHandle>> {
        self.dense(handle)?;
        Some(
            self.objects
                .iter()
                .filter(|object| object.parent == Some(handle))
                .map(|object| object.handle)
                .collect(),
        )
    }

    /// Собственный флаг активности объекта, без учёта родителей.
    pub fn is_active(&self, handle: ObjectHandle) -> Option<bool> {
        self.object(handle).map(|object| object.active)
    }

    /// Действующая активность: объект и все его предки активны.
    pub fn active_in_hierarchy(&self, handle: ObjectHandle) -> Option<bool> {
        let index = self.dense(handle)?;
        Some(self.active_in_hierarchy_at(index))
    }

    /// Действующая активность по плотным индексам; цикл parent в мире невозможен.
    pub(crate) fn active_in_hierarchy_at(&self, index: usize) -> bool {
        let mut current = Some(index);
        while let Some(index) = current {
            if !self.objects[index].active {
                return false;
            }
            current = self.parent_index(index);
        }
        true
    }

    /// Действующая активность всех объектов по плотным индексам.
    pub(crate) fn active_flags(&self) -> Vec<bool> {
        (0..self.objects.len())
            .map(|index| self.active_in_hierarchy_at(index))
            .collect()
    }

    /// Объекты, у которых действующая активность изменилась с последнего вызова. Мёртвые хендлы
    /// и повторы пропускаются.
    pub fn take_activity_changes(&mut self) -> Vec<ObjectHandle> {
        let changes = std::mem::take(&mut self.activity_changes);
        let mut seen = HashSet::with_capacity(changes.len());
        changes
            .into_iter()
            .filter(|&handle| self.is_alive(handle) && seen.insert(handle))
            .collect()
    }

    /// Меняет собственный флаг активности. Нельзя скрыть объект, внутри поддерева которого
    /// находится активная камера.
    pub fn set_active(&mut self, handle: ObjectHandle, active: bool) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        if self.objects[index].active == active {
            return Ok(());
        }
        if !active && self.subtree(&[handle]).contains(&self.active_camera) {
            return Err(self.object_error(index, "нельзя скрыть объект с активной камерой"));
        }
        let before = self.subtree_activity(handle);
        self.objects[index].active = active;
        self.record_activity(&before);
        Ok(())
    }

    /// Меняет родителя. `keep_world` сохраняет мировую позу, пересчитывая локальную трансформацию;
    /// при этом сдвиг, вызванный неравномерным масштабом родителя, не восстанавливается точно.
    ///
    /// Отказывает для циклов, родителя-самого объекта, dynamic rigid body и character controller
    /// (они остаются корнями) и для переноса активной камеры под неактивного родителя.
    pub fn set_parent(
        &mut self,
        handle: ObjectHandle,
        parent: Option<ObjectHandle>,
        keep_world: bool,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        let parent_index = match parent {
            Some(parent) => Some(self.live_index_or_error(parent)?),
            None => None,
        };
        if self.objects[index].parent == parent {
            return Ok(());
        }
        if let Some(parent_index) = parent_index {
            let mut cursor = Some(parent_index);
            while let Some(current) = cursor {
                if current == index {
                    return Err(self.object_error(index, "обнаружен цикл в иерархии parent"));
                }
                cursor = self.parent_index(current);
            }
            let object = &self.objects[index];
            let dynamic = object
                .rigid_body
                .as_ref()
                .is_some_and(|body| matches!(body.kind, RigidBodyKind::Dynamic));
            if dynamic || object.character_controller.is_some() {
                return Err(self.object_error(
                    index,
                    "dynamic rigid_body и character_controller остаются корневыми объектами",
                ));
            }
        }
        let parent_active = parent_index.is_none_or(|parent| self.active_in_hierarchy_at(parent));
        let effective_active = self.objects[index].active && parent_active;
        if !effective_active && self.subtree(&[handle]).contains(&self.active_camera) {
            return Err(self.object_error(index, "нельзя скрыть объект с активной камерой"));
        }

        let transform = if keep_world {
            let parent_matrix =
                parent_index.map_or(Mat4::IDENTITY, |parent| self.fresh_world_matrix(parent));
            let inverse = parent_matrix.inverse();
            if !inverse.is_finite() {
                return Err(self.object_error(index, "мировая матрица родителя вырождена"));
            }
            let local = local_from_matrix(inverse * self.fresh_world_matrix(index));
            if !transform_is_finite(local) {
                return Err(
                    self.object_error(index, "мировая поза объекта не переносится в локальную")
                );
            }
            Some(local)
        } else {
            None
        };

        let mut parents = self
            .objects
            .iter()
            .map(|object| object.parent.and_then(|parent| self.dense(parent)))
            .collect::<Vec<_>>();
        parents[index] = parent_index;
        let ids = self
            .objects
            .iter()
            .map(|object| object.id.as_str())
            .collect::<Vec<_>>();
        let order = topological_order(&parents, &ids, &self.source_label)?;

        let before = self.subtree_activity(handle);
        self.objects[index].parent = parent;
        if let Some(transform) = transform {
            self.objects[index].transform = transform;
        }
        self.topological_order = order;
        self.recalculate_world_matrices();
        self.record_activity(&before);
        Ok(())
    }

    /// Ставит мировую позицию, пересчитывая локальную относительно родителя.
    pub fn set_world_position(
        &mut self,
        handle: ObjectHandle,
        position: Vec3,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        self.objects[index].transform.position = self.local_position_for_world(index, position)?;
        Ok(())
    }

    /// Проверяет, что [`World::set_world_position`] примет эту позицию, и не меняет мир.
    pub fn check_world_position(
        &self,
        handle: ObjectHandle,
        position: Vec3,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        self.local_position_for_world(index, position).map(|_| ())
    }

    fn local_position_for_world(&self, index: usize, position: Vec3) -> Result<Vec3, SceneError> {
        if !position.is_finite() {
            return Err(self.object_error(index, "мировая позиция должна быть конечной"));
        }
        let parent_matrix = self
            .parent_index(index)
            .map_or(Mat4::IDENTITY, |parent| self.fresh_world_matrix(parent));
        let inverse = parent_matrix.inverse();
        if !inverse.is_finite() {
            return Err(self.object_error(index, "мировая матрица родителя вырождена"));
        }
        let local = inverse.transform_point3(position);
        if !local.is_finite() {
            return Err(self.object_error(index, "локальная позиция объекта не конечна"));
        }
        Ok(local)
    }

    /// Мировая позиция по текущим локальным трансформациям.
    pub fn world_position(&self, handle: ObjectHandle) -> Option<Vec3> {
        let index = self.dense(handle)?;
        Some(self.fresh_world_matrix(index).w_axis.truncate())
    }

    /// Ставит мировой поворот (углы Эйлера YXZ в градусах), пересчитывая локальный относительно родителя.
    pub fn set_world_rotation(
        &mut self,
        handle: ObjectHandle,
        rotation_deg: Vec3,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        self.objects[index].transform.rotation_deg =
            self.local_rotation_for_world(index, rotation_deg)?;
        Ok(())
    }

    /// Проверяет, что [`World::set_world_rotation`] примет этот поворот, и не меняет мир.
    pub fn check_world_rotation(
        &self,
        handle: ObjectHandle,
        rotation_deg: Vec3,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        self.local_rotation_for_world(index, rotation_deg)
            .map(|_| ())
    }

    fn local_rotation_for_world(
        &self,
        index: usize,
        rotation_deg: Vec3,
    ) -> Result<Vec3, SceneError> {
        if !rotation_deg.is_finite() {
            return Err(self.object_error(index, "мировой поворот должен быть конечным"));
        }
        let parent_rotation = self
            .parent_index(index)
            .map_or(Quat::IDENTITY, |parent| self.fresh_world_rotation(parent));
        let local = (parent_rotation.inverse() * quat_from_degrees(rotation_deg)).normalize();
        let rotation_deg = rotation_to_degrees(local);
        if !rotation_deg.is_finite() {
            return Err(self.object_error(index, "локальный поворот объекта не конечен"));
        }
        Ok(rotation_deg)
    }

    /// Мировой поворот в градусах по текущим локальным трансформациям.
    pub fn world_rotation_deg(&self, handle: ObjectHandle) -> Option<Vec3> {
        let index = self.dense(handle)?;
        Some(rotation_to_degrees(self.fresh_world_rotation(index)))
    }

    /// Применяет одно изменение; при ошибке мир не меняется.
    pub fn apply_edit(&mut self, edit: &ObjectEdit) -> Result<(), SceneError> {
        match edit {
            ObjectEdit::SetActive { object, active } => self.set_active(*object, *active),
            ObjectEdit::SetTag { object, tag } => self.set_tag(*object, tag.clone()),
            ObjectEdit::SetLayer { object, layer } => self.set_layer(*object, *layer),
            ObjectEdit::SetParent {
                object,
                parent,
                keep_world,
            } => self.set_parent(*object, *parent, *keep_world),
            ObjectEdit::SetWorldPosition { object, position } => {
                self.set_world_position(*object, *position)
            }
            ObjectEdit::SetWorldRotation {
                object,
                rotation_deg,
            } => self.set_world_rotation(*object, *rotation_deg),
        }
    }

    /// Первый объект с таким отображаемым именем (`name`, а без него `id`); неактивные тоже ищутся.
    pub fn find_by_name(&self, name: &str) -> Option<ObjectHandle> {
        self.objects
            .iter()
            .find(|object| object.name == name)
            .map(|object| object.handle)
    }

    /// Все живые объекты с таким отображаемым именем в порядке документа и spawn.
    pub fn find_all_by_name(&self, name: &str) -> Vec<ObjectHandle> {
        self.objects
            .iter()
            .filter(|object| object.name == name)
            .map(|object| object.handle)
            .collect()
    }

    /// Первый объект с тегом; пустой тег ничего не находит.
    pub fn find_with_tag(&self, tag: &str) -> Option<ObjectHandle> {
        if tag.is_empty() {
            return None;
        }
        self.objects
            .iter()
            .find(|object| object.tag == tag)
            .map(|object| object.handle)
    }

    /// Все живые объекты с тегом; пустой тег ничего не находит.
    pub fn find_all_with_tag(&self, tag: &str) -> Vec<ObjectHandle> {
        if tag.is_empty() {
            return Vec::new();
        }
        self.objects
            .iter()
            .filter(|object| object.tag == tag)
            .map(|object| object.handle)
            .collect()
    }

    /// Тег объекта; пустая строка означает отсутствие тега.
    pub fn tag(&self, handle: ObjectHandle) -> Option<&str> {
        self.object(handle).map(|object| object.tag.as_str())
    }

    /// Ставит тег `[A-Za-z0-9_-]{1,64}` или пустую строку для его снятия.
    pub fn set_tag(&mut self, handle: ObjectHandle, tag: String) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        if !tag.is_empty() && !valid_ascii_name(&tag, true) {
            return Err(self.object_error(index, "tag должен соответствовать [A-Za-z0-9_-]{1,64}"));
        }
        self.objects[index].tag = tag;
        Ok(())
    }

    /// Слой объекта `0..32`.
    pub fn layer(&self, handle: ObjectHandle) -> Option<u8> {
        self.object(handle).map(|object| object.layer)
    }

    pub fn set_layer(&mut self, handle: ObjectHandle, layer: u8) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        if layer >= LAYER_COUNT {
            return Err(self.object_error(
                index,
                format!("layer должен быть в диапазоне 0..{LAYER_COUNT}"),
            ));
        }
        self.objects[index].layer = layer;
        Ok(())
    }

    /// Есть ли у объекта компонент; `None`, если хендл не жив.
    pub fn has_component(&self, handle: ObjectHandle, kind: &ComponentKind) -> Option<bool> {
        self.object(handle)
            .map(|object| component_present(object, kind))
    }

    fn subtree_activity(&self, root: ObjectHandle) -> Vec<(ObjectHandle, bool)> {
        self.subtree(&[root])
            .into_iter()
            .filter_map(|handle| Some((handle, self.active_in_hierarchy(handle)?)))
            .collect()
    }

    /// Записывает объекты, у которых действующая активность отличается от `before`.
    fn record_activity(&mut self, before: &[(ObjectHandle, bool)]) {
        for &(handle, was_active) in before {
            if self.active_in_hierarchy(handle) != Some(was_active) {
                self.activity_changes.push(handle);
            }
        }
    }

    /// Мировая матрица по текущим локальным трансформациям, без кэша.
    fn fresh_world_matrix(&self, index: usize) -> Mat4 {
        let local = self.objects[index].transform.matrix();
        match self.parent_index(index) {
            Some(parent) => self.fresh_world_matrix(parent) * local,
            None => local,
        }
    }

    fn fresh_world_rotation(&self, index: usize) -> Quat {
        let local = quat_from_degrees(self.objects[index].transform.rotation_deg);
        match self.parent_index(index) {
            Some(parent) => (self.fresh_world_rotation(parent) * local).normalize(),
            None => local,
        }
    }

    fn object_error(&self, index: usize, message: impl Into<String>) -> SceneError {
        SceneError::new(&self.source_label, Some(&self.objects[index].id), message)
    }
}

fn component_present(object: &WorldObject, kind: &ComponentKind) -> bool {
    match kind {
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
        ComponentKind::Script(path) => object
            .lite_scripts
            .iter()
            .any(|script| script.path.as_str() == path.as_str()),
    }
}

fn transform_is_finite(transform: LocalTransform) -> bool {
    transform.position.is_finite()
        && transform.rotation_deg.is_finite()
        && transform.scale.is_finite()
}

/// Локальная трансформация из матрицы `T · R · S`; углы в градусах YXZ, как у `set_root_pose`.
fn local_from_matrix(matrix: Mat4) -> LocalTransform {
    let (scale, rotation, position) = matrix.to_scale_rotation_translation();
    LocalTransform {
        position,
        rotation_deg: rotation_to_degrees(rotation),
        scale,
    }
}

fn quat_from_degrees(rotation_deg: Vec3) -> Quat {
    let rotation = rotation_deg * (std::f32::consts::PI / 180.0);
    Quat::from_euler(EulerRot::YXZ, rotation.y, rotation.x, rotation.z).normalize()
}

fn rotation_to_degrees(rotation: Quat) -> Vec3 {
    let (yaw, pitch, roll) = rotation.normalize().to_euler(EulerRot::YXZ);
    Vec3::new(pitch, yaw, roll) * (180.0 / std::f32::consts::PI)
}
