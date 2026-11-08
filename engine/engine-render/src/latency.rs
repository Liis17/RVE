//! NVIDIA Reflex через `VK_NV_low_latency2` и `VK_KHR_present_id`.
//!
//! Владение и порядок блокировок:
//! - `Shared::swapchain` — текущий swapchain (null = неактивно). Блокировка держится на замене
//!   handle (`replace`, `deactivate`) и на вызовах `vkSetLatencySleepModeNV`/`vkSetLatencyMarkerNV`
//!   из `sleep` и `marker`. Так swapchain, заменённый в `replace`, не уничтожается во время
//!   Reflex-вызова на нём. Acquire, present и создание swapchain блокировку не берут: спецификация
//!   разрешает параллельные Reflex-вызовы на swapchain, кроме `vkDestroySwapchainKHR`.
//! - `Shared::alive` — внешний замок: `sleep` и `marker` держат read-lock, `sleep` держит его
//!   и на ожидании. Порядок: `alive` → swapchain. `vkWaitSemaphores` выполняется строго вне
//!   swapchain-блокировки, чтобы present не ждал до `SLEEP_TIMEOUT`.
//! - Деактивация (`Shared::deactivate`): под swapchain-блокировкой sleep mode выключается,
//!   swapchain обнуляется; затем write-lock `alive` дожидается `sleep` в полёте. После этого
//!   вызовы не доходят до backend. Последнее выданное значение semaphore дожидается тем же
//!   таймаутом, и владелец может дождаться простоя device и уничтожить timeline semaphore.

use ash::vk;
use std::ffi::CStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::Duration;

/// Таймаут ожидания timeline semaphore в `LatencyHandle::sleep`.
pub(crate) const SLEEP_TIMEOUT: Duration = Duration::from_millis(100);

/// Режим NVIDIA Reflex, запрошенный при создании renderer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LowLatencyMode {
    #[default]
    Off,
    On,
    /// Как `On`, плюс запрос буста частоты GPU драйвером.
    Boost,
}

/// Маркеры симуляции, которые ставит игровой цикл; маркеры рендера ставит сам renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatencyMarker {
    SimulationStart,
    SimulationEnd,
    InputSample,
}

impl LatencyMarker {
    fn vk(self) -> vk::LatencyMarkerNV {
        match self {
            Self::SimulationStart => vk::LatencyMarkerNV::SIMULATION_START,
            Self::SimulationEnd => vk::LatencyMarkerNV::SIMULATION_END,
            Self::InputSample => vk::LatencyMarkerNV::INPUT_SAMPLE,
        }
    }
}

/// Клонируемый доступ к Reflex из потоков, отличных от render thread.
///
/// `sleep` блокирует вызывающий поток не дольше `SLEEP_TIMEOUT`. После деактивации renderer
/// оба метода возвращаются сразу, не обращаясь к Vulkan.
#[derive(Clone)]
pub struct LatencyHandle {
    pub(crate) shared: Arc<Shared>,
}

impl LatencyHandle {
    /// Ожидает разрешения Reflex на начало симуляции кадра.
    pub fn sleep(&self) {
        self.shared.sleep();
    }

    /// Ставит маркер симуляции для кадра `frame_id` (`snapshot.sequence`); 0 игнорируется.
    pub fn marker(&self, frame_id: u64, marker: LatencyMarker) {
        self.shared.marker(frame_id, marker.vk());
    }
}

/// Узкий интерфейс над Vulkan-вызовами, чтобы логику тестировать без GPU.
pub(crate) trait LatencyBackend: Send + Sync {
    fn set_sleep_mode(&self, swapchain: vk::SwapchainKHR, mode: LowLatencyMode);
    /// Просит драйвер подать сигнал `value` в timeline semaphore после окончания ожидания.
    fn sleep(&self, swapchain: vk::SwapchainKHR, value: u64);
    fn set_marker(&self, swapchain: vk::SwapchainKHR, present_id: u64, marker: vk::LatencyMarkerNV);
    /// Ждёт, пока semaphore достигнет `value`, но не дольше `timeout`.
    fn wait(&self, value: u64, timeout: Duration);
}

/// Общее состояние между renderer и всеми `LatencyHandle`.
pub(crate) struct Shared {
    alive: RwLock<bool>,
    swapchain: Mutex<vk::SwapchainKHR>,
    semaphore_value: AtomicU64,
    mode: LowLatencyMode,
    backend: Arc<dyn LatencyBackend>,
}

impl Shared {
    pub(crate) fn new(mode: LowLatencyMode, backend: Arc<dyn LatencyBackend>) -> Self {
        Self {
            alive: RwLock::new(true),
            swapchain: Mutex::new(vk::SwapchainKHR::null()),
            semaphore_value: AtomicU64::new(0),
            mode,
            backend,
        }
    }

    /// Берёт swapchain-блокировку. Не вызывать, пока у того же потока уже взят `SwapchainLock`.
    pub(crate) fn lock_swapchain(&self) -> SwapchainLock<'_> {
        SwapchainLock {
            shared: self,
            handle: self
                .swapchain
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    fn sleep(&self) {
        let alive = self.alive.read().unwrap_or_else(PoisonError::into_inner);
        if !*alive {
            return;
        }
        let value = {
            let swapchain = self.lock_swapchain();
            if swapchain.handle() == vk::SwapchainKHR::null() {
                return;
            }
            // Значение берётся под блокировкой: сигналы уходят в драйвер в порядке выдачи.
            let value = self.semaphore_value.fetch_add(1, Ordering::Relaxed) + 1;
            self.backend.sleep(swapchain.handle(), value);
            value
        };
        // Таймаут не откатывает счётчик: следующий sleep получит большее значение.
        self.backend.wait(value, SLEEP_TIMEOUT);
        drop(alive);
    }

    pub(crate) fn marker(&self, frame_id: u64, marker: vk::LatencyMarkerNV) {
        let alive = self.alive.read().unwrap_or_else(PoisonError::into_inner);
        if !*alive {
            return;
        }
        self.lock_swapchain().marker(frame_id, marker);
    }

    /// Выключает Reflex и отделяет `Shared` от Vulkan; повторный вызов безопасен.
    /// Ждёт `sleep` в полёте и последнее выданное значение semaphore (не дольше `SLEEP_TIMEOUT`).
    /// После возврата владелец должен дождаться простоя device, прежде чем уничтожать ресурсы backend.
    pub(crate) fn deactivate(&self) {
        {
            let mut swapchain = self.lock_swapchain();
            if swapchain.handle() != vk::SwapchainKHR::null() {
                self.backend
                    .set_sleep_mode(swapchain.handle(), LowLatencyMode::Off);
            }
            swapchain.replace_without_configuring(vk::SwapchainKHR::null());
        }
        *self.alive.write().unwrap_or_else(PoisonError::into_inner) = false;
        let last = self.semaphore_value.load(Ordering::Relaxed);
        if last != 0 {
            self.backend.wait(last, SLEEP_TIMEOUT);
        }
    }
}

/// Захваченная swapchain-блокировка render thread или `Shared::sleep`/`marker`.
pub(crate) struct SwapchainLock<'a> {
    shared: &'a Shared,
    handle: MutexGuard<'a, vk::SwapchainKHR>,
}

impl SwapchainLock<'_> {
    pub(crate) fn handle(&self) -> vk::SwapchainKHR {
        *self.handle
    }

    /// Устанавливает новый swapchain и сразу применяет к нему режим sleep.
    pub(crate) fn replace(&mut self, swapchain: vk::SwapchainKHR) {
        self.replace_without_configuring(swapchain);
        if swapchain != vk::SwapchainKHR::null() {
            self.shared
                .backend
                .set_sleep_mode(swapchain, self.shared.mode);
        }
    }

    fn replace_without_configuring(&mut self, swapchain: vk::SwapchainKHR) {
        *self.handle = swapchain;
    }

    /// Ставит маркер для `frame_id`; 0 и отсутствующий swapchain игнорируются.
    pub(crate) fn marker(&self, frame_id: u64, marker: vk::LatencyMarkerNV) {
        if frame_id == 0 || *self.handle == vk::SwapchainKHR::null() {
            return;
        }
        self.shared
            .backend
            .set_marker(*self.handle, frame_id, marker);
    }
}

/// Выбирает present id для кадра. Id не расходуется, пока кадр не отправлен.
#[derive(Debug, Default)]
pub(crate) struct PresentIdTracker {
    /// Последний id, для которого уже поставлен START (даже если кадр не отправлен).
    started: u64,
    /// Последний id, отправленный через успешный `queue_submit2`.
    submitted: u64,
}

impl PresentIdTracker {
    /// Возвращает id, если для него ещё не ставился START и он больше отправленного.
    pub(crate) fn begin(&mut self, id: u64) -> Option<u64> {
        if id == 0 || id <= self.started || id <= self.submitted {
            return None;
        }
        self.started = id;
        Some(id)
    }

    pub(crate) fn submitted(&mut self, id: u64) {
        self.submitted = self.submitted.max(id);
    }
}

/// Расширения устройства, без которых Reflex не работает.
pub(crate) fn low_latency_device_extensions() -> [&'static CStr; 2] {
    [vk::NV_LOW_LATENCY2_NAME, vk::KHR_PRESENT_ID_NAME]
}

/// Параметры sleep mode для `vkSetLatencySleepModeNV`.
pub(crate) fn sleep_mode_info(mode: LowLatencyMode) -> vk::LatencySleepModeInfoNV<'static> {
    vk::LatencySleepModeInfoNV::default()
        .low_latency_mode(mode != LowLatencyMode::Off)
        .low_latency_boost(mode == LowLatencyMode::Boost)
        .minimum_interval_us(0)
}

/// Реализация `LatencyBackend` поверх ash. Семафор принадлежит renderer и уничтожается им.
pub(crate) struct VulkanLatency {
    reflex: ash::nv::low_latency2::Device,
    device: ash::Device,
    semaphore: vk::Semaphore,
}

impl VulkanLatency {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        semaphore: vk::Semaphore,
    ) -> Self {
        Self {
            reflex: ash::nv::low_latency2::Device::new(instance, device),
            device: device.clone(),
            semaphore,
        }
    }
}

impl LatencyBackend for VulkanLatency {
    fn set_sleep_mode(&self, swapchain: vk::SwapchainKHR, mode: LowLatencyMode) {
        let info = sleep_mode_info(mode);
        // SAFETY: вызывающий держит swapchain-блокировку; swapchain принадлежит этому device.
        if let Err(error) = unsafe { self.reflex.set_latency_sleep_mode(swapchain, Some(&info)) } {
            log::warn!("vkSetLatencySleepModeNV не выполнен: {error:?}");
        }
    }

    fn sleep(&self, swapchain: vk::SwapchainKHR, value: u64) {
        let info = vk::LatencySleepInfoNV::default()
            .signal_semaphore(self.semaphore)
            .value(value);
        // SAFETY: вызывающий держит swapchain-блокировку; semaphore живёт до деактивации.
        if let Err(error) = unsafe { self.reflex.latency_sleep(swapchain, &info) } {
            log::warn!("vkLatencySleepNV не выполнен: {error:?}");
        }
    }

    fn set_marker(
        &self,
        swapchain: vk::SwapchainKHR,
        present_id: u64,
        marker: vk::LatencyMarkerNV,
    ) {
        let info = vk::SetLatencyMarkerInfoNV::default()
            .present_id(present_id)
            .marker(marker);
        // SAFETY: вызывающий держит swapchain-блокировку; swapchain принадлежит этому device.
        unsafe { self.reflex.set_latency_marker(swapchain, &info) };
    }

    fn wait(&self, value: u64, timeout: Duration) {
        let semaphores = [self.semaphore];
        let values = [value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        let timeout_ns = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        // SAFETY: semaphore создан этим device и уничтожается владельцем только после деактивации,
        // когда ожидания больше не начинаются. Таймаут штатен: сигнал придёт позже, а счётчик
        // уже монотонен. Ошибку устройства увидит следующий вызов Vulkan.
        let _ = unsafe { self.device.wait_semaphores(&info, timeout_ns) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LatencyBackend, LowLatencyMode, PresentIdTracker, SLEEP_TIMEOUT, Shared,
        low_latency_device_extensions, sleep_mode_info,
    };
    use crate::RendererConfig;
    use ash::vk;
    use ash::vk::Handle as _;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    #[derive(Debug, PartialEq)]
    enum Call {
        SetSleepMode(LowLatencyMode),
        Sleep(u64),
        Marker(u64),
    }

    /// Блокирует первый `wait` до закрытия `release`; последующие `wait` не блокируются.
    struct Gate {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Mutex<Vec<Call>>,
        waits: Mutex<Vec<(u64, Duration)>>,
        gate: Mutex<Option<Gate>>,
    }

    impl FakeBackend {
        fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }

        fn take_waits(&self) -> Vec<(u64, Duration)> {
            std::mem::take(&mut *self.waits.lock().unwrap())
        }
    }

    impl LatencyBackend for FakeBackend {
        fn set_sleep_mode(&self, _swapchain: vk::SwapchainKHR, mode: LowLatencyMode) {
            self.calls.lock().unwrap().push(Call::SetSleepMode(mode));
        }

        fn sleep(&self, _swapchain: vk::SwapchainKHR, value: u64) {
            self.calls.lock().unwrap().push(Call::Sleep(value));
        }

        fn set_marker(
            &self,
            _swapchain: vk::SwapchainKHR,
            present_id: u64,
            _marker: vk::LatencyMarkerNV,
        ) {
            self.calls.lock().unwrap().push(Call::Marker(present_id));
        }

        fn wait(&self, value: u64, timeout: Duration) {
            self.waits.lock().unwrap().push((value, timeout));
            let gate = self.gate.lock().unwrap().take();
            if let Some(gate) = gate {
                let _ = gate.entered.send(());
                let _ = gate.release.recv();
                gate.log.lock().unwrap().push("wait released");
            }
        }
    }

    fn swapchain(raw: u64) -> vk::SwapchainKHR {
        vk::SwapchainKHR::from_raw(raw)
    }

    #[test]
    fn present_id_tracker_hands_out_each_id_once_and_skips_zero() {
        let mut tracker = PresentIdTracker::default();
        assert_eq!(tracker.begin(0), None);
        assert_eq!(tracker.begin(3), Some(3));
        assert_eq!(
            tracker.begin(3),
            None,
            "START для одного id ставится один раз"
        );
        tracker.submitted(3);
        assert_eq!(
            tracker.begin(3),
            None,
            "повтор уже отправленного снимка без id"
        );
        assert_eq!(tracker.begin(2), None, "меньший id отклоняется");
        assert_eq!(tracker.begin(4), Some(4));
    }

    #[test]
    fn present_id_tracker_failed_frame_does_not_consume_submission_order() {
        let mut tracker = PresentIdTracker::default();
        assert_eq!(tracker.begin(5), Some(5));
        // acquire вернул timeout/OUT_OF_DATE: submitted не вызывается, id 5 не отправлен.
        assert_eq!(tracker.begin(5), None);
        tracker.submitted(4);
        assert_eq!(
            tracker.begin(6),
            Some(6),
            "неотправленный id не блокирует следующий кадр"
        );
    }

    #[test]
    fn sleep_mode_info_maps_modes_to_flags() {
        let off = sleep_mode_info(LowLatencyMode::Off);
        assert_eq!(off.low_latency_mode, vk::FALSE);
        assert_eq!(off.low_latency_boost, vk::FALSE);

        let on = sleep_mode_info(LowLatencyMode::On);
        assert_eq!(on.low_latency_mode, vk::TRUE);
        assert_eq!(on.low_latency_boost, vk::FALSE);

        let boost = sleep_mode_info(LowLatencyMode::Boost);
        assert_eq!(boost.low_latency_mode, vk::TRUE);
        assert_eq!(boost.low_latency_boost, vk::TRUE);
        assert_eq!(boost.minimum_interval_us, 0);
    }

    #[test]
    fn device_extensions_request_low_latency_and_present_id() {
        let names = low_latency_device_extensions();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&vk::NV_LOW_LATENCY2_NAME));
        assert!(names.contains(&vk::KHR_PRESENT_ID_NAME));
    }

    #[test]
    fn shared_stops_calling_backend_after_deactivation() {
        let fake = Arc::new(FakeBackend::default());
        let shared = Shared::new(LowLatencyMode::On, fake.clone());
        shared.lock_swapchain().replace(swapchain(7));
        assert_eq!(
            fake.take_calls(),
            vec![Call::SetSleepMode(LowLatencyMode::On)]
        );

        shared.deactivate();
        assert_eq!(
            fake.take_calls(),
            vec![Call::SetSleepMode(LowLatencyMode::Off)]
        );

        shared.sleep();
        shared.marker(10, vk::LatencyMarkerNV::RENDERSUBMIT_START);
        assert!(
            fake.take_calls().is_empty(),
            "после деактивации backend не вызывается"
        );
        assert!(fake.take_waits().is_empty());
    }

    #[test]
    fn deactivate_returns_only_after_sleep_in_flight_is_released() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let log = Arc::new(Mutex::new(Vec::new()));
        let fake = Arc::new(FakeBackend {
            gate: Mutex::new(Some(Gate {
                entered: entered_tx,
                release: release_rx,
                log: Arc::clone(&log),
            })),
            ..FakeBackend::default()
        });
        let shared = Shared::new(LowLatencyMode::On, fake.clone());
        shared.lock_swapchain().replace(swapchain(7));
        fake.take_calls();

        thread::scope(|scope| {
            let sleeper = scope.spawn(|| shared.sleep());
            // Страховка от зависания при регрессии: в норме sleep доходит до wait сразу.
            entered_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("sleep должен дойти до backend.wait");
            let deactivator = scope.spawn(|| {
                shared.deactivate();
                log.lock().unwrap().push("deactivate returned");
            });
            drop(release_tx);
            sleeper.join().unwrap();
            deactivator.join().unwrap();
        });

        assert_eq!(
            *log.lock().unwrap(),
            vec!["wait released", "deactivate returned"],
            "deactivate ждёт sleep в полёте, а не выполняется параллельно"
        );
        assert_eq!(
            fake.take_calls(),
            vec![Call::Sleep(1), Call::SetSleepMode(LowLatencyMode::Off)]
        );
        assert_eq!(
            fake.take_waits(),
            vec![(1, SLEEP_TIMEOUT), (1, SLEEP_TIMEOUT)],
            "деактивация дожидается последнего выданного значения"
        );

        shared.sleep();
        shared.marker(10, vk::LatencyMarkerNV::RENDERSUBMIT_START);
        assert!(fake.take_calls().is_empty());
        assert!(fake.take_waits().is_empty());
    }

    #[test]
    fn sleep_without_swapchain_does_not_signal_or_wait() {
        let fake = Arc::new(FakeBackend::default());
        let shared = Shared::new(LowLatencyMode::On, fake.clone());
        shared.sleep();
        assert!(fake.take_calls().is_empty());
        assert!(fake.take_waits().is_empty());
    }

    #[test]
    fn semaphore_value_stays_monotonic_across_waits() {
        let fake = Arc::new(FakeBackend::default());
        let shared = Shared::new(LowLatencyMode::On, fake.clone());
        shared.lock_swapchain().replace(swapchain(7));
        fake.take_calls();
        shared.sleep();
        shared.sleep();
        shared.sleep();
        assert_eq!(
            fake.take_waits(),
            vec![(1, SLEEP_TIMEOUT), (2, SLEEP_TIMEOUT), (3, SLEEP_TIMEOUT)]
        );
        assert_eq!(
            fake.take_calls(),
            vec![Call::Sleep(1), Call::Sleep(2), Call::Sleep(3)]
        );
    }

    #[test]
    fn renderer_config_defaults_to_reflex_off() {
        assert_eq!(RendererConfig::default().low_latency, LowLatencyMode::Off);
    }

    #[test]
    fn marker_with_zero_frame_id_is_ignored() {
        let fake = Arc::new(FakeBackend::default());
        let shared = Shared::new(LowLatencyMode::On, fake.clone());
        shared.lock_swapchain().replace(swapchain(7));
        fake.take_calls();
        shared.marker(0, vk::LatencyMarkerNV::SIMULATION_START);
        shared.marker(4, vk::LatencyMarkerNV::SIMULATION_START);
        assert_eq!(fake.take_calls(), vec![Call::Marker(4)]);
    }
}
