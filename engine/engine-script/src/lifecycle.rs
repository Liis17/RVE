//! Создание и удаление объектов сцены из скриптов.
//!
//! Транзакция callback кладёт заявки в [`Lifecycle`] только при успешном коммите. `ScriptHost`
//! применяет их в конце фазы, когда у него есть `&mut World`. Состояние разделяется между
//! транзакцией (проверки id и префабов при вызове API) и хостом (очереди и журнал изменений).

use engine_files::VirtualPath;
use engine_physics::BodyHandle;
use engine_scene::{ObjectHandle, PrefabTemplate, SpawnOptions, World};
use glam::Vec3;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Наибольшее число spawn-объектов, которые хост применяет за один кадр.
pub(crate) const MAX_SPAWNED_OBJECTS_PER_FRAME: usize = 4096;
/// Наибольшая длина имени spawn-объекта в UTF-8 байтах.
pub(crate) const MAX_SPAWN_NAME_BYTES: usize = 256;
/// Предел привязок удалённых объектов. Окно закрывается только результатом шага физики, поэтому
/// при паузе физики без предела они копились бы; при переполнении все привязки сбрасываются.
pub(crate) const MAX_RETIRED_OBJECTS: usize = 65_536;
/// Предел пар «экземпляр + мёртвый объект», по которым уже выдан Warn. При переполнении
/// набор сбрасывается, и допустим повторный Warn для тех же пар.
pub(crate) const MAX_WARNED_PAIRS: usize = 65_536;

/// Быстрый префаб загружается вместе со сценой, тяжёлый — по требованию во время игры.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefabKind {
    Fast,
    Heavy,
}

/// Настройки префаба из списка, который получает хост.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrefabSpec {
    pub kind: PrefabKind,
    /// Лимит живых корней, созданных spawn; при превышении удаляется самый старый.
    pub max_instances: Option<u32>,
}

/// Ключ префаба в каталоге: канонический virtual path без ведущего слеша. Скрипты и ABI могут
/// передавать путь и как в файлах сцен (`/prefabs/x.rveprefab`), и без слеша; оба дают один ключ.
pub(crate) fn prefab_key(path: &str) -> Result<String, String> {
    VirtualPath::parse(path)
        .map(|path| path.as_str().to_owned())
        .map_err(|_| format!("prefab path {path:?} is not a valid asset path"))
}

/// Путь префаба в сообщениях об ошибках: всегда с ведущим слешем, как в файлах сцен.
pub(crate) fn prefab_display(key: &str) -> String {
    format!("/{key}")
}

/// Префабы, которые скрипты могут создавать. Ключ — канонический virtual path без ведущего слеша.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrefabCatalog {
    entries: BTreeMap<String, PrefabSpec>,
}

impl PrefabCatalog {
    pub fn insert(&mut self, path: impl Into<String>, spec: PrefabSpec) {
        self.entries.insert(path.into(), spec);
    }

    pub fn get(&self, path: &str) -> Option<PrefabSpec> {
        self.entries.get(path).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, PrefabSpec)> {
        self.entries
            .iter()
            .map(|(path, spec)| (path.as_str(), *spec))
    }
}

/// Состояние префаба. Быстрый префаб сразу `Resident` после регистрации; у тяжёлого
/// `Unloaded` → `Loading` → `Loaded` → `Unloading` → `Unloaded` либо `Failed`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefabStatus {
    Resident,
    Unloaded,
    Loading,
    Loaded,
    Unloading,
    Failed,
}

impl PrefabStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Unloaded => "unloaded",
            Self::Loading => "loading",
            Self::Loaded => "loaded",
            Self::Unloading => "unloading",
            Self::Failed => "failed",
        }
    }
}

/// Загрузка или выгрузка тяжёлого префаба. Хост выдаёт их вызывающему коду в [`WorldChanges`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrefabRequest {
    Load(String),
    Unload(String),
}

/// Корень, созданный `spawn`, и все объекты его шаблона.
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnedObject {
    /// Корень поддерева: хендл, который вернул `rve.spawn`.
    pub handle: ObjectHandle,
    /// Все объекты шаблона в его порядке; корень стоит на своей позиции.
    pub objects: Vec<ObjectHandle>,
    pub prefab: String,
    /// Номер регистрации шаблона этого пути: 0 для первой, затем по одному на каждую.
    pub template_index: usize,
    /// Скорость корня, которую вызывающий код задаёт телу после вставки в физику (`SetVelocity`).
    pub initial_velocity: Option<Vec3>,
}

/// Изменения мира, накопленные с прошлого `drain_world_changes`.
///
/// `spawned` идёт раньше `despawned`: объект, созданный и удалённый в одной фазе, попадает в
/// оба списка в этом порядке.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorldChanges {
    pub spawned: Vec<SpawnedObject>,
    /// Все удалённые хендлы, включая потомков; потомки идут раньше родителей.
    pub despawned: Vec<ObjectHandle>,
    pub prefab_requests: Vec<PrefabRequest>,
}

/// Состояние объекта для ABI и Lua: живой, pending (зарезервированный) или мёртвый.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectState {
    Alive,
    Pending,
    Dead,
}

/// Опции `rve.spawn` в виде, который понимает ядро.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SpawnArgs {
    pub(crate) options: SpawnOptions,
    pub(crate) velocity: Option<Vec3>,
}

/// Заявка на создание; `root` зарезервирован в мире ещё при вызове.
#[derive(Clone, Debug)]
pub(crate) struct SpawnRequest {
    pub(crate) seq: u64,
    pub(crate) root: ObjectHandle,
    pub(crate) prefab: String,
    pub(crate) args: SpawnArgs,
    pub(crate) source: String,
}

/// Заявка на удаление поддерева. `due = None` — в конце текущей фазы.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DestroyRequest {
    pub(crate) handle: ObjectHandle,
    pub(crate) due: Option<f64>,
}

pub(crate) struct PrefabSlot {
    pub(crate) kind: PrefabKind,
    pub(crate) max_instances: Option<u32>,
    pub(crate) status: PrefabStatus,
    pub(crate) template: Option<Arc<PrefabTemplate>>,
    /// Номер текущего шаблона: сколько раз префаб уже был зарегистрирован, минус один.
    pub(crate) template_index: usize,
    registrations: usize,
    /// Живые корни, созданные spawn.
    pub(crate) live_count: usize,
    /// Корни в порядке создания; ведётся только при `max_instances`.
    pub(crate) live: VecDeque<ObjectHandle>,
    /// Выгрузка отложена до последнего экземпляра.
    pub(crate) unload_requested: bool,
    /// Загрузка запрошена во время выгрузки и выполнится после её завершения.
    pub(crate) load_requested: bool,
}

impl PrefabSlot {
    pub(crate) fn new(spec: PrefabSpec) -> Self {
        Self {
            kind: spec.kind,
            max_instances: spec.max_instances,
            status: PrefabStatus::Unloaded,
            template: None,
            template_index: 0,
            registrations: 0,
            live_count: 0,
            live: VecDeque::new(),
            unload_requested: false,
            load_requested: false,
        }
    }

    /// Регистрирует шаблон: возвращает его номер.
    pub(crate) fn register(&mut self, template: Arc<PrefabTemplate>) -> usize {
        let index = self.registrations;
        self.registrations += 1;
        self.template_index = index;
        self.template = Some(template);
        index
    }
}

/// Объект, которого уже нет в мире, но который ещё нужен физике: привязка тела живёт до
/// следующего результата шага.
pub(crate) struct RetiredObject {
    pub(crate) body: BodyHandle,
    pub(crate) id: String,
    pub(crate) name: String,
    stamp: u64,
}

/// Состояние создания и удаления, общее для хоста и транзакций.
#[derive(Default)]
pub(crate) struct Lifecycle {
    /// Время `elapsed` текущей фазы; от него считаются задержки `destroy`.
    pub(crate) now: f64,
    next_seq: u64,
    pub(crate) prefabs: BTreeMap<String, PrefabSlot>,
    /// Явные id заявок, которые закоммичены, но ещё не применены.
    pub(crate) pending_ids: HashMap<String, ObjectHandle>,
    /// Закоммиченные заявки, которые хост применит в конце фазы. Хост снимает их по одной с начала,
    /// поэтому оставшиеся заявки видны `begin_unload_if_idle` во время обработки.
    pub(crate) queued: VecDeque<SpawnRequest>,
    /// Заявки, ждущие загрузки префаба.
    pub(crate) waiting: Vec<SpawnRequest>,
    pub(crate) destroys: Vec<DestroyRequest>,
    /// Отложенные удаления: ранний срок побеждает.
    pub(crate) scheduled: HashMap<ObjectHandle, f64>,
    pub(crate) changes: WorldChanges,
    /// Живые корни spawn и их префабы.
    pub(crate) roots: HashMap<ObjectHandle, String>,
    retired: HashMap<ObjectHandle, RetiredObject>,
    retired_bodies: HashMap<BodyHandle, ObjectHandle>,
    snapshot_seq: u64,
    /// Пары «экземпляр + мёртвый объект», по которым уже выдан Warn.
    warned: HashMap<u64, HashSet<ObjectHandle>>,
    warned_pairs: usize,
}

impl Lifecycle {
    pub(crate) fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    /// Применяет заявку загрузки или выгрузки, закоммиченную транзакцией.
    pub(crate) fn apply_prefab_request(&mut self, request: PrefabRequest) {
        match request {
            PrefabRequest::Load(path) => {
                let Some(slot) = self.prefabs.get_mut(&path) else {
                    return;
                };
                if slot.kind != PrefabKind::Heavy {
                    return;
                }
                match slot.status {
                    PrefabStatus::Unloaded | PrefabStatus::Failed => {
                        slot.status = PrefabStatus::Loading;
                        slot.unload_requested = false;
                        self.changes.prefab_requests.push(PrefabRequest::Load(path));
                    }
                    PrefabStatus::Unloading => slot.load_requested = true,
                    PrefabStatus::Loaded => slot.unload_requested = false,
                    PrefabStatus::Loading | PrefabStatus::Resident => {}
                }
            }
            PrefabRequest::Unload(path) => {
                let Some(slot) = self.prefabs.get_mut(&path) else {
                    return;
                };
                if slot.kind != PrefabKind::Heavy {
                    return;
                }
                if matches!(slot.status, PrefabStatus::Loaded | PrefabStatus::Loading) {
                    slot.unload_requested = true;
                }
                self.begin_unload_if_idle(&path);
            }
        }
    }

    /// Переводит загруженный префаб в `Unloading`, если отложенная выгрузка больше не ждёт экземпляров.
    /// Ждёт, пока у префаба не останется ни экземпляров, ни заявок spawn в очереди или ожидании.
    pub(crate) fn begin_unload_if_idle(&mut self, path: &str) {
        let spawns_pending = self
            .queued
            .iter()
            .chain(self.waiting.iter())
            .any(|request| request.prefab == path);
        let Some(slot) = self.prefabs.get_mut(path) else {
            return;
        };
        if slot.status == PrefabStatus::Loaded
            && slot.unload_requested
            && slot.live_count == 0
            && !spawns_pending
        {
            slot.status = PrefabStatus::Unloading;
            slot.unload_requested = false;
            self.changes
                .prefab_requests
                .push(PrefabRequest::Unload(path.to_owned()));
        }
    }

    /// Вызывающий код освободил ресурсы выгруженного префаба.
    pub(crate) fn prefab_released(&mut self, path: &str) {
        let waiting = self.waiting.iter().any(|request| request.prefab == path);
        let Some(slot) = self.prefabs.get_mut(path) else {
            return;
        };
        if slot.status != PrefabStatus::Unloading {
            return;
        }
        slot.status = PrefabStatus::Unloaded;
        slot.template = None;
        slot.unload_requested = false;
        let reload = std::mem::take(&mut slot.load_requested) || waiting;
        if reload {
            slot.status = PrefabStatus::Loading;
            self.changes
                .prefab_requests
                .push(PrefabRequest::Load(path.to_owned()));
        }
    }

    /// Запоминает данные объекта, которого уже нет в мире, для физических событий и предупреждений.
    pub(crate) fn retire(
        &mut self,
        handle: ObjectHandle,
        body: BodyHandle,
        id: String,
        name: String,
    ) {
        if self.retired.len() >= MAX_RETIRED_OBJECTS {
            self.retired.clear();
            self.retired_bodies.clear();
        }
        self.retired_bodies.insert(body, handle);
        self.retired.insert(
            handle,
            RetiredObject {
                body,
                id,
                name,
                stamp: self.snapshot_seq,
            },
        );
    }

    pub(crate) fn retired(&self, handle: ObjectHandle) -> Option<&RetiredObject> {
        self.retired.get(&handle)
    }

    pub(crate) fn retired_body(&self, body: BodyHandle) -> Option<ObjectHandle> {
        self.retired_bodies.get(&body).copied()
    }

    /// Новый результат шага физики. Привязки удалённых тел живут до следующего, а не дольше.
    pub(crate) fn advance_physics_snapshot(&mut self) {
        self.snapshot_seq += 1;
        let current = self.snapshot_seq;
        let bodies = &mut self.retired_bodies;
        self.retired.retain(|_, retired| {
            let keep = retired.stamp + 1 >= current;
            if !keep {
                bodies.remove(&retired.body);
            }
            keep
        });
    }

    /// Отмечает пару и возвращает `true`, если Warn по ней ещё не выдавали.
    pub(crate) fn mark_warned(&mut self, instance: u64, handle: ObjectHandle) -> bool {
        if self.has_warned(instance, handle) {
            return false;
        }
        if self.warned_pairs >= MAX_WARNED_PAIRS {
            self.warned.clear();
            self.warned_pairs = 0;
        }
        self.warned_pairs += 1;
        self.warned.entry(instance).or_default().insert(handle)
    }

    pub(crate) fn has_warned(&self, instance: u64, handle: ObjectHandle) -> bool {
        self.warned
            .get(&instance)
            .is_some_and(|objects| objects.contains(&handle))
    }

    pub(crate) fn forget_instance(&mut self, instance: u64) {
        if let Some(objects) = self.warned.remove(&instance) {
            self.warned_pairs -= objects.len();
        }
    }

    /// Заявка, которая ещё не создала объект, но уже резервирует хендл.
    pub(crate) fn pending_spawn(&self, root: ObjectHandle) -> Option<&SpawnRequest> {
        self.queued
            .iter()
            .chain(self.waiting.iter())
            .find(|request| request.root == root)
    }
}

/// Допустимый авторский id spawn-объекта: `[A-Za-z0-9_-]{1,64}`.
/// Проверяет заявку spawn до резервирования корня: префаб и его статус, лимит кадра, параметры
/// и явный id. `pending_spawns` и `pending_ids` описывают заявки, уже принятые в этом вызове.
pub(crate) fn check_spawn_request(
    world: &World,
    lifecycle: &Lifecycle,
    pending_spawns: usize,
    pending_ids: &HashMap<String, ObjectHandle>,
    prefab: &str,
    args: &SpawnArgs,
) -> Result<(), String> {
    let slot = lifecycle.prefabs.get(prefab).ok_or_else(|| {
        format!(
            "prefab {:?} is not in the project prefab list",
            prefab_display(prefab)
        )
    })?;
    if slot.status == PrefabStatus::Failed {
        return Err(format!(
            "prefab {:?} failed to load",
            prefab_display(prefab)
        ));
    }
    if slot.kind == PrefabKind::Fast && slot.template.is_none() {
        return Err(format!(
            "prefab {:?} is not registered",
            prefab_display(prefab)
        ));
    }
    if lifecycle.queued.len() + lifecycle.waiting.len() + pending_spawns
        >= MAX_SPAWNED_OBJECTS_PER_FRAME
    {
        return Err(format!(
            "spawn requests limit ({MAX_SPAWNED_OBJECTS_PER_FRAME} per frame) reached"
        ));
    }
    let options = &args.options;
    if [options.position, options.rotation_deg, options.scale]
        .into_iter()
        .flatten()
        .any(|value| !value.is_finite())
        || args.velocity.is_some_and(|value| !value.is_finite())
    {
        return Err("spawn transform and velocity must be finite".to_owned());
    }
    if let Some(parent) = options.parent
        && !world.is_alive(parent)
    {
        return Err("spawn parent must be an existing object".to_owned());
    }
    if options
        .name
        .as_ref()
        .is_some_and(|name| name.len() > MAX_SPAWN_NAME_BYTES)
    {
        return Err(format!(
            "spawn name must be at most {MAX_SPAWN_NAME_BYTES} bytes"
        ));
    }
    if let Some(id) = &options.id {
        if !valid_spawn_id(id) {
            return Err(format!("spawn id {id:?} must match [A-Za-z0-9_-]{{1,64}}"));
        }
        if world.handle(id).is_some()
            || pending_ids.contains_key(id)
            || lifecycle.pending_ids.contains_key(id)
        {
            return Err(format!("object id {id:?} is already used"));
        }
    }
    Ok(())
}

pub(crate) fn valid_spawn_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(slot: u64) -> ObjectHandle {
        ObjectHandle::from_abi(slot).expect("slot is non-zero")
    }

    #[test]
    fn warned_pairs_warn_once_and_forgotten_instances_release_their_count() {
        let mut lifecycle = Lifecycle::default();
        assert!(lifecycle.mark_warned(1, handle(1)));
        assert!(!lifecycle.mark_warned(1, handle(1)));
        assert!(lifecycle.mark_warned(2, handle(1)));
        lifecycle.forget_instance(1);
        assert_eq!(lifecycle.warned_pairs, 1);
        assert!(!lifecycle.has_warned(1, handle(1)));
    }

    #[test]
    fn warned_pairs_stay_bounded_and_may_warn_again_after_the_reset() {
        let mut lifecycle = Lifecycle::default();
        assert!(lifecycle.mark_warned(1, handle(1)));
        for slot in 1..=MAX_WARNED_PAIRS as u64 {
            lifecycle.mark_warned(2, handle(slot));
        }
        assert!(lifecycle.warned_pairs <= MAX_WARNED_PAIRS);
        assert!(
            !lifecycle.has_warned(1, handle(1)),
            "the first pair was reset"
        );
        assert!(lifecycle.mark_warned(1, handle(1)));
    }

    #[test]
    fn retired_objects_stay_bounded_while_physics_does_not_advance() {
        let mut lifecycle = Lifecycle::default();
        for slot in 1..=MAX_RETIRED_OBJECTS as u64 + 1 {
            lifecycle.retire(
                handle(slot),
                BodyHandle(slot),
                format!("id{slot}"),
                String::new(),
            );
        }
        assert!(lifecycle.retired.len() <= MAX_RETIRED_OBJECTS);
        assert_eq!(lifecycle.retired.len(), lifecycle.retired_bodies.len());
        assert!(
            lifecycle
                .retired(handle(MAX_RETIRED_OBJECTS as u64 + 1))
                .is_some()
        );
    }
}
