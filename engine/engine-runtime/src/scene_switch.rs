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
use engine_render::{MaterialId, MeshId, SkinId};
use engine_resources::ResourceHandle;
use engine_scene::{ObjectHandle, World};
use engine_script::{ScriptHost, ScriptTime};

use crate::game_ui::{PreparedUi, UiRegistry};
use crate::pipeline::{GpuSceneTable, SpawnPhysics};
use crate::prefabs::{ParticleRequest, PrefabLoads, RenderResourceRequest};
use crate::scene::{
    PreparedPrefabs, PreparedRenderAssets, PreparedScene, load_scene, load_screen_shader_sources,
};

/// Загрузчик сцен проекта: читает `.rvescene` и ассеты тем же `ResourceHandle`, что и старт.
#[derive(Clone)]
pub(crate) struct SceneLoader {
    resources: ResourceHandle,
    physics: PhysicsSettings,
    /// Исходники экранных шейдеров нужны только GPU-клиенту, как при старте.
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
}

impl SceneGpuIds {
    /// Меши, материалы и скиннинг, на которые ссылаются уровни объектов, шаблоны префабов сцены
    /// и экземпляры skinning таблицы.
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
        ids
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.meshes.is_empty() && self.materials.is_empty() && self.skins.is_empty()
    }

    fn extend(&mut self, other: &Self) {
        self.meshes.extend(other.meshes.iter().copied());
        self.materials.extend(other.materials.iter().copied());
        self.skins.extend(other.skins.iter().copied());
    }

    /// Ресурсы этого набора, которых нет в `live`.
    pub(crate) fn minus(&self, live: &Self) -> Self {
        Self {
            meshes: difference(&self.meshes, &live.meshes),
            materials: difference(&self.materials, &live.materials),
            skins: difference(&self.skins, &live.skins),
        }
    }

    fn intersect(&self, live: &Self) -> Self {
        Self {
            meshes: overlap(&self.meshes, &live.meshes),
            materials: overlap(&self.materials, &live.materials),
            skins: overlap(&self.skins, &live.skins),
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
                .send_render(RenderResourceRequest::ReleaseScene { ids, last_seq });
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

    /// Принимает запрос `rve.scene.load` на загрузку `path`. Новая заявка отменяет ожидающую
    /// замену: её ответ придёт с устаревшим поколением. Ошибка подготовки тоже отменяет
    /// ожидающую замену, а текущая сцена продолжает работать.
    pub(crate) fn request(
        &mut self,
        loader: Option<&SceneLoader>,
        path: &str,
    ) -> Result<SceneUpload, String> {
        self.generation += 1;
        self.awaiting = None;
        let loader = loader.ok_or_else(|| "загрузка сцен не подключена".to_owned())?;
        let (prepared, physics_geometry) = loader.prepare(path)?;
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
                ctx.release(ids, 0);
                continue;
            }
            let Some((_, pending)) = self.awaiting.take() else {
                ctx.release(ids, 0);
                continue;
            };
            let path = pending.path.clone();
            if let Err(error) = self.commit(&mut ctx, pending, *table) {
                ctx.release(ids, 0);
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
        // Префабы сцены проверяются до любых изменений и применяются только после замены мира,
        // которая может отказать. Регистрация UI аддитивна и мира не меняет.
        let scene_prefabs = ctx
            .scripts
            .check_scene_prefabs(
                &pending.world,
                &pending.prefabs.catalog,
                &pending.prefabs.templates,
                &pending.sources,
            )
            .map_err(|error| error.to_string())?;
        ctx.ui_registry.register(ctx.scripts, &pending.ui)?;

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
        gpu.by_object.extend(rekey(&mut table.by_object, &handles));
        gpu.material_sources
            .extend(rekey(&mut table.material_sources, &handles));
        gpu.skin_instances
            .extend(rekey(&mut table.skin_instances, &handles));
        gpu.joint_counts
            .extend(rekey(&mut table.joint_counts, &handles));
        // Удалённые объекты: их скиннинг больше не живой и уходит в освобождение ниже.
        for object in &replacement.removed {
            gpu.skin_instances.remove(object);
            gpu.joint_counts.remove(object);
        }
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
        retain_resident(&mut gpu.material_templates, &mut table.material_templates);
        gpu.environments.extend(table.environments);
        gpu.animation_models.extend(table.animation_models);
        gpu.animation_clips.extend(table.animation_clips);
        gpu.screen_shaders.extend(table.screen_shaders);

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
    use super::{difference, overlap};
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
}

#[cfg(test)]
mod commit_tests {
    use super::*;
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
        _render_rx: flume::Receiver<RenderResourceRequest>,
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
            dir.write("levels/e.rvescene", ENEMY_SCENE);
            dir.write("levels/pa.rvescene", PREFAB_SCENE_A);
            dir.write("levels/pb.rvescene", PREFAB_SCENE_B);
            dir.write("levels/pbad.rvescene", PREFAB_SCENE_BROKEN);
            dir.write("levels/pc.rvescene", PREFAB_SCENE_C);
            dir.write("levels/pphys.rvescene", PHYSICS_SCENE);
            dir.write("scripts/player.lua", PLAYER);
            dir.write("scripts/enemy.lua", ENEMY);
            dir.write("scripts/boss.lua", BOSS);
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
                _render_rx: render_rx,
                animation: AnimationBridge::default(),
                ui_registry: UiRegistry::default(),
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

        rig.switch.accept(SceneUploadReply {
            generation: first.generation,
            result: Ok(Box::default()),
        });
        assert!(
            rig.commit().is_empty(),
            "ответ устаревшего поколения молчит"
        );
        assert_eq!(rig.names(), sorted(&["camera", "enemy", "player"]));

        rig.switch.accept(SceneUploadReply {
            generation: second.generation,
            result: Ok(Box::default()),
        });
        assert!(rig.commit().is_empty());
        assert_eq!(rig.names(), sorted(&["crate", "camera_c", "player"]));
        assert_eq!(rig.host.current_scene(), "levels/c.rvescene");
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
}
