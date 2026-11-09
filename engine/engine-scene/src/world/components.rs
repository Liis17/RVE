//! Изменения компонентов, которые скрипты вносят во время игры: свет, видимость и материал
//! меш-рендерера, состояние эмиттеров частиц, процедурное небо и уровни LOD.
//!
//! Проверки значений — чистые функции: транзакция скрипта вызывает их в момент вызова, чтобы
//! плохое значение давало ошибку вызова, а не падение кадра. Сеттеры `World` повторяют проверку
//! и отказывают без изменений мира.

use super::{ObjectHandle, World};
use crate::audio::{AudioSourceComponent, audio_source_problem};
use crate::document::{
    DirectionalLightComponent, PointLightComponent, ProceduralSkyComponent, SpotLightComponent,
};
use crate::{SceneError, light::MAX_SHADOW_CASCADES};
use engine_files::VirtualPath;
use std::collections::HashSet;

/// Наибольший одноразовый выброс частиц за вызов.
pub const MAX_PARTICLE_BURST: u32 = 4096;

/// Параметры материала меш-рендерера, которые скрипт может заменить. `None` оставляет прежнее.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeshMaterialParams {
    pub base_color: Option<[f32; 3]>,
    pub emissive: Option<[f32; 3]>,
    pub metallic: Option<f32>,
    pub roughness: Option<f32>,
}

/// Команда эмиттеру частиц, которую применяют потоки рендера (runtime и редактор).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParticleCommand {
    /// Сбрасывает живые частицы и запускает цикл заново.
    Restart { object: ObjectHandle },
    /// Одноразовый выброс `count` частиц.
    Burst { object: ObjectHandle, count: u32 },
}

fn non_negative_color(color: [f32; 3]) -> bool {
    color
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
}

/// Причина отказа для направленного света или `None`, если значения допустимы.
pub fn directional_light_problem(light: &DirectionalLightComponent) -> Option<&'static str> {
    if !non_negative_color(light.color) {
        return Some("directional_light.color должен содержать конечные компоненты >= 0");
    }
    if !non_negative_color(light.ambient) {
        return Some("directional_light.ambient должен содержать конечные компоненты >= 0");
    }
    if !light.shadow_distance.is_finite() || light.shadow_distance <= 0.0 {
        return Some("directional_light.shadow_distance должен быть конечным и > 0");
    }
    if !(1..=MAX_SHADOW_CASCADES).contains(&light.shadow_cascades) {
        return Some("directional_light.shadow_cascades должен быть в диапазоне 1..=4");
    }
    None
}

/// Причина отказа для точечного света или `None`.
pub fn point_light_problem(light: &PointLightComponent) -> Option<&'static str> {
    if !non_negative_color(light.color) {
        return Some("point_light.color должен содержать конечные компоненты >= 0");
    }
    if !light.intensity.is_finite() || light.intensity < 0.0 {
        return Some("point_light.intensity должен быть конечным и >= 0");
    }
    if !light.range.is_finite() || light.range <= 0.0 {
        return Some("point_light.range должен быть конечным и > 0");
    }
    None
}

/// Причина отказа для конического света или `None`. Углы — те же границы, что у сцены.
pub fn spot_light_problem(light: &SpotLightComponent) -> Option<&'static str> {
    if !non_negative_color(light.color) {
        return Some("spot_light.color должен содержать конечные компоненты >= 0");
    }
    if !light.intensity.is_finite() || light.intensity < 0.0 {
        return Some("spot_light.intensity должен быть конечным и >= 0");
    }
    if !light.range.is_finite() || light.range <= 0.0 {
        return Some("spot_light.range должен быть конечным и > 0");
    }
    if !light.inner_angle_deg.is_finite()
        || !light.outer_angle_deg.is_finite()
        || light.inner_angle_deg <= 0.0
        || light.inner_angle_deg > light.outer_angle_deg
        || light.outer_angle_deg > 90.0
    {
        return Some(
            "spot_light angles должны удовлетворять 0 < inner_angle_deg <= outer_angle_deg <= 90",
        );
    }
    None
}

/// Причина отказа для параметров материала: факторы в `[0, 1]`, эмиссия неотрицательна.
pub fn mesh_material_problem(params: &MeshMaterialParams) -> Option<&'static str> {
    let unit = |value: f32| value.is_finite() && (0.0..=1.0).contains(&value);
    if params
        .base_color
        .is_some_and(|color| !color.into_iter().all(unit))
    {
        return Some("material.base_color должен быть конечным и находиться в диапазоне [0, 1]");
    }
    if params.metallic.is_some_and(|value| !unit(value)) {
        return Some("material.metallic должен быть конечным и находиться в диапазоне [0, 1]");
    }
    if params.roughness.is_some_and(|value| !unit(value)) {
        return Some("material.roughness должен быть конечным и находиться в диапазоне [0, 1]");
    }
    if params
        .emissive
        .is_some_and(|color| !non_negative_color(color))
    {
        return Some("material.emissive должен содержать конечные неотрицательные значения");
    }
    None
}

/// Причина отказа для интенсивности окружения.
pub fn environment_intensity_problem(intensity: f32) -> Option<&'static str> {
    (!intensity.is_finite() || intensity < 0.0)
        .then_some("environment.intensity должен быть конечным и >= 0")
}

/// Причина отказа для процедурного неба по тем же правилам, что и сцена.
pub fn procedural_sky_problem(sky: &ProceduralSkyComponent) -> Option<&'static str> {
    if ![sky.zenith_color, sky.horizon_color, sky.ground_color]
        .into_iter()
        .all(non_negative_color)
    {
        return Some("environment.procedural цвета должны содержать конечные компоненты >= 0");
    }
    if !sky.sun_size_deg.is_finite()
        || !(0.0..=180.0).contains(&sky.sun_size_deg)
        || sky.sun_size_deg == 0.0
    {
        return Some("environment.procedural.sun_size_deg должен быть в диапазоне (0, 180]");
    }
    if !sky.sun_intensity.is_finite() || sky.sun_intensity < 0.0 {
        return Some("environment.procedural.sun_intensity должен быть конечным и >= 0");
    }
    None
}

/// Причина отказа для дистанций LOD: конечные, строго возрастающие, больше нуля; cull — после них.
pub fn lod_distances_problem(
    distances: &[f32],
    cull_distance: Option<f32>,
) -> Option<&'static str> {
    let mut previous = 0.0_f32;
    for &distance in distances {
        if !distance.is_finite() || distance <= previous {
            return Some("дистанции LOD должны быть конечными, > 0 и строго возрастать");
        }
        previous = distance;
    }
    match cull_distance {
        Some(cull) if !cull.is_finite() || cull <= previous => {
            Some("cull_distance должен быть конечным и больше последнего distance")
        }
        _ => None,
    }
}

impl World {
    fn handle_error(&self, handle: ObjectHandle, message: impl Into<String>) -> SceneError {
        let object_id = self.object(handle).map(|object| object.id.as_str());
        SceneError::new(&self.source_label, object_id, message)
    }

    /// Заменяет направленный свет существующего объекта; новый компонент не создаётся.
    pub fn set_directional_light(
        &mut self,
        handle: ObjectHandle,
        light: DirectionalLightComponent,
    ) -> Result<(), SceneError> {
        if let Some(problem) = directional_light_problem(&light) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.directional_light.is_none() {
            return Err(self.handle_error(handle, "у объекта нет directional_light"));
        }
        object.directional_light = Some(light);
        Ok(())
    }

    /// Заменяет точечный свет существующего объекта.
    pub fn set_point_light(
        &mut self,
        handle: ObjectHandle,
        light: PointLightComponent,
    ) -> Result<(), SceneError> {
        if let Some(problem) = point_light_problem(&light) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.point_light.is_none() {
            return Err(self.handle_error(handle, "у объекта нет point_light"));
        }
        object.point_light = Some(light);
        Ok(())
    }

    /// Заменяет конический свет существующего объекта.
    pub fn set_spot_light(
        &mut self,
        handle: ObjectHandle,
        light: SpotLightComponent,
    ) -> Result<(), SceneError> {
        if let Some(problem) = spot_light_problem(&light) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.spot_light.is_none() {
            return Err(self.handle_error(handle, "у объекта нет spot_light"));
        }
        object.spot_light = Some(light);
        Ok(())
    }

    /// Включает или выключает свет объекта без потери его параметров. Не влияет на тени других
    /// компонентов; требует хотя бы одного света.
    pub fn set_light_enabled(
        &mut self,
        handle: ObjectHandle,
        enabled: bool,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.directional_light.is_none()
            && object.point_light.is_none()
            && object.spot_light.is_none()
        {
            return Err(self.handle_error(handle, "у объекта нет света"));
        }
        object.light_enabled = enabled;
        Ok(())
    }

    /// Видимость меш-рендерера: скрытый объект не попадает в кадр. Требует `mesh_renderer`.
    pub fn set_mesh_visible(
        &mut self,
        handle: ObjectHandle,
        visible: bool,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.mesh_renderer.is_none() {
            return Err(self.handle_error(handle, "у объекта нет mesh_renderer"));
        }
        object.mesh_visible = visible;
        Ok(())
    }

    /// Меняет переопределённые факторы материала меш-рендерера. Не заданные поля сохраняются.
    pub fn set_mesh_material_params(
        &mut self,
        handle: ObjectHandle,
        params: MeshMaterialParams,
    ) -> Result<(), SceneError> {
        if let Some(problem) = mesh_material_problem(&params) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        let Some(mesh) = object.mesh_renderer.as_mut() else {
            return Err(self.handle_error(handle, "у объекта нет mesh_renderer"));
        };
        let material = mesh.material.get_or_insert_with(Default::default);
        if params.base_color.is_some() {
            material.base_color = params.base_color;
        }
        if params.emissive.is_some() {
            material.emissive = params.emissive;
        }
        if params.metallic.is_some() {
            material.metallic = params.metallic;
        }
        if params.roughness.is_some() {
            material.roughness = params.roughness;
        }
        self.mark_mesh_material_changed(handle);
        Ok(())
    }

    /// Заменяет материал всех частей меш-рендерера на `.rvemat` из `[[assets]]` сцены. Путь
    /// проверяется здесь, поэтому отказ не меняет мир.
    pub fn set_mesh_material_path(
        &mut self,
        handle: ObjectHandle,
        path: VirtualPath,
    ) -> Result<(), SceneError> {
        if let Some(problem) = self.mesh_material_path_problem(&path) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        if self.objects[index].mesh_renderer.is_none() {
            return Err(self.handle_error(handle, "у объекта нет mesh_renderer"));
        }
        self.objects[index].mesh_material_path = Some(path);
        self.mark_mesh_material_changed(handle);
        Ok(())
    }

    /// Причина отказа для пути материала скрипта: файл должен быть объявлен в `[[assets]]` сцены
    /// и иметь расширение `.rvemat`, как слот материала в сцене.
    pub fn mesh_material_path_problem(&self, path: &VirtualPath) -> Option<String> {
        if !self.asset_paths.contains(path) {
            return Some(format!(
                "материал /{} не объявлен в [[assets]]",
                path.as_str()
            ));
        }
        if !path.as_str().to_ascii_lowercase().ends_with(".rvemat") {
            return Some(format!(
                "материал /{} должен иметь расширение .rvemat",
                path.as_str()
            ));
        }
        None
    }

    /// Путь материала, которым скрипт заменил меш-рендерер объекта; `None` — материал сцены.
    pub fn mesh_material_path(&self, handle: ObjectHandle) -> Option<&VirtualPath> {
        self.object(handle)?.mesh_material_path.as_ref()
    }

    /// Ревизия материала меш-рендерера; растёт при каждом изменении, которое нужно загрузить в GPU.
    pub fn mesh_material_revision(&self, handle: ObjectHandle) -> Option<u64> {
        Some(self.object(handle)?.mesh_material_revision)
    }

    /// Объекты, у которых материал меш-рендерера изменился с последнего вызова. Мёртвые хендлы
    /// и повторы пропускаются.
    pub fn take_mesh_material_changes(&mut self) -> Vec<ObjectHandle> {
        let changes = std::mem::take(&mut self.mesh_material_changes);
        let mut seen = HashSet::with_capacity(changes.len());
        changes
            .into_iter()
            .filter(|&handle| self.is_alive(handle) && seen.insert(handle))
            .collect()
    }

    fn mark_mesh_material_changed(&mut self, handle: ObjectHandle) {
        if let Some(index) = self.dense(handle) {
            let object = &mut self.objects[index];
            object.mesh_material_revision += 1;
            self.mesh_material_changes.push(handle);
        }
    }

    /// Запускает или останавливает рождение частиц. Симуляция живых частиц продолжается.
    pub fn set_particles_playing(
        &mut self,
        handle: ObjectHandle,
        playing: bool,
    ) -> Result<(), SceneError> {
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        if object.particle_emitter.is_none() {
            return Err(self.handle_error(handle, "у объекта нет particle_emitter"));
        }
        object.particles_playing = playing;
        Ok(())
    }

    /// Меняет интенсивность окружения объекта с компонентом `environment`.
    pub fn set_environment_intensity(
        &mut self,
        handle: ObjectHandle,
        intensity: f32,
    ) -> Result<(), SceneError> {
        if let Some(problem) = environment_intensity_problem(intensity) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        let Some(environment) = object.environment.as_mut() else {
            return Err(self.handle_error(handle, "у объекта нет environment"));
        };
        environment.intensity = intensity;
        Ok(())
    }

    /// Заменяет параметры процедурного неба. HDR-окружение и смена его источника не поддерживаются.
    pub fn set_procedural_sky(
        &mut self,
        handle: ObjectHandle,
        sky: ProceduralSkyComponent,
    ) -> Result<(), SceneError> {
        if let Some(problem) = procedural_sky_problem(&sky) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let object = &mut self.objects[index];
        let Some(environment) = object.environment.as_mut() else {
            return Err(self.handle_error(handle, "у объекта нет environment"));
        };
        if environment.procedural.is_none() {
            return Err(
                self.handle_error(handle, "environment использует HDR, а не процедурное небо")
            );
        }
        environment.procedural = Some(sky);
        Ok(())
    }

    /// Заменяет дистанции уровней LOD (по одной на уровень) и дистанцию отсечения.
    pub fn set_lod_distances(
        &mut self,
        handle: ObjectHandle,
        distances: &[f32],
        cull_distance: Option<f32>,
    ) -> Result<(), SceneError> {
        if let Some(problem) = lod_distances_problem(distances, cull_distance) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let Some(group_levels) = self.objects[index]
            .lod_group
            .as_ref()
            .map(|group| group.levels.len())
        else {
            return Err(self.handle_error(handle, "у объекта нет lod_group"));
        };
        if group_levels != distances.len() {
            return Err(self.handle_error(
                handle,
                format!(
                    "lod_group имеет {group_levels} уровней, а передано {} дистанций",
                    distances.len()
                ),
            ));
        }
        let Some(group) = self.objects[index].lod_group.as_mut() else {
            return Err(self.handle_error(handle, "у объекта нет lod_group"));
        };
        for (level, &distance) in group.levels.iter_mut().zip(distances) {
            level.distance = distance;
        }
        group.cull_distance = cull_distance;
        Ok(())
    }

    /// Заменяет параметры звукового источника; клип и `[[assets]]` сохраняются. Применяется к
    /// следующим запускам источника.
    pub fn set_audio_source_settings(
        &mut self,
        handle: ObjectHandle,
        settings: AudioSourceComponent,
    ) -> Result<(), SceneError> {
        if let Some(problem) = audio_source_problem(&settings) {
            return Err(self.handle_error(handle, problem));
        }
        let index = self.live_index_or_error(handle)?;
        let Some(current) = self.objects[index].audio_source.as_mut() else {
            return Err(self.handle_error(handle, "у объекта нет audio_source"));
        };
        *current = AudioSourceComponent {
            clip: current.clip.clone(),
            ..settings
        };
        Ok(())
    }

    /// Команда эмиттеру: сброс или одноразовый выброс. Проверяет наличие эмиттера и границы.
    pub fn particle_command_target(&self, command: ParticleCommand) -> Result<(), SceneError> {
        let (handle, count) = match command {
            ParticleCommand::Restart { object } => (object, None),
            ParticleCommand::Burst { object, count } => (object, Some(count)),
        };
        if count.is_some_and(|count| count == 0 || count > MAX_PARTICLE_BURST) {
            return Err(self.handle_error(
                handle,
                format!("emit: число частиц должно быть в диапазоне 1..={MAX_PARTICLE_BURST}"),
            ));
        }
        match self.object(handle) {
            Some(object) if object.particle_emitter.is_some() => Ok(()),
            Some(_) => Err(self.handle_error(handle, "у объекта нет particle_emitter")),
            None => Err(SceneError::new(
                &self.source_label,
                None,
                "объект не найден",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_validators_reject_non_finite_and_bad_angles() {
        let mut spot = SpotLightComponent::default();
        assert_eq!(spot_light_problem(&spot), None);
        spot.inner_angle_deg = 40.0;
        assert!(spot_light_problem(&spot).is_some());
        spot = SpotLightComponent::default();
        spot.range = f32::NAN;
        assert!(spot_light_problem(&spot).is_some());

        let point = PointLightComponent {
            intensity: -1.0,
            ..PointLightComponent::default()
        };
        assert!(point_light_problem(&point).is_some());

        let mut directional = DirectionalLightComponent {
            color: [1.0; 3],
            ambient: [0.1; 3],
            shadows: false,
            shadow_distance: 50.0,
            shadow_cascades: 2,
        };
        assert_eq!(directional_light_problem(&directional), None);
        directional.shadow_cascades = 9;
        assert!(directional_light_problem(&directional).is_some());
    }

    #[test]
    fn material_params_must_stay_in_range() {
        let ok = MeshMaterialParams {
            base_color: Some([0.2, 0.4, 0.6]),
            emissive: Some([4.0, 0.0, 1.0]),
            metallic: Some(1.0),
            roughness: Some(0.0),
        };
        assert_eq!(mesh_material_problem(&ok), None);
        for bad in [
            MeshMaterialParams {
                base_color: Some([1.5, 0.0, 0.0]),
                ..Default::default()
            },
            MeshMaterialParams {
                metallic: Some(-0.1),
                ..Default::default()
            },
            MeshMaterialParams {
                roughness: Some(f32::INFINITY),
                ..Default::default()
            },
            MeshMaterialParams {
                emissive: Some([-1.0, 0.0, 0.0]),
                ..Default::default()
            },
        ] {
            assert!(mesh_material_problem(&bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn environment_and_sky_validators() {
        assert!(environment_intensity_problem(2.5).is_none());
        assert!(environment_intensity_problem(-0.5).is_some());
        assert!(environment_intensity_problem(f32::NAN).is_some());

        let mut sky = ProceduralSkyComponent {
            zenith_color: [0.1, 0.2, 0.8],
            horizon_color: [0.6; 3],
            ground_color: [0.1; 3],
            sun_size_deg: 1.0,
            sun_intensity: 3.0,
        };
        assert!(procedural_sky_problem(&sky).is_none());
        sky.sun_size_deg = 0.0;
        assert!(procedural_sky_problem(&sky).is_some());
        sky.sun_size_deg = 1.0;
        sky.horizon_color = [-0.1, 0.0, 0.0];
        assert!(procedural_sky_problem(&sky).is_some());
    }

    #[test]
    fn lod_distances_must_ascend_and_cull_after_the_last_level() {
        assert_eq!(lod_distances_problem(&[10.0, 20.0], Some(40.0)), None);
        assert_eq!(lod_distances_problem(&[10.0, 20.0], None), None);
        assert!(lod_distances_problem(&[20.0, 10.0], None).is_some());
        assert!(lod_distances_problem(&[0.0], None).is_some());
        assert!(lod_distances_problem(&[10.0, f32::NAN], None).is_some());
        assert!(lod_distances_problem(&[10.0], Some(10.0)).is_some());
    }
}
