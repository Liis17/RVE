//! Применение изменений мира к физике, звуку и запросам префабов.

use super::*;

/// Переключает тела объектов, у которых изменилась действующая активность: тела неактивных
/// объектов удаляются, тела активированных создаются заново. Возвращает заявки звука: источники
/// неактивных объектов останавливаются, у показанных запускаются `play_on_start`. Тела вставляются
/// и удаляются до `sync_world_commands`, как и spawn.
pub(crate) fn apply_activity_changes(
    world: &mut World,
    physics: Option<&mut PhysicsSceneSync>,
    spawn_physics: &mut SpawnPhysics,
) -> Vec<AudioRequest> {
    let changed = world.take_activity_changes();
    if changed.is_empty() {
        return Vec::new();
    }
    let (hidden, shown): (Vec<ObjectHandle>, Vec<ObjectHandle>) = changed
        .into_iter()
        .partition(|&object| world.active_in_hierarchy(object) != Some(true));
    if let Some(sync) = physics {
        spawn_physics.changes.extend(sync.remove_objects(&hidden));
        let bound: std::collections::HashSet<ObjectHandle> = sync
            .bindings()
            .iter()
            .map(|binding| binding.object)
            .collect();
        let unbound: Vec<ObjectHandle> = shown
            .iter()
            .copied()
            .filter(|object| world.is_alive(*object) && !bound.contains(object))
            .collect();
        if !unbound.is_empty() {
            match sync.insert_objects(
                world,
                &unbound,
                &spawn_physics.geometry,
                &mut spawn_physics.shapes,
            ) {
                Ok(inserted) => spawn_physics.changes.extend(inserted),
                Err(error) => {
                    log::warn!("физика не создала тела активированных объектов: {error}");
                }
            }
        }
    }
    activity_audio_requests(world, &hidden, &shown)
}

/// Применяет spawn и destroy, закоммиченные скриптами в фазе: обновляет GPU-таблицу, физику и
/// звук. Тела удаляются и вставляются до `sync_world_commands`, поэтому позы новых тел не дают
/// ложных телепортаций. Возвращает корни созданных объектов для сетевого моста.
pub(crate) fn apply_world_changes(
    world: &mut World,
    scripts: &mut ScriptHost,
    mut physics: Option<&mut PhysicsSceneSync>,
    spawn_physics: &mut SpawnPhysics,
    gpu_scene: &mut GpuSceneTable,
    prefabs: &mut PrefabLoads,
    mut audio: Option<&mut engine_audio::AudioBridge>,
) -> Vec<SpawnedRoot> {
    let activity_requests = apply_activity_changes(world, physics.as_deref_mut(), spawn_physics);
    if let Some(audio) = audio.as_deref_mut()
        && !activity_requests.is_empty()
    {
        audio.apply(activity_requests, world);
    }
    let mut roots = Vec::new();
    // Выгрузка может сразу запросить новую загрузку (отложенный spawn), поэтому заявки
    // вычитываются, пока очередь не опустеет.
    loop {
        let changes = scripts.drain_world_changes();
        if changes.spawned.is_empty()
            && changes.despawned.is_empty()
            && changes.prefab_requests.is_empty()
            && changes.scene_replaced.is_none()
        {
            return roots;
        }
        roots.extend(changes.spawned.iter().map(|spawned| SpawnedRoot {
            handle: spawned.handle,
            prefab: spawned.prefab.clone(),
        }));
        apply_changes(
            world,
            scripts,
            physics.as_deref_mut(),
            spawn_physics,
            gpu_scene,
            prefabs,
            audio.as_deref_mut(),
            changes,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_changes(
    world: &mut World,
    scripts: &mut ScriptHost,
    mut physics: Option<&mut PhysicsSceneSync>,
    spawn_physics: &mut SpawnPhysics,
    gpu_scene: &mut GpuSceneTable,
    prefabs: &mut PrefabLoads,
    audio: Option<&mut engine_audio::AudioBridge>,
    changes: WorldChanges,
) {
    let mut requests = Vec::new();
    for spawned in &changes.spawned {
        if let Some(templates) = gpu_scene.prefabs.get(&spawned.prefab) {
            attach_prefab_levels(&mut gpu_scene.by_object, templates, spawned);
        }
        attach_prefab_material_sources(gpu_scene, spawned);
    }
    detach_object_render_data(gpu_scene, &changes.despawned);
    if !changes.despawned.is_empty() {
        prefabs.send_particles(ParticleRequest::Destroy(changes.despawned.clone()));
    }
    requests.extend(
        changes
            .despawned
            .iter()
            .map(|&object| AudioRequest::StopSource { object, fade: 0.0 }),
    );
    if let Some(sync) = physics.as_deref_mut() {
        spawn_physics
            .changes
            .extend(sync.remove_objects(&changes.despawned));
    }
    for spawned in &changes.spawned {
        // Объект, созданный и удалённый в одной фазе, уже не жив: тело и звук ему не нужны.
        if !world.is_alive(spawned.handle) {
            continue;
        }
        if let Some(sync) = physics.as_deref_mut() {
            // Объект, который активация этого кадра уже вставила, второе тело не получает.
            let fresh = sync.unbound_objects(&spawned.objects);
            let inserted = sync.insert_objects(
                world,
                &fresh,
                &spawn_physics.geometry,
                &mut spawn_physics.shapes,
            );
            match inserted {
                Ok(inserted) => {
                    spawn_physics.changes.extend(inserted);
                    if let Some(velocity) = spawned.initial_velocity
                        && let Some(body) = root_body(sync, spawned.handle)
                    {
                        spawn_physics
                            .velocities
                            .push(PhysicsCommand::SetVelocity { body, velocity });
                    }
                }
                Err(error) => {
                    // Откат снимает и экземпляры скриптов; Error из него печатается сразу.
                    scripts.revert_spawn(
                        world,
                        spawned.handle,
                        &format!("физика отклонила объект {:?}: {error}", spawned.prefab),
                    );
                    print_script_events(scripts.drain_events());
                    detach_object_render_data(gpu_scene, &spawned.objects);
                    continue;
                }
            }
        }
        if let Some(descs) = gpu_scene.particle_templates.get(&spawned.prefab) {
            for request in spawn_particle_requests(&spawned.objects, descs) {
                prefabs.send_particles(request);
            }
        }
        requests.extend(
            spawned
                .objects
                .iter()
                .filter(|&&object| {
                    world
                        .audio_source(object)
                        .is_some_and(|audio| audio.play_on_start)
                })
                .map(|&object| AudioRequest::PlaySource {
                    object,
                    voice: VoiceId::allocate(),
                }),
        );
    }
    if let Some(replacement) = &changes.scene_replaced {
        apply_scene_added(world, &replacement.added, &mut requests);
    }
    apply_prefab_requests(
        world,
        scripts,
        spawn_physics,
        gpu_scene,
        prefabs,
        &changes.prefab_requests,
    );
    if let Some(audio) = audio {
        audio.apply(requests, world);
    }
}

/// Звук объектов, добавленных заменой сцены: источники с `play_on_start`. Физика вставляется в
/// коммите замены (`SceneSwitch::commit`), удалённые объекты уже обработаны по `despawned`.
pub(crate) fn apply_scene_added(
    world: &World,
    added: &[ObjectHandle],
    requests: &mut Vec<AudioRequest>,
) {
    requests.extend(
        added
            .iter()
            .copied()
            .filter(|&object| world.is_alive(object))
            .filter(|&object| {
                world
                    .audio_source(object)
                    .is_some_and(|audio| audio.play_on_start)
            })
            .map(|object| AudioRequest::PlaySource {
                object,
                voice: VoiceId::allocate(),
            }),
    );
}

/// Загрузка и выгрузка тяжёлых префабов. Выгрузка снимает GPU-уровни и геометрию, отправляет
/// освобождение и сообщает хосту, что ресурсы ушли.
pub(crate) fn apply_prefab_requests(
    world: &World,
    scripts: &mut ScriptHost,
    spawn_physics: &mut SpawnPhysics,
    gpu_scene: &mut GpuSceneTable,
    prefabs: &mut PrefabLoads,
    requests: &[PrefabRequest],
) {
    for request in requests {
        match request {
            PrefabRequest::Load(path) => {
                if let Err(error) = prefabs.start_load(path) {
                    scripts.prefab_failed(world, path, &error);
                }
            }
            PrefabRequest::Unload(path) => {
                gpu_scene.prefabs.remove(path);
                gpu_scene.particle_templates.remove(path);
                gpu_scene.material_templates.remove(path);
                gpu_scene.prefab_tokens.remove(path);
                if let Some(slot) = prefabs.forget(path) {
                    if let Some(geometry) = slot.geometry {
                        spawn_physics.remove_heavy_geometry(&geometry);
                    }
                    prefabs.release(slot.token);
                }
                scripts.prefab_released(path);
            }
        }
    }
}
