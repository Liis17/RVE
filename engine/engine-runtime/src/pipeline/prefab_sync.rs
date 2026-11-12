//! Ответы загрузки префабов: привязка GPU-данных к объектам и предзагрузка звуков.

use super::*;

/// Применяет ответы загрузки: регистрирует шаблон, добавляет его GPU-уровни и геометрию. Ответы
/// со старым токеном освобождаются сразу: их ресурсы никто не использует.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drain_prefab_replies(
    pending: &mut VecDeque<ScriptReply>,
    scripts: &mut ScriptHost,
    world: &mut World,
    gpu_scene: &mut GpuSceneTable,
    spawn_physics: &mut SpawnPhysics,
    prefabs: &mut PrefabLoads,
    materials: &mut MaterialSync,
    ui_registry: &mut UiRegistry,
    scenes: &mut SceneSwitch,
) {
    while let Some(reply) = pending.pop_front() {
        let reply = match reply {
            ScriptReply::Material(reply) => {
                materials.accept(*reply);
                continue;
            }
            ScriptReply::Scene(reply) => {
                scenes.accept(*reply);
                continue;
            }
            ScriptReply::Prefab(reply) => reply,
        };
        match reply {
            PrefabReply::Ready(ready) => {
                let PrefabReady {
                    token,
                    gpu,
                    particles,
                    cpu,
                } = *ready;
                if !prefabs.is_current(&token) {
                    prefabs.release(token);
                    continue;
                }
                let PrefabCpu {
                    template,
                    sources,
                    geometry,
                    material_models,
                    ui,
                } = cpu;
                let ui_registration = match ui_registry.prepare(&ui) {
                    Ok(registration) => registration,
                    Err(error) => {
                        prefabs.forget(&token.path);
                        scripts.prefab_failed(world, &token.path, &error);
                        prefabs.release(token);
                        continue;
                    }
                };
                if let Err(error) = scripts.register_prefab(world, &token.path, template, &sources)
                {
                    prefabs.forget(&token.path);
                    scripts.prefab_failed(world, &token.path, &error.to_string());
                    prefabs.release(token);
                    continue;
                }
                ui_registry.commit(scripts, ui_registration);
                let geometry_paths = geometry.iter().map(|model| model.path.clone()).collect();
                spawn_physics.add_heavy_geometry(geometry);
                gpu_scene.prefabs.insert(token.path.clone(), gpu);
                gpu_scene
                    .particle_templates
                    .insert(token.path.clone(), particles);
                gpu_scene
                    .material_templates
                    .insert(token.path.clone(), material_models);
                gpu_scene
                    .prefab_tokens
                    .insert(token.path.clone(), token.clone());
                prefabs.mark_ready(&token, geometry_paths);
            }
            PrefabReply::Failed { token, error } => {
                if prefabs.is_current(&token) {
                    prefabs.forget(&token.path);
                    scripts.prefab_failed(world, &token.path, &error);
                }
            }
        }
    }
}

/// Записывает уровни шаблона на объекты поддерева: объект `i` получает уровни шаблона `i`.
/// Объекты без меша записи не получают.
pub(crate) fn attach_prefab_levels<T: Clone>(
    by_object: &mut HashMap<ObjectHandle, Vec<Vec<T>>>,
    templates: &[Vec<Vec<T>>],
    spawned: &SpawnedObject,
) {
    for (object, levels) in spawned.objects.iter().zip(templates) {
        if !levels.is_empty() {
            by_object.insert(*object, levels.clone());
        }
    }
}

pub(crate) fn attach_prefab_material_sources(
    gpu_scene: &mut GpuSceneTable,
    spawned: &SpawnedObject,
) {
    let Some(templates) = gpu_scene.material_templates.get(&spawned.prefab) else {
        return;
    };
    let token = gpu_scene.prefab_tokens.get(&spawned.prefab).cloned();
    for (object, source) in spawned.objects.iter().zip(templates) {
        if let Some(source) = source {
            gpu_scene.material_sources.insert(*object, source.clone());
            if let Some(token) = &token {
                gpu_scene.material_tokens.insert(*object, token.clone());
            }
        }
    }
}

pub(crate) fn detach_material_sources(gpu_scene: &mut GpuSceneTable, handles: &[ObjectHandle]) {
    for handle in handles {
        gpu_scene.material_sources.remove(handle);
        gpu_scene.material_tokens.remove(handle);
    }
}

pub(crate) fn detach_object_render_data(gpu_scene: &mut GpuSceneTable, handles: &[ObjectHandle]) {
    detach_objects(&mut gpu_scene.by_object, handles);
    detach_material_sources(gpu_scene, handles);
    for handle in handles {
        gpu_scene.particle_texture_owners.remove(handle);
    }
}

pub(crate) fn detach_objects<T>(
    by_object: &mut HashMap<ObjectHandle, Vec<Vec<T>>>,
    handles: &[ObjectHandle],
) {
    for handle in handles {
        by_object.remove(handle);
    }
}

/// Клипы шаблонов быстрых префабов предзагружаются вместе со сценой: spawn-объект должен
/// проигрывать звук без ожидания загрузки.
pub(crate) fn preload_prefab_sounds(
    audio: &engine_audio::AudioHandle,
    templates: &BTreeMap<String, Arc<PrefabTemplate>>,
) {
    let mut clips = templates
        .values()
        .flat_map(|template| template.objects())
        .filter_map(|object| object.audio_source.as_ref())
        .map(|source| source.clip.as_str().to_owned())
        .collect::<Vec<_>>();
    clips.sort();
    clips.dedup();
    if clips.is_empty() {
        return;
    }
    if let Err(error) = audio.preload(clips) {
        log::warn!("предзагрузка звука префабов: {error}");
    }
}
