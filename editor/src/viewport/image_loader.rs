//! Фоновое декодирование PNG-текстур и HDR-окружений сцены: файл читается и декодируется в
//! отдельном потоке, а GPU-ресурс создаёт главный поток, когда изображение готово. Так большой
//! PNG или HDR не задерживает кадр интерфейса.

use engine_assets::{Assets, HdrImageData, TextureData};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ImageKind {
    Texture,
    Hdr,
}

#[derive(Debug)]
pub enum DecodedImage {
    Texture(TextureData),
    Hdr(HdrImageData),
}

type Request = (u64, ImageKind, String);
type LoadFn = Arc<dyn Fn(ImageKind, &str) -> Result<DecodedImage, String> + Send + Sync>;
type LoadResult = (u64, ImageKind, String, Result<DecodedImage, String>);

/// Очередь декодирования. Поколение растёт при смене проекта: результаты прежнего проекта
/// отбрасываются. Повторный запрос пути, который уже декодируется, не ставится в очередь;
/// перезагрузка такого пути помечает текущий результат устаревшим и запрашивает файл заново.
#[derive(Default)]
pub struct ImageLoader {
    load: Option<LoadFn>,
    worker: Option<Worker>,
    generation: u64,
    pending: HashSet<(ImageKind, String)>,
    stale: HashSet<(ImageKind, String)>,
}

struct Worker {
    requests: Sender<Request>,
    results: Receiver<LoadResult>,
}

impl Worker {
    fn start(load: LoadFn) -> std::io::Result<Self> {
        let (requests, request_rx) = channel::<Request>();
        let (result_tx, results) = channel();
        // Поток завершается, когда очередь удаляет отправитель запросов.
        std::thread::Builder::new()
            .name("rve-editor-images".to_owned())
            .spawn(move || {
                for (generation, kind, path) in request_rx {
                    let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        load(kind, &path)
                    }))
                    .unwrap_or_else(|_| Err("паника при декодировании изображения".to_owned()));
                    if result_tx.send((generation, kind, path, image)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, results })
    }
}

impl ImageLoader {
    /// Декодирует изображения из ресурсов проекта; прежняя очередь сбрасывается.
    pub fn restart(&mut self, assets: Assets) {
        self.restart_with(Arc::new(move |kind, path: &str| match kind {
            ImageKind::Texture => assets
                .load_texture(path)
                .map(DecodedImage::Texture)
                .map_err(|error| error.to_string()),
            ImageKind::Hdr => assets
                .load_hdr(path)
                .map(DecodedImage::Hdr)
                .map_err(|error| error.to_string()),
        }));
    }

    fn restart_with(&mut self, load: LoadFn) {
        self.load = Some(load);
        self.worker = None;
        self.generation += 1;
        self.pending.clear();
        self.stale.clear();
    }

    /// Ставит изображение в очередь, если оно ещё не декодируется. `false` — декодирование
    /// невозможно: нет проекта или поток не запустился.
    pub fn request(&mut self, kind: ImageKind, path: &str) -> bool {
        let key = (kind, path.to_owned());
        if self.pending.contains(&key) {
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
                .send((self.generation, kind, path.to_owned()))
                .is_ok()
        });
        if sent {
            self.pending.insert(key);
        }
        sent
    }

    /// Декодирует файл заново после изменения на диске.
    pub fn reload(&mut self, kind: ImageKind, path: &str) -> bool {
        let key = (kind, path.to_owned());
        if self.pending.contains(&key) {
            self.stale.insert(key);
            true
        } else {
            self.request(kind, path)
        }
    }

    /// Есть незавершённые декодирования: окно должно проснуться, чтобы показать результат.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Готовые изображения текущего поколения; не блокирует.
    pub fn finished(&mut self) -> Vec<(ImageKind, String, Result<DecodedImage, String>)> {
        let results = self
            .worker
            .as_ref()
            .map(|worker| worker.results.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        self.accept(results)
    }

    fn accept(
        &mut self,
        results: Vec<LoadResult>,
    ) -> Vec<(ImageKind, String, Result<DecodedImage, String>)> {
        let mut accepted = Vec::new();
        for (generation, kind, path, result) in results {
            let key = (kind, path);
            if generation != self.generation || !self.pending.remove(&key) {
                continue;
            }
            if self.stale.remove(&key) {
                self.request(key.0, &key.1);
                continue;
            }
            accepted.push((key.0, key.1, result));
        }
        accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::SyncSender;

    impl ImageLoader {
        /// Ждёт хотя бы один результат потока блокирующим приёмом, без `sleep`.
        fn wait_finished(&mut self) -> Vec<(ImageKind, String, Result<DecodedImage, String>)> {
            let worker = self.worker.as_ref().unwrap();
            let first = worker.results.recv().unwrap();
            let mut results = vec![first];
            results.extend(worker.results.try_iter());
            self.accept(results)
        }
    }

    fn texture(width: u32) -> DecodedImage {
        DecodedImage::Texture(TextureData {
            width,
            height: 1,
            rgba: vec![0; width as usize * 4],
        })
    }

    fn width(result: &Result<DecodedImage, String>) -> u32 {
        match result {
            Ok(DecodedImage::Texture(data)) => data.width,
            other => panic!("ожидалась текстура: {other:?}"),
        }
    }

    #[test]
    fn duplicate_requests_decode_once_and_errors_are_reported() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let mut loader = ImageLoader::default();
        assert!(!loader.request(ImageKind::Texture, "a.png"), "нет проекта");
        loader.restart_with(Arc::new(move |kind, path: &str| {
            seen.lock().unwrap().push((kind, path.to_owned()));
            match kind {
                ImageKind::Texture => Ok(texture(2)),
                ImageKind::Hdr => Err("битый HDR".to_owned()),
            }
        }));
        assert!(loader.request(ImageKind::Texture, "a.png"));
        assert!(loader.request(ImageKind::Texture, "a.png"));
        assert!(loader.request(ImageKind::Hdr, "sky.hdr"));
        assert!(loader.has_pending());
        let mut results = Vec::new();
        while results.len() < 2 {
            results.extend(loader.wait_finished());
        }
        results.sort_by_key(|(kind, _, _)| *kind == ImageKind::Hdr);
        assert_eq!(width(&results[0].2), 2);
        assert_eq!(results[1].2.as_ref().unwrap_err(), "битый HDR");
        assert!(!loader.has_pending());
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn reload_during_decoding_discards_the_stale_result_and_decodes_again() {
        // Первый вызов ждёт сигнала теста: перезагрузка гарантированно приходит во время него.
        let (gate_tx, gate_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let gate = Mutex::new(Some(gate_rx));
        let version = Mutex::new(0_u32);
        let mut loader = ImageLoader::default();
        loader.restart_with(Arc::new(move |_, _: &str| {
            if let Some(gate) = gate.lock().unwrap().take() {
                gate.recv().unwrap();
            }
            let mut version = version.lock().unwrap();
            *version += 1;
            Ok(texture(*version))
        }));
        assert!(loader.request(ImageKind::Texture, "a.png"));
        assert!(loader.reload(ImageKind::Texture, "a.png"));
        release(&gate_tx);
        let results = loop {
            let results = loader.wait_finished();
            if !results.is_empty() {
                break results;
            }
        };
        assert_eq!(results.len(), 1);
        assert_eq!(width(&results[0].2), 2, "результат первого чтения отброшен");
        assert!(!loader.has_pending());
    }

    fn release(gate: &SyncSender<()>) {
        gate.send(()).unwrap();
    }

    #[test]
    fn restart_drops_results_of_the_previous_project() {
        let mut loader = ImageLoader::default();
        loader.restart_with(Arc::new(|_, _: &str| Ok(texture(1))));
        assert!(loader.request(ImageKind::Texture, "a.png"));
        let worker = loader.worker.take().unwrap();
        let stale = worker.results.recv().unwrap();
        loader.restart_with(Arc::new(|_, _: &str| Ok(texture(3))));
        assert!(loader.accept(vec![stale]).is_empty());
        assert!(!loader.has_pending());
    }
}
