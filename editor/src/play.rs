//! Play-режим: копия мира сцены и скрипты на главном потоке редактора.

use crate::profile::PlayStep;
use crate::project::read_resource_text;
use engine_animation::{AnimationBridge, AnimationEvent};
use engine_assets::Assets;
use engine_audio::{AudioBridge, AudioConfig, AudioEngine, AudioHandle};
use engine_console::{CommandCatalog, CommandLine};
use engine_files::VirtualPath;
use engine_physics::{
    BodyChange, BodyHandle, ModelGeometry, ModelMesh as PhysicsModelMesh, PhysicsClient,
    PhysicsCommand, PhysicsInitialization, PhysicsReceive, PhysicsSceneDesc, PhysicsSceneSync,
    PhysicsServer, PhysicsServerError, ShapeCache,
};
use engine_project::{InputSettings, PhysicsSettings, PrefabCatalogFile};
use engine_resources::ResourceHandle;
use engine_scene::document::mesh_collider_model;
use engine_scene::prefab::PREFAB_EXTENSION;
use engine_scene::{AudioRequest, ObjectHandle, PrefabTemplate, SceneDocument, VoiceId, World};
use engine_script::{
    CommandError, PrefabCatalog, PrefabKind, PrefabRequest, PrefabSpec, ScriptCursor, ScriptEvent,
    ScriptHost, ScriptInput, ScriptInterruptHandle, ScriptLibrary, ScriptLoadOptions, ScriptTime,
    SpawnedObject, WorldChanges,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Дольше этого Lua-вызов считается зависшим и прерывается, чтобы UI ожил.
const SCRIPT_TIME_LIMIT: Duration = Duration::from_secs(5);
/// Шаг времени не растёт при паузах окна и отладчике.
const MAX_DELTA_SECONDS: f32 = 0.25;

/// Прерывает Lua, если отмеченный участок выполняется дольше предела.
///
/// ABI-скрипты прервать нельзя: зависший Rust-скрипт останавливает весь редактор.
struct Watchdog {
    shared: Arc<WatchdogShared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct WatchdogState {
    deadline: Option<Instant>,
    stop: bool,
}

#[derive(Default)]
struct WatchdogShared {
    state: Mutex<WatchdogState>,
    changed: Condvar,
}

impl Watchdog {
    fn start(interrupt: ScriptInterruptHandle) -> std::io::Result<Self> {
        let shared = Arc::new(WatchdogShared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("rve-editor-watchdog".to_owned())
            .spawn(move || watch(&thread_shared, &interrupt))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn set_deadline(&self, deadline: Option<Instant>) {
        lock(&self.shared.state).deadline = deadline;
        self.shared.changed.notify_all();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn watch(shared: &WatchdogShared, interrupt: &ScriptInterruptHandle) {
    let mut state = lock(&shared.state);
    while !state.stop {
        state = match state.deadline {
            None => shared
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner()),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    interrupt.interrupt();
                    state.deadline = None;
                    continue;
                }
                shared
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
        };
    }
}

/// Префабы Play: быстрые — шаблоны из `[[assets]]` сцены, тяжёлые — записи списка проекта.
/// Быстрые строятся сразу, как в `rve-runtime`; тяжёлые лишь попадают в каталог и грузятся
/// фоновым потоком по заявке хоста.
#[derive(Default)]
pub struct PlayPrefabs {
    templates: BTreeMap<String, Arc<PrefabTemplate>>,
    catalog: PrefabCatalog,
    /// Исходники lite-скриптов шаблонов; без них `register_prefab` не скомпилирует класс.
    sources: BTreeMap<String, String>,
    /// Ресурсы проекта для фоновой загрузки тяжёлых префабов; без них они не грузятся.
    resources: Option<ResourceHandle>,
    /// Документ тяжёлого префаба из списка объявляет heavy-скрипт: Play нужна библиотека.
    heavy_prefabs_use_scripts: bool,
}

impl PlayPrefabs {
    /// Раскрывает каждый `.rveprefab` из `[[assets]]` документа. Префаб, который не читается,
    /// не разбирается или не может быть заспавнен, пропускается: его путь не попадает в
    /// каталог, и `spawn` по нему ошибку не даст. Причина возвращается предупреждением для лога
    /// Play. Префаб с камерой или светом, который используется экземпляром сцены, предупреждения
    /// не даёт. `instanced` — файлы, раскрытые экземплярами документа.
    ///
    /// Записи `heavy` без пути из `[[assets]]` сцены попадают в каталог как тяжёлые. Путь,
    /// который сцена объявила быстрым, остаётся быстрым, даже если он есть в списке проекта.
    /// Документ тяжёлой записи читается только для проверки heavy-скриптов; ассеты и Lua не
    /// открываются. Ошибку чтения здесь не сообщаем: загрузка префаба выведет её позже.
    pub fn load(
        document: &SceneDocument,
        read_text: &mut dyn FnMut(&str) -> Result<String, String>,
        instanced: &[VirtualPath],
        heavy: &PrefabCatalogFile,
    ) -> (Self, Vec<String>) {
        let mut prefabs = Self::default();
        let mut warnings = Vec::new();
        for asset in document
            .assets
            .iter()
            .filter(|asset| is_prefab_path(asset.path.as_str()))
        {
            let path = asset.path.as_str();
            let template = match PrefabTemplate::load(&asset.path, &mut |child: &VirtualPath| {
                read_text(&format!("/{}", child.as_str()))
            }) {
                Ok(template) => template,
                Err(error) => {
                    warnings.push(format!("префаб /{path} пропущен: {error}"));
                    continue;
                }
            };
            if template
                .objects()
                .iter()
                .any(|object| object.animator.is_some())
            {
                warnings.push(format!(
                    "префаб /{path}: аниматор объектов не поддерживается при спавне; объекты рисуются без анимации"
                ));
            }
            if let Err(error) = template.check_spawnable() {
                if !instanced.contains(&asset.path) {
                    warnings.push(format!("префаб /{path} пропущен: {error}"));
                }
                continue;
            }
            let sources = match lite_sources(&template, read_text) {
                Ok(sources) => sources,
                Err(error) => {
                    warnings.push(format!("префаб /{path} пропущен: {error}"));
                    continue;
                }
            };
            prefabs.sources.extend(sources);
            prefabs.catalog.insert(
                path,
                PrefabSpec {
                    kind: PrefabKind::Fast,
                    max_instances: asset.max_instances,
                },
            );
            prefabs
                .templates
                .insert(path.to_owned(), Arc::new(template));
        }
        for entry in &heavy.prefabs {
            let Ok(path) = VirtualPath::parse(&entry.path) else {
                warnings.push(format!("префаб {:?} в списке проекта пропущен", entry.path));
                continue;
            };
            let fast_in_scene = document
                .assets
                .iter()
                .any(|asset| asset.path == path && is_prefab_path(asset.path.as_str()));
            if fast_in_scene {
                continue;
            }
            if let Ok(template) = PrefabTemplate::load(&path, &mut |child: &VirtualPath| {
                read_text(&format!("/{}", child.as_str()))
            }) && template
                .objects()
                .iter()
                .any(|object| !object.heavy_scripts.is_empty())
            {
                prefabs.heavy_prefabs_use_scripts = true;
            }
            prefabs.catalog.insert(
                path.as_str(),
                PrefabSpec {
                    kind: PrefabKind::Heavy,
                    max_instances: entry.max_instances,
                },
            );
        }
        (prefabs, warnings)
    }

    /// Задаёт ресурсы проекта, из которых фоновый поток читает тяжёлые префабы.
    pub fn with_resources(mut self, resources: ResourceHandle) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Нужна ли Play библиотека Rust-скриптов: её требуют heavy-скрипты шаблонов быстрых
    /// префабов и документов тяжёлых префабов списка.
    pub fn has_heavy_scripts(&self) -> bool {
        self.heavy_prefabs_use_scripts
            || self.templates.values().any(|template| {
                template
                    .objects()
                    .iter()
                    .any(|object| !object.heavy_scripts.is_empty())
            })
    }
}

fn is_prefab_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension == PREFAB_EXTENSION)
}

/// Исходники lite-скриптов шаблона. Ошибка чтения любого из них делает префаб непригодным.
fn lite_sources(
    template: &PrefabTemplate,
    read_text: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut sources = BTreeMap::new();
    for script in template
        .objects()
        .iter()
        .flat_map(|object| &object.lite_scripts)
    {
        let path = script.path.as_str();
        if !sources.contains_key(path) {
            sources.insert(path.to_owned(), read_text(path)?);
        }
    }
    Ok(sources)
}

/// Тело корня spawn-объекта; `None`, если у корня нет коллайдера и тело не создавалось.
fn root_body(sync: &PhysicsSceneSync, root: ObjectHandle) -> Option<BodyHandle> {
    sync.bindings()
        .iter()
        .rev()
        .find(|binding| binding.object == root)
        .map(|binding| binding.body)
}

/// Геометрия модели для мешевого коллайдера: позиции и индексы мешей.
fn collider_geometry(assets: &Assets, path: &str) -> Result<ModelGeometry, String> {
    let model = assets
        .load_model(path)
        .map_err(|error| format!("{path}: {error}"))?;
    Ok(ModelGeometry {
        path: path.to_owned(),
        meshes: model
            .meshes
            .iter()
            .map(|mesh| PhysicsModelMesh {
                positions: mesh
                    .vertices
                    .iter()
                    .map(|vertex| glam::Vec3::from_array(vertex.position))
                    .collect(),
                indices: mesh.indices.clone(),
            })
            .collect(),
    })
}

/// Имя потока, который готовит тяжёлый префаб для Play.
const HEAVY_LOAD_THREAD: &str = "rve-editor-prefab-load";
const HEAVY_LOAD_INTERRUPTED: &str = "загрузка префаба прервана";

/// Тяжёлый префаб, подготовленный фоновым потоком: шаблон, Lua-исходники и геометрия физики.
/// GPU здесь не нужен: модели редактор подгружает лениво, как и объекты сцены.
struct HeavyPrefab {
    template: Arc<PrefabTemplate>,
    sources: BTreeMap<String, String>,
    geometry: Vec<ModelGeometry>,
}

/// Ответ фонового потока. `load_id` отличает ответ текущей загрузки от устаревшей.
struct HeavyReply {
    path: String,
    load_id: u64,
    result: Result<HeavyPrefab, String>,
}

/// Фоновая загрузка тяжёлых префабов Play: по потоку на загрузку, ответы в общий канал.
///
/// Потоки не join-ятся. При остановке сессии флаг `stop` сообщает им, что результат не нужен,
/// а ответы уходят в канал без получателя. Поток, который сейчас читает файл, завершится сам.
struct HeavyLoads {
    resources: Option<ResourceHandle>,
    next_load_id: u64,
    /// Загрузки, ответ на которые ещё ждётся: путь → `load_id`.
    pending: BTreeMap<String, u64>,
    /// Загруженные префабы: путь → модели, на которые он ссылается в физике.
    loaded: BTreeMap<String, Vec<String>>,
    reply_tx: flume::Sender<HeavyReply>,
    replies: flume::Receiver<HeavyReply>,
    stop: Arc<AtomicBool>,
}

impl HeavyLoads {
    fn new(resources: Option<ResourceHandle>) -> Self {
        let (reply_tx, replies) = flume::unbounded();
        Self {
            resources,
            next_load_id: 0,
            pending: BTreeMap::new(),
            loaded: BTreeMap::new(),
            reply_tx,
            replies,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Запускает загрузку `path`. Ошибка означает, что поток не создан, и префаб неудачен.
    fn start(&mut self, path: &str) -> Result<(), String> {
        let resources = self
            .resources
            .clone()
            .ok_or_else(|| format!("{path}: ресурсы проекта недоступны для тяжёлого префаба"))?;
        self.next_load_id += 1;
        let load_id = self.next_load_id;
        let owned = path.to_owned();
        let stop = Arc::clone(&self.stop);
        let replies = self.reply_tx.clone();
        std::thread::Builder::new()
            .name(HEAVY_LOAD_THREAD.to_owned())
            .spawn(move || {
                let result = load_heavy_prefab(&resources, &owned, &stop);
                // Канал закрыт, если сессия уже остановлена: тогда ответ никому не нужен.
                let _ = replies.send(HeavyReply {
                    path: owned,
                    load_id,
                    result,
                });
            })
            .map_err(|error| format!("{path}: не удалось запустить поток загрузки: {error}"))?;
        self.pending.insert(path.to_owned(), load_id);
        Ok(())
    }

    /// Ждёт ли загрузка `path` ответа с номером `load_id`.
    fn is_current(&self, path: &str, load_id: u64) -> bool {
        self.pending.get(path) == Some(&load_id)
    }

    fn try_reply(&self) -> Option<HeavyReply> {
        self.replies.try_recv().ok()
    }

    /// Загрузка получила ответ, поэтому больше не ждётся.
    fn finish(&mut self, path: &str) {
        self.pending.remove(path);
    }

    /// Префаб загружен и зарегистрирован. `models` — пути геометрии, которую он добавил в физику.
    fn mark_loaded(&mut self, path: &str, models: Vec<String>) {
        self.loaded.insert(path.to_owned(), models);
    }

    /// Забывает префаб при выгрузке и возвращает его модели. Загрузка в полёте тоже снимается,
    /// поэтому её ответ станет устаревшим.
    fn forget(&mut self, path: &str) -> Vec<String> {
        self.pending.remove(path);
        self.loaded.remove(path).unwrap_or_default()
    }
}

impl Drop for HeavyLoads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Готовит тяжёлый префаб в фоновом потоке тем же путём, что и `PlayPrefabs::load`, но читает
/// ресурсы проекта. Прерывание проверяется между шагами: без него поток дочитал бы все файлы
/// после остановки сессии.
fn load_heavy_prefab(
    resources: &ResourceHandle,
    path: &str,
    stop: &AtomicBool,
) -> Result<HeavyPrefab, String> {
    let interrupted = || stop.load(Ordering::Acquire);
    let prefab_path =
        VirtualPath::parse(path).map_err(|_| format!("некорректный путь префаба {path:?}"))?;
    let template = PrefabTemplate::load(&prefab_path, &mut |child: &VirtualPath| {
        read_resource_text(resources, &format!("/{}", child.as_str()))
    })
    .map_err(|error| error.to_string())?;
    template
        .check_spawnable()
        .map_err(|error| error.to_string())?;
    let sources = lite_sources(&template, &mut |script: &str| {
        if interrupted() {
            return Err(HEAVY_LOAD_INTERRUPTED.to_owned());
        }
        read_resource_text(resources, script)
    })?;
    let assets = Assets::with_resources(resources.clone());
    let models = template.mesh_collider_models();
    let mut geometry = Vec::with_capacity(models.len());
    for model in models {
        if interrupted() {
            return Err(HEAVY_LOAD_INTERRUPTED.to_owned());
        }
        geometry.push(collider_geometry(&assets, model)?);
    }
    Ok(HeavyPrefab {
        template: Arc::new(template),
        sources,
        geometry,
    })
}

/// Запущенная сцена. Документ не меняется: правки скриптов живут только в этом мире,
/// поэтому Stop возвращает редактор к исходному документу простым удалением сессии.
pub struct PlaySession {
    world: World,
    host: ScriptHost,
    physics: Option<PlayPhysics>,
    audio: Option<PlayAudio>,
    /// Аниматоры мира Play: запросы скриптов, время и события `animation_finished`.
    animation: AnimationBridge,
    watchdog: Watchdog,
    time_limit: Duration,
    paused: bool,
    elapsed: f64,
    frame: u64,
    last_tick: Instant,
    input: ScriptInput,
    heavy: HeavyLoads,
    last_step: PlayStep,
}

impl PlaySession {
    /// Запускает сцену. Исходники Lua читаются через `read_source`; скрипты получают `on_start`
    /// при первом обновлении.
    #[cfg(test)]
    pub fn start(
        document: &SceneDocument,
        library: Option<&ScriptLibrary>,
        read_source: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        let world = World::from_document(document).map_err(|error| error.to_string())?;
        Self::start_with_world(
            document,
            world,
            library,
            &InputSettings::default(),
            read_source,
        )
    }

    /// Starts from a world whose referenced camera settings are already loaded.
    #[cfg(test)]
    pub fn start_with_world(
        document: &SceneDocument,
        world: World,
        library: Option<&ScriptLibrary>,
        input_settings: &InputSettings,
        read_source: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        Self::start_with_world_limit(
            document,
            world,
            library,
            input_settings,
            read_source,
            PlayPrefabs::default(),
            Vec::new(),
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            AudioConfig::default(),
            AnimationBridge::default(),
        )
    }

    /// Starts Play with project input and physics settings, the fast prefabs of the scene, CPU
    /// geometry for mesh colliders, the audio configuration (ресурсы проекта и устройство вывода)
    /// and the animation bridge built from the same world.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_world_and_physics(
        document: &SceneDocument,
        world: World,
        library: Option<&ScriptLibrary>,
        input_settings: &InputSettings,
        read_source: impl FnMut(&str) -> Result<String, String>,
        prefabs: PlayPrefabs,
        model_geometry: Vec<ModelGeometry>,
        physics_settings: &PhysicsSettings,
        audio: AudioConfig,
        animation: AnimationBridge,
    ) -> Result<Self, String> {
        Self::start_with_world_limit(
            document,
            world,
            library,
            input_settings,
            read_source,
            prefabs,
            model_geometry,
            physics_settings,
            SCRIPT_TIME_LIMIT,
            audio,
            animation,
        )
    }

    /// Loads the CPU model geometry required by convex and triangle-mesh colliders of the scene
    /// objects and of the fast prefabs: bodies of spawned objects are inserted with it later.
    pub fn load_model_geometry(
        world: &World,
        prefabs: &PlayPrefabs,
        assets: &Assets,
    ) -> Result<Vec<ModelGeometry>, String> {
        let paths = world
            .objects()
            .iter()
            .filter_map(|object| {
                mesh_collider_model(object.collider.clone(), object.mesh_renderer.as_ref())
            })
            .chain(
                prefabs
                    .templates
                    .values()
                    .flat_map(|template| template.mesh_collider_models()),
            )
            .collect::<BTreeSet<_>>();
        paths
            .into_iter()
            .map(|path| collider_geometry(assets, path))
            .collect()
    }

    #[cfg(test)]
    fn start_with_limit(
        document: &SceneDocument,
        library: Option<&ScriptLibrary>,
        read_source: impl FnMut(&str) -> Result<String, String>,
        time_limit: Duration,
    ) -> Result<Self, String> {
        let world = World::from_document(document).map_err(|error| error.to_string())?;
        Self::start_with_world_limit(
            document,
            world,
            library,
            &InputSettings::default(),
            read_source,
            PlayPrefabs::default(),
            Vec::new(),
            &PhysicsSettings::default(),
            time_limit,
            AudioConfig::default(),
            AnimationBridge::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_world_limit(
        document: &SceneDocument,
        mut world: World,
        library: Option<&ScriptLibrary>,
        input_settings: &InputSettings,
        mut read_source: impl FnMut(&str) -> Result<String, String>,
        prefabs: PlayPrefabs,
        model_geometry: Vec<ModelGeometry>,
        physics_settings: &PhysicsSettings,
        time_limit: Duration,
        audio: AudioConfig,
        animation: AnimationBridge,
    ) -> Result<Self, String> {
        let PlayPrefabs {
            templates,
            catalog,
            mut sources,
            resources,
            heavy_prefabs_use_scripts: _,
        } = prefabs;
        for object in world.objects() {
            for script in &object.lite_scripts {
                let path = script.path.as_str();
                if !sources.contains_key(path) {
                    sources.insert(path.to_owned(), read_source(path)?);
                }
            }
        }
        let interrupt = ScriptInterruptHandle::new();
        let watchdog = Watchdog::start(interrupt.clone())
            .map_err(|error| format!("не удалось запустить watchdog скриптов: {error}"))?;
        watchdog.set_deadline(Some(Instant::now() + time_limit));
        let mut host = ScriptHost::load_with_options(
            &world,
            document.source_label(),
            &sources,
            library,
            ScriptLoadOptions {
                interrupt,
                call_time_limit: None,
                input: input_settings.clone(),
                prefabs: catalog,
            },
        )
        .map_err(|error| error.to_string())?;
        // Регистрация компилирует Lua шаблонов: зависший top-level chunk прерывает watchdog.
        for (path, template) in templates {
            host.register_prefab(&world, &path, template, &sources)
                .map_err(|error| error.to_string())?;
        }
        watchdog.set_deadline(None);
        host.set_cursor(ScriptCursor::from(world.active_camera_cursor()));
        let desc = PhysicsSceneDesc::from_world(
            &world,
            &model_geometry,
            glam::Vec3::from_array(physics_settings.gravity),
            physics_settings.fixed_hz,
            physics_settings.max_substeps as usize,
        )
        .map_err(|error| error.to_string())?;
        let (driver, bindings) = desc.build().map_err(|error| error.to_string())?;
        let (physics, initial_snapshot) =
            PlayPhysics::start(&world, driver, bindings, model_geometry)?;
        host.set_physics_snapshot(initial_snapshot, physics.sync.bindings());
        let mut audio = PlayAudio::start(audio)?;
        // Матрицы до первого кадра не посчитаны; play_on_start берёт позиции из них.
        world.recalculate_world_matrices();
        audio.bridge.start(&world);
        Ok(Self {
            world,
            host,
            physics: Some(physics),
            audio: Some(audio),
            animation,
            watchdog,
            time_limit,
            paused: false,
            elapsed: 0.0,
            frame: 0,
            last_tick: Instant::now(),
            last_step: PlayStep::default(),
            input: ScriptInput::default(),
            heavy: HeavyLoads::new(resources),
        })
    }

    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Мир и мост анимации для кадра: палитры берутся из моста, а скиннинг — из мира.
    pub fn world_and_animation(&mut self) -> (&mut World, &AnimationBridge) {
        (&mut self.world, &self.animation)
    }

    /// Замеры последнего выполненного шага Play.
    pub fn last_step(&self) -> PlayStep {
        self.last_step
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Курсор, установленный скриптами последним успешным вызовом или флагами камеры при старте.
    pub fn cursor(&self) -> ScriptCursor {
        self.host.cursor()
    }

    /// Пауза не накапливает время: первый кадр после неё получает обычный шаг.
    /// Звук на паузе замолкает; позиции и запуски в это время не обрабатываются.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.last_tick = Instant::now();
        if let Some(audio) = &self.audio {
            // Поток звука уже остановлен, если команда не принята; тишина тогда не нужна.
            let _ = audio.handle.set_paused(paused);
        }
    }

    /// Вводит состояние кнопок и клавиш для следующего обновления.
    pub fn input_mut(&mut self) -> &mut ScriptInput {
        &mut self.input
    }

    pub(crate) fn clear_input(&mut self) {
        self.input.release_all();
        self.input.clear_pointer();
    }

    pub fn command_catalog(&self) -> CommandCatalog {
        self.host.command_catalog()
    }

    /// Executes a console callback under the same watchdog used for Play updates.
    pub fn execute_command(&mut self, command: &CommandLine) -> Result<(), CommandError> {
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        let result = self.host.execute_command(&mut self.world, command);
        self.watchdog.set_deadline(None);
        self.apply_world_changes();
        result
    }

    /// Выполняет один кадр скриптов, если сцена не на паузе.
    pub fn tick(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last_tick)
            .as_secs_f32()
            .min(MAX_DELTA_SECONDS);
        self.last_tick = now;
        self.step(dt)
    }

    fn step(&mut self, dt: f32) -> Result<(), String> {
        // Регистрация готового префаба не выполняет кадр, поэтому её можно делать и на паузе.
        let replies_started = Instant::now();
        self.receive_heavy_replies();
        let prefab_replies = replies_started.elapsed();
        if self.paused {
            return Ok(());
        }
        let mut physics_wait = Duration::ZERO;
        if let Some(physics) = &mut self.physics {
            let started = Instant::now();
            physics.receive(&mut self.world, &mut self.host)?;
            physics_wait = started.elapsed();
        }
        self.elapsed += f64::from(dt);
        self.frame += 1;
        self.host.set_input(self.input.take_frame());
        let time = self.script_time();
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        self.host.update(&mut self.world, dt, time);
        self.watchdog.set_deadline(None);
        // Порядок как в runtime: запросы скриптов, затем время; события уйдут в следующий update.
        self.animation.apply(self.host.drain_animation_requests());
        let finished = self.animation.advance(dt);
        self.host.queue_animation_events(
            &self.world,
            finished.into_iter().map(|event| match event {
                AnimationEvent::Finished { object, motion } => (object, motion),
            }),
        );
        let world_started = Instant::now();
        self.apply_world_changes();
        let world_changes = world_started.elapsed();
        let physics_started = Instant::now();
        let mut physics_step = Duration::ZERO;
        if let Some(physics) = &mut self.physics {
            let mut commands = self.host.drain_physics_commands();
            commands.extend(
                physics
                    .sync
                    .sync_world_commands(&mut self.world)
                    .map_err(|error| error.to_string())?,
            );
            commands.append(&mut physics.velocities);
            let changes = std::mem::take(&mut physics.changes);
            physics
                .client
                .step_with_changes(dt, changes, commands)
                .map_err(|error| error.to_string())?;
            physics_step = physics_started.elapsed();
        }
        self.last_step = PlayStep {
            script: self.host.last_update_profile(),
            physics_wait,
            physics_step,
            world_changes,
            prefab_replies,
        };
        if let Some(audio) = &mut self.audio {
            audio
                .bridge
                .apply(self.host.drain_audio_requests(), &self.world);
            // Редактор не вызывает build_frame, поэтому матрицы пересчитываются здесь.
            self.world.recalculate_world_matrices();
            audio.bridge.sync_positions(&self.world);
        }
        Ok(())
    }

    /// Применяет закоммиченные скриптами изменения мира: spawn, destroy и заявки тяжёлых
    /// префабов. Порядок тот же, что в `rve-runtime`: тела удаляются и вставляются до
    /// синхронизации поз, поэтому новые тела не телепортируются. Выгрузка может сразу запросить
    /// новую загрузку (отложенный spawn), поэтому заявки вычитываются, пока очередь не опустеет.
    fn apply_world_changes(&mut self) {
        loop {
            let WorldChanges {
                spawned,
                despawned,
                prefab_requests,
            } = self.host.drain_world_changes();
            if spawned.is_empty() && despawned.is_empty() && prefab_requests.is_empty() {
                return;
            }
            self.apply_spawns(spawned, &despawned);
            self.apply_prefab_requests(prefab_requests);
        }
    }

    /// Вставляет тела spawn-объектов и удаляет тела уничтоженных. Отказ физики откатывает spawn;
    /// тело до вставки не создаётся, поэтому удалять нечего.
    fn apply_spawns(&mut self, spawned: Vec<SpawnedObject>, despawned: &[ObjectHandle]) {
        if spawned.is_empty() && despawned.is_empty() {
            return;
        }
        let mut requests: Vec<AudioRequest> = despawned
            .iter()
            .map(|&object| AudioRequest::StopSource { object })
            .collect();
        if let Some(physics) = &mut self.physics {
            physics
                .changes
                .extend(physics.sync.remove_objects(despawned));
        }
        for spawned in &spawned {
            // Объект, созданный и удалённый в одной фазе, уже не жив: тело и звук ему не нужны.
            if !self.world.is_alive(spawned.handle) {
                continue;
            }
            if let Some(physics) = &mut self.physics {
                let inserted = physics.sync.insert_objects(
                    &self.world,
                    &spawned.objects,
                    &physics.geometry,
                    &mut physics.shapes,
                );
                match inserted {
                    Ok(inserted) => {
                        physics.changes.extend(inserted);
                        if let Some(velocity) = spawned.initial_velocity
                            && let Some(body) = root_body(&physics.sync, spawned.handle)
                        {
                            physics
                                .velocities
                                .push(PhysicsCommand::SetVelocity { body, velocity });
                        }
                    }
                    Err(error) => {
                        self.host.revert_spawn(
                            &mut self.world,
                            spawned.handle,
                            &format!("физика отклонила объект {:?}: {error}", spawned.prefab),
                        );
                        continue;
                    }
                }
            }
            requests.extend(
                spawned
                    .objects
                    .iter()
                    .filter(|&&object| {
                        self.world
                            .audio_source(object)
                            .is_some_and(|audio| audio.play_on_start)
                    })
                    .map(|&object| AudioRequest::PlaySource {
                        object,
                        voice: VoiceId::allocate(),
                    }),
            );
        }
        if let Some(audio) = &mut self.audio {
            audio.bridge.apply(requests, &self.world);
        }
    }

    /// Выполняет заявки хоста на тяжёлые префабы. Загрузку запускает фоновый поток; выгрузка
    /// снимает геометрию физики и сообщает хосту, что ресурсы ушли.
    fn apply_prefab_requests(&mut self, requests: Vec<PrefabRequest>) {
        for request in requests {
            match request {
                PrefabRequest::Load(path) => {
                    if let Err(error) = self.heavy.start(&path) {
                        self.host.prefab_failed(&self.world, &path, &error);
                    }
                }
                PrefabRequest::Unload(path) => {
                    let models = self.heavy.forget(&path);
                    if let Some(physics) = &mut self.physics {
                        physics.remove_heavy_geometry(&models);
                    }
                    self.host.prefab_released(&path);
                }
            }
        }
    }

    /// Забирает готовые ответы фоновых загрузок, не дожидаясь их.
    fn receive_heavy_replies(&mut self) {
        while let Some(reply) = self.heavy.try_reply() {
            self.apply_heavy_reply(reply);
        }
    }

    /// Регистрирует загруженный префаб и добавляет его геометрию в физику. Неудачная загрузка
    /// или регистрация переводит префаб в `failed`: его ожидающие spawn получают Error. Ответ
    /// устаревшей загрузки отбрасывается.
    fn apply_heavy_reply(&mut self, reply: HeavyReply) {
        let HeavyReply {
            path,
            load_id,
            result,
        } = reply;
        if !self.heavy.is_current(&path, load_id) {
            return;
        }
        self.heavy.finish(&path);
        let HeavyPrefab {
            template,
            sources,
            geometry,
        } = match result {
            Ok(prefab) => prefab,
            Err(error) => {
                self.host.prefab_failed(&self.world, &path, &error);
                return;
            }
        };
        // Регистрация компилирует Lua шаблона: зависший top-level chunk прерывает watchdog.
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        let registered = self
            .host
            .register_prefab(&self.world, &path, template, &sources);
        self.watchdog.set_deadline(None);
        if let Err(error) = registered {
            self.host
                .prefab_failed(&self.world, &path, &error.to_string());
            return;
        }
        let models = geometry.iter().map(|model| model.path.clone()).collect();
        if let Some(physics) = &mut self.physics {
            physics.add_heavy_geometry(geometry);
        }
        self.heavy.mark_loaded(&path, models);
    }

    fn script_time(&self) -> ScriptTime {
        ScriptTime {
            elapsed: self.elapsed,
            frame: self.frame,
        }
    }

    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        self.host.drain_events()
    }

    /// Предупреждения звука, которые ещё не показаны: клип не загрузился, устройство недоступно.
    pub fn drain_audio_warnings(&mut self) -> Vec<String> {
        self.audio
            .as_mut()
            .map_or_else(Vec::new, |audio| audio.bridge.take_warnings())
    }

    /// Предупреждения анимации: клип или модель не загрузились, движение не найдено.
    pub fn drain_animation_warnings(&mut self) -> Vec<String> {
        self.animation.take_warnings()
    }

    /// Завершает скрипты вызовом `on_destroy` и возвращает последние события.
    pub fn stop(mut self) -> Vec<ScriptEvent> {
        let time = self.script_time();
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        self.host.destroy(&mut self.world, time);
        self.watchdog.set_deadline(None);
        let mut events = self.host.drain_events();
        if let Some(mut physics) = self.physics.take()
            && let Err(error) = physics.shutdown()
        {
            events.push(physics_error_event(error));
        }
        if let Some(mut audio) = self.audio.take() {
            audio.shutdown();
        }
        events
    }
}

impl Drop for PlaySession {
    fn drop(&mut self) {
        if let Some(mut physics) = self.physics.take() {
            let _ = physics.shutdown();
        }
        if let Some(mut audio) = self.audio.take() {
            audio.shutdown();
        }
    }
}

/// Звук Play: поток `rve-audio` и мост, который отдаёт ему запросы скриптов и позиции.
struct PlayAudio {
    bridge: AudioBridge,
    handle: AudioHandle,
    thread: Option<JoinHandle<()>>,
}

impl PlayAudio {
    fn start(config: AudioConfig) -> Result<Self, String> {
        let (engine, handle) = AudioEngine::new(config);
        let thread = std::thread::Builder::new()
            .name("rve-audio".to_owned())
            .spawn(move || engine.run())
            .map_err(|error| format!("не удалось запустить поток звука: {error}"))?;
        Ok(Self {
            bridge: AudioBridge::new(handle.clone()),
            handle,
            thread: Some(thread),
        })
    }

    /// Останавливает поток (он закрывает устройство вывода) и ждёт его завершения.
    /// Повторный вызов безвреден.
    fn shutdown(&mut self) {
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct PlayPhysics {
    client: PhysicsClient,
    sync: PhysicsSceneSync,
    /// Геометрия мешевых коллайдеров: по ней вставляются тела spawn-объектов.
    geometry: Vec<ModelGeometry>,
    /// Сколько загруженных тяжёлых префабов ссылаются на модель, добавленную ими в `geometry`.
    /// Модели сцены сюда не попадают и никогда не удаляются.
    heavy_users: BTreeMap<String, usize>,
    /// Формы мешей spawn-объектов; переиспользуются между spawn одной модели.
    shapes: ShapeCache,
    /// Вставки и удаления тел, которые уйдут в ближайший `step_with_changes`.
    changes: Vec<BodyChange>,
    /// Скорости корней spawn-объектов; применяются сразу после вставки тела в том же шаге.
    velocities: Vec<PhysicsCommand>,
    _control_tx: flume::Sender<()>,
    control_rx: flume::Receiver<()>,
    worker: Option<JoinHandle<Result<(), PhysicsServerError>>>,
}

impl PlayPhysics {
    fn start(
        world: &World,
        driver: engine_physics::StepDriver,
        bindings: Vec<engine_physics::PhysicsSceneBinding>,
        geometry: Vec<ModelGeometry>,
    ) -> Result<(Self, Arc<engine_physics::QuerySnapshot>), String> {
        let initialization =
            PhysicsInitialization::from_prebuilt(&driver, bindings).map_err(|error| {
                format!("не удалось создать начальный snapshot физики Play: {error}")
            })?;
        let sync = PhysicsSceneSync::new(world, initialization.bindings.clone())
            .map_err(|error| format!("не удалось синхронизировать физику Play: {error}"))?;
        let snapshot = Arc::clone(&initialization.result.snapshot);
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let worker = std::thread::Builder::new()
            .name("rve-editor-physics".to_owned())
            .spawn(move || PhysicsServer::run_preinitialized(request_rx, reply_tx, driver))
            .map_err(|error| format!("не удалось запустить физику Play: {error}"))?;
        let (control_tx, control_rx) = flume::unbounded();
        Ok((
            Self {
                client: PhysicsClient::new_preinitialized(request_tx, reply_rx),
                sync,
                geometry,
                heavy_users: BTreeMap::new(),
                shapes: ShapeCache::default(),
                changes: Vec::new(),
                velocities: Vec::new(),
                _control_tx: control_tx,
                control_rx,
                worker: Some(worker),
            },
            snapshot,
        ))
    }

    /// Добавляет геометрию тяжёлого префаба. Модель, которая уже есть в `geometry`, не
    /// дублируется: её ссылки считаются, но выгрузка удаляет только модели тяжёлых префабов.
    fn add_heavy_geometry(&mut self, models: Vec<ModelGeometry>) {
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
    /// никто не использует. Модели сцены и быстрых префабов не трогаются.
    fn remove_heavy_geometry(&mut self, paths: &[String]) {
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

    fn receive(&mut self, world: &mut World, host: &mut ScriptHost) -> Result<(), String> {
        match self
            .client
            .receive(&self.control_rx)
            .map_err(|error| format!("ошибка шага физики Play: {error}"))?
        {
            None => Ok(()),
            Some(PhysicsReceive::Step(result)) => {
                self.sync
                    .apply_physics_poses(world, &result.poses)
                    .map_err(|error| error.to_string())?;
                host.set_physics_snapshot(result.snapshot, self.sync.bindings());
                host.queue_physics_events(result.events);
                Ok(())
            }
            Some(PhysicsReceive::Interrupted(())) => {
                Err("физика Play получила неожиданное сообщение управления".to_owned())
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let mut error = self
            .client
            .shutdown()
            .err()
            .map(|error| format!("не удалось остановить физику Play: {error}"));
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(server_error)) => {
                    error.get_or_insert_with(|| {
                        format!("поток физики Play завершился: {server_error}")
                    });
                }
                Err(_) => {
                    error.get_or_insert_with(|| "поток физики Play завершился паникой".to_owned());
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

impl Drop for PlayPhysics {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.shutdown();
        }
    }
}

fn physics_error_event(message: String) -> ScriptEvent {
    ScriptEvent {
        level: engine_script::ScriptEventLevel::Error,
        source: "physics".to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_project::{InputAction, PrefabCatalogEntry};
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_script::PrefabStatus;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    const SCENE: &str = r#"
[[assets]]
path = "/rotate.lua"

[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "spinner"
[[objects.lite_scripts]]
path = "/rotate.lua"
params = { speed = 90.0 }
"#;

    const ROTATE: &str = r#"
local Rotate = {}
function Rotate:on_start()
  rve.log_info("started")
end
function Rotate:on_update(dt)
  local rotation = self.transform.rotation
  self.transform.rotation = { x = rotation.x, y = rotation.y + self.params.speed * dt, z = rotation.z }
end
function Rotate:on_destroy()
  rve.log_info("destroyed")
end
return Rotate
"#;

    fn document(source: &str) -> SceneDocument {
        SceneDocument::parse("scene", source).unwrap()
    }

    fn start(script: &str) -> Result<PlaySession, String> {
        PlaySession::start(&document(SCENE), None, |_| Ok(script.to_owned()))
    }

    fn start_with_input_settings(
        script: &str,
        input_settings: InputSettings,
    ) -> Result<PlaySession, String> {
        let document = document(SCENE);
        let world = World::from_document(&document).map_err(|error| error.to_string())?;
        PlaySession::start_with_world(&document, world, None, &input_settings, |_| {
            Ok(script.to_owned())
        })
    }

    fn rotation_y(session: &mut PlaySession) -> f32 {
        let world = session.world_mut();
        let handle = world.handle("spinner").unwrap();
        world.local_transform(handle).unwrap().rotation_deg.y
    }

    #[test]
    fn lua_script_changes_the_play_world_but_not_the_document() {
        let document = document(SCENE);
        let mut session = PlaySession::start(&document, None, |_| Ok(ROTATE.to_owned())).unwrap();
        session.step(0.5).unwrap();
        assert!((rotation_y(&mut session) - 45.0).abs() < 1e-3);
        let original = document.objects.iter().find(|o| o.id == "spinner").unwrap();
        assert_eq!(original.transform.rotation, [0.0; 3]);
    }

    #[test]
    fn play_starts_with_the_active_camera_cursor_flags() {
        let scene = SCENE.replace(
            "[objects.camera]\n",
            "[objects.camera]\n[objects.camera.cursor]\nhide_on_start = true\nlock_on_start = true\n",
        );
        let session =
            PlaySession::start(&document(&scene), None, |_| Ok(ROTATE.to_owned())).unwrap();
        assert_eq!(
            session.cursor(),
            ScriptCursor {
                visible: false,
                locked: true,
            }
        );
    }

    #[test]
    fn pause_stops_updates_and_resume_continues() {
        let mut session = start(ROTATE).unwrap();
        session.step(0.5).unwrap();
        session.set_paused(true);
        assert!(session.is_paused());
        session.step(0.5).unwrap();
        session.tick().unwrap();
        assert!((rotation_y(&mut session) - 45.0).abs() < 1e-3);
        session.set_paused(false);
        session.step(0.5).unwrap();
        assert!((rotation_y(&mut session) - 90.0).abs() < 1e-3);
    }

    #[test]
    fn play_receives_initial_physics_snapshot_before_on_start() {
        let source = r#"
[[assets]]
path = "probe.lua"
[[objects]]
id = "camera"
[objects.transform]
position = [0.0, 0.0, -2.0]
[objects.camera]
[[objects.lite_scripts]]
path = "probe.lua"
[[objects]]
id = "wall"
[objects.collider]
shape = "sphere"
radius = 0.5
"#;
        let script = r#"
local Script = {}
function Script:on_start()
  local hit = rve.physics.raycast({x=0,y=0,z=-2}, {x=0,y=0,z=1}, 5)
  assert(hit and hit.object.id == "wall")
  self.transform.position = {x=3,y=0,z=-2}
end
return Script
"#;
        let mut session =
            PlaySession::start(&document(source), None, |_| Ok(script.to_owned())).unwrap();
        session.step(0.0).unwrap();
        let camera = session.world.handle("camera").unwrap();
        assert_eq!(
            session.world.local_transform(camera).unwrap().position.x,
            3.0
        );
        session.stop();
    }

    #[test]
    fn pause_does_not_submit_or_wait_for_an_extra_physics_step() {
        let source = r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "ball"
[objects.transform]
position = [0.0, 2.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
[objects.rigid_body]
kind = "dynamic"
mass = 1.0
"#;
        let mut session =
            PlaySession::start(&document(source), None, |_| Ok(String::new())).unwrap();
        session.step(1.0 / 60.0).unwrap();
        session.set_paused(true);
        session.step(0.5).unwrap();
        session.tick().unwrap();
        session.set_paused(false);
        session.step(1.0 / 60.0).unwrap();

        let ball = session.world.handle("ball").unwrap();
        assert!(session.world.local_transform(ball).unwrap().position.y < 2.0);
        session.stop();
    }

    #[test]
    fn dropping_play_shuts_down_physics_with_an_outstanding_step() {
        let source = r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "ball"
[objects.transform]
position = [0.0, 2.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
[objects.rigid_body]
kind = "dynamic"
mass = 1.0
"#;
        let mut session =
            PlaySession::start(&document(source), None, |_| Ok(String::new())).unwrap();
        session.step(1.0 / 60.0).unwrap();

        drop(session);
    }

    const ANIMATED_SCENE: &str = r#"
[[assets]]
path = "/models/hero.fbx"
[[assets]]
path = "/hero.lua"

[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "hero"
[objects.mesh_renderer]
model = "/models/hero.fbx"
[objects.animator]
looping = false
[[objects.lite_scripts]]
path = "/hero.lua"
"#;

    const ANIMATED_HERO: &str = r#"
local Hero = {}
function Hero:on_start()
  rve.find("hero"):play_animation("Walk", { loop = false })
end
function Hero:on_message(sender, name, value)
  rve.log_info(name .. ":" .. value)
end
return Hero
"#;

    #[test]
    fn one_shot_animation_finish_reaches_the_script_as_a_message() {
        let document = document(ANIMATED_SCENE);
        let world = World::from_document(&document).unwrap();
        let animation = AnimationBridge::build(
            &world,
            |_| {
                Ok(std::sync::Arc::new(
                    crate::viewport::animation::test_support::walk_model(),
                ))
            },
            |_| Ok(std::sync::Arc::new(Vec::new())),
        );
        let mut session = PlaySession::start_with_world_limit(
            &document,
            world,
            None,
            &InputSettings::default(),
            |_| Ok(ANIMATED_HERO.to_owned()),
            PlayPrefabs::default(),
            Vec::new(),
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            AudioConfig::default(),
            animation,
        )
        .unwrap();

        // Первый шаг запускает on_start и клип; конец клипа приходит в шаге, где время прошло.
        session.step(0.1).unwrap();
        let hero = session.world.handle("hero").unwrap();
        assert!(session.animation.has_animator(hero));
        session.step(1.5).unwrap();
        session.step(0.0).unwrap();
        let messages: Vec<String> = session
            .drain_events()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message == "animation_finished:Walk"),
            "{messages:?}"
        );
        assert!(session.drain_animation_warnings().is_empty());
    }

    #[test]
    fn demo_worm_curl_finishes_into_wiggle_and_its_script_hears_it() {
        // Ассеты и скрипт берутся из examples/demo-game: тест следит за работоспособностью демо.
        let root = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/demo-game"
        ));
        let project = crate::project::OpenProject::open(root).unwrap();
        const SCENE: &str = "[[assets]]\npath = \"/models/worm.fbx\"\n[[assets]]\npath = \"/anims/worm_curl.fbx\"\n[[assets]]\npath = \"/scripts/worm.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"worm\"\n[objects.mesh_renderer]\nmodel = \"/models/worm.fbx\"\n[objects.animator]\nclips = [\"/anims/worm_curl.fbx\"]\nplay_on_start = false\n[[objects.lite_scripts]]\npath = \"/scripts/worm.lua\"\n";
        let document = SceneDocument::parse("worm.rvescene", SCENE).unwrap();
        let world = World::from_document(&document).unwrap();
        let animation = crate::viewport::animation::project_bridge(&world, &project.assets);
        let mut session = PlaySession::start_with_world_and_physics(
            &document,
            world,
            None,
            &project.settings.input,
            |path| project.read_text(path),
            PlayPrefabs::default(),
            Vec::new(),
            &project.settings.physics,
            AudioConfig::default(),
            animation,
        )
        .unwrap();

        // Разовый клип длится 1 с; за 1.5 с он заканчивается и переходит в цикл Wiggle.
        for _ in 0..30 {
            session.step(0.05).unwrap();
        }
        let messages: Vec<String> = session
            .drain_events()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert!(
            messages.contains(&"Worm: curl, then Wiggle".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"Worm: worm_curl finished".to_owned()),
            "{messages:?}"
        );
        assert!(session.drain_animation_warnings().is_empty());
    }

    #[test]
    fn stop_runs_on_destroy_and_reports_events() {
        let mut session = start(ROTATE).unwrap();
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "started")
        );
        let events = session.stop();
        assert!(events.iter().any(|event| event.message == "destroyed"));
    }

    #[test]
    fn heavy_list_prefab_with_heavy_scripts_needs_the_library_before_play() {
        // Ассет `missing.lua` не существует: превью читает только документы префабов.
        const SCRIPTED: &str = "[[assets]]\npath = \"/missing.lua\"\n[[objects]]\nid = \"root\"\n[[objects.heavy_scripts]]\nname = \"Bobber\"\n";
        const PLAIN: &str = "[[objects]]\nid = \"root\"\n";
        let files = [
            ("prefabs/tower.rveprefab", SCRIPTED),
            ("prefabs/hut.rveprefab", PLAIN),
            ("prefabs/broken.rveprefab", "[[objects"),
        ];
        let document = document(SCENE);
        let mut read = files_reader(&files);
        let catalog = heavy_catalog(&["prefabs/tower.rveprefab"]);
        let (prefabs, warnings) = PlayPrefabs::load(&document, &mut read, &[], &catalog);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(prefabs.has_heavy_scripts());

        for path in ["prefabs/hut.rveprefab", "prefabs/broken.rveprefab"] {
            let catalog = heavy_catalog(&[path]);
            let (prefabs, _) = PlayPrefabs::load(&document, &mut read, &[], &catalog);
            assert!(!prefabs.has_heavy_scripts(), "{path}");
        }
    }

    #[test]
    fn heavy_script_without_a_library_refuses_to_start() {
        let source = format!("{SCENE}\n[[objects.heavy_scripts]]\nname = \"Bobber\"\n");
        let error = PlaySession::start(&document(&source), None, |_| Ok(ROTATE.to_owned()))
            .err()
            .unwrap();
        assert!(error.contains("Bobber"), "{error}");
    }

    #[test]
    fn unreadable_or_broken_lua_refuses_to_start() {
        let error = PlaySession::start(&document(SCENE), None, |_| Err("нет файла".to_owned()))
            .err()
            .unwrap();
        assert!(error.contains("нет файла"));
        assert!(start("this is not lua").is_err());
    }

    #[test]
    fn watchdog_interrupts_a_hanging_lua_update_and_disables_lua() {
        let hanging =
            "local H = {}\nfunction H:on_update(dt)\n  while true do end\nend\nreturn H\n";
        let mut session = PlaySession::start_with_limit(
            &document(SCENE),
            None,
            |_| Ok(hanging.to_owned()),
            Duration::from_millis(50),
        )
        .unwrap();
        session.step(0.1).unwrap();
        assert!(!session.drain_events().is_empty());
        // Прерывание постоянно: следующий кадр не должен снова зависнуть.
        session.step(0.1).unwrap();
    }

    #[test]
    fn watchdog_interrupts_a_hanging_top_level_chunk() {
        let result = PlaySession::start_with_limit(
            &document(SCENE),
            None,
            |_| Ok("while true do end".to_owned()),
            Duration::from_millis(50),
        );
        assert!(result.is_err());
    }

    #[test]
    fn input_snapshot_reaches_the_script() {
        let script = r#"
local S = {}
function S:on_update(dt)
  if rve.input.key_pressed("Space") then rve.log_info("space") end
end
return S
"#;
        let mut session = start(script).unwrap();
        session.input_mut().set_key(engine_script::Key::Space, true);
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "space")
        );
    }

    #[test]
    fn project_input_actions_reach_editor_play() {
        let script = r#"
local S = {}
function S:on_update()
  if rve.input.action_pressed("Jump") then rve.log_info("jump action") end
end
return S
"#;
        let input_settings = InputSettings {
            default_context: Some("game".to_owned()),
            contexts: std::collections::BTreeMap::from([(
                "game".to_owned(),
                std::collections::BTreeMap::from([(
                    "Jump".to_owned(),
                    InputAction::Button {
                        inputs: vec!["Space".to_owned()],
                    },
                )]),
            )]),
        };
        let mut session = start_with_input_settings(script, input_settings).unwrap();
        session.input_mut().set_key(engine_script::Key::Space, true);
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "jump action")
        );
    }

    #[test]
    fn clear_input_releases_buttons_and_discards_pointer_deltas() {
        let script = r#"
local S = {}
function S:on_update()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if rve.input.key_released("Space") and not rve.input.key_down("Space")
      and dx == 0 and dy == 0 and wx == 0 and wy == 0 then
    rve.log_info("play input cleared")
  end
end
return S
"#;
        let mut session = start(Box::leak(script.to_owned().into_boxed_str())).unwrap();
        let input = session.input_mut();
        input.set_key(engine_script::Key::Space, true);
        input.add_mouse_delta(10.0, -5.0);
        input.add_wheel(0.0, 2.0);
        session.clear_input();
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "play input cleared")
        );
    }

    #[test]
    fn console_commands_register_and_execute_in_play() {
        let script = r#"
local S = {}
function S:on_start()
  rve.console.register("echo", function(args)
    rve.console.write(args[1], "warning")
  end, "write one argument")
end
return S
"#;
        let mut session = start(script).unwrap();
        session.tick().unwrap();
        assert_eq!(
            session.command_catalog().get("echo").unwrap().description,
            "write one argument"
        );
        session
            .execute_command(&CommandLine {
                name: "echo".to_owned(),
                args: vec!["hello".to_owned()],
            })
            .unwrap();
        let event = session.drain_events().pop().unwrap();
        assert_eq!(event.level, engine_script::ScriptEventLevel::Warn);
        assert_eq!(event.message, "hello");
    }

    const AUDIO_SCENE: &str = r#"
[[assets]]
path = "/audio/hum.wav"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "speaker"
[objects.transform]
position = [3.0, 0.0, 0.0]
[objects.audio_source]
clip = "/audio/hum.wav"
looping = true
play_on_start = true
spatial = true
"#;

    #[test]
    fn a_clip_that_cannot_load_is_reported_for_the_editor_log() {
        let mut session =
            PlaySession::start(&document(AUDIO_SCENE), None, |_| Ok(String::new())).unwrap();
        // Без ресурсов проекта play_on_start-клип не читается; барьер ждёт, пока поток это запишет.
        session.audio.as_ref().unwrap().handle.barrier().unwrap();
        let warnings = session.drain_audio_warnings();
        assert!(
            warnings
                .iter()
                .any(|message| message.contains("audio/hum.wav")),
            "{warnings:?}"
        );
        assert!(
            session.drain_audio_warnings().is_empty(),
            "предупреждение показывается один раз"
        );
    }

    #[test]
    fn play_audio_follows_a_moved_source_without_build_frame() {
        // Позиция отслеживается только у живого голоса: без читаемого клипа он завершается сразу.
        // Поэтому клип зациклен и читается из временного каталога ресурсов.
        let parent = std::env::temp_dir().join(format!("rve-play-audio-{}", std::process::id()));
        let assets = parent.join("Assets");
        std::fs::create_dir_all(assets.join("audio")).unwrap();
        std::fs::write(
            assets.join("audio").join("tone.ogg"),
            include_bytes!("../../engine/engine-audio/tests/fixtures/tone_mono_22050.ogg"),
        )
        .unwrap();
        let mut resources =
            engine_resources::ResourceSystem::start(engine_resources::ResourceConfig {
                root: assets,
                source_mode: engine_resources::SourceMode::Loose,
                ..engine_resources::ResourceConfig::default()
            })
            .unwrap();
        let scene = document(&AUDIO_SCENE.replace("hum.wav", "tone.ogg"));
        let world = World::from_document(&scene).unwrap();
        let audio = AudioConfig {
            resources: Some(resources.handle()),
            output: false,
        };
        let mut session = PlaySession::start_with_world_limit(
            &scene,
            world,
            None,
            &InputSettings::default(),
            |_| Ok(String::new()),
            PlayPrefabs::default(),
            Vec::new(),
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            audio,
            AnimationBridge::default(),
        )
        .unwrap();
        let speaker = session.world_mut().handle("speaker").unwrap();
        session.tick().unwrap();
        let bridge = &session.audio.as_ref().unwrap().bridge;
        assert_eq!(bridge.emitter_of(speaker), Some([3.0, 0.0, 0.0]));

        let world = session.world_mut();
        let mut transform = *world.local_transform(speaker).unwrap();
        transform.position.x = 5.0;
        world.set_local_transform(speaker, transform).unwrap();
        session.tick().unwrap();
        let bridge = &session.audio.as_ref().unwrap().bridge;
        assert_eq!(bridge.emitter_of(speaker), Some([5.0, 0.0, 0.0]));

        drop(session);
        resources.shutdown().unwrap();
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn play_pause_and_stop_drive_the_audio_thread_without_hanging() {
        let mut session =
            PlaySession::start(&document(AUDIO_SCENE), None, |_| Ok(String::new())).unwrap();
        session.set_paused(true);
        session.set_paused(false);
        // Stop ждёт завершения потока звука; если он не остановится, тест зависнет.
        assert!(session.stop().is_empty());
    }

    /// Файлы для тестов: ключи — пути без ведущего слеша, как в `[[assets]]`.
    fn files_reader<'a>(
        files: &'a [(&'a str, &'a str)],
    ) -> impl FnMut(&str) -> Result<String, String> + 'a {
        move |path: &str| {
            let key = path.trim_start_matches('/');
            files
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, text)| (*text).to_owned())
                .ok_or_else(|| format!("{path}: файл не найден"))
        }
    }

    /// Запускает Play со сценой `source`: префабы и скрипты читаются из `files`.
    fn start_with_prefabs(source: &str, files: &[(&str, &str)]) -> Result<PlaySession, String> {
        let document = document(source);
        let mut read = files_reader(files);
        let (prefabs, warnings) =
            PlayPrefabs::load(&document, &mut read, &[], &PrefabCatalogFile::default());
        assert!(warnings.is_empty(), "{warnings:?}");
        let world = World::from_document(&document).map_err(|error| error.to_string())?;
        PlaySession::start_with_world_limit(
            &document,
            world,
            None,
            &InputSettings::default(),
            read,
            prefabs,
            Vec::new(),
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            AudioConfig::default(),
            AnimationBridge::default(),
        )
    }

    const BALL_SCENE: &str = r#"
[[assets]]
path = "/prefabs/ball.rveprefab"
[[assets]]
path = "/spawner.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "gun"
[[objects.lite_scripts]]
path = "/spawner.lua"
"#;

    /// Быстрый префаб: динамический шар у потолка, со своим lite-скриптом.
    const BALL_PREFAB: &str = r#"
[[assets]]
path = "/ball.lua"
[[objects]]
id = "ball"
[objects.transform]
position = [0.0, 5.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
[objects.rigid_body]
kind = "dynamic"
mass = 1.0
[[objects.lite_scripts]]
path = "/ball.lua"
"#;

    const BALL_LUA: &str = r#"
local Ball = {}
function Ball:on_start()
  rve.log_info("ball start")
end
function Ball:on_destroy()
  rve.log_info("ball destroy")
end
return Ball
"#;

    const SPAWN_ONCE: &str = r#"
local Spawner = {}
local spawned = false
function Spawner:on_update(dt)
  if not spawned then
    spawned = true
    rve.spawn("prefabs/ball.rveprefab")
  end
end
return Spawner
"#;

    const SPAWN_THEN_DESTROY: &str = r#"
local Spawner = {}
local calls = 0
function Spawner:on_update(dt)
  calls = calls + 1
  if calls == 1 then
    rve.spawn("prefabs/ball.rveprefab", { id = "shot" })
  elseif calls == 3 then
    rve.destroy("shot")
  end
end
return Spawner
"#;

    fn ball_files(spawner: &str) -> [(&str, &str); 3] {
        [
            ("prefabs/ball.rveprefab", BALL_PREFAB),
            ("ball.lua", BALL_LUA),
            ("spawner.lua", spawner),
        ]
    }

    fn has_message(session: &mut PlaySession, message: &str) -> bool {
        session
            .drain_events()
            .iter()
            .any(|event| event.message == message)
    }

    #[test]
    fn spawned_prefab_gets_an_at_id_and_starts_on_the_next_tick() {
        let files = ball_files(SPAWN_ONCE);
        let mut session = start_with_prefabs(BALL_SCENE, &files).unwrap();
        session.step(0.1).unwrap();
        let id = session
            .world_mut()
            .objects()
            .iter()
            .map(|object| object.id.clone())
            .find(|id| id.starts_with("ball@"))
            .expect("spawned object has a generated id");
        assert!(session.world_mut().handle(&id).is_some());
        assert!(!has_message(&mut session, "ball start"));

        session.step(0.1).unwrap();
        assert!(has_message(&mut session, "ball start"));
        session.stop();
    }

    #[test]
    fn destroy_runs_on_destroy_removes_the_body_and_later_steps_stay_healthy() {
        let files = ball_files(SPAWN_THEN_DESTROY);
        let mut session = start_with_prefabs(BALL_SCENE, &files).unwrap();
        session.step(0.1).unwrap();
        let shot = session.world_mut().handle("shot").expect("spawned object");
        session.step(0.1).unwrap();
        assert!(has_message(&mut session, "ball start"));

        session.step(0.1).unwrap();
        assert!(has_message(&mut session, "ball destroy"));
        assert!(!session.world_mut().is_alive(shot));
        let physics = session.physics.as_ref().unwrap();
        assert!(
            physics
                .sync
                .bindings()
                .iter()
                .all(|binding| binding.object != shot)
        );

        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .all(|event| event.level != engine_script::ScriptEventLevel::Error)
        );
        session.stop();
    }

    #[test]
    fn spawned_dynamic_body_falls_under_physics() {
        let files = ball_files(SPAWN_ONCE);
        let mut session = start_with_prefabs(BALL_SCENE, &files).unwrap();
        session.step(1.0 / 60.0).unwrap();
        let ball = session
            .world_mut()
            .objects()
            .iter()
            .find(|object| object.id.starts_with("ball@"))
            .map(|object| object.handle)
            .expect("spawned object");
        let start_y = session
            .world_mut()
            .local_transform(ball)
            .unwrap()
            .position
            .y;
        for _ in 0..30 {
            session.step(1.0 / 60.0).unwrap();
        }
        let y = session
            .world_mut()
            .local_transform(ball)
            .unwrap()
            .position
            .y;
        assert!(y < start_y - 0.1, "{start_y} -> {y}");
        session.stop();
    }

    #[test]
    fn unusable_fast_prefabs_are_skipped_with_a_warning() {
        let source = r#"
[[assets]]
path = "/prefabs/ball.rveprefab"
[[assets]]
path = "/prefabs/broken.rveprefab"
[[assets]]
path = "/prefabs/cam.rveprefab"
[[objects]]
id = "camera"
[objects.camera]
"#;
        let files = [
            ("prefabs/ball.rveprefab", BALL_PREFAB),
            ("ball.lua", BALL_LUA),
            (
                "prefabs/cam.rveprefab",
                "[[objects]]\nid = \"cam\"\n[objects.camera]\n",
            ),
        ];
        let document = document(source);
        let mut read = files_reader(&files);
        let heavy = PrefabCatalogFile::default();
        let (prefabs, warnings) = PlayPrefabs::load(&document, &mut read, &[], &heavy);
        assert!(prefabs.catalog.get("prefabs/ball.rveprefab").is_some());
        assert!(prefabs.catalog.get("prefabs/broken.rveprefab").is_none());
        assert!(prefabs.catalog.get("prefabs/cam.rveprefab").is_none());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|warning| warning.contains("broken")));

        // Префаб с камерой, который экземпляр сцены использует как есть, предупреждения не даёт.
        let instanced = [VirtualPath::parse("prefabs/cam.rveprefab").unwrap()];
        let (_, warnings) = PlayPrefabs::load(&document, &mut read, &instanced, &heavy);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn fast_prefab_animator_is_reported_once_as_unsupported_for_spawn() {
        // Регрессия: аниматор шаблона при спавне не работает; это видно в логе Play, а не молча.
        let source = "[[assets]]\npath = \"/prefabs/hero.rveprefab\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n";
        let files = [(
            "prefabs/hero.rveprefab",
            "[[assets]]\npath = \"/models/hero.fbx\"\n[[assets]]\npath = \"/anims/walk.fbx\"\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\nclips = [\"/anims/walk.fbx\"]\n",
        )];
        let document = document(source);
        let mut read = files_reader(&files);
        let (prefabs, warnings) =
            PlayPrefabs::load(&document, &mut read, &[], &PrefabCatalogFile::default());
        assert!(prefabs.catalog.get("prefabs/hero.rveprefab").is_some());
        let animator_warnings: Vec<_> = warnings
            .iter()
            .filter(|warning| warning.contains("аниматор"))
            .collect();
        assert_eq!(animator_warnings.len(), 1, "{warnings:?}");
    }

    const BALL: &str = "prefabs/ball.rveprefab";
    const HEAVY_TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Сцена без быстрых префабов: только скрипт-спавнер.
    const HEAVY_SCENE: &str = r#"
[[assets]]
path = "/spawner.lua"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "gun"
[[objects.lite_scripts]]
path = "/spawner.lua"
"#;

    const SPAWN_MISSING: &str = r#"
local Spawner = {}
local spawned = false
function Spawner:on_update(dt)
  if not spawned then
    spawned = true
    rve.spawn("prefabs/missing.rveprefab", { id = "shot" })
  end
end
return Spawner
"#;

    const SPAWN_CAMERA: &str = r#"
local Spawner = {}
local spawned = false
function Spawner:on_update(dt)
  if not spawned then
    spawned = true
    rve.spawn("prefabs/cam.rveprefab", { id = "shot" })
  end
end
return Spawner
"#;

    const CAMERA_PREFAB: &str = "[[objects]]\nid = \"cam\"\n[objects.camera]\n";

    /// Создаёт шар в первом кадре, выгружает префаб во втором и уничтожает шар в третьем.
    const SPAWN_UNLOAD_DESTROY: &str = r#"
local Spawner = {}
local step = 0
function Spawner:on_update(dt)
  step = step + 1
  if step == 1 then rve.spawn("prefabs/ball.rveprefab", { id = "shot" }) end
  if step == 2 then rve.prefabs.unload("prefabs/ball.rveprefab") end
  if step == 3 then rve.destroy("shot") end
end
return Spawner
"#;

    /// Список проекта с путями без `max_instances`.
    fn heavy_catalog(paths: &[&str]) -> PrefabCatalogFile {
        PrefabCatalogFile {
            prefabs: paths
                .iter()
                .map(|path| PrefabCatalogEntry {
                    path: (*path).to_owned(),
                    archive: None,
                    max_instances: None,
                })
                .collect(),
        }
    }

    /// Временный каталог `Assets` с `files`. Файлы читаются через `ResourceSystem`, как в
    /// редакторе; каталог удаляется вместе с объектом.
    struct TempAssets {
        parent: PathBuf,
        resources: ResourceSystem,
    }

    impl TempAssets {
        fn new(files: &[(&str, &str)]) -> Self {
            static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
            let parent = std::env::temp_dir().join(format!(
                "rve-play-heavy-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            let root = parent.join("Assets");
            std::fs::create_dir_all(&root).unwrap();
            for (name, text) in files {
                let file = root.join(name);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, text).unwrap();
            }
            let resources = ResourceSystem::start(ResourceConfig {
                root,
                source_mode: SourceMode::Loose,
                ..ResourceConfig::default()
            })
            .unwrap();
            Self { parent, resources }
        }

        fn handle(&self) -> ResourceHandle {
            self.resources.handle()
        }
    }

    impl Drop for TempAssets {
        fn drop(&mut self) {
            let _ = self.resources.shutdown();
            let _ = std::fs::remove_dir_all(&self.parent);
        }
    }

    /// Запускает Play со сценой `source`; тяжёлые префабы из `catalog` грузятся из `assets`.
    fn start_heavy(source: &str, assets: &TempAssets, catalog: &PrefabCatalogFile) -> PlaySession {
        let document = document(source);
        let handle = assets.handle();
        let mut read = |path: &str| read_resource_text(&handle, path);
        let (prefabs, warnings) = PlayPrefabs::load(&document, &mut read, &[], catalog);
        assert!(warnings.is_empty(), "{warnings:?}");
        let world = World::from_document(&document).unwrap();
        PlaySession::start_with_world_limit(
            &document,
            world,
            None,
            &InputSettings::default(),
            read,
            prefabs.with_resources(assets.handle()),
            Vec::new(),
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            AudioConfig::default(),
            AnimationBridge::default(),
        )
        .unwrap()
    }

    /// Ждёт ответа фоновой загрузки и применяет его, как это делает `step`.
    fn finish_heavy_load(session: &mut PlaySession) {
        let reply = session
            .heavy
            .replies
            .recv_timeout(HEAVY_TEST_TIMEOUT)
            .expect("фоновая загрузка ответила");
        session.apply_heavy_reply(reply);
    }

    fn spawned_ball(session: &mut PlaySession) -> Option<ObjectHandle> {
        session
            .world_mut()
            .objects()
            .iter()
            .find(|object| object.id.starts_with("ball@"))
            .map(|object| object.handle)
    }

    #[test]
    fn heavy_spawn_waits_for_its_load_then_appears_and_starts_on_the_next_tick() {
        let assets = TempAssets::new(&ball_files(SPAWN_ONCE));
        let mut session = start_heavy(
            HEAVY_SCENE,
            &assets,
            &heavy_catalog(&["/prefabs/ball.rveprefab"]),
        );
        session.step(0.1).unwrap();
        assert!(spawned_ball(&mut session).is_none());
        assert_eq!(
            session.host.prefab_status(BALL),
            Some(PrefabStatus::Loading)
        );

        finish_heavy_load(&mut session);
        session.step(0.1).unwrap();
        assert!(spawned_ball(&mut session).is_some());
        assert!(!has_message(&mut session, "ball start"));

        session.step(0.1).unwrap();
        assert!(has_message(&mut session, "ball start"));
        session.stop();
    }

    /// Spawn префаба, который не загрузится: объект мёртв, причина попадает в Error-события.
    fn assert_heavy_spawn_fails(files: &[(&str, &str)], path: &str) {
        let assets = TempAssets::new(files);
        let mut session = start_heavy(HEAVY_SCENE, &assets, &heavy_catalog(&[path]));
        session.step(0.1).unwrap();
        finish_heavy_load(&mut session);
        assert_eq!(
            session.host.prefab_status(path.trim_start_matches('/')),
            Some(PrefabStatus::Failed)
        );
        assert!(session.world_mut().handle("shot").is_none());
        let events = session.drain_events();
        assert!(
            events.iter().any(|event| {
                event.level == engine_script::ScriptEventLevel::Error
                    && event.message.contains("failed to load")
            }),
            "{events:?}"
        );
        session.stop();
    }

    #[test]
    fn heavy_prefab_that_cannot_be_read_kills_its_pending_spawn_with_an_error() {
        assert_heavy_spawn_fails(
            &[("spawner.lua", SPAWN_MISSING)],
            "/prefabs/missing.rveprefab",
        );
    }

    #[test]
    fn heavy_prefab_with_a_camera_is_rejected_when_it_loads() {
        assert_heavy_spawn_fails(
            &[
                ("spawner.lua", SPAWN_CAMERA),
                ("prefabs/cam.rveprefab", CAMERA_PREFAB),
            ],
            "/prefabs/cam.rveprefab",
        );
    }

    #[test]
    fn unloading_after_the_last_instance_is_destroyed_returns_the_prefab_to_unloaded() {
        let assets = TempAssets::new(&ball_files(SPAWN_UNLOAD_DESTROY));
        let mut session = start_heavy(
            HEAVY_SCENE,
            &assets,
            &heavy_catalog(&["/prefabs/ball.rveprefab"]),
        );
        session.step(0.1).unwrap();
        finish_heavy_load(&mut session);
        // Выгрузка ждёт объект: пока он жив, префаб остаётся загруженным.
        session.step(0.1).unwrap();
        assert!(session.world_mut().handle("shot").is_some());
        assert_eq!(session.host.prefab_status(BALL), Some(PrefabStatus::Loaded));

        session.step(0.1).unwrap();
        assert!(session.world_mut().handle("shot").is_none());
        assert_eq!(
            session.host.prefab_status(BALL),
            Some(PrefabStatus::Unloaded)
        );
        session.stop();
    }

    #[test]
    fn a_fast_prefab_of_the_scene_takes_precedence_over_the_project_list() {
        let assets = TempAssets::new(&ball_files(SPAWN_ONCE));
        let mut session = start_heavy(
            BALL_SCENE,
            &assets,
            &heavy_catalog(&["/prefabs/ball.rveprefab"]),
        );
        session.step(0.1).unwrap();
        assert_eq!(
            session.host.prefab_status(BALL),
            Some(PrefabStatus::Resident)
        );
        assert!(spawned_ball(&mut session).is_some());
        assert!(session.heavy.pending.is_empty());
        session.stop();
    }

    #[test]
    fn a_reply_of_a_superseded_load_is_discarded() {
        let assets = TempAssets::new(&ball_files(SPAWN_ONCE));
        let mut session = start_heavy(
            HEAVY_SCENE,
            &assets,
            &heavy_catalog(&["/prefabs/ball.rveprefab"]),
        );
        session.step(0.1).unwrap();
        let current = session.heavy.pending[BALL];
        session.apply_heavy_reply(HeavyReply {
            path: BALL.to_owned(),
            load_id: current + 1,
            result: Err("устаревший ответ".to_owned()),
        });
        assert_eq!(
            session.host.prefab_status(BALL),
            Some(PrefabStatus::Loading)
        );
        assert!(session.drain_events().is_empty());

        finish_heavy_load(&mut session);
        assert_eq!(session.host.prefab_status(BALL), Some(PrefabStatus::Loaded));
        session.stop();
    }

    #[test]
    fn dropping_play_does_not_wait_for_a_heavy_load_in_progress() {
        let assets = TempAssets::new(&ball_files(SPAWN_ONCE));
        let mut session = start_heavy(
            HEAVY_SCENE,
            &assets,
            &heavy_catalog(&["/prefabs/ball.rveprefab"]),
        );
        session.step(0.1).unwrap();
        assert!(session.heavy.pending.contains_key(BALL));
        drop(session);
    }
}
