//! Выбор уровней LOD: метрика с учётом масштаба объекта и угла обзора, гистерезис между кадрами.

use super::{ObjectHandle, World};
use glam::Vec3;
use std::collections::HashMap;

/// Угол обзора, для которого заданы пороги `lod_group`: значение по умолчанию камеры (45°).
pub const LOD_REFERENCE_FOV: f32 = std::f32::consts::FRAC_PI_4;

impl World {
    /// Метрика LOD объекта: расстояние от `eye` до мировой позиции объекта, приведённое к объекту
    /// масштаба 1 и углу обзора [`LOD_REFERENCE_FOV`]. Метрика пропорциональна величине, обратной
    /// размеру объекта на экране: вдвое большой объект или вдвое меньший тангенс половины угла
    /// обзора дают вдвое меньшую метрику. Некорректный `fov_narrow` не учитывается.
    pub fn lod_distance(&self, handle: ObjectHandle, eye: Vec3, fov_narrow: f32) -> Option<f32> {
        let matrix = self.world_matrices.get(self.dense(handle)?)?;
        let scale = matrix
            .x_axis
            .truncate()
            .length()
            .max(matrix.y_axis.truncate().length())
            .max(matrix.z_axis.truncate().length());
        let distance = eye.distance(matrix.w_axis.truncate());
        let distance = if scale > 0.0 && scale.is_finite() {
            distance / scale
        } else {
            f32::INFINITY
        };
        let zoom =
            if fov_narrow.is_finite() && fov_narrow > 0.0 && fov_narrow < std::f32::consts::PI {
                (0.5 * fov_narrow).tan() / (0.5 * LOD_REFERENCE_FOV).tan()
            } else {
                1.0
            };
        Some(distance * zoom)
    }
}

/// Выбор уровней LOD одного вида (камеры) с гистерезисом: помнит уровень каждого объекта с
/// прошлого кадра. Объект, не выбиравшийся в кадре, забывается в [`LodSelector::finish_frame`].
#[derive(Debug, Default)]
pub struct LodSelector {
    previous: HashMap<ObjectHandle, Option<usize>>,
    current: HashMap<ObjectHandle, Option<usize>>,
}

impl LodSelector {
    /// Уровень объекта для вида: 0 — `mesh_renderer`, `None` — объект отсечён `cull_distance`
    /// или не жив. Объект без `lod_group` всегда получает уровень 0.
    pub fn select(
        &mut self,
        world: &World,
        handle: ObjectHandle,
        eye: Vec3,
        fov_narrow: f32,
    ) -> Option<usize> {
        let object = world.object(handle)?;
        let Some(group) = &object.lod_group else {
            return Some(0);
        };
        let distance = world.lod_distance(handle, eye, fov_narrow)?;
        let level = group.select_level(distance, self.previous.get(&handle).copied());
        self.current.insert(handle, level);
        level
    }

    /// Завершает кадр: выборы кадра становятся историей для следующего.
    pub fn finish_frame(&mut self) {
        std::mem::swap(&mut self.previous, &mut self.current);
        self.current.clear();
    }
}
