//! Замена сцены во время игры на scripts thread.
//!
//! Путь запроса: `request` загружает и проверяет новую сцену (включая пробную сборку физики)
//! до каких-либо изменений мира и отправляет GPU-заявку. Ответ render thread принимается только
//! для текущего поколения; `commit` вызывает `ScriptHost::replace_scene`, который проверяет
//! совпадение id и параметров до разрушения мира. Тела добавленных объектов привязываются к физике
//! сразу после замены в том же `commit`; удалённые объекты и звук добавленных идут дальше по
//! `apply_world_changes`. GPU-ресурсы, которыми больше не
//! владеет ни один живой объект, освобождаются render thread после снимка с этого кадра.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Arc;

use engine_animation::AnimationBridge;
use engine_assets::Assets;
use engine_project::PhysicsSettings;
use engine_render::{ColorLutId, MaterialId, MeshId, ScreenShaderId, SkinId};
use engine_resources::ResourceHandle;
use engine_scene::{ObjectHandle, World};
use engine_script::{ScriptHost, ScriptTime};

use crate::game_ui::{PreparedUi, UiRegistry};
use crate::pipeline::{GpuSceneTable, SpawnPhysics, detach_object_render_data};
use crate::prefabs::{ParticleRequest, PrefabLoads, RenderResourceRequest};
use crate::scene::{
    PreparedPrefabs, PreparedRenderAssets, PreparedScene, color_lut_paths, load_color_luts,
    load_scene, load_screen_shader_sources, screen_shader_paths,
};

/// Загрузчик сцен проекта: читает `.rvescene` и ассеты тем же `ResourceHandle`, что и старт.
#[derive(Clone)]
pub(crate) struct SceneLoader {
    resources: ResourceHandle,
    physics: PhysicsSettings,
    /// Исходники экранных шейдеров и LUT нужны только GPU-клиенту, как при старте.
    screen_shaders: bool,
}

impl SceneLoader {
    pub(crate) fn new(
        resources: ResourceHandle,
        physics: PhysicsSettings,
        screen_shaders: bool,
    ) -> Self {
        Self {
            resources,
            physics,
            screen_shaders,
        }
    }

    /// Готовит сцену и пробно строит её физику, как при старте. Возвращает геометрию моделей для
    /// коллайдеров. Ошибка не меняет ни мир, ни ресурсы текущей сцены.
    fn prepare(
        &self,
        path: &str,
    ) -> Result<(PreparedScene, Vec<engine_physics::ModelGeometry>), String> {
        let assets = Assets::with_resources(self.resources.clone());
        let mut prepared = load_scene(&self.resources, &assets, path)?;
        if self.screen_shaders {
            prepared.render_assets.screen_shaders =
                load_screen_shader_sources(&self.resources, &prepared.world);
            prepared.render_assets.color_luts = load_color_luts(&self.resources, &prepared.world);
        }
        let geometry = prepared.physics_geometry();
        prepared.build_physics(&geometry, &self.physics)?;
        Ok((prepared, geometry))
    }
}

/// Заявка render thread: GPU-ресурсы новой сцены для поколения `generation`.
pub(crate) struct SceneUpload {
    pub generation: u64,
    pub assets: Arc<PreparedRenderAssets>,
}

/// Ответ render thread на [`SceneUpload`]: таблица GPU с ключами по хендлам объектов сцены до
/// замены (уточняются при коммите) либо ошибка загрузки.
#[derive(Debug)]
pub(crate) struct SceneUploadReply {
    pub generation: u64,
    pub result: Result<Box<GpuSceneTable>, String>,
}

/// GPU-ресурсы загруженной сцены, которые освобождаются через render thread целиком.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SceneGpuIds {
    pub meshes: HashSet<MeshId>,
    pub materials: HashSet<MaterialId>,
    pub skins: HashSet<SkinId>,
    pub screen_shaders: HashSet<ScreenShaderId>,
    pub color_luts: HashSet<ColorLutId>,
    pub ui_scene_owners: HashSet<u64>,
    /// Scene generations that own cached particle textures.
    pub cache_owners: HashSet<u64>,
}

impl SceneGpuIds {
    /// Меши, материалы, скиннинг и экранные шейдеры, на которые ссылается таблица: уровни объектов,
    /// шаблоны префабов сцены, экземпляры skinning и шейдеры камер.
    pub(crate) fn of_table(table: &GpuSceneTable) -> Self {
        let mut ids = Self::default();
        for levels in table
            .by_object
            .values()
            .chain(table.prefabs.values().flatten())
        {
            for level in levels {
                for (mesh, material) in level {
                    ids.meshes.insert(*mesh);
                    ids.materials.insert(*material);
                }
            }
        }
        for instances in table.skin_instances.values() {
            ids.skins.extend(instances.iter().copied());
        }
        ids.screen_shaders
            .extend(table.screen_shaders.values().copied());
        ids.color_luts.extend(table.color_luts.values().copied());
        ids.ui_scene_owners
            .extend(table.ui_scene_owners.iter().copied());
        ids.cache_owners
            .extend(table.particle_texture_owners.values().copied());
        ids.cache_owners
            .extend(table.particle_template_owners.values().copied());
        ids
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.meshes.is_empty()
            && self.materials.is_empty()
            && self.skins.is_empty()
            && self.screen_shaders.is_empty()
            && self.color_luts.is_empty()
            && self.ui_scene_owners.is_empty()
            && self.cache_owners.is_empty()
    }

    fn extend(&mut self, other: &Self) {
        self.meshes.extend(other.meshes.iter().copied());
        self.materials.extend(other.materials.iter().copied());
        self.skins.extend(other.skins.iter().copied());
        self.screen_shaders
            .extend(other.screen_shaders.iter().copied());
        self.color_luts.extend(other.color_luts.iter().copied());
        self.ui_scene_owners
            .extend(other.ui_scene_owners.iter().copied());
        self.cache_owners.extend(other.cache_owners.iter().copied());
    }

    /// Ресурсы этого набора, которых нет в `live`.
    pub(crate) fn minus(&self, live: &Self) -> Self {
        Self {
            meshes: difference(&self.meshes, &live.meshes),
            materials: difference(&self.materials, &live.materials),
            skins: difference(&self.skins, &live.skins),
            screen_shaders: difference(&self.screen_shaders, &live.screen_shaders),
            color_luts: difference(&self.color_luts, &live.color_luts),
            ui_scene_owners: difference(&self.ui_scene_owners, &live.ui_scene_owners),
            cache_owners: difference(&self.cache_owners, &live.cache_owners),
        }
    }

    fn intersect(&self, live: &Self) -> Self {
        Self {
            meshes: overlap(&self.meshes, &live.meshes),
            materials: overlap(&self.materials, &live.materials),
            skins: overlap(&self.skins, &live.skins),
            screen_shaders: overlap(&self.screen_shaders, &live.screen_shaders),
            color_luts: overlap(&self.color_luts, &live.color_luts),
            ui_scene_owners: overlap(&self.ui_scene_owners, &live.ui_scene_owners),
            cache_owners: overlap(&self.cache_owners, &live.cache_owners),
        }
    }
}

/// Элементы `retired`, которых нет в `live`.
fn difference<T: Copy + Eq + Hash>(retired: &HashSet<T>, live: &HashSet<T>) -> HashSet<T> {
    retired.difference(live).copied().collect()
}

/// Элементы, которые есть и в `owned`, и в `live`.
fn overlap<T: Copy + Eq + Hash>(owned: &HashSet<T>, live: &HashSet<T>) -> HashSet<T> {
    owned.intersection(live).copied().collect()
}

/// CPU-часть подготовленной сцены, которая ждёт ответа render thread.
struct PendingScene {
    path: String,
    world: World,
    sources: BTreeMap<String, String>,
    prefabs: PreparedPrefabs,
    ui: PreparedUi,
    physics_geometry: Vec<engine_physics::ModelGeometry>,
}

/// Всё, что нужно для применения замены на scripts thread.
pub(crate) struct SceneCommit<'a> {
    pub world: &'a mut World,
    pub scripts: &'a mut ScriptHost,
    pub gpu_scene: &'a mut GpuSceneTable,
    pub spawn_physics: &'a mut SpawnPhysics,
    /// Физика сцены; `None` без физики (headless без тел).
    pub physics: Option<&'a mut engine_physics::PhysicsSceneSync>,
    pub prefabs: &'a mut PrefabLoads,
    pub audio: Option<&'a mut engine_audio::AudioBridge>,
    pub animation: &'a mut AnimationBridge,
    pub ui_registry: &'a mut UiRegistry,
    pub time: ScriptTime,
    /// Номер кадра: освобождение ждёт снимка с этим номером.
    pub sequence: u64,
}

impl SceneCommit<'_> {
    fn release(&self, ids: SceneGpuIds, last_seq: u64) {
        if !ids.is_empty() {
            self.prefabs
                .send_render(RenderResourceRequest::ReleaseScene {
                    ids: Box::new(ids),
                    last_seq,
                });
        }
    }
}

/// Состояние замены сцены на scripts thread.
#[derive(Default)]
pub(crate) struct SceneSwitch {
    /// Поколение последней заявки. Ответы более старых поколений отбрасываются.
    generation: u64,
    /// Заявка, которая ждёт GPU-ответа; новая заявка её заменяет.
    awaiting: Option<(u64, PendingScene)>,
    replies: VecDeque<SceneUploadReply>,
    /// GPU-ресурсы, которыми владеют загруженные сцены и которые ещё не освобождены.
    owned: SceneGpuIds,
}

impl SceneSwitch {
    /// `initial` — GPU-ресурсы стартовой сцены.
    pub(crate) fn new(initial: SceneGpuIds) -> Self {
        Self {
            owned: initial,
            ..Self::default()
        }
    }

    /// Принимает запрос `rve.scene.load` на загрузку `path`. Успешная новая заявка отменяет
    /// ожидающую замену: её ответ придёт с устаревшим поколением. Ошибка подготовки оставляет
    /// предыдущую ожидающую замену и текущую сцену без изменений.
    pub(crate) fn request(
        &mut self,
        loader: Option<&SceneLoader>,
        path: &str,
    ) -> Result<SceneUpload, String> {
        let loader = loader.ok_or_else(|| "загрузка сцен не подключена".to_owned())?;
        let (prepared, physics_geometry) = loader.prepare(path)?;
        self.generation += 1;
        let PreparedScene {
            world,
            sources,
            render_assets,
            prefabs,
            ui,
            ..
        } = prepared;
        self.awaiting = Some((
            self.generation,
            PendingScene {
                path: path.to_owned(),
                world,
                sources,
                prefabs,
                ui,
                physics_geometry,
            },
        ));
        Ok(SceneUpload {
            generation: self.generation,
            assets: Arc::new(render_assets),
        })
    }

    /// Принимает ответ render thread; применяется в [`Self::apply`].
    pub(crate) fn accept(&mut self, reply: SceneUploadReply) {
        self.replies.push_back(reply);
    }

    /// Применяет ответы текущего поколения. Возвращает ошибки как пары (путь, сообщение).
    /// Ответы устаревших поколений и ошибки после загрузки освобождают свои GPU-ресурсы сразу.
    pub(crate) fn apply(&mut self, mut ctx: SceneCommit<'_>) -> Vec<(String, String)> {
        let mut failures = Vec::new();
        while let Some(reply) = self.replies.pop_front() {
            let current = matches!(
                &self.awaiting,
                Some((generation, _)) if *generation == reply.generation
            );
            let table = match reply.result {
                Ok(table) => table,
                Err(error) => {
                    if current && let Some((_, pending)) = self.awaiting.take() {
                        failures.push((pending.path, error));
                    }
                    continue;
                }
            };
            let ids = SceneGpuIds::of_table(&table);
            if !current {
                let live = SceneGpuIds::of_table(ctx.gpu_scene);
                ctx.release(ids.minus(&live), 0);
                continue;
            }
            let Some((_, pending)) = self.awaiting.take() else {
                let live = SceneGpuIds::of_table(ctx.gpu_scene);
                ctx.release(ids.minus(&live), 0);
                continue;
            };
            let path = pending.path.clone();
            if let Err(error) = self.commit(&mut ctx, pending, *table) {
                let live = SceneGpuIds::of_table(ctx.gpu_scene);
                ctx.release(ids.minus(&live), 0);
                failures.push((path, error));
            }
        }
        failures
    }

    /// Применяет замену. Все отказы проверок происходят до `replace_scene`, поэтому при ошибке
    /// мир, ресурсы и хост остаются прежними.
    fn commit(
        &mut self,
        ctx: &mut SceneCommit<'_>,
        pending: PendingScene,
        mut table: GpuSceneTable,
    ) -> Result<(), String> {
        let new_ids = SceneGpuIds::of_table(&table);
        // Префабы и UI проверяются до замены. UI-реестр не меняется, пока host не принял сцену.
        let scene_prefabs = ctx
            .scripts
            .check_scene_prefabs(
                &pending.world,
                &pending.prefabs.catalog,
                &pending.prefabs.templates,
                &pending.sources,
            )
            .map_err(|error| error.to_string())?;
        // Столкновение id отсекается до регистрации UI, чтобы отказ не оставил документы сцены.
        ctx.world
            .check_scene_replacement(&pending.world)
            .map_err(|error| error.to_string())?;
        let ui_registration = ctx.ui_registry.prepare(&pending.ui)?;

        let scene_objects: Vec<ObjectHandle> = pending
            .world
            .objects()
            .iter()
            .map(|object| object.handle)
            .collect();
        let replacement = ctx.scripts.replace_scene(
            ctx.world,
            pending.world,
            &pending.sources,
            &pending.path,
            ctx.time,
        )?;
        ctx.ui_registry.commit(ctx.scripts, ui_registration);
        ctx.scripts.apply_scene_prefabs(scene_prefabs);

        // Изменения ниже не имеют отказов: мир уже заменён. Удалённые объекты, физика и звук
        // добавленных обрабатываются в `apply_world_changes` по `WorldChanges`.
        let handles: HashMap<ObjectHandle, ObjectHandle> = scene_objects
            .into_iter()
            .zip(replacement.added.iter().copied())
            .collect();
        ctx.spawn_physics
            .add_scene_geometry(pending.physics_geometry);
        // Тела новых объектов привязываются здесь, а не после коммита, поэтому отказ вставки
        // не может оставить мир уже замененным. Отказа не бывает: пробная сборка физики в
        // `SceneLoader::prepare` вызывает ту же `scene_body` и `insert_body` для тех же объектов
        // сцены, а новые хендлы никогда не привязаны. Поэтому вставка здесь только `expect`.
        if let Some(sync) = ctx.physics.as_deref_mut() {
            ctx.spawn_physics
                .insert_scene_bodies(sync, &*ctx.world, &replacement.added)
                .expect("scene bodies were validated by the trial physics build");
        }
        let gpu = &mut *ctx.gpu_scene;
        // Удалённые объекты: их отображение, материалы и скиннинг уходят из таблицы до расчёта живых
        // ресурсов ниже, иначе они остались бы живыми до следующей замены.
        detach_object_render_data(gpu, &replacement.removed);
        for object in &replacement.removed {
            gpu.skin_instances.remove(object);
            gpu.joint_counts.remove(object);
        }
        gpu.by_object.extend(rekey(&mut table.by_object, &handles));
        gpu.material_sources
            .extend(rekey(&mut table.material_sources, &handles));
        gpu.skin_instances
            .extend(rekey(&mut table.skin_instances, &handles));
        gpu.particle_texture_owners
            .extend(rekey(&mut table.particle_texture_owners, &handles));
        gpu.joint_counts
            .extend(rekey(&mut table.joint_counts, &handles));
        for (scene_object, desc) in table.particle_descs.drain() {
            if let Some(&object) = handles.get(&scene_object) {
                ctx.prefabs.send_particles(ParticleRequest::Create {
                    object,
                    desc: Box::new(desc),
                });
            }
        }
        // Быстрые префабы резидентны: путь, который уже зарегистрировала прежняя сцена, сохраняет
        // свои уровни и шаблоны, как и шаблон на scripts thread, а spawn из постоянных объектов
        // продолжает его видеть. Новые записи того же пути не используются: их GPU-ресурсы не
        // входят в `live` и освобождаются ниже.
        retain_resident(&mut gpu.prefabs, &mut table.prefabs);
        retain_resident(&mut gpu.particle_templates, &mut table.particle_templates);
        retain_resident(
            &mut gpu.particle_template_owners,
            &mut table.particle_template_owners,
        );
        retain_resident(&mut gpu.material_templates, &mut table.material_templates);
        gpu.environments.extend(table.environments);
        gpu.animation_models.extend(table.animation_models);
        gpu.animation_clips.extend(table.animation_clips);
        // Экранные шейдеры живут по путям камер мира: путь, который не нужен ни одной камере, уходит
        // из таблицы. Путь новой сцены заменяет прежний шейдер; прежний освободится ниже как неживой.
        let shader_paths = screen_shader_paths(ctx.world);
        gpu.screen_shaders
            .retain(|path, _| shader_paths.contains(path));
        gpu.screen_shaders.extend(table.screen_shaders);
        // LUT живут по путям постобработки камер мира по тому же правилу, что экранные шейдеры.
        let lut_paths = color_lut_paths(ctx.world);
        gpu.color_luts.retain(|path, _| lut_paths.contains(path));
        gpu.color_luts.extend(table.color_luts);
        gpu.ui_scene_owners.extend(table.ui_scene_owners);

        ctx.animation.replace_objects(
            ctx.world,
            &replacement.removed,
            &replacement.added,
            |path| {
                gpu.animation_models
                    .get(path.as_str())
                    .cloned()
                    .ok_or_else(|| format!("модель {} не подготовлена для анимации", path.as_str()))
            },
            |path| {
                gpu.animation_clips
                    .get(path.as_str())
                    .cloned()
                    .ok_or_else(|| format!("клип {} не загружен", path.as_str()))
            },
        );
        crate::pipeline::log_animation_warnings(ctx.animation);
        crate::pipeline::drop_mismatched_skins(ctx.world, ctx.gpu_scene, ctx.animation);
        if let Some(audio) = ctx.audio.as_deref_mut() {
            audio.scene_replaced(ctx.world);
        }

        // Освобождаем то, на что больше не ссылается ни один объект. Ресурсы прежних сцен,
        // на которые ссылаются живые объекты, сохраняются до их уничтожения.
        self.owned.extend(&new_ids);
        let live = SceneGpuIds::of_table(ctx.gpu_scene);
        let released = self.owned.minus(&live);
        self.owned = self.owned.intersect(&live);
        ctx.release(released, ctx.sequence);
        Ok(())
    }
}

/// Добавляет в `target` записи `incoming`, которых там ещё нет. Существующая запись побеждает.
fn retain_resident<V>(target: &mut HashMap<String, V>, incoming: &mut HashMap<String, V>) {
    for (path, value) in incoming.drain() {
        target.entry(path).or_insert(value);
    }
}

/// Переносит значения по хендлам сцены на финальные хендлы объектов замены.
fn rekey<V>(
    values: &mut HashMap<ObjectHandle, V>,
    handles: &HashMap<ObjectHandle, ObjectHandle>,
) -> HashMap<ObjectHandle, V> {
    std::mem::take(values)
        .into_iter()
        .filter_map(|(scene_object, value)| {
            handles.get(&scene_object).map(|&object| (object, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{SceneGpuIds, difference, overlap};
    use std::collections::HashSet;

    fn set(values: &[usize]) -> HashSet<usize> {
        values.iter().copied().collect()
    }

    #[test]
    fn difference_keeps_only_ids_that_no_live_object_references() {
        assert_eq!(
            difference(&set(&[1, 2, 3]), &set(&[2, 4])),
            set(&[1, 3]),
            "освобождаются только неиспользуемые ресурсы"
        );
        assert!(difference(&set(&[1]), &set(&[1, 2])).is_empty());
    }

    #[test]
    fn overlap_keeps_resources_that_stay_referenced_after_a_switch() {
        assert_eq!(overlap(&set(&[1, 2]), &set(&[2, 4])), set(&[2]));
        assert!(overlap(&set(&[1]), &set(&[2])).is_empty());
    }

    #[test]
    fn scene_ui_owners_retire_by_generation() {
        let mut old = SceneGpuIds::default();
        old.ui_scene_owners.insert(4);
        let mut new = SceneGpuIds::default();
        new.ui_scene_owners.insert(5);

        assert_eq!(old.minus(&new).ui_scene_owners, HashSet::from([4]));
        assert!(old.intersect(&new).ui_scene_owners.is_empty());
    }
}

#[cfg(test)]
mod commit_tests {
    use super::*;
    use engine_render::{ColorLutId, ScreenShaderId};
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_script::{PrefabStatus, ScriptFrameTime, ScriptLoadOptions};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PLAYER: &str = r#"
local Script = {}
function Script:on_start()
  rve.log("player start")
end
function Script:on_destroy()
  rve.log("player destroy")
end
return Script
"#;

    const ENEMY: &str = r#"
local Script = {}
function Script:on_destroy()
  rve.log("enemy destroy")
end
return Script
"#;

    const BOSS: &str = r#"
local Script = {}
function Script:on_start()
  rve.log("boss start")
end
return Script
"#;

    const UI_PROBE: &str = r#"
local Script = {}
function Script:on_update()
  self.step = (self.step or 0) + 1
  if self.step == 1 then
    local ok = pcall(rve.ui.open, "ui/existing.rveui")
    rve.log_info("existing ui " .. tostring(ok))
  elseif self.step == 2 then
    local ok = pcall(rve.ui.open, "ui/new.rveui")
    rve.log_info("new ui " .. tostring(ok))
    local existing = pcall(rve.ui.open, "ui/existing.rveui")
    rve.log_info("existing ui after " .. tostring(existing))
  end
end
return Script
"#;

    const START_SCENE: &str = "[[assets]]\npath = \"scripts/player.lua\"\n[[assets]]\npath = \"scripts/enemy.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"player\"\nkeep_on_load = true\n[[objects.lite_scripts]]\npath = \"scripts/player.lua\"\n[[objects]]\nid = \"enemy\"\n[[objects.lite_scripts]]\npath = \"scripts/enemy.lua\"\n";

    const BOSS_SCENE: &str = "[[assets]]\npath = \"scripts/boss.lua\"\n[[objects]]\nid = \"camera_b\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"boss\"\n[[objects.lite_scripts]]\npath = \"scripts/boss.lua\"\n";

    const CRATE_SCENE: &str = "[[objects]]\nid = \"camera_c\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"crate\"\n";

    /// Сцена без игрока: возврат в неё не конфликтует с сохраняемым объектом `player`.
    const ENEMY_SCENE: &str = "[[assets]]
path = \"scripts/enemy.lua\"
[[objects]]
id = \"camera_e\"
[objects.camera]
active = true
[[objects]]
id = \"enemy\"
[[objects.lite_scripts]]
path = \"scripts/enemy.lua\"
";

    const CLASH_SCENE: &str = "[[objects]]\nid = \"camera_x\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"player\"\n";

    const UI_ATOMICITY_SCENE: &str = "[[assets]]\npath = \"scripts/ui_probe.lua\"\n[[assets]]\npath = \"ui/existing.rveui\"\n[[objects]]\nid = \"camera_ui\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"ui_probe\"\n[[objects.lite_scripts]]\npath = \"scripts/ui_probe.lua\"\n";
    const UI_REPLACEMENT_FAILURE_SCENE: &str = "[[assets]]\npath = \"ui/new.rveui\"\n[[objects]]\nid = \"camera_new_ui\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"bad_ui_script\"\n[[objects.heavy_scripts]]\nname = \"MissingUiScript\"\n";
    const UI_EXISTING: &str = "[[assets]]\npath = \"ui/existing.png\"\n[canvas]\n";
    const UI_NEW: &str = "[[assets]]\npath = \"ui/new.png\"\n[canvas]\n";
    const TEST_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248, 255, 255, 255,
        127, 0, 9, 251, 3, 253, 42, 134, 227, 138, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    /// Сохраняемый `hub` спавнит быстрые префабы сцены `pb` после перехода в неё.
    const HUB: &str = r#"
local Script = {}
function Script:on_update(dt)
  if self.done or rve.scene.current() ~= "levels/pb.rvescene" then
    return
  end
  self.done = true
  rve.spawn("prefabs/x.rveprefab", { id = "spawned_x" })
  rve.spawn("prefabs/y.rveprefab", { id = "spawned_y" })
end
return Script
"#;

    const PREFAB_X: &str = "[[objects]]\nid = \"root\"\n";
    const PREFAB_Y: &str = "[[objects]]\nid = \"root\"\n";
    /// Префаб, чей скрипт не компилируется: переход в сцену с ним должен полностью отказать.
    const PREFAB_Z: &str = "[[assets]]\npath = \"scripts/broken.lua\"\n[[objects]]\nid = \"root\"\n[[objects.lite_scripts]]\npath = \"scripts/broken.lua\"\n";

    /// Стартовая сцена с общим префабом `x` и сохраняемым `hub`.
    const PREFAB_SCENE_A: &str = "[[assets]]\npath = \"scripts/hub.lua\"\n[[assets]]\npath = \"prefabs/x.rveprefab\"\n\
[[objects]]\nid = \"camera_a\"\n[objects.camera]\nactive = true\n\
[[objects]]\nid = \"hub\"\nkeep_on_load = true\n[[objects.lite_scripts]]\npath = \"scripts/hub.lua\"\n\
[[objects]]\nid = \"a_crate\"\n";

    /// Сцена с новым префабом `y` и тем же `x`.
    const PREFAB_SCENE_B: &str = "[[assets]]\npath = \"prefabs/x.rveprefab\"\n[[assets]]\npath = \"prefabs/y.rveprefab\"\n\
[[objects]]\nid = \"camera_b\"\n[objects.camera]\nactive = true\n\
[[objects]]\nid = \"b_crate\"\n";

    /// Сцена без `hub`: возврат в неё после `pb` снова использует уже резидентный `x`.
    const PREFAB_SCENE_C: &str = "[[assets]]\npath = \"prefabs/x.rveprefab\"\n\
[[objects]]\nid = \"camera_c\"\n[objects.camera]\nactive = true\n\
[[objects]]\nid = \"c_crate\"\n";

    /// Сцена с коллайдером: его тело вставляется физикой сразу при замене.
    const PHYSICS_SCENE: &str = "[[objects]]\nid = \"camera_p\"\n[objects.camera]\nactive = true\n\
[[objects]]\nid = \"ball\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.25\n";
    /// Сцена с камерой и экранным шейдером `fx/a`; `fx_c` объявляет тот же путь.
    const FX_A_SCENE: &str = "[[assets]]\npath = \"fx/a.glsl\"\n[[objects]]\nid = \"camera_fa\"\n[objects.camera]\nactive = true\n[[objects.camera.screen_shaders]]\nshader = \"/fx/a.glsl\"\nstage = \"after_post\"\n";
    const FX_B_SCENE: &str = "[[assets]]\npath = \"fx/b.glsl\"\n[[objects]]\nid = \"camera_fb\"\n[objects.camera]\nactive = true\n[[objects.camera.screen_shaders]]\nshader = \"/fx/b.glsl\"\nstage = \"after_post\"\n";
    /// Стартовая сцена с сохраняемой камерой `lens`, чей шейдер переживает замену.
    const FX_HUB_SCENE: &str = "[[assets]]\npath = \"fx/hub.glsl\"\n[[assets]]\npath = \"fx/lens.glsl\"\n[[objects]]\nid = \"camera_fh\"\n[objects.camera]\nactive = true\n[[objects.camera.screen_shaders]]\nshader = \"/fx/hub.glsl\"\nstage = \"after_post\"\n[[objects]]\nid = \"lens\"\nkeep_on_load = true\n[objects.camera]\nactive = false\n[[objects.camera.screen_shaders]]\nshader = \"/fx/lens.glsl\"\nstage = \"after_post\"\n";
    /// Сцена с `.rveui`, который не разбирается: переход в неё отказывает до любых изменений.
    const BAD_UI_SCENE: &str = "[[assets]]\npath = \"ui/bad.rveui\"\n[[objects]]\nid = \"camera_u\"\n[objects.camera]\nactive = true\n";

    /// Сцена с префабом `z`, чей скрипт не компилируется.
    const PREFAB_SCENE_BROKEN: &str = "[[assets]]\npath = \"prefabs/z.rveprefab\"\n\
[[objects]]\nid = \"camera_bad\"\n[objects.camera]\nactive = true\n\
[[objects]]\nid = \"bad_crate\"\n";

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    /// Временный каталог с файлами сцен и скриптов; удаляется вместе с ригом.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let index = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rve-scene-switch-{}-{index}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn write_bytes(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn root(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Всё, что нужно для замены сцены на scripts thread в одном процессе без GPU.
    struct Rig {
        _dir: TempDir,
        _resources: ResourceSystem,
        loader: SceneLoader,
        world: World,
        host: ScriptHost,
        gpu: GpuSceneTable,
        spawn_physics: SpawnPhysics,
        physics: Option<engine_physics::PhysicsSceneSync>,
        prefabs: PrefabLoads,
        render_rx: flume::Receiver<RenderResourceRequest>,
        animation: AnimationBridge,
        ui_registry: UiRegistry,
        switch: SceneSwitch,
        frame: u64,
    }

    impl Rig {
        fn new() -> Self {
            Self::start("levels/a.rvescene")
        }

        /// Стартовая сцена поднимается как при запуске: быстрые префабы попадают в каталог хоста
        /// и регистрируются по шаблонам сцены.
        fn start(start: &str) -> Self {
            let dir = TempDir::new();
            dir.write("levels/a.rvescene", START_SCENE);
            dir.write("levels/b.rvescene", BOSS_SCENE);
            dir.write("levels/c.rvescene", CRATE_SCENE);
            dir.write("levels/clash.rvescene", CLASH_SCENE);
            dir.write("levels/ui_atomicity.rvescene", UI_ATOMICITY_SCENE);
            dir.write(
                "levels/ui_replacement_failure.rvescene",
                UI_REPLACEMENT_FAILURE_SCENE,
            );
            dir.write("levels/e.rvescene", ENEMY_SCENE);
            dir.write("levels/pa.rvescene", PREFAB_SCENE_A);
            dir.write("levels/pb.rvescene", PREFAB_SCENE_B);
            dir.write("levels/pbad.rvescene", PREFAB_SCENE_BROKEN);
            dir.write("levels/pc.rvescene", PREFAB_SCENE_C);
            dir.write("levels/pphys.rvescene", PHYSICS_SCENE);
            dir.write("levels/fx_a.rvescene", FX_A_SCENE);
            dir.write("levels/fx_b.rvescene", FX_B_SCENE);
            dir.write("levels/fx_c.rvescene", FX_A_SCENE);
            dir.write("levels/fx_hub.rvescene", FX_HUB_SCENE);
            dir.write("levels/bad_ui.rvescene", BAD_UI_SCENE);
            dir.write("ui/bad.rveui", "[canvas\n");
            dir.write("ui/existing.rveui", UI_EXISTING);
            dir.write("ui/new.rveui", UI_NEW);
            dir.write_bytes("ui/existing.png", TEST_PNG);
            dir.write_bytes("ui/new.png", TEST_PNG);
            dir.write("scripts/player.lua", PLAYER);
            dir.write("scripts/enemy.lua", ENEMY);
            dir.write("scripts/boss.lua", BOSS);
            dir.write("scripts/ui_probe.lua", UI_PROBE);
            dir.write("scripts/hub.lua", HUB);
            dir.write("scripts/broken.lua", "return {");
            dir.write("prefabs/x.rveprefab", PREFAB_X);
            dir.write("prefabs/y.rveprefab", PREFAB_Y);
            dir.write("prefabs/z.rveprefab", PREFAB_Z);
            let resources = ResourceSystem::start(ResourceConfig {
                root: dir.root().to_owned(),
                source_mode: SourceMode::Loose,
                worker_count: 1,
                ..ResourceConfig::default()
            })
            .unwrap();
            let loader = SceneLoader::new(resources.handle(), PhysicsSettings::default(), false);
            let (prepared, geometry) = loader.prepare(start).unwrap();
            let (_, bindings) = prepared
                .build_physics(&geometry, &PhysicsSettings::default())
                .unwrap();
            let physics = engine_physics::PhysicsSceneSync::new(&prepared.world, bindings).unwrap();
            let PreparedScene {
                mut world,
                sources,
                prefabs,
                ui,
                ..
            } = prepared;
            let mut host = ScriptHost::load_with_options(
                &world,
                start,
                &sources,
                None,
                ScriptLoadOptions {
                    prefabs: prefabs.catalog.clone(),
                    ..ScriptLoadOptions::default()
                },
            )
            .unwrap();
            for (path, template) in &prefabs.templates {
                host.register_prefab(&world, path, Arc::clone(template), &sources)
                    .unwrap();
            }
            let mut ui_registry = UiRegistry::default();
            ui_registry.register(&mut host, &ui).unwrap();
            host.start(&mut world, ScriptTime::default());
            let (loader_tx, _loader_rx) = flume::unbounded();
            let (render_tx, render_rx) = flume::unbounded();
            Self {
                _dir: dir,
                _resources: resources,
                loader,
                world,
                host,
                gpu: GpuSceneTable::default(),
                spawn_physics: SpawnPhysics::default(),
                physics: Some(physics),
                prefabs: PrefabLoads::new(loader_tx, render_tx),
                render_rx,
                animation: AnimationBridge::default(),
                ui_registry,
                switch: SceneSwitch::default(),
                frame: 0,
            }
        }

        /// Запрос и ответ render thread без GPU: поведение headless-пути.
        fn load(&mut self, path: &str) -> Result<(), String> {
            let upload = self.switch.request(Some(&self.loader), path)?;
            self.switch.accept(SceneUploadReply {
                generation: upload.generation,
                result: Ok(Box::default()),
            });
            Ok(())
        }

        /// Ответ render thread с таблицей `table` вместо пустой.
        fn load_with_table(&mut self, path: &str, table: GpuSceneTable) -> Result<(), String> {
            let upload = self.switch.request(Some(&self.loader), path)?;
            self.switch.accept(SceneUploadReply {
                generation: upload.generation,
                result: Ok(Box::new(table)),
            });
            Ok(())
        }

        fn commit(&mut self) -> Vec<(String, String)> {
            self.frame += 1;
            self.switch.apply(SceneCommit {
                world: &mut self.world,
                scripts: &mut self.host,
                gpu_scene: &mut self.gpu,
                spawn_physics: &mut self.spawn_physics,
                physics: self.physics.as_mut(),
                prefabs: &mut self.prefabs,
                audio: None,
                animation: &mut self.animation,
                ui_registry: &mut self.ui_registry,
                time: ScriptTime {
                    elapsed: self.frame as f64 / 60.0,
                    frame: self.frame,
                },
                sequence: self.frame,
            })
        }

        /// Подменяет GPU-таблицу и владение ею, как если бы сцена загружена render thread.
        fn adopt_gpu(&mut self, gpu: GpuSceneTable) {
            self.switch = SceneSwitch::new(SceneGpuIds::of_table(&gpu));
            self.gpu = gpu;
        }

        /// Наборы, которые ушли на освобождение render thread с прошлого вызова.
        fn released(&self) -> Vec<SceneGpuIds> {
            self.render_rx
                .try_iter()
                .filter_map(|request| match request {
                    RenderResourceRequest::ReleaseScene { ids, .. } => Some(*ids),
                    _ => None,
                })
                .collect()
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = self
                .world
                .objects()
                .iter()
                .map(|object| object.id.clone())
                .collect();
            names.sort();
            names
        }

        /// Кадр скриптов: как в цикле scripts thread, запускает ожидающие экземпляры.
        fn frame_update(&mut self) {
            self.host.update_frame(
                &mut self.world,
                ScriptFrameTime {
                    dt: 1.0 / 60.0,
                    unscaled_dt: 1.0 / 60.0,
                    elapsed: self.frame as f64 / 60.0,
                    unscaled_elapsed: self.frame as f64 / 60.0,
                    fixed_dt: 1.0 / 60.0,
                    scale: 1.0,
                    ..ScriptFrameTime::default()
                },
            );
        }

        fn messages(&mut self) -> Vec<String> {
            self.host
                .drain_events()
                .into_iter()
                .map(|event| event.message)
                .collect()
        }
    }

    /// Фейковый GPU-id: рендерера в тестах нет, поэтому значение — обёртка над usize.
    fn fake_mesh(value: usize) -> MeshId {
        // SAFETY: MeshId — обёртка над одним usize; любое значение допустимо для теста.
        unsafe { std::mem::transmute(value) }
    }

    fn fake_material(value: usize) -> MaterialId {
        // SAFETY: MaterialId — обёртка над одним usize; любое значение допустимо для теста.
        unsafe { std::mem::transmute(value) }
    }

    fn fake_lut(value: usize) -> ColorLutId {
        // SAFETY: ColorLutId — обёртка над одним usize; любое значение допустимо для теста.
        unsafe { std::mem::transmute(value) }
    }

    fn fake_shader(value: usize) -> ScreenShaderId {
        // SAFETY: ScreenShaderId — обёртка над одним usize; любое значение допустимо для теста.
        unsafe { std::mem::transmute(value) }
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn switch_replaces_nonpersistent_objects_and_keeps_the_persistent_instance() {
        let mut rig = Rig::new();
        let player = rig.world.handle("player").unwrap();
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));

        rig.load("levels/b.rvescene").unwrap();
        let failures = rig.commit();
        assert!(failures.is_empty(), "{failures:?}");

        assert_eq!(rig.names(), sorted(&["boss", "camera_b", "player"]));
        assert_eq!(
            rig.world.handle("player"),
            Some(player),
            "хендл сохраняемого объекта не меняется"
        );
        assert_eq!(rig.host.current_scene(), "levels/b.rvescene");
        let messages = rig.messages();
        assert_eq!(
            messages
                .iter()
                .filter(|message| *message == "enemy destroy")
                .count(),
            1,
            "обычный экземпляр уничтожен один раз: {messages:?}"
        );
        assert!(
            !messages.iter().any(|message| message == "player destroy"),
            "сохраняемый экземпляр не уничтожается: {messages:?}"
        );
    }

    #[test]
    fn id_collision_with_a_persistent_object_is_rejected_before_any_teardown() {
        let mut rig = Rig::new();
        rig.load("levels/clash.rvescene").unwrap();
        let failures = rig.commit();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].0, "levels/clash.rvescene");
        assert!(failures[0].1.contains("совпадает"), "{failures:?}");
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));
        assert_eq!(rig.host.current_scene(), "levels/a.rvescene");
        assert!(
            !rig.messages()
                .iter()
                .any(|message| message == "enemy destroy"),
            "отказ происходит до on_destroy"
        );
    }

    #[test]
    fn missing_scene_fails_the_request_and_keeps_the_current_world() {
        let mut rig = Rig::new();
        let error = rig
            .switch
            .request(Some(&rig.loader), "levels/missing.rvescene");
        assert!(error.is_err());
        assert!(rig.commit().is_empty());
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));
        assert_eq!(rig.host.current_scene(), "levels/a.rvescene");
    }

    #[test]
    fn stale_success_releases_unique_resources_but_keeps_live_shared_materials() {
        let mut rig = Rig::new();
        let mut active = GpuSceneTable::default();
        active.by_object.insert(
            rig.world.handle("camera").unwrap(),
            vec![vec![(fake_mesh(1), fake_material(7))]],
        );
        rig.adopt_gpu(active);

        let stale = rig
            .switch
            .request(Some(&rig.loader), "levels/b.rvescene")
            .unwrap();
        rig.switch
            .request(Some(&rig.loader), "levels/c.rvescene")
            .unwrap();
        let mut uploaded = GpuSceneTable::default();
        uploaded.prefabs.insert(
            "prefabs/stale.rveprefab".to_owned(),
            vec![
                vec![vec![(fake_mesh(2), fake_material(7))]],
                vec![vec![(fake_mesh(3), fake_material(8))]],
            ],
        );
        uploaded
            .particle_template_owners
            .insert("prefabs/stale.rveprefab".to_owned(), 99);
        rig.switch.accept(SceneUploadReply {
            generation: stale.generation,
            result: Ok(Box::new(uploaded)),
        });

        assert!(rig.commit().is_empty());
        let releases = rig.released();
        assert_eq!(
            releases.len(),
            1,
            "stale upload releases once: {releases:?}"
        );
        assert_eq!(
            releases[0].meshes,
            HashSet::from([fake_mesh(2), fake_mesh(3)])
        );
        assert_eq!(releases[0].materials, HashSet::from([fake_material(8)]));
        assert_eq!(releases[0].cache_owners, HashSet::from([99]));
    }

    #[test]
    fn failed_scene_replacement_does_not_register_new_ui_or_damage_existing_ui() {
        let mut rig = Rig::start("levels/ui_atomicity.rvescene");
        rig.frame_update();
        assert!(
            rig.messages()
                .iter()
                .any(|message| message == "existing ui true"),
            "the current scene opens its registered UI document"
        );

        rig.load("levels/ui_replacement_failure.rvescene").unwrap();
        let failures = rig.commit();
        assert_eq!(
            failures.len(),
            1,
            "the bad script factory rejects replacement"
        );
        assert!(failures[0].1.contains("MissingUiScript"), "{failures:?}");
        assert_eq!(rig.names(), sorted(&["camera_ui", "ui_probe"]));
        assert_eq!(rig.host.current_scene(), "levels/ui_atomicity.rvescene");

        rig.frame_update();
        let messages = rig.messages();
        assert!(
            messages.iter().any(|message| message == "new ui false"),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message == "existing ui after true"),
            "the failed candidate document is absent and the existing document still opens: {messages:?}"
        );
    }

    #[test]
    fn requests_without_a_loader_are_refused() {
        let mut switch = SceneSwitch::default();
        let error = switch.request(None, "levels/b.rvescene").err().unwrap();
        assert!(error.contains("не подключена"), "{error}");
    }

    #[test]
    fn newer_request_supersedes_the_pending_one_and_its_stale_reply_changes_nothing() {
        let mut rig = Rig::new();
        let first = rig
            .switch
            .request(Some(&rig.loader), "levels/b.rvescene")
            .unwrap();
        let second = rig
            .switch
            .request(Some(&rig.loader), "levels/c.rvescene")
            .unwrap();
        assert!(second.generation > first.generation);

        let mut stale_table = GpuSceneTable::default();
        stale_table.ui_scene_owners.insert(first.generation);
        rig.switch.accept(SceneUploadReply {
            generation: first.generation,
            result: Ok(Box::new(stale_table)),
        });
        assert!(
            rig.commit().is_empty(),
            "ответ устаревшего поколения молчит"
        );
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));
        assert_eq!(
            rig.released()[0].ui_scene_owners,
            HashSet::from([first.generation]),
            "устаревший ответ снимает UI-владение своего поколения"
        );

        let mut current_table = GpuSceneTable::default();
        current_table.ui_scene_owners.insert(second.generation);
        rig.switch.accept(SceneUploadReply {
            generation: second.generation,
            result: Ok(Box::new(current_table)),
        });
        assert!(rig.commit().is_empty());
        assert_eq!(rig.names(), sorted(&["crate", "camera_c", "player"]));
        assert_eq!(rig.host.current_scene(), "levels/c.rvescene");
        assert!(rig.gpu.ui_scene_owners.contains(&second.generation));
    }

    #[test]
    fn invalid_later_request_keeps_the_previous_pending_scene() {
        let mut rig = Rig::new();
        let pending = rig
            .switch
            .request(Some(&rig.loader), "levels/b.rvescene")
            .unwrap();

        assert!(
            rig.switch
                .request(Some(&rig.loader), "levels/missing.rvescene")
                .is_err()
        );
        rig.switch.accept(SceneUploadReply {
            generation: pending.generation,
            result: Ok(Box::default()),
        });

        assert!(rig.commit().is_empty());
        assert_eq!(rig.host.current_scene(), "levels/b.rvescene");
    }

    #[test]
    fn gpu_error_reply_is_reported_and_keeps_the_current_world() {
        let mut rig = Rig::new();
        let upload = rig
            .switch
            .request(Some(&rig.loader), "levels/b.rvescene")
            .unwrap();
        rig.switch.accept(SceneUploadReply {
            generation: upload.generation,
            result: Err("ошибка загрузки GPU-ресурсов: test".to_owned()),
        });
        let failures = rig.commit();
        assert_eq!(
            failures,
            vec![(
                "levels/b.rvescene".to_owned(),
                "ошибка загрузки GPU-ресурсов: test".to_owned()
            )]
        );
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));
    }

    #[test]
    fn switching_back_and_forth_leaves_only_the_persistent_objects_and_the_current_scene() {
        let mut rig = Rig::new();
        let player = rig.world.handle("player").unwrap();
        for target in [
            "levels/b.rvescene",
            "levels/e.rvescene",
            "levels/b.rvescene",
            "levels/e.rvescene",
        ] {
            rig.load(target).unwrap();
            assert!(rig.commit().is_empty(), "переход в {target}");
            assert_eq!(rig.world.handle("player"), Some(player));
            assert_eq!(rig.host.current_scene(), target);
            // Кадр запускает экземпляры новой сцены; без него on_destroy не получит ни один из них.
            rig.frame_update();
        }
        assert_eq!(rig.names(), sorted(&["camera_e", "enemy", "player"]));
        let messages = rig.messages();
        assert_eq!(
            messages
                .iter()
                .filter(|message| *message == "enemy destroy")
                .count(),
            2,
            "обычный экземпляр уничтожается при каждом уходе из сцены с ним: {messages:?}"
        );
        assert!(
            !messages.iter().any(|message| message == "player destroy"),
            "{messages:?}"
        );
    }

    #[test]
    fn new_and_shared_fast_prefabs_switch_and_persistent_hub_spawns_them_afterward() {
        let mut rig = Rig::start("levels/pa.rvescene");
        let x = "prefabs/x.rveprefab";
        let y = "prefabs/y.rveprefab";
        assert_eq!(rig.host.prefab_status(x), Some(PrefabStatus::Resident));
        assert_eq!(rig.host.prefab_status(y), None);

        rig.load("levels/pb.rvescene").unwrap();
        let failures = rig.commit();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(rig.host.prefab_status(x), Some(PrefabStatus::Resident));
        assert_eq!(rig.host.prefab_status(y), Some(PrefabStatus::Resident));

        rig.frame_update();
        assert_eq!(
            rig.names(),
            sorted(&["b_crate", "camera_b", "hub", "spawned_x", "spawned_y"])
        );

        rig.load("levels/pc.rvescene").unwrap();
        let failures = rig.commit();
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(rig.host.prefab_status(x), Some(PrefabStatus::Resident));
        assert_eq!(rig.names(), sorted(&["c_crate", "camera_c", "hub"]));
        assert_eq!(rig.host.current_scene(), "levels/pc.rvescene");
    }

    #[test]
    fn resident_prefab_templates_keep_their_gpu_and_cpu_entries_across_a_switch() {
        let mut rig = Rig::start("levels/pa.rvescene");
        let x = "prefabs/x.rveprefab";
        let y = "prefabs/y.rveprefab";
        // Таблица стартовой сцены: `x` с двумя объектами шаблона. Длина отличает её от таблицы
        // следующей сцены, где `x` тоже объявлен, но с одним объектом.
        rig.gpu.prefabs.insert(x.to_owned(), vec![vec![], vec![]]);
        rig.gpu
            .particle_templates
            .insert(x.to_owned(), vec![None, None]);
        rig.gpu
            .material_templates
            .insert(x.to_owned(), vec![None, None]);

        let mut table = GpuSceneTable::default();
        table.prefabs.insert(x.to_owned(), vec![vec![]]);
        table.prefabs.insert(y.to_owned(), vec![vec![]]);
        table.particle_templates.insert(x.to_owned(), vec![None]);
        table.material_templates.insert(x.to_owned(), vec![None]);
        rig.load_with_table("levels/pb.rvescene", table).unwrap();
        let failures = rig.commit();
        assert!(failures.is_empty(), "{failures:?}");

        assert_eq!(
            rig.gpu.prefabs[x].len(),
            2,
            "уровни резидентного x не заменяются"
        );
        assert_eq!(rig.gpu.prefabs[y].len(), 1, "новый префаб получает уровни");
        assert_eq!(rig.gpu.particle_templates[x].len(), 2);
        assert_eq!(rig.gpu.material_templates[x].len(), 2);
        assert!(rig.gpu.material_templates.contains_key(x));

        rig.frame_update();
        assert!(rig.names().contains(&"spawned_x".to_owned()));
    }

    #[test]
    fn switch_inserts_the_bodies_of_the_new_scene_during_the_commit() {
        let mut rig = Rig::new();

        rig.load("levels/pphys.rvescene").unwrap();
        let failures = rig.commit();

        assert!(failures.is_empty(), "{failures:?}");
        let ball = rig.world.handle("ball").unwrap();
        let physics = rig.physics.as_ref().unwrap();
        assert!(
            physics
                .bindings()
                .iter()
                .any(|binding| binding.object == ball),
            "тело новой сцены привязано к физике сразу при замене"
        );
    }

    #[test]
    fn a_prefab_whose_script_fails_to_compile_leaves_no_registration_behind() {
        let mut rig = Rig::start("levels/pa.rvescene");
        let z = "prefabs/z.rveprefab";

        rig.load("levels/pbad.rvescene").unwrap();
        let failures = rig.commit();

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].1.contains("broken"), "{failures:?}");
        assert_eq!(
            rig.host.prefab_status(z),
            None,
            "нет орфан-состояния префаба"
        );
        assert_eq!(rig.names(), sorted(&["a_crate", "camera_a", "hub"]));
        assert_eq!(rig.host.current_scene(), "levels/pa.rvescene");
    }

    #[test]
    fn removed_objects_leave_the_table_before_their_resources_are_released() {
        let mut rig = Rig::new();
        let enemy = rig.world.handle("enemy").unwrap();
        let player = rig.world.handle("player").unwrap();
        let mut gpu = GpuSceneTable::default();
        gpu.by_object
            .insert(enemy, vec![vec![(fake_mesh(1), fake_material(1))]]);
        gpu.by_object
            .insert(player, vec![vec![(fake_mesh(2), fake_material(2))]]);
        rig.adopt_gpu(gpu);

        rig.load("levels/b.rvescene").unwrap();
        assert!(rig.commit().is_empty());

        assert!(
            !rig.gpu.by_object.contains_key(&enemy),
            "отображение удалённого объекта снято при замене, а не при следующей"
        );
        assert!(
            rig.gpu.by_object.contains_key(&player),
            "ресурсы сохраняемого объекта остаются"
        );
        let released = rig.released();
        assert_eq!(released.len(), 1, "{released:?}");
        assert_eq!(released[0].meshes, HashSet::from([fake_mesh(1)]));
        assert_eq!(released[0].materials, HashSet::from([fake_material(1)]));
    }

    #[test]
    fn failed_commit_releases_every_resource_of_its_upload_including_screen_shaders() {
        let mut rig = Rig::new();
        let mut table = GpuSceneTable::default();
        table.prefabs.insert(
            "prefabs/x.rveprefab".to_owned(),
            vec![vec![vec![(fake_mesh(11), fake_material(11))]]],
        );
        table
            .screen_shaders
            .insert("fx/a.glsl".to_owned(), fake_shader(10));
        table
            .color_luts
            .insert("luts/a.cube".to_owned(), fake_lut(12));
        table.ui_scene_owners.insert(1);
        rig.load_with_table("levels/clash.rvescene", table).unwrap();

        let failures = rig.commit();

        assert_eq!(failures.len(), 1, "{failures:?}");
        let released = rig.released();
        assert_eq!(released.len(), 1, "{released:?}");
        assert_eq!(released[0].meshes, HashSet::from([fake_mesh(11)]));
        assert_eq!(released[0].materials, HashSet::from([fake_material(11)]));
        assert_eq!(released[0].screen_shaders, HashSet::from([fake_shader(10)]));
        assert_eq!(released[0].color_luts, HashSet::from([fake_lut(12)]));
        assert_eq!(released[0].ui_scene_owners, HashSet::from([1]));
        assert!(rig.gpu.screen_shaders.is_empty());
        assert!(rig.gpu.color_luts.is_empty());
    }

    #[test]
    fn repeated_switches_release_each_screen_shader_once_and_keep_one_per_live_path() {
        let mut rig = Rig::new();
        let mut created = Vec::new();
        for (index, target) in [
            "levels/fx_a.rvescene",
            "levels/fx_b.rvescene",
            "levels/fx_a.rvescene",
            "levels/fx_b.rvescene",
            "levels/fx_c.rvescene",
        ]
        .into_iter()
        .enumerate()
        {
            let path = if target == "levels/fx_b.rvescene" {
                "fx/b.glsl"
            } else {
                "fx/a.glsl"
            };
            let shader = fake_shader(100 + index);
            created.push(shader);
            let mut table = GpuSceneTable::default();
            table.screen_shaders.insert(path.to_owned(), shader);
            rig.load_with_table(target, table).unwrap();
            assert!(rig.commit().is_empty(), "переход в {target}");
        }

        let live = rig.gpu.screen_shader_ids();
        assert_eq!(live, vec![fake_shader(104)], "один шейдер на живой путь");
        let released: Vec<ScreenShaderId> = rig
            .released()
            .into_iter()
            .flat_map(|ids| ids.screen_shaders)
            .collect();
        let unique: HashSet<ScreenShaderId> = released.iter().copied().collect();
        assert_eq!(unique.len(), released.len(), "каждый освобождён один раз");
        assert_eq!(released.len() + live.len(), created.len());
        assert!(
            created
                .iter()
                .all(|id| unique.contains(id) || live.contains(id))
        );
    }

    #[test]
    fn screen_shader_of_a_persistent_camera_survives_a_switch() {
        let mut rig = Rig::start("levels/fx_hub.rvescene");
        let mut gpu = GpuSceneTable::default();
        gpu.screen_shaders
            .insert("fx/hub.glsl".to_owned(), fake_shader(1));
        gpu.screen_shaders
            .insert("fx/lens.glsl".to_owned(), fake_shader(2));
        rig.adopt_gpu(gpu);

        rig.load("levels/fx_b.rvescene").unwrap();
        assert!(rig.commit().is_empty());

        assert_eq!(
            rig.gpu.screen_shaders.get("fx/lens.glsl"),
            Some(&fake_shader(2)),
            "шейдер сохраняемой камеры не освобождается"
        );
        let released: Vec<ScreenShaderId> = rig
            .released()
            .into_iter()
            .flat_map(|ids| ids.screen_shaders)
            .collect();
        assert_eq!(
            released,
            vec![fake_shader(1)],
            "шейдер ушедшей камеры освобождён"
        );
    }

    #[test]
    fn invalid_ui_document_rejects_the_switch_before_anything_changes() {
        let mut rig = Rig::new();
        let error = rig
            .switch
            .request(Some(&rig.loader), "levels/bad_ui.rvescene")
            .err()
            .unwrap();
        assert!(error.contains("ui/bad.rveui"), "{error}");
        assert!(rig.commit().is_empty());
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));
        assert_eq!(rig.host.current_scene(), "levels/a.rvescene");
        assert!(rig.released().is_empty());
    }
}
