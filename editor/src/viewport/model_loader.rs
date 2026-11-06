//! Фоновая загрузка FBX: разбор моделей идёт в отдельном потоке, а загрузку в GPU
//! выполняет главный поток, когда модель готова.

use engine_assets::{Assets, Model};
use engine_files::VirtualPath;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

type LoadFn = Arc<dyn Fn(&str) -> Result<LoadedModel, String> + Send + Sync>;
type LoadResult = (u64, String, Result<LoadedModel, String>);

#[derive(Debug)]
pub struct LoadedModel {
    pub model: Model,
    pub dependencies: Vec<VirtualPath>,
    pub dependency_warning: Option<String>,
}

/// Очередь загрузки моделей. Поколение растёт только при смене проекта.
#[derive(Default)]
pub struct ModelLoader {
    load: Option<LoadFn>,
    worker: Option<Worker>,
    generation: u64,
    pending: HashSet<String>,
    stale: HashSet<String>,
}

struct Worker {
    requests: Sender<(u64, String)>,
    results: Receiver<LoadResult>,
}

impl Worker {
    fn start(load: LoadFn) -> std::io::Result<Self> {
        let (requests, request_rx) = channel::<(u64, String)>();
        let (result_tx, results) = channel();
        // Поток завершается, когда очередь удаляет отправитель запросов.
        std::thread::Builder::new()
            .name("rve-editor-models".to_owned())
            .spawn(move || {
                for (generation, path) in request_rx {
                    // Паника разбора не должна оставлять модель в очереди навсегда.
                    let model =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(&path)))
                            .unwrap_or_else(|_| Err("паника при разборе модели".to_owned()));
                    if result_tx.send((generation, path, model)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, results })
    }
}

impl ModelLoader {
    /// Загружает модели из ресурсов проекта; прежняя очередь сбрасывается.
    pub fn restart(&mut self, assets: Assets) {
        self.restart_with(Arc::new(move |path: &str| {
            let model = assets.load_model(path).map_err(|error| error.to_string())?;
            let (dependencies, dependency_warning) = match assets
                .model_dependencies(path)
                .map_err(|error| error.to_string())
            {
                Ok(paths) => (
                    paths
                        .into_iter()
                        .filter_map(|path| VirtualPath::parse(&path).ok())
                        .collect(),
                    None,
                ),
                Err(error) => (Vec::new(), Some(error)),
            };
            Ok(LoadedModel {
                model,
                dependencies,
                dependency_warning,
            })
        }));
    }

    fn restart_with(&mut self, load: LoadFn) {
        self.load = Some(load);
        self.worker = None;
        self.generation += 1;
        self.pending.clear();
        self.stale.clear();
    }

    /// Ставит модель в очередь, если она ещё не загружается. `false` — загрузка невозможна:
    /// нет проекта или поток не запустился.
    pub fn request(&mut self, path: &str) -> bool {
        if self.pending.contains(path) {
            return true;
        }
        let Some(load) = &self.load else {
            return false;
        };
        if self.worker.is_none() {
            self.worker = Worker::start(Arc::clone(load)).ok();
        }
        let sent = self.worker.as_ref().is_some_and(|worker| {
            worker
                .requests
                .send((self.generation, path.to_owned()))
                .is_ok()
        });
        if sent {
            self.pending.insert(path.to_owned());
        }
        sent
    }

    pub fn is_pending(&self, path: &str) -> bool {
        self.pending.contains(path)
    }

    /// Перезагружает модель; незавершённый результат помечается устаревшим.
    pub fn reload(&mut self, path: &str) -> bool {
        if self.pending.contains(path) {
            self.stale.insert(path.to_owned());
            true
        } else {
            self.request(path)
        }
    }

    /// Помечает все незавершённые разборы устаревшими после изменения PNG или каталога.
    pub fn stale_pending(&mut self) {
        self.stale.extend(self.pending.iter().cloned());
    }

    pub fn pending_paths(&self) -> impl Iterator<Item = &str> {
        self.pending.iter().map(String::as_str)
    }

    /// Есть незавершённые загрузки: окно должно проснуться, чтобы показать результат.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Готовые модели текущего поколения; не блокирует.
    pub fn finished(&mut self) -> Vec<(String, Result<LoadedModel, String>)> {
        let results = self
            .worker
            .as_ref()
            .map(|worker| worker.results.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        self.accept(results)
    }

    fn accept(&mut self, results: Vec<LoadResult>) -> Vec<(String, Result<LoadedModel, String>)> {
        let mut accepted = Vec::new();
        for (generation, path, result) in results {
            if generation != self.generation || !self.pending.remove(&path) {
                continue;
            }
            if self.stale.remove(&path) {
                self.request(&path);
                continue;
            }
            accepted.push((path, result));
        }
        accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    impl ModelLoader {
        /// Ждёт хотя бы один результат потока; без `sleep`, блокирующим приёмом.
        fn wait_finished(&mut self) -> Vec<(String, Result<LoadedModel, String>)> {
            let worker = self.worker.as_ref().unwrap();
            let first = worker.results.recv().unwrap();
            let mut results = vec![first];
            results.extend(worker.results.try_iter());
            self.accept(results)
        }
    }

    fn empty_model() -> Model {
        Model {
            meshes: Vec::new(),
            textures: Vec::new(),
        }
    }

    fn loaded_model(dependencies: &[&str]) -> LoadedModel {
        LoadedModel {
            model: empty_model(),
            dependencies: dependencies
                .iter()
                .map(|path| VirtualPath::parse(path).unwrap())
                .collect(),
            dependency_warning: None,
        }
    }

    #[test]
    fn repeated_requests_load_once_and_results_clear_the_queue() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mut loader = ModelLoader::default();
        assert!(!loader.request("a.fbx"));
        loader.restart_with(Arc::new(move |path: &str| {
            counted.fetch_add(1, Ordering::SeqCst);
            if path == "bad.fbx" {
                Err("broken".to_owned())
            } else {
                Ok(loaded_model(&[]))
            }
        }));
        assert!(loader.request("a.fbx"));
        assert!(loader.request("a.fbx"));
        assert!(loader.is_pending("a.fbx"));
        let mut done = loader.wait_finished();
        assert!(loader.request("bad.fbx"));
        while loader.has_pending() {
            done.extend(loader.wait_finished());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(done.len(), 2);
        assert!(done[0].1.is_ok());
        assert_eq!(done[1].0, "bad.fbx");
        assert_eq!(done[1].1.as_ref().unwrap_err(), "broken");
    }

    #[test]
    fn a_panicking_parser_reports_an_error_and_the_queue_keeps_working() {
        let mut loader = ModelLoader::default();
        loader.restart_with(Arc::new(|path: &str| {
            if path == "crash.fbx" {
                panic!("broken parser");
            }
            Ok(loaded_model(&[]))
        }));
        assert!(loader.request("crash.fbx"));
        let done = loader.wait_finished();
        assert_eq!(done[0].0, "crash.fbx");
        assert!(done[0].1.as_ref().unwrap_err().contains("паника"));
        assert!(!loader.has_pending());
        assert!(loader.request("ok.fbx"));
        assert!(loader.wait_finished()[0].1.is_ok());
    }

    #[test]
    fn demo_project_models_load_in_the_background() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = crate::project::OpenProject::open(std::path::Path::new(dir)).unwrap();
        let mut loader = ModelLoader::default();
        loader.restart(project.assets.clone());
        assert!(loader.request("models/draft-cube.fbx"));
        assert!(loader.request("models/missing.fbx"));
        let mut done = Vec::new();
        while loader.has_pending() {
            done.extend(loader.wait_finished());
        }
        done.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(done[0].1.as_ref().unwrap().model.bounds().is_some());
        assert!(done[1].1.is_err());
    }

    #[test]
    fn loaded_demo_model_returns_its_external_png_dependencies() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let project = crate::project::OpenProject::open(std::path::Path::new(dir)).unwrap();
        let mut loader = ModelLoader::default();
        loader.restart(project.assets.clone());
        assert!(loader.request("models/props-cube.fbx"));
        let loaded = loader.wait_finished().remove(0).1.unwrap();
        assert_eq!(
            loaded.dependencies,
            [VirtualPath::parse("textures/props.png").unwrap()]
        );
    }

    #[test]
    fn results_started_before_a_restart_are_discarded() {
        // Загрузка первой модели ждёт разрешения теста, поэтому завершается уже после сброса.
        let (release, gate) = channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let mut loader = ModelLoader::default();
        let old_gate = Arc::clone(&gate);
        loader.restart_with(Arc::new(move |_: &str| {
            old_gate.lock().unwrap().recv().unwrap();
            Ok(loaded_model(&[]))
        }));
        assert!(loader.request("old.fbx"));
        // Старый поток остаётся у теста, чтобы дождаться его опоздавшего результата.
        let stale = loader.worker.take().unwrap();
        loader.restart_with(Arc::new(|_: &str| Ok(loaded_model(&[]))));
        assert!(!loader.has_pending());
        release.send(()).unwrap();
        // Результат старого поколения не принимается, даже если пришёл.
        let late = stale.results.recv().unwrap();
        assert_eq!(late.1, "old.fbx");
        assert!(loader.accept(vec![late]).is_empty());

        assert!(loader.request("new.fbx"));
        let done = loader.wait_finished();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, "new.fbx");
        assert!(!loader.has_pending());
    }

    #[test]
    fn reload_during_pending_discards_the_old_result_and_repeats() {
        let (release, gate) = channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut loader = ModelLoader::default();
        let worker_gate = Arc::clone(&gate);
        let worker_calls = Arc::clone(&calls);
        loader.restart_with(Arc::new(move |_: &str| {
            if worker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                worker_gate.lock().unwrap().recv().unwrap();
                Ok(loaded_model(&["old.png"]))
            } else {
                Ok(loaded_model(&["new.png"]))
            }
        }));
        assert!(loader.request("ship.fbx"));
        assert!(loader.reload("ship.fbx"));
        release.send(()).unwrap();
        let mut done = Vec::new();
        while loader.has_pending() {
            done.extend(loader.wait_finished());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(done.len(), 1);
        assert_eq!(
            done[0].1.as_ref().unwrap().dependencies[0].as_str(),
            "new.png"
        );
    }

    #[test]
    fn changed_png_marks_initial_pending_model_stale_and_returns_dependencies() {
        let (release, gate) = channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut loader = ModelLoader::default();
        let worker_gate = Arc::clone(&gate);
        let worker_calls = Arc::clone(&calls);
        loader.restart_with(Arc::new(move |_: &str| {
            if worker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                worker_gate.lock().unwrap().recv().unwrap();
            }
            Ok(loaded_model(&["textures/ship.png"]))
        }));
        assert!(loader.request("ship.fbx"));
        loader.stale_pending();
        release.send(()).unwrap();
        let mut done = Vec::new();
        while loader.has_pending() {
            done.extend(loader.wait_finished());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(done.len(), 1);
        assert_eq!(
            done[0].1.as_ref().unwrap().dependencies[0].as_str(),
            "textures/ship.png"
        );
    }
}
