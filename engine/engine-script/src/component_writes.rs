//! Заявки скриптов на изменение компонентов сцены во время игры: свет, видимость и материал
//! меш-рендерера, эмиттеры частиц, окружение, LOD и звуковой источник.
//!
//! Заявки накапливаются в транзакции вызова, проверяются при вызове и применяются `World` при
//! коммите. Команды эмиттеров частиц (перезапуск, выброс) уходят в очередь хоста: их
//! применяет рендер-поток.

use engine_files::VirtualPath;
use engine_scene::world::{MeshMaterialParams, ParticleCommand};
use engine_scene::{
    AudioSourceComponent, DirectionalLightComponent, ObjectHandle, PointLightComponent,
    ProceduralSkyComponent, SpotLightComponent,
};
use std::collections::HashMap;

/// Наибольшее число команд эмиттеров, которые один вызов может поставить в очередь.
pub(crate) const MAX_PARTICLE_COMMANDS_PER_CALL: usize = 64;

#[derive(Default)]
pub(crate) struct ComponentWrites {
    pub(crate) directional_lights: HashMap<ObjectHandle, DirectionalLightComponent>,
    pub(crate) point_lights: HashMap<ObjectHandle, PointLightComponent>,
    pub(crate) spot_lights: HashMap<ObjectHandle, SpotLightComponent>,
    pub(crate) light_enabled: HashMap<ObjectHandle, bool>,
    pub(crate) mesh_visible: HashMap<ObjectHandle, bool>,
    /// Несколько вызовов за один кадр объединяются: заданные поля перекрывают прежние.
    pub(crate) mesh_material: HashMap<ObjectHandle, MeshMaterialParams>,
    /// Материал всех частей меш-рендерера по пути `.rvemat`; последний вызов за кадр побеждает.
    pub(crate) mesh_material_path: HashMap<ObjectHandle, VirtualPath>,
    pub(crate) particles_playing: HashMap<ObjectHandle, bool>,
    pub(crate) environment_intensity: HashMap<ObjectHandle, f32>,
    pub(crate) sky: HashMap<ObjectHandle, ProceduralSkyComponent>,
    pub(crate) lod: HashMap<ObjectHandle, (Vec<f32>, Option<f32>)>,
    pub(crate) audio_sources: HashMap<ObjectHandle, AudioSourceComponent>,
    pub(crate) particle_commands: Vec<ParticleCommand>,
}

impl ComponentWrites {
    pub(crate) fn clear(&mut self) {
        self.directional_lights.clear();
        self.point_lights.clear();
        self.spot_lights.clear();
        self.light_enabled.clear();
        self.mesh_visible.clear();
        self.mesh_material.clear();
        self.mesh_material_path.clear();
        self.particles_playing.clear();
        self.environment_intensity.clear();
        self.sky.clear();
        self.lod.clear();
        self.audio_sources.clear();
        self.particle_commands.clear();
    }
}
