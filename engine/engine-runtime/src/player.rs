use crate::activation::PendingActivation;
use crate::module_host::ModuleHost;
use crate::pipeline::{
    ExitRequester, FrameSnapshot, RENDER_THREAD, RenderControl, RenderThreadConfig, RuntimeStatus,
    SCRIPTS_THREAD, ScriptControl, VulkanBackend, start_render,
};
use crate::scene::{PreparedRenderAssets, upload_scene};
use engine_render::{Renderer, RendererConfig};
use engine_script::ScriptInterruptHandle;
use engine_window::{Window, WindowConfig, WindowHandler, WindowProxy, run};
use flume::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

struct WindowExit(WindowProxy);

impl ExitRequester for WindowExit {
    fn request_exit(&self) {
        self.0.request_exit();
    }
}

pub(crate) struct PlayerConfig {
    pub window: WindowConfig,
    pub frame_limit: Option<u64>,
    pub render_assets: PreparedRenderAssets,
    pub activation: PendingActivation,
    pub script_tx: Sender<ScriptControl>,
    pub render_tx: Sender<RenderControl>,
    pub render_rx: Receiver<RenderControl>,
    pub frame_rx: Receiver<Arc<FrameSnapshot>>,
    pub script_stopped_rx: std::sync::mpsc::Receiver<()>,
    pub interrupt: ScriptInterruptHandle,
    pub status: Arc<RuntimeStatus>,
}

pub(crate) fn run_window(config: PlayerConfig, modules: &mut ModuleHost) -> u8 {
    let PlayerConfig {
        window,
        frame_limit,
        render_assets,
        activation,
        script_tx,
        render_tx,
        render_rx,
        frame_rx,
        script_stopped_rx,
        interrupt,
        status,
    } = config;
    let mut handler = PlayerWindow {
        frame_limit,
        render_assets,
        modules,
        activation,
        script_tx,
        render_tx,
        render_rx,
        frame_rx: Some(frame_rx),
        script_stopped_rx,
        interrupt,
        status: Arc::clone(&status),
        render_stopped_rx: None,
        window_exit: None,
        render_started: false,
        cleaned: false,
    };
    let run_result = run(window, &mut handler);
    handler.cleanup();
    if let Err(error) = run_result {
        status.record_failure(format!("ошибка оконного цикла: {error}"));
    }
    if let Some(message) = status.failure_message() {
        eprintln!("Ошибка runtime: {message}");
    }
    status.exit_code()
}

struct PlayerWindow<'a> {
    frame_limit: Option<u64>,
    render_assets: PreparedRenderAssets,
    modules: &'a mut ModuleHost,
    activation: PendingActivation,
    script_tx: Sender<ScriptControl>,
    render_tx: Sender<RenderControl>,
    render_rx: Receiver<RenderControl>,
    frame_rx: Option<Receiver<Arc<FrameSnapshot>>>,
    script_stopped_rx: std::sync::mpsc::Receiver<()>,
    interrupt: ScriptInterruptHandle,
    status: Arc<RuntimeStatus>,
    render_stopped_rx: Option<Receiver<()>>,
    window_exit: Option<Arc<dyn ExitRequester>>,
    render_started: bool,
    cleaned: bool,
}

impl PlayerWindow<'_> {
    fn fail_init(&mut self, message: impl Into<String>) -> bool {
        self.status.record_failure(message);
        self.interrupt.interrupt();
        let _ = self.script_tx.send(ScriptControl::Shutdown);
        let _ = self.render_tx.send(RenderControl::Shutdown);
        false
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        let _ = self.script_tx.send(ScriptControl::Shutdown);

        if self.render_started && !self.status.render_stopped() {
            let _ = self.render_tx.send(RenderControl::Shutdown);
            if let Some(receiver) = &self.render_stopped_rx {
                wait_for_render_or_detach(
                    self.modules,
                    &self.status,
                    receiver,
                    Duration::from_secs(2),
                );
            }
        }

        cleanup_threads(
            self.modules,
            self.status.render_stopped(),
            &self.script_stopped_rx,
            &self.interrupt,
            &self.status,
            Duration::from_secs(2),
        );
    }
}

trait RuntimeThreads {
    fn join_named(&mut self, name: &str) -> Result<(), String>;
    fn detach(&mut self, name: &str) -> bool;
}

impl RuntimeThreads for ModuleHost {
    fn join_named(&mut self, name: &str) -> Result<(), String> {
        ModuleHost::join_named(self, name)
    }

    fn detach(&mut self, name: &str) -> bool {
        ModuleHost::detach(self, name)
    }
}

fn cleanup_threads(
    modules: &mut impl RuntimeThreads,
    render_stopped: bool,
    stopped: &std::sync::mpsc::Receiver<()>,
    interrupt: &ScriptInterruptHandle,
    status: &RuntimeStatus,
    timeout: Duration,
) {
    if render_stopped && let Err(error) = modules.join_named(RENDER_THREAD) {
        status.record_failure(error);
    }
    if wait_for_scripts_or_detach(modules, stopped, interrupt, status, timeout)
        && let Err(error) = modules.join_named(SCRIPTS_THREAD)
    {
        status.record_failure(error);
    }
}

fn wait_for_scripts_or_detach(
    modules: &mut impl RuntimeThreads,
    stopped: &std::sync::mpsc::Receiver<()>,
    interrupt: &ScriptInterruptHandle,
    status: &RuntimeStatus,
    timeout: Duration,
) -> bool {
    match stopped.recv_timeout(timeout) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!("поток скриптов не остановился за {timeout:?}; прерываю Lua");
            status.record_failure(format!("поток скриптов не остановился за {timeout:?}"));
            interrupt.interrupt();
            match stopped.recv_timeout(timeout) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    eprintln!(
                        "поток скриптов не завершился после прерывания за {timeout:?}; отсоединяю его"
                    );
                    status.record_failure(format!(
                        "поток скриптов не завершился после прерывания за {timeout:?}"
                    ));
                    modules.detach(SCRIPTS_THREAD);
                    false
                }
            }
        }
    }
}

fn wait_for_render_or_detach(
    modules: &mut impl RuntimeThreads,
    status: &RuntimeStatus,
    stopped: &Receiver<()>,
    timeout: Duration,
) -> bool {
    if stopped.recv_timeout(timeout).is_ok() || status.render_stopped() {
        return true;
    }
    eprintln!("поток рендера не остановился за 2 секунды; отсоединяю его");
    status.record_failure("поток рендера не остановился за 2 секунды".to_owned());
    modules.detach(RENDER_THREAD);
    false
}

impl WindowHandler for PlayerWindow<'_> {
    fn attach_proxy(&mut self, proxy: WindowProxy) {
        self.activation.attach(proxy.clone());
        self.window_exit = Some(Arc::new(WindowExit(proxy)));
    }

    fn init(&mut self, window: Arc<Window>) -> bool {
        let size = window.inner_size();
        let mut renderer = match Renderer::new(
            Arc::clone(&window),
            size.width,
            size.height,
            RendererConfig::default(),
        ) {
            Ok(renderer) => renderer,
            Err(error) => return self.fail_init(format!("ошибка renderer: {error}")),
        };
        let gpu_scene = match upload_scene(&mut renderer, &self.render_assets) {
            Ok(scene) => scene,
            Err(error) => return self.fail_init(format!("ошибка загрузки GPU-ресурсов: {error}")),
        };
        let Some(frame_rx) = self.frame_rx.take() else {
            return self.fail_init("канал кадров отсутствует при запуске рендера");
        };
        let Some(window_exit) = self.window_exit.clone() else {
            return self.fail_init("WindowProxy не прикреплён перед init");
        };
        let (render_stopped_tx, render_stopped_rx) = flume::bounded(1);
        self.render_stopped_rx = Some(render_stopped_rx);
        if let Err(error) = start_render(
            self.modules,
            RenderThreadConfig {
                backend: Box::new(VulkanBackend::new(renderer)),
                width: size.width,
                height: size.height,
                frame_limit: self.frame_limit,
                frame_rx,
                control_tx: self.render_tx.clone(),
                control_rx: self.render_rx.clone(),
                script_tx: self.script_tx.clone(),
                interrupt: self.interrupt.clone(),
                status: Arc::clone(&self.status),
                stopped_tx: render_stopped_tx,
                exit: window_exit,
            },
        ) {
            return self.fail_init(format!("не удалось запустить поток рендера: {error}"));
        }
        self.render_started = true;
        if self.script_tx.send(ScriptControl::Go(gpu_scene)).is_err() {
            self.status
                .record_failure("поток скриптов завершился до запуска сцены");
            let _ = self.render_tx.send(RenderControl::Shutdown);
            return false;
        }
        true
    }

    fn frame(&mut self, _window: &Window) -> bool {
        true
    }

    fn resized(&mut self, width: u32, height: u32) {
        let _ = self.render_tx.send(RenderControl::Resize(width, height));
    }

    fn close_requested(&mut self) -> bool {
        let _ = self.script_tx.send(ScriptControl::Shutdown);
        let _ = self.render_tx.send(RenderControl::Shutdown);
        false
    }

    fn occluded(&mut self, occluded: bool) {
        let _ = self.render_tx.send(RenderControl::Occluded(occluded));
    }

    fn exiting(&mut self) {
        self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RuntimeThreads, cleanup_threads, wait_for_render_or_detach, wait_for_scripts_or_detach,
    };
    use crate::module_host::ModuleHost;
    use crate::pipeline::RuntimeStatus;
    use engine_script::ScriptInterruptHandle;
    use flume::bounded;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeThreads(Vec<String>);

    impl RuntimeThreads for FakeThreads {
        fn join_named(&mut self, name: &str) -> Result<(), String> {
            self.0.push(format!("join:{name}"));
            Ok(())
        }

        fn detach(&mut self, name: &str) -> bool {
            self.0.push(format!("detach:{name}"));
            true
        }
    }

    #[test]
    fn emergency_cleanup_detaches_a_hung_render_thread_without_joining_it() {
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mut modules = ModuleHost::new();
        modules
            .spawn(
                "rve-render",
                || {},
                move || {
                    let _ = release_rx.recv();
                    let _ = finished_tx.send(());
                },
            )
            .unwrap();
        let (stopped_tx, stopped_rx) = bounded(1);
        let status = Arc::new(RuntimeStatus::default());

        assert!(!wait_for_render_or_detach(
            &mut modules,
            &status,
            &stopped_rx,
            Duration::ZERO,
        ));
        assert_eq!(status.exit_code(), 1);
        assert!(status.failure_message().unwrap().contains("не остановился"));
        assert!(modules.shutdown().is_empty());

        release_tx.send(()).unwrap();
        finished_rx.recv().unwrap();
        drop(stopped_tx);
    }

    #[test]
    fn hung_scripts_are_interrupted_then_detached_without_joining() {
        let (release_tx, release_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mut modules = ModuleHost::new();
        modules
            .spawn(
                "rve-scripts",
                || {},
                move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    let _ = finished_tx.send(());
                },
            )
            .unwrap();
        started_rx.recv().unwrap();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let interrupt = ScriptInterruptHandle::new();
        let status = RuntimeStatus::default();

        assert!(!wait_for_scripts_or_detach(
            &mut modules,
            &stopped_rx,
            &interrupt,
            &status,
            Duration::ZERO,
        ));
        assert!(interrupt.is_interrupted());
        assert_eq!(status.exit_code(), 1);
        assert!(status.failure_message().unwrap().contains("скриптов"));
        assert!(modules.shutdown().is_empty());
        drop(stopped_tx);
        release_tx.send(()).unwrap();
        finished_rx.recv().unwrap();
    }

    #[test]
    fn cleanup_joins_render_only_after_render_stopped_then_joins_scripts() {
        for (render_stopped, expected) in [
            (true, vec!["join:rve-render", "join:rve-scripts"]),
            (false, vec!["join:rve-scripts"]),
        ] {
            let mut modules = FakeThreads::default();
            let (stopped_tx, stopped_rx) = mpsc::channel();
            stopped_tx.send(()).unwrap();
            let status = RuntimeStatus::default();

            cleanup_threads(
                &mut modules,
                render_stopped,
                &stopped_rx,
                &ScriptInterruptHandle::new(),
                &status,
                Duration::ZERO,
            );

            assert_eq!(modules.0, expected);
            assert_eq!(status.exit_code(), 0);
        }
    }
}
