//! Превью PNG для панели Project: декодирование в фоне и LRU-кэш UI-текстур.

use engine_assets::{TextureData, decode_png};
use engine_render::{RenderError, Renderer, UiTextureId};
use engine_ui::render::UiBridge;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

const MAX_PREVIEWS: usize = 64;
const MAX_PREVIEW_SIDE: u32 = 128;

type DecodeResult = (String, Result<TextureData, String>);

struct Cached {
    texture: UiTextureId,
    id: egui::TextureId,
    last_used: u64,
}

/// Кэш загруженных превью и очередь фонового декодирования.
#[derive(Default)]
pub struct PreviewCache {
    worker: Option<Worker>,
    cached: HashMap<String, Cached>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    clock: u64,
    clear_requested: bool,
}

struct Worker {
    requests: Sender<(String, PathBuf)>,
    results: Receiver<DecodeResult>,
}

impl Worker {
    fn start() -> std::io::Result<Self> {
        let (requests, request_rx) = channel::<(String, PathBuf)>();
        let (result_tx, results) = channel();
        // Поток завершается, когда кэш удаляет отправитель запросов.
        std::thread::Builder::new()
            .name("rve-editor-previews".to_owned())
            .spawn(move || {
                for (path, file) in request_rx {
                    let decoded = std::fs::read(&file)
                        .map_err(|error| error.to_string())
                        .and_then(|bytes| decode_png(&bytes))
                        .map(|data| preview_image(&data, MAX_PREVIEW_SIDE));
                    if result_tx.send((path, decoded)).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, results })
    }
}

impl PreviewCache {
    /// Возвращает готовое превью или ставит файл в очередь декодирования.
    pub fn get(
        &mut self,
        path: &str,
        file: impl FnOnce() -> Option<PathBuf>,
    ) -> Option<egui::TextureId> {
        self.clock += 1;
        if let Some(cached) = self.cached.get_mut(path) {
            cached.last_used = self.clock;
            return Some(cached.id);
        }
        if self.pending.contains(path) || self.failed.contains(path) {
            return None;
        }
        if self.worker.is_none() {
            self.worker = Worker::start().ok();
        }
        let requested = match (&self.worker, file()) {
            (Some(worker), Some(file)) => worker.requests.send((path.to_owned(), file)).is_ok(),
            _ => false,
        };
        if requested {
            self.pending.insert(path.to_owned());
        } else {
            self.failed.insert(path.to_owned());
        }
        None
    }

    /// Запрашивает сброс кэша: файлы изменились на диске.
    pub fn invalidate(&mut self) {
        self.clear_requested = true;
    }

    /// Есть незавершённые декодирования: окно должно проснуться, чтобы показать результат.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Загружает готовые изображения в GPU и освобождает лишние; вызывается до построения UI.
    pub fn upload(
        &mut self,
        renderer: &mut Renderer,
        bridge: &mut UiBridge,
    ) -> Result<(), RenderError> {
        if std::mem::take(&mut self.clear_requested) {
            for (_, cached) in self.cached.drain() {
                bridge.unregister(cached.id);
                renderer.free_ui_texture(cached.texture)?;
            }
            self.failed.clear();
        }
        let finished = self
            .worker
            .as_ref()
            .map(|worker| worker.results.try_iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for (path, decoded) in finished {
            self.pending.remove(&path);
            match decoded {
                Ok(data) => {
                    let texture =
                        renderer.create_ui_texture(data.width, data.height, &data.rgba)?;
                    let id = bridge.register_texture(texture);
                    self.clock += 1;
                    self.cached.insert(
                        path,
                        Cached {
                            texture,
                            id,
                            last_used: self.clock,
                        },
                    );
                }
                Err(_) => {
                    self.failed.insert(path);
                }
            }
        }
        for path in eviction_order(
            self.cached
                .iter()
                .map(|(path, entry)| (path, entry.last_used)),
            MAX_PREVIEWS,
        ) {
            if let Some(cached) = self.cached.remove(&path) {
                bridge.unregister(cached.id);
                renderer.free_ui_texture(cached.texture)?;
            }
        }
        Ok(())
    }
}

/// Давно не использованные записи сверх лимита.
fn eviction_order<'a>(
    entries: impl Iterator<Item = (&'a String, u64)>,
    limit: usize,
) -> Vec<String> {
    let mut entries = entries.collect::<Vec<_>>();
    if entries.len() <= limit {
        return Vec::new();
    }
    entries.sort_by_key(|(_, last_used)| *last_used);
    let excess = entries.len() - limit;
    entries
        .into_iter()
        .take(excess)
        .map(|(path, _)| path.clone())
        .collect()
}

/// Уменьшает изображение до `max_side` по большей стороне и предумножает альфу для UI pass.
fn preview_image(source: &TextureData, max_side: u32) -> TextureData {
    let largest = source.width.max(source.height).max(1);
    let scale = (max_side as f32 / largest as f32).min(1.0);
    let width = ((source.width as f32 * scale).round() as u32).max(1);
    let height = ((source.height as f32 * scale).round() as u32).max(1);
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let source_y = (u64::from(y) * u64::from(source.height) / u64::from(height)) as usize;
        for x in 0..width {
            let source_x = (u64::from(x) * u64::from(source.width) / u64::from(width)) as usize;
            let offset = (source_y * source.width as usize + source_x) * 4;
            let [r, g, b, a] = [
                source.rgba[offset],
                source.rgba[offset + 1],
                source.rgba[offset + 2],
                source.rgba[offset + 3],
            ];
            let premultiply = |channel: u8| (u16::from(channel) * u16::from(a) / 255) as u8;
            rgba.extend_from_slice(&[premultiply(r), premultiply(g), premultiply(b), a]);
        }
    }
    TextureData {
        width,
        height,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> TextureData {
        TextureData {
            width,
            height,
            rgba: color.repeat(width as usize * height as usize),
        }
    }

    #[test]
    fn large_images_shrink_keeping_the_aspect_ratio() {
        let preview = preview_image(&solid(400, 100, [1, 2, 3, 255]), 128);
        assert_eq!((preview.width, preview.height), (128, 32));
        assert_eq!(preview.rgba.len(), 128 * 32 * 4);
    }

    #[test]
    fn small_images_are_not_enlarged_and_extreme_ratios_keep_one_pixel() {
        let small = preview_image(&solid(4, 2, [0; 4]), 128);
        assert_eq!((small.width, small.height), (4, 2));
        let thin = preview_image(&solid(1000, 1, [0; 4]), 128);
        assert_eq!((thin.width, thin.height), (128, 1));
    }

    #[test]
    fn alpha_is_premultiplied() {
        let preview = preview_image(&solid(1, 1, [200, 100, 50, 128]), 128);
        assert_eq!(preview.rgba, [100, 50, 25, 128]);
    }

    #[test]
    fn eviction_removes_the_least_recently_used_over_the_limit() {
        let names = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let entries = [(&names[0], 5), (&names[1], 1), (&names[2], 9)];
        assert_eq!(eviction_order(entries.into_iter(), 2), ["b"]);
        assert!(eviction_order(entries.into_iter(), 3).is_empty());
    }

    #[test]
    fn unavailable_files_are_not_requested_again() {
        let mut cache = PreviewCache::default();
        assert_eq!(cache.get("a.png", || None), None);
        assert!(!cache.has_pending());
        let mut asked = false;
        assert_eq!(
            cache.get("a.png", || {
                asked = true;
                None
            }),
            None
        );
        assert!(!asked);
    }
}
